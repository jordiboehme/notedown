//! The prune of the recycle bin: `Store::prune_recycle` on a store in a temp folder with a fixed
//! clock and offset `+02:00`. Slots and index lines are written directly where a real commit
//! can't place them (old times, ids the store no longer knows).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::{Event, EventBody, FileKind, Journal, Origin};
use notedown_cloud::pages::{self, PageRecord};
use notedown_cloud::push::{PushPlanner, PushTiming};
use notedown_cloud::server::AppState;
use notedown_cloud::store::retention::Retention;
use notedown_cloud::store::{
    CrashPoint, IndexLine, PENDING_FILE, PutOptions, Recovery, RecycleEntry, RecycleItem,
    RecycleReason, ScanOptions, Store, WirePath,
};
use notedown_cloud::{Clock, Config};
use time::{OffsetDateTime, UtcOffset};

const EMAIL: &str = "jordi@example.org";
const DAY: u64 = 86_400_000;
const HOUR: u64 = 3_600_000;
/// 2026-10-06 08:20 UTC.
const NOW: u64 = 1_791_270_000_000;
/// An id the store never gave out: a deleted file.
const DEAD: u64 = 900_000;

const RETENTION: Retention = Retention {
    keep_latest: 3,
    keep_daily: 7,
    keep_monthly: 12,
    keep_deleted_days: 90,
};

fn offset() -> UtcOffset {
    UtcOffset::from_hms(2, 0, 0).unwrap()
}

fn wp(s: &str) -> WirePath {
    WirePath::parse_named(s).unwrap()
}

fn entry(id: u64, path: &str, deleted_at: u64, reason: RecycleReason) -> RecycleEntry {
    RecycleEntry {
        id,
        path: path.into(),
        md5: format!("{:032x}", deleted_at),
        deleted_at,
        reason,
        pruned: false,
    }
}

fn folder_entry(id: u64, path: &str, deleted_at: u64) -> RecycleEntry {
    RecycleEntry {
        md5: String::new(),
        ..entry(id, path, deleted_at, RecycleReason::Deleted)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    journal: Journal,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let store = Store::open(dir.path(), "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let journal = Journal::open(dir.path()).unwrap();
        Fixture {
            _dir: dir,
            store,
            journal,
        }
    }

    fn recycle(&self) -> PathBuf {
        self.store.recycle_dir()
    }

    fn index_path(&self) -> PathBuf {
        self.recycle().join("index.jsonl")
    }

    /// A live file in `files/`; returns its id.
    fn live(&self, path: &str, bytes: &[u8]) -> u64 {
        let p = self.store.fs_path(&wp(path));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        let g = self.store.lock().unwrap();
        self.store.entry(&g, &wp(path)).unwrap().unwrap().id
    }

    /// The bytes of `e` in its slot (for a folder line: the folder).
    fn slot(&self, e: &RecycleEntry, bytes: &[u8]) {
        let p = self.slot_path(e);
        if e.md5.is_empty() {
            std::fs::create_dir_all(&p).unwrap();
        } else {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
    }

    fn slot_path(&self, e: &RecycleEntry) -> PathBuf {
        self.recycle().join(e.location())
    }

    /// Append raw text (one line, the newline is added) to the index.
    fn raw_line(&self, text: &str) {
        use std::io::Write;
        std::fs::create_dir_all(self.recycle()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.index_path())
            .unwrap();
        writeln!(f, "{text}").unwrap();
    }

    fn line(&self, e: &RecycleEntry) {
        self.raw_line(&serde_json::to_string(e).unwrap());
    }

    /// The line and the slot of `e`, with `bytes` in it.
    fn add(&self, e: &RecycleEntry, bytes: &[u8]) {
        self.slot(e, bytes);
        self.line(e);
    }

    fn index_text(&self) -> String {
        std::fs::read_to_string(self.index_path()).unwrap()
    }

    fn prune(&self, now: u64, origin: Origin, dry_run: bool) -> notedown_cloud::store::PruneReport {
        self.store
            .prune_recycle(&self.journal, &RETENTION, now, offset(), origin, dry_run)
            .unwrap()
    }

    fn events(&self) -> Vec<Event> {
        self.journal
            .read_from(1)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    /// Every path below `recycle/` with its bytes (`None` for a folder).
    fn tree(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        walk(&self.recycle(), &self.recycle(), &mut out);
        out
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let p = e.unwrap().path();
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        if p.is_dir() {
            out.insert(rel, None);
            walk(root, &p, out);
        } else {
            out.insert(rel, Some(std::fs::read(&p).unwrap()));
        }
    }
}

/// A live file with five old versions and two deleted files: the two oldest versions (more
/// than 12 months back) and the deleted file older than 90 days go.
struct Scene {
    old1: RecycleEntry,
    old2: RecycleEntry,
    gone: RecycleEntry,
    recent: Vec<RecycleEntry>,
    young_deleted: RecycleEntry,
}

fn scene(f: &Fixture) -> Scene {
    let id = f.live("NOTE/Note/a.note", b"now");
    let ow = RecycleReason::Overwritten;
    let s = Scene {
        old1: entry(id, "NOTE/Note/a.note", NOW - 400 * DAY, ow),
        old2: entry(id, "NOTE/Note/a.note", NOW - 399 * DAY, ow),
        gone: entry(
            DEAD,
            "NOTE/Note/b.note",
            NOW - 100 * DAY,
            RecycleReason::Deleted,
        ),
        recent: (1..=3)
            .rev()
            .map(|h| entry(id, "NOTE/Note/a.note", NOW - h * HOUR, ow))
            .collect(),
        young_deleted: entry(
            DEAD + 1,
            "NOTE/Note/c.note",
            NOW - 10 * DAY,
            RecycleReason::Deleted,
        ),
    };
    f.add(&s.old1, b"one");
    f.add(&s.old2, b"twoo");
    f.add(&s.gone, b"three");
    for e in &s.recent {
        f.add(e, b"recent");
    }
    f.add(&s.young_deleted, b"young");
    s
}

#[test]
fn a_prune_removes_the_planned_slots_and_lines_and_journals_one_batch() {
    let f = Fixture::new();
    let s = scene(&f);
    let r = f.prune(NOW, Origin::Retention, false);
    let removed = vec![s.old1.clone(), s.old2.clone(), s.gone.clone()];
    assert_eq!(r.removed, removed);
    assert_eq!(r.removed_bytes, 3 + 4 + 5);
    assert_eq!((r.kept, r.unreadable_kept), (4, 0));
    for e in &removed {
        assert!(!f.slot_path(e).exists(), "{}", e.location());
        // the slot's empty `recycle/<ms>/` tree goes too, `recycle/` stays
        assert!(!f.recycle().join(e.deleted_at.to_string()).exists());
    }
    assert!(f.recycle().is_dir());
    let mut kept = s.recent.clone();
    kept.push(s.young_deleted.clone());
    for e in &kept {
        assert!(f.slot_path(e).is_file(), "{}", e.location());
    }
    let g = f.store.lock().unwrap();
    assert_eq!(f.store.recycle_index(&g).unwrap(), kept);
    drop(g);

    let events = f.events();
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|e| e.op_id == events[0].op_id));
    assert!(events.iter().all(|e| e.origin == Origin::Retention));
    assert!(
        events
            .iter()
            .all(|e| e.device.is_none() && e.account == "jordi")
    );
    let pruned: Vec<RecycleEntry> = events
        .iter()
        .map(|e| match &e.body {
            EventBody::RecyclePruned {
                id,
                path,
                md5,
                deleted_at,
                reason,
            } => RecycleEntry {
                id: *id,
                path: path.clone(),
                md5: md5.clone(),
                deleted_at: *deleted_at,
                reason: *reason,
                pruned: false,
            },
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(pruned, removed);
    let line = events[2].to_line();
    assert!(line.contains(r#""origin":"retention""#), "{line}");
    assert!(
        line.contains(&format!(
            r#""type":"recycle.pruned","id":{DEAD},"path":"NOTE/Note/b.note","md5":"{}","deletedAt":{},"reason":"deleted""#,
            s.gone.md5, s.gone.deleted_at
        )),
        "{line}"
    );
}

#[test]
fn a_dry_run_changes_nothing_and_reports_what_the_real_run_does() {
    let f = Fixture::new();
    scene(&f);
    let tree = f.tree();
    let dry = f.prune(NOW, Origin::Cli, true);
    assert_eq!(f.tree(), tree);
    assert!(f.events().is_empty());
    assert_eq!(dry.removed.len(), 3);
    let real = f.prune(NOW, Origin::Cli, false);
    assert_eq!(dry, real);
    assert!(f.events().iter().all(|e| e.origin == Origin::Cli));
}

#[test]
fn a_second_prune_removes_and_journals_nothing() {
    let f = Fixture::new();
    scene(&f);
    f.prune(NOW, Origin::Cli, false);
    let (tree, seq) = (f.tree(), f.journal.last_seq().unwrap());
    let again = f.prune(NOW, Origin::Cli, false);
    assert!(again.removed.is_empty());
    assert_eq!(again.kept, 4);
    assert_eq!(f.tree(), tree);
    assert_eq!(f.journal.last_seq().unwrap(), seq);
}

#[test]
fn a_corrupt_line_stays_unchanged_and_is_counted() {
    let f = Fixture::new();
    let gone = entry(
        DEAD,
        "NOTE/Note/b.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    let young = entry(
        DEAD + 1,
        "NOTE/Note/c.note",
        NOW - DAY,
        RecycleReason::Deleted,
    );
    f.add(&gone, b"b");
    f.raw_line("{not json");
    f.add(&young, b"c");
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![gone]);
    assert_eq!((r.kept, r.unreadable_kept), (1, 1));
    assert_eq!(
        f.index_text(),
        format!("{{not json\n{}\n", serde_json::to_string(&young).unwrap())
    );
    let g = f.store.lock().unwrap();
    assert_eq!(
        f.store.recycle_lines(&g).unwrap(),
        vec![IndexLine::Raw("{not json".into()), IndexLine::Entry(young)]
    );
}

#[test]
fn the_lines_read_leave_out_a_torn_last_line() {
    let f = Fixture::new();
    let e = entry(DEAD, "NOTE/Note/b.note", NOW, RecycleReason::Deleted);
    f.line(&e);
    f.raw_line("[1]");
    std::fs::write(f.index_path(), format!("{}{{\"id\":1,\"pa", f.index_text())).unwrap();
    let g = f.store.lock().unwrap();
    assert_eq!(
        f.store.recycle_lines(&g).unwrap(),
        vec![IndexLine::Entry(e), IndexLine::Raw("[1]".into())]
    );
}

#[test]
fn a_missing_slot_is_no_error() {
    let f = Fixture::new();
    let gone = entry(
        DEAD,
        "NOTE/Note/b.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    f.line(&gone);
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![gone]);
    assert_eq!(r.removed_bytes, 0);
    assert_eq!(f.index_text(), "");
    assert_eq!(f.events().len(), 1);
}

#[test]
fn a_folder_line_never_removes_a_file_that_is_still_listed() {
    let f = Fixture::new();
    let ms = NOW - 91 * DAY;
    // x.note's id is live (kept by keep_latest), y.note's and the folder's are not.
    let x_id = f.live("NOTE/Note/x.note", b"x now");
    let x = entry(x_id, "NOTE/Note/A/x.note", ms, RecycleReason::Deleted);
    let y = entry(DEAD, "NOTE/Note/A/y.note", ms, RecycleReason::Deleted);
    let folder = folder_entry(DEAD + 1, "NOTE/Note/A", ms);
    f.add(&x, b"x");
    f.add(&y, b"y");
    f.add(&folder, b"");
    std::fs::write(f.slot_path(&folder).join(".DS_Store"), b"dots").unwrap();
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![y.clone(), folder.clone()]);
    assert!(!f.slot_path(&y).exists());
    assert!(f.slot_path(&folder).is_dir());
    assert!(f.slot_path(&x).is_file());
    assert!(f.slot_path(&folder).join(".DS_Store").is_file());
    let g = f.store.lock().unwrap();
    assert_eq!(f.store.recycle_index(&g).unwrap(), vec![x]);
}

#[test]
fn when_every_line_of_a_folder_delete_expires_its_whole_slot_goes() {
    let f = Fixture::new();
    let ms = NOW - 91 * DAY;
    // The folder line first: the order of the lines must not matter.
    let folder = folder_entry(DEAD, "NOTE/Note/A", ms);
    let x = entry(DEAD + 1, "NOTE/Note/A/x.note", ms, RecycleReason::Deleted);
    let y = entry(DEAD + 2, "NOTE/Note/A/B/y.note", ms, RecycleReason::Deleted);
    let keep = entry(
        DEAD + 3,
        "NOTE/Note/k.note",
        NOW - DAY,
        RecycleReason::Deleted,
    );
    f.add(&folder, b"");
    f.add(&x, b"x");
    f.add(&y, b"y");
    f.add(&keep, b"k");
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![folder, x, y]);
    assert!(!f.recycle().join(ms.to_string()).exists());
    assert!(f.slot_path(&keep).is_file());
}

#[cfg(unix)]
#[test]
fn a_folder_slot_with_a_symlink_inside_keeps_its_line_and_is_reported() {
    let f = Fixture::new();
    let ms = NOW - 91 * DAY;
    let folder = folder_entry(DEAD, "NOTE/Note/A", ms);
    f.add(&folder, b"");
    std::fs::write(f.slot_path(&folder).join(".DS_Store"), b"dots").unwrap();
    let outside = f.store.account_dir().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, f.slot_path(&folder).join("link")).unwrap();
    for dry_run in [true, false] {
        let r = f.prune(NOW, Origin::Cli, dry_run);
        assert!(r.removed.is_empty());
        assert_eq!(r.kept, 1);
        assert_eq!(r.failed.len(), 1, "{:?}", r.failed);
        assert_eq!(r.failed[0].location, folder.location());
    }
    assert!(outside.join("keep.txt").is_file());
    assert!(f.slot_path(&folder).join("link").is_symlink());
    assert!(f.slot_path(&folder).join(".DS_Store").is_file());
    assert_eq!(
        f.index_text(),
        format!("{}\n", serde_json::to_string(&folder).unwrap())
    );
    assert!(f.events().is_empty());
}

#[test]
fn an_expired_folder_slot_goes_with_its_dot_files_and_unregistered_files() {
    let f = Fixture::new();
    let ms = NOW - 91 * DAY;
    let folder = folder_entry(DEAD, "NOTE/Note/A", ms);
    f.add(&folder, b"");
    let slot = f.slot_path(&folder);
    std::fs::create_dir_all(slot.join(".hidden/B")).unwrap();
    std::fs::write(slot.join(".DS_Store"), b"dots").unwrap();
    std::fs::write(slot.join(".hidden/B/x"), b"x").unwrap();
    std::fs::write(slot.join("stray.pdf"), b"stray").unwrap();
    let r = f.prune(NOW, Origin::Cli, true);
    assert_eq!(r.removed, vec![folder.clone()]);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert!(slot.join(".DS_Store").is_file());
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![folder.clone()]);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert!(!f.recycle().join(ms.to_string()).exists());
}

#[test]
fn a_folder_slot_that_a_corrupt_line_may_name_is_not_removed_whole() {
    let f = Fixture::new();
    let ms = NOW - 91 * DAY;
    let folder = folder_entry(DEAD, "NOTE/Note/A", ms);
    f.add(&folder, b"");
    let inside = f.slot_path(&folder).join("x.note");
    std::fs::write(&inside, b"x").unwrap();
    let raw = format!("{{\"deletedAt\":{ms},\"path\":\"NOTE/Note/A/x.note\"");
    f.raw_line(&raw);
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![folder]);
    assert_eq!(r.unreadable_kept, 1);
    assert!(inside.is_file());
    assert_eq!(f.index_text(), format!("{raw}\n"));
}

/// A real folder delete through the store, with a dot-file inside: before the 90 days its
/// slot stays whole, after them it goes whole.
#[test]
fn a_real_folder_delete_with_a_dot_file_goes_whole_when_it_expires() {
    let f = Fixture::new();
    f.live("NOTE/Note/A/x.note", b"x");
    std::fs::write(f.store.fs_path(&wp("NOTE/Note/A/.DS_Store")), b"dots").unwrap();
    let ms = NOW - 50 * DAY;
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("NOTE/Note/A"), ms, Origin::Device, None)
        .unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    drop(g);
    let slot = f.recycle().join(format!("{ms}/NOTE/Note/A"));
    assert!(slot.join(".DS_Store").is_file());
    assert!(slot.join("x.note").is_file());
    let lines = {
        let g = f.store.lock().unwrap();
        f.store.recycle_index(&g).unwrap()
    };
    assert_eq!(lines.len(), 2);
    let events = f.events().len();

    let r = f.prune(NOW, Origin::Cli, false);
    assert!(r.removed.is_empty());
    assert!(slot.join(".DS_Store").is_file());
    assert_eq!(f.events().len(), events);

    let later = ms + 91 * DAY;
    let r = f.prune(later, Origin::Cli, true);
    assert_eq!(r.removed, lines);
    let r = f.prune(later, Origin::Cli, false);
    assert_eq!(r.removed, lines);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert!(!f.recycle().join(ms.to_string()).exists());
    assert_eq!(f.index_text(), "");
}

/// The last file of a folder that also holds a dot-file: the emptied folder goes to its own
/// slot with the dot-file. When its line expires, that slot goes whole.
#[test]
fn a_pruned_folder_slot_goes_whole_when_its_line_expires() {
    let f = Fixture::new();
    f.live("NOTE/Note/P/a.note", b"a");
    std::fs::write(f.store.fs_path(&wp("NOTE/Note/P/.DS_Store")), b"dots").unwrap();
    let ms = NOW - 50 * DAY;
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("NOTE/Note/P/a.note"), ms, Origin::Device, None)
        .unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    drop(g);
    let lines = {
        let g = f.store.lock().unwrap();
        f.store.recycle_index(&g).unwrap()
    };
    assert_eq!(lines.len(), 2, "{lines:?}");
    let pruned = &lines[1];
    assert_eq!(pruned.md5, "");
    assert_eq!(pruned.path, "NOTE/Note/P");
    let slot = f.slot_path(pruned);
    assert!(slot.join(".DS_Store").is_file());

    assert!(f.prune(NOW, Origin::Cli, false).removed.is_empty());
    assert!(slot.join(".DS_Store").is_file());

    let r = f.prune(ms + 91 * DAY, Origin::Cli, false);
    assert_eq!(r.removed, lines);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert!(!f.recycle().join(ms.to_string()).exists());
    assert!(!f.recycle().join(pruned.deleted_at.to_string()).exists());
    assert_eq!(f.index_text(), "");
}

#[cfg(unix)]
#[test]
fn a_symlinked_recycle_bin_gives_one_message_and_prunes_nothing() {
    let f = Fixture::new();
    let s = scene(&f);
    let moved = f.store.account_dir().join("elsewhere");
    std::fs::rename(f.recycle(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, f.recycle()).unwrap();
    for dry_run in [true, false] {
        let r = f.prune(NOW, Origin::Cli, dry_run);
        assert!(r.removed.is_empty());
        assert_eq!(r.failed.len(), 1, "{:?}", r.failed);
        assert_eq!(r.failed[0].location, "");
    }
    assert!(moved.join(s.old1.location()).is_file());
    assert!(f.events().is_empty());
}

#[test]
fn a_prune_recovers_first_and_keeps_the_line_of_the_recovered_commit() {
    let f = Fixture::new();
    let gone = entry(
        DEAD,
        "NOTE/Note/b.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    f.add(&gone, b"b");
    let put = |bytes: &[u8], options: PutOptions, crash: Option<CrashPoint>| {
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let dir = f
            .store
            .account_dir()
            .join("staging/44444444-4444-4444-8444-444444444444");
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("data");
        std::fs::write(&staged, bytes).unwrap();
        let c = f
            .store
            .plan_put_with(
                &g,
                &wp("NOTE/Note/r.note"),
                &staged,
                Origin::Device,
                None,
                options,
            )
            .unwrap();
        f.store.commit_until(&g, c, &f.journal, crash).unwrap();
    };
    put(b"old", PutOptions::default(), None);
    let at = NOW - HOUR;
    put(
        b"new",
        PutOptions {
            recycle_at_ms: Some(at),
            ..PutOptions::default()
        },
        Some(CrashPoint::BeforeAppend),
    );
    assert!(f.store.account_dir().join(PENDING_FILE).exists());
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![gone]);
    assert!(!f.store.account_dir().join(PENDING_FILE).exists());
    let g = f.store.lock().unwrap();
    let index = f.store.recycle_index(&g).unwrap();
    assert_eq!(index.len(), 1, "{index:?}");
    assert_eq!(index[0].path, "NOTE/Note/r.note");
    assert_eq!(index[0].deleted_at, at);
    assert_eq!(index[0].reason, RecycleReason::Overwritten);
    assert!(f.slot_path(&index[0]).is_file());
}

#[test]
fn the_new_index_is_written_atomically_and_appends_continue() {
    let f = Fixture::new();
    let s = scene(&f);
    f.prune(NOW, Origin::Cli, false);
    let leftovers: Vec<String> = std::fs::read_dir(f.recycle())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let text = f.index_text();
    assert!(text.ends_with('\n'), "{text:?}");
    assert_eq!(text.lines().count(), 4);

    let next = entry(DEAD + 5, "NOTE/Note/d.note", NOW, RecycleReason::Deleted);
    let g = f.store.lock().unwrap();
    assert!(f.store.record_recycled(&g, &next).unwrap());
    // the tail check finds it on the rewritten index: no second line
    assert!(!f.store.record_recycled(&g, &next).unwrap());
    let index = f.store.recycle_index(&g).unwrap();
    let mut want = s.recent.clone();
    want.push(s.young_deleted.clone());
    want.push(next);
    assert_eq!(index, want);
}

#[test]
fn a_line_whose_slot_leaves_the_recycle_bin_is_kept_and_its_target_stays() {
    let f = Fixture::new();
    f.live("NOTE/Note/v.note", b"victim");
    let escape = entry(
        DEAD,
        "NOTE/Note/../../../../files/NOTE/Note/v.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    let gone = entry(
        DEAD + 1,
        "NOTE/Note/b.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    f.line(&escape);
    f.add(&gone, b"b");
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![gone]);
    assert_eq!((r.kept, r.unreadable_kept), (0, 1));
    assert!(f.store.fs_path(&wp("NOTE/Note/v.note")).is_file());
    assert_eq!(
        f.index_text(),
        format!("{}\n", serde_json::to_string(&escape).unwrap())
    );
    assert_eq!(f.events().len(), 1);
}

#[test]
fn a_corrupt_line_that_is_no_utf8_is_written_back_byte_for_byte() {
    let f = Fixture::new();
    let gone = entry(
        DEAD,
        "NOTE/Note/b.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    let young = entry(
        DEAD + 1,
        "NOTE/Note/c.note",
        NOW - DAY,
        RecycleReason::Deleted,
    );
    f.add(&gone, b"b");
    let mut bytes = std::fs::read(f.index_path()).unwrap();
    bytes.extend_from_slice(b"\xff\xfe junk\n");
    std::fs::write(f.index_path(), &bytes).unwrap();
    f.add(&young, b"c");
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![gone]);
    assert_eq!(r.unreadable_kept, 1);
    let mut want = b"\xff\xfe junk\n".to_vec();
    want.extend_from_slice(serde_json::to_string(&young).unwrap().as_bytes());
    want.push(b'\n');
    assert_eq!(std::fs::read(f.index_path()).unwrap(), want);
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_slot_that_cant_be_removed_doesnt_stop_the_rest() {
    let f = Fixture::new();
    let stuck = entry(
        DEAD,
        "NOTE/Note/a.note",
        NOW - 100 * DAY,
        RecycleReason::Deleted,
    );
    let folder = folder_entry(DEAD + 1, "NOTE/Note/F", NOW - 99 * DAY);
    let other = entry(
        DEAD + 2,
        "NOTE/Note/b.note",
        NOW - 98 * DAY,
        RecycleReason::Deleted,
    );
    f.add(&stuck, b"a");
    f.add(&folder, b"");
    f.add(&other, b"b");
    let parent = f.slot_path(&stuck).parent().unwrap().to_path_buf();
    set_mode(&parent, 0o555);
    let r = f
        .store
        .prune_recycle(&f.journal, &RETENTION, NOW, offset(), Origin::Cli, false);
    set_mode(&parent, 0o755);
    let r = r.unwrap();
    // The stuck slot is a failure only: not removed, not counted in the bytes.
    assert_eq!(r.removed, vec![folder.clone(), other.clone()]);
    assert_eq!(r.removed_bytes, 1);
    assert_eq!(r.kept, 0);
    assert_eq!(r.failed.len(), 1, "{:?}", r.failed);
    assert_eq!(r.failed[0].location, stuck.location());
    assert!(f.slot_path(&stuck).is_file());
    assert!(!f.recycle().join(folder.deleted_at.to_string()).exists());
    assert!(!f.recycle().join(other.deleted_at.to_string()).exists());
    assert_eq!(f.index_text(), "");
    assert_eq!(f.events().len(), 3);
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_slot_path_keeps_the_line_and_is_reported() {
    let f = Fixture::new();
    let ms = NOW - 100 * DAY;
    let linked = entry(DEAD, "NOTE/Note/s.note", ms, RecycleReason::Deleted);
    let outside = f.store.account_dir().join("outside");
    std::fs::create_dir_all(outside.join("Note")).unwrap();
    std::fs::write(outside.join("Note/s.note"), b"keep").unwrap();
    std::fs::create_dir_all(f.recycle().join(ms.to_string())).unwrap();
    std::os::unix::fs::symlink(&outside, f.recycle().join(format!("{ms}/NOTE"))).unwrap();
    f.line(&linked);
    let gone = entry(DEAD + 1, "NOTE/Note/b.note", ms, RecycleReason::Deleted);
    let gone = RecycleEntry {
        deleted_at: ms + 1,
        ..gone
    };
    f.add(&gone, b"b");
    for dry_run in [true, false] {
        let r = f.prune(NOW, Origin::Cli, dry_run);
        assert_eq!(r.removed, vec![gone.clone()]);
        assert_eq!(r.kept, 1);
        assert_eq!(r.failed.len(), 1, "{:?}", r.failed);
        assert_eq!(r.failed[0].location, linked.location());
    }
    assert!(outside.join("Note/s.note").is_file());
    assert_eq!(
        f.index_text(),
        format!("{}\n", serde_json::to_string(&linked).unwrap())
    );
    assert_eq!(f.events().len(), 1);
}

// ------------------------------------------------------------------ the server's hourly prune

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(self.0) * 1_000_000).unwrap()
    }
}

/// A data folder with accounts `slugs`, each with one live file and five versions of it: the
/// two old ones (more than 12 months back) go, the three recent ones stay. Returns the temp
/// folder and the state, with the clock at `NOW`.
fn server_with(slugs: &[&str], config: Config) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    for slug in slugs {
        Accounts::new(dir.path())
            .create(slug, &format!("{slug}@example.org"), "secret")
            .unwrap();
        let store = Store::open(dir.path(), slug).unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let p = store.fs_path(&wp("NOTE/Note/a.note"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"now").unwrap();
        let id = {
            let g = store.lock().unwrap();
            store
                .entry(&g, &wp("NOTE/Note/a.note"))
                .unwrap()
                .unwrap()
                .id
        };
        let ow = RecycleReason::Overwritten;
        for ms in [
            NOW - 400 * DAY,
            NOW - 399 * DAY,
            NOW - 3 * HOUR,
            NOW - 2 * HOUR,
            NOW - HOUR,
        ] {
            let e = entry(id, "NOTE/Note/a.note", ms, ow);
            let slot = store.recycle_dir().join(e.location());
            std::fs::create_dir_all(slot.parent().unwrap()).unwrap();
            std::fs::write(&slot, b"old").unwrap();
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(store.recycle_dir().join("index.jsonl"))
                .unwrap();
            writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
        }
    }
    let state = AppState::open_with_clock(config, dir.path(), Arc::new(FixedClock(NOW))).unwrap();
    (dir, state)
}

fn journal_events(state: &AppState) -> Vec<Event> {
    state
        .journal
        .read_from(1)
        .unwrap()
        .map(|e| e.unwrap())
        .collect()
}

#[test]
fn the_server_prune_covers_every_account_and_journals_origin_retention() {
    let (_dir, state) = server_with(&["anna", "ben"], Config::default());
    let r = state.prune_recycle_all().unwrap();
    assert_eq!(r.removed, 4);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    let events = journal_events(&state);
    assert_eq!(events.len(), 4);
    for e in &events {
        assert_eq!(e.origin, Origin::Retention);
        assert!(matches!(e.body, EventBody::RecyclePruned { .. }));
    }
    for slug in ["anna", "ben"] {
        assert_eq!(events.iter().filter(|e| e.account == slug).count(), 2);
    }
    assert_eq!(state.prune_recycle_all().unwrap().removed, 0);
}

#[test]
fn the_hourly_prune_does_nothing_when_recycle_prune_is_off() {
    let off = Config {
        recycle_prune: false,
        ..Config::default()
    };
    let (_dir, state) = server_with(&["anna"], off);
    assert!(state.hourly_recycle_prune().is_none());
    assert!(journal_events(&state).is_empty());

    let (_dir, state) = server_with(&["anna"], Config::default());
    assert_eq!(state.hourly_recycle_prune().unwrap().unwrap().removed, 2);
}

#[cfg(unix)]
#[test]
fn an_account_whose_prune_fails_doesnt_stop_the_others() {
    let (dir, state) = server_with(&["anna", "ben"], Config::default());
    // The index of anna can't be read.
    let store = Store::open(&state.data_dir, "anna").unwrap();
    let index = store.recycle_dir().join("index.jsonl");
    let tree = |root: &Path| {
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    };
    let before = tree(&store.recycle_dir());
    set_mode(&index, 0o000);
    let r = state.prune_recycle_all();
    set_mode(&index, 0o644);
    let r = r.unwrap();
    assert_eq!(r.removed, 2);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(
        r.problems[0].starts_with("account anna: recycle bin prune failed: "),
        "{}",
        r.problems[0]
    );
    let events = journal_events(&state);
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.account == "ben"));
    // anna's lines and slots are untouched.
    assert_eq!(tree(&store.recycle_dir()), before);
    assert_eq!(std::fs::read_to_string(&index).unwrap().lines().count(), 5);
    drop(dir);
}

#[test]
fn a_recycle_pruned_event_plans_no_push() {
    let event = Event {
        seq: 1,
        ts: NOW,
        account: "jordi".into(),
        origin: Origin::Retention,
        device: None,
        body: EventBody::RecyclePruned {
            id: 7,
            path: "NOTE/Note/a.note".into(),
            md5: "0".repeat(32),
            deleted_at: NOW - 400 * DAY,
            reason: RecycleReason::Overwritten,
        },
        op_id: "op".into(),
    };
    let mut planner = PushPlanner::new(PushTiming::default());
    planner.observe(&event, &["SN100".to_string()], NOW);
    assert!(planner.pending("jordi", "SN100").is_none());
    assert!(planner.due(NOW + 24 * HOUR).is_empty());
}

// ------------------------------------------------------------------ restore

/// A minimal note (docs/note-format.md 1 to 4) with `FILE_ID` `file_id` and the pages `ids`.
fn note(file_id: &str, ids: &[&str]) -> Vec<u8> {
    fn block(data: &mut Vec<u8>, payload: &[u8]) -> u32 {
        let at = data.len() as u32;
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(payload);
        at
    }
    let mut d = b"noteSN_FILE_VER_20230015".to_vec();
    let h = block(
        &mut d,
        format!("<FILE_ID:{file_id}><APPLY_EQUIPMENT:N6>").as_bytes(),
    );
    let mut footer = format!("<FILE_FEATURE:{h}>");
    for (n, id) in ids.iter().enumerate() {
        let at = block(&mut d, format!("<PAGEID:{id}><PAGESTYLE:s>").as_bytes());
        footer.push_str(&format!("<PAGE{}:{at}>", n + 1));
    }
    let f = block(&mut d, footer.as_bytes());
    d.extend_from_slice(b"tail");
    d.extend_from_slice(&f.to_le_bytes());
    d
}

const FID: &str = "F20261116120000000000abcdefABCDEF";

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    const_hex::encode(Md5::digest(bytes))
}

/// `(type, path)` of the file and folder events, page events as `(type, fileId)`.
fn shape(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .map(|e| {
            let what = match &e.body {
                EventBody::FileCreated { path, .. }
                | EventBody::FileDeleted { path, .. }
                | EventBody::FolderCreated { path, .. }
                | EventBody::FolderDeleted { path, .. } => path.clone(),
                EventBody::PageAdded { file_id, .. } => file_id.clone(),
                other => format!("{other:?}"),
            };
            (e.body.type_name().to_string(), what)
        })
        .collect()
}

fn ev(kind: &str, path: &str) -> (String, String) {
    (kind.to_string(), path.to_string())
}

impl Fixture {
    /// A put through the page indexer, with missing parents created; `recycle_at` keeps the
    /// replaced bytes as an old version.
    fn put(&self, path: &str, bytes: &[u8], recycle_at: Option<u64>) -> u64 {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let dir = self.store.account_dir().join("staging/u1");
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("f");
        std::fs::write(&staged, bytes).unwrap();
        let options = PutOptions {
            create_parents: true,
            recycle_at_ms: recycle_at,
            ..PutOptions::default()
        };
        let mut c = self
            .store
            .plan_put_with(&g, &wp(path), &staged, Origin::Device, None, options)
            .unwrap();
        pages::plan_page_events(&self.store, &g, &mut c);
        self.store.commit(&g, c, &self.journal).unwrap();
        self.store.id_of(&wp(path)).unwrap().unwrap()
    }

    /// A delete at `ms`, with the page records, as every writer does it.
    fn delete(&self, path: &str, ms: u64) -> Vec<Event> {
        let before = self.journal.last_seq().unwrap();
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let mut c = self
            .store
            .plan_delete(&g, &wp(path), ms, Origin::Device, None)
            .unwrap();
        pages::plan_page_delete(&self.store, &g, &mut c);
        self.store.commit(&g, c, &self.journal).unwrap();
        self.since(before)
    }

    fn since(&self, seq: u64) -> Vec<Event> {
        self.journal
            .read_from(seq + 1)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    fn items(&self) -> Vec<RecycleItem> {
        let g = self.store.lock().unwrap();
        self.store.recycle_items(&g).unwrap()
    }

    fn item(&self, path: &str) -> RecycleItem {
        self.items()
            .into_iter()
            .find(|i| i.path == path)
            .unwrap_or_else(|| panic!("no item {path}"))
    }

    fn try_restore(
        &self,
        item: &RecycleItem,
        crash: Option<CrashPoint>,
    ) -> notedown_cloud::Result<Vec<u64>> {
        let g = self.store.lock_recovered(&self.journal)?;
        let mut c = self
            .store
            .plan_restore(&g, item, &wp(&item.path), NOW + DAY, Origin::App)?;
        pages::plan_page_restore(&self.store, &g, &mut c)?;
        self.store.commit_until(&g, c, &self.journal, crash)
    }

    /// Restore `item` at its old path; returns the events of the restore.
    fn restore(&self, item: &RecycleItem) -> Vec<Event> {
        let before = self.journal.last_seq().unwrap();
        self.try_restore(item, None).unwrap();
        self.since(before)
    }

    /// A second process after a crash: fresh handles, then recovery.
    fn restart_and_recover(&self) -> Recovery {
        let store = Store::open(self._dir.path(), "jordi").unwrap();
        let journal = Journal::open(self._dir.path()).unwrap();
        let g = store.lock().unwrap();
        store.recover(&g, &journal).unwrap()
    }

    fn id(&self, path: &str) -> Option<u64> {
        self.store.id_of(&wp(path)).unwrap()
    }

    fn bytes(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.store.fs_path(&wp(path))).unwrap()
    }

    fn record(&self, file_name: &str) -> Option<PageRecord> {
        let p = self
            .store
            .account_dir()
            .join(pages::PAGES_DIR)
            .join(format!("{file_name}.json"));
        Some(serde_json::from_slice(&std::fs::read(p).ok()?).unwrap())
    }

    fn index_lines(&self) -> Vec<RecycleEntry> {
        let g = self.store.lock().unwrap();
        self.store.recycle_index(&g).unwrap()
    }

    fn scan_events(&self) -> Vec<Event> {
        let before = self.journal.last_seq().unwrap();
        let g = self.store.lock_recovered(&self.journal).unwrap();
        self.store
            .scan(&g, &self.journal, &ScanOptions::all(0), NOW + 2 * DAY)
            .unwrap();
        self.since(before)
    }
}

#[test]
fn a_restore_brings_a_file_back_with_a_new_id() {
    let f = Fixture::new();
    let old = f.put("NOTE/Note/x.txt", b"hello", None);
    f.delete("NOTE/Note/x.txt", NOW);
    let item = f.item("NOTE/Note/x.txt");
    let slot = f.slot_path(&item.entry);
    assert!(slot.is_file());
    let events = f.restore(&item);
    assert_eq!(f.bytes("NOTE/Note/x.txt"), b"hello");
    let id = f.id("NOTE/Note/x.txt").unwrap();
    assert_ne!(id, old);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].origin, Origin::App);
    assert_eq!(
        events[0].body,
        EventBody::FileCreated {
            id,
            path: "NOTE/Note/x.txt".into(),
            kind: FileKind::Document,
            md5: md5_hex(b"hello"),
            size: 5,
            pages_changed: None,
        }
    );
    assert!(f.items().is_empty());
    assert!(f.index_lines().is_empty());
    assert!(!slot.exists());
    // The emptied folders of the slot are gone too.
    assert!(!f.recycle().join(NOW.to_string()).exists());
    assert!(f.recycle().is_dir());
}

#[test]
fn a_restore_creates_missing_parents() {
    let f = Fixture::new();
    f.put("DOCUMENT/Document/a/b/x.pdf", b"pdf", None);
    let deleted = f.delete("DOCUMENT/Document/a/b/x.pdf", NOW);
    assert_eq!(
        shape(&deleted),
        [
            ev("file.deleted", "DOCUMENT/Document/a/b/x.pdf"),
            ev("folder.deleted", "DOCUMENT/Document/a/b"),
            ev("folder.deleted", "DOCUMENT/Document/a"),
        ]
    );
    let events = f.restore(&f.item("DOCUMENT/Document/a/b/x.pdf"));
    assert_eq!(
        shape(&events),
        [
            ev("folder.created", "DOCUMENT/Document/a"),
            ev("folder.created", "DOCUMENT/Document/a/b"),
            ev("file.created", "DOCUMENT/Document/a/b/x.pdf"),
        ]
    );
    let EventBody::FolderCreated { id, .. } = &events[0].body else {
        panic!()
    };
    assert_eq!(f.id("DOCUMENT/Document/a"), Some(*id));
    assert_eq!(f.bytes("DOCUMENT/Document/a/b/x.pdf"), b"pdf");
}

#[test]
fn a_folder_restore_brings_its_tree_back() {
    let f = Fixture::new();
    let a = f.put("NOTE/Note/W/a.txt", b"aaa", None);
    let b = f.put("NOTE/Note/W/S/b.txt", b"bb", None);
    let w = f.id("NOTE/Note/W").unwrap();
    f.delete("NOTE/Note/W", NOW);
    let item = f.item("NOTE/Note/W");
    assert_eq!(item.files, Some(2));
    // A dot-file moves along without an event.
    std::fs::write(f.slot_path(&item.entry).join(".DS_Store"), b"d").unwrap();
    let events = f.restore(&item);
    assert_eq!(
        shape(&events),
        [
            ev("folder.created", "NOTE/Note/W"),
            ev("folder.created", "NOTE/Note/W/S"),
            ev("file.created", "NOTE/Note/W/S/b.txt"),
            ev("file.created", "NOTE/Note/W/a.txt"),
        ]
    );
    let ids: Vec<u64> = events
        .iter()
        .map(|e| match &e.body {
            EventBody::FileCreated { id, path, .. } | EventBody::FolderCreated { id, path } => {
                assert_eq!(f.id(path), Some(*id), "{path}");
                *id
            }
            other => panic!("{other:?}"),
        })
        .collect();
    for old in [a, b, w] {
        assert!(!ids.contains(&old));
    }
    assert!(events.iter().all(|e| e.origin == Origin::App));
    assert_eq!(f.bytes("NOTE/Note/W/a.txt"), b"aaa");
    assert_eq!(f.bytes("NOTE/Note/W/S/b.txt"), b"bb");
    assert_eq!(f.bytes("NOTE/Note/W/.DS_Store"), b"d");
    assert!(f.items().is_empty());
    assert!(f.index_lines().is_empty());
}

#[test]
fn a_folder_restore_journals_only_the_files_that_are_there() {
    let f = Fixture::new();
    f.put("NOTE/Note/W/a.txt", b"aaa", None);
    f.put("NOTE/Note/W/b.txt", b"bb", None);
    f.delete("NOTE/Note/W", NOW);
    let item = f.item("NOTE/Note/W");
    std::fs::remove_file(f.slot_path(&item.entry).join("b.txt")).unwrap();
    let events = f.restore(&item);
    assert_eq!(
        shape(&events),
        [
            ev("folder.created", "NOTE/Note/W"),
            ev("file.created", "NOTE/Note/W/a.txt"),
        ]
    );
    assert_eq!(f.id("NOTE/Note/W/b.txt"), None);
    // Every line of the item goes, also the one whose file was missing.
    assert!(f.index_lines().is_empty());
}

#[test]
fn a_restore_of_a_vanished_slot_is_not_found() {
    let f = Fixture::new();
    f.put("NOTE/Note/x.txt", b"hello", None);
    f.delete("NOTE/Note/x.txt", NOW);
    let item = f.item("NOTE/Note/x.txt");
    std::fs::remove_file(f.slot_path(&item.entry)).unwrap();
    let before = f.journal.last_seq().unwrap();
    let r = f.try_restore(&item, None);
    assert!(
        matches!(r, Err(notedown_cloud::Error::NotFound(_))),
        "{r:?}"
    );
    assert!(!f.store.fs_path(&wp("NOTE/Note/x.txt")).exists());
    assert_eq!(f.id("NOTE/Note/x.txt"), None);
    assert!(!f.store.account_dir().join(PENDING_FILE).exists());
    assert_eq!(f.journal.last_seq().unwrap(), before);
}

#[test]
fn a_restore_refuses_a_symlink_in_the_slot() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"s").unwrap();
    // A symlink inside a folder slot.
    f.put("NOTE/Note/W/a.txt", b"aaa", None);
    f.delete("NOTE/Note/W", NOW);
    let item = f.item("NOTE/Note/W");
    let slot = f.slot_path(&item.entry);
    std::os::unix::fs::symlink(outside.path().join("secret"), slot.join("link.txt")).unwrap();
    let r = f.try_restore(&item, None);
    assert!(
        matches!(r, Err(notedown_cloud::Error::RecycleSlotDamaged(_))),
        "{r:?}"
    );
    assert!(slot.join("a.txt").is_file());
    assert_eq!(f.id("NOTE/Note/W"), None);
    // A file slot that is a symlink itself.
    f.put("NOTE/Note/x.txt", b"x", None);
    f.delete("NOTE/Note/x.txt", NOW + 1);
    let item = f.item("NOTE/Note/x.txt");
    let slot = f.slot_path(&item.entry);
    std::fs::remove_file(&slot).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), &slot).unwrap();
    let r = f.try_restore(&item, None);
    assert!(
        matches!(r, Err(notedown_cloud::Error::RecycleSlotDamaged(_))),
        "{r:?}"
    );
    assert!(!f.store.fs_path(&wp("NOTE/Note/x.txt")).exists());
    assert_eq!(f.items().len(), 2);
    assert!(!f.store.account_dir().join(PENDING_FILE).exists());
}

#[test]
fn a_restore_moves_the_old_versions_to_the_new_id() {
    let f = Fixture::new();
    let old = f.put("NOTE/Note/x.txt", b"one", None);
    f.put("NOTE/Note/x.txt", b"two", Some(NOW - HOUR));
    f.delete("NOTE/Note/x.txt", NOW);
    let version = f.index_lines()[0].clone();
    assert_eq!(
        (version.id, version.reason),
        (old, RecycleReason::Overwritten)
    );
    f.restore(&f.item("NOTE/Note/x.txt"));
    let id = f.id("NOTE/Note/x.txt").unwrap();
    assert_eq!(
        f.index_lines(),
        [RecycleEntry {
            id,
            ..version.clone()
        }]
    );
    assert_eq!(std::fs::read(f.slot_path(&version)).unwrap(), b"one");
    assert_eq!(f.bytes("NOTE/Note/x.txt"), b"two");
}

/// A file in a restored folder: its old versions follow it to its new id too.
#[test]
fn a_folder_restore_moves_the_old_versions_of_its_files() {
    let f = Fixture::new();
    let old = f.put("NOTE/Note/W/a.txt", b"one", None);
    f.put("NOTE/Note/W/a.txt", b"two", Some(NOW - HOUR));
    f.delete("NOTE/Note/W", NOW);
    let version = f.index_lines()[0].clone();
    assert_eq!(
        (version.id, version.reason),
        (old, RecycleReason::Overwritten)
    );
    f.restore(&f.item("NOTE/Note/W"));
    let id = f.id("NOTE/Note/W/a.txt").unwrap();
    assert_ne!(id, old);
    assert_eq!(f.index_lines(), [RecycleEntry { id, ..version }]);
}

#[test]
fn a_restore_cut_after_the_rename_is_finished_by_recovery() {
    let f = Fixture::new();
    f.put("NOTE/Note/x.txt", b"hello", None);
    f.put("NOTE/Note/W/a.txt", b"aaa", None);
    f.put("NOTE/Note/W/S/b.txt", b"bb", None);
    f.delete("NOTE/Note/x.txt", NOW);
    f.delete("NOTE/Note/W", NOW + 1);
    for (path, files) in [
        ("NOTE/Note/x.txt", vec!["NOTE/Note/x.txt"]),
        (
            "NOTE/Note/W",
            vec!["NOTE/Note/W/S/b.txt", "NOTE/Note/W/a.txt"],
        ),
    ] {
        let item = f.item(path);
        let before = f.journal.last_seq().unwrap();
        f.try_restore(&item, Some(CrashPoint::BeforeAppend))
            .unwrap();
        assert!(f.store.account_dir().join(PENDING_FILE).exists());
        assert!(f.since(before).is_empty());
        assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
        let events = f.since(before);
        assert!(events.iter().all(|e| e.origin == Origin::Repair));
        let created: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::FileCreated { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(created, files);
        for file in files {
            assert!(f.id(file).is_some());
        }
        assert!(!f.slot_path(&item.entry).exists());
        assert!(!f.store.account_dir().join(PENDING_FILE).exists());
    }
    assert!(f.items().is_empty());
    assert!(f.index_lines().is_empty());
    assert!(f.scan_events().is_empty());
}

#[test]
fn a_restore_cut_before_clear_keeps_the_other_items() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    for (n, name) in ["x.txt", "y.txt", "z.txt"].iter().enumerate() {
        f.put(&format!("NOTE/Note/{name}"), name.as_bytes(), None);
        f.delete(&format!("NOTE/Note/{name}"), NOW + n as u64);
    }
    let item = f.item("NOTE/Note/y.txt");
    let before = f.journal.last_seq().unwrap();
    f.try_restore(&item, Some(CrashPoint::BeforeClear)).unwrap();
    assert!(f.store.account_dir().join(PENDING_FILE).exists());
    let inode = std::fs::metadata(f.index_path()).unwrap().ino();
    let index = f.index_text();
    assert!(matches!(
        f.restart_and_recover(),
        Recovery::Repaired { seqs, .. } if seqs.is_empty()
    ));
    // The index was written once: recovery found nothing left to do.
    assert_eq!(std::fs::metadata(f.index_path()).unwrap().ino(), inode);
    assert_eq!(f.index_text(), index);
    let keys: Vec<String> = f.items().into_iter().map(|i| i.key).collect();
    assert_eq!(
        keys,
        [
            format!("{}/NOTE/Note/z.txt", NOW + 2),
            format!("{NOW}/NOTE/Note/x.txt"),
        ]
    );
    assert!(f.slot_path(&f.item("NOTE/Note/x.txt").entry).is_file());
    assert!(f.slot_path(&f.item("NOTE/Note/z.txt").entry).is_file());
    assert_eq!(
        shape(&f.since(before)),
        [ev("file.created", "NOTE/Note/y.txt")]
    );
    assert_eq!(f.bytes("NOTE/Note/y.txt"), b"y.txt");
}

#[test]
fn a_restore_cut_before_apply_is_discarded() {
    for crash in [CrashPoint::BeforeApply, CrashPoint::BeforeRename] {
        let f = Fixture::new();
        f.put("DOCUMENT/Document/a/b/x.pdf", b"pdf", None);
        f.delete("DOCUMENT/Document/a/b/x.pdf", NOW);
        let item = f.item("DOCUMENT/Document/a/b/x.pdf");
        let before = f.journal.last_seq().unwrap();
        f.try_restore(&item, Some(crash)).unwrap();
        assert!(f.store.account_dir().join(PENDING_FILE).exists());
        assert!(matches!(
            f.restart_and_recover(),
            Recovery::Discarded { .. }
        ));
        assert_eq!(f.items(), std::slice::from_ref(&item), "{crash:?}");
        assert!(f.slot_path(&item.entry).is_file());
        assert!(!f.store.fs_path(&wp("DOCUMENT/Document/a")).exists());
        for p in [
            "DOCUMENT/Document/a",
            "DOCUMENT/Document/a/b",
            "DOCUMENT/Document/a/b/x.pdf",
        ] {
            assert_eq!(f.id(p), None, "{crash:?} {p}");
        }
        assert!(f.since(before).is_empty());
        // No row of a planned path is left for the scan to find gone.
        assert!(f.scan_events().is_empty(), "{crash:?}");
    }
}

#[test]
fn a_restored_note_has_its_page_record() {
    let f = Fixture::new();
    let bytes = note(FID, &["PA", "PB", "PC"]);
    f.put("NOTE/Note/n.note", &bytes, None);
    assert!(f.record(FID).is_some());
    f.delete("NOTE/Note/n.note", NOW);
    assert!(f.record(FID).is_none());
    let events = f.restore(&f.item("NOTE/Note/n.note"));
    let id = f.id("NOTE/Note/n.note").unwrap();
    assert_eq!(
        shape(&events),
        [
            ev("file.created", "NOTE/Note/n.note"),
            ev("page.added", FID),
            ev("page.added", FID),
            ev("page.added", FID),
        ]
    );
    assert!(matches!(
        events[0].body,
        EventBody::FileCreated {
            pages_changed: Some(3),
            ..
        }
    ));
    assert!(
        events[1..]
            .iter()
            .all(|e| matches!(e.body, EventBody::PageAdded { id: i, .. } if i == id))
    );
    let rec = f.record(FID).unwrap();
    assert_eq!(rec.file_id, FID);
    assert_eq!(rec.path, "NOTE/Note/n.note");
    assert_eq!(rec.md5, md5_hex(&bytes));
    assert_eq!(rec.size, bytes.len() as u64);
    assert_eq!(rec.pages.len(), 3);
}

#[test]
fn a_restored_folder_indexes_every_note() {
    let f = Fixture::new();
    // Two copies of one note: the first in path order gets the FILE_ID, the other its path key.
    let bytes = note(FID, &["PA"]);
    f.put("NOTE/Note/W/a.note", &bytes, None);
    f.put("NOTE/Note/W/b.note", &bytes, None);
    f.put("NOTE/Note/W/c.txt", b"c", None);
    f.delete("NOTE/Note/W", NOW);
    let events = f.restore(&f.item("NOTE/Note/W"));
    let path_key = format!("path:{}", md5_hex(b"NOTE/Note/W/b.note"));
    assert_eq!(
        shape(&events),
        [
            ev("folder.created", "NOTE/Note/W"),
            ev("file.created", "NOTE/Note/W/a.note"),
            ev("page.added", FID),
            ev("file.created", "NOTE/Note/W/b.note"),
            ev("page.added", &path_key),
            ev("file.created", "NOTE/Note/W/c.txt"),
        ]
    );
    assert_eq!(f.record(FID).unwrap().path, "NOTE/Note/W/a.note");
    let rec = f
        .record(&format!("path-{}", md5_hex(b"NOTE/Note/W/b.note")))
        .unwrap();
    assert_eq!(rec.path, "NOTE/Note/W/b.note");
    assert_eq!(rec.md5, md5_hex(&bytes));
}

#[test]
fn a_restore_cut_before_append_still_writes_the_note_record() {
    let f = Fixture::new();
    let bytes = note(FID, &["PA", "PB"]);
    f.put("NOTE/Note/n.note", &bytes, None);
    f.delete("NOTE/Note/n.note", NOW);
    let before = f.journal.last_seq().unwrap();
    f.try_restore(&f.item("NOTE/Note/n.note"), Some(CrashPoint::BeforeAppend))
        .unwrap();
    assert!(f.record(FID).is_none());
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    let rec = f.record(FID).unwrap();
    assert_eq!(rec.path, "NOTE/Note/n.note");
    assert_eq!(rec.md5, md5_hex(&bytes));
    assert_eq!(
        shape(&f.since(before)),
        [
            ev("file.created", "NOTE/Note/n.note"),
            ev("page.added", FID),
            ev("page.added", FID),
        ]
    );
}

#[test]
fn a_restore_is_not_journaled_again_by_the_scan() {
    let f = Fixture::new();
    f.put("NOTE/Note/W/a.txt", b"aaa", None);
    f.put("NOTE/Note/W/S/n.note", &note(FID, &["PA"]), None);
    f.delete("NOTE/Note/W", NOW);
    std::fs::write(
        f.slot_path(&f.item("NOTE/Note/W").entry).join(".DS_Store"),
        b"d",
    )
    .unwrap();
    f.restore(&f.item("NOTE/Note/W"));
    assert!(f.scan_events().is_empty());
    assert_eq!(f.record(FID).unwrap().path, "NOTE/Note/W/S/n.note");
}

#[test]
fn a_restored_file_can_be_deleted_and_restored_again() {
    let f = Fixture::new();
    f.put("NOTE/Note/x.txt", b"hello", None);
    f.delete("NOTE/Note/x.txt", NOW);
    let first = f.item("NOTE/Note/x.txt");
    f.restore(&first);
    // A later delete of the same path is a new item with its own slot. (A delete in the very
    // ms of the restored one would get its freed slot and key again: the restore leaves no
    // trace of the slot.)
    f.delete("NOTE/Note/x.txt", NOW + 1);
    let second = f.item("NOTE/Note/x.txt");
    assert_ne!(second.key, first.key);
    assert_eq!(f.items().len(), 1);
    // The first key is gone for good.
    assert!(!f.slot_path(&first.entry).exists());
    assert_eq!(
        f.try_restore(&first, None).map_err(|e| e.to_string()),
        Err(format!("recycle bin item {} is not there", first.key))
    );
    let events = f.restore(&second);
    assert_eq!(shape(&events), [ev("file.created", "NOTE/Note/x.txt")]);
    assert_eq!(f.bytes("NOTE/Note/x.txt"), b"hello");
    assert!(f.items().is_empty());
    assert!(f.index_lines().is_empty());
}
