// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The page Markdown from the Markdown of its blocks.

/// Joins per-block Markdown into the page Markdown: blocks that render nothing are left out,
/// the others are separated by a blank line, then [`finalize`] runs.
pub fn assemble(parts: &[String]) -> String {
    let kept: Vec<&str> = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(String::as_str)
        .collect();
    finalize(&kept.join("\n\n"))
}

/// Collapses runs of blank lines and trims trailing spaces per line.
pub fn finalize(md: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in md.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        out.push_str(line);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_skips_empty_blocks_and_collapses_blank_lines() {
        let parts = vec![
            "# Title  ".to_string(),
            String::new(),
            "a\n\n\n\nb".to_string(),
        ];
        assert_eq!(assemble(&parts), "# Title\n\na\n\nb\n");
        assert_eq!(assemble(&[]), "");
    }
}
