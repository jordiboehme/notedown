// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::OcrError;
use crate::markdown::assemble;

/// Settings for one OCR run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrOptions {
    /// Upper limit for generated tokens per page.
    pub max_tokens: u32,
    /// CPU threads; `None` uses the available parallelism, at most 8.
    pub threads: Option<i32>,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            threads: None,
        }
    }
}

/// One block of the OCR result, independent of the model that produced it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OcrBlock {
    /// Layout label ("Text", "Section-Header", "List-Group", "Diagram", "Image", "Table", ...).
    /// The pipeline stores and compares these strings; notedown 0.1 also stored "Figure" and
    /// a few other labels.
    pub label: String,
    /// Box on the 0..=1000 grid, if the engine gave one.
    pub bbox: Option<[u32; 4]>,
    /// The block as Markdown; empty for a block that renders nothing (kept for index alignment).
    pub markdown: String,
}

/// The OCR result for one page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageOcr {
    pub blocks: Vec<OcrBlock>,
    /// The page as Markdown (what `markdown::assemble` gives for the blocks' Markdown).
    pub markdown: String,
    pub mermaid_blocks: usize,
    /// The engine's raw answers, for debug output only.
    pub raw: String,
    pub prompt_tokens: u32,
    pub output_tokens: u32,
    /// True when generation stopped at `max_tokens`.
    pub truncated: bool,
    pub encode_ms: u64,
    pub decode_ms: u64,
}

impl PageOcr {
    /// A page of these blocks: the page Markdown from [`assemble`], one Mermaid block per
    /// "Diagram" block, no raw answer, zero counters and `truncated = false`.
    pub fn from_blocks(blocks: Vec<OcrBlock>) -> PageOcr {
        let parts: Vec<String> = blocks.iter().map(|b| b.markdown.clone()).collect();
        PageOcr {
            markdown: assemble(&parts),
            mermaid_blocks: blocks.iter().filter(|b| b.label == "Diagram").count(),
            blocks,
            raw: String::new(),
            prompt_tokens: 0,
            output_tokens: 0,
            truncated: false,
            encode_ms: 0,
            decode_ms: 0,
        }
    }
}

/// A backend that turns a page image into blocks of Markdown.
pub trait OcrEngine {
    fn ocr_png(&mut self, png: &[u8]) -> Result<PageOcr, OcrError>;
    fn label(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_blocks_joins_the_markdown_and_counts_diagrams() {
        let block = |label: &str, markdown: &str| OcrBlock {
            label: label.into(),
            bbox: Some([0, 0, 10, 10]),
            markdown: markdown.into(),
        };
        let page = PageOcr::from_blocks(vec![
            block("Text", "Hello  "),
            block("Image", ""),
            block("Diagram", "```mermaid\ngraph LR\nA --> B\n```"),
        ]);
        assert_eq!(page.blocks.len(), 3);
        assert_eq!(
            page.markdown,
            "Hello\n\n```mermaid\ngraph LR\nA --> B\n```\n"
        );
        assert_eq!(page.mermaid_blocks, 1);
        assert!(!page.truncated);
        assert_eq!(page.raw, "");
    }

    #[test]
    fn default_options() {
        let o = OcrOptions::default();
        assert_eq!(o.max_tokens, 4096);
        assert_eq!(o.threads, None);
    }
}
