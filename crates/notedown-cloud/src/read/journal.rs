//! The journal of one account as the pipeline reads it: read-only, without the journal lock,
//! and tolerant of lines that do not parse.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{JournalBatch, Removal};
use crate::journal::{Event, EventBody, Journal};
use crate::{Result, io_err};

/// The account's events with `seq >= from`, at most `limit` lines (at least one). Never creates
/// anything. A missing `journal/` is an empty batch with `end == 0`.
///
/// `next` is one past the last real `seq` read (a line without one never moves it); `end` is
/// the highest `seq` of the journal, raised by what was read, never to `from`. A line without a
/// readable `account` counts as a change for every account. A poll past the end reads only the
/// tail of the newest journal file.
pub fn journal(root: &Path, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
    journal_until(root, slug, from, limit, u64::MAX)
}

/// [`journal`] up to `until`: only lines with `seq <= until`, `next <= until + 1` and
/// `end = min(journal end, until)`. For an account that was removed at `seq == until`.
pub fn journal_until(
    root: &Path,
    slug: &str,
    from: u64,
    limit: usize,
    until: u64,
) -> Result<JournalBatch> {
    read(root, slug, from, limit, until, true, &Cell::new(0))
}

/// The work of [`journal_until`]. `fast` allows the answer from the tail alone; `bytes` counts
/// the bytes taken from the journal files.
fn read(
    root: &Path,
    slug: &str,
    from: u64,
    limit: usize,
    until: u64,
    fast: bool,
    bytes: &Cell<u64>,
) -> Result<JournalBatch> {
    let journal = Journal::open_no_create(root);
    let mut batch = JournalBatch {
        changed: false,
        removed: Vec::new(),
        next: from,
        end: 0,
        more: false,
    };
    let bound = |mut b: JournalBatch| {
        b.next = b.next.min(until.saturating_add(1));
        b.end = b.end.min(until);
        b
    };
    // Only a folder that is certainly not there is "no journal"; any other error is no answer.
    match std::fs::metadata(journal.dir()) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(bound(batch)),
        Err(e) => return Err(io_err(journal.dir())(e)),
    }
    let files = journal.files()?;
    let last = last_seq(&files, bytes)?;
    batch.end = last.end;
    // A position past the last line, which has a real `seq`: every line is before it, so the
    // full read would find nothing either.
    if fast && last.exact && from > last.end {
        return Ok(bound(batch));
    }
    // The newest file whose first `seq` is at or below `from`, else the first.
    let start = files
        .iter()
        .rposition(|(first, _)| *first <= from)
        .unwrap_or(0);
    let mut lines = Lines {
        files: files.into_iter().skip(start).collect(),
        current: None,
        last: 0,
        bytes,
    };
    let limit = limit.max(1);
    let mut read = 0usize;
    while let Some(line) = lines.next_line()? {
        if line.est < from {
            continue;
        }
        if line.est > until {
            break;
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
    Ok(bound(batch))
}

/// A journal file that counts the bytes read from it.
struct Counted<'a> {
    file: std::fs::File,
    bytes: &'a Cell<u64>,
}

impl Read for Counted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        self.bytes.set(self.bytes.get() + n as u64);
        Ok(n)
    }
}

impl Seek for Counted<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
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

struct Lines<'a> {
    files: VecDeque<(u64, PathBuf)>,
    current: Option<(PathBuf, BufReader<Counted<'a>>)>,
    /// The `seq` of the line before.
    last: u64,
    bytes: &'a Cell<u64>,
}

impl Lines<'_> {
    fn next_line(&mut self) -> Result<Option<Line>> {
        loop {
            if self.current.is_none() {
                let Some((first, path)) = self.files.pop_front() else {
                    return Ok(None);
                };
                match std::fs::File::open(&path) {
                    Ok(file) => {
                        self.last = self.last.max(first.saturating_sub(1));
                        let file = Counted {
                            file,
                            bytes: self.bytes,
                        };
                        self.current = Some((path, BufReader::new(file)));
                    }
                    // Removed by the admin meanwhile.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_err(&path)(e)),
                }
            }
            let (path, reader) = self.current.as_mut().expect("set above");
            let mut buf = Vec::new();
            let n = reader.read_until(b'\n', &mut buf).map_err(io_err(path))?;
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

/// The end of the journal, see [`last_seq`].
struct Last {
    /// The highest `seq` as far as the tail tells.
    end: u64,
    /// `end` is the real `seq` of the last complete line, and no lower than the first `seq` of
    /// its file: every line of the journal is at or before it.
    exact: bool,
}

/// The highest `seq` in the journal: that of the last complete line of the newest journal file
/// that has one. Read without the journal lock, so a read-only folder works.
fn last_seq(files: &[(u64, PathBuf)], bytes: &Cell<u64>) -> Result<Last> {
    for (first, path) in files.iter().rev() {
        if let Some(line) = last_line(path, bytes)? {
            let seq = serde_json::from_slice::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("seq")?.as_u64());
            // A last line that has no readable `seq` still tells that the file ends at or after
            // its first `seq`.
            return Ok(Last {
                end: seq.unwrap_or(*first),
                exact: seq.is_some_and(|s| s >= *first),
            });
        }
    }
    Ok(Last {
        end: 0,
        exact: false,
    })
}

/// The last complete (newline-terminated) line of `path`, without the newline. Reads the last
/// 64 KiB first, more only for a longer line.
fn last_line(path: &Path, bytes: &Cell<u64>) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path).map_err(io_err(path))?;
    let len = file.metadata().map_err(io_err(path))?.len();
    let mut f = Counted { file, bytes };
    let mut size = 64 * 1024u64;
    loop {
        let start = len.saturating_sub(size);
        let mut buf = vec![0u8; (len - start) as usize];
        f.seek(SeekFrom::Start(start)).map_err(io_err(path))?;
        f.read_exact(&mut buf).map_err(io_err(path))?;
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use super::*;
    use crate::journal::{Event, EventBody, Origin};

    fn line(seq: u64, account: &str) -> String {
        let mut e = Event::new(
            account,
            Origin::Cli,
            None,
            "op",
            EventBody::AccountRemoved { user_id: seq },
        );
        e.seq = seq;
        e.to_line()
    }

    fn write(root: &Path, seqs: std::ops::RangeInclusive<u64>) {
        let dir = root.join("journal");
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        for seq in seqs {
            text.push_str(&line(seq, "a"));
            text.push('\n');
        }
        std::fs::write(dir.join("journal-000000000001.jsonl"), text).unwrap();
    }

    #[test]
    fn journal_until_stops_at_its_seq() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), 1..=10);
        let b = journal_until(dir.path(), "a", 1, 100, 6).unwrap();
        assert!(b.removed.iter().all(|r| r.seq <= 6), "{b:?}");
        assert_eq!(b.removed.len(), 6);
        assert!(b.next <= 7, "{b:?}");
        assert_eq!(b.end, 6);
        assert!(!b.more);
        // A page inside the bound pages on as before.
        let b = journal_until(dir.path(), "a", 1, 2, 6).unwrap();
        assert_eq!((b.next, b.end, b.more), (3, 6, true));
        // Past the bound: nothing, and the cursor never moves past it.
        let b = journal_until(dir.path(), "a", 9, 100, 6).unwrap();
        assert!(b.removed.is_empty() && !b.changed);
        assert!(b.next <= 7 && b.end == 6, "{b:?}");
        // Without a bound it is `journal`.
        assert_eq!(
            journal_until(dir.path(), "a", 1, 100, u64::MAX).unwrap(),
            journal(dir.path(), "a", 1, 100).unwrap()
        );
    }

    #[test]
    fn a_poll_past_the_end_reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), 1..=50_000);
        let fast_bytes = Cell::new(0);
        let fast = read(dir.path(), "a", 50_001, 10_000, u64::MAX, true, &fast_bytes).unwrap();
        let full_bytes = Cell::new(0);
        let full = read(
            dir.path(),
            "a",
            50_001,
            10_000,
            u64::MAX,
            false,
            &full_bytes,
        )
        .unwrap();
        assert_eq!(fast, full);
        assert_eq!(
            fast,
            JournalBatch {
                changed: false,
                removed: Vec::new(),
                next: 50_001,
                end: 50_000,
                more: false,
            }
        );
        assert!(full_bytes.get() > 64 * 1024, "{}", full_bytes.get());
        assert!(fast_bytes.get() <= 64 * 1024, "{}", fast_bytes.get());
        // The public poll takes the same path.
        assert_eq!(journal(dir.path(), "a", 50_001, 10_000).unwrap(), fast);
    }
}
