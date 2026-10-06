//! The page index of `.note` files: which page of a note changed, and which part of it.
//!
//! After every `file.created` or `file.modified` of a `.note`, the indexer parses the new bytes
//! with `notedown-note` (no OCR), fingerprints every page ([`fingerprint`]) and compares the
//! result with the stored record `accounts/<slug>/pages/<key>.json` ([`PageRecord`]). The
//! differences become page events in the same [`Commit`], right after the file event, so one
//! commit gives one journal append:
//!
//! - `page.added`: a `PAGEID` the record does not have.
//! - `page.modified`: the same `PAGEID` with another total hash; `parts` lists the changed parts
//!   with their old and new hashes.
//! - `page.removed`: a `PAGEID` the note no longer has.
//! - `page.moved`: the same `PAGEID` at another position. Every page whose position changes is
//!   moved, so an insert at the front also moves every later page.
//! - `page.index_failed`: the note does not parse or is larger than [`MAX_NOTE_BYTES`]. The
//!   commit goes on with its file event; the indexer never fails a sync.
//!
//! Event order: all `page.removed` (by old position), then per new position `page.added`, or
//! `page.moved` followed by `page.modified`. The file event carries `pagesChanged`, the number
//! of these events; it is absent when the index failed.
//!
//! The key of a note is its `FILE_ID` when that has the documented shape (`F` + letters and
//! digits, docs/note-format.md 3.1), else `path:<md5 of the wire path>` ([`note_key`]). The
//! record file is `<FILE_ID>.json` or `path-<md5>.json`.
//!
//! The record is written by [`Store::commit`] after the data change and before the journal
//! append, under the store lock ([`crate::store::SideWrite`]). It is not part of the intent in
//! `pending.json`: a missing or stale record (a crash between the data change and the record
//! write, a failed write) is harmless, because the next write of that note diffs against
//! whatever record is stored. At worst its page events repeat, and consumers are idempotent.

pub mod fingerprint;

use std::path::PathBuf;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::journal::{EventBody, FileKind, PartChange};
use crate::store::{Commit, PendingOp, SideWrite, Store, StoreGuard, WirePath, WriteGuard};

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
    /// The wire path at the last indexed write.
    pub path: String,
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

/// A page at another position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedPage {
    pub page_id: String,
    pub from: usize,
    pub to: usize,
}

/// What changed in a note against its stored record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageDiff {
    /// The note's key (`FILE_ID` or `path:<md5>`); empty when the note did not parse.
    pub file_id: String,
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
        let mut out: Vec<EventBody> = self
            .removed
            .iter()
            .map(|r| EventBody::PageRemoved {
                id,
                file_id: file_id(),
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

/// Compare two fingerprint lists by `pageId` (a pure function; `file_id`, `path` and `record`
/// stay empty). A `pageId` that appears twice in a list counts once, at its first position.
pub fn diff(old: &[PageFingerprint], new: &[PageFingerprint]) -> PageDiff {
    let mut by_id: std::collections::HashMap<&str, &PageFingerprint> =
        std::collections::HashMap::new();
    for page in old {
        by_id.entry(page.page_id.as_str()).or_insert(page);
    }
    let new_ids: std::collections::HashSet<&str> = new.iter().map(|p| p.page_id.as_str()).collect();
    let mut d = PageDiff::default();
    let mut removed_seen = std::collections::HashSet::new();
    for page in old {
        if !new_ids.contains(page.page_id.as_str()) && removed_seen.insert(page.page_id.as_str()) {
            d.removed.push(RemovedPage {
                page_id: page.page_id.clone(),
                position: page.position,
            });
        }
    }
    let mut new_seen = std::collections::HashSet::new();
    for page in new {
        if !new_seen.insert(page.page_id.as_str()) {
            continue;
        }
        let Some(before) = by_id.get(page.page_id.as_str()) else {
            d.added.push(AddedPage {
                page_id: page.page_id.clone(),
                position: page.position,
            });
            continue;
        };
        if before.position != page.position {
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

/// The key of a note: its `FILE_ID` when it is `F` followed by 1 to 63 ASCII letters and
/// digits (docs/note-format.md 3.1), else `path:<lowercase hex md5 of the wire path>`. The key
/// names a file, so nothing else from the note gets into it.
pub fn note_key(file_id: Option<&str>, path: &WirePath) -> String {
    match file_id {
        Some(id)
            if id.len() >= 2
                && id.len() <= 64
                && id.starts_with('F')
                && id.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            id.to_string()
        }
        _ => format!(
            "path:{}",
            const_hex::encode(Md5::digest(path.display().as_bytes()))
        ),
    }
}

/// The record file of `key`: `pages/<FILE_ID>.json` or `pages/path-<md5>.json`.
fn record_path(store: &Store, key: &str) -> PathBuf {
    let name = match key.strip_prefix("path:") {
        Some(md5) => format!("path-{md5}.json"),
        None => format!("{key}.json"),
    };
    store.account_dir().join(PAGES_DIR).join(name)
}

/// The stored record of `key`. A missing record is `None`; so is an unreadable one (it is
/// replaced by the next write), with a warning.
pub fn read_record(store: &Store, key: &str) -> Option<PageRecord> {
    let path = record_path(store, key);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("warning: cannot read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!(
                "warning: {} is not a page record ({e}); the note is indexed anew",
                path.display()
            );
            None
        }
    }
}

/// Index the note `bytes` written to `path`: fingerprint it and compare it with the stored
/// record. Nothing is written. Never fails: a note that does not parse or is larger than
/// [`MAX_NOTE_BYTES`] gives a diff with `failed`.
pub fn index_note(store: &Store, guard: &StoreGuard, path: &WirePath, bytes: &[u8]) -> PageDiff {
    if bytes.len() as u64 > MAX_NOTE_BYTES {
        return PageDiff::failed(path, too_large(bytes.len() as u64, MAX_NOTE_BYTES));
    }
    index_with_limit(store, guard, path, bytes.to_vec(), MAX_NOTE_BYTES)
}

fn index_with_limit(
    store: &Store,
    _guard: &StoreGuard,
    path: &WirePath,
    bytes: Vec<u8>,
    limit: u64,
) -> PageDiff {
    if bytes.len() as u64 > limit {
        return PageDiff::failed(path, too_large(bytes.len() as u64, limit));
    }
    let note = match fingerprint::fingerprint_owned(bytes) {
        Ok(n) => n,
        Err(reason) => return PageDiff::failed(path, reason),
    };
    let key = note_key(note.file_id.as_deref(), path);
    let old = read_record(store, &key)
        .map(|r| r.pages)
        .unwrap_or_default();
    let mut d = diff(&old, &note.pages);
    d.path = path.display();
    d.record = Some(PageRecord {
        file_id: key.clone(),
        path: d.path.clone(),
        pages: note.pages,
    });
    d.file_id = key;
    d
}

fn too_large(size: u64, limit: u64) -> String {
    format!("the note has {size} bytes, more than the limit of {limit} bytes for the page index")
}

/// The one entry point for writers (upload finish, `put`, `import`): add the page events of a
/// planned put of a `.note` to `commit`, right after its file event, set `pagesChanged` on that
/// event, and attach the page record so [`Store::commit`] writes it. Call it after
/// `Store::plan_put` and before `Store::commit`, under the same guard.
///
/// Returns `None` and changes nothing for anything but a `put` of a `.note` (other files, an
/// [`PendingOp::Unchanged`] put, moves, copies, deletes, folders). Never fails: an unreadable,
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
    guard: &WriteGuard,
    commit: &mut Commit,
    limit: u64,
) -> Option<PageDiff> {
    let PendingOp::Put {
        id, path, staged, ..
    } = &commit.op
    else {
        return None;
    };
    let id = *id;
    let wire = WirePath::parse_named(path).ok()?;
    if FileKind::of(wire.name().unwrap_or_default()) != FileKind::Note {
        return None;
    }
    let staged = store.account_dir().join(staged);
    let diff = match std::fs::metadata(&staged) {
        Ok(m) if m.len() > limit => PageDiff::failed(&wire, too_large(m.len(), limit)),
        Ok(_) => match std::fs::read(&staged) {
            Ok(bytes) => index_with_limit(store, guard, &wire, bytes, limit),
            Err(e) => PageDiff::failed(&wire, format!("cannot read the note: {e}")),
        },
        Err(e) => PageDiff::failed(&wire, format!("cannot read the note: {e}")),
    };
    let changed = diff.pages_changed();
    let at = commit.events.iter().position(|e| {
        matches!(
            e,
            EventBody::FileCreated { .. } | EventBody::FileModified { .. }
        )
    })?;
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
    if let Some(record) = &diff.record {
        commit.side_writes.push(SideWrite {
            path: record_path(store, &record.file_id),
            bytes: crate::fsutil::json_bytes(record),
        });
    }
    Some(diff)
}

/// The snapshot events of the note `path` (store id `id`): `pagesChanged` for its
/// `file.created` and one `page.added` per page, or `page.index_failed`. Nothing is written.
pub(crate) fn snapshot_note(
    store: &Store,
    path: &WirePath,
    id: u64,
) -> (Option<u64>, Vec<EventBody>) {
    let fs_path = store.fs_path(path);
    let note = match std::fs::metadata(&fs_path) {
        Ok(m) if m.len() > MAX_NOTE_BYTES => Err(too_large(m.len(), MAX_NOTE_BYTES)),
        Ok(_) => std::fs::read(&fs_path)
            .map_err(|e| format!("cannot read the note: {e}"))
            .and_then(fingerprint::fingerprint_owned),
        Err(e) => Err(format!("cannot read the note: {e}")),
    };
    match note {
        Ok(note) => {
            let file_id = note_key(note.file_id.as_deref(), path);
            let events: Vec<EventBody> = note
                .pages
                .iter()
                .map(|p| EventBody::PageAdded {
                    id,
                    file_id: file_id.clone(),
                    page_id: p.page_id.clone(),
                    position: p.position as u64,
                })
                .collect();
            (Some(events.len() as u64), events)
        }
        Err(reason) => (
            None,
            vec![EventBody::PageIndexFailed {
                id,
                path: path.display(),
                reason,
            }],
        ),
    }
}
