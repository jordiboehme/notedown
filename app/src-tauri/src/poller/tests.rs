//! The poller with fake sources and a fake clock; the real sources against shell scripts and
//! loopback listeners. No real sidecar runs and nothing leaves loopback.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Condvar;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Instant;

use super::*;
use crate::notify::STALE_MS;
use crate::notify::store::{KeyRecord, SyncCount};
use crate::probe::test_server::{TestServer, accept_within, free_port, http};
use crate::status::Attention;
use crate::supervisor::ChildState;
use crate::token::MemoryStore;

const WAIT: Duration = Duration::from_secs(10);
const T0: u64 = 1_763_300_000_000;
const SEC: u64 = 1_000;

const FACTS_BUSY: &str = r#"{"account": "jordi", "running": true, "pending": 1, "queued": 2,
    "waiting": 0, "lastError": null, "inWork": null, "consumers": []}"#;
const FACTS_IDLE: &str = r#"{"account": "jordi", "running": true, "pending": 0, "queued": 0,
    "waiting": 0, "lastError": null, "inWork": null, "consumers": []}"#;

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < WAIT, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Sources that count and record their calls, answer what they are told and can be held.
#[derive(Default)]
struct Fake {
    pipeline_args: Mutex<Vec<Vec<String>>>,
    account_calls: Mutex<Vec<(String, bool)>>,
    pipeline_answer: Mutex<Option<PipelineCall>>,
    account_answer: Mutex<Option<AccountAnswer>>,
    /// The bases `events_end` was asked for, and its answer.
    events_calls: Mutex<Vec<String>>,
    events_answer: Mutex<Option<Result<u64, String>>>,
    /// The marks `first_sync` was asked for (mode and seq), and its answer.
    check_calls: Mutex<Vec<(Mode, u64)>>,
    check_answer: Mutex<Option<Result<SyncCheck, String>>>,
    /// The cursors `device_changes` was asked for, and its answer.
    changes_calls: Mutex<Vec<u64>>,
    changes_answer: Mutex<Option<Result<DeviceChanges, String>>>,
    /// While true, every call waits.
    held: Mutex<bool>,
    released: Condvar,
    running: AtomicUsize,
    most_at_once: AtomicUsize,
}

impl Fake {
    fn hold(&self, held: bool) {
        *lock(&self.held) = held;
        self.released.notify_all();
    }

    fn enter(&self) {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_at_once.fetch_max(now, Ordering::SeqCst);
        let mut held = lock(&self.held);
        while *held {
            held = self
                .released
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn leave(&self) {
        self.running.fetch_sub(1, Ordering::SeqCst);
    }

    fn pipeline_calls(&self) -> usize {
        lock(&self.pipeline_args).len()
    }

    fn account_count(&self) -> usize {
        lock(&self.account_calls).len()
    }
}

impl Sources for Fake {
    fn pipeline_status(&self, args: &[String]) -> PipelineCall {
        lock(&self.pipeline_args).push(args.to_vec());
        self.enter();
        let answer = lock(&self.pipeline_answer)
            .clone()
            .unwrap_or_else(|| PipelineCall::Facts(serde_json::from_str(FACTS_IDLE).unwrap()));
        self.leave();
        answer
    }

    fn account(&self, base: &str, token: Option<&Token>) -> AccountAnswer {
        lock(&self.account_calls).push((base.to_owned(), token.is_some()));
        self.enter();
        let answer = lock(&self.account_answer)
            .clone()
            .unwrap_or(AccountAnswer::Refused);
        self.leave();
        answer
    }

    fn events_end(&self, base: &str, _token: &Token) -> Result<u64, String> {
        lock(&self.events_calls).push(base.to_owned());
        lock(&self.events_answer)
            .clone()
            .unwrap_or_else(|| Err("no answer".to_owned()))
    }

    fn first_sync(
        &self,
        config: &AppConfig,
        _base: &str,
        _token: &Token,
        mark: u64,
    ) -> Result<SyncCheck, String> {
        lock(&self.check_calls).push((config.mode, mark));
        lock(&self.check_answer)
            .clone()
            .unwrap_or_else(|| Err("no answer".to_owned()))
    }

    fn device_changes(
        &self,
        _base: &str,
        _token: &Token,
        cursor: u64,
    ) -> Result<DeviceChanges, String> {
        lock(&self.changes_calls).push(cursor);
        lock(&self.changes_answer)
            .clone()
            .unwrap_or_else(|| Err("no answer".to_owned()))
    }
}

struct Rig {
    fake: Arc<Fake>,
    /// The folder of notify.json; it lives as long as the rig.
    notify_dir: tempfile::TempDir,
    tokens: Arc<MemoryStore>,
    inputs: Arc<Mutex<Inputs>>,
    now: Arc<AtomicU64>,
    poller: Poller,
}

impl Rig {
    fn new(config: AppConfig) -> Self {
        let notify_dir = tempfile::tempdir().unwrap();
        let notify = Arc::new(NotifyStore::load(notify_dir.path()));
        let fake = Arc::new(Fake::default());
        let tokens = Arc::new(MemoryStore::new());
        let inputs = Arc::new(Mutex::new(Inputs {
            config,
            config_reset: false,
            processes: ProcessStates {
                server: ChildState::Running {
                    pid: 7,
                    since_ms: T0 - 3_600 * SEC,
                },
                pipeline: ChildState::Running {
                    pid: 8,
                    since_ms: T0 - 3_600 * SEC,
                },
            },
            serve_info: ServeInfo::default(),
            notify,
        }));
        let now = Arc::new(AtomicU64::new(T0));
        let read = inputs.clone();
        let clock_now = now.clone();
        let poller = Poller::new(
            fake.clone(),
            tokens.clone(),
            Arc::new(move || lock(&read).clone()),
            Arc::new(move || clock_now.load(Ordering::SeqCst)),
        );
        Self {
            fake,
            notify_dir,
            tokens,
            inputs,
            now,
            poller,
        }
    }

    fn local() -> Self {
        Self::new(local_config(None))
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    /// Ticks and waits until the polls it started have ended.
    fn tick(&self) {
        self.poller.tick();
        self.settle();
    }

    fn settle(&self) {
        wait_until("the polls to end", || self.poller.idle());
    }

    fn calls(&self) -> (usize, usize) {
        (self.fake.pipeline_calls(), self.fake.account_count())
    }
}

fn local_config(account: Option<&str>) -> AppConfig {
    AppConfig {
        mode: Mode::Local,
        data_dir: PathBuf::from("/nowhere/notedown-data"),
        account: account.map(str::to_owned),
        ..AppConfig::default()
    }
}

#[test]
fn the_cadence_follows_visibility_and_queue() {
    assert_eq!(pipeline_every(true, true), Duration::from_secs(5));
    assert_eq!(pipeline_every(true, false), Duration::from_secs(15));
    assert_eq!(pipeline_every(false, true), Duration::from_secs(60));
    assert_eq!(pipeline_every(false, false), Duration::from_secs(60));
    assert_eq!(account_every(true), Duration::from_secs(10));
    assert_eq!(account_every(false), Duration::from_secs(60));
}

#[test]
fn polls_follow_visibility_and_queue() {
    let rig = Rig::local();
    // Hidden: the first tick polls both, then every 60 s.
    rig.tick();
    assert_eq!(rig.calls(), (1, 1));
    rig.advance(30 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (1, 1), "hidden polls wait 60 s");
    rig.advance(30 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (2, 2));

    // Visible with an empty queue: 15 s and 10 s.
    rig.poller.set_visible(true);
    rig.settle();
    assert_eq!(rig.calls(), (2, 2), "nothing is due yet");
    rig.advance(10 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (2, 3));
    rig.advance(5 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (3, 3));

    // A queue that is not empty: every 5 s.
    *lock(&rig.fake.pipeline_answer) = Some(PipelineCall::Facts(
        serde_json::from_str(FACTS_BUSY).unwrap(),
    ));
    rig.advance(15 * SEC);
    rig.tick();
    assert_eq!(rig.fake.pipeline_calls(), 4);
    rig.advance(5 * SEC);
    rig.tick();
    assert_eq!(rig.fake.pipeline_calls(), 5);

    // Hidden again: 60 s, also with a queue.
    rig.poller.set_visible(false);
    rig.settle();
    rig.advance(30 * SEC);
    rig.tick();
    assert_eq!(rig.fake.pipeline_calls(), 5);
    rig.advance(30 * SEC);
    rig.tick();
    assert_eq!(rig.fake.pipeline_calls(), 6);
}

#[test]
fn becoming_visible_polls_what_is_overdue() {
    let rig = Rig::local();
    rig.tick();
    rig.advance(20 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (1, 1));
    rig.poller.set_visible(true);
    rig.settle();
    assert_eq!(rig.calls(), (2, 2), "15 s and 10 s have passed");
}

#[test]
fn poll_now_polls_at_once() {
    let rig = Rig::local();
    rig.tick();
    rig.advance(SEC);
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(rig.calls(), (2, 2));
}

#[test]
fn a_slow_poll_is_not_stacked() {
    let rig = Rig::local();
    rig.fake.hold(true);
    rig.poller.set_visible(true);
    wait_until("both polls to wait", || {
        rig.fake.running.load(Ordering::SeqCst) == 2
    });
    for _ in 0..20 {
        rig.advance(60 * SEC);
        rig.poller.tick();
        rig.poller.poll_now();
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rig.calls(), (1, 1), "no second call while the first runs");
    assert!(!rig.poller.idle());
    rig.fake.hold(false);
    rig.settle();
    assert_eq!(
        rig.fake.most_at_once.load(Ordering::SeqCst),
        2,
        "one of each"
    );
    assert_eq!(rig.calls(), (1, 1), "the end of a call starts none");
    rig.advance(15 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (2, 2));
}

#[test]
fn account_is_passed_only_when_known() {
    let mut config = local_config(None);
    assert_eq!(
        pipeline_status_args(&config),
        ["pipeline", "status", "--data", "/nowhere/notedown-data"]
    );
    config.account = Some("jordi".into());
    assert_eq!(
        pipeline_status_args(&config),
        [
            "pipeline",
            "status",
            "--data",
            "/nowhere/notedown-data",
            "--account",
            "jordi"
        ]
    );
    config.account = Some(String::new());
    assert!(!pipeline_status_args(&config).contains(&"--account".to_owned()));

    // Through the poller, with the token only when the account is known and has one.
    let rig = Rig::local();
    rig.tokens
        .set("local|jordi", &Token::new("secret"))
        .unwrap();
    rig.tick();
    assert!(!lock(&rig.fake.pipeline_args)[0].contains(&"--account".to_owned()));
    assert!(!lock(&rig.fake.account_calls)[0].1, "no account, no token");

    lock(&rig.inputs).config = local_config(Some("jordi"));
    rig.tick();
    assert_eq!(rig.calls(), (2, 2), "a new config polls at once");
    let args = lock(&rig.fake.pipeline_args)[1].clone();
    assert_eq!(args[args.len() - 2..], ["--account", "jordi"]);
    assert_eq!(
        lock(&rig.fake.account_calls)[1],
        ("http://127.0.0.1:19072/rest/v1".to_owned(), true)
    );
}

#[test]
fn nothing_is_polled_without_a_mode() {
    let rig = Rig::new(AppConfig::default());
    rig.poller.set_visible(true);
    rig.tick();
    rig.advance(120 * SEC);
    rig.tick();
    assert_eq!(rig.calls(), (0, 0));
    let s = rig.poller.status();
    assert_eq!(s.setup.line.as_deref(), Some("Notedown is not set up yet"));
}

#[test]
fn a_new_config_drops_the_old_answers() {
    let rig = Rig::local();
    *lock(&rig.fake.pipeline_answer) = Some(PipelineCall::Failed("the store is damaged".into()));
    rig.tick();
    assert_eq!(
        rig.poller.status().ocr.line,
        "OCR status is not available: the store is damaged"
    );
    // A result that ends after the config changed belongs to the old one.
    rig.fake.hold(true);
    lock(&rig.inputs).config = local_config(Some("jordi"));
    rig.poller.tick();
    wait_until("the new polls to start", || rig.calls() == (2, 2));
    lock(&rig.inputs).config = local_config(Some("other"));
    rig.fake.hold(false);
    rig.settle();
    assert_eq!(rig.poller.status().ocr.line, "Checking...");
    rig.tick();
    assert_eq!(rig.calls(), (3, 3));
}

#[test]
fn the_server_counts_as_unreachable_over_polls() {
    let rig = Rig::local();
    *lock(&rig.fake.account_answer) = Some(AccountAnswer::Unreachable("refused".into()));
    rig.poller.set_visible(true);
    rig.settle();
    rig.advance(10 * SEC);
    rig.tick();
    assert!(rig.poller.status().attention.is_empty());
    for _ in 0..12 {
        rig.advance(10 * SEC);
        rig.tick();
    }
    assert_eq!(
        rig.poller.status().attention,
        vec![Attention::ServerUnreachable]
    );
    *lock(&rig.fake.account_answer) = Some(AccountAnswer::Refused);
    rig.advance(10 * SEC);
    rig.tick();
    assert!(rig.poller.status().attention.is_empty());
    assert_eq!(
        rig.poller.status().device.line.as_deref(),
        Some("Not logged in yet")
    );
}

#[test]
fn listeners_get_each_new_status_once() {
    let rig = Rig::local();
    let seen = Arc::new(Mutex::new(Vec::<Status>::new()));
    let into = seen.clone();
    rig.poller.on_status(move |s| lock(&into).push(s.clone()));
    rig.tick();
    let count = lock(&seen).len();
    assert!(count >= 1);
    assert_eq!(
        lock(&seen).last().unwrap().ocr.line,
        "Caught up",
        "the last status has both answers"
    );
    rig.poller.refresh();
    rig.advance(SEC);
    rig.poller.tick();
    rig.settle();
    assert_eq!(lock(&seen).len(), count, "nothing changed, nothing is sent");

    // A child that changes its state is a new status at once.
    lock(&rig.inputs).processes.pipeline = ChildState::Backoff {
        until_ms: T0 + 60 * SEC,
        last_error: "boom".into(),
    };
    rig.poller.refresh();
    assert_eq!(
        lock(&seen).last().unwrap().attention,
        vec![Attention::PipelineBackoff]
    );
}

#[test]
fn a_stopped_poller_starts_no_poll() {
    let rig = Rig::local();
    rig.poller.stop();
    rig.tick();
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(rig.calls(), (0, 0));
}

#[test]
fn the_rest_base_of_each_mode() {
    let info = ServeInfo::default();
    assert_eq!(rest_base(&AppConfig::default(), &info), None);
    let dir = tempfile::tempdir().unwrap();
    let mut config = local_config(None);
    config.data_dir = dir.path().to_owned();
    assert_eq!(
        rest_base(&config, &info).as_deref(),
        Some("http://127.0.0.1:19072/rest/v1")
    );
    let info = ServeInfo {
        listen: vec!["0.0.0.0:19100".parse().unwrap()],
        ..ServeInfo::default()
    };
    assert_eq!(
        rest_base(&config, &info).as_deref(),
        Some("http://127.0.0.1:19100/rest/v1")
    );

    config.mode = Mode::Remote;
    assert_eq!(rest_base(&config, &info), None, "no address yet");
    for (url, base) in [
        ("https://n.example.org", "https://n.example.org/rest/v1"),
        ("https://n.example.org/", "https://n.example.org/rest/v1"),
        (
            "http://10.0.0.2:19072/rest/v1",
            "http://10.0.0.2:19072/rest/v1",
        ),
        (
            "http://10.0.0.2:19072/rest/v1/",
            "http://10.0.0.2:19072/rest/v1",
        ),
    ] {
        config.server_url = Some(url.into());
        assert_eq!(rest_base(&config, &info).as_deref(), Some(base), "{url}");
    }
}

/// A fake `notedown` that runs `script` with the arguments.
fn sidecar(dir: &Path, script: &str) -> SpawnFn {
    let path = dir.join("notedown");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::process::spawn_program(path)
}

#[test]
fn the_status_call_reads_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let args = pipeline_status_args(&local_config(Some("jordi")));
    let run = |script: &str| run_status(&sidecar(dir.path(), script), &args, WAIT);

    let ok = run(&format!(
        "[ \"$1 $2 $5 $6\" = 'pipeline status --account jordi' ] || exit 9\ncat <<'EOF'\n\
         {FACTS_BUSY}\nEOF"
    ));
    let PipelineCall::Facts(facts) = ok else {
        panic!("{ok:?}")
    };
    assert_eq!((facts.queued, facts.pending), (2, 1));

    assert_eq!(
        run(
            "echo 'error: this data folder has no account; create one with notedown cloud \
             account add' >&2\nexit 1"
        ),
        PipelineCall::Failed(
            "this data folder has no account; create one with notedown cloud account add".into()
        )
    );
    assert_eq!(
        run("exit 3"),
        PipelineCall::Failed("it ended with exit code 3".into())
    );
    let PipelineCall::Failed(text) = run("echo 'not json'") else {
        panic!("not json is no answer")
    };
    assert!(text.starts_with("the answer could not be read"), "{text}");

    let gone = run_status(
        &crate::process::spawn_program(dir.path().join("missing")),
        &args,
        WAIT,
    );
    let PipelineCall::Failed(text) = gone else {
        panic!("{gone:?}")
    };
    assert!(
        text.starts_with("the notedown program could not start"),
        "{text}"
    );
}

#[test]
fn a_hanging_status_call_is_ended() {
    let dir = tempfile::tempdir().unwrap();
    // The sleep runs from this test's folder, so only its own processes are looked for.
    let sleep = crate::test_procs::tagged_sleep(dir.path());
    let spawn = sidecar(dir.path(), &format!("exec \"{}\" 314", sleep.display()));
    let start = Instant::now();
    let answer = run_status(&spawn, &[], Duration::from_millis(300));
    assert_eq!(
        answer,
        PipelineCall::Failed("it did not answer in time".into())
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    let ended = crate::test_procs::gone(dir.path());
    crate::test_procs::kill_in(dir.path());
    assert!(ended, "the call was ended");
}

#[test]
fn the_account_call_reads_the_answer() {
    let sources = RealSources::new(crate::process::spawn_program("/bin/false".into()));
    let base = |s: &TestServer| format!("http://127.0.0.1:{}/rest/v1", s.port);
    let token = Token::new("secret");

    let ok = TestServer::start(Some(http(
        "200 OK",
        r#"{"slug": "jordi", "userId": 7, "devices": [{"serial": "SN1", "lastSyncMs": 5}]}"#,
    )));
    let AccountAnswer::Account(info) = sources.account(&base(&ok), Some(&token)) else {
        panic!("a 200 is the account")
    };
    assert_eq!(info.slug, "jordi");
    assert_eq!(info.devices[0].last_sync_ms, Some(5));

    let refused = TestServer::start(Some(http(
        "401 Unauthorized",
        r#"{"error": "token_refused"}"#,
    )));
    assert_eq!(
        sources.account(&base(&refused), None),
        AccountAnswer::Refused
    );
    let removed = TestServer::start(Some(http("410 Gone", r#"{"error": "account_removed"}"#)));
    assert_eq!(
        sources.account(&base(&removed), Some(&token)),
        AccountAnswer::Failed("the account was removed".into())
    );
    // A proxy whose server is down: the server does not answer.
    for (status, code) in [
        ("502 Bad Gateway", 502),
        ("503 Service Unavailable", 503),
        ("504 Gateway Timeout", 504),
    ] {
        let proxy = TestServer::start(Some(http(status, "<html>down</html>")));
        let b = base(&proxy);
        assert_eq!(
            sources.account(&b, Some(&token)),
            AccountAnswer::Unreachable(format!("the server behind {b} does not answer ({code})"))
        );
    }
    let empty = TestServer::start(Some(http("503 Service Unavailable", "{}")));
    assert!(matches!(
        sources.account(&base(&empty), Some(&token)),
        AccountAnswer::Unreachable(_)
    ));
    // A Notedown server that answers 503 itself is there.
    let damaged = TestServer::start(Some(http(
        "503 Service Unavailable",
        r#"{"error": "store_damaged"}"#,
    )));
    assert_eq!(
        sources.account(&base(&damaged), Some(&token)),
        AccountAnswer::Failed("the server answered 503".into())
    );
    let teapot = TestServer::start(Some(http("500 Internal Server Error", "{}")));
    assert_eq!(
        sources.account(&base(&teapot), Some(&token)),
        AccountAnswer::Failed("the server answered 500".into())
    );
    let garbage = TestServer::start(Some(http("200 OK", "[1,2")));
    let AccountAnswer::Failed(text) = sources.account(&base(&garbage), Some(&token)) else {
        panic!("broken JSON is no account")
    };
    assert!(text.starts_with("the answer could not be read"), "{text}");

    let nobody = format!("http://127.0.0.1:{}/rest/v1", free_port());
    let answer = sources.account(&nobody, Some(&token));
    let AccountAnswer::Unreachable(why) = &answer else {
        panic!("{answer:?}")
    };
    assert!(!why.contains("secret"));
    assert!(!format!("{answer:?}").contains("secret"));
}

#[test]
fn the_token_goes_only_into_the_header() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (heads_tx, heads_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..2 {
            // Gives up after 10 s, so a request that never comes cannot hold the test.
            let Some(mut stream) = accept_within(&listener, Duration::from_secs(10)) else {
                return;
            };
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let body = r#"{"error":"token_refused"}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = heads_tx.send(String::from_utf8_lossy(&head).to_lowercase());
        }
    });
    let sources = RealSources::new(crate::process::spawn_program("/bin/false".into()));
    let base = format!("http://127.0.0.1:{port}/rest/v1");
    sources.account(&base, Some(&Token::new("Secret-1")));
    sources.account(&base, None);
    let heads: Vec<String> = (0..2)
        .map(|_| {
            heads_rx
                .recv_timeout(Duration::from_secs(15))
                .expect("no request came")
        })
        .collect();
    assert!(
        heads[0].starts_with("get /rest/v1/account http/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[0].contains("authorization: bearer secret-1"),
        "{}",
        heads[0]
    );
    assert!(!heads[1].contains("authorization"), "{}", heads[1]);
}

#[test]
fn a_failed_request_shows_no_credentials_of_the_address() {
    // Not an HTTP answer at all: the request fails.
    let garbage = TestServer::start(Some("this is no HTTP\r\n\r\n".to_owned()));
    let sources = RealSources::new(crate::process::spawn_program("/bin/false".into()));
    let base = format!("http://user:hunter2@127.0.0.1:{}/rest/v1", garbage.port);
    let answer = sources.account(&base, None);
    let AccountAnswer::Failed(text) = &answer else {
        panic!("{answer:?}")
    };
    assert!(text.starts_with("the request failed"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("127.0.0.1"), "no address at all: {text}");
}

#[test]
fn a_broken_answer_shows_no_credentials_of_the_address() {
    // It promises more of the body than it sends, then closes.
    let short = TestServer::start(Some(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 500\r\n\
         connection: close\r\n\r\n{\"slug\""
            .to_owned(),
    ));
    let sources = RealSources::new(crate::process::spawn_program("/bin/false".into()));
    let base = format!("http://user:hunter2@127.0.0.1:{}/rest/v1", short.port);
    let answer = sources.account(&base, None);
    let AccountAnswer::Failed(text) = &answer else {
        panic!("{answer:?}")
    };
    assert!(text.starts_with("the answer could not be read"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("127.0.0.1"), "no address at all: {text}");
}

#[test]
fn a_redirect_is_not_followed() {
    let moved = TestServer::start(Some(
        "HTTP/1.1 301 Moved Permanently\r\nlocation: http://192.0.2.1/rest/v1/account\r\n\
         content-length: 0\r\nconnection: close\r\n\r\n"
            .to_owned(),
    ));
    let sources = RealSources::new(crate::process::spawn_program("/bin/false".into()));
    let base = format!("http://127.0.0.1:{}/rest/v1", moved.port);
    assert_eq!(
        sources.account(&base, Some(&Token::new("secret"))),
        AccountAnswer::Failed("the server answered 301".into())
    );
}

#[test]
fn loopback_bases_are_known() {
    for base in [
        "http://127.0.0.1:19072/rest/v1",
        "http://[::1]:19072/rest/v1",
        "http://localhost:19072/rest/v1",
        "http://127.0.0.5/rest/v1",
    ] {
        assert!(is_loopback(base), "{base}");
    }
    for base in [
        "https://notes.example.org/rest/v1",
        "http://192.168.1.5:19072/rest/v1",
        "http://localhost.example.org/rest/v1",
        "not a url",
    ] {
        assert!(!is_loopback(base), "{base}");
    }
}

#[test]
fn the_existing_setup_is_looked_at_per_poll() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = local_config(None);
    config.data_dir = dir.path().to_owned();
    let rig = Rig::new(config);
    rig.tick();
    assert!(!rig.poller.status().existing.server);
    std::fs::write(dir.path().join("server.toml"), "").unwrap();
    rig.advance(SEC);
    rig.tick();
    assert!(
        !rig.poller.status().existing.server,
        "no new look between two polls"
    );
    rig.advance(60 * SEC);
    rig.tick();
    assert!(
        rig.poller.status().existing.server,
        "the next poll looks again"
    );

    // Without a mode nothing is polled, but the setup still needs to know what is there.
    let empty = tempfile::tempdir().unwrap();
    std::fs::write(empty.path().join("pipeline.toml"), "").unwrap();
    let rig = Rig::new(AppConfig {
        data_dir: empty.path().to_owned(),
        ..AppConfig::default()
    });
    rig.tick();
    assert!(rig.poller.status().existing.pipeline);
}

#[test]
fn an_address_in_a_line_has_no_credentials() {
    assert_eq!(
        shown("https://user:hunter2@notes.example.org/rest/v1"),
        "https://notes.example.org/rest/v1"
    );
    assert_eq!(
        shown("http://127.0.0.1:19072/rest/v1"),
        "http://127.0.0.1:19072/rest/v1"
    );
    assert_eq!(shown("not a url"), "the server");
}

#[test]
fn the_poller_takes_the_mark_of_an_import_on_another_machine() {
    use crate::config::SyncMark;
    let url = "https://n.example.org";
    let key = format!("{url}|jordi");
    let rig = Rig::new(AppConfig {
        mode: Mode::Remote,
        server_url: Some(url.into()),
        account: Some("jordi".into()),
        ..AppConfig::default()
    });
    rig.tokens.set(&key, &Token::new("t")).unwrap();
    // The sync count has its baselines: only the mark asks for the journal end here.
    let notify = lock(&rig.inputs).notify.clone();
    for login in [key.as_str(), "local|jordi"] {
        notify
            .update(|r| r.keys.entry(login.to_owned()).or_default().cursor = Some(1))
            .unwrap();
    }
    let marks: Arc<Mutex<Vec<SyncMark>>> = Arc::default();
    let seen = marks.clone();
    rig.poller.on_sync_mark(move |mark| lock(&seen).push(mark));
    let account = |imported: Option<bool>| {
        AccountAnswer::Account(AccountInfo {
            slug: "jordi".into(),
            imported,
            ..AccountInfo::default()
        })
    };
    *lock(&rig.fake.events_answer) = Some(Ok(813));
    // Not imported yet, or an older server that does not say: no mark.
    for imported in [Some(false), None] {
        *lock(&rig.fake.account_answer) = Some(account(imported));
        rig.poller.poll_now();
        rig.settle();
    }
    assert!(lock(&rig.fake.events_calls).is_empty());
    // A failed call is tried again, but at most once a minute.
    *lock(&rig.fake.account_answer) = Some(account(Some(true)));
    *lock(&rig.fake.events_answer) = Some(Err("no answer".into()));
    rig.poller.poll_now();
    rig.settle();
    assert!(lock(&marks).is_empty());
    *lock(&rig.fake.events_answer) = Some(Ok(813));
    rig.advance(30 * SEC);
    rig.poller.poll_now();
    rig.settle();
    assert!(lock(&marks).is_empty());
    assert_eq!(lock(&rig.fake.events_calls).len(), 1);
    rig.advance(30 * SEC);
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(
        *lock(&marks),
        vec![SyncMark {
            key: key.clone(),
            seq: 813,
            at_ms: T0 + 60 * SEC,
            sync_at_mark: None,
        }]
    );
    assert_eq!(
        *lock(&rig.fake.events_calls),
        vec![format!("{url}/rest/v1"); 2]
    );
    // With the mark in the config, no second one.
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        key,
        seq: 813,
        at_ms: T0,
        sync_at_mark: None,
    });
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(lock(&marks).len(), 1);
    // This Mac's server takes its mark when the app imports, not from the poll.
    lock(&rig.inputs).config = local_config(Some("jordi"));
    rig.tokens.set("local|jordi", &Token::new("t")).unwrap();
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(lock(&marks).len(), 1);
    assert_eq!(lock(&rig.fake.events_calls).len(), 2);
}

#[test]
fn the_remote_mark_is_saved_before_the_status_shows_the_import() {
    use crate::config::SyncMark;
    let url = "https://n.example.org";
    let rig = Rig::new(AppConfig {
        mode: Mode::Remote,
        server_url: Some(url.into()),
        account: Some("jordi".into()),
        ..AppConfig::default()
    });
    rig.tokens
        .set(&format!("{url}|jordi"), &Token::new("t"))
        .unwrap();
    let marks: Arc<Mutex<Vec<SyncMark>>> = Arc::default();
    let seen = marks.clone();
    rig.poller.on_sync_mark(move |mark| lock(&seen).push(mark));
    // Each status that shows the files imported comes after the mark.
    let early: Arc<Mutex<usize>> = Arc::default();
    let (watch, count) = (marks.clone(), early.clone());
    rig.poller.on_status(move |status| {
        let imported = status.account.as_ref().and_then(|a| a.imported) == Some(true);
        if imported && lock(&watch).is_empty() {
            *lock(&count) += 1;
        }
    });
    *lock(&rig.fake.events_answer) = Some(Ok(900));
    *lock(&rig.fake.account_answer) = Some(AccountAnswer::Account(AccountInfo {
        slug: "jordi".into(),
        imported: Some(true),
        ..AccountInfo::default()
    }));
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(lock(&marks).len(), 1);
    assert_eq!(*lock(&early), 0);
}

/// An `/account` answer of the account `me` whose Supernote last synced at `sync`.
fn synced(sync: Option<u64>) -> AccountAnswer {
    AccountAnswer::Account(AccountInfo {
        slug: "me".into(),
        imported: Some(true),
        app_data_imported: Some(true),
        devices: vec![crate::status::Device {
            serial: "SN1".into(),
            last_sync_ms: sync,
            ..crate::status::Device::default()
        }],
        ..AccountInfo::default()
    })
}

#[test]
fn the_first_sync_is_checked_once() {
    use crate::config::SyncMark;
    let rig = Rig::new(local_config(Some("me")));
    rig.tokens.set("local|me", &Token::new("t")).unwrap();
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        key: "local|me".into(),
        seq: 10,
        at_ms: T0,
        sync_at_mark: Some(T0),
    });
    // The app saves the check into the config, as lib.rs does.
    let saved: Arc<Mutex<Vec<(String, SyncCheck)>>> = Arc::default();
    let (keep, inputs) = (saved.clone(), rig.inputs.clone());
    rig.poller.on_sync_checked(move |key, check| {
        lock(&keep).push((key.clone(), check.clone()));
        lock(&inputs).config.setup.sync_checked = Some(check);
    });
    // Each status with the first sync comes after its check is saved.
    let early: Arc<Mutex<usize>> = Arc::default();
    let (watch, count) = (saved.clone(), early.clone());
    rig.poller.on_status(move |status| {
        if status.device.last_sync_ms.is_some_and(|s| s > T0) && lock(&watch).is_empty() {
            *lock(&count) += 1;
        }
    });
    let deleted = SyncCheck {
        files: 2,
        digests: Some(0),
        ..SyncCheck::default()
    };
    *lock(&rig.fake.check_answer) = Some(Ok(deleted.clone()));
    // No sync yet, or one from before the import: no check.
    for sync in [None, Some(T0 - SEC)] {
        *lock(&rig.fake.account_answer) = Some(synced(sync));
        rig.poller.poll_now();
        rig.settle();
    }
    assert!(lock(&rig.fake.check_calls).is_empty());
    // A failed check is tried again, at most once a minute.
    *lock(&rig.fake.check_answer) = Some(Err("no answer".into()));
    *lock(&rig.fake.account_answer) = Some(synced(Some(T0 + 5 * SEC)));
    rig.poller.poll_now();
    rig.settle();
    rig.advance(30 * SEC);
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(lock(&rig.fake.check_calls).len(), 1);
    assert!(lock(&saved).is_empty());
    // The status says why, for step 6 of the Setup page.
    assert_eq!(
        rig.poller.status().first_sync_error.as_deref(),
        Some("no answer")
    );
    // A new mark of the same login (Import again) does not carry the old reason.
    let first = lock(&rig.inputs).config.setup.sync_mark.clone();
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        seq: 20,
        ..first.clone().unwrap()
    });
    assert_eq!(rig.poller.status().first_sync_error, None);
    lock(&rig.inputs).config.setup.sync_mark = first;
    // A status went out with the sync but without a check: the check failed. The next sync
    // is checked before its status goes out.
    *lock(&early) = 0;
    *lock(&rig.fake.check_answer) = Some(Ok(deleted.clone()));
    *lock(&rig.fake.account_answer) = Some(synced(Some(T0 + 6 * SEC)));
    rig.advance(30 * SEC);
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(
        *lock(&rig.fake.check_calls),
        vec![(Mode::Local, 10), (Mode::Local, 10)]
    );
    assert_eq!(*lock(&saved), vec![("local|me".to_owned(), deleted)]);
    assert_eq!(*lock(&early), 0);
    // Checked: the error is gone.
    assert_eq!(rig.poller.status().first_sync_error, None);
    // Checked: later syncs are not checked again.
    *lock(&rig.fake.account_answer) = Some(synced(Some(T0 + 600 * SEC)));
    rig.advance(120 * SEC);
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(lock(&rig.fake.check_calls).len(), 2);
    assert_eq!(lock(&saved).len(), 1);
}

#[test]
fn a_mark_of_another_login_is_not_checked() {
    use crate::config::SyncMark;
    let rig = Rig::new(local_config(Some("me")));
    rig.tokens.set("local|me", &Token::new("t")).unwrap();
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        key: "local|anna".into(),
        seq: 10,
        at_ms: T0,
        sync_at_mark: None,
    });
    rig.poller.on_sync_checked(|_, _| {});
    *lock(&rig.fake.check_answer) = Some(Ok(SyncCheck::default()));
    *lock(&rig.fake.account_answer) = Some(synced(Some(T0 + 5 * SEC)));
    rig.poller.poll_now();
    rig.settle();
    assert!(lock(&rig.fake.check_calls).is_empty());
}

#[test]
fn the_first_sync_is_found_by_server_times_only() {
    use crate::config::SyncMark;
    let url = "https://n.example.org";
    let key = format!("{url}|me");
    let rig = Rig::new(AppConfig {
        mode: Mode::Remote,
        server_url: Some(url.into()),
        account: Some("me".into()),
        ..AppConfig::default()
    });
    rig.tokens.set(&key, &Token::new("t")).unwrap();
    // The mark keeps the newest device sync the server reported when it was taken.
    let marks: Arc<Mutex<Vec<SyncMark>>> = Arc::default();
    let (seen, inputs) = (marks.clone(), rig.inputs.clone());
    rig.poller.on_sync_mark(move |mark| {
        lock(&seen).push(mark.clone());
        lock(&inputs).config.setup.sync_mark = Some(mark);
    });
    let checks: Arc<Mutex<Vec<SyncCheck>>> = Arc::default();
    let keep = checks.clone();
    rig.poller
        .on_sync_checked(move |_, check| lock(&keep).push(check));
    *lock(&rig.fake.events_answer) = Some(Ok(50));
    *lock(&rig.fake.check_answer) = Some(Ok(SyncCheck::default()));
    // The server's clock is far behind the Mac's: its times are all before `at_ms`.
    let server = T0 - 3_600 * SEC;
    *lock(&rig.fake.account_answer) = Some(synced(Some(server)));
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(lock(&marks).len(), 1);
    assert_eq!(lock(&marks)[0].sync_at_mark, Some(server));
    // The same sync again is no first sync.
    rig.advance(120 * SEC);
    rig.poller.poll_now();
    rig.settle();
    assert!(lock(&rig.fake.check_calls).is_empty());
    // A later sync by the server's clock is, though it is before the Mac's `at_ms`.
    *lock(&rig.fake.account_answer) = Some(synced(Some(server + SEC)));
    rig.poller.poll_now();
    rig.settle();
    assert_eq!(*lock(&rig.fake.check_calls), vec![(Mode::Remote, 50)]);
    assert_eq!(lock(&checks).len(), 1);
}

// The count of the files of a device sync (design note 3 of plan E).

const ME: &str = "local|me";
/// A device sync by the server's clock, far from the Mac's.
const S: u64 = T0 - 7_200 * SEC;

impl Rig {
    /// A rig of the account `me` on this Mac, logged in.
    fn me() -> Self {
        let rig = Self::new(local_config(Some("me")));
        rig.tokens.set(ME, &Token::new("t")).unwrap();
        rig
    }

    fn poll(&self) {
        self.poller.poll_now();
        self.settle();
    }

    fn answer(&self, answer: AccountAnswer) {
        *lock(&self.fake.account_answer) = Some(answer);
    }

    fn record(&self) -> KeyRecord {
        let notify = lock(&self.inputs).notify.clone();
        notify.read().keys.get(ME).cloned().unwrap_or_default()
    }

    fn set_record(&self, f: impl FnOnce(&mut KeyRecord)) {
        let notify = lock(&self.inputs).notify.clone();
        notify
            .update(|r| f(r.keys.entry(ME.to_owned()).or_default()))
            .unwrap();
    }

    /// Ends the poller and starts a new one on the same notify.json, as an app restart does.
    fn restart(&mut self) {
        self.poller.stop();
        lock(&self.inputs).notify = Arc::new(NotifyStore::load(self.notify_dir.path()));
        let read = self.inputs.clone();
        let clock_now = self.now.clone();
        self.poller = Poller::new(
            self.fake.clone(),
            self.tokens.clone(),
            Arc::new(move || lock(&read).clone()),
            Arc::new(move || clock_now.load(Ordering::SeqCst)),
        );
    }

    fn changes_calls(&self) -> Vec<u64> {
        lock(&self.fake.changes_calls).clone()
    }

    fn changes(&self, answer: Result<DeviceChanges, String>) {
        *lock(&self.fake.changes_answer) = Some(answer);
    }
}

fn changed(files: Option<u64>, end: u64) -> Result<DeviceChanges, String> {
    Ok(DeviceChanges { files, end })
}

/// The poller's fields of a record.
fn poller_fields(
    last_sync_ms: Option<u64>,
    cursor: Option<u64>,
    last_count: Option<SyncCount>,
) -> KeyRecord {
    KeyRecord {
        last_sync_ms,
        cursor,
        last_count,
        ..KeyRecord::default()
    }
}

/// An `/account` answer with two Supernotes: their last syncs and whether they sync now.
fn two_devices(a: (Option<u64>, bool), b: (Option<u64>, bool)) -> AccountAnswer {
    let device = |serial: &str, (sync, syncing): (Option<u64>, bool)| crate::status::Device {
        serial: serial.into(),
        last_sync_ms: sync,
        syncing,
        ..crate::status::Device::default()
    };
    AccountAnswer::Account(AccountInfo {
        slug: "me".into(),
        imported: Some(true),
        app_data_imported: Some(true),
        devices: vec![device("SN1", a), device("SN2", b)],
        ..AccountInfo::default()
    })
}

#[test]
fn the_first_sight_is_a_baseline_with_the_cursor() {
    let rig = Rig::me();
    rig.answer(synced(Some(S)));
    // Without the journal end nothing is stored; it is asked again at most once a minute.
    *lock(&rig.fake.events_answer) = Some(Err("no answer".into()));
    rig.poll();
    assert_eq!(rig.record(), KeyRecord::default());
    *lock(&rig.fake.events_answer) = Some(Ok(40));
    rig.advance(30 * SEC);
    rig.poll();
    assert_eq!(rig.record(), KeyRecord::default());
    rig.advance(30 * SEC);
    rig.poll();
    assert_eq!(rig.record(), poller_fields(Some(S), Some(40), None));
    assert_eq!(lock(&rig.fake.events_calls).len(), 2);
    // The baseline counts nothing.
    assert!(rig.changes_calls().is_empty());
    assert_eq!(rig.poller.status().device.files_changed, None);
    // The same sync again: nothing.
    rig.advance(120 * SEC);
    rig.poll();
    assert!(rig.changes_calls().is_empty());
    assert_eq!(lock(&rig.fake.events_calls).len(), 2);
}

#[test]
fn a_new_sync_is_counted_before_its_status_goes_out() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    // Each status with the new sync carries its count.
    let without: Arc<Mutex<usize>> = Arc::default();
    let count = without.clone();
    rig.poller.on_status(move |status| {
        if status.device.last_sync_ms == Some(S + SEC) && status.device.files_changed.is_none() {
            *lock(&count) += 1;
        }
    });
    rig.changes(changed(Some(3), 45));
    rig.answer(synced(Some(S + SEC)));
    rig.poll();
    assert_eq!(rig.changes_calls(), vec![40]);
    let count = SyncCount {
        at_ms: S + SEC,
        files: Some(3),
        counted_at_ms: None,
    };
    assert_eq!(
        rig.record(),
        poller_fields(Some(S + SEC), Some(45), Some(count))
    );
    assert_eq!(rig.poller.status().device.files_changed, Some(count));
    assert_eq!(*lock(&without), 0);
    // The next one counts after the new cursor.
    rig.changes(changed(Some(0), 47));
    rig.answer(synced(Some(S + 2 * SEC)));
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls(), vec![40, 45]);
    assert_eq!(
        rig.record().last_count,
        Some(SyncCount {
            at_ms: S + 2 * SEC,
            files: Some(0),
            counted_at_ms: Some(T0 + 60 * SEC)
        })
    );
}

#[test]
fn a_baseline_before_any_sync_counts_the_first_one() {
    let rig = Rig::me();
    *lock(&rig.fake.events_answer) = Some(Ok(40));
    rig.answer(synced(None));
    rig.poll();
    assert_eq!(rig.record(), poller_fields(None, Some(40), None));
    rig.changes(changed(Some(2), 44));
    rig.answer(synced(Some(S)));
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls(), vec![40]);
    assert_eq!(
        rig.record(),
        poller_fields(
            Some(S),
            Some(44),
            Some(SyncCount {
                at_ms: S,
                files: Some(2),
                counted_at_ms: Some(T0 + 60 * SEC)
            })
        )
    );
}

#[test]
fn a_device_that_syncs_holds_the_count() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    rig.changes(changed(Some(1), 41));
    // The newest sync is of SN1, but SN2 syncs now: wait.
    rig.answer(two_devices((Some(S + SEC), false), (Some(S - SEC), true)));
    rig.poll();
    assert!(rig.changes_calls().is_empty());
    assert_eq!(rig.record(), poller_fields(Some(S), Some(40), None));
    // Both are done: the newest sync over all devices is counted.
    rig.answer(two_devices(
        (Some(S + SEC), false),
        (Some(S + 2 * SEC), false),
    ));
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls(), vec![40]);
    assert_eq!(
        rig.record().last_count,
        Some(SyncCount {
            at_ms: S + 2 * SEC,
            files: Some(1),
            counted_at_ms: Some(T0 + 60 * SEC)
        })
    );
}

#[test]
fn a_failed_count_is_tried_again_once_a_minute() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    let shown: Arc<Mutex<Vec<Option<SyncCount>>>> = Arc::default();
    let keep = shown.clone();
    rig.poller.on_status(move |status| {
        let files = status.device.files_changed;
        let mut shown = lock(&keep);
        if shown.last() != Some(&files) {
            shown.push(files);
        }
    });
    rig.answer(synced(Some(S + SEC)));
    rig.changes(Err("no answer".into()));
    rig.poll();
    // The old values stay; the failure is kept with its time.
    assert_eq!(
        rig.record(),
        KeyRecord {
            count_failing_since: Some(T0),
            ..poller_fields(Some(S), Some(40), None)
        }
    );
    rig.advance(30 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls().len(), 1, "at most once a minute");
    rig.advance(30 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls().len(), 2);
    assert_eq!(
        rig.record().count_failing_since,
        Some(T0),
        "since the first"
    );
    rig.changes(changed(Some(12), 52));
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls(), vec![40, 40, 40]);
    let count = SyncCount {
        at_ms: S + SEC,
        files: Some(12),
        counted_at_ms: None,
    };
    assert_eq!(
        rig.record(),
        poller_fields(Some(S + SEC), Some(52), Some(count))
    );
    // One count, with the number.
    assert_eq!(*lock(&shown), vec![None, Some(count)]);
}

#[test]
fn five_minutes_of_failures_store_no_number_once() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    rig.answer(synced(Some(S + SEC)));
    rig.changes(Err("no answer".into()));
    for _ in 0..5 {
        rig.poll();
        assert_eq!(rig.record().last_count, None);
        rig.advance(60 * SEC);
    }
    rig.poll();
    assert_eq!(rig.changes_calls().len(), 6);
    // It moves on without a number. The cursor stays where the last count ended: the end of
    // this sync was never read, so the next count takes this sync's files too.
    let count = SyncCount {
        at_ms: S + SEC,
        files: None,
        counted_at_ms: None,
    };
    assert_eq!(
        rig.record(),
        poller_fields(Some(S + SEC), Some(40), Some(count))
    );
    assert_eq!(rig.poller.status().device.files_changed, Some(count));
    // Once.
    rig.advance(600 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls().len(), 6);
    // The next sync is counted again from the old cursor.
    rig.changes(changed(Some(5), 60));
    rig.answer(synced(Some(S + 2 * SEC)));
    rig.poll();
    assert_eq!(rig.changes_calls().last(), Some(&40));
    assert_eq!(rig.record().cursor, Some(60));
}

#[test]
fn a_sync_without_a_cursor_has_no_number() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), None, None));
    rig.answer(synced(Some(S + SEC)));
    *lock(&rig.fake.events_answer) = Some(Err("no answer".into()));
    rig.poll();
    assert_eq!(rig.record(), poller_fields(Some(S), None, None));
    *lock(&rig.fake.events_answer) = Some(Ok(50));
    rig.advance(60 * SEC);
    rig.poll();
    assert!(rig.changes_calls().is_empty());
    assert_eq!(
        rig.record(),
        poller_fields(
            Some(S + SEC),
            Some(50),
            Some(SyncCount {
                at_ms: S + SEC,
                files: None,
                counted_at_ms: None
            })
        )
    );
}

#[test]
fn the_import_mark_is_the_baseline() {
    use crate::config::SyncMark;
    let rig = Rig::me();
    // A record from before the import.
    rig.set_record(|r| *r = poller_fields(Some(S - 60 * SEC), Some(10), None));
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        key: ME.into(),
        seq: 30,
        at_ms: T0,
        sync_at_mark: Some(S),
    });
    let inputs = rig.inputs.clone();
    rig.poller.on_sync_checked(move |_, check| {
        lock(&inputs).config.setup.sync_checked = Some(check);
    });
    rig.changes(changed(Some(7), 38));
    // Before the first sync after the mark: the record is the mark.
    rig.answer(synced(Some(S)));
    rig.poll();
    assert_eq!(rig.record(), poller_fields(Some(S), Some(30), None));
    assert!(lock(&rig.fake.events_calls).is_empty());
    // The first sync, while its check fails: no count.
    rig.answer(synced(Some(S + SEC)));
    rig.poll();
    assert!(rig.changes_calls().is_empty());
    assert_eq!(rig.record(), poller_fields(Some(S), Some(30), None));
    // The check is saved, then the sync is counted from the mark.
    *lock(&rig.fake.check_answer) = Some(Ok(SyncCheck::default()));
    rig.advance(60 * SEC);
    rig.poll();
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls(), vec![30]);
    assert_eq!(
        rig.record(),
        poller_fields(
            Some(S + SEC),
            Some(38),
            Some(SyncCount {
                at_ms: S + SEC,
                files: Some(7),
                counted_at_ms: Some(T0 + 120 * SEC)
            })
        )
    );
    // A mark without a sync before it: any sync after it counts.
    let rig = Rig::me();
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        key: ME.into(),
        seq: 30,
        at_ms: T0,
        sync_at_mark: None,
    });
    rig.answer(synced(None));
    rig.poll();
    assert_eq!(rig.record(), poller_fields(None, Some(30), None));
    // The mark of another login is no baseline of this one.
    let rig = Rig::me();
    *lock(&rig.fake.events_answer) = Some(Ok(40));
    lock(&rig.inputs).config.setup.sync_mark = Some(SyncMark {
        key: "local|anna".into(),
        seq: 30,
        at_ms: T0,
        sync_at_mark: None,
    });
    rig.answer(synced(Some(S)));
    rig.poll();
    assert_eq!(rig.record(), poller_fields(Some(S), Some(40), None));
}

#[test]
fn a_restart_with_the_same_sync_does_nothing() {
    let mut rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    rig.changes(changed(Some(3), 45));
    rig.answer(synced(Some(S + SEC)));
    rig.poll();
    let counted = rig.record();
    rig.restart();
    rig.advance(3_600 * SEC);
    rig.poll();
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.changes_calls().len(), 1);
    assert!(lock(&rig.fake.events_calls).is_empty());
    assert_eq!(rig.record(), counted);
    // The page still shows the count after the restart.
    assert_eq!(rig.poller.status().device.files_changed, counted.last_count);
}

#[test]
fn the_notifiers_fields_stay_untouched() {
    use crate::notify::store::{ConsumerRecord, ProblemState};
    let rig = Rig::me();
    let notifier = KeyRecord {
        sync_shown_at: Some(S - SEC),
        sync_last_notice_ms: Some(T0 - SEC),
        unshown_files: 4,
        ocr_text_seq: Some(9),
        ocr_data_dir: Some(PathBuf::from("/nowhere/notedown-data")),
        ocr_last_run_ms: Some(T0 - 2 * SEC),
        ocr_last_notice_ms: Some(T0 - 3 * SEC),
        unshown_notes: 2,
        consumers: [(
            "claude".to_owned(),
            ConsumerRecord {
                seq: 5,
                pending_seq: Some(6),
                rose_at: Some(T0),
            },
        )]
        .into(),
        problems: [(
            "server".to_owned(),
            ProblemState {
                active: true,
                last_notice_ms: T0 - SEC,
                for_sync: None,
            },
        )]
        .into(),
        ..KeyRecord::default()
    };
    rig.set_record(|r| *r = notifier.clone());
    let only_notifier = |r: KeyRecord| KeyRecord {
        last_sync_ms: None,
        cursor: None,
        last_count: None,
        count_failing_since: None,
        ..r
    };
    // A baseline, a failed count, a count and a give-up.
    *lock(&rig.fake.events_answer) = Some(Ok(40));
    rig.answer(synced(Some(S)));
    rig.poll();
    assert_eq!(rig.record().cursor, Some(40));
    rig.answer(synced(Some(S + SEC)));
    rig.changes(Err("no answer".into()));
    rig.advance(60 * SEC);
    rig.poll();
    assert!(rig.record().count_failing_since.is_some());
    rig.changes(changed(Some(1), 41));
    rig.advance(60 * SEC);
    rig.poll();
    assert!(rig.record().last_count.is_some());
    rig.answer(synced(Some(S + 2 * SEC)));
    rig.changes(Err("no answer".into()));
    for _ in 0..6 {
        rig.advance(60 * SEC);
        rig.poll();
    }
    assert_eq!(rig.record().last_count.map(|c| c.files), Some(None));
    assert_eq!(only_notifier(rig.record()), notifier);
}

#[test]
fn without_a_login_nothing_is_counted() {
    let rig = Rig::new(local_config(Some("me")));
    *lock(&rig.fake.events_answer) = Some(Ok(40));
    rig.changes(changed(Some(1), 41));
    rig.answer(synced(Some(S)));
    rig.poll();
    assert!(lock(&rig.fake.events_calls).is_empty());
    assert!(rig.changes_calls().is_empty());
    assert_eq!(rig.record(), KeyRecord::default());
}

#[test]
fn a_give_up_takes_the_journal_end_as_the_cursor() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    rig.answer(synced(Some(S + SEC)));
    rig.changes(Err("no answer".into()));
    *lock(&rig.fake.events_answer) = Some(Ok(55));
    for _ in 0..6 {
        rig.poll();
        rig.advance(60 * SEC);
    }
    // The next sync does not take the files of the one without a number.
    let count = SyncCount {
        at_ms: S + SEC,
        files: None,
        counted_at_ms: None,
    };
    assert_eq!(
        rig.record(),
        poller_fields(Some(S + SEC), Some(55), Some(count))
    );
    assert_eq!(
        lock(&rig.fake.events_calls).len(),
        1,
        "once, at the give-up"
    );
}

#[test]
fn a_new_sync_starts_its_own_five_minutes() {
    let rig = Rig::me();
    // A failure left over from long ago (a restart, a device that started to sync).
    rig.set_record(|r| {
        *r = KeyRecord {
            count_failing_since: Some(T0 - 3_600 * SEC),
            ..poller_fields(Some(S), Some(40), None)
        }
    });
    rig.answer(synced(Some(S + SEC)));
    rig.changes(Err("no answer".into()));
    rig.poll();
    assert_eq!(rig.record().last_count, None, "no give-up at once");
    assert_eq!(rig.record().count_failing_since, Some(T0));
    // Three minutes on, a newer sync: its own five minutes start.
    for _ in 0..3 {
        rig.advance(60 * SEC);
        rig.poll();
    }
    rig.answer(synced(Some(S + 2 * SEC)));
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(rig.record().count_failing_since, Some(T0 + 240 * SEC));
    for _ in 0..4 {
        rig.advance(60 * SEC);
        rig.poll();
    }
    assert_eq!(
        rig.record().last_count,
        None,
        "four minutes into the new sync"
    );
    rig.advance(60 * SEC);
    rig.poll();
    assert_eq!(
        rig.record().last_count,
        Some(SyncCount {
            at_ms: S + 2 * SEC,
            files: None,
            counted_at_ms: Some(T0 + 540 * SEC)
        })
    );
    assert_eq!(rig.record().count_failing_since, None);
}

#[test]
fn a_journal_end_without_a_count_ends_the_failure() {
    let rig = Rig::me();
    rig.set_record(|r| {
        *r = KeyRecord {
            count_failing_since: Some(T0 - SEC),
            ..poller_fields(Some(S), None, None)
        }
    });
    *lock(&rig.fake.events_answer) = Some(Ok(50));
    rig.answer(synced(Some(S + SEC)));
    rig.poll();
    assert_eq!(rig.record().cursor, Some(50));
    assert_eq!(rig.record().count_failing_since, None);
}

#[test]
fn a_count_carries_this_macs_clock_only_after_a_recent_poll() {
    let rig = Rig::me();
    rig.set_record(|r| *r = poller_fields(Some(S), Some(40), None));
    let counted_at = |rig: &Rig| rig.record().last_count.and_then(|c| c.counted_at_ms);
    // The first poll of the run sees a new sync: it may be hours old, no time of this Mac.
    rig.changes(changed(Some(3), 45));
    rig.answer(synced(Some(S + SEC)));
    rig.poll();
    assert_eq!(rig.record().last_count.map(|c| c.files), Some(Some(3)));
    assert_eq!(counted_at(&rig), None);
    // A sync between two polls a minute apart: the count carries the time it was stored.
    rig.changes(changed(Some(2), 47));
    rig.answer(synced(Some(S + 2 * SEC)));
    rig.advance(60 * SEC);
    let now = rig.now.load(Ordering::SeqCst);
    rig.poll();
    assert_eq!(rig.record().last_count.map(|c| c.files), Some(Some(2)));
    assert_eq!(counted_at(&rig), Some(now));
    // After a long pause (the Mac slept) the sync may be old again.
    rig.changes(changed(Some(1), 48));
    rig.answer(synced(Some(S + 3 * SEC)));
    rig.advance(STALE_MS + SEC);
    rig.poll();
    assert_eq!(rig.record().last_count.map(|c| c.files), Some(Some(1)));
    assert_eq!(counted_at(&rig), None);
}
