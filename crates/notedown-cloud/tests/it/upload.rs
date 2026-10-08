//! Uploads in accept mode: `upload/apply`, `/api/oss/upload`, `/api/oss/upload/part` and
//! `upload/finish` (`docs/sync-protocol.md` 3.5, 7.3), the recycle bin of protect mode, the
//! staging sweep, and the extended signed download checks. Requests go through the router in
//! memory (`tower::oneshot`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use md5::{Digest, Md5};
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::journal::Origin;
use notedown_cloud::server::{AppState, router};
use notedown_cloud::store::{
    CrashPoint, PendingOp, PutOptions, RECYCLE_INDEX, Recovery, RecycleEntry, RecycleReason, Store,
    WirePath,
};
use notedown_cloud::sync::SignedTarget;
use notedown_cloud::sync::staging::{self, STAGING_MAX_AGE_MS};
use notedown_cloud::{Clock, Config, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const SERIAL: &str = "SN078C10000000";
const OTHER: &str = "SN078C10000001";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;

const START: &str = "/api/file/2/files/synchronous/start";
const END: &str = "/api/file/2/files/synchronous/end";
const LIST: &str = "/api/file/2/files/list_folder";
const APPLY: &str = "/api/file/3/files/upload/apply";
const FINISH: &str = "/api/file/2/files/upload/finish";
const DOWNLOAD: &str = "/api/file/3/files/download_v3";
const CAPACITY: &str = "/api/file/capacity/query";
const UPLOAD: &str = "/api/oss/upload";
const PART: &str = "/api/oss/upload/part";
const BY_PATH: &str = "/api/file/3/files/query/by/path_v3";
const CREATE: &str = "/api/file/2/files/create_folder_v2";

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
    body: Vec<u8>,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn md5_of(b: &[u8]) -> String {
    const_hex::encode(Md5::digest(b))
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn test1() -> Vec<u8> {
    std::fs::read(root().join("tests/fixtures/notes/test1.note")).unwrap()
}

/// A multipart body with these extra text fields, then the `file` field.
fn multipart_with(fields: &[(&str, Vec<u8>)], name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let b = "notedownTestBoundary7MA4YWxk";
    let mut body = Vec::new();
    for (field, value) in fields {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(
        format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

/// A multipart body with one `file` field (and an extra text field before it).
fn multipart(name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let b = "notedownTestBoundary7MA4YWxk";
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhello\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

fn path_and_query(url: &str) -> String {
    let rest = url.split_once("://").unwrap().1;
    rest[rest.find('/').unwrap()..].to_string()
}

fn query_value(url: &str, key: &str) -> String {
    url.split_once('?')
        .unwrap()
        .1
        .split('&')
        .find_map(|p| p.strip_prefix(&format!("{key}=")))
        .unwrap()
        .to_string()
}

/// The account as after an import: the native folders exist (with ids) and `account.json`
/// carries the `imported` marker. A store with the marker must keep its tree, and an account
/// without marker and without files gets no sync, so most tests start here.
fn seed_imported(data: &std::path::Path) {
    let store = notedown_cloud::store::Store::open(data, "jordi").unwrap();
    {
        let g = store.lock().unwrap();
        store.ensure_native_folders(&g).unwrap();
    }
    let accounts = notedown_cloud::auth::Accounts::new(data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.imported = Some(notedown_cloud::auth::Imported {
        at: "2026-10-06T10:00:00Z".into(),
        source: "/backup/Supernote".into(),
        files: 0,
        bytes: 0,
    });
    accounts.save(&a).unwrap();
}

/// The internal id of an upload: signed into the URLs of `upload/apply`, it names the staging
/// entry (`innerName` in the answer is the file name).
fn internal_id(apply: &Value) -> String {
    let url = apply["fullUploadUrl"].as_str().unwrap();
    SignedTarget::decode(&query_value(url, "path"))
        .unwrap()
        .inner
        .unwrap()
}

fn assert_ok(v: &Value) {
    assert_eq!(v["success"], true, "{v}");
    assert!(v["errorCode"].is_null(), "{v}");
}

/// The object has exactly these keys (the envelope's map does not keep an order).
fn assert_keys(v: &Value, keys: &[&str]) {
    let mut have: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let mut want = keys.to_vec();
    have.sort_unstable();
    want.sort_unstable();
    assert_eq!(have, want, "{v}");
}

fn assert_err(v: &Value, code: &str) {
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(v["errorCode"], code, "{v}");
}

impl Server {
    fn new() -> Server {
        let s = Self::with_config(Config {
            protect: false,
            ..Config::default()
        });
        seed_imported(s.data());
        s
    }

    /// Protect mode with the import marker set (the gate is open, overwrites are recycled).
    fn protected() -> Server {
        let s = Self::with_config(Config {
            protect: true,
            ..Config::default()
        });
        seed_imported(s.data());
        s
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

    fn account(&self) -> PathBuf {
        self.data().join("accounts/jordi")
    }

    fn files(&self) -> PathBuf {
        self.account().join("files")
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
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Raw { status, body }
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
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.text());
        r.json()
    }

    /// POST a multipart `file` to a signed upload URL (path and query of it, plus extra query).
    async fn send(&self, url: &str, extra: &str, name: &str, bytes: &[u8]) -> Raw {
        let (ct, body) = multipart(name, bytes);
        let uri = format!("{}{extra}", path_and_query(url));
        self.raw("POST", &uri, &[("content-type", &ct)], Body::from(body))
            .await
    }

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

    /// Log in and take the sync lock.
    async fn session(&self) -> String {
        let t = self.login(SERIAL).await;
        assert_ok(&self.post(START, &t, json!({"equipmentNo": SERIAL})).await);
        t
    }

    async fn apply(&self, t: &str, path: &str, name: &str, size: u64) -> Value {
        self.post(
            APPLY,
            t,
            json!({"equipmentNo": SERIAL, "path": path, "fileName": name, "size": size}),
        )
        .await
    }

    async fn finish(&self, t: &str, parent: &str, name: &str, inner: &str, bytes: &[u8]) -> Value {
        self.post(
            FINISH,
            t,
            json!({"equipmentNo": SERIAL, "path": parent, "fileName": name,
                   "content_hash": md5_of(bytes), "innerName": inner,
                   "size": bytes.len().to_string()}),
        )
        .await
    }

    /// apply, single upload, finish; returns the finish answer.
    async fn upload(&self, t: &str, dir: &str, name: &str, bytes: &[u8]) -> Value {
        let a = self
            .apply(t, &format!("/{dir}/{name}"), name, bytes.len() as u64)
            .await;
        assert_ok(&a);
        let up = self
            .send(a["fullUploadUrl"].as_str().unwrap(), "", name, bytes)
            .await;
        assert_eq!(up.status, StatusCode::OK, "{}", up.text());
        assert_ok(&up.json());
        self.finish(
            t,
            &format!("/{dir}/"),
            name,
            a["innerName"].as_str().unwrap(),
            bytes,
        )
        .await
    }

    fn events(&self) -> Vec<Value> {
        self.state
            .journal
            .read_from(1)
            .unwrap()
            .map(|e| serde_json::from_str(&e.unwrap().to_line()).unwrap())
            .collect()
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

    fn requests_to(&self, path: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["kind"] == "request" && e["path"] == path)
            .collect()
    }

    /// Staging entries that hold anything besides the apply record (bytes, parts, state).
    fn staging_entries(&self) -> Vec<String> {
        match std::fs::read_dir(self.account().join("staging")) {
            Ok(d) => d
                .map(|e| e.unwrap())
                .filter(|e| {
                    std::fs::read_dir(e.path()).is_ok_and(|mut files| {
                        files.any(|f| f.unwrap().file_name() != staging::APPLY_FILE)
                    })
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

// ------------------------------------------------------------------ single uploads

#[tokio::test]
async fn a_new_note_under_a_new_folder_lands_with_folder_file_and_page_events() {
    let s = Server::new();
    let t = s.session().await;
    let note = test1();
    let a = s
        .apply(
            &t,
            "/NOTE/Note/Work/Meeting.note",
            "Meeting.note",
            note.len() as u64,
        )
        .await;
    assert_ok(&a);
    assert_keys(
        &a,
        &[
            "success",
            "errorCode",
            "errorMsg",
            "equipmentNo",
            "bucketName",
            "innerName",
            "xamzDate",
            "authorization",
            "fullUploadUrl",
            "partUploadUrl",
        ],
    );
    assert_eq!(a["bucketName"], "Meeting.note");
    assert_eq!(a["authorization"], "");
    // As in Ratta's traffic, innerName is the file name; the internal id is in the URLs.
    assert_eq!(a["innerName"], "Meeting.note");
    let inner = internal_id(&a);
    assert!(staging::valid_inner_name(&inner), "{inner}");
    assert_eq!(a["xamzDate"], T0.to_string());
    let full = a["fullUploadUrl"].as_str().unwrap();
    assert!(
        full.starts_with("http://192.168.1.10:19072/api/oss/upload?path="),
        "{full}"
    );
    assert!(
        a["partUploadUrl"]
            .as_str()
            .unwrap()
            .starts_with("http://192.168.1.10:19072/api/oss/upload/part?path=")
    );
    // The signed target: account, device, generation, the parent folder, innerName, size.
    let target = SignedTarget::decode(&query_value(full, "path")).unwrap();
    assert_eq!(target.account, "jordi");
    assert_eq!(target.device, SERIAL);
    assert_eq!(target.path, "NOTE/Note/Work");
    assert_eq!(target.inner.as_deref(), Some(inner.as_str()));
    assert_eq!(target.size, note.len() as u64);
    assert_eq!(target.id, None);

    let up = s.send(full, "", "Meeting.note", &note).await;
    let v = up.json();
    assert_ok(&v);
    assert_eq!(v["innerName"], inner.as_str());
    assert_eq!(v["md5"], md5_of(&note));
    assert!(
        s.account()
            .join("staging")
            .join(&inner)
            .join("data")
            .is_file()
    );

    let before = s.state.journal.last_seq().unwrap();
    let f = s
        .finish(&t, "/NOTE/Note/Work/", "Meeting.note", &inner, &note)
        .await;
    assert_ok(&f);
    assert_keys(
        &f,
        &[
            "success",
            "errorCode",
            "errorMsg",
            "equipmentNo",
            "path_display",
            "id",
            "size",
            "name",
            "content_hash",
        ],
    );
    assert_eq!(f["path_display"], "NOTE/Note/Work/Meeting.note");
    assert!(f["id"].is_string(), "{f}");
    assert_eq!(f["size"], note.len() as u64);
    assert_eq!(f["name"], "Meeting.note");
    assert_eq!(f["content_hash"], md5_of(&note));
    assert_eq!(
        std::fs::read(s.files().join("NOTE/Note/Work/Meeting.note")).unwrap(),
        note
    );
    assert!(s.staging_entries().is_empty(), "{:?}", s.staging_entries());

    let new: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap() > before)
        .collect();
    let ty = types(&new);
    assert_eq!(ty[0], "folder.created", "{ty:?}");
    assert_eq!(new[0]["path"], "NOTE/Note/Work");
    assert_eq!(ty[1], "file.created", "{ty:?}");
    assert_eq!(new[1]["path"], "NOTE/Note/Work/Meeting.note");
    assert_eq!(new[1]["id"].to_string(), f["id"].as_str().unwrap());
    let pages = new[1]["pagesChanged"].as_u64().unwrap();
    assert!(pages > 0, "{}", new[1]);
    assert_eq!(ty.len() as u64, 2 + pages, "{ty:?}");
    assert!(ty[2..].iter().all(|t| t == "page.added"), "{ty:?}");
    // One commit: one opId, origin device, the device serial.
    for e in &new {
        assert_eq!(e["opId"], new[0]["opId"]);
        assert_eq!(e["origin"], "device");
        assert_eq!(e["device"], SERIAL);
    }
    // The listing shows the folder and the file with the same id.
    let l = s
        .post(
            LIST,
            &t,
            json!({"path": "/NOTE/Note/Work", "recursive": false}),
        )
        .await;
    assert_eq!(l["entries"][0]["id"], f["id"]);
    assert_eq!(l["entries"][0]["content_hash"], md5_of(&note));
}

#[tokio::test]
async fn a_replace_keeps_the_id_and_recycles_the_old_bytes_in_protect_mode() {
    let s = Server::protected();
    let t = s.session().await;
    let first = s.upload(&t, "NOTE/Note", "a.note", b"first version").await;
    assert_ok(&first);
    s.advance_ms(5_000);
    let before = s.state.journal.last_seq().unwrap();
    let second = s.upload(&t, "NOTE/Note", "a.note", b"second version").await;
    assert_ok(&second);
    assert_eq!(second["id"], first["id"]);
    assert_eq!(
        std::fs::read(s.files().join("NOTE/Note/a.note")).unwrap(),
        b"second version"
    );
    let new: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap() > before)
        .collect();
    assert_eq!(types(&new)[0], "file.modified", "{new:?}");
    assert_eq!(new[0]["prevMd5"], md5_of(b"first version"));
    assert_eq!(new[0]["md5"], md5_of(b"second version"));
    assert_eq!(new[0]["id"].to_string(), first["id"].as_str().unwrap());
    // The old bytes are in recycle/<ms>/<path>, with an index line.
    let at = (T0 + 5_000) as u64;
    let kept = s.account().join(format!("recycle/{at}/NOTE/Note/a.note"));
    assert_eq!(std::fs::read(&kept).unwrap(), b"first version");
    let index = std::fs::read_to_string(s.account().join("recycle").join(RECYCLE_INDEX)).unwrap();
    let id: u64 = first["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        index,
        format!(
            "{{\"id\":{id},\"path\":\"NOTE/Note/a.note\",\"md5\":\"{}\",\"deletedAt\":{at},\"reason\":\"overwritten\"}}\n",
            md5_of(b"first version")
        )
    );
    // The recycle bin is never listed.
    let l = s
        .post(LIST, &t, json!({"path": "/", "recursive": true}))
        .await;
    assert!(!l.to_string().contains("recycle"), "{l}");
    // More overwrites in the same ms take the next slots.
    s.upload(&t, "NOTE/Note", "a.note", b"third").await;
    s.upload(&t, "NOTE/Note", "a.note", b"fourth").await;
    let slot = |ms: u64| {
        std::fs::read(s.account().join(format!("recycle/{ms}/NOTE/Note/a.note"))).unwrap()
    };
    assert_eq!(slot(at), b"first version");
    assert_eq!(slot(at + 1), b"second version");
    assert_eq!(slot(at + 2), b"third");
    let index = std::fs::read_to_string(s.account().join("recycle").join(RECYCLE_INDEX)).unwrap();
    assert_eq!(index.lines().count(), 3);
}

#[tokio::test]
async fn a_replace_without_protect_keeps_no_copy() {
    let s = Server::new();
    let t = s.session().await;
    let first = s.upload(&t, "INBOX", "x.pdf", b"one").await;
    let second = s.upload(&t, "INBOX", "x.pdf", b"two").await;
    assert_eq!(first["id"], second["id"]);
    assert!(!s.account().join("recycle").exists());
}

/// A device upload over an existing file leaves no stale MD5 behind: a scan afterwards finds
/// nothing to journal.
#[tokio::test]
async fn a_scan_after_a_replace_finds_nothing() {
    let s = Server::new();
    let t = s.session().await;
    s.upload(&t, "INBOX", "x.pdf", b"one").await;
    s.upload(&t, "INBOX", "x.pdf", b"two, longer").await;
    let before = s.events().len();
    let store = Store::open(s.data(), "jordi").unwrap();
    let g = store.lock_recovered(&s.state.journal).unwrap();
    let report = store
        .scan(
            &g,
            &s.state.journal,
            &notedown_cloud::store::ScanOptions::all(1),
            s.state.now_ms(),
        )
        .unwrap();
    assert_eq!(report, notedown_cloud::store::ScanReport::default());
    assert_eq!(s.events().len(), before);
}

#[tokio::test]
async fn finish_joins_the_parent_folder_and_the_name_and_takes_the_size_as_string() {
    let s = Server::new();
    let t = s.session().await;
    let bytes = b"work note";
    let a = s
        .apply(&t, "/NOTE/Note/Work/x.note", "x.note", bytes.len() as u64)
        .await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "x.note", bytes)
        .await;
    // The device form: parent with a trailing slash, size as a string.
    let f = s
        .post(
            FINISH,
            &t,
            json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work/", "fileName": "x.note",
                   "content_hash": md5_of(bytes), "innerName": a["innerName"],
                   "size": "9"}),
        )
        .await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "NOTE/Note/Work/x.note");
    assert_eq!(f["size"], 9);
    assert!(s.files().join("NOTE/Note/Work/x.note").is_file());
    assert!(!s.files().join("NOTE/Note/Work/x.note/x.note").exists());
    assert!(s.files().join("NOTE/Note/Work").is_dir());

    // A double slash and a number size work too; the full path form (no trailing slash,
    // ending in the name) never nests the name.
    for (path, size) in [
        (json!("NOTE//Note/Work/"), json!(1)),
        (json!("/NOTE/Note/Work/y.note"), json!("1")),
    ] {
        let a = s.apply(&t, "/NOTE/Note/Work/y.note", "y.note", 1).await;
        s.send(a["fullUploadUrl"].as_str().unwrap(), "", "y.note", b"y")
            .await;
        let f = s
            .post(
                FINISH,
                &t,
                json!({"path": path, "fileName": "y.note", "content_hash": md5_of(b"y"),
                       "innerName": a["innerName"], "size": size}),
            )
            .await;
        assert_ok(&f);
        assert_eq!(f["path_display"], "NOTE/Note/Work/y.note");
    }
    assert!(!s.files().join("NOTE/Note/Work/y.note/y.note").exists());

    // apply: the full path, a double slash, or just the folder all name the same target.
    for path in [
        "/NOTE/Note/Work/z.note",
        "NOTE//Note/Work/z.note",
        "/NOTE/Note/Work/",
    ] {
        let a = s.apply(&t, path, "z.note", 1).await;
        let target =
            SignedTarget::decode(&query_value(a["fullUploadUrl"].as_str().unwrap(), "path"))
                .unwrap();
        assert_eq!(target.path, "NOTE/Note/Work", "{path}");
    }
}

#[tokio::test]
async fn a_hash_or_size_mismatch_keeps_nothing() {
    let s = Server::new();
    let t = s.session().await;
    let before = s.state.journal.last_seq().unwrap();
    for (hash, size) in [(md5_of(b"other"), "4"), (md5_of(b"data"), "5")] {
        let a = s.apply(&t, "/NOTE/Note/New/m.note", "m.note", 4).await;
        s.send(a["fullUploadUrl"].as_str().unwrap(), "", "m.note", b"data")
            .await;
        let f = s
            .post(
                FINISH,
                &t,
                json!({"path": "/NOTE/Note/New/", "fileName": "m.note", "content_hash": hash,
                       "innerName": a["innerName"], "size": size}),
            )
            .await;
        assert_err(&f, "E0324");
    }
    assert!(!s.files().join("NOTE/Note/New").exists());
    assert_eq!(s.state.journal.last_seq().unwrap(), before);
    assert!(s.staging_entries().is_empty());
    let flags: Vec<String> = s
        .requests_to(FINISH)
        .iter()
        .map(|e| e["flags"].to_string())
        .collect();
    assert!(flags[0].contains("hash_mismatch"), "{flags:?}");
    assert!(flags[1].contains("size_mismatch"), "{flags:?}");
    // An unknown or malformed innerName.
    for inner in [
        json!("00000000-0000-4000-8000-000000000000"),
        json!("../x"),
        json!(5),
    ] {
        let f = s
            .post(
                FINISH,
                &t,
                json!({"path": "/INBOX/", "fileName": "q", "content_hash": md5_of(b""),
                       "innerName": inner, "size": "0"}),
            )
            .await;
        assert_err(&f, "E0324");
    }
}

#[tokio::test]
async fn an_identical_upload_answers_the_entry_and_journals_nothing() {
    let s = Server::new();
    let t = s.session().await;
    let first = s.upload(&t, "INBOX", "same.pdf", b"same").await;
    let before = s.state.journal.last_seq().unwrap();
    let again = s.upload(&t, "INBOX", "same.pdf", b"same").await;
    assert_ok(&again);
    assert_eq!(again["id"], first["id"]);
    assert_eq!(s.state.journal.last_seq().unwrap(), before);
    assert!(s.staging_entries().is_empty());
}

#[tokio::test]
async fn names_are_checked_before_anything_is_signed() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/Plan.note", b"p");
    // A case variant of the file or of a folder on the way is that entry (3.0).
    assert_ok(&s.apply(&t, "/NOTE/Note/plan.note", "plan.note", 1).await);
    assert_ok(&s.apply(&t, "/note/Note/x.note", "x.note", 1).await);
    // A case variant of a folder at the target stays taken.
    assert_err(&s.apply(&t, "/NOTE/note", "note", 1).await, "E0322");
    // A dot-name and a name that is no single segment.
    assert_err(
        &s.apply(&t, "/NOTE/Note/.hidden", ".hidden", 1).await,
        "E0324",
    );
    assert_err(&s.apply(&t, "/NOTE/Note/", "a/b", 1).await, "E0324");
    // A folder at the target, a file in place of a folder.
    assert_err(&s.apply(&t, "/NOTE/Note", "Note", 1).await, "E0322");
    assert_err(
        &s.apply(&t, "/NOTE/Note/Plan.note/x.note", "x.note", 1)
            .await,
        "E0324",
    );
    // Too large for FILE_MAX.
    assert_err(
        &s.apply(&t, "/INBOX/big.pdf", "big.pdf", (1u64 << 30) + 1)
            .await,
        "E0324",
    );

    // The target is looked up again at finish: a case variant that appeared meanwhile is
    // replaced and keeps its spelling.
    let a = s.apply(&t, "/NOTE/Note/Late.note", "Late.note", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "Late.note", b"l")
        .await;
    s.put("NOTE/Note/late.note", b"other");
    let f = s
        .finish(
            &t,
            "/NOTE/Note/",
            "Late.note",
            a["innerName"].as_str().unwrap(),
            b"l",
        )
        .await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "NOTE/Note/late.note");
    let names = names_in(&s.files().join("NOTE/Note"));
    assert!(names.contains(&"late.note".to_string()), "{names:?}");
    assert!(!names.contains(&"Late.note".to_string()), "{names:?}");
    // A folder that appeared meanwhile under a case variant of the name is refused.
    let a = s.apply(&t, "/INBOX/Box", "Box", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "Box", b"b")
        .await;
    std::fs::create_dir(s.files().join("INBOX/box")).unwrap();
    let f = s
        .finish(&t, "/INBOX/", "Box", a["innerName"].as_str().unwrap(), b"b")
        .await;
    assert_err(&f, "E0322");
}

/// The names in a folder, byte-exact and sorted (a case-insensitive file system answers
/// `exists` for any spelling).
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every entry of `staging/`, also those that hold only the apply record.
fn all_staging(s: &Server) -> Vec<String> {
    match std::fs::read_dir(s.account().join("staging")) {
        Ok(d) => d
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[tokio::test]
async fn an_upload_over_a_case_variant_replaces_it_and_keeps_id_and_spelling() {
    let s = Server::protected();
    let t = s.session().await;
    let first = s.upload(&t, "INBOX", "Report.pdf", b"first").await;
    assert_ok(&first);
    s.advance_ms(5_000);
    let before = s.state.journal.last_seq().unwrap();
    let second = s.upload(&t, "INBOX", "report.PDF", b"second").await;
    assert_ok(&second);
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["path_display"], "INBOX/Report.pdf");
    assert_eq!(second["name"], "Report.pdf");
    assert_eq!(names_in(&s.files().join("INBOX")), ["Report.pdf"]);
    assert_eq!(
        std::fs::read(s.files().join("INBOX/Report.pdf")).unwrap(),
        b"second"
    );
    let new: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap() > before)
        .collect();
    assert_eq!(types(&new), ["file.modified"], "{new:?}");
    assert_eq!(new[0]["path"], "INBOX/Report.pdf");
    assert_eq!(new[0]["prevMd5"], md5_of(b"first"));
    assert_eq!(new[0]["id"].to_string(), first["id"].as_str().unwrap());
    // The old bytes are in the recycle bin under the stored spelling.
    let at = (T0 + 5_000) as u64;
    assert_eq!(
        names_in(&s.account().join(format!("recycle/{at}/INBOX"))),
        ["Report.pdf"]
    );
    let index = std::fs::read_to_string(s.account().join("recycle").join(RECYCLE_INDEX)).unwrap();
    assert!(index.contains("\"path\":\"INBOX/Report.pdf\""), "{index}");
    assert_eq!(index.lines().count(), 1);
    // The listing shows one file with the first spelling.
    let l = s
        .post(LIST, &t, json!({"path": "/INBOX", "recursive": false}))
        .await;
    assert_eq!(l["entries"].as_array().unwrap().len(), 1, "{l}");
    assert_eq!(l["entries"][0]["path_display"], "INBOX/Report.pdf");

    // The same lock rules as any overwrite: another device's session refuses it.
    assert_ok(
        &s.post(END, &t, json!({"equipmentNo": SERIAL, "flag": "Y"}))
            .await,
    );
    let other = s.login(OTHER).await;
    assert_ok(&s.post(START, &other, json!({"equipmentNo": OTHER})).await);
    assert_err(
        &s.apply(&t, "/INBOX/REPORT.pdf", "REPORT.pdf", 1).await,
        "E0079",
    );
    assert_eq!(
        std::fs::read(s.files().join("INBOX/Report.pdf")).unwrap(),
        b"second"
    );
}

#[tokio::test]
async fn a_note_over_a_case_variant_indexes_its_pages_under_the_stored_path() {
    let s = Server::new();
    let t = s.session().await;
    let first = s.upload(&t, "NOTE/Note", "Bike.note", &test1()).await;
    assert_ok(&first);
    let id = first["id"].as_str().unwrap().to_string();
    let before = s.state.journal.last_seq().unwrap();
    let second = std::fs::read(root().join("tests/fixtures/notes/test2.note")).unwrap();
    let f = s.upload(&t, "note/note", "bike.NOTE", &second).await;
    assert_ok(&f);
    assert_eq!(f["id"], id.as_str());
    assert_eq!(f["path_display"], "NOTE/Note/Bike.note");
    assert_eq!(names_in(&s.files().join("NOTE/Note")), ["Bike.note"]);
    let new: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap() > before)
        .collect();
    let ty = types(&new);
    assert_eq!(ty[0], "file.modified", "{ty:?}");
    assert_eq!(new[0]["path"], "NOTE/Note/Bike.note");
    assert!(ty.len() > 1, "page events: {ty:?}");
    for e in &new[1..] {
        assert!(e["type"].as_str().unwrap().starts_with("page."), "{e}");
        assert_ne!(e["type"], "page.index_failed", "{e}");
        assert_eq!(e["id"].to_string(), id, "{e}");
    }
    // The page index has one record, under the stored path.
    let records: Vec<Value> = std::fs::read_dir(s.account().join("pages"))
        .unwrap()
        .map(|e| serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["path"], "NOTE/Note/Bike.note");
}

#[tokio::test]
async fn a_case_variant_upload_into_a_case_variant_folder_is_one_file() {
    let s = Server::new();
    let t = s.session().await;
    let first = s.upload(&t, "NOTE/Note/Work", "report.pdf", b"one").await;
    assert_ok(&first);
    let before = s.state.journal.last_seq().unwrap();
    let second = s.upload(&t, "note/NOTE/work", "REPORT.pdf", b"two").await;
    assert_ok(&second);
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["path_display"], "NOTE/Note/Work/report.pdf");
    assert_eq!(
        names_in(&s.files()),
        ["DOCUMENT", "EXPORT", "INBOX", "NOTE", "SCREENSHOT"]
    );
    assert_eq!(names_in(&s.files().join("NOTE")), ["MyStyle", "Note"]);
    assert_eq!(names_in(&s.files().join("NOTE/Note")), ["Work"]);
    assert_eq!(names_in(&s.files().join("NOTE/Note/Work")), ["report.pdf"]);
    let new: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap() > before)
        .collect();
    assert_eq!(types(&new), ["file.modified"], "{new:?}");
    assert_eq!(new[0]["path"], "NOTE/Note/Work/report.pdf");
    // A new file into a case variant of a folder lands in that folder, no new folder.
    let before = s.state.journal.last_seq().unwrap();
    let f = s.upload(&t, "inbox", "new.pdf", b"n").await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "INBOX/new.pdf");
    assert_eq!(names_in(&s.files().join("INBOX")), ["new.pdf"]);
    let new: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["seq"].as_u64().unwrap() > before)
        .collect();
    assert_eq!(types(&new), ["file.created"], "{new:?}");
}

#[tokio::test]
async fn an_nfd_upload_over_the_nfc_name_replaces_it() {
    let s = Server::new();
    let t = s.session().await;
    let nfc = "Caf\u{e9}.pdf";
    let nfd = "Cafe\u{301}.pdf";
    let first = s.upload(&t, "INBOX", nfc, b"one").await;
    assert_ok(&first);
    let second = s.upload(&t, "INBOX", nfd, b"two").await;
    assert_ok(&second);
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["name"], nfc);
    assert_eq!(names_in(&s.files().join("INBOX")), [nfc]);
}

#[tokio::test]
async fn a_name_over_255_bytes_is_refused_before_anything_is_written() {
    let s = Server::new();
    let t = s.session().await;
    // 255 bytes pass and land.
    let ok = format!("{}.pdf", "a".repeat(251));
    assert_eq!(ok.len(), 255);
    let f = s.upload(&t, "INBOX", &ok, b"x").await;
    assert_ok(&f);
    assert_eq!(f["name"], ok.as_str());
    let staged = all_staging(&s);
    // 256 bytes fail, also as 128 characters, and also for a folder on the way.
    let long = format!("{}.pdf", "a".repeat(252));
    let wide = "\u{e9}".repeat(128);
    assert_eq!(wide.len(), 256);
    for (path, name) in [
        (format!("/INBOX/{long}"), long.clone()),
        (format!("/INBOX/{wide}"), wide.clone()),
        (format!("/INBOX/{wide}/x.pdf"), "x.pdf".to_string()),
    ] {
        let a = s.apply(&t, &path, &name, 1).await;
        assert_err(&a, "E0324");
        let e = s.requests_to(APPLY).pop().unwrap();
        assert!(e["flags"].to_string().contains("name_too_long"), "{e}");
    }
    assert_eq!(all_staging(&s), staged);
    assert_eq!(names_in(&s.files().join("INBOX")), [ok]);
    // A name over 255 bytes that exists already passes: nothing new is named (APFS takes 255
    // characters, so such a folder can be there).
    if std::fs::create_dir(s.files().join(format!("INBOX/{wide}"))).is_ok() {
        let f = s.upload(&t, &format!("INBOX/{wide}"), "y.pdf", b"y").await;
        assert_ok(&f);
        assert_eq!(f["path_display"], format!("INBOX/{wide}/y.pdf"));
    }
}

/// Needs a case-sensitive temp file system (the recipe is in `store.rs`).
#[tokio::test]
#[ignore = "needs a case-sensitive file system"]
async fn two_stored_case_variants_are_never_picked_silently() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"lower");
    s.put("INBOX/A.pdf", b"upper");
    assert_eq!(
        names_in(&s.files().join("INBOX")),
        ["A.pdf", "a.pdf"],
        "TMPDIR must be on a case-sensitive file system"
    );
    std::fs::create_dir(s.files().join("INBOX/Work")).unwrap();
    std::fs::create_dir(s.files().join("INBOX/work")).unwrap();
    let flagged = |path: &str| {
        let e = s.requests_to(path).pop().unwrap();
        assert!(e["flags"].to_string().contains("case_ambiguous"), "{e}");
    };
    // No exact match and two fold matches: E0322 with the flag, nothing changed.
    assert_err(&s.apply(&t, "/INBOX/a.PDF", "a.PDF", 1).await, "E0322");
    flagged(APPLY);
    assert_err(&s.apply(&t, "/INBOX/WORK/x.pdf", "x.pdf", 1).await, "E0322");
    flagged(APPLY);
    assert_err(
        &s.post(LIST, &t, json!({"path": "/INBOX/WORK"})).await,
        "E0322",
    );
    flagged(LIST);
    assert_err(
        &s.post(BY_PATH, &t, json!({"path": "/INBOX/A.PDF"})).await,
        "E0322",
    );
    flagged(BY_PATH);
    assert_err(
        &s.post(
            CREATE,
            &t,
            json!({"path": "/INBOX/WORK/sub", "autorename": false}),
        )
        .await,
        "E0322",
    );
    flagged(CREATE);
    assert!(all_staging(&s).is_empty());
    // The exact match wins.
    let f = s.upload(&t, "INBOX", "a.pdf", b"new").await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "INBOX/a.pdf");
    assert_eq!(
        std::fs::read(s.files().join("INBOX/a.pdf")).unwrap(),
        b"new"
    );
    assert_eq!(
        std::fs::read(s.files().join("INBOX/A.pdf")).unwrap(),
        b"upper"
    );
    let q = s.post(BY_PATH, &t, json!({"path": "/INBOX/Work"})).await;
    assert_eq!(q["entriesVO"]["path_display"], "INBOX/Work");
    assert_ok(&s.post(LIST, &t, json!({"path": "/INBOX/work"})).await);
}

// ------------------------------------------------------------------ chunked uploads

#[tokio::test]
async fn three_parts_out_of_order_with_a_retry_merge_once() {
    let s = Server::new();
    let t = s.session().await;
    let parts: [&[u8]; 3] = [b"part one|", b"part two|", b"part three"];
    let all: Vec<u8> = parts.concat();
    let a = s
        .apply(
            &t,
            "/DOCUMENT/Document/big.pdf",
            "big.pdf",
            all.len() as u64,
        )
        .await;
    let url = a["partUploadUrl"].as_str().unwrap();
    let inner = &internal_id(&a);
    let id = "1791270000000_0a1b2c3d-1111-4222-8333-444455556666";
    let send = |n: usize| {
        let extra = format!("&uploadId={id}&partNumber={n}&totalChunks=3");
        let bytes = parts[n - 1];
        let s = &s;
        async move { s.send(url, &extra, "big.pdf", bytes).await.json() }
    };
    let r = send(3).await;
    assert_ok(&r);
    assert_keys(
        &r,
        &[
            "success",
            "errorCode",
            "errorMsg",
            "uploadId",
            "partNumber",
            "totalChunks",
            "chunkMd5",
            "status",
        ],
    );
    assert_eq!(r["uploadId"], id);
    assert_eq!(r["partNumber"], 3);
    assert_eq!(r["totalChunks"], 3);
    assert_eq!(r["chunkMd5"], md5_of(parts[2]));
    assert_eq!(r["status"], "uploading");
    assert_eq!(send(1).await["status"], "uploading");
    // A retry of part 1.
    assert_eq!(send(1).await["status"], "uploading");
    let entry = s.account().join("staging").join(inner);
    assert!(entry.join("part-1").is_file() && entry.join("part-3").is_file());
    assert!(!entry.join("data").exists());
    assert_eq!(send(2).await["status"], "merged");
    assert_eq!(std::fs::read(entry.join("data")).unwrap(), all);
    assert!(!entry.join("part-1").exists() && !entry.join("part-2").exists());
    // A late retry after the merge changes nothing.
    assert_eq!(send(2).await["status"], "merged");
    assert_eq!(std::fs::read(entry.join("data")).unwrap(), all);

    let f = s
        .finish(&t, "/DOCUMENT/Document/", "big.pdf", inner, &all)
        .await;
    assert_ok(&f);
    assert_eq!(
        std::fs::read(s.files().join("DOCUMENT/Document/big.pdf")).unwrap(),
        all
    );
}

#[tokio::test]
async fn a_missing_part_means_no_merge_and_finish_refuses() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/p.pdf", "p.pdf", 6).await;
    let url = a["partUploadUrl"].as_str().unwrap();
    let inner = &internal_id(&a);
    for n in [1, 3] {
        let r = s
            .send(
                url,
                &format!("&uploadId=u1&partNumber={n}&totalChunks=3"),
                "p.pdf",
                b"ab",
            )
            .await
            .json();
        assert_eq!(r["status"], "uploading");
    }
    assert!(
        !s.account()
            .join("staging")
            .join(inner)
            .join("data")
            .exists()
    );
    let f = s.finish(&t, "/INBOX/", "p.pdf", inner, b"ababab").await;
    assert_err(&f, "E0324");
    assert!(!s.files().join("INBOX/p.pdf").exists());
    // Parts that do not fit: a number out of range, another uploadId or totalChunks.
    for extra in [
        "&uploadId=u1&partNumber=4&totalChunks=3",
        "&uploadId=u1&partNumber=0&totalChunks=3",
        "&uploadId=u2&partNumber=2&totalChunks=3",
        "&uploadId=u1&partNumber=2&totalChunks=4",
        "&uploadId=../u1&partNumber=2&totalChunks=3",
        "&partNumber=2&totalChunks=3",
    ] {
        let r = s.send(url, extra, "p.pdf", b"ab").await.json();
        assert_err(&r, "E0324");
    }
    assert!(
        !s.account()
            .join("staging")
            .join(inner)
            .join("data")
            .exists()
    );
}

#[tokio::test]
async fn limits_refuse_large_parts_and_files() {
    let s = Server::new();
    let t = s.session().await;
    s.state.set_upload_limits(10, 25);
    // apply refuses an announced size above the limit.
    assert_err(&s.apply(&t, "/INBOX/a.pdf", "a.pdf", 26).await, "E0324");
    // A single upload above the limit.
    let a = s.apply(&t, "/INBOX/a.pdf", "a.pdf", 20).await;
    let r = s
        .send(
            a["fullUploadUrl"].as_str().unwrap(),
            "",
            "a.pdf",
            &[7u8; 26],
        )
        .await;
    assert_err(&r.json(), "E0324");
    assert!(r.json().to_string().len() < 200);
    let entry = s.account().join("staging").join(internal_id(&a));
    assert!(!entry.join("data").exists());
    let left: Vec<_> = std::fs::read_dir(&entry)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != staging::APPLY_FILE)
        .collect();
    assert!(left.is_empty(), "no temp file is left: {left:?}");
    // At the limit it is fine.
    let r = s
        .send(
            a["fullUploadUrl"].as_str().unwrap(),
            "",
            "a.pdf",
            &[7u8; 25],
        )
        .await;
    assert_ok(&r.json());
    // A part above the part limit.
    let p = s.apply(&t, "/INBOX/b.pdf", "b.pdf", 20).await;
    let url = p["partUploadUrl"].as_str().unwrap();
    let r = s
        .send(
            url,
            "&uploadId=u&partNumber=1&totalChunks=3",
            "b.pdf",
            &[1u8; 11],
        )
        .await;
    assert_err(&r.json(), "E0324");
    // Parts within the part limit whose sum is above the file limit.
    for n in 1..=3 {
        let r = s
            .send(
                url,
                &format!("&uploadId=u&partNumber={n}&totalChunks=3"),
                "b.pdf",
                &[1u8; 10],
            )
            .await
            .json();
        if n < 3 {
            assert_eq!(r["status"], "uploading");
        } else {
            assert_err(&r, "E0324");
        }
    }
    let entry = s.account().join("staging").join(internal_id(&p));
    assert!(!entry.join("data").exists());
    assert!(!entry.join("part-1").exists());
}

#[tokio::test]
async fn the_real_limits_take_device_sized_parts_and_files_past_the_default_body_limits() {
    let s = Server::new();
    let t = s.session().await;
    // A single upload of 10 MiB: above axum's 2 MB default and the log's 8 MiB buffer.
    let big: Vec<u8> = (0..10 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let f = s.upload(&t, "DOCUMENT/Document", "big.pdf", &big).await;
    assert_ok(&f);
    assert_eq!(f["content_hash"], md5_of(&big));
    let e = s.requests_to(UPLOAD).pop().unwrap();
    let (_, body) = multipart("big.pdf", &big);
    assert_eq!(
        e["request_body"],
        format!("<{} bytes, not logged>", body.len())
    );
    // The device's part size (8 MiB) works; a part above 9 MiB does not.
    let a = s.apply(&t, "/INBOX/parts.pdf", "parts.pdf", 0).await;
    let url = a["partUploadUrl"].as_str().unwrap();
    let r = s
        .send(
            url,
            "&uploadId=u&partNumber=1&totalChunks=2",
            "parts.pdf",
            &big[..8 * 1024 * 1024],
        )
        .await;
    assert_eq!(r.json()["status"], "uploading", "{}", r.text());
    let r = s
        .send(
            url,
            "&uploadId=u&partNumber=2&totalChunks=2",
            "parts.pdf",
            &vec![1u8; 9 * 1024 * 1024 + 1],
        )
        .await;
    assert_err(&r.json(), "E0324");
}

#[test]
fn the_real_limits_are_9_mib_per_part_and_1_gib_per_file() {
    assert_eq!(staging::PART_MAX, 9 * 1024 * 1024);
    assert_eq!(staging::FILE_MAX, 1 << 30);
}

// ------------------------------------------------------------------ gates and signatures

#[tokio::test]
async fn every_upload_route_waits_while_another_device_syncs() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/l.pdf", "l.pdf", 1).await;
    assert_ok(&a);
    assert_ok(
        &s.post(END, &t, json!({"equipmentNo": SERIAL, "flag": "Y"}))
            .await,
    );
    let other = s.login(OTHER).await;
    assert_ok(&s.post(START, &other, json!({"equipmentNo": OTHER})).await);
    assert_err(&s.apply(&t, "/INBOX/l.pdf", "l.pdf", 1).await, "E0079");
    let r = s
        .send(a["fullUploadUrl"].as_str().unwrap(), "", "l.pdf", b"l")
        .await;
    assert_err(&r.json(), "E0079");
    let r = s
        .send(
            a["partUploadUrl"].as_str().unwrap(),
            "&uploadId=u&partNumber=1&totalChunks=1",
            "l.pdf",
            b"l",
        )
        .await;
    assert_err(&r.json(), "E0079");
    let f = s
        .finish(
            &t,
            "/INBOX/",
            "l.pdf",
            a["innerName"].as_str().unwrap(),
            b"l",
        )
        .await;
    assert_err(&f, "E0079");
    for path in [APPLY, UPLOAD, PART, FINISH] {
        let e = s.requests_to(path).pop().unwrap();
        assert!(e["flags"].to_string().contains("no_sync_lock"), "{e}");
        assert_eq!(e["device"], SERIAL, "{e}");
    }
    assert!(!s.files().join("INBOX/l.pdf").exists());
}

#[tokio::test]
async fn an_upload_without_a_session_takes_the_free_lock() {
    // Auto Sync sends a changed file without synchronous/start (query_v3, then apply).
    let s = Server::new();
    let t = s.session().await;
    assert_ok(
        &s.post(END, &t, json!({"equipmentNo": SERIAL, "flag": "Y"}))
            .await,
    );
    let a = s.apply(&t, "/INBOX/l.pdf", "l.pdf", 1).await;
    assert_ok(&a);
    let r = s
        .send(a["fullUploadUrl"].as_str().unwrap(), "", "l.pdf", b"l")
        .await;
    assert_ok(&r.json());
    let f = s
        .finish(
            &t,
            "/INBOX/",
            "l.pdf",
            a["innerName"].as_str().unwrap(),
            b"l",
        )
        .await;
    assert_ok(&f);
    assert_eq!(std::fs::read(s.files().join("INBOX/l.pdf")).unwrap(), b"l");
    assert_eq!(
        s.state
            .sync_locks
            .holder("jordi", s.state.now_ms())
            .as_deref(),
        Some(SERIAL)
    );
    let markers: Vec<Value> = s
        .log()
        .into_iter()
        .filter(|e| e["kind"] == "marker" && e["flags"].to_string().contains("implicit_lock"))
        .collect();
    assert_eq!(markers.len(), 1, "{markers:?}");
}

#[tokio::test]
async fn bad_or_stale_upload_signatures_get_plain_e1306() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/s.pdf", "s.pdf", 1).await;
    let url = a["fullUploadUrl"].as_str().unwrap().to_string();
    let sig = query_value(&url, "signature");
    let flipped = url.replace(
        &sig,
        &format!(
            "{}{}",
            if sig.starts_with('0') { "1" } else { "0" },
            &sig[1..]
        ),
    );
    // The upload signature does not work as a part URL of another target, nor tampered.
    let (ct, body) = multipart("s.pdf", b"s");
    for uri in [
        path_and_query(&flipped),
        "/api/oss/upload?path=abc".to_string(),
        "/api/oss/upload".to_string(),
    ] {
        let r = s
            .raw(
                "POST",
                &uri,
                &[("content-type", &ct)],
                Body::from(body.clone()),
            )
            .await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
        assert_eq!(r.text(), "E1306");
    }
    // Expired after 30 min.
    let a2 = s.apply(&t, "/INBOX/s.pdf", "s.pdf", 1).await;
    s.advance_ms(30 * 60 * 1000 + 1);
    s.post(START, &t, json!({})).await;
    let r = s
        .send(a2["fullUploadUrl"].as_str().unwrap(), "", "s.pdf", b"s")
        .await;
    assert_eq!(r.text(), "E1306");
    // A new login raises the token generation: older URLs stop working.
    let a3 = s.apply(&t, "/INBOX/s.pdf", "s.pdf", 1).await;
    let t2 = s.login(SERIAL).await;
    s.post(START, &t2, json!({})).await;
    let r = s
        .send(a3["fullUploadUrl"].as_str().unwrap(), "", "s.pdf", b"s")
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.text(), "E1306");
    let e = s.requests_to(UPLOAD).pop().unwrap();
    assert!(e["flags"].to_string().contains("stale_token"), "{e}");
    assert!(s.staging_entries().is_empty());
}

#[tokio::test]
async fn a_download_url_needs_the_same_id_at_the_path_and_a_current_generation() {
    let s = Server::new();
    let t = s.session().await;
    let f = s.upload(&t, "INBOX", "d.pdf", b"original").await;
    let d = s.post(DOWNLOAD, &t, json!({"id": f["id"]})).await;
    let url = d["url"].as_str().unwrap().to_string();
    let target = SignedTarget::decode(&query_value(&url, "path")).unwrap();
    assert_eq!(
        target.id.map(|i| i.to_string()),
        f["id"].as_str().map(str::to_string)
    );
    let r = s
        .raw("GET", &path_and_query(&url), &[], Body::empty())
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, b"original");

    // The file moves away and another file (another id) takes its path.
    let store = Store::open(s.data(), "jordi").unwrap();
    {
        let g = store.lock_recovered(&s.state.journal).unwrap();
        let from = WirePath::parse_named("INBOX/d.pdf").unwrap();
        let to = WirePath::parse_named("INBOX/moved.pdf").unwrap();
        let c = store.plan_move(&g, &from, &to, Origin::Cli, None).unwrap();
        store.commit(&g, c, &s.state.journal).unwrap();
    }
    let g = s.upload(&t, "INBOX", "d.pdf", b"someone else").await;
    assert_ne!(g["id"], f["id"]);
    let r = s
        .raw("GET", &path_and_query(&url), &[], Body::empty())
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.text(), "E1307");

    // A new login makes older download URLs stale.
    let d = s.post(DOWNLOAD, &t, json!({"id": g["id"]})).await;
    let url = d["url"].as_str().unwrap().to_string();
    s.login(SERIAL).await;
    let r = s
        .raw("GET", &path_and_query(&url), &[], Body::empty())
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.text(), "E1306");
    let e = s.requests_to("/api/oss/download").pop().unwrap();
    assert!(e["flags"].to_string().contains("stale_token"), "{e}");
}

// ------------------------------------------------------------------ listing, log, capacity

#[tokio::test]
async fn staged_data_is_never_listed_and_finish_updates_the_capacity() {
    let s = Server::new();
    let t = s.session().await;
    let used = |v: Value| v["usedCapacity"].as_u64().unwrap();
    let before = used(s.post(CAPACITY, &t, json!({})).await);
    let a = s.apply(&t, "/INBOX/st.pdf", "st.pdf", 5).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "st.pdf", b"stage")
        .await;
    let l = s
        .post(LIST, &t, json!({"path": "/", "recursive": true}))
        .await;
    assert!(!l.to_string().contains("st.pdf"), "{l}");
    assert!(!l.to_string().contains("staging"), "{l}");
    assert_eq!(used(s.post(CAPACITY, &t, json!({})).await), before);
    let f = s
        .finish(
            &t,
            "/INBOX/",
            "st.pdf",
            a["innerName"].as_str().unwrap(),
            b"stage",
        )
        .await;
    assert_ok(&f);
    // No 60 s wait: the cache was cleared.
    assert_eq!(used(s.post(CAPACITY, &t, json!({})).await), before + 5);
}

#[tokio::test]
async fn upload_bodies_are_streamed_and_logged_as_a_count_without_signatures() {
    let s = Server::new();
    let t = s.session().await;
    let bytes = vec![b'x'; 700_000];
    let a = s
        .apply(&t, "/INBOX/log.pdf", "log.pdf", bytes.len() as u64)
        .await;
    let full = a["fullUploadUrl"].as_str().unwrap();
    let b = s.apply(&t, "/INBOX/log2.pdf", "log2.pdf", 6).await;
    let part = b["partUploadUrl"].as_str().unwrap();
    let before = s.log().len();
    let r = s.send(full, "", "log.pdf", &bytes).await;
    assert_ok(&r.json());
    let r = s
        .send(
            part,
            "&uploadId=u&partNumber=1&totalChunks=2",
            "log.pdf",
            b"abc",
        )
        .await;
    assert_ok(&r.json());
    let after = s.log();
    assert_eq!(after.len() - before, 2);
    let (_, body) = multipart("log.pdf", &bytes);
    assert_eq!(
        after[before]["request_body"],
        format!("<{} bytes, not logged>", body.len())
    );
    assert!(
        !after[before]["flags"]
            .to_string()
            .contains("unknown_endpoint")
    );
    assert_eq!(after[before]["device"], SERIAL);
    assert!(
        after[before + 1]["request_body"]
            .as_str()
            .unwrap()
            .ends_with("bytes, not logged>")
    );
    let text: String = s.log().iter().map(|v| v.to_string()).collect();
    for url in [full, part] {
        let sig = query_value(url, "signature");
        assert!(!text.contains(&sig), "a signature is in the log");
    }
}

#[tokio::test]
async fn refuse_mode_refuses_every_upload_route() {
    let s = Server::with_config(Config {
        sync: SyncMode::Refuse,
        protect: false,
        ..Config::default()
    });
    let t = s.login(SERIAL).await;
    for path in [APPLY, FINISH] {
        assert_err(
            &s.post(path, &t, json!({"equipmentNo": SERIAL})).await,
            "E0079",
        );
    }
    for path in [UPLOAD, PART] {
        let (ct, body) = multipart("x", b"x");
        let r = s
            .raw(
                "POST",
                &format!("{path}?path=a"),
                &[("content-type", &ct)],
                Body::from(body),
            )
            .await;
        assert_err(&r.json(), "E0079");
    }
    assert!(!s.account().join("staging").exists());
}

// ------------------------------------------------------------------ staging sweep

#[tokio::test]
async fn the_staging_sweep_removes_entries_older_than_a_day() {
    let s = Server::new();
    let staging = s.account().join("staging");
    let old = staging.join("11111111-1111-4111-8111-111111111111");
    let fresh = staging.join("22222222-2222-4222-8222-222222222222");
    for dir in [&old, &fresh] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("part-1"), b"x").unwrap();
    }
    let at = |ms: i64| UNIX_EPOCH + Duration::from_millis(ms as u64);
    let set = |p: &Path, t: SystemTime| {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    };
    let age = STAGING_MAX_AGE_MS as i64;
    set(&old.join("part-1"), at(T0 - age - 1000));
    set(&fresh.join("part-1"), at(T0 - age + 60_000));
    for dir in [&old, &fresh] {
        std::fs::File::open(dir)
            .unwrap()
            .set_modified(at(T0 - age - 1000))
            .unwrap();
    }
    assert_eq!(s.state.sweep_staging().unwrap(), 1);
    assert!(!old.exists());
    assert!(fresh.exists());
    // A day later (by the clock) the other one goes too.
    s.advance_ms(STAGING_MAX_AGE_MS as i64);
    assert_eq!(s.state.sweep_staging().unwrap(), 1);
    assert!(!fresh.exists());
}

#[test]
fn the_server_start_sweeps_staging() {
    let dir = tempfile::tempdir().unwrap();
    Accounts::new(dir.path())
        .create("jordi", EMAIL, "secret")
        .unwrap();
    let entry = dir
        .path()
        .join("accounts/jordi/staging/33333333-3333-4333-8333-333333333333");
    std::fs::create_dir_all(&entry).unwrap();
    let real_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // The clock says two days later than the files.
    let clock = Arc::new(TestClock(AtomicI64::new(
        real_now + 2 * STAGING_MAX_AGE_MS as i64,
    )));
    AppState::open_with_clock(Config::default(), dir.path(), clock).unwrap();
    assert!(!entry.exists());
}

// ------------------------------------------------------------------ commit protocol

struct StoreFixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    store: Store,
    journal: notedown_cloud::journal::Journal,
}

impl StoreFixture {
    fn new() -> StoreFixture {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        Accounts::new(&data)
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let store = Store::open(&data, "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let journal = notedown_cloud::journal::Journal::open(&data).unwrap();
        StoreFixture {
            _dir: dir,
            data,
            store,
            journal,
        }
    }

    fn acct(&self) -> PathBuf {
        self.data.join("accounts/jordi")
    }

    fn stage(&self, bytes: &[u8]) -> PathBuf {
        let dir = self
            .acct()
            .join("staging/44444444-4444-4444-8444-444444444444");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("data");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn put(&self, path: &str, bytes: &[u8], options: PutOptions, crash: Option<CrashPoint>) {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let staged = self.stage(bytes);
        let c = self
            .store
            .plan_put_with(
                &g,
                &WirePath::parse_named(path).unwrap(),
                &staged,
                Origin::Device,
                Some(SERIAL.into()),
                options,
            )
            .unwrap();
        self.store
            .commit_until(&g, c, &self.journal, crash)
            .unwrap();
    }

    fn recover(&self) -> Recovery {
        let g = self.store.lock().unwrap();
        self.store.recover(&g, &self.journal).unwrap()
    }

    fn index(&self) -> Vec<RecycleEntry> {
        let g = self.store.lock().unwrap();
        self.store.recycle_index(&g).unwrap()
    }

    fn types(&self) -> Vec<String> {
        self.journal
            .read_from(1)
            .unwrap()
            .map(|e| e.unwrap().body.type_name().to_string())
            .collect()
    }
}

const NEW_PARENTS: PutOptions = PutOptions {
    create_parents: true,
    recycle_at_ms: None,
    expect_md5: None,
};

fn protect_at(ms: u64) -> PutOptions {
    PutOptions {
        create_parents: true,
        recycle_at_ms: Some(ms),
        expect_md5: None,
    }
}

#[test]
fn a_put_with_parents_plans_folder_events_first_and_keeps_old_intents_readable() {
    let f = StoreFixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage(b"n");
    let c = f
        .store
        .plan_put_with(
            &g,
            &WirePath::parse_named("NOTE/Note/A/B/n.pdf").unwrap(),
            &staged,
            Origin::Device,
            None,
            NEW_PARENTS,
        )
        .unwrap();
    let ty: Vec<&str> = c.events.iter().map(|e| e.type_name()).collect();
    assert_eq!(ty, ["folder.created", "folder.created", "file.created"]);
    let PendingOp::Put { folders, .. } = &c.op else {
        panic!("{:?}", c.op)
    };
    let paths: Vec<&str> = folders.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["NOTE/Note/A", "NOTE/Note/A/B"]);
    // Without the option the parent must exist.
    assert!(
        f.store
            .plan_put(
                &g,
                &WirePath::parse_named("NOTE/Note/C/n.pdf").unwrap(),
                &staged,
                Origin::Device,
                None
            )
            .is_err()
    );
    // A Task 3 intent without the new fields still parses, and a plain put writes none.
    let old = r#"{"op":"put","id":7,"path":"a","staged":"s","md5":"m","size":1}"#;
    let op: PendingOp = serde_json::from_str(old).unwrap();
    assert_eq!(serde_json::to_string(&op).unwrap(), old);
}

#[test]
fn a_crash_before_the_rename_removes_the_new_folders_and_the_recycle_copy() {
    let f = StoreFixture::new();
    f.put("NOTE/Note/r.note", b"old", PutOptions::default(), None);
    let before = f.types().len();
    // New folders made, then a crash before the rename.
    f.put(
        "NOTE/Note/X/Y/n.pdf",
        b"n",
        NEW_PARENTS,
        Some(CrashPoint::BeforeRename),
    );
    assert!(f.acct().join("files/NOTE/Note/X/Y").is_dir());
    assert!(matches!(f.recover(), Recovery::Discarded { .. }));
    assert!(!f.acct().join("files/NOTE/Note/X").exists());
    // The recycle copy made, then a crash before the rename: the copy goes, the old bytes stay.
    f.put(
        "NOTE/Note/r.note",
        b"new",
        protect_at(1000),
        Some(CrashPoint::BeforeRename),
    );
    assert_eq!(
        std::fs::read(f.acct().join("recycle/1000/NOTE/Note/r.note")).unwrap(),
        b"old"
    );
    assert!(matches!(f.recover(), Recovery::Discarded { .. }));
    assert!(!f.acct().join("recycle/1000/NOTE/Note/r.note").exists());
    assert_eq!(
        std::fs::read(f.acct().join("files/NOTE/Note/r.note")).unwrap(),
        b"old"
    );
    assert!(f.index().is_empty());
    assert_eq!(f.types().len(), before);
}

#[test]
fn a_failed_rename_in_commit_undoes_the_folders_and_the_copy() {
    let f = StoreFixture::new();
    f.put("NOTE/Note/r.note", b"old", PutOptions::default(), None);
    let before = f.types().len();
    for (path, options) in [
        ("NOTE/Note/X/Y/n.pdf", NEW_PARENTS),
        ("NOTE/Note/r.note", protect_at(3000)),
    ] {
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let staged = f.stage(b"new bytes");
        let c = f
            .store
            .plan_put_with(
                &g,
                &WirePath::parse_named(path).unwrap(),
                &staged,
                Origin::Device,
                None,
                options,
            )
            .unwrap();
        // The staged file vanishes: the folders and the copy are made, the rename fails.
        std::fs::remove_file(&staged).unwrap();
        assert!(f.store.commit(&g, c, &f.journal).is_err(), "{path}");
        assert!(f.store.pending().unwrap().is_none(), "{path}");
    }
    assert!(!f.acct().join("files/NOTE/Note/X").exists());
    assert!(!f.acct().join("recycle/3000/NOTE/Note/r.note").exists());
    assert_eq!(
        std::fs::read(f.acct().join("files/NOTE/Note/r.note")).unwrap(),
        b"old"
    );
    assert!(f.index().is_empty());
    assert_eq!(f.types().len(), before);
}

#[test]
fn a_wrong_expected_md5_plans_nothing() {
    let f = StoreFixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage(b"bytes");
    let path = WirePath::parse_named("NOTE/Note/New/h.pdf").unwrap();
    let mut options = NEW_PARENTS;
    options.expect_md5 = Some(md5_of(b"other"));
    let e = f
        .store
        .plan_put_with(&g, &path, &staged, Origin::Device, None, options.clone())
        .unwrap_err();
    assert!(e.to_string().contains("content hash"), "{e}");
    // No id was assigned for the file or the folder.
    assert_eq!(f.store.id_of(&path).unwrap(), None);
    assert_eq!(f.store.id_of(&path.parent().unwrap()).unwrap(), None);
    options.expect_md5 = Some(md5_of(b"bytes").to_uppercase());
    assert!(
        f.store
            .plan_put_with(&g, &path, &staged, Origin::Device, None, options)
            .is_ok()
    );
}

#[test]
fn a_crash_after_the_rename_journals_and_indexes_exactly_once() {
    for crash in [CrashPoint::BeforeAppend, CrashPoint::BeforeClear] {
        let f = StoreFixture::new();
        f.put("NOTE/Note/r.note", b"old", PutOptions::default(), None);
        f.put("NOTE/Note/r.note", b"new", protect_at(2000), Some(crash));
        assert!(
            matches!(f.recover(), Recovery::Repaired { .. }),
            "{crash:?}"
        );
        assert!(matches!(f.recover(), Recovery::Clean));
        let ty = f.types();
        assert_eq!(
            ty.iter().filter(|t| *t == "file.modified").count(),
            1,
            "{ty:?}"
        );
        let index = f.index();
        assert_eq!(index.len(), 1, "{crash:?}: {index:?}");
        assert_eq!(index[0].md5, md5_of(b"old"));
        assert_eq!(index[0].deleted_at, 2000);
        assert_eq!(index[0].reason, RecycleReason::Overwritten);
        assert_eq!(
            std::fs::read(f.acct().join("recycle/2000/NOTE/Note/r.note")).unwrap(),
            b"old"
        );
        assert_eq!(
            std::fs::read(f.acct().join("files/NOTE/Note/r.note")).unwrap(),
            b"new"
        );
    }
}

#[test]
fn the_recycle_index_is_idempotent_and_cuts_a_torn_line() {
    let f = StoreFixture::new();
    let g = f.store.lock().unwrap();
    let e = |ms| RecycleEntry {
        id: 1001,
        path: "NOTE/Note/z.note".into(),
        md5: "abc".into(),
        deleted_at: ms,
        reason: RecycleReason::Deleted,
    };
    assert!(f.store.record_recycled(&g, &e(1)).unwrap());
    assert!(!f.store.record_recycled(&g, &e(1)).unwrap());
    let path = f.acct().join("recycle").join(RECYCLE_INDEX);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"id\":5,\"pa");
    std::fs::write(&path, &text).unwrap();
    assert_eq!(f.store.recycle_index(&g).unwrap(), vec![e(1)]);
    assert!(f.store.record_recycled(&g, &e(2)).unwrap());
    assert_eq!(f.store.recycle_index(&g).unwrap(), vec![e(1), e(2)]);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .ends_with("\"reason\":\"deleted\"}\n")
    );
    // Slots: the next free ms.
    let p = WirePath::parse_named("NOTE/Note/z.note").unwrap();
    assert_eq!(f.store.recycle_slot(&g, &p, 7).unwrap(), 7);
    std::fs::create_dir_all(f.acct().join("recycle/7/NOTE/Note")).unwrap();
    std::fs::write(f.acct().join("recycle/7/NOTE/Note/z.note"), b"z").unwrap();
    assert_eq!(f.store.recycle_slot(&g, &p, 7).unwrap(), 8);
    // move_to_recycle (for deletes) moves the file into its slot.
    let src = f.acct().join("files/NOTE/Note/m.note");
    std::fs::write(&src, b"m").unwrap();
    let m = RecycleEntry {
        id: 1002,
        path: "NOTE/Note/m.note".into(),
        md5: md5_of(b"m"),
        deleted_at: 9,
        reason: RecycleReason::Deleted,
    };
    f.store.move_to_recycle(&g, &src, &m).unwrap();
    assert!(!src.exists());
    assert_eq!(
        std::fs::read(f.acct().join("recycle/9/NOTE/Note/m.note")).unwrap(),
        b"m"
    );
}

// ------------------------------------------------------------------ fix round 1

#[tokio::test]
async fn finish_must_name_the_parent_signed_at_apply() {
    let s = Server::new();
    let t = s.session().await;
    // apply names NOTE/Note/x.note; a finish with "/NOTE/Note/x.note/" would mean the folder
    // x.note: refused, and no x.note/x.note appears.
    let a = s.apply(&t, "/NOTE/Note/x.note", "x.note", 1).await;
    let target =
        SignedTarget::decode(&query_value(a["fullUploadUrl"].as_str().unwrap(), "path")).unwrap();
    assert_eq!(target.path, "NOTE/Note");
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "x.note", b"x")
        .await;
    // By file name the upload is looked up under the parent the finish names: none there.
    let f = s
        .finish(
            &t,
            "/NOTE/Note/x.note/",
            "x.note",
            a["innerName"].as_str().unwrap(),
            b"x",
        )
        .await;
    assert_err(&f, "E0324");
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(e["flags"].to_string().contains("upload_missing"), "{e}");
    assert!(!s.files().join("NOTE/Note/x.note").exists());
    // By the internal id the entry is found, and its signed parent does not match.
    let f = s
        .finish(&t, "/NOTE/Note/x.note/", "x.note", &internal_id(&a), b"x")
        .await;
    assert_err(&f, "E0324");
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(e["flags"].to_string().contains("parent_mismatch"), "{e}");
    assert!(!s.files().join("NOTE/Note/x.note").exists());
    assert!(s.staging_entries().is_empty());
    // Another folder entirely is refused too.
    let a = s.apply(&t, "/NOTE/Note/x.note", "x.note", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "x.note", b"x")
        .await;
    let f = s
        .finish(
            &t,
            "/INBOX/",
            "x.note",
            a["innerName"].as_str().unwrap(),
            b"x",
        )
        .await;
    assert_err(&f, "E0324");
    assert!(!s.files().join("INBOX/x.note").exists());

    // A real folder named like the file: apply with the full path, finish with the folder.
    std::fs::create_dir_all(s.files().join("NOTE/Note/y.note")).unwrap();
    let a = s.apply(&t, "/NOTE/Note/y.note/y.note", "y.note", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "y.note", b"y")
        .await;
    let f = s
        .finish(
            &t,
            "/NOTE/Note/y.note/",
            "y.note",
            a["innerName"].as_str().unwrap(),
            b"y",
        )
        .await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "NOTE/Note/y.note/y.note");

    // apply honours a trailing slash: the path is the folder.
    let a = s.apply(&t, "/NOTE/Note/Work/", "Work", 1).await;
    let target =
        SignedTarget::decode(&query_value(a["fullUploadUrl"].as_str().unwrap(), "path")).unwrap();
    assert_eq!(target.path, "NOTE/Note/Work");
}

#[tokio::test]
async fn an_upload_url_takes_one_set_of_bytes_and_answers_its_retry() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/once.pdf", "once.pdf", 4).await;
    let url = a["fullUploadUrl"].as_str().unwrap();
    let inner = &internal_id(&a);
    let first = s.send(url, "", "once.pdf", b"good").await.json();
    assert_ok(&first);
    // The answer was lost: the device sends the same bytes again and gets the same answer.
    let again = s.send(url, "", "once.pdf", b"good").await.json();
    assert_ok(&again);
    assert_eq!(again["md5"], first["md5"]);
    assert_eq!(again["innerName"], inner.as_str());
    let entry = s.account().join("staging").join(inner);
    let names: Vec<String> = std::fs::read_dir(&entry)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
    let meta: Value =
        serde_json::from_slice(&std::fs::read(entry.join("upload.json")).unwrap()).unwrap();
    assert_eq!(meta["md5"], md5_of(b"good"));
    assert_eq!(meta["parent"], "INBOX");
    // Other bytes are refused.
    let r = s.send(url, "", "once.pdf", b"evil").await;
    assert_err(&r.json(), "E0324");
    let data = s.account().join("staging").join(inner).join("data");
    assert_eq!(std::fs::read(&data).unwrap(), b"good");
    // Not after parts either, and parts not after a single upload.
    let b = s.apply(&t, "/INBOX/p.pdf", "p.pdf", 4).await;
    let r = s
        .send(
            b["partUploadUrl"].as_str().unwrap(),
            "&uploadId=u&partNumber=1&totalChunks=2",
            "p.pdf",
            b"ab",
        )
        .await;
    assert_ok(&r.json());
    assert_err(
        &s.send(b["fullUploadUrl"].as_str().unwrap(), "", "p.pdf", b"abab")
            .await
            .json(),
        "E0324",
    );
    let r = s
        .send(
            a["partUploadUrl"].as_str().unwrap(),
            "&uploadId=u&partNumber=1&totalChunks=1",
            "once.pdf",
            b"good",
        )
        .await;
    assert_err(&r.json(), "E0324");
    // finish commits what was uploaded first.
    assert_ok(&s.finish(&t, "/INBOX/", "once.pdf", inner, b"good").await);
    assert_eq!(
        std::fs::read(s.files().join("INBOX/once.pdf")).unwrap(),
        b"good"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posts_during_finish_are_refused_and_the_journal_matches_the_bytes() {
    let s = Arc::new(Server::new());
    let t = s.session().await;
    for round in 0..6u8 {
        let name = format!("race{round}.pdf");
        let good = vec![round; 300_000];
        let a = s
            .apply(&t, &format!("/INBOX/{name}"), &name, good.len() as u64)
            .await;
        let url = a["fullUploadUrl"].as_str().unwrap().to_string();
        let inner = a["innerName"].as_str().unwrap().to_string();
        assert_ok(&s.send(&url, "", &name, &good).await.json());
        let finish = {
            let (s, t, name, inner, good) =
                (s.clone(), t.clone(), name.clone(), inner, good.clone());
            tokio::spawn(async move { s.finish(&t, "/INBOX/", &name, &inner, &good).await })
        };
        let posts: Vec<_> = (0..3u8)
            .map(|i| {
                let (s, url, name) = (s.clone(), url.clone(), name.clone());
                tokio::spawn(async move {
                    s.send(&url, "", &name, &vec![200 + i; 300_000])
                        .await
                        .json()
                })
            })
            .collect();
        assert_ok(&finish.await.unwrap());
        for p in posts {
            assert_err(&p.await.unwrap(), "E0324");
        }
        let on_disk = std::fs::read(s.files().join(format!("INBOX/{name}"))).unwrap();
        assert_eq!(on_disk, good);
        let created = s
            .events()
            .into_iter()
            .rfind(|e| e["type"] == "file.created" && e["path"] == format!("INBOX/{name}"))
            .unwrap();
        assert_eq!(created["md5"], md5_of(&on_disk));
    }
}

#[tokio::test]
async fn other_multipart_fields_are_capped_together() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/f.pdf", "f.pdf", 1).await;
    let uri = path_and_query(a["fullUploadUrl"].as_str().unwrap());
    let send = |fields: Vec<(&'static str, Vec<u8>)>| {
        let (ct, body) = multipart_with(&fields, "f.pdf", b"f");
        let (s, uri) = (&s, uri.clone());
        async move {
            s.raw("POST", &uri, &[("content-type", &ct)], Body::from(body))
                .await
                .json()
        }
    };
    // Two fields of 150 KiB: each is small, together they are above 256 KiB.
    let r = send(vec![
        ("a", vec![b'a'; 150 * 1024]),
        ("b", vec![b'b'; 150 * 1024]),
    ])
    .await;
    assert_err(&r, "E0324");
    let e = s.requests_to(UPLOAD).pop().unwrap();
    assert!(e["flags"].to_string().contains("upload_broken"), "{e}");
    // 200 KiB in one field is fine.
    let r = send(vec![("a", vec![b'a'; 200 * 1024])]).await;
    assert_ok(&r);
}

#[tokio::test]
async fn a_dropped_request_stops_extending_the_lock() {
    use notedown_cloud::api::files::keep_lock_every;
    let s = Server::new();
    s.session().await; // the lock lives until T0 + 30 s
    let holder = || s.state.sync_locks.holder("jordi", s.state.now_ms());
    let every = Duration::from_millis(5);
    s.advance_ms(20_000);
    // While the work runs, the keeper extends the lock (to T0 + 50 s) ...
    let work = keep_lock_every(
        &s.state,
        "jordi",
        SERIAL,
        every,
        std::future::pending::<()>(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), work)
            .await
            .is_err()
    );
    s.advance_ms(15_000); // T0 + 35 s
    assert_eq!(holder().as_deref(), Some(SERIAL));
    // ... and the request future is dropped: no keeper is left to extend it further.
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.advance_ms(20_000); // T0 + 55 s
    assert_eq!(holder(), None);
}

// ------------------------------------------------------------------ fix round 2

#[test]
fn the_staging_lock_answers_retries_of_claimed_bytes_and_cleans_up_failures() {
    use notedown_cloud::sync::staging::{Claim, SingleOutcome, StagingLocks};
    let dir = tempfile::tempdir().unwrap();
    let entry = dir.path().join("55555555-5555-4555-8555-555555555555");
    std::fs::create_dir_all(&entry).unwrap();
    let locks = StagingLocks::default();
    let tmp = |bytes: &[u8]| {
        let p = entry.join(format!(".nd-test-{}.tmp", md5_of(bytes)));
        std::fs::write(&p, bytes).unwrap();
        p
    };
    // A write error after upload.json (the temp file is missing, so the rename fails): the
    // metadata is removed again and the URL stays usable.
    let missing = entry.join(".nd-missing.tmp");
    assert!(
        locks
            .store_single(&entry, &missing, "INBOX", &md5_of(b"a"))
            .is_err()
    );
    assert!(!entry.join("upload.json").exists());
    let t = tmp(b"a");
    assert_eq!(
        locks
            .store_single(&entry, &t, "INBOX", &md5_of(b"a"))
            .unwrap(),
        SingleOutcome::Stored
    );
    assert!(!t.exists());
    // Claimed by a finish: a retry of the same bytes is still answered, other bytes are not,
    // and no temp file is left either way.
    let Claim::Claimed(claimed) = locks.claim(&entry, "INBOX", "op1").unwrap() else {
        panic!("not claimed")
    };
    let t = tmp(b"a");
    assert_eq!(
        locks
            .store_single(&entry, &t, "INBOX", &md5_of(b"a"))
            .unwrap(),
        SingleOutcome::Same
    );
    assert!(!t.exists());
    let t = tmp(b"b");
    assert_eq!(
        locks
            .store_single(&entry, &t, "INBOX", &md5_of(b"b"))
            .unwrap(),
        SingleOutcome::Refused
    );
    assert!(!t.exists());
    assert!(!entry.join("data").exists());
    // Released after a failed finish: `data` again, and a new claim works.
    locks.release(&entry, &claimed).unwrap();
    assert_eq!(std::fs::read(entry.join("data")).unwrap(), b"a");
    assert!(matches!(
        locks.claim(&entry, "INBOX", "op2").unwrap(),
        Claim::Claimed(_)
    ));
}

#[tokio::test]
async fn a_finish_that_fails_before_its_commit_can_be_retried() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/retry.pdf", "retry.pdf", 5).await;
    let inner = &internal_id(&a);
    s.send(
        a["fullUploadUrl"].as_str().unwrap(),
        "",
        "retry.pdf",
        b"bytes",
    )
    .await;
    // An unreadable intent blocks every write of the account: the finish fails after its
    // claim, with no intent of its own.
    let pending = s.account().join("pending.json");
    std::fs::write(&pending, b"{broken").unwrap();
    let f = s.finish(&t, "/INBOX/", "retry.pdf", inner, b"bytes").await;
    assert_err(&f, "500");
    let entry = s.account().join("staging").join(inner);
    assert_eq!(std::fs::read(entry.join("data")).unwrap(), b"bytes");
    assert!(!std::fs::read_dir(&entry).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("finishing-")
    }));
    // The operator fixes it; the device retries the finish.
    std::fs::remove_file(&pending).unwrap();
    let f = s.finish(&t, "/INBOX/", "retry.pdf", inner, b"bytes").await;
    assert_ok(&f);
    assert_eq!(
        std::fs::read(s.files().join("INBOX/retry.pdf")).unwrap(),
        b"bytes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_finish_whose_intent_names_the_bytes_leaves_them_to_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let s = Server::new();
    let t = s.session().await;
    // The journal cannot be written: the commit writes its intent and applies the change, then
    // the append fails and the intent stays.
    let (_, newest) = s.state.journal.files().unwrap().pop().unwrap();
    let ro = std::fs::Permissions::from_mode(0o444);
    std::fs::set_permissions(&newest, ro).unwrap();
    let a = s.apply(&t, "/INBOX/intent.pdf", "intent.pdf", 5).await;
    let inner = internal_id(&a);
    s.send(
        a["fullUploadUrl"].as_str().unwrap(),
        "",
        "intent.pdf",
        b"bytes",
    )
    .await;
    let f = s
        .finish(&t, "/INBOX/", "intent.pdf", &inner, b"bytes")
        .await;
    assert_err(&f, "500");
    let intent: Value =
        serde_json::from_slice(&std::fs::read(s.account().join("pending.json")).unwrap()).unwrap();
    let staged = intent["op"]["staged"].as_str().unwrap().to_string();
    assert!(staged.contains("/finishing-"), "{staged}");
    // Nothing was given back: no `data` reappears beside the intent's file.
    let entry = s.account().join("staging").join(&inner);
    assert!(!entry.join("data").exists());
    assert_eq!(
        std::fs::read(s.files().join("INBOX/intent.pdf")).unwrap(),
        b"bytes"
    );
    // Recovery journals the change exactly once.
    std::fs::set_permissions(&newest, std::fs::Permissions::from_mode(0o644)).unwrap();
    s.post(LIST, &t, json!({"path": "/INBOX"})).await;
    assert!(!s.account().join("pending.json").exists());
    let created: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| e["type"] == "file.created" && e["path"] == "INBOX/intent.pdf")
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0]["origin"], "repair");
}

#[tokio::test]
async fn an_upload_into_a_folder_removed_out_of_band_answers_the_listed_id() {
    let s = Server::new();
    let t = s.session().await;
    // the folder gets ids through a listing, then vanishes by hand with its content (another
    // file stays, so the scan doesn't hold)
    s.put("NOTE/Note/Work/old.pdf", b"old");
    s.put("INBOX/keep.pdf", b"keep");
    let tree = s
        .post(LIST, &t, json!({"path": "/", "recursive": true}))
        .await;
    assert_ok(&tree);
    std::fs::remove_dir_all(s.files().join("NOTE/Note/Work")).unwrap();
    let n = s.events().len();
    let v = s
        .upload(&t, "NOTE/Note/Work", "new.pdf", b"new bytes")
        .await;
    assert_ok(&v);
    let answered = v["id"].as_str().unwrap().to_string();
    let ev: Vec<Value> = s.events().into_iter().skip(n).collect();
    // the scan before the upload journals the removal first
    assert_eq!(
        types(&ev),
        [
            "file.deleted",
            "folder.deleted",
            "folder.created",
            "file.created"
        ]
    );
    assert_eq!(ev[0]["origin"], "scan");
    assert_eq!(ev[1]["origin"], "scan");
    let ev = &ev[2..];
    assert_eq!(ev[1]["id"].as_u64().unwrap().to_string(), answered);
    let tree = s
        .post(LIST, &t, json!({"path": "/", "recursive": true}))
        .await;
    let listed = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path_display"] == "NOTE/Note/Work/new.pdf")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(listed, answered);
    let folder = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path_display"] == "NOTE/Note/Work")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(ev[0]["id"].as_u64().unwrap().to_string(), folder);
}

/// Ruling: the protect gate holds on every file route of a running session. Clearing the
/// marker (as `import --replace` does) stops the next call, not only the next
/// `synchronous/start`.
#[tokio::test]
async fn a_cleared_marker_closes_every_file_route_of_a_running_session() {
    let s = Server::protected();
    let t = s.session().await;
    let done = s.upload(&t, "INBOX", "a.pdf", b"a").await;
    assert_ok(&done);
    let id = done["id"].as_str().unwrap().to_string();
    let a = s.apply(&t, "/INBOX/b.pdf", "b.pdf", 1).await;
    assert_ok(&a);
    assert_ok(
        &s.post(LIST, &t, json!({"equipmentNo": SERIAL, "path": "/"}))
            .await,
    );

    let accounts = Accounts::new(s.data());
    {
        let _lock = accounts.lock("jordi").unwrap();
        let mut acc = accounts.get("jordi").unwrap().unwrap();
        acc.imported = None;
        accounts.save(&acc).unwrap();
    }
    let calls: Vec<(&str, Value)> = vec![
        (LIST, json!({"path": "/", "recursive": true})),
        ("/api/file/3/files/list_folder_v3", json!({"id": "0"})),
        ("/api/file/3/files/query_v3", json!({"id": id})),
        (
            "/api/file/3/files/query/by/path_v3",
            json!({"path": "/INBOX/a.pdf"}),
        ),
        (DOWNLOAD, json!({"id": id})),
        (
            APPLY,
            json!({"path": "/INBOX/c.pdf", "fileName": "c.pdf", "size": 1}),
        ),
        (
            FINISH,
            json!({"path": "/INBOX/", "fileName": "b.pdf", "content_hash": md5_of(b"b"),
                   "innerName": a["innerName"], "size": "1"}),
        ),
        (
            "/api/file/2/files/create_folder_v2",
            json!({"path": "/INBOX/New"}),
        ),
        ("/api/file/3/files/delete_folder_v3", json!({"id": id})),
        (
            "/api/file/3/files/move_v3",
            json!({"id": id, "to_path": "/INBOX/m.pdf"}),
        ),
        (
            "/api/file/3/files/copy_v3",
            json!({"id": id, "to_path": "/INBOX/c2.pdf"}),
        ),
    ];
    for (path, mut body) in calls {
        body["equipmentNo"] = json!(SERIAL);
        let v = s.post(path, &t, body).await;
        assert_err(&v, "E0079");
        let e = s.requests_to(path).pop().unwrap();
        assert!(
            e["flags"].to_string().contains("protect_refused"),
            "{path}: {e}"
        );
    }
    let r = s
        .send(a["fullUploadUrl"].as_str().unwrap(), "", "b.pdf", b"b")
        .await;
    assert_err(&r.json(), "E0079");
    let r = s
        .send(
            a["partUploadUrl"].as_str().unwrap(),
            "&uploadId=u&partNumber=1&totalChunks=1",
            "b.pdf",
            b"b",
        )
        .await;
    assert_err(&r.json(), "E0079");
    for path in [UPLOAD, PART] {
        let e = s.requests_to(path).pop().unwrap();
        assert!(
            e["flags"].to_string().contains("protect_refused"),
            "{path}: {e}"
        );
    }
    // nothing changed on disk
    assert!(s.account().join("files/INBOX/a.pdf").exists());
    assert!(!s.account().join("files/INBOX/New").exists());
}

/// A listing that passed the protect gate, then waited for the store lock behind an import
/// that cleared the marker, is refused after the lock is released: it never sees the
/// half-imported tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_that_waited_behind_a_failed_import_is_refused() {
    let s = Server::protected();
    let t = s.session().await; // lock taken at T0, live until T0 + 30 s
    s.advance_ms(20_000);
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let data = s.data().to_path_buf();
    // the import: holds the store lock, clears the marker, then gives up
    let import = std::thread::spawn(move || {
        let store = notedown_cloud::store::Store::open(&data, "jordi").unwrap();
        let guard = store.lock().unwrap();
        held_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        let accounts = Accounts::new(&data);
        {
            let _lock = accounts.lock("jordi").unwrap();
            let mut a = accounts.get("jordi").unwrap().unwrap();
            a.imported = None;
            accounts.save(&a).unwrap();
        }
        drop(guard);
    });
    held_rx.recv().unwrap();
    let state = s.state.clone();
    let listing = s.post(LIST, &t, json!({"equipmentNo": SERIAL, "path": "/"}));
    let watcher = async {
        // The request touched the sync lock (now T0 + 20 s, so it lives past T0 + 40 s): its
        // gate check ran in the same step, with the marker still set.
        while state
            .sync_locks
            .holder("jordi", (T0 + 40_000) as u64)
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        go_tx.send(()).unwrap();
    };
    let (v, ()) = tokio::join!(listing, watcher);
    import.join().unwrap();
    assert_err(&v, "E0079");
    let e = s.requests_to(LIST).pop().unwrap();
    assert!(e["flags"].to_string().contains("protect_refused"), "{e}");
}

/// Without its account, an upload URL gets an internal error, not the protect refusal (which
/// would be wrong with protect off).
#[tokio::test]
async fn an_upload_for_a_vanished_account_is_no_protect_refusal() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/v.pdf", "v.pdf", 1).await;
    assert_ok(&a);
    std::fs::remove_file(s.account().join("account.json")).unwrap();
    let r = s
        .send(a["fullUploadUrl"].as_str().unwrap(), "", "v.pdf", b"v")
        .await;
    let e = s.requests_to(UPLOAD).pop().unwrap();
    assert!(!e["flags"].to_string().contains("protect_refused"), "{e}");
    assert_ne!(r.json()["errorCode"], "E0079", "{}", r.json());
    assert_eq!(r.json()["success"], false);
}

// ------------------------------------------------------------------ innerName forms

#[tokio::test]
async fn finish_takes_the_file_name_or_the_internal_id_as_inner_name() {
    let s = Server::new();
    let t = s.session().await;
    // By file name, as apply answers it (Ratta's traffic).
    let a = s.apply(&t, "/INBOX/n.pdf", "n.pdf", 1).await;
    assert_eq!(a["innerName"], "n.pdf");
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "n.pdf", b"n")
        .await;
    let f = s.finish(&t, "/INBOX/", "n.pdf", "n.pdf", b"n").await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "INBOX/n.pdf");
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(
        e["flags"].to_string().contains("inner_name_file_name"),
        "{e}"
    );
    // By the internal id from the signed URL.
    let a = s.apply(&t, "/INBOX/i.pdf", "i.pdf", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "i.pdf", b"i")
        .await;
    let f = s
        .finish(&t, "/INBOX/", "i.pdf", &internal_id(&a), b"i")
        .await;
    assert_ok(&f);
    assert_eq!(f["path_display"], "INBOX/i.pdf");
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(e["flags"].to_string().contains("inner_name_id"), "{e}");
    assert_eq!(std::fs::read(s.files().join("INBOX/i.pdf")).unwrap(), b"i");
    assert!(s.staging_entries().is_empty(), "{:?}", s.staging_entries());
}

#[tokio::test]
async fn by_file_name_the_latest_apply_wins_and_other_names_are_unknown() {
    let s = Server::new();
    let t = s.session().await;
    // Two applies for the same file; the bytes go to the URL of the second one.
    let first = s.apply(&t, "/INBOX/r.pdf", "r.pdf", 2).await;
    let second = s.apply(&t, "/INBOX/r.pdf", "r.pdf", 2).await;
    assert_ne!(internal_id(&first), internal_id(&second));
    s.send(
        second["fullUploadUrl"].as_str().unwrap(),
        "",
        "r.pdf",
        b"r2",
    )
    .await;
    let f = s.finish(&t, "/INBOX/", "r.pdf", "r.pdf", b"r2").await;
    assert_ok(&f);
    assert_eq!(std::fs::read(s.files().join("INBOX/r.pdf")).unwrap(), b"r2");
    // The other way round: bytes at the first URL, a later apply without bytes wins and has
    // no data, so the finish is refused and nothing is written.
    let a = s.apply(&t, "/INBOX/q.pdf", "q.pdf", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "q.pdf", b"q")
        .await;
    assert_ok(&s.apply(&t, "/INBOX/q.pdf", "q.pdf", 1).await);
    let f = s.finish(&t, "/INBOX/", "q.pdf", "q.pdf", b"q").await;
    assert_err(&f, "E0324");
    assert!(!s.files().join("INBOX/q.pdf").exists());
    // An innerName that is neither the file name nor an internal id: unknown.
    let a = s.apply(&t, "/INBOX/u.pdf", "u.pdf", 1).await;
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "u.pdf", b"u")
        .await;
    for inner in ["other.pdf", "", "../u.pdf"] {
        let f = s.finish(&t, "/INBOX/", "u.pdf", inner, b"u").await;
        assert_err(&f, "E0324");
    }
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(e["flags"].to_string().contains("upload_missing"), "{e}");
    assert!(!s.files().join("INBOX/u.pdf").exists());
    // Another device does not hold the lock and gets nothing.
    let other = s.login(OTHER).await;
    let f = s
        .post(
            FINISH,
            &other,
            json!({"equipmentNo": OTHER, "path": "/INBOX/", "fileName": "u.pdf",
                   "content_hash": md5_of(b"u"), "innerName": "u.pdf", "size": "1"}),
        )
        .await;
    assert_err(&f, "E0079");
    // The right device and name still works.
    let f = s.finish(&t, "/INBOX/", "u.pdf", "u.pdf", b"u").await;
    assert_ok(&f);
}

/// The internal id names an upload of one device: another device that sends it gets the answer
/// of an unknown upload, and its bad request does not remove the staged bytes. The device that
/// applied finishes afterwards.
#[tokio::test]
async fn by_internal_id_only_the_applying_device_finishes() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/d.pdf", "d.pdf", 1).await;
    assert_ok(&a);
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", "d.pdf", b"d")
        .await;
    let id = internal_id(&a);
    let end = |token: String, serial: &'static str| {
        let s = &s;
        async move {
            assert_ok(
                &s.post(END, &token, json!({"equipmentNo": serial, "flag": "Y"}))
                    .await,
            );
        }
    };
    end(t.clone(), SERIAL).await;
    let other = s.login(OTHER).await;
    assert_ok(&s.post(START, &other, json!({"equipmentNo": OTHER})).await);
    let finish_as_other = |path: &'static str| {
        s.post(
            FINISH,
            &other,
            json!({"equipmentNo": OTHER, "path": path, "fileName": "d.pdf",
                   "content_hash": md5_of(b"d"), "innerName": id, "size": "1"}),
        )
    };
    let f = finish_as_other("/INBOX/").await;
    assert_err(&f, "E0324");
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(e["flags"].to_string().contains("upload_missing"), "{e}");
    assert!(e["flags"].to_string().contains("inner_name_id"), "{e}");
    // A bad path with the id of another device removes nothing either.
    assert_err(&finish_as_other("/../").await, "E0324");
    assert!(!s.files().join("INBOX/d.pdf").exists());
    assert_eq!(s.staging_entries(), vec![id.clone()]);
    end(other.clone(), OTHER).await;
    // The device that applied: success with the same id.
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    let f = s.finish(&t, "/INBOX/", "d.pdf", &id, b"d").await;
    assert_ok(&f);
    assert_eq!(std::fs::read(s.files().join("INBOX/d.pdf")).unwrap(), b"d");
    assert!(s.staging_entries().is_empty(), "{:?}", s.staging_entries());
}

/// A file whose name looks like an internal id takes the file name route when `innerName`
/// equals `fileName`: it finds the latest apply of this device for that name.
#[tokio::test]
async fn a_file_named_like_an_internal_id_finishes_by_its_name() {
    let s = Server::new();
    let t = s.session().await;
    let name = "0f8fad5b-d9cb-469f-a165-70867728950e";
    assert!(staging::valid_inner_name(name));
    let a = s.apply(&t, &format!("/INBOX/{name}"), name, 1).await;
    assert_ok(&a);
    assert_eq!(a["innerName"], name);
    assert_ne!(internal_id(&a), name);
    s.send(a["fullUploadUrl"].as_str().unwrap(), "", name, b"x")
        .await;
    let f = s.finish(&t, "/INBOX/", name, name, b"x").await;
    assert_ok(&f);
    assert_eq!(f["path_display"], format!("INBOX/{name}"));
    let e = s.requests_to(FINISH).pop().unwrap();
    assert!(
        e["flags"].to_string().contains("inner_name_file_name"),
        "{e}"
    );
    assert_eq!(
        std::fs::read(s.files().join("INBOX").join(name)).unwrap(),
        b"x"
    );
    assert!(s.staging_entries().is_empty(), "{:?}", s.staging_entries());
}
