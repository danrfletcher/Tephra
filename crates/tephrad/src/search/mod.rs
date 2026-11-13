//! `unit.search` (G8): `[[`-style search across every registered source.
//! Only `scope: "names"` exists so far; the other scopes are refused by
//! the dispatcher, never answered as a name search.
//!
//! A search is one read-only pass over the cache
//! ([`tephra_cache::Cache::search_names`]) on a fresh
//! [`tephra_cache::Cache::open_read_only`] connection: it never takes the
//! shared cache lock or the writer gate (`crate::writer_gate`), so it can
//! neither stall nor be stalled by a scan, a rescan or the watchers, and
//! under WAL it sees one consistent snapshot, so a unit is never listed
//! twice. It changes nothing: not the cache, not the files, and never how
//! a link resolves: this module is not used by `tephra-graph`, and
//! `tests/search_fence.rs` checks that `resolver.rs` stays that way.
//!
//! The matching rule is [`fuzzy`]; this module adds the wire shape, the
//! parameter limits and the `offline` flag.

pub mod fuzzy;

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use tephra_cache::{Cache, SourceStatus};

use crate::registry::{probe_path, PathProbe};

/// How many results come back when the caller gives no `limit`.
pub const DEFAULT_LIMIT: usize = 50;

/// The largest `limit` accepted. Memory is bounded by the limit, so this
/// only stops a request from asking for an absurd amount.
pub const MAX_LIMIT: usize = 100_000;

/// One `unit.search` result: exactly these six fields on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHitWire {
    /// `@source:rel_path`.
    pub id: String,
    pub source: String,
    pub title: String,
    /// `prose`, `code`, `document`, `text-other` or `binary`.
    pub kind: String,
    /// 0 to 1; see [`fuzzy`].
    pub score: f64,
    /// The source is offline: its stored status says so, or its root is
    /// missing (the same test as `unit.list`). The result comes from the
    /// cache.
    pub offline: bool,
}

/// Why a `unit.search` call failed. The dispatcher maps `InvalidParams`
/// to `INVALID_PARAMS` and `Source` to `SOURCE_ERROR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    InvalidParams(String),
    Source(String),
}

/// Checks a query before anything reads the cache: a string with at
/// least one non-whitespace character and at most
/// [`fuzzy::MAX_QUERY_CHARS`] characters. Returns the parsed query.
pub fn parse_query(raw: &str) -> Result<fuzzy::Query, String> {
    let query = fuzzy::Query::new(raw)
        .ok_or_else(|| "\"query\" must contain at least one non-blank character".to_string())?;
    if query.len() > fuzzy::MAX_QUERY_CHARS {
        return Err(format!(
            "\"query\" is longer than {} characters",
            fuzzy::MAX_QUERY_CHARS
        ));
    }
    Ok(query)
}

/// The best `limit` units of every source for `query`, ranked; see the
/// module doc comment. `cache_path` is the daemon's `cache.db`.
pub fn search_names(
    cache_path: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHitWire>, SearchError> {
    let query = parse_query(query).map_err(SearchError::InvalidParams)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(SearchError::InvalidParams(format!(
            "\"limit\" must be an integer from 1 to {MAX_LIMIT}"
        )));
    }

    let cache = Cache::open_read_only(cache_path).map_err(source_error)?;
    let hits = cache
        .search_names(limit, |c| {
            query
                .rank(c.name, c.stem, c.title, c.rel_path)
                .map(|rank| rank.score)
        })
        .map_err(source_error)?;

    // A root that has gone missing is offline before the watcher has
    // recorded it. Only the sources that actually produced a result are
    // probed, once each.
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
        out.push(SearchHitWire {
            id: format!("@{}:{}", hit.source, hit.rel_path),
            source: hit.source,
            title: hit.title,
            kind: hit.kind,
            score: hit.score,
            offline,
        });
    }
    Ok(out)
}

fn root_is_missing(cache: &Cache, source: &str) -> Result<bool, SearchError> {
    // A source unregistered since the search ran has no row any more:
    // its hit is already gone from the next search, so treat it as online.
    Ok(match cache.get_source(source).map_err(source_error)? {
        Some(row) if row.status != SourceStatus::Offline => {
            probe_path(Path::new(&row.path)) == PathProbe::Missing
        }
        _ => false,
    })
}

fn source_error(e: impl std::fmt::Display) -> SearchError {
    SearchError::Source(e.to_string())
}
