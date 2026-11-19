//! G20 (the Atlas fallback, `qualify_ambiguous_links`): how a fix spells
//! a plain link whose bare name also exists in another source.
//!
//! The spelling is decided by `FixSpelling` over an in-memory graph (the
//! name index is the graph's own, filled by `upsert_unit`, so no mock),
//! and the link text it gives is what `RewriteMode::changes` writes:
//! the same two calls the rename fix and the engine make. Every row runs
//! twice, key off and key on, against the exact text of a linking file;
//! a row names what each run writes.

use std::collections::HashMap;
use std::path::Path;

use tephra_graph::{Edge, EdgeTarget, UnitFacts, UnitGraph};
use tephrad::fix::FixSpelling;
use tephrad::refactor::{apply_changes_to_content, move_new_raw, MoveRewrite};

/// The source the renamed file is in; the linking file is in it too.
const OWN: &str = "docs";
const REF_FILE: &str = "@docs:Ref.md";

struct Row {
    name: &'static str,
    /// Files in the other sources, `(source, path)`.
    others: &'static [(&'static str, &'static str)],
    /// Other sources that are offline.
    offline: &'static [&'static str],
    /// Files of `docs` besides the renamed file.
    siblings: &'static [&'static str],
    /// The rename: `old` is gone, `new` is a unit.
    old: &'static str,
    new: &'static str,
    /// The text of the linking file.
    text: &'static str,
    /// What it reads after the fix, key off and key on.
    off: &'static str,
    on: &'static str,
}

fn graph_of(row: &Row) -> UnitGraph {
    let mut graph = UnitGraph::new();
    graph.register_source(OWN);
    for path in row.siblings.iter().chain([&row.new, &"Ref.md"]) {
        graph.upsert_unit(OWN, path, UnitFacts::default());
    }
    for (source, path) in row.others {
        graph.upsert_unit(source, path, UnitFacts::default());
    }
    for source in row.offline {
        graph.set_source_offline(source, true);
    }
    graph
}

/// The edges the graph would hold for `text`: every link of the file, as
/// the parser reads it, dangling (the old name no longer resolves).
fn edges_of(text: &str) -> Vec<Edge> {
    let parsed = tephra_parser::parse_unit(Path::new("Ref.md"), text);
    let wiki = parsed.wikilinks.iter().map(|w| (w.raw.clone(), "wikilink"));
    let md = parsed
        .markdown_links
        .iter()
        .map(|m| (m.target.clone(), "markdown"));
    wiki.chain(md)
        .map(|(to_raw, kind)| Edge {
            from: REF_FILE.to_string(),
            to: EdgeTarget::Dangling,
            to_raw,
            kind: kind.to_string(),
            offline: false,
            line: None,
        })
        .collect()
}

/// What the fix writes into `text`, with the key as given. The edges are
/// those of `row.text` (the links that named the old file, as the graph
/// held them); `text` is the file as it is now, which is `row.text` the
/// first time and the fixed text on a retry.
fn write(row: &Row, text: &str, key: bool) -> String {
    let graph = graph_of(row);
    let id = format!("@{OWN}:{}", row.new);
    let mode = FixSpelling::for_target(&graph, &id, key).rewrite_mode();
    let contents = HashMap::from([(REF_FILE.to_string(), text.to_string())]);
    let changes = mode.changes(&edges_of(row.text), &contents, row.old, row.new);
    let refs: Vec<_> = changes.iter().collect();
    apply_changes_to_content(text, &refs).expect("apply")
}

/// The target `Plan.md` at the root of `docs`, renamed from
/// `Training Plan.md`.
fn plan(
    name: &'static str,
    others: &'static [(&'static str, &'static str)],
    offline: &'static [&'static str],
    siblings: &'static [&'static str],
    on: &'static str,
) -> Row {
    Row {
        name,
        others,
        offline,
        siblings,
        old: "Training Plan.md",
        new: "Plan.md",
        text: "See [[Training Plan]].\n",
        off: "See [[Plan]].\n",
        on,
    }
}

/// The same rename, the renamed file now in the folder `docs/docs`.
fn into_folder(
    name: &'static str,
    others: &'static [(&'static str, &'static str)],
    siblings: &'static [&'static str],
    on: &'static str,
) -> Row {
    Row {
        name,
        others,
        offline: &[],
        siblings,
        old: "Training Plan.md",
        new: "docs/Plan.md",
        text: "See [[Training Plan]].\n",
        off: "See [[Plan]].\n",
        on,
    }
}

fn rows() -> Vec<Row> {
    vec![
        // --- the three derived cases: one other source, two, none ---
        into_folder(
            "clash with one other source: the fuller path",
            &[("other", "Plan.md")],
            &[],
            "See [[docs/Plan]].\n",
        ),
        into_folder(
            "clash with two other sources: one spelling, not one per source",
            &[("other", "Plan.md"), ("third", "sub/Plan.md")],
            &[],
            "See [[docs/Plan]].\n",
        ),
        into_folder(
            "no other source holds the name: as before",
            &[("other", "Notes.md")],
            &[],
            "See [[Plan]].\n",
        ),
        // --- what stays as it was ---
        into_folder(
            "case only differs in the other source: not a clash",
            &[("other", "plan.md")],
            &[],
            "See [[Plan]].\n",
        ),
        into_folder(
            "the other source holds a different file type of that name: not a clash",
            &[("other", "Plan.pdf")],
            &[],
            "See [[Plan]].\n",
        ),
        // --- the target ---
        plan(
            "the target is at its source root: no fuller path, stays bare",
            &[("other", "Plan.md")],
            &[],
            &[],
            "See [[Plan]].\n",
        ),
        // The name is unique in its own source and still clashes outside it.
        into_folder(
            "unique in its own source, same name elsewhere: qualified",
            &[("other", "deep/er/Plan.md")],
            &["docs/Notes.md"],
            "See [[docs/Plan]].\n",
        ),
        // The target is ambiguous in its own source: the full stem is the
        // spelling with the key off, and the key adds nothing.
        Row {
            name: "ambiguous in its own source already: the full stem either way",
            others: &[("other", "Plan.md")],
            offline: &[],
            siblings: &["Plan.md"],
            old: "Training Plan.md",
            new: "sub/dir/Plan.md",
            text: "See [[Training Plan]].\n",
            off: "See [[sub/dir/Plan]].\n",
            on: "See [[sub/dir/Plan]].\n",
        },
        // --- the other source is offline: its cached rows still count ---
        plan(
            "other source offline: its cached rows still count",
            &[("other", "Plan.md")],
            &["other"],
            &[],
            "See [[Plan]].\n", // at the root: nothing fuller
        ),
        Row {
            name: "other source offline, target in a folder: qualified",
            others: &[("other", "Plan.md")],
            offline: &["other"],
            siblings: &[],
            old: "Training Plan.md",
            new: "docs/Plan.md",
            text: "See [[Training Plan]].\n",
            off: "See [[Plan]].\n",
            on: "See [[docs/Plan]].\n",
        },
        // --- every part of the link but the target is kept ---
        Row {
            name: "alias, anchor, block id, embed and a frontmatter link are kept byte for byte",
            others: &[("other", "Plan.md")],
            offline: &[],
            siblings: &[],
            old: "Training Plan.md",
            new: "docs/Plan.md",
            text: concat!(
                "---\nrelated: \"[[Training Plan]]\"\n---\n",
                "[[Training Plan|the plan]] [[Training Plan#Goals]] [[Training Plan#^k3f9a2]]\n",
                "![[Training Plan]] ![[Training Plan#Goals|g]] [[Training Plan.md]]\n",
            ),
            off: concat!(
                "---\nrelated: \"[[Plan]]\"\n---\n",
                "[[Plan|the plan]] [[Plan#Goals]] [[Plan#^k3f9a2]]\n",
                "![[Plan]] ![[Plan#Goals|g]] [[Plan.md]]\n",
            ),
            on: concat!(
                "---\nrelated: \"[[docs/Plan]]\"\n---\n",
                "[[docs/Plan|the plan]] [[docs/Plan#Goals]] [[docs/Plan#^k3f9a2]]\n",
                "![[docs/Plan]] ![[docs/Plan#Goals|g]] [[docs/Plan.md]]\n",
            ),
        },
        Row {
            name: "a link with an @source: prefix is never given extra path",
            others: &[("other", "Plan.md")],
            offline: &[],
            siblings: &[],
            old: "Training Plan.md",
            new: "docs/Plan.md",
            text: "[[@docs:Training Plan]] [[@docs:Training Plan#^k3f9a2|p]] ![[@docs:Training Plan]]\n",
            off: "[[@docs:Plan]] [[@docs:Plan#^k3f9a2|p]] ![[@docs:Plan]]\n",
            on: "[[@docs:Plan]] [[@docs:Plan#^k3f9a2|p]] ![[@docs:Plan]]\n",
        },
        Row {
            name: "a link already written with a path keeps the full stem (both)",
            others: &[("other", "Plan.md")],
            offline: &[],
            siblings: &[],
            old: "old/Training Plan.md",
            new: "docs/Plan.md",
            text: "[[old/Training Plan]] [[Training Plan]]\n",
            off: "[[docs/Plan]] [[Plan]]\n",
            on: "[[docs/Plan]] [[docs/Plan]]\n",
        },
        Row {
            name: "a plain Markdown link is untouched (F7)",
            others: &[("other", "Plan.md")],
            offline: &[],
            siblings: &[],
            old: "Training Plan.md",
            new: "docs/Plan.md",
            text: "[the plan](Training%20Plan.md) and [[Training Plan]]\n",
            off: "[the plan](Training%20Plan.md) and [[Plan]]\n",
            on: "[the plan](Training%20Plan.md) and [[docs/Plan]]\n",
        },
        // --- renames whose spelling follows the final path ---
        Row {
            name: "case-only rename (E11): spelled from the final path",
            others: &[("other", "Plan.md")],
            offline: &[],
            siblings: &[],
            old: "docs/plan.md",
            new: "docs/Plan.md",
            text: "[[plan]]\n",
            off: "[[Plan]]\n",
            on: "[[docs/Plan]]\n",
        },
        Row {
            name: "chain rename (E1) A to C: the middle name is not a clash, C is",
            others: &[("other", "B.md"), ("third", "C.md")],
            offline: &[],
            siblings: &[],
            old: "A.md",
            new: "docs/C.md",
            text: "[[A]]\n",
            off: "[[C]]\n",
            on: "[[docs/C]]\n",
        },
        Row {
            name: "chain rename (E1) A to C: only the middle name exists elsewhere",
            others: &[("other", "B.md")],
            offline: &[],
            siblings: &[],
            old: "A.md",
            new: "docs/C.md",
            text: "[[A]]\n",
            off: "[[C]]\n",
            on: "[[C]]\n",
        },
        // --- multi-byte names near the extension (E26 class) ---
        Row {
            name: "multi-byte name near the extension: qualified, no panic",
            others: &[("other", "Résumé—v2.md")],
            offline: &[],
            siblings: &[],
            old: "CV.md",
            new: "docs/Résumé—v2.md",
            text: "[[CV]] [[CV|my cv]]\n",
            off: "[[Résumé—v2]] [[Résumé—v2|my cv]]\n",
            on: "[[docs/Résumé—v2]] [[docs/Résumé—v2|my cv]]\n",
        },
        Row {
            name: "the other source spells the same name decomposed (NFD): still a clash",
            others: &[("other", "Re\u{301}sume\u{301}—v2.md")],
            offline: &[],
            siblings: &[],
            old: "CV.md",
            new: "docs/Résumé—v2.md",
            text: "[[CV]]\n",
            off: "[[Résumé—v2]]\n",
            on: "[[docs/Résumé—v2]]\n",
        },
        Row {
            name: "a multi-byte character just before the extension: no panic",
            others: &[("other", "Todo…ab.md")],
            offline: &[],
            siblings: &[],
            old: "T.md",
            new: "docs/Todo…ab.md",
            text: "[[T]]\n",
            off: "[[Todo…ab]]\n",
            on: "[[docs/Todo…ab]]\n",
        },
    ]
}

#[test]
fn every_row_writes_what_it_says_with_the_key_off_and_on() {
    for row in rows() {
        assert_eq!(
            write(&row, row.text, false),
            row.off,
            "{}: key off",
            row.name
        );
        assert_eq!(write(&row, row.text, true), row.on, "{}: key on", row.name);
    }
}

#[test]
fn the_key_off_spelling_is_the_one_before_this_pr() {
    // `FixSpelling` with the key off is `RewriteMode::Fix { bare_resolves }`
    // where `bare_resolves` is what the graph said before G20: the same
    // mode, so the same bytes.
    for row in rows() {
        let graph = graph_of(&row);
        let id = format!("@{OWN}:{}", row.new);
        let bare_resolves = graph
            .shortest_unique_target(&id)
            .is_some_and(|t| !t.contains('/'));
        assert_eq!(
            FixSpelling::for_target(&graph, &id, false).rewrite_mode(),
            tephrad::refactor::fix_write::RewriteMode::Fix { bare_resolves },
            "{}",
            row.name
        );
    }
}

#[test]
fn a_retry_with_the_key_on_gives_the_same_text() {
    for row in rows() {
        let once = write(&row, row.text, true);
        // The retry after a deferral starts from the file as it is: the
        // links are rewritten already, so nothing is found to change.
        let again = write(&row, &once, true);
        assert_eq!(again, once, "{}: second run", row.name);
        // And the same fix started again from the original text is the
        // same text (the retry re-derives it from the current bytes).
        assert_eq!(write(&row, row.text, true), once, "{}: rerun", row.name);
    }
}

#[test]
fn the_spelling_is_never_longer_than_the_full_path_and_never_made_up() {
    for row in rows() {
        let graph = graph_of(&row);
        let id = format!("@{OWN}:{}", row.new);
        let full = graph.full_target(&id).expect("the target is a unit");
        for key in [false, true] {
            let text = FixSpelling::target_text(&graph, &id, key).expect("target text");
            assert!(
                text.chars().count() <= full.chars().count(),
                "{}: {text:?} is longer than {full:?}",
                row.name
            );
            assert!(
                full.ends_with(&text),
                "{}: {text:?} is not a tail of {full:?}",
                row.name
            );
        }
        // At the root of its source there is nothing fuller than the name.
        if !full.contains('/') {
            let spelling = FixSpelling::for_target(&graph, &id, true);
            assert!(!spelling.qualify, "{}: root file", row.name);
        }
    }
}

#[test]
fn the_other_sources_names_are_a_fact_about_the_index_not_a_search() {
    // F11: asking only reports; a bare link is resolved by the graph in
    // the linking file's own source, so a name that exists elsewhere does
    // not make a bare `[[Plan]]` reach it.
    let row = &rows()[0];
    let graph = graph_of(row);
    let id = format!("@{OWN}:{}", row.new);
    assert!(graph.name_in_other_source(&id));
    let resolved = graph
        .resolve_link("Plan", Some(OWN))
        .expect("resolves in its own source");
    assert_eq!(resolved.unit.id, id, "never the file in the other source");
    assert!(!graph.name_in_other_source("@docs:Nothing.md"));
}

#[test]
fn a_move_and_a_respelled_link_use_the_same_decision() {
    // Later fix kinds (move, promote, unpromote) take their spelling from
    // `FixSpelling`, not from a copy of the rule.
    let row = &rows()[0];
    let graph = graph_of(row);
    let id = format!("@{OWN}:{}", row.new);
    for (key, bare, respelled) in [(false, "Plan", "Plan"), (true, "docs/Plan", "docs/Plan")] {
        let spelling = FixSpelling::for_target(&graph, &id, key);
        // Rewrite 2 of a move (the file arrived in this source).
        let new_source = MoveRewrite::NewSource {
            new_path: row.new.to_string(),
            bare_resolves: spelling.plain_bare(),
        };
        assert_eq!(
            move_new_raw("@notes:Training Plan#^k3f9a2|p", &new_source).as_deref(),
            Some(format!("{bare}#^k3f9a2|p").as_str()),
            "key {key}: NewSource"
        );
        // A link of a promoted or folded folder, respelled.
        assert_eq!(
            FixSpelling::target_text(&graph, &id, key).as_deref(),
            Some(respelled),
            "key {key}: target text"
        );
        // A link to the moved file itself (SelfLink) is spelled alike.
        let own = MoveRewrite::Own {
            new_path: row.new.to_string(),
            bare_resolves: spelling.plain_bare(),
            links: HashMap::from([(
                "Training Plan".to_string(),
                tephrad::refactor::OwnLink::SelfLink,
            )]),
        };
        assert_eq!(
            move_new_raw("Training Plan", &own).as_deref(),
            Some(bare),
            "key {key}: SelfLink"
        );
    }
}
