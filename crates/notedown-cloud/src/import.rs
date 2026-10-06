//! `notedown cloud import`: fill an account with a device backup, so the server holds exactly
//! the state of the device before its first sync. A file missing here makes the device delete
//! its own copy at that sync, so completeness and byte-exact names come first.
//!
//! - Device folders map to wire folders ([`FOLDER_MAP`]): `Note` to `NOTE/Note`, `MyStyle` to
//!   `NOTE/MyStyle`, `Document` to `DOCUMENT/Document`; `EXPORT`, `SCREENSHOT` and `INBOX` keep
//!   their names. At the top of the backup folder, `REMOVABLE0` (the SD card), `backup.json`,
//!   `*.snbak` and dot-files are skipped, and any other entry is reported and skipped. Inside
//!   the device folders every file is imported; only host junk (`.DS_Store`, `._*`,
//!   `.localized`) is skipped, and any other dot-name refuses the import.
//! - The backup must be complete: no `*.part` file anywhere, and a `backup.json` that says
//!   `"complete": true` with no errors (a copy without it needs `no_report`). A backup without
//!   files needs `allow_empty`.
//! - All six device folders must be in the backup (the device root always holds them), so a
//!   wrong folder or a partial backup is refused instead of importing too little.
//! - Anything the store cannot hold (a name that is no valid wire name, a symlink, two names
//!   that differ only in case or Unicode normalization) refuses the whole import before
//!   anything is written.
//! - The whole run holds the store lock. The `imported` marker is cleared before the first
//!   write, so a failed import always leaves the protect gate closed. Every folder and every file is its own commit
//!   (`folder.created` / `file.created`, `origin: "cli"`), with the page index of notes.
//! - Afterwards every file is hashed again at the source and compared with the store listing
//!   (wire path, MD5, size). Only then the `imported` marker goes into `account.json` (under
//!   the account lock, taken after the store lock), which opens the protect gate.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::auth::{Accounts, Imported};
use crate::journal::{Journal, Origin};
use crate::store::{
    Kind, NATIVE_FOLDERS, PutOptions, Store, StoreGuard, WirePath, WriteGuard, create_temp,
};
use crate::{Error, Result, io_err};

/// The file a device backup (`notedown cloud backup-device`) writes into its folder.
pub const BACKUP_FILE: &str = "backup.json";
/// The device's SD card folder; never imported.
pub const SD_CARD: &str = "REMOVABLE0";
/// Device backup files at the top of the backup folder; not imported (inside the device
/// folders every file is).
pub const SNBAK_EXT: &str = ".snbak";

/// Device top folder and the wire folder it maps to, sorted by device name.
pub const FOLDER_MAP: [(&str, &str); 6] = [
    ("Document", "DOCUMENT/Document"),
    ("EXPORT", "EXPORT"),
    ("INBOX", "INBOX"),
    ("MyStyle", "NOTE/MyStyle"),
    ("Note", "NOTE/Note"),
    ("SCREENSHOT", "SCREENSHOT"),
];

/// The wire folder of a device top folder (byte-exact name).
pub fn wire_folder_of(device_folder: &str) -> Option<&'static str> {
    FOLDER_MAP
        .iter()
        .find(|(d, _)| *d == device_folder)
        .map(|(_, w)| *w)
}

/// The wire folders whose trees hold device files ([`FOLDER_MAP`] targets).
pub fn device_folders() -> impl Iterator<Item = &'static str> {
    FOLDER_MAP.iter().map(|(_, w)| *w)
}

/// Why an entry of the backup is not imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkipReason {
    /// `REMOVABLE0`, the SD card.
    SdCard,
    /// `backup.json` of `backup-device`.
    BackupFile,
    /// A `*.snbak` device backup file.
    Snbak,
    /// A name starting with `.` at the top of the backup folder.
    DotFile,
    /// Host junk inside a device folder: `.DS_Store`, `._*`, `.localized`.
    HostJunk,
    /// A top-level entry that is no device folder.
    Unknown,
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::SdCard => "SD card",
            SkipReason::BackupFile => "backup report",
            SkipReason::Snbak => "device backup file",
            SkipReason::DotFile => "dot-file",
            SkipReason::HostJunk => "host junk file",
            SkipReason::Unknown => "not a device folder",
        }
    }
}

/// A skipped entry; `path` is relative to the backup folder (`/` separated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub path: String,
    pub reason: SkipReason,
}

/// What one device folder brings in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCount {
    /// The device folder (`Note`).
    pub source: String,
    /// The wire folder (`NOTE/Note`).
    pub target: String,
    pub files: u64,
    /// Folders below the device folder (not counting itself).
    pub folders: u64,
    pub bytes: u64,
}

/// One file or folder to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub source: PathBuf,
    pub target: WirePath,
    pub kind: Kind,
    /// Bytes of a file (0 for folders).
    pub size: u64,
}

/// The read-only walk of a backup folder.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// One per device folder present in the backup, sorted by device name.
    pub folders: Vec<FolderCount>,
    /// Folders (parents first) and files, in wire path order.
    pub items: Vec<Item>,
    pub skipped: Vec<Skip>,
    /// Entries that cannot be imported; any of them refuses the import.
    pub errors: Vec<String>,
}

impl Scan {
    pub fn files(&self) -> u64 {
        self.folders.iter().map(|f| f.files).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.folders.iter().map(|f| f.bytes).sum()
    }
}

fn rel_display(rel: &[&str]) -> String {
    rel.join("/")
}

/// Host junk that may sit in a copied device folder; skipped silently (only counted).
pub fn is_host_junk(name: &str) -> bool {
    name == ".DS_Store" || name == ".localized" || name.starts_with("._")
}

/// The temp name suffix of an unfinished download (`backup-device`).
pub const PART_EXT: &str = ".part";

/// How many `.part` paths an error names before it gives a count.
const MAX_PART_NAMES: usize = 20;

/// Walk the backup folder (read only). Symlinks, dot-names inside device folders (except
/// [`is_host_junk`]), `*.part` files anywhere, missing device folders and names the store
/// cannot hold are errors. Unless `no_report`, the backup must carry a `backup.json` of a
/// complete `backup-device` run without errors.
pub fn scan(backup: &Path, no_report: bool) -> Result<Scan> {
    let meta = std::fs::metadata(backup).map_err(io_err(backup))?;
    if !meta.is_dir() {
        return Err(Error::Invalid {
            what: "backup folder",
            message: format!("{} is not a folder", backup.display()),
        });
    }
    let mut scan = Scan::default();
    let mut found = Vec::new();
    let mut parts = Vec::new();
    for (name, path, ftype) in sorted_children(backup)? {
        let Some(name) = name else {
            scan.errors
                .push(format!("{}: the name is not UTF-8", path.display()));
            continue;
        };
        if name.ends_with(PART_EXT) && !ftype.is_dir() {
            parts.push(name);
            continue;
        }
        let reason = if name == SD_CARD {
            Some(SkipReason::SdCard)
        } else if name == BACKUP_FILE {
            Some(SkipReason::BackupFile)
        } else if name.starts_with('.') {
            Some(SkipReason::DotFile)
        } else if name.ends_with(SNBAK_EXT) {
            Some(SkipReason::Snbak)
        } else if wire_folder_of(&name).is_none() {
            Some(SkipReason::Unknown)
        } else if !ftype.is_dir() {
            scan.errors.push(format!(
                "{name}: a device folder that is not a plain folder (a symlink?)"
            ));
            continue;
        } else {
            None
        };
        if let Some(reason) = reason {
            scan.skipped.push(Skip { path: name, reason });
            continue;
        }
        found.push((name, path));
    }
    // The device root always holds all six folders: a missing one means a wrong or partial
    // backup, which would make the device delete everything in that folder.
    for (device, _) in FOLDER_MAP {
        if !found.iter().any(|(name, _)| name == device)
            && !scan
                .errors
                .iter()
                .any(|e| e.starts_with(&format!("{device}:")))
        {
            scan.errors.push(format!(
                "{device}: the backup has no such folder; give the folder that holds all of {}",
                FOLDER_MAP.map(|(d, _)| d).join(", ")
            ));
        }
    }
    for (name, path) in found {
        let target = WirePath::parse_named(wire_folder_of(&name).expect("checked"))?;
        let mut count = FolderCount {
            source: name.clone(),
            target: target.display(),
            files: 0,
            folders: 0,
            bytes: 0,
        };
        walk(
            &path,
            &[name.as_str()],
            &target,
            &mut count,
            &mut scan,
            &mut parts,
        )?;
        scan.folders.push(count);
    }
    if !parts.is_empty() {
        parts.sort();
        let mut lines: Vec<String> = parts.iter().take(MAX_PART_NAMES).cloned().collect();
        if parts.len() > MAX_PART_NAMES {
            lines.push(format!("and {} more", parts.len() - MAX_PART_NAMES));
        }
        scan.errors.push(format!(
            "the backup is not complete, it holds unfinished downloads (*{PART_EXT}):\n    {}",
            lines.join("\n    ")
        ));
    }
    scan.items.sort_by(|a, b| a.target.cmp(&b.target));
    check_backup_report(backup, no_report, &mut scan);
    Ok(scan)
}

type Child = (Option<String>, PathBuf, std::fs::FileType);

/// The entries of `dir`, sorted by name bytes.
fn sorted_children(dir: &Path) -> Result<Vec<Child>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).map_err(io_err(dir))? {
        let e = e.map_err(io_err(dir))?;
        let ftype = e.file_type().map_err(io_err(&e.path()))?;
        out.push((e.file_name().to_str().map(String::from), e.path(), ftype));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

fn walk(
    dir: &Path,
    rel: &[&str],
    target: &WirePath,
    count: &mut FolderCount,
    scan: &mut Scan,
    parts: &mut Vec<String>,
) -> Result<()> {
    // Two names in one folder that the store would refuse as a collision.
    let mut folded: HashMap<String, String> = HashMap::new();
    for (name, path, ftype) in sorted_children(dir)? {
        let Some(name) = name else {
            scan.errors
                .push(format!("{}: the name is not UTF-8", path.display()));
            continue;
        };
        let mut here = rel.to_vec();
        here.push(&name);
        let shown = rel_display(&here);
        if name.ends_with(PART_EXT) && !ftype.is_dir() {
            parts.push(shown);
            continue;
        }
        if name.starts_with('.') {
            if is_host_junk(&name) {
                scan.skipped.push(Skip {
                    path: shown,
                    reason: SkipReason::HostJunk,
                });
            } else {
                // The store cannot hold a dot-name, and a silent skip would make the device
                // delete the file.
                scan.errors.push(format!(
                    "{shown}: a name starting with a dot cannot be stored"
                ));
            }
            continue;
        }
        if ftype.is_symlink() || !(ftype.is_file() || ftype.is_dir()) {
            scan.errors
                .push(format!("{shown}: not a plain file or folder (a symlink?)"));
            continue;
        }
        let wire = match target.join(&name) {
            Ok(w) => w,
            Err(e) => {
                scan.errors.push(format!("{shown}: {e}"));
                continue;
            }
        };
        if let Some(other) = folded.insert(crate::store::fold(&name), name.clone()) {
            scan.errors.push(format!(
                "{shown}: differs from {other:?} only in case or Unicode normalization"
            ));
            continue;
        }
        if ftype.is_dir() {
            count.folders += 1;
            scan.items.push(Item {
                source: path.clone(),
                target: wire.clone(),
                kind: Kind::Folder,
                size: 0,
            });
            walk(&path, &here, &wire, count, scan, parts)?;
        } else {
            let size = std::fs::metadata(&path).map_err(io_err(&path))?.len();
            count.files += 1;
            count.bytes += size;
            scan.items.push(Item {
                source: path,
                target: wire,
                kind: Kind::File,
                size,
            });
        }
    }
    Ok(())
}

/// The `backup.json` of `backup-device` must say `"complete": true` and carry an empty
/// `errors` array and no `stale` entries (a missing `stale` counts as empty). Without a report (a copy made another way, for example over USB) the import
/// needs `no_report`.
fn check_backup_report(backup: &Path, no_report: bool, scan: &mut Scan) {
    let path = backup.join(BACKUP_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !no_report {
                scan.errors.push(format!(
                    "{BACKUP_FILE}: missing. A backup made by `notedown cloud backup-device` has \
                     it. For a copy made another way (USB), make sure the copy is complete, then \
                     pass --no-report"
                ));
            }
            return;
        }
        Err(e) => {
            scan.errors.push(format!("{}: {e}", path.display()));
            return;
        }
    };
    let report: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            scan.errors.push(format!("{}: {e}", path.display()));
            return;
        }
    };
    match report.get("complete") {
        Some(serde_json::Value::Bool(true)) => {}
        None => scan.errors.push(format!(
            "{BACKUP_FILE}: it has no \"complete\" field, it was made by an older version or \
             did not finish; run the backup again"
        )),
        Some(_) => scan.errors.push(format!(
            "{BACKUP_FILE}: the backup run did not finish (\"complete\" is not true); run it \
             again with --resume"
        )),
    }
    // A resumed backup with files the device no longer lists would give them back.
    if let Some(stale) = report.get("stale").and_then(|v| v.as_array()) {
        for path in stale {
            scan.errors.push(format!(
                "{BACKUP_FILE} lists a file the device no longer has: {}. Make a fresh backup \
                 into a new, empty folder",
                path.as_str().unwrap_or("")
            ));
        }
    }
    match report["errors"].as_array() {
        None => scan
            .errors
            .push(format!("{BACKUP_FILE}: the report has no errors list")),
        Some(errors) => {
            for err in errors {
                scan.errors.push(format!(
                    "{BACKUP_FILE} lists a failed entry, the backup is incomplete: {}: {}",
                    err["path"].as_str().unwrap_or(""),
                    err["message"].as_str().unwrap_or("")
                ));
            }
        }
    }
}

/// The result of a dry run.
#[derive(Debug, Clone)]
pub struct DryRun {
    pub scan: Scan,
    /// Files the account holds now (a real run needs `--replace` when this is not 0).
    pub files_present: u64,
}

/// Scan the backup and count the account's files without writing anything (no lock file, no
/// `files/`, no journal).
pub fn dry_run(data: &Path, slug: &str, backup: &Path, options: &ImportOptions) -> Result<DryRun> {
    if Accounts::new(data).get(slug)?.is_none() {
        return Err(Error::NoAccount(slug.to_string()));
    }
    let scan = scan(backup, options.no_report)?;
    let files = data
        .join("accounts")
        .join(slug)
        .join(crate::store::FILES_DIR);
    let files_present = count_plain_files(&files)?;
    Ok(DryRun {
        scan,
        files_present,
    })
}

/// Regular files below `dir` that a listing shows (no dot-names), counted without the store.
fn count_plain_files(dir: &Path) -> Result<u64> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut n = 0;
    for e in entries {
        let e = e.map_err(io_err(dir))?;
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let t = e.file_type().map_err(io_err(&e.path()))?;
        if t.is_dir() {
            n += count_plain_files(&e.path())?;
        } else if t.is_file() {
            n += 1;
        }
    }
    Ok(n)
}

/// Options of [`import`].
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Move the account's current files to the recycle bin first, instead of refusing.
    pub replace: bool,
    /// Accept a backup without `backup.json` (a copy made another way, for example over USB).
    pub no_report: bool,
    /// Accept a backup with no files at all (it would open the gate on an empty tree).
    pub allow_empty: bool,
}

/// What [`import`] did.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub scan: Scan,
    /// Files imported and verified.
    pub files: u64,
    pub bytes: u64,
    /// Folders created by the import (native folders included).
    pub folders_created: u64,
    /// Files moved to the recycle bin by `--replace`.
    pub replaced: u64,
    /// The marker written into `account.json`.
    pub imported: Imported,
}

/// How often [`import`] reports progress, in files.
pub const PROGRESS_EVERY: u64 = 100;

fn failed(message: String) -> Error {
    Error::Invalid {
        what: "import",
        message,
    }
}

/// Import the backup folder into the account (see the module docs). `progress` gets one line
/// for people every [`PROGRESS_EVERY`] files and for each phase. Any problem returns an error
/// and leaves the `imported` marker unset (so the protect gate stays closed).
pub fn import(
    data: &Path,
    slug: &str,
    backup: &Path,
    options: &ImportOptions,
    journal: &Journal,
    progress: &mut dyn FnMut(&str),
) -> Result<ImportReport> {
    let accounts = Accounts::new(data);
    if accounts.get(slug)?.is_none() {
        return Err(Error::NoAccount(slug.to_string()));
    }
    let scan = scan(backup, options.no_report)?;
    if !scan.errors.is_empty() {
        return Err(failed(format!(
            "the backup cannot be imported completely, nothing was written:\n  {}",
            scan.errors.join("\n  ")
        )));
    }
    if scan.files() == 0 && !options.allow_empty {
        return Err(failed(
            "the backup holds no files; an empty import would open the protect gate on an \
             empty tree. Pass --allow-empty if the device is really empty"
                .into(),
        ));
    }
    // Only the import may create a missing `files/` or rebuild a damaged tree.
    let store = Store::open_for_import(data, slug)?;
    let guard = store.lock_recovered(journal)?;
    let present = listed_files(&store, &guard)?;
    if present > 0 && !options.replace {
        return Err(failed(format!(
            "account {slug} already holds {present} files; use --replace to move them to the \
             recycle bin first"
        )));
    }
    // Close the protect gate before the first write: an import that fails must not leave it
    // open on a half tree (also when an earlier marker is left on an account without files).
    set_marker(&accounts, slug, None)?;
    let mut replaced = 0;
    if present > 0 {
        progress(&format!(
            "moving the {present} files of account {slug} to the recycle bin"
        ));
        replaced = replace_all(&store, &guard, journal)?;
    }

    let mut folders_created = 0;
    for native in NATIVE_FOLDERS {
        let path = WirePath::parse_named(native)?;
        if create_folder(&store, &guard, journal, &path)? {
            folders_created += 1;
        }
    }
    // Native folders made before the import without an event (by hand, or by an older
    // version) get their `folder.created` now, so the journal knows every listed folder.
    store.journal_native_folders(&guard, journal)?;

    let staging = store
        .account_dir()
        .join(crate::sync::staging::STAGING_DIR)
        .join(format!("import-{}", crate::journal::new_op_id()));
    std::fs::create_dir_all(&staging).map_err(io_err(&staging))?;
    let copied = copy_items(
        &store,
        &guard,
        journal,
        &scan,
        &staging,
        &mut folders_created,
        progress,
    );
    let _ = std::fs::remove_dir_all(&staging);
    let (files, bytes) = copied?;

    progress("verifying every file against the backup");
    verify(&store, &guard, &scan)?;

    let imported = Imported {
        at: OffsetDateTime::from_unix_timestamp_nanos(journal.now_ms() as i128 * 1_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            .format(&Rfc3339)
            .expect("rfc3339 formatting"),
        source: backup.display().to_string(),
        files,
        bytes,
    };
    set_marker(&accounts, slug, Some(imported.clone()))?;
    drop(guard);
    Ok(ImportReport {
        scan,
        files,
        bytes,
        folders_created,
        replaced,
        imported,
    })
}

/// Files the store lists (dot-files and server folders do not count; empty native folders,
/// which `bindEquipment` creates, do not either).
fn listed_files(store: &Store, guard: &StoreGuard) -> Result<u64> {
    Ok(store
        .list(guard, &WirePath::root(), true)?
        .iter()
        .filter(|e| e.tag == Kind::File)
        .count() as u64)
}

/// Write or clear the `imported` marker under the account lock (the store lock is held).
fn set_marker(accounts: &Accounts, slug: &str, marker: Option<Imported>) -> Result<()> {
    let _lock = accounts.lock(slug)?;
    let mut account = accounts
        .get(slug)?
        .ok_or_else(|| Error::NoAccount(slug.to_string()))?;
    if account.imported == marker {
        return Ok(());
    }
    account.imported = marker;
    accounts.save(&account)
}

/// Move everything below the native folders (and every other top-level entry) to the recycle
/// bin, one delete commit each, all in one millisecond slot. Native folders stay with their
/// ids. Returns the number of files moved.
fn replace_all(store: &Store, guard: &WriteGuard, journal: &Journal) -> Result<u64> {
    let at = journal.now_ms();
    let mut moved = 0;
    let mut queue = vec![WirePath::root()];
    while let Some(folder) = queue.pop() {
        for e in store.list(guard, &folder, false)? {
            if e.tag == Kind::Folder && crate::store::is_native_folder(&e.path) {
                queue.push(e.path);
                continue;
            }
            let commit = store.plan_delete(guard, &e.path, at, Origin::Cli, None)?;
            moved += commit
                .events
                .iter()
                .filter(|ev| matches!(ev, crate::journal::EventBody::FileDeleted { .. }))
                .count() as u64;
            store.commit(guard, commit, journal)?;
        }
    }
    Ok(moved)
}

/// Create the folder `path` in its own commit unless it is a folder already.
fn create_folder(
    store: &Store,
    guard: &WriteGuard,
    journal: &Journal,
    path: &WirePath,
) -> Result<bool> {
    if store
        .entry(guard, path)?
        .is_some_and(|e| e.tag == Kind::Folder)
    {
        return Ok(false);
    }
    let commit = store.plan_create_folder(guard, path, Origin::Cli, None)?;
    store.commit(guard, commit, journal)?;
    Ok(true)
}

/// Copy every folder and file of the scan into the store, one commit each. Stops at the first
/// error. Returns the number of files and bytes.
fn copy_items(
    store: &Store,
    guard: &WriteGuard,
    journal: &Journal,
    scan: &Scan,
    staging: &Path,
    folders_created: &mut u64,
    progress: &mut dyn FnMut(&str),
) -> Result<(u64, u64)> {
    let total = scan.files();
    let (mut files, mut bytes) = (0u64, 0u64);
    for item in &scan.items {
        let fail = |e: Error| failed(format!("{}: {e}", item.target));
        match item.kind {
            Kind::Folder => {
                if create_folder(store, guard, journal, &item.target).map_err(fail)? {
                    *folders_created += 1;
                }
            }
            Kind::File => {
                import_file(store, guard, journal, item, staging).map_err(fail)?;
                files += 1;
                bytes += item.size;
                if files % PROGRESS_EVERY == 0 {
                    progress(&format!("imported {files} of {total} files"));
                }
            }
        }
    }
    Ok((files, bytes))
}

/// Stage one file inside the account, plan, index and commit it, then give it the source's
/// modification time (the listing's `lastUpdateTime`).
fn import_file(
    store: &Store,
    guard: &WriteGuard,
    journal: &Journal,
    item: &Item,
    staging: &Path,
) -> Result<()> {
    let (tmp, mut out) = create_temp(staging).map_err(io_err(staging))?;
    let staged = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(&item.source)?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()
    })();
    drop(out);
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(&item.source)(e));
    }
    let planned = store.plan_put_with(
        guard,
        &item.target,
        &tmp,
        Origin::Cli,
        None,
        PutOptions {
            create_parents: true,
            ..PutOptions::default()
        },
    );
    let mut commit = match planned {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    crate::pages::plan_page_events(store, guard, &mut commit);
    let done = store.commit(guard, commit, journal);
    // Gone after a commit (renamed into files/); left over after an unchanged put or an error.
    let _ = std::fs::remove_file(&tmp);
    done?;
    if let Ok(mtime) = std::fs::metadata(&item.source).and_then(|m| m.modified()) {
        let target = store.fs_path(&item.target);
        let set = std::fs::File::options()
            .write(true)
            .open(&target)
            .and_then(|f| f.set_modified(mtime));
        if let Err(e) = set {
            eprintln!(
                "warning: cannot set the modification time of {}: {e}",
                target.display()
            );
        }
    }
    Ok(())
}

/// MD5 and size of a file, read fresh.
fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).map_err(io_err(path))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(io_err(path))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((const_hex::encode(hasher.finalize()), size))
}

/// Compare every item with the store listing (the view `list_folder` serves): each file by
/// wire path, MD5 (hashed again at the source) and size, each folder by its path.
fn verify(store: &Store, guard: &StoreGuard, scan: &Scan) -> Result<()> {
    let listed: BTreeMap<String, (Kind, Option<String>, u64)> = store
        .list(guard, &WirePath::root(), true)?
        .into_iter()
        .map(|e| (e.path_display, (e.tag, e.content_hash, e.size)))
        .collect();
    let mut problems = Vec::new();
    for item in &scan.items {
        let path = item.target.display();
        let got = listed.get(&path);
        match item.kind {
            Kind::Folder => {
                if !matches!(got, Some((Kind::Folder, _, _))) {
                    problems.push(format!("{path}: the folder is not listed"));
                }
            }
            Kind::File => {
                let (md5, size) = hash_file(&item.source)?;
                match got {
                    Some((Kind::File, Some(m), s)) if *m == md5 && *s == size => {}
                    Some((Kind::File, m, s)) => problems.push(format!(
                        "{path}: the store has MD5 {} and {s} bytes, the backup {md5} and {size} \
                         bytes",
                        m.as_deref().unwrap_or("none")
                    )),
                    _ => problems.push(format!("{path}: the file is not listed")),
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(failed(format!(
            "the check against the backup failed, the protect gate stays closed:\n  {}",
            problems.join("\n  ")
        )))
    }
}
