//! G11 / FR22: a folder of a registered source becomes a source of its own
//! (`SourceRegistry::promote`, the one behind `source.promote`). One whole
//! daemon per case (real registry, cache, graph, write loop and push); the
//! text of every file is read back from the real disk.
//!
//! The folder here is `proj/` of the source `notes`, promoted as `proj`.

mod fix_world;
mod fixtures;

use std::sync::Arc;
use std::time::Duration;

use fix_world::{fixes_of, real_writer, writer_over, Daemon};
use tephra_cache::Cache;
use tephrad::fixes::{FixEngine, FixError};
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::registry::{PromoteError, RegistryError};
use tephrad::rpc::backend::GraphBackend;

const A: &str = "# A\n\nSee [[B]] and [[Outside]] and [[A]].\n";
const B: &str = "# B\n\nback to [[A|the a]] and [[Outside#Part]].\n";

fn notes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("proj/A.md", A),
        ("proj/sub/B.md", B),
        (
            "Outside.md",
            "# Outside\n\n## Part\n\nreaches [[proj/A]], [[A|alias]], ![[proj/sub/B]], [[B#Head]] and [[Outside]].\n",
        ),
        ("Other.md", "plain [[Outside]]\n"),
    ]
}

fn daemon() -> Daemon {
    Daemon::start(&notes(), &[], real_writer(), false)
}

fn promote(d: &Daemon) -> Result<tephrad::registry::PromoteOutcome, RegistryError> {
    d.registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
}

fn reopen(d: &Daemon) -> Cache {
    Cache::open(&d.cache_path).expect("reopen cache")
}

#[test]
fn promote_moves_the_folder_and_rewrites_links_that_cross_the_border() {
    let d = daemon();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let out = promote(&d).expect("promote");
    assert_eq!(out.parent, "notes");
    assert_eq!(out.folder, "proj");
    assert_eq!(out.units_moved, 2);
    assert_eq!(out.status, "applied");

    // The parent's file reaching in names the new source now; a link
    // reaching out of the new source names the parent; links that stay on
    // one side are untouched.
    let outside = d.text("Outside.md");
    assert!(outside.contains("[[@proj:A]]"), "{outside}");
    assert!(outside.contains("![[@proj:sub/B]]"), "{outside}");
    assert!(outside.contains("[[Outside]]"), "{outside}");
    let a = d.text("proj/A.md");
    assert!(a.contains("[[B]]"), "{a}");
    assert!(a.contains("[[@notes:Outside]]"), "{a}");
    assert!(a.contains("[[A]]"), "{a}");
    assert_eq!(d.text("Other.md"), "plain [[Outside]]\n");
    eprintln!("OUTSIDE: {outside}\nA: {a}\nB: {}", d.text("proj/sub/B.md"));

    // One push: the source-list change and the fix entry together.
    let (push, ordinary) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("a push with the fix");
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], "promote");
    assert_eq!(
        push["params"]["sources_changed"],
        serde_json::json!([{"kind": "promoted", "name": "proj"}])
    );
    assert!(
        ordinary
            .iter()
            .all(|p| p["params"].get("sources_changed").is_none()),
        "no second source-list push: {ordinary:?}"
    );

    // The graph answers for the new layout.
    assert!(!d.shared.get_backlinks("@proj:A.md").is_empty());
    assert!(d.shared.get_backlinks("@notes:proj/A.md").is_empty());
    d.close();
}

#[test]
fn the_parent_never_reads_the_folder_again() {
    let d = daemon();
    promote(&d).expect("promote");

    d.notes.write("proj/New.md", "# new\n");
    d.notes.write("Fresh.md", "# fresh\n");
    d.registry.rescan("notes").expect("rescan parent");
    d.registry.rescan("proj").expect("rescan child");

    let cache = reopen(&d);
    let parent = cache.get_source("notes").unwrap().unwrap();
    let child = cache.get_source("proj").unwrap().unwrap();
    assert!(cache
        .get_unit_by_rel_path(parent.id, "Fresh.md")
        .unwrap()
        .is_some());
    assert!(cache
        .get_unit_by_rel_path(parent.id, "proj/New.md")
        .unwrap()
        .is_none());
    assert!(cache
        .get_unit_by_rel_path(parent.id, "proj/A.md")
        .unwrap()
        .is_none());
    assert!(cache
        .get_unit_by_rel_path(child.id, "New.md")
        .unwrap()
        .is_some());
    assert!(cache
        .get_unit_by_rel_path(child.id, "A.md")
        .unwrap()
        .is_some());
    d.close();
}

#[test]
fn promote_is_refused_with_a_reason_and_changes_nothing() {
    let d = daemon();
    let before = (d.text("Outside.md"), d.text("proj/A.md"));
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let notes_root = d.notes_root();
    let elsewhere = fixtures::empty_source();
    elsewhere.mkdir("folder");
    let cases: Vec<(&str, std::path::PathBuf, &str)> = vec![
        ("proj", notes_root.clone(), "is_a_source_root"),
        ("proj", d.other_root(), "is_a_source_root"),
        (
            "proj",
            elsewhere.root().join("folder").canonicalize().unwrap(),
            "not_inside_a_source",
        ),
        ("proj", notes_root.join("Outside.md"), "not_a_folder"),
        ("proj", notes_root.join("nope"), "not_a_folder"),
        ("Notes", notes_root.join("proj"), "name_taken"),
        ("has space", notes_root.join("proj"), "invalid_name"),
    ];
    for (name, root, reason) in cases {
        match d.registry.promote(name, &root, &d.engine) {
            Err(RegistryError::Promote(e)) => assert_eq!(e.reason(), reason, "{root:?}"),
            other => panic!("{root:?}: wanted {reason}, got {other:?}"),
        }
    }

    assert_eq!((d.text("Outside.md"), d.text("proj/A.md")), before);
    let cache = reopen(&d);
    assert!(cache.get_source("proj").unwrap().is_none());
    assert!(cache.promoted_roots().unwrap().is_empty());
    assert!(
        client.next_push(Duration::from_millis(700)).is_none(),
        "a refusal sends nothing"
    );
    d.close();
}

#[test]
fn promote_a_second_time_or_over_a_taken_name_is_refused() {
    let d = daemon();
    promote(&d).expect("first");
    match promote(&d) {
        Err(RegistryError::Promote(e)) => {
            assert!(matches!(e, PromoteError::IsASourceRoot { .. }), "{e}")
        }
        other => panic!("{other:?}"),
    }
    d.close();
}

/// A kill between the one transaction and the move of the rows: the next
/// start finishes the re-owning, then the pending list is worked off.
#[test]
fn a_promote_killed_after_its_transaction_is_finished_at_the_next_start() {
    let d = daemon();
    let stopper = FixEngine::new(&d.cache_path, real_writer()).with_stop_after_promote_commit();
    let result = d
        .registry
        .promote("proj", d.notes_root().join("proj"), &stopper);
    assert!(
        matches!(&result, Err(RegistryError::Fix(m)) if m.contains(&FixError::Stopped.to_string())),
        "{result:?}"
    );

    // As the kill leaves it: the source, the exclusion, the log entry and
    // the pending rows are there; the units still sit in the parent; no
    // file has been written.
    {
        let cache = reopen(&d);
        let parent = cache.get_source("notes").unwrap().unwrap();
        assert!(cache.get_source("proj").unwrap().is_some());
        assert_eq!(cache.promoted_roots().unwrap().len(), 1);
        assert!(cache
            .get_unit_by_rel_path(parent.id, "proj/A.md")
            .unwrap()
            .is_some());
        assert!(!cache.pending_fixes(None, None).unwrap().is_empty());
    }
    assert!(d.text("Outside.md").contains("[[proj/A]]"));

    d.registry.complete_promotes();
    {
        let cache = reopen(&d);
        let child = cache.get_source("proj").unwrap().unwrap();
        let parent = cache.get_source("notes").unwrap().unwrap();
        assert!(cache
            .get_unit_by_rel_path(child.id, "A.md")
            .unwrap()
            .is_some());
        assert!(cache
            .get_unit_by_rel_path(parent.id, "proj/A.md")
            .unwrap()
            .is_none());
    }
    // Finishing again changes nothing.
    d.registry.complete_promotes();

    d.engine.resume().expect("resume");
    let outside = d.text("Outside.md");
    assert!(outside.contains("[[@proj:A]]"), "{outside}");
    assert!(d.text("proj/A.md").contains("[[@notes:Outside]]"));
    d.close();
}

/// A file that cannot be written does not stop the others: it is left for
/// a retry and the entry says so; the file it could not write is
/// byte-for-byte what it was.
#[test]
fn a_file_that_cannot_be_written_is_left_alone_and_the_rest_are_fixed() {
    struct Deny;
    impl AtomicFs for Deny {
        fn write_new(&self, path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
            RealFs.write_new(path, content)
        }
        fn set_permissions(
            &self,
            path: &std::path::Path,
            perms: std::fs::Permissions,
        ) -> std::io::Result<()> {
            RealFs.set_permissions(path, perms)
        }
        fn sync(&self, path: &std::path::Path) -> std::io::Result<()> {
            RealFs.sync(path)
        }
        fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
            if to.ends_with("Outside.md") {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            RealFs.rename(from, to)
        }
    }
    let d = Daemon::start(&notes(), &[], writer_over(Arc::new(Deny)), false);
    let before = d.text("Outside.md");
    let out = promote(&d).expect("promote itself succeeds");
    assert_ne!(out.status, "applied");
    assert_eq!(d.text("Outside.md"), before, "unwritable file untouched");
    assert!(d.text("proj/A.md").contains("[[@notes:Outside]]"));
    d.close();
}

// ---- GP10: the Tephra layout ----------------------------------------

fn tephra_layout() -> Vec<(&'static str, &'static str)> {
    vec![
        ("README.md", "# Tephra\n\nSee [[crates/tephrad/src/registry.rs]] and [[docs/Design]].\n"),
        (
            "docs/Design.md",
            "# Design\n\nThe vault link [[@notes:crates/tephrad/src/registry.rs#^k3f9a2]] and [[registry.rs]], plus [[lib.rs]].\n",
        ),
        ("crates/tephra-cache/src/lib.rs", "// cache ^cc0001\n"),
        (
            "crates/tephrad/src/registry.rs",
            "// the registry ^k3f9a2\n// see [[crates/tephrad/src/scan.rs]] [[scan.rs]] [[Design]] [[docs/Design]] [[@notes:README]] [[tephrad]]\n",
        ),
        ("crates/tephrad/src/scan.rs", "// the scan, back to [[registry.rs]] and [[registry.rs#^k3f9a2]]\n"),
        ("crates/tephrad/tephrad.md", "# tephrad notes\n\n[[registry.rs]] [[README]] [[crates/tephrad/tephrad]]\n"),
    ]
}

#[test]
fn gp10_the_tephra_layout_rewrites_exactly_the_links_that_must_change() {
    let d = Daemon::start(&tephra_layout(), &[], real_writer(), false);
    let out = d
        .registry
        .promote("tephrad", d.notes_root().join("crates/tephrad"), &d.engine)
        .expect("promote");
    assert_eq!(out.units_moved, 3);

    // A file in the parent that reached in gains the new source and a
    // shortest-unique path in it; the anchor is kept.
    let readme = d.text("README.md");
    assert!(readme.contains("[[@tephrad:src/registry.rs]]"), "{readme}");
    let design = d.text("docs/Design.md");
    assert!(
        design.contains("[[@tephrad:src/registry.rs#^k3f9a2]]"),
        "{design}"
    );
    assert!(!design.contains("@tephra:crates"), "{design}");

    // Inside the folder: links out gain `@notes:`; links to itself stay.
    let registry = d.text("crates/tephrad/src/registry.rs");
    assert!(registry.contains("[[@notes:docs/Design]]"), "{registry}");
    assert!(registry.contains("[[@notes:README]]"), "{registry}");
    assert!(registry.contains("[[scan.rs]]"), "{registry}");
    assert!(!registry.contains("crates/tephrad/"), "{registry}");
    assert!(registry.contains("[[@notes:Design]]"), "{registry}");
    let notes_md = d.text("crates/tephrad/tephrad.md");
    assert!(notes_md.contains("[[@notes:README]]"), "{notes_md}");
    // The path spelling of its own file is respelled (FR23) to the
    // shortest text that still reaches it, and `[[registry.rs]]` (a name
    // inside the folder) and `[[@notes:README]]` are exactly as above.
    assert_eq!(
        notes_md,
        "# tephrad notes\n\n[[registry.rs]] [[@notes:README]] [[tephrad]]\n"
    );
    let scan = d.text("crates/tephrad/src/scan.rs");
    assert_eq!(
        scan, "// the scan, back to [[registry.rs]] and [[registry.rs#^k3f9a2]]\n",
        "self-links are left as they were"
    );
    // Nothing outside the link spans changed in any file.
    assert!(readme.starts_with("# Tephra\n\nSee "));
    d.close();
}

#[test]
fn a_second_promote_run_over_a_finished_one_writes_nothing() {
    let d = Daemon::start(&tephra_layout(), &[], real_writer(), false);
    d.registry
        .promote("tephrad", d.notes_root().join("crates/tephrad"), &d.engine)
        .expect("promote");
    let snapshot = fixtures::tree_snapshot(d.notes.root());
    d.engine.resume().expect("resume");
    assert_eq!(fixtures::tree_snapshot(d.notes.root()), snapshot);
    d.close();
}

// ---- E14: a promoted source inside the folder ---------------------

#[test]
fn promoting_a_folder_over_a_promoted_source_keeps_the_inner_one_its_own() {
    let d = Daemon::start(
        &[
            ("outer/Top.md", "# top [[outer/inner/Deep]] [[Out]]\n"),
            ("outer/inner/Deep.md", "# deep [[Top]] [[Out]]\n"),
            ("outer/inner/Other.md", "# other [[Deep]]\n"),
            ("Out.md", "# out [[outer/Top]] [[outer/inner/Deep]]\n"),
        ],
        &[],
        real_writer(),
        false,
    );
    d.registry
        .promote("inner", d.notes_root().join("outer/inner"), &d.engine)
        .expect("inner first");
    d.registry
        .promote("outer", d.notes_root().join("outer"), &d.engine)
        .expect("outer over inner");

    let cache = reopen(&d);
    let outer = cache.get_source("outer").unwrap().unwrap();
    let inner = cache.get_source("inner").unwrap().unwrap();
    let parent = cache.get_source("notes").unwrap().unwrap();
    assert!(cache
        .get_unit_by_rel_path(outer.id, "Top.md")
        .unwrap()
        .is_some());
    assert!(cache
        .get_unit_by_rel_path(outer.id, "inner/Deep.md")
        .unwrap()
        .is_none());
    assert!(cache
        .get_unit_by_rel_path(inner.id, "Deep.md")
        .unwrap()
        .is_some());
    assert!(cache
        .get_unit_by_rel_path(parent.id, "outer/Top.md")
        .unwrap()
        .is_none());

    // A rescan of the outer source does not take the inner one's files.
    d.registry.rescan("outer").expect("rescan");
    assert!(cache
        .get_unit_by_rel_path(outer.id, "inner/Deep.md")
        .unwrap()
        .is_none());
    assert!(cache
        .get_unit_by_rel_path(inner.id, "Deep.md")
        .unwrap()
        .is_some());

    // Prefixes, both ways.
    let top = d.text("outer/Top.md");
    assert!(top.contains("[[@inner:Deep]]"), "{top}");
    assert!(top.contains("[[@notes:Out]]"), "{top}");
    let deep = d.text("outer/inner/Deep.md");
    assert!(deep.contains("[[@outer:Top]]"), "{deep}");
    assert!(deep.contains("[[@notes:Out]]"), "{deep}");
    assert!(d.text("outer/inner/Other.md").contains("[[Deep]]"));
    // Inner promoted first: the second promote rewrote the links the
    // first one had made into the inner source's files.
    let out = d.text("Out.md");
    assert!(out.contains("[[@outer:Top]]"), "{out}");
    assert!(out.contains("[[@inner:Deep]]"), "{out}");
    d.close();
}

// ---- names, paths, ignores, permissions --------------------------

#[test]
fn a_folder_with_spaces_and_multibyte_characters_promotes_and_rewrites() {
    let d = Daemon::start(
        &[
            (
                "café notes/Ünï cøde.md",
                "# u [[Outside]] [[café notes/Plain]]\n",
            ),
            ("café notes/Plain.md", "# plain\n"),
            ("Outside.md", "# o [[café notes/Ünï cøde]] [[Plain]]\n"),
        ],
        &[],
        real_writer(),
        false,
    );
    let out = d
        .registry
        .promote("cafe", d.notes_root().join("café notes"), &d.engine)
        .expect("promote");
    assert_eq!(out.folder, "café notes");
    assert_eq!(out.units_moved, 2);
    let o = d.text("Outside.md");
    assert!(o.contains("[[@cafe:Ünï cøde]]"), "{o}");
    assert!(o.contains("[[@cafe:Plain]]"), "{o}");
    let u = d.text("café notes/Ünï cøde.md");
    assert!(u.contains("[[@notes:Outside]]"), "{u}");
    assert!(u.contains("[[Plain]]"), "{u}");
    d.close();
}

#[test]
fn root_is_canonicalised_like_register() {
    let d = daemon();
    let root = d.notes_root();
    // A trailing slash, a `..` and a path through a symlink all name the
    // one folder.
    let link = d.other.root().join("link");
    std::os::unix::fs::symlink(root.join("proj"), &link).unwrap();
    let dotdot = root.join("proj/sub/..");
    let out = d.registry.promote("proj", &dotdot, &d.engine).expect("..");
    assert_eq!(out.folder, "proj");
    assert_eq!(out.root, root.join("proj"));
    d.close();

    let d = daemon();
    let slash = format!("{}/", d.notes_root().join("proj").display());
    assert_eq!(
        d.registry
            .promote("proj", &slash, &d.engine)
            .unwrap()
            .folder,
        "proj"
    );
    d.close();

    // A symlink out of the parent is refused for where it goes.
    let d = daemon();
    let outside = fixtures::empty_source();
    outside.mkdir("real");
    let link = d.notes.root().join("escape");
    std::os::unix::fs::symlink(outside.root().join("real"), &link).unwrap();
    match d.registry.promote("esc", &link, &d.engine) {
        Err(RegistryError::Promote(e)) => assert_eq!(e.reason(), "not_inside_a_source"),
        other => panic!("{other:?}"),
    }
    d.close();
}

#[test]
fn ignored_paths_stay_untracked_in_the_new_source() {
    let d = Daemon::start(
        &[
            ("proj/A.md", "# a\n"),
            ("proj/.tephraignore", "private/\n"),
            ("proj/private/Secret.md", "# secret\n"),
            ("proj/node_modules/x/Y.md", "# y\n"),
        ],
        &[],
        real_writer(),
        false,
    );
    d.registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
        .expect("promote");
    d.registry.rescan("proj").expect("rescan");
    let cache = reopen(&d);
    let child = cache.get_source("proj").unwrap().unwrap();
    let parent = cache.get_source("notes").unwrap().unwrap();
    assert!(cache
        .get_unit_by_rel_path(child.id, "A.md")
        .unwrap()
        .is_some());
    for ignored in ["private/Secret.md", "node_modules/x/Y.md"] {
        assert!(
            cache
                .get_unit_by_rel_path(child.id, ignored)
                .unwrap()
                .is_none(),
            "{ignored}"
        );
    }
    assert!(cache
        .units_under_prefix(parent.id, "proj")
        .unwrap()
        .is_empty());
    d.close();
}

/// The folder's own ignore rules decide what the new source keeps: an
/// ignored file is not re-owned (no rescan is needed to drop it), and a
/// link to it is left as it was, not rewritten to `@proj:` for a file the
/// new source never indexes.
#[test]
fn ignored_files_are_not_reowned_and_links_to_them_are_not_rewritten() {
    let d = Daemon::start(
        &[
            ("proj/A.md", "# a\n"),
            ("proj/.tephraignore", "private/\n"),
            ("proj/private/Secret.md", "# secret\n"),
            (
                "Outside.md",
                "reaches [[proj/A]] and [[proj/private/Secret]]\n",
            ),
        ],
        &[],
        real_writer(),
        false,
    );
    let out = d
        .registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
        .expect("promote");
    assert_eq!(out.units_moved, 1, "only A.md is re-owned");

    // No rescan: the rows are already where they belong.
    let cache = reopen(&d);
    let child = cache.get_source("proj").unwrap().unwrap();
    let parent = cache.get_source("notes").unwrap().unwrap();
    assert!(cache
        .get_unit_by_rel_path(child.id, "A.md")
        .unwrap()
        .is_some());
    assert!(cache
        .get_unit_by_rel_path(child.id, "private/Secret.md")
        .unwrap()
        .is_none());
    assert!(cache
        .units_under_prefix(parent.id, "proj")
        .unwrap()
        .is_empty());

    let outside = d.text("Outside.md");
    assert!(outside.contains("[[@proj:A]]"), "{outside}");
    assert!(
        outside.contains("[[proj/private/Secret]]"),
        "the link to the ignored file is as it was: {outside}"
    );
    d.close();
}

/// A failure of the promote's own work after its transaction committed
/// (not a kill) must not leave the new source unwatched and unannounced
/// until the next start: it is watched and pushed, and the rewrites still
/// pending are finished by a retry.
#[test]
fn a_promote_that_fails_after_it_committed_still_watches_and_announces_the_source() {
    let d = daemon();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    let failing = FixEngine::new(&d.cache_path, real_writer()).with_fail_after_promote_reown();
    let result = d
        .registry
        .promote("proj", d.notes_root().join("proj"), &failing);
    assert!(
        matches!(&result, Err(RegistryError::Fix(m)) if m.contains("injected failure")),
        "{result:?}"
    );

    assert!(
        d.registry.is_watching("proj"),
        "the new source has a watcher"
    );
    {
        let cache = reopen(&d);
        let child = cache.get_source("proj").unwrap().unwrap();
        let parent = cache.get_source("notes").unwrap().unwrap();
        assert!(cache
            .get_unit_by_rel_path(child.id, "A.md")
            .unwrap()
            .is_some());
        assert!(cache
            .units_under_prefix(parent.id, "proj")
            .unwrap()
            .is_empty());
    }
    let pushes = client.drain(Duration::from_secs(3));
    assert!(
        pushes
            .iter()
            .any(|p| p.to_string().contains("sources_changed") && p.to_string().contains("proj")),
        "a push says the source list changed: {pushes:?}"
    );

    // The rewrites the failed run left pending are done by a retry.
    d.engine.resume().expect("resume");
    let outside = d.text("Outside.md");
    assert!(outside.contains("[[@proj:A]]"), "{outside}");
    d.close();
}

#[test]
fn a_read_only_linking_file_is_never_overwritten_and_the_rest_are_fixed() {
    use std::os::unix::fs::PermissionsExt;
    let d = daemon();
    let path = d.notes.root().join("Outside.md");
    let before = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = promote(&d).expect("the promote still succeeds");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte");
    assert!(d.text("proj/A.md").contains("[[@notes:Outside]]"));
    assert_ne!(out.status, "applied", "{}", out.status);
    d.close();
}

#[test]
fn unregistering_the_parent_after_a_promote_leaves_the_child_a_top_level_source() {
    let d = daemon();
    promote(&d).expect("promote");
    let before = d.text("proj/A.md");
    d.registry
        .unregister("notes")
        .expect("unregister the parent");
    let names: Vec<String> = d
        .registry
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert!(names.contains(&"proj".to_string()), "{names:?}");
    assert!(!names.contains(&"notes".to_string()));
    assert_eq!(d.text("proj/A.md"), before, "no rewrites");
    // `register` of the old parent's root nested over the child is still
    // refused, and one of the child's own subfolders too.
    assert!(d
        .registry
        .register("again", d.notes.root().join("proj/sub"))
        .is_err());
    d.close();
}

#[test]
fn register_after_promote_still_refuses_nested_and_ancestor_roots() {
    let d = daemon();
    promote(&d).expect("promote");
    let nested = d
        .registry
        .register("nested", d.notes.root().join("proj/sub"));
    assert!(nested.is_err(), "inside the promoted source");
    let ancestor = d.registry.register("anc", d.notes.root().parent().unwrap());
    assert!(ancestor.is_err(), "above both");
    d.close();
}

/// A kill after the transaction AND after file N: the files already
/// written are not touched again, the rest are finished by the next
/// start, and a run after that writes nothing.
#[test]
fn a_promote_killed_after_file_n_finishes_the_rest_and_leaves_earlier_files_alone() {
    let d = Daemon::start(
        &[
            ("proj/A.md", "# A\n\n[[Outside]] [[Two]]\n"),
            ("proj/Two.md", "# Two\n"),
            ("One.md", "# One\n\n[[proj/A]]\n"),
            ("Three.md", "# Three\n\n[[proj/Two]]\n"),
            ("Four.md", "# Four\n\n[[proj/A]] and [[Outside]]\n"),
            ("Outside.md", "# O\n"),
        ],
        &[],
        real_writer(),
        false,
    );
    let stopper = FixEngine::new(&d.cache_path, real_writer()).with_stop_after_writes(1);
    let result = d
        .registry
        .promote("proj", d.notes_root().join("proj"), &stopper);
    assert!(
        matches!(&result, Err(RegistryError::Fix(m)) if m.contains(&FixError::Stopped.to_string())),
        "{result:?}"
    );

    let files = ["proj/A.md", "One.md", "Three.md", "Four.md"];
    let original: std::collections::BTreeMap<&str, String> = [
        ("proj/A.md", "# A\n\n[[Outside]] [[Two]]\n"),
        ("One.md", "# One\n\n[[proj/A]]\n"),
        ("Three.md", "# Three\n\n[[proj/Two]]\n"),
        ("Four.md", "# Four\n\n[[proj/A]] and [[Outside]]\n"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_string()))
    .collect();
    let written: Vec<&str> = files
        .iter()
        .copied()
        .filter(|f| d.text(f) != original[f])
        .collect();
    assert_eq!(
        written.len(),
        1,
        "exactly one file was written: {written:?}"
    );
    let after_kill: Vec<String> = files.iter().map(|f| d.text(f)).collect();

    // The next start: the registry finishes the re-owning, the engine
    // works off what is pending.
    d.registry.complete_promotes();
    d.engine.resume().expect("resume");

    for (f, before) in files.iter().zip(&after_kill) {
        if *f == written[0] {
            assert_eq!(
                &d.text(f),
                before,
                "{f} was done before the kill: untouched"
            );
        }
    }
    assert!(
        d.text("One.md").contains("[[@proj:A]]"),
        "{}",
        d.text("One.md")
    );
    assert!(d.text("Three.md").contains("[[@proj:Two]]"));
    assert_eq!(d.text("Four.md"), "# Four\n\n[[@proj:A]] and [[Outside]]\n");
    assert!(d.text("proj/A.md").contains("[[@notes:Outside]]"));
    let entries = d.entries();
    let promote: Vec<_> = entries.iter().filter(|e| e.kind == "promote").collect();
    assert_eq!(promote.len(), 1, "one entry for the whole promote");
    assert_eq!(promote[0].status, "applied");

    let settled: Vec<String> = files.iter().map(|f| d.text(f)).collect();
    d.engine.resume().expect("resume again");
    let again: Vec<String> = files.iter().map(|f| d.text(f)).collect();
    assert_eq!(
        again, settled,
        "a run after a finished promote writes nothing"
    );
    d.close();
}

/// FR24: a save of a file the promote rewrote, made after Tephra wrote it
/// and while the watchers are still paused, is a real edit: it is indexed
/// once the watchers resume. The folder holds many files so the new
/// source's scan keeps the pause open long enough for the save to land in
/// it; the save is made as soon as the cache shows the rewritten link.
#[test]
fn a_user_edit_of_a_rewritten_file_made_during_the_promote_is_indexed() {
    let mut files: Vec<(String, String)> = vec![
        ("proj/A.md".into(), "# A\n\nSee [[Outside]].\n".into()),
        (
            "Outside.md".into(),
            "# Outside\n\nreaches [[proj/A]].\n".into(),
        ),
    ];
    for i in 0..1500 {
        files.push((
            format!("proj/bulk/n{i}.md"),
            format!("# Note {i}\n\ntext [[A]] #t{i}\n"),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let d = Daemon::start(&refs, &[], real_writer(), false);
    let outside_path = d.notes_root().join("Outside.md");

    let cache_path = d.cache_path.clone();
    let editor_path = outside_path.clone();
    let editor = std::thread::spawn(move || {
        let cache = Cache::open(&cache_path).expect("editor cache");
        let notes = cache.get_source("notes").unwrap().expect("notes").id;
        let rewritten = fix_world::wait_until(Duration::from_secs(30), || {
            let Ok(Some(unit)) = cache.get_unit_by_rel_path(notes, "Outside.md") else {
                return false;
            };
            cache
                .links_from_unit(unit.id)
                .map(|links| links.iter().any(|l| l.to_raw.contains("@proj")))
                .unwrap_or(false)
        });
        if rewritten {
            let mut text = std::fs::read_to_string(&editor_path).expect("read");
            text.push_str("\n## Saved by the user\n");
            std::fs::write(&editor_path, text).expect("user save");
        }
        rewritten
    });

    promote(&d).expect("promote");
    assert!(
        editor.join().expect("editor thread"),
        "the cache never showed the rewritten link"
    );
    assert!(
        d.text("Outside.md").contains("[[@proj:A]]"),
        "the promote's rewrite is there"
    );

    let cache = reopen(&d);
    let notes = cache.get_source("notes").unwrap().expect("notes").id;
    let indexed = fix_world::wait_until(Duration::from_secs(60), || {
        let Ok(Some(unit)) = cache.get_unit_by_rel_path(notes, "Outside.md") else {
            return false;
        };
        cache
            .headings_for_unit(unit.id)
            .map(|h| h.iter().any(|h| h.text == "Saved by the user"))
            .unwrap_or(false)
    });
    assert!(indexed, "the user's save was dropped on resume");
    assert!(d.text("Outside.md").contains("Saved by the user"));
    d.close();
}
