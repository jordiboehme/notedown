//! A fake OCR engine for tests: no model, the reading is looked up by the SHA-256 of the PNG.
#![allow(dead_code)] // shared with the tests of later tasks

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use notedown_ocr::{OcrBlock, OcrEngine, OcrError, PageOcr};
use sha2::{Digest, Sha256};

/// The default reading: one text block over the whole page.
pub fn default_reading() -> Vec<OcrBlock> {
    vec![text_block([0, 0, 1000, 1000], "fake page")]
}

/// One block with a box on the 0..1000 grid, or none.
pub fn block(label: &str, bbox: Option<[u32; 4]>, markdown: &str) -> OcrBlock {
    OcrBlock {
        label: label.into(),
        bbox,
        markdown: markdown.into(),
    }
}

/// One `Text` block with a box on the 0..1000 grid.
pub fn text_block(bbox: [u32; 4], text: &str) -> OcrBlock {
    block("Text", Some(bbox), text)
}

pub fn sha256(png: &[u8]) -> [u8; 32] {
    Sha256::digest(png).into()
}

/// Returns the blocks mapped to the PNG's SHA-256, or the default. Counts calls and can fail
/// the next N of them. The handles are shared, so a test can keep them after the engine moved.
pub struct FakeEngine {
    map: HashMap<[u8; 32], Vec<OcrBlock>>,
    default_blocks: Vec<OcrBlock>,
    calls: Arc<AtomicUsize>,
    fail_next: Arc<AtomicU32>,
    truncated: bool,
    label: String,
    /// Runs before each call, as a person changing files while the engine reads.
    on_call: Option<Box<dyn FnMut() + Send>>,
}

impl FakeEngine {
    pub fn new(map: HashMap<[u8; 32], Vec<OcrBlock>>) -> Self {
        FakeEngine {
            map,
            default_blocks: default_reading(),
            calls: Arc::default(),
            fail_next: Arc::default(),
            truncated: false,
            label: "fake-engine".to_string(),
            on_call: None,
        }
    }

    /// Runs `f` before each call.
    pub fn on_call(mut self, f: impl FnMut() + Send + 'static) -> Self {
        self.on_call = Some(Box::new(f));
        self
    }

    pub fn with_default(mut self, blocks: Vec<OcrBlock>) -> Self {
        self.default_blocks = blocks;
        self
    }

    /// The model name the engine gives (`label()`).
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = label.to_string();
        self
    }

    /// Reports every reading as cut at the token limit.
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Maps one more PNG to `blocks`.
    pub fn insert(&mut self, png: &[u8], blocks: Vec<OcrBlock>) {
        self.map.insert(sha256(png), blocks);
    }

    /// The next `n` calls fail.
    pub fn fail_next(&self, n: u32) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn calls_handle(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }

    pub fn fail_handle(&self) -> Arc<AtomicU32> {
        self.fail_next.clone()
    }
}

impl OcrEngine for FakeEngine {
    fn ocr_png(&mut self, png: &[u8]) -> Result<PageOcr, OcrError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = &mut self.on_call {
            f();
        }
        let failing = self
            .fail_next
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if failing {
            return Err(OcrError::Engine("fake engine failure".into()));
        }
        let blocks = self
            .map
            .get(&sha256(png))
            .unwrap_or(&self.default_blocks)
            .clone();
        Ok(PageOcr {
            truncated: self.truncated,
            ..PageOcr::from_blocks(blocks)
        })
    }

    fn label(&self) -> &str {
        &self.label
    }
}
