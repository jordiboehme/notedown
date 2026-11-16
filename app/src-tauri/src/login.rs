//! Log in with an e-mail and a password: the server answers a write token, which goes into
//! the Keychain under the account's key. The password never leaves this module but as the
//! hash of the challenge, and no type that holds it or the token derives `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::commands::ConfigState;
use crate::config::AppConfig;
use crate::poller::rest_base;
use crate::rest::{Rest, RestError};
use crate::serve_info::ServeInfo;
use crate::token::{Token, TokenStore, token_key};

/// How the token is listed on the server.
pub const CLIENT_NAME: &str = "Notedown for Mac";
/// What a login without a mode answers.
pub const NO_MODE: &str = "Choose where the server runs first.";

/// The password as the settings window sent it. It has no `Debug` and no `Display`.
pub struct Password(String);

impl Password {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Why a login failed. None of them holds the password, its hashes or the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    /// No mode, so no server to log in to.
    NoMode,
    /// The e-mail or the password is empty.
    Missing,
    /// The server refused, did not answer, or answered something else.
    Rest(RestError),
    /// The Keychain did not take the token.
    Keychain(String),
    /// The config was not saved; the token was deleted again.
    Save(String),
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMode => f.write_str(NO_MODE),
            Self::Missing => f.write_str("Enter your e-mail and password."),
            Self::Rest(error) => error.fmt(f),
            Self::Keychain(why) => write!(f, "The Keychain did not take the login ({why})."),
            Self::Save(why) => write!(f, "The login was not kept: {why}."),
        }
    }
}

impl std::error::Error for LoginError {}

impl From<RestError> for LoginError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

/// `sha256_hex(md5_hex(password) + code)`, lowercase, as the server and the device compute it.
pub fn password_hash(password: &Password, code: &str) -> String {
    use md5::{Digest as _, Md5};
    use sha2::Sha256;
    let mut inner = hex(&Md5::digest(password.0.as_bytes()));
    inner.push_str(code);
    hex(&Sha256::digest(inner.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[derive(Serialize)]
struct CodeBody<'a> {
    email: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeAnswer {
    random_code: String,
}

/// The login body: exactly these fields, the server refuses any other. No `Debug`: it holds
/// the hash.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginBody<'a> {
    email: &'a str,
    password_hash: String,
    name: &'a str,
    scope: &'a str,
}

/// No `Debug`: it holds the token.
#[derive(Deserialize)]
struct LoginAnswer {
    token: String,
    slug: String,
}

/// Logs in to the server of the config and keeps a write token for the account it answers.
/// The token is written first, under the key of the answer's `slug`; then the config gets
/// the account and the e-mail. When that save fails, the token is deleted again. The token of
/// another account that the config named before is deleted last (that is a log out of it). A
/// login is never tried a second time by itself: six wrong tries lock the account.
pub fn log_in(
    state: &ConfigState,
    serve_info: &ServeInfo,
    store: &dyn TokenStore,
    email: &str,
    password: &Password,
) -> Result<AppConfig, LoginError> {
    let before = state.current();
    let base = rest_base(&before, serve_info).ok_or(LoginError::NoMode)?;
    let email = email.trim();
    if email.is_empty() || password.0.is_empty() {
        return Err(LoginError::Missing);
    }
    let rest = Rest::new(&base, None)?;
    let code: CodeAnswer = rest.post_json_once("/login/code", &CodeBody { email })?;
    let body = LoginBody {
        email,
        password_hash: password_hash(password, &code.random_code),
        name: CLIENT_NAME,
        scope: "write",
    };
    let answer: LoginAnswer = rest.post_json_once("/login", &body)?;
    if answer.token.is_empty() || answer.slug.is_empty() {
        return Err(RestError::Garbled("no token or no account in the answer".to_owned()).into());
    }

    let mut next = before.clone();
    next.account = Some(answer.slug.clone());
    let key = token_key(&next).ok_or(LoginError::NoMode)?;
    store
        .set(&key, &Token::new(answer.token))
        .map_err(|e| LoginError::Keychain(e.0))?;
    let saved = match state.set_login(&answer.slug, email) {
        Ok(saved) => saved,
        Err(why) => {
            // Without the account in the config, nothing would find the token again.
            let _ = store.delete(&key);
            return Err(LoginError::Save(why));
        }
    };
    if let Some(old) = token_key(&before)
        && old != key
    {
        let _ = store.delete(&old);
    }
    Ok(saved)
}

#[cfg(test)]
mod tests;
