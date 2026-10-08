//! The pending changes of one push target, collapsed per file or folder.
//!
//! Ratta merges every pending item into one push, without a cap (`docs/sync-protocol.md` 4.4).
//! notedown keeps no list of events for that: each event updates the net change of its entry
//! (a file or folder id), so the pending state of a target never holds more entries than the
//! account has (plus the entries deleted since the last push), however many events arrive
//! before the push goes out.

use std::collections::HashMap;

use crate::journal::{Event, EventBody};

/// The net change of one file or folder since the last push to the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub folder: bool,
    pub id: u64,
    /// The path the device knows: the path before the first pending event. `None` when the
    /// entry was created or copied after the last push.
    pub known_path: Option<String>,
    /// The path in the newest event; `None` when the entry is deleted. The item takes the path
    /// of the store when it is built (a parent folder may have moved since).
    pub path: Option<String>,
    /// The path before the newest event: what the device knows after a sync that saw every
    /// event but the newest ([`Changes::retain_after`]).
    pub prev_path: Option<String>,
    /// The newest event gave the file new content.
    pub last_content: bool,
    /// The latest md5 and size of a file whose content is new (created or modified).
    pub content: Option<(String, u64)>,
    /// The source id of the copy that made the file, while its content did not change after. A
    /// copy of a source that is new or changed in the same window takes the source's content
    /// instead: the device's copy of the source would be stale or missing.
    pub copy_of: Option<u64>,
    /// The device behind the newest event (`equipmentNo`), `None` for CLI and scan changes.
    pub device: Option<String>,
    /// The journal `seq` of the first and of the newest event.
    pub first_seq: u64,
    pub last_seq: u64,
}

/// What one event does to its entry.
enum Step<'a> {
    /// The entry is made: created, copied or a folder created.
    New {
        path: &'a str,
        content: Option<(&'a str, u64)>,
        copy_of: Option<u64>,
    },
    /// New content at `path`.
    Modified {
        path: &'a str,
        md5: &'a str,
        size: u64,
    },
    Moved {
        path: &'a str,
        old_path: &'a str,
    },
    Deleted {
        path: &'a str,
    },
}

/// `(folder, id, step)` of a file or folder event; `None` for any other event.
fn step(body: &EventBody) -> Option<(bool, u64, Step<'_>)> {
    Some(match body {
        EventBody::FileCreated {
            id,
            path,
            md5,
            size,
            ..
        } => (
            false,
            *id,
            Step::New {
                path,
                content: Some((md5, *size)),
                copy_of: None,
            },
        ),
        EventBody::FileModified {
            id,
            path,
            md5,
            size,
            ..
        } => (
            false,
            *id,
            Step::Modified {
                path,
                md5,
                size: *size,
            },
        ),
        EventBody::FileMoved { id, path, old_path } => (false, *id, Step::Moved { path, old_path }),
        EventBody::FileCopied {
            id,
            source_id,
            path,
        } => (
            false,
            *id,
            Step::New {
                path,
                content: None,
                copy_of: Some(*source_id),
            },
        ),
        EventBody::FileDeleted { id, path, .. } => (false, *id, Step::Deleted { path }),
        EventBody::FolderCreated { id, path } => (
            true,
            *id,
            Step::New {
                path,
                content: None,
                copy_of: None,
            },
        ),
        EventBody::FolderMoved { id, path, old_path } => {
            (true, *id, Step::Moved { path, old_path })
        }
        EventBody::FolderDeleted { id, path } => (true, *id, Step::Deleted { path }),
        _ => return None,
    })
}

/// The pending changes of one target, one [`Change`] per file or folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    entries: HashMap<(bool, u64), Change>,
}

impl Changes {
    /// Fold `event` into the change of its entry. Events that are no file or folder change are
    /// ignored.
    pub fn apply(&mut self, event: &Event) {
        let Some((folder, id, step)) = step(&event.body) else {
            return;
        };
        // What the device has of a copy's source: nothing or old bytes when the source is new or
        // changed in this window. Then the copy is a new file with the source's content.
        let source = match &step {
            Step::New {
                copy_of: Some(src), ..
            } => self
                .entries
                .get(&(false, *src))
                .filter(|s| s.known_path.is_none() || s.content.is_some())
                .map(|s| (s.content.clone(), s.copy_of)),
            _ => None,
        };
        let c = self.entries.entry((folder, id)).or_insert_with(|| {
            let known_path = match &step {
                Step::New { .. } => None,
                Step::Modified { path, .. } | Step::Deleted { path } => Some(path.to_string()),
                Step::Moved { old_path, .. } => Some(old_path.to_string()),
            };
            Change {
                folder,
                id,
                path: known_path.clone(),
                prev_path: None,
                last_content: false,
                known_path,
                content: None,
                copy_of: None,
                device: None,
                first_seq: event.seq,
                last_seq: event.seq,
            }
        });
        c.prev_path = c.path.clone();
        match step {
            Step::New {
                path,
                content,
                copy_of,
            } => {
                c.path = Some(path.to_string());
                c.content = content.map(|(m, s)| (m.to_string(), s));
                c.copy_of = copy_of;
                if let Some((content, copy_of)) = source {
                    c.copy_of = if content.is_some() { None } else { copy_of };
                    c.content = content;
                }
            }
            Step::Modified { path, md5, size } => {
                c.path = Some(path.to_string());
                c.content = Some((md5.to_string(), size));
                c.copy_of = None;
            }
            Step::Moved { path, .. } => c.path = Some(path.to_string()),
            Step::Deleted { .. } => {
                c.path = None;
                c.content = None;
                c.copy_of = None;
            }
        }
        c.last_content =
            c.content.is_some() && matches!(step, Step::New { .. } | Step::Modified { .. });
        c.device = event.device.clone();
        c.last_seq = c.last_seq.max(event.seq);
    }

    /// Drop the changes a sync up to `seq` has seen: those whose newest event is at or before
    /// `seq`. A change with events on both sides of `seq` (a race of the sync start with the
    /// follower) keeps only its newest event: the device knows the path before it, and the
    /// content is new only when that event changed it. With two or more events after `seq`
    /// the older ones count as seen, which is the closest the collapsed state can tell.
    pub fn retain_after(&mut self, seq: u64) {
        self.entries.retain(|_, c| c.last_seq > seq);
        for c in self.entries.values_mut() {
            if c.first_seq <= seq {
                c.known_path = c.prev_path.clone();
                if !c.last_content {
                    c.content = None;
                }
                c.copy_of = None;
                c.first_seq = c.last_seq;
            }
        }
    }

    /// The change of the file `id`, if it has one.
    pub fn file(&self, id: u64) -> Option<&Change> {
        self.entries.get(&(false, id))
    }

    /// The number of entries with a pending change.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The changes in the order of their first event, as Ratta queued the first item of each
    /// (a new folder comes before the files put into it).
    pub fn in_order(&self) -> Vec<&Change> {
        let mut v: Vec<&Change> = self.entries.values().collect();
        v.sort_by_key(|c| (c.first_seq, c.folder, c.id));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{FileKind, Origin};

    fn ev(seq: u64, body: EventBody) -> Event {
        let mut e = Event::new("acc", Origin::Cli, None, "op", body);
        e.seq = seq;
        e
    }

    fn created(id: u64, path: &str, md5: &str) -> EventBody {
        EventBody::FileCreated {
            id,
            path: path.into(),
            kind: FileKind::Note,
            md5: md5.into(),
            size: 1,
            pages_changed: None,
        }
    }

    fn modified(id: u64, path: &str, md5: &str) -> EventBody {
        EventBody::FileModified {
            id,
            path: path.into(),
            kind: FileKind::Note,
            md5: md5.into(),
            prev_md5: "p".into(),
            size: 2,
            pages_changed: None,
        }
    }

    fn moved(id: u64, old: &str, new: &str) -> EventBody {
        EventBody::FileMoved {
            id,
            path: new.into(),
            old_path: old.into(),
        }
    }

    fn changes(events: Vec<EventBody>) -> Changes {
        let mut c = Changes::default();
        for (i, body) in events.into_iter().enumerate() {
            c.apply(&ev(i as u64 + 1, body));
        }
        c
    }

    fn only(c: &Changes) -> &Change {
        assert_eq!(c.len(), 1);
        c.in_order()[0]
    }

    #[test]
    fn created_then_modified_is_one_new_file_with_the_latest_content() {
        let c = changes(vec![created(1, "N/a", "m1"), modified(1, "N/a", "m2")]);
        let ch = only(&c);
        assert_eq!(ch.known_path, None);
        assert_eq!(ch.path.as_deref(), Some("N/a"));
        assert_eq!(ch.content, Some(("m2".into(), 2)));
        assert_eq!((ch.first_seq, ch.last_seq), (1, 2));
    }

    #[test]
    fn created_then_deleted_leaves_nothing_to_tell() {
        let c = changes(vec![
            created(1, "N/a", "m1"),
            EventBody::FileDeleted {
                id: 1,
                path: "N/a".into(),
                recycle: "r".into(),
            },
        ]);
        let ch = only(&c);
        assert_eq!((ch.known_path.as_deref(), ch.path.as_deref()), (None, None));
    }

    #[test]
    fn moves_keep_the_first_old_path_and_the_last_new_path() {
        let c = changes(vec![moved(1, "N/a", "N/b"), moved(1, "N/b", "M/c")]);
        let ch = only(&c);
        assert_eq!(ch.known_path.as_deref(), Some("N/a"));
        assert_eq!(ch.path.as_deref(), Some("M/c"));
        assert_eq!(ch.content, None);
    }

    #[test]
    fn a_delete_after_a_move_keeps_the_path_the_device_knows() {
        let c = changes(vec![
            moved(1, "N/a", "N/b"),
            EventBody::FileDeleted {
                id: 1,
                path: "N/b".into(),
                recycle: "r".into(),
            },
        ]);
        let ch = only(&c);
        assert_eq!(ch.known_path.as_deref(), Some("N/a"));
        assert_eq!(ch.path, None);
    }

    #[test]
    fn files_and_folders_with_the_same_id_do_not_mix() {
        let c = changes(vec![
            created(7, "N/a", "m"),
            EventBody::FolderCreated {
                id: 7,
                path: "N/F".into(),
            },
        ]);
        assert_eq!(c.len(), 2);
        let order: Vec<bool> = c.in_order().iter().map(|ch| ch.folder).collect();
        assert_eq!(order, [false, true]);
    }

    #[test]
    fn many_events_on_few_entries_stay_small() {
        let mut c = Changes::default();
        for seq in 1..=10_000u64 {
            c.apply(&ev(seq, modified(seq % 3, "N/a", &format!("m{seq}"))));
        }
        assert_eq!(c.len(), 3);
        assert!(c.in_order().iter().all(|ch| ch.last_seq > 9_990));
    }

    fn deleted(id: u64, path: &str) -> EventBody {
        EventBody::FileDeleted {
            id,
            path: path.into(),
            recycle: "r".into(),
        }
    }

    fn apply_at(c: &mut Changes, seq: u64, body: EventBody) {
        c.apply(&ev(seq, body));
    }

    #[test]
    fn a_sync_between_create_and_delete_leaves_a_delete() {
        let mut c = Changes::default();
        apply_at(&mut c, 5, created(1, "N/a", "m"));
        apply_at(&mut c, 12, deleted(1, "N/a"));
        c.retain_after(10);
        let ch = only(&c);
        assert_eq!(ch.known_path.as_deref(), Some("N/a"));
        assert_eq!(ch.path, None);
    }

    #[test]
    fn a_sync_between_create_and_move_leaves_a_move_without_content() {
        let mut c = Changes::default();
        apply_at(&mut c, 5, created(1, "N/a", "m"));
        apply_at(&mut c, 12, moved(1, "N/a", "N/b"));
        c.retain_after(10);
        let ch = only(&c);
        assert_eq!(ch.known_path.as_deref(), Some("N/a"));
        assert_eq!(ch.path.as_deref(), Some("N/b"));
        assert_eq!(ch.content, None);
        assert_eq!(ch.first_seq, 12);
        // A change of content after the sync stays new content.
        let mut c = Changes::default();
        apply_at(&mut c, 5, created(1, "N/a", "m"));
        apply_at(&mut c, 12, modified(1, "N/a", "m2"));
        c.retain_after(10);
        assert_eq!(only(&c).content, Some(("m2".into(), 2)));
        assert_eq!(only(&c).known_path.as_deref(), Some("N/a"));
    }

    #[test]
    fn a_copy_of_a_new_or_changed_source_takes_its_content() {
        let copied = |id, source_id| EventBody::FileCopied {
            id,
            source_id,
            path: format!("N/copy{id}"),
        };
        // new source
        let c = changes(vec![created(1, "N/a", "m1"), copied(2, 1)]);
        let copy = c.file(2).unwrap();
        assert_eq!(
            (copy.content.clone(), copy.copy_of),
            (Some(("m1".into(), 1)), None)
        );
        // changed source
        let c = changes(vec![modified(1, "N/a", "m2"), copied(2, 1)]);
        assert_eq!(c.file(2).unwrap().content, Some(("m2".into(), 2)));
        // a source the device has as it is: a copy
        let c = changes(vec![moved(1, "N/a", "N/b"), copied(2, 1)]);
        let copy = c.file(2).unwrap();
        assert_eq!((copy.content.clone(), copy.copy_of), (None, Some(1)));
    }

    #[test]
    fn retain_after_drops_what_a_sync_has_seen() {
        let mut c = changes(vec![
            created(1, "N/a", "m"),
            created(2, "N/b", "m"),
            modified(1, "N/a", "m2"),
        ]);
        c.retain_after(2);
        let ids: Vec<u64> = c.in_order().iter().map(|ch| ch.id).collect();
        assert_eq!(ids, [1]);
        c.retain_after(3);
        assert!(c.is_empty());
    }
}
