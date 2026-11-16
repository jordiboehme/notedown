//! The Markdown files: one `.md` per note with text in a folder the user picks, kept by the
//! worker after each pass.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notedown_pipeline::PipelineConfig;
use notedown_pipeline::markdown::{self, Writer};
use notedown_pipeline::setup::Setup;
use notedown_pipeline::store::WriterLock;
use notedown_pipeline::worker::Worker;

use crate::fake_engine::FakeEngine;
use crate::worker::{Data, SLUG, engine_reading, no_settle, setup};

fn with_dir(dir: &Path) -> PipelineConfig {
    PipelineConfig {
        markdown_dir: Some(dir.to_path_buf()),
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
    let store = notedown_pipeline::store::TextStore::open_writer(&d.data, SLUG, &lock).unwrap();
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
        toml.push_str(&format!("markdown_dir = {m:?}\n"));
    }
    std::fs::write(local.path().join("pipeline.toml"), toml).unwrap();
    local
}

#[test]
fn the_flag_wins_over_pipeline_toml() {
    let d = setup();
    let toml_dir = tempfile::tempdir().unwrap();
    let flag_dir = tempfile::tempdir().unwrap();
    let key = d.put("test1", "NOTE/Note");
    let local = two_machines(&d, Some(toml_dir.path()));
    let s = Setup::load(local.path()).unwrap();
    assert_eq!(s.cfg.markdown_dir.as_deref(), Some(toml_dir.path()));
    let s = s.with_markdown_dir(Some(flag_dir.path().to_path_buf()));
    assert_eq!(s.cfg.markdown_dir.as_deref(), Some(flag_dir.path()));
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(files(flag_dir.path()), ["test1.md"]);
    assert!(files(toml_dir.path()).is_empty());
    let m: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            local
                .path()
                .join("pipeline")
                .join(SLUG)
                .join(markdown::MANIFEST_FILE),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(m["files"][&key], "test1.md");

    // Without the flag, pipeline.toml counts; a relative path is below the local folder.
    let s = Setup::load(local.path()).unwrap().with_markdown_dir(None);
    assert_eq!(s.cfg.markdown_dir.as_deref(), Some(toml_dir.path()));
    std::fs::write(
        local.path().join("pipeline.toml"),
        format!(
            "[server]\nfolder = {:?}\n[pipeline]\nmarkdown_dir = \"md\"\n",
            d.data
        ),
    )
    .unwrap();
    let s = Setup::load(local.path()).unwrap();
    assert_eq!(s.cfg.markdown_dir, Some(local.path().join("md")));
}

#[test]
fn no_flag_no_files() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let local = two_machines(&d, None);
    let s = Setup::load(local.path()).unwrap().with_markdown_dir(None);
    assert_eq!(s.cfg.markdown_dir, None);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    let store = local.path().join("pipeline").join(SLUG);
    assert!(store.join("notes").is_dir(), "the note was read");
    assert!(!store.join(markdown::MANIFEST_FILE).exists());
    // One machine, the default config: the same.
    run(&d, no_settle(), "hello");
    assert!(!manifest_path(&d).exists());
    assert_eq!(PipelineConfig::default().markdown_dir, None);
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
