//! The commit protocol: every change to `files/` is journaled exactly once, also across a crash.
//!
//! [`Store::commit`] runs four steps under the store lock:
//!
//! 1. Write the intent ([`Commit`]: the operation and its planned events) to
//!    `accounts/<slug>/pending.json` (atomic).
//! 2. Apply the data change durably (rename, fsync of the folders), then update the id registry
//!    and write the commit's side files ([`SideWrite`], the page record of a note).
//! 3. Append the events to the journal (one write, one fsync).
//! 4. Delete `pending.json`.
//!
//! [`Store::recover`] runs at server start, at the start of every CLI command and in
//! [`Store::lock_recovered`], the only way to get the [`WriteGuard`] that planning and
//! committing need. So no change is ever planned on a registry that an unfinished commit left
//! behind. With a `pending.json` recovery checks whether the change is applied ([`PendingOp`]
//! lists the check per operation). Applied: it brings the registry up to date and appends the
//! events that the journal does not have yet (`origin: "repair"`), found by the `opId` in the
//! journal after the `journalSeq` stored in the intent. Not applied: it discards the intent.
//!
//! Stale temp files (`.nd-*.tmp`, older than one hour) are swept from `files/` at server start
//! and with the hourly staging sweep ([`sweep_all_temps`]), not on every lock: the sweep walks
//! the whole tree.
//!
//! Lock order: the store lock first, then `journal/.lock`.

use std::collections::BTreeSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::recycle::{RecycleEntry, RecycleReason};
use super::{Kind, Store, StoreGuard, WirePath, is_native_folder, list};
use crate::journal::{Event, EventBody, FileKind, Journal, Origin, new_op_id};
use crate::{Error, Result, io_err};

/// `accounts/<slug>/pending.json`: the intent of the commit in progress.
pub const PENDING_FILE: &str = "pending.json";
/// `accounts/<slug>/recycle/`: deleted files and folders.
pub const RECYCLE_DIR: &str = "recycle";
/// Temp files in `files/` older than this are left over from a crash and are removed.
pub const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// One data change. Paths are wire paths (`NOTE/Note/x.note`); `staged` and `recycle` are
/// relative to the account folder and to `recycle/`.
///
/// "Applied" (checked by recovery) per operation:
/// - `put`, `copy`: the target is a file with the intended `md5` (this implies its `folders`
///   exist and the recycle copy of a `put` is in place, both are done before the rename).
/// - `move`: the source is gone and the target exists. A move `via` a temp name (a rename that
///   only changes case on a file system that ignores case) stopped between its two renames is
///   finished first, so the file never stays under the hidden temp name.
/// - `delete`: the source is gone and `recycle/<recycle>` exists.
/// - `createFolder`: the target is a folder.
/// - `restore`: the slot `recycle/<recycle>` is gone and `to` exists.
///
/// A `move` or `delete` that empties folders lists them in `prune` (deepest first). A folder
/// whose only other entries are dot-files counts as empty (server temp names `.nd-*` do not).
/// After the apply step (also by recovery) they are removed, or, when they hold dot-files, the
/// topmost one moves with everything in it to `recycle/<prune_at>/<path>`: dot-files are never
/// deleted. A folder that is not empty then stays, and its `folder.deleted` event is dropped
/// before the journal append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum PendingOp {
    /// Create or replace the file `path` by renaming the staged file onto it. First the
    /// missing parent `folders` are created (parents first), and with `recycle` the current
    /// bytes at `path` are copied into the recycle bin; its index line is written after the
    /// rename (also by recovery).
    #[serde(rename_all = "camelCase")]
    Put {
        id: u64,
        path: String,
        staged: String,
        md5: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        folders: Vec<PlannedFolder>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recycle: Option<RecycleEntry>,
    },
    /// Copy the file `source` to the new file `path`, after creating the missing parent
    /// `folders` (parents first).
    #[serde(rename_all = "camelCase")]
    Copy {
        id: u64,
        source_id: u64,
        source: String,
        path: String,
        md5: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        folders: Vec<PlannedFolder>,
    },
    /// Rename a file or folder; the id stays. The missing parent `folders` of `to` are created
    /// first (parents first). With `via` (a name in the source folder) the rename goes through
    /// that temp name: a rename that only changes case on a file system that ignores case.
    /// Afterwards the emptied source folders in `prune` are removed (see `prune_at`).
    #[serde(rename_all = "camelCase")]
    Move {
        id: u64,
        kind: Kind,
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        folders: Vec<PlannedFolder>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prune: Vec<PlannedFolder>,
        /// When the folders to prune hold dot-files: the ms of the recycle slot the topmost one
        /// moves to, with everything in it. Without it they are removed (they are empty).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prune_at: Option<u64>,
    },
    /// Move a file or folder into `recycle/<recycle>`; the id is dropped. `entry` is its line
    /// for `recycle/index.jsonl`, and `inside` has one line for each file inside a deleted
    /// folder; all of them are written in one append after the apply step (also by recovery).
    /// Afterwards the emptied folders in `prune` are removed (see `prune_at`).
    #[serde(rename_all = "camelCase")]
    Delete {
        id: u64,
        kind: Kind,
        path: String,
        recycle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<RecycleEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inside: Vec<RecycleEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prune: Vec<PlannedFolder>,
        /// As for `move`: the recycle slot of pruned folders that hold dot-files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prune_at: Option<u64>,
    },
    /// Create the folder `path`.
    #[serde(rename_all = "camelCase")]
    CreateFolder { id: u64, path: String },
    /// Bring an item of the recycle bin back: rename its slot `recycle/<recycle>` to `to`,
    /// after creating the missing parent `folders` (parents first). `id` is the new id of `to`,
    /// and `inside` lists the new ids of what a folder holds (parents first, from a walk of
    /// the slot). After the apply step (also by recovery) the index loses the item's `lines`
    /// (its own line and the lines of the files that were in it), and the old versions of each
    /// restored file move from its old id to its new one (`reid`, pairs old id and new id), in
    /// one atomic rewrite; then the emptied folders of the slot go.
    #[serde(rename_all = "camelCase")]
    Restore {
        kind: Kind,
        recycle: String,
        to: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        folders: Vec<PlannedFolder>,
        id: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inside: Vec<PlannedEntry>,
        lines: Vec<RecycleEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reid: Vec<(u64, u64)>,
    },
    /// Nothing to do: a put of the bytes the file already has. [`Store::commit`] writes no
    /// intent, no data and no event for it and returns no `seq`. Never in `pending.json`.
    #[serde(rename_all = "camelCase")]
    Unchanged { id: u64, path: String, md5: String },
}

/// A folder a `put`, `copy` or `move` creates, or a folder a `move` or `delete` prunes (its id
/// is assigned while planning).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFolder {
    pub id: u64,
    pub path: String,
}

/// A file or folder a `restore` brings back inside a folder, with its new id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedEntry {
    pub id: u64,
    pub kind: Kind,
    pub path: String,
}

/// Options of [`Store::plan_put_with`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PutOptions {
    /// Create missing parent folders (each gets `folder.created` before the file event).
    pub create_parents: bool,
    /// Keep the current bytes of a replaced file in `recycle/<ms>/<path>` (protect mode); the
    /// value is the time of the overwrite in ms.
    pub recycle_at_ms: Option<u64>,
    /// The MD5 the staged file must have (case-insensitive). A mismatch is refused with
    /// `Error::Invalid { what: "content hash" }` before anything is planned or assigned, so the
    /// caller hashes the staged file only once (here, under the store lock).
    pub expect_md5: Option<String>,
    /// Drop the cache row of the written file instead of keeping the put's MD5 (the import,
    /// which has no use for the row, and whose files are looked at again by `verify`).
    pub drop_md5: bool,
}

/// The `what` of the error [`Store::plan_put_with`] returns for a wrong `expect_md5`.
pub const CONTENT_HASH_MISMATCH: &str = "content hash";

/// A planned commit, the content of `pending.json`. Build it with a `Store::plan_*` method,
/// then add events if needed (the page indexer pushes its page events right after the file
/// event) and pass it to [`Store::commit`]. All events get the same `opId`, `origin` and
/// `device`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub op_id: String,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The journal's last `seq` when the commit started (set by [`Store::commit`]). Recovery
    /// looks for this commit's events only after it. `None` (an intent written without it)
    /// scans the whole journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_seq: Option<u64>,
    pub op: PendingOp,
    /// The events, in journal order.
    pub events: Vec<EventBody>,
    /// Files written after the data change and before the journal append, such as the page
    /// record of a note ([`crate::pages`]). Not part of the intent: recovery does not write
    /// them, so they must be files whose loss is harmless.
    #[serde(skip)]
    pub side_writes: Vec<SideWrite>,
    /// A put drops the cache row of its file instead of keeping the put's MD5. Not part of the
    /// intent: recovery always drops it.
    #[serde(skip)]
    pub drop_md5: bool,
}

/// A file directly inside `<account>/pages/` that a commit writes (atomically) or removes after
/// its data change and before its journal append; any other path is refused. A failure is reported on stderr and does not fail the
/// commit. A file that already has the new bytes is not written again.
#[derive(Clone, PartialEq, Eq)]
pub struct SideWrite {
    pub path: PathBuf,
    /// The new content; `None` removes the file.
    pub bytes: Option<Vec<u8>>,
}

/// The folders a run of side writes changed. Each is synced once at the end of the run
/// ([`Store::sync_sides`]) instead of once per file.
#[derive(Debug, Default)]
pub(super) struct SideBatch {
    dirs: BTreeSet<PathBuf>,
}

impl std::fmt::Debug for SideWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SideWrite")
            .field("path", &self.path)
            .field("bytes", &self.bytes.as_ref().map(Vec::len))
            .finish()
    }
}

impl Commit {
    /// A commit with a fresh `opId`.
    pub fn new(
        origin: Origin,
        device: Option<String>,
        op: PendingOp,
        events: Vec<EventBody>,
    ) -> Commit {
        Commit {
            op_id: new_op_id(),
            origin,
            device,
            journal_seq: None,
            op,
            events,
            side_writes: Vec::new(),
            drop_md5: false,
        }
    }

    /// Add an event at the end.
    pub fn push_event(&mut self, body: EventBody) -> &mut Commit {
        self.events.push(body);
        self
    }

    fn journal_events(&self, account: &str, origin: Origin, bodies: &[EventBody]) -> Vec<Event> {
        bodies
            .iter()
            .map(|b| Event::new(account, origin, self.device.clone(), &self.op_id, b.clone()))
            .collect()
    }
}

/// What [`Store::recover`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No `pending.json`.
    Clean,
    /// The change was not applied; the intent was dropped and nothing was journaled.
    Discarded { op_id: String },
    /// The change was applied; `seqs` are the events appended now with `origin: "repair"`
    /// (empty when the journal already had all of them).
    Repaired { op_id: String, seqs: Vec<u64> },
}

impl std::fmt::Display for Recovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Recovery::Clean => write!(f, "nothing to recover"),
            Recovery::Discarded { op_id } => {
                write!(
                    f,
                    "dropped the unfinished change {op_id} (it was not applied)"
                )
            }
            Recovery::Repaired { op_id, seqs } if seqs.is_empty() => {
                write!(
                    f,
                    "finished the change {op_id} (the journal had its events)"
                )
            }
            Recovery::Repaired { op_id, seqs } => write!(
                f,
                "finished the change {op_id} and journaled {} missing {}",
                seqs.len(),
                if seqs.len() == 1 { "event" } else { "events" }
            ),
        }
    }
}

/// What [`Store::lock_unless_pending_within`] got.
#[derive(Debug)]
pub enum LockTry {
    Locked(WriteGuard),
    /// Another process still held the store lock when the wait was over.
    Busy,
    /// An unfinished change is on disk; the lock was let go again.
    Pending,
}

/// The store lock, taken through [`Store::lock_recovered`]: recovery ran under it, so the id
/// registry is current. Planning (`plan_*`), [`Store::commit`] and [`Store::snapshot`] need it.
/// It derefs to the [`StoreGuard`] for every other method.
#[derive(Debug)]
pub struct WriteGuard {
    guard: StoreGuard,
}

impl Deref for WriteGuard {
    type Target = StoreGuard;

    fn deref(&self) -> &StoreGuard {
        &self.guard
    }
}

/// The facts of a file a `put` wrote, taken right after its rename: they make a cache row that
/// is right when the file is still the staged one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WrittenFile {
    pub(super) size: u64,
    pub(super) mtime_ns: u64,
    pub(super) ctime_ns: u64,
}

/// Test hook: stop a commit at a step, as a crash there would. Not for production use.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After `pending.json` is written, before the data change.
    BeforeApply,
    /// A `put`: after its new folders and its recycle copy, before the rename. A `restore`: after
    /// its new folders, before the rename. A move through a temp name: between its two renames.
    BeforeRename,
    /// After the data change, before the registry update and the journal append.
    BeforeAppend,
    /// A move through a temp name: the second rename fails, so the apply step returns an error
    /// while the entry is under the temp name.
    FailSecondRename,
    /// A move through a temp name: between its two renames, a file appears at the target (as
    /// an out-of-band write would place it), then the second rename runs.
    TakeTargetBetweenRenames,
    /// After the journal append, before `pending.json` is deleted.
    BeforeClear,
}

/// Whether a [`CrashPoint::BeforeRename`] can stop `op`: a put, a restore (after their new
/// folders), and a move through a temp name.
fn has_rename_stop(op: &PendingOp) -> bool {
    matches!(
        op,
        PendingOp::Put { .. } | PendingOp::Restore { .. } | PendingOp::Move { via: Some(_), .. }
    )
}

/// A name a prune moves along with its folder: a dot-file or dot-folder that is not a server
/// temp name (`.nd-*`: temp files, the temp name of a rename). Those are never swept.
pub(super) fn is_dot_entry(name: &str) -> bool {
    name.starts_with('.') && !name.starts_with(".nd-")
}

/// What else the folder `dir` holds besides `except`: `None` when it holds anything but dot
/// entries ([`is_dot_entry`]) or `dots` is false and it holds a dot entry; otherwise whether it
/// holds a dot entry.
fn folder_rest(dir: &Path, except: Option<&str>, dots: bool) -> Result<Option<bool>> {
    let mut found = false;
    for entry in std::fs::read_dir(dir).map_err(io_err(dir))? {
        let name = entry.map_err(io_err(dir))?.file_name();
        let name = name.to_string_lossy();
        if Some(name.as_ref()) == except {
            continue;
        }
        if dots && is_dot_entry(&name) {
            found = true;
            continue;
        }
        return Ok(None);
    }
    Ok(Some(found))
}

/// The index line of the topmost pruned folder moved to `recycle/<ms>/<path>`.
fn prune_entry(top: &PlannedFolder, ms: u64) -> RecycleEntry {
    RecycleEntry {
        id: top.id,
        path: top.path.clone(),
        md5: String::new(),
        deleted_at: ms,
        reason: RecycleReason::Deleted,
        pruned: true,
    }
}

/// `folder.deleted` for each pruned folder, in the given order.
fn prune_events(prune: &[PlannedFolder]) -> Vec<EventBody> {
    prune
        .iter()
        .map(|p| EventBody::FolderDeleted {
            id: p.id,
            path: p.path.clone(),
        })
        .collect()
}

fn invalid(what: &'static str, message: String) -> Error {
    Error::Invalid { what, message }
}

fn wire(text: &str) -> Result<WirePath> {
    WirePath::parse_named(text)
}

/// A relative path without `..`, `.` or empty segments (for `staged` and `recycle`).
fn safe_relative(text: &str, what: &'static str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for seg in text.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\0') || seg.contains('\\') {
            return Err(invalid(
                what,
                format!("{text:?} is not a plain relative path"),
            ));
        }
        out.push(seg);
    }
    Ok(out)
}

/// A file a restore brings back, as read from its slot before any row is planned.
struct RestoredFile {
    size: u64,
    md5: String,
    /// The id of its index line, whose old versions follow the file to its new id.
    old_id: Option<u64>,
}

impl RestoredFile {
    /// Its `file.created` with the new `id`; the pair of ids goes to `reid`.
    fn event(self, id: u64, path: &str, reid: &mut Vec<(u64, u64)>) -> EventBody {
        if let Some(old) = self.old_id {
            reid.push((old, id));
        }
        EventBody::FileCreated {
            id,
            path: path.to_string(),
            kind: FileKind::of(path.rsplit('/').next().unwrap_or_default()),
            md5: self.md5,
            size: self.size,
            pages_changed: None,
        }
    }
}

/// The file at `fs_path` in a slot, with its index `line` if it has one: the size from a stat,
/// the MD5 of the line when it has one, else the file is hashed.
fn restored_file(fs_path: &Path, line: Option<&RecycleEntry>) -> Result<RestoredFile> {
    let size = std::fs::symlink_metadata(fs_path)
        .map_err(io_err(fs_path))?
        .len();
    let md5 = match line.map(|l| l.md5.as_str()).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => super::hashes::md5_file(fs_path).map_err(io_err(fs_path))?,
    };
    Ok(RestoredFile {
        size,
        md5,
        old_id: line.map(|l| l.id),
    })
}

impl Store {
    fn pending_path(&self) -> PathBuf {
        self.account_dir.join(PENDING_FILE)
    }

    /// Take the store lock and recover an unfinished commit under it ([`Store::recover`]). The
    /// only way to a [`WriteGuard`]. An intent that cannot be read or finished is an error, so a
    /// write to that account fails until the operator fixes it.
    pub fn lock_recovered(&self, journal: &Journal) -> Result<WriteGuard> {
        let guard = self.lock()?;
        self.recover(&guard, journal).map_err(|e| match e {
            Error::Config { path, message } => Error::Config {
                path,
                message: format!(
                    "{message}; this unfinished change blocks every write to account {}",
                    self.slug
                ),
            },
            other => other,
        })?;
        Ok(WriteGuard { guard })
    }

    /// Take the store lock without recovering anything: `None` (the lock is let go again) when
    /// an unfinished commit is on disk, which only [`Store::lock_recovered`] repairs. For
    /// callers that must write nothing, such as a dry run.
    pub fn lock_unless_pending(&self) -> Result<Option<WriteGuard>> {
        let guard = self.lock()?;
        if self.pending_path().exists() {
            return Ok(None);
        }
        Ok(Some(WriteGuard { guard }))
    }

    /// [`Store::lock_unless_pending`] that gives up after `wait` ([`Store::lock_within`]).
    pub fn lock_unless_pending_within(&self, wait: std::time::Duration) -> Result<LockTry> {
        let Some(guard) = self.lock_within(wait)? else {
            return Ok(LockTry::Busy);
        };
        if self.pending_path().exists() {
            return Ok(LockTry::Pending);
        }
        Ok(LockTry::Locked(WriteGuard { guard }))
    }

    /// Refuse to plan or commit while an intent is on disk (a commit under this guard failed
    /// after writing it): the registry may not reflect it. `lock_recovered` again to repair.
    fn check_no_pending(&self) -> Result<()> {
        if self.pending_path().exists() {
            return Err(invalid(
                "store state",
                format!(
                    "an unfinished change is in {}; take the lock again to recover it first",
                    self.pending_path().display()
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn check_write(&self, guard: &WriteGuard) -> Result<()> {
        self.check_guard(guard)?;
        self.check_no_pending()
    }

    /// The intent of an unfinished commit, if any.
    pub fn pending(&self) -> Result<Option<Commit>> {
        let path = self.pending_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path)(e)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Config {
                path: path.clone(),
                message: e.to_string(),
            })
    }

    fn clear_pending(&self) -> Result<()> {
        let path = self.pending_path();
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(&path)(e)),
        }
        crate::fsutil::fsync_dir(&self.account_dir)
    }

    fn resolve(&self, path: &WirePath) -> Result<Option<list::Found>> {
        list::resolve(&self.files, path)
    }

    fn require_folder(&self, path: &WirePath, what: &'static str) -> Result<()> {
        if path.is_root() {
            return Ok(());
        }
        match self.resolve(path)? {
            Some(f) if f.kind == Kind::Folder => Ok(()),
            _ => Err(invalid(
                what,
                format!("{path} is not a folder in the store"),
            )),
        }
    }

    fn require_free(&self, guard: &StoreGuard, path: &WirePath) -> Result<()> {
        self.check_path(guard, path)?;
        if self.resolve(path)?.is_some() || std::fs::symlink_metadata(self.fs_path(path)).is_ok() {
            return Err(invalid("path", format!("{path} exists already")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ planning

    /// Plan writing the staged file `staged` (inside the account folder, for example in
    /// `staging/` or a temp file in the target folder) to `path`: `file.created`, or
    /// `file.modified` when `path` is a file already (the id stays). The parent folder must
    /// exist. The MD5 and size are taken from the staged file.
    pub fn plan_put(
        &self,
        guard: &WriteGuard,
        path: &WirePath,
        staged: &Path,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.plan_put_with(guard, path, staged, origin, device, PutOptions::default())
    }

    /// [`Store::plan_put`] with options: create missing parent folders in the same commit
    /// (`folder.created` for each, parents first, before the file event), and keep the replaced
    /// bytes in the recycle bin (protect mode).
    pub fn plan_put_with(
        &self,
        guard: &WriteGuard,
        path: &WirePath,
        staged: &Path,
        origin: Origin,
        device: Option<String>,
        options: PutOptions,
    ) -> Result<Commit> {
        self.check_write(guard)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("path", "the root is not a file".into()))?;
        let missing = if options.create_parents {
            self.missing_folders(&parent)?
        } else {
            self.require_folder(&parent, "parent folder")?;
            Vec::new()
        };
        self.check_path(guard, path)?;
        let rel = staged.strip_prefix(&self.account_dir).map_err(|_| {
            invalid(
                "staged file",
                format!("{} is outside the account", staged.display()),
            )
        })?;
        let rel = rel
            .to_str()
            .ok_or_else(|| invalid("staged file", "the name is not UTF-8".into()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        safe_relative(&rel, "staged file")?;
        let meta = std::fs::metadata(staged).map_err(io_err(staged))?;
        if !meta.is_file() {
            return Err(invalid(
                "staged file",
                format!("{} is not a file", staged.display()),
            ));
        }
        let md5 = super::hashes::md5_file(staged).map_err(io_err(staged))?;
        if let Some(want) = &options.expect_md5
            && !want.trim().eq_ignore_ascii_case(&md5)
        {
            return Err(invalid(
                CONTENT_HASH_MISMATCH,
                format!("the staged file has MD5 {md5}, not {want}"),
            ));
        }
        let size = meta.len();
        let prev = match self.resolve(path)? {
            Some(f) if f.kind == Kind::File => Some(self.md5_of(guard, path)?),
            Some(_) => return Err(invalid("path", format!("{path} is a folder"))),
            None => None,
        };
        // The folders first: planning them drops stale rows (with their subtree) at their
        // paths, so the file's id must be assigned after that. With missing folders there is no
        // current file, so this is never an unchanged put.
        let (mut events, folders) = self.plan_folders(guard, &missing)?;
        let id = self.assign(guard, path, Kind::File)?;
        let display = path.display();
        if prev.as_deref() == Some(md5.as_str()) {
            let op = PendingOp::Unchanged {
                id,
                path: display,
                md5,
            };
            return Ok(Commit::new(origin, device, op, Vec::new()));
        }
        let recycle = match (&prev, options.recycle_at_ms) {
            (Some(prev_md5), Some(at)) => Some(RecycleEntry {
                id,
                path: display.clone(),
                md5: prev_md5.clone(),
                deleted_at: self.recycle_slot(guard, path, at)?,
                reason: RecycleReason::Overwritten,
                pruned: false,
            }),
            _ => None,
        };
        let kind = FileKind::of(path.name().unwrap_or_default());
        let event = match prev {
            None => EventBody::FileCreated {
                id,
                path: display.clone(),
                kind,
                md5: md5.clone(),
                size,
                pages_changed: None,
            },
            Some(prev_md5) => EventBody::FileModified {
                id,
                path: display.clone(),
                kind,
                md5: md5.clone(),
                prev_md5,
                size,
                pages_changed: None,
            },
        };
        events.push(event);
        let op = PendingOp::Put {
            id,
            path: display,
            staged: rel,
            md5,
            size,
            folders,
            recycle,
        };
        let mut commit = Commit::new(origin, device, op, events);
        commit.drop_md5 = options.drop_md5;
        Ok(commit)
    }

    /// The folders of `parent` (itself included) that do not exist yet, parents first. Each is
    /// checked for name collisions. A file on the way is refused.
    fn missing_folders(&self, parent: &WirePath) -> Result<Vec<WirePath>> {
        let mut missing = Vec::new();
        let mut cur = WirePath::root();
        for segment in parent.segments() {
            cur = cur.join(segment)?;
            if !missing.is_empty() {
                missing.push(cur.clone());
                continue;
            }
            match self.resolve(&cur)? {
                Some(f) if f.kind == Kind::Folder => {}
                Some(_) => return Err(invalid("parent folder", format!("{cur} is a file"))),
                None => missing.push(cur.clone()),
            }
        }
        Ok(missing)
    }

    /// Plan copying the file `source` to the new path `target` (a new id, also when a stale
    /// registry row of a vanished file names `target`): `folder.created` for each missing parent
    /// folder (parents first), then `file.copied`.
    pub fn plan_copy(
        &self,
        guard: &WriteGuard,
        source: &WirePath,
        target: &WirePath,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_write(guard)?;
        match self.resolve(source)? {
            Some(f) if f.kind == Kind::File => {}
            _ => {
                return Err(invalid(
                    "source",
                    format!("{source} is not a file in the store"),
                ));
            }
        }
        let parent = target
            .parent()
            .ok_or_else(|| invalid("path", "the root is not a file".into()))?;
        let missing = self.missing_folders(&parent)?;
        self.require_free(guard, target)?;
        let md5 = self.md5_of(guard, source)?;
        let size = std::fs::metadata(self.fs_path(source))
            .map_err(io_err(&self.fs_path(source)))?
            .len();
        let source_id = self.assign(guard, source, Kind::File)?;
        self.drop_stale_row(guard, target, None)?;
        let (mut events, folders) = self.plan_folders(guard, &missing)?;
        let id = self.assign(guard, target, Kind::File)?;
        events.push(EventBody::FileCopied {
            id,
            source_id,
            path: target.display(),
        });
        let op = PendingOp::Copy {
            id,
            source_id,
            source: source.display(),
            path: target.display(),
            md5,
            size,
            folders,
        };
        Ok(Commit::new(origin, device, op, events))
    }

    /// Ids and `folder.created` events for folders to create (parents first).
    fn plan_folders(
        &self,
        guard: &StoreGuard,
        missing: &[WirePath],
    ) -> Result<(Vec<EventBody>, Vec<PlannedFolder>)> {
        let mut events = Vec::new();
        let mut folders = Vec::new();
        for folder in missing {
            // A new folder never takes the id of one that vanished out of band.
            self.drop_stale_row(guard, folder, None)?;
            let id = self.assign(guard, folder, Kind::Folder)?;
            events.push(EventBody::FolderCreated {
                id,
                path: folder.display(),
            });
            folders.push(PlannedFolder {
                id,
                path: folder.display(),
            });
        }
        Ok((events, folders))
    }

    /// Drop the registry row of `path` (and its subtree) when nothing is on disk there: the
    /// row of a file or folder removed out of band. `keep` is never dropped.
    fn drop_stale_row(&self, guard: &StoreGuard, path: &WirePath, keep: Option<u64>) -> Result<()> {
        if let Some(other) = self.id_of(path)?
            && Some(other) != keep
            && std::fs::symlink_metadata(self.fs_path(path)).is_err()
        {
            self.removed(guard, other)?;
        }
        Ok(())
    }

    /// The folders that are empty once `gone` has left them, deepest first: the parent of
    /// `gone` if its only other entries are dot-files ([`is_dot_entry`]), then its parent in
    /// the same way, and so on. It stops at the root, at a native folder or one of their
    /// parents ([`is_native_folder`]), and at a folder that `keep` (a move target) lies in. The
    /// second value tells whether any of them holds dot-files.
    fn plan_prune(
        &self,
        guard: &StoreGuard,
        gone: &WirePath,
        keep: Option<&WirePath>,
    ) -> Result<(Vec<PlannedFolder>, bool)> {
        let mut out = Vec::new();
        let mut dots = false;
        let mut cur = gone.clone();
        while let Some(parent) = cur.parent() {
            if parent.is_root()
                || is_native_folder(&parent)
                || keep.is_some_and(|k| k.starts_with(&parent))
            {
                break;
            }
            match self.resolve(&parent)? {
                Some(f) if f.kind == Kind::Folder => {}
                _ => break,
            }
            let Some(here) = folder_rest(&self.fs_path(&parent), cur.name(), true)? else {
                break;
            };
            dots |= here;
            let id = self.assign(guard, &parent, Kind::Folder)?;
            out.push(PlannedFolder {
                id,
                path: parent.display(),
            });
            cur = parent;
        }
        Ok((out, dots))
    }

    /// The recycle slot ms for pruned folders that hold dot-files: the first free ms at or
    /// after `from_ms` for the topmost one. `None` without dot-files.
    fn prune_slot(
        &self,
        guard: &StoreGuard,
        prune: &[PlannedFolder],
        dots: bool,
        from_ms: u64,
    ) -> Result<Option<u64>> {
        match (dots, prune.last()) {
            (true, Some(top)) => Ok(Some(self.recycle_slot(
                guard,
                &wire(&top.path)?,
                from_ms,
            )?)),
            _ => Ok(None),
        }
    }

    /// For a rename within one folder that only changes case or Unicode normalization: check the
    /// new name against the other entries ([`Store::check_rename`]) and return `Some(via)`,
    /// where `via` is a temp name when the file system ignores the difference (it finds the
    /// source under the new name), else `None`. `None` for any other move.
    fn plan_case_rename(
        &self,
        guard: &StoreGuard,
        from: &WirePath,
        to: &WirePath,
    ) -> Result<Option<Option<String>>> {
        let (Some(parent), Some(to_parent)) = (from.parent(), to.parent()) else {
            return Ok(None);
        };
        let (Some(old), Some(new)) = (from.name(), to.name()) else {
            return Ok(None);
        };
        if parent != to_parent || old == new || super::fold(old) != super::fold(new) {
            return Ok(None);
        }
        self.check_rename(guard, &parent, new, old)?;
        if std::fs::symlink_metadata(self.fs_path(to)).is_err() {
            return Ok(Some(None));
        }
        let dir = self.fs_path(&parent);
        loop {
            let via = super::move_temp_name();
            if std::fs::symlink_metadata(dir.join(&via)).is_err() {
                return Ok(Some(Some(via)));
            }
        }
    }

    /// Plan renaming the file or folder `from` to the free path `to` (the id stays):
    /// `folder.created` for each missing parent of `to` (parents first), `file.moved` or
    /// `folder.moved`, then `folder.deleted` for each source folder the move leaves empty
    /// (deepest first, never a native folder). A stale registry row of a vanished entry at `to`
    /// is dropped first. A rename that only changes case or Unicode normalization is allowed;
    /// on a file system that ignores the difference it goes through a temp name. Native folders
    /// cannot be moved. Pruned folders that hold dot-files go to the recycle bin with a slot
    /// from the current time ([`Store::plan_move_at`] takes the time).
    pub fn plan_move(
        &self,
        guard: &WriteGuard,
        from: &WirePath,
        to: &WirePath,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.plan_move_at(guard, from, to, now, origin, device)
    }

    /// [`Store::plan_move`] at the time `at_ms` (the recycle slot of pruned folders with
    /// dot-files).
    pub fn plan_move_at(
        &self,
        guard: &WriteGuard,
        from: &WirePath,
        to: &WirePath,
        at_ms: u64,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_write(guard)?;
        let Some(found) = self.resolve(from)? else {
            return Err(invalid("source", format!("{from} is not in the store")));
        };
        if from.is_root() || to.is_root() {
            return Err(invalid("path", "the root cannot be moved".into()));
        }
        if is_native_folder(from) {
            return Err(invalid("path", format!("{from} is a native folder")));
        }
        if found.kind == Kind::Folder && to.starts_with(from) {
            return Err(invalid(
                "move",
                format!("cannot move folder {from} into itself ({to})"),
            ));
        }
        let parent = to
            .parent()
            .ok_or_else(|| invalid("path", "no parent".into()))?;
        let case = self.plan_case_rename(guard, from, to)?;
        let missing = match case {
            Some(_) => Vec::new(),
            None => {
                let missing = self.missing_folders(&parent)?;
                self.require_free(guard, to)?;
                missing
            }
        };
        let id = self.assign(guard, from, found.kind)?;
        self.drop_stale_row(guard, to, Some(id))?;
        let (prune, dots) = self.plan_prune(guard, from, Some(to))?;
        let prune_at = self.prune_slot(guard, &prune, dots, at_ms)?;
        let (mut events, folders) = self.plan_folders(guard, &missing)?;
        let (path, old_path) = (to.display(), from.display());
        events.push(match found.kind {
            Kind::File => EventBody::FileMoved { id, path, old_path },
            Kind::Folder => EventBody::FolderMoved { id, path, old_path },
        });
        events.extend(prune_events(&prune));
        let op = PendingOp::Move {
            id,
            kind: found.kind,
            from: from.display(),
            to: to.display(),
            folders,
            via: case.flatten(),
            prune,
            prune_at,
        };
        Ok(Commit::new(origin, device, op, events))
    }

    /// Plan moving the file or folder `path` into `recycle/<ms>/<path>`, where `ms` is
    /// `deleted_at_ms` or the next free millisecond ([`Store::recycle_slot`]), with its index
    /// line (`reason: "deleted"`). Events: for a folder first `file.deleted` / `folder.deleted`
    /// for everything inside it (children before their folder), then `file.deleted` or
    /// `folder.deleted` for `path`, then `folder.deleted` for each folder the delete leaves empty
    /// (deepest first, never a native folder). Empty pruned folders are removed for good; when
    /// they hold dot-files, the topmost one moves with them to its own recycle slot
    /// (`prune_at`) and gets an index line. Native folders cannot be deleted. Plan and commit one delete at a time: the slot
    /// is picked by probing, so two planned deletes could pick the same one.
    pub fn plan_delete(
        &self,
        guard: &WriteGuard,
        path: &WirePath,
        deleted_at_ms: u64,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_write(guard)?;
        let Some(found) = self.resolve(path)? else {
            return Err(invalid("path", format!("{path} is not in the store")));
        };
        if path.is_root() || is_native_folder(path) {
            return Err(invalid("path", format!("{path} is a native folder")));
        }
        let display = path.display();
        let ms = self.recycle_slot(guard, path, deleted_at_ms)?;
        let md5 = match found.kind {
            Kind::File => self.md5_of(guard, path)?,
            Kind::Folder => String::new(),
        };
        let id = self.assign(guard, path, found.kind)?;
        let mut events = Vec::new();
        let mut inside = Vec::new();
        if found.kind == Kind::Folder {
            // The listing comes parents first; reversed, every child precedes its folder.
            for e in self.list(guard, path, true)?.into_iter().rev() {
                if e.tag == Kind::File {
                    // Each file gets its own index line: its bytes are at that slot too.
                    inside.push(RecycleEntry {
                        id: e.id,
                        path: e.path_display.clone(),
                        md5: e.content_hash.clone().unwrap_or_default(),
                        deleted_at: ms,
                        reason: RecycleReason::Deleted,
                        pruned: false,
                    });
                }
                events.push(match e.tag {
                    Kind::File => EventBody::FileDeleted {
                        id: e.id,
                        recycle: format!("{ms}/{}", e.path_display),
                        path: e.path_display,
                    },
                    Kind::Folder => EventBody::FolderDeleted {
                        id: e.id,
                        path: e.path_display,
                    },
                });
            }
        }
        let entry = RecycleEntry {
            id,
            path: display.clone(),
            md5,
            deleted_at: ms,
            reason: RecycleReason::Deleted,
            pruned: false,
        };
        events.push(match found.kind {
            Kind::File => EventBody::FileDeleted {
                id,
                path: display.clone(),
                recycle: entry.location(),
            },
            Kind::Folder => EventBody::FolderDeleted {
                id,
                path: display.clone(),
            },
        });
        let (prune, dots) = self.plan_prune(guard, path, None)?;
        // Not the delete's own ms: its slot holds the parents of the deleted item.
        let prune_at = self.prune_slot(guard, &prune, dots, ms + 1)?;
        events.extend(prune_events(&prune));
        let op = PendingOp::Delete {
            id,
            kind: found.kind,
            path: display,
            recycle: entry.location(),
            entry: Some(entry),
            inside,
            prune,
            prune_at,
        };
        Ok(Commit::new(origin, device, op, events))
    }

    /// Plan bringing the recycle bin item `item` back at `target` (its old path or another
    /// free one) with new ids: `folder.created` for each missing parent of `target` (parents
    /// first), then `file.created` or `folder.created` for `target`, and for a folder the same
    /// for everything in it, parents before children. `origin` is the origin of the events
    /// (no device).
    ///
    /// The content comes from a walk of the slot, not from the index: a file taken out of the
    /// slot by hand gets no event, and dot names, names the wire can't carry and special files
    /// move along without one, as a listing skips them. The MD5 of a file is the one of its
    /// line when it has one, else the file is hashed. Every file is read before the first
    /// registry row is planned, so a file that can't be read refuses the restore without a
    /// trace. `_now_ms` is the time of the restore; no part of the plan depends on it yet.
    ///
    /// Refused: a slot that is gone (`Error::NotFound`), a slot that is, holds or lies below a
    /// symlink (`Error::RecycleSlotDamaged`), a target that exists (also in another spelling) or
    /// lies below a file (`Error::Invalid`, `Error::NameCollision`), a hidden target and the
    /// root. Plan the page records with [`crate::pages::plan_page_restore`] before the commit.
    pub fn plan_restore(
        &self,
        guard: &WriteGuard,
        item: &super::RecycleItem,
        target: &WirePath,
        _now_ms: u64,
        origin: Origin,
    ) -> Result<Commit> {
        self.check_write(guard)?;
        let slot = self.recycle_path(&item.entry)?;
        let location = item.entry.location();
        let damaged = || Error::RecycleSlotDamaged(location.clone());
        if self.has_symlink(&slot) {
            return Err(damaged());
        }
        let meta = match std::fs::symlink_metadata(&slot) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("recycle bin item {location}")));
            }
            Err(e) => return Err(io_err(&slot)(e)),
        };
        let kind = if meta.is_dir() {
            if super::recycle::holds_symlink(&slot) {
                return Err(damaged());
            }
            Kind::Folder
        } else if meta.is_file() {
            Kind::File
        } else {
            return Err(damaged());
        };
        if target.is_root() || target.is_hidden() {
            return Err(invalid(
                "path",
                format!("{target} cannot be a restore target"),
            ));
        }
        let parent = target
            .parent()
            .ok_or_else(|| invalid("path", "no parent".into()))?;
        let missing = self.missing_folders(&parent)?;
        self.require_free(guard, target)?;
        // What the slot holds, as a listing would see it at the target, parents first.
        let mut walked = Vec::new();
        if kind == Kind::Folder {
            let found = list::Found {
                path: target.clone(),
                fs_path: slot.clone(),
                kind,
                meta,
            };
            list::walk(&found, true, &mut walked, &|_| None)?;
            walked.sort_by(|a, b| a.path.segments().cmp(b.path.segments()));
        }
        // The lines of the files that were in a folder, by their path below it.
        let below = |path: &str, top: &str| -> Option<String> {
            path.strip_prefix(top)?
                .strip_prefix('/')
                .map(str::to_string)
        };
        let owned: std::collections::HashMap<String, &RecycleEntry> = item
            .owned
            .iter()
            .filter_map(|e| Some((below(&e.path, &item.entry.path)?, e)))
            .collect();
        let display = target.display();
        // Every read of the slot comes first: a failure here plans no registry row.
        let top = match kind {
            Kind::File => Some(restored_file(&slot, Some(&item.entry))?),
            Kind::Folder => None,
        };
        let mut facts = Vec::with_capacity(walked.len());
        for found in &walked {
            facts.push(match found.kind {
                Kind::Folder => None,
                Kind::File => {
                    let path = found.path.display();
                    let line = below(&path, &display).and_then(|rel| owned.get(&rel).copied());
                    Some(restored_file(&found.fs_path, line)?)
                }
            });
        }
        // Then the rows and the events.
        let (mut events, folders) = self.plan_folders(guard, &missing)?;
        self.drop_stale_row(guard, target, None)?;
        let id = self.assign(guard, target, kind)?;
        let mut reid = Vec::new();
        events.push(match top {
            Some(file) => file.event(id, &display, &mut reid),
            None => EventBody::FolderCreated {
                id,
                path: display.clone(),
            },
        });
        let mut inside = Vec::with_capacity(walked.len());
        for (found, file) in walked.iter().zip(facts) {
            self.drop_stale_row(guard, &found.path, None)?;
            let child = self.assign(guard, &found.path, found.kind)?;
            let path = found.path.display();
            events.push(match file {
                Some(file) => file.event(child, &path, &mut reid),
                None => EventBody::FolderCreated {
                    id: child,
                    path: path.clone(),
                },
            });
            inside.push(PlannedEntry {
                id: child,
                kind: found.kind,
                path,
            });
        }
        let mut lines = vec![item.entry.clone()];
        lines.extend(item.owned.iter().cloned());
        let op = PendingOp::Restore {
            kind,
            recycle: location,
            to: display,
            folders,
            id,
            inside,
            lines,
            reid,
        };
        Ok(Commit::new(origin, None, op, events))
    }

    /// Plan creating the folder `path` (its parent must exist): `folder.created`.
    pub fn plan_create_folder(
        &self,
        guard: &WriteGuard,
        path: &WirePath,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_write(guard)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("path", "the root exists".into()))?;
        self.require_folder(&parent, "parent folder")?;
        self.require_free(guard, path)?;
        self.drop_stale_row(guard, path, None)?;
        let id = self.assign(guard, path, Kind::Folder)?;
        let event = EventBody::FolderCreated {
            id,
            path: path.display(),
        };
        let op = PendingOp::CreateFolder {
            id,
            path: path.display(),
        };
        Ok(Commit::new(origin, device, op, vec![event]))
    }

    /// Create every missing native folder and parent ([`super::NATIVE_FOLDERS`], parents
    /// first), one `plan_create_folder` commit each, so each gets its `folder.created` event
    /// with this origin. Existing folders are kept. A file or another spelling in the way is an
    /// error. Returns how many folders were created.
    pub fn create_native_folders(
        &self,
        guard: &WriteGuard,
        journal: &Journal,
        origin: Origin,
        device: Option<String>,
    ) -> Result<u64> {
        let mut created = 0;
        for folder in super::NATIVE_FOLDERS {
            let path = WirePath::parse_named(folder)?;
            if self.resolve(&path)?.is_some_and(|f| f.kind == Kind::Folder) {
                continue;
            }
            let commit = self.plan_create_folder(guard, &path, origin, device.clone())?;
            self.commit(guard, commit, journal)?;
            created += 1;
        }
        Ok(created)
    }

    /// Journal `folder.created` (`origin: "cli"`, one `opId`) for each native folder of this
    /// account whose id has no `folder.created` event yet (folders made before the events
    /// existed, or by hand). Nothing on disk changes. Returns how many events were appended.
    pub fn journal_native_folders(&self, guard: &WriteGuard, journal: &Journal) -> Result<u64> {
        self.check_write(guard)?;
        let mut native = Vec::new();
        for folder in super::NATIVE_FOLDERS {
            let path = WirePath::parse_named(folder)?;
            if let Some(e) = self.entry(guard, &path)?
                && e.tag == Kind::Folder
            {
                native.push((e.id, e.path_display));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for event in journal.read_from(0)? {
            let event = event?;
            if event.account == self.slug
                && let EventBody::FolderCreated { id, .. } = event.body
            {
                seen.insert(id);
            }
        }
        let op_id = new_op_id();
        let mut events: Vec<Event> = native
            .into_iter()
            .filter(|(id, _)| !seen.contains(id))
            .map(|(id, path)| {
                Event::new(
                    &self.slug,
                    Origin::Cli,
                    None,
                    &op_id,
                    EventBody::FolderCreated { id, path },
                )
            })
            .collect();
        if events.is_empty() {
            return Ok(0);
        }
        journal.append(&mut events)?;
        Ok(events.len() as u64)
    }

    // ------------------------------------------------------------------ commit

    /// Run the commit protocol (see the module docs) and return the `seq` values of its events.
    /// An unfinished earlier commit (a `pending.json`) is recovered first.
    pub fn commit(
        &self,
        guard: &WriteGuard,
        commit: Commit,
        journal: &Journal,
    ) -> Result<Vec<u64>> {
        self.commit_until(guard, commit, journal, None)
    }

    /// [`Store::commit`] that stops at `crash`, as a crash there would. Test hook.
    #[doc(hidden)]
    pub fn commit_until(
        &self,
        guard: &WriteGuard,
        mut commit: Commit,
        journal: &Journal,
        crash: Option<CrashPoint>,
    ) -> Result<Vec<u64>> {
        self.check_write(guard)?;
        if matches!(commit.op, PendingOp::Unchanged { .. }) {
            return Ok(Vec::new());
        }
        // 1. intent, with the journal position recovery searches from
        commit.journal_seq = Some(journal.last_seq()?);
        super::write_atomic_short(&self.pending_path(), &crate::fsutil::json_bytes(&commit))?;
        if crash == Some(CrashPoint::BeforeApply) {
            return Ok(Vec::new());
        }
        // 2. data change, then the registry
        let stop = crash == Some(CrashPoint::BeforeRename) && has_rename_stop(&commit.op);
        let mut written = None;
        if let Err(e) = self.apply(&commit.op, stop, crash, &mut written) {
            match self.is_applied(guard, &commit.op) {
                Ok(true) => {}
                Ok(false) => {
                    // The intent goes only when the old state is confirmed. An entry left under
                    // the temp name of a rename keeps it, and recovery finishes the rename.
                    if self.undo_unapplied(guard, &commit.op).is_ok() {
                        self.clear_pending()?;
                    }
                    return Err(e);
                }
                // Unknown state: keep the intent for recovery.
                Err(_) => return Err(e),
            }
        }
        if stop || crash == Some(CrashPoint::BeforeAppend) {
            return Ok(Vec::new());
        }
        if self.settle_prune(&mut commit.op, &mut commit.events)? {
            // A folder to prune was not empty: the intent must not promise its event.
            super::write_atomic_short(&self.pending_path(), &crate::fsutil::json_bytes(&commit))?;
        }
        let written = written.filter(|_| !commit.drop_md5);
        self.finish_registry(guard, &commit.op, written)?;
        self.finish_recycle(&commit.op)?;
        let mut batch = SideBatch::default();
        for w in &commit.side_writes {
            if let Err(e) = self.write_side(w, &mut batch) {
                eprintln!("warning: account {}: {e}", self.slug);
            }
        }
        self.sync_sides(batch);
        // 3. events
        let mut events = commit.journal_events(&self.slug, commit.origin, &commit.events);
        let seqs = journal.append(&mut events)?;
        if crash == Some(CrashPoint::BeforeClear) {
            return Ok(seqs);
        }
        // 4. done
        self.clear_pending()?;
        Ok(seqs)
    }

    /// Write a side file atomically (its folder is created if needed), or remove it. Only a
    /// file directly inside `<account>/pages/` is accepted: the path must be the account folder
    /// followed by exactly `pages` and one plain name (no `..`, `.`, root or prefix part). A
    /// regular file that already has the new bytes is left alone, and a missing file needs no
    /// removal. A change is visible right away, but its folder is only noted in `batch` (also
    /// for a file left alone or already gone): the caller syncs it with [`Store::sync_sides`]
    /// before it returns.
    pub(super) fn write_side(&self, w: &SideWrite, batch: &mut SideBatch) -> Result<()> {
        use std::path::Component;
        let confined = w.path.strip_prefix(&self.account_dir).is_ok_and(|rel| {
            let parts: Vec<Component<'_>> = rel.components().collect();
            matches!(
                parts.as_slice(),
                [Component::Normal(dir), Component::Normal(_)] if *dir == crate::pages::PAGES_DIR
            )
        });
        if !confined {
            return Err(invalid(
                "side file",
                format!(
                    "{} is not a file directly inside {}",
                    w.path.display(),
                    self.account_dir.join(crate::pages::PAGES_DIR).display()
                ),
            ));
        }
        let parent = w.path.parent().unwrap_or(&self.account_dir).to_path_buf();
        let Some(bytes) = &w.bytes else {
            return match std::fs::remove_file(&w.path) {
                Ok(()) => {
                    batch.dirs.insert(parent);
                    Ok(())
                }
                // Gone already: an earlier run may have died before its folder sync.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    batch.dirs.insert(parent);
                    Ok(())
                }
                Err(e) => Err(io_err(&w.path)(e)),
            };
        };
        self.create_dirs_durable(&parent)?;
        // Only a regular file of the same size is read: never a symlink, a FIFO or a folder.
        let same = std::fs::symlink_metadata(&w.path)
            .is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64)
            && std::fs::read(&w.path).is_ok_and(|old| old == *bytes);
        if same {
            // An earlier run may have died before its folder sync: sync it once here too.
            batch.dirs.insert(parent);
            return Ok(());
        }
        super::write_atomic_short_unsynced(&w.path, bytes)?;
        batch.dirs.insert(parent);
        Ok(())
    }

    /// Sync each folder of `batch` once (see [`Store::write_side`]). A failure is reported on
    /// stderr, like a failed side write.
    pub(super) fn sync_sides(&self, batch: SideBatch) {
        for dir in batch.dirs {
            if let Err(e) = crate::fsutil::fsync_dir(&dir) {
                eprintln!("warning: account {}: {e}", self.slug);
            }
        }
    }

    /// Perform the data change durably. `stop_before_rename` (test hook) ends a `put` after its
    /// folders and recycle copy, and a move through a temp name between its two renames, as a
    /// crash there would. `hook` (test hook) can also make the second rename of such a move
    /// fail, or place a file at its target first. For a `put`, `written` gets the file facts of
    /// the target when the rename kept the staged file as it was (see [`WrittenFile`]).
    fn apply(
        &self,
        op: &PendingOp,
        stop_before_rename: bool,
        hook: Option<CrashPoint>,
        written: &mut Option<WrittenFile>,
    ) -> Result<()> {
        match op {
            PendingOp::Put {
                path,
                staged,
                folders,
                recycle,
                size,
                ..
            } => {
                let target = self.fs_path(&wire(path)?);
                let staged = self.account_dir.join(safe_relative(staged, "staged file")?);
                self.create_planned(folders)?;
                if let Some(entry) = recycle {
                    // A copy: the old version stays listed until the rename replaces it.
                    self.copy_into_recycle(&target, entry)?;
                }
                if stop_before_rename {
                    return Ok(());
                }
                std::fs::File::open(&staged)
                    .and_then(|f| f.sync_all())
                    .map_err(io_err(&staged))?;
                // A rename keeps size and mtime: the target is the staged file as it was seen
                // here when both stats agree (and with the size the put promised).
                let before = std::fs::metadata(&staged).map_err(io_err(&staged))?;
                crate::fsutil::rename_durable(&staged, &target)?;
                if let Ok(after) = std::fs::metadata(&target)
                    && after.len() == before.len()
                    && after.len() == *size
                    && super::hashes::mtime_ns(&after) == super::hashes::mtime_ns(&before)
                {
                    *written = Some(WrittenFile {
                        size: after.len(),
                        mtime_ns: super::hashes::mtime_ns(&after),
                        ctime_ns: super::hashes::ctime_ns(&after),
                    });
                }
                Ok(())
            }
            PendingOp::Copy {
                source,
                path,
                folders,
                ..
            } => {
                self.create_planned(folders)?;
                let from = self.fs_path(&wire(source)?);
                let target = self.fs_path(&wire(path)?);
                let dir = target.parent().unwrap_or(&self.files).to_path_buf();
                let (tmp, mut out) = super::create_temp(&dir).map_err(io_err(&dir))?;
                let copied = (|| -> std::io::Result<()> {
                    let mut input = std::fs::File::open(&from)?;
                    std::io::copy(&mut input, &mut out)?;
                    out.sync_all()?;
                    if std::fs::symlink_metadata(&target).is_ok() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            "the copy target exists",
                        ));
                    }
                    std::fs::rename(&tmp, &target)
                })();
                if let Err(e) = copied {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(io_err(&target)(e));
                }
                crate::fsutil::fsync_dir(&dir)
            }
            PendingOp::Move {
                from,
                to,
                folders,
                via,
                ..
            } => {
                self.create_planned(folders)?;
                let (from_path, to_path) = (wire(from)?, wire(to)?);
                let to = self.fs_path(&to_path);
                let Some(via) = via else {
                    if std::fs::symlink_metadata(&to).is_ok() {
                        return Err(invalid("move", format!("{} exists", to.display())));
                    }
                    return crate::fsutil::rename_durable(&self.fs_path(&from_path), &to);
                };
                // A rename that only changes case, on a file system that ignores case: from
                // the old name to the temp name, then to the new name. Each step is durable,
                // and recovery finishes a move that stopped between them.
                let via = self.via_path(&from_path, via)?;
                if self.resolve(&from_path)?.is_some() {
                    crate::fsutil::rename_durable(&self.fs_path(&from_path), &via)?;
                }
                if stop_before_rename {
                    return Ok(());
                }
                match hook {
                    Some(CrashPoint::FailSecondRename) => {
                        return Err(invalid(
                            "move",
                            "the second rename failed (test hook)".into(),
                        ));
                    }
                    Some(CrashPoint::TakeTargetBetweenRenames) => {
                        std::fs::write(&to, b"out of band").map_err(io_err(&to))?;
                    }
                    _ => {}
                }
                self.finish_via(&to_path, &via)
            }
            PendingOp::Delete { path, recycle, .. } => {
                let from = self.fs_path(&wire(path)?);
                let root = self.account_dir.join(RECYCLE_DIR);
                let target = root.join(safe_relative(recycle, "recycle path")?);
                let parent = target.parent().unwrap_or(&root).to_path_buf();
                self.create_dirs_durable(&parent)?;
                crate::fsutil::rename_durable(&from, &target)
            }
            PendingOp::CreateFolder { path, .. } => {
                let target = self.fs_path(&wire(path)?);
                std::fs::create_dir(&target).map_err(io_err(&target))?;
                crate::fsutil::fsync_dir(target.parent().unwrap_or(&self.files))
            }
            PendingOp::Restore {
                recycle,
                to,
                folders,
                ..
            } => {
                let slot = self.restore_slot(recycle)?;
                self.create_planned(folders)?;
                if stop_before_rename {
                    return Ok(());
                }
                let to = self.fs_path(&wire(to)?);
                if std::fs::symlink_metadata(&to).is_ok() {
                    return Err(invalid("restore", format!("{} exists", to.display())));
                }
                crate::fsutil::rename_durable(&slot, &to)
            }
            PendingOp::Unchanged { .. } => Ok(()),
        }
    }

    /// The slot of a restore on disk; the path comes from the intent, so it is checked again.
    fn restore_slot(&self, recycle: &str) -> Result<PathBuf> {
        Ok(self
            .account_dir
            .join(RECYCLE_DIR)
            .join(safe_relative(recycle, "recycle path")?))
    }

    /// `create_dir_all` below the account folder, with an fsync of each new folder's parent.
    pub(super) fn create_dirs_durable(&self, dir: &Path) -> Result<()> {
        let mut missing = Vec::new();
        let mut cur = dir.to_path_buf();
        while !cur.is_dir() {
            missing.push(cur.clone());
            match cur.parent() {
                Some(p) if p.starts_with(&self.account_dir) => cur = p.to_path_buf(),
                _ => break,
            }
        }
        for d in missing.iter().rev() {
            match std::fs::create_dir(d) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(d)(e)),
            }
            crate::fsutil::fsync_dir(d.parent().unwrap_or(&self.account_dir))?;
        }
        Ok(())
    }

    /// Whether the data change of `op` is in place (see [`PendingOp`]).
    fn is_applied(&self, guard: &StoreGuard, op: &PendingOp) -> Result<bool> {
        Ok(match op {
            PendingOp::Put { path, md5, .. }
            | PendingOp::Copy { path, md5, .. }
            | PendingOp::Unchanged { path, md5, .. } => {
                let path = wire(path)?;
                match self.resolve(&path)? {
                    Some(f) if f.kind == Kind::File => self.md5_of(guard, &path)? == *md5,
                    _ => false,
                }
            }
            PendingOp::Move { from, to, via, .. } => {
                let from = wire(from)?;
                // An entry still under the temp name of a rename is not at `to`, whatever is
                // there: the move is not done.
                let at_via = match via {
                    Some(via) => std::fs::symlink_metadata(self.via_path(&from, via)?).is_ok(),
                    None => false,
                };
                !at_via && self.resolve(&from)?.is_none() && self.resolve(&wire(to)?)?.is_some()
            }
            PendingOp::Delete { path, recycle, .. } => {
                let target = self
                    .account_dir
                    .join(RECYCLE_DIR)
                    .join(safe_relative(recycle, "recycle path")?);
                self.resolve(&wire(path)?)?.is_none() && std::fs::symlink_metadata(target).is_ok()
            }
            PendingOp::CreateFolder { path, .. } => {
                matches!(self.resolve(&wire(path)?)?, Some(f) if f.kind == Kind::Folder)
            }
            PendingOp::Restore { recycle, to, .. } => {
                std::fs::symlink_metadata(self.restore_slot(recycle)?).is_err()
                    && self.resolve(&wire(to)?)?.is_some()
            }
        })
    }

    /// Bring the id registry (and for a put the MD5 cache) in line with an applied change.
    /// Idempotent. `written` is only known to the normal commit: recovery passes `None`.
    fn finish_registry(
        &self,
        guard: &StoreGuard,
        op: &PendingOp,
        written: Option<WrittenFile>,
    ) -> Result<()> {
        match op {
            // The id was assigned (durably) while planning, before the intent was written. The
            // cached MD5 of a replaced file is the old one: it goes, or the next scan would
            // journal the put again as a change made by hand. When the commit saw the file it
            // wrote, the row of that file takes its place, with the ctime as the baseline.
            PendingOp::Put { path, md5, .. } => match written {
                Some(w) => self.remember_md5(guard, &wire(path)?, w, md5),
                None => self.forget_md5(guard, &wire(path)?),
            },
            // A restore's rows were assigned while planning, and its paths have no cached MD5
            // (the next listing or scan hashes them without an event).
            PendingOp::Copy { .. }
            | PendingOp::CreateFolder { .. }
            | PendingOp::Restore { .. }
            | PendingOp::Unchanged { .. } => Ok(()),
            PendingOp::Move { id, to, prune, .. } => {
                let to = wire(to)?;
                let known = self.path_of(*id)?;
                // An unknown id (a registry reset): the new path gets an id at the next listing.
                if known.is_some() && known.as_ref() != Some(&to) {
                    if let Some(other) = self.id_of(&to)?
                        && other != *id
                    {
                        // A stale row of something that was at `to` before (the path was free).
                        self.removed(guard, other)?;
                    }
                    self.moved(guard, *id, &to)?;
                }
                self.finish_prune_rows(guard, prune)
            }
            PendingOp::Delete { id, prune, .. } => {
                if self.path_of(*id)?.is_some() {
                    self.removed(guard, *id)?;
                }
                self.finish_prune_rows(guard, prune)
            }
        }
    }

    /// Drop the registry rows of the pruned folders that still name them. Idempotent.
    fn finish_prune_rows(&self, guard: &StoreGuard, prune: &[PlannedFolder]) -> Result<()> {
        for p in prune {
            let Ok(path) = wire(&p.path) else {
                continue;
            };
            if self.path_of(p.id)?.as_ref() == Some(&path) {
                self.removed(guard, p.id)?;
            }
        }
        Ok(())
    }

    /// Prune the folders a `move` or `delete` left empty (`prune`, deepest first). First it
    /// finds how many of them can still go: each must be gone already, or hold nothing but the
    /// next deeper one and (with `prune_at`) dot-files. A folder that holds more stays, and so
    /// do the folders above it: they are taken out of `prune`, and their `folder.deleted` events
    /// out of `events`. Then, when the rest holds dot-files, the topmost one moves with
    /// everything in it to `recycle/<prune_at>/<path>`; otherwise each is removed, with an
    /// fsync of its parent, and `prune_at` is cleared. Returns whether `op` or `events` changed.
    /// Idempotent: folders that are gone already count as pruned.
    fn settle_prune(&self, op: &mut PendingOp, events: &mut Vec<EventBody>) -> Result<bool> {
        let (PendingOp::Move {
            prune, prune_at, ..
        }
        | PendingOp::Delete {
            prune, prune_at, ..
        }) = op
        else {
            return Ok(false);
        };
        let mut can_go = 0;
        let mut dots = false;
        let mut present = false;
        let mut below: Option<String> = None;
        for p in prune.iter() {
            let Ok(path) = wire(&p.path) else {
                break;
            };
            let dir = self.fs_path(&path);
            match std::fs::symlink_metadata(&dir) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&dir)(e)),
                Ok(m) if !m.is_dir() => break,
                Ok(_) => match folder_rest(&dir, below.as_deref(), prune_at.is_some())? {
                    Some(here) => {
                        dots |= here;
                        present = true;
                    }
                    None => break,
                },
            }
            can_go += 1;
            below = path.name().map(str::to_string);
        }
        let mut changed = false;
        if can_go < prune.len() {
            let kept = prune.split_off(can_go);
            events.retain(|e| {
                !matches!(e, EventBody::FolderDeleted { id, path }
                    if kept.iter().any(|k| k.id == *id && k.path == *path))
            });
            changed = true;
        }
        if !present {
            // Nothing left on disk (also after an earlier run of this step).
            return Ok(changed);
        }
        match (dots, *prune_at, prune.last()) {
            (true, Some(ms), Some(top)) => {
                let entry = prune_entry(top, ms);
                self.move_into_recycle(&self.fs_path(&wire(&top.path)?), &entry)?;
            }
            _ => {
                for p in prune.iter() {
                    let dir = self.fs_path(&wire(&p.path)?);
                    match std::fs::remove_dir(&dir) {
                        Ok(()) => crate::fsutil::fsync_dir(dir.parent().unwrap_or(&self.files))?,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err(&dir)(e)),
                    }
                }
                if prune_at.take().is_some() {
                    changed = true;
                }
            }
        }
        Ok(changed)
    }

    /// The temp name of a rename that only changes case, as a path next to the source. The name
    /// comes from the intent, so it is checked again.
    fn via_path(&self, from: &WirePath, via: &str) -> Result<PathBuf> {
        if !super::is_move_temp_name(via) {
            return Err(invalid(
                "move",
                format!("{via:?} is not a temp name of ours"),
            ));
        }
        let parent = from
            .parent()
            .ok_or_else(|| invalid("move", "the root has no folder".into()))?;
        Ok(self.fs_path(&parent).join(via))
    }

    /// The second step of a rename through a temp name: from the temp name to `to`. Nothing to
    /// do when the temp name is gone and `to` is there (the step is done). An error when `to`
    /// holds something while the entry is still under the temp name, and when neither exists:
    /// the intent then stays, and no move is journaled that did not happen.
    fn finish_via(&self, to: &WirePath, via: &Path) -> Result<()> {
        let at_via = std::fs::symlink_metadata(via).is_ok();
        let at_to = self.resolve(to)?.is_some();
        match (at_via, at_to) {
            (true, false) => crate::fsutil::rename_durable(via, &self.fs_path(to)),
            (false, true) => Ok(()),
            (true, true) => Err(invalid(
                "move",
                format!(
                    "{to} is taken by something else; the moved entry stays at {}",
                    via.display()
                ),
            )),
            (false, false) => Err(invalid(
                "move",
                format!("neither {to} nor the temp name {} exists", via.display()),
            )),
        }
    }

    /// Whether the entry of a move through a temp name is under none of its names (old, temp,
    /// new): removed out of band while the intent was pending.
    fn vanished(&self, op: &PendingOp) -> Result<bool> {
        let PendingOp::Move {
            from,
            to,
            via: Some(via),
            ..
        } = op
        else {
            return Ok(false);
        };
        let (from, to) = (wire(from)?, wire(to)?);
        let via = self.via_path(&from, via)?;
        Ok(self.resolve(&from)?.is_none()
            && self.resolve(&to)?.is_none()
            && std::fs::symlink_metadata(via).is_err())
    }

    /// Finish a move through a temp name that stopped between its two renames (recovery runs
    /// this before it checks whether the change is applied).
    fn resume(&self, op: &PendingOp) -> Result<()> {
        if let PendingOp::Move {
            from,
            to,
            via: Some(via),
            ..
        } = op
        {
            let (from, to) = (wire(from)?, wire(to)?);
            let via = self.via_path(&from, via)?;
            if self.resolve(&from)?.is_some() || self.vanished(op)? {
                // Not started, or the entry is gone from every name: nothing to finish, and
                // the change is discarded.
                return Ok(());
            }
            self.finish_via(&to, &via)?;
        }
        Ok(())
    }

    /// Create the planned folders (parents first) with an fsync of each parent. An existing
    /// folder is fine.
    fn create_planned(&self, folders: &[PlannedFolder]) -> Result<()> {
        for folder in folders {
            let dir = self.fs_path(&wire(&folder.path)?);
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
                Err(e) => return Err(io_err(&dir)(e)),
            }
            crate::fsutil::fsync_dir(dir.parent().unwrap_or(&self.files))?;
        }
        Ok(())
    }

    /// Remove planned folders while they are empty (deepest first). Failures are ignored.
    fn remove_planned(&self, folders: &[PlannedFolder]) {
        for folder in folders.iter().rev() {
            let Ok(path) = wire(&folder.path) else {
                continue;
            };
            let dir = self.fs_path(&path);
            if std::fs::remove_dir(&dir).is_ok() {
                let _ = crate::fsutil::fsync_dir(dir.parent().unwrap_or(&self.files));
            }
        }
    }

    /// Write the recycle index lines of an applied change in one append (idempotent, see
    /// [`Store::record_recycled`]): the old version of a put; the files inside a deleted folder,
    /// then the deleted item; and the topmost pruned folder when it went to the recycle bin
    /// (only if its slot is there). A restore instead drops its lines and moves the old
    /// versions to the new ids in one rewrite, then removes the emptied folders of its slot
    /// ([`Store::drop_recycled`], idempotent too).
    fn finish_recycle(&self, op: &PendingOp) -> Result<()> {
        if let PendingOp::Restore {
            recycle,
            lines,
            reid,
            ..
        } = op
        {
            self.drop_recycled(lines, reid)?;
            self.remove_slot_parents(&self.restore_slot(recycle)?);
            return Ok(());
        }
        let mut lines = Vec::new();
        let (prune, prune_at) = match op {
            PendingOp::Put {
                recycle: Some(entry),
                ..
            } => {
                lines.push(entry.clone());
                (&[][..], None)
            }
            PendingOp::Delete {
                entry,
                inside,
                prune,
                prune_at,
                ..
            } => {
                lines.extend(inside.iter().cloned());
                lines.extend(entry.iter().cloned());
                (&prune[..], *prune_at)
            }
            PendingOp::Move {
                prune, prune_at, ..
            } => (&prune[..], *prune_at),
            _ => (&[][..], None),
        };
        if let (Some(ms), Some(top)) = (prune_at, prune.last()) {
            let entry = prune_entry(top, ms);
            if std::fs::symlink_metadata(self.recycle_path(&entry)?).is_ok() {
                lines.push(entry);
            }
        }
        self.append_recycled_all(&lines)?;
        Ok(())
    }

    /// Clean up after a change that was not applied: remove the recycle copy the change made and
    /// the folders it created while they are empty (deepest first). Their registry rows stay, as
    /// for any discarded intent, except for a restore: it may have planned rows for a whole
    /// tree, so the rows that still name its planned paths are dropped. An error there (the
    /// registry can't be written) keeps the intent, so that recovery tries again. The other
    /// failures are ignored (the leftovers are harmless: an extra copy, an empty folder). An
    /// error when an entry is still under the temp name of a rename: then the intent must
    /// stay, so that recovery finishes the rename (the entry would otherwise stay hidden, and
    /// the device would delete its copy).
    fn undo_unapplied(&self, guard: &StoreGuard, op: &PendingOp) -> Result<()> {
        match op {
            PendingOp::Restore {
                to,
                folders,
                id,
                inside,
                ..
            } => {
                self.remove_planned(folders);
                // Every planned row that still names its path goes, also when something was
                // made there by hand meanwhile: no event created it, so the next scan journals
                // it as new. A folder row takes the rows below it along.
                let planned = folders
                    .iter()
                    .map(|f| (f.id, f.path.as_str()))
                    .chain(std::iter::once((*id, to.as_str())))
                    .chain(inside.iter().map(|e| (e.id, e.path.as_str())));
                for (row, path) in planned {
                    if let Ok(path) = wire(path)
                        && self.path_of(row)?.as_ref() == Some(&path)
                    {
                        self.removed(guard, row)?;
                    }
                }
            }
            PendingOp::Put {
                folders, recycle, ..
            } => {
                if let Some(entry) = recycle {
                    self.discard_recycled(entry);
                }
                self.remove_planned(folders);
            }
            PendingOp::Copy { folders, .. } => self.remove_planned(folders),
            PendingOp::Move {
                from, folders, via, ..
            } => {
                if let Some(via) = via {
                    let via = self.via_path(&wire(from)?, via)?;
                    if std::fs::symlink_metadata(&via).is_ok() {
                        return Err(invalid(
                            "move",
                            format!(
                                "{from} is still under the temp name {}; the intent stays for \
                                 recovery",
                                via.display()
                            ),
                        ));
                    }
                }
                self.remove_planned(folders);
            }
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------------ recovery

    /// Finish or discard an unfinished commit (see the module docs).
    pub fn recover(&self, guard: &StoreGuard, journal: &Journal) -> Result<Recovery> {
        self.check_guard(guard)?;
        self.recover_pending(guard, journal)
    }

    fn recover_pending(&self, guard: &StoreGuard, journal: &Journal) -> Result<Recovery> {
        let Some(mut commit) = self.pending()? else {
            return Ok(Recovery::Clean);
        };
        self.resume(&commit.op)?;
        if !self.is_applied(guard, &commit.op)? {
            if self.vanished(&commit.op)?
                && let PendingOp::Move { from, to, via, .. } = &commit.op
            {
                eprintln!(
                    "warning: account {}: the change {} renamed {from} to {to} through {}, but \
                     the entry is under none of these names (removed outside the server); the \
                     change is dropped and nothing is journaled",
                    self.slug,
                    commit.op_id,
                    via.as_deref().unwrap_or_default()
                );
            }
            self.undo_unapplied(guard, &commit.op)?;
            self.clear_pending()?;
            return Ok(Recovery::Discarded {
                op_id: commit.op_id,
            });
        }
        self.settle_prune(&mut commit.op, &mut commit.events)?;
        self.finish_registry(guard, &commit.op, None)?;
        self.finish_recycle(&commit.op)?;
        if matches!(commit.op, PendingOp::Restore { .. }) {
            // Side writes are not in the intent: the page records of the restored notes are
            // made again from their live paths, so the pipeline knows them again.
            let mut batch = SideBatch::default();
            for w in crate::pages::restore_records(self, &commit) {
                if let Err(e) = self.write_side(&w, &mut batch) {
                    eprintln!("warning: account {}: {e}", self.slug);
                }
            }
            self.sync_sides(batch);
        }
        let seqs = {
            let lock = journal.lock()?;
            let have = lock.count_op_after(&commit.op_id, commit.journal_seq.unwrap_or(0))?;
            let missing = commit.events.get(have..).unwrap_or(&[]);
            let mut events = commit.journal_events(&self.slug, Origin::Repair, missing);
            lock.append(&mut events)?
        };
        self.clear_pending()?;
        Ok(Recovery::Repaired {
            op_id: commit.op_id,
            seqs,
        })
    }

    /// Journal the current state for a new consumer: `folder.created` and `file.created` for
    /// every listed entry (parents first), as one batch with one `opId` and `origin: "snapshot"`.
    /// A `.note` gets `pagesChanged` and one `page.added` per page right after its
    /// `file.created` (or `page.index_failed`), and its page record is written now, under the
    /// same guard and before the append, so the records agree with the journal. After the
    /// files come `digest.created` for every live digest and `task.created` for every live task
    /// (see [`crate::appdata`]). Returns the `seq` values.
    pub fn snapshot(&self, guard: &WriteGuard, journal: &Journal) -> Result<Vec<u64>> {
        self.check_write(guard)?;
        let entries = self.list(guard, &WirePath::root(), true)?;
        // Read the app data first: a failure here leaves no page record without its events.
        let app_data = crate::appdata::AppData::new(self).snapshot_bodies(guard)?;
        let op_id = new_op_id();
        let mut events: Vec<Event> = Vec::new();
        // Each record is in place before the next note picks its key (it reads the records).
        let mut batch = SideBatch::default();
        for e in entries {
            let mut bodies = Vec::new();
            match e.tag {
                Kind::Folder => bodies.push(EventBody::FolderCreated {
                    id: e.id,
                    path: e.path_display,
                }),
                Kind::File => {
                    let kind = FileKind::of(&e.name);
                    let md5 = e.content_hash.clone().unwrap_or_default();
                    let (pages_changed, page_events, record) = if kind == FileKind::Note {
                        crate::pages::snapshot_note(self, &e.path, e.id, &md5, e.size)
                    } else {
                        (None, Vec::new(), None)
                    };
                    if let Some(w) = record
                        && let Err(err) = self.write_side(&w, &mut batch)
                    {
                        eprintln!("warning: account {}: {err}", self.slug);
                    }
                    bodies.push(EventBody::FileCreated {
                        id: e.id,
                        kind,
                        path: e.path_display,
                        md5,
                        size: e.size,
                        pages_changed,
                    });
                    bodies.extend(page_events);
                }
            }
            events.extend(
                bodies
                    .into_iter()
                    .map(|body| Event::new(&self.slug, Origin::Snapshot, None, &op_id, body)),
            );
        }
        events.extend(
            app_data
                .into_iter()
                .map(|body| Event::new(&self.slug, Origin::Snapshot, None, &op_id, body)),
        );
        self.sync_sides(batch);
        journal.append(&mut events)
    }

    /// Remove `.nd-*.tmp` files older than [`STALE_TEMP_AGE`] anywhere below `files/`. Symlinks
    /// are not followed. Returns how many were removed. The caller holds the store lock.
    pub fn sweep_temps(&self) -> Result<usize> {
        fn walk(dir: &Path, now: SystemTime, removed: &mut usize) -> Result<()> {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(io_err(dir)(e)),
            };
            for entry in entries {
                let entry = entry.map_err(io_err(dir))?;
                let ft = entry.file_type().map_err(io_err(dir))?;
                let path = entry.path();
                if ft.is_dir() {
                    walk(&path, now, removed)?;
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !(name.starts_with(".nd-") && name.ends_with(".tmp")) {
                    continue;
                }
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .is_some_and(|age| age > STALE_TEMP_AGE);
                if old {
                    match std::fs::remove_file(&path) {
                        Ok(()) => *removed += 1,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err(&path)(e)),
                    }
                }
            }
            Ok(())
        }
        let mut removed = 0;
        walk(&self.files, SystemTime::now(), &mut removed)?;
        Ok(removed)
    }
}

/// [`Store::sweep_temps`] under the store lock for every account with a `files/` folder: at
/// server start and with the hourly staging sweep. An account whose store cannot be opened (a
/// damaged one, say) is reported on stderr and skipped. Returns how many files were removed.
pub fn sweep_all_temps(data_dir: &Path) -> Result<usize> {
    sweep_all_temps_with(data_dir, &[])
}

/// [`sweep_all_temps`] that takes an account's store from `open` if it is there instead of
/// opening it again (the server start passes the stores its recovery opened). Only an opened
/// store compacts `ids.jsonl`, so the hourly sweep keeps opening its own.
pub(crate) fn sweep_all_temps_with(data_dir: &Path, open: &[Store]) -> Result<usize> {
    fn sweep(store: &Store) -> Result<usize> {
        let _guard = store.lock()?;
        store.sweep_temps()
    }
    let accounts = crate::auth::Accounts::new(data_dir);
    let mut removed = 0;
    for account in accounts.list()? {
        if !accounts
            .account_dir(&account.slug)
            .join(super::FILES_DIR)
            .is_dir()
        {
            continue;
        }
        let swept = match open.iter().find(|s| s.slug == account.slug) {
            Some(store) => sweep(store),
            None => Store::open(data_dir, &account.slug).and_then(|store| sweep(&store)),
        };
        match swept {
            Ok(n) => removed += n,
            Err(e) => eprintln!(
                "notedown cloud: account {}: temp file sweep failed: {e}",
                account.slug
            ),
        }
    }
    Ok(removed)
}

/// What [`prune_all_recycle`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneAllReport {
    /// Lines removed over all accounts.
    pub removed: usize,
    /// One message per problem, starting with `account <slug>: `: an account whose prune
    /// failed, a slot that couldn't be removed, a line kept because of a symlink.
    pub problems: Vec<String>,
}

/// [`Store::prune_recycle`] for every account with a `files/` folder, with origin
/// [`Origin::Retention`]: the server's hourly prune. An account whose prune fails (a store that
/// can't be opened, an unreadable index) is reported in `problems` and skipped; the others
/// still run. A slot that can't be removed is reported the same way.
pub fn prune_all_recycle(
    data_dir: &Path,
    journal: &Journal,
    retention: &super::retention::Retention,
    now_ms: u64,
    offset: time::UtcOffset,
) -> Result<PruneAllReport> {
    let accounts = crate::auth::Accounts::new(data_dir);
    let mut all = PruneAllReport::default();
    for account in accounts.list()? {
        if !accounts
            .account_dir(&account.slug)
            .join(super::FILES_DIR)
            .is_dir()
        {
            continue;
        }
        let pruned = Store::open(data_dir, &account.slug).and_then(|store| {
            store.prune_recycle(journal, retention, now_ms, offset, Origin::Retention, false)
        });
        match pruned {
            Ok(report) => {
                all.removed += report.removed.len();
                for f in &report.failed {
                    all.problems.push(format!(
                        "account {}: recycle bin prune: recycle/{}: {}",
                        account.slug, f.location, f.message
                    ));
                }
            }
            Err(e) => all.problems.push(format!(
                "account {}: recycle bin prune failed: {e}",
                account.slug
            )),
        }
    }
    Ok(all)
}

/// Run [`Store::recover`] for every account that may need it: one with a `pending.json` or with
/// a `files/` folder (an account that never synced is not touched, so no `files/` appears).
/// When an account's store lock is busy (an import runs), it prints "waiting for the store lock
/// of account <slug>" on stderr and then waits. Returns the accounts whose recovery did
/// something or failed. A failure of one account (an
/// unreadable `pending.json`, say) does not stop the others; the caller reports it.
pub fn recover_all(data_dir: &Path, journal: &Journal) -> Result<Vec<(String, Result<Recovery>)>> {
    Ok(recover_all_keeping(data_dir, journal)?.0)
}

/// What [`recover_all_keeping`] found: the accounts [`recover_all`] reports, and every store
/// it opened.
pub(crate) type RecoveredStores = (Vec<(String, Result<Recovery>)>, Vec<Store>);

/// [`recover_all`] that also hands back the stores it opened (also those whose recovery
/// failed: [`Store::lock_recovered`] tries again), so the server start keeps them instead of
/// loading each registry again. They no longer announce a busy store lock. A store whose
/// recovery did something compacts its id log, as a fresh open would.
pub(crate) fn recover_all_keeping(data_dir: &Path, journal: &Journal) -> Result<RecoveredStores> {
    let accounts = crate::auth::Accounts::new(data_dir);
    let mut out = Vec::new();
    let mut stores = Vec::new();
    for account in accounts.list()? {
        let store_dir = accounts.account_dir(&account.slug);
        if !store_dir.join(PENDING_FILE).exists() && !store_dir.join(super::FILES_DIR).is_dir() {
            continue;
        }
        // A running import holds the store lock for its whole run: say why this waits.
        let r = Store::open_announced(data_dir, &account.slug).and_then(|mut store| {
            let r = store.lock().and_then(|guard| {
                let r = store.recover(&guard, journal)?;
                // The repair may have appended registry lines; the kept store folds them in
                // now instead of at the first hourly sweep.
                if r != Recovery::Clean
                    && let Err(e) = store.compact_ids(&guard)
                {
                    eprintln!(
                        "notedown cloud: account {}: compacting the id log failed: {e}",
                        account.slug
                    );
                }
                Ok(r)
            });
            store.announce_wait = false;
            stores.push(store);
            r
        });
        if !matches!(r, Ok(Recovery::Clean)) {
            out.push((account.slug, r));
        }
    }
    Ok((out, stores))
}
