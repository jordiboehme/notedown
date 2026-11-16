//! The REST target check: what the app may write, on store primitives and stricter than the
//! device's lookup. Names are exact: another spelling of an existing name is refused, never
//! mapped to the stored one.
//!
//! - [`parse_target`] (no lock): the path parses and names something, no segment is a dot name
//!   (400 `bad_name`) or longer than [`MAX_NAME_BYTES`] (400 `name_too_long`), and it lies inside
//!   a device folder tree, not on a tree root (403 `not_writable`).
//! - [`check_target`] (under the store lock): the same again, then no segment collides in case
//!   or Unicode normalization with a sibling (409 `name_taken` with `existing`), and what is on
//!   the way fits: a file on the way is 409 `not_a_folder`, a folder at a file target 409
//!   `is_a_folder`, a file at a folder target and anything at the target of a move 409
//!   `name_taken`. A symlink (or another special entry) is never written through or replaced:
//!   on the way it is `not_a_folder`, at the target `name_taken`.
//! - [`check_source`]: the entry a move or delete takes must lie inside a device folder tree
//!   and not be a tree root (403 `not_writable`).
//! - [`check_move`] (under the store lock): the target of a move. A folder never moves into
//!   itself (400 `into_itself`). In the source's own folder the new name may differ from the
//!   old one only in case or Unicode normalization; any other entry with that name is
//!   `name_taken`. Elsewhere it is [`check_target`] with nothing allowed at the target.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::errors::name_taken;
use crate::Error;
use crate::api::files::MAX_NAME_BYTES;
use crate::store::{Store, StoreGuard, WirePath, trees};

/// Why the app may not write a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteRefusal {
    /// 400 `bad_name`: no valid path, or a dot name.
    BadName,
    /// 400 `name_too_long`: a segment over [`MAX_NAME_BYTES`].
    NameTooLong,
    /// 403 `not_writable`: outside the device folder trees, or a tree root.
    NotWritable,
    /// 409 `not_a_folder`: a file (or a symlink) where a folder must be.
    NotAFolder,
    /// 409 `is_a_folder`: a folder where the file goes.
    IsAFolder,
    /// 409 `name_taken`: `existing` is in the way.
    NameTaken { existing: String },
    /// 400 `into_itself`: a folder moved into itself.
    IntoItself,
}

impl WriteRefusal {
    pub(crate) fn response(&self) -> Response {
        let (status, code) = match self {
            WriteRefusal::BadName => (StatusCode::BAD_REQUEST, "bad_name"),
            WriteRefusal::NameTooLong => (StatusCode::BAD_REQUEST, "name_too_long"),
            WriteRefusal::NotWritable => (StatusCode::FORBIDDEN, "not_writable"),
            WriteRefusal::NotAFolder => (StatusCode::CONFLICT, "not_a_folder"),
            WriteRefusal::IsAFolder => (StatusCode::CONFLICT, "is_a_folder"),
            WriteRefusal::IntoItself => (StatusCode::BAD_REQUEST, "into_itself"),
            WriteRefusal::NameTaken { existing } => return name_taken(existing),
        };
        (status, Json(json!({ "error": code }))).into_response()
    }
}

/// What the target of a write becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// A file: a folder there is `is_a_folder`, a file there is replaced.
    File,
    /// A folder: a folder there is fine, a file there is `name_taken`.
    Folder,
    /// A new entry (a move): anything there is `name_taken`.
    Absent,
}

/// The checks of a path that need no store: see the module docs.
fn check_names(path: &WirePath) -> Result<(), WriteRefusal> {
    if path.is_root() {
        return Err(WriteRefusal::BadName);
    }
    for segment in path.segments() {
        if segment.starts_with('.') {
            return Err(WriteRefusal::BadName);
        }
        if segment.len() > MAX_NAME_BYTES {
            return Err(WriteRefusal::NameTooLong);
        }
    }
    if !trees::writable(path) {
        return Err(WriteRefusal::NotWritable);
    }
    Ok(())
}

/// The wire path `text` that the app may write, checked without the store (module docs).
pub(crate) fn parse_target(text: &str) -> Result<WirePath, WriteRefusal> {
    let path = WirePath::parse_named(text).map_err(|_| WriteRefusal::BadName)?;
    check_names(&path)?;
    Ok(path)
}

/// The whole check of `path` for a write that makes it a `want`, under the store lock (module
/// docs). `Ok(Err(_))` is a refusal, `Err(_)` a store failure.
pub(crate) fn check_target(
    store: &Store,
    guard: &StoreGuard,
    path: &WirePath,
    want: Want,
) -> crate::Result<Result<(), WriteRefusal>> {
    if let Err(r) = check_names(path) {
        return Ok(Err(r));
    }
    match store.check_path(guard, path) {
        Ok(()) => {}
        Err(Error::NameCollision { existing, .. }) => {
            return Ok(Err(WriteRefusal::NameTaken { existing }));
        }
        Err(Error::Invalid { .. }) => return Ok(Err(WriteRefusal::BadName)),
        Err(e) => return Err(e),
    }
    // Every segment is now there with exactly its bytes, or nothing is there under any
    // spelling. Look at what is there, without following a link.
    let mut cur = WirePath::root();
    for segment in path.segments() {
        cur = cur.join(segment)?;
        let last = &cur == path;
        let fs = store.fs_path(&cur);
        let meta = match std::fs::symlink_metadata(&fs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(Error::Io {
                    path: fs,
                    source: e,
                });
            }
        };
        let kind = meta.file_type();
        let taken = || WriteRefusal::NameTaken {
            existing: segment.clone(),
        };
        let refusal = if kind.is_dir() {
            match (last, want) {
                (true, Want::File) => Some(WriteRefusal::IsAFolder),
                (true, Want::Absent) => Some(taken()),
                _ => None,
            }
        } else if kind.is_file() {
            match (last, want) {
                (true, Want::File) => None,
                (true, Want::Folder | Want::Absent) => Some(taken()),
                (false, _) => Some(WriteRefusal::NotAFolder),
            }
        } else if last {
            Some(taken())
        } else {
            Some(WriteRefusal::NotAFolder)
        };
        if let Some(r) = refusal {
            return Ok(Err(r));
        }
    }
    Ok(Ok(()))
}

/// Whether the app may move or delete the entry at `path` (module docs).
pub(crate) fn check_source(path: &WirePath) -> Result<(), WriteRefusal> {
    if trees::writable(path) {
        Ok(())
    } else {
        Err(WriteRefusal::NotWritable)
    }
}

/// The check of the target `to` of moving the entry at `from` (a folder when `folder`), under
/// the store lock (module docs). `to` is not `from`. `Ok(Err(_))` is a refusal, `Err(_)` a
/// store failure.
pub(crate) fn check_move(
    store: &Store,
    guard: &StoreGuard,
    from: &WirePath,
    folder: bool,
    to: &WirePath,
) -> crate::Result<Result<(), WriteRefusal>> {
    if let Err(r) = check_names(to) {
        return Ok(Err(r));
    }
    if folder && to.starts_with(from) {
        return Ok(Err(WriteRefusal::IntoItself));
    }
    let (Some(parent), Some(old), Some(new)) = (from.parent(), from.name(), to.name()) else {
        return Ok(Err(WriteRefusal::BadName));
    };
    if to.parent().as_ref() != Some(&parent) {
        return check_target(store, guard, to, Want::Absent);
    }
    // A rename in place: the source's own name in another spelling is no collision (the file
    // system may find the source under it), any other entry is.
    match store.check_rename(guard, &parent, new, old) {
        Ok(()) => Ok(Ok(())),
        Err(Error::NameCollision { existing, .. }) => Ok(Err(WriteRefusal::NameTaken { existing })),
        Err(Error::Invalid { .. }) => Ok(Err(WriteRefusal::BadName)),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_before_the_trees() {
        assert_eq!(parse_target(""), Err(WriteRefusal::BadName));
        assert_eq!(parse_target("NOTE/Note/../a"), Err(WriteRefusal::BadName));
        assert_eq!(parse_target("NOTE/Note/.a"), Err(WriteRefusal::BadName));
        assert_eq!(parse_target(".a/b"), Err(WriteRefusal::BadName));
        let long = format!("NOTE/Note/{}", "x".repeat(256));
        assert_eq!(parse_target(&long), Err(WriteRefusal::NameTooLong));
        assert_eq!(parse_target("NOTE/Note"), Err(WriteRefusal::NotWritable));
        assert_eq!(parse_target("NOTE/a"), Err(WriteRefusal::NotWritable));
        assert_eq!(
            parse_target("/NOTE//Note/a/"),
            Ok(WirePath::parse("NOTE/Note/a").unwrap())
        );
    }
}
