//! The cache of listed folders. It is plain data: the library owns one behind a lock, and each
//! page merge and each batch of events runs whole under that lock, in the order they come.
//! No method here makes a call; they answer what changed (the events to send) and what the
//! library must fetch next.
//!
//! The rules (docs/rest-api.md, "Paging" and "Following changes"):
//! - Each folder keeps the `seq` it is valid at: the lowest `seq` of its pages while it loads,
//!   and the end of the last batch of events it took. An event is applied to a folder only
//!   when its `seq` is higher. The follower reads from the lowest `seq` plus one.
//! - Events are applied in order, the app's own writes too.
//! - A folder that is not fully loaded takes a create or a move-in only when the name sorts at
//!   or before the last name of its last page; the next page brings the rest.
//! - An unknown id that comes into a listed folder is fetched after the batch.
//! - A folder moved or deleted while it pages (or one above it) is dropped and listed again.
//! - A folder event is sent only when what the window sees changed, so an event applied twice
//!   (a late page lowers the `seq`) is never announced twice.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::{Entry, EntryKind, FolderState, LibraryEvent};

/// The error text of a listed folder that is not there anymore.
pub const GONE: &str = "This folder is not there anymore.";

/// One listing: a folder with all its children, or only its child folders (the tree).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub path: String,
    pub folders_only: bool,
}

impl Key {
    /// The key of `path` with its leading and trailing `/` taken off.
    pub fn new(path: &str, folders_only: bool) -> Self {
        Self {
            path: path.trim_matches('/').to_owned(),
            folders_only,
        }
    }
}

/// A page of `GET /tree`.
#[derive(Debug, Clone, Deserialize)]
pub struct TreePage {
    pub seq: u64,
    pub entries: Vec<Entry>,
    pub next: Option<String>,
}

/// A page of `GET /events`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventPage {
    pub events: Vec<Event>,
    pub next: u64,
    #[serde(default)]
    pub more: bool,
    #[serde(default)]
    pub gap: bool,
}

/// One event of `GET /events`; the fields that do not apply are missing.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Option<u64>,
    pub path: Option<String>,
    pub old_path: Option<String>,
    pub md5: Option<String>,
    pub size: Option<u64>,
}

/// A folder as the window gets it, with the cursor of its next page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub folder: FolderState,
    pub next: Option<String>,
}

/// Allows a fetched page to be merged: the binding and the listing it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    generation: u64,
    epoch: u64,
}

/// What a continuation asks for.
#[derive(Debug)]
pub enum Continue {
    /// Fetch the page and merge it with this ticket.
    Fetch(Ticket),
    /// The cursor is not the folder's next one: this is what the folder holds now.
    Current(Listed),
    /// The folder is not listed: list it from the first page.
    FirstPage,
}

/// What a batch of events left to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Ids (with the `seq` of their first event) that came into a listed folder and that no
    /// listed folder knows, or whose file changed: fetch them.
    pub wanted: Vec<(u64, u64)>,
    /// Listings that were dropped: they wait to be listed again from the first page.
    pub relist: Vec<Key>,
    /// Listings whose folder moved away: the window hears that their path is gone.
    pub gone: Vec<Key>,
    /// `account.removed`: stop following.
    pub stop: bool,
    /// The end of the batch: the `seq` the covered folders are valid at now.
    pub end: u64,
}

#[derive(Debug, Clone)]
struct Folder {
    /// Sorted by the bytes of the names, as the server pages them.
    entries: Vec<Entry>,
    seq: u64,
    /// The cursor of the next page; `None` once complete.
    next: Option<String>,
    /// The last name of the last page; an entry past it waits for its page.
    bound: Option<String>,
    epoch: u64,
    /// Pages that are being fetched.
    loading: u32,
    /// The batch being applied covers this folder: it was valid at the batch's `from - 1` or
    /// later. Only during [`Cache::apply`].
    covered: bool,
    error: Option<String>,
}

impl Folder {
    fn complete(&self) -> bool {
        self.next.is_none()
    }

    /// Whether an entry named `name` belongs to the pages loaded so far.
    fn admits(&self, name: &str) -> bool {
        self.complete()
            || self
                .bound
                .as_deref()
                .is_some_and(|bound| name.as_bytes() <= bound.as_bytes())
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    fn remove(&mut self, id: u64) {
        self.entries.retain(|e| e.id != id);
    }

    /// Puts `entry` in at its place by name, replacing the entry with its id or its name.
    fn put(&mut self, entry: Entry) {
        self.entries
            .retain(|e| e.id != entry.id && e.name != entry.name);
        let at = self
            .entries
            .partition_point(|e| e.name.as_bytes().cmp(entry.name.as_bytes()) == Ordering::Less);
        self.entries.insert(at, entry);
    }

    fn state(&self, path: &str) -> FolderState {
        FolderState {
            path: path.to_owned(),
            entries: self.entries.clone(),
            complete: self.complete(),
            loading: self.loading > 0,
            error: self.error.clone(),
        }
    }
}

/// The listed folders of one binding.
#[derive(Debug, Default)]
pub struct Cache {
    generation: u64,
    folders: HashMap<Key, Folder>,
    /// The epoch of the newest first page asked for each listing.
    first: HashMap<Key, u64>,
    epochs: u64,
    /// Dropped listings that wait to be listed again (a first page that failed is tried at
    /// each step of the follower).
    pending: BTreeSet<Key>,
    /// The listings the window shows; `None` until it says so (then all are kept).
    shown: Option<BTreeSet<Key>>,
}

/// The folder of `path`: everything before its last `/`, or the root.
pub fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |at| &path[..at])
}

/// The last name of `path`.
pub fn name_of(path: &str) -> &str {
    path.rfind('/').map_or(path, |at| &path[at + 1..])
}

/// Whether `path` is `top` or below it: `a` is above `a/x`, not above `a b`.
pub fn at_or_below(path: &str, top: &str) -> bool {
    top.is_empty()
        || path == top
        || (path.len() > top.len() && path.starts_with(top) && path.as_bytes()[top.len()] == b'/')
}

/// The folder event of a listing that is not there: no entries and `text`.
fn gone_event(key: &Key, text: &str) -> LibraryEvent {
    LibraryEvent::Folder {
        folder: FolderState {
            path: key.path.clone(),
            entries: Vec::new(),
            complete: false,
            loading: false,
            error: Some(text.to_owned()),
        },
    }
}

/// `path` with its `old` prefix changed to `new`.
fn rebase(path: &str, old: &str, new: &str) -> String {
    format!("{new}{}", &path[old.len()..])
}

impl Cache {
    /// Drops every folder; merges of other bindings are ignored from now on.
    pub fn clear(&mut self, generation: u64) {
        self.generation = generation;
        self.folders.clear();
        self.first.clear();
        self.pending.clear();
        self.shown = None;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The window shows `keys`: every other listing is dropped and no longer followed.
    pub fn set_shown(&mut self, keys: &[Key]) {
        let shown: BTreeSet<Key> = keys.iter().cloned().collect();
        self.folders.retain(|k, _| shown.contains(k));
        self.pending.retain(|k| shown.contains(k));
        self.shown = Some(shown);
    }

    /// The listings to list again after a gap or a long pause: the shown ones (all of them
    /// while the window has not said). The others are dropped.
    pub fn relist_keys(&mut self, generation: u64) -> Vec<Key> {
        if generation != self.generation {
            return Vec::new();
        }
        if let Some(shown) = &self.shown {
            self.folders.retain(|k, _| shown.contains(k));
        }
        self.keys()
    }

    /// The dropped listings that wait to be listed again.
    pub fn pending(&self) -> Vec<Key> {
        self.pending.iter().cloned().collect()
    }

    pub fn keys(&self) -> Vec<Key> {
        let mut keys: Vec<Key> = self.folders.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The lowest `seq` of the listed folders; the follower reads from one past it.
    pub fn min_seq(&self) -> Option<u64> {
        self.folders.values().map(|f| f.seq).min()
    }

    /// What the window sees of `key`, with the cursor of its next page.
    pub fn listed(&self, key: &Key) -> Option<Listed> {
        self.folders.get(key).map(|f| Listed {
            folder: f.state(&key.path),
            next: f.next.clone(),
        })
    }

    /// The entry with `id` in a listed folder: one the batch covers first, as a folder left
    /// out of it can hold an older state.
    fn known(&self, id: u64) -> Option<Entry> {
        let find = |covered: bool| {
            self.folders
                .values()
                .filter(|f| f.covered == covered)
                .find_map(|f| f.entries.iter().find(|e| e.id == id).cloned())
        };
        find(true).or_else(|| find(false))
    }

    /// A first page of `key` is asked for; a first page asked earlier is then stale.
    pub fn begin_first(&mut self, generation: u64, key: &Key) -> Ticket {
        self.epochs += 1;
        if generation == self.generation {
            self.first.insert(key.clone(), self.epochs);
        }
        Ticket {
            generation,
            epoch: self.epochs,
        }
    }

    /// A page after `cursor` is asked for.
    pub fn begin_next(
        &mut self,
        generation: u64,
        key: &Key,
        cursor: &str,
        out: &mut Vec<LibraryEvent>,
    ) -> Continue {
        if generation != self.generation {
            return Continue::FirstPage;
        }
        let Some(folder) = self.folders.get_mut(key) else {
            return Continue::FirstPage;
        };
        if folder.next.as_deref() != Some(cursor) {
            return Continue::Current(Listed {
                folder: folder.state(&key.path),
                next: folder.next.clone(),
            });
        }
        let before = folder.state(&key.path);
        folder.loading += 1;
        folder.error = None;
        let ticket = Ticket {
            generation,
            epoch: folder.epoch,
        };
        self.announce(key, before, out);
        Continue::Fetch(ticket)
    }

    /// Takes a first page: the folder is what the page holds. A page of another binding, or
    /// one that a newer first page made stale, changes nothing.
    pub fn merge_first(
        &mut self,
        ticket: Ticket,
        key: &Key,
        page: TreePage,
        out: &mut Vec<LibraryEvent>,
    ) -> Option<Listed> {
        if ticket.generation != self.generation || self.first.get(key) != Some(&ticket.epoch) {
            return self.listed(key);
        }
        self.first.remove(key);
        self.pending.remove(key);
        let before = self.folders.get(key).map(|f| f.state(&key.path));
        let mut folder = Folder {
            entries: Vec::new(),
            seq: page.seq,
            bound: page.entries.last().map(|e| e.name.clone()),
            next: page.next,
            epoch: ticket.epoch,
            loading: 0,
            covered: false,
            error: None,
        };
        for entry in page.entries {
            if !key.folders_only || entry.kind == EntryKind::Folder {
                folder.put(entry);
            }
        }
        self.folders.insert(key.clone(), folder);
        match before {
            Some(before) => self.announce(key, before, out),
            None => out.push(self.event(key)),
        }
        self.listed(key)
    }

    /// Takes the page after `cursor`, when the folder still waits for it.
    pub fn merge_next(
        &mut self,
        ticket: Ticket,
        key: &Key,
        cursor: &str,
        page: TreePage,
        out: &mut Vec<LibraryEvent>,
    ) -> Option<Listed> {
        if ticket.generation != self.generation {
            return None;
        }
        let folder = self.folders.get_mut(key)?;
        if folder.epoch != ticket.epoch {
            return self.listed(key);
        }
        let before = folder.state(&key.path);
        folder.loading = folder.loading.saturating_sub(1);
        if folder.next.as_deref() == Some(cursor) {
            // A page read before the events the folder took: they are read again.
            folder.seq = folder.seq.min(page.seq);
            if let Some(last) = page.entries.last() {
                folder.bound = Some(last.name.clone());
            }
            folder.next = page.next;
            for entry in page.entries {
                if !key.folders_only || entry.kind == EntryKind::Folder {
                    folder.put(entry);
                }
            }
        }
        self.announce(key, before, out);
        self.listed(key)
    }

    /// A page of `key` failed: the folder shows `text`.
    pub fn failed(&mut self, ticket: Ticket, key: &Key, text: &str, out: &mut Vec<LibraryEvent>) {
        if ticket.generation != self.generation {
            return;
        }
        let Some(folder) = self.folders.get_mut(key) else {
            return;
        };
        if folder.epoch != ticket.epoch {
            return;
        }
        let before = folder.state(&key.path);
        folder.loading = folder.loading.saturating_sub(1);
        folder.error = Some(text.to_owned());
        self.announce(key, before, out);
    }

    /// Drops `key`; its next page or event changes nothing.
    pub fn drop_folder(&mut self, generation: u64, key: &Key) {
        if generation == self.generation {
            self.folders.remove(key);
        }
    }

    /// Listing `key` again failed with `text`. A folder that is still listed is kept as it
    /// was, with the error (its old `seq` makes the follower meet a gap again, or follow on).
    /// A dropped one waits to be tried again and the window hears the error.
    pub fn relist_failed(
        &mut self,
        generation: u64,
        key: &Key,
        text: &str,
        out: &mut Vec<LibraryEvent>,
    ) {
        if generation != self.generation {
            return;
        }
        match self.folders.get_mut(key) {
            Some(folder) => {
                let before = folder.state(&key.path);
                folder.error = Some(text.to_owned());
                self.announce(key, before, out);
            }
            None => {
                self.pending.insert(key.clone());
                out.push(gone_event(key, text));
            }
        }
    }

    /// Tells the window that `key` is not there anymore, or could not be listed.
    pub fn gone(&mut self, generation: u64, key: &Key, text: &str, out: &mut Vec<LibraryEvent>) {
        if generation != self.generation {
            return;
        }
        self.folders.remove(key);
        self.pending.remove(key);
        out.push(gone_event(key, text));
    }

    /// Applies a page of events read from `from` in order. It covers only the folders that
    /// were valid at `from - 1` or later; a folder listed at a lower `seq` while the page was
    /// read is left alone and read up from its own `seq` next. A covered folder takes only the
    /// events past its `seq` and is valid at the end of the page afterwards.
    pub fn apply(
        &mut self,
        generation: u64,
        from: u64,
        page: &EventPage,
        out: &mut Vec<LibraryEvent>,
    ) -> Applied {
        let end = page.next.saturating_sub(1);
        let mut applied = Applied {
            end,
            ..Applied::default()
        };
        if generation != self.generation {
            return applied;
        }
        let before: HashMap<Key, FolderState> = self
            .folders
            .iter()
            .map(|(k, f)| (k.clone(), f.state(&k.path)))
            .collect();
        for folder in self.folders.values_mut() {
            folder.covered = folder.seq.saturating_add(1) >= from;
        }
        for event in &page.events {
            if event.kind == "account.removed" {
                applied.stop = true;
                break;
            }
            self.apply_one(event, &mut applied);
        }
        for folder in self.folders.values_mut() {
            if std::mem::take(&mut folder.covered) {
                folder.seq = folder.seq.max(end);
            }
        }
        // Emitted in a fixed order, so a test can compare them.
        let mut keys: Vec<&Key> = before.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(folder) = self.folders.get(key)
                && folder.state(&key.path) != before[key]
            {
                out.push(self.event(key));
            }
        }
        let mut added: Vec<&Key> = self
            .folders
            .keys()
            .filter(|k| !before.contains_key(*k))
            .collect();
        added.sort();
        for key in added {
            out.push(self.event(key));
        }
        for key in &applied.gone {
            if !self.folders.contains_key(key) {
                out.push(gone_event(key, GONE));
            }
        }
        self.pending.extend(applied.relist.iter().cloned());
        applied.wanted.retain({
            let mut seen = std::collections::HashSet::new();
            move |(id, _)| seen.insert(*id)
        });
        applied
    }

    fn apply_one(&mut self, event: &Event, applied: &mut Applied) {
        let (Some(id), Some(path)) = (event.id, event.path.as_deref()) else {
            return;
        };
        let seq = event.seq;
        match event.kind.as_str() {
            "file.created" | "folder.created" | "file.copied" => {
                let kind = if event.kind == "folder.created" {
                    EntryKind::Folder
                } else {
                    EntryKind::File
                };
                let known = self.known(id).map(|mut e| {
                    e.path = path.to_owned();
                    e.name = name_of(path).to_owned();
                    e.kind = kind;
                    if kind == EntryKind::File {
                        e.md5 = event.md5.clone().or(e.md5);
                        e.size = event.size.or(e.size);
                    }
                    e
                });
                // A copy has the md5 and size of its source; the fetch brings them.
                let known = if event.kind == "file.copied" {
                    None
                } else {
                    known
                };
                self.set_entry(seq, id, path, kind, known, applied);
            }
            "file.modified" => {
                for folder in self
                    .folders
                    .values_mut()
                    .filter(|f| f.covered && f.seq < seq)
                {
                    if let Some(at) = folder.position(id) {
                        let entry = &mut folder.entries[at];
                        entry.md5 = event.md5.clone();
                        entry.size = event.size;
                        // The new mtime comes with the entry.
                        applied.wanted.push((id, seq));
                    }
                }
            }
            "file.moved" | "folder.moved" => {
                let kind = if event.kind == "folder.moved" {
                    EntryKind::Folder
                } else {
                    EntryKind::File
                };
                if kind == EntryKind::Folder
                    && let Some(old) = event.old_path.as_deref()
                {
                    self.move_listed(seq, old, path, applied);
                }
                let known = self.known(id).map(|mut e| {
                    e.path = path.to_owned();
                    e.name = name_of(path).to_owned();
                    e
                });
                self.set_entry(seq, id, path, kind, known, applied);
            }
            "file.deleted" | "folder.deleted" => {
                for folder in self
                    .folders
                    .values_mut()
                    .filter(|f| f.covered && f.seq < seq)
                {
                    folder.remove(id);
                }
                if event.kind == "folder.deleted" {
                    let mut hit: Vec<Key> = self
                        .folders
                        .iter()
                        .filter(|(k, f)| f.covered && f.seq < seq && at_or_below(&k.path, path))
                        .map(|(k, _)| k.clone())
                        .collect();
                    hit.sort();
                    for key in hit {
                        self.folders.remove(&key);
                        applied.relist.push(key);
                    }
                }
            }
            _ => {}
        }
    }

    /// The entry `id` is at `path` now: it leaves every other folder and comes into the folder
    /// of `path` when that is listed and the name belongs to its loaded pages.
    fn set_entry(
        &mut self,
        seq: u64,
        id: u64,
        path: &str,
        kind: EntryKind,
        known: Option<Entry>,
        applied: &mut Applied,
    ) {
        let parent = parent_of(path);
        let name = name_of(path);
        for (key, folder) in self
            .folders
            .iter_mut()
            .filter(|(_, f)| f.covered && f.seq < seq)
        {
            let here = key.path == parent && (!key.folders_only || kind == EntryKind::Folder);
            if !here || !folder.admits(name) {
                folder.remove(id);
                continue;
            }
            match &known {
                Some(entry) => folder.put(entry.clone()),
                None => {
                    // A different entry at this path is replaced; the fetch brings the new one.
                    folder.entries.retain(|e| e.id != id && e.name != name);
                    applied.wanted.push((id, seq));
                }
            }
        }
    }

    /// A folder moved from `old` to `new`: listed folders at or below it move along when they
    /// are complete; one that still pages is dropped and listed again at its new path. The
    /// window hears that the old path is gone.
    fn move_listed(&mut self, seq: u64, old: &str, new: &str, applied: &mut Applied) {
        if old.is_empty() {
            return;
        }
        let mut hit: Vec<Key> = self
            .folders
            .iter()
            .filter(|(k, f)| f.covered && f.seq < seq && at_or_below(&k.path, old))
            .map(|(k, _)| k.clone())
            .collect();
        hit.sort();
        for key in hit {
            let Some(mut folder) = self.folders.remove(&key) else {
                continue;
            };
            let moved = Key {
                path: rebase(&key.path, old, new),
                folders_only: key.folders_only,
            };
            if folder.complete() {
                for entry in &mut folder.entries {
                    entry.path = rebase(&entry.path, old, new);
                }
                self.folders.insert(moved, folder);
            } else {
                applied.relist.push(moved);
            }
            applied.gone.push(key);
        }
    }

    /// Puts entries fetched after a batch into the listings of their folder that are valid
    /// between the entry's event and the end of the batch: the batch covered them, or a page
    /// read after the event came in meanwhile. A listing below the event reads it up again; one
    /// past the batch holds the entry as it is.
    pub fn insert_fetched(
        &mut self,
        generation: u64,
        end: u64,
        entries: Vec<(Entry, u64)>,
        out: &mut Vec<LibraryEvent>,
    ) {
        if generation != self.generation {
            return;
        }
        let mut before: Vec<(Key, FolderState)> = Vec::new();
        for (entry, seq) in entries {
            let parent = parent_of(&entry.path).to_owned();
            for (key, folder) in self.folders.iter_mut().filter(|(k, f)| {
                k.path == parent
                    && f.seq >= seq
                    && f.seq <= end
                    && (!k.folders_only || entry.kind == EntryKind::Folder)
                    && f.admits(&entry.name)
            }) {
                if !before.iter().any(|(k, _)| k == key) {
                    before.push((key.clone(), folder.state(&key.path)));
                }
                folder.put(entry.clone());
            }
        }
        before.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, state) in before {
            self.announce(&key, state, out);
        }
    }

    fn event(&self, key: &Key) -> LibraryEvent {
        LibraryEvent::Folder {
            folder: self.folders[key].state(&key.path),
        }
    }

    /// Sends the folder when it changed.
    fn announce(&self, key: &Key, before: FolderState, out: &mut Vec<LibraryEvent>) {
        if let Some(folder) = self.folders.get(key)
            && folder.state(&key.path) != before
        {
            out.push(self.event(key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: u64, path: &str) -> Entry {
        Entry {
            id,
            path: path.to_owned(),
            name: name_of(path).to_owned(),
            kind: EntryKind::File,
            size: Some(1),
            md5: None,
            mtime_ms: 0,
            writable: true,
        }
    }

    fn page(seq: u64, entries: Vec<Entry>, next: Option<&str>) -> TreePage {
        TreePage {
            seq,
            entries,
            next: next.map(str::to_owned),
        }
    }

    fn deleted(seq: u64, id: u64, path: &str) -> Event {
        Event {
            seq,
            kind: "file.deleted".to_owned(),
            id: Some(id),
            path: Some(path.to_owned()),
            old_path: None,
            md5: None,
            size: None,
        }
    }

    fn batch(events: Vec<Event>, next: u64) -> EventPage {
        EventPage {
            events,
            next,
            more: false,
            gap: false,
        }
    }

    fn names(cache: &Cache, key: &Key) -> Vec<String> {
        let listed = cache.listed(key).unwrap();
        listed.folder.entries.into_iter().map(|e| e.name).collect()
    }

    /// A page merged while the follower waits for a batch it asked from a higher `seq`: the
    /// batch does not cover that folder, which is read up from its own `seq` next.
    #[test]
    fn a_page_merged_during_a_poll_is_read_up_from_its_own_seq() {
        let mut cache = Cache::default();
        cache.clear(1);
        let out = &mut Vec::new();
        let a = Key::new("A", false);
        let b = Key::new("B", false);
        let c = Key::new("C", false);
        let t = cache.begin_first(1, &a);
        cache.merge_first(t, &a, page(25, vec![file(1, "A/a")], None), out);
        let t = cache.begin_first(1, &c);
        cache.merge_first(t, &c, page(10, vec![file(3, "C/c")], Some("t1:c")), out);
        cache.apply(1, 11, &batch(vec![], 26), out);
        assert_eq!(cache.min_seq(), Some(25));
        // The follower asks from 26. Meanwhile B is listed at 15 and page 2 of C (read at
        // 20) comes in.
        let t = cache.begin_first(1, &b);
        cache.merge_first(t, &b, page(15, vec![file(2, "B/x")], None), out);
        let Continue::Fetch(t) = cache.begin_next(1, &c, "t1:c", out) else {
            panic!("no fetch");
        };
        cache.merge_next(t, &c, "t1:c", page(20, vec![file(4, "C/x")], None), out);
        cache.apply(1, 26, &batch(vec![], 31), out);
        assert_eq!(cache.min_seq(), Some(15));
        assert_eq!(cache.folders[&b].seq, 15);
        assert_eq!(cache.folders[&c].seq, 20);
        assert_eq!(cache.folders[&a].seq, 30);
        // Read up from 16: the deletes at 23 reach B and C, A is past them.
        let applied = cache.apply(
            1,
            16,
            &batch(vec![deleted(23, 2, "B/x"), deleted(23, 4, "C/x")], 31),
            out,
        );
        assert_eq!(names(&cache, &b), Vec::<String>::new());
        assert_eq!(names(&cache, &c), ["c"]);
        assert_eq!(cache.min_seq(), Some(30));
        // A fetched entry goes only into folders the batch covered.
        let t = cache.begin_first(1, &b);
        cache.merge_first(t, &b, page(12, vec![], None), out);
        cache.insert_fetched(
            1,
            applied.end,
            vec![(file(5, "B/y"), 23), (file(6, "A/y"), 23)],
            out,
        );
        assert_eq!(names(&cache, &b), Vec::<String>::new());
        assert_eq!(names(&cache, &a), ["a", "y"]);
    }

    /// A page merged while the entries of a batch are fetched: the entry still comes into
    /// that folder when the page was read at or after the event.
    #[test]
    fn a_page_merged_during_a_fetch_keeps_the_fetched_entry() {
        let mut cache = Cache::default();
        cache.clear(1);
        let out = &mut Vec::new();
        let c = Key::new("C", false);
        let t = cache.begin_first(1, &c);
        cache.merge_first(t, &c, page(10, vec![file(3, "C/c")], Some("t1:c")), out);
        let Continue::Fetch(t) = cache.begin_next(1, &c, "t1:c", out) else {
            panic!("no fetch");
        };
        let created = Event {
            seq: 12,
            kind: "file.created".to_owned(),
            id: Some(5),
            path: Some("C/b".to_owned()),
            old_path: None,
            md5: None,
            size: None,
        };
        let applied = cache.apply(1, 11, &batch(vec![created], 21), out);
        assert_eq!(applied.wanted, [(5, 12)]);
        // Page 2 was read at 15: after the create, before the end of the batch.
        cache.merge_next(t, &c, "t1:c", page(15, vec![file(4, "C/x")], None), out);
        cache.insert_fetched(1, applied.end, vec![(file(5, "C/b"), 12)], out);
        assert_eq!(names(&cache, &c), ["b", "c", "x"]);
    }

    #[test]
    fn above_means_a_whole_name() {
        assert!(at_or_below("a/x", "a"));
        assert!(at_or_below("a", "a"));
        assert!(!at_or_below("a b", "a"));
        assert!(!at_or_below("ab/x", "a"));
        assert_eq!(parent_of("NOTE/Note/a.note"), "NOTE/Note");
        assert_eq!(parent_of("NOTE"), "");
        assert_eq!(name_of("NOTE/Note/a.note"), "a.note");
        assert_eq!(Key::new("/NOTE/Note/", false).path, "NOTE/Note");
    }
}
