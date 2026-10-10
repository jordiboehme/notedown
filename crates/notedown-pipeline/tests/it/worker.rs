//! The OCR worker over a real data folder: accounts, notes put through notedown-cloud (so the
//! page records are the server's own), the fake engine and a test clock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Journal;
use notedown_cloud::pages::{PageFingerprint, PageRecord};
use notedown_ocr::{OcrEngine, OcrError};
use notedown_pipeline::PipelineConfig;
use notedown_pipeline::model::{LineChange, LogLine, NoteState, PageOcrState};
use notedown_pipeline::store::TextStore;
use notedown_pipeline::worker::{Clock, Worker};

use crate::fake_engine::{FakeEngine, text_block};

pub(crate) const SLUG: &str = "jordi";
const HOUR: u64 = 3_600_000;

pub(crate) struct Data {
    _dir: tempfile::TempDir,
    pub(crate) data: PathBuf,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub(crate) fn setup() -> Data {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    Accounts::new(&data)
        .create(SLUG, "jordi@example.org", "secret")
        .unwrap();
    Data { _dir: dir, data }
}

impl Data {
    fn pages_dir(&self) -> PathBuf {
        self.data.join("accounts").join(SLUG).join("pages")
    }

    /// Puts fixture `name` into `folder` with notedown-cloud; returns the note key.
    pub(crate) fn put(&self, name: &str, folder: &str) -> String {
        let journal = Journal::open(&self.data).unwrap();
        let src = root().join(format!("tests/fixtures/notes/{name}.note"));
        notedown_cloud::put::put(&self.data, SLUG, &journal, &src, folder, None).unwrap();
        let path = format!("{folder}/{name}.note");
        self.records()
            .into_iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("no page record for {path}"))
            .file_id
    }

    fn records(&self) -> Vec<PageRecord> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.pages_dir()).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "json") {
                out.push(serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap());
            }
        }
        out
    }

    fn record_path(&self, key: &str) -> PathBuf {
        self.pages_dir()
            .join(format!("{}.json", key.replace("path:", "path-")))
    }

    /// Changes the page record of `key` as the server would after a sync, without a journal
    /// event.
    pub(crate) fn edit_record(&self, key: &str, f: impl FnOnce(&mut PageRecord)) {
        let path = self.record_path(key);
        let mut rec: PageRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        f(&mut rec);
        std::fs::write(&path, serde_json::to_vec_pretty(&rec).unwrap()).unwrap();
    }

    /// Sets the time of the last page change of `key` to `ago` before now.
    fn backdate(&self, key: &str, ago: Duration) {
        let f = std::fs::File::options()
            .write(true)
            .open(self.record_path(key))
            .unwrap();
        f.set_modified(SystemTime::now() - ago).unwrap();
    }

    pub(crate) fn store(&self) -> TextStore {
        TextStore::open(&self.data, SLUG).unwrap()
    }

    fn log(&self) -> Vec<LogLine> {
        let mut out = Vec::new();
        self.store()
            .read_log(0, None, &mut |l| {
                out.push(l);
                Ok(())
            })
            .unwrap();
        out
    }

    fn state(&self, key: &str) -> Option<NoteState> {
        self.store().note(key).unwrap()
    }

    fn errors(&self) -> Vec<serde_json::Value> {
        let path = self.data.join("pipeline").join(SLUG).join("errors.jsonl");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// A fingerprint like `fp` with other part hashes; `total` is computed again.
pub(crate) fn with_parts(
    fp: &PageFingerprint,
    strokes: Option<&str>,
    layer: Option<&str>,
    template: Option<&str>,
) -> PageFingerprint {
    let mut layers = fp.layers.clone();
    if let Some(l) = layer {
        let first = layers.keys().next().cloned().unwrap_or_else(|| "L".into());
        layers.insert(first, l.to_string());
    }
    PageFingerprint::new(
        &fp.page_id,
        fp.position,
        strokes.unwrap_or(&fp.strokes),
        layers,
        template.unwrap_or(&fp.template),
        &fp.textboxes,
        &fp.recognition,
    )
}

pub(crate) fn engine_reading(text: &str) -> FakeEngine {
    FakeEngine::new(HashMap::new()).with_default(&text_block([100, 100, 900, 200], text))
}

pub(crate) fn worker(d: &Data, cfg: PipelineConfig, engine: FakeEngine) -> Worker {
    Worker::new(&d.data, cfg, Box::new(engine))
}

pub(crate) fn no_settle() -> PipelineConfig {
    PipelineConfig {
        ocr_settle: Duration::ZERO,
        ..PipelineConfig::default()
    }
}

/// A clock the test sets; `sleep` moves it on.
#[derive(Clone)]
struct TestClock(Arc<AtomicU64>);

impl TestClock {
    fn at(ms: u64) -> TestClock {
        TestClock(Arc::new(AtomicU64::new(ms)))
    }
    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    fn sleep(&self, d: Duration) {
        self.0.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Every entry below `dir` with its size and mtime.
fn tree(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let m = e.metadata().unwrap();
        out.push((e.path(), m.len(), m.modified().unwrap()));
        if m.is_dir() {
            out.extend(tree(&e.path()));
        }
    }
    out.sort();
    out
}

fn texts(line: &LogLine) -> Vec<&str> {
    line.blocks.iter().map(|b| b.text.as_str()).collect()
}

#[test]
fn a_new_note_is_read_and_logged() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let engine = engine_reading("hello world");
    let calls = engine.calls_handle();
    let accounts = d.data.join("accounts");
    let before = tree(&accounts);
    let progress = worker(&d, no_settle(), engine).run_once(false).unwrap();
    assert_eq!(
        tree(&accounts),
        before,
        "nothing below accounts/ is written"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(progress.pages_read, 1);
    assert_eq!(progress.versions, 1);

    let log = d.log();
    assert_eq!(log.len(), 1, "{log:?}");
    let line = &log[0];
    let rec = d.records().into_iter().find(|r| r.file_id == key).unwrap();
    assert_eq!(line.note, key);
    assert_eq!(line.change, LineChange::New);
    assert_eq!(line.page_id.as_deref(), Some(rec.pages[0].page_id.as_str()));
    assert_eq!(line.page_hash.as_deref(), Some(rec.pages[0].total.as_str()));
    assert_eq!(line.model.as_deref(), Some("fake-engine"));
    assert_eq!(line.path.as_deref(), Some("NOTE/Note/test1.note"));
    assert_eq!(line.position, Some(0));
    assert_eq!(texts(line), ["hello world"]);
    assert_eq!(line.blocks[0].id, "b1");

    let state = d.state(&key).unwrap();
    assert_eq!(state.path, "NOTE/Note/test1.note");
    assert_eq!(state.title, "test1");
    assert_eq!(state.md5, rec.md5);
    assert_eq!(state.size, rec.size);
    let page = &state.pages[0];
    assert_eq!(page.state, PageOcrState::Done);
    assert_eq!(page.seq, Some(line.seq));
    assert_eq!(page.strokes_hash, rec.pages[0].strokes);
    assert_eq!(page.layers_hash, rec.pages[0].layers_hash());
    assert_eq!(page.page_hash, rec.pages[0].total);
    assert!(!page.stroke_list.is_empty());
    assert_eq!(page.blocks, line.blocks);
    assert!(d.store().stats().unwrap().last_run_ms > 0);

    // Nothing changed: a second run reads nothing and writes nothing.
    let engine = engine_reading("hello world");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(d.log().len(), 1);
}

#[test]
fn a_template_change_is_not_ocrd() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, None, Some("0123456789abcdef"));
    });
    let total = d.records()[0].pages[0].total.clone();
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no OCR for a template change"
    );
    assert_eq!(d.log().len(), 1, "no line");
    let page = &d.state(&key).unwrap().pages[0];
    assert_eq!(page.page_hash, total, "only the hash is updated");
    assert_eq!(page.state, PageOcrState::Done);
}

#[test]
fn a_layer_change_reads_the_whole_page() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, Some("fedcba9876543210"), None);
    });
    // The strokes are the same, so only the whole page as the region takes the new reading.
    let engine = engine_reading("goodbye moon");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].change, LineChange::Edited);
    assert_eq!(log[1].signal, ["layers"]);
    assert_eq!(texts(&log[1]), ["goodbye moon"]);
    let state = d.state(&key).unwrap();
    assert_eq!(
        state.pages[0].layers_hash,
        d.records()[0].pages[0].layers_hash()
    );
    assert_eq!(state.pages[0].seq, Some(log[1].seq));
}

#[test]
fn reocr_writes_new_versions() {
    let d = setup();
    let one = d.put("test1", "NOTE/Note");
    let two = d.put("test2", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    assert_eq!(d.log().len(), 2);

    // The same reading: nothing to write.
    let p = worker(&d, no_settle(), engine_reading("hello"))
        .reocr(None)
        .unwrap();
    assert_eq!(p.pages_read, 2);
    assert_eq!(d.log().len(), 2);

    // A better model reads one note again.
    let p = worker(&d, no_settle(), engine_reading("hello there friend"))
        .reocr(Some(&one))
        .unwrap();
    assert_eq!(p.pages_read, 1);
    let log = d.log();
    assert_eq!(log.len(), 3);
    assert_eq!(log[2].note, one);
    assert_eq!(log[2].change, LineChange::Edited);
    assert_eq!(texts(&log[2]), ["hello there friend"]);

    // Every note in scope, also by path.
    worker(&d, no_settle(), engine_reading("hello there friend"))
        .reocr(None)
        .unwrap();
    let log = d.log();
    assert_eq!(log.len(), 4);
    assert_eq!(log[3].note, two);
    assert!(
        worker(&d, no_settle(), engine_reading("x"))
            .reocr(Some("NOTE/Note/missing.note"))
            .is_err()
    );
    let p = worker(&d, no_settle(), engine_reading("by path"))
        .reocr(Some("NOTE/Note/test2.note"))
        .unwrap();
    assert_eq!(p.pages_read, 1);
}

#[test]
fn unchanged_text_writes_no_line() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, Some("fedcba9876543210"), None);
    });
    // Reading noise only: the same text after normalization.
    let engine = engine_reading("hello ");
    let calls = engine.calls_handle();
    let p = worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(p.versions, 0);
    assert_eq!(d.log().len(), 1);
    let state = d.state(&key).unwrap();
    assert_eq!(
        state.pages[0].layers_hash,
        d.records()[0].pages[0].layers_hash()
    );
    assert_eq!(state.pages[0].blocks[0].text, "hello");
}

#[test]
fn the_settle_time_waits() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    d.backdate(&key, Duration::from_secs(5 * 60));
    let cfg = PipelineConfig::default();
    assert_eq!(cfg.ocr_settle, Duration::from_secs(600));
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    let mut w = worker(&d, cfg.clone(), engine);
    let p = w.run_once(false).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "still settling");
    assert_eq!(p.waiting, 1);
    let next = p.next_due.unwrap();
    let since = now_ms() - 5 * 60_000;
    assert!(next.abs_diff(since + 600_000) < 60_000, "{next}");
    assert!(d.log().is_empty());

    let p = w.run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(p.pages_read, 1);
    assert_eq!(d.log().len(), 1);

    // Settled long ago: read without ignore_settle.
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    d.backdate(&key, Duration::from_secs(11 * 60));
    let p = worker(&d, cfg, engine_reading("hello"))
        .run_once(false)
        .unwrap();
    assert_eq!(p.pages_read, 1);
}

#[test]
fn a_failing_page_is_retried_then_failed() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let t0 = now_ms();
    let clock = TestClock::at(t0);
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    engine.fail_next(3);
    let mut w = worker(&d, no_settle(), engine).with_clock(Arc::new(clock.clone()));

    let p = w.run_once(true).unwrap();
    assert_eq!(p.failed, 1);
    let errors = d.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let e = &errors[0];
    assert_eq!(e["note"], key.as_str());
    assert_eq!(e["tries"], 1);
    assert_eq!(e["nextTry"], t0 + HOUR);
    assert!(e["pageId"].is_string());
    assert!(
        e["lastError"]
            .as_str()
            .unwrap()
            .contains("fake engine failure")
    );
    assert_eq!(d.state(&key).unwrap().pages[0].state, PageOcrState::Pending);

    // Not before the hour is over.
    clock.set(t0 + HOUR / 2);
    w.run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    clock.set(t0 + HOUR);
    w.run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(d.errors()[0]["tries"], 2);

    clock.set(t0 + 2 * HOUR);
    w.run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(d.errors()[0]["tries"], 3);
    let state = d.state(&key).unwrap();
    assert_eq!(state.pages[0].state, PageOcrState::Failed);
    assert!(d.log().is_empty());

    // Failed: no more tries.
    clock.set(t0 + 10 * HOUR);
    w.run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    // A new change of the page tries again from the start.
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], Some("0123456789abcdef"), None, None);
    });
    w.run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(d.state(&key).unwrap().pages[0].state, PageOcrState::Done);
    assert!(d.errors().is_empty());
    assert_eq!(d.log().len(), 1);
}

#[test]
fn a_truncated_page_keeps_its_text() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("cut").truncated(true))
        .run_once(true)
        .unwrap();
    let state = d.state(&key).unwrap();
    assert_eq!(state.pages[0].state, PageOcrState::Truncated);
    assert_eq!(state.pages[0].blocks[0].text, "cut");
}

#[test]
fn leaving_the_scope_gives_unscoped() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let never = d.put("test2", "NOTE/Note");
    let first = PipelineConfig {
        ingest_exclude: vec!["NOTE/Note/test2.note".into()],
        ..no_settle()
    };
    worker(&d, first, engine_reading("hello"))
        .run_once(true)
        .unwrap();
    assert_eq!(d.log().len(), 1);
    assert!(d.state(&never).is_none(), "out of scope: no state");

    let narrow = PipelineConfig {
        ingest_folders: vec!["DOCUMENT/**".into()],
        ..no_settle()
    };
    for _ in 0..2 {
        let engine = engine_reading("hello");
        let calls = engine.calls_handle();
        worker(&d, narrow.clone(), engine).run_once(true).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    let log = d.log();
    let unscoped: Vec<_> = log
        .iter()
        .filter(|l| l.change == LineChange::NoteUnscoped)
        .collect();
    assert_eq!(unscoped.len(), 1, "once: {log:?}");
    assert_eq!(unscoped[0].note, key);
    assert_eq!(unscoped[0].path, None, "no destination path");
    assert!(log.iter().all(|l| l.change != LineChange::NoteDeleted));
    assert!(
        log.iter()
            .all(|l| l.note != never || l.change != LineChange::NoteUnscoped),
        "a note without text leaves no line"
    );
    let state = d.state(&key).unwrap();
    assert!(!state.deleted, "narrowing the scope never reads as deleted");
    assert!(state.unscoped);
    assert_eq!(state.pages[0].blocks[0].text, "hello");

    // Back in scope: the note is added again, without OCR.
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    let log = d.log();
    let back: Vec<_> = log.iter().filter(|l| l.note == key).collect();
    let last = back.last().unwrap();
    assert_eq!(last.change, LineChange::New, "{log:?}");
    assert_eq!(texts(last), ["hello"]);
    assert!(!d.state(&key).unwrap().unscoped);
    // test2 enters the scope for the first time and is read.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(d.state(&never).is_some());
}

#[test]
fn a_deleted_note_gives_note_deleted() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    std::fs::remove_file(d.record_path(&key)).unwrap();
    for _ in 0..2 {
        worker(&d, no_settle(), engine_reading("hello"))
            .run_once(true)
            .unwrap();
    }
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].change, LineChange::NoteDeleted);
    assert_eq!(log[1].note, key);
    let state = d.state(&key).unwrap();
    assert!(state.deleted);
    assert_eq!(
        state.pages[0].blocks[0].text, "hello",
        "the last text stays"
    );
}

fn note_file(d: &Data, path: &str) -> PathBuf {
    d.data.join("accounts").join(SLUG).join("files").join(path)
}

#[test]
fn a_missing_note_file_means_the_note_is_gone() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    // Deleted by hand right after an edit the server indexed: the record stays, with work.
    std::fs::remove_file(note_file(&d, "NOTE/Note/test1.note")).unwrap();
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], Some("0123456789abcdef"), None, None);
    });
    for _ in 0..3 {
        let engine = engine_reading("hello");
        let calls = engine.calls_handle();
        let p = worker(&d, no_settle(), engine).run_once(true).unwrap();
        assert_eq!(p.failed, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].change, LineChange::NoteDeleted);
    assert_eq!(log[1].note, key);
    assert!(d.errors().is_empty(), "{:?}", d.errors());
    let state = d.state(&key).unwrap();
    assert!(state.deleted);
    assert_eq!(state.pages[0].blocks[0].text, "hello");
}

#[test]
fn a_missing_note_file_without_text_is_dropped_silently() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    // A failed read first: a page entry in errors.jsonl, no text.
    let engine = engine_reading("hello");
    engine.fail_next(1);
    assert_eq!(
        worker(&d, no_settle(), engine)
            .run_once(true)
            .unwrap()
            .failed,
        1
    );
    // And a note entry with its tries used up, as an older version wrote for a missing file.
    let errors = d.data.join("pipeline").join(SLUG).join("errors.jsonl");
    let mut text = std::fs::read_to_string(&errors).unwrap();
    text.push_str(&format!(
        "{{\"note\":\"{key}\",\"tries\":3,\"lastError\":\"No such file or directory (os error 2)\",\"nextTry\":1}}\n"
    ));
    std::fs::write(&errors, text).unwrap();
    assert_eq!(d.errors().len(), 2);
    std::fs::remove_file(note_file(&d, "NOTE/Note/test1.note")).unwrap();
    for _ in 0..2 {
        let engine = engine_reading("hello");
        let calls = engine.calls_handle();
        let p = worker(&d, no_settle(), engine).run_once(true).unwrap();
        assert_eq!(p.failed, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    assert!(d.log().is_empty(), "{:?}", d.log());
    assert!(d.errors().is_empty(), "{:?}", d.errors());
    let state = d.state(&key).unwrap();
    assert!(state.pages.iter().all(|p| p.seq.is_none()));
    assert!(state.pages.iter().all(|p| p.state != PageOcrState::Failed));
}

#[test]
fn a_note_file_deleted_while_another_note_is_read_is_gone() {
    let d = setup();
    let one = d.put("test1", "NOTE/Note");
    let two = d.put("test2", "NOTE/Note");
    let files = [
        note_file(&d, "NOTE/Note/test1.note"),
        note_file(&d, "NOTE/Note/test2.note"),
    ];
    // Both files go during the first OCR call: the note being read has its bytes already, the
    // other one finds no file.
    let engine = engine_reading("hello").on_call(move || {
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
    });
    let p = worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(p.failed, 0);
    assert!(d.errors().is_empty(), "{:?}", d.errors());
    let read: Vec<&String> = [&one, &two]
        .into_iter()
        .filter(|k| {
            d.state(k)
                .is_some_and(|s| s.pages.iter().any(|p| p.seq.is_some()))
        })
        .collect();
    assert_eq!(read.len(), 1, "one note was read before the files went");
}

#[test]
fn a_note_file_that_cannot_be_read_stays_a_failure() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let file = note_file(&d, "NOTE/Note/test1.note");
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let p = worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    assert_eq!(p.failed, 1);
    let errors = d.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["note"], key.as_str());
    assert!(errors[0]["pageId"].is_null());
    assert!(d.log().is_empty());
}

#[test]
fn a_moved_note_gives_note_moved() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let files = d.data.join("accounts").join(SLUG).join("files");
    std::fs::rename(
        files.join("NOTE/Note/test1.note"),
        files.join("NOTE/Note/Plans.note"),
    )
    .unwrap();
    d.edit_record(&key, |r| r.path = "NOTE/Note/Plans.note".into());
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a move needs no OCR");
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].change, LineChange::NoteMoved);
    assert_eq!(log[1].path.as_deref(), Some("NOTE/Note/Plans.note"));
    assert_eq!(log[1].old_path.as_deref(), Some("NOTE/Note/test1.note"));
    let state = d.state(&key).unwrap();
    assert_eq!(state.path, "NOTE/Note/Plans.note");
    assert_eq!(state.title, "Plans");
}

#[test]
fn the_keyword_scope() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let cfg = PipelineConfig {
        ingest_keyword: Some("ai".into()),
        ..no_settle()
    };
    // test1 carries no keyword on the device.
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    worker(&d, cfg, engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(d.state(&key).is_none());
    assert!(d.log().is_empty());
}

#[test]
fn the_first_run_goes_newest_first() {
    let d = setup();
    let old = d.put("test1", "NOTE/Note");
    let new = d.put("test2", "NOTE/Note");
    d.backdate(&old, Duration::from_secs(2 * 3600));
    d.backdate(&new, Duration::from_secs(3600));
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(false)
        .unwrap();
    let notes: Vec<String> = d.log().into_iter().map(|l| l.note).collect();
    assert_eq!(notes, [new.clone(), old.clone()]);

    // Later changes: the note whose settle time ended first goes first.
    d.edit_record(&old, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, Some("fedcba9876543210"), None);
    });
    d.edit_record(&new, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, Some("fedcba9876543210"), None);
    });
    d.backdate(&new, Duration::from_secs(2 * 3600));
    d.backdate(&old, Duration::from_secs(3600));
    worker(&d, no_settle(), engine_reading("changed text"))
        .run_once(false)
        .unwrap();
    let notes: Vec<String> = d.log().into_iter().skip(2).map(|l| l.note).collect();
    assert_eq!(notes, [new, old]);
}

#[test]
fn a_missed_journal_event_leaves_no_gap() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let journal = Journal::open(&d.data).unwrap();
    let last = journal.last_seq().unwrap();
    assert!(last > 0);
    // As if the events of the put were seen already, but never acted on.
    journal.set_cursor("pipeline", last).unwrap();
    let clock = TestClock::at(now_ms());
    let mut w =
        worker(&d, no_settle(), engine_reading("hello")).with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(d.log().len(), 1, "found from the page records");

    // A record that changes with no event at all.
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, Some("fedcba9876543210"), None);
    });
    clock.set(now_ms());
    let mut w = worker(&d, no_settle(), engine_reading("new words here"))
        .with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let log = d.log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].change, LineChange::Edited);
    assert_eq!(journal.cursor("pipeline").unwrap().unwrap().seq, last);
}

#[test]
fn the_journal_wakes_the_worker_and_moves_the_cursor() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let journal = Journal::open(&d.data).unwrap();
    let clock = TestClock::at(now_ms());
    let mut w =
        worker(&d, no_settle(), engine_reading("hello")).with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(d.log().len(), 1);
    assert_eq!(
        journal.cursor("pipeline").unwrap().unwrap().seq,
        journal.last_seq().unwrap()
    );
}

#[test]
fn the_continuous_worker_prunes_the_text_log() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let clock = TestClock::at(now_ms());
    let start = clock.now_ms();
    let mut w =
        worker(&d, no_settle(), engine_reading("hello")).with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, start + 1)).unwrap();
    assert_eq!(d.log().len(), 1);
    // The daily stamp of the prune: the pass ran it for the account.
    let stamp = d.data.join("pipeline").join(SLUG).join(".pruned");
    let ms: u64 = std::fs::read_to_string(&stamp)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!((start..=clock.now_ms()).contains(&ms), "{ms}");
}

/// A loader that counts its loads and can report a missing model.
fn loader(
    loads: Arc<AtomicUsize>,
    missing: bool,
) -> Box<dyn FnMut() -> Result<Box<dyn OcrEngine>, OcrError> + Send> {
    Box::new(move || {
        loads.fetch_add(1, Ordering::SeqCst);
        if missing {
            Err(OcrError::ModelMissing {
                path: PathBuf::from("/nowhere/model.gguf"),
            })
        } else {
            Ok(Box::new(engine_reading("hello")) as Box<dyn OcrEngine>)
        }
    })
}

/// A stop that says yes once `clock` reaches `ms`. The test clock stands still during a
/// pass, so a pass that starts before `ms` runs to its end.
fn stop_at(clock: &TestClock, ms: u64) -> impl Fn() -> bool + use<> {
    let c = clock.clone();
    move || c.now_ms() >= ms
}

#[test]
fn the_model_loads_when_work_is_due_and_unloads_when_idle() {
    let d = setup();
    let cfg = PipelineConfig {
        model_idle: Duration::from_secs(15 * 60),
        ..no_settle()
    };
    let loads = Arc::new(AtomicUsize::new(0));
    let clock = TestClock::at(now_ms());

    // No work: no model.
    let mut w = Worker::with_loader(&d.data, cfg.clone(), loader(loads.clone(), false))
        .with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 3000)).unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 0);

    d.put("test1", "NOTE/Note");
    let mut w = Worker::with_loader(&d.data, cfg.clone(), loader(loads.clone(), false))
        .with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 3000)).unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(w.model_loaded(), "still loaded right after the work");
    assert_eq!(d.log().len(), 1);
    // Idle for longer than model_idle: unloaded.
    w.run(stop_at(&clock, clock.now_ms() + 16 * 60_000))
        .unwrap();
    assert!(!w.model_loaded());
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[test]
fn a_missing_model_waits_and_checks_every_ten_minutes() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let loads = Arc::new(AtomicUsize::new(0));
    let clock = TestClock::at(now_ms());
    let mut w = Worker::with_loader(&d.data, no_settle(), loader(loads.clone(), true))
        .with_clock(Arc::new(clock.clone()));
    // 25 minutes of one-second polls: the first try and two more.
    w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
        .unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 3);
    assert!(d.log().is_empty());
    assert!(
        d.errors().is_empty(),
        "a missing model is not a page failure"
    );
    let state = d.state(&key);
    assert!(state.is_none_or(|s| s.pages.iter().all(|p| p.state == PageOcrState::Pending)));
    // --once reports it as an error.
    let mut w = Worker::with_loader(&d.data, no_settle(), loader(loads.clone(), true));
    assert!(w.run_once(true).is_err());
}

/// The worker side of a text box edit: an old text box in the stored list makes the change
/// region, so compose takes the new reading. It does not test the stroke list itself (it
/// fakes the old entry); `page::tests::a_text_box_enters_the_list_and_the_region` does.
#[test]
fn a_text_box_edit_is_not_lost() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello world"))
        .run_once(true)
        .unwrap();
    // The page had a text box inside the paragraph; its text was edited, so its record (and
    // the page's strokes hash) changed while the ink stayed. The old box is in the stored list.
    let note_file = d
        .data
        .join("pipeline")
        .join(SLUG)
        .join("notes")
        .join(format!("{key}.json"));
    let mut state: NoteState = serde_json::from_slice(&std::fs::read(&note_file).unwrap()).unwrap();
    state.pages[0]
        .stroke_list
        .push(notedown_pipeline::page::StrokeEntry {
            hash: "0123456789abcdef".into(),
            bbox: [300, 250, 600, 300],
        });
    std::fs::write(&note_file, serde_json::to_vec(&state).unwrap()).unwrap();
    d.edit_record(&key, |r| {
        r.pages[0] = with_parts(&r.pages[0], Some("0123456789abcdef"), None, None);
    });
    let p = worker(&d, no_settle(), engine_reading("hello world!"))
        .run_once(true)
        .unwrap();
    assert_eq!(p.versions, 1);
    let log = d.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].change, LineChange::Edited);
    assert_eq!(log[1].signal, ["strokes"]);
    assert_eq!(texts(&log[1]), ["hello world!"]);
}

impl Data {
    /// Puts test1 again as `copy.note`: a copy of an indexed note is keyed by its path.
    /// Returns its key.
    fn put_copy(&self) -> String {
        let journal = Journal::open(&self.data).unwrap();
        let src = root().join("tests/fixtures/notes/test1.note");
        notedown_cloud::put::put(
            &self.data,
            SLUG,
            &journal,
            &src,
            "NOTE/Note",
            Some("copy.note"),
        )
        .unwrap();
        let key = self
            .records()
            .into_iter()
            .find(|r| r.path == "NOTE/Note/copy.note")
            .unwrap()
            .file_id;
        assert!(key.starts_with("path:"), "{key}");
        key
    }

    /// Moves the note of the path key `old` to `new_path` as the server does: the file moves
    /// and the record is keyed by the new path. Returns the new key.
    fn move_path_keyed(&self, old: &str, new_path: &str) -> String {
        let new = notedown_cloud::pages::path_key(
            &notedown_cloud::store::WirePath::parse_named(new_path).unwrap(),
        );
        let files = self.data.join("accounts").join(SLUG).join("files");
        let mut rec: PageRecord =
            serde_json::from_slice(&std::fs::read(self.record_path(old)).unwrap()).unwrap();
        let to = files.join(new_path);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(files.join(&rec.path), to).unwrap();
        std::fs::remove_file(self.record_path(old)).unwrap();
        rec.file_id = new.clone();
        rec.path = new_path.into();
        std::fs::write(self.record_path(&new), serde_json::to_vec(&rec).unwrap()).unwrap();
        new
    }
}

#[test]
fn a_moved_note_without_a_file_id_keeps_its_text() {
    let d = setup();
    let original = d.put("test1", "NOTE/Note");
    let old = d.put_copy();
    assert_ne!(old, original);
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let before = d.state(&old).unwrap();

    let new_path = "NOTE/Note/Moved.note";
    let new = d.move_path_keyed(&old, new_path);
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a move is not read again");
    let log = d.log();
    assert!(
        log.iter().all(|l| l.change != LineChange::NoteDeleted),
        "{log:?}"
    );
    let moved = log.last().unwrap();
    assert_eq!(moved.change, LineChange::NoteMoved);
    assert_eq!(moved.note, new);
    assert_eq!(moved.old_note.as_deref(), Some(old.as_str()));
    assert_eq!(moved.path.as_deref(), Some(new_path));
    assert_eq!(moved.old_path.as_deref(), Some("NOTE/Note/copy.note"));
    assert!(d.state(&old).is_none(), "the old state is gone");
    let state = d.state(&new).unwrap();
    assert_eq!(state.path, new_path);
    assert_eq!(state.title, "Moved");
    assert_eq!(state.pages, before.pages, "same blocks and ids");
    assert_eq!(state.next_block, before.next_block);

    // Nothing more to do.
    let engine = engine_reading("hello");
    let calls = engine.calls_handle();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(d.log().len(), log.len());
}

#[test]
fn a_path_keyed_move_out_of_scope_names_no_path() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let old = d.put_copy();
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let new = d.move_path_keyed(&old, "NOTE/Private/Moved.note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let log = d.log();
    let tail: Vec<&LogLine> = log.iter().filter(|l| l.note == new).collect();
    assert_eq!(tail.len(), 2, "{log:?}");
    assert_eq!(tail[0].change, LineChange::NoteMoved);
    assert_eq!(tail[0].old_note.as_deref(), Some(old.as_str()));
    assert_eq!(tail[0].path, None, "out of scope: not listed");
    assert_eq!(tail[0].old_path, None);
    assert_eq!(tail[1].change, LineChange::NoteUnscoped);
    assert_eq!(tail[1].path, None);
    assert!(log.iter().all(|l| l.change != LineChange::NoteDeleted));
    assert!(d.state(&new).unwrap().unscoped);
}

#[test]
fn an_unscoped_path_keyed_note_that_moves_names_no_path() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let old = d.put_copy();
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let narrow = PipelineConfig {
        ingest_exclude: vec!["NOTE/Note/copy.note".into(), "NOTE/Note/Moved.note".into()],
        ..no_settle()
    };
    worker(&d, narrow.clone(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    assert_eq!(d.log().last().unwrap().change, LineChange::NoteUnscoped);
    let new = d.move_path_keyed(&old, "NOTE/Note/Moved.note");
    let before = d.log().len();
    worker(&d, narrow, engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let log = d.log();
    assert_eq!(log.len(), before + 1, "{log:?}");
    let moved = log.last().unwrap();
    assert_eq!(moved.change, LineChange::NoteMoved);
    assert_eq!(moved.note, new);
    assert_eq!(moved.old_note.as_deref(), Some(old.as_str()));
    assert_eq!(moved.path, None);
    assert_eq!(moved.old_path, None);
    let state = d.state(&new).unwrap();
    assert!(state.unscoped && !state.deleted);
}

#[test]
fn an_unscoped_note_that_is_deleted_gives_note_deleted() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let narrow = PipelineConfig {
        ingest_folders: vec!["DOCUMENT/**".into()],
        ..no_settle()
    };
    worker(&d, narrow.clone(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    std::fs::remove_file(d.record_path(&key)).unwrap();
    for _ in 0..2 {
        worker(&d, narrow.clone(), engine_reading("hello"))
            .run_once(true)
            .unwrap();
    }
    let changes: Vec<LineChange> = d.log().into_iter().map(|l| l.change).collect();
    assert_eq!(
        changes,
        [
            LineChange::New,
            LineChange::NoteUnscoped,
            LineChange::NoteDeleted
        ]
    );
    assert!(d.state(&key).unwrap().deleted);
}

#[test]
fn digests_of_the_note_are_in_its_metadata() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let items = d.data.join("accounts").join(SLUG).join("digests/items");
    std::fs::create_dir_all(&items).unwrap();
    let digest = |id: u64, meta: String, source: &str| {
        serde_json::json!({
            "id": id, "sourcePath": source, "sourceType": 2, "isDeleted": "N",
            "isSummaryGroup": "N", "metadata": meta,
        })
    };
    let by_id = digest(
        7,
        format!(r#"{{"note_fileId":"{key}","unique_identifier":"u7"}}"#),
        "Note/elsewhere.note",
    );
    let by_path = digest(8, r#"{"unique_identifier":"u8"}"#.into(), "Note/test1.note");
    let other = digest(9, r#"{"note_fileId":"F1"}"#.into(), "Note/other.note");
    // Another note's digest whose source path is this note's: not this note's.
    let other_here = digest(10, r#"{"note_fileId":"F1"}"#.into(), "Note/test1.note");
    for (n, v) in [(7, by_id), (8, by_path), (9, other), (10, other_here)] {
        std::fs::write(items.join(format!("{n}.json")), v.to_string()).unwrap();
    }
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    assert_eq!(d.state(&key).unwrap().meta.digests, ["u7", "u8"]);
}
