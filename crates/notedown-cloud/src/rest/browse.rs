//! Browse and download for the Mac app: `GET /tree`, `GET /entries/{id}` and `GET`/`HEAD
//! /files/{id}`. Any valid token reads, a read token too.
//!
//! Each request works as a device listing does: the store lock with recovery, the gates
//! ([`gate_locked`]: 423 `scan_hold` or `not_imported`), then a scan of what it shows, so a file
//! made by hand is journaled (origin `scan`) and has its id before the app sees it. Names are
//! exact: another spelling is no entry, and nothing hidden is listed or served. A download opens
//! the file under the lock and streams it after the lock is dropped, so a replace while it
//! streams never mixes old and new bytes. The one validator is the md5: `md5` in an entry, the
//! strong `ETag: "<md5>"` of a download.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::routes::{Refusal, account_of, bad_request, error, internal, not_modified, query};
use crate::Error;
use crate::api::files::gate_locked;
use crate::api::oss::send_tagged;
use crate::server::AppState;
use crate::store::{Entry, Kind, ScanOptions, Store, WirePath, WriteGuard, trees};

/// One file or folder as the app sees it. `path` is the wire path (`NOTE/Note/a.note`), `md5`
/// only for files, `writable` whether the app may change it ([`trees::writable`]).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppEntry {
    id: u64,
    path: String,
    name: String,
    kind: Kind,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    md5: Option<String>,
    mtime_ms: u64,
    writable: bool,
}

impl From<&Entry> for AppEntry {
    fn from(e: &Entry) -> AppEntry {
        AppEntry {
            id: e.id,
            path: e.path.display(),
            name: e.name.clone(),
            kind: e.tag,
            size: e.size,
            md5: e.content_hash.clone(),
            mtime_ms: e.last_update_time,
            writable: trees::writable(&e.path),
        }
    }
}

/// The answer to a store error: 423 `scan_hold` while a scan holds the account, 423
/// `not_imported` while the protect gate is closed, else 500 `internal`.
fn store_error(e: Error) -> Response {
    match e {
        Error::ScanHold(_) => error(StatusCode::LOCKED, "scan_hold"),
        Error::GateClosed(_) => error(StatusCode::LOCKED, "not_imported"),
        e => internal(e),
    }
}

/// Runs store work off the runtime; an error is [`store_error`], a panic a 500.
async fn run<T: Send + 'static>(
    work: impl FnOnce() -> crate::Result<T> + Send + 'static,
) -> Result<T, Refusal> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Box::new(store_error(e))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// The scan of `scope` (direct children unless `recursive`) under `guard`; a hold is
/// `Error::ScanHold`. Returns its walk for [`Store::list_walked`].
fn scan(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    scope: &WirePath,
    recursive: bool,
) -> crate::Result<Option<crate::store::Walk>> {
    let opts = ScanOptions {
        scope: scope.clone(),
        recursive,
        dry_run: false,
        accept: false,
        min_files: state.config.scan_min_files,
    };
    state
        .run_scan_walked(store, guard, &opts)
        .map(|(_, walk)| walk)
}

/// The entry of `id` after a scan of its parent folder (direct children), so a hand change of
/// it is journaled first; `None` when the id is unknown, gone from disk or hidden.
fn entry_of(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    id: u64,
) -> crate::Result<Option<Entry>> {
    let Some((_, path)) = store.kind_and_path_of(id)? else {
        return Ok(None);
    };
    if path.is_hidden() {
        return Ok(None);
    }
    scan(
        state,
        store,
        guard,
        &path.parent().unwrap_or_else(WirePath::root),
        false,
    )?;
    let Some(path) = store.path_of(id)? else {
        return Ok(None);
    };
    Ok(store.entry(guard, &path)?.filter(|e| e.id == id))
}

/// The id of a route path; anything but a number is a 400.
fn id_arg(text: &str) -> Result<u64, Refusal> {
    text.parse()
        .map_err(|_| Box::new(bad_request("the id is not a number")))
}

#[derive(Deserialize)]
struct TreeQuery {
    path: Option<String>,
    recursive: Option<String>,
}

/// What a tree request found.
enum Tree {
    Listed { seq: u64, entries: Vec<AppEntry> },
    NotFound,
    NotAFolder,
}

fn tree_of(state: &AppState, slug: &str, path: &WirePath, recursive: bool) -> crate::Result<Tree> {
    if path.is_hidden() {
        return Ok(Tree::NotFound);
    }
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, &store)?;
    let walk = scan(state, &store, &guard, path, recursive)?;
    if !path.is_root() {
        match store.entry(&guard, path)? {
            Some(e) if e.tag == Kind::Folder => {}
            Some(_) => return Ok(Tree::NotAFolder),
            None => return Ok(Tree::NotFound),
        }
    }
    let entries = store.list_walked(&guard, path, recursive, walk)?;
    // After the scan, under the same guard: the listing holds every event up to `seq`.
    let seq = state.journal.last_seq()?;
    Ok(Tree::Listed {
        seq,
        entries: entries.iter().map(AppEntry::from).collect(),
    })
}

/// `GET /tree?path=&recursive=`: `{"seq", "entries"}`, the children of the folder at `path`
/// (default the root), or all its descendants with `recursive=true`. `seq` is the journal's last
/// seq, read under the same lock: following `/journal` from `seq + 1` misses nothing. 404
/// `not_found`, 409 `not_a_folder`.
pub(super) async fn tree(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let (path, recursive) = match query::<TreeQuery>(&uri).and_then(|q| {
        let path = WirePath::parse(q.path.as_deref().unwrap_or(""))
            .map_err(|_| Box::new(bad_request("path is not a valid path")))?;
        let recursive = match q.recursive.as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(Box::new(bad_request("recursive is true or false"))),
        };
        Ok((path, recursive))
    }) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let st = state.clone();
    match run(move || tree_of(&st, &account.slug, &path, recursive)).await {
        Ok(Tree::Listed { seq, entries }) => {
            Json(serde_json::json!({"seq": seq, "entries": entries})).into_response()
        }
        Ok(Tree::NotFound) => error(StatusCode::NOT_FOUND, "not_found"),
        Ok(Tree::NotAFolder) => error(StatusCode::CONFLICT, "not_a_folder"),
        Err(r) => *r,
    }
}

/// `GET /entries/{id}`: the entry object of `/tree`; 404 `not_found`.
pub(super) async fn entry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let id = match id_arg(&id) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let st = state.clone();
    let found = run(move || {
        let store = st.store(&account.slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(&st, &store)?;
        entry_of(&st, &store, &guard, id)
    })
    .await;
    match found {
        Ok(Some(e)) => Json(AppEntry::from(&e)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "not_found"),
        Err(r) => *r,
    }
}

/// What a download found.
enum Download {
    /// The open file, its size and its strong ETag.
    File(std::fs::File, u64, String),
    Folder,
    Missing,
}

/// `GET` and `HEAD /files/{id}`: the file's bytes, streamed from the file opened under the lock,
/// with `Content-Length` and the strong `ETag: "<md5>"`. One `Range` (206, 416), honored when
/// `If-Range` is absent or that ETag; 304 for a matching `If-None-Match`. 404 `not_found`, 409
/// `is_a_folder`. `HEAD` has no body.
pub(super) async fn file(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let id = match id_arg(&id) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let st = state.clone();
    let found = run(move || {
        let store = st.store(&account.slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(&st, &store)?;
        let Some(e) = entry_of(&st, &store, &guard, id)? else {
            return Ok(Download::Missing);
        };
        if e.tag == Kind::Folder {
            return Ok(Download::Folder);
        }
        let (Some(md5), Some((file, size))) = (e.content_hash, store.open_file(&guard, &e.path)?)
        else {
            return Ok(Download::Missing);
        };
        drop(guard);
        Ok(Download::File(file, size, format!("\"{md5}\"")))
    })
    .await;
    let (file, size, etag) = match found {
        Ok(Download::File(file, size, etag)) => (file, size, etag),
        Ok(Download::Folder) => return error(StatusCode::CONFLICT, "is_a_folder"),
        Ok(Download::Missing) => return error(StatusCode::NOT_FOUND, "not_found"),
        Err(r) => return *r,
    };
    if not_modified(&headers, &etag) {
        drop(file);
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        if let Ok(v) = HeaderValue::from_str(&etag) {
            r.headers_mut().insert(header::ETAG, v);
        }
        return r;
    }
    let r = send_tagged(&headers, file, size, &etag);
    if method == Method::HEAD {
        // The headers stay (`Content-Length` included); the file closes with the body.
        let (parts, _) = r.into_parts();
        return Response::from_parts(parts, Body::empty());
    }
    r
}
