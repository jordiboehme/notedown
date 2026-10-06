//! The protocol log: one JSONL file per day, flushed per line.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, Month, OffsetDateTime, format_description::well_known::Rfc3339};

use super::mask::{cut_body, mask_headers, mask_json, mask_query};
use super::rotate::{LogFile, plan_deletions};
use crate::{Result, io_err};

const FILE_PREFIX: &str = "protocol-";
const FILE_SUFFIX: &str = ".jsonl";
const FLAG_FIRMWARE_CHANGED: &str = "firmware_changed";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<Value>,
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

    /// Apply masking and the size cap. Called by [`ProtocolLog::record`], so no caller can log a
    /// secret in clear.
    fn sanitized(mut self) -> Self {
        self.query = self.query.as_deref().map(mask_query);
        self.headers = mask_headers(&self.headers);
        let prep = |v: Option<Value>| v.map(|v| cut_body(&mask_json(&v)));
        self.request_body = prep(self.request_body.take());
        self.response_body = prep(self.response_body.take());
        self
    }
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
}

pub struct ProtocolLog {
    logs_dir: PathBuf,
    retention_days: u32,
    max_bytes: u64,
    clock: Arc<dyn Clock>,
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
            state: Mutex::new(State {
                day,
                file,
                total,
                firmware_dates,
                check_at: max_bytes,
            }),
        };
        {
            let mut st = log.lock();
            log.rotate(&mut st);
        }
        Ok(log)
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

    /// Append one line. Never fails the caller: I/O errors go to stderr.
    pub fn record(&self, entry: LogEntry) {
        let now = self.clock.now();
        let mut entry = entry.sanitized();
        if entry.ts.is_empty() {
            entry.ts = now.format(&Rfc3339).unwrap_or_default();
        }
        let mut line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("protocol log: cannot serialize entry: {e}");
                return;
            }
        };
        line.push('\n');

        let mut st = self.lock();
        let today = now.date();
        let mut need_rotate = false;
        if today != st.day {
            match open_day(&self.logs_dir, today) {
                Ok(f) => {
                    st.file = f;
                    st.day = today;
                    need_rotate = true;
                }
                Err(e) => eprintln!("protocol log: {e}"),
            }
        }
        if entry.is_firmware_marker() && !st.firmware_dates.contains(&st.day) {
            let d = st.day;
            st.firmware_dates.push(d);
        }
        match st
            .file
            .write_all(line.as_bytes())
            .and_then(|_| st.file.flush())
        {
            Ok(()) => st.total += line.len() as u64,
            Err(e) => eprintln!("protocol log: write failed: {e}"),
        }
        if need_rotate || st.total > st.check_at {
            self.rotate(&mut st);
        }
    }
}
