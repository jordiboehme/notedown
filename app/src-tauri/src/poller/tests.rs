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
}

struct Rig {
    fake: Arc<Fake>,
    tokens: Arc<MemoryStore>,
    inputs: Arc<Mutex<Inputs>>,
    now: Arc<AtomicU64>,
    poller: Poller,
}

impl Rig {
    fn new(config: AppConfig) -> Self {
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
    assert!(text.starts_with("notedown could not start"), "{text}");
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
