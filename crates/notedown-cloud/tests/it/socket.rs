//! The auto-sync socket (Engine.IO 3 / socket.io 2, `docs/sync-protocol.md` 4): handshake order,
//! every heartbeat variant, bad tokens, unknown frames and the protocol log. A real WebSocket
//! client talks to the server on `127.0.0.1:0`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::server::{AppState, serve_listeners};
use notedown_cloud::socket::LIVENESS_MS;
use notedown_cloud::{Clock, Config, SystemClock};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SERIAL: &str = "SN078C10000000";
const EMAIL: &str = "jordi@example.org";
const OPEN_TAIL: &str = r#"","upgrades":[],"pingInterval":5000,"pingTimeout":25000}"#;

struct Server {
    dir: tempfile::TempDir,
    state: Arc<AppState>,
    addrs: Vec<SocketAddr>,
    task: JoinHandle<notedown_cloud::Result<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start() -> Server {
        Self::start_listeners(1).await
    }

    async fn start_listeners(n: usize) -> Server {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let state = AppState::open(Config::default(), dir.path()).unwrap();
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..n {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(l.local_addr().unwrap());
            listeners.push(l);
        }
        let task = tokio::spawn(serve_listeners(listeners, state.clone()));
        Server {
            dir,
            state,
            addrs,
            task,
        }
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    /// A login token for the test device, through the same checks as the login route.
    fn token(&self) -> String {
        let now = u64::try_from(SystemClock.now().unix_timestamp_nanos() / 1_000_000).unwrap();
        let (code, _) = self.state.auth.challenge(EMAIL, now).unwrap();
        let hash = expected_login_hash(&md5_hex("secret"), &code);
        self.state
            .auth
            .login(EMAIL, &hash, SERIAL, now)
            .unwrap()
            .token
    }

    fn url(&self, addr: SocketAddr, token: Option<&str>) -> String {
        let mut q = String::new();
        if let Some(t) = token {
            q.push_str(&format!("token={t}&"));
        }
        format!(
            "ws://{addr}/socket.io/?{q}type={SERIAL}&random=1791270408879&sign=c2lnbmF0dXJlLXZhbHVl&EIO=3&transport=websocket"
        )
    }

    async fn connect_to(&self, addr: SocketAddr, token: Option<&str>) -> Ws {
        let (ws, resp) = tokio_tungstenite::connect_async(self.url(addr, token))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 101);
        ws
    }

    async fn connect(&self, token: Option<&str>) -> Ws {
        self.connect_to(self.addrs[0], token).await
    }

    /// A connection past the handshake (open packet and `40` read).
    async fn session(&self) -> Ws {
        let token = self.token();
        let mut ws = self.connect(Some(&token)).await;
        let open = recv(&mut ws).await;
        assert!(open.starts_with("0{"), "{open}");
        assert_eq!(recv(&mut ws).await, "40");
        ws
    }

    fn log_text(&self) -> String {
        let mut out = String::new();
        let mut files: Vec<_> = std::fs::read_dir(self.data().join("logs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        for f in files {
            out.push_str(&std::fs::read_to_string(f).unwrap());
        }
        out
    }

    fn log(&self) -> Vec<Value> {
        self.log_text()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Frames as (direction, payload, flags).
    fn frames(&self) -> Vec<(String, String, Vec<String>)> {
        self.log()
            .into_iter()
            .filter(|e| e["kind"] == "frame")
            .map(|e| {
                let dir = e["direction"].as_str().unwrap().to_string();
                let key = if dir == "in" {
                    "request_body"
                } else {
                    "response_body"
                };
                let payload = e[key].as_str().unwrap_or_default().to_string();
                let flags = e["flags"]
                    .as_array()
                    .map(|a| a.iter().map(|f| f.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                (dir, payload, flags)
            })
            .collect()
    }

    fn handshakes(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["kind"] == "request" && e["path"] == "/socket.io/")
            .collect()
    }
}

/// The next message of any kind, or `None` when the stream ended.
async fn next(ws: &mut Ws) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("no frame within 5 s")
        .map(|m| m.unwrap())
}

/// The next text frame.
async fn recv(ws: &mut Ws) -> String {
    loop {
        match next(ws).await {
            Some(Message::Text(t)) => return t.to_string(),
            Some(Message::Ping(_) | Message::Pong(_)) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, text: &str) {
    ws.send(Message::text(text.to_string())).await.unwrap();
}

/// Send and expect exactly this answer.
async fn ask(ws: &mut Ws, text: &str, answer: &str) {
    send(ws, text).await;
    assert_eq!(recv(ws).await, answer, "answer to {text}");
}

/// A round trip that proves the connection is open and every earlier frame was handled (and
/// logged) before the answer arrived.
async fn sync(ws: &mut Ws) {
    ask(ws, "2", "3").await;
}

async fn close(mut ws: Ws) {
    let _ = ws.close(None).await;
    while let Ok(Some(Ok(_))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await {}
}

/// After a rejection: the server closes the connection.
async fn expect_closed(ws: &mut Ws) {
    loop {
        match next(ws).await {
            None | Some(Message::Close(_)) => return,
            Some(Message::Ping(_) | Message::Pong(_)) => continue,
            Some(other) => panic!("expected the close, got {other:?}"),
        }
    }
}

// handshake

#[tokio::test]
async fn handshake_sends_open_packet_then_connect() {
    let s = Server::start().await;
    let token = s.token();
    let mut ws = s.connect(Some(&token)).await;
    let open = recv(&mut ws).await;
    let sid = open
        .strip_prefix(r#"0{"sid":""#)
        .and_then(|r| r.strip_suffix(OPEN_TAIL))
        .unwrap_or_else(|| panic!("open packet: {open}"));
    assert!(!sid.is_empty() && !sid.contains('"'), "sid {sid}");
    assert_eq!(recv(&mut ws).await, "40");
    sync(&mut ws).await;
    close(ws).await;

    let frames = s.frames();
    assert_eq!(frames[0].0, "out");
    assert_eq!(frames[0].1, open);
    assert_eq!(frames[1], ("out".into(), "40".into(), vec![]));
}

#[tokio::test]
async fn every_session_gets_its_own_sid() {
    let s = Server::start().await;
    let token = s.token();
    let mut sids = Vec::new();
    for _ in 0..2 {
        let mut ws = s.connect(Some(&token)).await;
        sids.push(recv(&mut ws).await);
        close(ws).await;
    }
    assert_ne!(sids[0], sids[1]);
}

#[tokio::test]
async fn handshake_request_is_logged_once_with_the_token_masked() {
    let s = Server::start().await;
    let token = s.token();
    let ws = s.connect(Some(&token)).await;
    close(ws).await;
    let hs = s.handshakes();
    assert_eq!(hs.len(), 1, "{hs:?}");
    let e = &hs[0];
    assert_eq!(e["method"], "GET");
    assert_eq!(e["response_status"], 101);
    assert_eq!(e["device"], SERIAL);
    let q = e["query"].as_str().unwrap();
    assert!(q.contains("token=<masked:"), "{q}");
    assert!(q.contains("sign=<masked:"), "{q}");
    assert!(q.contains("EIO=3"), "{q}");
    let text = s.log_text();
    assert!(!text.contains(&token), "token in clear in the log");
    assert!(
        !text.contains("c2lnbmF0dXJlLXZhbHVl"),
        "sign in clear in the log"
    );
    assert!(e.get("flags").is_none(), "{e}");
    assert!(
        !e["flags"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f == "unknown_endpoint")),
        "{e}"
    );
}

#[tokio::test]
async fn plain_get_without_upgrade_gets_an_envelope() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let s = Server::start().await;
    let mut conn = TcpStream::connect(s.addrs[0]).await.unwrap();
    conn.write_all(
        b"GET /socket.io/?EIO=3&transport=polling HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut text = String::new();
    conn.read_to_string(&mut text).await.unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    let body: Value = serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(s.handshakes().len(), 1);
}

// heartbeats (docs/sync-protocol.md 4.3)

#[tokio::test]
async fn engine_io_ping_gets_pong() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    ask(&mut ws, "2", "3").await;
    ask(&mut ws, "2probe", "3probe").await;
    close(ws).await;
}

#[tokio::test]
async fn ratta_ping_gets_received() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    let answer = r#"42["ratta_ping","Received"]"#;
    ask(&mut ws, r#"42["ratta_ping"]"#, answer).await;
    ask(&mut ws, r#"42["ratta_ping","{\"ts\":1}"]"#, answer).await;
    // With an ack id and with the default namespace spelled out.
    ask(&mut ws, r#"421["ratta_ping",1791270408879]"#, answer).await;
    ask(&mut ws, r#"42/,["ratta_ping"]"#, answer).await;
    close(ws).await;
    assert!(
        s.frames().iter().all(|f| f.2.is_empty()),
        "{:?}",
        s.frames()
    );
}

#[tokio::test]
async fn client_message_status_gets_true() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    ask(
        &mut ws,
        r#"42["ClientMessage","status"]"#,
        r#"42["ServerMessage","true"]"#,
    )
    .await;
    close(ws).await;
}

#[tokio::test]
async fn client_connect_gets_connect() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    ask(&mut ws, "40", "40").await;
    ask(&mut ws, "40/,", "40").await;
    close(ws).await;
}

#[tokio::test]
async fn client_message_received_gets_no_answer_and_no_flag() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    send(&mut ws, r#"42["ClientMessage","Received"]"#).await;
    sync(&mut ws).await;
    close(ws).await;
    let frames = s.frames();
    let i = frames
        .iter()
        .position(|f| f.1 == r#"42["ClientMessage","Received"]"#)
        .unwrap();
    assert_eq!(frames[i].0, "in");
    assert!(frames[i].2.is_empty());
    // The next frame is the ping of `sync`: nothing was sent in between.
    assert_eq!(frames[i + 1], ("in".into(), "2".into(), vec![]));
}

#[tokio::test]
async fn every_frame_is_logged_in_both_directions_in_order() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    ask(&mut ws, "2", "3").await;
    ask(
        &mut ws,
        r#"42["ratta_ping"]"#,
        r#"42["ratta_ping","Received"]"#,
    )
    .await;
    ask(
        &mut ws,
        r#"42["ClientMessage","status"]"#,
        r#"42["ServerMessage","true"]"#,
    )
    .await;
    close(ws).await;
    let frames: Vec<(String, String)> = s
        .frames()
        .into_iter()
        .skip(2)
        .take(6)
        .map(|f| (f.0, f.1))
        .collect();
    let want = [
        ("in", "2"),
        ("out", "3"),
        ("in", r#"42["ratta_ping"]"#),
        ("out", r#"42["ratta_ping","Received"]"#),
        ("in", r#"42["ClientMessage","status"]"#),
        ("out", r#"42["ServerMessage","true"]"#),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(d, p)| (d.to_string(), p.to_string()))
        .collect();
    assert_eq!(frames, want);
    for e in s.log().iter().filter(|e| e["kind"] == "frame") {
        assert_eq!(e["device"], SERIAL, "{e}");
        assert_eq!(e["path"], "/socket.io/", "{e}");
        assert!(e["ts"].as_str().is_some_and(|t| !t.is_empty()), "{e}");
    }
}

// unknown frames

#[tokio::test]
async fn unknown_frames_are_flagged_and_the_connection_stays_open() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    let unknown = [
        r#"42["mystery",{"a":1}]"#,
        r#"42["ClientMessage","whatever"]"#,
        r#"43[1]"#,
        r#"40/other,"#,
        r#"42/other,["ratta_ping"]"#,
        "0",
        "9",
        "hello",
        "42[broken",
        "",
    ];
    for u in unknown {
        send(&mut ws, u).await;
        sync(&mut ws).await;
    }
    ws.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    sync(&mut ws).await;
    close(ws).await;

    let frames = s.frames();
    for u in unknown {
        let f = frames
            .iter()
            .find(|f| f.0 == "in" && f.1 == u)
            .unwrap_or_else(|| panic!("frame {u:?} not logged: {frames:?}"));
        assert_eq!(f.2, vec!["unknown_event".to_string()], "{u}");
    }
    let binary = frames
        .iter()
        .find(|f| f.0 == "in" && f.1.contains("binary"))
        .unwrap_or_else(|| panic!("binary frame not logged: {frames:?}"));
    assert_eq!(binary.2, vec!["unknown_event".to_string()]);
    // No answer to any of them: every out-frame after the handshake is a pong.
    assert!(
        frames
            .iter()
            .skip(2)
            .filter(|f| f.0 == "out")
            .all(|f| f.1 == "3"),
        "{frames:?}"
    );
}

#[tokio::test]
async fn secrets_in_frames_are_masked() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    send(
        &mut ws,
        r#"42["ClientMessage","{\"token\":\"s3cr3t-value\"}"]"#,
    )
    .await;
    sync(&mut ws).await;
    send(&mut ws, r#"42["ClientMessage","{\"code\":\"777123\"}"]"#).await;
    sync(&mut ws).await;
    close(ws).await;
    let text = s.log_text();
    assert!(!text.contains("s3cr3t-value"));
    assert!(!text.contains("777123"), "a code from the device is masked");
}

// bad tokens

async fn expect_rejected(ws: &mut Ws) {
    let open = recv(ws).await;
    assert!(open.starts_with(r#"0{"sid":""#), "{open}");
    assert_eq!(recv(ws).await, "40");
    let msg = recv(ws).await;
    let packet: Value = serde_json::from_str(msg.strip_prefix("42").unwrap()).unwrap();
    assert_eq!(packet[0], "ServerMessage", "{msg}");
    let payload: Value = serde_json::from_str(packet[1].as_str().unwrap()).unwrap();
    assert_eq!(payload["code"], "403", "{msg}");
    assert!(payload["msg"].as_str().is_some_and(|m| !m.is_empty()));
    // socket.io disconnect, then the WebSocket close.
    assert_eq!(recv(ws).await, "41");
    expect_closed(ws).await;
}

#[tokio::test]
async fn missing_token_is_rejected_with_403_and_closed() {
    let s = Server::start().await;
    let mut ws = s.connect(None).await;
    expect_rejected(&mut ws).await;
}

#[tokio::test]
async fn garbage_token_is_rejected_with_403_and_closed() {
    let s = Server::start().await;
    let mut ws = s.connect(Some("eyJhbGciOiJIUzI1NiJ9.e30.Zm9yZ2Vk")).await;
    expect_rejected(&mut ws).await;
    let out: Vec<_> = s.frames().into_iter().filter(|f| f.0 == "out").collect();
    assert_eq!(out.len(), 5, "{out:?}");
    assert!(out[2].1.starts_with(r#"42["ServerMessage""#), "{out:?}");
    assert_eq!(out[3].1, "41");
    assert!(out[4].1.starts_with("<close"), "{out:?}");
    assert!(!s.log_text().contains("Zm9yZ2Vk"));
    let hs = s.handshakes();
    assert_eq!(hs.len(), 1, "{hs:?}");
    assert_eq!(hs[0]["response_status"], 101);
    // The claimed serial is not trusted: no device, but the flag and the serial in the query.
    assert!(hs[0].get("device").is_none(), "{}", hs[0]);
    assert_eq!(hs[0]["flags"], serde_json::json!(["unauthenticated"]));
    assert!(hs[0]["query"].as_str().unwrap().contains(SERIAL));
    for e in s.log().iter().filter(|e| e["kind"] == "frame") {
        assert!(e.get("device").is_none(), "{e}");
    }
    // A result code from the server stays readable (only device data masks `code`).
    assert!(
        out[2].1.contains(r#"\"code\":\"403\""#),
        "403 masked in the log: {}",
        out[2].1
    );
}

#[tokio::test]
async fn stale_token_is_rejected() {
    let s = Server::start().await;
    let old = s.token();
    let new = s.token();
    let mut ws = s.connect(Some(&old)).await;
    expect_rejected(&mut ws).await;
    let mut ws = s.connect(Some(&new)).await;
    assert!(recv(&mut ws).await.starts_with("0{"));
    assert_eq!(recv(&mut ws).await, "40");
    sync(&mut ws).await;
    close(ws).await;
}

#[tokio::test]
async fn a_token_that_cannot_be_checked_closes_without_the_disconnect_packet() {
    // A read error is not the device's fault: no 403 and no `41` (a socket.io client does not
    // connect again after a server disconnect), only the WebSocket close.
    let s = Server::start().await;
    let token = s.token();
    let device = s
        .data()
        .join(format!("accounts/jordi/devices/{SERIAL}.json"));
    std::fs::remove_file(&device).unwrap();
    std::fs::create_dir(&device).unwrap();
    let mut ws = s.connect(Some(&token)).await;
    assert!(recv(&mut ws).await.starts_with("0{"));
    assert_eq!(recv(&mut ws).await, "40");
    loop {
        match next(&mut ws).await {
            Some(Message::Close(Some(f))) => {
                assert_eq!(u16::from(f.code), 1011);
                break;
            }
            Some(Message::Ping(_) | Message::Pong(_)) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn rejected_session_ignores_frames_sent_after_the_403() {
    let s = Server::start().await;
    let mut ws = s.connect(Some("not-a-token")).await;
    // Sent at once: the server must not answer it, the session is over.
    send(&mut ws, "2").await;
    expect_rejected(&mut ws).await;
    assert!(!s.frames().iter().any(|f| f.0 == "out" && f.1 == "3"));
}

// listeners

#[tokio::test]
async fn socket_works_on_every_listener() {
    let s = Server::start_listeners(2).await;
    let token = s.token();
    for addr in s.addrs.clone() {
        let mut ws = s.connect_to(addr, Some(&token)).await;
        assert!(recv(&mut ws).await.starts_with("0{"));
        assert_eq!(recv(&mut ws).await, "40");
        ask(&mut ws, "2", "3").await;
        close(ws).await;
    }
    assert_eq!(s.handshakes().len(), 2);
}

// limits and broken connections

/// Wait until the log holds an entry for which `f` is true.
async fn wait_for_log(s: &Server, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        if let Some(e) = s.log().into_iter().find(|e| f(e)) {
            return e;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no log entry for {what}: {}", s.log_text());
}

fn ended(e: &Value) -> bool {
    e["kind"] == "frame"
        && e["direction"] == "in"
        && e["request_body"]
            .as_str()
            .is_some_and(|b| b.starts_with("<connection ended"))
}

#[tokio::test]
async fn oversized_frame_ends_the_session() {
    use notedown_cloud::socket::handler::MAX_FRAME_BYTES;
    let s = Server::start().await;
    let mut ws = s.session().await;
    // Just at the limit is fine.
    let at_limit = format!("2{}", "x".repeat(MAX_FRAME_BYTES - 1));
    send(&mut ws, &at_limit).await;
    assert_eq!(recv(&mut ws).await.len(), MAX_FRAME_BYTES);
    let big = format!("2{}", "x".repeat(MAX_FRAME_BYTES));
    let _ = ws.send(Message::text(big)).await;
    // The server ends the session: the stream ends or fails, no answer comes.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("session not ended")
        {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
            Some(Ok(Message::Text(t))) => panic!("answered an oversized frame: {} bytes", t.len()),
            Some(Ok(_)) => continue,
        }
    }
    wait_for_log(&s, "the oversized frame", ended).await;
    // The server is still fine.
    let mut ws = s.session().await;
    sync(&mut ws).await;
    close(ws).await;
}

#[tokio::test]
async fn rejected_session_has_the_same_frame_limit() {
    use notedown_cloud::socket::handler::MAX_FRAME_BYTES;
    let s = Server::start().await;
    let mut ws = s.connect(Some("bad")).await;
    let _ = ws
        .send(Message::text("x".repeat(MAX_FRAME_BYTES + 1)))
        .await;
    // The rejection still arrives and the session ends without a panic.
    expect_rejected(&mut ws).await;
}

#[tokio::test]
async fn abrupt_tcp_drop_ends_the_session() {
    for reset in [false, true] {
        let s = Server::start().await;
        let mut ws = s.session().await;
        sync(&mut ws).await;
        if reset {
            let MaybeTlsStream::Plain(tcp) = ws.get_mut() else {
                panic!("plain TCP expected");
            };
            // RST instead of FIN on drop.
            tcp.set_zero_linger().unwrap();
        }
        // No close handshake: the socket just goes away.
        drop(ws);
        let e = wait_for_log(&s, "the dropped connection", ended).await;
        assert_eq!(e["device"], SERIAL);
        let mut ws = s.session().await;
        sync(&mut ws).await;
        close(ws).await;
    }
}

#[tokio::test]
async fn a_device_that_stops_reading_is_dropped_after_the_send_timeout() {
    use notedown_cloud::socket::handler::SEND_TIMEOUT;
    let s = Server::start().await;
    let mut ws = s.session().await;
    // Pings with a large payload get equally large pongs; the client never reads them, so the
    // server's sends block once the socket buffers are full.
    let ping = format!("2{}", "p".repeat(512 * 1024));
    for _ in 0..256 {
        let sent =
            tokio::time::timeout(Duration::from_secs(1), ws.send(Message::text(ping.clone())))
                .await;
        if !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
    let started = std::time::Instant::now();
    wait_for_log(&s, "the send timeout", |e| {
        e["kind"] == "frame"
            && e["response_body"]
                .as_str()
                .is_some_and(|b| b.starts_with("<send timed out"))
    })
    .await;
    assert!(started.elapsed() <= SEND_TIMEOUT + Duration::from_secs(5));
    drop(ws);
}

// registry and pushes

/// Wait (under 3 s) until the registry says `connected` for the test device.
async fn wait_connected(s: &Server, connected: bool) {
    for _ in 0..60 {
        if s.state
            .sockets
            .is_connected("jordi", SERIAL, s.state.now_ms())
            == connected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("is_connected never became {connected}");
}

#[tokio::test]
async fn a_registered_session_receives_a_pushed_frame_and_logs_it() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    sync(&mut ws).await;
    let frame = notedown_cloud::push::server_message_frame("{\"x\":1}");
    assert!(
        s.state
            .sockets
            .send("jordi", SERIAL, frame.clone(), s.state.now_ms())
    );
    assert_eq!(recv(&mut ws).await, frame);
    sync(&mut ws).await;
    assert!(
        s.frames()
            .iter()
            .any(|(dir, payload, _)| dir == "out" && *payload == frame)
    );
    close(ws).await;
}

#[tokio::test]
async fn a_frame_from_the_device_keeps_the_session_live() {
    let s = Server::start().await;
    let mut ws = s.session().await;
    wait_connected(&s, true).await;
    // Registered at or before `t1`: without a newer frame it is stale at `late`.
    let t1 = s.state.now_ms();
    let late = t1 + LIVENESS_MS + 1;
    tokio::time::sleep(Duration::from_millis(200)).await;
    sync(&mut ws).await;
    assert!(s.state.sockets.is_connected("jordi", SERIAL, late));
    assert!(
        !s.state
            .sockets
            .is_connected("jordi", SERIAL, s.state.now_ms() + LIVENESS_MS + 1)
    );
    close(ws).await;
}

#[tokio::test]
async fn a_second_session_of_the_same_device_replaces_the_first() {
    let s = Server::start().await;
    let mut first = s.session().await;
    wait_connected(&s, true).await;
    let mut second = s.session().await;
    match next(&mut first).await {
        Some(Message::Close(Some(f))) => {
            assert_eq!(u16::from(f.code), 1001);
            assert_eq!(f.reason.as_str(), "replaced");
        }
        other => panic!("expected the close with reason replaced, got {other:?}"),
    }
    expect_closed(&mut first).await;
    let frame = notedown_cloud::push::server_message_frame("{}");
    assert!(
        s.state
            .sockets
            .send("jordi", SERIAL, frame.clone(), s.state.now_ms())
    );
    assert_eq!(recv(&mut second).await, frame);
    close(second).await;
}

#[tokio::test]
async fn the_replaced_sessions_end_keeps_the_new_registration() {
    let s = Server::start().await;
    let first = s.session().await;
    wait_connected(&s, true).await;
    let mut second = s.session().await;
    sync(&mut second).await;
    close(first).await;
    sync(&mut second).await;
    assert!(
        s.state
            .sockets
            .is_connected("jordi", SERIAL, s.state.now_ms())
    );
    close(second).await;
    wait_connected(&s, false).await;
}

#[tokio::test]
async fn a_rejected_session_is_never_registered() {
    let s = Server::start().await;
    let mut ws = s.connect(Some("garbage")).await;
    expect_rejected(&mut ws).await;
    assert!(
        !s.state
            .sockets
            .is_connected("jordi", SERIAL, s.state.now_ms())
    );
    assert!(
        !s.state
            .sockets
            .send("jordi", SERIAL, "x".into(), s.state.now_ms())
    );
}
