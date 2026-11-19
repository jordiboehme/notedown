// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Page OCR with pagevision: two detectors find the regions, Qwen3.5-4B reads them.

use pagevision::engine::PageVision;
use pagevision::llama::RunOptions;
use pagevision::{BlockKind, PageError, PageResult};

use crate::OcrError;
use crate::engine::{OcrBlock, OcrEngine, OcrOptions, PageOcr};
use crate::model::ModelPaths;

/// pagevision behind the `OcrEngine` seam.
#[derive(Debug)]
pub struct VisionEngine {
    inner: PageVision,
    label: String,
}

impl VisionEngine {
    /// Loads the four model files. A missing file is `OcrError::ModelMissing` with its path.
    /// `max_tokens` is the limit of each model call.
    pub fn load(paths: &ModelPaths, opts: OcrOptions) -> Result<Self, OcrError> {
        let run = RunOptions {
            max_tokens: opts.max_tokens,
            threads: opts.threads,
            // Small, and `convert --debug` writes it.
            keep_raw: true,
        };
        Ok(Self {
            inner: PageVision::load(&paths.files, run)?,
            label: paths.label.clone(),
        })
    }
}

impl OcrEngine for VisionEngine {
    fn ocr_png(&mut self, png: &[u8]) -> Result<PageOcr, OcrError> {
        Ok(page_ocr(self.inner.read_page(png)?))
    }

    fn label(&self) -> &str {
        &self.label
    }
}

/// The stored label of a block kind. These are the labels notedown 0.1 stored (Chandra OCR 2),
/// so old and new blocks compare by the same label classes. A sketch is a picture and stored as "Image".
pub fn block_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Paragraph | BlockKind::Text => "Text",
        BlockKind::Heading => "Section-Header",
        BlockKind::List => "List-Group",
        BlockKind::Diagram => "Diagram",
        BlockKind::Table => "Table",
        BlockKind::Picture => "Image",
        // A kind added later reads as text until it gets a label of its own.
        _ => "Text",
    }
}

/// A pagevision result as notedown's page result. The boxes are on the 0..=1000 grid of the
/// page already.
pub fn page_ocr(result: PageResult) -> PageOcr {
    let blocks = result
        .blocks
        .into_iter()
        .map(|b| OcrBlock {
            label: block_label(b.kind()).to_string(),
            bbox: Some([
                u32::from(b.bbox.x0),
                u32::from(b.bbox.y0),
                u32::from(b.bbox.x1),
                u32::from(b.bbox.y1),
            ]),
            markdown: b.markdown,
        })
        .collect();
    // Only a Diagram kind gets the label "Diagram", so the count is the count of Diagram blocks.
    PageOcr {
        raw: result.raw.unwrap_or_default(),
        prompt_tokens: result.stats.prompt_tokens,
        output_tokens: result.stats.output_tokens,
        truncated: result.truncated,
        encode_ms: result.stats.encode_ms,
        decode_ms: result.stats.decode_ms,
        ..PageOcr::from_blocks(blocks)
    }
}

impl From<PageError> for OcrError {
    fn from(e: PageError) -> Self {
        match e {
            PageError::Unreadable { reason } => OcrError::Unreadable { reason },
            PageError::ModelMissing { path } => OcrError::ModelMissing { path },
            e @ (PageError::UnknownImageSize { .. } | PageError::ImageTokens { .. }) => {
                OcrError::Runtime(e.to_string())
            }
            PageError::Image(m) => OcrError::Engine(format!("image: {m}")),
            PageError::Engine(m) => OcrError::Engine(m),
            PageError::Io(e) => OcrError::Io(e),
            // PageError is non_exhaustive: a newer error reads as an engine error.
            e => OcrError::Engine(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pagevision::{BBox, Block, BlockKind, Content, Fingerprint, PageResult, Stats};

    fn block(bbox: [u16; 4], content: Content, markdown: &str) -> Block {
        Block {
            bbox: BBox {
                x0: bbox[0],
                y0: bbox[1],
                x1: bbox[2],
                y1: bbox[3],
            },
            certainty: 0.9,
            content,
            markdown: markdown.to_string(),
        }
    }

    fn result(blocks: Vec<Block>) -> PageResult {
        PageResult {
            width_px: 1404,
            height_px: 1872,
            blocks,
            markdown: String::new(),
            truncated: false,
            stats: Stats {
                prompt_tokens: 3000,
                image_tokens: 2552,
                output_tokens: 120,
                encode_ms: 5,
                decode_ms: 7,
            },
            fingerprint: Fingerprint::default(),
            raw: None,
        }
    }

    #[test]
    fn labels_match_the_stored_vocabulary() {
        assert_eq!(block_label(BlockKind::Paragraph), "Text");
        assert_eq!(block_label(BlockKind::Text), "Text");
        assert_eq!(block_label(BlockKind::Heading), "Section-Header");
        assert_eq!(block_label(BlockKind::List), "List-Group");
        assert_eq!(block_label(BlockKind::Diagram), "Diagram");
        assert_eq!(block_label(BlockKind::Table), "Table");
        assert_eq!(block_label(BlockKind::Picture), "Image");
    }

    #[test]
    fn sketch_is_image() {
        let page = page_ocr(result(vec![block(
            [100, 200, 600, 700],
            Content::Picture {
                description: "A cup with a handle.".into(),
            },
            "> Sketch: A cup with a handle.",
        )]));
        assert_eq!(page.blocks[0].label, "Image");
        assert_eq!(page.mermaid_blocks, 0);
        assert_eq!(page.markdown, "> Sketch: A cup with a handle.\n");
    }

    #[test]
    fn page_ocr_keeps_block_order_and_boxes() {
        let mut r = result(vec![
            block(
                [50, 40, 900, 90],
                Content::Heading {
                    level: 1,
                    text: "Plan".into(),
                },
                "# Plan",
            ),
            block(
                [96, 291, 658, 567],
                Content::Diagram {
                    mermaid: "graph TD\nA --> B".into(),
                    description: "A goes to B.".into(),
                },
                "```mermaid\ngraph TD\nA --> B\n```\n\n> A goes to B.",
            ),
            block(
                [60, 800, 940, 900],
                Content::Paragraph {
                    text: "Hello  ".into(),
                },
                "Hello  ",
            ),
        ]);
        r.truncated = true;
        r.raw = Some("answers".into());
        let page = page_ocr(r);
        let got: Vec<(&str, Option<[u32; 4]>)> = page
            .blocks
            .iter()
            .map(|b| (b.label.as_str(), b.bbox))
            .collect();
        assert_eq!(
            got,
            [
                ("Section-Header", Some([50, 40, 900, 90])),
                ("Diagram", Some([96, 291, 658, 567])),
                ("Text", Some([60, 800, 940, 900])),
            ]
        );
        assert_eq!(
            page.blocks[1].markdown,
            "```mermaid\ngraph TD\nA --> B\n```\n\n> A goes to B."
        );
        // The page Markdown is the blocks joined as notedown always joined them.
        assert_eq!(
            page.markdown,
            "# Plan\n\n```mermaid\ngraph TD\nA --> B\n```\n\n> A goes to B.\n\nHello\n"
        );
        assert_eq!(page.mermaid_blocks, 1);
        assert_eq!(page.raw, "answers");
        assert!(page.truncated);
        assert_eq!((page.prompt_tokens, page.output_tokens), (3000, 120));
        assert_eq!((page.encode_ms, page.decode_ms), (5, 7));
    }

    #[test]
    fn page_without_raw_has_empty_raw() {
        assert_eq!(page_ocr(result(Vec::new())).raw, "");
    }

    #[test]
    fn unreadable_page_maps_to_unreadable() {
        let e = OcrError::from(pagevision::PageError::Unreadable {
            reason: "only empty answers".into(),
        });
        assert!(
            matches!(&e, OcrError::Unreadable { reason } if reason == "only empty answers"),
            "{e:?}"
        );
    }

    #[test]
    fn page_errors_map_to_their_kind() {
        use pagevision::PageError;
        let missing = OcrError::from(PageError::ModelMissing {
            path: "/m/pp-ocrv6-small-det.onnx".into(),
        });
        assert!(
            matches!(missing, OcrError::ModelMissing { .. }),
            "{missing:?}"
        );
        let size = OcrError::from(PageError::UnknownImageSize {
            width: 7,
            height: 9,
        });
        assert!(
            matches!(&size, OcrError::Runtime(m) if m.contains("7x9")),
            "{size:?}"
        );
        let tokens = OcrError::from(PageError::ImageTokens {
            width: 32,
            height: 32,
            expected: 1,
            actual: 2,
        });
        assert!(matches!(tokens, OcrError::Runtime(_)), "{tokens:?}");
        let image = OcrError::from(PageError::Image("bad png".into()));
        assert!(
            matches!(&image, OcrError::Engine(m) if m.contains("bad png")),
            "{image:?}"
        );
        let engine = OcrError::from(PageError::Engine("decode".into()));
        assert!(matches!(engine, OcrError::Engine(_)), "{engine:?}");
        let io = OcrError::from(PageError::Io(std::io::Error::other("disk")));
        assert!(matches!(io, OcrError::Io(_)), "{io:?}");
    }

    #[test]
    fn load_error_names_the_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::model::ModelPaths {
            files: pagevision::model::ModelFiles::in_dir(dir.path()),
            label: crate::model::ENGINE_LABEL.into(),
        };
        let err = VisionEngine::load(&paths, OcrOptions::default()).unwrap_err();
        assert!(matches!(err, OcrError::ModelMissing { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("Qwen3.5-4B-Q4_K_M.gguf"), "{text}");
        assert!(text.contains("notedown model pull"), "{text}");
    }
}
