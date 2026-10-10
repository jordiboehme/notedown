//! `errors.jsonl` of an account: OCR failures with their retry state (spec 4.4).
//!
//! The file holds one line per failing page (or note that does not parse, without `pageId`):
//! `{note, pageId, tries, lastError, nextTry}`. It is the current set, rewritten atomically on
//! every change; a page that reads again leaves it. A failure is tried again after
//! [`RETRY_AFTER_MS`], at most [`MAX_TRIES`] times; then the page state is `failed`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::store::WriterLock;

/// `<data>/pipeline/<slug>/errors.jsonl`.
pub const ERRORS_FILE: &str = "errors.jsonl";
/// A failed page is tried again after one hour.
pub const RETRY_AFTER_MS: u64 = 3_600_000;
/// After this many tries a page is `failed`.
pub const MAX_TRIES: u32 = 3;

/// One line of `errors.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEntry {
    /// The note key.
    pub note: String,
    /// The page; `None` for a note that does not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
    pub tries: u32,
    pub last_error: String,
    /// Ms since the epoch.
    pub next_try: u64,
}

impl ErrorEntry {
    /// Whether the entry still waits for a retry at `now`: fewer than [`MAX_TRIES`] tries and
    /// `nextTry` not reached.
    pub fn waits(&self, now: u64) -> bool {
        self.tries < MAX_TRIES && now < self.next_try
    }

    /// Whether every try is used up.
    pub fn exhausted(&self) -> bool {
        self.tries >= MAX_TRIES
    }
}

/// The failures of one account.
#[derive(Debug)]
pub struct Errors {
    path: PathBuf,
    entries: Vec<ErrorEntry>,
}

impl Errors {
    /// Reads `errors.jsonl` in the store folder `dir`. A missing file is empty; a line that
    /// does not parse is left out with a warning (it only costs a retry).
    pub fn load(dir: &Path) -> Result<Errors> {
        let path = dir.join(ERRORS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => return Err(crate::Error::Io { path, source }),
        };
        let mut entries = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<ErrorEntry>(line) {
                Ok(e) => entries.push(e),
                Err(e) => eprintln!("warning: {}: a line cannot be read: {e}", path.display()),
            }
        }
        Ok(Errors { path, entries })
    }

    pub fn entries(&self) -> &[ErrorEntry] {
        &self.entries
    }

    /// The entry of `note` and `page` (`None`: the note itself).
    pub fn get(&self, note: &str, page: Option<&str>) -> Option<&ErrorEntry> {
        self.entries
            .iter()
            .find(|e| e.note == note && e.page_id.as_deref() == page)
    }

    /// Counts one more failed try at `now` and saves. Returns the entry.
    pub fn fail(
        &mut self,
        _w: &WriterLock,
        note: &str,
        page: Option<&str>,
        error: &str,
        now: u64,
    ) -> Result<ErrorEntry> {
        let entry = match self
            .entries
            .iter_mut()
            .find(|e| e.note == note && e.page_id.as_deref() == page)
        {
            Some(e) => e,
            None => {
                self.entries.push(ErrorEntry {
                    note: note.to_string(),
                    page_id: page.map(str::to_string),
                    tries: 0,
                    last_error: String::new(),
                    next_try: 0,
                });
                self.entries.last_mut().expect("just pushed")
            }
        };
        entry.tries += 1;
        entry.last_error = error.to_string();
        entry.next_try = now + RETRY_AFTER_MS;
        let entry = entry.clone();
        self.save()?;
        Ok(entry)
    }

    /// Removes the entry of `note` and `page`; saves when there was one.
    pub fn clear(&mut self, _w: &WriterLock, note: &str, page: Option<&str>) -> Result<()> {
        let before = self.entries.len();
        self.entries
            .retain(|e| !(e.note == note && e.page_id.as_deref() == page));
        if self.entries.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// Removes every entry of `note` whose page is not in `keep` (a page that is gone).
    pub fn retain_pages(&mut self, _w: &WriterLock, note: &str, keep: &[&str]) -> Result<()> {
        let before = self.entries.len();
        self.entries
            .retain(|e| e.note != note || e.page_id.as_deref().is_none_or(|p| keep.contains(&p)));
        if self.entries.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// The earliest `nextTry` of an entry that still waits at `now`.
    pub fn next_try(&self, now: u64) -> Option<u64> {
        self.entries
            .iter()
            .filter(|e| e.waits(now))
            .map(|e| e.next_try)
            .min()
    }

    fn save(&self) -> Result<()> {
        let mut bytes = Vec::new();
        for e in &self.entries {
            bytes.extend_from_slice(serde_json::to_string(e)?.as_bytes());
            bytes.push(b'\n');
        }
        notedown_cloud::fsutil::write_atomic(&self.path, &bytes, false)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tries_count_up_and_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let w = WriterLock::take(tmp.path()).unwrap();
        let mut errors = Errors::load(tmp.path()).unwrap();
        let e = errors.fail(&w, "F1", Some("P1"), "boom", 10).unwrap();
        assert_eq!((e.tries, e.next_try), (1, 10 + RETRY_AFTER_MS));
        assert!(e.waits(11));
        assert!(!e.waits(10 + RETRY_AFTER_MS));
        errors.fail(&w, "F1", None, "parse", 10).unwrap();
        errors.fail(&w, "F1", Some("P1"), "boom2", 20).unwrap();
        let text = std::fs::read_to_string(tmp.path().join(ERRORS_FILE)).unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            format!(
                r#"{{"note":"F1","pageId":"P1","tries":2,"lastError":"boom2","nextTry":{}}}"#,
                20 + RETRY_AFTER_MS
            )
        );
        let errors2 = Errors::load(tmp.path()).unwrap();
        assert_eq!(errors2.entries(), errors.entries());
        assert_eq!(errors.next_try(0), Some(10 + RETRY_AFTER_MS));
        errors.retain_pages(&w, "F1", &[]).unwrap();
        assert_eq!(errors.entries().len(), 1);
        errors.clear(&w, "F1", None).unwrap();
        assert!(Errors::load(tmp.path()).unwrap().entries().is_empty());
    }
}
