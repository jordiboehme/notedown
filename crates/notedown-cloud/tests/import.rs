//! `notedown cloud import`, `put` and `backup-device`: a device backup fills an account
//! completely (Review Focus 1), the protect gate opens only after a verified import, `put`
//! copies one file in under the store lock, and the device backup keeps names byte-exact.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use md5::{Digest, Md5};
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::device_backup::{self, BackupOptions};
use notedown_cloud::import::{self, ImportOptions, SkipReason};
use notedown_cloud::journal::{Event, EventBody, Journal, Origin};
use notedown_cloud::put::{self, PutOutcome};
use notedown_cloud::server::{AppState, router};
use notedown_cloud::store::{Kind, Store, WirePath};
use notedown_cloud::{Clock, Config, Error};
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset};
use tower::ServiceExt;

const SERIAL: &str = "SN078C10000000";
const EMAIL: &str = "jordi@example.org";
const T0: i64 = 1_791_270_000_000;
const START: &str = "/api/file/2/files/synchronous/start";
const LIST: &str = "/api/file/2/files/list_folder";

/// NFD: `U` + combining diaeresis.
const NFD_NAME: &str = "U\u{308}berleben.note";
/// Spaces, `+`, `%` and `'` in one name.
const ODD_NAME: &str = "On the Road + 100% 'Zabel'.epub";

fn md5(bytes: &[u8]) -> String {
    const_hex::encode(Md5::digest(bytes))
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A synthetic device backup with all six folders, the SD card, an NFD name, a name with
/// spaces, `+`, `%` and `'`, an empty folder, a `.snbak`, dot-files and an unknown top entry.
/// Returns the backup folder and the expected wire files (path -> bytes) and wire folders.
fn backup_tree(root: &Path) -> (PathBuf, BTreeMap<String, Vec<u8>>, BTreeSet<String>) {
    let b = root.join("backup");
    let mut files = BTreeMap::new();
    let mut add = |rel: &str, wire: &str, bytes: &[u8]| {
        write(&b.join(rel), bytes);
        files.insert(wire.to_string(), bytes.to_vec());
    };
    add("Note/Bike.note", "NOTE/Note/Bike.note", b"bike note bytes");
    add(
        &format!("Note/Work/{NFD_NAME}"),
        &format!("NOTE/Note/Work/{NFD_NAME}"),
        b"nfd",
    );
    add(
        "Note/Work/Deep/x.note",
        "NOTE/Note/Work/Deep/x.note",
        b"deep",
    );
    add("MyStyle/style.png", "NOTE/MyStyle/style.png", b"png");
    add(
        &format!("Document/Books/{ODD_NAME}"),
        &format!("DOCUMENT/Document/Books/{ODD_NAME}"),
        b"epub bytes",
    );
    add("Document/empty.pdf", "DOCUMENT/Document/empty.pdf", b"");
    add("EXPORT/e.pdf", "EXPORT/e.pdf", b"export");
    add("SCREENSHOT/s.png", "SCREENSHOT/s.png", b"shot");
    add("INBOX/in.txt", "INBOX/in.txt", b"inbox");
    // empty folders are kept
    std::fs::create_dir_all(b.join("Note/Empty")).unwrap();
    std::fs::create_dir_all(b.join("Document/Books/Nothing")).unwrap();
    // skipped
    write(&b.join("REMOVABLE0/sd.note"), b"sd card");
    write(&b.join("Note/Bike.snbak"), b"device backup");
    write(&b.join("Note/.DS_Store"), b"finder");
    write(&b.join(".hidden"), b"dot");
    write(&b.join("Unknown/u.txt"), b"unknown top folder");
    write(
        &b.join("backup.json"),
        br#"{"url":"http://device:8089","errors":[]}"#,
    );
    let folders: BTreeSet<String> = [
        "NOTE",
        "NOTE/Note",
        "NOTE/Note/Work",
        "NOTE/Note/Work/Deep",
        "NOTE/Note/Empty",
        "NOTE/MyStyle",
        "DOCUMENT",
        "DOCUMENT/Document",
        "DOCUMENT/Document/Books",
        "DOCUMENT/Document/Books/Nothing",
        "EXPORT",
        "SCREENSHOT",
        "INBOX",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    (b, files, folders)
}

fn data_with_account() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    Accounts::new(dir.path())
        .create("jordi", EMAIL, "secret")
        .unwrap();
    dir
}

fn quiet() -> impl FnMut(&str) {
    |_| {}
}

/// Every path below `root` with its bytes (folders as `None`).
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.insert(p.clone(), None);
                stack.push(p);
            } else {
                out.insert(p.clone(), Some(std::fs::read(&p).unwrap()));
            }
        }
    }
    out
}

fn events(data: &Path) -> Vec<Event> {
    Journal::open(data)
        .unwrap()
        .read_from(0)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// (path, md5, size) of every file and the set of folders, as the store lists them.
fn store_view(data: &Path) -> (BTreeMap<String, (String, u64)>, BTreeSet<String>) {
    let store = Store::open(data, "jordi").unwrap();
    let g = store.lock().unwrap();
    let mut files = BTreeMap::new();
    let mut folders = BTreeSet::new();
    for e in store.list(&g, &WirePath::root(), true).unwrap() {
        match e.tag {
            Kind::File => {
                files.insert(e.path_display, (e.content_hash.unwrap(), e.size));
            }
            Kind::Folder => {
                folders.insert(e.path_display);
            }
        }
    }
    (files, folders)
}

fn run_import(
    data: &Path,
    backup: &Path,
    replace: bool,
) -> notedown_cloud::Result<import::ImportReport> {
    let journal = Journal::open(data).unwrap();
    import::import(
        data,
        "jordi",
        backup,
        &ImportOptions { replace },
        &journal,
        &mut quiet(),
    )
}

// ------------------------------------------------------------------ import

#[test]
fn scan_maps_the_device_folders_and_reports_every_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, files, _) = backup_tree(tmp.path());
    let scan = import::scan(&backup).unwrap();
    assert!(scan.errors.is_empty(), "{:?}", scan.errors);
    let mapping: Vec<(String, String)> = scan
        .folders
        .iter()
        .map(|f| (f.source.clone(), f.target.clone()))
        .collect();
    assert_eq!(
        mapping,
        [
            ("Document", "DOCUMENT/Document"),
            ("EXPORT", "EXPORT"),
            ("INBOX", "INBOX"),
            ("MyStyle", "NOTE/MyStyle"),
            ("Note", "NOTE/Note"),
            ("SCREENSHOT", "SCREENSHOT"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    let note = scan.folders.iter().find(|f| f.source == "Note").unwrap();
    assert_eq!((note.files, note.folders), (3, 3));
    let total: u64 = scan.folders.iter().map(|f| f.files).sum();
    assert_eq!(total as usize, files.len());
    let skips: BTreeSet<(String, SkipReason)> = scan
        .skipped
        .iter()
        .map(|s| (s.path.clone(), s.reason))
        .collect();
    let want: BTreeSet<(String, SkipReason)> = [
        (".hidden", SkipReason::DotFile),
        ("Note/.DS_Store", SkipReason::DotFile),
        ("Note/Bike.snbak", SkipReason::Snbak),
        ("REMOVABLE0", SkipReason::SdCard),
        ("Unknown", SkipReason::Unknown),
        ("backup.json", SkipReason::BackupFile),
    ]
    .into_iter()
    .map(|(p, r)| (p.to_string(), r))
    .collect();
    assert_eq!(skips, want);
}

#[test]
fn a_dry_run_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, _, _) = backup_tree(tmp.path());
    let data = data_with_account();
    let before = tree(data.path());
    let backup_before = tree(&backup);
    let plan = import::dry_run(data.path(), "jordi", &backup).unwrap();
    assert_eq!(plan.files_present, 0);
    assert_eq!(plan.scan.folders.len(), 6);
    assert_eq!(tree(data.path()), before);
    assert_eq!(tree(&backup), backup_before);
    assert!(!data.path().join("accounts/jordi/files").exists());
    assert!(!data.path().join("journal").exists());
}

#[test]
fn an_import_lists_every_file_with_its_wire_path_md5_and_size() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, files, folders) = backup_tree(tmp.path());
    let data = data_with_account();
    let report = run_import(data.path(), &backup, false).unwrap();
    assert_eq!(report.files as usize, files.len());
    let bytes: u64 = files.values().map(|b| b.len() as u64).sum();
    assert_eq!(report.bytes, bytes);

    let (listed, listed_folders) = store_view(data.path());
    let want: BTreeMap<String, (String, u64)> = files
        .iter()
        .map(|(p, b)| (p.clone(), (md5(b), b.len() as u64)))
        .collect();
    assert_eq!(listed, want);
    assert_eq!(listed_folders, folders);
    // byte-exact names on disk: the NFD name is not the NFC one
    let work = data.path().join("accounts/jordi/files/NOTE/Note/Work");
    let names: Vec<Vec<u8>> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_encoded_bytes())
        .collect();
    assert!(names.contains(&NFD_NAME.as_bytes().to_vec()), "{names:?}");
    assert!(!data.path().join("accounts/jordi/files/REMOVABLE0").exists());
    // no staging left behind
    let staging = data.path().join("accounts/jordi/staging");
    assert!(!staging.exists() || std::fs::read_dir(&staging).unwrap().next().is_none());

    // the marker
    let account = Accounts::new(data.path()).get("jordi").unwrap().unwrap();
    let marker = account.imported.unwrap();
    assert_eq!(marker.source, backup.display().to_string());
    assert_eq!((marker.files, marker.bytes), (files.len() as u64, bytes));

    // journal: every file and folder as created, origin cli, one commit each
    let ev = events(data.path());
    assert!(ev.iter().all(|e| e.origin == Origin::Cli), "{ev:?}");
    let created: BTreeSet<String> = ev
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::FileCreated { path, md5, .. } => {
                assert_eq!(Some(md5), want.get(path).map(|w| &w.0));
                Some(path.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(created, files.keys().cloned().collect());
    let file_ops: BTreeSet<&str> = ev
        .iter()
        .filter(|e| matches!(e.body, EventBody::FileCreated { .. }))
        .map(|e| e.op_id.as_str())
        .collect();
    assert_eq!(file_ops.len(), files.len(), "one commit per file");
    for f in ["NOTE/Note/Empty", "DOCUMENT/Document/Books/Nothing"] {
        assert!(
            ev.iter()
                .any(|e| matches!(&e.body, EventBody::FolderCreated { path, .. } if path == f)),
            "{f}"
        );
    }
}

#[test]
fn a_second_import_is_refused_without_replace() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, files, _) = backup_tree(tmp.path());
    let data = data_with_account();
    run_import(data.path(), &backup, false).unwrap();
    let before = events(data.path()).len();
    let err = run_import(data.path(), &backup, false).unwrap_err();
    assert!(err.to_string().contains("--replace"), "{err}");
    assert_eq!(events(data.path()).len(), before);
    let store = Store::open(data.path(), "jordi").unwrap();
    let old_id = store
        .id_of(&WirePath::parse("NOTE/Note/Bike.note").unwrap())
        .unwrap()
        .unwrap();

    // with --replace the old tree goes to the recycle bin and the new one comes in
    std::fs::write(backup.join("Note/Bike.note"), b"newer bike").unwrap();
    std::fs::remove_file(backup.join("EXPORT/e.pdf")).unwrap();
    let report = run_import(data.path(), &backup, true).unwrap();
    assert_eq!(report.replaced as usize, files.len());
    let (listed, _) = store_view(data.path());
    assert_eq!(listed["NOTE/Note/Bike.note"].0, md5(b"newer bike"));
    assert!(!listed.contains_key("EXPORT/e.pdf"));
    assert_eq!(listed.len(), files.len() - 1);
    let new_id = store
        .id_of(&WirePath::parse("NOTE/Note/Bike.note").unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(new_id, old_id);
    let g = store.lock().unwrap();
    let index = store.recycle_index(&g).unwrap();
    let old = index.iter().find(|e| e.id == old_id).unwrap();
    assert_eq!(old.md5, md5(b"bike note bytes"));
    assert_eq!(
        std::fs::read(store.recycle_dir().join(old.location())).unwrap(),
        b"bike note bytes"
    );
    drop(g);
    let ev = events(data.path());
    assert!(ev.iter().any(
        |e| matches!(&e.body, EventBody::FileDeleted { id, .. } if *id == old_id)
            && e.origin == Origin::Cli
    ));
    // native folders kept their ids and stayed
    let (_, folders) = store_view(data.path());
    assert!(folders.contains("EXPORT"));
}

#[test]
fn a_bad_name_in_the_backup_refuses_the_whole_import() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, _, _) = backup_tree(tmp.path());
    write(&backup.join("Note/back\\slash.note"), b"x");
    let data = data_with_account();
    let err = run_import(data.path(), &backup, false).unwrap_err();
    assert!(err.to_string().contains("back\\slash.note"), "{err}");
    assert!(!data.path().join("accounts/jordi/files").exists());
    assert!(
        Accounts::new(data.path())
            .get("jordi")
            .unwrap()
            .unwrap()
            .imported
            .is_none()
    );
}

#[test]
fn a_backup_with_errors_in_backup_json_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, _, _) = backup_tree(tmp.path());
    write(
        &backup.join("backup.json"),
        br#"{"errors":[{"path":"Note/a.note","message":"size 3, expected 4"}]}"#,
    );
    let data = data_with_account();
    let err = run_import(data.path(), &backup, false).unwrap_err();
    assert!(err.to_string().contains("Note/a.note"), "{err}");
    // errors on the SD card do not count: it is never imported
    write(
        &backup.join("backup.json"),
        br#"{"errors":[{"path":"REMOVABLE0/a.note","message":"x"}]}"#,
    );
    run_import(data.path(), &backup, false).unwrap();
}

#[test]
fn a_wrong_or_partial_backup_folder_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, _, _) = backup_tree(tmp.path());
    let data = data_with_account();
    // one level too deep: every entry would be "not a device folder"
    let err = run_import(data.path(), &backup.join("Note"), false).unwrap_err();
    assert!(
        err.to_string()
            .contains("INBOX: the backup has no such folder"),
        "{err}"
    );
    // a backup without INBOX
    std::fs::remove_dir_all(backup.join("INBOX")).unwrap();
    let err = run_import(data.path(), &backup, false).unwrap_err();
    assert!(
        err.to_string()
            .contains("INBOX: the backup has no such folder"),
        "{err}"
    );
    assert!(!err.to_string().contains("Note:"), "{err}");
    let plan = import::dry_run(data.path(), "jordi", &backup).unwrap();
    assert_eq!(plan.scan.errors.len(), 1, "{:?}", plan.scan.errors);
    // a device folder that is a file
    write(&backup.join("INBOX"), b"not a folder");
    let err = run_import(data.path(), &backup, false).unwrap_err();
    assert!(
        err.to_string()
            .contains("INBOX: a device folder that is not a plain folder"),
        "{err}"
    );
    assert!(!data.path().join("accounts/jordi/files").exists());
    assert!(
        Accounts::new(data.path())
            .get("jordi")
            .unwrap()
            .unwrap()
            .imported
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn a_failed_import_closes_the_gate_even_on_an_account_without_files() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (backup, _, _) = backup_tree(tmp.path());
    let data = data_with_account();
    let accounts = Accounts::new(data.path());
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.imported = Some(notedown_cloud::auth::Imported {
        at: "2026-10-06T10:00:00Z".into(),
        source: "/old".into(),
        files: 1,
        bytes: 1,
    });
    accounts.save(&a).unwrap();
    // the scan only reads metadata; the copy fails later
    let locked = backup.join("SCREENSHOT/s.png");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = run_import(data.path(), &backup, false);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = err.unwrap_err();
    assert!(err.to_string().contains("SCREENSHOT/s.png"), "{err}");
    assert!(accounts.get("jordi").unwrap().unwrap().imported.is_none());
}

// ------------------------------------------------------------------ protect gate, routes

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(self.0.load(Ordering::SeqCst) as i128 * 1_000_000)
            .unwrap()
    }
}

async fn call(state: &Arc<AppState>, path: &str, token: Option<&str>, body: Value) -> Value {
    let addr: SocketAddr = "192.168.1.50:40000".parse().unwrap();
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("host", "192.168.1.10:19072");
    if let Some(t) = token {
        req = req.header("x-access-token", t);
    }
    let resp = router(state.clone())
        .layer(MockConnectInfo(addr))
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{path}");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn login(state: &Arc<AppState>) -> String {
    let ch = call(
        state,
        "/api/official/user/query/random/code",
        None,
        json!({"account": EMAIL, "version": "202407"}),
    )
    .await;
    let code = ch["randomCode"].as_str().unwrap();
    let hash = expected_login_hash(&md5_hex("secret"), code);
    let login = call(
        state,
        "/api/official/user/account/login/equipment",
        None,
        json!({"account": EMAIL, "password": hash, "equipmentNo": SERIAL,
               "equipment": 3, "loginMethod": "2",
               "timestamp": ch["timestamp"], "version": "202407"}),
    )
    .await;
    assert_eq!(login["success"], true, "{login}");
    let token = login["token"].as_str().unwrap().to_string();
    let bind = call(
        state,
        "/api/terminal/user/bindEquipment",
        Some(&token),
        json!({"account": EMAIL, "equipmentNo": SERIAL, "flag": "1",
               "name": "Supernote Nomad", "totalCapacity": "25485312",
               "version": "202407"}),
    )
    .await;
    assert_eq!(bind["success"], true, "{bind}");
    token
}

#[tokio::test(flavor = "multi_thread")]
async fn the_protect_gate_opens_after_the_import_and_list_folder_shows_the_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let (backup, files, folders) = backup_tree(tmp.path());
    let data = data_with_account();
    let clock = Arc::new(TestClock(AtomicI64::new(T0)));
    let state = AppState::open_with_clock(Config::default(), data.path(), clock).unwrap();
    assert!(state.config.protect);
    let token = login(&state).await;
    // bindEquipment created the empty native folders; they do not count as files present
    let refused = call(&state, START, Some(&token), json!({"equipmentNo": SERIAL})).await;
    assert_eq!(refused["errorCode"], "E0079", "{refused}");

    let (d, b) = (data.path().to_path_buf(), backup.clone());
    tokio::task::spawn_blocking(move || run_import(&d, &b, false).unwrap())
        .await
        .unwrap();

    let started = call(&state, START, Some(&token), json!({"equipmentNo": SERIAL})).await;
    assert_eq!(started["success"], true, "{started}");
    let listed = call(
        &state,
        LIST,
        Some(&token),
        json!({"equipmentNo": SERIAL, "path": "/", "recursive": true}),
    )
    .await;
    let mut got_files = BTreeMap::new();
    let mut got_folders = BTreeSet::new();
    for e in listed["entries"].as_array().unwrap() {
        let path = e["path_display"].as_str().unwrap().to_string();
        if e["tag"] == "file" {
            got_files.insert(
                path,
                (
                    e["content_hash"].as_str().unwrap().to_string(),
                    e["size"].as_u64().unwrap(),
                ),
            );
        } else {
            got_folders.insert(path);
        }
    }
    let want: BTreeMap<String, (String, u64)> = files
        .iter()
        .map(|(p, b)| (p.clone(), (md5(b), b.len() as u64)))
        .collect();
    assert_eq!(got_files, want);
    assert_eq!(got_folders, folders);
    assert!(!listed.to_string().contains("REMOVABLE0"));
    assert!(!listed.to_string().contains("snbak"));
}

// ------------------------------------------------------------------ put

fn put_file(
    data: &Path,
    src: &Path,
    to: &str,
    name: Option<&str>,
) -> notedown_cloud::Result<put::PutResult> {
    let journal = Journal::open(data).unwrap();
    put::put(data, "jordi", &journal, src, to, name)
}

#[test]
fn put_waits_for_the_store_lock_and_journals_after_the_holder() {
    let data = data_with_account();
    let src = data.path().join("in.pdf");
    std::fs::write(&src, b"pdf bytes").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let d = data.path().to_path_buf();
    const HOLD: Duration = Duration::from_millis(400);
    let holder = std::thread::spawn(move || {
        let journal = Journal::open(&d).unwrap();
        let store = Store::open(&d, "jordi").unwrap();
        let g = store.lock_recovered(&journal).unwrap();
        let path = WirePath::parse("INBOX").unwrap();
        let c = store
            .plan_create_folder(&g, &path, Origin::Device, Some(SERIAL.into()))
            .unwrap();
        let seqs = store.commit(&g, c, &journal).unwrap();
        tx.send(Instant::now()).unwrap();
        std::thread::sleep(HOLD);
        drop(g);
        seqs
    });
    let since = rx.recv().unwrap();
    let r = put_file(data.path(), &src, "INBOX", None).unwrap();
    assert!(since.elapsed() >= HOLD, "put did not wait for the lock");
    let holder_seqs = holder.join().unwrap();
    assert_eq!(r.outcome, PutOutcome::Created);
    assert_eq!(r.path.display(), "INBOX/in.pdf");
    assert!(r.seqs.iter().all(|s| s > holder_seqs.last().unwrap()));
    let ev = events(data.path());
    let last = ev.last().unwrap();
    assert_eq!(last.origin, Origin::Cli);
    assert!(
        matches!(&last.body, EventBody::FileCreated { id, path, .. } if *id == r.id && path == "INBOX/in.pdf")
    );
    // put does not open the protect gate
    assert!(
        Accounts::new(data.path())
            .get("jordi")
            .unwrap()
            .unwrap()
            .imported
            .is_none()
    );
}

#[test]
fn put_creates_folders_replaces_with_the_same_id_and_recycles_the_old_bytes() {
    let data = data_with_account();
    let src = data.path().join("a.pdf");
    std::fs::write(&src, b"one").unwrap();
    let first = put_file(
        data.path(),
        &src,
        "DOCUMENT/Document/Inbox",
        Some("Report.pdf"),
    )
    .unwrap();
    assert_eq!(first.path.display(), "DOCUMENT/Document/Inbox/Report.pdf");
    let ev = events(data.path());
    let types: Vec<&str> = ev.iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        types,
        [
            "folder.created",
            "folder.created",
            "folder.created",
            "file.created"
        ]
    );
    std::fs::write(&src, b"two").unwrap();
    let second = put_file(
        data.path(),
        &src,
        "/DOCUMENT/Document/Inbox/",
        Some("Report.pdf"),
    )
    .unwrap();
    assert_eq!(second.outcome, PutOutcome::Replaced);
    assert_eq!(second.id, first.id);
    let store = Store::open(data.path(), "jordi").unwrap();
    let g = store.lock().unwrap();
    let index = store.recycle_index(&g).unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].md5, md5(b"one"));
    assert_eq!(
        std::fs::read(store.recycle_dir().join(index[0].location())).unwrap(),
        b"one"
    );
    drop(g);
    let third = put_file(
        data.path(),
        &src,
        "DOCUMENT/Document/Inbox",
        Some("Report.pdf"),
    )
    .unwrap();
    assert_eq!(third.outcome, PutOutcome::Unchanged);
    assert!(third.seqs.is_empty());
    let last = events(data.path()).pop().unwrap();
    assert!(matches!(last.body, EventBody::FileModified { .. }));
    assert_eq!(last.origin, Origin::Cli);
    // no staging left behind
    let staging = data.path().join("accounts/jordi/staging");
    assert!(!staging.exists() || std::fs::read_dir(&staging).unwrap().next().is_none());
}

#[test]
fn put_refuses_paths_outside_the_device_folders_and_name_collisions() {
    let data = data_with_account();
    let src = data.path().join("a.note");
    std::fs::write(&src, b"x").unwrap();
    for to in [
        "",
        "/",
        "NOTE",
        "DOCUMENT",
        "Other",
        "NOTE/Notes",
        "note/Note",
        "NOTE/Note/../x",
    ] {
        let r = put_file(data.path(), &src, to, None);
        assert!(r.is_err(), "{to:?} was accepted");
    }
    for name in [".hidden", "a/b", "..", ""] {
        assert!(
            put_file(data.path(), &src, "INBOX", Some(name)).is_err(),
            "{name:?}"
        );
    }
    put_file(data.path(), &src, "NOTE/Note", Some("Bike.note")).unwrap();
    for name in ["bike.note", "BIKE.note"] {
        let err = put_file(data.path(), &src, "NOTE/Note", Some(name)).unwrap_err();
        assert!(matches!(err, Error::NameCollision { .. }), "{err}");
    }
    let nfc = "\u{dc}berleben.note";
    put_file(data.path(), &src, "NOTE/Note", Some(NFD_NAME)).unwrap();
    let err = put_file(data.path(), &src, "NOTE/Note", Some(nfc)).unwrap_err();
    assert!(matches!(err, Error::NameCollision { .. }), "{err}");
    // a folder at the target
    std::fs::create_dir_all(data.path().join("accounts/jordi/files/INBOX/dir")).unwrap();
    assert!(put_file(data.path(), &src, "INBOX", Some("dir")).is_err());
}

// ------------------------------------------------------------------ backup-device

/// One page or file of the fake device, keyed by the raw (still encoded) request path.
#[derive(Clone)]
enum Served {
    Page(Vec<Value>),
    File(Vec<u8>),
}

fn entry(name: &str, uri: &str, dir: bool, size: u64) -> Value {
    json!({"date": "2026-10-01 12:34", "extension": if dir { "" } else { "note" },
           "isDirectory": dir, "name": name, "size": size, "uri": uri})
}

fn page_html(list: &[Value]) -> String {
    let body = json!({"deviceName": "Supernote", "fileList": list}).to_string();
    // the device escapes a single quote inside its single-quoted JS string
    let escaped = body.replace('\'', "\\'");
    format!(
        "<!DOCTYPE html>\n<html><head><title>Supernote</title></head><body>\n<script>\n    const json = '{escaped}'\n    render(JSON.parse(json));\n</script>\n</body></html>\n"
    )
}

/// A fake Browse & Access page on 127.0.0.1:0, on its own runtime thread. Unknown paths
/// answer 404, so a double-encoded uri fails.
fn fake_device(routes: BTreeMap<String, Served>) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let routes = Arc::new(routes);
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let routes = routes.clone();
                async move {
                    use axum::response::IntoResponse;
                    match routes.get(uri.path()) {
                        Some(Served::Page(list)) => {
                            ([("content-type", "text/html")], page_html(list)).into_response()
                        }
                        Some(Served::File(bytes)) => bytes.clone().into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

fn device_routes() -> BTreeMap<String, Served> {
    let mut r = BTreeMap::new();
    r.insert(
        "/".into(),
        Served::Page(vec![
            entry("Document", "/Document", true, 0),
            entry("Note", "/Note", true, 0),
            entry("INBOX", "/INBOX", true, 0),
            entry("REMOVABLE0", "/REMOVABLE0", true, 0),
        ]),
    );
    r.insert(
        "/Document".into(),
        Served::Page(vec![entry(
            "On the Road - Rick Zabel.epub",
            "/Document/On+the+Road+-+Rick+Zabel.epub",
            false,
            10,
        )]),
    );
    r.insert(
        "/Document/On+the+Road+-+Rick+Zabel.epub".into(),
        Served::File(b"epub bytes".to_vec()),
    );
    r.insert(
        "/Note".into(),
        Served::Page(vec![
            entry(NFD_NAME, "/Note/U%CC%88berleben.note", false, 3),
            entry("Jordi's list.note", "/Note/Jordi%27s+list.note", false, 4),
            entry("Short.note", "/Note/Short.note", false, 10),
            entry("Sub", "/Note/Sub", true, 0),
        ]),
    );
    r.insert(
        "/Note/U%CC%88berleben.note".into(),
        Served::File(b"nfd".to_vec()),
    );
    r.insert(
        "/Note/Jordi%27s+list.note".into(),
        Served::File(b"list".to_vec()),
    );
    // the listing says 10 bytes, the device sends 8
    r.insert(
        "/Note/Short.note".into(),
        Served::File(b"8 bytes!".to_vec()),
    );
    r.insert("/Note/Sub".into(), Served::Page(vec![]));
    r.insert("/INBOX".into(), Served::Page(vec![]));
    r.insert(
        "/REMOVABLE0".into(),
        Served::Page(vec![entry("sd.note", "/REMOVABLE0/sd.note", false, 2)]),
    );
    r.insert("/REMOVABLE0/sd.note".into(), Served::File(b"sd".to_vec()));
    r
}

fn names(dir: &Path) -> BTreeSet<Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_encoded_bytes())
        .collect()
}

#[test]
fn the_device_page_json_is_read_with_escaped_quotes() {
    let list = vec![entry("it's.note", "/Note/it%27s.note", false, 1)];
    let got = device_backup::parse_page(&page_html(&list)).unwrap();
    assert_eq!(got[0].name, "it's.note");
    assert_eq!(got[0].uri, "/Note/it%27s.note");
    // a raw quote (no escape) is read up to the last quote of the line
    let raw = "<script>\n    const json = '{\"deviceName\":\"Supernote\",\"fileList\":[{\"date\":\"2026-10-01 12:34\",\"extension\":\"note\",\"isDirectory\":false,\"name\":\"a'b.note\",\"size\":1,\"uri\":\"/a%27b.note\"}]}'\n</script>";
    assert_eq!(device_backup::parse_page(raw).unwrap()[0].name, "a'b.note");
    // a backslash escape of JSON passes through
    let esc = "    const json = '{\"fileList\":[{\"date\":\"2026-10-01 12:34\",\"isDirectory\":false,\"name\":\"a\\\\b\\\"c\",\"size\":1,\"uri\":\"/x\"}]}'\n";
    assert_eq!(device_backup::parse_page(esc).unwrap()[0].name, "a\\b\"c");
    assert!(device_backup::parse_page("<html>no listing</html>").is_err());
    assert!(device_backup::parse_page("    const json = 'not json'\n").is_err());
}

#[test]
fn backup_device_copies_byte_exact_names_and_fails_on_a_short_file() {
    let url = fake_device(device_routes());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("nomad");
    let opts = BackupOptions {
        include_sd: false,
        resume: false,
        utc_offset: UtcOffset::from_hms(2, 0, 0).unwrap(),
    };
    let report = device_backup::backup(&url, &dir, &opts, &mut quiet()).unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert_eq!(report.errors[0].path, "Note/Short.note");
    assert_eq!(report.files, 3);

    let top = names(&dir);
    let want: BTreeSet<Vec<u8>> = ["Document", "Note", "INBOX", "backup.json"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    assert_eq!(top, want, "REMOVABLE0 is skipped by default");
    let note = names(&dir.join("Note"));
    let want: BTreeSet<Vec<u8>> = [NFD_NAME, "Jordi's list.note", "Sub"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    assert_eq!(note, want, "no .part and no short file left");
    assert_eq!(
        std::fs::read(dir.join("Note").join(NFD_NAME)).unwrap(),
        b"nfd"
    );
    assert_eq!(
        std::fs::read(dir.join("Document/On the Road - Rick Zabel.epub")).unwrap(),
        b"epub bytes"
    );
    // the date is the mtime, read in the given offset: 12:34 at +02:00 is 10:34 UTC
    let mtime = std::fs::metadata(dir.join("Note/Jordi's list.note"))
        .unwrap()
        .modified()
        .unwrap();
    let want = OffsetDateTime::parse_rfc3339_free("2026-10-01T10:34:00Z");
    assert_eq!(OffsetDateTime::from(mtime), want);

    let json: Value =
        serde_json::from_slice(&std::fs::read(dir.join("backup.json")).unwrap()).unwrap();
    assert_eq!(json["url"], url);
    assert_eq!(json["files"], 3);
    assert_eq!(json["bytes"], 10 + 3 + 4);
    assert_eq!(json["folders"]["Note"]["files"], 2);
    assert_eq!(json["errors"][0]["path"], "Note/Short.note");

    // a rerun into the same folder needs --resume
    assert!(device_backup::backup(&url, &dir, &opts, &mut quiet()).is_err());
    let resumed = device_backup::backup(
        &url,
        &dir,
        &BackupOptions {
            resume: true,
            include_sd: true,
            ..opts
        },
        &mut quiet(),
    )
    .unwrap();
    assert_eq!(resumed.skipped_existing, 3);
    assert_eq!(
        std::fs::read(dir.join("REMOVABLE0/sd.note")).unwrap(),
        b"sd"
    );
}

#[test]
fn backup_device_refuses_names_that_leave_the_folder() {
    let mut r = BTreeMap::new();
    r.insert(
        "/".into(),
        Served::Page(vec![
            entry("..", "/x", true, 0),
            entry("a/b.note", "/a", false, 1),
            entry("Note", "/Note", true, 0),
            entry("note", "/note", true, 0),
        ]),
    );
    r.insert("/Note".into(), Served::Page(vec![]));
    r.insert("/note".into(), Served::Page(vec![]));
    let url = fake_device(r);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("nomad");
    let opts = BackupOptions {
        include_sd: false,
        resume: false,
        utc_offset: UtcOffset::UTC,
    };
    let report = device_backup::backup(&url, &dir, &opts, &mut quiet()).unwrap();
    assert_eq!(report.errors.len(), 3, "{:?}", report.errors);
    assert_eq!(
        names(tmp.path()),
        BTreeSet::from([b"nomad".to_vec()]),
        "nothing was written outside the folder"
    );
}

/// `OffsetDateTime` from an RFC 3339 text without the `parsing` feature of `time`.
trait FromRfc {
    fn parse_rfc3339_free(text: &str) -> OffsetDateTime;
}

impl FromRfc for OffsetDateTime {
    fn parse_rfc3339_free(text: &str) -> OffsetDateTime {
        // only the form used above: YYYY-MM-DDTHH:MM:SSZ
        let n = |r: std::ops::Range<usize>| text[r].parse::<i64>().unwrap();
        let date = time::Date::from_calendar_date(
            n(0..4) as i32,
            time::Month::try_from(n(5..7) as u8).unwrap(),
            n(8..10) as u8,
        )
        .unwrap();
        date.with_hms(n(11..13) as u8, n(14..16) as u8, n(17..19) as u8)
            .unwrap()
            .assume_utc()
    }
}
