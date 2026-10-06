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
use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
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
const LIST: &str = "/api/file/2/files/list_folder";
const APPLY: &str = "/api/file/3/files/upload/apply";
const FINISH: &str = "/api/file/2/files/upload/finish";
const DOWNLOAD: &str = "/api/file/3/files/download_v3";
const CAPACITY: &str = "/api/file/capacity/query";
const UPLOAD: &str = "/api/oss/upload";
const PART: &str = "/api/oss/upload/part";

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
        Self::with_config(Config {
            protect: false,
            ..Config::default()
        })
    }

    /// Protect mode with the import marker set (the gate is open, overwrites are recycled).
    fn protected() -> Server {
        let s = Self::with_config(Config {
            protect: true,
            ..Config::default()
        });
        let accounts = Accounts::new(s.data());
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 0,
            bytes: 0,
        });
        accounts.save(&a).unwrap();
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

    fn staging_entries(&self) -> Vec<String> {
        match std::fs::read_dir(self.account().join("staging")) {
            Ok(d) => d
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
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
    let inner = a["innerName"].as_str().unwrap().to_string();
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
    // Case collision with the file, and with a folder on the way.
    assert_err(
        &s.apply(&t, "/NOTE/Note/plan.note", "plan.note", 1).await,
        "E0322",
    );
    assert_err(
        &s.apply(&t, "/note/Note/x.note", "x.note", 1).await,
        "E0322",
    );
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

    // The collision check runs again at finish (a file appeared meanwhile).
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
    assert_err(&f, "E0322");
    let names: Vec<String> = std::fs::read_dir(s.files().join("NOTE/Note"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.contains(&"Late.note".to_string()), "{names:?}");
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
    let inner = a["innerName"].as_str().unwrap();
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
    let inner = a["innerName"].as_str().unwrap();
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
    let entry = s
        .account()
        .join("staging")
        .join(a["innerName"].as_str().unwrap());
    assert!(!entry.join("data").exists());
    assert_eq!(
        std::fs::read_dir(&entry).unwrap().count(),
        0,
        "no temp file is left"
    );
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
    let entry = s
        .account()
        .join("staging")
        .join(p["innerName"].as_str().unwrap());
    assert!(!entry.join("data").exists());
    assert!(!entry.join("part-1").exists());
}

#[test]
fn the_real_limits_are_9_mib_per_part_and_1_gib_per_file() {
    assert_eq!(staging::PART_MAX, 9 * 1024 * 1024);
    assert_eq!(staging::FILE_MAX, 1 << 30);
}

// ------------------------------------------------------------------ gates and signatures

#[tokio::test]
async fn every_upload_route_needs_the_live_sync_lock() {
    let s = Server::new();
    let t = s.session().await;
    let a = s.apply(&t, "/INBOX/l.pdf", "l.pdf", 1).await;
    assert_ok(&a);
    // The lock lapses (no call for 31 s).
    s.advance_ms(31_000);
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
    // Another device holds the lock: the URL of the first device does not work either.
    let other = s.login(OTHER).await;
    assert_ok(&s.post(START, &other, json!({"equipmentNo": OTHER})).await);
    let r = s
        .send(a["fullUploadUrl"].as_str().unwrap(), "", "l.pdf", b"l")
        .await;
    assert_err(&r.json(), "E0079");
    assert!(s.staging_entries().is_empty());
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
};

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
    let protect = PutOptions {
        create_parents: true,
        recycle_at_ms: Some(1000),
    };
    let before = f.types().len();
    // A new file under two new folders: crash after the intent, before anything is applied.
    f.put(
        "NOTE/Note/X/Y/n.pdf",
        b"n",
        NEW_PARENTS,
        Some(CrashPoint::BeforeApply),
    );
    // Simulate the half-applied state: the folders exist, the file does not.
    std::fs::create_dir_all(f.acct().join("files/NOTE/Note/X/Y")).unwrap();
    assert!(matches!(f.recover(), Recovery::Discarded { .. }));
    assert!(!f.acct().join("files/NOTE/Note/X").exists());
    // An overwrite with a recycle copy made, then a crash before the rename.
    f.put(
        "NOTE/Note/r.note",
        b"new",
        protect,
        Some(CrashPoint::BeforeApply),
    );
    let g = f.store.lock().unwrap();
    let entry = RecycleEntry {
        id: 0,
        path: "NOTE/Note/r.note".into(),
        md5: md5_of(b"old"),
        deleted_at: 1000,
        reason: RecycleReason::Overwritten,
    };
    f.store
        .copy_to_recycle(&g, &f.acct().join("files/NOTE/Note/r.note"), &entry)
        .unwrap();
    drop(g);
    assert!(f.acct().join("recycle/1000/NOTE/Note/r.note").is_file());
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
fn a_crash_after_the_rename_journals_and_indexes_exactly_once() {
    for crash in [CrashPoint::BeforeAppend, CrashPoint::BeforeClear] {
        let f = StoreFixture::new();
        f.put("NOTE/Note/r.note", b"old", PutOptions::default(), None);
        let protect = PutOptions {
            create_parents: true,
            recycle_at_ms: Some(2000),
        };
        f.put("NOTE/Note/r.note", b"new", protect, Some(crash));
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
