//! The commit protocol: every change to `files/` is journaled exactly once, also across a crash.
//!
//! [`Store::commit`] runs four steps under the store lock:
//!
//! 1. Write the intent ([`Commit`]: the operation and its planned events) to
//!    `accounts/<slug>/pending.json` (atomic).
//! 2. Apply the data change durably (rename, fsync of the folders), then update the id registry.
//! 3. Append the events to the journal (one write, one fsync).
//! 4. Delete `pending.json`.
//!
//! [`Store::recover`] runs at server start, at the start of every CLI command and before every
//! commit. With a `pending.json` it checks whether the change is applied ([`PendingOp`] lists
//! the check per operation). Applied: it brings the registry up to date and appends the events
//! that the journal does not have yet (`origin: "repair"`), found by the `opId` in the last
//! [`REPAIR_SCAN_LINES`] lines. Not applied: it discards the intent. It also sweeps stale temp
//! files (`.nd-*.tmp`, older than one hour) from `files/`.
//!
//! Lock order: the store lock first, then `journal/.lock`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::{Kind, Store, StoreGuard, WirePath, is_native_folder, list};
use crate::journal::{Event, EventBody, FileKind, Journal, Origin, REPAIR_SCAN_LINES, new_op_id};
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
/// - `put`, `copy`: the target is a file with the intended `md5`.
/// - `move`: the source is gone and the target exists.
/// - `delete`: the source is gone and `recycle/<recycle>` exists.
/// - `createFolder`: the target is a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum PendingOp {
    /// Create or replace the file `path` by renaming the staged file onto it.
    #[serde(rename_all = "camelCase")]
    Put {
        id: u64,
        path: String,
        staged: String,
        md5: String,
        size: u64,
    },
    /// Copy the file `source` to the new file `path`.
    #[serde(rename_all = "camelCase")]
    Copy {
        id: u64,
        source_id: u64,
        source: String,
        path: String,
        md5: String,
        size: u64,
    },
    /// Rename a file or folder; the id stays.
    #[serde(rename_all = "camelCase")]
    Move {
        id: u64,
        kind: Kind,
        from: String,
        to: String,
    },
    /// Move a file or folder into `recycle/<recycle>`; the id is dropped.
    #[serde(rename_all = "camelCase")]
    Delete {
        id: u64,
        kind: Kind,
        path: String,
        recycle: String,
    },
    /// Create the folder `path`.
    #[serde(rename_all = "camelCase")]
    CreateFolder { id: u64, path: String },
}

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
    pub op: PendingOp,
    /// The events, in journal order.
    pub events: Vec<EventBody>,
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
            op,
            events,
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
                "finished the change {op_id} and journaled {} missing events",
                seqs.len()
            ),
        }
    }
}

/// Test hook: stop a commit at a step, as a crash there would. Not for production use.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After `pending.json` is written, before the data change.
    BeforeApply,
    /// After the data change, before the registry update and the journal append.
    BeforeAppend,
    /// After the journal append, before `pending.json` is deleted.
    BeforeClear,
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

impl Store {
    fn pending_path(&self) -> PathBuf {
        self.account_dir.join(PENDING_FILE)
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
        guard: &StoreGuard,
        path: &WirePath,
        staged: &Path,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_guard(guard)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("path", "the root is not a file".into()))?;
        self.require_folder(&parent, "parent folder")?;
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
        let size = meta.len();
        let prev = match self.resolve(path)? {
            Some(f) if f.kind == Kind::File => Some(self.md5_of(guard, path)?),
            Some(_) => return Err(invalid("path", format!("{path} is a folder"))),
            None => None,
        };
        let id = self.assign(guard, path, Kind::File)?;
        let display = path.display();
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
        let op = PendingOp::Put {
            id,
            path: display,
            staged: rel,
            md5,
            size,
        };
        Ok(Commit::new(origin, device, op, vec![event]))
    }

    /// Plan copying the file `source` to the new path `target` (new id): `file.copied`.
    pub fn plan_copy(
        &self,
        guard: &StoreGuard,
        source: &WirePath,
        target: &WirePath,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_guard(guard)?;
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
        self.require_folder(&parent, "parent folder")?;
        self.require_free(guard, target)?;
        let md5 = self.md5_of(guard, source)?;
        let size = std::fs::metadata(self.fs_path(source))
            .map_err(io_err(&self.fs_path(source)))?
            .len();
        let source_id = self.assign(guard, source, Kind::File)?;
        let id = self.assign(guard, target, Kind::File)?;
        let event = EventBody::FileCopied {
            id,
            source_id,
            path: target.display(),
        };
        let op = PendingOp::Copy {
            id,
            source_id,
            source: source.display(),
            path: target.display(),
            md5,
            size,
        };
        Ok(Commit::new(origin, device, op, vec![event]))
    }

    /// Plan renaming the file or folder `from` to the free path `to` (the id stays):
    /// `file.moved` or `folder.moved`. Native folders cannot be moved.
    pub fn plan_move(
        &self,
        guard: &StoreGuard,
        from: &WirePath,
        to: &WirePath,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_guard(guard)?;
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
        self.require_folder(&parent, "parent folder")?;
        self.require_free(guard, to)?;
        let id = self.assign(guard, from, found.kind)?;
        let (path, old_path) = (to.display(), from.display());
        let event = match found.kind {
            Kind::File => EventBody::FileMoved { id, path, old_path },
            Kind::Folder => EventBody::FolderMoved { id, path, old_path },
        };
        let op = PendingOp::Move {
            id,
            kind: found.kind,
            from: from.display(),
            to: to.display(),
        };
        Ok(Commit::new(origin, device, op, vec![event]))
    }

    /// Plan moving the file or folder `path` into `recycle/<deleted_at_ms>/<path>` (a later
    /// millisecond if that is taken): `file.deleted` or `folder.deleted`. Native folders cannot
    /// be deleted.
    pub fn plan_delete(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        deleted_at_ms: u64,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_guard(guard)?;
        let Some(found) = self.resolve(path)? else {
            return Err(invalid("path", format!("{path} is not in the store")));
        };
        if path.is_root() || is_native_folder(path) {
            return Err(invalid("path", format!("{path} is a native folder")));
        }
        let display = path.display();
        let mut ms = deleted_at_ms;
        let recycle = loop {
            let rel = format!("{ms}/{display}");
            let abs = self
                .account_dir
                .join(RECYCLE_DIR)
                .join(safe_relative(&rel, "recycle path")?);
            if std::fs::symlink_metadata(&abs).is_err() {
                break rel;
            }
            ms += 1;
        };
        let id = self.assign(guard, path, found.kind)?;
        let event = match found.kind {
            Kind::File => EventBody::FileDeleted {
                id,
                path: display.clone(),
                recycle: recycle.clone(),
            },
            Kind::Folder => EventBody::FolderDeleted {
                id,
                path: display.clone(),
            },
        };
        let op = PendingOp::Delete {
            id,
            kind: found.kind,
            path: display,
            recycle,
        };
        Ok(Commit::new(origin, device, op, vec![event]))
    }

    /// Plan creating the folder `path` (its parent must exist): `folder.created`.
    pub fn plan_create_folder(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        origin: Origin,
        device: Option<String>,
    ) -> Result<Commit> {
        self.check_guard(guard)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("path", "the root exists".into()))?;
        self.require_folder(&parent, "parent folder")?;
        self.require_free(guard, path)?;
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

    // ------------------------------------------------------------------ commit

    /// Run the commit protocol (see the module docs) and return the `seq` values of its events.
    /// An unfinished earlier commit (a `pending.json`) is recovered first.
    pub fn commit(
        &self,
        guard: &StoreGuard,
        commit: Commit,
        journal: &Journal,
    ) -> Result<Vec<u64>> {
        self.commit_until(guard, commit, journal, None)
    }

    /// [`Store::commit`] that stops at `crash`, as a crash there would. Test hook.
    #[doc(hidden)]
    pub fn commit_until(
        &self,
        guard: &StoreGuard,
        commit: Commit,
        journal: &Journal,
        crash: Option<CrashPoint>,
    ) -> Result<Vec<u64>> {
        self.check_guard(guard)?;
        if self.pending_path().exists() {
            self.recover_pending(guard, journal)?;
        }
        // 1. intent
        super::write_atomic_short(&self.pending_path(), &crate::fsutil::json_bytes(&commit))?;
        if crash == Some(CrashPoint::BeforeApply) {
            return Ok(Vec::new());
        }
        // 2. data change, then the registry
        if let Err(e) = self.apply(&commit.op) {
            match self.is_applied(guard, &commit.op) {
                Ok(true) => {}
                Ok(false) => {
                    self.clear_pending()?;
                    return Err(e);
                }
                // Unknown state: keep the intent for recovery.
                Err(_) => return Err(e),
            }
        }
        if crash == Some(CrashPoint::BeforeAppend) {
            return Ok(Vec::new());
        }
        self.finish_registry(guard, &commit.op)?;
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

    /// Perform the data change durably.
    fn apply(&self, op: &PendingOp) -> Result<()> {
        match op {
            PendingOp::Put { path, staged, .. } => {
                let target = self.fs_path(&wire(path)?);
                let staged = self.account_dir.join(safe_relative(staged, "staged file")?);
                std::fs::File::open(&staged)
                    .and_then(|f| f.sync_all())
                    .map_err(io_err(&staged))?;
                crate::fsutil::rename_durable(&staged, &target)
            }
            PendingOp::Copy { source, path, .. } => {
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
            PendingOp::Move { from, to, .. } => {
                let from = self.fs_path(&wire(from)?);
                let to = self.fs_path(&wire(to)?);
                if std::fs::symlink_metadata(&to).is_ok() {
                    return Err(invalid("move", format!("{} exists", to.display())));
                }
                crate::fsutil::rename_durable(&from, &to)
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
        }
    }

    /// `create_dir_all` below the account folder, with an fsync of each new folder's parent.
    fn create_dirs_durable(&self, dir: &Path) -> Result<()> {
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
            PendingOp::Put { path, md5, .. } | PendingOp::Copy { path, md5, .. } => {
                let path = wire(path)?;
                match self.resolve(&path)? {
                    Some(f) if f.kind == Kind::File => self.md5_of(guard, &path)? == *md5,
                    _ => false,
                }
            }
            PendingOp::Move { from, to, .. } => {
                self.resolve(&wire(from)?)?.is_none() && self.resolve(&wire(to)?)?.is_some()
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
        })
    }

    /// Bring the id registry in line with an applied change. Idempotent.
    fn finish_registry(&self, guard: &StoreGuard, op: &PendingOp) -> Result<()> {
        match op {
            // The id was assigned (durably) while planning, before the intent was written.
            PendingOp::Put { .. } | PendingOp::Copy { .. } | PendingOp::CreateFolder { .. } => {
                Ok(())
            }
            PendingOp::Move { id, to, .. } => {
                let to = wire(to)?;
                if self.path_of(*id)?.as_ref() == Some(&to) {
                    return Ok(());
                }
                if self.path_of(*id)?.is_none() {
                    // Unknown id (a registry reset); the new path gets an id at the next listing.
                    return Ok(());
                }
                if let Some(other) = self.id_of(&to)?
                    && other != *id
                {
                    // A stale row of something that was at `to` before (the path was free).
                    self.removed(guard, other)?;
                }
                self.moved(guard, *id, &to)
            }
            PendingOp::Delete { id, .. } => {
                if self.path_of(*id)?.is_some() {
                    self.removed(guard, *id)?;
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------ recovery

    /// Finish or discard an unfinished commit and sweep stale temp files (see the module docs).
    pub fn recover(&self, guard: &StoreGuard, journal: &Journal) -> Result<Recovery> {
        self.check_guard(guard)?;
        self.sweep_temps()?;
        self.recover_pending(guard, journal)
    }

    fn recover_pending(&self, guard: &StoreGuard, journal: &Journal) -> Result<Recovery> {
        let Some(commit) = self.pending()? else {
            return Ok(Recovery::Clean);
        };
        if !self.is_applied(guard, &commit.op)? {
            self.clear_pending()?;
            return Ok(Recovery::Discarded {
                op_id: commit.op_id,
            });
        }
        self.finish_registry(guard, &commit.op)?;
        let seqs = {
            let lock = journal.lock()?;
            let have = lock.count_op(&commit.op_id, REPAIR_SCAN_LINES)?;
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
    /// every listed entry (parents first), as one batch with one `opId` and `origin: "cli"`.
    /// Returns the `seq` values. Page events for notes are added by the page indexer.
    pub fn snapshot(&self, guard: &StoreGuard, journal: &Journal) -> Result<Vec<u64>> {
        self.check_guard(guard)?;
        let entries = self.list(guard, &WirePath::root(), true)?;
        let op_id = new_op_id();
        let mut events: Vec<Event> = entries
            .into_iter()
            .map(|e| {
                let body = match e.tag {
                    Kind::Folder => EventBody::FolderCreated {
                        id: e.id,
                        path: e.path_display,
                    },
                    Kind::File => EventBody::FileCreated {
                        id: e.id,
                        kind: FileKind::of(&e.name),
                        path: e.path_display,
                        md5: e.content_hash.unwrap_or_default(),
                        size: e.size,
                        pages_changed: None,
                    },
                };
                Event::new(&self.slug, Origin::Cli, None, &op_id, body)
            })
            .collect();
        journal.append(&mut events)
    }

    /// Remove `.nd-*.tmp` files older than [`STALE_TEMP_AGE`] anywhere below `files/`. Symlinks
    /// are not followed. Returns how many were removed.
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

/// Run [`Store::recover`] for every account that may need it: one with a `pending.json` or with
/// a `files/` folder (an account that never synced is not touched, so no `files/` appears).
/// Returns the accounts whose recovery did something.
pub fn recover_all(data_dir: &Path, journal: &Journal) -> Result<Vec<(String, Recovery)>> {
    let mut out = Vec::new();
    for account in crate::auth::Accounts::new(data_dir).list()? {
        let store_dir = crate::auth::Accounts::new(data_dir).account_dir(&account.slug);
        if !store_dir.join(PENDING_FILE).exists() && !store_dir.join(super::FILES_DIR).is_dir() {
            continue;
        }
        let store = Store::open(data_dir, &account.slug)?;
        let guard = store.lock()?;
        let r = store.recover(&guard, journal)?;
        if r != Recovery::Clean {
            out.push((account.slug, r));
        }
    }
    Ok(out)
}
