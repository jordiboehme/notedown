// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use scraper::{ElementRef, Html, Node};

use super::LayoutBlock;

/// Removes a leading `<think>...</think>` block (an unclosed one swallows the rest).
pub(crate) fn strip_think(html: &str) -> &str {
    let t = html.trim_start();
    match t.strip_prefix("<think>") {
        Some(rest) => rest.split_once("</think>").map_or("", |(_, after)| after),
        None => html,
    }
}

fn parse_bbox(s: &str) -> Option<[u32; 4]> {
    let nums: Vec<u32> = s
        .split_whitespace()
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    nums.try_into().ok()
}

/// Splits the fragment into layout blocks. Top-level elements with `data-label` are blocks;
/// consecutive other nodes form one `Text` block.
pub(crate) fn split(html: &str) -> Vec<LayoutBlock> {
    let fragment = Html::parse_fragment(strip_think(html));
    let mut blocks = Vec::new();
    let mut loose = String::new();
    let flush = |loose: &mut String, blocks: &mut Vec<LayoutBlock>| {
        if !loose.trim().is_empty() {
            blocks.push(LayoutBlock {
                label: "Text".into(),
                bbox: None,
                html: std::mem::take(loose),
            });
        }
        loose.clear();
    };
    for node in fragment.root_element().children() {
        match node.value() {
            Node::Element(el) if el.attr("data-label").is_some() => {
                flush(&mut loose, &mut blocks);
                let Some(er) = ElementRef::wrap(node) else {
                    continue;
                };
                blocks.push(LayoutBlock {
                    label: el.attr("data-label").unwrap_or("Text").to_string(),
                    bbox: el.attr("data-bbox").and_then(parse_bbox),
                    html: er.inner_html(),
                });
            }
            Node::Element(_) => {
                if let Some(er) = ElementRef::wrap(node) {
                    loose.push_str(&er.html());
                }
            }
            Node::Text(t) if !loose.is_empty() || !t.trim().is_empty() => {
                loose.push_str(
                    &t.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;"),
                );
            }
            _ => {}
        }
    }
    flush(&mut loose, &mut blocks);
    blocks
}
