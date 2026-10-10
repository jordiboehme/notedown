//! The word search of `search_notes` (spec 6): over the current block texts of the notes that
//! are not deleted.
//!
//! A query is split at white space into words. A block matches when every word occurs in its
//! text, case-insensitive, also inside a longer word (`meet` finds `Meeting`). A hit is the
//! anchor of the block plus one line of context: the first line of the block that holds a
//! query word.

use serde::Serialize;

use crate::store::TextStore;
use crate::{Error, Result};

/// The number of hits when the caller gives no limit.
pub const DEFAULT_LIMIT: usize = 20;

/// The longest context line in characters; a longer line is cut and ends with `...`.
const CONTEXT_CHARS: usize = 200;

/// One block that holds every word of the query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    /// `notedown://<slug>/<key>/<pageId>#<blockId>`.
    pub anchor: String,
    /// The title of the note.
    pub note: String,
    /// The page number, from 1.
    pub page: usize,
    /// The first line of the block that holds a query word.
    pub context: String,
}

/// The blocks of `store` that hold every word of `query`, in note and page order, at most
/// `limit` (at least one).
pub fn search(store: &TextStore, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Err(Error::Invalid {
            what: "query",
            message: "give at least one word".into(),
        });
    }
    let limit = limit.max(1);
    let slug = store
        .dir()
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut hits = Vec::new();
    for note in store.notes()? {
        if note.deleted {
            continue;
        }
        for (i, page) in note.pages.iter().enumerate() {
            for block in &page.blocks {
                let lower = block.text.to_lowercase();
                if !words.iter().all(|w| lower.contains(w.as_str())) {
                    continue;
                }
                hits.push(Hit {
                    anchor: format!(
                        "notedown://{slug}/{}/{}#{}",
                        note.key, page.page_id, block.id
                    ),
                    note: note.title.clone(),
                    page: i + 1,
                    context: context(&block.text, &words),
                });
                if hits.len() == limit {
                    return Ok(hits);
                }
            }
        }
    }
    Ok(hits)
}

/// The first line of `text` that holds one of `words`, trimmed and cut to [`CONTEXT_CHARS`].
fn context(text: &str, words: &[String]) -> String {
    let line = text
        .lines()
        .find(|l| {
            let lower = l.to_lowercase();
            words.iter().any(|w| lower.contains(w.as_str()))
        })
        .or_else(|| text.lines().next())
        .unwrap_or_default()
        .trim();
    if line.chars().count() <= CONTEXT_CHARS {
        line.to_string()
    } else {
        let cut: String = line.chars().take(CONTEXT_CHARS).collect();
        format!("{}...", cut.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(q: &str) -> Vec<String> {
        q.split_whitespace().map(str::to_lowercase).collect()
    }

    #[test]
    fn the_context_is_the_first_line_with_a_word() {
        let text = "Plant tomatoes\n  and BEANS in June  \nbeans again";
        assert_eq!(context(text, &words("beans")), "and BEANS in June");
        // A word spread over two lines still gives the first line that holds one of them.
        assert_eq!(context(text, &words("june plant")), "Plant tomatoes");
    }

    #[test]
    fn a_long_line_is_cut() {
        let text = "word ".repeat(100);
        let c = context(&text, &words("word"));
        assert!(c.ends_with("..."), "{c}");
        assert!(c.chars().count() <= CONTEXT_CHARS + 3, "{c}");
    }
}
