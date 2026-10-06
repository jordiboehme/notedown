//! Public entities of a parsed notebook (docs/note-format.md Appendix A).
//!
//! Page numbers: `Page::index`, `Notebook::page(i)`, `Template::pages` and the
//! `page` field of links, titles and keywords are 0-based positions in
//! `Notebook::pages`, so `pages[i].index == i` (`page(0)` is the first page).
//! The footer keys (`PAGE1`, `LINKO_0001...`) count from 1 on the device; one
//! key-to-position table maps them, so gapped keys still give dense positions.
//! Raw device numbers stay 1-based and unchanged: `KeyPosition::page`,
//! `Save::page_addresses`, `Keyword::keyword_page` and `Link::target_page`.

use std::fmt;

use serde::Serialize;

use crate::meta::Meta;

/// How to treat a well-formed signature that is not in [`KNOWN_SIGNATURES`]
/// (docs/note-format.md 1, "Unknown signatures").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionPolicy {
    /// Reject with [`crate::NoteError::UnknownVersion`].
    Strict,
    /// Parse with the known rules and set `Notebook::version_known` to false.
    AllowUnknown,
}

/// Every signature named in docs/note-format.md 1 ("Known signatures").
pub const KNOWN_SIGNATURES: &[&str] = &[
    "SN_FILE_ASA_20190529",
    "SN_FILE_VER_20200001",
    "SN_FILE_VER_20200005",
    "SN_FILE_VER_20200006",
    "SN_FILE_VER_20200007",
    "SN_FILE_VER_20200008",
    "SN_FILE_VER_20210009",
    "SN_FILE_VER_20210010",
    "SN_FILE_VER_20220011",
    "SN_FILE_VER_20220013",
    "SN_FILE_VER_20230014",
    "SN_FILE_VER_20230015",
    "SN_FILE_VER_20260016",
];

/// A bounds-checked reference to a block: the address of its length field and
/// the payload length (docs/note-format.md 2). Read the payload with
/// [`crate::Notebook::block`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BlockRef {
    /// Absolute offset of the block's `u32` length field.
    pub address: u64,
    /// Payload length in bytes (without the 4-byte length field).
    pub len: usize,
}

/// A `YYYYMMDDhhmmss` + microseconds timestamp, device local time
/// (docs/note-format.md 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.microsecond
        )
    }
}

/// Kind of a block found by the sequential walk (docs/note-format.md 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BlockKind {
    /// Parses as `<KEY:VALUE>` items.
    Metadata,
    /// Anything else (bitmaps, stroke data, ZIP archives, ...).
    Binary,
}

/// One block of the file in file order, current or stale (docs/note-format.md 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawBlock {
    pub address: u64,
    pub len: usize,
    pub kind: BlockKind,
    /// True when the block is reachable from the current footer.
    pub reachable: bool,
}

/// One save of the append-only history (docs/note-format.md 2, "Save markers").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Save {
    pub footer_address: u64,
    pub footer: Meta,
    /// `FILE_FEATURE` of this save's footer.
    pub header_address: Option<u64>,
    /// `(raw 1-based page number from the PAGEn key, page block address)`, sorted by number.
    pub page_addresses: Vec<(u32, u64)>,
}

/// Layer slot, named by the page key that points at the layer block
/// (docs/note-format.md 4.3, 4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum LayerSlot {
    Main,
    Layer1,
    Layer2,
    Layer3,
    Background,
}

impl LayerSlot {
    /// All slots in page-key order.
    pub const ALL: [LayerSlot; 5] = [
        LayerSlot::Main,
        LayerSlot::Layer1,
        LayerSlot::Layer2,
        LayerSlot::Layer3,
        LayerSlot::Background,
    ];

    /// The page key of this slot (`MAINLAYER`, `LAYER1`, ..., `BGLAYER`).
    pub fn key(self) -> &'static str {
        match self {
            LayerSlot::Main => "MAINLAYER",
            LayerSlot::Layer1 => "LAYER1",
            LayerSlot::Layer2 => "LAYER2",
            LayerSlot::Layer3 => "LAYER3",
            LayerSlot::Background => "BGLAYER",
        }
    }

    /// The slot for a page key or `LAYERSEQ` entry.
    pub fn from_key(key: &str) -> Option<LayerSlot> {
        LayerSlot::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// One entry of the page's `LAYERINFO` JSON (docs/note-format.md 4.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LayerInfo {
    pub layer_id: Option<i64>,
    pub name: Option<String>,
    pub is_background: Option<bool>,
    pub is_current: Option<bool>,
    pub is_visible: Option<bool>,
    pub is_deleted: Option<bool>,
    pub is_allow_add: Option<bool>,
    pub is_allow_up: Option<bool>,
    pub is_allow_down: Option<bool>,
    /// The whole JSON object, including fields not listed above.
    pub raw: serde_json::Value,
}

/// A layer of a page (docs/note-format.md 4.5). The bitmap is not decoded here.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Layer {
    pub slot: LayerSlot,
    /// The page key that points at this layer (`MAINLAYER`, `LAYER1`..`LAYER3`,
    /// `BGLAYER`); matches the entries of `Page::layer_seq`. Some firmware writes
    /// `LAYERNAME:MAINLAYER` for the background layer, so the slot key is used
    /// here and `LAYERNAME` is kept in `layer_name` (docs/note-format.md 4.5).
    pub name: String,
    /// Address of the layer metadata block.
    pub address: u64,
    /// `LAYERNAME` as written in the layer block.
    pub layer_name: Option<String>,
    /// `LAYERTYPE` (`NOTE` or `MARK`).
    pub layer_type: Option<String>,
    /// `LAYERPROTOCOL`, e.g. `RATTA_RLE`; empty when the key is missing.
    pub protocol: String,
    /// `LAYERBITMAP`: the bitmap block, `None` for address 0.
    pub bitmap: Option<BlockRef>,
    /// `LAYERPATH` (address-shaped, content unknown).
    pub path: Option<BlockRef>,
    /// `LAYERVECTORGRAPH` (address-shaped, content unknown).
    pub vector_graph: Option<BlockRef>,
    /// `LAYERRECOGN` (address-shaped, content unknown).
    pub recogn: Option<BlockRef>,
    /// Visibility from `LAYERINFO` (docs/note-format.md 4.4).
    pub visible: bool,
    /// User-visible name from `LAYERINFO`.
    pub display_name: Option<String>,
    /// The matching `LAYERINFO` entry.
    pub info: Option<LayerInfo>,
    /// All keys of the layer block.
    pub meta: Meta,
}

/// One page (docs/note-format.md 4.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page {
    /// 0-based position in `Notebook::pages` (`pages[i].index == i`).
    pub index: usize,
    /// Address of the page block.
    pub address: u64,
    /// `PAGEID`.
    pub id: Option<String>,
    /// Creation time decoded from `PAGEID` (docs/note-format.md 3.1).
    pub created: Option<Timestamp>,
    /// `PAGESTYLE`, the template name.
    pub style: Option<String>,
    /// `PAGESTYLEMD5`; `None` for `0`.
    pub style_md5: Option<String>,
    /// `ORIENTATION` (`1000` portrait, `1090`/`1270` landscape).
    pub orientation: Option<u32>,
    /// True for orientation 1090 or 1270.
    pub landscape: bool,
    /// Bitmap width of this page (swapped for landscape, docs/note-format.md 4.6).
    pub width: u32,
    /// Bitmap height of this page.
    pub height: u32,
    /// `LAYERSEQ`, first = top.
    pub layer_seq: Vec<String>,
    /// Parsed `LAYERINFO`; `None` when absent, `none` or not decodable.
    pub layer_info: Option<Vec<LayerInfo>>,
    /// Layers with a non-zero address, in slot order MAIN, 1, 2, 3, BG.
    pub layers: Vec<Layer>,
    /// `TOTALPATH` stroke data block.
    pub totalpath: Option<BlockRef>,
    /// `RECOGNSTATUS` (0 none, 1 done, 2 running).
    pub recogn_status: Option<u32>,
    /// `RECOGNFILESTATUS`.
    pub recogn_file_status: Option<u32>,
    /// `RECOGNTYPE`.
    pub recogn_type: Option<u32>,
    /// `RECOGNLANGUAGE`; `None` for `none`.
    pub recogn_language: Option<String>,
    /// `RECOGNFILE` ZIP block.
    pub recogn_file: Option<BlockRef>,
    /// `RECOGNTEXT` block.
    pub recogn_text_block: Option<BlockRef>,
    /// Recognized text from the `RECOGNTEXT` block: the labels of its `Text`
    /// elements joined with `\n` (docs/note-format.md 8.2). `None` when the page
    /// has no such block or it does not decode (a warning is recorded then).
    pub recogn_text: Option<String>,
    /// `THUMBNAILTYPE`.
    pub thumbnail_type: Option<u32>,
    /// `EXTERNALLINKINFO` (address-shaped, content unknown).
    pub external_link_info: Option<BlockRef>,
    /// `IDTABLE` (address-shaped, content unknown).
    pub id_table: Option<BlockRef>,
    /// `PAGETEXTBOX`.
    pub page_text_box: Option<u32>,
    /// `DISABLE`; `None` for `none`.
    pub disable: Option<String>,
    /// Raw `FIVESTAR` value; present only on starred pages (docs/note-format.md 4.3).
    pub five_star: Option<String>,
    /// Vector freshness indicator (docs/note-format.md 9.7): `Some(true)` when the
    /// TOTALPATH block lies after the MAINLAYER bitmap, `Some(false)` when it lies
    /// before it or is absent while a bitmap exists, `None` without a MAINLAYER bitmap.
    pub vectors_current: Option<bool>,
    /// All keys of the page block, raw.
    pub meta: Meta,
}

/// Page number and position digits of a `TITLE_`, `KEYWORD_` or `LINKO_` footer
/// key (docs/note-format.md 4.1). Groups of 4 digits: page, top, left, height, width.
/// `page` is the raw 1-based device number from the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KeyPosition {
    pub page: u32,
    pub top: Option<u32>,
    pub left: Option<u32>,
    pub height: Option<u32>,
    pub width: Option<u32>,
}

/// `LINKTYPE` (docs/note-format.md 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LinkKind {
    /// `0`: link to a page inside notes.
    Page,
    /// `1`: link to another `.note` file.
    File,
    /// `4`: web link.
    Web,
    /// Any other value (`2` is reported as a non-note document).
    Other(u32),
}

/// `LINKINOUT` (docs/note-format.md 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LinkDirection {
    /// `0`.
    Outgoing,
    /// `1`.
    Incoming,
    Other(u32),
}

/// A link (docs/note-format.md 7.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Link {
    /// The footer key, e.g. `LINKO_00010246081200840537`.
    pub key: String,
    /// Address of the link block.
    pub address: u64,
    /// 0-based page position, from the footer key's page.
    pub page: usize,
    /// Position digits of the footer key.
    pub position: KeyPosition,
    pub kind: LinkKind,
    /// `LINKINOUT`.
    pub direction: Option<LinkDirection>,
    /// `LINKFILE` decoded from base64: a device path or a URL.
    pub target: Option<String>,
    /// `LINKFILEID`; `None` for `none`.
    pub target_file_id: Option<String>,
    /// `PAGEID` of the target page; `None` for `none`.
    pub target_page_id: Option<String>,
    /// `OBJPAGE`, the raw target page number as written by the device.
    pub target_page: Option<u32>,
    /// `LINKRECT`: x (left), y (top), width, height in page pixels.
    pub rect: [u32; 4],
    /// `LINKRECTORI`.
    pub rect_original: Option<[u32; 4]>,
    /// `LINKTIMESTAMP`.
    pub timestamp: Option<Timestamp>,
    /// `LINKSTYLE`, raw.
    pub style: Option<String>,
    /// `LINKPROTOCAL` (sic), the bitmap protocol.
    pub protocol: Option<String>,
    /// `LINKBITMAP` block.
    pub bitmap_block: Option<BlockRef>,
    /// Payload of the `LINKBITMAP` block (RLE image of `rect` width x height).
    pub bitmap: Option<Vec<u8>>,
    /// All keys of the link block, raw.
    pub meta: Meta,
}

/// Decoded `TITLESTYLE` `1BBBFFF` (docs/note-format.md 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TitleStyle {
    pub code: u32,
    pub background_gray: u16,
    pub text_gray: u16,
}

/// A title (heading) (docs/note-format.md 7.1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Title {
    pub key: String,
    pub address: u64,
    /// 0-based page position, from the footer key's page.
    pub page: usize,
    pub position: KeyPosition,
    /// `TITLERECT` (falls back to `TITLERECTORI`): x, y, width, height.
    pub rect: [u32; 4],
    /// `TITLERECTORI`.
    pub rect_original: Option<[u32; 4]>,
    /// `TITLELEVEL`.
    pub level: Option<u32>,
    /// `TITLESEQNO`.
    pub seq_no: Option<u32>,
    /// Decoded `TITLESTYLE`; the raw value stays in `meta`.
    pub style: Option<TitleStyle>,
    /// `TITLEPROTOCOL`.
    pub protocol: Option<String>,
    /// `TITLEBITMAP` block.
    pub bitmap_block: Option<BlockRef>,
    /// Payload of the `TITLEBITMAP` block.
    pub bitmap: Option<Vec<u8>>,
    pub meta: Meta,
}

/// A keyword (docs/note-format.md 7.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Keyword {
    pub key: String,
    pub address: u64,
    /// 0-based page position, from the footer key's page (`KEYWORDPAGE` can be wrong).
    pub page: usize,
    pub position: KeyPosition,
    /// `KEYWORDPAGE`, the raw value as written in the block (1-based, can be wrong).
    pub keyword_page: Option<u32>,
    /// `KEYWORDRECT` (falls back to `KEYWORDRECTORI`): x, y, width, height.
    pub rect: [u32; 4],
    /// `KEYWORDRECTORI`.
    pub rect_original: Option<[u32; 4]>,
    /// `KEYWORDSEQNO`.
    pub seq_no: Option<u32>,
    /// `KEYWORDLEN`.
    pub len: Option<u32>,
    /// The keyword text: the `KEYWORDSITE` block, else the inline `KEYWORD`.
    pub text: Option<String>,
    /// Text of the `KEYWORDSITE` block.
    pub site_text: Option<String>,
    /// Inline `KEYWORD` value.
    pub inline_text: Option<String>,
    /// `KEYWORDSITE` block.
    pub site: Option<BlockRef>,
    pub meta: Meta,
}

/// A template bitmap listed in the footer as `STYLE_<name>` (docs/note-format.md 4.1, 5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Template {
    /// Style name without the `STYLE_` prefix.
    pub name: String,
    pub key: String,
    pub bitmap: Option<BlockRef>,
    /// 0-based page positions whose `PAGESTYLE` uses this template.
    pub pages: Vec<usize>,
}

/// A cover entry `COVER_n` (docs/note-format.md 4.1, 6.3). The encoding is unknown;
/// only the raw bytes are exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cover {
    pub key: String,
    pub number: u32,
    pub block: Option<BlockRef>,
    pub data: Option<Vec<u8>>,
}

impl Page {
    /// The layer in `slot`, if the page has one.
    pub fn layer(&self, slot: LayerSlot) -> Option<&Layer> {
        self.layers.iter().find(|l| l.slot == slot)
    }
}
