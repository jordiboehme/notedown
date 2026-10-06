use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("model file not found: {}. Run `notedown model pull` to download it", path.display())]
    ModelMissing { path: PathBuf },
    #[error("checksum mismatch for {}: expected {expected}, got {actual}", path.display())]
    Checksum {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("download failed: {0}")]
    Download(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("OCR engine error: {0}")]
    Engine(String),
}
