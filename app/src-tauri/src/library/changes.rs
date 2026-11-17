//! Changes to the library's files and the recycle bin (docs/rest-api.md, "Write" and "Recycle
//! bin"): a new folder, a rename, a move, a delete, a restore and Empty.
//!
//! Each change is a `change` job: the window gets a `job` event when it starts, waits or ends,
//! and the command answers the job as it ended. The cache is not touched here: the server's
//! events bring each change, in order like every other one (`wrote` wakes the follower). After
//! a delete, a restore and Empty the window hears `recycle` and lists the bin again.
//!
//! A name the user typed is sent in NFC and checked before any request: a `/` in it would make
//! folders, as the server creates missing folders on the way. Paths that come from the server
//! (a parent, a target folder, an entry's own name) are sent as they are: the server's names
//! are exact, and another Unicode form would be another entry.
//!
//! A restore into a taken path ends its job as `asking`, with the name in the way. The window
//! asks for another name and calls the restore again with `path`; that call goes on with the
//! same job, also when the new name is not allowed or taken too (it asks again). A new binding
//! cancels a job that still asks.
//!
//! While a Supernote syncs a write waits by `Retry-After`, up to two minutes in all, in steps of
//! at most a quarter second: a new binding ends the wait (the job is cancelled) within a step.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use super::cache::{name_of, parent_of};
use super::{Clash, Entry, Job, JobKind, JobState, Library, LibraryEvent, Session, lock};
use crate::rest::{Rest, RestError, SYNC_WAIT};

/// The text of a change job while a Supernote syncs; it goes on when the sync ends.
pub const WAITING: &str = "The Supernote is syncing. Notedown waits.";
/// The longest single sleep of a wait: a new binding ends a wait within it.
pub const WAIT_STEP: Duration = Duration::from_millis(250);

fn as_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The parts of a write answer the app uses (`POST /folders`, `POST /move`, `DELETE
/// /entries/{id}`, `POST /recycle/restore`).
#[derive(Deserialize)]
struct Written {
    path: String,
    #[serde(default)]
    seqs: Vec<u64>,
}

#[derive(Serialize)]
struct FolderBody<'a> {
    path: &'a str,
}

#[derive(Serialize)]
struct MoveBody<'a> {
    id: u64,
    to: &'a str,
}

#[derive(Serialize)]
struct RestoreBody<'a> {
    key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
}

/// How a change ended, when it did not end well.
enum Stop {
    /// The library bound again: the change was not made.
    Cancelled,
    /// The server answered an error, or the name was refused before any request.
    Rest(RestError),
    /// The library could not call (not set up, logged out, refused).
    Failed(String),
    /// A restore into a taken path: the window asks for another name.
    Asking(Clash, String),
}

/// A change that worked: the path it made, and whether the bin changed.
struct Ended {
    result: Option<String>,
    recycle: bool,
}

/// A running change job: every change of its state goes to the window.
struct Run<'a> {
    library: &'a Library,
    job: Job,
}

impl Run<'_> {
    fn emit(&self) {
        (self.library.inner.emit)(LibraryEvent::Job {
            job: self.job.clone(),
        });
    }

    /// The job is now `state` with `text`; sent only when that is new.
    fn set(&mut self, state: JobState, text: Option<&str>) {
        if self.job.state != state || self.job.text.as_deref() != text {
            self.job.state = state;
            self.job.text = text.map(str::to_owned);
            self.emit();
        }
    }

    fn label(&mut self, label: String) {
        if self.job.label != label {
            self.job.label = label;
            self.emit();
        }
    }
}

/// `name` as the user typed it, in NFC, when it is one name the server takes: not empty, no
/// `/` or NUL, no leading dot (also `.` and `..`), at most 255 bytes.
fn typed_name(name: &str) -> Result<String, Stop> {
    let name: String = name.nfc().collect();
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\0']) || name.len() > 255 {
        return Err(Stop::Rest(RestError::BadName));
    }
    Ok(name)
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

/// The path a bin item had: its key is `<deletedAt>/<path>`.
fn key_path(key: &str) -> &str {
    key.split_once('/').map_or(key, |(_, path)| path)
}

impl Library {
    /// A new change job, not sent yet.
    fn new_job(&self, label: String) -> Job {
        Job {
            id: self.inner.next_job.fetch_add(1, Ordering::SeqCst) + 1,
            kind: JobKind::Change,
            label,
            done: 0,
            total: None,
            state: JobState::Running,
            text: None,
            question: None,
            result: None,
            drop_id: None,
        }
    }

    /// Runs `work` as the change `job` and sends each step of it; answers the job as it ended.
    fn run_change(&self, job: Job, work: impl FnOnce(&mut Run<'_>) -> Result<Ended, Stop>) -> Job {
        let mut run = Run { library: self, job };
        run.job.state = JobState::Running;
        run.job.text = None;
        run.job.question = None;
        run.emit();
        let ended = work(&mut run);
        let mut job = run.job;
        let mut recycle = false;
        match ended {
            Ok(ended) => {
                job.state = JobState::Done;
                job.text = None;
                job.result = ended.result;
                recycle = ended.recycle;
            }
            Err(Stop::Cancelled) => {
                job.state = JobState::Cancelled;
                job.text = None;
            }
            Err(Stop::Rest(e)) => {
                job.state = JobState::Failed;
                job.text = Some(e.to_string());
            }
            Err(Stop::Failed(text)) => {
                job.state = JobState::Failed;
                job.text = Some(text);
            }
            Err(Stop::Asking(clash, text)) => {
                job.state = JobState::Asking;
                job.text = Some(text);
                job.question = Some(clash);
            }
        }
        (self.inner.emit)(LibraryEvent::Job { job: job.clone() });
        if recycle {
            (self.inner.emit)(LibraryEvent::Recycle);
        }
        job
    }

    /// The write's `seqs` are the app's own, unless the library bound again meanwhile.
    fn wrote_for(&self, session: &Session, seqs: &[u64]) {
        if !session.is_cancelled() {
            self.wrote(seqs);
        }
    }

    fn change_session(&self) -> Result<Session, Stop> {
        let session = self.session().map_err(|e| Stop::Failed(e.to_string()))?;
        if session.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        Ok(session)
    }

    /// The entry `id` as the server has it now (its path and name may have changed since the
    /// window listed it).
    fn read_entry(&self, session: &Session, id: u64) -> Result<Entry, Stop> {
        if session.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        let result = session
            .rest()
            .get_json::<Entry>(&format!("/entries/{id}"), &[]);
        self.settle(session, &result);
        result.map_err(Stop::Rest)
    }

    /// One write. While a Supernote syncs (423 `device_syncing`) the job waits by
    /// `Retry-After` (at least a second) and tries again, up to two minutes in all. The wait is
    /// slept in short steps, so a new binding ends it within [`WAIT_STEP`].
    fn write<T>(
        &self,
        run: &mut Run<'_>,
        session: &Session,
        call: impl Fn(&Rest) -> Result<T, RestError>,
    ) -> Result<T, Stop> {
        let clock = &self.inner.clock;
        let start = clock();
        loop {
            if session.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            run.set(JobState::Running, None);
            let result = call(session.rest());
            let Err(RestError::Syncing { retry_after }) = result else {
                self.settle(session, &result);
                return result.map_err(Stop::Rest);
            };
            let waited = Duration::from_millis(clock().saturating_sub(start));
            let left = SYNC_WAIT.saturating_sub(waited);
            if left.is_zero() {
                return Err(Stop::Rest(RestError::Syncing { retry_after }));
            }
            run.set(JobState::Waiting, Some(WAITING));
            let until =
                clock().saturating_add(as_ms(retry_after.max(Duration::from_secs(1)).min(left)));
            loop {
                if session.is_cancelled() {
                    return Err(Stop::Cancelled);
                }
                let now = clock();
                if now >= until {
                    break;
                }
                (self.inner.sleeper)(Duration::from_millis(until - now).min(WAIT_STEP));
            }
        }
    }

    /// Makes the folder `name` in `parent`. A folder that is there already is a clash.
    /// Blocking: run it off the main thread.
    pub fn new_folder(&self, parent: &str, name: &str) -> Job {
        let job = self.new_job(if name.is_empty() {
            "New folder".to_owned()
        } else {
            format!("New folder {}", name.nfc())
        });
        self.run_change(job, |run| {
            let name = typed_name(name)?;
            let session = self.change_session()?;
            let path = join(parent, &name);
            let made: Written = self.write(run, &session, |rest| {
                rest.post_json_once("/folders", &FolderBody { path: &path })
            })?;
            if made.seqs.is_empty() {
                // The folder that is there, in its own spelling.
                let existing = name_of(&made.path).to_owned();
                return Err(Stop::Rest(RestError::NameTaken { existing }));
            }
            self.wrote_for(&session, &made.seqs);
            Ok(Ended {
                result: Some(made.path),
                recycle: false,
            })
        })
    }

    /// Renames the file or folder `id` to `name` in its folder. Blocking.
    pub fn rename(&self, id: u64, name: &str) -> Job {
        let job = self.new_job(if name.is_empty() {
            "Rename".to_owned()
        } else {
            format!("Rename to {}", name.nfc())
        });
        self.run_change(job, |run| {
            let name = typed_name(name)?;
            let session = self.change_session()?;
            let entry = self.read_entry(&session, id)?;
            run.label(format!("Rename {} to {name}", entry.name));
            let to = join(parent_of(&entry.path), &name);
            self.moved(run, &session, id, &to)
        })
    }

    /// Moves the file or folder `id` into the folder `to_folder`, under its name. Blocking.
    pub fn move_to(&self, id: u64, to_folder: &str) -> Job {
        let folder = match name_of(to_folder) {
            "" => "the top level",
            name => name,
        };
        let job = self.new_job(format!("Move to {folder}"));
        self.run_change(job, |run| {
            let session = self.change_session()?;
            let entry = self.read_entry(&session, id)?;
            run.label(format!("Move {} to {folder}", entry.name));
            let to = join(to_folder, &entry.name);
            self.moved(run, &session, id, &to)
        })
    }

    fn moved(
        &self,
        run: &mut Run<'_>,
        session: &Session,
        id: u64,
        to: &str,
    ) -> Result<Ended, Stop> {
        let moved: Written = self.write(run, session, |rest| {
            rest.post_json_once("/move", &MoveBody { id, to })
        })?;
        self.wrote_for(session, &moved.seqs);
        Ok(Ended {
            result: Some(moved.path),
            recycle: false,
        })
    }

    /// Moves the file or folder `id` to the recycle bin. Blocking.
    pub fn delete(&self, id: u64) -> Job {
        let job = self.new_job("Delete".to_owned());
        self.run_change(job, |run| {
            let session = self.change_session()?;
            let entry = self.read_entry(&session, id)?;
            run.label(format!("Delete {}", entry.name));
            let deleted: Written = self.write(run, &session, |rest| {
                rest.delete_json_once(&format!("/entries/{id}"))
            })?;
            self.wrote_for(&session, &deleted.seqs);
            Ok(Ended {
                result: None,
                recycle: true,
            })
        })
    }

    /// Brings the item `key` of the bin back, at `path` or at the path it had. A taken path
    /// ends the job as `asking` with the name in the way; the call with a `path` goes on with
    /// the same job. Blocking.
    pub fn restore(&self, key: &str, path: Option<&str>) -> Job {
        let old_path = key_path(key);
        let job = match lock(&self.inner.asking).remove(key) {
            Some(job) => job,
            None => self.new_job(format!("Restore {}", name_of(old_path))),
        };
        // The question of a restore that goes on: a name that is not allowed asks it again.
        let asked = job.question.clone();
        let mut session_of_job = None;
        let job = self.run_change(job, |run| {
            let ask_again = |stop: Stop| match (stop, &asked) {
                (Stop::Rest(e @ RestError::BadName), Some(clash)) => {
                    Stop::Asking(clash.clone(), e.to_string())
                }
                (stop, _) => stop,
            };
            // The session first: an answer to a question belongs to the binding that asked.
            let session = self.change_session()?;
            session_of_job = Some(session.clone());
            let target = match path {
                Some(path) => {
                    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
                    join(parent, &typed_name(name).map_err(ask_again)?)
                }
                None => old_path.to_owned(),
            };
            let body = RestoreBody {
                key,
                path: path.map(|_| target.as_str()),
            };
            let restored = self.write(run, &session, |rest| {
                rest.post_json_once::<_, Written>("/recycle/restore", &body)
            });
            match restored {
                Ok(restored) => {
                    self.wrote_for(&session, &restored.seqs);
                    Ok(Ended {
                        result: Some(restored.path),
                        recycle: true,
                    })
                }
                Err(Stop::Rest(RestError::NameTaken { existing })) => {
                    let clash = Clash {
                        name: name_of(&target).to_owned(),
                        existing: existing.clone(),
                        can_replace: false,
                    };
                    let text = RestError::NameTaken { existing }.to_string();
                    Err(Stop::Asking(clash, text))
                }
                Err(e) => Err(ask_again(e)),
            }
        });
        if job.state != JobState::Asking {
            return job;
        }
        let mut asking = lock(&self.inner.asking);
        if session_of_job.is_some_and(|s| !s.is_cancelled()) {
            asking.insert(key.to_owned(), job.clone());
            return job;
        }
        // A binding came meanwhile: it cancelled the jobs that asked before this one.
        drop(asking);
        let mut job = job;
        job.state = JobState::Cancelled;
        job.text = None;
        job.question = None;
        (self.inner.emit)(LibraryEvent::Job { job: job.clone() });
        job
    }

    /// Removes every deleted item from the bin for good. Blocking.
    pub fn empty_bin(&self) -> Job {
        let job = self.new_job("Empty the recycle bin".to_owned());
        self.run_change(job, |run| {
            let session = self.change_session()?;
            self.write(run, &session, |rest| {
                rest.post_empty_once::<IgnoredAny>("/recycle/empty")
            })?;
            Ok(Ended {
                result: None,
                recycle: true,
            })
        })
    }

    /// A new binding: the jobs that ask for a name end.
    pub(super) fn cancel_asking(&self) {
        let jobs: Vec<Job> = lock(&self.inner.asking)
            .drain()
            .map(|(_, job)| job)
            .collect();
        for mut job in jobs {
            job.state = JobState::Cancelled;
            job.text = None;
            job.question = None;
            (self.inner.emit)(LibraryEvent::Job { job });
        }
    }
}
