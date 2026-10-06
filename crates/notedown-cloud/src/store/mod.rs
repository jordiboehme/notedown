//! The file store of one account: the only writer of `accounts/<slug>/files/` (the device
//! tree), the id registry (`ids.json`, `ids.jsonl`) and the MD5 cache (`hashes.json`).
//!
//! Every change runs under the store lock `accounts/<slug>/.store.lock` (std `File::lock`,
//! exclusive), in the server and in every CLI command. Mutating methods take a [`StoreGuard`],
//! so the lock cannot be forgotten. Each `lock()` reloads the registry and the cache when another
//! process changed them on disk.
//!
//! Names are stored byte-exact. Lookups compare names byte by byte against the folder entries,
//! so a case-insensitive file system (macOS APFS) cannot match a different spelling.

mod hashes;
mod ids;
mod list;
mod path;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

pub use ids::FIRST_ID;
pub use list::Entry;
pub use path::WirePath;

use hashes::HashCache;
use ids::{Op, Registry};
use list::Found;

use crate::auth::{Accounts, validate_slug};
use crate::{Error, Result, io_err};

/// `accounts/<slug>/.store.lock`.
pub const STORE_LOCK_FILE: &str = ".store.lock";
/// `accounts/<slug>/files/`: the device tree.
pub const FILES_DIR: &str = "files";

/// The device's native folders and their parents, parents first (`docs/sync-protocol.md` 3.3).
/// They are created by [`Store::ensure_native_folders`] and are never pruned.
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
    })?;
    crate::fsutil::fsync_dir(dir)
}

/// The comparison key for name collisions, Unicode canonical caseless matching: NFD, full case
/// folding (`ß` and `SS`, final sigma, ligatures such as `ﬁ`), then NFC.
fn fold(name: &str) -> String {
    let nfd: String = name.nfd().collect();
    unicase::UniCase::new(nfd).to_folded_case().nfc().collect()
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
    account_dir: PathBuf,
    files: PathBuf,
    state: Mutex<State>,
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
    /// creates `files/` if needed and compacts `ids.jsonl` into `ids.json`.
    pub fn open(data_dir: &Path, slug: &str) -> Result<Store> {
        validate_slug(slug)?;
        let account_dir = Accounts::new(data_dir).account_dir(slug);
        if !account_dir.is_dir() {
            return Err(Error::NoAccount(slug.to_string()));
        }
        let files = account_dir.join(FILES_DIR);
        let store = Store {
            account_dir,
            files,
            state: Mutex::new(State {
                ids: Registry::default(),
                ids_stamp: None,
                hashes: HashCache::default(),
                hashes_stamp: None,
            }),
        };
        let guard = store.lock()?;
        {
            let mut state = store.state();
            if !Registry::log_is_empty(&store.account_dir) {
                state.ids.compact(&store.account_dir)?;
                state.ids_stamp = Some(store.ids_stamps());
            }
        }
        drop(guard);
        Ok(store)
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
    /// cache up to date with the disk. A torn last line of `ids.jsonl` is cut here, and a
    /// missing `files/` is created (only ever under the lock).
    pub fn lock(&self) -> Result<StoreGuard> {
        let path = self.account_dir.join(STORE_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_err(&path))?;
        file.lock().map_err(io_err(&path))?;
        let guard = StoreGuard {
            _file: file,
            account_dir: self.account_dir.clone(),
        };
        if !self.files.is_dir() {
            std::fs::create_dir_all(&self.files).map_err(io_err(&self.files))?;
            crate::fsutil::fsync_dir(&self.account_dir)?;
        }
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

    fn check_guard(&self, guard: &StoreGuard) -> Result<()> {
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

    /// The entries below `path` (direct children, or every descendant with `recursive`), flat
    /// and sorted by path segments, so a folder's descendants follow it. Folders are entries of
    /// their own. Dot-entries and everything outside `files/` never appear. A missing path or a
    /// file gives an empty list. Entries without an id get one here.
    pub fn list(&self, guard: &StoreGuard, path: &WirePath, recursive: bool) -> Result<Vec<Entry>> {
        self.check_guard(guard)?;
        let Some(found) = list::resolve(&self.files, path)? else {
            return Ok(Vec::new());
        };
        if found.kind != Kind::Folder {
            return Ok(Vec::new());
        }
        let mut all = Vec::new();
        list::walk(&found, recursive, &mut all)?;
        all.sort_by(|a, b| a.path.cmp(&b.path));
        let full = path.is_root() && recursive;
        let entries = self.entries(all)?;
        if full {
            let seen: HashSet<String> = entries
                .iter()
                .filter(|e| e.tag == Kind::File)
                .map(|e| e.path_display.clone())
                .collect();
            let mut state = self.state();
            state.hashes.retain(&seen);
            self.flush_hashes(&mut state)?;
        }
        Ok(entries)
    }

    /// Build entries: assign missing ids in one log append, hash files through the cache, write
    /// the cache once. A file that vanished meanwhile was deleted and is skipped.
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
        self.flush_hashes(&mut state)?;
        Ok(out)
    }

    // ------------------------------------------------------------------ folders and names

    /// Create the native folders and their parents under `files/` (idempotent) and give them
    /// ids. Called by `bindEquipment` and by import.
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
    /// under `name` that has other bytes. The same bytes are no collision (a replace). Names starting with `.` are refused (`Error::Invalid`): listings hide them, so
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
        if let Some(existing) = existing {
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
