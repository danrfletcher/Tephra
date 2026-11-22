//! PR-5 (GP4): links between a vault and a GitHub source, both ways, with
//! nothing downloaded. A vault note's `[[@github-atlas:README.md]]`
//! resolves through `unit.resolve_link`; `graph.get_backlinks` on the repo
//! file returns the note; the repo file's own links to the vault appear in
//! `graph.get_forward_links`, read once at the first index with the bytes
//! dropped. A link written before the add resolves once the listing exists.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use remote_support::{files_under, Daemon, Fake, REPO};
use serde_json::{json, Value};

const README: &str = "@github-atlas:README.md";

fn vault(d: &Daemon) -> fixtures::TempSource {
    let src = fixtures::empty_source();
    src.write(
        "note.md",
        "# Note\n\nThe project: [[@github-atlas:README.md]]\n",
    );
    d.ok(
        "source.register",
        json!({"name": "vault", "root": src.root().to_string_lossy()}),
    );
    src
}

fn edges(d: &Daemon, method: &str, id: &str) -> Vec<Value> {
    // Waits for the graph to carry the edges (see `Daemon::ok_when`).
    d.ok_when(method, json!({ "id": id }), |r| {
        r.as_array().is_some_and(|a| !a.is_empty())
    })
    .as_array()
    .unwrap()
    .clone()
}

fn froms(edges: &[Value]) -> Vec<&str> {
    let mut v: Vec<&str> = edges.iter().map(|e| e["from"].as_str().unwrap()).collect();
    v.sort_unstable();
    v
}

#[test]
fn a_vault_link_to_a_repo_file_resolves_with_nothing_downloaded() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let _vault = vault(&d);
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    d.wait_active("github-atlas");

    let reads = fake.reads().len();
    let requests = fake.requests();
    let resolved = d.ok(
        "unit.resolve_link",
        json!({"link": "[[@github-atlas:README.md]]"}),
    );
    assert_eq!(resolved["unit"]["id"], json!(README), "{resolved}");
    assert_eq!(fake.reads().len(), reads, "resolving reads no file");
    assert_eq!(fake.requests(), requests, "resolving asks GitHub nothing");
    // Still nothing on disk for the repo's files.
    assert!(files_under(&d.data_dir.join("remote")).is_empty());
    d.close();
}

#[test]
fn backlinks_work_both_ways() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let _vault = vault(&d);
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    d.wait_active("github-atlas");

    // Vault note -> repo file: the repo file's backlinks name the note.
    let back = edges(&d, "graph.get_backlinks", README);
    assert!(froms(&back).contains(&"@vault:note.md"), "{back:?}");
    let note_edge = back
        .iter()
        .find(|e| e["from"] == json!("@vault:note.md"))
        .unwrap();
    assert_eq!(note_edge["to"], json!({"resolved": README}));
    assert_eq!(note_edge["line"], json!(3));

    // Repo file -> vault note: read once at the first index, bytes dropped.
    let forward = edges(&d, "graph.get_forward_links", README);
    let to_vault = forward
        .iter()
        .find(|e| e["to_raw"] == json!("@vault:note"))
        .unwrap_or_else(|| panic!("the README's link to the vault: {forward:?}"));
    assert_eq!(to_vault["to"], json!({"resolved": "@vault:note.md"}));
    // ... and the vault note has it as a backlink.
    let back = edges(&d, "graph.get_backlinks", "@vault:note.md");
    assert_eq!(froms(&back), [README]);
    // A link inside the repo resolves inside the repo.
    let guide = edges(&d, "graph.get_backlinks", "@github-atlas:docs/Guide.md");
    assert_eq!(froms(&guide), [README]);

    let reads: Vec<String> = fake.reads();
    assert_eq!(
        reads
            .iter()
            .filter(
                |t| t.contains(&tephra_remote::testing::FakeGithub::blob_sha(
                    b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n"
                ))
            )
            .count(),
        1,
        "README is read exactly once"
    );
    d.close();
}

#[test]
fn a_vault_link_written_before_the_add_resolves_once_the_listing_exists() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let _vault = vault(&d);

    // Before the add: the link points nowhere.
    let reply = d.rpc(
        "unit.resolve_link",
        json!({"link": "[[@github-atlas:README.md]]"}),
    );
    assert!(
        reply.get("error").is_some(),
        "unresolved before the add: {reply}"
    );

    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    // At the listing, not at the end of the index: the link resolves now.
    let resolved = d.ok_when(
        "unit.resolve_link",
        json!({"link": "[[@github-atlas:README.md]]"}),
        |_| true,
    );
    assert_eq!(resolved["unit"]["id"], json!(README));
    // The vault edge exists from the listing on. The fixture's docs/Guide.md
    // also links [[README]], and that edge appears whenever the background
    // index has read Guide, so it is not asserted at this point.
    let back = edges(&d, "graph.get_backlinks", README);
    assert!(
        froms(&back).contains(&"@vault:note.md"),
        "the vault link is a backlink at the listing: {back:?}"
    );
    // Once the index is done the set is exact: the vault note and Guide.
    d.wait_active("github-atlas");
    let back = d.ok_when("graph.get_backlinks", json!({ "id": README }), |r| {
        r.as_array().is_some_and(|a| a.len() >= 2)
    });
    assert_eq!(
        froms(back.as_array().unwrap()),
        ["@github-atlas:docs/Guide.md", "@vault:note.md"],
        "{back}"
    );
    d.close();
}

#[test]
fn a_heading_of_an_indexed_repo_file_resolves_as_a_subpath() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    d.wait_active("github-atlas");
    let resolved = d.ok(
        "unit.resolve_link",
        json!({"link": "[[@github-atlas:README.md#Setup]]"}),
    );
    assert_eq!(resolved["unit"]["id"], json!(README), "{resolved}");
    d.close();
}

#[test]
fn a_refused_add_leaves_a_vault_link_dangling() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh.revoke_token();
    let d = Daemon::start(&fake);
    let _vault = vault(&d);
    assert!(d.add_github("github-atlas", REPO).get("error").is_some());
    let reply = d.rpc(
        "unit.resolve_link",
        json!({"link": "[[@github-atlas:README.md]]"}),
    );
    assert!(reply.get("error").is_some());
    d.close();
}
