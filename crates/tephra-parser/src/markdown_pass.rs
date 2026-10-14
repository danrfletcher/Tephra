//! The one `pulldown-cmark` pass over the body text: headings,
//! standard `[text](url)`/reference-style markdown links, and two
//! kinds of byte ranges the hand-rolled wikilink/tag/block scanners
//! need to steer clear of:
//!
//! - `excluded_ranges`: anything "code-like" or raw HTML rather than
//!   prose — fenced code blocks, indented code blocks, inline code
//!   spans, and HTML blocks/inline HTML (comments included). None of
//!   `[[wikilinks]]`, `#tags`, or `^block` anchors are ever
//!   recognized inside these.
//! - `link_destination_ranges`: just the destination text itself —
//!   excluding a title, excluding a `<`/`>`-bracketed destination's
//!   own brackets, and excluding the enclosing `(`/`)` — of every
//!   inline link or image, e.g. the `https://example.com/page#frag`
//!   in `[x](https://example.com/page#frag)`. Used only by the tag
//!   scanner, so a URL fragment or a `pic.png#x`-style destination
//!   anchor is never mistaken for a `#tag`.

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

use serde::Serialize;

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::line_index::LineIndex;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heading {
    /// 1 through 6.
    pub level: u8,
    pub text: String,
    /// 1-indexed line number, absolute within the original file.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarkdownLink {
    /// The link's visible label text.
    pub text: String,
    /// The link destination, verbatim — may be an absolute URL, a
    /// relative path, or empty (see `DiagnosticKind::EmptyMarkdownLink`).
    pub target: String,
    /// 1-indexed line number, absolute within the original file.
    pub line: usize,
    /// Absolute byte range `[start, end)` within the original file's
    /// content covering exactly the destination text itself —
    /// excluding the enclosing `(`/`)`, excluding a `<`/`>`-bracketed
    /// destination's own brackets, and excluding any following
    /// ` "title"` — `None` for a link whose destination never appears
    /// literally at a `(...)` position (e.g. a reference-style
    /// `[text][ref]` link; see [`inline_destination_range`]). R1
    /// review finding: `tephrad::refactor` rewrites a link by this
    /// exact span rather than a textual search-and-replace, so a
    /// textually-identical occurrence elsewhere on the same line
    /// (plain prose that happens to contain the same parenthesized
    /// text) is never mistaken for this one. R9 review finding: this
    /// used to be the whole span between the destination's opening
    /// `(` and the link's closing `)`, which silently included a
    /// title or a `<`/`>` destination's own brackets — `tephrad`
    /// could never actually apply a rewrite for such a link, because
    /// its byte-exact check against the destination-only replacement
    /// text always failed.
    pub span: Option<(usize, usize)>,
}

pub(crate) struct MarkdownPass {
    pub headings: Vec<Heading>,
    pub links: Vec<MarkdownLink>,
    /// Byte ranges (relative to `body`), sorted ascending, that the
    /// wikilink/tag/block scanners must never scan inside: fenced
    /// and indented code blocks, inline code spans, and HTML blocks
    /// / inline HTML.
    pub excluded_ranges: Vec<(usize, usize)>,
    /// Byte ranges (relative to `body`), sorted ascending, covering
    /// just the destination portion of every inline link or image.
    /// Consumed only by the tag scanner.
    pub link_destination_ranges: Vec<(usize, usize)>,
}

/// (link text so far, destination, starting line, destination's
/// exact byte-range in the body if it had one). Tracked while
/// walking `pulldown_cmark`'s event stream to accumulate one link's
/// pieces across the `Start(Tag::Link)`...`End(TagEnd::Link)` span.
type CurrentLink = (String, String, usize, Option<(usize, usize)>);

pub(crate) fn run(
    body: &str,
    body_start_line: usize,
    body_start_byte: usize,
    diagnostics: &mut Vec<Diagnostic>,
) -> MarkdownPass {
    let mut headings = Vec::new();
    let mut links = Vec::new();
    let mut excluded_ranges = Vec::new();
    let mut link_destination_ranges = Vec::new();

    let mut current_heading: Option<(u8, String, usize)> = None;
    let mut current_link: Option<CurrentLink> = None;
    let mut in_code_block_start: Option<usize> = None;

    let line_index = LineIndex::new(body);
    let absolute_line =
        |rel_offset: usize| body_start_line - 1 + line_index.line_number(rel_offset);

    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level_num = heading_level_to_u8(level);
                current_heading = Some((level_num, String::new(), range.start));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text, start)) = current_heading.take() {
                    headings.push(Heading {
                        level,
                        text,
                        line: absolute_line(start),
                    });
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let dest_range = inline_destination_range(body, &range);
                current_link = Some((dest_url.to_string(), String::new(), range.start, dest_range));
                if let Some(dest) = dest_range {
                    link_destination_ranges.push(dest);
                }
            }
            Event::Start(Tag::Image { .. }) => {
                // Images aren't extracted as `MarkdownLink`s (out of
                // scope for G3), but their destination still must
                // not leak a `#fragment`/`#anchor` into the tag
                // scanner.
                if let Some(dest) = inline_destination_range(body, &range) {
                    link_destination_ranges.push(dest);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((target, text, start, dest_range)) = current_link.take() {
                    let line = absolute_line(start);
                    if target.is_empty() {
                        diagnostics.push(Diagnostic::new(
                            DiagnosticKind::EmptyMarkdownLink,
                            line,
                            "markdown link has no destination",
                        ));
                    } else {
                        links.push(MarkdownLink {
                            text,
                            target,
                            line,
                            span: dest_range
                                .map(|(s, e)| (body_start_byte + s, body_start_byte + e)),
                        });
                    }
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block_start = Some(range.start);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = in_code_block_start.take() {
                    excluded_ranges.push((start, range.end));
                }
            }
            Event::Code(text) => {
                if let Some((_, buf, _)) = current_heading.as_mut() {
                    buf.push_str(&text);
                }
                if let Some((_, buf, _, _)) = current_link.as_mut() {
                    buf.push_str(&text);
                }
                excluded_ranges.push((range.start, range.end));
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                excluded_ranges.push((range.start, range.end));
            }
            Event::Text(text) => {
                if let Some((_, buf, _)) = current_heading.as_mut() {
                    buf.push_str(&text);
                }
                if let Some((_, buf, _, _)) = current_link.as_mut() {
                    buf.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, buf, _)) = current_heading.as_mut() {
                    buf.push(' ');
                }
                if let Some((_, buf, _, _)) = current_link.as_mut() {
                    buf.push(' ');
                }
            }
            _ => {}
        }
    }

    excluded_ranges.sort_unstable_by_key(|r| r.0);
    link_destination_ranges.sort_unstable_by_key(|r| r.0);

    MarkdownPass {
        headings,
        links,
        excluded_ranges,
        link_destination_ranges,
    }
}

/// For an inline link/image whose full event `range` ends at a `)`
/// — as opposed to a reference-style `[text][ref]`, whose range ends
/// at `]` with no destination written at this position at all —
/// returns the exact byte span (relative to `body`) of just the
/// destination text itself: excluding the enclosing `(`/`)`,
/// excluding a `<`/`>`-bracketed destination's own brackets, and
/// excluding any following ` "title"`.
///
/// R9 review finding: the previous version of this function found
/// the destination's opening `(` with `slice.rfind('(')` and simply
/// returned everything up to the link's own closing `)` — which
/// silently included a title string (`[d](Old.md "The title")`
/// yielded `Old.md "The title"` instead of just `Old.md`) and broke
/// outright on a destination containing its own balanced, unescaped
/// `(...)` (CommonMark permits this in the bare, non-angle-bracket
/// form: `[d](Old(2).md)`), since `rfind('(')` found that inner `(`
/// instead of the real, outer one. This version instead finds the
/// label's own closing `]` first — unambiguous, since a CommonMark
/// link label can never itself contain another link — and parses the
/// destination from directly after the `(` that must immediately
/// follow it, so an inner `(` or `"` never gets mistaken for the
/// link's own delimiters.
fn inline_destination_range(body: &str, range: &Range<usize>) -> Option<(usize, usize)> {
    if range.end == 0 || body.as_bytes().get(range.end - 1) != Some(&b')') {
        return None;
    }
    let slice = body.get(range.start..range.end)?;
    let bytes = slice.as_bytes();

    // The label (`[text]`/`![alt]`) always comes first; its closing
    // `]` is unambiguous here, since a CommonMark link label can
    // never itself contain another link.
    let mut i = 0;
    let label_close = loop {
        if i >= bytes.len() {
            return None;
        }
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b']' => break i,
            _ => i += 1,
        }
    };

    // Per CommonMark, an inline link's `(` must immediately follow
    // the label's `]` with no whitespace in between — otherwise this
    // isn't an inline link at this position at all.
    if bytes.get(label_close + 1) != Some(&b'(') {
        return None;
    }
    let mut j = label_close + 2;

    // Optional whitespace between `(` and the destination itself.
    while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }

    if bytes[j] == b'<' {
        // Angle-bracket destination: the returned span excludes the
        // brackets themselves.
        let inner_start = j + 1;
        let mut k = inner_start;
        loop {
            if k >= bytes.len() {
                return None;
            }
            match bytes[k] {
                b'\\' if k + 1 < bytes.len() => k += 2,
                b'>' => break,
                _ => k += 1,
            }
        }
        let inner_end = k;
        if inner_start > inner_end {
            return None;
        }
        return Some((range.start + inner_start, range.start + inner_end));
    }

    // Bare destination: a balanced, unescaped `(...)` nested inside
    // is part of the destination; an *unmatched* `)` or whitespace at
    // depth 0 ends it (whitespace there means a title follows).
    let dest_start = j;
    let mut depth: i32 = 0;
    loop {
        if j >= bytes.len() {
            return None;
        }
        match bytes[j] {
            b'\\' if j + 1 < bytes.len() => j += 2,
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' if depth > 0 => {
                depth -= 1;
                j += 1;
            }
            b')' => break,
            b' ' | b'\t' | b'\n' | b'\r' if depth == 0 => break,
            _ => j += 1,
        }
    }
    let dest_end = j;
    if dest_start >= dest_end {
        return None;
    }
    Some((range.start + dest_start, range.start + dest_end))
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
