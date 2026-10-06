//! Consumer cursors: `journal/consumers/<name>.cursor` with the last fully processed `seq`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, io_err};

/// `{"seq": N, "updatedAt": ms}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    /// The last fully processed `seq`. A consumer continues at `seq + 1`.
    pub seq: u64,
    /// When the cursor was written, in ms since the epoch.
    pub updated_at: u64,
}

/// A consumer name: 1 to 64 characters of `a-z`, `0-9`, `-` and `_`.
pub fn validate_consumer(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid {
            what: "consumer name",
            message: format!("{name:?}: use 1 to 64 of a-z, 0-9, - and _"),
        })
    }
}

pub(crate) fn cursor_path(consumers: &Path, name: &str) -> Result<PathBuf> {
    validate_consumer(name)?;
    Ok(consumers.join(format!("{name}.cursor")))
}

pub(crate) fn read(path: &Path) -> Result<Option<Cursor>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(path)(e)),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::Config {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
}

pub(crate) fn write(path: &Path, cursor: &Cursor) -> Result<()> {
    crate::fsutil::write_atomic(path, &crate::fsutil::json_bytes(cursor), false)
}
