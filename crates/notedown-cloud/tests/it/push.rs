//! Pushes after server-side changes: the journal follower feeds the planner, the tick sends
//! what D8 allows. Most tests drive `follow_once` and `tick` with an injected clock; the last
//! one runs the real server loop against a WebSocket client.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request;
use futures_util::StreamExt;
use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
use notedown_cloud::journal::{Event, EventBody, FileKind, Origin, new_op_id};
use notedown_cloud::push::{follow_once, server_message_frame, start_seq, startsync_message, tick};
use notedown_cloud::server::{AppState, router, serve_listeners};
use notedown_cloud::socket::Outbound;
use notedown_cloud::store::Store;
use notedown_cloud::{Clock, Config, PushMode, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

const A: &str = "SN078C10000000";
const B: &str = "SN078C10000001";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;
const START: &str = "/api/file/2/files/synchronous/start";

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(self.0.load(Ordering::SeqCst) as i128 * 1_000_000)
            .unwrap()
    }
}

/// A data folder with account `jordi` (native folders, optionally the `imported` marker), two
/// bound devices A and B, and the follower cursor where the server would start it.
struct Fixture {
    dir: tempfile::TempDir,
    src: tempfile::TempDir,
    state: Arc<AppState>,
    clock: Arc<TestClock>,
    cursor: u64,
    rx: HashMap<&'static str, Receiver<Outbound>>,
}

/// The account as after an import: native folders and, when asked, the `imported` marker.
fn seed_account(data: &Path, imported: bool) {
    Accounts::new(data)
        .create("jordi", EMAIL, "secret")
        .unwrap();
    let store = Store::open(data, "jordi").unwrap();
    {
        let g = store.lock().unwrap();
        store.ensure_native_folders(&g).unwrap();
    }
    if imported {
        mark_imported(data);
    }
}

fn mark_imported(data: &Path) {
    let accounts = Accounts::new(data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.imported = Some(Imported {
        at: "2026-10-06T10:00:00Z".into(),
        source: "/backup/Supernote".into(),
        files: 0,
        bytes: 0,
    });
    accounts.save(&a).unwrap();
}

impl Fixture {
    fn new() -> Fixture {
        Self::with(Config::default(), true)
    }

    fn with(config: Config, imported: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        seed_account(dir.path(), imported);
        let clock = Arc::new(TestClock(AtomicI64::new(T0)));
        let state = AppState::open_with_clock(config, dir.path(), clock.clone()).unwrap();
        for serial in [A, B] {
            state
                .auth
                .devices()
                .update("jordi", serial, |d| d.bound = true)
                .unwrap();
        }
        let cursor = start_seq(&state).unwrap();
        let mut f = Fixture {
            dir,
            src: tempfile::tempdir().unwrap(),
            state,
            clock,
            cursor,
            rx: HashMap::new(),
        };
        f.connect(A);
        f.connect(B);
        f
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    /// Register a session of `serial` and keep its receiver (a dropped one disconnects).
    fn connect(&mut self, serial: &'static str) {
        let rx = self
            .state
            .sockets
            .register("jordi", serial, &format!("sid-{serial}"));
        self.rx.insert(serial, rx);
    }

    fn now(&self) -> u64 {
        self.clock.0.load(Ordering::SeqCst) as u64
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// `notedown cloud put` of a new file `name` into `NOTE/Note`.
    fn put(&self, name: &str) {
        let src = self.src.path().join(name);
        std::fs::write(&src, format!("bytes of {name}")).unwrap();
        notedown_cloud::put::put(
            self.data(),
            "jordi",
            &self.state.journal,
            &src,
            "NOTE/Note",
            None,
        )
        .unwrap();
    }

    fn follow(&mut self) -> usize {
        follow_once(&self.state, &mut self.cursor).unwrap()
    }

    /// The frames waiting for `serial`, in order.
    fn frames(&mut self, serial: &str) -> Vec<String> {
        let rx = self.rx.get_mut(serial).unwrap();
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(Outbound::Frame(f)) => out.push(f),
                Ok(other) => panic!("unexpected {other:?}"),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return out,
            }
        }
    }

    fn pending(&self, serial: &str) -> bool {
        self.state
            .push
            .lock()
            .unwrap()
            .pending("jordi", serial)
            .is_some()
    }

    /// A login token for `serial`, through the same checks as the login route.
    fn token(&self, serial: &str) -> String {
        let now = self.state.now_ms();
        let (code, _) = self.state.auth.challenge(EMAIL, now).unwrap();
        let hash = expected_login_hash(&md5_hex("secret"), &code);
        self.state
            .auth
            .login(EMAIL, &hash, serial, now)
            .unwrap()
            .token
    }

    /// `synchronous/start` of `serial` through the router.
    async fn sync_start(&self, serial: &str) -> Value {
        let token = self.token(serial);
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(START)
            .header("x-access-token", token)
            .header("content-type", "application/json")
            .header("host", "192.168.1.10:19072")
            .body(Body::from(json!({"equipmentNo": serial}).to_string()))
            .unwrap();
        let resp = router(self.state.clone())
            .layer(MockConnectInfo(addr))
            .oneshot(req)
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

fn startsync(cause: Option<&str>, now_ms: u64) -> String {
    server_message_frame(&startsync_message(cause, now_ms))
}

#[test]
fn one_put_pushes_each_bound_device_once() {
    let mut f = Fixture::new();
    f.put("a.note");
    assert!(f.follow() > 0);
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 2);
    assert_eq!(f.frames(A), vec![startsync(None, t)]);
    assert_eq!(f.frames(B), vec![startsync(None, t)]);
    assert!(!f.pending(A) && !f.pending(B));
}

#[test]
fn quiet_time_holds_the_push_back() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 1999), 0);
    assert!(f.frames(A).is_empty());
    assert_eq!(tick(&f.state, f.now() + 2000), 2);
}

#[test]
fn a_burst_of_puts_gives_one_push_per_device() {
    let mut f = Fixture::new();
    for i in 0..30 {
        f.put(&format!("n{i}.note"));
        f.follow();
        assert_eq!(tick(&f.state, f.now()), 0);
        f.advance_ms(30);
    }
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 2);
    assert_eq!(f.frames(A), vec![startsync(None, t)]);
    assert_eq!(f.frames(B), vec![startsync(None, t)]);
    assert_eq!(tick(&f.state, t + 1000), 0);
    assert_eq!(tick(&f.state, t + 6000), 0);
    assert!(f.frames(A).is_empty() && f.frames(B).is_empty());
}

#[test]
fn a_device_change_is_pushed_to_the_other_devices_only() {
    let mut f = Fixture::new();
    let mut events = [Event::new(
        "jordi",
        Origin::Device,
        Some(A.to_string()),
        &new_op_id(),
        EventBody::FileCreated {
            id: 7,
            path: "/NOTE/Note/x.note".into(),
            kind: FileKind::Note,
            md5: "0".repeat(32),
            size: 1,
            pages_changed: None,
        },
    )];
    f.state.journal.append(&mut events).unwrap();
    f.follow();
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 1);
    assert!(f.frames(A).is_empty());
    assert_eq!(f.frames(B), vec![startsync(Some(A), t)]);
}

#[test]
fn no_push_while_a_device_holds_the_sync_lock() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    f.state.sync_locks.start("jordi", A, f.now());
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 0);
    assert!(f.frames(B).is_empty());
    assert!(f.pending(B));
    f.state.sync_locks.end("jordi", A, t);
    assert_eq!(tick(&f.state, t + 500), 2);
    assert_eq!(f.frames(B), vec![startsync(None, t + 500)]);
}

#[tokio::test]
async fn sync_start_covers_a_change_seen_before_it() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    assert!(f.pending(B));
    let v = f.sync_start(B).await;
    assert_eq!(v["success"], true, "{v}");
    assert!(!f.pending(B));
    assert!(f.pending(A));
    f.state.sync_locks.end("jordi", B, f.now());
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 1);
    assert_eq!(f.frames(A), vec![startsync(None, t)]);
    assert!(f.frames(B).is_empty());
}

#[tokio::test]
async fn sync_start_covers_a_change_the_follower_sees_after_it() {
    let mut f = Fixture::new();
    f.put("a.note");
    let v = f.sync_start(B).await;
    assert_eq!(v["success"], true, "{v}");
    f.state.sync_locks.end("jordi", B, f.now());
    f.follow();
    assert!(!f.pending(B));
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 1);
    assert!(f.frames(B).is_empty());
    assert_eq!(f.frames(A).len(), 1);
}

#[test]
fn closed_protect_gate_holds_the_push_back() {
    let mut f = Fixture::with(Config::default(), false);
    f.put("a.note");
    f.follow();
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 0);
    assert!(f.frames(A).is_empty() && f.frames(B).is_empty());
    assert!(f.pending(A));
    // The import finishes: the gate opens and the waiting push goes out.
    mark_imported(f.data());
    assert_eq!(tick(&f.state, t + 500), 2);
}

#[test]
fn protect_off_needs_no_imported_marker() {
    let config = Config {
        protect: false,
        ..Config::default()
    };
    let mut f = Fixture::with(config, false);
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 2000), 2);
}

#[test]
fn push_off_sends_nothing() {
    let config = Config {
        push: PushMode::Off,
        ..Config::default()
    };
    let mut f = Fixture::with(config, true);
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 2000), 0);
    assert!(f.frames(A).is_empty() && f.frames(B).is_empty());
}

#[test]
fn refused_sync_sends_nothing() {
    let config = Config {
        sync: SyncMode::Refuse,
        ..Config::default()
    };
    let mut f = Fixture::with(config, true);
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 2000), 0);
    assert!(f.frames(A).is_empty() && f.frames(B).is_empty());
}

#[test]
fn detail_mode_sends_startsync_for_now() {
    let config = Config {
        push: PushMode::Detail,
        ..Config::default()
    };
    let mut f = Fixture::with(config, true);
    f.put("a.note");
    f.follow();
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 2);
    assert_eq!(f.frames(A), vec![startsync(None, t)]);
}

#[test]
fn a_snapshot_is_no_change() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 2000), 2);
    f.frames(A);
    f.frames(B);
    let store = Store::open(f.data(), "jordi").unwrap();
    let g = store.lock_recovered(&f.state.journal).unwrap();
    assert!(!store.snapshot(&g, &f.state.journal).unwrap().is_empty());
    drop(g);
    assert!(f.follow() > 0);
    assert_eq!(tick(&f.state, f.now() + 60_000), 0);
    assert!(!f.pending(A) && !f.pending(B));
}

#[test]
fn a_device_that_is_not_connected_gets_the_push_after_it_connects() {
    let mut f = Fixture::new();
    drop(f.rx.remove(B));
    f.state.sockets.unregister("jordi", B, &format!("sid-{B}"));
    f.put("a.note");
    f.follow();
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 1);
    assert_eq!(f.frames(A).len(), 1);
    assert!(f.pending(B));
    f.connect(B);
    assert_eq!(tick(&f.state, t + 500), 1);
    assert_eq!(f.frames(B), vec![startsync(None, t + 500)]);
    assert!(!f.pending(B));
}

#[test]
fn unbound_devices_get_no_push() {
    let mut f = Fixture::new();
    f.state
        .auth
        .devices()
        .update("jordi", B, |d| d.bound = false)
        .unwrap();
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 2000), 1);
    assert!(f.frames(B).is_empty());
}

#[test]
fn the_follower_starts_at_the_end_of_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), true);
    let src = dir.path().join("old.note");
    std::fs::write(&src, b"old").unwrap();
    let journal = notedown_cloud::journal::Journal::open(dir.path()).unwrap();
    notedown_cloud::put::put(dir.path(), "jordi", &journal, &src, "NOTE/Note", None).unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(T0)));
    let state = AppState::open_with_clock(Config::default(), dir.path(), clock).unwrap();
    for serial in [A, B] {
        state
            .auth
            .devices()
            .update("jordi", serial, |d| d.bound = true)
            .unwrap();
    }
    let mut rx = state.sockets.register("jordi", A, "sid-a");
    let mut cursor = start_seq(&state).unwrap();
    assert!(cursor > 0);
    assert_eq!(follow_once(&state, &mut cursor).unwrap(), 0);
    assert_eq!(tick(&state, state.now_ms() + 60_000), 0);
    assert!(rx.try_recv().is_err());
}

// ---- the real loop ----

/// The server on `127.0.0.1:0` with the real clock; device A bound to an imported account.
#[tokio::test]
async fn the_server_pushes_startsync_to_a_connected_device_after_a_put() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), true);
    let state = AppState::open(Config::default(), dir.path()).unwrap();
    state
        .auth
        .devices()
        .update("jordi", A, |d| d.bound = true)
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let server = tokio::spawn(serve_listeners(vec![l], state.clone()));

    let now = state.now_ms();
    let (code, _) = state.auth.challenge(EMAIL, now).unwrap();
    let hash = expected_login_hash(&md5_hex("secret"), &code);
    let token = state.auth.login(EMAIL, &hash, A, now).unwrap().token;
    let url = format!(
        "ws://{addr}/socket.io/?token={token}&type={A}&random=1791270408879&sign=c2lnbmF0dXJlLXZhbHVl&EIO=3&transport=websocket"
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    // Past the handshake (open packet, `40`) the session is registered.
    let mut texts = Vec::new();
    while texts.len() < 2 {
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            texts.push(t.to_string());
        }
    }
    assert_eq!(texts[1], "40");

    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("pushed.note");
    std::fs::write(&file, b"pushed").unwrap();
    notedown_cloud::put::put(
        dir.path(),
        "jordi",
        &state.journal,
        &file,
        "NOTE/Note",
        None,
    )
    .unwrap();

    let message = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) if t.starts_with("42[\"ServerMessage\"") => {
                    return t.to_string();
                }
                Some(Ok(_)) => continue,
                other => panic!("connection ended: {other:?}"),
            }
        }
    })
    .await
    .expect("a push within 15 s");
    let frame: Value = serde_json::from_str(&message[2..]).unwrap();
    let inner: Value = serde_json::from_str(frame[1].as_str().unwrap()).unwrap();
    assert_eq!(inner["msgType"], "FILE-SYN");
    assert_eq!(inner["data"][0]["messageType"], "STARTSYNC");
    assert_eq!(inner["data"][0]["equipmentNo"], "null-null");
    server.abort();
}
