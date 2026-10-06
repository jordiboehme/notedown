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

/// The OCR result for one page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageOcr {
    /// Chandra layout HTML, without the leading think block.
    pub html: String,
    pub prompt_tokens: u32,
    pub output_tokens: u32,
    /// True when generation stopped at `max_tokens`.
    pub truncated: bool,
    pub encode_ms: u64,
    pub decode_ms: u64,
}

/// A backend that turns a page image into Chandra layout HTML.
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
