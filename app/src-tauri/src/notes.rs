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
//! markers belong to the account and its data folder: `<app config dir>/seen.json` holds
//! `{"<account>": [{"dataDir": "<folder>", "seen": {"<key>": <seq>}}]}`, the folder used last
//! first, at most [`FOLDERS_KEPT`] per account. When the first scan of a store (an existing
//! `<data_dir>/pipeline/<account>/`) finds no markers for it, every note with text gets one at
//! its current seq (the baseline); notes still waiting for their text become new when it
//! arrives. The app wakes a scan at each binding of the library, so this first scan comes at
//! the login. A note that comes back (from the bin, into the scope) while the app runs is not
//! new for text the user had seen.
//!
//! The index follows the store by polling: a scan every [`NOTES_EVERY`] while the panel or the
//! library window is visible, one at each show, and one before every answer of `notes_new` and
//! `notes_all`. A scan is a `stat` per file; only changed files are read. The windows hear
//! [`NOTES_EVENT`] when All notes or New notes changed and when a scan read a store file again
//! (or found one gone), so an open review hears about every page the OCR wrote. The app has no file
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
/// [`NotesChanged`]: the windows ask `notes_new` or `notes_all` again, and an open review reads
/// its note again when its path is named.
pub const NOTES_EVENT: &str = "notes";

/// What [`NOTES_EVENT`] carries: the wire paths of the notes whose store file a scan read again
/// or found gone since the last event (a moved note with its old and its new path), sorted.
/// Empty when only All notes or New notes changed (a note marked seen).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotesChanged {
    pub paths: Vec<String>,
}

/// How often the store is scanned while a window that shows notes is visible.
pub const NOTES_EVERY: Duration = Duration::from_secs(2);

/// The seen markers, in the app config dir.
const SEEN_FILE: &str = "seen.json";
const SEEN_TEMP: &str = "seen.json.tmp";
const SEEN_BROKEN: &str = "seen.json.broken";

/// What `note_seen` says for a key that is not in the index.
pub const NOTE_GONE: &str = "This note is not there anymore.";
/// What `note_seen` says while the markers cannot be read.
pub const SEEN_NOT_READ: &str = "Notedown could not read which notes you have seen.";
/// How many data folders of one account keep their markers.
pub const FOLDERS_KEPT: usize = 3;

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
pub type NotesEmit = Arc<dyn Fn(NotesChanged) + Send + Sync>;

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

/// The markers of one account in one data folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Folder {
    data_dir: PathBuf,
    seen: BTreeMap<String, u64>,
}

/// Per account its folders, the one used last first.
type Seen = BTreeMap<String, Vec<Folder>>;

fn marks_of<'a>(seen: &'a Seen, account: &str, dir: &Path) -> Option<&'a BTreeMap<String, u64>> {
    seen.get(account)?
        .iter()
        .find(|f| f.data_dir == dir)
        .map(|f| &f.seen)
}

fn marks_mut<'a>(
    seen: &'a mut Seen,
    account: &str,
    dir: &Path,
) -> Option<&'a mut BTreeMap<String, u64>> {
    seen.get_mut(account)?
        .iter_mut()
        .find(|f| f.data_dir == dir)
        .map(|f| &mut f.seen)
}

/// Puts the folder `dir` of `account` first. True when the order changed.
fn use_folder(seen: &mut Seen, account: &str, dir: &Path) -> bool {
    let Some(folders) = seen.get_mut(account) else {
        return false;
    };
    match folders.iter().position(|f| f.data_dir == dir) {
        Some(at) if at > 0 => {
            let folder = folders.remove(at);
            folders.insert(0, folder);
            true
        }
        _ => false,
    }
}

/// The baseline of `account` in `dir`: first, and the folder used longest ago goes past
/// [`FOLDERS_KEPT`].
fn add_folder(seen: &mut Seen, account: &str, dir: &Path, marks: BTreeMap<String, u64>) {
    let folders = seen.entry(account.to_owned()).or_default();
    folders.insert(
        0,
        Folder {
            data_dir: dir.to_path_buf(),
            seen: marks,
        },
    );
    folders.truncate(FOLDERS_KEPT);
}

struct State {
    /// The data folder and the account the index is of; `None` without an account.
    place: Option<(PathBuf, String)>,
    /// By file name.
    files: BTreeMap<String, Indexed>,
    /// Loaded at the first use; `None` also while seen.json cannot be read (nothing is written
    /// then, and every scan tries to load it again).
    seen: Option<Seen>,
    /// The markers changed and are not saved yet.
    dirty: bool,
    /// What All notes and New notes were when the windows were last told.
    told: Option<(Vec<NoteSummary>, Vec<NoteSummary>)>,
    /// The paths of the notes whose store file a scan read again (or found gone) since the
    /// windows were last told: an open review hears about them, also when the lists stay as
    /// they are.
    reread: BTreeSet<String>,
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
    /// it, with their file reads and writes; the emit made under it only sends into a channel.
    state: Mutex<State>,
    /// A window that shows notes is visible.
    visible: Mutex<bool>,
    /// Wakes the scanner's thread, once it runs.
    waker: Mutex<Option<Sender<()>>>,
    /// How many times seen.json was written, for the tests.
    #[cfg(test)]
    saves: std::sync::atomic::AtomicUsize,
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
                    reread: BTreeSet::new(),
                }),
                visible: Mutex::new(false),
                waker: Mutex::new(None),
                #[cfg(test)]
                saves: std::sync::atomic::AtomicUsize::new(0),
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

    /// How many notes of [`all`](Self::all) have text with a seq above `seq`, and the newest
    /// text seq of them all (`None` without notes). The seen markers do not matter. Scans
    /// first, so it takes the index lock and may tell the windows: the notifier calls it on
    /// its own thread, and only when an OCR pass is done.
    pub fn notes_newer_than(&self, seq: u64) -> (u64, Option<u64>) {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        self.save_seen(&mut state);
        self.tell(&mut state);
        let notes = listed_notes(&state);
        let newer = notes.iter().filter(|n| n.text_seq > seq).count();
        let newest = notes.iter().map(|n| n.text_seq).max();
        (u64::try_from(newer).unwrap_or(u64::MAX), newest)
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
        let place = state.place.clone();
        let result = match (found, place) {
            (Some(seq), Some((dir, account))) => {
                let marks = state
                    .seen
                    .as_mut()
                    .and_then(|seen| marks_mut(seen, &account, &dir));
                match marks {
                    None => Err(SEEN_NOT_READ.to_owned()),
                    Some(marks) => {
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

    /// The user cleared the New count: the marker of every note that is new now goes to its
    /// text seq, in one write, and the windows are told once. Scans first, so a note that got
    /// text since the list was read is marked at the text it has now. Answers how many notes it
    /// marked.
    pub fn seen_all(&self) -> Result<usize, String> {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        let fresh: Vec<(String, u64)> = new_notes_of(&state)
            .into_iter()
            .map(|n| (n.summary.key.clone(), n.text_seq))
            .collect();
        let place = state.place.clone();
        let result = match place {
            Some((dir, account)) => {
                let marks = state
                    .seen
                    .as_mut()
                    .and_then(|seen| marks_mut(seen, &account, &dir));
                match marks {
                    None => Err(SEEN_NOT_READ.to_owned()),
                    Some(marks) => {
                        for (key, seq) in &fresh {
                            marks.insert(key.clone(), *seq);
                        }
                        if !fresh.is_empty() {
                            state.dirty = true;
                        }
                        if self.save_seen(&mut state) {
                            Ok(fresh.len())
                        } else {
                            Err(SEEN_NOT_SAVED.to_owned())
                        }
                    }
                }
            }
            None => {
                self.save_seen(&mut state);
                Ok(0)
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

    /// How many notes are new in the index as it is, without waiting: `None` while a scan
    /// holds the index (for the main thread, which must not wait for one).
    pub fn try_new_count(&self) -> Option<usize> {
        let state = match self.inner.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(new_notes_of(&state).len())
    }

    /// [`at_path`](Self::at_path) without waiting: `None` while a scan holds the index (for
    /// the main thread, which must not wait for one).
    pub fn try_at_path(&self, path: &str) -> Option<Option<NoteSummary>> {
        let state = match self.inner.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(by_path(&state).remove(path).map(|n| n.summary.clone()))
    }

    /// The note with the key `key` when the index lists it (it has text and is not deleted,
    /// unscoped or hidden by another note at its path). Uses the index as it is.
    pub fn listed_by_key(&self, key: &str) -> Option<NoteSummary> {
        let state = lock(&self.inner.state);
        listed_notes(&state)
            .into_iter()
            .find(|n| n.summary.key == key)
            .map(|n| n.summary.clone())
    }

    /// Every note the index lists, by key, as [`listed_by_key`](Self::listed_by_key) finds
    /// them. Scans first, so a note that got text since the last scan is in it.
    pub fn listed_by_keys(&self) -> HashMap<String, NoteSummary> {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        self.save_seen(&mut state);
        self.tell(&mut state);
        listed_notes(&state)
            .into_iter()
            .map(|n| (n.summary.key.clone(), n.summary.clone()))
            .collect()
    }

    /// The store file of the note at the wire path `path`, by the rule of
    /// [`at_path`](Self::at_path), for a reader of the whole note (the review). Scans first.
    pub fn store_file(&self, path: &str) -> Option<PathBuf> {
        let mut state = lock(&self.inner.state);
        self.scan(&mut state);
        self.save_seen(&mut state);
        self.tell(&mut state);
        let (data_dir, account) = state.place.clone()?;
        let winner = by_path(&state).remove(path)?;
        let name = state
            .files
            .iter()
            .find(|(_, f)| f.note.as_ref().is_some_and(|n| std::ptr::eq(n, winner)))
            .map(|(name, _)| name.clone())?;
        Some(
            data_dir
                .join("pipeline")
                .join(account)
                .join("notes")
                .join(name),
        )
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

    /// Loads the markers when they are not loaded yet. A seen.json that cannot be read stays
    /// unloaded, so it is never written over.
    fn load(&self, state: &mut State) {
        if state.seen.is_none() {
            match load_seen(&self.inner.config_dir) {
                Ok(seen) => state.seen = Some(seen),
                Err(e) => eprintln!("notedown: the seen notes could not be read: {e}"),
            }
        }
    }

    /// Brings the index up to the store, then sets the baseline, brings markers along and
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
        self.load(state);
        let Some((data_dir, account)) = place else {
            return;
        };
        if let Some(seen) = state.seen.as_mut()
            && use_folder(seen, &account, &data_dir)
        {
            state.dirty = true;
        }
        let store = data_dir.join("pipeline").join(&account);
        let dir = store.join("notes");
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            // The store is not there (yet, or any more) in a data folder that is: no notes, and
            // nothing pruned. A store the pipeline made without notes yet gets an empty
            // baseline. A data folder that cannot be read (a disk that is not mounted) keeps
            // the index as it is.
            Err(e) if e.kind() == io::ErrorKind::NotFound && data_dir.is_dir() => {
                state.files.clear();
                if store.is_dir()
                    && let Some(seen) = state.seen.as_mut()
                    && marks_of(seen, &account, &data_dir).is_none()
                {
                    add_folder(seen, &account, &data_dir, BTreeMap::new());
                    state.dirty = true;
                }
                return;
            }
            Err(_) => return,
        };
        let mut old = std::mem::take(&mut state.files);
        let mut files = BTreeMap::new();
        // The earlier state of every note read again or gone.
        let mut before: Vec<Note> = Vec::new();
        let mut complete = true;
        for entry in read {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // The pipeline's temp files are `.<name>.<pid>.<n>.tmp`.
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                // Not gone, only not readable for now: it stays as it was.
                Err(_) => {
                    if let Some(known) = old.remove(name) {
                        files.insert(name.to_owned(), known);
                    }
                    complete = false;
                    continue;
                }
            };
            if !meta.is_file() {
                continue;
            }
            let stamp = stamp_of(&meta);
            let indexed = match old.remove(name) {
                Some(known) if known.stamp == stamp => known,
                known => match read_note(&path, stamp) {
                    Ok(note) => {
                        let known = known.and_then(|k| k.note);
                        for n in note.iter().chain(&known) {
                            state.reread.insert(n.summary.path.clone());
                        }
                        before.extend(known);
                        Indexed { stamp, note }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        let known = known.and_then(|k| k.note);
                        if let Some(n) = &known {
                            state.reread.insert(n.summary.path.clone());
                        }
                        before.extend(known);
                        continue;
                    }
                    // Not readable for now: the old entry stays (with its old stamp, so the
                    // next scan reads it again), and nothing is pruned.
                    Err(_) => {
                        complete = false;
                        match known {
                            Some(known) => known,
                            None => continue,
                        }
                    }
                },
            };
            files.insert(name.to_owned(), indexed);
        }
        if complete {
            let gone: Vec<Note> = old.into_values().filter_map(|f| f.note).collect();
            for n in &gone {
                state.reread.insert(n.summary.path.clone());
            }
            before.extend(gone);
        } else {
            // A file that could not be listed is not gone: the old entries stay this scan.
            for (name, known) in old {
                files.entry(name).or_insert(known);
            }
        }
        state.files = files;
        self.mark(state, &account, &data_dir, &before, complete);
    }

    /// The baseline of a new store, markers of notes that came back or moved, and pruning
    /// (only after a scan that read every file).
    fn mark(&self, state: &mut State, account: &str, dir: &Path, before: &[Note], complete: bool) {
        let Some(mut seen) = state.seen.take() else {
            return;
        };
        let notes: Vec<&Note> = state
            .files
            .values()
            .filter_map(|f| f.note.as_ref())
            .collect();
        let changed = match marks_mut(&mut seen, account, dir) {
            None => {
                let baseline = notes
                    .iter()
                    .filter(|n| n.summary.pages_with_text > 0)
                    .map(|n| (n.summary.key.clone(), n.text_seq))
                    .collect();
                add_folder(&mut seen, account, dir, baseline);
                true
            }
            Some(marks) => {
                let came_back = follow_returns(marks, &notes, before);
                let moving = follow_moves(marks, &notes);
                let pruned = complete && prune(marks, &notes, state.files.keys(), before);
                came_back || moving || pruned
            }
        };
        state.seen = Some(seen);
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
                #[cfg(test)]
                self.inner
                    .saves
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                state.dirty = false;
                true
            }
            Err(e) => {
                eprintln!("notedown: the seen notes could not be saved: {e}");
                false
            }
        }
    }

    /// Tells the windows when All notes or New notes is not what they were told last, or when a
    /// scan read a store file again since.
    fn tell(&self, state: &mut State) {
        let reread = std::mem::take(&mut state.reread);
        let all = listed_notes(state);
        let new = new_notes_of(state);
        let same = |told: &[NoteSummary], now: &[&Note]| {
            told.len() == now.len() && told.iter().zip(now).all(|(t, n)| *t == n.summary)
        };
        let unchanged = state
            .told
            .as_ref()
            .is_some_and(|(a, n)| same(a, &all) && same(n, &new));
        if !unchanged || !reread.is_empty() {
            let summaries =
                |notes: Vec<&Note>| notes.into_iter().map(|n| n.summary.clone()).collect();
            state.told = Some((summaries(all), summaries(new)));
            (self.inner.emit)(NotesChanged {
                paths: reread.into_iter().collect(),
            });
        }
    }
}

/// A note that came back (from the bin, into the scope): the text it had when it went was
/// seen, so its marker follows it to the text it has now. True when a marker changed.
fn follow_returns(marks: &mut BTreeMap<String, u64>, notes: &[&Note], before: &[Note]) -> bool {
    let mut changed = false;
    for note in notes.iter().filter(|n| n.live()) {
        let went = before
            .iter()
            .find(|b| b.summary.key == note.summary.key && !b.live());
        if let (Some(went), Some(mark)) = (went, marks.get_mut(&note.summary.key))
            && *mark >= went.text_seq
            && *mark < note.text_seq
        {
            *mark = note.text_seq;
            changed = true;
        }
    }
    changed
}

/// A path-keyed note in the middle of a move (the pipeline writes the new state before it
/// removes the old one): the new key has the same pages with the same text versions as one
/// marked note, and gets its marker. True when a marker was added.
fn follow_moves(marks: &mut BTreeMap<String, u64>, notes: &[&Note]) -> bool {
    let mut changed = false;
    for note in notes {
        let key = &note.summary.key;
        if !key.starts_with("path:") || note.pages.is_empty() || marks.contains_key(key) {
            continue;
        }
        let twins: Vec<u64> = notes
            .iter()
            .filter(|o| o.summary.key.starts_with("path:") && o.pages == note.pages)
            .filter_map(|o| marks.get(&o.summary.key).copied())
            .collect();
        if let [value] = twins.as_slice() {
            marks.insert(key.clone(), *value);
            changed = true;
        }
    }
    changed
}

/// Removes the markers of keys that have no state file. A gone path-keyed key passes its
/// marker to the one unmarked path-keyed note that is the same note. True when a marker
/// changed.
fn prune<'a>(
    marks: &mut BTreeMap<String, u64>,
    notes: &[&Note],
    names: impl Iterator<Item = &'a String>,
    before: &[Note],
) -> bool {
    let mut present: BTreeSet<String> = names.map(|name| key_of_file(name)).collect();
    present.extend(notes.iter().map(|n| n.summary.key.clone()));
    let gone: Vec<String> = marks
        .keys()
        .filter(|k| !present.contains(*k))
        .cloned()
        .collect();
    let changed = !gone.is_empty();
    for old in gone {
        let value = marks.remove(&old).unwrap_or_default();
        if !old.starts_with("path:") {
            continue;
        }
        let went = before.iter().find(|n| n.summary.key == old);
        let candidates: Vec<&&Note> = notes
            .iter()
            .filter(|n| n.summary.key.starts_with("path:") && !marks.contains_key(&n.summary.key))
            .filter(|n| match went {
                // The same pages with the same text versions.
                Some(went) => !went.pages.is_empty() && n.pages == went.pages,
                // Not indexed before (a move while the app was not running): the seq of a
                // text version belongs to one note only.
                None => value > 0 && n.text_seq == value,
            })
            .collect();
        if let [moved] = candidates.as_slice() {
            marks.insert(moved.summary.key.clone(), value);
        }
    }
    changed
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

/// New notes: none before the baseline of the store is set, or while the markers cannot be
/// read.
fn new_notes_of(state: &State) -> Vec<&Note> {
    let Some(marks) = state
        .place
        .as_ref()
        .and_then(|(dir, account)| marks_of(state.seen.as_ref()?, account, dir))
    else {
        return Vec::new();
    };
    listed_notes(state)
        .into_iter()
        .filter(|n| n.text_seq > marks.get(&n.summary.key).copied().unwrap_or(0))
        .collect()
}

fn new_of(state: &State) -> Vec<NoteSummary> {
    new_notes_of(state)
        .into_iter()
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

/// The summary of one store file; `None` when it does not parse, an error when it cannot be
/// read.
fn read_note(path: &Path, stamp: Stamp) -> io::Result<Option<Note>> {
    let file = io::BufReader::new(fs::File::open(path)?);
    let stored: StoredNote = match serde_json::from_reader(file) {
        Ok(stored) => stored,
        Err(e) if e.is_io() => return Err(e.into()),
        Err(_) => return Ok(None),
    };
    let pages_with_text = stored.pages.iter().filter(|p| !p.blocks.is_empty()).count();
    let text_seq = stored.pages.iter().filter_map(|p| p.seq).max().unwrap_or(0);
    let changed_ms = i64::try_from(stamp.mtime_ns.div_euclid(1_000_000)).unwrap_or(0);
    let title = if stored.title.is_empty() {
        title_of(&stored.path)
    } else {
        stored.title
    };
    Ok(Some(Note {
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
    }))
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
/// `seen.json.broken` and the markers start empty (the next scan sets a new baseline). A file
/// that cannot be read is an error: it is never written over.
fn load_seen(dir: &Path) -> io::Result<Seen> {
    let path = dir.join(SEEN_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            eprintln!("notedown: {} does not parse: {e}", path.display());
            let _ = fs::rename(&path, dir.join(SEEN_BROKEN));
            Seen::new()
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Seen::new()),
        Err(e) => Err(e),
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
        return written;
    }
    // The rename is flushed too; the new file is in place already, so a failure is only logged.
    if let Err(e) = fs::File::open(dir).and_then(|folder| folder.sync_all()) {
        eprintln!("notedown: the folder of {SEEN_FILE} could not be flushed: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
