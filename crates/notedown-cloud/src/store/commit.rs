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
//! journal after the `journalSeq` stored in the intent. Not applied: it discards the intent. It
//! also sweeps stale temp files (`.nd-*.tmp`, older than one hour) from `files/`.
//!
//! Lock order: the store lock first, then `journal/.lock`.

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
///
/// A `move` or `delete` that empties folders lists them in `prune` (deepest first). They are
/// removed after the apply step (also by recovery); a folder that is not empty then stays, and
/// its `folder.deleted` event is dropped before the journal append.
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
    /// Afterwards the emptied source folders in `prune` are removed.
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
    },
    /// Move a file or folder into `recycle/<recycle>`; the id is dropped. `entry` is its line
    /// for `recycle/index.jsonl` (written after the apply step, also by recovery). Afterwards
    /// the emptied folders in `prune` are removed.
    #[serde(rename_all = "camelCase")]
    Delete {
        id: u64,
        kind: Kind,
        path: String,
        recycle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<RecycleEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prune: Vec<PlannedFolder>,
    },
    /// Create the folder `path`.
    #[serde(rename_all = "camelCase")]
    CreateFolder { id: u64, path: String },
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
}

/// A file directly inside `<account>/pages/` that a commit writes (atomically) or removes after
/// its data change and before its journal append; any other path is refused. A failure is reported on stderr and does not fail the
/// commit.
#[derive(Clone, PartialEq, Eq)]
pub struct SideWrite {
    pub path: PathBuf,
    /// The new content; `None` removes the file.
    pub bytes: Option<Vec<u8>>,
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

/// Test hook: stop a commit at a step, as a crash there would. Not for production use.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After `pending.json` is written, before the data change.
    BeforeApply,
    /// A `put`: after its new folders and its recycle copy, before the rename. A move through a
    /// temp name: between its two renames.
    BeforeRename,
    /// After the data change, before the registry update and the journal append.
    BeforeAppend,
    /// After the journal append, before `pending.json` is deleted.
    BeforeClear,
}

/// Whether a [`CrashPoint::BeforeRename`] can stop `op`: a put, and a move through a temp name.
fn has_rename_stop(op: &PendingOp) -> bool {
    matches!(
        op,
        PendingOp::Put { .. } | PendingOp::Move { via: Some(_), .. }
    )
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

    fn check_write(&self, guard: &WriteGuard) -> Result<()> {
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
        let mut events = Vec::new();
        let mut folders = Vec::new();
        for folder in &missing {
            let folder_id = self.assign(guard, folder, Kind::Folder)?;
            events.push(EventBody::FolderCreated {
                id: folder_id,
                path: folder.display(),
            });
            folders.push(PlannedFolder {
                id: folder_id,
                path: folder.display(),
            });
        }
        let recycle = match (&prev, options.recycle_at_ms) {
            (Some(prev_md5), Some(at)) => Some(RecycleEntry {
                id,
                path: display.clone(),
                md5: prev_md5.clone(),
                deleted_at: self.recycle_slot(guard, path, at)?,
                reason: RecycleReason::Overwritten,
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
        Ok(Commit::new(origin, device, op, events))
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
    /// `gone` if `gone` is its only entry (dot-files count), then its parent if that folder is
    /// its only entry, and so on. It stops at the root, at a native folder or one of their
    /// parents ([`is_native_folder`]), and at a folder that `keep` (a move target) lies in.
    fn plan_prune(
        &self,
        guard: &StoreGuard,
        gone: &WirePath,
        keep: Option<&WirePath>,
    ) -> Result<Vec<PlannedFolder>> {
        let mut out = Vec::new();
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
            let dir = self.fs_path(&parent);
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&dir).map_err(io_err(&dir))? {
                names.push(entry.map_err(io_err(&dir))?.file_name());
                if names.len() > 1 {
                    break;
                }
            }
            let only = matches!(names.as_slice(), [n] if Some(n.as_os_str()) == cur.name().map(std::ffi::OsStr::new));
            if !only {
                break;
            }
            let id = self.assign(guard, &parent, Kind::Folder)?;
            out.push(PlannedFolder {
                id,
                path: parent.display(),
            });
            cur = parent;
        }
        Ok(out)
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
    /// cannot be moved.
    pub fn plan_move(
        &self,
        guard: &WriteGuard,
        from: &WirePath,
        to: &WirePath,
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
        let prune = self.plan_prune(guard, from, Some(to))?;
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
        };
        Ok(Commit::new(origin, device, op, events))
    }

    /// Plan moving the file or folder `path` into `recycle/<ms>/<path>`, where `ms` is
    /// `deleted_at_ms` or the next free millisecond ([`Store::recycle_slot`]), with its index
    /// line (`reason: "deleted"`). Events: for a folder first `file.deleted` / `folder.deleted`
    /// for everything inside it (children before their folder), then `file.deleted` or
    /// `folder.deleted` for `path`, then `folder.deleted` for each folder the delete leaves empty
    /// (deepest first, never a native folder; they are removed for good, without a recycle
    /// entry). Native folders cannot be deleted. Plan and commit one delete at a time: the slot
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
        if found.kind == Kind::Folder {
            // The listing comes parents first; reversed, every child precedes its folder.
            for e in self.list(guard, path, true)?.into_iter().rev() {
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
        let prune = self.plan_prune(guard, path, None)?;
        events.extend(prune_events(&prune));
        let op = PendingOp::Delete {
            id,
            kind: found.kind,
            path: display,
            recycle: entry.location(),
            entry: Some(entry),
            prune,
        };
        Ok(Commit::new(origin, device, op, events))
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
        if let Err(e) = self.apply(&commit.op, stop) {
            match self.is_applied(guard, &commit.op) {
                Ok(true) => {}
                Ok(false) => {
                    self.undo_unapplied(&commit.op);
                    self.clear_pending()?;
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
        self.finish_registry(guard, &commit.op)?;
        self.finish_recycle(&commit.op)?;
        for w in &commit.side_writes {
            if let Err(e) = self.write_side(w) {
                eprintln!("warning: account {}: {e}", self.slug);
            }
        }
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
    /// followed by exactly `pages` and one plain name (no `..`, `.`, root or prefix part).
    fn write_side(&self, w: &SideWrite) -> Result<()> {
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
                Ok(()) => crate::fsutil::fsync_dir(&parent),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(&w.path)(e)),
            };
        };
        self.create_dirs_durable(&parent)?;
        super::write_atomic_short(&w.path, bytes)
    }

    /// Perform the data change durably. `stop_before_rename` (test hook) ends a `put` after its
    /// folders and recycle copy, as a crash there would.
    fn apply(&self, op: &PendingOp, stop_before_rename: bool) -> Result<()> {
        match op {
            PendingOp::Put {
                path,
                staged,
                folders,
                recycle,
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
                crate::fsutil::rename_durable(&staged, &target)
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
            PendingOp::Unchanged { .. } => Ok(()),
        }
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
            PendingOp::Put { .. }
            | PendingOp::Copy { .. }
            | PendingOp::CreateFolder { .. }
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

    /// Remove the folders a `move` or `delete` left empty (`prune`, deepest first), each with an
    /// fsync of its parent. A folder that cannot be removed (it is not empty any more) stays,
    /// and so do the folders above it: they are taken out of `prune`, and their `folder.deleted`
    /// events out of `events`. Returns whether anything was taken out. Idempotent: a folder
    /// that is gone already counts as removed.
    fn settle_prune(&self, op: &mut PendingOp, events: &mut Vec<EventBody>) -> Result<bool> {
        let (PendingOp::Move { prune, .. } | PendingOp::Delete { prune, .. }) = op else {
            return Ok(false);
        };
        let mut stays = None;
        for (i, p) in prune.iter().enumerate() {
            let Ok(path) = wire(&p.path) else {
                stays = Some(i);
                break;
            };
            let dir = self.fs_path(&path);
            match std::fs::remove_dir(&dir) {
                Ok(()) => crate::fsutil::fsync_dir(dir.parent().unwrap_or(&self.files))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    stays = Some(i);
                    break;
                }
            }
        }
        let Some(i) = stays else {
            return Ok(false);
        };
        let kept = prune.split_off(i);
        events.retain(|e| {
            !matches!(e, EventBody::FolderDeleted { id, path }
                if kept.iter().any(|k| k.id == *id && k.path == *path))
        });
        Ok(true)
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

    /// The second step of a rename through a temp name: from the temp name to `to`, if the
    /// temp name is there and `to` is not listed yet.
    fn finish_via(&self, to: &WirePath, via: &Path) -> Result<()> {
        if std::fs::symlink_metadata(via).is_err() || self.resolve(to)?.is_some() {
            return Ok(());
        }
        crate::fsutil::rename_durable(via, &self.fs_path(to))
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
            if self.resolve(&from)?.is_none() {
                self.finish_via(&to, &via)?;
            }
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

    /// Write the recycle index line of an applied change (idempotent, see
    /// [`Store::record_recycled`]).
    fn finish_recycle(&self, op: &PendingOp) -> Result<()> {
        if let PendingOp::Put {
            recycle: Some(entry),
            ..
        }
        | PendingOp::Delete {
            entry: Some(entry), ..
        } = op
        {
            self.append_recycled(entry)?;
        }
        Ok(())
    }

    /// Clean up after a change that was not applied: put a file or folder that a rename through
    /// a temp name left under that name back to its old name, remove the recycle copy the change
    /// made and the folders it created while they are empty (deepest first). Their registry rows
    /// stay, as for any discarded intent; a retry reuses them. Failures are ignored (the
    /// leftovers are harmless: an extra copy, an empty folder).
    fn undo_unapplied(&self, op: &PendingOp) {
        match op {
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
                if let (Some(via), Ok(from)) = (via, wire(from))
                    && let Ok(via) = self.via_path(&from, via)
                    && std::fs::symlink_metadata(&via).is_ok()
                    && matches!(self.resolve(&from), Ok(None))
                {
                    let _ = crate::fsutil::rename_durable(&via, &self.fs_path(&from));
                }
                self.remove_planned(folders);
            }
            _ => {}
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
        let Some(mut commit) = self.pending()? else {
            return Ok(Recovery::Clean);
        };
        self.resume(&commit.op)?;
        if !self.is_applied(guard, &commit.op)? {
            self.undo_unapplied(&commit.op);
            self.clear_pending()?;
            return Ok(Recovery::Discarded {
                op_id: commit.op_id,
            });
        }
        self.settle_prune(&mut commit.op, &mut commit.events)?;
        self.finish_registry(guard, &commit.op)?;
        self.finish_recycle(&commit.op)?;
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
    /// every listed entry (parents first), as one batch with one `opId` and `origin: "cli"`.
    /// A `.note` gets `pagesChanged` and one `page.added` per page right after its
    /// `file.created` (or `page.index_failed`), and its page record is written now, under the
    /// same guard and before the append, so the records agree with the journal. Returns the
    /// `seq` values.
    pub fn snapshot(&self, guard: &WriteGuard, journal: &Journal) -> Result<Vec<u64>> {
        self.check_write(guard)?;
        let entries = self.list(guard, &WirePath::root(), true)?;
        let op_id = new_op_id();
        let mut events: Vec<Event> = Vec::new();
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
                        && let Err(err) = self.write_side(&w)
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
                    .map(|body| Event::new(&self.slug, Origin::Cli, None, &op_id, body)),
            );
        }
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
/// Returns the accounts whose recovery did something or failed. A failure of one account (an
/// unreadable `pending.json`, say) does not stop the others; the caller reports it.
pub fn recover_all(data_dir: &Path, journal: &Journal) -> Result<Vec<(String, Result<Recovery>)>> {
    let accounts = crate::auth::Accounts::new(data_dir);
    let mut out = Vec::new();
    for account in accounts.list()? {
        let store_dir = accounts.account_dir(&account.slug);
        if !store_dir.join(PENDING_FILE).exists() && !store_dir.join(super::FILES_DIR).is_dir() {
            continue;
        }
        let r = Store::open(data_dir, &account.slug).and_then(|store| {
            let guard = store.lock()?;
            store.recover(&guard, journal)
        });
        if !matches!(r, Ok(Recovery::Clean)) {
            out.push((account.slug, r));
        }
    }
    Ok(out)
}
