//! The Markdown files: one `.md` per note with text in a folder the user picks, kept by the
//! worker after each pass.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notedown_pipeline::PipelineConfig;
use notedown_pipeline::markdown::{self, Writer};
use notedown_pipeline::model::{Block, NoteState, PageOcrState, PageState};
use notedown_pipeline::setup::{self, Setup};
use notedown_pipeline::store::{TextStore, WriterLock};
use notedown_pipeline::worker::{Clock, Worker};

use crate::fake_engine::FakeEngine;
use crate::split::{
    Said, continuous, create_account, load, point_at, put_into, remove_account, store_dir,
};
use crate::worker::{Data, SLUG, engine_reading, no_settle, setup, stop_at};

fn with_dir(dir: &Path) -> PipelineConfig {
    PipelineConfig {
        markdown_dirs: [(SLUG.to_string(), dir.to_path_buf())].into(),
        ..no_settle()
    }
}

fn worker(d: &Data, cfg: PipelineConfig, engine: FakeEngine) -> Worker {
    crate::worker::worker(d, cfg, engine)
}

fn run(d: &Data, cfg: PipelineConfig, text: &str) {
    worker(d, cfg, engine_reading(text)).run_once(true).unwrap();
}

fn manifest_path(d: &Data) -> PathBuf {
    d.data
        .join("pipeline")
        .join(SLUG)
        .join(markdown::MANIFEST_FILE)
}

fn manifest(d: &Data) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(manifest_path(d)).unwrap()).unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every file below `dir`, relative, sorted.
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn files_dir(d: &Data) -> PathBuf {
    d.data.join("accounts").join(SLUG).join("files")
}

/// Moves the note `key` on the server to `to`, as a device sync and the server's scan would.
fn move_note(d: &Data, key: &str, from: &str, to: &str) {
    let files = files_dir(d);
    std::fs::create_dir_all(files.join(to).parent().unwrap()).unwrap();
    std::fs::rename(files.join(from), files.join(to)).unwrap();
    d.edit_record(key, |r| r.path = to.into());
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// Sets the mtime of `path` an hour back, so a new write shows.
fn age(path: &Path) -> SystemTime {
    let then = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(then)
        .unwrap();
    mtime(path)
}

#[test]
fn a_note_gets_its_markdown_file() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let key = d.put("test1", "NOTE/Note");
    run(&d, with_dir(out.path()), "hello world");
    let file = out.path().join("test1.md");
    let text = read(&file);
    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("title: \"test1\""), "{text}");
    assert!(text.contains("hello world"), "{text}");
    assert!(
        text.contains(&format!("<!-- notedown://{SLUG}/{key}/")),
        "{text}"
    );
    let m = manifest(&d);
    assert_eq!(m["dir"], out.path().to_str().unwrap());
    assert_eq!(m["files"][&key], "test1.md");
    assert_eq!(files(out.path()), ["test1.md"]);
}

#[test]
fn the_layout_follows_the_device_folders() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let work = d.put("test1", "NOTE/Note/Work");
    let style = d.put("test2", "NOTE/MyStyle");
    let doc = d.put("test1", "DOCUMENT/Document/Books");
    let cfg = PipelineConfig {
        ingest_folders: vec!["**".into()],
        ..with_dir(out.path())
    };
    run(&d, cfg, "hello");
    assert_eq!(
        files(out.path()),
        [
            "Document/Books/test1.md",
            "MyStyle/test2.md",
            "Work/test1.md"
        ]
    );
    let m = manifest(&d);
    assert_eq!(m["files"][&work], "Work/test1.md");
    assert_eq!(m["files"][&style], "MyStyle/test2.md");
    assert_eq!(m["files"][&doc], "Document/Books/test1.md");

    let rel = |p: &str| markdown::relative_path(p).map(|p| p.to_string_lossy().into_owned());
    assert_eq!(
        rel("NOTE/Note/Work/Meeting.note").unwrap(),
        "Work/Meeting.md"
    );
    assert_eq!(rel("NOTE/Note/Meeting.note").unwrap(), "Meeting.md");
    assert_eq!(rel("NOTE/MyStyle/a.note").unwrap(), "MyStyle/a.md");
    assert_eq!(
        rel("DOCUMENT/Document/x/b.note").unwrap(),
        "Document/x/b.md"
    );
    assert_eq!(rel("EXPORT/c.note").unwrap(), "EXPORT/c.md");
    assert_eq!(rel("SOMETHING/Else/d.note").unwrap(), "SOMETHING/Else/d.md");
    for bad in [
        "",
        "NOTE/Note/../x.note",
        "NOTE/Note/./x.note",
        "/x.note",
        "NOTE//x.note",
    ] {
        assert_eq!(rel(bad), None, "{bad:?}");
    }
}

#[test]
fn a_moved_note_moves_its_file() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let key = d.put("test1", "NOTE/Note/Old");
    // A folder the user made, with a file of their own: it stays.
    std::fs::create_dir_all(out.path().join("Mine")).unwrap();
    std::fs::write(out.path().join("Mine/keep.md"), "mine").unwrap();
    run(&d, with_dir(out.path()), "hello");
    assert!(out.path().join("Old/test1.md").is_file());

    move_note(
        &d,
        &key,
        "NOTE/Note/Old/test1.note",
        "NOTE/Note/Work/Plans.note",
    );
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["Mine/keep.md", "Work/Plans.md"]);
    assert!(
        !out.path().join("Old").exists(),
        "the folder it emptied goes"
    );
    let text = read(&out.path().join("Work/Plans.md"));
    assert!(text.contains("title: \"Plans\""), "{text}");
    assert_eq!(manifest(&d)["files"][&key], "Work/Plans.md");

    // Into the folder of the user: the user's folder and file stay after the next move.
    move_note(
        &d,
        &key,
        "NOTE/Note/Work/Plans.note",
        "NOTE/Note/Mine/Plans.note",
    );
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["Mine/Plans.md", "Mine/keep.md"]);
    move_note(
        &d,
        &key,
        "NOTE/Note/Mine/Plans.note",
        "NOTE/Note/Plans.note",
    );
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["Mine/keep.md", "Plans.md"]);
    assert!(out.path().join("Mine").is_dir());

    // Only the case changes: on a disk that ignores case this is the same file.
    move_note(&d, &key, "NOTE/Note/Plans.note", "NOTE/Note/PLANS.note");
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["Mine/keep.md", "PLANS.md"]);
    assert!(read(&out.path().join("PLANS.md")).contains("hello"));
    assert_eq!(manifest(&d)["files"][&key], "PLANS.md");
}

#[test]
fn a_deleted_note_removes_its_file() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let gone = d.put("test1", "NOTE/Note/Sub");
    d.put("test2", "NOTE/Note");
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["Sub/test1.md", "test2.md"]);

    std::fs::remove_file(d.record_path(&gone)).unwrap();
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["test2.md"]);
    assert!(!out.path().join("Sub").exists());
    assert!(manifest(&d)["files"].get(&gone).is_none());

    // A note that leaves the scope loses its file too.
    let narrow = PipelineConfig {
        ingest_exclude: vec!["NOTE/Note/test2.note".into()],
        ..with_dir(out.path())
    };
    run(&d, narrow, "hello");
    assert!(files(out.path()).is_empty());
    assert_eq!(manifest(&d)["files"], serde_json::json!({}));
    assert!(out.path().is_dir(), "the folder itself stays");
}

#[test]
fn a_note_without_text_gets_no_file() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let key = d.put("test1", "NOTE/Note");
    let engine = FakeEngine::new(Default::default()).with_default("");
    worker(&d, with_dir(out.path()), engine)
        .run_once(true)
        .unwrap();
    assert!(d.store().note(&key).unwrap().is_some(), "the note was read");
    assert!(files(out.path()).is_empty());
    assert!(manifest(&d)["files"].get(&key).is_none());
}

#[test]
fn a_users_file_is_never_overwritten() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("test1.md"), "mine").unwrap();
    std::fs::write(out.path().join("test1 (2).md"), "mine too").unwrap();
    let key = d.put("test1", "NOTE/Note");
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(read(&out.path().join("test1.md")), "mine");
    assert_eq!(read(&out.path().join("test1 (2).md")), "mine too");
    assert!(read(&out.path().join("test1 (3).md")).contains("hello"));
    assert_eq!(manifest(&d)["files"][&key], "test1 (3).md");

    // A move onto a name the user holds takes the next free one, and a later run keeps it.
    std::fs::create_dir_all(out.path().join("Work")).unwrap();
    std::fs::write(out.path().join("Work/Plan.md"), "my plan").unwrap();
    move_note(&d, &key, "NOTE/Note/test1.note", "NOTE/Note/Work/Plan.note");
    run(&d, with_dir(out.path()), "hello");
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(read(&out.path().join("Work/Plan.md")), "my plan");
    assert_eq!(
        files(out.path()),
        [
            "Work/Plan (2).md",
            "Work/Plan.md",
            "test1 (2).md",
            "test1.md"
        ]
    );

    // Deleting the note removes only its own file.
    std::fs::remove_file(d.record_path(&key)).unwrap();
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(
        files(out.path()),
        ["Work/Plan.md", "test1 (2).md", "test1.md"]
    );

    // A manifest that names a path outside the folder is never followed.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim.md"), "safe").unwrap();
    let evil = serde_json::json!({
        "dir": out.path(),
        "files": {
            "F20240424175151947577oMie9HNQYnTz": "../victim.md",
            "F20240424175151947577oMie9HNQYnTy": outside.path().join("victim.md"),
        }
    });
    std::fs::write(manifest_path(&d), evil.to_string()).unwrap();
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(read(&outside.path().join("victim.md")), "safe");
}

#[test]
fn the_manifest_survives_a_restart() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let key = d.put("test1", "NOTE/Note");
    run(&d, with_dir(out.path()), "hello");
    // A new worker (a restart) knows the file is its own: no second copy.
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["test1.md"]);
    assert_eq!(manifest(&d)["files"][&key], "test1.md");

    // A crash after the file was written and before the manifest: the file with the same
    // text is taken back, not copied.
    std::fs::remove_file(manifest_path(&d)).unwrap();
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["test1.md"]);
    assert_eq!(manifest(&d)["files"][&key], "test1.md");

    // The new worker removes the file of a note deleted while it was not running.
    std::fs::remove_file(d.record_path(&key)).unwrap();
    run(&d, with_dir(out.path()), "hello");
    assert!(files(out.path()).is_empty());
}

#[test]
fn a_new_folder_gets_every_note() {
    let d = setup();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let one = d.put("test1", "NOTE/Note");
    let two = d.put("test2", "NOTE/Note/Sub");
    run(&d, with_dir(first.path()), "hello");
    assert_eq!(files(first.path()), ["Sub/test2.md", "test1.md"]);

    // Nothing changed in the store, only the folder.
    let new = second.path().join("Markdown");
    run(&d, with_dir(&new), "hello");
    assert_eq!(files(&new), ["Sub/test2.md", "test1.md"]);
    assert_eq!(
        files(first.path()),
        ["Sub/test2.md", "test1.md"],
        "the old files stay"
    );
    let m = manifest(&d);
    assert_eq!(m["dir"], new.to_str().unwrap());
    assert_eq!(m["files"][&one], "test1.md");
    assert_eq!(m["files"][&two], "Sub/test2.md");
}

#[test]
fn unchanged_text_is_not_written_again() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let one = d.put("test1", "NOTE/Note");
    d.put("test2", "NOTE/Note");
    let mut w = worker(&d, with_dir(out.path()), engine_reading("hello"));
    w.run_once(true).unwrap();
    let file = out.path().join("test1.md");
    let before = read(&file);
    let at = age(&file);

    // A restart, a pass with nothing to do, and a read of every page with the same text.
    w.run_once(true).unwrap();
    run(&d, with_dir(out.path()), "hello");
    worker(&d, with_dir(out.path()), engine_reading("hello"))
        .reocr(None)
        .unwrap();
    assert_eq!(mtime(&file), at, "not written again");
    assert_eq!(read(&file), before);

    // New text is written.
    worker(&d, with_dir(out.path()), engine_reading("new words here"))
        .reocr(Some(&one))
        .unwrap();
    assert!(read(&file).contains("new words here"));
    assert!(read(&out.path().join("test2.md")).contains("hello"));
}

#[test]
fn only_the_changed_notes_are_looked_at() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let one = d.put("test1", "NOTE/Note");
    let two = d.put("test2", "NOTE/Note");
    run(&d, no_settle(), "hello");
    let lock = WriterLock::take(&d.data).unwrap();
    let store = TextStore::open_writer(&d.data, SLUG, &lock).unwrap();
    let mut writer = Writer::new(out.path(), &manifest_path(&d)).unwrap();
    // The first sync of a writer writes every note.
    writer.sync(&store, SLUG, &[]).unwrap();
    assert_eq!(files(out.path()), ["test1.md", "test2.md"]);
    // Gone by hand: only a sync that names the note brings it back.
    std::fs::remove_file(out.path().join("test1.md")).unwrap();
    std::fs::remove_file(out.path().join("test2.md")).unwrap();
    writer
        .sync(&store, SLUG, std::slice::from_ref(&two))
        .unwrap();
    assert_eq!(files(out.path()), ["test2.md"]);
    writer.sync(&store, SLUG, &[one]).unwrap();
    assert_eq!(files(out.path()), ["test1.md", "test2.md"]);
}

/// A second machine: `pipeline.toml` names the server's folder and the settings.
fn two_machines(d: &Data, markdown_dir: Option<&Path>) -> tempfile::TempDir {
    let local = tempfile::tempdir().unwrap();
    let mut toml = format!(
        "[server]\nfolder = {:?}\n[pipeline]\nocr_settle = \"0s\"\n",
        d.data
    );
    if let Some(m) = markdown_dir {
        toml.push_str(&format!("[pipeline.markdown_dirs]\n{SLUG} = {m:?}\n"));
    }
    std::fs::write(local.path().join("pipeline.toml"), toml).unwrap();
    local
}

fn store_manifest(local: &Path, slug: &str) -> serde_json::Value {
    let path = local
        .join("pipeline")
        .join(slug)
        .join(markdown::MANIFEST_FILE);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn the_flag_wins_over_pipeline_toml() {
    let d = setup();
    let toml_dir = tempfile::tempdir().unwrap();
    let flag_dir = tempfile::tempdir().unwrap();
    let key = d.put("test1", "NOTE/Note");
    let local = two_machines(&d, Some(toml_dir.path()));
    let s = Setup::load(local.path()).unwrap();
    assert_eq!(
        s.cfg.markdown_dirs.get(SLUG).map(PathBuf::as_path),
        Some(toml_dir.path())
    );
    let flag = setup::parse_markdown_flag(&format!("{SLUG}={}", flag_dir.path().display()));
    assert_eq!(
        flag,
        (Some(SLUG.to_string()), flag_dir.path().to_path_buf())
    );
    let s = s.with_markdown_dirs(vec![flag]).unwrap();
    assert_eq!(s.markdown_dir(SLUG, 1), Some(flag_dir.path().to_path_buf()));
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(files(flag_dir.path()), ["test1.md"]);
    assert!(files(toml_dir.path()).is_empty());
    assert_eq!(
        store_manifest(local.path(), SLUG)["files"][&key],
        "test1.md"
    );

    // A flag without an account is for the one account served; it wins too.
    let other = tempfile::tempdir().unwrap();
    let flag = setup::parse_markdown_flag(other.path().to_str().unwrap());
    assert_eq!(flag, (None, other.path().to_path_buf()));
    let s = Setup::load(local.path())
        .unwrap()
        .with_markdown_dirs(vec![flag])
        .unwrap();
    assert_eq!(s.markdown_dir(SLUG, 1), Some(other.path().to_path_buf()));
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(files(other.path()), ["test1.md"]);

    // Without the flag, pipeline.toml counts; a relative path is below the local folder.
    let s = Setup::load(local.path())
        .unwrap()
        .with_markdown_dirs(Vec::new())
        .unwrap();
    assert_eq!(s.markdown_dir(SLUG, 1), Some(toml_dir.path().to_path_buf()));
    std::fs::write(
        local.path().join("pipeline.toml"),
        format!(
            "[server]\nfolder = {:?}\n[pipeline.markdown_dirs]\n{SLUG} = \"md\"\n",
            d.data
        ),
    )
    .unwrap();
    let s = Setup::load(local.path()).unwrap();
    assert_eq!(s.markdown_dir(SLUG, 1), Some(local.path().join("md")));
    // An account name that is no slug is refused.
    std::fs::write(
        local.path().join("pipeline.toml"),
        format!(
            "[server]\nfolder = {:?}\n[pipeline.markdown_dirs]\n\"a/b\" = \"md\"\n",
            d.data
        ),
    )
    .unwrap();
    let e = Setup::load(local.path()).err().unwrap().to_string();
    assert!(e.contains("markdown_dirs"), "{e}");
}

#[test]
fn no_flag_no_files() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let local = two_machines(&d, None);
    let s = Setup::load(local.path())
        .unwrap()
        .with_markdown_dirs(Vec::new())
        .unwrap();
    assert_eq!(s.markdown_dir(SLUG, 1), None);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    let store = local.path().join("pipeline").join(SLUG);
    assert!(store.join("notes").is_dir(), "the note was read");
    assert!(!store.join(markdown::MANIFEST_FILE).exists());
    // One machine, the default config: the same.
    run(&d, no_settle(), "hello");
    assert!(!manifest_path(&d).exists());
    assert!(PipelineConfig::default().markdown_dirs.is_empty());
}

#[test]
fn each_account_has_its_own_folder() {
    let d = setup();
    create_account(&d.data, "anna");
    put_into(&d.data, "anna", "test2", "NOTE/Note");
    d.put("test1", "NOTE/Note");
    let mine = tempfile::tempdir().unwrap();
    let hers = tempfile::tempdir().unwrap();
    // Only one account has a folder: the other gets no files.
    run(&d, with_dir(mine.path()), "hello");
    assert_eq!(files(mine.path()), ["test1.md"]);
    assert!(
        !d.data
            .join("pipeline/anna")
            .join(markdown::MANIFEST_FILE)
            .exists()
    );
    let both = PipelineConfig {
        markdown_dirs: [
            (SLUG.to_string(), mine.path().to_path_buf()),
            ("anna".to_string(), hers.path().to_path_buf()),
        ]
        .into(),
        ..no_settle()
    };
    run(&d, both, "hello");
    assert_eq!(files(mine.path()), ["test1.md"]);
    assert_eq!(files(hers.path()), ["test2.md"]);
    assert!(read(&hers.path().join("test2.md")).contains("notedown://anna/"));
}

#[test]
fn a_flag_without_an_account_needs_exactly_one() {
    let d = setup();
    create_account(&d.data, "anna");
    d.put("test1", "NOTE/Note");
    let out = tempfile::tempdir().unwrap();
    let s = crate::worker::one_machine(&d, no_settle());
    let flag = || setup::parse_markdown_flag(out.path().to_str().unwrap());
    // Two flags without an account, or one mixed with named ones, are refused.
    assert!(s.clone().with_markdown_dirs(vec![flag(), flag()]).is_err());
    let named = (Some(SLUG.to_string()), out.path().to_path_buf());
    assert!(
        s.clone()
            .with_markdown_dirs(vec![flag(), named.clone()])
            .is_err()
    );
    assert!(
        s.clone()
            .with_markdown_dirs(vec![named.clone(), named])
            .is_err()
    );
    let s = s.with_markdown_dirs(vec![flag()]).unwrap();
    let e = s
        .check_markdown(&["anna".to_string(), SLUG.to_string()])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("anna") && e.contains(SLUG) && e.contains("SLUG=DIR"),
        "{e}"
    );
    assert!(s.check_markdown(&[SLUG.to_string()]).is_ok());
    // A worker that serves two accounts writes nothing and says why, once.
    let said = Said::default();
    let mut w = Worker::new(&s, Box::new(engine_reading("hello"))).with_messages(said.sink());
    w.run_once(true).unwrap();
    w.run_once(true).unwrap();
    assert!(files(out.path()).is_empty());
    assert_eq!(said.count("SLUG=DIR"), 1, "{:?}", said.lines());
}

#[test]
fn the_files_are_kept_while_the_model_is_missing() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let read = d.put("test1", "NOTE/Note");
    run(&d, no_settle(), "hello");
    // Turned on while a new note waits for a model that is not there yet.
    d.put("test2", "NOTE/Note");
    let missing: notedown_pipeline::worker::EngineLoader = Box::new(|| {
        Err(notedown_ocr::OcrError::ModelMissing {
            path: PathBuf::from("no-model"),
        })
    });
    let s = crate::worker::one_machine(&d, with_dir(out.path()));
    let r = Worker::with_loader(&s, missing).run_once(true);
    assert!(
        matches!(r, Err(notedown_pipeline::Error::ModelMissing)),
        "{r:?}"
    );
    assert_eq!(files(out.path()), ["test1.md"]);
    assert_eq!(manifest(&d)["files"][&read], "test1.md");
}

/// A note state with one page of `text`, as the worker leaves it.
fn note_state(key: &str, path: &str, text: &str) -> NoteState {
    NoteState {
        key: key.into(),
        path: path.into(),
        title: notedown_pipeline::model::title_of(path),
        pages: vec![PageState {
            page_id: "P1".into(),
            seq: Some(1),
            state: PageOcrState::Done,
            blocks: vec![Block {
                id: "b1".into(),
                label: "text".into(),
                bbox: [0, 0, 10, 10],
                text: text.into(),
                replaces: Vec::new(),
            }],
            ..PageState::default()
        }],
        ..NoteState::default()
    }
}

const LOW: &str = "path:00000000000000000000000000000001";
const HIGH: &str = "path:ffffffffffffffffffffffffffffffff";
const MID: &str = "path:88888888888888888888888888888888";

/// A store of its own (no server) and a folder for the writer.
struct Hand {
    _dir: tempfile::TempDir,
    data: PathBuf,
    out: PathBuf,
    lock: WriterLock,
    store: TextStore,
}

impl Hand {
    fn new() -> Hand {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        let out = data.join("md");
        let lock = WriterLock::take(&data).unwrap();
        let store = TextStore::open_writer(&data, SLUG, &lock).unwrap();
        Hand {
            _dir: dir,
            data,
            out,
            lock,
            store,
        }
    }
    fn save(&self, note: &NoteState) {
        self.store.save_note(&self.lock, note).unwrap();
    }
    fn writer(&self) -> Writer {
        Writer::new(&self.out, &self.store.dir().join(markdown::MANIFEST_FILE)).unwrap()
    }
    fn manifest(&self) -> serde_json::Value {
        let path = self
            .data
            .join("pipeline")
            .join(SLUG)
            .join(markdown::MANIFEST_FILE);
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
}

#[test]
fn a_rekeyed_note_keeps_its_name() {
    // A note keyed by its path gets a new key that sorts before the old one, at a path that
    // gives the same file name.
    let h = Hand::new();
    h.save(&note_state(HIGH, "NOTE/Note/MyStyle/x.note", "hello"));
    let mut w = h.writer();
    w.sync(&h.store, SLUG, &[]).unwrap();
    assert_eq!(files(&h.out), ["MyStyle/x.md"]);
    h.save(&note_state(LOW, "NOTE/MyStyle/x.note", "hello"));
    h.store.remove_note(&h.lock, HIGH).unwrap();
    w.sync(&h.store, SLUG, &h.store.take_touched()).unwrap();
    assert_eq!(files(&h.out), ["MyStyle/x.md"]);
    assert_eq!(h.manifest()["files"][LOW], "MyStyle/x.md");
    assert!(h.manifest()["files"].get(HIGH).is_none());
}

#[test]
fn a_name_freed_in_the_same_pass_is_taken() {
    for restart in [false, true] {
        let h = Hand::new();
        h.save(&note_state(LOW, "NOTE/Note/a.note", "note a"));
        h.save(&note_state(MID, "NOTE/Note/b.note", "note b"));
        h.save(&note_state(HIGH, "NOTE/Note/c.note", "note c"));
        let mut w = h.writer();
        w.sync(&h.store, SLUG, &[]).unwrap();
        assert_eq!(files(&h.out), ["a.md", "b.md", "c.md"]);
        // B is deleted and A is renamed onto B's name.
        let mut b = note_state(MID, "NOTE/Note/b.note", "note b");
        b.deleted = true;
        h.save(&b);
        h.save(&note_state(LOW, "NOTE/Note/b.note", "note a"));
        let changed = h.store.take_touched();
        if restart {
            w = h.writer();
            w.sync(&h.store, SLUG, &[]).unwrap();
        } else {
            w.sync(&h.store, SLUG, &changed).unwrap();
        }
        assert_eq!(files(&h.out), ["b.md", "c.md"], "restart {restart}");
        assert!(read(&h.out.join("b.md")).contains("note a"));
        assert_eq!(h.manifest()["files"][LOW], "b.md");

        // A swap: A takes C's name and C takes A's.
        h.save(&note_state(LOW, "NOTE/Note/c.note", "note a"));
        h.save(&note_state(HIGH, "NOTE/Note/b.note", "note c"));
        let changed = h.store.take_touched();
        if restart {
            w = h.writer();
            w.sync(&h.store, SLUG, &[]).unwrap();
        } else {
            w.sync(&h.store, SLUG, &changed).unwrap();
        }
        assert_eq!(files(&h.out), ["b.md", "c.md"], "restart {restart}");
        assert!(read(&h.out.join("c.md")).contains("note a"));
        assert!(read(&h.out.join("b.md")).contains("note c"));
    }
}

#[test]
fn reocr_skips_a_store_that_fails_the_owner_check() {
    let d = setup();
    let local = tempfile::tempdir().unwrap();
    point_at(local.path(), &d.data);
    d.put("test1", "NOTE/Note");
    Worker::new(&load(local.path()), Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    // Text without an owner on two machines: the account is not read, nor written.
    std::fs::remove_file(store_dir(local.path(), SLUG).join("owner.json")).unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut s = load(local.path());
    s.cfg = with_dir(out.path());
    let _ = Worker::new(&s, Box::new(engine_reading("hello")))
        .with_messages(Said::default().sink())
        .reocr(None);
    assert!(files(out.path()).is_empty());
    assert!(
        !store_dir(local.path(), SLUG)
            .join(markdown::MANIFEST_FILE)
            .exists()
    );
}

#[test]
fn a_new_owner_removes_the_old_files() {
    let d = setup();
    let local = tempfile::tempdir().unwrap();
    point_at(local.path(), &d.data);
    d.put("test1", "NOTE/Note/Sub");
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("mine.md"), "mine").unwrap();
    let mut s = load(local.path());
    s.cfg = with_dir(out.path());
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(files(out.path()), ["Sub/test1.md", "mine.md"]);
    // The slug is given to a new account: the old text and its files go.
    remove_account(&d.data, SLUG);
    create_account(&d.data, SLUG);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .with_messages(Said::default().sink())
        .run_once(true)
        .unwrap();
    assert_eq!(files(out.path()), ["mine.md"]);
    assert!(!out.path().join("Sub").exists());
}

#[test]
fn a_removed_account_loses_its_files() {
    let d = setup();
    let local = tempfile::tempdir().unwrap();
    point_at(local.path(), &d.data);
    d.put("test1", "NOTE/Note/Sub");
    let out = tempfile::tempdir().unwrap();
    let mut s = load(local.path());
    s.cfg = with_dir(out.path());
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(files(out.path()), ["Sub/test1.md"]);
    remove_account(&d.data, SLUG);
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(
        !store_dir(local.path(), SLUG).exists(),
        "{:?}",
        said.lines()
    );
    assert!(files(out.path()).is_empty());
    assert!(!out.path().join("Sub").exists());
    assert!(out.path().is_dir(), "the folder itself stays");
}

#[test]
fn a_long_name_is_cut() {
    for stem in ["a".repeat(300), "ä".repeat(300), "😀".repeat(100)] {
        let rel = markdown::relative_path(&format!("NOTE/Note/{stem}.note")).unwrap();
        let name = rel.to_str().unwrap();
        let cut = name.strip_suffix(".md").unwrap();
        assert!(stem.starts_with(cut));
        // The highest numbered name fits too, also as the temp file of its write.
        let temp = format!(".{cut} (10000).md.{}.{}.tmp", u32::MAX, u64::MAX);
        assert!(temp.len() <= 255, "{}", temp.len());
    }
    // Real files on disk: each long title gets its file, a second note its numbered one.
    let h = Hand::new();
    let titles = ["a".repeat(300), "ä".repeat(300)];
    let keys = [[LOW, MID], [HIGH, "path:44444444444444444444444444444444"]];
    for (title, [first, second]) in titles.iter().zip(keys) {
        h.save(&note_state(
            first,
            &format!("NOTE/Note/{title}.note"),
            "one",
        ));
        h.save(&note_state(
            second,
            &format!("NOTE/Note/{title}x.note"),
            "two",
        ));
    }
    h.writer().sync(&h.store, SLUG, &[]).unwrap();
    let names = files(&h.out);
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.iter().all(|n| n.len() <= 255), "{names:?}");
    assert_eq!(
        names.iter().filter(|n| n.ends_with(" (2).md")).count(),
        2,
        "{names:?}"
    );
    for name in &names {
        assert!(read(&h.out.join(name)).contains("<!-- notedown://"));
    }
}

#[test]
fn a_flag_without_an_account_counts_an_account_being_removed() {
    use notedown_cloud::journal::{Event, EventBody, Journal, Origin, new_op_id};
    let d = setup();
    let local = tempfile::tempdir().unwrap();
    point_at(local.path(), &d.data);
    create_account(&d.data, "anna");
    d.put("test1", "NOTE/Note");
    let out = tempfile::tempdir().unwrap();
    let s = load(local.path())
        .with_markdown_dirs(vec![(None, out.path().to_path_buf())])
        .unwrap();
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert!(files(out.path()).is_empty());
    // A removal record of a live account (a crash between the record and the removal): the
    // account waits a turn, but it is still served.
    Journal::open(&d.data)
        .unwrap()
        .append(&mut [Event::new(
            "anna",
            Origin::Cli,
            None,
            &new_op_id(),
            EventBody::AccountRemoved {
                user_id: crate::split::SECOND_ID,
            },
        )])
        .unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert!(files(out.path()).is_empty(), "{:?}", said.lines());
}

#[test]
fn a_folder_for_an_account_not_served_is_said_once() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let mine = tempfile::tempdir().unwrap();
    let ghost = tempfile::tempdir().unwrap();
    let cfg = PipelineConfig {
        markdown_dirs: [
            (SLUG.to_string(), mine.path().to_path_buf()),
            ("ghost".to_string(), ghost.path().to_path_buf()),
        ]
        .into(),
        ..no_settle()
    };
    let said = Said::default();
    let mut w = worker(&d, cfg, engine_reading("hello")).with_messages(said.sink());
    w.run_once(true).unwrap();
    w.run_once(true).unwrap();
    assert_eq!(files(mine.path()), ["test1.md"]);
    assert!(files(ghost.path()).is_empty());
    assert_eq!(said.count("ghost"), 1, "{:?}", said.lines());
}

#[test]
fn leftover_temp_files_are_removed() {
    let h = Hand::new();
    h.save(&note_state(LOW, "NOTE/Note/Sub/a.note", "hello"));
    // A folder the user made: the writer did not create it, but its own file is in it.
    std::fs::create_dir_all(h.out.join("Mine")).unwrap();
    h.save(&note_state(MID, "NOTE/Note/Mine/b.note", "hello"));
    h.writer().sync(&h.store, SLUG, &[]).unwrap();
    // A crash in a write leaves its temp file; a user's own files stay.
    std::fs::write(h.out.join(".a.md.123.4.tmp"), "x").unwrap();
    std::fs::write(h.out.join("Sub/.a.md.99.0.tmp"), "x").unwrap();
    std::fs::write(h.out.join("Mine/.b.md.7.8.tmp"), "x").unwrap();
    std::fs::write(h.out.join(".notes.tmp"), "mine").unwrap();
    std::fs::write(h.out.join("Sub/.a.md.tmp"), "mine").unwrap();
    h.writer().sync(&h.store, SLUG, &[]).unwrap();
    assert_eq!(
        files(&h.out),
        [".notes.tmp", "Mine/b.md", "Sub/.a.md.tmp", "Sub/a.md"]
    );
}

#[test]
fn a_note_back_in_scope_gets_its_file_again() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    d.put("test1", "NOTE/Note");
    run(&d, with_dir(out.path()), "hello");
    let narrow = PipelineConfig {
        ingest_exclude: vec!["NOTE/Note/test1.note".into()],
        ..with_dir(out.path())
    };
    let mut w = worker(&d, narrow, engine_reading("hello"));
    w.run_once(true).unwrap();
    assert!(files(out.path()).is_empty());
    run(&d, with_dir(out.path()), "hello");
    assert_eq!(files(out.path()), ["test1.md"]);
}

#[test]
fn a_folder_inside_the_data_folder_is_refused() {
    let d = setup();
    let out = tempfile::tempdir().unwrap();
    let named = |dir: PathBuf| vec![(Some(SLUG.to_string()), dir)];
    let refused = |s: &Setup, flags: Vec<(Option<String>, PathBuf)>| {
        let e = s.clone().with_markdown_dirs(flags).err().expect("refused");
        e.to_string()
    };
    // One machine: the data folder is the server's and the pipeline's.
    let s = crate::worker::one_machine(&d, no_settle());
    for dir in [
        d.data.clone(),
        d.data.join("md"),
        d.data.join("pipeline"),
        d.data.join("accounts").join(SLUG).join("files"),
        d.data
            .join("..")
            .join(d.data.file_name().unwrap())
            .join("x"),
    ] {
        let e = refused(&s, named(dir.clone()));
        assert!(
            e.contains("inside") && e.contains("--markdown-dir"),
            "{dir:?}: {e}"
        );
    }
    let e = refused(&s, vec![(None, d.data.join("md"))]);
    assert!(e.contains("inside"), "{e}");
    // From the config too.
    let s = crate::worker::one_machine(&d, with_dir(&d.data.join("md")));
    let e = refused(&s, Vec::new());
    assert!(e.contains("inside") && e.contains("markdown_dirs"), "{e}");
    // A folder next to it, with a name that starts the same, is fine.
    let s = crate::worker::one_machine(&d, no_settle());
    let mut next_to = d.data.clone().into_os_string();
    next_to.push("-md");
    assert!(s.clone().with_markdown_dirs(named(next_to.into())).is_ok());
    assert!(
        s.with_markdown_dirs(named(out.path().to_path_buf()))
            .is_ok()
    );

    // Two machines: the server's folder and the pipeline's own `pipeline/` are refused, the
    // local folder itself is not the server's.
    let local = two_machines(&d, None);
    let s = Setup::load(local.path()).unwrap();
    for dir in [d.data.join("md"), local.path().join("pipeline").join("md")] {
        let e = refused(&s, named(dir.clone()));
        assert!(e.contains("inside"), "{dir:?}: {e}");
    }
    assert!(s.with_markdown_dirs(named(local.path().join("md"))).is_ok());
}

/// A folder source that leaves account `SLUG` out of its list while `out` is set, like a REST
/// source whose probe of that account's token failed while the others answered.
struct LeavesOut {
    inner: notedown_pipeline::source::FolderSource,
    out: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl notedown_pipeline::source::ServerData for LeavesOut {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn accounts(&self) -> notedown_pipeline::Result<Vec<notedown_pipeline::source::ServerAccount>> {
        let mut all = self.inner.accounts()?;
        if self.out.load(std::sync::atomic::Ordering::SeqCst) {
            all.retain(|a| a.slug != SLUG);
        }
        Ok(all)
    }
    fn records(
        &self,
        slug: &str,
    ) -> notedown_pipeline::Result<notedown_pipeline::records::Records> {
        self.inner.records(slug)
    }
    fn record(
        &self,
        slug: &str,
        key: &str,
    ) -> notedown_pipeline::Result<Option<notedown_pipeline::records::Record>> {
        self.inner.record(slug, key)
    }
    fn note(
        &self,
        slug: &str,
        path: &str,
    ) -> notedown_pipeline::Result<notedown_pipeline::source::NoteBytes> {
        self.inner.note(slug, path)
    }
    fn note_exists(&self, slug: &str, path: &str) -> notedown_pipeline::Result<Option<bool>> {
        self.inner.note_exists(slug, path)
    }
    fn digests(
        &self,
        slug: &str,
    ) -> notedown_pipeline::Result<Vec<notedown_cloud::appdata::Digest>> {
        self.inner.digests(slug)
    }
    fn journal(
        &self,
        slug: &str,
        from: u64,
        limit: usize,
    ) -> notedown_pipeline::Result<notedown_pipeline::source::JournalBatch> {
        self.inner.journal(slug, from, limit)
    }
    fn knows_gone(&self, slug: &str) -> bool {
        self.inner.knows_gone(slug)
    }
}

/// Only the first sync of a writer looks at every note: a file removed by hand comes back
/// only then. A source error and its recovery keep the writer, so the file stays away.
#[test]
fn a_source_error_does_not_cause_a_second_full_look() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = setup();
    create_account(&d.data, "anna");
    d.put("test1", "NOTE/Note");
    let out = tempfile::tempdir().unwrap();
    let left_out = Arc::new(AtomicBool::new(false));
    let mut s = crate::worker::one_machine(&d, with_dir(out.path()));
    s.source = Arc::new(LeavesOut {
        inner: notedown_pipeline::source::FolderSource::new(&d.data),
        out: left_out.clone(),
    });
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(files(out.path()), ["test1.md"]);
    std::fs::remove_file(out.path().join("test1.md")).unwrap();
    // The account is left out of one list, then back.
    left_out.store(true, Ordering::SeqCst);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    left_out.store(false, Ordering::SeqCst);
    d.put("test2", "NOTE/Note");
    // The test clock started before the note was put: past its change time, so it is due.
    clock.sleep(Duration::from_secs(60));
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(files(out.path()), ["test2.md"], "{:?}", said.lines());
}

/// A pass writes the file of each note as soon as the note is read, so a pass that stops
/// (or a process that is killed) already left the files of the notes it finished.
#[test]
fn a_stopped_pass_has_the_files_of_the_notes_it_finished() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    d.put("test2", "NOTE/Note");
    let out = tempfile::tempdir().unwrap();
    let s = crate::worker::one_machine(&d, with_dir(out.path()));
    let said = Said::default();
    let (mut w, _clock) = continuous(&s, &said);
    let dir = out.path().to_path_buf();
    // Stops as soon as the first file is there: before the second note is read.
    w.run(move || !files(&dir).is_empty()).unwrap();
    assert_eq!(files(out.path()).len(), 1, "{:?}", files(out.path()));
}
