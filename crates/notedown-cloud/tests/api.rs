//! HTTP routes of the capture server: login, server queries, safe refusal of every sync, the
//! catch-all and the request log. Requests go through the router in memory (`tower::oneshot`).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::server::{AppState, MAX_REQUEST_BODY, router};
use notedown_cloud::{Clock, Config, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const SERIAL: &str = "SN078C10000000";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(self.0.load(Ordering::SeqCst) as i128 * 1_000_000)
            .unwrap()
    }
}

struct Server {
    dir: tempfile::TempDir,
    state: Arc<AppState>,
    clock: Arc<TestClock>,
}

impl Server {
    /// Refuse mode: the M2 behaviour, every file and oss path answers `E0079`.
    fn new() -> Server {
        Self::with_config(Config {
            sync: SyncMode::Refuse,
            ..Config::default()
        })
    }

    /// Accept mode (the default configuration).
    fn accept() -> Server {
        Self::with_config(Config::default())
    }

    fn with_config(config: Config) -> Server {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let clock = Arc::new(TestClock(AtomicI64::new(T0)));
        let state = AppState::open_with_clock(config, dir.path(), clock.clone()).unwrap();
        Server { dir, state, clock }
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    fn app(&self, peer: &str) -> Router {
        let addr: SocketAddr = peer.parse().unwrap();
        router(self.state.clone()).layer(MockConnectInfo(addr))
    }

    async fn raw(
        &self,
        peer: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = self
            .app(peer)
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&bytes)));
        (status, value)
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Value {
        self.call_from("192.168.1.50:40000", method, path, headers, body)
            .await
    }

    async fn call_from(
        &self,
        peer: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Value {
        let mut h: Vec<(&str, &str)> = headers.to_vec();
        let body = match body {
            Some(v) => {
                h.push(("content-type", "application/json; charset=UTF-8"));
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let (status, value) = self.raw(peer, method, path, &h, body).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {value}");
        value
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

    fn requests_to(&self, path: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["kind"] == "request" && e["path"] == path)
            .collect()
    }

    /// random/code then login/equipment; returns the login response and the password hash sent.
    async fn login(&self, password: &str, serial: &str, version: &str) -> (Value, String, String) {
        let ch = self
            .call(
                "POST",
                "/api/official/user/query/random/code",
                &[],
                Some(json!({"countryCode": null, "account": EMAIL, "version": version})),
            )
            .await;
        assert_eq!(ch["success"], true, "{ch}");
        let code = ch["randomCode"].as_str().unwrap().to_string();
        let hash = expected_login_hash(&md5_hex(password), &code);
        let resp = self
            .call(
                "POST",
                "/api/official/user/account/login/equipment",
                &[],
                Some(json!({
                    "account": EMAIL, "password": hash, "equipmentNo": serial, "equipment": 3,
                    "loginMethod": "2", "timestamp": ch["timestamp"], "version": version
                })),
            )
            .await;
        (resp, hash, code)
    }

    async fn bind(&self, serial: &str, version: &str) -> Value {
        self.call(
            "POST",
            "/api/terminal/user/bindEquipment",
            &[],
            Some(json!({
                "account": EMAIL, "equipmentNo": serial, "flag": "1", "name": "Supernote Nomad",
                "totalCapacity": "25485312", "version": version,
                "label": ["DOCUMENT/Document", "NOTE/Note", "NOTE/MyStyle", "EXPORT", "SCREENSHOT", "INBOX"]
            })),
        )
        .await
    }
}

fn assert_ok(v: &Value) {
    assert_eq!(v["success"], true, "{v}");
    assert!(v["errorCode"].is_null(), "{v}");
    assert!(v["errorMsg"].is_null(), "{v}");
    assert!(v.get("data").is_none(), "payload never under data: {v}");
}

fn assert_err(v: &Value, code: &str) {
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(v["errorCode"], code, "{v}");
    assert!(v["errorMsg"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
}

// login sequences (docs/sync-protocol.md 2.4)

async fn check_exists(s: &Server) -> Value {
    s.call(
        "POST",
        "/api/official/user/check/exists/server",
        &[],
        Some(json!({"email": EMAIL, "version": "202407"})),
    )
    .await
}

async fn finish_sequence(s: &Server) -> String {
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    assert_ok(&login);
    let token = login["token"].as_str().unwrap().to_string();
    assert_eq!(login["counts"], "0");
    assert_eq!(login["isBind"], "N");
    assert_eq!(login["isBindEquipment"], "N");
    assert_eq!(login["soldOutCount"], 0);
    assert_eq!(login["userName"], EMAIL);

    assert_ok(&s.bind(SERIAL, "202407").await);

    let user = s
        .call(
            "POST",
            "/api/user/query",
            &[("x-access-token", &token)],
            None,
        )
        .await;
    assert_ok(&user);
    assert_eq!(user["email"], EMAIL);
    assert_eq!(user["userName"], EMAIL);
    assert!(user["userId"].as_u64().unwrap() >= 1_000_001);
    assert_eq!(user["fileServer"], "0");
    assert_eq!(user["isNormal"], "Y");
    assert!(user["totalCapacity"].is_string());
    for k in [
        "telephone",
        "countryCode",
        "avatarsUrl",
        "sex",
        "birthday",
        "personalSign",
        "hobby",
        "education",
        "job",
        "address",
    ] {
        assert!(user.get(k).is_some(), "missing {k}");
    }
    token
}

#[tokio::test]
async fn variant_a_login_sequence() {
    let s = Server::new();
    let unlink = s
        .call(
            "POST",
            "/api/terminal/equipment/unlink",
            &[],
            Some(json!({"equipmentNo": SERIAL, "version": "202407"})),
        )
        .await;
    assert_ok(&unlink);
    let exists = check_exists(&s).await;
    assert_ok(&exists);
    assert_eq!(exists["dms"], "ALL");
    assert!(exists["userId"].as_u64().is_some());
    let machine = exists["uniqueMachineId"].as_str().unwrap().to_string();
    assert!(!machine.is_empty());
    finish_sequence(&s).await;

    // the device record carries the binding and the firmware; nothing under files/
    let text = std::fs::read_to_string(
        s.data()
            .join(format!("accounts/jordi/devices/{SERIAL}.json")),
    )
    .unwrap();
    assert!(
        text.starts_with(&format!("{{\n  \"serial\": \"{SERIAL}\"")),
        "{text}"
    );
    assert!(text.ends_with("}\n"));
    let dev: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(dev["bound"], true);
    assert_eq!(dev["name"], "Supernote Nomad");
    assert_eq!(dev["firmware"], "202407");
    assert!(!s.data().join("accounts/jordi/files").exists());
    // the binding is journaled; the unlink of a device that was not bound is not
    let types: Vec<String> = journal_lines(s.data())
        .iter()
        .map(|e| format!("{} {} {}", e["type"], e["device"], e["origin"]))
        .collect();
    assert_eq!(
        types,
        vec![format!("\"device.bound\" \"{SERIAL}\" \"device\"")]
    );

    // a second login now sees the binding; the machine id is stable across a restart
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    assert_eq!(login["isBind"], "Y");
    assert_eq!(login["isBindEquipment"], "Y");
    let again = AppState::open(Config::default(), s.data()).unwrap();
    let s2 = Server {
        dir: tempfile::tempdir().unwrap(),
        state: again,
        clock: s.clock.clone(),
    };
    let exists = check_exists(&s2).await;
    assert_eq!(exists["uniqueMachineId"], machine.as_str());
}

#[tokio::test]
async fn variant_b_login_sequence() {
    let s = Server::new();
    assert_ok(&check_exists(&s).await);
    let token = finish_sequence(&s).await;
    let status = s
        .call(
            "POST",
            "/api/equipment/bind/status",
            &[("x-access-token", &token)],
            Some(json!({})),
        )
        .await;
    assert_ok(&status);
    assert_eq!(status["bindStatus"], true);
}

#[tokio::test]
async fn check_exists_empty_body_and_unknown_email() {
    let s = Server::new();
    let v = s
        .call(
            "POST",
            "/api/official/user/check/exists/server",
            &[],
            Some(json!({})),
        )
        .await;
    assert_ok(&v);
    assert_eq!(v["dms"], "ALL");
    let v = s
        .call("POST", "/api/official/user/check/exists/server", &[], None)
        .await;
    assert_ok(&v);
    let v = s
        .call(
            "POST",
            "/api/official/user/check/exists/server",
            &[],
            Some(json!({"email": "ghost@example.org"})),
        )
        .await;
    assert_err(&v, "E0018");
}

#[tokio::test]
async fn wrong_password_unknown_account_and_reused_code() {
    let s = Server::new();
    let (v, _, _) = s.login("wrong", SERIAL, "202407").await;
    assert_err(&v, "E0019");
    assert!(v.get("token").is_none());

    let ch = s
        .call(
            "POST",
            "/api/official/user/query/random/code",
            &[],
            Some(json!({"account": "ghost@example.org"})),
        )
        .await;
    assert_ok(&ch);
    let hash = expected_login_hash(&md5_hex("secret"), ch["randomCode"].as_str().unwrap());
    let v = s
        .call(
            "POST",
            "/api/official/user/account/login/equipment",
            &[],
            Some(json!({"account": "ghost@example.org", "password": hash, "equipmentNo": SERIAL})),
        )
        .await;
    assert_err(&v, "E0018");

    // reuse the code of a successful login: single use
    let (ok, hash, _) = s.login("secret", SERIAL, "202407").await;
    assert_ok(&ok);
    let v = s
        .call(
            "POST",
            "/api/official/user/account/login/equipment",
            &[],
            Some(json!({"account": EMAIL, "password": hash, "equipmentNo": SERIAL})),
        )
        .await;
    assert_err(&v, "E0019");
}

#[tokio::test]
async fn malformed_body_gets_an_envelope() {
    let s = Server::new();
    let (status, v) = s
        .raw(
            "127.0.0.1:1",
            "POST",
            "/api/official/user/account/login/equipment",
            &[("content-type", "application/json")],
            Body::from("{not json"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_err(&v, "422");
}

// tokens

#[tokio::test]
async fn unauthenticated_calls_get_e0712() {
    let s = Server::new();
    for path in [
        "/api/user/query",
        "/api/equipment/bind/status",
        "/api/user/logout",
        "/api/user/query/token",
    ] {
        let v = s.call("POST", path, &[], Some(json!({}))).await;
        assert_err(&v, "E0712");
        let v = s
            .call(
                "POST",
                path,
                &[("x-access-token", "eyJhbGciOiJIUzI1NiJ9.e30.garbage")],
                None,
            )
            .await;
        assert_err(&v, "E0712");
    }
}

#[tokio::test]
async fn query_token_and_logout() {
    let s = Server::new();
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    let token = login["token"].as_str().unwrap();
    let v = s
        .call(
            "POST",
            "/api/user/query/token",
            &[("x-access-token", token)],
            None,
        )
        .await;
    assert_ok(&v);
    assert_eq!(v["token"], token);
    let v = s
        .call(
            "POST",
            "/api/user/logout",
            &[("x-access-token", token)],
            None,
        )
        .await;
    assert_ok(&v);
    let v = s
        .call(
            "POST",
            "/api/user/query",
            &[("x-access-token", token)],
            None,
        )
        .await;
    assert_err(&v, "E0712");
}

#[tokio::test]
async fn unlink_invalidates_the_device_token() {
    let s = Server::new();
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    let token = login["token"].as_str().unwrap();
    assert_ok(&s.bind(SERIAL, "202407").await);
    let v = s
        .call(
            "POST",
            "/api/terminal/equipment/unlink",
            &[],
            Some(json!({"equipmentNo": SERIAL, "version": "202407"})),
        )
        .await;
    assert_ok(&v);
    let v = s
        .call(
            "POST",
            "/api/user/query",
            &[("x-access-token", token)],
            None,
        )
        .await;
    assert_err(&v, "E0712");
    let dev: Value = serde_json::from_str(
        &std::fs::read_to_string(
            s.data()
                .join(format!("accounts/jordi/devices/{SERIAL}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(dev["bound"], false);
}

#[tokio::test]
async fn bind_rules() {
    let s = Server::new();
    // no login of this device before: refused, nothing written
    assert_err(&s.bind(SERIAL, "202407").await, "E0712");
    assert!(!s.data().join("accounts/jordi/devices").exists());
    // unknown account
    let v = s
        .call(
            "POST",
            "/api/terminal/user/bindEquipment",
            &[],
            Some(json!({"account": "ghost@example.org", "equipmentNo": SERIAL})),
        )
        .await;
    assert_err(&v, "E0018");
    // bound to another account
    Accounts::new(s.data())
        .create("anna", "anna@example.org", "pw")
        .unwrap();
    s.login("secret", SERIAL, "202407").await;
    assert_ok(&s.bind(SERIAL, "202407").await);
    assert_ok(&s.bind(SERIAL, "202407").await); // repeated bind to the same account is fine
    let ch = s
        .call(
            "POST",
            "/api/official/user/query/random/code",
            &[],
            Some(json!({"account": "anna@example.org"})),
        )
        .await;
    let hash = expected_login_hash(&md5_hex("pw"), ch["randomCode"].as_str().unwrap());
    let v = s
        .call(
            "POST",
            "/api/official/user/account/login/equipment",
            &[],
            Some(json!({"account": "anna@example.org", "password": hash, "equipmentNo": SERIAL})),
        )
        .await;
    assert_ok(&v);
    let v = s
        .call(
            "POST",
            "/api/terminal/user/bindEquipment",
            &[],
            Some(json!({"account": "anna@example.org", "equipmentNo": SERIAL})),
        )
        .await;
    assert_err(&v, "E0083");
}

fn journal_lines(data: &Path) -> Vec<Value> {
    let journal = notedown_cloud::journal::Journal::open(data).unwrap();
    journal
        .read_from(0)
        .unwrap()
        .map(|e| serde_json::to_value(e.unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn bind_in_accept_mode_creates_the_native_folders_and_journals_once() {
    let s = Server::accept();
    s.login("secret", SERIAL, "202407").await;
    assert_ok(&s.bind(SERIAL, "202407").await);
    assert_ok(&s.bind(SERIAL, "202407").await); // already bound: no second event
    for folder in [
        "NOTE/Note",
        "NOTE/MyStyle",
        "DOCUMENT/Document",
        "EXPORT",
        "SCREENSHOT",
        "INBOX",
    ] {
        assert!(
            s.data().join("accounts/jordi/files").join(folder).is_dir(),
            "{folder}"
        );
    }
    let lines = journal_lines(s.data());
    assert_eq!(lines.len(), 9);
    // the native folders, parents first, each a commit of the device
    let created: Vec<&str> = lines[..8]
        .iter()
        .map(|l| {
            assert_eq!(l["type"], "folder.created", "{l}");
            assert_eq!(l["origin"], "device", "{l}");
            assert_eq!(l["device"], SERIAL, "{l}");
            l["path"].as_str().unwrap()
        })
        .collect();
    assert_eq!(created, notedown_cloud::store::NATIVE_FOLDERS);
    let bound = &lines[8];
    assert_eq!(bound["type"], "device.bound");
    assert_eq!(bound["account"], "jordi");
    assert_eq!(bound["device"], SERIAL);
    assert_eq!(bound["seq"], 9);
    assert_eq!(bound["ts"], T0);
    let v = s
        .call(
            "POST",
            "/api/terminal/equipment/unlink",
            &[],
            Some(json!({"equipmentNo": SERIAL, "version": "202407"})),
        )
        .await;
    assert_ok(&v);
    // a second unlink of an unbound device journals nothing
    let v = s
        .call(
            "POST",
            "/api/terminal/equipment/unlink",
            &[],
            Some(json!({"equipmentNo": SERIAL, "version": "202407"})),
        )
        .await;
    assert_ok(&v);
    let lines = journal_lines(s.data());
    assert_eq!(lines.len(), 10);
    assert_eq!(lines[9]["type"], "device.unbound");
    assert_eq!(lines[9]["seq"], 10);
    // bound again after the unlink: journaled again
    s.login("secret", SERIAL, "202407").await;
    assert_ok(&s.bind(SERIAL, "202407").await);
    assert_eq!(journal_lines(s.data()).len(), 11);
}

#[tokio::test]
async fn a_journal_failure_does_not_fail_the_unlink() {
    let s = Server::accept();
    s.login("secret", SERIAL, "202407").await;
    assert_ok(&s.bind(SERIAL, "202407").await);
    let dir = s.data().join("journal");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"not a folder").unwrap();
    let v = s
        .call(
            "POST",
            "/api/terminal/equipment/unlink",
            &[],
            Some(json!({"equipmentNo": SERIAL, "version": "202407"})),
        )
        .await;
    assert_ok(&v);
}

#[tokio::test]
async fn a_firmware_change_is_journaled() {
    let s = Server::new();
    s.login("secret", SERIAL, "202407").await; // first sighting: no change
    s.login("secret", SERIAL, "202407").await;
    assert!(journal_lines(s.data()).is_empty());
    s.login("secret", SERIAL, "202408").await;
    let lines = journal_lines(s.data());
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["type"], "device.firmware_changed");
    assert_eq!(lines[0]["device"], SERIAL);
    assert_eq!(lines[0]["origin"], "device");
    assert_eq!(lines[0]["account"], "jordi");
    assert_eq!(lines[0]["from"], "202407");
    assert_eq!(lines[0]["to"], "202408");
}

#[test]
fn server_start_reports_a_broken_account_and_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(dir.path());
    accounts.create("jordi", EMAIL, "secret").unwrap();
    let account = accounts.account_dir("jordi");
    std::fs::create_dir_all(account.join("files")).unwrap();
    std::fs::write(account.join("pending.json"), b"{broken").unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(T0)));
    let state = AppState::open_with_clock(Config::default(), dir.path(), clock).unwrap();
    drop(state);
    let mut text = String::new();
    for e in std::fs::read_dir(dir.path().join("logs")).unwrap() {
        text.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    let marker: Value = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["flags"][0] == "recovery_failed")
        .expect("a recovery_failed marker");
    assert_eq!(marker["kind"], "marker");
    let message = marker["message"].as_str().unwrap();
    assert!(
        message.contains("account jordi: recovery failed"),
        "{message}"
    );
    assert!(message.contains("pending.json"), "{message}");
}

#[tokio::test]
async fn a_journal_failure_does_not_fail_the_bind() {
    let s = Server::accept();
    s.login("secret", SERIAL, "202407").await;
    // the journal folder is replaced by a file: every append fails
    let dir = s.data().join("journal");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"not a folder").unwrap();
    assert_ok(&s.bind(SERIAL, "202407").await);
}

// server info

#[tokio::test]
async fn query_server_get_and_post_and_base_param() {
    let s = Server::new();
    assert_ok(&s.call("GET", "/api/file/query/server", &[], None).await);
    assert_ok(
        &s.call("POST", "/api/file/query/server", &[], Some(json!({})))
            .await,
    );
    let v = s
        .call(
            "POST",
            "/api/official/system/base/param",
            &[],
            Some(json!({})),
        )
        .await;
    assert_ok(&v);
    let names: Vec<&str> = v["paramList"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for n in [
        "COPY_MAX",
        "DOWNLOAD_MAX_NUMBER",
        "EMAIL_CODE_TIME",
        "FILE_MAX",
        "FILE_TYPE",
        "MAX_ERR_COUNTS",
        "UPLOAD_MAX",
    ] {
        assert!(names.contains(&n), "{n}");
    }
    for p in v["paramList"].as_array().unwrap() {
        assert!(p.get("serial").is_some() && p["value"].is_string(), "{p}");
    }
    assert!(v.get("random").is_some());
}

// safety: every file operation is refused

/// Every documented file, digest, task, oss, recycle, share and convert endpoint
/// (docs/sync-protocol.md 3.4, 3.5, 3.8, 5.3, 6.1, 6.3 to 6.6, 6.8).
const REFUSED: &[(&str, &str)] = &[
    ("POST", "/api/file/2/files/synchronous/start"),
    ("POST", "/api/file/2/files/synchronous/end"),
    ("POST", "/api/file/2/files/list_folder"),
    ("POST", "/api/file/3/files/list_folder_v3"),
    ("POST", "/api/file/3/files/query/by/path_v3"),
    ("POST", "/api/file/3/files/query_v3"),
    ("POST", "/api/file/2/files/create_folder_v2"),
    ("POST", "/api/file/3/files/upload/apply"),
    ("POST", "/api/oss/upload"),
    ("POST", "/api/oss/upload/part"),
    ("POST", "/api/file/2/files/upload/finish"),
    ("POST", "/api/file/3/files/download_v3"),
    ("GET", "/api/oss/download"),
    ("POST", "/api/file/3/files/delete_folder_v3"),
    ("POST", "/api/file/3/files/move_v3"),
    ("POST", "/api/file/3/files/copy_v3"),
    ("POST", "/api/file/2/users/get_space_usage"),
    ("POST", "/api/file/capacity/query"),
    ("POST", "/api/file/terminal/upload/apply"),
    ("POST", "/api/file/terminal/upload/finish"),
    ("POST", "/api/file/upload/apply"),
    ("POST", "/api/file/upload/finish"),
    ("POST", "/api/file/add/summary"),
    ("PUT", "/api/file/update/summary"),
    ("DELETE", "/api/file/delete/summary"),
    ("POST", "/api/file/query/summary"),
    ("POST", "/api/file/query/summary/hash"),
    ("POST", "/api/file/query/summary/id"),
    ("POST", "/api/file/add/summary/group"),
    ("PUT", "/api/file/update/summary/group"),
    ("DELETE", "/api/file/delete/summary/group"),
    ("POST", "/api/file/query/summary/group"),
    ("POST", "/api/file/add/summary/tag"),
    ("PUT", "/api/file/update/summary/tag"),
    ("DELETE", "/api/file/delete/summary/tag"),
    ("GET", "/api/file/query/summary/tag"),
    ("POST", "/api/file/upload/apply/summary"),
    ("POST", "/api/file/download/summary"),
    ("POST", "/api/file/schedule/group/all"),
    ("POST", "/api/file/schedule/group"),
    ("PUT", "/api/file/schedule/group"),
    ("DELETE", "/api/file/schedule/group/123"),
    ("POST", "/api/file/schedule/group/clear"),
    ("GET", "/api/file/schedule/group/123"),
    ("POST", "/api/file/schedule/task/all"),
    ("POST", "/api/file/schedule/task"),
    ("PUT", "/api/file/schedule/task"),
    ("PUT", "/api/file/schedule/task/list"),
    (
        "DELETE",
        "/api/file/schedule/task/0123456789abcdef0123456789abcdef",
    ),
    (
        "GET",
        "/api/file/schedule/task/0123456789abcdef0123456789abcdef",
    ),
    ("POST", "/api/file/schedule/sort"),
    ("PUT", "/api/file/schedule/sort"),
    ("DELETE", "/api/file/schedule/sort/123"),
    ("POST", "/api/file/query/schedule/sort"),
    ("POST", "/api/file/recycle/list/query"),
    ("POST", "/api/file/recycle/clear"),
    ("POST", "/api/file/recycle/delete"),
    ("POST", "/api/file/recycle/revert"),
    ("POST", "/api/file/share/record/add"),
    ("POST", "/api/file/note/to/pdf"),
    ("POST", "/api/file/note/to/png"),
    ("POST", "/api/file/pdfwithmark/to/pdf"),
    ("POST", "/api/file/list/query"),
    ("POST", "/api/file/folder/list/query"),
    ("POST", "/api/file/delete"),
    ("POST", "/api/file/folder/add"),
    ("POST", "/api/file/move"),
    ("POST", "/api/file/copy"),
    ("POST", "/api/file/rename"),
    ("POST", "/api/file/list/search"),
    ("POST", "/api/file/path/query"),
    ("POST", "/api/file/download/url"),
];

/// Relative path, size and content hash of every file in the data folder except `logs/`.
fn snapshot(dir: &Path) -> Vec<(String, u64, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, String)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel == "logs" {
                continue;
            }
            if p.is_dir() {
                out.push((format!("{rel}/"), 0, String::new()));
                walk(root, &p, out);
            } else {
                let bytes = std::fs::read(&p).unwrap();
                out.push((
                    rel,
                    bytes.len() as u64,
                    md5_hex(&String::from_utf8_lossy(&bytes)),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

const LISTING_KEYS: &[&str] = &[
    "entries",
    "entriesVO",
    "metadata",
    "summaryDOList",
    "summaryInfoVOList",
    "scheduleTask",
    "scheduleTaskGroup",
    "recycleFileVOList",
    "url",
    "fullUploadUrl",
    "synType",
];

#[tokio::test]
async fn synchronous_start_is_refused_with_e0079() {
    let s = Server::new();
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    let token = login["token"].as_str().unwrap();
    let v = s
        .call(
            "POST",
            "/api/file/2/files/synchronous/start",
            &[("x-access-token", token)],
            Some(json!({"equipmentNo": SERIAL})),
        )
        .await;
    assert_err(&v, "E0079");
    assert!(v.get("synType").is_none());
    let logged = s.requests_to("/api/file/2/files/synchronous/start");
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0]["error_code"], "E0079");
    assert_eq!(logged[0]["success"], false);
    assert_eq!(logged[0]["device"], SERIAL);
    assert!(
        logged[0].get("flags").is_none(),
        "documented path is not unknown"
    );
}

#[tokio::test]
async fn every_documented_file_endpoint_is_refused() {
    let s = Server::new();
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    let token = login["token"].as_str().unwrap().to_string();
    let before = snapshot(s.data());
    for (method, path) in REFUSED {
        for auth in [None, Some(token.as_str())] {
            let headers: Vec<(&str, &str)> =
                auth.map(|t| ("x-access-token", t)).into_iter().collect();
            let body =
                (*method != "GET").then(|| json!({"equipmentNo": SERIAL, "path": "/", "id": "16"}));
            let v = s.call(method, path, &headers, body).await;
            assert_err(&v, "E0079");
            for k in LISTING_KEYS {
                assert!(v.get(*k).is_none(), "{method} {path} answered {k}");
            }
        }
    }
    // nothing in the data folder changed (the log aside)
    assert_eq!(snapshot(s.data()), before);
    assert!(!s.data().join("accounts/jordi/files").exists());
    // documented paths are not flagged as unknown (repeats after an error are retries)
    for e in s.log() {
        if e["error_code"] == "E0079" {
            assert!(!e["flags"].to_string().contains("unknown_endpoint"), "{e}");
        }
    }
}

#[tokio::test]
async fn accept_mode_without_m3_routes_still_refuses_digests_and_todos() {
    let s = Server::accept();
    assert_eq!(s.state.sync_mode(), SyncMode::Accept);
    let (login, _, _) = s.login("secret", SERIAL, "202407").await;
    let token = login["token"].as_str().unwrap();
    for path in [
        "/api/file/query/summary/group",
        "/api/file/schedule/group/all",
        "/api/file/terminal/upload/apply",
    ] {
        let v = s
            .call(
                "POST",
                path,
                &[("x-access-token", token)],
                Some(json!({"equipmentNo": SERIAL})),
            )
            .await;
        assert_err(&v, "E0079");
    }
    // query/server stays answered
    assert_ok(&s.call("POST", "/api/file/query/server", &[], None).await);
}

#[tokio::test]
async fn signature_in_a_logged_query_is_masked() {
    let s = Server::new();
    s.call(
        "GET",
        "/api/oss/download?path=a&signature=sEcReT-sig-value&timestamp=1&nonce=n",
        &[],
        None,
    )
    .await;
    let text = s.log_text();
    assert!(!text.contains("sEcReT-sig-value"), "{text}");
    let q = s.requests_to("/api/oss/download")[0]["query"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(q.contains("signature=<masked:"), "{q}");
    assert!(q.contains("path=a"), "{q}");
}

#[tokio::test]
async fn undocumented_file_paths_are_refused_and_flagged() {
    let s = Server::new();
    for (method, path) in [
        ("POST", "/api/file/3/files/list"),
        ("POST", "/api/file/3/files/upload/confirm"),
        ("POST", "/api/file"),
        ("POST", "/api/oss"),
        ("POST", "//api//file//2/files/list_folder"),
        ("POST", "/API/FILE/2/files/list_folder"),
        ("PATCH", "/api/file/2/files/synchronous/start"),
    ] {
        let v = s.call(method, path, &[], Some(json!({}))).await;
        assert_err(&v, "E0079");
        let logged = s.requests_to(path);
        assert_eq!(logged.len(), 1, "{path}");
        assert_eq!(logged[0]["flags"], json!(["unknown_endpoint"]), "{path}");
    }
}

#[tokio::test]
async fn oversized_upload_is_refused_without_reading_it_whole() {
    let s = Server::new();
    let big = vec![b'x'; MAX_REQUEST_BODY + 1];
    let (status, v) = s
        .raw(
            "127.0.0.1:1",
            "POST",
            "/api/oss/upload?signature=abc&timestamp=1&nonce=n&path=p",
            &[("content-type", "multipart/form-data; boundary=x")],
            Body::from(big),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_err(&v, "E0079");
    let logged = s.requests_to("/api/oss/upload");
    assert_eq!(logged.len(), 1);
    assert!(
        logged[0]["request_body"]
            .as_str()
            .unwrap()
            .contains("not read"),
        "{}",
        logged[0]
    );
    assert!(!s.log_text().contains("signature=abc"));
}

// catch-all and log

#[tokio::test]
async fn unknown_path_is_flagged_logged_once_and_answered() {
    let s = Server::new();
    let v = s
        .call(
            "POST",
            "/api/some/new/endpoint?x=1&token=SECRETQ",
            &[],
            Some(json!({"equipmentNo": SERIAL, "password": "hunter2"})),
        )
        .await;
    assert_err(&v, "500");
    let logged = s.requests_to("/api/some/new/endpoint");
    assert_eq!(logged.len(), 1);
    let e = &logged[0];
    assert_eq!(e["flags"], json!(["unknown_endpoint"]));
    assert_eq!(e["method"], "POST");
    assert_eq!(e["device"], SERIAL);
    assert_eq!(e["response_status"], 200);
    assert_eq!(e["error_code"], "500");
    assert!(e["duration_ms"].is_u64());
    assert!(e["query"].as_str().unwrap().contains("x=1"));
    assert_eq!(e["request_body"]["password"], "<masked:7 chars>");
    assert!(e["headers"]["content-type"].is_string());
    assert!(!s.log_text().contains("hunter2"));
    assert!(!s.log_text().contains("SECRETQ"));

    // a wrong method on an implemented route and a path outside /api
    let v = s
        .call(
            "GET",
            "/api/official/user/account/login/equipment",
            &[],
            None,
        )
        .await;
    assert_err(&v, "500");
    let logged = s.requests_to("/api/official/user/account/login/equipment");
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0]["flags"], json!(["unknown_endpoint"]));
    let v = s.call("GET", "/", &[], None).await;
    assert_err(&v, "500");
    assert_eq!(s.requests_to("/").len(), 1);
}

#[tokio::test]
async fn every_request_is_logged_once() {
    let s = Server::new();
    let token = {
        assert_ok(&check_exists(&s).await);
        finish_sequence(&s).await
    };
    let _ = token;
    let reqs: Vec<Value> = s
        .log()
        .into_iter()
        .filter(|e| e["kind"] == "request")
        .collect();
    let paths: Vec<&str> = reqs.iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        [
            "/api/official/user/check/exists/server",
            "/api/official/user/query/random/code",
            "/api/official/user/account/login/equipment",
            "/api/terminal/user/bindEquipment",
            "/api/user/query",
        ]
    );
    for e in &reqs {
        assert_eq!(e["success"], true, "{e}");
        assert_eq!(e["response_status"], 200);
        assert!(e["response_body"].is_object());
    }
    // user/query has no body: the device comes from the token
    assert_eq!(reqs[4]["device"], SERIAL);
    assert_eq!(reqs[2]["device"], SERIAL);
    assert_eq!(reqs[2]["firmware"], "202407");
}

#[tokio::test]
async fn secrets_never_reach_the_log_file() {
    let s = Server::new();
    let (login, hash, code) = s.login("secret", SERIAL, "202407").await;
    let token = login["token"].as_str().unwrap().to_string();
    let channel = format!("{token}_{SERIAL}_1791270000000");
    s.call(
        "POST",
        "/api/user/query",
        &[("x-access-token", &token), ("channel", &channel)],
        None,
    )
    .await;
    s.call(
        "POST",
        "/api/user/query/token",
        &[("x-access-token", &token)],
        None,
    )
    .await;
    let text = s.log_text();
    for secret in [&token, &hash, &code, &md5_hex("secret")] {
        assert!(!text.contains(secret.as_str()), "secret in log: {secret}");
    }
    // the second half of the token alone must not appear either
    let sig = token.rsplit('.').next().unwrap();
    assert!(!text.contains(sig));
    assert!(text.contains("<masked:"));
    let q = &s.requests_to("/api/user/query")[0];
    assert!(
        q["headers"]["x-access-token"]
            .as_str()
            .unwrap()
            .starts_with("<masked:")
    );
    assert!(
        q["headers"]["channel"]
            .as_str()
            .unwrap()
            .starts_with("<masked:")
    );
}

#[tokio::test]
async fn retry_after_an_error_is_flagged() {
    let s = Server::new();
    let body = json!({"equipmentNo": SERIAL});
    let path = "/api/file/2/files/synchronous/start";
    s.call("POST", path, &[], Some(body.clone())).await;
    s.advance_ms(3_000);
    s.call("POST", path, &[], Some(body.clone())).await;
    s.advance_ms(6_000);
    s.call("POST", path, &[], Some(body.clone())).await;
    // a successful call is never the base of a retry
    s.call("GET", "/api/file/query/server", &[], None).await;
    s.call("GET", "/api/file/query/server", &[], None).await;
    let logged = s.requests_to(path);
    assert_eq!(logged.len(), 3);
    assert!(logged[0].get("flags").is_none());
    assert_eq!(logged[1]["flags"], json!(["retry_suspected"]));
    assert!(logged[2].get("flags").is_none());
    for e in s.requests_to("/api/file/query/server") {
        assert!(e.get("flags").is_none());
    }
}

#[tokio::test]
async fn firmware_change_writes_a_marker() {
    let s = Server::new();
    s.login("secret", SERIAL, "202407").await;
    s.bind(SERIAL, "202407").await;
    let markers = |s: &Server| -> Vec<Value> {
        s.log()
            .into_iter()
            .filter(|e| e["kind"] == "marker")
            .collect()
    };
    assert!(markers(&s).is_empty(), "first sighting is not a change");
    s.login("secret", SERIAL, "202501").await;
    let m = markers(&s);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["flags"], json!(["firmware_changed"]));
    assert_eq!(m[0]["device"], SERIAL);
    assert_eq!(m[0]["firmware"], "202501");
    s.bind(SERIAL, "202501").await;
    assert_eq!(markers(&s).len(), 1);
    let dev: Value = serde_json::from_str(
        &std::fs::read_to_string(
            s.data()
                .join(format!("accounts/jordi/devices/{SERIAL}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(dev["firmware"], "202501");
}

// rate limit per client address

async fn ask_code(s: &Server, peer: &str) -> Value {
    s.call_from(
        peer,
        "POST",
        "/api/official/user/query/random/code",
        &[],
        Some(json!({"account": EMAIL})),
    )
    .await
}

#[tokio::test]
async fn per_address_limit_refuses_only_that_address() {
    let s = Server::new();
    let a = "192.168.1.60:5000";
    let b = "192.168.1.61:5000";
    for _ in 0..10 {
        assert_ok(&ask_code(&s, a).await);
    }
    let refused = ask_code(&s, a).await;
    assert_err(&refused, "409");
    assert!(refused.get("randomCode").is_none());
    assert_ok(&ask_code(&s, b).await);
    // login/equipment has its own budget per address, and a refused login leaves the account
    // alone: no failure counted, the pending code not consumed
    let c = "192.168.1.80:7000";
    let code = ask_code(&s, c).await["randomCode"]
        .as_str()
        .unwrap()
        .to_string();
    let login_from = |peer: &'static str, account: &'static str, hash: String| {
        let s = &s;
        async move {
            s.call_from(
                peer,
                "POST",
                "/api/official/user/account/login/equipment",
                &[],
                Some(json!({"account": account, "password": hash, "equipmentNo": SERIAL})),
            )
            .await
        }
    };
    for _ in 0..10 {
        // another account: uses up the budget without touching jordi's code
        assert_err(
            &login_from(c, "ghost@example.org", "00".into()).await,
            "E0018",
        );
    }
    let before = Accounts::new(s.data()).get("jordi").unwrap().unwrap();
    let v = login_from(c, EMAIL, expected_login_hash(&md5_hex("wrong"), &code)).await;
    assert_err(&v, "409");
    let after = Accounts::new(s.data()).get("jordi").unwrap().unwrap();
    assert_eq!(after.failed_logins, before.failed_logins);
    assert!(!after.locked);
    // the code is still pending: the right password from another address uses it
    let v = login_from(
        "192.168.1.81:7000",
        EMAIL,
        expected_login_hash(&md5_hex("secret"), &code),
    )
    .await;
    assert_ok(&v);
    // after a minute the address may try again
    s.advance_ms(60_001);
    assert_ok(&ask_code(&s, a).await);
}

#[tokio::test]
async fn forwarded_for_counts_only_behind_a_trusted_proxy() {
    let config = Config {
        trusted_proxies: vec!["10.0.0.1".parse().unwrap()],
        ..Config::default()
    };
    let s = Server::with_config(config);
    let proxy = "10.0.0.1:443";
    let ask = |xff: &'static str, peer: &'static str| {
        let s = &s;
        async move {
            s.call_from(
                peer,
                "POST",
                "/api/official/user/query/random/code",
                &[("x-forwarded-for", xff)],
                Some(json!({"account": EMAIL})),
            )
            .await
        }
    };
    for _ in 0..10 {
        assert_ok(&ask("6.6.6.6, 1.1.1.1", proxy).await);
    }
    // the rightmost entry is the client: 1.1.1.1 is used up, 2.2.2.2 is not
    assert_err(&ask("2.2.2.2, 1.1.1.1", proxy).await, "409");
    assert_ok(&ask("1.1.1.1, 2.2.2.2", proxy).await);
    // an untrusted peer cannot choose its address with the header
    let untrusted = "192.168.1.70:1";
    for i in 0..10 {
        let _ = i;
        assert_ok(&ask("3.3.3.3", untrusted).await);
    }
    assert_err(&ask("4.4.4.4", untrusted).await, "409");
}

// real listener

#[tokio::test]
async fn serves_on_a_real_loopback_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let s = Server::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = s.state.clone();
    let task = tokio::spawn(notedown_cloud::server::serve_listeners(
        vec![listener],
        state,
    ));
    let mut answers = Vec::new();
    for path in [
        "/api/file/query/server",
        "/api/file/2/files/synchronous/start",
    ] {
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        conn.write_all(req.as_bytes()).await.unwrap();
        let mut text = String::new();
        conn.read_to_string(&mut text).await.unwrap();
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        answers.push(serde_json::from_str::<Value>(body).unwrap());
    }
    task.abort();
    assert_ok(&answers[0]);
    assert_err(&answers[1], "E0079");
    assert_eq!(s.requests_to("/api/file/query/server").len(), 1);
}

#[tokio::test]
async fn oversized_body_to_a_known_route_is_not_called_unknown() {
    let s = Server::new();
    let (status, v) = s
        .raw(
            "127.0.0.1:1",
            "POST",
            "/api/official/user/account/login/equipment",
            &[("content-type", "application/json")],
            Body::from(vec![b' '; MAX_REQUEST_BODY + 1]),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_err(&v, "422");
    let logged = s.requests_to("/api/official/user/account/login/equipment");
    assert_eq!(logged.len(), 1);
    assert!(logged[0].get("flags").is_none(), "{}", logged[0]);
}

// fix round 1

#[tokio::test]
async fn truncated_login_body_never_leaks_the_hash() {
    let s = Server::new();
    let secret = "5ebe2294ecd0e0f08eab7690d2a6ee69";
    let body = format!(r#"{{"account":"{EMAIL}","password":"{secret}","equipmentNo":"SN07"#);
    let (status, v) = s
        .raw(
            "127.0.0.1:1",
            "POST",
            "/api/official/user/account/login/equipment",
            &[("content-type", "application/json; charset=UTF-8")],
            Body::from(body),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_err(&v, "422");
    let text = s.log_text();
    assert!(!text.contains(secret), "{text}");
    assert!(text.contains("<masked:32 chars>"), "{text}");
    // a form body with a bare flag
    s.raw(
        "127.0.0.1:1",
        "POST",
        "/api/some/form",
        &[("content-type", "application/x-www-form-urlencoded")],
        Body::from("password=hunter2&flag"),
    )
    .await;
    assert!(!s.log_text().contains("hunter2"));
}

#[tokio::test]
async fn percent_encoded_file_path_is_refused() {
    let s = Server::new();
    for path in [
        "/api/%66ile/2/files/list_folder",
        "/api/file%2F2/files/list_folder",
        "/%61pi/oss/upload",
        "/api/./file/2/files/list_folder",
        "/api/x/../file/2/files/list_folder",
        "/api/%2e/file/2/files/list_folder",
        "/api/file;v=1/2/files/list_folder",
        "/api;x=y/oss/upload",
        "/file/2/files/list_folder",
        "/FILE/2/files/synchronous/start",
        "/oss/download",
    ] {
        let v = s.call("POST", path, &[], Some(json!({}))).await;
        assert_err(&v, "E0079");
        let logged = s.requests_to(path);
        assert_eq!(logged.len(), 1, "{path}");
        assert_eq!(logged[0]["flags"], json!(["unknown_endpoint"]), "{path}");
    }
}

#[tokio::test]
async fn oversized_body_to_an_unknown_path_is_flagged() {
    let s = Server::new();
    let (status, v) = s
        .raw(
            "127.0.0.1:1",
            "POST",
            "/api/nowhere",
            &[("content-type", "application/json")],
            Body::from(vec![b' '; MAX_REQUEST_BODY + 1]),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_err(&v, "422");
    let logged = s.requests_to("/api/nowhere");
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0]["flags"], json!(["unknown_endpoint"]));
}

#[tokio::test]
async fn tokens_in_unknown_fields_and_raw_bodies_are_masked() {
    let s = Server::new();
    let (resp, _, _) = s.login("secret", "SN078C10000001", "Dsr.1").await;
    let token = resp["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("eyJ"));
    // The device's own key order, an unknown key and `channel` as a body field and a query value.
    let raw = format!(
        r#"{{"equipmentNo":"SN078C10000001","accessKey":"{token}","channel":"{token}_SN078C10000001_1791270000000"}}"#
    );
    let path = format!("/api/user/query?t={token}&channel={token}_SN1_1");
    let (status, _) = s
        .raw(
            "192.168.1.50:40000",
            "POST",
            &path,
            &[("content-type", "application/json")],
            Body::from(raw),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let text = s.log_text();
    assert!(!text.contains(&token[..20]), "{text}");
    let logged = s.requests_to("/api/user/query");
    assert_eq!(logged.len(), 1);
    let raw = logged[0]["request_body_raw"].as_str().unwrap();
    assert!(
        raw.starts_with(r#"{"equipmentNo":"SN078C10000001","accessKey":"<masked:"#),
        "{raw}"
    );
    assert!(
        logged[0]["query"]
            .as_str()
            .unwrap()
            .starts_with("t=<masked:")
    );
    // The login answer (as sent, with its token) is logged masked as well.
    let login = s.requests_to("/api/official/user/account/login/equipment");
    assert!(
        login[0]["response_body"]["token"]
            .as_str()
            .unwrap()
            .starts_with("<masked:")
    );
}
