//! Uploads: files and folders dropped on the library window go into a folder of the library
//! (docs/rest-api.md, `PUT /files?path=` and `POST /folders`).
//!
//! **Drops.** The paths of a drop come from the window's native drop event in Rust
//! (`windows.rs`), never from the web view: Rust keeps them under a drop id and the window gets
//! `{dropId, position, count}`. `library_upload(dropId, folder)` takes no paths, so a script in
//! the window can only upload what the user dropped. A drop is used once; only the newest
//! [`KEEP_DROPS`] are kept, and a new binding forgets them all.
//!
//! **The walk** reads with `symlink_metadata`: it skips symlinks (so no loop and nothing from
//! outside the drop), names with a leading dot (`.DS_Store`; the server refuses them), and
//! everything that is no regular file or folder. It does not go into packages (`.app`,
//! `.photoslibrary`, `.bundle`, `.pkg`) unless the user says so. A folder with nothing to upload
//! below it is made with `POST /folders`. Names go in NFC, as macOS hands them in NFD. More than
//! [`MANY`] files ask first, packages ask before that. A file over 1 GiB fails at once, with no
//! request.
//!
//! **Before any byte is sent** the target is looked up (`GET /entries?path=`). Free: `PUT` with
//! `If-None-Match: *`. A file there: the job asks (Replace sends `If-Match` with its md5, Keep
//! both picks the first free `name (2).ext` by looks, Skip ends it). A folder there, or a name
//! in another case or Unicode form (409 `name_taken` at the commit, after the body), asks with
//! Keep both or Skip; a folder on the way in another spelling fails with its name. A 412 at the
//! commit looks again and asks again.
//!
//! **The queue** runs two uploads at a time on threads of their own, which end when the queue is
//! empty. An answer of 423 `device_syncing` pauses the whole queue by `Retry-After`; after two
//! minutes of syncing the waiting and queued uploads fail ("The Supernote is syncing. Try again
//! in a moment."). A broken connection without an answer asks `GET /account`: when a device
//! syncs it waits as for a 423, else the job fails with "The connection was cut." and can be
//! tried again (with the target and answer it had). The body is watched (`transfer.rs`): the
//! progress shows about four times a second, a minute without a moving byte ends the job
//! ("The connection stalled."), and a cancel drops the request (the server discards what it
//! staged). A new binding cancels every upload.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use super::cache::name_of;
use super::changes::{WAIT_STEP, WAITING};
use super::{
    Clash, Entry, EntryKind, Inner, Job, JobKind, JobState, Library, LibraryEvent, Position,
    Session, lock,
};
use crate::rest::{Precondition, Put, RETRY_AFTER, RestError, SYNC_WAIT};
use crate::transfer::{FileBody, Watched, watched};

/// The answer to a drop id that is not kept (used, cancelled or too old).
pub const DROP_GONE: &str = "This drop is not there anymore. Drop the files again.";
/// A drop with nothing that can be uploaded.
pub const NOTHING: &str = "There is nothing in the drop to upload.";
/// The answer to a job that is not there (ended, cancelled, or of an earlier binding).
pub const JOB_GONE: &str = "This job is not there anymore.";
/// The text of a job that was skipped.
pub const SKIPPED: &str = "Skipped.";
/// A dropped file that cannot be read (or that became a symlink since the drop).
pub const UNREADABLE: &str = "This file could not be read.";
/// A dropped folder, or a part of it, that cannot be read (no permission, for example).
pub const UNREADABLE_FOLDER: &str = "This folder could not be read.";

/// Above this many files a drop asks first.
pub const MANY: usize = 500;
/// The largest file the server takes (1 GiB).
pub const FILE_MAX: u64 = 1 << 30;
/// How many uploads run at a time.
pub const AT_ONCE: usize = 2;
/// The walk stops at this many entries and asks before it goes on, so a dropped home folder
/// asks at once instead of being read for minutes.
pub const WALK_CAP: usize = 10_000;
/// A 423 later than this after the end of the last wait starts a new two minutes.
const PAUSE_GAP: Duration = Duration::from_secs(30);
/// How many drops are kept for `library_upload`.
pub const KEEP_DROPS: usize = 16;
/// Keep both tries `name (2).ext` up to `name (<this>).ext`.
const MAX_COPY: u32 = 100;
/// Folders whose contents the walk leaves alone unless asked: macOS packages.
const PACKAGES: [&str; 4] = [".app", ".photoslibrary", ".bundle", ".pkg"];

/// What `library_upload` asks before it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Ask {
    /// More than [`MANY`] files: call again with `many`.
    Many,
    /// The drop holds packages: call again with `packages` true (go into them) or false.
    Packages,
}

/// The question of `library_upload`, and its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadAsk {
    pub ask: Ask,
    pub text: String,
}

/// The answer to an upload that asks about a name in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Choice {
    Replace,
    KeepBoth,
    Skip,
}

/// The drops and the upload queue.
#[derive(Default)]
pub(super) struct Uploads {
    /// The paths of each drop, and the flag that stops its walk (Cancel all).
    drops: BTreeMap<u64, (Vec<PathBuf>, Arc<AtomicBool>)>,
    next_drop: u64,
    /// Every upload that has not ended well, by job id: queued, running, waiting, asking or
    /// failed (for Retry).
    items: BTreeMap<u64, Item>,
    /// The job ids to run, in order.
    queue: VecDeque<u64>,
    /// The upload threads, and how many of them run an upload now.
    workers: usize,
    busy: usize,
    handles: Vec<JoinHandle<()>>,
    /// The queue waits for a syncing Supernote.
    pause: Option<Pause>,
}

/// A wait for a syncing Supernote: since when, and until when nothing starts.
#[derive(Debug, Clone, Copy)]
struct Pause {
    since: u64,
    until: u64,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The pause after a 423 at `now` that asks to wait `retry_after`, or none when the sync has
/// lasted two minutes. A pause that ended more than [`PAUSE_GAP`] ago belongs to an earlier
/// sync and does not count; one that ended just now goes on (the retry met the same sync).
fn next_pause(pause: Option<Pause>, now: u64, retry_after: Duration) -> Option<Pause> {
    let pause = match pause {
        Some(p) if now <= p.until.saturating_add(ms(PAUSE_GAP)) => p,
        _ => Pause {
            since: now,
            until: now,
        },
    };
    let left = SYNC_WAIT.saturating_sub(Duration::from_millis(now.saturating_sub(pause.since)));
    if left.is_zero() {
        return None;
    }
    let wait = retry_after.max(Duration::from_secs(1)).min(left);
    Some(Pause {
        since: pause.since,
        until: pause.until.max(now.saturating_add(ms(wait))),
    })
}

/// What an upload does next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// Look the target up first.
    Look,
    /// Send it as a new file.
    Create,
    /// Replace the file with this md5.
    Replace(String),
    /// Pick the first free `name (n).ext`, then send it as a new file.
    KeepBoth,
}

struct Item {
    job: Job,
    drop: u64,
    generation: u64,
    source: PathBuf,
    /// An empty folder, made with `POST /folders`.
    folder: bool,
    /// A folder the walk could not read: it fails, also when tried again.
    unreadable: bool,
    /// The wire path it goes to first; Keep both counts its copies from this name.
    base: String,
    /// The wire path it goes to.
    target: String,
    next: Next,
    /// The md5 of the file in the way, for Replace.
    in_the_way: Option<String>,
    cancel: Arc<AtomicBool>,
}

/// One planned item of a walk: a file, or a folder with nothing to upload below it (`size`
/// none). `names` is the path below the target folder, in NFC. An unreadable folder (or an
/// entry of it that could not be read) is planned as a folder that fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Planned {
    pub source: PathBuf,
    pub names: Vec<String>,
    pub size: Option<u64>,
    pub unreadable: bool,
}

#[derive(Debug, Default)]
pub(super) struct Walk {
    pub items: Vec<Planned>,
    pub files: usize,
    /// The packages the walk did not go into.
    pub packages: Vec<String>,
    /// Folders, or entries of folders, that could not be read.
    pub unreadable: usize,
    /// The walk stopped at its cap: there is more.
    pub capped: bool,
    /// The drop was cancelled while it was walked.
    pub cancelled: bool,
    /// Entries looked at so far.
    seen: usize,
}

fn is_package(name: &str) -> bool {
    let name = name.to_lowercase();
    PACKAGES.iter().any(|ext| name.ends_with(ext))
}

/// The files and folders of the dropped `paths`, by the rules of the module. With a `cap` the
/// walk stops after that many entries (`capped`); a set `cancel` stops it (`cancelled`).
pub(super) fn walk(
    paths: &[PathBuf],
    into_packages: bool,
    cap: Option<usize>,
    cancel: &AtomicBool,
) -> Walk {
    let mut walk = Walk::default();
    let rules = Rules {
        into_packages,
        cap,
        cancel,
    };
    for path in paths {
        if walk.capped || walk.cancelled {
            break;
        }
        visit(path, &mut Vec::new(), &rules, &mut walk);
    }
    walk
}

struct Rules<'a> {
    into_packages: bool,
    cap: Option<usize>,
    cancel: &'a AtomicBool,
}

/// The folder at `path` (`names` with its name last) as an item that fails: it could not be
/// read, or not all of it.
fn unreadable(walk: &mut Walk, path: &Path, names: &[String]) {
    walk.unreadable += 1;
    walk.items.push(Planned {
        source: path.to_owned(),
        names: names.to_vec(),
        size: None,
        unreadable: true,
    });
}

/// Plans `path` with the names above it; answers whether anything was planned.
fn visit(path: &Path, names: &mut Vec<String>, rules: &Rules<'_>, walk: &mut Walk) -> bool {
    if rules.cancel.load(Ordering::SeqCst) {
        walk.cancelled = true;
        return false;
    }
    if rules.cap.is_some_and(|cap| walk.seen >= cap) {
        walk.capped = true;
        return false;
    }
    walk.seen += 1;
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with('.') {
        return false;
    }
    let name: String = name.nfc().collect();
    // Never follows a symlink: no loop, nothing from outside the drop.
    let Ok(meta) = fs::symlink_metadata(path) else {
        names.push(name);
        unreadable(walk, path, names);
        names.pop();
        return true;
    };
    let kind = meta.file_type();
    if kind.is_file() {
        names.push(name);
        walk.files += 1;
        walk.items.push(Planned {
            source: path.to_owned(),
            names: names.clone(),
            size: Some(meta.len()),
            unreadable: false,
        });
        names.pop();
        return true;
    }
    if !kind.is_dir() {
        return false;
    }
    if !rules.into_packages && is_package(&name) {
        walk.packages.push(name);
        return false;
    }
    names.push(name);
    let mut children = Vec::new();
    let mut whole = true;
    match fs::read_dir(path) {
        Ok(dir) => {
            for entry in dir {
                match entry {
                    Ok(entry) => children.push(entry.path()),
                    Err(_) => whole = false,
                }
            }
        }
        Err(_) => whole = false,
    }
    children.sort();
    let mut any = false;
    for child in children {
        any |= visit(&child, names, rules, walk);
    }
    if !whole {
        // Never made as an empty folder: what could not be read would look uploaded.
        unreadable(walk, path, names);
    } else if !any && !walk.capped && !walk.cancelled {
        walk.items.push(Planned {
            source: path.to_owned(),
            names: names.clone(),
            size: None,
            unreadable: false,
        });
    }
    names.pop();
    true
}

/// `n` with a comma between thousands: 1,234.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What to ask before a walk is uploaded: packages first, then a count over [`MANY`].
pub(super) fn ask_for(walk: &Walk, many: bool, packages: Option<bool>) -> Option<UploadAsk> {
    if packages.is_none() && !walk.packages.is_empty() {
        let first = &walk.packages[0];
        let text = if walk.packages.len() == 1 {
            format!("{first} is a package. Upload what is in it too?")
        } else {
            format!(
                "{} packages are in the drop, such as {first}. Upload what is in them too?",
                thousands(walk.packages.len())
            )
        };
        return Some(UploadAsk {
            ask: Ask::Packages,
            text,
        });
    }
    if walk.capped && !many {
        return Some(UploadAsk {
            ask: Ask::Many,
            text: format!(
                "This drop holds more than {} items. Upload them all?",
                thousands(WALK_CAP)
            ),
        });
    }
    if walk.files > MANY && !many {
        return Some(UploadAsk {
            ask: Ask::Many,
            text: format!("Upload {} files?", thousands(walk.files)),
        });
    }
    None
}

/// `name` in the folder `parent` (the root when empty).
fn join(parent: &str, name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// `a.txt` as its `n`th copy: `a (n).txt`; a name without an extension gets ` (n)` at the end.
fn copy_name(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

pub(super) fn is_md5(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A name the user could take for the same one: the same letters in another case or Unicode
/// form.
fn same_name(a: &str, b: &str) -> bool {
    let fold = |s: &str| s.nfc().collect::<String>().to_lowercase();
    fold(a) == fold(b)
}

/// What an upload needs while it runs (a copy, so no lock is held).
struct Work {
    id: u64,
    generation: u64,
    source: PathBuf,
    folder: bool,
    unreadable: bool,
    base: String,
    target: String,
    next: Next,
    cancel: Arc<AtomicBool>,
}

/// How one run of an upload ended.
enum End {
    Done {
        result: String,
        done: u64,
        replaced: bool,
    },
    /// A name in the way: the window asks.
    Ask {
        clash: Clash,
        md5: Option<String>,
    },
    Failed(String),
    /// A device syncs: the queue waits this long.
    Wait(Duration),
    Cancelled,
}

/// The end of a run with where it got: a retry or an answer goes on from there.
struct Ran {
    target: String,
    next: Next,
    end: End,
}

/// What a worker does next.
enum Take {
    Run(Work),
    Wait(Duration),
    Stop,
}

impl Uploads {
    /// An upload ended without a 423: once the wait is over, a later sync starts its own two
    /// minutes. An answer to a request that was sent before the pause began says nothing.
    fn pause_over(&mut self, now: u64) {
        if self.pause.is_some_and(|p| now >= p.until) {
            self.pause = None;
        }
    }

    /// The job `id` is now `state` with `text` (and no question), and the window hears it.
    fn set(&mut self, library: &Library, id: u64, state: JobState, text: Option<String>) {
        if let Some(item) = self.items.get_mut(&id) {
            item.job.state = state;
            item.job.text = text;
            item.job.question = None;
            library.send(&item.job);
        }
    }

    /// Ends the job `id` as cancelled, wherever it is.
    fn cancel(&mut self, library: &Library, id: u64) -> bool {
        let Some(mut item) = self.items.remove(&id) else {
            return false;
        };
        self.queue.retain(|q| *q != id);
        item.cancel.store(true, Ordering::SeqCst);
        item.job.state = JobState::Cancelled;
        item.job.text = None;
        item.job.question = None;
        library.send(&item.job);
        true
    }
}

impl Library {
    pub(super) fn send(&self, job: &Job) {
        (self.inner.emit)(LibraryEvent::Job { job: job.clone() });
    }

    /// Files or folders were dropped on the window at (`x`, `y`) (physical pixels): keeps their
    /// paths under a new drop id and tells the window. Never blocks.
    pub fn dropped(&self, paths: Vec<PathBuf>, x: f64, y: f64) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let mut up = lock(&self.inner.uploads);
        up.next_drop += 1;
        let drop_id = up.next_drop;
        up.drops
            .insert(drop_id, (paths, Arc::new(AtomicBool::new(false))));
        while up.drops.len() > KEEP_DROPS {
            up.drops.pop_first();
        }
        // Under the lock: the event comes before any job of this drop.
        let position = Position {
            x: x.round() as i64,
            y: y.round() as i64,
        };
        (self.inner.emit)(LibraryEvent::Drop {
            drop_id,
            position,
            count,
        });
    }

    /// Uploads the drop `drop` into `folder`, or answers what to ask first: packages (answered
    /// with `packages`), more than [`MANY`] files (answered with `many`). The jobs then run on
    /// threads of their own and send their events. Blocking (the walk reads the disk): run it
    /// off the main thread.
    pub fn upload(
        &self,
        drop: u64,
        folder: &str,
        many: bool,
        packages: Option<bool>,
    ) -> Result<Option<UploadAsk>, String> {
        let (paths, cancel) = lock(&self.inner.uploads)
            .drops
            .get(&drop)
            .cloned()
            .ok_or_else(|| DROP_GONE.to_owned())?;
        let session = self.session().map_err(|e| e.to_string())?;
        let cap = (!many).then_some(WALK_CAP);
        let walk = walk(&paths, packages == Some(true), cap, &cancel);
        if walk.cancelled {
            return Err(DROP_GONE.to_owned());
        }
        if let Some(ask) = ask_for(&walk, many, packages) {
            return Ok(Some(ask));
        }
        let mut up = lock(&self.inner.uploads);
        if up.drops.remove(&drop).is_none() {
            return Err(DROP_GONE.to_owned());
        }
        if walk.items.is_empty() {
            return Err(NOTHING.to_owned());
        }
        // A binding came since the session: it forgot the drop, or forgets these jobs next.
        if session.is_cancelled() {
            return Err(super::LibraryError::Changed.to_string());
        }
        for planned in walk.items {
            let id = self.inner.next_job.fetch_add(1, Ordering::SeqCst) + 1;
            let refused = if planned.unreadable {
                Some(UNREADABLE_FOLDER.to_owned())
            } else if planned.size.is_some_and(|size| size > FILE_MAX) {
                Some(RestError::TooLarge.to_string())
            } else {
                None
            };
            let job = Job {
                id,
                kind: JobKind::Upload,
                label: planned.names.join("/"),
                done: 0,
                total: planned.size,
                state: if refused.is_some() {
                    JobState::Failed
                } else {
                    JobState::Queued
                },
                text: refused.clone(),
                question: None,
                result: None,
            };
            self.send(&job);
            up.items.insert(
                id,
                Item {
                    job,
                    drop,
                    generation: session.generation(),
                    source: planned.source,
                    folder: planned.size.is_none(),
                    unreadable: planned.unreadable,
                    base: join(folder, &planned.names.join("/")),
                    target: join(folder, &planned.names.join("/")),
                    next: Next::Look,
                    in_the_way: None,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
            if refused.is_none() {
                up.queue.push_back(id);
            }
        }
        self.start_workers(&mut up);
        Ok(None)
    }

    /// The answer to an upload that asks about a name in the way.
    pub fn answer(&self, job: u64, choice: Choice) -> Result<(), String> {
        let mut up = lock(&self.inner.uploads);
        let Some(item) = up.items.get_mut(&job) else {
            return Err(JOB_GONE.to_owned());
        };
        if item.job.state != JobState::Asking {
            return Err("This upload asks nothing.".to_owned());
        }
        match choice {
            Choice::Replace => match &item.in_the_way {
                Some(md5) => item.next = Next::Replace(md5.clone()),
                None => return Err("Only a file can be replaced.".to_owned()),
            },
            Choice::KeepBoth => item.next = Next::KeepBoth,
            Choice::Skip => {
                let mut item = up.items.remove(&job).expect("the item is there");
                item.job.state = JobState::Done;
                item.job.text = Some(SKIPPED.to_owned());
                item.job.question = None;
                self.send(&item.job);
                return Ok(());
            }
        }
        item.in_the_way = None;
        up.set(self, job, JobState::Queued, None);
        up.queue.push_front(job);
        self.start_workers(&mut up);
        Ok(())
    }

    /// Tries a failed upload or download again, from where it was (an upload with its target
    /// and answer, a download with the bytes it has).
    pub fn retry(&self, job: u64) -> Result<(), String> {
        if self.has_download(job) {
            return self.retry_download(job);
        }
        let mut up = lock(&self.inner.uploads);
        let Some(item) = up.items.get(&job) else {
            return Err(JOB_GONE.to_owned());
        };
        if item.job.state != JobState::Failed {
            return Err("Only a failed upload can be tried again.".to_owned());
        }
        up.set(self, job, JobState::Queued, None);
        up.queue.push_back(job);
        self.start_workers(&mut up);
        Ok(())
    }

    /// Ends the job `job`: an upload or a download wherever it is (a running one drops its
    /// request), or a restore that asks for a name. A job that is not there is left alone.
    pub fn cancel(&self, job: u64) {
        if lock(&self.inner.uploads).cancel(self, job) || self.cancel_download(job) {
            return;
        }
        let asking = {
            let mut asking = lock(&self.inner.asking);
            let key = asking
                .iter()
                .find(|(_, j)| j.id == job)
                .map(|(key, _)| key.clone());
            key.and_then(|key| asking.remove(&key))
        };
        if let Some(mut job) = asking {
            job.state = JobState::Cancelled;
            job.text = None;
            job.question = None;
            self.send(&job);
        }
    }

    /// Cancel all of a drop: its jobs end, and an unused drop is forgotten.
    pub fn cancel_drop(&self, drop: u64) {
        let mut up = lock(&self.inner.uploads);
        if let Some((_, cancel)) = up.drops.remove(&drop) {
            // A walk of this drop stops.
            cancel.store(true, Ordering::SeqCst);
        }
        let ids: Vec<u64> = up
            .items
            .iter()
            .filter(|(_, item)| item.drop == drop)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            up.cancel(self, id);
        }
    }

    /// A new binding: every upload ends and the drops are forgotten.
    pub(super) fn cancel_uploads(&self) {
        let mut up = lock(&self.inner.uploads);
        for (_, cancel) in std::mem::take(&mut up.drops).into_values() {
            cancel.store(true, Ordering::SeqCst);
        }
        up.pause = None;
        let ids: Vec<u64> = up.items.keys().copied().collect();
        for id in ids {
            up.cancel(self, id);
        }
    }

    /// Starts upload threads up to [`AT_ONCE`] for what is queued.
    fn start_workers(&self, up: &mut Uploads) {
        up.handles.retain(|h| !h.is_finished());
        let wanted = AT_ONCE.min(up.busy + up.queue.len());
        while up.workers < wanted {
            up.workers += 1;
            let weak = Arc::downgrade(&self.inner);
            up.handles.push(std::thread::spawn(move || worker(&weak)));
        }
    }

    /// The next upload for a worker, a wait while the queue pauses, or the end of the worker.
    fn take(&self) -> Take {
        let mut up = lock(&self.inner.uploads);
        if let Some(pause) = up.pause {
            let now = (self.inner.clock)();
            if now < pause.until && !up.queue.is_empty() {
                return Take::Wait(Duration::from_millis(pause.until - now).min(WAIT_STEP));
            }
        }
        while let Some(id) = up.queue.pop_front() {
            let Some(item) = up.items.get_mut(&id) else {
                continue;
            };
            item.job.state = JobState::Running;
            item.job.text = None;
            item.job.question = None;
            item.job.done = 0;
            self.send(&item.job);
            let work = Work {
                id,
                generation: item.generation,
                source: item.source.clone(),
                folder: item.folder,
                unreadable: item.unreadable,
                base: item.base.clone(),
                target: item.target.clone(),
                next: item.next.clone(),
                cancel: item.cancel.clone(),
            };
            up.busy += 1;
            return Take::Run(work);
        }
        up.workers -= 1;
        if up.workers == 0 && up.busy == 0 {
            // Idle: the next sync starts its own two minutes.
            up.pause = None;
        }
        Take::Stop
    }

    /// Takes the end of a run into the queue and tells the window.
    fn finish(&self, id: u64, ran: Ran) {
        let mut up = lock(&self.inner.uploads);
        up.busy -= 1;
        let now = (self.inner.clock)();
        // Cancelled meanwhile: the canceller told the window.
        let Some(item) = up.items.get_mut(&id) else {
            return;
        };
        item.target = ran.target;
        item.next = ran.next;
        match ran.end {
            End::Done {
                result,
                done,
                replaced,
            } => {
                up.pause_over(now);
                let mut item = up.items.remove(&id).expect("the item is there");
                item.job.state = JobState::Done;
                item.job.text = None;
                item.job.done = done;
                item.job.result = Some(result);
                self.send(&item.job);
                if replaced {
                    (self.inner.emit)(LibraryEvent::Recycle);
                }
            }
            End::Ask { clash, md5 } => {
                item.in_the_way = md5;
                item.job.state = JobState::Asking;
                item.job.text = Some(
                    RestError::NameTaken {
                        existing: clash.existing.clone(),
                    }
                    .to_string(),
                );
                item.job.question = Some(clash);
                self.send(&item.job);
            }
            End::Failed(text) => {
                up.pause_over(now);
                up.set(self, id, JobState::Failed, Some(text));
            }
            End::Cancelled => {
                up.cancel(self, id);
            }
            End::Wait(retry_after) => {
                let Some(pause) = next_pause(up.pause, now, retry_after) else {
                    // Two minutes of syncing: what waits and what is queued ends; the next
                    // answer of a running upload ends at once too (the pause stays).
                    let text = RestError::Syncing { retry_after }.to_string();
                    up.set(self, id, JobState::Failed, Some(text.clone()));
                    let queued: Vec<u64> = up.queue.drain(..).collect();
                    for q in queued {
                        up.set(self, q, JobState::Failed, Some(text.clone()));
                    }
                    return;
                };
                up.pause = Some(pause);
                up.set(self, id, JobState::Waiting, Some(WAITING.to_owned()));
                up.queue.push_front(id);
            }
        }
    }

    /// The progress of the running job `id`.
    fn progress(&self, id: u64, done: u64) {
        let mut up = lock(&self.inner.uploads);
        if let Some(item) = up.items.get_mut(&id)
            && item.job.state == JobState::Running
        {
            item.job.done = done;
            self.send(&item.job);
        }
    }

    /// One run of an upload, without the lock.
    fn run(&self, work: &Work) -> Ran {
        let mut target = work.target.clone();
        let mut next = work.next.clone();
        let end = self.run_steps(work, &mut target, &mut next);
        Ran { target, next, end }
    }

    fn run_steps(&self, work: &Work, target: &mut String, next: &mut Next) -> End {
        let session = match self.session() {
            Ok(session) if session.generation() == work.generation => session,
            Ok(_) => return End::Cancelled,
            Err(e) => return End::Failed(e.to_string()),
        };
        let stopped = || session.is_cancelled() || work.cancel.load(Ordering::SeqCst);
        if work.unreadable {
            return End::Failed(UNREADABLE_FOLDER.to_owned());
        }
        // Checked before any request, without following a symlink: the source can have grown,
        // or been swapped for a symlink, since the walk.
        let meta = fs::symlink_metadata(&work.source);
        if work.folder {
            if !meta.is_ok_and(|m| m.is_dir()) {
                return End::Failed(UNREADABLE_FOLDER.to_owned());
            }
            return self.make_folder(&session, target);
        }
        match meta {
            Ok(meta) if !meta.is_file() => return End::Failed(UNREADABLE.to_owned()),
            Ok(meta) if meta.len() > FILE_MAX => {
                return End::Failed(RestError::TooLarge.to_string());
            }
            Ok(_) => {}
            Err(_) => return End::Failed(UNREADABLE.to_owned()),
        }
        // A commit that found the name taken after a free look looks again; a few times only.
        for _ in 0..3 {
            if stopped() {
                return End::Cancelled;
            }
            if *next == Next::KeepBoth {
                match self.free_name(&session, &work.base, &stopped) {
                    Ok(free) => {
                        *target = free;
                        *next = Next::Create;
                    }
                    Err(end) => return end,
                }
            }
            if *next == Next::Look {
                match self.look(&session, target) {
                    Ok(None) => *next = Next::Create,
                    Ok(Some(entry)) => return in_the_way(target, &entry),
                    Err(e) => return End::Failed(e.to_string()),
                }
            }
            if stopped() {
                return End::Cancelled;
            }
            let precondition = match next {
                Next::Replace(md5) => Precondition::Replace(md5.clone()),
                _ => Precondition::Create,
            };
            match self.put(&session, work, target, &precondition) {
                Err(PutStop::Rest(RestError::PreconditionFailed)) => *next = Next::Look,
                Err(stop) => return self.put_failed(&session, target, stop),
                Ok((put, done)) => {
                    if !session.is_cancelled() {
                        self.wrote(&put.seqs);
                    }
                    return End::Done {
                        result: put.path,
                        done,
                        replaced: put.outcome == "replaced",
                    };
                }
            }
        }
        End::Failed(RestError::PreconditionFailed.to_string())
    }

    /// What a failed `PUT` means for the job.
    fn put_failed(&self, session: &Session, target: &str, stop: PutStop) -> End {
        match stop {
            PutStop::Cancelled => End::Cancelled,
            PutStop::Text(text) => End::Failed(text),
            PutStop::Rest(RestError::Syncing { retry_after }) => End::Wait(retry_after),
            PutStop::Rest(RestError::NameTaken { existing }) => {
                let name = name_of(target);
                if same_name(&existing, name) {
                    End::Ask {
                        clash: Clash {
                            name: name.to_owned(),
                            existing,
                            can_replace: false,
                        },
                        md5: None,
                    }
                } else {
                    // A folder on the way in another spelling: no other name helps.
                    End::Failed(RestError::NameTaken { existing }.to_string())
                }
            }
            PutStop::Rest(RestError::Server { status: 409, code })
                if code.as_deref() == Some("is_a_folder") =>
            {
                let name = name_of(target).to_owned();
                End::Ask {
                    clash: Clash {
                        existing: name.clone(),
                        name,
                        can_replace: false,
                    },
                    md5: None,
                }
            }
            PutStop::Rest(RestError::Cut) => {
                // An early refusal may come as a broken connection: is a device syncing?
                #[derive(Deserialize)]
                struct Account {
                    #[serde(default)]
                    devices: Vec<Device>,
                }
                #[derive(Deserialize)]
                struct Device {
                    #[serde(default)]
                    syncing: bool,
                }
                let account = session.rest().get_json::<Account>("/account", &[]);
                match account {
                    Ok(account) if account.devices.iter().any(|d| d.syncing) => {
                        End::Wait(RETRY_AFTER)
                    }
                    _ => End::Failed(RestError::Cut.to_string()),
                }
            }
            PutStop::Rest(e) => End::Failed(e.to_string()),
        }
    }

    /// `GET /entries?path=`: the entry at `path`, or none.
    fn look(&self, session: &Session, path: &str) -> Result<Option<Entry>, RestError> {
        let result = session
            .rest()
            .get_json::<Entry>("/entries", &[("path", path)]);
        match result {
            Err(RestError::NotFound) => Ok(None),
            result => {
                self.settle(session, &result);
                result.map(Some)
            }
        }
    }

    /// The first free `name (n).ext` next to `base` (the first target, so a second Keep both
    /// does not make `name (2) (2).ext`), by looks.
    fn free_name(
        &self,
        session: &Session,
        base: &str,
        stopped: &dyn Fn() -> bool,
    ) -> Result<String, End> {
        let (parent, name) = base.rsplit_once('/').unwrap_or(("", base));
        for n in 2..=MAX_COPY {
            if stopped() {
                return Err(End::Cancelled);
            }
            let candidate = join(parent, &copy_name(name, n));
            match self.look(session, &candidate) {
                Ok(None) => return Ok(candidate),
                Ok(Some(_)) => {}
                Err(e) => return Err(End::Failed(e.to_string())),
            }
        }
        Err(End::Failed(format!(
            "There are too many copies of {name} here."
        )))
    }

    /// `POST /folders` for a folder with nothing to upload below it.
    fn make_folder(&self, session: &Session, target: &str) -> End {
        #[derive(Serialize)]
        struct FolderBody<'a> {
            path: &'a str,
        }
        #[derive(Deserialize)]
        struct Made {
            path: String,
            #[serde(default)]
            seqs: Vec<u64>,
        }
        let result = session
            .rest()
            .post_json_once::<_, Made>("/folders", &FolderBody { path: target });
        self.settle(session, &result);
        match result {
            Ok(made) => {
                if !session.is_cancelled() {
                    self.wrote(&made.seqs);
                }
                End::Done {
                    result: made.path,
                    done: 0,
                    replaced: false,
                }
            }
            Err(RestError::Syncing { retry_after }) => End::Wait(retry_after),
            Err(e) => End::Failed(e.to_string()),
        }
    }

    /// The `PUT` of the file, watched: progress, the stall watchdog, a cancel.
    fn put(
        &self,
        session: &Session,
        work: &Work,
        target: &str,
        precondition: &Precondition,
    ) -> Result<(Put, u64), PutStop> {
        use std::os::unix::fs::OpenOptionsExt;
        // A symlink put in place of the file since the walk is refused here.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&work.source)
            .map_err(|_| PutStop::Text(UNREADABLE.into()))?;
        let len = file
            .metadata()
            .map_err(|_| PutStop::Text(UNREADABLE.into()))?
            .len();
        if len > FILE_MAX {
            return Err(PutStop::Rest(RestError::TooLarge));
        }
        {
            let mut up = lock(&self.inner.uploads);
            if let Some(item) = up.items.get_mut(&work.id)
                && item.job.total != Some(len)
            {
                item.job.total = Some(len);
                self.send(&item.job);
            }
        }
        let count = Arc::new(AtomicU64::new(0));
        let unreadable = Arc::new(AtomicBool::new(false));
        let body = FileBody::new(file, len, count.clone(), unreadable.clone());
        let cancelled = || session.is_cancelled() || work.cancel.load(Ordering::SeqCst);
        let ended = watched(
            session.rest().put_file(target, body, precondition),
            &count,
            len,
            &self.inner.clock,
            &cancelled,
            &mut |done| self.progress(work.id, done),
        )
        .map_err(PutStop::Text)?;
        match ended {
            Watched::Cancelled => Err(PutStop::Cancelled),
            Watched::Stalled => Err(PutStop::Rest(RestError::Stalled)),
            Watched::Done(result) => {
                if unreadable.load(Ordering::SeqCst) {
                    return Err(PutStop::Text(UNREADABLE.into()));
                }
                self.settle(session, &result);
                result.map(|put| (put, len)).map_err(PutStop::Rest)
            }
        }
    }

    /// Waits until no upload thread runs. For tests: every thread ends before the test does.
    #[cfg(test)]
    pub(super) fn join_uploads(&self) {
        loop {
            let handles = std::mem::take(&mut lock(&self.inner.uploads).handles);
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                handle.join().expect("an upload thread panicked");
            }
        }
    }
}

/// Why a `PUT` gave no file.
enum PutStop {
    Rest(RestError),
    /// A local reason, shown as it is.
    Text(String),
    Cancelled,
}

/// The job asks about `entry`, which is at `target`.
fn in_the_way(target: &str, entry: &Entry) -> End {
    let name = name_of(target).to_owned();
    let file = entry.kind == EntryKind::File;
    let md5 = entry.md5.clone().filter(|_| file);
    if file && !md5.as_deref().is_some_and(is_md5) {
        return End::Failed(RestError::Garbled("no md5 for the file".to_owned()).to_string());
    }
    End::Ask {
        clash: Clash {
            name,
            existing: entry.name.clone(),
            can_replace: file,
        },
        md5,
    }
}

/// An upload thread: runs uploads from the queue until it is empty. It holds the library only
/// while it runs one, so it never keeps a dropped library alive.
fn worker(weak: &Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let library = Library { inner };
        match library.take() {
            Take::Stop => return,
            Take::Wait(d) => {
                let sleeper = library.inner.sleeper.clone();
                drop(library);
                sleeper(d);
            }
            Take::Run(work) => {
                let ran = library.run(&work);
                library.finish(work.id, ran);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_423_long_after_the_last_pause_starts_a_new_one() {
        let wait = Duration::from_secs(5);
        let first = next_pause(None, 1_000, wait).unwrap();
        assert_eq!((first.since, first.until), (1_000, 6_000));
        // Still syncing right after the wait: the same pause goes on.
        let on = next_pause(Some(first), 6_000, wait).unwrap();
        assert_eq!((on.since, on.until), (1_000, 11_000));
        // Two minutes of it: give up.
        let long = Pause {
            since: 0,
            until: 120_000,
        };
        assert!(next_pause(Some(long), 120_000, wait).is_none());
        // A pause that ended long ago does not count: a new sync starts its own two minutes.
        let old = Pause {
            since: 0,
            until: 5_000,
        };
        let new = next_pause(Some(old), 200_000, wait).unwrap();
        assert_eq!((new.since, new.until), (200_000, 205_000));
    }

    #[test]
    fn names_of_copies_and_counts() {
        assert_eq!(copy_name("a.txt", 2), "a (2).txt");
        assert_eq!(copy_name("archive.tar.gz", 3), "archive.tar (3).gz");
        assert_eq!(copy_name("README", 2), "README (2)");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(501), "501");
        assert_eq!(thousands(1234), "1,234");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert!(is_package("Photos Library.photoslibrary"));
        assert!(is_package("Foo.APP"));
        assert!(!is_package("notes.txt"));
        assert!(same_name("A.txt", "a.txt"));
        assert!(same_name("Cafe\u{301}", "caf\u{e9}"));
        assert!(!same_name("trip", "b.txt"));
    }
}
