//! The recycle bin for the Mac app: `GET /recycle` lists what the user deleted, newest first.
//! Any valid token reads, a read token too.
//!
//! An item is a deleted file, or a deleted folder with what was in it, as
//! [`Store::recycle_items`] finds it: old versions of replaced files, the folders a delete left
//! with only dot-files, hidden paths, lines that can't be read and slots that are gone are no
//! items. A list takes the store lock with recovery and the gates (423 `not_imported` and
//! `scan_hold`), like the other reads, and does not scan.

use std::cmp::Reverse;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::browse::run;
use super::routes::{Refusal, account_of, bad_request, number, query};
use crate::api::files::gate_locked;
use crate::server::AppState;
use crate::store::{ItemKind, RecycleItem};

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
    after: Option<String>,
}

/// The place of an item in the list: newest first, then by path.
fn order(deleted_at: u64, path: &str) -> (Reverse<u64>, &str) {
    (Reverse(deleted_at), path)
}

/// `after` as `(deletedAt, path)`: a key, `<deletedAt>/<path>`.
fn after_arg(text: &str) -> Result<(u64, String), Refusal> {
    text.split_once('/')
        .and_then(|(ms, path)| Some((ms.parse().ok()?, path.to_string())))
        .ok_or_else(|| Box::new(bad_request("after is not a key")))
}

/// `GET /recycle?limit=&after=`: `{"items", "more"}`, at most `limit` items (default and
/// maximum [`RECYCLE_LIMIT`], 0 is 1), newest first. `after` is the key of the last item of
/// the previous page: the page starts with the item that comes after it in this order, also
/// when that item is gone since. `more` tells that there are more items.
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
        let limit = number(q.limit.as_deref(), "limit", RECYCLE_LIMIT as u64)?;
        let after = q.after.as_deref().map(after_arg).transpose()?;
        Ok((limit, after))
    }) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let limit = usize::try_from(limit).map_or(RECYCLE_LIMIT, |l| l.clamp(1, RECYCLE_LIMIT));
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
    Json(serde_json::json!({"items": page, "more": more})).into_response()
}

fn items_of(state: &AppState, slug: &str) -> crate::Result<Vec<RecycleItem>> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, &store)?;
    store.recycle_items(&guard)
}
