//! Page rendering to RGBA images (docs/note-format.md 6).

use std::io::Cursor;

use image::{ImageFormat, Rgba, RgbaImage};

use crate::bitmap;
use crate::error::NoteError;
use crate::model::{Layer, LayerSlot, Page};
use crate::notebook::Notebook;

/// Which compositing rules to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStyle {
    /// Replacement compositing; matches the golden renderings pixel-exactly
    /// (docs/note-format.md 6.1).
    Reference,
    /// The device's own PNG export rules: black is gray 1 and anti-aliasing
    /// levels multiply with what is below (docs/note-format.md 6.2).
    Device,
}

/// Options for [`Notebook::render_page`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    /// Draw the template layer (BGLAYER). Without it, pixels no layer covers
    /// are transparent white (docs/note-format.md 6.1).
    pub include_background: bool,
    pub style: RenderStyle,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            include_background: false,
            style: RenderStyle::Reference,
        }
    }
}

/// Codes with a name in docs/note-format.md 5.3; every other code is an
/// anti-aliasing level.
fn is_named(code: u8) -> bool {
    matches!(code, 0x61..=0x68 | 0x9D | 0x9E | 0xC9 | 0xCA)
}

/// Layers to draw, bottom to top: `LAYERSEQ` reversed (docs/note-format.md 6.1 step 4).
fn drawing_order(page: &Page) -> Vec<&Layer> {
    let slots: Vec<LayerSlot> = if page.layer_seq.is_empty() {
        // no LAYERSEQ: bottom to top by slot
        vec![
            LayerSlot::Background,
            LayerSlot::Layer3,
            LayerSlot::Layer2,
            LayerSlot::Layer1,
            LayerSlot::Main,
        ]
    } else {
        page.layer_seq
            .iter()
            .rev()
            .filter_map(|name| LayerSlot::from_key(name))
            .collect()
    };
    slots.into_iter().filter_map(|s| page.layer(s)).collect()
}

impl Notebook {
    pub(crate) fn page_or_err(&self, index: usize) -> Result<&Page, NoteError> {
        self.page(index)
            .ok_or_else(|| NoteError::Decode(format!("no page {index}")))
    }

    /// Renders page `index` (0-based) in its stored orientation (docs/note-format.md 6).
    pub fn render_page(&self, index: usize, opts: &RenderOptions) -> Result<RgbaImage, NoteError> {
        let page = self.page_or_err(index)?;
        let x2 = self.uses_x2_colors();
        let count = bitmap::pixel_count("page size", page.width, page.height)?;
        let mut gray = vec![255u8; count];
        let mut covered = vec![false; count];
        for layer in drawing_order(page) {
            let is_background = layer.slot == LayerSlot::Background;
            if !layer.visible
                || layer.bitmap.is_none()
                || (is_background && !opts.include_background)
            {
                continue;
            }
            let decoded = self.decode_page_layer(page, layer)?;
            for ((&code, g), c) in decoded
                .codes
                .iter()
                .zip(gray.iter_mut())
                .zip(covered.iter_mut())
            {
                let table = if x2 {
                    bitmap::gray_x2(code)
                } else {
                    bitmap::gray_x(code)
                };
                let Some(level) = table else { continue };
                *g = match opts.style {
                    RenderStyle::Reference => level,
                    RenderStyle::Device => {
                        if code == 0x61 || code == 0x66 {
                            1
                        } else if is_named(code) {
                            level
                        } else {
                            // multiply with what is below (docs/note-format.md 6.2)
                            let product = u32::from(level) * u32::from(*g);
                            u8::try_from((product + 127) / 255).unwrap_or(255)
                        }
                    }
                };
                *c = true;
            }
        }
        let mut img = RgbaImage::new(page.width, page.height);
        for ((px, &g), &c) in img.pixels_mut().zip(&gray).zip(&covered) {
            let alpha = if c || opts.include_background { 255 } else { 0 };
            *px = Rgba([g, g, g, alpha]);
        }
        Ok(img)
    }

    /// True when every visible non-background layer is transparent or white only
    /// (docs/note-format.md 5.3: `0x62` and `0x65`).
    pub fn page_is_blank(&self, index: usize) -> Result<bool, NoteError> {
        let page = self.page_or_err(index)?;
        for layer in &page.layers {
            if layer.slot == LayerSlot::Background || !layer.visible || layer.bitmap.is_none() {
                continue;
            }
            if !self.decode_page_layer(page, layer)?.is_blank() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Encodes an image as PNG.
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, NoteError> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .map_err(|e| NoteError::Decode(format!("cannot encode PNG: {e}")))?;
    Ok(out.into_inner())
}
