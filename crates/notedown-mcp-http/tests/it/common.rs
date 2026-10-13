//! The real `serve` router on 127.0.0.1:0, with base `https://notedown.example` behind a
//! trusted proxy (the test client sends `X-Forwarded-Host`).

use std::net::SocketAddr;
use std::sync::Arc;

use notedown_cloud::server::{AppState, bind, serve_until_with};
use notedown_cloud::{Config, SyncMode};

pub const BASE: &str = "https://notedown.example";
pub const HOST: &str = "notedown.example";

#[allow(dead_code)] // `dir` and `state` are used by the tasks that follow
pub struct Server {
    pub addr: SocketAddr,
    pub dir: tempfile::TempDir,
    pub state: Arc<AppState>,
    pub client: reqwest::Client,
    shutdown: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<notedown_cloud::Result<()>>,
}

pub async fn start() -> Server {
    start_with(|_| {}).await
}

pub async fn start_with(edit: impl FnOnce(&mut Config)) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        external_base_url: Some(BASE.to_string()),
        trusted_proxies: vec!["127.0.0.1".parse().unwrap()],
        sync: SyncMode::Refuse,
        watch: false,
        scan_interval: 0,
        render_dir: Some(String::new()),
        ..Config::default()
    };
    edit(&mut config);
    let listeners = bind(&config).await.unwrap();
    let addr = listeners[0].local_addr().unwrap();
    let state = AppState::open(config, dir.path()).unwrap();
    let extra = notedown_mcp_http::router(&state)
        .unwrap()
        .unwrap_or_default();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(serve_until_with(listeners, state.clone(), extra, async {
        let _ = rx.await;
    }));
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-forwarded-host", HOST.parse().unwrap());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(headers)
        .build()
        .unwrap();
    Server {
        addr,
        dir,
        state,
        client,
        shutdown: tx,
        handle,
    }
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        self.handle.await.unwrap().unwrap();
    }
}
