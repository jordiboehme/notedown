//! Byte transfers under `/api/oss/` (`docs/sync-protocol.md` 3.5). No token: the signed URL is
//! the gate (see [`crate::sync::urls`]). A bad or expired signature, or one issued before the
//! device's token generation changed, answers HTTP 500 with the plain text `E1306`; a file that
//! cannot be served answers HTTP 500 with `E1307`, as Ratta's server does.
//!
//! Uploads (`/api/oss/upload`, `/api/oss/upload/part`) also need the sync lock for the
//! signed device, taken as for every write (another device's live lock: `E0079`, flag
//! `no_sync_lock`). Their multipart field `file` streams into the
//! staging entry ([`crate::sync::staging`]) and is never held in memory as a whole.
//!
//! The handwritten comments of digests (`docs/sync-protocol.md` 5.5) move on their own routes,
//! `/api/oss/upload/summary` and `/api/oss/download/summary`, with targets whose `path` is
//! [`MARK_TARGET`] (no file route takes such a target, and these routes take no other). They
//! pass the app data gates of the signed account ([`super::digests`]: `E0079`, flags
//! `protect_refused`, `app_data_refused`) and never take, revive or extend the sync lock.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::digests::{AppRequest, app_data_open, app_data_refused, on_app_data};
use super::envelope::{LogDevice, LogFlags, Reply, Streamed};
use super::files::{
    E_UPLOAD, FLAG_UPLOAD_TOO_LARGE, MSG_UPLOAD, holds_sync_lock, keep_lock, no_sync_lock,
};
use crate::appdata::{AppData, Digest, MAX_MARK_BYTES, valid_mark_name};
use crate::server::AppState;
use crate::store::WirePath;
use crate::sync::staging::{
    self, MAX_CHUNKS, PartOutcome, STATUS_MERGED, STATUS_UPLOADING, SingleOutcome, StreamFile,
    UploadMeta,
};
use crate::sync::urls::{E_DOWNLOAD, E_SIGNATURE, E_UPLOAD_FAILED, content_disposition};
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

/// Bytes of all multipart fields other than `file` together that are read (and dropped) at
/// most; more answers `E0324` (`upload_broken`).
pub const OTHER_FIELDS_MAX: usize = 256 * 1024;

/// Bytes read per step while a download streams.
const CHUNK: usize = 256 * 1024;

/// The `path` of a signed target that names a handwritten comment of a digest (`inner`: the
/// `.mark` name for uploads, `id`: the digest for downloads) instead of a file. A file target
/// carries a [`WirePath::display`], which never holds a backslash ([`WirePath::parse`] refuses
/// one), so no file or folder can share this value.
pub const MARK_TARGET: &str = "\\digest-mark";

/// Log flag: a mark upload whose account or store failed on the server side.
pub const FLAG_UPLOAD_FAILED: &str = "upload_failed";

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

/// The sync lock a download extends while it streams: the account and the device.
struct Touch {
    state: Arc<AppState>,
    slug: String,
    serial: String,
}

pub(crate) struct Flow {
    file: Option<std::fs::File>,
    seek_to: Option<u64>,
    remaining: u64,
    touch: Option<Touch>,
}

impl Flow {
    /// The first `size` bytes of `file` from its start, with no sync lock to extend.
    pub(crate) fn whole(file: std::fs::File, size: u64) -> Flow {
        Flow {
            file: Some(file),
            seek_to: None,
            remaining: size,
            touch: None,
        }
    }
}

/// The file's bytes as a body: read in blocking steps of [`CHUNK`], never the whole file in
/// memory. Each step extends the device's sync lock (when `touch` is set). A file that shrinks
/// while it streams ends the body with an error (the client sees a cut transfer, never wrong
/// bytes as complete).
pub(crate) fn stream_body(flow: Flow) -> Body {
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
                if let Some(t) = &flow.touch {
                    t.state
                        .sync_locks
                        .touch(&t.slug, &t.serial, t.state.now_ms());
                }
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

/// The checked target of a signed URL: the signature of `method` and `purpose` (the signed
/// size is the target's for uploads, 0 for downloads) and the kind of target, a handwritten
/// comment ([`MARK_TARGET`]) when `mark`, else a file. Anything else answers `E1306`.
fn verified(
    state: &AppState,
    q: &HashMap<String, String>,
    method: &str,
    purpose: Purpose,
    mark: bool,
) -> Result<SignedTarget, Box<Response>> {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let path_b64 = get("path");
    let bad = || Box::new(plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, None));
    let target = SignedTarget::decode(path_b64).ok_or_else(bad)?;
    let size = match purpose {
        Purpose::Upload => target.size,
        Purpose::Download => 0,
    };
    match state.url_signer.verify(
        method,
        path_b64,
        get("timestamp"),
        get("nonce"),
        get("signature"),
        purpose,
        size,
        state.now_ms(),
    ) {
        Ok(()) => {}
        Err(UrlError::Bad) => return Err(bad()),
        Err(UrlError::Expired) => {
            return Err(Box::new(plain_error(
                E_SIGNATURE,
                FLAG_EXPIRED_SIGNATURE,
                None,
            )));
        }
    }
    if (target.path == MARK_TARGET) != mark {
        return Err(Box::new(plain_error(
            E_SIGNATURE,
            FLAG_BAD_SIGNATURE,
            Some(&target.device),
        )));
    }
    Ok(target)
}

/// The response that sends `file` (`size` bytes, downloaded as `name`): one `Range` supported
/// (206, `Content-Range`), 416 for a start past the end.
fn send_file(
    headers: &HeaderMap,
    file: std::fs::File,
    size: u64,
    name: &str,
    serial: String,
    touch: Option<Touch>,
) -> Response {
    let range = parse_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        size,
    );
    let mut r = send_range(file, size, range, touch);
    if r.status() != StatusCode::RANGE_NOT_SATISFIABLE
        && let Ok(v) = HeaderValue::from_str(&content_disposition(name))
    {
        r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    r.extensions_mut().insert(LogDevice(serial));
    r
}

/// The response that sends `file` (`size` bytes) whose strong ETag is `etag` (`"<md5>"`), with
/// that `ETag`: one `Range` supported (206, `Content-Range`), 416 for a start past the end. An
/// `If-Range` that is not exactly `etag` (strong comparison: a weak tag or a date never
/// matches) sends the whole file (200).
pub(crate) fn send_tagged(
    headers: &HeaderMap,
    file: std::fs::File,
    size: u64,
    etag: &str,
) -> Response {
    let range = match headers.get(header::IF_RANGE) {
        Some(v) if v.to_str().ok().map(str::trim) != Some(etag) => ByteRange::Full,
        _ => parse_range(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            size,
        ),
    };
    let mut r = send_range(file, size, range, None);
    if let Ok(v) = HeaderValue::from_str(etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

/// The response that sends `range` of `file` (`size` bytes): 200 for the whole file, 206 with
/// `Content-Range` for a part, 416 with `bytes */<size>` (the file is dropped). The body streams
/// ([`stream_body`]), extending the sync lock of `touch`.
fn send_range(file: std::fs::File, size: u64, range: ByteRange, touch: Option<Touch>) -> Response {
    let (status, start, len) = match range {
        ByteRange::Full => (StatusCode::OK, 0, size),
        ByteRange::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        ByteRange::Unsatisfiable => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [
                    (header::CONTENT_RANGE, format!("bytes */{size}")),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                ],
            )
                .into_response();
        }
    };
    let body = stream_body(Flow {
        file: Some(file),
        seek_to: (start > 0).then_some(start),
        remaining: len,
        touch,
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
    if status == StatusCode::PARTIAL_CONTENT {
        let end = start + len - 1;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    r.extensions_mut().insert(Streamed);
    r
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
    let target = match verified(&state, &q, "GET", Purpose::Download, false) {
        Ok(t) => t,
        Err(r) => return *r,
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
    let touch = Touch {
        state: state.clone(),
        slug: target.account,
        serial: serial.clone(),
    };
    send_file(&headers, file, size, &name, serial, Some(touch))
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
    let target = verified(state, q, "POST", Purpose::Upload, false)?;
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
    // The token generation, then the protect gate (as on every file route).
    let checked = tokio::task::spawn_blocking(move || -> crate::Result<Option<bool>> {
        if !generation_current(&st, &t)? {
            return Ok(None);
        }
        let Some(account) = st.auth.accounts().get(&t.account)? else {
            return Err(crate::Error::NoAccount(t.account.clone()));
        };
        Ok(Some(
            super::files::protect_gate(&st, &account, &t.device).is_none(),
        ))
    })
    .await;
    match checked {
        Ok(Ok(Some(true))) => {}
        Ok(Ok(Some(false))) => {
            return Err(Box::new(
                super::files::protect_refused(&serial).into_response(),
            ));
        }
        Ok(Ok(None)) => {
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
    Ok(target)
}

/// Why receiving the `file` field failed.
enum Recv {
    TooLarge,
    Broken,
    Failed(crate::Error),
}

/// Stream the multipart field `file` into a temp file in `dir`, at most `cap` bytes. Other
/// fields are read and dropped (256 KiB at most, all together). Each step extends the sync lock
/// of `lock` (none for the app data). On any error the temp file is removed.
async fn receive_file(
    state: &AppState,
    lock: Option<&SignedTarget>,
    multipart: &mut Multipart,
    dir: &std::path::Path,
    cap: u64,
) -> Result<StreamFile, Recv> {
    let mut file: Option<StreamFile> = None;
    // Bytes of the other fields, all together.
    let mut seen = 0usize;
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
                        if let Some(t) = lock {
                            state
                                .sync_locks
                                .touch(&t.account, &t.device, state.now_ms());
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return fail(Some(f), Recv::Broken),
                }
            }
            file = Some(f);
        } else {
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        seen += chunk.len();
                        if seen > OTHER_FIELDS_MAX {
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

/// The staging entry of an upload target. `upload/apply` creates it; an entry that is gone
/// (removed by `upload/finish`, or never applied for) is `None`, and the upload is refused.
async fn open_entry(
    state: &AppState,
    target: &SignedTarget,
) -> crate::Result<Option<std::path::PathBuf>> {
    let account_dir = crate::auth::Accounts::new(&state.data_dir).account_dir(&target.account);
    let entry = staging::entry_dir(&account_dir, target.inner.as_deref().unwrap_or(""))?;
    let e = entry.clone();
    let gone = tokio::task::spawn_blocking(move || staging::entry_gone(&e))
        .await
        .map_err(|e| crate::Error::Invalid {
            what: "upload",
            message: e.to_string(),
        })?;
    Ok((!gone).then_some(entry))
}

/// [`recv_refused`], but a failure in an entry that is gone meanwhile (an `upload/finish`
/// removed it while the bytes arrived) is the refusal of an upload to a used URL.
fn failed_in(entry: &std::path::Path, why: Recv, serial: &str) -> Response {
    if matches!(why, Recv::Failed(_)) && staging::entry_gone(entry) {
        return upload_refused(FLAG_PART_MISMATCH, serial);
    }
    recv_refused(why, serial)
}

/// `POST /api/oss/upload?path&signature&timestamp&nonce` (multipart field `file`): the bytes
/// of a whole file into `staging/<innerName>/data`, at most 1 GiB (`E0324`). An upload URL
/// takes one set of bytes: a retry with the same bytes (a lost answer) is answered again, but
/// other bytes (also during and after `upload/finish`) or bytes after parts are refused
/// (`E0324`, `part_mismatch`), so the bytes a finish verifies cannot change. Answers
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
        Ok(Some(e)) => e,
        Ok(None) => return upload_refused(FLAG_PART_MISMATCH, &serial),
        Err(e) => return recv_refused(Recv::Failed(e), &serial),
    };
    let file = match receive_file(
        &state,
        Some(&target),
        &mut multipart,
        &entry,
        state.file_max(),
    )
    .await
    {
        Ok(f) => f,
        Err(why) => return failed_in(&entry, why, &serial),
    };
    let (tmp, _, md5) = match file.close().await {
        Ok(c) => c,
        Err(e) => return failed_in(&entry, Recv::Failed(e), &serial),
    };
    let (st, e, parent, m) = (
        state.clone(),
        entry.clone(),
        target.path.clone(),
        md5.clone(),
    );
    let t = tmp.clone();
    let stored = keep_lock(
        &state,
        &target.account,
        &serial,
        tokio::task::spawn_blocking(move || st.staging_locks.store_single(&e, &t, &parent, &m)),
    )
    .await;
    match stored {
        Ok(Ok(SingleOutcome::Stored | SingleOutcome::Same)) => Reply::ok(serde_json::json!({
            "innerName": target.inner,
            "md5": md5,
        }))
        .device(&serial)
        .into_response(),
        Ok(Ok(SingleOutcome::Refused)) => upload_refused(FLAG_PART_MISMATCH, &serial),
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&tmp);
            failed_in(&entry, Recv::Failed(e), &serial)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("notedown cloud: upload failed: {e}");
            Reply::internal().device(&serial).into_response()
        }
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
        Ok(Some(e)) => e,
        Ok(None) => return upload_refused(FLAG_PART_MISMATCH, &serial),
        Err(e) => return recv_refused(Recv::Failed(e), &serial),
    };
    let file = match receive_file(
        &state,
        Some(&target),
        &mut multipart,
        &entry,
        state.part_max(),
    )
    .await
    {
        Ok(f) => f,
        Err(why) => return failed_in(&entry, why, &serial),
    };
    let (tmp, _, chunk_md5) = match file.close().await {
        Ok(c) => c,
        Err(e) => return failed_in(&entry, Recv::Failed(e), &serial),
    };
    let meta = UploadMeta {
        parent: target.path.clone(),
        upload_id: Some(upload_id.to_string()),
        total_chunks: Some(total),
        md5: None,
    };
    let (st, e, file_max) = (state.clone(), entry.clone(), state.file_max());
    let t = tmp.clone();
    let stored = keep_lock(
        &state,
        &target.account,
        &serial,
        tokio::task::spawn_blocking(move || {
            st.staging_locks.store_part(&e, &t, part, &meta, file_max)
        }),
    )
    .await;
    let status = match stored {
        Ok(Ok(PartOutcome::Uploading)) => STATUS_UPLOADING,
        Ok(Ok(PartOutcome::Merged)) => STATUS_MERGED,
        Ok(Ok(PartOutcome::Mismatch)) => return upload_refused(FLAG_PART_MISMATCH, &serial),
        Ok(Ok(PartOutcome::TooLarge)) => return upload_refused(FLAG_UPLOAD_TOO_LARGE, &serial),
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&tmp);
            return failed_in(&entry, Recv::Failed(e), &serial);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
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

// ------------------------------------------------------------------ handwritten comments

/// What the gates of a mark URL found.
enum MarkGate {
    /// Open; the account's user id.
    Open(u64),
    /// The device's token generation changed since signing.
    Stale,
    /// A closed gate's answer.
    Refused(Reply),
}

/// The gates for the account a mark URL was signed for: the token generation (`E1306`, flag
/// `stale_token`), the protect gate and the app data gate (`E0079`). A missing account or a
/// store error answers the plain-text `failed` (`code`, `flag`) of the route. The request that
/// passes carries no body.
async fn mark_request(
    state: &Arc<AppState>,
    target: &SignedTarget,
    failed: (&str, &str),
) -> Result<AppRequest, Box<Response>> {
    let serial = target.device.clone();
    let (st, t) = (state.clone(), target.clone());
    let checked = tokio::task::spawn_blocking(move || -> crate::Result<MarkGate> {
        if !generation_current(&st, &t)? {
            return Ok(MarkGate::Stale);
        }
        let Some(account) = st.auth.accounts().get(&t.account)? else {
            return Err(crate::Error::NoAccount(t.account.clone()));
        };
        if let Some(refused) = super::files::protect_gate(&st, &account, &t.device) {
            return Ok(MarkGate::Refused(refused));
        }
        if !app_data_open(&st, &account) {
            return Ok(MarkGate::Refused(app_data_refused(&t.device)));
        }
        Ok(MarkGate::Open(account.user_id))
    })
    .await;
    match checked {
        Ok(Ok(MarkGate::Open(user_id))) => Ok(AppRequest {
            slug: target.account.clone(),
            serial,
            user_id,
            generation: target.generation,
            body: serde_json::Value::Null,
        }),
        Ok(Ok(MarkGate::Stale)) => Err(Box::new(plain_error(
            E_SIGNATURE,
            FLAG_STALE_TOKEN,
            Some(&serial),
        ))),
        Ok(Ok(MarkGate::Refused(reply))) => Err(Box::new(reply.into_response())),
        Ok(Err(e)) => {
            eprintln!("notedown cloud: {e}");
            Err(Box::new(plain_error(failed.0, failed.1, Some(&serial))))
        }
        Err(e) => {
            eprintln!("notedown cloud: {e}");
            Err(Box::new(plain_error(failed.0, failed.1, Some(&serial))))
        }
    }
}

/// `POST /api/oss/upload/summary?path&signature&timestamp&nonce` (multipart field `file`): the
/// handwritten comment signed in `upload/apply/summary`, at most 32 MiB (`E0324`). The bytes
/// stream into a temp file in `digests/marks/` without the store lock; then, under the store
/// lock and with the gates read again, a rename makes it `<name>.mark` (an existing comment of
/// that name is replaced: a changed comment keeps its name). Answers `{innerName, md5}`.
pub async fn upload_summary(
    State(state): State<Arc<AppState>>,
    query: QueryArg,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    let target = match verified(&state, &q, "POST", Purpose::Upload, true) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let serial = target.device.clone();
    let Some(name) = target.inner.clone().filter(|n| valid_mark_name(n)) else {
        return plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, Some(&serial));
    };
    let req = match mark_request(&state, &target, (E_UPLOAD_FAILED, FLAG_UPLOAD_FAILED)).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let Ok(mut multipart) = multipart else {
        return upload_refused(FLAG_UPLOAD_BROKEN, &serial);
    };
    let (st, slug) = (state.clone(), req.slug.clone());
    let dir = tokio::task::spawn_blocking(move || -> crate::Result<std::path::PathBuf> {
        let store = st.store(&slug)?;
        let dir = AppData::new(&store).marks_dir();
        // Only below the account folder: a removed account is never brought back.
        crate::fsutil::create_dirs_below(store.account_dir(), &dir).map_err(crate::io_err(&dir))?;
        Ok(dir)
    })
    .await;
    let dir = match dir {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return recv_refused(Recv::Failed(e), &serial),
        Err(e) => {
            eprintln!("notedown cloud: upload failed: {e}");
            return Reply::internal().device(&serial).into_response();
        }
    };
    let file = match receive_file(&state, None, &mut multipart, &dir, MAX_MARK_BYTES).await {
        Ok(f) => f,
        Err(why) => return recv_refused(why, &serial),
    };
    let (tmp, _, md5) = match file.close().await {
        Ok(c) => c,
        Err(e) => return recv_refused(Recv::Failed(e), &serial),
    };
    let t = tmp.clone();
    let reply = on_app_data(&state, &req, move |c| {
        c.app.put_mark_file(c.guard, &name, &t)?;
        Ok(Ok(Reply::ok(serde_json::json!({
            "innerName": name,
            "md5": md5,
        }))
        .device(c.serial)))
    })
    .await
    .unwrap_or_else(|refused| refused);
    // Renamed on success; else the temp file goes.
    let _ = std::fs::remove_file(&tmp);
    reply.into_response()
}

/// `GET /api/oss/download/summary?path&signature&timestamp&nonce`: the handwritten comment of
/// the signed digest (the `.mark` its `commentHandwriteName` names), with one `Range`
/// supported. A digest that is gone, names no valid comment or whose file is missing answers
/// HTTP 500 `E1307`.
pub async fn download_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: QueryArg,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    let target = match verified(&state, &q, "GET", Purpose::Download, true) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let serial = target.device.clone();
    let Some(id) = target.id else {
        return plain_error(E_SIGNATURE, FLAG_BAD_SIGNATURE, Some(&serial));
    };
    let req = match mark_request(&state, &target, (E_DOWNLOAD, FLAG_DOWNLOAD_FAILED)).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let opened = on_app_data(&state, &req, move |c| {
        let Some(digest) = c.app.digest(c.guard, id)?.filter(Digest::is_live) else {
            return Ok(Ok(None));
        };
        let Some(name) = digest.comment_handwrite_name.filter(|n| valid_mark_name(n)) else {
            return Ok(Ok(None));
        };
        Ok(Ok(c
            .app
            .open_mark(c.guard, &name)?
            .map(|(file, size)| (file, size, name))))
    })
    .await;
    match opened {
        Ok(Some((file, size, name))) => send_file(&headers, file, size, &name, serial, None),
        Ok(None) => plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial)),
        Err(refused) => refused.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_target_is_no_wire_path() {
        assert!(WirePath::parse(MARK_TARGET).is_err());
        // and no display of one: a display never holds a backslash
        assert!(MARK_TARGET.contains('\\'));
        let lookalike = WirePath::parse("digest-mark").unwrap();
        assert_ne!(lookalike.display(), MARK_TARGET);
    }
}
