use notedown_ocr::{LlamaEngine, OcrEngine, OcrOptions, model};

#[test]
#[ignore = "needs the Chandra 2 model files (notedown model pull) and takes about a minute"]
fn ocr_test2_page_finds_text_and_diagram() {
    let paths = model::resolve(&model::DEFAULT_MODEL, None, None).unwrap();
    let load_start = std::time::Instant::now();
    let mut engine = LlamaEngine::load(&paths, OcrOptions::default()).unwrap();
    let load_ms = load_start.elapsed().as_millis();
    let png = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/golden/test2-p1-nobg.png"
    ))
    .unwrap();
    let out = engine.ocr_png(&png).unwrap();
    eprintln!(
        "load {load_ms} ms, encode {} ms, decode {} ms, prompt {} tokens, output {} tokens\n{}",
        out.encode_ms, out.decode_ms, out.prompt_tokens, out.output_tokens, out.html
    );
    assert!(!out.truncated);
    let md = notedown_ocr::convert::html_to_markdown(&out.html)
        .markdown
        .to_lowercase();
    assert!(md.contains("recognition"), "{md}");
    assert!(md.contains("```mermaid"), "{md}");
}
