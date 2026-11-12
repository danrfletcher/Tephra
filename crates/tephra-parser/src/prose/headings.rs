//! Native heading rules for org, rst and adoc (G3). A `.txt` has no
//! headings, and Markdown's own come from `markdown_pass`.
//!
//! - org: a line that starts, at column 0, with one or more `*`
//!   followed by a space. The level is the number of stars (so it can
//!   exceed 6). `*bold*` mid-line, `*Title` (no space) and an indented
//!   `  * item` are not headings.
//! - adoc: one to six `=` at column 0 followed by a space and some
//!   text (`=` is level 1, `======` is level 6). `=Title` (no space),
//!   seven or more `=` and a bare `====` (an example-block delimiter)
//!   are not headings.
//! - rst: a title line underlined (or overlined and underlined) with
//!   a run of one punctuation character at least as wide as the
//!   title. Levels follow docutils: the first adornment style met in
//!   the file is level 1, the next new style level 2, and so on. An
//!   overline-and-underline pair is a different style from the same
//!   character used as an underline only. A lone adornment line (a
//!   transition), a table border, an underline shorter than its
//!   title, and a title that continues a paragraph are not headings.
//!
//! Lines inside a native literal region (`literal`) are never
//! headings. Every function returns the headings plus a per-line mask
//! of the lines those headings occupy (title, underline, overline),
//! which the paragraph pass keeps out of paragraphs.

use super::{Line, ProseFormat};
use crate::markdown_pass::Heading;

pub(crate) struct NativeHeadings {
    pub headings: Vec<Heading>,
    pub heading_lines: Vec<bool>,
}

pub(crate) fn find(format: ProseFormat, lines: &[Line<'_>], literal: &[bool]) -> NativeHeadings {
    let mut out = NativeHeadings {
        headings: Vec::new(),
        heading_lines: vec![false; lines.len()],
    };
    match format {
        ProseFormat::Org => star_or_equals(lines, literal, b'*', usize::MAX, &mut out),
        ProseFormat::Adoc => star_or_equals(lines, literal, b'=', 6, &mut out),
        ProseFormat::Rst => rst(lines, literal, &mut out),
        ProseFormat::Markdown | ProseFormat::Text => {}
    }
    out
}

fn level_u8(n: usize) -> u8 {
    u8::try_from(n).unwrap_or(u8::MAX)
}

/// Org (`*`, any depth) and adoc (`=`, up to `max_level`): a run of
/// `marker` at column 0, one space, then non-empty text.
fn star_or_equals(
    lines: &[Line<'_>],
    literal: &[bool],
    marker: u8,
    max_level: usize,
    out: &mut NativeHeadings,
) {
    for (i, line) in lines.iter().enumerate() {
        if literal[i] {
            continue;
        }
        let bytes = line.text.as_bytes();
        let count = bytes.iter().take_while(|&&b| b == marker).count();
        if count == 0 || count > max_level || bytes.get(count) != Some(&b' ') {
            continue;
        }
        let text = line.text[count + 1..].trim();
        if text.is_empty() {
            continue;
        }
        out.headings.push(Heading {
            level: level_u8(count),
            text: text.to_string(),
            line: i + 1,
        });
        out.heading_lines[i] = true;
    }
}

/// `(character, length)` of a line made of one repeated ASCII
/// punctuation character, with only trailing whitespace allowed.
fn adornment(text: &str) -> Option<(char, usize)> {
    let t = text.trim_end();
    let first = t.chars().next()?;
    if !first.is_ascii_punctuation() || !t.chars().all(|c| c == first) {
        return None;
    }
    Some((first, t.chars().count()))
}

fn rst(lines: &[Line<'_>], literal: &[bool], out: &mut NativeHeadings) {
    let n = lines.len();
    // Adornment styles in order of first appearance: (char, has overline).
    let mut styles: Vec<(char, bool)> = Vec::new();
    let mut level_of = |style: (char, bool)| -> u8 {
        let idx = match styles.iter().position(|s| *s == style) {
            Some(idx) => idx,
            None => {
                styles.push(style);
                styles.len() - 1
            }
        };
        level_u8(idx + 1)
    };
    // Whether the previous line was text that is not part of a title:
    // a title can't continue a paragraph.
    let mut prev_is_text = false;
    let mut i = 0usize;
    while i < n {
        if literal[i] || lines[i].is_blank() {
            prev_is_text = false;
            i += 1;
            continue;
        }
        if let Some((c, len)) = adornment(lines[i].text) {
            // Overline, title, matching underline.
            if i + 2 < n
                && !literal[i + 1]
                && !literal[i + 2]
                && !lines[i + 1].is_blank()
                && adornment(lines[i + 1].text).is_none()
                && adornment(lines[i + 2].text) == Some((c, len))
                && lines[i + 1].text.trim_end().chars().count() <= len
            {
                out.headings.push(Heading {
                    level: level_of((c, true)),
                    text: lines[i + 1].text.trim().to_string(),
                    line: i + 2,
                });
                for flag in &mut out.heading_lines[i..=i + 2] {
                    *flag = true;
                }
                prev_is_text = false;
                i += 3;
                continue;
            }
            // A transition, a table border or an unmatched adornment:
            // not a heading, and what follows is not a fresh title.
            prev_is_text = true;
            i += 1;
            continue;
        }
        // A title line underlined by the next line.
        let text = lines[i].text;
        if !prev_is_text
            && lines[i].indent() == 0
            && !text.starts_with(".. ")
            && i + 1 < n
            && !literal[i + 1]
        {
            if let Some((c, len)) = adornment(lines[i + 1].text) {
                if text.trim_end().chars().count() <= len {
                    out.headings.push(Heading {
                        level: level_of((c, false)),
                        text: text.trim().to_string(),
                        line: i + 1,
                    });
                    out.heading_lines[i] = true;
                    out.heading_lines[i + 1] = true;
                    prev_is_text = false;
                    i += 2;
                    continue;
                }
            }
        }
        prev_is_text = true;
        i += 1;
    }
}
