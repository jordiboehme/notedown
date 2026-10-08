//! The render pass (`render::render_account`): PDFs of an account's notes in a render folder,
//! on a store and a render folder in temp folders. Notes are placed, changed and removed by
//! hand and picked up with a scan, so ids follow moves.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Journal;
use notedown_cloud::render::manifest::{self, MANIFEST_FILE};
use notedown_cloud::render::{LOCK_FILE, PassOptions, PassReport, pdf_path_of, render_account};
use notedown_cloud::store::{ScanOptions, Store, WirePath};
use notedown_note::RENDERER_ID;

/// 2026-10-08 08:00 UTC.
const NOW: u64 = 1_791_446_400_000;

fn fixture_note(name: &str) -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join(format!("tests/fixtures/notes/{name}.note"))).unwrap()
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    const_hex::encode(Md5::digest(bytes))
}

struct Fx {
    data: tempfile::TempDir,
    render: tempfile::TempDir,
    store: Store,
    journal: Journal,
}

impl Fx {
    /// Account `acc` with its native folders, and an empty render folder.
    fn new() -> Fx {
        let data = tempfile::tempdir().unwrap();
        Accounts::new(data.path())
            .create("acc", "acc@example.org", "secret")
            .unwrap();
        let store = Store::open(data.path(), "acc").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let journal = Journal::open(data.path()).unwrap();
        Fx {
            data,
            render: tempfile::tempdir().unwrap(),
            store,
            journal,
        }
    }

    fn root(&self) -> &Path {
        self.render.path()
    }

    /// `<root>/acc`.
    fn out(&self) -> PathBuf {
        self.root().join("acc")
    }

    fn fs(&self, rel: &str) -> PathBuf {
        self.store.fs_path(&WirePath::parse_named(rel).unwrap())
    }

    /// Place a note by hand (parents too).
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.fs(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }

    fn rename(&self, from: &str, to: &str) {
        std::fs::rename(self.fs(from), self.fs(to)).unwrap();
    }

    fn id(&self, rel: &str) -> Option<u64> {
        self.store
            .id_of(&WirePath::parse_named(rel).unwrap())
            .unwrap()
    }

    fn scan(&self) {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        self.store
            .scan(&g, &self.journal, &ScanOptions::all(0), NOW)
            .unwrap();
    }

    fn pass(&self) -> PassReport {
        pass(&self.store, self.root())
    }

    fn pass_all(&self) -> PassReport {
        render_account(
            &self.store,
            self.root(),
            &PassOptions {
                all: true,
                wait_for_lock: true,
            },
        )
        .unwrap()
    }

    fn manifest(&self) -> manifest::Manifest {
        manifest::load(&self.out())
    }

    /// Every file below `<root>/acc` (relative paths), the manifest and the lock left out.
    fn files(&self) -> BTreeSet<String> {
        fn walk(dir: &Path, base: &Path, out: &mut BTreeSet<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
                    if rel != MANIFEST_FILE && rel != LOCK_FILE {
                        out.insert(rel);
                    }
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.out(), &self.out(), &mut out);
        out
    }
}

fn pass(store: &Store, root: &Path) -> PassReport {
    render_account(
        store,
        root,
        &PassOptions {
            all: false,
            wait_for_lock: true,
        },
    )
    .unwrap()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// The names `read_dir` lists in `dir` (exact spelling, also on a case-insensitive disk).
fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect()
}

#[test]
fn a_new_note_gets_its_pdf_and_a_manifest_row() {
    let fx = Fx::new();
    let bytes = fixture_note("test1");
    fx.write("NOTE/Note/a.note", &bytes);
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.written, s(&["NOTE/Note/a.pdf"]));
    assert!(r.failed.is_empty() && r.moved.is_empty() && r.deleted.is_empty());
    assert!(!r.busy);
    let pdf = std::fs::read(fx.out().join("NOTE/Note/a.pdf")).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));

    let m = fx.manifest();
    assert_eq!(m.version, 1);
    assert_eq!(m.renderer, RENDERER_ID);
    let id = fx.id("NOTE/Note/a.note").unwrap();
    let row = &m.notes[&id.to_string()];
    assert_eq!(row.path, "NOTE/Note/a.note");
    assert_eq!(row.md5, md5_hex(&bytes));
    assert!(row.pdf);

    let again = fx.pass();
    assert_eq!(again, PassReport::default());
}

#[test]
fn a_changed_note_is_rendered_again() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    fx.pass();
    let before = std::fs::read(fx.out().join("NOTE/Note/a.pdf")).unwrap();
    fx.write("NOTE/Note/a.note", &fixture_note("test2"));
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.written, s(&["NOTE/Note/a.pdf"]));
    let after = std::fs::read(fx.out().join("NOTE/Note/a.pdf")).unwrap();
    assert!(after.starts_with(b"%PDF-"));
    assert_ne!(before, after);
    let id = fx.id("NOTE/Note/a.note").unwrap();
    assert_eq!(
        fx.manifest().notes[&id.to_string()].md5,
        md5_hex(&fixture_note("test2"))
    );
}

#[test]
fn a_moved_note_and_a_moved_folder_move_the_pdfs() {
    let fx = Fx::new();
    fx.write("NOTE/Note/W/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/W/b.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    let (ida, idb) = (
        fx.id("NOTE/Note/W/a.note").unwrap(),
        fx.id("NOTE/Note/W/b.note").unwrap(),
    );
    let pdf_a = std::fs::read(fx.out().join("NOTE/Note/W/a.pdf")).unwrap();

    fx.rename("NOTE/Note/W/a.note", "NOTE/Note/W/c.note");
    fx.rename("NOTE/Note/W", "NOTE/Note/V");
    fx.scan();
    // The scan keeps the ids, so the pass sees moves.
    assert_eq!(fx.id("NOTE/Note/V/c.note"), Some(ida));
    assert_eq!(fx.id("NOTE/Note/V/b.note"), Some(idb));

    let r = fx.pass();
    assert!(r.written.is_empty(), "{r:?}");
    let mut moved = r.moved.clone();
    moved.sort();
    assert_eq!(
        moved,
        vec![
            (
                "NOTE/Note/W/a.pdf".to_string(),
                "NOTE/Note/V/c.pdf".to_string()
            ),
            (
                "NOTE/Note/W/b.pdf".to_string(),
                "NOTE/Note/V/b.pdf".to_string()
            ),
        ]
    );
    assert_eq!(
        std::fs::read(fx.out().join("NOTE/Note/V/c.pdf")).unwrap(),
        pdf_a
    );
    assert!(fx.out().join("NOTE/Note/V/b.pdf").is_file());
    assert!(!fx.out().join("NOTE/Note/W").exists());
    assert_eq!(
        fx.manifest().notes[&ida.to_string()].path,
        "NOTE/Note/V/c.note"
    );
}

#[test]
fn a_case_only_rename_moves_the_pdf() {
    let fx = Fx::new();
    fx.write("NOTE/Note/Meeting.note", &fixture_note("test1"));
    fx.scan();
    fx.pass();
    let id = fx.id("NOTE/Note/Meeting.note").unwrap();
    fx.rename("NOTE/Note/Meeting.note", "NOTE/Note/meeting.note");
    fx.scan();
    assert_eq!(fx.id("NOTE/Note/meeting.note"), Some(id));

    let r = fx.pass();
    assert!(r.written.is_empty(), "{r:?}");
    assert!(r.kept_foreign.is_empty(), "{r:?}");
    assert_eq!(
        r.moved,
        vec![(
            "NOTE/Note/Meeting.pdf".to_string(),
            "NOTE/Note/meeting.pdf".to_string()
        )]
    );
    let listed = names(&fx.out().join("NOTE/Note"));
    assert!(listed.contains("meeting.pdf"), "{listed:?}");
    assert!(!listed.contains("Meeting.pdf"), "{listed:?}");
}

#[test]
fn a_deleted_note_and_folder_remove_the_pdfs_and_empty_folders() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/b.note", &fixture_note("test2"));
    fx.write("NOTE/Note/V/c.note", &fixture_note("test1"));
    fx.write("NOTE/Note/V/Deep/d.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    assert!(fx.out().join("NOTE/Note/V/Deep/d.pdf").is_file());

    std::fs::remove_file(fx.fs("NOTE/Note/b.note")).unwrap();
    std::fs::remove_dir_all(fx.fs("NOTE/Note/V")).unwrap();
    fx.scan();
    let r = fx.pass();
    let mut deleted = r.deleted.clone();
    deleted.sort();
    assert_eq!(
        deleted,
        s(&[
            "NOTE/Note/V/Deep/d.pdf",
            "NOTE/Note/V/c.pdf",
            "NOTE/Note/b.pdf"
        ])
    );
    assert!(r.written.is_empty() && r.moved.is_empty());
    assert!(!fx.out().join("NOTE/Note/V").exists());
    assert!(!fx.out().join("NOTE/Note/b.pdf").exists());
    assert!(fx.out().join("NOTE/Note/a.pdf").is_file());
    assert_eq!(fx.files(), BTreeSet::from(["NOTE/Note/a.pdf".to_string()]));
    assert_eq!(fx.manifest().notes.len(), 1);
}

#[test]
fn an_unreadable_note_gets_no_pdf_and_is_not_retried() {
    let fx = Fx::new();
    let bytes = fixture_note("test1");
    fx.write("NOTE/Note/a.note", &bytes[..100]);
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert_eq!(r.failed[0].0, "NOTE/Note/a.note");
    assert!(!r.failed[0].1.is_empty());
    // A note that doesn't render is no disk error: the server doesn't try it again.
    assert_eq!(r.retry, None);
    assert!(r.written.is_empty());
    assert!(!fx.out().join("NOTE/Note/a.pdf").exists());
    let id = fx.id("NOTE/Note/a.note").unwrap();
    let row = fx.manifest().notes[&id.to_string()].clone();
    assert!(!row.pdf);
    assert_eq!(row.md5, md5_hex(&bytes[..100]));

    let again = fx.pass();
    assert!(
        again.failed.is_empty() && again.written.is_empty(),
        "{again:?}"
    );

    // Once it changes, it is tried again.
    fx.write("NOTE/Note/a.note", &bytes);
    fx.scan();
    assert_eq!(fx.pass().written, s(&["NOTE/Note/a.pdf"]));
}

#[test]
fn a_foreign_file_in_the_render_root_is_left_alone() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.out().join("NOTE/Note")).unwrap();
    std::fs::write(fx.out().join("NOTE/Note/notes.txt"), b"mine").unwrap();
    std::fs::write(fx.out().join("own.pdf"), b"%PDF-1.7 mine").unwrap();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/W/b.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    fx.rename("NOTE/Note/a.note", "NOTE/Note/W/a.note");
    fx.scan();
    fx.pass();
    std::fs::remove_dir_all(fx.fs("NOTE/Note/W")).unwrap();
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.deleted.len(), 2, "{r:?}");
    assert_eq!(
        std::fs::read(fx.out().join("NOTE/Note/notes.txt")).unwrap(),
        b"mine"
    );
    assert_eq!(
        std::fs::read(fx.out().join("own.pdf")).unwrap(),
        b"%PDF-1.7 mine"
    );
    assert_eq!(
        fx.files(),
        BTreeSet::from(["NOTE/Note/notes.txt".to_string(), "own.pdf".to_string()])
    );
}

#[test]
fn a_foreign_file_at_the_target_is_kept() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.out().join("NOTE/Note")).unwrap();
    // A PDF, but not one notedown made: kept also on the first pass (no manifest yet).
    std::fs::write(fx.out().join("NOTE/Note/a.pdf"), b"%PDF-1.7 mine").unwrap();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.kept_foreign, s(&["NOTE/Note/a.pdf"]));
    assert!(r.written.is_empty());
    assert_eq!(
        std::fs::read(fx.out().join("NOTE/Note/a.pdf")).unwrap(),
        b"%PDF-1.7 mine"
    );
    assert!(fx.manifest().notes.values().all(|row| !row.pdf));
}

#[test]
fn a_lost_manifest_renders_all_and_leaves_the_same_files() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/W/b.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    let files = fx.files();
    std::fs::remove_file(fx.out().join(MANIFEST_FILE)).unwrap();
    // The PDFs notedown wrote are known by their producer and replaced in place.
    let r = fx.pass();
    let mut written = r.written.clone();
    written.sort();
    assert_eq!(written, s(&["NOTE/Note/W/b.pdf", "NOTE/Note/a.pdf"]));
    assert!(r.kept_foreign.is_empty(), "{r:?}");
    assert_eq!(fx.files(), files);
    assert_eq!(fx.manifest().notes.len(), 2);
}

#[test]
fn a_new_renderer_id_renders_all() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/b.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    let mut m = fx.manifest();
    m.renderer = "old".into();
    manifest::save(&fx.out(), &m).unwrap();
    let r = fx.pass();
    assert_eq!(r.written, s(&["NOTE/Note/a.pdf", "NOTE/Note/b.pdf"]));
    assert_eq!(fx.manifest().renderer, RENDERER_ID);
    assert_eq!(fx.pass(), PassReport::default());
}

#[test]
fn all_renders_every_note_again() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/b.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    let files = fx.files();
    let r = fx.pass_all();
    assert_eq!(r.written, s(&["NOTE/Note/a.pdf", "NOTE/Note/b.pdf"]));
    assert_eq!(fx.files(), files);
}

#[test]
fn an_upper_case_extension_is_rendered() {
    let fx = Fx::new();
    fx.write("NOTE/Note/X.NOTE", &fixture_note("test1"));
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.written, s(&["NOTE/Note/X.pdf"]));
    assert!(names(&fx.out().join("NOTE/Note")).contains("X.pdf"));
}

#[test]
fn a_missing_render_dir_is_an_error_and_creates_nothing() {
    let fx = Fx::new();
    let root = fx.root().join("missing");
    let err = render_account(
        &fx.store,
        &root,
        &PassOptions {
            all: false,
            wait_for_lock: true,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("doesn't exist"), "{err}");
    assert!(!root.exists());
}

#[test]
fn a_render_dir_inside_the_accounts_is_refused() {
    let fx = Fx::new();
    let root = fx.data.path().join("accounts/acc/files/NOTE");
    let err = render_account(
        &fx.store,
        &root,
        &PassOptions {
            all: false,
            wait_for_lock: true,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("inside the data folder"), "{err}");
    assert!(!root.join("acc").exists());
}

#[test]
fn an_account_without_files_gets_no_pdf_and_keeps_no_files() {
    let data = tempfile::tempdir().unwrap();
    let render = tempfile::tempdir().unwrap();
    Accounts::new(data.path())
        .create("acc", "acc@example.org", "secret")
        .unwrap();
    let store = Store::open(data.path(), "acc").unwrap();
    let files = store.files_dir().to_path_buf();
    if files.exists() {
        std::fs::remove_dir_all(&files).unwrap();
    }
    let r = pass(&store, render.path());
    assert_eq!(r, PassReport::default());
    assert!(!files.exists(), "the pass created files/");
    assert!(!render.path().join("acc").join(MANIFEST_FILE).exists());
}

#[test]
fn a_render_folder_on_the_data_folder_is_refused() {
    let render_err = |store: &Store, root: &Path| {
        render_account(
            store,
            root,
            &PassOptions {
                all: false,
                wait_for_lock: true,
            },
        )
        .unwrap_err()
        .to_string()
    };
    // The data folder as the render root: `<root>/acc` would be a folder of the data folder.
    let fx = Fx::new();
    let err = render_err(&fx.store, fx.data.path());
    assert!(err.contains("inside the data folder"), "{err}");
    assert!(!fx.data.path().join("acc").exists());

    // A data folder named like the account, with its parent as the render root: `<root>/acc`
    // would be the data folder itself.
    let parent = tempfile::tempdir().unwrap();
    let data = parent.path().join("acc");
    std::fs::create_dir(&data).unwrap();
    Accounts::new(&data)
        .create("acc", "acc@example.org", "secret")
        .unwrap();
    let store = Store::open(&data, "acc").unwrap();
    {
        let g = store.lock().unwrap();
        store.ensure_native_folders(&g).unwrap();
    }
    let err = render_err(&store, parent.path());
    assert!(err.contains("inside the data folder"), "{err}");
    assert!(!data.join(MANIFEST_FILE).exists());
    assert!(!data.join(LOCK_FILE).exists());

    // The same with another spelling of the name: on a case-insensitive disk, `<root>/acc` is
    // the data folder `Acc`.
    let parent = tempfile::tempdir().unwrap();
    let data = parent.path().join("Acc");
    std::fs::create_dir(&data).unwrap();
    if parent.path().join("acc").exists() {
        Accounts::new(&data)
            .create("acc", "acc@example.org", "secret")
            .unwrap();
        let store = Store::open(&data, "acc").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let err = render_err(&store, parent.path());
        assert!(err.contains("inside the data folder"), "{err}");
        assert!(!data.join(MANIFEST_FILE).exists());
    }

    // The parent of a data folder with another name is fine.
    let other = tempfile::tempdir().unwrap();
    let data = other.path().join("notedown-data");
    std::fs::create_dir(&data).unwrap();
    Accounts::new(&data)
        .create("acc", "acc@example.org", "secret")
        .unwrap();
    let store = Store::open(&data, "acc").unwrap();
    {
        let g = store.lock().unwrap();
        store.ensure_native_folders(&g).unwrap();
    }
    assert!(!pass(&store, other.path()).busy);
    assert!(other.path().join("acc").join(MANIFEST_FILE).exists());
}

#[test]
fn a_busy_lock_skips_the_pass_without_waiting() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    std::fs::create_dir_all(fx.out()).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fx.out().join(LOCK_FILE))
        .unwrap();
    held.try_lock().unwrap();
    let r = render_account(
        &fx.store,
        fx.root(),
        &PassOptions {
            all: false,
            wait_for_lock: false,
        },
    )
    .unwrap();
    assert!(r.busy);
    assert!(r.written.is_empty());
    assert!(!fx.out().join("NOTE/Note/a.pdf").exists());
    assert!(!fx.out().join(MANIFEST_FILE).exists());
    drop(held);
    assert_eq!(fx.pass().written, s(&["NOTE/Note/a.pdf"]));
}

#[test]
fn pdf_path_of_replaces_the_last_note_extension() {
    assert_eq!(pdf_path_of("NOTE/Note/a.note"), "NOTE/Note/a.pdf");
    assert_eq!(pdf_path_of("NOTE/Note/X.NOTE"), "NOTE/Note/X.pdf");
    assert_eq!(pdf_path_of("NOTE/Note/a.note.note"), "NOTE/Note/a.note.pdf");
    assert_eq!(pdf_path_of("NOTE/Note/a"), "NOTE/Note/a.pdf");
}

#[test]
fn a_pdf_written_before_its_row_was_saved_is_still_ours() {
    // A crash after the PDF was written and before the manifest was saved.
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.write("NOTE/Note/b.note", &fixture_note("test2"));
    fx.scan();
    fx.pass();
    let mut m = fx.manifest();
    let id = fx.id("NOTE/Note/b.note").unwrap();
    m.notes.remove(&id.to_string());
    manifest::save(&fx.out(), &m).unwrap();
    let r = fx.pass();
    assert_eq!(r.written, s(&["NOTE/Note/b.pdf"]));
    assert!(r.kept_foreign.is_empty(), "{r:?}");
    assert!(fx.manifest().notes[&id.to_string()].pdf);
}

#[test]
fn a_foreign_pdf_at_the_target_is_kept_once_a_manifest_exists() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    fx.pass();
    std::fs::write(fx.out().join("NOTE/Note/b.pdf"), b"%PDF-1.7 mine").unwrap();
    fx.write("NOTE/Note/b.note", &fixture_note("test2"));
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.kept_foreign, s(&["NOTE/Note/b.pdf"]));
    assert!(r.written.is_empty());
    assert_eq!(
        std::fs::read(fx.out().join("NOTE/Note/b.pdf")).unwrap(),
        b"%PDF-1.7 mine"
    );
}

/// The pass reads the store without changing it: no ids, no MD5 cache rows, no journal. A
/// scan after it still journals every change made by hand before it.
#[test]
fn a_pass_before_a_scan_keeps_hand_changes_for_the_scan() {
    let fx = Fx::new();
    fx.write("NOTE/Note/k.note", &fixture_note("test1"));
    fx.write("NOTE/Note/r.note", &fixture_note("test1"));
    fx.scan();
    // Other bytes than r.note, so the scan can pair the rename by its MD5.
    let mut other = fixture_note("test1");
    other.extend_from_slice(b"x");
    fx.write("NOTE/Note/n.note", &other);
    fx.write("NOTE/Note/k.note", &fixture_note("test2"));
    fx.rename("NOTE/Note/r.note", "NOTE/Note/r2.note");
    fx.pass();
    let g = fx.store.lock_recovered(&fx.journal).unwrap();
    let rep = fx
        .store
        .scan(&g, &fx.journal, &ScanOptions::all(0), NOW)
        .unwrap();
    let p = |x: &str| WirePath::parse_named(x).unwrap();
    assert_eq!(rep.created, vec![p("NOTE/Note/n.note")]);
    assert_eq!(rep.modified, vec![p("NOTE/Note/k.note")]);
    assert_eq!(
        rep.moved,
        vec![(p("NOTE/Note/r.note"), p("NOTE/Note/r2.note"))]
    );
    assert!(rep.deleted.is_empty(), "{:?}", rep.deleted);
}

#[test]
fn a_note_placed_by_hand_waits_for_the_scan() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    let r = fx.pass();
    assert!(r.written.is_empty(), "{r:?}");
    assert_eq!(fx.id("NOTE/Note/a.note"), None);
    fx.scan();
    assert_eq!(fx.pass().written, s(&["NOTE/Note/a.pdf"]));
}

#[test]
fn a_note_deleted_by_hand_keeps_its_pdf_until_the_scan() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    fx.pass();
    std::fs::remove_file(fx.fs("NOTE/Note/a.note")).unwrap();
    let r = fx.pass();
    assert_eq!(r, PassReport::default());
    assert!(fx.out().join("NOTE/Note/a.pdf").is_file());
    fx.scan();
    assert_eq!(fx.pass().deleted, s(&["NOTE/Note/a.pdf"]));
    assert!(!fx.out().join("NOTE/Note/a.pdf").exists());
}

#[test]
fn a_note_edited_by_hand_before_the_scan_is_rendered_from_its_bytes() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    fx.pass();
    fx.write("NOTE/Note/a.note", &fixture_note("test2"));
    let r = fx.pass();
    assert_eq!(r.written, s(&["NOTE/Note/a.pdf"]));
    let id = fx.id("NOTE/Note/a.note").unwrap();
    assert_eq!(
        fx.manifest().notes[&id.to_string()].md5,
        md5_hex(&fixture_note("test2"))
    );
    // Nothing new until it changes again.
    assert_eq!(fx.pass(), PassReport::default());
}

/// Files and an empty folder outside `<root>/acc` (and a `.pdf` file inside it, the PDF path
/// of an empty note path) that bad manifest rows point at. They must all survive.
struct Sentinels {
    files: Vec<PathBuf>,
    empty: PathBuf,
}

impl Sentinels {
    fn new(fx: &Fx) -> Sentinels {
        let outside = fx.root().join("outside");
        std::fs::create_dir_all(outside.join("empty")).unwrap();
        std::fs::create_dir_all(fx.out()).unwrap();
        let files = vec![
            outside.join("tax.pdf"),
            outside.join("rel.pdf"),
            fx.out().join(".pdf"),
        ];
        for f in &files {
            std::fs::write(f, b"%PDF-1.7 keep").unwrap();
        }
        Sentinels {
            files,
            empty: outside.join("empty"),
        }
    }

    /// Row paths: absolute, with `..` (one at a file, one in the empty folder), empty.
    fn bad_paths(fx: &Fx) -> Vec<String> {
        vec![
            fx.root().join("outside/tax.note").display().to_string(),
            "../outside/rel.note".to_string(),
            "../outside/empty/x.note".to_string(),
            String::new(),
        ]
    }

    fn check(&self) {
        for f in &self.files {
            assert_eq!(
                std::fs::read(f).unwrap(),
                b"%PDF-1.7 keep",
                "{}",
                f.display()
            );
        }
        assert!(self.empty.is_dir(), "{}", self.empty.display());
    }
}

#[test]
fn rows_with_bad_paths_of_gone_notes_touch_nothing() {
    let fx = Fx::new();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    fx.pass();
    let sentinels = Sentinels::new(&fx);
    let mut m = fx.manifest();
    for (i, path) in Sentinels::bad_paths(&fx).into_iter().enumerate() {
        let row = manifest::Row {
            path,
            md5: "0".repeat(32),
            pdf: true,
        };
        m.notes.insert((900_000 + i).to_string(), row);
    }
    manifest::save(&fx.out(), &m).unwrap();
    let r = fx.pass();
    assert!(r.deleted.is_empty() && r.moved.is_empty(), "{r:?}");
    sentinels.check();
    // The bad rows are gone from the manifest.
    assert_eq!(fx.manifest().notes.len(), 1);
    fx.pass_all();
    sentinels.check();
}

#[test]
fn a_live_note_with_a_bad_row_path_touches_nothing_outside() {
    for i in 0..4 {
        let fx = Fx::new();
        let path = Sentinels::bad_paths(&fx)[i].clone();
        let bytes = fixture_note("test1");
        fx.write("NOTE/Note/a.note", &bytes);
        fx.scan();
        fx.pass();
        let sentinels = Sentinels::new(&fx);
        let id = fx.id("NOTE/Note/a.note").unwrap();
        let bad = |fx: &Fx| {
            let mut m = fx.manifest();
            m.notes.insert(
                id.to_string(),
                manifest::Row {
                    path: path.clone(),
                    md5: md5_hex(&bytes),
                    pdf: true,
                },
            );
            manifest::save(&fx.out(), &m).unwrap();
        };
        // Same MD5, other path: without the check this would be a move from outside.
        bad(&fx);
        let r = fx.pass();
        assert!(r.moved.is_empty() && r.deleted.is_empty(), "{path}: {r:?}");
        sentinels.check();
        assert!(fx.out().join("NOTE/Note/a.pdf").is_file());
        // `--all` renders every note and deletes the old PDF of each row.
        bad(&fx);
        let r = fx.pass_all();
        assert!(r.moved.is_empty() && r.deleted.is_empty(), "{path}: {r:?}");
        sentinels.check();
        assert_eq!(
            fx.manifest().notes[&id.to_string()].path,
            "NOTE/Note/a.note"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_follower_renders_after_a_put_and_a_quiet_time() {
    use notedown_cloud::Config;
    use notedown_cloud::render::follow::{Timing, run};
    use notedown_cloud::server::AppState;
    use std::time::{Duration, Instant};

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    let data = tempfile::tempdir().unwrap();
    let render = tempfile::tempdir().unwrap();
    Accounts::new(data.path())
        .create("acc", "acc@example.org", "secret")
        .unwrap();
    let config = Config {
        render_dir: Some(render.path().to_string_lossy().to_string()),
        ..Config::default()
    };
    let root = config.render_root(data.path()).unwrap();
    let state = AppState::open(config, data.path()).unwrap();
    let task = tokio::spawn(run(
        state.clone(),
        root.clone(),
        Timing {
            quiet: Duration::from_millis(200),
            poll: Duration::from_millis(50),
            retry: Duration::from_millis(100),
        },
    ));
    // The start pass.
    until("the start pass", || root.join("acc").exists()).await;
    assert!(!root.join("acc/NOTE/Note/n.pdf").exists());

    let src = data.path().join("n.note");
    std::fs::write(&src, fixture_note("test1")).unwrap();
    let st = state.clone();
    let put = tokio::task::spawn_blocking(move || {
        notedown_cloud::put::put(&st.data_dir, "acc", &st.journal, &src, "/NOTE/Note", None)
    })
    .await
    .unwrap()
    .unwrap();
    let last = *put.seqs.last().unwrap();
    until("the PDF", || root.join("acc/NOTE/Note/n.pdf").exists()).await;
    until("the cursor", || {
        state
            .journal
            .cursor("render")
            .unwrap()
            .is_some_and(|c| c.seq >= last)
    })
    .await;
    task.abort();
}

/// A served state with `render_dir` set, the follower running with short timing.
struct Followed {
    data: tempfile::TempDir,
    root: PathBuf,
    state: std::sync::Arc<notedown_cloud::server::AppState>,
    task: tokio::task::JoinHandle<()>,
    /// The failed passes of the follower.
    failed: std::sync::Arc<std::sync::atomic::AtomicU64>,
    _render: tempfile::TempDir,
}

impl Followed {
    /// The follower is started and its start pass is done (it set the cursor), so nothing
    /// writes into the render folder any more.
    async fn start() -> Followed {
        use notedown_cloud::render::follow::{Timing, run_counted};
        use std::time::Duration;
        let data = tempfile::tempdir().unwrap();
        let render = tempfile::tempdir().unwrap();
        Accounts::new(data.path())
            .create("acc", "acc@example.org", "secret")
            .unwrap();
        let config = notedown_cloud::Config {
            render_dir: Some(render.path().to_string_lossy().to_string()),
            ..notedown_cloud::Config::default()
        };
        let root = config.render_root(data.path()).unwrap();
        let state = notedown_cloud::server::AppState::open(config, data.path()).unwrap();
        let failed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let task = tokio::spawn(run_counted(
            state.clone(),
            root.clone(),
            Timing {
                quiet: Duration::from_millis(200),
                poll: Duration::from_millis(50),
                retry: Duration::from_millis(100),
            },
            failed.clone(),
        ));
        let f = Followed {
            data,
            root,
            state,
            task,
            failed,
            _render: render,
        };
        // The account folder appears at the start of the pass; the cursor is set at its end.
        until("the start pass", || {
            f.state.journal.cursor("render").unwrap().is_some()
        })
        .await;
        assert!(f.root.join("acc").is_dir());
        f
    }

    fn failures(&self) -> u64 {
        self.failed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Put test1 as `n.note`; the last journal seq.
    async fn put(&self) -> u64 {
        let src = self.data.path().join("n.note");
        std::fs::write(&src, fixture_note("test1")).unwrap();
        let st = self.state.clone();
        let r = tokio::task::spawn_blocking(move || {
            notedown_cloud::put::put(&st.data_dir, "acc", &st.journal, &src, "/NOTE/Note", None)
        })
        .await
        .unwrap()
        .unwrap();
        *r.seqs.last().unwrap()
    }

    fn pdf(&self) -> PathBuf {
        self.root.join("acc/NOTE/Note/n.pdf")
    }

    fn cursor(&self) -> u64 {
        self.state
            .journal
            .cursor("render")
            .unwrap()
            .map_or(0, |c| c.seq)
    }
}

/// Poll `ok` until it holds; a generous bound, so a slow machine can't fail a test.
async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ok() {
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pass_is_tried_again_when_the_render_folder_comes_back() {
    let f = Followed::start().await;
    std::fs::remove_dir_all(&f.root).unwrap();
    let last = f.put().await;
    // The pass fails and is tried again, failing again: the cursor stays before the note.
    until("two failed passes", || f.failures() >= 2).await;
    assert!(f.cursor() < last, "the cursor moved past a failed pass");
    std::fs::create_dir_all(&f.root).unwrap();
    until("the PDF", || f.pdf().exists()).await;
    until("the cursor", || f.cursor() >= last).await;
    f.task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_account_is_tried_again_when_the_lock_is_released() {
    let f = Followed::start().await;
    let lock_path = f.root.join("acc").join(LOCK_FILE);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();
    let last = f.put().await;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert!(!f.pdf().exists());
    assert!(f.cursor() < last);
    drop(lock);
    until("the PDF", || f.pdf().exists()).await;
    until("the cursor", || f.cursor() >= last).await;
    f.task.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_pdf_that_could_not_be_written_is_written_by_the_retry() {
    use std::os::unix::fs::PermissionsExt;
    let f = Followed::start().await;
    let folder = f.root.join("acc/NOTE/Note");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root may write into a read-only folder: then this test can't make the write fail.
    if std::fs::write(folder.join("probe"), b"").is_ok() {
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        f.task.abort();
        return;
    }
    let last = f.put().await;
    // The PDF can't be written: the pass counts as failed and the cursor stays before the note.
    until("a failed pass", || f.failures() >= 1).await;
    assert!(!f.pdf().exists());
    assert!(f.cursor() < last, "the cursor moved past a failed write");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
    until("the PDF", || f.pdf().exists()).await;
    until("the cursor", || f.cursor() >= last).await;
    f.task.abort();
}
