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
    let probe = Probe::new(fast);
    let mut changed = false;
    let mut removed = Vec::new();
    let walked = walk(root, from, until, limit.max(1), &probe, |line| {
        if line.account.as_deref().is_none_or(|a| a == slug) {
            changed = true;
        }
        if line.account.as_deref() == Some(slug)
            && let Some(Event {
                body: EventBody::AccountRemoved { user_id },
                ..
            }) = line.event
        {
            removed.push(Removal {
                seq: line.est,
                user_id,
            });
        }
        Flow::Go
    });
    bytes.set(bytes.get() + probe.bytes.get());
    let walked = walked?;
    Ok(JournalBatch {
        changed,
        removed,
        next: walked.next,
        end: walked.end,
        more: walked.more,
    })
}

/// How a visitor of [`walk`] goes on after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flow {
    /// Show the next line.
    Go,
    /// The visitor has taken all it wants: the walk stops at the next line with a real `seq`,
    /// which it only looks at (`more` is then true).
    Full,
}

/// The settings and counters of one [`walk`]: the test seams.
pub(super) struct Probe {
    /// Allows the answer from the tail alone for a position past the end.
    pub(super) fast: bool,
    /// The bytes taken from the journal files.
    pub(super) bytes: Cell<u64>,
    /// The lines parsed as JSON in full (the tail line of [`last_seq`] not counted).
    pub(super) parsed: Cell<u64>,
}

impl Probe {
    pub(super) fn new(fast: bool) -> Self {
        Probe {
            fast,
            bytes: Cell::new(0),
            parsed: Cell::new(0),
        }
    }
}

/// The outcome of a [`walk`], already bounded by its `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Walked {
    /// One past the last real `seq` taken, at least `from`, at most `until + 1`.
    pub(super) next: u64,
    /// The highest `seq` of the journal, raised by what was taken (never to `from`), at most
    /// `until`. 0 for no journal.
    pub(super) end: u64,
    /// The walk stopped at a line with a real `seq` that it did not take: the cap was reached
    /// or the visitor was full. Then `next > from`.
    pub(super) more: bool,
    /// Lines with a real `seq` at or after `from` that were taken (they count toward `cap`).
    pub(super) taken: usize,
    /// The real `seq`s taken do not follow on each other from `max(from, 1)`: a taken `seq`
    /// was above the one expected (the first expected is `max(from, 1)`, then one past the
    /// last taken). A journal file that was removed shows so. A line without a real `seq` is
    /// not looked at here. Only the lines taken count: `false` on a walk with `more` says
    /// nothing about the lines from `next` on (the next walk checks those).
    pub(super) broken: bool,
}

/// Walks the journal from `from` to `until` (both inclusive) and shows every line in between
/// to `visit`, in order, also lines without a real `seq` (placed by their `est`). Never
/// creates anything; a missing `journal/` is an empty walk with `end == 0`.
///
/// Lines below `from` are not parsed: only the `seq` after the fixed `{"seq":` prefix is
/// read, the full parse is the fallback for a line without that prefix. The walk ends at the
/// end of the journal, at the first line past `until`, or at a line with a real `seq` once
/// `cap` such lines were taken or the visitor said [`Flow::Full`] (after at least one was
/// taken, so `more` always means progress); that line is only looked
/// at, not shown, and `more` is set. With `probe.fast`, a `from` past the last line (which
/// has a real `seq`) is answered from the tail of the newest file alone.
pub(super) fn walk(
    root: &Path,
    from: u64,
    until: u64,
    cap: usize,
    probe: &Probe,
    mut visit: impl FnMut(&Line) -> Flow,
) -> Result<Walked> {
    let journal = Journal::open_no_create(root);
    let mut walked = Walked {
        next: from,
        end: 0,
        more: false,
        taken: 0,
        broken: false,
    };
    let bound = |mut w: Walked| {
        w.next = w.next.min(until.saturating_add(1));
        w.end = w.end.min(until);
        w
    };
    // Only a folder that is certainly not there is "no journal"; any other error is no answer.
    match std::fs::metadata(journal.dir()) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(bound(walked)),
        Err(e) => return Err(io_err(journal.dir())(e)),
    }
    let files = journal.files()?;
    let last = last_seq(&files, &probe.bytes)?;
    walked.end = last.end;
    // A position past the last line, which has a real `seq`: every line is before it, so the
    // full read would find nothing either.
    if probe.fast && last.exact && from > last.end {
        return Ok(bound(walked));
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
        from,
        probe,
    };
    let mut expected = from.max(1);
    let mut full = false;
    while let Some(line) = lines.next_line()? {
        if line.est < from {
            continue;
        }
        if line.est > until {
            break;
        }
        // The line after the cap is only looked at, not taken. Only lines with a real `seq`
        // count toward the cap and move `next`: a line without one is not a place to put the
        // cursor behind, since the event that follows may still be unread.
        if line.seq.is_some() && (walked.taken >= cap || (full && walked.taken > 0)) {
            walked.more = true;
            break;
        }
        if let Some(seq) = line.seq {
            walked.taken += 1;
            walked.next = walked.next.max(seq + 1);
            if seq > expected {
                walked.broken = true;
            }
            expected = expected.max(seq + 1);
        }
        if visit(&line) == Flow::Full {
            full = true;
        }
    }
    // Only a `seq` that was taken raises the end, never the position asked for: a cursor
    // ahead of the journal must be seen as such.
    if walked.taken > 0 {
        walked.end = walked.end.max(walked.next.saturating_sub(1));
    }
    Ok(bound(walked))
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
pub(super) struct Line {
    /// The `seq` the line carries, `None` when none can be read.
    pub(super) seq: Option<u64>,
    /// The `seq`, else the one after the last real one. It places the line against `from`,
    /// `until` and an events floor, and it is the `seq` a visitor reports for the line (as
    /// [`Removal::seq`]); for a line without a real `seq` it is only an estimate.
    pub(super) est: u64,
    pub(super) account: Option<String>,
    pub(super) event: Option<Event>,
}

struct Lines<'a> {
    files: VecDeque<(u64, PathBuf)>,
    current: Option<(PathBuf, BufReader<Counted<'a>>)>,
    /// The `seq` of the line before.
    last: u64,
    /// Lines below it are not parsed: they are never shown.
    from: u64,
    probe: &'a Probe,
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
                            bytes: &self.probe.bytes,
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
            // A line below `from` is never shown: its `seq` is all that is needed.
            if let Some(seq) = prefix_seq(&buf)
                && seq < self.from
            {
                self.last = seq;
                continue;
            }
            self.probe.parsed.set(self.probe.parsed.get() + 1);
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

/// The `seq` of a line as the journal writes it: a whole number right after the fixed
/// `{"seq":` prefix, ended by `,` or `}`. `None` when the line does not start so; the full
/// parse decides then.
fn prefix_seq(line: &[u8]) -> Option<u64> {
    let rest = line.strip_prefix(br#"{"seq":"#)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || (digits > 1 && rest[0] == b'0') {
        return None;
    }
    if !matches!(rest.get(digits), Some(b',' | b'}')) {
        return None;
    }
    std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()
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

    fn write_lines(root: &Path, name: &str, lines: &[String]) {
        let dir = root.join("journal");
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(dir.join(name), text).unwrap();
    }

    /// The `seq`s (or `est` for a line without one) that a walk shows, and its outcome.
    fn walk_seqs(root: &Path, from: u64, until: u64, cap: usize) -> (Vec<u64>, Walked, u64) {
        let probe = Probe::new(true);
        let mut seen = Vec::new();
        let w = walk(root, from, until, cap, &probe, |l| {
            seen.push(l.seq.unwrap_or(l.est));
            Flow::Go
        })
        .unwrap();
        (seen, w, probe.parsed.get())
    }

    #[test]
    fn lines_below_from_are_not_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), 1..=50_000);
        let (seen, w, parsed) = walk_seqs(dir.path(), 25_001, u64::MAX, 10);
        assert_eq!(seen, (25_001..=25_010).collect::<Vec<_>>());
        assert_eq!((w.next, w.end, w.more, w.taken), (25_011, 50_000, true, 10));
        assert!(!w.broken);
        // The ten lines taken and the one looked at after them, none of the 25000 before.
        assert_eq!(parsed, 11);
        // The public reader gives what it gave before.
        let b = journal(dir.path(), "a", 25_001, 10).unwrap();
        assert_eq!(b.removed.len(), 10);
        assert_eq!((b.next, b.end, b.more), (25_011, 50_000, true));
    }

    #[test]
    fn a_line_without_the_seq_prefix_is_parsed_in_full() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(
            dir.path(),
            "journal-000000000001.jsonl",
            &[
                line(1, "a"),
                // Another field order: no `{"seq":` prefix, the seq is still read.
                r#"{"account":"a","seq":2}"#.to_string(),
                // No seq at all: placed after the last real one.
                "not json".to_string(),
                // The prefix, but not a whole number: the full parse decides.
                r#"{"seq":4.5,"account":"a"}"#.to_string(),
                line(5, "a"),
                line(6, "a"),
            ],
        );
        let (seen, w, parsed) = walk_seqs(dir.path(), 6, u64::MAX, 100);
        assert_eq!(seen, vec![6]);
        assert_eq!((w.next, w.end, w.more), (7, 6, false));
        // The three lines without a readable prefix seq, and line 6.
        assert_eq!(parsed, 4);
        let (seen, ..) = walk_seqs(dir.path(), 3, u64::MAX, 100);
        assert_eq!(seen, vec![3, 3, 5, 6]);
        let (seen, ..) = walk_seqs(dir.path(), 2, u64::MAX, 100);
        assert_eq!(seen, vec![2, 3, 3, 5, 6]);
    }

    #[test]
    fn a_visitor_that_is_full_stops_the_walk_at_the_next_seq() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), 1..=20);
        let probe = Probe::new(true);
        let mut seen = Vec::new();
        let w = walk(dir.path(), 5, u64::MAX, 100, &probe, |l| {
            seen.push(l.est);
            if seen.len() == 3 {
                Flow::Full
            } else {
                Flow::Go
            }
        })
        .unwrap();
        assert_eq!(seen, vec![5, 6, 7]);
        assert_eq!((w.next, w.end, w.more, w.taken), (8, 20, true, 3));
        // The `until` break comes before the stop: nothing follows inside the bound.
        let w = walk(dir.path(), 5, 7, 100, &probe, |l| {
            if l.est == 7 { Flow::Full } else { Flow::Go }
        })
        .unwrap();
        assert_eq!((w.next, w.end, w.more), (8, 7, false));
    }

    #[test]
    fn the_walk_reports_seqs_that_do_not_follow_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines: Vec<String> = (1..=10).map(|s| line(s, "a")).collect();
        lines.extend((15..=20).map(|s| line(s, "a")));
        write_lines(dir.path(), "journal-000000000001.jsonl", &lines);
        assert!(walk_seqs(dir.path(), 1, u64::MAX, 100).1.broken);
        assert!(walk_seqs(dir.path(), 0, u64::MAX, 100).1.broken);
        assert!(walk_seqs(dir.path(), 12, u64::MAX, 100).1.broken);
        assert!(!walk_seqs(dir.path(), 15, u64::MAX, 100).1.broken);
        assert!(!walk_seqs(dir.path(), 3, 10, 100).1.broken);
        // A page that stops before the jump does not see it; the next page does.
        let (_, w, _) = walk_seqs(dir.path(), 1, u64::MAX, 10);
        assert!(!w.broken && w.more && w.next == 11);
        assert!(walk_seqs(dir.path(), w.next, u64::MAX, 10).1.broken);
    }

    #[test]
    fn a_removed_first_file_is_a_break() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (100..=110).map(|s| line(s, "a")).collect();
        write_lines(dir.path(), "journal-000000000100.jsonl", &lines);
        assert!(walk_seqs(dir.path(), 50, u64::MAX, 100).1.broken);
        assert!(walk_seqs(dir.path(), 0, u64::MAX, 100).1.broken);
        assert!(!walk_seqs(dir.path(), 100, u64::MAX, 100).1.broken);
        assert!(!walk_seqs(dir.path(), 105, u64::MAX, 100).1.broken);
    }

    #[test]
    fn no_journal_is_an_empty_walk() {
        let dir = tempfile::tempdir().unwrap();
        let (seen, w, _) = walk_seqs(dir.path(), 7, 3, 100);
        assert!(seen.is_empty());
        assert_eq!(
            w,
            Walked {
                next: 4,
                end: 0,
                more: false,
                taken: 0,
                broken: false
            }
        );
    }

    #[test]
    fn the_seq_prefix_is_read_only_when_it_is_a_whole_number() {
        assert_eq!(prefix_seq(br#"{"seq":0,"ts":1}"#), Some(0));
        assert_eq!(prefix_seq(br#"{"seq":123,"ts":1}"#), Some(123));
        assert_eq!(prefix_seq(br#"{"seq":7}"#), Some(7));
        assert_eq!(prefix_seq(br#"{"seq":07,"ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq":-1,"ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq":1.5,"ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq":1e3,"ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq": 1,"ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq":"1","ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq":99999999999999999999,"ts":1}"#), None);
        assert_eq!(prefix_seq(br#"{"seq":12"#), None);
        assert_eq!(prefix_seq(br#"{"ts":1,"seq":2}"#), None);
    }
}
