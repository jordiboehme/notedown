//! `notedown cloud put`: copy one file into an account, as a device upload would, under the
//! store lock (so it is safe while the server runs). The device downloads it at its next sync.
//!
//! - The target folder is a wire folder inside one of the device folder trees
//!   ([`crate::import::FOLDER_MAP`]: `NOTE/Note`, `NOTE/MyStyle`, `DOCUMENT/Document`, `EXPORT`,
//!   `SCREENSHOT`, `INBOX`). Missing folders are created (`folder.created` each).
//! - A name that collides with another name in case or Unicode normalization is refused.
//! - A replace keeps the id (`file.modified`) and copies the old bytes into the recycle bin
//!   first. Identical bytes change nothing.
//! - Events carry `origin: "cli"`. `put` never touches the `imported` marker.
//! - First the folders on the way to the target are scanned for changes made by hand, as for a
//!   device upload (`docs/sync-protocol.md` 8.3). While a scan holds the account
//!   (`scan-hold.json`), `put` refuses with `Error::ScanHold`.
//!
//! [`commit_staged`] is the commit of such an upload under the store lock, shared by `put`
//! (origin `cli`) and the REST upload (origin `app`).

use std::io::{Read, Write};
use std::path::Path;

use md5::{Digest, Md5};

use crate::journal::{Journal, Origin};
use crate::store::{
    Kind, PendingOp, PutOptions, ScanOptions, Store, WirePath, WriteGuard, create_temp,
};
use crate::{Error, Result, io_err};

/// What [`put`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// A new file.
    Created,
    /// The file existed with other bytes; same id, old bytes in the recycle bin.
    Replaced,
    /// The file existed with the same bytes; nothing was written or journaled.
    Unchanged,
}

/// The result of [`put`] and [`commit_staged`].
#[derive(Debug, Clone)]
pub struct PutResult {
    pub id: u64,
    pub path: WirePath,
    pub outcome: PutOutcome,
    /// The journal `seq` values of the commit (empty when unchanged).
    pub seqs: Vec<u64>,
    /// The MD5 of the file now there (lowercase hex).
    pub md5: String,
    pub size: u64,
    /// The modification time of the file now there, in ms (the listing's `lastUpdateTime`).
    pub mtime_ms: u64,
}

/// What the file at the target must be before [`commit_staged`] writes. A file that already
/// has the staged bytes is [`PutOutcome::Unchanged`] whatever the precondition says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// No condition.
    None,
    /// Nothing is at the target (`If-None-Match: *`).
    Absent,
    /// A file with this MD5 is at the target, compared case-insensitive (`If-Match`).
    Md5(String),
}

fn invalid(what: &'static str, message: String) -> Error {
    Error::Invalid { what, message }
}

/// The wire folder `to` (any device form: leading, double or trailing slashes), checked to lie
/// inside a device folder tree (byte-exact, segment-aware).
pub fn target_folder(to: &str) -> Result<WirePath> {
    let folder = WirePath::parse(to)?;
    if crate::store::trees::device_tree_of(&folder).is_none() {
        let roots: Vec<&str> = crate::import::device_folders().collect();
        return Err(invalid(
            "target folder",
            format!(
                "{to:?} is not inside one of the device folders {}",
                roots.join(", ")
            ),
        ));
    }
    Ok(folder)
}

/// Copy `source` to `<to>/<name>` (`name` defaults to the source's file name). See the module
/// docs. Waits for the store lock when the server or another command holds it.
pub fn put(
    data: &Path,
    slug: &str,
    journal: &Journal,
    source: &Path,
    to: &str,
    name: Option<&str>,
) -> Result<PutResult> {
    let folder = target_folder(to)?;
    let name = match name {
        Some(n) => n.to_string(),
        None => source
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .ok_or_else(|| {
                invalid(
                    "file name",
                    format!("{} has no UTF-8 file name; give --name", source.display()),
                )
            })?,
    };
    let path = folder.join(&name)?;
    let meta = std::fs::metadata(source).map_err(io_err(source))?;
    if !meta.is_file() {
        return Err(invalid(
            "source",
            format!("{} is not a file", source.display()),
        ));
    }
    let min_files = crate::Config::load_or_default(data)?.scan_min_files;
    let store = Store::open(data, slug)?;

    // Stage the copy inside the account before taking the lock.
    let staging = store
        .account_dir()
        .join(crate::sync::staging::STAGING_DIR)
        .join(format!("cli-{}", crate::journal::new_op_id()));
    std::fs::create_dir_all(&staging).map_err(io_err(&staging))?;
    let result = stage_and_commit(&store, journal, source, &path, &staging, min_files);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn stage_and_commit(
    store: &Store,
    journal: &Journal,
    source: &Path,
    path: &WirePath,
    staging: &Path,
    min_files: u64,
) -> Result<PutResult> {
    let (tmp, mut out) = create_temp(staging).map_err(io_err(staging))?;
    let copied = (|| -> std::io::Result<String> {
        let mut input = std::fs::File::open(source)?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.sync_all()?;
        Ok(const_hex::encode(hasher.finalize()))
    })();
    drop(out);
    let md5 = copied.map_err(io_err(source))?;

    let guard = store.lock_recovered(journal)?;
    commit_staged(
        store,
        &guard,
        journal,
        path,
        &tmp,
        &md5,
        Origin::Cli,
        journal.now_ms(),
        Precondition::None,
        &|path| scan_way(store, &guard, journal, path, min_files),
    )
}

/// Commits the staged file `staged` (inside the account, with MD5 `md5`) to `path` under
/// `guard`, with the rules of `cloud put` (module docs): missing folders created, collisions
/// refused, a replace keeps the id and recycles the old bytes at `now_ms`. In this order:
/// `way_scan(path)` (the scans of the folders on the way, which also refuse a scan hold), then
/// a file at `path` with these bytes is [`PutOutcome::Unchanged`], then `precondition`
/// (`Error::PreconditionFailed`), then the plan, the page events and the commit. It never
/// removes the staged file; the caller does. Once the commit is durable it never fails: a hash
/// cache that can't be written and a modification time that can't be read (then 0) are only
/// warned about.
#[allow(clippy::too_many_arguments)] // one call per caller; the decisions file names them all
pub fn commit_staged(
    store: &Store,
    guard: &WriteGuard,
    journal: &Journal,
    path: &WirePath,
    staged: &Path,
    md5: &str,
    origin: Origin,
    now_ms: u64,
    precondition: Precondition,
    way_scan: &dyn Fn(&WirePath) -> Result<()>,
) -> Result<PutResult> {
    way_scan(path)?;
    if precondition != Precondition::None {
        check_precondition(store, guard, path, md5, &precondition)?;
    }
    // The size of the staged bytes: what is at `path` after the commit, also when unchanged.
    let size = std::fs::metadata(staged).map_err(io_err(staged))?.len();
    let mut commit = store.plan_put_with(
        guard,
        path,
        staged,
        origin,
        None,
        PutOptions {
            create_parents: true,
            recycle_at_ms: Some(now_ms),
            expect_md5: Some(md5.to_string()),
            ..PutOptions::default()
        },
    )?;
    let (id, outcome, md5) = match &commit.op {
        PendingOp::Unchanged { id, md5, .. } => (*id, PutOutcome::Unchanged, md5.clone()),
        PendingOp::Put {
            id, recycle, md5, ..
        } => (
            *id,
            if recycle.is_some() {
                PutOutcome::Replaced
            } else {
                PutOutcome::Created
            },
            md5.clone(),
        ),
        other => {
            return Err(invalid("put", format!("unexpected plan {other:?}")));
        }
    };
    crate::pages::plan_page_events(store, guard, &mut commit);
    let seqs = store.commit(guard, commit, journal)?;
    // The commit is durable: nothing after it may turn it into an error.
    if let Err(e) = store.flush_cache(guard) {
        eprintln!("notedown cloud: warning: {path}: the hash cache is not written: {e}");
    }
    let fs = store.fs_path(path);
    let mtime_ms = match std::fs::metadata(&fs) {
        Ok(meta) => crate::store::mtime_ms(&meta),
        Err(e) => {
            eprintln!("notedown cloud: warning: {path}: no modification time: {e}");
            0
        }
    };
    Ok(PutResult {
        id,
        path: path.clone(),
        outcome,
        seqs,
        md5,
        size,
        mtime_ms,
    })
}

/// `Ok` when the file at `path` already has the bytes `md5` (an unchanged put) or meets
/// `precondition`; else `Error::PreconditionFailed`. A folder at `path` meets no precondition
/// but [`Precondition::None`].
fn check_precondition(
    store: &Store,
    guard: &WriteGuard,
    path: &WirePath,
    md5: &str,
    precondition: &Precondition,
) -> Result<()> {
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let (exists, current) = match store.entry(guard, path)? {
        None => (false, None),
        Some(e) if e.tag == Kind::File => (true, Some(store.md5_of(guard, path)?)),
        Some(_) => (true, None),
    };
    if current.as_deref().is_some_and(|c| same(c, md5)) {
        return Ok(());
    }
    let met = match precondition {
        Precondition::None => true,
        Precondition::Absent => !exists,
        Precondition::Md5(want) => current.as_deref().is_some_and(|c| same(c, want)),
    };
    if met {
        Ok(())
    } else {
        Err(Error::PreconditionFailed(path.display()))
    }
}

/// The scans of a device upload to `path` ([`Store::way_scopes`]), so the put never gives a
/// file or folder made by hand an id without its event. `Error::ScanHold` while the account is
/// held (also when the marker can't be read) or when one of these scans holds.
fn scan_way(
    store: &Store,
    guard: &WriteGuard,
    journal: &Journal,
    path: &WirePath,
    min_files: u64,
) -> Result<()> {
    let held = || Error::ScanHold(store.slug().to_string());
    if !matches!(store.scan_hold(), Ok(None)) {
        return Err(held());
    }
    for scope in store.way_scopes(path)? {
        let opts = ScanOptions {
            scope,
            recursive: false,
            dry_run: false,
            accept: false,
            min_files,
        };
        if store
            .scan(guard, journal, &opts, journal.now_ms())?
            .held
            .is_some()
        {
            return Err(held());
        }
    }
    Ok(())
}
