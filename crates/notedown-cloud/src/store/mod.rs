//! The file store of one account: the only writer of `accounts/<slug>/files/` (the device
//! tree), the id registry (`ids.json`, `ids.jsonl`) and the MD5 cache (`hashes.json`).
//!
//! Every change runs under the store lock `accounts/<slug>/.store.lock` (std `File::lock`,
//! exclusive), in the server and in every CLI command. Mutating methods take a [`StoreGuard`],
//! so the lock cannot be forgotten. Each `lock()` reloads the registry and the cache when another
//! process changed them on disk.
//!
//! Names are stored byte-exact. Lookups compare names byte by byte against the folder entries,
//! so a case-insensitive file system (macOS APFS) cannot match a different spelling. The device
//! routes first map a wire path to the stored spelling ([`Store::stored_path`]): names compare
//! without case and Unicode normalization per folder, as on Ratta's server.

mod commit;
mod hashes;
mod ids;
mod list;
mod path;
pub mod recycle;
pub mod retention;
mod scan;
pub mod trees;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

pub use commit::{
    CONTENT_HASH_MISMATCH, Commit, CrashPoint, LockTry, PENDING_FILE, PendingOp, PlannedFolder,
    PruneAllReport, PutOptions, RECYCLE_DIR, Recovery, STALE_TEMP_AGE, SideWrite, WriteGuard,
    prune_all_recycle, recover_all, sweep_all_temps,
};
pub(crate) use commit::{recover_all_keeping, sweep_all_temps_with};
pub use ids::{FIRST_ID, registry_loads};
pub use list::Entry;
pub use path::WirePath;
pub use recycle::{
    IndexLine, ItemKind, PruneFailure, PrunePlan, PruneReport, RECYCLE_INDEX, RecycleEntry,
    RecycleItem, RecycleReason, recycle_items,
};
pub use scan::{SCAN_HOLD_FILE, ScanHold, ScanOptions, ScanReport};

use hashes::HashCache;
pub(crate) use hashes::mtime_ms;
use ids::{Op, Registry};
use list::Found;

use crate::auth::{Accounts, validate_slug};
use crate::{Error, Result, io_err};

/// `accounts/<slug>/.store.lock`.
pub const STORE_LOCK_FILE: &str = ".store.lock";
/// `accounts/<slug>/files/`: the device tree.
pub const FILES_DIR: &str = "files";

/// The device's native folders and their parents, parents first (`docs/sync-protocol.md` 3.3).
/// They are created by [`Store::create_native_folders`] (with `folder.created` events) and are
/// never pruned.
pub const NATIVE_FOLDERS: [&str; 8] = [
    "NOTE",
    "NOTE/Note",
    "NOTE/MyStyle",
    "DOCUMENT",
    "DOCUMENT/Document",
    "EXPORT",
    "SCREENSHOT",
    "INBOX",
];

/// True for a native folder or one of their parents.
pub fn is_native_folder(path: &WirePath) -> bool {
    let display = path.display();
    NATIVE_FOLDERS.contains(&display.as_str())
}

/// File or folder; `"file"` / `"folder"` on the wire and on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Folder,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh temp path in `dir`: `.nd-<pid>-<counter>.tmp`. The name does not grow with the
/// target's name (device names can be 255 bytes), it is unique within the process and across
/// processes, and listings hide it (a dot-file).
pub fn temp_path_in(dir: &Path) -> PathBuf {
    dir.join(format!(
        ".nd-{}-{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Create a new temp file in `dir` with `create_new`. A name that exists (a stale temp file of
/// a crashed process with the same pid) is left alone and the next counter is tried.
pub fn create_temp(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let mut last = None;
    for _ in 0..1000 {
        let tmp = temp_path_in(dir);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("the loop ran"))
}

/// Write `bytes` to `path` atomically with a short temp name ([`temp_path_in`]): write, fsync,
/// rename, fsync of the folder. The folder must exist.
pub fn write_atomic_short(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_short_unsynced(path, bytes)?;
    crate::fsutil::fsync_dir(path.parent().unwrap_or_else(|| Path::new(".")))
}

/// [`write_atomic_short`] without the fsync of the folder: the file is synced before the
/// rename, so the path holds the old or the new bytes, never a torn file. The rename is durable
/// only after the caller syncs the folder.
pub(crate) fn write_atomic_short_unsynced(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let (tmp, mut f) = create_temp(dir).map_err(io_err(dir))?;
    // From here on the temp file is ours: only it is removed on failure.
    let mut write = || -> std::io::Result<()> {
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_err(path)(e)
    })
}

/// The comparison key for name collisions, Unicode canonical caseless matching: NFD, full case
/// folding (`ß` and `SS`, final sigma, ligatures such as `ﬁ`), then NFC.
pub(crate) fn fold(name: &str) -> String {
    let nfd: String = name.nfd().collect();
    unicase::UniCase::new(nfd).to_folded_case().nfc().collect()
}

/// What [`Store::stored_path`] found for a wire path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// The path with the stored spelling of every segment that names an entry; the first
    /// `existing` segments name entries on disk, the others keep the wire spelling.
    Path { path: WirePath, existing: usize },
    /// In `folder` no entry has the name exactly, and two or more have it without case.
    Ambiguous {
        folder: WirePath,
        names: Vec<String>,
    },
}

/// How a name matches the entries of one folder.
#[derive(Debug, PartialEq, Eq)]
enum NameMatch {
    /// An entry has exactly these bytes.
    Exact,
    /// No exact entry; this one entry has the same caseless form.
    Folded(String),
    Missing,
    /// No exact entry; these entries have the same caseless form.
    Ambiguous(Vec<String>),
}

/// Match `name` against the entries of a folder (name, and whether it is a real file or
/// folder). An exact entry always wins. Else the real files and folders with the same caseless
/// form ([`fold`]) count; dot-names never match that way.
fn match_name<'a>(entries: impl IntoIterator<Item = (&'a str, bool)>, name: &str) -> NameMatch {
    let key = fold(name);
    let mut folded = Vec::new();
    for (entry, listable) in entries {
        if entry == name {
            return NameMatch::Exact;
        }
        if listable && !entry.starts_with('.') && fold(entry) == key {
            folded.push(entry.to_string());
        }
    }
    match folded.len() {
        0 => NameMatch::Missing,
        1 => NameMatch::Folded(folded.remove(0)),
        _ => NameMatch::Ambiguous(folded),
    }
}

/// Identity of a file on disk, to notice changes by another process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Stamp(Option<(u64, u64, u64)>);

fn stamp(path: &Path) -> Stamp {
    match std::fs::metadata(path) {
        Ok(m) => {
            #[cfg(unix)]
            let ino = std::os::unix::fs::MetadataExt::ino(&m);
            #[cfg(not(unix))]
            let ino = 0;
            Stamp(Some((m.len(), hashes::mtime_ns(&m), ino)))
        }
        Err(_) => Stamp(None),
    }
}

/// What the tree check found: whether the account counts as imported, and then `files/` and
/// every native folder with its metadata (a label for messages first).
struct TreeCheck {
    imported: bool,
    folders: Vec<(String, std::fs::Metadata)>,
}

/// The walk of a scan that applied ([`Store::scan_walked`]): the tree check before it and what
/// it found. A listing of the same scope under the same guard reuses it
/// ([`Store::list_walked`]) instead of walking the tree again; the scan changes no file.
pub(crate) struct Walk {
    found: Vec<Found>,
    before: TreeCheck,
    /// The scope of the walk and whether it went below its direct children. A listing of
    /// another scope or depth does not reuse it.
    scope: WirePath,
    recursive: bool,
}

struct State {
    ids: Registry,
    /// Stamps of `ids.json` and `ids.jsonl` when `ids` was loaded; `None` forces a reload.
    ids_stamp: Option<(Stamp, Stamp)>,
    hashes: HashCache,
    hashes_stamp: Option<Stamp>,
}

/// The held store lock. Dropping it releases the lock. It is `Send`, so it can be held across
/// an `await` or moved into a blocking task.
#[derive(Debug)]
pub struct StoreGuard {
    _file: std::fs::File,
    account_dir: PathBuf,
}

/// The file store of one account.
pub struct Store {
    slug: String,
    account_dir: PathBuf,
    files: PathBuf,
    state: Mutex<State>,
    /// Opened by `notedown cloud import`: [`Store::lock`] may create a missing `files/` and
    /// does not check the tree behind the `imported` marker (the import rebuilds it).
    import_mode: bool,
    /// Print a line on stderr when the store lock is busy before waiting for it.
    announce_wait: bool,
    /// Whether the probe at open found that the file system keeps names byte-exact.
    names_preserved: bool,
    /// Test hook: runs right before a scan hashes a file ([`Store::set_scan_hash_hook`]).
    hash_hook: Mutex<Option<HashHook>>,
    /// What the walks cost ([`Store::walk_counts`]).
    counts: WalkCounts,
}

/// What [`Store::list_window`] found.
#[derive(Debug)]
pub(crate) enum Page {
    /// The entries of the page and whether more follow.
    Listed(Vec<Entry>, bool),
    NotFound,
    NotAFolder,
}

/// How many scans walked the disk and how many entries the walks stat'ed, for
/// [`Store::walk_counts`]. Two relaxed counters, nothing else.
#[derive(Debug, Default)]
struct WalkCounts {
    scans: AtomicU64,
    stats: AtomicU64,
}

impl WalkCounts {
    fn scanned(&self) {
        self.scans.fetch_add(1, Ordering::Relaxed);
    }

    fn stated(&self, n: usize) {
        self.stats.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// See [`Store::set_scan_hash_hook`].
type HashHook = Arc<dyn Fn(&WirePath) + Send + Sync>;

/// Take the store lock file of an account (`accounts/<slug>/.store.lock`, exclusive). It tries
/// first; when another process holds it and `announce` names the account, it prints "waiting
/// for the store lock of account <slug>" on stderr, then blocks. No state is read and the tree
/// is not checked, so a damaged account can still be removed.
pub fn take_store_lock(account_dir: &Path, announce: Option<&str>) -> Result<std::fs::File> {
    for _ in 0..STORE_LOCK_TRIES {
        let file = open_store_lock(account_dir)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                if let Some(slug) = announce {
                    eprintln!("waiting for the store lock of account {slug}");
                }
                file.lock()
                    .map_err(io_err(&account_dir.join(STORE_LOCK_FILE)))?;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(io_err(&account_dir.join(STORE_LOCK_FILE))(e));
            }
        }
        if let Some(file) = still_there(account_dir, file)? {
            return Ok(file);
        }
    }
    Err(lock_keeps_changing(account_dir))
}

/// [`take_store_lock`] that gives up: it tries, then tries again every 50 ms until `wait` is
/// over. `None` when the lock is still busy then. Never prints anything.
pub fn take_store_lock_within(
    account_dir: &Path,
    wait: std::time::Duration,
) -> Result<Option<std::fs::File>> {
    let path = account_dir.join(STORE_LOCK_FILE);
    let deadline = std::time::Instant::now() + wait;
    for _ in 0..STORE_LOCK_TRIES {
        let file = open_store_lock(account_dir)?;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(io_err(&path)(e)),
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep((deadline - now).min(std::time::Duration::from_millis(50)));
        }
        if let Some(file) = still_there(account_dir, file)? {
            return Ok(Some(file));
        }
    }
    Err(lock_keeps_changing(account_dir))
}

/// How often the store lock is taken again when its file was replaced.
const STORE_LOCK_TRIES: usize = 5;

/// The account of `account_dir`, for [`Error::NoAccount`].
fn slug_of(account_dir: &Path) -> String {
    account_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Opens (creates) `.store.lock` in an existing account folder; [`Error::NoAccount`] when the
/// folder is gone. The folder itself is never created.
fn open_store_lock(account_dir: &Path) -> Result<std::fs::File> {
    let path = account_dir.join(STORE_LOCK_FILE);
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::NoAccount(slug_of(account_dir)))
        }
        Err(e) => Err(io_err(&path)(e)),
    }
}

/// The held lock `file` when it is still the `.store.lock` of `account_dir`. `remove` renames
/// the account folder away while it holds the lock: a waiter then holds a lock that excludes
/// nobody, and its tree check would recreate `files/` at the old path. Gone:
/// [`Error::NoAccount`]; replaced by another file: `None` (take it again).
fn still_there(account_dir: &Path, file: std::fs::File) -> Result<Option<std::fs::File>> {
    let path = account_dir.join(STORE_LOCK_FILE);
    match crate::fsutil::is_same_file(&file, &path).map_err(io_err(&path))? {
        Some(true) => Ok(Some(file)),
        Some(false) => Ok(None),
        None => Err(Error::NoAccount(slug_of(account_dir))),
    }
}

fn lock_keeps_changing(account_dir: &Path) -> Error {
    io_err(&account_dir.join(STORE_LOCK_FILE))(std::io::Error::other(
        "the lock file keeps changing, try again",
    ))
}

/// The name of the normalization probe: `é` as one code point (NFC). A file system that
/// stores names in NFD (HFS+, some network shares) gives it back as `e` plus a combining
/// accent.
const PROBE_NFC_NAME: &str = "\u{e9}";

/// Create a file with an NFC name in `dir` and check that the folder lists it with exactly
/// these bytes. The file is removed again. An error of the probe itself counts as preserved
/// (nothing is known); only a changed name is a finding. Public for tests.
#[doc(hidden)]
pub fn probe_names_preserved(dir: &Path) -> bool {
    let name = format!(
        ".nd-probe-{}-{}-{PROBE_NFC_NAME}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(&name);
    if std::fs::File::create(&path).is_err() {
        return true;
    }
    let found = std::fs::read_dir(dir).map(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().as_encoded_bytes() == name.as_bytes())
    });
    let _ = std::fs::remove_file(&path);
    found.unwrap_or(true)
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("account_dir", &self.account_dir)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open the store of account `slug` in the data folder `data_dir`. Under the store lock it
    /// creates `files/` if needed (never for an account with an `imported` marker, see
    /// [`Store::lock`]) and compacts `ids.jsonl` into `ids.json`. It also probes once whether the
    /// file system keeps names byte-exact and warns on stderr if not
    /// ([`Store::names_preserved`]).
    pub fn open(data_dir: &Path, slug: &str) -> Result<Store> {
        Self::open_with(data_dir, slug, false, false, true)
    }

    /// [`Store::open`] for `notedown cloud import`: the import may create a missing `files/` and
    /// rebuild the tree of an account whose store is damaged.
    pub fn open_for_import(data_dir: &Path, slug: &str) -> Result<Store> {
        Self::open_with(data_dir, slug, true, false, true)
    }

    /// [`Store::open`] that prints "waiting for the store lock of account <slug>" when the lock
    /// is busy (a running import), for the recovery of every command.
    pub fn open_announced(data_dir: &Path, slug: &str) -> Result<Store> {
        Self::open_with(data_dir, slug, false, true, true)
    }

    /// [`Store::open`] that leaves `ids.jsonl` as it is (no compaction), for a dry run that must
    /// write nothing.
    pub fn open_no_compact(data_dir: &Path, slug: &str) -> Result<Store> {
        Self::open_with(data_dir, slug, false, false, false)
    }

    /// [`Store::open`] that takes no lock and compacts nothing, so it never waits for the
    /// server (`notedown cloud diagnose`). The name probe runs. Lock with
    /// [`Store::lock_unless_pending_within`].
    pub fn open_unlocked(data_dir: &Path, slug: &str) -> Result<Store> {
        Self::open_inner(data_dir, slug, false, false)
    }

    fn open_with(
        data_dir: &Path,
        slug: &str,
        import_mode: bool,
        announce_wait: bool,
        compact: bool,
    ) -> Result<Store> {
        let store = Self::open_inner(data_dir, slug, import_mode, announce_wait)?;
        let guard = store.lock()?;
        {
            let mut state = store.state();
            if compact && !Registry::log_is_empty(&store.account_dir) {
                state.ids.compact(&store.account_dir)?;
                state.ids_stamp = Some(store.ids_stamps());
            }
        }
        drop(guard);
        Ok(store)
    }

    fn open_inner(
        data_dir: &Path,
        slug: &str,
        import_mode: bool,
        announce_wait: bool,
    ) -> Result<Store> {
        validate_slug(slug)?;
        let account_dir = Accounts::new(data_dir).account_dir(slug);
        if !account_dir.is_dir() {
            return Err(Error::NoAccount(slug.to_string()));
        }
        let files = account_dir.join(FILES_DIR);
        let mut store = Store {
            slug: slug.to_string(),
            account_dir,
            files,
            state: Mutex::new(State {
                ids: Registry::default(),
                ids_stamp: None,
                hashes: HashCache::default(),
                hashes_stamp: None,
            }),
            import_mode,
            announce_wait,
            names_preserved: true,
            hash_hook: Mutex::new(None),
            counts: WalkCounts::default(),
        };
        store.names_preserved = probe_names_preserved(&store.account_dir);
        if !store.names_preserved {
            eprintln!("warning: {}", store.names_warning());
        }
        Ok(store)
    }

    /// Whether the file system of the data folder kept an NFC name byte-exact in the probe at
    /// open. When false, names from the device may come back in another spelling (HFS+ stores
    /// NFD), and lookups after a commit can fail.
    pub fn names_preserved(&self) -> bool {
        self.names_preserved
    }

    /// The warning for a file system that changes names (see [`Store::names_preserved`]).
    pub fn names_warning(&self) -> String {
        format!(
            "account {}: the file system of {} changes the Unicode normalization of file names; \
             keep the data folder on a file system that keeps names as they are (APFS on macOS)",
            self.slug,
            self.account_dir.display()
        )
    }

    /// The account slug.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The account folder (`accounts/<slug>`).
    pub fn account_dir(&self) -> &Path {
        &self.account_dir
    }

    /// The device tree (`accounts/<slug>/files`).
    pub fn files_dir(&self) -> &Path {
        &self.files
    }

    /// Where a wire path lives on disk (no existence check).
    pub fn fs_path(&self, path: &WirePath) -> PathBuf {
        path.to_fs(&self.files)
    }

    /// Take the store lock (exclusive, blocking) and bring the in-memory registry and hash
    /// cache up to date with the disk. A torn last line of `ids.jsonl` is cut here.
    ///
    /// The tree is checked first, so nothing is ever listed from a tree the server did not
    /// make (`Error::StoreDamaged`, fail closed):
    ///
    /// - `files/` must be a real folder, never a symlink or a file.
    /// - A missing `files/` is created only for an account without an `imported` marker, or by
    ///   the import ([`Store::open_for_import`]).
    /// - With the marker, every native folder must be a real folder ([`NATIVE_FOLDERS`]). The
    ///   server never deletes or moves them, so a missing one means the tree broke outside it.
    ///
    /// [`Store::list`] repeats the check around its reads, for a tree that breaks later.
    pub fn lock(&self) -> Result<StoreGuard> {
        let announce = self.announce_wait.then_some(self.slug.as_str());
        let file = take_store_lock(&self.account_dir, announce)?;
        self.locked(file)
    }

    /// [`Store::lock`] that gives up after `wait` ([`take_store_lock_within`]): `None` when the
    /// lock is still busy then.
    pub fn lock_within(&self, wait: std::time::Duration) -> Result<Option<StoreGuard>> {
        match take_store_lock_within(&self.account_dir, wait)? {
            Some(file) => self.locked(file).map(Some),
            None => Ok(None),
        }
    }

    /// The rest of [`Store::lock`] once the lock file is held.
    fn locked(&self, file: std::fs::File) -> Result<StoreGuard> {
        let guard = StoreGuard {
            _file: file,
            account_dir: self.account_dir.clone(),
        };
        self.check_tree(true)?;
        let mut state = self.state();
        self.refresh_ids(&mut state, true)?;
        let hs = stamp(&self.account_dir.join(hashes::HASHES_FILE));
        if state.hashes_stamp != Some(hs) {
            state.hashes = HashCache::load(&self.account_dir)?;
            state.hashes_stamp = Some(hs);
        }
        drop(state);
        Ok(guard)
    }

    fn damaged(&self, reason: String) -> Error {
        Error::StoreDamaged {
            slug: self.slug.clone(),
            reason,
        }
    }

    /// Whether `account.json` carries the `imported` marker.
    fn has_import_marker(&self) -> Result<bool> {
        let path = self.account_dir.join(crate::auth::ACCOUNT_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<crate::auth::Account>(&bytes)
                .map(|a| a.imported.is_some())
                .map_err(|e| Error::Config {
                    path,
                    message: e.to_string(),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// The repair advice for a missing `files/` (`path` is the root) or native folder.
    fn missing_reason(&self, path: &WirePath) -> String {
        if path.is_root() {
            format!(
                "{} is missing although the account was imported. Put the real folder back, \
                 or import the device again with --replace",
                self.files.display()
            )
        } else {
            format!(
                "the device folder {} is missing or no real folder. Put it back, \
                 or import the device again with --replace",
                path.display()
            )
        }
    }

    /// For [`list::walk`]: a folder that vanishes during the walk is damage if the tree check
    /// would refuse its absence (`files/` or a native folder of an imported account), else a
    /// deletion.
    fn vanished_damage(&self, imported: bool) -> impl Fn(&WirePath) -> Option<Error> + '_ {
        move |path: &WirePath| {
            (imported && (path.is_root() || is_native_folder(path)))
                .then(|| self.damaged(self.missing_reason(path)))
        }
    }

    /// The tree check of [`Store::lock`] (the lock is held). `create_missing` creates a missing
    /// `files/` where that is allowed (no marker, or an import). [`Store::list`] runs it without
    /// that before and after it reads the tree and compares the two [`TreeCheck`]s, so a tree
    /// that vanished or was swapped meanwhile is never listed.
    fn check_tree(&self, create_missing: bool) -> Result<TreeCheck> {
        let imported = !self.import_mode && self.has_import_marker()?;
        let mut check = TreeCheck {
            imported,
            folders: Vec::new(),
        };
        match std::fs::symlink_metadata(&self.files) {
            Ok(m) if m.file_type().is_dir() => {
                if imported {
                    check.folders.push((self.files.display().to_string(), m));
                }
            }
            Ok(m) if m.file_type().is_symlink() => {
                return Err(self.damaged(format!(
                    "{} is a symlink. Remove the link and put the real folder back in its place. \
                     import --replace does not help here",
                    self.files.display()
                )));
            }
            Ok(_) => {
                return Err(self.damaged(format!(
                    "{} is not a folder. Remove it and put the real folder back in its place",
                    self.files.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if imported {
                    return Err(self.damaged(self.missing_reason(&WirePath::root())));
                }
                if create_missing {
                    std::fs::create_dir_all(&self.files).map_err(io_err(&self.files))?;
                    crate::fsutil::fsync_dir(&self.account_dir)?;
                }
            }
            Err(e) => return Err(io_err(&self.files)(e)),
        }
        if imported {
            for folder in NATIVE_FOLDERS {
                let path = WirePath::parse_named(folder)?;
                match list::resolve(&self.files, &path)? {
                    Some(f) if f.kind == Kind::Folder => {
                        check
                            .folders
                            .push((format!("the device folder {folder}"), f.meta));
                    }
                    _ => return Err(self.damaged(self.missing_reason(&path))),
                }
            }
        }
        Ok(check)
    }

    /// Refuse a tree whose `files/` or native folders are other folders at `after` than at
    /// `before` (replaced, or removed and put back, while the tree was read).
    fn check_same_tree(&self, before: &TreeCheck, after: &TreeCheck) -> Result<()> {
        for (label, meta) in &before.folders {
            let now = after.folders.iter().find(|(l, _)| l == label);
            if !now.is_some_and(|(_, m)| same_file(meta, m)) {
                return Err(self.damaged(format!(
                    "{label} was replaced while it was read. Put the real folder back, \
                     or import the device again with --replace"
                )));
            }
        }
        Ok(())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ids_stamps(&self) -> (Stamp, Stamp) {
        (
            stamp(&self.account_dir.join(ids::IDS_FILE)),
            stamp(&self.account_dir.join(ids::LOG_FILE)),
        )
    }

    /// Reload the registry if the files changed since the last load. Without the lock a
    /// concurrent compaction may swap the files mid-read; the stamps before and after the read
    /// must match, else it reads again.
    fn refresh_ids(&self, state: &mut State, locked: bool) -> Result<()> {
        for _ in 0..16 {
            let before = self.ids_stamps();
            if state.ids_stamp == Some(before) {
                return Ok(());
            }
            let reg = Registry::load(&self.account_dir, locked)?;
            let after = self.ids_stamps();
            if locked || after == before {
                state.ids = reg;
                state.ids_stamp = Some(after);
                return Ok(());
            }
        }
        Err(Error::Invalid {
            what: "id registry",
            message: "it keeps changing while it is read".into(),
        })
    }

    /// Fold `ids.jsonl` into `ids.json` under the store lock, as [`Store::open`] does, and
    /// keep the stamps current, so the next use does not load the registry again. Nothing
    /// happens when the log is empty.
    pub(crate) fn compact_ids(&self, guard: &StoreGuard) -> Result<()> {
        self.check_guard(guard)?;
        if Registry::log_is_empty(&self.account_dir) {
            return Ok(());
        }
        let mut state = self.state();
        self.refresh_ids(&mut state, true)?;
        state.ids.compact(&self.account_dir)?;
        state.ids_stamp = Some(self.ids_stamps());
        Ok(())
    }

    /// An error unless `guard` is the lock of this store.
    pub(crate) fn check_guard(&self, guard: &StoreGuard) -> Result<()> {
        if guard.account_dir == self.account_dir {
            Ok(())
        } else {
            Err(Error::Invalid {
                what: "store guard",
                message: format!(
                    "the lock of {} does not cover {}",
                    guard.account_dir.display(),
                    self.account_dir.display()
                ),
            })
        }
    }

    /// Append registry ops and keep the stamps current. A failed append forces a reload at the
    /// next `lock()` (which cuts a torn line).
    fn append(&self, state: &mut State, ops: &[Op]) -> Result<()> {
        let result = state.ids.append(&self.account_dir, ops);
        state.ids_stamp = match result {
            Ok(()) => Some(self.ids_stamps()),
            Err(_) => None,
        };
        result
    }

    // ------------------------------------------------------------------ id registry

    /// The id of `path`, if it has one. Reads the registry from disk when another process
    /// changed it; no lock needed.
    pub fn id_of(&self, path: &WirePath) -> Result<Option<u64>> {
        let mut state = self.state();
        self.refresh_ids(&mut state, false)?;
        Ok(state.ids.id_of(path))
    }

    /// The path of `id`, if the id is known.
    pub fn path_of(&self, id: u64) -> Result<Option<WirePath>> {
        Ok(self.kind_and_path_of(id)?.map(|(_, p)| p))
    }

    /// Every file the id registry knows: id and path, by id. Touches no disk and changes
    /// nothing: unlike [`Store::list`] it assigns no ids and neither reads nor writes the MD5
    /// cache, so a scan after it still sees every change made by hand. A file may be missing on
    /// disk, and a file on disk may be missing here, until the next scan.
    pub fn registered_files(&self, guard: &StoreGuard) -> Result<Vec<(u64, WirePath)>> {
        self.check_guard(guard)?;
        let state = self.state();
        Ok(state
            .ids
            .rows()
            .filter(|(_, kind, _)| *kind == Kind::File)
            .map(|(id, _, path)| (id, path.clone()))
            .collect())
    }

    /// The kind and path of `id`, if the id is known.
    pub fn kind_and_path_of(&self, id: u64) -> Result<Option<(Kind, WirePath)>> {
        let mut state = self.state();
        self.refresh_ids(&mut state, false)?;
        Ok(state.ids.get(id).cloned())
    }

    /// The id of `path`; a new one (never used before) if it has none. A known path of the same
    /// kind keeps its id. A known path of the other kind (a file replaced by a folder) gets a
    /// new id.
    pub fn assign(&self, guard: &StoreGuard, path: &WirePath, kind: Kind) -> Result<u64> {
        self.check_guard(guard)?;
        let mut state = self.state();
        let (id, op) = plan_assign(&state.ids, path, kind, state.ids.next())?;
        if let Some(op) = op {
            self.append(&mut state, &[op])?;
        }
        Ok(id)
    }

    /// `id` now lives at `new_path`. For a folder every descendant moves with it. Refused
    /// (`Error::Invalid`, nothing written) for an unknown id, a folder moved into its own
    /// subtree, and a target path that holds another id: remove that one first.
    pub fn moved(&self, guard: &StoreGuard, id: u64, new_path: &WirePath) -> Result<()> {
        self.check_guard(guard)?;
        if new_path.is_root() {
            return Err(Error::Invalid {
                what: "path",
                message: "cannot move to the root".into(),
            });
        }
        let mut state = self.state();
        let Some((_, old)) = state.ids.get(id).cloned() else {
            return Err(unknown_id(id));
        };
        if &old == new_path {
            return Ok(());
        }
        if let Some(message) = state.ids.check_move(id, new_path) {
            return Err(Error::Invalid {
                what: "move",
                message,
            });
        }
        self.append(
            &mut state,
            &[Op::Move {
                id,
                path: new_path.display(),
            }],
        )?;
        state.hashes.moved(&old, new_path);
        self.flush_hashes(&mut state)?;
        Ok(())
    }

    /// `id` is gone. For a folder every descendant id is dropped too. Ids are never reused.
    pub fn removed(&self, guard: &StoreGuard, id: u64) -> Result<()> {
        self.check_guard(guard)?;
        let mut state = self.state();
        let Some((_, old)) = state.ids.get(id).cloned() else {
            return Err(unknown_id(id));
        };
        self.append(&mut state, &[Op::Remove { id }])?;
        state.hashes.removed(&old);
        self.flush_hashes(&mut state)?;
        Ok(())
    }

    // ------------------------------------------------------------------ hashes

    /// The MD5 (lowercase hex) of the file at `path`, from `hashes.json` when its size and mtime
    /// are unchanged. A missing path or a folder is an error.
    pub fn md5_of(&self, guard: &StoreGuard, path: &WirePath) -> Result<String> {
        self.check_guard(guard)?;
        let found = match list::resolve(&self.files, path)? {
            Some(f) if f.kind == Kind::File => f,
            _ => {
                return Err(Error::Invalid {
                    what: "path",
                    message: format!("{path} is not a file in the store"),
                });
            }
        };
        let mut state = self.state();
        let md5 = state
            .hashes
            .md5(path, &found.fs_path, &found.meta)
            .map_err(io_err(&found.fs_path))?;
        self.flush_hashes(&mut state)?;
        Ok(md5)
    }

    /// Drop the cached MD5 of the file at `path` (a write replaced it). The next read hashes
    /// it again, and a scan caches it without an event, as for any file without a row.
    pub(super) fn forget_md5(&self, guard: &StoreGuard, path: &WirePath) -> Result<()> {
        self.check_guard(guard)?;
        let mut state = self.state();
        state.hashes.removed_file(path);
        self.flush_hashes(&mut state)
    }

    /// Cache the MD5 of the file at `path` that a put just wrote, with the size, mtime and ctime
    /// seen right after the rename. A later write changes the ctime, so a scan sees it.
    pub(super) fn remember_md5(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        written: commit::WrittenFile,
        md5: &str,
    ) -> Result<()> {
        self.check_guard(guard)?;
        // A new row stays in memory until the caller calls `flush_cache` (the CLI put and the
        // device upload do that before they answer). Until then a stop, or a reload after
        // another process wrote the file (see `lock`), drops it, and a later edit by hand isn't
        // journaled. A row that replaces one is written at once: the old row on disk would make
        // the next scan of another process (or after a crash) journal the put again as a change
        // by hand.
        let mut state = self.state();
        let replaced = state.hashes.cached(path).is_some();
        state.hashes.insert(
            path,
            hashes::HashRow::new(written.size, written.mtime_ns, written.ctime_ns, md5),
        );
        if replaced {
            self.flush_hashes(&mut state)?;
        }
        Ok(())
    }

    /// The cached MD5 of the file at `path` when its cache row is current for the file as it is
    /// in `meta` (same size, mtime and ctime); `None` otherwise. Touches no disk and changes
    /// nothing: no hashing, no cache row is added or written. For readers that must leave the
    /// store as it is (the render pass).
    pub fn current_md5(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        meta: &std::fs::Metadata,
    ) -> Result<Option<String>> {
        self.check_guard(guard)?;
        Ok(self.state().hashes.current(path, meta).map(str::to_string))
    }

    /// The cached MD5 of the file at `path`, without touching the disk (it may be stale). Test
    /// hook only: the integration tests read the cache with it.
    #[doc(hidden)]
    pub fn cached_md5(&self, guard: &StoreGuard, path: &WirePath) -> Result<Option<String>> {
        self.check_guard(guard)?;
        Ok(self.state().hashes.cached(path).map(str::to_string))
    }

    /// The number of scans that walked the disk and of the entries that the walks of scans,
    /// listings and windows stat'ed, since the store was opened. Test hook only: the paging
    /// tests measure what a page costs with it.
    #[doc(hidden)]
    pub fn walk_counts(&self) -> (u64, u64) {
        (
            self.counts.scans.load(Ordering::Relaxed),
            self.counts.stats.load(Ordering::Relaxed),
        )
    }

    /// Write the MD5 cache if a commit or a listing changed it (a put of a new file keeps its
    /// row in memory only). A put or a copy calls this before it answers, so the row of the
    /// file it wrote survives a stop and a cache write by another process.
    pub fn flush_cache(&self, guard: &StoreGuard) -> Result<()> {
        self.check_guard(guard)?;
        let mut state = self.state();
        self.flush_hashes(&mut state)
    }

    fn flush_hashes(&self, state: &mut State) -> Result<()> {
        state.hashes.flush(&self.account_dir)?;
        state.hashes_stamp = Some(stamp(&self.account_dir.join(hashes::HASHES_FILE)));
        Ok(())
    }

    // ------------------------------------------------------------------ listing

    /// The entry at `path`, or `None` if nothing listable is there (missing, hidden, the root,
    /// or a different spelling of an existing name). Assigns an id when it has none.
    pub fn entry(&self, guard: &StoreGuard, path: &WirePath) -> Result<Option<Entry>> {
        self.check_guard(guard)?;
        if path.is_root() {
            return Ok(None);
        }
        let Some(found) = list::resolve(&self.files, path)? else {
            return Ok(None);
        };
        let mut entries = self.entries(vec![found])?;
        Ok(entries.pop())
    }

    /// Open the file at `path` for reading, resolved by exact names like a listing (no other
    /// spelling, no symlink, nothing hidden). `None` when no such file is there. The handle stays
    /// valid after the guard is dropped, so a download streams without the store lock.
    pub fn open_file(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
    ) -> Result<Option<(std::fs::File, u64)>> {
        self.check_guard(guard)?;
        let Some(found) = list::resolve(&self.files, path)? else {
            return Ok(None);
        };
        if found.kind != Kind::File {
            return Ok(None);
        }
        match std::fs::File::open(&found.fs_path) {
            Ok(f) => {
                let len = f.metadata().map_err(io_err(&found.fs_path))?.len();
                Ok(Some((f, len)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&found.fs_path)(e)),
        }
    }

    /// The entries below `path` (direct children, or every descendant with `recursive`), flat
    /// and sorted by path segments, so a folder's descendants follow it. Folders are entries of
    /// their own. Dot-entries and everything outside `files/` never appear. A missing path or a
    /// file gives an empty list. Entries without an id get one here. A tree that broke after
    /// [`Store::lock`] checked it (`files/` or a native folder gone) is `Error::StoreDamaged`,
    /// never an empty or partial list.
    pub fn list(&self, guard: &StoreGuard, path: &WirePath, recursive: bool) -> Result<Vec<Entry>> {
        self.list_walked(guard, path, recursive, None)
    }

    /// [`Store::list`] that takes the walk of the scan of the same scope right before it under
    /// the same guard ([`Store::scan_walked`]), so the tree is walked once. `None`, or a walk of
    /// another scope or depth, walks it. The size and time of a reused walk come from the
    /// scan's walk, a moment older than a fresh walk, and the MD5 read later can be newer than
    /// them; the next listing corrects it.
    pub(crate) fn list_walked(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        recursive: bool,
        walk: Option<Walk>,
    ) -> Result<Vec<Entry>> {
        let entries = self.list_unpruned(guard, path, recursive, walk)?;
        if path.is_root() && recursive {
            self.prune_hashes(guard, &entries)?;
        }
        Ok(entries)
    }

    /// [`Store::list_walked`] without the prune of the MD5 cache that a listing of the whole
    /// tree does. A paged listing of the whole tree prunes ([`Store::prune_hashes`]) only when
    /// its first page is also its last.
    pub(crate) fn list_unpruned(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        recursive: bool,
        walk: Option<Walk>,
    ) -> Result<Vec<Entry>> {
        self.check_guard(guard)?;
        // `files/` or a native folder may vanish or be swapped by another process after the
        // lock checked the tree: an empty or partial listing would make the device delete its
        // files. So the tree is checked before the walk and again after the last read of
        // `files/` (the hashing in `entries`), and the folders must be the same ones. A reused
        // walk brings the check from before it.
        // A walk of another scope or depth is dropped: a top-level walk taken for a full
        // listing would make the device delete files and drop rows of the hash cache.
        let walk = walk.filter(|w| w.scope == *path && w.recursive == recursive);
        let (before, mut all) = match walk {
            Some(walk) => {
                // The scan's walk holds the scope itself too (a file or folder below the root).
                let mut all = walk.found;
                all.retain(|f| f.path != *path);
                (walk.before, all)
            }
            None => {
                let before = self.check_tree(false)?;
                let mut all = Vec::new();
                if let Some(found) = list::resolve(&self.files, path)?
                    && found.kind == Kind::Folder
                {
                    list::walk(
                        &found,
                        recursive,
                        &mut all,
                        &self.vanished_damage(before.imported),
                    )?;
                }
                self.counts.stated(all.len());
                (before, all)
            }
        };
        all.sort_by(|a, b| a.path.cmp(&b.path));
        let entries = self.entries(all)?;
        let after = self.check_tree(false)?;
        self.check_same_tree(&before, &after)?;
        Ok(entries)
    }

    /// Keep only the MD5 cache rows of the files in `entries`, the listing of the whole tree,
    /// and write the cache. Never with a part of the tree: the rows of every other file would
    /// be lost, and the next listing would hash them all again.
    pub(crate) fn prune_hashes(&self, guard: &StoreGuard, entries: &[Entry]) -> Result<()> {
        self.check_guard(guard)?;
        let seen: HashSet<String> = entries
            .iter()
            .filter(|e| e.tag == Kind::File)
            .map(|e| e.path_display.clone())
            .collect();
        let mut state = self.state();
        state.hashes.retain(&seen);
        self.flush_hashes(&mut state)
    }

    /// Whether a scan saw `f` as it is: it has an id of its kind, and a file has a current
    /// cache row. Else it was made or changed by hand since.
    fn seen(&self, f: &Found) -> bool {
        let state = self.state();
        let known = state
            .ids
            .id_of(&f.path)
            .and_then(|id| state.ids.get(id))
            .is_some_and(|(k, _)| *k == f.kind);
        known && (f.kind == Kind::Folder || state.hashes.is_current(&f.path, &f.meta))
    }

    /// One page of the listing of the folder `path` (direct children, or every descendant with
    /// `recursive`; only folders with `folders_only`): the up to `limit` entries that come after
    /// `after` in the order of [`Store::list`], and whether more follow. `after` must lie below
    /// `path` (a direct child without `recursive`); it need not exist any more. `None` starts
    /// at the beginning.
    ///
    /// It walks a window, not the folder ([`list::walk_window`]): the names of the folders on
    /// the anchor's way and after it, and a stat of the `limit + 1` entries it collects. It does
    /// not prune or write the MD5 cache itself. When an entry of the page was not seen by a scan
    /// as it is (no id of its kind, or a file whose cache row is not current: made, changed or
    /// renamed by hand since the last scan), its folder goes through `scan` first: one scan of
    /// the direct children per such folder and request. So its id and its MD5 come with the event
    /// of that change, and a rename within the folder is a move that keeps the id. An entry
    /// that the scan could not journal (it is still changing) is left out of the page; its
    /// event brings it later. A missing `path` is [`Page::NotFound`], a file
    /// [`Page::NotAFolder`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list_window(
        &self,
        guard: &StoreGuard,
        path: &WirePath,
        recursive: bool,
        folders_only: bool,
        after: Option<&WirePath>,
        limit: usize,
        scan: &mut dyn FnMut(&WirePath) -> Result<()>,
    ) -> Result<Page> {
        self.check_guard(guard)?;
        let top = match list::resolve(&self.files, path)? {
            Some(top) if top.kind == Kind::Folder => top,
            Some(_) => return Ok(Page::NotAFolder),
            None => return Ok(Page::NotFound),
        };
        let limit = limit.max(1);
        let mut anchor = after.cloned();
        // The folders scanned in this request: a retry after a page that vanished does not
        // scan them again.
        let mut scanned: Vec<WirePath> = Vec::new();
        loop {
            let before = self.check_tree(false)?;
            let mut found = Vec::new();
            let rest = anchor
                .as_ref()
                .and_then(|a| a.segments().get(path.segments().len()..))
                .unwrap_or_default();
            let stated = |n: usize| self.counts.stated(n);
            let window = list::Window {
                recursive,
                folders_only,
                want: limit + 1,
                stated: &stated,
            };
            list::walk_window(path, &top.fs_path, rest, &window, &mut found)?;
            let more = found.len() > limit;
            found.truncate(limit);
            // The folders that hold an unseen entry, each once in this request, in the order
            // of the page.
            let mut unseen = false;
            for f in found.iter().filter(|f| !self.seen(f)) {
                unseen = true;
                let folder = f.path.parent().unwrap_or_else(WirePath::root);
                if !scanned.contains(&folder) {
                    scan(&folder)?;
                    scanned.push(folder);
                }
            }
            let last = found.last().map(|f| f.path.clone());
            if unseen {
                // One the scan could not journal yet (it changes right now) waits for its
                // event: an id or a cache row given here would swallow that event.
                found.retain(|f| self.seen(f));
            }
            let entries = self.entries(found)?;
            let after = self.check_tree(false)?;
            self.check_same_tree(&before, &after)?;
            // Every entry of a full page vanished while it was built: the next ones.
            match last {
                Some(last) if entries.is_empty() && more => anchor = Some(last),
                _ => return Ok(Page::Listed(entries, more)),
            }
        }
    }

    /// The bytes of every file a recursive listing of the root would show, from a walk that
    /// neither assigns ids nor touches the registry or the cache (`usedCapacity`). A tree that
    /// broke meanwhile is `Error::StoreDamaged`, as for [`Store::list`].
    pub fn disk_bytes(&self, guard: &StoreGuard) -> Result<u64> {
        Ok(self.disk_usage(guard)?.1)
    }

    /// The number of files and their bytes, from the walk of [`Store::disk_bytes`] (no ids, no
    /// registry, no cache).
    pub fn disk_usage(&self, guard: &StoreGuard) -> Result<(u64, u64)> {
        self.check_guard(guard)?;
        let before = self.check_tree(false)?;
        let mut all = Vec::new();
        if let Some(found) = list::resolve(&self.files, &WirePath::root())?
            && found.kind == Kind::Folder
        {
            list::walk(
                &found,
                true,
                &mut all,
                &self.vanished_damage(before.imported),
            )?;
        }
        let after = self.check_tree(false)?;
        self.check_same_tree(&before, &after)?;
        let files = all.iter().filter(|f| f.kind == Kind::File);
        Ok((
            files.clone().count() as u64,
            files.map(|f| f.meta.len()).sum(),
        ))
    }

    /// Build entries: assign missing ids in one log append, hash files through the cache. A
    /// fresh row stays in memory until the next flush (a scan, a listing of the whole tree,
    /// [`Store::flush_cache`]), so a lookup never rewrites the whole cache file. A row lost in a
    /// crash only costs a hash later: it holds the size, mtime and ctime it is for. A file that
    /// vanished meanwhile was deleted and is skipped.
    fn entries(&self, found: Vec<Found>) -> Result<Vec<Entry>> {
        let mut state = self.state();
        let mut ops = Vec::new();
        let mut next = state.ids.next();
        let mut ids = Vec::with_capacity(found.len());
        for f in &found {
            let (id, op) = plan_assign(&state.ids, &f.path, f.kind, next)?;
            if let Some(op) = op {
                next = id + 1;
                ops.push(op);
            }
            ids.push(id);
        }
        self.append(&mut state, &ops)?;
        let mut out = Vec::with_capacity(found.len());
        for (f, id) in found.into_iter().zip(ids) {
            let (content_hash, size) = match f.kind {
                Kind::Folder => (None, 0),
                Kind::File => match state.hashes.md5(&f.path, &f.fs_path, &f.meta) {
                    Ok(md5) => (Some(md5), f.meta.len()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_err(&f.fs_path)(e)),
                },
            };
            out.push(Entry {
                tag: f.kind,
                id,
                name: f.path.name().unwrap_or_default().to_string(),
                path_display: f.path.display(),
                content_hash,
                is_downloadable: true,
                size,
                last_update_time: hashes::mtime_ms(&f.meta),
                // Ruling: a file directly at the root has parent_path "/"; top-level folders
                // omit it.
                parent_path: match (f.path.parent_path(), f.kind) {
                    (Some(p), _) => Some(p),
                    (None, Kind::File) => Some("/".to_string()),
                    (None, Kind::Folder) => None,
                },
                path: f.path,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ folders and names

    /// Create the native folders and their parents under `files/` (idempotent) and give them
    /// ids, without journal events. Test hook only: `bindEquipment` and the import use
    /// [`Store::create_native_folders`], which journals `folder.created`.
    #[doc(hidden)]
    pub fn ensure_native_folders(&self, guard: &StoreGuard) -> Result<()> {
        self.check_guard(guard)?;
        for folder in NATIVE_FOLDERS {
            let path = WirePath::parse_named(folder)?;
            if list::resolve(&self.files, &path)?.is_some_and(|f| f.kind == Kind::Folder) {
                continue;
            }
            self.check_path(guard, &path)?;
            let fs = self.fs_path(&path);
            match std::fs::create_dir(&fs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Error::Invalid {
                        what: "native folder",
                        message: format!("{folder} exists but is not a folder"),
                    });
                }
                Err(e) => return Err(io_err(&fs)(e)),
            }
            crate::fsutil::fsync_dir(fs.parent().unwrap_or(&self.files))?;
        }
        let mut found = Vec::new();
        for folder in NATIVE_FOLDERS {
            let path = WirePath::parse_named(folder)?;
            if let Some(f) = list::resolve(&self.files, &path)? {
                found.push(f);
            }
        }
        self.entries(found)?;
        Ok(())
    }

    /// Refuse `name` in the folder `parent` if a sibling with different bytes has the same
    /// caseless form (`Error::NameCollision`, see `fold`), or if the file system finds an entry
    /// under `name` that has other bytes. An entry with the same bytes is no collision (a
    /// replace), also next to another spelling of it. Names starting with `.` are refused
    /// (`Error::Invalid`): listings hide them, so
    /// the device would see the file vanish and delete its copy.
    pub fn check_name(&self, guard: &StoreGuard, parent: &WirePath, name: &str) -> Result<()> {
        self.check_guard(guard)?;
        parent.join(name)?;
        if name.starts_with('.') {
            return Err(Error::Invalid {
                what: "name",
                message: format!("{name:?} starts with a dot; such names are not stored"),
            });
        }
        let Some(dir) = list::resolve(&self.files, parent)? else {
            return Ok(());
        };
        if dir.kind != Kind::Folder {
            return Ok(());
        }
        let entries = std::fs::read_dir(&dir.fs_path).map_err(io_err(&dir.fs_path))?;
        let key = fold(name);
        let mut exact = false;
        let mut existing = None;
        for entry in entries {
            let entry = entry.map_err(io_err(&dir.fs_path))?;
            let sibling = entry.file_name();
            let sibling = sibling.to_string_lossy();
            if sibling == name {
                exact = true;
            } else if existing.is_none() && fold(&sibling) == key {
                existing = Some(sibling.into_owned());
            }
        }
        // An entry with exactly these bytes is the target itself, also next to another
        // spelling of it (a store that holds both).
        if !exact && let Some(existing) = existing {
            return Err(Error::NameCollision {
                existing,
                new: name.to_string(),
            });
        }
        // The file system is the final judge: if it finds something under `name` but no entry
        // has exactly these bytes, it merges the two spellings (its folding differs from ours).
        let target = dir.fs_path.join(name);
        if !exact && std::fs::symlink_metadata(&target).is_ok() {
            let existing = std::fs::canonicalize(&target)
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "an entry the file system treats as the same name".into());
            return Err(Error::NameCollision {
                existing,
                new: name.to_string(),
            });
        }
        Ok(())
    }

    /// Whether the sibling `source` of the folder `parent` may be renamed to `name` in place: like
    /// [`Store::check_name`], but `source` itself is no collision (a rename that only changes
    /// case or Unicode normalization), and an entry with exactly the bytes of `name` is one (a
    /// rename never replaces). The file system judges last: if it finds an entry under `name`
    /// that is not `source`, the name is taken.
    pub fn check_rename(
        &self,
        guard: &StoreGuard,
        parent: &WirePath,
        name: &str,
        source: &str,
    ) -> Result<()> {
        self.check_guard(guard)?;
        parent.join(name)?;
        if name.starts_with('.') {
            return Err(Error::Invalid {
                what: "name",
                message: format!("{name:?} starts with a dot; such names are not stored"),
            });
        }
        let taken = |existing: String| Error::NameCollision {
            existing,
            new: name.to_string(),
        };
        let Some(dir) = list::resolve(&self.files, parent)? else {
            return Ok(());
        };
        if dir.kind != Kind::Folder {
            return Ok(());
        }
        let entries = std::fs::read_dir(&dir.fs_path).map_err(io_err(&dir.fs_path))?;
        let key = fold(name);
        for entry in entries {
            let entry = entry.map_err(io_err(&dir.fs_path))?;
            let sibling = entry.file_name();
            let sibling = sibling.to_string_lossy();
            if sibling == source {
                continue;
            }
            if sibling == name || fold(&sibling) == key {
                return Err(taken(sibling.into_owned()));
            }
        }
        let target = dir.fs_path.join(name);
        if let Ok(found) = std::fs::symlink_metadata(&target) {
            let same = std::fs::symlink_metadata(dir.fs_path.join(source))
                .is_ok_and(|src| same_file(&found, &src));
            if !same {
                return Err(taken(
                    "an entry the file system treats as the same name".into(),
                ));
            }
        }
        Ok(())
    }

    /// The wire path `path` as stored: each segment is looked up in its folder, an entry with
    /// exactly these bytes first, else the one entry with the same caseless form (`fold`; real
    /// files and folders only), whose spelling replaces the wire's (`docs/sync-protocol.md` 3.0).
    /// From the first segment that names nothing on, the wire spelling stays. A file on the way
    /// stays in the path, so the caller still sees it. Two or more caseless matches and no exact
    /// one give [`Stored::Ambiguous`]: nothing is picked.
    pub fn stored_path(&self, guard: &StoreGuard, path: &WirePath) -> Result<Stored> {
        self.check_guard(guard)?;
        let mut stored = WirePath::root();
        let mut found = true;
        let mut existing = 0;
        for segment in path.segments() {
            let name = if found {
                match self.match_child(&stored, segment)? {
                    NameMatch::Exact => {
                        existing += 1;
                        segment.clone()
                    }
                    NameMatch::Folded(name) => {
                        existing += 1;
                        name
                    }
                    NameMatch::Missing => {
                        found = false;
                        segment.clone()
                    }
                    NameMatch::Ambiguous(names) => {
                        return Ok(Stored::Ambiguous {
                            folder: stored,
                            names,
                        });
                    }
                }
            } else {
                segment.clone()
            };
            stored = stored.join(&name)?;
        }
        Ok(Stored::Path {
            path: stored,
            existing,
        })
    }

    /// [`match_name`] against the entries of `folder` on disk. A folder that is missing or no
    /// folder has no entries.
    fn match_child(&self, folder: &WirePath, name: &str) -> Result<NameMatch> {
        let dir = self.fs_path(folder);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(NameMatch::Missing);
            }
            Err(e) => return Err(io_err(&dir)(e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err(&dir))?;
            let Ok(sibling) = entry.file_name().into_string() else {
                continue;
            };
            let listable = entry.file_type().is_ok_and(|t| t.is_dir() || t.is_file());
            names.push((sibling, listable));
        }
        Ok(match_name(
            names.iter().map(|(n, listable)| (n.as_str(), *listable)),
            name,
        ))
    }

    /// [`Store::check_name`] for every segment of `path`, so a folder spelled differently from
    /// an existing one (`NOTE/note` next to `NOTE/Note`) is refused too.
    pub fn check_path(&self, guard: &StoreGuard, path: &WirePath) -> Result<()> {
        let mut parent = WirePath::root();
        for segment in path.segments() {
            self.check_name(guard, &parent, segment)?;
            parent = parent.join(segment)?;
        }
        Ok(())
    }
}

/// Whether two metadata records describe the same file system entry.
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.file_type() == b.file_type() && a.len() == b.len()
    }
}

/// The prefix of the temp name a rename that only changes case passes through
/// (`.nd-move-<pid>-<counter>`). It does not end in `.tmp`, so the temp sweep never removes
/// it; recovery finishes the rename from the name stored in the intent.
pub const MOVE_TEMP_PREFIX: &str = ".nd-move-";

/// A fresh name for the middle step of a rename that only changes case.
fn move_temp_name() -> String {
    format!(
        "{MOVE_TEMP_PREFIX}{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// True for a name [`move_temp_name`] makes (checked again when it comes from an intent file).
fn is_move_temp_name(name: &str) -> bool {
    name.strip_prefix(MOVE_TEMP_PREFIX).is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    })
}

fn unknown_id(id: u64) -> Error {
    Error::Invalid {
        what: "id",
        message: format!("{id} is not known"),
    }
}

/// The id for `path`: the registered one if the kind matches, else `next` with the op to log.
fn plan_assign(
    reg: &Registry,
    path: &WirePath,
    kind: Kind,
    next: u64,
) -> Result<(u64, Option<Op>)> {
    if path.is_root() {
        return Err(Error::Invalid {
            what: "path",
            message: "the root has no id".into(),
        });
    }
    if let Some(id) = reg.id_of(path)
        && reg.get(id).is_some_and(|(k, _)| *k == kind)
    {
        return Ok((id, None));
    }
    Ok((
        next,
        Some(Op::Assign {
            id: next,
            kind,
            path: path.display(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An imported account with its native folders, and its store.
    fn imported_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path());
        accounts
            .create("jordi", "jordi@example.org", "secret")
            .unwrap();
        let store = Store::open(dir.path(), "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let mut a = accounts.get("jordi").unwrap().unwrap();
        a.imported = Some(crate::auth::Imported {
            at: "2026-10-06T10:00:00Z".into(),
            source: "/backup/Supernote".into(),
            files: 0,
            bytes: 0,
        });
        accounts.save(&a).unwrap();
        (dir, store)
    }

    /// The exact entry wins, one caseless match is that entry, two are never picked; dot-names
    /// and entries that are no real file or folder never match without case.
    #[test]
    fn a_name_matches_exactly_first_and_never_picks_one_of_two_spellings() {
        let m = |entries: &[(&str, bool)], name: &str| match_name(entries.iter().copied(), name);
        let both = [("Work", true), ("work", true), ("x", true)];
        assert_eq!(m(&both, "work"), NameMatch::Exact);
        assert_eq!(m(&both, "Work"), NameMatch::Exact);
        assert_eq!(
            m(&both, "WORK"),
            NameMatch::Ambiguous(vec!["Work".into(), "work".into()])
        );
        assert_eq!(
            m(&[("Report.pdf", true)], "report.PDF"),
            NameMatch::Folded("Report.pdf".into())
        );
        assert_eq!(
            m(&[("Caf\u{e9}", true)], "CAFE\u{301}"),
            NameMatch::Folded("Caf\u{e9}".into())
        );
        assert_eq!(
            m(&[("Stra\u{df}e", true)], "STRASSE"),
            NameMatch::Folded("Stra\u{df}e".into())
        );
        assert_eq!(m(&[("Link", false)], "link"), NameMatch::Missing);
        assert_eq!(m(&[("Link", false)], "Link"), NameMatch::Exact);
        assert_eq!(m(&[(".X", true)], ".x"), NameMatch::Missing);
        assert_eq!(m(&[("a", true)], "b"), NameMatch::Missing);
        // one real entry and one link of the same caseless form: the real one
        assert_eq!(
            m(&[("Work", false), ("WORK", true)], "work"),
            NameMatch::Folded("WORK".into())
        );
    }

    fn reason(r: Result<()>) -> String {
        match r {
            Err(Error::StoreDamaged { reason, .. }) => reason,
            other => panic!("expected StoreDamaged, got {other:?}"),
        }
    }

    /// A native folder that vanishes between the resolve and its `read_dir` (the root of the
    /// walk) ends the walk with the reason of the tree check; a user folder is a deletion.
    #[test]
    fn the_walk_refuses_a_vanished_native_folder() {
        let (_dir, store) = imported_store();
        let _g = store.lock().unwrap();
        let note = WirePath::parse_named("NOTE/Note").unwrap();
        std::fs::create_dir(store.fs_path(&note).join("Work")).unwrap();
        let user = list::resolve(&store.files, &note.join("Work").unwrap())
            .unwrap()
            .unwrap();
        let native = list::resolve(&store.files, &note).unwrap().unwrap();
        std::fs::remove_dir_all(store.fs_path(&note)).unwrap();
        let damage = store.vanished_damage(true);
        let mut out = Vec::new();
        assert!(list::walk(&user, true, &mut out, &damage).is_ok());
        let r = reason(list::walk(&native, true, &mut out, &damage));
        assert!(r.contains("the device folder NOTE/Note is missing"), "{r}");
        // The walk of `files/` itself.
        let root = list::resolve(&store.files, &WirePath::root())
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(&store.files).unwrap();
        let r = reason(list::walk(&root, false, &mut out, &damage));
        assert!(
            r.contains("is missing although the account was imported"),
            "{r}"
        );
        // Before the import marker, the same is no damage.
        assert!(list::walk(&root, false, &mut out, &store.vanished_damage(false)).is_ok());
    }

    /// A native folder or `files/` that is removed and put back while the tree is read is
    /// another folder afterwards: refused, although both checks alone pass.
    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_while_the_tree_is_read_is_refused() {
        let (_dir, store) = imported_store();
        let _g = store.lock().unwrap();
        let note = store.fs_path(&WirePath::parse_named("NOTE/Note").unwrap());
        let before = store.check_tree(false).unwrap();
        std::fs::rename(&note, note.with_file_name("Note.old")).unwrap();
        std::fs::create_dir(&note).unwrap();
        let after = store.check_tree(false).unwrap();
        let r = reason(store.check_same_tree(&before, &after));
        assert!(
            r.contains("the device folder NOTE/Note was replaced"),
            "{r}"
        );
        // `files/` swapped as a whole.
        let before = store.check_tree(false).unwrap();
        let copy = store.files.with_file_name("files-copy");
        std::fs::rename(&store.files, &copy).unwrap();
        std::fs::create_dir(&store.files).unwrap();
        for folder in NATIVE_FOLDERS {
            std::fs::create_dir_all(store.files.join(folder)).unwrap();
        }
        let after = store.check_tree(false).unwrap();
        let r = reason(store.check_same_tree(&before, &after));
        assert!(r.contains("files was replaced"), "{r}");
        // Nothing swapped: the same tree.
        let again = store.check_tree(false).unwrap();
        assert!(store.check_same_tree(&after, &again).is_ok());
    }
}
