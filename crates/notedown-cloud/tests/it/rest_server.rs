//! A real server for the REST tests: the device routes and the REST routes on a socket
//! (`serve_until_with`), the server on a runtime in other threads, a blocking client for the
//! test thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
use notedown_cloud::put;
use notedown_cloud::rest;
use notedown_cloud::server::{AppState, bind, serve_until_with};
use notedown_cloud::{Config, SyncMode};
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};

pub(crate) const EMAIL: &str = "jordi@example.org";
pub(crate) const PASSWORD: &str = "a secret password";

/// A running server with the device routes and the REST routes, and its data folder.
pub(crate) struct Server {
    pub(crate) base: String,
    pub(crate) state: Arc<AppState>,
    pub(crate) data: PathBuf,
    pub(crate) client: Client,
    pub(crate) rt: Option<tokio::runtime::Runtime>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(5));
        }
    }
}

pub(crate) fn start() -> Server {
    start_with(|_| {})
}

/// [`start`] with `edit` applied to the server's config first.
pub(crate) fn start_with(edit: impl FnOnce(&mut Config)) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    Accounts::new(&data)
        .create("jordi", EMAIL, PASSWORD)
        .unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut config = Config {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        sync: SyncMode::Accept,
        ..Config::default()
    };
    edit(&mut config);
    let listeners = rt.block_on(bind(&config)).unwrap();
    let addr = listeners[0].local_addr().unwrap();
    let state = AppState::open(config, &data).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    rt.spawn(serve_until_with(
        listeners,
        state.clone(),
        rest::router(state.clone()),
        async {
            let _ = rx.await;
        },
    ));
    Server {
        base: format!("http://{addr}"),
        state,
        data,
        client: Client::new(),
        rt: Some(rt),
        stop: Some(tx),
        _dir: dir,
    }
}

/// [`start`] with the account imported: the protect gate is open and the native folders are
/// there (ids without events).
pub(crate) fn start_imported() -> Server {
    start_imported_with(|_| {})
}

/// [`start_imported`] with `edit` applied to the server's config first.
pub(crate) fn start_imported_with(edit: impl FnOnce(&mut Config)) -> Server {
    let s = start_with(edit);
    let store = s.state.store("jordi").unwrap();
    let g = store.lock().unwrap();
    store.ensure_native_folders(&g).unwrap();
    drop(g);
    let accounts = Accounts::new(&s.data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.imported = Some(Imported {
        at: "2026-11-16T10:00:00Z".into(),
        source: "/backup/Supernote".into(),
        files: 0,
        bytes: 0,
    });
    accounts.save(&a).unwrap();
    s
}

impl Server {
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.base, rest::PREFIX)
    }

    pub(crate) fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        json_of(
            self.client
                .post(self.url(path))
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .unwrap(),
        )
    }

    pub(crate) fn get(&self, token: Option<&str>, path: &str) -> (u16, Value) {
        json_of(self.request("GET", token, path))
    }

    pub(crate) fn request(&self, method: &str, token: Option<&str>, path: &str) -> Response {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self.client.request(method, self.url(path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().unwrap()
    }

    /// A random code and the login with it: (status, answer, hash sent).
    pub(crate) fn login_with(&self, email: &str, password: &str) -> (u16, Value, String) {
        let (status, code) = self.post("/login/code", &json!({"email": email}));
        assert_eq!(status, 200, "{code}");
        let hash = expected_login_hash(&md5_hex(password), code["randomCode"].as_str().unwrap());
        let (status, answer) = self.post(
            "/login",
            &json!({"email": email, "passwordHash": hash, "name": "mac"}),
        );
        (status, answer, hash)
    }

    /// A login of the test account with `extra` fields in the body (such as
    /// `{"scope": "write"}`): (status, answer).
    pub(crate) fn login_extra(&self, extra: Value) -> (u16, Value) {
        let (status, code) = self.post("/login/code", &json!({"email": EMAIL}));
        assert_eq!(status, 200, "{code}");
        let hash = expected_login_hash(&md5_hex(PASSWORD), code["randomCode"].as_str().unwrap());
        let mut body = json!({"email": EMAIL, "passwordHash": hash, "name": "mac"});
        if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
            body.extend(extra);
        }
        self.post("/login", &body)
    }

    pub(crate) fn token(&self) -> String {
        let (status, answer, _) = self.login_with(EMAIL, PASSWORD);
        assert_eq!(status, 201, "{answer}");
        answer["token"].as_str().unwrap().to_string()
    }

    /// Writes a page record file by hand, as the indexer would.
    pub(crate) fn record(&self, key: &str, path: &str) {
        let dir = self.data.join("accounts/jordi/pages");
        std::fs::create_dir_all(&dir).unwrap();
        let name = match key.strip_prefix("path:") {
            Some(md5) => format!("path-{md5}.json"),
            None => format!("{key}.json"),
        };
        let record = json!({"fileId": key, "path": path, "size": 3, "md5": "x", "pages": []});
        std::fs::write(dir.join(name), record.to_string()).unwrap();
    }

    /// Puts `bytes` as `NOTE/Note/<name>` the way `notedown cloud put` does.
    pub(crate) fn put(&self, name: &str, bytes: &[u8]) -> String {
        let src = self.data.parent().unwrap().join(name);
        std::fs::write(&src, bytes).unwrap();
        put::put(
            &self.data,
            "jordi",
            &self.state.journal,
            &src,
            "NOTE/Note",
            None,
        )
        .unwrap();
        format!("NOTE/Note/{name}")
    }

    /// The lines of every protocol log file.
    pub(crate) fn log_lines(&self) -> Vec<String> {
        log_text(self.state.log.logs_dir())
            .lines()
            .map(str::to_string)
            .collect()
    }
}

pub(crate) fn log_text(dir: &Path) -> String {
    let mut text = String::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            text.push_str(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()));
        }
    }
    text
}

pub(crate) fn json_of(r: Response) -> (u16, Value) {
    let status = r.status().as_u16();
    let text = r.text().unwrap();
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

pub(crate) fn q(s: &str) -> String {
    s.replace('/', "%2F").replace(' ', "%20")
}
