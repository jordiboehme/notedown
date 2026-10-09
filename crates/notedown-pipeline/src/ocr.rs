//! OCR of one page of a note (spec 3.2).

use notedown::pipeline::{NoteSource, PageSource};
use notedown_note::Notebook;
use notedown_ocr::OcrEngine;
use notedown_ocr::convert::html_to_markdown;

use crate::compose::NewBlock;
use crate::{Error, Result};

/// The whole page on Chandra's grid, for a block without a box.
const WHOLE_PAGE: [u32; 4] = [0, 0, 1000, 1000];

/// What the engine read on one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageReading {
    /// One block per layout block that renders text. A block without a box of its own gets
    /// the whole page, `[0, 0, 1000, 1000]`.
    pub blocks: Vec<NewBlock>,
    /// The engine stopped at its token limit.
    pub truncated: bool,
    /// Page size in pixels.
    pub width: u32,
    pub height: u32,
    pub landscape: bool,
    /// The engine's `label()`.
    pub model: String,
}

/// Renders page `index` (0-based) without the template, like `notedown convert` does by
/// default, and reads it with `engine`. A blank page gives no blocks and no engine call.
pub fn read_page(nb: &Notebook, index: usize, engine: &mut dyn OcrEngine) -> Result<PageReading> {
    let page = nb
        .page(index)
        .ok_or_else(|| Error::Read(format!("page {} does not exist", index + 1)))?;
    let mut reading = PageReading {
        blocks: Vec::new(),
        truncated: false,
        width: page.width,
        height: page.height,
        landscape: page.landscape,
        model: engine.label().to_string(),
    };
    let source = NoteSource::new(nb, false);
    let read = |e: anyhow::Error| Error::Read(format!("page {}: {e:#}", index + 1));
    if source.is_blank(index).map_err(read)? {
        return Ok(reading);
    }
    let png = source.png(index).map_err(read)?;
    let ocr = engine.ocr_png(&png)?;
    reading.truncated = ocr.truncated;
    let converted = html_to_markdown(&ocr.html);
    for (block, part) in converted.blocks.into_iter().zip(converted.parts) {
        if part.trim().is_empty() {
            continue;
        }
        reading.blocks.push(NewBlock {
            label: block.label,
            bbox: block.bbox.unwrap_or(WHOLE_PAGE),
            text: part,
        });
    }
    Ok(reading)
}
