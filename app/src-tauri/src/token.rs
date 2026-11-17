//! The REST token: only in the Keychain, never in a file, a log or a `Debug` line.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::config::{AppConfig, Mode};

/// A login token. Its `Debug` output hides the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value, for the `Authorization: Bearer` header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// A store failure. It never carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError(pub String);

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the token store failed: {}", self.0)
    }
}

impl std::error::Error for TokenError {}

pub trait TokenStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Token>, TokenError>;
    fn set(&self, key: &str, token: &Token) -> Result<(), TokenError>;
    /// Deleting a key that is not there is fine.
    fn delete(&self, key: &str) -> Result<(), TokenError>;
}

/// The Keychain key of the token: `local|<slug>` in Local mode (the port or the address can
/// change, so no URL), `<server_url>|<slug>` in Remote mode. `None` without a mode or an account.
pub fn token_key(config: &AppConfig) -> Option<String> {
    let slug = config.account.as_deref().filter(|s| !s.is_empty())?;
    match config.mode {
        Mode::NotSet => None,
        Mode::Local => Some(format!("local|{slug}")),
        Mode::Remote => {
            let url = config.server_url.as_deref().filter(|s| !s.is_empty())?;
            // Set refuses a login in the address; one in an older config stays out of the key.
            Some(format!("{}|{slug}", crate::config::without_userinfo(url)))
        }
    }
}

/// The macOS Keychain, under the app's bundle identifier.
pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    pub fn new() -> Self {
        Self::with_service(crate::identifier())
    }

    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_owned(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, TokenError> {
        keyring::Entry::new(&self.service, key).map_err(|e| TokenError(e.to_string()))
    }
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenStore for KeychainStore {
    fn get(&self, key: &str) -> Result<Option<Token>, TokenError> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(Token::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(TokenError(e.to_string())),
        }
    }

    fn set(&self, key: &str, token: &Token) -> Result<(), TokenError> {
        self.entry(key)?
            .set_password(token.expose())
            .map_err(|e| TokenError(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), TokenError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(TokenError(e.to_string())),
        }
    }
}

/// A store in memory, for tests and for runs without a Keychain.
#[derive(Default)]
pub struct MemoryStore {
    tokens: Mutex<HashMap<String, Token>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Token>, TokenError> {
        Ok(self.tokens.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, token: &Token) -> Result<(), TokenError> {
        self.tokens
            .lock()
            .unwrap()
            .insert(key.to_owned(), token.clone());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), TokenError> {
        self.tokens.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: Mode, url: Option<&str>, account: Option<&str>) -> AppConfig {
        AppConfig {
            mode,
            server_url: url.map(str::to_owned),
            account: account.map(str::to_owned),
            ..AppConfig::default()
        }
    }

    #[test]
    fn token_keys_for_local_and_remote() {
        let local = config(Mode::Local, None, Some("jordi"));
        assert_eq!(token_key(&local).as_deref(), Some("local|jordi"));
        // The URL plays no part in Local mode.
        let local = config(Mode::Local, Some("http://192.168.1.5:19072"), Some("jordi"));
        assert_eq!(token_key(&local).as_deref(), Some("local|jordi"));

        let remote = config(
            Mode::Remote,
            Some("https://notes.example.org"),
            Some("jordi"),
        );
        assert_eq!(
            token_key(&remote).as_deref(),
            Some("https://notes.example.org|jordi")
        );

        // A login in the address never goes into the key.
        let with_login = config(
            Mode::Remote,
            Some("https://user:secret@notes.example.org"),
            Some("jordi"),
        );
        assert_eq!(
            token_key(&with_login).as_deref(),
            Some("https://notes.example.org|jordi")
        );

        assert_eq!(token_key(&config(Mode::NotSet, None, Some("jordi"))), None);
        assert_eq!(token_key(&config(Mode::Local, None, None)), None);
        assert_eq!(token_key(&config(Mode::Remote, None, Some("jordi"))), None);
    }

    #[test]
    fn the_setup_checklist_makes_the_same_key() {
        // checklist.ts tokenKey tells the sync mark of the login from an older one; its test
        // expects these keys.
        let test = include_str!("../../src/tests/checklist.test.ts");
        let remote = config(
            Mode::Remote,
            Some("https://user:secret@notes.example.org/x"),
            Some("me"),
        );
        for key in [
            token_key(&config(Mode::Local, None, Some("me"))).unwrap(),
            token_key(&remote).unwrap(),
        ] {
            assert!(test.contains(&format!("\"{key}\"")), "{key}");
        }
    }

    #[test]
    fn memory_token_store_sets_gets_and_deletes() {
        let store = MemoryStore::new();
        assert_eq!(store.get("local|jordi").unwrap(), None);
        store.set("local|jordi", &Token::new("secret")).unwrap();
        let got = store.get("local|jordi").unwrap().unwrap();
        assert_eq!(got.expose(), "secret");
        store.delete("local|jordi").unwrap();
        assert_eq!(store.get("local|jordi").unwrap(), None);
        // Deleting what is not there is fine.
        store.delete("local|jordi").unwrap();
    }

    #[test]
    fn the_token_never_prints() {
        let token = Token::new("super-secret-value");
        assert_eq!(format!("{token:?}"), "***");
        assert_eq!(format!("{:?}", Some(&token)), "Some(***)");
        assert!(!format!("{token:#?}").contains("super-secret"));
    }

    /// Run by hand on a Mac: `cargo test -- --ignored keychain_store_round_trips`. It writes one
    /// entry under a test service and removes it again.
    #[test]
    #[ignore = "writes to the real Keychain"]
    fn keychain_store_round_trips() {
        let store = KeychainStore::with_service("io.github.jordiboehme.notedown.test");
        let key = "local|round-trip-test";
        store.delete(key).unwrap();
        assert_eq!(store.get(key).unwrap(), None);
        store.set(key, &Token::new("keychain-secret")).unwrap();
        assert_eq!(store.get(key).unwrap().unwrap().expose(), "keychain-secret");
        store.delete(key).unwrap();
        assert_eq!(store.get(key).unwrap(), None);
    }
}
