//! Link blocks (docs/note-format.md 4.1, 7.3).

use crate::error::{NoteError, corrupt};
use crate::model::{BlockRef, KeyPosition, Link, LinkDirection, LinkKind};
use crate::resolve::Resolver;
use crate::values::{base64_text, parse_timestamp, parse_u32, rect, required_rect, text, uint};

/// `LINKTYPE` values (docs/note-format.md 7.3).
fn kind(value: u32) -> LinkKind {
    match value {
        0 => LinkKind::Page,
        1 => LinkKind::File,
        4 => LinkKind::Web,
        other => LinkKind::Other(other),
    }
}

/// `LINKINOUT` values (docs/note-format.md 7.3).
fn direction(value: u32) -> LinkDirection {
    match value {
        0 => LinkDirection::Outgoing,
        1 => LinkDirection::Incoming,
        other => LinkDirection::Other(other),
    }
}

/// Parses the link block of footer key `key`. The page comes from the key, the
/// rect from `LINKRECT` (docs/note-format.md 4.1).
pub(crate) fn parse_link(
    rx: &mut Resolver<'_>,
    key: &str,
    page: usize,
    position: KeyPosition,
    block: BlockRef,
) -> Result<Link, NoteError> {
    let what = format!("link {key}");
    let meta = rx.metadata(&what, block)?;
    let link_type = meta
        .get("LINKTYPE")
        .and_then(parse_u32)
        .ok_or_else(|| corrupt(format!("{what}: missing or invalid LINKTYPE")))?;
    let w = &mut rx.warnings;
    let rect_value = required_rect(w, &what, &meta, "LINKRECT", None)?;
    let rect_original = rect(w, &what, &meta, "LINKRECTORI");
    let direction = uint(w, &what, &meta, "LINKINOUT").map(direction);
    let target_page = uint(w, &what, &meta, "OBJPAGE");
    let target = match text(&meta, "LINKFILE") {
        None => None,
        Some(encoded) => {
            let decoded = base64_text(&encoded);
            if decoded.is_none() {
                w.push(format!("{what} LINKFILE: not base64"));
            }
            decoded
        }
    };
    let timestamp = match text(&meta, "LINKTIMESTAMP") {
        None => None,
        Some(v) => {
            let t = parse_timestamp(&v);
            if t.is_none() {
                w.push(format!("{what} LINKTIMESTAMP: {v:?} is not a timestamp"));
            }
            t
        }
    };
    let bitmap_block = rx.address(&what, &meta, "LINKBITMAP")?;
    Ok(Link {
        key: key.to_owned(),
        address: block.address,
        page,
        position,
        kind: kind(link_type),
        direction,
        target,
        target_file_id: text(&meta, "LINKFILEID"),
        target_page_id: text(&meta, "PAGEID"),
        target_page,
        rect: rect_value,
        rect_original,
        timestamp,
        style: text(&meta, "LINKSTYLE"),
        // `LINKPROTOCAL` is the spelling in the samples (docs/note-format.md 7.3)
        protocol: text(&meta, "LINKPROTOCAL").or_else(|| text(&meta, "LINKPROTOCOL")),
        bitmap: bitmap_block.map(|b| rx.payload(b).to_vec()),
        bitmap_block,
        meta,
    })
}
