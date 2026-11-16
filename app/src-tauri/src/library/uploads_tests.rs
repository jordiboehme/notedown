//! Uploads against a routed server on loopback (one thread per connection, the answer picked by
//! the request line), with real files, folders, symlinks and a symlink loop in temp folders. A
//! wait for a syncing Supernote moves a fake clock instead of sleeping; the stall watchdog reads
//! the same clock, so the test says when a minute has passed. Each test joins the upload threads
//! before it ends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::listing_tests::{MD5_A, NOTE, Time};
use super::tests::{CountingStore, Events, Place, TOKEN, remote_key};
use super::uploads::{DROP_GONE, MANY, WALK_CAP, ask_for, walk};
use super::*;
use crate::rest::test_http::{Act, Request, Routed, answer, answer_with};

const WAITING: &str = "The Supernote is syncing. Notedown waits.";
const SYNC_FAILED: &str = "The Supernote is syncing. Try again in a moment.";

struct Setup {
    server: Routed,
    library: Library,
    events: Events,
    time: Time,
}

impl Drop for Setup {
    fn drop(&mut self) {
        // No upload thread outlives its test.
        self.library.join_uploads();
    }
}

fn setup(routes: Vec<(String, Act)>) -> Setup {
    setup_with(routes, Time::default(), Events::default(), |_| {})
}

/// [`setup`] with the clock `time` and the events `events`, and `hook` runs with each request
/// line as it comes.
fn setup_with(
    routes: Vec<(String, Act)>,
    time: Time,
    events: Events,
    hook: impl Fn(&str) + Send + Sync + 'static,
) -> Setup {
    let server = Routed::start_with(routes, hook);
    let store = CountingStore::with(&remote_key(server.port, "jordi"), TOKEN);
    let place = Place::remote(server.port, Some("jordi"));
    // A wait (423 `device_syncing`) moves the clock instead of sleeping.
    let slept = time.clone();
    let sleeper: Sleeper = Arc::new(move |d: Duration| {
        slept.pass(u64::try_from(d.as_millis()).unwrap());
    });
    let library = Library::with_timing(store, place.func(), events.emit(), time.clock(), sleeper);
    library.refresh();
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    events.0.lock().unwrap().clear();
    Setup {
        server,
        library,
        events,
        time,
    }
}

/// `"<METHOD> /rest/v1<route>?path=<path>"` as the client encodes it.
fn line(method: &str, route: &str, path: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("http://h/rest/v1{route}")).unwrap();
    url.query_pairs_mut().append_pair("path", path);
    format!("{method} {}?{}", url.path(), url.query().unwrap())
}

fn look(path: &str) -> String {
    line("GET", "/entries", path)
}

fn put(path: &str) -> String {
    line("PUT", "/files", path)
}

fn not_found() -> Act {
    Act::Answer(answer("404 Not Found", r#"{"error":"not_found"}"#))
}

fn put_answer(status: &str, path: &str, outcome: &str, seqs: &[u64]) -> String {
    answer(
        status,
        &json!({"id": 41, "path": path, "outcome": outcome, "md5": MD5_A, "size": 5,
                "mtimeMs": 1_763_290_000_000_i64, "seqs": seqs})
        .to_string(),
    )
}

fn created(path: &str) -> Act {
    Act::Answer(put_answer("201 Created", path, "created", &[30]))
}

fn found(path: &str, kind: &str) -> Act {
    let name = path.rsplit('/').next().unwrap();
    let md5 = if kind == "file" {
        json!(MD5_A)
    } else {
        Value::Null
    };
    Act::Answer(answer(
        "200 OK",
        &json!({"id": 9, "path": path, "name": name, "kind": kind, "size": 3, "md5": md5,
                "mtimeMs": 1, "writable": true})
        .to_string(),
    ))
}

fn error(status: &str, body: Value) -> Act {
    Act::Answer(answer(status, &body.to_string()))
}

fn syncing() -> Act {
    Act::Answer(answer_with(
        "423 Locked",
        &[("retry-after", "5")],
        r#"{"error":"device_syncing"}"#,
    ))
}

fn account(syncing: bool) -> Act {
    Act::Answer(answer(
        "200 OK",
        &json!({"slug": "jordi", "userId": 1, "devices": [
            {"serial": "SN1", "name": null, "bound": true, "lastLoginMs": 1,
             "lastSyncMs": null, "syncing": syncing, "online": true}]})
        .to_string(),
    ))
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Drops `paths` on the window and answers the drop id of the event.
fn drop_paths(s: &Setup, paths: Vec<PathBuf>) -> u64 {
    let count = paths.len();
    s.library.dropped(paths, 10.4, 20.6);
    let events = take(s);
    match events.as_slice() {
        [
            LibraryEvent::Drop {
                drop_id,
                position,
                count: n,
            },
        ] => {
            assert_eq!(*position, Position { x: 10, y: 21 });
            assert_eq!(*n, count);
            *drop_id
        }
        other => panic!("no drop event: {other:?}"),
    }
}

fn take(s: &Setup) -> Vec<LibraryEvent> {
    std::mem::take(&mut *s.events.0.lock().unwrap())
}

fn job_events(s: &Setup) -> Vec<Job> {
    take(s)
        .into_iter()
        .filter_map(|e| match e {
            LibraryEvent::Job { job } => Some(job),
            _ => None,
        })
        .collect()
}

/// The last event of each job, by label.
fn last(jobs: &[Job]) -> BTreeMap<String, Job> {
    jobs.iter().map(|j| (j.label.clone(), j.clone())).collect()
}

/// The requests the server got, as `"<METHOD> <target>"`, sorted.
fn lines(requests: &[Request]) -> Vec<String> {
    let mut all: Vec<String> = requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.target))
        .collect();
    all.sort();
    all
}

fn by_line<'a>(requests: &'a [Request], key: &str) -> &'a Request {
    requests
        .iter()
        .find(|r| format!("{} {}", r.method, r.target) == key)
        .unwrap_or_else(|| panic!("no request {key}"))
}

/// Waits (at most five seconds) until `done` holds for the events so far.
fn wait_for(events: &Events, done: impl Fn(&[LibraryEvent]) -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !done(&events.all()) {
        assert!(Instant::now() < until, "waited in vain: {:?}", events.all());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn is_job(e: &LibraryEvent, label: &str, state: JobState) -> bool {
    matches!(e, LibraryEvent::Job { job } if job.label == label && job.state == state)
}

#[test]
fn an_upload_takes_only_paths_of_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.txt", b"hello");
    write(dir.path(), "secret.txt", b"not dropped");
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), not_found()),
        (put("NOTE/Note/a.txt"), created("NOTE/Note/a.txt")),
    ]);

    // No drop with this id: nothing is read or sent.
    assert_eq!(
        s.library.upload(99, NOTE, false, None),
        Err(DROP_GONE.to_owned())
    );
    let id = drop_paths(&s, vec![a]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let requests = s.server.received();
    assert_eq!(
        lines(&requests),
        [look("NOTE/Note/a.txt"), put("NOTE/Note/a.txt")]
    );
    let sent = by_line(&requests, &put("NOTE/Note/a.txt"));
    assert_eq!(sent.body, b"hello");
    assert_eq!(sent.header("content-length"), Some("5"));
    assert_eq!(sent.header("if-none-match"), Some("*"));
    assert_eq!(sent.header("if-match"), None);
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    let jobs = job_events(&s);
    let ended = jobs.last().unwrap();
    assert_eq!(ended.kind, JobKind::Upload);
    assert_eq!(ended.label, "a.txt");
    assert_eq!(ended.state, JobState::Done);
    assert_eq!((ended.done, ended.total), (5, Some(5)));
    assert_eq!(ended.result.as_deref(), Some("NOTE/Note/a.txt"));
    assert_eq!(jobs.first().unwrap().state, JobState::Queued);

    // A drop is used once.
    assert_eq!(
        s.library.upload(id, NOTE, false, None),
        Err(DROP_GONE.to_owned())
    );
    // Only the newest drops are kept.
    let first = drop_paths(&s, vec![dir.path().join("a.txt")]);
    for _ in 0..16 {
        drop_paths(&s, vec![dir.path().join("a.txt")]);
    }
    assert_eq!(
        s.library.upload(first, NOTE, false, None),
        Err(DROP_GONE.to_owned())
    );
    // A drop of nothing sends no event.
    s.library.dropped(Vec::new(), 1.0, 1.0);
    assert!(take(&s).is_empty());
    assert!(s.server.received().is_empty());
}

#[cfg(unix)]
fn symlink(target: impl AsRef<Path>, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn a_dropped_folder_skips_dot_files_symlinks_and_packages() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    write(d, "Trip/a.txt", b"a");
    write(d, "Trip/.DS_Store", b"x");
    write(d, "Trip/sub/b.txt", b"b");
    write(d, "Trip/Foo.app/Contents/x", b"x");
    write(d, "Trip/.git/config", b"x");
    symlink(d.join("Trip/a.txt"), &d.join("Trip/link.txt"));
    symlink(d.join("secret.txt"), &d.join("Trip/outside.txt"));
    write(d, "secret.txt", b"outside the drop");
    let fifo = std::ffi::CString::new(d.join("Trip/fifo").to_str().unwrap()).unwrap();
    // SAFETY: a valid C string, and mkfifo reads only it.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    // macOS hands names in NFD.
    write(d, "Cafe\u{301}.txt", b"c");
    write(d, ".hidden.txt", b"h");
    symlink(d.join("Trip"), &d.join("alias"));
    let dropped = vec![
        d.join("Trip"),
        d.join("Cafe\u{301}.txt"),
        d.join(".hidden.txt"),
        d.join("alias"),
    ];

    let plan = walk(&dropped, false, None, &AtomicBool::new(false));
    let names: Vec<Vec<String>> = plan.items.iter().map(|p| p.names.clone()).collect();
    let n = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            n(&["Trip", "a.txt"]),
            n(&["Trip", "sub", "b.txt"]),
            n(&["Caf\u{e9}.txt"]),
        ]
    );
    assert_eq!(plan.items[0].source, d.join("Trip/a.txt"));
    assert_eq!(plan.items[0].size, Some(1));
    assert_eq!(plan.files, 3);
    assert_eq!(plan.packages, ["Foo.app"]);
    // Asked for, it goes into packages.
    let all = walk(&dropped, true, None, &AtomicBool::new(false));
    assert_eq!(all.files, 4);
    assert!(
        all.items
            .iter()
            .any(|p| p.names == n(&["Trip", "Foo.app", "Contents", "x"]))
    );
    assert!(all.packages.is_empty());

    let s = setup(vec![
        (look("NOTE/Note/Trip/a.txt"), not_found()),
        (put("NOTE/Note/Trip/a.txt"), created("NOTE/Note/Trip/a.txt")),
        (look("NOTE/Note/Trip/sub/b.txt"), not_found()),
        (
            put("NOTE/Note/Trip/sub/b.txt"),
            created("NOTE/Note/Trip/sub/b.txt"),
        ),
        (look("NOTE/Note/Caf\u{e9}.txt"), not_found()),
        (
            put("NOTE/Note/Caf\u{e9}.txt"),
            created("NOTE/Note/Caf\u{e9}.txt"),
        ),
    ]);
    let id = drop_paths(&s, dropped);
    // The package is asked about first; nothing is sent.
    assert_eq!(
        s.library.upload(id, NOTE, false, None),
        Ok(Some(UploadAsk {
            ask: Ask::Packages,
            text: "Foo.app is a package. Upload what is in it too?".into(),
        }))
    );
    assert!(s.server.received().is_empty());
    assert!(job_events(&s).is_empty());
    assert_eq!(s.library.upload(id, NOTE, false, Some(false)), Ok(None));
    s.library.join_uploads();
    let requests = s.server.received();
    assert_eq!(requests.len(), 6, "{:?}", lines(&requests));
    assert_eq!(by_line(&requests, &put("NOTE/Note/Trip/a.txt")).body, b"a");
    let ended = last(&job_events(&s));
    assert_eq!(
        ended.keys().collect::<Vec<_>>(),
        ["Caf\u{e9}.txt", "Trip/a.txt", "Trip/sub/b.txt"]
    );
    assert!(ended.values().all(|j| j.state == JobState::Done));
}

#[test]
fn a_symlink_loop_ends() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    write(d, "loop/a.txt", b"a");
    symlink(".", &d.join("loop/self"));
    symlink("..", &d.join("loop/up"));
    symlink(d.join("loop"), &d.join("loop/again"));
    symlink(d.join("loop"), &d.join("into"));
    let plan = walk(
        &[d.join("loop"), d.join("into")],
        true,
        None,
        &AtomicBool::new(false),
    );
    assert_eq!(plan.files, 1);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].names, ["loop", "a.txt"]);
}

#[test]
fn empty_folders_are_made() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("Trip/empty")).unwrap();
    std::fs::create_dir_all(d.join("Trip/deep/deeper")).unwrap();
    write(d, "Trip/dots/.DS_Store", b"x");
    write(d, "Trip/full/a.txt", b"a");
    std::fs::create_dir_all(d.join("Lonely")).unwrap();
    let folder = |path: &str| {
        Act::Answer(answer(
            "201 Created",
            &json!({"id": 7, "path": path, "seqs": [40]}).to_string(),
        ))
    };
    let s = setup(vec![
        (
            "POST /rest/v1/folders".into(),
            folder("NOTE/Note/Trip/deep/deeper"),
        ),
        (
            "POST /rest/v1/folders".into(),
            folder("NOTE/Note/Trip/dots"),
        ),
        (
            "POST /rest/v1/folders".into(),
            folder("NOTE/Note/Trip/empty"),
        ),
        ("POST /rest/v1/folders".into(), folder("NOTE/Note/Lonely")),
        (look("NOTE/Note/Trip/full/a.txt"), not_found()),
        (
            put("NOTE/Note/Trip/full/a.txt"),
            created("NOTE/Note/Trip/full/a.txt"),
        ),
    ]);
    let id = drop_paths(&s, vec![d.join("Trip"), d.join("Lonely")]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let requests = s.server.received();
    let mut made: Vec<String> = requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| r.json()["path"].as_str().unwrap().to_owned())
        .collect();
    made.sort();
    assert_eq!(
        made,
        [
            "NOTE/Note/Lonely",
            "NOTE/Note/Trip/deep/deeper",
            "NOTE/Note/Trip/dots",
            "NOTE/Note/Trip/empty",
        ]
    );
    assert_eq!(requests.len(), 6);
    let ended = last(&job_events(&s));
    assert_eq!(ended.len(), 5);
    assert!(
        ended.values().all(|j| j.state == JobState::Done),
        "{ended:?}"
    );
    assert_eq!(ended["Trip/empty"].total, None);
    assert!(ended["Trip/empty"].result.is_some());
}

#[test]
fn more_than_500_files_asks() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..=MANY {
        write(dir.path(), &format!("Many/{n}.txt"), b"x");
    }
    let s = setup(Vec::new());
    let id = drop_paths(&s, vec![dir.path().join("Many")]);
    assert_eq!(
        s.library.upload(id, NOTE, false, None),
        Ok(Some(UploadAsk {
            ask: Ask::Many,
            text: "Upload 501 files?".into(),
        }))
    );
    assert!(s.server.received().is_empty());
    assert!(job_events(&s).is_empty());
    // The user says no: the drop goes.
    s.library.cancel_drop(id);
    assert_eq!(
        s.library.upload(id, NOTE, true, None),
        Err(DROP_GONE.to_owned())
    );

    // 500 is fine; the count has thousands marked; packages are asked first.
    let mut plan = walk(
        &[dir.path().join("Many")],
        false,
        None,
        &AtomicBool::new(false),
    );
    plan.files = MANY;
    assert_eq!(ask_for(&plan, false, None), None);
    plan.files = 1234;
    assert_eq!(
        ask_for(&plan, false, None).unwrap().text,
        "Upload 1,234 files?"
    );
    assert_eq!(ask_for(&plan, true, None), None);
    plan.packages = vec!["A.app".into(), "B.photoslibrary".into()];
    assert_eq!(
        ask_for(&plan, true, None),
        Some(UploadAsk {
            ask: Ask::Packages,
            text: "2 packages are in the drop, such as A.app. Upload what is in them too?".into(),
        })
    );
    assert_eq!(ask_for(&plan, true, Some(false)), None);
    assert_eq!(ask_for(&plan, false, Some(true)).unwrap().ask, Ask::Many);
}

#[test]
fn a_file_over_1_gib_is_refused_locally() {
    let dir = tempfile::tempdir().unwrap();
    let big = std::fs::File::create(dir.path().join("big.bin")).unwrap();
    // Sparse: no gigabyte is written.
    big.set_len((1 << 30) + 1).unwrap();
    let small = write(dir.path(), "small.txt", b"s");
    let s = setup(vec![
        (look("NOTE/Note/small.txt"), not_found()),
        (put("NOTE/Note/small.txt"), created("NOTE/Note/small.txt")),
    ]);
    let id = drop_paths(&s, vec![dir.path().join("big.bin"), small]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    assert_eq!(
        lines(&s.server.received()),
        [look("NOTE/Note/small.txt"), put("NOTE/Note/small.txt")]
    );
    let ended = last(&job_events(&s));
    assert_eq!(ended["big.bin"].state, JobState::Failed);
    assert_eq!(ended["big.bin"].text.as_deref(), Some("Files up to 1 GB."));
    assert_eq!(ended["small.txt"].state, JobState::Done);
    // A retry is refused again, still without a request.
    s.library.retry(ended["big.bin"].id).unwrap();
    s.library.join_uploads();
    assert!(s.server.received().is_empty());
    assert_eq!(
        last(&job_events(&s))["big.bin"].text.as_deref(),
        Some("Files up to 1 GB.")
    );
}

#[test]
fn a_clash_is_asked_before_any_byte_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]
        .iter()
        .map(|name| write(dir.path(), name, b"new"))
        .collect();
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), found("NOTE/Note/a.txt", "file")),
        (
            put("NOTE/Note/a.txt"),
            Act::Answer(put_answer("200 OK", "NOTE/Note/a.txt", "replaced", &[31])),
        ),
        (look("NOTE/Note/b.txt"), found("NOTE/Note/b.txt", "file")),
        (
            look("NOTE/Note/b (2).txt"),
            found("NOTE/Note/b (2).txt", "file"),
        ),
        (look("NOTE/Note/b (3).txt"), not_found()),
        (put("NOTE/Note/b (3).txt"), created("NOTE/Note/b (3).txt")),
        (look("NOTE/Note/c.txt"), found("NOTE/Note/c.txt", "file")),
        (look("NOTE/Note/d.txt"), found("NOTE/Note/d.txt", "folder")),
        (look("NOTE/Note/d (2).txt"), not_found()),
        (put("NOTE/Note/d (2).txt"), created("NOTE/Note/d (2).txt")),
        // e: free at the look, taken at the commit (raced): asked again.
        (look("NOTE/Note/e.txt"), not_found()),
        (
            put("NOTE/Note/e.txt"),
            error(
                "412 Precondition Failed",
                json!({"error": "precondition_failed"}),
            ),
        ),
        (look("NOTE/Note/e.txt"), found("NOTE/Note/e.txt", "file")),
    ]);
    let id = drop_paths(&s, paths);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let requests = s.server.received();
    // Only looks, and the one raced put of e.
    assert_eq!(
        requests.iter().filter(|r| r.method == "PUT").count(),
        1,
        "{:?}",
        lines(&requests)
    );
    let asked = last(&job_events(&s));
    assert!(
        asked.values().all(|j| j.state == JobState::Asking),
        "{asked:?}"
    );
    let clash = |name: &str, can_replace: bool| Clash {
        name: name.into(),
        existing: name.into(),
        can_replace,
    };
    assert_eq!(asked["a.txt"].question, Some(clash("a.txt", true)));
    assert_eq!(
        asked["a.txt"].text.as_deref(),
        Some("There is already a.txt here.")
    );
    assert_eq!(asked["d.txt"].question, Some(clash("d.txt", false)));
    assert_eq!(asked["e.txt"].question, Some(clash("e.txt", true)));

    // A folder cannot be replaced.
    assert!(
        s.library
            .answer(asked["d.txt"].id, Choice::Replace)
            .is_err()
    );
    s.library
        .answer(asked["a.txt"].id, Choice::Replace)
        .unwrap();
    s.library
        .answer(asked["b.txt"].id, Choice::KeepBoth)
        .unwrap();
    s.library.answer(asked["c.txt"].id, Choice::Skip).unwrap();
    s.library
        .answer(asked["d.txt"].id, Choice::KeepBoth)
        .unwrap();
    s.library.join_uploads();
    let requests = s.server.received();
    let replaced = by_line(&requests, &put("NOTE/Note/a.txt"));
    assert_eq!(
        replaced.header("if-match"),
        Some(format!("\"{MD5_A}\"").as_str())
    );
    assert_eq!(replaced.header("if-none-match"), None);
    assert_eq!(replaced.body, b"new");
    let kept = by_line(&requests, &put("NOTE/Note/b (3).txt"));
    assert_eq!(kept.header("if-none-match"), Some("*"));
    by_line(&requests, &put("NOTE/Note/d (2).txt"));
    assert_eq!(requests.len(), 6, "{:?}", lines(&requests));
    let events = take(&s);
    // The replaced bytes went to the bin.
    assert!(events.contains(&LibraryEvent::Recycle));
    let ended = last(
        &events
            .into_iter()
            .filter_map(|e| match e {
                LibraryEvent::Job { job } => Some(job),
                _ => None,
            })
            .collect::<Vec<_>>(),
    );
    assert_eq!(ended["a.txt"].state, JobState::Done);
    assert_eq!(
        ended["b.txt"].result.as_deref(),
        Some("NOTE/Note/b (3).txt")
    );
    assert_eq!(ended["c.txt"].state, JobState::Done);
    assert_eq!(ended["c.txt"].text.as_deref(), Some("Skipped."));
    assert_eq!(ended["c.txt"].result, None);
    assert_eq!(
        ended["d.txt"].result.as_deref(),
        Some("NOTE/Note/d (2).txt")
    );
    assert!(!ended.contains_key("e.txt"));
    // A job that does not ask takes no answer.
    assert!(s.library.answer(ended["a.txt"].id, Choice::Skip).is_err());
    s.library.cancel(asked["e.txt"].id);
}

#[test]
fn a_case_clash_names_the_other_file() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.txt", b"a");
    write(dir.path(), "Trip/b.txt", b"b");
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), not_found()),
        (
            put("NOTE/Note/a.txt"),
            error(
                "409 Conflict",
                json!({"error": "name_taken", "existing": "A.txt"}),
            ),
        ),
        (look("NOTE/Note/a (2).txt"), not_found()),
        (put("NOTE/Note/a (2).txt"), created("NOTE/Note/a (2).txt")),
        // A folder on the way in another case: no other name helps.
        (look("NOTE/Note/Trip/b.txt"), not_found()),
        (
            put("NOTE/Note/Trip/b.txt"),
            error(
                "409 Conflict",
                json!({"error": "name_taken", "existing": "trip"}),
            ),
        ),
    ]);
    let id = drop_paths(&s, vec![a, dir.path().join("Trip")]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let asked = last(&job_events(&s));
    assert_eq!(asked["a.txt"].state, JobState::Asking);
    assert_eq!(
        asked["a.txt"].question,
        Some(Clash {
            name: "a.txt".into(),
            existing: "A.txt".into(),
            can_replace: false,
        })
    );
    assert_eq!(
        asked["a.txt"].text.as_deref(),
        Some("There is already A.txt here.")
    );
    assert_eq!(asked["Trip/b.txt"].state, JobState::Failed);
    assert_eq!(
        asked["Trip/b.txt"].text.as_deref(),
        Some("There is already trip here.")
    );
    assert!(
        s.library
            .answer(asked["a.txt"].id, Choice::Replace)
            .is_err()
    );
    s.library
        .answer(asked["a.txt"].id, Choice::KeepBoth)
        .unwrap();
    s.library.join_uploads();
    let ended = last(&job_events(&s));
    assert_eq!(
        ended["a.txt"].result.as_deref(),
        Some("NOTE/Note/a (2).txt")
    );
    assert_eq!(s.server.received().len(), 6);
}

#[test]
fn an_upload_streams_with_progress() {
    let dir = tempfile::tempdir().unwrap();
    let len: usize = 8 << 20;
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let file = write(dir.path(), "big.bin", &bytes);
    let time = Time::default();
    let events = Events::default();
    let (moved, seen) = (time.clone(), events.clone());
    let mut once = false;
    // After the first piece the server reads on only once the window was shown progress.
    let each = Box::new(move |read: usize| {
        if once || read == 0 {
            return;
        }
        once = true;
        moved.pass(300);
        wait_for(&seen, |all| {
            all.iter().any(|e| {
                matches!(e, LibraryEvent::Job { job }
                    if job.state == JobState::Running && job.done > 0)
            })
        });
    });
    let s = setup_with(
        vec![
            (look("NOTE/Note/big.bin"), not_found()),
            (
                put("NOTE/Note/big.bin"),
                Act::Slow {
                    answer: put_answer("201 Created", "NOTE/Note/big.bin", "created", &[30]),
                    each,
                },
            ),
        ],
        time,
        events,
        |_| {},
    );
    let id = drop_paths(&s, vec![file]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let requests = s.server.received();
    let sent = by_line(&requests, &put("NOTE/Note/big.bin"));
    assert_eq!(
        sent.header("content-length"),
        Some(len.to_string().as_str())
    );
    assert_eq!(sent.header("transfer-encoding"), None);
    assert!(sent.body == bytes, "the body is not the file");
    let jobs = job_events(&s);
    let progress: Vec<u64> = jobs
        .iter()
        .filter(|j| j.state == JobState::Running)
        .map(|j| j.done)
        .collect();
    assert_eq!(progress.first(), Some(&0));
    assert!(
        progress.iter().any(|&d| d > 0 && d < len as u64),
        "{progress:?}"
    );
    assert!(progress.windows(2).all(|w| w[0] <= w[1]));
    let ended = jobs.last().unwrap();
    assert_eq!(ended.state, JobState::Done);
    assert_eq!((ended.done, ended.total), (len as u64, Some(len as u64)));
}

#[test]
fn a_stalled_upload_ends() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "big.bin", &vec![7u8; 8 << 20]);
    let (reached_tx, reached) = channel();
    let (release, release_rx) = channel();
    let (closed_tx, closed) = channel();
    let s = setup(vec![
        (look("NOTE/Note/big.bin"), not_found()),
        (
            put("NOTE/Note/big.bin"),
            Act::Stall {
                after: 64 * 1024,
                reached: reached_tx,
                release: release_rx,
                closed: closed_tx,
            },
        ),
    ]);
    let id = drop_paths(&s, vec![file]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    // The server reads nothing more. The client fills the socket's buffers first, so a minute
    // passes again and again until the counter has stood still for one.
    let until = Instant::now() + Duration::from_secs(5);
    while !s
        .events
        .all()
        .iter()
        .any(|e| is_job(e, "big.bin", JobState::Failed))
    {
        assert!(Instant::now() < until, "no stall: {:?}", s.events.all());
        s.time.pass(60_000);
        std::thread::sleep(Duration::from_millis(5));
    }
    s.library.join_uploads();
    let ended = job_events(&s).pop().unwrap();
    assert_eq!(ended.text.as_deref(), Some("The connection stalled."));
    // The request was dropped: the connection ended before the whole body came.
    release.send(()).unwrap();
    assert!(closed.recv_timeout(Duration::from_secs(5)).unwrap());
}

#[test]
fn an_upload_waits_while_a_device_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
        .iter()
        .map(|name| write(dir.path(), name, b"x"))
        .collect();
    let time = Time::default();
    let seen: Arc<Mutex<Vec<(String, u64)>>> = Arc::default();
    let (log, clock) = (seen.clone(), time.clock());
    let s = setup_with(
        vec![
            (look("NOTE/Note/a.txt"), not_found()),
            (put("NOTE/Note/a.txt"), syncing()),
            (put("NOTE/Note/a.txt"), created("NOTE/Note/a.txt")),
            (look("NOTE/Note/b.txt"), not_found()),
            (put("NOTE/Note/b.txt"), created("NOTE/Note/b.txt")),
            (look("NOTE/Note/c.txt"), not_found()),
            (put("NOTE/Note/c.txt"), created("NOTE/Note/c.txt")),
        ],
        time,
        Events::default(),
        move |line| log.lock().unwrap().push((line.to_owned(), clock())),
    );
    let id = drop_paths(&s, paths);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let jobs = job_events(&s);
    assert!(
        jobs.iter().any(|j| j.label == "a.txt"
            && j.state == JobState::Waiting
            && j.text.as_deref() == Some(WAITING)),
        "{jobs:?}"
    );
    assert!(last(&jobs).values().all(|j| j.state == JobState::Done));
    // Nothing was sent while the queue paused.
    let seen = seen.lock().unwrap().clone();
    let at = |key: &str, nth: usize| {
        seen.iter()
            .filter(|(l, _)| *l == key)
            .nth(nth)
            .unwrap_or_else(|| panic!("no {key} {nth}: {seen:?}"))
            .1
    };
    let paused = at(&put("NOTE/Note/a.txt"), 0);
    assert!(at(&put("NOTE/Note/a.txt"), 1) >= paused + 5_000, "{seen:?}");
    assert!(
        seen.iter()
            .all(|(_, t)| *t <= paused || *t >= paused + 5_000),
        "{seen:?}"
    );
}

#[test]
fn a_sync_that_lasts_two_minutes_ends_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
        .iter()
        .map(|name| write(dir.path(), name, b"x"))
        .collect();
    let mut routes = Vec::new();
    for name in ["a.txt", "b.txt", "c.txt"] {
        let path = format!("NOTE/Note/{name}");
        routes.push((look(&path), not_found()));
        for _ in 0..30 {
            routes.push((put(&path), syncing()));
        }
    }
    let s = setup(routes);
    let id = drop_paths(&s, paths);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let ended = last(&job_events(&s));
    assert_eq!(ended.len(), 3);
    for job in ended.values() {
        assert_eq!(job.state, JobState::Failed, "{job:?}");
        assert_eq!(job.text.as_deref(), Some(SYNC_FAILED));
    }
    let now = (s.time.clock())();
    assert!(now >= 120_000, "{now}");
    // The queue ended at the limit, not one wait per file.
    assert!(now < 200_000, "{now}");
}

#[test]
fn scan_hold_is_not_waited_for() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.txt", b"a");
    let b = write(dir.path(), "b.txt", b"b");
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), not_found()),
        (
            put("NOTE/Note/a.txt"),
            error("423 Locked", json!({"error": "scan_hold"})),
        ),
        (look("NOTE/Note/b.txt"), not_found()),
        (
            put("NOTE/Note/b.txt"),
            error("423 Locked", json!({"error": "not_imported"})),
        ),
    ]);
    let id = drop_paths(&s, vec![a, b]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let ended = last(&job_events(&s));
    assert_eq!(ended["a.txt"].state, JobState::Failed);
    assert_eq!(
        ended["a.txt"].text.as_deref(),
        Some("The server is reading the files of the Supernote. Try again in a moment.")
    );
    assert_eq!(
        ended["b.txt"].text.as_deref(),
        Some("The Supernote has not been imported yet.")
    );
    assert_eq!((s.time.clock())(), 0);
    assert_eq!(s.server.received().len(), 4);
    assert_eq!(s.library.state().unwrap().state, LibraryState::NotImported);
}

#[test]
fn a_reset_during_a_sync_waits() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.txt", &vec![1u8; 256 * 1024]);
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), not_found()),
        // An early refusal can come as a closed connection instead of the 423.
        (put("NOTE/Note/a.txt"), Act::Cut { after: 0 }),
        ("GET /rest/v1/account".into(), account(true)),
        (put("NOTE/Note/a.txt"), created("NOTE/Note/a.txt")),
    ]);
    let id = drop_paths(&s, vec![a]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let jobs = job_events(&s);
    assert!(
        jobs.iter()
            .any(|j| j.state == JobState::Waiting && j.text.as_deref() == Some(WAITING)),
        "{jobs:?}"
    );
    assert_eq!(jobs.last().unwrap().state, JobState::Done);
    assert!((s.time.clock())() >= 5_000);
    assert_eq!(s.server.received().len(), 4);
}

#[test]
fn a_cut_upload_reports_and_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = vec![3u8; 1 << 20];
    let a = write(dir.path(), "a.txt", &bytes);
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), not_found()),
        (put("NOTE/Note/a.txt"), Act::Cut { after: 64 * 1024 }),
        ("GET /rest/v1/account".into(), account(false)),
        // The retry goes on where it was: no new look.
        (
            put("NOTE/Note/a.txt"),
            Act::Answer(put_answer("200 OK", "NOTE/Note/a.txt", "unchanged", &[])),
        ),
    ]);
    let id = drop_paths(&s, vec![a]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let failed = job_events(&s).pop().unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.text.as_deref(), Some("The connection was cut."));
    assert_eq!(s.server.received().len(), 3);
    s.library.retry(failed.id).unwrap();
    s.library.join_uploads();
    let requests = s.server.received();
    assert_eq!(lines(&requests), [put("NOTE/Note/a.txt")]);
    assert_eq!(requests[0].header("if-none-match"), Some("*"));
    assert!(requests[0].body == bytes);
    let jobs = job_events(&s);
    assert_eq!(jobs.first().unwrap().state, JobState::Queued);
    let ended = jobs.last().unwrap();
    assert_eq!((ended.id, ended.state), (failed.id, JobState::Done));
    // Only a failed job is tried again.
    assert!(s.library.retry(failed.id).is_err());
}

#[test]
fn two_uploads_run_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
        .iter()
        .map(|name| write(dir.path(), name, b"x"))
        .collect();
    let (arrived, came) = channel();
    let (go_a, a_waits) = channel();
    let (go_b, b_waits) = channel();
    let s = setup(vec![
        (look("NOTE/Note/a.txt"), not_found()),
        (
            put("NOTE/Note/a.txt"),
            Act::Hold {
                answer: put_answer("201 Created", "NOTE/Note/a.txt", "created", &[1]),
                arrived: arrived.clone(),
                go: a_waits,
            },
        ),
        (look("NOTE/Note/b.txt"), not_found()),
        (
            put("NOTE/Note/b.txt"),
            Act::Hold {
                answer: put_answer("201 Created", "NOTE/Note/b.txt", "created", &[2]),
                arrived,
                go: b_waits,
            },
        ),
        (look("NOTE/Note/c.txt"), not_found()),
        (put("NOTE/Note/c.txt"), created("NOTE/Note/c.txt")),
    ]);
    let id = drop_paths(&s, paths);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    came.recv_timeout(Duration::from_secs(5)).unwrap();
    came.recv_timeout(Duration::from_secs(5)).unwrap();
    let now = last(&job_events(&s));
    assert_eq!(now["a.txt"].state, JobState::Running);
    assert_eq!(now["b.txt"].state, JobState::Running);
    assert_eq!(now["c.txt"].state, JobState::Queued);
    assert!(
        !lines(&s.server.received()).contains(&look("NOTE/Note/c.txt")),
        "c started while two ran"
    );
    go_a.send(()).unwrap();
    go_b.send(()).unwrap();
    s.library.join_uploads();
    let ended = last(&job_events(&s));
    assert!(
        ended.values().all(|j| j.state == JobState::Done),
        "{ended:?}"
    );
}

#[test]
fn cancel_all_stops_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.bin", &vec![5u8; 8 << 20]);
    let b = write(dir.path(), "b.txt", b"b");
    let c = write(dir.path(), "c.txt", b"c");
    let d = write(dir.path(), "d.txt", b"d");
    let (reached_tx, reached) = channel();
    let (release, release_rx) = channel();
    let (closed_tx, closed) = channel();
    let (arrived, came) = channel();
    let (go, waits) = channel();
    let s = setup(vec![
        (look("NOTE/Note/a.bin"), not_found()),
        (
            put("NOTE/Note/a.bin"),
            Act::Stall {
                after: 64 * 1024,
                reached: reached_tx,
                release: release_rx,
                closed: closed_tx,
            },
        ),
        (look("NOTE/Note/b.txt"), not_found()),
        (
            put("NOTE/Note/b.txt"),
            Act::Hold {
                answer: put_answer("201 Created", "NOTE/Note/b.txt", "created", &[1]),
                arrived,
                go: waits,
            },
        ),
        (look("NOTE/Note/d.txt"), found("NOTE/Note/d.txt", "file")),
        (
            "POST /rest/v1/recycle/restore".into(),
            error(
                "409 Conflict",
                json!({"error": "name_taken", "existing": "b.note"}),
            ),
        ),
    ]);
    let id = drop_paths(&s, vec![a, b, c]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    came.recv_timeout(Duration::from_secs(5)).unwrap();
    take(&s);
    s.library.cancel_drop(id);
    let cancelled = job_events(&s);
    assert_eq!(
        cancelled
            .iter()
            .map(|j| (j.label.as_str(), j.state))
            .collect::<Vec<_>>(),
        [
            ("a.bin", JobState::Cancelled),
            ("b.txt", JobState::Cancelled),
            ("c.txt", JobState::Cancelled),
        ]
    );
    s.library.join_uploads();
    // Nothing more came, and nothing ended otherwise.
    assert!(job_events(&s).is_empty());
    release.send(()).unwrap();
    assert!(closed.recv_timeout(Duration::from_secs(5)).unwrap());
    let _ = go.send(());
    assert!(
        !lines(&s.server.received()).contains(&look("NOTE/Note/c.txt")),
        "c was looked up"
    );

    // One job: an upload that asks, and a restore that asks.
    let id = drop_paths(&s, vec![d]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let asking = job_events(&s).pop().unwrap();
    assert_eq!(asking.state, JobState::Asking);
    s.library.cancel(asking.id);
    let ended = job_events(&s);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].id, ended[0].state),
        (asking.id, JobState::Cancelled)
    );
    assert_eq!(ended[0].question, None);
    assert!(s.library.answer(asking.id, Choice::Skip).is_err());
    let restore = s.library.restore("1763290000000/NOTE/Note/b.note", None);
    assert_eq!(restore.state, JobState::Asking);
    take(&s);
    s.library.cancel(restore.id);
    let ended = job_events(&s);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].id, ended[0].state),
        (restore.id, JobState::Cancelled)
    );
    // Cancelling a job that is gone does nothing.
    s.library.cancel(restore.id);
    s.library.cancel(9_999);
    assert!(take(&s).is_empty());
}

#[test]
fn a_new_binding_cancels_the_uploads_and_forgets_the_drops() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.txt", b"a");
    let b = write(dir.path(), "b.txt", b"b");
    let s = setup(vec![(
        look("NOTE/Note/a.txt"),
        found("NOTE/Note/a.txt", "file"),
    )]);
    let id = drop_paths(&s, vec![a]);
    let kept = drop_paths(&s, vec![b]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let asking = job_events(&s).pop().unwrap();
    assert_eq!(asking.state, JobState::Asking);
    s.library.relogin();
    let ended = job_events(&s);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].id, ended[0].state),
        (asking.id, JobState::Cancelled)
    );
    assert_eq!(
        s.library.upload(kept, NOTE, false, None),
        Err(DROP_GONE.to_owned())
    );
}

/// Sets the mode of `path` (and puts it back when dropped, so the temp folder can go).
struct Mode(PathBuf);

impl Mode {
    fn set(path: PathBuf, mode: u32) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        Self(path)
    }
}

impl Drop for Mode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_folder_that_cannot_be_read_fails() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    write(d, "Trip/a.txt", b"a");
    write(d, "Trip/Locked/b.txt", b"b");
    let _locked = Mode::set(d.join("Trip/Locked"), 0o000);
    let plan = walk(&[d.join("Trip")], false, None, &AtomicBool::new(false));
    assert_eq!(plan.files, 1);
    assert_eq!(plan.unreadable, 1);
    let locked = plan
        .items
        .iter()
        .find(|p| p.names == ["Trip", "Locked"])
        .expect("the locked folder is planned");
    assert!(locked.unreadable);

    let s = setup(vec![
        (look("NOTE/Note/Trip/a.txt"), not_found()),
        (put("NOTE/Note/Trip/a.txt"), created("NOTE/Note/Trip/a.txt")),
    ]);
    let id = drop_paths(&s, vec![d.join("Trip")]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    // No empty folder is made for it.
    assert_eq!(
        lines(&s.server.received()),
        [look("NOTE/Note/Trip/a.txt"), put("NOTE/Note/Trip/a.txt")]
    );
    let ended = last(&job_events(&s));
    assert_eq!(ended["Trip/a.txt"].state, JobState::Done);
    assert_eq!(ended["Trip/Locked"].state, JobState::Failed);
    assert_eq!(
        ended["Trip/Locked"].text.as_deref(),
        Some("This folder could not be read.")
    );
    // A retry fails again, without a request.
    s.library.retry(ended["Trip/Locked"].id).unwrap();
    s.library.join_uploads();
    assert!(s.server.received().is_empty());
    assert_eq!(last(&job_events(&s))["Trip/Locked"].state, JobState::Failed);
}

#[test]
fn the_walk_stops_at_its_cap_and_on_cancel() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..10 {
        write(dir.path(), &format!("Many/{n}.txt"), b"x");
    }
    let many = [dir.path().join("Many")];
    let plan = walk(&many, false, Some(5), &AtomicBool::new(false));
    assert!(plan.capped);
    assert!(plan.items.len() < 10);
    assert_eq!(
        ask_for(&plan, false, None),
        Some(UploadAsk {
            ask: Ask::Many,
            text: "This drop holds more than 10,000 items. Upload them all?".into(),
        })
    );
    assert_eq!(ask_for(&plan, true, None), None);
    let all = walk(&many, false, None, &AtomicBool::new(false));
    assert!(!all.capped);
    assert_eq!(all.files, 10);
    // A cancelled drop stops the walk.
    let stopped = walk(&many, false, None, &AtomicBool::new(true));
    assert!(stopped.cancelled);
    assert!(stopped.items.is_empty());
    assert_eq!(WALK_CAP, 10_000);
}

#[test]
fn a_keep_both_after_a_race_counts_from_the_first_name() {
    let dir = tempfile::tempdir().unwrap();
    let b = write(dir.path(), "b.txt", b"b");
    let s = setup(vec![
        (look("NOTE/Note/b.txt"), found("NOTE/Note/b.txt", "file")),
        (look("NOTE/Note/b (2).txt"), not_found()),
        // Someone else took it meanwhile.
        (
            put("NOTE/Note/b (2).txt"),
            error(
                "412 Precondition Failed",
                json!({"error": "precondition_failed"}),
            ),
        ),
        (
            look("NOTE/Note/b (2).txt"),
            found("NOTE/Note/b (2).txt", "file"),
        ),
        (
            look("NOTE/Note/b (2).txt"),
            found("NOTE/Note/b (2).txt", "file"),
        ),
        (look("NOTE/Note/b (3).txt"), not_found()),
        (put("NOTE/Note/b (3).txt"), created("NOTE/Note/b (3).txt")),
    ]);
    let id = drop_paths(&s, vec![b]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let asked = job_events(&s).pop().unwrap();
    s.library.answer(asked.id, Choice::KeepBoth).unwrap();
    s.library.join_uploads();
    let again = job_events(&s).pop().unwrap();
    assert_eq!(again.state, JobState::Asking, "{again:?}");
    assert_eq!(again.question.as_ref().unwrap().name, "b (2).txt");
    s.library.answer(again.id, Choice::KeepBoth).unwrap();
    s.library.join_uploads();
    let ended = job_events(&s).pop().unwrap();
    assert_eq!(ended.result.as_deref(), Some("NOTE/Note/b (3).txt"));
    assert_eq!(s.server.received().len(), 7);
}

#[test]
fn a_cancel_during_keep_both_stops_the_looks() {
    let dir = tempfile::tempdir().unwrap();
    let b = write(dir.path(), "b.txt", b"b");
    let (arrived, came) = channel();
    let (go, waits) = channel();
    let s = setup(vec![
        (look("NOTE/Note/b.txt"), found("NOTE/Note/b.txt", "file")),
        (
            look("NOTE/Note/b (2).txt"),
            Act::Hold {
                answer: answer(
                    "200 OK",
                    &json!({"id": 9, "path": "NOTE/Note/b (2).txt", "name": "b (2).txt",
                            "kind": "file", "size": 3, "md5": MD5_A, "mtimeMs": 1,
                            "writable": true})
                    .to_string(),
                ),
                arrived,
                go: waits,
            },
        ),
        (look("NOTE/Note/b (3).txt"), not_found()),
    ]);
    let id = drop_paths(&s, vec![b]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let asked = job_events(&s).pop().unwrap();
    s.library.answer(asked.id, Choice::KeepBoth).unwrap();
    came.recv_timeout(Duration::from_secs(5)).unwrap();
    s.library.cancel(asked.id);
    go.send(()).unwrap();
    s.library.join_uploads();
    let requests = s.server.received();
    assert!(
        !lines(&requests).contains(&look("NOTE/Note/b (3).txt")),
        "{:?}",
        lines(&requests)
    );
}

#[test]
fn a_source_swapped_for_a_symlink_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.txt", b"a");
    write(dir.path(), "secret.txt", b"secret");
    let s = setup(vec![(
        look("NOTE/Note/a.txt"),
        found("NOTE/Note/a.txt", "file"),
    )]);
    let id = drop_paths(&s, vec![a.clone()]);
    assert_eq!(s.library.upload(id, NOTE, false, None), Ok(None));
    s.library.join_uploads();
    let asked = job_events(&s).pop().unwrap();
    assert_eq!(lines(&s.server.received()), [look("NOTE/Note/a.txt")]);
    // While the window asks, the dropped file becomes a symlink to another file.
    std::fs::remove_file(&a).unwrap();
    symlink(dir.path().join("secret.txt"), &a);
    s.library.answer(asked.id, Choice::Replace).unwrap();
    s.library.join_uploads();
    let ended = job_events(&s).pop().unwrap();
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(ended.text.as_deref(), Some("This file could not be read."));
    assert!(s.server.received().is_empty());
}
