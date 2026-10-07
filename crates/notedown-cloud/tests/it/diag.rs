use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use notedown_cloud::diag::{MAX_BODY_BYTES, Origin, cut_body, mask_headers, mask_json, mask_query};
use notedown_cloud::{Clock, Config, LogEntry, LogKind, ProtocolLog, PushMode, SyncMode};
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

#[test]
fn code_is_masked_only_in_device_data() {
    use notedown_cloud::diag::{Origin, mask_json_from, mask_text_from};
    // Server data: result codes stay readable, every other secret stays masked.
    let resp = mask_json_from(
        &json!({"code": "403", "errorCode": "E0019", "randomCode": "abc", "token": "t",
                "inner": "{\"code\":\"200\",\"password\":\"pw\"}"}),
        Origin::Server,
    );
    assert_eq!(resp["code"], "403");
    assert_eq!(resp["errorCode"], "E0019");
    assert_eq!(resp["randomCode"], "<masked:3 chars>");
    assert_eq!(resp["token"], "<masked:1 chars>");
    let inner = resp["inner"].as_str().unwrap();
    assert!(inner.contains(r#""code":"200""#), "{inner}");
    assert!(!inner.contains(r#""pw""#), "{inner}");
    assert_eq!(
        mask_text_from("code=403&token=abc", Origin::Server),
        "code=403&token=<masked:3 chars>"
    );
    // Device data: `code` is a login code.
    assert_eq!(
        mask_json_from(&json!({"code": "123456"}), Origin::Client)["code"],
        "<masked:6 chars>"
    );
    assert_eq!(mask_json(&json!({"code": "1"}))["code"], "<masked:1 chars>");

    // Through the log: request body and query mask `code`, the response keeps it.
    let dir = tempfile::tempdir().unwrap();
    let log =
        ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, FakeClock::at(2026, 10, 6)).unwrap();
    let mut e = req("/api/x");
    e.query = Some("code=QCODE".into());
    e.request_body = Some(json!({"code": "BODYCODE"}));
    e.response_body = Some(json!({"code": "403", "errorCode": "E0019", "token": "SECRETJWT"}));
    log.record(e);
    let mut f = LogEntry::new(LogKind::Frame);
    f.request_body = Some(Value::String(
        r#"42["ClientMessage","{\"code\":\"FRAMECODE\"}"]"#.into(),
    ));
    log.record(f);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    for secret in ["QCODE", "BODYCODE", "SECRETJWT", "FRAMECODE"] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    let v: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(v["response_body"]["code"], "403");
    assert_eq!(v["response_body"]["errorCode"], "E0019");
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
fn concurrent_writers_lose_no_lines_across_a_day_switch() {
    use std::sync::Barrier;
    const THREADS: usize = 8;
    const PER_PHASE: usize = 25;
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::at(2026, 10, 6);
    let log =
        Arc::new(ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, clock.clone()).unwrap());
    let barrier = Arc::new(Barrier::new(THREADS + 1));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (log, barrier) = (log.clone(), barrier.clone());
            std::thread::spawn(move || {
                for _ in 0..PER_PHASE {
                    log.record(req("/x"));
                }
                barrier.wait(); // phase 1 done
                barrier.wait(); // day switched
                for _ in 0..PER_PHASE {
                    log.record(req("/y"));
                }
            })
        })
        .collect();
    barrier.wait();
    clock.set(2026, 10, 7);
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    let count = |n: &str| {
        std::fs::read_to_string(dir.path().join("logs").join(n))
            .unwrap()
            .lines()
            .inspect(|l| {
                serde_json::from_str::<Value>(l).unwrap();
            })
            .count()
    };
    assert_eq!(count("protocol-2026-10-06.jsonl"), THREADS * PER_PHASE);
    assert_eq!(count("protocol-2026-10-07.jsonl"), THREADS * PER_PHASE);
    assert_eq!(log.dropped_lines(), 0);
}

#[test]
fn clock_going_back_keeps_the_current_file() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::at(2026, 10, 7);
    let log = ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, clock.clone()).unwrap();
    log.record(req("/a"));
    clock.set(2026, 10, 6);
    log.record(req("/b"));
    assert_eq!(names(dir.path()), ["protocol-2026-10-07.jsonl"]);
    let t = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-07.jsonl")).unwrap();
    assert_eq!(t.lines().count(), 2);
}

#[test]
fn all_headers_logged_secret_ones_masked_repeats_joined() {
    let dir = tempfile::tempdir().unwrap();
    let log =
        ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, FakeClock::at(2026, 10, 6)).unwrap();
    let mut e = req("/p?sign=SECRETSIGN&a=1");
    e.add_header("User-Agent", "nomad");
    e.add_header("Accept", "a/b");
    e.add_header("accept", "c/d");
    e.add_header("Authorization", "Bearer SECRETAUTH");
    e.add_header("Proxy-Authorization", "SECRETPROXY");
    e.add_header("Cookie", "s=SECRETCOOKIE");
    e.add_header("Set-Cookie", "s=SECRETSETCOOKIE");
    e.add_header("X-Access-Token", "SECRETTOKEN");
    e.add_header("X-Api-Secret", "SECRETX");
    e.add_header("X-Custom-Whatever", "visible");
    e.add_header("x-long", &"y".repeat(10_000));
    e.query = Some("password=SECRETPW&Pass%77ord=SECRETPW2&flag".into());
    log.record(e);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    assert!(!text.contains("SECRET"), "{text}");
    let v: Value = serde_json::from_str(text.trim()).unwrap();
    let h = &v["headers"];
    assert_eq!(h["x-custom-whatever"], "visible");
    assert_eq!(h["accept"], "a/b, c/d");
    assert!(h["authorization"].as_str().unwrap().starts_with("<masked:"));
    assert!(h["x-long"].as_str().unwrap().contains("<cut:"));
    assert_eq!(v["path"], "/p?sign=<masked:10 chars>&a=1");
    assert_eq!(
        v["query"],
        "password=<masked:8 chars>&Pass%77ord=<masked:9 chars>&flag"
    );
}

#[test]
fn long_path_and_query_are_cut() {
    let dir = tempfile::tempdir().unwrap();
    let log =
        ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, FakeClock::at(2026, 10, 6)).unwrap();
    let mut e = req(&format!("/{}", "p".repeat(10_000)));
    e.query = Some("q".repeat(10_000));
    log.record(e);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    assert!(text.len() < 10_000);
    assert!(text.contains("<cut:"));
}

#[test]
fn embedded_secrets_in_frames_and_form_bodies_are_masked() {
    let frame = Value::String(r#"42["ClientMessage","{\"token\":\"abc\"}"]"#.into());
    let m = mask_json(&frame);
    assert_eq!(
        m,
        Value::String(r#"42["ClientMessage","{\"token\":\"<masked:3 chars>\"}"]"#.into())
    );
    // JSON inside a JSON string field
    let nested = json!({"msg": "{\"a\":{\"password\":\"pw\"}}", "keep": "plain"});
    let m = mask_json(&nested);
    assert!(!m.to_string().contains("\\\"pw\\\""));
    assert_eq!(m["keep"], "plain");

    let dir = tempfile::tempdir().unwrap();
    let log =
        ProtocolLog::open_with_clock(dir.path(), 30, 1 << 30, FakeClock::at(2026, 10, 6)).unwrap();
    let mut e = LogEntry::new(LogKind::Frame);
    e.request_body = Some(Value::String("password=x&randomCode=yy&account=me".into()));
    e.response_body = Some(Value::String(
        r#"42["ClientMessage","{\"token\":\"abc\"}"]"#.into(),
    ));
    log.record(e);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    let v: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        v["request_body"],
        "password=<masked:1 chars>&randomCode=<masked:2 chars>&account=me"
    );
    assert!(!text.contains("abc"));
}

#[test]
fn key_rule_matches_names_containing_secret_parts() {
    let m = mask_json(&json!({
        "userToken": "a", "client_secret": "b", "Authorization": "c", "setCookie": "d",
        "signature": "e", "newPassword": "f", "validCode": "g", "code": "h",
        "errorCode": "E0019", "codes": "keep"
    }));
    for k in [
        "userToken",
        "client_secret",
        "Authorization",
        "setCookie",
        "signature",
        "newPassword",
        "validCode",
        "code",
    ] {
        assert!(m[k].as_str().unwrap().starts_with("<masked:"), "{k}");
    }
    assert_eq!(m["errorCode"], "E0019");
    assert_eq!(m["codes"], "keep");
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
    assert_eq!(c.sync, SyncMode::Accept);
    assert_eq!(c.push, PushMode::Reconnect);
    assert_eq!(c.log_retention_days, 30);
    assert_eq!(c.log_max_bytes, 1 << 30);
    let text = std::fs::read_to_string(dir.path().join("server.toml")).unwrap();
    assert!(text.ends_with('\n'));
    assert!(
        text.contains("# For one capture session only.") && text.contains("\ncapture = false\n"),
        "{text}"
    );
    let comment = text.find("# For one capture session only.").unwrap();
    assert!(comment < text.find("\ncapture = ").unwrap(), "{text}");
    assert_eq!(Config::load_or_create(dir.path()).unwrap(), c);

    let custom = Config {
        listen: vec!["127.0.0.1:1".parse().unwrap(), "[::1]:2".parse().unwrap()],
        external_base_url: Some("https://notes.example.org".into()),
        trusted_proxies: vec!["10.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
        protect: false,
        sync: SyncMode::Refuse,
        push: PushMode::Off,
        capture: true,
        log_retention_days: 7,
        log_max_bytes: 123,
    };
    custom.save(dir.path()).unwrap();
    assert_eq!(Config::load_or_create(dir.path()).unwrap(), custom);
    let text = std::fs::read_to_string(dir.path().join("server.toml")).unwrap();
    assert!(text.contains("\nsync = \"refuse\"\n"), "{text}");
    assert!(text.contains("\npush = \"off\"\n"), "{text}");
}

#[test]
fn config_push_key_defaults_to_reconnect_and_parses() {
    let dir = tempfile::tempdir().unwrap();
    let c = Config::load_or_create(dir.path()).unwrap();
    assert_eq!(c.push, PushMode::Reconnect);
    let text = std::fs::read_to_string(dir.path().join("server.toml")).unwrap();
    assert!(text.contains("\npush = \"reconnect\"\n"), "{text}");
    assert!(
        text.find("\nsync = ").unwrap() < text.find("\npush = ").unwrap(),
        "{text}"
    );

    // An older file without the key loads as reconnect and is not rewritten.
    let path = dir.path().join("server.toml");
    let old = "listen = [\"127.0.0.1:0\"]\nsync = \"accept\"\n";
    std::fs::write(&path, old).unwrap();
    assert_eq!(
        Config::load_or_create(dir.path()).unwrap().push,
        PushMode::Reconnect
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old);

    std::fs::write(&path, "push = \"startsync\"\n").unwrap();
    assert_eq!(
        Config::load_or_create(dir.path()).unwrap().push,
        PushMode::Startsync
    );
    std::fs::write(&path, old).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old);

    std::fs::write(&path, "push = \"off\"\n").unwrap();
    assert_eq!(
        Config::load_or_create(dir.path()).unwrap().push,
        PushMode::Off
    );
    std::fs::write(&path, "push = \"detail\"\n").unwrap();
    assert_eq!(
        Config::load_or_create(dir.path()).unwrap().push,
        PushMode::Detail
    );
    std::fs::write(&path, "push = \"sometimes\"\n").unwrap();
    assert!(Config::load_or_create(dir.path()).is_err());
}

#[test]
fn config_sync_key_parses_and_rejects_other_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::write(&path, "sync = \"refuse\"\n").unwrap();
    assert_eq!(
        Config::load_or_create(dir.path()).unwrap().sync,
        SyncMode::Refuse
    );
    std::fs::write(&path, "sync = \"accept\"\n").unwrap();
    assert_eq!(
        Config::load_or_create(dir.path()).unwrap().sync,
        SyncMode::Accept
    );
    std::fs::write(&path, "sync = \"maybe\"\n").unwrap();
    assert!(Config::load_or_create(dir.path()).is_err());
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

#[test]
fn channel_header_and_jwt_shaped_header_values_are_masked() {
    use notedown_cloud::diag::{contains_jwt, mask_headers};
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VySWQiOjF9.c2lnbmF0dXJl";
    assert!(contains_jwt(jwt));
    assert!(contains_jwt(&format!("{jwt}_SN078C10000000_1791270000000")));
    assert!(!contains_jwt("eyJ only"));
    assert!(!contains_jwt("application/json"));
    let mut h = std::collections::BTreeMap::new();
    h.insert("channel".to_string(), String::new());
    h.insert("x-custom".to_string(), format!("Bearer {jwt}"));
    h.insert("user-agent".to_string(), "okhttp/3.12".to_string());
    let m = mask_headers(&h);
    assert_eq!(m["channel"], "<masked:0 chars>");
    assert!(m["x-custom"].starts_with("<masked:"));
    assert_eq!(m["user-agent"], "okhttp/3.12");
}

#[test]
fn text_that_is_neither_json_nor_form_is_masked_by_substring() {
    use notedown_cloud::diag::mask_text;
    let md5 = "5ebe2294ecd0e0f08eab7690d2a6ee69";
    // cut JSON
    let cut = format!(r#"{{"account":"a","password":"{md5}","equipmentNo":"X"#);
    let m = mask_text(&cut);
    assert!(!m.contains(md5), "{m}");
    assert!(m.contains(r#""password":"<masked:32 chars>""#), "{m}");
    assert!(m.contains(r#""account":"a""#), "{m}");
    // value cut before its closing quote
    let m = mask_text(r#"{"account":"a","token":"eyJabc.def"#);
    assert!(!m.contains("eyJabc"), "{m}");
    // a form with a bare flag, and other spellings of the listed keys
    let m = mask_text("password=hunter2&flag");
    assert_eq!(m, "password=<masked:7 chars>&flag");
    for (text, secret) in [
        ("passwordMd5: abc123 rest", "abc123"),
        ("x-access-token=tok123;", "tok123"),
        ("'randomCode': 'Ab12Cd34'", "Ab12Cd34"),
        ("sign=s1g; code=9876 validCode=777", "9876"),
        ("validCode=777 tail", "777"),
        (r#"42["ClientMessage","{\"token\":\"abc"#, "abc"),
        (r#"{"password":"a\"b secret"#, "secret"),
        ("CODE=4321", "4321"),
    ] {
        let m = mask_text(text);
        assert!(!m.contains(secret), "{text} -> {m}");
        assert!(m.contains("<masked:"), "{text} -> {m}");
    }
    // harmless text stays as it is; masking twice is stable
    assert_eq!(mask_text("errorCode=E0019 hello"), "errorCode=E0019 hello");
    assert_eq!(
        mask_text("plain words, no secrets"),
        "plain words, no secrets"
    );
    let once = mask_text(&cut);
    assert_eq!(mask_text(&once), once);
}

// ---- JWT-shaped values outside sensitive keys ----

const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VySWQiOjF9.c2lnbmF0dXJl";

fn one_line(dir: &Path) -> (String, Value) {
    let text = std::fs::read_to_string(dir.join("logs/protocol-2026-10-06.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    let v = serde_json::from_str(text.trim()).unwrap();
    (text, v)
}

fn open_log(dir: &Path) -> ProtocolLog {
    ProtocolLog::open_with_clock(dir, 30, 1 << 30, FakeClock::at(2026, 10, 6)).unwrap()
}

#[test]
fn jwt_shaped_values_are_masked_in_every_form() {
    use notedown_cloud::diag::{mask_form, mask_jwts, mask_text, mask_text_from};
    let n = JWT.len();
    let ph = format!("<masked:{n} chars>");
    assert_eq!(mask_jwts(&format!("a {JWT} b")), format!("a {ph} b"));
    // unknown key, nested
    let m = mask_json(&json!({"accessKey": JWT, "auth": JWT, "deep": [{"x": JWT}]}));
    assert_eq!(m, json!({"accessKey": ph, "auth": ph, "deep": [{"x": ph}]}));
    // a JWT as an object key
    let m = mask_json(&json!({ JWT: 1 }));
    assert!(!m.to_string().contains(JWT), "{m}");
    // keyless event argument, both directions
    let frame = format!(r#"42["auth","{JWT}"]"#);
    for origin in [Origin::Client, Origin::Server] {
        let m = mask_text_from(&frame, origin);
        assert_eq!(m, format!(r#"42["auth","{ph}"]"#));
    }
    // event argument holding embedded JSON with an unknown key
    let m = mask_text(&format!(r#"42["ClientMessage","{{\"k\":\"{JWT}\"}}"]"#));
    assert!(!m.contains(JWT), "{m}");
    // channel in a body: `<jwt>_<serial>_<ms>`
    let m = mask_json(&json!({"channel": format!("{JWT}_SN078C1_1791270000000")}));
    let ch = m["channel"].as_str().unwrap();
    assert!(!ch.contains("eyJ"), "{ch}");
    assert!(ch.starts_with("<masked:"), "{ch}");
    // query values, plain and percent-encoded
    let q = mask_query(&format!("t={JWT}&channel={JWT}_SN1_123&EIO=3"));
    assert!(!q.contains("eyJ"), "{q}");
    assert!(q.starts_with(&format!("t={ph}&channel=<masked:")), "{q}");
    assert!(q.ends_with("&EIO=3"), "{q}");
    let enc = JWT.replace('.', "%2E");
    let q = mask_form(&format!("v={enc}&x=1"));
    assert_eq!(q, format!("v=<masked:{} chars>&x=1", enc.len()));
    // free text and a form body
    assert_eq!(
        mask_text(&format!("Bearer {JWT} tail")),
        format!("Bearer {ph} tail")
    );
    assert_eq!(
        mask_text(&format!("a=1&blob={JWT}")),
        format!("a=1&blob={ph}")
    );
    // harmless dots stay; masking twice is stable
    assert_eq!(mask_jwts("v1.2.3 eyJ.x"), "v1.2.3 eyJ.x");
    let once = mask_text(&frame);
    assert_eq!(mask_text(&once), once);
}

#[test]
fn jwt_shaped_values_never_reach_the_log_or_capture() {
    let dir = tempfile::tempdir().unwrap();
    let capture = notedown_cloud::diag::Capture::open(dir.path()).unwrap();
    let log = open_log(dir.path()).with_capture(capture);
    let mut e = req(&format!("/socket.io/{JWT}/?t={JWT}"));
    e.query = Some(format!("t={JWT}&channel={JWT}_SN1_123"));
    e.request_body = Some(json!({"accessKey": JWT, "channel": format!("{JWT}_SN1_1")}));
    e.request_body_raw = Some(format!(
        r#"{{"channel":"{JWT}_SN1_1","accessKey":"{JWT}"}}"#
    ));
    e.response_body = Some(json!({"data": JWT}));
    e.response_body_raw = Some(format!(r#"{{"data":"{JWT}"}}"#));
    log.record(e);
    let mut f = LogEntry::new(LogKind::Frame);
    f.direction = Some("out".into());
    f.response_body = Some(Value::String(format!(r#"42["auth","{JWT}"]"#)));
    log.record(f);
    let text = std::fs::read_to_string(dir.path().join("logs/protocol-2026-10-06.jsonl")).unwrap();
    assert!(!text.contains("eyJ"), "{text}");
    assert_eq!(text.matches("<masked:").count(), 10, "{text}");
    for entry in std::fs::read_dir(dir.path().join("capture")).unwrap() {
        let dump = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!dump.contains("eyJ"), "{dump}");
    }
}

// ---- raw bodies ----

#[test]
fn raw_body_kept_masked_when_it_differs_from_the_parsed_one() {
    let dir = tempfile::tempdir().unwrap();
    let log = open_log(dir.path());
    let raw = r#"{"z":1.50,"password":"SECRETPW","code":123456,"a":"y"}"#;
    let mut e = req("/api/x");
    e.request_body = Some(serde_json::from_str(raw).unwrap());
    e.request_body_raw = Some(raw.into());
    log.record(e);
    let (text, v) = one_line(dir.path());
    assert!(
        !text.contains("SECRET") && !text.contains("123456"),
        "{text}"
    );
    assert_eq!(
        v["request_body_raw"],
        r#"{"z":1.50,"password":"<masked:8 chars>","code":"<masked:6 chars>","a":"y"}"#
    );
    assert_eq!(v["request_body"]["a"], "y");
}

#[test]
fn raw_body_left_out_when_equal_or_not_safely_maskable() {
    // Equal to the re-serialized form: no raw field.
    let dir = tempfile::tempdir().unwrap();
    let log = open_log(dir.path());
    let raw = r#"{"a":1,"password":"pw"}"#;
    let mut e = req("/api/x");
    e.request_body = Some(serde_json::from_str(raw).unwrap());
    e.request_body_raw = Some(raw.into());
    e.response_body = Some(json!({"errorCode": "E0079", "success": false}));
    e.response_body_raw = Some(r#"{"errorCode":"E0079","success":false}"#.into());
    log.record(e);
    let (_, v) = one_line(dir.path());
    assert!(v.get("request_body_raw").is_none(), "{v}");
    assert!(v.get("response_body_raw").is_none(), "{v}");
    // An object under a sensitive key cannot be masked by substring: the raw text is dropped,
    // the parsed body is masked as always.
    let dir = tempfile::tempdir().unwrap();
    let log = open_log(dir.path());
    let raw = r#"{"b":1, "token":{"inner":"SECRETVALUE"}}"#;
    let mut e = req("/api/x");
    e.request_body = Some(serde_json::from_str(raw).unwrap());
    e.request_body_raw = Some(raw.into());
    log.record(e);
    let (text, v) = one_line(dir.path());
    assert!(!text.contains("SECRET"), "{text}");
    assert!(v.get("request_body_raw").is_none(), "{v}");
}

#[test]
fn raw_body_is_cut_at_the_body_cap() {
    let dir = tempfile::tempdir().unwrap();
    let log = open_log(dir.path());
    let raw = format!(r#"{{"z":"{}","a":1}}"#, "x".repeat(MAX_BODY_BYTES));
    let mut e = req("/api/x");
    e.request_body = Some(serde_json::from_str(&raw).unwrap());
    e.request_body_raw = Some(raw);
    log.record(e);
    let (_, v) = one_line(dir.path());
    let r = v["request_body_raw"].as_str().unwrap();
    assert!(r.len() < MAX_BODY_BYTES + 100, "{}", r.len());
    assert!(r.contains("<cut: "), "cut note");
}

// ---- linear JWT scan, duplicate keys ----

#[test]
fn jwt_scan_is_linear_on_hostile_input() {
    use notedown_cloud::diag::{contains_jwt, mask_jwts, mask_text};
    use std::time::{Duration, Instant};
    let hostile = "eyJ".repeat((1 << 20) / 3);
    let started = Instant::now();
    assert!(!contains_jwt(&hostile));
    assert_eq!(mask_jwts(&hostile), hostile);
    let _ = mask_text(&hostile);
    let _ = mask_json(&json!({ "x": hostile }));
    // Other near misses: many dots, empty parts.
    let dots = "eyJa..".repeat((1 << 20) / 6);
    assert!(!contains_jwt(&dots));
    let empty3 = "eyJa.b.!".repeat((1 << 20) / 8);
    assert!(!contains_jwt(&empty3));
    let nodot = "eyJa.b!".repeat((1 << 20) / 7);
    assert!(!contains_jwt(&nodot));
    assert!(!contains_jwt(&empty3));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // Results stay the same, also behind failed candidates.
    let ph = format!("<masked:{} chars>", JWT.len());
    assert_eq!(mask_jwts(&format!("eyJeyJ {JWT}")), format!("eyJeyJ {ph}"));
    // Same results as the plain quadratic scan, on every short text over a small alphabet.
    let alphabet = ['e', 'y', 'J', 'a', '.', '!'];
    let mut texts = vec![String::new()];
    let mut level = vec![String::new()];
    for _ in 0..7 {
        level = level
            .iter()
            .flat_map(|t| alphabet.iter().map(move |c| format!("{t}{c}")))
            .collect();
        texts.extend(level.iter().cloned());
    }
    assert_eq!(texts.len(), (0..=7).map(|n| 6usize.pow(n)).sum::<usize>());
    for t in &texts {
        assert_eq!(mask_jwts(t), naive_mask_jwts(t), "{t}");
    }
}

/// The plain scan: try every `eyJ` in turn.
fn naive_mask_jwts(s: &str) -> String {
    let b = s.as_bytes();
    let b64 = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'=');
    let run = |mut j: usize| {
        while j < b.len() && b64(b[j]) {
            j += 1;
        }
        j
    };
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i + 3 <= b.len() {
        if i >= copied && &s[i..i + 3] == "eyJ" {
            let e1 = run(i);
            if b.get(e1) == Some(&b'.') {
                let e2 = run(e1 + 1);
                if e2 > e1 + 1 && b.get(e2) == Some(&b'.') {
                    let e3 = run(e2 + 1);
                    if e3 > e2 + 1 {
                        out.push_str(&s[copied..i]);
                        out.push_str(&format!("<masked:{} chars>", e3 - i));
                        copied = e3;
                        i = e3;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

/// `PASSW` becomes the JSON escape spelling of `password` (backslash, u0077 for the w), which the
/// substring masker cannot read.
fn esc(s: &str) -> String {
    s.replace("PASSW", &format!("pass{}u0077ord", '\\'))
}

#[test]
fn duplicate_keys_never_let_a_secret_through_raw_text() {
    use notedown_cloud::diag::{has_duplicate_keys, mask_raw_json_from, mask_text};
    assert!(has_duplicate_keys(
        r#"{"password":"hunter2","password":"x"}"#
    ));
    assert!(has_duplicate_keys(r#"[{"a":{"b":1,"b":2}}]"#));
    assert!(has_duplicate_keys(&esc(r#"{"PASSW":"a","password":"b"}"#)));
    assert!(has_duplicate_keys(
        r#"["m","{\"password\":\"a\",\"password\":\"b\"}"]"#
    ));
    assert!(!has_duplicate_keys(r#"{"a":1,"b":{"a":2}}"#));
    assert!(!has_duplicate_keys("not json"));
    // Body: the raw text is dropped.
    for raw in [
        r#"{"password":"hunter2","password":"x"}"#,
        r#"{"password":"hunter2","password":"<masked:3 chars>"}"#,
        r#"{"PASSW":"hunter2","password":"x"}"#,
        r#"{"a":{"PASSW":"hunter2"},"a":{}}"#,
    ] {
        let raw = esc(raw);
        assert_eq!(mask_raw_json_from(&raw, Origin::Client), None, "{raw}");
    }
    // Without duplicates, an escaped key name is caught by the structural check.
    assert_eq!(
        mask_raw_json_from(&esc(r#"{"PASSW":"hunter2"}"#), Origin::Client),
        None
    );
    // Through the log: no secret in the line.
    let dir = tempfile::tempdir().unwrap();
    let log = open_log(dir.path());
    let raw = esc(r#"{"z":1,"password":"<masked:3 chars>","PASSW":"hunter2"}"#);
    let mut e = req("/api/x");
    e.request_body = Some(serde_json::from_str(&raw).unwrap());
    e.request_body_raw = Some(raw);
    log.record(e);
    let (text, v) = one_line(dir.path());
    assert!(!text.contains("hunter2"), "{text}");
    assert!(v.get("request_body_raw").is_none(), "{v}");
    assert_eq!(v["flags"], json!(["duplicate_keys"]), "{v}");
    // Frame: the parsed, masked form is logged instead of the raw text.
    for frame in [
        r#"42["login",{"password":"hunter2","password":"<masked:3 chars>"}]"#,
        r#"42["login",{"a":{"password":"hunter2"},"a":{}}]"#,
        r#"42["m","{\"password\":\"hunter2\",\"password\":\"<masked:1 chars>\"}"]"#,
    ] {
        let m = mask_text(frame);
        assert!(!m.contains("hunter2"), "{frame} -> {m}");
        assert!(m.starts_with("42["), "{m}");
    }
    let dir = tempfile::tempdir().unwrap();
    let log = open_log(dir.path());
    let mut f = LogEntry::new(LogKind::Frame);
    f.direction = Some("in".into());
    f.request_body = Some(Value::String(
        r#"42["login",{"password":"hunter2","password":"<masked:3 chars>"}]"#.into(),
    ));
    log.record(f);
    let (text, _) = one_line(dir.path());
    assert!(!text.contains("hunter2"), "{text}");
}
