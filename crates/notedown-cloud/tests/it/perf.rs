//! Benchmarks of the scan, the device routes, the server start and the snapshot on a large
//! synthetic account. Every test is ignored, so the normal `cargo test` never runs them. Run
//! them in release, one at a time (`scripts/perf.sh`):
//!
//! `cargo test --release -p notedown-cloud --test it perf -- --ignored --nocapture --test-threads=1`
//!
//! `NOTEDOWN_PERF_FILES` sets the number of files (default 20000). Each test builds its own
//! account in a temp dir and prints one line per measurement, `perf <name> files=<n> ms=<t>`
//! (some add `per=<ms per call>` and `max=<slowest call>`), and `setup` lines for the time the
//! account took to build. No test asserts a time; they only check that the result is right.
//!
//! The account (fixed seed, so runs compare): the native folders and the `imported` marker,
//! 95 % unique PDF-like files (of all files: 80 % of 4-64 KiB, 15 % of 1 MiB) below
//! `DOCUMENT/Document` and 5 % copies of a fixture note below `NOTE/Note`, in leaf folders of 50
//! files at `<native>/Tn/Snn/Lnn`. Each `S` folder holds about 2 % of the files. Unless a test
//! measures it, a whole-tree scan gives every file an id, an event and a cached MD5 first.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use md5::{Digest, Md5};
use notedown_cloud::auth::{Accounts, Imported, expected_login_hash, md5_hex};
use notedown_cloud::journal::{EventBody, Journal, Origin};
use notedown_cloud::server::{AppState, router};
use notedown_cloud::store::{ScanOptions, ScanReport, Store, WirePath};
use notedown_cloud::{Config, SystemClock};
use serde_json::{Value, json};
use tower::ServiceExt;

const EMAIL: &str = "jordi@example.org";
const SERIAL: &str = "SN078C10000000";
/// 2026-10-06 08:20 UTC.
const NOW: u64 = 1_791_270_000_000;
const DOC_ROOT: &str = "DOCUMENT/Document";
const NOTE_ROOT: &str = "NOTE/Note";
/// Files per leaf folder.
const LEAF: usize = 50;
/// `S` folders per `T` folder.
const S_PER_T: usize = 10;

const START: &str = "/api/file/2/files/synchronous/start";
const END: &str = "/api/file/2/files/synchronous/end";
const LIST: &str = "/api/file/2/files/list_folder";
const BY_PATH: &str = "/api/file/3/files/query/by/path_v3";
const APPLY: &str = "/api/file/3/files/upload/apply";
const FINISH: &str = "/api/file/2/files/upload/finish";

// ------------------------------------------------------------------ the synthetic account

/// The number of files, from `NOTEDOWN_PERF_FILES` (default 20000).
fn files_wanted() -> usize {
    std::env::var("NOTEDOWN_PERF_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

/// xorshift64: a fixed sequence without a new dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn fixture_note() -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join("tests/fixtures/notes/test1.note")).unwrap()
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn report(name: &str, files: usize, ms: f64) {
    println!("perf {name} files={files} ms={ms:.1}");
}

/// A measurement of `calls` calls with their times in ms: total, mean and the slowest.
fn report_calls(name: &str, files: usize, times: &[f64]) {
    let total: f64 = times.iter().sum();
    let max = times.iter().copied().fold(0.0, f64::max);
    println!(
        "perf {name} files={files} ms={total:.1} calls={} per={:.1} max={max:.1}",
        times.len(),
        total / times.len() as f64
    );
}

/// A file of the account (wire path) and whether its content is unique.
struct File {
    path: String,
    unique: bool,
    size: u64,
}

/// The synthetic account in a temp dir (removed on drop).
struct Account {
    dir: tempfile::TempDir,
    files: Vec<File>,
    /// Every folder below the native folders, parents first.
    folders: Vec<String>,
    /// The `S` folders of the PDF tree (about 2 % of the files each).
    s_folders: Vec<String>,
    /// The leaf folders of the PDF tree.
    leaves: Vec<String>,
}

impl Account {
    /// Build the account of the module docs with `n` files (no ids for them yet).
    fn build(n: usize) -> Account {
        let t = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        Accounts::new(data)
            .create("jordi", EMAIL, "secret")
            .unwrap();
        let store = Store::open(data, "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let accounts = Accounts::new(data);
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 0,
            bytes: 0,
        });
        accounts.save(&a).unwrap();
        drop(store);

        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // One random block; each file is a unique header plus a slice of it.
        let block: Vec<u8> = (0..(1 << 20) + (64 << 10))
            .map(|_| rng.next() as u8)
            .collect();
        let note = fixture_note();
        let notes = n / 20;
        // Leaves per S folder: an S folder holds about 2 % of the files.
        let per_s = (n / LEAF / 50).max(1);
        let mut acc = Account {
            dir,
            files: Vec::with_capacity(n),
            folders: Vec::new(),
            s_folders: Vec::new(),
            leaves: Vec::new(),
        };
        let mut made = std::collections::HashSet::new();
        for i in 0..n {
            let (root, k) = if i < notes {
                (NOTE_ROOT, i)
            } else {
                (DOC_ROOT, i - notes)
            };
            let leaf = k / LEAF;
            let s = leaf / per_s;
            let (tn, sn, ln) = (s / S_PER_T, s % S_PER_T, leaf % per_s);
            let folder = format!("{root}/T{tn}/S{sn:02}/L{ln:02}");
            if made.insert(folder.clone()) {
                for p in [
                    format!("{root}/T{tn}"),
                    format!("{root}/T{tn}/S{sn:02}"),
                    folder.clone(),
                ] {
                    if !acc.folders.contains(&p) {
                        acc.folders.push(p.clone());
                        if root == DOC_ROOT && p.matches('/').count() == 3 {
                            acc.s_folders.push(p);
                        }
                    }
                }
                if root == DOC_ROOT {
                    acc.leaves.push(folder.clone());
                }
                std::fs::create_dir_all(acc.fs(&folder)).unwrap();
            }
            let (path, bytes, unique) = if root == NOTE_ROOT {
                (format!("{folder}/n{i:06}.note"), note.clone(), false)
            } else {
                // Of all files: 15 % of 1 MiB, the rest (80 %) of 4-64 KiB.
                let size = if rng.below(95) < 15 {
                    1 << 20
                } else {
                    4096 + rng.below(60 << 10) as usize
                };
                let mut bytes = format!("{i:015}\n").into_bytes();
                let off = rng.below(64 << 10) as usize;
                bytes.extend_from_slice(&block[off..off + size - 16]);
                (format!("{folder}/f{i:06}.pdf"), bytes, true)
            };
            std::fs::write(acc.fs(&path), &bytes).unwrap();
            acc.files.push(File {
                path,
                unique,
                size: bytes.len() as u64,
            });
        }
        let bytes: u64 = acc.files.iter().map(|f| f.size).sum();
        println!(
            "setup build files={n} ms={:.1} folders={} mib={}",
            ms(t),
            acc.folders.len(),
            bytes >> 20
        );
        acc
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }

    fn fs(&self, rel: &str) -> PathBuf {
        self.data().join("accounts/jordi/files").join(rel)
    }

    fn n(&self) -> usize {
        self.files.len()
    }

    fn store(&self) -> Store {
        Store::open(self.data(), "jordi").unwrap()
    }

    fn journal(&self) -> Journal {
        Journal::open(self.data()).unwrap()
    }

    /// The files and folders at `folder` and below.
    fn count_below(&self, folder: &str) -> (usize, usize) {
        let prefix = format!("{folder}/");
        let files = self
            .files
            .iter()
            .filter(|f| f.path.starts_with(&prefix))
            .count();
        let folders = self
            .folders
            .iter()
            .filter(|f| *f == folder || f.starts_with(&prefix))
            .count();
        (files, folders)
    }
}

fn scan(store: &Store, journal: &Journal, opts: &ScanOptions) -> ScanReport {
    let g = store.lock_recovered(journal).unwrap();
    store.scan(&g, journal, opts, NOW).unwrap()
}

fn whole() -> ScanOptions {
    ScanOptions::all(1)
}

fn wp(s: &str) -> WirePath {
    WirePath::parse_named(s).unwrap()
}

fn journal_len(journal: &Journal) -> usize {
    journal.read_from(1).unwrap().count()
}

/// The untimed first scan that gives every file an id, an event and a cached MD5.
fn baseline(acc: &Account, store: &Store, journal: &Journal) {
    let t = Instant::now();
    let r = scan(store, journal, &whole());
    assert_eq!(r.created.len(), acc.n() + acc.folders.len());
    println!("setup scan files={} ms={:.1}", acc.n(), ms(t));
}

// ------------------------------------------------------------------ P4.1, P4.2: whole scans

#[test]
#[ignore = "benchmark: scripts/perf.sh"]
fn perf_scan_cold_and_warm() {
    let acc = Account::build(files_wanted());
    let (store, journal) = (acc.store(), acc.journal());
    let t = Instant::now();
    let r = scan(&store, &journal, &whole());
    report("scan_cold", acc.n(), ms(t));
    assert_eq!(r.created.len(), acc.n() + acc.folders.len());
    assert!(r.modified.is_empty() && r.moved.is_empty() && r.deleted.is_empty());
    let events = journal
        .read_from(1)
        .unwrap()
        .map(|e| e.unwrap())
        .collect::<Vec<_>>();
    let created = events
        .iter()
        .filter(|e| matches!(e.body, EventBody::FileCreated { .. }))
        .count();
    assert_eq!(created, acc.n());
    assert!(events.iter().all(|e| e.origin == Origin::Scan));

    let len = events.len();
    let t = Instant::now();
    let r = scan(&store, &journal, &whole());
    report("scan_warm", acc.n(), ms(t));
    assert_eq!(r, ScanReport::default());
    assert_eq!(journal_len(&journal), len);
}

#[test]
#[ignore = "benchmark: scripts/perf.sh"]
fn perf_scan_after_upgrade() {
    let acc = Account::build(files_wanted());
    let (store, journal) = (acc.store(), acc.journal());
    baseline(&acc, &store, &journal);
    drop(store);
    // Rows as an older build wrote them: without `ctime_ns`, so none of them matches.
    let hashes = acc.data().join("accounts/jordi/hashes.json");
    let mut rows: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&hashes).unwrap()).unwrap();
    assert_eq!(rows.len(), acc.n());
    for row in rows.values_mut() {
        row.as_object_mut().unwrap().remove("ctime_ns");
    }
    std::fs::write(&hashes, serde_json::to_vec(&rows).unwrap()).unwrap();
    let len = journal_len(&journal);

    // A fresh store, as after the upgrade and a restart.
    let t = Instant::now();
    let store = acc.store();
    let r = scan(&store, &journal, &whole());
    report("scan_after_upgrade", acc.n(), ms(t));
    assert_eq!(r, ScanReport::default());
    assert_eq!(journal_len(&journal), len);
    let rows: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&hashes).unwrap()).unwrap();
    assert_eq!(rows.len(), acc.n());
    assert!(rows.values().all(|r| r["ctime_ns"].is_u64()));
}

// ------------------------------------------------------------------ P4.3, P4.4: over HTTP

/// A server on the account (through the router, in memory) with a bound device.
struct Server {
    state: Arc<AppState>,
    app: Router,
    token: String,
}

impl Server {
    fn open(acc: &Account) -> Server {
        let config = Config {
            protect: false,
            ..Config::default()
        };
        let state = AppState::open_with_clock(config, acc.data(), Arc::new(SystemClock)).unwrap();
        let now = state.now_ms();
        let (code, _) = state.auth.challenge(EMAIL, now).unwrap();
        let hash = expected_login_hash(&md5_hex("secret"), &code);
        let token = state.auth.login(EMAIL, &hash, SERIAL, now).unwrap().token;
        state
            .auth
            .devices()
            .update("jordi", SERIAL, |d| d.bound = true)
            .unwrap();
        let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
        let app = router(state.clone()).layer(MockConnectInfo(addr));
        Server { state, app, token }
    }

    /// The raw answer and the time until its last byte.
    async fn raw(&self, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (Vec<u8>, f64) {
        let mut req = Request::builder().method("POST").uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let t = Instant::now();
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        let took = ms(t);
        assert_eq!(status, StatusCode::OK, "{uri}");
        (bytes, took)
    }

    /// POST `body` as the device: the answer and the time of the call (parsing excluded).
    async fn post(&self, path: &str, body: Value) -> (Value, f64) {
        let (bytes, took) = self
            .raw(
                path,
                &[
                    ("x-access-token", &self.token),
                    ("content-type", "application/json"),
                    ("host", "192.168.1.10:19072"),
                ],
                body.to_string().into_bytes(),
            )
            .await;
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["success"], true, "{path}: {v}");
        (v, took)
    }

    fn events(&self) -> Vec<notedown_cloud::journal::Event> {
        self.state
            .journal
            .read_from(1)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }
}

/// `count` items of `all`, spread evenly (some twice when `all` holds fewer).
fn spread<T: Clone>(all: &[T], count: usize) -> Vec<T> {
    (0..count)
        .map(|i| all[i * all.len() / count].clone())
        .collect()
}

/// An account after its baseline scan, and a server on it.
fn served(n: usize) -> (Account, Server) {
    let acc = Account::build(n);
    {
        let (store, journal) = (acc.store(), acc.journal());
        baseline(&acc, &store, &journal);
    }
    let server = Server::open(&acc);
    (acc, server)
}

#[tokio::test]
#[ignore = "benchmark: scripts/perf.sh"]
async fn perf_device_sync() {
    let (acc, s) = served(files_wanted());
    let n = acc.n();
    let before = s.events().len();

    let (_, took) = s.post(START, json!({"equipmentNo": SERIAL})).await;
    report("sync_start", n, took);

    let (v, took) = s.post(LIST, json!({"path": "/", "recursive": true})).await;
    report("list_root_recursive", n, took);
    // Every file and folder below the root, the native folders too.
    let natives = notedown_cloud::store::NATIVE_FOLDERS.len();
    assert_eq!(
        v["entries"].as_array().unwrap().len(),
        n + acc.folders.len() + natives
    );

    let mut times = Vec::new();
    for folder in spread(&acc.leaves, 20) {
        let (v, took) = s
            .post(
                LIST,
                json!({"path": format!("/{folder}"), "recursive": false}),
            )
            .await;
        assert_eq!(
            v["entries"].as_array().unwrap().len(),
            acc.count_below(&folder).0
        );
        times.push(took);
    }
    report_calls("list_folder_x20", n, &times);

    let mut times = Vec::new();
    let files: Vec<&File> = acc.files.iter().collect();
    for f in spread(&files, 20) {
        let (v, took) = s
            .post(BY_PATH, json!({"path": format!("/{}", f.path)}))
            .await;
        assert_eq!(v["entriesVO"]["path_display"], f.path.as_str(), "{v}");
        times.push(took);
    }
    report_calls("query_by_path_x20", n, &times);

    let (_, took) = s
        .post(END, json!({"equipmentNo": SERIAL, "flag": "Y"}))
        .await;
    report("sync_end", n, took);
    // Nothing was changed by hand: the sync journals nothing.
    assert_eq!(s.events().len(), before);
}

fn path_and_query(url: &str) -> String {
    let rest = url.split_once("://").unwrap().1;
    rest[rest.find('/').unwrap()..].to_string()
}

/// A multipart body with one `file` field.
fn multipart(name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let b = "notedownPerfBoundary7MA4YWxk";
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

#[tokio::test]
#[ignore = "benchmark: scripts/perf.sh"]
async fn perf_uploads() {
    let (acc, s) = served(files_wanted());
    let n = acc.n();
    s.post(START, json!({"equipmentNo": SERIAL})).await;
    let before = s.events().len();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let (mut apply, mut send, mut finish, mut total) = (vec![], vec![], vec![], vec![]);
    for (i, folder) in spread(&acc.leaves, 50).into_iter().enumerate() {
        let name = format!("up{i:03}.pdf");
        let mut bytes = format!("upload {i:08}\n").into_bytes();
        bytes.extend((bytes.len()..64 << 10).map(|_| rng.next() as u8));
        let (a, ta) = s
            .post(
                APPLY,
                json!({"equipmentNo": SERIAL, "path": format!("/{folder}/{name}"),
                       "fileName": name, "size": bytes.len()}),
            )
            .await;
        let (ct, body) = multipart(&name, &bytes);
        let url = path_and_query(a["fullUploadUrl"].as_str().unwrap());
        let (up, tu) = s.raw(&url, &[("content-type", &ct)], body).await;
        let up: Value = serde_json::from_slice(&up).unwrap();
        assert_eq!(up["success"], true, "{up}");
        let (_, tf) = s
            .post(
                FINISH,
                json!({"equipmentNo": SERIAL, "path": format!("/{folder}/"), "fileName": name,
                       "content_hash": const_hex::encode(Md5::digest(&bytes)),
                       "innerName": a["innerName"], "size": bytes.len().to_string()}),
            )
            .await;
        assert!(acc.fs(&format!("{folder}/{name}")).is_file());
        apply.push(ta);
        send.push(tu);
        finish.push(tf);
        total.push(ta + tu + tf);
    }
    report_calls("upload_x50", n, &total);
    report_calls("upload_apply_x50", n, &apply);
    report_calls("upload_send_x50", n, &send);
    report_calls("upload_finish_x50", n, &finish);
    let created: Vec<_> = s.events()[before..]
        .iter()
        .filter(|e| matches!(e.body, EventBody::FileCreated { .. }))
        .map(|e| e.origin)
        .collect();
    assert_eq!(created, vec![Origin::Device; 50]);
}

// ------------------------------------------------------------------ P4.5, P4.6: hand changes

#[test]
#[ignore = "benchmark: scripts/perf.sh"]
fn perf_hand_delete() {
    let acc = Account::build(files_wanted());
    let (store, journal) = (acc.store(), acc.journal());
    baseline(&acc, &store, &journal);
    let folder = acc.s_folders[acc.s_folders.len() / 2].clone();
    let (files, folders) = acc.count_below(&folder);
    let away = acc.data().join("away");
    println!(
        "setup hand_delete folder={folder} files={files} folders={folders} share={:.1}%",
        100.0 * files as f64 / acc.n() as f64
    );

    // The whole tree.
    std::fs::rename(acc.fs(&folder), &away).unwrap();
    let t = Instant::now();
    let r = scan(&store, &journal, &whole());
    report("hand_delete_whole_scan", acc.n(), ms(t));
    assert_eq!(r.deleted.len(), files + folders);
    assert!(r.created.is_empty() && r.moved.is_empty());

    // Back (new ids), then the same delete and a scan of the parent only, as a route does.
    std::fs::rename(&away, acc.fs(&folder)).unwrap();
    let r = scan(&store, &journal, &whole());
    assert_eq!(r.created.len(), files + folders);
    std::fs::rename(acc.fs(&folder), &away).unwrap();
    let parent = wp(&folder).parent().unwrap();
    let opts = ScanOptions {
        scope: parent,
        recursive: false,
        ..whole()
    };
    let t = Instant::now();
    let r = scan(&store, &journal, &opts);
    report("hand_delete_parent_scan", acc.n(), ms(t));
    assert_eq!(r.deleted.len(), files + folders);
    assert!(r.created.is_empty() && r.moved.is_empty());
}

#[test]
#[ignore = "benchmark: scripts/perf.sh"]
fn perf_hand_move() {
    let acc = Account::build(files_wanted());
    let (store, journal) = (acc.store(), acc.journal());
    baseline(&acc, &store, &journal);
    // 100 unique files from all over the tree into one leaf folder.
    let target = acc.leaves[0].clone();
    let unique: Vec<&File> = acc
        .files
        .iter()
        .filter(|f| f.unique && !f.path.starts_with(&format!("{target}/")))
        .collect();
    for f in spread(&unique, 100) {
        let name = f.path.rsplit('/').next().unwrap();
        std::fs::rename(acc.fs(&f.path), acc.fs(&format!("{target}/{name}"))).unwrap();
    }
    let t = Instant::now();
    let r = scan(&store, &journal, &whole());
    report("hand_move_x100_whole_scan", acc.n(), ms(t));
    assert_eq!(r.moved.len(), 100);
    assert!(r.created.is_empty() && r.deleted.is_empty() && r.modified.is_empty());
}

// ------------------------------------------------------------------ P4.7, P4.8

#[test]
#[ignore = "benchmark: scripts/perf.sh"]
fn perf_server_start() {
    let acc = Account::build(files_wanted());
    {
        let (store, journal) = (acc.store(), acc.journal());
        baseline(&acc, &store, &journal);
    }
    let config = || Config {
        protect: false,
        ..Config::default()
    };
    // The first start after the baseline compacts the registry log it left.
    let t = Instant::now();
    let state = AppState::open(config(), acc.data()).unwrap();
    report("server_open_compacting", acc.n(), ms(t));
    drop(state);

    let t = Instant::now();
    let state = AppState::open(config(), acc.data()).unwrap();
    report("server_open", acc.n(), ms(t));
    let len = journal_len(&state.journal);
    // `serve` runs a whole-tree scan of every account right after the open.
    let t = Instant::now();
    state.scan_all();
    report("server_first_scan", acc.n(), ms(t));
    assert_eq!(journal_len(&state.journal), len);
}

#[test]
#[ignore = "benchmark: scripts/perf.sh"]
fn perf_snapshot() {
    let acc = Account::build(files_wanted());
    let (store, journal) = (acc.store(), acc.journal());
    baseline(&acc, &store, &journal);
    let len = journal_len(&journal);
    let t = Instant::now();
    let seqs = {
        let g = store.lock_recovered(&journal).unwrap();
        store.snapshot(&g, &journal).unwrap()
    };
    report("snapshot", acc.n(), ms(t));
    let events: Vec<_> = journal
        .read_from(len as u64 + 1)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(seqs.len(), events.len());
    let count = |f: fn(&EventBody) -> bool| events.iter().filter(|e| f(&e.body)).count();
    assert_eq!(
        count(|b| matches!(b, EventBody::FileCreated { .. })),
        acc.n()
    );
    let natives = notedown_cloud::store::NATIVE_FOLDERS.len();
    assert_eq!(
        count(|b| matches!(b, EventBody::FolderCreated { .. })),
        acc.folders.len() + natives
    );
    assert!(events.iter().all(|e| e.origin == Origin::Snapshot));
}
