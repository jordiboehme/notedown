//! The app's upload: `PUT /files?path=` with the raw bytes, by the rules of `notedown cloud
//! put` ([`crate::put::commit_staged`], origin `app`, no device, so every bound device gets the
//! push). A write token only (403 `read_only_token`).
//!
//! Before the body is read (no store lock): the path ([`parse_target`]), the sync mode (409
//! `sync_refused`), a look at the gates (423 `not_imported`, `scan_hold`) and at the device sync
//! lock (423 `device_syncing` with `Retry-After`), `Content-Length` (411 `length_required`), the
//! size limit (413 `too_large`) and the quota from the cached usage (507 `quota_exceeded`).
//! Then the body streams into `accounts/<slug>/staging/app-<op id>/`; a body shorter than
//! `Content-Length` is 400 `bad_length`, one whose MD5 is not `X-Notedown-Md5` 400 `bad_md5`.
//! Then, in one blocking task under one store lock, in this order: the gates, the device sync
//! lock, the sync mode, the scans of the folders on the way, [`check_target`], the quota again
//! (a walk of the tree only when the cached usage is cold or near the quota), and the commit
//! (the precondition `If-None-Match: *` or `If-Match: "<md5>"`, 412 `precondition_failed`,
//! comes after the check for identical bytes, which is always 200 `unchanged`). The staging
//! folder goes away in every case, also when the client drops the connection.
//!
//! Only an account with `quota_bytes` has a quota; without it there is no check and no walk,
//! as for a device upload. The cached usage learns each upload's size change, so it stays warm.
//!
//! The other writes take a JSON body or an id: `POST /folders {"path"}`, `POST /move {"id",
//! "to"}` and `DELETE /entries/{id}`. Before the store lock: the token, the body (400
//! `bad_request`), the path ([`parse_target`]) and the sync mode. Then in one blocking task
//! under one store lock: the gates, the device sync lock and the sync mode as for an upload,
//! then the source ([`source_of`]: it is scanned first; 404 `not_found`, 403 `not_writable`
//! outside the trees or for a tree root, [`check_source`]), the scans of the folders on the way
//! to the target, the target check ([`check_target`], [`check_move`]), the plan with its page
//! changes, the commit. Origin `app`, no device. A move or a delete removes the folders it
//! leaves empty, as a device change does, and lists them in `removed`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::browse::id_arg;
use super::check::{Want, WriteRefusal, check_move, check_source, check_target, parse_target};
use super::errors::{device_syncing, write_error};
use super::routes::{Refusal, bad_request, error, internal, query, writer_of};
use crate::SyncMode;
use crate::api::files::{create_folders, gate_locked, scan_way, source_of};
use crate::auth::Account;
use crate::journal::Origin;
use crate::put::{Precondition, PutOutcome, PutResult, commit_staged};
use crate::server::AppState;
use crate::store::{Kind, PendingOp, Store, WirePath, WriteGuard};
use crate::sync::staging::{STAGING_DIR, StreamFile};

/// The header with the MD5 the client computed (lowercase or uppercase hex).
pub const MD5_HEADER: &str = "x-notedown-md5";

/// The staging folder of one upload, removed when this is dropped: at the end of the commit,
/// on a refusal, and when the handler future is dropped (a cut connection).
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "notedown cloud: cannot remove the staging folder {}: {e}",
                self.0.display()
            );
        }
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    path: Option<String>,
}

/// Why a look or the commit refuses a write.
enum Stop {
    Target(WriteRefusal),
    NotFound,
    SyncRefused,
    DeviceSyncing,
    Quota,
    NotImported,
    ScanHold,
}

impl Stop {
    fn response(self) -> Response {
        match self {
            Stop::Target(r) => r.response(),
            Stop::NotFound => error(StatusCode::NOT_FOUND, "not_found"),
            Stop::SyncRefused => error(StatusCode::CONFLICT, "sync_refused"),
            Stop::DeviceSyncing => device_syncing(),
            Stop::Quota => error(StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded"),
            Stop::NotImported => error(StatusCode::LOCKED, "not_imported"),
            Stop::ScanHold => error(StatusCode::LOCKED, "scan_hold"),
        }
    }
}

/// The precondition of the request: `If-None-Match: *` or one strong `If-Match: "<md5>"`. A
/// weak tag (`W/"..."`) never matches: the md5 is a strong validator. Other forms are a 400.
fn precondition_of(headers: &HeaderMap) -> Result<Precondition, Refusal> {
    let one = |name: header::HeaderName| -> Result<Option<String>, Refusal> {
        let mut values = headers.get_all(name.clone()).iter();
        let Some(v) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err(Box::new(bad_request(&format!("one {name} header at most"))));
        }
        v.to_str()
            .map(|v| Some(v.trim().to_string()))
            .map_err(|_| Box::new(bad_request(&format!("{name} is not text"))))
    };
    let none_match = one(header::IF_NONE_MATCH)?;
    let if_match = one(header::IF_MATCH)?;
    match (none_match, if_match) {
        (None, None) => Ok(Precondition::None),
        (Some(_), Some(_)) => Err(Box::new(bad_request("If-None-Match or If-Match, not both"))),
        (Some(v), None) if v == "*" => Ok(Precondition::Absent),
        (Some(_), None) => Err(Box::new(bad_request("If-None-Match takes only *"))),
        (None, Some(v)) => {
            if v.starts_with("W/") {
                // Kept as it is: no md5 equals it, so the precondition fails.
                return Ok(Precondition::Md5(v));
            }
            match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                Some(md5) if is_md5(md5) => Ok(Precondition::Md5(md5.to_string())),
                _ => Err(Box::new(bad_request("If-Match takes one \"<md5>\""))),
            }
        }
    }
}

fn is_md5(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `X-Notedown-Md5`, if sent; a value that is no MD5 is a 400.
fn md5_header(headers: &HeaderMap) -> Result<Option<String>, Refusal> {
    match headers.get(MD5_HEADER) {
        None => Ok(None),
        Some(v) => match v.to_str().map(str::trim) {
            Ok(md5) if is_md5(md5) => Ok(Some(md5.to_ascii_lowercase())),
            _ => Err(Box::new(bad_request("X-Notedown-Md5 is not an MD5 in hex"))),
        },
    }
}

/// The length of the body by `Content-Length`; `None` without one (a chunked body).
fn content_length(headers: &HeaderMap) -> Result<Option<u64>, Refusal> {
    match headers.get(header::CONTENT_LENGTH) {
        None => Ok(None),
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .map(Some)
            .ok_or_else(|| Box::new(bad_request("Content-Length is not a number"))),
    }
}

/// The size of the file at `path` now (0 when there is none), read past the store.
fn current_size(store: &Store, path: &WirePath) -> u64 {
    std::fs::symlink_metadata(store.fs_path(path))
        .ok()
        .filter(|m| m.is_file())
        .map_or(0, |m| m.len())
}

/// Whether writing `size` bytes over a file of `current` bytes takes the account past
/// `quota` when it uses `used` bytes: only what the upload adds counts, so a replace by fewer
/// bytes always passes.
fn over_quota(quota: u64, used: u64, current: u64, size: u64) -> bool {
    size > current && used.saturating_sub(current).saturating_add(size) > quota
}

/// Whether `used` (the cached usage) is close enough to `quota` after the upload that the
/// check under the lock walks the tree for the exact usage: within a tenth of the quota.
fn near_quota(quota: u64, used: u64, current: u64, size: u64) -> bool {
    over_quota(quota - quota / 10, used, current, size)
}

/// The look before the body: the gates and the device sync lock, read without the store lock.
/// Returns the account folder.
fn look(state: &AppState, account: &Account) -> crate::Result<Result<PathBuf, Stop>> {
    if state.config.protect && account.imported.is_none() {
        return Ok(Err(Stop::NotImported));
    }
    let store = state.store(&account.slug)?;
    if !matches!(store.scan_hold(), Ok(None)) {
        return Ok(Err(Stop::ScanHold));
    }
    if state
        .sync_locks
        .holder(&account.slug, state.now_ms())
        .is_some()
    {
        return Ok(Err(Stop::DeviceSyncing));
    }
    Ok(Ok(store.account_dir().to_path_buf()))
}

/// The quota before the body: only for an account with `quota_bytes`, and only from the
/// cached usage. A cold cache is no refusal here: the check under the lock decides.
fn quota_look(
    state: &AppState,
    account: &Account,
    path: &WirePath,
    size: u64,
) -> crate::Result<bool> {
    let (Some(quota), Some(used)) = (account.quota_bytes, state.cached_usage(&account.slug)) else {
        return Ok(false);
    };
    let store = state.store(&account.slug)?;
    Ok(over_quota(quota, used, current_size(&store, path), size))
}

/// The checks every write makes first under the store lock: the gates (423 `not_imported`,
/// `scan_hold` as errors), the device sync lock, the sync mode.
fn gates(state: &AppState, store: &Store) -> crate::Result<Result<(), Stop>> {
    gate_locked(state, store)?;
    if state
        .sync_locks
        .holder(store.slug(), state.now_ms())
        .is_some()
    {
        return Ok(Err(Stop::DeviceSyncing));
    }
    if state.sync_mode() == SyncMode::Refuse {
        return Ok(Err(Stop::SyncRefused));
    }
    Ok(Ok(()))
}

/// The upload's commit under the store lock (module docs).
#[allow(clippy::too_many_arguments)]
fn commit(
    state: &AppState,
    account: &Account,
    path: &WirePath,
    staged: &std::path::Path,
    md5: &str,
    size: u64,
    precondition: Precondition,
) -> crate::Result<Result<PutResult, Stop>> {
    let slug = &account.slug;
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    if let Err(stop) = gates(state, &store)? {
        return Ok(Err(stop));
    }
    scan_way(state, &store, &guard, path)?;
    if let Err(r) = check_target(&store, &guard, path, Want::File)? {
        return Ok(Err(Stop::Target(r)));
    }
    let current = current_size(&store, path);
    // Only an account with `quota_bytes` has a quota (the device uploads check none). The
    // tree is walked only when the cached usage is cold or near the quota.
    if let Some(quota) = account.quota_bytes
        && size > current
    {
        let cached = state
            .cached_usage(slug)
            .filter(|&used| !near_quota(quota, used, current, size));
        if cached.is_none() {
            let used = store.disk_bytes(&guard)?;
            state.remember_usage(slug, used);
            if over_quota(quota, used, current, size) {
                return Ok(Err(Stop::Quota));
            }
        }
    }
    let done = commit_staged(
        &store,
        &guard,
        &state.journal,
        path,
        staged,
        md5,
        Origin::App,
        state.now_ms(),
        precondition,
        // The way was scanned above, before the target check.
        &|_| Ok(()),
    );
    // The cached usage stays warm: it learns what this upload changed, under the lock.
    match &done {
        Ok(r) if r.outcome == PutOutcome::Created => state.adjust_usage(slug, r.size, 0),
        Ok(r) if r.outcome == PutOutcome::Replaced => state.adjust_usage(slug, r.size, current),
        _ => {}
    }
    drop(guard);
    done.map(Ok)
}

/// Runs blocking work; a store error is [`write_error`], a panic a 500.
async fn run<T: Send + 'static>(
    work: impl FnOnce() -> crate::Result<Result<T, Stop>> + Send + 'static,
) -> Result<T, Refusal> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(refused))) => Err(Box::new(refused.response())),
        Ok(Err(e)) => Err(Box::new(write_error(e))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// `PUT /files?path=`: see the module docs. 201 for a new file, 200 for a replace or identical
/// bytes: `{"id", "path", "outcome", "md5", "size", "mtimeMs", "seqs"}`.
pub(super) async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    body: Body,
) -> Response {
    match receive(state, headers, uri, body).await {
        Ok(r) => r,
        Err(r) => *r,
    }
}

async fn receive(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    body: Body,
) -> Result<Response, Refusal> {
    let account = writer_of(&state, &headers).await?;
    let path = match query::<UploadQuery>(&uri)? {
        UploadQuery { path: Some(p) } => p,
        UploadQuery { path: None } => return Err(Box::new(bad_request("path is missing"))),
    };
    let path = parse_target(&path).map_err(|r| Box::new(r.response()))?;
    if state.sync_mode() == SyncMode::Refuse {
        return Err(Box::new(Stop::SyncRefused.response()));
    }
    let precondition = precondition_of(&headers)?;
    let want_md5 = md5_header(&headers)?;

    let (st, a) = (state.clone(), account.clone());
    let account_dir = run(move || look(&st, &a)).await?;
    let Some(len) = content_length(&headers)? else {
        return Err(Box::new(error(
            StatusCode::LENGTH_REQUIRED,
            "length_required",
        )));
    };
    if len > state.file_max() {
        return Err(Box::new(error(StatusCode::PAYLOAD_TOO_LARGE, "too_large")));
    }
    let (st, a, p) = (state.clone(), account.clone(), path.clone());
    let over = run(move || quota_look(&st, &a, &p, len).map(Ok)).await?;
    if over {
        return Err(Box::new(Stop::Quota.response()));
    }

    // From here on the staging folder is removed whatever happens, also when this future is
    // dropped. It and its temp file are made right here, not in a blocking task, so a drop
    // can never come before them and leave them behind (two small file system calls).
    let staging = Staging(
        account_dir
            .join(STAGING_DIR)
            .join(format!("app-{}", crate::journal::new_op_id())),
    );
    let mut file = std::fs::create_dir_all(&staging.0)
        .map_err(crate::io_err(&staging.0))
        .and_then(|()| StreamFile::create(&staging.0))
        .map_err(|e| Box::new(internal(e)))?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            file.abort();
            return Err(Box::new(error(StatusCode::BAD_REQUEST, "bad_length")));
        };
        if file.len() + chunk.len() as u64 > len {
            // hyper never hands over more than Content-Length; this is only a guard.
            file.abort();
            return Err(Box::new(error(StatusCode::BAD_REQUEST, "bad_length")));
        }
        if let Err(e) = file.push(&chunk).await {
            file.abort();
            return Err(Box::new(internal(e)));
        }
    }
    if file.len() != len {
        file.abort();
        return Err(Box::new(error(StatusCode::BAD_REQUEST, "bad_length")));
    }
    let (staged, size, md5) = file.close().await.map_err(|e| Box::new(internal(e)))?;
    if want_md5.is_some_and(|want| want != md5) {
        return Err(Box::new(error(StatusCode::BAD_REQUEST, "bad_md5")));
    }

    let st = state.clone();
    let done = run(move || {
        // Removed when the commit is done, in this blocking task.
        let _staging = staging;
        commit(&st, &account, &path, &staged, &md5, size, precondition)
    })
    .await?;
    let status = if done.outcome == PutOutcome::Created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let outcome = match done.outcome {
        PutOutcome::Created => "created",
        PutOutcome::Replaced => "replaced",
        PutOutcome::Unchanged => "unchanged",
    };
    Ok((
        status,
        Json(json!({
            "id": done.id,
            "path": done.path.display(),
            "outcome": outcome,
            "md5": done.md5,
            "size": done.size,
            "mtimeMs": done.mtime_ms,
            "seqs": done.seqs,
        })),
    )
        .into_response())
}

// ------------------------------------------------------- folders, move, delete

/// The JSON body of a write; anything else is a 400 that names the `shape`.
fn json_body<T: DeserializeOwned>(body: &Bytes, shape: &str) -> Result<T, Refusal> {
    serde_json::from_slice(body)
        .map_err(|_| Box::new(bad_request(&format!("the body must be {shape}"))))
}

/// What every JSON write checks before the store lock: the write token and the sync mode.
async fn writer(state: &Arc<AppState>, headers: &HeaderMap) -> Result<Account, Refusal> {
    let account = writer_of(state, headers).await?;
    if state.sync_mode() == SyncMode::Refuse {
        return Err(Box::new(Stop::SyncRefused.response()));
    }
    Ok(account)
}

/// The folders that a planned move or delete removes because it leaves them empty (deepest
/// first), as `{"id", "path"}`.
fn removed(op: &PendingOp) -> Value {
    let prune = match op {
        PendingOp::Move { prune, .. } | PendingOp::Delete { prune, .. } => prune.as_slice(),
        // A restore leaves no folder empty.
        PendingOp::Restore { .. } => &[],
        _ => &[],
    };
    json!(
        prune
            .iter()
            .map(|p| json!({"id": p.id, "path": p.path}))
            .collect::<Vec<_>>()
    )
}

/// The store, its recovered lock and the gates of a write.
fn locked(state: &AppState, slug: &str) -> crate::Result<Result<(Arc<Store>, WriteGuard), Stop>> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    Ok(gates(state, &store)?.map(|()| (store, guard)))
}

#[derive(Deserialize)]
struct FolderBody {
    path: String,
}

/// `POST /folders` under the store lock: the folder and every missing folder above it.
fn make_folder(
    state: &AppState,
    slug: &str,
    path: &WirePath,
) -> crate::Result<Result<(StatusCode, Value), Stop>> {
    let (store, guard) = match locked(state, slug)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    // A folder made by hand is journaled (origin `scan`) before it counts as there.
    scan_way(state, &store, &guard, path)?;
    if let Err(r) = check_target(&store, &guard, path, Want::Folder)? {
        return Ok(Err(Stop::Target(r)));
    }
    let seqs = create_folders(state, &store, &guard, path, Origin::App, None)?;
    let Some(entry) = store.entry(&guard, path)?.filter(|e| e.tag == Kind::Folder) else {
        return Err(crate::Error::Invalid {
            what: "path",
            message: format!("{path} is no folder after its creation"),
        });
    };
    let status = if seqs.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok(Ok((
        status,
        json!({"id": entry.id, "path": path.display(), "seqs": seqs}),
    )))
}

/// `POST /folders {"path"}`: creates the folder and the missing folders above it, one
/// `folder.created` each, parents first. 201 `{"id", "path", "seqs"}`; 200 with no seqs when the
/// folder is there. A file at the path is 409 `name_taken`, a file on the way 409
/// `not_a_folder`.
pub(super) async fn folder(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let made = async {
        let account = writer(&state, &headers).await?;
        let body: FolderBody = json_body(&body, "{\"path\"}")?;
        let path = parse_target(&body.path).map_err(|r| Box::new(r.response()))?;
        let st = state.clone();
        run(move || make_folder(&st, &account.slug, &path)).await
    };
    match made.await {
        Ok((status, answer)) => (status, Json(answer)).into_response(),
        Err(r) => *r,
    }
}

#[derive(Deserialize)]
struct MoveBody {
    id: u64,
    to: String,
}

/// `POST /move` under the store lock.
fn move_entry(
    state: &AppState,
    slug: &str,
    id: u64,
    to: &WirePath,
) -> crate::Result<Result<Value, Stop>> {
    let (store, guard) = match locked(state, slug)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let Some(src) = source_of(state, &store, &guard, id)? else {
        return Ok(Err(Stop::NotFound));
    };
    if let Err(r) = check_source(&src.path) {
        return Ok(Err(Stop::Target(r)));
    }
    if *to == src.path {
        return Ok(Ok(
            json!({"id": id, "path": to.display(), "seqs": [], "removed": []}),
        ));
    }
    scan_way(state, &store, &guard, to)?;
    if let Err(r) = check_move(&store, &guard, &src.path, src.tag == Kind::Folder, to)? {
        return Ok(Err(Stop::Target(r)));
    }
    let mut c = store.plan_move_at(&guard, &src.path, to, state.now_ms(), Origin::App, None)?;
    crate::pages::plan_page_move(&store, &guard, &mut c);
    let removed = removed(&c.op);
    let seqs = store.commit(&guard, c, &state.journal)?;
    // A move changes no usage.
    Ok(Ok(
        json!({"id": id, "path": to.display(), "seqs": seqs, "removed": removed}),
    ))
}

/// `POST /move {"id", "to"}`: renames or moves the file or folder `id` to the wire path `to`;
/// the id stays. Missing folders on the way are created. 200 `{"id", "path", "seqs",
/// "removed"}`; the same path is 200 with no seqs. A case-only rename is allowed; any other
/// entry at `to`, also in another case, is 409 `name_taken`; a folder into itself 400
/// `into_itself`; an unknown id 404 `not_found`.
pub(super) async fn move_(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let moved = async {
        let account = writer(&state, &headers).await?;
        let body: MoveBody = json_body(&body, "{\"id\": <number>, \"to\"}")?;
        let to = parse_target(&body.to).map_err(|r| Box::new(r.response()))?;
        let st = state.clone();
        run(move || move_entry(&st, &account.slug, body.id, &to)).await
    };
    match moved.await {
        Ok(answer) => Json(answer).into_response(),
        Err(r) => *r,
    }
}

/// `DELETE /entries/{id}` under the store lock.
fn delete_entry(state: &AppState, slug: &str, id: u64) -> crate::Result<Result<Value, Stop>> {
    let (store, guard) = match locked(state, slug)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let Some(src) = source_of(state, &store, &guard, id)? else {
        return Ok(Err(Stop::NotFound));
    };
    if let Err(r) = check_source(&src.path) {
        return Ok(Err(Stop::Target(r)));
    }
    let mut c = store.plan_delete(&guard, &src.path, state.now_ms(), Origin::App, None)?;
    crate::pages::plan_page_delete(&store, &guard, &mut c);
    let removed = removed(&c.op);
    let seqs = store.commit(&guard, c, &state.journal)?;
    // The bytes left `files/` for the recycle bin; under the lock, so the cache stays right.
    match src.tag {
        Kind::File => state.adjust_usage(slug, 0, src.size),
        Kind::Folder => state.invalidate_usage(slug),
    }
    Ok(Ok(
        json!({"id": id, "path": src.path.display(), "seqs": seqs, "removed": removed}),
    ))
}

/// `DELETE /entries/{id}`: moves the file, or the folder with everything in it, to the recycle
/// bin (`file.deleted` with `recycle`, `folder.deleted`, children first). 200 `{"id", "path",
/// "seqs", "removed"}`; an unknown id (also a retried delete) 404 `not_found`.
pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let deleted = async {
        let account = writer(&state, &headers).await?;
        let id = id_arg(&id)?;
        let st = state.clone();
        run(move || delete_entry(&st, &account.slug, id)).await
    };
    match deleted.await {
        Ok(answer) => Json(answer).into_response(),
        Err(r) => *r,
    }
}
