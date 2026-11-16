//! Downloads against a routed server on loopback, into a temp folder that stands for Downloads
//! (never the real one). The stall watchdog reads a fake clock, so the test says when a minute
//! has passed. Each test joins the download threads before it ends.

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use md5::{Digest as _, Md5};
use serde_json::{Value, json};

use super::listing_tests::Time;
use super::tests::{CountingStore, Events, Place, TOKEN, remote_key};
use super::*;
use crate::rest::test_http::{Act, Request, Routed, answer, file_answer};

const HELLO_MD5: &str = "5d41402abc4b2a76b9719d911017c592";
const ENTRY: &str = "GET /rest/v1/entries/41";
const FILE: &str = "GET /rest/v1/files/41";
const NOT_WHOLE: &str = "The file did not arrive whole. Try again.";
const CHANGED: &str = "The file was changed on the server. Try again.";
const BAD_NAME: &str = "The name of this file cannot be used on this Mac.";
const NO_MD5: &str = "The server sent no valid md5 for this file.";

struct Setup {
    server: Routed,
    library: Library,
    events: Events,
    time: Time,
    /// The folder that holds `dir`: nothing may land here but `dir`.
    root: tempfile::TempDir,
    /// The Downloads folder of the test.
    dir: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        // No download thread outlives its test.
        self.library.join_downloads();
    }
}

fn setup(routes: Vec<(String, Act)>) -> Setup {
    setup_with(routes, |_| {})
}

/// [`setup`], and `hook` runs with each request line as it comes.
fn setup_with(routes: Vec<(String, Act)>, hook: impl Fn(&str) + Send + Sync + 'static) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("Downloads");
    std::fs::create_dir(&dir).unwrap();
    setup_in(routes, hook, root, dir)
}

fn setup_in(
    routes: Vec<(String, Act)>,
    hook: impl Fn(&str) + Send + Sync + 'static,
    root: tempfile::TempDir,
    dir: PathBuf,
) -> Setup {
    let server = Routed::start_with(routes, hook);
    let store = CountingStore::with(&remote_key(server.port, "jordi"), TOKEN);
    let place = Place::remote(server.port, Some("jordi"));
    let time = Time::default();
    let events = Events::default();
    let sleeper: Sleeper = Arc::new(|_| {});
    let library = Library::with_timing(store, place.func(), events.emit(), time.clock(), sleeper);
    library.set_downloads(dir.clone());
    library.refresh();
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    events.0.lock().unwrap().clear();
    Setup {
        server,
        library,
        events,
        time,
        root,
        dir,
    }
}

fn md5_hex(bytes: &[u8]) -> String {
    crate::login::hex(&Md5::digest(bytes))
}

fn entry(name: &str, md5: Value, size: u64) -> Act {
    Act::Answer(answer(
        "200 OK",
        &json!({"id": 41, "path": format!("NOTE/Note/{name}"), "name": name, "kind": "file",
                "size": size, "md5": md5, "mtimeMs": 1, "writable": true})
        .to_string(),
    ))
}

fn hello_entry(name: &str) -> Act {
    entry(name, json!(HELLO_MD5), 5)
}

fn tag(md5: &str) -> String {
    format!("\"{md5}\"")
}

/// The whole file, 200 with its ETag.
fn whole(body: &[u8]) -> Act {
    Act::Raw(file_answer(
        "200 OK",
        &[("etag", &tag(&md5_hex(body)))],
        body,
    ))
}

fn hello() -> Act {
    whole(b"hello")
}

/// 200 with the bytes of `body` and the ETag of `md5`, cut off after `after` bytes of the body.
fn cut_after(body: &[u8], after: usize) -> Act {
    let reply = file_answer("200 OK", &[("etag", &tag(&md5_hex(body)))], body);
    let head = reply.len() - body.len();
    Act::Partial {
        reply,
        after: head + after,
    }
}

/// Bytes that do not repeat in short steps, so a wrong offset shows.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
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

fn last_job(s: &Setup) -> Job {
    job_events(s).pop().expect("no job event")
}

fn by_line<'a>(requests: &'a [Request], key: &str) -> Vec<&'a Request> {
    requests
        .iter()
        .filter(|r| format!("{} {}", r.method, r.target) == key)
        .collect()
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut all: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    all.sort();
    all
}

fn part(s: &Setup, name: &str, job: u64) -> PathBuf {
    s.dir.join(format!(".{name}.{job}.part"))
}

/// Downloads entry 41 and waits until its threads end; answers the job as it ended.
fn download(s: &Setup) -> Job {
    let queued = s.library.download(41).expect("the download did not start");
    assert_eq!(queued.kind, JobKind::Download);
    assert_eq!(queued.state, JobState::Queued);
    s.library.join_downloads();
    let ended = last_job(s);
    assert_eq!(ended.id, queued.id);
    ended
}

#[test]
fn a_download_lands_in_downloads() {
    let s = setup(vec![
        (ENTRY.into(), hello_entry("a.txt")),
        (FILE.into(), hello()),
    ]);
    let queued = s.library.download(41).unwrap();
    assert_eq!(queued.label, "a.txt");
    assert_eq!(queued.total, Some(5));
    s.library.join_downloads();
    let ended = last_job(&s);
    assert_eq!(ended.state, JobState::Done, "{ended:?}");
    assert_eq!(ended.done, 5);
    let target = s.dir.join("a.txt");
    assert_eq!(
        ended.result.as_deref(),
        Some(target.to_str().unwrap()),
        "the result is the absolute path"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    // The temp file is gone and nothing else is there.
    assert_eq!(names(&s.dir), ["a.txt"]);
    assert_eq!(names(s.root.path()), ["Downloads"]);
    let requests = s.server.received();
    let file = by_line(&requests, FILE);
    assert_eq!(file.len(), 1);
    assert_eq!(
        file[0].header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    // A first try asks for the whole file.
    assert_eq!(file[0].header("range"), None);
    assert_eq!(file[0].header("if-range"), None);
}

#[test]
fn a_download_never_overwrites() {
    // `a.txt` is there before; `a (2).txt` appears while the bytes come.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("Downloads");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"old").unwrap();
    let racer = dir.join("a (2).txt");
    let s = setup_in(
        vec![(ENTRY.into(), hello_entry("a.txt")), (FILE.into(), hello())],
        move |line| {
            if line == FILE {
                std::fs::write(&racer, b"racer").unwrap();
            }
        },
        root,
        dir,
    );
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Done, "{ended:?}");
    let target = s.dir.join("a (3).txt");
    assert_eq!(ended.result.as_deref(), Some(target.to_str().unwrap()));
    assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    assert_eq!(std::fs::read(s.dir.join("a.txt")).unwrap(), b"old");
    assert_eq!(std::fs::read(s.dir.join("a (2).txt")).unwrap(), b"racer");
    assert_eq!(names(&s.dir), ["a (2).txt", "a (3).txt", "a.txt"]);
}

#[test]
fn a_download_name_with_a_slash_or_dots_stays_in_downloads() {
    let long = "x".repeat(256);
    let bad = [
        "../evil.txt",
        "a/b.txt",
        "/etc/evil",
        "..",
        ".",
        ".hidden",
        "x\u{0}y",
        "",
        long.as_str(),
    ];
    // The longest name that is allowed: 255 bytes, with a copy of it already there.
    let longest = format!("{}.txt", "y".repeat(251));
    assert_eq!(longest.len(), 255);
    let mut routes: Vec<(String, Act)> = bad
        .iter()
        .map(|name| (ENTRY.to_owned(), hello_entry(name)))
        .collect();
    routes.push((ENTRY.into(), hello_entry(&longest)));
    routes.push((FILE.into(), hello()));
    let s = setup(routes);
    for name in bad {
        assert_eq!(
            s.library.download(41).map(|j| j.label),
            Err(BAD_NAME.to_owned()),
            "{name:?}"
        );
    }
    // No byte was asked for, nothing was written.
    assert!(by_line(&s.server.received(), FILE).is_empty());
    assert!(names(&s.dir).is_empty());
    assert_eq!(names(s.root.path()), ["Downloads"]);

    std::fs::write(s.dir.join(&longest), b"old").unwrap();
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Done, "{ended:?}");
    // The copy keeps its extension and fits in 255 bytes.
    let copy = format!("{} (2).txt", "y".repeat(247));
    assert_eq!(copy.len(), 255);
    assert_eq!(
        ended.result.as_deref(),
        Some(s.dir.join(&copy).to_str().unwrap())
    );
    assert_eq!(std::fs::read(s.dir.join(&copy)).unwrap(), b"hello");
    assert_eq!(names(s.root.path()), ["Downloads"]);
}

#[test]
fn an_md5_that_is_no_md5_is_refused() {
    let bad = [
        json!("../../x"),
        json!(null),
        json!(HELLO_MD5.to_uppercase()),
        json!(&HELLO_MD5[1..]),
        json!(format!("{HELLO_MD5}0")),
        json!(format!("{}/", &HELLO_MD5[1..])),
    ];
    let routes = bad
        .iter()
        .map(|md5| (ENTRY.to_owned(), entry("a.txt", md5.clone(), 5)))
        .collect();
    let s = setup(routes);
    for md5 in &bad {
        assert_eq!(
            s.library.download(41).map(|j| j.label),
            Err(NO_MD5.to_owned()),
            "{md5}"
        );
    }
    assert!(by_line(&s.server.received(), FILE).is_empty());
    assert!(names(&s.dir).is_empty());
}

#[test]
fn bytes_that_do_not_match_the_md5_are_dropped() {
    let s = setup(vec![
        (ENTRY.into(), hello_entry("a.txt")),
        // The ETag says hello, the bytes are not.
        (
            FILE.into(),
            Act::Raw(file_answer(
                "200 OK",
                &[("etag", &tag(HELLO_MD5))],
                b"hellx",
            )),
        ),
        // A retry asks for the entry again and starts from the first byte.
        (ENTRY.into(), hello_entry("a.txt")),
        (FILE.into(), hello()),
        // The file changed on the server between the entry and the bytes.
        (ENTRY.into(), hello_entry("b.txt")),
        (FILE.into(), whole(b"other")),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(ended.text.as_deref(), Some(NOT_WHOLE));
    assert!(names(&s.dir).is_empty(), "{:?}", names(&s.dir));

    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    let again = last_job(&s);
    assert_eq!(again.state, JobState::Done, "{again:?}");
    assert_eq!(std::fs::read(s.dir.join("a.txt")).unwrap(), b"hello");
    let requests = s.server.received();
    let files = by_line(&requests, FILE);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|r| r.header("range").is_none()));

    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(ended.text.as_deref(), Some(CHANGED));
    assert_eq!(names(&s.dir), ["a.txt"]);
}

#[test]
fn a_download_resumes() {
    let body = pattern(100_000);
    let md5 = md5_hex(&body);
    let rest = file_answer(
        "206 Partial Content",
        &[
            ("etag", &tag(&md5)),
            ("content-range", "bytes 40000-99999/100000"),
        ],
        &body[40_000..],
    );
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 100_000)),
        (FILE.into(), cut_after(&body, 40_000)),
        (ENTRY.into(), entry("big.bin", json!(md5), 100_000)),
        (FILE.into(), Act::Raw(rest)),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed, "{ended:?}");
    assert_eq!(ended.text.as_deref(), Some("The connection was cut."));
    // What came is kept for the retry, out of sight.
    let kept = part(&s, "big.bin", ended.id);
    assert_eq!(std::fs::read(&kept).unwrap(), &body[..40_000]);
    assert_eq!(names(&s.dir), [format!(".big.bin.{}.part", ended.id)]);

    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    let again = last_job(&s);
    assert_eq!(again.state, JobState::Done, "{again:?}");
    assert_eq!(again.done, 100_000);
    assert_eq!(std::fs::read(s.dir.join("big.bin")).unwrap(), body);
    assert_eq!(names(&s.dir), ["big.bin"]);
    let requests = s.server.received();
    let files = by_line(&requests, FILE);
    assert_eq!(files[1].header("range"), Some("bytes=40000-"));
    assert_eq!(files[1].header("if-range"), Some(tag(&md5).as_str()));
}

#[test]
fn a_resume_that_gets_200_starts_again() {
    let body = pattern(100_000);
    let md5 = md5_hex(&body);
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 100_000)),
        (FILE.into(), cut_after(&body, 40_000)),
        (ENTRY.into(), entry("big.bin", json!(md5), 100_000)),
        // The server sends the whole file again (its If-Range did not hold).
        (FILE.into(), whole(&body)),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed, "{ended:?}");
    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    let again = last_job(&s);
    assert_eq!(again.state, JobState::Done, "{again:?}");
    // Not the first 40,000 bytes and then the whole file.
    assert_eq!(std::fs::read(s.dir.join("big.bin")).unwrap(), body);
    assert_eq!(names(&s.dir), ["big.bin"]);
    let requests = s.server.received();
    assert_eq!(
        by_line(&requests, FILE)[1].header("range"),
        Some("bytes=40000-")
    );
}

#[test]
fn a_stalled_download_ends() {
    let body = pattern(100_000);
    let md5 = md5_hex(&body);
    let (reached_tx, reached) = channel();
    let (release, release_rx) = channel();
    let (closed_tx, closed) = channel();
    let reply = file_answer("200 OK", &[("etag", &tag(&md5))], &body);
    let head = reply.len() - body.len();
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 100_000)),
        (
            FILE.into(),
            Act::Trickle {
                reply,
                after: head + 1_000,
                reached: reached_tx,
                release: release_rx,
                closed: closed_tx,
            },
        ),
    ]);
    let queued = s.library.download(41).unwrap();
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    // Wait until the 1,000 bytes are written, then let a minute pass with nothing new.
    let kept = part(&s, "big.bin", queued.id);
    let until = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&kept).map(|m| m.len()).unwrap_or(0) < 1_000 {
        assert!(Instant::now() < until, "the bytes did not come");
        std::thread::sleep(Duration::from_millis(5));
    }
    let until = Instant::now() + Duration::from_secs(5);
    while !s.events.all().iter().any(|e| {
        matches!(e, LibraryEvent::Job { job } if job.id == queued.id && job.state == JobState::Failed)
    }) {
        assert!(Instant::now() < until, "no stall: {:?}", s.events.all());
        s.time.pass(60_000);
        std::thread::sleep(Duration::from_millis(5));
    }
    s.library.join_downloads();
    let ended = last_job(&s);
    assert_eq!(ended.text.as_deref(), Some("The connection stalled."));
    // The request was dropped; what came is kept for a retry.
    release.send(()).unwrap();
    assert!(closed.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(std::fs::read(&kept).unwrap(), &body[..1_000]);

    // A cancel removes it.
    s.library.cancel(queued.id);
    assert_eq!(last_job(&s).state, JobState::Cancelled);
    assert!(names(&s.dir).is_empty());
}

#[test]
fn stale_part_files_are_removed() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("Downloads");
    std::fs::create_dir(&dir).unwrap();
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, b"keep me").unwrap();
    // Temp files of an earlier run.
    for stale in [".a.txt.3.part", ".b.12.part"] {
        std::fs::write(dir.join(stale), b"old").unwrap();
    }
    std::os::unix::fs::symlink(&outside, dir.join(".c.1.part")).unwrap();
    // Look-alikes that are not.
    for keep in [
        ".keep.part",
        "notes.7.part",
        ".d.x1.part",
        ".e.4.part.txt",
        ".7.part",
    ] {
        std::fs::write(dir.join(keep), b"mine").unwrap();
    }
    std::fs::create_dir(dir.join(".f.2.part")).unwrap();
    let s = setup_in(
        vec![
            (ENTRY.into(), hello_entry("a.txt")),
            (FILE.into(), hello()),
            (ENTRY.into(), hello_entry("b.txt")),
            (FILE.into(), hello()),
        ],
        |_| {},
        root,
        dir,
    );
    // Nothing is touched before the first download.
    assert!(s.dir.join(".b.12.part").exists());
    assert_eq!(download(&s).state, JobState::Done);
    assert_eq!(
        names(&s.dir),
        [
            ".7.part",
            ".d.x1.part",
            ".e.4.part.txt",
            ".f.2.part",
            ".keep.part",
            "a.txt",
            "notes.7.part"
        ]
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");

    // Only once a run: a temp file of this run stays.
    std::fs::write(s.dir.join(".g.9.part"), b"this run").unwrap();
    assert_eq!(download(&s).state, JobState::Done);
    assert!(s.dir.join(".g.9.part").exists());
}

#[test]
fn show_in_finder_passes_one_argument() {
    let name = "-R x.txt";
    let s = setup(vec![
        (ENTRY.into(), hello_entry(name)),
        (FILE.into(), hello()),
    ]);
    let other = s.dir.join("other.txt");
    std::fs::write(&other, b"not ours").unwrap();
    // Nothing downloaded yet: nothing is shown.
    assert!(s.library.reveal_command(other.to_str().unwrap()).is_err());

    let ended = download(&s);
    assert_eq!(ended.state, JobState::Done, "{ended:?}");
    let path = ended.result.unwrap();
    let command = s.library.reveal_command(&path).unwrap();
    assert_eq!(command.get_program(), "/usr/bin/open");
    let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
    assert_eq!(args, ["-R", "--", path.as_str()]);
    assert!(Path::new(&path).is_absolute());

    // Only what Notedown downloaded in this run, as it named it.
    for refused in [
        other.to_str().unwrap(),
        name,
        "/etc/passwd",
        &format!("{}/../Downloads/{name}", s.dir.display()),
        "",
    ] {
        assert!(s.library.reveal_command(refused).is_err(), "{refused}");
    }
    // A file that is gone since says so.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        s.library.reveal_command(&path).map(|_| ()),
        Err("This file is not there anymore.".to_owned())
    );
}

#[test]
fn a_new_binding_cancels_downloads() {
    let body = pattern(10_000);
    let md5 = md5_hex(&body);
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 10_000)),
        (FILE.into(), cut_after(&body, 4_000)),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed, "{ended:?}");
    assert!(part(&s, "big.bin", ended.id).exists());
    s.library.relogin();
    let cancelled = job_events(&s).pop().unwrap();
    assert_eq!(
        (cancelled.id, cancelled.state),
        (ended.id, JobState::Cancelled)
    );
    assert!(names(&s.dir).is_empty());
    assert_eq!(
        s.library.retry(ended.id),
        Err(super::uploads::JOB_GONE.to_owned())
    );
}

#[test]
fn two_downloads_run_at_a_time() {
    let body = pattern(100_000);
    let md5 = md5_hex(&body);
    let (reached_tx, reached) = channel();
    let (release, release_rx) = channel();
    let (closed_tx, _closed) = channel();
    let reply = file_answer("200 OK", &[("etag", &tag(&md5))], &body);
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 100_000)),
        (
            FILE.into(),
            Act::Trickle {
                reply,
                after: 0,
                reached: reached_tx,
                release: release_rx,
                closed: closed_tx,
            },
        ),
        (
            "GET /rest/v1/entries/42".into(),
            Act::Answer(answer(
                "200 OK",
                &json!({"id": 42, "path": "NOTE/Note/a.txt", "name": "a.txt", "kind": "file",
                        "size": 5, "md5": HELLO_MD5, "mtimeMs": 1, "writable": true})
                .to_string(),
            )),
        ),
        ("GET /rest/v1/files/42".into(), hello()),
    ]);
    let held = s.library.download(41).unwrap();
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    // While the first one waits for its bytes, the second one runs and ends.
    let second = s.library.download(42).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !s.events.all().iter().any(|e| {
        matches!(e, LibraryEvent::Job { job } if job.id == second.id && job.state == JobState::Done)
    }) {
        assert!(Instant::now() < until, "the second download waited");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(std::fs::read(s.dir.join("a.txt")).unwrap(), b"hello");
    s.library.cancel(held.id);
    release.send(()).unwrap();
    s.library.join_downloads();
}

#[test]
fn a_late_cancel_still_reports_done() {
    let body = pattern(10_000);
    let md5 = md5_hex(&body);
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 10_000)),
        (FILE.into(), cut_after(&body, 4_000)),
    ]);
    let ended = download(&s);
    s.library.cancel(ended.id);
    assert_eq!(last_job(&s).state, JobState::Cancelled);
    // The run had placed the file when the cancel came: Done wins and the file stays.
    let placed = s.dir.join("big.bin");
    std::fs::write(&placed, &body).unwrap();
    s.library.settle_download(
        ended.id,
        super::downloads::End::Done {
            path: placed.clone(),
            size: 10_000,
            label: "big.bin".to_owned(),
        },
    );
    let done = last_job(&s);
    assert_eq!((done.id, done.state), (ended.id, JobState::Done));
    assert_eq!(done.result.as_deref(), Some(placed.to_str().unwrap()));
    assert_eq!(std::fs::read(&placed).unwrap(), body);
    assert!(s.library.reveal_command(placed.to_str().unwrap()).is_ok());
}

#[test]
fn a_first_run_never_opens_an_existing_temp_file() {
    let s = setup(vec![
        (ENTRY.into(), hello_entry("a.txt")),
        (FILE.into(), hello()),
        (ENTRY.into(), hello_entry("b.txt")),
        (FILE.into(), hello()),
    ]);
    // The first download sweeps; then a file takes the temp name of the next job.
    assert_eq!(download(&s).state, JobState::Done);
    let planted = part(&s, "b.txt", 2);
    std::fs::write(&planted, b"hel").unwrap();
    let ended = download(&s);
    assert_eq!(ended.id, 2);
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(
        ended.text.as_deref(),
        Some("The file could not be saved in the Downloads folder.")
    );
    assert_eq!(std::fs::read(&planted).unwrap(), b"hel");
    assert_eq!(by_line(&s.server.received(), FILE).len(), 1);
}

#[test]
fn a_short_body_is_a_cut_and_resumes() {
    let rest = file_answer(
        "206 Partial Content",
        &[("etag", &tag(HELLO_MD5)), ("content-range", "bytes 3-4/5")],
        b"lo",
    );
    let s = setup(vec![
        (ENTRY.into(), hello_entry("a.txt")),
        // A whole answer of 3 bytes for a file of 5.
        (
            FILE.into(),
            Act::Raw(file_answer("200 OK", &[("etag", &tag(HELLO_MD5))], b"hel")),
        ),
        (ENTRY.into(), hello_entry("a.txt")),
        (FILE.into(), Act::Raw(rest)),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(ended.text.as_deref(), Some("The connection was cut."));
    assert_eq!(std::fs::read(part(&s, "a.txt", ended.id)).unwrap(), b"hel");
    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    assert_eq!(last_job(&s).state, JobState::Done);
    assert_eq!(std::fs::read(s.dir.join("a.txt")).unwrap(), b"hello");
    let requests = s.server.received();
    assert_eq!(
        by_line(&requests, FILE)[1].header("range"),
        Some("bytes=3-")
    );
}

#[test]
fn a_retry_with_all_bytes_only_places() {
    let body = pattern(10_000);
    let md5 = md5_hex(&body);
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 10_000)),
        (FILE.into(), cut_after(&body, 4_000)),
        (ENTRY.into(), entry("big.bin", json!(md5), 10_000)),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed);
    // All bytes are there by now (a run that failed after the body).
    std::fs::write(part(&s, "big.bin", ended.id), &body).unwrap();
    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    let again = last_job(&s);
    assert_eq!(again.state, JobState::Done, "{again:?}");
    assert_eq!(std::fs::read(s.dir.join("big.bin")).unwrap(), body);
    assert_eq!(names(&s.dir), ["big.bin"]);
    // No second request for the bytes.
    assert_eq!(by_line(&s.server.received(), FILE).len(), 1);
}

#[test]
fn a_retry_after_the_file_changed_starts_again() {
    let old = pattern(10_000);
    let new = b"a new version".to_vec();
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5_hex(&old)), 10_000)),
        (FILE.into(), cut_after(&old, 4_000)),
        (
            ENTRY.into(),
            entry("big.bin", json!(md5_hex(&new)), new.len() as u64),
        ),
        (FILE.into(), whole(&new)),
    ]);
    let ended = download(&s);
    assert_eq!(ended.state, JobState::Failed);
    assert!(part(&s, "big.bin", ended.id).exists());
    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    let again = last_job(&s);
    assert_eq!(again.state, JobState::Done, "{again:?}");
    assert_eq!(again.total, Some(new.len() as u64));
    assert_eq!(std::fs::read(s.dir.join("big.bin")).unwrap(), new);
    assert_eq!(names(&s.dir), ["big.bin"]);
    let requests = s.server.received();
    let files = by_line(&requests, FILE);
    assert_eq!(files[1].header("range"), None);
    assert_eq!(files[1].header("if-range"), None);
}

#[test]
fn a_retry_with_wrong_full_bytes_fetches_again() {
    let body = pattern(10_000);
    let md5 = md5_hex(&body);
    let s = setup(vec![
        (ENTRY.into(), entry("big.bin", json!(md5), 10_000)),
        (FILE.into(), cut_after(&body, 4_000)),
        (ENTRY.into(), entry("big.bin", json!(md5), 10_000)),
        (FILE.into(), whole(&body)),
    ]);
    let ended = download(&s);
    std::fs::write(part(&s, "big.bin", ended.id), vec![0u8; 10_000]).unwrap();
    s.library.retry(ended.id).unwrap();
    s.library.join_downloads();
    assert_eq!(last_job(&s).state, JobState::Done);
    assert_eq!(std::fs::read(s.dir.join("big.bin")).unwrap(), body);
    let requests = s.server.received();
    assert_eq!(by_line(&requests, FILE)[1].header("range"), None);
}
