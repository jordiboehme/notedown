//! `<data>/server.toml`: created with defaults on first start.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::retention::Retention;
use crate::{Error, Result, io_err};

pub const CONFIG_FILE: &str = "server.toml";

/// The render folder in the data folder when `render_dir` is not set.
pub const DEFAULT_RENDER_DIR: &str = "share";

/// The comment above `capture` in the written `server.toml`.
pub const CAPTURE_COMMENT: &str = "# For one capture session only. The dumps in capture/ have no size cap \
     and no rotation.\n# Prefer `notedown serve --capture`, which counts for one run.";

/// Whether the server exchanges files with the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    /// Serve the file sync routes that are implemented; refuse the rest.
    #[default]
    Accept,
    /// Refuse every file and oss request with `E0079`: capture only, no files are exchanged.
    Refuse,
}

/// What the server pushes to connected devices after a server-side change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushMode {
    /// One item per changed file or folder in Ratta's form, queued and sent after the device's
    /// next heartbeat reply, again until the device answers `Received`
    /// (`docs/sync-protocol.md` 4.4). Every pending change is sent, collapsed per file or
    /// folder. When the items can't be built the push waits and is tried again. The value
    /// `"reconnect"` of older versions reads as this one ([`Config::load_or_create`] warns).
    #[default]
    #[serde(alias = "reconnect")]
    Detail,
    /// One `STARTSYNC` item, queued and sent after the device's next heartbeat reply as Ratta
    /// sends its pushes (4.4). Firmware 3.29.42 acknowledges it but does not sync.
    Startsync,
    /// No pushes; the device syncs on its own schedule.
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Addresses to listen on.
    pub listen: Vec<SocketAddr>,
    /// Base URL the device must use to reach the server (behind a reverse proxy).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_base_url: Option<String>,
    /// Reverse proxies whose `X-Forwarded-For` header names the client address. Empty: the
    /// header is ignored and the peer address counts.
    pub trusted_proxies: Vec<IpAddr>,
    /// Protect mode (default on). Every file request is refused (`E0079`) until
    /// `notedown cloud import` has filled the account and set its `imported` marker. After that,
    /// device deletes, moves and overwrites are logged and carried out, and deleted or
    /// overwritten bytes are kept in the recycle bin.
    pub protect: bool,
    /// `"accept"` (default) exchanges files with the device. `"refuse"` answers every sync
    /// with a busy error (capture only). `notedown serve --refuse-sync` sets it for one run.
    pub sync: SyncMode,
    /// What a connected device gets after a change it did not make itself (a
    /// `notedown cloud put`, another device's upload), after its next heartbeat. `"detail"`
    /// (default) sends one item per changed file or folder, `"startsync"` one `STARTSYNC`
    /// message, `"off"` nothing.
    pub push: PushMode,
    /// Write full, masked dumps of every request and frame to `capture/`. For one capture
    /// session only: the dumps have no size cap and no rotation. The `--capture` flag of
    /// `serve` is the usual way; it counts for one run.
    pub capture: bool,
    /// Protocol log files older than this many days are deleted.
    pub log_retention_days: u32,
    /// Cap for the total size of `logs/` in bytes.
    pub log_max_bytes: u64,
    /// The server prunes the recycle bin by the `recycle_keep_*` rules (hourly).
    pub recycle_prune: bool,
    /// Newest recycled versions kept per file, whatever their age. At least 1.
    pub recycle_keep_latest: u32,
    /// Local days (back from today) whose end keeps the version a file had then.
    pub recycle_keep_daily: u32,
    /// Local months (back from this month) whose end keeps the version a file had then.
    pub recycle_keep_monthly: u32,
    /// A deleted file whose newest recycled line is older than this many days is removed
    /// completely. At least 1.
    pub recycle_keep_deleted_days: u32,
    /// Seconds between two scans of the data folder for changes made by hand (each account,
    /// skipped while a device syncs). The first scan runs at start. 0 turns the timer off; a
    /// device sync and a device listing still scan first.
    pub scan_interval: u64,
    /// A scan that would remove files holds instead when fewer than this many files would be
    /// left in the account (an empty data folder, an unmounted volume): device sync waits until
    /// the files are back or `notedown cloud scan --accept` runs. 0 never holds.
    pub scan_min_files: u64,
    /// Watch the data folder for files changed by hand and scan the changed folders after 2 s
    /// without a new change. The timer scan (`scan_interval`) stays as a backstop. false turns
    /// the watch off.
    pub watch: bool,
    /// A folder where the server keeps a PDF of every note, one subfolder per account. Not
    /// set: `<data>/share` ([`DEFAULT_RENDER_DIR`]), created when needed. Empty: nothing is
    /// rendered. Otherwise an absolute path or one that starts with `~` (the home folder),
    /// outside the data folder; that folder must exist, the server creates only the account
    /// subfolders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: vec![SocketAddr::from(([0, 0, 0, 0], 19072))],
            external_base_url: None,
            trusted_proxies: Vec::new(),
            protect: true,
            sync: SyncMode::Accept,
            push: PushMode::Detail,
            capture: false,
            log_retention_days: 30,
            log_max_bytes: 1024 * 1024 * 1024,
            recycle_prune: true,
            recycle_keep_latest: 3,
            recycle_keep_daily: 7,
            recycle_keep_monthly: 12,
            recycle_keep_deleted_days: 90,
            scan_interval: 300,
            scan_min_files: 1,
            watch: true,
            render_dir: None,
        }
    }
}

impl Config {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(CONFIG_FILE)
    }

    /// Load `<data>/server.toml`, or the defaults when it does not exist. Writes nothing.
    pub fn load_or_default(data_dir: &Path) -> Result<Config> {
        if Self::path(data_dir).exists() {
            Self::load_or_create(data_dir)
        } else {
            Ok(Config::default())
        }
    }

    /// Load `<data>/server.toml`; write the defaults when it does not exist yet.
    pub fn load_or_create(data_dir: &Path) -> Result<Config> {
        Ok(Self::load_with_warnings(data_dir)?.0)
    }

    /// [`Config::load_or_create`], plus the warnings about the file for `notedown serve` to
    /// print. The file is never rewritten for them.
    pub fn load_with_warnings(data_dir: &Path) -> Result<(Config, Vec<String>)> {
        let path = Self::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config: Config = toml::from_str(&text).map_err(|e| Error::Config {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
                config.validate(&path)?;
                Ok((config, warnings(&text)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.save(data_dir)?;
                Ok((config, Vec::new()))
            }
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// The recycle bin retention settings.
    pub fn retention(&self) -> Retention {
        Retention {
            keep_latest: self.recycle_keep_latest,
            keep_daily: self.recycle_keep_daily,
            keep_monthly: self.recycle_keep_monthly,
            keep_deleted_days: self.recycle_keep_deleted_days,
        }
    }

    /// The time between two timer scans; `None` when `scan_interval` is 0 (no timer).
    pub fn scan_period(&self) -> Option<std::time::Duration> {
        (self.scan_interval > 0).then(|| std::time::Duration::from_secs(self.scan_interval))
    }

    /// The render folder of `render_dir`, resolved: `<data>/share` when it is not set, `None`
    /// when it is empty (rendering is off).
    pub fn render_root(&self, data_dir: &Path) -> Option<PathBuf> {
        let Some(raw) = self.render_dir.as_deref() else {
            return Some(data_dir.join(DEFAULT_RENDER_DIR));
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        // `~` and `~/...` are the home folder; `~user` is not expanded.
        let path = if raw == "~" {
            dirs::home_dir()?
        } else {
            match raw.strip_prefix("~/") {
                Some(rest) => dirs::home_dir()?.join(rest),
                None => PathBuf::from(raw),
            }
        };
        Some(if path.is_absolute() {
            path
        } else {
            data_dir.join(path)
        })
    }

    /// Refuse values the server can't work with. `path` is the file named in the message.
    pub fn validate(&self, path: &Path) -> Result<()> {
        for (key, value) in [
            ("recycle_keep_latest", self.recycle_keep_latest),
            ("recycle_keep_deleted_days", self.recycle_keep_deleted_days),
        ] {
            if value < 1 {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!("{key} must be at least 1"),
                });
            }
        }
        self.validate_render_dir(path)
    }

    /// A set `render_dir` must be absolute (or start with `~`) and lie outside the data folder,
    /// the folder of `path`. A folder that holds the data folder is left to the render pass,
    /// which refuses it only when the account subfolder would hold the data folder.
    fn validate_render_dir(&self, path: &Path) -> Result<()> {
        let refuse = |message: String| Error::Config {
            path: path.to_path_buf(),
            message,
        };
        let Some(raw) = self.render_dir.as_deref().map(str::trim) else {
            return Ok(());
        };
        if raw.is_empty() {
            return Ok(());
        }
        if !(raw == "~" || raw.starts_with("~/") || Path::new(raw).is_absolute()) {
            return Err(refuse(format!(
                "render_dir = \"{raw}\" must be an absolute path or start with ~/; leave it \
                 out to use {DEFAULT_RENDER_DIR}/ in the data folder"
            )));
        }
        // `server.toml` alone has the parent "", the current folder.
        let data = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let Some(root) = self.render_root(data) else {
            return Ok(());
        };
        if crate::fsutil::real_path(&root).starts_with(crate::fsutil::real_path(data)) {
            return Err(refuse(format!(
                "render_dir = \"{raw}\" must be outside the data folder {}; leave it out to \
                 use {DEFAULT_RENDER_DIR}/ in the data folder",
                data.display()
            )));
        }
        Ok(())
    }

    /// Atomic write: unique temp file, fsync, rename, fsync of the folder.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        std::fs::create_dir_all(data_dir).map_err(io_err(data_dir))?;
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let text = text.replacen(
            "\ncapture = ",
            &format!("\n{CAPTURE_COMMENT}\ncapture = "),
            1,
        );
        crate::fsutil::write_atomic(&path, text.as_bytes(), false)
    }
}

/// The warnings about a `server.toml` that loaded: values of older versions that still work.
fn warnings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let table: toml::Table = toml::from_str(text).unwrap_or_default();
    if table.get("push").and_then(|v| v.as_str()) == Some("reconnect") {
        out.push(format!(
            "{CONFIG_FILE}: push = \"reconnect\" is gone, using \"detail\". Remove the line or \
             set push = \"detail\"."
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn load_or_default_reads_the_file_and_never_writes_one() {
        let d = dir();
        assert_eq!(
            Config::load_or_default(d.path()).unwrap(),
            Config::default()
        );
        assert!(!Config::path(d.path()).exists());
        std::fs::write(Config::path(d.path()), "recycle_keep_latest = 5\n").unwrap();
        assert_eq!(
            Config::load_or_default(d.path())
                .unwrap()
                .recycle_keep_latest,
            5
        );
        std::fs::write(Config::path(d.path()), "recycle_keep_latest = 0\n").unwrap();
        assert!(Config::load_or_default(d.path()).is_err());
    }

    #[test]
    fn recycle_defaults() {
        let c = Config::default();
        assert!(c.recycle_prune);
        assert_eq!(
            c.retention(),
            Retention {
                keep_latest: 3,
                keep_daily: 7,
                keep_monthly: 12,
                keep_deleted_days: 90
            }
        );
    }

    #[test]
    fn old_file_gets_the_defaults() {
        let d = dir();
        std::fs::write(Config::path(d.path()), "protect = true\n").unwrap();
        let c = Config::load_or_create(d.path()).unwrap();
        assert_eq!(c.retention(), Config::default().retention());
        assert!(c.recycle_prune);
    }

    #[test]
    fn zero_values_are_refused_with_the_key() {
        for key in ["recycle_keep_latest", "recycle_keep_deleted_days"] {
            let d = dir();
            std::fs::write(Config::path(d.path()), format!("{key} = 0\n")).unwrap();
            let err = Config::load_or_create(d.path()).unwrap_err().to_string();
            assert!(err.contains(key), "{err}");
            assert!(err.contains("must be at least 1"), "{err}");
        }
    }

    #[test]
    fn written_default_names_the_keys() {
        let d = dir();
        Config::load_or_create(d.path()).unwrap();
        let text = std::fs::read_to_string(Config::path(d.path())).unwrap();
        for key in [
            "recycle_prune = true",
            "recycle_keep_latest = 3",
            "recycle_keep_daily = 7",
            "recycle_keep_monthly = 12",
            "recycle_keep_deleted_days = 90",
            "scan_interval = 300",
            "scan_min_files = 1",
            "watch = true",
        ] {
            assert!(text.contains(key), "{key} missing in:\n{text}");
        }
        assert!(!text.contains("render_dir"), "{text}");
    }

    #[test]
    fn watch_false_loads() {
        let d = dir();
        assert!(Config::default().watch);
        std::fs::write(Config::path(d.path()), "watch = false\n").unwrap();
        assert!(!Config::load_or_create(d.path()).unwrap().watch);
    }

    fn render_root_of(value: Option<&str>) -> Option<PathBuf> {
        let c = Config {
            render_dir: value.map(str::to_string),
            ..Config::default()
        };
        c.render_root(Path::new("/data"))
    }

    #[test]
    fn render_root_resolves_render_dir() {
        assert_eq!(
            render_root_of(Some("~/x")),
            Some(dirs::home_dir().unwrap().join("x"))
        );
        assert_eq!(render_root_of(Some("/abs")), Some(PathBuf::from("/abs")));
        assert_eq!(render_root_of(Some("~")), dirs::home_dir());
        assert_eq!(render_root_of(Some(" ~ ")), dirs::home_dir());
        // Not set: the default folder in the data folder.
        assert_eq!(render_root_of(None), Some(PathBuf::from("/data/share")));
        // Empty: rendering is off.
        assert_eq!(render_root_of(Some("")), None);
        assert_eq!(render_root_of(Some("  ")), None);
    }

    fn validate_render(data: &Path, value: &str) -> Result<()> {
        Config {
            render_dir: Some(value.to_string()),
            ..Config::default()
        }
        .validate(&Config::path(data))
    }

    #[test]
    fn a_relative_render_dir_is_refused() {
        let d = dir();
        for value in ["out", "share", "~other/x", "./x"] {
            let err = validate_render(d.path(), value).unwrap_err().to_string();
            assert!(err.contains("absolute"), "{value}: {err}");
        }
    }

    #[test]
    fn a_render_dir_in_the_data_folder_is_refused() {
        let d = dir();
        let data = d.path().join("data");
        std::fs::create_dir_all(data.join("share")).unwrap();
        for inside in [data.clone(), data.join("share"), data.join("pdf")] {
            let err = validate_render(&data, &inside.to_string_lossy())
                .unwrap_err()
                .to_string();
            assert!(err.contains("outside the data folder"), "{inside:?}: {err}");
        }
        // Through a symlink to the data folder too.
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        let err = validate_render(&data, &link.join("pdf").to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the data folder"), "{err}");
    }

    #[test]
    fn a_render_dir_outside_the_data_folder_is_accepted() {
        let d = dir();
        let data = d.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        // A sibling whose name starts with the data folder's name is outside.
        let sibling = d.path().join("data-share");
        assert!(validate_render(&data, &sibling.to_string_lossy()).is_ok());
        assert!(validate_render(&data, "").is_ok());
        assert!(validate_render(&data, "~/PDFs").is_ok());
        // A folder that holds the data folder is the render pass's business (the home folder).
        assert!(validate_render(&data, &d.path().to_string_lossy()).is_ok());
    }

    #[test]
    fn render_dir_is_read_from_the_file() {
        let d = dir();
        std::fs::write(Config::path(d.path()), "render_dir = \"/abs/pdf\"\n").unwrap();
        let c = Config::load_or_default(d.path()).unwrap();
        assert_eq!(c.render_root(d.path()), Some(PathBuf::from("/abs/pdf")));
        std::fs::write(Config::path(d.path()), "render_dir = \"pdf\"\n").unwrap();
        assert!(Config::load_or_default(d.path()).is_err());
    }
}
