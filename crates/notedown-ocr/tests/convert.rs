// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use notedown_ocr::convert::{html_to_markdown, looks_like_mermaid};

#[test]
fn text_and_headers() {
    let html = r#"<div data-bbox="0 0 1000 50" data-label="Page-Header"><p>Header</p></div>
<div data-bbox="10 60 900 120" data-label="Section-Header"><h2>Plan</h2></div>
<div data-bbox="10 130 900 300" data-label="Text"><p>This note is <b>without</b> real-time recognition.</p></div>"#;
    let c = html_to_markdown(html);
    assert_eq!(
        c.markdown.trim(),
        "## Plan\n\nThis note is **without** real-time recognition."
    );
    assert_eq!(c.blocks.len(), 3);
    assert_eq!(c.blocks[2].bbox, Some([10, 130, 900, 300]));
}

#[test]
fn diagram_with_mermaid_becomes_fence() {
    let html = r#"<div data-bbox="100 300 900 600" data-label="Diagram"><pre>graph LR
  search --> learn
  learn --> teach
  teach --> search</pre></div>"#;
    let c = html_to_markdown(html);
    assert_eq!(c.mermaid_blocks, 1);
    assert_eq!(
        c.markdown.trim(),
        "```mermaid\ngraph LR\n  search --> learn\n  learn --> teach\n  teach --> search\n```"
    );
}

#[test]
fn diagram_without_mermaid_becomes_figure_quote() {
    let html =
        r#"<div data-bbox="1 2 3 4" data-label="Figure"><img alt="A cylinder labeled DB"/></div>"#;
    assert_eq!(
        html_to_markdown(html).markdown.trim(),
        "> Figure: A cylinder labeled DB"
    );
}

#[test]
fn lists_checkboxes_tables_math() {
    let html = r#"<div data-label="List-Group"><ul><li>Tomorrow</li><li>Today<ul><li>Scotty</li></ul></li></ul></div>
<div data-label="Form"><ul><li><input type="checkbox" checked> done</li><li><input type="checkbox"> open</li></ul></div>
<div data-label="Table"><table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table></div>
<div data-label="Equation-Block"><math display="block">E=mc^2</math></div>"#;
    let md = html_to_markdown(html).markdown;
    assert!(md.contains("- Tomorrow\n- Today\n  - Scotty"), "{md}");
    assert!(md.contains("- [x] done\n- [ ] open"), "{md}");
    assert!(md.contains("| a | b |\n| --- | --- |\n| 1 | 2 |"), "{md}");
    assert!(md.contains("$$E=mc^2$$"), "{md}");
}

#[test]
fn spanned_table_stays_html() {
    let html = r#"<div data-label="Table"><table><tr><td colspan="2">x</td></tr></table></div>"#;
    assert!(html_to_markdown(html).markdown.contains("<table>"));
}

#[test]
fn plain_html_without_layout_divs() {
    assert_eq!(
        html_to_markdown("<p>Hello <i>there</i></p>")
            .markdown
            .trim(),
        "Hello *there*"
    );
}

#[test]
fn truncated_and_messy_output_never_panics() {
    let messy = r#"stray text <div data-label="Text"><p>Half a sent"#;
    let c = html_to_markdown(messy);
    assert!(c.markdown.contains("stray text"));
    assert!(c.markdown.contains("Half a sent"));
}

#[test]
fn mermaid_keywords() {
    assert!(looks_like_mermaid("  flowchart TD\nA-->B"));
    assert!(looks_like_mermaid("sequenceDiagram\nA->>B: hi"));
    assert!(!looks_like_mermaid("graphics are nice"));
    assert!(!looks_like_mermaid("A cylinder labeled DB"));
}

#[test]
fn mermaid_fence_noise_is_recognized() {
    assert!(looks_like_mermaid("```mermaid\ngraph TD; a --> b\n```"));
    assert!(looks_like_mermaid("mermaid\ngraph TD; a --> b"));
}

#[test]
fn think_block_is_ignored() {
    let c = html_to_markdown("<think>\n\n</think>\n\n<div data-label=\"Text\"><p>Hi</p></div>");
    assert_eq!(c.markdown.trim(), "Hi");
    assert_eq!(c.blocks.len(), 1);
}

#[test]
fn br_becomes_backslash_line_break() {
    let md =
        html_to_markdown(r#"<div data-label="Text"><p>without<br/>real-time</p></div>"#).markdown;
    assert!(md.contains("without\\\nreal-time"), "{md}");
}

#[test]
fn one_line_mermaid_is_pretty_printed() {
    let html = r#"<div data-label="Diagram"><pre>graph TD; a --&gt; b; b --&gt; c</pre></div>"#;
    let md = html_to_markdown(html).markdown;
    assert!(
        md.contains("```mermaid\ngraph TD\n    a --> b\n    b --> c\n```"),
        "{md}"
    );
}

#[test]
fn semicolon_inside_quotes_keeps_line() {
    let html = r#"<div data-label="Diagram"><pre>graph TD; a["x; y"] --&gt; b</pre></div>"#;
    let md = html_to_markdown(html).markdown;
    assert!(md.contains("graph TD; a[\"x; y\"] --> b"), "{md}");
}

#[test]
fn diagram_description_follows_fence() {
    let html = r#"<div data-label="Diagram"><img alt="alt text"/><pre>graph TD; a --&gt; b</pre><p>The   description.</p></div>"#;
    let md = html_to_markdown(html).markdown;
    assert!(md.trim_end().ends_with("```\n\n> The description."), "{md}");
    let html =
        r#"<div data-label="Diagram"><img alt="alt text"/><pre>graph TD; a --&gt; b</pre></div>"#;
    assert!(
        html_to_markdown(html)
            .markdown
            .trim_end()
            .ends_with("```\n\n> alt text")
    );
}

#[test]
fn device_glyphs_in_lists() {
    let html = r#"<div data-label="List-Group"><ul><li>• Tomorrow</li><li>○ Today</li><li>❑ open</li><li>☑ done</li><li>- dash</li></ul></div>"#;
    let md = html_to_markdown(html).markdown;
    assert_eq!(
        md.trim(),
        "- Tomorrow\n- Today\n- [ ] open\n- [x] done\n- dash"
    );
}

#[test]
fn linked_word_marker_is_kept_as_text() {
    let md = html_to_markdown(r#"<div data-label="Text"><p><u>Bike @</u></p></div>"#).markdown;
    assert_eq!(md.trim(), "Bike @");
}

#[test]
fn spike_fixtures_convert() {
    // Raw Chandra output recorded in Task 0.
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/chandra");
    let t2 = std::fs::read_to_string(root.join("test2.html")).unwrap();
    let c = html_to_markdown(&t2);
    assert!(
        c.markdown.contains("without\\\nreal-time recognition."),
        "{}",
        c.markdown
    );
    assert!(c.markdown.contains("```mermaid\ngraph TD\n    search --> Learn\n    Learn --> teach\n    teach --> search\n```"), "{}", c.markdown);
    assert!(
        c.markdown
            .contains("> A cyclic diagram with three nodes: \"search\", \"Learn\", and \"teach\".")
    );
    assert_eq!(c.mermaid_blocks, 2);
    insta::assert_snapshot!("test2_markdown", c.markdown);
    let t1 = std::fs::read_to_string(root.join("test1.html")).unwrap();
    let md1 = html_to_markdown(&t1).markdown;
    assert!(md1.contains("- Tomorrow\n- Today\n- Scotty"), "{md1}");
    insta::assert_snapshot!("test1_markdown", md1);
}

#[test]
fn local_bike_p3_checkbox_lines() {
    // Private fixture: structural assertions only.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/local/chandra/bike-p3.html");
    let Ok(html) = std::fs::read_to_string(&path) else {
        eprintln!("skipping: {} is missing", path.display());
        return;
    };
    let md = html_to_markdown(&html).markdown;
    let lines: Vec<&str> = md.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty());
    assert!(
        lines.iter().all(|l| l.starts_with("- [ ] ")),
        "{} lines, not all checkboxes",
        lines.len()
    );
}

#[test]
fn local_fixtures_never_panic() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/local/chandra");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!("skipping: {} is missing", dir.display());
        return;
    };
    for e in rd.flatten() {
        let html = std::fs::read_to_string(e.path()).unwrap();
        let c = html_to_markdown(&html);
        assert!(!c.blocks.is_empty());
    }
}
