//! Links between notes: the `FILE_ID` index, link placement on OCR blocks and the
//! rewriting of page Markdown.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use notedown_note::{Link, LinkKind, Notebook, VersionPolicy};
use notedown_ocr::convert::{LayoutBlock, assemble};

use crate::document::{Frontmatter, FrontmatterLink};
use crate::inputs::{Input, InputKind};
use crate::pipeline::PageResult;

/// Percent-encodes what would break a Markdown link destination for a file path.
pub(crate) fn encode_url(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            c => out.push(c),
        }
    }
    out
}

/// Encodes only what ends a Markdown destination; keeps `#`, `?` and `%` of web URLs.
fn encode_web_url(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '(' => "%28".to_string(),
            ')' => "%29".to_string(),
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// Escapes the Markdown-significant characters of a string that comes from the note file
/// (target names, device paths, URLs used as text), so it reads as plain text.
pub fn escape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '[' | ']' | '(' | ')' | '*' | '_' | '`' | '<') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escapes the brackets of OCR text used as a link label; the text is already Markdown.
fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

/// The absolute form of a path as its anchor (root, drive) and its names, with `.` and
/// `..` folded away by the text alone: no symbolic link is followed, so `/var` and
/// `/private/var` on macOS stay what the caller wrote.
fn lexical_parts(path: &Path) -> Option<(std::ffi::OsString, Vec<std::ffi::OsString>)> {
    let abs = std::path::absolute(path).ok()?;
    let mut anchor = std::ffi::OsString::new();
    let mut names = Vec::new();
    for c in abs.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => anchor.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                names.pop();
            }
            Component::Normal(n) => names.push(n.to_os_string()),
        }
    }
    Some((anchor, names))
}

/// The path from a folder to a file as `/`-separated parts (`..` to go up). `None` when
/// the two are on different drives.
pub(crate) fn relative_parts(from_dir: &Path, target: &Path) -> Option<Vec<String>> {
    let (base_anchor, base) = lexical_parts(from_dir)?;
    let (target_anchor, tgt) = lexical_parts(target)?;
    if base_anchor != target_anchor {
        return None;
    }
    let common = base.iter().zip(&tgt).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); base.len() - common];
    parts.extend(
        tgt[common..]
            .iter()
            .map(|n| n.to_string_lossy().into_owned()),
    );
    Some(parts)
}

/// [`relative_parts`] joined with `/`, without percent-encoding.
pub fn relative_path(from_dir: &Path, target: &Path) -> Option<String> {
    relative_parts(from_dir, target).map(|p| p.join("/"))
}

/// Maps the `FILE_ID` of a note to the Markdown file generated for it.
#[derive(Debug, Clone, Default)]
pub struct FileIndex {
    map: HashMap<String, PathBuf>,
}

fn scan_md(dir: &Path, recursive: bool, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // file_type does not follow symlinks, so a symlink loop cannot recurse
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if recursive {
                scan_md(&path, true, found);
            }
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Some(id) = Frontmatter::parse(&text).and_then(|fm| fm.file_id)
        {
            found.push((id, path));
        }
    }
}

impl FileIndex {
    /// Indexes every `.note` input (its output path) and every existing `.md` with a
    /// `file_id` under `out_root`, or next to the outputs when there is no output root.
    /// Inputs win over existing files.
    pub fn build(inputs: &[Input], outputs: &[PathBuf], out_root: Option<&Path>) -> FileIndex {
        let mut found = Vec::new();
        match out_root {
            Some(root) => scan_md(root, true, &mut found),
            None => {
                let mut dirs: Vec<&Path> = outputs.iter().filter_map(|o| o.parent()).collect();
                dirs.sort();
                dirs.dedup();
                for dir in dirs {
                    let dir = if dir.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        dir
                    };
                    scan_md(dir, false, &mut found);
                }
            }
        }
        let mut map: HashMap<String, PathBuf> = found.into_iter().collect();
        for (input, output) in inputs.iter().zip(outputs) {
            if input.kind != InputKind::Note {
                continue;
            }
            if let Ok(nb) = Notebook::open(&input.source, VersionPolicy::AllowUnknown)
                && let Some(id) = nb.file_id
            {
                map.insert(id, output.clone());
            }
        }
        FileIndex { map }
    }

    /// The generated Markdown path for a file id.
    pub fn path_of(&self, file_id: &str) -> Option<&Path> {
        self.map.get(file_id).map(PathBuf::as_path)
    }

    /// The relative URL from the linking document to the target (spaces as `%20`).
    pub fn resolve(&self, file_id: &str, from_md: &Path) -> Option<String> {
        let from = std::path::absolute(from_md).ok()?;
        let parts = relative_parts(from.parent()?, self.map.get(file_id)?)?;
        (!parts.is_empty()).then(|| {
            parts
                .iter()
                .map(|p| encode_url(p))
                .collect::<Vec<_>>()
                .join("/")
        })
    }

    /// The same relative path without percent-encoding.
    fn resolve_plain(&self, file_id: &str, from_md: &Path) -> Option<String> {
        let url = self.resolve(file_id, from_md)?;
        Some(url.split('/').map(decode_url).collect::<Vec<_>>().join("/"))
    }
}

fn decode_url(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// For each link the index of the block it labels: the block whose bbox (0-1000, scaled
/// to the page) overlaps the link rectangle by at least half of the link's area, and
/// where that overlap also covers at least half of the block's area. A small link inside
/// a large paragraph therefore labels no block: the paragraph keeps its text and the link
/// is only listed in the `Links:` line.
pub fn place(
    links: &[Link],
    blocks: &[LayoutBlock],
    page_w: u32,
    page_h: u32,
) -> Vec<Option<usize>> {
    let (w, h) = (f64::from(page_w), f64::from(page_h));
    links
        .iter()
        .map(|l| {
            let [x, y, lw, lh] = l.rect.map(f64::from);
            let area = lw * lh;
            if area <= 0.0 {
                return None;
            }
            let mut best: Option<(usize, f64)> = None;
            for (i, b) in blocks.iter().enumerate() {
                let Some([bx0, by0, bx1, by1]) = b.bbox else {
                    continue;
                };
                let (bx0, bx1) = (f64::from(bx0) * w / 1000.0, f64::from(bx1) * w / 1000.0);
                let (by0, by1) = (f64::from(by0) * h / 1000.0, f64::from(by1) * h / 1000.0);
                let ow = (x + lw).min(bx1) - x.max(bx0);
                let oh = (y + lh).min(by1) - y.max(by0);
                if ow <= 0.0 || oh <= 0.0 {
                    continue;
                }
                let overlap = ow * oh;
                let block_area = (bx1 - bx0) * (by1 - by0);
                if overlap * 2.0 >= area
                    && overlap * 2.0 >= block_area
                    && best.is_none_or(|(_, o)| overlap > o)
                {
                    best = Some((i, overlap));
                }
            }
            best.map(|(i, _)| i)
        })
        .collect()
}

/// What `apply` needs to know about the linking note.
pub struct LinkContext<'a> {
    pub index: &'a FileIndex,
    /// The Markdown file being written.
    pub from_md: &'a Path,
    /// `PAGEID` of every page of the note, by 0-based position.
    pub page_ids: &'a [Option<String>],
    pub width: u32,
    pub height: u32,
}

fn file_stem_of(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?;
    let stem = name.strip_suffix(".note").unwrap_or(name);
    (!stem.is_empty()).then(|| stem.to_string())
}

/// Strips the link icon (read as a trailing `@`) and underline markup from OCR text.
fn clean_ocr(md: &str) -> String {
    let t = md
        .replace("<u>", "")
        .replace("</u>", "")
        .replace("\\\n", " ");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.strip_suffix('@').unwrap_or(&t);
    t.trim().to_string()
}

/// Number (1-based) of the page the link points to inside the same note.
fn target_page_number(link: &Link, ctx: &LinkContext) -> Option<usize> {
    let id = link.target_page_id.as_deref()?;
    ctx.page_ids
        .iter()
        .position(|p| p.as_deref() == Some(id))
        .map(|i| i + 1)
}

/// The text of one link as Markdown. `ocr` is the cleaned OCR text of the block, if any.
fn entry(link: &Link, ocr: Option<&str>, ctx: &LinkContext) -> String {
    let ocr = ocr.filter(|t| !t.is_empty());
    match link.kind {
        LinkKind::File => {
            let stem = link
                .target
                .as_deref()
                .and_then(file_stem_of)
                .or_else(|| link.target_file_id.clone())
                .map_or_else(|| "link".into(), |s| escape_md(&s));
            let url = link
                .target_file_id
                .as_deref()
                .and_then(|id| ctx.index.resolve(id, ctx.from_md));
            match (url, link.target.as_deref()) {
                (Some(url), _) => format!("[{stem}]({url})"),
                (None, Some(path)) => format!("{stem} ({})", escape_md(path)),
                (None, None) => stem,
            }
        }
        LinkKind::Web => match link.target.as_deref() {
            Some(url) => format!(
                "[{}]({})",
                ocr.map_or_else(|| escape_md(url), escape_label),
                encode_web_url(url)
            ),
            None => ocr.unwrap_or("web link").to_string(),
        },
        LinkKind::Page => match target_page_number(link, ctx) {
            Some(n) => {
                let label = ocr.map_or_else(|| format!("page {n}"), str::to_string);
                format!("[{}](#page-{n})", escape_label(&label))
            }
            None => ocr.map_or_else(
                || format!("page {}", link.target_page.unwrap_or(0)),
                str::to_string,
            ),
        },
        LinkKind::Other(_) => ocr
            .map(str::to_string)
            .or_else(|| link.target.as_deref().map(escape_md))
            .unwrap_or_else(|| "link".into()),
    }
}

fn replaceable(label: &str) -> bool {
    matches!(label, "Text" | "Section-Header")
}

/// Rewrites the Markdown of every page: placed links replace the OCR text of their block,
/// and a `Links:` line closes each page that has links. Returns the `links:` frontmatter
/// entries (file links with a target id), page numbers 1-based.
pub fn apply(links: &[Link], pages: &mut [PageResult], ctx: &LinkContext) -> Vec<FrontmatterLink> {
    let mut front = Vec::new();
    for (pos, page) in pages.iter_mut().enumerate() {
        let page_links: Vec<&Link> = links.iter().filter(|l| l.page == pos).collect();
        if page_links.is_empty() {
            continue;
        }
        let owned: Vec<Link> = page_links.iter().map(|l| (*l).clone()).collect();
        let placed = place(&owned, &page.blocks, ctx.width, ctx.height);
        let mut replacements: HashMap<usize, Vec<String>> = HashMap::new();
        for (link, block) in page_links.iter().zip(&placed) {
            let Some(b) = *block else { continue };
            if !replaceable(&page.blocks[b].label) || page.parts.get(b).is_none_or(|p| p.is_empty())
            {
                continue;
            }
            let ocr = clean_ocr(page.parts[b].trim_start_matches('#'));
            replacements
                .entry(b)
                .or_default()
                .push(entry(link, Some(&ocr), ctx));
        }
        for (b, entries) in replacements {
            let old = &page.parts[b];
            let hashes = old.chars().take_while(|&c| c == '#').count();
            let prefix = if hashes > 0 && old[hashes..].starts_with(' ') {
                format!("{} ", "#".repeat(hashes))
            } else {
                String::new()
            };
            page.parts[b] = format!("{prefix}{}", entries.join(" "));
        }
        let mut md = assemble(&page.parts);
        let line: Vec<String> = page_links.iter().map(|l| entry(l, None, ctx)).collect();
        if page.parts.is_empty() {
            md = page.out.markdown.clone();
        }
        let mut text = md.trim_end().to_string();
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("Links: {}\n", line.join(", ")));
        page.out.markdown = text;

        for l in page_links {
            if l.kind == LinkKind::File
                && let Some(id) = &l.target_file_id
            {
                let target_md = ctx.index.resolve_plain(id, ctx.from_md);
                front.push(FrontmatterLink {
                    page: pos + 1,
                    target_file_id: id.clone(),
                    target_path: l.target.clone().unwrap_or_default(),
                    target_md,
                });
            }
        }
    }
    front
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let rel = |from: &str, to: &str| relative_path(Path::new(from), Path::new(to));
        assert_eq!(rel("/n", "/n/Test 2.note").as_deref(), Some("Test 2.note"));
        assert_eq!(
            rel("/t/out", "/t/in/Test 2.note").as_deref(),
            Some("../in/Test 2.note")
        );
        assert_eq!(rel("/a/b", "/a/X.note").as_deref(), Some("../X.note"));
        assert_eq!(
            rel("/t/a/../out/sub", "/t/in/./sub/x.png").as_deref(),
            Some("../../in/sub/x.png")
        );
        assert_eq!(
            rel("/t/out", "/t/out/../in/x.note").as_deref(),
            Some("../in/x.note")
        );
        assert_eq!(rel("/", "/x.note").as_deref(), Some("x.note"));
    }
}
