// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

const KEYWORDS: &[&str] = &[
    "graph",
    "flowchart",
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "stateDiagram-v2",
    "erDiagram",
    "journey",
    "gantt",
    "pie",
    "mindmap",
    "timeline",
    "gitGraph",
    "quadrantChart",
    "requirementDiagram",
    "C4Context",
    "sankey-beta",
    "xychart-beta",
    "block-beta",
];

/// True when the text starts with a Mermaid diagram keyword (after fence noise is removed).
pub fn looks_like_mermaid(text: &str) -> bool {
    let cleaned = clean(text);
    let first = cleaned
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or("");
    KEYWORDS.contains(&first)
}

/// Strips a leading "```mermaid" or "mermaid" line, a trailing "```" line and surrounding blank space.
pub(crate) fn clean(text: &str) -> String {
    let mut lines: Vec<&str> = text.trim().lines().collect();
    if let Some(first) = lines.first() {
        let f = first.trim();
        if f.starts_with("```") || (f.eq_ignore_ascii_case("mermaid") && lines.len() > 1) {
            lines.remove(0);
        }
    }
    if lines.last().is_some_and(|l| l.trim() == "```") {
        lines.pop();
    }
    lines.join("\n").trim().to_string()
}

/// Pretty-prints one-line `;` separated Mermaid: first statement alone, the rest indented.
/// Lines with a `;` inside quotes or brackets, and multi-line sources, stay as they are.
pub(crate) fn pretty(src: &str) -> String {
    if src.contains('\n') || !src.contains(';') {
        return src.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut depth = 0usize;
    for ch in src.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                cur.push(ch);
            }
            '[' | '(' | '{' if !quoted => {
                depth += 1;
                cur.push(ch);
            }
            ']' | ')' | '}' if !quoted => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ';' if quoted || depth > 0 => return src.to_string(),
            ';' => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    parts.push(cur);
    let mut parts = parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty());
    let mut out = String::new();
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for p in parts {
        out.push_str("\n    ");
        out.push_str(p);
    }
    out
}
