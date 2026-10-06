//! Parser for Supernote `.note` notebooks.
//!
//! This is a clean-room implementation of `docs/note-format.md`; comments cite
//! the section a rule comes from. This crate reads the container, the save
//! history and all metadata (pages, layers, templates, links, titles, keywords,
//! covers), decodes layer bitmaps and renders pages, decodes the stroke
//! records of `TOTALPATH` ([`Notebook::strokes`], [`Notebook::ink_polylines`])
//! and the real-time recognition data ([`Notebook::recognition`]).
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

mod bitmap;
mod container;
mod error;
mod keywords;
mod links;
mod meta;
mod model;
mod notebook;
mod page;
mod recognition;
mod render;
mod resolve;
mod strokes;
mod titles;
mod values;

pub use bitmap::{CODE_TRANSPARENT, DecodedLayer, PROTOCOL_RLE, decode_rle, gray_x, gray_x2};
/// Largest accepted bitmap side and pixel count; callers that decode other images (PNG
/// pages) reuse them so every image the pipeline handles has the same bound.
pub use bitmap::{MAX_PIXELS as MAX_BITMAP_PIXELS, MAX_SIDE as MAX_BITMAP_SIDE};
pub use error::NoteError;
pub use image::RgbaImage;
pub use meta::Meta;
pub use model::{
    BlockKind, BlockRef, Cover, KNOWN_SIGNATURES, KeyPosition, Keyword, Layer, LayerInfo,
    LayerSlot, Link, LinkDirection, LinkKind, Page, RawBlock, Save, Template, Timestamp, Title,
    TitleStyle, VersionPolicy,
};
pub use notebook::Notebook;
pub use recognition::{
    ArchiveEntry, InkBink, InkStroke, RecognArchive, RecognElement, RecognText, RecognWord,
    Recognition, WordBox, decode_ink_bink, decode_recogn_file, decode_recogn_text,
};
pub use render::{RenderOptions, RenderStyle, encode_png};
pub use strokes::{
    OutlineRing, Polyline, RecordKind, StrokeBox, StrokeRecord, StrokeStatus, StrokeTrailer,
    decode_totalpath,
};
