//! What a code file contributes to the `^^` full-text index (G10):
//! comment blocks and identifier words.
//!
//! # Comment blocks
//!
//! A **comment block** is the unit the index stores for comments:
//!
//! - a run of consecutive line comments (`//`, `///`, `//!`, `#`, `--`,
//!   `;`...), each on a line of its own with no blank or code line
//!   between, is one block;
//! - one block comment (`/* */`, `/** */`, `<!-- -->`, a Lua
//!   `--[[ ]]`) is one block, and so is one Python docstring;
//! - a comment that follows code on its line (`x = 1  # why`) is a block
//!   of its own and never joins the line comments around it.
//!
//! A block is indexed as one row at its first line. The text has each
//! line's comment marker removed (`//`, `#`, `/*`, a leading `*`, `*/`,
//! the docstring quotes) and any trailing `^id` token cut off (an ID
//! comment is a block ID, not text to search; the symbol or paragraph
//! that owns it carries the ID in its own row).
//!
//! The ranges come from [`super::comments::find_ranges`], so a string
//! literal that merely contains a comment marker or the word being
//! searched for is never a comment block (F5).
//!
//! # Identifier words
//!
//! [`split_identifier`] turns a symbol name into the words a person
//! would search for, so `conflict reason` finds `ConflictReason` even
//! though no trigram of the query is a substring of the name's
//! spelling.

use super::comments::ByteRange;
use crate::blocks::BlockAnchor;
use crate::line_index::LineIndex;

/// One comment block of a code file. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommentBlock {
    /// 1-based line of the first comment of the block.
    pub start_line: usize,
    /// 1-based line of the last one.
    pub end_line: usize,
    /// The block's text: one line per comment line, markers and
    /// trailing `^id` tokens removed, empty lines dropped. Never empty.
    pub text: String,
    /// The `^id` an ID comment inside this block gives it, when no
    /// symbol owns that ID. `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
}

/// Groups comment/docstring `ranges` (sorted, non-overlapping, as
/// [`super::comments::find_ranges`] returns them) of `src` into
/// [`CommentBlock`]s. `anchors` are the file's block IDs: an ID with no
/// owning symbol, on a line inside a block, becomes the block's
/// `block_id`. A block whose text is empty once markers and IDs are
/// removed (an ID-only comment) is dropped.
pub(crate) fn group_comments(
    src: &str,
    ranges: &[ByteRange],
    lines: &LineIndex,
    anchors: &[BlockAnchor],
) -> Vec<CommentBlock> {
    // (start_line, end_line, mergeable, cleaned lines)
    struct Run {
        start_line: usize,
        end_line: usize,
        mergeable: bool,
        lines: Vec<String>,
    }
    let mut runs: Vec<Run> = Vec::new();

    for &(start, end) in ranges {
        let Some(text) = src.get(start..end) else {
            continue;
        };
        let text = text.trim_end();
        if text.is_empty() {
            continue;
        }
        let start_line = lines.line_number(start);
        let end_line = lines.line_number(start + text.len().saturating_sub(1));
        let own_line = src[..start]
            .rsplit('\n')
            .next()
            .is_some_and(|before| before.trim().is_empty());
        let mergeable = own_line && start_line == end_line && is_line_comment(text);
        let cleaned: Vec<String> = text.lines().map(clean_comment_line).collect();

        match runs.last_mut() {
            Some(prev) if mergeable && prev.mergeable && prev.end_line + 1 == start_line => {
                prev.end_line = end_line;
                prev.lines.extend(cleaned);
            }
            _ => runs.push(Run {
                start_line,
                end_line,
                mergeable,
                lines: cleaned,
            }),
        }
    }

    runs.into_iter()
        .filter_map(|run| {
            let text = run
                .lines
                .iter()
                .map(String::as_str)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                return None;
            }
            let block_id = anchors
                .iter()
                .find(|a| a.symbol.is_none() && (run.start_line..=run.end_line).contains(&a.line))
                .map(|a| a.block_id.clone());
            Some(CommentBlock {
                start_line: run.start_line,
                end_line: run.end_line,
                text,
                block_id,
            })
        })
        .collect()
}

/// Whether the single-line comment `text` is a line comment (`// x`,
/// `# x`, `-- x`) rather than a one-line block comment (`/* x */`,
/// `<!-- x -->`, Lua's `--[[ x ]]`) or a docstring (`"""x"""`).
fn is_line_comment(text: &str) -> bool {
    if text.starts_with("/*") || text.starts_with("<!--") || text.starts_with("{-") {
        return false;
    }
    if let Some(rest) = text.strip_prefix("--[") {
        // `--[[`, `--[=[`...: a Lua long bracket.
        let rest = rest.trim_start_matches('=');
        if rest.starts_with('[') {
            return false;
        }
    }
    // A Python docstring starts with its quotes or an `r`/`u` prefix.
    let unprefixed = text.trim_start_matches(['r', 'R', 'u', 'U']);
    !(unprefixed.starts_with('"') || unprefixed.starts_with('\''))
}

/// One comment line with its marker, a block comment's decoration and a
/// trailing `^id` removed, trimmed.
fn clean_comment_line(line: &str) -> String {
    let mut t = line.trim();
    t = t.trim_start_matches(['/', '#', '-', '!', ';', '*', '"', '\'']);
    t = t.trim();
    for closer in ["*/", "-->", "\"\"\"", "'''"] {
        if let Some(rest) = t.strip_suffix(closer) {
            t = rest.trim_end();
            break;
        }
    }
    crate::blocks::strip_trailing_block_id(t)
        .0
        .trim()
        .to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Upper,
    /// A lower-case letter, or a letter with no case (CJK).
    Lower,
    Digit,
    Separator,
}

fn class_of(c: char) -> Class {
    if c.is_uppercase() {
        Class::Upper
    } else if c.is_alphabetic() {
        Class::Lower
    } else if c.is_numeric() {
        Class::Digit
    } else {
        Class::Separator
    }
}

/// The words of an identifier, lower-cased and joined by single
/// spaces: `ConflictReason` -> `conflict reason`, `walk_and_write` ->
/// `walk and write`.
///
/// A word ends at a separator (`_`, `-`, `.`, `:`, anything that is not
/// a letter or digit), before an upper-case letter that follows a
/// lower-case letter or a digit (`camelCase`), before the last capital
/// of an acronym that a lower-case letter follows (`HTTPServer` ->
/// `http server`), and between a letter and a digit (`utf8Decode` ->
/// `utf 8 decode`, `SHA256` -> `sha 256`). A name with one word, or
/// none, is returned lower-cased as it is.
pub fn split_identifier(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let class = class_of(c);
        if class == Class::Separator {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(&prev) = i.checked_sub(1).and_then(|p| chars.get(p)) {
            let prev_class = class_of(prev);
            let next_class = chars.get(i + 1).map(|&n| class_of(n));
            let boundary = match (prev_class, class) {
                (Class::Separator, _) => false,
                (Class::Lower | Class::Digit, Class::Upper) => true,
                (Class::Upper, Class::Upper) => next_class == Some(Class::Lower),
                (Class::Digit, Class::Lower) | (Class::Lower | Class::Upper, Class::Digit) => true,
                _ => false,
            };
            if boundary && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        }
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_pascal_snake_and_screaming_case() {
        assert_eq!(split_identifier("ConflictReason"), "conflict reason");
        assert_eq!(split_identifier("conflictReason"), "conflict reason");
        assert_eq!(split_identifier("walk_and_write"), "walk and write");
        assert_eq!(split_identifier("MAX_QUERY_CHARS"), "max query chars");
        assert_eq!(split_identifier("kebab-case-name"), "kebab case name");
    }

    #[test]
    fn splits_acronyms_and_digits() {
        assert_eq!(split_identifier("HTTPServer"), "http server");
        assert_eq!(split_identifier("parseHTTPResponse"), "parse http response");
        assert_eq!(split_identifier("utf8Decode"), "utf 8 decode");
        assert_eq!(split_identifier("SHA256"), "sha 256");
        assert_eq!(split_identifier("v2"), "v 2");
        assert_eq!(split_identifier("HTTP"), "http");
    }

    #[test]
    fn plain_and_odd_names_are_lowercased_whole() {
        assert_eq!(split_identifier("walk"), "walk");
        assert_eq!(split_identifier(""), "");
        assert_eq!(split_identifier("__init__"), "init");
        assert_eq!(split_identifier("_"), "");
        assert_eq!(split_identifier("日本語Name"), "日本語 name");
    }

    fn blocks_of(src: &str, anchors: &[BlockAnchor]) -> Vec<CommentBlock> {
        // Every `//`-style or `/* */` comment of `src`, found by hand,
        // as the byte ranges `find_ranges` would give.
        let mut ranges = Vec::new();
        let mut offset = 0;
        let mut in_block: Option<usize> = None;
        for line in src.split_inclusive('\n') {
            let trimmed = line.trim_end_matches('\n');
            if let Some(start) = in_block {
                if let Some(at) = trimmed.find("*/") {
                    ranges.push((start, offset + at + 2));
                    in_block = None;
                }
            } else if let Some(at) = trimmed.find("/*") {
                if let Some(close) = trimmed[at..].find("*/") {
                    ranges.push((offset + at, offset + at + close + 2));
                } else {
                    in_block = Some(offset + at);
                }
            } else if let Some(at) = trimmed.find("//") {
                ranges.push((offset + at, offset + trimmed.len()));
            }
            offset += line.len();
        }
        group_comments(src, &ranges, &LineIndex::new(src), anchors)
    }

    #[test]
    fn consecutive_line_comments_are_one_block() {
        let src = "// one\n// two\n/// three\nfn f() {}\n";
        let blocks = blocks_of(src, &[]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            (blocks[0].start_line, blocks[0].end_line),
            (1, 3),
            "{blocks:?}"
        );
        assert_eq!(blocks[0].text, "one\ntwo\nthree");
    }

    #[test]
    fn a_blank_or_code_line_ends_the_run() {
        let src = "// a\n\n// b\nlet x = 1;\n// c\n";
        let blocks = blocks_of(src, &[]);
        let starts: Vec<usize> = blocks.iter().map(|b| b.start_line).collect();
        assert_eq!(starts, [1, 3, 5]);
    }

    #[test]
    fn one_block_comment_is_one_block_even_across_lines() {
        let src = "/**\n * Docs here.\n * More.\n */\nfn f() {}\n";
        let blocks = blocks_of(src, &[]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_line, 1);
        assert_eq!(blocks[0].end_line, 4);
        assert_eq!(blocks[0].text, "Docs here.\nMore.");
    }

    #[test]
    fn adjacent_one_line_block_comments_stay_separate_blocks() {
        let src = "/* a */\n/* b */\n";
        assert_eq!(blocks_of(src, &[]).len(), 2);
    }

    #[test]
    fn a_comment_after_code_does_not_join_the_run_below_it() {
        let src = "let x = 1; // why\n// own line\n";
        let blocks = blocks_of(src, &[]);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "why");
        assert_eq!(blocks[1].text, "own line");
    }

    #[test]
    fn id_comments_are_not_text_and_unowned_ids_label_the_block() {
        let src = "// Explains it. ^k3f9a2\n// ^other\nlet x = 1;\n// ^solo\n";
        let anchors = [
            BlockAnchor {
                block_id: "k3f9a2".into(),
                line: 1,
                symbol: None,
                token_line: 1,
            },
            BlockAnchor {
                block_id: "other".into(),
                line: 2,
                symbol: Some(0),
                token_line: 2,
            },
            BlockAnchor {
                block_id: "solo".into(),
                line: 4,
                symbol: Some(0),
                token_line: 4,
            },
        ];
        let blocks = blocks_of(src, &anchors);
        // The `^solo` block has no text left, so it is dropped.
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert_eq!(blocks[0].text, "Explains it.");
        assert_eq!(blocks[0].block_id.as_deref(), Some("k3f9a2"));
    }
}
