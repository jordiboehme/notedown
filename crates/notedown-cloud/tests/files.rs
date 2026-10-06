//! Accept mode file sync: the sync lock and the protect gate, listing, queries, signed
//! downloads and capacity (`docs/sync-protocol.md` 3.4, 3.5). Requests go through the router in
//! memory (`tower::oneshot`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use notedown_cloud::api::oss::{ByteRange, parse_range};
use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
use notedown_cloud::server::{AppState, MAX_REQUEST_BODY, router};
use notedown_cloud::sync::urls::{base_url, content_disposition};
use notedown_cloud::sync::{Purpose, SignedTarget, UrlError, UrlSigner};
use notedown_cloud::{Clock, Config, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const SERIAL: &str = "SN078C10000000";
const OTHER: &str = "SN078C10000001";
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

struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Server {
    /// Accept mode with the protect gate open (most tests).
    fn new() -> Server {
        Self::with_config(Config {
            protect: false,
            ..Config::default()
        })
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

    fn files(&self) -> PathBuf {
        self.data().join("accounts/jordi/files")
    }

    fn put(&self, path: &str, bytes: &[u8]) {
        let p = self.files().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    async fn raw(&self, method: &str, uri: &str, headers: &[(&str, &str)], body: Body) -> Raw {
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = router(self.state.clone())
            .layer(MockConnectInfo(addr))
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Raw {
            status,
            headers,
            body,
        }
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> Value {
        let r = self
            .raw(
                "POST",
                path,
                &[
                    ("x-access-token", token),
                    ("content-type", "application/json"),
                    ("host", "192.168.1.10:19072"),
                ],
                Body::from(body.to_string()),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.json());
        r.json()
    }

    async fn get(&self, uri: &str, headers: &[(&str, &str)]) -> Raw {
        self.raw("GET", uri, headers, Body::empty()).await
    }

    /// random/code, login/equipment and bindEquipment; returns the token.
    async fn login(&self, serial: &str) -> String {
        let ch = self
            .raw(
                "POST",
                "/api/official/user/query/random/code",
                &[("content-type", "application/json")],
                Body::from(json!({"account": EMAIL, "version": "202407"}).to_string()),
            )
            .await
            .json();
        let code = ch["randomCode"].as_str().unwrap();
        let hash = expected_login_hash(&md5_hex("secret"), code);
        let login = self
            .raw(
                "POST",
                "/api/official/user/account/login/equipment",
                &[("content-type", "application/json")],
                Body::from(
                    json!({"account": EMAIL, "password": hash, "equipmentNo": serial,
                           "equipment": 3, "loginMethod": "2",
                           "timestamp": ch["timestamp"], "version": "202407"})
                    .to_string(),
                ),
            )
            .await
            .json();
        assert_eq!(login["success"], true, "{login}");
        let token = login["token"].as_str().unwrap().to_string();
        let bind = self
            .post(
                "/api/terminal/user/bindEquipment",
                &token,
                json!({"account": EMAIL, "equipmentNo": serial, "flag": "1",
                       "name": "Supernote Nomad", "totalCapacity": "25485312",
                       "version": "202407"}),
            )
            .await;
        assert_eq!(bind["success"], true, "{bind}");
        token
    }

    fn log(&self) -> Vec<Value> {
        let mut files: Vec<_> = std::fs::read_dir(self.data().join("logs"))
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
                    .map(|l| serde_json::from_str::<Value>(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn log_text(&self) -> String {
        self.log()
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn requests_to(&self, path: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["kind"] == "request" && e["path"] == path)
            .collect()
    }

    fn mark_imported(&self) {
        let accounts = Accounts::new(self.data());
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 3,
            bytes: 1234,
        });
        accounts.save(&a).unwrap();
    }
}

const START: &str = "/api/file/2/files/synchronous/start";
const END: &str = "/api/file/2/files/synchronous/end";
const LIST: &str = "/api/file/2/files/list_folder";
const LIST_V3: &str = "/api/file/3/files/list_folder_v3";
const QUERY: &str = "/api/file/3/files/query_v3";
const BY_PATH: &str = "/api/file/3/files/query/by/path_v3";
const DOWNLOAD: &str = "/api/file/3/files/download_v3";
const CAPACITY: &str = "/api/file/capacity/query";
const SPACE: &str = "/api/file/2/users/get_space_usage";

fn assert_ok(v: &Value) {
    assert_eq!(v["success"], true, "{v}");
    assert!(v["errorCode"].is_null(), "{v}");
}

fn assert_err(v: &Value, code: &str) {
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(v["errorCode"], code, "{v}");
    assert!(v["errorMsg"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
}

fn paths(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("no entries: {v}"))
        .iter()
        .map(|e| e["path_display"].as_str().unwrap().to_string())
        .collect()
}

fn find<'a>(v: &'a Value, path: &str) -> &'a Value {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path_display"] == path)
        .unwrap_or_else(|| panic!("{path} not in {v}"))
}

/// The query of a URL as pairs.
fn query_of(url: &str) -> Vec<(String, String)> {
    url.split_once('?')
        .unwrap()
        .1
        .split('&')
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn path_and_query(url: &str) -> String {
    let rest = url.split_once("://").unwrap().1;
    rest[rest.find('/').unwrap()..].to_string()
}

// ------------------------------------------------------------------ sync lock

#[tokio::test]
async fn start_takes_renews_and_refuses_another_device() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    let v = s.post(START, &a, json!({"equipmentNo": SERIAL})).await;
    assert_ok(&v);
    assert_eq!(v["synType"], true);
    assert_eq!(v["equipmentNo"], SERIAL);
    assert_err(
        &s.post(START, &b, json!({"equipmentNo": OTHER})).await,
        "E0078",
    );
    // the same device again: renewed
    assert_ok(&s.post(START, &a, json!({"equipmentNo": SERIAL})).await);
    // the lock lapses after 30 s without a call
    s.advance_ms(30_001);
    assert_ok(&s.post(START, &b, json!({"equipmentNo": OTHER})).await);
    assert_err(
        &s.post(START, &a, json!({"equipmentNo": SERIAL})).await,
        "E0078",
    );
}

#[tokio::test]
async fn end_releases_only_the_owner_and_logs_the_flag() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    assert_ok(&s.post(START, &a, json!({"equipmentNo": SERIAL})).await);
    // another device's end does not drop the owner's lock
    let v = s
        .post(END, &b, json!({"equipmentNo": OTHER, "flag": "Y"}))
        .await;
    assert_ok(&v);
    assert_err(
        &s.post(START, &b, json!({"equipmentNo": OTHER})).await,
        "E0078",
    );
    let v = s
        .post(END, &a, json!({"equipmentNo": SERIAL, "flag": "N"}))
        .await;
    assert_ok(&v);
    assert_eq!(v["equipmentNo"], SERIAL);
    assert_ok(&s.post(START, &b, json!({"equipmentNo": OTHER})).await);
    let ends = s.requests_to(END);
    assert_eq!(ends.len(), 2);
    let flags0 = ends[0]["flags"].to_string();
    assert!(flags0.contains("sync_end_ok") && flags0.contains("sync_lock_not_held"));
    let flags1 = ends[1]["flags"].to_string();
    assert!(flags1.contains("sync_end_failed"), "{flags1}");
    assert!(!flags1.contains("sync_lock_not_held"), "{flags1}");
}

#[tokio::test]
async fn every_file_call_of_the_owner_extends_the_lock() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    assert_ok(&s.post(START, &a, json!({"equipmentNo": SERIAL})).await);
    s.advance_ms(20_000);
    assert_ok(
        &s.post(LIST, &a, json!({"equipmentNo": SERIAL, "path": "/"}))
            .await,
    );
    s.advance_ms(20_000);
    // 40 s after start, 20 s after the listing: still held
    assert_err(
        &s.post(START, &b, json!({"equipmentNo": OTHER})).await,
        "E0078",
    );
    // a call of the other device does not extend it
    s.advance_ms(9_000);
    s.post(LIST, &b, json!({"equipmentNo": OTHER, "path": "/"}))
        .await;
    s.advance_ms(1_001);
    assert_ok(&s.post(START, &b, json!({"equipmentNo": OTHER})).await);
}

// ------------------------------------------------------------------ protect gate

#[tokio::test]
async fn protect_gate_refuses_start_until_the_import_marker() {
    let s = Server::with_config(Config::default());
    assert!(s.state.config.protect);
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    let v = s.post(START, &a, json!({"equipmentNo": SERIAL})).await;
    assert_err(&v, "E0079");
    let entry = &s.requests_to(START)[0];
    assert!(
        entry["flags"].to_string().contains("protect_refused"),
        "{entry}"
    );
    assert_eq!(entry["device"], SERIAL);
    // the refused start took no lock
    s.mark_imported();
    assert_ok(&s.post(START, &b, json!({"equipmentNo": OTHER})).await);
    assert_err(
        &s.post(START, &a, json!({"equipmentNo": SERIAL})).await,
        "E0078",
    );
}

#[tokio::test]
async fn protect_off_opens_the_gate() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let v = s.post(START, &a, json!({"equipmentNo": SERIAL})).await;
    assert_ok(&v);
    assert!(
        !s.requests_to(START)[0]["flags"]
            .to_string()
            .contains("protect_refused")
    );
}

#[test]
fn imported_marker_round_trips_and_stays_out_of_files_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(dir.path());
    let mut a = accounts.create("jordi", EMAIL, "secret").unwrap();
    let text = std::fs::read_to_string(dir.path().join("accounts/jordi/account.json")).unwrap();
    assert!(!text.contains("imported"), "{text}");
    assert!(!text.contains("quota_bytes"), "{text}");
    a.imported = Some(Imported {
        at: "2026-10-06T10:00:00Z".into(),
        source: "/backup".into(),
        files: 2,
        bytes: 10,
    });
    accounts.save(&a).unwrap();
    let text = std::fs::read_to_string(dir.path().join("accounts/jordi/account.json")).unwrap();
    let locked = text.find("\"locked\"").unwrap();
    let imported = text.find("\"imported\"").unwrap();
    let locked_at = text.find("\"locked_at_ms\"").unwrap();
    assert!(locked < imported && imported < locked_at, "{text}");
    let at = text.find("\"at\"").unwrap();
    let source = text.find("\"source\"").unwrap();
    let files = text.find("\"files\"").unwrap();
    let bytes = text.find("\"bytes\"").unwrap();
    assert!(at < source && source < files && files < bytes, "{text}");
    assert_eq!(accounts.get("jordi").unwrap().unwrap(), a);
}

// ------------------------------------------------------------------ device check

#[tokio::test]
async fn equipment_no_must_match_the_token() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    for path in [
        START, END, LIST, LIST_V3, QUERY, BY_PATH, DOWNLOAD, CAPACITY, SPACE,
    ] {
        let v = s
            .post(path, &a, json!({"equipmentNo": OTHER, "id": "0"}))
            .await;
        assert_err(&v, "E0312");
    }
    // a body without equipmentNo is accepted
    assert_ok(&s.post(LIST, &a, json!({"path": "/"})).await);
    assert_ok(&s.post(CAPACITY, &a, json!({})).await);
    // no token: E0712
    let r = s
        .raw(
            "POST",
            LIST,
            &[],
            Body::from(json!({"path": "/"}).to_string()),
        )
        .await;
    assert_err(&r.json(), "E0712");
}

// ------------------------------------------------------------------ listing

#[tokio::test]
async fn list_folder_shows_the_root_with_native_folders_and_files() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("NOTE/Note/Bike.note", b"bike bytes");
    s.put("NOTE/Note/Work/Plan.note", b"plan");
    s.put("NOTE/Note/.hidden.note", b"x");
    s.put("DOCUMENT/Document/book.pdf", b"%PDF");
    std::fs::create_dir_all(s.data().join("accounts/jordi/staging")).unwrap();

    let top = s
        .post(
            LIST,
            &a,
            json!({"equipmentNo": SERIAL, "path": "/", "recursive": false}),
        )
        .await;
    assert_ok(&top);
    assert_eq!(top["equipmentNo"], SERIAL);
    let mut names = paths(&top);
    names.sort();
    assert_eq!(names, ["DOCUMENT", "EXPORT", "INBOX", "NOTE", "SCREENSHOT"]);

    let all = s
        .post(
            LIST,
            &a,
            json!({"equipmentNo": SERIAL, "path": "", "recursive": true}),
        )
        .await;
    let p = paths(&all);
    for want in [
        "NOTE",
        "NOTE/Note",
        "NOTE/MyStyle",
        "NOTE/Note/Bike.note",
        "NOTE/Note/Work",
        "NOTE/Note/Work/Plan.note",
        "DOCUMENT/Document/book.pdf",
    ] {
        assert!(p.contains(&want.to_string()), "{want} in {p:?}");
    }
    assert!(
        !p.iter()
            .any(|x| x.contains("hidden") || x.contains("staging"))
    );
    let bike = find(&all, "NOTE/Note/Bike.note");
    assert_eq!(bike["tag"], "file");
    assert!(bike["id"].is_string());
    assert_eq!(bike["content_hash"], md5_hex("bike bytes"));
    assert_eq!(bike["size"], 10);
    assert_eq!(bike["parent_path"], "/NOTE/Note");
    assert_eq!(bike["is_downloadable"], true);
    let note = find(&all, "NOTE");
    assert_eq!(note["tag"], "folder");
    assert!(note.get("parent_path").is_none());

    // a subfolder and the double-slash form
    let sub = s
        .post(
            LIST,
            &a,
            json!({"path": "/NOTE//Note/", "recursive": false}),
        )
        .await;
    let mut p = paths(&sub);
    p.sort();
    assert_eq!(p, ["NOTE/Note/Bike.note", "NOTE/Note/Work"]);
    // a missing folder or a file: E0081, never an empty list
    assert_err(
        &s.post(LIST, &a, json!({"path": "/NOTE/Nope"})).await,
        "E0081",
    );
    assert_err(
        &s.post(LIST, &a, json!({"path": "/NOTE/Note/Bike.note"}))
            .await,
        "E0081",
    );
    assert_err(
        &s.post(LIST, &a, json!({"path": "/NOTE/../x"})).await,
        "E0081",
    );
}

#[tokio::test]
async fn list_folder_v3_lists_by_id() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("NOTE/Note/Bike.note", b"bike");
    let all = s
        .post(LIST, &a, json!({"path": "/", "recursive": true}))
        .await;
    let note_id = find(&all, "NOTE/Note")["id"].as_str().unwrap().to_string();
    let file_id = find(&all, "NOTE/Note/Bike.note")["id"]
        .as_str()
        .unwrap()
        .to_string();

    for id in [
        json!(note_id.clone()),
        json!(note_id.parse::<u64>().unwrap()),
    ] {
        let v = s
            .post(
                LIST_V3,
                &a,
                json!({"equipmentNo": SERIAL, "id": id, "recursive": false}),
            )
            .await;
        assert_ok(&v);
        assert_eq!(paths(&v), ["NOTE/Note/Bike.note"]);
    }
    for id in [json!(0), json!("0"), Value::Null] {
        let v = s
            .post(LIST_V3, &a, json!({"id": id, "recursive": true}))
            .await;
        assert_eq!(paths(&v).len(), paths(&all).len(), "{id}");
    }
    let v = s.post(LIST_V3, &a, json!({"recursive": false})).await;
    assert_eq!(paths(&v).len(), 5);
    for id in [json!("999999"), json!(file_id), json!("abc"), json!(-1)] {
        assert_err(&s.post(LIST_V3, &a, json!({"id": id})).await, "E0081");
    }
    assert_err(&s.post(LIST_V3, &a, json!({"id": true})).await, "422");
}

// ------------------------------------------------------------------ queries

#[tokio::test]
async fn query_v3_by_string_and_number_ids() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("INBOX/a.pdf", b"aaa");
    let all = s
        .post(LIST, &a, json!({"path": "/", "recursive": true}))
        .await;
    let id = find(&all, "INBOX/a.pdf")["id"]
        .as_str()
        .unwrap()
        .to_string();
    for sent in [json!(id.clone()), json!(id.parse::<u64>().unwrap())] {
        let v = s
            .post(QUERY, &a, json!({"equipmentNo": SERIAL, "id": sent}))
            .await;
        assert_ok(&v);
        assert_eq!(v["entriesVO"]["path_display"], "INBOX/a.pdf");
        assert_eq!(v["entriesVO"]["id"], id);
        assert_eq!(v["entriesVO"]["content_hash"], md5_hex("aaa"));
    }
    for sent in [json!("424242"), json!("x"), json!(true), Value::Null] {
        let v = s.post(QUERY, &a, json!({"id": sent})).await;
        assert_ok(&v);
        assert!(v["entriesVO"].is_null(), "{v}");
        assert!(v.as_object().unwrap().contains_key("entriesVO"));
    }
    // a file removed on disk: null
    std::fs::remove_file(s.files().join("INBOX/a.pdf")).unwrap();
    let v = s.post(QUERY, &a, json!({"id": id})).await;
    assert!(v["entriesVO"].is_null(), "{v}");
}

#[tokio::test]
async fn query_by_path_v3_answers_200_with_null_or_the_entry() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("NOTE/Note/Personal/IMG.jpg", b"jpeg");
    let r = s
        .raw(
            "POST",
            BY_PATH,
            &[("x-access-token", &a), ("content-type", "application/json")],
            Body::from(json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work"}).to_string()),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_ok(&v);
    assert!(v["entriesVO"].is_null());
    assert!(v.as_object().unwrap().contains_key("entriesVO"));

    let v = s
        .post(BY_PATH, &a, json!({"path": "NOTE/Note/Personal//IMG.jpg"}))
        .await;
    assert_eq!(v["entriesVO"]["path_display"], "NOTE/Note/Personal/IMG.jpg");
    assert_eq!(v["entriesVO"]["tag"], "file");
    let v = s
        .post(BY_PATH, &a, json!({"path": "/NOTE/Note/Personal/"}))
        .await;
    assert_eq!(v["entriesVO"]["tag"], "folder");
    for path in [
        json!("/"),
        json!(""),
        json!("/NOTE/../x"),
        json!(5),
        Value::Null,
    ] {
        let v = s.post(BY_PATH, &a, json!({"path": path})).await;
        assert_ok(&v);
        assert!(v["entriesVO"].is_null(), "{path}: {v}");
    }
    // another spelling of an existing name is not found
    let v = s
        .post(BY_PATH, &a, json!({"path": "/note/note/personal/img.jpg"}))
        .await;
    assert!(v["entriesVO"].is_null(), "{v}");
}

// ------------------------------------------------------------------ downloads

async fn download_url_for(s: &Server, token: &str, path: &str) -> (Value, String) {
    let all = s
        .post(LIST, token, json!({"path": "/", "recursive": true}))
        .await;
    let id = find(&all, path)["id"].as_str().unwrap().to_string();
    let v = s
        .post(DOWNLOAD, token, json!({"equipmentNo": SERIAL, "id": id}))
        .await;
    assert_ok(&v);
    let url = v["url"].as_str().unwrap().to_string();
    (v, url)
}

#[tokio::test]
async fn download_v3_signs_a_url_and_the_get_serves_the_bytes() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let content: Vec<u8> = (0..=255u8).cycle().take(700_000).collect();
    s.put("NOTE/Note/Bike Ä.note", &content);
    let (v, url) = download_url_for(&s, &a, "NOTE/Note/Bike Ä.note").await;
    assert_eq!(v["equipmentNo"], SERIAL);
    assert!(v["id"].is_string());
    assert_eq!(v["name"], "Bike Ä.note");
    assert_eq!(v["path_display"], "NOTE/Note/Bike Ä.note");
    assert_eq!(v["size"], 700_000);
    assert_eq!(v["is_downloadable"], true);
    assert_eq!(v["content_hash"], md5_hex_bytes(&content));
    assert!(
        url.starts_with("http://192.168.1.10:19072/api/oss/download?"),
        "{url}"
    );
    let keys: Vec<String> = query_of(&url).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["path", "signature", "timestamp", "nonce", "pathId"]);
    let path_id = &query_of(&url)[4].1;
    assert_eq!(path_id, v["id"].as_str().unwrap());

    let r = s.get(&path_and_query(&url), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, content);
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.header("content-length"), Some("700000"));
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(
        r.header("content-disposition"),
        Some("attachment; filename*=UTF-8''Bike%20%C3%84.note")
    );
    assert!(r.header("content-range").is_none());

    // bytes=0- (the Partner app)
    let r = s.get(&path_and_query(&url), &[("range", "bytes=0-")]).await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.body, content);
    assert_eq!(r.header("content-range"), Some("bytes 0-699999/700000"));
    // a middle range
    let r = s
        .get(&path_and_query(&url), &[("range", "bytes=300000-300009")])
        .await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.body, &content[300_000..300_010]);
    assert_eq!(r.header("content-length"), Some("10"));
    assert_eq!(
        r.header("content-range"),
        Some("bytes 300000-300009/700000")
    );
    // a suffix range
    let r = s.get(&path_and_query(&url), &[("range", "bytes=-5")]).await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.body, &content[699_995..]);
    // an end past the file is cut to the file
    let r = s
        .get(&path_and_query(&url), &[("range", "bytes=699990-900000")])
        .await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.body, &content[699_990..]);
    // past the end: 416
    let r = s
        .get(&path_and_query(&url), &[("range", "bytes=700000-")])
        .await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.header("content-range"), Some("bytes */700000"));
    // several ranges: the whole file
    let r = s
        .get(&path_and_query(&url), &[("range", "bytes=0-1,5-6")])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, content);

    // one log entry per GET, attributed to the device, no signature in the log
    let gets = s.requests_to("/api/oss/download");
    assert_eq!(gets.len(), 7);
    assert_eq!(gets[0]["device"], SERIAL);
    assert_eq!(gets[0]["response_status"], 200);
    assert_eq!(gets[0]["response_body"], "<700000 bytes streamed>");
    assert_eq!(gets[5]["response_status"], 416);
    let sig = &query_of(&url)[1].1;
    let text = s.log_text();
    assert!(!text.contains(sig.as_str()), "signature in the log");
}

fn md5_hex_bytes(b: &[u8]) -> String {
    // md5_hex takes text; the test content is binary.
    use md5::{Digest, Md5};
    const_hex::encode(Md5::digest(b))
}

#[tokio::test]
async fn download_of_an_empty_file_with_bytes_0_is_200() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("EXPORT/empty.txt", b"");
    let (_, url) = download_url_for(&s, &a, "EXPORT/empty.txt").await;
    let r = s.get(&path_and_query(&url), &[("range", "bytes=0-")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn download_v3_refuses_unknown_ids_and_folders() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let all = s
        .post(LIST, &a, json!({"path": "/", "recursive": true}))
        .await;
    let folder = find(&all, "NOTE")["id"].clone();
    for id in [
        json!("987654"),
        json!(987654),
        folder,
        json!("x"),
        json!([1]),
        Value::Null,
    ] {
        assert_err(&s.post(DOWNLOAD, &a, json!({"id": id})).await, "E0321");
    }
}

#[tokio::test]
async fn tampered_or_expired_signatures_get_plain_e1306() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("INBOX/a.txt", b"hello");
    let (_, url) = download_url_for(&s, &a, "INBOX/a.txt").await;
    let pq = path_and_query(&url);
    let q = query_of(&url);
    let sig = q[1].1.clone();
    let mut flipped = sig.clone().into_bytes();
    flipped[0] = if flipped[0] == b'0' { b'1' } else { b'0' };
    let bad_sig = pq.replace(&sig, std::str::from_utf8(&flipped).unwrap());
    let mut other = SignedTarget::decode(&q[0].1).unwrap();
    other.path = "INBOX/other.txt".into();
    let other_target = other.encode();
    let bad_path = pq.replace(&q[0].1, &other_target);
    let bad_ts = pq.replace(&q[2].1, &(q[2].1.parse::<u64>().unwrap() + 1).to_string());
    for uri in [
        bad_sig,
        bad_path,
        bad_ts,
        "/api/oss/download".to_string(),
        "/api/oss/download?path=x&signature=y&timestamp=z&nonce=n".to_string(),
    ] {
        let r = s.get(&uri, &[]).await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
        assert_eq!(r.body, b"E1306", "{uri}");
        assert!(r.header("content-type").unwrap().starts_with("text/plain"));
    }
    // valid for 24 h
    s.advance_ms(24 * 60 * 60 * 1000);
    assert_eq!(s.get(&pq, &[]).await.status, StatusCode::OK);
    s.advance_ms(1);
    let r = s.get(&pq, &[]).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.body, b"E1306");
    let last = s.requests_to("/api/oss/download").pop().unwrap();
    assert!(
        last["flags"].to_string().contains("expired_signature"),
        "{last}"
    );
    assert!(!s.log_text().contains(&sig));
}

#[tokio::test]
async fn a_file_gone_before_the_get_answers_plain_e1307() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("INBOX/a.txt", b"hello");
    let (_, url) = download_url_for(&s, &a, "INBOX/a.txt").await;
    std::fs::remove_file(s.files().join("INBOX/a.txt")).unwrap();
    let r = s.get(&path_and_query(&url), &[]).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.body, b"E1307");
    // replaced by a folder: still no panic
    std::fs::create_dir_all(s.files().join("INBOX/a.txt")).unwrap();
    let r = s.get(&path_and_query(&url), &[]).await;
    assert_eq!(r.body, b"E1307");
    let last = s.requests_to("/api/oss/download").pop().unwrap();
    assert_eq!(last["device"], SERIAL);
    assert!(last["flags"].to_string().contains("download_failed"));
}

#[tokio::test]
async fn a_download_extends_the_owner_lock() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    s.put("INBOX/a.txt", b"hello");
    assert_ok(&s.post(START, &a, json!({})).await);
    let (_, url) = download_url_for(&s, &a, "INBOX/a.txt").await;
    s.advance_ms(25_000);
    assert_eq!(
        s.get(&path_and_query(&url), &[]).await.status,
        StatusCode::OK
    );
    s.advance_ms(25_000);
    assert_err(&s.post(START, &b, json!({})).await, "E0078");
}

#[tokio::test]
async fn external_base_url_is_used_for_download_urls() {
    let s = Server::with_config(Config {
        protect: false,
        external_base_url: Some("https://notes.example.org/".into()),
        ..Config::default()
    });
    let a = s.login(SERIAL).await;
    s.put("INBOX/a.txt", b"x");
    let (_, url) = download_url_for(&s, &a, "INBOX/a.txt").await;
    assert!(
        url.starts_with("https://notes.example.org/api/oss/download?path="),
        "{url}"
    );
}

// ------------------------------------------------------------------ capacity

#[tokio::test]
async fn capacity_counts_listed_bytes_and_caches_for_60s() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("NOTE/Note/a.note", &[1u8; 100]);
    s.put("INBOX/b.pdf", &[2u8; 50]);
    s.put("INBOX/.tmp", &[2u8; 999]);
    let v = s.post(CAPACITY, &a, json!({})).await;
    assert_ok(&v);
    assert_eq!(v["usedCapacity"], 150);
    assert_eq!(v["totalCapacity"], 64u64 * 1024 * 1024 * 1024);
    let v = s.post(SPACE, &a, json!({"equipmentNo": SERIAL})).await;
    assert_ok(&v);
    assert_eq!(v["equipmentNo"], SERIAL);
    assert_eq!(v["used"], 150);
    assert_eq!(v["allocationVO"]["tag"], "individual");
    assert_eq!(v["allocationVO"]["allocated"], 64u64 * 1024 * 1024 * 1024);
    s.put("INBOX/c.pdf", &[3u8; 7]);
    assert_eq!(s.post(CAPACITY, &a, json!({})).await["usedCapacity"], 150);
    s.advance_ms(60_000);
    assert_eq!(s.post(CAPACITY, &a, json!({})).await["usedCapacity"], 157);
    // a quota in account.json
    let accounts = Accounts::new(s.data());
    let mut acc = accounts.get("jordi").unwrap().unwrap();
    acc.quota_bytes = Some(1_000_000);
    accounts.save(&acc).unwrap();
    assert_eq!(
        s.post(CAPACITY, &a, json!({})).await["totalCapacity"],
        1_000_000
    );
}

// ------------------------------------------------------------------ routing and log

#[tokio::test]
async fn other_methods_and_refuse_mode_still_answer_e0079() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let r = s.get(LIST, &[("x-access-token", &a)]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_err(&r.json(), "E0079");
    let r = s.raw("POST", "/api/oss/download", &[], Body::empty()).await;
    assert_err(&r.json(), "E0079");
    // without the sync lock (no synchronous/start)
    assert_err(
        &s.post("/api/file/3/files/upload/apply", &a, json!({}))
            .await,
        "E0079",
    );

    let r = Server::with_config(Config {
        sync: SyncMode::Refuse,
        protect: false,
        ..Config::default()
    });
    let t = r.login(SERIAL).await;
    for path in [
        START, LIST, LIST_V3, QUERY, BY_PATH, DOWNLOAD, CAPACITY, SPACE,
    ] {
        assert_err(
            &r.post(path, &t, json!({"equipmentNo": SERIAL})).await,
            "E0079",
        );
    }
    let g = r.get("/api/oss/download?path=a", &[]).await;
    assert_err(&g.json(), "E0079");
}

#[tokio::test]
async fn every_route_writes_one_log_entry_and_a_large_body_is_not_unknown() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let before = s.log().len();
    for path in [
        START, END, LIST, LIST_V3, QUERY, BY_PATH, DOWNLOAD, CAPACITY, SPACE,
    ] {
        s.post(path, &a, json!({"equipmentNo": SERIAL})).await;
    }
    let after = s.log();
    assert_eq!(after.len() - before, 9);
    for e in &after[before..] {
        assert_eq!(e["device"], SERIAL, "{e}");
        assert!(!e["flags"].to_string().contains("unknown_endpoint"), "{e}");
    }
    let big = vec![b' '; MAX_REQUEST_BODY + 10];
    let r = s
        .raw(
            "POST",
            LIST,
            &[("x-access-token", &a), ("content-type", "application/json")],
            Body::from(big),
        )
        .await;
    assert_err(&r.json(), "422");
    let e = s.requests_to(LIST).pop().unwrap();
    assert!(!e["flags"].to_string().contains("unknown_endpoint"), "{e}");
}

#[test]
fn url_key_is_created_with_mode_0600_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let one = UrlSigner::load_or_create(dir.path()).unwrap();
    let path = dir.path().join("keys/url.key");
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim().len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let two = UrlSigner::load_or_create(dir.path()).unwrap();
    let s = one.sign("GET", "cGF0aA", Purpose::Download, 0, 1000);
    let ts = s.timestamp.to_string();
    let check = |signer: &UrlSigner, method, purpose, size, now| {
        signer.verify(
            method,
            "cGF0aA",
            &ts,
            &s.nonce,
            &s.signature,
            purpose,
            size,
            now,
        )
    };
    assert_eq!(check(&two, "GET", Purpose::Download, 0, 1000), Ok(()));
    assert_eq!(
        check(&two, "POST", Purpose::Download, 0, 1000),
        Err(UrlError::Bad)
    );
    assert_eq!(
        check(&two, "GET", Purpose::Upload, 0, 1000),
        Err(UrlError::Bad)
    );
    assert_eq!(
        check(&two, "GET", Purpose::Download, 1, 1000),
        Err(UrlError::Bad)
    );
    // uploads are valid for 30 min
    let u = one.sign("POST", "cGF0aA", Purpose::Upload, 5, 0);
    let ok = |now| {
        one.verify(
            "POST",
            "cGF0aA",
            "0",
            &u.nonce,
            &u.signature,
            Purpose::Upload,
            5,
            now,
        )
    };
    assert_eq!(ok(30 * 60 * 1000), Ok(()));
    assert_eq!(ok(30 * 60 * 1000 + 1), Err(UrlError::Expired));
    // a different key does not verify
    let other = UrlSigner::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
    assert_eq!(
        check(&other, "GET", Purpose::Download, 0, 1000),
        Err(UrlError::Bad)
    );
}

#[test]
fn server_start_creates_the_url_key() {
    let s = Server::new();
    assert!(s.data().join("keys/url.key").is_file());
}

#[test]
fn ranges_parse() {
    assert_eq!(parse_range(None, 10), ByteRange::Full);
    assert_eq!(
        parse_range(Some("bytes=0-"), 10),
        ByteRange::Partial { start: 0, end: 9 }
    );
    assert_eq!(
        parse_range(Some("bytes=2-4"), 10),
        ByteRange::Partial { start: 2, end: 4 }
    );
    assert_eq!(
        parse_range(Some("bytes=-3"), 10),
        ByteRange::Partial { start: 7, end: 9 }
    );
    assert_eq!(
        parse_range(Some("bytes=-30"), 10),
        ByteRange::Partial { start: 0, end: 9 }
    );
    assert_eq!(parse_range(Some("bytes=10-"), 10), ByteRange::Unsatisfiable);
    assert_eq!(parse_range(Some("bytes=-0"), 10), ByteRange::Unsatisfiable);
    assert_eq!(parse_range(Some("bytes=4-2"), 10), ByteRange::Full);
    assert_eq!(parse_range(Some("bytes=0-1,3-4"), 10), ByteRange::Full);
    assert_eq!(parse_range(Some("items=0-1"), 10), ByteRange::Full);
    assert_eq!(parse_range(Some("bytes=x-1"), 10), ByteRange::Full);
    assert_eq!(parse_range(Some("bytes=0-"), 0), ByteRange::Full);
}

#[test]
fn content_disposition_and_base_url() {
    assert_eq!(
        content_disposition("a b/ü~-._(1).note"),
        "attachment; filename*=UTF-8''a%20b%2F%C3%BC~-._%281%29.note"
    );
    assert_eq!(
        base_url(Some("https://x.org/"), Some("h:1")),
        "https://x.org"
    );
    assert_eq!(
        base_url(None, Some("10.0.0.2:19072")),
        "http://10.0.0.2:19072"
    );
    assert_eq!(base_url(None, Some("evil.org/x?y")), "http://localhost");
    assert_eq!(base_url(None, None), "http://localhost");
}

#[test]
fn signed_targets_round_trip() {
    let t = SignedTarget {
        account: "jordi".into(),
        device: SERIAL.into(),
        generation: 3,
        path: "NOTE/Note/a|b ü.note".into(),
        id: Some(1008),
        inner: None,
        size: 0,
    };
    let enc = t.encode();
    assert!(
        enc.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_eq!(SignedTarget::decode(&enc), Some(t.clone()));
    assert_eq!(SignedTarget::decode("!!"), None);
    assert_eq!(SignedTarget::decode("e30"), None);
    let upload = SignedTarget {
        inner: Some("0a1b2c3d-1111-4222-8333-444455556666".into()),
        id: None,
        size: 1135077,
        ..t
    };
    assert_eq!(SignedTarget::decode(&upload.encode()), Some(upload));
    // The Task 5 form (account, device, path) is no longer accepted.
    let old = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        br#"["jordi","SN078C10000000","NOTE/Note/a.note"]"#,
    );
    assert_eq!(SignedTarget::decode(&old), None);
}

#[tokio::test]
async fn list_folder_v3_resolves_the_id_after_recovery() {
    use notedown_cloud::journal::Origin;
    use notedown_cloud::store::{CrashPoint, WirePath};
    let s = Server::new();
    let a = s.login(SERIAL).await;
    s.put("NOTE/Note/Work/Plan.note", b"plan");
    let all = s
        .post(LIST, &a, json!({"path": "/", "recursive": true}))
        .await;
    let id = find(&all, "NOTE/Note/Work")["id"].clone();
    // a folder move that stops after the rename, before the registry and the journal
    let store = s.state.store("jordi").unwrap();
    let g = store.lock_recovered(&s.state.journal).unwrap();
    let c = store
        .plan_move(
            &g,
            &WirePath::parse("NOTE/Note/Work").unwrap(),
            &WirePath::parse("NOTE/Note/Play").unwrap(),
            Origin::Cli,
            None,
        )
        .unwrap();
    let _ = store.commit_until(&g, c, &s.state.journal, Some(CrashPoint::BeforeAppend));
    drop(g);
    let v = s
        .post(LIST_V3, &a, json!({"id": id, "recursive": false}))
        .await;
    assert_ok(&v);
    assert_eq!(paths(&v), ["NOTE/Note/Play/Plan.note"]);
}
