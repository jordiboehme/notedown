//! Text normalization, similarity and the word diff of a block.

use similar::{ChangeTag, TextDiff};

/// How a block changed, for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordDiff {
    /// The text with `[-removed-]` and `[+added+]` markers.
    Inline(String),
    /// More than half of the words changed: the texts are given in full.
    Full,
}

const MERMAID_TOKENS: [&str; 9] = ["-->", "---", "==>", "[", "]", "(", ")", "{", "}"];

fn collapse(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn mermaid_line(line: &str) -> String {
    let mut line = collapse(line);
    for token in MERMAID_TOKENS {
        line = line
            .replace(&format!(" {token}"), token)
            .replace(&format!("{token} "), token);
    }
    line
}

/// Collapses whitespace; inside mermaid blocks also layout that does not matter.
pub fn normalize(text: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut mermaid: Option<Vec<String>> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(lines) = mermaid.as_mut() {
            if trimmed.starts_with("```") {
                lines.push("```".into());
                parts.push(lines.join("\n"));
                mermaid = None;
            } else if !trimmed.is_empty() {
                lines.push(mermaid_line(trimmed));
            }
        } else if trimmed.starts_with("```mermaid") {
            mermaid = Some(vec!["```mermaid".into()]);
        } else {
            let collapsed = collapse(trimmed);
            if !collapsed.is_empty() {
                parts.push(collapsed);
            }
        }
    }
    if let Some(lines) = mermaid {
        parts.push(lines.join("\n"));
    }
    parts.join(" ")
}

/// True when both texts are equal after normalization.
pub fn same_text(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

/// 1 minus the Levenshtein distance over the longer length, on normalized text.
pub fn similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = normalize(a).chars().collect();
    let b: Vec<char> = normalize(b).chars().collect();
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    1.0 - previous[b.len()] as f32 / longest as f32
}

/// The word diff from `old` to `new`.
pub fn word_diff(old: &str, new: &str) -> WordDiff {
    let old_words: Vec<&str> = old.split_whitespace().collect();
    let new_words: Vec<&str> = new.split_whitespace().collect();
    let diff = TextDiff::from_slices(&old_words, &new_words);
    let mut segments: Vec<String> = Vec::new();
    let mut removed: Vec<&str> = Vec::new();
    let mut added: Vec<&str> = Vec::new();
    let mut changed = 0;
    let flush = |segments: &mut Vec<String>, removed: &mut Vec<&str>, added: &mut Vec<&str>| {
        let mut run = String::new();
        if !removed.is_empty() {
            run.push_str(&format!("[-{}-]", removed.join(" ")));
        }
        if !added.is_empty() {
            run.push_str(&format!("[+{}+]", added.join(" ")));
        }
        if !run.is_empty() {
            segments.push(run);
        }
        let n = removed.len().max(added.len());
        removed.clear();
        added.clear();
        n
    };
    for change in diff.iter_all_changes() {
        let word: &str = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                changed += flush(&mut segments, &mut removed, &mut added);
                segments.push(word.to_string());
            }
            ChangeTag::Delete => removed.push(word),
            ChangeTag::Insert => added.push(word),
        }
    }
    changed += flush(&mut segments, &mut removed, &mut added);
    if changed * 2 > old_words.len().max(new_words.len()) {
        WordDiff::Full
    } else {
        WordDiff::Inline(segments.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_does_not_count() {
        assert!(same_text("Meeting  with\n Anna ", "Meeting with Anna"));
        assert!(!same_text("Meeting with Anna", "Meeting with Anne"));
        assert_eq!(normalize("  a \t b\n\nc "), "a b c");
    }

    #[test]
    fn mermaid_layout_does_not_count() {
        let a = "Flow:\n```mermaid\ngraph TD\n  A[Start] --> B(End)\n\n  B --> C{Ok}\n```";
        let b = "Flow:\n```mermaid\ngraph TD\nA[ Start ]-->B( End )\nB --> C{ Ok }\n```";
        let c = "Flow:\n```mermaid\ngraph TD\n  A[Start] --> B(End)\n\n  B --> D{Ok}\n```";
        assert!(same_text(a, b));
        assert!(!same_text(a, c));
        // outside a mermaid block the spaces around brackets stay
        assert!(!same_text("f (x)", "f(x)"));
    }

    #[test]
    fn similarity_bounds() {
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("abc", "abc"), 1.0);
        assert_eq!(similarity("abc", "xyz"), 0.0);
        assert_eq!(similarity("abcd", "abce"), 0.75);
        assert_eq!(similarity("a b", "a  b"), 1.0);
        assert_eq!(similarity("", "abc"), 0.0);
    }

    #[test]
    fn an_inline_word_diff() {
        assert_eq!(
            word_diff("Meeting with Anna on Friday", "Meeting with Anne on Friday"),
            WordDiff::Inline("Meeting with [-Anna-][+Anne+] on Friday".into())
        );
        assert_eq!(
            word_diff("one two three four", "one three four five"),
            WordDiff::Inline("one [-two-] three four [+five+]".into())
        );
    }

    #[test]
    fn more_than_half_changed_gives_full() {
        assert_eq!(word_diff("call Bob today", "buy milk now"), WordDiff::Full);
        assert_eq!(
            word_diff("a b c d", "a x y d"),
            WordDiff::Inline("a [-b c-][+x y+] d".into())
        );
        assert_eq!(word_diff("a b c d e", "a x y z e"), WordDiff::Full);
        assert_eq!(word_diff("a b c d", "a x y z"), WordDiff::Full);
    }
}
