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
use tephra_cache::InFileScope;

use crate::rpc::auth::Authenticated;
use crate::rpc::backend::{
    BlockBackend, GraphBackend, RefactorBackend, SearchError, SourceBackend, UnitListError,
};
use crate::rpc::protocol::{
    BLOCK_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, REFACTOR_ERROR, RESOLVE_ERROR, SOURCE_ERROR,
};

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
    "link.preview_refactor",
    "link.apply_refactor",
    "unit.list",
    "unit.search",
    "block.ensure_id",
];

/// The three backends one `dispatch` call reads through, bundled so
/// the function signature doesn't grow a parameter every time a new
/// verb needs a new backend.
pub struct Backends<'a> {
    pub graph: &'a dyn GraphBackend,
    pub sources: &'a dyn SourceBackend,
    pub refactor: &'a dyn RefactorBackend,
    pub blocks: &'a dyn BlockBackend,
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
        "link.preview_refactor" => link_preview_refactor(params, backends.refactor),
        "link.apply_refactor" => link_apply_refactor(params, backends.refactor),
        "unit.list" => unit_list(params, backends.sources),
        "unit.search" => unit_search(params, backends.sources),
        "block.ensure_id" => block_ensure_id(params, backends.blocks),
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

/// `unit.list {source, folder?, recursive?}` (G1). `source` is a
/// non-empty string; `folder` (default: the root) a string; `recursive`
/// (default false) a boolean; `null` counts as absent for the two
/// optional ones. `folder` is normalized here, so a `..` or absolute
/// path is refused with `INVALID_PARAMS` before any backend runs.
fn unit_list(params: &Value, sources: &dyn SourceBackend) -> DispatchResult {
    let source = require_str(params, "source")?;
    let folder = match params.get("folder") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(invalid_params("\"folder\" must be a string")),
    };
    let recursive = match params.get("recursive") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid_params("\"recursive\" must be a boolean")),
    };
    let folder = crate::unit_list::normalize_folder(folder).map_err(invalid_params)?;
    sources
        .list_units(source, &folder, recursive)
        .map(|wire| serde_json::to_value(wire).expect("UnitListWire always serializes"))
        .map_err(|e| match e {
            UnitListError::InvalidParams(message) => invalid_params(message),
            UnitListError::Source(message) => (SOURCE_ERROR, message, None),
        })
}

/// `unit.search {query, scope?, in?, limit?}` (G8, G9). `query` is a
/// string, at most [`crate::search::fuzzy::MAX_QUERY_CHARS`] long.
/// `scope` (default `names`; `null` counts as absent) is one of:
///
/// - `names`: a search across every source. `query` needs at least one
///   non-blank character, and `in` is refused.
/// - `headings` or `blocks` (G9): a search inside the one file `in`
///   (`@source:path`), which is required: without it the error is
///   "pick a file first". `query` may be empty or blank, which lists
///   everything in the file in file order.
/// - `all_blocks` (G10, the `^^` search): a full-text search of every
///   block of every source. `query` needs at least one non-blank
///   character ("empty query" otherwise), and `in` is refused.
///
/// Any other value is unknown. A search is never quietly run as another
/// scope, and an `in` is never quietly ignored. `limit` (default
/// [`crate::search::DEFAULT_LIMIT`]; `null` counts as absent) is an
/// integer from 1 to [`crate::search::MAX_LIMIT`]. Every check happens
/// here, before the backend is called and so before any cache read.
fn unit_search(params: &Value, sources: &dyn SourceBackend) -> DispatchResult {
    let query = match params.get("query") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(invalid_params("\"query\" (string) is required")),
    };

    let scope = match params.get("scope") {
        None | Some(Value::Null) => "names",
        Some(Value::String(scope)) => scope.as_str(),
        Some(_) => return Err(invalid_params("\"scope\" must be a string")),
    };
    let in_file_scope = match scope {
        "headings" => Some(InFileScope::Headings),
        "blocks" => Some(InFileScope::Blocks),
        _ => None,
    };
    let unit = match params.get("in") {
        None | Some(Value::Null) => None,
        Some(Value::String(unit)) => Some(unit.as_str()),
        Some(_) => return Err(invalid_params("\"in\" must be a string")),
    };

    match (scope, in_file_scope, unit) {
        ("names", _, None) => {
            crate::search::parse_query(query).map_err(invalid_params)?;
        }
        ("names" | "all_blocks", _, Some(_)) => {
            return Err(invalid_params(format!(
                "\"in\" only goes with scope \"headings\" or \"blocks\", not \"{scope}\""
            )))
        }
        ("all_blocks", _, None) => {
            crate::search::all_blocks::check_query(query).map_err(invalid_params)?;
        }
        (_, Some(_), None) => {
            return Err(invalid_params(format!(
                "pick a file first: scope \"{scope}\" searches inside one file, so \"in\" (a unit id like \"@source:path\") is required"
            )))
        }
        (_, Some(_), Some(unit)) => {
            if crate::search::in_file::split_unit_id(unit).is_none() {
                return Err(invalid_params(format!(
                    "\"in\" must be a unit id like \"@source:path\", got \"{unit}\""
                )));
            }
            crate::search::in_file::parse_in_file_query(query).map_err(invalid_params)?;
        }
        _ => {
            return Err(invalid_params(format!(
                "unknown scope \"{scope}\"; \"names\", \"headings\", \"blocks\" and \"all_blocks\" are supported"
            )))
        }
    }

    let limit = match params.get("limit") {
        None | Some(Value::Null) => crate::search::DEFAULT_LIMIT,
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=crate::search::MAX_LIMIT).contains(n))
            .ok_or_else(|| {
                invalid_params(format!(
                    "\"limit\" must be an integer from 1 to {}",
                    crate::search::MAX_LIMIT
                ))
            })?,
        Some(_) => {
            return Err(invalid_params(format!(
                "\"limit\" must be an integer from 1 to {}",
                crate::search::MAX_LIMIT
            )))
        }
    };

    let to_error = |e| match e {
        SearchError::InvalidParams(message) => invalid_params(message),
        SearchError::Source(message) => (SOURCE_ERROR, message, None),
    };
    match (in_file_scope, unit) {
        (Some(scope), Some(unit)) => sources
            .search_in_file(unit, scope, query, limit)
            .map(|wire| serde_json::to_value(wire).expect("InFileSearchWire always serializes"))
            .map_err(to_error),
        _ if scope == "all_blocks" => sources
            .search_all_blocks(query, limit)
            .map(|hits| {
                serde_json::to_value(hits).expect("Vec<AllBlocksHitWire> always serializes")
            })
            .map_err(to_error),
        _ => sources
            .search_names(query, limit)
            .map(|hits| serde_json::to_value(hits).expect("Vec<SearchHitWire> always serializes"))
            .map_err(to_error),
    }
}

fn link_preview_refactor(params: &Value, refactor: &dyn RefactorBackend) -> DispatchResult {
    let source = require_str(params, "source")?;
    let old_path = require_str(params, "old_path")?;
    let new_path = require_str(params, "new_path")?;
    refactor
        .preview_refactor(source, old_path, new_path)
        .map(|wire| serde_json::to_value(wire).expect("RefactorPreviewWire always serializes"))
        .map_err(|e| (REFACTOR_ERROR, e, None))
}

fn link_apply_refactor(params: &Value, refactor: &dyn RefactorBackend) -> DispatchResult {
    let source = require_str(params, "source")?;
    let old_path = require_str(params, "old_path")?;
    let new_path = require_str(params, "new_path")?;
    refactor
        .apply_refactor(source, old_path, new_path)
        .map(|wire| serde_json::to_value(wire).expect("RefactorApplyWire always serializes"))
        .map_err(|e| (REFACTOR_ERROR, e, None))
}

fn block_ensure_id(params: &Value, blocks: &dyn BlockBackend) -> DispatchResult {
    let unit = require_str(params, "unit")?;
    // A JSON integer from 1 up; 0, negatives, fractions and strings are
    // refused here as malformed, a line past the file's end by the engine.
    let line = params
        .get("line")
        .and_then(Value::as_u64)
        .filter(|n| *n >= 1)
        .ok_or_else(|| invalid_params("\"line\" (integer, 1 or more) is required"))?;
    blocks
        .ensure_id(unit, line)
        .map(|wire| serde_json::to_value(wire).expect("EnsureIdWire always serializes"))
        .map_err(|e| match e {
            crate::block_id::BlockIdError::InvalidParams(m) => invalid_params(m),
            crate::block_id::BlockIdError::Refused(m) => (BLOCK_ERROR, m, None),
        })
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
                "link.preview_refactor",
                "link.apply_refactor",
                "unit.list",
                "unit.search",
                "block.ensure_id",
            ]
        );
    }

    use std::sync::Mutex;

    use serde_json::json;

    use crate::rpc::backend::{ScanStatsWire, SourceRowWire, UnitListWire};
    use crate::rpc::protocol::SOURCE_ERROR;

    /// A `SourceBackend` whose only live verb is `list_units`: it
    /// records the arguments it receives and answers a canned result.
    /// Nothing else in the dispatch path is faked.
    struct ListStub {
        calls: Mutex<Vec<(String, String, bool)>>,
        reply: Result<UnitListWire, UnitListError>,
    }

    impl ListStub {
        fn answering(reply: Result<UnitListWire, UnitListError>) -> Self {
            ListStub {
                calls: Mutex::new(Vec::new()),
                reply,
            }
        }
        fn empty_ok() -> Self {
            Self::answering(Ok(UnitListWire {
                source: "s".into(),
                folder: String::new(),
                recursive: false,
                offline: false,
                needs_rescan: false,
                entries: Vec::new(),
            }))
        }
        fn calls(&self) -> Vec<(String, String, bool)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SourceBackend for ListStub {
        fn register(&self, _: &str, _: &str) -> Result<ScanStatsWire, String> {
            unreachable!("unit.list never registers")
        }
        fn unregister(&self, _: &str) -> Result<(), String> {
            unreachable!("unit.list never unregisters")
        }
        fn list(&self) -> Result<Vec<SourceRowWire>, String> {
            unreachable!("unit.list never lists sources")
        }
        fn rescan(&self, _: &str) -> Result<ScanStatsWire, String> {
            unreachable!("unit.list never rescans")
        }
        fn list_units(
            &self,
            source: &str,
            folder: &str,
            recursive: bool,
        ) -> Result<UnitListWire, UnitListError> {
            self.calls
                .lock()
                .unwrap()
                .push((source.to_string(), folder.to_string(), recursive));
            self.reply.clone()
        }
    }

    fn invalid(params: Value) -> String {
        let stub = ListStub::empty_ok();
        let err = unit_list(&params, &stub).expect_err("should be rejected");
        assert_eq!(err.0, INVALID_PARAMS, "{params}: {}", err.1);
        assert!(stub.calls().is_empty(), "{params} reached the backend");
        err.1
    }

    #[test]
    fn unit_list_rejects_bad_params_before_the_backend_runs() {
        invalid(json!({}));
        invalid(json!({"source": ""}));
        invalid(json!({"source": 7}));
        invalid(json!({"source": null}));
        invalid(json!({"source": "s", "folder": 1}));
        invalid(json!({"source": "s", "folder": ["a"]}));
        invalid(json!({"source": "s", "recursive": 1}));
        invalid(json!({"source": "s", "recursive": "true"}));
        invalid(json!({"source": "s", "folder": "a\u{0}b"}));
    }

    #[test]
    fn unit_list_rejects_dot_dot_and_absolute_folders() {
        for folder in ["..", "../x", "a/../b", "a/..", "/etc", "/etc/passwd", "//x"] {
            let message = invalid(json!({"source": "s", "folder": folder}));
            assert!(
                message.contains("folder"),
                "message should name the param: {message}"
            );
        }
    }

    #[test]
    fn unit_list_passes_normalized_arguments_to_the_backend() {
        let cases = [
            (json!({"source": "s"}), ("", false)),
            (json!({"source": "s", "folder": ""}), ("", false)),
            (json!({"source": "s", "folder": "/"}), ("", false)),
            (json!({"source": "s", "folder": "."}), ("", false)),
            (
                json!({"source": "s", "folder": null, "recursive": null}),
                ("", false),
            ),
            (
                json!({"source": "s", "folder": "Attachments/"}),
                ("Attachments", false),
            ),
            (
                json!({"source": "s", "folder": "a//b/./c", "recursive": true}),
                ("a/b/c", true),
            ),
            (
                json!({"source": "s", "folder": "caf\u{e9} notes"}),
                ("caf\u{e9} notes", false),
            ),
        ];
        for (params, (folder, recursive)) in cases {
            let stub = ListStub::empty_ok();
            unit_list(&params, &stub).expect("accepted");
            assert_eq!(
                stub.calls(),
                vec![("s".to_string(), folder.to_string(), recursive)],
                "{params}"
            );
        }
    }

    #[test]
    fn unit_list_maps_backend_errors_to_the_right_codes() {
        let stub = ListStub::answering(Err(UnitListError::Source("no such folder: x".into())));
        let err = unit_list(&json!({"source": "s", "folder": "x"}), &stub).unwrap_err();
        assert_eq!((err.0, err.1.as_str()), (SOURCE_ERROR, "no such folder: x"));

        let stub = ListStub::answering(Err(UnitListError::InvalidParams("escapes".into())));
        let err = unit_list(&json!({"source": "s", "folder": "link"}), &stub).unwrap_err();
        assert_eq!((err.0, err.1.as_str()), (INVALID_PARAMS, "escapes"));
    }

    #[test]
    fn unit_list_serializes_the_backend_result_as_the_response() {
        use crate::rpc::backend::UnitEntryWire;
        let stub = ListStub::answering(Ok(UnitListWire {
            source: "s".into(),
            folder: "d".into(),
            recursive: true,
            offline: true,
            needs_rescan: true,
            entries: vec![
                UnitEntryWire {
                    id: "@s:d/a.md".into(),
                    name: "a.md".into(),
                    rel_path: "d/a.md".into(),
                    kind: "prose".into(),
                    size: None,
                    mtime: Some(5),
                    title: Some("A".into()),
                },
                UnitEntryWire {
                    id: "@s:d/b.bin".into(),
                    name: "b.bin".into(),
                    rel_path: "d/b.bin".into(),
                    kind: "binary".into(),
                    size: Some(3),
                    mtime: None,
                    title: None,
                },
            ],
        }));
        let value = unit_list(
            &json!({"source": "s", "folder": "d", "recursive": true}),
            &stub,
        )
        .expect("ok");
        assert_eq!(
            value,
            json!({
                "source": "s", "folder": "d", "recursive": true,
                "offline": true, "needs_rescan": true,
                "entries": [
                    {"id": "@s:d/a.md", "name": "a.md", "rel_path": "d/a.md",
                     "kind": "prose", "size": null, "mtime": 5, "title": "A"},
                    {"id": "@s:d/b.bin", "name": "b.bin", "rel_path": "d/b.bin",
                     "kind": "binary", "size": 3, "mtime": null},
                ]
            })
        );
    }

    // ---- unit.search ---------------------------------------------------

    use crate::rpc::backend::SearchHitWire;

    /// A `SourceBackend` whose only live verb is `search_names`: it
    /// records the arguments it receives and answers a canned result.
    struct SearchStub {
        calls: Mutex<Vec<(String, usize)>>,
        /// The `search_all_blocks` calls (G10), kept apart from the
        /// name searches so a test can tell which verb ran.
        all_blocks_calls: Mutex<Vec<(String, usize)>>,
        reply: Result<Vec<SearchHitWire>, SearchError>,
    }

    impl SearchStub {
        fn answering(reply: Result<Vec<SearchHitWire>, SearchError>) -> Self {
            SearchStub {
                calls: Mutex::new(Vec::new()),
                all_blocks_calls: Mutex::new(Vec::new()),
                reply,
            }
        }
        fn empty_ok() -> Self {
            Self::answering(Ok(Vec::new()))
        }
        fn calls(&self) -> Vec<(String, usize)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SourceBackend for SearchStub {
        fn register(&self, _: &str, _: &str) -> Result<ScanStatsWire, String> {
            unreachable!("unit.search never registers")
        }
        fn unregister(&self, _: &str) -> Result<(), String> {
            unreachable!("unit.search never unregisters")
        }
        fn list(&self) -> Result<Vec<SourceRowWire>, String> {
            unreachable!("unit.search never lists sources")
        }
        fn rescan(&self, _: &str) -> Result<ScanStatsWire, String> {
            unreachable!("unit.search never rescans")
        }
        fn search_names(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<SearchHitWire>, SearchError> {
            self.calls.lock().unwrap().push((query.to_string(), limit));
            self.reply.clone()
        }
        fn search_all_blocks(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<crate::rpc::backend::AllBlocksHitWire>, SearchError> {
            self.all_blocks_calls
                .lock()
                .unwrap()
                .push((query.to_string(), limit));
            Ok(Vec::new())
        }
    }

    fn search_invalid(params: Value) -> String {
        let stub = SearchStub::empty_ok();
        let err = unit_search(&params, &stub).expect_err("should be rejected");
        assert_eq!(err.0, INVALID_PARAMS, "{params}: {}", err.1);
        assert!(stub.calls().is_empty(), "{params} reached the backend");
        err.1
    }

    #[test]
    fn unit_search_rejects_a_missing_blank_or_non_string_query_before_the_backend_runs() {
        for params in [
            json!({}),
            json!({"query": ""}),
            json!({"query": "   "}),
            json!({"query": "\t\n"}),
            json!({"query": null}),
            json!({"query": 7}),
            json!({"query": ["a"]}),
            json!({"query": {"q": "a"}}),
            json!({"scope": "names"}),
        ] {
            let message = search_invalid(params.clone());
            assert!(message.contains("query"), "{params}: {message}");
        }
        let too_long = "a".repeat(crate::search::fuzzy::MAX_QUERY_CHARS + 1);
        search_invalid(json!({ "query": too_long }));
    }

    #[test]
    fn unit_search_refuses_every_unknown_scope_and_bad_all_blocks_params_and_names_the_scope() {
        // `headings` and `blocks` need `in` (tests/unit/search_in_file.rs).
        // `all_blocks` (G10) is built: it refuses a blank or too-long
        // query and an `in`, before the backend runs.
        let message = search_invalid(json!({"query": " ", "scope": "all_blocks"}));
        assert!(message.contains("empty query"), "{message}");
        let message = search_invalid(json!({"query": "", "scope": "all_blocks"}));
        assert!(message.contains("empty query"), "{message}");
        let too_long = "a".repeat(crate::search::fuzzy::MAX_QUERY_CHARS + 1);
        search_invalid(json!({"query": too_long, "scope": "all_blocks"}));
        let message = search_invalid(json!({"query": "a", "scope": "all_blocks", "in": "@s:a"}));
        assert!(message.contains("all_blocks"), "{message}");
        for scope in ["", "Names", "NAMES", "name", "files", "names "] {
            let message = search_invalid(json!({"query": "a", "scope": scope}));
            assert!(message.contains("unknown scope"), "{scope:?}: {message}");
            assert!(message.contains(scope), "{scope:?}: {message}");
        }
        for scope in [json!(1), json!(true), json!(["names"]), json!({})] {
            let message = search_invalid(json!({"query": "a", "scope": scope}));
            assert!(message.contains("scope"), "{message}");
        }
    }

    #[test]
    fn unit_search_rejects_a_limit_that_is_not_a_positive_integer() {
        for limit in [
            json!(0),
            json!(-1),
            json!(-50),
            json!(1.5),
            json!(2.0),
            json!(1e3),
            json!("5"),
            json!("abc"),
            json!(true),
            json!([5]),
            json!({}),
            json!(crate::search::MAX_LIMIT + 1),
            json!(u64::MAX),
        ] {
            let message = search_invalid(json!({"query": "a", "limit": limit}));
            assert!(message.contains("limit"), "{limit}: {message}");
        }
    }

    #[test]
    fn unit_search_passes_the_query_verbatim_with_the_default_or_given_limit() {
        let cases = [
            (json!({"query": "atlas outside"}), ("atlas outside", 50)),
            (json!({"query": "atlas", "scope": "names"}), ("atlas", 50)),
            (
                json!({"query": "atlas", "scope": null, "limit": null}),
                ("atlas", 50),
            ),
            (json!({"query": "CAF\u{c9}", "limit": 1}), ("CAF\u{c9}", 1)),
            // Nothing is trimmed, escaped or reinterpreted.
            (
                json!({"query": " a,b @c/[[d]] .*( 100%_ "}),
                (" a,b @c/[[d]] .*( 100%_ ", 50),
            ),
            (
                json!({"query": "x", "limit": crate::search::MAX_LIMIT}),
                ("x", crate::search::MAX_LIMIT),
            ),
        ];
        for (params, (query, limit)) in cases {
            let stub = SearchStub::empty_ok();
            let value = unit_search(&params, &stub).expect("accepted");
            assert_eq!(value, json!([]));
            assert_eq!(stub.calls(), vec![(query.to_string(), limit)], "{params}");
        }
    }

    #[test]
    fn unit_search_all_blocks_calls_its_own_verb_with_the_query_verbatim() {
        let stub = SearchStub::empty_ok();
        // FTS5 operators and quotes are the daemon's to escape, not the
        // dispatcher's: it passes the text through unchanged.
        let value = unit_search(
            &json!({"query": " a\"b NEAR(c) ", "scope": "all_blocks"}),
            &stub,
        )
        .expect("accepted");
        assert_eq!(value, json!([]));
        let value = unit_search(
            &json!({"query": "x", "scope": "all_blocks", "limit": 7}),
            &stub,
        )
        .expect("accepted");
        assert_eq!(value, json!([]));
        assert_eq!(
            *stub.all_blocks_calls.lock().unwrap(),
            [(" a\"b NEAR(c) ".to_string(), 50), ("x".to_string(), 7)]
        );
        assert!(stub.calls().is_empty(), "no name search ran");
    }

    #[test]
    fn unit_search_serializes_each_hit_as_exactly_the_six_documented_fields() {
        let stub = SearchStub::answering(Ok(vec![SearchHitWire {
            id: "@notes:a/b.md".into(),
            source: "notes".into(),
            title: "B".into(),
            kind: "prose".into(),
            score: 0.875,
            offline: true,
        }]));
        let value = unit_search(&json!({"query": "b"}), &stub).expect("ok");
        assert_eq!(
            value,
            json!([{"id": "@notes:a/b.md", "source": "notes", "title": "B",
                    "kind": "prose", "score": 0.875, "offline": true}])
        );
    }

    #[test]
    fn unit_search_maps_backend_errors_to_the_right_codes() {
        let stub = SearchStub::answering(Err(SearchError::Source("cache is locked".into())));
        let err = unit_search(&json!({"query": "a"}), &stub).unwrap_err();
        assert_eq!((err.0, err.1.as_str()), (SOURCE_ERROR, "cache is locked"));

        let stub = SearchStub::answering(Err(SearchError::InvalidParams("bad".into())));
        let err = unit_search(&json!({"query": "a"}), &stub).unwrap_err();
        assert_eq!((err.0, err.1.as_str()), (INVALID_PARAMS, "bad"));
    }

    // -- block.ensure_id (G11) ------------------------------------------

    use crate::block_id::{BlockIdError, EnsureIdWire};

    /// A `BlockBackend` that records `(unit, line)` and answers a canned
    /// result. The validation under test is `block_ensure_id`'s own.
    struct BlockStub {
        calls: Mutex<Vec<(String, u64)>>,
        reply: Result<EnsureIdWire, BlockIdError>,
    }

    impl BlockStub {
        fn answering(reply: Result<EnsureIdWire, BlockIdError>) -> Self {
            BlockStub {
                calls: Mutex::new(Vec::new()),
                reply,
            }
        }
        fn created() -> Self {
            Self::answering(Ok(EnsureIdWire {
                id: Some("k3f9a2".into()),
                link: "[[@notes:a#^k3f9a2]]".into(),
                created: true,
            }))
        }
        fn calls(&self) -> Vec<(String, u64)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl BlockBackend for BlockStub {
        fn ensure_id(&self, unit: &str, line: u64) -> Result<EnsureIdWire, BlockIdError> {
            self.calls.lock().unwrap().push((unit.to_string(), line));
            self.reply.clone()
        }
    }

    #[test]
    fn block_ensure_id_passes_the_unit_and_line_through_and_serializes_the_result() {
        let stub = BlockStub::created();
        let value = block_ensure_id(&json!({"unit": "@notes:a.md", "line": 3}), &stub).expect("ok");
        assert_eq!(
            value,
            json!({"id": "k3f9a2", "link": "[[@notes:a#^k3f9a2]]", "created": true})
        );
        assert_eq!(stub.calls(), vec![("@notes:a.md".to_string(), 3)]);
    }

    #[test]
    fn block_ensure_id_serializes_a_heading_result_with_a_null_id() {
        let stub = BlockStub::answering(Ok(EnsureIdWire {
            id: None,
            link: "[[@notes:a#Head]]".into(),
            created: false,
        }));
        let value = block_ensure_id(&json!({"unit": "@notes:a.md", "line": 1}), &stub).unwrap();
        assert_eq!(
            value,
            json!({"id": null, "link": "[[@notes:a#Head]]", "created": false})
        );
    }

    #[test]
    fn block_ensure_id_refuses_a_missing_unit_a_missing_line_or_a_line_that_is_no_positive_integer()
    {
        for params in [
            json!({}),
            json!({"line": 3}),
            json!({"unit": "", "line": 3}),
            json!({"unit": 5, "line": 3}),
            json!({"unit": "@notes:a.md"}),
            json!({"unit": "@notes:a.md", "line": null}),
            json!({"unit": "@notes:a.md", "line": "3"}),
            json!({"unit": "@notes:a.md", "line": 3.5}),
            json!({"unit": "@notes:a.md", "line": -3}),
            json!({"unit": "@notes:a.md", "line": 0}),
            json!({"unit": "@notes:a.md", "line": true}),
            json!({"unit": "@notes:a.md", "line": [3]}),
        ] {
            let stub = BlockStub::created();
            let err = block_ensure_id(&params, &stub).unwrap_err();
            assert_eq!(err.0, INVALID_PARAMS, "{params}: {}", err.1);
            assert!(
                stub.calls().is_empty(),
                "{params}: the backend must not be reached"
            );
        }
    }

    #[test]
    fn block_ensure_id_maps_the_engines_errors_to_the_right_codes() {
        let stub = BlockStub::answering(Err(BlockIdError::Refused("line 5 is blank".into())));
        let err = block_ensure_id(&json!({"unit": "@notes:a.md", "line": 5}), &stub).unwrap_err();
        assert_eq!((err.0, err.1.as_str()), (BLOCK_ERROR, "line 5 is blank"));

        let stub = BlockStub::answering(Err(BlockIdError::InvalidParams("bad unit".into())));
        let err = block_ensure_id(&json!({"unit": "@notes:a.md", "line": 5}), &stub).unwrap_err();
        assert_eq!((err.0, err.1.as_str()), (INVALID_PARAMS, "bad unit"));
    }
}
