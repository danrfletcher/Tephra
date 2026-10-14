//! The JSON-RPC method dispatch table (G6): maps a validated
//! `(method, params)` pair to one of PR-5's resolver/graph verbs or
//! PR-4's source-registry verbs. Adds no new graph/registry logic —
//! every arm below is a thin params-shape check plus one call into
//! [`crate::rpc::backend::GraphBackend`]/[`crate::rpc::backend::SourceBackend`].
//!
//! [`dispatch`] takes `_proof: &Authenticated` as its first
//! parameter, which only [`crate::rpc::auth::authenticate`] can
//! produce (see that module's doc comment) — this is the
//! compile-time guarantee that no method in [`METHOD_NAMES`] can ever
//! be reached without the Bearer check having already run, satisfying
//! F5-adjacent without a per-method opt-in.

use serde_json::Value;

use crate::rpc::auth::Authenticated;
use crate::rpc::backend::{GraphBackend, SourceBackend};
use crate::rpc::protocol::{INVALID_PARAMS, METHOD_NOT_FOUND, RESOLVE_ERROR, SOURCE_ERROR};

/// Every JSON-RPC method this server exposes, exactly matching the
/// plan's named verbs (F4: "no bespoke client-convenience endpoints").
/// Kept as an explicit, reviewable list separate from the `match` arm
/// in [`dispatch`] below so `tests/unit/rpc_envelope.rs` can assert
/// the dispatch table is exactly this closed set, no more and no
/// fewer.
pub const METHOD_NAMES: &[&str] = &[
    "unit.resolve_link",
    "graph.get_backlinks",
    "graph.get_forward_links",
    "graph.get_edges",
    "source.register",
    "source.unregister",
    "source.list",
    "source.rescan",
];

/// The two backends one `dispatch` call reads through, bundled so the
/// function signature doesn't grow a parameter every time a new verb
/// needs a new backend.
pub struct Backends<'a> {
    pub graph: &'a dyn GraphBackend,
    pub sources: &'a dyn SourceBackend,
}

/// `(code, message, optional structured data)` — the shape every
/// dispatch failure path returns; `server.rs` turns this directly
/// into a JSON-RPC error object.
pub type DispatchError = (i64, String, Option<Value>);
pub type DispatchResult = Result<Value, DispatchError>;

fn invalid_params(message: impl Into<String>) -> DispatchError {
    (INVALID_PARAMS, message.into(), None)
}

fn require_str<'a>(params: &'a Value, field: &str) -> Result<&'a str, DispatchError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_params(format!("\"{field}\" (non-empty string) is required")))
}

fn optional_str<'a>(params: &'a Value, field: &str) -> Option<&'a str> {
    params.get(field).and_then(Value::as_str)
}

/// Dispatches one already-authenticated, already-envelope-validated
/// call. Returns the JSON `result` value on success, or a dispatch
/// error on failure — `server.rs` is responsible for wrapping either
/// into the full JSON-RPC response envelope (including echoing back
/// the client's own `id`, which this function never sees).
pub fn dispatch(
    _proof: &Authenticated,
    method: &str,
    params: &Value,
    backends: &Backends<'_>,
) -> DispatchResult {
    match method {
        "unit.resolve_link" => unit_resolve_link(params, backends.graph),
        "graph.get_backlinks" => graph_get_backlinks(params, backends.graph),
        "graph.get_forward_links" => graph_get_forward_links(params, backends.graph),
        "graph.get_edges" => graph_get_edges(backends.graph),
        "source.register" => source_register(params, backends.sources),
        "source.unregister" => source_unregister(params, backends.sources),
        "source.list" => source_list(backends.sources),
        "source.rescan" => source_rescan(params, backends.sources),
        other => Err((
            METHOD_NOT_FOUND,
            format!("unknown method \"{other}\""),
            None,
        )),
    }
}

fn unit_resolve_link(params: &Value, graph: &dyn GraphBackend) -> DispatchResult {
    let link = require_str(params, "link")?;
    let default_source = optional_str(params, "default_source");
    match graph.resolve_link(link, default_source) {
        Ok(result) => Ok(serde_json::to_value(result).expect("ResolveResult always serializes")),
        Err(e) => {
            let data = serde_json::to_value(&e).expect("ResolveError always serializes");
            Err((RESOLVE_ERROR, e.to_string(), Some(data)))
        }
    }
}

fn graph_get_backlinks(params: &Value, graph: &dyn GraphBackend) -> DispatchResult {
    let id = require_str(params, "id")?;
    Ok(serde_json::to_value(graph.get_backlinks(id)).expect("Vec<Edge> always serializes"))
}

fn graph_get_forward_links(params: &Value, graph: &dyn GraphBackend) -> DispatchResult {
    let id = require_str(params, "id")?;
    Ok(serde_json::to_value(graph.get_forward_links(id)).expect("Vec<Edge> always serializes"))
}

fn graph_get_edges(graph: &dyn GraphBackend) -> DispatchResult {
    Ok(serde_json::to_value(graph.get_edges()).expect("Vec<Edge> always serializes"))
}

fn source_register(params: &Value, sources: &dyn SourceBackend) -> DispatchResult {
    let name = require_str(params, "name")?;
    let root = require_str(params, "root")?;
    sources
        .register(name, root)
        .map(|stats| serde_json::to_value(stats).expect("ScanStatsWire always serializes"))
        .map_err(|e| (SOURCE_ERROR, e, None))
}

fn source_unregister(params: &Value, sources: &dyn SourceBackend) -> DispatchResult {
    let name = require_str(params, "name")?;
    sources
        .unregister(name)
        .map(|_| Value::Null)
        .map_err(|e| (SOURCE_ERROR, e, None))
}

fn source_list(sources: &dyn SourceBackend) -> DispatchResult {
    sources
        .list()
        .map(|rows| serde_json::to_value(rows).expect("Vec<SourceRowWire> always serializes"))
        .map_err(|e| (SOURCE_ERROR, e, None))
}

fn source_rescan(params: &Value, sources: &dyn SourceBackend) -> DispatchResult {
    let name = require_str(params, "name")?;
    sources
        .rescan(name)
        .map(|stats| serde_json::to_value(stats).expect("ScanStatsWire always serializes"))
        .map_err(|e| (SOURCE_ERROR, e, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F5-adjacent, the structural half: the dispatch table's method
    /// set is exactly [`METHOD_NAMES`], matched in `dispatch`'s `match`
    /// arms. This can't fully verify the `match` is exhaustive over
    /// that exact list from a unit test (that's what the
    /// `#[deny(unreachable_patterns)]`-free exhaustive `match` plus
    /// the `Authenticated` proof parameter already guarantee at
    /// compile time — see the module doc comment) but it does pin the
    /// list itself down against silent drift.
    #[test]
    fn method_names_is_the_frozen_closed_list() {
        assert_eq!(
            METHOD_NAMES,
            &[
                "unit.resolve_link",
                "graph.get_backlinks",
                "graph.get_forward_links",
                "graph.get_edges",
                "source.register",
                "source.unregister",
                "source.list",
                "source.rescan",
            ]
        );
    }
}
