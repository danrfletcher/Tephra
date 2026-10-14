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

use crate::model::{NodeFacts, UnitMetadata};
use crate::EdgeTarget;

/// The full result of a successful [`crate::UnitGraph::resolve_link`]
/// call: the resolved unit's metadata plus its own outgoing edges —
/// the exact shape GP5 freezes for PR-6's RPC handler to return
/// unmodified.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolveResult {
    pub unit: UnitMetadata,
    pub edges: Vec<crate::Edge>,
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
    Ambiguous {
        source: String,
        target: String,
        candidates: Vec<String>,
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
                write!(f, "no unit in source \"{source}\" matches \"{target}\"")
            }
            ResolveError::Ambiguous {
                source,
                target,
                candidates,
            } => write!(
                f,
                "\"{target}\" is ambiguous in source \"{source}\": tied candidates {candidates:?}"
            ),
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

/// File extensions [`strip_markdown_extension`] strips, matching
/// `tephrad::scan`'s own `MARKDOWN_EXTENSIONS` — every unit this
/// graph ever holds came from a markdown file (PR-4's scan pipeline
/// never writes a non-markdown `units` row), so every `rel_path`
/// ends in one of these, case-insensitively.
const MARKDOWN_EXTENSIONS: [&str; 2] = [".md", ".markdown"];

/// Strips a trailing `.md`/`.markdown` (case-insensitive) from
/// `rel_path`, if present; returns `rel_path` unchanged otherwise.
/// Applied to both sides of every match (R1): a stored unit's own
/// path *and* an incoming link's target, since a standard markdown
/// link (`[x](B.md)`) or an extension-qualified wikilink (`[[B.md]]`)
/// both name the file including its extension, unlike the bare-name
/// form (`[[B]]`) most wikilinks use.
pub(crate) fn strip_markdown_extension(rel_path: &str) -> &str {
    for ext in MARKDOWN_EXTENSIONS {
        if rel_path.len() > ext.len() {
            let (base, suffix) = rel_path.split_at(rel_path.len() - ext.len());
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
fn join_relative(from_dir: &str, target: &str) -> String {
    let mut stack: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
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
pub(crate) fn normalize_markdown_target(from_rel_path: &str, raw_target: &str) -> String {
    let without_anchor = raw_target.split('#').next().unwrap_or(raw_target);
    let decoded = percent_decode(without_anchor);
    let from_dir = from_rel_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let joined = join_relative(from_dir, &decoded);
    normalize_key(strip_markdown_extension(&joined))
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
        Some((
            from_source.to_string(),
            normalize_markdown_target(from_rel_path, to_raw),
        ))
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

/// `unit.resolve_link` (G4/GP5): resolves `raw` (a full `[[...]]`
/// wikilink or its bare inner text) against this graph.
/// `default_source` is the source to resolve a non-federated
/// (`@source`-prefix-less) target against; ignored when `raw` itself
/// carries a `@source:` prefix, in which case that source is used
/// instead (and must be a known source, E "unknown @source prefix").
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
    let index = graph.index();
    match resolve_in_scope(&index, &scope, &key, true) {
        ResolveOutcome::Unique(id) => {
            let unit = graph
                .unit(&id)
                .expect("a resolved id always has a corresponding node");
            let edges = graph.get_forward_links(&id);
            Ok(ResolveResult { unit, edges })
        }
        ResolveOutcome::Ambiguous(candidates) => Err(ResolveError::Ambiguous {
            source: scope,
            target: wikilink.target.clone(),
            candidates,
        }),
        ResolveOutcome::NotFound => Err(ResolveError::Unresolved {
            source: scope,
            target: wikilink.target.clone(),
        }),
    }
}
