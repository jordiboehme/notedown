//! The REST client of the app: JSON calls to the Notedown server, for the login and the
//! library. All calls are blocking, so they run on threads of their own, never on the main
//! thread. The token goes only into the `Authorization` header, and no error holds it, the
//! address or a body.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, IF_MATCH, IF_NONE_MATCH, IF_RANGE,
    RANGE, RETRY_AFTER as RETRY_AFTER_HEADER,
};
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::supervisor::Clock;
use crate::token::Token;
use crate::transfer::FileBody;

/// How long a connection may take to open.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a JSON call may take as a whole.
pub const JSON_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a call waits in all for a Supernote that syncs (423 `device_syncing`).
pub const SYNC_WAIT: Duration = Duration::from_secs(120);
/// The wait when a 423 `device_syncing` has no usable `Retry-After`.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Sleeps for a while. Injected, so tests do not really wait.
pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

/// Milliseconds since the first call, from a monotonic clock: a wall clock set back or
/// forward never makes a wait longer or shorter.
pub fn monotonic_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX))
}

/// The real sleep.
pub fn real_sleeper() -> Sleeper {
    Arc::new(std::thread::sleep)
}

/// Why a call failed, by the server's error code. The texts are shown as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestError {
    /// No connection, no answer in time, or a proxy in front of a server that is down.
    Unreachable,
    /// 401 `token_refused`: the stored token is not good anymore.
    TokenRefused,
    /// 401 `login_failed`: a wrong e-mail, password or code, or a locked account.
    LoginFailed,
    /// 429 `too_many`.
    TooMany,
    /// 423 `device_syncing`, with the wait the server asks for.
    Syncing { retry_after: Duration },
    /// 423 `scan_hold`: the server is reading the account's files.
    ScanHold,
    /// 423 `not_imported`: the Supernote was not imported yet.
    NotImported,
    /// 403 `not_writable`.
    NotWritable,
    /// 403 `read_only_token`.
    ReadOnlyToken,
    /// 400 `bad_name` or `name_too_long`.
    BadName,
    /// 409 `name_taken`: `existing` is the name in the way.
    NameTaken { existing: String },
    /// 404 `not_found`.
    NotFound,
    /// 412 `precondition_failed`.
    PreconditionFailed,
    /// 413 `too_large`.
    TooLarge,
    /// 507 `quota_exceeded`.
    Full,
    /// 410 `account_removed`.
    AccountRemoved,
    /// 503 `store_damaged`.
    StoreDamaged,
    /// Any other answer: its status and its error code, if it had one.
    Server { status: u16, code: Option<String> },
    /// A success answer that could not be read. The detail names no value of the body.
    Garbled(String),
    /// The request failed on the way, without the address in the detail.
    Failed(String),
    /// An upload got no answer: the connection broke while the body was sent or before the
    /// answer came.
    Cut,
    /// A transfer whose bytes stood still for a minute was ended.
    Stalled,
}

impl fmt::Display for RestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable => f.write_str("The server does not answer."),
            Self::TokenRefused => f.write_str("The server refused the login. Log in again."),
            Self::LoginFailed => {
                f.write_str("Wrong e-mail or password, or the account is locked for now.")
            }
            Self::TooMany => f.write_str("Too many tries. Wait a minute."),
            Self::Syncing { .. } => f.write_str("The Supernote is syncing. Try again in a moment."),
            Self::ScanHold => f.write_str(
                "The server is reading the files of the Supernote. Try again in a moment.",
            ),
            Self::NotImported => f.write_str("The Supernote has not been imported yet."),
            Self::NotWritable => f.write_str("This folder cannot be changed."),
            Self::ReadOnlyToken => {
                f.write_str("The login of this Mac may not change files. Log in again.")
            }
            Self::BadName => f.write_str("This name is not allowed."),
            Self::NameTaken { existing } => write!(f, "There is already {existing} here."),
            Self::NotFound => f.write_str("This is not there anymore."),
            Self::PreconditionFailed => f.write_str("The file was changed on the server."),
            Self::TooLarge => f.write_str("Files up to 1 GB."),
            Self::Full => f.write_str("The account is full."),
            Self::AccountRemoved => f.write_str("The account was removed from the server."),
            Self::StoreDamaged => {
                f.write_str("The server cannot read its files. See the server's log.")
            }
            Self::Server { status, code } => match code.as_deref() {
                Some(code) => match code_text(code) {
                    Some(text) => f.write_str(text),
                    None => write!(f, "The server answered {status} ({code})."),
                },
                None => write!(f, "The server answered {status}."),
            },
            Self::Garbled(why) => write!(f, "The answer of the server could not be read ({why})."),
            Self::Failed(why) => write!(f, "The request failed ({why})."),
            Self::Cut => f.write_str("The connection was cut."),
            Self::Stalled => f.write_str("The connection stalled."),
        }
    }
}

impl std::error::Error for RestError {}

/// The text of an error code that has no variant of its own.
fn code_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "is_a_folder" => "This is a folder, not a file.",
        "not_a_folder" => "A file is where a folder should be.",
        "not_a_file" => "This is not a file.",
        "into_itself" => "A folder cannot move into itself.",
        "sync_refused" => "The server takes no files now.",
        "slot_damaged" => "This item of the recycle bin is damaged.",
        "bad_length" | "bad_md5" => "The file did not arrive whole. Try again.",
        "length_required" => "The server needs the size of the file.",
        "bad_cursor" => "The list changed on the server. Load it again.",
        "missing" => "The note is not on the server.",
        "no_record" => "The server has nothing for this note.",
        "no_route" => "The server does not know this request. Update the server.",
        "bad_request" => "The server did not understand the request.",
        "internal" => "The server had an error. See the server's log.",
        _ => return None,
    })
}

/// The error of an answer that was no success: by its code, then by its status.
pub fn error_for(status: u16, body: &[u8], retry_after: Option<&str>) -> RestError {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: Option<String>,
        existing: Option<String>,
    }
    let parsed = serde_json::from_slice::<ErrorBody>(body).ok();
    let code = parsed.as_ref().and_then(|b| b.error.clone());
    match (status, code.as_deref()) {
        (401, Some("login_failed")) => RestError::LoginFailed,
        (401, _) => RestError::TokenRefused,
        (429, _) => RestError::TooMany,
        (423, Some("device_syncing")) => RestError::Syncing {
            retry_after: retry_after
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(RETRY_AFTER, Duration::from_secs),
        },
        (423, Some("scan_hold")) => RestError::ScanHold,
        (423, Some("not_imported")) => RestError::NotImported,
        (403, Some("not_writable")) => RestError::NotWritable,
        (403, Some("read_only_token")) => RestError::ReadOnlyToken,
        (400, Some("bad_name" | "name_too_long")) => RestError::BadName,
        (409, Some("name_taken")) => RestError::NameTaken {
            existing: parsed.and_then(|b| b.existing).unwrap_or_default(),
        },
        (404, Some("not_found")) => RestError::NotFound,
        (412, _) => RestError::PreconditionFailed,
        (413, _) => RestError::TooLarge,
        (507, _) => RestError::Full,
        (410, _) => RestError::AccountRemoved,
        (503, Some("store_damaged")) => RestError::StoreDamaged,
        // A proxy in front of a server that is down: no Notedown error in the answer.
        (502..=504, None) => RestError::Unreachable,
        (status, code) => RestError::Server {
            status,
            code: code.map(str::to_owned),
        },
    }
}

/// The precondition of an upload: a new file only, or a replace of the file with this md5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    Create,
    Replace(String),
}

/// The answer of `PUT /files?path=`, as far as the app uses it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Put {
    pub path: String,
    /// `created`, `replaced` or `unchanged`.
    pub outcome: String,
    #[serde(default)]
    pub seqs: Vec<u64>,
}

/// A client for one server and one token (or none, for the login).
pub struct Rest {
    base: String,
    token: Option<Token>,
    client: reqwest::blocking::Client,
    clock: Clock,
    sleeper: Sleeper,
}

impl Rest {
    /// A client with the wall clock and real sleeps. Make it on a thread that may block, never
    /// inside an async task.
    pub fn new(base: &str, token: Option<Token>) -> Result<Self, RestError> {
        Self::with_timing(base, token, monotonic_clock(), real_sleeper())
    }

    /// [`new`](Self::new) with another clock (milliseconds, monotonic) and sleeper.
    pub fn with_timing(
        base: &str,
        token: Option<Token>,
        clock: Clock,
        sleeper: Sleeper,
    ) -> Result<Self, RestError> {
        // No redirect is followed: a Notedown server never sends one, so the token never goes
        // to another address. The app's own server is on this Mac: no system proxy sees it.
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(JSON_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if crate::poller::is_loopback(base) {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| RestError::Failed(format!("no HTTP client: {}", e.without_url())))?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            client,
            clock,
            sleeper,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The URL of `path` (such as `/entries`) under the base, with `query` encoded.
    pub fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, RestError> {
        let mut url = Url::parse(&format!("{}{path}", self.base))
            .map_err(|_| RestError::Failed("the server address is not valid".to_owned()))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// `GET` with a JSON answer.
    pub fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, RestError> {
        let url = self.url(path, query)?;
        self.wait_while_syncing(|| self.send(Method::GET, url.clone(), None))
    }

    /// `POST` of a JSON body with a JSON answer; waits while a Supernote syncs.
    pub fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RestError> {
        let url = self.url(path, &[])?;
        let body = to_body(body)?;
        self.wait_while_syncing(|| self.send(Method::POST, url.clone(), Some(body.clone())))
    }

    /// `POST` once: no wait and no second try (the login).
    pub fn post_json_once<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RestError> {
        let url = self.url(path, &[])?;
        self.send(Method::POST, url, Some(to_body(body)?))
    }

    /// `POST` without a body, once (a change job waits by itself, see
    /// [`wait_while_syncing`](Self::wait_while_syncing)).
    pub fn post_empty_once<T: DeserializeOwned>(&self, path: &str) -> Result<T, RestError> {
        let url = self.url(path, &[])?;
        self.send(Method::POST, url, None)
    }

    /// `DELETE` once, with a JSON answer.
    pub fn delete_json_once<T: DeserializeOwned>(&self, path: &str) -> Result<T, RestError> {
        let url = self.url(path, &[])?;
        self.send(Method::DELETE, url, None)
    }

    /// `DELETE` with a JSON answer; waits while a Supernote syncs.
    pub fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, RestError> {
        let url = self.url(path, &[])?;
        self.wait_while_syncing(|| self.send(Method::DELETE, url.clone(), None))
    }

    /// Runs `call` again while it answers 423 `device_syncing`, each time after the wait the
    /// server asked for, up to [`SYNC_WAIT`] in all. Other answers, 423 `scan_hold` and
    /// `not_imported` too, end it at once.
    pub fn wait_while_syncing<T>(
        &self,
        mut call: impl FnMut() -> Result<T, RestError>,
    ) -> Result<T, RestError> {
        let start = (self.clock)();
        loop {
            match call() {
                Err(RestError::Syncing { retry_after }) => {
                    let waited = Duration::from_millis((self.clock)().saturating_sub(start));
                    let left = SYNC_WAIT.saturating_sub(waited);
                    if left.is_zero() {
                        return Err(RestError::Syncing { retry_after });
                    }
                    // At least a second, so a `Retry-After: 0` does not spin.
                    (self.sleeper)(retry_after.max(Duration::from_secs(1)).min(left));
                }
                other => return other,
            }
        }
    }

    /// The `Authorization` header of this client's token, marked sensitive.
    fn bearer(&self) -> Result<Option<HeaderValue>, RestError> {
        let Some(token) = &self.token else {
            return Ok(None);
        };
        // The value never shows in an error or a log line: only the header carries it.
        let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", token.expose())) else {
            return Err(RestError::Failed(
                "the stored login cannot be sent".to_owned(),
            ));
        };
        value.set_sensitive(true);
        Ok(Some(value))
    }

    /// `PUT /files?path=<path>` with `body` streamed as the bytes of the file, once. Its client
    /// has no total timeout (a large file on a small server takes minutes): the caller watches
    /// the body's counter and drops this future when it stands still (see
    /// [`crate::transfer::watched`]). It runs inside a runtime. A connection that breaks without
    /// an answer is [`RestError::Cut`]: after an early refusal the server may close it while
    /// the body is still sent.
    pub async fn put_file(
        &self,
        path: &str,
        body: FileBody,
        precondition: &Precondition,
    ) -> Result<Put, RestError> {
        let url = self.url("/files", &[("path", path)])?;
        let mut request = self
            .transfer_client()?
            .put(url)
            .header(CONTENT_LENGTH, HeaderValue::from(body.remaining()));
        if let Some(bearer) = self.bearer()? {
            request = request.header(AUTHORIZATION, bearer);
        }
        request = match precondition {
            Precondition::Create => request.header(IF_NONE_MATCH, HeaderValue::from_static("*")),
            Precondition::Replace(md5) => {
                let tag = HeaderValue::from_str(&format!("\"{md5}\""))
                    .map_err(|_| RestError::Garbled("no valid md5".to_owned()))?;
                request.header(IF_MATCH, tag)
            }
        };
        let response = request
            .body(reqwest::Body::wrap(body))
            .send()
            .await
            .map_err(cut)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let answer = response.bytes().await.map_err(cut)?;
        if (200..300).contains(&status) {
            serde_json::from_slice(&answer).map_err(garbled)
        } else {
            Err(error_for(status, &answer, retry_after.as_deref()))
        }
    }

    /// The client of one transfer: no total timeout (the caller watches the bytes), no
    /// redirect followed, and no proxy for the app's own server, as for the JSON calls. Made
    /// per transfer, inside the transfer's runtime.
    fn transfer_client(&self) -> Result<reqwest::Client, RestError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if crate::poller::is_loopback(&self.base) {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .map_err(|e| RestError::Failed(format!("no HTTP client: {}", e.without_url())))
    }

    /// `GET /files/{id}`, once: the answer with its head, 200 or 206, for the caller to read
    /// the body from. With `from` above 0 it asks for the bytes from there on, only while the
    /// file is still the one with `md5` (`Range` and `If-Range`); the server answers 206, or
    /// 200 with the whole file when it changed. Like [`put_file`](Self::put_file) it has no
    /// total timeout and runs inside a runtime: the caller watches the bytes.
    pub async fn get_file(
        &self,
        id: u64,
        from: u64,
        md5: &str,
    ) -> Result<reqwest::Response, RestError> {
        let url = self.url(&format!("/files/{id}"), &[])?;
        let mut request = self.transfer_client()?.get(url);
        if let Some(bearer) = self.bearer()? {
            request = request.header(AUTHORIZATION, bearer);
        }
        if from > 0 {
            let tag = HeaderValue::from_str(&format!("\"{md5}\""))
                .map_err(|_| RestError::Garbled("no valid md5".to_owned()))?;
            request = request
                .header(RANGE, format!("bytes={from}-"))
                .header(IF_RANGE, tag);
        }
        let response = request.send().await.map_err(cut)?;
        let status = response.status().as_u16();
        if status == 200 || status == 206 {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get(RETRY_AFTER_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let answer = response.bytes().await.map_err(cut)?;
        Err(error_for(status, &answer, retry_after.as_deref()))
    }

    /// One request with an optional JSON body.
    fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<T, RestError> {
        let mut request = self.client.request(method, url);
        if let Some(bearer) = self.bearer()? {
            request = request.header(AUTHORIZATION, bearer);
        }
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .body(body);
        }
        let response = request.send().map_err(failed)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().map_err(failed)?;
        if (200..300).contains(&status) {
            serde_json::from_slice(&body).map_err(garbled)
        } else {
            Err(error_for(status, &body, retry_after.as_deref()))
        }
    }
}

/// A request that got no answer: without the address, which could hold a login.
fn failed(e: reqwest::Error) -> RestError {
    if e.is_timeout() || e.is_connect() {
        RestError::Unreachable
    } else {
        RestError::Failed(e.without_url().to_string())
    }
}

/// A transfer that got no answer: no connection is unreachable, a broken one is cut.
fn cut(e: reqwest::Error) -> RestError {
    if e.is_connect() || e.is_timeout() {
        RestError::Unreachable
    } else {
        RestError::Cut
    }
}

/// An answer that is no JSON of the expected shape. serde's message can quote a value of the
/// body, so only its kind and place are kept.
fn garbled(e: serde_json::Error) -> RestError {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "cut off",
        serde_json::error::Category::Syntax => "no JSON",
        serde_json::error::Category::Data => "unexpected content",
        serde_json::error::Category::Eof => "cut off",
    };
    RestError::Garbled(format!("{kind} at line {} column {}", e.line(), e.column()))
}

fn to_body<B: Serialize>(body: &B) -> Result<Vec<u8>, RestError> {
    serde_json::to_vec(body).map_err(|e| RestError::Failed(format!("no request body: {e}")))
}

#[cfg(test)]
pub(crate) mod test_http;

#[cfg(test)]
mod tests;
