//! Target resolution: turning a wikilink/markdown-link's raw text
//! into an [`crate::EdgeTarget`] (used while building/updating edges)
//! or a full [`ResolveResult`]/[`ResolveError`] (used by the public
//! [`crate::UnitGraph::resolve_link`] entry point, GP5). Both share
//! the same underlying in-source matching rule
//! ([`resolve_in_scope`]) — E4's tie-break and every other edge case
//! in this file apply identically whether the link in question is a
//! graph-internal edge or a directly-resolved query.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::model::{is_block_id, is_tephra_block_id, NodeFacts, UnitMetadata};
use crate::EdgeTarget;

/// The full result of a successful [`crate::UnitGraph::resolve_link`]
/// call: the resolved unit's metadata plus its own outgoing edges —
/// the exact shape GP5 freezes for PR-6's RPC handler to return
/// unmodified — and, for a block link, the block it points at.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolveResult {
    pub unit: UnitMetadata,
    pub edges: Vec<crate::Edge>,
    /// The block a `[[path#^id]]` link resolved to (G13), `null` for
    /// every other link: a link with no anchor, a heading anchor
    /// (`#Heading`) or a code symbol's name (`#parse_config`, F3).
    pub block: Option<BlockRef>,
}

/// The block a block link resolved to: its ID (without the caret),
/// its line (1-based; for a code block the definition's first line,
/// the one that carries the ID comment) and whether it was found in
/// another file than the one the link names (`moved`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockRef {
    pub id: String,
    pub line: i64,
    pub moved: bool,
}

/// One occurrence of a block ID in a unit, as an ambiguity error
/// lists it: the unit's federated id and the line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct BlockLine {
    pub unit: String,
    pub line: i64,
}

/// Why [`crate::UnitGraph::resolve_link`] failed. Distinct from
/// [`EdgeTarget::Dangling`] (used internally for an already-built
/// edge) because a direct `resolve_link` call needs to report *why*,
/// not just *that*, resolution failed.
///
/// Serialized adjacently-tagged (`{"error": "...", "data": ...}`) —
/// the one `#[serde]` representation that supports a mix of tuple,
/// struct, and unit variants without reshaping any of them — so GP5's
/// frozen wire contract is explicit rather than left to serde's
/// default externally-tagged guess. See
/// `tests/integration/gp5_contract.rs` for the exact JSON this
/// produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "error", content = "data", rename_all = "snake_case")]
pub enum ResolveError {
    /// The link's `@source` prefix (or, when none was given, the
    /// caller-supplied default source) names a source this graph has
    /// never registered — never a panic, never a silent empty
    /// result.
    UnknownSource(String),
    /// Zero units in `source` match `target`. Distinguishable in type
    /// from [`ResolveError::Ambiguous`]: this is "nothing matched",
    /// not "more than one thing matched".
    Unresolved { source: String, target: String },
    /// Two or more units in `source` tied on the shortest matching
    /// path for `target` (E4): every tied candidate's federated id,
    /// sorted, rather than guessing one.
    ///
    /// For a block link (`[[path#^id]]`, E7/E8) `candidates` are the
    /// units that carry the ID (a unit once, however often it repeats
    /// the ID) and `lines` lists every occurrence, unit and line,
    /// sorted. `lines` is empty (and not serialized) for an ambiguous
    /// file name.
    Ambiguous {
        source: String,
        target: String,
        candidates: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        lines: Vec<BlockLine>,
    },
    /// The link has no `@source` prefix (a same-source-relative
    /// target) and the caller gave no `default_source` to resolve it
    /// against. Only reachable via the public API: every edge the
    /// graph itself resolves while building/updating always has a
    /// `default_source` (the referring unit's own source).
    NoSourceContext,
    /// The raw text, once an optional surrounding `[[`/`]]` is
    /// stripped, has an empty target and no originating unit to
    /// anchor a same-file reference to (e.g. `resolve_link("[[#Heading]]",
    /// None)`) — [`crate::UnitGraph::resolve_link`] has no "current
    /// file" concept, unlike the graph's own internal edge-building,
    /// which resolves an empty target to the referring unit itself.
    EmptyTarget,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnknownSource(source) => {
                write!(f, "no source named \"{source}\" is registered")
            }
            ResolveError::Unresolved { source, target } => {
                // A block link's target is `file#^id` (see
                // `resolve_link_impl`): the file may well exist, so say
                // the block is what is missing. Only a Tephra-format ID
                // is searched for in other files (G13), so only then
                // does the message say "or elsewhere".
                match target.rsplit_once("#^") {
                    Some((file, id)) if is_tephra_block_id(id) => write!(
                        f,
                        "no block ^{id} in \"{file}\" or elsewhere in source \"{source}\""
                    ),
                    Some((file, id)) if is_block_id(id) => {
                        write!(f, "no block ^{id} in \"{file}\" in source \"{source}\"")
                    }
                    _ => write!(f, "no unit in source \"{source}\" matches \"{target}\""),
                }
            }
            ResolveError::Ambiguous {
                source,
                target,
                candidates,
                lines,
            } => {
                write!(
                    f,
                    "\"{target}\" is ambiguous in source \"{source}\": tied candidates {candidates:?}"
                )?;
                if !lines.is_empty() {
                    let at: Vec<String> = lines
                        .iter()
                        .map(|l| format!("{}:{}", l.unit, l.line))
                        .collect();
                    write!(f, " (at {})", at.join(", "))?;
                }
                Ok(())
            }
            ResolveError::NoSourceContext => {
                write!(
                    f,
                    "link has no @source prefix and no default_source was given"
                )
            }
            ResolveError::EmptyTarget => {
                write!(
                    f,
                    "link target is empty and there is no originating unit to anchor it to"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// The outcome of matching `target` against every unit in one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveOutcome {
    Unique(String),
    Ambiguous(Vec<String>),
    NotFound,
}

/// File extensions [`strip_markdown_extension`] strips. Only Markdown
/// names are matched without their extension: a bare `[[Foo]]` finds
/// `Foo.md`. The graph also holds units of other file types (a `.txt`,
/// `.org`, a `.py`), which are matched by their full name; making a
/// bare name reach those is PR-13's job, not this module's.
const MARKDOWN_EXTENSIONS: [&str; 2] = [".md", ".markdown"];

/// Strips a trailing `.md`/`.markdown` (case-insensitive) from
/// `rel_path`, if present; returns `rel_path` unchanged otherwise.
/// Never panics, whatever the string: the extensions are ASCII, so a
/// name that ends in one has a char boundary exactly `ext.len()` bytes
/// from its end. When that offset falls inside a multi-byte character
/// (`Todo…ab`, `Résumé—v2.docx`) the name cannot end in the extension
/// and is returned unchanged.
/// Applied to both sides of every match (R1): a stored unit's own
/// path *and* an incoming link's target, since a standard markdown
/// link (`[x](B.md)`) or an extension-qualified wikilink (`[[B.md]]`)
/// both name the file including its extension, unlike the bare-name
/// form (`[[B]]`) most wikilinks use.
pub(crate) fn strip_markdown_extension(rel_path: &str) -> &str {
    for ext in MARKDOWN_EXTENSIONS {
        if rel_path.len() > ext.len() {
            let cut = rel_path.len() - ext.len();
            if !rel_path.is_char_boundary(cut) {
                continue;
            }
            let (base, suffix) = rel_path.split_at(cut);
            if suffix.eq_ignore_ascii_case(ext) {
                return base;
            }
        }
    }
    rel_path
}

/// Normalizes `s` to Unicode NFC (canonical composition) — applied to
/// every stored stem and every incoming link target before comparing
/// them (R6), so a file whose name is stored NFD-normalized on disk
/// (common on macOS/HFS+/APFS) still matches an NFC-typed `[[café]]`
/// link rather than silently failing to resolve over a byte-identical
/// but differently-normalized string. A deliberate, tested policy
/// rather than the byte-exact comparison this crate started with.
pub(crate) fn normalize_key(s: &str) -> String {
    s.nfc().collect()
}

/// The key under which the unit at `rel_path` is filed in
/// [`crate::UnitGraph::basename_index`]: its file name, without a
/// Markdown extension, in NFC.
pub(crate) fn bare_name_key(rel_path: &str) -> String {
    normalize_key(basename_of(strip_markdown_extension(rel_path)))
}

/// `target`'s normalized full-stem match key (NFC, extension
/// stripped) — used for both a wikilink's `target` field and, after
/// percent-decoding and relative-path resolution, a markdown link's
/// destination. The one key [`resolve_in_scope`] compares against
/// [`crate::UnitGraph::stem_index`]/[`crate::UnitGraph::basename_index`].
pub(crate) fn normalize_wikilink_target(target: &str) -> String {
    normalize_key(strip_markdown_extension(target))
}

/// Percent-decodes `s` (`%XX` escapes), falling back to the original
/// text unchanged if the decoded bytes aren't valid UTF-8 (never
/// panics, never silently drops or replaces characters) — a markdown
/// link to a non-ASCII or space-containing path is commonly
/// percent-encoded by the editor that wrote it (R1:
/// `caf%C3%A9%20notes.md`), unlike a wikilink target, which this
/// parser's grammar already stores decoded. Deliberately uses
/// `decode_utf8` (strict), not `decode_utf8_lossy` (R15): lossy
/// decoding would replace invalid bytes with U+FFFD, silently
/// changing the text rather than leaving it unchanged as documented.
fn percent_decode(s: &str) -> Cow<'_, str> {
    match percent_encoding::percent_decode_str(s).decode_utf8() {
        Ok(decoded) => decoded,
        Err(_) => Cow::Borrowed(s),
    }
}

/// Joins `target` onto `from_dir` (the directory containing the
/// referring unit, `""` for a unit at the source root), resolving
/// `.`/`..` components the way a browser or filesystem would for a
/// relative link — so a markdown link is resolved relative to the
/// *linking file's* directory (R1), not the source root, matching
/// standard relative-markdown-link semantics. Pure string/`/`-split
/// arithmetic, independent of the host OS's path separator, since
/// every `rel_path` in this crate already uses `/` (see
/// `tests/fixtures/mod.rs`).
///
/// Two cases anchor the result at the source root rather than
/// `from_dir`, and both must give the same answer regardless of how
/// deep the linking file sits (R22, the same class of depth-dependent
/// bug R21 fixed for bare basenames):
///
/// - A target starting with `/` (a root-absolute path, standard in
///   GitHub-style READMEs such as the `atlas-repo` code source) is
///   resolved from the source root, never from `from_dir` — so
///   `/docs/X.md` means `docs/X.md` from *every* linking file, not
///   `sub/docs/X.md` when the link happens to live in `sub/`.
/// - A `..` that would pop past the source root returns `None` (so
///   the caller dangles the edge) instead of silently stopping at the
///   root: the target names a path outside the source entirely, and
///   it must never be treated as if it named some unrelated file that
///   happens to share the remaining components.
fn join_relative(from_dir: &str, target: &str) -> Option<String> {
    let (mut stack, remainder): (Vec<&str>, &str) = match target.strip_prefix('/') {
        Some(rest) => (Vec::new(), rest),
        None => {
            let base: Vec<&str> = if from_dir.is_empty() {
                Vec::new()
            } else {
                from_dir.split('/').filter(|s| !s.is_empty()).collect()
            };
            (base, target)
        }
    };
    for component in remainder.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    Some(stack.join("/"))
}

/// A markdown link's (`[text](target)`) normalized match key: the
/// `#anchor` suffix stripped, percent-decoded, resolved relative to
/// `from_rel_path`'s own directory (a leading `./` included — joining
/// against a directory already absorbs it), then NFC-normalized with
/// its markdown extension stripped — the exact form compared against
/// [`crate::UnitGraph::stem_index`] (R1/R6). Only ever looked up
/// against `stem_index` (R21), never `basename_index`, regardless of
/// whether the result contains a `/` — see
/// [`markdown_target_is_self_reference`] for the case where this
/// would otherwise be empty, and [`resolve_in_scope`]'s
/// `allow_basename` parameter for why a markdown link never falls
/// back to a basename-anywhere-in-source match the way a bare
/// wikilink does.
///
/// Returns `None` when the target names a path outside the source
/// (a `..` that climbs past the source root, R22) — the caller must
/// then dangle the edge rather than index it under a bogus,
/// depth-dependent key.
pub(crate) fn normalize_markdown_target(from_rel_path: &str, raw_target: &str) -> Option<String> {
    let without_anchor = raw_target.split('#').next().unwrap_or(raw_target);
    let decoded = percent_decode(without_anchor);
    let from_dir = from_rel_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let joined = join_relative(from_dir, &decoded)?;
    Some(normalize_key(strip_markdown_extension(&joined)))
}

/// Whether a markdown link's raw target names no file at all once any
/// `#anchor` suffix is removed and it is percent-decoded — a
/// same-file anchor link like `[x](#heading)` (R20), the markdown
/// equivalent of a wikilink's empty-target self-reference
/// (`[[#Heading]]`). Checked on the raw target directly, *before*
/// [`join_relative`] ever runs: joining an empty remainder onto the
/// linking file's own directory previously produced a bogus
/// non-empty key (`""` at the source root, or the directory's own
/// name — e.g. `"sub"` — for a file inside a subdirectory), so the
/// bug's exact shape depended on how deep the linking file was.
pub(crate) fn markdown_target_is_self_reference(raw_target: &str) -> bool {
    let without_anchor = raw_target.split('#').next().unwrap_or(raw_target);
    percent_decode(without_anchor).is_empty()
}

/// The final path component of a normalized stem/target key — the
/// basename a bare (no `/`) link target matches against regardless of
/// which directory the target unit actually lives in.
pub(crate) fn basename_of(stem: &str) -> &str {
    stem.rsplit('/').next().unwrap_or(stem)
}

/// Bundles the three pieces of [`crate::UnitGraph`] state that
/// resolution reads but never mutates (`nodes`, `stem_index`,
/// `basename_index`), so [`resolve_in_scope`] and
/// [`resolve_edge_target`] take one borrow instead of three separate
/// parameters each — kept under clippy's `too_many_arguments` limit
/// without losing the "plain function over borrowed data" shape the
/// rest of this module uses. Deliberately does *not* carry
/// `known_sources` (R18): whether a scope is a currently-registered
/// source must never gate whether an edge gets a match key at all —
/// see [`edge_target_scope_key`].
pub(crate) struct GraphIndex<'a> {
    pub nodes: &'a HashMap<String, NodeFacts>,
    pub stem_index: &'a HashMap<String, HashMap<String, Vec<String>>>,
    pub basename_index: &'a HashMap<String, HashMap<String, Vec<String>>>,
}

/// Resolves `target` (already normalized — NFC, extension stripped,
/// and for a markdown link, percent-decoded and joined relative to
/// its own directory) against every unit belonging to `scope_source`
/// only (R4/"no cross-source leakage": a caller must pick the scope
/// before calling this — it never searches outside the one source
/// given).
///
/// Candidate gathering goes through `stem_index`/`basename_index`
/// (O(1) average lookups) rather than scanning every unit in the
/// source (R3) — the quadratic-update bug's root cause, since every
/// edge in the graph used to pay an O(units-in-source) cost on every
/// rebuild. Matching rule (E4, a judgement call where the spec is
/// silent on the exact shape): a candidate matches if its normalized
/// stem equals `target` exactly, or — when `allow_basename` is `true`
/// *and* `target` itself contains no `/` — its basename equals
/// `target`. Among every match, the one(s) with the shortest matched
/// path (by `char` count) win; a tie among the shortest is reported
/// as [`ResolveOutcome::Ambiguous`] with every tied id, sorted,
/// rather than guessing one.
///
/// `allow_basename` is `false` for every markdown-link lookup (R21):
/// a markdown link's key is always an exact relative path from the
/// source root (root-level targets simply have no `/` in that path),
/// so falling back to `basename_index` for one would silently match
/// some unrelated same-named file anywhere else in the source,
/// exactly when the linking file happens to sit at the source root
/// or climbs back to it with `../` — a "link to a path that doesn't
/// exist must dangle" violation, not an exact-path lookup giving the
/// wrong depth-dependent answer. It is `true` only for a wikilink's
/// bare (no `@source:`-prefix-stripped) target, where a basename
/// match anywhere in the source is the documented, tested rule.
pub(crate) fn resolve_in_scope(
    index: &GraphIndex<'_>,
    scope_source: &str,
    target: &str,
    allow_basename: bool,
) -> ResolveOutcome {
    let candidate_ids: &[String] = if !allow_basename || target.contains('/') {
        index
            .stem_index
            .get(scope_source)
            .and_then(|m| m.get(target))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    } else {
        index
            .basename_index
            .get(scope_source)
            .and_then(|m| m.get(target))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    };

    let mut matches: Vec<(String, usize)> = Vec::new();
    for id in candidate_ids {
        if let Some(facts) = index.nodes.get(id) {
            let stem = normalize_key(strip_markdown_extension(&facts.rel_path));
            matches.push((id.clone(), stem.chars().count()));
        }
    }

    if matches.is_empty() {
        return ResolveOutcome::NotFound;
    }

    let min_len = matches
        .iter()
        .map(|(_, len)| *len)
        .min()
        .expect("non-empty");
    let mut tied: Vec<String> = matches
        .into_iter()
        .filter(|(_, len)| *len == min_len)
        .map(|(id, _)| id)
        .collect();
    tied.sort();

    if tied.len() == 1 {
        ResolveOutcome::Unique(tied.remove(0))
    } else {
        ResolveOutcome::Ambiguous(tied)
    }
}

/// The scope source and normalized match key `to_raw`/`kind` would be
/// resolved against, or `None` when there is no sensible key to index
/// — a same-file anchor-only wikilink (`[[#Heading]]`, always
/// resolves to the referring unit itself, never dangling) or a
/// markdown link that is an absolute URL (never names a vault/repo
/// unit). Shared by [`resolve_edge_target`] (to actually resolve the
/// edge) and by `crate::ingest`'s dangling-edge indexing (R4/GP7): a
/// dangling edge is indexed under exactly the key that would make it
/// resolve, so a later-added/renamed unit whose own stem or basename
/// equals that key can be found in O(1) instead of rescanning every
/// edge in the graph.
///
/// Returns a key for a wikilink's explicit `@source:` scope even
/// when that source is not currently registered at all (R18): a
/// federated link to a source that hasn't been added yet (or has
/// been added but has no units yet) must still land in
/// [`crate::UnitGraph::match_index`] under `(scope, key)`, so that
/// once the source is registered and populated, the touched-id scan
/// in [`crate::ingest::UnitGraph::recompute_scoped`] finds and
/// re-resolves it the same way it would any other dangling edge —
/// rather than the edge being invisible to every future re-resolution
/// because it was never indexed under any key at all. Whether the
/// scope is actually known only matters to [`resolve_in_scope`]'s own
/// lookup (an unregistered source's `stem_index`/`basename_index`
/// entry is simply absent, so resolution still correctly yields
/// [`ResolveOutcome::NotFound`] and the edge stays dangling until a
/// matching unit actually shows up).
pub(crate) fn edge_target_scope_key(
    index: &GraphIndex<'_>,
    from_id: &str,
    from_source: &str,
    to_raw: &str,
    kind: &str,
) -> Option<(String, String)> {
    if kind == tephra_parser::LINK_KIND_WIKILINK {
        let wikilink = tephra_parser::parse_wikilink_inner(to_raw);
        if wikilink.target.is_empty() {
            return None;
        }
        let scope = wikilink
            .source_prefix
            .as_deref()
            .unwrap_or(from_source)
            .to_string();
        Some((scope, normalize_wikilink_target(&wikilink.target)))
    } else {
        // Markdown-style `[text](target)` link: no `@source` prefix
        // grammar, resolved only within the referring unit's own
        // source. An absolute URL (contains "://") never names a
        // vault/repo unit, so it is never worth indexing. A target
        // that is empty once its `#anchor` is stripped (R20) is a
        // same-file anchor link, resolved below in
        // `resolve_edge_target` without a match key at all — just
        // like a wikilink's empty-target self-reference — so it must
        // not get indexed under a bogus key here either.
        if to_raw.contains("://") || markdown_target_is_self_reference(to_raw) {
            return None;
        }
        let from_rel_path = index
            .nodes
            .get(from_id)
            .map(|f| f.rel_path.as_str())
            .unwrap_or("");
        let key = normalize_markdown_target(from_rel_path, to_raw)?;
        Some((from_source.to_string(), key))
    }
}

/// Resolves one outgoing link (`to_raw`, `kind`) authored by
/// `from_id` (belonging to `from_source`) into an [`EdgeTarget`],
/// used while (re)building the graph's edge layer. Never returns an
/// error — a link that can't be resolved for any reason (unknown
/// source prefix, zero matches, an ambiguous tie, an absolute URL)
/// becomes [`EdgeTarget::Dangling`] (E4/"never silently resolved as
/// if current"), since an edge, unlike a direct `resolve_link` call,
/// has nowhere to surface a typed error to.
pub(crate) fn resolve_edge_target(
    index: &GraphIndex<'_>,
    from_id: &str,
    from_source: &str,
    to_raw: &str,
    kind: &str,
) -> EdgeTarget {
    if kind == tephra_parser::LINK_KIND_WIKILINK {
        let wikilink = tephra_parser::parse_wikilink_inner(to_raw);
        if wikilink.target.is_empty() {
            // A same-file anchor-only link, e.g. `[[#Heading]]` or a
            // block-anchor variant: always resolves to the referring
            // unit itself (the "block linking to its own heading"
            // self-reference edge case) — no search needed.
            return EdgeTarget::Resolved(from_id.to_string());
        }
    } else if markdown_target_is_self_reference(to_raw) {
        // The markdown equivalent of the wikilink case just above
        // (R20): `[x](#heading)` names no file, only an anchor within
        // the linking file itself, so it always resolves to the
        // referring unit regardless of that unit's own directory
        // depth — checked here, before `edge_target_scope_key` would
        // otherwise join the empty remainder onto `from_dir` and
        // produce a bogus non-empty key.
        return EdgeTarget::Resolved(from_id.to_string());
    }
    match edge_target_scope_key(index, from_id, from_source, to_raw, kind) {
        None => EdgeTarget::Dangling,
        Some((scope, key)) => {
            let allow_basename = kind == tephra_parser::LINK_KIND_WIKILINK;
            match resolve_in_scope(index, &scope, &key, allow_basename) {
                ResolveOutcome::Unique(id) => EdgeTarget::Resolved(id),
                ResolveOutcome::Ambiguous(_) | ResolveOutcome::NotFound => EdgeTarget::Dangling,
            }
        }
    }
}

/// The shortest wikilink target that resolves to the unit `id`, written
/// the way a user would type it (no extension for a Markdown file, the
/// file's own case): the bare name when [`resolve_in_scope`] maps it to
/// `id`, otherwise the full source-relative stem. Nothing in between is
/// tried: the resolver matches a bare name or a full stem and does no
/// suffix matching. `None` when `id` is not in the graph.
///
/// Every target this returns resolves, through [`resolve_in_scope`] in
/// the unit's own source, to `id` (checked by the round trip in
/// `tests/unit/resolver.rs`). A full stem that two units share (`Plan.md`
/// and `Plan.markdown` in one folder) cannot be told apart by any link
/// text, so for those the stem is returned and does not resolve uniquely.
pub(crate) fn shortest_unique_target(graph: &crate::UnitGraph, id: &str) -> Option<String> {
    let facts = graph.nodes.get(id)?;
    let stem = strip_markdown_extension(&facts.rel_path);
    let base = basename_of(stem);
    let key = normalize_key(base);
    match resolve_in_scope(&graph.index(), &facts.source, &key, true) {
        ResolveOutcome::Unique(found) if found == id => Some(base.to_string()),
        _ => Some(stem.to_string()),
    }
}

/// Whether a wikilink whose normalized match key is `key` (in scope
/// `source`) resolved to the file at `old_rel_path` *before* it was
/// renamed to `new_id`, judged from the graph as it is after the rename.
///
/// It replays [`resolve_in_scope`]'s rule on the pre-rename file set:
/// the candidates now indexed under `key` (a full stem when `key`
/// contains `/`, else a basename), without `new_id` (that is the old
/// file under its new name), plus the old file when its stem (or
/// basename) is `key`. The old file won when it has the one shortest
/// stem; a tie with another file means the link was ambiguous and never
/// resolved to it. The old file's own id is left out of the candidates
/// as well as `new_id`, so a graph that still holds it gives the same
/// answer as one that does not.
pub(crate) fn old_path_won_before_rename(
    graph: &crate::UnitGraph,
    source: &str,
    old_rel_path: &str,
    new_id: &str,
    key: &str,
) -> bool {
    let old_stem = normalize_key(strip_markdown_extension(old_rel_path));
    let by_stem = key.contains('/');
    let old_matches = if by_stem {
        old_stem == key
    } else {
        basename_of(&old_stem) == key
    };
    if !old_matches {
        return false;
    }
    let index = if by_stem {
        &graph.stem_index
    } else {
        &graph.basename_index
    };
    let old_len = old_stem.chars().count();
    let others = index
        .get(source)
        .and_then(|m| m.get(key))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let old_id = format!("@{source}:{old_rel_path}");
    others
        .iter()
        .filter(|id| id.as_str() != new_id && id.as_str() != old_id)
        .filter_map(|id| graph.nodes.get(id))
        .all(|facts| {
            normalize_key(strip_markdown_extension(&facts.rel_path))
                .chars()
                .count()
                > old_len
        })
}

/// Strips one optional surrounding `[[`/`]]` pair from `raw`, so
/// [`crate::UnitGraph::resolve_link`] accepts both the full wikilink
/// syntax GP5's example uses (`"[[@atlas-repo:README]]"`) and the bare
/// inner text (`"@atlas-repo:README"`) interchangeably.
pub(crate) fn strip_brackets(raw: &str) -> &str {
    let trimmed = raw.trim();
    match trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        Some(inner) => inner,
        None => trimmed,
    }
}

/// Where a block link looks for its block first.
pub(crate) enum FileRef<'a> {
    /// The unit that holds the link itself (`[[#^id]]`, `[x](#^id)`).
    Unit(&'a str),
    /// A normalized match key, resolved by [`resolve_in_scope`].
    Key { key: &'a str, allow_basename: bool },
}

/// A block found by [`resolve_block`]: the unit that holds it and the
/// block itself.
pub(crate) struct BlockHit {
    pub unit: String,
    pub block: BlockRef,
}

/// Why [`resolve_block`] found no one block.
pub(crate) enum BlockMiss {
    /// The file the link names is itself ambiguous (E4): the tied
    /// candidates, sorted.
    FileAmbiguous(Vec<String>),
    /// The ID is in more than one place (E7/E8): the units that carry
    /// it and every occurrence, both sorted.
    Ambiguous {
        candidates: Vec<String>,
        lines: Vec<BlockLine>,
    },
    /// Not in the named file, and no other file in the source may
    /// stand in for it: dangling.
    Unresolved,
}

/// The ID of the block a link points at: the part of the link's anchor
/// after the caret, for a `#^id` anchor (a wikilink's, or a Markdown
/// link's `(path#^id)`). `None` for a link with no anchor, a heading
/// anchor, or an anchor that is not a block ID (`#^`, `#^a b`): such a
/// link is a link to the file, as it always was.
pub(crate) fn edge_block_anchor(to_raw: &str, kind: &str) -> Option<String> {
    let anchor: Option<String> = if kind == tephra_parser::LINK_KIND_WIKILINK {
        tephra_parser::parse_wikilink_inner(to_raw).anchor
    } else {
        to_raw.split_once('#').map(|(_, a)| a.to_string())
    };
    let id = anchor?.strip_prefix('^')?.to_string();
    is_block_id(&id).then_some(id)
}

/// Resolves block `id` (G13) for a link that names `file` in
/// `scope`:
///
/// 1. In the named file, if it exists: exactly one occurrence is the
///    block (`moved: false`); two or more are an ambiguity listing
///    every line (E7), never the first one.
/// 2. Not in the named file (or the file is gone), and `id` is a
///    Tephra-format ID: the one other file of the *same source* that
///    has it (`moved: true`). Two or more files, or one file with the
///    ID twice, are an ambiguity listing every occurrence (E8);
///    none is dangling. The search never leaves `scope` (F7).
/// 3. Any other ID (a typed one such as `^summary`) missing from the
///    named file is dangling, whatever other files have it.
///
/// A file name that is ambiguous in itself is
/// [`BlockMiss::FileAmbiguous`] before any block is looked at.
pub(crate) fn resolve_block(
    graph: &crate::UnitGraph,
    scope: &str,
    file: FileRef<'_>,
    id: &str,
) -> Result<BlockHit, BlockMiss> {
    let file_id: Option<String> = match file {
        FileRef::Unit(unit) => Some(unit.to_string()),
        FileRef::Key {
            key,
            allow_basename,
        } => match resolve_in_scope(&graph.index(), scope, key, allow_basename) {
            ResolveOutcome::Unique(unit) => Some(unit),
            ResolveOutcome::Ambiguous(candidates) => {
                return Err(BlockMiss::FileAmbiguous(candidates))
            }
            ResolveOutcome::NotFound => None,
        },
    };

    let occurrences_in = |unit: &str| -> Vec<BlockLine> {
        graph
            .nodes
            .get(unit)
            .map(|n| {
                n.blocks
                    .iter()
                    .filter(|b| b.id == id)
                    .map(|b| BlockLine {
                        unit: unit.to_string(),
                        line: b.line,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    if let Some(unit) = &file_id {
        let mut here = occurrences_in(unit);
        match here.len() {
            0 => {}
            1 => {
                let at = here.remove(0);
                return Ok(BlockHit {
                    unit: at.unit,
                    block: BlockRef {
                        id: id.to_string(),
                        line: at.line,
                        moved: false,
                    },
                });
            }
            _ => return Err(ambiguous_block(here)),
        }
    }

    if !is_tephra_block_id(id) {
        return Err(BlockMiss::Unresolved);
    }
    let mut elsewhere: Vec<BlockLine> = graph
        .block_index
        .get(scope)
        .and_then(|by_id| by_id.get(id))
        .map(|units| {
            units
                .iter()
                .filter(|u| Some(*u) != file_id.as_ref())
                .flat_map(|u| occurrences_in(u))
                .collect()
        })
        .unwrap_or_default();
    match elsewhere.len() {
        0 => Err(BlockMiss::Unresolved),
        1 => {
            let at = elsewhere.remove(0);
            Ok(BlockHit {
                unit: at.unit,
                block: BlockRef {
                    id: id.to_string(),
                    line: at.line,
                    moved: true,
                },
            })
        }
        _ => Err(ambiguous_block(elsewhere)),
    }
}

/// The [`BlockMiss::Ambiguous`] for these occurrences: sorted, with
/// the units that carry them listed once each.
fn ambiguous_block(mut lines: Vec<BlockLine>) -> BlockMiss {
    lines.sort();
    let mut candidates: Vec<String> = lines.iter().map(|l| l.unit.clone()).collect();
    candidates.dedup();
    BlockMiss::Ambiguous { candidates, lines }
}

/// The block `edge` points at, for an edge whose link anchor is a
/// block ID (`None` for any other edge, and for one whose ID is
/// dangling or ambiguous): [`resolve_block`] from the link's own
/// source and the file it names. What
/// [`crate::UnitGraph::get_backlinks`] asks to find the links to one
/// block, including a link written to the block's old file.
pub(crate) fn resolve_edge_block(graph: &crate::UnitGraph, edge: &crate::Edge) -> Option<BlockHit> {
    let from = graph.nodes.get(&edge.from)?;
    let id = edge_block_anchor(&edge.to_raw, &edge.kind)?;
    let result = if edge.kind == tephra_parser::LINK_KIND_WIKILINK {
        let wikilink = tephra_parser::parse_wikilink_inner(&edge.to_raw);
        if wikilink.target.is_empty() {
            let scope = wikilink.source_prefix.as_deref().unwrap_or(&from.source);
            resolve_block(graph, scope, FileRef::Unit(&edge.from), &id)
        } else {
            let scope = wikilink.source_prefix.as_deref().unwrap_or(&from.source);
            let key = normalize_wikilink_target(&wikilink.target);
            let file = FileRef::Key {
                key: &key,
                allow_basename: true,
            };
            resolve_block(graph, scope, file, &id)
        }
    } else if markdown_target_is_self_reference(&edge.to_raw) {
        resolve_block(graph, &from.source, FileRef::Unit(&edge.from), &id)
    } else {
        let key = normalize_markdown_target(&from.rel_path, &edge.to_raw)?;
        let file = FileRef::Key {
            key: &key,
            allow_basename: false,
        };
        resolve_block(graph, &from.source, file, &id)
    };
    result.ok()
}

/// `unit.resolve_link` (G4/GP5): resolves `raw` (a full `[[...]]`
/// wikilink or its bare inner text) against this graph.
/// `default_source` is the source to resolve a non-federated
/// (`@source`-prefix-less) target against; ignored when `raw` itself
/// carries a `@source:` prefix, in which case that source is used
/// instead (and must be a known source, E \"unknown @source prefix\").
///
/// A block link (`[[path#^id]]`) resolves to the block ([`resolve_block`],
/// G13) and reports it in [`ResolveResult::block`]. Any other anchor
/// (a heading, a symbol name — F3) is not looked at: the link
/// resolves to the file and `block` is `None`.
pub(crate) fn resolve_link_impl(
    graph: &crate::UnitGraph,
    raw: &str,
    default_source: Option<&str>,
) -> Result<ResolveResult, ResolveError> {
    let inner = strip_brackets(raw);
    let wikilink = tephra_parser::parse_wikilink_inner(inner);

    if wikilink.target.is_empty() {
        return Err(ResolveError::EmptyTarget);
    }

    let scope: String = match (&wikilink.source_prefix, default_source) {
        (Some(prefix), _) => prefix.clone(),
        (None, Some(source)) => source.to_string(),
        (None, None) => return Err(ResolveError::NoSourceContext),
    };

    if !graph.known_sources.contains(&scope) {
        return Err(ResolveError::UnknownSource(scope));
    }

    let key = normalize_wikilink_target(&wikilink.target);

    if let Some(block_id) = edge_block_anchor(inner, tephra_parser::LINK_KIND_WIKILINK) {
        let file = FileRef::Key {
            key: &key,
            allow_basename: true,
        };
        let block_target = format!("{}#^{}", wikilink.target, block_id);
        return match resolve_block(graph, &scope, file, &block_id) {
            Ok(hit) => {
                let unit = graph
                    .unit(&hit.unit)
                    .expect("a resolved block's unit is in the graph");
                let edges = graph.get_forward_links(&hit.unit);
                Ok(ResolveResult {
                    unit,
                    edges,
                    block: Some(hit.block),
                })
            }
            Err(BlockMiss::FileAmbiguous(candidates)) => Err(ResolveError::Ambiguous {
                source: scope,
                target: wikilink.target.clone(),
                candidates,
                lines: Vec::new(),
            }),
            Err(BlockMiss::Ambiguous { candidates, lines }) => Err(ResolveError::Ambiguous {
                source: scope,
                target: block_target,
                candidates,
                lines,
            }),
            Err(BlockMiss::Unresolved) => Err(ResolveError::Unresolved {
                source: scope,
                target: block_target,
            }),
        };
    }

    let index = graph.index();
    match resolve_in_scope(&index, &scope, &key, true) {
        ResolveOutcome::Unique(id) => {
            let unit = graph
                .unit(&id)
                .expect("a resolved id always has a corresponding node");
            let edges = graph.get_forward_links(&id);
            Ok(ResolveResult {
                unit,
                edges,
                block: None,
            })
        }
        ResolveOutcome::Ambiguous(candidates) => Err(ResolveError::Ambiguous {
            source: scope,
            target: wikilink.target.clone(),
            candidates,
            lines: Vec::new(),
        }),
        ResolveOutcome::NotFound => Err(ResolveError::Unresolved {
            source: scope,
            target: wikilink.target.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One character of each UTF-8 width the fix has to survive: 2
    /// bytes (`é`), 3 bytes (`—`, `…`, a CJK character) and 4 bytes
    /// (an emoji).
    const MULTIBYTE: [&str; 5] = ["é", "—", "…", "日", "😀"];

    /// The expected result computed a different way from the code under
    /// test: lower-case the name (ASCII only, so every byte offset is
    /// kept) and let `strip_suffix`, which is boundary-safe by
    /// construction, find the extension. The stem must be non-empty
    /// (`.md` alone stays as it is).
    fn oracle(name: &str) -> &str {
        let lower = name.to_ascii_lowercase();
        for ext in [".md", ".markdown"] {
            if let Some(stem) = lower.strip_suffix(ext) {
                if !stem.is_empty() {
                    return &name[..stem.len()];
                }
            }
        }
        name
    }

    #[test]
    fn markdown_extensions_are_stripped_exactly_as_before() {
        let cases = [
            ("Plan.md", "Plan"),
            ("Plan.MD", "Plan"),
            ("Plan.Md", "Plan"),
            ("Plan.markdown", "Plan"),
            ("Plan.MARKDOWN", "Plan"),
            ("Plan.MarkDown", "Plan"),
            ("dir/Plan.md", "dir/Plan"),
            ("Plan.md.md", "Plan.md"),
            ("Plan.txt", "Plan.txt"),
            ("Plan", "Plan"),
            // An extension with no stem in front of it stays as it is.
            (".md", ".md"),
            (".markdown", ".markdown"),
            ("md", "md"),
            ("markdown", "markdown"),
            ("", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_markdown_extension(input), expected, "{input:?}");
        }
    }

    #[test]
    fn a_name_ending_in_a_multibyte_char_and_ascii_letters_is_returned_unchanged() {
        for c in MULTIBYTE {
            for letters in 1..=8 {
                let name = format!("Todo{c}{}", "a".repeat(letters));
                assert_eq!(strip_markdown_extension(&name), name, "{name:?}");
                let name = format!("Résumé—v2{c}{}", "z".repeat(letters));
                assert_eq!(strip_markdown_extension(&name), name, "{name:?}");
            }
        }
        // The names from the bug report.
        for name in ["Résumé—v2.docx", "Todo…ab", "日本x", "Résumé—v2.markdowñ"] {
            assert_eq!(strip_markdown_extension(name), name);
        }
    }

    #[test]
    fn multibyte_names_with_a_real_markdown_extension_are_still_stripped() {
        let cases = [
            ("Plan…ab.md", "Plan…ab"),
            ("Résumé—v2.MD", "Résumé—v2"),
            ("日本.markdown", "日本"),
            ("😀.Markdown", "😀"),
            ("dé/日本/Plan—.md", "dé/日本/Plan—"),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_markdown_extension(input), expected, "{input:?}");
        }
    }

    /// Each of a 2-, 3- and 4-byte character at every distance from 0 to
    /// 12 bytes before the end of the name (so the 3-byte `.md` cut and
    /// the 9-byte `.markdown` cut each land inside the character, on
    /// either side of it, and on a boundary), alone, at the start and
    /// doubled. Nothing may panic and the result must be the oracle's.
    #[test]
    fn cut_position_table_never_panics() {
        let mut checked = 0;
        for c in MULTIBYTE {
            for pad in 0..=12 {
                let ascii = "a".repeat(pad);
                let names = [
                    format!("{c}{ascii}"),
                    format!("x{c}{ascii}"),
                    format!("{c}{c}{ascii}"),
                    format!("ab{c}{ascii}"),
                    // The extension itself sitting `pad` bytes before the end.
                    format!("{c}.md{ascii}"),
                    format!("{c}.markdown{ascii}"),
                    format!("{c}{ascii}.md"),
                    format!("{c}{ascii}.markdown"),
                ];
                for name in names {
                    assert_eq!(strip_markdown_extension(&name), oracle(&name), "{name:?}");
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 5 * 13 * 8);
    }

    #[test]
    fn a_name_exactly_as_long_as_the_extension_empty_or_only_multibyte_does_not_panic() {
        for name in [
            "",
            ".md",
            ".MD",
            ".markdown",
            ".MARKDOWN",
            // Exactly 3 and 9 bytes, but not the extension.
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
            assert_eq!(strip_markdown_extension(name), name, "{name:?}");
        }
    }

    /// Every string of up to six tokens over a small alphabet that mixes
    /// the extension's own letters with characters of every width:
    /// 8^6 strings, none may panic, and each must give the oracle's
    /// answer.
    #[test]
    fn exhaustive_short_strings_never_panic() {
        const TOKENS: [&str; 8] = ["a", ".", "md", "MD", "markdown", "é", "—", "😀"];
        fn walk(prefix: &mut String, depth: usize, count: &mut usize) {
            assert_eq!(
                strip_markdown_extension(prefix),
                oracle(prefix),
                "{prefix:?}"
            );
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
    fn normalize_wikilink_target_handles_multibyte_input() {
        let nfc = "Résumé—v2.docx";
        let nfd = "Re\u{301}sume\u{301}—v2.docx";
        assert_ne!(nfc, nfd);
        let cases = [
            (nfc, nfc),
            (nfd, nfc),
            ("Todo…ab", "Todo…ab"),
            ("日本x", "日本x"),
            ("Plan…ab.md", "Plan…ab"),
            ("caf\u{e9}\u{2014}v2.MD", "caf\u{e9}\u{2014}v2"),
            ("cafe\u{301}—v2.markdown", "caf\u{e9}—v2"),
            ("dé/日本/Todo…ab", "dé/日本/Todo…ab"),
            ("", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_wikilink_target(input), expected, "{input:?}");
        }
    }
}
