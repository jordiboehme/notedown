//! notedown-cloud: a file-based Supernote Private Cloud server.
//!
//! This crate currently holds the configuration, the always-on protocol log, the accounts, login
//! codes and tokens, and the HTTP routes of the capture server (login, server queries, refusal of
//! every file operation).

pub(crate) mod fsutil;

pub mod api;
pub mod auth;
pub mod config;
pub mod diag;
pub mod server;

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
    #[error("invalid {what}: {message}")]
    Invalid { what: &'static str, message: String },
    #[error("account {0} already exists")]
    AccountExists(String),
    #[error("account {0} does not exist")]
    NoAccount(String),
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        source: std::io::Error,
    },
    #[error("server on {addr} stopped: {source}")]
    Serve {
        addr: String,
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
