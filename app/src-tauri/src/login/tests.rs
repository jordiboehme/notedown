//! The login against a scripted server on loopback, with a store in memory (never the real
//! Keychain) and the config in a temp folder.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::commands::{ConfigAnswer, set_mode_then};
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
    scoped_answer(slug, "write")
}

fn scoped_answer(slug: &str, scope: &str) -> String {
    answer(
        "201 Created",
        &format!(r#"{{"token":"tok-123","id":"t1","slug":"{slug}","userId":7,"scope":"{scope}"}}"#),
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
        data_dir: dir.join("data"),
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
        &LoginGate::default(),
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
    assert_eq!(saved.config.account.as_deref(), Some("jordi"));
    assert_eq!(saved.config.email.as_deref(), Some("me@example.org"));
    assert_eq!(state.current(), saved.config);
    assert_eq!(saved.keychain, None);
    // On disk too.
    assert_eq!(config::load(dir.path()), saved.config);
}

#[test]
fn a_failed_save_removes_the_token() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(server.port)),
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load_with(
        dir.path().to_path_buf(),
        Arc::new(|_: &Path, _: &AppConfig| Err(std::io::Error::other("the disk is full"))),
    );
    let store = MemoryStore::new();

    let error = log_in(
        &LoginGate::default(),
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
        &LoginGate::default(),
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
            &LoginGate::default(),
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
        &LoginGate::default(),
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
            &LoginGate::default(),
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
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    assert_eq!(saved.config.account.as_deref(), Some("jordi"));
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
        &LoginGate::default(),
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
                &LoginGate::default(),
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
        LoginError::Busy,
        LoginError::ServerChanged,
        LoginError::NoWriteAccess,
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

#[test]
fn a_server_change_during_the_login_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // The port is known only once the server runs; the hook reads the state from here.
    let slot: Arc<Mutex<Option<ConfigState>>> = Arc::default();
    let hook_slot = slot.clone();
    let server = Scripted::start_with(vec![code_answer(), token_answer("jordi")], move |n, _| {
        if n == 1 {
            // Another address on the Server page while the login waits for its answer.
            let state = hook_slot.lock().unwrap().clone().unwrap();
            state
                .set_mode(
                    Mode::Remote,
                    None,
                    Some("http://other.example:19072".into()),
                )
                .unwrap();
        }
    });
    let state = remote_config(dir.path(), server.port, None);
    *slot.lock().unwrap() = Some(state.clone());
    let store = MemoryStore::new();
    let error = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert_eq!(error, LoginError::ServerChanged);
    assert_eq!(
        error.to_string(),
        "The server changed during the login. Log in again."
    );
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);
    assert_eq!(store.get("http://other.example:19072|jordi").unwrap(), None);
    assert_eq!(state.current().account, None);
    assert_eq!(state.current().email, None);
}

#[test]
fn a_second_login_while_one_runs_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(LoginGate::default());
    let slot: Arc<Mutex<Option<ConfigState>>> = Arc::default();
    let second: Arc<Mutex<Option<Result<ConfigAnswer, LoginError>>>> = Arc::default();
    let (hook_gate, hook_slot, hook_second) = (gate.clone(), slot.clone(), second.clone());
    let server = Scripted::start_with(
        vec![code_answer(), token_answer("jordi"), code_answer()],
        move |n, _| {
            if n == 0 {
                let state = hook_slot.lock().unwrap().clone().unwrap();
                let result = log_in(
                    &hook_gate,
                    &state,
                    &ServeInfo::default(),
                    &MemoryStore::new(),
                    "me@example.org",
                    &Password::new(PASSWORD),
                );
                *hook_second.lock().unwrap() = Some(result);
            }
        },
    );
    let state = remote_config(dir.path(), server.port, None);
    *slot.lock().unwrap() = Some(state.clone());
    let store = MemoryStore::new();
    let saved = log_in(
        &gate,
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    assert_eq!(saved.config.account.as_deref(), Some("jordi"));
    let second = second.lock().unwrap().take().expect("the second login ran");
    let error = second.unwrap_err();
    assert_eq!(error, LoginError::Busy);
    assert_eq!(error.to_string(), "A login is already running.");
    // The second one sent nothing.
    assert_eq!(server.received().len(), 2);
    // Once the first ended, a login runs again (and stops at the empty e-mail).
    let again = log_in(
        &gate,
        &state,
        &ServeInfo::default(),
        &store,
        "",
        &Password::new(PASSWORD),
    );
    assert_eq!(again.unwrap_err(), LoginError::Missing);
}

#[test]
fn a_failed_login_keeps_the_old_accounts_token() {
    let server = Scripted::start(vec![
        code_answer(),
        answer("401 Unauthorized", r#"{"error":"login_failed"}"#),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, Some("old"));
    let store = MemoryStore::new();
    store
        .set(&key(server.port, "old"), &Token::new("old-token"))
        .unwrap();
    let error = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new("wrong"),
    )
    .unwrap_err();
    assert_eq!(error, LoginError::Rest(RestError::LoginFailed));
    let old = store.get(&key(server.port, "old")).unwrap().unwrap();
    assert_eq!(old.expose(), "old-token");
    assert_eq!(state.current().account.as_deref(), Some("old"));
}

#[test]
fn a_local_login_keeps_the_token_under_local_and_the_slug() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Local,
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    let serve_info = ServeInfo {
        listen: vec![format!("127.0.0.1:{}", server.port).parse().unwrap()],
        ..ServeInfo::default()
    };
    let store = MemoryStore::new();
    let saved = log_in(
        &LoginGate::default(),
        &state,
        &serve_info,
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    assert_eq!(saved.config.account.as_deref(), Some("jordi"));
    assert_eq!(
        store.get("local|jordi").unwrap().unwrap().expose(),
        "tok-123"
    );
    assert_eq!(server.next().target, "/rest/v1/login/code");
}

#[test]
fn a_token_without_write_access_is_refused() {
    let server = Scripted::start(vec![code_answer(), scoped_answer("jordi", "read")]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, Some("old"));
    let store = MemoryStore::new();
    store
        .set(&key(server.port, "old"), &Token::new("old-token"))
        .unwrap();
    let error = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert_eq!(error, LoginError::NoWriteAccess);
    assert_eq!(
        error.to_string(),
        "The server gave this Mac no write access."
    );
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);
    assert!(store.get(&key(server.port, "old")).unwrap().is_some());
    assert_eq!(state.current().account.as_deref(), Some("old"));
}

/// A store that keeps tokens but cannot delete them.
struct NoDelete(MemoryStore);

impl TokenStore for NoDelete {
    fn get(&self, key: &str) -> Result<Option<Token>, TokenError> {
        self.0.get(key)
    }
    fn set(&self, key: &str, token: &Token) -> Result<(), TokenError> {
        self.0.set(key, token)
    }
    fn delete(&self, _: &str) -> Result<(), TokenError> {
        Err(TokenError("locked".into()))
    }
}

#[test]
fn a_failed_rollback_is_named() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(server.port)),
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load_with(
        dir.path().to_path_buf(),
        Arc::new(|_: &Path, _: &AppConfig| Err(std::io::Error::other("the disk is full"))),
    );
    let error = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &NoDelete(MemoryStore::new()),
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("the disk is full"), "{text}");
    assert!(
        text.contains("The new login could not be removed from the Keychain"),
        "{text}"
    );
    assert!(!text.contains("tok-123"), "{text}");
}

#[test]
fn has_login_says_whether_a_token_is_kept() {
    let store = MemoryStore::new();
    let config = AppConfig {
        mode: Mode::Local,
        account: Some("jordi".into()),
        ..AppConfig::default()
    };
    assert!(!has_login(&store, &config).unwrap());
    store.set("local|jordi", &Token::new("t")).unwrap();
    assert!(has_login(&store, &config).unwrap());
    // No account, or no mode: no login.
    assert!(!has_login(&store, &AppConfig::default()).unwrap());
}

#[test]
fn a_new_local_folder_during_the_login_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("other-data");
    let slot: Arc<Mutex<Option<ConfigState>>> = Arc::default();
    let hook_slot = slot.clone();
    let server = Scripted::start_with(vec![code_answer(), token_answer("jordi")], move |n, _| {
        if n == 1 {
            // The key `local|jordi` stays the same, but the folder is another server.
            let state = hook_slot.lock().unwrap().clone().unwrap();
            state
                .set_mode(Mode::Local, Some(other.clone()), None)
                .unwrap();
        }
    });
    let config = AppConfig {
        mode: Mode::Local,
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    *slot.lock().unwrap() = Some(state.clone());
    let serve_info = ServeInfo {
        listen: vec![format!("127.0.0.1:{}", server.port).parse().unwrap()],
        ..ServeInfo::default()
    };
    let store = MemoryStore::new();
    let error = log_in(
        &LoginGate::default(),
        &state,
        &serve_info,
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert_eq!(error, LoginError::ServerChanged);
    assert_eq!(store.get("local|jordi").unwrap(), None);
    assert_eq!(state.current().account, None);
}

/// A log out that runs on a thread of its own.
type LogOut = std::thread::JoinHandle<Result<AppConfig, String>>;

#[test]
fn a_log_out_during_a_login_leaves_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(LoginGate::default());
    let store = Arc::new(MemoryStore::new());
    let slot: Arc<Mutex<Option<ConfigState>>> = Arc::default();
    let leaving: Arc<Mutex<Option<LogOut>>> = Arc::default();
    let (hook_gate, hook_store, hook_slot, hook_leaving) =
        (gate.clone(), store.clone(), slot.clone(), leaving.clone());
    let answers = vec![code_answer(), token_answer("jordi"), revoked_answer()];
    let server = Scripted::start_with(answers, move |n, _| {
        if n == 1 {
            // Log out is pressed while the login waits for its token.
            let (gate, store) = (hook_gate.clone(), hook_store.clone());
            let state = hook_slot.lock().unwrap().clone().unwrap();
            *hook_leaving.lock().unwrap() = Some(std::thread::spawn(move || {
                crate::commands::log_out_of(
                    &gate,
                    &*store,
                    &state,
                    &crate::library::idle(),
                    &ServeInfo::default(),
                )
            }));
        }
    });
    let state = remote_config(dir.path(), server.port, None);
    *slot.lock().unwrap() = Some(state.clone());
    let saved = log_in(
        &gate,
        &state,
        &ServeInfo::default(),
        &*store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    assert_eq!(saved.config.account.as_deref(), Some("jordi"));
    let after = leaving
        .lock()
        .unwrap()
        .take()
        .expect("the log out started")
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(after.account, None);
    assert_eq!(after.email, None);
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);
    assert_eq!(state.current().account, None);
    // The token the login left is revoked on the server too.
    server.next();
    server.next();
    let revoke = server.next();
    assert_eq!(revoke.target, "/rest/v1/logout");
    assert_eq!(revoke.header("authorization"), Some("Bearer tok-123"));
}

/// Logs in to a scripted server with `state`; the store keeps the token.
fn logged_in(state: &ConfigState, serve_info: &ServeInfo, store: &dyn TokenStore) {
    log_in(
        &LoginGate::default(),
        state,
        serve_info,
        store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
}

fn set_mode_now(
    state: &ConfigState,
    store: Arc<dyn TokenStore>,
    mode: Mode,
    data_dir: Option<std::path::PathBuf>,
    server_url: Option<&str>,
    serve_info: &ServeInfo,
) -> ConfigAnswer {
    tauri::async_runtime::block_on(set_mode_then(
        state,
        Arc::new(LoginGate::default()),
        store,
        crate::library::idle(),
        mode,
        data_dir,
        server_url.map(str::to_owned),
        || {},
        serve_info,
    ))
    .unwrap()
}

#[test]
fn a_new_address_after_a_login_removes_its_token() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi"), revoked_answer()]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let store = Arc::new(MemoryStore::new());
    logged_in(&state, &ServeInfo::default(), &*store);
    assert!(store.get(&key(server.port, "jordi")).unwrap().is_some());

    let answer = set_mode_now(
        &state,
        store.clone(),
        Mode::Remote,
        None,
        Some("http://other.example:19072"),
        &ServeInfo::default(),
    );
    assert_eq!(answer.keychain, None);
    assert_eq!(answer.config.account, None);
    assert_eq!(answer.config.email, None);
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);
    assert_eq!(state.current().account, None);
    assert_revoked(&server, "tok-123");
}

#[test]
fn a_new_local_folder_after_a_login_removes_its_token() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi"), revoked_answer()]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Local,
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    let serve_info = ServeInfo {
        listen: vec![format!("127.0.0.1:{}", server.port).parse().unwrap()],
        ..ServeInfo::default()
    };
    let store = Arc::new(MemoryStore::new());
    logged_in(&state, &serve_info, &*store);
    assert!(store.get("local|jordi").unwrap().is_some());

    let answer = set_mode_now(
        &state,
        store.clone(),
        Mode::Local,
        Some(dir.path().join("other-data")),
        None,
        &serve_info,
    );
    assert_eq!(answer.keychain, None);
    assert_eq!(answer.config.account, None);
    assert_eq!(store.get("local|jordi").unwrap(), None);
    assert_revoked(&server, "tok-123");
    // The same place again keeps a login made after it.
    store.set("local|jordi", &Token::new("t")).unwrap();
    set_mode_now(
        &state,
        store.clone(),
        Mode::Local,
        Some(dir.path().join("other-data")),
        None,
        &serve_info,
    );
    assert!(store.get("local|jordi").unwrap().is_some());
}

#[test]
fn a_token_that_set_mode_cannot_remove_is_named() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi"), revoked_answer()]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, None);
    let store = Arc::new(NoDelete(MemoryStore::new()));
    logged_in(&state, &ServeInfo::default(), &*store);
    let answer = set_mode_now(
        &state,
        store.clone(),
        Mode::Remote,
        None,
        Some("http://other.example:19072"),
        &ServeInfo::default(),
    );
    // The new place is kept; the answer names what stayed.
    assert_eq!(answer.config.account, None);
    let text = answer.keychain.expect("the kept token is named");
    assert!(
        text.contains("The old login could not be removed from the Keychain"),
        "{text}"
    );
    assert!(!text.contains("tok-123"), "{text}");
    // The token the Keychain kept is useless now: it is revoked all the same.
    assert_revoked(&server, "tok-123");
}

#[test]
fn an_old_token_that_stays_after_a_login_is_named() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let state = remote_config(dir.path(), server.port, Some("old"));
    let store = NoDelete(MemoryStore::new());
    store
        .set(&key(server.port, "old"), &Token::new("old-token"))
        .unwrap();
    let answer = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    assert_eq!(answer.config.account.as_deref(), Some("jordi"));
    let text = answer.keychain.expect("the old token is named");
    assert!(
        text.contains("The old login could not be removed from the Keychain"),
        "{text}"
    );
    assert!(store.get(&key(server.port, "jordi")).unwrap().is_some());
}

#[test]
fn a_failed_save_on_a_new_login_of_the_same_account_keeps_the_old_token() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(server.port)),
        account: Some("jordi".into()),
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load_with(
        dir.path().to_path_buf(),
        Arc::new(|_: &Path, _: &AppConfig| Err(std::io::Error::other("the disk is full"))),
    );
    let store = MemoryStore::new();
    store
        .set(&key(server.port, "jordi"), &Token::new("old-token"))
        .unwrap();
    let error = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &store,
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap_err();
    assert!(matches!(error, LoginError::Save(_)), "{error:?}");
    let kept = store.get(&key(server.port, "jordi")).unwrap().unwrap();
    assert_eq!(kept.expose(), "old-token");
    assert_eq!(state.current().account.as_deref(), Some("jordi"));
}

#[test]
fn a_login_keeps_the_markdown_folder_of_the_pipeline() {
    let server = Scripted::start(vec![code_answer(), token_answer("jordi")]);
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("pipeline.toml"),
        "[server]\nurl = \"http://pi.local:19072\"\n[pipeline.markdown_dirs]\njordi = \"/notes/md\"\n",
    )
    .unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(server.port)),
        data_dir: data,
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    let saved = log_in(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &MemoryStore::new(),
        "me@example.org",
        &Password::new(PASSWORD),
    )
    .unwrap();
    // Saved with the login, so the pipeline starts again with the same folder.
    assert_eq!(
        saved.config.markdown_dir(),
        Some(std::path::PathBuf::from("/notes/md"))
    );
    assert_eq!(config::load(dir.path()), saved.config);
}

#[test]
fn the_password_shows_no_text_in_debug() {
    assert_eq!(format!("{:?}", Password::new(PASSWORD)), "***");
}

/// The scripted server got the login (two calls), then the revoke of `token`.
fn assert_revoked(server: &Scripted, token: &str) {
    server.next();
    server.next();
    let revoke = server.next();
    assert_eq!(revoke.method, "POST");
    assert_eq!(revoke.target, "/rest/v1/logout");
    assert_eq!(
        revoke.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
}

/// What a server answers that revoked the token.
fn revoked_answer() -> String {
    answer("200 OK", r#"{"revoked":true}"#)
}

/// A Remote config for `port` with the account `jordi` and its token `tok-123` in the store.
fn kept_login(dir: &Path, port: u16) -> (ConfigState, MemoryStore) {
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(server_url(port)),
        account: Some("jordi".into()),
        email: Some("me@example.org".into()),
        data_dir: dir.join("data"),
        ..AppConfig::default()
    };
    config::save(dir, &config).unwrap();
    let state = ConfigState::load(dir.to_path_buf());
    let store = MemoryStore::new();
    store
        .set(&key(port, "jordi"), &Token::new("tok-123"))
        .unwrap();
    (state, store)
}

/// Log out of `state` as the command does it.
fn log_out_now(state: &ConfigState, store: &dyn TokenStore, serve_info: &ServeInfo) -> AppConfig {
    crate::commands::log_out_of(
        &LoginGate::default(),
        store,
        state,
        &crate::library::idle(),
        serve_info,
    )
    .unwrap()
}

/// The config, the Keychain and the file know no login of `jordi` at `port` anymore.
fn assert_logged_out(dir: &Path, state: &ConfigState, store: &dyn TokenStore, port: u16) {
    assert_eq!(store.get(&key(port, "jordi")).unwrap(), None);
    assert_eq!(state.current().account, None);
    assert_eq!(state.current().email, None);
    assert_eq!(config::load(dir).account, None);
}

#[test]
fn log_out_revokes_the_token_on_the_server() {
    let server = Scripted::start(vec![revoked_answer()]);
    let dir = tempfile::tempdir().unwrap();
    let (state, store) = kept_login(dir.path(), server.port);

    let after = log_out_now(&state, &store, &ServeInfo::default());

    let request = server.next();
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rest/v1/logout");
    assert_eq!(request.header("authorization"), Some("Bearer tok-123"));
    assert!(request.body.is_empty());
    assert_eq!(after.account, None);
    assert_logged_out(dir.path(), &state, &store, server.port);
}

#[test]
fn log_out_without_a_server_still_logs_out() {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let (state, store) = kept_login(dir.path(), port);

    let started = std::time::Instant::now();
    log_out_now(&state, &store, &ServeInfo::default());

    assert!(started.elapsed() < LOGOUT_TIMEOUT + std::time::Duration::from_secs(1));
    assert_logged_out(dir.path(), &state, &store, port);
}

#[test]
fn log_out_waits_for_a_silent_server_no_longer_than_the_bound() {
    // The connection opens (the system accepts it), but nobody ever answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let (state, store) = kept_login(dir.path(), port);

    let started = std::time::Instant::now();
    log_out_now(&state, &store, &ServeInfo::default());

    assert!(started.elapsed() < LOGOUT_TIMEOUT + std::time::Duration::from_secs(1));
    assert_logged_out(dir.path(), &state, &store, port);
    drop(silent);
}

#[test]
fn log_out_on_an_older_server_still_logs_out() {
    let server = Scripted::start(vec![answer("404 Not Found", r#"{"error":"no_route"}"#)]);
    let dir = tempfile::tempdir().unwrap();
    let (state, store) = kept_login(dir.path(), server.port);

    log_out_now(&state, &store, &ServeInfo::default());

    assert_eq!(server.next().target, "/rest/v1/logout");
    assert_logged_out(dir.path(), &state, &store, server.port);
}

#[test]
fn revoke_counts_a_token_that_is_gone_as_done() {
    for (status, body) in [
        ("200 OK", r#"{"revoked":true}"#),
        ("401 Unauthorized", r#"{"error":"token_refused"}"#),
        ("410 Gone", r#"{"error":"account_removed"}"#),
    ] {
        let server = Scripted::start(vec![answer(status, body)]);
        assert_eq!(
            revoke(&server.base(), Token::new("tok-123")),
            Ok(()),
            "{status}"
        );
    }
    let older = Scripted::start(vec![answer("404 Not Found", r#"{"error":"no_route"}"#)]);
    let error = revoke(&older.base(), Token::new("tok-123")).unwrap_err();
    assert!(!error.to_string().contains("tok-123"), "{error}");
    assert_eq!(
        error,
        RestError::Server {
            status: 404,
            code: Some("no_route".into())
        }
    );
}

#[test]
fn a_new_server_revokes_the_old_token() {
    // From one address to another: the old server hears of it.
    let server = Scripted::start(vec![revoked_answer()]);
    let dir = tempfile::tempdir().unwrap();
    let (state, store) = kept_login(dir.path(), server.port);
    let store = Arc::new(store);
    let answer = set_mode_now(
        &state,
        store.clone(),
        Mode::Remote,
        None,
        Some("http://other.example:19072"),
        &ServeInfo::default(),
    );
    let request = server.next();
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rest/v1/logout");
    assert_eq!(request.header("authorization"), Some("Bearer tok-123"));
    assert_eq!(answer.config.account, None);
    assert_eq!(store.get(&key(server.port, "jordi")).unwrap(), None);

    // From this Mac to another machine: the app's own server (from its start lines) hears of
    // it before the restart stops it.
    let heard = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_heard = heard.clone();
    let server = Scripted::start_with(vec![revoked_answer()], move |_, _| {
        hook_heard.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Local,
        account: Some("jordi".into()),
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    let store = Arc::new(MemoryStore::new());
    store.set("local|jordi", &Token::new("tok-local")).unwrap();
    let serve_info = ServeInfo {
        listen: vec![format!("127.0.0.1:{}", server.port).parse().unwrap()],
        ..ServeInfo::default()
    };
    let stopped_after: Arc<Mutex<Option<bool>>> = Arc::default();
    let restart_saw = stopped_after.clone();
    let answer = tauri::async_runtime::block_on(set_mode_then(
        &state,
        Arc::new(LoginGate::default()),
        store.clone(),
        crate::library::idle(),
        Mode::Remote,
        None,
        Some("http://other.example:19072".to_owned()),
        move || {
            *restart_saw.lock().unwrap() = Some(heard.load(std::sync::atomic::Ordering::SeqCst));
        },
        &serve_info,
    ))
    .unwrap();
    let request = server.next();
    assert_eq!(request.target, "/rest/v1/logout");
    assert_eq!(request.header("authorization"), Some("Bearer tok-local"));
    assert_eq!(
        *stopped_after.lock().unwrap(),
        Some(true),
        "the restart ran before the revoke"
    );
    assert_eq!(answer.config.account, None);
    assert_eq!(store.get("local|jordi").unwrap(), None);
}
