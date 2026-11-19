// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
    /// The page gave no readable block.
    #[error("no readable block on the page: {reason}")]
    Unreadable { reason: String },
    /// The model runtime did not do what it should (image size or image tokens).
    #[error("OCR runtime error: {0}")]
    Runtime(String),
}
