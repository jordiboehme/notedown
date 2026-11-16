//! The recycle bin for the Mac app: `GET /recycle` lists what the user deleted, newest first,
//! `POST /recycle/restore` brings one item back and `POST /recycle/empty` removes every deleted
//! file and folder for good. Any valid token reads, a read token too; a restore and Empty need
//! a write token.
//!
//! An item is a deleted file, or a deleted folder with what was in it, as
//! [`Store::recycle_items`] finds it: old versions of replaced files, the folders a delete left
//! with only dot-files, hidden paths, lines that can't be read and slots that are gone are no
//! items. A list takes the store lock with recovery and the gates (423 `not_imported` and
//! `scan_hold`), like the other reads, and does not scan.
//!
//! A restore is a write like those of [`super::write`]: before the store lock the token (403
//! `read_only_token`), the sync mode, the body (400 `bad_request`) and a `path` it names
//! ([`parse_target`]). Then in one blocking task under one store lock: the gates and the device
//! sync lock ([`locked`]), the item by its key (404 `not_found` when the bin has no such item:
//! a key already restored or pruned), the item's old path when the body names none, the scans
//! of the folders on the way, [`check_target`] with nothing allowed at the target (a folder is
//! never merged into another one), the quota ([`quota_locked`], the bytes of the item), the
//! plan ([`Store::plan_restore`](crate::store::Store::plan_restore)) with its page changes and
//! the commit. Origin `app`, no device. The restored entry gets a new id, as does everything in
//! a restored folder.
//!
//! Empty is a write too: the token and the sync mode, then under one store lock the gates and
//! the device sync lock ([`locked`]), the items of the bin, the prune with
//! [`PrunePlan::Empty`] under that same lock ([`Store::prune_recycle_with`]) and the lines that
//! are left. It takes every `deleted` line, also those the bin does not list, and never an old
//! version of a replaced file. One `recycle.pruned` event per removed line, one op id, origin
//! `app`. It changes nothing in `files/`, so the usage stays.
//!
//! [`Store::prune_recycle_with`]: crate::store::Store::prune_recycle_with

use std::cmp::Reverse;
use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::browse::run;
use super::check::{Want, check_target, parse_target};
use super::paging::{self, BadCursor, CursorKind};
use super::routes::{account_of, bad_request, query};
use super::write::{Stop, json_body, locked, quota_locked, writer};
use crate::api::files::{gate_locked, scan_way};
use crate::auth::Account;
use crate::journal::Origin;
use crate::server::AppState;
use crate::store::{
    IndexLine, ItemKind, Kind, PendingOp, PrunePlan, RecycleItem, RecycleReason, WirePath,
};

/// The default and the largest `limit` of `GET /recycle`.
pub const RECYCLE_LIMIT: usize = 500;

/// One item of the bin as the app sees it. `md5` only for a file whose line has one, `files`
/// only for a folder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppItem {
    key: String,
    path: String,
    name: String,
    kind: ItemKind,
    deleted_at: u64,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<u64>,
}

impl From<RecycleItem> for AppItem {
    fn from(i: RecycleItem) -> AppItem {
        AppItem {
            key: i.key,
            path: i.path,
            name: i.name,
            kind: i.kind,
            deleted_at: i.deleted_at,
            size: i.size,
            md5: i.md5,
            files: i.files,
        }
    }
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<String>,
    cursor: Option<String>,
    /// The parameter before `cursor`: refused, so an old client does not get page 1 forever.
    after: Option<String>,
}

/// The place of an item in the list: newest first, then by path (compared as strings).
fn order(deleted_at: u64, path: &str) -> (Reverse<u64>, &str) {
    (Reverse(deleted_at), path)
}

/// The place of a recycle cursor's anchor, a key (`<deletedAt>/<path>`): `(deletedAt, path)`.
fn cursor_arg(cursor: &str) -> Result<(u64, String), BadCursor> {
    let key = paging::decode(CursorKind::Recycle, cursor)?;
    key.split_once('/')
        .and_then(|(ms, path)| Some((ms.parse().ok()?, path.to_string())))
        .ok_or(BadCursor)
}

/// `GET /recycle?limit=&cursor=`: `{"items", "next"}`, at most `limit` items (default and
/// maximum [`RECYCLE_LIMIT`], 0 is 1), newest first. `cursor` is the `next` of the previous
/// page, which names its last item: the page starts with the item that comes after it in this
/// order, also when that item is gone since. `next` is `null` on the last page. Each page is cut
/// from the whole bin index (fine at the size of a bin). 400 `bad_cursor` for a cursor that is
/// garbled or of another route, 400 `bad_request` for the old `after`.
pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let (limit, after) = match query::<ListQuery>(&uri).and_then(|q| {
        if q.after.is_some() {
            return Err(Box::new(bad_request(
                "after is gone: ask with cursor set to the next of the page before",
            )));
        }
        let limit = paging::limit(q.limit.as_deref(), RECYCLE_LIMIT, RECYCLE_LIMIT)?;
        let after = q
            .cursor
            .as_deref()
            .map(cursor_arg)
            .transpose()
            .map_err(|_| Box::new(paging::bad_cursor()))?;
        Ok((limit, after))
    }) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let st = state.clone();
    let items = match run(move || items_of(&st, &account.slug)).await {
        Ok(items) => items,
        Err(r) => return *r,
    };
    let mut page: Vec<AppItem> = items
        .into_iter()
        .filter(|i| {
            after
                .as_ref()
                .is_none_or(|(ms, path)| order(i.deleted_at, &i.path) > order(*ms, path))
        })
        .take(limit + 1)
        .map(AppItem::from)
        .collect();
    let more = page.len() > limit;
    page.truncate(limit);
    let next = match page.last() {
        Some(last) if more => Some(paging::encode(CursorKind::Recycle, &last.key)),
        _ => None,
    };
    Json(serde_json::json!({"items": page, "next": next})).into_response()
}

fn items_of(state: &AppState, slug: &str) -> crate::Result<Vec<RecycleItem>> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, &store)?;
    store.recycle_items(&guard)
}

#[derive(Deserialize)]
struct RestoreBody {
    key: String,
    path: Option<String>,
}

/// `POST /recycle/restore` under the store lock (module docs).
fn restore_item(
    state: &AppState,
    account: &Account,
    key: &str,
    path: Option<WirePath>,
) -> crate::Result<Result<Value, Stop>> {
    let slug = &account.slug;
    let (store, guard) = match locked(state, slug)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let Some(item) = store
        .recycle_items(&guard)?
        .into_iter()
        .find(|i| i.key == key)
    else {
        return Ok(Err(Stop::NotFound));
    };
    let target = match path {
        Some(p) => p,
        None => match parse_target(&item.path) {
            Ok(p) => p,
            Err(r) => return Ok(Err(Stop::Target(r))),
        },
    };
    scan_way(state, &store, &guard, &target)?;
    if let Err(r) = check_target(&store, &guard, &target, Want::Absent)? {
        return Ok(Err(Stop::Target(r)));
    }
    if quota_locked(state, &store, &guard, account, 0, item.size)? {
        return Ok(Err(Stop::Quota));
    }
    let mut c = store.plan_restore(&guard, &item, &target, state.now_ms(), Origin::App)?;
    crate::pages::plan_page_restore(&store, &guard, &mut c)?;
    let PendingOp::Restore { id, kind, .. } = c.op else {
        return Err(crate::Error::Invalid {
            what: "restore",
            message: "the plan is no restore".into(),
        });
    };
    let seqs = store.commit(&guard, c, &state.journal)?;
    // The bytes came back to `files/`; under the lock, so the cache stays right.
    let kind = match kind {
        Kind::File => {
            state.adjust_usage(slug, item.size, 0);
            ItemKind::File
        }
        Kind::Folder => {
            state.invalidate_usage(slug);
            ItemKind::Folder
        }
    };
    Ok(Ok(
        json!({"id": id, "path": target.display(), "kind": kind, "seqs": seqs}),
    ))
}

/// `POST /recycle/restore {"key", "path"}`: brings the item `key` of the bin back at `path`,
/// by default the path it had (module docs). 200 `{"id", "path", "kind", "seqs"}`.
pub(super) async fn restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let restored = async {
        let account = writer(&state, &headers).await?;
        let body: RestoreBody = json_body(&body, "{\"key\", \"path\"}")?;
        let path = body
            .path
            .as_deref()
            .map(parse_target)
            .transpose()
            .map_err(|r| Box::new(r.response()))?;
        let st = state.clone();
        super::write::run(move || restore_item(&st, &account, &body.key, path)).await
    };
    match restored.await {
        Ok(answer) => Json(answer).into_response(),
        Err(r) => *r,
    }
}

/// `POST /recycle/empty` under the store lock (module docs): `removed` counts the items of the
/// bin that are gone after the prune and `bytes` their size before it; `kept` counts the
/// `deleted` lines that are still in the index (the prune could not remove them, for example
/// because of a symlink in their slot).
fn empty_bin(state: &AppState, slug: &str) -> crate::Result<Result<Value, Stop>> {
    let (store, guard) = match locked(state, slug)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let before = store.recycle_items(&guard)?;
    store.prune_recycle_with(
        &guard,
        &state.journal,
        PrunePlan::Empty,
        state.now_ms(),
        Origin::App,
        false,
    )?;
    let left: HashSet<String> = store
        .recycle_items(&guard)?
        .into_iter()
        .map(|i| i.key)
        .collect();
    let (removed, bytes) = before
        .iter()
        .filter(|i| !left.contains(&i.key))
        .fold((0u64, 0u64), |(n, b), i| (n + 1, b + i.size));
    let kept = store
        .recycle_lines(&guard)?
        .iter()
        .filter(|l| matches!(l, IndexLine::Entry(e) if e.reason == RecycleReason::Deleted))
        .count();
    Ok(Ok(
        json!({"removed": removed, "bytes": bytes, "kept": kept}),
    ))
}

/// `POST /recycle/empty`: removes every deleted file and folder of the bin for good (module
/// docs). A body is not read. 200 `{"removed", "bytes", "kept"}`.
pub(super) async fn empty(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let emptied = async {
        let account = writer(&state, &headers).await?;
        let st = state.clone();
        super::write::run(move || empty_bin(&st, &account.slug)).await
    };
    match emptied.await {
        Ok(answer) => Json(answer).into_response(),
        Err(r) => *r,
    }
}
