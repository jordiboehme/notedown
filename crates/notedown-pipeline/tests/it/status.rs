//! `status_facts`: the pipeline status of one account, read from its files without writing.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notedown_cloud::appdata::Digest;
use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Journal;
use notedown_ocr::{OcrEngine, OcrError};
use notedown_pipeline::PipelineConfig;
use notedown_pipeline::records::{Record, Records};
use notedown_pipeline::source::{JournalBatch, NoteBytes, ServerAccount, ServerData};
use notedown_pipeline::status::{StatusFacts, status_facts};
use notedown_pipeline::store::{Bookmark, TextStore, WORK_FILE, Work, WriterLock};
use notedown_pipeline::worker::{Clock, Worker};
use serde_json::json;

use crate::worker::{
    SLUG, TestClock, engine_reading, no_settle, now_ms, one_machine, setup, stop_at, tree, worker,
};

fn facts(data: &std::path::Path) -> StatusFacts {
    status_facts(data, SLUG, now_ms()).unwrap()
}

fn work_file(data: &std::path::Path) -> PathBuf {
    data.join("pipeline").join(SLUG).join(WORK_FILE)
}

fn old_work(key: &str) -> Work {
    Work {
        note: key.into(),
        path: "NOTE/Note/test1.note".into(),
        page: 1,
        pages: 1,
        started_ms: 1,
    }
}

#[test]
fn status_counts_pages_and_consumers() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let pages = d.store().note(&key).unwrap().unwrap().pages.len();
    let f = facts(&d.data);
    assert_eq!(f.account, SLUG);
    assert!(!f.running);
    assert_eq!(f.notes, 1);
    assert_eq!(f.pages_with_text, pages);
    assert_eq!((f.pending, f.waiting, f.failed, f.truncated), (0, 0, 0, 0));
    assert_eq!(f.errors, 0);
    assert_eq!(f.next_due_ms, None);
    assert!(f.last_error.is_none());
    assert!(f.last_run_ms.is_some_and(|ms| ms > 0));
    assert!(f.in_work.is_none());
    assert!(f.consumers.is_empty());

    // The decided keys, no more and no less.
    let v = serde_json::to_value(&f).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    let mut want = vec![
        "account",
        "running",
        "notes",
        "pagesWithText",
        "pending",
        "waiting",
        "nextDueMs",
        "failed",
        "truncated",
        "errors",
        "lastError",
        "lastRunMs",
        "inWork",
        "consumers",
    ];
    want.sort();
    assert_eq!(keys, want, "{v}");

    let store = d.store();
    let last = store.last_seq().unwrap();
    store.ack("agent", 0).unwrap();
    store.ack("done", last).unwrap();
    let f = facts(&d.data);
    let names: Vec<&str> = f.consumers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["agent", "done"]);
    let agent = &f.consumers[0];
    assert_eq!(agent.seq, 0);
    assert!(agent.acked_at.is_some());
    assert_eq!(agent.read_at, None);
    assert_eq!(agent.notes_behind, 1);
    assert!(!agent.more);
    let done = &f.consumers[1];
    assert_eq!(done.seq, last);
    assert_eq!(done.notes_behind, 0);
    let c = serde_json::to_value(agent).unwrap();
    let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        ["ackedAt", "more", "name", "notesBehind", "readAt", "seq"],
        "{c}"
    );
}

#[test]
fn status_shows_pages_inside_the_settle_time() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let clock = TestClock::at(now_ms());
    let start = clock.now_ms();
    // The default settle time: the fresh note waits.
    let mut w = Worker::new(
        &one_machine(&d, PipelineConfig::default()),
        Box::new(engine_reading("hello")),
    )
    .with_clock(Arc::new(clock.clone()));
    let p = w.run_once(false).unwrap();
    assert!(p.waiting > 0);
    let f = facts(&d.data);
    assert_eq!(f.waiting, p.waiting as u64);
    assert_eq!(f.pending, 0);
    let due = f.next_due_ms.expect("a next due time");
    assert!(due > start, "{due} {start}");
    assert_eq!(Some(due), p.next_due);

    // Read: nothing waits any more.
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let f = facts(&d.data);
    assert_eq!(f.waiting, 0);
    assert_eq!(f.next_due_ms, None);
}

fn missing_model() -> Box<dyn FnMut() -> Result<Box<dyn OcrEngine>, OcrError> + Send> {
    Box::new(|| {
        Err(OcrError::ModelMissing {
            path: PathBuf::from("/nowhere/model.gguf"),
        })
    })
}

#[test]
fn status_shows_the_last_worker_error() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let before = now_ms();
    let mut w = Worker::with_loader(&one_machine(&d, no_settle()), missing_model());
    assert!(w.run_once(true).is_err());
    let f = facts(&d.data);
    let e = f.last_error.expect("the missing model is the last error");
    assert!(e.text.contains("model"), "{}", e.text);
    assert!(e.at_ms >= before, "{} {before}", e.at_ms);
    assert!(f.in_work.is_none());
    assert!(
        !work_file(&d.data).exists(),
        "a failed pass leaves no work file"
    );
    // The same failure again keeps the time it started.
    std::thread::sleep(Duration::from_millis(5));
    let mut w = Worker::with_loader(&one_machine(&d, no_settle()), missing_model());
    assert!(w.run_once(true).is_err());
    assert_eq!(facts(&d.data).last_error, Some(e.clone()));

    // A good pass clears it.
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    assert!(facts(&d.data).last_error.is_none());
}

#[tokio::test]
async fn get_changes_with_a_consumer_stamps_read_at() {
    let d = crate::mcp::setup(&[SLUG]);
    let client = crate::mcp::connect(&d.data, None).await;
    let store = TextStore::open(&d.data, SLUG).unwrap();
    // Without a consumer nothing is stamped.
    crate::mcp::ok(&client, "get_changes", json!({"since": 0})).await;
    assert!(store.consumers().unwrap().is_empty());

    let before = now_ms();
    crate::mcp::ok(&client, "get_changes", json!({"consumer": "agent"})).await;
    let b = store.bookmark("agent").unwrap().expect("a bookmark");
    assert_eq!((b.seq, b.updated_at), (0, 0), "a read is no ack");
    assert!(b.read_at.is_some_and(|ms| ms >= before), "{b:?}");

    crate::mcp::ok(&client, "ack", json!({"consumer": "agent", "upTo": 2})).await;
    let acked = store.bookmark("agent").unwrap().unwrap();
    assert_eq!(acked.seq, 2);
    assert_eq!(acked.read_at, b.read_at);

    let status = crate::mcp::ok(&client, "status", json!({})).await;
    assert!(status.contains("readAt"), "{status}");
    assert!(status.contains("notesBehind"), "{status}");
}

#[test]
fn an_ack_keeps_read_at() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let store = d.store();
    store.mark_read("agent", 123).unwrap();
    let b = store.ack("agent", 1).unwrap();
    assert_eq!(b.seq, 1);
    assert_eq!(b.read_at, Some(123));
    assert_eq!(store.bookmark("agent").unwrap(), Some(b));
    // A later read keeps the ack.
    let r = store.mark_read("agent", 456).unwrap();
    assert_eq!(
        (r.seq, r.updated_at, r.read_at),
        (1, b.updated_at, Some(456))
    );
}

#[test]
fn a_first_read_of_a_new_consumer_is_listed_with_no_ack() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let store = d.store();
    let b = store.mark_read("fresh", 500).unwrap();
    assert_eq!(
        b,
        Bookmark {
            seq: 0,
            updated_at: 0,
            read_at: Some(500)
        }
    );
    let f = facts(&d.data);
    assert_eq!(f.consumers.len(), 1);
    let c = &f.consumers[0];
    assert_eq!(c.name, "fresh");
    assert_eq!(c.acked_at, None);
    assert_eq!(c.read_at, Some(500));
    assert_eq!(c.notes_behind, 1);
    let v = serde_json::to_value(c).unwrap();
    assert_eq!(v["ackedAt"], serde_json::Value::Null, "{v}");
}

#[test]
fn an_old_bookmark_still_reads() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let dir = d.data.join("pipeline").join(SLUG).join("consumers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old.json"), r#"{"seq": 1, "updatedAt": 123}"#).unwrap();
    let store = d.store();
    let b = store.bookmark("old").unwrap().unwrap();
    assert_eq!(
        b,
        Bookmark {
            seq: 1,
            updated_at: 123,
            read_at: None
        }
    );
    let f = facts(&d.data);
    assert_eq!(f.consumers[0].acked_at, Some(123));
    assert_eq!(f.consumers[0].read_at, None);
    let r = store.mark_read("old", 900).unwrap();
    assert_eq!((r.seq, r.updated_at, r.read_at), (1, 123, Some(900)));
}

#[test]
fn status_without_a_running_pipeline_has_no_work() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    // A killed worker left its work file.
    std::fs::write(
        work_file(&d.data),
        serde_json::to_vec(&old_work(&key)).unwrap(),
    )
    .unwrap();
    let f = facts(&d.data);
    assert!(!f.running);
    assert!(f.in_work.is_none());
}

#[test]
fn status_while_the_pipeline_runs_shows_the_page() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let seen: Arc<Mutex<Vec<StatusFacts>>> = Arc::default();
    let (data, out) = (d.data.clone(), seen.clone());
    let engine = engine_reading("hello").on_call(move || {
        out.lock()
            .unwrap()
            .push(status_facts(&data, SLUG, now_ms()).unwrap());
    });
    let before = now_ms();
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    let seen = seen.lock().unwrap();
    let first = seen.first().expect("the engine was called");
    assert!(first.running);
    let work = first.in_work.as_ref().expect("a page in work");
    assert_eq!(work.note, key);
    assert_eq!(work.path, "NOTE/Note/test1.note");
    assert_eq!(work.page, 1);
    assert!(work.pages >= 1);
    assert!(work.started_ms >= before);
    // The end of the pass removes the work file.
    assert!(!work_file(&d.data).exists());
    let f = facts(&d.data);
    assert!(!f.running);
    assert!(f.in_work.is_none());
}

#[test]
fn a_new_worker_removes_an_old_work_file() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let file = work_file(&d.data);
    std::fs::write(&file, serde_json::to_vec(&old_work(&key)).unwrap()).unwrap();
    let gone_at_start = Arc::new(AtomicBool::new(false));
    let clock = TestClock::at(now_ms());
    let mut w =
        worker(&d, no_settle(), engine_reading("hello")).with_clock(Arc::new(clock.clone()));
    let (f, flag) = (file.clone(), gone_at_start.clone());
    w.run_with_start(
        move || flag.store(!f.exists(), Ordering::SeqCst),
        stop_at(&clock, clock.now_ms()),
    )
    .unwrap();
    assert!(gone_at_start.load(Ordering::SeqCst));
}

#[test]
fn status_on_a_fresh_data_folder_creates_nothing() {
    let d = setup();
    let before = tree(&d.data);
    let f = facts(&d.data);
    assert_eq!(f.account, SLUG);
    assert!(!f.running);
    assert_eq!(
        (f.notes, f.pages_with_text, f.pending, f.waiting),
        (0, 0, 0, 0)
    );
    assert!(f.consumers.is_empty());
    assert_eq!(
        notedown_pipeline::mcp::resolve_account(&d.data, None).unwrap(),
        SLUG
    );
    assert_eq!(tree(&d.data), before);
    assert!(!d.data.join("pipeline").exists());
}

#[test]
fn a_writer_starts_while_status_probes() {
    let d = setup();
    let dir = d.data.join("pipeline");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".writer.lock");
    std::fs::write(&path, b"").unwrap();
    // The probe holds the lock for a moment, as a status call does.
    let probe = std::fs::File::open(&path).unwrap();
    probe.try_lock_shared().unwrap();
    // A second probe at the same moment sees no writer.
    assert!(!WriterLock::held(&d.data).unwrap());
    let data = d.data.clone();
    let writer = std::thread::spawn(move || WriterLock::take(&data).map(|_| ()));
    std::thread::sleep(Duration::from_millis(200));
    drop(probe);
    writer
        .join()
        .unwrap()
        .expect("the writer waits for the probe");
}

/// The server data of one machine, with a switch that fails the account list and a list of
/// refused tokens, as a server that cannot be reached or a revoked token would.
struct Flaky {
    inner: Arc<dyn ServerData>,
    fail: Arc<AtomicBool>,
    refused: Arc<Mutex<Vec<(String, String)>>>,
}

impl ServerData for Flaky {
    fn describe(&self) -> String {
        "flaky server".into()
    }
    fn accounts(&self) -> notedown_pipeline::Result<Vec<ServerAccount>> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(notedown_pipeline::Error::Read(
                "the server cannot be reached".into(),
            ));
        }
        let refused = self.refused.lock().unwrap().clone();
        Ok(self
            .inner
            .accounts()?
            .into_iter()
            .filter(|a| !refused.iter().any(|(s, _)| s == &a.slug))
            .collect())
    }
    fn records(&self, slug: &str) -> notedown_pipeline::Result<Records> {
        self.inner.records(slug)
    }
    fn record(&self, slug: &str, key: &str) -> notedown_pipeline::Result<Option<Record>> {
        self.inner.record(slug, key)
    }
    fn note(&self, slug: &str, path: &str) -> notedown_pipeline::Result<NoteBytes> {
        self.inner.note(slug, path)
    }
    fn note_exists(&self, slug: &str, path: &str) -> notedown_pipeline::Result<Option<bool>> {
        self.inner.note_exists(slug, path)
    }
    fn digests(&self, slug: &str) -> notedown_pipeline::Result<Vec<Digest>> {
        self.inner.digests(slug)
    }
    fn journal(
        &self,
        slug: &str,
        from: u64,
        limit: usize,
    ) -> notedown_pipeline::Result<JournalBatch> {
        self.inner.journal(slug, from, limit)
    }
    fn refusals(&self) -> Vec<(String, String)> {
        self.refused.lock().unwrap().clone()
    }
}

struct FlakyRig {
    setup: notedown_pipeline::setup::Setup,
    fail: Arc<AtomicBool>,
    refused: Arc<Mutex<Vec<(String, String)>>>,
}

fn flaky(d: &crate::worker::Data) -> FlakyRig {
    let mut setup = one_machine(d, no_settle());
    let fail = Arc::new(AtomicBool::new(false));
    let refused: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    setup.source = Arc::new(Flaky {
        inner: setup.source.clone(),
        fail: fail.clone(),
        refused: refused.clone(),
    });
    FlakyRig {
        setup,
        fail,
        refused,
    }
}

fn quiet() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|_: &str| {})
}

#[test]
fn an_account_list_error_is_the_last_error_until_the_list_works() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let rig = flaky(&d);
    let clock = TestClock::at(now_ms());
    let t0 = clock.now_ms();
    let during: Arc<Mutex<Option<StatusFacts>>> = Arc::default();
    let (c, fail, data, seen) = (
        clock.clone(),
        rig.fail.clone(),
        d.data.clone(),
        during.clone(),
    );
    // A good turn (a pass), then the list fails from 2 s to 5 s, then works again.
    let stop = move || {
        let t = c.now_ms() - t0;
        if (4_000..5_000).contains(&t) {
            *seen.lock().unwrap() = Some(status_facts(&data, SLUG, c.now_ms()).unwrap());
        }
        fail.store((2_000..5_000).contains(&t), Ordering::SeqCst);
        t >= 8_000
    };
    Worker::new(&rig.setup, Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(quiet())
        .run(stop)
        .unwrap();
    let during = during
        .lock()
        .unwrap()
        .clone()
        .expect("a status while it failed");
    let e = during.last_error.expect("the list error is the last error");
    assert!(
        e.text.contains("the server cannot be reached"),
        "{}",
        e.text
    );
    let f = facts(&d.data);
    assert!(f.last_error.is_none(), "{:?}", f.last_error);
    // The recovery started a pass at once, not after the rescan time.
    assert!(
        f.last_run_ms.is_some_and(|ms| ms >= t0 + 5_000),
        "{:?} {t0}",
        f.last_run_ms
    );
}

#[test]
fn a_refused_token_is_the_last_error_until_it_works_again() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let rig = flaky(&d);
    rig.refused
        .lock()
        .unwrap()
        .push((SLUG.into(), "the token of jordi was refused".into()));
    let clock = TestClock::at(now_ms());
    let mut w = Worker::new(&rig.setup, Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(quiet());
    w.run(stop_at(&clock, clock.now_ms() + 2_000)).unwrap();
    let e = facts(&d.data)
        .last_error
        .expect("the refusal is the last error");
    assert!(
        e.text.contains("the token of jordi was refused"),
        "{}",
        e.text
    );
    rig.refused.lock().unwrap().clear();
    w.run(stop_at(&clock, clock.now_ms() + 2_000)).unwrap();
    assert!(facts(&d.data).last_error.is_none());
}

#[test]
fn in_work_names_only_the_account_being_read() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    Accounts::new(&d.data)
        .create("anna", "anna@example.org", "secret")
        .unwrap();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/notes/test2.note");
    let journal = Journal::open(&d.data).unwrap();
    notedown_cloud::put::put(&d.data, "anna", &journal, &src, "NOTE/Note", None).unwrap();
    // Per engine call: the page in work of jordi and of anna.
    type Seen = Vec<(Option<Work>, Option<Work>)>;
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let (data, out) = (d.data.clone(), seen.clone());
    let engine = engine_reading("hello").on_call(move || {
        let of = |slug: &str| status_facts(&data, slug, now_ms()).unwrap().in_work;
        out.lock().unwrap().push((of(SLUG), of("anna")));
    });
    worker(&d, no_settle(), engine).run_once(true).unwrap();
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    for (jordi, anna) in seen.iter() {
        assert!(
            jordi.is_some() != anna.is_some(),
            "one account in work at a time: {jordi:?} {anna:?}"
        );
    }
    assert!(seen.iter().any(|(j, _)| j.is_some()) && seen.iter().any(|(_, a)| a.is_some()));
}

#[test]
fn a_past_next_due_time_is_not_shown() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    let path = d.data.join("pipeline").join(SLUG).join("stats.json");
    let mut stats: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stats["waiting"] = json!(1);
    stats["nextDueMs"] = json!(now_ms() - 1_000);
    std::fs::write(&path, serde_json::to_vec(&stats).unwrap()).unwrap();
    assert_eq!(facts(&d.data).next_due_ms, None);
    let later = now_ms() + 60_000;
    stats["nextDueMs"] = json!(later);
    std::fs::write(&path, serde_json::to_vec(&stats).unwrap()).unwrap();
    assert_eq!(facts(&d.data).next_due_ms, Some(later));
}

#[test]
fn a_broken_error_or_stats_file_does_not_fail_the_status() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    worker(&d, no_settle(), engine_reading("hello"))
        .run_once(true)
        .unwrap();
    std::fs::write(d.data.join("pipeline/last_error.json"), "{not json").unwrap();
    let f = status_facts(&d.data, SLUG, now_ms()).expect("a broken last_error.json");
    assert_eq!(f.notes, 1);
    assert!(f.last_error.is_none());
    std::fs::write(
        d.data.join("pipeline").join(SLUG).join("stats.json"),
        [0xff, 0x00],
    )
    .unwrap();
    let f = status_facts(&d.data, SLUG, now_ms()).expect("a broken stats.json");
    assert_eq!(f.notes, 1);
    assert_eq!((f.waiting, f.last_run_ms), (0, None));
}
