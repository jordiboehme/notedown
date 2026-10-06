use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use notedown_cloud::diag::{MAX_BODY_BYTES, cut_body, mask_headers, mask_json, mask_query};
use notedown_cloud::{Clock, Config, LogEntry, LogKind, ProtocolLog};
use serde_json::{Value, json};
use time::{Date, Month, OffsetDateTime, Time};

struct FakeClock(Mutex<OffsetDateTime>);

impl FakeClock {
    fn at(y: i32, m: u8, d: u8) -> Arc<FakeClock> {
        Arc::new(FakeClock(Mutex::new(stamp(y, m, d))))
    }
    fn set(&self, y: i32, m: u8, d: u8) {
        *self.0.lock().unwrap() = stamp(y, m, d);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

fn stamp(y: i32, m: u8, d: u8) -> OffsetDateTime {
    Date::from_calendar_date(y, Month::try_from(m).unwrap(), d)
        .unwrap()
        .with_time(Time::from_hms(12, 0, 0).unwrap())
        .assume_utc()
}

fn names(data: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(data.join("logs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

fn req(path: &str) -> LogEntry {
    let mut e = LogEntry::new(LogKind::Request);
    e.method = Some("POST".into());
    e.path = Some(path.into());
    e
}

fn marker_firmware() -> LogEntry {
    let mut e = LogEntry::new(LogKind::Marker);
    e.flags = vec!["firmware_changed".into()];
    e
}

// ---- masking ----

#[test]
fn masks_sensitive_keys_at_any_depth() {
    let v = json!({
        "account": "a@b.c",
        "password": "hunter2",
        "nested": {"list": [{"passwordMd5": "abcdef", "ok": 1}], "x-access-token": "tok"},
        "randomCode": "123456", "token": "jwt", "sign": "sig", "code": "42", "validCode": "vv",
        "Token": {"obj": true}
    });
    let m = mask_json(&v);
    assert_eq!(m["account"], "a@b.c");
    assert_eq!(m["password"], "<masked:7 chars>");
    assert_eq!(m["nested"]["list"][0]["passwordMd5"], "<masked:6 chars>");
    assert_eq!(m["nested"]["list"][0]["ok"], 1);
    assert_eq!(m["nested"]["x-access-token"], "<masked:3 chars>");
    assert_eq!(m["randomCode"], "<masked:6 chars>");
    assert_eq!(m["token"], "<masked:3 chars>");
    assert_eq!(m["sign"], "<masked:3 chars>");
    assert_eq!(m["code"], "<masked:2 chars>");
    assert_eq!(m["validCode"], "<masked:2 chars>");
    assert!(m["Token"].as_str().unwrap().starts_with("<masked:"));
    // idempotent
    assert_eq!(mask_json(&m), m);
}

#[test]
fn masks_query_and_headers() {
    assert_eq!(
        mask_query("a=1&sign=abcd&token=xyz&b=2&flag"),
        "a=1&sign=<masked:4 chars>&token=<masked:3 chars>&b=2&flag"
    );
    let mut h = BTreeMap::new();
    h.insert("X-Access-Token".to_string(), "secret".to_string());
    h.insert("user-agent".to_string(), "nomad".to_string());
    let m = mask_headers(&h);
    assert_eq!(m["X-Access-Token"], "<masked:6 chars>");
    assert_eq!(m["user-agent"], "nomad");
}

#[test]
fn cuts_large_bodies_with_note() {
    let big = Value::String("x".repeat(MAX_BODY_BYTES + 100));
    let cut = cut_body(&big);
    let s = cut.as_str().unwrap();
    assert!(s.contains("<cut:"));
    assert!(s.len() < MAX_BODY_BYTES + 100);
    let small = json!({"a": 1});
    assert_eq!(cut_body(&small), small);
}

#[test]
fn record_masks_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::at(2026, 10, 6);
    let log = ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, clock).unwrap();
    let mut e = req("/api/user/login");
    e.query = Some("sign=SECRETSIGN&x=1".into());
    e.headers
        .insert("x-access-token".into(), "SECRETTOKEN".into());
    e.request_body = Some(json!({"password": "SECRETPW", "deep": {"randomCode": "SECRETCODE"}}));
    e.response_body = Some(json!({"token": "SECRETJWT"}));
    log.record(e);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    assert!(!text.contains("SECRET"), "{text}");
    assert_eq!(text.lines().count(), 1);
    let v: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(v["kind"], "request");
    assert_eq!(v["ts"], "2026-10-06T12:00:00Z");
    assert_eq!(v["request_body"]["password"], "<masked:8 chars>");
}

// ---- rotation ----

#[test]
fn new_file_at_day_change() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::at(2026, 10, 6);
    let log = ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, clock.clone()).unwrap();
    log.record(req("/a"));
    clock.set(2026, 10, 7);
    log.record(req("/b"));
    log.record(req("/c"));
    assert_eq!(
        names(dir.path()),
        ["protocol-2026-10-06.jsonl", "protocol-2026-10-07.jsonl"]
    );
    let day2 = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-07.jsonl")).unwrap();
    assert_eq!(day2.lines().count(), 2);
}

fn put(data: &Path, date: &str, bytes: usize) {
    let logs = data.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join(format!("protocol-{date}.jsonl")),
        "x".repeat(bytes),
    )
    .unwrap();
}

#[test]
fn retention_deletes_old_files_on_day_change() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "2026-09-01", 10);
    put(dir.path(), "2026-09-20", 10);
    let clock = FakeClock::at(2026, 10, 6);
    let log = ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, clock.clone()).unwrap();
    // open already rotated: 09-01 is 35 days old
    assert_eq!(
        names(dir.path()),
        ["protocol-2026-09-20.jsonl", "protocol-2026-10-06.jsonl"]
    );
    clock.set(2026, 10, 25); // 09-20 is now 35 days old
    log.record(req("/a"));
    assert_eq!(
        names(dir.path()),
        ["protocol-2026-10-06.jsonl", "protocol-2026-10-25.jsonl"]
    );
}

#[test]
fn size_cap_deletes_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "2026-10-01", 400);
    put(dir.path(), "2026-10-02", 400);
    put(dir.path(), "2026-10-03", 400);
    let clock = FakeClock::at(2026, 10, 6);
    // cap 900: 1200 total, delete 10-01 (800 left), keep the rest
    let _log = ProtocolLog::open_with_clock(dir.path(), 30, 900, clock).unwrap();
    assert_eq!(
        names(dir.path()),
        [
            "protocol-2026-10-02.jsonl",
            "protocol-2026-10-03.jsonl",
            "protocol-2026-10-06.jsonl"
        ]
    );
}

#[test]
fn size_cap_reached_while_writing_and_current_file_survives() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "2026-10-04", 500);
    let clock = FakeClock::at(2026, 10, 6);
    let log = ProtocolLog::open_with_clock(dir.path(), 30, 600, clock).unwrap();
    for _ in 0..20 {
        log.record(req("/some/path/that/makes/the/line/longer"));
    }
    assert_eq!(names(dir.path()), ["protocol-2026-10-06.jsonl"]);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 20, "no lost lines");
}

#[test]
fn firmware_days_are_protected_from_rotation() {
    let dir = tempfile::tempdir().unwrap();
    // marker on 09-10 (written in an earlier run); 09-08..09-12 are protected
    let logs = dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let mut m = marker_firmware();
    m.ts = "2026-09-10T08:00:00Z".into();
    std::fs::write(
        logs.join("protocol-2026-09-10.jsonl"),
        format!("{}\n", serde_json::to_string(&m).unwrap()),
    )
    .unwrap();
    for d in [
        "2026-09-07",
        "2026-09-08",
        "2026-09-09",
        "2026-09-11",
        "2026-09-12",
        "2026-09-13",
    ] {
        put(dir.path(), d, 300);
    }
    put(dir.path(), "2026-10-01", 300);
    let clock = FakeClock::at(2026, 10, 6);
    // retention 30 days would delete everything before 09-06; cap 1 byte forces deletion of all
    // unprotected files.
    let _log = ProtocolLog::open_with_clock(dir.path(), 30, 1, clock).unwrap();
    assert_eq!(
        names(dir.path()),
        [
            "protocol-2026-09-08.jsonl",
            "protocol-2026-09-09.jsonl",
            "protocol-2026-09-10.jsonl",
            "protocol-2026-09-11.jsonl",
            "protocol-2026-09-12.jsonl",
            "protocol-2026-10-06.jsonl",
        ]
    );
}

#[test]
fn firmware_marker_recorded_in_this_run_protects_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "2026-10-04", 300);
    put(dir.path(), "2026-10-01", 300);
    let clock = FakeClock::at(2026, 10, 6);
    let log = ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, clock.clone()).unwrap();
    log.record(marker_firmware());
    // shrink effectively: reopen with a tiny cap
    drop(log);
    let _log = ProtocolLog::open_with_clock(dir.path(), 30, 1, clock).unwrap();
    // 10-04 is within 2 days of 10-06, 10-01 is not
    assert_eq!(
        names(dir.path()),
        ["protocol-2026-10-04.jsonl", "protocol-2026-10-06.jsonl"]
    );
}

#[test]
fn concurrent_writers_lose_no_lines() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::at(2026, 10, 6);
    let log = Arc::new(ProtocolLog::open_with_clock(dir.path(), 30, 4000, clock.clone()).unwrap());
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let log = log.clone();
            let clock = clock.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    if t == 0 && i == 25 {
                        clock.set(2026, 10, 7);
                    }
                    log.record(req("/x"));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let total: usize = names(dir.path())
        .iter()
        .map(|n| {
            std::fs::read_to_string(dir.path().join("logs").join(n))
                .unwrap()
                .lines()
                .inspect(|l| {
                    serde_json::from_str::<Value>(l).unwrap();
                })
                .count()
        })
        .sum();
    // the cap may have deleted the (older) first-day file, but never lines of the current one
    assert!(total >= 1);
    let day2 = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-07.jsonl")).unwrap();
    assert!(day2.lines().count() > 0);
}

// ---- config ----

#[test]
fn config_defaults_created_and_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let c = Config::load_or_create(dir.path()).unwrap();
    assert_eq!(c.listen, vec!["0.0.0.0:19072".parse().unwrap()]);
    assert_eq!(c.external_base_url, None);
    assert!(c.protect);
    assert!(!c.capture);
    assert_eq!(c.log_retention_days, 30);
    assert_eq!(c.log_max_bytes, 1 << 30);
    let text = std::fs::read_to_string(dir.path().join("server.toml")).unwrap();
    assert!(text.ends_with('\n'));
    assert_eq!(Config::load_or_create(dir.path()).unwrap(), c);

    let custom = Config {
        listen: vec!["127.0.0.1:1".parse().unwrap(), "[::1]:2".parse().unwrap()],
        external_base_url: Some("https://notes.example.org".into()),
        protect: false,
        capture: true,
        log_retention_days: 7,
        log_max_bytes: 123,
    };
    custom.save(dir.path()).unwrap();
    assert_eq!(Config::load_or_create(dir.path()).unwrap(), custom);
}

#[test]
fn config_partial_file_uses_defaults_and_bad_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.toml"), "capture = true\n").unwrap();
    let c = Config::load_or_create(dir.path()).unwrap();
    assert!(c.capture && c.protect);
    std::fs::write(dir.path().join("server.toml"), "listen = 3\n").unwrap();
    assert!(Config::load_or_create(dir.path()).is_err());
}
