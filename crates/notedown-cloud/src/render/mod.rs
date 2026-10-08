//! PDFs of an account's notes in a folder outside the data tree, kept up to date by comparing
//! the store with a manifest (docs/superpowers/specs/2026-10-08-notedown-render-and-push-order-design.md 4).
//!
//! The pass touches only files that are its own: PDFs the manifest names, and at a target path
//! also a PDF that notedown made (see [`Pass::is_ours`]). Every other file in the render folder
//! is never moved, replaced or deleted.

pub mod manifest;

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use notedown_note::{Notebook, RENDERER_ID, RenderOptions, RenderStyle, VersionPolicy};

use crate::journal::FileKind;
use crate::store::{Kind, Store, WirePath};
use crate::{Error, Result, io_err};
use manifest::{Manifest, Row};

pub const LOCK_FILE: &str = ".notedown-render.lock";
/// The manifest is written after this many changed notes, so a crash loses little work.
const SAVE_EVERY: usize = 20;
/// The producer entry `render_pdf` writes into every PDF.
const PRODUCER: &[u8] = b"/Producer (notedown ";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassOptions {
    /// Render every note again, also those the manifest says are up to date.
    pub all: bool,
    /// Wait for the render lock of the account (and say so on stderr) instead of skipping.
    pub wait_for_lock: bool,
}

/// What a pass did. Paths are relative to `<root>/<slug>/`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// PDFs written.
    pub written: Vec<String>,
    /// PDFs moved: (old, new).
    pub moved: Vec<(String, String)>,
    /// PDFs deleted.
    pub deleted: Vec<String>,
    /// Notes that failed: (note path, error text).
    pub failed: Vec<(String, String)>,
    /// Target paths that hold a file notedown didn't write; it is left alone.
    pub kept_foreign: Vec<String>,
    /// Another pass held the render lock; nothing was done.
    pub busy: bool,
}

/// The wire path with its last `.note` extension (any case) replaced by `.pdf`; a path without
/// it gets `.pdf` appended.
pub fn pdf_path_of(wire_path: &str) -> String {
    let cut = wire_path.len().saturating_sub(".note".len());
    match wire_path.get(cut..) {
        Some(ext) if ext.eq_ignore_ascii_case(".note") => format!("{}.pdf", &wire_path[..cut]),
        _ => format!("{wire_path}.pdf"),
    }
}

/// One pass for the account of `store`: write, move or delete the PDFs below `<root>/<slug>/`
/// so they match the store's notes. `root` must exist. The store lock is held only for the
/// listing, never while a note is read or rendered.
pub fn render_account(store: &Store, root: &Path, opts: &PassOptions) -> Result<PassReport> {
    let dir = account_dir(store, root)?;
    let Some(_lock) = take_lock(&dir, store.slug(), opts.wait_for_lock)? else {
        return Ok(PassReport {
            busy: true,
            ..PassReport::default()
        });
    };
    let notes = list_notes(store)?;
    let (m, lost) = match manifest::read(&dir) {
        Some(m) => (m, false),
        None => (Manifest::default(), true),
    };
    let mut pass = Pass {
        store,
        stale: opts.all || m.renderer != RENDERER_ID,
        dir,
        m,
        lost,
        report: PassReport::default(),
        emptied: BTreeSet::new(),
        changes: 0,
    };
    let result = pass.run(&notes);
    // Also after an error: the rows of the work done so far stay true.
    pass.save(result.is_ok())?;
    result?;
    Ok(pass.report)
}

/// `<root>/<slug>`, created. `root` must be an existing folder outside the accounts.
fn account_dir(store: &Store, root: &Path) -> Result<PathBuf> {
    let invalid = |message: String| Error::Invalid {
        what: "render_dir",
        message,
    };
    if !root.is_dir() {
        return Err(invalid(format!("{} doesn't exist", root.display())));
    }
    let real_root = root.canonicalize().map_err(io_err(root))?;
    if let Some(accounts) = store.account_dir().parent()
        && let Ok(real_accounts) = accounts.canonicalize()
        && real_root.starts_with(&real_accounts)
    {
        return Err(invalid(format!(
            "{} is inside the data folder's accounts",
            root.display()
        )));
    }
    let dir = root.join(store.slug());
    std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    Ok(dir)
}

/// The render lock of the account folder; `None` when it is busy and `wait` is false.
fn take_lock(dir: &Path, slug: &str, wait: bool) -> Result<Option<std::fs::File>> {
    let path = dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err(&path))?;
    match file.try_lock() {
        Ok(()) => return Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) if !wait => return Ok(None),
        Err(std::fs::TryLockError::WouldBlock) => {
            eprintln!("waiting for the render lock of account {slug}");
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(io_err(&path)(e)),
    }
    file.lock().map_err(io_err(&path))?;
    Ok(Some(file))
}

/// A note of the store as the listing gave it.
struct Note {
    id: u64,
    wire: WirePath,
    path: String,
    md5: String,
}

/// The store's notes, sorted by path. The store lock is held for the listing only.
fn list_notes(store: &Store) -> Result<Vec<Note>> {
    let entries = {
        let g = store.lock()?;
        store.list(&g, &WirePath::root(), true)?
    };
    let mut notes: Vec<Note> = entries
        .into_iter()
        .filter(|e| e.tag == Kind::File && FileKind::of(&e.name) == FileKind::Note)
        .filter_map(|e| {
            Some(Note {
                id: e.id,
                path: e.path.display(),
                md5: e.content_hash?,
                wire: e.path,
            })
        })
        .collect();
    notes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(notes)
}

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

/// Whether `a` and `b` name the same file (a case-only rename on a case-insensitive disk).
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
        a.as_os_str().eq_ignore_ascii_case(b.as_os_str()) && a.exists()
    }
}

/// Whether the file at `path` is a PDF, and with `by_notedown` one that `render_pdf` made.
fn is_pdf(path: &Path, by_notedown: bool) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(b"%PDF-")
        && (!by_notedown || bytes.windows(PRODUCER.len()).any(|w| w == PRODUCER))
}

struct Pass<'a> {
    store: &'a Store,
    /// `<root>/<slug>`.
    dir: PathBuf,
    m: Manifest,
    /// The manifest was missing or unreadable.
    lost: bool,
    /// Every row counts as out of date for rendering (`--all` or another renderer id). The rows
    /// still say which PDFs are ours.
    stale: bool,
    report: PassReport,
    /// Folders that lost a file in this pass.
    emptied: BTreeSet<PathBuf>,
    changes: usize,
}

impl Pass<'_> {
    fn run(&mut self, notes: &[Note]) -> Result<()> {
        // Gone notes first, so a PDF that a later note moves or writes to the same path is
        // never deleted as the PDF of a gone note.
        self.drop_gone(notes)?;
        for note in notes {
            self.note(note)?;
        }
        self.prune_empty();
        Ok(())
    }

    /// Write the manifest. Only a finished pass names the current renderer: after a crash
    /// with another renderer id, the next pass renders every note again.
    fn save(&mut self, finished: bool) -> Result<()> {
        self.m.version = manifest::VERSION;
        if finished {
            self.m.renderer = RENDERER_ID.to_string();
        }
        manifest::save(&self.dir, &self.m)
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
                let pdf = pdf_path_of(&row.path);
                self.remove_pdf(&pdf)?;
                self.report.deleted.push(pdf);
            }
            self.changed()?;
        }
        Ok(())
    }

    fn note(&mut self, n: &Note) -> Result<()> {
        let key = n.id.to_string();
        let row = self.m.notes.get(&key).cloned();
        if let Some(row) = &row
            && !self.stale
            && row.md5 == n.md5
        {
            if row.path == n.path {
                return Ok(());
            }
            if !row.pdf {
                // A note that failed and moved: still no PDF, and no retry until it changes.
                self.set_row(&key, n, false);
                return self.changed();
            }
            if self.move_pdf(&key, row, n)? {
                return self.changed();
            }
        }
        self.render_one(&key, row.as_ref(), n)
    }

    fn set_row(&mut self, key: &str, n: &Note, pdf: bool) {
        let row = Row {
            path: n.path.clone(),
            md5: n.md5.clone(),
            pdf,
        };
        self.m.notes.insert(key.to_string(), row);
    }

    /// Move the PDF of a moved note. False when the old PDF is gone (render it instead).
    fn move_pdf(&mut self, key: &str, row: &Row, n: &Note) -> Result<bool> {
        let (old, new) = (pdf_path_of(&row.path), pdf_path_of(&n.path));
        let (from, to) = (self.dir.join(&old), self.dir.join(&new));
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
        self.set_row(key, n, true);
        self.report.moved.push((old, new));
        Ok(true)
    }

    /// Read, check and render one note. A note that changes right now (its bytes don't match
    /// the listing) or that can't be read is left for the next pass.
    fn render_one(&mut self, key: &str, row: Option<&Row>, n: &Note) -> Result<()> {
        let Ok(bytes) = std::fs::read(self.store.fs_path(&n.wire)) else {
            return Ok(());
        };
        if md5_hex(&bytes) != n.md5 {
            return Ok(());
        }
        let slug = self.store.slug().to_string();
        let new = pdf_path_of(&n.path);
        let old = row.filter(|r| r.pdf).map(|r| pdf_path_of(&r.path));
        let pdf = match render_pdf(bytes) {
            Ok(pdf) => pdf,
            Err(e) => {
                eprintln!("notedown render: {slug}: {}: {e}", n.path);
                if let Some(old) = &old {
                    self.remove_pdf(old)?;
                }
                self.set_row(key, n, false);
                self.report.failed.push((n.path.clone(), e));
                return self.changed();
            }
        };
        if !self.is_ours(&new, old.as_deref()) {
            self.report.kept_foreign.push(new);
            return Ok(());
        }
        self.release(&new, key);
        let target = self.dir.join(&new);
        if let Err(e) = crate::fsutil::write_atomic(&target, &pdf, false) {
            // The row stays as it was, so the next pass tries again.
            eprintln!("notedown render: {slug}: {}: {e}", n.path);
            self.report.failed.push((n.path.clone(), e.to_string()));
            return Ok(());
        }
        if let Some(old) = old
            && old != new
            && !same_file(&self.dir.join(&old), &target)
        {
            self.remove_pdf(&old)?;
        }
        self.set_row(key, n, true);
        self.report.written.push(new);
        self.changed()
    }

    /// Whether the pass may write to `rel`: nothing is there, or the file there is ours. Ours
    /// is a PDF a row names; the old PDF of this note (`own`) under another spelling of the
    /// name; a PDF that notedown made (written right before a crash, before its row was
    /// saved). When the manifest was lost, every PDF counts as ours: the PDFs of earlier passes
    /// are then unknown, and their names match notes of the store. A target that is not a PDF
    /// stays foreign.
    fn is_ours(&self, rel: &str, own: Option<&str>) -> bool {
        let target = self.dir.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            return true;
        };
        if !meta.is_file() {
            return false;
        }
        if self
            .m
            .notes
            .values()
            .any(|r| r.pdf && pdf_path_of(&r.path) == rel)
        {
            return true;
        }
        if own.is_some_and(|own| same_file(&self.dir.join(own), &target)) {
            return true;
        }
        is_pdf(&target, !self.lost)
    }

    /// Drop the rows of other notes whose PDF is at `rel`: the pass is about to replace that
    /// file. Such a note gets a new PDF when its turn comes (or in the next pass).
    fn release(&mut self, rel: &str, key: &str) {
        self.m
            .notes
            .retain(|k, r| k == key || !r.pdf || pdf_path_of(&r.path) != rel);
    }

    /// Delete one of our PDFs; one that is gone already is fine.
    fn remove_pdf(&mut self, rel: &str) -> Result<()> {
        let path = self.dir.join(rel);
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
    /// (exclusive). A folder that holds anything else stays.
    fn prune_empty(&mut self) {
        for folder in std::mem::take(&mut self.emptied) {
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
