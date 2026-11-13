//! Prose formats beyond Markdown: `.txt`, `.org`, `.rst` and `.adoc`
//! (G3).
//!
//! These files get the same extraction as Markdown for the things
//! that are plain text conventions: `[[wikilinks]]` (through the one
//! grammar in [`crate::wikilinks`]), `[text](target)` Markdown links
//! (through the one Markdown pass in [`crate::markdown_pass`], run
//! per paragraph), and `^id` anchors (through [`crate::blocks`]). They
//! differ from Markdown in three ways, all decided here:
//!
//! - **Headings** use each format's own syntax ([`headings`]): org
//!   `*`-stars, adoc `=`-signs, rst underlines and overlines. A `.txt`
//!   has none.
//! - **Native literal regions** are skipped, as Markdown fences are:
//!   org `#+BEGIN_SRC`/`#+BEGIN_EXAMPLE`/`#+BEGIN_EXPORT`/
//!   `#+BEGIN_COMMENT` ... `#+END_*`, rst `::` literal blocks and
//!   `.. code-block::`/`.. code::`/`.. sourcecode::` directives, and
//!   adoc `----` listing and `....` literal delimited blocks. Nothing
//!   inside one is a link, `^id`, heading or paragraph. An
//!   unterminated region runs to the end of the file.
//! - **Paragraphs** are runs of non-blank lines, minus heading lines,
//!   literal regions, (adoc) block-delimiter lines and (rst, org)
//!   transition / horizontal-rule lines.
//!
//! Native link syntax (org `[[file:x.org][X]]`, rst `` `t <url>`_ ``,
//! adoc `link:`/`xref:`/`<<anchor>>`) is deliberately not read, and
//! `#tag`s are only read from Markdown. The one place a native form
//! is filtered is org's `[[...]]`, which shares its brackets with the
//! wikilink grammar: see [`is_native_org_link`].
//!
//! Everything here works on a table of lines ([`Line`]) that strips
//! `\r\n` / `\n` the same way, so a CRLF copy of a file gives the same
//! line numbers and ranges as its LF copy.

use std::path::Path;

pub(crate) mod headings;

/// Which extraction rules a prose file's content follows. Decided by
/// the path's extension, case-insensitively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProseFormat {
    Markdown,
    Text,
    Org,
    Rst,
    Adoc,
}

impl ProseFormat {
    /// The format named by `path`'s extension (`md`, `markdown`, `txt`,
    /// `org`, `rst`, `adoc`, any case). Any other extension, or none,
    /// is [`ProseFormat::Markdown`]: that is what every caller got
    /// before the other formats existed, and callers only hand this
    /// crate files `tephrad` already classified as prose.
    pub fn from_path(path: &Path) -> ProseFormat {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return ProseFormat::Markdown;
        };
        let ext = ext.to_ascii_lowercase();
        match ext.as_str() {
            "txt" => ProseFormat::Text,
            "org" => ProseFormat::Org,
            "rst" => ProseFormat::Rst,
            "adoc" => ProseFormat::Adoc,
            _ => ProseFormat::Markdown,
        }
    }

    pub fn is_markdown(self) -> bool {
        self == ProseFormat::Markdown
    }
}

/// One line of the body, without its line ending.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Line<'a> {
    pub text: &'a str,
    /// Byte offset of the first byte, relative to the body.
    pub start: usize,
    /// Byte offset just past the last byte of `text` (before the
    /// line ending).
    pub end: usize,
    /// Byte offset of the next line's first byte (past the line
    /// ending; the body's length for the last line).
    pub next: usize,
}

impl Line<'_> {
    pub(crate) fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Leading whitespace width; a tab counts as 8 columns.
    pub(crate) fn indent(&self) -> usize {
        let mut width = 0;
        for c in self.text.chars() {
            match c {
                ' ' => width += 1,
                '\t' => width += 8,
                _ => break,
            }
        }
        width
    }
}

/// Splits `body` into lines. A final line with no newline counts, so a
/// missing final newline never changes any line number; a body that
/// ends in a newline has no extra empty last line.
pub(crate) fn split_lines(body: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for raw in body.split_inclusive('\n') {
        let next = start + raw.len();
        let text = raw.trim_end_matches(['\n', '\r']);
        out.push(Line {
            text,
            start,
            end: start + text.len(),
            next,
        });
        start = next;
    }
    out
}

/// Marks every line that sits inside a native literal region of
/// `format`, delimiters and directive lines included. Always all
/// `false` for Markdown and plain text.
pub(crate) fn literal_mask(format: ProseFormat, lines: &[Line<'_>]) -> Vec<bool> {
    match format {
        ProseFormat::Org => org_literal_mask(lines),
        ProseFormat::Adoc => adoc_literal_mask(lines),
        ProseFormat::Rst => rst_literal_mask(lines),
        ProseFormat::Markdown | ProseFormat::Text => vec![false; lines.len()],
    }
}

/// Org block kinds whose content is verbatim, not prose.
const ORG_LITERAL_BLOCKS: [&str; 4] = ["src", "example", "export", "comment"];

fn org_literal_mask(lines: &[Line<'_>]) -> Vec<bool> {
    let mut mask = vec![false; lines.len()];
    // The lower-cased block name we are inside, if any.
    let mut open: Option<String> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.text.trim_start();
        if let Some(name) = &open {
            mask[i] = true;
            if let Some(rest) = strip_prefix_ci(t, "#+end_") {
                if first_word(rest).eq_ignore_ascii_case(name) {
                    open = None;
                }
            }
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "#+begin_") {
            let name = first_word(rest).to_ascii_lowercase();
            if ORG_LITERAL_BLOCKS.contains(&name.as_str()) {
                mask[i] = true;
                open = Some(name);
            }
        }
    }
    mask
}

/// `s` without a case-insensitive ASCII `prefix`, if it starts with
/// it. Safe on any UTF-8 (no slicing mid-character).
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

/// A delimiter line of an adoc listing (`----`) or literal (`....`)
/// block: four or more of one of those characters and nothing else.
fn adoc_literal_delimiter(text: &str) -> Option<&str> {
    let t = text.trim_end();
    let first = t.chars().next()?;
    if (first == '-' || first == '.') && t.len() >= 4 && t.chars().all(|c| c == first) {
        Some(t)
    } else {
        None
    }
}

fn adoc_literal_mask(lines: &[Line<'_>]) -> Vec<bool> {
    let mut mask = vec![false; lines.len()];
    let mut open: Option<&str> = None;
    for (i, line) in lines.iter().enumerate() {
        match open {
            Some(delim) => {
                mask[i] = true;
                if line.text.trim_end() == delim {
                    open = None;
                }
            }
            None => {
                if let Some(delim) = adoc_literal_delimiter(line.text) {
                    mask[i] = true;
                    open = Some(delim);
                }
            }
        }
    }
    mask
}

/// rst directives whose body is literal source.
const RST_CODE_DIRECTIVES: [&str; 3] = ["code-block::", "code::", "sourcecode::"];

fn rst_literal_mask(lines: &[Line<'_>]) -> Vec<bool> {
    let n = lines.len();
    let mut mask = vec![false; n];
    let mut i = 0usize;
    while i < n {
        if lines[i].is_blank() {
            i += 1;
            continue;
        }
        let indent = lines[i].indent();
        let t = lines[i].text.trim();
        let is_explicit_markup = t == ".." || t.starts_with(".. ") || t.starts_with("..\t");
        if is_explicit_markup {
            let rest = t[2..].trim_start();
            let is_code = RST_CODE_DIRECTIVES.iter().any(|d| rest.starts_with(d));
            if is_code {
                // The directive line, its options and its indented
                // body: everything up to the first line that is not
                // indented deeper than the directive.
                let end = deeper_block_end(lines, i, indent);
                for m in &mut mask[i..=end] {
                    *m = true;
                }
                i = end + 1;
            } else {
                i += 1;
            }
            continue;
        }

        // An ordinary paragraph: consecutive non-blank lines.
        let mut p_end = i;
        while p_end + 1 < n && !lines[p_end + 1].is_blank() {
            p_end += 1;
        }
        // A paragraph ending in `::` introduces an indented literal
        // block after one or more blank lines.
        if lines[p_end].text.trim_end().ends_with("::") {
            let mut j = p_end + 1;
            while j < n && lines[j].is_blank() {
                j += 1;
            }
            if j < n && lines[j].indent() > indent {
                let end = deeper_block_end(lines, j, indent);
                for m in &mut mask[j..=end] {
                    *m = true;
                }
                i = end + 1;
                continue;
            }
        }
        i = p_end + 1;
    }
    mask
}

/// Index of the last non-blank line of the block that starts at
/// `from` and continues over blank lines and lines indented deeper
/// than `base_indent`. `from` itself is always part of it.
fn deeper_block_end(lines: &[Line<'_>], from: usize, base_indent: usize) -> usize {
    let mut last = from;
    let mut k = from + 1;
    while k < lines.len() {
        if lines[k].is_blank() {
            k += 1;
            continue;
        }
        if lines[k].indent() > base_indent {
            last = k;
            k += 1;
        } else {
            break;
        }
    }
    last
}

/// adoc block delimiters that structure the document without being
/// text: example `====`, sidebar `****`, quote `____`, open `--`,
/// pass-through `++++` and table `|===`. They split paragraphs and are
/// never headings.
pub(crate) fn is_adoc_block_delimiter(text: &str) -> bool {
    let t = text.trim_end();
    if t == "--" || t == "|===" {
        return true;
    }
    let Some(first) = t.chars().next() else {
        return false;
    };
    matches!(first, '=' | '*' | '_' | '+') && t.len() >= 4 && t.chars().all(|c| c == first)
}

/// Whether line `idx` is a standalone rule, not text: an rst
/// transition (four or more of one punctuation character) or an org
/// horizontal rule (five or more `-`), with a blank line (or the file
/// edge) on both sides. Always `false` for the other formats.
pub(crate) fn is_transition(format: ProseFormat, lines: &[Line<'_>], idx: usize) -> bool {
    let t = lines[idx].text.trim_end();
    let Some(first) = t.chars().next() else {
        return false;
    };
    if !t.chars().all(|c| c == first) {
        return false;
    }
    let long_enough = match format {
        ProseFormat::Rst => first.is_ascii_punctuation() && t.chars().count() >= 4,
        ProseFormat::Org => first == '-' && t.chars().count() >= 5,
        _ => false,
    };
    long_enough
        && (idx == 0 || lines[idx - 1].is_blank())
        && (idx + 1 == lines.len() || lines[idx + 1].is_blank())
}

/// Org link types: a `[[type:...]]` with one of these is org's own
/// link syntax, not a wikilink.
const ORG_LINK_TYPES: [&str; 31] = [
    "file",
    "id",
    "http",
    "https",
    "ftp",
    "mailto",
    "news",
    "shell",
    "elisp",
    "doi",
    "info",
    "man",
    "help",
    "irc",
    "attachment",
    "docview",
    "bbdb",
    "gnus",
    "rmail",
    "mhe",
    "w3m",
    "eww",
    "roam",
    "geo",
    "cite",
    "calc",
    "tel",
    "xmpp",
    "news+",
    "mid",
    "file+sys",
];

/// Whether the inner text of a `[[...]]` is org's native link syntax:
/// it contains `][` (`[[target][description]]`) or starts with an org
/// link type and a colon (`[[id:...]]`, `[[file:...]]`,
/// `[[https://...]]`). Such a link is not a wikilink. A plain
/// `[[Foo]]` in an org file is one.
pub(crate) fn is_native_org_link(inner: &str) -> bool {
    if inner.contains("][") {
        return true;
    }
    match inner.split_once(':') {
        Some((kind, _)) => ORG_LINK_TYPES.iter().any(|t| kind.eq_ignore_ascii_case(t)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_picks_the_format_case_insensitively() {
        for (name, format) in [
            ("a.md", ProseFormat::Markdown),
            ("a.MARKDOWN", ProseFormat::Markdown),
            ("NOTES.TXT", ProseFormat::Text),
            ("Plan.Org", ProseFormat::Org),
            ("x.RsT", ProseFormat::Rst),
            ("x.AdOc", ProseFormat::Adoc),
            ("noext", ProseFormat::Markdown),
        ] {
            assert_eq!(ProseFormat::from_path(Path::new(name)), format, "{name}");
        }
    }

    #[test]
    fn split_lines_strips_cr_lf_and_keeps_a_last_line_without_newline() {
        let lf = split_lines("a\nb\nc");
        let crlf = split_lines("a\r\nb\r\nc");
        let texts = |ls: &[Line<'_>]| ls.iter().map(|l| l.text.to_string()).collect::<Vec<_>>();
        assert_eq!(texts(&lf), ["a", "b", "c"]);
        assert_eq!(texts(&crlf), ["a", "b", "c"]);
        assert_eq!(split_lines("a\n").len(), 1);
        assert_eq!(split_lines("").len(), 0);
    }

    #[test]
    fn org_native_links_are_recognised() {
        for inner in ["file:x.org][X", "https://x.org][X", "id:abc", "FILE:x.org"] {
            assert!(is_native_org_link(inner), "{inner}");
        }
        for inner in ["Foo", "Foo|alias", "@src:Foo", "Foo#Heading", "a: b"] {
            assert!(!is_native_org_link(inner), "{inner}");
        }
    }
}
