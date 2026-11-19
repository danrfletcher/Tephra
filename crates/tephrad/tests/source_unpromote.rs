//! G12 / GP11 / FR22: a promoted source is folded back into its parent
//! (`SourceRegistry::unpromote`, the one behind `source.unpromote`). One
//! whole daemon per case (real registry, cache, graph, write loop and
//! push); the text of every file is read back from the real disk.
//!
//! The GP10 layout: the repo is the source `notes` (the parent), the vault
//! is the source `other`, and `crates/tephrad` of the repo is promoted as
//! `tephrad`.

mod fix_world;
mod fixtures;

use std::collections::BTreeSet;
use std::time::Duration;

use fix_world::{fixes_of, real_writer, Daemon};
use tephra_cache::Cache;
use tephra_graph::{EdgeTarget, UnitGraph};
use tephrad::registry::{RegistryError, UnpromoteError};

const FOLDER: &str = "crates/tephrad";

fn repo() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "README.md",
            "# Tephra\n\nSee [[crates/tephrad/src/registry.rs]] and [[docs/Design]].\n",
        ),
        (
            "docs/Design.md",
            "# Design\n\nThe link [[crates/tephrad/src/registry.rs#^k3f9a2|the registry]] and ![[crates/tephrad/tephrad]], plus [[lib.rs]].\n",
        ),
        ("crates/tephra-cache/src/lib.rs", "// cache ^cc0001\n"),
        (
            "crates/tephrad/src/registry.rs",
            "// the registry ^k3f9a2\n// see [[crates/tephrad/src/scan.rs]] [[scan.rs]] [[Design]] [[docs/Design]] [[@notes:README]] [[tephrad]]\n",
        ),
        (
            "crates/tephrad/src/scan.rs",
            "// the scan, back to [[registry.rs]] and [[registry.rs#^k3f9a2]]\n",
        ),
        (
            "crates/tephrad/tephrad.md",
            "---\nrelated: \"[[README]]\"\n---\n# tephrad notes\n\n[[registry.rs]] [[README]] [[crates/tephrad/tephrad]]\n",
        ),
    ]
}

fn vault() -> Vec<(&'static str, &'static str)> {
    vec![(
        "Vault.md",
        "# Vault\n\n[[@notes:crates/tephrad/src/registry.rs#^k3f9a2]] and ![[@notes:crates/tephrad/src/scan.rs]] and [[@notes:README|readme]]\n",
    )]
}

fn daemon() -> Daemon {
    Daemon::start(&repo(), &vault(), real_writer(), false)
}

fn promote(d: &Daemon) {
    d.registry
        .promote("tephrad", d.notes_root().join(FOLDER), &d.engine)
        .expect("promote");
}

fn unpromote(d: &Daemon) -> tephrad::registry::UnpromoteOutcome {
    d.registry
        .unpromote("tephrad", &d.engine)
        .expect("unpromote")
}

fn reopen(d: &Daemon) -> Cache {
    Cache::open(&d.cache_path).expect("reopen cache")
}

/// Every edge as `(from, resolved target or "", kind)`: what a link
/// reaches, not how it is spelled.
fn edge_set(d: &Daemon) -> BTreeSet<(String, String, String)> {
    let graph = UnitGraph::build_from_cache(&reopen(d)).expect("graph");
    graph
        .get_edges()
        .into_iter()
        .map(|e| {
            let to = match e.to {
                EdgeTarget::Resolved(id) => id,
                EdgeTarget::Dangling => format!("dangling:{}", e.to_raw),
            };
            (e.from, to, e.kind)
        })
        .collect()
}

fn names(d: &Daemon) -> Vec<String> {
    d.registry
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn gp11_the_vault_link_goes_back_to_the_parent_with_its_anchor() {
    let d = daemon();
    promote(&d);
    let promoted = d.other_text("Vault.md");
    assert!(
        promoted.contains("[[@tephrad:src/registry.rs#^k3f9a2]]"),
        "{promoted}"
    );
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let out = unpromote(&d);
    assert_eq!(out.name, "tephrad");
    assert_eq!(out.parent, "notes");
    assert_eq!(out.folder, FOLDER);
    assert_eq!(out.units_moved, 3);
    assert_eq!(out.status, "applied");

    let vault = d.other_text("Vault.md");
    assert!(
        vault.contains("[[@notes:crates/tephrad/src/registry.rs#^k3f9a2]]"),
        "{vault}"
    );
    assert!(
        vault.contains("![[@notes:crates/tephrad/src/scan.rs]]"),
        "{vault}"
    );
    assert!(vault.contains("[[@notes:README|readme]]"), "{vault}");
    assert!(!names(&d).contains(&"tephrad".to_string()));

    // ONE push: the fix entry and the source-list change together.
    let (push, ordinary) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("a push with the fix");
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], "unpromote");
    assert_eq!(
        push["params"]["sources_changed"],
        serde_json::json!([{"kind": "unpromoted", "name": "tephrad"}])
    );
    assert!(
        ordinary
            .iter()
            .all(|p| p["params"].get("sources_changed").is_none()),
        "no second source-list push: {ordinary:?}"
    );
    d.close();
}

#[test]
fn links_inside_the_folder_lose_their_prefix_and_respell_from_the_parent_root() {
    let d = daemon();
    promote(&d);
    let promoted = d.text("crates/tephrad/src/registry.rs");
    assert!(promoted.contains("[[@notes:README]]"), "{promoted}");
    unpromote(&d);

    let registry = d.text("crates/tephrad/src/registry.rs");
    // The prefix a promote added goes again.
    assert!(registry.contains("[[README]]"), "{registry}");
    assert!(!registry.contains("@notes"), "{registry}");
    // Bare-name links within the same source are untouched.
    assert!(registry.contains("[[scan.rs]]"), "{registry}");
    assert!(registry.contains("[[Design]]"), "{registry}");
    assert!(registry.starts_with("// the registry ^k3f9a2\n"));

    // The file that reached in from the parent reads as before.
    let readme = d.text("README.md");
    assert!(readme.contains("[[registry.rs]]"), "{readme}");
    let scan = d.text("crates/tephrad/src/scan.rs");
    assert_eq!(
        scan, "// the scan, back to [[registry.rs]] and [[registry.rs#^k3f9a2]]\n",
        "bare self-links stay"
    );
    d.close();
}

#[test]
fn round_trip_every_link_still_resolves_to_the_same_file() {
    let d = daemon();
    let before = edge_set(&d);
    assert!(!before.is_empty());
    promote(&d);
    unpromote(&d);
    assert_eq!(edge_set(&d), before);
    d.close();
}

#[test]
fn a_round_trip_changes_only_link_spans() {
    let d = daemon();
    let before = fixtures::tree_snapshot(d.notes.root());
    let vault_before = fixtures::tree_snapshot(d.other.root());
    promote(&d);
    unpromote(&d);
    let after = fixtures::tree_snapshot(d.notes.root());
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "same files"
    );
    for (path, (text_before, _)) in &before {
        let text_after = &after[path].0;
        // Whatever differs sits inside `[[...]]`: stripping every link
        // span leaves the same bytes.
        assert_eq!(
            strip_links(text_before),
            strip_links(text_after),
            "{path}: only link text may change"
        );
    }
    let vault_after = fixtures::tree_snapshot(d.other.root());
    for (path, (text_before, _)) in &vault_before {
        assert_eq!(
            strip_links(text_before),
            strip_links(&vault_after[path].0),
            "{path}"
        );
    }
    d.close();
}

fn strip_links(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        match rest[start..].find("]]") {
            Some(end) => rest = &rest[start + end + 2..],
            None => {
                rest = "";
                break;
            }
        }
        out.push_str("[[]]");
    }
    out.push_str(rest);
    out
}

#[test]
fn the_promoted_roots_row_the_source_and_the_exclusion_are_gone() {
    let d = daemon();
    promote(&d);
    assert!(d.registry.is_promoted("tephrad").unwrap());
    unpromote(&d);
    assert!(!d.registry.is_promoted("tephrad").unwrap());

    let cache = reopen(&d);
    assert!(cache.get_source("tephrad").unwrap().is_none());
    assert!(cache.promoted_roots().unwrap().is_empty());
    let parent = cache.get_source("notes").unwrap().unwrap();
    for rel in [
        "crates/tephrad/src/registry.rs",
        "crates/tephrad/src/scan.rs",
        "crates/tephrad/tephrad.md",
    ] {
        assert!(
            cache
                .get_unit_by_rel_path(parent.id, rel)
                .unwrap()
                .is_some(),
            "{rel} is a unit of the parent again"
        );
    }

    // The parent reads the folder again: a new file is picked up.
    d.notes.write("crates/tephrad/src/new.rs", "// new\n");
    d.registry.rescan("notes").expect("rescan parent");
    let cache = reopen(&d);
    assert!(cache
        .get_unit_by_rel_path(parent.id, "crates/tephrad/src/new.rs")
        .unwrap()
        .is_some());
    d.close();
}

#[test]
fn one_fix_log_entry_of_kind_unpromote_and_the_promote_entry_stays() {
    let d = daemon();
    promote(&d);
    unpromote(&d);
    let entries = d.entries();
    let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["unpromote", "promote"], "newest first, both kept");
    let entry = &entries[0];
    assert_eq!(entry.old.as_deref(), Some("@tephrad:"));
    assert_eq!(entry.new.as_deref(), Some(FOLDER));
    assert!(entry.files_changed > 0);
    d.close();
}

#[test]
fn refusals_change_nothing() {
    let d = daemon();
    let before = (d.other_text("Vault.md"), d.text("README.md"));
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    // Unknown name.
    assert!(matches!(
        d.registry.unpromote("ghost", &d.engine),
        Err(RegistryError::NotFound(_))
    ));
    // A normally registered source.
    match d.registry.unpromote("notes", &d.engine) {
        Err(RegistryError::Unpromote(e)) => assert_eq!(e.reason(), "not_promoted"),
        other => panic!("{other:?}"),
    }
    // A promoted source whose parent was unregistered is top-level.
    promote(&d);
    let promoted = (d.other_text("Vault.md"), d.text("README.md"));
    d.registry.unregister("notes").expect("unregister parent");
    match d.registry.unpromote("tephrad", &d.engine) {
        Err(RegistryError::Unpromote(UnpromoteError::NotPromoted { .. })) => {}
        other => panic!("{other:?}"),
    }
    assert!(names(&d).contains(&"tephrad".to_string()));
    assert_eq!(d.other_text("Vault.md"), promoted.0);
    assert_ne!(promoted, before, "the promote did rewrite");
    d.close();
}

#[test]
fn a_refusal_sends_no_push_and_no_log_entry() {
    let d = daemon();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    assert!(d.registry.unpromote("notes", &d.engine).is_err());
    assert!(d.registry.unpromote("ghost", &d.engine).is_err());
    assert!(client.next_push(Duration::from_millis(700)).is_none());
    assert!(d.entries().is_empty());
    d.close();
}

#[test]
fn an_offline_parent_refuses_with_source_offline_and_changes_nothing() {
    let d = daemon();
    promote(&d);
    let before = d.other_text("Vault.md");
    // The parent's root goes away: a rescan marks it offline.
    let parked = d.notes.root().with_extension("parked");
    std::fs::rename(d.notes.root(), &parked).expect("move the parent root away");
    let _ = d.registry.rescan("notes");
    let result = d.registry.unpromote("tephrad", &d.engine);
    std::fs::rename(&parked, d.notes.root()).expect("put the parent root back");
    match result {
        Err(RegistryError::Unpromote(e)) => assert_eq!(e.reason(), "source_offline"),
        other => panic!("{other:?}"),
    }
    assert_eq!(d.other_text("Vault.md"), before);
    assert!(names(&d).contains(&"tephrad".to_string()));
    assert!(reopen(&d).get_source("tephrad").unwrap().is_some());
    d.close();
}

// ---- FR13: promoted children ------------------------------------------

fn nested() -> Vec<(&'static str, &'static str)> {
    vec![
        ("outer/Top.md", "# top [[outer/inner/Deep]] [[Out]]\n"),
        ("outer/inner/Deep.md", "# deep [[Top]] [[Out]]\n"),
        ("Out.md", "# out [[outer/Top]] [[outer/inner/Deep]]\n"),
    ]
}

#[test]
fn unpromoting_a_source_with_promoted_children_leaves_them_promoted_under_its_parent() {
    let d = Daemon::start(&nested(), &[], real_writer(), false);
    d.registry
        .promote("outer", d.notes_root().join("outer"), &d.engine)
        .expect("outer");
    d.registry
        .promote("inner", d.notes_root().join("outer/inner"), &d.engine)
        .expect("inner out of outer");
    let before = edge_set_of(&d);
    assert!(reopen(&d)
        .promoted_roots()
        .unwrap()
        .iter()
        .any(|r| r.source == "inner" && r.parent == "outer"));

    d.registry.unpromote("outer", &d.engine).expect("unpromote");

    let roots = reopen(&d).promoted_roots().unwrap();
    assert_eq!(roots.len(), 1, "{roots:?}");
    assert_eq!(roots[0].source, "inner");
    assert_eq!(roots[0].parent, "notes", "now a child of the grandparent");
    assert_eq!(roots[0].rel_path, "outer/inner");
    assert!(names(&d).contains(&"inner".to_string()));
    assert!(!names(&d).contains(&"outer".to_string()));
    // The grandparent skips the folder the child owns: a new file there
    // goes to the child.
    d.notes.write("outer/inner/Fresh.md", "# f\n");
    d.registry.rescan("notes").expect("rescan");
    d.registry.rescan("inner").expect("rescan inner");
    let cache = reopen(&d);
    let notes = cache.get_source("notes").unwrap().unwrap();
    let inner = cache.get_source("inner").unwrap().unwrap();
    assert!(cache
        .get_unit_by_rel_path(notes.id, "outer/inner/Fresh.md")
        .unwrap()
        .is_none());
    assert!(cache
        .get_unit_by_rel_path(inner.id, "Fresh.md")
        .unwrap()
        .is_some());
    let _ = before;
    d.close();
}

fn edge_set_of(d: &Daemon) -> BTreeSet<(String, String, String)> {
    edge_set(d)
}

#[test]
fn unregistering_the_parent_of_promoted_children_leaves_them_top_level_with_no_rewrites() {
    let d = Daemon::start(&nested(), &[], real_writer(), false);
    d.registry
        .promote("outer", d.notes_root().join("outer"), &d.engine)
        .expect("outer");
    let before = (d.text("outer/Top.md"), d.text("Out.md"));
    d.registry
        .unregister("notes")
        .expect("unregister the parent");
    assert_eq!(names(&d), ["other", "outer"]);
    assert!(
        !d.registry.is_promoted("outer").unwrap(),
        "its parent is gone: it is top-level, whatever history the cache keeps"
    );
    assert_eq!((d.text("outer/Top.md"), d.text("Out.md")), before);
    // And it is top-level: no unpromote.
    assert!(matches!(
        d.registry.unpromote("outer", &d.engine),
        Err(RegistryError::Unpromote(_))
    ));
    d.close();
}

/// R1: the cache keeps the `promoted_roots` row as history when the
/// parent is unregistered. A DIFFERENT source registered later under the
/// parent's name is not that parent: the orphan stays top-level, is not
/// "promoted", and the new source skips nothing.
#[test]
fn a_new_source_with_the_old_parents_name_does_not_make_the_orphan_promoted_again() {
    let d = Daemon::start(&nested(), &[], real_writer(), false);
    d.registry
        .promote("outer", d.notes_root().join("outer"), &d.engine)
        .expect("promote outer");
    d.registry
        .unregister("notes")
        .expect("unregister the parent");

    // `notes` again, somewhere else, with a folder of its own named `outer`.
    let elsewhere = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(elsewhere.path().join("outer")).unwrap();
    std::fs::write(elsewhere.path().join("O.md"), "# o\n").unwrap();
    std::fs::write(elsewhere.path().join("outer/Mine.md"), "# mine\n").unwrap();
    d.registry
        .register("notes", elsewhere.path())
        .expect("register the new notes");

    let texts = |d: &Daemon| (d.text("outer/Top.md"), d.text("outer/inner/Deep.md"));
    let before = (texts(&d), names(&d), fixes_of_count(&d));
    let edges = edge_set(&d);

    // The stale row is not a promote of the new `notes`.
    assert!(!d.registry.is_promoted("outer").unwrap());
    match d.registry.unpromote("outer", &d.engine) {
        Err(RegistryError::Unpromote(e)) => assert_eq!(e.reason(), "not_promoted"),
        other => panic!("{other:?}"),
    }
    // Nothing moved, nothing rewritten, nothing logged.
    assert_eq!((texts(&d), names(&d), fixes_of_count(&d)), before);
    assert_eq!(edge_set(&d), edges);
    let cache = reopen(&d);
    let outer = cache.get_source("outer").unwrap().expect("outer stays");
    let notes = cache.get_source("notes").unwrap().unwrap();
    let top = cache.get_unit_by_rel_path(outer.id, "Top.md").unwrap();
    assert!(top.is_some(), "Top.md is still indexed by `outer`");
    assert!(cache
        .get_unit_by_rel_path(notes.id, "outer/Top.md")
        .unwrap()
        .is_none());

    // The new `notes` skips no folder: its own `outer/` is indexed.
    assert!(cache
        .get_unit_by_rel_path(notes.id, "outer/Mine.md")
        .unwrap()
        .is_some());
    assert!(cache.promoted_roots_of("notes").unwrap().is_empty());

    // Unregister of the orphan is the plain one: its rows go, no rewrite,
    // no log entry, and the new `notes` is untouched.
    assert!(!d
        .registry
        .unregister_promoted("outer", &d.engine)
        .expect("unregister_promoted"));
    d.registry
        .unregister("outer")
        .expect("unregister the orphan");
    let mut left = names(&d);
    left.sort();
    assert_eq!(left, ["notes", "other"]);
    assert_eq!(fixes_of_count(&d), before.2);
    assert_eq!(d.text("outer/Top.md"), before.0 .0);
    assert!(reopen(&d)
        .get_unit_by_rel_path(notes.id, "outer/Mine.md")
        .unwrap()
        .is_some());
    d.close();
}

fn fixes_of_count(d: &Daemon) -> usize {
    d.entries().len()
}

// ---- edge cases ---------------------------------------------------------

#[test]
fn a_folder_with_a_multibyte_name_unpromotes_without_a_panic() {
    let d = Daemon::start(
        &[
            ("café notes/Ünï.md", "# u [[Out]] [[café notes/Two]]\n"),
            ("café notes/Two.md", "# two [[Ünï]]\n"),
            ("Out.md", "# out [[café notes/Ünï]]\n"),
        ],
        &[],
        real_writer(),
        false,
    );
    d.registry
        .promote("cafe", d.notes_root().join("café notes"), &d.engine)
        .expect("promote");
    let out = d.registry.unpromote("cafe", &d.engine).expect("unpromote");
    assert_eq!(out.units_moved, 2);
    assert!(
        d.text("Out.md").contains("[[café notes/Ünï]]") || d.text("Out.md").contains("[[Ünï]]")
    );
    d.close();
}

#[test]
fn a_read_only_linking_file_is_left_alone_and_the_unpromote_still_completes() {
    use std::os::unix::fs::PermissionsExt;
    let d = daemon();
    promote(&d);
    let path = d.other.root().join("Vault.md");
    let before = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = d
        .registry
        .unpromote("tephrad", &d.engine)
        .expect("completes");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte");
    assert_ne!(out.status, "applied");
    // The rest were written, and the source is gone.
    assert!(d
        .text("crates/tephrad/src/registry.rs")
        .contains("[[README]]"));
    assert!(!names(&d).contains(&"tephrad".to_string()));
    d.close();
}

#[test]
fn f10_register_still_refuses_nested_and_ancestor_roots_after_unpromote_and_repromote() {
    let d = daemon();
    promote(&d);
    unpromote(&d);
    promote(&d);
    let nested = d
        .registry
        .register("nested", d.notes_root().join("crates/tephrad/src"));
    assert!(nested.is_err(), "inside the promoted source");
    let ancestor = d.registry.register("anc", d.notes.root().parent().unwrap());
    assert!(ancestor.is_err(), "above everything");
    d.close();
}

#[test]
fn a_second_pass_of_the_work_list_after_an_unpromote_writes_nothing() {
    let d = daemon();
    promote(&d);
    unpromote(&d);
    let snapshot = fixtures::tree_snapshot(d.notes.root());
    let vault = fixtures::tree_snapshot(d.other.root());
    d.engine.resume().expect("resume");
    assert_eq!(fixtures::tree_snapshot(d.notes.root()), snapshot);
    assert_eq!(fixtures::tree_snapshot(d.other.root()), vault);
    assert_eq!(d.entries().len(), 2, "no extra entry");
    d.close();
}

/// Links a user wrote inside the promoted source, spelled from its root
/// or with its own prefix or the parent's, one per link kind of G7 (a
/// wikilink, an embed, a frontmatter property link, the `@source:` form,
/// a block link with its anchor, an alias): each goes back to the
/// parent's form, a bare-name link is left as it is, and every link still
/// reaches the file it reached.
#[test]
fn links_written_inside_the_source_come_back_in_the_parents_form() {
    let d = daemon();
    promote(&d);
    d.notes.write(
        "crates/tephrad/src/extra.rs",
        "// [[src/scan.rs]] ![[@tephrad:src/scan.rs]] [[@tephrad:src/registry.rs#^k3f9a2|reg]] [[registry.rs]] [[@notes:README]] [[@notes:docs/Design|design]]\n",
    );
    d.notes.write(
        "crates/tephrad/prop.md",
        "---\nsource: \"[[src/scan.rs]]\"\nup: \"[[@notes:README]]\"\n---\n# p\n",
    );
    d.registry.rescan("tephrad").expect("rescan");
    let before = edge_set(&d);

    unpromote(&d);

    let extra = d.text("crates/tephrad/src/extra.rs");
    assert_eq!(
        extra,
        "// [[scan.rs]] ![[scan.rs]] [[registry.rs#^k3f9a2|reg]] [[registry.rs]] [[README]] [[docs/Design|design]]\n"
    );
    let prop = d.text("crates/tephrad/prop.md");
    assert_eq!(
        prop,
        "---\nsource: \"[[scan.rs]]\"\nup: \"[[README]]\"\n---\n# p\n"
    );
    // The same set of (from, to) pairs, spelled however: every moved
    // file now has the parent's id, so compare by file, not by source.
    let after = edge_set(&d);
    let tail = |id: &str| id.rsplit(':').next().unwrap().to_string();
    let reached = |set: &BTreeSet<(String, String, String)>| -> BTreeSet<(String, String)> {
        set.iter()
            .map(|(from, to, _)| (tail(from), tail(to)))
            .collect()
    };
    assert_eq!(
        reached(&after)
            .into_iter()
            .filter(|(f, _)| f.ends_with("extra.rs") || f.ends_with("prop.md"))
            .collect::<BTreeSet<_>>()
            .len(),
        reached(&before)
            .into_iter()
            .filter(|(f, _)| f.ends_with("extra.rs") || f.ends_with("prop.md"))
            .collect::<BTreeSet<_>>()
            .len()
    );
    assert!(
        after.iter().all(|(_, to, _)| !to.starts_with("dangling")),
        "{after:?}"
    );
    d.close();
}

/// E17 / FR24: the unpromote's own writes start no watcher batch, no
/// second fix and no second log entry; a user's save of a file it
/// rewrote, made afterwards, is a real edit and is indexed.
#[test]
fn own_writes_start_nothing_and_a_later_user_edit_is_indexed() {
    let d = daemon();
    promote(&d);
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    unpromote(&d);
    let (push, _) = client.next_fix_push(Duration::from_secs(8));
    assert!(push.is_some());

    // Nothing follows: no second fix push, no extra entry.
    let later = client.drain(Duration::from_millis(1500));
    assert!(
        later.iter().all(|p| p["params"].get("fixes").is_none()),
        "a second fix push: {later:?}"
    );
    assert_eq!(d.entries().len(), 2);

    let path = d.notes.root().join("README.md");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n## Saved by the user\n");
    std::fs::write(&path, text).unwrap();
    let cache = reopen(&d);
    let notes = cache.get_source("notes").unwrap().unwrap().id;
    let indexed = fix_world::wait_until(Duration::from_secs(15), || {
        let Ok(Some(unit)) = cache.get_unit_by_rel_path(notes, "README.md") else {
            return false;
        };
        cache
            .headings_for_unit(unit.id)
            .map(|h| h.iter().any(|h| h.text == "Saved by the user"))
            .unwrap_or(false)
    });
    assert!(indexed, "the user's save was not indexed");
    assert_eq!(d.entries().len(), 2, "the user's edit made no fix");
    d.close();
}
