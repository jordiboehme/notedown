//! The setup's calls of the bundled `notedown` that need a password: a new account on this
//! Mac (`cloud account add`) and the pipeline's connection to a server on another machine
//! (`connect`). The password goes to the sidecar over stdin only
//! ([`process::run_with_password`]), and no error text that holds it comes back. Also what
//! the data folders on this Mac already hold, so the setup can use them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::commands::{ConfigAnswer, ConfigState, log_in_to};
use crate::config::{AppConfig, Mode, existing_setup, pipeline_server_url};
use crate::library::Library;
use crate::login::{self, LoginGate, Password};
use crate::process::{self, SpawnFn};
use crate::serve_info::ServeInfo;
use crate::token::TokenStore;

/// How long `cloud account add` and `connect` may take.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How the pipeline's token is listed on the server.
pub const PIPELINE_NAME: &str = "Notedown for Mac pipeline";
/// What an error text that holds the password is replaced with.
pub const COMMAND_FAILED: &str = "The command failed.";
/// The account was made, but the login after it failed.
pub const ACCOUNT_MADE: &str = "The account is made. Log in with it.";
/// What a new account answers in Remote mode (without a mode: [`login::NO_MODE`]).
pub const NOT_HERE: &str = "Accounts on another machine are made there.";
/// What the pipeline connection answers outside Remote mode.
pub const ONLY_REMOTE: &str = "Only a server on another machine needs this connection.";
/// What the pipeline connection answers without an e-mail in the config.
pub const LOG_IN_FIRST: &str = "Log in first.";
/// What an account name that `cloud account add` would refuse answers, before the spawn.
pub const BAD_SLUG: &str =
    "The account name must be 1 to 64 of a-z, 0-9, - and _, starting with a letter or digit.";

/// The bundled `notedown` for the setup's calls.
pub struct Sidecar(pub SpawnFn);

/// Makes the account `slug` in the server folder `data_dir`:
/// `cloud account --data <dir> add <slug> --email <email> --password-stdin`, within
/// [`SETUP_TIMEOUT`].
pub fn create_account(
    spawn: &SpawnFn,
    data_dir: &Path,
    slug: &str,
    email: &str,
    password: &Password,
) -> Result<(), String> {
    password.one_line()?;
    if !valid_slug(slug) {
        return Err(BAD_SLUG.to_owned());
    }
    let args = vec![
        "cloud".to_owned(),
        "account".to_owned(),
        "--data".to_owned(),
        data_dir.display().to_string(),
        "add".to_owned(),
        slug.to_owned(),
        "--email".to_owned(),
        email.trim().to_owned(),
        "--password-stdin".to_owned(),
    ];
    run(spawn, &args, password)
}

/// The rule of `cloud account add`: 1 to 64 of `a-z 0-9 - _`, starting with a letter or a
/// digit. Checked here too, so no account name can pass for an option of the command.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Runs a call with the password on stdin; a failure is the sidecar's error text, and an
/// error text that holds the password is [`COMMAND_FAILED`].
fn run(spawn: &SpawnFn, args: &[String], password: &Password) -> Result<(), String> {
    let result = match process::run_with_password(spawn, args, password, SETUP_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(process::error_text(&output.stderr, output.status)),
        Err(error) => Err(error),
    };
    result.map_err(|text| without_password(text, password))
}

/// `text`, or [`COMMAND_FAILED`] when it holds the password. A password that is not one line
/// never reached the sidecar, so its text cannot be in the answer.
fn without_password(text: String, password: &Password) -> String {
    match password.one_line() {
        Ok(line) if text.contains(line) => COMMAND_FAILED.to_owned(),
        _ => text,
    }
}

/// Connects the pipeline in `data_dir` to the server at `url` (its address without
/// `/rest/v1`): `connect <url> --data <dir> --email <email> --password-stdin --name
/// "Notedown for Mac pipeline"`, within [`SETUP_TIMEOUT`]. The CLI writes `pipeline.toml` and
/// the pipeline's read token.
pub fn connect_pipeline(
    spawn: &SpawnFn,
    url: &str,
    data_dir: &Path,
    email: &str,
    password: &Password,
) -> Result<(), String> {
    password.one_line()?;
    let url = url.trim().trim_end_matches('/');
    let url = url
        .strip_suffix("/rest/v1")
        .unwrap_or(url)
        .trim_end_matches('/');
    let args = vec![
        "connect".to_owned(),
        url.to_owned(),
        "--data".to_owned(),
        data_dir.display().to_string(),
        "--email".to_owned(),
        email.trim().to_owned(),
        "--password-stdin".to_owned(),
        "--name".to_owned(),
        PIPELINE_NAME.to_owned(),
    ];
    run(spawn, &args, password)
}

/// The new account on this Mac, then a login with it as Log in does. When the account was
/// made but the login failed, the answer says so: the form then offers Log in.
#[allow(clippy::too_many_arguments)]
pub fn create_and_log_in(
    spawn: &SpawnFn,
    gate: &LoginGate,
    state: &ConfigState,
    serve_info: &ServeInfo,
    store: &dyn TokenStore,
    library: &Library,
    slug: &str,
    email: &str,
    password: &Password,
) -> Result<ConfigAnswer, String> {
    let config = state.current();
    match config.mode {
        Mode::Local => {}
        Mode::Remote => return Err(NOT_HERE.to_owned()),
        Mode::NotSet => return Err(login::NO_MODE.to_owned()),
    }
    create_account(spawn, &config.data_dir, slug, email, password)?;
    log_in_to(gate, state, serve_info, store, library, email, password)
        .map_err(|_| ACCOUNT_MADE.to_owned())
}

/// The pipeline connection for the config: its server address, its folder and its e-mail.
pub fn connect_configured(
    spawn: &SpawnFn,
    config: &AppConfig,
    password: &Password,
) -> Result<(), String> {
    let url = match (config.mode, config.server_url.as_deref()) {
        (Mode::Remote, Some(url)) => url,
        _ => return Err(ONLY_REMOTE.to_owned()),
    };
    let Some(email) = config.email.as_deref().filter(|e| !e.trim().is_empty()) else {
        return Err(LOG_IN_FIRST.to_owned());
    };
    connect_pipeline(spawn, url, &config.data_dir, email, password)
}

/// A data folder on this Mac that holds a server, a pipeline or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingFolder {
    pub path: PathBuf,
    pub server: Option<ServerFolder>,
    pub pipeline: Option<PipelineFolder>,
}

/// The server part of a folder: its accounts, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerFolder {
    pub accounts: Vec<ExistingAccount>,
}

/// One account of a server folder, from its `account.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingAccount {
    pub slug: String,
    pub email: String,
    /// `cloud import` of a device backup is done.
    pub imported: bool,
}

/// The pipeline part of a folder: the server it reads from (`[server] url` of
/// `pipeline.toml`), when the file names one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineFolder {
    pub url: Option<String>,
}

/// What each folder holds: only folders with a server or a pipeline, each path once.
pub fn existing_details(dirs: &[PathBuf]) -> Vec<ExistingFolder> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut found = Vec::new();
    for dir in dirs {
        // The same folder written another way (a link, `/tmp` and `/private/tmp`) is one.
        let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        let what = existing_setup(dir);
        let server = what.server.then(|| ServerFolder {
            accounts: accounts_in(dir),
        });
        let pipeline = what.pipeline.then(|| PipelineFolder {
            url: pipeline_server_url(dir),
        });
        if server.is_some() || pipeline.is_some() {
            found.push(ExistingFolder {
                path: dir.clone(),
                server,
                pipeline,
            });
        }
    }
    found
}

/// `account.json` as far as the setup reads it; the other fields (the password hash among
/// them) are never read.
#[derive(Deserialize)]
struct AccountFile {
    slug: String,
    email: String,
    #[serde(default)]
    imported: Option<serde_json::Value>,
}

/// The accounts of the server folder `dir`, by name; a file that is not readable is skipped.
fn accounts_in(dir: &Path) -> Vec<ExistingAccount> {
    let Ok(entries) = std::fs::read_dir(dir.join("accounts")) else {
        return Vec::new();
    };
    let mut accounts: Vec<ExistingAccount> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let text = std::fs::read_to_string(entry.path().join("account.json")).ok()?;
            let file: AccountFile = serde_json::from_str(&text).ok()?;
            Some(ExistingAccount {
                slug: file.slug,
                email: file.email,
                imported: file.imported.is_some_and(|v| !v.is_null()),
            })
        })
        .collect();
    accounts.sort_by(|a, b| a.slug.cmp(&b.slug));
    accounts
}

/// The folders the setup looks in: the config's data folder, `~/notedown-data` and
/// `~/notedown-mac` (the docs' pipeline folder).
pub fn setup_folders(config: &AppConfig) -> Vec<PathBuf> {
    let mut dirs = vec![config.data_dir.clone(), crate::config::default_data_dir()];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("notedown-mac"));
    }
    dirs
}

#[cfg(test)]
mod tests;
