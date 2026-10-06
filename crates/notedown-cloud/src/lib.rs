//! notedown-cloud: a file-based Supernote Private Cloud server.
//!
//! This crate currently holds the configuration, the always-on protocol log, the accounts, login
//! codes and tokens, the HTTP routes (login, server queries, in accept mode the sync session,
//! listing, queries, signed downloads and uploads, refusal of every other file operation), the
//! auto-sync socket, the file store of each account with its recycle bin and upload staging, the
//! change journal and the page index of notes.

pub mod fsutil;

pub mod api;
pub mod auth;
pub mod config;
pub mod device_backup;
pub mod diag;
pub mod import;
pub mod journal;
pub mod pages;
pub mod push;
pub mod put;
pub mod server;
pub mod socket;
pub mod store;
pub mod sync;

pub use config::{Config, PushMode, SyncMode};
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
    #[error("email {email} is already used by account {slug}")]
    EmailInUse { email: String, slug: String },
    #[error("account {0} does not exist")]
    NoAccount(String),
    #[error(
        "{new:?} collides with {existing:?}: the names differ only in case or Unicode normalization"
    )]
    NameCollision { existing: String, new: String },
    /// The protect gate of the account closed while a request waited for the store lock (an
    /// import cleared the `imported` marker). Routes answer `E0079` with `protect_refused`.
    #[error("the protect gate of account {0} is closed (no imported marker)")]
    GateClosed(String),
    /// The tree of an imported account is not what the import left: `files/` is missing, a
    /// symlink or no folder, or a native folder is missing. The server never answers a listing
    /// from such a tree (the device would delete its own files); routes answer `E0079` with
    /// `store_damaged`.
    #[error("the store of account {slug} is damaged: {reason}")]
    StoreDamaged { slug: String, reason: String },
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
