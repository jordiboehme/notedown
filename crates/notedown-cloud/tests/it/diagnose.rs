//! `notedown cloud diagnose`: the account state, the dry-run scan and the bundle, on a fixture
//! data folder full of known private values that a safe bundle must not show.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notedown_cloud::auth::{Accounts, Device};
use notedown_cloud::diag::bundle::{DiagnoseOptions, diagnose};
use notedown_cloud::diag::check::Needles;
use notedown_cloud::diag::filter::Scrub;
use notedown_cloud::diag::state::{ScanResult, account_state, collect_needles, dry_run_scan};
use notedown_cloud::journal::Journal;
use notedown_cloud::store::{PENDING_FILE, ScanOptions, Store, take_store_lock};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const SLUG: &str = "jordi-test";
pub const EMAIL: &str = "privat@example.org";
pub const SERIAL_1: &str = "SN100TEST0001";
pub const SERIAL_2: &str = "SN100TEST0002";
pub const IP: &str = "192.168.77.5";

/// Every real value of the fixture that a safe bundle must not show, searched as raw bytes.
pub const SECRETS: &[&str] = &[
    "Secret plans",
    "Trips",
    "Spain",
    "beach",
    "Meeting 3.5 Smith",
    "Smith",
    "Old diary",
    "jordi-test",
    "privat@example.org",
    SERIAL_1,
    SERIAL_2,
    "Jordis Nomad",
    "Second Manta",
    IP,
    "my private digest text",
    "Hidden Book",
    "ABCD-1234",
    "comment words",
    "Call the doctor",
    "bring papers",
    "Private list",
    "Digest group name",
    "Lunch with Anna",
    "Ghost file",
    "ghost file",
    "Private Disk",
    "Volumes",
    "Vanished",
];

pub struct Fixture {
    pub dir: tempfile::TempDir,
}

impl Fixture {
    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn account(&self) -> PathBuf {
        self.data().join("accounts").join(SLUG)
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn ts(ago: Duration) -> String {
    (OffsetDateTime::now_utc() - ago).format(&Rfc3339).unwrap()
}

/// A data folder with one account, files, a recycle entry, a digest, to-dos, two devices, a
/// protocol log and journal events.
pub fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture { dir };
    let data = fx.data();
    std::fs::create_dir_all(&data).unwrap();
    Accounts::new(&data).create(SLUG, EMAIL, "pw").unwrap();
    let store = Store::open(&data, SLUG).unwrap();
    {
        let g = store.lock().unwrap();
        store.ensure_native_folders(&g).unwrap();
    }
    let files = fx.account().join("files");
    for (rel, bytes) in [
        ("NOTE/Note/Secret plans.note", &b"one"[..]),
        ("NOTE/Note/Secret plans 2.note", b"two"),
        ("DOCUMENT/Document/a", b"three"),
        ("NOTE/Note/Trips/Spain/beach.png", b"four"),
        ("NOTE/Note/Meeting 3.5 Smith/x.note", b"five"),
        // a name that is also a protocol key (`data` of a push frame)
        ("DOCUMENT/Document/data", b"six"),
    ] {
        write(&files.join(rel), bytes);
    }
    let journal = Journal::open(&data).unwrap();
    {
        let g = store.lock_recovered(&journal).unwrap();
        store
            .scan(&g, &journal, &ScanOptions::all(0), 1_791_270_000_000)
            .unwrap();
    }
    let acc = fx.account();
    write(
        &acc.join("recycle/index.jsonl"),
        br#"{"id":1999,"path":"NOTE/Note/Old diary.note","md5":"e29417db90606232f515eb10af94fab5","deletedAt":1791322981221,"reason":"deleted"}
"#,
    );
    write(
        &acc.join("recycle/1791322981221/NOTE/Note/Old diary.note"),
        b"old",
    );
    write(
        &acc.join("digests/items/1.json"),
        json!({"id": 1, "content": "my private digest text",
            "sourcePath": "/storage/ABCD-1234/Books/Hidden Book.epub", "commentStr": "comment words",
            "isDeleted": "N", "md5Hash": "2b78a16daaecd6328ead00f430446d6e",
            "equipmentNo": "null-null"})
        .to_string()
        .as_bytes(),
    );
    write(
        &acc.join("digests/groups/9.json"),
        json!({"id": 9, "name": "Digest group name"})
            .to_string()
            .as_bytes(),
    );
    write(&acc.join("digests/marks/abc.mark"), b"mark");
    write(
        &acc.join("tasks/items/t1.json"),
        json!({"taskId": "08e32ffac5fdadb1287f48099222621d", "title": "Call the doctor",
            "detail": "bring papers", "status": "needsAction"})
        .to_string()
        .as_bytes(),
    );
    write(
        &acc.join("tasks/groups/g1.json"),
        json!({"taskListId": "1bda96ede2baec839ee0214a1b0b040e", "title": "Private list"})
            .to_string()
            .as_bytes(),
    );
    for (serial, name) in [(SERIAL_1, "Jordis Nomad"), (SERIAL_2, "Second Manta")] {
        let d = Device {
            serial: serial.into(),
            name: Some(name.into()),
            bound: true,
            firmware: Some("202407".into()),
            total_capacity: None,
            token_generation: 1,
            last_login_ms: 1_791_322_352_199,
        };
        write(
            &acc.join(format!("devices/{serial}.json")),
            serde_json::to_vec(&d).unwrap().as_slice(),
        );
    }
    write(
        &data.join("server.toml"),
        format!("listen = [\"{IP}:19072\"]\nrender_dir = \"/Volumes/Private Disk/PDFs\"\n")
            .as_bytes(),
    );
    write_log(&data);
    fx
}

fn write_log(data: &Path) {
    let day = OffsetDateTime::now_utc().date();
    let path = data.join("logs").join(format!(
        "protocol-{:04}-{:02}-{:02}.jsonl",
        day.year(),
        day.month() as u8,
        day.day()
    ));
    let hour = Duration::from_secs(3600);
    let minute = Duration::from_secs(60);
    let headers = json!({"host": format!("{IP}:19072"), "equipmentno": SERIAL_1,
        "user-agent": "okhttp/5.1.0", "x-access-token": "<masked:199 chars>"});
    let base64_path =
        "WyJqb3JkaS10ZXN0IiwiU04xMDBURVNUMDAwMSIsMSwiTk9URS9Ob3RlL1NlY3JldCBwbGFucy5ub3RlIl0";
    let data_dir = data.to_string_lossy();
    let push = json!([
        "ServerMessage",
        json!({"code": "200", "msgType": "FILE-SYN", "data": [
        {"messageType": "DOWNLOADFILE", "equipmentNo": "null-null", "fileType": "FILE", "id": 1080,
         "originalName": "Secret plans.note", "originalPath": "NOTE/Note/Trips/", "size": 3}]})
        .to_string()
    ]);
    let lines = vec![
        json!({"ts": ts(hour), "kind": "request", "device": SERIAL_1, "method": "POST",
            "path": "/api/file/2/files/synchronous/start", "headers": headers,
            "request_body": {"equipmentNo": SERIAL_1}, "response_status": 200,
            "response_body": {"success": true, "equipmentNo": SERIAL_1}, "success": true}),
        json!({"ts": ts(hour - minute), "kind": "request", "device": SERIAL_1, "method": "POST",
            "path": "/api/file/3/files/upload/apply", "headers": headers,
            "request_body": {"equipmentNo": SERIAL_1, "fileName": "Secret plans.note",
                "path": "/NOTE/Note/Trips/Spain/Secret plans.note", "size": 3},
            "request_body_raw": format!("{{\"path\":\"/NOTE/Note/Secret plans.note\",\"equipmentNo\":\"{SERIAL_1}\"}}"),
            "response_status": 200, "response_body": {"success": true}, "success": true}),
        json!({"ts": ts(hour - 2 * minute), "kind": "request", "device": SERIAL_1, "method": "GET",
            "path": "/api/oss/download",
            "query": format!("path={base64_path}&signature=<masked:64 chars>&timestamp=1&nonce=ab&pathId=1077"),
            "headers": headers, "response_status": 200, "response_body": "<3 bytes streamed>"}),
        json!({"ts": ts(hour - 3 * minute), "kind": "request", "device": SERIAL_1, "method": "POST",
            "path": "/api/user/query", "headers": headers, "response_status": 200,
            "response_body": {"email": EMAIL, "userName": SLUG, "success": true}, "success": true}),
        json!({"ts": ts(hour - 4 * minute), "kind": "request", "device": SERIAL_2, "method": "POST",
            "path": "/api/file/2/files/synchronous/start",
            "headers": {"host": format!("{IP}:19072"), "equipmentno": SERIAL_2},
            "request_body": {"equipmentNo": SERIAL_2}, "response_status": 200,
            "response_body": {"success": false, "errorCode": "E0079",
                "errorMsg": format!("account {SLUG} is busy with Secret plans.note")},
            "success": false, "error_code": "E0079"}),
        json!({"ts": ts(hour - 5 * minute), "kind": "request", "device": SERIAL_1, "method": "POST",
            "path": "/api/file/xyz", "headers": headers, "response_status": 404,
            "flags": ["unknown_endpoint"]}),
        json!({"ts": ts(hour - 6 * minute), "kind": "frame", "device": SERIAL_1, "path": "/socket.io/",
            "response_body": format!("42{push}"), "direction": "out"}),
        json!({"ts": ts(hour - 7 * minute), "kind": "frame", "device": SERIAL_1, "path": "/socket.io/",
            "request_body": "2", "direction": "in"}),
        json!({"ts": ts(hour - 7 * minute), "kind": "frame", "device": SERIAL_1, "path": "/socket.io/",
            "response_body": "3", "direction": "out"}),
        json!({"ts": ts(hour - 8 * minute), "kind": "marker", "flags": ["recovery_failed"],
            "message": format!("account {SLUG}: recovery failed: {data_dir}/accounts/{SLUG}/files/NOTE/Note/Secret plans.note: gone")}),
        json!({"ts": ts(hour - 9 * minute), "kind": "marker", "device": SERIAL_1, "flags": ["implicit_lock"],
            "message": format!("account {SLUG}: sync lock taken for a write without synchronous/start")}),
        json!({"ts": ts(hour - 10 * minute), "kind": "request", "device": SERIAL_1, "method": "POST",
            "path": "/api/file/3/files/upload/apply", "response_status": 200,
            "response_body": {"success": false, "errorCode": "E0001",
                "errorMsg": "invalid type: string \"Lunch with Anna\", expected u64"},
            "success": false, "message": format!("account {SLUG}: \"Ghost file.note\" collides with \"ghost file.note\"")}),
        json!({"ts": ts(hour - 11 * minute), "kind": "marker",
            "message": "render: /Volumes/Private Disk/PDFs/x: no space left"}),
        json!({"ts": ts(hour - 12 * minute), "kind": "marker", "flags": ["store_damaged"],
            "message": format!("the store of account {SLUG} is damaged: NOTE/Note/Vanished folder is missing")}),
        json!({"ts": ts(Duration::from_secs(10 * 24 * 3600)), "kind": "request", "device": SERIAL_1,
            "method": "POST", "path": "/api/old"}),
    ];
    let mut text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    text.push_str("not json\n");
    write(&path, text.as_bytes());
}

#[test]
fn account_state_counts_files_recycle_digests_and_todos() {
    let fx = fixture();
    let s = account_state(&fx.data(), SLUG).unwrap();
    assert_eq!(s.files, 6);
    assert_eq!(s.notes, 3);
    assert_eq!(s.bytes, 3 + 3 + 5 + 4 + 4 + 3);
    // the native folders, Trips, Spain and the meeting folder
    assert_eq!(s.folders, 8 + 3);
    assert_eq!((s.recycle_entries, s.recycle_bytes), (1, 3));
    assert_eq!((s.digests, s.digest_groups, s.marks), (1, 1, 1));
    assert_eq!((s.todos, s.todo_groups), (1, 1));
    assert_eq!(s.devices.len(), 2);
    assert_eq!(s.devices[0].serial, SERIAL_1);
    assert!(s.hold.is_none() && !s.pending);
}

#[test]
fn needles_hold_names_slugs_serials_people_and_texts() {
    let fx = fixture();
    let mut needles = Needles::new();
    let mut scrub = Scrub::default();
    collect_needles(&fx.data(), &[SLUG.to_string()], &mut needles, &mut scrub).unwrap();
    let probe = |s: &str| {
        notedown_cloud::diag::check::check_json(&needles, "f", &json!({"x": s}), false).is_some()
    };
    for s in [
        "Secret plans.note",
        "Trips",
        "Meeting 3.5 Smith",
        "Old diary.note",
        SLUG,
        EMAIL,
        "privat",
        SERIAL_2,
        "Jordis Nomad",
        "my private digest text",
        "Hidden Book.epub",
        "ABCD-1234",
        "Call the doctor",
        "Private list",
        "Digest group name",
    ] {
        assert!(probe(s), "{s} is no needle");
    }
    assert!(!probe("NOTE") && !probe("needsAction"));
    assert!(scrub.slugs.contains(&SLUG.to_string()));
    assert!(scrub.serials.contains(&SERIAL_1.to_string()));
    assert!(scrub.names.contains(&"Secret plans.note".to_string()));
}

#[test]
fn the_dry_run_scan_reports_drift_and_writes_nothing() {
    let fx = fixture();
    let acc = fx.account();
    write(&acc.join("files/NOTE/Note/new by hand.note"), b"new");
    let before: Vec<_> = ["ids.json", "ids.jsonl", "hashes.json"]
        .iter()
        .map(|n| std::fs::read(acc.join(n)).ok())
        .collect();
    let ScanResult::Done(report) = dry_run_scan(&fx.data(), SLUG, 0, Duration::from_secs(1)) else {
        panic!("scan skipped");
    };
    assert_eq!(report.created.len(), 1);
    let after: Vec<_> = ["ids.json", "ids.jsonl", "hashes.json"]
        .iter()
        .map(|n| std::fs::read(acc.join(n)).ok())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn a_busy_store_skips_the_scan_after_the_wait() {
    let fx = fixture();
    let _held = take_store_lock(&fx.account(), None).unwrap();
    let started = std::time::Instant::now();
    let got = dry_run_scan(&fx.data(), SLUG, 0, Duration::from_millis(300));
    assert!(
        matches!(&got, ScanResult::Skipped(why) if why.contains("busy")),
        "{got:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_pending_change_skips_the_scan() {
    let fx = fixture();
    write(&fx.account().join(PENDING_FILE), b"{}");
    let got = dry_run_scan(&fx.data(), SLUG, 0, Duration::from_millis(300));
    assert!(
        matches!(&got, ScanResult::Skipped(why) if why.contains("unfinished")),
        "{got:?}"
    );
    assert!(account_state(&fx.data(), SLUG).unwrap().pending);
}

fn opts(fx: &Fixture, out: &str) -> DiagnoseOptions {
    DiagnoseOptions {
        data: fx.data(),
        since: OffsetDateTime::now_utc() - Duration::from_secs(3 * 24 * 3600),
        device: None,
        account: None,
        private: false,
        out: fx.dir.path().join(out),
        lock_wait: Duration::from_millis(500),
        test_keep_key: None,
    }
}

/// The files of a zip, by name.
pub fn unzip(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut bytes).unwrap();
        out.insert(f.name().to_string(), bytes);
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn a_safe_bundle_holds_none_of_the_real_values() {
    let fx = fixture();
    let done = diagnose(&opts(&fx, "b.zip")).unwrap();
    let files = unzip(&done.zip);
    let names: Vec<_> = files.keys().cloned().collect();
    assert_eq!(
        names,
        [
            "about.json",
            "accounts/acct-1.json",
            "journal.jsonl",
            "protocol.jsonl",
            "report.txt",
            "scan.json",
            "server.toml"
        ]
    );
    let data_dir = fx.data().to_string_lossy().into_owned();
    let mut secrets: Vec<&str> = SECRETS.to_vec();
    secrets.push(&data_dir);
    for (name, bytes) in &files {
        for s in &secrets {
            assert!(
                !contains(bytes, s),
                "{s:?} in {name}:\n{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }
    assert_eq!(files["report.txt"], done.report.as_bytes());
    // what the bundle is for: the facts are there
    let protocol = String::from_utf8_lossy(&files["protocol.jsonl"]).into_owned();
    assert!(protocol.contains("/api/file/3/files/upload/apply"));
    assert!(protocol.contains("DOWNLOADFILE"));
    assert!(protocol.contains("ip-1:19072"));
    assert!(protocol.contains("dev-1") && protocol.contains("dev-2"));
    assert!(protocol.contains(".note"));
    // heartbeats and lines outside the window are left out
    assert!(!protocol.contains("\"request_body\":\"2\""));
    assert!(!protocol.contains("/api/old"));
    let journal = String::from_utf8_lossy(&files["journal.jsonl"]).into_owned();
    assert!(journal.contains("file.created"), "{journal}");
    let report = &done.report;
    assert!(report.contains("unknown_endpoint"), "{report}");
    assert!(report.contains("E0079"), "{report}");
    assert!(report.contains("recovery_failed"), "{report}");
    assert!(report.contains("unreadable lines   1"), "{report}");
    assert!(report.contains("heartbeats 2"), "{report}");
    let server = String::from_utf8_lossy(&files["server.toml"]).into_owned();
    assert!(
        server.contains("<dir>") && server.contains("ip-1:19072"),
        "{server}"
    );
    let about: Value = serde_json::from_slice(&files["about.json"]).unwrap();
    assert_eq!(about["private"], false);
}

#[test]
fn journal_events_in_the_window_are_filtered() {
    let fx = fixture();
    let done = diagnose(&opts(&fx, "b.zip")).unwrap();
    let files = unzip(&done.zip);
    let journal = String::from_utf8_lossy(&files["journal.jsonl"]).into_owned();
    assert!(journal.contains("\"account\":\"acct-1\""), "{journal}");
    assert!(journal.contains("file.created") || journal.contains("folder.created"));
    for s in SECRETS {
        assert!(!journal.contains(s), "{s} in {journal}");
    }
    assert!(done.report.contains("Journal  "), "{}", done.report);
}

#[test]
fn a_planted_gap_stops_the_bundle() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.test_keep_key = Some("originalName".into());
    let err = diagnose(&o).unwrap_err().to_string();
    assert!(
        err.contains("would show a real name in protocol.jsonl"),
        "{err}"
    );
    assert!(!err.contains("Secret"), "{err}");
    let left: Vec<_> = std::fs::read_dir(fx.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["data"]);
}

#[test]
fn a_private_bundle_keeps_names_and_masks_secrets() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.private = true;
    let done = diagnose(&o).unwrap();
    let files = unzip(&done.zip);
    assert!(contains(&files["protocol.jsonl"], "Secret plans.note"));
    assert!(contains(&files["protocol.jsonl"], SERIAL_1));
    assert!(contains(&files["protocol.jsonl"], "<masked:199 chars>"));
    assert!(files.contains_key(&format!("accounts/{SLUG}.json")));
    assert!(
        done.report
            .starts_with("private bundle: holds real names, do not share\n")
    );
    let about: Value = serde_json::from_slice(&files["about.json"]).unwrap();
    assert_eq!(about["private"], true);
}

#[test]
fn an_empty_window_still_writes_a_bundle() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.since = OffsetDateTime::now_utc();
    let done = diagnose(&o).unwrap();
    assert!(
        done.report.contains("no log lines in the window"),
        "{}",
        done.report
    );
    assert!(done.zip.is_file());
}

#[test]
fn an_existing_out_file_stops_before_any_work() {
    let fx = fixture();
    let o = opts(&fx, "b.zip");
    std::fs::write(&o.out, b"keep").unwrap();
    let err = diagnose(&o).unwrap_err().to_string();
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(std::fs::read(&o.out).unwrap(), b"keep");
}

#[test]
fn an_unknown_account_or_device_stops() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.account = Some("nobody".into());
    assert!(
        diagnose(&o)
            .unwrap_err()
            .to_string()
            .contains("no account nobody")
    );
    let mut o = opts(&fx, "b.zip");
    o.device = Some("SN999".into());
    assert!(
        diagnose(&o)
            .unwrap_err()
            .to_string()
            .contains("device SN999")
    );
    assert!(!o.out.exists());
}

#[test]
fn a_missing_data_folder_stops() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.data = fx.dir.path().join("none");
    let err = diagnose(&o).unwrap_err().to_string();
    assert!(err.contains("no notedown data folder"), "{err}");
}

#[test]
fn a_device_filter_keeps_only_its_lines() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.device = Some(SERIAL_2.into());
    let done = diagnose(&o).unwrap();
    let files = unzip(&done.zip);
    let protocol = String::from_utf8_lossy(&files["protocol.jsonl"]).into_owned();
    assert_eq!(protocol.lines().count(), 1, "{protocol}");
    assert!(protocol.contains("E0079"));
    let about: Value = serde_json::from_slice(&files["about.json"]).unwrap();
    assert_eq!(about["filters"]["device"], "dev-1");
}

#[test]
fn an_account_filter_keeps_its_devices_and_neutral_markers() {
    let fx = fixture();
    let mut o = opts(&fx, "b.zip");
    o.account = Some(SLUG.into());
    let done = diagnose(&o).unwrap();
    let protocol = String::from_utf8_lossy(&unzip(&done.zip)["protocol.jsonl"]).into_owned();
    assert!(protocol.contains("recovery_failed"), "{protocol}");
    assert!(protocol.contains("dev-2"), "{protocol}");
}
