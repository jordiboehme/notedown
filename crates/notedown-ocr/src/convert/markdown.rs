use scraper::{ElementRef, Html, Node};

use super::mermaid;

/// Marks a `<br/>` while rendering inline content.
const BR: char = '\u{E000}';

pub(crate) struct Rendered {
    pub text: String,
    pub mermaid_blocks: usize,
}

#[derive(Clone, Copy)]
struct Ctx {
    display_math: bool,
}

pub(crate) fn render_block(label: &str, html: &str) -> Rendered {
    let fragment = Html::parse_fragment(html);
    let root = fragment.root_element();
    match label {
        "Page-Header" | "Page-Footer" | "Blank-Page" => Rendered {
            text: String::new(),
            mermaid_blocks: 0,
        },
        "Diagram" | "Figure" | "Image" => render_diagram(root),
        _ => {
            let ctx = Ctx {
                display_math: label == "Equation-Block",
            };
            Rendered {
                text: block_parts(root, ctx).join("\n\n"),
                mermaid_blocks: 0,
            }
        }
    }
}

/// Collapses runs of blank lines and trims trailing spaces per line.
pub(crate) fn finalize(md: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in md.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        out.push_str(line);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn descendants_named<'a>(root: ElementRef<'a>, names: &[&str]) -> Vec<ElementRef<'a>> {
    root.descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| names.contains(&e.value().name()))
        .collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_diagram(root: ElementRef) -> Rendered {
    let code = descendants_named(root, &["pre", "code"])
        .first()
        .map(|e| e.text().collect::<String>());
    let all_text: String = root.text().collect();
    let source = [code.as_deref(), Some(all_text.as_str())]
        .into_iter()
        .flatten()
        .find(|t| mermaid::looks_like_mermaid(t))
        .map(mermaid::clean);
    let description = descendants_named(root, &["p"])
        .first()
        .map(|p| collapse_ws(&p.text().collect::<String>()))
        .filter(|d| !d.is_empty())
        .or_else(|| {
            descendants_named(root, &["img"])
                .iter()
                .filter_map(|i| i.value().attr("alt"))
                .map(collapse_ws)
                .find(|a| !a.is_empty())
        });
    match source {
        Some(src) => {
            let mut text = format!("```mermaid\n{}\n```", mermaid::pretty(&src));
            if let Some(d) = description {
                text.push_str(&format!("\n\n> {d}"));
            }
            Rendered {
                text,
                mermaid_blocks: 1,
            }
        }
        None => {
            let d = description.unwrap_or_else(|| collapse_ws(&all_text));
            Rendered {
                text: format!("> Figure: {d}").trim_end().to_string(),
                mermaid_blocks: 0,
            }
        }
    }
}

fn is_block_element(name: &str) -> bool {
    matches!(
        name,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "pre"
            | "table"
            | "div"
            | "section"
            | "article"
            | "blockquote"
            | "header"
            | "footer"
            | "figure"
    )
}

/// Renders the children of `parent` as Markdown blocks; loose inline content becomes paragraphs.
fn block_parts(parent: ElementRef, ctx: Ctx) -> Vec<String> {
    let mut out = Vec::new();
    let mut inline = String::new();
    let flush = |inline: &mut String, out: &mut Vec<String>| {
        let text = finish_inline(inline, "\\\n");
        if !text.is_empty() {
            out.push(text);
        }
        inline.clear();
    };
    for node in parent.children() {
        match node.value() {
            Node::Text(t) => push_text(&mut inline, t),
            Node::Element(el) => {
                let Some(er) = ElementRef::wrap(node) else {
                    continue;
                };
                if is_block_element(el.name()) {
                    flush(&mut inline, &mut out);
                    render_block_element(er, ctx, &mut out);
                } else {
                    inline_element(er, ctx, &mut inline);
                }
            }
            _ => {}
        }
    }
    flush(&mut inline, &mut out);
    out
}

fn render_block_element(el: ElementRef, ctx: Ctx, out: &mut Vec<String>) {
    let name = el.value().name();
    match name {
        "p" => {
            let text = finish_inline(&inline_string(el, ctx), "\\\n");
            if !text.is_empty() {
                out.push(text);
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name[1..].parse::<usize>().unwrap_or(1);
            let text = finish_inline(&inline_string(el, ctx), " ");
            if !text.is_empty() {
                out.push(format!("{} {}", "#".repeat(level), text));
            }
        }
        "ul" | "ol" => {
            let list = render_list(el, 0, ctx);
            if !list.is_empty() {
                out.push(list);
            }
        }
        "pre" => {
            let text: String = el.text().collect();
            let text = text.trim_matches('\n').trim_end();
            if !text.trim().is_empty() {
                let fence = if text.contains("```") { "````" } else { "```" };
                out.push(format!("{fence}\n{text}\n{fence}"));
            }
        }
        "table" => {
            let table = render_table(el, ctx);
            if !table.is_empty() {
                out.push(table);
            }
        }
        "blockquote" => {
            let inner = block_parts(el, ctx).join("\n\n");
            if !inner.is_empty() {
                let quoted: Vec<String> = inner
                    .lines()
                    .map(|l| {
                        if l.is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {l}")
                        }
                    })
                    .collect();
                out.push(quoted.join("\n"));
            }
        }
        _ => out.extend(block_parts(el, ctx)),
    }
}

/// Appends text with whitespace runs collapsed, avoiding doubled spaces across node boundaries.
fn push_text(out: &mut String, text: &str) {
    let mut s = collapse_inner(text);
    if (out.is_empty() || out.ends_with(' ') || out.ends_with(BR)) && s.starts_with(' ') {
        s.remove(0);
    }
    out.push_str(&s);
}

/// Collapses whitespace runs to single spaces but keeps one leading/trailing space.
fn collapse_inner(text: &str) -> String {
    let core = collapse_ws(text);
    if core.is_empty() {
        return if text.is_empty() {
            String::new()
        } else {
            " ".to_string()
        };
    }
    let mut s = String::new();
    if text.starts_with(char::is_whitespace) {
        s.push(' ');
    }
    s.push_str(&core);
    if text.ends_with(char::is_whitespace) {
        s.push(' ');
    }
    s
}

fn finish_inline(buf: &str, joiner: &str) -> String {
    buf.split(BR)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(joiner)
}

fn inline_nodes(parent: ElementRef, ctx: Ctx, out: &mut String) {
    for node in parent.children() {
        match node.value() {
            Node::Text(t) => push_text(out, t),
            Node::Element(_) => {
                if let Some(er) = ElementRef::wrap(node) {
                    inline_element(er, ctx, out);
                }
            }
            _ => {}
        }
    }
}

fn inline_string(el: ElementRef, ctx: Ctx) -> String {
    let mut buf = String::new();
    inline_nodes(el, ctx, &mut buf);
    buf
}

fn wrap_inline(out: &mut String, open: &str, close: &str, inner: &str) {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        if !inner.is_empty() && !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
        return;
    }
    if inner.starts_with(' ') && !out.ends_with(' ') && !out.is_empty() {
        out.push(' ');
    }
    out.push_str(open);
    out.push_str(trimmed);
    out.push_str(close);
    if inner.ends_with(' ') {
        out.push(' ');
    }
}

fn inline_element(el: ElementRef, ctx: Ctx, out: &mut String) {
    match el.value().name() {
        "br" => out.push(BR),
        "b" | "strong" => wrap_inline(out, "**", "**", &inline_string(el, ctx)),
        "i" | "em" => wrap_inline(out, "*", "*", &inline_string(el, ctx)),
        "del" | "s" | "strike" => wrap_inline(out, "~~", "~~", &inline_string(el, ctx)),
        "sup" => wrap_inline(out, "<sup>", "</sup>", &inline_string(el, ctx)),
        "sub" => wrap_inline(out, "<sub>", "</sub>", &inline_string(el, ctx)),
        "code" => {
            let text: String = el.text().collect();
            wrap_inline(out, "`", "`", &collapse_inner(&text));
        }
        "a" => {
            let inner = inline_string(el, ctx);
            match el.value().attr("href") {
                Some(href) if !inner.trim().is_empty() => {
                    wrap_inline(out, "[", &format!("]({href})"), &inner)
                }
                _ => out.push_str(&inner),
            }
        }
        "math" => {
            let text: String = el.text().collect();
            let text = text.trim();
            if !text.is_empty() {
                let display = ctx.display_math || el.value().attr("display").is_some();
                let d = if display { "$$" } else { "$" };
                out.push_str(&format!("{d}{text}{d}"));
            }
        }
        "img" => {
            if let Some(alt) = el.value().attr("alt") {
                push_text(out, alt);
            }
        }
        "input" if el.value().attr("type") == Some("checkbox") => {
            out.push_str(if el.value().attr("checked").is_some() {
                "[x] "
            } else {
                "[ ] "
            });
        }
        // Block-level elements met inside inline content: keep their text, separated by spaces.
        name if is_block_element(name) => {
            if !out.is_empty() && !out.ends_with(' ') && !out.ends_with(BR) {
                out.push(' ');
            }
            inline_nodes(el, ctx, out);
            if !out.ends_with(' ') {
                out.push(' ');
            }
        }
        _ => inline_nodes(el, ctx, out),
    }
}

/// Strips the device's bullet and checkbox glyphs from the start of a list item.
fn strip_glyph(text: &str) -> String {
    const BULLETS: &[char] = &['•', '○', '◦', '▪', '-', '*'];
    const OPEN: &[char] = &['❑', '☐', '□'];
    const DONE: &[char] = &['☑', '☒', '✓', '✔', '■'];
    let Some(first) = text.chars().next() else {
        return String::new();
    };
    let rest = &text[first.len_utf8()..];
    if BULLETS.contains(&first) && (rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        rest.trim_start().to_string()
    } else if OPEN.contains(&first) {
        format!("[ ] {}", rest.trim_start())
    } else if DONE.contains(&first) {
        format!("[x] {}", rest.trim_start())
    } else {
        text.to_string()
    }
}

fn render_list(list: ElementRef, depth: usize, ctx: Ctx) -> String {
    let ordered = list.value().name() == "ol";
    let indent = "  ".repeat(depth);
    let mut lines: Vec<String> = Vec::new();
    let mut n = 0;
    for child in list.children().filter_map(ElementRef::wrap) {
        if child.value().name() != "li" {
            continue;
        }
        n += 1;
        let marker = if ordered {
            format!("{n}. ")
        } else {
            "- ".to_string()
        };
        let mut buf = String::new();
        let mut nested = Vec::new();
        for node in child.children() {
            match ElementRef::wrap(node) {
                Some(e) if matches!(e.value().name(), "ul" | "ol") => nested.push(e),
                Some(e) => inline_element(e, ctx, &mut buf),
                None => {
                    if let Node::Text(t) = node.value() {
                        push_text(&mut buf, t);
                    }
                }
            }
        }
        let text = strip_glyph(&finish_inline(&buf, "\n"));
        let mut item_lines = text.lines();
        lines.push(
            format!("{indent}{marker}{}", item_lines.next().unwrap_or(""))
                .trim_end()
                .to_string(),
        );
        for l in item_lines {
            lines.push(format!("{indent}  {l}"));
        }
        for sub in nested {
            let rendered = render_list(sub, depth + 1, ctx);
            if !rendered.is_empty() {
                lines.push(rendered);
            }
        }
    }
    lines.join("\n")
}

fn render_table(table: ElementRef, ctx: Ctx) -> String {
    let cells = descendants_named(table, &["td", "th"]);
    if cells
        .iter()
        .any(|c| c.value().attr("colspan").is_some() || c.value().attr("rowspan").is_some())
    {
        return table.html();
    }
    let rows: Vec<Vec<String>> = descendants_named(table, &["tr"])
        .into_iter()
        .map(|tr| {
            tr.children()
                .filter_map(ElementRef::wrap)
                .filter(|c| matches!(c.value().name(), "td" | "th"))
                .map(|c| finish_inline(&inline_string(c, ctx), " ").replace('|', "\\|"))
                .collect()
        })
        .filter(|r: &Vec<String>| !r.is_empty())
        .collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return String::new();
    }
    let line = |row: &[String]| {
        let mut cells: Vec<&str> = row.iter().map(String::as_str).collect();
        cells.resize(cols, "");
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![
        line(&rows[0]),
        format!("| {} |", vec!["---"; cols].join(" | ")),
    ];
    out.extend(rows[1..].iter().map(|r| line(r)));
    out.join("\n")
}
