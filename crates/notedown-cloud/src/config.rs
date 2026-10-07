//! `<data>/server.toml`: created with defaults on first start.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// One `STARTSYNC` message. Firmware 3.29.42 acknowledges it but does not sync.
    Startsync,
    /// One item per changed file or folder. Falls back to `startsync` when the message cannot be
    /// built or the list of changes was cut at its cap. Firmware 3.29.42 does not sync on it.
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
    /// `"startsync"` (default) tells a connected device to sync after a change it did not make
    /// itself (a `notedown cloud put`, another device's upload). `"detail"` sends the changed
    /// items instead, `"off"` sends nothing.
    pub push: PushMode,
    /// Write full, masked dumps of every request and frame to `capture/`. For one capture
    /// session only: the dumps have no size cap and no rotation. The `--capture` flag of
    /// `serve` is the usual way; it counts for one run.
    pub capture: bool,
    /// Protocol log files older than this many days are deleted.
    pub log_retention_days: u32,
    /// Cap for the total size of `logs/` in bytes.
    pub log_max_bytes: u64,
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
        }
    }
}

impl Config {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(CONFIG_FILE)
    }

    /// Load `<data>/server.toml`; write the defaults when it does not exist yet.
    pub fn load_or_create(data_dir: &Path) -> Result<Config> {
        let path = Self::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| Error::Config {
                path,
                message: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.save(data_dir)?;
                Ok(config)
            }
            Err(e) => Err(io_err(&path)(e)),
        }
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
