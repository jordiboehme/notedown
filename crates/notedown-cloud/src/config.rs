//! `<data>/server.toml`: created with defaults on first start.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, io_err};

pub const CONFIG_FILE: &str = "server.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Addresses to listen on.
    pub listen: Vec<SocketAddr>,
    /// Base URL the device must use to reach the server (behind a reverse proxy).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_base_url: Option<String>,
    /// Log and refuse device deletes, moves and overwrites.
    pub protect: bool,
    /// Write full, masked dumps of every request and frame to `capture/`.
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
            protect: true,
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

    /// Atomic write: temp file, fsync, rename.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        std::fs::create_dir_all(data_dir).map_err(io_err(data_dir))?;
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let tmp = data_dir.join(format!(".{CONFIG_FILE}.tmp"));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        write().map_err(io_err(&path))
    }
}
