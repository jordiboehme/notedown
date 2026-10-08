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

use std::path::Path;

use crate::journal::{Journal, Origin};
use crate::store::{PendingOp, PutOptions, ScanOptions, Store, WirePath, WriteGuard, create_temp};
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

/// The result of [`put`].
#[derive(Debug, Clone)]
pub struct PutResult {
    pub id: u64,
    pub path: WirePath,
    pub outcome: PutOutcome,
    /// The journal `seq` values of the commit (empty when unchanged).
    pub seqs: Vec<u64>,
}

fn invalid(what: &'static str, message: String) -> Error {
    Error::Invalid { what, message }
}

/// The wire folder `to` (any device form: leading, double or trailing slashes), checked to lie
/// inside a device folder tree (byte-exact, segment-aware).
pub fn target_folder(to: &str) -> Result<WirePath> {
    let folder = WirePath::parse(to)?;
    let inside = crate::import::device_folders()
        .any(|root| WirePath::parse_named(root).is_ok_and(|root| folder.starts_with(&root)));
    if folder.is_root() || !inside {
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
    let copied = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(source)?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()
    })();
    drop(out);
    copied.map_err(io_err(source))?;

    let guard = store.lock_recovered(journal)?;
    scan_way(store, &guard, journal, path, min_files)?;
    let mut commit = store.plan_put_with(
        &guard,
        path,
        &tmp,
        Origin::Cli,
        None,
        PutOptions {
            create_parents: true,
            recycle_at_ms: Some(journal.now_ms()),
            ..PutOptions::default()
        },
    )?;
    let (id, outcome) = match &commit.op {
        PendingOp::Unchanged { id, .. } => (*id, PutOutcome::Unchanged),
        PendingOp::Put { id, recycle, .. } => (
            *id,
            if recycle.is_some() {
                PutOutcome::Replaced
            } else {
                PutOutcome::Created
            },
        ),
        other => {
            return Err(invalid("put", format!("unexpected plan {other:?}")));
        }
    };
    crate::pages::plan_page_events(store, &guard, &mut commit);
    let seqs = store.commit(&guard, commit, journal)?;
    Ok(PutResult {
        id,
        path: path.clone(),
        outcome,
        seqs,
    })
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
