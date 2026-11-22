//! G13 / G14 / GP12: renaming a source (`SourceRegistry::rename`, the one
//! behind `source.rename`). One whole daemon per case (real registry,
//! cache, graph, write loop and push); the text of every file is read
//! back from the real disk.
//!
//! The GP12 layout: the repo is the source `notes`, the vault is the
//! source `other`, and `crates/tephrad` of the repo is promoted as
//! `tephrad`. The renames are `tephrad` -> `daemon` and `other` ->
//! `vault`.

mod fix_world;
mod fixtures;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use fix_world::{fixes_of, real_writer, Daemon};
use tephra_cache::Cache;
use tephra_graph::{EdgeTarget, UnitGraph};
use tephrad::fixes::{FixEngine, FixError};
use tephrad::registry::{RegistryError, RenameError, RenameOutcome};

const FOLDER: &str = "crates/tephrad";

fn repo() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "README.md",
            "# Tephra\n\nSee [[docs/Design]], [[@tephrad:src/scan.rs|the scan]] and [a plain link](@tephrad:src/scan.rs), and read about @tephrad: in prose.\n",
        ),
        (
            "docs/Design.md",
            "---\nrelated: \"[[@tephrad:src/scan.rs]]\"\n---\n# Design\n\nThe link [[@tephrad:src/registry.rs#^k3f9a2|the registry]] and ![[@tephrad:tephrad]], plus [[@other:Vault]].\n",
        ),
        ("crates/tephra-cache/src/lib.rs", "// cache [[@tephrad:src/scan.rs]] ^cc0001\n"),
        (
            "crates/tephrad/src/registry.rs",
            "// the registry ^k3f9a2\n// see [[@tephrad:src/scan.rs]] [[scan.rs]] [[docs/Design]] [[@notes:README]] [[@other:Vault]]\n",
        ),
        (
            "crates/tephrad/src/scan.rs",
            "// the scan, back to [[registry.rs]] and [[@tephrad:src/registry.rs#^k3f9a2]]\n",
        ),
        (
            "crates/tephrad/tephrad.md",
            "---\nrelated: \"[[README]]\"\n---\n# tephrad notes\n\n[[@tephrad:src/registry.rs]] [[README]]\n",
        ),
    ]
}

fn vault() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Vault.md",
            "# Vault\n\n[[@notes:crates/tephrad/src/registry.rs#^k3f9a2]] and ![[@notes:crates/tephrad/src/scan.rs]] and [[@notes:README|readme]] and [[@notes:docs/Design]]\n",
        ),
        ("Other.md", "# Other\n\n[[Vault]] and [[@notes:README]]\n"),
    ]
}

fn daemon() -> Daemon {
    Daemon::start(&repo(), &vault(), real_writer(), false)
}

fn promoted() -> Daemon {
    let d = daemon();
    d.registry
        .promote("tephrad", d.notes_root().join(FOLDER), &d.engine)
        .expect("promote");
    d
}

fn rename(d: &Daemon, old: &str, new: &str) -> Result<RenameOutcome, RegistryError> {
    d.registry.rename(old, new, &d.engine)
}

fn reopen(d: &Daemon) -> Cache {
    Cache::open(&d.cache_path).expect("reopen cache")
}

fn names(d: &Daemon) -> Vec<String> {
    let mut v: Vec<String> = d
        .registry
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    v.sort();
    v
}

type Snapshot = BTreeMap<String, (Vec<u8>, std::time::SystemTime)>;

fn tree(d: &Daemon) -> (Snapshot, Snapshot) {
    (
        fixtures::tree_snapshot(d.notes.root()),
        fixtures::tree_snapshot(d.other.root()),
    )
}

/// Every edge as `(from, resolved target or "", kind)` with the source
/// prefix `old` spelled `new` in all of them: what a link reaches, not how
/// it is spelled.
fn edge_set(d: &Daemon, old: &str, new: &str) -> Vec<(String, String, String)> {
    let graph = UnitGraph::build_from_cache(&reopen(d)).expect("graph");
    let swap = |s: String| s.replace(&format!("@{old}:"), &format!("@{new}:"));
    let mut v: Vec<_> = graph
        .get_edges_all()
        .into_iter()
        .map(|e| {
            let to = match e.to {
                EdgeTarget::Resolved(id) => swap(id),
                EdgeTarget::Dangling => "dangling".to_string(),
            };
            (swap(e.from), to, e.kind)
        })
        .collect();
    v.sort();
    v
}

fn rename_entries(d: &Daemon) -> Vec<tephrad::fixes::FixEntryWire> {
    d.entries()
        .into_iter()
        .filter(|e| e.kind == "source_rename")
        .collect()
}

#[test]
fn gp12_every_link_of_every_kind_is_respelled_with_its_anchor_alias_and_form_intact() {
    let d = promoted();
    let vault_promoted = d.other_text("Vault.md");
    assert!(
        vault_promoted.contains("[[@tephrad:src/registry.rs#^k3f9a2]]"),
        "{vault_promoted}"
    );
    let before_edges = edge_set(&d, "tephrad", "daemon");
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    let (notes_before, other_before) = tree(&d);

    let out = rename(&d, "tephrad", "daemon").expect("rename");
    assert_eq!(
        (out.old_name.as_str(), out.new_name.as_str()),
        ("tephrad", "daemon")
    );
    assert!(!out.unchanged);
    assert_eq!(out.status, "applied");
    assert_eq!(out.deferred, 0);

    // The vault link, anchor kept; the alias and the embed kept.
    let vault = d.other_text("Vault.md");
    assert!(
        vault.contains("[[@daemon:src/registry.rs#^k3f9a2]]"),
        "{vault}"
    );
    assert!(vault.contains("![[@daemon:src/scan.rs]]"), "{vault}");
    assert!(
        vault.contains("[[@notes:README|readme]]"),
        "other prefixes stay: {vault}"
    );

    // The links a person wrote by hand keep their alias, anchor and form;
    // a plain Markdown link and prose are never touched.
    let readme = d.text("README.md");
    assert!(
        readme.contains("[[@daemon:src/scan.rs|the scan]]"),
        "{readme}"
    );
    assert!(
        readme.contains("[a plain link](@tephrad:src/scan.rs)"),
        "{readme}"
    );
    assert!(
        readme.contains("read about @tephrad: in prose."),
        "{readme}"
    );
    let design = d.text("docs/Design.md");
    assert!(
        design.contains("related: \"[[@daemon:src/scan.rs]]\""),
        "{design}"
    );
    assert!(
        design
            .contains("[[@daemon:src/registry.rs#^k3f9a2|the registry]] and ![[@daemon:tephrad]]"),
        "{design}"
    );
    let cache_lib = d.text("crates/tephra-cache/src/lib.rs");
    assert_eq!(cache_lib, "// cache [[@daemon:src/scan.rs]] ^cc0001\n");

    // F3: no file was moved, added or removed; only files that linked
    // changed; no `@tephrad:` link is left in a wikilink.
    let (notes_after, other_after) = tree(&d);
    assert_eq!(
        notes_before.keys().collect::<Vec<_>>(),
        notes_after.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        other_before.keys().collect::<Vec<_>>(),
        other_after.keys().collect::<Vec<_>>()
    );
    // Each source is compared with its own "after": both sources hold a
    // `Warmup.md` (the daemon fixture's watcher check), and the number of
    // warm-up rounds, so its text, differs between them under load.
    for (before, after) in [(&notes_before, &notes_after), (&other_before, &other_after)] {
        for (rel, (bytes, _)) in before.iter() {
            let now = String::from_utf8(after.get(rel).unwrap().0.clone()).unwrap();
            let was = String::from_utf8(bytes.clone()).unwrap();
            assert_eq!(
                now,
                was.replace("[[@tephrad:", "[[@daemon:")
                    .replace("![[@tephrad:", "![[@daemon:"),
                "{rel}"
            );
            assert!(!now.contains("[[@tephrad:"), "{rel}");
        }
    }

    // Every link reaches the same file as before.
    assert_eq!(edge_set(&d, "tephrad", "daemon"), before_edges);

    // The registry row, the cache and `source.list`.
    assert_eq!(names(&d), vec!["daemon", "notes", "other"]);
    let cache = reopen(&d);
    assert!(cache.get_source("tephrad").unwrap().is_none());
    let row = cache.get_source("daemon").unwrap().expect("daemon row");
    assert!(cache
        .get_unit_by_rel_path(row.id, "src/registry.rs")
        .unwrap()
        .is_some());
    let live = cache.live_promoted_roots().unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].source, "daemon");
    assert_eq!(live[0].parent, "notes");

    // ONE log entry for the whole rename.
    let entries = rename_entries(&d);
    assert_eq!(entries.len(), 1, "{:?}", d.entries());
    assert_eq!(entries[0].old.as_deref(), Some("@tephrad:"));
    assert_eq!(entries[0].new.as_deref(), Some("@daemon:"));
    assert_eq!(entries[0].sources, vec!["tephrad", "daemon"]);
    assert_eq!(entries[0].status, "applied");
    assert_eq!(entries[0].files_changed, out.files_changed);
    let linking = notes_before
        .iter()
        .chain(other_before.iter())
        .filter(|(_, (b, _))| String::from_utf8_lossy(b).contains("[[@tephrad:"))
        .count() as i64;
    assert!(linking >= 4, "the fixture has linking files: {linking}");
    assert_eq!(out.files_changed, linking);

    // ONE push: the fix entry and the source-list change together.
    let (push, ordinary) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("a push with the fix");
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], "source_rename");
    assert_eq!(
        push["params"]["sources_changed"],
        serde_json::json!([{"kind": "renamed", "name": "daemon", "old_name": "tephrad"}])
    );
    assert!(
        ordinary
            .iter()
            .all(|p| p["params"].get("sources_changed").is_none()),
        "no second source-list push: {ordinary:?}"
    );

    // E17: the rename's own writes start no watcher fix and no second entry.
    let _ = client.drain(Duration::from_millis(800));
    assert_eq!(rename_entries(&d).len(), 1);
    assert_eq!(d.entries().len(), 2, "the promote entry and the rename's");
    d.close();
}

#[test]
fn a_normal_source_is_renamed_the_same_way_and_the_old_name_no_longer_resolves() {
    let d = daemon();
    let before_edges = edge_set(&d, "other", "vault");
    let out = rename(&d, "other", "vault").expect("rename");
    assert_eq!(out.status, "applied");
    assert_eq!(out.files_changed, 2, "{out:?}");

    assert!(
        d.text("docs/Design.md").contains("plus [[@vault:Vault]]."),
        "{}",
        d.text("docs/Design.md")
    );
    assert!(d
        .text("crates/tephrad/src/registry.rs")
        .contains("[[@vault:Vault]]"));
    // The renamed source's own files keep their text (they link `@notes:`).
    assert!(d
        .other_text("Vault.md")
        .contains("[[@notes:README|readme]]"));
    assert_eq!(edge_set(&d, "other", "vault"), before_edges);

    assert_eq!(names(&d), vec!["notes", "vault"]);
    let cache = reopen(&d);
    assert!(cache.get_source("other").unwrap().is_none());
    let row = cache.get_source("vault").unwrap().expect("vault row");
    assert!(cache
        .get_unit_by_rel_path(row.id, "Vault.md")
        .unwrap()
        .is_some());
    // `@other:` no longer resolves: its links are not rewritten again, and
    // a link written that way is dangling.
    let graph = UnitGraph::build_from_cache(&cache).unwrap();
    assert!(graph
        .get_edges_all()
        .into_iter()
        .all(|e| !matches!(&e.to, EdgeTarget::Resolved(id) if id.starts_with("@other:"))));
    let entries = rename_entries(&d);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].old.as_deref(), Some("@other:"));

    // Unregistering the renamed source uses the new name; the old one is unknown.
    d.registry
        .unregister("other")
        .expect("the old name is a no-op");
    assert_eq!(
        names(&d),
        vec!["notes", "vault"],
        "the old name removed nothing"
    );
    d.registry
        .unregister("vault")
        .expect("unregister by the new name");
    assert_eq!(names(&d), vec!["notes"]);
    d.close();
}

#[test]
fn every_refusal_changes_nothing_and_has_its_stable_reason() {
    let d = promoted();
    let (notes, other) = tree(&d);
    let names_before = names(&d);
    let entries_before = d.entries().len();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let cases: [(&str, &str, &str); 11] = [
        ("tephrad", "", "invalid_name"),
        ("tephrad", "a b", "invalid_name"),
        ("tephrad", "a/b", "invalid_name"),
        ("tephrad", "a@b", "invalid_name"),
        ("tephrad", "a:b", "invalid_name"),
        ("tephrad", "a*b", "invalid_name"),
        ("tephrad", "notes", "name_taken"),
        // FR14: compared without regard to case.
        ("tephrad", "Notes", "name_taken"),
        ("tephrad", "OTHER", "name_taken"),
        ("ghost", "anything", "unknown_source"),
        ("Tephrad", "daemon", "unknown_source"),
    ];
    for (old, new, reason) in cases {
        match rename(&d, old, new) {
            Err(RegistryError::Rename(e)) => {
                assert_eq!(e.reason(), reason, "{old} -> {new:?}: {e}");
                assert!(!e.to_string().is_empty());
            }
            other => panic!("{old} -> {new:?}: expected a refusal, got {other:?}"),
        }
    }

    let (notes_after, other_after) = tree(&d);
    assert_eq!(notes, notes_after, "bytes and mtimes of notes");
    assert_eq!(other, other_after, "bytes and mtimes of other");
    assert_eq!(names(&d), names_before);
    assert_eq!(d.entries().len(), entries_before);
    assert!(
        client.drain(Duration::from_millis(500)).is_empty(),
        "no push"
    );
    d.close();
}

#[test]
fn renaming_to_the_same_name_is_a_no_op_success() {
    let d = promoted();
    let (notes, other) = tree(&d);
    let entries_before = d.entries().len();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let out = rename(&d, "tephrad", "tephrad").expect("a no-op is a success");
    assert!(out.unchanged);
    assert_eq!(out.files_changed, 0);
    assert_eq!(tree(&d), (notes, other));
    assert_eq!(d.entries().len(), entries_before);
    assert!(
        client.drain(Duration::from_millis(500)).is_empty(),
        "no push"
    );
    // An unknown name is still refused first.
    assert!(matches!(
        rename(&d, "ghost", "ghost"),
        Err(RegistryError::Rename(RenameError::UnknownSource { .. }))
    ));
    d.close();
}

#[test]
fn a_new_name_other_links_already_use_and_a_unicode_name_are_fine() {
    let d = Daemon::start(
        &[
            ("A.md", "# A\n\n[[@zeta:Gone]] and [[@other:B]]\n"),
            ("C.md", "# C\n"),
        ],
        &[("B.md", "# B\n\n[[@notes:A]]\n")],
        real_writer(),
        false,
    );
    // `@zeta:` is a prefix nobody owns yet: the rename onto it rewrites the
    // `@other:` links and leaves the `@zeta:` one as it is.
    rename(&d, "other", "zeta").expect("rename onto a name links already use");
    assert_eq!(d.text("A.md"), "# A\n\n[[@zeta:Gone]] and [[@zeta:B]]\n");

    // Now every `@zeta:` link is a link to the source, the old dangling one too.
    rename(&d, "zeta", "démon☃").expect("a Unicode name");
    assert_eq!(
        d.text("A.md"),
        "# A\n\n[[@démon☃:Gone]] and [[@démon☃:B]]\n"
    );
    assert!(names(&d).contains(&"démon☃".to_string()));
    d.close();
}

#[test]
fn a_document_that_links_is_logged_needs_manual_fix_and_never_written() {
    let d = Daemon::start(
        &[("Tephra test.md", "# T\n"), ("café notes.md", "# c\n")],
        &[(
            "Link.md",
            "# L\n\n[[@notes:Tephra test]] and [[@notes:café notes|alias]]\n",
        )],
        real_writer(),
        false,
    );
    let fixtures_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tephra-parser/tests/fixtures/documents");
    for (name, file) in [("report.docx", "headings.docx"), ("text.pdf", "text.pdf")] {
        d.notes
            .write_bytes(name, &std::fs::read(fixtures_dir.join(file)).unwrap());
    }
    d.registry.rescan("notes").unwrap();
    let docs_before: Vec<Vec<u8>> = ["report.docx", "text.pdf"]
        .iter()
        .map(|f| std::fs::read(d.notes.root().join(f)).unwrap())
        .collect();

    let out = rename(&d, "notes", "journal").expect("rename");
    assert_eq!(out.status, "partial", "{out:?}");
    assert_eq!(out.files_changed, 1);
    assert_eq!(out.deferred, 2);
    assert_eq!(
        d.other_text("Link.md"),
        "# L\n\n[[@journal:Tephra test]] and [[@journal:café notes|alias]]\n"
    );
    for (f, before) in ["report.docx", "text.pdf"].iter().zip(&docs_before) {
        assert_eq!(
            &std::fs::read(d.notes.root().join(f)).unwrap(),
            before,
            "{f}"
        );
    }
    let cache = reopen(&d);
    let pending = cache.pending_fixes(None, None).unwrap();
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert!(pending.iter().all(|p| p.state == "needs_manual_fix"));
    let entries = rename_entries(&d);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "partial");

    // No retry loop: a pass at start finds nothing it may write.
    d.engine.resume().expect("resume");
    for (f, before) in ["report.docx", "text.pdf"].iter().zip(&docs_before) {
        assert_eq!(
            &std::fs::read(d.notes.root().join(f)).unwrap(),
            before,
            "{f}"
        );
    }
    assert_eq!(rename_entries(&d).len(), 1);
    d.close();
}

#[test]
fn e21_a_read_only_linking_file_is_left_for_a_person_and_the_others_are_rewritten() {
    let d = promoted();
    let locked = d.notes_root().join("docs/Design.md");
    let before = std::fs::read_to_string(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
    let linking = fixtures::tree_snapshot(d.notes.root())
        .into_iter()
        .chain(fixtures::tree_snapshot(d.other.root()))
        .filter(|(_, (b, _))| String::from_utf8_lossy(b).contains("[[@tephrad:"))
        .count() as i64;

    let out = rename(&d, "tephrad", "daemon").expect("rename");
    assert_eq!(out.status, "partial", "{out:?}");
    assert_eq!(out.deferred, 1);
    assert_eq!(out.files_changed, linking - 1);
    assert_eq!(
        std::fs::read_to_string(&locked).unwrap(),
        before,
        "never written"
    );
    assert!(d
        .other_text("Vault.md")
        .contains("[[@daemon:src/registry.rs#^k3f9a2]]"));
    assert!(d
        .text("README.md")
        .contains("[[@daemon:src/scan.rs|the scan]]"));

    let cache = reopen(&d);
    let pending = cache.pending_fixes(None, None).unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].file_path, "docs/Design.md");
    assert_eq!(pending[0].state, "needs_manual_fix");

    // No retry loop starts: another pass leaves it alone, one entry stays.
    d.engine.resume().expect("resume");
    assert_eq!(std::fs::read_to_string(&locked).unwrap(), before);
    assert_eq!(rename_entries(&d).len(), 1);
    assert_eq!(rename_entries(&d)[0].status, "partial");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    d.close();
}

fn stopped(result: &Result<RenameOutcome, RegistryError>) -> bool {
    matches!(result, Err(RegistryError::Fix(m)) if m.contains(&FixError::Stopped.to_string()))
}

const LINKING: [&str; 7] = [
    "README.md",
    "docs/Design.md",
    "crates/tephra-cache/src/lib.rs",
    "crates/tephrad/src/registry.rs",
    "crates/tephrad/src/scan.rs",
    "crates/tephrad/tephrad.md",
    "Vault.md",
];

fn texts(d: &Daemon) -> Vec<String> {
    LINKING
        .iter()
        .map(|f| {
            if *f == "Vault.md" {
                d.other_text(f)
            } else {
                d.text(f)
            }
        })
        .collect()
}

#[test]
fn e9_killed_after_the_transaction_the_rest_resume_and_a_second_rename_is_a_no_op() {
    // The reference: the same rename with no kill.
    let whole = promoted();
    rename(&whole, "tephrad", "daemon").expect("rename");
    let want = texts(&whole);
    whole.close();

    let d = promoted();
    let original = texts(&d);
    let stopper = FixEngine::new(&d.cache_path, real_writer()).with_stop_after_writes(1);
    let result = d.registry.rename("tephrad", "daemon", &stopper);
    assert!(stopped(&result), "{result:?}");

    // The cache is settled (the name moved, one entry, the list stored).
    let cache = reopen(&d);
    assert!(cache.get_source("tephrad").unwrap().is_none());
    assert!(cache.get_source("daemon").unwrap().is_some());
    assert!(!cache.pending_fixes(None, None).unwrap().is_empty());
    let after_kill = texts(&d);
    let changed: Vec<usize> = (0..LINKING.len())
        .filter(|i| after_kill[*i] != original[*i])
        .collect();
    assert_eq!(changed.len(), 1, "exactly one file was written");

    d.engine.resume().expect("resume");
    let settled = texts(&d);
    assert_eq!(settled, want, "converges on what an unbroken run writes");
    assert_eq!(
        settled[changed[0]], after_kill[changed[0]],
        "the file done before the kill is untouched"
    );
    assert!(cache.pending_fixes(None, None).unwrap().is_empty());
    assert_eq!(
        rename_entries(&d).len(),
        1,
        "one entry for the whole rename"
    );

    // The same rename again, as a client that never saw the answer sends
    // it: `tephrad` no longer exists, `daemon` is what it became.
    let again = rename(&d, "tephrad", "daemon").expect("a no-op success");
    assert!(again.unchanged);
    assert_eq!(
        (
            again.old_name.as_str(),
            again.new_name.as_str(),
            again.files_changed
        ),
        ("tephrad", "daemon", 0)
    );
    d.engine.resume().expect("resume again");
    assert_eq!(texts(&d), settled, "a second pass writes nothing");
    assert_eq!(rename_entries(&d).len(), 1);
    assert_eq!(names(&d), vec!["daemon", "notes", "other"]);
    // The ordinary same-name no-op still holds.
    assert!(rename(&d, "daemon", "daemon").expect("same name").unchanged);
    d.close();
}

#[test]
fn a_repeat_of_a_committed_rename_is_a_silent_no_op_but_an_unregistered_name_is_unknown() {
    let d = promoted();
    rename(&d, "tephrad", "daemon").expect("rename");
    let (notes, other) = tree(&d);
    let entries_before = d.entries().len();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let again = rename(&d, "tephrad", "daemon").expect("the retry is a no-op success");
    assert!(again.unchanged);
    assert_eq!(tree(&d), (notes, other), "nothing on disk changed");
    assert_eq!(d.entries().len(), entries_before, "no second log entry");
    assert!(
        client.drain(Duration::from_millis(500)).is_empty(),
        "no push"
    );

    // Only the pair that was renamed counts: another new name, or a name
    // that was never a source, is still unknown.
    assert!(matches!(
        rename(&d, "tephrad", "notes"),
        Err(RegistryError::Rename(RenameError::UnknownSource { .. }))
    ));
    assert!(matches!(
        rename(&d, "ghost", "daemon"),
        Err(RegistryError::Rename(RenameError::UnknownSource { .. }))
    ));

    // A source that was unregistered is unknown, even under its new name's
    // old spelling.
    d.registry.unregister("daemon").expect("unregister");
    assert!(matches!(
        rename(&d, "tephrad", "daemon"),
        Err(RegistryError::Rename(RenameError::UnknownSource { .. }))
    ));
    assert!(matches!(
        rename(&d, "daemon", "tephrad"),
        Err(RegistryError::Rename(RenameError::UnknownSource { .. }))
    ));
    d.close();
}

#[test]
fn a_four_thousand_link_file_is_rewritten_whole_in_one_call() {
    let mut body = String::from("# Big\n\n");
    for i in 0..4000 {
        body.push_str(&format!(
            "[[@other:Vault#^id{i:04}|link {i}]] and text {i}\n"
        ));
    }
    let d = Daemon::start(
        &[("Big.md", &body)],
        &[("Vault.md", "# Vault\n")],
        real_writer(),
        false,
    );
    let out = rename(&d, "other", "vault").expect("rename");
    assert_eq!(out.files_changed, 1);
    assert_eq!(out.status, "applied");
    let text = d.text("Big.md");
    assert!(!text.contains("[[@other:"));
    assert_eq!(text.matches("[[@vault:Vault#^id").count(), 4000);
    assert_eq!(text, body.replace("[[@other:", "[[@vault:"));
    d.close();
}
