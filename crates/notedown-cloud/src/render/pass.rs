//! One render pass over an account's notes: write, move or delete the PDFs so they match the
//! store.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use notedown_note::{Notebook, RENDERER_ID, RenderOptions, RenderStyle, VersionPolicy};

use super::PassReport;
use super::manifest::{self, Manifest, Row};
use super::notes::{Note, assign_targets, pdf_path_of};
use super::own::same_file;
use crate::store::WirePath;
use crate::{Result, io_err};

/// The manifest is written after this many changed notes, so a crash loses little work.
const SAVE_EVERY: usize = 20;

fn md5_hex(bytes: &[u8]) -> String {
    const_hex::encode(Md5::digest(bytes))
}

/// The PDF of a note, with the template, in the reference style. A panic of the renderer on a
/// broken note is an error of that note, not of the pass.
fn render_pdf(bytes: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
    let run = || {
        Notebook::from_bytes(bytes, VersionPolicy::AllowUnknown)?.render_pdf(&RenderOptions {
            include_background: true,
            style: RenderStyle::Reference,
        })
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("the renderer stopped on this note".into()),
    }
}

pub(super) struct Pass {
    slug: String,
    /// `<root>/<slug>`.
    dir: PathBuf,
    pub(super) m: Manifest,
    /// The manifest as the file holds it (loaded or last saved), so an unchanged one is not
    /// written again: a cloud folder would see a file change on every pass.
    saved: Manifest,
    /// Every row counts as out of date for rendering (`--all` or another renderer id). The rows
    /// still say which PDFs are ours.
    stale: bool,
    pub(super) report: PassReport,
    /// Folders that lost a file in this pass.
    emptied: BTreeSet<PathBuf>,
    changes: usize,
}

impl Pass {
    /// A pass on the account folder `dir`, with its manifest loaded.
    pub(super) fn new(slug: &str, dir: PathBuf, all: bool) -> Pass {
        // `saved` is the file as it is, so rows dropped here are removed from it too.
        let saved = manifest::read(&dir);
        let mut m = saved.clone();
        manifest::drop_bad_rows(&mut m);
        let mut pass = Pass {
            slug: slug.to_string(),
            stale: all || m.renderer != RENDERER_ID,
            dir,
            saved,
            m,
            report: PassReport::default(),
            emptied: BTreeSet::new(),
            changes: 0,
        };
        pass.split_shared_pdfs();
        pass
    }

    pub(super) fn run(&mut self, notes: &[Note]) -> Result<()> {
        let owned: Vec<Option<String>> = notes
            .iter()
            .map(|n| {
                self.m
                    .notes
                    .get(&n.id.to_string())
                    .filter(|r| r.pdf && r.path == n.path)
                    .map(Row::pdf_path)
            })
            .collect();
        let targets = assign_targets(
            notes
                .iter()
                .zip(&owned)
                .map(|(n, o)| (n.path.as_str(), o.as_deref())),
        );
        // Gone notes first, so a PDF that a later note moves or writes to the same path is
        // never deleted as the PDF of a gone note.
        self.drop_gone(notes)?;
        for note in notes {
            self.note(note, &targets[&note.path])?;
        }
        self.prune_empty();
        Ok(())
    }

    /// Write the manifest when it changed. Only a finished pass names the current renderer:
    /// after a crash with another renderer id, the next pass renders every note again.
    pub(super) fn save(&mut self, finished: bool) -> Result<()> {
        self.m.version = manifest::VERSION;
        if finished {
            self.m.renderer = RENDERER_ID.to_string();
        }
        if self.m == self.saved {
            return Ok(());
        }
        manifest::save(&self.dir, &self.m)?;
        self.saved = self.m.clone();
        Ok(())
    }

    fn changed(&mut self) -> Result<()> {
        self.changes += 1;
        if self.changes.is_multiple_of(SAVE_EVERY) {
            self.save(false)?;
        }
        Ok(())
    }

    /// Delete the PDFs of notes the store no longer has, and their rows.
    fn drop_gone(&mut self, notes: &[Note]) -> Result<()> {
        let live: HashSet<String> = notes.iter().map(|n| n.id.to_string()).collect();
        let gone: Vec<String> = self
            .m
            .notes
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for key in gone {
            let Some(row) = self.m.notes.remove(&key) else {
                continue;
            };
            if row.pdf {
                let pdf = row.pdf_path();
                self.remove_pdf(&pdf)?;
                self.report.deleted.push(pdf);
            }
            self.changed()?;
        }
        Ok(())
    }

    /// Bring the PDF of note `n` up to date; `target` is its PDF path in this pass.
    fn note(&mut self, n: &Note, target: &str) -> Result<()> {
        // A registered note that is missing on disk waits for the scan: its PDF stays.
        let Some(disk) = &n.on_disk else {
            return Ok(());
        };
        // Without a current cache row the bytes are read and hashed here (and rendered from
        // these bytes if needed); nothing is cached.
        let (md5, bytes) = match &disk.md5 {
            Some(md5) => (md5.clone(), None),
            None => {
                let Some(bytes) = self.read_note(n, &disk.fs_path) else {
                    return Ok(());
                };
                (md5_hex(&bytes), Some(bytes))
            }
        };
        let key = n.id.to_string();
        let row = self.m.notes.get(&key).cloned();
        if let Some(row) = &row
            && !self.stale
            && row.md5 == md5
        {
            if row.path == n.path && (!row.pdf || row.pdf_path() == target) {
                return Ok(());
            }
            if !row.pdf {
                // A note that failed and moved: still no PDF, and no retry until it changes.
                self.set_row(&key, n, &md5, None);
                return self.changed();
            }
            if self.move_pdf(&key, row, n, target, &md5)? {
                return self.changed();
            }
        }
        // A foreign file at the target is kept, so the note is not read or rendered at all.
        let old = row.as_ref().filter(|r| r.pdf).map(Row::pdf_path);
        if !self.is_ours(target, old.as_deref()) {
            self.report.kept_foreign.push(target.to_string());
            return Ok(());
        }
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => {
                // The cache row was current at the listing; a note that changed since (its
                // bytes don't match) is left for the next pass, which its change wakes. One that
                // can't be read is tried again.
                let Some(bytes) = self.read_note(n, &disk.fs_path) else {
                    return Ok(());
                };
                if md5_hex(&bytes) != md5 {
                    return Ok(());
                }
                bytes
            }
        };
        self.render_one(&key, row.as_ref(), n, target, &md5, bytes)
    }

    /// The bytes of a note. A note that is gone now waits for the scan, which journals the
    /// change and wakes the next pass. Any other read error is a disk error to try again.
    fn read_note(&mut self, n: &Note, fs_path: &Path) -> Option<Vec<u8>> {
        match std::fs::read(fs_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                self.disk_error(n, io_err(fs_path)(e).to_string());
                None
            }
        }
    }

    /// A note failed on the disk: it counts as failed, and the pass asks to be tried again.
    fn disk_error(&mut self, n: &Note, e: String) {
        if self.report.retry.is_none() {
            self.report.retry = Some(format!("{}: {e}", n.path));
        }
        self.report.failed.push((n.path.clone(), e));
    }

    /// The row of note `n`; `target` is the path of its PDF, `None` when it has none.
    fn set_row(&mut self, key: &str, n: &Note, md5: &str, target: Option<&str>) {
        let row = Row {
            path: n.path.clone(),
            md5: md5.to_string(),
            pdf: target.is_some(),
            target: target
                .filter(|t| *t != pdf_path_of(&n.path))
                .map(str::to_string),
        };
        self.m.notes.insert(key.to_string(), row);
    }

    /// The file at `rel` below `<root>/<slug>/`; `None` when `rel` is not a wire path in its
    /// plain form (absolute, with `..`, empty), so no path of a row or a note can reach a file
    /// outside the account folder.
    pub(super) fn file_of(&self, rel: &str) -> Option<PathBuf> {
        manifest::is_wire_path(rel)
            .then(|| WirePath::parse_named(rel).ok())
            .flatten()
            .map(|w| w.to_fs(&self.dir))
    }

    /// Move the PDF of a moved note. False when the old PDF is gone (render it instead).
    fn move_pdf(
        &mut self,
        key: &str,
        row: &Row,
        n: &Note,
        target: &str,
        md5: &str,
    ) -> Result<bool> {
        let (old, new) = (row.pdf_path(), target.to_string());
        let (Some(from), Some(to)) = (self.file_of(&old), self.file_of(&new)) else {
            return Ok(false);
        };
        if !from.is_file() {
            return Ok(false);
        }
        if !self.is_ours(&new, Some(&old)) {
            self.report.kept_foreign.push(new);
            return Ok(true);
        }
        self.release(&new, key);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        crate::fsutil::rename_durable(&from, &to)?;
        self.note_emptied(&from);
        self.set_row(key, n, md5, Some(&new));
        self.report.moved.push((old, new));
        Ok(true)
    }

    /// Render one note from `bytes` (whose MD5 is `md5`) and write its PDF.
    fn render_one(
        &mut self,
        key: &str,
        row: Option<&Row>,
        n: &Note,
        target: &str,
        md5: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let slug = self.slug.clone();
        let new = target.to_string();
        let old = row.filter(|r| r.pdf).map(Row::pdf_path);
        let pdf = match render_pdf(bytes) {
            Ok(pdf) => pdf,
            Err(e) => {
                eprintln!("notedown render: {slug}: {}: {e}", n.path);
                if let Some(old) = &old {
                    self.remove_pdf(old)?;
                }
                self.set_row(key, n, md5, None);
                self.report.failed.push((n.path.clone(), e));
                return self.changed();
            }
        };
        // Checked again: a file may have been put there while the note was rendered.
        if !self.is_ours(&new, old.as_deref()) {
            self.report.kept_foreign.push(new);
            return Ok(());
        }
        let Some(target) = self.file_of(&new) else {
            return Ok(());
        };
        self.release(&new, key);
        if let Err(e) = crate::fsutil::write_atomic(&target, &pdf, false) {
            // The row stays as it was, so the retry renders the note again.
            self.disk_error(n, e.to_string());
            return Ok(());
        }
        if let Some(old) = old
            && old != new
            && !self
                .file_of(&old)
                .is_some_and(|old| same_file(&old, &target))
        {
            self.remove_pdf(&old)?;
        }
        self.set_row(key, n, md5, Some(&new));
        self.report.written.push(new);
        self.changed()
    }

    /// Delete one of our PDFs; one that is gone already is fine. A bad path deletes nothing.
    fn remove_pdf(&mut self, rel: &str) -> Result<()> {
        let Some(path) = self.file_of(rel) else {
            return Ok(());
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        self.note_emptied(&path);
        Ok(())
    }

    fn note_emptied(&mut self, file: &Path) {
        if let Some(parent) = file.parent() {
            self.emptied.insert(parent.to_path_buf());
        }
    }

    /// Remove the folders that lost a file and are empty now, up to the account folder
    /// (exclusive). A folder that holds anything else stays. Only folders strictly below the
    /// account folder by plain names (no `..`) are touched.
    fn prune_empty(&mut self) {
        for folder in std::mem::take(&mut self.emptied) {
            let below = folder.strip_prefix(&self.dir).is_ok_and(|rel| {
                rel.components().next().is_some()
                    && rel
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
            });
            if !below {
                continue;
            }
            let mut d = folder.as_path();
            while d != self.dir && d.starts_with(&self.dir) {
                if std::fs::remove_dir(d).is_err() {
                    break;
                }
                match d.parent() {
                    Some(p) => d = p,
                    None => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::notes::OnDisk;

    fn fixture(dir: &Path, name: &str) -> PathBuf {
        let src =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note");
        let to = dir.join(name);
        std::fs::copy(src, &to).unwrap();
        to
    }

    fn note(id: u64, path: &str, fs_path: &Path) -> Note {
        Note {
            id,
            path: path.into(),
            on_disk: Some(OnDisk {
                fs_path: fs_path.to_path_buf(),
                md5: None,
            }),
        }
    }

    fn run(dir: &Path, notes: &[Note]) -> PassReport {
        let mut pass = Pass::new("acc", dir.to_path_buf(), false);
        pass.run(notes).unwrap();
        pass.save(true).unwrap();
        pass.report
    }

    fn fixture2(dir: &Path, name: &str) -> PathBuf {
        let src =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test2.note");
        let to = dir.join(name);
        std::fs::copy(src, &to).unwrap();
        to
    }

    /// The PDF the pass writes for the note file at `path`.
    fn pdf_of(path: &Path) -> Vec<u8> {
        render_pdf(std::fs::read(path).unwrap()).unwrap()
    }

    fn read(out: &Path, rel: &str) -> Vec<u8> {
        std::fs::read(out.join(rel)).unwrap()
    }

    /// Two notes that map to the same PDF, driven through the pass directly: their note files
    /// sit under other names, so this runs on a disk that ignores case too.
    #[test]
    fn two_notes_with_the_same_pdf_get_two_pdfs() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (one, two) = (
            fixture(src.path(), "one.note"),
            fixture2(src.path(), "two.note"),
        );
        let notes = [
            note(2, "NOTE/Note/a.NOTE", &two),
            note(1, "NOTE/Note/a.note", &one),
        ];
        // Both are new: byte order decides.
        let r = run(out.path(), &notes);
        let mut written = r.written.clone();
        written.sort();
        assert_eq!(written, ["NOTE/Note/a.note.pdf", "NOTE/Note/a.pdf"]);
        let m = manifest::load(out.path());
        assert_eq!(m.notes["1"].pdf_path(), "NOTE/Note/a.note.pdf");
        assert_eq!(m.notes["2"].pdf_path(), "NOTE/Note/a.pdf");
        assert_eq!(m.notes["1"].path, "NOTE/Note/a.note");
        assert_eq!(read(out.path(), "NOTE/Note/a.pdf"), pdf_of(&two));
        assert_eq!(read(out.path(), "NOTE/Note/a.note.pdf"), pdf_of(&one));

        let bytes = std::fs::read(manifest::path(out.path())).unwrap();
        assert_eq!(run(out.path(), &notes), PassReport::default());
        assert_eq!(std::fs::read(manifest::path(out.path())).unwrap(), bytes);

        // The first note is gone: its PDF is deleted, the other one keeps its name.
        let r = run(out.path(), &notes[1..]);
        assert_eq!(r.deleted, ["NOTE/Note/a.pdf"]);
        assert!(r.moved.is_empty() && r.written.is_empty(), "{r:?}");
        assert!(!out.path().join("NOTE/Note/a.pdf").exists());
        assert_eq!(read(out.path(), "NOTE/Note/a.note.pdf"), pdf_of(&one));
        assert_eq!(run(out.path(), &notes[1..]), PassReport::default());
    }

    /// The device's own `a.note` has `a.pdf`; `A.note` placed next to it (it sorts first) gets
    /// its full name. On a render folder that ignores case, `A.pdf` would be `a.pdf`.
    #[test]
    fn a_note_keeps_its_pdf_when_another_note_collides() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (one, two) = (
            fixture(src.path(), "one.note"),
            fixture2(src.path(), "two.note"),
        );
        let lower = note(1, "NOTE/Note/a.note", &one);
        run(out.path(), std::slice::from_ref(&lower));
        let pdf_one = read(out.path(), "NOTE/Note/a.pdf");

        let notes = [note(2, "NOTE/Note/A.note", &two), lower];
        let r = run(out.path(), &notes);
        assert_eq!(r.written, ["NOTE/Note/A.note.pdf"]);
        assert!(r.moved.is_empty() && r.deleted.is_empty(), "{r:?}");
        assert_eq!(read(out.path(), "NOTE/Note/a.pdf"), pdf_one);
        assert_eq!(read(out.path(), "NOTE/Note/A.note.pdf"), pdf_of(&two));
        assert_eq!(run(out.path(), &notes), PassReport::default());
    }

    /// A note missing on disk keeps its PDF and its path when a colliding note appears.
    #[test]
    fn a_missing_note_keeps_its_pdf_when_another_note_collides() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (one, two) = (
            fixture(src.path(), "one.note"),
            fixture2(src.path(), "two.note"),
        );
        run(out.path(), &[note(1, "NOTE/Note/a.note", &one)]);
        let pdf_one = read(out.path(), "NOTE/Note/a.pdf");

        let missing = Note {
            id: 1,
            path: "NOTE/Note/a.note".into(),
            on_disk: None,
        };
        let notes = [note(2, "NOTE/Note/a.NOTE", &two), missing];
        let r = run(out.path(), &notes);
        assert_eq!(r.written, ["NOTE/Note/a.NOTE.pdf"]);
        assert!(r.moved.is_empty() && r.deleted.is_empty(), "{r:?}");
        assert_eq!(read(out.path(), "NOTE/Note/a.pdf"), pdf_one);
        assert_eq!(read(out.path(), "NOTE/Note/a.NOTE.pdf"), pdf_of(&two));
        let m = manifest::load(out.path());
        assert_eq!(m.notes["1"].pdf_path(), "NOTE/Note/a.pdf");
        assert_eq!(run(out.path(), &notes), PassReport::default());
    }

    /// `b.note` moves to `c.note` while a new `B.note` takes `B.pdf`, which is `b.pdf` on a
    /// render folder that ignores case. Each note ends with its own PDF.
    #[test]
    fn a_new_note_on_the_old_pdf_of_a_moved_note_gets_its_own_pdf() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (one, two) = (
            fixture(src.path(), "one.note"),
            fixture2(src.path(), "two.note"),
        );
        run(out.path(), &[note(1, "NOTE/Note/b.note", &one)]);
        let notes = [
            note(2, "NOTE/Note/B.note", &two),
            note(1, "NOTE/Note/c.note", &one),
        ];
        run(out.path(), &notes);
        assert_eq!(read(out.path(), "NOTE/Note/B.pdf"), pdf_of(&two));
        assert_eq!(read(out.path(), "NOTE/Note/c.pdf"), pdf_of(&one));
        let m = manifest::load(out.path());
        assert_eq!(m.notes["1"].pdf_path(), "NOTE/Note/c.pdf");
        assert_eq!(m.notes["2"].pdf_path(), "NOTE/Note/B.pdf");
        assert_eq!(run(out.path(), &notes), PassReport::default());
    }

    /// A manifest from before collisions were handled: `A.note -> A.pdf` and `a.note -> a.pdf`
    /// on a render folder that ignores case are one file. One pass gives each note its own PDF.
    #[test]
    fn two_rows_on_one_file_get_their_own_pdfs() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (one, two) = (
            fixture(src.path(), "one.note"),
            fixture2(src.path(), "two.note"),
        );
        let dir = out.path().join("NOTE/Note");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.pdf"), pdf_of(&two)).unwrap();
        if !dir.join("A.pdf").exists() {
            // A render folder that keeps case: two files, nothing to repair.
            return;
        }
        let row = |path: &str, file: &Path| Row {
            path: path.into(),
            md5: md5_hex(&std::fs::read(file).unwrap()),
            pdf: true,
            target: None,
        };
        let mut m = Manifest {
            version: manifest::VERSION,
            renderer: RENDERER_ID.into(),
            ..Manifest::default()
        };
        m.notes.insert("1".into(), row("NOTE/Note/A.note", &one));
        m.notes.insert("2".into(), row("NOTE/Note/a.note", &two));
        manifest::save(out.path(), &m).unwrap();

        let notes = [
            note(1, "NOTE/Note/A.note", &one),
            note(2, "NOTE/Note/a.note", &two),
        ];
        run(out.path(), &notes);
        assert_eq!(read(out.path(), "NOTE/Note/A.pdf"), pdf_of(&one));
        assert_eq!(read(out.path(), "NOTE/Note/a.note.pdf"), pdf_of(&two));
        let m = manifest::load(out.path());
        assert_eq!(m.notes["1"].pdf_path(), "NOTE/Note/A.pdf");
        assert_eq!(m.notes["2"].pdf_path(), "NOTE/Note/a.note.pdf");
        assert_eq!(run(out.path(), &notes), PassReport::default());
    }
}
