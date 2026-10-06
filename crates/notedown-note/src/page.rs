//! Page and layer blocks (docs/note-format.md 4.3, 4.4, 4.5, 4.6, 9.7).

use base64::Engine;
use serde_json::Value;

use crate::error::NoteError;
use crate::meta::Meta;
use crate::model::{BlockRef, Layer, LayerInfo, LayerSlot, Page};
use crate::recognition::decode_recogn_text;
use crate::resolve::Resolver;
use crate::values::{Warnings, id_timestamp, parse_rect, text, uint};

/// Parses the page block `block` of footer key `key` (e.g. `PAGE1`) at the
/// 0-based position `index`. Messages name the footer key.
pub(crate) fn parse_page(
    rx: &mut Resolver<'_>,
    key: &str,
    index: usize,
    block: BlockRef,
    portrait: (u32, u32),
) -> Result<Page, NoteError> {
    let what = format!("page {key}");
    let meta = rx.metadata(&what, block)?;

    let id = text(&meta, "PAGEID");
    let orientation = uint(&mut rx.warnings, &what, &meta, "ORIENTATION");
    // docs/note-format.md 4.6: landscape bitmaps have width and height swapped
    let landscape = matches!(orientation, Some(1090 | 1270));
    let (width, height) = if landscape {
        (portrait.1, portrait.0)
    } else {
        portrait
    };
    let layer_seq = text(&meta, "LAYERSEQ")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let layer_info = parse_layer_info(rx, &what, &meta);

    let mut layers = Vec::new();
    for slot in LayerSlot::ALL {
        if let Some(layer_block) = rx.address(&what, &meta, slot.key())? {
            layers.push(parse_layer(
                rx,
                &what,
                slot,
                layer_block,
                layer_info.as_deref(),
            )?);
        }
    }

    let totalpath = rx.address(&what, &meta, "TOTALPATH")?;
    let recogn_file = rx.address(&what, &meta, "RECOGNFILE")?;
    let recogn_text_block = rx.address(&what, &meta, "RECOGNTEXT")?;
    // docs/note-format.md 8.2, 10.10: base64 then JSON, both can fail
    let recogn_text = match recogn_text_block {
        Some(b) => match decode_recogn_text(rx.payload(b)) {
            Ok(t) => Some(t.text),
            Err(e) => {
                rx.warnings.push(format!("{what} RECOGNTEXT: {e}"));
                None
            }
        },
        None => None,
    };
    let external_link_info = rx.soft_address(&what, &meta, "EXTERNALLINKINFO");
    let id_table = rx.soft_address(&what, &meta, "IDTABLE");

    // docs/note-format.md 9.7: TOTALPATH after the MAINLAYER bitmap means the
    // vectors were written in the same or a later save than the bitmap.
    let main_bitmap = layers
        .iter()
        .find(|l| l.slot == LayerSlot::Main)
        .and_then(|l| l.bitmap);
    let vectors_current = match (totalpath, main_bitmap) {
        (Some(path), Some(bitmap)) => Some(path.address > bitmap.address),
        (None, Some(_)) => Some(false),
        (_, None) => None,
    };

    let w = &mut rx.warnings;
    let page_text_box = uint(w, &what, &meta, "PAGETEXTBOX");
    let disabled_rects = parse_disabled_rects(w, &what, text(&meta, "DISABLE").as_deref());
    Ok(Page {
        index,
        address: block.address,
        created: id.as_deref().and_then(id_timestamp),
        id,
        style: text(&meta, "PAGESTYLE"),
        style_md5: text(&meta, "PAGESTYLEMD5").filter(|v| v != "0"),
        orientation,
        landscape,
        width,
        height,
        layer_seq,
        layer_info,
        layers,
        totalpath,
        recogn_status: uint(w, &what, &meta, "RECOGNSTATUS"),
        recogn_file_status: uint(w, &what, &meta, "RECOGNFILESTATUS"),
        recogn_type: uint(w, &what, &meta, "RECOGNTYPE"),
        recogn_language: text(&meta, "RECOGNLANGUAGE"),
        recogn_file,
        recogn_text_block,
        recogn_text,
        thumbnail_type: uint(w, &what, &meta, "THUMBNAILTYPE"),
        external_link_info,
        id_table,
        page_text_box,
        text_box_flag: page_text_box.map(|n| n != 0),
        disabled_rects,
        disable: text(&meta, "DISABLE"),
        five_star: meta.get("FIVESTAR").map(str::to_owned),
        vectors_current,
        meta,
    })
}

/// `DISABLE`: rects `left,top,width,height`, each followed by `|`
/// (docs/note-format.md 4.3). A malformed entry is skipped with a warning.
fn parse_disabled_rects(w: &mut Warnings, what: &str, value: Option<&str>) -> Vec<[u32; 4]> {
    let mut rects = Vec::new();
    for entry in value.unwrap_or_default().split('|') {
        if entry.trim().is_empty() {
            continue;
        }
        match parse_rect(entry) {
            Some(rect) => rects.push(rect),
            None => w.push(format!("{what} DISABLE: {entry:?} is not a rect")),
        }
    }
    rects
}

fn parse_layer(
    rx: &mut Resolver<'_>,
    page: &str,
    slot: LayerSlot,
    block: BlockRef,
    layer_info: Option<&[LayerInfo]>,
) -> Result<Layer, NoteError> {
    let what = format!("{page} {}", slot.key());
    let meta = rx.metadata(&what, block)?;
    let protocol = match text(&meta, "LAYERPROTOCOL") {
        Some(p) => p,
        None => {
            rx.warnings.push(format!("{what}: no LAYERPROTOCOL"));
            String::new()
        }
    };
    let bitmap = rx.address(&what, &meta, "LAYERBITMAP")?;
    let path = rx.soft_address(&what, &meta, "LAYERPATH");
    let vector_graph = rx.soft_address(&what, &meta, "LAYERVECTORGRAPH");
    let recogn = rx.soft_address(&what, &meta, "LAYERRECOGN");
    let layer_type = text(&meta, "LAYERTYPE");

    let info = layer_info
        .and_then(|entries| matching_info(entries, slot))
        .cloned();
    // docs/note-format.md 4.4: drawn when isVisible; a layer missing from
    // LAYERINFO is visible; without LAYERINFO a .mark file shows MARK layers.
    let visible = match (&info, layer_info) {
        (Some(entry), _) => entry.is_visible.unwrap_or(true),
        (None, Some(_)) => true,
        (None, None) if rx.file_type == "mark" => layer_type.as_deref() == Some("MARK"),
        (None, None) => true,
    };

    Ok(Layer {
        slot,
        name: slot.key().to_owned(),
        address: block.address,
        layer_name: text(&meta, "LAYERNAME"),
        layer_type,
        protocol,
        bitmap,
        path,
        vector_graph,
        recogn,
        visible,
        display_name: info.as_ref().and_then(|i| i.name.clone()),
        info,
        meta,
    })
}

/// `layerId` 0 = MAINLAYER, 1..3 = LAYER1..3, `isBackgroundLayer` = BGLAYER
/// (docs/note-format.md 4.4).
fn matching_info(entries: &[LayerInfo], slot: LayerSlot) -> Option<&LayerInfo> {
    entries.iter().find(|e| {
        let background = e.is_background == Some(true);
        match slot {
            LayerSlot::Background => background,
            LayerSlot::Main => !background && e.layer_id == Some(0),
            LayerSlot::Layer1 => !background && e.layer_id == Some(1),
            LayerSlot::Layer2 => !background && e.layer_id == Some(2),
            LayerSlot::Layer3 => !background && e.layer_id == Some(3),
        }
    })
}

/// `LAYERINFO`: JSON with `#` for `:`, or base64 JSON as a fallback
/// (docs/note-format.md 4.4). Undecodable values give a warning and `None`.
fn parse_layer_info(rx: &mut Resolver<'_>, what: &str, meta: &Meta) -> Option<Vec<LayerInfo>> {
    let raw = text(meta, "LAYERINFO")?;
    let json = serde_json::from_str::<Value>(&raw.replace('#', ":"))
        .ok()
        .or_else(|| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(raw.trim())
                .ok()?;
            serde_json::from_slice::<Value>(&bytes).ok()
        });
    let Some(Value::Array(items)) = json else {
        rx.warnings
            .push(format!("{what} LAYERINFO: not a JSON array"));
        return None;
    };
    let entries = items
        .into_iter()
        .map(|item| {
            let flag = |name: &str| item.get(name).and_then(Value::as_bool);
            LayerInfo {
                layer_id: item.get("layerId").and_then(Value::as_i64),
                name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                is_background: flag("isBackgroundLayer"),
                is_current: flag("isCurrentLayer"),
                is_visible: flag("isVisible"),
                is_deleted: flag("isDeleted"),
                is_allow_add: flag("isAllowAdd"),
                is_allow_up: flag("isAllowUp"),
                is_allow_down: flag("isAllowDown"),
                raw: item,
            }
        })
        .collect();
    Some(entries)
}
