//! The report of `notedown cloud diagnose` (spec 4): counts gathered from the raw lines while
//! the bundle is written, and the text at the end. Every real value in the text goes through
//! the filter, so the report of a safe bundle shows stand-ins only.

use std::collections::BTreeMap;

use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use super::collect::{Window, is_heartbeat, log_ts};
use super::filter::Filter;
use super::state::{AccountState, ScanResult};

/// Log flags that always need a look.
const ATTENTION_FLAGS: [&str; 4] = [
    "unknown_endpoint",
    "firmware_changed",
    "unknown_event",
    "retry_suspected",
];

/// Marker flags that are normal and not listed.
const QUIET_MARKERS: [&str; 1] = ["implicit_lock"];

#[derive(Debug, Default, Clone)]
struct DeviceStats {
    firmware: Option<String>,
    last_request: Option<OffsetDateTime>,
    last_open: Option<OffsetDateTime>,
    last_close: Option<OffsetDateTime>,
    sync_starts: u64,
    sync_ends: u64,
    uploads: u64,
    downloads: u64,
    pushes: u64,
    push_items: u64,
    heartbeats: u64,
}

/// A group of the same finding: how often, when last, and one example (raw, filtered when
/// the report is written).
#[derive(Debug, Default, Clone)]
struct Group {
    count: u64,
    last: Option<OffsetDateTime>,
    example: Example,
}

#[derive(Debug, Default, Clone)]
enum Example {
    #[default]
    None,
    /// A route such as `POST /api/file/xyz`; routes stay.
    Route(String),
    /// Free text written by notedown; scrubbed.
    Text(String),
}

/// The counts of one bundle.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    devices: BTreeMap<String, DeviceStats>,
    flags: BTreeMap<String, Group>,
    errors: BTreeMap<String, Group>,
    refused: BTreeMap<String, Group>,
    markers: BTreeMap<String, Group>,
    /// Lines of the log and the journal that could not be read.
    pub unreadable: u64,
    log_lines: u64,
    last_line: Option<OffsetDateTime>,
    journal: BTreeMap<String, u64>,
    journal_events: u64,
}

impl Group {
    fn add(&mut self, at: Option<OffsetDateTime>, example: Example) {
        self.count += 1;
        if at >= self.last {
            self.last = at;
            self.example = example;
        }
    }
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

impl Stats {
    /// Count one protocol log line of the window (the raw line, before the filter).
    pub fn observe_log(&mut self, v: &Value) {
        let at = log_ts(v);
        self.log_lines += 1;
        if at > self.last_line {
            self.last_line = at;
        }
        let device = s(v, "device").map(String::from);
        let flags: Vec<&str> = v
            .get("flags")
            .and_then(Value::as_array)
            .map(|f| f.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let kind = s(v, "kind").unwrap_or_default();
        let route = || {
            let path = s(v, "path").unwrap_or_default();
            let path = path.split_once('?').map_or(path, |(r, _)| r);
            match s(v, "method") {
                Some(m) => format!("{m} {path}"),
                None => path.to_string(),
            }
        };
        for f in &flags {
            if ATTENTION_FLAGS.contains(f) {
                let example = match (*f, s(v, "message")) {
                    (_, Some(m)) => Example::Text(m.to_string()),
                    ("unknown_endpoint" | "retry_suspected", None) => Example::Route(route()),
                    _ => Example::None,
                };
                self.flags
                    .entry(f.to_string())
                    .or_default()
                    .add(at, example);
            } else if f.ends_with("_refused") {
                self.refused
                    .entry(route())
                    .or_default()
                    .add(at, Example::None);
            }
        }
        if let Some(d) = &device {
            let stats = self.devices.entry(d.clone()).or_default();
            if let Some(fw) = s(v, "firmware") {
                stats.firmware = Some(fw.to_string());
            }
            if let Some(fw) = v
                .get("headers")
                .and_then(|h| h.get("version"))
                .and_then(Value::as_str)
            {
                stats.firmware = Some(fw.to_string());
            }
        }
        match kind {
            "request" => {
                let path = s(v, "path").unwrap_or_default();
                if let Some(d) = &device {
                    let stats = self.devices.entry(d.clone()).or_default();
                    stats.last_request = stats.last_request.max(at);
                    if path.ends_with("synchronous/start") {
                        stats.sync_starts += 1;
                    } else if path.ends_with("synchronous/end") {
                        stats.sync_ends += 1;
                    } else if path.ends_with("upload/finish") {
                        stats.uploads += 1;
                    } else if path.starts_with("/api/oss/download") {
                        stats.downloads += 1;
                    }
                }
                let status = v.get("response_status").and_then(Value::as_u64);
                let failed = v.get("success").and_then(Value::as_bool) == Some(false)
                    || status.is_some_and(|c| c >= 400);
                if failed && !flags.contains(&"unknown_endpoint") {
                    let code = s(v, "error_code")
                        .or_else(|| {
                            v.get("response_body")
                                .and_then(|b| b.get("errorCode"))
                                .and_then(Value::as_str)
                        })
                        .map(String::from)
                        .unwrap_or_else(|| format!("HTTP {}", status.unwrap_or(0)));
                    self.errors
                        .entry(code)
                        .or_default()
                        .add(at, Example::Route(route()));
                }
            }
            "frame" => {
                let Some(d) = &device else { return };
                let stats = self.devices.entry(d.clone()).or_default();
                if is_heartbeat(v) {
                    stats.heartbeats += 1;
                    return;
                }
                let out = s(v, "direction") == Some("out");
                let body = s(v, "response_body").unwrap_or_default();
                if out && body.starts_with("0{") {
                    stats.last_open = stats.last_open.max(at);
                } else if out && body.starts_with("<close") {
                    stats.last_close = stats.last_close.max(at);
                } else if out && body.contains("ServerMessage") && body.contains("FILE-SYN") {
                    stats.pushes += 1;
                    stats.push_items += push_items(body);
                }
            }
            "marker" => {
                let message = s(v, "message").unwrap_or_default().to_string();
                let listed: Vec<&&str> = flags
                    .iter()
                    .filter(|f| !ATTENTION_FLAGS.contains(f) && !QUIET_MARKERS.contains(f))
                    .collect();
                if flags.is_empty() {
                    self.markers
                        .entry("marker".into())
                        .or_default()
                        .add(at, Example::Text(message));
                } else {
                    for f in listed {
                        self.markers
                            .entry(f.to_string())
                            .or_default()
                            .add(at, Example::Text(message.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    /// Count one journal event of the window (the raw line).
    pub fn observe_journal(&mut self, v: &Value) {
        self.journal_events += 1;
        let group = s(v, "type")
            .map(|t| t.split('.').next().unwrap_or(t).to_string())
            .unwrap_or_else(|| "other".into());
        *self.journal.entry(group).or_default() += 1;
    }
}

/// The items of a FILE-SYN push frame.
fn push_items(body: &str) -> u64 {
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&body[digits..]) else {
        return 0;
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .filter_map(|m| m.get("data").and_then(Value::as_array).map(Vec::len))
        .sum::<usize>() as u64
}

/// What the report shows besides the counts.
pub struct ReportInput<'a> {
    pub version: &'a str,
    pub now: OffsetDateTime,
    pub window: &'a Window,
    pub private: bool,
    pub fs: &'a str,
    pub names_preserved: bool,
    pub accounts: &'a [AccountState],
    pub scans: &'a [(String, ScanResult)],
    /// Events of all accounts after the `render` cursor; `None` without a cursor.
    pub render_behind: Option<u64>,
    /// The OCR pipeline of the data folder, numbers only.
    pub pipeline: PipelineCounts,
}

/// The OCR pipeline in the report: its journal cursor per account and the pages of its note
/// states that wait for OCR or failed. Read raw from `pipeline/<slug>.cursor` and
/// `pipeline/<slug>/notes/*.json` (notedown-cloud does not link the pipeline); `pipeline/`
/// itself never goes into the bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineCounts {
    /// By account slug: the cursor, `None` without one.
    pub cursors: Vec<(String, Option<u64>)>,
    pub pending: u64,
    pub failed: u64,
}

/// The report text and every real-derived value written into it (for the final check).
pub fn render(stats: &Stats, input: &ReportInput, filter: &mut Filter) -> (String, Vec<String>) {
    let mut values = Vec::new();
    let mut out = Vec::new();
    let now = input.now.to_offset(UtcOffset::UTC);
    let when = |t: Option<OffsetDateTime>| -> String {
        match t.map(|t| t.to_offset(UtcOffset::UTC)) {
            None => "-".into(),
            Some(t) if t.date() == now.date() => format!("{:02}:{:02}", t.hour(), t.minute()),
            Some(t) => format!(
                "{:02}-{:02} {:02}:{:02}",
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute()
            ),
        }
    };
    if input.private {
        out.push("private bundle: holds real names, do not share".to_string());
    }
    out.push(format!(
        "notedown {} - diagnose {}-{:02}-{:02} {:02}:{:02} UTC - window {} - {} bundle",
        input.version,
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        window_text(input.window),
        if input.private { "private" } else { "safe" }
    ));
    let last_line = if stats.log_lines == 0 {
        "no log lines in the window".to_string()
    } else {
        format!("last log line {} UTC", when(stats.last_line))
    };
    out.push(format!(
        "Data folder: {}, {} - {} - {last_line}",
        input.fs,
        if input.names_preserved {
            "keeps name spelling"
        } else {
            "changes name spelling"
        },
        count(input.accounts.len() as u64, "account")
    ));
    out.push(String::new());

    out.push("Devices".into());
    if stats.devices.is_empty() {
        out.push("  none in the window".into());
    }
    let mut unended = Vec::new();
    for (serial, d) in &stats.devices {
        let name = filter.device(serial);
        values.push(name.clone());
        let socket = match (d.last_open, d.last_close) {
            (Some(o), c) if Some(o) > c => format!("  socket open since {}", when(Some(o))),
            (_, Some(c)) => format!("  socket closed at {}", when(Some(c))),
            _ => String::new(),
        };
        let firmware = d
            .firmware
            .as_deref()
            .map(|f| {
                values.push(f.to_string());
                format!("  firmware {f}")
            })
            .unwrap_or_default();
        out.push(format!(
            "  {name}{firmware}  last request {}{socket}",
            when(d.last_request)
        ));
        let ended = if d.sync_starts > d.sync_ends {
            unended.push(format!(
                "{name}: {} of {}",
                d.sync_starts - d.sync_ends,
                d.sync_starts
            ));
            format!("{} not ended", d.sync_starts - d.sync_ends)
        } else {
            "all ended".into()
        };
        out.push(format!(
            "         syncs {} ({ended})  uploads {}  downloads {}  pushes {} ({} items)",
            d.sync_starts, d.uploads, d.downloads, d.pushes, d.push_items
        ));
        out.push(format!("         heartbeats {}", d.heartbeats));
    }
    out.push(String::new());

    out.push("Attention".into());
    let mut attention: Vec<(String, String)> = Vec::new();
    let example = |e: &Example, filter: &mut Filter, values: &mut Vec<String>| match e {
        Example::None => String::new(),
        Example::Route(r) => format!("  {r}"),
        Example::Text(t) => {
            let t = filter.text(t);
            values.push(t.clone());
            format!("  {t}")
        }
    };
    for (flag, g) in &stats.flags {
        let ex = example(&g.example, filter, &mut values);
        attention.push((
            flag.clone(),
            format!("{}x{ex}  last {}", g.count, when(g.last)),
        ));
    }
    for (code, g) in &stats.errors {
        let ex = example(&g.example, filter, &mut values);
        values.push(code.clone());
        attention.push((
            "errors".into(),
            format!("{}x  {code}{ex}  last {}", g.count, when(g.last)),
        ));
    }
    for (route, g) in &stats.refused {
        attention.push((
            "refused routes".into(),
            format!("{}x  {route}  last {}", g.count, when(g.last)),
        ));
    }
    for (flag, g) in &stats.markers {
        let ex = example(&g.example, filter, &mut values);
        attention.push((
            flag.clone(),
            format!("{}x{ex}  last {}", g.count, when(g.last)),
        ));
    }
    for u in unended {
        attention.push(("unended sync".into(), u));
    }
    for a in input.accounts {
        let acct = filter.account(&a.slug);
        values.push(acct.clone());
        if let Some(h) = &a.hold {
            let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(h.at) * 1_000_000).ok();
            attention.push((
                "scan hold".into(),
                format!(
                    "{acct} since {} (would delete {})",
                    when(at),
                    count(h.gone as u64, "file")
                ),
            ));
        }
        if a.hold_unreadable {
            attention.push((
                "scan hold".into(),
                format!("{acct}: the marker cannot be read"),
            ));
        }
        if a.pending {
            attention.push(("unfinished change".into(), acct.clone()));
        }
    }
    if let Some(n) = input.render_behind.filter(|n| *n > 0) {
        attention.push((
            "followers".into(),
            format!("render {} behind the journal", count(n, "event")),
        ));
    }
    for (slug, scan) in input.scans {
        let acct = filter.account(slug);
        match scan {
            ScanResult::Done(r) => {
                let parts: Vec<String> = [
                    (r.created.len(), "new"),
                    (r.modified.len(), "changed"),
                    (r.moved.len(), "moved"),
                    (r.deleted.len(), "deleted"),
                ]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, w)| format!("{n} {w}"))
                .collect();
                if !parts.is_empty() {
                    attention.push((
                        "scan drift".into(),
                        format!("{acct}: {} not yet applied", parts.join(", ")),
                    ));
                }
                if let Some(h) = &r.held {
                    attention.push((
                        "scan drift".into(),
                        format!(
                            "{acct}: the scan would hold ({} gone)",
                            count(h.gone as u64, "file")
                        ),
                    ));
                }
            }
            ScanResult::Skipped(why) => {
                let why = filter.text(why);
                values.push(why.clone());
                attention.push(("scan skipped".into(), format!("{acct}: {why}")));
            }
        }
    }
    if stats.unreadable > 0 {
        attention.push(("unreadable lines".into(), stats.unreadable.to_string()));
    }
    if attention.is_empty() {
        out.push("  nothing found".into());
    }
    for (label, text) in attention {
        out.push(format!("  {label:<18} {text}"));
    }
    out.push(String::new());

    if stats.journal_events == 0 {
        out.push("Journal  no events in the window".into());
    } else {
        let parts: Vec<String> = stats
            .journal
            .iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        out.push(format!(
            "Journal  {}: {}",
            count(stats.journal_events, "event"),
            parts.join(", ")
        ));
    }
    let p = &input.pipeline;
    if p.cursors.iter().all(|(_, c)| c.is_none()) {
        // No cursor of any account: the pipeline runs on another machine, or not at all.
        out.push("Pipeline  not on this machine".into());
    } else {
        let mut cursors = Vec::new();
        for (slug, c) in &p.cursors {
            let acct = filter.account(slug);
            values.push(acct.clone());
            let at = c.map_or_else(|| "none".to_string(), |c| c.to_string());
            cursors.push(format!("{acct} {at}"));
        }
        out.push(format!(
            "Pipeline  cursor {}, {} pending, {} failed",
            cursors.join(", "),
            count(p.pending, "page"),
            p.failed
        ));
    }
    (out.join("\n") + "\n", values)
}

fn count(n: u64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// `3 days`, `12 hours`, `30 minutes`, or `since 10-06 14:10 UTC`.
fn window_text(w: &Window) -> String {
    let minutes = (w.to - w.from).whole_minutes().max(0) as u64;
    if minutes > 0 && minutes.is_multiple_of(24 * 60) {
        count(minutes / (24 * 60), "day")
    } else if minutes > 0 && minutes.is_multiple_of(60) {
        count(minutes / 60, "hour")
    } else if minutes < 120 {
        count(minutes, "minute")
    } else {
        let f = w.from.to_offset(UtcOffset::UTC);
        format!(
            "since {:02}-{:02} {:02}:{:02} UTC",
            f.month() as u8,
            f.day(),
            f.hour(),
            f.minute()
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;
    use crate::diag::filter::Scrub;
    use crate::diag::standin::StandIns;

    const NOW: OffsetDateTime = datetime!(2026-10-09 14:10 UTC);

    fn input<'a>(w: &'a Window, private: bool) -> ReportInput<'a> {
        ReportInput {
            version: "0.1.0",
            now: NOW,
            window: w,
            private,
            fs: "apfs",
            names_preserved: true,
            accounts: &[],
            scans: &[],
            render_behind: None,
            pipeline: PipelineCounts::default(),
        }
    }

    fn window() -> Window {
        Window {
            from: datetime!(2026-10-06 14:10 UTC),
            to: NOW,
        }
    }

    fn filter() -> Filter {
        let scrub = Scrub {
            slugs: vec!["jordi".into()],
            ..Scrub::default()
        };
        Filter::new(false, StandIns::new(4), scrub)
    }

    fn req(ts: &str, path: &str) -> Value {
        json!({"ts": ts, "kind": "request", "device": "SN1", "method": "POST", "path": path,
            "response_status": 200, "success": true})
    }

    #[test]
    fn a_fixture_log_gives_device_and_attention_lines() {
        let mut st = Stats::default();
        st.observe_log(&req(
            "2026-10-09T13:00:00Z",
            "/api/file/2/files/synchronous/start",
        ));
        st.observe_log(&req(
            "2026-10-09T13:01:00Z",
            "/api/file/2/files/upload/finish",
        ));
        st.observe_log(&req(
            "2026-10-09T13:02:00Z",
            "/api/file/2/files/synchronous/end",
        ));
        st.observe_log(&req(
            "2026-10-09T13:05:00Z",
            "/api/file/2/files/synchronous/start",
        ));
        st.observe_log(&json!({"ts": "2026-10-09T13:06:00Z", "kind": "request", "device": "SN1",
            "method": "POST", "path": "/api/file/xyz", "response_status": 404, "flags": ["unknown_endpoint"]}));
        st.observe_log(
            &json!({"ts": "2026-10-09T13:07:00Z", "kind": "request", "device": "SN1",
            "method": "POST", "path": "/api/file/2/files/synchronous/start", "response_status": 200,
            "success": false, "error_code": "E0079"}),
        );
        st.observe_log(&json!({"ts": "2026-10-08T09:12:00Z", "kind": "marker",
            "flags": ["recovery_failed"], "message": "account jordi: recovery failed"}));
        st.observe_log(&json!({"ts": "2026-10-09T13:08:00Z", "kind": "marker",
            "flags": ["implicit_lock"], "message": "account jordi: sync lock taken"}));
        st.observe_log(
            &json!({"ts": "2026-10-09T13:09:00Z", "kind": "frame", "device": "SN1",
            "direction": "in", "request_body": "2"}),
        );
        st.observe_log(
            &json!({"ts": "2026-10-09T13:09:00Z", "kind": "frame", "device": "SN1",
            "direction": "out", "response_body": "0{\"sid\":\"x\"}"}),
        );
        let push =
            r#"42["ServerMessage","{\"msgType\":\"FILE-SYN\",\"data\":[{\"id\":1},{\"id\":2}]}"]"#;
        st.observe_log(
            &json!({"ts": "2026-10-09T13:10:00Z", "kind": "frame", "device": "SN1",
            "direction": "out", "response_body": push}),
        );
        let w = window();
        let (text, values) = render(&st, &input(&w, false), &mut filter());
        assert!(
            text.starts_with(
                "notedown 0.1.0 - diagnose 2026-10-09 14:10 UTC - window 3 days - safe bundle\n"
            ),
            "{text}"
        );
        assert!(text.contains("last log line 13:10 UTC"), "{text}");
        assert!(
            text.contains("  dev-1  last request 13:07  socket open since 13:09"),
            "{text}"
        );
        assert!(
            text.contains("syncs 3 (2 not ended)  uploads 1  downloads 0  pushes 1 (2 items)"),
            "{text}"
        );
        assert!(text.contains("heartbeats 1"), "{text}");
        assert!(
            text.contains("unknown_endpoint   1x  POST /api/file/xyz  last 13:06"),
            "{text}"
        );
        assert!(
            text.contains(
                "errors             1x  E0079  POST /api/file/2/files/synchronous/start  last 13:07"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "recovery_failed    1x  account acct-1: recovery failed  last 10-08 09:12"
            ),
            "{text}"
        );
        assert!(text.contains("unended sync       dev-1: 2 of 3"), "{text}");
        assert!(
            !text.contains("implicit_lock") && !text.contains("jordi") && !text.contains("SN1"),
            "{text}"
        );
        assert!(values.contains(&"dev-1".to_string()));
    }

    #[test]
    fn an_empty_window_says_so_and_finds_nothing() {
        let w = window();
        let (text, _) = render(&Stats::default(), &input(&w, false), &mut filter());
        assert!(text.contains("no log lines in the window"), "{text}");
        assert!(text.contains("Attention\n  nothing found\n"), "{text}");
        assert!(text.contains("Journal  no events in the window"), "{text}");
    }

    #[test]
    fn a_private_report_says_so_on_the_first_line() {
        let w = window();
        let (text, _) = render(&Stats::default(), &input(&w, true), &mut filter());
        assert!(
            text.starts_with("private bundle: holds real names, do not share\n"),
            "{text}"
        );
        assert!(text.lines().nth(1).unwrap().ends_with("private bundle"));
    }

    #[test]
    fn the_window_is_said_in_plain_units() {
        let w = |from| Window { from, to: NOW };
        assert_eq!(window_text(&w(datetime!(2026-10-09 02:10 UTC))), "12 hours");
        assert_eq!(
            window_text(&w(datetime!(2026-10-09 13:40 UTC))),
            "30 minutes"
        );
        assert_eq!(
            window_text(&w(datetime!(2026-10-08 22:00 UTC))),
            "since 10-08 22:00 UTC"
        );
    }
}
