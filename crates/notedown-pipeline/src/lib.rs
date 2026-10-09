//! The notedown pipeline: page text, changes for agents and the config that drives them.

pub mod config;
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
}

pub type Result<T> = std::result::Result<T, Error>;
