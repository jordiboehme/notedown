//! What is due (spec 4.1), and the lines of whole notes: `note.deleted`, `note.moved` and
//! `note.unscoped` (spec 2.2, 4.5).

use std::collections::BTreeMap;

use notedown_cloud::pages::{PageFingerprint, PageRecord};
use notedown_note::{Notebook, VersionPolicy};

use crate::errors::{ErrorEntry, Errors};
use crate::model::{LineChange, LogLine, NoteState, PageOcrState, PageState, title_of};
use crate::records::Record;
use crate::scope::in_scope;
use crate::source::{NoteBytes, ServerData};
use crate::store::{TextStore, WriterLock};
use crate::{PipelineConfig, Result};

/// The text store of one account with the writer lock that allows writing it.
#[derive(Debug, Clone, Copy)]
pub struct AccountStore<'a> {
    pub text: &'a TextStore,
    pub lock: &'a WriterLock,
}

/// A note with work to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueNote {
    pub key: String,
    /// The wire path.
    pub path: String,
    /// Ms since the epoch of the last page change: the mtime of the note's page record.
    pub since: u64,
    /// The pages to read, by `pageId`, in page order. Empty when only hashes, page positions,
    /// removed pages or metadata need an update.
    pub pages: Vec<String>,
    /// No page of the note has text yet. Such notes go newest first (the first run).
    pub new: bool,
}

/// What to do with one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageWork {
    /// OCR it.
    Read,
    /// Its tries are used up (a crash between the last try and the state): mark it `failed`.
    MarkFailed,
}

/// Whether the page's strokes and layers are those of the record.
pub(super) fn same_ink(p: &PageState, fp: &PageFingerprint) -> bool {
    p.strokes_hash == fp.strokes && p.layers_hash == fp.layers_hash()
}

/// The work of page `fp` of note `key` at `now`: a page without a state or still `pending`,
/// or whose strokes or layers changed, is read, unless it waits for a retry. A `failed` page
/// is read again only after a new change, from its first try.
pub(super) fn page_work(
    key: &str,
    fp: &PageFingerprint,
    page: Option<&PageState>,
    errors: &Errors,
    now: u64,
) -> Option<PageWork> {
    let changed = page.is_none_or(|p| p.state == PageOcrState::Pending || !same_ink(p, fp));
    if !changed {
        return None;
    }
    if page.is_some_and(|p| p.state == PageOcrState::Failed) {
        // The ink changed since it failed: a fresh start.
        return Some(PageWork::Read);
    }
    match errors.get(key, Some(&fp.page_id)) {
        Some(e) if e.exhausted() => Some(PageWork::MarkFailed),
        Some(e) if e.waits(now) => None,
        _ => Some(PageWork::Read),
    }
}

/// Whether a note that did not parse waits: same bytes as then, and its tries are used up or
/// the retry time is not reached.
pub(super) fn note_waits(
    entry: Option<&ErrorEntry>,
    state: Option<&NoteState>,
    rec: &PageRecord,
    now: u64,
) -> bool {
    let same = state.is_some_and(|s| s.md5 == rec.md5 && s.size == rec.size);
    entry.is_some_and(|e| same && (e.exhausted() || e.waits(now)))
}

pub(super) fn has_text(state: &NoteState) -> bool {
    state.pages.iter().any(|p| p.seq.is_some())
}

/// The device keywords read from note files, by account and note key, with the md5 they were
/// read at (`None`: the note did not parse). A note out of scope has no state to hold them, so
/// without this every pass would parse it again.
#[derive(Debug, Default)]
pub struct KeywordCache(BTreeMap<(String, String), Known>);

/// The keywords of one note file and the md5 they were read at.
#[derive(Debug)]
struct Known {
    md5: String,
    words: Option<Vec<String>>,
}

impl KeywordCache {
    /// The keywords of note `key` of account `slug` when they are known for `md5`.
    fn known(&self, slug: &str, key: &str, md5: &str) -> Option<Option<Vec<String>>> {
        self.0
            .get(&(slug.to_string(), key.to_string()))
            .filter(|k| k.md5 == md5)
            .map(|k| k.words.clone())
    }

    /// The keywords of note `key` of account `slug` at `md5`, from `read` only when they are
    /// not known for these bytes.
    fn get_or(
        &mut self,
        slug: &str,
        key: &str,
        md5: &str,
        read: impl FnOnce() -> Option<Vec<String>>,
    ) -> Option<Vec<String>> {
        let id = (slug.to_string(), key.to_string());
        if let Some(known) = self.0.get(&id)
            && known.md5 == md5
        {
            return known.words.clone();
        }
        let words = read();
        let md5 = md5.to_string();
        self.0.insert(
            id,
            Known {
                md5,
                words: words.clone(),
            },
        );
        words
    }
}

/// The device keywords of a note for the scope check: from the state while its bytes are the
/// ones it was read from, else from the note file (only when `ingest_keyword` is set), read
/// once per note bytes through `cache`. A note the source cannot give now keeps the scope it
/// had and is not remembered, so it is read again next time.
fn keywords(
    source: &dyn ServerData,
    slug: &str,
    cfg: &PipelineConfig,
    cache: &mut KeywordCache,
    (key, rec): (&str, &PageRecord),
    state: Option<&NoteState>,
) -> Vec<String> {
    let from_state = || state.map(|s| s.meta.keywords.clone()).unwrap_or_default();
    if cfg.ingest_keyword.is_none() || state.is_some_and(|s| s.md5 == rec.md5) {
        return from_state();
    }
    if let Some(words) = cache.known(slug, key, &rec.md5) {
        return words.unwrap_or_else(from_state);
    }
    let bytes = match source.note(slug, &rec.path) {
        Ok(NoteBytes::Found(b)) => Some(b),
        Ok(NoteBytes::Missing) => None,
        Err(_) => return from_state(),
    };
    let parsed = cache.get_or(slug, key, &rec.md5, || {
        let nb = Notebook::from_bytes(bytes?, VersionPolicy::AllowUnknown).ok()?;
        Some(
            nb.keywords
                .iter()
                .filter_map(|k| k.text.as_deref())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect(),
        )
    });
    // A note that does not parse keeps the scope it had.
    parsed.unwrap_or_else(from_state)
}

/// The due notes of account `slug`: its page records against its note states. On the way it
/// writes the lines of whole notes: `note.moved` with `oldNote` for a note re-keyed by a move
/// (a gone key whose md5 and size are those of a new record without a state; no OCR),
/// `note.deleted` for a note whose record is gone (also after `note.unscoped`),
/// `note.moved` for a new path, `note.unscoped` (once, and only for a note with text) for a
/// note that left the scope. A note whose record is there but whose file is missing is skipped
/// until the server's scan moves or drops the record. The errors of a note whose record is gone
/// are cleared. A note that comes back (into the scope or after a deletion) gets
/// a `new` line per page with text, with its last text, so a consumer that dropped it sees
/// it again. Notes out of scope without a state get nothing.
pub fn due(
    source: &dyn ServerData,
    slug: &str,
    store: &AccountStore,
    cfg: &PipelineConfig,
    cache: &mut KeywordCache,
    now: u64,
) -> Result<Vec<DueNote>> {
    let recs = source.records(slug)?;
    let mut errors = Errors::load(store.text.dir())?;
    let mut states: BTreeMap<String, NoteState> = store
        .text
        .notes()?
        .into_iter()
        .map(|n| (n.key.clone(), n))
        .collect();
    // A note keyed by its path gets a new key when it moves: a gone key whose bytes are those
    // of a new record without a state is that note, moved.
    let gone: Vec<String> = states
        .iter()
        .filter(|(k, s)| !s.deleted && !s.md5.is_empty() && recs.gone(k))
        .map(|(k, _)| k.clone())
        .collect();
    for old in gone {
        let st = &states[&old];
        let target = recs
            .records
            .iter()
            .find(|(k, r)| {
                !states.contains_key(*k) && r.record.md5 == st.md5 && r.record.size == st.size
            })
            .map(|(k, r)| (k.clone(), r.record.clone()));
        if let Some((new, rec)) = target {
            let mut state = states.remove(&old).expect("listed above");
            // Same bytes, so the keywords of the state hold.
            let words = keywords(source, slug, cfg, cache, (&new, &rec), Some(&state));
            let listed = !state.unscoped && in_scope(cfg, &rec.path, &words);
            rekey(store, &mut state, &new, &rec.path, listed)?;
            // The tries of the old key are of no note any more.
            errors.clear_note(store.lock, &old)?;
            states.insert(new, state);
        }
    }
    for (key, state) in states.iter_mut() {
        if !state.deleted && recs.gone(key) {
            // Also for a note that left the scope before: the consumer learns it is gone.
            if has_text(state) {
                store.text.append(
                    store.lock,
                    &mut [LogLine::new(key, LineChange::NoteDeleted)],
                )?;
            }
            state.deleted = true;
            store.text.save_note(store.lock, state)?;
        }
    }
    // The errors of every note whose record is gone (a note that never got a state, too).
    let stale: Vec<String> = errors
        .entries()
        .iter()
        .filter(|e| recs.gone(&e.note))
        .map(|e| e.note.clone())
        .collect();
    for key in stale {
        errors.clear_note(store.lock, &key)?;
    }
    let mut out = Vec::new();
    for (key, rec) in &recs.records {
        let r = &rec.record;
        let state = states.get_mut(key);
        let words = keywords(source, slug, cfg, cache, (key, r), state.as_deref());
        if !in_scope(cfg, &r.path, &words) {
            if let Some(state) = state {
                leave_scope(store, state)?;
            }
            continue;
        }
        // Planned before `enter`, which changes nothing the plan reads.
        let work = plan(key, rec, state.as_deref(), &errors, now);
        // A note file moved or deleted by hand before the server's scan moved or dropped its
        // record: the note waits for the scan, untouched. A dropped record then gives
        // `note.deleted` above. Looked at only for a note with work, errors, or to bring back,
        // so an idle note costs no file check. Only a certain "not there" counts; a check that
        // gives no answer lets the read find out.
        let reviving = state.as_ref().is_some_and(|s| s.deleted || s.unscoped);
        let failing = errors.entries().iter().any(|e| e.note == *key);
        if (work.is_some() || failing || reviving)
            && matches!(source.note_exists(slug, &r.path), Ok(Some(false)))
        {
            continue;
        }
        if let Some(s) = state {
            enter(store, s, r)?;
        }
        out.extend(work);
    }
    Ok(out)
}

/// Moves the state of a note to its new key `new` at `path`: the same pages, blocks, ids and
/// successors. A note with text gets `note.moved` under the new key with `oldNote`; `path` and
/// `oldPath` only when the note is `listed` (it was not unscoped and its new path is in
/// scope), since notes out of scope are not listed (spec 4.5). A note that moved out of scope
/// gets its `note.unscoped` from the scope check after this. The line comes first, then the
/// state under the new key, then the old state file goes (crash repair finishes the move from
/// the line, which is why it always carries `oldNote`).
fn rekey(
    store: &AccountStore,
    state: &mut NoteState,
    new: &str,
    path: &str,
    listed: bool,
) -> Result<()> {
    let old = std::mem::replace(&mut state.key, new.to_string());
    if has_text(state) {
        let mut line = LogLine::new(new, LineChange::NoteMoved);
        line.old_note = Some(old.clone());
        if listed {
            line.path = Some(path.to_string());
            line.old_path = Some(state.path.clone()).filter(|p| !p.is_empty());
        }
        store.text.append(store.lock, &mut [line])?;
    }
    state.path = path.to_string();
    state.title = title_of(path);
    store.text.save_note(store.lock, state)?;
    store.text.remove_note(store.lock, &old)
}

/// A note that left the scope: one `note.unscoped` line, without a path.
fn leave_scope(store: &AccountStore, state: &mut NoteState) -> Result<()> {
    if state.unscoped || state.deleted {
        return Ok(());
    }
    if has_text(state) {
        store.text.append(
            store.lock,
            &mut [LogLine::new(&state.key, LineChange::NoteUnscoped)],
        )?;
    }
    state.unscoped = true;
    store.text.save_note(store.lock, state)
}

/// A note in scope with a state: brings it back after `note.unscoped` or `note.deleted`, or
/// writes `note.moved` for a new path.
fn enter(store: &AccountStore, state: &mut NoteState, rec: &PageRecord) -> Result<()> {
    let mut lines = Vec::new();
    if state.unscoped || state.deleted {
        for page in state.pages.iter().filter(|p| p.seq.is_some()) {
            let mut line = LogLine::new(&state.key, LineChange::New);
            line.page_id = Some(page.page_id.clone());
            line.page_hash = Some(page.page_hash.clone()).filter(|h| !h.is_empty());
            line.blocks = page.blocks.clone();
            line.path = Some(rec.path.clone());
            line.position = Some(page.position);
            lines.push(line);
        }
        state.unscoped = false;
        state.deleted = false;
    } else if state.path != rec.path {
        if !state.path.is_empty() {
            let mut line = LogLine::new(&state.key, LineChange::NoteMoved);
            line.path = Some(rec.path.clone());
            line.old_path = Some(state.path.clone());
            lines.push(line);
        }
    } else {
        return Ok(());
    }
    state.path = rec.path.clone();
    state.title = title_of(&rec.path);
    store.text.append(store.lock, &mut lines)?;
    for line in &lines {
        if let Some(page) = state
            .pages
            .iter_mut()
            .find(|p| line.page_id.as_deref() == Some(p.page_id.as_str()))
        {
            page.seq = Some(line.seq);
        }
    }
    store.text.save_note(store.lock, state)
}

/// The work of a note in scope, `None` when there is none.
fn plan(
    key: &str,
    rec: &Record,
    state: Option<&NoteState>,
    errors: &Errors,
    now: u64,
) -> Option<DueNote> {
    let r = &rec.record;
    if note_waits(errors.get(key, None), state, r, now) {
        return None;
    }
    let find = |id: &str| state.and_then(|s| s.pages.iter().find(|p| p.page_id == id));
    let mut fps: Vec<&PageFingerprint> = r.pages.iter().collect();
    fps.sort_by_key(|fp| fp.position);
    let pages: Vec<String> = fps
        .iter()
        .filter(|fp| page_work(key, fp, find(&fp.page_id), errors, now).is_some())
        .map(|fp| fp.page_id.clone())
        .collect();
    let other = match state {
        None => true,
        Some(s) => {
            s.md5 != r.md5
                || s.size != r.size
                || s.pages.len() != r.pages.len()
                || r.pages.iter().any(|fp| match find(&fp.page_id) {
                    None => true,
                    Some(p) => {
                        p.position != fp.position
                            || (p.page_hash != fp.total
                                && p.state != PageOcrState::Pending
                                && same_ink(p, fp))
                    }
                })
        }
    };
    if pages.is_empty() && !other {
        return None;
    }
    Some(DueNote {
        key: key.to_string(),
        path: r.path.clone(),
        since: rec.changed_ms,
        pages,
        new: state.is_none_or(|s| !has_text(s)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_read_once_per_note_bytes() {
        let mut cache = KeywordCache::default();
        let mut reads = 0;
        let mut read = |words: &[&str]| {
            reads += 1;
            Some(words.iter().map(|w| w.to_string()).collect())
        };
        assert_eq!(
            cache.get_or("a", "k", "m1", || read(&["ai"])),
            Some(vec!["ai".into()])
        );
        assert_eq!(
            cache.get_or("a", "k", "m1", || read(&["x"])),
            Some(vec!["ai".into()])
        );
        // other bytes, another note or another account read again
        assert_eq!(cache.get_or("a", "k", "m2", || read(&[])), Some(vec![]));
        cache.get_or("a", "k2", "m2", || read(&[]));
        cache.get_or("b", "k", "m2", || read(&[]));
        // a note that did not parse is not read again either
        assert_eq!(cache.get_or("a", "k3", "m", || None), None);
        assert_eq!(cache.get_or("a", "k3", "m", || read(&["ai"])), None);
        assert_eq!(reads, 4);
    }
}
