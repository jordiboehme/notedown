//! The Markdown document: frontmatter and page sections.
//!
//! The frontmatter is an Open Knowledge Format (OKF v0.2) concept header with notedown's
//! own keys after the OKF ones. `type` is always `note` and `tags` always `[supernote]`.
//! Keys that another tool added are kept and written after notedown's keys.

use crate::yaml::{Block, Node, top_level};

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
    /// Local calendar date of the source file's modification time (`YYYY-MM-DD`); left
    /// out when the time cannot be read.
    pub source_date: Option<String>,
    /// The source file's modification time, RFC 3339 UTC (`sources[0].last_modified`).
    pub last_modified: Option<String>,
    /// `draft` when a page hit the token limit, else `stable`.
    pub status: String,
    pub source_sha256: String,
    pub file_id: Option<String>,
    pub pages: usize,
    pub keywords: Vec<String>,
    pub links: Vec<FrontmatterLink>,
    pub digests: Vec<FrontmatterDigest>,
    /// Top-level keys notedown does not know, as they were written (each ending with a
    /// newline). OKF asks to keep them.
    pub extra: Vec<String>,
    /// Not written; set by [`Frontmatter::parse`].
    pub format: HeaderFormat,
    /// Write the header (and a new body) with CRLF line endings, like the file it replaces.
    pub crlf: bool,
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

/// A double-quoted YAML string. Control characters, the Unicode line and paragraph
/// separators and the non-characters U+FFFE and U+FFFF are written as `\uXXXX`, since YAML
/// readers refuse them raw or read them as line breaks.
fn quote(v: &str) -> String {
    let mut s = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if c.is_control()
                || matches!(c, '\u{2028}' | '\u{2029}' | '\u{FFFE}' | '\u{FFFF}') =>
            {
                s.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// A fixed word of notedown (`note`, `stable`, `notedown/0.1.0`): plain when it is a
/// simple token that no YAML reader takes for anything but a string, else quoted.
fn token(v: &str) -> String {
    let simple = v.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '+' | '-'))
        && !matches!(
            v.to_ascii_lowercase().as_str(),
            "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if simple { v.to_string() } else { quote(v) }
}

fn number(s: &str, len: usize) -> Option<u32> {
    (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

/// `YYYY-MM-DD` that is a real calendar date.
fn is_date(v: &str) -> bool {
    let mut parts = v.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let (Some(y), Some(m), Some(d)) = (number(y, 4), number(m, 2), number(d, 2)) else {
        return false;
    };
    let Ok(month) = time::Month::try_from(m as u8) else {
        return false;
    };
    time::Date::from_calendar_date(y as i32, month, d as u8).is_ok()
}

/// An RFC 3339 date and time that is real: `YYYY-MM-DDTHH:MM:SS`, an optional fraction
/// and `Z` or `+HH:MM`.
fn is_datetime(v: &str) -> bool {
    let Some((date, rest)) = v.split_once('T') else {
        return false;
    };
    let (clock, zone_ok) = if let Some(c) = rest.strip_suffix('Z') {
        (c, true)
    } else if rest.len() > 6 {
        let (c, z) = rest.split_at(rest.len() - 6);
        let ok = z.starts_with(['+', '-'])
            && z[1..].split_once(':').is_some_and(|(h, m)| {
                number(h, 2).is_some_and(|h| h < 24) && number(m, 2).is_some_and(|m| m < 60)
            });
        (c, ok)
    } else {
        return false;
    };
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, "0"));
    let mut parts = hms.split(':');
    let (Some(h), Some(m), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let clock_ok = matches!(
        (number(h, 2), number(m, 2), number(s, 2)),
        (Some(h), Some(m), Some(s)) if h < 24 && m < 60 && s < 61
    );
    is_date(date)
        && zone_ok
        && clock_ok
        && !fraction.is_empty()
        && fraction.bytes().all(|b| b.is_ascii_digit())
}

/// A date or time value: plain when it is a real date or date and time, else quoted.
fn when(v: &str) -> String {
    if is_date(v) || is_datetime(v) {
        v.to_string()
    } else {
        quote(v)
    }
}

/// The keys notedown reads; every other top-level key is kept as it is.
const KNOWN_KEYS: &[&str] = &[
    "type",
    "title",
    "description",
    "resource",
    "tags",
    "generated",
    "source_date",
    "sources",
    "status",
    "source",
    "kind",
    "model",
    "converted_at",
    "generator",
    "source_sha256",
    "file_id",
    "pages",
    "keywords",
    "links",
    "digests",
];

/// The front matter block of a Markdown file, split into top-level keys.
#[derive(Debug, Clone)]
pub struct Header {
    block: Block,
    crlf: bool,
}

/// The lines between the opening and the closing `---`, the byte offset of the body and
/// whether the opening line ends with CRLF.
fn split_block(md: &str) -> Option<(Vec<&str>, usize, bool)> {
    let mut offset = 0;
    let mut lines = Vec::new();
    let mut crlf = None;
    for raw in md.split_inclusive('\n') {
        offset += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if crlf.is_none() {
            if line != "---" {
                return None;
            }
            crlf = Some(raw.ends_with("\r\n"));
            continue;
        }
        if line == "---" {
            return Some((lines, offset, crlf.unwrap_or(false)));
        }
        lines.push(line);
    }
    None
}

impl Header {
    /// `None` when the file has no front matter block or it cannot be read.
    pub fn read(md: &str) -> Option<Header> {
        let (lines, _, crlf) = split_block(md)?;
        Some(Header {
            block: top_level(&lines)?,
            crlf,
        })
    }

    /// The header uses CRLF line endings.
    pub fn crlf(&self) -> bool {
        self.crlf
    }

    fn get(&self, key: &str) -> Option<&Node> {
        self.block
            .entries
            .iter()
            .rev()
            .find(|e| e.key == key)
            .map(|e| &e.value)
    }

    fn text(&self, key: &str) -> Option<String> {
        self.get(key).and_then(Node::as_str).map(str::to_string)
    }

    /// `Some(None)` when the key is missing, `None` when it holds no text.
    fn optional(&self, key: &str) -> Option<Option<String>> {
        match self.get(key) {
            None => Some(None),
            Some(n) => n.as_str().map(|s| Some(s.to_string())),
        }
    }

    /// Written by notedown: `generated.by` is `notedown/...` or the old `generator` is
    /// `notedown ...`.
    pub fn is_notedown(&self) -> bool {
        self.get("generated")
            .and_then(|g| g.get("by"))
            .and_then(Node::as_str)
            .is_some_and(|by| by.starts_with("notedown/"))
            || self
                .text("generator")
                .is_some_and(|g| g.starts_with("notedown "))
    }

    /// The top-level keys notedown does not know, as written, each with the comment lines
    /// before it. Comment lines before one of notedown's keys and after the last key come
    /// in their order too, since their place among notedown's keys cannot be kept.
    pub fn extra(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.block.entries {
            if !KNOWN_KEYS.contains(&e.key.as_str()) {
                out.push(format!("{}{}", e.lead, e.raw));
            } else if !e.lead.is_empty() {
                out.push(e.lead.clone());
            }
        }
        if !self.block.trailing.is_empty() {
            out.push(self.block.trailing.clone());
        }
        out
    }
}

/// The file name of a `/` path without its extension.
fn stem_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name.to_string(),
    }
}

fn texts(node: Option<&Node>) -> Option<Vec<String>> {
    match node {
        None => Some(Vec::new()),
        Some(n) => n
            .as_list()?
            .iter()
            .map(|i| i.as_str().map(str::to_string))
            .collect(),
    }
}

fn items<T>(node: Option<&Node>, read: impl Fn(&Node) -> Option<T>) -> Option<Vec<T>> {
    match node {
        None => Some(Vec::new()),
        Some(n) => n.as_list()?.iter().map(read).collect(),
    }
}

fn field(node: &Node, key: &str) -> Option<String> {
    node.get(key).and_then(Node::as_str).map(str::to_string)
}

/// `Some(None)` when the key is missing, `None` when it holds no text.
fn optional_field(node: &Node, key: &str) -> Option<Option<String>> {
    match node.get(key) {
        None => Some(None),
        Some(n) => n.as_str().map(|s| Some(s.to_string())),
    }
}

fn rect_of(node: &Node) -> Option<[u32; 4]> {
    let parts: Vec<String> = match node {
        Node::Scalar(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        Node::List(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_string))
            .collect::<Option<_>>()?,
        Node::Map(_) | Node::Unknown => return None,
    };
    let mut rect = [0u32; 4];
    if parts.len() != 4 {
        return None;
    }
    for (slot, p) in rect.iter_mut().zip(&parts) {
        *slot = p.parse().ok()?;
    }
    Some(rect)
}

/// `+00:00` and `Z` name the same offset; other tools write either.
pub fn same_instant(a: &str, b: &str) -> bool {
    let norm = |s: &str| match s.strip_suffix("+00:00") {
        Some(rest) => format!("{rest}Z"),
        None => s.to_string(),
    };
    norm(a) == norm(b)
}

impl Frontmatter {
    /// The frontmatter block including both `---` lines and a final newline. Every value
    /// from a file, the note or the model is double-quoted; dates and times are plain
    /// when they are real ones; numbers are plain.
    pub fn render(&self) -> String {
        let s = self.render_lf();
        if self.crlf {
            s.replace('\n', "\r\n")
        } else {
            s
        }
    }

    /// [`Frontmatter::render`] with `\n` line endings.
    fn render_lf(&self) -> String {
        let mut s = String::from("---\n");
        s.push_str("type: note\n");
        s.push_str(&format!("title: {}\n", quote(&self.title)));
        s.push_str(&format!("description: {}\n", quote(&self.description)));
        s.push_str(&format!("resource: {}\n", quote(&self.resource)));
        s.push_str("tags: [supernote]\n");
        s.push_str(&format!(
            "generated: {{ by: {}, model: {}, at: {} }}\n",
            token(&self.generated_by),
            quote(&self.generated_model),
            when(&self.generated_at)
        ));
        if let Some(date) = &self.source_date {
            s.push_str(&format!("source_date: {}\n", when(date)));
        }
        let modified = self
            .last_modified
            .as_ref()
            .map(|m| format!(", last_modified: {}", when(m)))
            .unwrap_or_default();
        s.push_str(&format!(
            "sources:\n  - {{ id: note, resource: {}{modified} }}\n",
            quote(&self.resource)
        ));
        s.push_str(&format!("status: {}\n", token(&self.status)));
        s.push_str(&format!("source_sha256: {}\n", quote(&self.source_sha256)));
        if let Some(id) = &self.file_id {
            s.push_str(&format!("file_id: {}\n", quote(id)));
        }
        s.push_str(&format!("pages: {}\n", self.pages));
        if self.keywords.is_empty() {
            s.push_str("keywords: []\n");
        } else {
            s.push_str("keywords:\n");
            for k in &self.keywords {
                s.push_str(&format!("  - {}\n", quote(k)));
            }
        }
        if !self.links.is_empty() {
            s.push_str("links:\n");
            for l in &self.links {
                let md = l
                    .target_md
                    .as_ref()
                    .map(|m| format!(", target_md: {}", quote(m)))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "  - {{ page: {}, target_file_id: {}, target_path: {}{md} }}\n",
                    l.page,
                    quote(&l.target_file_id),
                    quote(&l.target_path)
                ));
            }
        }
        if !self.digests.is_empty() {
            s.push_str("digests:\n");
            for d in &self.digests {
                s.push_str(&format!(
                    "  - {{ page: {}, digest_id: {}, text: {}, rect: \"{},{},{},{}\" }}\n",
                    d.page,
                    quote(&d.digest_id),
                    quote(&d.text),
                    d.rect[0],
                    d.rect[1],
                    d.rect[2],
                    d.rect[3]
                ));
            }
        }
        for raw in &self.extra {
            s.push_str(raw);
        }
        s.push_str("---\n");
        s
    }

    /// Reads the header notedown writes, the same header as another tool wrote it back
    /// (other quoting, block or flow style, other key order, added keys) and the header
    /// notedown wrote before (see [`HeaderFormat::Old`]); `None` when notedown's keys are
    /// missing or do not fit.
    pub fn parse(md: &str) -> Option<Frontmatter> {
        Frontmatter::from_header(&Header::read(md)?)
    }

    pub fn from_header(h: &Header) -> Option<Frontmatter> {
        let has = |k: &str| h.get(k).is_some();
        let old_keys = ["source", "kind", "model", "converted_at", "generator"]
            .iter()
            .any(|k| has(k));
        let format = match (has("generated"), old_keys) {
            (true, false) => HeaderFormat::Okf,
            (false, true) => HeaderFormat::Old,
            _ => return None,
        };
        let source_sha256 = h.text("source_sha256")?;
        let pages = h.text("pages")?.parse().ok()?;
        let keywords = texts(h.get("keywords"))?;
        let links = items(h.get("links"), |n| {
            Some(FrontmatterLink {
                page: field(n, "page")?.parse().ok()?,
                target_file_id: field(n, "target_file_id")?,
                target_path: field(n, "target_path")?,
                target_md: optional_field(n, "target_md")?,
            })
        })?;
        let digests = items(h.get("digests"), |n| {
            Some(FrontmatterDigest {
                page: field(n, "page")?.parse().ok()?,
                digest_id: field(n, "digest_id")?,
                text: field(n, "text")?,
                rect: rect_of(n.get("rect")?)?,
            })
        })?;
        let file_id = h.optional("file_id")?.filter(|id| !id.is_empty());
        let extra = h.extra();
        let fm = match format {
            HeaderFormat::Okf => {
                if h.text("type").is_none_or(|t| t.is_empty()) {
                    return None;
                }
                let generated = h.get("generated")?;
                let last_modified = match h.get("sources") {
                    None => None,
                    Some(sources) => match sources.as_list()?.first() {
                        None => None,
                        Some(first) => optional_field(first, "last_modified")?,
                    },
                };
                let non_empty = |v: Option<String>| v.filter(|s| !s.is_empty());
                Frontmatter {
                    title: h.optional("title")?.unwrap_or_default(),
                    description: h.optional("description")?.unwrap_or_default(),
                    resource: h.optional("resource")?.unwrap_or_default(),
                    generated_by: field(generated, "by")?,
                    generated_model: optional_field(generated, "model")?.unwrap_or_default(),
                    generated_at: field(generated, "at")?,
                    source_date: non_empty(h.optional("source_date")?),
                    last_modified: non_empty(last_modified),
                    status: h.optional("status")?.unwrap_or_else(|| "stable".into()),
                    source_sha256,
                    file_id,
                    pages,
                    keywords,
                    links,
                    digests,
                    extra,
                    format,
                    crlf: h.crlf,
                }
            }
            HeaderFormat::Old => {
                let source = h.text("source")?;
                let generator = h.text("generator")?;
                Frontmatter {
                    title: stem_of(&source),
                    description: String::new(),
                    resource: source,
                    generated_by: match generator.strip_prefix("notedown ") {
                        Some(version) => format!("notedown/{version}"),
                        None => generator,
                    },
                    generated_model: h.text("model")?,
                    generated_at: h.text("converted_at")?,
                    source_date: None,
                    last_modified: None,
                    status: "stable".into(),
                    source_sha256,
                    file_id,
                    pages,
                    keywords,
                    links,
                    digests,
                    extra,
                    format,
                    crlf: h.crlf,
                }
            }
        };
        Some(fm)
    }
}

/// The Markdown body after the frontmatter block, byte for byte.
pub fn body_of(md: &str) -> Option<&str> {
    let (_, offset, _) = split_block(md)?;
    Some(&md[offset..])
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

/// The whole Markdown file, with CRLF line endings when the header asks for them.
pub fn render_document(fm: &Frontmatter, pages: &[PageOut]) -> String {
    let mut doc = fm.render_lf();
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
    if fm.crlf {
        doc = doc.replace('\n', "\r\n");
    }
    doc
}
