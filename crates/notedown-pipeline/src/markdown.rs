//! One Markdown file per note in a folder the user picks (`markdown_dir`, `--markdown-dir`).
//!
//! - Layout: the note's folder as on the device, then `<title>.md`: `NOTE/Note/Work/Meeting.note`
//!   gives `Work/Meeting.md`, `NOTE/MyStyle/a.note` gives `MyStyle/a.md`, a note of another tree
//!   keeps its device path (`DOCUMENT/Document/x/b.note` gives `Document/x/b.md`).
//! - Content: [`format::note_document`], the document the MCP server's `get_note` gives.
//! - A note has a file while it has a page with text and is neither deleted nor unscoped.
//! - The manifest (`<store>/markdown.json`) names every file the writer made and the folders it
//!   created. The writer overwrites, moves and deletes only files of its manifest; a name held
//!   by any other file gives `<title> (2).md` (then 3, ...). A folder it created goes when a
//!   move or delete leaves it empty.
//! - Crash safety: files are written atomically, the manifest after them. A file at the
//!   chosen name that is not in the manifest but holds the same text (a crash before the
//!   manifest was written) is taken back instead of copied.
//! - A file whose text is the same (all but the `generated:` line of the frontmatter, which
//!   holds the time of the render) is not written again, so sync tools see no change.
//! - The first sync of a [`Writer`] looks at every note (a restart, a new folder, a crash that
//!   lost the changes of a pass); later syncs only at the notes they are given.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::format;
use crate::model::NoteState;
use crate::store::TextStore;
use crate::{Error, Result};

/// The manifest's file name in the store of an account.
pub const MANIFEST_FILE: &str = "markdown.json";
/// The most numbered names tried for one note.
const MAX_NUMBER: usize = 10_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    dir: PathBuf,
    /// Note key to the file's path below `dir`, with `/` between folders.
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// The folders below `dir` the writer created, so a move or delete may remove them again.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    folders: BTreeSet<String>,
}

/// Keeps the Markdown files of one account in one folder.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    manifest_path: PathBuf,
    manifest: Manifest,
    /// The manifest as it is on disk; `None` when there is none.
    saved: Option<Manifest>,
    /// The next sync looks at every note.
    full: bool,
    /// Notes whose file is not up to date yet (a failed sync).
    pending: BTreeSet<String>,
    /// The files of the manifest, by path: the note that holds each.
    held: BTreeMap<String, String>,
}

impl Writer {
    /// A writer into `dir` with its manifest at `manifest_path`. A manifest of another folder,
    /// or one that cannot be read, is started over: the files it names stay where they are.
    /// Entries that are no safe path below the folder are dropped. Writes nothing.
    pub fn new(dir: &Path, manifest_path: &Path) -> Result<Writer> {
        let saved = match std::fs::read(manifest_path) {
            Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes).ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(Error::Io {
                    path: manifest_path.to_path_buf(),
                    source,
                });
            }
        };
        let mut manifest = match &saved {
            Some(m) if m.dir == dir => m.clone(),
            _ => Manifest {
                dir: dir.to_path_buf(),
                ..Manifest::default()
            },
        };
        manifest
            .files
            .retain(|key, rel| notedown_cloud::pages::valid_key(key) && safe_rel(rel).is_some());
        manifest.folders.retain(|rel| safe_rel(rel).is_some());
        let held = manifest
            .files
            .iter()
            .map(|(key, rel)| (rel.clone(), key.clone()))
            .collect();
        Ok(Writer {
            dir: dir.to_path_buf(),
            manifest_path: manifest_path.to_path_buf(),
            manifest,
            saved,
            full: true,
            pending: BTreeSet::new(),
            held,
        })
    }

    /// Brings the files of the notes `changed` (and, on the first sync, of every note) up to
    /// date with `store`, then writes the manifest if it changed. A note that fails is tried
    /// again by the next sync; the first error is returned after the others were done.
    pub fn sync(&mut self, store: &TextStore, slug: &str, changed: &[String]) -> Result<()> {
        self.pending.extend(changed.iter().cloned());
        let mut states: BTreeMap<String, NoteState> = BTreeMap::new();
        if self.full {
            for note in store.notes()? {
                self.pending.insert(note.key.clone());
                states.insert(note.key.clone(), note);
            }
            self.pending.extend(self.manifest.files.keys().cloned());
            self.full = false;
        }
        let keys: Vec<String> = self.pending.iter().cloned().collect();
        let mut first_error = None;
        for key in keys {
            let state = match states.remove(&key) {
                Some(s) => Some(s),
                None if notedown_cloud::pages::valid_key(&key) => match store.note(&key) {
                    Ok(s) => s,
                    Err(e) => {
                        first_error.get_or_insert(e);
                        continue;
                    }
                },
                None => None,
            };
            match self.place(&key, state.as_ref(), slug) {
                Ok(()) => {
                    self.pending.remove(&key);
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        self.save()?;
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn save(&mut self) -> Result<()> {
        if self.saved.as_ref() == Some(&self.manifest) {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec_pretty(&self.manifest)?;
        bytes.push(b'\n');
        notedown_cloud::fsutil::write_atomic(&self.manifest_path, &bytes, false)?;
        self.saved = Some(self.manifest.clone());
        Ok(())
    }

    /// Brings the file of note `key` in line with its state.
    fn place(&mut self, key: &str, state: Option<&NoteState>, slug: &str) -> Result<()> {
        let want = state
            .filter(|s| !s.deleted && !s.unscoped && has_text(s))
            .and_then(|s| relative_path(&s.path).map(|rel| (rel, s)));
        let current = self.manifest.files.get(key).cloned();
        let Some((rel, note)) = want else {
            if let Some(old) = current {
                self.remove(&old)?;
                self.manifest.files.remove(key);
                self.held.remove(&old);
            }
            return Ok(());
        };
        let content = format::note_document(note, slug);
        let target = self.choose(key, &rel, current.as_deref(), &content)?;
        let target_path = self.dir.join(&target);
        let mut renamed = false;
        if let Some(old) = current.as_deref().filter(|old| *old != target) {
            let old_path = self.dir.join(old);
            // A name that differs only in case on a disk that ignores case is the same file:
            // it is renamed, never written and then removed.
            if same_file(&old_path, &target_path) {
                self.make_parents(&target)?;
                notedown_cloud::fsutil::rename_durable(&old_path, &target_path)?;
                renamed = true;
            }
        }
        self.make_parents(&target)?;
        let up_to_date = read_text(&target_path)?.is_some_and(|t| same_document(&t, &content));
        if !up_to_date {
            notedown_cloud::fsutil::write_atomic_in(&target_path, content.as_bytes(), false)?;
        }
        if let Some(old) = current.filter(|old| *old != target) {
            if renamed {
                self.prune(&old);
            } else {
                self.remove(&old)?;
            }
        }
        if let Some(old) = self.manifest.files.insert(key.to_string(), target.clone()) {
            self.held.remove(&old);
        }
        self.held.insert(target, key.to_string());
        Ok(())
    }

    /// The file name for note `key` at `rel`: its current file when that has the same name
    /// (or a numbered one of it), else the first of `rel`, `<stem> (2).md`, ... that no other
    /// note holds and that is free on disk or holds the same text already.
    fn choose(
        &self,
        key: &str,
        rel: &Path,
        current: Option<&str>,
        content: &str,
    ) -> Result<String> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let stem = rel
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if let Some(c) = current.filter(|c| numbered_of(c, parent, &stem)) {
            return Ok(c.to_string());
        }
        let current_path = current.map(|c| self.dir.join(c));
        for n in 1..=MAX_NUMBER {
            let name = if n == 1 {
                format!("{stem}.md")
            } else {
                format!("{stem} ({n}).md")
            };
            let candidate = rel_string(&parent.join(name));
            if current == Some(candidate.as_str()) {
                return Ok(candidate);
            }
            if self.held.get(&candidate).is_some_and(|k| k != key) {
                continue;
            }
            let path = self.dir.join(&candidate);
            let free = match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(source) => return Err(Error::Io { path, source }),
                Ok(m) if !m.is_file() => false,
                Ok(_) => {
                    current_path.as_deref().is_some_and(|c| same_file(c, &path))
                        || read_text(&path)?.is_some_and(|t| same_document(&t, content))
                }
            };
            if free {
                return Ok(candidate);
            }
        }
        Err(Error::Invalid {
            what: "Markdown file name",
            message: format!(
                "no free name for {} in {}",
                rel.display(),
                self.dir.display()
            ),
        })
    }

    /// Removes the file `rel` of the manifest and the folders it empties.
    fn remove(&mut self, rel: &str) -> Result<()> {
        let path = self.dir.join(rel);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path, source }),
        }
        self.prune(rel);
        Ok(())
    }

    /// Removes the folders above `rel` the writer created while they are empty.
    fn prune(&mut self, rel: &str) {
        let mut folder = Path::new(rel).parent();
        while let Some(f) = folder.filter(|f| !f.as_os_str().is_empty()) {
            let name = rel_string(f);
            if !self.manifest.folders.contains(&name) {
                return;
            }
            match std::fs::remove_dir(self.dir.join(f)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Not empty (a file of the user's, another note) or not removable: it stays.
                Err(_) => return,
            }
            self.manifest.folders.remove(&name);
            folder = f.parent();
        }
    }

    /// Creates the folder of `rel` and every missing one above it, and notes those it made.
    fn make_parents(&mut self, rel: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(|source| Error::Io {
            path: self.dir.clone(),
            source,
        })?;
        let Some(parent) = Path::new(rel).parent() else {
            return Ok(());
        };
        let mut at = PathBuf::new();
        for part in parent.components() {
            at.push(part);
            let path = self.dir.join(&at);
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    self.manifest.folders.insert(rel_string(&at));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        Ok(())
    }
}

/// The path of a note's Markdown file below the folder, from its wire path; `None` for a path
/// that is not a safe relative path.
pub fn relative_path(wire: &str) -> Option<PathBuf> {
    let segments: Vec<&str> = wire.split('/').collect();
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains('\0'))
    {
        return None;
    }
    // The device's own folder names: `NOTE/MyStyle` is `MyStyle`, and `NOTE/Note` is the top.
    let mut device: Vec<&str> = segments.clone();
    for (name, folder) in notedown_cloud::import::FOLDER_MAP {
        let parts: Vec<&str> = folder.split('/').collect();
        if segments.len() > parts.len() && segments[..parts.len()] == parts[..] {
            device = std::iter::once(name)
                .chain(segments[parts.len()..].iter().copied())
                .collect();
            if name == "Note" {
                device.remove(0);
            }
            break;
        }
    }
    let (last, folders) = device.split_last()?;
    let stem = last.strip_suffix(".note").unwrap_or(last);
    if stem.is_empty() {
        return None;
    }
    let mut out: PathBuf = folders.iter().collect();
    out.push(format!("{stem}.md"));
    safe_rel(&rel_string(&out))
}

/// Whether `rel` is `<parent>/<stem>.md` or `<parent>/<stem> (N).md`.
fn numbered_of(rel: &str, parent: &Path, stem: &str) -> bool {
    let rel = Path::new(rel);
    if rel.parent().unwrap_or(Path::new("")) != parent {
        return false;
    }
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == format!("{stem}.md") {
        return true;
    }
    name.strip_prefix(stem)
        .and_then(|r| r.strip_prefix(" ("))
        .and_then(|r| r.strip_suffix(").md"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `rel` as a path below the folder, when it is one: relative, no `.` or `..`.
fn safe_rel(rel: &str) -> Option<PathBuf> {
    let path = Path::new(rel);
    let ok = !rel.is_empty()
        && !rel.contains('\0')
        && path.components().all(|c| matches!(c, Component::Normal(_)));
    ok.then(|| path.to_path_buf())
}

/// A relative path with `/` between its parts.
fn rel_string(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether the note has a page with text.
fn has_text(note: &NoteState) -> bool {
    note.pages
        .iter()
        .any(|p| p.seq.is_some() && p.blocks.iter().any(|b| !b.text.trim().is_empty()))
}

/// The text of the file at `path`, `None` when there is none (or it is no UTF-8 text).
fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8(bytes).ok()),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::IsADirectory =>
        {
            Ok(None)
        }
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Whether two documents are the same but for the `generated:` line of their frontmatter.
fn same_document(a: &str, b: &str) -> bool {
    fn lines(text: &str) -> impl Iterator<Item = &str> {
        let mut fences = 0;
        text.split('\n').filter(move |line| {
            if *line == "---" {
                fences += 1;
            }
            !(fences == 1 && line.starts_with("generated:"))
        })
    }
    lines(a).eq(lines(b))
}

/// Whether `a` and `b` are the same file (also under two spellings on a disk that ignores
/// case).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_of_the_render_is_no_change() {
        let a = "---\ntitle: x\ngenerated: { by: n, at: 1 }\n---\n\nhello\n";
        let b = "---\ntitle: x\ngenerated: { by: n, at: 2 }\n---\n\nhello\n";
        assert!(same_document(a, b));
        assert!(!same_document(a, &b.replace("hello", "hullo")));
        let body = "---\ntitle: x\n---\ngenerated: 1\n";
        assert!(!same_document(body, "---\ntitle: x\n---\ngenerated: 2\n"));
    }

    #[test]
    fn manifest_paths_must_stay_below_the_folder() {
        assert!(safe_rel("a/b.md").is_some());
        for bad in ["", "../a.md", "a/../../b.md", "/etc/passwd", "./a.md"] {
            assert!(safe_rel(bad).is_none(), "{bad}");
        }
    }
}
