//! Pushes after server-side changes: the journal follower feeds the planner, the tick hands over
//! what D8 allows, and a session sends its queued items after its next heartbeat reply. Most
//! tests drive `follow_once`, `tick` and the registry's `deliver` (a `ratta_ping`) with an
//! injected clock; the last ones run the real server loop against a WebSocket client.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request;
use futures_util::{SinkExt, StreamExt};
use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
use notedown_cloud::journal::{Event, EventBody, FileKind, Origin, new_op_id};
use notedown_cloud::push::{
    RETRY_MS, file_syn_message, follow_once, server_message_frame, start_seq, startsync_item, tick,
};
use notedown_cloud::server::{AppState, router, serve_listeners};
use notedown_cloud::socket::{LIVENESS_MS, LastInbound, Outbound, RESEND_MS};
use notedown_cloud::store::Store;
use notedown_cloud::{Clock, Config, PushMode, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

const A: &str = "SN078C10000000";
const B: &str = "SN078C10000001";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;
const START: &str = "/api/file/2/files/synchronous/start";
/// The planner's minimum gap between two pushes to one device, as a clock step.
const MIN_GAP: i64 = notedown_cloud::push::MIN_GAP_MS as i64;

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
    /// The last inbound time of each session, as the socket handler sets it per frame.
    inbound: HashMap<&'static str, LastInbound>,
    /// The session id of each device's current session.
    sids: HashMap<&'static str, String>,
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
    /// Most message tests use `startsync`: its one item is easy to compare. `detail` is the
    /// default ([`detail_fixture`]).
    fn new() -> Fixture {
        Self::with(
            Config {
                push: PushMode::Startsync,
                ..Config::default()
            },
            true,
        )
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
            inbound: HashMap::new(),
            sids: HashMap::new(),
        };
        f.connect(A);
        f.connect(B);
        f
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    /// Register a session of `serial` now and keep its receiver (a dropped one disconnects).
    fn connect(&mut self, serial: &'static str) {
        self.connect_as(serial, &format!("sid-{serial}"));
    }

    /// Register a session of `serial` with session id `sid` now.
    fn connect_as(&mut self, serial: &'static str, sid: &str) {
        let (rx, inbound) = self
            .state
            .sockets
            .register("jordi", serial, sid, self.now());
        self.rx.insert(serial, rx);
        self.inbound.insert(serial, inbound);
        self.sids.insert(serial, sid.to_string());
    }

    /// A `ratta_ping` of `serial` now: the frame the session sends after its reply, if any.
    fn ping(&self, serial: &str) -> Option<String> {
        self.heartbeat(serial);
        self.state
            .sockets
            .deliver("jordi", serial, &self.sids[serial], self.now())
    }

    /// A `ratta_ping` of `serial` after the re-send window: the unacknowledged push again.
    fn ping_again(&self, serial: &str) -> String {
        self.advance_ms(RESEND_MS as i64);
        self.ping(serial).expect("the push again")
    }

    /// A `ClientMessage "Received"` of `serial`.
    fn received(&self, serial: &str) {
        self.heartbeat(serial);
        self.state
            .sockets
            .received("jordi", serial, &self.sids[serial]);
    }

    /// A frame from `serial` arrives now (a heartbeat).
    fn heartbeat(&self, serial: &str) {
        self.inbound[serial].store(self.now(), Ordering::SeqCst);
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

    /// Everything waiting for `serial`, in order.
    fn outbound(&mut self, serial: &str) -> Vec<Outbound> {
        let rx = self.rx.get_mut(serial).unwrap();
        let mut out = Vec::new();
        while let Ok(o) = rx.try_recv() {
            out.push(o);
        }
        out
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

/// The `ServerMessage` frame of one `STARTSYNC` item built at `built` and sent at `sent`.
fn startsync(cause: Option<&str>, built: u64, sent: u64) -> String {
    server_message_frame(&file_syn_message(&[startsync_item(cause, built)], sent))
}

/// The items of a `ServerMessage` frame and its send time.
fn message(frame: &str) -> (Vec<Value>, u64) {
    let v: Value = serde_json::from_str(frame.strip_prefix("42").unwrap()).unwrap();
    assert_eq!(v[0], "ServerMessage");
    let text = v[1].as_str().unwrap();
    let m: Value = serde_json::from_str(text).unwrap();
    assert!(m.get("msg").is_none(), "{text}");
    fn no_null(v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Array(a) => a.iter().all(no_null),
            Value::Object(o) => o.values().all(no_null),
            _ => true,
        }
    }
    assert!(no_null(&m), "{text}");
    assert_eq!(m["code"], "200");
    assert_eq!(m["msgType"], "FILE-SYN");
    (
        m["data"].as_array().unwrap().clone(),
        m["timestamp"].as_u64().unwrap(),
    )
}

/// The default push mode, `detail`.
fn detail_fixture() -> Fixture {
    let f = Fixture::with(Config::default(), true);
    assert_eq!(f.state.config.push, PushMode::Detail);
    f
}

/// A journal event of the CLI for account `jordi`.
fn cli_event(body: EventBody) -> Event {
    Event::new("jordi", Origin::Cli, None, &new_op_id(), body)
}

fn file_created(id: u64, name: &str) -> EventBody {
    EventBody::FileCreated {
        id,
        path: format!("NOTE/Note/{name}"),
        kind: FileKind::Note,
        md5: "0".repeat(32),
        size: 1,
        pages_changed: None,
    }
}

/// The protocol log lines whose message contains `text`.
fn log_messages(data: &Path, text: &str) -> Vec<String> {
    let mut files: Vec<_> = std::fs::read_dir(data.join("logs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|f| {
            std::fs::read_to_string(f)
                .unwrap()
                .lines()
                .filter_map(|l| {
                    let v: Value = serde_json::from_str(l).unwrap();
                    v["message"].as_str().map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .filter(|m| m.contains(text))
        .collect()
}

#[test]
fn one_put_pushes_each_bound_device_once() {
    let mut f = Fixture::new();
    f.put("a.note");
    assert!(f.follow() > 0);
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 2);
    // Queued, not sent: nothing goes out before the device's next heartbeat.
    assert!(f.outbound(A).is_empty() && f.outbound(B).is_empty());
    assert!(!f.pending(A) && !f.pending(B));
    f.advance_ms(300);
    assert_eq!(f.ping(A), Some(startsync(None, t, t + 300)));
    assert_eq!(f.ping(B), Some(startsync(None, t, t + 300)));
}

#[test]
fn quiet_time_holds_the_push_back() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 1999), 0);
    assert_eq!(f.ping(A), None);
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
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 2);
    assert_eq!(f.ping(A), Some(startsync(None, t, t)));
    assert_eq!(f.ping(B), Some(startsync(None, t, t)));
    f.received(A);
    f.received(B);
    assert_eq!(tick(&f.state, t + 1000), 0);
    f.advance_ms(6000);
    assert_eq!(tick(&f.state, f.now()), 0);
    assert_eq!(f.ping(A), None);
    assert_eq!(f.ping(B), None);
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
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 1);
    assert_eq!(f.ping(A), None);
    assert_eq!(f.ping(B), Some(startsync(Some(A), t, t)));
    assert!(
        f.ping_again(B)
            .contains(&format!(r#"\"equipmentNo\":\"{A}-null\""#))
    );
}

#[test]
fn a_device_change_reaches_the_other_device_as_a_detail_item() {
    let mut f = detail_fixture();
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
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 1);
    assert_eq!(f.ping(A), None);
    let (items, _) = message(&f.ping(B).unwrap());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["messageType"], "DOWNLOADFILE");
    assert_eq!(items[0]["equipmentNo"], format!("{A}-null"));
    assert_eq!(items[0]["originalName"], "x.note");
}

#[test]
fn no_push_while_a_device_holds_the_sync_lock() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    f.state.sync_locks.start("jordi", A, f.now());
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 0);
    assert_eq!(f.ping(B), None);
    assert!(f.pending(B));
    f.state.sync_locks.end("jordi", A, t);
    f.advance_ms(500);
    assert_eq!(tick(&f.state, t + 500), 2);
    assert_eq!(f.ping(B), Some(startsync(None, t + 500, t + 500)));
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
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 1);
    assert_eq!(f.ping(A), Some(startsync(None, t, t)));
    assert_eq!(f.ping(B), None);
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
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 1);
    assert_eq!(f.ping(B), None);
    assert!(f.ping(A).is_some());
}

#[test]
fn closed_protect_gate_holds_the_push_back() {
    let mut f = Fixture::with(Config::default(), false);
    f.put("a.note");
    f.follow();
    let t = f.now() + 2000;
    assert_eq!(tick(&f.state, t), 0);
    assert_eq!(f.ping(A), None);
    assert_eq!(f.ping(B), None);
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
    assert!(f.outbound(A).is_empty() && f.outbound(B).is_empty());
    assert_eq!(f.ping(A), None);
    assert_eq!(f.ping(B), None);
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
    assert!(f.outbound(A).is_empty() && f.outbound(B).is_empty());
    assert_eq!(f.ping(A), None);
    assert_eq!(f.ping(B), None);
}

#[test]
fn detail_mode_sends_one_item_per_change_after_a_ping() {
    let mut f = detail_fixture();
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 2);
    assert!(f.outbound(A).is_empty());
    f.advance_ms(400);
    let (items, sent) = message(&f.ping(A).unwrap());
    assert_eq!(sent, t + 400);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["messageType"], "DOWNLOADFILE");
    assert_eq!(item["fileType"], "FILE");
    assert_eq!(item["equipmentNo"], "null-null");
    assert_eq!(item["originalName"], "a.note");
    assert_eq!(item["newName"], "a.note");
    assert_eq!(item["originalPath"], "NOTE/Note/");
    assert_eq!(item["newPath"], "NOTE/Note/");
    assert_eq!(item["md5"], md5_hex("bytes of a.note"));
    assert_eq!(item["size"], "bytes of a.note".len());
    assert!(item["id"].is_u64());
    assert!(item["directoryId"].is_u64());
    assert!(item.get("newId").is_none() && item.get("goDirectoryId").is_none());
    // The item time is the build time, the message time the send time.
    assert_eq!(item["timestamp"], t);
    assert!(f.ping(B).is_some());
}

#[test]
fn two_changes_before_a_ping_merge_into_one_message() {
    let mut f = detail_fixture();
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    // A second push is handed over after the planner's gap, still before any heartbeat.
    f.advance_ms(MIN_GAP);
    f.put("b.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    assert!(f.outbound(A).is_empty());
    let (items, _) = message(&f.ping(A).unwrap());
    let names: Vec<&str> = items
        .iter()
        .map(|i| i["originalName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a.note", "b.note"]);
    assert_eq!(f.ping(A), None);
}

#[test]
fn an_unacknowledged_push_goes_out_again_after_5_s_until_received() {
    let mut f = detail_fixture();
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    let (first, _) = message(&f.ping(A).unwrap());
    f.advance_ms(1000);
    assert_eq!(f.ping(A), None);
    f.advance_ms(RESEND_MS as i64 - 1000);
    let (again, sent) = message(&f.ping(A).unwrap());
    assert_eq!(again, first);
    assert_eq!(sent, f.now());
    f.received(A);
    for _ in 0..3 {
        f.advance_ms(RESEND_MS as i64);
        assert_eq!(f.ping(A), None);
    }
}

#[test]
fn received_within_5_s_sends_nothing_twice() {
    let mut f = detail_fixture();
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    assert!(f.ping(A).is_some());
    f.advance_ms(200);
    f.received(A);
    f.advance_ms(RESEND_MS as i64);
    assert_eq!(f.ping(A), None);
}

#[test]
fn startsync_pushes_merge_into_one_item() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    f.advance_ms(MIN_GAP);
    f.put("b.note");
    f.follow();
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 2);
    assert_eq!(f.ping(A), Some(startsync(None, t, t)));
}

#[test]
fn a_session_that_ends_drops_its_queue() {
    let mut f = detail_fixture();
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    // A never sends a heartbeat (a half-open socket); the session ends.
    drop(f.rx.remove(A));
    f.state.sockets.unregister("jordi", A, &format!("sid-{A}"));
    f.connect_as(A, "sid-a2");
    assert_eq!(f.ping(A), None);
    // B is replaced by a reconnect before its heartbeat: the new session gets nothing old.
    f.connect_as(B, "sid-b2");
    assert_eq!(f.ping(B), None);
    assert!(!f.pending(A) && !f.pending(B));
}

#[test]
fn a_snapshot_is_no_change() {
    let mut f = Fixture::new();
    f.put("a.note");
    f.follow();
    assert_eq!(tick(&f.state, f.now() + 2000), 2);
    let store = Store::open(f.data(), "jordi").unwrap();
    let g = store.lock_recovered(&f.state.journal).unwrap();
    assert!(!store.snapshot(&g, &f.state.journal).unwrap().is_empty());
    drop(g);
    assert!(f.follow() > 0);
    // Past the quiet time and the 5 s gap, but within the liveness limit of the sessions.
    assert_eq!(tick(&f.state, f.now() + 7000), 0);
    assert!(!f.pending(A) && !f.pending(B));
}

#[test]
fn a_device_that_is_not_connected_gets_the_push_after_it_connects() {
    let mut f = Fixture::new();
    drop(f.rx.remove(B));
    f.state.sockets.unregister("jordi", B, &format!("sid-{B}"));
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    let t = f.now();
    assert_eq!(tick(&f.state, t), 1);
    assert!(f.ping(A).is_some());
    assert!(f.pending(B));
    f.advance_ms(500);
    f.connect(B);
    assert_eq!(tick(&f.state, t + 500), 1);
    assert_eq!(f.ping(B), Some(startsync(None, t + 500, t + 500)));
    assert!(!f.pending(B));
}

#[test]
fn a_silent_session_keeps_the_push_until_a_frame_arrives() {
    let mut f = Fixture::new();
    // Both devices went quiet after the handshake (a sleeping device leaves the socket open).
    f.advance_ms(LIVENESS_MS as i64 + 1000);
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 0);
    assert!(f.outbound(A).is_empty() && f.outbound(B).is_empty());
    assert!(f.pending(A) && f.pending(B));
    // A heartbeat from A: A is live again and gets the push, B stays pending.
    f.heartbeat(A);
    assert_eq!(tick(&f.state, f.now()), 1);
    assert_eq!(f.ping(A), Some(startsync(None, f.now(), f.now())));
    assert!(!f.pending(A) && f.pending(B));
    // B comes back with a new session (same serial) and gets the push there.
    f.advance_ms(500);
    f.connect_as(B, "sid-b2");
    assert_eq!(tick(&f.state, f.now()), 1);
    assert_eq!(f.ping(B), Some(startsync(None, f.now(), f.now())));
    assert!(!f.pending(B));
}

#[test]
fn detail_sends_every_change_collapsed_per_file_in_one_message() {
    let mut f = detail_fixture();
    // 1500 new files, each changed 3 times after; 200 more created and deleted again.
    let mut events: Vec<Event> = Vec::new();
    for i in 0..1500u64 {
        events.push(cli_event(file_created(5000 + i, &format!("n{i}.note"))));
    }
    for round in 1..=3u64 {
        for i in 0..1500u64 {
            events.push(cli_event(EventBody::FileModified {
                id: 5000 + i,
                path: format!("NOTE/Note/n{i}.note"),
                kind: FileKind::Note,
                md5: format!("{round:032}"),
                prev_md5: "0".repeat(32),
                size: round + 1,
                pages_changed: None,
            }));
        }
    }
    for i in 0..200u64 {
        events.push(cli_event(file_created(9000 + i, &format!("gone{i}.note"))));
        events.push(cli_event(EventBody::FileDeleted {
            id: 9000 + i,
            path: format!("NOTE/Note/gone{i}.note"),
            recycle: format!("gone{i}.note"),
        }));
    }
    f.state.journal.append(&mut events).unwrap();
    assert_eq!(f.follow(), 6400);
    {
        let planner = f.state.push.lock().unwrap();
        assert_eq!(planner.pending("jordi", A).unwrap().changes.len(), 1700);
    }
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
    // Every file in one message, in journal order, with its latest content.
    let (items, _) = message(&f.ping(A).unwrap());
    assert_eq!(items.len(), 1500);
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item["messageType"], "DOWNLOADFILE");
        assert_eq!(item["id"], 5000 + i as u64);
        assert_eq!(item["originalName"], format!("n{i}.note"));
        assert_eq!(item["md5"], format!("{:032}", 3));
        assert_eq!(item["size"], 4);
    }
    assert_eq!(message(&f.ping(B).unwrap()).0, items);
}

#[test]
fn a_file_created_and_deleted_again_sends_nothing() {
    let mut f = detail_fixture();
    let mut events = vec![
        cli_event(file_created(77, "x.note")),
        cli_event(EventBody::FileDeleted {
            id: 77,
            path: "NOTE/Note/x.note".into(),
            recycle: "x.note".into(),
        }),
    ];
    f.state.journal.append(&mut events).unwrap();
    f.follow();
    assert!(f.pending(A) && f.pending(B));
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 0);
    assert!(!f.pending(A) && !f.pending(B));
    assert_eq!(f.ping(A), None);
    assert_eq!(f.ping(B), None);
    // The next change goes out without waiting for a push gap.
    f.put("a.note");
    f.follow();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 2);
}

#[test]
fn an_unreadable_id_registry_keeps_the_push_and_tries_again() {
    let mut f = detail_fixture();
    f.put("a.note");
    f.follow();
    // A broken line in the registry's log: the parent folder ids can't be read.
    let log = f.data().join("accounts/jordi/ids.jsonl");
    let good = std::fs::read(&log).unwrap_or_default();
    let mut bad = good.clone();
    bad.extend_from_slice(b"not json\n");
    std::fs::write(&log, &bad).unwrap();
    f.advance_ms(2000);
    assert_eq!(tick(&f.state, f.now()), 0);
    assert!(f.pending(A) && f.pending(B));
    assert_eq!(f.ping(A), None);
    // No new try before about one heartbeat, then a try per heartbeat; one log line per target.
    f.advance_ms(RETRY_MS as i64 - 1);
    assert_eq!(tick(&f.state, f.now()), 0);
    for _ in 0..3 {
        f.advance_ms(RETRY_MS as i64);
        f.heartbeat(A);
        f.heartbeat(B);
        assert_eq!(tick(&f.state, f.now()), 0);
        assert!(f.pending(A) && f.pending(B));
    }
    assert_eq!(log_messages(f.data(), "can't be sent").len(), 2);
    // The registry is good again: the push goes out at the next try, in full.
    std::fs::write(&log, &good).unwrap();
    f.advance_ms(RETRY_MS as i64);
    f.heartbeat(A);
    f.heartbeat(B);
    assert_eq!(tick(&f.state, f.now()), 2);
    let (items, _) = message(&f.ping(A).unwrap());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["messageType"], "DOWNLOADFILE");
    assert!(items[0]["directoryId"].is_u64());
    assert_eq!(log_messages(f.data(), "can be sent again").len(), 2);
    assert_eq!(log_messages(f.data(), "can't be sent").len(), 2);
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
    assert_eq!(f.ping(B), None);
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
    let (mut rx, _inbound) = state.sockets.register("jordi", A, "sid-a", state.now_ms());
    let mut cursor = start_seq(&state).unwrap();
    assert!(cursor > 0);
    assert_eq!(follow_once(&state, &mut cursor).unwrap(), 0);
    assert_eq!(tick(&state, state.now_ms() + 2000), 0);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        state.sockets.deliver("jordi", A, "sid-a", state.now_ms()),
        None
    );
}

// ---- the real loop ----

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The server on `127.0.0.1:0` with `config` and an injected clock that starts at the real time
/// (the push timing moves only when the test moves it); device A bound to an imported account
/// and connected past the handshake.
struct Live {
    dir: tempfile::TempDir,
    state: Arc<AppState>,
    clock: Arc<TestClock>,
    server: tokio::task::JoinHandle<notedown_cloud::Result<()>>,
    ws: Ws,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Live {
    async fn start(config: Config) -> Live {
        let dir = tempfile::tempdir().unwrap();
        seed_account(dir.path(), true);
        let real = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        let clock = Arc::new(TestClock(AtomicI64::new(real as i64)));
        let state = AppState::open_with_clock(config, dir.path(), clock.clone()).unwrap();
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
        Live {
            dir,
            state,
            clock,
            server,
            ws,
        }
    }

    /// `notedown cloud put` of a small note into `NOTE/Note`.
    fn put(&self) {
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("pushed.note");
        std::fs::write(&file, b"pushed").unwrap();
        notedown_cloud::put::put(
            self.dir.path(),
            "jordi",
            &self.state.journal,
            &file,
            "NOTE/Note",
            None,
        )
        .unwrap();
    }

    async fn recv(&mut self) -> String {
        match tokio::time::timeout(Duration::from_secs(5), self.ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    /// Send `frame` (an event with ack id `id`) and read every frame up to its ack `43<id>[]`.
    async fn exchange(&mut self, frame: &str, id: u32) -> Vec<String> {
        self.ws.send(Message::text(frame)).await.unwrap();
        let ack = format!("43{id}[]");
        let mut out = Vec::new();
        loop {
            let t = self.recv().await;
            if t == ack {
                return out;
            }
            out.push(t);
        }
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// `4212["ratta_ping"]` every 300 ms, the clock 1 s further each time, until the answer
    /// carries a push: the frames between the ping and its ack.
    async fn ping_until_push(&mut self) -> Vec<String> {
        for _ in 0..50 {
            self.advance_ms(1000);
            let frames = self.exchange(r#"4212["ratta_ping"]"#, 12).await;
            assert_eq!(frames[0], r#"42["ratta_ping","Received"]"#);
            if frames.len() > 1 {
                return frames;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        panic!("no push within 15 s");
    }
}

#[tokio::test]
async fn the_server_pushes_startsync_to_a_connected_device_after_a_put() {
    let mut live = Live::start(Config {
        push: PushMode::Startsync,
        ..Config::default()
    })
    .await;
    live.put();
    let frames = live.ping_until_push().await;
    assert_eq!(frames.len(), 2, "{frames:?}");
    let (items, _) = message(&frames[1]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["messageType"], "STARTSYNC");
    assert_eq!(items[0]["equipmentNo"], "null-null");
}

/// `detail`: nothing is sent until the device's heartbeat; then the push follows the heartbeat
/// reply and precedes the ack. `Received` ends it.
#[tokio::test]
async fn the_server_sends_a_detail_push_after_the_ratta_ping_reply() {
    let mut live = Live::start(Config {
        push: PushMode::Detail,
        ..Config::default()
    })
    .await;
    live.put();
    // The follower reads the journal within 1 s, the push is due 2 s (clock) later and the
    // tick runs every 500 ms; the device says nothing meanwhile, and nothing arrives.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    live.advance_ms(3000);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let silent = tokio::time::timeout(Duration::from_millis(300), live.ws.next()).await;
    assert!(silent.is_err(), "a frame without a heartbeat: {silent:?}");
    let frames = live.ping_until_push().await;
    assert_eq!(frames.len(), 2, "{frames:?}");
    let (items, _) = message(&frames[1]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["messageType"], "DOWNLOADFILE");
    assert_eq!(items[0]["newName"], "pushed.note");
    // Without `Received`, the device's own heartbeat form (no ack id) 5 s later gets the reply
    // and the same items again.
    live.advance_ms(RESEND_MS as i64);
    live.ws
        .send(Message::text(r#"42["ratta_ping"]"#))
        .await
        .unwrap();
    assert_eq!(live.recv().await, r#"42["ratta_ping","Received"]"#);
    let (again, _) = message(&live.recv().await);
    assert_eq!(again, items);
    // The device acknowledges it; the next heartbeats get their reply only.
    assert!(
        live.exchange(r#"420["ClientMessage","Received"]"#, 0)
            .await
            .is_empty()
    );
    live.advance_ms(RESEND_MS as i64);
    assert_eq!(
        live.exchange(r#"4213["ratta_ping"]"#, 13).await,
        vec![r#"42["ratta_ping","Received"]"#.to_string()]
    );
    live.ws
        .send(Message::text(r#"42["ratta_ping"]"#))
        .await
        .unwrap();
    assert_eq!(live.recv().await, r#"42["ratta_ping","Received"]"#);
}
