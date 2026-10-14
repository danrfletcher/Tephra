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
//! Does not implement link *resolution* (PR-5's `tephra_graph`) or
//! ambiguous-path tie-breaking (also PR-5) — this module only ever
//! rewrites a link whose `to_raw`/`kind` the graph already told it
//! resolves to the unit being renamed.

use std::collections::{HashMap, HashSet};
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
    /// The full matched link text before the rewrite (including its
    /// `[[`/`]]` or `(`/`)` delimiters).
    pub before: String,
    /// The full matched link text after the rewrite.
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
/// the old unit's federated id — see [`tephra_graph::UnitGraph::get_backlinks`],
/// which resolves a stale/renamed id's dangling edges too) and the
/// full current text content of every referring file named by those
/// edges' `from` id.
///
/// Pure: no filesystem, no SQLite, no RPC — every input is already an
/// in-memory value, which is what lets this be tested directly
/// against small hand-built fixtures (single-source match,
/// cross-source match, zero matches, identical old/new path,
/// case-only rename — see this module's tests below) rather than
/// only through a slower, real-filesystem integration test.
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
pub fn compute_changes(
    edges: &[Edge],
    file_contents: &HashMap<String, String>,
    old_path: &str,
    new_path: &str,
) -> Vec<LinkChange> {
    if old_path == new_path {
        return Vec::new();
    }
    let mut out = Vec::new();
    for edge in edges {
        let Some(content) = file_contents.get(&edge.from) else {
            continue;
        };
        let new_raw = if edge.kind.as_str() == tephra_parser::LINK_KIND_WIKILINK {
            rewrite_wikilink_raw(&edge.to_raw, new_path)
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
    let parsed = tephra_parser::parse_unit(Path::new(file_id), content);
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
                out.push(LinkChange {
                    file: file_id.to_string(),
                    line: m.line,
                    before: format!("({old_raw})"),
                    after: format!("({new_raw})"),
                    byte_start,
                    byte_end,
                });
            }
        }
    }
    out
}

/// Rewrites one wikilink's full raw inner text (`WikiLink::raw` /
/// `Edge::to_raw` for a wikilink-kind edge) so its target component
/// refers to `new_path` instead, leaving any `@source:` prefix,
/// `#anchor`, and `|alias` byte-for-byte unchanged. Returns `None` for
/// a same-file anchor-only link (`target` empty) — there is nothing
/// to rewrite, and this should not occur for an edge this function is
/// ever called with (such a link never resolves to a *different*
/// unit), but is handled rather than panicking.
fn rewrite_wikilink_raw(to_raw: &str, new_path: &str) -> Option<String> {
    let w = tephra_parser::parse_wikilink_inner(to_raw);
    if w.target.is_empty() {
        return None;
    }
    let new_target = rewrite_target_component(&w.target, new_path);
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
fn rewrite_target_component(target: &str, new_path: &str) -> String {
    let (_, ext) = strip_md_ext(target);
    let (new_stem, _) = strip_md_ext(new_path);
    let core = if target.contains('/') {
        new_stem.to_string()
    } else {
        basename(new_stem).to_string()
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
/// Preserves a trailing `#anchor` and round-trips percent-encoding
/// (re-encodes the replacement only if the original destination was
/// itself percent-encoded).
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
    let was_percent_encoded = before_anchor.contains('%');
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
    let mut new_target = match ext {
        Some(e) => format!("{core}{e}"),
        None => core,
    };
    if was_percent_encoded {
        new_target = percent_encode_path(&new_target);
    }
    let mut out = new_target;
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
fn strip_md_ext(p: &str) -> (&str, Option<&str>) {
    for ext in [".md", ".markdown"] {
        if p.len() > ext.len() {
            let idx = p.len() - ext.len();
            if p[idx..].eq_ignore_ascii_case(ext) {
                return (&p[..idx], Some(&p[idx..]));
            }
        }
    }
    (p, None)
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

impl WatcherPauseControl for SourceRegistry {
    fn pause_all(&self) {
        SourceRegistry::pause_all_watchers(self);
    }
    fn resume_all(&self, written_paths: &HashSet<PathBuf>) {
        SourceRegistry::resume_all_watchers_except(self, written_paths);
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
    apply_lock: Mutex<()>,
}

impl RefactorEngine {
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
            apply_lock: Mutex::new(()),
        }
    }

    fn source_roots(&self) -> Result<HashMap<String, String>, String> {
        Ok(self
            .sources
            .list()?
            .into_iter()
            .map(|r| (r.name, r.path))
            .collect())
    }

    fn require_source(&self, roots: &HashMap<String, String>, source: &str) -> Result<(), String> {
        if roots.contains_key(source) {
            Ok(())
        } else {
            Err(format!("no source named '{source}' is registered"))
        }
    }

    fn resolve_abs_path(&self, id: &str, roots: &HashMap<String, String>) -> Option<PathBuf> {
        let (source, rel_path) = split_federated_id(id)?;
        let root = roots.get(source)?;
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
    ) -> Result<Vec<LinkChange>, String> {
        let roots = self.source_roots()?;
        self.require_source(&roots, source)?;

        let old_id = format!("@{source}:{old_path}");
        let edges = self.graph.get_backlinks(&old_id);

        let mut mtimes = HashMap::new();
        let changes = if old_path == new_path {
            Vec::new()
        } else {
            let mut file_contents = HashMap::new();
            for edge in &edges {
                if file_contents.contains_key(&edge.from) {
                    continue;
                }
                let Some(abs) = self.resolve_abs_path(&edge.from, &roots) else {
                    continue;
                };
                let Ok(content) = fs::read_to_string(&abs) else {
                    continue;
                };
                // R7 review finding: a file whose mtime can't be
                // captured right now must not be included in this
                // preview at all. Recording it in `file_contents`
                // (hence proposing a change for it) without an mtime
                // snapshot would silently exempt it from
                // `verify_staleness`'s later re-check at `apply` time
                // — the exact TOCTOU gap that gate exists to close.
                // Treat a metadata/mtime failure exactly like an
                // unreadable file: skip it.
                let Ok(meta) = fs::metadata(&abs) else {
                    continue;
                };
                let Ok(mtime) = meta.modified() else {
                    continue;
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
        Ok(changes)
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
        }
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
        assert_eq!(changes[0].before, "(OldDoc.md)");
        assert_eq!(changes[0].after, "(NewDoc.md)");
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
        assert_eq!(changes[0].after, "(../archive/NewDoc.md)");
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
}
