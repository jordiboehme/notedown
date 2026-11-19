// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
        out.encode_ms, out.decode_ms, out.prompt_tokens, out.output_tokens, out.raw
    );
    assert!(!out.truncated);
    let md = out.markdown.to_lowercase();
    assert!(md.contains("recognition"), "{md}");
    assert!(md.contains("```mermaid"), "{md}");

    // The arrows must be right, not only present: a llama.cpp update changed the image
    // preprocessing once and the same model then drew most of these edges wrong.
    let mut edges = mermaid_edges(&md);
    edges.sort();
    let mut want = vec![
        edge("search", "-->", "learn"),
        edge("learn", "-->", "teach"),
        edge("teach", "-->", "search"),
        edge("app", "-->", "db"),
        edge("db", "<-->", "mcp"),
    ];
    want.sort();
    assert_eq!(edges, want, "{md}");
}

#[test]
fn edge_check_sees_wrong_arrows() {
    // Chandra 2 output for test2 from a newer llama.cpp: the right nodes, mostly wrong arrows.
    let html = r#"<div data-bbox="96 291 658 567" data-label="Diagram"><pre>graph TD; search((search)) --&gt; teach((teach)); teach --&gt; learn((learn)); learn --&gt; search;</pre></div><div data-bbox="195 652 829 786" data-label="Diagram"><pre>graph LR; App((App)) --&gt; DB[(DB)]; DB --&gt; MCP[MCP];</pre></div>"#;
    let md = notedown_ocr::convert::chandra_page(html)
        .markdown
        .to_lowercase();
    let mut edges = mermaid_edges(&md);
    edges.sort();
    let mut want = vec![
        edge("search", "-->", "teach"),
        edge("teach", "-->", "learn"),
        edge("learn", "-->", "search"),
        edge("app", "-->", "db"),
        edge("db", "-->", "mcp"),
    ];
    want.sort();
    assert_eq!(edges, want, "{md}");
    assert!(!edges.contains(&edge("db", "<-->", "mcp")));
}

/// One edge as (from, arrow, to); a two-way edge keeps its ends in sorted order.
fn edge(a: &str, arrow: &str, b: &str) -> (String, String, String) {
    if arrow == "<-->" && b < a {
        (b.to_string(), arrow.to_string(), a.to_string())
    } else {
        (a.to_string(), arrow.to_string(), b.to_string())
    }
}

/// The label of a Mermaid node like `db[(DB)]`, `app((App))` or a bare id, lowercased.
fn node_label(node: &str) -> String {
    let node = node.trim();
    let id_end = node
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(node.len());
    let rest = node[id_end..].trim_matches(|c: char| "[](){}\"' ".contains(c));
    let label = if rest.is_empty() {
        &node[..id_end]
    } else {
        rest
    };
    label.to_lowercase()
}

/// All edges of all mermaid blocks, with node ids resolved to their labels.
fn mermaid_edges(md: &str) -> Vec<(String, String, String)> {
    let mut edges = Vec::new();
    for block in md.split("```mermaid").skip(1) {
        let body = block.split("```").next().unwrap_or("");
        let mut labels = std::collections::HashMap::new();
        let mut raw = Vec::new();
        for stmt in body.split(['\n', ';']) {
            let stmt = stmt.trim();
            if stmt.is_empty() || stmt.starts_with("graph") || stmt.starts_with("flowchart") {
                continue;
            }
            // Split on arrows, keeping the arrow kind between the nodes.
            let mut nodes = Vec::new();
            let mut arrows = Vec::new();
            let mut rest = stmt;
            loop {
                let next = ["<-->", "-->"]
                    .iter()
                    .filter_map(|a| rest.find(a).map(|i| (i, *a)))
                    .min_by_key(|(i, a)| (*i, std::cmp::Reverse(a.len())));
                match next {
                    Some((i, a)) => {
                        nodes.push(&rest[..i]);
                        arrows.push(a);
                        rest = &rest[i + a.len()..];
                    }
                    None => {
                        nodes.push(rest);
                        break;
                    }
                }
            }
            let ids: Vec<String> = nodes
                .iter()
                .map(|n| {
                    let n = n.trim();
                    let id_end = n
                        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .unwrap_or(n.len());
                    let id = n[..id_end].to_string();
                    if id_end < n.len() || !labels.contains_key(&id) {
                        labels.insert(id.clone(), node_label(n));
                    }
                    id
                })
                .collect();
            for (i, a) in arrows.iter().enumerate() {
                raw.push((ids[i].clone(), *a, ids[i + 1].clone()));
            }
        }
        for (a, arrow, b) in raw {
            edges.push(edge(&labels[&a], arrow, &labels[&b]));
        }
    }
    edges
}
