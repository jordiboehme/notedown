//! HTTP routes of the capture server and the request log.
//!
//! Every request passes [`log_request`], which writes exactly one protocol-log line for it.
//! Routes: login and binding ([`login`]), server queries ([`server`]), the refusal of every file
//! operation ([`refuse`]), the socket handshake ([`crate::socket`]) and the catch-all
//! ([`catchall`]).

pub mod catchall;
pub mod envelope;
pub mod limit;
pub mod login;
pub mod refuse;
pub mod server;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::Value;
use tower_http::catch_panic::CatchPanicLayer;

use crate::auth::Device;
use crate::diag::{LogEntry, LogKind};
use crate::server::{AppState, MAX_REQUEST_BODY};
use envelope::{LogDevice, LogFlags, Reply};

pub const FLAG_UNKNOWN: &str = "unknown_endpoint";
pub const FLAG_RETRY: &str = "retry_suspected";
pub const FLAG_FIRMWARE: &str = "firmware_changed";

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

fn is_implemented(method: &axum::http::Method, path: &str) -> bool {
    IMPLEMENTED
        .iter()
        .any(|(m, p)| method.as_str() == *m && path == *p)
}

/// The client address used for rate limits: the peer, or the `X-Forwarded-For` client when the
/// peer is a trusted proxy.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr(pub IpAddr);

/// The router with all routes, the catch-all, panic protection and the request log.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
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
        .route(crate::socket::PATH, get(crate::socket::upgrade))
        // Safety: every other file, digest, task, oss, recycle, share and convert path.
        .route("/api/file/{*rest}", any(refuse::refuse))
        .route("/api/oss/{*rest}", any(refuse::refuse))
        .fallback(catchall::catch_all)
        .method_not_allowed_fallback(catchall::catch_all)
        .layer(CatchPanicLayer::custom(panic_reply))
        .layer(middleware::from_fn_with_state(state.clone(), log_request))
        .with_state(state)
}

fn panic_reply(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    Reply::internal()
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
}

/// Change a device record and store the `version` it sent. A change from a stored value writes a
/// `firmware_changed` marker to the protocol log (that marker protects the log days around it
/// from rotation). The first sighting is not a change.
pub(crate) fn note_firmware(
    state: &AppState,
    slug: &str,
    serial: &str,
    version: Option<&str>,
    change: impl FnOnce(&mut Device),
) -> crate::Result<Device> {
    let (before, after) = state.auth.devices().update(slug, serial, |d| {
        if let Some(v) = version {
            d.firmware = Some(v.to_string());
        }
        change(d);
    })?;
    let old = before.and_then(|d| d.firmware);
    if let (Some(old), Some(new)) = (old, version)
        && old != new
    {
        let mut m = LogEntry::new(LogKind::Marker);
        m.device = Some(serial.to_string());
        m.firmware = Some(new.to_string());
        m.flags = vec![FLAG_FIRMWARE.to_string()];
        state.log.record(m);
    }
    Ok(after)
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

/// Writes one [`LogEntry`] per request: method, path, query, headers, masked bodies, status,
/// envelope result, duration, device, firmware and flags. The request body is read up to
/// [`MAX_REQUEST_BODY`]; a larger one is not passed on (the answer is still an envelope).
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

    let response = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(bytes) => {
            entry.request_body = body_for_log(&bytes, &content_type);
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
            let reply = if refuse::is_refused_path(&normalized) {
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
    };

    let (mut rparts, rbody) = response.into_parts();
    let rbytes = axum::body::to_bytes(rbody, usize::MAX)
        .await
        .unwrap_or_else(|_| Bytes::new());
    let rjson = serde_json::from_slice::<Value>(&rbytes).ok();
    if let Some(LogFlags(flags)) = rparts.extensions.remove::<LogFlags>() {
        entry.flags.extend(flags);
    }
    if entry.device.is_none()
        && let Some(LogDevice(d)) = rparts.extensions.remove::<LogDevice>()
    {
        entry.device = Some(d);
    }
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
    entry.response_status = Some(rparts.status.as_u16());
    entry.response_body = rjson.or_else(|| {
        (!rbytes.is_empty()).then(|| Value::String(String::from_utf8_lossy(&rbytes).into_owned()))
    });

    let failed = success == Some(false) || rparts.status.as_u16() >= 400;
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
    rparts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Response::from_parts(rparts, Body::from(rbytes))
}
