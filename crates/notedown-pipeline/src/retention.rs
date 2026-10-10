//! Retention: pruning old page versions from the text log (spec 2.4, 5.3).
//!
//! A line goes when it is older than `keep_days`, every consumer's bookmark is past it or
//! older than `keep_days`, and it is not protected:
//! - the newest line of the log stays whatever it is (the next `seq` is read from the log
//!   tail, so removing it would repeat numbers and make bookmarks skip lines);
//! - the current text of every page stays: the newest text version of a page of a note, and
//!   the newest line of the page of any kind. Page lines under the old key of a re-keyed note
//!   (`note.moved` with `oldNote`) belong to the moved note, so they stay while they are the
//!   current text of a page;
//! - the newest note-level line of a note stays, so its path and flags can be rebuilt;
//! - a deleted note keeps the current text of its pages until its `note.deleted` line itself
//!   may go, then both go.
//!
//! The rewrite is atomic under `.lock`, `seq` numbers are never changed, and the prune runs at
//! most once a day (stamp file `.pruned`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::Result;
use crate::model::{LineChange, LogLine};
use crate::store::{TextStore, WriterLock};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const STAMP_FILE: &str = ".pruned";

/// What a [`prune`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Lines in the log afterwards (0 when the prune was skipped).
    pub kept: usize,
    /// Lines removed.
    pub removed: usize,
    /// True when the daily stamp said a prune already ran in the last day; nothing was read.
    pub skipped: bool,
}

/// Removes the old page versions nobody needs from the log of `store`. See the module doc.
/// `now_ms` is the time, ms since the epoch.
pub fn prune(
    store: &TextStore,
    w: &WriterLock,
    keep_days: u32,
    now_ms: u64,
) -> Result<PruneReport> {
    let stamp = store.dir().join(STAMP_FILE);
    if let Some(last) = read_stamp(&stamp)
        && last <= now_ms
        && now_ms - last < DAY_MS
    {
        return Ok(PruneReport {
            skipped: true,
            ..PruneReport::default()
        });
    }
    let keep_ms = u64::from(keep_days) * DAY_MS;
    let consumers = store.consumers()?;
    let (kept, removed) = store.rewrite_log(w, &mut |lines| {
        Ok(removable(lines, &consumers, keep_ms, now_ms))
    })?;
    notedown_cloud::fsutil::write_atomic(&stamp, format!("{now_ms}\n").as_bytes(), false)?;
    Ok(PruneReport {
        kept,
        removed,
        skipped: false,
    })
}

fn read_stamp(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The `seq` numbers of the lines to remove.
fn removable(
    lines: &[LogLine],
    consumers: &[(String, crate::store::Bookmark)],
    keep_ms: u64,
    now_ms: u64,
) -> BTreeSet<u64> {
    let old = |ts: u64| now_ms.saturating_sub(ts) > keep_ms;
    // A line may go on age and bookmarks alone.
    let aged: BTreeSet<u64> = lines
        .iter()
        .filter(|l| {
            old(l.ts)
                && consumers
                    .iter()
                    .all(|(_, b)| b.seq >= l.seq || old(b.updated_at))
        })
        .map(|l| l.seq)
        .collect();

    // A re-keyed note is one note: old key -> new key.
    let mut rekey: BTreeMap<&str, &str> = BTreeMap::new();
    for l in lines {
        if let Some(old) = &l.old_note {
            rekey.insert(old, &l.note);
        }
    }
    let group = |key: &str| -> String {
        let mut key = key;
        for _ in 0..=rekey.len() {
            match rekey.get(key) {
                Some(next) => key = next,
                None => break,
            }
        }
        key.to_string()
    };

    #[derive(Default)]
    struct Note {
        deleted_seq: Option<u64>,
        last_note_line: Option<u64>,
        /// The page lines to keep as current, per page: newest line, newest text line.
        pages: BTreeMap<String, (u64, Option<u64>)>,
    }
    let mut notes: BTreeMap<String, Note> = BTreeMap::new();
    for l in lines {
        let note = notes.entry(group(&l.note)).or_default();
        if l.change.is_note_change() {
            note.last_note_line = Some(l.seq);
            if l.change == LineChange::NoteDeleted {
                note.deleted_seq = Some(l.seq);
            }
            continue;
        }
        let Some(page) = &l.page_id else { continue };
        let entry = note.pages.entry(page.clone()).or_insert((l.seq, None));
        entry.0 = l.seq;
        match l.change {
            LineChange::New | LineChange::Edited => {
                entry.1 = Some(l.seq);
                // Text again after a deletion: the note lives.
                if note.last_note_line == note.deleted_seq {
                    note.last_note_line = None;
                }
                note.deleted_seq = None;
            }
            LineChange::Deleted => entry.1 = None,
            _ => {}
        }
    }

    let mut protected: BTreeSet<u64> = BTreeSet::new();
    if let Some(last) = lines.last() {
        protected.insert(last.seq);
    }
    for note in notes.values() {
        let gone = note.deleted_seq.is_some_and(|d| aged.contains(&d));
        if gone {
            continue;
        }
        if note.deleted_seq.is_none()
            && let Some(seq) = note.last_note_line
        {
            protected.insert(seq);
        }
        if let Some(seq) = note.deleted_seq {
            // A deleted note: the line itself stays while it may not go.
            protected.insert(seq);
        }
        for (newest, text) in note.pages.values() {
            protected.insert(*newest);
            protected.extend(*text);
        }
    }
    aged.difference(&protected).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Block;

    const KEY: &str = "F20240424175151947577oMie9HNQYnTi";
    const NEW_KEY: &str = "F20240424175151947577oMie9HNQYnTj";
    const KEEP: u32 = 90;
    const NOW: u64 = 400 * DAY_MS;
    const OLD: u64 = 100 * DAY_MS;
    const YOUNG: u64 = NOW - 10 * DAY_MS;

    struct Fixture {
        _tmp: tempfile::TempDir,
        w: WriterLock,
        store: TextStore,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let w = WriterLock::take(tmp.path()).unwrap();
        let store = TextStore::open_writer(tmp.path(), "jordi", &w).unwrap();
        Fixture {
            _tmp: tmp,
            w,
            store,
        }
    }

    fn page(note: &str, page: &str, change: LineChange, ts: u64) -> LogLine {
        let mut line = LogLine::new(note, change);
        line.page_id = Some(page.into());
        line.ts = ts;
        if matches!(change, LineChange::New | LineChange::Edited) {
            line.blocks = vec![Block {
                id: "b1".into(),
                label: "text".into(),
                bbox: [0, 0, 1, 1],
                text: format!("text at {ts}"),
                replaces: Vec::new(),
            }];
        }
        line
    }

    fn note_line(note: &str, change: LineChange, ts: u64) -> LogLine {
        let mut line = LogLine::new(note, change);
        line.ts = ts;
        line
    }

    impl Fixture {
        fn append(&self, mut lines: Vec<LogLine>) {
            self.store.append(&self.w, &mut lines).unwrap();
        }

        fn seqs(&self) -> Vec<u64> {
            let mut out = Vec::new();
            self.store
                .read_log(0, None, &mut |l| {
                    out.push(l.seq);
                    Ok(())
                })
                .unwrap();
            out
        }

        fn prune(&self, now: u64) -> PruneReport {
            prune(&self.store, &self.w, KEEP, now).unwrap()
        }

        fn bookmark(&self, name: &str, seq: u64, updated_at: u64) {
            let dir = self.store.dir().join("consumers");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{name}.json")),
                format!(r#"{{"seq":{seq},"updatedAt":{updated_at}}}"#),
            )
            .unwrap();
        }
    }

    #[test]
    fn old_superseded_lines_go() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),    // 1: old, superseded
            page(KEY, "P1", LineChange::Edited, OLD), // 2: old, superseded
            page(KEY, "P1", LineChange::Edited, YOUNG), // 3: current
            page(KEY, "P2", LineChange::New, OLD),    // 4: old, superseded
            page(KEY, "P2", LineChange::Edited, YOUNG), // 5: current
            page(KEY, "P3", LineChange::New, YOUNG),  // 6: young
            page(KEY, "P3", LineChange::Edited, YOUNG), // 7: young, superseded
        ]);
        let report = f.prune(NOW);
        assert_eq!(f.seqs(), vec![3, 5, 6, 7], "young superseded lines stay");
        assert_eq!(
            report,
            PruneReport {
                kept: 4,
                removed: 3,
                skipped: false
            }
        );
    }

    #[test]
    fn the_current_text_stays() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD), // 1: current text of P1
            page(KEY, "P1", LineChange::Moved, OLD), // 2: newest line of P1, no text
            page(KEY, "P2", LineChange::New, OLD), // 3: current text of P2
            page(KEY, "P3", LineChange::New, OLD), // 4: P3 was deleted
            page(KEY, "P3", LineChange::Deleted, OLD), // 5
            page(KEY, "P4", LineChange::New, YOUNG), // 6
        ]);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![1, 2, 3, 5, 6]);
    }

    #[test]
    fn the_newest_line_is_never_removed() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            page(KEY, "P1", LineChange::Deleted, OLD),
        ]);
        let report = f.prune(NOW);
        assert_eq!(f.seqs(), vec![2]);
        assert_eq!(report.removed, 1);
        let mut next = vec![page(KEY, "P9", LineChange::New, NOW)];
        f.store.append(&f.w, &mut next).unwrap();
        assert_eq!(next[0].seq, 3, "above every seq ever written");
        assert_eq!(f.seqs(), vec![2, 3]);
    }

    #[test]
    fn the_newest_line_survives_when_everything_else_goes() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            note_line(KEY, LineChange::NoteDeleted, OLD),
        ]);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![2]);
        let mut next = vec![page(NEW_KEY, "P1", LineChange::New, NOW)];
        f.store.append(&f.w, &mut next).unwrap();
        assert_eq!(next[0].seq, 3);
    }

    #[test]
    fn a_bookmark_holds_lines() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),    // 1
            page(KEY, "P1", LineChange::Edited, OLD), // 2
            page(KEY, "P1", LineChange::Edited, OLD), // 3
            page(KEY, "P1", LineChange::Edited, OLD), // 4 current
        ]);
        f.bookmark("agent", 1, YOUNG);
        f.bookmark("other", 3, YOUNG);
        f.prune(NOW);
        // "agent" has seen only line 1, so lines 2 and 3 are still to be read.
        assert_eq!(f.seqs(), vec![2, 3, 4]);
    }

    #[test]
    fn an_old_bookmark_does_not_hold_forever() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
        ]);
        f.bookmark("gone", 0, OLD);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![3]);
    }

    #[test]
    fn seq_numbers_stay() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            page(KEY, "P2", LineChange::New, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
            page(KEY, "P2", LineChange::Edited, OLD),
        ]);
        let before = std::fs::read_to_string(f.store.dir().join("log.jsonl")).unwrap();
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![3, 4]);
        let after = std::fs::read_to_string(f.store.dir().join("log.jsonl")).unwrap();
        let kept: Vec<&str> = before.lines().skip(2).collect();
        assert_eq!(
            after.lines().collect::<Vec<_>>(),
            kept,
            "kept bytes unchanged"
        );
        assert_eq!(f.store.last_seq().unwrap(), 4);
        let mut next = vec![page(KEY, "P1", LineChange::Edited, NOW)];
        f.store.append(&f.w, &mut next).unwrap();
        assert_eq!(next[0].seq, 5);
    }

    #[test]
    fn a_young_note_deleted_line_and_its_pages_stay() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
            note_line(KEY, LineChange::NoteDeleted, YOUNG),
            page(NEW_KEY, "Q1", LineChange::New, YOUNG),
        ]);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![2, 3, 4]);
    }

    #[test]
    fn a_deleted_note_goes_when_its_deleted_line_expires() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
            note_line(KEY, LineChange::NoteDeleted, OLD),
            page(NEW_KEY, "Q1", LineChange::New, YOUNG),
        ]);
        // A reader that has not seen the deletion holds it, and with it the pages.
        f.bookmark("agent", 2, YOUNG);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![2, 3, 4]);
        f.bookmark("agent", 4, YOUNG);
        f.prune(NOW + 2 * DAY_MS);
        assert_eq!(f.seqs(), vec![4]);
    }

    #[test]
    fn a_note_that_came_back_is_not_deleted() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            note_line(KEY, LineChange::NoteDeleted, OLD),
            page(KEY, "P1", LineChange::New, OLD),
            page(NEW_KEY, "Q1", LineChange::New, YOUNG),
        ]);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![3, 4]);
    }

    #[test]
    fn a_moved_note_keeps_the_text_under_its_old_key() {
        let f = fixture();
        let mut moved = note_line(NEW_KEY, LineChange::NoteMoved, OLD);
        moved.old_note = Some(KEY.into());
        moved.path = Some("NOTE/b.note".into());
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD), // 1: superseded after the move
            page(KEY, "P2", LineChange::New, OLD), // 2: still current text
            page(KEY, "P1", LineChange::Edited, OLD), // 3: superseded by 5
            moved,                                 // 4
            page(NEW_KEY, "P1", LineChange::Edited, OLD), // 5: current P1
            page(NEW_KEY, "P3", LineChange::New, YOUNG), // 6
        ]);
        f.prune(NOW);
        assert_eq!(f.seqs(), vec![2, 4, 5, 6]);
    }

    #[test]
    fn a_prune_runs_once_a_day() {
        let f = fixture();
        f.append(vec![
            page(KEY, "P1", LineChange::New, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
        ]);
        let first = f.prune(NOW);
        assert_eq!((first.removed, first.skipped), (1, false));
        f.append(vec![
            page(KEY, "P1", LineChange::Edited, OLD),
            page(KEY, "P1", LineChange::Edited, OLD),
        ]);
        let second = f.prune(NOW + DAY_MS - 1);
        assert!(second.skipped);
        assert_eq!((second.kept, second.removed), (0, 0));
        assert_eq!(f.seqs(), vec![2, 3, 4], "a skipped prune reads nothing");
        let third = f.prune(NOW + DAY_MS);
        assert_eq!((third.removed, third.skipped), (2, false));
        assert_eq!(f.seqs(), vec![4]);
    }
}
