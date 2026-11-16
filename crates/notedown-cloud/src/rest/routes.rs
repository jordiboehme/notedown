//! The REST routes. Disk work runs in `spawn_blocking`; a 500 answer carries only
//! `{"error":"internal"}`, its detail goes to the request log and stderr.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{PREFIX, RefusedToken, browse};
use crate::api::ClientAddr;
use crate::api::envelope::{LogMessage, Streamed};
use crate::api::oss::{Flow, stream_body};
use crate::auth::{Account, AuthError, Scope, TokenCheck};
use crate::pages::valid_key;
use crate::read::{self, NoteFile, ServerAccount};
use crate::server::AppState;
use crate::store::WirePath;

/// The default and the largest `limit` of `GET /journal`.
pub const JOURNAL_LIMIT: usize = 10_000;

/// The header of a note answer: `found`, `missing` or `not_a_file`.
pub const NOTE_HEADER: &str = "x-notedown-note";

/// The REST routes, with their state, for `serve_until_with`. No fallback (merging two
/// routers with one each panics): `/rest/v1/{*rest}` answers every other path, and every route
/// answers another method, with 404 `no_route`.
pub fn router(state: Arc<AppState>) -> Router {
    let path = |p: &str| format!("{PREFIX}{p}");
    let get_only = |r: MethodRouter<Arc<AppState>>| r.fallback(no_route);
    Router::new()
        .route(&path("/login/code"), post(login_code).fallback(no_route))
        .route(&path("/login"), post(login).fallback(no_route))
        .route(&path("/account"), get_only(get(account)))
        .route(&path("/records"), get_only(get(records)))
        .route(&path("/records/{key}"), get_only(get(record)))
        .route(&path("/note"), get_only(get(note)))
        .route(&path("/digests"), get_only(get(digests)))
        .route(&path("/journal"), get_only(get(journal)))
        .route(&path("/tree"), get_only(get(browse::tree)))
        .route(&path("/entries/{id}"), get_only(get(browse::entry)))
        .route(&path("/files/{id}"), get_only(get(browse::file)))
        .route(PREFIX, any(no_route))
        .route(&path("/"), any(no_route))
        .route(&path("/{*rest}"), any(no_route))
        .with_state(state)
}

/// `{"error": code}` with `status`.
pub(super) fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

pub(super) fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "bad_request", "message": message})),
    )
        .into_response()
}

/// 500 `internal`; `detail` (never a secret) goes to the request log and stderr only.
pub(super) fn internal(detail: impl std::fmt::Display) -> Response {
    let detail = detail.to_string();
    eprintln!("notedown cloud: rest: {detail}");
    let mut r = error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    r.extensions_mut().insert(LogMessage(detail));
    r
}

async fn no_route() -> Response {
    error(StatusCode::NOT_FOUND, "no_route")
}

/// Runs disk work off the runtime; an error or a panic is a 500.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> crate::Result<T> + Send + 'static,
) -> Result<T, Refusal> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Box::new(internal(e))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// An answer that ends a handler early (boxed: a `Response` is large).
pub(super) type Refusal = Box<Response>;

/// Who a token speaks for.
enum Caller {
    Live(Box<Account>),
    Removed {
        slug: String,
        user_id: u64,
        seq: u64,
    },
}

/// The token of `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// 401 `token_refused`, marked with the token id for the request log.
fn refused(id: String) -> Response {
    let mut r = error(StatusCode::UNAUTHORIZED, "token_refused");
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    r.extensions_mut().insert(RefusedToken(id));
    r
}

/// 410 `account_removed` with the removal, marked with the token id for the request log.
fn removed(id: String, user_id: u64, seq: u64) -> Response {
    let mut r = (
        StatusCode::GONE,
        Json(json!({"error": "account_removed", "userId": user_id, "seq": seq})),
    )
        .into_response();
    r.extensions_mut().insert(RefusedToken(id));
    r
}

/// The caller of a request, or the refusal to answer. The id is that of a token with a good
/// MAC, else `-`.
async fn caller(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(Caller, String), Refusal> {
    let Some(token) = bearer(headers).map(str::to_string) else {
        return Err(Box::new(refused("-".to_string())));
    };
    let Some((slug, id)) = state.rest_tokens.slug_and_id(&token) else {
        return Err(Box::new(refused("-".to_string())));
    };
    let st = state.clone();
    let now = state.now_ms();
    match blocking(move || st.rest_tokens.check(&token, now)).await? {
        // The scope is enforced by the write routes.
        TokenCheck::Valid(account, _scope) => Ok((Caller::Live(Box::new(account)), id)),
        TokenCheck::Removed { user_id, seq } => Ok((Caller::Removed { slug, user_id, seq }, id)),
        TokenCheck::Refused => Err(Box::new(refused(id))),
    }
}

/// The live account of the caller; a removed one is 410.
pub(super) async fn account_of(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<Account, Refusal> {
    match caller(state, headers).await? {
        (Caller::Live(account), _) => Ok(*account),
        (Caller::Removed { user_id, seq, .. }, id) => Err(Box::new(removed(id, user_id, seq))),
    }
}

/// The client address that the request log found; without the log (a bare router) all
/// requests share one.
fn client(addr: Option<Extension<ClientAddr>>) -> IpAddr {
    addr.map_or(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        |Extension(ClientAddr(ip))| ip,
    )
}

#[derive(Deserialize)]
struct CodeBody {
    email: String,
}

/// `POST /login/code` `{"email"}`: a random code for the login, from the store and with the
/// limits of the device codes.
async fn login_code(
    State(state): State<Arc<AppState>>,
    addr: Option<Extension<ClientAddr>>,
    body: Bytes,
) -> Response {
    let now = state.now_ms();
    if !state.limiter.allow(client(addr), "random_code", now) {
        return error(StatusCode::TOO_MANY_REQUESTS, "too_many");
    }
    let Ok(body) = serde_json::from_slice::<CodeBody>(&body) else {
        return bad_request("the body must be {\"email\"}");
    };
    match state.auth.challenge(&body.email, now) {
        Some((code, ts)) => Json(json!({"randomCode": code, "timestamp": ts})).into_response(),
        None => error(StatusCode::TOO_MANY_REQUESTS, "too_many"),
    }
}

/// The login body: the password travels only as `sha256_hex(md5_hex(password) + randomCode)`;
/// any other field (a clear `password`) is refused. `scope` is `read` (the default) or `write`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginBody {
    email: String,
    password_hash: String,
    name: String,
    #[serde(default)]
    scope: Scope,
}

/// `POST /login`: 201 with a new token of the asked scope. A wrong email, password or code and
/// a locked account are all 401 `login_failed`.
async fn login(
    State(state): State<Arc<AppState>>,
    addr: Option<Extension<ClientAddr>>,
    body: Bytes,
) -> Response {
    let now = state.now_ms();
    if !state.limiter.allow(client(addr), "rest_login", now) {
        return error(StatusCode::TOO_MANY_REQUESTS, "too_many");
    }
    let Ok(body) = serde_json::from_slice::<LoginBody>(&body) else {
        return bad_request(
            "the body must be {\"email\",\"passwordHash\",\"name\"} with an optional \"scope\" of \"read\" or \"write\"",
        );
    };
    let st = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        st.auth.rest_login(
            &body.email,
            &body.password_hash,
            &body.name,
            body.scope,
            now,
        )
    })
    .await;
    match result {
        Ok(Ok((token, info, account))) => (
            StatusCode::CREATED,
            Json(json!({
                "token": token,
                "id": info.id,
                "slug": account.slug,
                "userId": account.user_id,
                "scope": info.scope,
            })),
        )
            .into_response(),
        Ok(Err(AuthError::Internal(e))) => internal(e),
        Ok(Err(_)) => error(StatusCode::UNAUTHORIZED, "login_failed"),
        Err(e) => internal(e),
    }
}

/// `GET /account`: `{"slug","userId"}`.
async fn account(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match account_of(&state, &headers).await {
        Ok(a) => Json(ServerAccount {
            slug: a.slug,
            user_id: a.user_id,
        })
        .into_response(),
        Err(r) => *r,
    }
}

/// `GET /records`: [`read::Records`].
async fn records(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let data = state.data_dir.clone();
    match blocking(move || read::records(&data, &account.slug)).await {
        Ok(records) => Json(records).into_response(),
        Err(r) => *r,
    }
}

/// `GET /records/{key}`: [`read::Record`], 404 `no_record`.
async fn record(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    if !valid_key(&key) {
        return bad_request("the key is not a FILE_ID or path:<md5>");
    }
    let data = state.data_dir.clone();
    match blocking(move || read::record(&data, &account.slug, &key)).await {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no_record"),
        Err(r) => *r,
    }
}

/// `GET /digests`: the live digests.
async fn digests(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let data = state.data_dir.clone();
    match blocking(move || read::digests(&data, &account.slug)).await {
        Ok(digests) => Json(digests).into_response(),
        Err(r) => *r,
    }
}

/// The query of a route; a query that does not parse is a 400.
pub(super) fn query<T: serde::de::DeserializeOwned>(uri: &Uri) -> Result<T, Refusal> {
    Query::<T>::try_from_uri(uri)
        .map(|Query(q)| q)
        .map_err(|e| Box::new(bad_request(&e.body_text())))
}

#[derive(Deserialize)]
struct JournalQuery {
    from: Option<String>,
    limit: Option<String>,
}

fn number(text: Option<&str>, name: &str, default: u64) -> Result<u64, Refusal> {
    match text {
        None => Ok(default),
        Some(t) => t
            .parse()
            .map_err(|_| Box::new(bad_request(&format!("{name} is not a number")))),
    }
}

/// `GET /journal?from=&limit=`: [`read::JournalBatch`] from `from` (default 0), at most
/// `limit` (default and maximum [`JOURNAL_LIMIT`]). A removed account's token reads up to its
/// removal.
async fn journal(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let (caller, _) = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let (from, limit) = match query::<JournalQuery>(&uri).and_then(|q| {
        Ok((
            number(q.from.as_deref(), "from", 0)?,
            number(q.limit.as_deref(), "limit", JOURNAL_LIMIT as u64)?,
        ))
    }) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let limit = usize::try_from(limit).map_or(JOURNAL_LIMIT, |l| l.min(JOURNAL_LIMIT));
    let (slug, until) = match caller {
        Caller::Live(a) => (a.slug, u64::MAX),
        Caller::Removed { slug, seq, .. } => (slug, seq),
    };
    let data = state.data_dir.clone();
    match blocking(move || read::journal_until(&data, &slug, from, limit, until)).await {
        Ok(batch) => Json(batch).into_response(),
        Err(r) => *r,
    }
}

#[derive(Deserialize)]
struct NoteQuery {
    path: Option<String>,
}

/// What `open_note` found, with the file's ETag.
enum Opened {
    Found(std::fs::File, u64, String),
    Missing,
    NotAFile,
}

/// Whether `If-None-Match` names `etag` (weak comparison) or is `*`.
pub(super) fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_string();
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim() == "*" || strip(t) == strip(etag))
}

fn with_note_header(mut r: Response, tell: &'static str) -> Response {
    r.headers_mut()
        .insert(NOTE_HEADER, HeaderValue::from_static(tell));
    r
}

/// `GET` and `HEAD /note?path=`: the note's bytes, streamed from the open file, with
/// `Content-Length` and `ETag: W/"<size>-<mtime ms>"`; 304 for a matching `If-None-Match`;
/// 404 `missing`, 409 `not_a_file`. Every answer has [`NOTE_HEADER`]; `HEAD` has no body.
async fn note(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let account = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let path = match query::<NoteQuery>(&uri) {
        Ok(NoteQuery { path: Some(p) }) => p,
        Ok(NoteQuery { path: None }) => return bad_request("path is missing"),
        Err(r) => return *r,
    };
    if WirePath::parse(&path).is_err() {
        return bad_request("path is not a valid note path");
    }
    let data = state.data_dir.clone();
    let opened = blocking(move || {
        Ok(match read::open_note(&data, &account.slug, &path)? {
            NoteFile::Found(file, size) => {
                let mtime = file
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis());
                Opened::Found(file, size, format!("W/\"{size}-{mtime}\""))
            }
            NoteFile::Missing => Opened::Missing,
            NoteFile::NotAFile => Opened::NotAFile,
        })
    })
    .await;
    let (file, size, etag) = match opened {
        Ok(Opened::Found(file, size, etag)) => (file, size, etag),
        Ok(Opened::Missing) => {
            return with_note_header(error(StatusCode::NOT_FOUND, "missing"), "missing");
        }
        Ok(Opened::NotAFile) => {
            return with_note_header(error(StatusCode::CONFLICT, "not_a_file"), "not_a_file");
        }
        Err(r) => return *r,
    };
    let etag_value = match HeaderValue::from_str(&etag) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let mut r = if not_modified(&headers, &etag) {
        drop(file);
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        let body = if method == Method::HEAD {
            drop(file);
            Body::empty()
        } else {
            stream_body(Flow::whole(file, size))
        };
        let mut r = Response::new(body);
        let h = r.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
        r.extensions_mut().insert(Streamed);
        r
    };
    r.headers_mut().insert(header::ETAG, etag_value);
    with_note_header(r, "found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bearer_token_is_read_from_the_header() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer  abc "),
        );
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer(&h), None);
    }

    #[test]
    fn if_none_match_compares_weakly() {
        let mut h = HeaderMap::new();
        assert!(!not_modified(&h, "W/\"1-2\""));
        h.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"0-0\", W/\"1-2\""),
        );
        assert!(not_modified(&h, "W/\"1-2\""));
        assert!(!not_modified(&h, "W/\"1-3\""));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(not_modified(&h, "W/\"1-3\""));
    }
}
