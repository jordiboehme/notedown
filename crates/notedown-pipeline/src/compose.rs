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
//!   `region_missed` (once). A merge or split match takes the new reading,
//!   because it carries text of other blocks. Only
//!   an unmatched block is removed and goes into `replaces`. Owning a taken
//!   reading also pulls in the paragraph whose last line was re-read with new
//!   ink below it, so that line does not appear twice.
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
        // close; a merge or a split brings text of other blocks, so the new
        // reading wins there.
        let keep_old = same_text(&old[i].text, &n.text)
            || (!in_region[i] && sim >= SAFETY_NET && !merge[j] && !split[i]);
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
}
