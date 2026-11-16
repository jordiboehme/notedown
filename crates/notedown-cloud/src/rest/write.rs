//! The app's upload: `PUT /files?path=` with the raw bytes, by the rules of `notedown cloud
//! put` ([`crate::put::commit_staged`], origin `app`, no device, so every bound device gets the
//! push). A write token only (403 `read_only_token`).
//!
//! Before the body is read (no store lock): the path ([`parse_target`]), the sync mode (409
//! `sync_refused`), a look at the gates (423 `not_imported`, `scan_hold`) and at the device sync
//! lock (423 `device_syncing` with `Retry-After`), `Content-Length` (411 `length_required`), the
//! size limit (413 `too_large`) and the quota (507 `quota_exceeded`). Then the body streams into
//! `accounts/<slug>/staging/app-<op id>/`; a body shorter than `Content-Length` is 400
//! `bad_length`, one whose MD5 is not `X-Notedown-Md5` 400 `bad_md5`. Then, in one blocking
//! task under one store lock, in this order: the gates, the device sync lock, the sync mode, the
//! scans of the folders on the way, [`check_target`], the quota again, and the commit (the
//! precondition `If-None-Match: *` or `If-Match: "<md5>"`, 412 `precondition_failed`, comes after
//! the check for identical bytes, which is always 200 `unchanged`). The staging folder goes
//! away in every case, also when the client drops the connection.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;

use super::check::{Want, WriteRefusal, check_target, parse_target};
use super::errors::{device_syncing, write_error};
use super::routes::{Refusal, bad_request, error, internal, query, writer_of};
use crate::SyncMode;
use crate::api::files::{gate_locked, scan_way};
use crate::auth::{Account, DEFAULT_QUOTA_BYTES};
use crate::journal::Origin;
use crate::put::{Precondition, PutOutcome, PutResult, commit_staged};
use crate::server::AppState;
use crate::store::{Store, WirePath};
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

/// Why a look or the commit refuses the upload.
enum Stop {
    Target(WriteRefusal),
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

/// Whether writing `size` bytes at `path` takes the account past its quota: only what the
/// upload adds counts, so a replace by fewer bytes always passes.
fn over_quota(account: &Account, used: u64, current: u64, size: u64) -> bool {
    let quota = account.quota_bytes.unwrap_or(DEFAULT_QUOTA_BYTES);
    size > current && used.saturating_sub(current).saturating_add(size) > quota
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

/// The quota before the body (the usage of the cache, which may take the store lock briefly).
fn quota_look(
    state: &AppState,
    account: &Account,
    path: &WirePath,
    size: u64,
) -> crate::Result<bool> {
    let store = state.store(&account.slug)?;
    let current = current_size(&store, path);
    if size <= current {
        return Ok(false);
    }
    Ok(over_quota(
        account,
        state.used_bytes(&account.slug)?,
        current,
        size,
    ))
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
    gate_locked(state, &store)?;
    if state.sync_locks.holder(slug, state.now_ms()).is_some() {
        return Ok(Err(Stop::DeviceSyncing));
    }
    if state.sync_mode() == SyncMode::Refuse {
        return Ok(Err(Stop::SyncRefused));
    }
    scan_way(state, &store, &guard, path)?;
    if let Err(r) = check_target(&store, &guard, path, Want::File)? {
        return Ok(Err(Stop::Target(r)));
    }
    let current = current_size(&store, path);
    if size > current && over_quota(account, store.disk_bytes(&guard)?, current, size) {
        return Ok(Err(Stop::Quota));
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
    drop(guard);
    if matches!(&done, Ok(r) if r.outcome != PutOutcome::Unchanged) {
        state.invalidate_usage(slug);
    }
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
