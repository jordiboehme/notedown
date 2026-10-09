//! The notedown pipeline: page text, changes for agents and the config that drives them.

pub mod compose;
pub mod config;
pub mod meta;
pub mod model;
pub mod ocr;
pub mod page;
pub mod region;
pub mod store;
pub mod text;
pub mod toon;

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
    #[error(transparent)]
    Cloud(#[from] notedown_cloud::Error),
    #[error("another notedown pipeline runs for this data folder")]
    WriterBusy,
    #[error("the text store needs a repair before it is written")]
    NeedsRepair,
    #[error("invalid {what}: {message}")]
    Invalid { what: &'static str, message: String },
    #[error("{path}: {message}")]
    Corrupt {
        path: std::path::PathBuf,
        message: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
