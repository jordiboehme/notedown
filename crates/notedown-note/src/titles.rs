//! Title blocks (docs/note-format.md 4.1, 7.1).

use crate::error::NoteError;
use crate::model::{BlockRef, KeyPosition, Title, TitleStyle};
use crate::resolve::Resolver;
use crate::values::{parse_u32, rect, required_rect, text, uint};

/// `TITLESTYLE` is 7 digits `1BBBFFF`: background gray BBB, text gray FFF
/// (docs/note-format.md 7.1).
fn style(value: &str) -> Option<TitleStyle> {
    if value.len() != 7 || !value.starts_with('1') {
        return None;
    }
    let code = parse_u32(value)?;
    Some(TitleStyle {
        code,
        background_gray: value.get(1..4)?.parse().ok()?,
        text_gray: value.get(4..7)?.parse().ok()?,
    })
}

/// Parses the title block of footer key `key`; the page comes from the key.
pub(crate) fn parse_title(
    rx: &mut Resolver<'_>,
    key: &str,
    page: usize,
    position: KeyPosition,
    block: BlockRef,
) -> Result<Title, NoteError> {
    let what = format!("title {key}");
    let meta = rx.metadata(&what, block)?;
    let w = &mut rx.warnings;
    let rect_value = required_rect(w, &what, &meta, "TITLERECT", Some("TITLERECTORI"))?;
    let rect_original = rect(w, &what, &meta, "TITLERECTORI");
    let level = uint(w, &what, &meta, "TITLELEVEL");
    let seq_no = uint(w, &what, &meta, "TITLESEQNO");
    let title_style = match text(&meta, "TITLESTYLE") {
        None => None,
        Some(v) => {
            let s = style(&v);
            if s.is_none() {
                w.push(format!("{what} TITLESTYLE: {v:?} is not 1BBBFFF"));
            }
            s
        }
    };
    let bitmap_block = rx.address(&what, &meta, "TITLEBITMAP")?;
    Ok(Title {
        key: key.to_owned(),
        address: block.address,
        page,
        position,
        rect: rect_value,
        rect_original,
        level,
        seq_no,
        style: title_style,
        protocol: text(&meta, "TITLEPROTOCOL"),
        bitmap: bitmap_block.map(|b| rx.payload(b).to_vec()),
        bitmap_block,
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::style;

    #[test]
    fn title_styles() {
        let s = style("1201000").unwrap();
        assert_eq!((s.background_gray, s.text_gray), (201, 0));
        assert_eq!(style("2000000"), None);
        assert_eq!(style("100"), None);
    }
}
