//! The notedown pipeline: page text, changes for agents and the config that drives them.

pub mod compose;
pub mod config;
pub mod connect;
pub mod cursor;
pub mod delta;
pub mod errors;
pub mod format;
pub mod markdown;
pub mod mcp;
pub mod meta;
pub mod model;
pub mod ocr;
pub mod page;
pub mod records;
pub mod region;
pub mod rest;
pub mod retention;
pub mod scope;
pub mod search;
pub mod setup;
pub mod source;
pub mod status;
pub mod store;
pub mod text;
pub mod toon;
pub mod worker;

pub use config::PipelineConfig;

/// Errors of the pipeline crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {message}")]
    Config {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("stroke encoding: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("note: {0}")]
    Note(#[from] notedown_note::NoteError),
    #[error(transparent)]
    Ocr(#[from] notedown_ocr::OcrError),
    #[error("{0}")]
    Read(String),
    /// The server refused the token of an account (401); a new login fixes it. The text says
    /// how.
    #[error("{0}")]
    TokenRefused(String),
    /// A data folder that is the server's own and has no `accounts/` yet.
    #[error("no accounts in {} yet; create one with `notedown cloud account add`", .0.display())]
    NoAccounts(std::path::PathBuf),
    #[error(transparent)]
    Cloud(#[from] notedown_cloud::Error),
    #[error("another notedown pipeline runs for this data folder")]
    WriterBusy,
    #[error("the OCR model is missing, run `notedown model pull`")]
    ModelMissing,
    #[error("the text store needs a repair before it is written")]
    NeedsRepair,
    /// The account of an MCP session cannot be chosen; the text says why.
    #[error("{0}")]
    Account(String),
    #[error("mcp: {0}")]
    Mcp(String),
    /// `notedown connect` failed; the text says why.
    #[error("{0}")]
    Connect(String),
    #[error("invalid {what}: {message}")]
    Invalid { what: &'static str, message: String },
    #[error("{path}: {message}")]
    Corrupt {
        path: std::path::PathBuf,
        message: String,
    },
}

impl Error {
    /// What the error is, for a program to act on (the code of a [`store::LastError`]).
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Error::TokenRefused(_) => Some(store::TOKEN_REFUSED),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
