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

    // R14: the *full* shape, asserted by exact equality — not just a
    // handful of spot-checked fields (`is_array`/`is_string`), which
    // would silently pass a renamed, added, or dropped field. README
    // has no frontmatter (`frontmatter_json: null`), no tags, no
    // blocks, and its two real outgoing edges: a same-source wikilink
    // to `one.md` and a cross-source one to `@notes:Journal.md`, in
    // `get_edges`'s sorted order.
    assert_eq!(
        json,
        serde_json::json!({
            "unit": {
                "id": "@atlas-repo:README.md",
                "source": "atlas-repo",
                "rel_path": "README.md",
                "frontmatter_json": null,
                "tags": [],
                "blocks": [],
                "offline": false
            },
            "edges": [
                {
                    "from": "@atlas-repo:README.md",
                    "to": {"resolved": "@atlas-repo:one.md"},
                    "to_raw": "one",
                    "kind": "wikilink",
                    "offline": false
                },
                {
                    "from": "@atlas-repo:README.md",
                    "to": {"resolved": "@notes:Journal.md"},
                    "to_raw": "@notes:Journal",
                    "kind": "wikilink",
                    "offline": false
                }
            ]
        }),
        "the frozen GP5 wire shape must match exactly, field-for-field — a renamed or \
         added field must fail this test, not silently pass it"
    );
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
    assert_eq!(
        json,
        serde_json::json!({
            "error": "ambiguous",
            "data": {
                "source": "atlas-repo",
                "target": "Guide",
                "candidates": ["@atlas-repo:api/Guide.md", "@atlas-repo:lib/Guide.md"]
            }
        }),
        "the frozen GP5 ambiguous-error wire shape must match exactly, including the \
         actual tied candidate ids, not just that `candidates` is some array"
    );
}
