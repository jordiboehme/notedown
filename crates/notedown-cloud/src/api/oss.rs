//! Byte transfers under `/api/oss/` (`docs/sync-protocol.md` 3.5). No token: the signed URL is
//! the gate (see [`crate::sync::urls`]). A bad or expired signature, or one issued before the
//! device's token generation changed, answers HTTP 500 with the plain text `E1306`; a file that
//! cannot be served answers HTTP 500 with `E1307`, as Ratta's server does.
//!
//! Uploads (`/api/oss/upload`, `/api/oss/upload/part`) also need the signed device to hold the
//! live sync lock (`E0079`, flag `no_sync_lock`). Their multipart field `file` streams into the
//! staging entry ([`crate::sync::staging`]) and is never held in memory as a whole.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::envelope::{LogDevice, LogFlags, Reply, Streamed};
use super::files::{
    E_UPLOAD, FLAG_UPLOAD_TOO_LARGE, MSG_UPLOAD, holds_sync_lock, keep_lock, no_sync_lock,
};
use crate::server::AppState;
use crate::store::WirePath;
use crate::sync::staging::{
    self, MAX_CHUNKS, PartOutcome, STATUS_MERGED, STATUS_UPLOADING, StreamFile, UploadMeta,
};
use crate::sync::urls::{E_DOWNLOAD, E_SIGNATURE, content_disposition};
use crate::sync::{Purpose, SignedTarget, UrlError};

/// Log flag: a signed URL failed the check.
pub const FLAG_BAD_SIGNATURE: &str = "bad_signature";
/// Log flag: a signed URL was valid once but has expired.
pub const FLAG_EXPIRED_SIGNATURE: &str = "expired_signature";
/// Log flag: a signed download whose file is gone or unreadable.
pub const FLAG_DOWNLOAD_FAILED: &str = "download_failed";
/// Log flag: a signed URL issued before the device's token generation changed (a logout or a
/// new login since).
pub const FLAG_STALE_TOKEN: &str = "stale_token";
/// Log flag: an upload body that is no usable multipart form (or was cut).
pub const FLAG_UPLOAD_BROKEN: &str = "upload_broken";
/// Log flag: a part that does not fit its upload (bad numbers, another `uploadId`).
pub const FLAG_PART_MISMATCH: &str = "part_mismatch";

/// Bytes of multipart fields other than `file` that are read (and dropped) at most.
const OTHER_FIELD_MAX: usize = 64 * 1024;

/// Bytes read per step while a download streams.
const CHUNK: usize = 256 * 1024;

/// HTTP 500 with a plain-text error code (the OSS endpoints answer no envelope).
pub fn plain_error(code: &str, flag: &str, device: Option<&str>) -> Response {
    let mut r = (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        code.to_string(),
    )
        .into_response();
    r.extensions_mut().insert(LogFlags(vec![flag.to_string()]));
    if let Some(d) = device {
        r.extensions_mut().insert(LogDevice(d.to_string()));
    }
    r
}

/// The byte range to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// The whole file (no `Range`, a malformed or multi-range header, or an empty file).
    Full,
    /// `start..=end`, both inside the file.
    Partial { start: u64, end: u64 },
    /// A start past the end: 416.
    Unsatisfiable,
}

/// One `Range: bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `size` bytes. Several
/// ranges get the full content (200). A malformed header is ignored (RFC 9110). An empty file is
/// always served whole, so the `bytes=0-` of the Partner app works on it.
pub fn parse_range(header: Option<&str>, size: u64) -> ByteRange {
    let Some(spec) = header.map(str::trim).and_then(|h| h.strip_prefix("bytes=")) else {
        return ByteRange::Full;
    };
    if spec.contains(',') || size == 0 {
        return ByteRange::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return ByteRange::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| -> Option<u64> {
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    if a.is_empty() {
        return match num(b) {
            Some(0) => ByteRange::Unsatisfiable,
            Some(n) => ByteRange::Partial {
                start: size.saturating_sub(n),
                end: size - 1,
            },
            None => ByteRange::Full,
        };
    }
    let Some(start) = num(a) else {
        return ByteRange::Full;
    };
    let end = if b.is_empty() {
        size - 1
    } else {
        match num(b) {
            Some(e) if e >= start => e.min(size - 1),
            _ => return ByteRange::Full,
        }
    };
    if start >= size {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Partial { start, end }
}

/// What a signed download found.
enum Opened {
    File(std::fs::File, u64, String),
    /// The device's token generation changed since signing.
    Stale,
    /// The id no longer names that path, or no file is there.
    Gone,
}

/// Whether the token generation signed into `target` is still the device's (blocking).
fn generation_current(state: &AppState, target: &SignedTarget) -> crate::Result<bool> {
    Ok(state
        .auth
        .devices()
        .generation(&target.account, &target.device)?
        == Some(target.generation))
}

struct Flow {
    file: Option<std::fs::File>,
    seek_to: Option<u64>,
    remaining: u64,
    state: Arc<AppState>,
    slug: String,
    serial: String,
}

/// The file's bytes as a body: read in blocking steps of [`CHUNK`], never the whole file in
/// memory. Each step extends the device's sync lock. A file that shrinks while it streams ends
/// the body with an error (the client sees a cut transfer, never wrong bytes as complete).
fn stream_body(flow: Flow) -> Body {
    let stream = futures_util::stream::unfold(flow, |mut flow| async move {
        if flow.remaining == 0 {
            return None;
        }
        let mut file = flow.file.take()?;
        let seek_to = flow.seek_to.take();
        let want = flow.remaining.min(CHUNK as u64) as usize;
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            if let Some(pos) = seek_to {
                file.seek(SeekFrom::Start(pos))?;
            }
            let mut buf = vec![0u8; want];
            let mut filled = 0;
            while filled < want {
                match file.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            buf.truncate(filled);
            Ok((file, buf))
        })
        .await;
        match read {
            Ok(Ok((file, buf))) if !buf.is_empty() => {
                flow.remaining -= buf.len() as u64;
                flow.file = Some(file);
                flow.state
                    .sync_locks
                    .touch(&flow.slug, &flow.serial, flow.state.now_ms());
                Some((Ok(Bytes::from(buf)), flow))
            }
            Ok(Ok(_)) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the file got shorter while it was sent",
                )),
                flow,
            )),
            Ok(Err(e)) => Some((Err(e), flow)),
            Err(e) => Some((Err(std::io::Error::other(e.to_string())), flow)),
        }
    });
    Body::from_stream(stream)
}

/// `GET /api/oss/download?path&signature&timestamp&nonce&pathId`: the file's bytes, with one
/// `Range` supported (206, `Content-Range`). The signed id must still name the signed path and
/// the signed token generation must be current. `pathId` is not signed and not used.
pub async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Result<Query<HashMap<String, String>>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let path_b64 = get("path");
    let checked = state.url_signer.verify(
        "GET",
        path_b64,
        get("timestamp"),
        get("nonce"),
        get("signature"),
        Purpose::Download,
        0,
        state.now_ms(),
    );
    let target = match checked.map(|()| SignedTarget::decode(path_b64)) {
        Ok(Some(t)) => t,
        Ok(None) | Err(UrlError::Bad) => return plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, None),
        Err(UrlError::Expired) => {
            return plain_error(E_SIGNATURE, FLAG_EXPIRED_SIGNATURE, None);
        }
    };
    let serial = target.device.clone();
    let Some(id) = target.id else {
        return plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, Some(&serial));
    };
    let st = state.clone();
    let t = target.clone();
    let opened = tokio::task::spawn_blocking(move || -> crate::Result<Opened> {
        if !generation_current(&st, &t)? {
            return Ok(Opened::Stale);
        }
        let path = WirePath::parse_named(&t.path)?;
        let store = st.store(&t.account)?;
        let guard = store.lock_recovered(&st.journal)?;
        if store.path_of(id)?.as_ref() != Some(&path) {
            return Ok(Opened::Gone);
        }
        let file = store.open_file(&guard, &path)?;
        drop(guard);
        Ok(match file {
            Some((file, size)) => {
                Opened::File(file, size, path.name().unwrap_or_default().to_string())
            }
            None => Opened::Gone,
        })
    })
    .await;
    let (file, size, name) = match opened {
        Ok(Ok(Opened::File(file, size, name))) => (file, size, name),
        Ok(Ok(Opened::Stale)) => return plain_error(E_SIGNATURE, FLAG_STALE_TOKEN, Some(&serial)),
        Ok(Ok(Opened::Gone)) => {
            return plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial));
        }
        Ok(Err(e)) => {
            eprintln!("notedown cloud: download failed: {e}");
            return plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial));
        }
        Err(e) => {
            eprintln!("notedown cloud: download failed: {e}");
            return plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial));
        }
    };
    state
        .sync_locks
        .touch(&target.account, &serial, state.now_ms());
    let range = parse_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        size,
    );
    let (status, start, len) = match range {
        ByteRange::Full => (StatusCode::OK, 0, size),
        ByteRange::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        ByteRange::Unsatisfiable => {
            let mut r = (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [
                    (header::CONTENT_RANGE, format!("bytes */{size}")),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                ],
            )
                .into_response();
            r.extensions_mut().insert(LogDevice(serial));
            return r;
        }
    };
    let body = stream_body(Flow {
        file: Some(file),
        seek_to: (start > 0).then_some(start),
        remaining: len,
        state: state.clone(),
        slug: target.account,
        serial: serial.clone(),
    });
    let mut r = Response::new(body);
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&content_disposition(&name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if status == StatusCode::PARTIAL_CONTENT {
        let end = start + len - 1;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    r.extensions_mut().insert(Streamed);
    r.extensions_mut().insert(LogDevice(serial));
    r
}

// ------------------------------------------------------------------ uploads

type QueryArg = Result<Query<HashMap<String, String>>, axum::extract::rejection::QueryRejection>;

/// The checked target of an upload URL: signature (`E1306`), token generation (`E1306`), an
/// `innerName` in the target, and the signed device holding the sync lock (`E0079`). Nothing of
/// the body is read before this passes.
async fn upload_target(
    state: &Arc<AppState>,
    q: &HashMap<String, String>,
) -> Result<SignedTarget, Box<Response>> {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let path_b64 = get("path");
    let Some(target) = SignedTarget::decode(path_b64) else {
        return Err(Box::new(plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, None)));
    };
    match state.url_signer.verify(
        "POST",
        path_b64,
        get("timestamp"),
        get("nonce"),
        get("signature"),
        Purpose::Upload,
        target.size,
        state.now_ms(),
    ) {
        Ok(()) => {}
        Err(UrlError::Bad) => {
            return Err(Box::new(plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, None)));
        }
        Err(UrlError::Expired) => {
            return Err(Box::new(plain_error(
                E_SIGNATURE,
                FLAG_EXPIRED_SIGNATURE,
                None,
            )));
        }
    }
    let serial = target.device.clone();
    if !target
        .inner
        .as_deref()
        .is_some_and(staging::valid_inner_name)
    {
        return Err(Box::new(plain_error(
            E_SIGNATURE,
            FLAG_BAD_SIGNATURE,
            Some(&serial),
        )));
    }
    let (st, t) = (state.clone(), target.clone());
    match tokio::task::spawn_blocking(move || generation_current(&st, &t)).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            return Err(Box::new(plain_error(
                E_SIGNATURE,
                FLAG_STALE_TOKEN,
                Some(&serial),
            )));
        }
        Ok(Err(e)) => {
            eprintln!("notedown cloud: {e}");
            return Err(Box::new(Reply::internal().device(&serial).into_response()));
        }
        Err(e) => {
            eprintln!("notedown cloud: {e}");
            return Err(Box::new(Reply::internal().device(&serial).into_response()));
        }
    }
    if !holds_sync_lock(state, &target.account, &serial) {
        return Err(Box::new(no_sync_lock(&serial).into_response()));
    }
    state
        .sync_locks
        .touch(&target.account, &serial, state.now_ms());
    Ok(target)
}

/// Why receiving the `file` field failed.
enum Recv {
    TooLarge,
    Broken,
    Failed(crate::Error),
}

/// Stream the multipart field `file` into a temp file in `dir`, at most `cap` bytes. Other
/// fields are read and dropped (64 KiB at most). Each step extends the sync lock. On any error
/// the temp file is removed.
async fn receive_file(
    state: &AppState,
    target: &SignedTarget,
    multipart: &mut Multipart,
    dir: &std::path::Path,
    cap: u64,
) -> Result<StreamFile, Recv> {
    let mut file: Option<StreamFile> = None;
    let fail = |file: Option<StreamFile>, why: Recv| {
        if let Some(f) = file {
            f.abort();
        }
        Err(why)
    };
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return fail(file, Recv::Broken),
        };
        if field.name() == Some("file") && file.is_none() {
            let d = dir.to_path_buf();
            let mut f = match tokio::task::spawn_blocking(move || StreamFile::create(&d)).await {
                Ok(Ok(f)) => f,
                Ok(Err(e)) => return Err(Recv::Failed(e)),
                Err(e) => {
                    return Err(Recv::Failed(crate::Error::Invalid {
                        what: "upload",
                        message: e.to_string(),
                    }));
                }
            };
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if f.len() + chunk.len() as u64 > cap {
                            return fail(Some(f), Recv::TooLarge);
                        }
                        if let Err(e) = f.push(&chunk).await {
                            return fail(Some(f), Recv::Failed(e));
                        }
                        state
                            .sync_locks
                            .touch(&target.account, &target.device, state.now_ms());
                    }
                    Ok(None) => break,
                    Err(_) => return fail(Some(f), Recv::Broken),
                }
            }
            file = Some(f);
        } else {
            let mut seen = 0usize;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        seen += chunk.len();
                        if seen > OTHER_FIELD_MAX {
                            return fail(file, Recv::Broken);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return fail(file, Recv::Broken),
                }
            }
        }
    }
    file.ok_or(Recv::Broken)
}

fn upload_refused(flag: &str, serial: &str) -> Response {
    Reply::error(E_UPLOAD, MSG_UPLOAD)
        .flag(flag)
        .device(serial)
        .into_response()
}

fn recv_refused(why: Recv, serial: &str) -> Response {
    match why {
        Recv::TooLarge => upload_refused(FLAG_UPLOAD_TOO_LARGE, serial),
        Recv::Broken => upload_refused(FLAG_UPLOAD_BROKEN, serial),
        Recv::Failed(e) => {
            eprintln!("notedown cloud: upload failed: {e}");
            Reply::internal().device(serial).into_response()
        }
    }
}

/// The staging entry of an upload target, created if needed (blocking).
async fn open_entry(state: &AppState, target: &SignedTarget) -> crate::Result<std::path::PathBuf> {
    let account_dir = crate::auth::Accounts::new(&state.data_dir).account_dir(&target.account);
    let entry = staging::entry_dir(&account_dir, target.inner.as_deref().unwrap_or(""))?;
    let e = entry.clone();
    tokio::task::spawn_blocking(move || staging::create_entry(&e))
        .await
        .map_err(|e| crate::Error::Invalid {
            what: "upload",
            message: e.to_string(),
        })??;
    Ok(entry)
}

/// `POST /api/oss/upload?path&signature&timestamp&nonce` (multipart field `file`): the bytes
/// of a whole file into `staging/<innerName>/data`, at most 1 GiB (`E0324`). Answers
/// `{innerName, md5}`.
pub async fn upload(
    State(state): State<Arc<AppState>>,
    query: QueryArg,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    let target = match upload_target(&state, &q).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let serial = target.device.clone();
    let Ok(mut multipart) = multipart else {
        return upload_refused(FLAG_UPLOAD_BROKEN, &serial);
    };
    let entry = match open_entry(&state, &target).await {
        Ok(e) => e,
        Err(e) => return recv_refused(Recv::Failed(e), &serial),
    };
    let file = match receive_file(&state, &target, &mut multipart, &entry, state.file_max()).await {
        Ok(f) => f,
        Err(why) => return recv_refused(why, &serial),
    };
    let finished = keep_lock(
        &state,
        &target.account,
        &serial,
        file.finish(entry.join(staging::DATA_FILE)),
    )
    .await;
    match finished {
        Ok((_, md5)) => Reply::ok(serde_json::json!({
            "innerName": target.inner,
            "md5": md5,
        }))
        .device(&serial)
        .into_response(),
        Err(e) => recv_refused(Recv::Failed(e), &serial),
    }
}

/// `POST /api/oss/upload/part?path&signature&timestamp&nonce&uploadId&partNumber&totalChunks`
/// (multipart field `file`): one part (at most 9 MiB, `E0324`) into
/// `staging/<innerName>/part-<n>`. Retries and any order are fine. When every part is there
/// they are merged into `data` (at most 1 GiB, `E0324`). Answers `{uploadId, partNumber,
/// totalChunks, chunkMd5, status}` with `status` `uploading` until the merge, then `merged`.
pub async fn upload_part(
    State(state): State<Arc<AppState>>,
    query: QueryArg,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    let target = match upload_target(&state, &q).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let serial = target.device.clone();
    let number = |k: &str| {
        q.get(k)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let upload_id = q.get("uploadId").map(String::as_str).unwrap_or("");
    let (Some(part), Some(total)) = (number("partNumber"), number("totalChunks")) else {
        return upload_refused(FLAG_PART_MISMATCH, &serial);
    };
    if !staging::valid_upload_id(upload_id)
        || !(1..=MAX_CHUNKS).contains(&total)
        || !(1..=total).contains(&part)
    {
        return upload_refused(FLAG_PART_MISMATCH, &serial);
    }
    let Ok(mut multipart) = multipart else {
        return upload_refused(FLAG_UPLOAD_BROKEN, &serial);
    };
    let entry = match open_entry(&state, &target).await {
        Ok(e) => e,
        Err(e) => return recv_refused(Recv::Failed(e), &serial),
    };
    let file = match receive_file(&state, &target, &mut multipart, &entry, state.part_max()).await {
        Ok(f) => f,
        Err(why) => return recv_refused(why, &serial),
    };
    let (tmp, _, chunk_md5) = match file.close().await {
        Ok(c) => c,
        Err(e) => return recv_refused(Recv::Failed(e), &serial),
    };
    let meta = UploadMeta {
        upload_id: upload_id.to_string(),
        total_chunks: total,
    };
    let (st, e, file_max) = (state.clone(), entry.clone(), state.file_max());
    let stored = keep_lock(
        &state,
        &target.account,
        &serial,
        tokio::task::spawn_blocking(move || {
            st.staging_locks.store_part(&e, &tmp, part, &meta, file_max)
        }),
    )
    .await;
    let status = match stored {
        Ok(Ok(PartOutcome::Uploading)) => STATUS_UPLOADING,
        Ok(Ok(PartOutcome::Merged)) => STATUS_MERGED,
        Ok(Ok(PartOutcome::Mismatch)) => return upload_refused(FLAG_PART_MISMATCH, &serial),
        Ok(Ok(PartOutcome::TooLarge)) => return upload_refused(FLAG_UPLOAD_TOO_LARGE, &serial),
        Ok(Err(e)) => return recv_refused(Recv::Failed(e), &serial),
        Err(e) => {
            eprintln!("notedown cloud: upload failed: {e}");
            return Reply::internal().device(&serial).into_response();
        }
    };
    Reply::ok(serde_json::json!({
        "uploadId": upload_id,
        "partNumber": part,
        "totalChunks": total,
        "chunkMd5": chunk_md5,
        "status": status,
    }))
    .device(&serial)
    .into_response()
}
