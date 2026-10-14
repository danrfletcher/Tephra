//! Frontmatter delimiter detection and YAML parsing.
//!
//! Delimiter rule: the file's very first line must be exactly `---`
//! (ignoring a trailing `\r`, and ignoring a leading UTF-8 BOM
//! (`\u{FEFF}`) on the very first line only — common from Windows
//! editors; the content hash in `lib.rs` still covers the raw bytes
//! including that BOM, since this stripping is local to delimiter
//! detection); the frontmatter block then runs until the next line
//! that is exactly `---`. If no such closing line is ever found,
//! there is no frontmatter at all — the whole file, including that
//! leading `---`, is ordinary body content (this is deliberately
//! lenient: an unterminated block never errors, it just isn't
//! frontmatter).

use serde::Serialize;

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::line_index::line_number_at;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Frontmatter {
    /// The raw YAML text between the delimiters (no leading/trailing
    /// `---` lines), exactly as it appeared in the file.
    pub raw: String,
    /// The parsed value re-serialized as a JSON string — this is the
    /// value `tephra-cache`'s `units.frontmatter_json` column stores.
    /// `None` when the YAML failed to parse at all (see
    /// `DiagnosticKind::InvalidFrontmatterYaml`); a duplicate-key
    /// diagnostic does *not* suppress this, since serde_yaml still
    /// produces a (last-value-wins) value in that case.
    pub json: Option<String>,
    /// 1-indexed line number of the opening `---`.
    pub start_line: usize,
    /// 1-indexed line number of the closing `---`.
    pub end_line: usize,
}

/// Splits `content` into an optional frontmatter block plus the
/// remaining body, with the body's first line's 1-indexed line number
/// in the original file (so later passes over the body can report
/// absolute line numbers). Any frontmatter diagnostics are appended to
/// `diagnostics`.
pub(crate) fn extract<'a>(
    content: &'a str,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Option<Frontmatter>, &'a str, usize) {
    // A leading BOM is invisible in every editor that doesn't show
    // it, so a vault note saved by a Windows/legacy editor often has
    // one; strip it before any delimiter comparison so it doesn't
    // turn a real `---` frontmatter block into ordinary body text.
    // This is just a different starting slice of the same `'a`
    // input — the content hash computed in `lib.rs` is taken from
    // the original, un-stripped `content` argument to `parse_unit`,
    // so it still covers the raw bytes including the BOM.
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);

    let Some(first_line_end) = content.find('\n') else {
        // Single-line (or empty) file: a bare "---" with nothing
        // after it can't contain a closing delimiter either way.
        return (None, content, 1);
    };
    let first_line = content[..first_line_end].trim_end_matches('\r');
    if first_line != "---" {
        return (None, content, 1);
    }

    let after_open = &content[first_line_end + 1..];
    // Search for a line that is exactly "---" within `after_open`.
    let mut search_from = 0usize;
    let mut closing: Option<(usize, usize)> = None; // (start, end) of the closing line incl. its own newline, relative to after_open
    loop {
        let line_end_rel = after_open[search_from..].find('\n');
        let line = match line_end_rel {
            Some(rel) => &after_open[search_from..search_from + rel],
            None => &after_open[search_from..],
        };
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "---" {
            let line_start_abs = search_from;
            let consumed = match line_end_rel {
                Some(rel) => search_from + rel + 1,
                None => after_open.len(),
            };
            closing = Some((line_start_abs, consumed));
            break;
        }
        match line_end_rel {
            Some(rel) => search_from += rel + 1,
            None => break, // reached EOF without finding a closing delimiter
        }
    }

    let Some((raw_end, body_start_rel)) = closing else {
        // No closing delimiter anywhere in the file: not frontmatter.
        return (None, content, 1);
    };

    let raw = after_open[..raw_end].to_string();
    let body = &after_open[body_start_rel..];

    let start_line = 1;
    let end_line = line_number_at(content, first_line_end + 1 + raw_end);
    // Body's first line is one past the closing delimiter's line.
    let body_start_line = end_line + 1;

    let raw_start_line = start_line + 1;
    check_duplicate_keys(&raw, raw_start_line, diagnostics);

    let json = match serde_yaml::from_str::<serde_json::Value>(&raw) {
        Ok(value) => serde_json::to_string(&value).ok(),
        Err(e) => {
            let message = e.to_string();
            // serde_yaml's error message typically has two "at line
            // N column M" clauses: the first is where the parser
            // gave up (often "unexpected end of stream", which can
            // land past the whole block), and the second — "while
            // scanning/parsing ... at line N column M" — is where
            // the offending construct (e.g. an unterminated quoted
            // scalar) actually started. Prefer that second, later
            // clause; it is the more useful line to report. Both are
            // 1-indexed within `raw`. Fall back to `e.location()`
            // (the first clause) if the message has only one, or
            // none at all.
            let loc_line = last_at_line(&message)
                .or_else(|| e.location().map(|l| l.line()))
                .unwrap_or(1);
            // Map onto the absolute file line, clamped to the
            // frontmatter block so a wild value still lands inside
            // or immediately after a real line of this file.
            let absolute = (raw_start_line + loc_line.saturating_sub(1)).min(end_line);
            diagnostics.push(Diagnostic::new(
                DiagnosticKind::InvalidFrontmatterYaml,
                absolute,
                format!("malformed frontmatter YAML: {e}"),
            ));
            None
        }
    };

    (
        Some(Frontmatter {
            raw,
            json,
            start_line,
            end_line,
        }),
        body,
        body_start_line,
    )
}

/// Finds the last "at line N" occurrence in a serde_yaml error
/// message and returns `N`. serde_yaml messages are typically shaped
/// like `"found unexpected end of stream at line 3 column 1, while
/// scanning a quoted scalar at line 1 column 8"`: the last "at line"
/// clause (here, "at line 1") is the one that points at where the
/// offending construct actually started, which is more useful than
/// the first (where the parser gave up).
fn last_at_line(message: &str) -> Option<usize> {
    const MARKER: &str = "at line ";
    let start = message.rfind(MARKER)? + MARKER.len();
    let rest = &message[start..];
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    rest[..digits_end].parse::<usize>().ok()
}

/// Flags a top-level key (column 0, `key:` form) that appears more
/// than once in the frontmatter block. Only top-level keys are
/// checked — nested/indented keys under different parents are allowed
/// to repeat (e.g. two list items each with their own `name:`).
fn check_duplicate_keys(raw: &str, raw_start_line: usize, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen = std::collections::HashSet::new();
    for (idx, line) in raw.lines().enumerate() {
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            continue; // nested or list-item line, not a top-level key
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        if !seen.insert(key.to_string()) {
            diagnostics.push(Diagnostic::new(
                DiagnosticKind::DuplicateFrontmatterKey,
                raw_start_line + idx,
                format!("duplicate frontmatter key \"{key}\""),
            ));
        }
    }
}
