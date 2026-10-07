//! One socket session: the handshake, the heartbeat answers, server pushes and the frame log.
//! The session's queued file push goes out right after the reply to its `ratta_ping` or
//! `status` (`docs/sync-protocol.md` 4.4).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use rand::RngExt;
use serde_json::Value;

use super::eio::{self, Action};
use super::registry::Outbound;
use crate::diag::{LogEntry, LogKind};
use crate::server::AppState;

/// Flag of a frame that is neither a known Engine.IO packet nor a known event.
pub const FLAG_UNKNOWN_EVENT: &str = "unknown_event";

/// A session without any frame from the device for this long is closed (a device that went to
/// sleep leaves a half-open connection behind). Far above every heartbeat interval: the capture
/// must see the device's own heartbeat style, so the Engine.IO ping timeout is not enforced.
pub const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// A send or close that does not finish within this time (the device stopped reading) ends the
/// session.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest WebSocket message and frame the server reads. Heartbeats are a few bytes; a larger
/// frame ends the session (logged as a receive error) instead of filling memory.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

const SID_LEN: usize = 20;
const SID_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A new random Engine.IO session id.
pub fn new_sid() -> String {
    let mut rng = rand::rng();
    (0..SID_LEN)
        .map(|_| SID_ALPHABET[rng.random_range(0..SID_ALPHABET.len())] as char)
        .collect()
}

/// The result of the token check, decided before the upgrade.
pub enum Access {
    Granted,
    /// Bad, stale or missing token: the reason goes to the device in the `403` message.
    Denied(&'static str),
    /// The token could not be checked (a read error): the session closes without the
    /// socket.io disconnect packet, which a client treats as final.
    Unavailable,
}

/// Who is on the other end, for the frame log.
pub struct Session {
    pub state: Arc<AppState>,
    pub sid: String,
    pub device: Option<String>,
    /// The account slug of a granted session; it is the registry key with the device serial.
    pub slug: Option<String>,
    pub path: String,
}

impl Session {
    fn log(&self, direction: &str, payload: Value, flags: &[&str]) {
        let mut e = LogEntry::new(LogKind::Frame);
        e.device = self.device.clone();
        e.path = Some(self.path.clone());
        e.direction = Some(direction.to_string());
        if direction == "in" {
            e.request_body = Some(payload);
        } else {
            e.response_body = Some(payload);
        }
        e.flags = flags.iter().map(|f| f.to_string()).collect();
        self.state.log.record(e);
    }

    /// Log, then send. `false` when the connection is gone or the send timed out.
    async fn send(&self, ws: &mut WebSocket, frame: String) -> bool {
        self.log("out", Value::String(frame.clone()), &[]);
        self.send_bounded(ws, Message::text(frame)).await
    }

    async fn send_bounded(&self, ws: &mut WebSocket, msg: Message) -> bool {
        match tokio::time::timeout(SEND_TIMEOUT, ws.send(msg)).await {
            Ok(r) => r.is_ok(),
            Err(_) => {
                self.log(
                    "out",
                    Value::String(format!(
                        "<send timed out after {} s, session ended>",
                        SEND_TIMEOUT.as_secs()
                    )),
                    &[],
                );
                false
            }
        }
    }

    async fn close(&self, ws: &mut WebSocket, code: u16, reason: &'static str) {
        self.log(
            "out",
            Value::String(format!("<close {code} {reason}>")),
            &[],
        );
        let close = Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }));
        self.send_bounded(ws, close).await;
    }
}

/// Removes the session from the registry on every exit path (only if the entry is still its own).
struct Registration {
    state: Arc<AppState>,
    slug: String,
    serial: String,
    sid: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.state
            .sockets
            .unregister(&self.slug, &self.serial, &self.sid);
    }
}

/// Run one session until the device or the server ends it.
pub async fn run(mut ws: WebSocket, session: Session, access: Access) {
    // Server speaks first: open packet, then the connect packet (4.2).
    if !session.send(&mut ws, eio::open_packet(&session.sid)).await
        || !session.send(&mut ws, eio::CONNECT.to_string()).await
    {
        return;
    }
    if let Access::Denied(reason) = access {
        if session.send(&mut ws, eio::rejection(reason)).await
            && session.send(&mut ws, eio::DISCONNECT.to_string()).await
        {
            session
                .close(&mut ws, close_code::POLICY, "forbidden")
                .await;
        }
        return;
    }
    if let Access::Unavailable = access {
        session
            .close(&mut ws, close_code::ERROR, "unavailable")
            .await;
        return;
    }
    // The registration guard lives until the session ends, whatever the exit path.
    // The last inbound time tells the registry that the device is still there (LIVENESS_MS).
    let (registration, mut rx, last_inbound) = match (&session.slug, &session.device) {
        (Some(slug), Some(serial)) => {
            let (rx, last_inbound) =
                session
                    .state
                    .sockets
                    .register(slug, serial, &session.sid, session.state.now_ms());
            let guard = Registration {
                state: session.state.clone(),
                slug: slug.clone(),
                serial: serial.clone(),
                sid: session.sid.clone(),
            };
            (Some(guard), Some(rx), Some(last_inbound))
        }
        _ => (None, None, None),
    };
    // The idle rule counts frames from the device only: the deadline moves when a message
    // arrives, never when the server pushes, so a half-open session cannot swallow pushes forever.
    let mut deadline = tokio::time::Instant::now() + IDLE_LIMIT;
    loop {
        let msg = tokio::select! {
            received = tokio::time::timeout_at(deadline, ws.recv()) => match received {
                Err(_) => {
                    session.close(&mut ws, close_code::AWAY, "idle").await;
                    return;
                }
                Ok(None) => return,
                Ok(Some(Err(e))) => {
                    // A dropped connection, a frame above MAX_FRAME_BYTES or a protocol error.
                    session.log("in", Value::String(format!("<connection ended: {e}>")), &[]);
                    return;
                }
                Ok(Some(Ok(msg))) => {
                    deadline = tokio::time::Instant::now() + IDLE_LIMIT;
                    if let Some(last) = &last_inbound {
                        last.store(session.state.now_ms(), Ordering::Relaxed);
                    }
                    msg
                }
            },
            out = recv_outbound(&mut rx) => match out {
                Outbound::Frame(frame) => {
                    if !session.send(&mut ws, frame).await {
                        return;
                    }
                    continue;
                }
                Outbound::Close(reason) => {
                    session.close(&mut ws, close_code::AWAY, reason).await;
                    return;
                }
            },
        };
        match msg {
            Message::Text(text) => {
                let text = text.as_str();
                let action = match &registration {
                    Some(r) => {
                        let sockets = &session.state.sockets;
                        let now = session.state.now_ms();
                        eio::answer_with(
                            text,
                            || sockets.deliver(&r.slug, &r.serial, &r.sid, now),
                            || sockets.received(&r.slug, &r.serial, &r.sid),
                        )
                    }
                    None => eio::answer(text),
                };
                let unknown = matches!(
                    action,
                    Action::Unknown | Action::Reply { unknown: true, .. }
                );
                let flags: &[&str] = if unknown { &[FLAG_UNKNOWN_EVENT] } else { &[] };
                session.log("in", Value::String(text.to_string()), flags);
                match action {
                    Action::Answer(frame) => {
                        if !session.send(&mut ws, frame).await {
                            return;
                        }
                    }
                    Action::Reply { frames, .. } => {
                        for frame in frames {
                            if !session.send(&mut ws, frame).await {
                                return;
                            }
                        }
                    }
                    Action::Close => {
                        session.close(&mut ws, close_code::NORMAL, "bye").await;
                        return;
                    }
                    Action::Nothing | Action::Unknown => {}
                }
            }
            Message::Binary(bytes) => {
                // Engine.IO 3 binary data only follows a binary event, which the device does not
                // send (4.6).
                session.log(
                    "in",
                    Value::String(format!("<{} bytes binary>", bytes.len())),
                    &[FLAG_UNKNOWN_EVENT],
                );
            }
            Message::Ping(bytes) => {
                // The WebSocket layer answers with a pong itself.
                session.log(
                    "in",
                    Value::String(format!("<websocket ping, {} bytes>", bytes.len())),
                    &[],
                );
            }
            Message::Pong(bytes) => {
                session.log(
                    "in",
                    Value::String(format!("<websocket pong, {} bytes>", bytes.len())),
                    &[],
                );
            }
            Message::Close(frame) => {
                let text = match frame {
                    Some(f) => format!("<close {} {}>", f.code, f.reason.as_str()),
                    None => "<close>".to_string(),
                };
                session.log("in", Value::String(text), &[]);
                return;
            }
        }
    }
}

/// The next server instruction. Pends forever when the session has no channel. A registered
/// session whose sender is gone was replaced (the registry dropped it, and the notice may have
/// been lost on a full channel), so that is the same as `Close("replaced")`.
async fn recv_outbound(rx: &mut Option<tokio::sync::mpsc::Receiver<Outbound>>) -> Outbound {
    match rx {
        Some(rx) => match rx.recv().await {
            Some(out) => out,
            None => Outbound::Close("replaced"),
        },
        None => std::future::pending().await,
    }
}
