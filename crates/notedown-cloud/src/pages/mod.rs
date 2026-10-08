//! The page index of `.note` files: which page of a note changed, and which part of it.
//!
//! After every `file.created`, `file.modified` or `file.copied` of a `.note`, the indexer parses
//! the new bytes with `notedown-note` (no OCR), fingerprints every page ([`fingerprint`]) and
//! compares the result with the state before the write. The differences become page events in
//! the same [`Commit`], right after the file event, so one commit gives one journal append:
//!
//! - `page.added`: a `PAGEID` the note did not have.
//! - `page.modified`: the same `PAGEID` with another total hash; `parts` lists the changed parts
//!   with their old and new hashes.
//! - `page.removed`: a `PAGEID` the note no longer has.
//! - `page.moved`: a page whose order changed against the other pages. The pages kept in place
//!   are a longest common subsequence of the old and new `PAGEID` order; only the other pages
//!   move. So an insert at the front moves nothing, and a swap of two pages moves one of them.
//!   A page outside that subsequence whose position number stays the same gets no event.
//! - `page.index_failed`: the note does not parse or is larger than [`MAX_NOTE_BYTES`]. The
//!   commit goes on with its file event; the indexer never fails a sync.
//!
//! Event order: all `page.removed` (by old position), then per new position `page.added`, or
//! `page.moved` followed by `page.modified`. `file.created` and `file.modified` carry
//! `pagesChanged`, the number of these events; it is absent when the index failed.
//!
//! # Records and keys
//!
//! `accounts/<slug>/pages/<key>.json` ([`PageRecord`]) holds the fingerprints of a note at its
//! last indexed write, with its `path`, `size` and `md5`. The key is the note's `FILE_ID` when it
//! has the documented shape (`F` + letters and digits, docs/note-format.md 3.1), else
//! `path:<md5 of the wire path>` ([`note_key`]); the file is `<FILE_ID>.json` or
//! `path-<md5>.json`. A copy has the `FILE_ID` of its original: when that `FILE_ID` is already
//! indexed for another path that still exists, the new file is keyed by its path.
//!
//! # The state before a write
//!
//! For a write onto an existing file, the baseline is the record that describes the file at that
//! path now (same `path` and `md5`). Without one (no record, a stale record after a crash, a note
//! placed before the indexer ran, a corrupt record file) the current file is still on disk before
//! the write, so it is fingerprinted and used instead. A record therefore heals itself, and a
//! stale record never produces events for changes that are not real.
//!
//! When the new note has another `FILE_ID` than the note it replaces, every page of the old note
//! is `page.removed` (with the old key), the old record is dropped, and every page of the new
//! note is `page.added`.
//!
//! # Write order
//!
//! Records are written by [`Store::commit`] after the data change and before the journal append,
//! under the store lock ([`crate::store::SideWrite`]). They are not part of the intent in
//! `pending.json`: a missing or stale record is harmless (see above). [`Store::snapshot`] writes
//! the record of every note it journals, before its append. Moves carry the record along with
//! [`plan_page_move`].

pub mod fingerprint;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::journal::{EventBody, FileKind, PartChange};
use crate::store::{Commit, Kind, PendingOp, SideWrite, Store, StoreGuard, WirePath, WriteGuard};

pub use fingerprint::{
    HASH_HEX_LEN, NoteFingerprint, PARTS, PageFingerprint, fingerprint_note, short_hash,
};

/// `accounts/<slug>/pages/`: one record per note.
pub const PAGES_DIR: &str = "pages";
/// Notes larger than this are not parsed: `page.index_failed`.
pub const MAX_NOTE_BYTES: u64 = 256 * 1024 * 1024;

/// `pages/<key>.json`: the fingerprints of a note at its last indexed write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRecord {
    /// The key: `FILE_ID` or `path:<md5>`.
    pub file_id: String,
    /// The note's `FILE_ID` when it has the documented shape; `None` otherwise. It differs from
    /// the key for a copy keyed by its path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    /// The wire path of the file the record describes.
    pub path: String,
    /// Size and lowercase hex MD5 of that file. A record from before these fields has `0` and
    /// `""`, which never match, so it is healed at the next write.
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub md5: String,
    pub pages: Vec<PageFingerprint>,
}

/// A new page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedPage {
    pub page_id: String,
    pub position: usize,
}

/// A page with other content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifiedPage {
    pub page_id: String,
    pub position: usize,
    pub parts: Vec<PartChange>,
}

/// A page that is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedPage {
    pub page_id: String,
    /// The old position.
    pub position: usize,
}

/// A page whose order changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedPage {
    pub page_id: String,
    pub from: usize,
    pub to: usize,
}

/// What changed in a note against the state before the write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageDiff {
    /// The note's key (`FILE_ID` or `path:<md5>`); empty when the note did not parse.
    pub file_id: String,
    /// The key of the replaced note when it had another `FILE_ID`; `page.removed` carries it.
    pub old_file_id: Option<String>,
    /// The wire path of the note.
    pub path: String,
    pub added: Vec<AddedPage>,
    pub modified: Vec<ModifiedPage>,
    pub removed: Vec<RemovedPage>,
    pub moved: Vec<MovedPage>,
    /// Why the note could not be indexed. The other lists are empty then.
    pub failed: Option<String>,
    /// The record to store; `None` when the index failed.
    pub record: Option<PageRecord>,
    /// Keys of records to delete: the record of a replaced note, or a record for this path
    /// under another key.
    pub drop_records: Vec<String>,
}

impl PageDiff {
    fn failed(path: &WirePath, reason: String) -> PageDiff {
        PageDiff {
            path: path.display(),
            failed: Some(reason),
            ..PageDiff::default()
        }
    }

    /// The number of page events (`added`, `modified`, `removed`, `moved`); `None` when the
    /// index failed.
    pub fn pages_changed(&self) -> Option<u64> {
        if self.failed.is_some() {
            return None;
        }
        let n = self.added.len() + self.modified.len() + self.removed.len() + self.moved.len();
        Some(n as u64)
    }

    /// The page events for the note with store id `id`, in journal order (see the module docs).
    pub fn events(&self, id: u64) -> Vec<EventBody> {
        if let Some(reason) = &self.failed {
            return vec![EventBody::PageIndexFailed {
                id,
                path: self.path.clone(),
                reason: reason.clone(),
            }];
        }
        let file_id = || self.file_id.clone();
        let removed_id = self.old_file_id.as_ref().unwrap_or(&self.file_id);
        let mut out: Vec<EventBody> = self
            .removed
            .iter()
            .map(|r| EventBody::PageRemoved {
                id,
                file_id: removed_id.clone(),
                page_id: r.page_id.clone(),
            })
            .collect();
        // (position, rank) keeps added, moved, modified per position in that order
        let mut rest: Vec<(usize, u8, EventBody)> = Vec::new();
        for a in &self.added {
            rest.push((
                a.position,
                0,
                EventBody::PageAdded {
                    id,
                    file_id: file_id(),
                    page_id: a.page_id.clone(),
                    position: a.position as u64,
                },
            ));
        }
        for m in &self.moved {
            rest.push((
                m.to,
                1,
                EventBody::PageMoved {
                    id,
                    file_id: file_id(),
                    page_id: m.page_id.clone(),
                    from: m.from as u64,
                    to: m.to as u64,
                },
            ));
        }
        for m in &self.modified {
            rest.push((
                m.position,
                2,
                EventBody::PageModified {
                    id,
                    file_id: file_id(),
                    page_id: m.page_id.clone(),
                    position: m.position as u64,
                    parts: m.parts.clone(),
                },
            ));
        }
        rest.sort_by_key(|(position, rank, _)| (*position, *rank));
        out.extend(rest.into_iter().map(|(_, _, e)| e));
        out
    }
}

/// Indexes into `seq` of one longest strictly increasing subsequence (patience sorting with
/// predecessor links; deterministic).
fn longest_increasing(seq: &[usize]) -> HashSet<usize> {
    let mut tails: Vec<usize> = Vec::new(); // index into seq of the smallest tail per length
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let at = tails.partition_point(|&t| seq[t] < v);
        prev[i] = at.checked_sub(1).map(|p| tails[p]);
        if at == tails.len() {
            tails.push(i);
        } else {
            tails[at] = i;
        }
    }
    let mut keep = HashSet::new();
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        keep.insert(i);
        cur = prev[i];
    }
    keep
}

/// Compare two fingerprint lists by `pageId` (a pure function; `file_id`, `path` and `record`
/// stay empty). A `pageId` that appears twice in a list counts once, at its first position.
pub fn diff(old: &[PageFingerprint], new: &[PageFingerprint]) -> PageDiff {
    let mut by_id: HashMap<&str, &PageFingerprint> = HashMap::new();
    for page in old {
        by_id.entry(page.page_id.as_str()).or_insert(page);
    }
    let new_ids: HashSet<&str> = new.iter().map(|p| p.page_id.as_str()).collect();
    let mut d = PageDiff::default();
    let mut removed_seen = HashSet::new();
    for page in old {
        if !new_ids.contains(page.page_id.as_str()) && removed_seen.insert(page.page_id.as_str()) {
            d.removed.push(RemovedPage {
                page_id: page.page_id.clone(),
                position: page.position,
            });
        }
    }
    // the pages both lists have, in new order, with their old page
    let mut new_seen = HashSet::new();
    let mut common: Vec<(&PageFingerprint, &PageFingerprint)> = Vec::new();
    for page in new {
        if !new_seen.insert(page.page_id.as_str()) {
            continue;
        }
        match by_id.get(page.page_id.as_str()) {
            Some(before) => common.push((before, page)),
            None => d.added.push(AddedPage {
                page_id: page.page_id.clone(),
                position: page.position,
            }),
        }
    }
    let old_order: Vec<usize> = common.iter().map(|(b, _)| b.position).collect();
    let kept = longest_increasing(&old_order);
    for (i, (before, page)) in common.iter().enumerate() {
        if !kept.contains(&i) && before.position != page.position {
            d.moved.push(MovedPage {
                page_id: page.page_id.clone(),
                from: before.position,
                to: page.position,
            });
        }
        if before.total != page.total {
            let parts = before
                .parts()
                .into_iter()
                .zip(page.parts())
                .filter(|((_, a), (_, b))| a != b)
                .map(|((name, old), (_, new))| PartChange {
                    name: name.to_string(),
                    old,
                    new,
                })
                .collect();
            d.modified.push(ModifiedPage {
                page_id: page.page_id.clone(),
                position: page.position,
                parts,
            });
        }
    }
    d
}

/// The `FILE_ID` when it is `F` followed by 1 to 63 ASCII letters and digits
/// (docs/note-format.md 3.1); it names a file, so nothing else gets through.
pub fn valid_file_id(file_id: Option<&str>) -> Option<String> {
    file_id
        .filter(|id| {
            id.len() >= 2
                && id.len() <= 64
                && id.starts_with('F')
                && id.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(str::to_string)
}

/// `path:<lowercase hex md5 of the wire path>`.
pub fn path_key(path: &WirePath) -> String {
    format!(
        "path:{}",
        const_hex::encode(Md5::digest(path.display().as_bytes()))
    )
}

/// The key of a note: its valid `FILE_ID` ([`valid_file_id`]), else [`path_key`]. A copy of an
/// indexed note is keyed by its path instead (see the module docs); this function does not
/// look at records.
pub fn note_key(file_id: Option<&str>, path: &WirePath) -> String {
    valid_file_id(file_id).unwrap_or_else(|| path_key(path))
}

/// The record file of `key`: `pages/<FILE_ID>.json` or `pages/path-<md5>.json`.
/// `None` for anything but a valid key ([`valid_key`]), so no record path is ever built from
/// other text.
fn record_path(store: &Store, key: &str) -> Option<PathBuf> {
    if !valid_key(key) {
        return None;
    }
    let name = match key.strip_prefix("path:") {
        Some(md5) => format!("path-{md5}.json"),
        None => format!("{key}.json"),
    };
    Some(store.account_dir().join(PAGES_DIR).join(name))
}

/// True for a key this module makes: a valid `FILE_ID` ([`valid_file_id`]) or `path:` followed
/// by 32 lowercase hex digits ([`path_key`]).
pub fn valid_key(key: &str) -> bool {
    match key.strip_prefix("path:") {
        Some(md5) => {
            md5.len() == 32
                && md5
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => valid_file_id(Some(key)).is_some(),
    }
}

/// The side write that stores `record` under its key, or removes the record of `key`.
fn record_write(store: &Store, key: &str, bytes: Option<Vec<u8>>) -> Option<SideWrite> {
    Some(SideWrite {
        path: record_path(store, key)?,
        bytes,
    })
}

/// The stored record of `key`. A missing record is `None`; so is an unreadable one, and one
/// whose `fileId` is not `key` or whose `noteId` is not a valid `FILE_ID` (it is replaced by the
/// next write), with a warning. Record content never names a file.
pub fn read_record(store: &Store, key: &str) -> Option<PageRecord> {
    let path = record_path(store, key)?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("warning: cannot read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_slice::<PageRecord>(&bytes) {
        Ok(r)
            if r.file_id == key && r.note_id.iter().all(|id| valid_file_id(Some(id)).is_some()) =>
        {
            Some(r)
        }
        Ok(_) => {
            eprintln!(
                "warning: {} has a fileId or noteId that does not match its name; the note is \
                 indexed anew",
                path.display()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "warning: {} is not a page record ({e}); the note is indexed anew",
                path.display()
            );
            None
        }
    }
}

fn is_file_at(store: &Store, wire: &str) -> bool {
    WirePath::parse_named(wire).is_ok_and(|p| store.fs_path(&p).is_file())
}

/// The key for the note with `note_id` at `target`: the `FILE_ID`, unless a record of that
/// `FILE_ID` describes another path that still exists (a copy); then the path key.
fn choose_key(store: &Store, note_id: Option<&str>, target: &WirePath) -> String {
    let Some(id) = note_id else {
        return path_key(target);
    };
    match read_record(store, id) {
        Some(r) if r.path != target.display() && is_file_at(store, &r.path) => path_key(target),
        _ => id.to_string(),
    }
}

/// Read and fingerprint a note on disk, refusing files over `limit` before reading.
fn fingerprint_file(path: &Path, limit: u64) -> Result<NoteFingerprint, String> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > limit => Err(too_large(m.len(), limit)),
        Ok(_) => std::fs::read(path)
            .map_err(|e| format!("cannot read the note: {e}"))
            .and_then(fingerprint_note),
        Err(e) => Err(format!("cannot read the note: {e}")),
    }
}

/// The note at a path before a write.
#[derive(Clone)]
struct Before {
    /// The record that describes it, or a stale record for that path.
    key: Option<String>,
    note_id: Option<String>,
    pages: Vec<PageFingerprint>,
}

/// The state of the file at `target` (MD5 `md5`) before the write: the record that describes
/// it, else a fresh fingerprint of the file.
fn before(store: &Store, target: &WirePath, md5: &str, new_key: &str, limit: u64) -> Before {
    let display = target.display();
    let mut candidates = vec![new_key.to_string()];
    let by_path = path_key(target);
    if by_path != new_key {
        candidates.push(by_path);
    }
    let mut stale: Option<(String, PageRecord)> = None;
    for key in &candidates {
        if let Some(r) = read_record(store, key)
            && r.path == display
        {
            if r.md5 == md5 {
                return Before {
                    key: Some(key.clone()),
                    note_id: r.note_id,
                    pages: r.pages,
                };
            }
            if stale.is_none() {
                stale = Some((key.clone(), r));
            }
        }
    }
    match fingerprint_file(&store.fs_path(target), limit) {
        Ok(n) => {
            let note_id = valid_file_id(n.file_id.as_deref());
            let key = stale.map(|(k, _)| k).or_else(|| {
                note_id
                    .clone()
                    .filter(|id| read_record(store, id).is_some_and(|r| r.path == display))
            });
            Before {
                key,
                note_id,
                pages: n.pages,
            }
        }
        // The old file does not parse (a truncated upload, say): the last good state recorded
        // for this path is the best baseline; without one there is nothing to compare with.
        Err(_) => match stale {
            Some((key, r)) => Before {
                key: Some(key),
                note_id: r.note_id,
                pages: r.pages,
            },
            None => Before {
                key: None,
                note_id: None,
                pages: Vec::new(),
            },
        },
    }
}

/// What a write is, for [`compute`].
struct Write<'a> {
    target: &'a WirePath,
    md5: String,
    size: u64,
    /// MD5 of the file at `target` before the write; `None` for a new file.
    current_md5: Option<String>,
    limit: u64,
    /// The state before, when the caller knows it (the old file is no longer on disk).
    known: Option<Before>,
}

fn compute(store: &Store, w: &Write<'_>, note: NoteFingerprint) -> PageDiff {
    let note_id = valid_file_id(note.file_id.as_deref());
    let key = choose_key(store, note_id.as_deref(), w.target);
    let mut d = match &w.current_md5 {
        None => diff(&[], &note.pages),
        Some(md5) => {
            let b = match &w.known {
                Some(b) => b.clone(),
                None => before(store, w.target, md5, &key, w.limit),
            };
            let mut d = if b.note_id != note_id {
                // another note at this path: all old pages go, all new pages come
                let mut d = diff(&[], &note.pages);
                d.removed = b
                    .pages
                    .iter()
                    .map(|p| RemovedPage {
                        page_id: p.page_id.clone(),
                        position: p.position,
                    })
                    .collect();
                d.old_file_id = Some(
                    b.key
                        .clone()
                        .unwrap_or_else(|| note_key(b.note_id.as_deref(), w.target)),
                );
                d
            } else {
                diff(&b.pages, &note.pages)
            };
            d.drop_records = b.key.into_iter().filter(|k| *k != key).collect();
            d
        }
    };
    d.path = w.target.display();
    d.record = Some(PageRecord {
        file_id: key.clone(),
        note_id,
        path: d.path.clone(),
        size: w.size,
        md5: w.md5.clone(),
        pages: note.pages,
    });
    d.file_id = key;
    d
}

fn index_bytes(store: &Store, w: &Write<'_>, bytes: Vec<u8>) -> PageDiff {
    if bytes.len() as u64 > w.limit {
        return PageDiff::failed(w.target, too_large(bytes.len() as u64, w.limit));
    }
    match fingerprint_note(bytes) {
        Ok(note) => compute(store, w, note),
        Err(reason) => PageDiff::failed(w.target, reason),
    }
}

/// Index the note `bytes` about to be written to `path`, against the file there now (see the
/// module docs). Nothing is written. Never fails: a note that does not parse or is larger than
/// [`MAX_NOTE_BYTES`] gives a diff with `failed`. `bytes` is moved into the parser, not copied.
pub fn index_note(store: &Store, guard: &StoreGuard, path: &WirePath, bytes: Vec<u8>) -> PageDiff {
    let current_md5 = if store.fs_path(path).is_file() {
        store.md5_of(guard, path).ok()
    } else {
        None
    };
    let w = Write {
        target: path,
        md5: const_hex::encode(Md5::digest(&bytes)),
        size: bytes.len() as u64,
        current_md5,
        limit: MAX_NOTE_BYTES,
        known: None,
    };
    index_bytes(store, &w, bytes)
}

fn too_large(size: u64, limit: u64) -> String {
    format!("the note has {size} bytes, more than the limit of {limit} bytes for the page index")
}

/// The one entry point for writers (upload finish, `put`, `import`, copies): add the page events
/// of a planned put or copy of a `.note` to `commit`, right after its file event, set
/// `pagesChanged` on `file.created` / `file.modified`, and attach the record changes so
/// [`Store::commit`] writes them. Call it after `Store::plan_put` or `Store::plan_copy` and before
/// `Store::commit`, under the same guard (the file at the target must still be the old one).
///
/// Returns `None` and changes nothing for anything but a put or copy of a `.note` (other files,
/// an [`PendingOp::Unchanged`] put, moves, deletes, folders). Never fails: an unreadable,
/// oversized or unparsable note gives `page.index_failed`.
pub fn plan_page_events(
    store: &Store,
    guard: &WriteGuard,
    commit: &mut Commit,
) -> Option<PageDiff> {
    plan_page_events_with_limit(store, guard, commit, MAX_NOTE_BYTES)
}

/// [`plan_page_events`] with another size limit. Test hook.
#[doc(hidden)]
pub fn plan_page_events_with_limit(
    store: &Store,
    _guard: &WriteGuard,
    commit: &mut Commit,
    limit: u64,
) -> Option<PageDiff> {
    let (id, target, source, md5, size) = match &commit.op {
        PendingOp::Put {
            id,
            path,
            staged,
            md5,
            size,
            ..
        } => (*id, path, store.account_dir().join(staged), md5, *size),
        PendingOp::Copy {
            id,
            source,
            path,
            md5,
            size,
            ..
        } => {
            let source = WirePath::parse_named(source).ok()?;
            (*id, path, store.fs_path(&source), md5, *size)
        }
        _ => return None,
    };
    let target = WirePath::parse_named(target).ok()?;
    if FileKind::of(target.name().unwrap_or_default()) != FileKind::Note {
        return None;
    }
    let at = commit.events.iter().position(|e| {
        matches!(
            e,
            EventBody::FileCreated { .. }
                | EventBody::FileModified { .. }
                | EventBody::FileCopied { .. }
        )
    })?;
    let current_md5 = match commit.events.get(at) {
        Some(EventBody::FileModified { prev_md5, .. }) => Some(prev_md5.clone()),
        _ => None,
    };
    let w = Write {
        target: &target,
        md5: md5.clone(),
        size,
        current_md5,
        limit,
        known: None,
    };
    let diff = match fingerprint_file(&source, limit) {
        Ok(note) => compute(store, &w, note),
        Err(reason) => PageDiff::failed(&target, reason),
    };
    let changed = diff.pages_changed();
    if let Some(
        EventBody::FileCreated { pages_changed, .. }
        | EventBody::FileModified { pages_changed, .. },
    ) = commit.events.get_mut(at)
    {
        *pages_changed = changed;
    }
    let events = diff.events(id);
    let tail = commit.events.split_off(at + 1);
    commit.events.extend(events);
    commit.events.extend(tail);
    for key in &diff.drop_records {
        commit.side_writes.extend(record_write(store, key, None));
    }
    if let Some(record) = &diff.record {
        let bytes = crate::fsutil::json_bytes(record);
        commit
            .side_writes
            .extend(record_write(store, &record.file_id, Some(bytes)));
    }
    Some(diff)
}

/// Carry the page records along with a planned move (`Store::plan_move`): every `.note` that the
/// move takes along (the file itself, or every note below a moved folder) gets its record's
/// `path` updated, and a record keyed by the old path is re-keyed by the new path. No events.
/// Call it after `plan_move` and before `Store::commit`, under the same guard (the files must
/// still be at the old path). Does nothing for other operations. Never fails: a note it cannot
/// read keeps its record, which heals at the next write.
pub fn plan_page_move(store: &Store, _guard: &WriteGuard, commit: &mut Commit) {
    let PendingOp::Move { kind, from, to, .. } = &commit.op else {
        return;
    };
    let (Ok(from), Ok(to)) = (WirePath::parse_named(from), WirePath::parse_named(to)) else {
        return;
    };
    let mut notes: Vec<(WirePath, WirePath)> = Vec::new();
    match kind {
        Kind::File => {
            if FileKind::of(from.name().unwrap_or_default()) == FileKind::Note {
                notes.push((from, to));
            }
        }
        Kind::Folder => {
            let mut rel = Vec::new();
            collect_notes(&store.fs_path(&from), String::new(), &mut rel);
            for r in rel {
                let old = WirePath::parse_named(&format!("{}/{r}", from.display()));
                let new = WirePath::parse_named(&format!("{}/{r}", to.display()));
                if let (Ok(old), Ok(new)) = (old, new) {
                    notes.push((old, new));
                }
            }
        }
    }
    commit.side_writes.extend(move_records(store, &notes));
}

/// The record changes for notes that moved from the first path to the second (see
/// [`plan_page_move`]): the record's `path` follows, and a record keyed by the old path is
/// re-keyed by the new path. Empty when no note has a record.
pub(crate) fn move_records(store: &Store, notes: &[(WirePath, WirePath)]) -> Vec<SideWrite> {
    if notes.is_empty() {
        return Vec::new();
    }
    // One pass over the records, instead of parsing every moved note for its FILE_ID.
    let by_path = records_by_path(store);
    let mut writes = Vec::new();
    for (old, new) in notes {
        for (key, mut r) in by_path.get(&old.display()).cloned().unwrap_or_default() {
            r.path = new.display();
            if key.starts_with("path:") {
                writes.extend(record_write(store, &key, None));
                r.file_id = path_key(new);
            }
            // `r.file_id` is `key` (checked by read_record) or the new path key
            let bytes = crate::fsutil::json_bytes(&r);
            writes.extend(record_write(store, &r.file_id, Some(bytes)));
        }
    }
    writes
}

/// The page index of a note the scan found new or changed on disk (store id `id`, `md5`,
/// `size`): `pagesChanged` for its file event, the page events, and the record changes. The
/// old note is no longer on disk, so the state before is the record for this path with
/// `prev_md5` (the MD5 the store knew), under any key; without one, the new file is the
/// baseline and the diff is empty. `None` for `prev_md5` is a new note: every page is added.
/// Never fails: a note that does not parse gives `page.index_failed`.
pub(crate) fn scan_note(
    store: &Store,
    path: &WirePath,
    id: u64,
    md5: &str,
    size: u64,
    prev_md5: Option<&str>,
) -> (Option<u64>, Vec<EventBody>, Vec<SideWrite>) {
    let w = Write {
        target: path,
        md5: md5.to_string(),
        size,
        current_md5: prev_md5.map(str::to_string),
        limit: MAX_NOTE_BYTES,
        known: prev_md5.and_then(|prev| {
            let records = records_by_path(store).remove(&path.display())?;
            let (key, r) = records.into_iter().find(|(_, r)| r.md5 == prev)?;
            Some(Before {
                key: Some(key),
                note_id: r.note_id,
                pages: r.pages,
            })
        }),
    };
    let diff = match fingerprint_file(&store.fs_path(path), MAX_NOTE_BYTES) {
        Ok(note) => compute(store, &w, note),
        Err(reason) => PageDiff::failed(path, reason),
    };
    let mut writes = Vec::new();
    for key in &diff.drop_records {
        writes.extend(record_write(store, key, None));
    }
    if let Some(record) = &diff.record {
        let bytes = crate::fsutil::json_bytes(record);
        writes.extend(record_write(store, &record.file_id, Some(bytes)));
    }
    (diff.pages_changed(), diff.events(id), writes)
}

/// Every valid record in `pages/` by its `path`, with its key.
fn records_by_path(store: &Store) -> HashMap<String, Vec<(String, PageRecord)>> {
    let mut out: HashMap<String, Vec<(String, PageRecord)>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(store.account_dir().join(PAGES_DIR)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let key = match stem.strip_prefix("path-") {
            Some(md5) => format!("path:{md5}"),
            None => stem.to_string(),
        };
        if let Some(r) = read_record(store, &key) {
            out.entry(r.path.clone()).or_default().push((key, r));
        }
    }
    out
}

/// Relative paths (`a/b.note`) of the `.note` files below `dir`; dot-files are skipped.
fn collect_notes(dir: &Path, prefix: String, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_notes(&entry.path(), rel, out),
            Ok(t) if t.is_file() && FileKind::of(&name) == FileKind::Note => out.push(rel),
            _ => {}
        }
    }
}

/// The snapshot of the note `path` (store id `id`, `md5`, `size`): `pagesChanged` for its
/// `file.created`, one `page.added` per page (or `page.index_failed`), and the record to write.
pub(crate) fn snapshot_note(
    store: &Store,
    path: &WirePath,
    id: u64,
    md5: &str,
    size: u64,
) -> (Option<u64>, Vec<EventBody>, Option<SideWrite>) {
    match fingerprint_file(&store.fs_path(path), MAX_NOTE_BYTES) {
        Ok(note) => {
            let note_id = valid_file_id(note.file_id.as_deref());
            let key = choose_key(store, note_id.as_deref(), path);
            let events: Vec<EventBody> = note
                .pages
                .iter()
                .map(|p| EventBody::PageAdded {
                    id,
                    file_id: key.clone(),
                    page_id: p.page_id.clone(),
                    position: p.position as u64,
                })
                .collect();
            let record = PageRecord {
                file_id: key.clone(),
                note_id,
                path: path.display(),
                size,
                md5: md5.to_string(),
                pages: note.pages,
            };
            let write = record_write(store, &key, Some(crate::fsutil::json_bytes(&record)));
            (Some(events.len() as u64), events, write)
        }
        Err(reason) => (
            None,
            vec![EventBody::PageIndexFailed {
                id,
                path: path.display(),
                reason,
            }],
            None,
        ),
    }
}
