//! `link.preview_refactor` / `link.apply_refactor` (G7/PR-8): computes
//! every incoming wikilink/markdown-link reference to a moved or
//! renamed unit across every registered source, then atomically
//! rewrites just those link spans on disk.
//!
//! Split, like the rest of this crate's domain logic, into pure
//! pieces unit-tested directly against in-memory fixtures (no
//! filesystem, no real graph) and a thin [`RefactorEngine`]
//! orchestrator that wires those pieces to the real
//! [`crate::rpc::backend::GraphBackend`]/[`crate::rpc::backend::SourceBackend`]
//! seams, real file I/O, and the watcher pause/resume control added
//! alongside it in `crate::watcher`/`crate::registry`:
//!
//! - [`compute_changes`] — pure: given the graph's backlink [`Edge`]s
//!   for the old unit and the current text content of every
//!   referring file, computes the exact line-level rewrites
//!   (`old_path` -> `new_path`'s link spans).
//! - [`apply_changes_to_content`] — pure: given one file's current
//!   content and the subset of [`LinkChange`]s that belong to it,
//!   produces the new content with only those spans rewritten.
//! - [`verify_staleness`] — pure (mtime lookup is injected): compares
//!   a preview's recorded per-file mtimes against a fresh read,
//!   aborting the whole apply if anything has changed underneath it
//!   (E3).
//! - [`rewrite_links_atomic`] — the actual disk write: temp file in
//!   the same directory, `fsync`, then `rename` (so a reader of the
//!   target path only ever observes the fully-old or fully-new
//!   content, never a partial write, and a process kill mid-write
//!   leaves the target untouched).
//! - [`RefactorEngine`] — holds the in-memory "last preview per
//!   `{source, old_path, new_path}`" state an `apply_refactor` call
//!   is checked against (an apply with no matching prior preview is
//!   rejected, per this PR's spec), the single apply-serializing
//!   lock, and the real file reads/writes and source-root lookups
//!   preview/apply need.
//!
//! - [`fix_write`] — the automatic fix's per-file writer. It shares
//!   [`compute_changes`], [`apply_changes_to_content`], [`AtomicFs`],
//!   this engine's apply lock ([`RefactorEngine::writer_gate`]) and the
//!   per-file lock, but not [`RefactorEngine`]'s preview/apply flow.
//!
//! Does not implement link *resolution* (PR-5's `tephra_graph`) or
//! ambiguous-path tie-breaking (also PR-5) — this module only ever
//! rewrites a link whose `to_raw`/`kind` the graph already told it
//! resolves to the unit being renamed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;

use tephra_graph::Edge;

use crate::registry::SourceRegistry;
use crate::rpc::backend::{GraphBackend, SourceBackend};
use crate::watcher;

pub mod fix_write;

// ---------------------------------------------------------------
// Pure diff computation
// ---------------------------------------------------------------

/// One line-level link rewrite: the exact bracketed text on one line
/// of one referring file, before and after a rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkChange {
    /// The referring unit's federated id (`"@source:rel_path"`).
    pub file: String,
    /// 1-indexed line number within that file.
    pub line: usize,
    /// The exact source bytes at `[byte_start, byte_end)` before the
    /// rewrite — for a wikilink, includes its `[[`/`]]` delimiters;
    /// for a markdown link, covers only the destination text itself,
    /// never the enclosing `(`/`)`, a title, or a `<`/`>` destination's
    /// own brackets (R9). Taken verbatim from the referring file's
    /// own content at that span (R13 review finding), never from the
    /// parser's already-unescaped/entity-decoded value: a markdown
    /// destination written with a backslash escape (`Old\(2\).md`) or
    /// an HTML entity reference (`&amp;`) has raw source bytes that
    /// differ from `tephra_parser`'s decoded `MarkdownLink::target`,
    /// and `apply_changes_to_content`'s byte-exact check against this
    /// field would otherwise always fail for such a link.
    pub before: String,
    /// The full replacement text after the rewrite, in the same raw
    /// source form as `before` (e.g. percent-encoded where writing
    /// the new destination literally would otherwise be unsafe or
    /// ambiguous — see [`locate_and_diff`]'s markdown branch).
    pub after: String,
    /// Absolute byte offset of the start of the span this change
    /// rewrites, within the referring file's own content (R1 review
    /// finding: carried straight from `tephra_parser`'s
    /// `WikiLink::span`/`MarkdownLink::span`, never recomputed by
    /// re-searching for `before`'s text).
    pub byte_start: usize,
    /// Absolute byte offset one past the end of that span (exclusive).
    pub byte_end: usize,
}

/// Computes every [`LinkChange`] implied by renaming `old_path` to
/// `new_path`, given `edges` (the graph's current backlink edges for
/// the old unit's federated id — see [`tephra_graph::UnitGraph::get_backlinks_all`],
/// which resolves a stale/renamed id's dangling edges too, and —
/// unlike [`tephra_graph::UnitGraph::get_backlinks`] — never
/// collapses two distinct raw occurrences in the same referring file
/// into one, even when they share a resolved target: T1 review
/// finding) and the full current text content of every referring file
/// named by those edges' `from` id.
///
/// Pure: no filesystem, no SQLite, no RPC — every input is already an
/// in-memory value, which is what lets this be tested directly
/// against small hand-built fixtures (single-source match,
/// cross-source match, zero matches, identical old/new path,
/// case-only rename, mixed-kind double-occurrence — see this module's
/// tests below) rather than only through a slower, real-filesystem
/// integration test.
///
/// `old_path == new_path` always returns an empty diff (one of this
/// PR's explicit edge cases) without even inspecting `edges`.
///
/// An edge whose `from` id is missing from `file_contents` (the
/// caller couldn't read that file — already deleted, permission
/// denied, etc.) is silently skipped: [`RefactorEngine::preview`]
/// only ever populates `file_contents` with files it could actually
/// read, so a missing entry here just means "nothing to show for
/// that file in this preview", not an error.
///
/// `edges` may contain more than one edge for the exact same `(from,
/// kind, to_raw)` triple — the same raw link occurring twice,
/// verbatim, in the same referring file produces one `tephra-cache`
/// row (hence one [`Edge`]) per occurrence (T1 review finding), and
/// [`locate_and_diff`] already finds *every* occurrence of a given
/// `(kind, to_raw)` pair in one pass. Processing such a duplicate
/// edge a second time would therefore re-locate and re-emit the exact
/// same span(s) already covered by the first, so each distinct
/// `(from, kind, to_raw)` triple is only ever dispatched to
/// `locate_and_diff` once.
pub fn compute_changes(
    edges: &[Edge],
    file_contents: &HashMap<String, String>,
    old_path: &str,
    new_path: &str,
) -> Vec<LinkChange> {
    compute_changes_impl(edges, file_contents, old_path, new_path, None)
}

/// [`compute_changes`] for the automatic fix after a rename
/// ([`crate::fix::fix_rename`]), which differs in two ways:
///
/// - Only wikilink edges are rewritten. A plain Markdown link
///   (`[text](Old.md)`) is never touched by the fix; only the manual
///   `link.apply_refactor` rewrites those (F7).
/// - A bare wikilink target is written as the new file name only when
///   `bare_resolves` is `true` (the caller asked the graph whether that
///   bare name reaches the renamed file). Otherwise it is written as the
///   full source-relative stem, the shortest form that does. A target
///   already written with a `/` always gets the full new stem.
pub fn compute_fix_changes(
    edges: &[Edge],
    file_contents: &HashMap<String, String>,
    old_path: &str,
    new_path: &str,
    bare_resolves: bool,
) -> Vec<LinkChange> {
    compute_changes_impl(
        edges,
        file_contents,
        old_path,
        new_path,
        Some(FixBare {
            plain: bare_resolves,
            prefixed: bare_resolves,
        }),
    )
}

/// [`compute_fix_changes`] with the Atlas fallback on (G20,
/// `qualify_ambiguous_links`): the bare file name of the renamed file
/// also exists in another source, so a link with no `@source:` prefix
/// is written as the full source-relative stem even when the bare name
/// still reaches the file in its own source. A link that carries a
/// prefix already names its source and keeps the rule of
/// [`compute_fix_changes`] (`bare_resolves`), so the flag never gives it
/// extra path. Only the target part of a link changes; the extension,
/// anchor and alias are kept as in the plain fix.
///
/// Which of the two the caller uses is decided in one place,
/// [`crate::fix::FixSpelling`]; nothing else chooses a spelling.
pub fn compute_fix_changes_qualifying(
    edges: &[Edge],
    file_contents: &HashMap<String, String>,
    old_path: &str,
    new_path: &str,
    bare_resolves: bool,
) -> Vec<LinkChange> {
    compute_changes_impl(
        edges,
        file_contents,
        old_path,
        new_path,
        Some(FixBare {
            plain: false,
            prefixed: bare_resolves,
        }),
    )
}

/// Whether a bare target (no `/`) may stay bare in the automatic fix,
/// for a link without an `@source:` prefix (`plain`) and for one with
/// it (`prefixed`). The two differ only with the Atlas fallback on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FixBare {
    plain: bool,
    prefixed: bool,
}

/// What one link in the moved file itself needs (see
/// [`MoveRewrite::Own`]), decided by the caller from the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnLink {
    /// An unprefixed link that resolved in the source the file left:
    /// it now names that source, so the link gains `@<source>:`.
    AddPrefix(String),
    /// A link prefixed with the source the file is in now, which
    /// resolves inside that source without the prefix: it loses it.
    DropPrefix,
    /// A link to the moved file itself: it names the new path in the
    /// shortest unique form, never with a prefix.
    SelfLink,
    /// A link inside a folder that was just made a source, spelled from
    /// the old source's root (`folder/x`, with or without the old
    /// source's prefix), to another file of that folder: it becomes
    /// this target text, relative to the new root and shortest unique,
    /// with no prefix (FR23).
    Retarget(String),
}

/// How the links in one file change when a file moves from one source
/// to another (G6), or when a source is renamed (G13,
/// [`MoveRewrite::SourceRename`]). Wikilinks only, like every automatic
/// fix (F7). Every cross-source link written carries an explicit
/// `@source:` prefix (F11); none is written bare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveRewrite {
    /// Rewrite 1: a file that is not in the source the file moved to
    /// (the one it left, or any other). A link that named the old path
    /// becomes `@<new_source>:<new stem>`; anchor and alias stay.
    OldSource {
        new_source: String,
        new_path: String,
    },
    /// Rewrite 2: a file in the source the file moved to. A link written
    /// `@<old_source>:<old path>` becomes the shortest unique target in
    /// its own source, with no prefix. `bare_resolves` is whether the
    /// bare file name reaches the moved file there.
    NewSource {
        new_path: String,
        bare_resolves: bool,
    },
    /// Rewrite 3: the moved file's own links, by the raw text of each
    /// link (the key of `links`). A link not in `links` is left alone.
    Own {
        new_path: String,
        bare_resolves: bool,
        links: HashMap<String, OwnLink>,
    },
    /// A source was renamed (G13): every link written with the prefix
    /// `@<old_source>:` is written `@<new_source>:`, whatever file it is
    /// in. Only the prefix changes; the target, the anchor (a heading or
    /// a `^block` id) and the alias are kept byte for byte.
    SourceRename {
        old_source: String,
        new_source: String,
    },
}

/// The shortest target text for `new_path` in its own source: the bare
/// file name when `bare_resolves`, else the full stem. The extension is
/// dropped (`.md`, `.markdown`); another extension stays.
fn shortest_target(new_path: &str, bare_resolves: bool) -> String {
    let (stem, _) = strip_md_ext(new_path);
    let core = if bare_resolves { basename(stem) } else { stem };
    core.to_string()
}

/// Puts a wikilink's parts back together: `[@prefix:]target[#anchor][|alias]`.
fn join_wikilink(
    prefix: Option<&str>,
    target: &str,
    anchor: Option<&str>,
    alias: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(p) = prefix {
        out.push('@');
        out.push_str(p);
        out.push(':');
    }
    out.push_str(target);
    if let Some(a) = anchor {
        out.push('#');
        out.push_str(a);
    }
    if let Some(a) = alias {
        out.push('|');
        out.push_str(a);
    }
    out
}

/// The new raw text of one wikilink under `rewrite`, or `None` when the
/// link stays as it is. Pure: the raw text in, the raw text out; the
/// anchor (heading or `^block` id) and the alias are never touched.
pub fn move_new_raw(to_raw: &str, rewrite: &MoveRewrite) -> Option<String> {
    let w = tephra_parser::parse_wikilink_inner(to_raw);
    if w.target.is_empty() {
        return None;
    }
    let anchor = w.anchor.as_deref();
    let alias = w.alias.as_deref();
    let new_raw = match rewrite {
        MoveRewrite::OldSource {
            new_source,
            new_path,
        } => {
            let (stem, _) = strip_md_ext(new_path);
            join_wikilink(Some(new_source), stem, anchor, alias)
        }
        MoveRewrite::NewSource {
            new_path,
            bare_resolves,
        } => {
            let target = shortest_target(new_path, *bare_resolves);
            join_wikilink(None, &target, anchor, alias)
        }
        MoveRewrite::SourceRename {
            old_source,
            new_source,
        } => {
            if w.source_prefix.as_deref() != Some(old_source.as_str()) {
                return None;
            }
            // The prefix is replaced in the raw text itself, so nothing
            // else of the link can change.
            let lead = to_raw.len() - to_raw.trim_start().len();
            let rest = to_raw[lead..].strip_prefix(&format!("@{old_source}:"))?;
            format!("{}@{new_source}:{rest}", &to_raw[..lead])
        }
        MoveRewrite::Own {
            new_path,
            bare_resolves,
            links,
        } => match links.get(to_raw)? {
            OwnLink::AddPrefix(source) => join_wikilink(Some(source), &w.target, anchor, alias),
            OwnLink::DropPrefix => join_wikilink(None, &w.target, anchor, alias),
            OwnLink::Retarget(target) => join_wikilink(None, target, anchor, alias),
            OwnLink::SelfLink => {
                let target = shortest_target(new_path, *bare_resolves);
                join_wikilink(None, &target, anchor, alias)
            }
        },
    };
    (new_raw != to_raw).then_some(new_raw)
}

/// [`compute_fix_changes`] for a file that moved to another source:
/// the span changes `rewrite` implies for the wikilinks in `edges`,
/// found again in the text of `file_contents`. Same shape and same
/// span-exact guarantees as the rename rewrite: only link text changes.
pub fn compute_move_changes(
    edges: &[Edge],
    file_contents: &HashMap<String, String>,
    rewrite: &MoveRewrite,
) -> Vec<LinkChange> {
    // The new text of every raw link, per file: a file is parsed ONCE, however
    // many different links it holds (a file of thousands of distinct links
    // is one parse, not one per link).
    let mut wanted: BTreeMap<&str, HashMap<&str, String>> = BTreeMap::new();
    for edge in edges {
        if edge.kind.as_str() != tephra_parser::LINK_KIND_WIKILINK {
            continue;
        }
        if !file_contents.contains_key(&edge.from) {
            continue;
        }
        let Some(new_raw) = move_new_raw(&edge.to_raw, rewrite) else {
            continue;
        };
        wanted
            .entry(edge.from.as_str())
            .or_default()
            .insert(edge.to_raw.as_str(), new_raw);
    }
    let mut out = Vec::new();
    for (file_id, links) in wanted {
        let parsed = parse_for_links(file_id, &file_contents[file_id]);
        for w in &parsed.wikilinks {
            if let Some(new_raw) = links.get(w.raw.as_str()) {
                out.push(LinkChange {
                    file: file_id.to_string(),
                    line: w.line,
                    before: format!("[[{}]]", w.raw),
                    after: format!("[[{new_raw}]]"),
                    byte_start: w.span.0,
                    byte_end: w.span.1,
                });
            }
        }
    }
    out.sort_by_key(|a| (a.file.clone(), a.line));
    out
}

/// The one body behind [`compute_changes`], [`compute_fix_changes`] and
/// [`compute_fix_changes_qualifying`]. `fix` is `None` for the manual
/// refactor (every kind, a bare target always rewritten to the new
/// basename) and `Some(..)` for the automatic fix.
fn compute_changes_impl(
    edges: &[Edge],
    file_contents: &HashMap<String, String>,
    old_path: &str,
    new_path: &str,
    fix: Option<FixBare>,
) -> Vec<LinkChange> {
    if old_path == new_path {
        return Vec::new();
    }
    let bare_ok = fix.unwrap_or(FixBare {
        plain: true,
        prefixed: true,
    });
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut out = Vec::new();
    for edge in edges {
        if fix.is_some() && edge.kind.as_str() != tephra_parser::LINK_KIND_WIKILINK {
            continue;
        }
        let Some(content) = file_contents.get(&edge.from) else {
            continue;
        };
        let key = (edge.from.clone(), edge.kind.clone(), edge.to_raw.clone());
        if !seen.insert(key) {
            // Same referring file, same link kind, same raw target
            // text as an edge already processed above — `locate_and_diff`
            // already found every occurrence of it on that first pass.
            continue;
        }
        let new_raw = if edge.kind.as_str() == tephra_parser::LINK_KIND_WIKILINK {
            rewrite_wikilink_raw(&edge.to_raw, new_path, bare_ok)
        } else {
            rewrite_markdown_raw(&edge.from, &edge.to_raw, new_path)
        };
        let Some(new_raw) = new_raw else { continue };
        out.extend(locate_and_diff(
            &edge.from,
            content,
            &edge.kind,
            &edge.to_raw,
            &new_raw,
        ));
    }
    out.sort_by_key(|a| (a.file.clone(), a.line));
    out
}

/// Re-parses `content` (one referring file) with `tephra_parser` to
/// find every occurrence of the raw link `(kind, old_raw)` together
/// with the exact byte span `tephra_parser` recorded for it, pairing
/// each with the already-computed `new_raw` replacement text. More
/// than one occurrence of the same raw link in one file (linked
/// twice) yields one [`LinkChange`] per occurrence, each with its own
/// span. R1 review finding: a plain line/text search previously
/// stood in for this, so a textually-identical but unrelated
/// occurrence on the same line — a copy of the raw link inside inline
/// code, or plain prose that happened to read like a markdown link's
/// `(target)` — could be mistaken for the real, parsed link and
/// corrupted while the real link itself was left stale. Locating by
/// `tephra_parser`'s own span instead means a change is only ever
/// produced for text the parser itself recognised as this exact link
/// occurrence — a parser that recognises a destination in an inline
/// code span or plain prose as a link at all is a parser bug to fix
/// in `tephra_parser`, not something this function can or should work
/// around with its own heuristics. A markdown link with no recorded
/// span (`MarkdownLink::span == None`, e.g. a reference-style
/// `[text][ref]` link) is skipped entirely rather than guessed at.
fn locate_and_diff(
    file_id: &str,
    content: &str,
    kind: &str,
    old_raw: &str,
    new_raw: &str,
) -> Vec<LinkChange> {
    let parsed = parse_for_links(file_id, content);
    let mut out = Vec::new();
    if kind == tephra_parser::LINK_KIND_WIKILINK {
        for w in &parsed.wikilinks {
            if w.raw == old_raw {
                out.push(LinkChange {
                    file: file_id.to_string(),
                    line: w.line,
                    before: format!("[[{old_raw}]]"),
                    after: format!("[[{new_raw}]]"),
                    byte_start: w.span.0,
                    byte_end: w.span.1,
                });
            }
        }
    } else {
        for m in &parsed.markdown_links {
            if m.target == old_raw {
                let Some((byte_start, byte_end)) = m.span else {
                    // No exact byte span available for this link
                    // occurrence (see `MarkdownLink::span`'s doc
                    // comment) — never fall back to a textual
                    // search-and-replace that could rewrite the
                    // wrong thing; simply report no change here
                    // rather than guess.
                    continue;
                };
                if byte_end > content.len()
                    || !content.is_char_boundary(byte_start)
                    || !content.is_char_boundary(byte_end)
                {
                    continue;
                }
                // R9 review finding: `m.span` (and hence `before`)
                // covers only the destination text itself — never a
                // surrounding `(`/`)`, a title, or a `<`/`>`
                // destination's own brackets — since that's the only
                // text `tephra_parser` actually recorded a byte-exact
                // span for. Wrapping `before`/`after` in literal
                // parens here (as a prior version of this function
                // did) silently promised a rewrite this function
                // could never perform for a titled, angle-bracket, or
                // paren-containing destination: `apply_changes_to_content`'s
                // byte-exact check against the recorded span would
                // always fail, because the span never covered the
                // title/brackets wrapped into `before`.
                //
                // R13 review finding: `old_raw`/`m.target` is
                // `pulldown-cmark`'s already backslash-unescaped,
                // entity-decoded destination — but the recorded span
                // covers the *raw source bytes*, which differ from
                // that decoded value whenever the destination used a
                // backslash escape (`Old\(2\).md`) or an HTML entity
                // reference (`&amp;`). `before` must always be taken
                // from the raw bytes at the span, never from the
                // decoded `old_raw`, so `apply_changes_to_content`'s
                // byte-exact check can ever succeed for such a link.
                let raw_before = &content[byte_start..byte_end];
                out.push(LinkChange {
                    file: file_id.to_string(),
                    line: m.line,
                    before: raw_before.to_string(),
                    after: finalize_markdown_after(
                        raw_before, old_raw, new_raw, content, byte_start,
                    ),
                    byte_start,
                    byte_end,
                });
            }
        }
    }
    out
}

/// Parses `content` the way the scan did when it indexed the file, so
/// the spans found here are the ones the cache's edges came from: a
/// code file (by its name, or its `#!` line) through
/// `parse_code_unit`, which reports only links inside comments, and
/// every other file as Markdown/prose. Parsing a `.rs` or `.py` file as
/// Markdown would report a `[[...]]` inside a string literal as a link
/// and rewrite it.
fn parse_for_links(file_id: &str, content: &str) -> tephra_parser::ParsedUnit {
    let rel = split_federated_id(file_id).map_or(file_id, |(_, rel)| rel);
    let file_name = rel.rsplit('/').next().unwrap_or(rel);
    match crate::file_kind::language_for(file_name, content.as_bytes()) {
        Some(lang) => tephra_parser::parse_code_unit(Path::new(file_id), lang, content),
        None => tephra_parser::parse_unit(Path::new(file_id), content),
    }
}

/// Builds the exact raw replacement text for one markdown-link
/// destination occurrence, given:
/// - `raw_before`: the literal source bytes currently at the span
///   (may differ from `old_raw` if the source used a backslash
///   escape or HTML entity reference — R13).
/// - `old_raw`/`new_raw`: the parser-decoded old and new destination
///   text (plus any `#anchor`), as computed by [`rewrite_markdown_raw`]
///   (never itself percent-encoded — that decision is made here).
/// - `content`/`byte_start`: used only to look at the single byte
///   immediately before the span, which tells us whether this
///   destination is written in `<...>` (angle-bracket) form, in
///   which case spaces and parentheses never need escaping, or in
///   bare form, which has real CommonMark syntax constraints.
///
/// Percent-encodes the new destination's path portion (preserving
/// any `#anchor` unencoded, exactly as the destination's style
/// already did) whenever any of the following holds, each a
/// guarantee that writing `new_raw` back literally would either
/// break re-parsing (R12) or require reproducing an escaping
/// convention this function does not attempt to replicate
/// byte-for-byte (R13):
/// - the original destination was itself percent-encoded (style
///   preservation, pre-existing behaviour);
/// - the original destination's raw source bytes differ from its
///   decoded value (a backslash escape or HTML entity was used);
/// - the new destination text is not safe to write literally as-is
///   for this occurrence's bracket style (a bare destination
///   containing whitespace, an unbalanced paren, a literal
///   backslash, or a leading `<`; or an angle-bracket destination
///   containing a literal `<`, `>`, backslash, or newline).
///
/// A percent-encoded destination contains none of CommonMark's
/// syntactically significant characters, so it is always safe to
/// write literally in either form, and `tephra_graph`'s own resolver
/// already treats percent-encoding as a normal, fully round-trippable
/// destination convention (see `tephra_graph::resolver::percent_decode`).
fn finalize_markdown_after(
    raw_before: &str,
    old_raw: &str,
    new_raw: &str,
    content: &str,
    byte_start: usize,
) -> String {
    let (new_path, anchor) = match new_raw.split_once('#') {
        Some((p, a)) => (p, Some(a)),
        None => (new_raw, None),
    };
    let old_path_before_anchor = old_raw.split_once('#').map(|(p, _)| p).unwrap_or(old_raw);
    let originally_percent_encoded = old_path_before_anchor.contains('%');
    let original_used_escaping = raw_before != old_raw;
    let is_angle_bracket = byte_start > 0 && content.as_bytes().get(byte_start - 1) == Some(&b'<');

    let needs_percent_encoding = originally_percent_encoded
        || original_used_escaping
        || !destination_is_safe_literal(new_path, is_angle_bracket);

    let final_path = if needs_percent_encoding {
        percent_encode_path(new_path)
    } else {
        new_path.to_string()
    };
    match anchor {
        Some(a) => format!("{final_path}#{a}"),
        None => final_path,
    }
}

/// Whether `path` (the new destination's path portion, decoded, with
/// no `#anchor`) can be written literally at this span without any
/// escaping, given whether the span is written in `<...>`
/// (angle-bracket) or bare form — see [`finalize_markdown_after`].
fn destination_is_safe_literal(path: &str, is_angle_bracket: bool) -> bool {
    if is_angle_bracket {
        !path.is_empty()
            && !path
                .bytes()
                .any(|b| matches!(b, b'<' | b'>' | b'\\' | b'\n' | b'\r'))
    } else {
        is_safe_bare_destination(path)
    }
}

/// Mirrors `tephra_parser::markdown_pass`'s own bare-destination scan
/// (whitespace or an unmatched `)` ends the destination early; a
/// balanced, unescaped `(...)` is part of it) to decide whether
/// writing `path` literally, followed by the link's own real closing
/// `)`, would re-parse back to exactly `path` with no truncation, no
/// early termination, and no unterminated open paren. A leading `<`
/// is also unsafe: a bare destination that starts with one would be
/// misread as the start of an angle-bracket destination on reparse.
fn is_safe_bare_destination(path: &str) -> bool {
    if path.is_empty() || path.as_bytes()[0] == b'<' {
        return false;
    }
    let mut depth: i32 = 0;
    for &b in path.as_bytes() {
        match b {
            b'\\' => return false,
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => return false,
            _ => {}
        }
    }
    depth == 0
}

/// Rewrites one wikilink's full raw inner text (`WikiLink::raw` /
/// `Edge::to_raw` for a wikilink-kind edge) so its target component
/// refers to `new_path` instead, leaving any `@source:` prefix,
/// `#anchor`, and `|alias` byte-for-byte unchanged. Returns `None` for
/// a same-file anchor-only link (`target` empty) — there is nothing
/// to rewrite, and this should not occur for an edge this function is
/// ever called with (such a link never resolves to a *different*
/// unit), but is handled rather than panicking.
///
/// `bare_ok` ([`FixBare`]) says whether a bare target may stay bare, for
/// a link with an `@source:` prefix and for one without; the two differ
/// only under the Atlas fallback (G20).
fn rewrite_wikilink_raw(to_raw: &str, new_path: &str, bare_ok: FixBare) -> Option<String> {
    let w = tephra_parser::parse_wikilink_inner(to_raw);
    if w.target.is_empty() {
        return None;
    }
    let bare_ok = if w.source_prefix.is_some() {
        bare_ok.prefixed
    } else {
        bare_ok.plain
    };
    let new_target = rewrite_target_component(&w.target, new_path, bare_ok);
    let mut out = String::new();
    if let Some(prefix) = &w.source_prefix {
        out.push('@');
        out.push_str(prefix);
        out.push(':');
    }
    out.push_str(&new_target);
    if let Some(anchor) = &w.anchor {
        out.push('#');
        out.push_str(anchor);
    }
    if let Some(alias) = &w.alias {
        out.push('|');
        out.push_str(alias);
    }
    Some(out)
}

/// Rewrites one wikilink target component (the part before any
/// `#anchor`/`|alias`, after any `@source:` prefix) from naming the
/// old unit to naming `new_path`: a bare (no `/`) target becomes
/// `new_path`'s basename, a full (`/`-containing) target becomes
/// `new_path`'s full stem — either way preserving whether the
/// original included a `.md`/`.markdown` extension (a rename never
/// adds or removes the extension convention the user originally
/// typed).
///
/// `bare_ok` is whether a bare target may stay bare: the basename still
/// resolves to the renamed file (the automatic fix asked the graph), and
/// the Atlas fallback does not ask for a fuller path (G20, see
/// [`compute_fix_changes_qualifying`]). When it is `false`, a bare
/// target also becomes the full stem, the shortest form that resolves.
/// A target typed in Unicode NFC stays NFC even when the new path is
/// spelled NFD on disk (macOS), so the fix does not change how the
/// name looks in the user's bytes; the resolver compares both forms
/// as equal either way.
fn rewrite_target_component(target: &str, new_path: &str, bare_ok: bool) -> String {
    let (_, ext) = strip_md_ext(target);
    let (new_stem, _) = strip_md_ext(new_path);
    let core = if target.contains('/') || !bare_ok {
        new_stem.to_string()
    } else {
        basename(new_stem).to_string()
    };
    let core = if unicode_normalization::is_nfc(target) {
        unicode_normalization::UnicodeNormalization::nfc(core.chars()).collect()
    } else {
        core
    };
    match ext {
        Some(e) => format!("{core}{e}"),
        None => core,
    }
}

/// Rewrites a markdown link's raw destination text (`Edge::to_raw`
/// for a markdown-kind edge, exactly `MarkdownLink::target`) so it
/// names `new_path` instead, relative to `from_id`'s own directory
/// (markdown link destinations, unlike wikilinks, are always
/// directory-relative — or root-relative if they start with `/`).
/// Preserves a trailing `#anchor`, decoding any pre-existing
/// percent-encoding on the old destination's path portion only to
/// compute `new_stem`'s relative placement correctly (a percent-
/// encoded old path must still be treated as naming the same file
/// its decoded form would).
///
/// Returns the new destination's *decoded* text — never itself
/// percent-encoded, nor escaped for any particular bracket style.
/// Those decisions depend on where in the source text this
/// destination actually sits (bare vs `<...>`, and whether its raw
/// bytes already differed from this decoded form), which only
/// [`locate_and_diff`]'s `finalize_markdown_after` can see (R12/R13
/// review findings: a single global percent-encoding choice here,
/// keyed only on the *old* destination's own style, could neither
/// react to a new destination that needs escaping the old one never
/// did, nor to an old destination that used backslash/entity
/// escaping rather than percent-encoding).
fn rewrite_markdown_raw(from_id: &str, to_raw: &str, new_path: &str) -> Option<String> {
    let (before_anchor, anchor) = match to_raw.split_once('#') {
        Some((a, b)) => (a, Some(b)),
        None => (to_raw, None),
    };
    if before_anchor.is_empty() || before_anchor.contains("://") {
        // Anchor-only or absolute-URL destinations never resolve to a
        // file unit, so an edge targeting our renamed unit should
        // never carry one here — handled defensively rather than
        // assumed.
        return None;
    }
    let decoded = percent_decode(before_anchor);
    let is_absolute = decoded.starts_with('/');
    let decoded_trimmed = decoded.trim_start_matches('/');
    let (_, ext) = strip_md_ext(decoded_trimmed);
    let (new_stem, _) = strip_md_ext(new_path);

    let (_, from_rel) = split_federated_id(from_id)?;
    let from_dir = from_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    let core = if is_absolute {
        format!("/{new_stem}")
    } else {
        relative_path(from_dir, new_stem)
    };
    let mut out = match ext {
        Some(e) => format!("{core}{e}"),
        None => core,
    };
    if let Some(a) = anchor {
        out.push('#');
        out.push_str(a);
    }
    Some(out)
}

/// Splits a federated id (`"@<source>:<rel_path>"`) into its two
/// parts, mirroring `tephra_graph`'s own (private) helper of the same
/// shape — duplicated here rather than exposed from that crate, since
/// it is trivial string splitting, not graph/resolution logic.
fn split_federated_id(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix('@')?.split_once(':')
}

/// Strips a case-insensitive `.md`/`.markdown` suffix off `p`,
/// returning `(stem, Some(extension_as_written))` if present or
/// `(p, None)` if not. The extension is returned with its original
/// case so a replacement can restore exactly what the user typed.
/// Never panics, whatever the string: the extensions are ASCII, so a
/// name that ends in one has a char boundary exactly `ext.len()` bytes
/// from its end. When that offset falls inside a multi-byte character
/// (`Todo…ab`, `Résumé—v2.docx`) the name cannot end in the extension
/// and comes back as `(p, None)`.
fn strip_md_ext(p: &str) -> (&str, Option<&str>) {
    for ext in [".md", ".markdown"] {
        if p.len() > ext.len() {
            let idx = p.len() - ext.len();
            if !p.is_char_boundary(idx) {
                continue;
            }
            if p[idx..].eq_ignore_ascii_case(ext) {
                return (&p[..idx], Some(&p[idx..]));
            }
        }
    }
    (p, None)
}

/// [`strip_md_ext`] for the fix engine: the path without a `.md` or
/// `.markdown` extension, and the extension.
pub(crate) fn strip_md_ext_pub(p: &str) -> (&str, Option<&str>) {
    strip_md_ext(p)
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// The minimal relative path from directory `from_dir` to file
/// `target_path` (both source-root-relative, `/`-separated, no
/// leading/trailing slash expected on `from_dir`): shared leading
/// directory components are dropped, any remaining `from_dir`
/// components become a `../` each, and the rest of `target_path`'s
/// directories plus its filename are appended. Two files in the same
/// directory relativize to just the filename (no `./` prefix).
fn relative_path(from_dir: &str, target_path: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let mut target: Vec<&str> = target_path.split('/').filter(|s| !s.is_empty()).collect();
    if target.is_empty() {
        return String::new();
    }
    let filename = target.pop().expect("checked non-empty above");
    let mut common = 0;
    while common < from.len() && common < target.len() && from[common] == target[common] {
        common += 1;
    }
    let ups = from.len() - common;
    let mut parts: Vec<String> = Vec::with_capacity(ups + target.len() - common + 1);
    for _ in 0..ups {
        parts.push("..".to_string());
    }
    for seg in &target[common..] {
        parts.push((*seg).to_string());
    }
    parts.push(filename.to_string());
    parts.join("/")
}

/// Minimal percent-decoding: only well-formed `%XX` triples are
/// decoded, anything else (including a lone `%` or malformed hex) is
/// left verbatim. Enough to round-trip the convention
/// [`percent_encode_path`] itself produces, without adding a crate
/// dependency for a single-direction, best-effort decode.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encodes every byte outside a small unreserved set
/// (alphanumerics plus `/ - _ . ~`), so a space or non-ASCII
/// character round-trips the same way a markdown-link destination
/// that already used percent-encoding did.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Applies the subset of `changes` belonging to one file to that
/// file's current `content`, returning the new content. Rewrites
/// exactly the byte range [`LinkChange::byte_start`,
/// `LinkChange::byte_end`) recorded for each change — never a
/// textual search-and-replace — after first verifying the bytes
/// currently at that range still equal `before` exactly (R1/R7 review
/// findings): a span whose bytes no longer match `before` (the file
/// changed underneath the preview in a way the mtime-based staleness
/// gate didn't catch, or the recorded span is simply out of range for
/// the content passed in) is reported as an error rather than
/// silently rewriting the wrong text, or an unrelated occurrence of
/// the same bytes elsewhere on the same line. Changes are applied
/// from the highest `byte_start` down to the lowest, so rewriting one
/// change's span never shifts the byte offsets of any still-pending
/// earlier one.
pub fn apply_changes_to_content(content: &str, changes: &[&LinkChange]) -> Result<String, String> {
    let mut ordered: Vec<&LinkChange> = changes.to_vec();
    ordered.sort_by_key(|c| std::cmp::Reverse(c.byte_start));

    let mut buf = content.to_string();
    for c in ordered {
        if c.byte_start > c.byte_end
            || c.byte_end > buf.len()
            || !buf.is_char_boundary(c.byte_start)
            || !buf.is_char_boundary(c.byte_end)
        {
            return Err(format!(
                "{}:{}: the recorded link span is out of range for the file's current content",
                c.file, c.line
            ));
        }
        if buf[c.byte_start..c.byte_end] != c.before {
            return Err(format!(
                "{}:{}: content at the recorded link span no longer matches the expected text {:?}",
                c.file, c.line, c.before
            ));
        }
        buf.replace_range(c.byte_start..c.byte_end, &c.after);
    }
    Ok(buf)
}

// ---------------------------------------------------------------
// Staleness gate
// ---------------------------------------------------------------

/// Compares `expected` (each affected file's federated id mapped to
/// the mtime recorded at preview time) against a fresh mtime read for
/// each one, via the injected `current_mtime` lookup — pure aside
/// from that one injection point, so a test can assert "all fresh",
/// "one stale", "missing file", and "permission-denied file" without
/// touching a real filesystem (see this module's tests).
///
/// Returns `Err` naming the first problem found (a changed mtime, a
/// file that no longer exists, or one that couldn't be stat'd at
/// all) and stops there — this function's only job is "is it safe to
/// proceed at all", not enumerating every problem, since
/// [`RefactorEngine::apply`] aborts the *entire* operation rather
/// than writing to whichever files are still fresh (E3: "aborts ...
/// including files that were not stale").
pub fn verify_staleness<F>(
    expected: &HashMap<String, SystemTime>,
    mut current_mtime: F,
) -> Result<(), String>
where
    F: FnMut(&str) -> io::Result<SystemTime>,
{
    let mut files: Vec<&String> = expected.keys().collect();
    files.sort();
    for file in files {
        let expected_mtime = expected[file];
        match current_mtime(file) {
            Ok(actual) if actual == expected_mtime => {}
            Ok(_) => return Err(format!("file modified since preview: {file}")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(format!("file missing since preview: {file}"));
            }
            Err(e) => {
                return Err(format!("file unreadable since preview ({e}): {file}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------
// Atomic single-file rewrite
// ---------------------------------------------------------------

/// The three filesystem primitives an atomic rewrite needs, behind a
/// trait so a test can intercept them (the spec's "fsync + rename
/// mocked/intercepted in test harness" requirement) without actually
/// racing a reader thread against real disk I/O timing.
pub trait AtomicFs: Send + Sync {
    /// Creates `path` (must not already exist as the real target —
    /// callers always pass a fresh temp path) and writes `content` to
    /// it in full.
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()>;
    /// Sets `path`'s permission bits to `perms` (R2 review finding:
    /// called with the real target's own existing permissions before
    /// every rename, so `apply_refactor` never itself widens,
    /// narrows, or drops the executable bit on a file it rewrites —
    /// `write_new`'s freshly-created temp file otherwise gets the
    /// process's default mode).
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()>;
    /// Flushes `path` to stable storage.
    fn sync(&self, path: &Path) -> io::Result<()>;
    /// Atomically replaces `to` with `from` (same directory, so this
    /// is a single `rename(2)`/`MoveFileEx` syscall, not a copy).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// What identifies the exact bytes of `path` between two looks at
    /// it: its size and mtime (the entry itself, a symlink not followed).
    /// A writer that read a file takes this before the read and again
    /// just before its write (G19's duplicate ID rename), and writes
    /// nothing if the two differ. Behind the trait so a test can make the
    /// file change between the two looks.
    fn fingerprint(&self, path: &Path) -> io::Result<(u64, Option<std::time::SystemTime>)> {
        let meta = fs::symlink_metadata(path)?;
        Ok((meta.len(), meta.modified().ok()))
    }
}

/// The real implementation: `std::fs` all the way down.
pub struct RealFs;

impl AtomicFs for RealFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        let mut f = fs::File::create(path)?;
        f.write_all(content)
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        fs::set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        fs::File::open(path)?.sync_all()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
}

/// Rewrites `target`'s content to `new_content` via the real
/// filesystem (temp file in `target`'s own directory, with `target`'s
/// own permissions copied onto it, `fsync`, then `rename`) — see
/// [`rewrite_links_atomic_with`] for the test-intercepted version and
/// this module's doc comment for why this sequencing is what keeps a
/// reader (or a process restart after a kill) from ever observing a
/// half-written file.
pub fn rewrite_links_atomic(target: &Path, new_content: &[u8]) -> io::Result<()> {
    rewrite_links_atomic_with(&RealFs, target, new_content)
}

/// Same as [`rewrite_links_atomic`], but through an injected
/// [`AtomicFs`] so a test can observe/intercept the write-fsync-rename
/// sequence. The temp file is named `.<original-name>.tephra-tmp-<n>`
/// in the same directory as `target` (so the final `rename` is
/// guaranteed same-filesystem, hence atomic), gets `target`'s own
/// current permissions copied onto it before the rename (R2 review
/// finding — a bare temp-file-then-rename otherwise silently widens
/// or narrows the real file's permissions, or drops its executable
/// bit, to whatever the process's default mode happens to be), and is
/// removed again if anything after its creation fails, rather than
/// left behind (R3: this module's temp files must never survive a
/// failure here, only a process being killed outright between
/// `write_new` and `rename` — see `crate::scan`'s sweep for that
/// case).
///
/// Refuses outright (an `io::ErrorKind::Unsupported` error, `target`
/// left completely untouched) if `target` is itself a symlink (R2
/// review finding): `rename` replaces whatever directory entry sits
/// at `target`, so renaming a temp *file* onto a symlinked (or
/// hard-linked) note would silently turn it into a plain file —
/// losing the link itself — rather than updating whatever it points
/// to. Resolving through the symlink to its real target was
/// considered and rejected: that target can resolve outside the
/// source root entirely, and a link-rewrite RPC silently following it
/// there is a bigger surprise than simply refusing.
pub fn rewrite_links_atomic_with(
    fs_ops: &dyn AtomicFs,
    target: &Path,
    new_content: &[u8],
) -> io::Result<()> {
    // `symlink_metadata` (unlike `metadata`) does not follow the
    // link, so this sees `target` itself rather than whatever it
    // points to.
    let target_meta = fs::symlink_metadata(target)?;
    if target_meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "refusing to rewrite '{}': it is a symlink, and rewriting it atomically would replace the link itself with a plain file",
                target.display()
            ),
        ));
    }

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp_name = format!(".{file_name}.tephra-tmp-{}", rand::random::<u32>());
    let tmp_path = dir.join(tmp_name);

    let write_and_prepare = (|| -> io::Result<()> {
        fs_ops.write_new(&tmp_path, new_content)?;
        fs_ops.set_permissions(&tmp_path, target_meta.permissions())?;
        fs_ops.sync(&tmp_path)
    })();
    if let Err(e) = write_and_prepare {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    let result = fs_ops.rename(&tmp_path, target);
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

// ---------------------------------------------------------------
// Watcher pause/resume control
// ---------------------------------------------------------------

/// GP8: pauses/resumes every currently-running source's watcher
/// around a refactor `apply`'s own writes, so the watcher's debounced
/// reindex never races (or is triggered redundantly by) the very
/// writes `apply_refactor` is in the middle of making. Behind a trait
/// so a unit test can substitute [`NoopWatcherPause`] instead of a
/// real [`SourceRegistry`].
///
/// `resume_all`'s `written_paths` are the absolute paths `apply` just
/// finished rewriting (R4 review finding): a notify event for one of
/// those paths reaching the watcher's debounced pipeline after resume
/// would just be a redundant, racy reindex of content `apply` already
/// accounted for synchronously itself (see `RefactorEngine::write_all`'s
/// own `sources.rescan` call) — not a real external change to react
/// to. An implementation is expected to wait out one full debounce
/// window before actually resuming (so any such event has already
/// landed in its paused, per-source queue) and then discard exactly
/// `written_paths` from that queue before flushing whatever, if
/// anything, remains — see [`crate::watcher::WatcherPause::resume_except`].
/// Any *other* path that queued up during that window (a concurrent
/// edit by something other than this `apply`) is still flushed
/// normally.
pub trait WatcherPauseControl: Send + Sync {
    fn pause_all(&self);
    fn resume_all(&self, written_paths: &HashSet<PathBuf>);
}

/// Reference-counted on the registry (see
/// [`SourceRegistry::pause_for_write`]): every `pause_all` must be
/// paired with one `resume_all`, and the watchers resume only when the
/// last overlapping writer calls it.
impl WatcherPauseControl for SourceRegistry {
    fn pause_all(&self) {
        SourceRegistry::pause_for_write(self);
    }
    fn resume_all(&self, written_paths: &HashSet<PathBuf>) {
        SourceRegistry::resume_after_write(self, written_paths);
    }
}

/// A [`WatcherPauseControl`] that does nothing — for tests that
/// exercise [`RefactorEngine`] without a real [`SourceRegistry`]/
/// watcher.
pub struct NoopWatcherPause;

impl WatcherPauseControl for NoopWatcherPause {
    fn pause_all(&self) {}
    fn resume_all(&self, _written_paths: &HashSet<PathBuf>) {}
}

// ---------------------------------------------------------------
// Stale temp-file sweep (R3)
// ---------------------------------------------------------------

/// Recognizes this module's own atomic-rewrite temp-file naming
/// convention (`.{original-file-name}.tephra-tmp-{digits}`, see
/// [`rewrite_links_atomic_with`]). Exposed so `crate::scan`'s source
/// walk — which already visits every file in a source on every
/// `register`/`rescan` — can sweep away any file matching it that
/// survived a process kill between `write_new` and `rename` (R3
/// review finding: nothing previously cleaned these up, so a kill at
/// exactly the wrong moment left a stray file in the user's source
/// tree forever) rather than leaving it on disk indefinitely or
/// indexing it as if it were real note content.
pub fn is_stale_temp_file_name(file_name: &str) -> bool {
    let Some(rest) = file_name.strip_prefix('.') else {
        return false;
    };
    match rest.rsplit_once(".tephra-tmp-") {
        Some((_, suffix)) => !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

// ---------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------

/// One already-written file's failure during an otherwise-successful
/// `apply` (the permission-denied-mid-apply edge case): already
/// succeeded files are not rolled back, and the response names
/// exactly which files failed and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedFile {
    pub file: String,
    pub error: String,
}

/// `link.apply_refactor`'s result: either every affected file was
/// rewritten (`Applied`, `files_changed` may be `0` for a zero-incoming
/// -links rename), the whole operation was aborted before any write
/// because of a staleness conflict (`Aborted`), or some files were
/// rewritten before a later one failed (`Partial`). `Partial` names
/// exactly which files are in each group (R5 review finding: the
/// wire result previously carried only a *count* of succeeded files
/// alongside the failed list, so a caller could never actually tell
/// which specific files still needed attention).
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    Applied {
        files_changed: usize,
    },
    Aborted {
        reason: String,
    },
    Partial {
        files_changed: usize,
        succeeded: Vec<String>,
        failed: Vec<FailedFile>,
    },
}

struct StoredPreview {
    changes: Vec<LinkChange>,
    mtimes: HashMap<String, SystemTime>,
}

/// Why a referring docx, pptx or PDF is left out of a refactor: Tephra
/// never writes to a document (F4), so a link inside one stays as it is.
pub const DOCUMENT_READ_ONLY: &str =
    "read-only document: links inside a docx, pptx or pdf are never rewritten";

/// Why a referring file of a remote source is left out of a refactor: a
/// remote file is never written (F2), so a link inside one stays as it is.
pub const REMOTE_READ_ONLY: &str =
    "remote file: it is never edited, so a link inside it is not rewritten";

/// Whether a source row's `path` is a remote source's marker, not a folder.
fn is_remote_root(root: &str) -> bool {
    root.starts_with(tephra_cache::REMOTE_ROOT_PREFIX)
}

/// One referring file [`RefactorEngine::preview`] could not read or
/// stat, and therefore could not scan for incoming links at all.
///
/// R10 review finding: a prior version of `preview` silently `continue`d
/// past a file it couldn't read, stat, or get an mtime for — the
/// `changes` list (and the `apply` result later derived from it)
/// never mentioned the file at all, so its stale links stayed stale
/// with no indication anything had been left out, contradicting G7's
/// "computes the full set of files". Every file skipped for any of
/// those reasons is now named here instead, alongside why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedFile {
    pub file: String,
    pub error: String,
}

/// [`RefactorEngine::preview`]'s full result: the changes it would
/// make, plus any referring file it had to leave out of that
/// computation because it couldn't be read or stat'd (R10) — a
/// caller always sees both, never just a silently-narrowed `changes`
/// list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewResult {
    pub changes: Vec<LinkChange>,
    pub skipped: Vec<SkippedFile>,
}

/// Orchestrates `link.preview_refactor`/`link.apply_refactor`: wires
/// the pure functions above to the real [`GraphBackend`]/
/// [`SourceBackend`] seams, real file reads/writes, and watcher
/// pause/resume.
///
/// Keeps the most recent preview per distinct `(source, old_path,
/// new_path)` triple in memory (not persisted — a daemon restart
/// forgets every pending preview, which is fine: `apply_refactor`
/// requires a fresh one anyway). An `apply` call first removes the
/// matching stored preview (so a repeated `apply` — or one after a
/// staleness abort — always requires a fresh `preview` first, per
/// this PR's spec) and holds `apply_lock` for its entire body, so two
/// concurrent `apply_refactor` calls are fully serialized rather than
/// interleaving their writes.
pub struct RefactorEngine {
    graph: std::sync::Arc<dyn GraphBackend>,
    sources: std::sync::Arc<dyn SourceBackend>,
    watcher: std::sync::Arc<dyn WatcherPauseControl>,
    previews: Mutex<HashMap<(String, String, String), StoredPreview>>,
    apply_lock: std::sync::Arc<Mutex<()>>,
}

impl RefactorEngine {
    /// The lock `apply` holds for its whole body, shared so the
    /// automatic fix's per-file writer ([`fix_write::FixWriter`]) takes
    /// the very same one: a fix and a `link.apply_refactor` never write
    /// at the same time. Lock order everywhere is this gate first, then
    /// the file's [`crate::file_lock`].
    pub fn writer_gate(&self) -> std::sync::Arc<Mutex<()>> {
        std::sync::Arc::clone(&self.apply_lock)
    }

    pub fn new(
        graph: std::sync::Arc<dyn GraphBackend>,
        sources: std::sync::Arc<dyn SourceBackend>,
        watcher: std::sync::Arc<dyn WatcherPauseControl>,
    ) -> Self {
        RefactorEngine {
            graph,
            sources,
            watcher,
            previews: Mutex::new(HashMap::new()),
            apply_lock: std::sync::Arc::new(Mutex::new(())),
        }
    }

    fn source_roots(&self) -> Result<HashMap<String, String>, String> {
        Ok(self
            .sources
            .list()?
            .into_iter()
            // A remote source's wire path is a display string, not a
            // folder: give it the marker root so no path is made from it.
            .map(|r| {
                let root = if r.remote.is_some() {
                    format!("{}{}", tephra_cache::REMOTE_ROOT_PREFIX, r.name)
                } else {
                    r.path
                };
                (r.name, root)
            })
            .collect())
    }

    fn require_source(&self, roots: &HashMap<String, String>, source: &str) -> Result<(), String> {
        match roots.get(source) {
            None => Err(format!("no source named '{source}' is registered")),
            // F2: a remote file is never written, moved or renamed.
            Some(root) if is_remote_root(root) => Err(format!(
                "source '{source}' is a remote source: its files are never written or renamed"
            )),
            Some(_) => Ok(()),
        }
    }

    /// The file on disk behind a unit id. `None` for an unregistered source
    /// and for a remote one: a remote source has no folder (its root is a
    /// marker, not a path), and a remote file is never written (F2).
    fn resolve_abs_path(&self, id: &str, roots: &HashMap<String, String>) -> Option<PathBuf> {
        let (source, rel_path) = split_federated_id(id)?;
        let root = roots.get(source)?;
        if is_remote_root(root) {
            return None;
        }
        Some(Path::new(root).join(rel_path))
    }

    /// `link.preview_refactor`: computes and records the diff for
    /// renaming `old_path` to `new_path` within `source`. Never
    /// writes to any file — only reads (file content, to compute the
    /// diff; mtimes, to record the staleness snapshot a later `apply`
    /// is checked against).
    pub fn preview(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<PreviewResult, String> {
        let roots = self.source_roots()?;
        self.require_source(&roots, source)?;

        let old_id = format!("@{source}:{old_path}");
        let edges = self.graph.get_backlinks_all(&old_id);

        let mut mtimes = HashMap::new();
        let mut skipped = Vec::new();
        let changes = if old_path == new_path {
            Vec::new()
        } else {
            let mut file_contents = HashMap::new();
            for edge in &edges {
                if file_contents.contains_key(&edge.from) {
                    continue;
                }
                let Some(abs) = self.resolve_abs_path(&edge.from, &roots) else {
                    // A link inside a remote file is not rewritten (F2):
                    // the file is reported as skipped, once.
                    if split_federated_id(&edge.from)
                        .and_then(|(s, _)| roots.get(s))
                        .is_some_and(|r| is_remote_root(r))
                        && !skipped.iter().any(|s: &SkippedFile| s.file == edge.from)
                    {
                        skipped.push(SkippedFile {
                            file: edge.from.clone(),
                            error: REMOTE_READ_ONLY.to_string(),
                        });
                    }
                    continue;
                };
                // G5/F4: a docx, pptx or PDF is read-only. A link inside
                // one is never rewritten, so the file is reported as
                // skipped and never read as text.
                // A document holds many links, so one entry per file.
                if tephra_parser::DocumentFormat::from_path(&abs).is_some() {
                    if !skipped.iter().any(|s: &SkippedFile| s.file == edge.from) {
                        skipped.push(SkippedFile {
                            file: edge.from.clone(),
                            error: DOCUMENT_READ_ONLY.to_string(),
                        });
                    }
                    continue;
                }
                let content = match fs::read_to_string(&abs) {
                    Ok(c) => c,
                    Err(e) => {
                        skipped.push(SkippedFile {
                            file: edge.from.clone(),
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                // R7 review finding: a file whose mtime can't be
                // captured right now must not be included in this
                // preview at all. Recording it in `file_contents`
                // (hence proposing a change for it) without an mtime
                // snapshot would silently exempt it from
                // `verify_staleness`'s later re-check at `apply` time
                // — the exact TOCTOU gap that gate exists to close.
                // R10 review finding: treating that (and a plain
                // unreadable file, above) exactly like an ordinary
                // skip left the preview silently incomplete with no
                // indication anything was left out — report it in
                // `skipped` instead of just dropping it.
                let meta = match fs::metadata(&abs) {
                    Ok(m) => m,
                    Err(e) => {
                        skipped.push(SkippedFile {
                            file: edge.from.clone(),
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                let mtime = match meta.modified() {
                    Ok(t) => t,
                    Err(e) => {
                        skipped.push(SkippedFile {
                            file: edge.from.clone(),
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                mtimes.insert(edge.from.clone(), mtime);
                file_contents.insert(edge.from.clone(), content);
            }
            compute_changes(&edges, &file_contents, old_path, new_path)
        };

        let key = (
            source.to_string(),
            old_path.to_string(),
            new_path.to_string(),
        );
        self.previews.lock().unwrap().insert(
            key,
            StoredPreview {
                changes: changes.clone(),
                mtimes,
            },
        );
        Ok(PreviewResult { changes, skipped })
    }

    /// `link.apply_refactor`: re-validates and rewrites the diff
    /// computed by the most recent matching [`RefactorEngine::preview`]
    /// call. Rejects outright (an `Err`, not an [`ApplyOutcome`]) if
    /// there is no such prior preview. Pauses every running source's
    /// watcher for the duration of the actual writes and always
    /// resumes it afterwards, even on a staleness abort or a
    /// mid-write failure — see [`WatcherPauseControl`]'s doc comment
    /// for why resume additionally waits out a debounce window and is
    /// then told exactly which paths were written (R4).
    pub fn apply(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<ApplyOutcome, String> {
        let _apply_guard = self.apply_lock.lock().unwrap_or_else(|p| p.into_inner());

        let key = (
            source.to_string(),
            old_path.to_string(),
            new_path.to_string(),
        );
        let stored = self.previews.lock().unwrap().remove(&key);
        let Some(stored) = stored else {
            return Err(format!(
                "no prior preview_refactor call for source='{source}' old_path='{old_path}' new_path='{new_path}'; call preview_refactor again before apply_refactor"
            ));
        };

        if stored.changes.is_empty() {
            return Ok(ApplyOutcome::Applied { files_changed: 0 });
        }

        let roots = self.source_roots()?;
        let staleness = verify_staleness(&stored.mtimes, |file_id| {
            let abs = self.resolve_abs_path(file_id, &roots).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "source no longer registered")
            })?;
            fs::metadata(&abs)?.modified()
        });
        if let Err(reason) = staleness {
            return Ok(ApplyOutcome::Aborted { reason });
        }

        self.watcher.pause_all();
        let outcome = self.write_all(source, &roots, &stored.mtimes, &stored.changes);
        let (result, written_paths) = match outcome {
            Ok((outcome, written_paths)) => (Ok(outcome), written_paths),
            Err(e) => (Err(e), HashSet::new()),
        };
        // R4 review finding: wait out one full debounce window — plus
        // a margin for FSEvents' own extra latency on macOS — before
        // actually resuming, so a notify event the writes above
        // themselves produced has reliably already landed in each
        // paused source's queue. Without this, `resume_all` ran
        // before that event ever arrived, so it was never paused at
        // all: it fired straight through live, as though it were a
        // real external change, immediately after resume.
        std::thread::sleep(watcher::DEBOUNCE_WINDOW + std::time::Duration::from_millis(150));
        self.watcher.resume_all(&written_paths);
        result
    }

    fn write_all(
        &self,
        primary_source: &str,
        roots: &HashMap<String, String>,
        expected_mtimes: &HashMap<String, SystemTime>,
        changes: &[LinkChange],
    ) -> Result<(ApplyOutcome, HashSet<PathBuf>), String> {
        let mut by_file: HashMap<String, Vec<&LinkChange>> = HashMap::new();
        for c in changes {
            by_file.entry(c.file.clone()).or_default().push(c);
        }

        let mut files_changed = 0usize;
        let mut succeeded = Vec::new();
        let mut failed = Vec::new();
        let mut written_paths: HashSet<PathBuf> = HashSet::new();
        let mut touched_sources: HashSet<String> = HashSet::new();
        touched_sources.insert(primary_source.to_string());

        let mut files: Vec<&String> = by_file.keys().collect();
        files.sort();
        for file_id in files {
            let file_changes = &by_file[file_id];
            let Some(abs) = self.resolve_abs_path(file_id, roots) else {
                failed.push(FailedFile {
                    file: file_id.clone(),
                    error: "source no longer registered".to_string(),
                });
                continue;
            };

            // Held from this file's mtime re-check to its rename: a
            // `block.ensure_id` on the same file (see `crate::block_id`)
            // takes the same lock, so neither starts from bytes the
            // other is about to replace. If it wrote first, the mtime
            // check below sees it and this file is reported as failed
            // rather than overwritten.
            let _file_guard = crate::file_lock::lock(&abs);

            // R7 review finding: `verify_staleness` only checked every
            // affected file's mtime once, up front in `apply`. On a
            // large fan-in that loop below can run long enough for a
            // later file in the list to be edited from outside in the
            // meantime without being caught. Re-check this one file's
            // mtime again here, immediately before reading it, against
            // the very same snapshot.
            if let Some(expected) = expected_mtimes.get(file_id) {
                match fs::metadata(&abs).and_then(|m| m.modified()) {
                    Ok(actual) if actual == *expected => {}
                    Ok(_) => {
                        failed.push(FailedFile {
                            file: file_id.clone(),
                            error: "file modified since preview".to_string(),
                        });
                        continue;
                    }
                    Err(e) => {
                        failed.push(FailedFile {
                            file: file_id.clone(),
                            error: format!("file unreadable since preview ({e})"),
                        });
                        continue;
                    }
                }
            }

            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(e) => {
                    failed.push(FailedFile {
                        file: file_id.clone(),
                        error: e.to_string(),
                    });
                    continue;
                }
            };
            let new_content = match apply_changes_to_content(&content, file_changes) {
                Ok(c) => c,
                Err(e) => {
                    failed.push(FailedFile {
                        file: file_id.clone(),
                        error: e,
                    });
                    continue;
                }
            };
            match rewrite_links_atomic(&abs, new_content.as_bytes()) {
                Ok(()) => {
                    files_changed += 1;
                    succeeded.push(file_id.clone());
                    written_paths.insert(abs.clone());
                    if let Some((src, _)) = split_federated_id(file_id) {
                        touched_sources.insert(src.to_string());
                    }
                }
                Err(e) => failed.push(FailedFile {
                    file: file_id.clone(),
                    error: e.to_string(),
                }),
            }
        }

        // Synchronously refresh every touched source's cache (the
        // files above were written directly, bypassing the normal
        // `source.rescan` RPC path) so the graph rebuild the RPC
        // dispatch layer performs after a successful
        // `link.apply_refactor` (see `rpc::server::MUTATING_SOURCE_METHODS`)
        // sees the new link text immediately, on this same running
        // daemon, rather than waiting for the resumed watcher's own
        // async debounce.
        for src in &touched_sources {
            let _ = self.sources.rescan(src);
        }

        let outcome = if failed.is_empty() {
            ApplyOutcome::Applied { files_changed }
        } else {
            ApplyOutcome::Partial {
                files_changed,
                succeeded,
                failed,
            }
        };
        Ok((outcome, written_paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tephra_graph::{EdgeTarget, ResolveError, ResolveResult};

    use crate::rpc::backend::{ScanStatsWire, SourceRowWire};

    fn edge(from: &str, to: &str, to_raw: &str, kind: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: EdgeTarget::Resolved(to.to_string()),
            to_raw: to_raw.to_string(),
            kind: kind.to_string(),
            offline: false,
            line: None,
        }
    }

    // --- compute_move_changes (G6) ----------------------------------------

    /// One edge per distinct raw wikilink of `text` as the graph would hold
    /// it: `from` is the file, every link dangling (what a move leaves).
    fn dangling_edges(from: &str, raws: &[&str]) -> Vec<Edge> {
        raws.iter()
            .map(|raw| Edge {
                from: from.to_string(),
                to: EdgeTarget::Dangling,
                to_raw: (*raw).to_string(),
                kind: tephra_parser::LINK_KIND_WIKILINK.to_string(),
                offline: false,
                line: None,
            })
            .collect()
    }

    /// `text` after `compute_move_changes` and the real span rewrite.
    fn moved_text(from: &str, text: &str, raws: &[&str], rewrite: &MoveRewrite) -> String {
        let contents = HashMap::from([(from.to_string(), text.to_string())]);
        let changes = compute_move_changes(&dangling_edges(from, raws), &contents, rewrite);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        apply_changes_to_content(text, &refs).expect("the spans apply")
    }

    /// The link kinds the rewrites have to survive, in one file.
    const LINK_KINDS: &str = "---\nrelated: \"[[Rename Notes]]\"\n---\n\
        plain [[Rename Notes]], alias [[Rename Notes|the notes]], embed ![[Rename Notes]],\n\
        block [[Rename Notes#^k3f9a2]], heading [[Rename Notes#Goals|goals]],\n\
        with extension [[Rename Notes.md]], prefixed [[@notes:Ideas/Rename Notes]],\n\
        other [[Unrelated]], markdown [text](Rename%20Notes.md), code `[[Rename Notes]]`\n";

    #[test]
    fn rewrite_one_names_the_new_source_with_the_full_path_and_keeps_anchor_alias_and_embed() {
        let rewrite = MoveRewrite::OldSource {
            new_source: "tephra".into(),
            new_path: "docs/rename-notes.md".into(),
        };
        let raws = [
            "Rename Notes",
            "Rename Notes|the notes",
            "Rename Notes#^k3f9a2",
            "Rename Notes#Goals|goals",
            "Rename Notes.md",
            "@notes:Ideas/Rename Notes",
        ];
        let out = moved_text("@notes:Index.md", LINK_KINDS, &raws, &rewrite);
        assert_eq!(
            out,
            "---\nrelated: \"[[@tephra:docs/rename-notes]]\"\n---\n\
        plain [[@tephra:docs/rename-notes]], alias [[@tephra:docs/rename-notes|the notes]], embed ![[@tephra:docs/rename-notes]],\n\
        block [[@tephra:docs/rename-notes#^k3f9a2]], heading [[@tephra:docs/rename-notes#Goals|goals]],\n\
        with extension [[@tephra:docs/rename-notes]], prefixed [[@tephra:docs/rename-notes]],\n\
        other [[Unrelated]], markdown [text](Rename%20Notes.md), code `[[Rename Notes]]`\n"
        );
    }

    #[test]
    fn rewrite_two_drops_the_prefix_and_writes_the_shortest_unique_path() {
        let raws = [
            "@notes:Ideas/Rename Notes",
            "@notes:Ideas/Rename Notes|notes",
            "@notes:Ideas/Rename Notes#^k3f9a2",
        ];
        let text = "a [[@notes:Ideas/Rename Notes]] b [[@notes:Ideas/Rename Notes|notes]] c ![[@notes:Ideas/Rename Notes#^k3f9a2]]\n";
        let bare = MoveRewrite::NewSource {
            new_path: "docs/rename-notes.md".into(),
            bare_resolves: true,
        };
        assert_eq!(
            moved_text("@tephra:Readme.md", text, &raws, &bare),
            "a [[rename-notes]] b [[rename-notes|notes]] c ![[rename-notes#^k3f9a2]]\n"
        );
        // Another file of that name in the source: the full stem is the
        // shortest that resolves.
        let full = MoveRewrite::NewSource {
            new_path: "docs/rename-notes.md".into(),
            bare_resolves: false,
        };
        assert_eq!(
            moved_text("@tephra:Readme.md", text, &raws, &full),
            "a [[docs/rename-notes]] b [[docs/rename-notes|notes]] c ![[docs/rename-notes#^k3f9a2]]\n"
        );
    }

    #[test]
    fn rewrite_three_prefixes_links_that_resolved_in_the_old_source_and_unprefixes_the_self_link() {
        let links = HashMap::from([
            ("Other Note".to_string(), OwnLink::AddPrefix("notes".into())),
            (
                "Other Note|alias".to_string(),
                OwnLink::AddPrefix("notes".into()),
            ),
            (
                "Other Note#^b1".to_string(),
                OwnLink::AddPrefix("notes".into()),
            ),
            ("Rename Notes".to_string(), OwnLink::SelfLink),
            ("Rename Notes#^k3f9a2".to_string(), OwnLink::SelfLink),
            ("@tephra:docs/sibling".to_string(), OwnLink::DropPrefix),
        ]);
        let rewrite = MoveRewrite::Own {
            new_path: "docs/rename-notes.md".into(),
            bare_resolves: true,
            links,
        };
        let raws = [
            "Other Note",
            "Other Note|alias",
            "Other Note#^b1",
            "Rename Notes",
            "Rename Notes#^k3f9a2",
            "@tephra:docs/sibling",
            "Not Planned",
        ];
        let text = "[[Other Note]] [[Other Note|alias]] ![[Other Note#^b1]] self [[Rename Notes]] [[Rename Notes#^k3f9a2]] [[@tephra:docs/sibling]] [[Not Planned]]\n";
        assert_eq!(
            moved_text("@tephra:docs/rename-notes.md", text, &raws, &rewrite),
            "[[@notes:Other Note]] [[@notes:Other Note|alias]] ![[@notes:Other Note#^b1]] self [[rename-notes]] [[rename-notes#^k3f9a2]] [[docs/sibling]] [[Not Planned]]\n"
        );
    }

    #[test]
    fn rewrite_three_respells_a_link_to_a_sibling_in_a_promoted_folder_and_keeps_anchor_and_alias()
    {
        let links = HashMap::from([
            (
                "café notes/Plain#^b1|the plain".to_string(),
                OwnLink::Retarget("Plain".into()),
            ),
            (
                "@notes:café notes/sub/Deep".to_string(),
                OwnLink::Retarget("sub/Deep".into()),
            ),
        ]);
        let rewrite = MoveRewrite::Own {
            new_path: "Ünï.md".into(),
            bare_resolves: true,
            links,
        };
        let raws = [
            "café notes/Plain#^b1|the plain",
            "@notes:café notes/sub/Deep",
        ];
        let text = "a [[café notes/Plain#^b1|the plain]] b ![[@notes:café notes/sub/Deep]] c\n";
        assert_eq!(
            moved_text("@cafe:Ünï.md", text, &raws, &rewrite),
            "a [[Plain#^b1|the plain]] b ![[sub/Deep]] c\n"
        );
    }

    #[test]
    fn a_move_rewrite_never_touches_markdown_links_or_text_outside_link_spans() {
        let rewrite = MoveRewrite::OldSource {
            new_source: "tephra".into(),
            new_path: "docs/rename-notes.md".into(),
        };
        let mut edges = dangling_edges("@notes:Index.md", &["Rename Notes"]);
        edges.push(Edge {
            kind: tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            to_raw: "Rename%20Notes.md".into(),
            ..edges[0].clone()
        });
        let text = "keep [[Rename Notes]] and [x](Rename%20Notes.md) as they are\n";
        let contents = HashMap::from([("@notes:Index.md".to_string(), text.to_string())]);
        let changes = compute_move_changes(&edges, &contents, &rewrite);
        assert_eq!(changes.len(), 1, "only the wikilink: {changes:?}");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        assert_eq!(
            apply_changes_to_content(text, &refs).unwrap(),
            "keep [[@tephra:docs/rename-notes]] and [x](Rename%20Notes.md) as they are\n"
        );
    }

    #[test]
    fn move_new_raw_is_pure_and_leaves_a_link_it_has_no_rule_for() {
        let own = MoveRewrite::Own {
            new_path: "docs/a.md".into(),
            bare_resolves: true,
            links: HashMap::new(),
        };
        assert_eq!(move_new_raw("Anything", &own), None);
        let one = MoveRewrite::OldSource {
            new_source: "tephra".into(),
            new_path: "docs/a.md".into(),
        };
        assert_eq!(
            move_new_raw("#Heading", &one),
            None,
            "an anchor-only link has no target"
        );
        assert_eq!(
            move_new_raw("@tephra:docs/a", &one),
            None,
            "a link already written as the rewrite would write it is not changed"
        );
    }

    // --- compute_changes --------------------------------------------------

    #[test]
    fn single_source_wikilink_match() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let mut files = HashMap::new();
        files.insert(
            "@notes:Journal.md".to_string(),
            "Morning note.\n\nSee [[OldDoc]] for details.\n".to_string(),
        );
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file, "@notes:Journal.md");
        assert_eq!(changes[0].line, 3);
        assert_eq!(changes[0].before, "[[OldDoc]]");
        assert_eq!(changes[0].after, "[[NewDoc]]");
    }

    #[test]
    fn wikilink_alias_is_preserved() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc|Old Doc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let mut files = HashMap::new();
        files.insert(
            "@notes:Journal.md".to_string(),
            "[[OldDoc|Old Doc]]\n".to_string(),
        );
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "[[OldDoc|Old Doc]]");
        assert_eq!(changes[0].after, "[[NewDoc|Old Doc]]");
    }

    #[test]
    fn cross_source_match() {
        let edges = vec![edge(
            "@atlas-repo:README.md",
            "@notes:OldDoc.md",
            "@notes:OldDoc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let mut files = HashMap::new();
        files.insert(
            "@atlas-repo:README.md".to_string(),
            "Linked from another source: [[@notes:OldDoc]]\n".to_string(),
        );
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file, "@atlas-repo:README.md");
        assert_eq!(changes[0].before, "[[@notes:OldDoc]]");
        assert_eq!(changes[0].after, "[[@notes:NewDoc]]");
    }

    #[test]
    fn zero_matches_returns_empty() {
        let changes = compute_changes(&[], &HashMap::new(), "OldDoc.md", "NewDoc.md");
        assert!(changes.is_empty());
    }

    #[test]
    fn identical_old_and_new_path_returns_empty_diff() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), "[[OldDoc]]\n".to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "OldDoc.md");
        assert!(changes.is_empty());
    }

    #[test]
    fn case_only_rename_rewrites_without_duplication() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), "[[OldDoc]]\n".to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "olddoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].after, "[[olddoc]]");
    }

    #[test]
    fn markdown_link_same_directory() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let mut files = HashMap::new();
        files.insert(
            "@notes:Journal.md".to_string(),
            "See [doc](OldDoc.md) for more.\n".to_string(),
        );
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "OldDoc.md");
        assert_eq!(changes[0].after, "NewDoc.md");
    }

    #[test]
    fn markdown_link_relative_directory_move() {
        let edges = vec![edge(
            "@notes:sub/Journal.md",
            "@notes:OldDoc.md",
            "../OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let mut files = HashMap::new();
        files.insert(
            "@notes:sub/Journal.md".to_string(),
            "[doc](../OldDoc.md)\n".to_string(),
        );
        let changes = compute_changes(&edges, &files, "OldDoc.md", "archive/NewDoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].after, "../archive/NewDoc.md");
    }

    /// R9 review finding: a titled markdown link's destination is
    /// rewritten in place, leaving the title untouched — the whole
    /// point of recording a span that excludes the title at all.
    #[test]
    fn markdown_link_with_title_rewrites_only_the_destination() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "See [doc](OldDoc.md \"The title\") for more.\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "OldDoc.md");
        assert_eq!(changes[0].after, "NewDoc.md");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(result, "See [doc](NewDoc.md \"The title\") for more.\n");
    }

    /// R9 review finding: an angle-bracket destination is rewritten
    /// in place, with the `<`/`>` delimiters preserved around the new
    /// destination text.
    #[test]
    fn markdown_link_with_angle_bracket_destination_is_rewritten() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "Old Doc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "See [doc](<Old Doc.md>) for more.\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "Old Doc.md", "New Doc.md");
        assert_eq!(changes.len(), 1);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(result, "See [doc](<New Doc.md>) for more.\n");
    }

    /// R9 review finding: a bare destination containing its own
    /// balanced, unescaped `(...)` is rewritten in full, not
    /// truncated at the destination's own inner paren.
    #[test]
    fn markdown_link_with_paren_in_destination_is_rewritten_in_full() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "Old(2).md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "See [doc](Old(2).md) for more.\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "Old(2).md", "New(3).md");
        assert_eq!(changes.len(), 1);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(result, "See [doc](New(3).md) for more.\n");
    }

    /// R12 review finding: renaming to a destination that contains a
    /// space is not a safe bare CommonMark destination — writing it
    /// literally would make the title-less `)` at the space's
    /// position terminate the destination early, silently turning
    /// the rest of the link into plain text. The rewrite must
    /// percent-encode it instead, and the result must still parse
    /// back to a markdown link whose target resolves to the new
    /// path.
    #[test]
    fn markdown_link_renamed_to_a_path_with_a_space_is_percent_encoded() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "Bare: [d](OldDoc.md) end.\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "New Doc (2).md");
        assert_eq!(changes.len(), 1);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(result, "Bare: [d](New%20Doc%20%282%29.md) end.\n");

        // The rewritten file still parses to a link whose target
        // resolves to the new path (R12's acceptance bar).
        let reparsed = tephra_parser::parse_unit(Path::new("@notes:Journal.md"), &result);
        assert_eq!(reparsed.markdown_links.len(), 1);
        assert_eq!(
            percent_decode(&reparsed.markdown_links[0].target),
            "New Doc (2).md"
        );
    }

    /// R12: the same rename, but through a titled markdown link —
    /// only the destination is percent-encoded; the title is left
    /// exactly as written.
    #[test]
    fn markdown_link_with_title_renamed_to_a_path_with_a_space_percent_encodes_only_the_destination(
    ) {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "[d](OldDoc.md \"The title\")\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "New Doc (2).md");
        assert_eq!(changes.len(), 1);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(result, "[d](New%20Doc%20%282%29.md \"The title\")\n");

        let reparsed = tephra_parser::parse_unit(Path::new("@notes:Journal.md"), &result);
        assert_eq!(reparsed.markdown_links.len(), 1);
        assert_eq!(
            percent_decode(&reparsed.markdown_links[0].target),
            "New Doc (2).md"
        );
    }

    /// R12: a relative markdown link renamed across a directory to a
    /// path with a space — the `../` segments stay unencoded (they
    /// are in `percent_encode_path`'s unreserved set), only the
    /// filename's unsafe characters are encoded.
    #[test]
    fn relative_markdown_link_renamed_to_a_path_with_a_space_is_percent_encoded() {
        let edges = vec![edge(
            "@notes:sub/Deep.md",
            "@notes:OldDoc.md",
            "../OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "[d](../OldDoc.md)\n";
        let mut files = HashMap::new();
        files.insert("@notes:sub/Deep.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "Café Notes (2).md");
        assert_eq!(changes.len(), 1);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");

        let reparsed = tephra_parser::parse_unit(Path::new("@notes:sub/Deep.md"), &result);
        assert_eq!(reparsed.markdown_links.len(), 1);
        assert_eq!(
            percent_decode(&reparsed.markdown_links[0].target),
            "../Café Notes (2).md"
        );
    }

    /// R13 review finding, exact repro: a bare markdown destination
    /// written with backslash escapes (`Old\(2\).md`, decoding to
    /// `Old(2).md`) has raw source bytes that differ from the
    /// parser's decoded target — `before` must come from those raw
    /// bytes, not the decoded value, or the byte-exact check in
    /// `apply_changes_to_content` always fails.
    #[test]
    fn markdown_link_with_backslash_escaped_destination_is_applied() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "Old(2).md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "[d](Old\\(2\\).md)\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "Old(2).md", "New(3).md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "Old\\(2\\).md");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");

        let reparsed = tephra_parser::parse_unit(Path::new("@notes:Journal.md"), &result);
        assert_eq!(reparsed.markdown_links.len(), 1);
        assert_eq!(
            percent_decode(&reparsed.markdown_links[0].target),
            "New(3).md"
        );
    }

    /// R13: an HTML entity reference (`&amp;`) in the destination has
    /// the same raw-vs-decoded mismatch as a backslash escape.
    #[test]
    fn markdown_link_with_html_entity_in_destination_is_applied() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "Old&Doc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "[d](Old&amp;Doc.md)\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "Old&Doc.md", "New&Doc.md");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "Old&amp;Doc.md");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");

        let reparsed = tephra_parser::parse_unit(Path::new("@notes:Journal.md"), &result);
        assert_eq!(reparsed.markdown_links.len(), 1);
        assert_eq!(
            percent_decode(&reparsed.markdown_links[0].target),
            "New&Doc.md"
        );
    }

    #[test]
    fn unreadable_referring_file_is_skipped_not_errored() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        // No entry in `files` for "@notes:Journal.md".
        let changes = compute_changes(&edges, &HashMap::new(), "OldDoc.md", "NewDoc.md");
        assert!(changes.is_empty());
    }

    // --- T1: a file linking to the same target via more than one ------------
    // --- link style/spelling must have every occurrence rewritten -----------

    /// T1 review finding, exact repro: a file containing both a
    /// wikilink and a markdown link to the same renamed target used
    /// to have only one of the two rewritten, because
    /// `tephra_graph::UnitGraph::get_backlinks`'s query-display dedup
    /// collapsed both edges (same `from`, same resolved target) into
    /// one before `compute_changes` ever saw them — whichever
    /// `(kind, to_raw)` pair sorted first silently became the only
    /// one considered for that file. `compute_changes` (fed by
    /// `get_backlinks_all` now, via `RefactorEngine::preview`) must
    /// see both edges and rewrite both occurrences.
    #[test]
    fn compute_changes_rewrites_every_link_style_to_the_same_target_in_one_file() {
        let edges = vec![
            edge(
                "@notes:Journal.md",
                "@notes:OldDoc.md",
                "OldDoc",
                tephra_parser::LINK_KIND_WIKILINK,
            ),
            edge(
                "@notes:Journal.md",
                "@notes:OldDoc.md",
                "OldDoc.md",
                tephra_parser::LINK_KIND_MARKDOWN,
            ),
        ];
        let content = "Link to [[OldDoc]] and also [desc](OldDoc.md).\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 2, "{changes:?}");

        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("spans match");
        assert_eq!(result, "Link to [[NewDoc]] and also [desc](NewDoc.md).\n");
    }

    /// T1: the spec's own unicode/space rename, with a bare markdown
    /// link, a titled markdown link, and a wikilink all pointing at
    /// the same target in one file — all three must be found and
    /// rewritten, not just one.
    #[test]
    fn compute_changes_rewrites_three_mixed_link_styles_to_the_same_unicode_target() {
        let edges = vec![
            edge(
                "@notes:Journal.md",
                "@notes:Café Notes.md",
                "Café Notes",
                tephra_parser::LINK_KIND_WIKILINK,
            ),
            edge(
                "@notes:Journal.md",
                "@notes:Café Notes.md",
                "Caf%C3%A9%20Notes.md",
                tephra_parser::LINK_KIND_MARKDOWN,
            ),
            edge(
                "@notes:Journal.md",
                "@notes:Café Notes.md",
                "Caf%C3%A9%20Notes.md",
                tephra_parser::LINK_KIND_MARKDOWN,
            ),
        ];
        let content = "Wiki: [[Café Notes]]\nBare: [b](Caf%C3%A9%20Notes.md)\nTitled: [t](Caf%C3%A9%20Notes.md \"Notes\")\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        // Two of the three edges above share the exact same
        // (kind, to_raw) pair but target two genuinely distinct
        // occurrences in `content` (the bare and titled links) — both
        // must still be found, via `locate_and_diff`'s own full scan,
        // not just one.
        let changes = compute_changes(&edges, &files, "Café Notes.md", "Café Notes (2).md");
        assert_eq!(changes.len(), 3, "{changes:?}");

        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("spans match");
        let reparsed = tephra_parser::parse_unit(Path::new("@notes:Journal.md"), &result);
        assert_eq!(reparsed.wikilinks.len(), 1);
        assert_eq!(reparsed.wikilinks[0].raw, "Café Notes (2)");
        assert_eq!(reparsed.markdown_links.len(), 2);
        for m in &reparsed.markdown_links {
            assert_eq!(
                percent_decode(&m.target),
                "Café Notes (2).md",
                "{:?}",
                reparsed.markdown_links
            );
        }
    }

    /// T1: two `Edge` records for the exact same `(from, kind, to_raw)`
    /// triple (the same raw link occurring twice, verbatim, in the
    /// same file — one cache row per occurrence, R16) must not cause
    /// `compute_changes` to re-locate and double-emit the same
    /// span(s): `apply_changes_to_content` would then try to rewrite
    /// an already-rewritten span and fail.
    #[test]
    fn compute_changes_does_not_duplicate_a_repeated_identical_raw_link() {
        let edges = vec![
            edge(
                "@notes:Journal.md",
                "@notes:OldDoc.md",
                "OldDoc",
                tephra_parser::LINK_KIND_WIKILINK,
            ),
            edge(
                "@notes:Journal.md",
                "@notes:OldDoc.md",
                "OldDoc",
                tephra_parser::LINK_KIND_WIKILINK,
            ),
        ];
        let content = "See [[OldDoc]] and again [[OldDoc]].\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(
            changes.len(),
            2,
            "two real occurrences, not four from the duplicate edge: {changes:?}"
        );

        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("spans match");
        assert_eq!(result, "See [[NewDoc]] and again [[NewDoc]].\n");
    }

    // --- T1: RefactorEngine::preview/apply (not just compute_changes) --------
    // --- also rewrite every link style in one file --------------------------

    /// T1, engine-level: the live repro from the review/tester finding
    /// — `RefactorEngine::preview` then `apply` against a file
    /// containing both a wikilink and a markdown link to the same
    /// renamed target must rewrite both, and the preview must have
    /// already reported both as changes (GP7/GP8).
    #[test]
    fn apply_rewrites_every_link_style_to_the_same_target_in_one_file() {
        let dir = std::env::temp_dir().join(format!(
            "tephra-refactor-mixed-kind-test-{}",
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("Journal.md");
        fs::write(&journal, "Link to [[OldDoc]] and also [desc](OldDoc.md).\n").unwrap();

        let graph = FakeGraphBackend {
            edges: vec![
                edge(
                    "@notes:Journal.md",
                    "@notes:OldDoc.md",
                    "OldDoc",
                    tephra_parser::LINK_KIND_WIKILINK,
                ),
                edge(
                    "@notes:Journal.md",
                    "@notes:OldDoc.md",
                    "OldDoc.md",
                    tephra_parser::LINK_KIND_MARKDOWN,
                ),
            ],
        };
        let sources = FakeSourceBackend {
            name: "notes".to_string(),
            path: dir.to_string_lossy().to_string(),
        };
        let watcher = std::sync::Arc::new(NoopWatcherPause);
        let engine = RefactorEngine::new(
            std::sync::Arc::new(graph),
            std::sync::Arc::new(sources),
            watcher,
        );

        let preview = engine.preview("notes", "OldDoc.md", "NewDoc.md").unwrap();
        assert_eq!(
            preview.changes.len(),
            2,
            "preview must list both the wikilink and the markdown link: {:?}",
            preview.changes
        );

        let outcome = engine.apply("notes", "OldDoc.md", "NewDoc.md").unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied { files_changed: 1 });
        assert_eq!(
            fs::read_to_string(&journal).unwrap(),
            "Link to [[NewDoc]] and also [desc](NewDoc.md).\n",
            "both link styles must be rewritten, not just one"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- apply_changes_to_content ------------------------------------------

    #[test]
    fn apply_changes_only_touches_targeted_spans() {
        let content = "Intro line.\nSee [[OldDoc]] and also [[OldDoc]] again.\nOutro line.\n";
        let changes = [
            LinkChange {
                file: "@notes:Journal.md".into(),
                line: 2,
                before: "[[OldDoc]]".into(),
                after: "[[NewDoc]]".into(),
                byte_start: 16,
                byte_end: 26,
            },
            LinkChange {
                file: "@notes:Journal.md".into(),
                line: 2,
                before: "[[OldDoc]]".into(),
                after: "[[NewDoc]]".into(),
                byte_start: 36,
                byte_end: 46,
            },
        ];
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("spans match");
        assert_eq!(
            result,
            "Intro line.\nSee [[NewDoc]] and also [[NewDoc]] again.\nOutro line.\n"
        );
    }

    /// R1 review finding, exact repro: a wikilink inside inline code
    /// that reads identically to a real wikilink elsewhere in the
    /// same file must never be mistaken for the real occurrence —
    /// `tephra_parser` itself never recognises a `[[...]]` inside an
    /// inline code span as a wikilink at all, so `compute_changes`
    /// (locating purely via the parser's own spans, not a textual
    /// search) only ever produces a change for the real one.
    #[test]
    fn inline_code_occurrence_of_old_name_is_never_rewritten() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let content = "Type `[[OldDoc]]` to get [[OldDoc]].\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(
            changes.len(),
            1,
            "the inline-code occurrence must not be located as a link: {changes:?}"
        );
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(result, "Type `[[OldDoc]]` to get [[NewDoc]].\n");
    }

    /// R1 review finding: a markdown link whose destination also
    /// happens to appear, verbatim and parenthesized, as plain prose
    /// earlier on the very same line must not corrupt that prose —
    /// only the real `[text](...)` link's own recorded span is ever
    /// rewritten.
    #[test]
    fn markdown_link_with_coincidental_parenthesized_text_on_the_same_line_is_untouched() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:OldDoc.md",
            "OldDoc.md",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "See (OldDoc.md) in prose, but the real link is [doc](OldDoc.md).\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "OldDoc.md", "NewDoc.md");
        assert_eq!(changes.len(), 1);
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert_eq!(
            result,
            "See (OldDoc.md) in prose, but the real link is [doc](NewDoc.md).\n"
        );
    }

    /// R1/R7 review finding: if the bytes at a change's recorded span
    /// no longer equal `before` (the file changed underneath in a way
    /// the staleness gate didn't catch — simulated here directly,
    /// without any real file), `apply_changes_to_content` must refuse
    /// rather than rewrite whatever happens to be there.
    #[test]
    fn apply_changes_to_content_errors_if_the_recorded_span_no_longer_matches() {
        let changes = [LinkChange {
            file: "@notes:Journal.md".into(),
            line: 1,
            before: "[[OldDoc]]".into(),
            after: "[[NewDoc]]".into(),
            byte_start: 4,
            byte_end: 14,
        }];
        let changed_content = "See [[Other!]] here.\n";
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(changed_content, &refs);
        assert!(result.is_err());
    }

    // --- verify_staleness ---------------------------------------------------

    #[test]
    fn staleness_all_fresh_is_ok() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let mut expected = HashMap::new();
        expected.insert("@notes:Journal.md".to_string(), t);
        let result = verify_staleness(&expected, |_| Ok(t));
        assert!(result.is_ok());
    }

    #[test]
    fn staleness_one_stale_aborts() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let later = t + std::time::Duration::from_secs(5);
        let mut expected = HashMap::new();
        expected.insert("@notes:Journal.md".to_string(), t);
        let result = verify_staleness(&expected, |_| Ok(later));
        let err = result.unwrap_err();
        assert!(err.contains("modified since preview"));
        assert!(err.contains("@notes:Journal.md"));
    }

    #[test]
    fn staleness_missing_file_aborts() {
        let t = SystemTime::UNIX_EPOCH;
        let mut expected = HashMap::new();
        expected.insert("@notes:Journal.md".to_string(), t);
        let result = verify_staleness(&expected, |_| {
            Err(io::Error::new(io::ErrorKind::NotFound, "nope"))
        });
        assert!(result.unwrap_err().contains("missing since preview"));
    }

    #[test]
    fn staleness_permission_denied_aborts() {
        let t = SystemTime::UNIX_EPOCH;
        let mut expected = HashMap::new();
        expected.insert("@notes:Journal.md".to_string(), t);
        let result = verify_staleness(&expected, |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        });
        assert!(result.unwrap_err().contains("unreadable since preview"));
    }

    // --- rewrite_links_atomic ------------------------------------------------

    /// Records every call so a test can assert the write happens to a
    /// *different* path than `target`, and that a failed rename still
    /// cleans up the temp file.
    struct RecordingFs {
        fail_rename: bool,
        calls: Mutex<Vec<String>>,
    }

    impl AtomicFs for RecordingFs {
        fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("write_new:{}", path.display()));
            fs::write(path, content)
        }
        fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_permissions:{}", path.display()));
            fs::set_permissions(path, perms)
        }
        fn sync(&self, path: &Path) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("sync:{}", path.display()));
            Ok(())
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rename:{}->{}", from.display(), to.display()));
            if self.fail_rename {
                return Err(io::Error::other("simulated rename failure"));
            }
            fs::rename(from, to)
        }
    }

    #[test]
    fn atomic_rewrite_writes_temp_then_renames() {
        let dir =
            std::env::temp_dir().join(format!("tephra-refactor-test-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("Doc.md");
        fs::write(&target, b"old content").unwrap();

        let fs_ops = RecordingFs {
            fail_rename: false,
            calls: Mutex::new(Vec::new()),
        };
        rewrite_links_atomic_with(&fs_ops, &target, b"new content").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new content");
        let calls = fs_ops.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].starts_with("write_new:"));
        assert!(
            !calls[0].contains("Doc.md\""),
            "temp write must not target the real file path directly"
        );
        assert!(calls[0] != format!("write_new:{}", target.display()));
        assert!(calls[1].starts_with("set_permissions:"));
        assert!(calls[2].starts_with("sync:"));
        assert!(calls[3].ends_with(&format!("->{}", target.display())));

        // No leftover temp files after a successful rename.
        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(remaining.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_rewrite_cleans_up_temp_file_on_failed_rename() {
        let dir =
            std::env::temp_dir().join(format!("tephra-refactor-test-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("Doc.md");
        fs::write(&target, b"old content").unwrap();

        let fs_ops = RecordingFs {
            fail_rename: true,
            calls: Mutex::new(Vec::new()),
        };
        let result = rewrite_links_atomic_with(&fs_ops, &target, b"new content");
        assert!(result.is_err());
        // Original file untouched.
        assert_eq!(fs::read_to_string(&target).unwrap(), "old content");
        // No leftover temp files after the failure's cleanup.
        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(remaining.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_rewrite_target_never_observed_half_written() {
        // Real filesystem, no mock: a reader thread polls the target
        // file's content in a tight loop while the main thread
        // performs the atomic rewrite, recording every distinct
        // content it observes. Because the write lands on a
        // different path until the final `rename`, the only possible
        // observations are "fully old" or "fully new" — never a
        // truncated/partial state.
        let dir =
            std::env::temp_dir().join(format!("tephra-refactor-test-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("Doc.md");
        let old_content = "old content ".repeat(10_000);
        let new_content = "new content ".repeat(10_000);
        fs::write(&target, &old_content).unwrap();

        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = std::sync::Arc::new(Mutex::new(HashSet::new()));
        let reader_target = target.clone();
        let reader_done = done.clone();
        let reader_observed = observed.clone();
        let reader = std::thread::spawn(move || {
            while !reader_done.load(std::sync::atomic::Ordering::SeqCst) {
                if let Ok(content) = fs::read_to_string(&reader_target) {
                    reader_observed.lock().unwrap().insert(content);
                }
            }
        });

        rewrite_links_atomic(&target, new_content.as_bytes()).unwrap();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        reader.join().unwrap();

        let observed = observed.lock().unwrap();
        for content in observed.iter() {
            assert!(
                *content == old_content || *content == new_content,
                "observed a partially-written file"
            );
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), new_content);

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- R2: permissions / symlinks -----------------------------------------

    #[test]
    #[cfg(unix)]
    fn atomic_rewrite_preserves_the_targets_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("tephra-refactor-test-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("Private.md");
        fs::write(&target, b"old content").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        rewrite_links_atomic(&target, b"new content").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new content");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "apply_refactor must not change the file's permission bits"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn atomic_rewrite_refuses_to_replace_a_symlink() {
        let dir =
            std::env::temp_dir().join(format!("tephra-refactor-test-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("Real.md");
        fs::write(&real, b"old content").unwrap();
        let link = dir.join("Link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let result = rewrite_links_atomic(&link, b"new content");
        assert!(result.is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself must be left exactly as it was"
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "old content",
            "the symlink's target must be untouched"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- R3: stale temp-file recognition --------------------------------------

    #[test]
    fn is_stale_temp_file_name_recognizes_the_atomic_rewrite_convention() {
        assert!(is_stale_temp_file_name(".Doc.md.tephra-tmp-123456"));
        assert!(is_stale_temp_file_name(".a.tephra-tmp-0"));
        assert!(!is_stale_temp_file_name("Doc.md"));
        assert!(!is_stale_temp_file_name(".Doc.md"));
        assert!(!is_stale_temp_file_name(".Doc.md.tephra-tmp-"));
        assert!(!is_stale_temp_file_name(".Doc.md.tephra-tmp-12a"));
        assert!(!is_stale_temp_file_name("Doc.md.tephra-tmp-123"));
    }

    // --- R4: apply pauses/resumes the watcher around its own writes ----------

    struct FakeGraphBackend {
        edges: Vec<Edge>,
    }

    impl GraphBackend for FakeGraphBackend {
        fn resolve_link(
            &self,
            _raw: &str,
            _default_source: Option<&str>,
        ) -> Result<ResolveResult, ResolveError> {
            unimplemented!("not exercised by RefactorEngine::preview/apply")
        }
        fn get_backlinks(&self, _id: &str) -> Vec<Edge> {
            self.edges.clone()
        }
        fn get_forward_links(&self, _id: &str) -> Vec<Edge> {
            Vec::new()
        }
        fn get_edges(&self) -> Vec<Edge> {
            self.edges.clone()
        }
    }

    struct FakeSourceBackend {
        name: String,
        path: String,
    }

    impl SourceBackend for FakeSourceBackend {
        fn register(&self, _name: &str, _root: &str) -> Result<ScanStatsWire, String> {
            unimplemented!("not exercised by RefactorEngine::preview/apply")
        }
        fn unregister(&self, _name: &str) -> Result<(), String> {
            unimplemented!("not exercised by RefactorEngine::preview/apply")
        }
        fn list(&self) -> Result<Vec<SourceRowWire>, String> {
            Ok(vec![SourceRowWire {
                name: self.name.clone(),
                path: self.path.clone(),
                status: "active".to_string(),
                last_scanned_at: None,
                remote: None,
            }])
        }
        fn rescan(&self, _name: &str) -> Result<ScanStatsWire, String> {
            Ok(ScanStatsWire {
                files_seen: 0,
                files_ignored: 0,
                units_written: 0,
                links_written: 0,
                tags_written: 0,
                blocks_written: 0,
            })
        }
    }

    struct RecordingWatcherPauseControl {
        log: Mutex<Vec<String>>,
    }

    impl WatcherPauseControl for RecordingWatcherPauseControl {
        fn pause_all(&self) {
            self.log.lock().unwrap().push("pause".to_string());
        }
        fn resume_all(&self, written_paths: &HashSet<PathBuf>) {
            let mut paths: Vec<String> = written_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            paths.sort();
            self.log
                .lock()
                .unwrap()
                .push(format!("resume:{}", paths.join(",")));
        }
    }

    #[test]
    fn apply_pauses_and_resumes_the_watcher_with_exactly_the_paths_it_wrote() {
        let dir = std::env::temp_dir().join(format!(
            "tephra-refactor-apply-test-{}",
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("Journal.md");
        fs::write(&journal, "See [[OldDoc]] for details.\n").unwrap();

        let graph = FakeGraphBackend {
            edges: vec![edge(
                "@notes:Journal.md",
                "@notes:OldDoc.md",
                "OldDoc",
                tephra_parser::LINK_KIND_WIKILINK,
            )],
        };
        let sources = FakeSourceBackend {
            name: "notes".to_string(),
            path: dir.to_string_lossy().to_string(),
        };
        let watcher = std::sync::Arc::new(RecordingWatcherPauseControl {
            log: Mutex::new(Vec::new()),
        });

        let engine = RefactorEngine::new(
            std::sync::Arc::new(graph),
            std::sync::Arc::new(sources),
            watcher.clone(),
        );

        engine.preview("notes", "OldDoc.md", "NewDoc.md").unwrap();
        let outcome = engine.apply("notes", "OldDoc.md", "NewDoc.md").unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied { files_changed: 1 });
        assert_eq!(
            fs::read_to_string(&journal).unwrap(),
            "See [[NewDoc]] for details.\n"
        );

        let log = watcher.log.lock().unwrap();
        assert_eq!(
            log.len(),
            2,
            "expected exactly one pause then one resume: {log:?}"
        );
        assert_eq!(log[0], "pause");
        assert!(
            log[1].starts_with("resume:") && log[1].contains(&journal.display().to_string()),
            "resume must be told exactly the path apply itself wrote: {log:?}"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- R10: preview reports files it could not read/stat -------------------

    /// R10 review finding: `RefactorEngine::preview` must name a
    /// referring file it could not read at all in `skipped`, not
    /// just silently narrow `changes` with no indication anything
    /// was left out.
    #[test]
    #[cfg(unix)]
    fn preview_reports_an_unreadable_referring_file_as_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "tephra-refactor-preview-skip-test-{}",
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("Journal.md");
        fs::write(&journal, "See [[OldDoc]] for details.\n").unwrap();
        let secret = dir.join("Secret.md");
        fs::write(&secret, "Also see [[OldDoc]].\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

        let graph = FakeGraphBackend {
            edges: vec![
                edge(
                    "@notes:Journal.md",
                    "@notes:OldDoc.md",
                    "OldDoc",
                    tephra_parser::LINK_KIND_WIKILINK,
                ),
                edge(
                    "@notes:Secret.md",
                    "@notes:OldDoc.md",
                    "OldDoc",
                    tephra_parser::LINK_KIND_WIKILINK,
                ),
            ],
        };
        let sources = FakeSourceBackend {
            name: "notes".to_string(),
            path: dir.to_string_lossy().to_string(),
        };
        let watcher = std::sync::Arc::new(RecordingWatcherPauseControl {
            log: Mutex::new(Vec::new()),
        });
        let engine = RefactorEngine::new(
            std::sync::Arc::new(graph),
            std::sync::Arc::new(sources),
            watcher,
        );

        // Reading as root (e.g. inside some CI sandboxes) bypasses
        // the permission bits this test depends on; skip rather than
        // produce a false failure if so.
        if fs::read_to_string(&secret).is_ok() {
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
            fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let result = engine.preview("notes", "OldDoc.md", "NewDoc.md").unwrap();
        assert_eq!(result.changes.len(), 1, "{:?}", result.changes);
        assert_eq!(result.changes[0].file, "@notes:Journal.md");
        assert_eq!(result.skipped.len(), 1, "{:?}", result.skipped);
        assert_eq!(result.skipped[0].file, "@notes:Secret.md");
        assert!(
            !result.skipped[0].error.is_empty(),
            "skipped entry must explain why"
        );

        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    // --- relative_path / percent encode-decode -------------------------------

    #[test]
    fn relative_path_same_directory_has_no_prefix() {
        assert_eq!(relative_path("sub", "sub/NewDoc.md"), "NewDoc.md");
        assert_eq!(relative_path("", "NewDoc.md"), "NewDoc.md");
    }

    #[test]
    fn relative_path_crosses_directories() {
        assert_eq!(relative_path("sub", "NewDoc.md"), "../NewDoc.md");
        assert_eq!(relative_path("a/b", "a/c/NewDoc.md"), "../c/NewDoc.md");
    }

    #[test]
    fn percent_encoding_round_trips_spaces_and_unicode() {
        let encoded = percent_encode_path("Caf\u{e9} Notes (2).md");
        let decoded = percent_decode(&encoded);
        assert_eq!(decoded, "Caf\u{e9} Notes (2).md");
    }

    // --- multi-byte names (E26) ---------------------------------------------

    /// A 2-, 3- and 4-byte character each, plus a second 3-byte one and
    /// a CJK character.
    const MULTIBYTE: [&str; 5] = ["é", "—", "…", "日", "😀"];

    /// The expected `strip_md_ext` result, worked out a different way
    /// from the code under test: `strip_suffix` on an ASCII-lower-cased
    /// copy (same byte offsets) is boundary-safe by construction.
    fn md_ext_oracle(name: &str) -> (&str, Option<&str>) {
        let lower = name.to_ascii_lowercase();
        for ext in [".md", ".markdown"] {
            if let Some(stem) = lower.strip_suffix(ext) {
                if !stem.is_empty() {
                    return (&name[..stem.len()], Some(&name[stem.len()..]));
                }
            }
        }
        (name, None)
    }

    #[test]
    fn strip_md_ext_keeps_todays_results() {
        let cases: [(&str, (&str, Option<&str>)); 12] = [
            ("Plan.md", ("Plan", Some(".md"))),
            ("Plan.MD", ("Plan", Some(".MD"))),
            ("Plan.Md", ("Plan", Some(".Md"))),
            ("Plan.markdown", ("Plan", Some(".markdown"))),
            ("Plan.MarkDown", ("Plan", Some(".MarkDown"))),
            ("dir/Plan.md", ("dir/Plan", Some(".md"))),
            ("Plan.txt", ("Plan.txt", None)),
            ("Plan", ("Plan", None)),
            (".md", (".md", None)),
            (".markdown", (".markdown", None)),
            ("md", ("md", None)),
            ("", ("", None)),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_md_ext(input), expected, "{input:?}");
        }
    }

    #[test]
    fn strip_md_ext_returns_the_input_when_the_cut_is_not_a_char_boundary() {
        for name in ["Résumé—v2.docx", "Todo…ab", "日本x", "Résumé—v2.markdowñ"] {
            assert_eq!(strip_md_ext(name), (name, None), "{name:?}");
        }
        for c in MULTIBYTE {
            for letters in 1..=8 {
                let name = format!("Todo{c}{}", "a".repeat(letters));
                assert_eq!(strip_md_ext(&name), (name.as_str(), None), "{name:?}");
            }
        }
    }

    #[test]
    fn strip_md_ext_keeps_the_extension_as_written_after_a_multibyte_stem() {
        assert_eq!(strip_md_ext("Plan…ab.md"), ("Plan…ab", Some(".md")));
        assert_eq!(strip_md_ext("Résumé—v2.MD"), ("Résumé—v2", Some(".MD")));
        assert_eq!(strip_md_ext("日本.Markdown"), ("日本", Some(".Markdown")));
        assert_eq!(strip_md_ext("😀.MARKDOWN"), ("😀", Some(".MARKDOWN")));
    }

    /// Each multi-byte character at every distance from 0 to 12 bytes
    /// before the end, around both the 3-byte `.md` cut and the 9-byte
    /// `.markdown` cut: no case panics, each gives the oracle's answer.
    #[test]
    fn strip_md_ext_cut_position_table_never_panics() {
        let mut checked = 0;
        for c in MULTIBYTE {
            for pad in 0..=12 {
                let ascii = "a".repeat(pad);
                for name in [
                    format!("{c}{ascii}"),
                    format!("x{c}{ascii}"),
                    format!("{c}{c}{ascii}"),
                    format!("ab{c}{ascii}"),
                    format!("{c}.md{ascii}"),
                    format!("{c}.markdown{ascii}"),
                    format!("{c}{ascii}.md"),
                    format!("{c}{ascii}.markdown"),
                ] {
                    assert_eq!(strip_md_ext(&name), md_ext_oracle(&name), "{name:?}");
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 5 * 13 * 8);
    }

    #[test]
    fn strip_md_ext_name_as_long_as_the_extension_empty_or_only_multibyte() {
        for name in [
            "",
            ".md",
            ".MD",
            ".markdown",
            "日",
            "é—",
            "…ab",
            "日本語",
            "😀😀😀😀😀😀",
            "ééééé—",
            "é",
            "—",
            "😀",
        ] {
            assert_eq!(strip_md_ext(name), (name, None), "{name:?}");
        }
    }

    #[test]
    fn strip_md_ext_exhaustive_short_strings_never_panic() {
        const TOKENS: [&str; 8] = ["a", ".", "md", "MD", "markdown", "é", "—", "😀"];
        fn walk(prefix: &mut String, depth: usize, count: &mut usize) {
            assert_eq!(strip_md_ext(prefix), md_ext_oracle(prefix), "{prefix:?}");
            *count += 1;
            if depth == 0 {
                return;
            }
            for t in TOKENS {
                let len = prefix.len();
                prefix.push_str(t);
                walk(prefix, depth - 1, count);
                prefix.truncate(len);
            }
        }
        let mut count = 0;
        walk(&mut String::new(), 6, &mut count);
        assert_eq!(count, (0..=6).map(|n| 8usize.pow(n)).sum::<usize>());
    }

    #[test]
    fn rewrite_target_component_on_multibyte_names() {
        let cases = [
            // A non-Markdown target: no extension convention to keep.
            ("Résumé—v2.docx", "Résumé—v3.docx", "Résumé—v3.docx"),
            ("Todo…ab", "Todo…ac", "Todo…ac"),
            ("日本x", "日本y", "日本y"),
            // A bare target takes the new basename, a full one the full stem.
            ("Plan…ab", "dé/日本/Plan…ac.md", "Plan…ac"),
            ("dé/日本/Plan…ab", "dé/日本/Plan…ac.md", "dé/日本/Plan…ac"),
            // The extension the user typed is kept as typed.
            ("Plan…ab.MD", "Plan…ac.md", "Plan…ac.MD"),
            ("Plan…ab.markdown", "日本/Plan…ac.md", "Plan…ac.markdown"),
            // Typed with no extension, renamed to a name that has one.
            ("Plan—", "Plan—2.md", "Plan—2"),
        ];
        for (target, new_path, expected) in cases {
            assert_eq!(
                rewrite_target_component(target, new_path, true),
                expected,
                "{target:?} -> {new_path:?}"
            );
        }
    }

    #[test]
    fn rewrite_markdown_raw_on_a_percent_encoded_multibyte_destination() {
        // `café—v2`, percent-encoded as an editor writes it.
        let encoded = "caf%C3%A9%E2%80%94v2";
        assert_eq!(percent_decode(encoded), "café—v2");
        let cases = [
            // (from, to_raw, new_path, decoded result)
            ("@notes:Journal.md", encoded, "café—v3", "café—v3"),
            ("@notes:Journal.md", encoded, "café—v3.docx", "café—v3.docx"),
            ("@notes:日本/Journal.md", encoded, "café—v3", "../café—v3"),
            (
                "@notes:日本/Journal.md",
                "caf%C3%A9%E2%80%94v2#^k3f9a2",
                "日本/café—v3",
                "café—v3#^k3f9a2",
            ),
            // With a Markdown extension the user typed, in either case.
            (
                "@notes:Journal.md",
                "caf%C3%A9%E2%80%94v2.md",
                "café—v3.md",
                "café—v3.md",
            ),
            (
                "@notes:Journal.md",
                "caf%C3%A9%E2%80%94v2.MD",
                "café—v3.md",
                "café—v3.MD",
            ),
            // Root-absolute.
            (
                "@notes:日本/Journal.md",
                "/caf%C3%A9%E2%80%94v2",
                "dé/café—v3",
                "/dé/café—v3",
            ),
        ];
        for (from, to_raw, new_path, expected) in cases {
            let out = rewrite_markdown_raw(from, to_raw, new_path)
                .unwrap_or_else(|| panic!("no rewrite for {to_raw:?}"));
            assert_eq!(
                percent_decode(&out),
                expected,
                "{from} {to_raw:?} -> {new_path:?}"
            );
        }
    }

    #[test]
    fn compute_changes_rewrites_a_non_markdown_multibyte_target() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:Résumé—v2.docx",
            "Résumé—v2.docx",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let content = "CV: [[Résumé—v2.docx]] and more.\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "Résumé—v2.docx", "Résumé—v3.docx");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "[[Résumé—v2.docx]]");
        assert_eq!(changes[0].after, "[[Résumé—v3.docx]]");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        assert_eq!(
            apply_changes_to_content(content, &refs).unwrap(),
            "CV: [[Résumé—v3.docx]] and more.\n"
        );
    }

    #[test]
    fn compute_changes_keeps_anchor_and_alias_on_a_bare_multibyte_target() {
        let edges = vec![edge(
            "@notes:Journal.md",
            "@notes:Todo…ab",
            "Todo…ab#^k3f9a2|plan",
            tephra_parser::LINK_KIND_WIKILINK,
        )];
        let content = "Start [[Todo…ab#^k3f9a2|plan]] end\n";
        let mut files = HashMap::new();
        files.insert("@notes:Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "Todo…ab", "Todo…ac");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].after, "[[Todo…ac#^k3f9a2|plan]]");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        assert_eq!(
            apply_changes_to_content(content, &refs).unwrap(),
            "Start [[Todo…ac#^k3f9a2|plan]] end\n"
        );
    }

    #[test]
    fn compute_changes_in_a_nested_multibyte_path_changes_link_text_only() {
        let edges = vec![
            edge(
                "@notes:dé/日本/Journal—.md",
                "@notes:dé/日本/Plan…ab.md",
                "dé/日本/Plan…ab",
                tephra_parser::LINK_KIND_WIKILINK,
            ),
            edge(
                "@notes:dé/日本/Journal—.md",
                "@notes:dé/日本/Plan…ab.md",
                "Plan…ab.MD|the plan",
                tephra_parser::LINK_KIND_WIKILINK,
            ),
        ];
        let content = "# Journal—\n\nplan…ab.md [[dé/日本/Plan…ab]] and [[Plan…ab.MD|the plan]]\n";
        let mut files = HashMap::new();
        files.insert(
            "@notes:dé/日本/Journal—.md".to_string(),
            content.to_string(),
        );
        let changes = compute_changes(&edges, &files, "dé/日本/Plan…ab.md", "dé/日本/Plan…ac.md");
        assert_eq!(changes.len(), 2, "{changes:?}");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        assert_eq!(
            apply_changes_to_content(content, &refs).unwrap(),
            "# Journal—\n\nplan…ab.md [[dé/日本/Plan…ac]] and [[Plan…ac.MD|the plan]]\n"
        );
    }

    #[test]
    fn compute_changes_rewrites_a_percent_encoded_multibyte_markdown_link() {
        let edges = vec![edge(
            "@notes:日本/Journal.md",
            "@notes:café—v2",
            "../caf%C3%A9%E2%80%94v2",
            tephra_parser::LINK_KIND_MARKDOWN,
        )];
        let content = "See [x](../caf%C3%A9%E2%80%94v2) now.\n";
        let mut files = HashMap::new();
        files.insert("@notes:日本/Journal.md".to_string(), content.to_string());
        let changes = compute_changes(&edges, &files, "café—v2", "café—v3");
        assert_eq!(changes.len(), 1, "{changes:?}");
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let result = apply_changes_to_content(content, &refs).expect("span matches");
        assert!(
            result.starts_with("See [x](") && result.ends_with(") now.\n"),
            "only the destination changes: {result:?}"
        );
        let reparsed = tephra_parser::parse_unit(Path::new("@notes:日本/Journal.md"), &result);
        assert_eq!(reparsed.markdown_links.len(), 1);
        assert_eq!(
            percent_decode(&reparsed.markdown_links[0].target),
            "../café—v3"
        );
    }

    // --- compute_fix_changes (the automatic fix's variant) ---------------

    /// One in-memory referring file for the fix tests below.
    fn one_file(id: &str, content: &str) -> HashMap<String, String> {
        HashMap::from([(id.to_string(), content.to_string())])
    }

    /// Applies `changes` to `content`, as the writer does.
    fn applied(content: &str, changes: &[LinkChange]) -> String {
        let refs: Vec<&LinkChange> = changes.iter().collect();
        apply_changes_to_content(content, &refs).expect("spans match")
    }

    #[test]
    fn fix_changes_are_per_kind_wikilinks_are_rewritten_and_markdown_links_are_not() {
        let from = "@notes:Ref.md";
        let content = "w [[Old]] m [t](Old.md) e ![[Old|a]]\n";
        let edges = vec![
            edge(from, "@notes:New.md", "Old", "wikilink"),
            edge(from, "@notes:New.md", "Old|a", "wikilink"),
            edge(from, "@notes:New.md", "Old.md", "markdown"),
        ];
        let files = one_file(from, content);

        let fix = compute_fix_changes(&edges, &files, "Old.md", "New.md", true);
        let kinds: Vec<(&str, &str)> = fix
            .iter()
            .map(|c| (c.before.as_str(), c.after.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![("[[Old]]", "[[New]]"), ("[[Old|a]]", "[[New|a]]")],
            "no markdown change, the embed's `!` is outside its span"
        );
        assert_eq!(
            applied(content, &fix),
            "w [[New]] m [t](Old.md) e ![[New|a]]\n"
        );

        // The manual refactor, with the same edges, still rewrites the
        // markdown link too.
        let manual = compute_changes(&edges, &files, "Old.md", "New.md");
        assert_eq!(manual.len(), 3, "{manual:?}");
        assert_eq!(
            applied(content, &manual),
            "w [[New]] m [t](New.md) e ![[New|a]]\n"
        );
    }

    #[test]
    fn fix_changes_write_the_full_stem_for_a_bare_link_only_when_the_bare_name_does_not_resolve() {
        let from = "@notes:Ref.md";
        let content = "[[Old]] [[Old.md]] [[@notes:Old#^k3f9a2]] [[dir/Old]]\n";
        let edges = vec![
            edge(from, "@notes:a/New.md", "Old", "wikilink"),
            edge(from, "@notes:a/New.md", "Old.md", "wikilink"),
            edge(from, "@notes:a/New.md", "@notes:Old#^k3f9a2", "wikilink"),
            edge(from, "@notes:a/New.md", "dir/Old", "wikilink"),
        ];
        let files = one_file(from, content);

        let bare = compute_fix_changes(&edges, &files, "dir/Old.md", "a/New.md", true);
        assert_eq!(
            applied(content, &bare),
            "[[New]] [[New.md]] [[@notes:New#^k3f9a2]] [[a/New]]\n",
            "a bare link stays bare, a path link gets the full stem"
        );
        let qualified = compute_fix_changes(&edges, &files, "dir/Old.md", "a/New.md", false);
        assert_eq!(
            applied(content, &qualified),
            "[[a/New]] [[a/New.md]] [[@notes:a/New#^k3f9a2]] [[a/New]]\n",
            "extension, prefix and anchor are kept, every target is the full stem"
        );
    }

    #[test]
    fn fix_changes_leave_a_nfc_target_nfc_when_the_new_path_is_nfd() {
        let from = "@notes:Ref.md";
        let content = "[[caf\u{e9} old]]\n";
        let edges = vec![edge(from, "@notes:x.md", "caf\u{e9} old", "wikilink")];
        let changes = compute_fix_changes(
            &edges,
            &one_file(from, content),
            "cafe\u{301} old.md",
            "cafe\u{301} new.md",
            true,
        );
        assert_eq!(applied(content, &changes), "[[caf\u{e9} new]]\n");
    }

    #[test]
    fn fix_changes_in_frontmatter_have_exact_spans() {
        let from = "@notes:Ref.md";
        let content = "---\nup: [[Old]]\nlist:\n  - \"[[Old|a]]\"\n# [[Old]]\n---\nbody\n";
        let edges = vec![
            edge(from, "@notes:New.md", "Old", "wikilink"),
            edge(from, "@notes:New.md", "Old|a", "wikilink"),
        ];
        let changes =
            compute_fix_changes(&edges, &one_file(from, content), "Old.md", "New.md", true);
        assert_eq!(changes.len(), 2, "{changes:?}");
        for c in &changes {
            assert_eq!(&content[c.byte_start..c.byte_end], c.before);
        }
        assert_eq!(
            applied(content, &changes),
            "---\nup: [[New]]\nlist:\n  - \"[[New|a]]\"\n# [[Old]]\n---\nbody\n",
            "the YAML comment is not a link"
        );
    }

    #[test]
    fn a_code_file_is_parsed_as_code_so_only_a_comment_link_gets_a_change() {
        let from = "@notes:tool.py";
        let content = "# see [[Old]]\nDECOY = \"[[Old]]\"\n";
        let edges = vec![edge(from, "@notes:New.md", "Old", "wikilink")];
        let changes = compute_changes(&edges, &one_file(from, content), "Old.md", "New.md");
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(
            applied(content, &changes),
            "# see [[New]]\nDECOY = \"[[Old]]\"\n"
        );
        // The same text in a prose file would find both.
        let prose = compute_changes(
            &[edge("@notes:tool.md", "@notes:New.md", "Old", "wikilink")],
            &one_file("@notes:tool.md", content),
            "Old.md",
            "New.md",
        );
        assert_eq!(prose.len(), 2);
    }

    #[test]
    fn a_shebang_script_with_no_extension_is_parsed_as_code() {
        let from = "@notes:bin/run";
        let content = "#!/usr/bin/env python3\n# see [[Old]]\nx = \"[[Old]]\"\n";
        let edges = vec![edge(from, "@notes:New.md", "Old", "wikilink")];
        let changes = compute_changes(&edges, &one_file(from, content), "Old.md", "New.md");
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].line, 2);
    }
}
