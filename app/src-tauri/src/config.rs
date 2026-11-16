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
    /// The file format, 1 for now. Nothing checks it yet; a later format change will.
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

/// What `load_checked` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub config: AppConfig,
    /// The file was unreadable as a config and was kept aside as `config.json.broken`.
    pub reset: bool,
}

pub fn load(dir: &Path) -> AppConfig {
    load_checked(dir).config
}

/// Reads `<dir>/config.json`. A missing file gives the defaults. A file that is not valid UTF-8
/// or not a valid config is renamed to `config.json.broken` (an older one is replaced, so the
/// newest wins), a line goes to stderr, and `reset` is set. Any other read error (permissions, a
/// failing disk) gives the defaults and leaves the file alone.
pub fn load_checked(dir: &Path) -> Loaded {
    let path = dir.join(FILE);
    let parsed = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return not_reset(),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(e.to_string()),
        Err(e) => {
            eprintln!(
                "notedown: the config {} could not be read, using defaults: {e}",
                path.display()
            );
            return not_reset();
        }
    };
    match parsed {
        Ok(config) => Loaded {
            config,
            reset: false,
        },
        Err(why) => {
            // Best effort: if the rename fails, the next save replaces the file anyway.
            let _ = fs::rename(&path, dir.join(BROKEN));
            eprintln!(
                "notedown: the config {} is broken ({why}); kept as {BROKEN}, using defaults",
                path.display()
            );
            Loaded {
                config: AppConfig::default(),
                reset: true,
            }
        }
    }
}

fn not_reset() -> Loaded {
    Loaded {
        config: AppConfig::default(),
        reset: false,
    }
}

/// Writes `<dir>/config.json` durably: the new content goes to a temp file that is flushed to
/// disk (`sync_all`, a full fsync on macOS), then renamed over the old file, then the folder is
/// flushed too. A crash leaves the old file or the new one, never half of one. A failed flush
/// of the folder after the rename is logged, not an error.
pub fn save(dir: &Path, config: &AppConfig) -> io::Result<()> {
    use std::io::Write;
    fs::create_dir_all(dir)?;
    let temp = dir.join(TEMP);
    let json = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(FILE))
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    sync_folder(dir);
    Ok(())
}

/// Flushes the rename to disk. The new file is in place already, so a failure here is only
/// logged: the save worked, it may just not survive a power cut.
fn sync_folder(dir: &Path) {
    if let Err(e) = fs::File::open(dir).and_then(|folder| folder.sync_all()) {
        eprintln!(
            "notedown: the config folder {} could not be flushed: {e}",
            dir.display()
        );
    }
}

/// The address without a user or a password in it, for a status line or the Keychain key.
/// Everything else stays as written (no parser that changes the case or adds a slash), so an
/// address without a login comes back unchanged.
pub fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{tail}"),
        None => url.to_owned(),
    }
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
    fn the_login_part_of_an_address_is_dropped() {
        for (url, shown) in [
            ("https://user:secret@n.example.org", "https://n.example.org"),
            ("https://user@n.example.org/x", "https://n.example.org/x"),
            (
                "http://u:p@pi.local:19072/rest/v1?a=b",
                "http://pi.local:19072/rest/v1?a=b",
            ),
            // Unchanged without a login, also the case and a trailing slash.
            ("https://Notes.Example.org/", "https://Notes.Example.org/"),
            (
                "https://n.example.org/x?to=a@b",
                "https://n.example.org/x?to=a@b",
            ),
            ("https://n.example.org#a@b", "https://n.example.org#a@b"),
            ("no address", "no address"),
        ] {
            assert_eq!(without_userinfo(url), shown, "{url}");
        }
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
    fn a_failed_folder_sync_after_the_rename_is_no_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("config");
        fs::create_dir(&folder).unwrap();
        // Write and search, but no read: the file goes in, the folder cannot be opened.
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o300)).unwrap();
        let saved = save(&folder, &sample());
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        saved.unwrap();
        assert_eq!(load(&folder), sample());
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

    #[test]
    fn a_partial_config_loads_through_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), r#"{"mode":"local"}"#).unwrap();
        let loaded = load_checked(dir.path());
        assert!(!loaded.reset);
        assert_eq!(loaded.config.mode, Mode::Local);
        assert_eq!(loaded.config.start_view, StartView::Tree);
        assert!(loaded.config.data_dir.ends_with("notedown-data"));
    }

    #[test]
    fn the_default_folder_is_absolute_under_home() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let dir = default_data_dir();
        assert!(dir.is_absolute());
        assert!(dir.starts_with(&home));
        assert_eq!(dir, home.join("notedown-data"));
    }

    #[test]
    fn a_read_error_keeps_the_file_and_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        // A folder where the file should be: reading fails, but not with NotFound.
        fs::create_dir(dir.path().join("config.json")).unwrap();
        let loaded = load_checked(dir.path());
        assert!(!loaded.reset);
        assert_eq!(loaded.config.mode, Mode::NotSet);
        assert!(dir.path().join("config.json").is_dir());
        assert!(!dir.path().join("config.json.broken").exists());
    }

    #[test]
    fn invalid_utf8_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), [0xff, 0xfe, 0x00]).unwrap();
        let loaded = load_checked(dir.path());
        assert!(loaded.reset);
        assert!(dir.path().join("config.json.broken").is_file());
    }

    #[test]
    fn the_newest_broken_file_wins_and_reset_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json.broken"), "older").unwrap();
        fs::write(dir.path().join("config.json"), "newer {").unwrap();
        let loaded = load_checked(dir.path());
        assert!(loaded.reset);
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json.broken")).unwrap(),
            "newer {"
        );
        // A healthy file reports no reset.
        save(dir.path(), &sample()).unwrap();
        assert!(!load_checked(dir.path()).reset);
    }
}
