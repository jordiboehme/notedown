//! The notes of the account: a small index of the pipeline's store, All notes and New notes.
//!
//! The pipeline keeps one `notes/<key>.json` per note in `<data_dir>/pipeline/<account>/` (in
//! Local and in Remote mode it runs on this Mac) and writes each file atomically. The index
//! reads a summary of each file with a struct that leaves out the strokes and the device's
//! recognition text, and reads a file again only when its mtime, size or inode changed. When
//! the data folder or the account changes, the index starts over.
//!
//! A note is new when it has a page with text, is not deleted or unscoped, and the highest
//! `seq` of its pages (the seq of its newest text) is above the seen marker of its key. The
//! markers live in `<app config dir>/seen.json` as `{"<account>": {"<key>": <seq>}}`. When
//! the first scan of an account finds no markers for it, every note with text gets one at its
//! current seq (the baseline); notes still waiting for their text become new when it arrives.
//! The app wakes a scan at each binding of the library, so this first scan comes at the login.
//!
//! The index follows the store by polling: a scan every [`NOTES_EVERY`] while the panel or the
//! library window is visible, one at each show, and one before every answer of `notes_new` and
//! `notes_all`. A scan is a `stat` per file; only changed files are read. The app has no file
//! watcher, and a watcher would add a dependency for a folder that matters only while a window
//! shows the notes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// The event that tells the panel and the library window that the notes changed. It carries
/// nothing: the windows ask `notes_new` or `notes_all` again.
pub const NOTES_EVENT: &str = "notes";

/// How often the store is scanned while a window that shows notes is visible.
pub const NOTES_EVERY: Duration = Duration::from_secs(2);

/// The seen markers, in the app config dir.
const SEEN_FILE: &str = "seen.json";
const SEEN_TEMP: &str = "seen.json.tmp";
const SEEN_BROKEN: &str = "seen.json.broken";

/// What `note_seen` says for a key that is not in the index.
pub const NOTE_GONE: &str = "This note is not there anymore.";
/// What `note_seen` says when the markers could not be saved.
pub const SEEN_NOT_SAVED: &str = "Notedown could not save which notes you have seen.";

/// One note as the panel and the library list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteSummary {
    pub key: String,
    /// The wire path, as the library's entries have it.
    pub path: String,
    pub title: String,
    pub pages: usize,
    pub pages_with_text: usize,
    /// The mtime of the note's store file, in ms since the epoch: the store keeps no time of
    /// its own per note.
    pub changed_ms: i64,
}

/// The data folder and the account in the config, as they are now.
pub type NotesPlace = Arc<dyn Fn() -> (PathBuf, Option<String>) + Send + Sync>;

/// Tells the windows that the notes changed. It must not block: the app only puts it into a
/// channel.
pub type NotesEmit = Arc<dyn Fn() + Send + Sync>;

/// The part of a store file the index reads. `strokeList`, `meta` (with the device's
/// recognition text), the blocks' text and everything else is skipped while parsing.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNote {
    key: String,
    path: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    pages: Vec<StoredPage>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    unscoped: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPage {
    #[serde(default)]
    page_id: String,
    #[serde(default)]
    seq: Option<u64>,
    /// Only counted: a page with blocks has text, as `pipeline status` counts it.
    #[serde(default)]
    blocks: Vec<IgnoredAny>,
}

/// A note of the index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Note {
    summary: NoteSummary,
    /// The highest `seq` of the pages: the seq of the note's newest text.
    text_seq: u64,
    deleted: bool,
    unscoped: bool,
    /// The page ids with their seq, in page order. A path-keyed note keeps them on a move.
    pages: Vec<(String, Option<u64>)>,
}

impl Note {
    fn listed(&self) -> bool {
        self.summary.pages_with_text > 0 && !self.deleted && !self.unscoped
    }

    fn live(&self) -> bool {
        !self.deleted && !self.unscoped
    }
}

/// What a file looked like when it was read: a write of the pipeline is a rename, so the inode
/// changes even when the mtime does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime_ns: i128,
    len: u64,
    ino: u64,
}

struct Indexed {
    stamp: Stamp,
    /// `None`: the file did not parse. Its key still counts as there, so its marker stays.
    note: Option<Note>,
}

type Seen = BTreeMap<String, BTreeMap<String, u64>>;

struct State {
    /// The data folder and the account the index is of; `None` without an account.
    place: Option<(PathBuf, String)>,
    /// By file name.
    files: BTreeMap<String, Indexed>,
    /// Loaded at the first use.
    seen: Option<Seen>,
    /// The markers changed and are not saved yet.
    dirty: bool,
    /// What All notes and New notes were when the windows were last told.
    told: Option<(Vec<NoteSummary>, Vec<NoteSummary>)>,
}

/// The notes index. Cheap to clone; every clone is the same index.
#[derive(Clone)]
pub struct Notes {
    inner: Arc<Inner>,
}

struct Inner {
    place: NotesPlace,
    config_dir: PathBuf,
    emit: NotesEmit,
    /// The index, the markers and what the windows were told. Scans and marks run whole under
    /// it; no call to anything but the file system is made under it.
    state: Mutex<State>,
    /// A window that shows notes is visible.
    visible: Mutex<bool>,
    /// Wakes the scanner's thread, once it runs.
    waker: Mutex<Option<Sender<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Notes {
    /// The index of the account `place` names, with the markers in `config_dir`.
    pub fn new(place: NotesPlace, config_dir: PathBuf, emit: NotesEmit) -> Self {
        Self {
            inner: Arc::new(Inner {
                place,
                config_dir,
                emit,
                state: Mutex::new(State {
                    place: None,
                    files: BTreeMap::new(),
                    seen: None,
                    dirty: false,
                    told: None,
                }),
                visible: Mutex::new(false),
                waker: Mutex::new(None),
            }),
        }
    }

    /// Scans the store and tells the windows when All notes or New notes changed. Blocking:
    /// run it off the main thread.
    pub fn refresh(&self) {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        self.save_seen(&mut state);
        self.tell(&mut state);
    }

    /// Every note with text that is not deleted or unscoped, newest first. Scans first.
    pub fn all(&self) -> Vec<NoteSummary> {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        self.save_seen(&mut state);
        self.tell(&mut state);
        listed(&state)
    }

    /// The notes of [`all`](Self::all) with text the user has not seen. Scans first.
    pub fn new_notes(&self) -> Vec<NoteSummary> {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        self.save_seen(&mut state);
        self.tell(&mut state);
        new_of(&state)
    }

    /// The user opened the note `key`: its text up to now is seen. Scans first.
    pub fn seen(&self, key: &str) -> Result<(), String> {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        let found = state
            .files
            .values()
            .filter_map(|f| f.note.as_ref())
            .find(|n| n.summary.key == key)
            .map(|n| n.text_seq);
        let account = state.place.as_ref().map(|(_, a)| a.clone());
        let result = match (found, account) {
            (Some(seq), Some(account)) => {
                let marks = self.seen_map(&mut state).entry(account).or_default();
                if marks.get(key) != Some(&seq) {
                    marks.insert(key.to_owned(), seq);
                    state.dirty = true;
                }
                if self.save_seen(&mut state) {
                    Ok(())
                } else {
                    Err(SEEN_NOT_SAVED.to_owned())
                }
            }
            _ => {
                self.save_seen(&mut state);
                Err(NOTE_GONE.to_owned())
            }
        };
        self.tell(&mut state);
        result
    }

    /// The note at the wire path `path`: the state that is not deleted or unscoped wins over
    /// one that is. Uses the index as it is.
    pub fn at_path(&self, path: &str) -> Option<NoteSummary> {
        let state = lock(&self.inner.state);
        by_path(&state).remove(path).map(|n| n.summary.clone())
    }

    /// A window that shows notes (the panel, the library) was shown or hidden. It never
    /// blocks: the scanner's thread does the work.
    pub fn set_visible(&self, visible: bool) {
        let was = std::mem::replace(&mut *lock(&self.inner.visible), visible);
        if visible && !was {
            self.wake();
        }
    }

    /// Asks the scanner's thread for a scan, also while no window shows notes: the app calls it
    /// at each binding of the library (the start, a login, a log out, a new place), so the
    /// baseline of a new account is set at its login.
    pub fn wake(&self) {
        if let Some(waker) = lock(&self.inner.waker).as_ref() {
            let _ = waker.send(());
        }
    }

    /// Starts the scanner's thread: a scan every `every` while a window that shows notes is
    /// visible, and one at each show. It holds only a weak handle, so it ends when the index
    /// is dropped.
    pub fn start_scanning(&self, every: Duration) -> std::thread::JoinHandle<()> {
        let (tx, rx) = channel::<()>();
        *lock(&self.inner.waker) = Some(tx);
        let weak = Arc::downgrade(&self.inner);
        std::thread::spawn(move || scan_loop(&weak, &rx, every))
    }

    fn seen_map<'a>(&self, state: &'a mut State) -> &'a mut Seen {
        state
            .seen
            .get_or_insert_with(|| load_seen(&self.inner.config_dir))
    }

    /// Brings the index up to the store, then sets the baseline, carries markers over and
    /// prunes them.
    fn scan(&self, state: &mut State) {
        let (data_dir, account) = (self.inner.place)();
        let place = account
            .filter(|a| safe_component(a))
            .map(|account| (data_dir, account));
        if state.place != place {
            state.place = place.clone();
            state.files.clear();
        }
        let Some((data_dir, account)) = place else {
            return;
        };
        let dir = data_dir.join("pipeline").join(&account).join("notes");
        // A folder that cannot be read keeps the index as it is, and prunes nothing. In a data
        // folder that is there, a store the pipeline has not made yet is a new account: its
        // baseline is empty. A data folder that is not there (a disk that is not mounted) sets
        // none.
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound && data_dir.is_dir() {
                    let seen = self.seen_map(state);
                    let fresh = !seen.contains_key(&account);
                    seen.entry(account).or_default();
                    if fresh {
                        state.dirty = true;
                    }
                }
                return;
            }
        };
        let mut old = std::mem::take(&mut state.files);
        let mut files = BTreeMap::new();
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // The pipeline's temp files are `.<name>.<pid>.<n>.tmp`.
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let stamp = stamp_of(&meta);
            let indexed = match old.remove(name) {
                Some(known) if known.stamp == stamp => known,
                _ => Indexed {
                    stamp,
                    note: read_note(&entry.path(), stamp),
                },
            };
            files.insert(name.to_owned(), indexed);
        }
        let gone: Vec<Note> = old.into_values().filter_map(|f| f.note).collect();
        state.files = files;
        self.mark(state, &account, &gone);
    }

    /// The baseline of a new account, the markers of moved path-keyed notes, and pruning.
    fn mark(&self, state: &mut State, account: &str, gone: &[Note]) {
        let mut present: BTreeSet<String> = BTreeSet::new();
        let mut notes: Vec<Note> = Vec::new();
        for (name, file) in &state.files {
            present.insert(key_of_file(name));
            if let Some(note) = &file.note {
                present.insert(note.summary.key.clone());
                notes.push(note.clone());
            }
        }
        let seen = self.seen_map(state);
        let mut changed = false;
        match seen.get_mut(account) {
            None => {
                let baseline = notes
                    .iter()
                    .filter(|n| n.summary.pages_with_text > 0)
                    .map(|n| (n.summary.key.clone(), n.text_seq))
                    .collect();
                seen.insert(account.to_owned(), baseline);
                changed = true;
            }
            Some(marks) => {
                let old_keys: Vec<String> = marks
                    .keys()
                    .filter(|k| !present.contains(*k))
                    .cloned()
                    .collect();
                for old in old_keys {
                    let value = marks.remove(&old).unwrap_or_default();
                    changed = true;
                    if !old.starts_with("path:") {
                        continue;
                    }
                    let before = gone.iter().find(|n| n.summary.key == old);
                    let candidates: Vec<&Note> = notes
                        .iter()
                        .filter(|n| {
                            n.summary.key.starts_with("path:")
                                && !marks.contains_key(&n.summary.key)
                        })
                        .filter(|n| match before {
                            // The same pages with the same text versions.
                            Some(before) => !before.pages.is_empty() && n.pages == before.pages,
                            // Not indexed before (a move while the app was not running): the
                            // seq of a text version belongs to one note only.
                            None => value > 0 && n.text_seq == value,
                        })
                        .collect();
                    if let [moved] = candidates.as_slice() {
                        marks.insert(moved.summary.key.clone(), value);
                    }
                }
            }
        }
        if changed {
            state.dirty = true;
        }
    }

    /// Saves the markers when they changed. False when the save failed; it is tried again at
    /// the next change or scan.
    fn save_seen(&self, state: &mut State) -> bool {
        if !state.dirty {
            return true;
        }
        let Some(seen) = &state.seen else {
            return true;
        };
        match write_seen(&self.inner.config_dir, seen) {
            Ok(()) => {
                state.dirty = false;
                true
            }
            Err(e) => {
                eprintln!("notedown: the seen notes could not be saved: {e}");
                false
            }
        }
    }

    /// Tells the windows when All notes or New notes is not what they were told last.
    fn tell(&self, state: &mut State) {
        let now = (listed(state), new_of(state));
        if state.told.as_ref() != Some(&now) {
            state.told = Some(now);
            (self.inner.emit)();
        }
    }
}

fn scan_loop(weak: &Weak<Inner>, rx: &Receiver<()>, every: Duration) {
    loop {
        let visible = match weak.upgrade() {
            Some(inner) => *lock(&inner.visible),
            None => return,
        };
        let woke = if visible {
            rx.recv_timeout(every)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        if woke == Err(RecvTimeoutError::Disconnected) {
            return;
        }
        // Wakes that piled up during a scan make one scan.
        while rx.try_recv().is_ok() {}
        let Some(inner) = weak.upgrade() else {
            return;
        };
        Notes { inner }.refresh();
    }
}

/// For each path the note that wins: one that is not deleted or unscoped over one that is,
/// then the newer file.
fn by_path(state: &State) -> HashMap<&str, &Note> {
    let mut paths: HashMap<&str, &Note> = HashMap::new();
    for note in state.files.values().filter_map(|f| f.note.as_ref()) {
        let path = note.summary.path.as_str();
        match paths.get(path) {
            Some(held)
                if (held.live(), held.summary.changed_ms)
                    >= (note.live(), note.summary.changed_ms) => {}
            _ => {
                paths.insert(path, note);
            }
        }
    }
    paths
}

fn listed_notes(state: &State) -> Vec<&Note> {
    let mut notes: Vec<&Note> = by_path(state)
        .into_values()
        .filter(|n| n.listed())
        .collect();
    notes.sort_by(|a, b| {
        b.summary
            .changed_ms
            .cmp(&a.summary.changed_ms)
            .then_with(|| a.summary.path.cmp(&b.summary.path))
    });
    notes
}

fn listed(state: &State) -> Vec<NoteSummary> {
    listed_notes(state)
        .into_iter()
        .map(|n| n.summary.clone())
        .collect()
}

/// New notes: none before the baseline of the account is set.
fn new_of(state: &State) -> Vec<NoteSummary> {
    let Some(marks) = state
        .place
        .as_ref()
        .and_then(|(_, account)| state.seen.as_ref()?.get(account))
    else {
        return Vec::new();
    };
    listed_notes(state)
        .into_iter()
        .filter(|n| n.text_seq > marks.get(&n.summary.key).copied().unwrap_or(0))
        .map(|n| n.summary.clone())
        .collect()
}

fn stamp_of(meta: &fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp {
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        len: meta.len(),
        ino: meta.ino(),
    }
}

/// The summary of one store file; `None` when it cannot be read or parsed.
fn read_note(path: &Path, stamp: Stamp) -> Option<Note> {
    let bytes = fs::read(path).ok()?;
    let stored: StoredNote = serde_json::from_slice(&bytes).ok()?;
    let pages_with_text = stored.pages.iter().filter(|p| !p.blocks.is_empty()).count();
    let text_seq = stored.pages.iter().filter_map(|p| p.seq).max().unwrap_or(0);
    let changed_ms = i64::try_from(stamp.mtime_ns.div_euclid(1_000_000)).unwrap_or(0);
    let title = if stored.title.is_empty() {
        title_of(&stored.path)
    } else {
        stored.title
    };
    Some(Note {
        summary: NoteSummary {
            key: stored.key,
            path: stored.path,
            title,
            pages: stored.pages.len(),
            pages_with_text,
            changed_ms,
        },
        text_seq,
        deleted: stored.deleted,
        unscoped: stored.unscoped,
        pages: stored
            .pages
            .into_iter()
            .map(|p| (p.page_id, p.seq))
            .collect(),
    })
}

/// The file name without `.note`, as the pipeline names a note.
fn title_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".note").unwrap_or(name).to_owned()
}

/// The key of a store file by its name: `path-<md5>.json` is `path:<md5>`.
fn key_of_file(name: &str) -> String {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    match stem.strip_prefix("path-") {
        Some(md5) => format!("path:{md5}"),
        None => stem.to_owned(),
    }
}

/// An account slug that is one plain path component.
fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name.len() <= 255
}

/// The markers in `dir`; none when there is no file. A file that does not parse is kept as
/// `seen.json.broken` and the markers start empty (the next scan sets a new baseline).
fn load_seen(dir: &Path) -> Seen {
    let path = dir.join(SEEN_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            eprintln!("notedown: {} does not parse: {e}", path.display());
            let _ = fs::rename(&path, dir.join(SEEN_BROKEN));
            Seen::new()
        }),
        Err(_) => Seen::new(),
    }
}

/// Writes the markers durably: a temp file flushed to disk, then renamed over the old one.
fn write_seen(dir: &Path, seen: &Seen) -> io::Result<()> {
    use std::io::Write;
    fs::create_dir_all(dir)?;
    let temp = dir.join(SEEN_TEMP);
    let json = serde_json::to_vec(seen).map_err(io::Error::other)?;
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(SEEN_FILE))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

#[cfg(test)]
mod tests;
