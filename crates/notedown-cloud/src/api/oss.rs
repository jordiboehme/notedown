//! Byte transfers under `/api/oss/` (`docs/sync-protocol.md` 3.5). No token: the signed URL is
//! the gate (see [`crate::sync::urls`]). A bad or expired signature answers HTTP 500 with the
//! plain text `E1306`; a file that cannot be served answers HTTP 500 with `E1307`, as Ratta's
//! server does.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::envelope::{LogDevice, LogFlags, Streamed};
use crate::server::AppState;
use crate::store::WirePath;
use crate::sync::urls::{E_DOWNLOAD, E_SIGNATURE, content_disposition};
use crate::sync::{Purpose, SignedTarget, UrlError};

/// Log flag: a signed URL failed the check.
pub const FLAG_BAD_SIGNATURE: &str = "bad_signature";
/// Log flag: a signed URL was valid once but has expired.
pub const FLAG_EXPIRED_SIGNATURE: &str = "expired_signature";
/// Log flag: a signed download whose file is gone or unreadable.
pub const FLAG_DOWNLOAD_FAILED: &str = "download_failed";

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
/// `Range` supported (206, `Content-Range`). `pathId` is not signed and not used.
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
    state
        .sync_locks
        .touch(&target.account, &serial, state.now_ms());
    let st = state.clone();
    let (slug, path_text) = (target.account.clone(), target.path.clone());
    let opened = tokio::task::spawn_blocking(move || -> crate::Result<_> {
        let path = WirePath::parse_named(&path_text)?;
        let store = st.store(&slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        let file = store.open_file(&guard, &path)?;
        drop(guard);
        Ok(file.map(|f| (f, path.name().unwrap_or_default().to_string())))
    })
    .await;
    let (file, size, name) = match opened {
        Ok(Ok(Some(((file, size), name)))) => (file, size, name),
        Ok(Ok(None)) => return plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial)),
        Ok(Err(e)) => {
            eprintln!("notedown cloud: download failed: {e}");
            return plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial));
        }
        Err(e) => {
            eprintln!("notedown cloud: download failed: {e}");
            return plain_error(E_DOWNLOAD, FLAG_DOWNLOAD_FAILED, Some(&serial));
        }
    };
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
