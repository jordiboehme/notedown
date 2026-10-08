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
    // Not a PDF: a file that is one would count as a PDF of an earlier pass without manifest.
    std::fs::write(fx.out().join("NOTE/Note/a.pdf"), b"mine").unwrap();
    fx.write("NOTE/Note/a.note", &fixture_note("test1"));
    fx.scan();
    let r = fx.pass();
    assert_eq!(r.kept_foreign, s(&["NOTE/Note/a.pdf"]));
    assert!(r.written.is_empty());
    assert_eq!(
        std::fs::read(fx.out().join("NOTE/Note/a.pdf")).unwrap(),
        b"mine"
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
