//! The response envelope (`docs/sync-protocol.md` 7.1): `success`, `errorCode`, `errorMsg` at the
//! top level, payload fields beside them. Business errors use HTTP 200, as Ratta's server does.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

/// Not logged in or login expired.
pub const E_NOT_LOGGED_IN: &str = "E0712";
/// Synchronization in progress, wait: the answer to every file operation (Safety constraint).
pub const E_SYNC_BUSY: &str = "E0079";
/// Device bound to another account.
pub const E_BOUND_ELSEWHERE: &str = "E0083";
/// Generic error of the global handler (7.1); used for unknown endpoints and internal errors.
pub const E_GENERIC: &str = "500";
/// Bad parameters (7.1).
pub const E_BAD_PARAMS: &str = "422";
/// Too many requests from one address, or too many pending login codes (7.1 "resubmitted").
pub const E_TOO_MANY: &str = "409";

pub const MSG_NOT_LOGGED_IN: &str = "You are not logged in or your login has expired";
pub const MSG_SYNC_BUSY: &str = "Synchronization in progress, please wait";
pub const MSG_UNKNOWN: &str = "Unknown request";
pub const MSG_BAD_PARAMS: &str = "Bad parameters";
pub const MSG_TOO_MANY: &str = "Too many requests, please wait a minute";
pub const MSG_INTERNAL: &str = "Internal server error";

/// Flags for the request log, attached to a response as an extension.
#[derive(Debug, Clone, Default)]
pub struct LogFlags(pub Vec<String>);

/// The device serial for the request log when the body does not name it (token requests).
#[derive(Debug, Clone)]
pub struct LogDevice(pub String);

/// Marks a response whose body streams (a download): the request log records its status and
/// headers and passes the body through without reading it.
#[derive(Debug, Clone, Copy)]
pub struct Streamed;

/// One answer: an envelope plus what the request log should know about it.
#[derive(Debug)]
pub struct Reply {
    status: StatusCode,
    body: Map<String, Value>,
    flags: Vec<String>,
    device: Option<String>,
}

impl Reply {
    /// Success envelope with these payload fields beside it (`fields` must be an object).
    pub fn ok(fields: Value) -> Reply {
        let mut body = Map::new();
        body.insert("success".into(), Value::Bool(true));
        body.insert("errorCode".into(), Value::Null);
        body.insert("errorMsg".into(), Value::Null);
        if let Value::Object(m) = fields {
            body.extend(m);
        }
        Reply {
            status: StatusCode::OK,
            body,
            flags: Vec::new(),
            device: None,
        }
    }

    pub fn error(code: &str, msg: &str) -> Reply {
        let mut body = Map::new();
        body.insert("success".into(), Value::Bool(false));
        body.insert("errorCode".into(), json!(code));
        body.insert("errorMsg".into(), json!(msg));
        Reply {
            status: StatusCode::OK,
            body,
            flags: Vec::new(),
            device: None,
        }
    }

    /// `E0079`: the device ends its sync and retries later; nothing changes on either side.
    pub fn sync_busy() -> Reply {
        Reply::error(E_SYNC_BUSY, MSG_SYNC_BUSY)
    }

    pub fn not_logged_in() -> Reply {
        Reply::error(E_NOT_LOGGED_IN, MSG_NOT_LOGGED_IN)
    }

    pub fn bad_params(what: &str) -> Reply {
        Reply::error(E_BAD_PARAMS, &format!("{MSG_BAD_PARAMS}: {what}"))
    }

    pub fn too_many() -> Reply {
        Reply::error(E_TOO_MANY, MSG_TOO_MANY)
    }

    pub fn internal() -> Reply {
        Reply::error(E_GENERIC, MSG_INTERNAL)
    }

    pub fn with_status(mut self, status: StatusCode) -> Reply {
        self.status = status;
        self
    }

    pub fn flag(mut self, flag: &str) -> Reply {
        self.flags.push(flag.to_string());
        self
    }

    pub fn device(mut self, serial: &str) -> Reply {
        self.device = Some(serial.to_string());
        self
    }
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        let mut resp = (self.status, axum::Json(Value::Object(self.body))).into_response();
        if !self.flags.is_empty() {
            resp.extensions_mut().insert(LogFlags(self.flags));
        }
        if let Some(d) = self.device {
            resp.extensions_mut().insert(LogDevice(d));
        }
        resp
    }
}

/// Request body as JSON. Empty means `{}` (the device sends `{}` or nothing on some calls).
pub fn parse_body(bytes: &[u8]) -> Result<Value, Reply> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(bytes).map_err(|_| Reply::bad_params("body is not JSON"))
}

/// A field as a trimmed, non-empty string; numbers are accepted and turned into text.
pub fn field(body: &Value, key: &str) -> Option<String> {
    let s = match body.get(key)? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}
