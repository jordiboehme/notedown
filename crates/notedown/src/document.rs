//! The Markdown document: frontmatter and page sections.
//!
//! The frontmatter is an Open Knowledge Format (OKF v0.2) concept header with notedown's
//! own keys after the OKF ones. `type` is always `note` and `tags` always `[supernote]`.

/// Which header a parsed file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderFormat {
    /// The Open Knowledge Format header that [`Frontmatter::render`] writes.
    Okf,
    /// The header of notedown before OKF (`source`, `kind`, `converted_at`, `generator`).
    Old,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// The source file name without its extension.
    pub title: String,
    pub description: String,
    /// The source file, relative to the Markdown file's folder, with `/` separators.
    pub resource: String,
    /// `generated.by`: `notedown/<version>`.
    pub generated_by: String,
    /// `generated.model`: the label of the OCR model.
    pub generated_model: String,
    /// `generated.at`: when the text was read, RFC 3339 UTC.
    pub generated_at: String,
    /// Local calendar date of the source file's modification time (`YYYY-MM-DD`).
    pub source_date: String,
    /// The source file's modification time, RFC 3339 UTC (`sources[0].last_modified`).
    pub last_modified: String,
    /// `draft` when a page hit the token limit, else `stable`.
    pub status: String,
    pub source_sha256: String,
    pub file_id: Option<String>,
    pub pages: usize,
    pub keywords: Vec<String>,
    pub links: Vec<FrontmatterLink>,
    pub digests: Vec<FrontmatterDigest>,
    /// Not written; set by [`Frontmatter::parse`].
    pub format: HeaderFormat,
}

/// The comment a page that hit the token limit carries.
pub const TRUNCATED_MARK: &str = "<!-- notedown: truncated -->";

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

/// A text box that carries a note digest (`page` is 1-based). The digest's own
/// annotation is not in the `.note` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterDigest {
    pub page: usize,
    pub digest_id: String,
    pub text: String,
    /// `left, top, width, height` in page pixels.
    pub rect: [u32; 4],
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

/// `YYYY-MM-DDTHH:MM:SS` with optional fraction and `Z` or `+HH:MM`: a YAML timestamp,
/// written plain like the examples of the OKF spec.
fn is_timestamp(v: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some((date, time)) = v.split_once('T') else {
        return false;
    };
    let date_ok = date.len() == 10
        && date.split('-').map(str::len).eq([4, 2, 2])
        && date.split('-').all(digits);
    let (clock, zone) = match time.strip_suffix('Z') {
        Some(c) => (c, None),
        None if time.len() > 6 => {
            let (c, z) = time.split_at(time.len() - 6);
            (c, Some(z))
        }
        None => return false,
    };
    let zone_ok = zone.is_none_or(|z| {
        let (sign, rest) = z.split_at(1);
        matches!(sign, "+" | "-")
            && rest.split(':').map(str::len).eq([2, 2])
            && rest.split(':').all(digits)
    });
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, "0"));
    date_ok
        && zone_ok
        && digits(fraction)
        && hms.split(':').map(str::len).eq([2, 2, 2])
        && hms.split(':').all(digits)
}

fn needs_quotes(v: &str) -> bool {
    if v.is_empty() || v != v.trim() {
        return true;
    }
    if is_timestamp(v) {
        return false;
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

/// Reads the `  - { ... }` items that follow a list key; `None` when one is not a flow map.
fn flow_items(lines: &mut std::str::Lines) -> Option<Vec<Vec<(String, String)>>> {
    let mut items = Vec::new();
    let mut rest = lines.clone();
    while let Some(item) = rest.clone().next() {
        let Some(v) = item.strip_prefix("  - ") else {
            break;
        };
        items.push(parse_flow_map(v)?);
        rest.next();
    }
    *lines = rest;
    Some(items)
}

fn field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields
        .iter()
        .find(|(n, _)| n == key)
        .map(|(_, v)| v.clone())
}

/// The file name of a `/` path without its extension.
fn stem_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name.to_string(),
    }
}

impl Frontmatter {
    /// The frontmatter block including both `---` lines and a final newline.
    pub fn render(&self) -> String {
        let mut s = String::from("---\n");
        s.push_str("type: note\n");
        s.push_str(&format!("title: {}\n", scalar(&self.title)));
        s.push_str(&format!("description: {}\n", scalar(&self.description)));
        s.push_str(&format!("resource: {}\n", scalar(&self.resource)));
        s.push_str("tags: [supernote]\n");
        s.push_str(&format!(
            "generated: {{ by: {}, model: {}, at: {} }}\n",
            flow_scalar(&self.generated_by),
            flow_scalar(&self.generated_model),
            flow_scalar(&self.generated_at)
        ));
        s.push_str(&format!("source_date: {}\n", scalar(&self.source_date)));
        s.push_str(&format!(
            "sources:\n  - {{ id: note, resource: {}, last_modified: {} }}\n",
            flow_scalar(&self.resource),
            flow_scalar(&self.last_modified)
        ));
        s.push_str(&format!("status: {}\n", scalar(&self.status)));
        s.push_str(&format!("source_sha256: {}\n", scalar(&self.source_sha256)));
        if let Some(id) = &self.file_id {
            s.push_str(&format!("file_id: {}\n", scalar(id)));
        }
        s.push_str(&format!("pages: {}\n", self.pages));
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
        if !self.digests.is_empty() {
            s.push_str("digests:\n");
            for d in &self.digests {
                s.push_str(&format!(
                    "  - {{ page: {}, digest_id: {}, text: {}, rect: \"{},{},{},{}\" }}\n",
                    d.page,
                    flow_scalar(&d.digest_id),
                    flow_scalar(&d.text),
                    d.rect[0],
                    d.rect[1],
                    d.rect[2],
                    d.rect[3]
                ));
            }
        }
        s.push_str("---\n");
        s
    }

    /// Reads back what [`Frontmatter::render`] writes, and the header notedown wrote
    /// before (see [`HeaderFormat::Old`]); `None` on anything unexpected.
    pub fn parse(md: &str) -> Option<Frontmatter> {
        let mut lines = md.lines();
        if lines.next()? != "---" {
            return None;
        }
        // the Open Knowledge Format keys
        let mut doc_type = None;
        let mut title = None;
        let mut description = None;
        let mut resource = None;
        let mut tags = false;
        let mut generated = None;
        let mut source_date = None;
        let mut sources = None;
        let mut status = None;
        // the keys of the old header
        let mut source = None;
        let mut kind = None;
        let mut model = None;
        let mut converted_at = None;
        let mut generator = None;
        // both
        let mut sha = None;
        let mut file_id = None;
        let mut pages = None;
        let mut keywords = None;
        let mut links = Vec::new();
        let mut digests = Vec::new();
        let mut closed = false;
        while let Some(line) = lines.next() {
            if line == "---" {
                closed = true;
                break;
            }
            let (key, value) = line.split_once(':')?;
            let value = value.trim();
            match key {
                "type" => doc_type = Some(unscalar(value)?),
                "title" => title = Some(unscalar(value)?),
                "description" => description = Some(unscalar(value)?),
                "resource" => resource = Some(unscalar(value)?),
                "tags" => tags = value.starts_with('[') && value.ends_with(']'),
                "generated" => generated = Some(parse_flow_map(value)?),
                "source_date" => source_date = Some(unscalar(value)?),
                "sources" => {
                    if !value.is_empty() {
                        return None;
                    }
                    sources = flow_items(&mut lines)?.into_iter().next();
                }
                "status" => status = Some(unscalar(value)?),
                "source" => source = Some(unscalar(value)?),
                "kind" => kind = Some(unscalar(value)?),
                "model" => model = Some(unscalar(value)?),
                "converted_at" => converted_at = Some(unscalar(value)?),
                "generator" => generator = Some(unscalar(value)?),
                "source_sha256" => sha = Some(unscalar(value)?),
                "file_id" => file_id = Some(unscalar(value)?),
                "pages" => pages = Some(value.parse::<usize>().ok()?),
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
                    for fields in flow_items(&mut lines)? {
                        links.push(FrontmatterLink {
                            page: field(&fields, "page")?.parse().ok()?,
                            target_file_id: field(&fields, "target_file_id")?,
                            target_path: field(&fields, "target_path")?,
                            target_md: field(&fields, "target_md"),
                        });
                    }
                }
                "digests" => {
                    if !value.is_empty() {
                        return None;
                    }
                    for fields in flow_items(&mut lines)? {
                        let mut rect = [0u32; 4];
                        let raw = field(&fields, "rect")?;
                        let mut parts = raw.split(',');
                        for slot in &mut rect {
                            *slot = parts.next()?.trim().parse().ok()?;
                        }
                        if parts.next().is_some() {
                            return None;
                        }
                        digests.push(FrontmatterDigest {
                            page: field(&fields, "page")?.parse().ok()?,
                            digest_id: field(&fields, "digest_id")?,
                            text: field(&fields, "text")?,
                            rect,
                        });
                    }
                }
                _ => return None,
            }
        }
        if !closed {
            return None;
        }
        let old_keys = source.is_some()
            || kind.is_some()
            || model.is_some()
            || converted_at.is_some()
            || generator.is_some();
        let new_keys = doc_type.is_some()
            || title.is_some()
            || description.is_some()
            || resource.is_some()
            || tags
            || generated.is_some()
            || source_date.is_some()
            || sources.is_some()
            || status.is_some();
        let shared = (sha?, pages?, keywords?);
        let fm = match (old_keys, new_keys) {
            (false, true) => {
                let generated = generated?;
                let sources = sources.unwrap_or_default();
                if doc_type.is_none_or(|t| t.is_empty()) {
                    return None;
                }
                Frontmatter {
                    title: title.unwrap_or_default(),
                    description: description.unwrap_or_default(),
                    resource: resource.unwrap_or_default(),
                    generated_by: field(&generated, "by")?,
                    generated_model: field(&generated, "model").unwrap_or_default(),
                    generated_at: field(&generated, "at")?,
                    source_date: source_date.unwrap_or_default(),
                    last_modified: field(&sources, "last_modified").unwrap_or_default(),
                    status: status.unwrap_or_else(|| "stable".into()),
                    source_sha256: shared.0,
                    file_id,
                    pages: shared.1,
                    keywords: shared.2,
                    links,
                    digests,
                    format: HeaderFormat::Okf,
                }
            }
            (true, false) => {
                let source = source?;
                kind?;
                let generator = generator?;
                Frontmatter {
                    title: stem_of(&source),
                    description: String::new(),
                    resource: source,
                    generated_by: match generator.strip_prefix("notedown ") {
                        Some(version) => format!("notedown/{version}"),
                        None => generator,
                    },
                    generated_model: model?,
                    generated_at: converted_at?,
                    source_date: String::new(),
                    last_modified: String::new(),
                    status: "stable".into(),
                    source_sha256: shared.0,
                    file_id,
                    pages: shared.1,
                    keywords: shared.2,
                    links,
                    digests,
                    format: HeaderFormat::Old,
                }
            }
            _ => return None,
        };
        Some(fm)
    }
}

/// The Markdown body after the frontmatter block, byte for byte.
pub fn body_of(md: &str) -> Option<&str> {
    let rest = md.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some(&rest[end + "\n---\n".len()..])
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
            doc.push_str(TRUNCATED_MARK);
            doc.push('\n');
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
