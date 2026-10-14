//! Target resolution: turning a wikilink/markdown-link's raw text
//! into an [`crate::EdgeTarget`] (used while building/updating edges)
//! or a full [`ResolveResult`]/[`ResolveError`] (used by the public
//! [`crate::UnitGraph::resolve_link`] entry point, GP5). Both share
//! the same underlying in-source matching rule
//! ([`resolve_in_scope`]) — E4's tie-break and every other edge case
//! in this file apply identically whether the link in question is a
//! graph-internal edge or a directly-resolved query.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::model::{NodeFacts, UnitMetadata};
use crate::EdgeTarget;

/// The full result of a successful [`crate::UnitGraph::resolve_link`]
/// call: the resolved unit's metadata plus its own outgoing edges —
/// the exact shape GP5 freezes for PR-6's RPC handler to return
/// unmodified.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveResult {
    pub unit: UnitMetadata,
    pub edges: Vec<crate::Edge>,
}

/// Why [`crate::UnitGraph::resolve_link`] failed. Distinct from
/// [`EdgeTarget::Dangling`] (used internally for an already-built
/// edge) because a direct `resolve_link` call needs to report *why*,
/// not just *that*, resolution failed.
#[derive(Debug, Clone, PartialEq, Eq)]
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
fn strip_markdown_extension(rel_path: &str) -> &str {
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

/// The length (in `char`s, not bytes — so a multi-byte Unicode
/// character counts once, consistent regardless of encoding) of the
/// matched candidate path, used only to compare candidates against
/// each other for E4's shortest-path tie-break; never compared across
/// different units' absolute magnitude for any other purpose.
fn matched_length(stem: &str, target: &str) -> Option<usize> {
    if stem == target {
        return Some(stem.chars().count());
    }
    if !target.contains('/') {
        let basename = stem.rsplit('/').next().unwrap_or(stem);
        if basename == target {
            return Some(stem.chars().count());
        }
    }
    None
}

/// Resolves `target` against every unit belonging to `scope_source`
/// only (R4/"no cross-source leakage": a caller must pick the scope
/// before calling this — it never searches outside the one source
/// given). Matching rule (E4, a judgement call where the spec is
/// silent on the exact shape): a candidate matches if its path, with
/// a trailing `.md`/`.markdown` stripped, equals `target` exactly, or
/// — when `target` itself contains no `/` — its final path component
/// (basename, extension stripped) equals `target`. Among every match,
/// the one(s) with the shortest matched path (by `char` count) win;
/// a tie among the shortest is reported as [`ResolveOutcome::Ambiguous`]
/// with every tied id, sorted, rather than guessing one.
pub(crate) fn resolve_in_scope(
    nodes: &HashMap<String, NodeFacts>,
    by_source: &HashMap<String, HashSet<String>>,
    scope_source: &str,
    target: &str,
) -> ResolveOutcome {
    let empty: HashSet<String> = HashSet::new();
    let ids = by_source.get(scope_source).unwrap_or(&empty);

    let mut matches: Vec<(String, usize)> = Vec::new();
    for id in ids {
        if let Some(facts) = nodes.get(id) {
            let stem = strip_markdown_extension(&facts.rel_path);
            if let Some(len) = matched_length(stem, target) {
                matches.push((id.clone(), len));
            }
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

/// Resolves one outgoing link (`to_raw`, `kind`) authored by
/// `from_id` (belonging to `from_source`) into an [`EdgeTarget`],
/// used while (re)building the graph's edge layer. Never returns an
/// error — a link that can't be resolved for any reason (unknown
/// source prefix, zero matches, an ambiguous tie, an absolute URL)
/// becomes [`EdgeTarget::Dangling`] (E4/"never silently resolved as
/// if current"), since an edge, unlike a direct `resolve_link` call,
/// has nowhere to surface a typed error to.
pub(crate) fn resolve_edge_target(
    nodes: &HashMap<String, NodeFacts>,
    by_source: &HashMap<String, HashSet<String>>,
    known_sources: &HashSet<String>,
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
        let scope = wikilink.source_prefix.as_deref().unwrap_or(from_source);
        if !known_sources.contains(scope) {
            return EdgeTarget::Dangling;
        }
        match resolve_in_scope(nodes, by_source, scope, &wikilink.target) {
            ResolveOutcome::Unique(id) => EdgeTarget::Resolved(id),
            ResolveOutcome::Ambiguous(_) | ResolveOutcome::NotFound => EdgeTarget::Dangling,
        }
    } else {
        // Markdown-style `[text](target)` link: no `@source` prefix
        // grammar, resolved only within the referring unit's own
        // source. An absolute URL (contains "://") never names a
        // vault/repo unit, so it is always dangling rather than
        // attempting (and inevitably failing) to match it.
        if to_raw.contains("://") {
            return EdgeTarget::Dangling;
        }
        let target = to_raw.split('#').next().unwrap_or(to_raw);
        match resolve_in_scope(nodes, by_source, from_source, target) {
            ResolveOutcome::Unique(id) => EdgeTarget::Resolved(id),
            ResolveOutcome::Ambiguous(_) | ResolveOutcome::NotFound => EdgeTarget::Dangling,
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

    match resolve_in_scope(&graph.nodes, &graph.by_source, &scope, &wikilink.target) {
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
