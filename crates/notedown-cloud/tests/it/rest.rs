//! The REST read API under `/rest/v1`: on a real socket next to the device routes
//! (`serve_until_with`), with a blocking client on the test thread and the server on a runtime
//! in other threads, and router-level checks with `oneshot`.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedown_cloud::api::envelope::Streamed;
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex, revoke_token};
use notedown_cloud::put;
use notedown_cloud::read;
use notedown_cloud::rest;
use notedown_cloud::server::{AppState, bind, serve_until_with};
use notedown_cloud::{Config, SyncMode};
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};
use tower::ServiceExt;

const EMAIL: &str = "jordi@example.org";
const PASSWORD: &str = "a secret password";

/// A running server with the device routes and the REST routes, and its data folder.
struct Server {
    base: String,
    state: Arc<AppState>,
    data: PathBuf,
    client: Client,
    rt: Option<tokio::runtime::Runtime>,
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

fn start() -> Server {
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
    let config = Config {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        sync: SyncMode::Accept,
        ..Config::default()
    };
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

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.base, rest::PREFIX)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        json_of(
            self.client
                .post(self.url(path))
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .unwrap(),
        )
    }

    fn get(&self, token: Option<&str>, path: &str) -> (u16, Value) {
        json_of(self.request("GET", token, path))
    }

    fn request(&self, method: &str, token: Option<&str>, path: &str) -> Response {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self.client.request(method, self.url(path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().unwrap()
    }

    /// A random code and the login with it: (status, answer, hash sent).
    fn login_with(&self, email: &str, password: &str) -> (u16, Value, String) {
        let (status, code) = self.post("/login/code", &json!({"email": email}));
        assert_eq!(status, 200, "{code}");
        let hash = expected_login_hash(&md5_hex(password), code["randomCode"].as_str().unwrap());
        let (status, answer) = self.post(
            "/login",
            &json!({"email": email, "passwordHash": hash, "name": "mac"}),
        );
        (status, answer, hash)
    }

    fn token(&self) -> String {
        let (status, answer, _) = self.login_with(EMAIL, PASSWORD);
        assert_eq!(status, 201, "{answer}");
        answer["token"].as_str().unwrap().to_string()
    }

    /// Writes a page record file by hand, as the indexer would.
    fn record(&self, key: &str, path: &str) {
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
    fn put(&self, name: &str, bytes: &[u8]) -> String {
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
    fn log_lines(&self) -> Vec<String> {
        log_text(self.state.log.logs_dir())
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn log_text(dir: &Path) -> String {
    let mut text = String::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            text.push_str(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()));
        }
    }
    text
}

fn json_of(r: Response) -> (u16, Value) {
    let status = r.status().as_u16();
    let text = r.text().unwrap();
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

fn q(s: &str) -> String {
    s.replace('/', "%2F").replace(' ', "%20")
}

#[test]
fn login_gives_a_token_that_reads_the_account() {
    let s = start();
    let (status, answer, _) = s.login_with(EMAIL, PASSWORD);
    assert_eq!(status, 201, "{answer}");
    let account = Accounts::new(&s.data).get("jordi").unwrap().unwrap();
    assert_eq!(answer["slug"], "jordi");
    assert_eq!(answer["userId"], account.user_id);
    let id = answer["id"].as_str().unwrap();
    assert!(
        id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    let token = answer["token"].as_str().unwrap();
    assert!(token.starts_with("ndt1."), "{token}");
    let (status, me) = s.get(Some(token), "/account");
    assert_eq!(status, 200, "{me}");
    assert_eq!(me, json!({"slug": "jordi", "userId": account.user_id}));
    // Without a token, or with another scheme, nothing is read.
    let (status, body) = s.get(None, "/account");
    assert_eq!((status, &body["error"]), (401, &json!("token_refused")));
    let r = s
        .client
        .get(s.url("/account"))
        .header("Authorization", format!("Basic {token}"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
}

#[test]
fn a_wrong_password_is_login_failed() {
    let s = start();
    let (status, answer, _) = s.login_with(EMAIL, "wrong");
    assert_eq!((status, answer), (401, json!({"error": "login_failed"})));
    let (status, answer, _) = s.login_with("nobody@example.org", PASSWORD);
    assert_eq!((status, answer), (401, json!({"error": "login_failed"})));
    // A login without a code is no login either.
    let (status, answer) = s.post(
        "/login",
        &json!({"email": EMAIL, "passwordHash": "00", "name": "mac"}),
    );
    assert_eq!((status, answer), (401, json!({"error": "login_failed"})));
    // A body that is not the login body is a bad request.
    let (status, answer) = s.post("/login", &json!({"email": EMAIL}));
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
    assert!(answer["message"].is_string(), "{answer}");
}

#[test]
fn logins_are_rate_limited() {
    let s = start();
    // The login itself: ten attempts a minute from one address.
    for _ in 0..10 {
        let (status, _) = s.post(
            "/login",
            &json!({"email": EMAIL, "passwordHash": "00", "name": "mac"}),
        );
        assert_eq!(status, 401);
    }
    let (status, answer) = s.post(
        "/login",
        &json!({"email": EMAIL, "passwordHash": "00", "name": "mac"}),
    );
    assert_eq!((status, answer), (429, json!({"error": "too_many"})));
    // The codes have the limits of the device codes.
    for _ in 0..10 {
        let (status, _) = s.post("/login/code", &json!({"email": EMAIL}));
        assert_eq!(status, 200);
    }
    let (status, answer) = s.post("/login/code", &json!({"email": EMAIL}));
    assert_eq!((status, answer), (429, json!({"error": "too_many"})));
}

#[test]
fn the_password_never_travels_in_clear() {
    let s = start();
    // The login body holds only the hash: a clear password field is refused, and the clear
    // password in place of the hash is a failed login.
    let (_, code) = s.post("/login/code", &json!({"email": EMAIL}));
    assert!(
        code["randomCode"].is_string() && code["timestamp"].is_u64(),
        "{code}"
    );
    let (status, answer) = s.post(
        "/login",
        &json!({"email": EMAIL, "password": PASSWORD, "name": "mac"}),
    );
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
    let (status, answer) = s.post(
        "/login",
        &json!({"email": EMAIL, "passwordHash": PASSWORD, "name": "mac"}),
    );
    assert_eq!((status, answer), (401, json!({"error": "login_failed"})));
    let (status, _, hash) = s.login_with(EMAIL, PASSWORD);
    assert_eq!(status, 201);
    assert_ne!(hash, md5_hex(PASSWORD));
    assert!(!hash.contains(PASSWORD));
}

#[test]
fn records_and_one_record_match_the_folder_reader() {
    let s = start();
    let token = s.token();
    s.record("F1", "NOTE/Note/a.note");
    s.record("path:0123456789abcdef0123456789abcdef", "NOTE/Note/b.note");
    std::fs::write(s.data.join("accounts/jordi/pages/F2.json"), "{").unwrap();
    let (status, records) = s.get(Some(&token), "/records");
    assert_eq!(status, 200, "{records}");
    let expected = read::records(&s.data, "jordi").unwrap();
    assert_eq!(records, serde_json::to_value(&expected).unwrap());
    let parsed: read::Records = serde_json::from_value(records).unwrap();
    assert!(parsed.complete && parsed.unreadable.contains("F2"));

    for key in ["F1", "path:0123456789abcdef0123456789abcdef"] {
        let (status, one) = s.get(Some(&token), &format!("/records/{key}"));
        assert_eq!(status, 200, "{one}");
        let expected = read::record(&s.data, "jordi", key).unwrap().unwrap();
        assert_eq!(one, serde_json::to_value(&expected).unwrap());
    }
    let (status, body) = s.get(Some(&token), "/records/F9");
    assert_eq!((status, body), (404, json!({"error": "no_record"})));
    let (status, body) = s.get(Some(&token), "/records/not%20a%20key");
    assert_eq!((status, &body["error"]), (400, &json!("bad_request")));

    // The digests too.
    let items = s.data.join("accounts/jordi/digests/items");
    std::fs::create_dir_all(&items).unwrap();
    std::fs::write(
        items.join("7.json"),
        r#"{"id":7,"name":"x","isDeleted":"N"}"#,
    )
    .unwrap();
    let (status, digests) = s.get(Some(&token), "/digests");
    assert_eq!(status, 200, "{digests}");
    assert_eq!(
        digests,
        serde_json::to_value(read::digests(&s.data, "jordi").unwrap()).unwrap()
    );
    assert_eq!(digests.as_array().unwrap().len(), 1);
}

#[test]
fn a_note_download_is_streamed() {
    let s = start();
    let token = s.token();
    // Far larger than the socket buffers.
    let size = 48 * 1024 * 1024;
    let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let path = s.put("big.note", &bytes);
    let file = s.data.join("accounts/jordi/files").join(&path);

    // The router alone: a streamed answer with its length.
    let rt = s.rt.as_ref().unwrap();
    let response = rt
        .block_on(
            rest::router(s.state.clone()).oneshot(
                Request::get(format!("{}/note?path={}", rest::PREFIX, q(&path)))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.extensions().get::<Streamed>().is_some());
    assert_eq!(
        response.headers()["content-length"].to_str().unwrap(),
        size.to_string()
    );
    drop(response);

    // Over the socket: the first bytes come, then the end of the file changes on disk, and
    // the new end arrives: the server had not read it yet.
    let mut r = s.request("GET", Some(&token), &format!("/note?path={}", q(&path)));
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["content-length"].to_str().unwrap(),
        size.to_string()
    );
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "application/octet-stream"
    );
    let mut got = vec![0u8; 64 * 1024];
    r.read_exact(&mut got).unwrap();
    let tail = vec![0xAAu8; 4096];
    let mut f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
    f.seek(SeekFrom::Start((size - tail.len()) as u64)).unwrap();
    f.write_all(&tail).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let mut rest_of = Vec::new();
    r.read_to_end(&mut rest_of).unwrap();
    got.extend(rest_of);
    assert_eq!(got.len(), size);
    assert_eq!(got, std::fs::read(&file).unwrap());
    assert!(got.ends_with(&tail));
}

#[test]
fn note_etag_gives_304() {
    let s = start();
    let token = s.token();
    let path = s.put("a.note", b"note bytes");
    let r = s.request("GET", Some(&token), &format!("/note?path={}", q(&path)));
    assert_eq!(r.status().as_u16(), 200);
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with("W/\"10-"), "{etag}");
    assert_eq!(r.bytes().unwrap().as_ref(), b"note bytes");
    let r = s
        .client
        .get(s.url(&format!("/note?path={}", q(&path))))
        .bearer_auth(&token)
        .header("If-None-Match", &etag)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 304);
    assert_eq!(r.headers()["etag"].to_str().unwrap(), etag);
    assert!(r.bytes().unwrap().is_empty());
    // Another tag gets the bytes.
    let r = s
        .client
        .get(s.url(&format!("/note?path={}", q(&path))))
        .bearer_auth(&token)
        .header("If-None-Match", "W/\"1-1\"")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.bytes().unwrap().as_ref(), b"note bytes");
}

#[test]
fn head_note_tells_missing_and_not_a_file() {
    let s = start();
    let token = s.token();
    let path = s.put("a.note", b"note bytes");
    let head = |p: &str| {
        let r = s.request("HEAD", Some(&token), &format!("/note?path={}", q(p)));
        let status = r.status().as_u16();
        let tell = r.headers()["x-notedown-note"].to_str().unwrap().to_string();
        let len = r
            .headers()
            .get("content-length")
            .map(|v| v.to_str().unwrap().to_string());
        assert!(r.bytes().unwrap().is_empty());
        (status, tell, len)
    };
    assert_eq!(
        head(&path),
        (200, "found".to_string(), Some("10".to_string()))
    );
    let (status, tell, _) = head("NOTE/Note/gone.note");
    assert_eq!((status, tell.as_str()), (404, "missing"));
    let (status, tell, _) = head("NOTE/Note");
    assert_eq!((status, tell.as_str()), (409, "not_a_file"));
    // GET answers with JSON.
    let (status, body) = s.get(Some(&token), "/note?path=NOTE%2FNote%2Fgone.note");
    assert_eq!((status, body), (404, json!({"error": "missing"})));
    let (status, body) = s.get(Some(&token), "/note?path=NOTE%2FNote");
    assert_eq!((status, body), (409, json!({"error": "not_a_file"})));
    // A bad path, or none, is a bad request.
    for bad in ["/note?path=NOTE%2F..%2Fx", "/note"] {
        let (status, body) = s.get(Some(&token), bad);
        assert_eq!(
            (status, &body["error"]),
            (400, &json!("bad_request")),
            "{bad}"
        );
    }
}

#[test]
fn the_journal_matches_the_folder_reader() {
    let s = start();
    let token = s.token();
    s.put("a.note", b"a");
    s.put("b.note", b"b");
    for (from, limit) in [(0u64, 2usize), (1, 10_000), (3, 1), (1_000, 5)] {
        let (status, batch) = s.get(Some(&token), &format!("/journal?from={from}&limit={limit}"));
        assert_eq!(status, 200, "{batch}");
        let expected = read::journal(&s.data, "jordi", from, limit).unwrap();
        assert_eq!(batch, serde_json::to_value(&expected).unwrap());
    }
    // `limit` defaults to and stops at 10000.
    let all = serde_json::to_value(read::journal(&s.data, "jordi", 0, 10_000).unwrap()).unwrap();
    assert_eq!(s.get(Some(&token), "/journal?from=0").1, all);
    assert_eq!(s.get(Some(&token), "/journal?from=0&limit=99999999").1, all);
    let (status, body) = s.get(Some(&token), "/journal?from=x");
    assert_eq!((status, &body["error"]), (400, &json!("bad_request")));
    let (status, body) = s.get(Some(&token), "/journal?from=0&limit=-1");
    assert_eq!((status, &body["error"]), (400, &json!("bad_request")));
}

#[test]
fn a_removed_account_token_sees_only_its_removal() {
    let s = start();
    let token = s.token();
    let old = Accounts::new(&s.data).get("jordi").unwrap().unwrap();
    s.put("a.note", b"a");
    let accounts = s.state.auth.accounts();
    accounts.remove("jordi", &s.state.journal).unwrap();
    let new = accounts.create("jordi", EMAIL, PASSWORD).unwrap();
    assert_ne!(new.user_id, old.user_id);
    s.put("b.note", b"b");

    let removed = read::journal(&s.data, "jordi", 0, 10_000).unwrap();
    let removal = removed
        .removed
        .iter()
        .find(|r| r.user_id == old.user_id)
        .copied()
        .unwrap();
    assert!(removed.end > removal.seq, "{removed:?}");

    let (status, batch) = s.get(Some(&token), "/journal?from=0");
    assert_eq!(status, 200, "{batch}");
    let batch: read::JournalBatch = serde_json::from_value(batch).unwrap();
    assert_eq!(batch.end, removal.seq);
    assert!(batch.next <= removal.seq + 1);
    assert_eq!(batch.removed, vec![removal]);
    // A poll past the removal stays there.
    let (status, batch) = s.get(Some(&token), &format!("/journal?from={}", removal.seq + 1));
    assert_eq!(status, 200);
    assert_eq!(batch["end"], removal.seq);
    assert_eq!(batch["removed"], json!([]));

    let gone = json!({"error": "account_removed", "userId": old.user_id, "seq": removal.seq});
    for path in [
        "/account",
        "/records",
        "/records/F1",
        "/digests",
        "/note?path=NOTE%2FNote%2Fb.note",
    ] {
        let (status, body) = s.get(Some(&token), path);
        assert_eq!((status, &body), (410, &gone), "{path}");
    }
    // The new account's token reads the new account.
    let fresh = s.token();
    let (status, me) = s.get(Some(&fresh), "/account");
    assert_eq!(
        (status, me),
        (200, json!({"slug": "jordi", "userId": new.user_id}))
    );
}

#[test]
fn a_revoked_token_is_refused() {
    let s = start();
    let (_, answer, _) = s.login_with(EMAIL, PASSWORD);
    let token = answer["token"].as_str().unwrap();
    assert_eq!(s.get(Some(token), "/records").0, 200);
    assert!(revoke_token(&s.data, "jordi", answer["id"].as_str().unwrap()).unwrap());
    for path in ["/account", "/records", "/journal?from=0", "/digests"] {
        let (status, body) = s.get(Some(token), path);
        assert_eq!(
            (status, body),
            (401, json!({"error": "token_refused"})),
            "{path}"
        );
    }
    let r = s.request("GET", Some(token), "/account");
    assert!(
        r.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Bearer")
    );
    // A token that was never issued.
    let (status, _) = s.get(Some("ndt1.e30.AAAA"), "/account");
    assert_eq!(status, 401);
}

#[test]
fn an_unknown_rest_path_is_404_json() {
    let s = start();
    let token = s.token();
    for (method, path) in [
        ("GET", "/nothing"),
        ("GET", "/records/F1/more"),
        ("POST", "/records"),
        ("GET", "/login"),
        ("DELETE", "/account"),
        ("GET", "/"),
        ("GET", ""),
    ] {
        let (status, body) = json_of(s.request(method, Some(&token), path));
        assert_eq!(
            (status, body),
            (404, json!({"error": "no_route"})),
            "{method} {path}"
        );
    }
    // The device routes still answer next to them.
    let r = s
        .client
        .get(format!("{}/api/file/query/server", s.base))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let r = s
        .client
        .post(format!("{}/some/unknown/device/path", s.base))
        .body("{}")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

#[test]
fn the_request_log_never_holds_a_token() {
    let s = start();
    let (status, answer, hash) = s.login_with(EMAIL, PASSWORD);
    assert_eq!(status, 201);
    let token = answer["token"].as_str().unwrap().to_string();
    // Reads, logged and not, and refusals that are logged with their header.
    s.put("a.note", b"a");
    assert_eq!(s.get(Some(&token), "/records").0, 200);
    assert_eq!(s.get(Some(&token), "/records/F9").0, 404);
    assert_eq!(
        s.get(Some(&token), "/note?path=NOTE%2FNote%2Fa.note").0,
        200
    );
    assert_eq!(s.get(Some(&token), "/journal?from=x").0, 400);
    revoke_token(&s.data, "jordi", answer["id"].as_str().unwrap()).unwrap();
    assert_eq!(s.get(Some(&token), "/account").0, 401);
    let text = log_text(s.state.log.logs_dir());
    assert!(text.contains("/rest/v1/login"), "the login is logged");
    assert!(text.contains("/rest/v1/records/F9"), "a 404 is logged");
    assert!(text.contains("/rest/v1/account"), "a 401 is logged");
    let (_, mac) = token.rsplit_once('.').unwrap();
    for secret in [token.as_str(), mac, PASSWORD, &hash, &md5_hex(PASSWORD)] {
        assert!(!text.contains(secret), "the log holds {secret:?}");
    }
}

#[test]
fn quiet_polls_are_not_logged() {
    let s = start();
    let (_, answer, _) = s.login_with(EMAIL, PASSWORD);
    let token = answer["token"].as_str().unwrap().to_string();
    let path = s.put("a.note", b"a");
    let before = s.log_lines().len();
    for _ in 0..10 {
        assert_eq!(s.get(Some(&token), "/journal?from=1&limit=100").0, 200);
    }
    assert_eq!(s.get(Some(&token), "/records").0, 200);
    let r = s.request("HEAD", Some(&token), &format!("/note?path={}", q(&path)));
    assert_eq!(r.status().as_u16(), 200);
    let r = s.request("GET", Some(&token), &format!("/note?path={}", q(&path)));
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        s.log_lines().len(),
        before,
        "200 answers to reads add no line"
    );

    revoke_token(&s.data, "jordi", answer["id"].as_str().unwrap()).unwrap();
    for from in 0..10 {
        let (status, _) = s.get(Some(&token), &format!("/journal?from={from}"));
        assert_eq!(status, 401);
    }
    assert_eq!(
        s.log_lines().len(),
        before + 1,
        "ten refused polls add one line"
    );
    // Another path is another line; a bad request is always logged.
    assert_eq!(s.get(Some(&token), "/records").0, 401);
    assert_eq!(s.log_lines().len(), before + 2);
    let (_, fresh, _) = s.login_with(EMAIL, PASSWORD);
    let fresh = fresh["token"].as_str().unwrap().to_string();
    let lines = s.log_lines().len();
    assert_eq!(s.get(Some(&fresh), "/journal?from=x").0, 400);
    assert_eq!(s.get(Some(&fresh), "/journal?from=x").0, 400);
    assert_eq!(s.log_lines().len(), lines + 2);
}
