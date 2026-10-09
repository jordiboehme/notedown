//! Converts one input file into a Markdown document.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use notedown_note::{Notebook, RenderOptions, TextBox, VersionPolicy, encode_png};
use notedown_ocr::convert::{LayoutBlock, html_to_markdown};
use notedown_ocr::{OcrEngine, PageOcr};
use sha2::{Digest, Sha256};

use crate::document::{
    Frontmatter, FrontmatterDigest, Header, HeaderFormat, PageOut, TRUNCATED_MARK, body_of,
    render_document, same_instant,
};
use crate::inputs::{Input, InputKind};
use crate::links::{FileIndex, LinkContext, apply as apply_links, encode_url, relative_path};

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub out: Option<PathBuf>,
    pub images: bool,
    pub debug: bool,
    pub force: bool,
    pub keep_template: bool,
    pub allow_unknown_version: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Converted {
        pages: usize,
    },
    Skipped,
    /// Only the header of an older notedown file was rewritten; no OCR ran.
    HeaderUpdated,
}

/// Where the pages of one input come from. Positions are 0-based.
pub trait PageSource {
    fn pages(&self) -> usize;
    fn is_blank(&self, i: usize) -> Result<bool>;
    /// The PNG the OCR engine sees (and `--images` writes).
    fn png(&self, i: usize) -> Result<Vec<u8>>;
    fn page_id(&self, i: usize) -> Option<String>;
    /// The text boxes of the page; none by default.
    fn text_boxes(&self, _i: usize) -> Result<Vec<TextBox>> {
        Ok(Vec::new())
    }
}

pub struct NoteSource<'a> {
    notebook: &'a Notebook,
    keep_template: bool,
}

impl<'a> NoteSource<'a> {
    pub fn new(notebook: &'a Notebook, keep_template: bool) -> Self {
        Self {
            notebook,
            keep_template,
        }
    }
}

impl PageSource for NoteSource<'_> {
    fn pages(&self) -> usize {
        self.notebook.pages.len()
    }
    fn is_blank(&self, i: usize) -> Result<bool> {
        Ok(self.notebook.page_is_blank(i)?)
    }
    fn png(&self, i: usize) -> Result<Vec<u8>> {
        let opts = RenderOptions {
            include_background: self.keep_template,
            ..RenderOptions::default()
        };
        let img = self.notebook.render_page(i, &opts)?;
        load_png_page(&encode_png(&img)?)
    }
    fn page_id(&self, i: usize) -> Option<String> {
        self.notebook.page(i).and_then(|p| p.id.clone())
    }
    fn text_boxes(&self, i: usize) -> Result<Vec<TextBox>> {
        Ok(self.notebook.text_boxes(i)?)
    }
}

/// The text boxes of every page, and the frontmatter entries for those with a digest.
/// A page whose text boxes cannot be read gets a warning (through `warn`) and no entries.
pub fn collect_text_boxes(
    source: &dyn PageSource,
    warn: &mut dyn FnMut(String),
) -> (Vec<Vec<TextBox>>, Vec<FrontmatterDigest>) {
    let mut per_page = Vec::new();
    let mut digests = Vec::new();
    for i in 0..source.pages() {
        let boxes = match source.text_boxes(i) {
            Ok(b) => b,
            Err(e) => {
                warn(format!(
                    "page {}: cannot read the text boxes, no digests listed: {e:#}",
                    i + 1
                ));
                Vec::new()
            }
        };
        for b in &boxes {
            if let Some(d) = &b.digest {
                digests.push(FrontmatterDigest {
                    page: i + 1,
                    digest_id: d.id.clone(),
                    text: b.text.clone(),
                    rect: b.rect,
                });
            }
        }
        per_page.push(boxes);
    }
    (per_page, digests)
}

/// The debug JSON of a text box: every field, the embedded strokes as a count.
fn text_box_json(b: &TextBox) -> serde_json::Value {
    let mut v = serde_json::to_value(b).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("source_strokes");
        o.insert(
            "source_stroke_count".into(),
            serde_json::json!(b.source_strokes.len()),
        );
    }
    v
}

struct PngSource {
    png: Vec<u8>,
}

impl PageSource for PngSource {
    fn pages(&self) -> usize {
        1
    }
    fn is_blank(&self, _i: usize) -> Result<bool> {
        Ok(false)
    }
    fn png(&self, _i: usize) -> Result<Vec<u8>> {
        Ok(self.png.clone())
    }
    fn page_id(&self, _i: usize) -> Option<String> {
        None
    }
}

/// Largest accepted aspect ratio of a page image (long side to short side). Supernote
/// pages are about 3:4; a strip longer than 1:16 is not a page the OCR model can read.
pub const MAX_ASPECT: u32 = 16;

/// Decodes any PNG, flattens alpha onto white and re-encodes it as an RGB PNG.
///
/// The header is checked before any pixel is decoded: each side at most
/// [`notedown_note::MAX_BITMAP_SIDE`], at most [`notedown_note::MAX_BITMAP_PIXELS`]
/// pixels (the bound of the `.note` parser) and an aspect ratio of at most 1:16.
pub fn load_png_page(bytes: &[u8]) -> Result<Vec<u8>> {
    use image::ImageDecoder;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(notedown_note::MAX_BITMAP_SIDE);
    limits.max_image_height = Some(notedown_note::MAX_BITMAP_SIDE);
    reader.limits(limits);
    let decoder = reader.into_decoder().context("cannot decode the PNG")?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > notedown_note::MAX_BITMAP_PIXELS {
        return Err(anyhow!(
            "the PNG is too large: {w} x {h} pixels (at most {} MPixel)",
            notedown_note::MAX_BITMAP_PIXELS / (1024 * 1024)
        ));
    }
    let (long, short) = (w.max(h), w.min(h));
    if short == 0 || long / short > MAX_ASPECT || (long / short == MAX_ASPECT && long % short != 0)
    {
        return Err(anyhow!(
            "the PNG has an unusable shape: {w} x {h} pixels (aspect ratio above 1:{MAX_ASPECT})"
        ));
    }
    let img = image::DynamicImage::from_decoder(decoder)
        .context("cannot decode the PNG")?
        .to_rgba8();
    let mut rgb = image::RgbImage::new(img.width(), img.height());
    for (src, dst) in img.pixels().zip(rgb.pixels_mut()) {
        let a = u32::from(src.0[3]);
        let mix = |c: u8| ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        *dst = image::Rgb([mix(src.0[0]), mix(src.0[1]), mix(src.0[2])]);
    }
    let mut out = Vec::new();
    rgb.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

/// What one page produced.
pub struct PageResult {
    pub out: PageOut,
    pub png: Option<Vec<u8>>,
    pub ocr: Option<PageOcr>,
    pub blocks: Vec<LayoutBlock>,
    /// Markdown of each block, aligned with `blocks`.
    pub parts: Vec<String>,
}

const EMPTY_PAGE: &str = "_(empty page)_";

/// Runs OCR over every page of a source. Blank pages skip the engine.
pub fn process_pages(
    source: &dyn PageSource,
    engine: &mut dyn OcrEngine,
    images: bool,
    assets_name: &str,
) -> Result<Vec<PageResult>> {
    let mut results = Vec::new();
    for i in 0..source.pages() {
        let number = i + 1;
        let page_id = source.page_id(i);
        let image_rel = images.then(|| encode_url(&format!("{assets_name}/page-{number:03}.png")));
        let blank = source
            .is_blank(i)
            .with_context(|| format!("page {number}"))?;
        if blank {
            let png = if images {
                Some(source.png(i).with_context(|| format!("page {number}"))?)
            } else {
                None
            };
            results.push(PageResult {
                out: PageOut {
                    number,
                    page_id,
                    image_rel,
                    markdown: EMPTY_PAGE.to_string(),
                    truncated: false,
                    empty: true,
                },
                png,
                ocr: None,
                blocks: Vec::new(),
                parts: Vec::new(),
            });
            continue;
        }
        let png = source.png(i).with_context(|| format!("page {number}"))?;
        let ocr = engine
            .ocr_png(&png)
            .map_err(|e| anyhow!("OCR failed on page {number}: {e}"))?;
        let converted = html_to_markdown(&ocr.html);
        results.push(PageResult {
            out: PageOut {
                number,
                page_id,
                image_rel,
                markdown: converted.markdown,
                truncated: ocr.truncated,
                empty: false,
            },
            png: images.then_some(png),
            ocr: Some(ocr),
            blocks: converted.blocks,
            parts: converted.parts,
        });
    }
    Ok(results)
}

/// `0.x` counts as the major version for 0-versions. Takes `notedown/<version>` and the
/// old `notedown <version>`.
fn major_key(generator: &str) -> Option<String> {
    let version = generator
        .strip_prefix("notedown/")
        .or_else(|| generator.strip_prefix("notedown "))?;
    let mut parts = version.split('.');
    let major = parts.next()?;
    if major == "0" {
        Some(format!("0.{}", parts.next()?))
    } else {
        Some(major.to_string())
    }
}

/// What the existing Markdown file means for this run.
enum Existing {
    /// Missing, not readable as notedown's, or the text is out of date: run OCR.
    Convert,
    /// Same source, model and notedown major version, current header and source fields.
    Current,
    /// The text is current but the header is old or names the source as it was: rewrite
    /// the header, keep the body.
    Header(Box<Frontmatter>, String),
}

fn existing(
    text: &str,
    header: &Header,
    sha: &str,
    model: &str,
    generator: &str,
    src: &SourceFields,
) -> Existing {
    let Some(fm) = Frontmatter::from_header(header) else {
        return Existing::Convert;
    };
    let same = fm.source_sha256 == sha
        && fm.generated_model == model
        && major_key(&fm.generated_by).is_some()
        && major_key(&fm.generated_by) == major_key(generator);
    let Some(body) = body_of(text).filter(|_| same) else {
        return Existing::Convert;
    };
    let fresh = fm.format == HeaderFormat::Okf
        && fm.resource == src.resource
        && fm.source_date == src.source_date
        && match (&fm.last_modified, &src.last_modified) {
            (Some(a), Some(b)) => same_instant(a, b),
            (a, b) => a == b,
        };
    if fresh {
        Existing::Current
    } else {
        Existing::Header(Box::new(fm), body.to_string())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn rfc3339(at: time::OffsetDateTime) -> String {
    let at = at.replace_nanosecond(0).unwrap_or(at);
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn now_rfc3339() -> String {
    rfc3339(time::OffsetDateTime::now_utc())
}

/// This computer's offset from UTC, read once per run; UTC when it cannot be read.
fn local_offset() -> time::UtcOffset {
    static OFFSET: std::sync::OnceLock<time::UtcOffset> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        notedown_cloud::localtime::local_utc_offset().unwrap_or(time::UtcOffset::UTC)
    })
}

/// `source_date` (the local calendar date) and `last_modified` (RFC 3339 UTC) of the
/// source file's modification time; `None` when the time cannot be read.
fn source_times(source: &Path) -> Option<(String, String)> {
    let modified = std::fs::metadata(source).and_then(|m| m.modified()).ok()?;
    let at = time::OffsetDateTime::from(modified);
    let date = at.to_offset(local_offset()).date();
    Some((
        format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        ),
        rfc3339(at),
    ))
}

/// The OKF `description` of a converted input.
fn description(kind: InputKind, pages: usize) -> String {
    match kind {
        InputKind::Note if pages == 1 => "Handwritten Supernote note, 1 page, read by OCR.".into(),
        InputKind::Note => format!("Handwritten Supernote note, {pages} pages, read by OCR."),
        InputKind::Png => "Handwritten Supernote page, read by OCR.".into(),
    }
}

/// The source file relative to the folder of the Markdown file, with `/` separators;
/// the absolute path when there is no relative one (another drive).
fn resource_of(source: &Path, output: &Path) -> String {
    let dir = match output.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    relative_path(dir, source).unwrap_or_else(|| {
        std::path::absolute(source)
            .unwrap_or_else(|_| source.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/")
    })
}

/// The header fields every run takes from the input file as it is now.
struct SourceFields {
    title: String,
    resource: String,
    source_date: Option<String>,
    last_modified: Option<String>,
}

fn source_fields(input: &Input, output: &Path) -> SourceFields {
    let (source_date, last_modified) = source_times(&input.source).unzip();
    SourceFields {
        title: input
            .source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        resource: resource_of(&input.source, output),
        source_date,
        last_modified,
    }
}

/// Puts the current header on a file whose text is current. The body stays byte for
/// byte, `generated` keeps who read the text and when, added keys stay; the source fields
/// are read again.
fn update_header(
    input: &Input,
    output: &Path,
    src: SourceFields,
    old: Frontmatter,
    body: &str,
) -> Result<()> {
    let truncated = body.lines().any(|l| l.trim_end() == TRUNCATED_MARK);
    let fm = Frontmatter {
        title: src.title,
        description: description(input.kind, old.pages),
        resource: src.resource,
        generated_by: old.generated_by,
        generated_model: old.generated_model,
        generated_at: old.generated_at,
        source_date: src.source_date,
        last_modified: src.last_modified,
        status: if truncated { "draft" } else { "stable" }.into(),
        source_sha256: old.source_sha256,
        file_id: old.file_id.filter(|_| input.kind == InputKind::Note),
        pages: old.pages,
        keywords: old.keywords,
        links: old.links,
        digests: old.digests,
        extra: old.extra,
        format: HeaderFormat::Okf,
        crlf: old.crlf,
    };
    write_atomic(output, format!("{}{body}", fm.render()).as_bytes())
}

/// For a source named `index` or `log`: the `<name>.md` an older notedown wrote for the
/// same source next to the new output, which OKF reserves. notedown leaves it alone.
pub fn left_over_reserved(input: &Input, output: &Path) -> Option<PathBuf> {
    let stem = input.source.file_stem()?.to_string_lossy().into_owned();
    if !(stem.eq_ignore_ascii_case("index") || stem.eq_ignore_ascii_case("log")) {
        return None;
    }
    let old = output.with_file_name(format!("{stem}.md"));
    if old == output {
        return None;
    }
    let text = std::fs::read_to_string(&old).ok()?;
    let header = Header::read(&text).filter(Header::is_notedown)?;
    let fm = Frontmatter::from_header(&header)?;
    let name = input.source.file_name()?.to_string_lossy();
    (fm.resource.rsplit('/').next() == Some(name.as_ref())).then_some(old)
}

fn warn_left_over(input: &Input, output: &Path) {
    if let Some(old) = left_over_reserved(input, output) {
        eprintln!(
            "warning: {} is from an older notedown and is no longer updated, the text of {} is now in {}. You can delete the old file.",
            old.display(),
            input.rel.display(),
            output.display()
        );
    }
}

/// Converts one input. Skips when the output is current (see the skip rule in the design).
pub fn convert_one(
    input: &Input,
    output: &Path,
    engine: &mut dyn OcrEngine,
    opts: &Options,
    index: &FileIndex,
) -> Result<Outcome> {
    let bytes = std::fs::read(&input.source)
        .with_context(|| format!("cannot read {}", input.source.display()))?;
    let current = std::fs::read_to_string(output).ok();
    let header = current
        .as_deref()
        .and_then(Header::read)
        .filter(Header::is_notedown);
    if !opts.force && output.exists() && header.is_none() {
        return Err(anyhow!(
            "refusing to overwrite {}: it was not created by notedown (use --force to replace it)",
            output.display()
        ));
    }
    let sha = const_hex::encode(Sha256::digest(&bytes));
    let generator = format!("notedown/{}", env!("CARGO_PKG_VERSION"));
    let src = source_fields(input, output);
    if !opts.force
        && let (Some(text), Some(header)) = (&current, &header)
    {
        match existing(text, header, &sha, engine.label(), &generator, &src) {
            Existing::Current => return Ok(Outcome::Skipped),
            Existing::Header(old, body) => {
                update_header(input, output, src, *old, &body)?;
                warn_left_over(input, output);
                return Ok(Outcome::HeaderUpdated);
            }
            Existing::Convert => {}
        }
    }
    // a notedown file keeps the keys and comments other tools added and its line endings
    let (extra, crlf) = header.map_or((Vec::new(), false), |h| (h.extra(), h.crlf()));

    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let assets_name = format!("{stem}.assets");
    let dir = output.parent().unwrap_or_else(|| Path::new("."));

    let (file_id, keywords, notebook, results, fm_links, text_boxes, digests) = match input.kind {
        InputKind::Note => {
            let policy = if opts.allow_unknown_version {
                VersionPolicy::AllowUnknown
            } else {
                VersionPolicy::Strict
            };
            let nb = Notebook::from_bytes(bytes, policy)
                .with_context(|| format!("cannot read {}", input.source.display()))?;
            let src = NoteSource {
                notebook: &nb,
                keep_template: opts.keep_template,
            };
            let mut results = process_pages(&src, engine, opts.images, &assets_name)?;
            let page_ids: Vec<Option<String>> = nb.pages.iter().map(|p| p.id.clone()).collect();
            let fm_links = apply_links(
                &nb.links,
                &mut results,
                &LinkContext {
                    index,
                    from_md: output,
                    page_ids: &page_ids,
                    width: nb.width,
                    height: nb.height,
                },
            );
            let (text_boxes, digests) = collect_text_boxes(&src, &mut |w| {
                eprintln!("warning: {}: {w}", input.rel.display())
            });
            let keywords = nb
                .keywords
                .iter()
                .filter_map(|k| {
                    let text = k.text.as_deref()?.trim();
                    (!text.is_empty()).then(|| format!("p{}: {text}", k.page + 1))
                })
                .collect::<Vec<_>>();
            (
                nb.file_id.clone(),
                keywords,
                Some(nb),
                results,
                fm_links,
                text_boxes,
                digests,
            )
        }
        InputKind::Png => {
            let src = PngSource {
                png: load_png_page(&bytes)
                    .with_context(|| format!("cannot read {}", input.source.display()))?,
            };
            let results = process_pages(&src, engine, opts.images, &assets_name)?;
            (
                None,
                Vec::new(),
                None,
                results,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
        }
    };

    let fm = Frontmatter {
        title: src.title,
        description: description(input.kind, results.len()),
        resource: src.resource,
        generated_by: generator,
        generated_model: engine.label().to_string(),
        generated_at: now_rfc3339(),
        source_date: src.source_date,
        last_modified: src.last_modified,
        status: if results.iter().any(|r| r.out.truncated) {
            "draft"
        } else {
            "stable"
        }
        .into(),
        source_sha256: sha,
        file_id,
        pages: results.len(),
        keywords,
        links: fm_links,
        digests,
        extra,
        format: HeaderFormat::Okf,
        crlf,
    };

    if opts.images {
        for r in &results {
            if let Some(png) = &r.png {
                write_atomic(
                    &dir.join(&assets_name)
                        .join(format!("page-{:03}.png", r.out.number)),
                    png,
                )?;
            }
        }
    }
    if opts.debug {
        let debug = serde_json::json!({
            "source": input.rel.to_string_lossy().replace('\\', "/"),
            "notebook": notebook.as_ref().map(|nb| serde_json::json!({
                "signature": nb.signature,
                "file_id": nb.file_id,
                "equipment": nb.equipment,
                "pages": nb.pages.iter().map(|p| serde_json::json!({
                    "id": p.id,
                    "style": p.style,
                    "recogn_status": p.recogn_status,
                    "recogn_text": p.recogn_text,
                })).collect::<Vec<_>>(),
            })),
            "pages": results.iter().enumerate().map(|(i, r)| serde_json::json!({
                "number": r.out.number,
                "ocr": r.ocr,
                "blocks": r.blocks,
                "text_boxes": text_boxes
                    .get(i)
                    .map(|bs| bs.iter().map(text_box_json).collect::<Vec<_>>())
                    .unwrap_or_default(),
            })).collect::<Vec<_>>(),
        });
        write_atomic(
            &dir.join(format!("{stem}.debug.json")),
            serde_json::to_string_pretty(&debug)?.as_bytes(),
        )?;
    }
    let outs: Vec<PageOut> = results.iter().map(|r| r.out.clone()).collect();
    write_atomic(output, render_document(&fm, &outs).as_bytes())?;
    warn_left_over(input, output);
    Ok(Outcome::Converted { pages: outs.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedown_ocr::OcrError;

    struct Counting(usize);
    impl OcrEngine for Counting {
        fn ocr_png(&mut self, _png: &[u8]) -> Result<PageOcr, OcrError> {
            self.0 += 1;
            Ok(PageOcr {
                html: "<div data-label=\"Text\"><p>Hi</p></div>".into(),
                prompt_tokens: 1,
                output_tokens: 1,
                truncated: false,
                encode_ms: 0,
                decode_ms: 0,
            })
        }
        fn label(&self) -> &str {
            "stub"
        }
    }

    struct Stub;
    impl PageSource for Stub {
        fn pages(&self) -> usize {
            2
        }
        fn is_blank(&self, i: usize) -> Result<bool> {
            Ok(i == 0)
        }
        fn png(&self, _i: usize) -> Result<Vec<u8>> {
            Ok(vec![1, 2, 3])
        }
        fn page_id(&self, i: usize) -> Option<String> {
            Some(format!("P{i}"))
        }
    }

    #[test]
    fn blank_page_skips_the_engine() {
        let mut engine = Counting(0);
        let r = process_pages(&Stub, &mut engine, false, "x.assets").unwrap();
        assert_eq!(r[0].out.markdown, "_(empty page)_");
        assert!(r[0].out.empty);
        assert_eq!(engine.0, 1, "only the second page reaches the engine");
        assert!(r[1].out.markdown.contains("Hi"));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A PNG with a valid 8-bit grayscale IHDR of the given size, an empty IDAT and IEND.
    fn png_header_only(w: u32, h: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &[]);
        chunk(b"IEND", &[]);
        out
    }

    #[test]
    fn huge_png_is_rejected_before_decoding() {
        let start = std::time::Instant::now();
        // over the side limit: the decoder refuses the header
        let e = load_png_page(&png_header_only(100_000, 100_000)).unwrap_err();
        assert!(
            matches!(
                e.downcast_ref::<image::ImageError>(),
                Some(image::ImageError::Limits(_))
            ),
            "{e:#}"
        );
        // under the side limit but over the pixel cap
        let e = load_png_page(&png_header_only(16_000, 16_000)).unwrap_err();
        assert!(
            format!("{e:#}").contains("too large: 16000 x 16000"),
            "{e:#}"
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn extreme_aspect_ratio_is_rejected() {
        let e = load_png_page(&png_header_only(100, 1_700)).unwrap_err();
        assert!(
            format!("{e:#}").contains("aspect ratio above 1:16"),
            "{e:#}"
        );
        let e = load_png_page(&png_header_only(1_601, 100)).unwrap_err();
        assert!(
            format!("{e:#}").contains("aspect ratio above 1:16"),
            "{e:#}"
        );
        // exactly 1:16 passes the shape check and fails later only on the empty IDAT
        let e = load_png_page(&png_header_only(100, 1_600)).unwrap_err();
        assert!(!format!("{e:#}").contains("aspect"), "{e:#}");
    }

    #[test]
    fn page_errors_name_one_1_based_page() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/notes/test2.note"
        );
        let mut nb = Notebook::open(Path::new(path), VersionPolicy::Strict).unwrap();
        nb.pages[0].width = 100_000;
        nb.pages[0].height = 100_000;
        let src = NoteSource {
            notebook: &nb,
            keep_template: false,
        };
        let Err(e) = process_pages(&src, &mut Counting(0), false, "x.assets") else {
            panic!("expected an error");
        };
        let msg = format!("{e:#}");
        assert!(msg.starts_with("page 1: "), "{msg}");
        assert_eq!(msg.matches("page").count(), 1, "{msg}");
        assert!(msg.contains("implausible"), "{msg}");
    }

    #[test]
    fn major_keys() {
        assert_eq!(major_key("notedown 0.1.0"), Some("0.1".into()));
        assert_eq!(major_key("notedown 0.1.7"), major_key("notedown 0.1.0"));
        assert_ne!(major_key("notedown 0.2.0"), major_key("notedown 0.1.0"));
        assert_eq!(major_key("notedown 1.4.2"), Some("1".into()));
        assert_eq!(major_key("notedown/0.1.3"), major_key("notedown 0.1.0"));
        assert_eq!(major_key("notedown/2.0.0"), Some("2".into()));
        assert_eq!(major_key("other/0.1.0"), None);
    }
}
