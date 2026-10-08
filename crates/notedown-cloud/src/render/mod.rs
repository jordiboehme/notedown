//! PDFs of an account's notes in a folder outside the data tree, kept up to date by comparing
//! the store with a manifest (docs/superpowers/specs/2026-10-08-notedown-render-and-push-order-design.md 4).
//!
//! The pass reads the store without changing it: no ids, no MD5 cache rows, no journal
//! events. It renders only notes the id registry knows, so a note placed by hand waits for the
//! scan, which journals it first.
//!
//! The pass touches only files that are its own: PDFs the manifest names, and at a target path
//! also a PDF that notedown made (see [`Pass::is_ours`]). Every other file in the render folder
//! is never moved, replaced or deleted.

pub mod follow;
pub mod manifest;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use notedown_note::{Notebook, RENDERER_ID, RenderOptions, RenderStyle, VersionPolicy};

use crate::journal::FileKind;
use crate::store::{Store, WirePath};
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
    /// The first disk error of the pass (`<note path>: <error>`): a note could not be read or
    /// its PDF not written. Its row stays as it was, and another pass may work, so the server
    /// tries again. A note that can't be rendered is no disk error.
    pub retry: Option<String>,
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
/// so they match the store's notes. `root` must exist. The store lock is held only while the
/// registry and the MD5 cache are read, never while a note is read or rendered.
pub fn render_account(store: &Store, root: &Path, opts: &PassOptions) -> Result<PassReport> {
    let dir = account_dir(store, root)?;
    let Some(_lock) = take_lock(&dir, store.slug(), opts.wait_for_lock)? else {
        return Ok(PassReport {
            busy: true,
            ..PassReport::default()
        });
    };
    // An account without `files/` has nothing to render. The pass reads the store only, so
    // it never lets the store lock create the folder; the manifest and the PDFs stay.
    if !store.files_dir().is_dir() {
        return Ok(PassReport::default());
    }
    let notes = list_notes(store)?;
    let m = manifest::load(&dir);
    let mut pass = Pass {
        slug: store.slug().to_string(),
        stale: opts.all || m.renderer != RENDERER_ID,
        dir,
        m,
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

/// `<root>/<slug>`, created. `root` must be an existing folder, and `<root>/<slug>` must be
/// neither the data folder, nor inside it, nor hold it (`root` may hold the data folder).
fn account_dir(store: &Store, root: &Path) -> Result<PathBuf> {
    let invalid = |message: String| Error::Invalid {
        what: "render_dir",
        message,
    };
    if !root.is_dir() {
        return Err(invalid(format!("{} doesn't exist", root.display())));
    }
    let dir = root.join(store.slug());
    // The real path: through a symlink at `<root>/<slug>` too, when it is there.
    let real_dir = match dir.canonicalize() {
        Ok(real) => real,
        Err(_) => root
            .canonicalize()
            .map_err(io_err(root))?
            .join(store.slug()),
    };
    // `<data>/accounts/<slug>` is the account folder.
    if let Some(data) = store.account_dir().parent().and_then(Path::parent)
        && let Ok(real_data) = data.canonicalize()
    {
        if real_dir.starts_with(&real_data) {
            return Err(invalid(format!(
                "{} is inside the data folder {} or is that folder",
                dir.display(),
                data.display()
            )));
        }
        // The other way round, the pass would write PDFs and prune folders next to the data.
        if real_data.starts_with(&real_dir) {
            return Err(invalid(format!(
                "{} holds the data folder {}",
                dir.display(),
                data.display()
            )));
        }
    }
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

/// A note the id registry knows.
struct Note {
    id: u64,
    path: String,
    /// The file on disk; `None` when it is missing (the scan journals the delete or move).
    on_disk: Option<OnDisk>,
}

struct OnDisk {
    fs_path: PathBuf,
    /// From the MD5 cache when its row is current for the file; `None`: hash the bytes.
    md5: Option<String>,
}

/// The registered notes, sorted by path, read under a short store guard. Changes nothing in
/// the store: a file on disk the registry doesn't know is not listed (the scan adds it).
fn list_notes(store: &Store) -> Result<Vec<Note>> {
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

/// Whether the file at `path` is a PDF that `render_pdf` made.
fn is_notedown_pdf(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(b"%PDF-") && bytes.windows(PRODUCER.len()).any(|w| w == PRODUCER)
}

struct Pass {
    slug: String,
    /// `<root>/<slug>`.
    dir: PathBuf,
    m: Manifest,
    /// Every row counts as out of date for rendering (`--all` or another renderer id). The rows
    /// still say which PDFs are ours.
    stale: bool,
    report: PassReport,
    /// Folders that lost a file in this pass.
    emptied: BTreeSet<PathBuf>,
    changes: usize,
}

impl Pass {
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
            if row.path == n.path {
                return Ok(());
            }
            if !row.pdf {
                // A note that failed and moved: still no PDF, and no retry until it changes.
                self.set_row(&key, n, &md5, false);
                return self.changed();
            }
            if self.move_pdf(&key, row, n, &md5)? {
                return self.changed();
            }
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
        self.render_one(&key, row.as_ref(), n, &md5, bytes)
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

    fn set_row(&mut self, key: &str, n: &Note, md5: &str, pdf: bool) {
        let row = Row {
            path: n.path.clone(),
            md5: md5.to_string(),
            pdf,
        };
        self.m.notes.insert(key.to_string(), row);
    }

    /// The file at `rel` below `<root>/<slug>/`; `None` when `rel` is not a wire path in its
    /// plain form (absolute, with `..`, empty), so no path of a row or a note can reach a file
    /// outside the account folder.
    fn file_of(&self, rel: &str) -> Option<PathBuf> {
        manifest::is_wire_path(rel)
            .then(|| WirePath::parse_named(rel).ok())
            .flatten()
            .map(|w| w.to_fs(&self.dir))
    }

    /// Move the PDF of a moved note. False when the old PDF is gone (render it instead).
    fn move_pdf(&mut self, key: &str, row: &Row, n: &Note, md5: &str) -> Result<bool> {
        let (old, new) = (pdf_path_of(&row.path), pdf_path_of(&n.path));
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
        self.set_row(key, n, md5, true);
        self.report.moved.push((old, new));
        Ok(true)
    }

    /// Render one note from `bytes` (whose MD5 is `md5`) and write its PDF.
    fn render_one(
        &mut self,
        key: &str,
        row: Option<&Row>,
        n: &Note,
        md5: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let slug = self.slug.clone();
        let new = pdf_path_of(&n.path);
        let old = row.filter(|r| r.pdf).map(|r| pdf_path_of(&r.path));
        let pdf = match render_pdf(bytes) {
            Ok(pdf) => pdf,
            Err(e) => {
                eprintln!("notedown render: {slug}: {}: {e}", n.path);
                if let Some(old) = &old {
                    self.remove_pdf(old)?;
                }
                self.set_row(key, n, md5, false);
                self.report.failed.push((n.path.clone(), e));
                return self.changed();
            }
        };
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
        self.set_row(key, n, md5, true);
        self.report.written.push(new);
        self.changed()
    }

    /// Whether the pass may write to `rel`: nothing is there, or the file there is ours. Ours
    /// is a PDF a row names; the old PDF of this note (`own`) under another spelling of the
    /// name; a PDF that notedown made (its producer entry), which covers a PDF written right
    /// before a crash and the PDFs of earlier passes after the manifest was lost. Any other
    /// file, a PDF from another program too, stays foreign, also on the first pass.
    fn is_ours(&self, rel: &str, own: Option<&str>) -> bool {
        let Some(target) = self.file_of(rel) else {
            return false;
        };
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
        if own
            .and_then(|own| self.file_of(own))
            .is_some_and(|own| same_file(&own, &target))
        {
            return true;
        }
        is_notedown_pdf(&target)
    }

    /// Drop the rows of other notes whose PDF is at `rel`: the pass is about to replace that
    /// file. Such a note gets a new PDF when its turn comes (or in the next pass).
    fn release(&mut self, rel: &str, key: &str) {
        self.m
            .notes
            .retain(|k, r| k == key || !r.pdf || pdf_path_of(&r.path) != rel);
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
