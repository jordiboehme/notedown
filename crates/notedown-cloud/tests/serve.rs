//! `serve` on real sockets (127.0.0.1:0 only) and the capture dumps.

use std::net::SocketAddr;
use std::sync::Arc;

use notedown_cloud::Config;
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::server::{AppState, bind, serve_until};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

fn config(capture: bool) -> Config {
    Config {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        capture,
        ..Config::default()
    }
}

async fn start(
    capture: bool,
    dir: &std::path::Path,
) -> (
    SocketAddr,
    Arc<AppState>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<notedown_cloud::Result<()>>,
) {
    let config = config(capture);
    let listeners = bind(&config).await.unwrap();
    let addr = listeners[0].local_addr().unwrap();
    let state = AppState::open(config, dir).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(serve_until(listeners, state.clone(), async {
        let _ = rx.await;
    }));
    (addr, state, tx, handle)
}

#[tokio::test]
async fn serve_answers_file_query_server_and_stops_on_signal() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _state, tx, handle) = start(false, dir.path()).await;
    let reply = http(addr, "GET", "/api/file/query/server", "").await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("\"success\":true"), "{reply}");
    assert!(!dir.path().join("capture").exists());
    tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

fn json_body(reply: &str) -> serde_json::Value {
    let (a, b) = (reply.find('{').unwrap(), reply.rfind('}').unwrap());
    serde_json::from_str(&reply[a..=b]).unwrap()
}

#[tokio::test]
async fn file_endpoints_answer_e0079_through_serve() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _state, tx, handle) = start(false, dir.path()).await;
    for path in [
        "/api/file/2/files/synchronous/start",
        "/api/oss/upload/part",
    ] {
        let reply = http(addr, "POST", path, "{}").await;
        assert_eq!(json_body(&reply)["errorCode"], "E0079", "{path}: {reply}");
    }
    tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn capture_writes_masked_dumps() {
    let dir = tempfile::tempdir().unwrap();
    Accounts::new(dir.path())
        .create("jordi", "a@b.c", "hunter2-very-secret")
        .unwrap();
    let (addr, state, tx, handle) = start(true, dir.path()).await;
    let code_reply = http(
        addr,
        "POST",
        "/api/official/user/query/random/code",
        r#"{"account":"a@b.c"}"#,
    )
    .await;
    let ch = json_body(&code_reply);
    let hash = expected_login_hash(
        &md5_hex("hunter2-very-secret"),
        ch["randomCode"].as_str().unwrap(),
    );
    let body = serde_json::json!({
        "account": "a@b.c", "password": hash, "equipmentNo": "SN1",
        "timestamp": ch["timestamp"]
    })
    .to_string();
    let login = http(
        addr,
        "POST",
        "/api/official/user/account/login/equipment",
        &body,
    )
    .await;
    let login = json_body(&login);
    let token = login["token"].as_str().expect("login succeeds").to_string();
    tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(state.log.capture_failures(), 0);

    let cap = dir.path().join("capture");
    let mut names: Vec<_> = std::fs::read_dir(&cap)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert!(names.len() >= 2, "{names:?}");
    assert!(names[0].ends_with("-000001.json"), "{names:?}");
    let mut saw_login = false;
    for name in &names {
        let text = std::fs::read_to_string(cap.join(name)).unwrap();
        assert!(!text.contains("hunter2"), "{name}: {text}");
        assert!(!text.contains(&hash), "{name}: {text}");
        assert!(!text.contains(&token), "{name}: {text}");
        assert!(text.contains("\n  \"kind\""), "{text}");
        if text.contains("login/equipment") {
            saw_login = true;
            assert!(text.contains("\"token\": \"<masked:"), "{text}");
        }
    }
    assert!(saw_login);
}
