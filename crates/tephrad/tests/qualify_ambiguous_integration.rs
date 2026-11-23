//! G20 / FR16 (the Atlas fallback, `qualify_ambiguous_links`) through the
//! real path: a whole daemon (real registry, cache, graph and write loop
//! over the sources `notes` and `other`), the fix made by the real
//! engine, the text of every file read back from the real disk, and the
//! links resolved again by the real graph after a rescan. The key is
//! handed to the engine the way `main.rs` hands it from the config.
//!
//! Each case runs twice, key off and key on. Off is what every fix did
//! before the setting existed and must not move.

mod fix_world;
mod fixtures;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fix_world::{real_writer, Daemon};
use tephra_cache::Cache;
use tephra_graph::{EdgeTarget, UnitGraph};
use tephrad::fixes::event::RenameEventWire;
use tephrad::rpc::backend::FixBackend;

fn daemon(notes: &[(&str, &str)], other: &[(&str, &str)], qualify_ambiguous_links: bool) -> Daemon {
    Daemon::start_with(notes, other, real_writer(), false, qualify_ambiguous_links)
}

fn graph(d: &Daemon) -> UnitGraph {
    let cache = Cache::open(&d.cache_path).expect("open cache");
    UnitGraph::build_from_cache(&cache).expect("graph")
}

/// Rescans both sources and returns where every link of `file` goes.
fn targets(d: &Daemon, file: &str) -> Vec<EdgeTarget> {
    d.registry.rescan("notes").expect("rescan notes");
    d.registry.rescan("other").expect("rescan other");
    graph(d)
        .get_forward_links(file)
        .into_iter()
        .map(|e| e.to)
        .collect()
}

// ---------------------------------------------------------------
// A rename within one source
// ---------------------------------------------------------------

const INDEX: &str = "plain [[Training Plan]], alias [[Training Plan|the plan]], \
anchor [[Training Plan#Goals]], block [[Training Plan#^k3f9a2]],\n\
embed ![[Training Plan]], ext [[Training Plan.md]], prefixed [[@notes:Training Plan]]\n";

fn rename_notes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Training Plan.md", "# Training Plan\n\n## Goals\n"),
        ("Index.md", INDEX),
    ]
}

fn rename_other() -> Vec<(&'static str, &'static str)> {
    vec![("Plan.md", "# The other plan\n")]
}

/// `Training Plan.md` goes into the folder `docs` as `Plan.md`, in-app
/// (the exact `link.rename_event`).
fn rename_into_docs(d: &Daemon) -> RenameEventWire {
    let from = d.notes_root().join("Training Plan.md");
    let to = d.notes_root().join("docs/Plan.md");
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(&from, &to).expect("rename on disk");
    d.service
        .rename_event(&from.display().to_string(), &to.display().to_string())
        .expect("rename_event")
}

const INDEX_OFF: &str = "plain [[Plan]], alias [[Plan|the plan]], \
anchor [[Plan#Goals]], block [[Plan#^k3f9a2]],\n\
embed ![[Plan]], ext [[Plan.md]], prefixed [[@notes:Plan]]\n";

const INDEX_ON: &str = "plain [[docs/Plan]], alias [[docs/Plan|the plan]], \
anchor [[docs/Plan#Goals]], block [[docs/Plan#^k3f9a2]],\n\
embed ![[docs/Plan]], ext [[docs/Plan.md]], prefixed [[@notes:Plan]]\n";

#[test]
fn a_rename_keeps_the_bare_name_with_the_key_off() {
    let d = daemon(&rename_notes(), &rename_other(), false);
    let reply = rename_into_docs(&d);
    assert!(
        matches!(reply, RenameEventWire::Applied { .. }),
        "{reply:?}"
    );
    assert_eq!(d.text("Index.md"), INDEX_OFF);
    d.close();
}

#[test]
fn a_rename_writes_the_fuller_path_with_the_key_on_and_only_for_a_plain_link() {
    let d = daemon(&rename_notes(), &rename_other(), true);
    let reply = rename_into_docs(&d);
    assert!(
        matches!(reply, RenameEventWire::Applied { .. }),
        "{reply:?}"
    );
    // Alias, anchor, block id, embed and extension are kept; the link
    // that carries `@notes:` keeps the existing rule (no extra path).
    assert_eq!(d.text("Index.md"), INDEX_ON);
    d.close();
}

#[test]
fn with_the_key_on_every_rewritten_link_still_reaches_the_file_in_its_own_source() {
    // F11: a bare `[[Plan]]` would reach `notes` too, and `other`'s
    // `Plan.md` is never what any link reaches. The fuller path changes
    // the spelling, never the target.
    for key in [false, true] {
        let d = daemon(&rename_notes(), &rename_other(), key);
        rename_into_docs(&d);
        let all = targets(&d, "@notes:Index.md");
        assert!(!all.is_empty());
        assert!(
            all.iter()
                .all(|t| *t == EdgeTarget::Resolved("@notes:docs/Plan.md".to_string())),
            "key {key}: {all:?}"
        );
        d.close();
    }
}

#[test]
fn a_name_that_exists_in_no_other_source_is_spelled_as_before_with_the_key_on() {
    let d = daemon(&rename_notes(), &[("Notes.md", "# Notes\n")], true);
    rename_into_docs(&d);
    assert_eq!(d.text("Index.md"), INDEX_OFF);
    d.close();
}

#[test]
fn a_target_at_the_root_of_its_source_stays_bare_with_the_key_on() {
    let d = daemon(&rename_notes(), &rename_other(), true);
    let from = d.notes_root().join("Training Plan.md");
    let to = d.notes_root().join("Plan.md");
    fs::rename(&from, &to).expect("rename on disk");
    d.service
        .rename_event(&from.display().to_string(), &to.display().to_string())
        .expect("rename_event");
    assert_eq!(d.text("Index.md"), INDEX_OFF);
    d.close();
}

#[test]
fn a_second_fix_of_the_same_rename_changes_nothing() {
    let d = daemon(&rename_notes(), &rename_other(), true);
    rename_into_docs(&d);
    let after = d.text("Index.md");
    let from = d.notes_root().join("Training Plan.md");
    let to = d.notes_root().join("docs/Plan.md");
    let _ = d
        .service
        .rename_event(&from.display().to_string(), &to.display().to_string());
    assert_eq!(d.text("Index.md"), after);
    d.close();
}

// ---------------------------------------------------------------
// A file moved to another source (the three rewrites)
// ---------------------------------------------------------------

/// `Ideas/Target.md` of notes goes to `docs/target.md` of other. A third
/// file `archive/target.md` stays in notes: after the move the name
/// exists in `notes` as well as in `other`.
fn move_notes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Ideas/Target.md", "# Target\n\nback to [[Sibling]]\n"),
        ("Sibling.md", "# Sibling\n"),
        ("archive/target.md", "# Old target\n"),
        ("Index.md", "[[Target]] and [[@notes:Ideas/Target|t]]\n"),
    ]
}

fn move_other() -> Vec<(&'static str, &'static str)> {
    vec![(
        "Reader.md",
        "reads [[@notes:Ideas/Target]] and [[@notes:Ideas/Target#Part|p]]\n",
    )]
}

fn move_across(d: &Daemon) {
    let from = d.notes_root().join("Ideas/Target.md");
    let to = d.other.root().join("docs/target.md");
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(&from, &to).expect("move on disk");
    let reply = d
        .service
        .rename_event(&from.display().to_string(), &to.display().to_string())
        .expect("rename_event");
    assert!(
        matches!(reply, RenameEventWire::Applied { .. }),
        "{reply:?}"
    );
}

#[test]
fn a_move_to_another_source_writes_the_bare_name_with_the_key_off() {
    let d = daemon(&move_notes(), &move_other(), false);
    move_across(&d);
    // The links in the source it went to lose their prefix: bare.
    assert_eq!(
        d.other_text("Reader.md"),
        "reads [[target]] and [[target#Part|p]]\n"
    );
    // The links in the source it left name the new source.
    assert_eq!(
        d.text("Index.md"),
        "[[@other:docs/target]] and [[@other:docs/target|t]]\n"
    );
    d.close();
}

#[test]
fn a_move_to_another_source_writes_the_fuller_path_in_the_new_source_with_the_key_on() {
    let d = daemon(&move_notes(), &move_other(), true);
    move_across(&d);
    // `target` also exists in `notes` (archive/target.md): the plain
    // links in `other` carry the path from `other`'s root.
    assert_eq!(
        d.other_text("Reader.md"),
        "reads [[docs/target]] and [[docs/target#Part|p]]\n"
    );
    // A link with the `@other:` prefix is never given extra path.
    assert_eq!(
        d.text("Index.md"),
        "[[@other:docs/target]] and [[@other:docs/target|t]]\n"
    );
    // The fuller path reaches the moved file, in its own source.
    assert!(
        targets(&d, "@other:Reader.md")
            .iter()
            .all(|t| *t == EdgeTarget::Resolved("@other:docs/target.md".to_string())),
        "{:?}",
        targets(&d, "@other:Reader.md")
    );
    d.close();
}

// ---------------------------------------------------------------
// A folder promoted to a source
// ---------------------------------------------------------------

fn promote_notes() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "proj/A.md",
            "# A\n\nsee [[proj/sub/B]] and [[proj/sub/B|the b]]\n",
        ),
        ("proj/sub/B.md", "# B\n"),
    ]
}

fn promote_other() -> Vec<(&'static str, &'static str)> {
    vec![("B.md", "# The other B\n")]
}

#[test]
fn a_promote_respells_a_link_inside_the_folder_by_the_same_rule() {
    // `[[proj/sub/B]]` named a path from the parent's root; from the new
    // source's root it is `sub/B` or `B`. `B.md` also exists in `other`.
    for (key, expect) in [
        (false, "# A\n\nsee [[B]] and [[B|the b]]\n"),
        (true, "# A\n\nsee [[sub/B]] and [[sub/B|the b]]\n"),
    ] {
        let d = daemon(&promote_notes(), &promote_other(), key);
        d.registry
            .promote("proj", d.notes_root().join("proj"), &d.engine)
            .expect("promote");
        assert_eq!(d.text("proj/A.md"), expect, "key {key}");
        d.close();
    }
}

#[test]
fn a_daemon_built_without_the_key_does_not_qualify() {
    // The default of `Daemon::start` is the engine's default: off.
    let d = Daemon::start(&rename_notes(), &rename_other(), real_writer(), false);
    rename_into_docs(&d);
    assert_eq!(d.text("Index.md"), INDEX_OFF);
    d.close();
}

// ---------------------------------------------------------------
// The federated-vault fixtures: a same-named file in two sources
// ---------------------------------------------------------------

fn fixture(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tephra-graph/tests/fixtures/federated-vault")
        .join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn the_shared_fixture_pair_is_qualified_only_with_the_key_on() {
    // `teamA/Shared.md` and `teamB/Shared.md`: one in each source.
    let team_a = fixture("teamA/Shared.md");
    let team_b = fixture("teamB/Shared.md");
    for (key, expect) in [
        (false, "see [[Shared]]\n"),
        (true, "see [[teamA/Shared]]\n"),
    ] {
        let d = daemon(
            &[
                ("Old Shared.md", team_a.as_str()),
                ("Index.md", "see [[Old Shared]]\n"),
            ],
            &[("teamB/Shared.md", team_b.as_str())],
            key,
        );
        let from = d.notes_root().join("Old Shared.md");
        let to = d.notes_root().join("teamA/Shared.md");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(&from, &to).unwrap();
        d.service
            .rename_event(&from.display().to_string(), &to.display().to_string())
            .expect("rename_event");
        assert_eq!(d.text("Index.md"), expect, "key {key}");
        // Never the same-named file of the other source.
        assert!(
            targets(&d, "@notes:Index.md")
                .iter()
                .all(|t| *t == EdgeTarget::Resolved("@notes:teamA/Shared.md".to_string())),
            "key {key}"
        );
        d.close();
    }
}

#[test]
fn the_unicode_fixture_name_is_qualified_in_either_normal_form() {
    // `unicode/café notes.md` (NFC on disk in the fixture): the other
    // source holds the same name decomposed (NFD).
    let text = fixture("unicode/café notes.md");
    let nfc = "unicode/caf\u{e9} notes.md";
    let nfd = "unicode/cafe\u{301} notes.md";
    for other_name in [nfc, nfd] {
        let d = daemon(
            &[
                ("Old.md", text.as_str()),
                ("Index.md", "[[Old]] and [[Old|the café]]\n"),
            ],
            &[(other_name, "# elsewhere\n")],
            true,
        );
        let from = d.notes_root().join("Old.md");
        let to = d.notes_root().join(nfc);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(&from, &to).unwrap();
        d.service
            .rename_event(&from.display().to_string(), &to.display().to_string())
            .expect("rename_event");
        assert_eq!(
            d.text("Index.md"),
            "[[unicode/caf\u{e9} notes]] and [[unicode/caf\u{e9} notes|the café]]\n",
            "other source spells it {other_name:?}"
        );
        d.close();
    }
}

#[test]
fn a_multibyte_name_near_the_extension_is_qualified_on_the_real_path() {
    let d = daemon(
        &[("CV.md", "# CV\n"), ("Index.md", "[[CV]] [[CV|mine]]\n")],
        &[("Résumé—v2.md", "# other\n")],
        true,
    );
    let from = d.notes_root().join("CV.md");
    let to = d.notes_root().join("docs/Résumé—v2.md");
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(&from, &to).unwrap();
    d.service
        .rename_event(&from.display().to_string(), &to.display().to_string())
        .expect("rename_event");
    assert_eq!(
        d.text("Index.md"),
        "[[docs/Résumé—v2]] [[docs/Résumé—v2|mine]]\n"
    );
    d.close();
}

#[test]
fn a_case_only_difference_in_the_other_source_is_not_a_clash() {
    let d = daemon(&rename_notes(), &[("plan.md", "# lower\n")], true);
    rename_into_docs(&d);
    assert_eq!(d.text("Index.md"), INDEX_OFF);
    d.close();
}

#[test]
fn a_name_only_in_an_ignored_directory_or_a_tephraignore_path_is_not_a_clash() {
    // `node_modules/` is ignored by default, `private/` by the other
    // source's `.tephraignore`: neither file is indexed.
    let d = daemon(
        &rename_notes(),
        &[
            (".tephraignore", "private/\n"),
            ("node_modules/pkg/Plan.md", "# not indexed\n"),
            ("private/Plan.md", "# not indexed\n"),
            ("Kept.md", "# indexed\n"),
        ],
        true,
    );
    rename_into_docs(&d);
    assert_eq!(d.text("Index.md"), INDEX_OFF);
    d.close();
}

#[test]
fn an_offline_other_source_still_counts_and_the_fix_still_writes() {
    use tephra_cache::SourceStatus;
    let d = daemon(&rename_notes(), &rename_other(), true);
    Cache::open(&d.cache_path)
        .unwrap()
        .transition_source_status("other", SourceStatus::Offline)
        .expect("offline");
    let reply = rename_into_docs(&d);
    assert!(
        matches!(reply, RenameEventWire::Applied { .. }),
        "{reply:?}"
    );
    assert_eq!(d.text("Index.md"), INDEX_ON);
    d.close();
}

#[test]
fn several_other_sources_give_one_spelling() {
    // The name is in two folders of `other`: one spelling is written,
    // not one per file. (The unit test has a row with two other sources.)
    let d = daemon(
        &rename_notes(),
        &[("a/Plan.md", "# a\n"), ("b/Plan.md", "# b\n")],
        true,
    );
    rename_into_docs(&d);
    assert_eq!(d.text("Index.md"), INDEX_ON);
    d.close();
}

#[test]
fn a_chain_of_renames_is_spelled_from_the_final_path() {
    // E1: A to B, then B to C in the same minute. The links are fixed
    // twice; the second fix works from what the first wrote.
    let d = daemon(
        &[("A.md", "# A\n"), ("Index.md", "[[A]]\n")],
        &[("C.md", "# other C\n")],
        true,
    );
    for (from, to) in [("A.md", "B.md"), ("B.md", "docs/C.md")] {
        let f = d.notes_root().join(from);
        let t = d.notes_root().join(to);
        fs::create_dir_all(t.parent().unwrap()).unwrap();
        fs::rename(&f, &t).unwrap();
        d.service
            .rename_event(&f.display().to_string(), &t.display().to_string())
            .expect("rename_event");
    }
    assert_eq!(d.text("Index.md"), "[[docs/C]]\n");
    d.close();
}

#[test]
fn a_case_only_rename_is_spelled_from_the_final_path() {
    // E11.
    let d = daemon(
        &[("docs/plan.md", "# p\n"), ("Index.md", "[[plan]]\n")],
        &[("Plan.md", "# other\n")],
        true,
    );
    let f = d.notes_root().join("docs/plan.md");
    let t = d.notes_root().join("docs/Plan.md");
    // Two steps, as on a case-insensitive disk (E11).
    let mid = d.notes_root().join("docs/plan-tmp.md");
    fs::rename(&f, &mid).unwrap();
    fs::rename(&mid, &t).unwrap();
    d.service
        .rename_event(&f.display().to_string(), &t.display().to_string())
        .expect("rename_event");
    assert_eq!(d.text("Index.md"), "[[docs/Plan]]\n");
    d.close();
}

// ---------------------------------------------------------------
// Fences
// ---------------------------------------------------------------

#[test]
fn a_fix_edits_link_text_only_and_a_markdown_link_is_untouched_with_the_key_on() {
    // F3 and F7: prose, the heading, a Markdown link and a link to
    // another file are byte for byte as they were; no file moved.
    let before = "# Index\n\nProse mentions Training Plan and Plan.\n\n\
[t](Training%20Plan.md) and [[Training Plan]] and [[Other]]\n\n```\n[[Training Plan]]\n```\n";
    let after = "# Index\n\nProse mentions Training Plan and Plan.\n\n\
[t](Training%20Plan.md) and [[docs/Plan]] and [[Other]]\n\n```\n[[Training Plan]]\n```\n";
    let d = daemon(
        &[
            ("Training Plan.md", "# Training Plan\n"),
            ("Other.md", "# Other\n"),
            ("Index.md", before),
        ],
        &rename_other(),
        true,
    );
    rename_into_docs(&d);
    assert_eq!(d.text("Index.md"), after);
    assert_eq!(d.text("Other.md"), "# Other\n");
    assert_eq!(d.text("docs/Plan.md"), "# Training Plan\n");
    d.close();
}

#[test]
fn no_rpc_method_was_added_for_the_key() {
    // F12: the method list is the PR-3 set (the full list is pinned in
    // `tests/unit/rpc_envelope.rs`); nothing here is named for the key.
    use tephrad::rpc::dispatch::METHOD_NAMES;
    // 18 before atlas-27 PR-5 added `source.add_github`, 19 before PR-7 added
    // `unit.fetch`, 20 before PR-8 added the three cache verbs, 23 before
    // PR-10 added `source.add_drive`.
    assert_eq!(METHOD_NAMES.len(), 24);
    assert!(METHOD_NAMES
        .iter()
        .all(|m| !m.contains("qualif") && !m.contains("config")));
}

// ---------------------------------------------------------------
// The qualify branch exists once
// ---------------------------------------------------------------

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Where `needle` is called, as `file:count`, in the non-comment lines of
/// every `.rs` file of `crates/<krate>/src`.
fn callers(krate: &str, needle: &str) -> Vec<(String, usize)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(krate)
        .join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let n = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(|l| l.matches(needle).count())
            .sum::<usize>();
        if n > 0 {
            let rel = file.strip_prefix(&root).unwrap().display().to_string();
            found.push((rel, n));
        }
    }
    found.sort();
    found
}

#[test]
fn the_qualify_branch_exists_once_and_only_the_spelling_step_reaches_it() {
    // The cross-source fact is asked in one place.
    assert_eq!(
        callers("tephrad", ".name_in_other_source("),
        vec![("fix/mod.rs".to_string(), 1)]
    );
    // The full-path spelling is asked in one place too.
    assert_eq!(
        callers("tephrad", ".full_target("),
        vec![("fix/mod.rs".to_string(), 1)]
    );
    // The qualifying rewrite is picked in one place: `RewriteMode::changes`
    // (its definition is the only other mention).
    assert_eq!(
        callers("tephrad", "compute_fix_changes_qualifying("),
        vec![
            ("refactor/fix_write.rs".to_string(), 1),
            ("refactor/mod.rs".to_string(), 1)
        ]
    );
    // Every fix kind gets its spelling from `FixSpelling`: the callers
    // are the rename fix, the engine's fixes and the event path, and no
    // other code decides `bare_resolves` from the graph.
    assert_eq!(
        callers("tephrad", "shortest_unique_target("),
        vec![("fix/mod.rs".to_string(), 2)],
        "the bare-or-full decision is made inside FixSpelling only"
    );
    // The config value reaches the engine at one place: `main.rs`.
    assert_eq!(
        callers("tephrad", "with_qualify_ambiguous_links("),
        vec![("fixes/mod.rs".to_string(), 1), ("main.rs".to_string(), 1)]
    );
}

// ---------------------------------------------------------------
// The key is read at the start, from the config file only
// ---------------------------------------------------------------

struct Started {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Started {
    fn spawn(home: &Path, config: &Path) -> Started {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tephrad"))
            .arg("--config")
            .arg(config)
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tephrad");
        let stderr = child.stderr.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        Started { child, lines }
    }

    fn has(&self, needle: &str) -> bool {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains(needle))
    }

    fn wait_for(&self, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.has(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// Stops this child (its own PID, by handle) and waits for it.
    fn stop(mut self) -> Vec<String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(50));
        let lines = self.lines.lock().unwrap().clone();
        lines
    }
}

fn config_text(data_dir: &Path, key: Option<&str>) -> String {
    let mut text = format!(
        "listen_addr = \"127.0.0.1:0\"\ndata_dir = \"{}\"\nlog_level = \"info\"\n",
        data_dir.display()
    );
    if let Some(value) = key {
        text.push_str(&format!("qualify_ambiguous_links = {value}\n"));
    }
    text
}

#[test]
fn the_new_key_value_is_read_at_the_next_start_and_a_bad_one_stops_the_start() {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("config.toml");
    let data = home.path().join("data");
    const ON: &str = "qualify_ambiguous_links is on";

    // A key that is absent: the fallback is off.
    fs::write(&config, config_text(&data, None)).unwrap();
    let run = Started::spawn(home.path(), &config);
    assert!(
        run.wait_for("listening on"),
        "{:?}",
        run.lines.lock().unwrap()
    );
    let lines = run.stop();
    assert!(!lines.iter().any(|l| l.contains(ON)), "{lines:?}");

    // Turned on in the file: the next start has it, and says so.
    fs::write(&config, config_text(&data, Some("true"))).unwrap();
    let run = Started::spawn(home.path(), &config);
    assert!(
        run.wait_for("listening on"),
        "{:?}",
        run.lines.lock().unwrap()
    );
    assert!(run.has(ON), "{:?}", run.lines.lock().unwrap());
    run.stop();

    // Turned off again: the start after that is back to off.
    fs::write(&config, config_text(&data, Some("false"))).unwrap();
    let run = Started::spawn(home.path(), &config);
    assert!(
        run.wait_for("listening on"),
        "{:?}",
        run.lines.lock().unwrap()
    );
    let lines = run.stop();
    assert!(!lines.iter().any(|l| l.contains(ON)), "{lines:?}");

    // A string is no boolean: the start fails, names the key, and the
    // user's file is not touched.
    let bad = config_text(&data, Some("\"yes\""));
    fs::write(&config, &bad).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tephrad"))
        .arg("--config")
        .arg(&config)
        .env("HOME", home.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tephrad");
    let status = child.wait().expect("wait");
    let mut message = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut message).unwrap();
    assert!(!status.success());
    assert!(message.contains("qualify_ambiguous_links"), "{message}");
    assert_eq!(fs::read_to_string(&config).unwrap(), bad);
}

#[test]
fn turning_the_key_on_rewrites_no_existing_link() {
    // Links written bare stay bare: the key affects fixes made after the
    // next start, and starting rewrites nothing.
    let notes = [
        ("docs/Plan.md", "# Plan\n"),
        ("Index.md", "[[Plan]] [[docs/Plan]]\n"),
    ];
    let d = daemon(&notes, &rename_other(), true);
    d.registry.rescan("notes").expect("rescan");
    assert_eq!(d.text("Index.md"), "[[Plan]] [[docs/Plan]]\n");
    d.close();
}
