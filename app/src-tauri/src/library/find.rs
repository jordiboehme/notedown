//! The name search of the library (spec 3): the files whose name holds a text, below a folder
//! or in the whole library. The server has no name search, so the library walks
//! `GET /tree?recursive=true` page by page and filters here. Folders are not results: the
//! search lists notes and files.

use std::sync::atomic::Ordering;

use serde::Serialize;

use super::cache::TreePage;
use super::{Entry, EntryKind, Library, LibraryError};

/// The page size of the walk: the largest the server gives.
const PAGE: &str = "1000";

/// What [`Library::find`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub entries: Vec<Entry>,
    /// There were more matches than the limit.
    pub more: bool,
}

/// True when `name` holds `text`, without case. An empty text matches nothing.
pub fn name_matches(name: &str, text: &str) -> bool {
    !text.is_empty() && name.to_lowercase().contains(&text.to_lowercase())
}

impl Library {
    /// Begins a find and answers its number. A walk stops when a newer find began: the window
    /// only takes the newest answer, so an older walk would only load the server.
    pub(crate) fn begin_find(&self) -> u64 {
        self.inner.finds.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The first `limit` files below the folder `under` ("" is the whole library) whose name
    /// holds `text`, in tree order, and whether there were more. A binding that changes during
    /// the walk, or a newer find, ends it with [`LibraryError::Changed`]. Blocking: run it off
    /// the main thread.
    pub fn find(&self, text: &str, under: &str, limit: usize) -> Result<Found, LibraryError> {
        let mine = self.begin_find();
        let mut found = Found::default();
        if text.is_empty() {
            return Ok(found);
        }
        let session = self.session()?;
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![("path", under), ("recursive", "true"), ("limit", PAGE)];
            if let Some(cursor) = cursor.as_deref() {
                query.push(("cursor", cursor));
            }
            let result = session.rest().get_json::<TreePage>("/tree", &query);
            self.settle(&session, &result);
            let page = result.map_err(LibraryError::Rest)?;
            let stale = self
                .session()
                .map_or(true, |now| now.generation() != session.generation());
            if stale || session.is_cancelled() || self.inner.finds.load(Ordering::SeqCst) != mine {
                return Err(LibraryError::Changed);
            }
            for entry in page.entries {
                if entry.kind != EntryKind::File || !name_matches(&entry.name, text) {
                    continue;
                }
                if found.entries.len() == limit {
                    found.more = true;
                    return Ok(found);
                }
                found.entries.push(entry);
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(found),
            }
        }
    }
}
