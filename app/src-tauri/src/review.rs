//! The review of one note: its text per page from the pipeline's store, and its pages as
//! images.
//!
//! **The text.** `note_open` reads the whole store file of the note (`notes/<key>.json` of the
//! account, found through the notes index) and asks the server for the entry
//! (`GET /entries?path=`). When the store read the version the server has (or the server does
//! not answer), the pages are the store's: per page its blocks, and their text joined by blank
//! lines as its Markdown. The note is catching up while a page waits for OCR.
//!
//! Otherwise (the store does not know the note, or read another version) the pages come from
//! the file: its bytes are cached as for a page image (step 1 below) and
//! `notedown render <note> --info --allow-unknown-version` counts its pages (by md5, once a
//! run). Each page takes the state and the blocks of the store's page with the same page id;
//! any other page waits for OCR. Such a note is always catching up. When the file cannot be
//! read, the open fails with the reason (never the store's text of another version). The
//! download and the count stop when the review closes and at Quit, as a render does. A note
//! the server does not have
//! (or a folder) is gone. `note_page_image` checks a page against the pages of the last
//! `note_open` of the path; for a path never opened against the notes index, and for one the
//! index does not know against the file's pages.
//!
//! **The images.** `note_page_image(path, index)` asks the server for the entry, checks that it
//! is a file with an md5 of 32 lower-case hex characters (server data never becomes a path
//! unchecked), and answers `pages/<md5>/<index>.png` of the app cache as a
//! `data:image/png;base64,...` URL. A page that is not cached is rendered alone:
//!
//! 1. The bytes of the note are cached at `notes/<md5>.note`. They are downloaded into a temp
//!    file (`.<md5>.<job>.part`, made with `O_EXCL` and `O_NOFOLLOW`) under the stall watchdog
//!    of `transfer.rs`, and kept only when their md5 is the entry's. A cached note is checked
//!    again once a run before it is used. One download per md5 at a time.
//! 2. `notedown render <note> --page <index + 1> --allow-unknown-version --out <dir>` runs in a
//!    process group of its own, with absolute paths, into a temp folder
//!    `pages/<md5>.tmp-<job>/`. Only when it ends with code 0 is the PNG it printed (a regular
//!    file in that folder, with a PNG signature) moved to `pages/<md5>/<index>.png`. A run that
//!    fails or is killed leaves nothing in the cache; its temp folder goes.
//! 3. One render per (md5, page): a second call for the same page waits for the running one.
//!    A render is killed when every review that waits for it closes (`note_close(path)`, and
//!    every review when the library window hides) and at Quit; a render that takes longer than
//!    [`RENDER_TIMEOUT`] is killed too, until its group is empty (a process forked during the
//!    kill can miss it). A group is signalled only while it has a member.
//!
//! The leftovers of a run that ended without cleaning up (temp folders and temp files) are
//! removed once a run, before its first render or download. After each render the cache
//! (`notes/` and `pages/` together) is trimmed to [`CACHE_LIMIT`], the oldest files first (a
//! file used is touched); files of a note that is being rendered stay.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use md5::{Digest as _, Md5};
use reqwest::header::ETAG;
use serde::{Deserialize, Serialize, Serializer};

use crate::library::{Entry, EntryKind, Library, LibraryError, Session, is_md5};
use crate::notes::{NOTE_GONE, Notes};
use crate::process::{self, SpawnFn};
use crate::rest::RestError;
use crate::supervisor::Clock;
use crate::transfer::{Watched, watched};

/// The cache of `notes/` and `pages/` together.
pub const CACHE_LIMIT: u64 = 1 << 30;
/// How long one render of one page may take before it is killed.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

/// What a call hears when its review was closed while it waited.
pub const REVIEW_CLOSED: &str = "The review was closed.";
/// What a call hears after Quit.
pub const QUITTING: &str = "Notedown is quitting.";
/// An entry without a usable md5.
pub const NO_MD5: &str = "The server sent no valid md5 for this file.";
/// The bytes of the note did not have the md5 of the entry, or were more than its size.
pub const NOT_WHOLE: &str = "The file did not arrive whole. Try again.";
/// The server sent another version of the file than the entry named.
pub const CHANGED: &str = "The file was changed on the server. Try again.";
/// The entry is a folder.
pub const NOT_A_FILE: &str = "This is a folder, not a file.";
/// The store file of the note does not parse.
pub const NOT_READ: &str = "Notedown could not read this note.";
/// The cache folder could not be used.
pub const NO_CACHE: &str = "Notedown could not use its cache folder.";
/// The render printed no usable image.
pub const NO_IMAGE: &str = "The page could not be shown.";
/// The render did not end in time.
pub const TOO_LONG: &str = "The page took too long to show.";
/// How long `render --info` may take to count the pages of a note. The run is not stopped by a
/// close: it ends within this bound, and its answer is dropped for a closed review.
pub const INFO_TIMEOUT: Duration = Duration::from_secs(60);
/// How many page lists of note files are kept in memory (by md5).
pub const INFOS_KEPT: usize = 64;
/// A page index past the note's pages.
pub const NO_SUCH_PAGE: &str = "This page is not in the note.";
/// What a call hears when its page was dropped from the view while it waited.
pub const PAGE_DROPPED: &str = "The page is not shown anymore.";

/// The first bytes of every PNG file.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// How many renders run at once.
pub const RUNS_AT_ONCE: usize = 2;
/// How long a render that was killed may take to be reaped.
const GONE_WAIT: Duration = Duration::from_secs(5);
/// The stderr bytes a render keeps for its error text.
const STDERR_KEPT: u64 = 64 * 1024;
/// The stdout lines a render keeps.
const LINES_KEPT: usize = 16;
/// The size of one piece read for an md5.
const CHUNK: usize = 64 * 1024;

/// One page of a note as the review shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageView {
    /// 0-based, as `note_page_image` takes it.
    pub index: usize,
    /// The page id of the pipeline's store (`PAGEID`, `position:<n>`, `<id>#2`).
    pub page_id: String,
    pub state: PageState,
    /// The text of the page's blocks, joined by blank lines.
    pub markdown: String,
    /// The blocks of the page in their order; their texts make up `markdown`.
    pub blocks: Vec<BlockView>,
    pub landscape: bool,
}

/// One block of a page: its id in the store and its text (without trailing white space).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockView {
    pub id: String,
    pub text: String,
}

/// The OCR state of a page, as the store has it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PageState {
    Done,
    #[default]
    Pending,
    Failed,
    Truncated,
}

/// A page image as a `data:image/png;base64,...` URL. Shared: every call that waited for one
/// render gets the same string, not a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUrl(Arc<str>);

impl DataUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for DataUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl PartialEq<String> for DataUrl {
    fn eq(&self, other: &String) -> bool {
        *self.0 == **other
    }
}

/// A note as the review shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteView {
    /// The key of the note in the pipeline's store; none when the store does not know it.
    pub key: Option<String>,
    /// The md5 of the file version the pages are of: the server's when the pages come from
    /// the file, else the one the store read. A window drops its page images when it changes.
    pub md5: Option<String>,
    pub path: String,
    pub title: String,
    /// A page waits for OCR, or the server has another version than the text was read from.
    pub catching_up: bool,
    pub pages: Vec<PageView>,
}

/// The part of a store file the review reads; the strokes and the device's recognition are
/// skipped while parsing. `landscape` is read when the store has it (it does not yet).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNote {
    key: String,
    path: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    md5: String,
    #[serde(default)]
    landscape: bool,
    #[serde(default)]
    pages: Vec<StoredPage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPage {
    #[serde(default)]
    page_id: String,
    #[serde(default)]
    state: PageState,
    #[serde(default)]
    landscape: Option<bool>,
    #[serde(default)]
    blocks: Vec<StoredBlock>,
}

#[derive(Deserialize)]
struct StoredBlock {
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
}

impl StoredNote {
    /// The view of the note as the store has it.
    fn view(self, newer: bool) -> NoteView {
        let pages: Vec<PageView> = self
            .pages
            .iter()
            .enumerate()
            .map(|(index, page)| {
                page_view(
                    index,
                    &page.page_id,
                    Some(page),
                    page.landscape.unwrap_or(self.landscape),
                )
            })
            .collect();
        let pending = pages.iter().any(|p| p.state == PageState::Pending);
        let title = if self.title.is_empty() {
            title_of(&self.path)
        } else {
            self.title
        };
        NoteView {
            key: Some(self.key),
            md5: Some(self.md5).filter(|m| !m.is_empty()),
            path: self.path,
            title,
            catching_up: pending || newer,
            pages,
        }
    }
}

/// The pages of a note file as `render --info` prints them.
#[derive(Deserialize)]
struct FileInfo {
    pages: Vec<FilePage>,
}

#[derive(Debug, Deserialize)]
struct FilePage {
    id: String,
    #[serde(default)]
    landscape: bool,
}

/// A page of the review: with the store's page its state and blocks, else it waits for OCR.
fn page_view(
    index: usize,
    page_id: &str,
    stored: Option<&StoredPage>,
    landscape: bool,
) -> PageView {
    let blocks: Vec<BlockView> = stored
        .map(|page| {
            page.blocks
                .iter()
                .map(|b| BlockView {
                    id: b.id.clone(),
                    // As the pipeline's Markdown writer joins them.
                    text: b.text.trim_end().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    PageView {
        index,
        page_id: page_id.to_owned(),
        state: stored.map_or(PageState::Pending, |p| p.state),
        markdown: blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        blocks,
        landscape,
    }
}

/// The view of the note at `path` with the pages of its file: the store's text (when it knows
/// the note) goes to the page with the same id. Always catching up.
fn file_view(path: &str, md5: &str, stored: Option<StoredNote>, file: &[FilePage]) -> NoteView {
    let by_id: HashMap<&str, &StoredPage> = stored
        .iter()
        .flat_map(|n| n.pages.iter())
        .map(|p| (p.page_id.as_str(), p))
        .collect();
    let pages = file
        .iter()
        .enumerate()
        .map(|(index, page)| {
            page_view(
                index,
                &page.id,
                by_id.get(page.id.as_str()).copied(),
                page.landscape,
            )
        })
        .collect();
    let (key, title) = match &stored {
        Some(n) if !n.title.is_empty() => (Some(n.key.clone()), n.title.clone()),
        Some(n) => (Some(n.key.clone()), title_of(path)),
        None => (None, title_of(path)),
    };
    NoteView {
        key,
        md5: Some(md5.to_owned()),
        path: path.to_owned(),
        title,
        catching_up: true,
        pages,
    }
}

/// The review: note views and page images. Cheap to clone; every clone is the same review. It
/// holds the library and the notes index; neither holds it.
#[derive(Clone)]
pub struct Review {
    inner: Arc<Inner>,
}

struct Inner {
    library: Library,
    notes: Notes,
    spawn: SpawnFn,
    /// The app cache: `notes/` and `pages/` in it. Absolute.
    cache: PathBuf,
    clock: Clock,
    limit: u64,
    /// The running renders and who waits for them. Held only for short changes and signals,
    /// never across a call, a file read or a wait: `close_all` runs on the main thread.
    table: Mutex<Table>,
    /// The pages of note files by md5, as `render --info` counted them.
    infos: Mutex<HashMap<String, Arc<Vec<FilePage>>>>,
    /// The page count of the last `open` of each path, until `close(path)`. Hiding the window
    /// (`close_all`) keeps them: the reviews stay and ask for their pages again when it shows.
    opened: Mutex<HashMap<String, usize>>,
    /// One download of a note's bytes per md5 at a time.
    fetching: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The leftovers of earlier runs were removed. Held while they are removed.
    swept: Mutex<bool>,
    next_job: AtomicU64,
    /// The renders that run and the ones that wait for a free run.
    runs: Mutex<Runs>,
    /// Signals a free run, a new waiter or a stop.
    runs_changed: Condvar,
    /// Orders the requests: the newest waits least.
    next_order: AtomicU64,
}

/// At most [`RUNS_AT_ONCE`] renders run; of the ones that wait, the newest request goes first.
#[derive(Default)]
struct Runs {
    running: usize,
    waiting: Vec<Arc<Slot>>,
}

#[derive(Default)]
struct Table {
    quitting: bool,
    /// How often each review was closed, and how often all of them were.
    closes: HashMap<String, u64>,
    all_closes: u64,
    /// The running renders by (md5, page index).
    jobs: HashMap<(String, usize), Arc<Slot>>,
    /// The downloads and counts of note files for `open` (and for a page of a note the index
    /// does not know), by note path, with their md5. Not renders: they never get a result.
    opens: HashMap<String, Vec<(String, Arc<Slot>)>>,
    /// The cached notes whose bytes were checked in this run.
    verified: HashSet<String>,
    handles: Vec<JoinHandle<()>>,
    /// How many calls joined a running render, and how many render jobs were started and
    /// have not ended (for tests).
    joined: usize,
    started: usize,
}

/// One render and the reviews that wait for it.
#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

#[derive(Default)]
struct SlotState {
    /// The data URL, or why there is none.
    result: Option<Result<DataUrl, String>>,
    /// The reviews (by note path) that wait for it.
    paths: HashSet<String>,
    /// Why a review stopped waiting: it closed, or it dropped the page.
    left: HashMap<String, &'static str>,
    /// The newest request for it, for the order of the runs.
    order: u64,
    /// The process group of the render, until it is reaped.
    pgid: Option<u32>,
    /// Why it was stopped: no review waits anymore, or Quit.
    stopped: Option<&'static str>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Table {
    /// How often the review of `path` and all reviews were closed: a call that saw other
    /// marks at its start belongs to a review that closed.
    fn marks(&self, path: &str) -> (u64, u64) {
        (self.closes.get(path).copied().unwrap_or(0), self.all_closes)
    }

    /// A render or a download for `open` uses the note with `md5`: its cache files stay.
    fn uses(&self, md5: &str) -> bool {
        self.jobs.keys().any(|(m, _)| m == md5)
            || self.opens.values().flatten().any(|(m, _)| m == md5)
    }
}

/// Takes the slot of a download for `open` out of the table when the call ends.
struct OpenSlot<'a> {
    inner: &'a Inner,
    path: &'a str,
    slot: Arc<Slot>,
}

impl Drop for OpenSlot<'_> {
    fn drop(&mut self) {
        let mut table = lock(&self.inner.table);
        if let Some(slots) = table.opens.get_mut(self.path) {
            slots.retain(|(_, s)| !Arc::ptr_eq(s, &self.slot));
            if slots.is_empty() {
                table.opens.remove(self.path);
            }
        }
    }
}

/// What a failed lookup of an entry says: a gone note, or what went wrong.
fn gone_or(error: LibraryError) -> String {
    match error {
        LibraryError::Rest(RestError::NotFound) => NOTE_GONE.to_owned(),
        e => e.to_string(),
    }
}

/// Removes a file or a folder when it goes out of scope, unless it was kept.
struct Leftover {
    path: Option<PathBuf>,
    folder: bool,
}

impl Leftover {
    fn file(path: &Path) -> Self {
        Self {
            path: Some(path.to_owned()),
            folder: false,
        }
    }

    fn folder(path: &Path) -> Self {
        Self {
            path: Some(path.to_owned()),
            folder: true,
        }
    }

    fn keep(&mut self) {
        self.path = None;
    }
}

impl Drop for Leftover {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = if self.folder {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
        }
    }
}

fn data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// The file name without `.note`, as the pipeline names a note.
fn title_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".note").unwrap_or(name).to_owned()
}

/// Marks a cache file as used, for the trim's "oldest first".
fn touch(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let opened = File::options()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    if let Ok(file) = opened {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// The md5 of an open file, read from its start, as lower-case hex.
fn md5_of(file: &mut File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(crate::login::hex(&hasher.finalize()))
}

/// A new file at `path`; fails when anything is there, a symlink too.
fn create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// A regular file at `path` opened for reading, never through a symlink.
fn open_regular(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// A regular file at `path` (no symlink), read whole.
fn read_regular(path: &Path) -> Option<Vec<u8>> {
    let mut file = open_regular(path).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Why the bytes of a note stopped coming.
enum Stop {
    Rest(RestError),
    Changed,
    NotWhole,
    Disk,
}

/// Reads the answer of `GET /files/{id}` into `file`, counting the bytes.
async fn fetch(
    session: &Session,
    id: u64,
    md5: &str,
    size: u64,
    file: &mut File,
    count: &AtomicU64,
) -> Result<(), Stop> {
    let mut response = session
        .rest()
        .get_file(id, 0, md5)
        .await
        .map_err(Stop::Rest)?;
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if etag != format!("\"{md5}\"") {
        return Err(Stop::Changed);
    }
    let mut at = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Stop::Rest(RestError::Cut))?
    {
        let len = chunk.len() as u64;
        if at.saturating_add(len) > size {
            return Err(Stop::NotWhole);
        }
        file.write_all(&chunk).map_err(|_| Stop::Disk)?;
        at += len;
        count.store(at, Ordering::SeqCst);
    }
    if at < size {
        return Err(Stop::Rest(RestError::Cut));
    }
    Ok(())
}

impl Review {
    /// The review with the cache in `cache` (an absolute folder; the app's cache dir) of at
    /// most `limit` bytes. `spawn` runs the `notedown` sidecar; `clock` is the transfer
    /// watchdog's.
    pub fn new(
        library: Library,
        notes: Notes,
        spawn: SpawnFn,
        cache: PathBuf,
        clock: Clock,
        limit: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                library,
                notes,
                spawn,
                cache,
                clock,
                limit,
                table: Mutex::new(Table::default()),
                infos: Mutex::new(HashMap::new()),
                opened: Mutex::new(HashMap::new()),
                fetching: Mutex::new(HashMap::new()),
                swept: Mutex::new(false),
                next_job: AtomicU64::new(0),
                runs: Mutex::new(Runs::default()),
                runs_changed: Condvar::new(),
                next_order: AtomicU64::new(0),
            }),
        }
    }

    /// The note at the wire path `path`: its pages with the text of the pipeline's store.
    /// Blocking: it scans the store, asks the server for the entry and may download the note
    /// and count its pages.
    pub fn open(&self, path: &str) -> Result<NoteView, String> {
        let inner = &self.inner;
        let mark = {
            let table = lock(&inner.table);
            if table.quitting {
                return Err(QUITTING.to_owned());
            }
            table.marks(path)
        };
        let stored: Option<StoredNote> = match inner.notes.store_file(path) {
            None => None,
            Some(file) => match File::open(&file) {
                Ok(f) => Some(
                    serde_json::from_reader(BufReader::new(f)).map_err(|_| NOT_READ.to_owned())?,
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(_) => return Err(NOT_READ.to_owned()),
            },
        };
        let entry = match inner
            .library
            .call(|rest| rest.get_json::<Entry>("/entries", &[("path", path)]))
        {
            Ok(entry) if entry.kind == EntryKind::File => entry,
            Ok(_) | Err(LibraryError::Rest(RestError::NotFound)) => {
                return Err(NOTE_GONE.to_owned());
            }
            // Without an answer of the server the store's pages are all there is.
            Err(e) => {
                return match stored {
                    Some(stored) => Ok(inner.opened(path, stored.view(false))),
                    None => Err(e.to_string()),
                };
            }
        };
        let view = match stored {
            Some(stored) if entry.md5.as_deref() == Some(stored.md5.as_str()) => stored.view(false),
            // The file decides. When it cannot be read now, the open fails: the store's text
            // must not stand next to images of a file its pages were not matched to.
            stored => match inner.file_pages(path, &entry, mark) {
                Ok(pages) => file_view(
                    path,
                    entry.md5.as_deref().unwrap_or_default(),
                    stored,
                    &pages,
                ),
                Err(e) => {
                    lock(&inner.opened).remove(path);
                    return Err(e);
                }
            },
        };
        Ok(inner.opened(path, view))
    }

    /// Page `index` (0-based) of the note at `path` as a `data:image/png;base64,...` URL.
    /// Blocking: it asks the server, may download the note and waits for the render.
    pub fn page_image(&self, path: &str, index: usize) -> Result<DataUrl, String> {
        let inner = &self.inner;
        if lock(&inner.table).quitting {
            return Err(QUITTING.to_owned());
        }
        // Only a page of the note as the last `note_open` of the path showed it: no request,
        // download or run for one past its end. A path never opened takes the index as it is
        // (the scanner keeps it fresh, so a page does not scan the store again).
        let count = lock(&inner.opened)
            .get(path)
            .copied()
            .or_else(|| inner.notes.at_path(path).map(|note| note.pages));
        if count.is_some_and(|count| index >= count) {
            return Err(NO_SUCH_PAGE.to_owned());
        }
        let mark = {
            let table = lock(&inner.table);
            if table.quitting {
                return Err(QUITTING.to_owned());
            }
            table.marks(path)
        };
        let entry = inner
            .library
            .call(|rest| rest.get_json::<Entry>("/entries", &[("path", path)]))
            .map_err(gone_or)?;
        if entry.kind != EntryKind::File {
            return Err(if count.is_some() {
                NOT_A_FILE.to_owned()
            } else {
                NOTE_GONE.to_owned()
            });
        }
        // Checked before it is part of any path.
        let md5 = entry
            .md5
            .clone()
            .filter(|m| is_md5(m))
            .ok_or_else(|| NO_MD5.to_owned())?;
        // A note neither opened nor in the index: the pages of its file decide.
        if count.is_none() && index >= inner.file_pages(path, &entry, mark)?.len() {
            return Err(NO_SUCH_PAGE.to_owned());
        }
        let png = inner.page_file(&md5, index);
        if let Some(url) = cached(&png) {
            return Ok(url);
        }
        let slot = {
            let mut table = lock(&inner.table);
            if table.quitting {
                return Err(QUITTING.to_owned());
            }
            if table.marks(path) != mark {
                return Err(REVIEW_CLOSED.to_owned());
            }
            let key = (md5.clone(), index);
            // A stopped job stays in the table until its worker sees the stop: a new call
            // starts a job of its own over it.
            let running = table
                .jobs
                .get(&key)
                .filter(|slot| lock(&slot.state).stopped.is_none())
                .cloned();
            let order = inner.next_order.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(slot) = running {
                {
                    let mut state = lock(&slot.state);
                    state.paths.insert(path.to_owned());
                    state.left.remove(path);
                    state.order = order;
                }
                table.joined += 1;
                slot
            } else {
                let slot = Arc::new(Slot::default());
                {
                    let mut state = lock(&slot.state);
                    state.paths.insert(path.to_owned());
                    state.order = order;
                }
                table.jobs.insert(key, slot.clone());
                table.started += 1;
                let worker = inner.clone();
                let job = slot.clone();
                table.handles.retain(|h| !h.is_finished());
                table.handles.push(std::thread::spawn(move || {
                    worker.run(&entry, &md5, index, &job);
                }));
                slot
            }
        };
        let mut state = lock(&slot.state);
        loop {
            if let Some(result) = &state.result {
                return result.clone();
            }
            if let Some(why) = state.stopped {
                return Err(why.to_owned());
            }
            if !state.paths.contains(path) {
                return Err(state
                    .left
                    .get(path)
                    .copied()
                    .unwrap_or(REVIEW_CLOSED)
                    .to_owned());
            }
            state = slot
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The review of `path` closed: its calls that wait end, and a render no other review
    /// waits for is killed. It never blocks for long (short locks and signals).
    pub fn close(&self, path: &str) {
        {
            let mut table = lock(&self.inner.table);
            *table.closes.entry(path.to_owned()).or_default() += 1;
            lock(&self.inner.opened).remove(path);
            for slot in table.jobs.values() {
                leave(slot, path, REVIEW_CLOSED);
            }
            for (_, slot) in table.opens.get(path).into_iter().flatten() {
                stop(&mut lock(&slot.state), REVIEW_CLOSED);
            }
        }
        self.inner.wake_runs();
    }

    /// The review of `path` does not show page `index` anymore (it went far out of view): its
    /// call that waits ends, and the render is killed when no other review waits for it. It
    /// never blocks for long.
    pub fn drop_page(&self, path: &str, index: usize) {
        {
            let table = lock(&self.inner.table);
            for ((_, at), slot) in &table.jobs {
                if *at == index {
                    leave(slot, path, PAGE_DROPPED);
                }
            }
        }
        self.inner.wake_runs();
    }

    /// Every review closed (the library window hid). Runs on the main thread: it only takes
    /// short locks and sends signals.
    pub fn close_all(&self) {
        {
            let mut table = lock(&self.inner.table);
            table.all_closes += 1;
            for slot in table.jobs.values() {
                let mut state = lock(&slot.state);
                state.paths.clear();
                stop(&mut state, REVIEW_CLOSED);
                slot.done.notify_all();
            }
            for (_, slot) in table.opens.values().flatten() {
                stop(&mut lock(&slot.state), REVIEW_CLOSED);
            }
        }
        self.inner.wake_runs();
    }

    /// Quit: every render is killed and nothing starts after it. It does not wait.
    pub fn quit(&self) {
        {
            let mut table = lock(&self.inner.table);
            table.quitting = true;
            for slot in table.jobs.values() {
                let mut state = lock(&slot.state);
                stop(&mut state, QUITTING);
                slot.done.notify_all();
            }
            for (_, slot) in table.opens.values().flatten() {
                stop(&mut lock(&slot.state), QUITTING);
            }
        }
        self.inner.wake_runs();
    }

    /// How many renders wait for a free run.
    #[cfg(test)]
    fn queued(&self) -> usize {
        lock(&self.inner.runs).waiting.len()
    }

    /// How many calls joined a running render.
    #[cfg(test)]
    fn joined(&self) -> usize {
        lock(&self.inner.table).joined
    }

    /// How many render jobs were started and have not ended (a stopped one counts until its
    /// worker ends).
    #[cfg(test)]
    fn running(&self) -> usize {
        lock(&self.inner.table).started
    }

    /// Waits until no render thread runs.
    #[cfg(test)]
    fn join(&self) {
        loop {
            let handles = std::mem::take(&mut lock(&self.inner.table).handles);
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                handle.join().expect("a render thread panicked");
            }
        }
    }

    #[cfg(test)]
    fn library(&self) -> Library {
        self.inner.library.clone()
    }

    #[cfg(test)]
    fn notes(&self) -> Notes {
        self.inner.notes.clone()
    }
}

/// The review of `path` stops waiting for `slot`, for `why`; with no review left the render
/// stops.
fn leave(slot: &Slot, path: &str, why: &'static str) {
    let mut state = lock(&slot.state);
    if state.paths.remove(path) {
        state.left.insert(path.to_owned(), why);
        if state.paths.is_empty() {
            stop(&mut state, why);
        }
        slot.done.notify_all();
    }
}

/// Stops a render that has no result yet: kills its group while it is unreaped.
fn stop(state: &mut SlotState, why: &'static str) {
    if state.result.is_some() {
        return;
    }
    state.stopped.get_or_insert(why);
    if let Some(pgid) = state.pgid {
        let _ = process::signal_group(pgid, libc::SIGKILL);
        kill_until_empty_in_background(pgid);
    }
}

/// Kills the group `pgid` until it is empty, on a thread of its own: the stops run on the main
/// thread and never wait. A process forked while the first SIGKILL was on its way can have
/// missed it, and it holds the render's pipes, so its reaper would wait for it.
fn kill_until_empty_in_background(pgid: u32) {
    std::thread::spawn(move || {
        if !process::kill_group_until_empty(pgid, process::KILL_GUARD) {
            eprintln!("notedown: the process group {pgid} is still there after the kill");
        }
    });
}

/// The cached page at `png` as a data URL, touched; none when it is not there.
fn cached(png: &Path) -> Option<DataUrl> {
    let bytes = read_regular(png)?;
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    touch(png);
    Some(DataUrl(data_url(&bytes).into()))
}

/// A run of the sidecar; the next waiter may go when it is dropped.
struct RunPermit<'a> {
    inner: &'a Inner,
}

impl Drop for RunPermit<'_> {
    fn drop(&mut self) {
        lock(&self.inner.runs).running -= 1;
        self.inner.runs_changed.notify_all();
    }
}

/// Answers every waiter of a render when the worker ends, also when it panics.
struct Finish<'a> {
    inner: &'a Inner,
    key: (String, usize),
    slot: &'a Arc<Slot>,
    result: Option<Result<DataUrl, String>>,
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err(NO_IMAGE.to_owned()));
        {
            let mut table = lock(&self.inner.table);
            if table
                .jobs
                .get(&self.key)
                .is_some_and(|s| Arc::ptr_eq(s, self.slot))
            {
                table.jobs.remove(&self.key);
            }
            table.started -= 1;
        }
        let mut state = lock(&self.slot.state);
        let result = match state.stopped {
            Some(why) if result.is_err() => Err(why.to_owned()),
            _ => result,
        };
        state.result = Some(result);
        self.slot.done.notify_all();
    }
}

impl Inner {
    fn page_file(&self, md5: &str, index: usize) -> PathBuf {
        self.cache
            .join("pages")
            .join(md5)
            .join(format!("{index}.png"))
    }

    /// The worker of one render: renders, trims the cache, takes the job out of the table and
    /// answers every caller that waits.
    fn run(self: &Arc<Self>, entry: &Entry, md5: &str, index: usize, slot: &Arc<Slot>) {
        let mut finish = Finish {
            inner: self,
            key: (md5.to_owned(), index),
            slot,
            result: None,
        };
        let result = self
            .render(entry, md5, index, slot)
            .map(|url| DataUrl(url.into()));
        // While the job is still in the table, so its own files stay.
        self.trim();
        finish.result = Some(result);
    }

    /// Wakes the renders that wait for a run, after a stop. Taking the lock first means no
    /// waiter misses it between its look and its wait.
    fn wake_runs(&self) {
        let _runs = lock(&self.runs);
        self.runs_changed.notify_all();
    }

    /// Waits for a free run: at most [`RUNS_AT_ONCE`], the newest request first. A stop ends
    /// the wait.
    fn run_permit(&self, slot: &Arc<Slot>) -> Result<RunPermit<'_>, String> {
        let mut runs = lock(&self.runs);
        runs.waiting.push(slot.clone());
        loop {
            let (stopped, mine) = {
                let state = lock(&slot.state);
                (state.stopped, state.order)
            };
            let newest = runs.waiting.iter().all(|other| {
                let state = lock(&other.state);
                state.stopped.is_some() || state.order <= mine
            });
            if stopped.is_some() || (newest && runs.running < RUNS_AT_ONCE) {
                runs.waiting.retain(|s| !Arc::ptr_eq(s, slot));
                if let Some(why) = stopped {
                    drop(runs);
                    self.runs_changed.notify_all();
                    return Err(why.to_owned());
                }
                runs.running += 1;
                return Ok(RunPermit { inner: self });
            }
            runs = self
                .runs_changed
                .wait(runs)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Keeps the page count of `view` as the last open of `path`.
    fn opened(&self, path: &str, view: NoteView) -> NoteView {
        lock(&self.opened).insert(path.to_owned(), view.pages.len());
        view
    }

    /// The pages of the note file of `entry`, counted by `render --info` and kept by md5. The
    /// download and the count belong to the review of `path`, whose close marks were `mark` at
    /// the start of the call: a close or Quit stops them.
    fn file_pages(
        &self,
        path: &str,
        entry: &Entry,
        mark: (u64, u64),
    ) -> Result<Arc<Vec<FilePage>>, String> {
        // Checked before it is part of any path.
        let md5 = entry
            .md5
            .clone()
            .filter(|m| is_md5(m))
            .ok_or_else(|| NO_MD5.to_owned())?;
        if let Some(pages) = lock(&self.infos).get(&md5) {
            return Ok(pages.clone());
        }
        if !self.cache.is_absolute() {
            return Err(NO_CACHE.to_owned());
        }
        let slot = Arc::new(Slot::default());
        {
            let mut table = lock(&self.table);
            if table.quitting {
                return Err(QUITTING.to_owned());
            }
            if table.marks(path) != mark {
                return Err(REVIEW_CLOSED.to_owned());
            }
            table
                .opens
                .entry(path.to_owned())
                .or_default()
                .push((md5.clone(), slot.clone()));
        }
        let _open = OpenSlot {
            inner: self,
            path,
            slot: slot.clone(),
        };
        self.sweep_once();
        let note = self.note_file(entry, &md5, &slot)?;
        if let Some(why) = lock(&slot.state).stopped {
            return Err(why.to_owned());
        }
        let args = vec![
            "render".to_owned(),
            note.display().to_string(),
            "--info".to_owned(),
            "--allow-unknown-version".to_owned(),
        ];
        let output = process::run_bounded(&self.spawn, &args, INFO_TIMEOUT)?;
        if let Some(why) = lock(&slot.state).stopped {
            return Err(why.to_owned());
        }
        if !output.status.success() {
            return Err(format!(
                "{NOT_READ} {}",
                process::error_text(&output.stderr, output.status)
            ));
        }
        let info: FileInfo = String::from_utf8_lossy(&output.stdout)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str(l).ok())
            .ok_or_else(|| NOT_READ.to_owned())?;
        let pages = Arc::new(info.pages);
        self.keep_info(md5, pages.clone());
        Ok(pages)
    }

    /// Keeps the page list of the file with `md5`; past [`INFOS_KEPT`] another one goes.
    fn keep_info(&self, md5: String, pages: Arc<Vec<FilePage>>) {
        let mut infos = lock(&self.infos);
        if !infos.contains_key(&md5) && infos.len() >= INFOS_KEPT {
            let other = infos.keys().next().cloned();
            if let Some(other) = other {
                infos.remove(&other);
            }
        }
        infos.insert(md5, pages);
    }

    /// One render of page `index`, into a temp folder; the PNG moves into the cache only when
    /// the run ended with code 0.
    fn render(
        &self,
        entry: &Entry,
        md5: &str,
        index: usize,
        slot: &Arc<Slot>,
    ) -> Result<String, String> {
        if !self.cache.is_absolute() {
            return Err(NO_CACHE.to_owned());
        }
        self.sweep_once();
        let note = self.note_file(entry, md5, slot)?;
        // Held until the run ends; the download above does not count.
        let _run = self.run_permit(slot)?;
        let pages = self.cache.join("pages");
        fs::create_dir_all(&pages).map_err(|_| NO_CACHE.to_owned())?;
        let job = self.next_job.fetch_add(1, Ordering::SeqCst) + 1;
        let tmp = pages.join(format!("{md5}.tmp-{job}"));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir(&tmp).map_err(|_| NO_CACHE.to_owned())?;
        let _tmp = Leftover::folder(&tmp);
        let page = index.checked_add(1).ok_or_else(|| NO_IMAGE.to_owned())?;
        let args = vec![
            "render".to_owned(),
            note.display().to_string(),
            "--page".to_owned(),
            page.to_string(),
            "--allow-unknown-version".to_owned(),
            "--out".to_owned(),
            tmp.display().to_string(),
        ];
        let mut command = (self.spawn)(&args);
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Spawned under the slot's lock: a stop either comes before (nothing starts) or sees
        // the group.
        let mut child = {
            let mut state = lock(&slot.state);
            if let Some(why) = state.stopped {
                return Err(why.to_owned());
            }
            let child = command
                .spawn()
                .map_err(|e| format!("The notedown program could not start: {e}"))?;
            state.pgid = Some(child.id());
            child
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::channel();
        // The reaper is a thread of its own (not scoped): a wait for it is always bounded.
        let reaper = slot.clone();
        std::thread::spawn(move || {
            let errors = std::thread::spawn(move || {
                let mut kept = Vec::new();
                if let Some(mut stderr) = stderr {
                    let _ = (&mut stderr).take(STDERR_KEPT).read_to_end(&mut kept);
                    let _ = io::copy(&mut stderr, &mut io::sink());
                }
                kept
            });
            let mut lines = Vec::new();
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if lines.len() < LINES_KEPT {
                        lines.push(line);
                    }
                }
            }
            let pgid = child.id();
            let status = child.wait();
            // Reaped: no signal to this group from now on, but from here: what the render
            // left in its group (a process forked while a kill was on its way) goes too, before
            // the render ends.
            lock(&reaper.state).pgid = None;
            if !process::kill_group_until_empty(pgid, process::KILL_GUARD) {
                eprintln!("notedown: the process group {pgid} is still there after the kill");
            }
            let errors = errors.join().unwrap_or_default();
            let _ = tx.send((status, lines, errors));
        });
        {
            let (ended, late) = match rx.recv_timeout(RENDER_TIMEOUT) {
                Ok(ended) => (Some(ended), false),
                Err(_) => {
                    {
                        let state = lock(&slot.state);
                        if let Some(pgid) = state.pgid {
                            let _ = process::signal_group(pgid, libc::SIGKILL);
                            kill_until_empty_in_background(pgid);
                        }
                    }
                    (rx.recv_timeout(GONE_WAIT).ok(), true)
                }
            };
            if let Some(why) = lock(&slot.state).stopped {
                return Err(why.to_owned());
            }
            if late {
                return Err(TOO_LONG.to_owned());
            }
            let Some((status, lines, errors)) = ended else {
                return Err(NO_IMAGE.to_owned());
            };
            let status = status.map_err(|_| NO_IMAGE.to_owned())?;
            if !status.success() {
                return Err(format!(
                    "{NO_IMAGE} {}",
                    process::error_text(&errors, status)
                ));
            }
            self.keep_page(&tmp, &lines, md5, index)
        }
    }

    /// Moves the PNG the render printed into the cache and answers it as a data URL.
    fn keep_page(
        &self,
        tmp: &Path,
        lines: &[String],
        md5: &str,
        index: usize,
    ) -> Result<String, String> {
        let printed = lines
            .iter()
            .find(|l| !l.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| NO_IMAGE.to_owned())?;
        // Only a file the run wrote into its own folder.
        if printed.parent() != Some(tmp) {
            return Err(NO_IMAGE.to_owned());
        }
        let bytes = read_regular(&printed).ok_or_else(|| NO_IMAGE.to_owned())?;
        if !bytes.starts_with(PNG_SIGNATURE) {
            return Err(NO_IMAGE.to_owned());
        }
        let target = self.page_file(md5, index);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).map_err(|_| NO_CACHE.to_owned())?;
        }
        fs::rename(&printed, &target).map_err(|_| NO_CACHE.to_owned())?;
        Ok(data_url(&bytes))
    }

    /// The bytes of the note with `md5`, cached at `notes/<md5>.note`: checked once a run,
    /// downloaded when they are not there or not right.
    fn note_file(&self, entry: &Entry, md5: &str, slot: &Slot) -> Result<PathBuf, String> {
        let notes = self.cache.join("notes");
        let file = notes.join(format!("{md5}.note"));
        let gate = lock(&self.fetching)
            .entry(md5.to_owned())
            .or_default()
            .clone();
        let result = {
            let _one = lock(&gate);
            self.note_file_once(entry, md5, slot, &notes, &file)
        };
        let mut fetching = lock(&self.fetching);
        // Two holders: the map and this call.
        if fetching
            .get(md5)
            .is_some_and(|g| Arc::ptr_eq(g, &gate) && Arc::strong_count(g) == 2)
        {
            fetching.remove(md5);
        }
        result
    }

    fn note_file_once(
        &self,
        entry: &Entry,
        md5: &str,
        slot: &Slot,
        notes: &Path,
        file: &Path,
    ) -> Result<PathBuf, String> {
        match fs::symlink_metadata(file) {
            Ok(meta) if meta.is_file() => {
                if lock(&self.table).verified.contains(md5) {
                    touch(file);
                    return Ok(file.to_owned());
                }
                let right = open_regular(file)
                    .and_then(|mut f| md5_of(&mut f))
                    .is_ok_and(|m| m == md5);
                if right {
                    lock(&self.table).verified.insert(md5.to_owned());
                    touch(file);
                    return Ok(file.to_owned());
                }
                let _ = fs::remove_file(file);
            }
            Ok(meta) if meta.is_dir() => return Err(NO_CACHE.to_owned()),
            // A symlink or another thing in the note's place.
            Ok(_) => {
                let _ = fs::remove_file(file);
            }
            Err(_) => {}
        }
        fs::create_dir_all(notes).map_err(|_| NO_CACHE.to_owned())?;
        let job = self.next_job.fetch_add(1, Ordering::SeqCst) + 1;
        let part = notes.join(format!(".{md5}.{job}.part"));
        let mut out = create_new(&part).map_err(|_| NO_CACHE.to_owned())?;
        let mut leftover = Leftover::file(&part);
        let session = self.library.session().map_err(|e| e.to_string())?;
        let stopped = || session.is_cancelled() || lock(&slot.state).stopped.is_some();
        let count = AtomicU64::new(0);
        let size = entry.size.unwrap_or(CACHE_LIMIT);
        let ended = watched(
            fetch(&session, entry.id, md5, size, &mut out, &count),
            &count,
            u64::MAX,
            &self.clock,
            &stopped,
            &mut |_| {},
        )?;
        match ended {
            // No review waits anymore, or the library bound again (a new login or server).
            Watched::Cancelled => Err(match lock(&slot.state).stopped {
                Some(why) => why.to_owned(),
                None => crate::library::LibraryError::Changed.to_string(),
            }),
            Watched::Stalled => Err(RestError::Stalled.to_string()),
            Watched::Done(Err(Stop::Rest(e))) => {
                self.library.settle(&session, &Err::<(), _>(e.clone()));
                Err(e.to_string())
            }
            Watched::Done(Err(Stop::Changed)) => Err(CHANGED.to_owned()),
            Watched::Done(Err(Stop::NotWhole)) => Err(NOT_WHOLE.to_owned()),
            Watched::Done(Err(Stop::Disk)) => Err(NO_CACHE.to_owned()),
            Watched::Done(Ok(())) => {
                self.library.settle(&session, &Ok::<(), RestError>(()));
                match md5_of(&mut out) {
                    Ok(m) if m == md5 => {}
                    Ok(_) => return Err(NOT_WHOLE.to_owned()),
                    Err(_) => return Err(NO_CACHE.to_owned()),
                }
                fs::rename(&part, file).map_err(|_| NO_CACHE.to_owned())?;
                leftover.keep();
                lock(&self.table).verified.insert(md5.to_owned());
                Ok(file.to_owned())
            }
        }
    }

    /// Removes what runs that did not clean up left: temp folders in `pages/` and temp files
    /// in `notes/`. Once a run, before its first render or download.
    fn sweep_once(&self) {
        let mut swept = lock(&self.swept);
        if *swept {
            return;
        }
        *swept = true;
        if let Ok(read) = fs::read_dir(self.cache.join("pages")) {
            for entry in read.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let temp = name.split_once(".tmp-").is_some_and(|(md5, job)| {
                    is_md5(md5) && job.bytes().all(|b| b.is_ascii_digit())
                });
                if !temp {
                    continue;
                }
                let path = entry.path();
                match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => {
                        let _ = fs::remove_dir_all(&path);
                    }
                    Ok(_) => {
                        let _ = fs::remove_file(&path);
                    }
                    Err(_) => {}
                }
            }
        }
        if let Ok(read) = fs::read_dir(self.cache.join("notes")) {
            for entry in read.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let temp = name
                    .strip_prefix('.')
                    .and_then(|n| n.strip_suffix(".part"))
                    .and_then(|n| n.split_once('.'))
                    .is_some_and(|(md5, job)| {
                        is_md5(md5) && job.bytes().all(|b| b.is_ascii_digit())
                    });
                let path = entry.path();
                if temp && fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    /// Trims `notes/` and `pages/` together to the limit, the oldest files first. Only the
    /// cache's own files count: `notes/<md5>.note` and `pages/<md5>/<n>.png`. Files of a note
    /// that is being rendered stay.
    fn trim(&self) {
        let busy: HashSet<String> = {
            let table = lock(&self.table);
            table
                .jobs
                .keys()
                .map(|(md5, _)| md5.clone())
                .chain(table.opens.values().flatten().map(|(md5, _)| md5.clone()))
                .collect()
        };
        let mut files: Vec<(SystemTime, PathBuf, u64, String)> = Vec::new();
        let mut folders = Vec::new();
        let regular = |path: &Path| {
            fs::symlink_metadata(path)
                .ok()
                .filter(fs::Metadata::is_file)
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
        };
        if let Ok(read) = fs::read_dir(self.cache.join("notes")) {
            for entry in read.flatten() {
                let name = entry.file_name();
                let Some(md5) = name.to_str().and_then(|n| n.strip_suffix(".note")) else {
                    continue;
                };
                if !is_md5(md5) {
                    continue;
                }
                let path = entry.path();
                if let Some((mtime, len)) = regular(&path) {
                    files.push((mtime, path, len, md5.to_owned()));
                }
            }
        }
        if let Ok(read) = fs::read_dir(self.cache.join("pages")) {
            for folder in read.flatten() {
                let name = folder.file_name();
                let Some(md5) = name.to_str().filter(|n| is_md5(n)) else {
                    continue;
                };
                let dir = folder.path();
                if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                let Ok(pages) = fs::read_dir(&dir) else {
                    continue;
                };
                for page in pages.flatten() {
                    let name = page.file_name();
                    let ours = name
                        .to_str()
                        .and_then(|n| n.strip_suffix(".png"))
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
                    let path = page.path();
                    if let Some((mtime, len)) = regular(&path).filter(|_| ours) {
                        files.push((mtime, path, len, md5.to_owned()));
                    }
                }
                if !busy.contains(md5) {
                    folders.push((dir, md5.to_owned()));
                }
            }
        }
        let mut total: u64 = files.iter().map(|f| f.2).sum();
        if total > self.limit {
            files.sort();
            for (_, path, len, md5) in files {
                if total <= self.limit {
                    break;
                }
                if busy.contains(&md5) {
                    continue;
                }
                // Checked again right before the remove: a job that started since keeps its
                // note's files.
                let table = lock(&self.table);
                if table.uses(&md5) {
                    continue;
                }
                if fs::remove_file(&path).is_ok() {
                    total -= len;
                }
            }
        }
        // A page folder left empty goes (it fails while a page is in it); not one a render is
        // about to use.
        for (dir, md5) in folders {
            let table = lock(&self.table);
            if !table.uses(&md5) {
                let _ = fs::remove_dir(dir);
            }
        }
    }
}

#[cfg(test)]
mod tests;
