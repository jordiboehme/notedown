//! `<root>/<slug>/.notedown-render.json`: which PDF the pass wrote for which note.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::WirePath;

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

/// The manifest of `dir`; empty when it is missing or can't be read (every note then counts
/// as new, which is safe: a PDF the pass doesn't know it never deletes). A row whose path is
/// not a wire path in its plain form (absolute, with `..`, empty) is dropped: the file is only
/// a file in the render folder, and nothing in it may name a file outside.
pub fn load(dir: &Path) -> Manifest {
    let mut m: Manifest = std::fs::read(path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    m.notes.retain(|_, row| is_wire_path(&row.path));
    m
}

/// Whether `text` is a wire path naming a file or folder, written as the store writes it:
/// segments joined by `/`, no leading or doubled `/`, no `.` or `..`.
pub fn is_wire_path(text: &str) -> bool {
    WirePath::parse_named(text).is_ok_and(|w| w.display() == text)
}

pub fn save(dir: &Path, m: &Manifest) -> crate::Result<()> {
    crate::fsutil::write_atomic(&path(dir), &crate::fsutil::json_bytes(m), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_wire_paths_are_valid() {
        assert!(is_wire_path("NOTE/Note/a.note"));
        assert!(is_wire_path("NOTE/Note/a.pdf"));
        for bad in [
            "",
            "/",
            "/Users/x/tax.note",
            "../outside/a.note",
            "NOTE/../../a.note",
            "NOTE/./a.note",
            "NOTE//a.note",
            "NOTE/a.note/",
            "NOTE\\a.note",
        ] {
            assert!(!is_wire_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn load_drops_rows_with_bad_paths() {
        let d = tempfile::tempdir().unwrap();
        let row = |p: &str| Row {
            path: p.into(),
            md5: "0".repeat(32),
            pdf: true,
        };
        let mut m = Manifest {
            version: VERSION,
            renderer: "r".into(),
            ..Manifest::default()
        };
        m.notes.insert("1".into(), row("NOTE/Note/a.note"));
        m.notes.insert("2".into(), row("/abs/a.note"));
        m.notes.insert("3".into(), row("../a.note"));
        m.notes.insert("4".into(), row(""));
        save(d.path(), &m).unwrap();
        let loaded = load(d.path());
        assert_eq!(loaded.notes.keys().collect::<Vec<_>>(), ["1"]);
    }
}
