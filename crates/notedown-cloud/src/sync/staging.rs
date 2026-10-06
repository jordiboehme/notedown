//! Upload staging: `accounts/<slug>/staging/<innerName>/` holds the bytes of one upload until
//! `upload/finish` moves them into `files/` (`docs/sync-protocol.md` 3.5).
//!
//! - `data`: the complete file. A single upload (`/api/oss/upload`) writes it directly; a
//!   chunked upload writes it when the last part arrives.
//! - `part-<n>`: one part of a chunked upload (`/api/oss/upload/part`, `n` from 1).
//! - `upload.json`: `{"parent", "uploadId", "totalChunks"}`, written with the first bytes: the
//!   parent folder signed at apply, and for a chunked upload its id and part count. A part with
//!   another `uploadId` or `totalChunks` is refused, and so is a second single upload.
//! - `finishing-<opId>`: the data, renamed by `upload/finish` before it verifies and commits
//!   it, so no other request can change the bytes in between.
//!
//! - `apply.json`: `{"device", "parent", "fileName", "appliedAtMs", "seq"}`, written by
//!   `upload/apply`, so `upload/finish` can find the entry by its file name.
//!
//! The entry is named by an internal id (a UUID that `upload/apply` signs into the upload URLs),
//! never by a value the device chose: `uploadId` is not signed. `upload/apply` answers
//! `innerName` = `fileName`, as in Ratta's traffic, so `upload/finish` may name the upload by
//! its file name ([`find_applied`]: the latest apply of this device for this parent folder and
//! name wins) or by the internal id.
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

/// The apply record of an entry.
pub const APPLY_FILE: &str = "apply.json";

/// `apply.json`: who applied for the upload, for which parent folder and name, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyRecord {
    pub device: String,
    pub parent: String,
    pub file_name: String,
    pub applied_at_ms: u64,
    /// Orders two applies in the same millisecond (within one server run).
    pub seq: u64,
}

static APPLY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write the apply record of a new upload (blocking): creates the entry folder and
/// `apply.json`.
pub fn record_apply(
    entry: &Path,
    device: &str,
    parent: &str,
    file_name: &str,
    now_ms: u64,
) -> Result<()> {
    create_entry(entry)?;
    let record = ApplyRecord {
        device: device.to_string(),
        parent: parent.to_string(),
        file_name: file_name.to_string(),
        applied_at_ms: now_ms,
        seq: APPLY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    };
    crate::store::write_atomic_short(&entry.join(APPLY_FILE), &crate::fsutil::json_bytes(&record))
}

/// The entry of the latest `upload/apply` of `device` for `file_name` in the folder `parent`
/// (the form `upload/finish` uses when its `innerName` is the file name). `None` when there is
/// none.
pub fn find_applied(
    account_dir: &Path,
    device: &str,
    parent: &str,
    file_name: &str,
) -> Result<Option<PathBuf>> {
    let dir = staging_dir(account_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(&dir)(e)),
    };
    let mut best: Option<((u64, u64), PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !valid_inner_name(&name.to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        let Ok(bytes) = std::fs::read(path.join(APPLY_FILE)) else {
            continue;
        };
        let Ok(r) = serde_json::from_slice::<ApplyRecord>(&bytes) else {
            continue;
        };
        if r.device != device || r.parent != parent || r.file_name != file_name {
            continue;
        }
        let key = (r.applied_at_ms, r.seq);
        if best.as_ref().is_none_or(|(k, _)| key > *k) {
            best = Some((key, path));
        }
    }
    Ok(best.map(|(_, p)| p))
}

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
/// written (and hashed) in a blocking task. [`StreamFile::close`] makes it durable (the caller
/// renames it under the entry's lock); dropping it leaves a temp file that
/// [`StreamFile::abort`] or the sweep removes.
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

    /// Drop the bytes and remove the temp file.
    pub fn abort(mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.tmp);
    }

    /// Write the rest and fsync. Returns the temp file (the caller renames it under the entry's
    /// lock), the size and the lowercase hex MD5. On an error the temp file is removed.
    pub async fn close(self) -> Result<(PathBuf, u64, String)> {
        let tmp = self.tmp.clone();
        let r = self.close_inner().await;
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    }

    async fn close_inner(mut self) -> Result<(PathBuf, u64, String)> {
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

/// `upload.json` of a staging entry, written under the entry's lock by the first request that
/// sends bytes: the parent folder signed at `upload/apply` (finish must name the same one), and
/// for a chunked upload its `uploadId` and `totalChunks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadMeta {
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_chunks: Option<u64>,
    /// A single upload: the MD5 of its bytes, so a retry of the same bytes (after a lost
    /// answer) is recognized and answered again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
}

/// What became of a single upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleOutcome {
    /// The bytes are the entry's `data` now.
    Stored,
    /// The entry already holds these bytes (a retry): nothing changed.
    Same,
    /// The entry holds other bytes or parts: refused, nothing changed.
    Refused,
}

/// What became of a received part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartOutcome {
    /// Stored; parts are still missing.
    Uploading,
    /// All parts were there and are merged into `data` (now or by an earlier request).
    Merged,
    /// The part does not belong to this upload (another `uploadId` or `totalChunks`, or the
    /// entry holds a single upload).
    Mismatch,
    /// The merged file would be larger than the limit; the parts are removed.
    TooLarge,
}

/// What `upload/finish` found in a staging entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// No complete data (unknown upload, or parts missing). The entry is kept.
    Missing,
    /// The finish names another parent folder than the one signed at apply. The entry is
    /// removed.
    ParentMismatch,
    /// The data now belongs to this finish alone, under this name.
    Claimed(PathBuf),
}

/// The name of data claimed by a finish: `finishing-<opId>`.
fn finishing_path(entry: &Path, op_id: &str) -> PathBuf {
    entry.join(format!("finishing-{op_id}"))
}

/// Whether a finish claimed the data of the entry already.
fn is_finishing(entry: &Path) -> bool {
    std::fs::read_dir(entry).is_ok_and(|d| {
        d.flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("finishing-"))
    })
}

fn read_meta(entry: &Path) -> Result<Option<UploadMeta>> {
    let path = entry.join(META_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&path)(e)),
    }
}

fn write_meta(entry: &Path, meta: &UploadMeta) -> Result<()> {
    crate::store::write_atomic_short(&entry.join(META_FILE), &crate::fsutil::json_bytes(meta))
}

/// Serializes every change of one staging entry: the single upload's `data`, storing a part and
/// merging (two last parts arriving together merge once), and the claim of the data by
/// `upload/finish`.
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

    /// Run `f` under the entry's lock (blocking).
    fn with<T>(&self, entry: &Path, f: impl FnOnce() -> T) -> T {
        let lock = self.get(entry);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    /// Make the closed temp file `tmp` (with MD5 `md5`) the `data` of a single upload
    /// (blocking). The temp file is gone afterwards in every case, errors included.
    ///
    /// - A fresh entry: `upload.json` (with the MD5) is written, then `tmp` becomes `data`. If
    ///   that fails, `upload.json` is removed again, so the URL stays usable.
    /// - An entry with a single upload of the same MD5 (still `data`, or claimed by a finish):
    ///   [`SingleOutcome::Same`], the retry of a lost answer.
    /// - Anything else (other bytes, parts): [`SingleOutcome::Refused`]. Bytes a finish
    ///   verifies can never change.
    pub fn store_single(
        &self,
        entry: &Path,
        tmp: &Path,
        parent: &str,
        md5: &str,
    ) -> Result<SingleOutcome> {
        let drop_tmp = || {
            let _ = std::fs::remove_file(tmp);
        };
        let r = self.with(entry, || {
            if let Some(meta) = read_meta(entry)? {
                let same = meta.upload_id.is_none()
                    && meta.md5.as_deref() == Some(md5)
                    && (entry.join(DATA_FILE).is_file() || is_finishing(entry));
                return Ok(if same {
                    SingleOutcome::Same
                } else {
                    SingleOutcome::Refused
                });
            }
            let used = std::fs::read_dir(entry)
                .map_err(io_err(entry))?
                .flatten()
                .any(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    !(name.starts_with(".nd-") && name.ends_with(".tmp")) && name != APPLY_FILE
                });
            if used {
                return Ok(SingleOutcome::Refused);
            }
            let meta = UploadMeta {
                parent: parent.to_string(),
                upload_id: None,
                total_chunks: None,
                md5: Some(md5.to_string()),
            };
            write_meta(entry, &meta)?;
            if let Err(e) = crate::fsutil::rename_durable(tmp, &entry.join(DATA_FILE)) {
                let _ = std::fs::remove_file(entry.join(META_FILE));
                let _ = crate::fsutil::fsync_dir(entry);
                return Err(e);
            }
            Ok(SingleOutcome::Stored)
        });
        drop_tmp();
        r
    }

    /// Give claimed data back after a finish failed without committing it (blocking): rename
    /// `claimed` to `data` again, so the finish can be retried. Nothing happens when the file is
    /// gone (the commit moved it) or `data` exists.
    pub fn release(&self, entry: &Path, claimed: &Path) -> Result<()> {
        self.with(entry, || {
            let data = entry.join(DATA_FILE);
            if claimed.is_file() && !data.exists() {
                crate::fsutil::rename_durable(claimed, &data)?;
            }
            Ok(())
        })
    }

    /// Store the closed temp file `tmp` as part `n` of the entry and merge when every part is
    /// present (blocking). A part that arrives after the merge is dropped (`Merged`).
    pub fn store_part(
        &self,
        entry: &Path,
        tmp: &Path,
        n: u64,
        meta: &UploadMeta,
        file_max: u64,
    ) -> Result<PartOutcome> {
        self.with(entry, || {
            let drop_tmp = || {
                let _ = std::fs::remove_file(tmp);
            };
            let total_chunks = meta.total_chunks.unwrap_or(0);
            match read_meta(entry)? {
                Some(m) if m == *meta => {}
                Some(_) => {
                    drop_tmp();
                    return Ok(PartOutcome::Mismatch);
                }
                None => {
                    if entry.join(DATA_FILE).exists() || is_finishing(entry) {
                        drop_tmp();
                        return Ok(PartOutcome::Mismatch);
                    }
                    write_meta(entry, meta)?;
                }
            }
            if entry.join(DATA_FILE).exists() || is_finishing(entry) {
                drop_tmp();
                return Ok(PartOutcome::Merged);
            }
            crate::fsutil::rename_durable(tmp, &part_path(entry, n))?;
            let mut total = 0u64;
            for i in 1..=total_chunks {
                match std::fs::metadata(part_path(entry, i)) {
                    Ok(m) => total += m.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(PartOutcome::Uploading);
                    }
                    Err(e) => return Err(io_err(&part_path(entry, i))(e)),
                }
            }
            if total > file_max {
                for i in 1..=total_chunks {
                    let _ = std::fs::remove_file(part_path(entry, i));
                }
                return Ok(PartOutcome::TooLarge);
            }
            merge(entry, total_chunks)?;
            Ok(PartOutcome::Merged)
        })
    }

    /// Give the entry's `data` to one `upload/finish` (blocking): check that `parent` is the
    /// folder signed at apply, then rename `data` to `finishing-<op_id>`. From then on no upload
    /// request can change those bytes (they refuse an entry with `upload.json`), and a second
    /// finish finds no `data`.
    pub fn claim(&self, entry: &Path, parent: &str, op_id: &str) -> Result<Claim> {
        self.with(entry, || {
            let Some(meta) = read_meta(entry)? else {
                return Ok(Claim::Missing);
            };
            if meta.parent != parent {
                remove_entry(entry);
                return Ok(Claim::ParentMismatch);
            }
            let data = entry.join(DATA_FILE);
            if !std::fs::symlink_metadata(&data).is_ok_and(|m| m.is_file()) {
                return Ok(Claim::Missing);
            }
            let claimed = finishing_path(entry, op_id);
            crate::fsutil::rename_durable(&data, &claimed)?;
            Ok(Claim::Claimed(claimed))
        })
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
