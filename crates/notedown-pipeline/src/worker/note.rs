//! Reading one due note: metadata, page moves and deletions, then page by page OCR, each page
//! written at once (spec 2.2, 3, 4.4).

use std::collections::BTreeMap;
use std::path::Path;

use notedown_cloud::pages::PageFingerprint;
use notedown_note::{DigestRef, Notebook, VersionPolicy, decode_text_boxes};

use super::due::{AccountStore, DueNote, PageWork, note_waits, page_work};
use super::{EngineSlot, Progress};
use crate::compose::compose;
use crate::errors::Errors;
use crate::meta::note_meta;
use crate::model::{LineChange, LogLine, NoteState, PageOcrState, PageState, title_of};
use crate::ocr::read_page;
use crate::page::stroke_list;
use crate::records::{self, Record};
use crate::region::{Region, change_region};
use crate::{Error, Result};

/// How the pages of a note are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// The due pages, with the change region.
    Due,
    /// Every page, with the whole page as the region (`--reocr`).
    Reocr,
}

/// Everything one note needs besides the engine.
pub(super) struct NoteJob<'a> {
    pub data: &'a Path,
    pub slug: &'a str,
    pub store: AccountStore<'a>,
    /// The account's records, for the keys of link targets.
    pub records: &'a BTreeMap<String, Record>,
    pub now: &'a dyn Fn() -> u64,
    pub mode: Mode,
}

/// The digests registered in the note's text boxes.
fn digests(nb: &Notebook) -> Vec<DigestRef> {
    let mut out: Vec<DigestRef> = Vec::new();
    for page in 0..nb.pages.len() {
        let Ok(strokes) = nb.strokes(page) else {
            continue;
        };
        let Ok(boxes) = decode_text_boxes(&strokes, page) else {
            continue;
        };
        for d in boxes.into_iter().filter_map(|b| b.digest) {
            if !out.iter().any(|o| o.id == d.id) {
                out.push(d);
            }
        }
    }
    out
}

/// The index in `nb` of the page the record calls `page_id`: `<PAGEID>`, `<PAGEID>#<n>` for
/// the n-th page with that id, or `position:<n>` for a page without an id.
fn page_index(nb: &Notebook, page_id: &str) -> Option<usize> {
    if let Some(n) = page_id.strip_prefix("position:") {
        let n: usize = n.parse().ok()?;
        return nb.pages.get(n).filter(|p| p.id.is_none()).map(|_| n);
    }
    let (base, nth) = match page_id.rsplit_once('#') {
        Some((base, n)) => (base, n.parse::<usize>().ok()?),
        None => (page_id, 1),
    };
    nb.pages
        .iter()
        .filter(|p| p.id.as_deref() == Some(base))
        .nth(nth.checked_sub(1)?)
        .map(|p| p.index)
}

fn page_line(key: &str, change: LineChange, page_id: &str, position: usize) -> LogLine {
    let mut line = LogLine::new(key, change);
    line.page_id = Some(page_id.to_string());
    line.position = Some(position);
    line
}

/// Reads the due note `due`. The note's state is saved after the page structure is brought up
/// to date, and again after every page. Stops early (state saved) when `stop` says so, and
/// returns [`Error::ModelMissing`] when the model cannot be had.
pub(super) fn process(
    job: &NoteJob,
    engine: &mut EngineSlot,
    due: &DueNote,
    progress: &mut Progress,
    report: &mut dyn FnMut(&Progress),
    stop: &dyn Fn() -> bool,
) -> Result<()> {
    let NoteJob { data, slug, .. } = *job;
    let (text, w) = (job.store.text, job.store.lock);
    let key = due.key.as_str();
    let Some(rec) = records::read_key(data, slug, key) else {
        return Ok(());
    };
    let r = &rec.record;
    let mut errors = Errors::load(text.dir())?;
    let mut state = text.note(key)?.unwrap_or_else(|| NoteState {
        key: key.to_string(),
        path: r.path.clone(),
        title: title_of(&r.path),
        next_block: 1,
        ..NoteState::default()
    });
    if state.deleted || state.unscoped {
        // `due` brings such a note back first; this one is out of scope.
        return Ok(());
    }
    let now = (job.now)();
    if job.mode == Mode::Due && note_waits(errors.get(key, None), Some(&state), r, now) {
        return Ok(());
    }
    if state.md5 != r.md5 || state.size != r.size {
        // New bytes: a note that did not parse starts again with fresh tries.
        errors.clear(w, key, None)?;
    }
    state.path = r.path.clone();
    state.title = title_of(&r.path);

    let nb = records::note_file(data, slug, &r.path).and_then(|file| {
        let bytes = std::fs::read(&file).map_err(|source| Error::Io { path: file, source })?;
        Ok(Notebook::from_bytes(bytes, VersionPolicy::AllowUnknown)?)
    });
    let nb = match nb {
        Ok(nb) => nb,
        Err(e) => {
            eprintln!("notedown pipeline: {}: {e}", r.path);
            errors.fail(w, key, None, &e.to_string(), now)?;
            state.md5 = r.md5.clone();
            state.size = r.size;
            text.save_note(w, &state)?;
            progress.failed += 1;
            return Ok(());
        }
    };
    errors.clear(w, key, None)?;

    // Metadata.
    state.meta = note_meta(&nb, &digests(&nb));
    for link in &mut state.meta.links {
        link.key = records::link_key(job.records, link.key.as_deref(), &link.target);
    }
    state.md5 = r.md5.clone();
    state.size = r.size;

    // Removed and moved pages; new pages start `pending`.
    let mut fps: Vec<&PageFingerprint> = r.pages.iter().collect();
    fps.sort_by_key(|fp| fp.position);
    let ids: Vec<&str> = fps.iter().map(|fp| fp.page_id.as_str()).collect();
    let mut lines = Vec::new();
    state.pages.retain(|p| {
        let keep = ids.contains(&p.page_id.as_str());
        if !keep && p.seq.is_some() {
            lines.push(page_line(key, LineChange::Deleted, &p.page_id, p.position));
        }
        keep
    });
    for fp in &fps {
        match state.pages.iter_mut().find(|p| p.page_id == fp.page_id) {
            Some(p) => {
                if p.position != fp.position {
                    if p.seq.is_some() {
                        lines.push(page_line(key, LineChange::Moved, &p.page_id, fp.position));
                    }
                    p.position = fp.position;
                }
                // Only the template, recognition or text boxes changed: the hash, no OCR.
                if p.state != PageOcrState::Pending && super::due::same_ink(p, fp) {
                    p.page_hash = fp.total.clone();
                }
            }
            None => state.pages.push(PageState {
                page_id: fp.page_id.clone(),
                position: fp.position,
                state: PageOcrState::Pending,
                ..PageState::default()
            }),
        }
    }
    state.pages.sort_by_key(|p| p.position);
    errors.retain_pages(w, key, &ids)?;
    text.append(w, &mut lines)?;
    text.save_note(w, &state)?;

    for fp in fps {
        if stop() {
            return Ok(());
        }
        let now = (job.now)();
        let i = state
            .pages
            .iter()
            .position(|p| p.page_id == fp.page_id)
            .expect("every record page has a state");
        let work = match job.mode {
            Mode::Reocr => Some(PageWork::Read),
            Mode::Due => page_work(key, fp, Some(&state.pages[i]), &errors, now),
        };
        match work {
            None => continue,
            Some(PageWork::MarkFailed) => {
                mark_failed(&mut state.pages[i], fp);
                text.save_note(w, &state)?;
                continue;
            }
            Some(PageWork::Read) => {}
        }
        let Some(index) = page_index(&nb, &fp.page_id) else {
            eprintln!(
                "notedown pipeline: {}: page {} is not in the note file (yet)",
                r.path, fp.page_id
            );
            continue;
        };
        if state.pages[i].state == PageOcrState::Failed {
            // A new change after the tries were used up: start again, with retries.
            errors.clear(w, key, Some(&fp.page_id))?;
            state.pages[i].state = PageOcrState::Pending;
        }
        let reading = {
            let engine = engine.get(now)?;
            read_page(&nb, index, engine)
        };
        engine.used((job.now)());
        progress.attempted += 1;
        let reading = match reading {
            Ok(r) => r,
            Err(e) => {
                let entry = errors.fail(w, key, Some(&fp.page_id), &e.to_string(), now)?;
                eprintln!(
                    "notedown pipeline: {} page {}: {e} (try {} of {})",
                    r.path,
                    fp.position + 1,
                    entry.tries,
                    crate::errors::MAX_TRIES
                );
                if entry.exhausted() {
                    mark_failed(&mut state.pages[i], fp);
                }
                text.save_note(w, &state)?;
                progress.failed += 1;
                report(progress);
                continue;
            }
        };
        errors.clear(w, key, Some(&fp.page_id))?;
        progress.pages_read += 1;

        // The change region (spec 3.1).
        let page = &state.pages[i];
        let had_text = page.seq.is_some();
        let strokes_changed = page.strokes_hash != fp.strokes;
        let layers_changed = page.layers_hash != fp.layers_hash();
        let list = stroke_list(&nb, index);
        let region = match (&list, job.mode) {
            (Ok(new), Mode::Due) if had_text => match change_region(&page.stroke_list, new, &[]) {
                Region::Empty if layers_changed => Region::Whole,
                region => region,
            },
            _ => Region::Whole,
        };
        if state.next_block == 0 {
            state.next_block = 1;
        }
        let composed = compose(
            &page.blocks,
            &reading.blocks,
            &region,
            (reading.width, reading.height, reading.landscape),
            &mut state.next_block,
        );
        let mut seq = page.seq;
        if !composed.unchanged {
            let change = if had_text {
                LineChange::Edited
            } else {
                LineChange::New
            };
            let mut line = page_line(key, change, &fp.page_id, fp.position);
            line.page_hash = Some(fp.total.clone());
            line.model = Some(reading.model.clone());
            if had_text {
                if strokes_changed {
                    line.signal.push("strokes".into());
                }
                if layers_changed {
                    line.signal.push("layers".into());
                }
            }
            line.blocks = composed.blocks.clone();
            line.path = Some(state.path.clone());
            let mut batch = [line];
            text.append(w, &mut batch)?;
            seq = Some(batch[0].seq);
            progress.versions += 1;
        }
        for (old, new_ids) in composed.replaced {
            let next = state.successors.entry(old).or_default();
            for id in new_ids {
                if !next.contains(&id) {
                    next.push(id);
                }
            }
        }
        let page = &mut state.pages[i];
        page.seq = seq;
        page.blocks = composed.blocks;
        page.stroke_list = list.unwrap_or_default();
        page.strokes_hash = fp.strokes.clone();
        page.layers_hash = fp.layers_hash();
        page.page_hash = fp.total.clone();
        page.state = if reading.truncated {
            PageOcrState::Truncated
        } else {
            PageOcrState::Done
        };
        text.save_note(w, &state)?;
        if composed.region_missed > 0 {
            text.add_region_missed(w, u64::from(composed.region_missed))?;
            progress.region_missed += u64::from(composed.region_missed);
        }
        report(progress);
    }
    Ok(())
}

/// A page whose tries are used up: `failed`, with the hashes it failed on, so only a new
/// change reads it again. Its last text, if any, stays.
fn mark_failed(page: &mut PageState, fp: &PageFingerprint) {
    page.state = PageOcrState::Failed;
    page.strokes_hash = fp.strokes.clone();
    page.layers_hash = fp.layers_hash();
    page.page_hash = fp.total.clone();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_page_ids_find_their_page() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note");
        let mut nb = Notebook::open(&path, VersionPolicy::AllowUnknown).unwrap();
        let id = nb.pages[0].id.clone().unwrap();
        assert_eq!(page_index(&nb, &id), Some(0));
        assert_eq!(page_index(&nb, &format!("{id}#2")), None);
        assert_eq!(page_index(&nb, "P-unknown"), None);
        assert_eq!(page_index(&nb, "position:0"), None);
        nb.pages[0].id = None;
        assert_eq!(page_index(&nb, "position:0"), Some(0));
    }
}
