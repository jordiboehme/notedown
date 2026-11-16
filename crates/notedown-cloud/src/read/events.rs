//! The file and folder events of one account, as the app follows them: a visitor of the
//! journal walker. Read-only, like the rest of [`crate::read`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::journal::{Flow, Line, Probe, walk};
use crate::Result;
use crate::journal::{Event, EventBody, FileKind, Origin};

/// A page stops after this many lines with a real `seq` at or after `from`, of any account.
const LINE_CAP: usize = 10_000;

/// One page of an account's file and folder events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPage {
    pub events: Vec<FileEvent>,
    /// One past the last `seq` read, of any account: the `from` of the next poll.
    pub next: u64,
    /// The highest `seq` of the journal as far as the reader can tell, 0 if none; at most the
    /// `until` of a removed account.
    pub end: u64,
    /// The page stopped at the limit of events or lines: ask again before sleeping. Then
    /// `next > from`.
    pub more: bool,
    /// Events may be missing between `from` and `next`: the app lists again and follows on
    /// from the `seq` of that listing.
    pub gap: bool,
}

/// A file or folder event, or the `account.removed` that ends a removed account's events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEvent {
    pub seq: u64,
    /// The time of the change in ms since the epoch, not the file time.
    pub ts: u64,
    /// `file.created`, `folder.moved`, ..., `account.removed`.
    pub r#type: String,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Shared by all events of one change.
    pub op_id: String,
    /// Not set for `account.removed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// Not set for `account.removed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// For copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<u64>,
    /// For created and modified files: the journal's file kind (not the tree's `kind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_kind: Option<FileKind>,
    /// For created and modified files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// For modified files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_md5: Option<String>,
    /// For created and modified files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// For `account.removed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<u64>,
}

impl FileEvent {
    fn new(e: &Event, r#type: &str) -> FileEvent {
        FileEvent {
            seq: e.seq,
            ts: e.ts,
            r#type: r#type.to_string(),
            origin: e.origin,
            device: e.device.clone(),
            op_id: e.op_id.clone(),
            id: None,
            path: None,
            old_path: None,
            source_id: None,
            file_kind: None,
            md5: None,
            prev_md5: None,
            size: None,
            user_id: None,
        }
    }

    fn at(mut self, id: u64, path: &str) -> FileEvent {
        self.id = Some(id);
        self.path = Some(path.to_string());
        self
    }
}

/// The file and folder events of `slug` with `seq >= from`, at most `limit` (at least one) and
/// at most 10000 lines of any account per page. Never creates anything; a missing `journal/`
/// is an empty page with `end == 0`.
///
/// `floor` is the last `account.removed` of an earlier account with this slug (0 for none):
/// nothing at or below it is shown, and a `from` at or below it is a gap. `until` is the
/// caller's own removal for a removed account: the page ends there, with that one
/// `account.removed`. Lines that are not file or folder events of the account are left out,
/// and so are events with a hidden path (a move to a hidden path is a delete of the old one).
///
/// `gap` is set when events may be missing: a position past the journal end, a journal file
/// that was removed (`seq`s that do not follow on), a line of the account (or of no readable
/// account) that is not an event this version knows, a position at or below `floor`, or a move
/// from a hidden path into sight. A line without a `seq` counts only once a real `seq` follows
/// it, so a damaged last line is not a gap on every poll.
pub fn events(
    root: &Path,
    slug: &str,
    from: u64,
    limit: usize,
    floor: u64,
    until: Option<u64>,
) -> Result<EventPage> {
    page(root, slug, from, limit, floor, until, &Probe::new(true))
}

/// The work of [`events`], with the walker's test seam.
fn page(
    root: &Path,
    slug: &str,
    from: u64,
    limit: usize,
    floor: u64,
    until: Option<u64>,
    probe: &Probe,
) -> Result<EventPage> {
    let limit = limit.max(1);
    let below_floor = floor > 0 && from <= floor;
    // Nothing at or below the floor is shown: start right after it.
    let start = if below_floor { floor + 1 } else { from };
    let mut events = Vec::new();
    let mut gap = below_floor;
    // A line without a `seq` that would be a gap once a real `seq` follows it.
    let mut pending = false;
    let walked = walk(
        root,
        start,
        until.unwrap_or(u64::MAX),
        LINE_CAP,
        probe,
        |line| {
            if line.est <= floor {
                return Flow::Go;
            }
            if line.seq.is_some() && pending {
                gap = true;
            }
            match visit(line, slug, until) {
                Seen::Skip => {}
                Seen::Gap if line.seq.is_some() => gap = true,
                Seen::Gap => pending = true,
                Seen::Event(e) => events.push(*e),
            }
            if events.len() >= limit {
                Flow::Full
            } else {
                Flow::Go
            }
        },
    )?;
    // The walk stopped at a real `seq` it did not show: it follows the pending line.
    if walked.more && pending {
        gap = true;
    }
    // `broken` speaks only for the lines taken; the next page checks on from `next`.
    if walked.broken || from > walked.end.saturating_add(1) {
        gap = true;
    }
    Ok(EventPage {
        events,
        next: walked.next,
        end: walked.end,
        more: walked.more,
        gap,
    })
}

/// What one line is to the app.
enum Seen {
    Skip,
    Gap,
    Event(Box<FileEvent>),
}

/// A line as the app sees it (the floor is checked before).
fn visit(line: &Line, slug: &str, until: Option<u64>) -> Seen {
    let Some(e) = &line.event else {
        // Not an event this version can read: only the account's own lines (or lines of no
        // readable account) can hide something of it.
        return match line.account.as_deref() {
            Some(a) if a != slug => Seen::Skip,
            _ => Seen::Gap,
        };
    };
    if e.account != slug {
        return Seen::Skip;
    }
    let hidden = |path: &str| path.split('/').any(|s| s.starts_with('.'));
    let event = |r#type: &str, id: u64, path: &str| FileEvent::new(e, r#type).at(id, path);
    match &e.body {
        EventBody::FileCreated {
            id,
            path,
            kind,
            md5,
            size,
            ..
        } if !hidden(path) => {
            let mut f = event("file.created", *id, path);
            f.file_kind = Some(*kind);
            f.md5 = Some(md5.clone());
            f.size = Some(*size);
            Seen::Event(Box::new(f))
        }
        EventBody::FileModified {
            id,
            path,
            kind,
            md5,
            prev_md5,
            size,
            ..
        } if !hidden(path) => {
            let mut f = event("file.modified", *id, path);
            f.file_kind = Some(*kind);
            f.md5 = Some(md5.clone());
            f.prev_md5 = Some(prev_md5.clone());
            f.size = Some(*size);
            Seen::Event(Box::new(f))
        }
        EventBody::FileMoved { id, path, old_path } => moved(
            e,
            "file",
            *id,
            path,
            old_path,
            hidden(path),
            hidden(old_path),
        ),
        EventBody::FolderMoved { id, path, old_path } => moved(
            e,
            "folder",
            *id,
            path,
            old_path,
            hidden(path),
            hidden(old_path),
        ),
        EventBody::FileCopied {
            id,
            source_id,
            path,
        } if !hidden(path) => {
            let mut f = event("file.copied", *id, path);
            f.source_id = Some(*source_id);
            Seen::Event(Box::new(f))
        }
        EventBody::FileDeleted { id, path, .. } if !hidden(path) => {
            Seen::Event(Box::new(event("file.deleted", *id, path)))
        }
        EventBody::FolderCreated { id, path } if !hidden(path) => {
            Seen::Event(Box::new(event("folder.created", *id, path)))
        }
        EventBody::FolderDeleted { id, path } if !hidden(path) => {
            Seen::Event(Box::new(event("folder.deleted", *id, path)))
        }
        // Only the caller's own removal, the one that ends its events. Others of the slug are
        // a repeated removal (the crash case) or of an account the floor or `until` keeps out.
        EventBody::AccountRemoved { user_id } if until == Some(e.seq) => {
            let mut f = FileEvent::new(e, "account.removed");
            f.user_id = Some(*user_id);
            Seen::Event(Box::new(f))
        }
        _ => Seen::Skip,
    }
}

/// A move as the app sees it: in sight on both sides a move, out of sight a delete of the old
/// path, into sight a gap (the app has none of what came in).
fn moved(
    e: &Event,
    what: &str,
    id: u64,
    path: &str,
    old_path: &str,
    hidden: bool,
    was_hidden: bool,
) -> Seen {
    match (was_hidden, hidden) {
        (false, false) => {
            let mut f = FileEvent::new(e, &format!("{what}.moved")).at(id, path);
            f.old_path = Some(old_path.to_string());
            Seen::Event(Box::new(f))
        }
        (false, true) => Seen::Event(Box::new(
            FileEvent::new(e, &format!("{what}.deleted")).at(id, old_path),
        )),
        (true, false) => Seen::Gap,
        (true, true) => Seen::Skip,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::journal::{DigestGroupInfo, Event, EventBody, FileKind, Origin, TaskGroupInfo};

    fn ev(seq: u64, account: &str, body: EventBody) -> String {
        let mut e = Event::new(account, Origin::Device, None, "op-1", body);
        e.seq = seq;
        e.ts = 1000 + seq;
        e.to_line()
    }

    fn created(seq: u64, account: &str, path: &str) -> String {
        ev(
            seq,
            account,
            EventBody::FileCreated {
                id: seq,
                path: path.to_string(),
                kind: FileKind::of(path),
                md5: "m".into(),
                size: 1,
                pages_changed: None,
            },
        )
    }

    fn moved(seq: u64, path: &str, old_path: &str, folder: bool) -> String {
        let (id, path, old_path) = (seq, path.to_string(), old_path.to_string());
        let body = if folder {
            EventBody::FolderMoved { id, path, old_path }
        } else {
            EventBody::FileMoved { id, path, old_path }
        };
        ev(seq, "a", body)
    }

    fn removed(seq: u64, account: &str, user_id: u64) -> String {
        ev(seq, account, EventBody::AccountRemoved { user_id })
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

    fn write(root: &Path, lines: &[String]) {
        write_lines(root, "journal-000000000001.jsonl", lines);
    }

    fn seqs(page: &EventPage) -> Vec<u64> {
        page.events.iter().map(|e| e.seq).collect()
    }

    fn all(root: &Path, from: u64) -> EventPage {
        events(root, "a", from, 1000, 0, None).unwrap()
    }

    #[test]
    fn only_file_and_folder_events_of_the_account() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                created(1, "a", "/Note/x.note"),
                created(2, "b", "/Note/y.note"),
                ev(
                    3,
                    "a",
                    EventBody::PageRemoved {
                        id: 1,
                        file_id: "F".into(),
                        page_id: "P".into(),
                    },
                ),
                ev(
                    4,
                    "a",
                    EventBody::DigestGroupCreated(DigestGroupInfo {
                        id: 1,
                        name: "g".into(),
                    }),
                ),
                ev(
                    5,
                    "a",
                    EventBody::TaskGroupCreated(TaskGroupInfo {
                        task_list_id: "t".into(),
                        title: "t".into(),
                    }),
                ),
                ev(6, "a", EventBody::DeviceBound),
                ev(
                    7,
                    "a",
                    EventBody::FolderCreated {
                        id: 7,
                        path: "/Note/f".into(),
                    },
                ),
                // Hidden paths are not shown.
                created(8, "a", "/Note/.hidden.note"),
                created(9, "a", "/.trash/z.note"),
                ev(
                    10,
                    "a",
                    EventBody::FolderCreated {
                        id: 10,
                        path: "/Note/.git".into(),
                    },
                ),
                moved(11, "/Note/.a", "/Note/.b", false),
                // The removal of another account and of this one (the crash case) are no
                // events of a live account.
                removed(12, "b", 5),
                removed(13, "a", 1),
                created(14, "a", "/Note/w.note"),
            ],
        );
        let page = all(dir.path(), 0);
        assert_eq!(seqs(&page), vec![1, 7, 14]);
        assert!(!page.gap && !page.more, "{page:?}");
        assert_eq!((page.next, page.end), (15, 14));
    }

    #[test]
    fn the_event_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut with_device = Event::new(
            "a",
            Origin::App,
            Some("SN1".into()),
            "op-7",
            EventBody::FileModified {
                id: 1,
                path: "/Document/x.pdf".into(),
                kind: FileKind::Document,
                md5: "new".into(),
                prev_md5: "old".into(),
                size: 9,
                pages_changed: Some(2),
            },
        );
        with_device.seq = 2;
        with_device.ts = 77;
        write(
            dir.path(),
            &[
                created(1, "a", "/Note/x.note"),
                with_device.to_line(),
                moved(3, "/Note/y.note", "/Note/x.note", false),
                ev(4, "a", EventBody::FileCopied {
                    id: 4,
                    source_id: 1,
                    path: "/Note/z.note".into(),
                }),
                ev(5, "a", EventBody::FileDeleted {
                    id: 4,
                    path: "/Note/z.note".into(),
                    recycle: "123/Note/z.note".into(),
                }),
                ev(6, "a", EventBody::FolderCreated {
                    id: 6,
                    path: "/Note/f".into(),
                }),
                moved(7, "/Note/g", "/Note/f", true),
                ev(8, "a", EventBody::FolderDeleted {
                    id: 6,
                    path: "/Note/g".into(),
                }),
                // A line of an origin this version does not know.
                r#"{"seq":9,"ts":1,"account":"a","origin":"robot","type":"folder.created","id":9,"path":"/R","opId":"o"}"#.to_string(),
                removed(10, "a", 3),
            ],
        );
        let page = events(dir.path(), "a", 1, 1000, 0, Some(10)).unwrap();
        let json: Vec<serde_json::Value> = page
            .events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let expected = serde_json::json!([
            {"seq":1,"ts":1001,"type":"file.created","origin":"device","opId":"op-1","id":1,
             "path":"/Note/x.note","fileKind":"note","md5":"m","size":1},
            {"seq":2,"ts":77,"type":"file.modified","origin":"app","device":"SN1","opId":"op-7",
             "id":1,"path":"/Document/x.pdf","fileKind":"document","md5":"new","prevMd5":"old",
             "size":9},
            {"seq":3,"ts":1003,"type":"file.moved","origin":"device","opId":"op-1","id":3,
             "path":"/Note/y.note","oldPath":"/Note/x.note"},
            {"seq":4,"ts":1004,"type":"file.copied","origin":"device","opId":"op-1","id":4,
             "path":"/Note/z.note","sourceId":1},
            {"seq":5,"ts":1005,"type":"file.deleted","origin":"device","opId":"op-1","id":4,
             "path":"/Note/z.note"},
            {"seq":6,"ts":1006,"type":"folder.created","origin":"device","opId":"op-1","id":6,
             "path":"/Note/f"},
            {"seq":7,"ts":1007,"type":"folder.moved","origin":"device","opId":"op-1","id":7,
             "path":"/Note/g","oldPath":"/Note/f"},
            {"seq":8,"ts":1008,"type":"folder.deleted","origin":"device","opId":"op-1","id":6,
             "path":"/Note/g"},
            {"seq":9,"ts":1,"type":"folder.created","origin":"unknown","opId":"o","id":9,
             "path":"/R"},
            {"seq":10,"ts":1010,"type":"account.removed","origin":"device","opId":"op-1",
             "userId":3},
        ]);
        assert_eq!(serde_json::Value::Array(json), expected);
        assert!(!page.gap);
        // The page as a whole, camelCase.
        let whole = serde_json::to_value(&page).unwrap();
        for key in ["events", "next", "end", "more", "gap"] {
            assert!(whole.get(key).is_some(), "{key}: {whole}");
        }
        // And back.
        let back: EventPage = serde_json::from_value(whole).unwrap();
        assert_eq!(back, page);
    }

    #[test]
    fn a_quiet_poll_is_no_gap_and_a_position_past_the_end_is() {
        let dir = tempfile::tempdir().unwrap();
        // No journal yet.
        let page = all(dir.path(), 0);
        assert_eq!(
            (page.next, page.end, page.more, page.gap),
            (0, 0, false, false)
        );
        assert!(!all(dir.path(), 1).gap);
        assert!(all(dir.path(), 2).gap);
        write(
            dir.path(),
            &(1..=5).map(|s| created(s, "a", "/x")).collect::<Vec<_>>(),
        );
        // From the start: no gap without a floor.
        assert!(!all(dir.path(), 0).gap);
        let page = all(dir.path(), 6);
        assert!(page.events.is_empty());
        assert_eq!(
            (page.next, page.end, page.more, page.gap),
            (6, 5, false, false)
        );
        let page = all(dir.path(), 7);
        assert!(page.events.is_empty());
        assert_eq!((page.end, page.gap), (5, true));
        assert!(all(dir.path(), 500).gap);
    }

    #[test]
    fn events_come_in_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        for s in 1..=30 {
            let account = if s % 3 == 0 { "a" } else { "b" };
            lines.push(created(s, account, "/x"));
        }
        write(dir.path(), &lines);
        let page = events(dir.path(), "a", 0, 4, 0, None).unwrap();
        assert_eq!(seqs(&page), vec![3, 6, 9, 12]);
        // The page stops at the next line with a seq: the foreign lines after 12 are not read.
        assert_eq!((page.next, page.more, page.gap), (13, true, false));
        let page = events(dir.path(), "a", page.next, 4, 0, None).unwrap();
        assert_eq!(seqs(&page), vec![15, 18, 21, 24]);
        assert_eq!((page.next, page.more), (25, true));
        let page = events(dir.path(), "a", page.next, 4, 0, None).unwrap();
        assert_eq!(seqs(&page), vec![27, 30]);
        assert_eq!(
            (page.next, page.end, page.more, page.gap),
            (31, 30, false, false)
        );
        // A limit of 0 is 1.
        let page = events(dir.path(), "a", 0, 0, 0, None).unwrap();
        assert_eq!(seqs(&page), vec![3]);
        assert!(page.more && page.next == 4);
    }

    #[test]
    fn a_page_always_makes_progress() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines: Vec<String> = (1..=10_500).map(|s| created(s, "b", "/x")).collect();
        lines.push(created(10_501, "a", "/x"));
        write(dir.path(), &lines);
        let page = all(dir.path(), 1);
        assert!(page.events.is_empty());
        assert!(page.more && !page.gap, "{:?}", (page.next, page.end));
        assert_eq!(page.next, 10_001);
        let page = all(dir.path(), page.next);
        assert_eq!(seqs(&page), vec![10_501]);
        assert_eq!((page.next, page.more, page.gap), (10_502, false, false));
    }

    #[test]
    fn a_removed_journal_file_is_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (100..=110).map(|s| created(s, "a", "/x")).collect();
        write_lines(dir.path(), "journal-000000000100.jsonl", &lines);
        // The file with 1 to 99 is gone.
        assert!(all(dir.path(), 0).gap);
        assert!(all(dir.path(), 50).gap);
        assert!(!all(dir.path(), 100).gap);
        assert!(!all(dir.path(), 105).gap);
        // Two seqs that do not follow on, here in the middle of a file and of another account.
        let mut lines: Vec<String> = (1..=10).map(|s| created(s, "a", "/x")).collect();
        lines.extend((15..=20).map(|s| created(s, "b", "/x")));
        let other = tempfile::tempdir().unwrap();
        write(other.path(), &lines);
        let page = all(other.path(), 1);
        assert_eq!(seqs(&page), (1..=10).collect::<Vec<_>>());
        assert!(page.gap);
        assert!(!all(other.path(), 15).gap);
    }

    #[test]
    fn a_line_that_cannot_be_read_is_a_gap() {
        let cases: [(&str, bool); 6] = [
            // A line of the account that is not an event.
            (r#"{"seq":2,"account":"a","type":"file.created"}"#, true),
            // A type this version does not know, of the account.
            (
                r#"{"seq":2,"ts":1,"account":"a","origin":"cli","type":"file.renamed","opId":"o"}"#,
                true,
            ),
            // A line without a readable account.
            (r#"{"seq":2,"what":1}"#, true),
            // A line of another account that does not parse: not the app's business.
            (r#"{"seq":2,"account":"b","type":"file.created"}"#, false),
            // A known type that is no file event.
            (&ev(2, "a", EventBody::DeviceUnbound), false),
            // A damaged line followed by a real seq.
            ("not json", true),
        ];
        for (bad, gap) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(
                dir.path(),
                &[
                    created(1, "a", "/x"),
                    bad.to_string(),
                    created(3, "a", "/y"),
                ],
            );
            let page = all(dir.path(), 1);
            assert_eq!(page.gap, gap, "{bad}");
            assert_eq!(seqs(&page), vec![1, 3], "{bad}");
        }
        // A seq-less last line is a gap only once a real seq follows it: the page ends at it,
        // and every poll would see it again.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &[created(1, "a", "/x"), "not json".to_string()]);
        let page = all(dir.path(), 1);
        assert_eq!(seqs(&page), vec![1]);
        assert_eq!((page.next, page.gap), (2, false));
        let again = all(dir.path(), page.next);
        assert!(again.events.is_empty() && !again.gap, "{again:?}");
        write(
            dir.path(),
            &[
                created(1, "a", "/x"),
                "not json".to_string(),
                created(2, "a", "/y"),
            ],
        );
        let page = all(dir.path(), 2);
        assert_eq!(seqs(&page), vec![2]);
        assert!(page.gap);
        // Also when the page stops right at that seq.
        write(
            dir.path(),
            &[
                created(1, "a", "/x"),
                "not json".to_string(),
                created(2, "b", "/y"),
            ],
        );
        let page = events(dir.path(), "a", 1, 1, 0, None).unwrap();
        assert_eq!(seqs(&page), vec![1]);
        assert!(page.more && page.gap, "{page:?}");
        // A damaged line of another account is no gap, also at the end.
        write(
            dir.path(),
            &[
                created(1, "a", "/x"),
                r#"{"account":"b","what":1}"#.to_string(),
                created(2, "b", "/y"),
            ],
        );
        assert!(!all(dir.path(), 1).gap);
    }

    #[test]
    fn the_floor_hides_an_earlier_account() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                // The earlier account "a" (user 1).
                created(1, "a", "/old.note"),
                "not json".to_string(),
                created(2, "a", "/older.note"),
                removed(3, "a", 1),
                // The account "a" of today (user 2).
                created(4, "a", "/new.note"),
                created(5, "b", "/x"),
            ],
        );
        // Without a floor the damaged line of the earlier account is in sight.
        assert!(events(dir.path(), "a", 0, 1000, 0, None).unwrap().gap);
        // With a floor, nothing at or below it is shown, and a position at or below it is a gap.
        for from in [0, 1, 3] {
            let page = events(dir.path(), "a", from, 1000, 3, None).unwrap();
            assert_eq!(seqs(&page), vec![4], "{from}");
            assert!(page.gap, "{from}");
            assert_eq!((page.next, page.end, page.more), (6, 5, false));
        }
        let page = events(dir.path(), "a", 4, 1000, 3, None).unwrap();
        assert_eq!(seqs(&page), vec![4]);
        assert!(!page.gap);
        // A removed token of the account of today (removed at 7) sees nothing of the earlier.
        let mut lines = vec![
            created(1, "a", "/old.note"),
            removed(3, "a", 1),
            created(4, "a", "/new.note"),
            removed(7, "a", 2),
        ];
        lines.insert(1, created(2, "a", "/older.note"));
        lines.insert(4, created(5, "b", "/x"));
        lines.insert(5, created(6, "a", "/n2.note"));
        write(dir.path(), &lines);
        let page = events(dir.path(), "a", 0, 1000, 3, Some(7)).unwrap();
        assert_eq!(seqs(&page), vec![4, 6, 7]);
    }

    #[test]
    fn the_ceiling_ends_with_one_removal() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                created(1, "a", "/x"),
                // A removal of the same user that crashed and was repeated.
                removed(2, "a", 2),
                created(3, "a", "/y"),
                removed(4, "b", 9),
                removed(5, "a", 2),
                // A later account with the name.
                created(6, "a", "/z"),
                removed(7, "a", 3),
            ],
        );
        let page = events(dir.path(), "a", 0, 1000, 0, Some(5)).unwrap();
        assert_eq!(seqs(&page), vec![1, 3, 5]);
        let last = page.events.last().unwrap();
        assert_eq!(
            (last.r#type.as_str(), last.user_id),
            ("account.removed", Some(2))
        );
        assert_eq!((last.id, last.path.as_deref()), (None, None));
        assert_eq!(
            (page.next, page.end, page.more, page.gap),
            (6, 5, false, false)
        );
        // Paged, the removal is still the last.
        let page = events(dir.path(), "a", 0, 2, 0, Some(5)).unwrap();
        assert_eq!(seqs(&page), vec![1, 3]);
        let page = events(dir.path(), "a", page.next, 2, 0, Some(5)).unwrap();
        assert_eq!(seqs(&page), vec![5]);
        assert_eq!((page.next, page.more), (6, false));
        // A poll after it: nothing, no gap, and the cursor stays.
        let page = events(dir.path(), "a", 6, 1000, 0, Some(5)).unwrap();
        assert!(page.events.is_empty());
        assert_eq!(
            (page.next, page.end, page.more, page.gap),
            (6, 5, false, false)
        );
    }

    #[test]
    fn a_move_out_of_sight_is_a_delete_and_into_sight_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                moved(1, "/Note/.x.note", "/Note/x.note", false),
                moved(2, "/.old/f", "/Note/f", true),
                moved(3, "/Note/.a/y.note", "/Note/.b/y.note", false),
            ],
        );
        let page = all(dir.path(), 1);
        let json: Vec<serde_json::Value> = page
            .events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert_eq!(
            serde_json::Value::Array(json),
            serde_json::json!([
                {"seq":1,"ts":1001,"type":"file.deleted","origin":"device","opId":"op-1","id":1,
                 "path":"/Note/x.note"},
                {"seq":2,"ts":1002,"type":"folder.deleted","origin":"device","opId":"op-1","id":2,
                 "path":"/Note/f"},
            ])
        );
        assert!(!page.gap);
        // Into sight: the app lists again.
        write(
            dir.path(),
            &[
                created(1, "a", "/x"),
                moved(2, "/Note/f", "/.old/f", true),
                created(3, "a", "/y"),
            ],
        );
        let page = all(dir.path(), 1);
        assert_eq!(seqs(&page), vec![1, 3]);
        assert!(page.gap);
    }

    #[test]
    fn a_poll_past_the_end_reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=50_000).map(|s| created(s, "a", "/x")).collect();
        write(dir.path(), &lines);
        let fast = Probe::new(true);
        let a = page(dir.path(), "a", 50_001, 1000, 0, None, &fast).unwrap();
        let full = Probe::new(false);
        let b = page(dir.path(), "a", 50_001, 1000, 0, None, &full).unwrap();
        assert_eq!(a, b);
        assert!(a.events.is_empty());
        assert_eq!(
            (a.next, a.end, a.more, a.gap),
            (50_001, 50_000, false, false)
        );
        assert!(full.bytes.get() > 64 * 1024, "{}", full.bytes.get());
        assert!(fast.bytes.get() <= 64 * 1024, "{}", fast.bytes.get());
        // Past the end the fast path also tells the gap.
        let c = page(dir.path(), "a", 50_002, 1000, 0, None, &Probe::new(true)).unwrap();
        assert!(c.gap);
        // The public poll takes the fast path.
        assert_eq!(all(dir.path(), 50_001), a);
    }
}
