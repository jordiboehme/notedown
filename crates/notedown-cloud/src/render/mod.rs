//! PDFs of an account's notes in a folder outside the data tree, kept up to date by comparing
//! the store with a manifest (docs/superpowers/specs/2026-10-08-notedown-render-and-push-order-design.md 4).
//!
//! The pass reads the store without changing it: no ids, no MD5 cache rows, no journal
//! events. It renders only notes the id registry knows, so a note placed by hand waits for the
//! scan, which journals it first.
//!
//! The pass touches only files that are its own: PDFs the manifest names, and at a target path
//! also a PDF that notedown made (see `Pass::is_ours` in `own.rs`). Every other file in the
//! render folder is never moved, replaced or deleted.
//!
//! - `notes`: the store's notes and the PDF path of each;
//! - `pass`: one pass over the notes (write, move, delete);
//! - `own`: which files in the render folder are the pass's own.

pub mod follow;
pub mod manifest;
mod notes;
mod own;
mod pass;

use std::path::{Path, PathBuf};

use crate::config::DEFAULT_RENDER_DIR;
use crate::fsutil::real_path;
use crate::store::Store;
use crate::{Error, Result, io_err};

pub use notes::{assign_targets, pdf_path_of};

pub const LOCK_FILE: &str = ".notedown-render.lock";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassOptions {
    /// Render every note again, also those the manifest says are up to date.
    pub all: bool,
    /// Wait for the render lock of the account (and say so on stderr) instead of skipping.
    pub wait_for_lock: bool,
}

/// What a pass did. Paths are relative to `<root>/<slug>/`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// PDFs written.
    pub written: Vec<String>,
    /// PDFs moved: (old, new).
    pub moved: Vec<(String, String)>,
    /// PDFs deleted.
    pub deleted: Vec<String>,
    /// Notes that failed: (note path, error text).
    pub failed: Vec<(String, String)>,
    /// Target paths that hold a file notedown didn't write; it is left alone.
    pub kept_foreign: Vec<String>,
    /// Another pass held the render lock; nothing was done.
    pub busy: bool,
    /// The first disk error of the pass (`<note path>: <error>`): a note could not be read or
    /// its PDF not written. Its row stays as it was, and another pass may work, so the server
    /// tries again. A note that can't be rendered is no disk error.
    pub retry: Option<String>,
}

/// One pass for the account of `store`: write, move or delete the PDFs below `<root>/<slug>/`
/// so they match the store's notes. `root` must exist. The store lock is held only while the
/// registry and the MD5 cache are read, never while a note is read or rendered.
pub fn render_account(store: &Store, root: &Path, opts: &PassOptions) -> Result<PassReport> {
    let dir = account_dir(store, root)?;
    let Some(_lock) = take_lock(&dir, store.slug(), opts.wait_for_lock)? else {
        return Ok(PassReport {
            busy: true,
            ..PassReport::default()
        });
    };
    // An account without `files/` has nothing to render. The pass reads the store only, so
    // it never lets the store lock create the folder; the manifest and the PDFs stay.
    if !store.files_dir().is_dir() {
        return Ok(PassReport::default());
    }
    let notes = notes::list_notes(store)?;
    let mut pass = pass::Pass::new(store.slug(), dir, opts.all);
    let result = pass.run(&notes);
    // Also after an error: the rows of the work done so far stay true.
    pass.save(result.is_ok())?;
    result?;
    Ok(pass.report)
}

/// `<root>/<slug>`, created. `root` must be an existing folder, and `<root>/<slug>` must be
/// neither the data folder, nor inside it, nor hold it (`root` may hold the data folder). The
/// one exception is the default `<data>/share` ([`DEFAULT_RENDER_DIR`]), which is created when
/// it is missing.
fn account_dir(store: &Store, root: &Path) -> Result<PathBuf> {
    let invalid = |message: String| Error::Invalid {
        what: "render_dir",
        message,
    };
    // `<data>/accounts/<slug>` is the account folder.
    let data = store.account_dir().parent().and_then(Path::parent);
    let default_root = data.map(|d| real_path(&d.join(DEFAULT_RENDER_DIR)));
    let is_default = default_root.as_deref() == Some(real_path(root).as_path());
    if is_default {
        std::fs::create_dir_all(root).map_err(io_err(root))?;
    }
    if !root.is_dir() {
        return Err(invalid(format!("{} doesn't exist", root.display())));
    }
    let dir = root.join(store.slug());
    // The real path: through a symlink at `<root>/<slug>` too, when it is there.
    let real_dir = match dir.canonicalize() {
        Ok(real) => real,
        Err(_) => root
            .canonicalize()
            .map_err(io_err(root))?
            .join(store.slug()),
    };
    if let Some(data) = data
        && let Ok(real_data) = data.canonicalize()
    {
        // Only `<data>/share/<slug>` itself, not a symlink there that leads elsewhere.
        let own = is_default
            && default_root
                .as_ref()
                .is_some_and(|d| real_dir == d.join(store.slug()));
        if real_dir.starts_with(&real_data) && !own {
            return Err(invalid(format!(
                "{} is inside the data folder {} or is that folder",
                dir.display(),
                data.display()
            )));
        }
        // The other way round, the pass would write PDFs and prune folders next to the data.
        if real_data.starts_with(&real_dir) {
            return Err(invalid(format!(
                "{} holds the data folder {}",
                dir.display(),
                data.display()
            )));
        }
    }
    std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    Ok(dir)
}

/// The render lock of the account folder; `None` when it is busy and `wait` is false.
fn take_lock(dir: &Path, slug: &str, wait: bool) -> Result<Option<std::fs::File>> {
    let path = dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err(&path))?;
    match file.try_lock() {
        Ok(()) => return Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) if !wait => return Ok(None),
        Err(std::fs::TryLockError::WouldBlock) => {
            eprintln!("waiting for the render lock of account {slug}");
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(io_err(&path)(e)),
    }
    file.lock().map_err(io_err(&path))?;
    Ok(Some(file))
}
