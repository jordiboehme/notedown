//! To-do routes of accept mode (`docs/sync-protocol.md` 6.1): groups, tasks and sorts, the
//! full and the incremental pull (D8), tombstones (D5), the gate (D1, never the sync lock) and
//! unknown fields (D10). Requests go through the router in memory (`tower::oneshot`).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use notedown_cloud::auth::{Accounts, AppDataImported, Imported, expected_login_hash, md5_hex};
use notedown_cloud::journal::{Event, Origin};
use notedown_cloud::server::{AppState, router};
use notedown_cloud::{Clock, Config, SyncMode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const SERIAL: &str = "SN078C10000000";
const OTHER: &str = "SN078C10000001";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

const GROUP_ALL: &str = "/api/file/schedule/group/all";
const GROUP: &str = "/api/file/schedule/group";
const GROUP_CLEAR: &str = "/api/file/schedule/group/clear";
const TASK_ALL: &str = "/api/file/schedule/task/all";
const TASK: &str = "/api/file/schedule/task";
const TASK_LIST: &str = "/api/file/schedule/task/list";
const SORT: &str = "/api/file/schedule/sort";
const SORT_QUERY: &str = "/api/file/query/schedule/sort";
const START: &str = "/api/file/2/files/synchronous/start";

const L1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const L2: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";
const T1: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1";
const T2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";
const T3: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb3";

/// Above 2^53: an `f64` would round it.
const BIG: u64 = 895_334_188_016_730_113;

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

    fn tasks_dir(&self) -> std::path::PathBuf {
        self.data().join("accounts/jordi/tasks")
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

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

    fn user_id(&self) -> u64 {
        Accounts::new(self.data())
            .get("jordi")
            .unwrap()
            .unwrap()
            .user_id
    }

    fn events(&self) -> Vec<Event> {
        self.state
            .journal
            .read_from(0)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    /// The device's `task.*` and `task_group.*` events.
    fn task_events(&self) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|e| e.origin == Origin::Device && e.body.type_name().starts_with("task"))
            .collect()
    }

    fn holder(&self) -> Option<String> {
        self.state.sync_locks.holder("jordi", self.state.now_ms())
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
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Raw { status, body }
    }

    async fn call(&self, method: &str, path: &str, token: &str, body: Option<Value>) -> Value {
        let r = self.raw(method, path, Some(token), body).await;
        assert_eq!(r.status, StatusCode::OK, "{method} {path}: {}", r.text());
        r.json()
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> Value {
        self.call("POST", path, token, Some(body)).await
    }

    async fn put(&self, path: &str, token: &str, body: Value) -> Value {
        self.call("PUT", path, token, Some(body)).await
    }

    async fn get(&self, path: &str, token: &str) -> Value {
        self.call("GET", path, token, None).await
    }

    async fn delete(&self, path: &str, token: &str) -> Value {
        self.call("DELETE", path, token, None).await
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

    /// `task/all` with these arguments.
    async fn pull(&self, token: &str, args: Value) -> Value {
        let v = self.post(TASK_ALL, token, args).await;
        assert_ok(&v);
        v
    }

    /// A group and three tasks in it, as the device makes them.
    async fn seed(&self, token: &str) {
        assert_ok(&self.post(GROUP, token, group_body(L1, "Home")).await);
        for (id, title) in [(T1, "milk"), (T2, "bread"), (T3, "eggs")] {
            assert_ok(&self.post(TASK, token, task_body(id, L1, title)).await);
        }
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

/// A `POST schedule/group` body (6.1).
fn group_body(id: &str, title: &str) -> Value {
    json!({
        "taskListId": id,
        "title": title,
        "lastModified": T0 - 10_000,
        "createTime": T0 - 20_000,
        "equipmentNo": SERIAL,
    })
}

/// A task as the device sends it (6.1 task fields).
fn task_body(id: &str, list: &str, title: &str) -> Value {
    json!({
        "taskId": id,
        "taskListId": list,
        "recurrenceId": null,
        "title": title,
        "detail": "",
        "lastModified": T0 - 1_000,
        "recurrence": "RRULE:FREQ=WEEKLY;BYDAY=MO",
        "isReminderOn": "N",
        "status": "needsAction",
        "importance": null,
        "dueTime": 0,
        "completedTime": null,
        "links": "eyJhcHBOYW1lIjoibm90ZSJ9",
        "isDeleted": "N",
        "sort": 0,
        "sortCompleted": 0,
        "planerSort": 0,
        "allSort": 0,
        "allSortCompleted": 0,
        "sortTime": T0 - 1_000,
        "planerSortTime": T0 - 1_000,
        "allSortTime": T0 - 1_000,
        "equipmentNo": SERIAL,
    })
}

fn tasks_of(v: &Value) -> Vec<Value> {
    v["scheduleTask"].as_array().unwrap().clone()
}

fn ids_of(v: &Value) -> Vec<String> {
    tasks_of(v)
        .iter()
        .map(|t| t["taskId"].as_str().unwrap().to_string())
        .collect()
}

fn has_key(v: &Value, key: &str) -> bool {
    v.as_object().unwrap().contains_key(key)
}

// ------------------------------------------------------------------ groups and tasks

#[tokio::test]
async fn a_group_and_three_tasks_come_back_in_a_full_pull() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let v = s.post(GROUP, &t, group_body(L1, "Home")).await;
    assert_ok(&v);
    assert_eq!(v["taskListId"], L1);
    for (id, title) in [(T3, "eggs"), (T1, "milk"), (T2, "bread")] {
        let v = s.post(TASK, &t, task_body(id, L1, title)).await;
        assert_ok(&v);
        assert_eq!(v["taskId"], id);
    }

    let groups = s.post(GROUP_ALL, &t, json!({"maxResults": "200"})).await;
    assert_ok(&groups);
    assert!(!has_key(&groups, "pageToken"), "{groups}");
    let list = groups["scheduleTaskGroup"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0],
        json!({"taskListId": L1, "userId": s.user_id(), "title": "Home",
               "lastModified": T0 - 10_000, "isDeleted": "N", "createTime": T0 - 20_000})
    );
    let g = s.get(&format!("{GROUP}/{L1}"), &t).await;
    assert_ok(&g);
    assert_eq!(g["title"], "Home");
    assert_eq!(g["userId"], s.user_id());

    let v = s.pull(&t, json!({"maxResults": "200"})).await;
    assert_eq!(ids_of(&v), vec![T1, T2, T3]);
    assert_eq!(v["nextSyncToken"], T0);
    assert!(!has_key(&v, "nextPageToken"), "{v}");
    let first = &tasks_of(&v)[0];
    let mut expected = task_body(T1, L1, "milk");
    expected.as_object_mut().unwrap().remove("equipmentNo");
    assert_eq!(first, &expected);
    // the server times stay on disk
    assert!(!v.to_string().contains("_at_ms"));

    let events: Vec<&str> = s.task_events().iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        events,
        vec![
            "task_group.created",
            "task.created",
            "task.created",
            "task.created"
        ]
    );
    assert!(
        s.task_events()
            .iter()
            .all(|e| e.device.as_deref() == Some(SERIAL))
    );
}

#[tokio::test]
async fn a_task_is_read_with_an_empty_recurrence_list() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    let v = s.get(&format!("{TASK}/{T2}"), &t).await;
    assert_ok(&v);
    assert_eq!(v["taskId"], T2);
    assert_eq!(v["title"], "bread");
    assert_eq!(v["links"], "eyJhcHBOYW1lIjoibm90ZSJ9");
    assert_eq!(v["scheduleRecurTask"], json!([]));
    assert_err(
        &s.get(&format!("{TASK}/cccccccccccccccccccccccccccccccc"), &t)
            .await,
        "E0329",
    );
    assert_err(
        &s.get(&format!("{GROUP}/cccccccccccccccccccccccccccccccc"), &t)
            .await,
        "E0328",
    );
    // a tombstone is no task
    assert_ok(&s.delete(&format!("{TASK}/{T2}"), &t).await);
    assert_err(&s.get(&format!("{TASK}/{T2}"), &t).await, "E0329");
}

#[tokio::test]
async fn post_and_put_upsert_a_task_by_its_id() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    let mut body = task_body(T1, L1, "oat milk");
    body["status"] = json!("completed");
    let v = s.put(TASK, &t, body).await;
    assert_ok(&v);
    assert_eq!(v["taskId"], T1);
    let v = s.post(TASK, &t, task_body(T1, L1, "soy milk")).await;
    assert_eq!(v["taskId"], T1);
    // an unknown id on PUT is created
    let new = "dddddddddddddddddddddddddddddddd";
    assert_ok(&s.put(TASK, &t, task_body(new, L1, "new")).await);
    let v = s.pull(&t, json!({})).await;
    assert_eq!(ids_of(&v), vec![T1, T2, T3, new]);
    assert_eq!(tasks_of(&v)[0]["title"], "soy milk");
    let kinds: Vec<&str> = s
        .task_events()
        .iter()
        .skip(4)
        .map(|e| e.body.type_name())
        .collect();
    assert_eq!(kinds, vec!["task.updated", "task.updated", "task.created"]);
}

#[tokio::test]
async fn a_group_update_keeps_its_create_time_and_an_unknown_group_is_created() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    let v = s
        .put(
            GROUP,
            &t,
            json!({"taskListId": L1, "title": "House", "lastModified": T0 + 5,
                   "equipmentNo": SERIAL}),
        )
        .await;
    assert_ok(&v);
    let g = s.get(&format!("{GROUP}/{L1}"), &t).await;
    assert_eq!(g["title"], "House");
    assert_eq!(g["lastModified"], T0 + 5);
    assert_eq!(g["createTime"], T0 - 20_000);
    assert_eq!(g["isDeleted"], "N");
    // PUT of an unknown group creates it, with the server time as createTime
    assert_ok(
        &s.put(
            GROUP,
            &t,
            json!({"taskListId": L2, "title": "Work", "lastModified": T0}),
        )
        .await,
    );
    let g = s.get(&format!("{GROUP}/{L2}"), &t).await;
    assert_eq!(g["createTime"], T0);
    let kinds: Vec<&str> = s
        .task_events()
        .iter()
        .filter(|e| e.body.type_name().starts_with("task_group"))
        .map(|e| e.body.type_name())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "task_group.created",
            "task_group.updated",
            "task_group.created"
        ]
    );
}

// ------------------------------------------------------------------ D8 incremental pull

#[tokio::test]
async fn an_incremental_pull_returns_updates_and_deletes() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    s.advance_ms(1_000);
    let full = s.pull(&t, json!({"maxResults": "200"})).await;
    let token = full["nextSyncToken"].as_u64().unwrap();
    assert_eq!(token, (T0 + 1_000) as u64);

    // nothing changed: nothing comes back
    s.advance_ms(1_000);
    let v = s
        .pull(&t, json!({"maxResults": "200", "nextSyncToken": token}))
        .await;
    assert!(tasks_of(&v).is_empty(), "{v}");

    s.advance_ms(1_000);
    assert_ok(&s.put(TASK, &t, task_body(T1, L1, "oat milk")).await);
    assert_ok(&s.delete(&format!("{TASK}/{T2}"), &t).await);
    s.advance_ms(1_000);
    let v = s
        .pull(
            &t,
            json!({"maxResults": "200", "nextSyncToken": token.to_string()}),
        )
        .await;
    assert_eq!(ids_of(&v), vec![T1, T2]);
    let tasks = tasks_of(&v);
    assert_eq!(tasks[0]["title"], "oat milk");
    assert_eq!(tasks[0]["isDeleted"], "N");
    assert_eq!(tasks[1]["isDeleted"], "Y");
    assert_eq!(tasks[1]["title"], "bread");
    assert_eq!(v["nextSyncToken"], T0 + 4_000);

    // a full pull has no tombstones
    let v = s.pull(&t, json!({})).await;
    assert_eq!(ids_of(&v), vec![T1, T3]);
    let deleted = s
        .task_events()
        .into_iter()
        .find(|e| e.body.type_name() == "task.deleted")
        .unwrap();
    let body = serde_json::to_value(&deleted).unwrap();
    assert_eq!(body["title"], "bread");
    assert_eq!(body["taskId"], T2);
}

#[tokio::test]
async fn a_change_in_the_millisecond_of_the_token_is_not_lost() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    s.advance_ms(1_000);
    let token = s.pull(&t, json!({})).await["nextSyncToken"]
        .as_u64()
        .unwrap();
    // same millisecond as the answered token
    assert_ok(&s.put(TASK, &t, task_body(T3, L1, "brown eggs")).await);
    let v = s.pull(&t, json!({"nextSyncToken": token})).await;
    assert_eq!(ids_of(&v), vec![T3]);
}

#[tokio::test]
async fn a_token_older_than_five_days_answers_e0330() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    s.advance_ms(1_000);
    let token = s.pull(&t, json!({})).await["nextSyncToken"]
        .as_u64()
        .unwrap();
    // exactly 5 days: still answered
    s.advance_ms(5 * DAY);
    let v = s.pull(&t, json!({"nextSyncToken": token})).await;
    assert!(tasks_of(&v).is_empty(), "{v}");
    s.advance_ms(1);
    let v = s.post(TASK_ALL, &t, json!({"nextSyncToken": token})).await;
    assert_err(&v, "E0330");
    assert!(!has_key(&v, "scheduleTask"));
    // a token the server never made (in the future, or no number) is too old as well
    let v = s
        .post(TASK_ALL, &t, json!({"nextSyncToken": (T0 + 6 * DAY)}))
        .await;
    assert_err(&v, "E0330");
    let v = s
        .post(TASK_ALL, &t, json!({"nextSyncToken": "yesterday"}))
        .await;
    assert_err(&v, "E0330");
    // no token, null, "" and 0: a full pull
    for token in [json!(null), json!(""), json!(0), json!("0")] {
        let v = s.pull(&t, json!({"nextSyncToken": token})).await;
        assert_eq!(ids_of(&v).len(), 3, "{token}: {v}");
    }
}

#[tokio::test]
async fn tombstones_are_swept_after_five_days_before_the_pull() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    assert_ok(&s.delete(&format!("{TASK}/{T1}"), &t).await);
    let file = s.tasks_dir().join(format!("items/{T1}.json"));
    assert!(file.is_file());
    s.advance_ms(5 * DAY);
    let token = s.pull(&t, json!({})).await["nextSyncToken"]
        .as_u64()
        .unwrap();
    assert!(file.is_file(), "not older than 5 days yet");
    s.advance_ms(1);
    let v = s.pull(&t, json!({"nextSyncToken": token})).await;
    assert!(tasks_of(&v).is_empty());
    assert!(!file.exists());
}

// ------------------------------------------------------------------ paging

#[tokio::test]
async fn task_pages_omit_the_token_on_the_last_page() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    for id in [
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb4",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb5",
    ] {
        assert_ok(&s.post(TASK, &t, task_body(id, L1, "x")).await);
    }
    let mut seen = Vec::new();
    let mut page_token: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut args = json!({"maxResults": "2"});
        if let Some(p) = &page_token {
            args["nextPageTokens"] = json!(p);
        }
        let raw = s.raw("POST", TASK_ALL, Some(&t), Some(args)).await;
        let v = raw.json();
        assert_ok(&v);
        pages += 1;
        seen.extend(ids_of(&v));
        if has_key(&v, "nextPageToken") {
            let next = v["nextPageToken"].as_str().unwrap().to_string();
            assert!(!next.is_empty());
            page_token = Some(next);
        } else {
            assert!(!raw.text().contains("nextPageToken"));
            break;
        }
        assert!(pages < 10, "page loop");
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 5);
    seen.dedup();
    assert_eq!(seen.len(), 5);
    // a number works as well, and exactly full pages end without a token
    let v = s.pull(&t, json!({"maxResults": 5})).await;
    assert_eq!(ids_of(&v).len(), 5);
    assert!(!has_key(&v, "nextPageToken"));
    // the default is 20
    let v = s.pull(&t, json!({"maxResults": null})).await;
    assert_eq!(ids_of(&v).len(), 5);
}

#[tokio::test]
async fn group_pages_carry_a_page_token_only_when_more_follow() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    for (i, id) in [L1, L2, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3"]
        .into_iter()
        .enumerate()
    {
        assert_ok(&s.post(GROUP, &t, group_body(id, &format!("g{i}"))).await);
    }
    let v = s.post(GROUP_ALL, &t, json!({"maxResults": "2"})).await;
    assert_ok(&v);
    assert_eq!(v["scheduleTaskGroup"].as_array().unwrap().len(), 2);
    let next = v["pageToken"].as_str().unwrap().to_string();
    let v = s
        .post(GROUP_ALL, &t, json!({"maxResults": "2", "pageToken": next}))
        .await;
    assert_ok(&v);
    let list = v["scheduleTaskGroup"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["taskListId"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3");
    assert!(!has_key(&v, "pageToken"), "{v}");
}

// ------------------------------------------------------------------ batch, group delete, clear

#[tokio::test]
async fn task_list_stores_every_task_in_one_journal_batch() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    let mut no_list = task_body("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeee1", L1, "no list");
    no_list.as_object_mut().unwrap().remove("taskListId");
    let body = json!({
        "taskListId": L1,
        "updateScheduleTaskList": [
            task_body(T1, L1, "milk 2"),
            task_body(T2, L1, "bread 2"),
            no_list,
        ],
        "equipmentNo": SERIAL,
    });
    let before = s.task_events().len();
    let v = s.put(TASK_LIST, &t, body).await;
    assert_ok(&v);
    let events = s.task_events();
    let batch = &events[before..];
    assert_eq!(batch.len(), 3);
    assert!(batch.iter().all(|e| e.op_id == batch[0].op_id));
    let v = s.pull(&t, json!({})).await;
    let tasks = tasks_of(&v);
    assert_eq!(tasks[0]["title"], "milk 2");
    assert_eq!(tasks[1]["title"], "bread 2");
    assert_eq!(tasks[3]["taskListId"], L1);

    // one bad key: nothing is stored
    let body = json!({
        "taskListId": L1,
        "updateScheduleTaskList": [task_body(T1, L1, "milk 3"), task_body("../x", L1, "bad")],
    });
    assert_err(&s.put(TASK_LIST, &t, body).await, "422");
    let v = s.pull(&t, json!({})).await;
    assert_eq!(tasks_of(&v)[0]["title"], "milk 2");
}

#[tokio::test]
async fn a_group_delete_removes_its_tasks_and_its_sort() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    assert_ok(&s.post(GROUP, &t, group_body(L2, "Work")).await);
    let other = "ffffffffffffffffffffffffffffffff";
    assert_ok(&s.post(TASK, &t, task_body(other, L2, "report")).await);
    assert_ok(
        &s.post(
            SORT,
            &t,
            json!({"taskListId": L1, "title": "Home", "lastModify": T0, "content": "[]"}),
        )
        .await,
    );
    s.advance_ms(1_000);
    let token = s.pull(&t, json!({})).await["nextSyncToken"]
        .as_u64()
        .unwrap();
    let before = s.task_events().len();
    s.advance_ms(1_000);
    assert_ok(&s.delete(&format!("{GROUP}/{L1}"), &t).await);

    let events = s.task_events();
    let after = &events[before..];
    let kinds: Vec<&str> = after.iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        kinds,
        vec![
            "task.deleted",
            "task.deleted",
            "task.deleted",
            "task_group.deleted"
        ]
    );
    assert!(after[..3].iter().all(|e| e.op_id == after[0].op_id));

    let v = s.pull(&t, json!({})).await;
    assert_eq!(ids_of(&v), vec![other]);
    let v = s.pull(&t, json!({"nextSyncToken": token})).await;
    assert_eq!(ids_of(&v), vec![T1, T2, T3]);
    assert!(tasks_of(&v).iter().all(|t| t["isDeleted"] == "Y"));
    assert_err(&s.get(&format!("{GROUP}/{L1}"), &t).await, "E0328");
    assert!(!s.tasks_dir().join(format!("sort/{L1}.json")).exists());
    let groups = s.post(GROUP_ALL, &t, json!({})).await;
    assert_eq!(groups["scheduleTaskGroup"].as_array().unwrap().len(), 1);
    // a second delete finds nothing and still succeeds
    assert_ok(&s.delete(&format!("{GROUP}/{L1}"), &t).await);
    assert_eq!(s.task_events().len(), events.len());
}

#[tokio::test]
async fn group_clear_deletes_the_completed_tasks() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    let other = "ffffffffffffffffffffffffffffffff";
    for (id, list) in [(T1, L1), (T3, L1), (other, L2)] {
        let mut body = task_body(id, list, "done");
        body["status"] = json!("completed");
        assert_ok(&s.put(TASK, &t, body).await);
    }
    let before = s.task_events().len();
    let v = s
        .post(
            GROUP_CLEAR,
            &t,
            json!({"taskListId": L1, "lastModified": T0 + 77, "equipmentNo": SERIAL}),
        )
        .await;
    assert_ok(&v);
    let events = s.task_events();
    let after = &events[before..];
    let kinds: Vec<&str> = after.iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        kinds,
        vec!["task.deleted", "task.deleted", "task_group.updated"]
    );
    assert_eq!(after[0].op_id, after[1].op_id);
    let v = s.pull(&t, json!({})).await;
    assert_eq!(ids_of(&v), vec![T2, other]);
    assert_eq!(
        s.get(&format!("{GROUP}/{L1}"), &t).await["lastModified"],
        T0 + 77
    );
}

// ------------------------------------------------------------------ sort

#[tokio::test]
async fn sorts_round_trip_one_per_call() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    // no sorts: an empty answer that ends
    let v = s
        .post(SORT_QUERY, &t, json!({"nextIndexNumber": null}))
        .await;
    assert_ok(&v);
    assert!(v["nextIndexNumber"].is_null());
    assert!(v["taskListId"].is_null());

    let content = r#"[{"taskId":"b1","sort":1}]"#;
    assert_ok(
        &s.post(
            SORT,
            &t,
            json!({"taskListId": L2, "title": "Work", "lastModify": T0, "content": content,
                   "equipmentNo": SERIAL}),
        )
        .await,
    );
    assert_ok(
        &s.put(
            SORT,
            &t,
            json!({"taskListId": L1, "title": "Home", "lastModify": T0, "content": "[]"}),
        )
        .await,
    );
    // upsert
    assert_ok(
        &s.put(
            SORT,
            &t,
            json!({"taskListId": L1, "title": "Home", "lastModify": T0 + 1, "content": "[1]"}),
        )
        .await,
    );
    let v = s.post(SORT_QUERY, &t, json!({})).await;
    assert_ok(&v);
    assert_eq!(v["taskListId"], L1);
    assert_eq!(v["lastModify"], T0 + 1);
    assert_eq!(v["content"], "[1]");
    assert_eq!(v["nextIndexNumber"], 1);
    let v = s
        .post(SORT_QUERY, &t, json!({"nextIndexNumber": "1"}))
        .await;
    assert_eq!(v["taskListId"], L2);
    assert_eq!(v["title"], "Work");
    assert_eq!(v["content"], content);
    assert!(v["nextIndexNumber"].is_null());
    assert!(!v.to_string().contains("equipmentNo"));

    assert_ok(&s.delete(&format!("{SORT}/{L1}"), &t).await);
    let v = s
        .post(SORT_QUERY, &t, json!({"nextIndexNumber": null}))
        .await;
    assert_eq!(v["taskListId"], L2);
    assert!(v["nextIndexNumber"].is_null());
    // deleting what is gone succeeds
    assert_ok(&s.delete(&format!("{SORT}/{L1}"), &t).await);
    // sorts are not journaled
    assert!(s.task_events().is_empty());
}

// ------------------------------------------------------------------ D10 and bad parameters

#[tokio::test]
async fn unknown_fields_round_trip() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let mut g = group_body(L1, "Home");
    g["colour"] = json!("blue");
    assert_ok(&s.post(GROUP, &t, g).await);
    let mut task = task_body(T1, L1, "milk");
    task["newFlag"] = json!({"a": [1, 2]});
    task["allSortTime"] = json!(BIG);
    assert_ok(&s.post(TASK, &t, task).await);
    assert_ok(
        &s.post(
            SORT,
            &t,
            json!({"taskListId": L1, "title": "t", "lastModify": 1, "content": "[]",
                   "extraSort": 7}),
        )
        .await,
    );

    let groups = s.post(GROUP_ALL, &t, json!({})).await;
    assert_eq!(groups["scheduleTaskGroup"][0]["colour"], "blue");
    let raw = s.raw("POST", TASK_ALL, Some(&t), Some(json!({}))).await;
    assert!(raw.text().contains(&format!("\"allSortTime\":{BIG}")));
    let v = raw.json();
    assert_eq!(tasks_of(&v)[0]["newFlag"], json!({"a": [1, 2]}));
    assert!(!raw.text().contains("equipmentNo"));
    let v = s.get(&format!("{TASK}/{T1}"), &t).await;
    assert_eq!(v["newFlag"], json!({"a": [1, 2]}));
    let v = s.post(SORT_QUERY, &t, json!({})).await;
    assert_eq!(v["extraSort"], 7);
}

#[tokio::test]
async fn bad_parameters_answer_422_and_store_nothing() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let mut no_id = task_body(T1, L1, "x");
    no_id.as_object_mut().unwrap().remove("taskId");
    let calls: Vec<(&str, String, Option<Value>)> = vec![
        ("POST", TASK.into(), Some(no_id)),
        ("PUT", TASK.into(), Some(task_body("a.b", L1, "x"))),
        ("POST", TASK.into(), Some(json!({"taskId": T1, "title": 5}))),
        ("POST", GROUP.into(), Some(json!({"title": "no id"}))),
        ("PUT", GROUP.into(), Some(json!({"taskListId": "../x"}))),
        ("POST", GROUP_CLEAR.into(), Some(json!({"lastModified": 1}))),
        (
            "PUT",
            TASK_LIST.into(),
            Some(json!({"taskListId": L1, "updateScheduleTaskList": "no"})),
        ),
        ("POST", SORT.into(), Some(json!({"title": "no id"}))),
        ("POST", TASK_ALL.into(), Some(json!({"maxResults": "many"}))),
        (
            "POST",
            TASK_ALL.into(),
            Some(json!({"nextPageTokens": "abc"})),
        ),
        ("POST", GROUP_ALL.into(), Some(json!({"pageToken": "-1"}))),
        (
            "POST",
            SORT_QUERY.into(),
            Some(json!({"nextIndexNumber": "x"})),
        ),
        ("GET", format!("{TASK}/a.b"), None),
        ("DELETE", format!("{TASK}/a.b"), None),
        ("GET", format!("{GROUP}/a.b"), None),
        ("DELETE", format!("{GROUP}/a.b"), None),
        ("DELETE", format!("{SORT}/a.b"), None),
    ];
    for (method, path, body) in &calls {
        let v = s.call(method, path, &t, body.clone()).await;
        assert_err(&v, "422");
    }
    assert!(!s.tasks_dir().exists());
    assert!(s.task_events().is_empty());
}

// ------------------------------------------------------------------ D1: the gate, never the lock

/// Every to-do route with a body it accepts.
fn every_route() -> Vec<(&'static str, String, Option<Value>)> {
    vec![
        ("POST", GROUP_ALL.into(), Some(json!({"maxResults": "200"}))),
        ("POST", GROUP.into(), Some(group_body(L1, "Home"))),
        ("PUT", GROUP.into(), Some(group_body(L1, "Home"))),
        ("GET", format!("{GROUP}/{L1}"), None),
        (
            "POST",
            GROUP_CLEAR.into(),
            Some(json!({"taskListId": L1, "lastModified": 1})),
        ),
        ("DELETE", format!("{GROUP}/{L1}"), None),
        ("POST", TASK_ALL.into(), Some(json!({"maxResults": "200"}))),
        ("POST", TASK.into(), Some(task_body(T1, L1, "x"))),
        ("PUT", TASK.into(), Some(task_body(T1, L1, "x"))),
        (
            "PUT",
            TASK_LIST.into(),
            Some(json!({"taskListId": L1, "updateScheduleTaskList": [task_body(T2, L1, "y")]})),
        ),
        ("DELETE", format!("{TASK}/{T1}"), None),
        ("GET", format!("{TASK}/{T2}"), None),
        (
            "POST",
            SORT.into(),
            Some(json!({"taskListId": L1, "content": "[]"})),
        ),
        (
            "PUT",
            SORT.into(),
            Some(json!({"taskListId": L1, "content": "[]"})),
        ),
        ("DELETE", format!("{SORT}/{L1}"), None),
        ("POST", SORT_QUERY.into(), Some(json!({}))),
    ]
}

#[tokio::test]
async fn the_gate_refuses_until_the_app_data_import() {
    let s = Server::with_config(Config::default());
    let t = s.login(SERIAL).await;
    let v = s.post(TASK_ALL, &t, json!({})).await;
    assert_err(&v, "E0079");
    assert!(
        s.last_flags(TASK_ALL)
            .contains(&"protect_refused".to_string())
    );

    s.mark_imported();
    for (method, path, body) in every_route() {
        let v = s.raw(method, &path, Some(&t), body).await.json();
        assert_err(&v, "E0079");
        let flags = s.last_flags(&path);
        assert!(
            flags.contains(&"app_data_refused".to_string()),
            "{method} {path}: {flags:?}"
        );
    }
    assert!(!s.tasks_dir().exists());
    assert!(s.task_events().is_empty());

    s.mark_app_data_imported();
    for (method, path, body) in every_route() {
        let v = s.raw(method, &path, Some(&t), body).await.json();
        assert_ok(&v);
    }
}

#[tokio::test]
async fn with_protect_off_the_routes_are_open() {
    let s = Server::with_config(Config {
        protect: false,
        ..Config::default()
    });
    let t = s.login(SERIAL).await;
    s.seed(&t).await;
    assert_eq!(ids_of(&s.pull(&t, json!({})).await).len(), 3);
}

#[tokio::test]
async fn without_a_token_e0712_and_another_device_e0312() {
    let s = Server::new();
    let v = s.raw("POST", TASK_ALL, None, Some(json!({}))).await.json();
    assert_err(&v, "E0712");
    let v = s
        .raw("GET", &format!("{TASK}/{T1}"), Some("garbage"), None)
        .await
        .json();
    assert_err(&v, "E0712");
    let t = s.login(SERIAL).await;
    let mut body = task_body(T1, L1, "x");
    body["equipmentNo"] = json!(OTHER);
    assert_err(&s.post(TASK, &t, body).await, "E0312");
    assert!(!s.tasks_dir().exists());
}

#[tokio::test]
async fn refuse_mode_refuses_the_todo_routes() {
    let s = Server::with_config(Config {
        sync: SyncMode::Refuse,
        ..Config::default()
    });
    s.mark_imported();
    s.mark_app_data_imported();
    let t = s.login(SERIAL).await;
    for (method, path, body) in every_route() {
        let v = s.raw(method, &path, Some(&t), body).await.json();
        assert_err(&v, "E0079");
        assert!(s.last_flags(&path).is_empty(), "{method} {path}");
    }
    assert!(!s.tasks_dir().exists());
}

#[tokio::test]
async fn another_method_on_a_todo_path_is_refused() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let v = s.raw("GET", TASK_ALL, Some(&t), None).await.json();
    assert_err(&v, "E0079");
    let v = s
        .raw("POST", &format!("{TASK}/{T1}"), Some(&t), Some(json!({})))
        .await
        .json();
    assert_err(&v, "E0079");
}

#[tokio::test]
async fn a_todo_call_never_revives_a_lapsed_sync_lock() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    assert_eq!(s.holder().as_deref(), Some(SERIAL));
    s.advance_ms(30_001);
    for (method, path, body) in every_route() {
        assert_ok(&s.raw(method, &path, Some(&t), body).await.json());
    }
    assert_eq!(s.holder(), None);
}

#[tokio::test]
async fn a_todo_call_never_extends_a_live_sync_lock() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    assert_ok(&s.post(START, &t, json!({"equipmentNo": SERIAL})).await);
    s.advance_ms(20_000);
    for (method, path, body) in every_route() {
        assert_ok(&s.raw(method, &path, Some(&t), body).await.json());
    }
    s.advance_ms(10_001);
    assert_eq!(s.holder(), None);
}

#[tokio::test]
async fn todo_calls_pass_while_another_device_holds_the_sync_lock() {
    let s = Server::new();
    let a = s.login(SERIAL).await;
    let b = s.login(OTHER).await;
    assert_ok(&s.post(START, &a, json!({"equipmentNo": SERIAL})).await);
    let mut body = task_body(T1, L1, "x");
    body["equipmentNo"] = json!(OTHER);
    assert_ok(&s.post(TASK, &b, body).await);
    assert_eq!(ids_of(&s.pull(&b, json!({})).await), vec![T1]);
    assert_eq!(s.holder().as_deref(), Some(SERIAL));
}

#[tokio::test]
async fn an_oversized_body_on_a_route_with_an_id_is_a_bad_parameter() {
    let s = Server::new();
    let t = s.login(SERIAL).await;
    let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
    let path = format!("{TASK}/{T1}");
    let req = Request::builder()
        .method("DELETE")
        .uri(&path)
        .header("x-access-token", &t)
        .header("content-type", "application/json")
        .body(Body::from(vec![
            b' ';
            notedown_cloud::server::MAX_REQUEST_BODY + 1
        ]))
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
    assert!(s.last_flags(&path).is_empty());
}
