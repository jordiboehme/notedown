//! The Markdown document: frontmatter and page sections.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub source: String,
    pub source_sha256: String,
    pub kind: String,
    pub file_id: Option<String>,
    pub pages: usize,
    pub model: String,
    pub converted_at: String,
    pub generator: String,
    pub keywords: Vec<String>,
    pub links: Vec<FrontmatterLink>,
}

/// A link to another note, kept for a later import (`page` is 1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterLink {
    pub page: usize,
    pub target_file_id: String,
    /// The device path of the target (`LINKFILE`), the stable identity next to the id.
    pub target_path: String,
    /// Relative path of the generated Markdown file, only when the target resolved.
    pub target_md: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PageOut {
    /// 1-based page number.
    pub number: usize,
    pub page_id: Option<String>,
    pub image_rel: Option<String>,
    pub markdown: String,
    pub truncated: bool,
    pub empty: bool,
}

fn needs_quotes(v: &str) -> bool {
    if v.is_empty() || v != v.trim() {
        return true;
    }
    if v.chars()
        .any(|c| c.is_control() || matches!(c, ':' | '#' | '"' | '\\'))
    {
        return true;
    }
    let first = v.chars().next().unwrap_or(' ');
    if "-?,[]{}&*!|>'%@`~".contains(first) {
        return true;
    }
    let lower = v.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "null" | "true" | "false" | "yes" | "no" | "on" | "off"
    ) || v.parse::<f64>().is_ok()
}

fn scalar(v: &str) -> String {
    if needs_quotes(v) {
        quote(v)
    } else {
        v.to_string()
    }
}

fn quote(v: &str) -> String {
    let mut s = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04X}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Like [`scalar`], but also quotes what would end a flow map entry.
fn flow_scalar(v: &str) -> String {
    if v.chars().any(|c| matches!(c, ',' | '{' | '}' | '[' | ']')) {
        quote(v)
    } else {
        scalar(v)
    }
}

/// Parses `{ key: value, ... }` with plain or double-quoted values.
fn parse_flow_map(s: &str) -> Option<Vec<(String, String)>> {
    let inner = s.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut out = Vec::new();
    let mut rest = inner.trim_start();
    while !rest.is_empty() {
        let (key, after) = rest.split_once(':')?;
        let after = after.trim_start();
        let (raw, remaining) = if after.starts_with('"') {
            let mut end = None;
            let mut escaped = false;
            for (i, c) in after.char_indices().skip(1) {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    end = Some(i);
                    break;
                }
            }
            let end = end?;
            (&after[..=end], &after[end + 1..])
        } else {
            match after.find(',') {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            }
        };
        out.push((key.trim().to_string(), unscalar(raw)?));
        let remaining = remaining.trim_start();
        rest = match remaining.strip_prefix(',') {
            Some(r) => r.trim_start(),
            None if remaining.is_empty() => "",
            None => return None,
        };
    }
    Some(out)
}

fn unscalar(v: &str) -> Option<String> {
    let v = v.trim();
    let Some(inner) = v.strip_prefix('"') else {
        return Some(v.to_string());
    };
    let inner = inner.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                if hex.len() != 4 {
                    return None;
                }
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

impl Frontmatter {
    /// The frontmatter block including both `---` lines and a final newline.
    pub fn render(&self) -> String {
        let mut s = String::from("---\n");
        s.push_str(&format!("source: {}\n", scalar(&self.source)));
        s.push_str(&format!("source_sha256: {}\n", scalar(&self.source_sha256)));
        s.push_str(&format!("kind: {}\n", scalar(&self.kind)));
        if let Some(id) = &self.file_id {
            s.push_str(&format!("file_id: {}\n", scalar(id)));
        }
        s.push_str(&format!("pages: {}\n", self.pages));
        s.push_str(&format!("model: {}\n", scalar(&self.model)));
        s.push_str(&format!("converted_at: {}\n", scalar(&self.converted_at)));
        s.push_str(&format!("generator: {}\n", scalar(&self.generator)));
        if self.keywords.is_empty() {
            s.push_str("keywords: []\n");
        } else {
            s.push_str("keywords:\n");
            for k in &self.keywords {
                s.push_str(&format!("  - {}\n", scalar(k)));
            }
        }
        if !self.links.is_empty() {
            s.push_str("links:\n");
            for l in &self.links {
                let md = l
                    .target_md
                    .as_ref()
                    .map(|m| format!(", target_md: {}", flow_scalar(m)))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "  - {{ page: {}, target_file_id: {}, target_path: {}{md} }}\n",
                    l.page,
                    flow_scalar(&l.target_file_id),
                    flow_scalar(&l.target_path)
                ));
            }
        }
        s.push_str("---\n");
        s
    }

    /// Reads back what [`Frontmatter::render`] writes; `None` on anything unexpected.
    pub fn parse(md: &str) -> Option<Frontmatter> {
        let mut lines = md.lines();
        if lines.next()? != "---" {
            return None;
        }
        let mut source = None;
        let mut sha = None;
        let mut kind = None;
        let mut file_id = None;
        let mut pages = None;
        let mut model = None;
        let mut converted_at = None;
        let mut generator = None;
        let mut keywords = None;
        let mut links = Vec::new();
        let mut closed = false;
        while let Some(line) = lines.next() {
            if line == "---" {
                closed = true;
                break;
            }
            let (key, value) = line.split_once(':')?;
            let value = value.trim();
            match key {
                "source" => source = Some(unscalar(value)?),
                "source_sha256" => sha = Some(unscalar(value)?),
                "kind" => kind = Some(unscalar(value)?),
                "file_id" => file_id = Some(unscalar(value)?),
                "pages" => pages = Some(value.parse::<usize>().ok()?),
                "model" => model = Some(unscalar(value)?),
                "converted_at" => converted_at = Some(unscalar(value)?),
                "generator" => generator = Some(unscalar(value)?),
                "keywords" => {
                    let mut list = Vec::new();
                    if value != "[]" {
                        if !value.is_empty() {
                            return None;
                        }
                        let mut rest = lines.clone();
                        while let Some(item) = rest.clone().next() {
                            let Some(v) = item.strip_prefix("  - ") else {
                                break;
                            };
                            list.push(unscalar(v)?);
                            rest.next();
                        }
                        lines = rest;
                    }
                    keywords = Some(list);
                }
                "links" => {
                    if !value.is_empty() {
                        return None;
                    }
                    let mut rest = lines.clone();
                    while let Some(item) = rest.clone().next() {
                        let Some(v) = item.strip_prefix("  - ") else {
                            break;
                        };
                        let fields = parse_flow_map(v)?;
                        let get =
                            |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                        links.push(FrontmatterLink {
                            page: get("page")?.parse().ok()?,
                            target_file_id: get("target_file_id")?,
                            target_path: get("target_path")?,
                            target_md: get("target_md"),
                        });
                        rest.next();
                    }
                    lines = rest;
                }
                _ => return None,
            }
        }
        if !closed {
            return None;
        }
        Some(Frontmatter {
            source: source?,
            source_sha256: sha?,
            kind: kind?,
            file_id,
            pages: pages?,
            model: model?,
            converted_at: converted_at?,
            generator: generator?,
            keywords: keywords?,
            links,
        })
    }
}

/// Keeps a value from the note safe inside an HTML comment: only letters, digits, `_`,
/// `.`, `:` and single `-` stay, so it can never contain `--` or `>`.
pub fn comment_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let keep = c.is_ascii_alphanumeric()
            || matches!(c, '_' | '.' | ':')
            || (c == '-' && !out.ends_with('-'));
        if keep {
            out.push(c);
        }
    }
    out
}

/// The whole Markdown file.
pub fn render_document(fm: &Frontmatter, pages: &[PageOut]) -> String {
    let mut doc = fm.render();
    for p in pages {
        doc.push_str(&format!(
            "\n<a id=\"page-{}\"></a>\n## Page {}\n",
            p.number, p.number
        ));
        if let Some(id) = &p.page_id {
            doc.push_str(&format!("<!-- page_id: {} -->\n", comment_safe(id)));
        }
        if p.truncated {
            doc.push_str("<!-- notedown: truncated -->\n");
        }
        if let Some(img) = &p.image_rel {
            doc.push_str(&format!("\n![Page {}]({img})\n", p.number));
        }
        if !p.markdown.trim().is_empty() {
            doc.push_str(&format!("\n{}\n", p.markdown.trim_end()));
        }
    }
    doc
}
