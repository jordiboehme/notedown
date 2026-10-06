//! The parsed notebook and its entry points.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::Serialize;

use crate::container;
use crate::error::{NoteError, corrupt};
use crate::keywords::parse_keyword;
use crate::links::parse_link;
use crate::meta::Meta;
use crate::model::{
    BlockKind, Cover, KNOWN_SIGNATURES, KeyPosition, Keyword, Link, Page, RawBlock, Save, Template,
    Timestamp, Title, VersionPolicy,
};
use crate::page::parse_page;
use crate::resolve::Resolver;
use crate::titles::parse_title;
use crate::values::{Warnings, id_timestamp, key_position, parse_u32, text, uint};

/// The file bytes. `Debug` prints only the length.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct FileBytes(pub(crate) Vec<u8>);

impl fmt::Debug for FileBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
}

/// A parsed `.note` file: its current state (reachable from the last footer) plus
/// the save history and all raw blocks (docs/note-format.md Appendix A, "File").
#[derive(Debug, Clone, Serialize)]
pub struct Notebook {
    #[serde(skip)]
    pub(crate) data: FileBytes,
    /// Offset of the first block: 24, or 20 for the original A5 layout
    /// (docs/note-format.md 1).
    pub first_block: u64,
    /// File type field, `note` in all samples, `mark` for PDF annotations. Empty for
    /// the original A5 layout, where the signature starts at offset 0.
    pub file_type: String,
    /// Signature, e.g. `SN_FILE_VER_20260016`.
    pub signature: String,
    /// The 8 digits of the signature as a number.
    pub version: Option<u32>,
    /// True when the signature is in [`crate::KNOWN_SIGNATURES`].
    pub version_known: bool,
    /// Address of the current footer block.
    pub footer_address: u64,
    /// All footer keys, raw.
    pub footer: Meta,
    /// `FILE_FEATURE`: address of the header block.
    pub header_address: Option<u64>,
    /// All header keys, raw.
    pub header: Meta,
    /// `FILE_ID`.
    pub file_id: Option<String>,
    /// Creation time decoded from `FILE_ID`.
    pub created: Option<Timestamp>,
    /// `APPLY_EQUIPMENT`, the device code.
    pub equipment: Option<String>,
    /// False when the device code is missing or not in the table of
    /// docs/note-format.md 4.6 and the size fell back to 1404 x 1872.
    pub device_known: bool,
    /// Portrait page width in pixels (docs/note-format.md 4.6).
    pub width: u32,
    /// Portrait page height in pixels.
    pub height: u32,
    /// `FILE_RECOGN_TYPE` is 1 (real-time recognition note).
    pub realtime_recognition: bool,
    /// `FILE_RECOGN_LANGUAGE`; `None` for `none`.
    pub recognition_language: Option<String>,
    /// Footer `DIRTY`.
    pub dirty: Option<u32>,
    /// Pages in the order of the number in their `PAGEn` key; `pages[i].index == i`.
    pub pages: Vec<Page>,
    pub links: Vec<Link>,
    pub titles: Vec<Title>,
    pub keywords: Vec<Keyword>,
    pub templates: Vec<Template>,
    pub covers: Vec<Cover>,
    /// Save history in file order; the last entry is the current save.
    pub saves: Vec<Save>,
    /// Every block in file order (sequential walk, docs/note-format.md 2).
    pub blocks: Vec<RawBlock>,
    /// Why the sequential walk stopped early, if it did. The current state does
    /// not depend on the walk.
    pub history_error: Option<String>,
    /// Values that are present but could not be typed; their raw form is kept.
    pub warnings: Vec<String>,
}

impl Notebook {
    /// Reads and parses a `.note` file.
    pub fn open(path: &Path, policy: VersionPolicy) -> Result<Notebook, NoteError> {
        let bytes = std::fs::read(path)?;
        Notebook::from_bytes(bytes, policy)
    }

    /// Parses a `.note` file held in memory.
    pub fn from_bytes(bytes: Vec<u8>, policy: VersionPolicy) -> Result<Notebook, NoteError> {
        let envelope = container::envelope(&bytes)?;
        let version_known = KNOWN_SIGNATURES.contains(&envelope.signature.as_str());
        // docs/note-format.md 1, "Unknown signatures"
        if !version_known && policy == VersionPolicy::Strict {
            return Err(NoteError::UnknownVersion(envelope.signature));
        }
        let first_block = envelope.first_block;
        let footer_address = container::current_footer(&bytes, first_block)?;
        let current = parse_current(&bytes, first_block, footer_address, &envelope.file_type)?;
        let (saves, blocks, history_error) = history(&bytes, first_block, &current.reachable);

        Ok(Notebook {
            first_block,
            file_type: envelope.file_type,
            signature: envelope.signature,
            version: envelope.version,
            version_known,
            footer_address,
            footer: current.footer,
            header_address: current.header_address,
            header: current.header,
            file_id: current.file_id,
            created: current.created,
            equipment: current.equipment,
            device_known: current.device_known,
            width: current.width,
            height: current.height,
            realtime_recognition: current.realtime_recognition,
            recognition_language: current.recognition_language,
            dirty: current.dirty,
            pages: current.pages,
            links: current.links,
            titles: current.titles,
            keywords: current.keywords,
            templates: current.templates,
            covers: current.covers,
            saves,
            blocks,
            history_error,
            warnings: current.warnings,
            data: FileBytes(bytes),
        })
    }

    /// The payload of the block at `address`, bounds-checked
    /// (docs/note-format.md 2, 10.2). Address 0 ("absent") is an error.
    pub fn block(&self, address: u64) -> Result<&[u8], NoteError> {
        container::payload(&self.data.0, self.first_block, address).map_err(corrupt)
    }

    /// The page at the 0-based position `index` (`page(0)` is the first page).
    pub fn page(&self, index: usize) -> Option<&Page> {
        self.pages.get(index)
    }

    /// The cover image entry: `COVER_2` is preferred over `COVER_1`
    /// (docs/note-format.md 4.1). `COVER_0` means no cover.
    pub fn cover(&self) -> Option<&Cover> {
        let with_data = |n: u32| {
            self.covers
                .iter()
                .find(|c| c.number == n && c.block.is_some())
        };
        with_data(2).or_else(|| with_data(1))
    }

    /// True when the signature version selects the "X2" color table for RATTA_RLE
    /// bitmaps: version 20230015 or higher (docs/note-format.md 1, 5.3).
    pub fn uses_x2_colors(&self) -> bool {
        self.signature.starts_with("SN_FILE_VER_") && self.version.is_some_and(|v| v >= 20230015)
    }

    /// The whole file.
    pub fn bytes(&self) -> &[u8] {
        &self.data.0
    }
}

/// Page size by device code (docs/note-format.md 4.6): `(width, height, known)`.
fn page_size(equipment: Option<&str>) -> (u32, u32, bool) {
    match equipment {
        Some("N5") => (1920, 2560, true),
        Some("N6" | "A6X") => (1404, 1872, true),
        _ => (1404, 1872, false),
    }
}

/// The current state, reached from the current footer.
struct Current {
    footer: Meta,
    header_address: Option<u64>,
    header: Meta,
    file_id: Option<String>,
    created: Option<Timestamp>,
    equipment: Option<String>,
    device_known: bool,
    width: u32,
    height: u32,
    realtime_recognition: bool,
    recognition_language: Option<String>,
    dirty: Option<u32>,
    pages: Vec<Page>,
    links: Vec<Link>,
    titles: Vec<Title>,
    keywords: Vec<Keyword>,
    templates: Vec<Template>,
    covers: Vec<Cover>,
    reachable: std::collections::BTreeSet<u64>,
    warnings: Vec<String>,
}

/// Position digits of an item key, or `Corrupt` naming the key.
fn position(key: &str, digits: &str) -> Result<KeyPosition, NoteError> {
    key_position(digits).ok_or_else(|| corrupt(format!("footer {key}: malformed position digits")))
}

/// Sort key for links and keywords: page position, top, left.
fn order(page: usize, p: &KeyPosition) -> (usize, u32, u32) {
    (page, p.top.unwrap_or(0), p.left.unwrap_or(0))
}

fn parse_current(
    data: &[u8],
    first_block: u64,
    footer_address: u64,
    file_type: &str,
) -> Result<Current, NoteError> {
    let mut rx = Resolver {
        data,
        first_block,
        footer_address,
        file_type: file_type.to_owned(),
        reachable: Default::default(),
        warnings: Warnings::default(),
    };
    let footer_block = crate::model::BlockRef {
        address: footer_address,
        len: container::payload(data, first_block, footer_address)
            .map_err(corrupt)?
            .len(),
    };
    rx.reachable.insert(footer_address);
    let footer = rx.metadata("footer", footer_block)?;

    // header (docs/note-format.md 4.2)
    let header_ref = rx.address("footer", &footer, "FILE_FEATURE")?;
    let header = match header_ref {
        Some(r) => rx.metadata("header FILE_FEATURE", r)?,
        None => Meta::default(),
    };
    let equipment = text(&header, "APPLY_EQUIPMENT");
    let (width, height, device_known) = page_size(equipment.as_deref());
    let file_id = text(&header, "FILE_ID");
    let realtime_recognition =
        uint(&mut rx.warnings, "header", &header, "FILE_RECOGN_TYPE") == Some(1);
    let dirty = uint(&mut rx.warnings, "footer", &footer, "DIRTY");

    // footer keys (docs/note-format.md 4.1); a key may repeat (3)
    let mut page_refs: Vec<(u32, &str, &str)> = Vec::new();
    let mut plain_pages = 0u32;
    let mut item_refs: Vec<(&str, &str, &str, KeyPosition)> = Vec::new();
    let mut style_refs: Vec<(&str, &str)> = Vec::new();
    let mut cover_refs: Vec<(&str, u32, &str)> = Vec::new();
    for (key, value) in footer.iter() {
        if key == "PAGE" {
            // original A5 repeats a plain PAGE key (docs/note-format.md 1)
            plain_pages += 1;
            page_refs.push((plain_pages, key, value));
        } else if let Some(n) = key.strip_prefix("PAGE").and_then(parse_u32) {
            page_refs.push((n, key, value));
        } else if key.starts_with("STYLE_") {
            style_refs.push((key, value));
        } else if let Some(n) = key.strip_prefix("COVER_").and_then(parse_u32) {
            cover_refs.push((key, n, value));
        } else if let Some(digits) = key.strip_prefix("TITLE_") {
            item_refs.push(("title", key, value, position(key, digits)?));
        } else if let Some(digits) = key.strip_prefix("KEYWORD_") {
            item_refs.push(("keyword", key, value, position(key, digits)?));
        } else if let Some(digits) = key.strip_prefix("LINKO_") {
            item_refs.push(("link", key, value, position(key, digits)?));
        }
    }
    // a parser should sort pages by the number in the key (docs/note-format.md 3)
    page_refs.sort_by_key(|(n, _, _)| *n);

    // One table maps the 1-based device page number of a key to the 0-based
    // position in `pages`, so gapped keys (PAGE1, PAGE3) still give
    // `pages[i].index == i` and items point at the same positions.
    let mut positions: BTreeMap<u32, usize> = BTreeMap::new();
    let mut pages = Vec::new();
    for (n, key, value) in page_refs {
        if n == 0 {
            return Err(corrupt(format!(
                "footer {key}: page number 0 (keys are 1-based)"
            )));
        }
        if let Some(block) = rx.address_value("footer", key, value)? {
            let index = pages.len();
            pages.push(parse_page(&mut rx, key, index, block, (width, height))?);
            positions.entry(n).or_insert(index);
        }
    }

    let mut links = Vec::new();
    let mut titles = Vec::new();
    let mut keywords = Vec::new();
    for (kind, key, value, pos) in item_refs {
        if pos.page == 0 {
            return Err(corrupt(format!(
                "footer {key}: page number 0 (keys are 1-based)"
            )));
        }
        let Some(&page) = positions.get(&pos.page) else {
            return Err(corrupt(format!(
                "footer {key}: page {} has no PAGE{} key",
                pos.page, pos.page
            )));
        };
        let Some(block) = rx.address_value("footer", key, value)? else {
            continue;
        };
        match kind {
            "title" => titles.push(parse_title(&mut rx, key, page, pos, block)?),
            "keyword" => keywords.push(parse_keyword(&mut rx, key, page, pos, block)?),
            _ => links.push(parse_link(&mut rx, key, page, pos, block)?),
        }
    }
    links.sort_by_key(|l| order(l.page, &l.position));
    keywords.sort_by_key(|k| order(k.page, &k.position));
    // the reference sorts titles by top then left of TITLERECTORI (docs/note-format.md 7.1)
    titles.sort_by_key(|t| {
        let r = t.rect_original.unwrap_or(t.rect);
        (t.page, r[1], r[0])
    });

    // templates (docs/note-format.md 4.1, 5.4)
    let mut templates = Vec::new();
    for (key, value) in style_refs {
        let name = key.strip_prefix("STYLE_").unwrap_or(key).to_owned();
        let bitmap = rx.address_value("footer", key, value)?;
        let users = pages
            .iter()
            .filter(|p| {
                let style = p.style.as_deref().unwrap_or("");
                // custom templates carry the md5 in the footer name
                let with_md5 = format!("{style}{}", p.style_md5.as_deref().unwrap_or(""));
                style == name || with_md5 == name
            })
            .map(|p| p.index)
            .collect();
        templates.push(Template {
            name,
            key: key.to_owned(),
            bitmap,
            pages: users,
        });
    }

    // covers (docs/note-format.md 4.1, 6.3)
    let mut covers = Vec::new();
    for (key, number, value) in cover_refs {
        let block = rx.address_value("footer", key, value)?;
        covers.push(Cover {
            key: key.to_owned(),
            number,
            data: block.map(|b| rx.payload(b).to_vec()),
            block,
        });
    }

    // HIGHLIGHTINFO is address-shaped (docs/note-format.md 4.2)
    let _ = rx.soft_address("header", &header, "HIGHLIGHTINFO");

    Ok(Current {
        header_address: header_ref.map(|r| r.address),
        created: file_id.as_deref().and_then(id_timestamp),
        file_id,
        equipment,
        device_known,
        width,
        height,
        realtime_recognition,
        recognition_language: text(&header, "FILE_RECOGN_LANGUAGE"),
        dirty,
        pages,
        links,
        titles,
        keywords,
        templates,
        covers,
        header,
        footer,
        reachable: rx.reachable,
        warnings: rx.warnings.0,
    })
}

/// Save history and raw blocks from the sequential walk (docs/note-format.md 2).
fn history(
    data: &[u8],
    first_block: u64,
    reachable: &std::collections::BTreeSet<u64>,
) -> (Vec<Save>, Vec<RawBlock>, Option<String>) {
    let walk = container::walk(data, first_block);
    let mut error = walk.error;
    let mut saves = Vec::new();
    for address in walk.footers {
        let footer = container::payload(data, first_block, address)
            .map_err(|e| e.to_string())
            .and_then(Meta::parse);
        let footer = match footer {
            Ok(f) => f,
            Err(e) => {
                error.get_or_insert(format!("footer at {address}: {e}"));
                break;
            }
        };
        let header_address = footer
            .get("FILE_FEATURE")
            .and_then(parse_u32)
            .filter(|a| *a != 0)
            .map(u64::from);
        let mut page_addresses: Vec<(u32, u64)> = footer
            .iter()
            .filter_map(|(k, v)| {
                let n = k.strip_prefix("PAGE").and_then(parse_u32)?;
                Some((n, u64::from(parse_u32(v)?)))
            })
            .collect();
        page_addresses.sort_by_key(|(n, _)| *n);
        saves.push(Save {
            footer_address: address,
            footer,
            header_address,
            page_addresses,
        });
    }
    let blocks = walk
        .blocks
        .into_iter()
        .map(|(address, len)| {
            let payload = container::payload(data, first_block, address).unwrap_or_default();
            let metadata = payload.first() == Some(&b'<')
                && payload.last() == Some(&b'>')
                && Meta::parse(payload).is_ok();
            RawBlock {
                address,
                len,
                kind: if metadata {
                    BlockKind::Metadata
                } else {
                    BlockKind::Binary
                },
                reachable: reachable.contains(&address),
            }
        })
        .collect();
    (saves, blocks, error)
}

impl RawBlock {
    /// The payload of this block.
    pub fn payload<'a>(&self, notebook: &'a Notebook) -> Result<&'a [u8], NoteError> {
        notebook.block(self.address)
    }
}

impl Link {
    /// True when the link targets this notebook: `LINKFILEID` equals the file's
    /// own `FILE_ID` (docs/note-format.md 7.3).
    pub fn is_internal(&self, notebook: &Notebook) -> bool {
        self.target_file_id.is_some() && self.target_file_id == notebook.file_id
    }
}
