//! Converts Chandra OCR 2 layout HTML into Markdown with Mermaid fences.

mod blocks;
mod markdown;
mod mermaid;

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

/// Joins per-block Markdown the way [`html_to_markdown`] does.
pub fn assemble(parts: &[String]) -> String {
    let kept: Vec<&str> = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(String::as_str)
        .collect();
    markdown::finalize(&kept.join("\n\n"))
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
