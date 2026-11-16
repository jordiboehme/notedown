//! Log in with an e-mail and a password: the server answers a write token, which goes into
//! the Keychain under the account's key. The password never leaves this module but as the
//! hash of the challenge, and no type that holds it or the token derives `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::commands::{ConfigAnswer, ConfigState, TryUpdateError};
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
    /// Another login runs.
    Busy,
    /// The mode, the address or the folder changed while the login ran.
    ServerChanged,
    /// The server answered a token that may not write.
    NoWriteAccess,
    /// A failed login whose new token could not be deleted again.
    Rollback {
        error: Box<LoginError>,
        keychain: String,
    },
}

/// One login at a time, and no log out during a login.
#[derive(Default)]
pub struct LoginGate(std::sync::Mutex<()>);

impl LoginGate {
    /// Waits until no login runs and keeps the next one out while the guard lives.
    pub fn hold(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether the Keychain holds a token for the account of `config`.
pub fn has_login(store: &dyn TokenStore, config: &AppConfig) -> Result<bool, String> {
    match token_key(config) {
        Some(key) => Ok(store.get(&key).map_err(|e| e.to_string())?.is_some()),
        None => Ok(false),
    }
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMode => f.write_str(NO_MODE),
            Self::Missing => f.write_str("Enter your e-mail and password."),
            Self::Rest(error) => error.fmt(f),
            Self::Keychain(why) => write!(f, "The Keychain did not take the login ({why})."),
            Self::Save(why) => write!(f, "The login was not kept: {why}."),
            Self::Busy => f.write_str("A login is already running."),
            Self::ServerChanged => {
                f.write_str("The server changed during the login. Log in again.")
            }
            Self::NoWriteAccess => f.write_str("The server gave this Mac no write access."),
            Self::Rollback { error, keychain } => write!(
                f,
                "{error} The new login could not be removed from the Keychain ({keychain})."
            ),
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
    scope: String,
}

/// The sentence for a token of an old login that the Keychain did not let go.
pub fn old_login_kept(why: &str) -> String {
    format!("The old login could not be removed from the Keychain ({why}).")
}

/// Takes the new token back after a failed step: the token that was there before (a new login
/// of the same account) comes back, else the key is deleted. A failure is named.
fn undo(
    store: &dyn TokenStore,
    key: &str,
    previous: Option<&Token>,
    error: LoginError,
) -> LoginError {
    let undone = match previous {
        Some(token) => store.set(key, token),
        None => store.delete(key),
    };
    match undone {
        Ok(()) => error,
        Err(e) => LoginError::Rollback {
            error: Box::new(error),
            keychain: e.0,
        },
    }
}

/// Logs in to the server of the config and keeps a write token for the account it answers.
/// The token is written first, under the key of the answer's `slug`; then the config gets
/// the account and the e-mail. When that save fails, the token is deleted again. The token of
/// another account that the config named before is deleted last (that is a log out of it), so
/// a failed login keeps the account that works. When the mode, the address or the folder
/// changed while the login ran, nothing is kept. One login runs at a time, and a login is
/// never tried a second time by itself: six wrong tries lock the account.
pub fn log_in(
    gate: &LoginGate,
    state: &ConfigState,
    serve_info: &ServeInfo,
    store: &dyn TokenStore,
    email: &str,
    password: &Password,
) -> Result<ConfigAnswer, LoginError> {
    let _running = match gate.0.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return Err(LoginError::Busy),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
    };
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
    if answer.scope != "write" {
        return Err(LoginError::NoWriteAccess);
    }

    let mut next = before.clone();
    next.account = Some(answer.slug.clone());
    let key = token_key(&next).ok_or(LoginError::NoMode)?;
    let previous = store.get(&key).map_err(|e| LoginError::Keychain(e.0))?;
    store
        .set(&key, &Token::new(answer.token))
        .map_err(|e| LoginError::Keychain(e.0))?;
    // Checked under the config's lock: the server must still be the one logged in to (in
    // Local mode the folder is the server, though the key leaves it out), and the key the one
    // of the config, or Log out could never find the token.
    let saved = state.try_update(|c| {
        let mut probe = c.clone();
        probe.account = Some(answer.slug.clone());
        let same_server = (c.mode, &c.server_url, &c.data_dir)
            == (before.mode, &before.server_url, &before.data_dir);
        if !same_server || token_key(&probe).as_deref() != Some(key.as_str()) {
            return Err(LoginError::ServerChanged);
        }
        c.account = Some(answer.slug.clone());
        c.email = Some(email.to_owned());
        Ok(())
    });
    let (replaced, saved) = match saved {
        Ok(both) => both,
        Err(TryUpdateError::Refused(error)) => {
            return Err(undo(store, &key, previous.as_ref(), error));
        }
        // Without the account in the config, nothing would find the token again.
        Err(TryUpdateError::NotSaved(why)) => {
            return Err(undo(store, &key, previous.as_ref(), LoginError::Save(why)));
        }
    };
    let mut keychain = None;
    if let Some(old) = token_key(&replaced)
        && old != key
        && let Err(e) = store.delete(&old)
    {
        keychain = Some(old_login_kept(&e.0));
    }
    Ok(ConfigAnswer {
        config: saved,
        keychain,
    })
}

#[cfg(test)]
mod tests;
