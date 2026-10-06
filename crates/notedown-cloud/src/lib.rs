//! notedown-cloud: a file-based Supernote Private Cloud server.
//!
//! This crate currently holds the configuration and the always-on protocol log.

pub mod config;
pub mod diag;

pub use config::Config;
pub use diag::{Clock, LogEntry, LogKind, ProtocolLog, SystemClock};

/// Errors of the cloud crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error on {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path}: {message}")]
    Config {
        path: std::path::PathBuf,
        message: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
