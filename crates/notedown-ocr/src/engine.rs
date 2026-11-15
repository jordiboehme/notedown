use crate::OcrError;

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
    /// Layout label in Chandra's vocabulary ("Text", "Section-Header", "List-Group", "Diagram",
    /// "Figure", "Image", "Table", ...). The pipeline stores and compares these strings.
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
    /// The page as Markdown (what `convert::assemble` gives for the blocks' Markdown).
    pub markdown: String,
    pub mermaid_blocks: usize,
    /// The engine's raw answer (Chandra HTML today), for debug output only.
    pub raw: String,
    pub prompt_tokens: u32,
    pub output_tokens: u32,
    /// True when generation stopped at `max_tokens`.
    pub truncated: bool,
    pub encode_ms: u64,
    pub decode_ms: u64,
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
    fn default_options() {
        let o = OcrOptions::default();
        assert_eq!(o.max_tokens, 4096);
        assert_eq!(o.threads, None);
    }
}
