//! The recycle bin `accounts/<slug>/recycle/`: old versions and deleted files, so every
//! overwrite and every delete can be undone by hand.
//!
//! Layout:
//!
//! - `recycle/<deletedAt>/<wire path>`: the bytes (or the folder) as they were. `deletedAt` is
//!   ms since the epoch; [`Store::recycle_slot`] picks a later ms when that path is taken, so a
//!   slot is never reused.
//! - `recycle/index.jsonl`: one line per recycled item, [`RecycleEntry`]
//!   `{"id","path","md5","deletedAt","reason"}`. The slot of a line is
//!   `recycle/<deletedAt>/<path>` ([`RecycleEntry::location`]).
//!
//! Use inside the commit protocol ([`super::commit`]), under the store lock:
//!
//! 1. While planning, pick the slot with [`Store::recycle_slot`] and put the [`RecycleEntry`]
//!    into the intent (`PendingOp::Put { recycle }` does this for an overwrite).
//! 2. In the apply step, put the bytes into the slot: [`Store::copy_to_recycle`] (an overwrite:
//!    the file stays listed until the new bytes replace it) or [`Store::move_to_recycle`] (a
//!    delete).
//! 3. After the apply step (where the registry is updated, also on recovery), append the index
//!    line with [`Store::record_recycled`]. It is idempotent: a line that is already the last
//!    one is not written again, so recovery can call it after a crash at any point.
//!
//! A commit that is discarded (not applied) removes its copy with
//! [`Store::discard_recycled`]; no line was written for it.
//!
//! [`Store::prune_recycle`] applies the retention ([`super::retention`]): it journals a
//! `recycle.pruned` event per removed line, rewrites the index atomically and then removes the
//! slots. A line it can't read stays as it is.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::commit::RECYCLE_DIR;
use super::retention::Retention;
use super::{Store, StoreGuard, WirePath};
use crate::journal::{Event, EventBody, Journal, Origin, new_op_id};
use crate::{Error, Result, io_err};

/// `recycle/index.jsonl`.
pub const RECYCLE_INDEX: &str = "index.jsonl";

/// How far back from the end of the index the last line is searched (a line holds one path).
const TAIL_BYTES: u64 = 64 * 1024;

/// One line of the index as read: parsed, or kept verbatim when it doesn't parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexLine {
    Entry(RecycleEntry),
    Raw(String),
}

/// What a prune did (or would do, for a dry run).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// The removed lines whose slots are gone, in index order. A line whose slot couldn't be
    /// removed is only in `failed`.
    pub removed: Vec<RecycleEntry>,
    /// The size of the removed files (a missing slot or a folder counts 0).
    pub removed_bytes: u64,
    /// Readable lines that stay.
    pub kept: usize,
    /// Lines that stay because they can't be read (or name a slot outside `recycle/`).
    pub unreadable_kept: usize,
    /// Slots that couldn't be removed (their lines are gone all the same), lines kept because
    /// of a symlink in or below their slot path, and a symlinked `recycle/` (one entry, nothing
    /// pruned). The prune goes on after each.
    pub failed: Vec<PruneFailure>,
}

/// A slot the prune couldn't remove, or a line it kept because of its slot path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneFailure {
    /// The slot, relative to `recycle/` ([`RecycleEntry::location`]); empty for `recycle/`
    /// itself.
    pub location: String,
    /// What went wrong.
    pub message: String,
}

/// Why something is in the recycle bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecycleReason {
    /// An older version replaced by an upload (protect mode).
    Overwritten,
    /// A deleted file or folder.
    Deleted,
}

/// One line of `recycle/index.jsonl`. Field order is the line order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleEntry {
    /// The store id the item had.
    pub id: u64,
    /// Its wire path (`NOTE/Note/x.note`).
    pub path: String,
    /// The MD5 of the recycled bytes (empty for a folder).
    pub md5: String,
    /// ms since the epoch; also the first segment of the slot.
    pub deleted_at: u64,
    pub reason: RecycleReason,
}

impl RecycleEntry {
    /// The slot, relative to `recycle/`: `<deletedAt>/<path>`.
    pub fn location(&self) -> String {
        format!("{}/{}", self.deleted_at, self.path)
    }

    fn line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("plain struct serializes");
        line.push('\n');
        line
    }
}

impl Store {
    /// `accounts/<slug>/recycle`.
    pub fn recycle_dir(&self) -> PathBuf {
        self.account_dir.join(RECYCLE_DIR)
    }

    /// Where the slot of `entry` is on disk. The path is checked again (it may come from an
    /// intent file).
    pub fn recycle_path(&self, entry: &RecycleEntry) -> Result<PathBuf> {
        let path = WirePath::parse_named(&entry.path)?;
        Ok(path.to_fs(&self.recycle_dir().join(entry.deleted_at.to_string())))
    }

    /// The first ms at or after `at_ms` whose slot `recycle/<ms>/<path>` is free.
    pub fn recycle_slot(&self, guard: &StoreGuard, path: &WirePath, at_ms: u64) -> Result<u64> {
        self.check_guard(guard)?;
        let mut ms = at_ms;
        loop {
            let slot = path.to_fs(&self.recycle_dir().join(ms.to_string()));
            if std::fs::symlink_metadata(&slot).is_err() {
                return Ok(ms);
            }
            ms += 1;
        }
    }

    /// Copy the file `from` into the slot of `entry` durably (temp file, fsync, rename, fsync
    /// of the folder). The source stays where it is.
    pub fn copy_to_recycle(
        &self,
        guard: &StoreGuard,
        from: &Path,
        entry: &RecycleEntry,
    ) -> Result<()> {
        self.check_guard(guard)?;
        self.copy_into_recycle(from, entry)
    }

    pub(super) fn copy_into_recycle(&self, from: &Path, entry: &RecycleEntry) -> Result<()> {
        let target = self.recycle_path(entry)?;
        let dir = target.parent().unwrap_or(&self.account_dir).to_path_buf();
        self.create_dirs_durable(&dir)?;
        let (tmp, mut out) = super::create_temp(&dir).map_err(io_err(&dir))?;
        let copied = (|| -> std::io::Result<()> {
            let mut input = std::fs::File::open(from)?;
            std::io::copy(&mut input, &mut out)?;
            out.sync_all()?;
            std::fs::rename(&tmp, &target)
        })();
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err(&target)(e));
        }
        crate::fsutil::fsync_dir(&dir)
    }

    /// Move the file or folder `from` into the slot of `entry` durably (the slot's folders are
    /// created and synced, then `rename_durable`).
    pub fn move_to_recycle(
        &self,
        guard: &StoreGuard,
        from: &Path,
        entry: &RecycleEntry,
    ) -> Result<()> {
        self.check_guard(guard)?;
        self.move_into_recycle(from, entry)
    }

    pub(super) fn move_into_recycle(&self, from: &Path, entry: &RecycleEntry) -> Result<()> {
        let target = self.recycle_path(entry)?;
        let dir = target.parent().unwrap_or(&self.account_dir).to_path_buf();
        self.create_dirs_durable(&dir)?;
        crate::fsutil::rename_durable(from, &target)
    }

    /// Append the index line of `entry` (write, fsync) unless it is the last line already.
    /// A torn last line (a crash while writing it) is cut first. Returns whether a line was
    /// written.
    pub fn record_recycled(&self, guard: &StoreGuard, entry: &RecycleEntry) -> Result<bool> {
        self.check_guard(guard)?;
        self.append_recycled(entry)
    }

    pub(super) fn append_recycled(&self, entry: &RecycleEntry) -> Result<bool> {
        self.append_recycled_all(std::slice::from_ref(entry))
    }

    /// Append the index lines of `entries` in one write and one fsync, idempotently: when the
    /// index already ends with the first `k` of them (a crash after a part or all of the write
    /// reached the disk), only the rest is written. A torn last line is cut first. Returns
    /// whether a line was written. Lines name their slot (`deletedAt` and path), and a slot is
    /// never reused, so an earlier line never matches by chance.
    pub(super) fn append_recycled_all(&self, entries: &[RecycleEntry]) -> Result<bool> {
        if entries.is_empty() {
            return Ok(false);
        }
        let dir = self.recycle_dir();
        self.create_dirs_durable(&dir)?;
        let path = dir.join(RECYCLE_INDEX);
        let existed = path.exists();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err(&path))?;
        let lines: Vec<String> = entries.iter().map(RecycleEntry::line).collect();
        let window = lines.iter().map(|l| l.len() as u64).sum::<u64>() + TAIL_BYTES;
        let (complete, last) = tail(&mut f, window, entries.len()).map_err(io_err(&path))?;
        // The longest head of `entries` that the index ends with.
        let done = (1..=entries.len().min(last.len()))
            .rev()
            .find(|&k| {
                last[last.len() - k..]
                    .iter()
                    .zip(&entries[..k])
                    .all(|(have, want)| have.as_ref() == Some(want))
            })
            .unwrap_or(0);
        if done == entries.len() {
            return Ok(false);
        }
        let len = f.metadata().map_err(io_err(&path))?.len();
        if complete != len {
            f.set_len(complete).map_err(io_err(&path))?;
        }
        f.seek(SeekFrom::Start(complete)).map_err(io_err(&path))?;
        f.write_all(lines[done..].concat().as_bytes())
            .and_then(|()| f.sync_data())
            .map_err(io_err(&path))?;
        if !existed {
            crate::fsutil::fsync_dir(&dir)?;
        }
        Ok(true)
    }

    /// Remove the copy of a discarded commit from its slot (nothing else is touched). A missing
    /// copy is fine.
    pub(super) fn discard_recycled(&self, entry: &RecycleEntry) {
        if let Ok(p) = self.recycle_path(entry)
            && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file())
        {
            let _ = std::fs::remove_file(&p);
        }
    }

    /// Every line of `recycle/index.jsonl` in order; a torn last line is left out.
    pub fn recycle_lines(&self, guard: &StoreGuard) -> Result<Vec<IndexLine>> {
        self.check_guard(guard)?;
        Ok(self
            .read_index_lines()?
            .into_iter()
            .map(|(bytes, entry)| match entry {
                Some(e) => IndexLine::Entry(e),
                None => IndexLine::Raw(String::from_utf8_lossy(&bytes).into_owned()),
            })
            .collect())
    }

    /// The complete lines of the index (blank lines left out), each with its bytes (without
    /// the newline) and its parsed entry (`None` when it doesn't parse). A missing index is
    /// empty.
    fn read_index_lines(&self) -> Result<Vec<(Vec<u8>, Option<RecycleEntry>)>> {
        let path = self.recycle_dir().join(RECYCLE_INDEX);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&path)(e)),
        };
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        Ok(bytes[..complete]
            .split(|b| *b == b'\n')
            .filter(|l| !l.trim_ascii().is_empty())
            .map(|l| (l.to_vec(), serde_json::from_slice(l).ok()))
            .collect())
    }

    /// Apply the retention to this account (R2 to R8). With `dry_run` nothing changes and
    /// nothing is journaled. Takes the store lock with recovery itself.
    ///
    /// A real run recovers an unfinished commit first, plans on the index read after that,
    /// journals one `recycle.pruned` per removed line (one batch, one `opId`), writes the new
    /// index atomically and then removes the slots: a file only when it is a file. A folder
    /// slot goes whole (dot-files and files without a line too) when no kept line has its slot
    /// inside it and no line that can't be read names its `deletedAt`; otherwise only its
    /// empty folders go. Empty folders up to `recycle/` go too.
    /// A slot that can't be removed is reported in `failed` and the prune goes on with the
    /// rest. A dry run takes the plain lock and doesn't recover (recovery writes), so with an
    /// unfinished commit on disk its plan can differ a little from the real run's.
    ///
    /// A line that doesn't parse, or whose slot would leave `recycle/`, is kept unchanged and
    /// counted in `unreadable_kept`. A line with a symlink anywhere in its slot path (from
    /// `recycle/` down), or inside its folder slot, is kept and reported in `failed`: no
    /// symlink is ever followed. A symlinked `recycle/` prunes nothing and gives one entry in
    /// `failed`. Kept lines are written back byte for byte.
    pub fn prune_recycle(
        &self,
        journal: &Journal,
        retention: &Retention,
        now_ms: u64,
        offset: time::UtcOffset,
        origin: Origin,
        dry_run: bool,
    ) -> Result<PruneReport> {
        // Held to the end. Only a real run recovers.
        let (_plain, _recovered);
        if dry_run {
            _plain = self.lock()?;
        } else {
            _recovered = self.lock_recovered(journal)?;
        }
        let root = self.recycle_dir();
        if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
            return Ok(PruneReport {
                failed: vec![PruneFailure {
                    location: String::new(),
                    message: "it is a symlink, nothing is pruned".into(),
                }],
                ..PruneReport::default()
            });
        }
        let lines = self.read_index_lines()?;
        // The readable lines with a slot inside `recycle/`, their slots and where each is in
        // `lines`.
        let mut entries = Vec::new();
        let mut slots = Vec::new();
        let mut at = Vec::new();
        let mut unreadable = 0;
        let mut failed = Vec::new();
        let mut linked_slots = Vec::new();
        let mut raw = Vec::new();
        for (i, (bytes, entry)) in lines.iter().enumerate() {
            let Some((e, slot)) = entry
                .as_ref()
                .and_then(|e| Some((e, self.recycle_path(e).ok()?)))
            else {
                unreadable += 1;
                raw.push(bytes.as_slice());
                continue;
            };
            if self.has_symlink(&slot) {
                failed.push(PruneFailure {
                    location: e.location(),
                    message: "a part of its path is a symlink, the line is kept".into(),
                });
                linked_slots.push(slot);
                continue;
            }
            entries.push(e.clone());
            slots.push(slot);
            at.push(i);
        }
        let linked = linked_slots.len();
        let mut live = HashSet::new();
        for e in &entries {
            if !live.contains(&e.id) && self.path_of(e.id)?.is_some() {
                live.insert(e.id);
            }
        }
        let mut remove = super::retention::plan(
            &entries,
            &|id| live.contains(&id),
            retention,
            now_ms,
            offset,
        );
        // A folder slot with a symlink inside keeps its line.
        let is_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
        remove.retain(|&i| {
            if is_dir(&slots[i]) && holds_symlink(&slots[i]) {
                failed.push(PruneFailure {
                    location: entries[i].location(),
                    message: "it holds a symlink, the line is kept".into(),
                });
                false
            } else {
                true
            }
        });
        // A folder slot goes whole when no kept line has its slot inside it and no line that
        // can't be read names its `deletedAt` (it might hold bytes in there).
        let going: HashSet<usize> = remove.iter().copied().collect();
        let kept_slots: Vec<&PathBuf> = (0..entries.len())
            .filter(|i| !going.contains(i))
            .map(|i| &slots[i])
            .chain(&linked_slots)
            .collect();
        let whole: Vec<bool> = remove
            .iter()
            .map(|&i| {
                let ms = entries[i].deleted_at.to_string();
                is_dir(&slots[i])
                    && !kept_slots.iter().any(|k| k.starts_with(&slots[i]))
                    && !raw
                        .iter()
                        .any(|l| l.windows(ms.len()).any(|w| w == ms.as_bytes()))
            })
            .collect();
        let sizes: Vec<u64> = remove
            .iter()
            .map(|&i| {
                std::fs::symlink_metadata(&slots[i])
                    .ok()
                    .filter(|m| m.is_file())
                    .map_or(0, |m| m.len())
            })
            .collect();
        let mut report = PruneReport {
            kept: entries.len() - remove.len() + linked,
            unreadable_kept: unreadable,
            removed: remove.iter().map(|&i| entries[i].clone()).collect(),
            removed_bytes: sizes.iter().sum(),
            failed,
        };
        if dry_run || report.removed.is_empty() {
            return Ok(report);
        }

        // 1. the journal
        let op_id = new_op_id();
        let mut events: Vec<Event> = report
            .removed
            .iter()
            .map(|e| {
                let body = EventBody::RecyclePruned {
                    id: e.id,
                    path: e.path.clone(),
                    md5: e.md5.clone(),
                    deleted_at: e.deleted_at,
                    reason: e.reason,
                };
                Event::new(&self.slug, origin, None, &op_id, body)
            })
            .collect();
        journal.append(&mut events)?;

        // 2. the new index, kept lines as they were
        let gone: HashSet<usize> = remove.iter().map(|&i| at[i]).collect();
        let mut text = Vec::new();
        for (i, (line, _)) in lines.iter().enumerate() {
            if !gone.contains(&i) {
                text.extend_from_slice(line);
                text.push(b'\n');
            }
        }
        super::write_atomic_short(&root.join(RECYCLE_INDEX), &text)?;

        // 3. the slots: files first, then folder slots, then the emptied folders above them.
        // A failure is reported and the rest goes on: the index no longer names these slots.
        let mut stuck = HashSet::new();
        let mut folders = Vec::new();
        for (k, &i) in remove.iter().enumerate() {
            let slot = &slots[i];
            match std::fs::symlink_metadata(slot) {
                Ok(m) if m.is_file() => {
                    if let Err(err) = std::fs::remove_file(slot) {
                        report.failed.push(PruneFailure {
                            location: entries[i].location(),
                            message: format!("can't remove it: {err}"),
                        });
                        stuck.insert(k);
                    }
                }
                Ok(m) if m.is_dir() => folders.push(k),
                _ => {}
            }
        }
        for k in folders {
            let i = remove[k];
            if !whole[k] {
                remove_empty_folders(&slots[i]);
                continue;
            }
            match std::fs::remove_dir_all(&slots[i]) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    report.failed.push(PruneFailure {
                        location: entries[i].location(),
                        message: format!("can't remove it: {err}"),
                    });
                    stuck.insert(k);
                }
                _ => {}
            }
        }
        for &i in &remove {
            let mut dir = slots[i].parent();
            while let Some(d) = dir {
                if d == root || !d.starts_with(&root) || std::fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
        if let Err(err) = crate::fsutil::fsync_dir(&root) {
            report.failed.push(PruneFailure {
                location: String::new(),
                message: format!("can't sync the folder: {err}"),
            });
        }
        // A slot that is still there is a failure only.
        if !stuck.is_empty() {
            let mut k = 0;
            report.removed.retain(|_| {
                k += 1;
                !stuck.contains(&(k - 1))
            });
            report.removed_bytes -= stuck.iter().map(|&k| sizes[k]).sum::<u64>();
        }
        Ok(report)
    }

    /// Whether `recycle/` or any part of `slot` below it is a symlink. A missing part ends the
    /// check (nothing below it exists).
    fn has_symlink(&self, slot: &Path) -> bool {
        let root = self.recycle_dir();
        let Ok(rel) = slot.strip_prefix(&root) else {
            return true;
        };
        let mut p = root.clone();
        let mut parts = std::iter::once(None).chain(rel.components().map(Some));
        parts.any(|c| {
            if let Some(c) = c {
                p.push(c);
            }
            std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
        })
    }

    /// Every complete line of `recycle/index.jsonl`, oldest first. A missing index is empty.
    pub fn recycle_index(&self, guard: &StoreGuard) -> Result<Vec<RecycleEntry>> {
        self.check_guard(guard)?;
        let path = self.recycle_dir().join(RECYCLE_INDEX);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&path)(e)),
        };
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        text[..complete]
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| Error::Config {
                    path: path.clone(),
                    message: e.to_string(),
                })
            })
            .collect()
    }
}

/// Whether there is a symlink anywhere below `dir` (none is followed). A folder that can't be
/// read counts as none: removing it fails then and is reported.
fn holds_symlink(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| match entry.file_type() {
        Ok(t) if t.is_symlink() => true,
        Ok(t) if t.is_dir() => holds_symlink(&entry.path()),
        _ => false,
    })
}

/// Remove the empty folders in `dir` bottom-up, `dir` last. Files and symlinks stay, and a
/// symlink is never followed.
fn remove_empty_folders(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_folders(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// The length of the complete part of the index (up to and including the last newline) and its
/// last complete lines (at most `count`, oldest first), each parsed (`None` when it is not a
/// [`RecycleEntry`]). Only the last `window` bytes are read.
fn tail(
    f: &mut std::fs::File,
    window: u64,
    count: usize,
) -> std::io::Result<(u64, Vec<Option<RecycleEntry>>)> {
    let len = f.metadata()?.len();
    let start = len.saturating_sub(window);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
        // No newline at all: an empty file or a single torn line, cut it. A window without a
        // newline in a longer file is no line of ours; leave the file as it is.
        return Ok((if start == 0 { 0 } else { len }, Vec::new()));
    };
    let complete = start + end as u64 + 1;
    let mut parts: Vec<&[u8]> = buf[..end].split(|b| *b == b'\n').collect();
    if start > 0 {
        // The first part may be the cut-off end of an older line.
        parts.remove(0);
    }
    let skip = parts.len().saturating_sub(count);
    let last = parts[skip..]
        .iter()
        .map(|l| serde_json::from_slice(l).ok())
        .collect();
    Ok((complete, last))
}
