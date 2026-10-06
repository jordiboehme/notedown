//! Capture dumps: with capture mode on, every logged request and frame is also written to its own
//! file `<data>/capture/<ts>-<n>.json`.
//!
//! The dump is the sanitized [`LogEntry`] the protocol log writes, so it has exactly the masking of
//! the log (client masking for request data, server masking for response data). Nothing unmasked
//! is ever written.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;

use super::log::LogEntry;
use crate::fsutil::{json_bytes, write_atomic};
use crate::{Result, io_err};

pub struct Capture {
    dir: PathBuf,
    counter: AtomicU64,
    failed: AtomicU64,
}

/// A sortable UTC stamp such as `20261006T153012Z`.
fn stamp(t: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

impl Capture {
    /// Open (and create) `<data_dir>/capture/`.
    pub fn open(data_dir: &Path) -> Result<Capture> {
        let dir = data_dir.join("capture");
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        Ok(Capture {
            dir,
            counter: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Dumps that could not be written.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Write one already sanitized entry. Never fails the caller: a failure is counted and
    /// reported on stderr.
    pub(super) fn write(&self, entry: &LogEntry, now: OffsetDateTime) {
        // Never overwrite a dump of an earlier run: take the next free number.
        let path = loop {
            let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let path = self.dir.join(format!("{}-{n:06}.json", stamp(now)));
            if !path.exists() {
                break path;
            }
        };
        if let Err(e) = write_atomic(&path, &json_bytes(entry), false) {
            self.failed.fetch_add(1, Ordering::Relaxed);
            let _ = writeln!(std::io::stderr(), "capture: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::LogKind;

    #[test]
    fn writes_pretty_numbered_files() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::open(dir.path()).unwrap();
        let t = OffsetDateTime::from_unix_timestamp(1_791_300_612).unwrap();
        let mut e = LogEntry::new(LogKind::Request);
        e.path = Some("/x".into());
        c.write(&e, t);
        c.write(&e, t);
        let name = format!("{}-000001.json", stamp(t));
        assert!(name.starts_with("2026"), "{name}");
        let text = std::fs::read_to_string(c.dir().join(&name)).unwrap();
        assert!(text.contains("\n  \"path\": \"/x\""), "{text}");
        assert!(text.ends_with("}\n"));
        assert!(c.dir().join(format!("{}-000002.json", stamp(t))).exists());
        assert_eq!(c.failed(), 0);
    }

    #[test]
    fn an_existing_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let t = OffsetDateTime::from_unix_timestamp(1_791_300_612).unwrap();
        let old = dir
            .path()
            .join("capture")
            .join(format!("{}-000001.json", stamp(t)));
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "old").unwrap();
        let c = Capture::open(dir.path()).unwrap();
        c.write(&LogEntry::new(LogKind::Request), t);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "old");
        assert!(c.dir().join(format!("{}-000002.json", stamp(t))).exists());
    }

    #[test]
    fn a_failed_write_is_counted_not_raised() {
        let dir = tempfile::tempdir().unwrap();
        let c = Capture::open(dir.path()).unwrap();
        std::fs::remove_dir_all(c.dir()).unwrap();
        std::fs::write(c.dir(), b"not a dir").unwrap();
        c.write(&LogEntry::new(LogKind::Request), OffsetDateTime::now_utc());
        assert_eq!(c.failed(), 1);
    }
}
