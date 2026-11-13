//! `unit.search` with `scope: "all_blocks"` (G10, the `^^` search):
//! full-text search over every paragraph, comment block, symbol name,
//! heading and document paragraph of every registered source.
//!
//! What can be found is whatever the scan indexed into `block_fts`
//! ([`tephra_cache::Cache::search_all_blocks`] holds the matching and
//! ranking rules): `binary` and `too_large` files give nothing, and an
//! offline source's cached rows are searched like any other, each hit
//! flagged `offline` by the same test the name search uses.
//!
//! Like every search this is one read-only pass on a fresh
//! [`Cache::open_read_only`] connection: it never takes the shared cache
//! lock or the writer gate, so it can neither stall nor be stalled by a
//! scan, a rescan or a watcher batch, and under WAL it sees one
//! consistent snapshot (a unit is wholly its old rows or wholly its new
//! ones, and never a half-written one). It changes nothing: not the
//! cache, not any file.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use tephra_cache::{BlockHit, Cache};

use super::{fuzzy, root_is_missing, source_error, SearchError, MAX_LIMIT};

/// One `^^` result. `line` is set for a file; a document's hit has
/// `page`, `slide` or `paragraph` (the number) and `index` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllBlocksHitWire {
    /// `@source:rel_path`.
    pub unit: String,
    pub source: String,
    /// `paragraph`, `comment`, `symbol`, `heading` or `document
    /// paragraph`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slide: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paragraph: Option<i64>,
    /// A document paragraph's 1-based position within its page or
    /// slide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// A symbol as `enum ConflictReason`, else the text around the
    /// match, on one line.
    pub snippet: String,
    /// Only when the block carries a `^id` already in the file; never
    /// for a document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// The source is offline: its stored status says so, or its root is
    /// missing. The hit comes from the cache.
    pub offline: bool,
}

/// Checks `query` before anything reads the cache: it must have a
/// non-whitespace character ("empty query" otherwise) and be at most
/// [`fuzzy::MAX_QUERY_CHARS`] characters.
pub fn check_query(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("empty query: \"query\" must contain at least one non-blank character".into());
    }
    if raw.chars().count() > fuzzy::MAX_QUERY_CHARS {
        return Err(format!(
            "\"query\" is longer than {} characters",
            fuzzy::MAX_QUERY_CHARS
        ));
    }
    Ok(())
}

/// The best `limit` indexed blocks of every source for `query`, ranked.
/// `cache_path` is the daemon's `cache.db`.
pub fn search_all_blocks(
    cache_path: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<AllBlocksHitWire>, SearchError> {
    check_query(query).map_err(SearchError::InvalidParams)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(SearchError::InvalidParams(format!(
            "\"limit\" must be an integer from 1 to {MAX_LIMIT}"
        )));
    }

    let cache = Cache::open_read_only(cache_path).map_err(source_error)?;
    let hits = cache
        .search_all_blocks(query, limit)
        .map_err(source_error)?;

    // As in the name search: a root that has gone missing is offline
    // before the watcher has recorded it, and each source that produced
    // a hit is probed once.
    let mut missing: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let offline = hit.offline
            || match missing.get(&hit.source) {
                Some(&gone) => gone,
                None => {
                    let gone = root_is_missing(&cache, &hit.source)?;
                    missing.insert(hit.source.clone(), gone);
                    gone
                }
            };
        out.push(to_wire(hit, offline));
    }
    Ok(out)
}

fn to_wire(hit: BlockHit, offline: bool) -> AllBlocksHitWire {
    let kind = match (hit.kind.as_str(), hit.place.is_some()) {
        ("paragraph", true) => "document paragraph",
        ("paragraph", false) => "paragraph",
        ("comment", _) => "comment",
        ("symbol", _) => "symbol",
        ("heading", _) => "heading",
        // Only the kinds above are ever written.
        _ => "paragraph",
    };
    let place = |name: &str| {
        (hit.place.as_deref() == Some(name))
            .then_some(hit.place_n)
            .flatten()
    };
    AllBlocksHitWire {
        unit: format!("@{}:{}", hit.source, hit.rel_path),
        page: place("page"),
        slide: place("slide"),
        paragraph: place("paragraph"),
        index: hit.idx,
        source: hit.source,
        kind,
        line: hit.line,
        snippet: hit.snippet,
        block_id: hit.block_id,
        offline,
    }
}
