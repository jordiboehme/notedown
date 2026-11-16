//! Downloads: a file of the library goes into the Downloads folder of this Mac
//! (docs/rest-api.md, `GET /files/{id}`).
//!
//! **Server data never becomes a path unchecked.** Before any path is built, the entry must be a
//! file whose md5 is 32 lower-case hex characters and whose name is one safe path component (no
//! `/`, no NUL, no leading dot, so no `.` or `..`, at most 255 bytes). Every joined path is
//! checked to stay in the Downloads folder.
//!
//! **The bytes** go into a temp file in the Downloads folder, `.<name>.<job>.part` (the dot
//! keeps it out of sight, and it is on the same volume as the target). It is made with
//! `O_EXCL` and opened with `O_NOFOLLOW`, so a symlink put in its place is refused. A retry
//! resumes it with `Range` and `If-Range: "<md5>"`; when the server sends the whole file again
//! (200), the temp file is cut to nothing first. The `ETag` must be the md5 of the entry (else
//! the file changed on the server), and no more bytes than the entry's size are written. The
//! body is watched (`transfer.rs`): progress about four times a second, a minute without a new
//! byte ends the job ("The connection stalled."), and a cancel drops the request.
//!
//! **The last step.** The finished temp file must have the md5 of the entry, else it is
//! removed. Then it is hard-linked to the first free name (`name`, `name (2).ext`, ...): a link
//! never replaces a file, so a file that appears meanwhile only moves the download to the next
//! number. Then the temp file is removed.
//!
//! **Temp files of earlier runs** (`.<x>.<digits>.part`, files or symlinks only) are removed
//! once a run, before the first download of the run writes. Not at the start of the app: that
//! would read the Downloads folder (and could raise the macOS privacy question) before the user
//! asked for anything.
//!
//! **Show in Finder** runs `/usr/bin/open -R -- <path>` with the path as one argument, and only
//! for a file this run downloaded, by the exact path its job answered.
//!
//! A failed download keeps its temp file for Retry; a cancel and a new binding remove it.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;

use md5::{Digest as _, Md5};
use reqwest::header::{CONTENT_RANGE, ETAG};

use super::{Entry, EntryKind, Inner, Job, JobKind, JobState, Library, Session, lock};
use crate::rest::RestError;
use crate::transfer::{Watched, watched};

/// A name that cannot be a file name in the Downloads folder.
pub const BAD_NAME: &str = "The name of this file cannot be used on this Mac.";
/// An entry without a usable md5.
pub const NO_MD5: &str = "The server sent no valid md5 for this file.";
/// The bytes did not have the md5 of the entry, or there were more of them than its size.
pub const NOT_WHOLE: &str = "The file did not arrive whole. Try again.";
/// The server sent another version of the file than the entry named.
pub const CHANGED: &str = "The file was changed on the server. Try again.";
/// No Downloads folder was given, or it is gone.
pub const NO_FOLDER: &str = "Notedown cannot find the Downloads folder.";
/// The temp file or the file could not be written.
pub const NOT_SAVED: &str = "The file could not be saved in the Downloads folder.";
/// Show in Finder for a path this run did not download.
pub const NOT_OURS: &str = "Notedown did not download this file.";
/// Show in Finder for a downloaded file that is gone since.
pub const GONE: &str = "This file is not there anymore.";

/// How many downloads run at a time.
const AT_ONCE: usize = 2;
/// The longest file name macOS takes, in bytes.
const NAME_MAX: usize = 255;
/// A download tries `name (2).ext` up to `name (<this>).ext`.
const MAX_COPY: u32 = 100;
/// The size of one piece read for the md5.
const CHUNK: usize = 64 * 1024;

/// The download state of a library.
#[derive(Default)]
pub(super) struct Shared {
    /// The Downloads folder; none until the app gives it.
    dir: Mutex<Option<PathBuf>>,
    /// The temp files of earlier runs were removed. Held while they are removed, so no
    /// download of this run writes before.
    swept: Mutex<bool>,
    /// The queue and the jobs.
    queue: Mutex<Queue>,
    /// The files this run downloaded, as their jobs answered them.
    finished: Mutex<HashSet<PathBuf>>,
}

#[derive(Default)]
struct Queue {
    /// Every download that has not ended well: queued, running or failed (for Retry).
    items: BTreeMap<u64, Item>,
    order: VecDeque<u64>,
    /// The download threads, and how many of them run a download now.
    workers: usize,
    busy: usize,
    handles: Vec<JoinHandle<()>>,
}

/// What a download is about: the checked name, md5 and size of the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Want {
    name: String,
    md5: String,
    size: u64,
}

struct Item {
    job: Job,
    generation: u64,
    entry_id: u64,
    want: Want,
    /// A retry asks for the entry again: the file may have changed on the server.
    look_again: bool,
    /// The temp file, once a run made it.
    part: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
}

/// What a download needs while it runs (a copy, so no lock is held).
struct Work {
    id: u64,
    generation: u64,
    entry_id: u64,
    want: Want,
    look_again: bool,
    /// The temp file an earlier run made: a retry keeps it, also when the name changed.
    part: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
}

/// How one run of a download ended.
pub(super) enum End {
    Done {
        path: PathBuf,
        size: u64,
        label: String,
    },
    Failed(String),
    Cancelled,
}

/// Why the bytes stopped coming.
enum Stop {
    Rest(RestError),
    /// The ETag is not the md5 of the entry, or the range does not fit the file.
    Changed,
    /// More bytes than the size of the entry, or a range that starts elsewhere.
    NotWhole,
    /// The temp file could not be written.
    Disk,
}

/// Whether `name` is one safe path component.
pub(super) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\0')
}

/// The checked name, md5 and size of `entry`, or why it cannot be downloaded.
fn checked(entry: &Entry) -> Result<Want, String> {
    if entry.kind != EntryKind::File {
        return Err(RestError::Server {
            status: 409,
            code: Some("is_a_folder".to_owned()),
        }
        .to_string());
    }
    let md5 = entry
        .md5
        .clone()
        .filter(|m| super::uploads::is_md5(m))
        .ok_or_else(|| NO_MD5.to_owned())?;
    if !safe_name(&entry.name) {
        return Err(BAD_NAME.to_owned());
    }
    let size = entry
        .size
        .ok_or_else(|| RestError::Garbled("no size for the file".to_owned()).to_string())?;
    Ok(Want {
        name: entry.name.clone(),
        md5,
        size,
    })
}

/// The longest start of `s` with at most `max` bytes, cut at a char boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The name of the temp file of job `id`: `.<name>.<id>.part`, the name cut so it fits.
fn part_name(name: &str, id: u64) -> String {
    let tail = format!(".{id}.part");
    let room = NAME_MAX.saturating_sub(1 + tail.len());
    format!(".{}{tail}", cut(name, room))
}

/// Whether `name` is a temp file name of a download: `.<x>.<digits>.part`.
fn is_part_name(name: &str) -> bool {
    let Some(inner) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".part")) else {
        return false;
    };
    match inner.rsplit_once('.') {
        Some((stem, digits)) => {
            !stem.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// The `n`th name for `name`: the name itself, then `name (2).ext` and up, the stem cut so the
/// name fits in [`NAME_MAX`] bytes.
fn nth_name(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_owned();
    }
    let (stem, suffix) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() + 8 < NAME_MAX => {
            (stem, format!(" ({n}).{ext}"))
        }
        _ => (name, format!(" ({n})")),
    };
    format!(
        "{}{suffix}",
        cut(stem, NAME_MAX.saturating_sub(suffix.len()))
    )
}

/// `dir` joined with `name`, checked to stay in `dir`.
fn inside(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(name);
    (path.parent() == Some(dir) && path.file_name().is_some_and(|n| n == name)).then_some(path)
}

/// The md5 of the open temp file, read from its start, as lower-case hex.
fn md5_of(file: &mut File) -> std::io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(crate::login::hex(&hasher.finalize()))
}

/// A new file at `path`; fails when anything is there, a symlink too.
fn create_new(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Opens the temp file of a run. A first run only makes a new one; a retry opens the one its
/// failed run left (or a new one when that is gone). Never through a symlink.
fn open_part(path: &Path, first: bool) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    if first {
        return create_new(path);
    }
    let opened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    match opened {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_new(path),
        other => other,
    }
}

/// Puts the finished temp file `part` in `dir` under the first free name for `name` and
/// answers that path; never replaces a file. `link` is `fs::hard_link`. Where the volume has no
/// hard links, the name is taken with an empty new file first and the temp file is renamed
/// over that placeholder.
fn place_file(
    dir: &Path,
    part: &Path,
    name: &str,
    link: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf, String> {
    use std::io::ErrorKind::AlreadyExists;
    for n in 1..=MAX_COPY {
        let Some(target) = inside(dir, &nth_name(name, n)) else {
            return Err(BAD_NAME.to_owned());
        };
        match link(part, &target) {
            Ok(()) => {
                let _ = fs::remove_file(part);
                return Ok(target);
            }
            // Never replaces: the next number.
            Err(e) if e.kind() == AlreadyExists => continue,
            Err(_) => {}
        }
        match create_new(&target) {
            Ok(_) => {}
            Err(e) if e.kind() == AlreadyExists => continue,
            Err(_) => return Err(NOT_SAVED.to_owned()),
        }
        // Replaces only the empty placeholder just made.
        if fs::rename(part, &target).is_ok() {
            return Ok(target);
        }
        let _ = fs::remove_file(&target);
        return Err(NOT_SAVED.to_owned());
    }
    Err(format!(
        "There are too many copies of {name} in the Downloads folder."
    ))
}

/// Removes the temp files of earlier runs from `dir`: files and symlinks named like one, not
/// what they point to, nothing in sub-folders.
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_part_name(name) {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() || m.file_type().is_symlink()) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Reads the answer of `GET /files/{id}` into `file` from `from` on, counting the position.
async fn fetch(
    session: &Session,
    entry_id: u64,
    want: &Want,
    file: &mut File,
    from: u64,
    count: &AtomicU64,
) -> Result<(), Stop> {
    let mut response = session
        .rest()
        .get_file(entry_id, from, &want.md5)
        .await
        .map_err(Stop::Rest)?;
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if etag != format!("\"{}\"", want.md5) {
        return Err(Stop::Changed);
    }
    let mut at = if response.status().as_u16() == 206 {
        let start = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes "))
            .and_then(|v| v.split_once('-'))
            .and_then(|(start, _)| start.parse::<u64>().ok());
        if start != Some(from) {
            return Err(Stop::NotWhole);
        }
        from
    } else {
        // The whole file: what a failed run left goes.
        file.set_len(0).map_err(|_| Stop::Disk)?;
        0
    };
    file.seek(SeekFrom::Start(at)).map_err(|_| Stop::Disk)?;
    count.store(at, Ordering::SeqCst);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Stop::Rest(RestError::Cut))?
    {
        let len = chunk.len() as u64;
        if at.saturating_add(len) > want.size {
            return Err(Stop::NotWhole);
        }
        file.write_all(&chunk).map_err(|_| Stop::Disk)?;
        at += len;
        count.store(at, Ordering::SeqCst);
    }
    // An answer that ended early: what came is kept for a retry.
    if at < want.size {
        return Err(Stop::Rest(RestError::Cut));
    }
    Ok(())
}

impl Library {
    /// The Downloads folder of this Mac (an absolute path); tests give a temp folder.
    pub fn set_downloads(&self, dir: PathBuf) {
        *lock(&self.inner.downloads.dir) = dir.is_absolute().then_some(dir);
    }

    fn downloads_dir(&self) -> Option<PathBuf> {
        lock(&self.inner.downloads.dir)
            .clone()
            .filter(|dir| dir.is_dir())
    }

    /// Downloads the file `id` into the Downloads folder: checks the entry and queues a
    /// `download` job, whose events follow. Answers the job as it was queued. Blocking (it asks
    /// for the entry): run it off the main thread.
    pub fn download(&self, id: u64) -> Result<Job, String> {
        let session = self.session().map_err(|e| e.to_string())?;
        let result = session
            .rest()
            .get_json::<Entry>(&format!("/entries/{id}"), &[]);
        self.settle(&session, &result);
        let want = checked(&result.map_err(|e| e.to_string())?)?;
        if self.downloads_dir().is_none() {
            return Err(NO_FOLDER.to_owned());
        }
        let mut queue = lock(&self.inner.downloads.queue);
        // A binding came since the session: these jobs would be forgotten next.
        if session.is_cancelled() {
            return Err(super::LibraryError::Changed.to_string());
        }
        let job = Job {
            id: self.inner.next_job.fetch_add(1, Ordering::SeqCst) + 1,
            kind: JobKind::Download,
            label: want.name.clone(),
            done: 0,
            total: Some(want.size),
            state: JobState::Queued,
            text: None,
            question: None,
            result: None,
        };
        self.send(&job);
        queue.items.insert(
            job.id,
            Item {
                job: job.clone(),
                generation: session.generation(),
                entry_id: id,
                want,
                look_again: false,
                part: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        queue.order.push_back(job.id);
        self.start_downloads(&mut queue);
        Ok(job)
    }

    /// Whether `job` is a download this library still holds.
    pub(super) fn has_download(&self, job: u64) -> bool {
        lock(&self.inner.downloads.queue).items.contains_key(&job)
    }

    /// Tries a failed download again, from the bytes it has.
    pub(super) fn retry_download(&self, job: u64) -> Result<(), String> {
        let mut queue = lock(&self.inner.downloads.queue);
        let Some(item) = queue.items.get_mut(&job) else {
            return Err(super::uploads::JOB_GONE.to_owned());
        };
        if item.job.state != JobState::Failed {
            return Err("Only a failed download can be tried again.".to_owned());
        }
        item.look_again = true;
        item.job.state = JobState::Queued;
        item.job.text = None;
        self.send(&item.job);
        queue.order.push_back(job);
        self.start_downloads(&mut queue);
        Ok(())
    }

    /// Ends the download `job` wherever it is and removes its temp file; answers whether it
    /// was one.
    pub(super) fn cancel_download(&self, job: u64) -> bool {
        let mut queue = lock(&self.inner.downloads.queue);
        let Some(mut item) = queue.items.remove(&job) else {
            return false;
        };
        queue.order.retain(|q| *q != job);
        item.cancel.store(true, Ordering::SeqCst);
        if let Some(part) = &item.part {
            let _ = fs::remove_file(part);
        }
        item.job.state = JobState::Cancelled;
        item.job.text = None;
        self.send(&item.job);
        true
    }

    /// A new binding: every download ends.
    pub(super) fn cancel_downloads(&self) {
        let ids: Vec<u64> = lock(&self.inner.downloads.queue)
            .items
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.cancel_download(id);
        }
    }

    /// The command of Show in Finder for `path`: `/usr/bin/open -R -- <path>`, only for a file
    /// this run downloaded (the exact path its job answered) that is still there.
    pub fn reveal_command(&self, path: &str) -> Result<std::process::Command, String> {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !lock(&self.inner.downloads.finished).contains(&path) {
            return Err(NOT_OURS.to_owned());
        }
        if fs::symlink_metadata(&path).is_err() {
            return Err(GONE.to_owned());
        }
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg("-R").arg("--").arg(&path);
        Ok(command)
    }

    /// Starts download threads up to [`AT_ONCE`] for what is queued.
    fn start_downloads(&self, queue: &mut Queue) {
        queue.handles.retain(|h| !h.is_finished());
        let wanted = AT_ONCE.min(queue.busy + queue.order.len());
        while queue.workers < wanted {
            queue.workers += 1;
            let weak = Arc::downgrade(&self.inner);
            queue
                .handles
                .push(std::thread::spawn(move || download_worker(&weak)));
        }
    }

    /// The next download for a worker, or none: the worker ends.
    fn take_download(&self) -> Option<Work> {
        let mut queue = lock(&self.inner.downloads.queue);
        while let Some(id) = queue.order.pop_front() {
            let Some(item) = queue.items.get_mut(&id) else {
                continue;
            };
            item.job.state = JobState::Running;
            item.job.text = None;
            self.send(&item.job);
            let work = Work {
                id,
                generation: item.generation,
                entry_id: item.entry_id,
                want: item.want.clone(),
                look_again: item.look_again,
                part: item.part.clone(),
                cancel: item.cancel.clone(),
            };
            queue.busy += 1;
            return Some(work);
        }
        queue.workers -= 1;
        None
    }

    /// Takes the end of a run into the queue and tells the window.
    fn finish_download(&self, id: u64, end: End) {
        lock(&self.inner.downloads.queue).busy -= 1;
        self.settle_download(id, end);
    }

    /// Takes the end of a run of job `id` into the queue and tells the window. A file that was
    /// placed counts, also when a cancel came meanwhile: the job ends Done, never with the file
    /// removed.
    pub(super) fn settle_download(&self, id: u64, end: End) {
        let mut queue = lock(&self.inner.downloads.queue);
        if let End::Done { path, size, label } = end {
            let shown = path.to_string_lossy().into_owned();
            lock(&self.inner.downloads.finished).insert(path);
            let mut job = match queue.items.remove(&id) {
                Some(item) => item.job,
                None => Job {
                    id,
                    kind: JobKind::Download,
                    label,
                    done: 0,
                    total: Some(size),
                    state: JobState::Done,
                    text: None,
                    question: None,
                    result: None,
                },
            };
            job.state = JobState::Done;
            job.done = size;
            job.total = Some(size);
            job.text = None;
            job.result = Some(shown);
            self.send(&job);
            return;
        }
        // Cancelled meanwhile: the canceller told the window.
        let Some(item) = queue.items.get_mut(&id) else {
            return;
        };
        match end {
            End::Done { .. } => unreachable!("handled above"),
            End::Failed(text) => {
                item.job.state = JobState::Failed;
                item.job.text = Some(text);
                self.send(&item.job);
            }
            End::Cancelled => {
                drop(queue);
                self.cancel_download(id);
            }
        }
    }

    /// The progress of the running download `id`.
    fn download_progress(&self, id: u64, done: u64) {
        let mut queue = lock(&self.inner.downloads.queue);
        if let Some(item) = queue.items.get_mut(&id)
            && item.job.state == JobState::Running
        {
            item.job.done = done;
            self.send(&item.job);
        }
    }

    /// Keeps what a run found out about the job: the entry it has now, its temp file.
    fn note_download(&self, id: u64, want: Option<&Want>, part: Option<&Path>) {
        let mut queue = lock(&self.inner.downloads.queue);
        if let Some(item) = queue.items.get_mut(&id) {
            if let Some(want) = want {
                item.want = want.clone();
                item.look_again = false;
                if item.job.total != Some(want.size) || item.job.label != want.name {
                    item.job.total = Some(want.size);
                    item.job.label = want.name.clone();
                    self.send(&item.job);
                }
            }
            if let Some(part) = part {
                item.part = Some(part.to_owned());
            }
        }
    }

    /// One run of a download, without the lock.
    fn run_download(&self, work: &Work) -> End {
        let session = match self.session() {
            Ok(session) if session.generation() == work.generation => session,
            Ok(_) => return End::Cancelled,
            Err(e) => return End::Failed(e.to_string()),
        };
        let stopped = || session.is_cancelled() || work.cancel.load(Ordering::SeqCst);
        let Some(dir) = self.downloads_dir() else {
            return End::Failed(NO_FOLDER.to_owned());
        };
        {
            let mut swept = lock(&self.inner.downloads.swept);
            if !*swept {
                sweep(&dir);
                *swept = true;
            }
        }
        let part = match &work.part {
            Some(part) => part.clone(),
            None => match inside(&dir, &part_name(&work.want.name, work.id)) {
                Some(part) => part,
                None => return End::Failed(BAD_NAME.to_owned()),
            },
        };
        self.note_download(work.id, None, Some(&part));
        let mut want = work.want.clone();
        if work.look_again {
            // The file may have changed on the server since the last run.
            let result = session
                .rest()
                .get_json::<Entry>(&format!("/entries/{}", work.entry_id), &[]);
            self.settle(&session, &result);
            let now = match result.map_err(|e| e.to_string()).and_then(|e| checked(&e)) {
                Ok(now) => now,
                Err(text) => return End::Failed(text),
            };
            // The bytes of another version are no start. The temp file keeps the job's first
            // name; a new name changes only the target.
            if now.md5 != want.md5 {
                let _ = fs::remove_file(&part);
            }
            want = now;
            self.note_download(work.id, Some(&want), None);
        }
        if stopped() {
            return End::Cancelled;
        }
        let mut file = match open_part(&part, work.part.is_none()) {
            Ok(file) => file,
            Err(_) => return End::Failed(NOT_SAVED.to_owned()),
        };
        let Ok(meta) = file.metadata() else {
            return End::Failed(NOT_SAVED.to_owned());
        };
        // A failed run that had every byte: when they are right, place them with no request;
        // else they are fetched again (a whole answer cuts the file first).
        if meta.len() == want.size && md5_of(&mut file).is_ok_and(|md5| md5 == want.md5) {
            return place_checked(&dir, &part, &want);
        }
        // What a failed run left counts only when it is shorter than the file.
        let from = if meta.len() < want.size {
            meta.len()
        } else {
            0
        };
        let count = AtomicU64::new(from);
        let ended = watched(
            fetch(&session, work.entry_id, &want, &mut file, from, &count),
            &count,
            u64::MAX,
            &self.inner.clock,
            &stopped,
            &mut |done| self.download_progress(work.id, done),
        );
        match ended {
            Err(text) => End::Failed(text),
            Ok(Watched::Cancelled) => {
                let _ = fs::remove_file(&part);
                End::Cancelled
            }
            Ok(Watched::Stalled) => End::Failed(RestError::Stalled.to_string()),
            Ok(Watched::Done(Err(Stop::Rest(e)))) => {
                self.settle(&session, &Err::<(), _>(e.clone()));
                End::Failed(e.to_string())
            }
            Ok(Watched::Done(Err(Stop::Changed))) => {
                let _ = fs::remove_file(&part);
                End::Failed(CHANGED.to_owned())
            }
            Ok(Watched::Done(Err(Stop::NotWhole))) => {
                let _ = fs::remove_file(&part);
                End::Failed(NOT_WHOLE.to_owned())
            }
            Ok(Watched::Done(Err(Stop::Disk))) => End::Failed(NOT_SAVED.to_owned()),
            Ok(Watched::Done(Ok(()))) => {
                self.settle(&session, &Ok::<(), RestError>(()));
                self.place_download(&dir, &part, &mut file, &want)
            }
        }
    }

    /// The last step: the md5 of the temp file (through its open handle), then the first free
    /// name. A cancel from here on comes too late: the job ends Done.
    fn place_download(&self, dir: &Path, part: &Path, file: &mut File, want: &Want) -> End {
        match md5_of(file) {
            Ok(md5) if md5 == want.md5 => {}
            Ok(_) => {
                let _ = fs::remove_file(part);
                return End::Failed(NOT_WHOLE.to_owned());
            }
            Err(_) => return End::Failed(NOT_SAVED.to_owned()),
        }
        place_checked(dir, part, want)
    }

    /// Waits until no download thread runs. For tests: every thread ends before the test does.
    #[cfg(test)]
    pub(super) fn join_downloads(&self) {
        loop {
            let handles = std::mem::take(&mut lock(&self.inner.downloads.queue).handles);
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                handle.join().expect("a download thread panicked");
            }
        }
    }
}

/// Places a temp file whose md5 was checked under the first free name.
fn place_checked(dir: &Path, part: &Path, want: &Want) -> End {
    match place_file(dir, part, &want.name, &|from, to| fs::hard_link(from, to)) {
        Ok(path) => End::Done {
            path,
            size: want.size,
            label: want.name.clone(),
        },
        Err(text) => End::Failed(text),
    }
}

/// A download thread: runs downloads from the queue until it is empty. It holds the library
/// only while it runs one, so it never keeps a dropped library alive.
fn download_worker(weak: &Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let library = Library { inner };
        let Some(work) = library.take_download() else {
            return;
        };
        let end = library.run_download(&work);
        library.finish_download(work.id, end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_hard_links_a_placeholder_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        std::fs::write(dir.join("a.txt"), b"old").unwrap();
        let part = dir.join(".a.txt.1.part");
        std::fs::write(&part, b"hello").unwrap();
        let no_links = |_: &Path, _: &Path| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        };
        let placed = place_file(dir, &part, "a.txt", &no_links).unwrap();
        assert_eq!(placed, dir.join("a (2).txt"));
        assert_eq!(std::fs::read(&placed).unwrap(), b"hello");
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"old");
        assert!(!part.exists());
    }

    #[test]
    fn names_of_temp_files_and_copies() {
        assert_eq!(part_name("a.txt", 3), ".a.txt.3.part");
        let long = "z".repeat(255);
        let part = part_name(&long, 12);
        assert_eq!(part.len(), 255);
        assert!(part.ends_with(".12.part"));
        assert!(is_part_name(&part));
        assert!(is_part_name(".a.txt.3.part"));
        for not in [
            ".keep.part",
            "a.1.part",
            ".1.part",
            ".a.x1.part",
            ".a.1.part.txt",
        ] {
            assert!(!is_part_name(not), "{not}");
        }
        assert_eq!(nth_name("a.txt", 1), "a.txt");
        assert_eq!(nth_name("a.txt", 2), "a (2).txt");
        assert_eq!(nth_name("README", 3), "README (3)");
        // A multi-byte char is never cut in half.
        let wide = format!("{}.txt", "\u{e9}".repeat(125));
        assert_eq!(wide.len(), 254);
        let copy = nth_name(&wide, 2);
        assert!(copy.len() <= 255 && copy.ends_with(" (2).txt"), "{copy}");
        assert!(safe_name("-R x.txt"));
        assert!(!safe_name("a\0b"));
    }
}
