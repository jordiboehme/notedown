//! The prune of the recycle bin: `Store::prune_recycle` on a store in a temp folder with a fixed
//! clock and offset `+02:00`. Slots and index lines are written directly where a real commit
//! can't place them (old times, ids the store no longer knows).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::{Event, EventBody, Journal, Origin};
use notedown_cloud::store::retention::Retention;
use notedown_cloud::store::{
    CrashPoint, IndexLine, PENDING_FILE, PutOptions, RecycleEntry, RecycleReason, Store, WirePath,
};
use time::UtcOffset;

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
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![y.clone(), folder.clone()]);
    assert!(!f.slot_path(&y).exists());
    assert!(f.slot_path(&folder).is_dir());
    assert!(f.slot_path(&x).is_file());
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

#[test]
fn a_folder_line_leaves_a_symlink_and_its_target_alone() {
    let f = Fixture::new();
    let ms = NOW - 91 * DAY;
    let folder = folder_entry(DEAD, "NOTE/Note/A", ms);
    f.add(&folder, b"");
    let outside = f.store.account_dir().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, f.slot_path(&folder).join("link")).unwrap();
    let r = f.prune(NOW, Origin::Cli, false);
    assert_eq!(r.removed, vec![folder.clone()]);
    assert!(outside.join("keep.txt").is_file());
    #[cfg(unix)]
    assert!(f.slot_path(&folder).join("link").is_symlink());
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
    assert_eq!(
        r.removed,
        vec![stuck.clone(), folder.clone(), other.clone()]
    );
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
