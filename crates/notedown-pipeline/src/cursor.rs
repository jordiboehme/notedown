//! The journal cursor of each account the worker follows: `<local>/pipeline/<slug>.cursor`,
//! `{"seq":N,"updatedAt":ms}` with the last `seq` of the account's journal that a good pass
//! has seen. It lives in the pipeline's own folder, never in the server's.

use std::path::{Path, PathBuf};

use notedown_cloud::journal::{Cursor, Journal};

use crate::store::PIPELINE_DIR;
use crate::{Error, Result};

/// `<local>/pipeline/<slug>.cursor`.
pub fn path(local: &Path, slug: &str) -> PathBuf {
    local.join(PIPELINE_DIR).join(format!("{slug}.cursor"))
}

/// The cursor of account `slug`, 0 when it is missing.
pub fn read(local: &Path, slug: &str) -> Result<u64> {
    Ok(load(local, slug)?.unwrap_or(0))
}

/// The cursor of account `slug`, `None` when there is no cursor file.
pub(crate) fn load(local: &Path, slug: &str) -> Result<Option<u64>> {
    let path = path(local, slug);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let c: Cursor = serde_json::from_str(&text).map_err(|e| Error::Corrupt {
        path,
        message: e.to_string(),
    })?;
    Ok(Some(c.seq))
}

/// Writes the cursor of account `slug`.
pub fn write(local: &Path, slug: &str, seq: u64) -> Result<()> {
    let c = Cursor {
        seq,
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    };
    let mut text = serde_json::to_string_pretty(&c)?;
    text.push('\n');
    notedown_cloud::fsutil::write_atomic(&path(local, slug), text.as_bytes(), false)?;
    Ok(())
}

/// The cursor one machine kept for all accounts before: `<local>/journal/consumers/
/// pipeline.cursor`, `None` when it is missing. Read only; the file stays.
pub(crate) fn old(local: &Path) -> Result<Option<u64>> {
    Ok(Journal::open_no_create(local)
        .cursor(crate::worker::CONSUMER)?
        .map(|c| c.seq))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_is_written_and_read() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read(d.path(), "a").unwrap(), 0);
        assert_eq!(load(d.path(), "a").unwrap(), None);
        write(d.path(), "a", 42).unwrap();
        assert_eq!(read(d.path(), "a").unwrap(), 42);
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path(d.path(), "a")).unwrap()).unwrap();
        assert_eq!(v["seq"], 42);
        assert!(v["updatedAt"].as_u64().unwrap() > 0);
        std::fs::write(path(d.path(), "b"), "{").unwrap();
        assert!(read(d.path(), "b").is_err());
        assert_eq!(old(d.path()).unwrap(), None);
        assert!(!d.path().join("journal").exists());
    }
}
