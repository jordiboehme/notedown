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

pub const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

pub async fn register(s: &Server, redirect: &str) -> String {
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(&serde_json::json!({
            "client_name": "Claude",
            "client_uri": "https://claude.ai",
            "redirect_uris": [redirect],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.unwrap();
    v["client_id"].as_str().unwrap().to_string()
}

pub const VERIFIER: &str = "dBjftJeZ4CVP-mJ92K28uhbUJU1p1r_wW1gFWFOEjXk";
pub const CHALLENGE: &str = "jDBroimTx0Ag-gB-3YHpo5TS8niR-0pvTfRtAXTl1p8";

pub fn add_account(s: &Server, slug: &str, email: &str, password: &str) {
    notedown_cloud::auth::Accounts::new(s.dir.path())
        .create(slug, email, password)
        .unwrap();
}

pub fn authorize_query(client_id: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut q: Vec<(String, String)> = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", REDIRECT),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("state", "st-1"),
        ("resource", "https://notedown.example/mcp"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        q.retain(|(qk, _)| qk != k);
        if !v.is_empty() {
            q.push((k.to_string(), v.to_string()));
        }
    }
    q
}

pub fn pending_id_of(html: &str) -> String {
    let at = html
        .find("name=\"pending\" value=\"")
        .expect("a login form")
        + 22;
    html[at..].split('"').next().unwrap().to_string()
}

/// Query parameters of a `Location` header.
pub fn location_params(r: &reqwest::Response) -> std::collections::HashMap<String, String> {
    let loc = r.headers()["location"].to_str().unwrap();
    let query = loc.split_once('?').map_or("", |(_, q)| q);
    reqwest::Url::parse(&format!("http://x/?{query}"))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Opens the login page and signs in; returns the code.
pub async fn sign_in(s: &Server, client_id: &str, email: &str, password: &str) -> String {
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(client_id, &[]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let pending = pending_id_of(&r.text().await.unwrap());
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&[
            ("pending", pending.as_str()),
            ("email", email),
            ("password", password),
            ("action", "login"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302, "sign-in redirects");
    let p = location_params(&r);
    assert!(
        r.headers()["location"]
            .to_str()
            .unwrap()
            .starts_with(REDIRECT)
    );
    assert_eq!(p["state"], "st-1");
    assert_eq!(p["iss"], BASE);
    p["code"].clone()
}

pub async fn exchange(s: &Server, client_id: &str, code: &str) -> serde_json::Value {
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("client_id", client_id),
            ("code_verifier", VERIFIER),
            ("resource", "https://notedown.example/mcp"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    r.json().await.unwrap()
}

pub async fn refresh(s: &Server, client_id: &str, token: &str) -> reqwest::Response {
    s.client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .unwrap()
}
