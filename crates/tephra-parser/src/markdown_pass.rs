//! The one `pulldown-cmark` pass over the body text: headings,
//! standard `[text](url)`/reference-style markdown links, and the
//! byte ranges covered by fenced code blocks (used by
//! `wikilinks`/`tags`/`blocks` to skip scanning inside fences).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

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
}

pub(crate) struct MarkdownPass {
    pub headings: Vec<Heading>,
    pub links: Vec<MarkdownLink>,
    /// Byte ranges (relative to `body`) covered by fenced code
    /// blocks, in ascending order.
    pub fenced_ranges: Vec<(usize, usize)>,
}

pub(crate) fn run(
    body: &str,
    body_start_line: usize,
    diagnostics: &mut Vec<Diagnostic>,
) -> MarkdownPass {
    let mut headings = Vec::new();
    let mut links = Vec::new();
    let mut fenced_ranges = Vec::new();

    let mut current_heading: Option<(u8, String, usize)> = None;
    let mut current_link: Option<(String, String, usize)> = None;
    let mut in_fence_start: Option<usize> = None;

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
                current_link = Some((dest_url.to_string(), String::new(), range.start));
            }
            Event::End(TagEnd::Link) => {
                if let Some((target, text, start)) = current_link.take() {
                    let line = absolute_line(start);
                    if target.is_empty() {
                        diagnostics.push(Diagnostic::new(
                            DiagnosticKind::EmptyMarkdownLink,
                            line,
                            "markdown link has no destination",
                        ));
                    } else {
                        links.push(MarkdownLink { text, target, line });
                    }
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => {
                in_fence_start = Some(range.start);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = in_fence_start.take() {
                    fenced_ranges.push((start, range.end));
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, buf, _)) = current_heading.as_mut() {
                    buf.push_str(&text);
                }
                if let Some((_, buf, _)) = current_link.as_mut() {
                    buf.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, buf, _)) = current_heading.as_mut() {
                    buf.push(' ');
                }
                if let Some((_, buf, _)) = current_link.as_mut() {
                    buf.push(' ');
                }
            }
            _ => {}
        }
    }

    MarkdownPass {
        headings,
        links,
        fenced_ranges,
    }
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
