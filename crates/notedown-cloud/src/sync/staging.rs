//! Upload staging: `accounts/<slug>/staging/<innerName>/` holds the bytes of one upload until
//! `upload/finish` moves them into `files/` (`docs/sync-protocol.md` 3.5).
//!
//! - `data`: the complete file. A single upload (`/api/oss/upload`) writes it directly; a
//!   chunked upload writes it when the last part arrives.
//! - `part-<n>`: one part of a chunked upload (`/api/oss/upload/part`, `n` from 1).
//! - `upload.json`: `{"uploadId", "totalChunks"}` of a chunked upload, written with its first
//!   part. A part with another `uploadId` or `totalChunks` is refused.
//!
//! The entry is named by the signed `innerName` (a UUID from `upload/apply`), never by a value
//! the device chose: `uploadId` is not signed, and `upload/finish` names only `innerName`.
//! Every file is written to a temp file first (fsync, rename), so a cut transfer never counts
//! as a part or as data. Staged data is never listed (everything outside `files/` is hidden).
//! [`sweep`] removes entries older than [`STAGING_MAX_AGE_MS`] on start and every hour.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::store::create_temp;
use crate::{Error, Result, io_err};

/// `accounts/<slug>/staging/`.
pub const STAGING_DIR: &str = "staging";
/// The complete staged file of an entry.
pub const DATA_FILE: &str = "data";
/// The chunk state of an entry.
pub const META_FILE: &str = "upload.json";
/// Staging entries untouched for longer than this are removed (ms).
pub const STAGING_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
/// How often the server sweeps staging.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The largest part of a chunked upload (the device sends 8 MiB).
pub const PART_MAX: u64 = 9 * 1024 * 1024;
/// The largest file (`FILE_MAX` of Ratta's server, `docs/sync-protocol.md` 3.3).
pub const FILE_MAX: u64 = 1 << 30;
/// The most parts one upload may announce.
pub const MAX_CHUNKS: u64 = 4096;
/// Bytes buffered in memory before a blocking write.
const WRITE_STEP: usize = 256 * 1024;

/// `chunk status` until all parts are there.
pub const STATUS_UPLOADING: &str = "uploading";
/// `chunk status` once the parts are merged into `data`.
pub const STATUS_MERGED: &str = "merged";

/// A UUID in its 36-character text form (lowercase or uppercase hex), as `upload/apply`
/// issues for `innerName`.
pub fn valid_inner_name(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// An `uploadId` as the device sends it (`<ms>_<uuid>`): 1 to 128 of `A-Z a-z 0-9 _ -`.
pub fn valid_upload_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `accounts/<slug>/staging` below an account folder.
pub fn staging_dir(account_dir: &Path) -> PathBuf {
    account_dir.join(STAGING_DIR)
}

/// The entry of `inner` (which must be a [`valid_inner_name`]).
pub fn entry_dir(account_dir: &Path, inner: &str) -> Result<PathBuf> {
    if !valid_inner_name(inner) {
        return Err(Error::Invalid {
            what: "innerName",
            message: format!("{inner:?} is not an upload name"),
        });
    }
    Ok(staging_dir(account_dir).join(inner))
}

/// `part-<n>` of an entry.
pub fn part_path(entry: &Path, n: u64) -> PathBuf {
    entry.join(format!("part-{n}"))
}

/// Create the entry folder (and `staging/`) with fsyncs of the new folders' parents.
pub fn create_entry(entry: &Path) -> Result<()> {
    let staging = entry.parent().unwrap_or(entry);
    for dir in [staging, entry] {
        match std::fs::create_dir(dir) {
            Ok(()) => crate::fsutil::fsync_dir(dir.parent().unwrap_or(dir))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(dir)(e)),
        }
    }
    Ok(())
}

/// Remove an entry and everything in it (a missing one is fine).
pub fn remove_entry(entry: &Path) {
    match std::fs::remove_dir_all(entry) {
        Ok(()) => {
            if let Some(parent) = entry.parent() {
                let _ = crate::fsutil::fsync_dir(parent);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "notedown cloud: cannot remove the staging entry {}: {e}",
            entry.display()
        ),
    }
}

/// Bytes streamed into a temp file of a staging entry: buffered in memory up to 256 KiB, then
/// written (and hashed) in a blocking task. [`StreamFile::finish`] makes it durable under its
/// final name; dropping it without `finish` leaves a temp file that [`StreamFile::abort`] or the
/// sweep removes.
pub struct StreamFile {
    file: Option<std::fs::File>,
    hasher: Option<Md5>,
    buf: Vec<u8>,
    tmp: PathBuf,
    len: u64,
}

impl StreamFile {
    /// A new temp file in `dir` (blocking; the folder must exist).
    pub fn create(dir: &Path) -> Result<StreamFile> {
        let (tmp, file) = create_temp(dir).map_err(io_err(dir))?;
        Ok(StreamFile {
            file: Some(file),
            hasher: Some(Md5::new()),
            buf: Vec::with_capacity(WRITE_STEP),
            tmp,
            len: 0,
        })
    }

    /// Bytes received so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub async fn push(&mut self, bytes: &[u8]) -> Result<()> {
        self.len += bytes.len() as u64;
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= WRITE_STEP {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let buf = std::mem::replace(&mut self.buf, Vec::with_capacity(WRITE_STEP));
        let (Some(mut file), Some(mut hasher)) = (self.file.take(), self.hasher.take()) else {
            return Err(io_err(&self.tmp)(std::io::Error::other(
                "the file is closed",
            )));
        };
        let tmp = self.tmp.clone();
        let (file, hasher) = tokio::task::spawn_blocking(move || -> Result<_> {
            file.write_all(&buf).map_err(io_err(&tmp))?;
            hasher.update(&buf);
            Ok((file, hasher))
        })
        .await
        .map_err(|e| io_err(&self.tmp)(std::io::Error::other(e.to_string())))??;
        self.file = Some(file);
        self.hasher = Some(hasher);
        Ok(())
    }

    /// Write the rest, fsync, rename onto `target`, fsync the folder. Returns the size and the
    /// lowercase hex MD5.
    pub async fn finish(mut self, target: PathBuf) -> Result<(u64, String)> {
        self.flush().await?;
        let (Some(file), Some(hasher)) = (self.file.take(), self.hasher.take()) else {
            return Err(io_err(&self.tmp)(std::io::Error::other(
                "the file is closed",
            )));
        };
        let tmp = self.tmp.clone();
        let len = self.len;
        tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
            file.sync_all().map_err(io_err(&tmp))?;
            drop(file);
            crate::fsutil::rename_durable(&tmp, &target)?;
            Ok((len, const_hex::encode(hasher.finalize())))
        })
        .await
        .map_err(|e| io_err(&self.tmp)(std::io::Error::other(e.to_string())))?
    }

    /// Drop the bytes and remove the temp file.
    pub fn abort(mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.tmp);
    }

    /// The temp file (for a caller that renames it itself under a lock).
    pub async fn close(mut self) -> Result<(PathBuf, u64, String)> {
        self.flush().await?;
        let (Some(file), Some(hasher)) = (self.file.take(), self.hasher.take()) else {
            return Err(io_err(&self.tmp)(std::io::Error::other(
                "the file is closed",
            )));
        };
        let tmp = self.tmp.clone();
        let len = self.len;
        tokio::task::spawn_blocking(move || -> Result<()> {
            file.sync_all().map_err(io_err(&tmp))
        })
        .await
        .map_err(|e| io_err(&self.tmp)(std::io::Error::other(e.to_string())))??;
        Ok((self.tmp.clone(), len, const_hex::encode(hasher.finalize())))
    }
}

/// `upload.json` of a chunked upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadMeta {
    pub upload_id: String,
    pub total_chunks: u64,
}

/// What became of a received part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartOutcome {
    /// Stored; parts are still missing.
    Uploading,
    /// All parts were there and are merged into `data` (now or by an earlier request).
    Merged,
    /// The part does not belong to this upload (another `uploadId` or `totalChunks`).
    Mismatch,
    /// The merged file would be larger than the limit; the parts are removed.
    TooLarge,
}

/// Serializes the "store the part, merge when complete" step per staging entry, so two last
/// parts arriving together merge once.
#[derive(Debug, Default)]
pub struct StagingLocks {
    held: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl StagingLocks {
    fn get(&self, entry: &Path) -> Arc<Mutex<()>> {
        let mut map = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 1024 {
            map.retain(|_, m| Arc::strong_count(m) > 1);
        }
        map.entry(entry.to_path_buf()).or_default().clone()
    }

    /// Store the closed temp file `tmp` as part `n` of `total` of the entry and merge when every
    /// part is present (blocking). A part that arrives after the merge is dropped (`Merged`).
    pub fn store_part(
        &self,
        entry: &Path,
        tmp: &Path,
        n: u64,
        meta: &UploadMeta,
        file_max: u64,
    ) -> Result<PartOutcome> {
        let lock = self.get(entry);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let drop_tmp = || {
            let _ = std::fs::remove_file(tmp);
        };
        let meta_path = entry.join(META_FILE);
        match std::fs::read(&meta_path) {
            Ok(bytes) => match serde_json::from_slice::<UploadMeta>(&bytes) {
                Ok(m) if m == *meta => {}
                _ => {
                    drop_tmp();
                    return Ok(PartOutcome::Mismatch);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if entry.join(DATA_FILE).exists() {
                    // A single upload went to this entry already.
                    drop_tmp();
                    return Ok(PartOutcome::Mismatch);
                }
                crate::store::write_atomic_short(&meta_path, &crate::fsutil::json_bytes(meta))?;
            }
            Err(e) => return Err(io_err(&meta_path)(e)),
        }
        if entry.join(DATA_FILE).exists() {
            drop_tmp();
            return Ok(PartOutcome::Merged);
        }
        crate::fsutil::rename_durable(tmp, &part_path(entry, n))?;
        let mut total = 0u64;
        for i in 1..=meta.total_chunks {
            match std::fs::metadata(part_path(entry, i)) {
                Ok(m) => total += m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(PartOutcome::Uploading);
                }
                Err(e) => return Err(io_err(&part_path(entry, i))(e)),
            }
        }
        if total > file_max {
            for i in 1..=meta.total_chunks {
                let _ = std::fs::remove_file(part_path(entry, i));
            }
            return Ok(PartOutcome::TooLarge);
        }
        merge(entry, meta.total_chunks)?;
        Ok(PartOutcome::Merged)
    }
}

/// Concatenate `part-1..=part-N` into `data` (temp file, fsync, rename, fsync), then remove the
/// parts.
fn merge(entry: &Path, total: u64) -> Result<()> {
    let (tmp, mut out) = create_temp(entry).map_err(io_err(entry))?;
    let written = (|| -> std::io::Result<()> {
        for i in 1..=total {
            let mut part = std::fs::File::open(part_path(entry, i))?;
            std::io::copy(&mut part, &mut out)?;
        }
        out.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(entry)(e));
    }
    crate::fsutil::rename_durable(&tmp, &entry.join(DATA_FILE))?;
    for i in 1..=total {
        let _ = std::fs::remove_file(part_path(entry, i));
    }
    crate::fsutil::fsync_dir(entry)
}

/// The newest mtime (ms) of a path and, for a folder, of its direct children.
fn newest_ms(path: &Path) -> Option<u64> {
    let ms = |m: &std::fs::Metadata| {
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    };
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut newest = ms(&meta)?;
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for e in entries.flatten() {
            if let Some(t) = e.metadata().ok().as_ref().and_then(ms) {
                newest = newest.max(t);
            }
        }
    }
    Some(newest)
}

/// Remove every staging entry of every account that was not touched for longer than
/// [`STAGING_MAX_AGE_MS`] before `now_ms` (by the newest mtime of the entry and its files).
/// Returns how many were removed. An unreadable account is skipped.
pub fn sweep(data_dir: &Path, now_ms: u64) -> Result<usize> {
    let accounts = data_dir.join("accounts");
    let list = match std::fs::read_dir(&accounts) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(&accounts)(e)),
    };
    let mut removed = 0;
    for account in list.flatten() {
        let staging = account.path().join(STAGING_DIR);
        let Ok(entries) = std::fs::read_dir(&staging) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(newest) = newest_ms(&path) else {
                continue;
            };
            if now_ms.saturating_sub(newest) <= STAGING_MAX_AGE_MS {
                continue;
            }
            let gone = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match gone {
                Ok(()) => removed += 1,
                Err(e) => eprintln!(
                    "notedown cloud: cannot remove the stale staging entry {}: {e}",
                    path.display()
                ),
            }
        }
    }
    Ok(removed)
}
