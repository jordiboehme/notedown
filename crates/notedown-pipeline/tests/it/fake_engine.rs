//! A fake OCR engine for tests: no model, the reading is looked up by the SHA-256 of the PNG.
#![allow(dead_code)] // shared with the tests of later tasks

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use notedown_ocr::convert::chandra_page;
use notedown_ocr::{OcrEngine, OcrError, PageOcr};
use sha2::{Digest, Sha256};

/// The default reading: one text block over the whole page.
pub const DEFAULT_HTML: &str =
    r#"<div data-label="Text" data-bbox="0 0 1000 1000"><p>fake page</p></div>"#;

/// Chandra layout HTML for one `Text` block with a box on the 0..1000 grid.
pub fn text_block(bbox: [u32; 4], text: &str) -> String {
    format!(
        r#"<div data-label="Text" data-bbox="{} {} {} {}"><p>{text}</p></div>"#,
        bbox[0], bbox[1], bbox[2], bbox[3]
    )
}

pub fn sha256(png: &[u8]) -> [u8; 32] {
    Sha256::digest(png).into()
}

/// Returns the HTML mapped to the PNG's SHA-256, or the default. Counts calls and can fail
/// the next N of them. The handles are shared, so a test can keep them after the engine moved.
pub struct FakeEngine {
    map: HashMap<[u8; 32], String>,
    default_html: String,
    calls: Arc<AtomicUsize>,
    fail_next: Arc<AtomicU32>,
    truncated: bool,
    /// Runs before each call, as a person changing files while the engine reads.
    on_call: Option<Box<dyn FnMut() + Send>>,
}

impl FakeEngine {
    pub fn new(map: HashMap<[u8; 32], String>) -> Self {
        FakeEngine {
            map,
            default_html: DEFAULT_HTML.to_string(),
            calls: Arc::default(),
            fail_next: Arc::default(),
            truncated: false,
            on_call: None,
        }
    }

    /// Runs `f` before each call.
    pub fn on_call(mut self, f: impl FnMut() + Send + 'static) -> Self {
        self.on_call = Some(Box::new(f));
        self
    }

    pub fn with_default(mut self, html: &str) -> Self {
        self.default_html = html.to_string();
        self
    }

    /// Reports every reading as cut at the token limit.
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Maps one more PNG to `html`.
    pub fn insert(&mut self, png: &[u8], html: &str) {
        self.map.insert(sha256(png), html.to_string());
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
        let html = self
            .map
            .get(&sha256(png))
            .unwrap_or(&self.default_html)
            .clone();
        Ok(PageOcr {
            truncated: self.truncated,
            ..chandra_page(&html)
        })
    }

    fn label(&self) -> &str {
        "fake-engine"
    }
}
