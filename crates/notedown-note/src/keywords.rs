//! Keyword blocks (docs/note-format.md 4.1, 6.3, 7.2).

use crate::error::NoteError;
use crate::model::{BlockRef, KeyPosition, Keyword};
use crate::resolve::Resolver;
use crate::values::{rect, required_rect, text, uint};

/// Parses the keyword block of footer key `key`. The page comes from the key
/// because `KEYWORDPAGE` can be wrong; the text comes from the `KEYWORDSITE`
/// block, else from the inline `KEYWORD` value (docs/note-format.md 7.2).
pub(crate) fn parse_keyword(
    rx: &mut Resolver<'_>,
    key: &str,
    page: usize,
    position: KeyPosition,
    block: BlockRef,
) -> Result<Keyword, NoteError> {
    let what = format!("keyword {key}");
    let meta = rx.metadata(&what, block)?;
    let w = &mut rx.warnings;
    let rect_value = required_rect(w, &what, &meta, "KEYWORDRECT", Some("KEYWORDRECTORI"))?;
    let rect_original = rect(w, &what, &meta, "KEYWORDRECTORI");
    let keyword_page = uint(w, &what, &meta, "KEYWORDPAGE");
    let seq_no = uint(w, &what, &meta, "KEYWORDSEQNO");
    let len = uint(w, &what, &meta, "KEYWORDLEN");
    let site = rx.address(&what, &meta, "KEYWORDSITE")?;
    let site_text = site.map(|b| String::from_utf8_lossy(rx.payload(b)).into_owned());
    let inline_text = text(&meta, "KEYWORD");
    Ok(Keyword {
        key: key.to_owned(),
        address: block.address,
        page,
        position,
        keyword_page,
        rect: rect_value,
        rect_original,
        seq_no,
        len,
        text: site_text.clone().or_else(|| inline_text.clone()),
        site_text,
        inline_text,
        site,
        meta,
    })
}
