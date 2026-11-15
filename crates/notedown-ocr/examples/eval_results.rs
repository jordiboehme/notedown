//! Writes Chandra's output for page images in the `pagevision-results/1` format.
//!
//! Usage: cargo run --release -p notedown-ocr --example eval_results -- --out <file> <id>=<png> ...

use notedown_ocr::prompt::OCR_LAYOUT_PROMPT;
use notedown_ocr::{LlamaEngine, OcrEngine, OcrOptions, model};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
struct Fingerprint {
    #[serde(rename = "crate")]
    krate: String,
    llama_cpp_2: &'static str,
    model_sha256: [&'static str; 2],
    prompt_sha256: String,
    preprocess: &'static str,
}

#[derive(Serialize)]
struct Block {
    kind: &'static str,
    label: String,
    bbox: Option<[u32; 4]>,
    markdown: String,
}

#[derive(Serialize)]
struct Page {
    id: String,
    markdown: String,
    truncated: bool,
    prompt_tokens: u32,
    output_tokens: u32,
    encode_ms: u64,
    decode_ms: u64,
    blocks: Vec<Block>,
}

#[derive(Serialize)]
struct Results {
    format: &'static str,
    engine: &'static str,
    fingerprint: Fingerprint,
    pages: Vec<Page>,
}

/// The harness kind of a Chandra block.
fn kind_of(label: &str, markdown: &str) -> &'static str {
    match label {
        "Section-Header" | "Page-Header" => "heading",
        "List-Group" => "list",
        "Table" => "table",
        "Diagram" | "Figure" | "Image" => {
            if markdown.contains("```mermaid") {
                "diagram"
            } else {
                "picture"
            }
        }
        _ => "paragraph",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut out = None;
    let mut inputs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--out" {
            out = args.next();
        } else if let Some((id, path)) = a.split_once('=') {
            inputs.push((id.to_string(), path.to_string()));
        } else {
            return Err(format!("unexpected argument {a}").into());
        }
    }
    let out = out.ok_or("missing --out <file>")?;
    if inputs.is_empty() {
        return Err("no <id>=<png> given".into());
    }

    let paths = model::resolve(&model::DEFAULT_MODEL, None, None)?;
    let mut engine = LlamaEngine::load(&paths, OcrOptions::default())?;
    let mut pages = Vec::new();
    for (id, path) in inputs {
        let png = std::fs::read(&path)?;
        let ocr = engine.ocr_png(&png)?;
        let blocks = ocr
            .blocks
            .into_iter()
            .map(|b| Block {
                kind: kind_of(&b.label, &b.markdown),
                label: b.label,
                bbox: b.bbox,
                markdown: b.markdown,
            })
            .collect();
        pages.push(Page {
            id,
            markdown: ocr.markdown,
            truncated: ocr.truncated,
            prompt_tokens: ocr.prompt_tokens,
            output_tokens: ocr.output_tokens,
            encode_ms: ocr.encode_ms,
            decode_ms: ocr.decode_ms,
            blocks,
        });
    }
    let results = Results {
        format: "pagevision-results/1",
        engine: "chandra-ocr-2",
        fingerprint: Fingerprint {
            krate: format!("notedown-ocr {}", env!("CARGO_PKG_VERSION")),
            llama_cpp_2: "0.1.161",
            model_sha256: [
                model::DEFAULT_MODEL.model.sha256,
                model::DEFAULT_MODEL.mmproj.sha256,
            ],
            prompt_sha256: const_hex::encode(Sha256::digest(OCR_LAYOUT_PROMPT.as_bytes())),
            preprocess: "none",
        },
        pages,
    };
    std::fs::write(out, serde_json::to_string_pretty(&results)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::kind_of;

    #[test]
    fn kinds_follow_the_label() {
        assert_eq!(kind_of("Section-Header", "# A"), "heading");
        assert_eq!(kind_of("Page-Header", "x"), "heading");
        assert_eq!(kind_of("List-Group", "- a"), "list");
        assert_eq!(kind_of("Table", "| a |"), "table");
        assert_eq!(kind_of("Text", "hello"), "paragraph");
        assert_eq!(kind_of("Footnote", "1"), "paragraph");
        for l in ["Diagram", "Figure", "Image"] {
            assert_eq!(kind_of(l, "```mermaid\ngraph TD\n```"), "diagram");
            assert_eq!(kind_of(l, "![](x.png)"), "picture");
        }
        assert_eq!(kind_of("Text", "```mermaid\ngraph TD\n```"), "paragraph");
    }
}
