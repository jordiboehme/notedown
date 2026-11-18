//! [`RestSource`]: the server's data over its REST API (`notedown serve` on another machine).
//!
//! Each account has its own token in `<local>/tokens/<slug>.token` (written by
//! `notedown connect`). The folder is listed again in every [`ServerData::accounts`] call and a
//! changed file is read again, so a new login needs no restart.
//!
//! Every failure is "nothing known" (`Err`): a network error, a timeout, a half download, an
//! answer that is not the API's own (the device API answers 200 to anything), a refused token
//! and a token of another account. Only the API's own JSON answers give `Missing`, `NotAFile`
//! or "no record". A token never appears in an error or a message.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use notedown_cloud::appdata::Digest;
use notedown_cloud::auth::{token_slug, validate_slug};
use notedown_cloud::pages::valid_key;
use notedown_cloud::rest::PREFIX;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::records::{Record, Records};
use crate::source::{JournalBatch, NoteBytes, ServerAccount, ServerData};
use crate::worker::{Clock, SystemClock};
use crate::{Error, Result};

/// The time to open a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The time for a JSON call, for the answer and again for its body.
const JSON_TIMEOUT: Duration = Duration::from_secs(15);
/// The time for each read of a note's bytes (not for the whole download).
const NOTE_TIMEOUT: Duration = Duration::from_secs(120);
/// A refused token is asked again after this long (ms), or at once when its file changes.
const REPROBE_MS: u64 = 60_000;
/// The header of a note answer: `found`, `missing` or `not_a_file`.
const NOTE_HEADER: &str = "x-notedown-note";
/// The file name ending of a token file.
const TOKEN_EXT: &str = ".token";

/// What the source knows about one token file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Not asked yet: the server is asked (`GET /account`) before the first use.
    Unchecked,
    /// The server confirmed the account.
    Live,
    /// The server refused the token (401) at this time (ms).
    Refused(u64),
    /// The token's account was removed (410); only its journal can still be read.
    Removed,
    /// The token is that of another account (`Some(slug)`), or no token at all (`None`).
    Elsewhere(Option<String>),
}

/// The size and the modification time of a token file: a change reads it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

struct Entry {
    stamp: Stamp,
    token: String,
    state: State,
}

/// The server's data over its REST API.
pub struct RestSource {
    /// Without a trailing `/`.
    url: String,
    tokens: PathBuf,
    local: PathBuf,
    json: Client,
    notes: Client,
    clock: Arc<dyn Clock>,
    entries: Mutex<BTreeMap<String, Entry>>,
}

/// The JSON error body of the API.
#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

/// Percent-encodes `text` for a query value or a path segment: everything but the unreserved
/// characters (`A-Z a-z 0-9 - . _ ~`), so a `+` is `%2B` and a space `%20` (the server reads
/// the query as a form, where a bare `+` is a space).
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An error and its causes in one line.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let part = s.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = s.source();
    }
    text
}

impl RestSource {
    /// The server at `url` (`http://host:port`); `tokens`: the folder of `<slug>.token` files;
    /// `local`: the pipeline's local folder, for messages.
    pub fn new(url: &str, tokens: &Path, local: &Path) -> Result<RestSource> {
        let url = url.trim().trim_end_matches('/').to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(Error::Invalid {
                what: "server url",
                message: format!("{url:?} does not start with http:// or https://"),
            });
        }
        let client = |timeout: Duration| {
            Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| Error::Invalid {
                    what: "http client",
                    message: chain(&e),
                })
        };
        Ok(RestSource {
            json: client(JSON_TIMEOUT)?,
            notes: client(NOTE_TIMEOUT)?,
            url,
            tokens: tokens.to_path_buf(),
            local: local.to_path_buf(),
            clock: Arc::new(SystemClock),
            entries: Mutex::default(),
        })
    }

    /// Uses `clock` for the time between two probes of a refused token.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> RestSource {
        self.clock = clock;
        self
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token_file(&self, slug: &str) -> PathBuf {
        self.tokens.join(format!("{slug}{TOKEN_EXT}"))
    }

    fn connect_again(&self) -> String {
        format!(
            "run notedown connect {} --data {} again",
            self.url,
            self.local.display()
        )
    }

    /// The message of a token that cannot be used now; `None` for a usable one.
    fn refusal(&self, slug: &str, state: &State) -> Option<String> {
        let file = self.token_file(slug);
        match state {
            State::Refused(_) => Some(format!(
                "the server refused the token of {slug}; {}",
                self.connect_again()
            )),
            State::Elsewhere(Some(other)) => Some(format!(
                "{} holds a token of {other}; run notedown connect again",
                file.display()
            )),
            State::Elsewhere(None) => Some(format!(
                "{} holds no notedown token; {}",
                file.display(),
                self.connect_again()
            )),
            State::Unchecked | State::Live | State::Removed => None,
        }
    }

    /// The token of `slug` and what is known about it; the file is read again when it changed.
    fn load(&self, slug: &str) -> Result<(String, State)> {
        validate_slug(slug).map_err(|_| Error::Read(format!("{slug:?} is no account name")))?;
        let path = self.token_file(slug);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.entries().remove(slug);
                return Err(Error::Read(format!(
                    "no token for account {slug} in {}; run notedown connect {} --data {}",
                    self.tokens.display(),
                    self.url,
                    self.local.display()
                )));
            }
            Err(source) => return Err(Error::Io { path, source }),
        };
        let stamp = Stamp {
            modified: meta.modified().ok(),
            len: meta.len(),
        };
        if let Some(e) = self.entries().get(slug).filter(|e| e.stamp == stamp) {
            return Ok((e.token.clone(), e.state.clone()));
        }
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let token = text.trim().to_string();
        let state = match token_slug(&token) {
            Some(s) if s == slug => State::Unchecked,
            other => State::Elsewhere(other),
        };
        self.entries().insert(
            slug.to_string(),
            Entry {
                stamp,
                token: token.clone(),
                state: state.clone(),
            },
        );
        Ok((token, state))
    }

    /// Sets the state of `slug`, unless its file was read again since `token` was taken.
    fn set_state(&self, slug: &str, token: &str, state: State) {
        if let Some(e) = self.entries().get_mut(slug).filter(|e| e.token == token) {
            e.state = state;
        }
    }

    /// The token to read account `slug` with. A token not yet confirmed is asked about first,
    /// so no request ever reads another account's data. `journal`: a removed account's token
    /// still reads its journal (up to the removal).
    fn token_for(&self, slug: &str, journal: bool) -> Result<String> {
        let (token, mut state) = self.load(slug)?;
        if state == State::Unchecked {
            self.probe(slug, &token)?;
            state = self.load(slug)?.1;
        }
        if let Some(message) = self.refusal(slug, &state) {
            return Err(match state {
                State::Refused(_) => Error::TokenRefused(message),
                _ => Error::Read(message),
            });
        }
        match state {
            State::Removed if !journal => Err(Error::Read(format!(
                "account {slug} was removed on the server"
            ))),
            State::Unchecked => Err(Error::Read(format!(
                "the token of {slug} changed while it was checked"
            ))),
            _ => Ok(token),
        }
    }

    /// Asks the server whose token this is (`GET /account`) and notes the answer. `Ok(None)`
    /// for a refused or removed token and one of another account; `Err` when nothing is known.
    fn probe(&self, slug: &str, token: &str) -> Result<Option<ServerAccount>> {
        let path = "/account";
        let resp = self.send(&self.json, token, path, false)?;
        let status = resp.status();
        let body = self.body(path, resp)?;
        if status != StatusCode::OK {
            let (e, noted) = self.failure(slug, token, path, status, &body);
            return if noted { Ok(None) } else { Err(e) };
        }
        let account: ServerAccount = self.parse(path, &body)?;
        if account.slug != slug {
            self.set_state(slug, token, State::Elsewhere(Some(account.slug)));
            return Ok(None);
        }
        self.set_state(slug, token, State::Live);
        Ok(Some(account))
    }

    fn send(&self, client: &Client, token: &str, path: &str, head: bool) -> Result<Response> {
        let url = format!("{}{PREFIX}{path}", self.url);
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            Error::Read(format!(
                "{url}: the token file holds characters a token never has; run notedown \
                 connect again"
            ))
        })?;
        auth.set_sensitive(true);
        let request = if head {
            client.head(&url)
        } else {
            client.get(&url)
        };
        request
            .header(AUTHORIZATION, auth)
            .send()
            .map_err(|_| self.no_answer(path))
    }

    /// A transport error (no connection, a timeout, a broken answer) in one steady text per
    /// route, so a network that comes and goes gives one message, not one per poll.
    fn no_answer(&self, path: &str) -> Error {
        let route = path.split('?').next().unwrap_or(path);
        Error::Read(format!(
            "{}{PREFIX}{route}: no answer from the server",
            self.url
        ))
    }

    fn body(&self, path: &str, resp: Response) -> Result<Vec<u8>> {
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|_| self.no_answer(path))
    }

    fn parse<T: DeserializeOwned>(&self, path: &str, body: &[u8]) -> Result<T> {
        serde_json::from_slice(body).map_err(|e| {
            Error::Read(format!(
                "{}{PREFIX}{path}: the answer is not that of a notedown server ({e})",
                self.url
            ))
        })
    }

    /// The error of an answer that is not the one wanted. A 401 `token_refused` and a 410
    /// `account_removed` are noted for the token; the flag tells whether it was one of them.
    fn failure(
        &self,
        slug: &str,
        token: &str,
        path: &str,
        status: StatusCode,
        body: &[u8],
    ) -> (Error, bool) {
        let code = serde_json::from_slice::<ErrorBody>(body)
            .ok()
            .map(|b| b.error);
        match (status.as_u16(), code.as_deref()) {
            (401, Some("token_refused")) => {
                let state = State::Refused(self.clock.now_ms());
                let message = self.refusal(slug, &state).unwrap_or_default();
                self.set_state(slug, token, state);
                (Error::TokenRefused(message), true)
            }
            (410, Some("account_removed")) => {
                self.set_state(slug, token, State::Removed);
                (
                    Error::Read(format!("account {slug} was removed on the server")),
                    true,
                )
            }
            (_, code) => (
                Error::Read(format!(
                    "{}{PREFIX}{path}: the server answered {status}{}",
                    self.url,
                    code.map(|c| format!(" ({c})")).unwrap_or_default()
                )),
                false,
            ),
        }
    }

    /// `GET path` for account `slug` and its JSON answer.
    fn get_json<T: DeserializeOwned>(&self, slug: &str, path: &str, journal: bool) -> Result<T> {
        let token = self.token_for(slug, journal)?;
        let resp = self.send(&self.json, &token, path, false)?;
        let status = resp.status();
        let body = self.body(path, resp)?;
        if status != StatusCode::OK {
            return Err(self.failure(slug, &token, path, status, &body).0);
        }
        self.parse(path, &body)
    }

    /// The value of the note header, if any.
    fn note_header(resp: &Response) -> Option<String> {
        resp.headers()
            .get(NOTE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }
}

impl ServerData for RestSource {
    fn describe(&self) -> String {
        self.url.clone()
    }

    /// The accounts whose token the server accepts now. A refused token, one of another
    /// account and a removed account's token are left out (see [`ServerData::refusals`]); one
    /// account the server cannot answer for is left out too. `Err` when no account answered
    /// and at least one could not be asked, and when the tokens folder cannot be listed.
    fn accounts(&self) -> Result<Vec<ServerAccount>> {
        let list = match std::fs::read_dir(&self.tokens) {
            Ok(list) => list,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Read(format!(
                    "{} is missing; run notedown connect {} --data {}",
                    self.tokens.display(),
                    self.url,
                    self.local.display()
                )));
            }
            Err(source) => {
                return Err(Error::Io {
                    path: self.tokens.clone(),
                    source,
                });
            }
        };
        let mut slugs = Vec::new();
        for entry in list {
            let entry = entry.map_err(|source| Error::Io {
                path: self.tokens.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(slug) = name.to_str().and_then(|n| n.strip_suffix(TOKEN_EXT)) else {
                continue;
            };
            if validate_slug(slug).is_ok() {
                slugs.push(slug.to_string());
            }
        }
        slugs.sort();
        self.entries().retain(|slug, _| slugs.contains(slug));
        let now = self.clock.now_ms();
        let mut out = Vec::new();
        let mut failed = None;
        for slug in &slugs {
            let (token, state) = match self.load(slug) {
                Ok(v) => v,
                Err(e) => {
                    failed.get_or_insert(e);
                    continue;
                }
            };
            match state {
                State::Elsewhere(_) | State::Removed => continue,
                State::Refused(at) if now.saturating_sub(at) < REPROBE_MS => continue,
                _ => {}
            }
            match self.probe(slug, &token) {
                Ok(Some(account)) => out.push(account),
                Ok(None) => {}
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        match failed {
            Some(e) if out.is_empty() => Err(e),
            _ => Ok(out),
        }
    }

    fn records(&self, slug: &str) -> Result<Records> {
        self.get_json(slug, "/records", false)
    }

    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>> {
        let token = self.token_for(slug, false)?;
        if !valid_key(key) {
            return Ok(None);
        }
        let path = format!("/records/{}", encode(key));
        let resp = self.send(&self.json, &token, &path, false)?;
        let status = resp.status();
        let body = self.body(&path, resp)?;
        match status {
            StatusCode::OK => self.parse(&path, &body).map(Some),
            StatusCode::NOT_FOUND
                if serde_json::from_slice::<ErrorBody>(&body)
                    .is_ok_and(|b| b.error == "no_record") =>
            {
                Ok(None)
            }
            _ => Err(self.failure(slug, &token, &path, status, &body).0),
        }
    }

    /// `Found` only for a 200 with the note header and all the bytes `Content-Length`
    /// announced; `Missing` and `NotAFile` only for the API's own JSON answers.
    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes> {
        let token = self.token_for(slug, false)?;
        let route = format!("/note?path={}", encode(path));
        let resp = self.send(&self.notes, &token, &route, false)?;
        let status = resp.status();
        let header = Self::note_header(&resp);
        let at = || format!("{}{PREFIX}{route}", self.url);
        if status == StatusCode::OK && header.as_deref() == Some("found") {
            let Some(size) = resp.content_length() else {
                return Err(Error::Read(format!("{}: no Content-Length", at())));
            };
            let mut bytes = Vec::with_capacity(size.min(64 << 20) as usize);
            resp.take(size.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| Error::Read(format!("{}: {}", at(), chain(&e))))?;
            if bytes.len() as u64 != size {
                return Err(Error::Read(format!(
                    "{}: the note arrived with {} of {size} bytes",
                    at(),
                    bytes.len()
                )));
            }
            return Ok(NoteBytes::Found(bytes));
        }
        let body = self.body(&route, resp)?;
        let code = serde_json::from_slice::<ErrorBody>(&body)
            .ok()
            .map(|b| b.error);
        match (status, header.as_deref(), code.as_deref()) {
            (StatusCode::NOT_FOUND, Some("missing"), Some("missing")) => Ok(NoteBytes::Missing),
            (StatusCode::CONFLICT, Some("not_a_file"), Some("not_a_file")) => {
                Ok(NoteBytes::NotAFile)
            }
            _ => Err(self.failure(slug, &token, &route, status, &body).0),
        }
    }

    /// `HEAD`: the note header decides. `Some(true)` only for a 200 `found`, `Some(false)`
    /// only for a 404 `missing`; any other answer of the API is `None`. A server error and a
    /// refused token are `Err`.
    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>> {
        let token = self.token_for(slug, false)?;
        let route = format!("/note?path={}", encode(path));
        let resp = self.send(&self.json, &token, &route, true)?;
        let status = resp.status();
        match (status, Self::note_header(&resp).as_deref()) {
            (StatusCode::OK, Some("found")) => Ok(Some(true)),
            (StatusCode::NOT_FOUND, Some("missing")) => Ok(Some(false)),
            _ if status.is_server_error() || status.is_redirection() => Err(Error::Read(format!(
                "{}{PREFIX}{route}: the server answered {status}",
                self.url
            ))),
            (StatusCode::UNAUTHORIZED | StatusCode::GONE, _) => Err(Error::Read(format!(
                "{}{PREFIX}{route}: the server answered {status}",
                self.url
            ))),
            _ => Ok(None),
        }
    }

    fn digests(&self, slug: &str) -> Result<Vec<Digest>> {
        self.get_json(slug, "/digests", false)
    }

    /// A removed account's token reads its journal up to the removal.
    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
        self.get_json(slug, &format!("/journal?from={from}&limit={limit}"), true)
    }

    fn refusals(&self) -> Vec<(String, String)> {
        self.entries()
            .iter()
            .filter_map(|(slug, e)| Some((slug.clone(), self.refusal(slug, &e.state)?)))
            .collect()
    }

    fn refused(&self) -> Vec<String> {
        self.entries()
            .iter()
            .filter(|(_, e)| matches!(e.state, State::Refused(_)))
            .map(|(slug, _)| slug.clone())
            .collect()
    }

    /// Only a 410 for the token of `slug` says the account is gone. A refused token, a token
    /// of another account, a failed request and a missing token file know nothing.
    fn knows_gone(&self, slug: &str) -> bool {
        self.entries()
            .get(slug)
            .is_some_and(|e| e.state == State::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unreserved_characters_stay() {
        assert_eq!(
            encode("NOTE/Note/a+b c.note"),
            "NOTE%2FNote%2Fa%2Bb%20c.note"
        );
        assert_eq!(encode("&=#%?"), "%26%3D%23%25%3F");
        assert_eq!(encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(encode("path:ab"), "path%3Aab");
        assert_eq!(encode("ä"), "%C3%A4");
    }

    #[test]
    fn the_url_must_be_http_and_loses_its_trailing_slash() {
        let dir = Path::new("/nowhere");
        assert!(RestSource::new("pi.local:19072", dir, dir).is_err());
        let src = RestSource::new("http://pi.local:19072/", dir, dir).unwrap();
        assert_eq!(src.describe(), "http://pi.local:19072");
    }
}
