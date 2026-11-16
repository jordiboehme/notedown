//! `RestSource`: the server's data over the REST API. A real server runs on a tokio runtime in
//! another thread; the blocking source runs on the test thread. Small fake servers play the
//! failures a real one never shows (a hang, a half download, another program on the port).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex, revoke_all_tokens};
use notedown_cloud::config::{Config, PushMode, SyncMode};
use notedown_cloud::journal::Journal;
use notedown_cloud::server::{AppState, bind, serve_until_with};
use notedown_pipeline::cursor;
use notedown_pipeline::rest::RestSource;
use notedown_pipeline::setup::Setup;
use notedown_pipeline::source::{NoteBytes, Removal, ServerData};
use notedown_pipeline::worker::{Clock, Worker};
use tempfile::TempDir;

use crate::source::contract;
use crate::worker::{Data, SLUG, TestClock, engine_reading, no_settle, now_ms, setup, stop_at};

/// A notedown server over a data folder, on a runtime of its own thread.
pub(crate) struct Server {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub(crate) fn start(data: &Path) -> Server {
        let data = data.to_path_buf();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                // Nothing on the server writes to the journal by itself: no scan timer, no
                // watch, no push, no renders.
                let config = Config {
                    listen: vec!["127.0.0.1:0".parse().unwrap()],
                    sync: SyncMode::Refuse,
                    push: PushMode::Off,
                    watch: false,
                    scan_interval: 0,
                    render_dir: Some(String::new()),
                    ..Config::default()
                };
                let listeners = bind(&config).await.unwrap();
                let addr = listeners[0].local_addr().unwrap();
                let state = AppState::open(config, &data).unwrap();
                addr_tx.send(addr).unwrap();
                serve_until_with(
                    listeners,
                    state.clone(),
                    notedown_cloud::rest::router(state),
                    async {
                        let _ = stop_rx.await;
                    },
                )
                .await
                .unwrap();
            });
        });
        let addr = addr_rx.recv().unwrap();
        Server {
            addr,
            stop: Some(stop_tx),
            thread: Some(thread),
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Logs `email` in over REST and gives the new token.
fn login(url: &str, email: &str, password: &str) -> String {
    let client = reqwest::blocking::Client::new();
    let code = client
        .post(format!("{url}/rest/v1/login/code"))
        .body(serde_json::json!({ "email": email }).to_string())
        .send()
        .unwrap();
    assert_eq!(code.status().as_u16(), 200);
    let code: serde_json::Value = serde_json::from_slice(&code.bytes().unwrap()).unwrap();
    let hash = expected_login_hash(&md5_hex(password), code["randomCode"].as_str().unwrap());
    let answer = client
        .post(format!("{url}/rest/v1/login"))
        .body(
            serde_json::json!({ "email": email, "passwordHash": hash, "name": "test" }).to_string(),
        )
        .send()
        .unwrap();
    assert_eq!(answer.status().as_u16(), 201);
    let answer: serde_json::Value = serde_json::from_slice(&answer.bytes().unwrap()).unwrap();
    answer["token"].as_str().unwrap().to_string()
}

fn write_token(tokens: &Path, slug: &str, token: &str) {
    std::fs::create_dir_all(tokens).unwrap();
    std::fs::write(tokens.join(format!("{slug}.token")), format!("{token}\n")).unwrap();
}

/// A server over `setup()` and the Mac's local folder with its `tokens/`.
struct Rig {
    d: Data,
    server: Server,
    local: TempDir,
}

impl Rig {
    fn new() -> Rig {
        let d = setup();
        let server = Server::start(&d.data);
        Rig {
            d,
            server,
            local: tempfile::tempdir().unwrap(),
        }
    }

    fn tokens(&self) -> PathBuf {
        self.local.path().join("tokens")
    }

    /// Logs account `slug` in (`<slug>@example.org`, as the tests create them) and writes its
    /// token file, as `notedown connect` does.
    fn connect(&self, slug: &str) -> String {
        let token = login(&self.server.url(), &format!("{slug}@example.org"), "secret");
        write_token(&self.tokens(), slug, &token);
        token
    }

    fn source(&self) -> RestSource {
        RestSource::new(&self.server.url(), &self.tokens(), self.local.path()).unwrap()
    }
}

fn create_account(data: &Path, slug: &str) -> u64 {
    Accounts::new(data)
        .create(slug, &format!("{slug}@example.org"), "secret")
        .unwrap()
        .user_id
}

fn put_into(data: &Path, slug: &str, src: &Path, folder: &str) {
    let journal = Journal::open(data).unwrap();
    notedown_cloud::put::put(data, slug, &journal, src, folder, None).unwrap();
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/notes")
        .join(format!("{name}.note"))
}

/// A token of the right form for `slug`, which no server signed.
fn fake_token(slug: &str, nonce: &str) -> String {
    let payload = serde_json::json!({
        "slug": slug, "userId": 1_000_001, "id": "0123abcd", "nonce": nonce,
    });
    format!(
        "ndt1.{}.{}",
        URL_SAFE_NO_PAD.encode(payload.to_string()),
        URL_SAFE_NO_PAD.encode([7u8; 32])
    )
}

/// An HTTP answer that closes the connection.
fn answer(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    let mut length = false;
    for (k, v) in headers {
        length |= k.eq_ignore_ascii_case("content-length");
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !length {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

fn json_answer(status: &str, body: serde_json::Value) -> Vec<u8> {
    answer(
        status,
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    )
}

/// A fake server: `reply` gets the request head and gives the bytes to send; it counts the
/// requests.
struct Fake {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
}

impl Fake {
    fn start(reply: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let reply = Arc::new(reply);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let count = count.clone();
                let reply = reply.clone();
                std::thread::spawn(move || {
                    let Some(head) = read_head(&mut stream) else {
                        return;
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(&reply(&head));
                    let _ = stream.flush();
                });
            }
        });
        Fake { addr, requests }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).ok()
}

/// The request line's path and query.
fn target(head: &str) -> &str {
    head.split(' ').nth(1).unwrap_or("")
}

fn bearer(head: &str) -> &str {
    head.lines()
        .find_map(|l| {
            l.strip_prefix("authorization: Bearer ")
                .or_else(|| l.strip_prefix("Authorization: Bearer "))
        })
        .unwrap_or("")
}

fn account_json(slug: &str) -> Vec<u8> {
    json_answer(
        "200 OK",
        serde_json::json!({ "slug": slug, "userId": 1_000_001 }),
    )
}

/// A source over a fake server with one token file per `(slug, token)`.
fn fake_source(fake: &Fake, tokens: &[(&str, &str)]) -> (RestSource, TempDir) {
    let local = tempfile::tempdir().unwrap();
    let dir = local.path().join("tokens");
    std::fs::create_dir_all(&dir).unwrap();
    for (slug, token) in tokens {
        write_token(&dir, slug, token);
    }
    let src = RestSource::new(&fake.url(), &dir, local.path()).unwrap();
    (src, local)
}

#[test]
fn rest_source_meets_the_contract() {
    let rig = Rig::new();
    rig.connect(SLUG);
    let src = rig.source();
    contract(&src, &rig.d, &|slug| {
        rig.connect(slug);
    });
    let accounts = src.accounts().unwrap();
    assert!(accounts.iter().any(|a| a.slug == SLUG && a.user_id > 0));
    assert!(!accounts.iter().any(|a| a.slug == "bob"), "bob was removed");
    assert_eq!(src.describe(), rig.server.url());
}

#[test]
fn a_server_that_goes_away_is_nothing_known() {
    let mut rig = Rig::new();
    rig.connect(SLUG);
    let key = rig.d.put("test1", "NOTE/Note");
    let src = rig.source();
    assert_eq!(src.accounts().unwrap().len(), 1);
    assert!(src.records(SLUG).unwrap().complete);
    rig.server.stop();
    assert!(src.accounts().is_err(), "nothing known, not []");
    assert!(src.records(SLUG).is_err());
    assert!(src.record(SLUG, &key).is_err());
    assert!(src.note(SLUG, "NOTE/Note/test1.note").is_err());
    assert!(src.note_exists(SLUG, "NOTE/Note/test1.note").is_err());
    assert!(src.digests(SLUG).is_err());
    assert!(src.journal(SLUG, 1, 10).is_err());
}

#[test]
fn a_hung_server_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
        }
    });
    let local = tempfile::tempdir().unwrap();
    let dir = local.path().join("tokens");
    write_token(&dir, SLUG, &fake_token(SLUG, "n"));
    let src = RestSource::new(&format!("http://{addr}"), &dir, local.path()).unwrap();
    let start = Instant::now();
    assert!(src.accounts().is_err());
    let took = start.elapsed();
    assert!(took >= Duration::from_secs(10), "{took:?}");
    assert!(took < Duration::from_secs(25), "{took:?}");
}

#[test]
fn a_half_download_is_nothing_known() {
    let fake = Fake::start(|head| {
        if target(head).starts_with("/rest/v1/account") {
            return account_json(SLUG);
        }
        answer(
            "200 OK",
            &[("Content-Length", "1000"), ("X-Notedown-Note", "found")],
            &[1u8; 500],
        )
    });
    let token = fake_token(SLUG, "n");
    let (src, _local) = fake_source(&fake, &[(SLUG, &token)]);
    assert!(src.note(SLUG, "NOTE/Note/test1.note").is_err());
}

#[test]
fn another_server_is_nothing_known() {
    let token = fake_token(SLUG, "n");
    // The device API's catch-all answers 200 to anything.
    let catch_all = Fake::start(|_| json_answer("200 OK", serde_json::json!({"success": true})));
    let (src, _l1) = fake_source(&catch_all, &[(SLUG, &token)]);
    assert!(src.accounts().is_err());
    assert!(src.records(SLUG).is_err());
    assert!(src.note(SLUG, "NOTE/Note/a.note").is_err());
    assert!(!matches!(
        src.note_exists(SLUG, "NOTE/Note/a.note"),
        Ok(Some(false))
    ));
    assert!(src.journal(SLUG, 1, 10).is_err());

    // A server that knows the account but answers notes without our JSON.
    let cases: Vec<Vec<u8>> = vec![
        answer("200 OK", &[], b"hello"),
        answer("404 Not Found", &[], b"not found"),
        json_answer("404 Not Found", serde_json::json!({"error": "no_route"})),
        json_answer("409 Conflict", serde_json::json!({"error": "other"})),
        answer("302 Found", &[("Location", "/rest/v1/note?path=x")], b""),
    ];
    for case in cases {
        let fake = Fake::start(move |head| {
            if target(head).starts_with("/rest/v1/account") {
                return account_json(SLUG);
            }
            case.clone()
        });
        let (src, _local) = fake_source(&fake, &[(SLUG, &token)]);
        let note = src.note(SLUG, "NOTE/Note/a.note");
        assert!(note.is_err(), "{note:?}");
        let exists = src.note_exists(SLUG, "NOTE/Note/a.note");
        assert!(matches!(exists, Err(_) | Ok(None)), "{exists:?}");
    }

    // Our header without our JSON: `note()` gives no `Missing` (only the HEAD trusts the
    // header).
    let fake = Fake::start(|head| {
        if target(head).starts_with("/rest/v1/account") {
            return account_json(SLUG);
        }
        answer("404 Not Found", &[("X-Notedown-Note", "missing")], b"gone")
    });
    let (src, _local) = fake_source(&fake, &[(SLUG, &token)]);
    assert!(src.note(SLUG, "NOTE/Note/a.note").is_err());
}

#[test]
fn a_note_path_with_a_plus_and_a_space_is_sent_as_it_is() {
    let rig = Rig::new();
    rig.connect(SLUG);
    let dir = tempfile::tempdir().unwrap();
    let odd = dir.path().join("a+b c&d#e%f.note");
    std::fs::copy(fixture("test1"), &odd).unwrap();
    put_into(&rig.d.data, SLUG, &odd, "NOTE/Note");
    let src = rig.source();
    let want = std::fs::read(fixture("test1")).unwrap();
    assert_eq!(
        src.note(SLUG, "NOTE/Note/a+b c&d#e%f.note").unwrap(),
        NoteBytes::Found(want)
    );
    assert_eq!(
        src.note_exists(SLUG, "NOTE/Note/a+b c&d#e%f.note").unwrap(),
        Some(true)
    );
    // Read as a form, `+` would be a space: that note is not there.
    assert_eq!(
        src.note(SLUG, "NOTE/Note/a b c&d#e%f.note").unwrap(),
        NoteBytes::Missing
    );
    assert_eq!(
        src.note_exists(SLUG, "NOTE/Note/a b c&d#e%f.note").unwrap(),
        Some(false)
    );
}

pub(crate) fn refused_message(url: &str, local: &Path, slug: &str) -> String {
    format!(
        "the server refused the token of {slug}; run notedown connect {url} --data {} again",
        local.display()
    )
}

#[test]
fn a_refused_token_is_left_out_and_reported() {
    let rig = Rig::new();
    create_account(&rig.d.data, "anna");
    rig.connect(SLUG);
    rig.connect("anna");
    let src = rig.source();
    assert_eq!(src.accounts().unwrap().len(), 2);
    assert!(src.refusals().is_empty());
    revoke_all_tokens(&rig.d.data, SLUG).unwrap();
    let accounts = src.accounts().unwrap();
    let slugs: Vec<&str> = accounts.iter().map(|a| a.slug.as_str()).collect();
    assert_eq!(slugs, ["anna"]);
    assert_eq!(
        src.refusals(),
        [(
            SLUG.to_string(),
            refused_message(&rig.server.url(), rig.local.path(), SLUG)
        )]
    );
    assert!(src.records(SLUG).is_err());
    assert!(src.journal(SLUG, 1, 10).is_err());
    // A new login (notedown connect) settles it.
    rig.connect(SLUG);
    assert_eq!(src.accounts().unwrap().len(), 2);
    assert!(src.refusals().is_empty());
}

#[test]
fn a_refused_token_is_not_polled_every_second() {
    let fake = Fake::start(|_| {
        json_answer(
            "401 Unauthorized",
            serde_json::json!({"error": "token_refused"}),
        )
    });
    let token = fake_token(SLUG, "n");
    let (src, local) = fake_source(&fake, &[(SLUG, &token)]);
    let clock = TestClock::at(now_ms());
    let src = src.with_clock(Arc::new(clock.clone()));
    for _ in 0..5 {
        assert_eq!(src.accounts().unwrap(), vec![]);
        clock.sleep(Duration::from_secs(10));
    }
    assert_eq!(fake.requests(), 1, "one probe in a minute");
    assert!(src.records(SLUG).is_err());
    assert!(src.journal(SLUG, 1, 10).is_err());
    assert_eq!(fake.requests(), 1, "a refused token sends nothing else");
    assert_eq!(src.refusals().len(), 1);
    clock.sleep(Duration::from_secs(11));
    src.accounts().unwrap();
    assert_eq!(fake.requests(), 2, "asked again after a minute");
    // A changed file is asked at once.
    write_token(
        &local.path().join("tokens"),
        SLUG,
        &fake_token(SLUG, "another"),
    );
    src.accounts().unwrap();
    assert_eq!(fake.requests(), 3);
}

#[test]
fn a_new_token_file_is_picked_up() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.tokens()).unwrap();
    let src = rig.source();
    assert_eq!(src.accounts().unwrap(), vec![]);
    rig.connect(SLUG);
    let accounts = src.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].slug, SLUG);
}

#[test]
fn a_missing_tokens_folder_is_nothing_known() {
    let rig = Rig::new();
    let src = rig.source();
    let e = src.accounts().unwrap_err().to_string();
    assert!(e.contains("notedown connect"), "{e}");
}

#[test]
fn a_token_under_the_wrong_name_is_left_out() {
    let rig = Rig::new();
    create_account(&rig.d.data, "anna");
    let token = login(&rig.server.url(), "jordi@example.org", "secret");
    write_token(&rig.tokens(), "anna", &token);
    // A fresh source, asked for anna's data before any `accounts()`.
    let src = rig.source();
    assert!(src.records("anna").is_err());
    assert!(src.journal("anna", 1, 10).is_err());
    assert!(src.note("anna", "NOTE/Note/test1.note").is_err());
    assert!(src.digests("anna").is_err());
    assert_eq!(src.accounts().unwrap(), vec![]);
    let file = rig.tokens().join("anna.token");
    assert_eq!(
        src.refusals(),
        [(
            "anna".to_string(),
            format!(
                "{} holds a token of {SLUG}; run notedown connect again",
                file.display()
            )
        )]
    );

    // A server that says the token is another account's, whatever the token claims.
    let fake = Fake::start(|head| {
        if target(head).starts_with("/rest/v1/account") {
            return account_json(SLUG);
        }
        // A good answer for any account: only the account check keeps it out.
        json_answer(
            "200 OK",
            serde_json::json!({"records": {}, "unreadable": [], "complete": true}),
        )
    });
    let token = fake_token("anna", "n");
    let (src, local) = fake_source(&fake, &[("anna", &token)]);
    assert!(src.records("anna").is_err());
    assert_eq!(src.accounts().unwrap(), vec![]);
    let file = local.path().join("tokens/anna.token");
    assert_eq!(
        src.refusals(),
        [(
            "anna".to_string(),
            format!(
                "{} holds a token of {SLUG}; run notedown connect again",
                file.display()
            )
        )]
    );
}

#[test]
fn one_bad_account_does_not_stop_the_others() {
    let good = fake_token("anna", "good");
    let bad = fake_token("bob", "bad");
    let g = good.clone();
    let fake = Fake::start(move |head| {
        if bearer(head) == g {
            account_json("anna")
        } else {
            json_answer(
                "500 Internal Server Error",
                serde_json::json!({"error": "internal"}),
            )
        }
    });
    let (src, _local) = fake_source(&fake, &[("anna", &good), ("bob", &bad)]);
    let accounts = src.accounts().unwrap();
    let slugs: Vec<&str> = accounts.iter().map(|a| a.slug.as_str()).collect();
    assert_eq!(slugs, ["anna"]);
    assert!(src.refusals().is_empty(), "a failure is no refusal");

    // When no account answers, nothing is known.
    let down = Fake::start(|_| answer("503 Service Unavailable", &[], b"busy"));
    let (src, _local) = fake_source(&down, &[("anna", &good), ("bob", &bad)]);
    assert!(src.accounts().is_err());
}

#[test]
fn a_removed_account_token_reads_its_removal() {
    let rig = Rig::new();
    let bob = create_account(&rig.d.data, "bob");
    rig.connect(SLUG);
    rig.connect("bob");
    put_into(&rig.d.data, "bob", &fixture("test2"), "NOTE/Note");
    let src = rig.source();
    assert_eq!(src.accounts().unwrap().len(), 2);
    let from = src.journal("bob", 1, 10_000).unwrap().next;
    Accounts::new(&rig.d.data)
        .remove("bob", &Journal::open(&rig.d.data).unwrap())
        .unwrap();
    // Another account's change after the removal.
    rig.d.put("test1", "NOTE/Note");
    let accounts = src.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].slug, SLUG);
    assert!(src.refusals().is_empty(), "a removal is no refusal to fix");
    assert!(src.records("bob").is_err());
    assert!(src.note_exists("bob", "NOTE/Note/test2.note").is_err());
    let b = src.journal("bob", from, 10_000).unwrap();
    assert!(b.changed);
    assert_eq!(
        b.removed,
        [Removal {
            seq: b.end,
            user_id: bob
        }]
    );
    let until = b.end;
    assert!(Journal::open(&rig.d.data).unwrap().last_seq().unwrap() > until);
    // A position past the removal: `next` comes back below `from`, `end` is the removal. The
    // worker reads that as a journal that ends before its cursor and starts over (see
    // `a_removal_behind_the_cursor_is_read_once`).
    let ahead = src.journal("bob", until + 5, 10_000).unwrap();
    assert_eq!(ahead.next, until + 1);
    assert!(ahead.next < until + 5);
    assert_eq!(ahead.end, until);
    assert!(ahead.removed.is_empty());
}

#[test]
fn errors_never_show_the_token() {
    let mut rig = Rig::new();
    create_account(&rig.d.data, "anna");
    let jordi = rig.connect(SLUG);
    let anna = rig.connect("anna");
    write_token(&rig.tokens(), "bob", &jordi);
    let src = rig.source();
    let texts = std::cell::RefCell::new(Vec::new());
    let keep = |r: Result<String, notedown_pipeline::Error>| {
        if let Err(e) = r {
            texts.borrow_mut().push(e.to_string());
            texts.borrow_mut().push(format!("{e:?}"));
        }
    };
    keep(src.records("bob").map(|_| String::new()));
    revoke_all_tokens(&rig.d.data, "anna").unwrap();
    keep(src.records("anna").map(|_| String::new()));
    keep(src.accounts().map(|_| String::new()));
    for (_, message) in src.refusals() {
        texts.borrow_mut().push(message);
    }
    rig.server.stop();
    keep(src.accounts().map(|_| String::new()));
    keep(src.records(SLUG).map(|_| String::new()));
    keep(src.note(SLUG, "NOTE/Note/x.note").map(|_| String::new()));
    keep(
        src.note_exists(SLUG, "NOTE/Note/x.note")
            .map(|_| String::new()),
    );
    keep(src.digests(SLUG).map(|_| String::new()));
    keep(src.journal(SLUG, 1, 1).map(|_| String::new()));
    keep(src.record(SLUG, "F1").map(|_| String::new()));
    let broken = Fake::start(|_| {
        json_answer(
            "500 Internal Server Error",
            serde_json::json!({"error": "internal"}),
        )
    });
    let (fsrc, _l) = fake_source(&broken, &[(SLUG, &jordi)]);
    keep(fsrc.accounts().map(|_| String::new()));
    keep(fsrc.records(SLUG).map(|_| String::new()));
    let texts = texts.into_inner();
    assert!(texts.len() >= 20, "{texts:?}");
    for token in [&jordi, &anna] {
        let parts: Vec<&str> = token.split('.').skip(1).collect();
        for t in &texts {
            assert!(!t.contains(token.as_str()), "{t}");
            for p in &parts {
                assert!(!t.contains(p), "{t}");
            }
        }
    }
}

// The worker over REST.

#[derive(Clone, Default)]
struct Said(Arc<Mutex<Vec<String>>>);

impl Said {
    fn sink(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        let lines = self.0.clone();
        Arc::new(move |s: &str| lines.lock().unwrap().push(s.to_string()))
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, part: &str) -> usize {
        self.lines().iter().filter(|l| l.contains(part)).count()
    }
}

/// The Mac's setup with the REST source of `rig` and the test clock.
fn rest_setup(rig: &Rig, clock: &TestClock) -> Setup {
    std::fs::write(
        rig.local.path().join("pipeline.toml"),
        format!("[server]\nfolder = {:?}\n", rig.d.data.to_str().unwrap()),
    )
    .unwrap();
    let mut s = Setup::load(rig.local.path()).unwrap();
    s.cfg = no_settle();
    s.source = Arc::new(rig.source().with_clock(Arc::new(clock.clone())));
    s
}

fn worker(s: &Setup, clock: &TestClock, said: &Said) -> Worker {
    Worker::new(s, Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink())
}

#[test]
fn a_removal_behind_the_cursor_is_read_once() {
    let rig = Rig::new();
    rig.connect(SLUG);
    rig.d.put("test1", "NOTE/Note");
    let clock = TestClock::at(now_ms());
    let s = rest_setup(&rig, &clock);
    let said = Said::default();
    worker(&s, &clock, &said)
        .run(stop_at(&clock, clock.now_ms() + 1))
        .unwrap();
    let store = rig.local.path().join("pipeline").join(SLUG);
    assert!(store.join("owner.json").exists(), "{:?}", said.lines());

    Accounts::new(&rig.d.data)
        .remove(SLUG, &Journal::open(&rig.d.data).unwrap())
        .unwrap();
    let removal = Journal::open(&rig.d.data).unwrap().last_seq().unwrap();
    create_account(&rig.d.data, "anna");
    put_into(&rig.d.data, "anna", &fixture("test2"), "NOTE/Note");
    let last = Journal::open(&rig.d.data).unwrap().last_seq().unwrap();
    assert!(last > removal + 2);
    // The cursor is past the removal, but not past the journal: only the removed token's
    // journal (which ends at the removal) is behind it.
    cursor::write(rig.local.path(), SLUG, removal + 2).unwrap();
    let said = Said::default();
    worker(&s, &clock, &said)
        .run(stop_at(&clock, clock.now_ms() + 10_000))
        .unwrap();
    let rewind = format!(
        "notedown pipeline: the journal of {SLUG} ends at {removal}, before this pipeline's \
         position {}; reading it again from the start",
        removal + 2
    );
    assert_eq!(said.count(&rewind), 1, "{:?}", said.lines());
    assert_eq!(
        said.count(&format!(
            "notedown pipeline: account {SLUG} was removed on the server; its note text is \
             deleted"
        )),
        1,
        "{:?}",
        said.lines()
    );
    assert!(!store.exists(), "{:?}", said.lines());
}

#[test]
fn the_worker_says_a_refusal_once() {
    let rig = Rig::new();
    rig.connect(SLUG);
    rig.d.put("test1", "NOTE/Note");
    let clock = TestClock::at(now_ms());
    let s = rest_setup(&rig, &clock);
    revoke_all_tokens(&rig.d.data, SLUG).unwrap();
    let said = Said::default();
    let mut w = worker(&s, &clock, &said);
    w.run(stop_at(&clock, clock.now_ms() + 130_000)).unwrap();
    let message = format!(
        "notedown pipeline: {}",
        refused_message(&rig.server.url(), rig.local.path(), SLUG)
    );
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());
    // A new login settles it; a later refusal is said again.
    rig.connect(SLUG);
    w.run(stop_at(&clock, clock.now_ms() + 3_000)).unwrap();
    revoke_all_tokens(&rig.d.data, SLUG).unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 3_000)).unwrap();
    assert_eq!(said.count(&message), 2, "{:?}", said.lines());
}

#[test]
fn a_flapping_network_is_said_once() {
    // Every other request is closed after its head, the others get an answer that is no HTTP:
    // two kinds of transport error, one after the other.
    let turn = Arc::new(AtomicUsize::new(0));
    let t = turn.clone();
    let fake = Fake::start(move |_| {
        if t.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
            Vec::new()
        } else {
            b"nonsense\r\n\r\n".to_vec()
        }
    });
    let (src, local) = fake_source(&fake, &[(SLUG, &fake_token(SLUG, "a"))]);
    std::fs::write(
        local.path().join("pipeline.toml"),
        format!("[server]\nurl = {:?}\n", fake.url()),
    )
    .unwrap();
    let clock = TestClock::at(now_ms());
    let mut s = Setup::load(local.path()).unwrap();
    s.cfg = no_settle();
    s.source = Arc::new(src.with_clock(Arc::new(clock.clone())));
    let said = Said::default();
    worker(&s, &clock, &said)
        .run(stop_at(&clock, clock.now_ms() + 120_000))
        .unwrap();
    assert!(fake.requests() > 10, "{}", fake.requests());
    assert_eq!(said.count(&fake.url()), 1, "{:?}", said.lines());
}

/// The body of a request whose head is `head`, read from `stream` by its `Content-Length`.
fn read_body(stream: &mut TcpStream, head: &str) -> Vec<u8> {
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    body
}

#[test]
fn connect_leaves_the_scope_out() {
    // A server that gives a code, keeps the login body and refuses the login.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let logins: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let seen = logins.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let Some(head) = read_head(&mut stream) else {
                continue;
            };
            let body = read_body(&mut stream, &head);
            let reply = match target(&head) {
                "/rest/v1/login/code" => json_answer(
                    "200 OK",
                    serde_json::json!({ "randomCode": "1234", "timestamp": 1 }),
                ),
                "/rest/v1/login" => {
                    seen.lock()
                        .unwrap()
                        .push(serde_json::from_slice(&body).unwrap());
                    json_answer(
                        "401 Unauthorized",
                        serde_json::json!({ "error": "login_failed" }),
                    )
                }
                _ => json_answer("404 Not Found", serde_json::json!({ "error": "no_route" })),
            };
            let _ = stream.write_all(&reply);
        }
    });
    let local = tempfile::tempdir().unwrap();
    let e = notedown_pipeline::connect::connect(
        &url,
        local.path(),
        "jordi@example.org",
        "secret",
        "test",
    )
    .unwrap_err();
    assert!(e.to_string().contains("refused"), "{e}");
    let logins = logins.lock().unwrap();
    assert_eq!(logins.len(), 1);
    // An older server refuses unknown fields; a login without scope gives a read token.
    assert!(logins[0].get("scope").is_none(), "{}", logins[0]);
}
