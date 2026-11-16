//! The recycle bin `accounts/<slug>/recycle/`: old versions and deleted files, so every
//! overwrite and every delete can be undone by hand.
//!
//! Layout:
//!
//! - `recycle/<deletedAt>/<wire path>`: the bytes (or the folder) as they were. `deletedAt` is
//!   ms since the epoch; [`Store::recycle_slot`] picks a later ms when that path is taken, so a
//!   slot is never reused.
//! - `recycle/index.jsonl`: one line per recycled item, [`RecycleEntry`]
//!   `{"id","path","md5","deletedAt","reason"}`, and `"pruned": true` for a folder that a
//!   delete or a move left with only dot-files. The slot of a line is
//!   `recycle/<deletedAt>/<path>` ([`RecycleEntry::location`]).
//!
//! The bin as the user sees it is a list of items ([`recycle_items`], [`Store::recycle_items`]):
//! each deleted file, and each deleted folder with the lines of the files that were in it.
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::commit::{RECYCLE_DIR, is_dot_entry};
use super::retention::Retention;
use super::{Store, StoreGuard, WirePath, is_native_folder};
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
    /// A folder that a delete or a move left with only dot-files, not one the user deleted
    /// (written only when true). Such a line is no item of the bin.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pruned: bool,
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

/// Whether an item of the bin was a file or a folder (from its slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    File,
    Folder,
}

/// One thing the user deleted, as the bin lists it: a file, or a folder with the files that
/// were in it ([`recycle_items`], [`Store::recycle_items`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleItem {
    /// [`RecycleEntry::location`] of `entry`: `<deletedAt>/<path>`.
    pub key: String,
    /// The wire path it had.
    pub path: String,
    /// The last name of `path`.
    pub name: String,
    pub kind: ItemKind,
    pub deleted_at: u64,
    /// The bytes in the slot (0 before [`Store::recycle_items`] looked).
    pub size: u64,
    /// The MD5 of a file, when its line has one.
    pub md5: Option<String>,
    /// For a folder: how many lines it owns.
    pub files: Option<u64>,
    /// The line of the item itself.
    pub entry: RecycleEntry,
    /// The lines of the files that were in a folder, in index order.
    pub owned: Vec<RecycleEntry>,
}

impl RecycleItem {
    fn set_kind(&mut self, kind: ItemKind) {
        self.kind = kind;
        (self.md5, self.files) = match kind {
            ItemKind::File => (Some(self.entry.md5.clone()).filter(|m| !m.is_empty()), None),
            ItemKind::Folder => (None, Some(self.owned.len() as u64)),
        };
    }
}

/// The items of the bin in the lines of the index, newest first (then by path). Pure: no
/// slot is looked at, so `size` is 0 and `kind` is a guess (a folder when it owns lines) that
/// [`Store::recycle_items`] corrects.
///
/// Only `deleted` lines that are no pruned parent and name a slot inside `recycle/` count;
/// lines that can't be read and old versions (`overwritten`) are never items. Lines of the
/// same `deletedAt` whose path lies below the path of another such line (by whole names) are
/// owned by the topmost of them: a folder delete writes a line per file and one for the
/// folder, the folder last. A line not owned is an item, unless its path is hidden.
pub fn recycle_items(lines: &[IndexLine]) -> Vec<RecycleItem> {
    let mut groups: BTreeMap<u64, Vec<(WirePath, &RecycleEntry)>> = BTreeMap::new();
    for line in lines {
        let IndexLine::Entry(e) = line else {
            continue;
        };
        if e.reason != RecycleReason::Deleted || e.pruned {
            continue;
        }
        if let Ok(path) = WirePath::parse_named(&e.path) {
            groups.entry(e.deleted_at).or_default().push((path, e));
        }
    }
    let mut items = Vec::new();
    for group in groups.values() {
        let paths: HashSet<&[String]> = group.iter().map(|(p, _)| p.segments()).collect();
        let mut owned: HashMap<&[String], Vec<RecycleEntry>> = HashMap::new();
        let mut heads = Vec::new();
        for (path, e) in group {
            let names = path.segments();
            match (1..names.len())
                .map(|n| &names[..n])
                .find(|top| paths.contains(top))
            {
                Some(top) => owned.entry(top).or_default().push((*e).clone()),
                None => heads.push((path, *e)),
            }
        }
        let mut seen = HashSet::new();
        for (path, e) in heads {
            if path.is_hidden() || !seen.insert(path.segments()) {
                continue;
            }
            let owned = owned.remove(path.segments()).unwrap_or_default();
            let mut item = RecycleItem {
                key: e.location(),
                path: e.path.clone(),
                name: path.name().unwrap_or_default().to_string(),
                kind: ItemKind::File,
                deleted_at: e.deleted_at,
                size: 0,
                md5: None,
                files: None,
                entry: e.clone(),
                owned,
            };
            let kind = if item.owned.is_empty() {
                ItemKind::File
            } else {
                ItemKind::Folder
            };
            item.set_kind(kind);
            items.push(item);
        }
    }
    items.sort_by(|a, b| {
        b.deleted_at
            .cmp(&a.deleted_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    items
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

    /// The first ms at or after `at_ms` whose slot `recycle/<ms>/<path>` is free and does not
    /// lie inside the slot of an index line with that ms (a folder deleted in the same ms), so
    /// a later delete never lands inside an item of the bin.
    ///
    /// The index is read only when a folder above the slot is there that a line could name (no
    /// native folder or parent of one), so a batch of deletes in one ms (`import --replace`)
    /// of what lies right in the native folders never reads it.
    pub fn recycle_slot(&self, guard: &StoreGuard, path: &WirePath, at_ms: u64) -> Result<u64> {
        self.check_guard(guard)?;
        // The slots of the index, read once when needed.
        let mut slots: Option<Vec<(u64, WirePath)>> = None;
        let mut ms = at_ms;
        loop {
            let base = self.recycle_dir().join(ms.to_string());
            let taken = std::fs::symlink_metadata(path.to_fs(&base)).is_ok()
                || (folder_above(&base, path) && {
                    if slots.is_none() {
                        slots = Some(
                            self.read_index_lines()?
                                .into_iter()
                                .filter_map(|(_, e)| {
                                    let e = e?;
                                    Some((e.deleted_at, WirePath::parse_named(&e.path).ok()?))
                                })
                                .collect(),
                        );
                    }
                    slots
                        .iter()
                        .flatten()
                        .any(|(at, slot)| *at == ms && path.starts_with(slot))
                });
            if !taken {
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

    /// The items of the bin ([`recycle_items`]) as they are on disk, newest first: the kind
    /// from the slot, `size` the bytes in it (no symlink followed). An item whose slot is gone
    /// is left out, and so is a folder that owns no line and holds only dot-files (in folders
    /// too): a pruned parent written before lines had `pruned`. An item with a symlink in its
    /// slot path is listed as found in the index, with size 0.
    pub fn recycle_items(&self, guard: &StoreGuard) -> Result<Vec<RecycleItem>> {
        let lines = self.recycle_lines(guard)?;
        let mut items = recycle_items(&lines);
        items.retain_mut(|item| {
            let Ok(slot) = self.recycle_path(&item.entry) else {
                return false;
            };
            if self.has_symlink(&slot) {
                return true;
            }
            match std::fs::symlink_metadata(&slot) {
                Err(_) => false,
                Ok(m) if m.is_dir() => {
                    item.set_kind(ItemKind::Folder);
                    if item.owned.is_empty() && only_dots(&slot) == Some(true) {
                        return false;
                    }
                    item.size = bytes_in(&slot);
                    true
                }
                Ok(m) => {
                    item.set_kind(ItemKind::File);
                    item.size = if m.is_file() { m.len() } else { 0 };
                    true
                }
            }
        });
        Ok(items)
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

/// Whether a folder above `path` is in `base` (`recycle/<ms>`) that is no native folder or
/// parent of one, so it may be the slot of a line.
fn folder_above(base: &Path, path: &WirePath) -> bool {
    let names = path.segments();
    let mut dir = base.to_path_buf();
    for n in 1..names.len() {
        dir.push(&names[n - 1]);
        if std::fs::symlink_metadata(&dir).is_err() {
            return false;
        }
        if WirePath::from_segments(names[..n].to_vec()).is_ok_and(|a| !is_native_folder(&a)) {
            return true;
        }
    }
    false
}

/// Whether the folder `dir` holds only dot entries ([`is_dot_entry`]) and folders of that
/// kind: `None` when it holds anything else, otherwise whether there is a dot entry. No
/// symlink is followed.
fn only_dots(dir: &Path) -> Option<bool> {
    let mut found = false;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        if is_dot_entry(&name.to_string_lossy()) {
            found = true;
        } else if entry.file_type().ok()?.is_dir() {
            found |= only_dots(&entry.path())?;
        } else {
            return None;
        }
    }
    Some(found)
}

/// The bytes of the files below `dir`; no symlink is followed.
fn bytes_in(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => bytes_in(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Accounts;

    const T: u64 = 1_763_290_000_000;

    fn entry(path: &str, md5: &str, ms: u64, reason: RecycleReason) -> RecycleEntry {
        RecycleEntry {
            id: 1000 + path.len() as u64,
            path: path.into(),
            md5: md5.into(),
            deleted_at: ms,
            reason,
            pruned: false,
        }
    }

    fn deleted(path: &str, md5: &str, ms: u64) -> IndexLine {
        IndexLine::Entry(entry(path, md5, ms, RecycleReason::Deleted))
    }

    /// The line of a folder that a delete left with only dot-files.
    fn pruned(path: &str, ms: u64) -> IndexLine {
        IndexLine::Entry(RecycleEntry {
            pruned: true,
            ..entry(path, "", ms, RecycleReason::Deleted)
        })
    }

    fn keys(items: &[RecycleItem]) -> Vec<&str> {
        items.iter().map(|i| i.key.as_str()).collect()
    }

    #[test]
    fn a_deleted_file_is_one_item() {
        let lines = [deleted("NOTE/Note/a.note", "m1", T)];
        let items = recycle_items(&lines);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.key, format!("{T}/NOTE/Note/a.note"));
        assert_eq!(item.path, "NOTE/Note/a.note");
        assert_eq!(item.name, "a.note");
        assert_eq!(item.kind, ItemKind::File);
        assert_eq!(item.deleted_at, T);
        assert_eq!(item.md5.as_deref(), Some("m1"));
        assert_eq!(item.files, None);
        assert_eq!(item.size, 0);
        assert_eq!(IndexLine::Entry(item.entry.clone()), lines[0]);
        assert!(item.owned.is_empty());
    }

    /// As a delete writes it: the file lines, then the folder line, then the emptied parent
    /// (with a dot-file) at the next ms.
    #[test]
    fn a_folder_delete_is_one_item_with_its_files() {
        let lines = [
            deleted("NOTE/Note/P/W/S/b.txt", "mb", T),
            deleted("NOTE/Note/P/W/a.txt", "ma", T),
            deleted("NOTE/Note/P/W", "", T),
            pruned("NOTE/Note/P", T + 1),
        ];
        let items = recycle_items(&lines);
        assert_eq!(keys(&items), [format!("{T}/NOTE/Note/P/W")]);
        let item = &items[0];
        assert_eq!(item.kind, ItemKind::Folder);
        assert_eq!(item.files, Some(2));
        assert_eq!(item.md5, None);
        assert_eq!(item.owned.len(), 2);
        let owned: Vec<IndexLine> = item.owned.iter().cloned().map(IndexLine::Entry).collect();
        assert_eq!(owned, lines[..2]);
    }

    #[test]
    fn overwritten_lines_are_no_items() {
        let over =
            |path: &str, ms| IndexLine::Entry(entry(path, "old", ms, RecycleReason::Overwritten));
        let lines = [
            over("NOTE/Note/a.note", T),
            over("NOTE/Note/W/x.txt", T + 5),
            deleted("NOTE/Note/W", "", T + 5),
        ];
        let items = recycle_items(&lines);
        assert_eq!(keys(&items), [format!("{}/NOTE/Note/W", T + 5)]);
        // An old version is never part of an item.
        assert!(items[0].owned.is_empty());
        assert!(recycle_items(&lines[..1]).is_empty());
    }

    #[test]
    fn a_hidden_path_is_no_item() {
        let lines = [
            deleted("NOTE/Note/.hidden", "m", T),
            deleted("NOTE/.x/a.note", "m", T + 1),
            deleted("NOTE/.y/b.note", "m", T + 2),
            deleted("NOTE/.y", "", T + 2),
        ];
        assert!(recycle_items(&lines).is_empty());
    }

    #[test]
    fn a_pruned_parent_is_no_item() {
        let lines = [
            deleted("NOTE/Note/P/a.note", "m", T),
            pruned("NOTE/Note/P", T + 1),
        ];
        let items = recycle_items(&lines);
        assert_eq!(keys(&items), [format!("{T}/NOTE/Note/P/a.note")]);
        assert!(recycle_items(&lines[1..]).is_empty());
    }

    #[test]
    fn a_line_that_cannot_be_read_is_no_item() {
        let lines = [
            IndexLine::Raw("not json".into()),
            deleted("../outside", "m", T),
            deleted("", "", T + 1),
            deleted("NOTE/Note/a.note", "m", T + 2),
        ];
        let items = recycle_items(&lines);
        assert_eq!(keys(&items), [format!("{}/NOTE/Note/a.note", T + 2)]);
    }

    #[test]
    fn a_file_line_with_an_empty_md5_inside_a_folder_is_owned() {
        let lines = [
            deleted("NOTE/Note/W/a.txt", "", T),
            deleted("NOTE/Note/W", "", T),
        ];
        let items = recycle_items(&lines);
        assert_eq!(keys(&items), [format!("{T}/NOTE/Note/W")]);
        assert_eq!(items[0].files, Some(1));
        assert_eq!(items[0].owned[0].path, "NOTE/Note/W/a.txt");
    }

    /// Paths are compared by whole names: `W.txt` is no part of the folder `W`.
    #[test]
    fn a_sibling_with_a_longer_name_is_its_own_item() {
        let lines = [
            deleted("NOTE/Note/W.txt", "m", T),
            deleted("NOTE/Note/W/a.txt", "m", T),
            deleted("NOTE/Note/W", "", T),
        ];
        let items = recycle_items(&lines);
        assert_eq!(
            keys(&items),
            [format!("{T}/NOTE/Note/W"), format!("{T}/NOTE/Note/W.txt")]
        );
        assert_eq!(items[0].files, Some(1));
    }

    #[test]
    fn items_are_newest_first() {
        let lines = [
            deleted("NOTE/Note/old.note", "m", T),
            deleted("NOTE/Note/b.note", "m", T + 2),
            deleted("NOTE/Note/mid.note", "m", T + 1),
            deleted("NOTE/Note/a.note", "m", T + 2),
        ];
        let items = recycle_items(&lines);
        assert_eq!(
            keys(&items),
            [
                format!("{}/NOTE/Note/a.note", T + 2),
                format!("{}/NOTE/Note/b.note", T + 2),
                format!("{}/NOTE/Note/mid.note", T + 1),
                format!("{T}/NOTE/Note/old.note"),
            ]
        );
    }

    #[test]
    fn an_old_entry_without_pruned_still_reads() {
        let old = r#"{"id":7,"path":"NOTE/Note/P","md5":"","deletedAt":5,"reason":"deleted"}"#;
        let e: RecycleEntry = serde_json::from_str(old).unwrap();
        assert!(!e.pruned);
        // A line that is no pruned parent is written as before, byte for byte.
        assert_eq!(e.line(), format!("{old}\n"));
        let p = RecycleEntry { pruned: true, ..e };
        assert_eq!(
            p.line(),
            "{\"id\":7,\"path\":\"NOTE/Note/P\",\"md5\":\"\",\"deletedAt\":5,\"reason\":\"deleted\",\"pruned\":true}\n"
        );
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", "jordi@example.org", "secret")
            .unwrap();
        let store = Store::open(dir.path(), "jordi").unwrap();
        (dir, store)
    }

    fn wp(p: &str) -> WirePath {
        WirePath::parse(p).unwrap()
    }

    /// A slot with its line: a folder slot (with `files` in it) or a file slot.
    fn slot(store: &Store, e: &RecycleEntry, files: &[(&str, &[u8])]) {
        let p = store.recycle_path(e).unwrap();
        if files.is_empty() && !e.md5.is_empty() {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"bytes").unwrap();
        } else {
            std::fs::create_dir_all(&p).unwrap();
            for (name, bytes) in files {
                let f = p.join(name);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, bytes).unwrap();
            }
        }
        store.append_recycled(e).unwrap();
    }

    #[test]
    fn a_slot_inside_a_folder_slot_of_the_same_ms_is_bumped() {
        let (_dir, store) = store();
        let g = store.lock().unwrap();
        let folder = entry("NOTE/Note/A", "", T, RecycleReason::Deleted);
        slot(&store, &folder, &[]);
        // Inside the folder slot of a line with that ms: the next ms.
        assert_eq!(
            store.recycle_slot(&g, &wp("NOTE/Note/A/B"), T).unwrap(),
            T + 1
        );
        assert_eq!(
            store.recycle_slot(&g, &wp("NOTE/Note/A/B/c"), T).unwrap(),
            T + 1
        );
        // A sibling shares the ms.
        assert_eq!(store.recycle_slot(&g, &wp("NOTE/Note/C"), T).unwrap(), T);
        // A folder that only holds slots of that ms is no slot itself.
        let file = entry("NOTE/Note/F/x.txt", "m", T + 7, RecycleReason::Deleted);
        slot(&store, &file, &[]);
        assert_eq!(
            store
                .recycle_slot(&g, &wp("NOTE/Note/F/y.txt"), T + 7)
                .unwrap(),
            T + 7
        );
        // Right in a native folder no line can lie above: the index is not read (here it
        // could not be).
        std::fs::remove_file(store.recycle_dir().join(RECYCLE_INDEX)).unwrap();
        std::fs::create_dir(store.recycle_dir().join(RECYCLE_INDEX)).unwrap();
        assert_eq!(store.recycle_slot(&g, &wp("NOTE/Note/D"), T).unwrap(), T);
        assert!(store.recycle_slot(&g, &wp("NOTE/Note/A/E"), T).is_err());
    }

    #[test]
    fn the_store_items_take_kind_and_size_from_the_slots() {
        let (_dir, store) = store();
        let g = store.lock().unwrap();
        let file = entry("NOTE/Note/a.txt", "m", T, RecycleReason::Deleted);
        slot(&store, &file, &[]);
        let inside = entry("NOTE/Note/W/x.txt", "", T + 1, RecycleReason::Deleted);
        let folder = entry("NOTE/Note/W", "", T + 1, RecycleReason::Deleted);
        slot(
            &store,
            &folder,
            &[
                ("x.txt", b"12345"),
                ("S/y.txt", b"123"),
                (".DS_Store", b"d"),
            ],
        );
        store.append_recycled(&inside).unwrap();
        let gone = entry("NOTE/Note/gone.txt", "m", T + 2, RecycleReason::Deleted);
        store.append_recycled(&gone).unwrap();
        let empty = entry("NOTE/Note/E", "", T + 3, RecycleReason::Deleted);
        slot(&store, &empty, &[]);
        let items = store.recycle_items(&g).unwrap();
        assert_eq!(
            keys(&items),
            [
                format!("{}/NOTE/Note/E", T + 3),
                format!("{}/NOTE/Note/W", T + 1),
                format!("{T}/NOTE/Note/a.txt"),
            ]
        );
        assert_eq!(
            (items[0].kind, items[0].size, items[0].files),
            (ItemKind::Folder, 0, Some(0))
        );
        assert_eq!(
            (items[1].kind, items[1].size, items[1].files),
            (ItemKind::Folder, 9, Some(1))
        );
        assert_eq!(items[1].md5, None);
        assert_eq!(
            (items[2].kind, items[2].size, items[2].files),
            (ItemKind::File, 5, None)
        );
    }

    /// A pruned parent written before lines had `pruned`: a folder line that owns nothing and
    /// whose slot holds only dot-files (in folders too).
    #[test]
    fn an_old_pruned_parent_of_dot_files_is_no_item() {
        let (_dir, store) = store();
        let g = store.lock().unwrap();
        let old = entry("NOTE/Note/P", "", T, RecycleReason::Deleted);
        slot(&store, &old, &[(".DS_Store", b"d"), ("Q/.keep", b"")]);
        assert!(store.recycle_items(&g).unwrap().is_empty());
    }

    #[test]
    fn a_line_twice_is_one_item() {
        let lines = [
            deleted("NOTE/Note/a.note", "m", T),
            deleted("NOTE/Note/a.note", "m", T),
        ];
        assert_eq!(recycle_items(&lines).len(), 1);
    }

    #[test]
    fn a_folder_inside_a_folder_of_the_same_ms_is_owned_by_the_top() {
        let lines = [
            deleted("NOTE/Note/A/x.txt", "m", T),
            deleted("NOTE/Note/A", "", T),
            deleted("NOTE/Note/A/B/y.txt", "m", T),
            deleted("NOTE/Note/A/B", "", T),
        ];
        let items = recycle_items(&lines);
        assert_eq!(keys(&items), [format!("{T}/NOTE/Note/A")]);
        assert_eq!(items[0].owned.len(), 3);
    }
}
