//! The scan: journal the changes a person made by hand in `files/` (`docs/sync-protocol.md`
//! 8.3).
//!
//! The baseline is the id registry (which paths have ids) and `hashes.json` (the last known MD5
//! per path). The scan walks its scope like a listing does (dot-names, names the wire can't
//! carry, symlinks and special files are skipped) and compares:
//!
//! - **new**: a file or folder on disk without a registry row of its kind at its path;
//! - **changed**: a file whose fresh MD5 differs from the cached one (a file without a cached
//!   MD5 is hashed and cached without an event);
//! - **gone**: a registry row whose path no longer holds an entry of its kind;
//! - **moved**: a gone file and a new file with the same size and MD5 (for the gone file: its
//!   row in `hashes.json`), when that pair of values is unique among the gone files and among
//!   the new files. The file keeps its id. Folders are never paired; a folder renamed on disk
//!   is deleted and created, and its files move one by one.
//!
//! The scope is a folder (or a file) and what lies below it: every descendant, or only the
//! direct children when the scan is not recursive. A gone folder in the scope takes its whole
//! subtree along. Parents of the scope that have no id yet are created too.
//!
//! The scan never renames, moves or deletes anything in `files/`. Under the store lock it hashes
//! into a copy of the cache (for a scoped scan only the rows of the files it found), plans
//! against the registry, reserves the new ids in the registry log (the next id only, no rows),
//! appends the events to the journal (one batch, one `opId`, `origin: "scan"`), then the
//! registry ops in one log append and the cache, then the page records. A crash after the
//! journal append journals the same changes again at the next scan (with new ids; the reserved
//! ones stay unused); the registry is never ahead of the journal, and no id the journal used is
//! handed out again.
//!
//! A plan that removes files holds instead of applying when fewer than `min_files` files would
//! be left in the whole account (an empty data folder, an unmounted volume). It writes
//! [`SCAN_HOLD_FILE`] and keeps it while the condition stays. A later whole-tree scan that no
//! longer matches, or one with `accept`, removes it and applies its plan. While it exists, a
//! scoped scan applies nothing and reports the hold: it can't see whether the files outside its
//! scope came back.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::commit::{SideWrite, WriteGuard};
use super::hashes::HashCache;
use super::ids::{Op, Registry};
use super::{Kind, Store, WirePath, is_native_folder, list};
use crate::journal::{Event, EventBody, FileKind, Journal, Origin, new_op_id};
use crate::{Error, Result, io_err};

/// `accounts/<slug>/scan-hold.json`: the marker of a scan that held instead of applying.
pub const SCAN_HOLD_FILE: &str = "scan-hold.json";

/// How many paths [`ScanHold::examples`] lists at most.
const HOLD_EXAMPLES: usize = 10;

/// From this many files to hash, a scan says on stderr that it can take a while (the first scan
/// after an upgrade hashes every file once).
const LONG_HASH: usize = 100;

/// What [`Store::scan`] looks at and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// The folder (or file) to scan; the root for the whole tree.
    pub scope: WirePath,
    /// Every descendant of the scope, or only its direct children.
    pub recursive: bool,
    /// Plan and report only: nothing is written, not even the hold marker.
    pub dry_run: bool,
    /// Apply the plan even when it would hold, and remove the hold marker.
    pub accept: bool,
    /// Hold when a plan that removes files leaves fewer files than this in the account; 0 never
    /// holds.
    pub min_files: u64,
}

impl ScanOptions {
    /// The whole tree, recursive, applied.
    pub fn all(min_files: u64) -> ScanOptions {
        ScanOptions {
            scope: WirePath::root(),
            recursive: true,
            dry_run: false,
            accept: false,
            min_files,
        }
    }
}

/// What [`Store::scan`] found. The lists describe the plan in journal order, also for a dry
/// run and when the scan held (then nothing of it was applied and `seqs` is empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// New folders (parents first), then new files.
    pub created: Vec<WirePath>,
    pub modified: Vec<WirePath>,
    /// Old path, new path.
    pub moved: Vec<(WirePath, WirePath)>,
    /// Gone files, then gone folders (children first).
    pub deleted: Vec<WirePath>,
    /// The hold, when the plan was not applied: too few files would be left, or a hold exists
    /// and the scan is scoped.
    pub held: Option<ScanHold>,
    /// An earlier hold was removed (for a dry run: would be removed). Only a whole-tree scan or
    /// one with `accept` removes it.
    pub released: bool,
    /// The `seq` values of the journaled events.
    pub seqs: Vec<u64>,
}

/// The content of [`SCAN_HOLD_FILE`] (field order is the file order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanHold {
    /// When the hold was written, in ms since the epoch.
    pub at: u64,
    /// The files the plan removes.
    pub gone: usize,
    /// The files that would be left in the account.
    pub left: usize,
    /// The minimum in force when the hold was written.
    pub min: u64,
    /// Up to ten of the removed paths.
    pub examples: Vec<String>,
}

/// A file or folder of the scope on disk. `md5` and `prev` are for files: the fresh MD5 and the
/// cached one before this scan. `unstable` is a file that changed while it was hashed: it is
/// there, but its MD5 is unknown until the next scan.
struct OnDisk {
    path: WirePath,
    kind: Kind,
    size: u64,
    md5: String,
    prev: Option<String>,
    unstable: bool,
}

/// A new entry with the id it gets.
struct New {
    id: u64,
    entry: OnDisk,
}

/// A registry row that is gone.
struct Gone {
    id: u64,
    kind: Kind,
    path: WirePath,
}

/// The plan of one scan.
#[derive(Default)]
struct Plan {
    new_folders: Vec<New>,
    new_files: Vec<New>,
    /// Id, the entry on disk.
    modified: Vec<(u64, OnDisk)>,
    /// Id, old path, the entry on disk.
    moved: Vec<(u64, WirePath, OnDisk)>,
    gone_files: Vec<Gone>,
    gone_folders: Vec<Gone>,
    /// Files that would be left in the account.
    left: usize,
    /// The files of the scope on disk (display paths).
    files_on_disk: HashSet<String>,
}

impl Plan {
    fn report(&self) -> ScanReport {
        ScanReport {
            created: self
                .new_folders
                .iter()
                .chain(&self.new_files)
                .map(|n| n.entry.path.clone())
                .collect(),
            modified: self.modified.iter().map(|(_, d)| d.path.clone()).collect(),
            moved: self
                .moved
                .iter()
                .map(|(_, from, d)| (from.clone(), d.path.clone()))
                .collect(),
            deleted: self
                .gone_files
                .iter()
                .chain(&self.gone_folders)
                .map(|g| g.path.clone())
                .collect(),
            ..ScanReport::default()
        }
    }

    /// The registry ops, in an order that the log replays as planned: files that leave, moves
    /// (before a gone folder drops its subtree), gone folders (children first), new entries
    /// (parents first).
    fn ops(&self) -> Vec<Op> {
        let mut ops: Vec<Op> = self
            .gone_files
            .iter()
            .map(|g| Op::Remove { id: g.id })
            .collect();
        ops.extend(self.moved.iter().map(|(id, _, d)| Op::Move {
            id: *id,
            path: d.path.display(),
        }));
        ops.extend(self.gone_folders.iter().map(|g| Op::Remove { id: g.id }));
        ops.extend(
            self.new_folders
                .iter()
                .chain(&self.new_files)
                .map(|n| Op::Assign {
                    id: n.id,
                    kind: n.entry.kind,
                    path: n.entry.path.display(),
                }),
        );
        ops
    }

    /// The hold this plan would write: it removes files and leaves fewer than `min`.
    fn hold(&self, min: u64, now_ms: u64) -> Option<ScanHold> {
        if self.gone_files.is_empty() || self.left as u64 >= min {
            return None;
        }
        let mut examples: Vec<String> = self.gone_files.iter().map(|g| g.path.display()).collect();
        examples.sort();
        examples.truncate(HOLD_EXAMPLES);
        Some(ScanHold {
            at: now_ms,
            gone: self.gone_files.len(),
            left: self.left,
            min,
            examples,
        })
    }
}

/// Whether `path` lies in the scope: the scope itself (never the root) and its descendants, or
/// only its direct children when not `recursive`.
fn in_scope(scope: &WirePath, recursive: bool, path: &WirePath) -> bool {
    if path.is_root() || !path.starts_with(scope) {
        return false;
    }
    recursive || path.segments().len() <= scope.segments().len() + 1
}

impl Store {
    /// Journal the changes made by hand in the scope (see the module docs) and return what it
    /// found. Under `guard`; nothing on disk below `files/` is changed. A tree that breaks
    /// while it is read (`files/` or a native folder of an imported account gone or swapped) is
    /// `Error::StoreDamaged`, so a broken tree never journals deletes. A file that vanishes
    /// while it is hashed counts as gone. A file that changes while it is hashed counts as
    /// there, but gets no id and no event this time: the next scan journals it.
    pub fn scan(
        &self,
        guard: &WriteGuard,
        journal: &Journal,
        opts: &ScanOptions,
        now_ms: u64,
    ) -> Result<ScanReport> {
        self.scan_until(guard, journal, opts, now_ms, false)
    }

    /// [`Store::scan`] that stops after the journal append, before the registry append, as a
    /// crash there would. Test hook.
    #[doc(hidden)]
    pub fn scan_until(
        &self,
        guard: &WriteGuard,
        journal: &Journal,
        opts: &ScanOptions,
        now_ms: u64,
        stop_after_journal: bool,
    ) -> Result<ScanReport> {
        self.check_write(guard)?;
        let (disk, parents, cache) = self.scan_disk(opts)?;
        // The cache rows of gone paths come from the store's cache: the walk hashed only paths
        // on disk, so the copy holds no rows for them.
        let plan = {
            let state = self.state();
            plan(&state.ids, &state.hashes, opts, disk, parents)
        };
        let mut report = plan.report();
        // An unreadable marker counts as one: it is replaced or removed.
        let marker = self.scan_hold();
        let has_marker = !matches!(marker, Ok(None));
        let whole = opts.scope.is_root() && opts.recursive;
        if !opts.accept {
            let held = if let Some(fresh) = plan.hold(opts.min_files, now_ms) {
                // An existing marker stays as it is while the condition holds.
                Some(match marker {
                    Ok(Some(old)) => old,
                    _ => {
                        if !opts.dry_run {
                            self.write_hold(&fresh)?;
                        }
                        fresh
                    }
                })
            } else if has_marker && !whole {
                // Only a whole-tree scan knows whether the files came back. A scoped one counts
                // the files outside its scope as left, so it applies nothing while held.
                Some(match marker {
                    Ok(Some(old)) => old,
                    _ => ScanHold {
                        at: now_ms,
                        gone: 0,
                        left: plan.left,
                        min: opts.min_files,
                        examples: Vec::new(),
                    },
                })
            } else {
                None
            };
            if held.is_some() {
                report.held = held;
                return Ok(report);
            }
        }
        report.released = has_marker;
        if opts.dry_run {
            return Ok(report);
        }
        let (mut events, side_writes) = self.scan_events(&plan);
        // The new ids are reserved first: after a crash between the journal append and the
        // registry append, no other write gets an id the journal already used.
        if let Some(last) = plan
            .new_folders
            .iter()
            .chain(&plan.new_files)
            .map(|n| n.id)
            .max()
        {
            let mut state = self.state();
            self.append(&mut state, &[Op::Reserve { next: last + 1 }])?;
        }
        // Events next: the registry is never ahead of the journal.
        report.seqs = journal.append(&mut events)?;
        if stop_after_journal {
            return Ok(report);
        }
        self.finish_scan(&plan, cache, opts)?;
        if report.released {
            self.remove_hold()?;
        }
        for w in &side_writes {
            if let Err(e) = self.write_side(w) {
                eprintln!("warning: account {}: {e}", self.slug);
            }
        }
        Ok(report)
    }

    /// Walk the scope and hash its files through a copy of the cache: the whole cache for a
    /// whole-tree scan, else only the rows of the files found. Returns the entries of the scope
    /// (files with a fresh MD5 and the cached one), the folders above it and the copy. The store's
    /// cache is not touched: a scan that holds or stops early leaves it as it was. The tree is
    /// checked before and after, like [`Store::list`] does.
    fn scan_disk(&self, opts: &ScanOptions) -> Result<(Vec<OnDisk>, Vec<WirePath>, HashCache)> {
        let before = self.check_tree(false)?;
        let mut found = Vec::new();
        let mut parents = Vec::new();
        if let Some(top) = self.resolve_scope(&opts.scope, &mut parents)? {
            if top.kind == Kind::Folder {
                list::walk(
                    &top,
                    opts.recursive,
                    &mut found,
                    &self.vanished_damage(before.imported),
                )?;
            }
            if !top.path.is_root() {
                found.push(top);
            }
        }
        let mut cache = {
            let state = self.state();
            if opts.scope.is_root() && opts.recursive {
                state.hashes.clone()
            } else {
                state.hashes.subset(
                    found
                        .iter()
                        .filter(|f| f.kind == Kind::File)
                        .map(|f| &f.path),
                )
            }
        };
        let to_hash = found
            .iter()
            .filter(|f| f.kind == Kind::File && !cache.is_current(&f.path, &f.meta))
            .count();
        if to_hash > LONG_HASH {
            eprintln!(
                "notedown cloud: account {}: checking {to_hash} files, this can take a while",
                self.slug
            );
        }
        let hook = self.hash_hook();
        let mut disk = Vec::with_capacity(found.len());
        for f in found {
            let (md5, prev, unstable) = match f.kind {
                Kind::Folder => (String::new(), None, false),
                Kind::File => {
                    let prev = cache.cached(&f.path).map(str::to_string);
                    if let Some(hook) = &hook {
                        hook(&f.path);
                    }
                    match cache.md5_checked(&f.path, &f.fs_path, &f.meta) {
                        Ok(Some(md5)) => (md5, prev, false),
                        // Changed while it was hashed: the MD5 may be of no version at all.
                        Ok(None) => (String::new(), prev, true),
                        // Removed while the scan ran: it is gone.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(io_err(&f.fs_path)(e)),
                    }
                }
            };
            disk.push(OnDisk {
                size: if f.kind == Kind::File {
                    f.meta.len()
                } else {
                    0
                },
                path: f.path,
                kind: f.kind,
                md5,
                prev,
                unstable,
            });
        }
        let after = self.check_tree(false)?;
        self.check_same_tree(&before, &after)?;
        Ok((disk, parents, cache))
    }

    /// The scope on disk, and in `parents` the folders above it. `None` when the scope or one
    /// of its parents is not a real file or folder.
    fn resolve_scope(
        &self,
        scope: &WirePath,
        parents: &mut Vec<WirePath>,
    ) -> Result<Option<list::Found>> {
        let mut path = WirePath::root();
        for segment in scope.segments() {
            match list::resolve(&self.files, &path)? {
                Some(f) if f.kind == Kind::Folder => {}
                _ => return Ok(None),
            }
            if !path.is_root() {
                parents.push(path.clone());
            }
            path = path.join(segment)?;
        }
        list::resolve(&self.files, scope)
    }

    /// The events of `plan` in journal order (see [`ScanReport`]), with the page events of
    /// every new or changed note right after its file event, and the page records to write
    /// after the registry.
    fn scan_events(&self, plan: &Plan) -> (Vec<Event>, Vec<SideWrite>) {
        let mut bodies = Vec::new();
        let mut writes = Vec::new();
        // The page records, read once, when a changed or moved note needs them.
        let is_note = |p: &WirePath| FileKind::of(p.name().unwrap_or_default()) == FileKind::Note;
        let by_path = if plan.modified.iter().any(|(_, d)| is_note(&d.path))
            || plan.moved.iter().any(|(_, from, _)| is_note(from))
        {
            crate::pages::records_by_path(self)
        } else {
            Default::default()
        };
        for n in &plan.new_folders {
            bodies.push(EventBody::FolderCreated {
                id: n.id,
                path: n.entry.path.display(),
            });
        }
        for n in &plan.new_files {
            let (kind, pages_changed, pages) =
                self.scan_pages(n.id, &n.entry, None, &by_path, &mut writes);
            bodies.push(EventBody::FileCreated {
                id: n.id,
                path: n.entry.path.display(),
                kind,
                md5: n.entry.md5.clone(),
                size: n.entry.size,
                pages_changed,
            });
            bodies.extend(pages);
        }
        for (id, d) in &plan.modified {
            let prev = d.prev.clone().unwrap_or_default();
            let (kind, pages_changed, pages) =
                self.scan_pages(*id, d, Some(&prev), &by_path, &mut writes);
            bodies.push(EventBody::FileModified {
                id: *id,
                path: d.path.display(),
                kind,
                md5: d.md5.clone(),
                prev_md5: prev,
                size: d.size,
                pages_changed,
            });
            bodies.extend(pages);
        }
        let mut moved_notes = Vec::new();
        for (id, from, d) in &plan.moved {
            if is_note(from) {
                moved_notes.push((from.clone(), d.path.clone()));
            }
            bodies.push(EventBody::FileMoved {
                id: *id,
                path: d.path.display(),
                old_path: from.display(),
            });
        }
        writes.extend(crate::pages::move_records(self, &by_path, &moved_notes));
        for g in &plan.gone_files {
            bodies.push(EventBody::FileDeleted {
                id: g.id,
                path: g.path.display(),
                recycle: String::new(),
            });
        }
        for g in &plan.gone_folders {
            bodies.push(EventBody::FolderDeleted {
                id: g.id,
                path: g.path.display(),
            });
        }
        let op_id = new_op_id();
        let events = bodies
            .into_iter()
            .map(|b| Event::new(&self.slug, Origin::Scan, None, &op_id, b))
            .collect();
        (events, writes)
    }

    /// The kind of the file `d` (store id `id`) and, for a note, `pagesChanged` and its page
    /// events; its record changes go to `writes`. `prev` is the MD5 before a change.
    fn scan_pages(
        &self,
        id: u64,
        d: &OnDisk,
        prev: Option<&str>,
        by_path: &crate::pages::RecordsByPath,
        writes: &mut Vec<SideWrite>,
    ) -> (FileKind, Option<u64>, Vec<EventBody>) {
        let kind = FileKind::of(d.path.name().unwrap_or_default());
        if kind != FileKind::Note {
            return (kind, None, Vec::new());
        }
        let (changed, events, w) =
            crate::pages::scan_note(self, &d.path, id, &d.md5, d.size, prev, by_path);
        writes.extend(w);
        (kind, changed, events)
    }

    /// After the journal append: the registry ops in one log append, then the cache. After a
    /// whole-tree scan its copy replaces the cache, with only the rows of the files on disk
    /// kept. After a scoped scan the rows it hashed go into the cache and the rows of gone and
    /// moved paths are dropped.
    fn finish_scan(&self, plan: &Plan, mut cache: HashCache, opts: &ScanOptions) -> Result<()> {
        let mut state = self.state();
        self.append(&mut state, &plan.ops())?;
        if opts.scope.is_root() && opts.recursive {
            cache.retain(&plan.files_on_disk);
            state.hashes = cache;
        } else {
            let hashes = &mut state.hashes;
            hashes.absorb(cache);
            // By exact key for files (a moved file's old path is a file too), and one pass for
            // every gone folder: a large delete by hand stays linear in the cache.
            for g in &plan.gone_files {
                hashes.removed_file(&g.path);
            }
            for (_, from, _) in &plan.moved {
                hashes.removed_file(from);
            }
            let folders: Vec<WirePath> = plan.gone_folders.iter().map(|g| g.path.clone()).collect();
            hashes.removed_all(&folders);
        }
        self.flush_hashes(&mut state)
    }

    /// The scopes a write to `target` scans first, each without recursion: the root, then each
    /// folder above `target` that is a real folder on disk, from the top down. A write then
    /// plans against a registry that knows every change made by hand on its way.
    pub fn way_scopes(&self, target: &WirePath) -> Result<Vec<WirePath>> {
        let mut cur = WirePath::root();
        let mut scopes = vec![cur.clone()];
        let Some(parent) = target.parent() else {
            return Ok(scopes);
        };
        for segment in parent.segments() {
            cur = cur.join(segment)?;
            let is_dir = std::fs::symlink_metadata(self.fs_path(&cur)).is_ok_and(|m| m.is_dir());
            if !is_dir {
                break;
            }
            scopes.push(cur.clone());
        }
        Ok(scopes)
    }

    /// Run `hook` with the wire path of each file a scan of this store is about to hash, as a
    /// person changing or removing it at that moment would. Test hook.
    #[doc(hidden)]
    pub fn set_scan_hash_hook(&self, hook: impl Fn(&WirePath) + Send + Sync + 'static) {
        *self.hash_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(hook));
    }

    fn hash_hook(&self) -> Option<super::HashHook> {
        self.hash_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn hold_path(&self) -> std::path::PathBuf {
        self.account_dir.join(SCAN_HOLD_FILE)
    }

    /// The marker of a scan that held instead of applying (`accounts/<slug>/scan-hold.json`),
    /// if there is one. While it exists, device sync of the account waits. An unreadable marker
    /// is an error.
    pub fn scan_hold(&self) -> Result<Option<ScanHold>> {
        let path = self.hold_path();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Error::Config {
                    path,
                    message: e.to_string(),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    fn write_hold(&self, hold: &ScanHold) -> Result<()> {
        super::write_atomic_short(&self.hold_path(), &crate::fsutil::json_bytes(hold))
    }

    fn remove_hold(&self) -> Result<()> {
        let path = self.hold_path();
        match std::fs::remove_file(&path) {
            Ok(()) => crate::fsutil::fsync_dir(&self.account_dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&path)(e)),
        }
    }
}

/// The registry rows that are gone (see [`plan`]) and the files that would be left outside the
/// scope. Reads only the rows of the scope, of the subtrees of gone folders and of `parents`,
/// never every row: the count of all files comes from the registry.
fn gone_rows(
    reg: &Registry,
    opts: &ScanOptions,
    on_disk: &HashMap<&WirePath, Kind>,
    parents: &[WirePath],
) -> (Vec<Gone>, usize) {
    // Rows of the scope whose path holds no entry of their kind. Membership in the walk
    // decides, never a stat: a file system that ignores case finds `a.txt` after a rename to
    // `A.txt`.
    // The whole tree is every row: read them in id order, without the path index.
    let scope_rows: Vec<(u64, Kind, &WirePath)> = if opts.scope.is_root() && opts.recursive {
        reg.rows().collect()
    } else {
        reg.scope_rows(&opts.scope, opts.recursive)
    };
    let files_in_scope = scope_rows.iter().filter(|r| r.1 == Kind::File).count();
    let mut gone: Vec<Gone> = scope_rows
        .into_iter()
        .filter(|(_, kind, path)| on_disk.get(path) != Some(kind))
        .map(|(id, kind, path)| Gone {
            id,
            kind,
            path: path.clone(),
        })
        .collect();
    // Gone files outside the scope: below a gone folder, or above the scope.
    let mut gone_outside = 0;
    if !opts.recursive {
        // A gone folder takes its subtree along, which a scan that is not recursive doesn't see.
        let tops: Vec<WirePath> = gone
            .iter()
            .filter(|g| g.kind == Kind::Folder)
            .map(|g| g.path.clone())
            .collect();
        let mut known: HashSet<u64> = gone.iter().map(|g| g.id).collect();
        for top in &tops {
            for (id, kind, path) in reg.subtree_rows(top) {
                // Tops nest when the scope itself is gone: each row once.
                if known.insert(id) {
                    if kind == Kind::File && !in_scope(&opts.scope, opts.recursive, path) {
                        gone_outside += 1;
                    }
                    gone.push(Gone {
                        id,
                        kind,
                        path: path.clone(),
                    });
                }
            }
        }
    }
    // A folder above the scope whose row is a file (a file replaced by hand with a folder of
    // the same name): the file is gone, and the folder is new below.
    for path in parents {
        if let Some(id) = reg.id_of(path)
            && reg.get(id).is_some_and(|(k, _)| *k == Kind::File)
        {
            if !in_scope(&opts.scope, opts.recursive, path) {
                gone_outside += 1;
            }
            gone.push(Gone {
                id,
                kind: Kind::File,
                path: path.clone(),
            });
        }
    }
    // Every file row is in the scope or outside it, and the gone ones outside are counted above
    // (each id once: a parent above the scope is never below it).
    let left_outside = reg
        .files()
        .saturating_sub(files_in_scope)
        .saturating_sub(gone_outside);
    (gone, left_outside)
}

/// Compare the scope on disk (`disk`, hashed; `parents` are the folders above the scope) with
/// the registry rows in the scope and plan the changes (see the module docs). `cache` holds the
/// MD5s the store knew for the gone paths (the walk only hashed paths on disk).
fn plan(
    reg: &Registry,
    cache: &HashCache,
    opts: &ScanOptions,
    disk: Vec<OnDisk>,
    parents: Vec<WirePath>,
) -> Plan {
    let has_row = |path: &WirePath, kind: Kind| {
        reg.id_of(path)
            .and_then(|id| reg.get(id))
            .is_some_and(|(k, _)| *k == kind)
    };
    let on_disk: HashMap<&WirePath, Kind> = disk.iter().map(|d| (&d.path, d.kind)).collect();
    let (gone, left_outside) = gone_rows(reg, opts, &on_disk, &parents);

    let mut p = Plan {
        left: left_outside + disk.iter().filter(|d| d.kind == Kind::File).count(),
        files_on_disk: disk
            .iter()
            .filter(|d| d.kind == Kind::File)
            .map(|d| d.path.display())
            .collect(),
        ..Plan::default()
    };
    let mut new_folders: Vec<OnDisk> = parents
        .into_iter()
        .filter(|path| !has_row(path, Kind::Folder))
        .map(|path| OnDisk {
            path,
            kind: Kind::Folder,
            size: 0,
            md5: String::new(),
            prev: None,
            unstable: false,
        })
        .collect();
    let mut new_files = Vec::new();
    for d in disk {
        if d.unstable {
            // Present, so never gone; journaled by a later scan once it holds still.
            continue;
        }
        if !has_row(&d.path, d.kind) {
            match d.kind {
                Kind::Folder => new_folders.push(d),
                Kind::File => new_files.push(d),
            }
        } else if d.kind == Kind::File
            && let Some(id) = reg.id_of(&d.path)
            && d.prev.as_ref().is_some_and(|prev| *prev != d.md5)
        {
            p.modified.push((id, d));
        }
    }

    // Moves: a gone file and a new file with the same size and MD5, each the only one with
    // them on its side. The new path must be free of other rows (a gone folder there is removed later).
    let mut gone_files = Vec::new();
    for g in gone {
        match g.kind {
            Kind::File => gone_files.push(g),
            // A scan never deletes a native folder (an imported account refuses a tree without
            // them before this point).
            Kind::Folder if is_native_folder(&g.path) => {}
            Kind::Folder => p.gone_folders.push(g),
        }
    }
    // (size, MD5) -> the gone files and the new files with them.
    type Sides = (Vec<usize>, Vec<usize>);
    let mut by_content: HashMap<(u64, &str), Sides> = HashMap::new();
    for (i, g) in gone_files.iter().enumerate() {
        if let Some(key) = cache.cached_with_size(&g.path) {
            by_content.entry(key).or_default().0.push(i);
        }
    }
    for (i, d) in new_files.iter().enumerate() {
        by_content
            .entry((d.size, d.md5.as_str()))
            .or_default()
            .1
            .push(i);
    }
    let mut pairs: Vec<(usize, usize)> = by_content
        .values()
        .filter_map(|(g, n)| match (g.as_slice(), n.as_slice()) {
            ([g], [n]) if reg.id_of(&new_files[*n].path).is_none() => Some((*g, *n)),
            _ => None,
        })
        .collect();
    pairs.sort_unstable();
    let paired_gone: HashSet<usize> = pairs.iter().map(|(g, _)| *g).collect();
    let paired_new: HashSet<usize> = pairs.iter().map(|(_, n)| *n).collect();
    let mut new_files: Vec<Option<OnDisk>> = new_files.into_iter().map(Some).collect();
    for (g, n) in &pairs {
        let d = new_files[*n].take().expect("each new file pairs once");
        p.moved
            .push((gone_files[*g].id, gone_files[*g].path.clone(), d));
    }
    p.gone_files = gone_files
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !paired_gone.contains(i))
        .map(|(_, g)| g)
        .collect();
    let mut new_files: Vec<OnDisk> = new_files
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !paired_new.contains(i))
        .filter_map(|(_, d)| d)
        .collect();

    // Journal order; new ids in that order.
    new_folders.sort_by(|a, b| {
        (a.path.segments().len(), &a.path).cmp(&(b.path.segments().len(), &b.path))
    });
    new_files.sort_by(|a, b| a.path.cmp(&b.path));
    p.modified.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    p.moved.sort_by(|a, b| a.2.path.cmp(&b.2.path));
    p.gone_files.sort_by(|a, b| a.path.cmp(&b.path));
    p.gone_folders.sort_by(|a, b| {
        (b.path.segments().len(), &a.path).cmp(&(a.path.segments().len(), &b.path))
    });
    let mut next = reg.next();
    let mut with_id = |entry: OnDisk| {
        let id = next;
        next += 1;
        New { id, entry }
    };
    p.new_folders = new_folders.into_iter().map(&mut with_id).collect();
    p.new_files = new_files.into_iter().map(&mut with_id).collect();
    p
}

#[cfg(test)]
mod tests {
    use super::super::ids::tests::{Rng, random_path, random_registry};
    use super::*;

    /// The gone rows and the files left outside the scope as a walk of every registry row finds
    /// them (the plan before the scope queries).
    fn gone_rows_by_walk(
        reg: &Registry,
        opts: &ScanOptions,
        on_disk: &HashMap<&WirePath, Kind>,
        parents: &[WirePath],
    ) -> (Vec<u64>, usize) {
        let mut gone: Vec<(u64, Kind)> = reg
            .rows()
            .filter(|(_, kind, path)| {
                in_scope(&opts.scope, opts.recursive, path) && on_disk.get(path) != Some(kind)
            })
            .map(|(id, kind, _)| (id, kind))
            .collect();
        if !opts.recursive {
            let tops: Vec<&WirePath> = gone
                .iter()
                .filter(|g| g.1 == Kind::Folder)
                .filter_map(|g| reg.get(g.0).map(|(_, p)| p))
                .collect();
            let known: HashSet<u64> = gone.iter().map(|g| g.0).collect();
            let below: Vec<(u64, Kind)> = reg
                .rows()
                .filter(|(id, _, path)| {
                    !known.contains(id) && tops.iter().any(|t| path.starts_with(t))
                })
                .map(|(id, kind, _)| (id, kind))
                .collect();
            gone.extend(below);
        }
        for path in parents {
            if let Some(id) = reg.id_of(path)
                && reg.get(id).is_some_and(|(k, _)| *k == Kind::File)
            {
                gone.push((id, Kind::File));
            }
        }
        let gone_ids: HashSet<u64> = gone.iter().map(|g| g.0).collect();
        let left_outside = reg
            .rows()
            .filter(|(id, kind, path)| {
                *kind == Kind::File
                    && !gone_ids.contains(id)
                    && !in_scope(&opts.scope, opts.recursive, path)
            })
            .count();
        let mut ids: Vec<u64> = gone.iter().map(|g| g.0).collect();
        ids.sort_unstable();
        (ids, left_outside)
    }

    #[test]
    fn the_scope_queries_find_what_a_walk_of_every_row_finds() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for seed in 1..=300 {
            let reg = random_registry(seed, 50);
            let rows: Vec<(Kind, WirePath)> = reg.rows().map(|(_, k, p)| (k, p.clone())).collect();
            // On disk: some rows as they are, some with the other kind, the rest gone.
            let disk: Vec<(WirePath, Kind)> = rows
                .iter()
                .filter_map(|(k, p)| match rng.next() % 4 {
                    0 => None,
                    1 => Some((
                        p.clone(),
                        if *k == Kind::File {
                            Kind::Folder
                        } else {
                            Kind::File
                        },
                    )),
                    _ => Some((p.clone(), *k)),
                })
                .collect();
            let on_disk: HashMap<&WirePath, Kind> = disk.iter().map(|(p, k)| (p, *k)).collect();
            // Scopes: the root, rows of each kind and paths without a row.
            let mut scopes = vec![WirePath::root()];
            scopes.extend((0..4).map(|_| random_path(&mut rng)));
            scopes.extend(
                (0..4)
                    .filter(|_| !rows.is_empty())
                    .map(|_| rng.pick(&rows).1.clone()),
            );
            for scope in scopes {
                // Every folder above the scope, as the walk passes them.
                let parents: Vec<WirePath> = (1..scope.segments().len())
                    .map(|n| WirePath::from_segments(scope.segments()[..n].to_vec()).unwrap())
                    .collect();
                for recursive in [true, false] {
                    let opts = ScanOptions {
                        scope: scope.clone(),
                        recursive,
                        ..ScanOptions::all(1)
                    };
                    let (gone, left) = gone_rows(&reg, &opts, &on_disk, &parents);
                    let mut ids: Vec<u64> = gone.iter().map(|g| g.id).collect();
                    ids.sort_unstable();
                    assert_eq!(
                        (ids, left),
                        gone_rows_by_walk(&reg, &opts, &on_disk, &parents),
                        "seed {seed}, scope {scope:?}, recursive {recursive}"
                    );
                }
            }
        }
    }
}
