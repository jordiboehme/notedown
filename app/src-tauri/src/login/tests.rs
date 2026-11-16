//! The login against a scripted server on loopback, with a store in memory (never the real
//! Keychain) and the config in a temp folder.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::config::{self, Mode};
use crate::probe::test_server::free_port;
use crate::rest::test_http::{Scripted, answer};
use crate::token::{MemoryStore, TokenError};

const PASSWORD: &str = "correct horse 1";
const CODE: &str = "c0de42";
/// Computed with the shell, as docs/rest-api.md shows: `md5 -q` of the password.
const PASSWORD_MD5: &str = "26cddb9f7d28f3d57a5a7c824b1216b7";
/// `shasum -a 256` of PASSWORD_MD5 + CODE.
const HASH: &str = "b65fe840db3dab756a5d0640b6fdaeccef7648cc3f1b4dd0a62470753c23e905";

fn code_answer() -> String {
    answer(
        "200 OK",
        &format!(r#"{{"randomCode":"{CODE}","timestamp":1763300000000}}"#),
    )
}

fn token_answer(slug: &str) -> String {
    answer(
        "201 Created",
        &format!(r#"{{"token":"tok-123","id":"t1","slug":"{slug}","userId":7,"scope":"write"}}"#),
    )
}

/// The server address of the config: the scripted server without `/rest/v1`.
fn server_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// A config in Remote mode for `port`, saved in `dir`.
fn remote_config(dir: &Path, port: u16, account: Option<&str>) -> ConfigState {
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(port)),
        account: account.map(str::to_owned),
        ..AppConfig::default()
    };
    config::save(dir, &config).unwrap();
    ConfigState::load(dir.to_path_buf())
}

fn key(port: u16, slug: &str) -> String {
    format!("{}|{slug}", server_url(port))
}

#[test]
fn the_hash_is_the_devices() {
    assert_eq!(password_hash(&Password::new(PASSWORD), CODE), HASH);
}

#[test]
fn login_stores_a_write_token() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let store = MemoryStore::new();

    let saved = log_in(
        &state,
        &ServeInfo::default(),
        &store,
        " me@example.org ",
        &Password::new(PASSWORD),
    )
    .unwrap();

    let code = server.next();
    assert_eq!(code.method, "POST");
    assert_eq!(code.target, "/rest/v1/login/code");
    assert_eq!(code.header("content-type"), Some("application/json"));
    assert_eq!(code.header("authorization"), None);
    assert_eq!(code.json(), json!({"email": "me@example.org"}));

    let login = server.next();
    assert_eq!(login.method, "POST");
    assert_eq!(login.target, "/rest/v1/login");
    assert_eq!(login.header("authorization"), None);
    let body = login.json();
    // The server refuses any other field.
    let keys: BTreeSet<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["email", "passwordHash", "name", "scope"])
    );
    assert_eq!(body["email"], "me@example.org");
    assert_eq!(body["passwordHash"], HASH);
    assert_eq!(body["name"], "Notedown for Mac");
    assert_eq!(body["scope"], "write");
    assert!(!String::from_utf8_lossy(&login.body).contains(PASSWORD));

    let token = store.get(&key(server.port, "jordi")).unwrap().unwrap();
    assert_eq!(token.expose(), "tok-123");
    assert_eq!(saved.account.as_deref(), Some("jordi"));
    assert_eq!(saved.email.as_deref(), Some("me@example.org"));
    assert_eq!(state.current(), saved);
    // On disk too.
    assert_eq!(config::load(dir.path()), saved);
}

#[test]
fn a_failed_save_removes_the_token() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(server.port)),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load_with(
        dir.path().to_path_buf(),
        Arc::new(|_: &Path, _: &AppConfig| Err(std::io::Error::other("the disk is full"))),
    );
    let store = MemoryStore::new();

    let error = log_in(
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert!(matches!(error, LoginError::Save(_)), "{error:?}");
    assert!(error.to_string().contains("the disk is full"), "{error}");
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);
    assert_eq!(state.current().account, None);
    assert_eq!(state.current().email, None);
}

#[test]
fn login_without_a_mode_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    let store = MemoryStore::new();
    let error = log_in(
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert_eq!(error, LoginError::NoMode);
    assert_eq!(error.to_string(), "Choose where the server runs first.");
    assert_eq!(state.current(), AppConfig::default());
}

#[test]
fn an_empty_field_is_refused_before_any_request() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let store = MemoryStore::new();
    for (email, password) in [(" ", PASSWORD), ("me@example.org", "")] {
        let error = log_in(
            &state,
            &ServeInfo::default(),
            &store,
            email,
            &Password::new(password),
        )
        .unwrap_err();
        assert_eq!(error, LoginError::Missing);
        assert_eq!(error.to_string(), "Enter your e-mail and password.");
    }
    assert!(server.received().is_empty());
}

#[test]
fn a_wrong_password_says_so() {
    // A third answer that a second try would get.
    let server = Scripted::start(vec![
        code_answer(),
        answer("401 Unauthorized", r#"{"error":"login_failed"}"#),
        code_answer(),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let store = MemoryStore::new();
    let error = log_in(
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new("wrong"),
    )
    .unwrap_err();
    assert_eq!(error, LoginError::Rest(RestError::LoginFailed));
    assert_eq!(
        error.to_string(),
        "Wrong e-mail or password, or the account is locked for now."
    );
    // Never tried again by itself: six wrong tries lock the account.
    assert_eq!(server.received().len(), 2);
    assert_eq!(state.current().account, None);
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);
}

#[test]
fn too_many_tries_says_so() {
    let too_many = || answer("429 Too Many Requests", r#"{"error":"too_many"}"#);
    // At the code, and at the login.
    let server = Scripted::start(vec![too_many(), code_answer(), too_many()]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let store = MemoryStore::new();
    for _ in 0..2 {
        let error = log_in(
            &state,
            &ServeInfo::default(),
            &store,
            "me@example.org",
            &Password::new(PASSWORD),
        )
        .unwrap_err();
        assert_eq!(error, LoginError::Rest(RestError::TooMany));
        assert_eq!(error.to_string(), "Too many tries. Wait a minute.");
    }
    assert_eq!(server.received().len(), 3);
}

#[test]
fn another_account_replaces_the_old_one() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, Some("old"));
    let store = MemoryStore::new();
    store
        .set(&key(server.port, "old"), &Token::new("old-token"))
        .unwrap();
    store.set("local|other", &Token::new("other")).unwrap();
    let saved = log_in(
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    assert_eq!(saved.account.as_deref(), Some("jordi"));
    assert_eq!(store.get(&key(server.port, "old")).unwrap(), None);
    assert!(store.get(&key(server.port, "jordi")).unwrap().is_some());
    // Only the old account of this server goes.
    assert!(store.get("local|other").unwrap().is_some());
}

/// A Keychain that takes nothing.
struct NoKeychain;

impl TokenStore for NoKeychain {
    fn get(&self, _: &str) -> Result<Option<Token>, TokenError> {
        Ok(None)
    }
    fn set(&self, _: &str, _: &Token) -> Result<(), TokenError> {
        Err(TokenError("locked".into()))
    }
    fn delete(&self, _: &str) -> Result<(), TokenError> {
        Ok(())
    }
}

#[test]
fn a_keychain_that_refuses_keeps_the_config() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let error = log_in(
        &state,
        &ServeInfo::default(),
        &NoKeychain,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert!(matches!(error, LoginError::Keychain(_)), "{error:?}");
    assert!(!error.to_string().contains("tok-123"), "{error}");
    assert_eq!(state.current().account, None);
}

#[test]
fn no_login_error_holds_the_password() {
    let password = Password::new(PASSWORD);
    let mut errors = Vec::new();
    let try_at = |port: u16, errors: &mut Vec<LoginError>| {
        let dir = tempfile::tempdir().unwrap();
        let state = remote_config(dir.path(), port, None);
        errors.push(
            log_in(
                &state,
                &ServeInfo::default(),
                &MemoryStore::new(),
                "me@example.org",
                &password,
            )
            .unwrap_err(),
        );
    };

    // Refused.
    let refused = Scripted::start(vec![
        code_answer(),
        answer("401 Unauthorized", r#"{"error":"login_failed"}"#),
    ]);
    try_at(refused.port, &mut errors);
    // Unreachable.
    try_at(free_port(), &mut errors);
    // Garbled: the code, then the login answer.
    let garbled_code = Scripted::start(vec![answer("200 OK", "[1,2")]);
    try_at(garbled_code.port, &mut errors);
    let garbled = Scripted::start(vec![
        code_answer(),
        answer("201 Created", r#"{"token":"tok-123","slug":"#),
    ]);
    try_at(garbled.port, &mut errors);
    let no_slug = Scripted::start(vec![
        code_answer(),
        answer("201 Created", r#"{"token":"tok-123","slug":""}"#),
    ]);
    try_at(no_slug.port, &mut errors);
    // Not HTTP at all.
    let broken = Scripted::start(vec!["no HTTP here\r\n\r\n".to_owned()]);
    try_at(broken.port, &mut errors);

    assert!(matches!(
        errors[0],
        LoginError::Rest(RestError::LoginFailed)
    ));
    assert!(matches!(
        errors[1],
        LoginError::Rest(RestError::Unreachable)
    ));
    for garbled in &errors[2..5] {
        assert!(
            matches!(garbled, LoginError::Rest(RestError::Garbled(_))),
            "{garbled:?}"
        );
    }
    errors.extend([
        LoginError::NoMode,
        LoginError::Missing,
        LoginError::Keychain("locked".into()),
        LoginError::Save("the config was not saved: full".into()),
    ]);
    for error in &errors {
        for text in [
            error.to_string(),
            format!("{error:?}"),
            format!("{error:#?}"),
        ] {
            for secret in [PASSWORD, PASSWORD_MD5, HASH, "tok-123"] {
                assert!(!text.contains(secret), "{text} holds {secret}");
            }
        }
    }
}
