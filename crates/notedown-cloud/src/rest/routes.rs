//! The REST routes. Disk work runs in `spawn_blocking`; a 500 answer carries only
//! `{"error":"internal"}`, its detail goes to the request log and stderr.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::{Body, Bytes};
use axum::extract::DefaultBodyLimit;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{PREFIX, RefusedToken, browse, recycle, write};
use crate::api::ClientAddr;
use crate::api::envelope::{LogMessage, Streamed};
use crate::api::oss::{Flow, stream_body};
use crate::auth::{Account, AuthError, Scope, TokenCheck};
use crate::pages::valid_key;
use crate::read::{self, DeviceView, NoteFile, ServerAccount};
use crate::server::AppState;
use crate::store::WirePath;

/// The default and the largest `limit` of `GET /journal`.
pub const JOURNAL_LIMIT: usize = 10_000;

/// The default and the largest `limit` of `GET /events`.
pub const EVENTS_LIMIT: usize = 1_000;

/// The header of a note answer: `found`, `missing` or `not_a_file`.
pub const NOTE_HEADER: &str = "x-notedown-note";

/// The REST routes, with their state, for `serve_until_with`. No fallback (merging two
/// routers with one each panics): `/rest/v1/{*rest}` answers every other path, and every route
/// answers another method, with 404 `no_route`.
pub fn router(state: Arc<AppState>) -> Router {
    let path = |p: &str| format!("{PREFIX}{p}");
    let get_only = |r: MethodRouter<Arc<AppState>>| r.fallback(no_route);
    // A JSON body is read whole: up to the limit of the request log, whose JSON 413 answers a
    // larger one (axum's own default of 2 MiB would answer plain text first).
    let json_body = |r: MethodRouter<Arc<AppState>>| {
        r.fallback(no_route)
            .layer(DefaultBodyLimit::max(crate::server::MAX_REQUEST_BODY))
    };
    Router::new()
        .route(&path("/login/code"), json_body(post(login_code)))
        .route(&path("/login"), json_body(post(login)))
        .route(&path("/account"), get_only(get(account)))
        .route(&path("/records"), get_only(get(records)))
        .route(&path("/records/{key}"), get_only(get(record)))
        .route(&path("/note"), get_only(get(note)))
        .route(&path("/digests"), get_only(get(digests)))
        .route(&path("/journal"), get_only(get(journal)))
        .route(&path("/events"), get_only(get(events)))
        .route(&path("/tree"), get_only(get(browse::tree)))
        .route(
            &path("/entries/{id}"),
            get(browse::entry).delete(write::delete).fallback(no_route),
        )
        .route(&path("/files/{id}"), get_only(get(browse::file)))
        // The upload streams its body to staging and enforces its own limit.
        .route(
            &path("/files"),
            put(write::upload)
                .fallback(no_route)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(&path("/folders"), json_body(post(write::folder)))
        .route(&path("/move"), json_body(post(write::move_)))
        .route(&path("/recycle"), get_only(get(recycle::list)))
        .route(&path("/recycle/restore"), json_body(post(recycle::restore)))
        .route(&path("/recycle/empty"), json_body(post(recycle::empty)))
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
    Live(Box<Account>, Scope),
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
        // The scope is enforced by the write routes ([`writer_of`]).
        TokenCheck::Valid(account, scope) => Ok((Caller::Live(Box::new(account), scope), id)),
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
        (Caller::Live(account, _), _) => Ok(*account),
        (Caller::Removed { user_id, seq, .. }, id) => Err(Box::new(removed(id, user_id, seq))),
    }
}

/// [`account_of`] for a write: a read token is 403 `read_only_token`.
pub(super) async fn writer_of(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<Account, Refusal> {
    match caller(state, headers).await? {
        (Caller::Live(account, Scope::Write), _) => Ok(*account),
        (Caller::Live(_, Scope::Read), _) => {
            Err(Box::new(error(StatusCode::FORBIDDEN, "read_only_token")))
        }
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

/// `GET /account`: [`ServerAccount`], the devices newest sync first.
async fn account(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let a = match account_of(&state, &headers).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let st = state.clone();
    let slug = a.slug.clone();
    let list = blocking(move || st.auth.devices().list_readable(&slug)).await;
    let list = match list {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let now = state.now_ms();
    let holder = state.sync_locks.holder(&a.slug, now);
    let mut devices: Vec<DeviceView> = list
        .into_iter()
        .map(|d| DeviceView {
            syncing: holder.as_deref() == Some(d.serial.as_str()),
            online: state.sockets.is_connected(&a.slug, &d.serial, now),
            serial: d.serial,
            name: d.name,
            bound: d.bound,
            last_login_ms: d.last_login_ms,
            last_sync_ms: d.last_sync_ms,
        })
        .collect();
    // Newest sync first, never synced last; `sort_by` is stable, so ties stay by serial.
    devices.sort_by_key(|x| std::cmp::Reverse(x.last_sync_ms));
    Json(ServerAccount {
        slug: a.slug,
        user_id: a.user_id,
        devices,
    })
    .into_response()
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

pub(super) fn number(text: Option<&str>, name: &str, default: u64) -> Result<u64, Refusal> {
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
        Caller::Live(a, _) => (a.slug, u64::MAX),
        Caller::Removed { slug, seq, .. } => (slug, seq),
    };
    let data = state.data_dir.clone();
    match blocking(move || read::journal_until(&data, &slug, from, limit, until)).await {
        Ok(batch) => Json(batch).into_response(),
        Err(r) => *r,
    }
}

/// `GET /events?from=&limit=`: [`read::EventPage`], the file and folder events of the
/// caller's account from `from` (default 0), at most `limit` (default and maximum
/// [`EVENTS_LIMIT`], 0 is 1). Events of an earlier account with the same slug are at or below
/// the caller's floor ([`crate::auth::RestTokens::floor`]) and never returned. A removed
/// account's token reads up to its removal, which is its last event; so does a live token whose
/// account is removed while it polls ([`follow`]).
async fn events(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let (caller, id) = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let (from, limit) = match query::<JournalQuery>(&uri).and_then(|q| {
        Ok((
            number(q.from.as_deref(), "from", 0)?,
            number(q.limit.as_deref(), "limit", EVENTS_LIMIT as u64)?,
        ))
    }) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let limit = usize::try_from(limit).map_or(EVENTS_LIMIT, |l| l.clamp(1, EVENTS_LIMIT));
    let (slug, user_id, until) = match caller {
        Caller::Live(a, _) => (a.slug, a.user_id, None),
        Caller::Removed { slug, user_id, seq } => (slug, user_id, Some(seq)),
    };
    let st = state.clone();
    match blocking(move || follow(&st, &slug, user_id, until, from, limit)).await {
        Ok(Followed::Page(page)) => Json(page).into_response(),
        Ok(Followed::Refused) => refused(id),
        Err(r) => *r,
    }
}

/// What a poll of `/events` gives.
#[derive(Debug)]
enum Followed {
    Page(read::EventPage),
    Refused,
}

/// The events page of the caller `user_id` of `slug`, `until` its removal for a removed
/// account's token.
///
/// A live caller was checked before the read. When its account is gone after the read, the
/// page may hold events of a new account with the same slug, so it is dropped and the caller is
/// answered as a removed account's token: up to its removal, or refused when there is none.
/// This is sound because the removal record is appended before the account folder goes, so a
/// line of a later account can only exist once the caller's account is gone.
///
/// A page that read the caller's own `account.removed` asks under the store lock: the record
/// may be of a removal that is still running (the folder goes later, under the same lock), and
/// a live answer then would move the caller past its removal without it. Still live after the
/// lock means a crash left the record, and the page stays as it is.
fn follow(
    state: &AppState,
    slug: &str,
    user_id: u64,
    until: Option<u64>,
    from: u64,
    limit: usize,
) -> crate::Result<Followed> {
    let floor = state.rest_tokens.floor(slug, user_id, until)?;
    let read::OwnPage { page, own_removal } =
        read::events_of(&state.data_dir, slug, user_id, from, limit, floor, until)?;
    if until.is_some() {
        return Ok(Followed::Page(page));
    }
    let live = if own_removal {
        state.rest_tokens.is_live_settled(slug, user_id)?
    } else {
        state.rest_tokens.is_live(slug, user_id)?
    };
    if live {
        return Ok(Followed::Page(page));
    }
    let Some(until) = state.rest_tokens.removal_now(slug, user_id)? else {
        return Ok(Followed::Refused);
    };
    let floor = state.rest_tokens.floor(slug, user_id, Some(until))?;
    let page = read::events(&state.data_dir, slug, from, limit, floor, Some(until))?;
    Ok(Followed::Page(page))
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

    /// Appends a `file.created` of `path` for the account `jordi`.
    fn created(state: &AppState, path: &str) {
        use crate::journal::{Event, EventBody, FileKind, Origin, new_op_id};
        let mut e = [Event::new(
            "jordi",
            Origin::Cli,
            None,
            &new_op_id(),
            EventBody::FileCreated {
                id: 7,
                path: path.to_string(),
                kind: FileKind::Document,
                md5: "0".repeat(32),
                size: 1,
                pages_changed: None,
            },
        )];
        state.journal.append(&mut e).unwrap();
    }

    #[test]
    fn a_live_caller_removed_during_the_poll_gets_only_its_own_events() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::open(crate::Config::default(), dir.path()).unwrap();
        let accounts = state.auth.accounts();
        let old = accounts
            .create("jordi", "j@example.org", "a password")
            .unwrap();
        created(&state, "NOTE/Note/a.txt");
        // The token check found the old account live; then it was removed and the slug made
        // again and written to before the read.
        accounts.remove("jordi", &state.journal).unwrap();
        accounts
            .create("jordi", "j@example.org", "a password")
            .unwrap();
        created(&state, "NOTE/Note/b.txt");
        let Followed::Page(page) = follow(&state, "jordi", old.user_id, None, 0, 1_000).unwrap()
        else {
            panic!("refused");
        };
        let paths: Vec<_> = page.events.iter().filter_map(|e| e.path.clone()).collect();
        assert_eq!(paths, ["NOTE/Note/a.txt"], "{page:?}");
        let last = page.events.last().unwrap();
        assert_eq!(last.r#type, "account.removed");
        assert_eq!(last.user_id, Some(old.user_id));
        assert_eq!(page.next, last.seq + 1);
        assert!(!page.more);
        // A user id that is neither live nor removed is refused.
        assert!(matches!(
            follow(&state, "jordi", 999, None, 0, 1_000).unwrap(),
            Followed::Refused
        ));
    }

    #[test]
    fn a_live_poll_between_the_removal_record_and_the_removal_ends_with_the_removal() {
        use crate::journal::{Event, EventBody, Origin, new_op_id};
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::open(crate::Config::default(), dir.path()).unwrap());
        let accounts = state.auth.accounts();
        let old = accounts
            .create("jordi", "j@example.org", "a password")
            .unwrap();
        created(&state, "NOTE/Note/a.txt");
        // A second thread does what `Accounts::remove` does: it holds the store lock from the
        // record to the rename of the folder. The poll runs between the two.
        let (recorded, wait) = std::sync::mpsc::channel();
        let remover = {
            let state = state.clone();
            let account_dir = accounts.account_dir("jordi");
            let user_id = old.user_id;
            std::thread::spawn(move || {
                let store = crate::store::take_store_lock(&account_dir, None).unwrap();
                let mut record = [Event::new(
                    "jordi",
                    Origin::Cli,
                    None,
                    &new_op_id(),
                    EventBody::AccountRemoved { user_id },
                )];
                state.journal.append(&mut record).unwrap();
                recorded.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
                let tomb = account_dir.with_file_name(".removed-jordi-test");
                std::fs::rename(&account_dir, &tomb).unwrap();
                drop(store);
                std::fs::remove_dir_all(&tomb).unwrap();
            })
        };
        wait.recv().unwrap();
        let Followed::Page(first) = follow(&state, "jordi", old.user_id, None, 0, 1_000).unwrap()
        else {
            panic!("refused");
        };
        remover.join().unwrap();
        let mut events = first.events.clone();
        let mut gap = first.gap;
        if events.last().is_none_or(|e| e.r#type != "account.removed") {
            // The next poll comes with a removed account's token.
            let until = state
                .rest_tokens
                .removal_now("jordi", old.user_id)
                .unwrap()
                .unwrap();
            let Followed::Page(second) =
                follow(&state, "jordi", old.user_id, Some(until), first.next, 1_000).unwrap()
            else {
                panic!("refused");
            };
            events.extend(second.events);
            gap |= second.gap;
        }
        let removed = events.iter().any(|e| e.r#type == "account.removed");
        assert!(removed || gap, "{first:?} {events:?}");
        assert_eq!(
            events.last().unwrap().r#type,
            "account.removed",
            "{events:?}"
        );
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
