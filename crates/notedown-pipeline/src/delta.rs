//! Deltas, manifests and fixed ranges: what an agent gets from `get_changes` (spec 5).
//!
//! A delta covers the log lines with `from < seq <= up_to`. The text "before" of a page is its
//! text at `from` (its newest version at or before `from`), the text "now" its text at
//! `up_to`. Every log line is read once, in one pass; `up_to`, `more` and the `seq` of a
//! "since <time>" all come from that pass, so a prune between two reads cannot split them.
//!
//! Decisions (spec points left open):
//! - `from` is exclusive everywhere: a bookmark, a `since` seq and the `from` of a fixed range
//!   name the last line the consumer has seen. The text "seq A to seq B" of a fixed range
//!   means "from the text at seq A to the text at seq B".
//! - "Since <time>" starts after the last line before the first line with `ts >= time`.
//! - The cut ([`MAX_CHARS`]) counts the characters of the block texts of every line in the
//!   range, also of notes a `notes` filter leaves out, so `up_to` does not depend on the
//!   filter and matches the manifest. A fixed range is never cut (a sub-agent cannot loop);
//!   its `up_to` is clamped to the last line, and `from > up_to` is an error.
//! - A range that starts at seq 0 from a bookmark or a `since` seq has an empty base: an
//!   unknown consumer's first delta lists everything as new (spec 5.3, the first import). A
//!   "since <time>" has no such base: a page whose first kept line is not `new` and whose text
//!   at `from` the log does not hold gets `beforeUnknown` (spec 5.3) instead of a change, as one
//!   entry whose `block` is the page anchor without a block id.
//! - The change of a page is derived from its text before and now, never copied from a line:
//!   the worker writes `new` lines also when a note comes back into the scope.
//! - A note that was unscoped at `from` and is in scope again at `up_to` is `new`; its pages
//!   show the change against the text the consumer kept (it never retires on `unscoped`).
//! - A note created and then unscoped inside the range is left out, like one created and
//!   deleted (spec 5.1).
//! - Notes with pending pages that the answer does not list otherwise are listed (`pending: true`, no pages,
//!   `new` when no page has text yet, else `edited`) only in an answer that reaches the end of
//!   the log (`more: false`), so a loop lists them once. A pending page never holds back
//!   `up_to`.
//! - Dates in `rangeText` and `beforeUnknown` are local times with their offset
//!   (`2026-10-08 23:14 +02:00`); the caller passes the offset (ruling of fix round 1).
//! - A note whose last text went with page deletions (its other pages still pending) is
//!   `edited` with those deletions: the next base no longer holds the blocks.
//! - A re-keyed note (`note.moved` with `oldNote`) is reported under its key at `up_to`; only
//!   moves up to `up_to` count, and `oldNote` is the key the consumer knew at `from`. Keywords,
//!   links and pending come from the current note state, found through every move in the log.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use time::{OffsetDateTime, UtcOffset};

use crate::model::{
    Block, Change, LineChange, LogLine, MetaLink, NoteState, PageOcrState, title_of,
};
use crate::store::TextStore;
use crate::text::{WordDiff, same_text, word_diff};
use crate::{Error, Result};

/// The size of one answer in characters (spec 5.2).
pub const MAX_CHARS: usize = 30_000;
/// The cut of a manifest: ten answers, so the main agent can group the range for sub-agents
/// (spec 6.1) and its own context stays small.
pub const MANIFEST_MAX_CHARS: usize = 300_000;

/// A "since X" start (spec 5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    /// The last `seq` seen.
    Seq(u64),
    /// Ms since the epoch.
    Time(u64),
}

/// Where a delta starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// The bookmark of a consumer; an unknown consumer starts at 0.
    Bookmark(String),
    Since(Since),
    /// A fixed range: the lines with `from < seq <= up_to` (spec 5.2a).
    Range {
        from: u64,
        up_to: u64,
    },
}

/// Parses a "since X": a plain number is a `seq` (a date always holds `-`, a time `-` and
/// `:`), otherwise an RFC 3339 time, a date (local midnight at `local`) or a duration back from
/// `now` (`30m`, `12h`, `3d`), as `notedown diag --since` reads them.
pub fn parse_since(text: &str, now: OffsetDateTime, local: UtcOffset) -> Result<Since> {
    let t = text.trim();
    let invalid = || Error::Invalid {
        what: "since",
        message: format!(
            "{t:?} is no seq (412), date (2026-10-09), time (2026-10-09T08:00:00Z) or duration (3d)"
        ),
    };
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map(Since::Seq).map_err(|_| invalid());
    }
    let at = notedown_cloud::diag::collect::parse_since(t, now, local).map_err(|_| invalid())?;
    let ms = at.unix_timestamp_nanos() / 1_000_000;
    Ok(Since::Time(u64::try_from(ms).unwrap_or(0)))
}

/// One delta (spec 5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Delta {
    /// The account slug of the anchors.
    pub slug: String,
    /// Exclusive: the text before is the text at this `seq`.
    pub from: u64,
    pub up_to: u64,
    /// The log has lines after `up_to`.
    pub more: bool,
    /// The range in words.
    pub range_text: String,
    pub notes: Vec<NoteDelta>,
}

/// The change of one note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteDelta {
    pub key: String,
    /// The key the consumer knew, when a move re-keyed the note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_note: Option<String>,
    /// The path now.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub title: String,
    pub keywords: Vec<String>,
    pub links: Vec<MetaLink>,
    pub change: Change,
    pub pending: bool,
    pub pages: Vec<PageDelta>,
}

/// The change of one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageDelta {
    /// 1-based page number.
    pub page: usize,
    pub page_id: String,
    pub change: Change,
    /// The changed parts of the page: `strokes`, `layers`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub signal: Vec<String>,
    /// The full page text now; empty for a deleted or moved page.
    pub text_now: Vec<Block>,
    pub what_changed: Vec<BlockChange>,
    /// The blocks in `text_now` that were not there before (they have no entry).
    #[serde(skip)]
    pub new_blocks: usize,
}

/// The change of one block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockChange {
    /// The anchor `notedown://<slug>/<key>/<pageId>#<blockId>`.
    pub block: String,
    pub change: Change,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub word_diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_before: Option<String>,
    /// The new text in full, when more than half of the words changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_full_now: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replaced_by: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_unknown: Option<String>,
}

/// The manifest of a range (spec 5.2a): no text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub from: u64,
    pub up_to: u64,
    pub more: bool,
    pub notes: Vec<ManifestNote>,
}

/// One note of a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestNote {
    pub key: String,
    pub path: String,
    /// The path without the file name.
    pub folder: String,
    pub change: Change,
    /// Changed pages.
    pub pages: usize,
    /// Changed blocks: the `whatChanged` entries and the new blocks.
    pub blocks: usize,
    pub pending: bool,
    /// The size of the note's delta: the characters of its texts.
    pub chars: usize,
}

/// The manifest of `start`: the delta up to [`MANIFEST_MAX_CHARS`], summed up per note.
pub fn manifest(store: &TextStore, start: &Start, local: UtcOffset) -> Result<Manifest> {
    let d = delta(store, start, None, MANIFEST_MAX_CHARS, local)?;
    let notes = d
        .notes
        .iter()
        .map(|n| ManifestNote {
            key: n.key.clone(),
            path: n.path.clone(),
            folder: n
                .path
                .rsplit_once('/')
                .map_or(String::new(), |(f, _)| f.to_string()),
            change: n.change,
            pages: n.pages.len(),
            blocks: n
                .pages
                .iter()
                .map(|p| p.what_changed.len() + p.new_blocks)
                .sum(),
            pending: n.pending,
            chars: note_chars(n),
        })
        .collect();
    Ok(Manifest {
        from: d.from,
        up_to: d.up_to,
        more: d.more,
        notes,
    })
}

fn chars(s: &Option<String>) -> usize {
    s.as_deref().map_or(0, |s| s.chars().count())
}

fn note_chars(n: &NoteDelta) -> usize {
    n.pages
        .iter()
        .map(|p| {
            p.text_now
                .iter()
                .map(|b| b.text.chars().count())
                .sum::<usize>()
                + p.what_changed
                    .iter()
                    .map(|c| chars(&c.word_diff) + chars(&c.text_before) + chars(&c.text_full_now))
                    .sum::<usize>()
        })
        .sum()
}

/// The size of a line for the cut: the characters of its block texts.
fn line_chars(line: &LogLine) -> usize {
    line.blocks.iter().map(|b| b.text.chars().count()).sum()
}

/// `2026-10-08 23:14 +02:00`: `ms` at the offset `local`.
pub(crate) fn stamp(ms: u64, local: UtcOffset) -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute] [offset_hour sign:mandatory]:[offset_minute]"
    );
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.to_offset(local).format(&format).ok())
        .unwrap_or_else(|| format!("{ms} ms"))
}

/// A move that re-keyed a note: old key, new key, `seq` of the move line.
type Move<'a> = (&'a str, &'a str, u64);

/// The key at `limit` of a line under `key` at `seq`: each later move of the key up to `limit`
/// is followed. A move line raises the `seq`, so a move back ends.
fn key_at<'a>(moves: &[Move<'a>], key: &'a str, seq: u64, limit: u64) -> &'a str {
    let (mut key, mut seq) = (key, seq);
    while let Some(&(_, new, at)) = moves
        .iter()
        .find(|(old, _, at)| *old == key && *at > seq && *at <= limit)
    {
        key = new;
        seq = at;
    }
    key
}

/// A page at one `seq`.
#[derive(Debug, Clone, Default)]
struct PageAt {
    text: Option<Vec<Block>>,
    /// A line at or before the `seq` says what the text is (a version or a deletion).
    determined: bool,
    position: Option<usize>,
}

/// A note at one `seq`.
#[derive(Debug, Default)]
struct NoteAt {
    pages: BTreeMap<String, PageAt>,
    deleted: bool,
    unscoped: bool,
    path: Option<String>,
    /// The key of the newest line.
    key: Option<String>,
}

impl NoteAt {
    fn has_text(&self) -> bool {
        !self.deleted && self.pages.values().any(|p| p.text.is_some())
    }

    fn visible(&self) -> bool {
        self.has_text() && !self.unscoped
    }
}

/// The note of `lines` (in `seq` order) at `seq`.
fn note_at(lines: &[&LogLine], seq: u64) -> NoteAt {
    let mut at = NoteAt::default();
    for l in lines.iter().take_while(|l| l.seq <= seq) {
        at.key = Some(l.note.clone());
        match l.change {
            LineChange::NoteDeleted => {
                at.deleted = true;
                for p in at.pages.values_mut() {
                    p.text = None;
                    p.determined = true;
                }
            }
            LineChange::NoteUnscoped => at.unscoped = true,
            LineChange::NoteMoved => {
                if l.path.is_some() {
                    at.path.clone_from(&l.path);
                }
            }
            LineChange::New | LineChange::Edited | LineChange::Deleted | LineChange::Moved => {
                let Some(id) = &l.page_id else { continue };
                let page = at.pages.entry(id.clone()).or_default();
                if l.position.is_some() {
                    page.position = l.position;
                }
                match l.change {
                    LineChange::New | LineChange::Edited => {
                        page.text = Some(l.blocks.clone());
                        page.determined = true;
                        // Text again: the note lives and is in scope.
                        at.deleted = false;
                        at.unscoped = false;
                        if l.path.is_some() {
                            at.path.clone_from(&l.path);
                        }
                    }
                    LineChange::Deleted => {
                        page.text = None;
                        page.determined = true;
                    }
                    _ => {}
                }
            }
        }
    }
    at
}

/// What a delta builds a note from.
struct Ctx<'a> {
    slug: &'a str,
    /// The offset dates are shown at.
    local: UtcOffset,
    from: u64,
    up_to: u64,
    /// The base at `from` is empty (a first import).
    empty_base: bool,
}

impl Ctx<'_> {
    fn anchor(&self, key: &str, page: &str, block: &str) -> String {
        format!("notedown://{}/{key}/{page}#{block}", self.slug)
    }

    fn page_anchor(&self, key: &str, page: &str) -> String {
        format!("notedown://{}/{key}/{page}", self.slug)
    }

    fn in_range(&self, seq: u64) -> bool {
        seq > self.from && seq <= self.up_to
    }
}

/// The delta of `start` (see the module doc). `notes` keeps only these note keys (the key at
/// `up_to`); `max_chars` is the cut of a bookmark or `since` start. Dates in the texts are
/// shown at the offset `local` (the CLI and the MCP server pass this computer's offset).
pub fn delta(
    store: &TextStore,
    start: &Start,
    notes: Option<&[String]>,
    max_chars: usize,
    local: UtcOffset,
) -> Result<Delta> {
    let slug = store
        .dir()
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut lines: Vec<LogLine> = Vec::new();
    store.read_log(0, None, &mut |l| {
        lines.push(l);
        Ok(())
    })?;
    let last = lines.last().map_or(0, |l| l.seq);

    let mut confirmed = None;
    let (from, fixed) = match start {
        Start::Bookmark(name) => {
            confirmed = store.bookmark(name)?;
            (confirmed.map_or(0, |b| b.seq), None)
        }
        Start::Since(Since::Seq(n)) => (*n, None),
        Start::Since(Since::Time(t)) => (
            lines
                .iter()
                .find(|l| l.ts >= *t)
                .map_or(last, |l| l.seq - 1),
            None,
        ),
        Start::Range { from, up_to } => {
            if from > up_to {
                return Err(Error::Invalid {
                    what: "range",
                    message: format!("from {from} is after upTo {up_to}"),
                });
            }
            (*from, Some((*up_to).min(last).max(*from)))
        }
    };
    let up_to = match fixed {
        Some(u) => u,
        None => {
            let (mut up_to, mut size, mut taken) = (from, 0usize, false);
            for l in lines.iter().filter(|l| l.seq > from) {
                let s = line_chars(l);
                if taken && size + s > max_chars {
                    break;
                }
                size += s;
                up_to = l.seq;
                taken = true;
            }
            up_to
        }
    };
    let more = lines.iter().any(|l| l.seq > up_to);
    let range_text = match start {
        Start::Bookmark(_) => match confirmed {
            Some(b) => format!(
                "since your last confirm on {} (seq {from}) up to seq {up_to}",
                stamp(b.updated_at, local)
            ),
            None => format!("since the start (no confirm yet) up to seq {up_to}"),
        },
        Start::Since(Since::Seq(n)) => format!("since seq {n} up to seq {up_to}"),
        Start::Since(Since::Time(t)) => {
            format!("since {} (seq {from}) up to seq {up_to}", stamp(*t, local))
        }
        Start::Range { .. } => format!("seq {from} to seq {up_to}"),
    };
    let ctx = Ctx {
        slug: &slug,
        local,
        from,
        up_to,
        empty_base: from == 0 && !matches!(start, Start::Since(Since::Time(_))),
    };

    let moves: Vec<Move> = lines
        .iter()
        .filter_map(|l| {
            l.old_note
                .as_deref()
                .map(|old| (old, l.note.as_str(), l.seq))
        })
        .collect();
    // The lines up to `up_to` per note (its key at `up_to`), and the notes in order of their
    // first line in the range.
    let mut by_note: BTreeMap<&str, Vec<&LogLine>> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();
    for l in lines.iter().take_while(|l| l.seq <= up_to) {
        let key = key_at(&moves, &l.note, l.seq, up_to);
        by_note.entry(key).or_default().push(l);
        if ctx.in_range(l.seq) && !order.contains(&key) {
            order.push(key);
        }
    }
    let wanted = |key: &str| notes.is_none_or(|n| n.iter().any(|k| k == key));
    let state_of = |key: &str| -> Result<Option<NoteState>> {
        // The current state: through every move, also those after `up_to`.
        store.note(key_at(&moves, key, up_to, u64::MAX))
    };

    let mut out = Vec::new();
    for key in order {
        if !wanted(key) {
            continue;
        }
        let state = state_of(key)?;
        if let Some(n) = note_delta(&ctx, key, &by_note[key], state.as_ref()) {
            out.push(n);
        }
    }
    if !more {
        for state in store.notes()? {
            let listed = out.iter().any(|n: &NoteDelta| n.key == state.key);
            if listed || !wanted(&state.key) || !pending(&state) {
                continue;
            }
            let change = if state.pages.iter().any(|p| p.seq.is_some()) {
                Change::Edited
            } else {
                Change::New
            };
            out.push(NoteDelta {
                key: state.key.clone(),
                old_note: None,
                path: state.path.clone(),
                old_path: None,
                title: state.title.clone(),
                keywords: state.meta.keywords.clone(),
                links: state.meta.links.clone(),
                change,
                pending: true,
                pages: Vec::new(),
            });
        }
    }
    Ok(Delta {
        slug,
        from,
        up_to,
        more,
        range_text,
        notes: out,
    })
}

/// A note in scope with a page waiting for OCR.
fn pending(state: &NoteState) -> bool {
    !state.deleted
        && !state.unscoped
        && state.pages.iter().any(|p| p.state == PageOcrState::Pending)
}

/// The delta of the note `key` from its `lines` up to `up_to`; `None` when there is nothing
/// to tell.
fn note_delta(
    ctx: &Ctx,
    key: &str,
    lines: &[&LogLine],
    state: Option<&NoteState>,
) -> Option<NoteDelta> {
    let before = if ctx.empty_base {
        NoteAt::default()
    } else {
        note_at(lines, ctx.from)
    };
    let now = note_at(lines, ctx.up_to);

    // Successors: the note state and the `replaces` of the lines.
    let mut successors: BTreeMap<String, Vec<String>> =
        state.map(|s| s.successors.clone()).unwrap_or_default();
    for l in lines {
        for b in &l.blocks {
            for old in &b.replaces {
                let next = successors.entry(old.clone()).or_default();
                if !next.contains(&b.id) {
                    next.push(b.id.clone());
                }
            }
        }
    }
    let note_deleted_in_range = lines
        .iter()
        .any(|l| l.change == LineChange::NoteDeleted && ctx.in_range(l.seq));

    let page_ids: BTreeSet<&String> = before.pages.keys().chain(now.pages.keys()).collect();
    let mut pages = Vec::new();
    let mut unknown = false;
    for id in page_ids {
        let page_lines: Vec<&&LogLine> = lines
            .iter()
            .filter(|l| l.page_id.as_ref() == Some(id))
            .collect();
        let b = before.pages.get(id).cloned().unwrap_or_default();
        let n = now.pages.get(id).cloned().unwrap_or_default();
        let position = n
            .position
            .or(b.position)
            .or_else(|| {
                state
                    .and_then(|s| s.pages.iter().find(|p| &p.page_id == id))
                    .map(|p| p.position)
            })
            .unwrap_or(0);
        let touched = page_lines.iter().any(|l| ctx.in_range(l.seq));
        let first = page_lines.first();
        let known =
            ctx.empty_base || b.determined || first.is_some_and(|l| l.change == LineChange::New);
        let signal: Vec<String> = page_lines
            .iter()
            .filter(|l| ctx.in_range(l.seq))
            .flat_map(|l| l.signal.iter().cloned())
            .fold(Vec::new(), |mut acc, s| {
                if !acc.contains(&s) {
                    acc.push(s);
                }
                acc
            });
        let page_anchor = ctx.page_anchor(key, id);
        if !known {
            if !touched && !(note_deleted_in_range && n.text.is_none()) {
                continue;
            }
            unknown = true;
            let change = if n.text.is_some() {
                Change::Edited
            } else {
                Change::Deleted
            };
            let ts = first.map_or(0, |l| l.ts);
            pages.push(PageDelta {
                page: position + 1,
                page_id: id.clone(),
                change,
                signal,
                text_now: n.text.clone().unwrap_or_default(),
                what_changed: vec![BlockChange {
                    block: page_anchor,
                    change,
                    word_diff: None,
                    text_before: None,
                    text_full_now: None,
                    replaced_by: Vec::new(),
                    before_unknown: Some(format!("no versions before {}", stamp(ts, ctx.local))),
                }],
                new_blocks: 0,
            });
            continue;
        }
        let page = |change, text_now: Vec<Block>, what_changed, new_blocks, signal| PageDelta {
            page: position + 1,
            page_id: id.clone(),
            change,
            signal,
            text_now,
            what_changed,
            new_blocks,
        };
        match (b.text, n.text) {
            (None, None) => {}
            (None, Some(text)) => {
                let count = text.len();
                pages.push(page(Change::New, text, Vec::new(), count, Vec::new()));
            }
            (Some(old), None) => {
                let gone = old
                    .iter()
                    .map(|blk| BlockChange {
                        block: ctx.anchor(key, id, &blk.id),
                        change: Change::Deleted,
                        word_diff: None,
                        text_before: Some(blk.text.clone()),
                        text_full_now: None,
                        replaced_by: Vec::new(),
                        before_unknown: None,
                    })
                    .collect();
                pages.push(page(Change::Deleted, Vec::new(), gone, 0, Vec::new()));
            }
            (Some(old), Some(text)) => {
                let changes = block_changes(ctx, key, id, &old, &text, &successors);
                let fresh = text
                    .iter()
                    .filter(|t| !old.iter().any(|o| o.id == t.id))
                    .count();
                if !changes.is_empty() || fresh > 0 {
                    pages.push(page(Change::Edited, text, changes, fresh, signal));
                } else {
                    let moved = page_lines
                        .iter()
                        .any(|l| l.change == LineChange::Moved && ctx.in_range(l.seq))
                        && b.position != n.position;
                    if moved {
                        pages.push(page(Change::Moved, Vec::new(), Vec::new(), 0, Vec::new()));
                    }
                }
            }
        }
    }
    pages.sort_by_key(|p| p.page);

    let had_text = before.has_text();
    let path_now = now
        .path
        .clone()
        .or_else(|| state.map(|s| s.path.clone()))
        .unwrap_or_default();
    let old_path = match (&before.path, &now.path) {
        (Some(b), Some(n)) if b != n => Some(b.clone()),
        _ => None,
    };
    let moved_in_range = lines
        .iter()
        .any(|l| l.change == LineChange::NoteMoved && ctx.in_range(l.seq));
    let change = if now.deleted {
        if pages.is_empty() {
            return None;
        }
        Change::Deleted
    } else if now.unscoped {
        if !had_text || before.unscoped {
            return None;
        }
        Change::Unscoped
    } else if !now.visible() {
        // No text left, but pages went: their deletions must come now, the next base has
        // them gone.
        if pages.is_empty() {
            return None;
        }
        Change::Edited
    } else if !before.visible() && !unknown {
        Change::New
    } else if !pages.is_empty() {
        Change::Edited
    } else if moved_in_range || old_path.is_some() {
        Change::Moved
    } else {
        return None;
    };
    let old_note = before.key.filter(|k| k != key);
    let title = if path_now.is_empty() {
        state.map_or_else(|| key.to_string(), |s| s.title.clone())
    } else {
        title_of(&path_now)
    };
    Some(NoteDelta {
        key: key.to_string(),
        old_note,
        path: path_now,
        old_path,
        title,
        keywords: state.map(|s| s.meta.keywords.clone()).unwrap_or_default(),
        links: state.map(|s| s.meta.links.clone()).unwrap_or_default(),
        change,
        pending: state.is_some_and(pending),
        pages,
    })
}

/// The `whatChanged` of a page from `old` to `new` blocks: edited, deleted and replaced
/// blocks in the order of `old`. A new block has no entry.
fn block_changes(
    ctx: &Ctx,
    key: &str,
    page: &str,
    old: &[Block],
    new: &[Block],
    successors: &BTreeMap<String, Vec<String>>,
) -> Vec<BlockChange> {
    let mut out = Vec::new();
    for o in old {
        let entry = |change, word_diff, text_full_now, replaced_by| BlockChange {
            block: ctx.anchor(key, page, &o.id),
            change,
            word_diff,
            text_before: None,
            text_full_now,
            replaced_by,
            before_unknown: None,
        };
        if let Some(n) = new.iter().find(|n| n.id == o.id) {
            // Equal after normalization is no change; only then the word diff.
            if same_text(&o.text, &n.text) {
                continue;
            }
            out.push(match word_diff(&o.text, &n.text) {
                WordDiff::Inline(diff) => entry(Change::Edited, Some(diff), None, Vec::new()),
                WordDiff::Full => BlockChange {
                    text_before: Some(o.text.clone()),
                    ..entry(Change::Edited, None, Some(n.text.clone()), Vec::new())
                },
            });
            continue;
        }
        let heirs = heirs(&o.id, new, successors);
        let change = if heirs.is_empty() {
            Change::Deleted
        } else {
            Change::Replaced
        };
        let replaced_by = heirs.iter().map(|h| ctx.anchor(key, page, h)).collect();
        out.push(BlockChange {
            text_before: Some(o.text.clone()),
            ..entry(change, None, None, replaced_by)
        });
    }
    out
}

/// The blocks of `new` that replaced `id`, through chains of replaced blocks (b3 became b7, b7
/// became b9: b9).
pub(crate) fn heirs(
    id: &str,
    new: &[Block],
    successors: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut todo: Vec<String> = vec![id.to_string()];
    while let Some(cur) = todo.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        for next in successors.get(&cur).into_iter().flatten().rev() {
            if new.iter().any(|b| &b.id == next) {
                if !out.contains(next) {
                    out.push(next.clone());
                }
            } else {
                todo.push(next.clone());
            }
        }
    }
    // In page order.
    out.sort_by_key(|h| new.iter().position(|b| &b.id == h));
    out
}

#[cfg(test)]
mod tests;
