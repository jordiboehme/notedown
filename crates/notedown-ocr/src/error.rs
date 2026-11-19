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
}
