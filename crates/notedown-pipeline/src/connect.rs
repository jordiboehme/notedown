//! `notedown connect`: log in to a server over REST, keep the account's token on this machine
//! and write `pipeline.toml` so the pipeline reads the server over REST.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notedown_cloud::auth::{expected_login_hash, md5_hex, token_id, token_slug, validate_slug};
use notedown_cloud::fsutil::write_atomic;
use notedown_cloud::rest::PREFIX;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::json;

use crate::setup::{FILE, LocalConfig, ServerAt, TOKENS_FOLDER};
use crate::{Error, Result};

/// The time to open a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The time for each call, for the answer and again for its body.
const TIMEOUT: Duration = Duration::from_secs(15);

/// What a login stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    pub slug: String,
    pub user_id: u64,
    /// The id of the new token (never the token).
    pub token_id: String,
    pub token_file: PathBuf,
    /// The id of the token the file held before: it still works on the server.
    pub replaced_token_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Code {
    random_code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Login {
    token: String,
    id: String,
    slug: String,
    user_id: u64,
}

fn invalid(message: String) -> Error {
    Error::Connect(message)
}

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

/// The checks of [`connect`] that need no password: the URL, and a `local` folder that is no
/// server's data and whose `pipeline.toml` points at no other server. Run them before the
/// password is asked for. Gives the URL without a trailing slash and the `pipeline.toml`.
pub fn check(url: &str, local: &Path) -> Result<(String, Option<LocalConfig>)> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(invalid(format!(
            "{url:?} does not start with http:// or https://"
        )));
    }
    // The URL is not repeated here: it may hold a password.
    let parsed = reqwest::Url::parse(&url)
        .map_err(|e| invalid(format!("the server url cannot be read: {e}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "the server url must not hold a user name or a password; connect asks for the \
             password"
                .into(),
        ));
    }
    if local.join("accounts").exists() || local.join("server.toml").exists() {
        return Err(invalid(format!(
            "{} holds a server's data (accounts or server.toml); use another --data folder for \
             the pipeline",
            local.display()
        )));
    }
    let existing = LocalConfig::load(local)?;
    match existing.as_ref().map(|c| &c.server) {
        None => {}
        Some(ServerAt::Url(u)) if *u == url => {}
        Some(at) => {
            let what = match at {
                ServerAt::Url(u) => u.clone(),
                ServerAt::Folder(f) => format!("the folder {}", f.display()),
            };
            return Err(invalid(format!(
                "{FILE} points at {what}; change it or use another --data folder"
            )));
        }
    }
    Ok((url, existing))
}

/// Logs in to the server at `url` as `email` and stores the token in
/// `<local>/tokens/<slug>.token`. Writes `<local>/pipeline.toml` when it is missing. The
/// password and the token appear in no error.
pub fn connect(
    url: &str,
    local: &Path,
    email: &str,
    password: &str,
    name: &str,
) -> Result<Connected> {
    let (url, existing) = check(url, local)?;

    let client = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| invalid(format!("http client: {}", chain(&e))))?;
    let post = |path: &str, body: serde_json::Value| -> Result<(StatusCode, Vec<u8>)> {
        let target = format!("{url}{PREFIX}{path}");
        let transport =
            |e: reqwest::Error| invalid(format!("{target}: {}", chain(&e.without_url())));
        let resp = client
            .post(&target)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .map_err(transport)?;
        let status = resp.status();
        let bytes = resp.bytes().map_err(transport)?;
        Ok((status, bytes.to_vec()))
    };
    let unexpected = |path: &str, status: StatusCode| {
        invalid(format!(
            "{url}{PREFIX}{path}: the server answered {status}; is this a notedown server?"
        ))
    };
    let parse = |path: &str, status: StatusCode, bytes: &[u8]| -> Result<serde_json::Value> {
        serde_json::from_slice(bytes).map_err(|_| unexpected(path, status))
    };

    let (status, bytes) = post("/login/code", json!({ "email": email }))?;
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(invalid("too many logins; wait a minute".into()));
    }
    if status != StatusCode::OK {
        return Err(unexpected("/login/code", status));
    }
    let code: Code = serde_json::from_value(parse("/login/code", status, &bytes)?)
        .map_err(|_| unexpected("/login/code", status))?;

    let hash = expected_login_hash(&md5_hex(password), &code.random_code);
    let (status, bytes) = post(
        "/login",
        // No scope: the server gives a read token, and an older server refuses an unknown field.
        json!({ "email": email, "passwordHash": hash, "name": name }),
    )?;
    match status {
        StatusCode::UNAUTHORIZED => {
            return Err(invalid(
                "the server refused the email or the password".into(),
            ));
        }
        StatusCode::TOO_MANY_REQUESTS => {
            return Err(invalid("too many logins; wait a minute".into()));
        }
        StatusCode::CREATED => {}
        other => return Err(unexpected("/login", other)),
    }
    let login: Login = serde_json::from_value(parse("/login", status, &bytes)?)
        .map_err(|_| unexpected("/login", status))?;
    if validate_slug(&login.slug).is_err()
        || token_slug(&login.token).as_deref() != Some(&login.slug)
    {
        return Err(invalid(format!(
            "{url}{PREFIX}/login: the answer is not a login of this server"
        )));
    }

    let tokens = local.join(TOKENS_FOLDER);
    let token_file = tokens.join(format!("{}.token", login.slug));
    let replaced_token_id = std::fs::read_to_string(&token_file)
        .ok()
        .and_then(|old| token_id(old.trim()))
        .filter(|old| *old != login.id);
    std::fs::create_dir_all(&tokens).map_err(|source| Error::Io {
        path: tokens.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tokens, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| Error::Io {
                path: tokens.clone(),
                source,
            },
        )?;
    }
    write_atomic(&token_file, format!("{}\n", login.token).as_bytes(), true)?;
    if existing.is_none() {
        write_atomic(
            &local.join(FILE),
            format!("[server]\nurl = {}\n", toml::Value::String(url.clone())).as_bytes(),
            false,
        )?;
    }
    Ok(Connected {
        slug: login.slug,
        user_id: login.user_id,
        token_id: login.id,
        token_file,
        replaced_token_id,
    })
}
