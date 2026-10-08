//! `<root>/<slug>/.notedown-render.json`: which PDF the pass wrote for which note.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = ".notedown-render.json";

/// The format version written.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub renderer: String,
    /// By file id (as a string, JSON keys are strings).
    pub notes: BTreeMap<String, Row>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The wire path of the note when the row was written.
    pub path: String,
    pub md5: String,
    /// False: the note failed to render and has no PDF.
    pub pdf: bool,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(MANIFEST_FILE)
}

/// The manifest of `dir`; `None` when it is missing or can't be read.
pub fn read(dir: &Path) -> Option<Manifest> {
    std::fs::read(path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// The manifest of `dir`; empty when it is missing or can't be read (every note then counts
/// as new, which is safe: a PDF the pass doesn't know it never deletes).
pub fn load(dir: &Path) -> Manifest {
    read(dir).unwrap_or_default()
}

pub fn save(dir: &Path, m: &Manifest) -> crate::Result<()> {
    crate::fsutil::write_atomic(&path(dir), &crate::fsutil::json_bytes(m), false)
}
