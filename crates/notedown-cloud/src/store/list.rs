//! Listing entries (`EntriesVO`) and exact path resolution.

use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

use super::Kind;
use super::path::WirePath;
use crate::{Error, Result, io_err};

/// One file or folder as the device sees it (`EntriesVO`, `docs/sync-protocol.md` 3.5).
/// Field order is the wire order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub tag: Kind,
    /// Sent as a JSON string.
    #[serde(serialize_with = "id_string")]
    pub id: u64,
    pub name: String,
    pub path_display: String,
    /// Lowercase hex MD5; `None` for folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub is_downloadable: bool,
    /// Bytes; 0 for folders.
    pub size: u64,
    /// mtime in ms since the epoch.
    #[serde(rename = "lastUpdateTime")]
    pub last_update_time: u64,
    /// `/NOTE/Note`; `"/"` for a file directly at the root; `None` (omitted) for top-level
    /// folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_path: Option<String>,
    /// The parsed path (not on the wire).
    #[serde(skip)]
    pub path: WirePath,
}

fn id_string<S: Serializer>(id: &u64, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(id)
}

/// A file or folder found on disk that the store may list.
pub(crate) struct Found {
    pub path: WirePath,
    pub fs_path: PathBuf,
    pub kind: Kind,
    pub meta: std::fs::Metadata,
}

/// The kind of a listable directory entry: regular files and real folders. Symlinks, devices,
/// sockets and the like are never listed.
fn listable(meta: &std::fs::Metadata) -> Option<Kind> {
    let ft = meta.file_type();
    if ft.is_symlink() {
        None
    } else if ft.is_dir() {
        Some(Kind::Folder)
    } else if ft.is_file() {
        Some(Kind::File)
    } else {
        None
    }
}

/// True if a folder holds an entry named exactly `name` (byte comparison, so a case-insensitive
/// or normalization-insensitive file system cannot answer for a different spelling).
pub(crate) fn has_exact_child(dir: &Path, name: &str) -> Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Ok(false),
        Err(e) => return Err(io_err(dir)(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io_err(dir))?;
        if entry.file_name().as_encoded_bytes() == name.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Resolve `path` under `files` by exact names. `None` if any segment is missing, hidden, or
/// not listable. The root resolves to `files` itself as a folder.
pub(crate) fn resolve(files: &Path, path: &WirePath) -> Result<Option<Found>> {
    if path.is_hidden() {
        return Ok(None);
    }
    let mut dir = files.to_path_buf();
    for segment in path.segments() {
        if !has_exact_child(&dir, segment)? {
            return Ok(None);
        }
        dir.push(segment);
    }
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(&dir)(e)),
    };
    let Some(kind) = listable(&meta) else {
        return Ok(None);
    };
    Ok(Some(Found {
        path: path.clone(),
        fs_path: dir,
        kind,
        meta,
    }))
}

/// The listable children of the folder `found` (and their descendants with `recursive`).
/// Dot-entries, non-UTF-8 names, names the wire cannot carry, symlinks and special files are
/// skipped. An entry that vanishes during the walk was deleted and is skipped too, unless
/// `vanished` answers an error for its path (a folder that must not be missing): then the walk
/// ends with that error.
pub(crate) fn walk(
    found: &Found,
    recursive: bool,
    out: &mut Vec<Found>,
    vanished: &dyn Fn(&WirePath) -> Option<Error>,
) -> Result<()> {
    let dir = &found.fs_path;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return vanished(&found.path).map_or(Ok(()), Err);
        }
        Err(e) => return Err(io_err(dir)(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io_err(dir))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(path) = found.path.join(&name) else {
            continue;
        };
        let fs_path = entry.path();
        let meta = match std::fs::symlink_metadata(&fs_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match vanished(&path) {
                Some(err) => return Err(err),
                None => continue,
            },
            Err(e) => return Err(io_err(&fs_path)(e)),
        };
        let Some(kind) = listable(&meta) else {
            continue;
        };
        let child = Found {
            path,
            fs_path,
            kind,
            meta,
        };
        if recursive && kind == Kind::Folder {
            walk(&child, true, out, vanished)?;
        }
        out.push(child);
    }
    Ok(())
}

/// A window of a walk ([`walk_window`]): what it lists and how many entries it collects.
pub(crate) struct Window<'a> {
    pub recursive: bool,
    /// Only folders: files are neither stat'ed nor collected.
    pub folders_only: bool,
    /// Collect at most this many entries.
    pub want: usize,
    /// Counts each `symlink_metadata` call.
    pub stated: &'a dyn Fn(usize),
}

/// Collect into `out` the listable entries below the folder `path` (at `dir` on disk) that come
/// after the anchor in the order of paths, in that order, until `out` holds `window.want`.
/// `rest` is the anchor below `path` (its segments after those of `path`); empty starts at the
/// beginning. Each folder on the way is read by names only (`read_dir` and the type of each
/// name, no stat), its names sorted by bytes: the order of [`WirePath`]. With `recursive` a
/// folder is followed by what is in it (a pre-order walk). Only the entries that are collected
/// are stat'ed. The anchor itself is not collected, nor anything before it. When the anchor or
/// a folder on its way is gone, the walk goes on with the names after it. Names are skipped as
/// in [`walk`]; a folder that vanished is skipped too.
pub(crate) fn walk_window(
    path: &WirePath,
    dir: &Path,
    rest: &[String],
    window: &Window<'_>,
    out: &mut Vec<Found>,
) -> Result<()> {
    if out.len() >= window.want {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(dir))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // The type of the name itself (a symlink stays a symlink), mostly without a stat.
        let is_dir = match entry.file_type() {
            Ok(ft) if ft.is_dir() => true,
            Ok(ft) if ft.is_file() => false,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&entry.path())(e)),
        };
        names.push((name, is_dir));
    }
    names.sort_unstable();
    for (name, is_dir) in names {
        if out.len() >= window.want {
            break;
        }
        let Ok(child) = path.join(&name) else {
            continue;
        };
        let fs_path = dir.join(&name);
        if let Some(first) = rest.first() {
            match name.as_str().cmp(first.as_str()) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => {
                    // The anchor or a folder on its way: shown before, its later names not.
                    if window.recursive && is_dir {
                        walk_window(&child, &fs_path, &rest[1..], window, out)?;
                    }
                    continue;
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        if window.folders_only && !is_dir {
            continue;
        }
        (window.stated)(1);
        let meta = match std::fs::symlink_metadata(&fs_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&fs_path)(e)),
        };
        let Some(kind) = listable(&meta) else {
            continue;
        };
        if window.folders_only && kind != Kind::Folder {
            continue;
        }
        out.push(Found {
            path: child.clone(),
            fs_path: fs_path.clone(),
            kind,
            meta,
        });
        if window.recursive && kind == Kind::Folder {
            walk_window(&child, &fs_path, &[], window, out)?;
        }
    }
    Ok(())
}
