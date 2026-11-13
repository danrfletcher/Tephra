//! The rows a parsed file contributes to the `^^` full-text index
//! (G10), as plain owned values.
//!
//! This crate only decides *what* is indexed; the scan hands the rows
//! to `tephra-cache` in the same transaction as the unit's other rows
//! (see `tephra_cache::NewFtsRow`). Every builder is a pure function of
//! its input.
//!
//! What each kind of file gives:
//!
//! | file | rows |
//! |---|---|
//! | prose | one `heading` per heading, one `paragraph` per paragraph (with its `^id`) |
//! | code | one `symbol` per symbol, one `comment` per comment block, no headings or paragraphs |
//! | text-other | one `paragraph` per run of lines between blank lines |
//! | document | one `paragraph` per non-empty paragraph, plus a `heading` for each heading that is not also a paragraph at its place |
//! | binary, too large | nothing (the scan never calls a builder) |
//!
//! A row's text is at most [`MAX_ROW_CHARS`] characters, so one huge
//! block (a CSV with no blank lines, a minified file's single line) cannot
//! make a row unbounded. Nothing is dropped for it, though: a block over
//! the limit becomes several rows of about 2K characters, cut between
//! lines (each row at the line it starts on), and a single line longer
//! than that is cut between words, or anywhere if it has none. Only the first row of a block
//! carries its `^id`. A search phrase that straddles a cut inside one
//! over-long line is the one thing not found. A heading's or symbol's
//! name is cut at the limit, never split.

use crate::blocks::strip_trailing_block_id;
use crate::code::split_identifier;
use crate::documents::{DocLocation, ParsedDocument};
use crate::ParsedUnit;

/// The most characters one row's text holds. A longer block is split into
/// several rows (see the module docs).
pub const MAX_ROW_CHARS: usize = 16 * 1024;

/// The size a block over [`MAX_ROW_CHARS`] is split into.
const SPLIT_CHARS: usize = 2 * 1024;

/// One row of the `^^` index. See `tephra_cache::NewFtsRow` for what
/// each field means once stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    /// `paragraph`, `comment`, `symbol` or `heading`.
    pub kind: &'static str,
    /// 1-based line, for a file's row; `None` for a document's.
    pub line: Option<i64>,
    pub block_id: Option<String>,
    /// A symbol's or heading's bare name.
    pub name: Option<String>,
    /// What a symbol is shown as: `enum ConflictReason`.
    pub label: Option<String>,
    /// `page`, `slide` or `paragraph`, for a document's row.
    pub place: Option<&'static str>,
    pub place_n: Option<i64>,
    /// Position within the page or slide, for a document's row.
    pub idx: Option<i64>,
    /// What is searched.
    pub text: String,
}

impl IndexRow {
    fn file_row(kind: &'static str, line: usize, text: String) -> IndexRow {
        IndexRow {
            kind,
            line: Some(line as i64),
            block_id: None,
            name: None,
            label: None,
            place: None,
            place_n: None,
            idx: None,
            text: cap(text),
        }
    }

    /// The rows of one block of text that starts at line `first` and
    /// ends at `last`: one row, or several when the text is over
    /// [`MAX_ROW_CHARS`]. A piece's line is `first` plus the lines of
    /// text before it, never past `last` (the text can have dropped
    /// empty lines). Only the first row gets `block_id`.
    fn block_rows(
        kind: &'static str,
        first: usize,
        last: usize,
        text: &str,
        block_id: Option<String>,
    ) -> Vec<IndexRow> {
        let mut block_id = block_id;
        split_text(text)
            .into_iter()
            .map(|(offset, piece)| {
                let line = (first + offset).min(last.max(first));
                let mut row = IndexRow::file_row(kind, line, piece.to_string());
                row.block_id = block_id.take();
                row
            })
            .collect()
    }
}

fn cap(text: String) -> String {
    match text.char_indices().nth(MAX_ROW_CHARS) {
        Some((at, _)) => text[..at].to_string(),
        None => text,
    }
}

/// `text` as one piece when it is within [`MAX_ROW_CHARS`] characters;
/// otherwise cut into pieces of at most [`SPLIT_CHARS`] characters (small,
/// so the line a piece starts on is close to the line of a match in it),
/// each with the number of newlines before it (0 for the first). Pieces
/// break between lines; a line longer than a piece is cut between words
/// (at the last whitespace in the second half of the window) or, with
/// none, at the limit. Nothing is dropped except the whitespace a word
/// cut lands on.
fn split_text(text: &str) -> Vec<(usize, &str)> {
    if text.len() <= MAX_ROW_CHARS || text.chars().count() <= MAX_ROW_CHARS {
        return vec![(0, text)];
    }
    // The byte index `n` characters into `s` (or its end).
    fn byte_at(s: &str, n: usize) -> usize {
        s.char_indices().nth(n).map_or(s.len(), |(at, _)| at)
    }
    let limit = SPLIT_CHARS;
    let mut pieces: Vec<(usize, &str)> = Vec::new();
    // The current piece: byte range of `text`, its characters, its line.
    let mut cur: Option<(usize, usize, usize, usize)> = None;
    let mut offset = 0usize;
    for (line_no, line) in text.split('\n').enumerate() {
        let start = offset;
        offset += line.len() + 1;
        let chars = line.chars().count();
        if chars > limit {
            if let Some((from, to, _, first)) = cur.take() {
                pieces.push((first, &text[from..to]));
            }
            let mut rest = line;
            while !rest.is_empty() {
                let window = &rest[..byte_at(rest, limit)];
                let cut = if window.len() == rest.len() {
                    window.len()
                } else {
                    // After the last whitespace in the window's second
                    // half, else at the window's end.
                    let half = byte_at(window, limit / 2);
                    window[half..]
                        .rfind(char::is_whitespace)
                        .map_or(window.len(), |at| half + at)
                };
                let piece = rest[..cut].trim_end();
                if !piece.is_empty() {
                    pieces.push((line_no, piece));
                }
                rest = rest[cut..].trim_start();
            }
            continue;
        }
        match cur {
            Some((from, _, held, first)) if held + 1 + chars <= limit => {
                cur = Some((from, start + line.len(), held + 1 + chars, first));
            }
            Some((from, to, _, first)) => {
                pieces.push((first, &text[from..to]));
                cur = Some((start, start + line.len(), chars, line_no));
            }
            None => cur = Some((start, start + line.len(), chars, line_no)),
        }
    }
    if let Some((from, to, _, first)) = cur {
        pieces.push((first, &text[from..to]));
    }
    pieces
}

/// The lines `first..=last` (1-based) of `lines` (a file split on `\n`), with
/// a trailing `\r` and a trailing `^id` token removed from each, joined
/// by newlines, empty lines dropped.
fn lines_text(lines: &[&str], first: usize, last: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in lines
        .iter()
        .skip(first.saturating_sub(1))
        .take(last.saturating_sub(first) + 1)
    {
        let line = strip_trailing_block_id(line.trim_end_matches('\r'))
            .0
            .trim();
        if !line.is_empty() {
            out.push(line);
        }
    }
    out.join("\n")
}

impl ParsedUnit {
    /// This unit's `^^` index rows; `content` is the exact text the
    /// unit was parsed from. In file order within each kind: headings,
    /// paragraphs, comment blocks, symbols.
    pub fn index_rows(&self, content: &str) -> Vec<IndexRow> {
        let lines: Vec<&str> = content.split('\n').collect();
        let mut rows = Vec::new();

        for heading in &self.headings {
            if heading.text.trim().is_empty() {
                continue;
            }
            let mut row = IndexRow::file_row("heading", heading.line, heading.text.clone());
            row.name = Some(heading.text.trim().to_string());
            rows.push(row);
        }
        for paragraph in &self.paragraphs {
            let text = lines_text(&lines, paragraph.start_line, paragraph.end_line);
            if text.is_empty() {
                continue;
            }
            rows.extend(IndexRow::block_rows(
                "paragraph",
                paragraph.start_line,
                paragraph.end_line,
                &text,
                paragraph.block_id.clone(),
            ));
        }
        for block in &self.comment_blocks {
            rows.extend(IndexRow::block_rows(
                "comment",
                block.start_line,
                block.end_line,
                &block.text,
                block.block_id.clone(),
            ));
        }
        for (i, symbol) in self.symbols.iter().enumerate() {
            let words = split_identifier(&symbol.name);
            // The name as written, then its words: `ConflictReason
            // conflict reason`. A one-word name has nothing to add.
            let text = if words == symbol.name.to_lowercase() {
                symbol.name.clone()
            } else {
                format!("{} {}", symbol.name, words)
            };
            let mut row = IndexRow::file_row("symbol", symbol.line, text);
            row.name = Some(symbol.name.clone());
            row.label = Some(format!("{} {}", symbol.kind, symbol.name));
            row.block_id = self
                .blocks
                .iter()
                .find(|b| b.symbol == Some(i))
                .map(|b| b.block_id.clone());
            rows.push(row);
        }
        rows
    }
}

impl ParsedDocument {
    /// This document's `^^` index rows. A row has `place` (`page`,
    /// `slide` or `paragraph`), `place_n` and `idx` instead of a line,
    /// and never a `block_id`.
    pub fn index_rows(&self) -> Vec<IndexRow> {
        let place_of = |location: DocLocation| match location {
            DocLocation::Page(_) => "page",
            DocLocation::Slide(_) => "slide",
            DocLocation::Paragraph(_) => "paragraph",
        };
        let mut rows = Vec::new();
        for paragraph in &self.paragraphs {
            if paragraph.text.trim().is_empty() {
                continue;
            }
            // A paragraph over the limit is several rows at the same
            // place and index.
            for (_, piece) in split_text(paragraph.text.trim()) {
                rows.push(IndexRow {
                    kind: "paragraph",
                    line: None,
                    block_id: None,
                    name: None,
                    label: None,
                    place: Some(place_of(paragraph.location)),
                    place_n: Some(paragraph.location.number() as i64),
                    idx: Some(paragraph.index as i64),
                    text: piece.to_string(),
                });
            }
        }
        for heading in &self.headings {
            let text = heading.text.trim();
            if text.is_empty() {
                continue;
            }
            // A docx heading or a slide title is also one of the
            // paragraphs; one row for it, not two.
            let also_a_paragraph = self
                .paragraphs
                .iter()
                .any(|p| p.location == heading.location && p.text.trim() == text);
            if also_a_paragraph {
                continue;
            }
            rows.push(IndexRow {
                kind: "heading",
                line: None,
                block_id: None,
                name: Some(text.to_string()),
                label: None,
                place: Some(place_of(heading.location)),
                place_n: Some(heading.location.number() as i64),
                idx: None,
                text: cap(text.to_string()),
            });
        }
        rows
    }
}

/// The rows of a `text-other` file (G6): its text split into blocks on
/// blank lines, each one `paragraph` row at its first line (a block over
/// [`MAX_ROW_CHARS`] is several rows, each at the line it starts on). No
/// headings, no `^id`s: nothing in such a file is structure this crate
/// knows.
pub fn text_other_rows(content: &str) -> Vec<IndexRow> {
    let mut rows = Vec::new();
    let mut first = 0usize;
    let mut current: Vec<&str> = Vec::new();
    let flush = |first: usize, current: &mut Vec<&str>, rows: &mut Vec<IndexRow>| {
        if !current.is_empty() {
            let last = first + current.len() - 1;
            rows.extend(IndexRow::block_rows(
                "paragraph",
                first,
                last,
                &current.join("\n"),
                None,
            ));
            current.clear();
        }
    };
    for (i, line) in content.split('\n').enumerate() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            flush(first, &mut current, &mut rows);
        } else {
            if current.is_empty() {
                first = i + 1;
            }
            current.push(line);
        }
    }
    flush(first, &mut current, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_other_splits_on_blank_lines_and_numbers_from_the_first_line() {
        let rows = text_other_rows("a\nb\n\n\n  \nc\r\n\nd");
        let got: Vec<(i64, &str)> = rows
            .iter()
            .map(|r| (r.line.unwrap(), r.text.as_str()))
            .collect();
        assert_eq!(got, [(1, "a\nb"), (6, "c"), (8, "d")]);
        assert!(rows
            .iter()
            .all(|r| r.kind == "paragraph" && r.block_id.is_none()));
    }

    #[test]
    fn text_other_of_nothing_is_no_rows() {
        assert!(text_other_rows("").is_empty());
        assert!(text_other_rows("\n  \n\n").is_empty());
    }

    /// `n` lines of `width` characters, each `lNNNNN` then `x` padding.
    fn lines_of(n: usize, width: usize) -> String {
        (1..=n)
            .map(|i| format!("{:<width$}", format!("l{i:05}")).replace(' ', "x"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn text_within_the_limit_is_one_row() {
        let rows = text_other_rows(&"é".repeat(MAX_ROW_CHARS));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.chars().count(), MAX_ROW_CHARS);
    }

    #[test]
    fn a_long_block_is_split_between_lines_with_each_rows_own_start_line() {
        // 500 lines of 100 chars = ~50K chars in one block (no blank lines).
        let content = lines_of(500, 100);
        let rows = text_other_rows(&content);
        assert!(rows.len() >= 20, "{} rows", rows.len());
        let mut want_line = 1usize;
        let mut joined = Vec::new();
        for row in &rows {
            assert!(row.text.chars().count() <= SPLIT_CHARS);
            assert_eq!(row.line, Some(want_line as i64));
            // A row starts at a whole line, so its first word is that line's.
            assert!(
                row.text.starts_with(&format!("l{want_line:05}")),
                "{}",
                &row.text[..10]
            );
            want_line += row.text.split('\n').count();
            joined.push(row.text.clone());
        }
        // Nothing lost, nothing repeated: the rows put back together are the text.
        assert_eq!(joined.join("\n"), content);
        assert_eq!(want_line, 501);
    }

    #[test]
    fn a_needle_past_the_limit_is_in_a_row_at_its_own_line() {
        let mut content = lines_of(400, 100);
        content.push_str("\nneedle at the very end");
        let rows = text_other_rows(&content);
        let hit: Vec<_> = rows.iter().filter(|r| r.text.contains("needle")).collect();
        assert_eq!(hit.len(), 1);
        // The row starts at a line close before the needle's own (401),
        // and the needle is that many lines into it.
        let into = hit[0].text.split('\n').position(|l| l.contains("needle"));
        assert_eq!(hit[0].line.unwrap() + into.unwrap() as i64, 401);
        assert!(hit[0].line.unwrap() > 401 - 30);
    }

    #[test]
    fn a_single_line_over_the_limit_is_split_between_words_without_losing_any() {
        let words: Vec<String> = (0..20_000).map(|i| format!("w{i}")).collect();
        let line = words.join(" ");
        assert!(line.chars().count() > 3 * MAX_ROW_CHARS);
        let rows = text_other_rows(&line);
        assert!(rows.len() >= 4);
        assert!(rows.iter().all(|r| r.line == Some(1)));
        assert!(rows.iter().all(|r| r.text.chars().count() <= SPLIT_CHARS));
        // Every word is whole in exactly one row.
        let got: Vec<&str> = rows.iter().flat_map(|r| r.text.split(' ')).collect();
        assert_eq!(got, words.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn a_single_line_with_no_spaces_is_cut_at_the_limit_on_character_boundaries() {
        let rows = text_other_rows(&"é".repeat(MAX_ROW_CHARS + SPLIT_CHARS + 50));
        let sizes: Vec<usize> = rows.iter().map(|r| r.text.chars().count()).collect();
        assert_eq!(
            sizes.iter().sum::<usize>(),
            MAX_ROW_CHARS + SPLIT_CHARS + 50
        );
        assert!(sizes[..sizes.len() - 1].iter().all(|&n| n == SPLIT_CHARS));
    }

    #[test]
    fn only_the_first_row_of_a_long_block_carries_its_block_id() {
        let rows = IndexRow::block_rows(
            "paragraph",
            10,
            400,
            &lines_of(500, 100),
            Some("abc".to_string()),
        );
        assert!(rows.len() > 1);
        assert_eq!(rows[0].block_id.as_deref(), Some("abc"));
        assert!(rows[1..].iter().all(|r| r.block_id.is_none()));
        // Lines run on from the block's first line.
        assert_eq!(rows[0].line, Some(10));
        assert!(rows[1].line.unwrap() > 10);
    }
}
