//! Digest routes of accept mode (`docs/sync-protocol.md` 5.3, 5.4, 5.7): items, groups, tags,
//! the gate (D1, never the sync lock), ids (D2, D3), paging (D7) and unknown fields (D10).
//! Requests go through the router in memory (`tower::oneshot`).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use notedown_cloud::appdata::{AppData, Digest, DigestGroup};
use notedown_cloud::auth::{Accounts, AppDataImported, Imported, expected_login_hash, md5_hex};
use notedown_cloud::journal::{Event, EventBody, Origin};
use notedown_cloud::server::{AppState, router};
use notedown_cloud::{Clock, Config, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const SERIAL: &str = "SN078C10000000";
const OTHER: &str = "SN078C10000001";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;

const ADD: &str = "/api/file/add/summary";
const UPDATE: &str = "/api/file/update/summary";
const DELETE: &str = "/api/file/delete/summary";
const QUERY: &str = "/api/file/query/summary";
const HASH: &str = "/api/file/query/summary/hash";
const BY_ID: &str = "/api/file/query/summary/id";
const GROUP_ADD: &str = "/api/file/add/summary/group";
const GROUP_UPDATE: &str = "/api/file/update/summary/group";
const GROUP_DELETE: &str = "/api/file/delete/summary/group";
const GROUP_QUERY: &str = "/api/file/query/summary/group";
const TAG_ADD: &str = "/api/file/add/summary/tag";
const TAG_UPDATE: &str = "/api/file/update/summary/tag";
const TAG_DELETE: &str = "/api/file/delete/summary/tag";
const TAG_QUERY: &str = "/api/file/query/summary/tag";
const START: &str = "/api/file/2/files/synchronous/start";
const MARK_APPLY: &str = "/api/file/upload/apply/summary";
const MARK_DOWNLOAD: &str = "/api/file/download/summary";
const MARK_UPLOAD: &str = "/api/oss/upload/summary";
const MARK_GET: &str = "/api/oss/download/summary";
/// A handwritten comment's name as the device makes it (5.5): 32 hex characters, case kept.
const MARK: &str = "0123456789abcdefABCDEF0123456789.mark";

/// Above 2^53: an `f64` would round it.
const BIG_ID: u64 = 895_334_188_016_730_113;

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
    headers: axum::http::HeaderMap,
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

impl Server {
    /// Accept mode, protect on, files and app data imported.
    fn new() -> Server {
        let s = Self::with_config(Config::default());
        s.mark_imported();
        s.mark_app_data_imported();
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

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// The `imported` marker and the native folders, as after a file import.
    fn mark_imported(&self) {
        let store = notedown_cloud::store::Store::open(self.data(), "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let accounts = Accounts::new(self.data());
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 0,
            bytes: 0,
        });
        accounts.save(&a).unwrap();
    }

    fn mark_app_data_imported(&self) {
        let accounts = Accounts::new(self.data());
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.app_data_imported = Some(AppDataImported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Nomad.snbak".into(),
            digests: 0,
            groups: 0,
            marks: 0,
        });
        accounts.save(&a).unwrap();
    }

    fn clear_app_data_imported(&self) {
        let accounts = Accounts::new(self.data());
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.app_data_imported = None;
        accounts.save(&a).unwrap();
    }

    fn user_id(&self) -> u64 {
        Accounts::new(self.data())
            .get("jordi")
            .unwrap()
            .unwrap()
            .user_id
    }

    /// Store records as an import would (origin `cli`).
    fn seed(&self, digests: &[Digest], groups: &[DigestGroup]) {
        let store = self.state.store("jordi").unwrap();
        let g = store.lock_recovered(&self.state.journal).unwrap();
        let app = AppData::new(&store);
        for grp in groups {
            app.put_digest_group(&g, grp, Origin::Cli, None, &self.state.journal)
                .unwrap();
        }
        for d in digests {
            app.put_digest(&g, d, Origin::Cli, None, &self.state.journal)
                .unwrap();
        }
    }

    fn stored(&self) -> (Vec<Digest>, Vec<DigestGroup>) {
        let store = self.state.store("jordi").unwrap();
        let g = store.lock_recovered(&self.state.journal).unwrap();
        let app = AppData::new(&store);
        (app.digests(&g).unwrap(), app.digest_groups(&g).unwrap())
    }

    fn events(&self) -> Vec<Event> {
        self.state
            .journal
            .read_from(0)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    async fn raw(&self, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Raw {
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("host", "192.168.1.10:19072");
        if let Some(t) = token {
            req = req.header("x-access-token", t);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
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

    /// A request with a raw body and content type (multipart uploads, signed GETs).
    async fn bytes(&self, method: &str, uri: &str, content_type: &str, body: Vec<u8>) -> Raw {
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .header("host", "192.168.1.10:19072")
            .body(Body::from(body))
            .unwrap();
        let resp = router(self.state.clone())
            .layer(MockConnectInfo(addr))
            .oneshot(req)
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

    /// `POST <signed upload URL>` with `bytes` as the multipart field `file`.
    async fn send_mark(&self, url: &str, bytes: &[u8]) -> Raw {
        let (ct, body) = multipart(bytes);
        self.bytes("POST", &path_and_query(url), &ct, body).await
    }

    /// `GET <signed download URL>`.
    async fn fetch(&self, url: &str) -> Raw {
        self.bytes("GET", &path_and_query(url), "", Vec::new())
            .await
    }

    fn holder(&self) -> Option<String> {
        self.state.sync_locks.holder("jordi", self.state.now_ms())
    }

    async fn call(&self, method: &str, path: &str, token: &str, body: Value) -> Value {
        let r = self.raw(method, path, Some(token), Some(body)).await;
        assert_eq!(r.status, StatusCode::OK, "{method} {path}: {}", r.text());
        r.json()
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> Value {
        self.call("POST", path, token, body).await
    }

    async fn put(&self, path: &str, token: &str, body: Value) -> Value {
        self.call("PUT", path, token, body).await
    }

    async fn delete(&self, path: &str, token: &str, body: Value) -> Value {
        self.call("DELETE", path, token, body).await
    }

    /// random/code, login/equipment and bindEquipment; returns the token.
    async fn login(&self, serial: &str) -> String {
        let ch = self
            .raw(
                "POST",
                "/api/official/user/query/random/code",
                None,
                Some(json!({"account": EMAIL, "version": "202407"})),
            )
            .await
            .json();
        let code = ch["randomCode"].as_str().unwrap();
        let hash = expected_login_hash(&md5_hex("secret"), code);
        let login = self
            .raw(
                "POST",
                "/api/official/user/account/login/equipment",
                None,
                Some(
                    json!({"account": EMAIL, "password": hash, "equipmentNo": serial,
                       "equipment": 3, "loginMethod": "2",
                       "timestamp": ch["timestamp"], "version": "202407"}),
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

    /// The flags of the last logged request to `path`.
    fn last_flags(&self, path: &str) -> Vec<String> {
        let entry = self
            .log()
            .into_iter()
            .rfind(|e| e["kind"] == "request" && e["path"] == path)
            .unwrap_or_else(|| panic!("no request to {path} logged"));
        entry["flags"]
            .as_array()
            .map(|a| a.iter().map(|f| f.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }
}

fn assert_ok(v: &Value) {
    assert_eq!(v["success"], true, "{v}");
    assert!(v["errorCode"].is_null(), "{v}");
}

fn assert_err(v: &Value, code: &str) {
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(v["errorCode"], code, "{v}");
    assert!(v["errorMsg"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
}

fn metadata(unique: &str) -> String {
    format!(r#"{{"author":"Jordi","note_page":"3","unique_identifier":"{unique}"}}"#)
}

/// An imported digest.
fn digest(id: u64, unique: &str, group: &str, creation: i64) -> Digest {
    Digest {
        id,
        user_id: 0,
        unique_identifier: Some(String::new()),
        parent_unique_identifier: Some(group.into()),
        content: Some(format!("excerpt {id}")),
        source_path: Some("Note/x.note".into()),
        source_type: Some(2),
        md5_hash: Some(format!("{id:032x}")),
        metadata: Some(metadata(unique)),
        comment_str: Some(String::new()),
        comment_handwrite_name: Some(String::new()),
        handwrite_inner_name: Some(String::new()),
        handwrite_md5: Some(String::new()),
        creation_time: Some(creation),
        last_modified_time: Some(creation),
        ..Digest::default()
    }
}

fn group(id: u64, unique: &str, name: &str, creation: i64) -> DigestGroup {
    DigestGroup {
        id,
        unique_identifier: Some(unique.into()),
        name: Some(name.into()),
        description: Some(String::new()),
        md5_hash: Some(format!("{id:032x}")),
        creation_time: Some(creation),
        last_modified_time: Some(creation),
        // As the import sets it: the device's creationTime when the backup has no createTime.
        create_time: creation as u64,
        ..DigestGroup::default()
    }
}

/// An `add/summary` body as the device sends it (5.4 step 2).
fn add_body(unique: &str, content: &str) -> Value {
    json!({
        "uniqueIdentifier": "",
        "fileId": null,
        "parentUniqueIdentifier": "",
        "content": content,
        "dataSource": null,
        "sourcePath": "Document/Books/y.epub",
        "sourceType": 1,
        "tags": "",
        "md5Hash": "11112222333344445555666677778888",
        "metadata": format!(r#"{{"document_location_data":"[]","source_size":3344196,"unique_identifier":"{unique}","author":"Jordi"}}"#),
        "commentStr": "",
        "commentHandwriteName": "",
        "handwriteInnerName": "",
        "handwriteMD5": "",
        "creationTime": T0 - 5_000,
        "lastModifiedTime": T0 - 5_000,
        "author": "Jordi",
        "equipmentNo": SERIAL,
    })
}

/// A multipart body with a text field and then the field `file`.
fn multipart(bytes: &[u8]) -> (String, Vec<u8>) {
    let b = "notedownTestBoundary7MA4YWxk";
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhello\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{MARK}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

/// `http://host:port/a/b?c` -> `/a/b?c`.
fn path_and_query(url: &str) -> String {
    let rest = url.split_once("://").unwrap().1;
    rest[rest.find('/').unwrap()..].to_string()
}

fn md5_of(bytes: &[u8]) -> String {
    use md5::Digest as _;
    const_hex::encode(md5::Md5::digest(bytes))
}

/// The URL with one character of its `signature` changed.
fn tampered(url: &str) -> String {
    let at = url.find("signature=").unwrap() + "signature=".len();
    let flipped = if &url[at..=at] == "0" { "1" } else { "0" };
    format!("{}{flipped}{}", &url[..at], &url[at + 1..])
}

fn ids_of(list: &Value) -> Vec<u64> {
    list.as_array()
        .unwrap_or_else(|| panic!("no list: {list}"))
        .iter()
        .map(|d| d["id"].as_u64().unwrap())
        .collect()
}

/// The `digest.*` and `digest_group.*` events of devices.
fn digest_events(s: &Server) -> Vec<Event> {
    s.events()
        .into_iter()
        .filter(|e| e.origin == Origin::Device && e.body.type_name().starts_with("digest"))
        .collect()
}

// ------------------------------------------------------------------ 5.4 flow

#[tokio::test]
async fn the_sync_flow_of_5_4() {
    let s = Server::new();
    s.seed(
        &[
            digest(101, "aaaa", "g1", T0 - 50_000),
            digest(102, "bbbb", "", T0 - 40_000),
        ],
        &[group(100, "g1", "Reading", T0 - 60_000)],
    );
    let t = s.login(SERIAL).await;
    let uid = s.user_id();

    // 1. all groups, the hashes, then the records
    let groups = s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await;
    assert_ok(&groups);
    assert_eq!(groups["totalRecords"], 1);
    let g = &groups["summaryDOList"][0];
    assert_eq!(g["id"], 100);
    assert_eq!(g["uniqueIdentifier"], "g1");
    assert_eq!(g["name"], "Reading");
    assert_eq!(g["isSummaryGroup"], "Y");
    assert_eq!(g["userId"], s.user_id());
    assert_eq!(g["isDeleted"], "N");
    assert_eq!(g["userId"], uid);
    // an imported group: createTime from the backup, updateTime the time of the import
    assert_eq!(g["createTime"], T0 - 60_000);
    assert_eq!(g["updateTime"], T0);

    let hash = s.post(HASH, &t, json!({"page": 1, "size": 100})).await;
    assert_ok(&hash);
    assert_eq!(hash["totalRecords"], 2);
    let infos = &hash["summaryInfoVOList"];
    assert_eq!(ids_of(infos), vec![102, 101]);
    let info = &infos[1];
    assert_eq!(info["userId"], uid);
    assert_eq!(info["md5Hash"], format!("{:032x}", 101));
    assert_eq!(info["handwriteMd5"], "");
    assert_eq!(info["commentHandwriteName"], "");
    assert_eq!(info["lastModifiedTime"], T0 - 50_000);
    assert_eq!(info["metadataMap"]["unique_identifier"], "aaaa");
    assert_eq!(info["metadataMap"]["note_page"], "3");

    let by_id = s.post(BY_ID, &t, json!({"ids": [101]})).await;
    assert_ok(&by_id);
    let d = &by_id["summaryDOList"][0];
    assert_eq!(d["id"], 101);
    assert_eq!(d["userId"], uid);
    assert_eq!(d["content"], "excerpt 101");
    assert_eq!(d["parentUniqueIdentifier"], "g1");
    assert_eq!(d["isSummaryGroup"], "N");
    assert_eq!(d["handwriteMD5"], "");
    assert!(d.get("created_at_ms").is_none(), "{d}");
    assert!(d.get("updated_at_ms").is_none(), "{d}");

    // 2. a new digest on the device
    let added = s.post(ADD, &t, add_body("cccc", "new excerpt")).await;
    assert_ok(&added);
    let id = added["id"].as_u64().unwrap();
    assert_eq!(id, 103, "the next id after the imported ones");

    // 4. an edit: a typed comment, the full record again
    let mut edit = add_body("cccc", "new excerpt");
    edit["id"] = json!(id);
    edit["commentStr"] = json!("my comment");
    edit["parentUniqueIdentifier"] = json!("g1");
    edit.as_object_mut().unwrap().remove("creationTime");
    edit.as_object_mut().unwrap().remove("uniqueIdentifier");
    edit.as_object_mut().unwrap().remove("fileId");
    s.advance_ms(1_000);
    assert_ok(&s.put(UPDATE, &t, edit).await);
    let d = &s.post(BY_ID, &t, json!({"ids": [id]})).await["summaryDOList"][0];
    assert_eq!(d["commentStr"], "my comment");
    assert_eq!(d["parentUniqueIdentifier"], "g1");
    assert_eq!(
        d["creationTime"],
        T0 - 5_000,
        "kept: update/summary has none"
    );
    assert_eq!(d["uniqueIdentifier"], "");
    assert_eq!(d["createTime"], T0 as u64);
    assert_eq!(d["updateTime"], T0 as u64 + 1_000);
    assert!(d.get("equipmentNo").is_none(), "{d}");

    // 6. a delete on the device
    assert_ok(&s.delete(DELETE, &t, json!({"id": id})).await);
    let hash = s.post(HASH, &t, json!({"page": 1, "size": 100})).await;
    assert_eq!(ids_of(&hash["summaryInfoVOList"]), vec![102, 101]);
    assert_eq!(
        s.post(BY_ID, &t, json!({"ids": [id]})).await["summaryDOList"],
        json!([])
    );
    let (items, _) = s.stored();
    let gone = items.iter().find(|d| d.id == id).unwrap();
    assert_eq!(gone.is_deleted, "Y", "soft delete, the file stays");

    // every write journaled with origin device and the serial
    let events = digest_events(&s);
    let kinds: Vec<(&str, u64)> = events
        .iter()
        .map(|e| match &e.body {
            EventBody::DigestCreated(i) => ("created", i.id),
            EventBody::DigestUpdated(i) => ("updated", i.id),
            EventBody::DigestDeleted(i) => ("deleted", i.id),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![("created", id), ("updated", id), ("deleted", id)]
    );
    for e in &events {
        assert_eq!(e.device.as_deref(), Some(SERIAL));
        assert_eq!(e.account, "jordi");
    }
    if let EventBody::DigestCreated(i) = &events[0].body {
        assert_eq!(i.unique_identifier, "cccc");
        assert_eq!(i.source_path, "Document/Books/y.epub");
    }
}

// ------------------------------------------------------------------ D3 identity and upsert

#[tokio::test]
async fn a_re_add_with_a_known_identity_updates_and_answers_the_id() {
    let s = Server::new();
    s.seed(&[digest(7, "dddd", "", T0 - 1_000)], &[]);
    let t = s.login(SERIAL).await;
    let v = s.post(ADD, &t, add_body("dddd", "edited text")).await;
    assert_ok(&v);
    assert_eq!(v["id"], 7);
    let (items, _) = s.stored();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].content.as_deref(), Some("edited text"));
    assert_eq!(items[0].user_id, s.user_id());
    // the same add again: still one record
    let v = s.post(ADD, &t, add_body("dddd", "edited text")).await;
    assert_eq!(v["id"], 7);
    assert_eq!(s.stored().0.len(), 1);
    // a deleted record with that identity is not reused
    assert_ok(&s.delete(DELETE, &t, json!({"id": 7})).await);
    let v = s.post(ADD, &t, add_body("dddd", "again")).await;
    assert_eq!(v["id"], 8);
}

#[tokio::test]
async fn an_update_of_an_unknown_id_creates_it() {
    let s = Server::new();
    s.seed(&[digest(5, "eeee", "", T0 - 1_000)], &[]);
    let t = s.login(SERIAL).await;
    let mut body = add_body("ffff", "back from the device");
    body["id"] = json!(BIG_ID);
    let v = s.put(UPDATE, &t, body).await;
    assert_ok(&v);
    assert!(s.last_flags(UPDATE).contains(&"digest_adopted".to_string()));
    let (items, _) = s.stored();
    assert_eq!(items.iter().filter(|d| d.id == BIG_ID).count(), 1);
    assert_eq!(items.len(), 2);
    let hash = s.post(HASH, &t, json!({"page": 1, "size": 20})).await;
    assert_eq!(ids_of(&hash["summaryInfoVOList"]), vec![5, BIG_ID]);
    let created = digest_events(&s);
    assert!(matches!(&created[0].body, EventBody::DigestCreated(i) if i.id == BIG_ID));
    // an update of a known id is no adoption
    let mut body = add_body("eeee", "edit");
    body["id"] = json!(5);
    assert_ok(&s.put(UPDATE, &t, body).await);
    assert!(!s.last_flags(UPDATE).contains(&"digest_adopted".to_string()));
}

#[tokio::test]
async fn an_update_revives_a_digest_the_server_deleted() {
    let s = Server::new();
    s.seed(&[digest(5, "eeee", "", T0 - 1_000)], &[]);
    let t = s.login(SERIAL).await;
    assert_ok(&s.delete(DELETE, &t, json!({"id": 5})).await);
    let mut body = add_body("eeee", "pushed back");
    body["id"] = json!(5);
    assert_ok(&s.put(UPDATE, &t, body).await);
    assert!(s.last_flags(UPDATE).contains(&"digest_adopted".to_string()));
    let hash = s.post(HASH, &t, json!({"page": 1, "size": 20})).await;
    assert_eq!(ids_of(&hash["summaryInfoVOList"]), vec![5]);
}

#[tokio::test]
async fn a_group_update_of_an_unknown_id_creates_it() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let v = s
        .put(
            GROUP_UPDATE,
            &t,
            json!({"id": 4711, "uniqueIdentifier": "g9", "name": "Physics",
                   "description": "", "md5Hash": "abc", "lastModifiedTime": T0}),
        )
        .await;
    assert_ok(&v);
    assert!(
        s.last_flags(GROUP_UPDATE)
            .contains(&"digest_group_adopted".to_string())
    );
    let groups = s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await;
    assert_eq!(ids_of(&groups["summaryDOList"]), vec![4711]);
    assert_eq!(s.stored().1.len(), 1);
    // the next id is above it
    let v = s
        .post(
            GROUP_ADD,
            &t,
            json!({"uniqueIdentifier": "g10", "name": "Math", "md5Hash": "def",
                   "creationTime": T0, "lastModifiedTime": T0}),
        )
        .await;
    assert_eq!(v["id"], 4712);
}

#[tokio::test]
async fn ids_above_2_pow_53_stay_exact() {
    let s = Server::new();
    s.seed(&[digest(BIG_ID, "1111", "", T0 - 1_000)], &[]);
    let t = s.login(SERIAL).await;
    let r = s
        .raw("POST", HASH, Some(&t), Some(json!({"page": 1, "size": 10})))
        .await;
    assert!(
        r.text().contains(&format!("\"id\":{BIG_ID}")),
        "{}",
        r.text()
    );
    assert_eq!(
        r.json()["summaryInfoVOList"][0]["id"].as_u64(),
        Some(BIG_ID)
    );
    let r = s
        .raw("POST", BY_ID, Some(&t), Some(json!({"ids": [BIG_ID]})))
        .await;
    assert!(
        r.text().contains(&format!("\"id\":{BIG_ID}")),
        "{}",
        r.text()
    );
    // and a new digest gets the id after it
    let v = s.post(ADD, &t, add_body("2222", "x")).await;
    assert_eq!(v["id"].as_u64(), Some(BIG_ID + 1));
    let raw = s
        .raw("POST", ADD, Some(&t), Some(add_body("3333", "y")))
        .await;
    assert!(raw.text().contains(&format!("\"id\":{}", BIG_ID + 2)));
}

// ------------------------------------------------------------------ D1: never the sync lock

#[tokio::test]
async fn a_digest_call_never_revives_a_lapsed_sync_lock() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    assert_eq!(
        s.state
            .sync_locks
            .holder("jordi", s.state.now_ms())
            .as_deref(),
        Some(SERIAL)
    );
    s.advance_ms(30_001);
    assert_ok(&s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await);
    assert_ok(&s.post(ADD, &t, add_body("aaaa", "x")).await);
    assert_eq!(s.state.sync_locks.holder("jordi", s.state.now_ms()), None);
}

#[tokio::test]
async fn a_digest_call_never_extends_a_live_sync_lock() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    s.advance_ms(20_000);
    assert_ok(&s.post(HASH, &t, json!({"page": 1, "size": 20})).await);
    assert_ok(&s.post(ADD, &t, add_body("aaaa", "x")).await);
    s.advance_ms(10_001);
    assert_eq!(s.state.sync_locks.holder("jordi", s.state.now_ms()), None);
}

#[tokio::test]
async fn digest_calls_pass_while_another_device_holds_the_sync_lock() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    assert_ok(&s.post(START, &a, json!({"equipmentNo": SERIAL})).await);
    let v = s
        .post(ADD, &b, {
            let mut body = add_body("aaaa", "x");
            body["equipmentNo"] = json!(OTHER);
            body
        })
        .await;
    assert_ok(&v);
    assert_eq!(
        s.state
            .sync_locks
            .holder("jordi", s.state.now_ms())
            .as_deref(),
        Some(SERIAL)
    );
}

// ------------------------------------------------------------------ D7 paging

#[tokio::test]
async fn page_0_size_0_answers_everything() {
    let s = Server::new();
    s.seed(
        &[],
        &[
            group(1, "g1", "A", T0 - 3_000),
            group(2, "g2", "B", T0 - 1_000),
            group(3, "g3", "C", T0 - 2_000),
        ],
    );
    let t = s.login(SERIAL).await;
    let v = s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await;
    assert_ok(&v);
    assert_eq!(ids_of(&v["summaryDOList"]), vec![2, 3, 1]);
    assert_eq!(v["totalRecords"], 3);
    assert_eq!(v["totalPages"], 1);
    assert_eq!(v["currentPage"], 0);
    assert_eq!(v["pageSize"], 0);
    // an empty body too
    let v = s.post(GROUP_QUERY, &t, json!({})).await;
    assert_eq!(ids_of(&v["summaryDOList"]), vec![2, 3, 1]);
    // nothing at all: one page
    let empty = Server::new();
    let t = empty.login(SERIAL).await;
    let v = empty
        .post(GROUP_QUERY, &t, json!({"page": 0, "size": 0}))
        .await;
    assert_eq!(v["summaryDOList"], json!([]));
    assert_eq!(v["totalRecords"], 0);
    assert_eq!(v["totalPages"], 1);
}

#[tokio::test]
async fn paging_size_2_over_5_records() {
    let s = Server::new();
    // creation times out of id order; 4 and 5 tie, the larger id first
    s.seed(
        &[
            digest(1, "a1", "", T0 - 5_000),
            digest(2, "a2", "", T0 - 1_000),
            digest(3, "a3", "", T0 - 3_000),
            digest(4, "a4", "", T0 - 2_000),
            digest(5, "a5", "", T0 - 2_000),
        ],
        &[],
    );
    let t = s.login(SERIAL).await;
    let mut seen = Vec::new();
    for page in 1..=3 {
        let v = s.post(QUERY, &t, json!({"page": page, "size": 2})).await;
        assert_ok(&v);
        assert_eq!(v["totalRecords"], 5);
        assert_eq!(v["totalPages"], 3);
        assert_eq!(v["currentPage"], page);
        assert_eq!(v["pageSize"], 2);
        seen.extend(ids_of(&v["summaryDOList"]));
    }
    assert_eq!(seen, vec![2, 5, 4, 3, 1]);
    let v = s.post(QUERY, &t, json!({"page": 4, "size": 2})).await;
    assert_eq!(v["summaryDOList"], json!([]));
    // page 0 counts as page 1; numbers as strings are read too
    let v = s.post(HASH, &t, json!({"page": "0", "size": "2"})).await;
    assert_eq!(ids_of(&v["summaryInfoVOList"]), vec![2, 5]);
}

#[tokio::test]
async fn queries_filter_by_group_and_ids_and_skip_deleted() {
    let s = Server::new();
    s.seed(
        &[
            digest(1, "a1", "g1", T0 - 5_000),
            digest(2, "a2", "g2", T0 - 4_000),
            digest(3, "a3", "g1", T0 - 3_000),
            Digest {
                is_deleted: "Y".into(),
                ..digest(4, "a4", "g1", T0 - 2_000)
            },
        ],
        &[],
    );
    let t = s.login(SERIAL).await;
    let v = s
        .post(
            QUERY,
            &t,
            json!({"page": 1, "size": 20, "parentUniqueIdentifier": "g1"}),
        )
        .await;
    assert_eq!(ids_of(&v["summaryDOList"]), vec![3, 1]);
    let v = s
        .post(
            HASH,
            &t,
            json!({"page": 1, "size": 20, "parentUniqueIdentifier": "", "ids": [1, "2", 4]}),
        )
        .await;
    assert_eq!(ids_of(&v["summaryInfoVOList"]), vec![2, 1]);
    let v = s.post(BY_ID, &t, json!({"ids": [4, 3, 99]})).await;
    assert_eq!(ids_of(&v["summaryDOList"]), vec![3]);
    let v = s.post(BY_ID, &t, json!({})).await;
    assert_eq!(v["summaryDOList"], json!([]));
}

// ------------------------------------------------------------------ metadataMap, D10

#[tokio::test]
async fn metadata_map_keeps_integers_and_bad_metadata_is_empty() {
    let s = Server::new();
    s.seed(
        &[Digest {
            metadata: Some("not json {".into()),
            ..digest(1, "", "", T0 - 9_000)
        }],
        &[],
    );
    let t = s.login(SERIAL).await;
    assert_ok(&s.post(ADD, &t, add_body("abcd", "doc")).await);
    let r = s
        .raw("POST", HASH, Some(&t), Some(json!({"page": 1, "size": 10})))
        .await;
    assert!(r.text().contains("\"source_size\":3344196"), "{}", r.text());
    let v = r.json();
    let list = v["summaryInfoVOList"].as_array().unwrap();
    assert_eq!(
        list[0]["metadataMap"]["source_size"].as_u64(),
        Some(3344196)
    );
    assert_eq!(list[0]["metadataMap"]["document_location_data"], "[]");
    assert_eq!(list[1]["id"], 1);
    assert_eq!(list[1]["metadataMap"], json!({}));
}

#[tokio::test]
async fn unknown_fields_round_trip() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let mut body = add_body("abcd", "doc");
    body["futureField"] = json!({"nested": [1, 2], "big": BIG_ID});
    body["flagX"] = json!("Y");
    let id = s.post(ADD, &t, body).await["id"].as_u64().unwrap();
    let d = &s.post(BY_ID, &t, json!({"ids": [id]})).await["summaryDOList"][0];
    assert_eq!(d["futureField"]["nested"], json!([1, 2]));
    assert_eq!(d["futureField"]["big"].as_u64(), Some(BIG_ID));
    assert_eq!(d["flagX"], "Y");
    assert!(d.get("equipmentNo").is_none(), "{d}");

    let v = s
        .post(
            GROUP_ADD,
            &t,
            json!({"uniqueIdentifier": "g1", "name": "A", "md5Hash": "m",
                   "creationTime": T0, "lastModifiedTime": T0, "groupColor": 3}),
        )
        .await;
    assert_ok(&v);
    let g = &s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await["summaryDOList"][0];
    assert_eq!(g["groupColor"], 3);
}

// ------------------------------------------------------------------ groups

#[tokio::test]
async fn deleting_a_group_soft_deletes_its_items() {
    let s = Server::new();
    s.seed(
        &[
            digest(11, "a1", "g1", T0 - 5_000),
            digest(12, "a2", "g2", T0 - 4_000),
            digest(13, "a3", "", T0 - 3_000),
            digest(14, "a4", "g1", T0 - 2_000),
        ],
        &[group(1, "g1", "A", T0), group(2, "g2", "B", T0)],
    );
    let t = s.login(SERIAL).await;
    assert_ok(&s.delete(GROUP_DELETE, &t, json!({"id": 1})).await);
    let v = s.post(HASH, &t, json!({"page": 1, "size": 20})).await;
    assert_eq!(ids_of(&v["summaryInfoVOList"]), vec![13, 12]);
    let v = s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await;
    assert_eq!(ids_of(&v["summaryDOList"]), vec![2]);
    let (items, groups) = s.stored();
    assert_eq!(items.len(), 4, "soft deletes keep the files");
    assert_eq!(groups.len(), 2);
    let deleted: Vec<String> = digest_events(&s)
        .iter()
        .map(|e| match &e.body {
            EventBody::DigestDeleted(i) => format!("digest {}", i.id),
            EventBody::DigestGroupDeleted(i) => format!("group {}", i.id),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(deleted, vec!["digest 11", "digest 14", "group 1"]);
    assert_err(&s.delete(GROUP_DELETE, &t, json!({"id": 1})).await, "E0339");
}

#[tokio::test]
async fn deleting_a_group_without_identifier_leaves_ungrouped_items() {
    let s = Server::new();
    s.seed(
        &[digest(11, "a1", "", T0 - 5_000)],
        &[group(1, "", "A", T0)],
    );
    let t = s.login(SERIAL).await;
    assert_ok(&s.delete(GROUP_DELETE, &t, json!({"id": "1"})).await);
    let v = s.post(HASH, &t, json!({"page": 1, "size": 20})).await;
    assert_eq!(ids_of(&v["summaryInfoVOList"]), vec![11]);
}

#[tokio::test]
async fn group_add_update_and_their_errors() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let body = json!({"uniqueIdentifier": "g1", "name": "A", "description": "d",
                      "md5Hash": "m1", "creationTime": T0 - 9, "lastModifiedTime": T0 - 9,
                      "equipmentNo": SERIAL});
    let v = s.post(GROUP_ADD, &t, body.clone()).await;
    assert_ok(&v);
    let id = v["id"].as_u64().unwrap();
    assert_err(&s.post(GROUP_ADD, &t, body).await, "E0338");
    s.advance_ms(1_000);
    let v = s
        .put(
            GROUP_UPDATE,
            &t,
            json!({"id": id, "uniqueIdentifier": "g1", "name": "B", "description": "",
                   "md5Hash": "m2", "lastModifiedTime": T0,
                   "createTime": 5, "updateTime": 6, "userId": 9, "isSummaryGroup": "N"}),
        )
        .await;
    assert_ok(&v);
    assert!(
        !s.last_flags(GROUP_UPDATE)
            .contains(&"digest_group_adopted".to_string())
    );
    let g = &s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await["summaryDOList"][0];
    assert_eq!(g["name"], "B");
    assert_eq!(g["creationTime"], T0 - 9, "kept: the update has none");
    assert_eq!(g["createTime"], T0 as u64, "server time of the add");
    assert_eq!(
        g["updateTime"],
        T0 as u64 + 1_000,
        "server time of the update"
    );
    assert_eq!(g["isSummaryGroup"], "Y");
    assert!(g.get("equipmentNo").is_none(), "{g}");
    let kinds: Vec<&str> = digest_events(&s)
        .iter()
        .map(|e| e.body.type_name())
        .collect();
    assert_eq!(kinds, vec!["digest_group.created", "digest_group.updated"]);
    // a deleted group's identifier may be added again
    assert_ok(&s.delete(GROUP_DELETE, &t, json!({"id": id})).await);
    let v = s
        .post(
            GROUP_ADD,
            &t,
            json!({"uniqueIdentifier": "g1", "name": "A", "md5Hash": "m"}),
        )
        .await;
    assert_ok(&v);
    assert_ne!(v["id"], id);
}

// ------------------------------------------------------------------ tags

#[tokio::test]
async fn tags_add_query_update_delete() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let v = s.post(TAG_ADD, &t, json!({"name": "physics"})).await;
    assert_ok(&v);
    assert_eq!(v["id"], 1);
    let v = s.post(TAG_ADD, &t, json!({"name": "math"})).await;
    assert_eq!(v["id"], 2);
    assert_err(&s.post(TAG_ADD, &t, json!({"name": "math"})).await, "E0335");
    let r = s.raw("GET", TAG_QUERY, Some(&t), None).await;
    let v = r.json();
    assert_ok(&v);
    let list = v["summaryTagDOList"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], 1);
    assert_eq!(list[0]["name"], "physics");
    assert_eq!(list[0]["userId"], s.user_id());
    assert_eq!(list[0]["createdAt"], T0 as u64);
    assert!(list[0]["uniqueIdentifier"].is_string());

    assert_ok(
        &s.put(TAG_UPDATE, &t, json!({"id": 1, "name": "astro"}))
            .await,
    );
    assert_err(
        &s.put(TAG_UPDATE, &t, json!({"id": 9, "name": "x"})).await,
        "E0336",
    );
    assert_err(
        &s.put(TAG_UPDATE, &t, json!({"id": 1, "name": "math"}))
            .await,
        "E0335",
    );
    assert_ok(&s.delete(TAG_DELETE, &t, json!({"id": 2})).await);
    assert_err(&s.delete(TAG_DELETE, &t, json!({"id": 2})).await, "E0336");
    let v = s.raw("GET", TAG_QUERY, Some(&t), None).await.json();
    assert_eq!(v["summaryTagDOList"].as_array().unwrap().len(), 1);
    assert_eq!(v["summaryTagDOList"][0]["name"], "astro");
    // the next id follows the largest id left
    let v = s.post(TAG_ADD, &t, json!({"name": "bio"})).await;
    assert_eq!(v["id"], 2);
}

#[tokio::test]
async fn a_corrupt_tag_file_fails_only_the_tag_routes() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let dir = s.data().join("accounts/jordi/digests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tags.json"), "garbage").unwrap();
    let v = s.raw("GET", TAG_QUERY, Some(&t), None).await.json();
    assert_eq!(v["success"], false, "{v}");
    assert_ok(&s.post(HASH, &t, json!({"page": 1, "size": 20})).await);
    assert_ok(&s.post(ADD, &t, add_body("a", "b")).await);
}

#[tokio::test]
async fn a_corrupt_digest_record_is_skipped() {
    let s = Server::new();
    s.seed(&[digest(1, "a1", "", T0 - 5_000)], &[]);
    let t = s.login(SERIAL).await;
    let dir = s.data().join("accounts/jordi/digests/items");
    std::fs::write(dir.join("2.json"), "garbage").unwrap();
    let v = s.post(HASH, &t, json!({"page": 1, "size": 20})).await;
    assert_eq!(ids_of(&v["summaryInfoVOList"]), vec![1]);
    // the id of the unreadable file is not reused
    let v = s.post(ADD, &t, add_body("zz", "b")).await;
    assert_eq!(v["id"], 3);
}

// ------------------------------------------------------------------ errors

#[tokio::test]
async fn every_error_code() {
    let s = Server::new();
    s.seed(
        &[digest(1, "a1", "", T0 - 5_000)],
        &[group(2, "g", "G", T0)],
    );
    let t = s.login(SERIAL).await;
    assert_err(&s.delete(DELETE, &t, json!({"id": 99})).await, "E0340");
    assert_ok(&s.delete(DELETE, &t, json!({"id": "1"})).await);
    assert_err(&s.delete(DELETE, &t, json!({"id": 1})).await, "E0340");
    assert_err(
        &s.delete(GROUP_DELETE, &t, json!({"id": 99})).await,
        "E0339",
    );
    assert_err(
        &s.post(
            GROUP_ADD,
            &t,
            json!({"uniqueIdentifier": "g", "name": "X", "md5Hash": "m"}),
        )
        .await,
        "E0338",
    );
    // bad parameters: 422, never a panic
    let bad: Vec<(&str, &str, Value)> = vec![
        ("PUT", UPDATE, json!({"content": "no id"})),
        ("PUT", UPDATE, json!({"id": 0, "content": "x"})),
        ("PUT", UPDATE, json!({"id": -3})),
        ("PUT", UPDATE, json!({"id": 5, "content": 17})),
        ("PUT", UPDATE, json!({"id": 5, "creationTime": "yesterday"})),
        ("POST", ADD, json!({"content": ["a"]})),
        ("POST", ADD, json!({"sourceType": "x"})),
        ("POST", ADD, json!([1, 2])),
        ("DELETE", DELETE, json!({})),
        ("DELETE", DELETE, json!({"id": "abc"})),
        ("POST", QUERY, json!({"page": "x"})),
        ("POST", HASH, json!({"ids": "1,2"})),
        ("POST", BY_ID, json!({"ids": [{"a": 1}]})),
        (
            "POST",
            GROUP_ADD,
            json!({"uniqueIdentifier": "g7", "name": "X"}),
        ),
        ("POST", GROUP_ADD, json!({"name": 5, "md5Hash": "m"})),
        ("PUT", GROUP_UPDATE, json!({"name": "no id"})),
        ("DELETE", GROUP_DELETE, json!({})),
        ("POST", GROUP_QUERY, json!({"size": true})),
        ("POST", TAG_ADD, json!({})),
        ("POST", TAG_ADD, json!({"name": ""})),
        ("PUT", TAG_UPDATE, json!({"name": "x"})),
        ("PUT", TAG_UPDATE, json!({"id": 1})),
        ("DELETE", TAG_DELETE, json!({})),
    ];
    for (method, path, body) in bad {
        let v = s.call(method, path, &t, body).await;
        assert_err(&v, "422");
    }
    // an empty body reads as {}: an add of an empty digest is fine
    let r = s.raw("POST", ADD, Some(&t), None).await.json();
    assert_ok(&r);
    // nothing of the bad calls was stored
    let (items, groups) = s.stored();
    assert_eq!(items.len(), 2);
    assert_eq!(groups.len(), 1);
}

#[tokio::test]
async fn a_body_that_is_no_json_is_a_bad_parameter() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(ADD)
        .header("x-access-token", &t)
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = router(s.state.clone())
        .layer(MockConnectInfo(addr))
        .oneshot(req)
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_err(&v, "422");
}

#[tokio::test]
async fn equipment_no_must_be_the_tokens_device() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let mut body = add_body("a", "b");
    body["equipmentNo"] = json!(OTHER);
    assert_err(&s.post(ADD, &t, body).await, "E0312");
    assert_err(
        &s.post(
            GROUP_QUERY,
            &t,
            json!({"page": 0, "size": 0, "equipmentNo": OTHER}),
        )
        .await,
        "E0312",
    );
    assert!(s.stored().0.is_empty());
}

// ------------------------------------------------------------------ the gate

#[tokio::test]
async fn without_a_token_e0712() {
    let s = Server::new();
    let v = s
        .raw(
            "POST",
            GROUP_QUERY,
            None,
            Some(json!({"page": 0, "size": 0})),
        )
        .await
        .json();
    assert_err(&v, "E0712");
    let v = s
        .raw("PUT", UPDATE, Some("garbage"), Some(json!({"id": 1})))
        .await
        .json();
    assert_err(&v, "E0712");
}

#[tokio::test]
async fn the_gate_refuses_until_the_app_data_import() {
    // protect on, nothing imported: the file gate
    let s = Server::with_config(Config::default());
    let t = s.login(SERIAL).await;
    let v = s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await;
    assert_err(&v, "E0079");
    assert!(
        s.last_flags(GROUP_QUERY)
            .contains(&"protect_refused".to_string())
    );

    // files imported, app data not: the app data gate, on every route
    s.mark_imported();
    let calls: Vec<(&str, &str, Option<Value>)> = vec![
        ("POST", GROUP_QUERY, Some(json!({"page": 0, "size": 0}))),
        ("POST", HASH, Some(json!({"page": 1, "size": 20}))),
        ("POST", QUERY, Some(json!({"page": 1, "size": 20}))),
        ("POST", BY_ID, Some(json!({"ids": [1]}))),
        ("POST", ADD, Some(add_body("a", "b"))),
        ("PUT", UPDATE, Some(json!({"id": 1}))),
        ("DELETE", DELETE, Some(json!({"id": 1}))),
        (
            "POST",
            GROUP_ADD,
            Some(json!({"name": "x", "md5Hash": "m"})),
        ),
        ("PUT", GROUP_UPDATE, Some(json!({"id": 1}))),
        ("DELETE", GROUP_DELETE, Some(json!({"id": 1}))),
        ("POST", TAG_ADD, Some(json!({"name": "x"}))),
        ("PUT", TAG_UPDATE, Some(json!({"id": 1, "name": "y"}))),
        ("DELETE", TAG_DELETE, Some(json!({"id": 1}))),
        ("GET", TAG_QUERY, None),
    ];
    for (method, path, body) in &calls {
        let v = s.raw(method, path, Some(&t), body.clone()).await.json();
        assert_err(&v, "E0079");
        let flags = s.last_flags(path);
        assert!(
            flags.contains(&"app_data_refused".to_string()),
            "{path}: {flags:?}"
        );
    }
    assert!(!s.data().join("accounts/jordi/digests").exists());
    assert!(digest_events(&s).is_empty());

    // both markers: open
    s.mark_app_data_imported();
    assert_ok(&s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await);
    assert_ok(&s.post(ADD, &t, add_body("a", "b")).await);
}

#[tokio::test]
async fn with_protect_off_the_routes_are_open() {
    let s = Server::with_config(Config {
        protect: false,
        ..Config::default()
    });
    let t = s.login(SERIAL).await;
    assert_ok(&s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await);
    let v = s.post(ADD, &t, add_body("a", "b")).await;
    assert_ok(&v);
    assert_eq!(v["id"], 1);
}

#[tokio::test]
async fn refuse_mode_and_the_transfer_routes_stay_refused() {
    let s = Server::with_config(Config {
        sync: SyncMode::Refuse,
        ..Config::default()
    });
    s.mark_imported();
    s.mark_app_data_imported();
    let t = s.login(SERIAL).await;
    assert_err(
        &s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await,
        "E0079",
    );
    assert_err(&s.put(UPDATE, &t, json!({"id": 1})).await, "E0079");
    let v = s.raw("GET", TAG_QUERY, Some(&t), None).await.json();
    assert_err(&v, "E0079");

    let v = s
        .post(
            MARK_APPLY,
            &t,
            json!({"fileName": MARK, "equipmentNo": SERIAL}),
        )
        .await;
    assert_err(&v, "E0079");
    let v = s.post(MARK_DOWNLOAD, &t, json!({"id": 1})).await;
    assert_err(&v, "E0079");
    for (method, path) in [("POST", MARK_UPLOAD), ("GET", MARK_GET)] {
        let v = s.raw(method, path, Some(&t), None).await.json();
        assert_err(&v, "E0079");
    }

    // another method on a digest path is refused like any file path
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let v = s.raw("GET", ADD, Some(&t), None).await.json();
    assert_err(&v, "E0079");
}

// ------------------------------------------------------------------ 5.4 steps 3 and 5, 5.5, D9

/// `upload/apply/summary` for `name`; asserts success and returns the answer.
async fn apply_mark(s: &Server, t: &str, name: &str) -> Value {
    let v = s
        .post(
            MARK_APPLY,
            t,
            json!({"fileName": name, "equipmentNo": SERIAL}),
        )
        .await;
    assert_ok(&v);
    v
}

/// The update of 5.4 step 3: the digest `id` names the comment `name` with `md5`.
fn update_with_mark(id: u64, name: &str, md5: &str) -> Value {
    let mut body = add_body("aaaa", "x");
    body["id"] = json!(id);
    body["commentHandwriteName"] = json!(name);
    body["handwriteInnerName"] = json!(name);
    body["handwriteMD5"] = json!(md5);
    body
}

#[tokio::test]
async fn a_handwritten_comment_round_trip() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    // A lapsed sync lock of this device stays lapsed through every transfer (D1).
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    s.advance_ms(30_001);
    assert_eq!(s.holder(), None);

    let id = s.post(ADD, &t, add_body("aaaa", "x")).await["id"]
        .as_u64()
        .unwrap();
    let a = apply_mark(&s, &t, MARK).await;
    let url = a["fullUploadUrl"].as_str().unwrap();
    assert!(
        url.starts_with("http://192.168.1.10:19072/api/oss/upload/summary?path="),
        "{url}"
    );
    assert_eq!(a["partUploadUrl"], "");
    assert_eq!(a["innerName"], MARK);

    let bytes: Vec<u8> = (0..29_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let r = s.send_mark(url, &bytes).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let v = r.json();
    assert_ok(&v);
    assert_eq!(v["md5"], md5_of(&bytes));
    assert_eq!(v["innerName"], MARK);
    let stored = s.data().join("accounts/jordi/digests/marks").join(MARK);
    assert_eq!(std::fs::read(&stored).unwrap(), bytes);
    assert_eq!(s.holder(), None);

    // Nothing links the bytes before the update (5.4 step 3).
    assert_err(&s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await, "E0321");
    let md5 = md5_of(&bytes);
    assert_ok(&s.put(UPDATE, &t, update_with_mark(id, MARK, &md5)).await);
    let (digests, _) = s.stored();
    assert_eq!(digests[0].comment_handwrite_name.as_deref(), Some(MARK));
    assert_eq!(digests[0].handwrite_md5.as_deref(), Some(md5.as_str()));

    let d = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await;
    assert_ok(&d);
    let get = d["url"].as_str().unwrap();
    assert!(
        get.starts_with("http://192.168.1.10:19072/api/oss/download/summary?path="),
        "{get}"
    );
    let r = s.fetch(get).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.body, bytes);

    // A changed comment keeps its name: the upload replaces the bytes.
    let a = apply_mark(&s, &t, MARK).await;
    let r = s
        .send_mark(a["fullUploadUrl"].as_str().unwrap(), b"changed")
        .await;
    assert_ok(&r.json());
    assert_eq!(s.fetch(get).await.body, b"changed");
    // No temp file is left beside the comment.
    let names: Vec<_> = std::fs::read_dir(stored.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from(MARK)]);
    assert_eq!(s.holder(), None);
}

#[tokio::test]
async fn a_bad_mark_name_is_refused() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let long = format!("{}.mark", "a".repeat(60));
    for name in [
        "",
        ".mark",
        ".hidden.mark",
        "x.txt",
        "../x.mark",
        "a/b.mark",
        "a\\b.mark",
        "a\0b.mark",
        long.as_str(),
    ] {
        let v = s
            .post(
                MARK_APPLY,
                &t,
                json!({"fileName": name, "equipmentNo": SERIAL}),
            )
            .await;
        assert_err(&v, "E0324");
    }
    let v = s.post(MARK_APPLY, &t, json!({"equipmentNo": SERIAL})).await;
    assert_err(&v, "E0324");
    // 64 bytes is the limit.
    apply_mark(&s, &t, &format!("{}.mark", "a".repeat(59))).await;
    assert!(!s.data().join("accounts/jordi/digests/marks").exists());
}

#[tokio::test]
async fn download_without_handwriting_or_digest() {
    let s = Server::new();
    let mut gone = digest(7, "gone", "", T0);
    gone.comment_handwrite_name = Some(MARK.into());
    let mut bad = digest(8, "bad", "", T0);
    bad.comment_handwrite_name = Some("../../account.json".into());
    let mut deleted = digest(9, "deleted", "", T0);
    deleted.is_deleted = "Y".into();
    s.seed(&[digest(5, "plain", "", T0), gone, bad, deleted], &[]);
    let t = s.login(SERIAL).await;
    for id in [5, 7, 8] {
        let v = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await;
        assert_err(&v, "E0321");
    }
    for id in [9, 404] {
        let v = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await;
        assert_err(&v, "E0340");
    }
}

#[tokio::test]
async fn a_tampered_or_foreign_signature_is_e1306() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let a = apply_mark(&s, &t, MARK).await;
    let up = a["fullUploadUrl"].as_str().unwrap().to_string();
    for url in [
        tampered(&up),
        "http://h/api/oss/upload/summary?path=abc".to_string(),
        "http://h/api/oss/upload/summary".to_string(),
        // the file upload's URL shape is no mark URL
        up.replace("/upload/summary?", "/upload?"),
    ] {
        let r = s.send_mark(&url, b"x").await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{url}");
        assert_eq!(r.text(), "E1306", "{url}");
    }
    assert!(
        !s.data()
            .join("accounts/jordi/digests/marks")
            .join(MARK)
            .exists()
    );

    let r = s.send_mark(&up, b"x").await;
    assert_ok(&r.json());
    let id = s.post(ADD, &t, add_body("aaaa", "x")).await["id"]
        .as_u64()
        .unwrap();
    assert_ok(
        &s.put(UPDATE, &t, update_with_mark(id, MARK, &md5_of(b"x")))
            .await,
    );
    let get = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await["url"]
        .as_str()
        .unwrap()
        .to_string();
    for url in [
        tampered(&get),
        "http://h/api/oss/download/summary?path=abc".to_string(),
        // the file download's URL shape is no mark URL
        get.replace("/download/summary?", "/download?"),
    ] {
        let r = s.fetch(&url).await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{url}");
        assert_eq!(r.text(), "E1306", "{url}");
    }
    // An upload URL is no download URL.
    let r = s
        .fetch(&up.replace("/upload/summary?", "/download/summary?"))
        .await;
    assert_eq!(r.text(), "E1306");
    // A new login raises the token generation: older URLs stop working.
    s.login(SERIAL).await;
    assert_eq!(s.fetch(&get).await.text(), "E1306");
    assert_eq!(s.send_mark(&up, b"y").await.text(), "E1306");
}

#[tokio::test]
async fn the_gate_covers_the_transfers() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let id = s.post(ADD, &t, add_body("aaaa", "x")).await["id"]
        .as_u64()
        .unwrap();
    let up = apply_mark(&s, &t, MARK).await["fullUploadUrl"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(&s.send_mark(&up, b"x").await.json());
    assert_ok(
        &s.put(UPDATE, &t, update_with_mark(id, MARK, &md5_of(b"x")))
            .await,
    );
    let get = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await["url"]
        .as_str()
        .unwrap()
        .to_string();

    s.clear_app_data_imported();
    let v = s
        .post(
            MARK_APPLY,
            &t,
            json!({"fileName": MARK, "equipmentNo": SERIAL}),
        )
        .await;
    assert_err(&v, "E0079");
    assert!(
        s.last_flags(MARK_APPLY)
            .contains(&"app_data_refused".to_string())
    );
    assert_err(&s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await, "E0079");
    // The signed account decides for the byte transfers.
    let v = s.send_mark(&up, b"other").await.json();
    assert_err(&v, "E0079");
    assert!(
        s.last_flags(MARK_UPLOAD)
            .contains(&"app_data_refused".to_string())
    );
    let v = s.fetch(&get).await.json();
    assert_err(&v, "E0079");
    assert!(
        s.last_flags(MARK_GET)
            .contains(&"app_data_refused".to_string())
    );
    let stored = s.data().join("accounts/jordi/digests/marks").join(MARK);
    assert_eq!(std::fs::read(stored).unwrap(), b"x");

    // With protect off the transfers are open without the marker.
    let s = Server::with_config(Config {
        protect: false,
        ..Config::default()
    });
    let t = s.login(SERIAL).await;
    let up = apply_mark(&s, &t, MARK).await["fullUploadUrl"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(&s.send_mark(&up, b"x").await.json());
}

#[tokio::test]
async fn a_mark_transfer_never_extends_a_live_sync_lock() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let id = s.post(ADD, &t, add_body("aaaa", "x")).await["id"]
        .as_u64()
        .unwrap();
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    // 20 s into the 30 s lock: a touch here would carry it past the second step.
    s.advance_ms(20_000);
    let up = apply_mark(&s, &t, MARK).await["fullUploadUrl"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(&s.send_mark(&up, b"x").await.json());
    assert_ok(
        &s.put(UPDATE, &t, update_with_mark(id, MARK, &md5_of(b"x")))
            .await,
    );
    let get = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await["url"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(s.fetch(&get).await.body, b"x");
    assert_eq!(s.holder().as_deref(), Some(SERIAL));
    s.advance_ms(10_001);
    assert_eq!(s.holder(), None);
}

#[tokio::test]
async fn a_mark_upload_over_32_mib_is_refused() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let up = apply_mark(&s, &t, MARK).await["fullUploadUrl"]
        .as_str()
        .unwrap()
        .to_string();
    let big = vec![7u8; 32 * 1024 * 1024 + 1];
    let v = s.send_mark(&up, &big).await.json();
    assert_err(&v, "E0324");
    assert!(
        s.last_flags(MARK_UPLOAD)
            .contains(&"upload_too_large".to_string())
    );
    let marks = s.data().join("accounts/jordi/digests/marks");
    assert_eq!(std::fs::read_dir(&marks).unwrap().count(), 0);
    // exactly 32 MiB passes
    let v = s.send_mark(&up, &big[1..]).await.json();
    assert_ok(&v);
}

#[tokio::test]
async fn a_root_file_named_like_the_mark_target_still_downloads() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let files = s.data().join("accounts/jordi/files");
    std::fs::write(files.join("digest-mark"), b"file bytes").unwrap();
    let all = s
        .post(
            "/api/file/2/files/list_folder",
            &t,
            json!({"path": "/", "recursive": true}),
        )
        .await;
    let id = all["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path_display"] == "digest-mark")
        .unwrap_or_else(|| panic!("no digest-mark in {all}"))["id"]
        .clone();
    let v = s
        .post(
            "/api/file/3/files/download_v3",
            &t,
            json!({"equipmentNo": SERIAL, "id": id}),
        )
        .await;
    assert_ok(&v);
    let url = v["url"].as_str().unwrap();
    let r = s.fetch(url).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.body, b"file bytes");
    // A real file URL on the mark route is no mark URL.
    let r = s
        .fetch(&url.replace("/api/oss/download?", "/api/oss/download/summary?"))
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.text(), "E1306");
}

#[tokio::test]
async fn a_root_folder_named_like_the_mark_target_still_takes_uploads() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    std::fs::create_dir_all(s.data().join("accounts/jordi/files/digest-mark")).unwrap();
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    let a = s
        .post(
            "/api/file/3/files/upload/apply",
            &t,
            json!({"equipmentNo": SERIAL, "path": "/digest-mark/x.txt",
                   "fileName": "x.txt", "size": 1}),
        )
        .await;
    assert_ok(&a);
    let url = a["fullUploadUrl"].as_str().unwrap();
    // A real file URL on the mark route is no mark URL.
    let r = s
        .send_mark(
            &url.replace("/api/oss/upload?", "/api/oss/upload/summary?"),
            b"x",
        )
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.text(), "E1306");
    assert!(
        !s.data()
            .join("accounts/jordi/digests/marks")
            .join(MARK)
            .exists()
    );
    let r = s.send_mark(url, b"x").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_ok(&r.json());
}

// ------------------------------------------------------------------ server-set fields and ids

#[tokio::test]
async fn server_set_fields_of_a_wrong_type_are_ignored() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let mut body = add_body("aaaa", "x");
    body["userId"] = json!("not a number");
    body["createTime"] = json!(-5);
    body["updateTime"] = json!("yesterday");
    let v = s.post(ADD, &t, body).await;
    assert_ok(&v);
    let id = v["id"].as_u64().unwrap();
    s.advance_ms(1_000);
    let v = s
        .put(
            UPDATE,
            &t,
            json!({"id": id, "content": "y", "userId": -1, "createTime": "x",
                   "updateTime": -7}),
        )
        .await;
    assert_ok(&v);
    let d = &s.post(BY_ID, &t, json!({"ids": [id]})).await["summaryDOList"][0];
    assert_eq!(d["content"], "y");
    assert_eq!(d["userId"], s.user_id());
    assert_eq!(d["createTime"], T0 as u64);
    assert_eq!(d["updateTime"], T0 as u64 + 1_000);
}

#[tokio::test]
async fn ids_above_i64_max_are_refused() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let too_big = i64::MAX as u64 + 1;
    let bad: Vec<(&str, &str, Value)> = vec![
        ("PUT", UPDATE, json!({"id": too_big, "content": "x"})),
        (
            "PUT",
            UPDATE,
            json!({"id": too_big.to_string(), "content": "x"}),
        ),
        ("PUT", UPDATE, json!({"id": u64::MAX, "content": "x"})),
        ("DELETE", DELETE, json!({"id": too_big})),
        ("PUT", GROUP_UPDATE, json!({"id": too_big, "md5Hash": "m"})),
        ("DELETE", GROUP_DELETE, json!({"id": too_big})),
        ("PUT", TAG_UPDATE, json!({"id": too_big, "name": "x"})),
        ("DELETE", TAG_DELETE, json!({"id": too_big})),
        ("POST", MARK_DOWNLOAD, json!({"id": too_big})),
        ("POST", QUERY, json!({"ids": [too_big]})),
        ("POST", HASH, json!({"ids": [too_big.to_string()]})),
        ("POST", BY_ID, json!({"ids": [1, too_big]})),
    ];
    for (method, path, body) in bad {
        let v = s.call(method, path, &t, body.clone()).await;
        assert_err(&v, "422");
    }
    assert!(s.stored().0.is_empty());
    // i64::MAX itself is a valid id
    let v = s
        .put(UPDATE, &t, json!({"id": i64::MAX, "content": "x"}))
        .await;
    assert_ok(&v);
    assert_eq!(s.stored().0[0].id, i64::MAX as u64);
}

// ------------------------------------------------------------------ import, then the device

#[tokio::test]
async fn the_device_sees_the_backup_after_the_import() {
    use super::appdata::{MARK_A, MARK_B, backup_entries, snbak};
    use notedown_cloud::appdata::{AppImportOptions, import_app_data};

    let s = Server::with_config(Config::default());
    s.mark_imported();
    let file = s.data().join("device.snbak");
    snbak(&file, &backup_entries());
    let r = import_app_data(
        s.data(),
        "jordi",
        Some(&file),
        &AppImportOptions::default(),
        &s.state.journal,
    )
    .unwrap();
    assert_eq!((r.digests, r.groups, r.marks), (2, 1, 2));
    let t = s.login(SERIAL).await;

    // The hash query answers what the backup holds, ids exact. The digest that never reached
    // a cloud is not there: the device adds it itself.
    let raw = s
        .raw("POST", HASH, Some(&t), Some(json!({"page": 0, "size": 0})))
        .await;
    let text = raw.text();
    assert!(text.contains("\"id\":895334188016730112"), "{text}");
    assert!(text.contains("\"id\":895334188016730200"), "{text}");
    let v = raw.json();
    assert_ok(&v);
    assert_eq!(v["totalRecords"], 2);
    let list = v["summaryInfoVOList"].as_array().unwrap();
    // Newest creationTime first: the document, then the note.
    let expect = [
        (
            895_334_188_016_730_200u64,
            "m2",
            "h2",
            MARK_B,
            1_700_000_003_000i64,
        ),
        (
            895_334_188_016_730_112u64,
            "m1",
            "h1",
            MARK_A,
            1_700_000_001_000i64,
        ),
    ];
    for (row, (id, md5, hw, name, modified)) in list.iter().zip(expect) {
        assert_eq!(row["id"].as_u64(), Some(id), "{row}");
        assert_eq!(row["userId"], s.user_id(), "{row}");
        assert_eq!(row["md5Hash"], md5, "{row}");
        assert_eq!(row["handwriteMd5"], hw, "{row}");
        assert_eq!(row["commentHandwriteName"], name, "{row}");
        assert_eq!(row["lastModifiedTime"], modified, "{row}");
    }
    assert_eq!(
        list[1]["metadataMap"],
        json!({"author": "Jordi", "unique_identifier": "n1", "source_size": 3344196})
    );

    // The full records.
    let d = &s
        .post(BY_ID, &t, json!({"ids": [895_334_188_016_730_112u64]}))
        .await["summaryDOList"][0];
    assert_eq!(d["parentUniqueIdentifier"], "g1");
    assert_eq!(d["handwriteInnerName"], MARK_A);
    assert_eq!(d["handwriteMD5"], "h1");
    assert_eq!(d["creationTime"], 1_700_000_000_000i64);

    // The handwritten comments download with their bytes and their name.
    for (id, name, bytes) in [
        (895_334_188_016_730_112u64, MARK_A, &b"mark A"[..]),
        (895_334_188_016_730_200u64, MARK_B, &b"mark B"[..]),
    ] {
        let v = s.post(MARK_DOWNLOAD, &t, json!({"id": id})).await;
        assert_ok(&v);
        let r = s.fetch(v["url"].as_str().unwrap()).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        assert_eq!(r.body, bytes);
        let disposition = r.headers["content-disposition"].to_str().unwrap();
        assert!(disposition.contains(name), "{disposition}");
    }

    // The group keeps its id and its createTime through a device edit.
    let groups = s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await;
    let g = &groups["summaryDOList"][0];
    assert_eq!(g["id"].as_u64(), Some(895_334_188_016_730_300));
    assert_eq!(g["uniqueIdentifier"], "g1");
    assert_eq!(g["createTime"], 1_700_000_000_000i64);
    s.advance_ms(1_000);
    let v = s
        .put(
            GROUP_UPDATE,
            &t,
            json!({"id": 895_334_188_016_730_300u64, "uniqueIdentifier": "g1",
                   "name": "Ideas 2", "md5Hash": "gm2"}),
        )
        .await;
    assert_ok(&v);
    let g = &s.post(GROUP_QUERY, &t, json!({"page": 0, "size": 0})).await["summaryDOList"][0];
    assert_eq!(g["name"], "Ideas 2");
    assert_eq!(g["createTime"], 1_700_000_000_000i64);
    assert_eq!(g["updateTime"], T0 as u64 + 1_000);
}
