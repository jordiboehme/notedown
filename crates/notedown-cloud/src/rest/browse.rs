//! Browse and download for the Mac app: `GET /tree`, `GET /entries/{id}` and `GET`/`HEAD
//! /files/{id}`. Any valid token reads, a read token too.
//!
//! Each request works as a device listing does: the store lock with recovery, the gates
//! ([`gate_locked`]: 423 `scan_hold` or `not_imported`), then a scan of what it shows, so a file
//! made by hand is journaled (origin `scan`) and has its id before the app sees it. A
//! continuation page of `/tree` does not scan the folder: it scans only the folder of an entry
//! that was made, changed or renamed by hand since ([`Store::list_window`]). Names are
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

use super::check::WriteRefusal;
use super::errors::write_error;
use super::paging::{self, BadCursor, CursorKind};
use super::routes::{Refusal, account_of, bad_request, error, internal, not_modified, query};
use crate::Error;
use crate::api::files::gate_locked;
use crate::api::oss::send_tagged;
use crate::server::AppState;
use crate::store::{Entry, Kind, Page, ScanOptions, Store, WirePath, WriteGuard, trees};

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

/// The answer to a store error: that of a write ([`write_error`]). A read only meets its
/// `ScanHold` (423 `scan_hold`), `GateClosed` (423 `not_imported`), `StoreDamaged` (503
/// `store_damaged`) and I/O (500 `internal`) arms.
fn store_error(e: Error) -> Response {
    write_error(e)
}

/// Runs store work off the runtime; an error is [`store_error`], a panic a 500.
pub(super) async fn run<T: Send + 'static>(
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
    entry_scanned(state, store, guard, &path, Some(id))
}

/// The entry at `path` after a scan of its parent folder (direct children); `None` when
/// nothing is there or the path is hidden. With `id`, an entry of another id is none.
fn entry_scanned(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    path: &WirePath,
    id: Option<u64>,
) -> crate::Result<Option<Entry>> {
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
    let Some(id) = id else {
        return store.entry(guard, path);
    };
    let Some(path) = store.path_of(id)? else {
        return Ok(None);
    };
    Ok(store.entry(guard, &path)?.filter(|e| e.id == id))
}

/// The id of a route path; anything but a number is a 400.
pub(super) fn id_arg(text: &str) -> Result<u64, Refusal> {
    text.parse()
        .map_err(|_| Box::new(bad_request("the id is not a number")))
}

/// The default `limit` of `GET /tree`.
pub const TREE_LIMIT: usize = 500;

/// The largest `limit` of `GET /tree`.
pub const TREE_LIMIT_MAX: usize = 1000;

#[derive(Deserialize)]
struct TreeQuery {
    path: Option<String>,
    recursive: Option<String>,
    kind: Option<String>,
    limit: Option<String>,
    cursor: Option<String>,
}

/// What a tree request asks for.
struct TreeAsk {
    path: WirePath,
    recursive: bool,
    folders_only: bool,
    limit: usize,
    /// The last entry of the page before.
    after: Option<WirePath>,
}

/// What a tree request found.
enum Tree {
    Listed {
        seq: u64,
        entries: Vec<AppEntry>,
        next: Option<String>,
    },
    NotFound,
    NotAFolder,
}

/// The anchor of a tree cursor, which must lie in the scope of the request: a wire path, not
/// hidden, below `path`, and without `recursive` a direct child of it.
fn tree_anchor(cursor: &str, path: &WirePath, recursive: bool) -> Result<WirePath, BadCursor> {
    let text = paging::decode(CursorKind::Tree, cursor)?;
    let anchor = WirePath::parse(&text).map_err(|_| BadCursor)?;
    let inside = if recursive {
        anchor.starts_with(path) && anchor != *path
    } else {
        anchor.parent().as_ref() == Some(path)
    };
    if anchor.is_hidden() || !inside {
        return Err(BadCursor);
    }
    Ok(anchor)
}

/// The query of `GET /tree`.
fn tree_ask(uri: &Uri) -> Result<TreeAsk, Refusal> {
    let q = query::<TreeQuery>(uri)?;
    let path = WirePath::parse(q.path.as_deref().unwrap_or(""))
        .map_err(|_| Box::new(bad_request("path is not a valid path")))?;
    let recursive = match q.recursive.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(Box::new(bad_request("recursive is true or false"))),
    };
    let folders_only = match q.kind.as_deref() {
        None => false,
        Some("folder") => true,
        Some(_) => return Err(Box::new(bad_request("kind is folder"))),
    };
    let limit = paging::limit(q.limit.as_deref(), TREE_LIMIT, TREE_LIMIT_MAX)?;
    let after = q
        .cursor
        .as_deref()
        .map(|c| tree_anchor(c, &path, recursive))
        .transpose()
        .map_err(|_| Box::new(paging::bad_cursor()))?;
    Ok(TreeAsk {
        path,
        recursive,
        folders_only,
        limit,
        after,
    })
}

/// One page of the tree. The first page (no cursor) scans the scope, lists it from the scan's
/// walk and cuts the page; only a first page that is also the last prunes the MD5 cache of a
/// whole-tree listing. A continuation page does not scan the scope: it walks a window after
/// the anchor ([`Store::list_window`]), and only the folder of an entry made, changed or
/// renamed by hand since is scanned (its direct children, once per page).
fn tree_of(state: &AppState, slug: &str, ask: TreeAsk) -> crate::Result<Tree> {
    let TreeAsk {
        path,
        recursive,
        folders_only,
        limit,
        after,
    } = ask;
    if path.is_hidden() {
        return Ok(Tree::NotFound);
    }
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, &store)?;
    let (entries, more) = match after {
        None => {
            let walk = scan(state, &store, &guard, &path, recursive)?;
            if !path.is_root() {
                match store.entry(&guard, &path)? {
                    Some(e) if e.tag == Kind::Folder => {}
                    Some(_) => return Ok(Tree::NotAFolder),
                    None => return Ok(Tree::NotFound),
                }
            }
            let all = store.list_unpruned(&guard, &path, recursive, walk)?;
            let shown = |e: &Entry| !folders_only || e.tag == Kind::Folder;
            let more = all.iter().filter(|e| shown(e)).count() > limit;
            if !more && path.is_root() && recursive {
                store.prune_hashes(&guard, &all)?;
            }
            let page: Vec<Entry> = all.into_iter().filter(shown).take(limit).collect();
            (page, more)
        }
        Some(anchor) => match store.list_window(
            &guard,
            &path,
            recursive,
            folders_only,
            Some(&anchor),
            limit,
            &mut |folder| scan(state, &store, &guard, folder, false).map(|_| ()),
        )? {
            Page::Listed(entries, more) => (entries, more),
            Page::NotFound => return Ok(Tree::NotFound),
            Page::NotAFolder => return Ok(Tree::NotAFolder),
        },
    };
    // After the scan, under the same guard: the page holds every event up to `seq`.
    let seq = state.journal.last_seq()?;
    let next = match entries.last() {
        Some(last) if more => Some(paging::encode(CursorKind::Tree, &last.path.display())),
        _ => None,
    };
    Ok(Tree::Listed {
        seq,
        entries: entries.iter().map(AppEntry::from).collect(),
        next,
    })
}

/// `GET /tree?path=&recursive=&kind=&limit=&cursor=`: `{"seq", "entries", "next"}`, one page of
/// the children of the folder at `path` (default the root), or of all its descendants with
/// `recursive=true`, in the order of their paths (segment by segment, each by its bytes; a
/// pre-order walk). `kind=folder` lists only folders. `limit` is [`TREE_LIMIT`] by default and
/// at most [`TREE_LIMIT_MAX`], 0 is 1. `next` is the cursor of the next page, `null` on the
/// last. `seq` is the journal's last seq, read under the same lock as the page: following
/// `/events` from `seq + 1` misses nothing of what the page shows. 404 `not_found`, 409
/// `not_a_folder`, 400 `bad_cursor` for a cursor that is garbled, of another route or outside
/// the scope.
pub(super) async fn tree(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let ask = match tree_ask(&uri) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let st = state.clone();
    match run(move || tree_of(&st, &account.slug, ask)).await {
        Ok(Tree::Listed { seq, entries, next }) => {
            Json(serde_json::json!({"seq": seq, "entries": entries, "next": next})).into_response()
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

#[derive(Deserialize)]
struct EntryQuery {
    path: Option<String>,
}

/// `GET /entries?path=`: the entry object of `/entries/{id}` for the entry at `path`, after a
/// scan of its parent folder. 404 `not_found` for an unknown, hidden or gone path, 400
/// `bad_name` for a path that does not parse (or is missing).
pub(super) async fn entry_by_path(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let path = match query::<EntryQuery>(&uri) {
        Ok(q) => q.path.as_deref().map(WirePath::parse_named),
        Err(r) => return *r,
    };
    let Some(Ok(path)) = path else {
        return WriteRefusal::BadName.response();
    };
    let st = state.clone();
    let found = run(move || {
        let store = st.store(&account.slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(&st, &store)?;
        entry_scanned(&st, &store, &guard, &path, None)
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
