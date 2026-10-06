//! HTTP routes of the capture server and the request log.
//!
//! Every request passes [`log_request`], which writes exactly one protocol-log line for it.
//! Routes: login and binding ([`login`]), server queries ([`server`]), in accept mode the sync
//! session, listing, queries, capacity, upload apply and finish, folders, delete, move and copy
//! ([`files`]) and signed downloads
//! and uploads ([`oss`]), the refusal of
//! every other file operation ([`refuse`]), the socket handshake ([`crate::socket`]) and the
//! catch-all ([`catchall`]).

pub mod catchall;
pub mod envelope;
pub mod files;
pub mod limit;
pub mod login;
pub mod oss;
pub mod refuse;
pub mod server;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::Value;
use tower_http::catch_panic::CatchPanicLayer;

use crate::SyncMode;
use crate::auth::Device;
use crate::diag::{LogEntry, LogKind};
use crate::server::{AppState, MAX_REQUEST_BODY};
use envelope::{LogDevice, LogFlags, Reply, Streamed};

pub const FLAG_UNKNOWN: &str = "unknown_endpoint";
pub const FLAG_RETRY: &str = "retry_suspected";

/// The implemented routes (method, path), besides the refused families. A request outside
/// them and outside the documented refused endpoints is flagged `unknown_endpoint`.
const IMPLEMENTED: &[(&str, &str)] = &[
    ("POST", "/api/terminal/equipment/unlink"),
    ("POST", "/api/official/user/check/exists/server"),
    ("POST", "/api/official/user/query/random/code"),
    ("POST", "/api/official/user/account/login/equipment"),
    ("POST", "/api/terminal/user/bindEquipment"),
    ("POST", "/api/user/query"),
    ("POST", "/api/equipment/bind/status"),
    ("POST", "/api/user/logout"),
    ("POST", "/api/user/query/token"),
    ("GET", "/api/file/query/server"),
    ("POST", "/api/file/query/server"),
    ("POST", "/api/official/system/base/param"),
    ("GET", crate::socket::PATH),
];

/// The file and oss routes served in accept mode (in refuse mode they answer `E0079`).
const ACCEPT_IMPLEMENTED: &[(&str, &str)] = &[
    ("POST", "/api/file/2/files/synchronous/start"),
    ("POST", "/api/file/2/files/synchronous/end"),
    ("POST", "/api/file/2/files/list_folder"),
    ("POST", "/api/file/3/files/list_folder_v3"),
    ("POST", "/api/file/3/files/query_v3"),
    ("POST", "/api/file/3/files/query/by/path_v3"),
    ("POST", "/api/file/3/files/download_v3"),
    ("GET", "/api/oss/download"),
    ("POST", "/api/file/capacity/query"),
    ("POST", "/api/file/2/users/get_space_usage"),
    ("POST", "/api/file/3/files/upload/apply"),
    ("POST", "/api/file/2/files/upload/finish"),
    ("POST", UPLOAD_PATH),
    ("POST", UPLOAD_PART_PATH),
    ("POST", "/api/file/2/files/create_folder_v2"),
    ("POST", "/api/file/3/files/delete_folder_v3"),
    ("POST", "/api/file/3/files/move_v3"),
    ("POST", "/api/file/3/files/copy_v3"),
];

/// The byte uploads: their bodies stream to staging and are never read by the request log.
pub const UPLOAD_PATH: &str = "/api/oss/upload";
pub const UPLOAD_PART_PATH: &str = "/api/oss/upload/part";

/// Whether accept mode streams this request's body to disk (the log does not read it).
fn is_streamed_upload(state: &AppState, method: &axum::http::Method, path: &str) -> bool {
    state.sync_mode() == SyncMode::Accept
        && method == axum::http::Method::POST
        && (path == UPLOAD_PATH || path == UPLOAD_PART_PATH)
}

fn is_implemented(method: &axum::http::Method, path: &str) -> bool {
    IMPLEMENTED
        .iter()
        .any(|(m, p)| method.as_str() == *m && path == *p)
}

/// Whether accept mode serves this method and path (see [`ACCEPT_IMPLEMENTED`]).
fn is_accept_implemented(method: &axum::http::Method, path: &str) -> bool {
    ACCEPT_IMPLEMENTED
        .iter()
        .any(|(m, p)| method.as_str() == *m && path == *p)
}

/// The client address used for rate limits: the peer, or the `X-Forwarded-For` client when the
/// peer is a trusted proxy.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr(pub IpAddr);

/// The router with all routes, the catch-all, panic protection and the request log.
pub fn router(state: Arc<AppState>) -> Router {
    let base = Router::new()
        .route("/api/terminal/equipment/unlink", post(login::unlink))
        .route(
            "/api/official/user/check/exists/server",
            post(login::check_exists),
        )
        .route(
            "/api/official/user/query/random/code",
            post(login::random_code),
        )
        .route(
            "/api/official/user/account/login/equipment",
            post(login::login_equipment),
        )
        .route(
            "/api/terminal/user/bindEquipment",
            post(login::bind_equipment),
        )
        .route("/api/user/query", post(server::user_query))
        .route("/api/equipment/bind/status", post(server::bind_status))
        .route("/api/user/logout", post(server::logout))
        .route("/api/user/query/token", post(server::query_token))
        .route(
            "/api/file/query/server",
            get(server::query_server).post(server::query_server),
        )
        .route("/api/official/system/base/param", post(server::base_param))
        // The auto-sync socket: the handshake passes the request log like any request.
        .route(crate::socket::PATH, get(crate::socket::upgrade));
    // In accept mode the implemented file routes come first: a concrete route wins over the
    // wildcard refusal below, which keeps every family that is not implemented refused. In
    // refuse mode there are none, so every file and oss path answers `E0079` as in M2.
    let base = match state.sync_mode() {
        SyncMode::Accept => base.merge(accept_routes()),
        SyncMode::Refuse => base,
    };
    base
        // Safety: every other file, digest, task, oss, recycle, share and convert path.
        .route("/api/file/{*rest}", any(refuse::refuse))
        .route("/api/oss/{*rest}", any(refuse::refuse))
        .fallback(catchall::catch_all)
        .method_not_allowed_fallback(catchall::catch_all)
        .layer(CatchPanicLayer::custom(panic_reply))
        .layer(middleware::from_fn_with_state(state.clone(), log_request))
        .with_state(state)
}

/// The file sync routes of accept mode. Another method on one of these paths is refused like
/// any file path (`E0079`), not answered by the catch-all.
fn accept_routes() -> Router<Arc<AppState>> {
    fn p<H, T>(handler: H) -> axum::routing::MethodRouter<Arc<AppState>>
    where
        H: axum::handler::Handler<T, Arc<AppState>>,
        T: 'static,
    {
        post(handler).fallback(refuse::refuse)
    }
    Router::new()
        .route("/api/file/2/files/synchronous/start", p(files::sync_start))
        .route("/api/file/2/files/synchronous/end", p(files::sync_end))
        .route("/api/file/2/files/list_folder", p(files::list_folder))
        .route("/api/file/3/files/list_folder_v3", p(files::list_folder_v3))
        .route("/api/file/3/files/query_v3", p(files::query_v3))
        .route(
            "/api/file/3/files/query/by/path_v3",
            p(files::query_by_path_v3),
        )
        .route("/api/file/3/files/download_v3", p(files::download_v3))
        .route("/api/file/capacity/query", p(files::capacity_query))
        .route("/api/file/2/users/get_space_usage", p(files::space_usage))
        .route("/api/file/3/files/upload/apply", p(files::upload_apply))
        .route("/api/file/2/files/upload/finish", p(files::upload_finish))
        .route(
            "/api/file/2/files/create_folder_v2",
            p(files::create_folder_v2),
        )
        .route(
            "/api/file/3/files/delete_folder_v3",
            p(files::delete_folder_v3),
        )
        .route("/api/file/3/files/move_v3", p(files::move_v3))
        .route("/api/file/3/files/copy_v3", p(files::copy_v3))
        .route(
            "/api/oss/download",
            get(oss::download).fallback(refuse::refuse),
        )
        // The byte uploads lift axum's body limit (2 MB); the handlers stream the field to disk
        // and enforce their own limits (9 MiB per part, 1 GiB per file).
        .route(
            UPLOAD_PATH,
            post(oss::upload)
                .fallback(refuse::refuse)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            UPLOAD_PART_PATH,
            post(oss::upload_part)
                .fallback(refuse::refuse)
                .layer(DefaultBodyLimit::disable()),
        )
}

fn panic_reply(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    Reply::internal()
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
}

/// Change a device record and store the `version` it sent. A change from a stored value writes a
/// `firmware_changed` marker to the protocol log (that marker protects the log days around it
/// from rotation) and journals `device.firmware_changed`. The first sighting is not a change. Returns the record before (if any) and
/// after the change.
pub(crate) fn note_firmware(
    state: &AppState,
    slug: &str,
    serial: &str,
    version: Option<&str>,
    change: impl FnOnce(&mut Device),
) -> crate::Result<(Option<Device>, Device)> {
    let (before, after) = state.auth.devices().update(slug, serial, |d| {
        if let Some(v) = version {
            d.firmware = Some(v.to_string());
        }
        change(d);
    })?;
    let old = before.as_ref().and_then(|d| d.firmware.clone());
    if let (Some(old), Some(new)) = (old, version)
        && old != new
    {
        let mut m = LogEntry::new(LogKind::Marker);
        m.device = Some(serial.to_string());
        m.firmware = Some(new.to_string());
        m.flags = vec![crate::diag::FLAG_FIRMWARE_CHANGED.to_string()];
        state.log.record(m);
        let mut event = crate::journal::Event::new(
            slug,
            crate::journal::Origin::Device,
            Some(serial.to_string()),
            &crate::journal::new_op_id(),
            crate::journal::EventBody::DeviceFirmwareChanged {
                from: old,
                to: new.to_string(),
            },
        );
        // A journal failure is logged and never fails the request.
        if let Err(e) = state.journal.append(std::slice::from_mut(&mut event)) {
            eprintln!("notedown cloud: journal append failed: {e}");
        }
    }
    Ok((before, after))
}

/// The client address: the peer, or with a trusted proxy as the peer the rightmost
/// `X-Forwarded-For` entry that is not itself a trusted proxy (entries further left are set by
/// the client and can be forged).
pub fn client_ip(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    let peer = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if !trusted.contains(&peer) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .collect();
    forwarded
        .into_iter()
        .rev()
        .find(|ip| !trusted.contains(ip))
        .unwrap_or(peer)
}

/// The request body as it goes to the log: JSON as JSON, text as text (masked by the log),
/// anything else as a short description.
fn body_for_log(bytes: &[u8], content_type: &str) -> Option<Value> {
    if bytes.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return Some(v);
    }
    let binary = content_type.starts_with("multipart/")
        || content_type.starts_with("application/octet-stream");
    match std::str::from_utf8(bytes) {
        Ok(text) if !binary => Some(Value::String(text.to_string())),
        _ => Some(Value::String(format!(
            "<{} bytes of {}>",
            bytes.len(),
            if content_type.is_empty() {
                "unknown type"
            } else {
                content_type
            }
        ))),
    }
}

/// The text of a JSON body, for `request_body_raw` / `response_body_raw` (the log masks it and
/// keeps it only when it differs from the re-serialized body).
fn raw_json(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    serde_json::from_str::<serde::de::IgnoredAny>(text).ok()?;
    Some(text.to_string())
}

/// Writes one [`LogEntry`] per request: method, path, query, headers, masked bodies, status,
/// envelope result, duration, device, firmware and flags. The request body is read up to
/// [`MAX_REQUEST_BODY`]; a larger one is not passed on (the answer is still an envelope). The
/// byte uploads of accept mode are passed on unread and logged as `<N bytes, not logged>`.
pub async fn log_request(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let (mut parts, body) = request.into_parts();
    // The real peer, or the stand-in that tests install with `MockConnectInfo`.
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .or_else(|| {
            parts
                .extensions
                .get::<MockConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
        });
    let client = client_ip(peer, &parts.headers, &state.config.trusted_proxies);
    parts.extensions.insert(ClientAddr(client));

    let mut entry = LogEntry::new(LogKind::Request);
    entry.method = Some(parts.method.to_string());
    entry.path = Some(parts.uri.path().to_string());
    entry.query = parts.uri.query().map(str::to_string);
    for (name, value) in &parts.headers {
        entry.add_header(name.as_str(), &String::from_utf8_lossy(value.as_bytes()));
    }
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let response = if is_streamed_upload(&state, &parts.method, parts.uri.path()) {
        // Counted while the handler streams it; never buffered here.
        let seen = Arc::new(AtomicU64::new(0));
        let counter = seen.clone();
        let stream = futures_util::StreamExt::map(body.into_data_stream(), move |chunk| {
            if let Ok(b) = &chunk {
                counter.fetch_add(b.len() as u64, Ordering::Relaxed);
            }
            chunk
        });
        let request = Request::from_parts(parts, Body::from_stream(stream));
        let response = next.run(request).await;
        entry.request_body = Some(Value::String(format!(
            "<{} bytes, not logged>",
            seen.load(Ordering::Relaxed)
        )));
        response
    } else {
        match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
            Ok(bytes) => {
                entry.request_body = body_for_log(&bytes, &content_type);
                entry.request_body_raw = raw_json(&bytes);
                if let Some(Value::Object(m)) = &entry.request_body {
                    let text = |k: &str| match m.get(k) {
                        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                        _ => None,
                    };
                    entry.device = text("equipmentNo");
                    entry.firmware = text("version");
                }
                let request = Request::from_parts(parts, Body::from(bytes));
                next.run(request).await
            }
            Err(_) => {
                entry.request_body = Some(Value::String(format!(
                    "<body not read: larger than {MAX_REQUEST_BODY} bytes or broken; content-type {}>",
                    if content_type.is_empty() {
                        "none"
                    } else {
                        &content_type
                    }
                )));
                let normalized = refuse::normalize(parts.uri.path());
                let accepted = state.sync_mode() == SyncMode::Accept
                    && is_accept_implemented(&parts.method, parts.uri.path());
                let reply = if accepted {
                    // An implemented route: the note in request_body explains the refusal.
                    Reply::bad_params("body too large")
                } else if refuse::is_refused_path(&normalized) {
                    refuse::refusal(&parts.method, &parts.uri)
                } else {
                    // The note in request_body explains the refusal; only a path no route takes is
                    // unknown.
                    let reply = Reply::bad_params("body too large");
                    if is_implemented(&parts.method, parts.uri.path()) {
                        reply
                    } else {
                        reply.flag(FLAG_UNKNOWN)
                    }
                };
                reply.into_response()
            }
        }
    };

    if response.extensions().get::<Streamed>().is_some() {
        // A download: one entry with status and size, written before the bytes flow (its
        // duration is the time to the headers). The body is passed on untouched.
        let (mut rparts, rbody) = response.into_parts();
        let len = rparts
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("?")
            .to_string();
        entry.response_body = Some(Value::String(format!("<{len} bytes streamed>")));
        finish_entry(&state, entry, &mut rparts, client, started, false);
        return Response::from_parts(rparts, rbody);
    }

    let (mut rparts, rbody) = response.into_parts();
    let rbytes = axum::body::to_bytes(rbody, usize::MAX)
        .await
        .unwrap_or_else(|_| Bytes::new());
    let rjson = serde_json::from_slice::<Value>(&rbytes).ok();
    let success = rjson
        .as_ref()
        .and_then(|v| v.get("success"))
        .and_then(Value::as_bool);
    entry.success = success;
    entry.error_code = rjson
        .as_ref()
        .and_then(|v| v.get("errorCode"))
        .and_then(Value::as_str)
        .map(str::to_string);
    entry.response_body_raw = rjson.as_ref().and_then(|_| raw_json(&rbytes));
    entry.response_body = rjson.or_else(|| {
        (!rbytes.is_empty()).then(|| Value::String(String::from_utf8_lossy(&rbytes).into_owned()))
    });

    finish_entry(
        &state,
        entry,
        &mut rparts,
        client,
        started,
        success == Some(false),
    );
    rparts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Response::from_parts(rparts, Body::from(rbytes))
}

/// Complete and write the log entry of a request: flags and device from the response, status,
/// the retry check and the duration.
fn finish_entry(
    state: &AppState,
    mut entry: LogEntry,
    rparts: &mut axum::http::response::Parts,
    client: IpAddr,
    started: Instant,
    envelope_failed: bool,
) {
    if let Some(LogFlags(flags)) = rparts.extensions.remove::<LogFlags>() {
        entry.flags.extend(flags);
    }
    if entry.device.is_none()
        && let Some(LogDevice(d)) = rparts.extensions.remove::<LogDevice>()
    {
        entry.device = Some(d);
    }
    entry.response_status = Some(rparts.status.as_u16());
    let failed = envelope_failed || rparts.status.as_u16() >= 400;
    let key = format!(
        "{} {} {}",
        entry.method.as_deref().unwrap_or(""),
        entry.path.as_deref().unwrap_or(""),
        entry.device.clone().unwrap_or_else(|| client.to_string())
    );
    if state.retries.observe(key, state.now_ms(), failed) {
        entry.flags.push(FLAG_RETRY.to_string());
    }
    entry.duration_ms = Some(started.elapsed().as_millis() as u64);
    state.log.record(entry);
}
