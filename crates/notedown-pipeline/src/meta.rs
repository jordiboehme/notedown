//! Metadata of a note read from the `.note` without OCR (spec 2.1).

use std::collections::BTreeMap;

use notedown_note::{DigestRef, LinkKind, Notebook};

use crate::model::{MetaLink, MetaTitle, NoteMeta};

/// Keywords, titles, links, stars, the device's own recognition text per `pageId` and the
/// ids of `digests`, the digests that point to this note.
///
/// A link keeps the `FILE_ID` of its target as `key`; that equals the page-index key of the
/// target note when it has a valid `FILE_ID`. Web links have no key. A page whose
/// recognition data cannot be read, or that has no `pageId`, gets no recognition text.
pub fn note_meta(nb: &Notebook, digests: &[DigestRef]) -> NoteMeta {
    let keywords = nb
        .keywords
        .iter()
        .filter_map(|k| k.text.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let titles = nb
        .titles
        .iter()
        .map(|t| MetaTitle {
            page: t.page,
            rect: t.rect,
            level: t.level,
        })
        .collect();
    let links = nb
        .links
        .iter()
        .map(|l| MetaLink {
            page: l.page,
            target: l.target.clone().unwrap_or_default(),
            key: match l.kind {
                LinkKind::Web => None,
                _ => l.target_file_id.clone(),
            },
        })
        .collect();
    let stars = nb
        .pages
        .iter()
        .filter(|p| p.five_star.is_some())
        .map(|p| p.index)
        .collect();
    let mut recognition = BTreeMap::new();
    for page in &nb.pages {
        let Some(id) = &page.id else { continue };
        if let Ok(Some(rec)) = nb.recognition(page.index)
            && let Some(text) = rec.text
            && !text.text.trim().is_empty()
        {
            recognition.insert(id.clone(), text.text);
        }
    }
    NoteMeta {
        keywords,
        titles,
        links,
        stars,
        recognition,
        digests: digests.iter().map(|d| d.id.clone()).collect(),
    }
}
