//! Folders, deletes, moves and copies in accept mode: `create_folder_v2`, `delete_folder_v3`,
//! `move_v3` and `copy_v3` (`docs/sync-protocol.md` 3.5, 7.3), the recycle bin of deletes, the
//! prune of emptied folders (never the native ones) and the crash repair of these commits.
//! Requests go through the router in memory (`tower::oneshot`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use md5::{Digest, Md5};
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::journal::{Journal, Origin};
use notedown_cloud::server::{AppState, router};
use notedown_cloud::store::{
    CrashPoint, PENDING_FILE, Recovery, RecycleEntry, RecycleReason, Store, WirePath,
};
use notedown_cloud::{Clock, Config};
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
const QUERY: &str = "/api/file/3/files/query_v3";
const CAPACITY: &str = "/api/file/capacity/query";
const CREATE: &str = "/api/file/2/files/create_folder_v2";
const DELETE: &str = "/api/file/3/files/delete_folder_v3";
const MOVE: &str = "/api/file/3/files/move_v3";
const COPY: &str = "/api/file/3/files/copy_v3";

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

fn md5_of(b: &[u8]) -> String {
    const_hex::encode(Md5::digest(b))
}

fn wp(s: &str) -> WirePath {
    WirePath::parse_named(s).unwrap()
}

fn test1() -> Vec<u8> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join("tests/fixtures/notes/test1.note")).unwrap()
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

fn assert_ok(v: &Value) {
    assert_eq!(v["success"], true, "{v}");
    assert!(v["errorCode"].is_null(), "{v}");
}

fn assert_err(v: &Value, code: &str) {
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(v["errorCode"], code, "{v}");
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

/// Whether the file system of `dir` ignores case (macOS APFS by default).
fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe.tmp");
    std::fs::write(&probe, b"x").unwrap();
    let folds = dir.join("caseprobe.tmp").exists();
    std::fs::remove_file(probe).unwrap();
    folds
}

impl Server {
    fn new() -> Server {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let clock = Arc::new(TestClock(AtomicI64::new(T0)));
        let config = Config {
            protect: false,
            ..Config::default()
        };
        seed_imported(dir.path());
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

    /// A file placed by hand (it gets its id at the next listing).
    fn put(&self, path: &str, bytes: &[u8]) {
        let p = self.files().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// Write a note through a store put with its page index, as an upload does.
    fn index_note(&self, path: &str, bytes: &[u8]) {
        let store = self.state.store("jordi").unwrap();
        let g = store.lock_recovered(&self.state.journal).unwrap();
        let staged = self.account().join("staged-note");
        std::fs::write(&staged, bytes).unwrap();
        let mut c = store
            .plan_put(&g, &wp(path), &staged, Origin::Cli, None)
            .unwrap();
        notedown_cloud::pages::plan_page_events(&store, &g, &mut c);
        store.commit(&g, c, &self.state.journal).unwrap();
    }

    /// The page records as (file name, content).
    fn records(&self) -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = std::fs::read_dir(self.account().join("pages"))
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> Value {
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("x-access-token", token)
            .header("content-type", "application/json")
            .header("host", "192.168.1.10:19072")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router(self.state.clone())
            .layer(MockConnectInfo(addr))
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_raw(&self, path: &str, token: &str, body: Value) -> Value {
        self.post(path, token, body).await
    }

    async fn login(&self) -> String {
        self.login_as(SERIAL).await
    }

    async fn login_as(&self, serial: &str) -> String {
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let call = |uri: &str, body: Value| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let send = |req: Request<Body>| async {
            let resp = router(self.state.clone())
                .layer(MockConnectInfo(addr))
                .oneshot(req)
                .await
                .unwrap();
            let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&b).unwrap()
        };
        let ch = send(call(
            "/api/official/user/query/random/code",
            json!({"account": EMAIL, "version": "202407"}),
        ))
        .await;
        let code = ch["randomCode"].as_str().unwrap();
        let hash = expected_login_hash(&md5_hex("secret"), code);
        let login = send(call(
            "/api/official/user/account/login/equipment",
            json!({"account": EMAIL, "password": hash, "equipmentNo": serial,
                   "equipment": 3, "loginMethod": "2",
                   "timestamp": ch["timestamp"], "version": "202407"}),
        ))
        .await;
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
        let t = self.login().await;
        assert_ok(&self.post(START, &t, json!({"equipmentNo": SERIAL})).await);
        t
    }

    /// The whole tree as `path_display -> id`.
    async fn tree(&self, t: &str) -> Vec<(String, String)> {
        let v = self
            .post(
                LIST,
                t,
                json!({"equipmentNo": SERIAL, "path": "/", "recursive": true}),
            )
            .await;
        assert_ok(&v);
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["path_display"].as_str().unwrap().to_string(),
                    e["id"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    async fn id_of(&self, t: &str, path: &str) -> String {
        self.tree(t)
            .await
            .into_iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("{path} is not listed"))
            .1
    }

    async fn listed(&self, t: &str, path: &str) -> bool {
        self.tree(t).await.iter().any(|(p, _)| p == path)
    }

    async fn delete(&self, t: &str, id: &str) -> Value {
        self.post(DELETE, t, json!({"equipmentNo": SERIAL, "id": id}))
            .await
    }

    async fn mv(&self, t: &str, id: &str, to: &str, autorename: bool) -> Value {
        self.post(
            MOVE,
            t,
            json!({"equipmentNo": SERIAL, "id": id, "to_path": to, "autorename": autorename}),
        )
        .await
    }

    async fn cp(&self, t: &str, id: &str, to: &str, autorename: bool) -> Value {
        self.post(
            COPY,
            t,
            json!({"equipmentNo": SERIAL, "id": id, "to_path": to, "autorename": autorename}),
        )
        .await
    }

    async fn mkdir(&self, t: &str, path: &str, autorename: bool) -> Value {
        self.post(
            CREATE,
            t,
            json!({"equipmentNo": SERIAL, "path": path, "autorename": autorename}),
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

    /// Events after the first `n`.
    fn events_after(&self, n: usize) -> Vec<Value> {
        self.events().into_iter().skip(n).collect()
    }

    fn recycle_index(&self) -> Vec<RecycleEntry> {
        let store = Store::open(self.data(), "jordi").unwrap();
        let g = store.lock().unwrap();
        store.recycle_index(&g).unwrap()
    }

    /// Every protocol log line, oldest first.
    fn log_lines(&self) -> Vec<Value> {
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

    /// The marker lines with `flag`.
    fn markers(&self, flag: &str) -> Vec<Value> {
        self.log_lines()
            .into_iter()
            .filter(|e| e["kind"] == "marker" && e["flags"].to_string().contains(flag))
            .collect()
    }

    fn log_flags(&self, path: &str) -> Vec<Vec<String>> {
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
            .filter(|e| e["kind"] == "request" && e["path"] == path)
            .map(|e| {
                e["flags"]
                    .as_array()
                    .map(|a| a.iter().map(|f| f.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default()
            })
            .collect()
    }
}

// ------------------------------------------------------------------ create_folder_v2

#[tokio::test]
async fn create_folder_answers_a_real_id_and_journals_it() {
    let s = Server::new();
    let t = s.session().await;
    let v = s.mkdir(&t, "/NOTE/Note/Work", false).await;
    assert_ok(&v);
    assert_eq!(v["equipmentNo"], SERIAL);
    let m = &v["metadata"];
    assert_eq!(m["tag"], "folder");
    assert_eq!(m["name"], "Work");
    assert_eq!(m["path_display"], "NOTE/Note/Work");
    let id = m["id"].as_str().expect("the id is a string");
    assert!(id.parse::<u64>().unwrap() >= 1000);
    assert!(s.files().join("NOTE/Note/Work").is_dir());
    assert_eq!(s.id_of(&t, "NOTE/Note/Work").await, id);
    let ev = s.events();
    let last = ev.last().unwrap();
    assert_eq!(last["type"], "folder.created");
    assert_eq!(last["path"], "NOTE/Note/Work");
    assert_eq!(last["id"].as_u64().unwrap().to_string(), id);
    assert_eq!(last["origin"], "device");
    assert_eq!(last["device"], SERIAL);
}

#[tokio::test]
async fn create_folder_refuses_a_taken_name_or_renames_it() {
    let s = Server::new();
    let t = s.session().await;
    assert_ok(&s.mkdir(&t, "/NOTE/Note/Work", false).await);
    let n = s.events().len();
    // the same name, a case variant and a file of that name are taken
    assert_err(&s.mkdir(&t, "/NOTE/Note/Work", false).await, "E0322");
    assert_err(&s.mkdir(&t, "/NOTE/Note/work", false).await, "E0322");
    s.put("NOTE/Note/Plan", b"a file");
    assert_err(&s.mkdir(&t, "/NOTE/Note/Plan", false).await, "E0322");
    // dot names are never stored, also not with autorename
    assert_err(&s.mkdir(&t, "/NOTE/Note/.hidden", false).await, "E0322");
    assert_err(&s.mkdir(&t, "/NOTE/Note/.hidden", true).await, "E0322");
    assert_eq!(s.events().len(), n, "nothing journaled");
    // autorename: name(1), name(2)
    let a = s.mkdir(&t, "/NOTE/Note/Work", true).await;
    assert_ok(&a);
    assert_eq!(a["metadata"]["path_display"], "NOTE/Note/Work(1)");
    assert_eq!(a["metadata"]["name"], "Work(1)");
    let b = s.mkdir(&t, "//NOTE/Note/Work/", true).await;
    assert_eq!(b["metadata"]["path_display"], "NOTE/Note/Work(2)");
    // a case variant is taken too; autorename looks for a free caseless name
    assert_ok(&s.mkdir(&t, "/INBOX/Box", false).await);
    let c = s.mkdir(&t, "/INBOX/box", true).await;
    assert_eq!(c["metadata"]["path_display"], "INBOX/box(1)");
    assert!(s.listed(&t, "NOTE/Note/Work(2)").await);
    assert_eq!(
        types(&s.events_after(n)),
        [
            "folder.created",
            "folder.created",
            "folder.created",
            "folder.created"
        ]
    );
}

#[tokio::test]
async fn create_folder_creates_missing_parents_first() {
    let s = Server::new();
    let t = s.session().await;
    let v = s.mkdir(&t, "/INBOX/A/B", false).await;
    assert_ok(&v);
    let ev = s.events();
    let tail: Vec<&Value> = ev.iter().rev().take(2).rev().collect();
    assert_eq!(tail[0]["type"], "folder.created");
    assert_eq!(tail[0]["path"], "INBOX/A");
    assert_eq!(tail[1]["path"], "INBOX/A/B");
    assert!(s.listed(&t, "INBOX/A").await);
}

// ------------------------------------------------------------------ delete_folder_v3

#[tokio::test]
async fn a_deleted_file_goes_to_the_recycle_bin_with_an_index_line() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/a.note", b"hello");
    let id = s.id_of(&t, "NOTE/Note/a.note").await;
    let used = s.post(CAPACITY, &t, json!({})).await["usedCapacity"].clone();
    assert_eq!(used, 5);
    let n = s.events().len();
    let v = s.delete(&t, &id).await;
    assert_ok(&v);
    let m = &v["metadata"];
    assert_eq!(m["tag"], "file");
    assert_eq!(m["id"], id.as_str());
    assert_eq!(m["name"], "a.note");
    assert_eq!(m["path_display"], "NOTE/Note/a.note");
    // the bytes are in recycle/<ms>/<path>, nothing is left in files/
    assert!(!s.files().join("NOTE/Note/a.note").exists());
    let slot = s.account().join(format!("recycle/{T0}/NOTE/Note/a.note"));
    assert_eq!(std::fs::read(&slot).unwrap(), b"hello");
    // the index line, exactly
    let index = std::fs::read_to_string(s.account().join("recycle/index.jsonl")).unwrap();
    assert_eq!(
        index,
        format!(
            "{{\"id\":{id},\"path\":\"NOTE/Note/a.note\",\"md5\":\"{}\",\"deletedAt\":{T0},\
             \"reason\":\"deleted\"}}\n",
            md5_of(b"hello")
        )
    );
    let entries = s.recycle_index();
    assert_eq!(
        entries,
        [RecycleEntry {
            id: id.parse().unwrap(),
            path: "NOTE/Note/a.note".into(),
            md5: md5_of(b"hello"),
            deleted_at: T0 as u64,
            reason: RecycleReason::Deleted,
        }]
    );
    assert_eq!(entries[0].location(), format!("{T0}/NOTE/Note/a.note"));
    // the event, and NOTE/Note stays (a native folder)
    let ev = s.events_after(n);
    assert_eq!(types(&ev), ["file.deleted"]);
    assert_eq!(ev[0]["path"], "NOTE/Note/a.note");
    assert_eq!(ev[0]["recycle"], format!("{T0}/NOTE/Note/a.note"));
    assert_eq!(ev[0]["id"].as_u64().unwrap().to_string(), id);
    assert!(s.listed(&t, "NOTE/Note").await);
    assert!(!s.listed(&t, "NOTE/Note/a.note").await);
    // the recycle bin is not listed; the id is gone; capacity is current at once
    assert!(s.tree(&t).await.iter().all(|(p, _)| !p.contains("recycle")));
    let q = s.post(QUERY, &t, json!({"id": id})).await;
    assert!(q["entriesVO"].is_null(), "{q}");
    assert_eq!(s.post(CAPACITY, &t, json!({})).await["usedCapacity"], 0);
    // a second delete of the same id (a retry) is an unknown id
    assert_err(&s.delete(&t, &id).await, "E0318");
}

#[tokio::test]
async fn a_folder_delete_arrives_per_file_and_prunes_the_emptied_user_folders() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/Proj/Sub/a.note", b"a");
    s.put("NOTE/Note/Proj/b.note", b"b");
    s.put("NOTE/Note/keep.note", b"k");
    s.put("Trip/Day/x.pdf", b"x");
    s.put("INBOX/in.pdf", b"i");
    s.put("DOCUMENT/Document/d.pdf", b"d");
    s.put("EXPORT/e.png", b"e");
    let a = s.id_of(&t, "NOTE/Note/Proj/Sub/a.note").await;
    let sub = s.id_of(&t, "NOTE/Note/Proj/Sub").await;
    let b = s.id_of(&t, "NOTE/Note/Proj/b.note").await;
    let proj = s.id_of(&t, "NOTE/Note/Proj").await;
    let n = s.events().len();

    // the first file: its folder Sub is empty now and is pruned in the same commit
    assert_ok(&s.delete(&t, &a).await);
    let ev = s.events_after(n);
    assert_eq!(types(&ev), ["file.deleted", "folder.deleted"]);
    assert_eq!(ev[1]["path"], "NOTE/Note/Proj/Sub");
    assert_eq!(ev[1]["id"].as_u64().unwrap().to_string(), sub);
    assert_eq!(ev[0]["opId"], ev[1]["opId"]);
    assert!(!s.files().join("NOTE/Note/Proj/Sub").exists());
    assert!(s.listed(&t, "NOTE/Note/Proj").await);

    // the last file: Proj goes too, NOTE/Note stays
    assert_ok(&s.delete(&t, &b).await);
    let ev = s.events_after(n + 2);
    assert_eq!(types(&ev), ["file.deleted", "folder.deleted"]);
    assert_eq!(ev[1]["path"], "NOTE/Note/Proj");
    assert_eq!(ev[1]["id"].as_u64().unwrap().to_string(), proj);
    assert!(!s.listed(&t, "NOTE/Note/Proj").await);
    assert!(s.listed(&t, "NOTE/Note").await);
    assert!(s.listed(&t, "NOTE/Note/keep.note").await);

    // a user folder at the root is pruned up to the root
    let x = s.id_of(&t, "Trip/Day/x.pdf").await;
    assert_ok(&s.delete(&t, &x).await);
    let ev = s.events_after(n + 4);
    assert_eq!(
        types(&ev),
        ["file.deleted", "folder.deleted", "folder.deleted"]
    );
    assert_eq!(ev[1]["path"], "Trip/Day");
    assert_eq!(ev[2]["path"], "Trip");
    assert!(!s.files().join("Trip").exists());

    // the last files of native folders: they all stay, and so do their parents
    for path in ["INBOX/in.pdf", "DOCUMENT/Document/d.pdf", "EXPORT/e.png"] {
        let id = s.id_of(&t, path).await;
        assert_ok(&s.delete(&t, &id).await);
    }
    s.post(END, &t, json!({"flag": "Y"})).await;
    let t = s.session().await;
    let tree = s.tree(&t).await;
    let paths: Vec<&str> = tree.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "DOCUMENT",
            "DOCUMENT/Document",
            "EXPORT",
            "INBOX",
            "NOTE",
            "NOTE/MyStyle",
            "NOTE/Note",
            "NOTE/Note/keep.note",
            "SCREENSHOT",
        ]
    );
    // pruned folders are removed for good: no recycle entry for them
    let index = s.recycle_index();
    assert_eq!(index.len(), 6);
    assert!(index.iter().all(|e| !e.md5.is_empty()));
}

#[tokio::test]
async fn a_folder_with_only_dot_files_goes_to_the_recycle_bin_with_them() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/Proj/Sub/a.note", b"a");
    s.put("NOTE/Note/Proj/Sub/.DS_Store", b"sub mac");
    s.put("NOTE/Note/Proj/.DS_Store", b"mac");
    let a = s.id_of(&t, "NOTE/Note/Proj/Sub/a.note").await;
    let proj = s.id_of(&t, "NOTE/Note/Proj").await;
    let n = s.events().len();
    assert_ok(&s.delete(&t, &a).await);
    let ev = s.events_after(n);
    assert_eq!(
        types(&ev),
        ["file.deleted", "folder.deleted", "folder.deleted"]
    );
    assert_eq!(ev[1]["path"], "NOTE/Note/Proj/Sub");
    assert_eq!(ev[2]["path"], "NOTE/Note/Proj");
    assert!(!s.files().join("NOTE/Note/Proj").exists());
    assert!(!s.listed(&t, "NOTE/Note/Proj").await);
    // the dot-files are kept: the topmost pruned folder moved with them to its own slot
    let slot = s
        .account()
        .join(format!("recycle/{}/NOTE/Note/Proj", T0 + 1));
    assert_eq!(std::fs::read(slot.join(".DS_Store")).unwrap(), b"mac");
    assert_eq!(
        std::fs::read(slot.join("Sub/.DS_Store")).unwrap(),
        b"sub mac"
    );
    assert_eq!(
        std::fs::read(
            s.account()
                .join(format!("recycle/{T0}/NOTE/Note/Proj/Sub/a.note"))
        )
        .unwrap(),
        b"a"
    );
    let index = s.recycle_index();
    assert_eq!(index.len(), 2);
    assert_eq!(index[0].path, "NOTE/Note/Proj/Sub/a.note");
    assert_eq!(index[1].path, "NOTE/Note/Proj");
    assert_eq!(index[1].id.to_string(), proj);
    assert_eq!(index[1].md5, "");
    assert_eq!(index[1].deleted_at, T0 as u64 + 1);
    for e in &index {
        assert!(
            s.account().join("recycle").join(e.location()).exists(),
            "{e:?}"
        );
    }
}

#[tokio::test]
async fn a_move_out_of_a_folder_with_dot_files_recycles_the_folder() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/M/x.pdf", b"x");
    s.put("NOTE/Note/M/.DS_Store", b"mac");
    let x = s.id_of(&t, "NOTE/Note/M/x.pdf").await;
    let n = s.events().len();
    assert_ok(&s.mv(&t, &x, "/INBOX/x.pdf", false).await);
    assert_eq!(types(&s.events_after(n)), ["file.moved", "folder.deleted"]);
    assert_eq!(
        std::fs::read(
            s.account()
                .join(format!("recycle/{T0}/NOTE/Note/M/.DS_Store"))
        )
        .unwrap(),
        b"mac"
    );
    let index = s.recycle_index();
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].location(), format!("{T0}/NOTE/Note/M"));
}

#[tokio::test]
async fn a_folder_with_a_server_temp_name_is_not_pruned() {
    let s = Server::new();
    let t = s.session().await;
    for (folder, temp) in [("Q", ".nd-move-77-1"), ("R", ".nd-77-1.tmp")] {
        s.put(&format!("NOTE/Note/{folder}/a.pdf"), b"a");
        s.put(&format!("NOTE/Note/{folder}/{temp}"), b"temp");
        let a = s.id_of(&t, &format!("NOTE/Note/{folder}/a.pdf")).await;
        let n = s.events().len();
        assert_ok(&s.delete(&t, &a).await);
        assert_eq!(types(&s.events_after(n)), ["file.deleted"]);
        assert!(
            s.files()
                .join(format!("NOTE/Note/{folder}/{temp}"))
                .is_file()
        );
    }
}

#[tokio::test]
async fn a_folder_with_content_goes_to_the_recycle_bin_as_a_whole() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/Proj/Sub/a.note", b"a");
    s.put("NOTE/Note/Proj/b.note", b"b");
    let proj = s.id_of(&t, "NOTE/Note/Proj").await;
    let a = s.id_of(&t, "NOTE/Note/Proj/Sub/a.note").await;
    let n = s.events().len();
    let v = s.delete(&t, &proj).await;
    assert_ok(&v);
    assert_eq!(v["metadata"]["tag"], "folder");
    let ev = s.events_after(n);
    // the contents first (children before their folder), then the folder; one opId
    assert_eq!(
        types(&ev),
        [
            "file.deleted",
            "file.deleted",
            "folder.deleted",
            "folder.deleted"
        ]
    );
    assert_eq!(ev[0]["path"], "NOTE/Note/Proj/b.note");
    assert_eq!(ev[0]["recycle"], format!("{T0}/NOTE/Note/Proj/b.note"));
    assert_eq!(ev[1]["path"], "NOTE/Note/Proj/Sub/a.note");
    assert_eq!(ev[1]["id"].as_u64().unwrap().to_string(), a);
    assert_eq!(ev[2]["path"], "NOTE/Note/Proj/Sub");
    assert_eq!(ev[3]["path"], "NOTE/Note/Proj");
    assert!(ev.iter().all(|e| e["opId"] == ev[0]["opId"]));
    assert_eq!(
        std::fs::read(
            s.account()
                .join(format!("recycle/{T0}/NOTE/Note/Proj/Sub/a.note"))
        )
        .unwrap(),
        b"a"
    );
    // one line per file, then the folder line, each locating its own bytes
    let index = s.recycle_index();
    let lines: Vec<(&str, &str)> = index
        .iter()
        .map(|e| (e.path.as_str(), e.md5.as_str()))
        .collect();
    let (ma, mb) = (md5_of(b"a"), md5_of(b"b"));
    assert_eq!(
        lines,
        [
            ("NOTE/Note/Proj/b.note", mb.as_str()),
            ("NOTE/Note/Proj/Sub/a.note", ma.as_str()),
            ("NOTE/Note/Proj", ""),
        ]
    );
    assert_eq!(index[1].id.to_string(), a);
    for e in &index {
        assert_eq!(e.deleted_at, T0 as u64);
        assert_eq!(e.reason, RecycleReason::Deleted);
        assert!(
            s.account().join("recycle").join(e.location()).exists(),
            "{e:?}"
        );
    }
    assert!(!s.listed(&t, "NOTE/Note/Proj").await);
    assert_err(&s.delete(&t, &a).await, "E0318");
}

#[tokio::test]
async fn two_deletes_in_one_millisecond_take_separate_slots() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"one");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    assert_ok(&s.delete(&t, &a).await);
    s.put("INBOX/a.pdf", b"two");
    let a2 = s.id_of(&t, "INBOX/a.pdf").await;
    assert_ne!(a, a2, "a deleted id is never reused");
    assert_ok(&s.delete(&t, &a2).await);
    let index = s.recycle_index();
    assert_eq!(index.len(), 2);
    assert_eq!(index[0].deleted_at, T0 as u64);
    assert_eq!(index[1].deleted_at, T0 as u64 + 1);
    for (e, bytes) in index.iter().zip([b"one", b"two"]) {
        let p = s.account().join("recycle").join(e.location());
        assert_eq!(std::fs::read(p).unwrap(), bytes);
    }
}

#[tokio::test]
async fn unknown_ids_get_their_error_codes_and_never_panic() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"a");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    let n = s.events().len();
    for id in [
        json!("999999"),
        json!(999999),
        json!("abc"),
        json!(-1),
        json!(null),
        json!([1]),
    ] {
        let body = |extra: Value| {
            let mut b = json!({"equipmentNo": SERIAL, "id": id.clone()});
            b.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            b
        };
        assert_err(&s.post_raw(DELETE, &t, body(json!({}))).await, "E0318");
        let to = json!({"to_path": "/INBOX/z.pdf", "autorename": false});
        assert_err(&s.post_raw(MOVE, &t, body(to.clone())).await, "E0320");
        assert_err(&s.post_raw(COPY, &t, body(to)).await, "E0308");
    }
    // a registry row whose file is gone on disk is unknown too, and the row is dropped
    std::fs::remove_file(s.files().join("INBOX/a.pdf")).unwrap();
    assert_err(&s.delete(&t, &a).await, "E0318");
    assert_err(&s.mv(&t, &a, "/INBOX/z.pdf", false).await, "E0320");
    assert_err(&s.cp(&t, &a, "/INBOX/z.pdf", false).await, "E0308");
    assert_eq!(s.events().len(), n);
}

#[tokio::test]
async fn native_folders_cannot_be_deleted_or_moved() {
    let s = Server::new();
    let t = s.session().await;
    for path in ["NOTE/Note", "NOTE", "INBOX"] {
        let id = s.id_of(&t, path).await;
        assert_err(&s.delete(&t, &id).await, "422");
        assert_err(&s.mv(&t, &id, "/Elsewhere", false).await, "422");
        for route in [DELETE, MOVE] {
            let flags = s.log_flags(route);
            assert!(
                flags.last().unwrap().contains(&"native_folder".to_string()),
                "{flags:?}"
            );
        }
        assert!(s.files().join(path).is_dir());
    }
}

// ------------------------------------------------------------------ move_v3

#[tokio::test]
async fn a_move_keeps_the_id_creates_parents_and_prunes_the_source_folders() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/Old/Bike.note", b"bike");
    let id = s.id_of(&t, "NOTE/Note/Old/Bike.note").await;
    let old = s.id_of(&t, "NOTE/Note/Old").await;
    let n = s.events().len();
    let v = s
        .mv(&t, &id, "/NOTE/Note/Archive/2026/Bike 2026.note", false)
        .await;
    assert_ok(&v);
    let e = &v["entriesVO"];
    assert_eq!(e["id"], id.as_str());
    assert_eq!(e["tag"], "file");
    assert_eq!(e["name"], "Bike 2026.note");
    assert_eq!(e["path_display"], "NOTE/Note/Archive/2026/Bike 2026.note");
    assert_eq!(e["content_hash"], md5_of(b"bike"));
    let ev = s.events_after(n);
    assert_eq!(
        types(&ev),
        [
            "folder.created",
            "folder.created",
            "file.moved",
            "folder.deleted"
        ]
    );
    assert_eq!(ev[0]["path"], "NOTE/Note/Archive");
    assert_eq!(ev[1]["path"], "NOTE/Note/Archive/2026");
    assert_eq!(ev[2]["path"], "NOTE/Note/Archive/2026/Bike 2026.note");
    assert_eq!(ev[2]["oldPath"], "NOTE/Note/Old/Bike.note");
    assert_eq!(ev[2]["id"].as_u64().unwrap().to_string(), id);
    assert_eq!(ev[3]["path"], "NOTE/Note/Old");
    assert_eq!(ev[3]["id"].as_u64().unwrap().to_string(), old);
    assert!(ev.iter().all(|e| e["opId"] == ev[0]["opId"]));
    assert_eq!(
        s.id_of(&t, "NOTE/Note/Archive/2026/Bike 2026.note").await,
        id
    );
    assert!(!s.listed(&t, "NOTE/Note/Old").await);
    // a rename in place
    let v = s
        .mv(&t, &id, "/NOTE/Note/Archive/2026/Bike.note", false)
        .await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["id"], id.as_str());
    assert_eq!(types(&s.events_after(n + 4)), ["file.moved"]);
}

#[tokio::test]
async fn a_folder_move_keeps_every_id_and_does_not_prune_the_target_way() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/A/W/x.note", b"x");
    let w = s.id_of(&t, "NOTE/Note/A/W").await;
    let x = s.id_of(&t, "NOTE/Note/A/W/x.note").await;
    let n = s.events().len();
    // out of A into a new folder inside A: A is not empty afterwards
    assert_ok(&s.mv(&t, &w, "/NOTE/Note/A/B/W2", false).await);
    let ev = s.events_after(n);
    assert_eq!(types(&ev), ["folder.created", "folder.moved"]);
    assert_eq!(s.id_of(&t, "NOTE/Note/A/B/W2/x.note").await, x);
    assert_eq!(s.id_of(&t, "NOTE/Note/A/B/W2").await, w);
    // then out of A: A and B are pruned
    assert_ok(&s.mv(&t, &w, "/INBOX/W", false).await);
    let ev = s.events_after(n + 2);
    assert_eq!(
        types(&ev),
        ["folder.moved", "folder.deleted", "folder.deleted"]
    );
    assert_eq!(ev[1]["path"], "NOTE/Note/A/B");
    assert_eq!(ev[2]["path"], "NOTE/Note/A");
    assert_eq!(s.id_of(&t, "INBOX/W/x.note").await, x);
}

#[tokio::test]
async fn a_move_onto_a_taken_name_is_refused_or_renamed() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/a.note", b"a");
    s.put("NOTE/Note/Bike.note", b"b");
    let a = s.id_of(&t, "NOTE/Note/a.note").await;
    let n = s.events().len();
    assert_err(&s.mv(&t, &a, "/NOTE/Note/Bike.note", false).await, "E0322");
    assert_err(&s.mv(&t, &a, "/NOTE/Note/bike.note", false).await, "E0322");
    assert_err(&s.mv(&t, &a, "/NOTE/Note/.a.note", false).await, "E0322");
    // a file the registry does not know yet (placed by hand, never listed) is taken too
    s.put("NOTE/Note/unseen.note", b"u");
    assert_err(
        &s.mv(&t, &a, "/NOTE/Note/unseen.note", false).await,
        "E0322",
    );
    assert_eq!(s.events().len(), n);
    assert_eq!(
        std::fs::read(s.files().join("NOTE/Note/unseen.note")).unwrap(),
        b"u"
    );
    // autorename puts the suffix before the extension
    let v = s.mv(&t, &a, "/NOTE/Note/Bike.note", true).await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["path_display"], "NOTE/Note/Bike(1).note");
    assert_eq!(v["entriesVO"]["id"], a.as_str());
    assert_eq!(
        std::fs::read(s.files().join("NOTE/Note/Bike.note")).unwrap(),
        b"b"
    );
}

#[tokio::test]
async fn a_move_onto_a_stale_registry_row_drops_that_row() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"a");
    s.put("INBOX/gone.pdf", b"g");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    let gone = s.id_of(&t, "INBOX/gone.pdf").await;
    std::fs::remove_file(s.files().join("INBOX/gone.pdf")).unwrap();
    let v = s.mv(&t, &a, "/INBOX/gone.pdf", false).await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["id"], a.as_str());
    let q = s.post(QUERY, &t, json!({"id": gone})).await;
    assert!(q["entriesVO"].is_null(), "{q}");
}

#[tokio::test]
async fn a_folder_cannot_move_into_itself() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/W/x.note", b"x");
    let w = s.id_of(&t, "NOTE/Note/W").await;
    let n = s.events().len();
    assert_err(&s.mv(&t, &w, "/NOTE/Note/W/inner", false).await, "E0358");
    assert_err(
        &s.mv(&t, &w, "/NOTE/Note/W/inner/deeper", true).await,
        "E0358",
    );
    assert_err(&s.cp(&t, &w, "/NOTE/Note/W/inner", false).await, "E0358");
    assert_eq!(s.events().len(), n);
    assert!(!s.files().join("NOTE/Note/W/inner").exists());
    // the same path is no move at all
    let v = s.mv(&t, &w, "/NOTE/Note/W", false).await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["id"], w.as_str());
    assert_eq!(s.events().len(), n);
}

#[tokio::test]
async fn a_rename_that_only_changes_case_keeps_the_file_and_the_id() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/bike.note", b"bike");
    s.put("NOTE/Note/work/x.note", b"x");
    let id = s.id_of(&t, "NOTE/Note/bike.note").await;
    let n = s.events().len();
    let v = s.mv(&t, &id, "/NOTE/Note/Bike.note", false).await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["path_display"], "NOTE/Note/Bike.note");
    assert_eq!(v["entriesVO"]["id"], id.as_str());
    let ev = s.events_after(n);
    assert_eq!(types(&ev), ["file.moved"]);
    assert_eq!(ev[0]["oldPath"], "NOTE/Note/bike.note");
    // only the new spelling is there, with the bytes, and no temp name is left behind
    let names: Vec<String> = std::fs::read_dir(s.files().join("NOTE/Note"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"Bike.note".to_string()), "{names:?}");
    assert!(!names.contains(&"bike.note".to_string()), "{names:?}");
    assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
    assert_eq!(
        std::fs::read(s.files().join("NOTE/Note/Bike.note")).unwrap(),
        b"bike"
    );
    assert_eq!(s.id_of(&t, "NOTE/Note/Bike.note").await, id);
    // a folder, too; its content keeps its ids
    let w = s.id_of(&t, "NOTE/Note/work").await;
    let x = s.id_of(&t, "NOTE/Note/work/x.note").await;
    assert_ok(&s.mv(&t, &w, "/NOTE/Note/Work", false).await);
    assert_eq!(s.id_of(&t, "NOTE/Note/Work/x.note").await, x);
    assert!(!s.listed(&t, "NOTE/Note/work").await);
    // a case variant of another file stays refused
    s.put("NOTE/Note/other.note", b"o");
    assert_err(
        &s.mv(&t, &id, "/NOTE/Note/OTHER.note", false).await,
        "E0322",
    );
}

#[tokio::test]
async fn nfd_names_are_kept_byte_exact_and_renamed_to_nfc_in_place() {
    let s = Server::new();
    let t = s.session().await;
    let nfd = "U\u{308}berleben";
    let nfc = "\u{dc}berleben";
    assert_ne!(nfd, nfc);
    let v = s.mkdir(&t, &format!("/INBOX/{nfd}"), false).await;
    assert_ok(&v);
    assert_eq!(v["metadata"]["name"], nfd);
    let id = v["metadata"]["id"].as_str().unwrap().to_string();
    assert_eq!(s.id_of(&t, &format!("INBOX/{nfd}")).await, id);
    // the other normalization of the same name is taken
    assert_err(&s.mkdir(&t, &format!("/INBOX/{nfc}"), false).await, "E0322");
    let n = s.events().len();
    // a rename that only changes the normalization keeps the id and leaves no temp name
    let v = s.mv(&t, &id, &format!("/INBOX/{nfc}"), false).await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["id"], id.as_str());
    assert_eq!(v["entriesVO"]["name"], nfc);
    assert_eq!(types(&s.events_after(n)), ["folder.moved"]);
    let names: Vec<String> = std::fs::read_dir(s.files().join("INBOX"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [nfc]);
    assert_eq!(s.id_of(&t, &format!("INBOX/{nfc}")).await, id);
    assert!(!s.listed(&t, &format!("INBOX/{nfd}")).await);
}

#[tokio::test]
async fn a_moved_note_carries_its_page_record() {
    let s = Server::new();
    let t = s.session().await;
    let note = test1();
    // index the note through a store put, as an upload does
    {
        let store = s.state.store("jordi").unwrap();
        let g = store.lock_recovered(&s.state.journal).unwrap();
        let staged = s.account().join("t1");
        std::fs::write(&staged, &note).unwrap();
        let mut c = store
            .plan_put(&g, &wp("NOTE/Note/t.note"), &staged, Origin::Cli, None)
            .unwrap();
        notedown_cloud::pages::plan_page_events(&store, &g, &mut c);
        store.commit(&g, c, &s.state.journal).unwrap();
    }
    let id = s.id_of(&t, "NOTE/Note/t.note").await;
    assert_ok(&s.mv(&t, &id, "/INBOX/t.note", false).await);
    let records: Vec<Value> = std::fs::read_dir(s.account().join("pages"))
        .unwrap()
        .map(|e| serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["path"], "INBOX/t.note");
    // a delete leaves the record (it heals at the next write), and journals no page events
    let n = s.events().len();
    assert_ok(&s.delete(&t, &id).await);
    assert_eq!(types(&s.events_after(n)), ["file.deleted"]);
}

// ------------------------------------------------------------------ copy_v3

#[tokio::test]
async fn a_copy_gets_a_new_id_and_journals_file_copied() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"pdf bytes");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    let n = s.events().len();
    let v = s
        .cp(&t, &a, "/DOCUMENT/Document/New/a copy.pdf", false)
        .await;
    assert_ok(&v);
    let e = &v["entriesVO"];
    assert_ne!(e["id"], a.as_str());
    assert_eq!(e["path_display"], "DOCUMENT/Document/New/a copy.pdf");
    assert_eq!(e["content_hash"], md5_of(b"pdf bytes"));
    let ev = s.events_after(n);
    assert_eq!(types(&ev), ["folder.created", "file.copied"]);
    assert_eq!(ev[1]["sourceId"].as_u64().unwrap().to_string(), a);
    assert_eq!(
        ev[1]["id"].as_u64().unwrap().to_string(),
        e["id"].as_str().unwrap()
    );
    assert_eq!(ev[1]["path"], "DOCUMENT/Document/New/a copy.pdf");
    // both are listed with their ids; the source is untouched
    assert_eq!(s.id_of(&t, "INBOX/a.pdf").await, a);
    assert_eq!(
        std::fs::read(s.files().join("DOCUMENT/Document/New/a copy.pdf")).unwrap(),
        b"pdf bytes"
    );
    // taken name, and autorename
    assert_err(&s.cp(&t, &a, "/INBOX/a.pdf", false).await, "E0322");
    let r = s.cp(&t, &a, "/INBOX/a.pdf", true).await;
    assert_eq!(r["entriesVO"]["path_display"], "INBOX/a(1).pdf");
    let r = s.cp(&t, &a, "/INBOX/a.pdf", true).await;
    assert_eq!(r["entriesVO"]["path_display"], "INBOX/a(2).pdf");
}

#[tokio::test]
async fn a_copied_note_gets_page_events_after_file_copied() {
    let s = Server::new();
    let t = s.session().await;
    s.index_note("NOTE/Note/t.note", &test1());
    let id = s.id_of(&t, "NOTE/Note/t.note").await;
    let n = s.events().len();
    assert_ok(&s.cp(&t, &id, "/NOTE/Note/t2.note", false).await);
    let ev = s.events_after(n);
    assert_eq!(ev[0]["type"], "file.copied");
    assert!(ev.len() > 1);
    assert!(ev[1..].iter().all(|e| e["type"] == "page.added"), "{ev:?}");
    // two records: the source under its FILE_ID, the copy under its path
    let records = s.records();
    assert_eq!(records.len(), 2, "{records:?}");
    let source = records.iter().find(|(n, _)| n.starts_with('F')).unwrap();
    let copy = records
        .iter()
        .find(|(n, _)| n.starts_with("path-"))
        .unwrap();
    assert_eq!(source.1["path"], "NOTE/Note/t.note");
    assert_eq!(
        format!("{}.json", source.1["fileId"].as_str().unwrap()),
        source.0
    );
    assert_eq!(copy.1["path"], "NOTE/Note/t2.note");
    assert!(copy.1["fileId"].as_str().unwrap().starts_with("path:"));
    assert_eq!(copy.1["noteId"], source.1["fileId"]);
}

#[tokio::test]
async fn a_folder_copy_gives_every_descendant_a_new_id() {
    let s = Server::new();
    let t = s.session().await;
    s.put("NOTE/Note/P/a.pdf", b"a");
    s.put("NOTE/Note/P/S/b.pdf", b"b");
    let p = s.id_of(&t, "NOTE/Note/P").await;
    let before = s.tree(&t).await;
    let n = s.events().len();
    let v = s.cp(&t, &p, "/INBOX/P2", false).await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["tag"], "folder");
    assert_eq!(v["entriesVO"]["path_display"], "INBOX/P2");
    let ev = s.events_after(n);
    assert_eq!(
        types(&ev),
        [
            "folder.created",
            "folder.created",
            "file.copied",
            "file.copied"
        ]
    );
    assert_eq!(ev[0]["path"], "INBOX/P2");
    assert_eq!(ev[1]["path"], "INBOX/P2/S");
    assert_eq!(ev[2]["path"], "INBOX/P2/S/b.pdf");
    assert_eq!(ev[3]["path"], "INBOX/P2/a.pdf");
    let old_ids: Vec<&String> = before.iter().map(|(_, id)| id).collect();
    for (path, id) in s.tree(&t).await {
        if path.starts_with("INBOX/P2") {
            assert!(!old_ids.contains(&&id), "{path} reuses id {id}");
        }
    }
    assert_eq!(
        std::fs::read(s.files().join("INBOX/P2/S/b.pdf")).unwrap(),
        b"b"
    );
    // the source is untouched
    assert_eq!(s.id_of(&t, "NOTE/Note/P").await, p);
    assert!(s.listed(&t, "NOTE/Note/P/S/b.pdf").await);
    // a taken folder name with autorename gets the suffix at the end
    let v = s.cp(&t, &p, "/INBOX/P2", true).await;
    assert_eq!(v["entriesVO"]["path_display"], "INBOX/P2(1)");
}

// ------------------------------------------------------------------ wire forms and the lock

#[tokio::test]
async fn ids_arrive_as_strings_or_numbers_and_paths_with_double_slashes() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"a");
    s.put("INBOX/b.pdf", b"b");
    let a: u64 = s.id_of(&t, "INBOX/a.pdf").await.parse().unwrap();
    let b: u64 = s.id_of(&t, "INBOX/b.pdf").await.parse().unwrap();
    let v = s
        .post(
            MOVE,
            &t,
            json!({"equipmentNo": SERIAL, "id": a, "to_path": "/INBOX//c.pdf", "autorename": false}),
        )
        .await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["path_display"], "INBOX/c.pdf");
    let v = s
        .post(
            COPY,
            &t,
            json!({"equipmentNo": SERIAL, "id": format!("{a}"), "to_path": "INBOX//d.pdf/",
                   "autorename": "false"}),
        )
        .await;
    assert_ok(&v);
    assert_eq!(v["entriesVO"]["path_display"], "INBOX/d.pdf");
    let v = s
        .post(DELETE, &t, json!({"equipmentNo": SERIAL, "id": b}))
        .await;
    assert_ok(&v);
}

#[tokio::test]
async fn a_mutation_without_a_session_takes_the_free_lock() {
    // Auto Sync sends a change without synchronous/start: the write takes the free lock.
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"a");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    assert_ok(
        &s.post(END, &t, json!({"equipmentNo": SERIAL, "flag": "Y"}))
            .await,
    );
    assert_eq!(s.state.sync_locks.holder("jordi", s.state.now_ms()), None);
    let v = s.mkdir(&t, "/INBOX/F", false).await;
    assert_ok(&v);
    assert!(s.files().join("INBOX/F").is_dir());
    assert_eq!(
        s.state
            .sync_locks
            .holder("jordi", s.state.now_ms())
            .as_deref(),
        Some(SERIAL)
    );
    let markers = s.markers("implicit_lock");
    assert_eq!(markers.len(), 1, "{markers:?}");
    assert_eq!(markers[0]["device"], SERIAL);
    // The lock is the device's now: the next writes need no new marker.
    assert_ok(
        &s.post(MOVE, &t, json!({"id": a, "to_path": "/INBOX/b.pdf"}))
            .await,
    );
    assert!(s.files().join("INBOX/b.pdf").is_file());
    assert_eq!(s.markers("implicit_lock").len(), 1);
    // The change is the device's own: its events carry the serial.
    let last = s.events().pop().unwrap();
    assert_eq!(last["device"], SERIAL);
}

#[tokio::test]
async fn a_mutation_without_a_session_waits_while_another_device_syncs() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"a");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    assert_ok(
        &s.post(END, &t, json!({"equipmentNo": SERIAL, "flag": "Y"}))
            .await,
    );
    let other = s.login_as(OTHER).await;
    assert_ok(&s.post(START, &other, json!({"equipmentNo": OTHER})).await);
    let n = s.events().len();
    for (path, body) in [
        (CREATE, json!({"path": "/INBOX/F", "autorename": false})),
        (DELETE, json!({"id": a})),
        (MOVE, json!({"id": a, "to_path": "/INBOX/b.pdf"})),
        (COPY, json!({"id": a, "to_path": "/INBOX/b.pdf"})),
    ] {
        let v = s.post(path, &t, body).await;
        assert_err(&v, "E0079");
        assert!(
            s.log_flags(path)
                .last()
                .unwrap()
                .contains(&"no_sync_lock".to_string())
        );
    }
    assert_eq!(s.events().len(), n);
    assert!(s.files().join("INBOX/a.pdf").is_file());
    assert!(!s.files().join("INBOX/F").exists());
    assert!(s.markers("implicit_lock").is_empty());
}

#[tokio::test]
async fn a_pause_longer_than_the_lock_inside_a_session_takes_the_lock_again() {
    let s = Server::new();
    let t = s.session().await;
    s.put("INBOX/a.pdf", b"a");
    let a = s.id_of(&t, "INBOX/a.pdf").await;
    // The device hashes a large file for 45 s: the lock lapses without a call.
    s.advance_ms(45_000);
    assert_eq!(s.state.sync_locks.holder("jordi", s.state.now_ms()), None);
    let v = s.mkdir(&t, "/INBOX/F", false).await;
    assert_ok(&v);
    assert!(s.files().join("INBOX/F").is_dir());
    assert_eq!(
        s.state
            .sync_locks
            .holder("jordi", s.state.now_ms())
            .as_deref(),
        Some(SERIAL)
    );
    // Again a pause, then a move: the write works without a new synchronous/start.
    s.advance_ms(31_000);
    let v = s
        .post(MOVE, &t, json!({"id": a, "to_path": "/INBOX/b.pdf"}))
        .await;
    assert_ok(&v);
    assert!(s.files().join("INBOX/b.pdf").is_file());
}

#[tokio::test]
async fn a_lapsed_lock_is_not_taken_back_while_another_device_holds_it() {
    let s = Server::new();
    let t = s.session().await;
    s.advance_ms(31_000);
    let other = s.login_as(OTHER).await;
    assert_ok(&s.post(START, &other, json!({"equipmentNo": OTHER})).await);
    let v = s.mkdir(&t, "/INBOX/F", false).await;
    assert_err(&v, "E0079");
    assert!(
        s.log_flags(CREATE)
            .last()
            .unwrap()
            .contains(&"no_sync_lock".to_string())
    );
    assert!(!s.files().join("INBOX/F").exists());
    assert_eq!(
        s.state
            .sync_locks
            .holder("jordi", s.state.now_ms())
            .as_deref(),
        Some(OTHER)
    );
}

#[tokio::test]
async fn mutations_answer_the_device_mismatch() {
    let s = Server::new();
    let t = s.session().await;
    for path in [CREATE, DELETE, MOVE, COPY] {
        let v = s
            .post(path, &t, json!({"equipmentNo": "SN000", "id": "1"}))
            .await;
        assert_err(&v, "E0312");
    }
}

#[tokio::test]
async fn a_new_folder_never_takes_the_id_of_a_vanished_one() {
    let s = Server::new();
    let t = s.session().await;
    let first = s.mkdir(&t, "/INBOX/A", false).await["metadata"]["id"].clone();
    std::fs::remove_dir(s.files().join("INBOX/A")).unwrap();
    let again = s.mkdir(&t, "/INBOX/A", false).await;
    assert_ok(&again);
    assert_ne!(again["metadata"]["id"], first);
    // the same for a parent folder that a move creates
    let b = s.mkdir(&t, "/INBOX/B", false).await["metadata"]["id"].clone();
    std::fs::remove_dir(s.files().join("INBOX/B")).unwrap();
    s.put("INBOX/f.pdf", b"f");
    let f = s.id_of(&t, "INBOX/f.pdf").await;
    let n = s.events().len();
    assert_ok(&s.mv(&t, &f, "/INBOX/B/f.pdf", false).await);
    let ev = s.events_after(n);
    assert_eq!(ev[0]["type"], "folder.created");
    assert_ne!(
        ev[0]["id"].as_u64().unwrap().to_string(),
        b.as_str().unwrap()
    );
    assert_eq!(
        s.id_of(&t, "INBOX/B").await,
        ev[0]["id"].as_u64().unwrap().to_string()
    );
}

// ------------------------------------------------------------------ crash repair (store level)

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    journal: Journal,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let store = Store::open(dir.path(), "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let journal = Journal::open(dir.path()).unwrap();
        Fixture {
            dir,
            store,
            journal,
        }
    }

    fn put(&self, path: &str, bytes: &[u8]) -> u64 {
        let p = self.store.fs_path(&wp(path));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        let g = self.store.lock().unwrap();
        self.store.entry(&g, &wp(path)).unwrap().unwrap().id
    }

    fn types(&self) -> Vec<String> {
        self.journal
            .read_from(1)
            .unwrap()
            .map(|e| e.unwrap().body.type_name().to_string())
            .collect()
    }

    fn restart_and_recover(&self) -> Recovery {
        let store = Store::open(self.dir.path(), "jordi").unwrap();
        let journal = Journal::open(self.dir.path()).unwrap();
        let g = store.lock().unwrap();
        store.recover(&g, &journal).unwrap()
    }
}

#[test]
fn a_crash_in_the_middle_of_a_case_only_rename_finishes_it() {
    let f = Fixture::new();
    let id = f.put("NOTE/Note/bike.note", b"bike");
    // Where the file system ignores case the rename has two steps: stop between them. Where it
    // does not, it is one plain rename: stop after it, before the journal append.
    let crash = if case_insensitive(&f.store.fs_path(&wp("NOTE/Note"))) {
        CrashPoint::BeforeRename
    } else {
        CrashPoint::BeforeAppend
    };
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/bike.note"),
            &wp("NOTE/Note/Bike.note"),
            Origin::Device,
            None,
        )
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(crash))
        .unwrap();
    drop(g);
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    let names: Vec<String> = std::fs::read_dir(f.store.fs_path(&wp("NOTE/Note")))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["Bike.note"]);
    assert_eq!(
        std::fs::read(f.store.fs_path(&wp("NOTE/Note/Bike.note"))).unwrap(),
        b"bike"
    );
    assert_eq!(f.store.id_of(&wp("NOTE/Note/Bike.note")).unwrap(), Some(id));
    assert_eq!(f.types(), ["file.moved"]);
    // a recovery later finds nothing to do
    assert_eq!(f.restart_and_recover(), Recovery::Clean);
}

#[test]
fn a_case_only_rename_plans_a_temp_name_only_where_the_file_system_folds_case() {
    let f = Fixture::new();
    f.put("NOTE/Note/bike.note", b"bike");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/bike.note"),
            &wp("NOTE/Note/Bike.note"),
            Origin::Device,
            None,
        )
        .unwrap();
    let via = match &c.op {
        notedown_cloud::store::PendingOp::Move { via, .. } => via.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        via.is_some(),
        case_insensitive(&f.store.fs_path(&wp("NOTE/Note")))
    );
    if let Some(via) = via {
        assert!(via.starts_with(".nd-move-"), "{via}");
        assert!(
            !via.ends_with(".tmp"),
            "the temp sweep must never take it: {via}"
        );
    }
}

#[test]
fn a_crash_after_a_delete_with_a_prune_journals_both_once() {
    let f = Fixture::new();
    f.put("NOTE/Note/P/a.note", b"a");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("NOTE/Note/P/a.note"), 5, Origin::Device, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    assert_eq!(f.types(), ["file.deleted", "folder.deleted"]);
    assert!(!f.store.fs_path(&wp("NOTE/Note/P")).exists());
    assert!(f.store.fs_path(&wp("NOTE/Note")).is_dir());
    assert_eq!(f.store.id_of(&wp("NOTE/Note/P")).unwrap(), None);
    let g = f.store.lock().unwrap();
    let index = f.store.recycle_index(&g).unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].location(), "5/NOTE/Note/P/a.note");
    drop(g);
    assert_eq!(f.restart_and_recover(), Recovery::Clean);
    assert!(!f.store.account_dir().join(PENDING_FILE).exists());
}

#[test]
fn a_prune_folder_that_is_no_longer_empty_stays_and_gets_no_event() {
    let f = Fixture::new();
    f.put("NOTE/Note/P/Q/a.note", b"a");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("NOTE/Note/P/Q/a.note"), 5, Origin::Device, None)
        .unwrap();
    // something lands in P out of band after the plan: P must stay, Q still goes
    std::fs::write(f.store.fs_path(&wp("NOTE/Note/P/late.pdf")), b"late").unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    drop(g);
    assert_eq!(f.types(), ["file.deleted", "folder.deleted"]);
    let ev: Vec<_> = f
        .journal
        .read_from(1)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert!(ev[1].to_line().contains("\"path\":\"NOTE/Note/P/Q\""));
    assert!(f.store.fs_path(&wp("NOTE/Note/P/late.pdf")).is_file());
    assert!(f.store.id_of(&wp("NOTE/Note/P")).unwrap().is_some());
}

#[test]
fn a_failed_second_rename_keeps_the_intent_and_recovery_finishes_the_move() {
    let f = Fixture::new();
    let id = f.put("NOTE/Note/bike.note", b"bike");
    if !case_insensitive(&f.store.fs_path(&wp("NOTE/Note"))) {
        // A plain rename here: there is no second rename to fail.
        return;
    }
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/bike.note"),
            &wp("NOTE/Note/Bike.note"),
            Origin::Device,
            None,
        )
        .unwrap();
    let err = f
        .store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::FailSecondRename))
        .unwrap_err();
    assert!(err.to_string().contains("second rename"), "{err}");
    drop(g);
    // the entry is under the temp name, and the intent is kept for it
    assert!(f.store.account_dir().join(PENDING_FILE).exists());
    let names = names_in(&f, "NOTE/Note");
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with(".nd-move-"), "{names:?}");
    assert!(f.types().is_empty());
    // the next lock recovers it: the move is finished and journaled once
    let g = f.store.lock_recovered(&f.journal).unwrap();
    drop(g);
    assert_eq!(names_in(&f, "NOTE/Note"), ["Bike.note"]);
    assert_eq!(f.store.id_of(&wp("NOTE/Note/Bike.note")).unwrap(), Some(id));
    assert_eq!(f.types(), ["file.moved"]);
    assert!(!f.store.account_dir().join(PENDING_FILE).exists());
}

#[test]
fn a_rename_whose_target_was_taken_or_whose_entry_vanished_is_never_journaled() {
    let f = Fixture::new();
    f.put("NOTE/Note/bike.note", b"bike");
    if !case_insensitive(&f.store.fs_path(&wp("NOTE/Note"))) {
        return;
    }
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/bike.note"),
            &wp("NOTE/Note/Bike.note"),
            Origin::Device,
            None,
        )
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::FailSecondRename))
        .unwrap_err();
    drop(g);
    let via = names_in(&f, "NOTE/Note").remove(0);
    // something else takes the new name while the entry is under the temp name
    std::fs::write(f.store.fs_path(&wp("NOTE/Note/Bike.note")), b"other").unwrap();
    let store = Store::open(f.dir.path(), "jordi").unwrap();
    assert!(store.lock_recovered(&f.journal).is_err());
    assert!(f.store.account_dir().join(PENDING_FILE).exists());
    let dir = f.store.fs_path(&wp("NOTE/Note"));
    assert_eq!(std::fs::read(dir.join(&via)).unwrap(), b"bike");
    assert_eq!(std::fs::read(dir.join("Bike.note")).unwrap(), b"other");
    assert!(f.types().is_empty());
    // the entry is gone from every name (removed out of band): recovery drops the change,
    // journals nothing, and the account is writable again
    std::fs::remove_file(dir.join(&via)).unwrap();
    std::fs::remove_file(dir.join("Bike.note")).unwrap();
    let g = store.lock_recovered(&f.journal).unwrap();
    drop(g);
    assert!(!f.store.account_dir().join(PENDING_FILE).exists());
    assert!(f.types().is_empty());
    assert!(names_in(&f, "NOTE/Note").is_empty());
}

#[test]
fn a_target_taken_between_the_renames_is_never_journaled_as_the_move() {
    let f = Fixture::new();
    let id = f.put("NOTE/Note/bike.note", b"bike");
    if !case_insensitive(&f.store.fs_path(&wp("NOTE/Note"))) {
        return;
    }
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/bike.note"),
            &wp("NOTE/Note/Bike.note"),
            Origin::Device,
            None,
        )
        .unwrap();
    // the live commit: a file lands at the target while the entry is under the temp name
    assert!(
        f.store
            .commit_until(
                &g,
                c,
                &f.journal,
                Some(CrashPoint::TakeTargetBetweenRenames)
            )
            .is_err()
    );
    drop(g);
    assert!(
        f.types().is_empty(),
        "no file.moved for bytes that are not at the target"
    );
    assert!(f.store.account_dir().join(PENDING_FILE).exists());
    let names = names_in(&f, "NOTE/Note");
    let dir = f.store.fs_path(&wp("NOTE/Note"));
    assert_eq!(
        std::fs::read(dir.join("Bike.note")).unwrap(),
        b"out of band"
    );
    let via = names.iter().find(|n| n.starts_with(".nd-move-")).unwrap();
    assert_eq!(std::fs::read(dir.join(via)).unwrap(), b"bike");
    // the id did not move onto the other file
    assert_eq!(
        f.store.path_of(id).unwrap(),
        Some(wp("NOTE/Note/bike.note"))
    );
    assert_ne!(f.store.id_of(&wp("NOTE/Note/Bike.note")).unwrap(), Some(id));
}

#[test]
fn the_index_lines_of_a_folder_delete_are_written_once_after_a_crash() {
    let f = Fixture::new();
    f.put("INBOX/P/a.pdf", b"a");
    f.put("INBOX/P/S/b.pdf", b"b");
    f.put("INBOX/Q/c.pdf", b"c");
    let index_file = f.store.account_dir().join("recycle/index.jsonl");
    let index = |f: &Fixture| {
        let g = f.store.lock().unwrap();
        f.store.recycle_index(&g).unwrap()
    };
    // a crash after the whole batch reached the disk: nothing is written twice
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("INBOX/P"), 7, Origin::Device, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeClear))
        .unwrap();
    drop(g);
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    let paths: Vec<String> = index(&f).into_iter().map(|e| e.path).collect();
    assert_eq!(paths, ["INBOX/P/a.pdf", "INBOX/P/S/b.pdf", "INBOX/P"]);
    // a crash after only the first line of the batch reached the disk: the rest is added
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("INBOX/Q"), 9, Origin::Device, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeClear))
        .unwrap();
    drop(g);
    let text = std::fs::read_to_string(&index_file).unwrap();
    let keep: usize = text.lines().take(4).map(|l| l.len() + 1).sum();
    std::fs::write(&index_file, &text[..keep]).unwrap();
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    let all = index(&f);
    let paths: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "INBOX/P/a.pdf",
            "INBOX/P/S/b.pdf",
            "INBOX/P",
            "INBOX/Q/c.pdf",
            "INBOX/Q"
        ]
    );
    for e in &all {
        assert!(f.store.recycle_dir().join(e.location()).exists(), "{e:?}");
    }
}

fn names_in(f: &Fixture, folder: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(f.store.fs_path(&wp(folder)))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
