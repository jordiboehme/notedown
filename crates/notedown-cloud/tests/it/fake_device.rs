//! A fake device runs a whole sync against `serve` on a real socket (127.0.0.1:0 only):
//! login with the real hash, `synchronous/start`, `list_folder`, the upload of a new note
//! into a new folder, a download, a move, a folder delete as one delete per file and
//! `synchronous/end` (`docs/sync-protocol.md` 2.4 and 3.4). Then the files on disk, the journal,
//! the protocol log and the answers for digests and to-dos are checked.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;

use md5::{Digest, Md5};
use notedown_cloud::Config;
use notedown_cloud::auth::{Accounts, expected_login_hash, md5_hex};
use notedown_cloud::import::{self, ImportOptions};
use notedown_cloud::journal::Journal;
use notedown_cloud::server::{AppState, bind, serve_until};
use reqwest::blocking::Client;
use serde_json::{Value, json};

const SERIAL: &str = "SN078C10000000";
const EMAIL: &str = "jordi@example.org";
const PASSWORD: &str = "a-long-test-password";

fn md5_of(b: &[u8]) -> String {
    const_hex::encode(Md5::digest(b))
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A small device backup: all six folders, five files.
fn backup(root: &Path) -> std::path::PathBuf {
    let b = root.join("backup");
    for dir in [
        "Note",
        "MyStyle",
        "Document",
        "EXPORT",
        "SCREENSHOT",
        "INBOX",
    ] {
        std::fs::create_dir_all(b.join(dir)).unwrap();
    }
    write(&b.join("Note/Bike.note"), b"bike note bytes");
    write(&b.join("Note/Old.note"), b"old note bytes");
    write(&b.join("Document/book.pdf"), b"book pdf bytes");
    write(&b.join("Document/Trip/a.pdf"), b"trip a");
    write(&b.join("Document/Trip/b.pdf"), b"trip b");
    write(
        &b.join("backup.json"),
        br#"{"url":"http://device:8089","complete":true,"errors":[]}"#,
    );
    b
}

fn multipart(name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let b = "notedownFakeDeviceBoundary";
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

/// The device: one HTTP client that counts its requests and keeps every secret it saw.
struct Device {
    base: String,
    http: Client,
    token: String,
    requests: usize,
    secrets: Vec<String>,
}

impl Device {
    fn send(&mut self, req: reqwest::blocking::RequestBuilder) -> (u16, Vec<u8>) {
        self.requests += 1;
        let resp = req.send().unwrap();
        let status = resp.status().as_u16();
        (status, resp.bytes().unwrap().to_vec())
    }

    fn post(&mut self, path: &str, body: Value) -> Value {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if !self.token.is_empty() {
            req = req.header("x-access-token", &self.token);
        }
        let (status, bytes) = self.send(req);
        assert_eq!(status, 200, "{path}");
        serde_json::from_slice(&bytes).unwrap()
    }

    fn ok(&mut self, path: &str, body: Value) -> Value {
        let v = self.post(path, body);
        assert_eq!(v["success"], true, "{path}: {v}");
        v
    }

    fn get(&mut self, url: &str) -> Vec<u8> {
        let req = self.http.get(url);
        let (status, bytes) = self.send(req);
        assert_eq!(status, 200, "{url}");
        bytes
    }

    fn listing(&mut self) -> Vec<Value> {
        let v = self.ok(
            "/api/file/2/files/list_folder",
            json!({"equipmentNo": SERIAL, "path": "/", "recursive": true}),
        );
        v["entries"].as_array().unwrap().clone()
    }

    fn id_of(entries: &[Value], path: &str) -> String {
        entries
            .iter()
            .find(|e| e["path_display"] == path)
            .unwrap_or_else(|| panic!("{path} is not listed"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn refused_routes(d: &mut Device) {
    for path in [
        "/api/file/query/summary/hash",
        "/api/file/schedule/group/all",
        "/api/file/schedule/task/all",
    ] {
        let v = d.post(path, json!({"equipmentNo": SERIAL}));
        assert_eq!(v["success"], false, "{path}: {v}");
        assert_eq!(v["errorCode"], "E0079", "{path}: {v}");
    }
}

/// The login of variant A, then the sync of `docs/sync-protocol.md` 3.4. Returns the device.
fn run_device(base: String) -> Device {
    let mut d = Device {
        base,
        http: Client::new(),
        token: String::new(),
        requests: 0,
        secrets: Vec::new(),
    };
    d.ok(
        "/api/terminal/equipment/unlink",
        json!({"equipmentNo": SERIAL, "version": "202407"}),
    );
    d.ok(
        "/api/official/user/check/exists/server",
        json!({"email": EMAIL, "version": "202407"}),
    );
    let ch = d.ok(
        "/api/official/user/query/random/code",
        json!({"account": EMAIL, "version": "202407"}),
    );
    let code = ch["randomCode"].as_str().unwrap().to_string();
    // the real hash: sha256_hex(md5(password) + randomCode)
    let hash = expected_login_hash(&md5_hex(PASSWORD), &code);
    let login = d.ok(
        "/api/official/user/account/login/equipment",
        json!({"account": EMAIL, "password": hash, "equipmentNo": SERIAL, "equipment": 3,
               "loginMethod": "2", "timestamp": ch["timestamp"], "version": "202407"}),
    );
    d.token = login["token"].as_str().unwrap().to_string();
    d.secrets.push(d.token.clone());
    d.secrets.push(hash);
    d.secrets.push(code.clone());
    d.secrets.push(md5_hex(PASSWORD));
    d.ok(
        "/api/terminal/user/bindEquipment",
        json!({"account": EMAIL, "equipmentNo": SERIAL, "flag": "1", "name": "Supernote Nomad",
               "totalCapacity": "25485312", "version": "202407",
               "label": ["DOCUMENT/Document", "NOTE/Note", "NOTE/MyStyle", "EXPORT",
                         "SCREENSHOT", "INBOX"]}),
    );
    d.ok("/api/user/query", json!({"equipmentNo": SERIAL}));

    d.ok(
        "/api/file/2/files/synchronous/start",
        json!({"equipmentNo": SERIAL}),
    );
    let entries = d.listing();
    let paths: BTreeSet<&str> = entries
        .iter()
        .map(|e| e["path_display"].as_str().unwrap())
        .collect();
    for p in [
        "NOTE/Note/Bike.note",
        "NOTE/Note/Old.note",
        "DOCUMENT/Document/book.pdf",
        "DOCUMENT/Document/Trip/a.pdf",
        "DOCUMENT/Document/Trip/b.pdf",
    ] {
        assert!(paths.contains(p), "{p} is missing from the listing");
    }
    let hash_of =
        |p: &str| entries.iter().find(|e| e["path_display"] == p).unwrap()["content_hash"].clone();
    assert_eq!(hash_of("NOTE/Note/Bike.note"), md5_of(b"bike note bytes"));

    // upload a new note into a new folder
    let new_bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note"),
    )
    .unwrap();
    let q = d.ok(
        "/api/file/3/files/query/by/path_v3",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work"}),
    );
    assert!(q["entriesVO"].is_null(), "{q}");
    let f = d.ok(
        "/api/file/2/files/create_folder_v2",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work", "autorename": false}),
    );
    assert_eq!(f["metadata"]["path_display"], "NOTE/Note/Work");
    let q = d.ok(
        "/api/file/3/files/query/by/path_v3",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work/New.note"}),
    );
    assert!(q["entriesVO"].is_null(), "{q}");
    let a = d.ok(
        "/api/file/3/files/upload/apply",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work/New.note",
               "fileName": "New.note", "size": new_bytes.len()}),
    );
    let url = a["fullUploadUrl"].as_str().unwrap().to_string();
    d.secrets.extend(
        url.split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|p| p.strip_prefix("signature="))
            .map(String::from),
    );
    let (ct, body) = multipart("New.note", &new_bytes);
    let (status, reply) = {
        let req = d.http.post(&url).header("content-type", ct).body(body);
        d.send(req)
    };
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&reply).unwrap()["success"],
        true
    );
    d.ok(
        "/api/file/2/files/upload/finish",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/Work/", "fileName": "New.note",
               "content_hash": md5_of(&new_bytes), "innerName": a["innerName"],
               "size": new_bytes.len().to_string()}),
    );

    // download
    let book = Device::id_of(&entries, "DOCUMENT/Document/book.pdf");
    let dl = d.ok(
        "/api/file/3/files/download_v3",
        json!({"equipmentNo": SERIAL, "id": book}),
    );
    let url = dl["url"].as_str().unwrap().to_string();
    d.secrets.extend(
        url.split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|p| p.strip_prefix("signature="))
            .map(String::from),
    );
    assert_eq!(d.get(&url), b"book pdf bytes");

    // the device replaces book.pdf with a new version
    let book_v2 = b"book pdf bytes, second version".to_vec();
    let a = d.ok(
        "/api/file/3/files/upload/apply",
        json!({"equipmentNo": SERIAL, "path": "/DOCUMENT/Document/book.pdf",
               "fileName": "book.pdf", "size": book_v2.len()}),
    );
    let (ct, body) = multipart("book.pdf", &book_v2);
    let url = a["fullUploadUrl"].as_str().unwrap().to_string();
    // The signature of this second upload URL must not reach the log either.
    d.secrets.extend(
        url.split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|p| p.strip_prefix("signature="))
            .map(String::from),
    );
    assert_eq!(a["innerName"], "book.pdf");
    let (status, _) = {
        let req = d.http.post(&url).header("content-type", ct).body(body);
        d.send(req)
    };
    assert_eq!(status, 200);
    d.ok(
        "/api/file/2/files/upload/finish",
        json!({"equipmentNo": SERIAL, "path": "/DOCUMENT/Document/", "fileName": "book.pdf",
               "content_hash": md5_of(&book_v2), "innerName": a["innerName"],
               "size": book_v2.len().to_string()}),
    );

    // move
    let old = Device::id_of(&entries, "NOTE/Note/Old.note");
    d.ok(
        "/api/file/3/files/query_v3",
        json!({"equipmentNo": SERIAL, "id": old}),
    );
    d.ok(
        "/api/file/3/files/move_v3",
        json!({"equipmentNo": SERIAL, "id": old, "to_path": "/NOTE/Note/Work/Old.note",
               "autorename": false}),
    );

    // a folder delete: one delete per file
    for p in [
        "DOCUMENT/Document/Trip/a.pdf",
        "DOCUMENT/Document/Trip/b.pdf",
    ] {
        let id = Device::id_of(&entries, p);
        d.ok(
            "/api/file/3/files/query_v3",
            json!({"equipmentNo": SERIAL, "id": id}),
        );
        d.ok(
            "/api/file/3/files/delete_folder_v3",
            json!({"equipmentNo": SERIAL, "id": id}),
        );
    }
    // digests and to-dos are not served yet; inside the session this does not break the sync
    refused_routes(&mut d);
    d.ok(
        "/api/file/2/files/synchronous/end",
        json!({"equipmentNo": SERIAL, "flag": "Y"}),
    );

    // and once more after the session
    refused_routes(&mut d);
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_device_sync_runs_against_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    Accounts::new(&data)
        .create("jordi", EMAIL, PASSWORD)
        .unwrap();
    let source = backup(tmp.path());

    // protect is on (the default): the gate opens with the import
    let config = Config {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        ..Config::default()
    };
    assert!(config.protect);
    let journal = Journal::open(&data).unwrap();
    let report = import::import(
        &data,
        "jordi",
        &source,
        &ImportOptions::default(),
        &journal,
        &mut |_: &str| {},
    )
    .unwrap();
    assert_eq!(report.files, 5);
    drop(journal);
    let import_seqs = {
        let j = Journal::open(&data).unwrap();
        j.read_from(0).unwrap().count() as u64
    };

    let listeners = bind(&config).await.unwrap();
    let addr: SocketAddr = listeners[0].local_addr().unwrap();
    assert!(addr.ip().is_loopback());
    let state = AppState::open(config, &data).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_until(listeners, state, async {
        let _ = rx.await;
    }));

    let base = format!("http://{addr}");
    let device = tokio::task::spawn_blocking(move || run_device(base))
        .await
        .unwrap();
    tx.send(()).unwrap();
    server.await.unwrap().unwrap();

    // the files on disk
    let files = data.join("accounts/jordi/files");
    assert_eq!(
        md5_of(&std::fs::read(files.join("NOTE/Note/Work/New.note")).unwrap()),
        md5_of(
            &std::fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note")
            )
            .unwrap()
        )
    );
    assert_eq!(
        std::fs::read(files.join("NOTE/Note/Work/Old.note")).unwrap(),
        b"old note bytes"
    );
    assert!(!files.join("NOTE/Note/Old.note").exists());
    assert!(!files.join("DOCUMENT/Document/Trip").exists());
    assert_eq!(
        std::fs::read(files.join("DOCUMENT/Document/book.pdf")).unwrap(),
        b"book pdf bytes, second version"
    );
    assert!(files.join("NOTE/Note/Bike.note").exists());

    // the journal: import events first, then the device events in order, no gaps in seq
    let events: Vec<Value> = Journal::open(&data)
        .unwrap()
        .read_from(0)
        .unwrap()
        .map(|e| serde_json::from_str(&e.unwrap().to_line()).unwrap())
        .collect();
    assert_eq!(events[0]["seq"], 1);
    for (i, e) in events.iter().enumerate() {
        assert_eq!(
            e["seq"].as_u64().unwrap(),
            events[0]["seq"].as_u64().unwrap() + i as u64
        );
    }
    assert!(events.len() as u64 > import_seqs);
    let device_events: Vec<(&str, &str)> = events[import_seqs as usize..]
        .iter()
        .map(|e| {
            (
                e["type"].as_str().unwrap(),
                e["path"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        device_events,
        [
            ("device.bound", ""),
            ("folder.created", "NOTE/Note/Work"),
            ("file.created", "NOTE/Note/Work/New.note"),
            ("page.added", ""),
            ("file.modified", "DOCUMENT/Document/book.pdf"),
            ("file.moved", "NOTE/Note/Work/Old.note"),
            ("file.deleted", "DOCUMENT/Document/Trip/a.pdf"),
            ("file.deleted", "DOCUMENT/Document/Trip/b.pdf"),
            ("folder.deleted", "DOCUMENT/Document/Trip"),
        ]
    );
    assert!(
        events[..import_seqs as usize]
            .iter()
            .all(|e| e["origin"] == "cli")
    );
    assert!(
        events[import_seqs as usize..]
            .iter()
            .all(|e| e["origin"] == "device")
    );

    // the recycle bin holds the replaced and the deleted bytes, each with an index line
    let recycle = data.join("accounts/jordi/recycle");
    let index = std::fs::read_to_string(recycle.join("index.jsonl")).unwrap();
    let rows: Vec<Value> = index
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for (path, bytes) in [
        ("DOCUMENT/Document/book.pdf", &b"book pdf bytes"[..]),
        ("DOCUMENT/Document/Trip/a.pdf", &b"trip a"[..]),
        ("DOCUMENT/Document/Trip/b.pdf", &b"trip b"[..]),
    ] {
        let row = rows
            .iter()
            .find(|r| r["path"] == path)
            .unwrap_or_else(|| panic!("no recycle line for {path}: {index}"));
        assert_eq!(row["md5"], md5_of(bytes), "{path}");
        let slot = format!("{}/{path}", row["deletedAt"]);
        assert_eq!(std::fs::read(recycle.join(&slot)).unwrap(), bytes, "{slot}");
    }

    // the protocol log: one entry per request, no secret in the clear
    let mut log_files: Vec<_> = std::fs::read_dir(data.join("logs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    log_files.sort();
    let text: String = log_files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect();
    let entries: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let requests = entries.iter().filter(|e| e["kind"] == "request").count();
    assert_eq!(requests, device.requests);
    assert!(!device.secrets.is_empty());
    for secret in &device.secrets {
        assert!(secret.len() >= 8);
        assert!(
            !text.contains(secret.as_str()),
            "a secret is in the log: {secret}"
        );
    }
    assert!(!text.contains(PASSWORD));
    // masking is visible: the login password and the token header are replaced
    let login: Vec<&Value> = entries
        .iter()
        .filter(|e| e["path"] == "/api/official/user/account/login/equipment")
        .collect();
    assert_eq!(login.len(), 1);
    assert!(login[0].to_string().contains("<masked:"), "{}", login[0]);
    let with_token = entries
        .iter()
        .find(|e| e["path"] == "/api/file/2/files/synchronous/start")
        .unwrap();
    assert!(with_token.to_string().contains("<masked:"), "{with_token}");
}
