// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Converts Chandra OCR 2 layout HTML into Markdown with Mermaid fences.

mod blocks;
mod markdown;
mod mermaid;

use crate::engine::{OcrBlock, PageOcr};

pub use crate::markdown::assemble;
pub use mermaid::looks_like_mermaid;

/// One layout block of the OCR output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LayoutBlock {
    pub label: String,
    pub bbox: Option<[u32; 4]>,
    pub html: String,
}

/// Result of converting one page of OCR output.
#[derive(Debug, Clone)]
pub struct Converted {
    pub markdown: String,
    pub blocks: Vec<LayoutBlock>,
    /// The Markdown of each block, aligned with `blocks` (empty for blocks that render nothing).
    pub parts: Vec<String>,
    pub mermaid_blocks: usize,
}

/// Converts Chandra layout HTML to Markdown. Never panics on truncated or messy input.
pub fn html_to_markdown(html: &str) -> Converted {
    let blocks = blocks::split(html);
    let mut parts = Vec::new();
    let mut mermaid_blocks = 0;
    for block in &blocks {
        let rendered = markdown::render_block(&block.label, &block.html);
        mermaid_blocks += rendered.mermaid_blocks;
        parts.push(rendered.text);
    }
    Converted {
        markdown: assemble(&parts),
        blocks,
        parts,
        mermaid_blocks,
    }
}

/// Converts Chandra HTML into a `PageOcr` with zero counters and `truncated = false`.
pub fn chandra_page(html: &str) -> PageOcr {
    let converted = html_to_markdown(html);
    let blocks = converted
        .blocks
        .into_iter()
        .zip(converted.parts)
        .map(|(block, markdown)| OcrBlock {
            label: block.label,
            bbox: block.bbox,
            markdown,
        })
        .collect();
    PageOcr {
        blocks,
        markdown: converted.markdown,
        mermaid_blocks: converted.mermaid_blocks,
        raw: html.to_string(),
        prompt_tokens: 0,
        output_tokens: 0,
        truncated: false,
        encode_ms: 0,
        decode_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRONG_ARROWS: &str = r#"<div data-bbox="96 291 658 567" data-label="Diagram"><pre>graph TD; search((search)) --&gt; teach((teach)); teach --&gt; learn((learn)); learn --&gt; search;</pre></div><div data-bbox="195 652 829 786" data-label="Diagram"><pre>graph LR; App((App)) --&gt; DB[(DB)]; DB --&gt; MCP[MCP];</pre></div>"#;

    fn check(html: &str) {
        let page = chandra_page(html);
        let old = html_to_markdown(html);
        assert_eq!(page.markdown, old.markdown);
        assert_eq!(page.mermaid_blocks, old.mermaid_blocks);
        assert_eq!(page.blocks.len(), old.blocks.len());
        assert_eq!(page.blocks.len(), old.parts.len());
        for ((b, l), part) in page.blocks.iter().zip(&old.blocks).zip(&old.parts) {
            assert_eq!((&b.label, b.bbox), (&l.label, l.bbox));
            assert_eq!(&b.markdown, part);
        }
    }

    #[test]
    fn chandra_page_matches_html_to_markdown() {
        check(WRONG_ARROWS);
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/chandra");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "html") {
                check(&std::fs::read_to_string(&path).unwrap());
                seen += 1;
            }
        }
        assert!(seen > 0);
    }

    #[test]
    fn line_breaks_in_diagram_labels_are_kept() {
        let html = r#"<div data-label="Diagram" data-bbox="0 0 500 500"><pre>graph TD; A["UI<br/>mac OS menu bar item"] --&gt; B["notedown CLI"];</pre></div>"#;
        let page = chandra_page(html);
        assert!(
            page.markdown
                .contains(r#"A["UI<br/>mac OS menu bar item"] --> B["notedown CLI"]"#),
            "{}",
            page.markdown
        );
    }

    #[test]
    fn chandra_page_keeps_empty_blocks() {
        let html = r#"<div data-label="Text" data-bbox="0 0 500 100"><p>Hi</p></div><div data-label="Blank-Page" data-bbox="0 0 1000 1000"></div>"#;
        let page = chandra_page(html);
        assert_eq!(page.blocks.len(), 2);
        assert!(!page.blocks[0].markdown.is_empty());
        assert!(page.blocks[1].markdown.is_empty());
        assert_eq!(page.raw, html);
    }
}
