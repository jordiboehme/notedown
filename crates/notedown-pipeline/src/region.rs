//! Change regions between two stroke lists and block coordinates.

use std::collections::HashMap;

use crate::page::StrokeEntry;

/// A rectangle in page pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub fn area(&self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    /// Like `intersects`, but edges count: a box of zero width or height (a
    /// dot, a straight line) touches what it lies on or crosses. Used for the
    /// touch test of a change region, where a missed touch keeps stale text.
    pub fn touches(&self, other: &Rect) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    /// The share (0..=1) of `other` that lies inside `self`.
    pub fn overlap_share_of(&self, other: &Rect) -> f32 {
        let area = other.area();
        if area <= 0.0 {
            return 0.0;
        }
        let w = (self.x1.min(other.x1) - self.x0.max(other.x0)).max(0.0);
        let h = (self.y1.min(other.y1) - self.y0.max(other.y0)).max(0.0);
        w * h / area
    }

    fn of_bbox(b: [i32; 4]) -> Rect {
        Rect {
            x0: b[0] as f32,
            y0: b[1] as f32,
            x1: b[2] as f32,
            y1: b[3] as f32,
        }
    }
}

/// Where a page changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Region {
    Whole,
    Parts(Vec<Rect>),
    Empty,
}

impl Region {
    /// True when `rect` touches the region (edges count, see `Rect::touches`).
    /// `Whole` touches everything, `Empty` nothing.
    pub fn touches(&self, rect: &Rect) -> bool {
        match self {
            Region::Whole => true,
            Region::Parts(parts) => parts.iter().any(|p| p.touches(rect)),
            Region::Empty => false,
        }
    }
}

/// The boxes of strokes that exist on one side only (added, erased or moved),
/// plus the `extra` boxes. Strokes are matched by hash with multiplicity.
pub fn change_region(old: &[StrokeEntry], new: &[StrokeEntry], extra: &[Rect]) -> Region {
    let mut left: HashMap<&str, Vec<[i32; 4]>> = HashMap::new();
    for e in old {
        left.entry(&e.hash).or_default().push(e.bbox);
    }
    let mut rects = Vec::new();
    for e in new {
        match left.get_mut(e.hash.as_str()).and_then(Vec::pop) {
            Some(_) => {}
            None => rects.push(Rect::of_bbox(e.bbox)),
        }
    }
    for (_, boxes) in left {
        rects.extend(boxes.into_iter().map(Rect::of_bbox));
    }
    rects.extend_from_slice(extra);
    if rects.is_empty() {
        Region::Empty
    } else {
        Region::Parts(rects)
    }
}

/// Maps an OCR block box (`[x0, y0, x1, y1]` on a 0..1000 grid over the
/// rendered page image) to page pixels. `width` and `height` are the rendered
/// image size, which is the page bitmap size already swapped for landscape pages
/// (docs/note-format.md 4.6). The renderer draws in the stored orientation, so
/// the image axes are the bitmap axes and no rotation applies; `landscape` is
/// kept for the caller's sake.
pub fn block_rect(bbox: [u32; 4], width: u32, height: u32, _landscape: bool) -> Rect {
    let (w, h) = (width as f32 / 1000.0, height as f32 / 1000.0);
    Rect {
        x0: (bbox[0] as f32 * w).round(),
        y0: (bbox[1] as f32 * h).round(),
        x1: (bbox[2] as f32 * w).round(),
        y1: (bbox[3] as f32 * h).round(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(hash: &str, bbox: [i32; 4]) -> StrokeEntry {
        StrokeEntry {
            hash: hash.into(),
            bbox,
        }
    }
    fn r(b: [f32; 4]) -> Rect {
        Rect {
            x0: b[0],
            y0: b[1],
            x1: b[2],
            y1: b[3],
        }
    }

    #[test]
    fn a_new_stroke_gives_its_box() {
        let old = [s("a", [0, 0, 10, 10])];
        let new = [s("a", [0, 0, 10, 10]), s("b", [20, 20, 30, 40])];
        assert_eq!(
            change_region(&old, &new, &[]),
            Region::Parts(vec![r([20.0, 20.0, 30.0, 40.0])])
        );
    }

    #[test]
    fn an_erased_stroke_gives_its_box() {
        let old = [s("a", [0, 0, 10, 10]), s("b", [20, 20, 30, 40])];
        let new = [s("a", [0, 0, 10, 10])];
        assert_eq!(
            change_region(&old, &new, &[]),
            Region::Parts(vec![r([20.0, 20.0, 30.0, 40.0])])
        );
    }

    #[test]
    fn a_lasso_move_gives_old_and_new_box() {
        let old = [s("a", [0, 0, 10, 10])];
        let new = [s("a2", [100, 100, 110, 110])];
        let Region::Parts(p) = change_region(&old, &new, &[]) else {
            panic!("expected parts");
        };
        assert_eq!(p.len(), 2);
        assert!(p.contains(&r([0.0, 0.0, 10.0, 10.0])));
        assert!(p.contains(&r([100.0, 100.0, 110.0, 110.0])));
    }

    #[test]
    fn no_difference_is_empty() {
        let l = [s("a", [0, 0, 10, 10])];
        assert_eq!(change_region(&l, &l, &[]), Region::Empty);
    }

    #[test]
    fn rect_helpers() {
        let a = r([0.0, 0.0, 10.0, 10.0]);
        let b = r([5.0, 0.0, 15.0, 10.0]);
        assert!(a.intersects(&b));
        assert!(!a.intersects(&r([10.0, 0.0, 20.0, 10.0])));
        assert_eq!(a.area(), 100.0);
        assert_eq!(a.overlap_share_of(&b), 0.5);
    }

    #[test]
    fn a_line_stroke_touches_the_block_it_crosses() {
        // A horizontal line has a box of zero height. Strictly inside a block
        // `intersects` finds it, but not on the block's edge (an underline at
        // the bottom of the box) or crossing an edge-on box. `touches` does.
        let block = r([0.0, 0.0, 200.0, 100.0]);
        let inside = r([100.0, 50.0, 300.0, 50.0]);
        let underline = r([20.0, 100.0, 180.0, 100.0]);
        let dot = r([200.0, 40.0, 200.0, 40.0]);
        assert!(inside.intersects(&block) && inside.touches(&block));
        assert!(!underline.intersects(&block));
        assert!(underline.touches(&block));
        assert!(!dot.intersects(&block));
        assert!(dot.touches(&block));
        assert!(Region::Parts(vec![underline]).touches(&block));
        assert!(!Region::Parts(vec![underline]).touches(&r([0.0, 110.0, 200.0, 200.0])));
        assert!(Region::Whole.touches(&block));
        assert!(!Region::Empty.touches(&block));
    }

    #[test]
    fn block_rect_scales_the_grid() {
        assert_eq!(
            block_rect([0, 0, 1000, 1000], 1404, 1872, false),
            r([0.0, 0.0, 1404.0, 1872.0])
        );
        assert_eq!(
            block_rect([500, 250, 750, 500], 1404, 1872, false),
            r([702.0, 468.0, 1053.0, 936.0])
        );
    }

    #[test]
    fn block_rect_in_landscape() {
        // A landscape page is rendered 1872 x 1404 (Page::width x height are
        // already swapped), so the grid scales onto those axes, no rotation.
        assert_eq!(
            block_rect([0, 0, 1000, 1000], 1872, 1404, true),
            r([0.0, 0.0, 1872.0, 1404.0])
        );
        assert_eq!(
            block_rect([500, 250, 750, 500], 1872, 1404, true),
            r([936.0, 351.0, 1404.0, 702.0])
        );
    }
}
