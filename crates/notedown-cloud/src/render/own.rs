//! Which files in the render folder are the pass's own: a PDF a row names, or a PDF that
//! notedown made. Every other file stays foreign.

use std::collections::HashMap;
use std::path::Path;

use crate::store::fold;

use super::pass::Pass;

/// The producer entry `render_pdf` writes into every PDF.
const PRODUCER: &[u8] = b"/Producer (notedown ";

/// Whether `a` and `b` name the same file (a case-only rename on a case-insensitive disk).
pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    // `canonicalize` gives the spelling on disk, so two spellings of one file on a
    // case-insensitive folder compare equal, and two real files on a case-sensitive folder
    // don't.
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

/// Whether the file at `path` is a PDF that `render_pdf` made.
fn is_notedown_pdf(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(b"%PDF-") && bytes.windows(PRODUCER.len()).any(|w| w == PRODUCER)
}

impl Pass {
    /// Rows of an older manifest whose PDF paths differ only in case can name one file (on a
    /// render folder that ignores case). The note that sorts first keeps the file and is
    /// rendered again; the rows of the others are dropped, so they get their own PDF.
    pub(super) fn split_shared_pdfs(&mut self) {
        let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (key, row) in &self.m.notes {
            if row.pdf {
                groups
                    .entry(fold(&row.pdf_path()))
                    .or_default()
                    .push((row.path.clone(), key.clone()));
            }
        }
        for mut group in groups.into_values().filter(|g| g.len() > 1) {
            group.sort();
            let first = group[0].1.clone();
            let Some(file) = self.file_of(&self.m.notes[&first].pdf_path()) else {
                continue;
            };
            let mut shared = false;
            for (_, key) in &group[1..] {
                if self
                    .file_of(&self.m.notes[key].pdf_path())
                    .is_some_and(|other| same_file(&file, &other))
                {
                    self.m.notes.remove(key);
                    shared = true;
                }
            }
            if shared && let Some(row) = self.m.notes.get_mut(&first) {
                // The file may hold the PDF of another note: render this one again.
                row.md5.clear();
            }
        }
    }

    /// Whether the pass may write to `rel`: nothing is there, or the file there is ours. Ours
    /// is a PDF a row names; the old PDF of this note (`own`) under another spelling of the
    /// name; a PDF that notedown made (its producer entry), which covers a PDF written right
    /// before a crash and the PDFs of earlier passes after the manifest was lost. Any other
    /// file, a PDF from another program too, stays foreign, also on the first pass.
    pub(super) fn is_ours(&self, rel: &str, own: Option<&str>) -> bool {
        let Some(target) = self.file_of(rel) else {
            return false;
        };
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            return true;
        };
        if !meta.is_file() {
            return false;
        }
        if self.m.notes.values().any(|r| r.pdf && r.pdf_path() == rel) {
            return true;
        }
        if own
            .and_then(|own| self.file_of(own))
            .is_some_and(|own| same_file(&own, &target))
        {
            return true;
        }
        is_notedown_pdf(&target)
    }

    /// Drop the rows of other notes whose PDF is at `rel`, also under another spelling of the
    /// same file: the pass is about to replace that file. Such a note gets a new PDF when its
    /// turn comes (or in the next pass).
    pub(super) fn release(&mut self, rel: &str, key: &str) {
        let target = self.file_of(rel);
        let key_rel = fold(rel);
        let mut drop = Vec::new();
        for (k, r) in &self.m.notes {
            if k == key || !r.pdf {
                continue;
            }
            let pdf = r.pdf_path();
            let same = pdf == rel
                || (fold(&pdf) == key_rel
                    && target
                        .as_deref()
                        .zip(self.file_of(&pdf))
                        .is_some_and(|(t, p)| same_file(t, &p)));
            if same {
                drop.push(k.clone());
            }
        }
        for k in drop {
            self.m.notes.remove(&k);
        }
    }
}
