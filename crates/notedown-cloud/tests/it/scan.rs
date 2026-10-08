//! The scan of changes made by hand in `files/` (`Store::scan`), on a store in a temp folder.
//! Files are placed, changed and removed directly on disk, as a person would. The baseline is
//! made with a listing, which gives ids and caches the MD5s without journal events.

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use notedown_cloud::Error;
use notedown_cloud::auth::{Accounts, Imported};
use notedown_cloud::journal::{Event, EventBody, FileKind, Journal, Origin};
use notedown_cloud::pages::fingerprint_note;
use notedown_cloud::store::{Kind, SCAN_HOLD_FILE, ScanOptions, ScanReport, Store, WirePath};

const EMAIL: &str = "jordi@example.org";
/// 2026-10-06 08:20 UTC.
const NOW: u64 = 1_791_270_000_000;
const DOC: &str = "DOCUMENT/Document";

fn wp(s: &str) -> WirePath {
    WirePath::parse_named(s).unwrap()
}

fn md5_hex(bytes: &[u8]) -> String {
    const_hex::encode(Md5::digest(bytes))
}

fn fixture_note(name: &str) -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join(format!("tests/fixtures/notes/{name}.note"))).unwrap()
}

struct Fx {
    dir: tempfile::TempDir,
    store: Store,
    journal: Journal,
}

impl Fx {
    /// An imported account with its native folders (ids without events).
    fn imported() -> Fx {
        let fx = Fx::plain();
        let accounts = Accounts::new(fx.dir.path());
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 0,
            bytes: 0,
        });
        accounts.save(&a).unwrap();
        fx
    }

    /// An account that was never imported, with its native folders (ids without events).
    fn plain() -> Fx {
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
        Fx {
            dir,
            store,
            journal,
        }
    }

    fn fs(&self, rel: &str) -> PathBuf {
        self.store.fs_path(&wp(rel))
    }

    /// Place a file by hand (parents too).
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.fs(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }

    fn rm(&self, rel: &str) {
        std::fs::remove_file(self.fs(rel)).unwrap();
    }

    /// Give every file on disk an id and a cached MD5, without events (a listing).
    fn seed(&self) {
        let g = self.store.lock().unwrap();
        self.store.list(&g, &WirePath::root(), true).unwrap();
    }

    fn id(&self, rel: &str) -> Option<u64> {
        self.store.id_of(&wp(rel)).unwrap()
    }

    fn try_scan(&self, opts: &ScanOptions, now: u64) -> notedown_cloud::Result<ScanReport> {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        self.store.scan(&g, &self.journal, opts, now)
    }

    fn scan_with(&self, opts: &ScanOptions) -> ScanReport {
        self.try_scan(opts, NOW).unwrap()
    }

    fn scan(&self) -> ScanReport {
        self.scan_with(&ScanOptions::all(1))
    }

    fn events(&self) -> Vec<Event> {
        self.journal
            .read_from(1)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    fn bodies(&self) -> Vec<EventBody> {
        self.events().into_iter().map(|e| e.body).collect()
    }

    fn account_file(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.store.account_dir().join(name)).ok()
    }

    /// The registry and cache files, to check that nothing was written.
    fn state_files(&self) -> Vec<Option<Vec<u8>>> {
        ["ids.json", "ids.jsonl", "hashes.json"]
            .iter()
            .map(|n| self.account_file(n))
            .collect()
    }

    fn hold_marker(&self) -> Option<serde_json::Value> {
        self.account_file(SCAN_HOLD_FILE)
            .map(|b| serde_json::from_slice(&b).unwrap())
    }
}

fn kind_names(bodies: &[EventBody]) -> Vec<&'static str> {
    bodies
        .iter()
        .map(|b| match b {
            EventBody::FileCreated { .. } => "file.created",
            EventBody::FileModified { .. } => "file.modified",
            EventBody::FileMoved { .. } => "file.moved",
            EventBody::FileDeleted { .. } => "file.deleted",
            EventBody::FolderCreated { .. } => "folder.created",
            EventBody::FolderDeleted { .. } => "folder.deleted",
            EventBody::PageAdded { .. } => "page.added",
            EventBody::PageRemoved { .. } => "page.removed",
            _ => "other",
        })
        .collect()
}

fn paths(v: &[WirePath]) -> Vec<String> {
    v.iter().map(WirePath::display).collect()
}

// ---------------------------------------------------------------- what the scan journals

#[test]
fn a_file_added_by_hand_is_journaled_as_created_by_the_scan() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.pdf");
    fx.write(&path, b"hello");
    let report = fx.scan();
    let id = fx.id(&path).expect("an id was assigned");
    let events = fx.events();
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    assert_eq!(e.origin, Origin::Scan);
    assert_eq!(e.device, None);
    assert_eq!(
        e.body,
        EventBody::FileCreated {
            id,
            path: path.clone(),
            kind: FileKind::Document,
            md5: md5_hex(b"hello"),
            size: 5,
            pages_changed: None,
        }
    );
    let line = serde_json::to_value(e).unwrap();
    assert_eq!(line["origin"], "scan");
    assert!(line.get("device").is_none());
    assert_eq!(paths(&report.created), vec![path]);
    assert_eq!(report.seqs, vec![e.seq]);
    // A second scan finds nothing.
    let again = fx.scan();
    assert_eq!(again, ScanReport::default());
    assert_eq!(fx.events().len(), 1);
}

#[test]
fn a_new_folder_with_files_gives_folder_then_file_events() {
    let fx = Fx::imported();
    fx.write("Work/a.txt", b"a");
    fx.write("Work/Sub/b.txt", b"bb");
    fx.scan();
    let bodies = fx.bodies();
    assert_eq!(
        kind_names(&bodies),
        vec![
            "folder.created",
            "folder.created",
            "file.created",
            "file.created"
        ]
    );
    let EventBody::FolderCreated { id, path } = &bodies[0] else {
        unreachable!()
    };
    assert_eq!(path, "Work");
    assert_eq!(Some(*id), fx.id("Work"));
    let EventBody::FolderCreated { path, .. } = &bodies[1] else {
        unreachable!()
    };
    assert_eq!(path, "Work/Sub");
    for b in &bodies[2..] {
        let EventBody::FileCreated { id, path, .. } = b else {
            unreachable!()
        };
        assert_eq!(Some(*id), fx.id(path));
    }
    // All events of one scan share one opId.
    let events = fx.events();
    assert!(events.iter().all(|e| e.op_id == events[0].op_id));
}

#[test]
fn a_file_changed_by_hand_is_journaled_as_modified() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"one");
    fx.seed();
    let id = fx.id(&path).unwrap();
    fx.write(&path, b"three");
    let report = fx.scan();
    assert_eq!(
        fx.bodies(),
        vec![EventBody::FileModified {
            id,
            path: path.clone(),
            kind: FileKind::Document,
            md5: md5_hex(b"three"),
            prev_md5: md5_hex(b"one"),
            size: 5,
            pages_changed: None,
        }]
    );
    assert_eq!(fx.id(&path), Some(id));
    assert_eq!(paths(&report.modified), vec![path]);
    assert_eq!(fx.scan(), ScanReport::default());
}

#[test]
fn a_file_without_a_cached_md5_is_hashed_silently() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"one");
    fx.seed();
    std::fs::remove_file(fx.store.account_dir().join("hashes.json")).unwrap();
    // A fresh store reads the missing cache.
    let store = Store::open(fx.dir.path(), "jordi").unwrap();
    let g = store.lock_recovered(&fx.journal).unwrap();
    let report = store
        .scan(&g, &fx.journal, &ScanOptions::all(1), NOW)
        .unwrap();
    assert_eq!(report, ScanReport::default());
    assert!(fx.events().is_empty());
    assert!(fx.account_file("hashes.json").is_some());
}

#[test]
fn a_file_deleted_by_hand_is_journaled_as_deleted_and_its_id_is_dropped() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"one");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    let id = fx.id(&path).unwrap();
    fx.rm(&path);
    let report = fx.scan();
    assert_eq!(
        fx.bodies(),
        vec![EventBody::FileDeleted {
            id,
            path: path.clone(),
            recycle: String::new(),
        }]
    );
    assert_eq!(fx.id(&path), None);
    assert_eq!(paths(&report.deleted), vec![path]);
    assert_eq!(report.held, None);
}

#[test]
fn a_folder_deleted_by_hand_gives_file_then_folder_deletes() {
    let fx = Fx::imported();
    fx.write("Work/a.txt", b"a");
    fx.write("Work/Sub/b.txt", b"bb");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    let work = fx.id("Work").unwrap();
    let sub = fx.id("Work/Sub").unwrap();
    std::fs::remove_dir_all(fx.fs("Work")).unwrap();
    fx.scan();
    let bodies = fx.bodies();
    assert_eq!(
        kind_names(&bodies),
        vec![
            "file.deleted",
            "file.deleted",
            "folder.deleted",
            "folder.deleted"
        ]
    );
    assert_eq!(
        bodies[2],
        EventBody::FolderDeleted {
            id: sub,
            path: "Work/Sub".into()
        }
    );
    assert_eq!(
        bodies[3],
        EventBody::FolderDeleted {
            id: work,
            path: "Work".into()
        }
    );
    for p in ["Work", "Work/Sub", "Work/a.txt", "Work/Sub/b.txt"] {
        assert_eq!(fx.id(p), None, "{p}");
    }
    assert!(fx.id(&format!("{DOC}/keep.txt")).is_some());
}

#[test]
fn a_file_moved_by_hand_keeps_its_id() {
    let fx = Fx::imported();
    let from = format!("{DOC}/a.txt");
    fx.write(&from, b"content");
    fx.seed();
    let id = fx.id(&from).unwrap();
    std::fs::create_dir_all(fx.fs("Work")).unwrap();
    std::fs::rename(fx.fs(&from), fx.fs("Work/b.txt")).unwrap();
    let report = fx.scan();
    let bodies = fx.bodies();
    assert_eq!(kind_names(&bodies), vec!["folder.created", "file.moved"]);
    assert_eq!(
        bodies[1],
        EventBody::FileMoved {
            id,
            path: "Work/b.txt".into(),
            old_path: from.clone(),
        }
    );
    assert_eq!(fx.id("Work/b.txt"), Some(id));
    assert_eq!(fx.id(&from), None);
    assert_eq!(report.moved, vec![(wp(&from), wp("Work/b.txt"))]);
    assert_eq!(fx.scan(), ScanReport::default());
}

#[test]
fn a_case_only_rename_by_hand_keeps_its_id() {
    let fx = Fx::imported();
    let from = format!("{DOC}/report.txt");
    let to = format!("{DOC}/Report.txt");
    fx.write(&from, b"content");
    fx.seed();
    let id = fx.id(&from).unwrap();
    std::fs::rename(fx.fs(&from), fx.fs(&to)).unwrap();
    fx.scan();
    assert_eq!(
        fx.bodies(),
        vec![EventBody::FileMoved {
            id,
            path: to.clone(),
            old_path: from.clone(),
        }]
    );
    assert_eq!(fx.id(&to), Some(id));
    assert_eq!(fx.id(&from), None);
}

#[test]
fn a_folder_renamed_by_case_only_keeps_the_ids_of_its_files() {
    let fx = Fx::imported();
    fx.write("Work/a.txt", b"a");
    fx.write("Work/Sub/b.txt", b"bb");
    fx.seed();
    let a = fx.id("Work/a.txt").unwrap();
    let b = fx.id("Work/Sub/b.txt").unwrap();
    let work = fx.id("Work").unwrap();
    let sub = fx.id("Work/Sub").unwrap();
    std::fs::rename(fx.fs("Work"), fx.fs("work")).unwrap();
    fx.scan();
    let bodies = fx.bodies();
    assert_eq!(
        kind_names(&bodies),
        vec![
            "folder.created",
            "folder.created",
            "file.moved",
            "file.moved",
            "folder.deleted",
            "folder.deleted"
        ]
    );
    assert_eq!(
        bodies[4],
        EventBody::FolderDeleted {
            id: sub,
            path: "Work/Sub".into()
        }
    );
    assert_eq!(
        bodies[5],
        EventBody::FolderDeleted {
            id: work,
            path: "Work".into()
        }
    );
    // The folder removes in the registry never drop the moved rows.
    assert_eq!(fx.id("work/a.txt"), Some(a));
    assert_eq!(fx.id("work/Sub/b.txt"), Some(b));
    for p in ["Work", "Work/Sub", "Work/a.txt", "Work/Sub/b.txt"] {
        assert_eq!(fx.id(p), None, "{p}");
    }
    let fresh = Store::open(fx.dir.path(), "jordi").unwrap();
    assert_eq!(fresh.id_of(&wp("work/a.txt")).unwrap(), Some(a));
    assert_eq!(fresh.id_of(&wp("work/Sub/b.txt")).unwrap(), Some(b));
    assert_eq!(fx.scan(), ScanReport::default());
}

#[test]
fn two_identical_files_moved_at_once_are_not_paired() {
    let fx = Fx::imported();
    fx.write(&format!("{DOC}/x1.txt"), b"same");
    fx.write(&format!("{DOC}/x2.txt"), b"same");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    let old = [
        fx.id(&format!("{DOC}/x1.txt")).unwrap(),
        fx.id(&format!("{DOC}/x2.txt")).unwrap(),
    ];
    for (a, b) in [("x1", "y1"), ("x2", "y2")] {
        std::fs::rename(
            fx.fs(&format!("{DOC}/{a}.txt")),
            fx.fs(&format!("{DOC}/{b}.txt")),
        )
        .unwrap();
    }
    fx.scan();
    assert_eq!(
        kind_names(&fx.bodies()),
        vec![
            "file.created",
            "file.created",
            "file.deleted",
            "file.deleted"
        ]
    );
    for b in ["y1", "y2"] {
        let id = fx.id(&format!("{DOC}/{b}.txt")).unwrap();
        assert!(!old.contains(&id));
    }
}

#[test]
fn a_note_added_by_hand_gets_its_page_index() {
    let fx = Fx::imported();
    let path = "NOTE/Note/t.note";
    let bytes = fixture_note("test1");
    fx.write(path, &bytes);
    fx.scan();
    let id = fx.id(path).unwrap();
    let bodies = fx.bodies();
    let EventBody::FileCreated {
        kind,
        pages_changed,
        md5,
        ..
    } = &bodies[0]
    else {
        panic!("{bodies:?}")
    };
    assert_eq!(*kind, FileKind::Note);
    assert_eq!(*md5, md5_hex(&bytes));
    let n = pages_changed.expect("the note was indexed");
    assert!(n > 0);
    assert_eq!(bodies.len() as u64, 1 + n);
    assert!(
        bodies[1..]
            .iter()
            .all(|b| matches!(b, EventBody::PageAdded { id: i, .. } if *i == id))
    );
    let records = std::fs::read_dir(fx.store.account_dir().join("pages"))
        .unwrap()
        .count();
    assert_eq!(records, 1);
}

#[test]
fn a_note_changed_by_hand_is_compared_with_its_record() {
    let fx = Fx::imported();
    let path = "NOTE/Note/t.note";
    fx.write(path, &fixture_note("test1"));
    fx.scan();
    // Another note at the same path: its pages are compared with the record of the old one,
    // not with the new file on disk (which would give no page events).
    fx.write(path, &fixture_note("test2"));
    let before = fx.events().len();
    fx.scan();
    let bodies: Vec<EventBody> = fx.bodies().split_off(before);
    let EventBody::FileModified { pages_changed, .. } = &bodies[0] else {
        panic!("{bodies:?}")
    };
    let old = fingerprint_note(fixture_note("test1")).unwrap().pages.len();
    let new = fingerprint_note(fixture_note("test2")).unwrap().pages.len();
    assert_eq!(*pages_changed, Some((old + new) as u64));
    let mut expect = vec!["file.modified"];
    expect.extend(std::iter::repeat_n("page.removed", old));
    expect.extend(std::iter::repeat_n("page.added", new));
    assert_eq!(kind_names(&bodies), expect);
}

#[test]
fn a_note_moved_by_hand_takes_its_record_along() {
    let fx = Fx::imported();
    fx.write("NOTE/Note/t.note", &fixture_note("test1"));
    fx.scan();
    let before = fx.events().len();
    std::fs::rename(fx.fs("NOTE/Note/t.note"), fx.fs("NOTE/Note/u.note")).unwrap();
    fx.scan();
    let bodies: Vec<EventBody> = fx.bodies().split_off(before);
    assert_eq!(kind_names(&bodies), vec!["file.moved"]);
    let pages = fx.store.account_dir().join("pages");
    let texts: Vec<String> = std::fs::read_dir(&pages)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("NOTE/Note/u.note"), "{}", texts[0]);
}

#[test]
fn a_dry_run_changes_nothing() {
    let fx = Fx::imported();
    fx.write(&format!("{DOC}/mod.txt"), b"one");
    fx.write(&format!("{DOC}/gone.txt"), b"gone");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    fx.write(&format!("{DOC}/mod.txt"), b"three");
    fx.rm(&format!("{DOC}/gone.txt"));
    fx.write("Work/new.txt", b"new");
    let state = fx.state_files();
    let opts = ScanOptions {
        dry_run: true,
        ..ScanOptions::all(1)
    };
    let report = fx.scan_with(&opts);
    assert_eq!(paths(&report.created), vec!["Work", "Work/new.txt"]);
    assert_eq!(paths(&report.modified), vec![format!("{DOC}/mod.txt")]);
    assert_eq!(paths(&report.deleted), vec![format!("{DOC}/gone.txt")]);
    assert!(report.seqs.is_empty());
    assert_eq!(fx.state_files(), state);
    assert!(fx.events().is_empty());
    assert_eq!(fx.id("Work/new.txt"), None);
    // The same scan for real still sees everything.
    let real = fx.scan();
    assert_eq!(real.created, report.created);
    assert_eq!(real.modified, report.modified);
    assert_eq!(real.deleted, report.deleted);
}

#[test]
fn a_dry_run_reports_a_hold_without_writing_the_marker() {
    let fx = Fx::imported();
    fx.write(&format!("{DOC}/a.txt"), b"a");
    fx.seed();
    fx.rm(&format!("{DOC}/a.txt"));
    let report = fx.scan_with(&ScanOptions {
        dry_run: true,
        ..ScanOptions::all(1)
    });
    let hold = report.held.expect("held");
    assert_eq!((hold.gone, hold.left, hold.min), (1, 0, 1));
    assert_eq!(fx.hold_marker(), None);
}

#[test]
fn a_scoped_scan_only_sees_its_folder() {
    let fx = Fx::imported();
    fx.write("Work/a.txt", b"a");
    fx.write("Other/b.txt", b"b");
    let report = fx.scan_with(&ScanOptions {
        scope: wp("Work"),
        recursive: true,
        ..ScanOptions::all(1)
    });
    assert_eq!(paths(&report.created), vec!["Work", "Work/a.txt"]);
    assert_eq!(fx.id("Other"), None);
    assert_eq!(fx.id("Other/b.txt"), None);
    assert_eq!(
        kind_names(&fx.bodies()),
        vec!["folder.created", "file.created"]
    );
}

#[test]
fn a_non_recursive_scan_sees_direct_children_only() {
    let fx = Fx::imported();
    fx.write("Work/a.txt", b"a");
    fx.write("Work/Sub/b.txt", b"b");
    let report = fx.scan_with(&ScanOptions {
        scope: wp("Work"),
        recursive: false,
        ..ScanOptions::all(1)
    });
    assert_eq!(
        paths(&report.created),
        vec!["Work", "Work/Sub", "Work/a.txt"]
    );
    assert_eq!(fx.id("Work/Sub/b.txt"), None);
    // A gone child folder takes its whole subtree along.
    fx.seed();
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    std::fs::remove_dir_all(fx.fs("Work/Sub")).unwrap();
    let report = fx.scan_with(&ScanOptions {
        scope: wp("Work"),
        recursive: false,
        ..ScanOptions::all(1)
    });
    assert_eq!(paths(&report.deleted), vec!["Work/Sub/b.txt", "Work/Sub"]);
    assert_eq!(fx.id("Work/Sub/b.txt"), None);
}

/// A file replaced by hand with a folder of the same name, seen by a scan of a folder below it:
/// the file's row is journaled as deleted, never dropped without an event.
#[test]
fn a_file_above_the_scope_that_became_a_folder_is_journaled_as_deleted() {
    let fx = Fx::imported();
    fx.write("INBOX/A", b"a file");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    let old = fx.id("INBOX/A").unwrap();
    fx.rm("INBOX/A");
    fx.write("INBOX/A/B/x.txt", b"x");
    let report = fx.scan_with(&ScanOptions {
        scope: wp("INBOX/A/B"),
        recursive: false,
        ..ScanOptions::all(1)
    });
    assert_eq!(
        paths(&report.created),
        vec!["INBOX/A", "INBOX/A/B", "INBOX/A/B/x.txt"]
    );
    assert_eq!(paths(&report.deleted), vec!["INBOX/A"]);
    let bodies = fx.bodies();
    assert_eq!(
        kind_names(&bodies),
        vec![
            "folder.created",
            "folder.created",
            "file.created",
            "file.deleted"
        ]
    );
    assert!(
        matches!(&bodies[3], EventBody::FileDeleted { id, path, .. } if *id == old && path == "INBOX/A"),
        "{bodies:?}"
    );
    let now = fx.id("INBOX/A").unwrap();
    assert_ne!(now, old);
    assert_eq!(
        fx.store.kind_and_path_of(now).unwrap().map(|(k, _)| k),
        Some(Kind::Folder)
    );
}

/// A scoped scan reads only its folder: changes by hand elsewhere are not journaled, and the
/// registry and cache rows there stay as they are until a scan that sees them.
#[test]
fn a_scoped_scan_leaves_the_rows_of_other_folders_alone() {
    let fx = Fx::imported();
    fx.write("A/a.txt", b"a");
    fx.write("B/changed.txt", b"one");
    fx.write("B/gone.txt", b"gone");
    fx.seed();
    let ids = (
        fx.id("B/changed.txt").unwrap(),
        fx.id("B/gone.txt").unwrap(),
    );
    let rows_of_b = || {
        let rows: serde_json::Value =
            serde_json::from_slice(&fx.account_file("hashes.json").unwrap()).unwrap();
        (rows["B/changed.txt"].clone(), rows["B/gone.txt"].clone())
    };
    let before = rows_of_b();
    fx.write("B/changed.txt", b"two, longer");
    fx.rm("B/gone.txt");
    fx.write("B/new.txt", b"new");
    fx.write("A/b.txt", b"b");
    // The root without recursion (the first scope of every write), then the folder A.
    let root = fx.scan_with(&ScanOptions {
        recursive: false,
        ..ScanOptions::all(1)
    });
    assert_eq!(root, ScanReport::default());
    let report = fx.scan_with(&ScanOptions {
        scope: wp("A"),
        recursive: true,
        ..ScanOptions::all(1)
    });
    assert_eq!(paths(&report.created), vec!["A/b.txt"]);
    assert!(report.modified.is_empty() && report.deleted.is_empty());
    assert_eq!(kind_names(&fx.bodies()), vec!["file.created"]);
    assert_eq!(fx.id("B/changed.txt"), Some(ids.0));
    assert_eq!(fx.id("B/gone.txt"), Some(ids.1));
    assert_eq!(fx.id("B/new.txt"), None);
    {
        let g = fx.store.lock().unwrap();
        let cached = |p: &str| fx.store.cached_md5(&g, &wp(p)).unwrap();
        assert_eq!(cached("B/changed.txt"), Some(md5_hex(b"one")));
        assert_eq!(cached("B/gone.txt"), Some(md5_hex(b"gone")));
    }
    assert_eq!(rows_of_b(), before);
    // A whole-tree scan then journals the three changes in B, with the MD5 before the change.
    let report = fx.scan();
    assert_eq!(paths(&report.created), vec!["B/new.txt"]);
    assert_eq!(paths(&report.modified), vec!["B/changed.txt"]);
    assert_eq!(paths(&report.deleted), vec!["B/gone.txt"]);
    let bodies = fx.bodies();
    assert!(
        bodies.iter().any(|b| matches!(
            b,
            EventBody::FileModified { id, md5, prev_md5, .. }
                if *id == ids.0 && *md5 == md5_hex(b"two, longer") && *prev_md5 == md5_hex(b"one")
        )),
        "{bodies:?}"
    );
}

// ---------------------------------------------------------------- changes while hashed

/// A file that changes while the scan hashes it is there (never gone) but gets no event this
/// time; the next scan journals it with the right MD5s.
#[test]
fn a_file_that_changes_while_hashed_is_left_for_the_next_scan() {
    let fx = Fx::imported();
    let (old, new) = (format!("{DOC}/a.txt"), format!("{DOC}/new.txt"));
    fx.write(&old, b"one");
    fx.seed();
    let id = fx.id(&old).unwrap();
    // Changed by hand, so the scan hashes both files.
    fx.write(&old, b"two");
    fx.write(&new, b"n");
    // A person writes each of them again while the scan reads it (once).
    let pending =
        std::sync::Mutex::new(vec![(old.clone(), fx.fs(&old)), (new.clone(), fx.fs(&new))]);
    fx.store.set_scan_hash_hook(move |path| {
        let mut pending = pending.lock().unwrap();
        if let Some(i) = pending.iter().position(|(p, _)| *p == path.display()) {
            let (p, f) = pending.remove(i);
            std::fs::write(f, format!("{p} changed while it was read")).unwrap();
        }
    });
    let report = fx.scan();
    assert_eq!(report, ScanReport::default());
    assert!(fx.events().is_empty(), "{:?}", fx.events());
    assert_eq!(fx.id(&new), None);
    assert_eq!(fx.id(&old), Some(id));
    let report = fx.scan();
    assert_eq!(paths(&report.created), vec![new.clone()]);
    assert_eq!(paths(&report.modified), vec![old.clone()]);
    let modified = fx
        .bodies()
        .iter()
        .find_map(|b| match b {
            EventBody::FileModified {
                id: i,
                md5,
                prev_md5,
                ..
            } if *i == id => Some((md5.clone(), prev_md5.clone())),
            _ => None,
        })
        .expect("file.modified");
    let now = std::fs::read(fx.fs(&old)).unwrap();
    assert_eq!(modified, (md5_hex(&now), md5_hex(b"one")));
}

/// A file that vanishes while the scan hashes it is gone.
#[test]
fn a_file_that_vanishes_while_hashed_is_gone() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"a");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    let id = fx.id(&path).unwrap();
    // Changed by hand, so the scan reads it.
    fx.write(&path, b"aa");
    let fs = fx.fs(&path);
    let key = path.clone();
    fx.store.set_scan_hash_hook(move |p| {
        if p.display() == key {
            let _ = std::fs::remove_file(&fs);
        }
    });
    let report = fx.scan();
    assert_eq!(paths(&report.deleted), vec![path.clone()]);
    assert!(
        matches!(&fx.bodies()[..], [EventBody::FileDeleted { id: i, .. }] if *i == id),
        "{:?}",
        fx.bodies()
    );
    assert_eq!(fx.id(&path), None);
}

/// Enough changed files for the scan to hash them on several threads: each gets the MD5 of its
/// bytes, a file removed while the threads hash is gone and a file changed while it is hashed
/// is left for the next scan.
#[test]
fn many_files_hashed_on_threads_keep_their_md5_and_the_per_file_rules() {
    let fx = Fx::imported();
    // Well above the count from which the scan hashes on threads (8).
    let names: Vec<String> = (0..40).map(|i| format!("{DOC}/t{i:02}.txt")).collect();
    for (i, p) in names.iter().enumerate() {
        fx.write(p, format!("file {i}").as_bytes());
    }
    fx.seed();
    let body = |i: usize| -> Vec<u8> {
        // Some files take more than one read of the hash buffer (256 KiB).
        let len = if i % 10 == 3 { 600 * 1024 + i } else { 100 + i };
        (0..len).map(|b| (b * 7 + i) as u8).collect()
    };
    for (i, p) in names.iter().enumerate() {
        fx.write(p, &body(i));
    }
    let gone = [names[5].clone(), names[31].clone()];
    let changed = names[17].clone();
    let later = b"changed while it was read".to_vec();
    let removed: Vec<(String, PathBuf)> = gone.iter().map(|p| (p.clone(), fx.fs(p))).collect();
    let rewrite = std::sync::Mutex::new(Some((changed.clone(), fx.fs(&changed), later.clone())));
    let caller = std::thread::current().id();
    let other_thread = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = other_thread.clone();
    fx.store.set_scan_hash_hook(move |p| {
        if std::thread::current().id() != caller {
            seen.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some((_, fs)) = removed.iter().find(|(k, _)| *k == p.display()) {
            let _ = std::fs::remove_file(fs);
        }
        let mut rewrite = rewrite.lock().unwrap();
        if rewrite.as_ref().is_some_and(|(k, _, _)| *k == p.display()) {
            let (_, fs, bytes) = rewrite.take().unwrap();
            std::fs::write(fs, bytes).unwrap();
        }
    });
    let report = fx.scan();
    if std::thread::available_parallelism().map_or(1, |n| n.get()) > 1 {
        assert!(other_thread.load(std::sync::atomic::Ordering::Relaxed));
    }
    assert_eq!(paths(&report.deleted), gone.to_vec());
    let kept: Vec<String> = names
        .iter()
        .filter(|p| !gone.contains(p) && **p != changed)
        .cloned()
        .collect();
    assert_eq!(paths(&report.modified), kept);
    let changed_id = fx.id(&changed).unwrap();
    let bodies = fx.bodies();
    assert!(
        !bodies.iter().any(|b| matches!(
            b,
            EventBody::FileModified { id, .. } | EventBody::FileDeleted { id, .. }
                if *id == changed_id
        )),
        "{bodies:?}"
    );
    for (i, p) in names.iter().enumerate().filter(|(_, p)| kept.contains(p)) {
        let id = fx.id(p).unwrap();
        assert!(
            bodies.iter().any(|b| matches!(
                b,
                EventBody::FileModified { id: m, md5, prev_md5, .. }
                    if *m == id
                        && *md5 == md5_hex(&body(i))
                        && *prev_md5 == md5_hex(format!("file {i}").as_bytes())
            )),
            "{p}: {bodies:?}"
        );
    }
    // The next scan journals the changed file with the right MD5s.
    let before = fx.bodies().len();
    let report = fx.scan();
    assert_eq!(paths(&report.modified), vec![changed.clone()]);
    assert!(report.deleted.is_empty() && report.created.is_empty());
    let bodies = fx.bodies();
    assert!(
        bodies[before..].iter().any(|b| matches!(
            b,
            EventBody::FileModified { id, md5, prev_md5, .. }
                if *id == changed_id
                    && *md5 == md5_hex(&later)
                    && *prev_md5 == md5_hex(b"file 17")
        )),
        "{bodies:?}"
    );
    // The cache holds the right rows: a third scan finds nothing to do.
    let events = fx.events().len();
    assert_eq!(fx.scan(), ScanReport::default());
    assert_eq!(fx.events().len(), events);
}

// ---------------------------------------------------------------- mass delete

fn many(fx: &Fx, folder: &str, n: usize) -> Vec<String> {
    let names: Vec<String> = (0..n).map(|i| format!("{folder}/f{i:02}.txt")).collect();
    for (i, p) in names.iter().enumerate() {
        fx.write(p, format!("file {i}").as_bytes());
    }
    names
}

#[test]
fn deleting_every_file_holds_and_applies_nothing() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 5);
    fx.seed();
    for f in &files {
        fx.rm(f);
    }
    let state = fx.state_files();
    let report = fx.scan();
    let hold = report.held.clone().expect("held");
    assert_eq!((hold.gone, hold.left, hold.min, hold.at), (5, 0, 1, NOW));
    assert_eq!(hold.examples, files);
    assert!(report.seqs.is_empty());
    assert!(fx.events().is_empty());
    assert_eq!(fx.state_files(), state);
    let marker = fx.hold_marker().expect("marker");
    assert_eq!(marker["gone"], 5);
    assert_eq!(marker["left"], 0);
    assert_eq!(marker["min"], 1);
    assert_eq!(marker["at"], NOW);
    assert_eq!(fx.store.scan_hold().unwrap(), Some(hold));
    // While the files stay gone the marker is kept as it is.
    let bytes = fx.account_file(SCAN_HOLD_FILE);
    let again = fx.try_scan(&ScanOptions::all(1), NOW + 5000).unwrap();
    assert!(again.held.is_some());
    assert_eq!(fx.account_file(SCAN_HOLD_FILE), bytes);
}

#[test]
fn a_large_cleanup_that_leaves_one_file_never_holds() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 20);
    fx.seed();
    for f in &files[1..] {
        fx.rm(f);
    }
    let report = fx.scan();
    assert_eq!(report.held, None);
    assert_eq!(report.deleted.len(), 19);
    assert_eq!(fx.events().len(), 19);
    assert_eq!(fx.hold_marker(), None);
}

#[test]
fn scan_min_files_counts_the_whole_account_in_a_scoped_scan() {
    let fx = Fx::imported();
    let work = many(&fx, "Work", 3);
    many(&fx, DOC, 2);
    fx.seed();
    for f in &work {
        fx.rm(f);
    }
    let report = fx.scan_with(&ScanOptions {
        scope: wp("Work"),
        recursive: true,
        ..ScanOptions::all(1)
    });
    assert_eq!(report.held, None);
    assert_eq!(report.deleted.len(), 3);
}

#[test]
fn scan_min_files_zero_never_holds() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 5);
    fx.seed();
    for f in &files {
        fx.rm(f);
    }
    let report = fx.scan_with(&ScanOptions::all(0));
    assert_eq!(report.held, None);
    assert_eq!(fx.events().len(), 5);
}

#[test]
fn a_higher_scan_min_files_holds_earlier() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 9);
    fx.seed();
    for f in &files[..5] {
        fx.rm(f);
    }
    let report = fx.scan_with(&ScanOptions::all(5));
    let hold = report.held.expect("held");
    assert_eq!((hold.gone, hold.left, hold.min), (5, 4, 5));
    assert!(fx.events().is_empty());
    // With four left and a minimum of four it applies.
    let report = fx.scan_with(&ScanOptions::all(4));
    assert_eq!(report.held, None);
    assert!(report.released);
    assert_eq!(fx.events().len(), 5);
}

#[test]
fn an_empty_files_folder_on_a_never_imported_account_holds() {
    let fx = Fx::plain();
    many(&fx, DOC, 3);
    many(&fx, "Work", 2);
    fx.seed();
    let files = fx.store.files_dir().to_path_buf();
    std::fs::remove_dir_all(&files).unwrap();
    std::fs::create_dir(&files).unwrap();
    let state = fx.state_files();
    let report = fx.scan();
    let hold = report.held.expect("held");
    assert_eq!((hold.gone, hold.left), (5, 0));
    assert_eq!(fx.state_files(), state);
    assert!(fx.events().is_empty());
    assert!(fx.hold_marker().is_some());
    // Accepted: the files go, the native folders are never deleted by a scan.
    let report = fx.scan_with(&ScanOptions {
        accept: true,
        ..ScanOptions::all(1)
    });
    assert!(report.released);
    let bodies = fx.bodies();
    let folders: Vec<&String> = bodies
        .iter()
        .filter_map(|b| match b {
            EventBody::FolderDeleted { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(folders, vec!["Work"]);
    assert!(fx.id("NOTE/Note").is_some());
}

#[test]
fn the_hold_is_released_when_the_files_come_back() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 5);
    fx.seed();
    let aside = fx.dir.path().join("aside");
    std::fs::rename(fx.fs(DOC), &aside).unwrap();
    std::fs::create_dir(fx.fs(DOC)).unwrap();
    assert!(fx.scan().held.is_some());
    std::fs::remove_dir(fx.fs(DOC)).unwrap();
    std::fs::rename(&aside, fx.fs(DOC)).unwrap();
    let report = fx.scan();
    assert_eq!(report.held, None);
    assert!(report.released);
    assert_eq!(fx.hold_marker(), None);
    assert_eq!(fx.store.scan_hold().unwrap(), None);
    // Nothing changed: no events, the ids are the old ones.
    assert!(fx.events().is_empty());
    assert!(fx.id(&files[0]).is_some());
}

/// A scoped scan sees nothing gone in its folder, but it can't tell whether the files elsewhere
/// came back: the hold of a whole-tree scan stays.
#[test]
fn a_scoped_scan_never_releases_the_hold() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 5);
    fx.write("Work/a.txt", b"a");
    fx.seed();
    fx.rm("Work/a.txt");
    for f in &files {
        fx.rm(f);
    }
    assert!(fx.scan().held.is_some());
    let bytes = fx.account_file(SCAN_HOLD_FILE);
    let hold = fx.store.scan_hold().unwrap();
    let state = fx.state_files();
    fx.write("INBOX/new.txt", b"new");
    let report = fx.scan_with(&ScanOptions {
        scope: wp("INBOX"),
        recursive: true,
        ..ScanOptions::all(1)
    });
    // While the hold exists a scoped scan applies nothing: it only reports its plan.
    assert!(!report.released);
    assert_eq!(report.held, hold);
    assert_eq!(paths(&report.created), vec!["INBOX/new.txt"]);
    assert!(report.seqs.is_empty());
    assert!(fx.events().is_empty());
    assert_eq!(fx.state_files(), state);
    assert_eq!(fx.id("INBOX/new.txt"), None);
    assert_eq!(fx.account_file(SCAN_HOLD_FILE), bytes);
    // The whole-tree scan with accept applies everything and removes the hold.
    let report = fx.scan_with(&ScanOptions {
        accept: true,
        ..ScanOptions::all(1)
    });
    assert!(report.released);
    assert!(fx.id("INBOX/new.txt").is_some());
    assert_eq!(fx.hold_marker(), None);
}

#[test]
fn accept_applies_the_mass_delete_and_removes_the_hold() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 5);
    fx.seed();
    for f in &files {
        fx.rm(f);
    }
    assert!(fx.scan().held.is_some());
    let report = fx.scan_with(&ScanOptions {
        accept: true,
        ..ScanOptions::all(1)
    });
    assert_eq!(report.held, None);
    assert!(report.released);
    assert_eq!(report.deleted.len(), 5);
    assert_eq!(fx.events().len(), 5);
    assert_eq!(fx.hold_marker(), None);
    // Without a hold, accept is a normal scan.
    fx.write(&format!("{DOC}/new.txt"), b"new");
    let report = fx.scan_with(&ScanOptions {
        accept: true,
        ..ScanOptions::all(1)
    });
    assert!(!report.released);
    assert_eq!(report.created.len(), 1);
}

/// A scoped scan that holds keeps nothing of what it hashed: the next scan still knows the MD5
/// before a change by hand and journals it.
#[test]
fn a_held_scoped_scan_keeps_the_old_md5_for_the_next_scan() {
    let fx = Fx::imported();
    let files = many(&fx, DOC, 3);
    fx.write("INBOX/x.txt", b"one");
    fx.seed();
    let id = fx.id("INBOX/x.txt").unwrap();
    for f in &files {
        fx.rm(f);
    }
    assert!(fx.scan_with(&ScanOptions::all(2)).held.is_some());
    let state = fx.state_files();
    fx.write("INBOX/x.txt", b"two, longer");
    let report = fx.scan_with(&ScanOptions {
        scope: wp("INBOX"),
        recursive: false,
        ..ScanOptions::all(2)
    });
    assert!(report.held.is_some());
    assert_eq!(paths(&report.modified), vec!["INBOX/x.txt"]);
    assert!(fx.events().is_empty());
    assert_eq!(fx.state_files(), state);
    {
        let g = fx.store.lock().unwrap();
        assert_eq!(
            fx.store.cached_md5(&g, &wp("INBOX/x.txt")).unwrap(),
            Some(md5_hex(b"one"))
        );
    }
    let report = fx.scan_with(&ScanOptions {
        accept: true,
        ..ScanOptions::all(2)
    });
    assert_eq!(paths(&report.modified), vec!["INBOX/x.txt"]);
    assert_eq!(report.deleted.len(), 3);
    let bodies = fx.bodies();
    assert!(
        bodies.iter().any(|b| matches!(
            b,
            EventBody::FileModified { id: i, md5, prev_md5, .. }
                if *i == id && *md5 == md5_hex(b"two, longer") && *prev_md5 == md5_hex(b"one")
        )),
        "{bodies:?}"
    );
}

/// The hold of a scoped scan counts every file of the account that would be left: the files
/// outside the scope, also the ones below a gone child folder that a scan without recursion
/// doesn't walk.
#[test]
fn a_scoped_scan_counts_the_files_left_in_the_whole_account() {
    for recursive in [true, false] {
        for other in [true, false] {
            let fx = Fx::imported();
            fx.write("Work/a.txt", b"a");
            many(&fx, "Work/Sub", 3);
            if other {
                fx.write("Other/keep.txt", b"keep");
            }
            fx.seed();
            fx.rm("Work/a.txt");
            std::fs::remove_dir_all(fx.fs("Work/Sub")).unwrap();
            let left = usize::from(other);
            let opts = ScanOptions {
                scope: wp("Work"),
                recursive,
                min_files: left as u64 + 1,
                ..ScanOptions::all(1)
            };
            let case = format!("recursive {recursive}, other {other}");
            // One file fewer to keep: no hold.
            let dry = fx.scan_with(&ScanOptions {
                dry_run: true,
                min_files: left as u64,
                ..opts.clone()
            });
            assert_eq!(dry.held, None, "{case}");
            assert_eq!(dry.deleted.len(), 5, "{case}");
            let state = fx.state_files();
            let report = fx.scan_with(&opts);
            let hold = report.held.expect("held");
            assert_eq!(
                (hold.gone, hold.left, hold.min),
                (4, left, left as u64 + 1),
                "{case}"
            );
            assert_eq!(
                paths(&report.deleted),
                vec![
                    "Work/Sub/f00.txt",
                    "Work/Sub/f01.txt",
                    "Work/Sub/f02.txt",
                    "Work/a.txt",
                    "Work/Sub"
                ],
                "{case}"
            );
            assert!(fx.events().is_empty(), "{case}");
            assert_eq!(fx.state_files(), state, "{case}");
        }
    }
}

// ---------------------------------------------------------------- safety

#[test]
fn the_registry_is_never_ahead_of_the_journal() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"one");
    fx.seed();
    fx.write(&path, b"three");
    fx.write(&format!("{DOC}/new.txt"), b"new");
    let state = fx.state_files();
    // A folder where the journal lock file belongs: every append fails.
    let g = fx.store.lock_recovered(&fx.journal).unwrap();
    let lock = fx.journal.dir().join(".lock");
    let _ = std::fs::remove_file(&lock);
    std::fs::create_dir(&lock).unwrap();
    let r = fx.store.scan(&g, &fx.journal, &ScanOptions::all(1), NOW);
    assert!(r.is_err(), "{r:?}");
    // Only the reserve of the new id reached the registry log: no row changed.
    let mut now = fx.state_files();
    let log = String::from_utf8(now[1].take().unwrap()).unwrap();
    let old = String::from_utf8(state[1].clone().unwrap()).unwrap();
    let added = log.strip_prefix(old.as_str()).expect("appended only");
    assert!(
        added.lines().all(|l| l.starts_with(r#"{"op":"reserve""#)),
        "{added}"
    );
    assert_eq!(now[0], state[0]);
    assert_eq!(now[2], state[2]);
    assert_eq!(fx.id(&format!("{DOC}/new.txt")), None);
    drop(g);
    std::fs::remove_dir(&lock).unwrap();
    // The same store object scans again: the change is still found.
    fx.scan();
    assert_eq!(
        kind_names(&fx.bodies()),
        vec!["file.created", "file.modified"]
    );
}

/// A crash after the journal append and before the registry append: the ids the journal used
/// are reserved, so nothing else gets them after a restart.
#[test]
fn ids_journaled_before_a_crash_are_never_handed_out_again() {
    let fx = Fx::imported();
    fx.write("Work/a.txt", b"a");
    fx.write("Work/b.txt", b"bb");
    {
        let g = fx.store.lock_recovered(&fx.journal).unwrap();
        fx.store
            .scan_until(&g, &fx.journal, &ScanOptions::all(1), NOW, true)
            .unwrap();
    }
    let journaled = fx
        .bodies()
        .iter()
        .filter_map(|b| match b {
            EventBody::FileCreated { id, .. } | EventBody::FolderCreated { id, .. } => Some(*id),
            _ => None,
        })
        .max()
        .expect("journaled");
    // A restart, then a write that mints an id.
    let store = Store::open(fx.dir.path(), "jordi").unwrap();
    assert_eq!(store.id_of(&wp("Work/a.txt")).unwrap(), None);
    let g = store.lock_recovered(&fx.journal).unwrap();
    let id = store.assign(&g, &wp("INBOX/x.txt"), Kind::File).unwrap();
    assert!(id > journaled, "{id} <= {journaled}");
}

/// A scan that mints no ids reserves none.
#[test]
fn a_scan_without_new_entries_writes_no_registry_line() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"one");
    fx.seed();
    let log = fx.account_file("ids.jsonl");
    fx.write(&path, b"three");
    fx.scan();
    assert_eq!(fx.account_file("ids.jsonl"), log);
}

/// A gone file and a new file with the same MD5 but another size than the cached row are not
/// paired (S4 asks for both).
#[test]
fn a_move_needs_the_cached_size_too() {
    let fx = Fx::imported();
    let from = format!("{DOC}/a.txt");
    fx.write(&from, b"content");
    fx.write(&format!("{DOC}/keep.txt"), b"keep");
    fx.seed();
    // Plant another size in the cached row of the file.
    let hashes = fx.store.account_dir().join("hashes.json");
    let mut rows: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&hashes).unwrap()).unwrap();
    rows[&from]["size"] = 99.into();
    std::fs::write(&hashes, serde_json::to_vec(&rows).unwrap()).unwrap();
    let store = Store::open(fx.dir.path(), "jordi").unwrap();
    std::fs::rename(fx.fs(&from), fx.fs(&format!("{DOC}/b.txt"))).unwrap();
    let g = store.lock_recovered(&fx.journal).unwrap();
    store
        .scan(&g, &fx.journal, &ScanOptions::all(1), NOW)
        .unwrap();
    assert_eq!(
        kind_names(&fx.bodies()),
        vec!["file.created", "file.deleted"]
    );
}

/// A `put` that replaces a file leaves no stale MD5 behind: the next scan finds nothing.
#[test]
fn a_put_that_replaces_a_file_is_not_journaled_again_by_the_scan() {
    use notedown_cloud::put::{PutOutcome, put};
    let fx = Fx::imported();
    let src = fx.dir.path().join("src.txt");
    std::fs::write(&src, b"one").unwrap();
    put(fx.dir.path(), "jordi", &fx.journal, &src, "INBOX", None).unwrap();
    std::fs::write(&src, b"two, longer").unwrap();
    let r = put(fx.dir.path(), "jordi", &fx.journal, &src, "INBOX", None).unwrap();
    assert_eq!(r.outcome, PutOutcome::Replaced);
    let before = fx.events().len();
    assert_eq!(fx.scan(), ScanReport::default());
    assert_eq!(fx.events().len(), before);
}

/// A put keeps the cache row of the file it wrote, with the ctime of the target, so a hand edit
/// right after it (same size, mtime set back) is journaled by the next scan.
#[test]
fn a_hand_edit_right_after_a_put_is_journaled() {
    use notedown_cloud::put::put;
    let fx = Fx::imported();
    let src = fx.dir.path().join("src.txt");
    std::fs::write(&src, b"one").unwrap();
    put(fx.dir.path(), "jordi", &fx.journal, &src, "INBOX", None).unwrap();
    let path = "INBOX/src.txt";
    let id = fx.id(path).unwrap();
    let target = fx.fs(path);
    let old_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
    std::fs::write(&target, b"two").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();
    {
        let g = fx.store.lock().unwrap();
        assert_eq!(
            fx.store.cached_md5(&g, &wp(path)).unwrap(),
            Some(md5_hex(b"one"))
        );
    }
    let before = fx.events().len();
    let report = fx.scan();
    assert_eq!(paths(&report.modified), vec![path.to_string()]);
    let events = fx.events();
    assert_eq!(events.len(), before + 1);
    let last = events.last().unwrap();
    assert_eq!(last.origin, Origin::Scan);
    assert_eq!(
        last.body,
        EventBody::FileModified {
            id,
            path: path.into(),
            kind: FileKind::Document,
            md5: md5_hex(b"two"),
            prev_md5: md5_hex(b"one"),
            size: 3,
            pages_changed: None,
        }
    );
}

/// A put in the server's way keeps the row in memory and doesn't rewrite `hashes.json`.
#[test]
fn a_put_of_a_new_file_keeps_its_row_without_rewriting_the_cache() {
    let fx = Fx::imported();
    fx.write(&format!("{DOC}/a.txt"), b"a");
    fx.seed();
    let stamp = || {
        let m = std::fs::metadata(fx.store.account_dir().join("hashes.json")).unwrap();
        (m.modified().unwrap(), m.len(), ctime(&m))
    };
    let before = stamp();
    let staging = fx.store.account_dir().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let staged = staging.join("f");
    std::fs::write(&staged, b"new file").unwrap();
    let path = format!("{DOC}/b.txt");
    let g = fx.store.lock_recovered(&fx.journal).unwrap();
    let c = fx
        .store
        .plan_put(&g, &wp(&path), &staged, Origin::Device, None)
        .unwrap();
    fx.store.commit(&g, c, &fx.journal).unwrap();
    assert_eq!(
        fx.store.cached_md5(&g, &wp(&path)).unwrap(),
        Some(md5_hex(b"new file"))
    );
    drop(g);
    assert_eq!(stamp(), before);
    // The next flush point writes the row.
    let g = fx.store.lock_recovered(&fx.journal).unwrap();
    fx.store.flush_cache(&g).unwrap();
    let rows: serde_json::Value =
        serde_json::from_slice(&fx.account_file("hashes.json").unwrap()).unwrap();
    assert_eq!(rows[&path]["md5"], md5_hex(b"new file"));
}

/// A put with `drop_md5` (the import's) leaves no row for the file, and a scan hashes it
/// silently.
#[test]
fn a_put_with_drop_md5_leaves_no_row() {
    use notedown_cloud::store::PutOptions;
    let fx = Fx::imported();
    let staging = fx.store.account_dir().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let staged = staging.join("f");
    std::fs::write(&staged, b"imported").unwrap();
    let path = format!("{DOC}/c.txt");
    let g = fx.store.lock_recovered(&fx.journal).unwrap();
    let c = fx
        .store
        .plan_put_with(
            &g,
            &wp(&path),
            &staged,
            Origin::Cli,
            None,
            PutOptions {
                drop_md5: true,
                ..PutOptions::default()
            },
        )
        .unwrap();
    fx.store.commit(&g, c, &fx.journal).unwrap();
    assert_eq!(fx.store.cached_md5(&g, &wp(&path)).unwrap(), None);
    drop(g);
    let before = fx.events().len();
    assert_eq!(fx.scan(), ScanReport::default());
    assert_eq!(fx.events().len(), before);
}

#[cfg(unix)]
fn ctime(m: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    m.ctime_nsec() + m.ctime() * 1_000_000_000
}

#[cfg(not(unix))]
fn ctime(_: &std::fs::Metadata) -> i64 {
    0
}

/// A put that a crash cut after the rename leaves no cache row: recovery can't prove the file
/// is still what the put wrote, so a scan hashes it silently.
#[test]
fn a_recovered_put_drops_the_row() {
    use notedown_cloud::store::CrashPoint;
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"old");
    fx.seed();
    let staging = fx.store.account_dir().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let staged = staging.join("f");
    std::fs::write(&staged, b"new").unwrap();
    {
        let g = fx.store.lock_recovered(&fx.journal).unwrap();
        let c = fx
            .store
            .plan_put(&g, &wp(&path), &staged, Origin::Device, None)
            .unwrap();
        fx.store
            .commit_until(&g, c, &fx.journal, Some(CrashPoint::BeforeAppend))
            .unwrap();
    }
    // The next lock recovers the put.
    drop(fx.store.lock_recovered(&fx.journal).unwrap());
    let cache = String::from_utf8(fx.account_file("hashes.json").unwrap()).unwrap();
    let rows: serde_json::Value = serde_json::from_str(&cache).unwrap();
    assert!(rows.get(&path).is_none(), "{cache}");
    let before = fx.events().len();
    assert_eq!(fx.scan(), ScanReport::default());
    assert_eq!(fx.events().len(), before);
    let cache = String::from_utf8(fx.account_file("hashes.json").unwrap()).unwrap();
    assert!(cache.contains(&md5_hex(b"new")));
}

/// `put` scans the folders on its way first, as a device upload does: a folder and a file made
/// by hand there are journaled by the scan before the put's own event.
#[test]
fn a_put_journals_the_hand_changes_on_its_way_first() {
    let fx = Fx::imported();
    fx.write("INBOX/A/hand.txt", b"hand");
    let src = fx.dir.path().join("src.txt");
    std::fs::write(&src, b"src").unwrap();
    notedown_cloud::put::put(fx.dir.path(), "jordi", &fx.journal, &src, "INBOX/A", None).unwrap();
    let got: Vec<(&str, Origin)> = fx
        .events()
        .iter()
        .map(|e| (kind_names(std::slice::from_ref(&e.body))[0], e.origin))
        .collect();
    assert_eq!(
        got,
        vec![
            ("folder.created", Origin::Scan),
            ("file.created", Origin::Scan),
            ("file.created", Origin::Cli),
        ]
    );
    // Nothing is left for the next scan.
    assert_eq!(fx.scan(), ScanReport::default());
}

/// While a scan holds the account, `put` refuses and writes nothing.
#[test]
fn a_put_refuses_while_the_account_is_held() {
    let fx = Fx::imported();
    let path = format!("{DOC}/a.txt");
    fx.write(&path, b"a");
    fx.seed();
    fx.rm(&path);
    assert!(fx.scan().held.is_some());
    let src = fx.dir.path().join("src.txt");
    std::fs::write(&src, b"src").unwrap();
    let r = notedown_cloud::put::put(fx.dir.path(), "jordi", &fx.journal, &src, "INBOX", None);
    assert!(matches!(r, Err(Error::ScanHold(_))), "{r:?}");
    assert!(!fx.fs("INBOX/src.txt").exists());
    assert!(fx.events().is_empty(), "{:?}", fx.events());
}

#[test]
fn hidden_and_temp_files_are_never_seen() {
    let fx = Fx::imported();
    fx.write(".hidden", b"x");
    fx.write(&format!("{DOC}/.nd-1-2.tmp"), b"x");
    fx.write(&format!("{DOC}/.DS_Store"), b"x");
    fx.write(".Trash/a.txt", b"x");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", fx.fs("link")).unwrap();
    let report = fx.scan();
    assert_eq!(report, ScanReport::default());
    assert!(fx.events().is_empty());
}

#[test]
fn a_broken_tree_is_never_scanned() {
    let fx = Fx::imported();
    fx.write(&format!("{DOC}/a.txt"), b"a");
    fx.seed();
    let g = fx.store.lock_recovered(&fx.journal).unwrap();
    std::fs::remove_dir_all(fx.fs("DOCUMENT")).unwrap();
    let r = fx.store.scan(&g, &fx.journal, &ScanOptions::all(0), NOW);
    assert!(matches!(r, Err(Error::StoreDamaged { .. })), "{r:?}");
    assert!(fx.events().is_empty());
}

// ---------------------------------------------------------------- the scan in the server

/// The scan as the server runs it: at `synchronous/start`, before a device listing and on the
/// timer. Requests go through the router in memory (`tower::oneshot`).
mod served {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
    use notedown_cloud::journal::{Event, EventBody, Origin};
    use notedown_cloud::push::{follow_once, start_seq, tick};
    use notedown_cloud::server::{
        AppState, ScanOutcome, router, scan_hold_warning, serve_listeners,
    };
    use notedown_cloud::socket::Outbound;
    use notedown_cloud::store::{SCAN_HOLD_FILE, ScanHold, Store};
    use notedown_cloud::{Clock, Config, PushMode};
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use tower::ServiceExt;

    use super::{DOC, EMAIL, kind_names};

    const SERIAL: &str = "SN078C10000000";
    const T0: i64 = 1_791_270_000_000;
    const START: &str = "/api/file/2/files/synchronous/start";
    const END: &str = "/api/file/2/files/synchronous/end";
    const LIST: &str = "/api/file/2/files/list_folder";
    const LIST_V3: &str = "/api/file/3/files/list_folder_v3";
    const QUERY: &str = "/api/file/3/files/query_v3";
    const BY_PATH: &str = "/api/file/3/files/query/by/path_v3";
    const DOWNLOAD: &str = "/api/file/3/files/download_v3";
    const CREATE: &str = "/api/file/2/files/create_folder_v2";
    const APPLY: &str = "/api/file/3/files/upload/apply";
    const DELETE: &str = "/api/file/3/files/delete_folder_v3";
    const MOVE: &str = "/api/file/3/files/move_v3";
    const COPY: &str = "/api/file/3/files/copy_v3";
    const CAPACITY: &str = "/api/file/capacity/query";
    const DIGEST_GROUPS: &str = "/api/file/query/summary/group";
    const TODO_GROUPS: &str = "/api/file/schedule/group/all";

    struct TestClock(AtomicI64);

    impl Clock for TestClock {
        fn now(&self) -> OffsetDateTime {
            OffsetDateTime::from_unix_timestamp_nanos(
                self.0.load(Ordering::SeqCst) as i128 * 1_000_000,
            )
            .unwrap()
        }
    }

    /// The account as after an import (native folders, `imported` marker).
    fn seed_imported(data: &std::path::Path) {
        Accounts::new(data)
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let store = Store::open(data, "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let accounts = Accounts::new(data);
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 0,
            bytes: 0,
        });
        accounts.save(&a).unwrap();
    }

    struct Server {
        dir: tempfile::TempDir,
        state: Arc<AppState>,
        clock: Arc<TestClock>,
        token: String,
    }

    impl Server {
        /// Protect off (no app data gate for the digest and to-do checks).
        fn new() -> Server {
            Self::with(Config {
                protect: false,
                ..Config::default()
            })
        }

        fn with(config: Config) -> Server {
            let dir = tempfile::tempdir().unwrap();
            seed_imported(dir.path());
            let clock = Arc::new(TestClock(AtomicI64::new(T0)));
            let state = AppState::open_with_clock(config, dir.path(), clock.clone()).unwrap();
            let now = state.now_ms();
            let (code, _) = state.auth.challenge(EMAIL, now).unwrap();
            let hash = expected_login_hash(&md5_hex("secret"), &code);
            let token = state.auth.login(EMAIL, &hash, SERIAL, now).unwrap().token;
            state
                .auth
                .devices()
                .update("jordi", SERIAL, |d| d.bound = true)
                .unwrap();
            Server {
                dir,
                state,
                clock,
                token,
            }
        }

        fn fs(&self, rel: &str) -> std::path::PathBuf {
            self.dir.path().join("accounts/jordi/files").join(rel)
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let p = self.fs(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }

        fn rm(&self, rel: &str) {
            std::fs::remove_file(self.fs(rel)).unwrap();
        }

        fn now(&self) -> u64 {
            self.clock.0.load(Ordering::SeqCst) as u64
        }

        fn advance_ms(&self, ms: i64) {
            self.clock.0.fetch_add(ms, Ordering::SeqCst);
        }

        async fn post(&self, path: &str, body: Value) -> Value {
            self.post_as(&self.token, path, body).await
        }

        /// A login token for another device `serial`.
        fn token_of(&self, serial: &str) -> String {
            let now = self.state.now_ms();
            let (code, _) = self.state.auth.challenge(EMAIL, now).unwrap();
            let hash = expected_login_hash(&md5_hex("secret"), &code);
            self.state
                .auth
                .login(EMAIL, &hash, serial, now)
                .unwrap()
                .token
        }

        async fn post_as(&self, token: &str, path: &str, body: Value) -> Value {
            let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
            let req = Request::builder()
                .method("POST")
                .uri(path)
                .header("x-access-token", token)
                .header("content-type", "application/json")
                .header("host", "192.168.1.10:19072")
                .body(Body::from(body.to_string()))
                .unwrap();
            let resp = router(self.state.clone())
                .layer(MockConnectInfo(addr))
                .oneshot(req)
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        async fn start(&self) -> Value {
            self.post(START, json!({"equipmentNo": SERIAL})).await
        }

        async fn end(&self) -> Value {
            self.post(END, json!({"equipmentNo": SERIAL, "flag": "Y"}))
                .await
        }

        async fn list(&self, path: &str, recursive: bool) -> Value {
            self.post(LIST, json!({"path": path, "recursive": recursive}))
                .await
        }

        fn events(&self) -> Vec<Event> {
            self.state
                .journal
                .read_from(1)
                .unwrap()
                .map(|e| e.unwrap())
                .collect()
        }

        fn kinds(&self) -> Vec<&'static str> {
            let bodies: Vec<EventBody> = self.events().into_iter().map(|e| e.body).collect();
            kind_names(&bodies)
        }

        fn holder(&self) -> Option<String> {
            self.state.sync_locks.holder("jordi", self.now())
        }

        fn marker(&self) -> std::path::PathBuf {
            self.dir.path().join("accounts/jordi").join(SCAN_HOLD_FILE)
        }

        /// The flags of the last logged request to `path`.
        fn last_flags(&self, path: &str) -> Vec<String> {
            let mut files: Vec<_> = std::fs::read_dir(self.dir.path().join("logs"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            files.sort();
            let entry = files
                .iter()
                .flat_map(|f| {
                    std::fs::read_to_string(f)
                        .unwrap()
                        .lines()
                        .map(|l| serde_json::from_str::<Value>(l).unwrap())
                        .collect::<Vec<_>>()
                })
                .rfind(|e| e["kind"] == "request" && e["path"] == path)
                .unwrap_or_else(|| panic!("no request to {path}"));
            entry["flags"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        }

        /// Two files listed by the device: both have ids and `file.created` events.
        async fn two_files(&self) -> (String, String) {
            let (a, b) = (format!("{DOC}/a.txt"), format!("{DOC}/b.txt"));
            self.write(&a, b"a");
            self.write(&b, b"b");
            let v = self.list("/", true).await;
            assert_eq!(v["success"], true, "{v}");
            (a, b)
        }
    }

    fn assert_ok(v: &Value) {
        assert_eq!(v["success"], true, "{v}");
    }

    fn assert_scan_hold(s: &Server, path: &str, v: &Value) {
        assert_eq!(v["success"], false, "{path}: {v}");
        assert_eq!(v["errorCode"], "E0079", "{path}: {v}");
        assert!(v.get("entries").is_none(), "{path}: {v}");
        assert!(
            s.last_flags(path).contains(&"scan_hold".to_string()),
            "{path}: {:?}",
            s.last_flags(path)
        );
    }

    #[tokio::test]
    async fn a_hand_added_file_is_journaled_before_the_device_lists_it() {
        let s = Server::new();
        let path = format!("{DOC}/a.pdf");
        s.write(&path, b"hello");
        let v = s.list(&format!("/{DOC}"), false).await;
        assert_ok(&v);
        let events = s.events();
        assert_eq!(events.len(), 1, "{events:?}");
        let e = &events[0];
        assert_eq!(e.origin, Origin::Scan);
        let EventBody::FileCreated { id, path: p, .. } = &e.body else {
            panic!("{e:?}");
        };
        assert_eq!(p, &path);
        let entries = v["entries"].as_array().unwrap();
        let listed = entries
            .iter()
            .find(|x| x["path_display"] == path.as_str())
            .unwrap_or_else(|| panic!("{v}"));
        assert_eq!(listed["id"], id.to_string());
        // The next listing finds nothing new.
        assert_ok(&s.list("/", true).await);
        assert_eq!(s.events().len(), 1);
    }

    #[tokio::test]
    async fn the_v3_listing_and_the_queries_journal_first_too() {
        let s = Server::new();
        s.write(&format!("{DOC}/a.txt"), b"a");
        assert_ok(&s.post(LIST_V3, json!({"id": "0", "recursive": true})).await);
        assert_eq!(s.kinds(), vec!["file.created"]);
        s.write(&format!("{DOC}/b.txt"), b"b");
        let v = s
            .post(BY_PATH, json!({"path": format!("/{DOC}/b.txt")}))
            .await;
        assert_ok(&v);
        assert_eq!(s.kinds(), vec!["file.created", "file.created"]);
        let EventBody::FileCreated { id, .. } = &s.events()[1].body else {
            panic!();
        };
        assert_eq!(v["entriesVO"]["id"], id.to_string());
        // A file removed by hand is journaled before query_v3 answers null.
        let id = *id;
        s.rm(&format!("{DOC}/b.txt"));
        let v = s.post(QUERY, json!({"id": id.to_string()})).await;
        assert_ok(&v);
        assert!(v["entriesVO"].is_null(), "{v}");
        assert_eq!(
            s.kinds(),
            vec!["file.created", "file.created", "file.deleted"]
        );
    }

    #[tokio::test]
    async fn sync_start_journals_hand_deletes_and_grants_the_lock() {
        let s = Server::new();
        let (a, _) = s.two_files().await;
        s.rm(&a);
        assert_ok(&s.start().await);
        assert_eq!(s.holder().as_deref(), Some(SERIAL));
        let last = s.events().pop().unwrap();
        assert_eq!(last.origin, Origin::Scan);
        assert!(
            matches!(&last.body, EventBody::FileDeleted { path, .. } if *path == a),
            "{last:?}"
        );
    }

    #[tokio::test]
    async fn sync_start_answers_scan_hold_after_a_mass_delete() {
        let s = Server::new();
        let (a, b) = s.two_files().await;
        let id = s.events()[0].body.clone();
        let EventBody::FileCreated { id, .. } = id else {
            panic!();
        };
        let before = s.events().len();
        s.rm(&a);
        s.rm(&b);
        let v = s.start().await;
        assert_scan_hold(&s, START, &v);
        assert_eq!(s.holder(), None);
        assert!(s.marker().is_file());
        assert_eq!(s.events().len(), before, "nothing journaled");
        let calls = [
            (LIST, json!({"path": "/", "recursive": true})),
            (LIST, json!({"path": format!("/{DOC}"), "recursive": false})),
            (LIST_V3, json!({"id": "0", "recursive": true})),
            (QUERY, json!({"id": id.to_string()})),
            (BY_PATH, json!({"path": format!("/{a}")})),
            (DOWNLOAD, json!({"id": id.to_string()})),
            (CREATE, json!({"path": "/INBOX/New", "autorename": false})),
            (
                APPLY,
                json!({"path": "/INBOX/x.pdf", "fileName": "x.pdf", "size": 1}),
            ),
            (DELETE, json!({"id": id.to_string()})),
            (
                MOVE,
                json!({"id": id.to_string(), "to_path": "/INBOX/a.txt", "autorename": false}),
            ),
            (
                COPY,
                json!({"id": id.to_string(), "to_path": "/INBOX/a.txt", "autorename": false}),
            ),
        ];
        for (path, body) in calls {
            let v = s.post(path, body).await;
            assert_scan_hold(&s, path, &v);
        }
        assert_eq!(s.events().len(), before, "nothing journaled");
        // Digests and to-dos stay open.
        for (path, body) in [
            (DIGEST_GROUPS, json!({"page": 0, "size": 0})),
            (TODO_GROUPS, json!({"maxResults": "20"})),
        ] {
            let v = s.post(path, body).await;
            assert_ok(&v);
            assert!(!s.last_flags(path).contains(&"scan_hold".to_string()));
        }
    }

    #[tokio::test]
    async fn the_hold_ends_when_the_files_come_back() {
        let s = Server::new();
        let (a, b) = s.two_files().await;
        let before = s.events().len();
        s.rm(&a);
        s.rm(&b);
        assert_scan_hold(&s, START, &s.start().await);
        s.write(&a, b"a");
        s.write(&b, b"b");
        assert_ok(&s.start().await);
        assert!(!s.marker().exists());
        assert_eq!(s.holder().as_deref(), Some(SERIAL));
        assert_eq!(s.events().len(), before, "the files came back unchanged");
        assert_ok(&s.list("/", true).await);
    }

    #[tokio::test]
    async fn an_unreadable_hold_marker_closes_the_file_routes() {
        let s = Server::new();
        std::fs::write(s.marker(), b"{not json").unwrap();
        let v = s.list("/", true).await;
        assert_scan_hold(&s, LIST, &v);
    }

    #[tokio::test]
    async fn the_timer_scan_skips_while_a_device_syncs() {
        let s = Server::new();
        assert_ok(&s.start().await);
        s.write(&format!("{DOC}/a.txt"), b"a");
        s.state.scan_all();
        assert!(s.events().is_empty(), "{:?}", s.events());
        assert_ok(&s.end().await);
        s.state.scan_all();
        assert_eq!(s.kinds(), vec!["file.created"]);
        assert_eq!(s.events()[0].origin, Origin::Scan);
    }

    #[test]
    fn the_timer_scan_holds_after_a_mass_delete() {
        let s = Server::new();
        let (a, b) = (format!("{DOC}/a.txt"), format!("{DOC}/b.txt"));
        s.write(&a, b"a");
        s.write(&b, b"b");
        s.state.scan_all();
        assert_eq!(s.kinds(), vec!["file.created", "file.created"]);
        s.rm(&a);
        s.rm(&b);
        s.state.scan_all();
        assert!(s.marker().is_file());
        assert_eq!(s.events().len(), 2);
    }

    /// A data folder with a socket session of the bound device and the follower cursor.
    struct Pushed {
        s: Server,
        cursor: u64,
        _rx: tokio::sync::mpsc::Receiver<Outbound>,
        inbound: notedown_cloud::socket::LastInbound,
    }

    impl Pushed {
        fn new() -> Pushed {
            let s = Server::with(Config {
                protect: false,
                ..Config::default()
            });
            assert_eq!(s.state.config.push, PushMode::Detail);
            let cursor = start_seq(&s.state).unwrap();
            let (rx, inbound) = s.state.sockets.register("jordi", SERIAL, "sid", s.now());
            Pushed {
                s,
                cursor,
                _rx: rx,
                inbound,
            }
        }

        /// Follow the journal and tick 2 s later; the pushes handed over.
        fn follow_and_tick(&mut self) -> usize {
            follow_once(&self.s.state, &mut self.cursor).unwrap();
            self.s.advance_ms(2000);
            let now = self.s.now();
            self.inbound.store(now, Ordering::SeqCst);
            tick(&self.s.state, now)
        }

        /// A heartbeat of the device now: `(messageType, originalName)` of each item of the
        /// push it gets, sorted; empty when it gets none.
        fn pushed(&mut self) -> Vec<(String, String)> {
            let now = self.s.now();
            self.inbound.store(now, Ordering::SeqCst);
            let Some(frame) = self.s.state.sockets.deliver("jordi", SERIAL, "sid", now) else {
                return Vec::new();
            };
            let v: Value = serde_json::from_str(frame.strip_prefix("42").unwrap()).unwrap();
            let m: Value = serde_json::from_str(v[1].as_str().unwrap()).unwrap();
            let mut items: Vec<(String, String)> = m["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    (
                        i["messageType"].as_str().unwrap().to_string(),
                        i["originalName"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            items.sort();
            items
        }
    }

    #[test]
    fn a_scan_event_is_pushed() {
        let mut p = Pushed::new();
        p.s.write(&format!("{DOC}/a.txt"), b"a");
        p.s.state.scan_all();
        assert_eq!(p.follow_and_tick(), 1);
        assert_eq!(p.pushed(), [download("a.txt")]);
    }

    fn download(name: &str) -> (String, String) {
        ("DOWNLOADFILE".to_string(), name.to_string())
    }

    #[test]
    fn no_push_while_a_hold_exists() {
        let mut p = Pushed::new();
        let (a, b) = (format!("{DOC}/a.txt"), format!("{DOC}/b.txt"));
        p.s.write(&a, b"a");
        p.s.write(&b, b"b");
        p.s.state.scan_all();
        p.s.rm(&a);
        p.s.rm(&b);
        p.s.state.scan_all();
        assert!(p.s.marker().is_file());
        // The created events wait for the push gate.
        assert_eq!(p.follow_and_tick(), 0);
        assert!(p.pushed().is_empty());
        // The files come back: the hold ends and the waiting push goes out.
        p.s.write(&a, b"a");
        p.s.write(&b, b"b");
        p.s.state.scan_all();
        assert!(!p.s.marker().exists());
        assert_eq!(p.follow_and_tick(), 1);
        assert_eq!(p.pushed(), [download("a.txt"), download("b.txt")]);
    }

    #[test]
    fn scan_interval_zero_turns_the_timer_off() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::default();
        assert_eq!(c.scan_interval, 300);
        assert_eq!(c.scan_min_files, 1);
        assert_eq!(c.scan_period(), Some(std::time::Duration::from_secs(300)));
        std::fs::write(Config::path(d.path()), "scan_interval = 0\n").unwrap();
        let c = Config::load_or_create(d.path()).unwrap();
        assert_eq!(c.scan_interval, 0);
        assert_eq!(c.scan_period(), None);
    }

    #[test]
    fn the_hold_warning_names_the_account_and_the_way_out() {
        let hold = ScanHold {
            at: 0,
            gone: 412,
            left: 0,
            min: 1,
            examples: Vec::new(),
        };
        let data = std::path::Path::new("/srv/notedown");
        assert_eq!(
            scan_hold_warning(data, "jordi", &hold),
            "account jordi: 412 files are gone from the data folder and only 0 would be left. \
             Device sync is on hold. If that's on purpose, run: \
             notedown cloud scan --data /srv/notedown --account jordi --accept"
        );
        let one = ScanHold { gone: 1, ..hold };
        assert!(
            scan_hold_warning(data, "jordi", &one)
                .starts_with("account jordi: 1 file is gone from the data folder"),
            "{}",
            scan_hold_warning(data, "jordi", &one)
        );
        // A folder name with a space is quoted, so the command can be copied as it is.
        assert!(
            scan_hold_warning(
                std::path::Path::new("/Volumes/My Disk/notedown"),
                "jordi",
                &one
            )
            .ends_with("--data '/Volumes/My Disk/notedown' --account jordi --accept")
        );
    }

    #[tokio::test]
    async fn the_server_scans_at_start() {
        let s = Server::new();
        s.write(&format!("{DOC}/a.txt"), b"a");
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = tokio::spawn(serve_listeners(vec![l], s.state.clone()));
        let mut found = false;
        for _ in 0..100 {
            if !s.events().is_empty() {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        server.abort();
        assert!(found, "no scan at start");
        assert_eq!(s.kinds(), vec!["file.created"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_added_by_hand_is_journaled_by_the_watch() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        seed_imported(dir.path());
        let config = Config {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            scan_interval: 0,
            watch: true,
            ..Config::default()
        };
        let state = AppState::open(config, dir.path()).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(notedown_cloud::server::serve_until(
            vec![l],
            state.clone(),
            async {
                let _ = rx.await;
            },
        ));
        let file = dir.path().join("accounts/jordi/files/NOTE/Note/hand.pdf");
        let journal = notedown_cloud::journal::Journal::open_no_create(dir.path());
        let found = |journal: &notedown_cloud::journal::Journal| {
            journal.read_from(1).unwrap().any(|e| {
                let e = e.unwrap();
                e.origin == Origin::Scan
                    && matches!(&e.body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/hand.pdf")
            })
        };
        // The file is written again every 5 s, so a watch registered late still sees a change.
        // Not more often: each write starts the watch's quiet time (2 s) again.
        let end = Instant::now() + Duration::from_secs(20);
        let mut again = Instant::now();
        while !found(&journal) {
            assert!(
                Instant::now() < end,
                "no scan event for the hand change in 20 s"
            );
            if Instant::now() >= again {
                std::fs::write(&file, b"%PDF-1.4 hand").unwrap();
                again = Instant::now() + Duration::from_secs(5);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tx.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    /// A hand change while a device holds the sync lock: the watch keeps the scope and scans it
    /// after the device ends its sync, without another change on disk.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_watch_waits_for_the_sync_lock() {
        use std::time::{Duration, Instant};
        let s = Server::with(Config {
            protect: false,
            scan_interval: 0,
            watch: true,
            ..Config::default()
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(notedown_cloud::server::serve_until(
            vec![l],
            s.state.clone(),
            async {
                let _ = rx.await;
            },
        ));
        let created = |rel: &str| {
            s.events().iter().any(|e| {
                e.origin == Origin::Scan
                    && matches!(&e.body, EventBody::FileCreated { path, .. } if path == rel)
            })
        };
        // A first hand change, written again every 5 s, shows that the watch is running.
        let warm = format!("{DOC}/warm.txt");
        let end = Instant::now() + Duration::from_secs(20);
        let mut again = Instant::now();
        while !created(&warm) {
            assert!(Instant::now() < end, "the watch did not start in 20 s");
            if Instant::now() >= again {
                s.write(&warm, b"warm");
                again = Instant::now() + Duration::from_secs(5);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // The start scans, so the file is written after it.
        assert_ok(&s.start().await);
        assert_eq!(s.holder().as_deref(), Some(SERIAL));
        let held = format!("{DOC}/held.txt");
        s.write(&held, b"held");
        // Longer than the quiet time of the watch (2 s): its round finds the account busy.
        let until = Instant::now() + Duration::from_secs(4);
        while Instant::now() < until {
            assert!(!created(&held), "scanned while the device held the lock");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert_ok(&s.end().await);
        assert_eq!(s.holder(), None);
        let end = Instant::now() + Duration::from_secs(20);
        while !created(&held) {
            assert!(
                Instant::now() < end,
                "no scan event in 20 s after the lock was released"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tx.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    /// A data folder without `accounts/` at start: the watch starts once it appears.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_watch_starts_when_the_accounts_folder_appears() {
        use notedown_cloud::watch::Timing;
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            scan_interval: 0,
            watch: true,
            ..Config::default()
        };
        let state = AppState::open(config, dir.path()).unwrap();
        assert!(!dir.path().join("accounts").exists());
        let watch = tokio::spawn(notedown_cloud::watch::run(
            state.clone(),
            Timing {
                quiet: Duration::from_millis(200),
                missing: Duration::from_millis(50),
            },
        ));
        // Long enough for the watch to look for the folder and miss it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!watch.is_finished(), "the watch stopped");
        seed_imported(dir.path());
        let rel = "NOTE/Note/hand.pdf";
        let file = dir.path().join("accounts/jordi/files").join(rel);
        std::fs::write(&file, b"%PDF-1.4 hand").unwrap();
        let found = || {
            state.journal.read_from(1).unwrap().any(|e| {
                let e = e.unwrap();
                e.origin == Origin::Scan
                    && matches!(&e.body, EventBody::FileCreated { path, .. } if path == rel)
            })
        };
        let end = Instant::now() + Duration::from_secs(10);
        while !found() {
            assert!(
                Instant::now() < end,
                "no scan event for the hand change in 10 s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        watch.abort();
    }

    /// A held account whose files come back into one folder: the watch scope of that folder
    /// scans the whole tree, so the hold ends at once.
    #[test]
    fn a_folder_scan_of_a_held_account_scans_the_whole_tree() {
        let s = Server::new();
        let (a, b) = (format!("{DOC}/a.txt"), format!("{DOC}/b.txt"));
        s.write(&a, b"a");
        s.write(&b, b"b");
        s.state.scan_all();
        s.rm(&a);
        s.rm(&b);
        s.state.scan_all();
        assert!(s.marker().is_file());
        s.write(&a, b"a");
        s.write(&b, b"b");
        let folder = notedown_cloud::store::WirePath::parse_named(DOC).unwrap();
        assert_eq!(
            s.state
                .scan_account("jordi", &notedown_cloud::watch::Scope::Folder(folder)),
            ScanOutcome::Done
        );
        assert!(!s.marker().exists());
        assert_eq!(s.kinds(), vec!["file.created", "file.created"]);
    }

    /// The same with a hold file that can't be read: the folder scan scans the whole tree and
    /// removes it.
    #[test]
    fn a_folder_scan_with_an_unreadable_hold_scans_the_whole_tree() {
        let s = Server::new();
        s.write(&format!("{DOC}/a.txt"), b"a");
        s.state.scan_all();
        std::fs::write(s.marker(), b"not json").unwrap();
        let folder = notedown_cloud::store::WirePath::parse_named(DOC).unwrap();
        assert_eq!(
            s.state
                .scan_account("jordi", &notedown_cloud::watch::Scope::Folder(folder)),
            ScanOutcome::Done
        );
        assert!(!s.marker().exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_watch_lifts_a_hold_when_the_files_come_back_by_hand() {
        use std::time::{Duration, Instant};
        let s = Server::with(Config {
            protect: false,
            scan_interval: 0,
            watch: true,
            ..Config::default()
        });
        let (a, b) = (format!("{DOC}/a.txt"), format!("{DOC}/b.txt"));
        s.write(&a, b"a");
        s.write(&b, b"b");
        s.state.scan_all();
        s.rm(&a);
        s.rm(&b);
        s.state.scan_all();
        assert!(s.marker().is_file());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(notedown_cloud::server::serve_until(
            vec![l],
            s.state.clone(),
            async {
                let _ = rx.await;
            },
        ));
        // Time for the watch to be registered; the files are written again when the first
        // write came too early.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let end = Instant::now() + Duration::from_secs(20);
        let mut again = Instant::now();
        while s.marker().exists() {
            assert!(Instant::now() < end, "the hold is still there after 20 s");
            if Instant::now() >= again {
                s.write(&a, b"a");
                s.write(&b, b"b");
                again = Instant::now() + Duration::from_secs(5);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(s.state.store("jordi").unwrap().scan_hold().unwrap(), None);
        tx.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    /// A file moved by hand between two folders, scanned as the watch does: the scope of the
    /// two changed paths is their lowest common folder, so one scan pairs the move.
    fn moved_by_watch_scan(
        from: &str,
        to: &str,
    ) -> (
        notedown_cloud::watch::Scope,
        Vec<(&'static str, Option<u64>)>,
    ) {
        let s = Server::new();
        for i in 0..5 {
            s.write(&format!("{DOC}/k{i}.txt"), format!("k{i}").as_bytes());
        }
        s.write(from, b"a");
        std::fs::create_dir_all(s.fs(to).parent().unwrap()).unwrap();
        s.state.scan_all();
        let before = s.events().len();
        std::fs::rename(s.fs(from), s.fs(to)).unwrap();
        let accounts = s.dir.path().join("accounts");
        let scopes = notedown_cloud::watch::scopes_of(
            &accounts,
            &[
                accounts.join("jordi/files").join(from),
                accounts.join("jordi/files").join(to),
            ],
            false,
        );
        assert_eq!(
            s.state.scan_account("jordi", &scopes["jordi"]),
            ScanOutcome::Done
        );
        let events = s.events();
        let created = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::FileCreated { path, id, .. } if path == from => Some(*id),
                _ => None,
            })
            .unwrap();
        let moved = events[before..]
            .iter()
            .map(|e| {
                let k = kind_names(std::slice::from_ref(&e.body))[0];
                let id = match &e.body {
                    EventBody::FileMoved { id, .. } => Some(*id),
                    _ => None,
                };
                (k, id.filter(|i| *i == created))
            })
            .collect();
        (scopes["jordi"].clone(), moved)
    }

    #[test]
    fn a_move_across_folders_keeps_its_id_in_a_watch_scan() {
        use notedown_cloud::watch::Scope;
        let note = notedown_cloud::store::WirePath::parse_named("NOTE/Note").unwrap();
        for (from, to, scope) in [
            (
                "NOTE/Note/A/a.txt",
                "NOTE/Note/B/a.txt",
                Scope::Folder(note),
            ),
            (
                &format!("{DOC}/a.txt") as &str,
                "NOTE/Note/a.txt",
                Scope::Whole,
            ),
        ] {
            let (scanned, moved) = moved_by_watch_scan(from, to);
            assert_eq!(scanned, scope, "{from} -> {to}");
            assert!(
                matches!(moved.as_slice(), [("file.moved", Some(_))]),
                "{from} -> {to}: {moved:?}"
            );
        }
    }

    // ------------------------------------------------ every device route scans first

    fn origins(s: &Server) -> Vec<(&'static str, Origin)> {
        s.events()
            .into_iter()
            .map(|e| {
                let k = kind_names(std::slice::from_ref(&e.body))[0];
                (k, e.origin)
            })
            .collect()
    }

    fn id_in(v: &Value, key: &str) -> String {
        v[key]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{v}"))
            .to_string()
    }

    #[tokio::test]
    async fn a_folder_made_by_hand_is_journaled_before_a_folder_inside_it() {
        let s = Server::new();
        std::fs::create_dir_all(s.fs("INBOX/A")).unwrap();
        let v = s
            .post(CREATE, json!({"path": "/INBOX/A/B", "autorename": false}))
            .await;
        assert_ok(&v);
        assert_eq!(
            origins(&s),
            vec![
                ("folder.created", Origin::Scan),
                ("folder.created", Origin::Device)
            ]
        );
        let paths: Vec<String> = s
            .events()
            .iter()
            .map(|e| match &e.body {
                EventBody::FolderCreated { path, .. } => path.clone(),
                b => panic!("{b:?}"),
            })
            .collect();
        assert_eq!(paths, vec!["INBOX/A", "INBOX/A/B"]);
    }

    #[tokio::test]
    async fn an_upload_into_a_folder_made_by_hand_journals_the_folder_first() {
        let s = Server::new();
        std::fs::create_dir_all(s.fs("INBOX/A")).unwrap();
        let v = s
            .post(
                APPLY,
                json!({"path": "/INBOX/A/x.pdf", "fileName": "x.pdf", "size": 1}),
            )
            .await;
        assert_ok(&v);
        assert_eq!(origins(&s), vec![("folder.created", Origin::Scan)]);
    }

    #[tokio::test]
    async fn a_device_delete_of_a_folder_journals_its_hand_added_file_first() {
        let s = Server::new();
        let v = s
            .post(CREATE, json!({"path": "/INBOX/F", "autorename": false}))
            .await;
        assert_ok(&v);
        let id = id_in(&v, "metadata");
        s.write("INBOX/F/x.txt", b"x");
        assert_ok(&s.post(DELETE, json!({"id": id})).await);
        assert_eq!(
            origins(&s),
            vec![
                ("folder.created", Origin::Device),
                ("file.created", Origin::Scan),
                ("file.deleted", Origin::Device),
                ("folder.deleted", Origin::Device),
            ]
        );
    }

    #[tokio::test]
    async fn a_device_copy_of_a_folder_journals_its_hand_added_file_first() {
        let s = Server::new();
        let v = s
            .post(CREATE, json!({"path": "/INBOX/F", "autorename": false}))
            .await;
        let id = id_in(&v, "metadata");
        s.write("INBOX/F/sub/x.txt", b"x");
        let v = s
            .post(
                COPY,
                json!({"id": id, "to_path": "/INBOX/G", "autorename": false}),
            )
            .await;
        assert_ok(&v);
        let o = origins(&s);
        assert_eq!(
            o[..3],
            [
                ("folder.created", Origin::Device),
                ("folder.created", Origin::Scan),
                ("file.created", Origin::Scan),
            ],
            "{o:?}"
        );
        assert!(
            o[3..].iter().all(|(_, origin)| *origin == Origin::Device),
            "{o:?}"
        );
    }

    #[tokio::test]
    async fn a_move_of_a_file_gone_by_hand_journals_the_delete() {
        let s = Server::new();
        let (a, _) = s.two_files().await;
        let EventBody::FileCreated { id, .. } = s.events()[0].body.clone() else {
            panic!();
        };
        s.rm(&a);
        let v = s
            .post(
                MOVE,
                json!({"id": id.to_string(), "to_path": "/INBOX/a.txt", "autorename": false}),
            )
            .await;
        assert_eq!(v["errorCode"], "E0320", "{v}");
        let last = s.events().pop().unwrap();
        assert_eq!(last.origin, Origin::Scan);
        assert!(
            matches!(&last.body, EventBody::FileDeleted { id: i, .. } if *i == id),
            "{last:?}"
        );
    }

    #[tokio::test]
    async fn capacity_never_gives_a_hand_added_file_an_id() {
        let s = Server::new();
        let path = format!("{DOC}/a.pdf");
        s.write(&path, b"hello");
        let v = s.post(CAPACITY, json!({})).await;
        assert_ok(&v);
        assert_eq!(v["usedCapacity"], 5, "{v}");
        let store = Store::open(s.dir.path(), "jordi").unwrap();
        assert_eq!(
            store
                .id_of(&notedown_cloud::store::WirePath::parse(&path).unwrap())
                .unwrap(),
            None
        );
        assert_ok(&s.list("/", true).await);
        assert_eq!(origins(&s), vec![("file.created", Origin::Scan)]);
    }

    #[tokio::test]
    async fn sync_start_does_not_scan_while_another_device_syncs() {
        let s = Server::new();
        let other = s.token_of("SN078C10000001");
        let v = s
            .post_as(&other, START, json!({"equipmentNo": "SN078C10000001"}))
            .await;
        assert_ok(&v);
        s.write(&format!("{DOC}/a.txt"), b"a");
        let v = s.start().await;
        assert_eq!(v["errorCode"], "E0078", "{v}");
        assert!(s.events().is_empty(), "{:?}", s.events());
    }
}
