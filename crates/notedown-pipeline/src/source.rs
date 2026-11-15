//! Where the pipeline reads the server's data from. [`ServerData`] is the only way the worker
//! sees the server; [`FolderSource`] reads a server data folder, which may be mounted
//! read-only and is never written to.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use notedown_cloud::appdata::Digest;
use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::{Event, EventBody, Journal};

use crate::records::{self, Record, Records};
use crate::{Error, Result};

/// An account of the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAccount {
    pub slug: String,
    pub user_id: u64,
}

/// The bytes of a note, or the certain news that it is not there or not a note file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteBytes {
    Found(Vec<u8>),
    Missing,
    /// Something is at the note's path, but it is not a file (a folder, say). It stays so
    /// until someone changes it, so the worker counts it as a failure of the note.
    NotAFile,
}

/// An `account.removed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Removal {
    pub seq: u64,
    pub user_id: u64,
}

/// One page of an account's journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalBatch {
    /// Any new line for this account, also lines that did not parse.
    pub changed: bool,
    /// The `account.removed` events of this batch, in `seq` order.
    pub removed: Vec<Removal>,
    /// One past the last `seq` read; the cursor may move to `next - 1`.
    pub next: u64,
    /// The highest `seq` in the journal as far as the source can tell, 0 if none.
    pub end: u64,
    /// A page limit was hit; ask again before sleeping.
    pub more: bool,
}

/// Every method may block for seconds. Call it from the worker thread or CLI code, never on a
/// tokio runtime thread. `Err` always means "nothing known": skip and retry, never "deleted".
pub trait ServerData: Send + Sync {
    fn describe(&self) -> String;
    fn accounts(&self) -> Result<Vec<ServerAccount>>;
    fn records(&self, slug: &str) -> Result<Records>;
    /// `Ok(None)`: no such record. The caller skips the note in both `Err` and `None`.
    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>>;
    /// `Missing` only when the account's `files/` exists and the note does not; `NotAFile` when
    /// something that is not a file is at its path; a missing account is `Err`. Any `Err` is
    /// "nothing known": the worker skips the note and counts no try.
    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes>;
    /// A cheap check without the bytes: `Some(false)` as for `Missing`, `None` when nothing is
    /// known.
    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>>;
    fn digests(&self, slug: &str) -> Result<Vec<Digest>>;
    /// The account's events with `seq >= from`, at most `limit` lines. Never creates anything.
    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch>;
}

/// The server data in a folder on this machine.
pub struct FolderSource {
    root: PathBuf,
}

impl FolderSource {
    pub fn new(root: &Path) -> FolderSource {
        FolderSource {
            root: root.to_path_buf(),
        }
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl ServerData for FolderSource {
    fn describe(&self) -> String {
        format!("server folder {}", self.root.display())
    }

    /// A server folder without `accounts/` (a share that is not or only half mounted) is no
    /// answer, never "no accounts": a server whose last account was removed still has the
    /// folder (it holds `.next_user_id`).
    fn accounts(&self) -> Result<Vec<ServerAccount>> {
        let dir = self.root.join("accounts");
        match std::fs::metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(Error::Read(format!(
                    "{} is not a folder; is the server folder mounted?",
                    dir.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Read(format!(
                    "{} is missing; is the server folder mounted?",
                    dir.display()
                )));
            }
            Err(e) => return Err(io(&dir)(e)),
        }
        Ok(Accounts::new(&self.root)
            .list()?
            .into_iter()
            .map(|a| ServerAccount {
                slug: a.slug,
                user_id: a.user_id,
            })
            .collect())
    }

    fn records(&self, slug: &str) -> Result<Records> {
        records::read(&self.root, slug)
    }

    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>> {
        Ok(records::read_key(&self.root, slug, key))
    }

    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes> {
        let file = records::note_file(&self.root, slug, path)?;
        if std::fs::metadata(&file).is_ok_and(|m| !m.is_file()) {
            return Ok(NoteBytes::NotAFile);
        }
        match std::fs::read(&file) {
            Ok(b) => Ok(NoteBytes::Found(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if records::account_dir(&self.root, slug)
                    .join("files")
                    .is_dir()
                {
                    Ok(NoteBytes::Missing)
                } else {
                    Err(io(&file)(e))
                }
            }
            Err(e) => Err(io(&file)(e)),
        }
    }

    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>> {
        let file = records::note_file(&self.root, slug, path)?;
        if records::note_missing(&self.root, slug, path) {
            return Ok(Some(false));
        }
        Ok(file.is_file().then_some(true))
    }

    fn digests(&self, slug: &str) -> Result<Vec<Digest>> {
        records::digests(&self.root, slug)
    }

    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
        let journal = Journal::open_no_create(&self.root);
        let mut batch = JournalBatch {
            changed: false,
            removed: Vec::new(),
            next: from,
            end: 0,
            more: false,
        };
        // Only a folder that is certainly not there is "no journal"; any other error is no answer.
        match std::fs::metadata(journal.dir()) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(batch),
            Err(e) => return Err(io(journal.dir())(e)),
        }
        batch.end = last_seq(&journal)?;
        let files = journal.files()?;
        // The newest file whose first `seq` is at or below `from`, else the first.
        let start = files
            .iter()
            .rposition(|(first, _)| *first <= from)
            .unwrap_or(0);
        let mut lines = Lines {
            files: files.into_iter().skip(start).collect(),
            current: None,
            last: 0,
        };
        let limit = limit.max(1);
        let mut read = 0usize;
        while let Some(line) = lines.next_line()? {
            if line.est < from {
                continue;
            }
            // The line after the limit is only looked at, not taken. Only lines with a real
            // `seq` count toward the limit and move `next`: a line without one is not a place
            // to put the cursor behind, since the event that follows may still be unread.
            if line.seq.is_some() && read >= limit {
                batch.more = true;
                break;
            }
            if let Some(seq) = line.seq {
                read += 1;
                batch.next = batch.next.max(seq + 1);
            }
            if line.account.as_deref().is_none_or(|a| a == slug) {
                batch.changed = true;
            }
            if line.account.as_deref() == Some(slug)
                && let Some(Event {
                    body: EventBody::AccountRemoved { user_id },
                    ..
                }) = line.event
            {
                batch.removed.push(Removal {
                    seq: line.est,
                    user_id,
                });
            }
        }
        // Only a `seq` that was read raises the end, never the position asked for: a cursor
        // ahead of the journal must be seen as such.
        if read > 0 {
            batch.end = batch.end.max(batch.next.saturating_sub(1));
        }
        Ok(batch)
    }
}

/// A complete journal line. A line that does not parse as an event has `event: None`. Its
/// `account` is read from the line when it is still JSON with one, else it is `None`, and the
/// line counts for every account (the safe side). Its `seq` is read the same way, else it is
/// taken to follow the last real one, and it never moves a cursor.
struct Line {
    /// The `seq` the line carries, `None` when none can be read.
    seq: Option<u64>,
    /// The `seq`, else the one after the last real one: only for placing the line before `from`.
    est: u64,
    account: Option<String>,
    event: Option<Event>,
}

struct Lines {
    files: VecDeque<(u64, PathBuf)>,
    current: Option<(PathBuf, BufReader<std::fs::File>)>,
    /// The `seq` of the line before.
    last: u64,
}

impl Lines {
    fn next_line(&mut self) -> Result<Option<Line>> {
        loop {
            if self.current.is_none() {
                let Some((first, path)) = self.files.pop_front() else {
                    return Ok(None);
                };
                match std::fs::File::open(&path) {
                    Ok(f) => {
                        self.last = self.last.max(first.saturating_sub(1));
                        self.current = Some((path, BufReader::new(f)));
                    }
                    // Removed by the admin meanwhile.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io(&path)(e)),
                }
            }
            let (path, reader) = self.current.as_mut().expect("set above");
            let mut buf = Vec::new();
            let n = reader.read_until(b'\n', &mut buf).map_err(io(path))?;
            if n == 0 || buf.last() != Some(&b'\n') {
                // The end of the file, or a torn last line that is not an event (yet).
                self.current = None;
                continue;
            }
            buf.pop();
            let value: Option<serde_json::Value> = serde_json::from_slice(&buf).ok();
            let event: Option<Event> = serde_json::from_slice(&buf).ok();
            let seq = event
                .as_ref()
                .map(|e| e.seq)
                .or_else(|| value.as_ref()?.get("seq")?.as_u64());
            let account = event
                .as_ref()
                .map(|e| e.account.clone())
                .or_else(|| value.as_ref()?.get("account")?.as_str().map(str::to_string));
            let est = seq.unwrap_or(self.last + 1);
            if let Some(seq) = seq {
                self.last = seq;
            }
            return Ok(Some(Line {
                seq,
                est,
                account,
                event,
            }));
        }
    }
}

/// The highest `seq` in the journal: that of the last complete line of the newest journal file
/// that has one. Read without the journal lock, so a read-only folder works.
fn last_seq(journal: &Journal) -> Result<u64> {
    let files = journal.files()?;
    for (first, path) in files.iter().rev() {
        if let Some(line) = last_line(path)? {
            let seq = serde_json::from_slice::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("seq")?.as_u64());
            // A last line that has no readable `seq` still tells that the file ends at or after
            // its first `seq`.
            return Ok(seq.unwrap_or(*first));
        }
    }
    Ok(0)
}

/// The last complete (newline-terminated) line of `path`, without the newline.
fn last_line(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut f = std::fs::File::open(path).map_err(io(path))?;
    let len = f.metadata().map_err(io(path))?.len();
    let mut size = 64 * 1024u64;
    loop {
        let start = len.saturating_sub(size);
        let mut buf = vec![0u8; (len - start) as usize];
        f.seek(SeekFrom::Start(start)).map_err(io(path))?;
        f.read_exact(&mut buf).map_err(io(path))?;
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
            if start == 0 {
                return Ok(None);
            }
            size *= 2;
            continue;
        };
        let body = &buf[..end];
        match body.iter().rposition(|&b| b == b'\n') {
            Some(p) => return Ok(Some(body[p + 1..].to_vec())),
            None if start == 0 => return Ok(Some(body.to_vec())),
            None => size *= 2,
        }
    }
}
