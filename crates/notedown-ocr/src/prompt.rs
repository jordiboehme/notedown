/// The exact `OCR_LAYOUT_PROMPT` from datalab-to/chandra `chandra/prompts.py`.
pub const OCR_LAYOUT_PROMPT: &str =
    include_str!("../../../tests/fixtures/chandra/ocr_layout_prompt.txt");

/// Builds the ChatML prompt with the image marker in front of the layout prompt and an
/// empty think block, so the model answers with the layout HTML right away.
pub fn chat_prompt(media_marker: &str) -> String {
    format!(
        "<|im_start|>user\n{media_marker}{OCR_LAYOUT_PROMPT}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
    )
}

/// Removes a leading `<think>...</think>` block and the whitespace around it.
pub fn strip_think(output: &str) -> &str {
    let trimmed = output.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<think>")
        && let Some(end) = rest.find("</think>")
    {
        return rest[end + "</think>".len()..].trim_start();
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_the_chandra_layout_prompt() {
        assert!(
            OCR_LAYOUT_PROMPT.starts_with("OCR this image to HTML, arranged as layout blocks.")
        );
        assert!(OCR_LAYOUT_PROMPT.contains("- Diagram"));
    }

    #[test]
    fn chat_prompt_framing() {
        let p = chat_prompt("<__media__>");
        assert_eq!(p.matches("<__media__>").count(), 1);
        assert!(p.starts_with("<|im_start|>user\n<__media__>OCR this image"));
        assert!(p.contains(OCR_LAYOUT_PROMPT));
        assert!(p.ends_with("<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"));
    }

    #[test]
    fn strips_think_block() {
        assert_eq!(
            strip_think("<think>\n\n</think>\n\n<div>x</div>"),
            "<div>x</div>"
        );
        assert_eq!(
            strip_think("\n<think>\nhmm\n</think><div>x</div>"),
            "<div>x</div>"
        );
    }

    #[test]
    fn keeps_output_without_think_block() {
        assert_eq!(strip_think("\n\n<div>x</div>\n"), "<div>x</div>\n");
        assert_eq!(
            strip_think("<div><think>a</think></div>"),
            "<div><think>a</think></div>"
        );
    }

    #[test]
    fn keeps_unclosed_think_block() {
        assert_eq!(
            strip_think("<think>\nnever closed"),
            "<think>\nnever closed"
        );
    }
}
