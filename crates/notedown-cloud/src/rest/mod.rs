//! The REST API under [`PREFIX`], for a pipeline on another machine and for the Mac app, which
//! browses and downloads the account's files ([`browse`]).
//!
//! Plain JSON with real HTTP status codes, not the device envelope; every error body is
//! `{"error": "<code>", ...}`. The token in `Authorization: Bearer <token>` decides the account
//! (see [`crate::auth::RestTokens`]); no path names one. The answers come from the same
//! [`crate::read`] functions that the pipeline uses on a mounted folder, so they never depend
//! on the transport; the app's routes work on the store under its lock, as the device routes
//! do.
//!
//! The routes ([`router`]) are merged next to the device routes (`serve_until_with`), so they
//! pass the request log and its panic guard. The log keeps quiet about the polls
//! ([`unlogged`]).

mod browse;
mod routes;

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::Method;
use axum::response::Response;

use crate::server::AppState;

pub use routes::router;

/// The path every REST route starts with.
pub const PREFIX: &str = "/rest/v1";

/// A refused token (401) or a token of a removed account (410) is logged at most once in this
/// time per token id and path.
pub const REFUSAL_LOG_MS: u64 = 10 * 60 * 1000;

/// At most this many (token id, path) pairs are remembered for [`REFUSAL_LOG_MS`]. When all
/// are recent, a refusal is logged every time instead.
const MAX_REMEMBERED: usize = 1024;

/// Whether `path` belongs to the REST API.
pub fn is_rest_path(path: &str) -> bool {
    path.strip_prefix(PREFIX)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The token id of a refused or removed answer, for the request log (`-` when the token has
/// none that could be trusted). Never the token.
#[derive(Debug, Clone)]
pub(crate) struct RefusedToken(pub(crate) String);

/// When each refusal was last logged, by (token id, path).
#[derive(Debug, Default)]
pub(crate) struct RefusalLog {
    logged: Mutex<HashMap<(String, String), u64>>,
}

impl RefusalLog {
    /// Whether a refusal for `id` on `path` is to be logged now, and if so it is remembered.
    fn log_now(&self, id: &str, path: &str, now_ms: u64) -> bool {
        let mut map = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        let key = (id.to_string(), path.to_string());
        let recent = |at: &u64| now_ms.saturating_sub(*at) < REFUSAL_LOG_MS;
        if map.get(&key).is_some_and(recent) {
            return false;
        }
        if map.len() >= MAX_REMEMBERED && !map.contains_key(&key) {
            map.retain(|_, at| recent(at));
            if map.len() >= MAX_REMEMBERED {
                return true;
            }
        }
        map.insert(key, now_ms);
        true
    }
}

/// Whether the request log leaves out this answer: a REST `GET` or `HEAD` that answered 200,
/// 201, 206 or 304 (the polls of the pipeline and the downloads of the app), and a 401 or 410
/// for a token id and path that was logged in the last [`REFUSAL_LOG_MS`]. Every other answer
/// is logged, logins too.
pub(crate) fn unlogged(state: &AppState, method: &Method, path: &str, response: &Response) -> bool {
    if !is_rest_path(path) {
        return false;
    }
    let status = response.status().as_u16();
    if (method == Method::GET || method == Method::HEAD) && matches!(status, 200 | 201 | 206 | 304)
    {
        return true;
    }
    match response.extensions().get::<RefusedToken>() {
        Some(RefusedToken(id)) if matches!(status, 401 | 410) => {
            !state.rest_refusals.log_now(id, path, state.now_ms())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_paths_are_those_under_the_prefix() {
        assert!(is_rest_path("/rest/v1"));
        assert!(is_rest_path("/rest/v1/"));
        assert!(is_rest_path("/rest/v1/journal"));
        assert!(!is_rest_path("/rest/v10/journal"));
        assert!(!is_rest_path("/rest"));
        assert!(!is_rest_path("/api/file/query/server"));
    }

    #[test]
    fn a_refusal_is_logged_once_per_window_and_the_map_stays_bounded() {
        let log = RefusalLog::default();
        assert!(log.log_now("a", "/p", 0));
        assert!(!log.log_now("a", "/p", REFUSAL_LOG_MS - 1));
        assert!(log.log_now("a", "/q", 1));
        assert!(log.log_now("b", "/p", 1));
        assert!(log.log_now("a", "/p", REFUSAL_LOG_MS));
        for i in 0..MAX_REMEMBERED * 2 {
            assert!(log.log_now("c", &format!("/{i}"), 10));
        }
        assert!(log.logged.lock().unwrap().len() <= MAX_REMEMBERED);
        // When every remembered one is recent, a new pair is logged every time.
        assert!(log.log_now("d", "/x", 11));
        assert!(log.log_now("d", "/x", 12));
        // Old ones make room again.
        assert!(log.log_now("e", "/y", 10 + REFUSAL_LOG_MS));
        assert!(!log.log_now("e", "/y", 11 + REFUSAL_LOG_MS));
    }
}
