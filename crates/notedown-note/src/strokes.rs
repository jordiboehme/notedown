//! Stroke records of the `TOTALPATH` block (docs/note-format.md 9).
//!
//! Every field of the record layout in 9.3 is exposed: typed where the spec
//! gives a meaning, as plain numbers or bytes where it does not (the
//! `unknown_*` fields). Parts A to O are read strictly; part P is read best
//! effort and kept as raw bytes when it does not parse (9.3, last paragraph).
//! Whether the vector data is complete relative to the bitmap (9.7) is
//! [`crate::Page::vectors_current`].

use base64::Engine;
use serde::Serialize;

use crate::error::NoteError;
use crate::notebook::Notebook;

/// Status of a record (docs/note-format.md 9.3 G).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StrokeStatus {
    /// `0`: the stroke is visible.
    Live,
    /// `-99`: the stroke was erased.
    Erased,
    /// Any other code (others report -16, -4, -3, -2).
    Other(i32),
}

impl StrokeStatus {
    fn from_code(code: i32) -> StrokeStatus {
        match code {
            0 => StrokeStatus::Live,
            -99 => StrokeStatus::Erased,
            other => StrokeStatus::Other(other),
        }
    }
}

/// What a record is, derived from tool, color, class and kind name with the
/// skip rules of docs/note-format.md 9.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RecordKind {
    /// A drawn stroke: everything not listed below.
    Ink,
    /// Tool 3 or color 255: an eraser path.
    Eraser,
    /// Class -5 or tool 4: a lasso path.
    Lasso,
    /// Kind name `0000`: a link box (9.6). The rectangle is not drawn.
    LinkBox,
    /// Kind name `0001`: a filled rectangle (heading background) \[others\].
    FilledRect,
    /// Kind name `straightLine`: a ruler line between two points \[others\].
    StraightLine,
}

/// The bounding box at offset 100, in page pixels (docs/note-format.md 9.3 A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct StrokeBox {
    pub left: i32,
    pub top: i32,
    pub center_x: i32,
    pub center_y: i32,
    pub right: i32,
    pub bottom: i32,
}

/// One outline ring with its place in the ring hierarchy
/// (docs/note-format.md 9.3 M, N). Indexes point into [`StrokeRecord::rings`];
/// `-1` means none. Outer rings have parent -1, holes have their outer ring
/// as parent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutlineRing {
    /// Ring points in page pixels, x then y, not mirrored (9.2).
    pub points: Vec<(f32, f32)>,
    pub next: i32,
    pub previous: i32,
    pub first_child: i32,
    pub parent: i32,
}

/// Part P of a record when it parses (docs/note-format.md 9.3 P).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrokeTrailer {
    /// The three length-prefixed strings, raw. The first is `none` on ink and the
    /// base64 link items on link boxes (9.6); see [`StrokeRecord::link_items`].
    pub strings: Vec<Vec<u8>>,
    /// The `u32` after the strings, `3` in all samples.
    pub constant: u32,
    /// The `f32` pairs after it ("marker fill direction" \[others\]).
    pub fill_direction: Vec<(f32, f32)>,
}

/// One record of a `TOTALPATH` block (docs/note-format.md 9.3, Appendix A
/// "Stroke record"). Field names follow the spec; `unknown_<offset>` fields
/// are head fields whose meaning is not established.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrokeRecord {
    /// 0-based position in the block (record order = creation order, 9.1).
    pub index: usize,
    /// Record length in bytes.
    pub len: usize,

    // A. fixed head
    /// Offset 0: 10 ink stroke, 3 eraser, 4 lasso, 0 link box.
    pub tool: u32,
    /// Offset 4: 0 black, 255 eraser; draw color c as gray c (9.5).
    pub color: u32,
    /// Offset 8: width in 1/100 page pixel \[others\]; 400 for ink.
    pub thickness: u32,
    pub unknown_12: u32,
    pub unknown_16: u32,
    /// "font height" \[others\].
    pub unknown_20: u32,
    pub unknown_24: i32,
    /// Offset 28: page number, 1-based as written by the device.
    pub page_number: u32,
    pub unknown_32: u32,
    pub unknown_36: u32,
    /// Offset 40: record class, 5000 ink, -2 eraser, -5 lasso, 0 link box.
    pub class: i32,
    /// Offset 44: layer, 0 MAINLAYER, 1-3 LAYER1-3 \[others\].
    pub layer: u32,
    /// Offset 48: kind name (`others`, `0000`, ...), NUL padding removed.
    pub kind_name: String,
    /// Offset 100: bounding box in page pixels.
    pub bbox: StrokeBox,
    pub unknown_124: i32,
    /// Offset 128: vertical digitizer range.
    pub max_a: u32,
    /// Offset 132: horizontal digitizer range.
    pub max_b: u32,
    /// Offset 136: document kind (`superNoteNote`), NUL padding removed.
    pub document_kind: String,
    pub unknown_188: u32,
    /// Offset 192: PDF anchor (chapter, position, offset x, offset y) \[others\].
    pub pdf_anchor: [u32; 4],
    /// Offset 208: "disable areas", 24 raw bytes each \[others\].
    pub disable_areas: Vec<Vec<u8>>,

    // B. to F.
    /// Points in raw digitizer units, `(a, b)` as stored: `a` vertical, `b`
    /// horizontal and mirrored (9.2).
    pub points: Vec<(u32, u32)>,
    /// Points in page pixels `(x, y)` (9.2 transform). Empty when the record's
    /// digitizer range is implausible (10.8): smaller than the page size.
    pub points_px: Vec<(f32, f32)>,
    /// Pressure per point (C); full scale likely 4095.
    pub pressure: Vec<u16>,
    /// Tilt per point (D), meaning not established.
    pub tilt: Vec<(i16, i16)>,
    /// Point flags (E), 1 in all samples.
    pub point_flags: Vec<u8>,
    /// First array of part F (same shape as the points), empty in the samples.
    pub extra_points: Vec<(u32, u32)>,
    /// Second array of part F (`i32` gray values \[others\]), empty in the samples.
    pub gray_values: Vec<i32>,

    // G. to L.
    /// Part G, typed.
    pub status: StrokeStatus,
    /// Part G, raw code.
    pub status_code: i32,
    /// Part H: 0 ink, 601 eraser, 604 lasso path, 500 link box.
    pub operation: i32,
    /// Part I: per-page record id, strictly increasing, gaps possible.
    pub id: u32,
    /// Part J: shift angles before and after a lasso move \[others\].
    pub lasso_angles: [u32; 2],
    /// Part J: rect before a move \[others\], `0,0,1,1` when never moved.
    pub rect_before: [i32; 4],
    /// Part J: rect after a move \[others\].
    pub rect_after: [i32; 4],
    /// Part K: ids an eraser erased or a link box links (9.6).
    pub referenced_ids: Vec<u32>,
    /// Part L: the 9 group bytes ("group number, nesting, end" \[others\]).
    pub group: [u8; 9],
    /// Part L: render flag, meaning unknown.
    pub render_flag: u8,

    // M., N.
    /// Outline rings with their hierarchy (device-rendered stroke shape, 9.5).
    pub rings: Vec<OutlineRing>,

    // O.
    /// Part O, first `i32` (-1 in the samples).
    pub unknown_o: i32,
    /// Part O `f64` (1.0 in the samples, a scale is plausible).
    pub scale: f64,
    /// Part O first `u8`.
    pub flag_o: u8,
    /// Part O rotation in degrees from a lasso rotate \[others\].
    pub rotation: i32,
    /// Part O counted `i32` values, meaning unknown.
    pub values_o: Vec<i32>,
    /// Part O page width (1404).
    pub page_width: u32,
    /// Part O page height (1872).
    pub page_height: u32,
    /// Part O: 0 on ink, `LINKSTYLE` on link boxes (9.6).
    pub link_style: u32,
    /// Part O last `u8`.
    pub end_o: u8,

    // P.
    /// Part P when it parses.
    pub trailer: Option<StrokeTrailer>,
    /// Bytes after the parsed trailer (4 zero bytes in 20260016 files), or all
    /// bytes after part O when part P does not parse.
    pub trailer_extra: Vec<u8>,
}

impl StrokeRecord {
    /// What the record is (docs/note-format.md 9.5 skip rules).
    pub fn kind(&self) -> RecordKind {
        match self.kind_name.as_str() {
            "0000" => RecordKind::LinkBox,
            _ if self.tool == 3 || self.color == 255 => RecordKind::Eraser,
            _ if self.class == -5 || self.tool == 4 => RecordKind::Lasso,
            "0001" => RecordKind::FilledRect,
            "straightLine" => RecordKind::StraightLine,
            _ => RecordKind::Ink,
        }
    }

    /// True for a visible drawn line: live (9.3 G) ink or straight line.
    pub fn is_live_ink(&self) -> bool {
        self.status == StrokeStatus::Live
            && matches!(self.kind(), RecordKind::Ink | RecordKind::StraightLine)
    }

    /// The base64 items of a link box's first string, decoded
    /// (docs/note-format.md 9.6): `LINKTYPE`, `LINKINOUT`, `LINKSTYLE`, ...
    /// The string ends with a comma in the samples; that empty last item is
    /// dropped. `None` when the string is absent, `none`, or an item is not base64.
    pub fn link_items(&self) -> Option<Vec<String>> {
        let first = self.trailer.as_ref()?.strings.first()?;
        let first = first.strip_suffix(b",").unwrap_or(first);
        if first.is_empty() || first == b"none" {
            return None;
        }
        first
            .split(|&b| b == b',')
            .map(|item| {
                base64::engine::general_purpose::STANDARD
                    .decode(item)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            })
            .collect()
    }
}

/// A live ink stroke as a polyline, the input of a vector (SVG) converter
/// (docs/note-format.md 9.4, 9.5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Polyline {
    /// Centerline points in page pixels.
    pub points: Vec<(f32, f32)>,
    /// Line width in page pixels: `thickness / 100` (9.4, 9.5). The bitmap is
    /// matched best at about 4.5 px for the needle-point pen (9.5).
    pub width: f32,
    /// Gray level of the stroke color (9.5: draw color c as gray c); color codes
    /// above 255 are clamped to 255.
    pub color: u8,
}

/// Size of the fixed head including the disable-area count (9.3 A).
const HEAD_LEN: usize = 212;

/// Bounds-checked little-endian reader over one record.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub(crate) fn take(&mut self, n: usize, field: &str) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| {
                format!(
                    "{field}: needs {n} bytes at {}, {} left",
                    self.pos,
                    self.remaining()
                )
            })?;
        let out = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| field.to_owned())?;
        self.pos = end;
        Ok(out)
    }

    pub(crate) fn array<const N: usize>(&mut self, field: &str) -> Result<[u8; N], String> {
        let bytes = self.take(N, field)?;
        <[u8; N]>::try_from(bytes).map_err(|_| field.to_owned())
    }

    pub(crate) fn u8(&mut self, field: &str) -> Result<u8, String> {
        Ok(self.array::<1>(field)?[0])
    }
    pub(crate) fn u16(&mut self, field: &str) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.array(field)?))
    }
    pub(crate) fn i16(&mut self, field: &str) -> Result<i16, String> {
        Ok(i16::from_le_bytes(self.array(field)?))
    }
    pub(crate) fn u32(&mut self, field: &str) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array(field)?))
    }
    pub(crate) fn i32(&mut self, field: &str) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.array(field)?))
    }
    pub(crate) fn u64(&mut self, field: &str) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array(field)?))
    }
    pub(crate) fn f32(&mut self, field: &str) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.array(field)?))
    }
    pub(crate) fn f64(&mut self, field: &str) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.array(field)?))
    }

    /// Reads a `u32` count and checks that `count * elem` bytes remain
    /// (docs/note-format.md 10.7) before anything is allocated.
    pub(crate) fn count(&mut self, elem: usize, field: &str) -> Result<usize, String> {
        let n = self.u32(field)?;
        let n = usize::try_from(n).map_err(|_| format!("{field}: count {n} too large"))?;
        match n.checked_mul(elem) {
            Some(bytes) if bytes <= self.remaining() => Ok(n),
            _ => Err(format!(
                "{field}: count {n} x {elem} bytes exceeds the {} bytes left",
                self.remaining()
            )),
        }
    }

    /// A counted array of `elem`-byte items read with `read`. The capacity
    /// reserved up front is bounded by [`capacity`].
    pub(crate) fn list<T>(
        &mut self,
        elem: usize,
        field: &str,
        mut read: impl FnMut(&mut Self) -> Result<T, String>,
    ) -> Result<Vec<T>, String> {
        let n = self.count(elem, field)?;
        let mut out = Vec::with_capacity(capacity::<T>(n, elem));
        for _ in 0..n {
            out.push(read(self)?);
        }
        Ok(out)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let out = self.data.get(self.pos..).unwrap_or_default();
        self.pos = self.data.len();
        out
    }
}

/// Capacity to reserve for `n` items of type `T` that take `encoded` bytes each
/// in the input: never more than the encoded bytes, so a count that passed the
/// bounds check cannot reserve many times the block size when `T` is larger
/// in memory than in the file. The vector grows normally past this.
pub(crate) fn capacity<T>(n: usize, encoded: usize) -> usize {
    let size = std::mem::size_of::<T>().max(1);
    n.min(n.saturating_mul(encoded) / size)
}

/// A NUL-padded name field.
fn name(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(bytes.get(..end).unwrap_or_default()).into_owned()
}

/// The 9.2 transform from digitizer units to page pixels of the portrait page
/// `(width, height)`. Empty when a range is smaller than the page size, which
/// covers ranges of 0 and the range 120 others report (10.8).
fn to_pixels(points: &[(u32, u32)], max_a: u32, max_b: u32, size: (u32, u32)) -> Vec<(f32, f32)> {
    let (w, h) = size;
    if max_a < h || max_b < w || max_a == 0 || max_b == 0 {
        return Vec::new();
    }
    let (w, h, ma, mb) = (
        f64::from(w),
        f64::from(h),
        f64::from(max_a),
        f64::from(max_b),
    );
    points
        .iter()
        .map(|&(a, b)| {
            let x = (mb - f64::from(b)) * w / mb;
            let y = f64::from(a) * h / ma;
            (x as f32, y as f32)
        })
        .collect()
}

/// Parses one record (docs/note-format.md 9.3). `size` is the portrait page size.
fn parse_record(index: usize, data: &[u8], size: (u32, u32)) -> Result<StrokeRecord, String> {
    let mut r = Reader::new(data);
    if data.len() < HEAD_LEN {
        return Err(format!(
            "record of {} bytes is shorter than the head",
            data.len()
        ));
    }
    // A. fixed head
    let tool = r.u32("tool")?;
    let color = r.u32("color")?;
    let thickness = r.u32("thickness")?;
    let unknown_12 = r.u32("offset 12")?;
    let unknown_16 = r.u32("offset 16")?;
    let unknown_20 = r.u32("offset 20")?;
    let unknown_24 = r.i32("offset 24")?;
    let page_number = r.u32("page number")?;
    let unknown_32 = r.u32("offset 32")?;
    let unknown_36 = r.u32("offset 36")?;
    let class = r.i32("class")?;
    let layer = r.u32("layer")?;
    let kind_name = name(r.take(52, "kind name")?);
    let bbox = StrokeBox {
        left: r.i32("bbox")?,
        top: r.i32("bbox")?,
        center_x: r.i32("bbox")?,
        center_y: r.i32("bbox")?,
        right: r.i32("bbox")?,
        bottom: r.i32("bbox")?,
    };
    let unknown_124 = r.i32("offset 124")?;
    let max_a = r.u32("maxA")?;
    let max_b = r.u32("maxB")?;
    let document_kind = name(r.take(52, "document kind")?);
    let unknown_188 = r.u32("offset 188")?;
    let pdf_anchor = [
        r.u32("pdf anchor")?,
        r.u32("pdf anchor")?,
        r.u32("pdf anchor")?,
        r.u32("pdf anchor")?,
    ];
    let disable_areas = r.list(24, "disable areas", |r| {
        Ok(r.take(24, "disable area")?.to_vec())
    })?;

    // B. to E., counts must be equal (10.7)
    let points = r.list(8, "points", |r| Ok((r.u32("point")?, r.u32("point")?)))?;
    let pressure = r.list(2, "pressure", |r| r.u16("pressure"))?;
    let tilt = r.list(4, "tilt", |r| Ok((r.i16("tilt")?, r.i16("tilt")?)))?;
    let point_flags = r.list(1, "point flags", |r| r.u8("point flag"))?;
    let n = points.len();
    if pressure.len() != n || tilt.len() != n || point_flags.len() != n {
        return Err(format!(
            "point counts differ: {n} points, {} pressure, {} tilt, {} flags",
            pressure.len(),
            tilt.len(),
            point_flags.len()
        ));
    }
    // F.
    let extra_points = r.list(8, "extra points", |r| {
        Ok((r.u32("point")?, r.u32("point")?))
    })?;
    let gray_values = r.list(4, "gray values", |r| r.i32("gray value"))?;
    // G. to I.
    let status_code = r.i32("status")?;
    let operation = r.i32("operation")?;
    let id = r.u32("id")?;
    // J.
    let lasso_angles = [r.u32("angle")?, r.u32("angle")?];
    let mut rect_before = [0i32; 4];
    for v in &mut rect_before {
        *v = r.i32("rect before")?;
    }
    let mut rect_after = [0i32; 4];
    for v in &mut rect_after {
        *v = r.i32("rect after")?;
    }
    // K., L.
    let referenced_ids = r.list(4, "referenced ids", |r| r.u32("id"))?;
    let group = r.array::<9>("group")?;
    let render_flag = r.u8("render flag")?;
    // M., N.
    let ring_points = r.list(4, "rings", |r| {
        r.list(8, "ring points", |r| {
            Ok((r.f32("ring point")?, r.f32("ring point")?))
        })
    })?;
    let hierarchy = r.list(16, "ring hierarchy", |r| {
        Ok([
            r.i32("ring")?,
            r.i32("ring")?,
            r.i32("ring")?,
            r.i32("ring")?,
        ])
    })?;
    if hierarchy.len() != ring_points.len() {
        return Err(format!(
            "{} rings but {} hierarchy entries",
            ring_points.len(),
            hierarchy.len()
        ));
    }
    let rings = ring_points
        .into_iter()
        .zip(hierarchy)
        .map(
            |(points, [next, previous, first_child, parent])| OutlineRing {
                points,
                next,
                previous,
                first_child,
                parent,
            },
        )
        .collect();
    // O.
    let unknown_o = r.i32("part O")?;
    let scale = r.f64("part O scale")?;
    let flag_o = r.u8("part O flag")?;
    let rotation = r.i32("rotation")?;
    let values_o = r.list(4, "part O values", |r| r.i32("part O value"))?;
    let page_width = r.u32("page width")?;
    let page_height = r.u32("page height")?;
    let link_style = r.u32("link style")?;
    let end_o = r.u8("part O end")?;

    // P., best effort (9.3 P, last paragraph)
    let after_o = r.pos;
    let (trailer, trailer_extra) = match parse_trailer(&mut r) {
        Ok(t) => (Some(t), r.rest().to_vec()),
        Err(_) => (None, data.get(after_o..).unwrap_or_default().to_vec()),
    };

    let points_px = to_pixels(&points, max_a, max_b, size);
    Ok(StrokeRecord {
        index,
        len: data.len(),
        tool,
        color,
        thickness,
        unknown_12,
        unknown_16,
        unknown_20,
        unknown_24,
        page_number,
        unknown_32,
        unknown_36,
        class,
        layer,
        kind_name,
        bbox,
        unknown_124,
        max_a,
        max_b,
        document_kind,
        unknown_188,
        pdf_anchor,
        disable_areas,
        points,
        points_px,
        pressure,
        tilt,
        point_flags,
        extra_points,
        gray_values,
        status: StrokeStatus::from_code(status_code),
        status_code,
        operation,
        id,
        lasso_angles,
        rect_before,
        rect_after,
        referenced_ids,
        group,
        render_flag,
        rings,
        unknown_o,
        scale,
        flag_o,
        rotation,
        values_o,
        page_width,
        page_height,
        link_style,
        end_o,
        trailer,
        trailer_extra,
    })
}

fn parse_trailer(r: &mut Reader<'_>) -> Result<StrokeTrailer, String> {
    let mut strings = Vec::with_capacity(3);
    for _ in 0..3 {
        let n = r.count(1, "string")?;
        strings.push(r.take(n, "string")?.to_vec());
    }
    let constant = r.u32("trailer constant")?;
    let fill_direction = r.list(8, "fill direction", |r| {
        Ok((r.f32("fill")?, r.f32("fill")?))
    })?;
    Ok(StrokeTrailer {
        strings,
        constant,
        fill_direction,
    })
}

/// Decodes a `TOTALPATH` block payload (docs/note-format.md 9.1, 9.3).
/// `page_size` is the portrait page size `(width, height)` used by the 9.2
/// point transform. Every count and length is checked against the bytes left;
/// malformed data is [`NoteError::Decode`].
pub fn decode_totalpath(
    data: &[u8],
    page_size: (u32, u32),
) -> Result<Vec<StrokeRecord>, NoteError> {
    let err = |m: String| NoteError::Decode(format!("TOTALPATH: {m}"));
    let mut r = Reader::new(data);
    // each record needs its 4-byte length and at least the head (10.7)
    let count = r.count(4 + HEAD_LEN, "record count").map_err(err)?;
    let mut records = Vec::with_capacity(capacity::<StrokeRecord>(count, 4 + HEAD_LEN));
    for index in 0..count {
        let len = r
            .count(1, "record length")
            .map_err(|m| err(format!("record {index}: {m}")))?;
        let body = r
            .take(len, "record")
            .map_err(|m| err(format!("record {index}: {m}")))?;
        records.push(
            parse_record(index, body, page_size)
                .map_err(|m| err(format!("record {index}: {m}")))?,
        );
    }
    if r.remaining() != 0 {
        return Err(err(format!(
            "{} bytes after {count} records",
            r.remaining()
        )));
    }
    Ok(records)
}

impl Notebook {
    /// The stroke records of page `page` (0-based) in record order
    /// (docs/note-format.md 9). A page without `TOTALPATH` gives an empty list;
    /// [`crate::Page::vectors_current`] tells whether the records are complete
    /// relative to the bitmap (9.7).
    pub fn strokes(&self, page: usize) -> Result<Vec<StrokeRecord>, NoteError> {
        let p = self.page_or_err(page)?;
        let Some(block) = p.totalpath else {
            return Ok(Vec::new());
        };
        decode_totalpath(self.block(block.address)?, (self.width, self.height))
    }

    /// The live ink strokes of page `page` (0-based) as centerline polylines in
    /// page pixels with their width and gray level (docs/note-format.md 9.4,
    /// 9.5). Erased strokes, eraser and lasso paths, link boxes and filled
    /// rectangles are left out, and so are records without plausible points.
    pub fn ink_polylines(&self, page: usize) -> Result<Vec<Polyline>, NoteError> {
        Ok(self
            .strokes(page)?
            .into_iter()
            .filter(|r| r.is_live_ink() && !r.points_px.is_empty())
            .map(|r| Polyline {
                width: r.thickness as f32 / 100.0,
                color: u8::try_from(r.color).unwrap_or(u8::MAX),
                points: r.points_px,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_rejects_implausible_ranges() {
        let pts = [(1404, 10347)];
        assert!(to_pixels(&pts, 0, 11864, (1404, 1872)).is_empty());
        assert!(to_pixels(&pts, 120, 120, (1404, 1872)).is_empty());
        let px = to_pixels(&pts, 15819, 11864, (1404, 1872));
        assert_eq!(px.len(), 1);
    }

    #[test]
    fn capacity_is_bounded_by_encoded_bytes() {
        assert_eq!(capacity::<u8>(10, 1), 10);
        assert_eq!(capacity::<u32>(10, 8), 10);
        // 4-byte ring counts versus 24-byte vectors
        assert_eq!(capacity::<Vec<(f32, f32)>>(600, 4), 100);
        assert!(
            capacity::<StrokeRecord>(1000, 216) * std::mem::size_of::<StrokeRecord>() <= 216_000
        );
        assert_eq!(capacity::<()>(5, 0), 0);
    }

    #[test]
    fn huge_counts_do_not_allocate() {
        let mut data = vec![0u8; 8];
        data[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_totalpath(&data, (1404, 1872)).is_err());
    }
}
