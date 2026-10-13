//! Durable file writes: unique temp file, fsync, rename, fsync of the folder.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Result, io_err};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `path` with symlinks resolved, also when its end does not exist yet: the longest existing
/// ancestor is resolved and the rest is joined to it. `..` in the missing rest stays as written.
pub fn real_path(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(real) = cur.canonicalize() {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Flush a folder's entries (a created or renamed file) to disk. A no-op where a folder cannot be
/// opened for this (non-Unix).
pub fn fsync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(io_err(dir))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// `Some(true)` when the open `file` is the file at `path` (same device and inode), `None` when
/// `path` is gone. A lock taken on a file that was deleted or replaced meanwhile excludes
/// nobody; the lock functions check with this after the lock is held.
#[cfg(unix)]
pub(crate) fn is_same_file(file: &std::fs::File, path: &Path) -> std::io::Result<Option<bool>> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(now) => Ok(Some(open.dev() == now.dev() && open.ino() == now.ino())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The server runs on unix only. Here the check is weaker: it sees a missing path, not a file
/// that was replaced.
#[cfg(not(unix))]
pub(crate) fn is_same_file(_file: &std::fs::File, path: &Path) -> std::io::Result<Option<bool>> {
    Ok(path.exists().then_some(true))
}

/// Rename `from` to `to`, then fsync the folder of `to` and, when it is another one, the folder
/// of `from`.
pub fn rename_durable(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(io_err(to))?;
    let dir_of = |p: &Path| match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    let (to_dir, from_dir) = (dir_of(to), dir_of(from));
    fsync_dir(&to_dir)?;
    if from_dir != to_dir {
        fsync_dir(&from_dir)?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically: a temp file `.<name>.<pid>.<counter>.tmp` in the same
/// folder (unique, so concurrent writers never share one), fsync, rename, fsync of the folder.
/// With `private` the file has mode 0600 from its creation on.
pub fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    let _ = private;
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_err(path)(e)
    })?;
    fsync_dir(dir)
}

/// JSON with two-space indent and a trailing newline.
pub(crate) fn json_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(value).expect("plain structs serialize");
    text.push('\n');
    text.into_bytes()
}
