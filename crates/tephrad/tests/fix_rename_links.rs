//! G7 / G8 / GP4 / E11 / E19 / F3 / F4 / F7 through the real path: real
//! files in `fixtures::TempSource` directories, a real `SourceRegistry`
//! scan and rescan into a real cache, the real `UnitGraph` built from
//! that cache, and `tephrad::fix::fix_rename` writing real files. No
//! mocks.
//!
//! Every test renames a file on disk (`fs::rename`), rescans so the
//! cache and graph see the rename the way the watcher will, then calls
//! `fix_rename` with that graph. Expected file contents are written out
//! in full, so a stray byte anywhere fails the test.
//!
//! Run with `--nocapture` to see the three checkpoint outputs
//! (`GP1 BEFORE`, `GP4 DIFF`, `E19 DIFF`).

mod fixtures;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use tephra_cache::Cache;
use tephra_graph::{EdgeTarget, UnitGraph};
use tephrad::fix::{fix_rename, FixResult, FixStatus};
use tephrad::registry::SourceRegistry;

struct World {
    notes: fixtures::TempSource,
    other: fixtures::TempSource,
    cache_path: std::path::PathBuf,
    registry: SourceRegistry,
    _cache_dir: tempfile::TempDir,
}

/// A source `notes` holding `files` and a second source `other`
/// holding `other_files`, both scanned.
fn world(files: &[(&str, &str)], other_files: &[(&str, &str)]) -> World {
    let notes = fixtures::empty_source();
    for (rel, text) in files {
        notes.write(rel, text);
    }
    let other = fixtures::empty_source();
    for (rel, text) in other_files {
        other.write(rel, text);
    }
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("notes", notes.root()).expect("notes");
    registry.register("other", other.root()).expect("other");
    World {
        notes,
        other,
        cache_path,
        registry,
        _cache_dir: cache_dir,
    }
}

impl World {
    fn graph(&self) -> UnitGraph {
        let cache = Cache::open(&self.cache_path).expect("open cache for graph");
        UnitGraph::build_from_cache(&cache).expect("build graph")
    }

    /// Renames `old` to `new` in `notes` on disk and indexes the result.
    fn rename(&self, old: &str, new: &str) {
        let to = self.notes.root().join(new);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(self.notes.root().join(old), to).expect("rename on disk");
        self.registry.rescan("notes").expect("rescan");
    }

    /// The fix for a rename already done and indexed.
    fn fix(&self, old: &str, new: &str) -> FixResult {
        fix_rename(&self.graph(), &self.registry, "notes", old, new).expect("fix_rename")
    }

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.notes.root().join(rel)).expect("read notes file")
    }

    fn other_text(&self, rel: &str) -> String {
        fs::read_to_string(self.other.root().join(rel)).expect("read other file")
    }

    /// After a fix and a rescan: every link in `file` resolves to
    /// `target_id`, none dangles or ties.
    fn assert_links_resolve_to(&self, file: &str, target_id: &str) {
        self.registry.rescan("notes").expect("rescan");
        self.registry.rescan("other").expect("rescan");
        let graph = self.graph();
        let edges = graph.get_forward_links(file);
        assert!(!edges.is_empty(), "{file} has no links");
        for e in edges {
            assert_eq!(
                e.to,
                EdgeTarget::Resolved(target_id.to_string()),
                "{file}: [[{}]] must resolve to {target_id}",
                e.to_raw
            );
        }
    }

    fn finish(self) {
        self.registry.unregister("notes").unwrap();
        self.registry.unregister("other").unwrap();
    }
}

/// Every file under `root` as `rel path -> (bytes, mtime)`.
fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    fixtures::tree_snapshot(root)
}

/// `text` with the inside of every `[[...]]` blanked: what is left is
/// everything that is not link text.
fn mask_links(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start + 2..].find("]]") else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str("[[]]");
        rest = &rest[start + 2 + len + 2..];
    }
    out.push_str(rest);
    out
}

/// F3: between `before` and `after` of every file, only link text
/// differs, and the set of files and their names is the same.
fn assert_only_link_text_changed(
    before: &BTreeMap<String, (Vec<u8>, SystemTime)>,
    after: &BTreeMap<String, (Vec<u8>, SystemTime)>,
) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "no file is added, removed or renamed by a fix"
    );
    for (rel, (old, _)) in before {
        let new = &after[rel].0;
        if old == new {
            continue;
        }
        let (old, new) = (
            String::from_utf8(old.clone()).expect("utf-8"),
            String::from_utf8(new.clone()).expect("utf-8"),
        );
        assert_eq!(
            mask_links(&old),
            mask_links(&new),
            "{rel}: something besides link text changed"
        );
    }
}

/// Prints `diff -u` of one file (falls back to the two texts when
/// `diff` is not available).
fn print_diff(rel: &str, before: &str, after: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    fs::write(&a, before).unwrap();
    fs::write(&b, after).unwrap();
    let out = std::process::Command::new("diff")
        .arg("-u")
        .arg("--label")
        .arg(format!("a/{rel}"))
        .arg("--label")
        .arg(format!("b/{rel}"))
        .arg(&a)
        .arg(&b)
        .output();
    match out {
        Ok(o) => print!("{}", String::from_utf8_lossy(&o.stdout)),
        Err(_) => println!("--- a/{rel}\n{before}\n+++ b/{rel}\n{after}"),
    }
}

fn changed_files(result: &FixResult) -> Vec<&str> {
    result
        .with_status(FixStatus::Changed)
        .iter()
        .map(|f| f.file.as_str())
        .collect()
}

// ---------------------------------------------------------------- GP4

const GP1: [(&str, &str); 5] = [
    ("Training Plan.md", "# Training Plan\n\n## Goals\n\nRun.\n"),
    (
        "Weekly Review.md",
        "# Weekly Review\n\nThis week: [[Training Plan]].\n",
    ),
    ("Goals.md", "# Goals\n\nSee [[Training Plan|plan]] first.\n"),
    (
        "Ideas/Rename Notes.md",
        "# Rename notes\n\n![[Training Plan]]\n",
    ),
    (
        "Unrelated.md",
        "# Unrelated\n\nNo links, mentions Training Plan in prose.\n",
    ),
];

#[test]
fn gp4_renaming_training_plan_rewrites_exactly_the_three_linking_files() {
    let w = world(&GP1, &[]);
    println!("---- GP1 BEFORE ----");
    for rel in ["Weekly Review.md", "Goals.md", "Ideas/Rename Notes.md"] {
        println!("== {rel}\n{}", w.text(rel));
    }
    let before = snapshot(w.notes.root());
    w.rename("Training Plan.md", "Plan.md");

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(result.files_changed, 3, "{result:?}");
    assert_eq!(
        changed_files(&result),
        vec![
            "@notes:Goals.md",
            "@notes:Ideas/Rename Notes.md",
            "@notes:Weekly Review.md"
        ]
    );
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\n"
    );
    assert_eq!(w.text("Goals.md"), "# Goals\n\nSee [[Plan|plan]] first.\n");
    assert_eq!(
        w.text("Ideas/Rename Notes.md"),
        "# Rename notes\n\n![[Plan]]\n"
    );
    // The unlinked file is byte- and mtime-identical.
    let after = snapshot(w.notes.root());
    assert_eq!(after["Unrelated.md"], before["Unrelated.md"]);
    // F3: the renamed file is exactly where the rename left it.
    assert_eq!(after["Plan.md"].0, before["Training Plan.md"].0);
    assert_eq!(after["Plan.md"].1, before["Training Plan.md"].1);
    let mut before_renamed = before.clone();
    let moved = before_renamed.remove("Training Plan.md").unwrap();
    before_renamed.insert("Plan.md".to_string(), moved);
    assert_only_link_text_changed(&before_renamed, &after);

    println!("---- GP4 DIFF ----");
    for rel in ["Weekly Review.md", "Goals.md", "Ideas/Rename Notes.md"] {
        print_diff(rel, &String::from_utf8_lossy(&before[rel].0), &w.text(rel));
    }

    w.assert_links_resolve_to("@notes:Weekly Review.md", "@notes:Plan.md");
    w.assert_links_resolve_to("@notes:Goals.md", "@notes:Plan.md");
    w.assert_links_resolve_to("@notes:Ideas/Rename Notes.md", "@notes:Plan.md");
    w.finish();
}

// ------------------------------------------- G7: every kind in one file

const KITCHEN_SINK_BEFORE: &str = concat!(
    "---\n",
    "title: Everything\n",
    "quoted: \"[[Training Plan]]\"\n",
    "single: '[[Training Plan]]'\n",
    "up: [[Training Plan]]\n",
    "list:\n",
    "  - \"[[Training Plan|plan]]\"\n",
    "  - [[Goals]]\n",
    "nested:\n",
    "  deep:\n",
    "    note: \"[[Training Plan#Goals]]\"\n",
    "# a comment about [[Training Plan]]\n",
    "---\n",
    "# Everything\n",
    "\n",
    "Plain [[Training Plan]] and aliased [[Training Plan|plan]].\n",
    "Heading [[Training Plan#Goals]] and extension [[Training Plan.md]].\n",
    "Block [[Training Plan#^k3f9a2]] and prefix [[@notes:Training Plan]].\n",
    "Embed ![[Training Plan]] and ![[Training Plan#Goals]].\n",
    "Twice [[Training Plan]] then [[Training Plan]] on one line.\n",
    "Decoys: [[PLAN]] [[Training Plans]] [[Goals]] Training Plan in prose.\n",
    "Inline code `[[Training Plan]]` is not a link.\n",
    "Markdown link [text](Training%20Plan.md) and [again](Training Plan.md).\n",
    "\n",
    "```\n",
    "[[Training Plan]] in a fenced block\n",
    "```\n",
);

const KITCHEN_SINK_AFTER: &str = concat!(
    "---\n",
    "title: Everything\n",
    "quoted: \"[[Plan]]\"\n",
    "single: '[[Plan]]'\n",
    "up: [[Plan]]\n",
    "list:\n",
    "  - \"[[Plan|plan]]\"\n",
    "  - [[Goals]]\n",
    "nested:\n",
    "  deep:\n",
    "    note: \"[[Plan#Goals]]\"\n",
    "# a comment about [[Training Plan]]\n",
    "---\n",
    "# Everything\n",
    "\n",
    "Plain [[Plan]] and aliased [[Plan|plan]].\n",
    "Heading [[Plan#Goals]] and extension [[Plan.md]].\n",
    "Block [[Plan#^k3f9a2]] and prefix [[@notes:Plan]].\n",
    "Embed ![[Plan]] and ![[Plan#Goals]].\n",
    "Twice [[Plan]] then [[Plan]] on one line.\n",
    "Decoys: [[PLAN]] [[Training Plans]] [[Goals]] Training Plan in prose.\n",
    "Inline code `[[Training Plan]]` is not a link.\n",
    "Markdown link [text](Training%20Plan.md) and [again](Training Plan.md).\n",
    "\n",
    "```\n",
    "[[Training Plan]] in a fenced block\n",
    "```\n",
);

#[test]
fn g7_every_link_kind_in_one_file_is_rewritten_and_every_decoy_is_not() {
    let w = world(
        &[
            (
                "Training Plan.md",
                "# Training Plan\n\n## Goals\n\nx ^k3f9a2\n",
            ),
            ("Goals.md", "# Goals\n"),
            ("Everything.md", KITCHEN_SINK_BEFORE),
        ],
        &[],
    );
    let before = snapshot(w.notes.root());
    w.rename("Training Plan.md", "Plan.md");

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(w.text("Everything.md"), KITCHEN_SINK_AFTER);
    assert_eq!(changed_files(&result), vec!["@notes:Everything.md"]);
    // 5 in the frontmatter (quoted, single, unquoted, list item, nested)
    // and 10 in the body (2 on each of 5 lines).
    assert_eq!(result.files[0].links_changed, 15, "{result:?}");

    let mut expect_before = before.clone();
    let moved = expect_before.remove("Training Plan.md").unwrap();
    expect_before.insert("Plan.md".to_string(), moved);
    assert_only_link_text_changed(&expect_before, &snapshot(w.notes.root()));
    w.finish();
}

#[test]
fn g7_a_second_run_after_success_writes_nothing_and_keeps_every_mtime() {
    let w = world(&GP1, &[]);
    w.rename("Training Plan.md", "Plan.md");
    // The same graph for both runs: the second sees links the files no
    // longer contain and skips them without an error.
    let graph = w.graph();
    let first = fix_rename(&graph, &w.registry, "notes", "Training Plan.md", "Plan.md").unwrap();
    assert_eq!(first.files_changed, 3);

    let after_first = snapshot(w.notes.root());
    let second = fix_rename(&graph, &w.registry, "notes", "Training Plan.md", "Plan.md").unwrap();
    assert_eq!(second, FixResult::default());
    assert_eq!(snapshot(w.notes.root()), after_first);

    // And on a freshly indexed graph there is nothing left to find.
    w.registry.rescan("notes").unwrap();
    let third = w.fix("Training Plan.md", "Plan.md");
    assert_eq!(third, FixResult::default());
    assert_eq!(snapshot(w.notes.root()), after_first);
    w.finish();
}

#[test]
fn a_rename_nothing_links_to_writes_nothing() {
    let w = world(
        &[
            ("Lonely.md", "# Lonely\n"),
            ("Other.md", "# Other\n[[Lonely2]]\n"),
        ],
        &[],
    );
    let before = snapshot(w.notes.root());
    w.rename("Lonely.md", "Alone.md");
    let result = w.fix("Lonely.md", "Alone.md");
    assert_eq!(result, FixResult::default());
    assert_eq!(snapshot(w.notes.root())["Other.md"], before["Other.md"]);
    w.finish();
}

#[test]
fn a_link_the_file_no_longer_contains_when_re_read_is_skipped_without_an_error() {
    let w = world(&GP1, &[]);
    w.rename("Training Plan.md", "Plan.md");
    let graph = w.graph();
    // Dan edits Goals.md after the graph was built: the link is gone.
    w.notes.write("Goals.md", "# Goals\n\nNo link any more.\n");
    // And Weekly Review.md is deleted.
    w.notes.remove("Weekly Review.md");

    let result =
        fix_rename(&graph, &w.registry, "notes", "Training Plan.md", "Plan.md").expect("no error");

    assert_eq!(changed_files(&result), vec!["@notes:Ideas/Rename Notes.md"]);
    assert_eq!(result.files.len(), 1, "{result:?}");
    assert_eq!(w.text("Goals.md"), "# Goals\n\nNo link any more.\n");
    w.finish();
}

// ------------------------------------------------- G7: @source and code

#[test]
fn g7_source_prefixed_links_are_rewritten_from_the_same_source_and_from_another() {
    let w = world(
        &[
            ("Training Plan.md", "# Training Plan\n"),
            ("Same.md", "see [[@notes:Training Plan]]\n"),
        ],
        &[
            (
                "Cross.md",
                "see [[@notes:Training Plan]] and [[@notes:Training Plan|plan]] and ![[@notes:Training Plan#^k3f9a2]]\n",
            ),
            // Not a link to notes: no prefix means this source's own scope.
            ("Local.md", "# Local\n\n[[Training Plan]]\n"),
        ],
    );
    let local_before = fs::read(w.other.root().join("Local.md")).unwrap();
    w.rename("Training Plan.md", "Plan.md");

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(w.text("Same.md"), "see [[@notes:Plan]]\n");
    assert_eq!(
        w.other_text("Cross.md"),
        "see [[@notes:Plan]] and [[@notes:Plan|plan]] and ![[@notes:Plan#^k3f9a2]]\n"
    );
    assert_eq!(
        fs::read(w.other.root().join("Local.md")).unwrap(),
        local_before
    );
    assert_eq!(
        changed_files(&result),
        vec!["@notes:Same.md", "@other:Cross.md"]
    );
    w.finish();
}

#[test]
fn f4_code_files_are_rewritten_in_comments_only_and_a_string_decoy_stays_byte_for_byte() {
    let py = concat!(
        "# see [[@notes:Training Plan]] for the plan\n",
        "DECOY = \"[[@notes:Training Plan]]\"\n",
        "def run():\n",
        "    return DECOY  # and [[@notes:Training Plan#Goals]]\n",
    );
    let rs = concat!(
        "// see [[@notes:Training Plan]]\n",
        "const DECOY: &str = \"[[@notes:Training Plan]]\";\n",
        "fn f() {}\n",
    );
    let w = world(
        &[("Training Plan.md", "# Training Plan\n\n## Goals\n")],
        &[("tool.py", py), ("lib.rs", rs)],
    );
    w.rename("Training Plan.md", "Plan.md");

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(
        w.other_text("tool.py"),
        concat!(
            "# see [[@notes:Plan]] for the plan\n",
            "DECOY = \"[[@notes:Training Plan]]\"\n",
            "def run():\n",
            "    return DECOY  # and [[@notes:Plan#Goals]]\n",
        )
    );
    assert_eq!(
        w.other_text("lib.rs"),
        concat!(
            "// see [[@notes:Plan]]\n",
            "const DECOY: &str = \"[[@notes:Training Plan]]\";\n",
            "fn f() {}\n",
        )
    );
    assert_eq!(
        changed_files(&result),
        vec!["@other:lib.rs", "@other:tool.py"]
    );
    w.finish();
}

// ------------------------------------------------------- G8 / E19

#[test]
fn e19_two_files_with_the_new_name_give_the_full_stem_which_resolves_to_the_renamed_file() {
    let w = world(
        &[
            ("a/Training Plan.md", "# Plan A\n"),
            ("b/Plan.md", "# Plan B\n"),
            (
                "Ref.md",
                "see [[Training Plan]] and [[Training Plan|alias]]\n",
            ),
        ],
        &[],
    );
    let before = w.text("Ref.md");
    w.rename("a/Training Plan.md", "a/Plan.md");

    let result = w.fix("a/Training Plan.md", "a/Plan.md");

    assert_eq!(w.text("Ref.md"), "see [[a/Plan]] and [[a/Plan|alias]]\n");
    assert_eq!(result.files_changed, 1);
    println!("---- E19 DIFF ----");
    print_diff("Ref.md", &before, &w.text("Ref.md"));
    w.assert_links_resolve_to("@notes:Ref.md", "@notes:a/Plan.md");
    w.finish();
}

#[test]
fn e19_a_shorter_stem_in_the_source_forces_the_path() {
    let w = world(
        &[
            ("Plan.md", "# Root plan\n"),
            ("Archive/Training Plan.md", "# Archived\n"),
            (
                "Ref.md",
                "see [[Training Plan]] and [[Archive/Training Plan]]\n",
            ),
        ],
        &[],
    );
    w.rename("Archive/Training Plan.md", "Archive/Plan.md");

    w.fix("Archive/Training Plan.md", "Archive/Plan.md");

    // A bare [[Plan]] would reach the root file; the path-style link
    // keeps its style.
    assert_eq!(
        w.text("Ref.md"),
        "see [[Archive/Plan]] and [[Archive/Plan]]\n"
    );
    w.assert_links_resolve_to("@notes:Ref.md", "@notes:Archive/Plan.md");
    w.finish();
}

#[test]
fn e19_a_link_the_rename_handed_to_another_file_of_the_same_name_is_rewritten_to_the_renamed_file()
{
    // R1: before the rename `[[Training Plan]]` reached the root file;
    // afterwards it resolves, uniquely, to the archive copy and never
    // dangles. It still has to follow the renamed file.
    let w = world(
        &[
            ("Training Plan.md", "# Plan\n"),
            ("Archive/Training Plan.md", "# Archived\n"),
            (
                "Ref.md",
                "see [[Training Plan]] and [[Training Plan|alias]]\n",
            ),
        ],
        &[],
    );
    w.registry.rescan("notes").expect("rescan");
    assert_eq!(
        w.graph().get_forward_links("@notes:Ref.md")[0].to,
        EdgeTarget::Resolved("@notes:Training Plan.md".to_string()),
        "precondition: the link reaches the root file before the rename"
    );
    w.rename("Training Plan.md", "Plan.md");
    assert_eq!(
        w.graph().get_forward_links("@notes:Ref.md")[0].to,
        EdgeTarget::Resolved("@notes:Archive/Training Plan.md".to_string()),
        "precondition: after the rename the link resolves to the other file"
    );

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(result.files_changed, 1, "{result:?}");
    assert_eq!(w.text("Ref.md"), "see [[Plan]] and [[Plan|alias]]\n");
    w.assert_links_resolve_to("@notes:Ref.md", "@notes:Plan.md");
    w.finish();
}

#[test]
fn e19_a_move_that_leaves_the_old_name_to_another_file_gives_the_full_new_stem() {
    // R1, second shape: the moved file keeps its name, so the new bare
    // name is shadowed by a shorter path. The bare link and the
    // path-style link both end up at the full new stem.
    let w = world(
        &[
            ("Notes/Training Plan.md", "# Plan\n"),
            ("Archive/Deep/Training Plan.md", "# Archived\n"),
            (
                "Ref.md",
                "bare [[Training Plan]] and path [[Notes/Training Plan]]\n",
            ),
        ],
        &[],
    );
    w.rename("Notes/Training Plan.md", "Zeta/Longer Dir/Training Plan.md");

    let result = w.fix("Notes/Training Plan.md", "Zeta/Longer Dir/Training Plan.md");

    assert_eq!(result.files_changed, 1, "{result:?}");
    assert_eq!(
        w.text("Ref.md"),
        "bare [[Zeta/Longer Dir/Training Plan]] and path [[Zeta/Longer Dir/Training Plan]]\n"
    );
    w.assert_links_resolve_to("@notes:Ref.md", "@notes:Zeta/Longer Dir/Training Plan.md");
    w.finish();
}

#[test]
fn a_link_that_already_reached_the_other_file_before_the_rename_is_left_alone() {
    // The shorter `a/Training Plan` won the bare name before the rename
    // and still does: the link never pointed at the renamed file.
    let w = world(
        &[
            ("a/Training Plan.md", "# A\n"),
            ("Archive/x/Training Plan.md", "# X\n"),
            ("Ref.md", "see [[Training Plan]]\n"),
        ],
        &[],
    );
    w.rename("Archive/x/Training Plan.md", "Archive/x/Plan.md");
    let before = snapshot(w.notes.root());

    let result = w.fix("Archive/x/Training Plan.md", "Archive/x/Plan.md");

    assert_eq!(result, FixResult::default());
    assert_eq!(snapshot(w.notes.root()), before);
    w.finish();
}

#[test]
fn e19_a_unique_name_stays_bare_and_the_same_name_in_another_source_does_not_force_a_path() {
    let w = world(
        &[
            ("Archive/Training Plan.md", "# Archived\n"),
            ("Ref.md", "see [[Training Plan]]\n"),
        ],
        &[("Plan.md", "# Another source's Plan\n")],
    );
    w.rename("Archive/Training Plan.md", "Archive/Plan.md");

    w.fix("Archive/Training Plan.md", "Archive/Plan.md");

    assert_eq!(w.text("Ref.md"), "see [[Plan]]\n");
    w.assert_links_resolve_to("@notes:Ref.md", "@notes:Archive/Plan.md");
    w.finish();
}

#[test]
fn moving_into_another_folder_keeps_bare_links_bare_and_gives_path_links_the_full_new_stem() {
    let w = world(
        &[
            ("Notes/Training Plan.md", "# Plan\n"),
            ("Ref.md", "bare [[Training Plan]] and path [[Notes/Training Plan]] and ext [[Notes/Training Plan.md]]\n"),
        ],
        &[],
    );
    w.rename("Notes/Training Plan.md", "Archive/Training Plan.md");

    let result = w.fix("Notes/Training Plan.md", "Archive/Training Plan.md");

    assert_eq!(result.files_changed, 1);
    assert_eq!(
        w.text("Ref.md"),
        "bare [[Training Plan]] and path [[Archive/Training Plan]] and ext [[Archive/Training Plan.md]]\n"
    );
    w.assert_links_resolve_to("@notes:Ref.md", "@notes:Archive/Training Plan.md");
    w.finish();
}

// ------------------------------------------------------------- E11

#[test]
fn e11_a_case_only_rename_rewrites_each_link_once_and_leaves_a_wrong_case_link_alone() {
    let w = world(
        &[
            ("Plan.md", "# Plan\n"),
            (
                "Ref.md",
                "[[Plan]] and [[Plan]] and [[Plan|p]] but not [[PLAN]]\n",
            ),
        ],
        &[],
    );
    w.rename("Plan.md", "plan.md");

    let result = w.fix("Plan.md", "plan.md");

    assert_eq!(
        w.text("Ref.md"),
        "[[plan]] and [[plan]] and [[plan|p]] but not [[PLAN]]\n"
    );
    assert_eq!(result.files[0].links_changed, 3, "no duplicates");
    w.finish();
}

// ------------------------------------------------ Unicode, bytes, F7

#[test]
fn unicode_names_nfd_on_disk_and_nfc_in_the_link_are_matched_and_written_nfc() {
    let nfd_old = "cafe\u{301} notes.md";
    let nfd_new = "cafe\u{301} plan.md";
    let w = world(
        &[
            (nfd_old, "# Café\n"),
            ("Ref.md", "see [[café notes]] and [[café notes|alias]]\n"),
        ],
        &[],
    );
    w.rename(nfd_old, nfd_new);

    let result = w.fix(nfd_old, nfd_new);

    assert_eq!(result.files_changed, 1, "{result:?}");
    // Typed NFC, written NFC, even though the file name on disk is NFD.
    assert_eq!(
        w.text("Ref.md"),
        "see [[caf\u{e9} plan]] and [[caf\u{e9} plan|alias]]\n"
    );
    w.assert_links_resolve_to("@notes:Ref.md", &format!("@notes:{nfd_new}"));
    w.finish();
}

#[test]
fn crlf_a_bom_and_a_missing_final_newline_are_kept_and_only_link_bytes_differ() {
    let before = "\u{feff}---\r\nup: \"[[Training Plan]]\"\r\ntitle: Café\r\n---\r\nBody [[Training Plan]] ünï\r\nlast [[Training Plan|x]]";
    let after = "\u{feff}---\r\nup: \"[[Plan]]\"\r\ntitle: Café\r\n---\r\nBody [[Plan]] ünï\r\nlast [[Plan|x]]";
    let w = world(
        &[
            ("Training Plan.md", "# Training Plan\n"),
            ("Crlf.md", before),
        ],
        &[],
    );
    w.rename("Training Plan.md", "Plan.md");

    w.fix("Training Plan.md", "Plan.md");

    assert_eq!(w.text("Crlf.md").as_bytes(), after.as_bytes());
    w.finish();
}

#[test]
fn f7_plain_markdown_links_are_byte_identical_after_a_fix() {
    let before = "wiki [[Training Plan]] md [text](Training%20Plan.md) and [t2](<Training Plan.md>) and [t3](Training Plan.md#Goals)\n";
    let w = world(
        &[
            ("Training Plan.md", "# Training Plan\n"),
            ("Mixed.md", before),
        ],
        &[],
    );
    w.rename("Training Plan.md", "Plan.md");

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(
        w.text("Mixed.md"),
        "wiki [[Plan]] md [text](Training%20Plan.md) and [t2](<Training Plan.md>) and [t3](Training Plan.md#Goals)\n"
    );
    assert_eq!(result.files[0].links_changed, 1);
    w.finish();
}

// ---------------------------------------------------- self-links

#[test]
fn self_links_in_the_renamed_file_are_rewritten_like_any_other_link_and_counted() {
    let before = concat!(
        "# Training Plan\n",
        "\n",
        "## Goals\n",
        "\n",
        "Para ^k3f9a2\n",
        "\n",
        "Self [[Training Plan]], block [[Training Plan#^k3f9a2]], heading [[Training Plan#Goals]],\n",
        "alias [[Training Plan|me]], embed ![[Training Plan]], same-file [[#Goals]], other [[Goals]].\n",
        "plain [t](Training%20Plan.md) and prose Training Plan stay.\n",
    );
    let after = concat!(
        "# Training Plan\n",
        "\n",
        "## Goals\n",
        "\n",
        "Para ^k3f9a2\n",
        "\n",
        "Self [[Plan]], block [[Plan#^k3f9a2]], heading [[Plan#Goals]],\n",
        "alias [[Plan|me]], embed ![[Plan]], same-file [[#Goals]], other [[Goals]].\n",
        "plain [t](Training%20Plan.md) and prose Training Plan stay.\n",
    );
    // No other referrer.
    let w = world(
        &[("Training Plan.md", before), ("Goals.md", "# Goals\n")],
        &[],
    );
    let goals_before = snapshot(w.notes.root())["Goals.md"].clone();
    w.rename("Training Plan.md", "Plan.md");

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(w.text("Plan.md"), after);
    assert_eq!(result.files_changed, 1, "{result:?}");
    assert_eq!(changed_files(&result), vec!["@notes:Plan.md"]);
    assert_eq!(snapshot(w.notes.root())["Goals.md"], goals_before);
    w.finish();
}

#[test]
fn a_self_link_that_already_resolves_after_the_rename_is_left_untouched() {
    let w = world(&[("Training Plan.md", "# Plan\n\nSelf [[Plan]].\n")], &[]);
    w.rename("Training Plan.md", "Plan.md");
    let before = snapshot(w.notes.root());

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(result, FixResult::default());
    assert_eq!(snapshot(w.notes.root()), before);
    w.finish();
}

#[cfg(unix)]
#[test]
fn a_read_only_renamed_file_is_not_written_and_its_self_link_needs_a_manual_fix() {
    use std::os::unix::fs::PermissionsExt;
    let w = world(
        &[("Training Plan.md", "# Plan\n\nSelf [[Training Plan]].\n")],
        &[],
    );
    w.rename("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Plan.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let before = snapshot(w.notes.root());

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(result.files_changed, 0);
    assert_eq!(result.files.len(), 1, "{result:?}");
    assert_eq!(result.files[0].file, "@notes:Plan.md");
    assert_eq!(result.files[0].status, FixStatus::NeedsManualFix);
    assert!(result.files[0]
        .reason
        .as_deref()
        .unwrap()
        .contains("read-only"));
    assert_eq!(snapshot(w.notes.root()), before);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    w.finish();
}

/// R2: the rule is "owner-write bit off", not "no write bit at all". A
/// mode with the group-write bit set but the owner-write bit off is
/// still read-only here.
#[cfg(unix)]
#[test]
fn a_file_with_owner_write_off_but_group_write_on_is_not_written() {
    use std::os::unix::fs::PermissionsExt;
    let w = world(
        &[
            ("Training Plan.md", "# Plan\n"),
            ("Ref.md", "see [[Training Plan]]\n"),
        ],
        &[],
    );
    w.rename("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Ref.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o464)).unwrap();
    let before = snapshot(w.notes.root());

    let result = w.fix("Training Plan.md", "Plan.md");

    assert_eq!(result.files_changed, 0, "{result:?}");
    assert_eq!(result.files.len(), 1, "{result:?}");
    assert_eq!(result.files[0].file, "@notes:Ref.md");
    assert_eq!(result.files[0].status, FixStatus::NeedsManualFix);
    assert_eq!(snapshot(w.notes.root()), before);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    w.finish();
}

// ------------------------------------------------------- bad calls

#[test]
fn a_call_that_cannot_be_answered_is_an_error_and_writes_nothing() {
    let w = world(&GP1, &[]);
    let graph = w.graph();
    let before = snapshot(w.notes.root());

    // Nothing was renamed: the old path is still a unit.
    let still_there =
        fix_rename(&graph, &w.registry, "notes", "Training Plan.md", "Plan.md").unwrap_err();
    assert!(still_there.contains("not a unit"), "{still_there}");
    // The new path is not in the graph.
    w.rename("Training Plan.md", "Plan.md");
    let graph = w.graph();
    let unknown_new =
        fix_rename(&graph, &w.registry, "notes", "Training Plan.md", "Nope.md").unwrap_err();
    assert!(unknown_new.contains("not a unit"), "{unknown_new}");
    // An unregistered source.
    let unknown =
        fix_rename(&graph, &w.registry, "ghost", "Training Plan.md", "Plan.md").unwrap_err();
    assert!(unknown.contains("no source named"), "{unknown}");
    // The same path twice is a no-op, not an error.
    let same = fix_rename(&graph, &w.registry, "notes", "Plan.md", "Plan.md").unwrap();
    assert_eq!(same, FixResult::default());

    let after = snapshot(w.notes.root());
    for rel in ["Weekly Review.md", "Goals.md", "Ideas/Rename Notes.md"] {
        assert_eq!(after[rel], before[rel], "{rel}");
    }
    w.finish();
}
