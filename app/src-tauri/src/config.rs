//! The app config: a small JSON file in the app config dir.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "config.json";
const BROKEN: &str = "config.json.broken";
const TEMP: &str = "config.json.tmp";

/// Where the notes live and which server the app talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Nothing chosen yet: no child runs.
    #[default]
    NotSet,
    /// The server runs on this Mac, started by the app.
    Local,
    /// The server runs on another machine.
    Remote,
}

/// How the library shows folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartView {
    #[default]
    Tree,
    Columns,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    pub version: u32,
    pub mode: Mode,
    pub data_dir: PathBuf,
    pub server_url: Option<String>,
    pub account: Option<String>,
    pub email: Option<String>,
    pub start_view: StartView,
}

/// `~/notedown-data`, or `./notedown-data` when there is no home folder.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("notedown-data")
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            mode: Mode::NotSet,
            data_dir: default_data_dir(),
            server_url: None,
            account: None,
            email: None,
            start_view: StartView::Tree,
        }
    }
}

/// What a data folder already holds, so the setup can adopt it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingSetup {
    /// `server.toml` or `accounts/` is there.
    pub server: bool,
    /// `pipeline.toml` is there.
    pub pipeline: bool,
}

pub fn existing_setup(dir: &Path) -> ExistingSetup {
    ExistingSetup {
        server: dir.join("server.toml").is_file() || dir.join("accounts").is_dir(),
        pipeline: dir.join("pipeline.toml").is_file(),
    }
}

/// Reads `<dir>/config.json`. A missing file gives the defaults. An unreadable one is renamed to
/// `config.json.broken` (so it can be looked at) and the defaults are returned.
pub fn load(dir: &Path) -> AppConfig {
    let path = dir.join(FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return AppConfig::default(),
        Err(_) => return keep_aside(dir),
    };
    match serde_json::from_str(&text) {
        Ok(config) => config,
        Err(_) => keep_aside(dir),
    }
}

fn keep_aside(dir: &Path) -> AppConfig {
    // Best effort: if the rename fails, the next save overwrites the file anyway.
    let _ = fs::rename(dir.join(FILE), dir.join(BROKEN));
    AppConfig::default()
}

/// Writes `<dir>/config.json` through a temp file and a rename, so a crash never leaves half a
/// file.
pub fn save(dir: &Path, config: &AppConfig) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let temp = dir.join(TEMP);
    let json = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    fs::write(&temp, json)?;
    fs::rename(&temp, dir.join(FILE)).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppConfig {
        AppConfig {
            version: 1,
            mode: Mode::Remote,
            data_dir: PathBuf::from("/data/notes"),
            server_url: Some("https://notes.example.org".into()),
            account: Some("jordi".into()),
            email: Some("jordi@example.org".into()),
            start_view: StartView::Columns,
        }
    }

    #[test]
    fn a_missing_config_is_not_set_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = load(dir.path());
        assert_eq!(config.mode, Mode::NotSet);
        assert_eq!(config.start_view, StartView::Tree);
        assert!(config.data_dir.ends_with("notedown-data"));
        assert_eq!(config.server_url, None);
        assert_eq!(config.account, None);
    }

    #[test]
    fn a_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &sample()).unwrap();
        assert_eq!(load(dir.path()), sample());
    }

    #[test]
    fn a_broken_config_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), "{ not json").unwrap();
        let config = load(dir.path());
        assert_eq!(config.mode, Mode::NotSet);
        assert!(!dir.path().join("config.json").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json.broken")).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn save_writes_through_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A stale temp file from a crash is replaced, and none is left behind.
        fs::write(dir.path().join("config.json.tmp"), "stale").unwrap();
        save(dir.path(), &sample()).unwrap();
        assert!(!dir.path().join("config.json.tmp").exists());
        let text = fs::read_to_string(dir.path().join("config.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["mode"], "remote");
        assert_eq!(value["startView"], "columns");
        // A second save replaces the first.
        let mut next = sample();
        next.mode = Mode::Local;
        save(dir.path(), &next).unwrap();
        assert_eq!(load(dir.path()).mode, Mode::Local);
    }

    #[test]
    fn existing_setup_sees_server_and_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let none = existing_setup(dir.path());
        assert!(!none.server && !none.pipeline);

        fs::write(dir.path().join("server.toml"), "").unwrap();
        let server = existing_setup(dir.path());
        assert!(server.server && !server.pipeline);

        fs::remove_file(dir.path().join("server.toml")).unwrap();
        fs::create_dir(dir.path().join("accounts")).unwrap();
        fs::write(dir.path().join("pipeline.toml"), "").unwrap();
        let both = existing_setup(dir.path());
        assert!(both.server && both.pipeline);
    }
}
