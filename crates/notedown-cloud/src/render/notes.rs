//! The store's notes as the pass sees them, and the PDF path of each note.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::Result;
use crate::journal::FileKind;
use crate::store::{Store, WirePath, fold};

/// The wire path with its last `.note` extension (any case) replaced by `.pdf`; a path without
/// it gets `.pdf` appended.
pub fn pdf_path_of(wire_path: &str) -> String {
    let cut = wire_path.len().saturating_sub(".note".len());
    match wire_path.get(cut..) {
        Some(ext) if ext.eq_ignore_ascii_case(".note") => format!("{}.pdf", &wire_path[..cut]),
        _ => format!("{wire_path}.pdf"),
    }
}

/// The PDF path of every note, by note path. `notes` are the wire paths of all registered
/// notes, sorted in byte order, each with the PDF path it owns (its row has a PDF and the same
/// path). Two PDF paths collide when they are equal without case (a render folder may ignore
/// case even when the data folder doesn't).
///
/// A note keeps the PDF path it owns; of two owned paths that collide, the note that sorts
/// first keeps it. Every other note, in order, gets [`pdf_path_of`] when that is free, else
/// its full name plus `.pdf`, then ` (2)`, ` (3)` ... before `.pdf`. A note never moves back
/// to the plain name when it becomes free, so the names stay stable.
pub fn assign_targets<'a>(
    notes: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> HashMap<String, String> {
    let notes: Vec<_> = notes.into_iter().collect();
    let mut taken = HashSet::new();
    let mut out = HashMap::new();
    for &(path, owned) in &notes {
        if let Some(owned) = owned
            && taken.insert(fold(owned))
        {
            out.insert(path.to_string(), owned.to_string());
        }
    }
    for &(path, _) in &notes {
        if out.contains_key(path) {
            continue;
        }
        let mut target = pdf_path_of(path);
        if taken.contains(&fold(&target)) {
            target = format!("{path}.pdf");
            let mut k = 2;
            while taken.contains(&fold(&target)) {
                target = format!("{path} ({k}).pdf");
                k += 1;
            }
        }
        taken.insert(fold(&target));
        out.insert(path.to_string(), target);
    }
    out
}

/// A note the id registry knows.
pub(super) struct Note {
    pub(super) id: u64,
    pub(super) path: String,
    /// The file on disk; `None` when it is missing (the scan journals the delete or move).
    pub(super) on_disk: Option<OnDisk>,
}

pub(super) struct OnDisk {
    pub(super) fs_path: PathBuf,
    /// From the MD5 cache when its row is current for the file; `None`: hash the bytes.
    pub(super) md5: Option<String>,
}

/// The registered notes, sorted by path, read under a short store guard. Changes nothing in
/// the store: a file on disk the registry doesn't know is not listed (the scan adds it).
pub(super) fn list_notes(store: &Store) -> Result<Vec<Note>> {
    let g = store.lock()?;
    let mut names = FolderNames::default();
    let mut notes = Vec::new();
    for (id, wire) in store.registered_files(&g)? {
        let Some(name) = wire.name() else { continue };
        if wire.is_hidden() || FileKind::of(name) != FileKind::Note {
            continue;
        }
        let fs_path = store.fs_path(&wire);
        let meta = names
            .exists(store.files_dir(), &wire)
            .then(|| std::fs::symlink_metadata(&fs_path).ok())
            .flatten()
            .filter(|m| m.is_file());
        let on_disk = match meta {
            Some(meta) => Some(OnDisk {
                md5: store.current_md5(&g, &wire, &meta)?,
                fs_path,
            }),
            None => None,
        };
        notes.push(Note {
            id,
            path: wire.display(),
            on_disk,
        });
    }
    drop(g);
    notes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(notes)
}

/// The names in each folder, read once per folder, to check paths by their exact names (a
/// case-insensitive disk opens `a.note` for `A.note` too).
#[derive(Default)]
struct FolderNames(HashMap<PathBuf, HashSet<String>>);

impl FolderNames {
    /// Whether every segment of `wire` is there below `files` with exactly this name.
    fn exists(&mut self, files: &Path, wire: &WirePath) -> bool {
        let mut dir = files.to_path_buf();
        for segment in wire.segments() {
            let names = self.0.entry(dir.clone()).or_insert_with(|| {
                std::fs::read_dir(&dir)
                    .map(|rd| {
                        rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                            .collect()
                    })
                    .unwrap_or_default()
            });
            if !names.contains(segment) {
                return false;
            }
            dir.push(segment);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(paths: &[&str]) -> Vec<String> {
        owned_targets(&paths.iter().map(|p| (*p, None)).collect::<Vec<_>>())
    }

    fn owned_targets(notes: &[(&str, Option<&str>)]) -> Vec<String> {
        let t = assign_targets(notes.iter().copied());
        notes.iter().map(|(p, _)| t[*p].clone()).collect()
    }

    #[test]
    fn a_note_keeps_the_pdf_path_it_owns() {
        // `a.note` had `a.pdf` first; `A.note` sorts first but is new.
        assert_eq!(
            owned_targets(&[
                ("NOTE/Note/A.note", None),
                ("NOTE/Note/a.note", Some("NOTE/Note/a.pdf")),
            ]),
            ["NOTE/Note/A.note.pdf", "NOTE/Note/a.pdf"]
        );
        // A full name stays when the plain name is free again.
        assert_eq!(
            owned_targets(&[("NOTE/Note/a.note", Some("NOTE/Note/a.note.pdf"))]),
            ["NOTE/Note/a.note.pdf"]
        );
        // Two owned paths that collide: the first note keeps its path.
        assert_eq!(
            owned_targets(&[
                ("NOTE/Note/A.note", Some("NOTE/Note/A.pdf")),
                ("NOTE/Note/a.note", Some("NOTE/Note/a.pdf")),
            ]),
            ["NOTE/Note/A.pdf", "NOTE/Note/a.note.pdf"]
        );
    }

    #[test]
    fn colliding_notes_get_their_full_name() {
        // Byte order: `a.NOTE` before `a.note`.
        assert_eq!(
            targets(&["NOTE/Note/a.NOTE", "NOTE/Note/a.note"]),
            ["NOTE/Note/a.pdf", "NOTE/Note/a.note.pdf"]
        );
        // Case of the stem too: a render folder that ignores case can't hold both.
        assert_eq!(
            targets(&["NOTE/Note/A.note", "NOTE/Note/a.note", "NOTE/Note/b.note"]),
            ["NOTE/Note/A.pdf", "NOTE/Note/a.note.pdf", "NOTE/Note/b.pdf"]
        );
        // The full name is taken too: a number is added.
        assert_eq!(
            targets(&[
                "NOTE/Note/A.note",
                "NOTE/Note/a.NOTE.note",
                "NOTE/Note/a.note"
            ]),
            [
                "NOTE/Note/A.pdf",
                "NOTE/Note/a.NOTE.pdf",
                "NOTE/Note/a.note (2).pdf"
            ]
        );
        assert_eq!(
            targets(&[
                "NOTE/Note/A.note",
                "NOTE/Note/a (2).note",
                "NOTE/Note/a.NOTE.note",
                "NOTE/Note/a.note",
            ]),
            [
                "NOTE/Note/A.pdf",
                "NOTE/Note/a (2).pdf",
                "NOTE/Note/a.NOTE.pdf",
                "NOTE/Note/a.note (2).pdf",
            ]
        );
        // Other folders don't collide.
        assert_eq!(
            targets(&["NOTE/Note/V/a.note", "NOTE/Note/a.NOTE"]),
            ["NOTE/Note/V/a.pdf", "NOTE/Note/a.pdf"]
        );
    }
}
