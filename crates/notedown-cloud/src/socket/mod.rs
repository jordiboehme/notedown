//! The auto-sync socket: Engine.IO 3 / socket.io 2 over the axum WebSocket
//! (`docs/sync-protocol.md` 4).
//!
//! `GET /socket.io/?token=...&type=...&random=...&sign=...&EIO=3&transport=websocket` is upgraded
//! to a WebSocket. The handshake request passes the request log like every other request (one
//! entry, token and sign masked); every frame in both directions is logged as `kind: frame`. The
//! token is checked for signature and generation; `sign` is accepted and ignored (4.2). A bad
//! token still gets the open and connect packets, then `ServerMessage` `{"code":"403",...}` and
//! the close, as Ratta does. A granted session registers in the [`SocketRegistry`] and gets the
//! heartbeat answers of 4.3 plus the pushes the server hands to the registry.

pub mod eio;
pub mod handler;
pub mod registry;

pub use registry::{LIVENESS_MS, LastInbound, Outbound, RESEND_MS, SocketRegistry};

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Query, State};
use axum::http::{Method, Uri};
use axum::response::{IntoResponse, Response};

use crate::api::catchall::catch_all;
use crate::api::envelope::{LogDevice, LogFlags};
use crate::server::AppState;
use handler::{Access, Session};

/// The socket path (4.1).
pub const PATH: &str = "/socket.io/";

/// Flag of a handshake whose token was missing, bad or stale.
pub const FLAG_UNAUTHENTICATED: &str = "unauthenticated";

/// The handshake route. A request that is not a WebSocket upgrade (for example Engine.IO long
/// polling) gets the envelope of the catch-all and the `unknown_endpoint` flag.
pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Ok(ws) = ws else {
        return catch_all(method, uri).await.into_response();
    };
    let query = query.map(|Query(q)| q).unwrap_or_default();
    let token = query.get("token").map(String::as_str).unwrap_or("");
    let (access, device, slug) = if token.is_empty() {
        (Access::Denied("Token missing"), None, None)
    } else {
        match state.auth.authenticate(token) {
            Ok(a) => (
                Access::Granted,
                Some(a.claims.equipment_no),
                Some(a.account.slug),
            ),
            // A bad or stale token is refused. Any other error (the account or device file could
            // not be read) is not the device's fault: the session closes without the socket.io
            // disconnect, so the device connects again.
            Err(crate::auth::AuthError::NotLoggedIn) => {
                (Access::Denied("Token verification failed"), None, None)
            }
            Err(e) => {
                eprintln!("notedown cloud: socket: {e}");
                (Access::Unavailable, None, None)
            }
        }
    };
    // Without a valid token the device stays unnamed: the serial it claims is visible in the
    // logged query only.
    let denied = matches!(access, Access::Denied(_));
    let session = Session {
        state,
        sid: handler::new_sid(),
        device: device.clone(),
        slug,
        path: uri.path().to_string(),
    };
    let ws = ws
        .max_message_size(handler::MAX_FRAME_BYTES)
        .max_frame_size(handler::MAX_FRAME_BYTES);
    let mut response = ws.on_upgrade(move |socket| handler::run(socket, session, access));
    if let Some(d) = device {
        response.extensions_mut().insert(LogDevice(d));
    }
    if denied {
        response
            .extensions_mut()
            .insert(LogFlags(vec![FLAG_UNAUTHENTICATED.to_string()]));
    }
    response
}
