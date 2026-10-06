//! The protocol log: one JSONL file per day, flushed per line.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, Month, OffsetDateTime, format_description::well_known::Rfc3339};

use super::capture::Capture;
use super::mask::{
    MAX_BODY_BYTES, MAX_FIELD_BYTES, Origin, cap_text, cut_body, mask_form, mask_headers,
    mask_json_from, mask_jwts, mask_raw_json_from,
};
use super::rotate::{LogFile, plan_deletions};
use crate::{Result, io_err};

const FILE_PREFIX: &str = "protocol-";
const FILE_SUFFIX: &str = ".jsonl";
/// The flag of the marker written when a device reports a new firmware. Rotation keeps the log
/// days around such a marker, so every writer of the marker must use this constant.
pub const FLAG_FIRMWARE_CHANGED: &str = "firmware_changed";

/// Source of time, injectable for tests. Days are UTC days.
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    #[default]
    Request,
    Frame,
    Marker,
}

/// One protocol log line. Unset fields are omitted.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LogEntry {
    /// RFC 3339 UTC; filled by the log when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ts: String,
    pub kind: LogKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<Value>,
    /// The request body as sent (JSON text), kept only when it differs from the re-serialized
    /// `request_body` (key order, number spelling, duplicate keys). Masked like the body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<Value>,
    /// The response body as sent, like `request_body_raw`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// `unknown_endpoint`, `retry_suspected`, `firmware_changed`, `unknown_event`, ...
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// For frames: `in` or `out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// For a `log_lines_dropped` marker: how many lines could not be written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
}

impl LogEntry {
    pub fn new(kind: LogKind) -> Self {
        LogEntry {
            kind,
            ..Default::default()
        }
    }

    fn is_firmware_marker(&self) -> bool {
        self.kind == LogKind::Marker && self.flags.iter().any(|f| f == FLAG_FIRMWARE_CHANGED)
    }

    /// Add a header, joining the values of a repeated header with `, ` (names lowercased).
    pub fn add_header(&mut self, name: &str, value: &str) {
        let slot = self.headers.entry(name.to_ascii_lowercase()).or_default();
        if !slot.is_empty() {
            slot.push_str(", ");
        }
        slot.push_str(value);
    }

    /// Apply masking and the size caps to every field that can carry request data. Called by
    /// [`ProtocolLog::record`], so no caller can log a secret in clear.
    fn sanitized(mut self) -> Self {
        // A query string inside the path is masked like the query field.
        self.path = self.path.take().map(|p| {
            let p = match p.split_once('?') {
                Some((path, q)) => format!("{}?{}", mask_jwts(path), mask_form(q)),
                None => mask_jwts(&p),
            };
            cap_text(&p, MAX_FIELD_BYTES)
        });
        self.query = self
            .query
            .take()
            .map(|q| cap_text(&mask_form(&q), MAX_FIELD_BYTES));
        self.headers = mask_headers(&self.headers);
        // Request bodies and in-frames come from the device, responses and out-frames from the
        // server (`code` is masked only in device data, see `Origin`).
        let (body, raw) = sanitize_body(
            self.request_body.take(),
            self.request_body_raw.take(),
            Origin::Client,
        );
        (self.request_body, self.request_body_raw) = (body, raw);
        let (body, raw) = sanitize_body(
            self.response_body.take(),
            self.response_body_raw.take(),
            Origin::Server,
        );
        (self.response_body, self.response_body_raw) = (body, raw);
        self
    }
}

/// Mask and cut a body and its raw text. The raw text is kept only when its masked form differs
/// from the masked, re-serialized body, and only when it can be masked safely
/// ([`mask_raw_json_from`]).
fn sanitize_body(
    body: Option<Value>,
    raw: Option<String>,
    origin: Origin,
) -> (Option<Value>, Option<String>) {
    let Some(body) = body else {
        return (None, None);
    };
    let masked = mask_json_from(&body, origin);
    let reserialized = masked.to_string();
    let raw = raw
        .and_then(|r| mask_raw_json_from(&r, origin))
        .filter(|r| *r != reserialized)
        .map(|r| cap_text(&r, MAX_BODY_BYTES));
    (Some(cut_body(&masked)), raw)
}

struct State {
    /// Day of the open file.
    day: Date,
    file: File,
    /// Total bytes in `logs/` as far as this process knows.
    total: u64,
    /// Dates of `firmware_changed` markers.
    firmware_dates: Vec<Date>,
    /// Size-cap check runs again when `total` exceeds this (avoids a directory scan per line when
    /// protected files keep the total above the cap).
    check_at: u64,
    /// A write failed: start the next line with a newline so a torn line cannot merge with it.
    torn: bool,
    /// Lines lost since the last `log_lines_dropped` marker.
    pending_dropped: u64,
}

pub struct ProtocolLog {
    logs_dir: PathBuf,
    retention_days: u32,
    max_bytes: u64,
    clock: Arc<dyn Clock>,
    dropped: AtomicU64,
    capture: Option<Capture>,
    state: Mutex<State>,
}

pub fn file_name(day: Date) -> String {
    format!(
        "{FILE_PREFIX}{:04}-{:02}-{:02}{FILE_SUFFIX}",
        day.year(),
        day.month() as u8,
        day.day()
    )
}

fn parse_file_date(name: &str) -> Option<Date> {
    let s = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    let mut it = s.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u8 = it.next()?.parse().ok()?;
    let d: u8 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Date::from_calendar_date(y, Month::try_from(m).ok()?, d).ok()
}

fn list_files(dir: &Path) -> Vec<LogFile> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| {
        let e = e.ok()?;
        let date = parse_file_date(e.file_name().to_str()?)?;
        let size = e.metadata().ok()?.len();
        Some(LogFile {
            date,
            size,
            path: e.path(),
        })
    })
    .collect()
}

/// Dates of `firmware_changed` markers found in existing files (the file date approximates the
/// marker date; markers are written to the file of their own day).
fn scan_firmware_dates(files: &[LogFile]) -> Vec<Date> {
    files
        .iter()
        .filter(|f| {
            std::fs::read_to_string(&f.path)
                .map(|text| {
                    text.lines().any(|l| {
                        l.contains(FLAG_FIRMWARE_CHANGED)
                            && serde_json::from_str::<LogEntry>(l)
                                .map(|e| e.is_firmware_marker())
                                .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .map(|f| f.date)
        .collect()
}

fn open_day(dir: &Path, day: Date) -> Result<File> {
    let path = dir.join(file_name(day));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(io_err(&path))
}

impl ProtocolLog {
    /// Open the log below the data folder `dir` (files go to `<dir>/logs/`).
    pub fn open(dir: &Path, retention_days: u32, max_bytes: u64) -> Result<ProtocolLog> {
        Self::open_with_clock(dir, retention_days, max_bytes, Arc::new(SystemClock))
    }

    pub fn open_with_clock(
        dir: &Path,
        retention_days: u32,
        max_bytes: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<ProtocolLog> {
        let logs_dir = dir.join("logs");
        std::fs::create_dir_all(&logs_dir).map_err(io_err(&logs_dir))?;
        let day = clock.now().date();
        let file = open_day(&logs_dir, day)?;
        let files = list_files(&logs_dir);
        let firmware_dates = scan_firmware_dates(&files);
        let total = files.iter().map(|f| f.size).sum();
        let log = ProtocolLog {
            logs_dir,
            retention_days,
            max_bytes,
            clock,
            dropped: AtomicU64::new(0),
            capture: None,
            state: Mutex::new(State {
                day,
                file,
                total,
                firmware_dates,
                check_at: max_bytes,
                torn: false,
                pending_dropped: 0,
            }),
        };
        {
            let mut st = log.lock();
            log.rotate(&mut st);
        }
        Ok(log)
    }

    /// Also write every request and frame as a masked dump (see [`Capture`]).
    pub fn with_capture(mut self, capture: Capture) -> ProtocolLog {
        self.capture = Some(capture);
        self
    }

    /// Capture dumps that could not be written (0 when capture is off).
    pub fn capture_failures(&self) -> u64 {
        self.capture.as_ref().map_or(0, Capture::failed)
    }

    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Delete files per the rotation policy and resync the size total.
    fn rotate(&self, st: &mut State) {
        let files = list_files(&self.logs_dir);
        for path in plan_deletions(
            &files,
            &st.firmware_dates,
            st.day,
            self.retention_days,
            self.max_bytes,
        ) {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!("protocol log: cannot delete {}: {e}", path.display());
            }
        }
        st.total = list_files(&self.logs_dir).iter().map(|f| f.size).sum();
        st.check_at = if st.total > self.max_bytes {
            st.total + 1024 * 1024
        } else {
            self.max_bytes
        };
    }

    /// Total lines that could not be written to disk (they went to stderr instead).
    pub fn dropped_lines(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Write one line, retrying once after reopening the file. Returns whether it reached the file.
    fn try_write(&self, st: &mut State, line: &str) -> bool {
        let mut buf = String::with_capacity(line.len() + 1);
        if st.torn {
            buf.push('\n');
        }
        buf.push_str(line);
        let attempt = |f: &mut File, b: &str| f.write_all(b.as_bytes()).and_then(|_| f.flush());
        let mut ok = attempt(&mut st.file, &buf).is_ok();
        if !ok {
            // A partial write may be on disk: separate the retry from it.
            st.torn = true;
            if buf.as_bytes().first() != Some(&b'\n') {
                buf.insert(0, '\n');
            }
            if let Ok(f) = open_day(&self.logs_dir, st.day) {
                st.file = f;
                ok = attempt(&mut st.file, &buf).is_ok();
            }
        }
        if ok {
            st.torn = false;
            st.total += buf.len() as u64;
        }
        ok
    }

    /// Append one line. Never fails the caller and never loses an entry silently: when the file
    /// cannot be written, the masked line goes to stderr and is counted in [`Self::dropped_lines`];
    /// after recovery a `log_lines_dropped` marker with the count follows.
    pub fn record(&self, entry: LogEntry) {
        let mut entry = entry.sanitized();
        let mut st = self.lock();
        // The clock is read inside the lock so lines and day switches stay ordered.
        let now = self.clock.now();
        if entry.ts.is_empty() {
            entry.ts = now.format(&Rfc3339).unwrap_or_default();
        }
        // The entry is sanitized here: the dump has the masking of the log line.
        if let Some(capture) = &self.capture
            && entry.kind != LogKind::Marker
        {
            capture.write(&entry, now);
        }
        let mut line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "protocol log: cannot serialize entry: {e}"
                );
                self.dropped.fetch_add(1, Ordering::Relaxed);
                st.pending_dropped += 1;
                return;
            }
        };
        line.push('\n');

        let today = now.date();
        let mut need_rotate = false;
        // Only move forward: a clock that goes back keeps writing to the current file.
        if today > st.day {
            match open_day(&self.logs_dir, today) {
                Ok(f) => {
                    st.file = f;
                    st.day = today;
                    need_rotate = true;
                }
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "protocol log: {e}");
                }
            }
        }
        if entry.is_firmware_marker() && !st.firmware_dates.contains(&st.day) {
            let d = st.day;
            st.firmware_dates.push(d);
        }
        if self.try_write(&mut st, &line) {
            if st.pending_dropped > 0 {
                let mut m = LogEntry::new(LogKind::Marker);
                m.ts = entry.ts.clone();
                m.flags = vec!["log_lines_dropped".into()];
                m.dropped = Some(st.pending_dropped);
                if let Ok(mut ml) = serde_json::to_string(&m) {
                    ml.push('\n');
                    if self.try_write(&mut st, &ml) {
                        st.pending_dropped = 0;
                    }
                }
            }
        } else {
            let _ = std::io::stderr().write_all(line.as_bytes());
            self.dropped.fetch_add(1, Ordering::Relaxed);
            st.pending_dropped += 1;
        }
        if need_rotate || st.total > st.check_at {
            self.rotate(&mut st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_failure_is_counted_then_marked_and_never_merges_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = ProtocolLog::open(dir.path(), 30, 1 << 30).unwrap();
        let path = dir
            .path()
            .join("logs")
            .join(file_name(OffsetDateTime::now_utc().date()));
        log.record(LogEntry::new(LogKind::Request));
        // Break the file: read-only handle and mode 0444 so the reopen fails too.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        log.lock().file = File::open(&path).unwrap();
        let mut e = LogEntry::new(LogKind::Request);
        e.path = Some("/lost?token=SECRET".into());
        log.record(e);
        assert_eq!(log.dropped_lines(), 1);
        // Recovery.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        log.record(LogEntry::new(LogKind::Request));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "{text}");
        for l in &lines {
            serde_json::from_str::<Value>(l).unwrap();
        }
        let marker: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(marker["flags"][0], "log_lines_dropped");
        assert_eq!(marker["dropped"], 1);
        assert_eq!(log.dropped_lines(), 1);
    }

    #[test]
    fn one_failed_write_is_retried_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let log = ProtocolLog::open(dir.path(), 30, 1 << 30).unwrap();
        let path = dir
            .path()
            .join("logs")
            .join(file_name(OffsetDateTime::now_utc().date()));
        log.lock().file = File::open(&path).unwrap(); // read-only: write fails, reopen works
        log.record(LogEntry::new(LogKind::Request));
        assert_eq!(log.dropped_lines(), 0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().filter(|l| !l.is_empty()).count(), 1);
    }
}
