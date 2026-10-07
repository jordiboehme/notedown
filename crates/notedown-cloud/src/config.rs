//! `<data>/server.toml`: created with defaults on first start.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::retention::Retention;
use crate::{Error, Result, io_err};

pub const CONFIG_FILE: &str = "server.toml";

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
    /// Close the device's socket. The device connects again within seconds and runs a full
    /// sync on every new connection (`docs/sync-protocol.md` 9.3).
    #[default]
    Reconnect,
    /// One `STARTSYNC` item, queued and sent after the device's next heartbeat reply as Ratta
    /// sends its pushes (`docs/sync-protocol.md` 4.4). Firmware 3.29.42 acknowledges it but does
    /// not sync.
    Startsync,
    /// One item per changed file or folder in Ratta's form, queued and sent after the device's
    /// next heartbeat reply, again until the device answers `Received` (4.4). Falls back to
    /// `startsync` when the items cannot be built or the list of changes was cut at its cap.
    Detail,
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
    /// `"reconnect"` (default) closes a connected device's socket after a change it did not make
    /// itself (a `notedown cloud put`, another device's upload), so it connects again and syncs.
    /// `"startsync"` sends one `STARTSYNC` message, `"detail"` sends the changed items instead,
    /// both after the device's next heartbeat; `"off"` sends nothing.
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: vec![SocketAddr::from(([0, 0, 0, 0], 19072))],
            external_base_url: None,
            trusted_proxies: Vec::new(),
            protect: true,
            sync: SyncMode::Accept,
            push: PushMode::Reconnect,
            capture: false,
            log_retention_days: 30,
            log_max_bytes: 1024 * 1024 * 1024,
            recycle_prune: true,
            recycle_keep_latest: 3,
            recycle_keep_daily: 7,
            recycle_keep_monthly: 12,
            recycle_keep_deleted_days: 90,
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
        let path = Self::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config: Config = toml::from_str(&text).map_err(|e| Error::Config {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
                config.validate(&path)?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.save(data_dir)?;
                Ok(config)
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
        ] {
            assert!(text.contains(key), "{key} missing in:\n{text}");
        }
    }
}
