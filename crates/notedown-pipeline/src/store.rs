//! The text store below `<data>/pipeline/<slug>/` (spec 2, 5.3, 5.4).
//!
//! - `log.jsonl`: one [`LogLine`] per page text version or note change. `seq` grows by one per
//!   line and is read from the last complete line. An append writes all lines of a batch with
//!   one write and one fsync. Readers stop at the last complete line; a torn last line (a crash
//!   during an append) is cut by [`TextStore::repair`] and before the next append.
//! - `notes/<key>.json`: the [`NoteState`] of a note, written atomically.
//! - `consumers/<name>.json`: the [`Bookmark`] of a consumer, written under `.lock`.
//! - `stats.json`: the [`Stats`] of the account.
//!
//! One writer per data folder: the holder of `<data>/pipeline/.writer.lock` ([`WriterLock`]).
//! Its write order per page version is binding: first the log line, then the note state. A
//! crash between the two leaves a state behind the log, never ahead; [`TextStore::repair`]
//! brings it up to date.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::model::{LineChange, LogLine, NoteState, PageOcrState, PageState, title_of};
use crate::{Error, Result};

/// `<data>/pipeline`.
pub use notedown_cloud::auth::PIPELINE_DIR;
/// `<data>/pipeline/.writer.lock`.
pub const WRITER_LOCK_FILE: &str = ".writer.lock";
/// `<data>/pipeline/<slug>/.lock`: the prune and bookmark writes.
pub const LOCK_FILE: &str = ".lock";
const LOG_FILE: &str = "log.jsonl";
const NOTES_DIR: &str = "notes";
const CONSUMERS_DIR: &str = "consumers";
const STATS_FILE: &str = "stats.json";
/// `<local>/pipeline/<slug>/owner.json`: `{"userId":N}`, the server account the text is of.
pub const OWNER_FILE: &str = "owner.json";
/// The name start of a store on its way out: `<local>/pipeline/.removed-<slug>-<hex>`.
pub const TOMBSTONE_PREFIX: &str = ".removed-";

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn corrupt(path: &Path, message: String) -> Error {
    Error::Corrupt {
        path: path.to_path_buf(),
        message,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// JSON with two-space indent and a trailing newline.
fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text.into_bytes())
}

/// The parsed JSON file at `path`, `None` when it does not exist. A file that does not parse
/// is an error.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(path)(e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| corrupt(path, format!("cannot be read: {e}")))
}

/// The `*.json` files of `dir` with their stem, sorted; temp files (dot names) are left out.
fn json_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(dir))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if let Some(stem) = name.strip_suffix(".json") {
            out.push((stem.to_string(), entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Opens (creates) `path` and takes its lock, waiting for it.
fn lock_file(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(io_err(path))?;
    file.lock().map_err(io_err(path))?;
    Ok(file)
}

/// The last complete line of `path` (without its newline), the length of the complete part
/// and the whole length. A missing file is empty.
fn tail(path: &Path) -> Result<(Option<Vec<u8>>, u64, u64)> {
    const CHUNK: u64 = 64 * 1024;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, 0, 0)),
        Err(e) => return Err(io_err(path)(e)),
    };
    let len = f.metadata().map_err(io_err(path))?.len();
    // `buf` holds bytes [start, len).
    let mut buf: Vec<u8> = Vec::new();
    let mut start = len;
    let mut complete: Option<u64> = None;
    loop {
        if let Some(end) = complete {
            let body = &buf[..(end - start) as usize - 1];
            if let Some(p) = body.iter().rposition(|&b| b == b'\n') {
                return Ok((Some(body[p + 1..].to_vec()), end, len));
            }
            if start == 0 {
                return Ok((Some(body.to_vec()), end, len));
            }
        } else if let Some(p) = buf.iter().rposition(|&b| b == b'\n') {
            complete = Some(start + p as u64 + 1);
            continue;
        } else if start == 0 {
            return Ok((None, 0, len));
        }
        let from = start.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (start - from) as usize];
        f.seek(SeekFrom::Start(from)).map_err(io_err(path))?;
        f.read_exact(&mut chunk).map_err(io_err(path))?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        start = from;
    }
}

/// The held writer lock of a data folder. Dropping it releases the lock.
#[derive(Debug)]
pub struct WriterLock {
    _file: std::fs::File,
}

impl WriterLock {
    /// Takes `<data>/pipeline/.writer.lock` without waiting. A second writer gets
    /// [`Error::WriterBusy`].
    pub fn take(data: &Path) -> Result<WriterLock> {
        let dir = data.join(PIPELINE_DIR);
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let path = dir.join(WRITER_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_err(&path))?;
        match file.try_lock() {
            Ok(()) => Ok(WriterLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::WriterBusy),
            Err(std::fs::TryLockError::Error(e)) => Err(io_err(&path)(e)),
        }
    }
}

/// `consumers/<name>.json`: `{seq, updatedAt}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    /// The last acknowledged `seq`.
    pub seq: u64,
    /// Ms since the epoch.
    pub updated_at: u64,
}

/// `stats.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Stats {
    /// How often the safety net took a new reading of an untouched block (spec 3.3).
    pub region_missed: u64,
    /// When the last worker run ended, ms since the epoch.
    pub last_run_ms: u64,
}

/// The text store of one account. [`TextStore::open`] gives a read-only store (the MCP
/// server: reads and bookmarks); the writer opens it with [`TextStore::open_writer`], which
/// repairs it first. The log, note states and stats are written only while the store is
/// repaired: a failed append clears that until the next [`TextStore::repair`].
#[derive(Debug)]
pub struct TextStore {
    dir: PathBuf,
    repaired: AtomicBool,
}

/// What a sweep did.
#[derive(Debug, Default)]
pub struct Swept {
    /// Tombstones deleted.
    pub deleted: usize,
    /// Tombstones still there, with the error of their delete.
    pub failed: Vec<(PathBuf, std::io::Error)>,
}

/// `owner.json`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Owner {
    user_id: u64,
}

/// A name part no other tombstone of this process has: the time and the process id, and a
/// count for two in the same nanosecond.
fn tomb_hex() -> String {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&(nanos ^ n.rotate_left(48)).to_be_bytes());
    raw[8..12].copy_from_slice(&std::process::id().to_be_bytes());
    raw[12..].copy_from_slice(&(n as u32).to_be_bytes());
    const_hex::encode(raw)
}

/// Deletes a tombstone, a folder or a file.
fn remove_tombstone(path: &Path) -> std::io::Result<()> {
    let r = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Only the `seq` of a log line.
#[derive(Deserialize)]
struct SeqOnly {
    seq: u64,
}

impl TextStore {
    /// The store of account `slug` in `data`; creates `pipeline/<slug>/`.
    pub fn open(data: &Path, slug: &str) -> Result<TextStore> {
        notedown_cloud::auth::validate_slug(slug)?;
        let dir = data.join(PIPELINE_DIR).join(slug);
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        Ok(TextStore {
            dir,
            repaired: AtomicBool::new(false),
        })
    }

    /// The store of account `slug` for the writer: [`open`](Self::open), then
    /// [`repair`](Self::repair). Only such a store takes writes.
    pub fn open_writer(data: &Path, slug: &str, w: &WriterLock) -> Result<TextStore> {
        let store = TextStore::open(data, slug)?;
        store.repair(w)?;
        Ok(store)
    }

    /// The user id of the server account this text is of (`owner.json`), `None` when the store
    /// has no owner file. A file that cannot be read or parsed is an error, never `None`.
    pub fn owner(&self) -> Result<Option<u64>> {
        Ok(read_json::<Owner>(&self.dir.join(OWNER_FILE))?.map(|o| o.user_id))
    }

    /// Writes `owner.json`. For a new store it is the first write.
    pub fn set_owner(&self, user_id: u64, _w: &WriterLock) -> Result<()> {
        let path = self.dir.join(OWNER_FILE);
        notedown_cloud::fsutil::write_atomic(&path, &json_bytes(&Owner { user_id })?, false)?;
        Ok(())
    }

    /// Deletes the store of account `slug` in `local` as a whole: renames it to a tombstone
    /// (`pipeline/.removed-<slug>-<hex>`), fsyncs `pipeline/` and deletes the tombstone. Earlier
    /// tombstones are swept first. Returns whether a store was there; a missing store is not
    /// created. Only a failed rename is an error: once renamed, the store is gone, and a
    /// tombstone that cannot be deleted now is left to the next [`sweep`](Self::sweep). Works
    /// only below `<local>/pipeline`.
    pub fn destroy(local: &Path, slug: &str, w: &WriterLock) -> Result<bool> {
        notedown_cloud::auth::validate_slug(slug)?;
        TextStore::sweep(local, w);
        let pipeline = local.join(PIPELINE_DIR);
        let dir = pipeline.join(slug);
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err(&dir)(e)),
        }
        let tomb = pipeline.join(format!("{TOMBSTONE_PREFIX}{slug}-{}", tomb_hex()));
        notedown_cloud::fsutil::rename_durable(&dir, &tomb)?;
        let _ = remove_tombstone(&tomb);
        Ok(true)
    }

    /// Deletes every tombstone (`.removed-*`) under `<local>/pipeline` that a crash or a failed
    /// delete left. Errors only leave a tombstone for the next sweep. Returns how many went.
    pub fn sweep(local: &Path, w: &WriterLock) -> usize {
        TextStore::sweep_all(local, w).deleted
    }

    /// [`sweep`](Self::sweep), with the tombstones that could not be deleted and why.
    pub fn sweep_all(local: &Path, _w: &WriterLock) -> Swept {
        let mut swept = Swept::default();
        let Ok(entries) = std::fs::read_dir(local.join(PIPELINE_DIR)) else {
            return swept;
        };
        for e in entries.flatten() {
            if !e
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(TOMBSTONE_PREFIX))
            {
                continue;
            }
            match remove_tombstone(&e.path()) {
                Ok(()) => swept.deleted += 1,
                Err(err) => swept.failed.push((e.path(), err)),
            }
        }
        swept
    }

    /// `<data>/pipeline/<slug>`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    /// The file of note `key`: `notes/<FILE_ID>.json` or `notes/path-<md5>.json`. Any other
    /// key is refused, so no path is ever built from other text.
    fn note_path(&self, key: &str) -> Result<PathBuf> {
        if !notedown_cloud::pages::valid_key(key) {
            return Err(Error::Invalid {
                what: "note key",
                message: format!("{key:?}"),
            });
        }
        let name = match key.strip_prefix("path:") {
            Some(md5) => format!("path-{md5}.json"),
            None => format!("{key}.json"),
        };
        Ok(self.dir.join(NOTES_DIR).join(name))
    }

    fn bookmark_path(&self, consumer: &str) -> Result<PathBuf> {
        notedown_cloud::journal::validate_consumer(consumer)?;
        Ok(self
            .dir
            .join(CONSUMERS_DIR)
            .join(format!("{consumer}.json")))
    }

    /// Errors unless a repair has run since the open or the last failed append.
    fn writable(&self) -> Result<()> {
        if self.repaired.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::NeedsRepair)
        }
    }

    fn lock(&self) -> Result<std::fs::File> {
        lock_file(&self.dir.join(LOCK_FILE))
    }

    /// Cuts a torn last line of the log. Returns the length of the complete part and the seq
    /// of the last complete line.
    fn cut_torn(&self) -> Result<(u64, u64)> {
        let path = self.log_path();
        let (last, complete, len) = tail(&path)?;
        if len > complete {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(io_err(&path))?;
            f.set_len(complete).map_err(io_err(&path))?;
            f.sync_all().map_err(io_err(&path))?;
        }
        Ok((complete, last_seq_of(&path, last)?))
    }

    /// Crash repair at the writer's start: cuts a torn last line, then brings every note state
    /// up to the newest log lines of its pages: the lines of the note past its `log_seq` are
    /// applied in log order. Only what a log line holds is taken (text, `pageHash`, the
    /// strokes and layers hashes, truncation, position, deletions, path); the stroke list
    /// stays that of the version before, which only makes the next change region larger. A
    /// note with log lines and no state file (a crash before its first save) gets a state
    /// started from its lines, so its logged block ids are never given out again; a
    /// `note.moved` line with `oldNote` (a re-keyed note) starts it from the old key's state,
    /// and the old state file goes when it was saved before that line. Afterwards the store
    /// takes writes.
    pub fn repair(&self, _w: &WriterLock) -> Result<()> {
        self.repaired.store(false, Ordering::SeqCst);
        self.cut_torn()?;
        let mut notes: BTreeMap<String, (NoteState, NoteState)> = BTreeMap::new();
        for note in self.notes()? {
            notes.insert(note.key.clone(), (note.clone(), note));
        }
        // Old keys of re-keyed notes with the seq of their move line.
        let mut rekeyed: Vec<(String, String, u64)> = Vec::new();
        self.read_log(0, None, &mut |line| {
            if let Some(old) = &line.old_note {
                rekeyed.push((old.clone(), line.note.clone(), line.seq));
            }
            if !notes.contains_key(&line.note) && notedown_cloud::pages::valid_key(&line.note) {
                let before = NoteState {
                    key: line.note.clone(),
                    ..NoteState::default()
                };
                let start = match line.old_note.as_ref().and_then(|old| notes.get(old)) {
                    Some((_, old)) => NoteState {
                        key: line.note.clone(),
                        ..old.clone()
                    },
                    None => before.clone(),
                };
                notes.insert(line.note.clone(), (before, start));
            }
            if let Some((before, state)) = notes.get_mut(&line.note)
                && line.seq > before.log_seq
            {
                apply_line(state, line);
            }
            Ok(())
        })?;
        let mut gone = Vec::new();
        for (old, new, seq) in rekeyed {
            if notes.contains_key(&new)
                && notes
                    .get(&old)
                    .is_some_and(|(before, _)| before.log_seq < seq)
            {
                notes.remove(&old);
                gone.push(old);
            }
        }
        for (_, (before, after)) in notes {
            if before != after {
                self.write_note(&after)?;
            }
        }
        for old in gone {
            self.delete_note_file(&old)?;
        }
        self.repaired.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Removes the state file of note `key` (a note re-keyed by a move). Call it after the
    /// `note.moved` line and the save of the state under its new key.
    pub fn remove_note(&self, _w: &WriterLock, key: &str) -> Result<()> {
        self.writable()?;
        self.delete_note_file(key)
    }

    fn delete_note_file(&self, key: &str) -> Result<()> {
        let path = self.note_path(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => notedown_cloud::fsutil::fsync_dir(&self.dir.join(NOTES_DIR))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        Ok(())
    }

    /// Appends `lines` with one write: sets their `seq` (the next numbers) and, where it is 0,
    /// their `ts` (now). A torn last line is cut first. A failed append keeps every complete
    /// line (a reader may have seen it already, so its `seq` is never given out again) and
    /// blocks all writes until the next [`repair`](Self::repair) has applied it.
    pub fn append(&self, _w: &WriterLock, lines: &mut [LogLine]) -> Result<()> {
        self.writable()?;
        if lines.is_empty() {
            return Ok(());
        }
        // The account lock excludes a prune rewrite of the log (see `rewrite_log`).
        let _lock = self.lock()?;
        let result = self.append_lines(lines);
        if result.is_err() {
            self.repaired.store(false, Ordering::SeqCst);
            // Only a torn tail goes; complete lines stay. The next repair cuts it otherwise.
            let _ = self.cut_torn();
        }
        result
    }

    fn append_lines(&self, lines: &mut [LogLine]) -> Result<()> {
        let path = self.log_path();
        let (base, last) = self.cut_torn()?;
        let now = now_ms();
        let mut bytes = Vec::new();
        for (i, line) in lines.iter_mut().enumerate() {
            line.seq = last + 1 + i as u64;
            if line.ts == 0 {
                line.ts = now;
            }
            bytes.extend_from_slice(serde_json::to_string(line)?.as_bytes());
            bytes.push(b'\n');
        }
        let created = base == 0 && !path.exists();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io_err(&path))?;
        if created {
            notedown_cloud::fsutil::fsync_dir(&self.dir)?;
        }
        f.write_all(&bytes)
            .and_then(|()| f.sync_data())
            .map_err(io_err(&path))
    }

    /// Writes the state of a note atomically, with `log_seq` set to the last log line. Call it
    /// after the [`append`](Self::append) of its lines.
    pub fn save_note(&self, _w: &WriterLock, note: &NoteState) -> Result<()> {
        self.writable()?;
        self.write_note(note)
    }

    fn write_note(&self, note: &NoteState) -> Result<()> {
        let path = self.note_path(&note.key)?;
        let mut note = note.clone();
        note.log_seq = self.last_seq()?;
        notedown_cloud::fsutil::write_atomic(&path, &json_bytes(&note)?, false)?;
        Ok(())
    }

    /// Calls `each` for every complete log line with `from_exclusive < seq <= up_to`, in
    /// order. An error of `each` stops the read and is returned.
    pub fn read_log(
        &self,
        from_exclusive: u64,
        up_to: Option<u64>,
        each: &mut dyn FnMut(LogLine) -> Result<()>,
    ) -> Result<()> {
        let path = self.log_path();
        let f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(&path)(e)),
        };
        let mut reader = BufReader::new(f);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf).map_err(io_err(&path))? == 0 {
                return Ok(());
            }
            if buf.last() != Some(&b'\n') {
                // A torn or unfinished last line: not a line (yet).
                return Ok(());
            }
            buf.pop();
            let line: LogLine = serde_json::from_slice(&buf)
                .map_err(|e| corrupt(&path, format!("log line cannot be read: {e}")))?;
            if up_to.is_some_and(|u| line.seq > u) {
                return Ok(());
            }
            if line.seq > from_exclusive {
                each(line)?;
            }
        }
    }

    /// Rewrites the log without the lines `plan` names, atomically and under `.lock`. A torn
    /// last line is cut first. `plan` gets every complete line in order and returns the
    /// `seq` numbers to drop; kept lines are copied byte for byte, so no `seq` changes.
    /// Returns `(kept, removed)`. The log is left alone when `plan` names no line.
    pub fn rewrite_log(
        &self,
        _w: &WriterLock,
        plan: &mut dyn FnMut(&[LogLine]) -> Result<std::collections::BTreeSet<u64>>,
    ) -> Result<(usize, usize)> {
        self.writable()?;
        let _lock = self.lock()?;
        let (complete, _) = self.cut_torn()?;
        let path = self.log_path();
        let mut bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io_err(&path)(e)),
        };
        bytes.truncate(complete as usize);
        let mut raw: Vec<(LogLine, &[u8])> = Vec::new();
        for chunk in bytes.split_inclusive(|&b| b == b'\n') {
            let line: LogLine = serde_json::from_slice(&chunk[..chunk.len() - 1])
                .map_err(|e| corrupt(&path, format!("log line cannot be read: {e}")))?;
            raw.push((line, chunk));
        }
        let lines: Vec<LogLine> = raw.iter().map(|(l, _)| l.clone()).collect();
        let drop = plan(&lines)?;
        let removed = raw.iter().filter(|(l, _)| drop.contains(&l.seq)).count();
        if removed == 0 {
            return Ok((raw.len(), 0));
        }
        let mut out = Vec::with_capacity(bytes.len());
        for (line, chunk) in &raw {
            if !drop.contains(&line.seq) {
                out.extend_from_slice(chunk);
            }
        }
        notedown_cloud::fsutil::write_atomic(&path, &out, false)?;
        Ok((raw.len() - removed, removed))
    }

    /// The `seq` of the last complete log line, 0 for an empty log.
    pub fn last_seq(&self) -> Result<u64> {
        let path = self.log_path();
        let (last, _, _) = tail(&path)?;
        last_seq_of(&path, last)
    }

    /// The state of note `key`, `None` when it has none.
    pub fn note(&self, key: &str) -> Result<Option<NoteState>> {
        read_json(&self.note_path(key)?)
    }

    /// Every note state, sorted by file name.
    pub fn notes(&self) -> Result<Vec<NoteState>> {
        let mut out = Vec::new();
        for (_, path) in json_files(&self.dir.join(NOTES_DIR))? {
            if let Some(note) = read_json(&path)? {
                out.push(note);
            }
        }
        Ok(out)
    }

    /// The bookmark of `consumer`, `None` for an unknown consumer. A file that does not parse
    /// is an error, never a reset.
    pub fn bookmark(&self, consumer: &str) -> Result<Option<Bookmark>> {
        read_json(&self.bookmark_path(consumer)?)
    }

    /// Moves the bookmark of `consumer` to `up_to`, never backwards and never past the last
    /// log line. Runs under `.lock`.
    pub fn ack(&self, consumer: &str, up_to: u64) -> Result<Bookmark> {
        let path = self.bookmark_path(consumer)?;
        let _lock = self.lock()?;
        let old = read_json::<Bookmark>(&path)?.map_or(0, |b| b.seq);
        let bookmark = Bookmark {
            seq: old.max(up_to.min(self.last_seq()?)),
            updated_at: now_ms(),
        };
        notedown_cloud::fsutil::write_atomic(&path, &json_bytes(&bookmark)?, false)?;
        Ok(bookmark)
    }

    /// Every consumer with its bookmark, sorted by name.
    pub fn consumers(&self) -> Result<Vec<(String, Bookmark)>> {
        let mut out = Vec::new();
        for (name, path) in json_files(&self.dir.join(CONSUMERS_DIR))? {
            if notedown_cloud::journal::validate_consumer(&name).is_err() {
                continue;
            }
            if let Some(b) = read_json(&path)? {
                out.push((name, b));
            }
        }
        Ok(out)
    }

    /// Removes the bookmark of `consumer` under `.lock`; false when there was none.
    pub fn remove_consumer(&self, consumer: &str) -> Result<bool> {
        let path = self.bookmark_path(consumer)?;
        let _lock = self.lock()?;
        match std::fs::remove_file(&path) {
            Ok(()) => {
                notedown_cloud::fsutil::fsync_dir(&self.dir.join(CONSUMERS_DIR))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// The stats, zero when none were written yet.
    pub fn stats(&self) -> Result<Stats> {
        Ok(read_json(&self.dir.join(STATS_FILE))?.unwrap_or_default())
    }

    fn update_stats(&self, change: impl FnOnce(&mut Stats)) -> Result<Stats> {
        self.writable()?;
        let mut stats = self.stats()?;
        change(&mut stats);
        notedown_cloud::fsutil::write_atomic(
            &self.dir.join(STATS_FILE),
            &json_bytes(&stats)?,
            false,
        )?;
        Ok(stats)
    }

    /// Adds `n` to `region_missed`.
    pub fn add_region_missed(&self, _w: &WriterLock, n: u64) -> Result<Stats> {
        self.update_stats(|s| s.region_missed += n)
    }

    /// Sets `last_run_ms`.
    pub fn set_last_run(&self, _w: &WriterLock, ms: u64) -> Result<Stats> {
        self.update_stats(|s| s.last_run_ms = ms)
    }
}

fn last_seq_of(path: &Path, last: Option<Vec<u8>>) -> Result<u64> {
    match last {
        None => Ok(0),
        Some(line) => serde_json::from_slice::<SeqOnly>(&line)
            .map(|l| l.seq)
            .map_err(|e| corrupt(path, format!("last log line cannot be read: {e}"))),
    }
}

/// The number of a block id `b<N>`.
fn block_number(id: &str) -> Option<u64> {
    id.strip_prefix('b')?.parse().ok()
}

/// Applies a log line the state has not seen yet (its `seq` is past the state's `log_seq`).
/// Lines are replayed in log order, so the newest one wins.
fn apply_line(state: &mut NoteState, mut line: LogLine) {
    if let Some(path) = line.path.take() {
        state.title = title_of(&path);
        state.path = path;
    }
    if line.change.is_note_change() {
        // `note.moved` set the path above. Leaving the scope is not a deletion (spec 4.5).
        match line.change {
            LineChange::NoteDeleted => state.deleted = true,
            LineChange::NoteUnscoped => state.unscoped = true,
            _ => {}
        }
        return;
    }
    let Some(page_id) = line.page_id.as_deref() else {
        return;
    };
    let index = state.pages.iter().position(|p| p.page_id == page_id);
    match line.change {
        LineChange::Deleted => {
            if let Some(i) = index {
                state.pages.remove(i);
            }
        }
        LineChange::Moved => {
            if let (Some(i), Some(pos)) = (index, line.position) {
                state.pages[i].position = pos;
            }
        }
        _ => {
            let i = match index {
                Some(i) => i,
                None => {
                    state.pages.push(PageState {
                        page_id: page_id.to_string(),
                        position: line.position.unwrap_or(state.pages.len()),
                        ..PageState::default()
                    });
                    state.pages.len() - 1
                }
            };
            for b in &line.blocks {
                if let Some(n) = block_number(&b.id) {
                    state.next_block = state.next_block.max(n + 1);
                }
                for old in &b.replaces {
                    let next = state.successors.entry(old.clone()).or_default();
                    if !next.contains(&b.id) {
                        next.push(b.id.clone());
                    }
                }
            }
            let page = &mut state.pages[i];
            page.seq = Some(line.seq);
            if let Some(model) = line.model.take() {
                state.model = Some(model);
            }
            page.state = if line.truncated {
                PageOcrState::Truncated
            } else {
                PageOcrState::Done
            };
            page.blocks = line.blocks;
            if let Some(hash) = line.page_hash {
                page.page_hash = hash;
            }
            if let Some(hash) = line.strokes_hash {
                page.strokes_hash = hash;
            }
            if let Some(hash) = line.layers_hash {
                page.layers_hash = hash;
            }
            if let Some(pos) = line.position {
                page.position = pos;
            }
            state.deleted = false;
            state.unscoped = false;
        }
    }
    state.pages.sort_by_key(|p| p.position);
}

#[cfg(test)]
mod tests;
