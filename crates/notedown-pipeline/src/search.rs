//! The word search of `search_notes` (spec 6): over the current block texts of the notes that
//! are not deleted.
//!
//! A query and a block text are split into words at every character that is not a letter or
//! a digit, so punctuation does not count, and compared case-insensitive. A block matches when
//! every query word is one of its words (`meet` does not find `Meeting`, `flow` finds `Flow:`).
//! A hit is the anchor of the block plus one line of context: the first line of the block that
//! holds a query word.

use std::collections::BTreeSet;

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

/// The lowercase words of `text`: the runs of letters and digits.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

/// The blocks of `store` (account `slug`) that hold every word of `query`, in note and page
/// order, at most `limit`.
pub fn search(store: &TextStore, slug: &str, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let query: BTreeSet<String> = words(query).collect();
    if query.is_empty() {
        return Err(Error::Invalid {
            what: "query",
            message: "give at least one word".into(),
        });
    }
    if limit == 0 {
        return Err(Error::Invalid {
            what: "limit",
            message: "give 1 or more".into(),
        });
    }
    let mut hits = Vec::new();
    for note in store.notes()? {
        if note.deleted {
            continue;
        }
        for (i, page) in note.pages.iter().enumerate() {
            for block in &page.blocks {
                let have: BTreeSet<String> = words(&block.text).collect();
                if !query.is_subset(&have) {
                    continue;
                }
                hits.push(Hit {
                    anchor: format!(
                        "notedown://{slug}/{}/{}#{}",
                        note.key, page.page_id, block.id
                    ),
                    note: note.title.clone(),
                    page: i + 1,
                    context: context(&block.text, &query),
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
fn context(text: &str, query: &BTreeSet<String>) -> String {
    let line = text
        .lines()
        .find(|l| words(l).any(|w| query.contains(&w)))
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

    fn q(text: &str) -> BTreeSet<String> {
        words(text).collect()
    }

    #[test]
    fn the_context_is_the_first_line_with_a_word() {
        let text = "Plant tomatoes\n  and BEANS in June  \nbeans again";
        assert_eq!(context(text, &q("beans")), "and BEANS in June");
        // A word spread over two lines still gives the first line that holds one of them.
        assert_eq!(context(text, &q("june plant")), "Plant tomatoes");
    }

    #[test]
    fn words_drop_punctuation_and_case() {
        assert_eq!(
            words("Flow: A-then (B), Über!").collect::<Vec<_>>(),
            ["flow", "a", "then", "b", "über"]
        );
        // A whole word only: `meet` is no word of this line.
        assert!(!q("Meeting with Anne").contains("meet"));
    }

    #[test]
    fn a_long_line_is_cut() {
        let text = "word ".repeat(100);
        let c = context(&text, &q("word"));
        assert!(c.ends_with("..."), "{c}");
        assert!(c.chars().count() <= CONTEXT_CHARS + 3, "{c}");
    }
}
