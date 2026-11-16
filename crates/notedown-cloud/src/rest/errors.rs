//! The answers to store errors of the app's routes. A store `Invalid` message never reaches
//! the client: what is left after the REST target check ([`super::check`]) is a 500 with its
//! detail in the request log.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::routes::{error, internal};
use crate::Error;
use crate::api::envelope::LogMessage;

/// How long the app waits before it tries a write again while a device syncs (s).
pub const RETRY_AFTER_S: u64 = 5;

/// The answer to a store error of a write (and of a read): 423 `scan_hold` while a scan holds
/// the account, 423 `not_imported` while the protect gate is closed, 503 `store_damaged` for a
/// damaged tree (the reason goes to the log and stderr), 409 `name_taken` with `existing` for a
/// name collision, 412 `precondition_failed`, 404 `not_found` for an item of the recycle bin
/// whose bytes are gone, 409 `slot_damaged` for one whose slot is or holds a symlink; anything
/// else is 500 `internal`.
pub(crate) fn write_error(e: Error) -> Response {
    match e {
        Error::ScanHold(_) => error(StatusCode::LOCKED, "scan_hold"),
        Error::GateClosed(_) => error(StatusCode::LOCKED, "not_imported"),
        Error::StoreDamaged { .. } => {
            let text = e.to_string();
            eprintln!("notedown cloud: warning: {text}");
            let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "store_damaged");
            r.extensions_mut().insert(LogMessage(text));
            r
        }
        Error::NameCollision { existing, .. } => name_taken(&existing),
        Error::PreconditionFailed(_) => {
            error(StatusCode::PRECONDITION_FAILED, "precondition_failed")
        }
        Error::NotFound(_) => error(StatusCode::NOT_FOUND, "not_found"),
        Error::RecycleSlotDamaged(_) => error(StatusCode::CONFLICT, "slot_damaged"),
        e => internal(e),
    }
}

/// 409 `name_taken` with the name that is in the way.
pub(crate) fn name_taken(existing: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": "name_taken", "existing": existing})),
    )
        .into_response()
}

/// 423 `device_syncing` with `Retry-After`: a device holds the account's sync lock.
pub(crate) fn device_syncing() -> Response {
    let mut r = error(StatusCode::LOCKED, "device_syncing");
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_S));
    r
}
