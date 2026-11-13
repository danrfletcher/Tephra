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
    // to `one.md` (line 3) and a cross-source one to
    // `@notes:Journal.md` (line 4), in `get_edges`'s sorted order. It
    // is not a block link, so `block` is `null`. PR-13.F1 added
    // `edges[].line` and `block` to this shape on purpose.
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
                    "offline": false,
                    "line": 3
                },
                {
                    "from": "@atlas-repo:README.md",
                    "to": {"resolved": "@notes:Journal.md"},
                    "to_raw": "@notes:Journal",
                    "kind": "wikilink",
                    "offline": false,
                    "line": 4
                }
            ],
            "block": null
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

/// PR-13.F1 (G13): a block link's result carries `block {id, line,
/// moved}`. `ConflictReason`'s `// ^k3f9a2` sits on the `pub enum` line
/// (line 5) of the fixture's `src/registry.rs`, so the link written to
/// that file finds it there (`moved: false`).
#[test]
fn resolve_link_s_block_result_serializes_to_the_frozen_wire_shape() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let resolved = graph
        .resolve_link("[[@atlas-repo:src/registry.rs#^k3f9a2]]", Some("notes"))
        .expect("the block link must resolve");

    let json = serde_json::to_value(&resolved).expect("ResolveResult must serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "unit": {
                "id": "@atlas-repo:src/registry.rs",
                "source": "atlas-repo",
                "rel_path": "src/registry.rs",
                "frontmatter_json": null,
                "tags": [],
                "blocks": ["k3f9a2", "f1x7b2"],
                "offline": false
            },
            "edges": [],
            "block": {"id": "k3f9a2", "line": 5, "moved": false}
        })
    );
}

/// A block link written to the wrong file of the same source finds the
/// one file that has the ID and says so: `moved: true`.
#[test]
fn resolve_link_s_moved_block_serializes_with_moved_true() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let resolved = graph
        .resolve_link("[[@atlas-repo:one#^k3f9a2]]", Some("notes"))
        .expect("the ID is in exactly one other file of the source");

    let json = serde_json::to_value(&resolved).expect("ResolveResult must serialize");
    assert_eq!(json["unit"]["id"], "@atlas-repo:src/registry.rs");
    assert_eq!(
        json["block"],
        serde_json::json!({"id": "k3f9a2", "line": 5, "moved": true})
    );
}

/// `Edge.line` is the link's own line, in prose and in a code comment
/// alike, and it is part of the frozen shape `graph.get_backlinks`
/// returns.
#[test]
fn backlink_edges_serialize_with_their_line() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let backlinks = graph.get_backlinks("@atlas-repo:src/registry.rs#^k3f9a2");
    let json = serde_json::to_value(&backlinks).expect("Vec<Edge> must serialize");
    assert_eq!(
        json,
        serde_json::json!([{
            "from": "@notes:Code links.md",
            "to": {"resolved": "@atlas-repo:src/registry.rs"},
            "to_raw": "@atlas-repo:src/registry.rs#^k3f9a2",
            "kind": "wikilink",
            "offline": false,
            "line": 3
        }])
    );
}

/// The ambiguity error for a block ID lists every occurrence in
/// `lines` (E7/E8), next to the unchanged `candidates`.
#[test]
fn an_ambiguous_block_error_serializes_with_every_line() {
    let mut graph = UnitGraph::new();
    let facts = |lines: &[i64]| tephra_graph::UnitFacts {
        blocks: lines
            .iter()
            .map(|&line| tephra_graph::BlockFact {
                id: "abc123".to_string(),
                line,
            })
            .collect(),
        ..Default::default()
    };
    graph.upsert_unit("s", "Named.md", facts(&[]));
    graph.upsert_unit("s", "a.md", facts(&[2]));
    graph.upsert_unit("s", "b.md", facts(&[4, 9]));

    let err = graph
        .resolve_link("[[@s:Named#^abc123]]", None)
        .expect_err("the ID is in two files");
    assert_eq!(
        serde_json::to_value(&err).expect("ResolveError must serialize"),
        serde_json::json!({
            "error": "ambiguous",
            "data": {
                "source": "s",
                "target": "Named#^abc123",
                "candidates": ["@s:a.md", "@s:b.md"],
                "lines": [
                    {"unit": "@s:a.md", "line": 2},
                    {"unit": "@s:b.md", "line": 4},
                    {"unit": "@s:b.md", "line": 9}
                ]
            }
        })
    );
}
