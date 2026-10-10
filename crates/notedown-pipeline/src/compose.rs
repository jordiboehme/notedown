//! Building the new page text from the change region (spec 3.3).
//!
//! Untouched handwriting keeps its text and id exactly; only the place that
//! changed takes the new reading. Box comparisons happen in page pixels: every
//! block box (old and new, both on Chandra's 0..1000 grid) goes through
//! `block_rect` first. The output blocks keep their grid boxes.
//!
//! Decisions where spec 3.3 leaves room (also in the task 5 report):
//! - Touch test: `Region::touches`, edges count, so a dot or a straight line
//!   (a box of zero width or height) touches the block it lies on.
//! - Ownership: every new block reads at most one old block, its owner: the
//!   old block its box overlaps by the largest area (ties: nearer center,
//!   then lower index). Steps 2, 3 and 7 use this one relation.
//! - Step 2: the readings a touched block owns are taken, also one that does
//!   not touch the region. Without this, erasing the end of a line shrinks
//!   the new box away from the region and the rest of the line would be lost.
//! - Step 3 (ruling, step 1 wins over a literal drop): the owner of a taken
//!   reading, and an untouched block that a taken reading covers by at least
//!   half, are not dropped but join the step-4 match pool as touched. A
//!   same-label match keeps the id, and on equal normalized text its old
//!   bytes, so box jitter costs nothing. A block the region does not touch
//!   keeps its old block (bytes, id, box) whenever its matched reading has a
//!   normalized similarity of at least 0.8, also when it joined through step
//!   3, ownership or the net; below 0.8 it takes the new reading and counts a
//!   `region_missed` (once). Three exceptions take the matched reading's
//!   text instead of the old bytes, compared on words (markdown hard breaks,
//!   punctuation and case stripped, words read in pieces joined): the
//!   reading holds new ink (a line more than half of whose words the old
//!   text holds less often than the reading, or at least 3 more words than
//!   the old text), because the old bytes would lose it; a line break at
//!   another word or a misread word inside a line is no new ink; words of the
//!   old text that the reading lacks turned up in another taken reading that
//!   overlaps the block (at least 2 of them, or the word of a one-word old
//!   line), because the old bytes would show that line twice; and a merge
//!   match, because the reading carries the text of a block that does not
//!   survive. None of these depends on the half-box split rule: a reading
//!   that lies half inside the block but holds none of its words costs it
//!   nothing. Known limits, accepted: a re-read line whose words are misread
//!   in the other reading can appear twice; a short common word in an
//!   overlapping taken reading can count as moved, so the block takes its
//!   close reading without need; a misread one-word line counts as new ink.
//!   Only an unmatched block is removed and goes
//!   into `replaces`. Owning a taken reading also pulls in the paragraph
//!   whose last line was re-read with new ink below it, so the word rule
//!   above can see both readings and that line does not appear twice.
//! - Step 4: labels `Diagram`, `Figure` and `Image` count as one label (the
//!   converter renders all three as a diagram). Candidates must overlap. A
//!   merge (a new block that covers two or more touched old blocks of its
//!   label) and a split (an old block with two or more taken new blocks of
//!   its label at least half inside it) do not match, unless the pair's
//!   similarity is at least 0.8 (then it is an edit plus a neighbour). Pairs
//!   are assigned greedily by overlap area, then similarity.
//! - Step 5: blocks are whole units here, so a taken diagram or table always
//!   brings its whole new text; nothing is cut or stitched.
//! - Step 6: `replaces` lists the old blocks that did not survive and lie at
//!   least half inside the new block; for a split parent it is enough that the
//!   new block lies at least half inside the old one (a split with a gap leaves
//!   less than half of the old box in each piece). Matched blocks record
//!   `replaces` too (a merge that matched one side replaces the other).
//! - Step 7: an untouched block is compared with the text of the readings it
//!   owns, in position order. Owning none counts as similarity 0, also when
//!   a neighbour's reading overlaps it: the net fires, the block goes (it
//!   may still appear in a `replaces` per step 6) and no reading is taken
//!   for it. A block that fails becomes touched, its readings are taken, and
//!   the normal matching runs (so it can keep its id). The net fires for one
//!   block at a time and steps 2-3 run again before the next, so a block
//!   that the taken reading covers joins through step 3 and is not counted.
//! - Carried-over and matched blocks drop their old `replaces`: it records the
//!   step from one version to the next; the note state keeps the history.
//! - A matched block whose text is the same after normalization keeps its old
//!   text bytes. When the whole page is unchanged, `blocks` are the old blocks.

use std::collections::HashMap;

use crate::model::Block;
use crate::region::{Rect, Region, block_rect};
use crate::text::{same_text, similarity};

/// Untouched text is kept only when its new reading is at least this close.
pub const SAFETY_NET: f32 = 0.8;
/// The share of a box that must lie inside another to count as covered.
const COVER: f32 = 0.5;

/// A block of the new OCR reading, before it has an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlock {
    pub label: String,
    /// `[left, top, right, bottom]` on Chandra's 0..1000 grid.
    pub bbox: [u32; 4],
    pub text: String,
}

/// The new page text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Composed {
    /// Ordered top to bottom, then left to right.
    pub blocks: Vec<Block>,
    /// Same ids with the same normalized text, nothing added or removed.
    pub unchanged: bool,
    /// Untouched blocks the safety net gave the new reading.
    pub region_missed: u32,
    /// Per old id that did not survive, the ids that replace it.
    pub replaced: Vec<(String, Vec<String>)>,
}

/// One label for the three labels the converter renders as a diagram.
fn label_class(label: &str) -> &str {
    match label {
        "Diagram" | "Figure" | "Image" => "Diagram",
        other => other,
    }
}

fn overlap_area(a: &Rect, b: &Rect) -> f32 {
    a.overlap_share_of(b) * b.area()
}

/// The old block a new block reads: the one its box overlaps by the largest
/// area; ties go to the nearer center, then to the lower index. `None` when
/// it overlaps no old block.
fn owner_of(new: &Rect, old: &[Rect]) -> Option<usize> {
    let center = |r: &Rect| ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
    let (nx, ny) = center(new);
    let distance = |r: &Rect| {
        let (x, y) = center(r);
        (x - nx).hypot(y - ny)
    };
    let mut best: Option<(usize, f32, f32)> = None;
    for (i, r) in old.iter().enumerate() {
        let area = overlap_area(r, new);
        if area <= 0.0 {
            continue;
        }
        let d = distance(r);
        let better = match best {
            None => true,
            Some((_, a, bd)) => area > a || (area == a && d < bd),
        };
        if better {
            best = Some((i, area, d));
        }
    }
    best.map(|(i, _, _)| i)
}

/// This many more words than the old text (split words joined) is new ink.
const NEW_WORDS: usize = 3;
/// This many old words found in another reading mean a line moved there.
const MOVED_WORDS: usize = 2;
/// At most this many adjacent pieces are joined into one split word.
const MAX_PIECES: usize = 3;

/// The words of a line for the keep-old check: without the markdown hard
/// break (a trailing `\`), punctuation and case.
fn words(line: &str) -> Vec<String> {
    let line = line.trim_end();
    let line = line.strip_suffix('\\').unwrap_or(line);
    line.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The non-empty lines of a text as words.
fn line_words(text: &str) -> Vec<Vec<String>> {
    text.lines().map(words).filter(|w| !w.is_empty()).collect()
}

/// Takes `word` out of `pool` once; false when it is not there.
fn take_word(pool: &mut Vec<String>, word: &str) -> bool {
    pool.iter()
        .position(|w| w == word)
        .map(|p| pool.swap_remove(p))
        .is_some()
}

/// How often each word occurs.
fn counts<'a>(words: impl Iterator<Item = &'a String>) -> HashMap<&'a str, usize> {
    let mut out: HashMap<&str, usize> = HashMap::new();
    for w in words {
        *out.entry(w.as_str()).or_default() += 1;
    }
    out
}

/// The reading lines with split words joined: adjacent pieces (`an na`)
/// whose concatenation is a word the old text holds more often than the
/// reading become that word. Longer joins go first.
fn join_splits(lines: Vec<Vec<String>>, old: &HashMap<&str, usize>) -> Vec<Vec<String>> {
    let mut read: HashMap<String, usize> = HashMap::new();
    for w in lines.iter().flatten() {
        *read.entry(w.clone()).or_default() += 1;
    }
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut joined_line = Vec::with_capacity(line.len());
        let mut i = 0;
        while i < line.len() {
            let join = (2..=MAX_PIECES.min(line.len() - i)).rev().find_map(|k| {
                let joined: String = line[i..i + k].concat();
                let lacks = old.get(joined.as_str()).copied().unwrap_or(0)
                    > read.get(&joined).copied().unwrap_or(0);
                lacks.then_some((k, joined))
            });
            match join {
                Some((k, joined)) => {
                    for piece in &line[i..i + k] {
                        if let Some(c) = read.get_mut(piece) {
                            *c = c.saturating_sub(1);
                        }
                    }
                    *read.entry(joined.clone()).or_default() += 1;
                    joined_line.push(joined);
                    i += k;
                }
                None => {
                    joined_line.push(line[i].clone());
                    i += 1;
                }
            }
        }
        out.push(joined_line);
    }
    out
}

/// Whether the matched reading of a block outside the region wins over the
/// old bytes, compared on words with split words joined (`join_splits`): it
/// holds new ink, or words of the old text that it lacks turned up in
/// `others`, the other taken readings over the block (at least
/// `MOVED_WORDS` of them, or the word of a one-word old line).
///
/// New ink is a reading line most of whose words (more than half) the old
/// text cannot explain, or `NEW_WORDS` more words than the old text. A word
/// counts as unexplained when the reading holds it more often than the old
/// text, wherever it stands, so a new line above the re-read lines is found
/// like one below them. A line break at another word, a misread word inside
/// a line and words read in pieces are no new ink.
fn reading_wins(old: &str, reading: &str, others: &[&str]) -> bool {
    let old_lines = line_words(old);
    let old_words: Vec<&str> = old_lines.iter().flatten().map(String::as_str).collect();
    let old_count = counts(old_lines.iter().flatten());
    let new_lines = join_splits(line_words(reading), &old_count);
    let mut new_words: Vec<String> = new_lines.iter().flatten().cloned().collect();
    if new_words.len() >= old_words.len() + NEW_WORDS {
        return true;
    }
    let new_count = counts(new_lines.iter().flatten());
    let surplus = |w: &str| new_count[w] > old_count.get(w).copied().unwrap_or(0);
    let new_line = |line: &Vec<String>| line.iter().filter(|w| surplus(w)).count() * 2 > line.len();
    if new_lines.iter().any(new_line) {
        return true;
    }
    let mut elsewhere: Vec<String> = others
        .iter()
        .flat_map(|t| line_words(t))
        .flatten()
        .collect();
    let found: Vec<&str> = old_words
        .iter()
        .filter(|w| !take_word(&mut new_words, w))
        .filter(|w| take_word(&mut elsewhere, w))
        .copied()
        .collect();
    found.len() >= MOVED_WORDS
        || old_lines
            .iter()
            .any(|l| l.len() == 1 && found.contains(&l[0].as_str()))
}

fn by_position(bbox: [u32; 4]) -> (u32, u32) {
    (bbox[1], bbox[0])
}

/// Builds the new page text from the old blocks, the new reading and the
/// change region (spec 3.3, steps 1-7). `page` is the rendered image width,
/// height and landscape flag; `next_id` is the next free block number of the
/// note and is advanced for every new id.
pub fn compose(
    old: &[Block],
    new: &[NewBlock],
    region: &Region,
    page: (u32, u32, bool),
    next_id: &mut u64,
) -> Composed {
    let (width, height, landscape) = page;
    let old_r: Vec<Rect> = old
        .iter()
        .map(|b| block_rect(b.bbox, width, height, landscape))
        .collect();
    let new_r: Vec<Rect> = new
        .iter()
        .map(|b| block_rect(b.bbox, width, height, landscape))
        .collect();

    // Steps 1 and 2: what the change touches.
    let in_region: Vec<bool> = old_r.iter().map(|r| region.touches(r)).collect();
    let mut touched = in_region.clone();
    let mut netted = vec![false; old.len()];
    let mut taken: Vec<bool> = new_r.iter().map(|r| region.touches(r)).collect();
    let owner: Vec<Option<usize>> = new_r.iter().map(|r| owner_of(r, &old_r)).collect();
    let mut region_missed = 0;
    loop {
        let mut changed = false;
        // Step 2: the readings of a touched block are taken (also one that
        // shrank away from the region, like the rest of a line after its
        // last word was erased).
        for j in 0..new.len() {
            if !taken[j] && owner[j].is_some_and(|i| touched[i]) {
                taken[j] = true;
                changed = true;
            }
        }
        // Step 3, no double text: the owner of a taken reading, and an old
        // block that a taken reading covers by half, join the match pool
        // (step 1 wins: box jitter must not cost them their id).
        for i in 0..old.len() {
            if !touched[i]
                && (0..new.len()).any(|j| {
                    taken[j]
                        && (owner[j] == Some(i) || new_r[j].overlap_share_of(&old_r[i]) >= COVER)
                })
            {
                touched[i] = true;
                changed = true;
            }
        }
        if changed {
            continue;
        }
        // Step 7: the safety net, one block at a time, so a block that the
        // taken reading covers joins through step 3 and is not counted.
        for i in 0..old.len() {
            if touched[i] {
                continue;
            }
            let mut own: Vec<usize> = (0..new.len()).filter(|&j| owner[j] == Some(i)).collect();
            own.sort_by_key(|&j| by_position(new[j].bbox));
            let reading: Vec<&str> = own.iter().map(|&j| new[j].text.as_str()).collect();
            if similarity(&old[i].text, &reading.join("\n")) < SAFETY_NET {
                touched[i] = true;
                netted[i] = true;
                region_missed += 1;
                for j in own {
                    taken[j] = true;
                }
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }

    // Step 4: ids inside the touched area.
    let same_label = |i: usize, j: usize| label_class(&old[i].label) == label_class(&new[j].label);
    let merge: Vec<bool> = (0..new.len())
        .map(|j| {
            taken[j]
                && (0..old.len())
                    .filter(|&i| {
                        touched[i]
                            && same_label(i, j)
                            && new_r[j].overlap_share_of(&old_r[i]) >= COVER
                    })
                    .count()
                    >= 2
        })
        .collect();
    let split: Vec<bool> = (0..old.len())
        .map(|i| {
            touched[i]
                && (0..new.len())
                    .filter(|&j| {
                        taken[j]
                            && same_label(i, j)
                            && old_r[i].overlap_share_of(&new_r[j]) >= COVER
                    })
                    .count()
                    >= 2
        })
        .collect();
    let mut pairs: Vec<(usize, usize, f32, f32)> = Vec::new();
    for i in (0..old.len()).filter(|&i| touched[i]) {
        for j in (0..new.len()).filter(|&j| taken[j]) {
            if !same_label(i, j) || !old_r[i].intersects(&new_r[j]) {
                continue;
            }
            let sim = similarity(&old[i].text, &new[j].text);
            if (merge[j] || split[i]) && sim < SAFETY_NET {
                continue;
            }
            pairs.push((i, j, overlap_area(&old_r[i], &new_r[j]), sim));
        }
    }
    pairs.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.3.total_cmp(&a.3)));
    let mut match_of_new: Vec<Option<usize>> = vec![None; new.len()];
    let mut matched_old = vec![false; old.len()];
    for (i, j, _, _) in pairs {
        if !matched_old[i] && match_of_new[j].is_none() {
            matched_old[i] = true;
            match_of_new[j] = Some(i);
        }
    }

    // Who survives: untouched blocks and matched ones.
    let survives: Vec<bool> = (0..old.len())
        .map(|i| !touched[i] || matched_old[i])
        .collect();

    // The page: kept blocks verbatim, taken blocks with steps 5 and 6.
    let mut entries: Vec<(Option<String>, Block)> = Vec::new();
    for (i, b) in old.iter().enumerate() {
        if !touched[i] {
            entries.push((
                Some(b.id.clone()),
                Block {
                    replaces: vec![],
                    ..b.clone()
                },
            ));
        }
    }
    for (j, n) in new.iter().enumerate() {
        if !taken[j] {
            continue;
        }
        let replaces: Vec<String> = (0..old.len())
            .filter(|&i| {
                !survives[i]
                    && (new_r[j].overlap_share_of(&old_r[i]) >= COVER
                        || (split[i] && old_r[i].overlap_share_of(&new_r[j]) >= COVER))
            })
            .map(|i| old[i].id.clone())
            .collect();
        let Some(i) = match_of_new[j] else {
            entries.push((
                None,
                Block {
                    id: String::new(),
                    label: n.label.clone(),
                    bbox: n.bbox,
                    text: n.text.clone(),
                    replaces,
                },
            ));
            continue;
        };
        let sim = similarity(&old[i].text, &n.text);
        // A block outside the region keeps its bytes when its reading is
        // close, unless the reading holds new ink or words of the old text
        // moved to another taken reading over its box (`reading_wins`), or
        // the reading is a merge that carries the text of a block that does
        // not survive (that text would be lost).
        let others: Vec<&str> = (0..new.len())
            .filter(|&k| k != j && taken[k] && old_r[i].intersects(&new_r[k]))
            .map(|k| new[k].text.as_str())
            .collect();
        let keep_old = same_text(&old[i].text, &n.text)
            || (!in_region[i]
                && sim >= SAFETY_NET
                && !merge[j]
                && !reading_wins(&old[i].text, &n.text, &others));
        if !in_region[i] && !netted[i] && sim < SAFETY_NET {
            region_missed += 1;
        }
        let block = if keep_old && !in_region[i] {
            Block {
                replaces,
                ..old[i].clone()
            }
        } else {
            Block {
                id: String::new(),
                label: n.label.clone(),
                bbox: n.bbox,
                text: if keep_old {
                    old[i].text.clone()
                } else {
                    n.text.clone()
                },
                replaces,
            }
        };
        entries.push((Some(old[i].id.clone()), block));
    }
    entries.sort_by_key(|(_, b)| by_position(b.bbox));
    let blocks: Vec<Block> = entries
        .into_iter()
        .map(|(id, mut b)| {
            b.id = id.unwrap_or_else(|| {
                let id = format!("b{next_id}");
                *next_id += 1;
                id
            });
            b
        })
        .collect();

    let replaced: Vec<(String, Vec<String>)> = old
        .iter()
        .enumerate()
        .filter(|&(i, _)| !survives[i])
        .filter_map(|(_, b)| {
            let next: Vec<String> = blocks
                .iter()
                .filter(|n| n.replaces.contains(&b.id))
                .map(|n| n.id.clone())
                .collect();
            (!next.is_empty()).then(|| (b.id.clone(), next))
        })
        .collect();

    let unchanged = blocks.len() == old.len()
        && blocks.iter().all(|n| {
            old.iter()
                .any(|o| o.id == n.id && same_text(&o.text, &n.text))
        });
    Composed {
        blocks: if unchanged { old.to_vec() } else { blocks },
        unchanged,
        region_missed,
        replaced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::Rect;
    use crate::text::similarity;

    /// Not 1000 x 1000, so a missing grid conversion fails a test.
    const PAGE: (u32, u32, bool) = (2000, 3000, false);

    fn old(id: &str, label: &str, bbox: [u32; 4], text: &str) -> Block {
        Block {
            id: id.into(),
            label: label.into(),
            bbox,
            text: text.into(),
            replaces: vec![],
        }
    }
    fn new(label: &str, bbox: [u32; 4], text: &str) -> NewBlock {
        NewBlock {
            label: label.into(),
            bbox,
            text: text.into(),
        }
    }
    /// A change region from grid boxes, in page pixels.
    fn region(grid: &[[u32; 4]]) -> Region {
        Region::Parts(
            grid.iter()
                .map(|g| Rect {
                    x0: g[0] as f32 * 2.0,
                    y0: g[1] as f32 * 3.0,
                    x1: g[2] as f32 * 2.0,
                    y1: g[3] as f32 * 3.0,
                })
                .collect(),
        )
    }
    fn find<'a>(c: &'a Composed, id: &str) -> &'a Block {
        c.blocks
            .iter()
            .find(|b| b.id == id)
            .unwrap_or_else(|| panic!("no block {id} in {:?}", c.blocks))
    }

    #[test]
    fn untouched_blocks_keep_text_and_id_against_a_noisy_reading() {
        let first = "Meeting with Anna on Friday about the budget.";
        let noisy = "Meeting with Anna on Fridav about  the budget";
        assert!(similarity(first, noisy) > 0.8 && first != noisy);
        let olds = [
            old("b1", "Text", [100, 100, 900, 300], first),
            old("b2", "Text", [100, 400, 900, 600], "Buy milk and eggs"),
        ];
        let news = [
            new("Text", [100, 100, 900, 300], noisy),
            new("Text", [100, 400, 900, 600], "Buy milk, eggs and bread"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[600, 450, 800, 550]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(c.blocks[1].id, "b2");
        assert_eq!(c.blocks[1].text, "Buy milk, eggs and bread");
        assert!(!c.unchanged);
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
        assert_eq!(next, 3);
    }

    #[test]
    fn a_different_split_of_an_untouched_paragraph_changes_nothing() {
        let olds = [
            old(
                "b1",
                "Text",
                [100, 100, 900, 500],
                "Line one of the text.\nLine two of the text.",
            ),
            old("b2", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 100, 900, 300], "Line one of the text."),
            new("Text", [100, 300, 900, 500], "Line two of the text."),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks, olds);
        assert!(c.unchanged);
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
    }

    #[test]
    fn the_safety_net_takes_the_new_reading() {
        let stale = "Meeting with Anna";
        let fresh = "Meet the bank team";
        assert!((similarity(stale, fresh) - 0.3).abs() < 0.05);
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], stale),
            old("b2", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 100, 900, 200], fresh),
            new("Text", [100, 600, 900, 700], "Todo list"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.region_missed, 1);
        assert_eq!(find(&c, "b1").text, fresh);
        assert_eq!(find(&c, "b2").text, "Todo list");
        assert_eq!(c.blocks.len(), 2);
        assert!(!c.unchanged);
    }

    #[test]
    fn an_edited_block_keeps_its_id() {
        let olds = [old("b1", "Text", [100, 100, 900, 200], "Call Bob today")];
        let news = [new("Text", [100, 100, 950, 210], "Call Bob tomorrow")];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[700, 120, 940, 190]]),
            PAGE,
            &mut next,
        );
        assert_eq!(
            c.blocks,
            vec![Block {
                id: "b1".into(),
                label: "Text".into(),
                bbox: [100, 100, 950, 210],
                text: "Call Bob tomorrow".into(),
                replaces: vec![],
            }]
        );
        assert!(!c.unchanged);
        assert!(c.replaced.is_empty());
        assert_eq!(next, 2);
    }

    #[test]
    fn a_new_block_gets_the_next_id() {
        let olds = [old("b1", "Text", [100, 100, 900, 200], "Call Bob")];
        let news = [
            new("Text", [100, 100, 900, 200], "Call Bob"),
            new("Text", [100, 500, 600, 600], "Pay rent"),
        ];
        let mut next = 7;
        let c = compose(
            &olds,
            &news,
            &region(&[[120, 520, 580, 580]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(c.blocks[1].id, "b7");
        assert_eq!(c.blocks[1].text, "Pay rent");
        assert!(c.blocks[1].replaces.is_empty());
        assert_eq!(next, 8);
        assert!(!c.unchanged);
    }

    #[test]
    fn a_touched_diagram_takes_the_new_mermaid_whole() {
        let before = "```mermaid\ngraph TD\nA[Start] --> B[End]\n```";
        let after = "```mermaid\ngraph TD\nA[Start] --> B[End]\nB --> C[Done]\n```";
        let olds = [
            old("b1", "Text", [100, 50, 900, 100], "Flow"),
            old("b2", "Diagram", [100, 150, 900, 600], before),
        ];
        let news = [
            new("Text", [100, 50, 900, 100], "Flow"),
            new("Diagram", [100, 150, 900, 700], after),
        ];
        let mut next = 3;
        // Only a corner of the diagram changed.
        let c = compose(
            &olds,
            &news,
            &region(&[[700, 580, 880, 690]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(c.blocks[1].id, "b2");
        assert_eq!(c.blocks[1].label, "Diagram");
        assert_eq!(c.blocks[1].text, after);
        assert_eq!(next, 3);
    }

    #[test]
    fn text_that_becomes_a_diagram_is_replaced() {
        let olds = [
            old("b1", "Text", [100, 50, 900, 100], "Plan"),
            old("b3", "Text", [200, 200, 600, 300], "Start then End"),
        ];
        let mermaid = "```mermaid\ngraph LR\nA[Start] --> B[End]\n```";
        let news = [
            new("Text", [100, 50, 900, 100], "Plan"),
            new("Diagram", [150, 150, 800, 450], mermaid),
        ];
        let mut next = 4;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 150, 800, 450]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(
            c.blocks[1],
            Block {
                id: "b4".into(),
                label: "Diagram".into(),
                bbox: [150, 150, 800, 450],
                text: mermaid.into(),
                replaces: vec!["b3".into()],
            }
        );
        assert_eq!(c.replaced, vec![("b3".to_string(), vec!["b4".to_string()])]);
        assert_eq!(next, 5);
    }

    #[test]
    fn two_blocks_merged() {
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "first part"),
            old("b2", "Text", [100, 200, 900, 300], "second part"),
        ];
        let news = [new(
            "Text",
            [100, 100, 900, 300],
            "first part and second part",
        )];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[850, 150, 900, 250]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 1);
        assert_eq!(c.blocks[0].id, "b3");
        assert_eq!(c.blocks[0].replaces, vec!["b1", "b2"]);
        assert_eq!(
            c.replaced,
            vec![
                ("b1".to_string(), vec!["b3".to_string()]),
                ("b2".to_string(), vec!["b3".to_string()]),
            ]
        );
        assert_eq!(next, 4);
    }

    #[test]
    fn one_block_split() {
        let olds = [old(
            "b1",
            "Text",
            [100, 100, 900, 500],
            "Alpha beta gamma delta",
        )];
        // A gap between the pieces: the old box lies less than half inside each.
        let news = [
            new("Text", [100, 100, 900, 280], "Alpha beta"),
            new("Text", [100, 340, 900, 500], "gamma delta"),
        ];
        let mut next = 5;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 250, 900, 360]]),
            PAGE,
            &mut next,
        );
        let ids: Vec<&str> = c.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["b5", "b6"]);
        assert_eq!(c.blocks[0].replaces, vec!["b1"]);
        assert_eq!(c.blocks[1].replaces, vec!["b1"]);
        assert_eq!(
            c.replaced,
            vec![("b1".to_string(), vec!["b5".to_string(), "b6".to_string()])]
        );
        assert_eq!(next, 7);
    }

    #[test]
    fn an_erased_end_keeps_the_rest_of_the_block() {
        // The new box ends before the erased word, so it does not touch the
        // region; it lies inside the touched old box and is taken all the same.
        let olds = [old("b1", "Text", [100, 100, 900, 200], "Meeting on Friday")];
        let news = [new("Text", [100, 100, 600, 200], "Meeting on")];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[650, 110, 880, 190]]),
            PAGE,
            &mut next,
        );
        let got: Vec<(&str, &str)> = c
            .blocks
            .iter()
            .map(|b| (b.id.as_str(), b.text.as_str()))
            .collect();
        assert_eq!(got, [("b1", "Meeting on")]);
    }

    #[test]
    fn an_underline_on_the_edge_touches_the_block() {
        // The stroke box of a straight underline has zero height and lies on
        // the bottom edge of the block box.
        let olds = [old("b1", "Text", [100, 100, 900, 200], "Call Bob")];
        let news = [new("Text", [100, 100, 900, 200], "Call Bob today")];
        let mut next = 2;
        let underline = Region::Parts(vec![Rect {
            x0: 300.0,
            y0: 600.0,
            x1: 1500.0,
            y1: 600.0,
        }]);
        let c = compose(&olds, &news, &underline, PAGE, &mut next);
        assert_eq!(c.region_missed, 0);
        assert_eq!(c.blocks[0].id, "b1");
        assert_eq!(c.blocks[0].text, "Call Bob today");
    }

    #[test]
    fn unchanged_text_is_reported_unchanged() {
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Call Bob"),
            old("b2", "Text", [100, 300, 900, 400], "Pay  the rent"),
        ];
        let news = [
            new("Text", [100, 100, 900, 200], "Call Bob"),
            new("Text", [110, 300, 890, 410], "Pay the\nrent "),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[200, 320, 300, 380]]),
            PAGE,
            &mut next,
        );
        assert!(c.unchanged);
        assert_eq!(c.blocks, olds);
        assert!(c.replaced.is_empty());
        assert_eq!(next, 3);
    }

    #[test]
    fn region_whole_reads_everything() {
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Hello"),
            old("b2", "Text", [100, 300, 900, 400], "Gone"),
        ];
        let news = [
            new("Text", [100, 100, 900, 200], "Hallo world"),
            new("Text", [100, 600, 900, 700], "Fresh line"),
        ];
        let mut next = 3;
        let c = compose(&olds, &news, &Region::Whole, PAGE, &mut next);
        let got: Vec<(&str, &str)> = c
            .blocks
            .iter()
            .map(|b| (b.id.as_str(), b.text.as_str()))
            .collect();
        assert_eq!(got, [("b1", "Hallo world"), ("b3", "Fresh line")]);
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
        assert!(!c.unchanged);
        assert_eq!(next, 4);
    }

    fn texts(c: &Composed) -> Vec<(&str, &str)> {
        c.blocks
            .iter()
            .map(|b| (b.id.as_str(), b.text.as_str()))
            .collect()
    }

    #[test]
    fn a_shifted_reading_with_the_same_text_keeps_the_block() {
        // The new box of b1 overlaps only 40% of the old one: the half rule
        // finds no reading there, any overlap does.
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Call Bob"),
            old("b2", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 160, 900, 260], "Call Bob"),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.region_missed, 0);
        assert!(c.unchanged);
        assert_eq!(c.blocks, olds);
    }

    #[test]
    fn the_net_takes_a_shifted_reading() {
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Meeting with Anna"),
            old("b2", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 160, 900, 260], "Meet the bank team"),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.region_missed, 1);
        assert_eq!(texts(&c), [("b1", "Meet the bank team"), ("b2", "Todo")]);
    }

    #[test]
    fn jitter_does_not_cost_a_neighbour_its_id() {
        // A stroke near the top of b2 touches the slightly grown new box of
        // b1, whose text is the same.
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Meeting  with Anna"),
            old("b2", "Text", [100, 250, 900, 350], "Call Bob"),
        ];
        let news = [
            new("Text", [100, 100, 900, 240], "Meeting with Anna"),
            new("Text", [100, 250, 900, 350], "Call Bob"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[200, 230, 400, 260]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks, olds);
        assert!(c.unchanged);
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
        assert_eq!(next, 3);
    }

    #[test]
    fn a_reread_last_line_does_not_appear_twice() {
        // The last line of b1 is read again together with new ink below it.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [
            new(
                "Text",
                [100, 100, 900, 580],
                "Line one.\nLine two.\nLine three.\nLine four.\nLine five.",
            ),
            new("Text", [100, 590, 900, 780], "Line six.\nNew line."),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 780]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("Line six.").count(), 1);
        assert_eq!(
            texts(&c),
            [
                (
                    "b1",
                    "Line one.\nLine two.\nLine three.\nLine four.\nLine five."
                ),
                ("b2", "Line six.\nNew line."),
            ]
        );
        assert!(c.replaced.is_empty());
    }

    #[test]
    fn an_untouched_block_covered_by_a_new_diagram_is_replaced() {
        let mermaid = "```mermaid\ngraph LR\nA --> B\n```";
        let olds = [
            old("b1", "Text", [100, 100, 500, 200], "A to B"),
            old("b2", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Diagram", [80, 80, 700, 400], mermaid),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 3;
        // Only the far corner of the diagram is new ink; b1 is untouched.
        let c = compose(
            &olds,
            &news,
            &region(&[[550, 300, 690, 390]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b3", mermaid), ("b2", "Todo")]);
        assert_eq!(c.blocks[0].replaces, vec!["b1"]);
        assert_eq!(c.replaced, vec![("b1".to_string(), vec!["b3".to_string()])]);
        assert_eq!(c.region_missed, 0);
    }

    #[test]
    fn the_net_cascades_into_a_covered_block() {
        // The net fires for b1; the reading it takes also covers b2, which
        // then joins through step 3 and is not counted as missed.
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Meeting with Anna"),
            old("b2", "Text", [100, 200, 900, 300], "Call Bob"),
            old("b3", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 100, 900, 300], "Meet the bank team\nCall Bob"),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 4;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.region_missed, 1);
        assert_eq!(
            texts(&c),
            [("b4", "Meet the bank team\nCall Bob"), ("b3", "Todo")]
        );
        assert_eq!(c.blocks[0].replaces, vec!["b1", "b2"]);
        assert_eq!(next, 5);
    }

    #[test]
    fn a_merge_that_reads_like_one_side_keeps_its_id() {
        let long = "Meeting with Anna on Friday about the budget for next year";
        let merged = "Meeting with Anna on Friday about the budget for next year ok";
        assert!(similarity(long, merged) >= SAFETY_NET);
        let olds = [
            old("b1", "Text", [100, 100, 900, 300], long),
            old("b2", "Text", [100, 300, 900, 330], "ok"),
        ];
        let news = [new("Text", [100, 100, 900, 330], merged)];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[850, 290, 900, 320]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b1", merged)]);
        assert_eq!(c.blocks[0].replaces, vec!["b2"]);
        assert_eq!(c.replaced, vec![("b2".to_string(), vec!["b1".to_string()])]);
        assert_eq!(next, 3);
    }

    #[test]
    fn a_split_with_one_close_piece_keeps_its_id() {
        let whole = "Alpha beta gamma delta epsilon zeta";
        let piece = "Alpha beta gamma delta epsilon";
        assert!(similarity(whole, piece) >= SAFETY_NET);
        let olds = [old("b1", "Text", [100, 100, 900, 500], whole)];
        let news = [
            new("Text", [100, 100, 900, 420], piece),
            new("Text", [100, 430, 900, 500], "zeta"),
        ];
        let mut next = 5;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 400, 900, 450]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b1", piece), ("b5", "zeta")]);
        assert!(c.blocks[1].replaces.is_empty());
        assert!(c.replaced.is_empty());
        assert_eq!(next, 6);
    }

    #[test]
    fn a_taken_reading_of_an_untouched_block_is_not_emitted_twice() {
        // The region touches the shifted new box of b1, not b1 itself.
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Call Bob"),
            old("b2", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 160, 900, 260], "Call Bob"),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 240, 300, 255]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b1", "Call Bob"), ("b2", "Todo")]);
        assert!(c.unchanged);
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
    }

    #[test]
    fn a_neighbour_without_a_reading_does_not_pull_in_a_noisy_one() {
        let first = "Meeting with Anna on Friday about the budget.";
        let noisy = "Meeting with Anna on Fridav about  the budget";
        assert!(similarity(first, noisy) >= SAFETY_NET);
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], first),
            old("b2", "Text", [100, 200, 900, 215], "ok"),
            old("b3", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 100, 900, 205], noisy),
            new("Text", [100, 600, 900, 700], "Todo list"),
        ];
        let mut next = 4;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        // b2 owns no reading: the reading has nothing there, so it goes and
        // counts as missed; b1 keeps its bytes, its noisy reading is not taken.
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(texts(&c)[1], ("b3", "Todo list"));
        assert_eq!(c.region_missed, 1);
        assert!(c.replaced.is_empty());
    }

    #[test]
    fn a_block_whose_reading_went_to_a_neighbour_is_not_kept_stale() {
        // Probe 1: the only reading near b2 is b1's, grazing b2.
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Line one is here"),
            old("b2", "Text", [100, 200, 900, 300], "Buy milk and eggs"),
            old("b3", "Text", [100, 600, 900, 700], "Todo"),
        ];
        let news = [
            new("Text", [100, 100, 900, 205], "Line one is here"),
            new("Text", [100, 600, 900, 700], "Todo"),
        ];
        let mut next = 4;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 620, 300, 680]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b1", "Line one is here"), ("b3", "Todo")]);
        assert_eq!(c.region_missed, 1);
        assert!(!c.unchanged);
    }

    #[test]
    fn a_reading_between_two_blocks_belongs_to_one() {
        // Probe 5: one reading overlaps b1 and b2 equally; the tie gives it
        // to b1, and b2 is not kept stale.
        let olds = [
            old("b1", "Text", [100, 100, 900, 200], "Line one is here"),
            old("b2", "Text", [100, 200, 900, 300], "Buy milk and eggs"),
        ];
        let news = [new("Text", [100, 150, 900, 250], "Line one is here")];
        let mut next = 3;
        let c = compose(&olds, &news, &Region::Empty, PAGE, &mut next);
        assert_eq!(c.blocks, vec![olds[0].clone()]);
        assert_eq!(c.region_missed, 1);
        assert!(!c.unchanged);
    }

    #[test]
    fn a_taken_reading_over_part_of_a_neighbour_does_not_repeat_it() {
        // Probe 4: the taken reading of b1 also holds b2's text but covers
        // only 40% of b2's box.
        let olds = [
            old("b1", "Text", [100, 100, 900, 300], "Alpha beta gamma"),
            old("b2", "Text", [100, 300, 900, 400], "Delta epsilon"),
        ];
        let news = [new(
            "Text",
            [100, 100, 900, 340],
            "Alpha beta gamma X\nDelta epsilon",
        )];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[200, 150, 400, 250]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("Delta epsilon").count(), 1);
        assert_eq!(texts(&c), [("b1", "Alpha beta gamma X\nDelta epsilon")]);
        assert_eq!(c.region_missed, 1);
    }

    #[test]
    fn a_new_line_grazing_a_block_keeps_its_bytes() {
        // Probe 2: the new line overlaps b1 by 5 of 100 rows, so b1 owns it
        // and joins the match pool; its own reading is close, so it keeps
        // its bytes.
        let first = "Meeting with Anna on Friday about the budget.";
        let noisy = "Meeting with Anna on Fridav about  the budget";
        assert!(similarity(first, noisy) >= SAFETY_NET);
        let olds = [old("b1", "Text", [100, 100, 900, 200], first)];
        let news = [
            new("Text", [100, 100, 900, 198], noisy),
            new("Text", [100, 195, 900, 260], "Call the bank"),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 210, 800, 250]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(texts(&c)[1], ("b2", "Call the bank"));
        assert!(c.blocks[1].replaces.is_empty());
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
    }

    #[test]
    fn a_reread_last_line_under_half_inside_its_paragraph_does_not_appear_twice() {
        // The reading that holds the re-read last line lies only a third
        // inside b1, so b1 is no split; b1 must still give up its old bytes,
        // because "Line six." lives in the other reading.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        let first_five = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.";
        assert!(similarity(paragraph, first_five) >= SAFETY_NET);
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [
            new("Text", [100, 100, 900, 590], first_five),
            new(
                "Text",
                [100, 600, 900, 900],
                "Line six.\nNew line one.\nNew line two.",
            ),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 880]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("Line six.").count(), 1);
        assert_eq!(
            texts(&c),
            [
                ("b1", first_five),
                ("b2", "Line six.\nNew line one.\nNew line two."),
            ]
        );
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
        assert_eq!(next, 3);
    }

    #[test]
    fn a_reading_half_inside_an_untouched_paragraph_does_not_cost_its_bytes() {
        // A new reading lies exactly half inside b1 (a split by the box
        // rule), but holds none of b1's lines; b1's own reading is close, so
        // b1 keeps its bytes.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        let noisy = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six";
        assert!(similarity(paragraph, noisy) >= SAFETY_NET && paragraph != noisy);
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [
            new("Text", [100, 100, 900, 690], noisy),
            new("Text", [100, 500, 900, 900], "New line one.\nNew line two."),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 880]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(texts(&c)[1], ("b2", "New line one.\nNew line two."));
        assert!(c.blocks[1].replaces.is_empty());
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
    }

    #[test]
    fn a_merge_outside_the_region_takes_the_whole_reading() {
        // The region touches only the short second line. The merged reading
        // matches b1 by similarity; b1 lies outside the region, but keeping
        // its old bytes would lose "ok", which has no other place to live.
        let long = "Meeting with Anna on Friday about the budget for next year";
        let merged = "Meeting with Anna on Friday about the budget for next year ok";
        assert!(similarity(long, merged) >= SAFETY_NET);
        let olds = [
            old("b1", "Text", [100, 100, 900, 300], long),
            old("b2", "Text", [100, 300, 900, 330], "ok"),
        ];
        let news = [new("Text", [100, 100, 900, 330], merged)];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[850, 305, 900, 325]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b1", merged)]);
        assert_eq!(c.blocks[0].replaces, vec!["b2"]);
        assert_eq!(c.replaced, vec![("b2".to_string(), vec!["b1".to_string()])]);
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
    }

    /// Three paragraph lines the way the converter writes them: a markdown
    /// hard break ends every line but the last.
    const THREE_LINES: &str = "Meeting with Anna on Friday about the budget.\\\nCall Bob about the invoice for March.\\\nSend the draft to the team by Monday.";

    fn short_last_line_reread(b_box: [u32; 4]) -> Composed {
        let old_text = format!("{THREE_LINES}\\\nok");
        assert!(similarity(&old_text, THREE_LINES) >= SAFETY_NET);
        let olds = [old("b1", "Text", [100, 100, 900, 700], &old_text)];
        let news = [
            new("Text", [100, 100, 900, 580], THREE_LINES),
            new("Text", b_box, "ok\\\nNew line."),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 880]]),
            PAGE,
            &mut next,
        );
        assert_eq!(next, 3);
        c
    }

    #[test]
    fn a_short_reread_last_line_with_a_hard_break_does_not_appear_twice() {
        // bs58: "ok" becomes "ok\" as the first line of piece B, which lies
        // 58% inside b1.
        let c = short_last_line_reread([100, 590, 900, 780]);
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("ok").count(), 1);
        assert_eq!(texts(&c), [("b1", THREE_LINES), ("b2", "ok\\\nNew line.")]);
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
    }

    #[test]
    fn a_short_reread_last_line_under_half_inside_does_not_appear_twice() {
        // bs33: as above, with piece B only a third inside b1.
        let c = short_last_line_reread([100, 600, 900, 900]);
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("ok").count(), 1);
        assert_eq!(texts(&c), [("b1", THREE_LINES), ("b2", "ok\\\nNew line.")]);
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
    }

    #[test]
    fn a_reread_last_line_joined_with_new_ink_does_not_appear_twice() {
        // x03: piece B reads the old last line and the new ink as one line.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        let first_five = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.";
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [
            new("Text", [100, 100, 900, 580], first_five),
            new("Text", [100, 590, 900, 780], "Line six. New line one."),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 780]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("Line six.").count(), 1);
        assert_eq!(page.matches("New line one.").count(), 1);
        assert_eq!(
            texts(&c),
            [("b1", first_five), ("b2", "Line six. New line one.")]
        );
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
    }

    #[test]
    fn a_reread_last_line_that_shared_its_old_line_does_not_appear_twice() {
        // x05: the old text held "Line five. Line six." as one line.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five. Line six.";
        let first_five = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.";
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [
            new("Text", [100, 100, 900, 580], first_five),
            new("Text", [100, 590, 900, 780], "Line six.\nNew line."),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 780]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("Line six.").count(), 1);
        assert_eq!(page.matches("New line.").count(), 1);
        assert_eq!(
            texts(&c),
            [("b1", first_five), ("b2", "Line six.\nNew line.")]
        );
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
    }

    #[test]
    fn new_ink_read_into_a_close_reading_is_not_lost() {
        // x07: a line written just below b1 is read into b1's own reading,
        // which stays close to the old text; b1 lies outside the region.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        let with_new =
            "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.\nNew line.";
        assert!(similarity(paragraph, with_new) >= SAFETY_NET);
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [new("Text", [100, 100, 900, 790], with_new)];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 720, 900, 780]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("New line.").count(), 1);
        assert_eq!(page.matches("Line six.").count(), 1);
        assert_eq!(texts(&c), [("b1", with_new)]);
        assert!(!c.unchanged);
        assert_eq!(c.region_missed, 0);
        assert!(c.replaced.is_empty());
        assert_eq!(next, 2);
    }

    #[test]
    fn an_edit_read_into_a_neighbours_close_reading_is_not_lost() {
        // x13: the edited short line "ok!" is read into the long line above
        // it, whose reading covers only 40% of the short line's box.
        let long = "Meeting with Anna on Friday about the budget for next year";
        let reading = "Meeting with Anna on Friday about the budget for next year\\\nok!";
        assert!(similarity(long, reading) >= SAFETY_NET);
        let olds = [
            old("b1", "Text", [100, 100, 900, 300], long),
            old("b2", "Text", [100, 300, 900, 350], "ok"),
        ];
        let news = [new("Text", [100, 100, 900, 320], reading)];
        let mut next = 3;
        let c = compose(
            &olds,
            &news,
            &region(&[[850, 310, 900, 340]]),
            PAGE,
            &mut next,
        );
        let page: String = c.blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(page.matches("ok").count(), 1);
        assert_eq!(page.matches("ok!").count(), 1);
        assert_eq!(texts(&c), [("b1", reading)]);
        assert!(!c.unchanged);
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
    }

    /// b1 outside the region owns a grazing new line, so it joins the match
    /// pool with its own `reading` (as in `a_new_line_grazing_a_block_keeps_its_bytes`).
    fn grazed(first: &str, reading: &str) -> Composed {
        assert!(
            similarity(first, reading) >= SAFETY_NET,
            "{first:?} / {reading:?}"
        );
        let olds = [old("b1", "Text", [100, 100, 900, 200], first)];
        let news = [
            new("Text", [100, 100, 900, 198], reading),
            new("Text", [100, 195, 900, 260], "Call the bank"),
        ];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[150, 210, 800, 250]]),
            PAGE,
            &mut next,
        );
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0], olds[0]);
        assert_eq!(texts(&c)[1], ("b2", "Call the bank"));
        assert_eq!(c.region_missed, 0);
        assert_eq!(next, 3);
        c
    }

    #[test]
    fn a_line_rebreak_of_a_close_reading_keeps_the_bytes() {
        // The reading breaks the two lines at another word: no new ink.
        grazed(
            "Meeting with Anna on Friday about the budget.\\\nand the plan",
            "Meeting with Anna on\\\nFriday about the budget and the plan",
        );
    }

    #[test]
    fn split_words_in_a_close_reading_keep_the_bytes() {
        // Three words read in two pieces each: no new words.
        grazed(
            "Meeting with Anna on Friday about the budget.",
            "Meet ing with An na on Fri day about the budget.",
        );
    }

    #[test]
    fn a_one_character_misread_keeps_the_bytes() {
        // "A" read as "4" inside a line of several words.
        grazed(
            "Plan A for Monday with the whole team.\\\nCall Bob about it",
            "Plan 4 for Monday with the whole team.\\\nCall Bob about it",
        );
    }

    #[test]
    fn a_new_line_above_the_reread_lines_is_not_lost() {
        // Like x07, with the new line first: it shares "line" with the old
        // text, which must not hide it.
        let paragraph = "Line one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        let with_new =
            "New line.\nLine one.\nLine two.\nLine three.\nLine four.\nLine five.\nLine six.";
        assert!(similarity(paragraph, with_new) >= SAFETY_NET);
        let olds = [old("b1", "Text", [100, 100, 900, 700], paragraph)];
        let news = [new("Text", [100, 20, 900, 700], with_new)];
        let mut next = 2;
        let c = compose(
            &olds,
            &news,
            &region(&[[100, 20, 900, 90]]),
            PAGE,
            &mut next,
        );
        assert_eq!(texts(&c), [("b1", with_new)]);
        assert_eq!(c.region_missed, 0);
    }
}
