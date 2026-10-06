//! The change journal: server-wide, append-only JSONL under `<data>/journal/`, shared by all
//! accounts. Consumers (the Crystalline pipeline) read it from their cursor on.
//!
//! - Files: `journal-<first seq, 12 digits>.jsonl`. A new file starts at a month change (UTC,
//!   from the first line's `ts`) or when the current file has reached the size cap (64 MiB). A
//!   batch is never split across files.
//! - `seq` is strictly increasing and never reused. It is read from the tail of the newest file
//!   on every append, under the lock, so several handles and processes can append.
//! - Every append takes `journal/.lock` (std `File::lock`, exclusive), writes all lines of the
//!   batch with one write and one fsync, and returns their `seq` values.
//! - A torn last line (a crash during an append, no trailing newline) is ignored by readers and
//!   cut before the next append.
//!
//! Lock order: the account store lock (`accounts/<slug>/.store.lock`) first, then
//! `journal/.lock`. The journal never takes a store lock, so this order cannot deadlock.

mod cursor;
mod event;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use cursor::{Cursor, validate_consumer};
pub use event::{Event, EventBody, FileKind, Origin, new_op_id};

use crate::diag::{Clock, SystemClock};
use crate::{Error, Result, io_err};

/// `<data>/journal`.
pub const JOURNAL_DIR: &str = "journal";
/// `<data>/journal/.lock`.
pub const LOCK_FILE: &str = ".lock";
/// `<data>/journal/consumers`.
pub const CONSUMERS_DIR: &str = "consumers";
/// A journal file that has reached this size is closed; the next append starts a new one.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Crash repair looks for an operation's events in this many last journal lines.
pub const REPAIR_SCAN_LINES: usize = 1000;

const PREFIX: &str = "journal-";
const SUFFIX: &str = ".jsonl";

/// Settings of a [`Journal`]; tests inject a small cap and a clock.
#[derive(Clone)]
pub struct JournalOptions {
    pub max_file_bytes: u64,
    pub clock: Arc<dyn Clock>,
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            max_file_bytes: MAX_FILE_BYTES,
            clock: Arc::new(SystemClock),
        }
    }
}

/// A handle on the journal folder. Cheap; any number of handles may exist, in any process.
pub struct Journal {
    dir: PathBuf,
    consumers: PathBuf,
    max_file_bytes: u64,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("dir", &self.dir)
            .field("max_file_bytes", &self.max_file_bytes)
            .finish_non_exhaustive()
    }
}

/// The held journal lock. Dropping it releases the lock.
pub struct JournalLock<'a> {
    journal: &'a Journal,
    _file: std::fs::File,
}

fn file_name(first_seq: u64) -> String {
    format!("{PREFIX}{first_seq:012}{SUFFIX}")
}

fn parse_file_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    if digits.len() < 12 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn month_of(ms: u64) -> (i32, u8) {
    let ns = i128::from(ms) * 1_000_000;
    match time::OffsetDateTime::from_unix_timestamp_nanos(ns) {
        Ok(t) => (t.year(), t.month() as u8),
        Err(_) => (0, 0),
    }
}

fn corrupt(path: &Path, message: String) -> Error {
    Error::Config {
        path: path.to_path_buf(),
        message,
    }
}

/// Up to `max` complete lines from the end of `path`, oldest first, and the length of the
/// complete part of the file (a torn last line is not part of it).
fn tail_lines(path: &Path, max: usize) -> Result<(Vec<String>, u64)> {
    const CHUNK: u64 = 64 * 1024;
    let mut f = std::fs::File::open(path).map_err(io_err(path))?;
    let len = f.metadata().map_err(io_err(path))?.len();
    let mut buf: Vec<u8> = Vec::new();
    let mut start = len;
    let mut complete: Option<u64> = None;
    let mut lines: Vec<String> = Vec::new();
    // `buf` holds bytes [start, end) where `end` is the end of the complete part once known.
    loop {
        if complete.is_none() || lines.len() < max {
            if start == 0 {
                break;
            }
            let read_from = start.saturating_sub(CHUNK);
            let mut chunk = vec![0u8; (start - read_from) as usize];
            f.seek(SeekFrom::Start(read_from)).map_err(io_err(path))?;
            f.read_exact(&mut chunk).map_err(io_err(path))?;
            chunk.extend_from_slice(&buf);
            buf = chunk;
            start = read_from;
        } else {
            break;
        }
        if complete.is_none() {
            match buf.iter().rposition(|&b| b == b'\n') {
                Some(p) => {
                    complete = Some(start + p as u64 + 1);
                    buf.truncate(p + 1);
                }
                None => continue,
            }
        }
        // Peel off complete lines from the back while a newline precedes them.
        while lines.len() < max {
            let body_end = buf.len() - 1; // buf ends with '\n'
            match buf[..body_end].iter().rposition(|&b| b == b'\n') {
                Some(p) => {
                    lines.push(String::from_utf8_lossy(&buf[p + 1..body_end]).into_owned());
                    buf.truncate(p + 1);
                }
                None => break,
            }
        }
    }
    // At the start of the file the rest of `buf` is the first line.
    if start == 0 && lines.len() < max && !buf.is_empty() && complete.is_some() {
        let body_end = buf.len() - 1;
        lines.push(String::from_utf8_lossy(&buf[..body_end]).into_owned());
    }
    lines.reverse();
    Ok((lines, complete.unwrap_or(0)))
}

fn first_line(path: &Path) -> Result<Option<String>> {
    let f = std::fs::File::open(path).map_err(io_err(path))?;
    let mut line = Vec::new();
    BufReader::new(f)
        .read_until(b'\n', &mut line)
        .map_err(io_err(path))?;
    if line.last() != Some(&b'\n') {
        return Ok(None);
    }
    line.pop();
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

fn field_u64(line: &str, key: &str, path: &Path) -> Result<u64> {
    let v: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| corrupt(path, format!("journal line is not JSON: {e}")))?;
    v.get(key)
        .and_then(|s| s.as_u64())
        .ok_or_else(|| corrupt(path, format!("journal line without {key}")))
}

impl Journal {
    /// Open (and create) `<data_dir>/journal/` with the default cap and the system clock.
    pub fn open(data_dir: &Path) -> Result<Journal> {
        Self::open_with(data_dir, JournalOptions::default())
    }

    /// Open with explicit options.
    pub fn open_with(data_dir: &Path, options: JournalOptions) -> Result<Journal> {
        let dir = data_dir.join(JOURNAL_DIR);
        let consumers = dir.join(CONSUMERS_DIR);
        if !consumers.is_dir() {
            std::fs::create_dir_all(&consumers).map_err(io_err(&consumers))?;
            crate::fsutil::fsync_dir(&dir)?;
            crate::fsutil::fsync_dir(data_dir)?;
        }
        Ok(Journal {
            dir,
            consumers,
            max_file_bytes: options.max_file_bytes.max(1),
            clock: options.clock,
        })
    }

    /// The journal folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Milliseconds since the epoch, from the clock.
    pub fn now_ms(&self) -> u64 {
        let ns = self.clock.now().unix_timestamp_nanos();
        u64::try_from(ns / 1_000_000).unwrap_or(0)
    }

    /// The journal files, sorted by their first `seq`.
    pub fn files(&self) -> Result<Vec<(u64, PathBuf)>> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&self.dir).map_err(io_err(&self.dir))?;
        for entry in entries {
            let entry = entry.map_err(io_err(&self.dir))?;
            if let Some(first) = entry.file_name().to_str().and_then(parse_file_name)
                && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            {
                out.push((first, entry.path()));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Take the journal lock (exclusive, blocking). Hold it to look at the tail and append in
    /// one step. Lock order: a store lock, if any, is taken before this one.
    pub fn lock(&self) -> Result<JournalLock<'_>> {
        let path = self.dir.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_err(&path))?;
        file.lock().map_err(io_err(&path))?;
        Ok(JournalLock {
            journal: self,
            _file: file,
        })
    }

    /// Append `events` as one batch: assign `seq` and `ts` (in place), write all lines with
    /// one write, fsync, and return the `seq` values. An empty batch writes nothing.
    pub fn append(&self, events: &mut [Event]) -> Result<Vec<u64>> {
        self.lock()?.append(events)
    }

    /// The last `seq` in the journal (0 for an empty journal).
    pub fn last_seq(&self) -> Result<u64> {
        Ok(self.lock()?.tail_state()?.last_seq)
    }

    /// The events with `seq >= from`, in order, across rotated files. A torn last line is not
    /// returned. A line that cannot be read is an `Err` item.
    pub fn read_from(&self, from: u64) -> Result<ReadFrom> {
        let files = self.files()?;
        let start = files
            .iter()
            .rposition(|(first, _)| *first <= from)
            .unwrap_or(0);
        Ok(ReadFrom {
            files: files.into_iter().skip(start).map(|(_, p)| p).collect(),
            current: None,
            from,
        })
    }

    /// The cursor of consumer `name`, if it has one.
    pub fn cursor(&self, name: &str) -> Result<Option<Cursor>> {
        cursor::read(&cursor::cursor_path(&self.consumers, name)?)
    }

    /// Store the cursor of consumer `name` atomically: `seq` is the last fully processed one.
    pub fn set_cursor(&self, name: &str, seq: u64) -> Result<Cursor> {
        let c = Cursor {
            seq,
            updated_at: self.now_ms(),
        };
        cursor::write(&cursor::cursor_path(&self.consumers, name)?, &c)?;
        Ok(c)
    }
}

struct TailState {
    last_seq: u64,
    /// The newest file: its path, the length of its complete part, its real length and the
    /// `ts` of its first line (None when it has no complete line).
    newest: Option<(PathBuf, u64, u64, Option<u64>)>,
}

impl JournalLock<'_> {
    fn tail_state(&self) -> Result<TailState> {
        let files = self.journal.files()?;
        let Some((first, path)) = files.last().cloned() else {
            return Ok(TailState {
                last_seq: 0,
                newest: None,
            });
        };
        let len = std::fs::metadata(&path).map_err(io_err(&path))?.len();
        let (lines, complete) = tail_lines(&path, 1)?;
        let (last_seq, first_ts) = match lines.last() {
            Some(line) => {
                let seq = field_u64(line, "seq", &path)?;
                let head = first_line(&path)?.unwrap_or_else(|| line.clone());
                (seq, Some(field_u64(&head, "ts", &path)?))
            }
            None => (first.saturating_sub(1), None),
        };
        Ok(TailState {
            last_seq,
            newest: Some((path, complete, len, first_ts)),
        })
    }

    /// See [`Journal::append`]; this one runs under the held lock.
    pub fn append(&self, events: &mut [Event]) -> Result<Vec<u64>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let j = self.journal;
        let tail = self.tail_state()?;
        let now = j.now_ms();
        let next = tail.last_seq + 1;
        let (path, base) = match tail.newest {
            Some((path, complete, len, first_ts)) => {
                let full = complete >= j.max_file_bytes;
                let new_month = first_ts.is_some_and(|ts| month_of(ts) != month_of(now));
                if complete > 0 && (full || new_month) {
                    (self.create(next)?, 0)
                } else {
                    if len > complete {
                        // A torn line of a crashed append: cut it so the next line starts clean.
                        let f = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .map_err(io_err(&path))?;
                        f.set_len(complete).map_err(io_err(&path))?;
                        f.sync_all().map_err(io_err(&path))?;
                    }
                    (path, complete)
                }
            }
            None => (self.create(next)?, 0),
        };
        let mut bytes = Vec::new();
        let mut seqs = Vec::with_capacity(events.len());
        for (i, ev) in events.iter_mut().enumerate() {
            ev.seq = next + i as u64;
            ev.ts = now;
            seqs.push(ev.seq);
            bytes.extend_from_slice(ev.to_line().as_bytes());
            bytes.push(b'\n');
        }
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(io_err(&path))?;
        let written = f.write_all(&bytes).and_then(|()| f.sync_data());
        if let Err(e) = written {
            // Leave no partial batch behind.
            let _ = f.set_len(base).and_then(|()| f.sync_all());
            return Err(io_err(&path)(e));
        }
        Ok(seqs)
    }

    fn create(&self, first_seq: u64) -> Result<PathBuf> {
        let path = self.journal.dir.join(file_name(first_seq));
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err(&path))?;
        crate::fsutil::fsync_dir(&self.journal.dir)?;
        Ok(path)
    }

    /// How many of the last `window` complete journal lines carry `opId == op_id`. Continues
    /// into older files when the newest one is short.
    pub fn count_op(&self, op_id: &str, window: usize) -> Result<usize> {
        let mut left = window;
        let mut count = 0;
        for (_, path) in self.journal.files()?.iter().rev() {
            if left == 0 {
                break;
            }
            let (lines, _) = tail_lines(path, left)?;
            left -= lines.len();
            for line in &lines {
                let v: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("opId").and_then(|o| o.as_str()) == Some(op_id) {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}

/// The iterator of [`Journal::read_from`].
pub struct ReadFrom {
    files: VecDeque<PathBuf>,
    current: Option<(PathBuf, BufReader<std::fs::File>)>,
    from: u64,
}

impl Iterator for ReadFrom {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Result<Event>> {
        loop {
            if self.current.is_none() {
                let path = self.files.pop_front()?;
                match std::fs::File::open(&path) {
                    Ok(f) => self.current = Some((path, BufReader::new(f))),
                    // Removed by the admin meanwhile: go on with the next file.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Some(Err(io_err(&path)(e))),
                }
            }
            let (path, reader) = self.current.as_mut().expect("set above");
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Err(e) => {
                    let err = io_err(path)(e);
                    self.current = None;
                    return Some(Err(err));
                }
                Ok(0) => {
                    self.current = None;
                    continue;
                }
                Ok(_) => {}
            }
            if line.last() != Some(&b'\n') {
                // A torn or unfinished last line: not an event (yet).
                self.current = None;
                continue;
            }
            line.pop();
            let event: Event = match serde_json::from_slice(&line) {
                Ok(e) => e,
                Err(e) => {
                    return Some(Err(corrupt(
                        path,
                        format!("journal line cannot be read: {e}"),
                    )));
                }
            };
            if event.seq >= self.from {
                return Some(Ok(event));
            }
        }
    }
}
