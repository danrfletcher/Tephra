//! R7: GP5 freezes `resolve_link`'s result as the exact shape PR-6
//! returns unmodified over JSON-RPC. This snapshots that JSON for the
//! spec's own example (`[[@atlas-repo:README]]`) against the federated
//! fixtures, so the wire shape is pinned down and reviewable, not left
//! for PR-6 to invent. A deliberate change to this shape should update
//! this test in the same commit.

use tempfile::tempdir;

use crate::fixtures::{load_federated_fixtures, open_temp_cache};
use tephra_graph::UnitGraph;

#[test]
fn resolve_link_s_gp5_example_serializes_to_the_frozen_wire_shape() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let resolved = graph
        .resolve_link("[[@atlas-repo:README]]", Some("notes"))
        .expect("the spec's own GP5 example must resolve");

    let json = serde_json::to_value(&resolved).expect("ResolveResult must serialize");

    assert_eq!(json["unit"]["id"], "@atlas-repo:README.md");
    assert_eq!(json["unit"]["source"], "atlas-repo");
    assert_eq!(json["unit"]["rel_path"], "README.md");
    assert_eq!(json["unit"]["offline"], false);
    assert!(json["unit"]["tags"].is_array());
    assert!(json["unit"]["blocks"].is_array());
    assert!(json["edges"].is_array());

    // Every edge embeds the externally-tagged, snake_case EdgeTarget
    // shape: `{"resolved": "<id>"}` or the bare string `"dangling"`.
    for edge in json["edges"].as_array().expect("edges is an array") {
        assert!(edge["from"].is_string());
        assert!(edge["to"].is_object() || edge["to"] == "dangling");
        if let Some(obj) = edge["to"].as_object() {
            assert!(obj.contains_key("resolved"));
        }
        assert!(edge["to_raw"].is_string());
        assert!(edge["kind"].is_string());
        assert!(edge["offline"].is_boolean());
    }
}

/// R7: a `ResolveError` serializes adjacently-tagged
/// (`{"error": "...", "data": ...}`), so PR-6's RPC error mapping is
/// equally frozen, not just the success path.
#[test]
fn resolve_link_s_ambiguous_error_serializes_to_the_frozen_wire_shape() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let err = graph
        .resolve_link("[[Guide]]", Some("atlas-repo"))
        .expect_err("Guide.md is ambiguous between api/ and lib/");

    let json = serde_json::to_value(&err).expect("ResolveError must serialize");
    assert_eq!(json["error"], "ambiguous");
    assert_eq!(json["data"]["source"], "atlas-repo");
    assert_eq!(json["data"]["target"], "Guide");
    assert!(json["data"]["candidates"].is_array());
}
