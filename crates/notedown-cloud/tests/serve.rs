//! `serve` on real sockets (127.0.0.1:0 only) and the capture dumps.

use std::net::SocketAddr;
use std::sync::Arc;

use notedown_cloud::Config;
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

#[tokio::test]
async fn capture_writes_masked_dumps() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, state, tx, handle) = start(true, dir.path()).await;
    let body = r#"{"account":"a@b.c","password":"hunter2-very-secret","equipmentNo":"SN1"}"#;
    http(
        addr,
        "POST",
        "/api/official/user/account/login/equipment",
        body,
    )
    .await;
    tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(state.log.capture_failures(), 0);

    let cap = dir.path().join("capture");
    let mut names: Vec<_> = std::fs::read_dir(&cap)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert!(!names.is_empty());
    assert!(names[0].ends_with("-1.json"), "{names:?}");
    let text = std::fs::read_to_string(cap.join(&names[0])).unwrap();
    assert!(!text.contains("hunter2"), "{text}");
    assert!(text.contains("<masked:"), "{text}");
    assert!(text.contains("\n  \"kind\""), "{text}");
}
