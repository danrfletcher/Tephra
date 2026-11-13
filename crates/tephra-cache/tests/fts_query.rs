//! G10: the `^^` query over `block_fts`, through the cache's own writer
//! and reader: ranking (exact symbol/heading name first, then bm25, then
//! shorter path, path, source, line), the escaping rule (user text is
//! never FTS5 syntax), the `LIKE` fallback under three characters, the
//! limit, Unicode, offline flags and snippets.

mod common;

use common::open_temp_cache;
use tephra_cache::{Cache, NewFtsRow, NewUnit, SourceStatus};

/// Registers `source` once, inserts the unit `rel`, and returns its id.
fn unit(cache: &Cache, source: &str, rel: &str) -> i64 {
    let source_id = match cache.get_source(source).unwrap() {
        Some(row) => row.id,
        None => cache
            .register_source(source, &format!("/{source}"), SourceStatus::Active)
            .unwrap(),
    };
    cache
        .insert_unit(&NewUnit {
            source_id,
            rel_path: rel,
            mtime: Some(1),
            frontmatter_json: None,
            kind: "prose",
            size: Some(10),
            too_large: false,
        })
        .unwrap()
}

fn para(cache: &Cache, unit: i64, line: i64, text: &str) {
    cache
        .insert_fts_rows(
            unit,
            &[NewFtsRow {
                kind: "paragraph",
                line: Some(line),
                text,
                ..Default::default()
            }],
        )
        .unwrap();
}

fn symbol(cache: &Cache, unit: i64, line: i64, kind: &str, name: &str, text: &str) {
    let label = format!("{kind} {name}");
    cache
        .insert_fts_rows(
            unit,
            &[NewFtsRow {
                kind: "symbol",
                line: Some(line),
                name: Some(name),
                label: Some(&label),
                text,
                ..Default::default()
            }],
        )
        .unwrap();
}

/// `(source, rel_path, line)` of each hit, in order.
fn order(cache: &Cache, query: &str, limit: usize) -> Vec<(String, String, Option<i64>)> {
    cache
        .search_all_blocks(query, limit)
        .unwrap()
        .into_iter()
        .map(|h| (h.source, h.rel_path, h.line))
        .collect()
}

fn t(source: &str, rel: &str, line: i64) -> (String, String, Option<i64>) {
    (source.to_string(), rel.to_string(), Some(line))
}

#[test]
fn ranking_rules_table() {
    type Seed = Vec<(
        &'static str,
        &'static str,
        i64,
        Option<(&'static str, &'static str)>,
        &'static str,
    )>;
    // (source, rel_path, line, Some((symbol kind, name)) for a symbol row, text)
    struct Case {
        name: &'static str,
        seed: Seed,
        query: &'static str,
        want: Vec<(&'static str, &'static str, i64)>,
    }
    let long: &'static str = Box::leak(
        format!("walk {}", "filler words that make this row long ".repeat(6)).into_boxed_str(),
    );
    let cases = vec![
        Case {
            name: "a short row with the match beats a long one (bm25)",
            seed: vec![
                (
                    "s",
                    "long.md",
                    1,
                    None,
                    "conflict hides inside a much longer paragraph of text for the test",
                ),
                ("s", "short.md", 1, None, "conflict"),
            ],
            query: "conflict",
            want: vec![("s", "short.md", 1), ("s", "long.md", 1)],
        },
        Case {
            name: "an exact symbol name is boosted over a better bm25 paragraph",
            seed: vec![
                ("s", "a.md", 1, None, "conflict"),
                (
                    "s",
                    "zzz/deep/b.rs",
                    7,
                    Some(("enum", "Conflict")),
                    "Conflict conflict filler words around it to weaken the match",
                ),
            ],
            query: "conflict",
            want: vec![("s", "zzz/deep/b.rs", 7), ("s", "a.md", 1)],
        },
        Case {
            name: "a partial name match is not boosted",
            seed: vec![
                ("s", "a.md", 1, None, "conflict"),
                (
                    "s",
                    "b.rs",
                    3,
                    Some(("enum", "ConflictReason")),
                    "ConflictReason conflict reason",
                ),
            ],
            query: "conflict",
            want: vec![("s", "a.md", 1), ("s", "b.rs", 3)],
        },
        Case {
            name: "ties go to the shorter path",
            seed: vec![
                ("s", "docs/long/path/n.md", 1, None, "same text here"),
                ("s", "n.md", 1, None, "same text here"),
                ("s", "docs/n.md", 1, None, "same text here"),
            ],
            query: "same text",
            want: vec![
                ("s", "n.md", 1),
                ("s", "docs/n.md", 1),
                ("s", "docs/long/path/n.md", 1),
            ],
        },
        Case {
            name: "then the path, then the source",
            seed: vec![
                ("b", "x.md", 1, None, "same text here"),
                ("a", "y.md", 1, None, "same text here"),
                ("a", "x.md", 1, None, "same text here"),
            ],
            query: "same text",
            want: vec![("a", "x.md", 1), ("b", "x.md", 1), ("a", "y.md", 1)],
        },
        Case {
            name: "then the line, in one file",
            seed: vec![
                ("s", "f.md", 30, None, "same text here"),
                ("s", "f.md", 4, None, "same text here"),
                ("s", "f.md", 12, None, "same text here"),
            ],
            query: "same text",
            want: vec![("s", "f.md", 4), ("s", "f.md", 12), ("s", "f.md", 30)],
        },
        Case {
            name: "the exact-name check ignores ASCII case",
            seed: vec![
                ("s", "a.md", 1, None, "walk"),
                ("s", "b.rs", 9, Some(("fn", "WALK")), long),
            ],
            query: "walk",
            want: vec![("s", "b.rs", 9), ("s", "a.md", 1)],
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        let cache = open_temp_cache(&dir);
        for (source, rel, line, sym, text) in &case.seed {
            // One unit per (source, path), however many rows it has.
            let id = match cache
                .get_source(source)
                .unwrap()
                .and_then(|s| cache.get_unit_by_rel_path(s.id, rel).unwrap())
            {
                Some(row) => row.id,
                None => unit(&cache, source, rel),
            };
            match sym {
                Some((kind, name)) => symbol(&cache, id, *line, kind, name, text),
                None => para(&cache, id, *line, text),
            }
        }
        let want: Vec<_> = case.want.iter().map(|(s, r, l)| t(s, r, *l)).collect();
        assert_eq!(order(&cache, case.query, 50), want, "{}", case.name);
    }
}

#[test]
fn the_order_is_repeatable() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    for i in 0..30 {
        let id = unit(&cache, "s", &format!("f{i:02}.md"));
        para(&cache, id, 1, "identical text");
    }
    let first = order(&cache, "identical", 50);
    for _ in 0..5 {
        assert_eq!(order(&cache, "identical", 50), first);
    }
    let paths: Vec<&str> = first.iter().map(|h| h.1.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

#[test]
fn every_fts5_operator_in_user_text_is_plain_text_and_never_an_sqlite_error() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let id = unit(&cache, "s", "a.md");
    for (line, text) in [
        (1, "say \"hi\" now"),
        (2, "key a:b value"),
        (3, "foo* star"),
        (4, "foobar plain"),
        (5, "x -minus y"),
        (6, "call(arg) here"),
        (7, "this NEAR that"),
        (8, "left AND right"),
        (9, "one OR two"),
        (10, "not NOT here"),
        (11, "caret ^id tail"),
        (12, "plus+sign {brace} [bracket] comma, dot."),
        (13, "foo\"bar quote"),
    ] {
        para(&cache, id, line, text);
    }

    let lines = |q: &str| -> Vec<i64> {
        cache
            .search_all_blocks(q, 50)
            .unwrap_or_else(|e| panic!("{q:?} must not be an error: {e}"))
            .into_iter()
            .filter_map(|h| h.line)
            .collect()
    };
    // Each finds exactly the row that contains the text literally.
    assert_eq!(lines("\"hi\""), [1]);
    assert_eq!(lines("a:b"), [2]);
    assert_eq!(
        lines("foo*"),
        [3],
        "`*` is not a prefix operator: foobar is not a hit"
    );
    assert_eq!(lines("-minus"), [5]);
    assert_eq!(lines("(arg)"), [6]);
    assert_eq!(lines("NEAR that"), [7]);
    assert_eq!(lines("AND right"), [8]);
    assert_eq!(lines("OR two"), [9]);
    assert_eq!(lines("NOT here"), [10]);
    assert_eq!(lines("^id"), [11]);
    assert_eq!(lines("foo\"bar"), [13]);
    assert_eq!(lines("{brace}"), [12]);

    // And none of these is ever an error, whatever they match.
    for q in [
        "\"",
        "\"\"",
        "\"unterminated",
        "*",
        "**",
        "-",
        "--",
        ":",
        "::",
        "(",
        ")",
        "()",
        "(((",
        "NEAR",
        "NEAR(",
        "NEAR(a b)",
        "NEAR/2",
        "AND",
        "OR",
        "NOT",
        "a AND",
        "OR b",
        "a OR b",
        "a:",
        ":b",
        "col:val",
        "unit_id:1",
        "text:foo",
        "^",
        "^^",
        "+",
        "{",
        "}",
        "a,b",
        "a*b",
        "-a",
        "\"a\" \"b\"",
        "a\"b",
        "'",
        "''",
        "\\",
        "%",
        "_",
        "a b c d e f g",
        "foo\u{0}bar",
    ] {
        cache
            .search_all_blocks(q, 50)
            .unwrap_or_else(|e| panic!("{q:?} must not be an error: {e}"));
    }
}

#[test]
fn a_blank_query_runs_nothing_and_returns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let id = unit(&cache, "s", "a.md");
    para(&cache, id, 1, "text");
    for q in ["", " ", "\t\n"] {
        assert!(cache.search_all_blocks(q, 50).unwrap().is_empty());
    }
    assert!(cache.search_all_blocks("text", 0).unwrap().is_empty());
}

#[test]
fn queries_under_three_characters_fall_back_to_like_with_the_same_order_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let a = unit(&cache, "s", "b.md");
    let b = unit(&cache, "s", "a.md");
    para(&cache, a, 5, "an ab here");
    para(&cache, b, 2, "AB upper");
    para(&cache, b, 9, "nothing");
    // Case-insensitive for ASCII, ordered like the trigram path.
    assert_eq!(
        order(&cache, "ab", 50),
        [t("s", "a.md", 2), t("s", "b.md", 5)]
    );
    // One character.
    assert_eq!(
        order(&cache, "n", 50).len(),
        2,
        "'an ab here' and 'nothing'"
    );
    // The limit applies.
    assert_eq!(order(&cache, "ab", 1), [t("s", "a.md", 2)]);

    // `%`, `_` and `\` are literal.
    let c = unit(&cache, "s", "c.md");
    para(&cache, c, 1, "100% sure");
    para(&cache, c, 2, "ten sure");
    para(&cache, c, 3, "a_b");
    para(&cache, c, 4, "axb");
    para(&cache, c, 5, "back\\slash");
    let lines_in_c = |q: &str| -> Vec<i64> {
        cache
            .search_all_blocks(q, 50)
            .unwrap()
            .into_iter()
            .filter(|h| h.rel_path == "c.md")
            .filter_map(|h| h.line)
            .collect()
    };
    assert_eq!(lines_in_c("%"), [1]);
    assert_eq!(lines_in_c("a_"), [3]);
    assert_eq!(lines_in_c("\\"), [5]);
    assert_eq!(lines_in_c("0%"), [1]);
}

#[test]
fn the_limit_is_honoured_with_200_matching_rows_on_both_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    for i in 0..200 {
        let id = unit(&cache, "s", &format!("f{i:03}.md"));
        para(&cache, id, 1, "needle in a haystack");
    }
    let hits = order(&cache, "needle", 50);
    assert_eq!(hits.len(), 50);
    // The best 50 of the ordered 200: the first 50 paths.
    let want: Vec<_> = (0..50).map(|i| t("s", &format!("f{i:03}.md"), 1)).collect();
    assert_eq!(hits, want);
    // Same through the LIKE path (two characters).
    assert_eq!(order(&cache, "ne", 50), want);
    assert_eq!(order(&cache, "needle", 7).len(), 7);
    assert_eq!(order(&cache, "needle", 500).len(), 200);
}

#[test]
fn unicode_matches_and_emoji_and_cjk_queries_do_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let id = unit(&cache, "s", "café notes.md");
    para(&cache, id, 1, "we met at the café yesterday");
    para(&cache, id, 2, "日本語のテキストです");
    para(&cache, id, 3, "party 🎉🎉🎉 time");
    let lines = |q: &str| -> Vec<i64> {
        cache
            .search_all_blocks(q, 50)
            .unwrap()
            .into_iter()
            .filter_map(|h| h.line)
            .collect()
    };
    assert_eq!(lines("café"), [1]);
    assert_eq!(lines("日本語"), [2]);
    assert_eq!(lines("日本"), [2], "two characters: the LIKE path");
    assert_eq!(lines("🎉"), [3], "one emoji: the LIKE path");
    assert_eq!(lines("🎉🎉🎉"), [3]);
    assert_eq!(lines("😀😀😀😀"), Vec::<i64>::new());
    // A query that is one multi-byte character, and a mix.
    assert!(cache.search_all_blocks("é", 5).is_ok());
    assert!(cache.search_all_blocks("日本語🎉é", 5).is_ok());
}

#[test]
fn hits_carry_kind_place_block_id_snippet_and_the_offline_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let live = unit(&cache, "live", "n.md");
    cache
        .insert_fts_rows(
            live,
            &[NewFtsRow {
                kind: "paragraph",
                line: Some(4),
                block_id: Some("k3f9a2"),
                text: "a paragraph about conflicts",
                ..Default::default()
            }],
        )
        .unwrap();
    symbol(
        &cache,
        live,
        9,
        "enum",
        "ConflictReason",
        "ConflictReason conflict reason",
    );
    let doc = unit(&cache, "live", "d.pdf");
    cache
        .insert_fts_rows(
            doc,
            &[NewFtsRow {
                kind: "paragraph",
                place: Some("page"),
                place_n: Some(3),
                idx: Some(2),
                text: "page text about conflicts",
                ..Default::default()
            }],
        )
        .unwrap();
    let gone = unit(&cache, "gone", "o.md");
    para(&cache, gone, 1, "an offline conflict");
    cache
        .transition_source_status("gone", SourceStatus::Offline)
        .unwrap();

    let hits = cache.search_all_blocks("conflict", 50).unwrap();
    assert_eq!(hits.len(), 4);
    let by = |rel: &str| hits.iter().find(|h| h.rel_path == rel).unwrap();

    // Two rows in n.md: the paragraph and the symbol.
    let paragraph = hits
        .iter()
        .find(|h| h.rel_path == "n.md" && h.kind == "paragraph")
        .unwrap();
    assert_eq!(paragraph.line, Some(4));
    assert_eq!(paragraph.block_id.as_deref(), Some("k3f9a2"));
    assert_eq!(paragraph.snippet, "a paragraph about conflicts");
    assert!(!paragraph.offline);

    let sym = hits.iter().find(|h| h.kind == "symbol").unwrap();
    assert_eq!(
        sym.snippet, "enum ConflictReason",
        "a symbol shows its label"
    );
    assert_eq!(sym.line, Some(9));
    assert!(sym.block_id.is_none());

    let d = by("d.pdf");
    assert_eq!(
        (
            d.line,
            d.place.as_deref(),
            d.place_n,
            d.idx,
            d.block_id.clone()
        ),
        (None, Some("page"), Some(3), Some(2), None)
    );

    let o = by("o.md");
    assert!(
        o.offline,
        "an offline source's cached rows are searched, flagged"
    );
    assert_eq!(o.source, "gone");
}

#[test]
fn a_long_paragraph_snippet_is_a_window_around_the_match() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let id = unit(&cache, "s", "a.md");
    let text = format!(
        "{} the NEEDLE is here {}",
        "word ".repeat(100),
        "tail ".repeat(100)
    );
    para(&cache, id, 1, &text);
    let hit = &cache.search_all_blocks("needle", 5).unwrap()[0];
    assert!(hit.snippet.contains("NEEDLE"), "{}", hit.snippet);
    assert!(hit.snippet.chars().count() <= 130, "{}", hit.snippet.len());
    assert!(hit.snippet.starts_with("...") && hit.snippet.ends_with("..."));
    assert!(!hit.snippet.contains('\n'));
}

#[test]
fn duplicate_block_ids_in_one_file_are_one_hit_per_occurrence_each_with_its_line() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let id = unit(&cache, "s", "a.md");
    for line in [3, 8] {
        cache
            .insert_fts_rows(
                id,
                &[NewFtsRow {
                    kind: "paragraph",
                    line: Some(line),
                    block_id: Some("dup"),
                    text: "repeated words",
                    ..Default::default()
                }],
            )
            .unwrap();
    }
    let hits = cache.search_all_blocks("repeated", 50).unwrap();
    let got: Vec<(Option<i64>, Option<&str>)> = hits
        .iter()
        .map(|h| (h.line, h.block_id.as_deref()))
        .collect();
    assert_eq!(got, [(Some(3), Some("dup")), (Some(8), Some("dup"))]);
}

/// A source of `units` prose files with five rows each (about
/// `5 * units` rows): four rows of ordinary words that all contain the
/// common words "the paragraph" and "translation", and one that holds
/// "the rare conflict word" in every 997th unit. The last unit also has a
/// symbol named `Paragraph`, the highest rowids in the index.
fn seed_large(cache: &Cache, units: usize) {
    let source_id = cache
        .register_source("big", "/big", SourceStatus::Active)
        .unwrap();
    let tx = cache.connection().unchecked_transaction().unwrap();
    for i in 0..units {
        let rel = format!("dir{}/file{i}.md", i % 100);
        let id = cache
            .insert_unit(&NewUnit {
                source_id,
                rel_path: &rel,
                mtime: None,
                frontmatter_json: None,
                kind: "prose",
                size: Some(10),
                too_large: false,
            })
            .unwrap();
        let text = |n: usize| format!("the paragraph number {n} of file {i} about the translation");
        let texts = [text(1), text(2), text(3), text(4)];
        let mut rows: Vec<NewFtsRow<'_>> = texts
            .iter()
            .enumerate()
            .map(|(n, text)| NewFtsRow {
                kind: "paragraph",
                line: Some(1 + 4 * n as i64),
                text,
                ..Default::default()
            })
            .collect();
        rows.push(NewFtsRow {
            kind: "paragraph",
            line: Some(30),
            text: if i % 997 == 0 {
                "the rare conflict word"
            } else {
                "third paragraph of plain prose"
            },
            ..Default::default()
        });
        if i + 1 == units {
            rows.push(NewFtsRow {
                kind: "symbol",
                line: Some(40),
                name: Some("Paragraph"),
                label: Some("struct Paragraph"),
                text: "Paragraph",
                ..Default::default()
            });
        }
        cache.insert_fts_rows(id, &rows).unwrap();
    }
    tx.commit().unwrap();
}

/// Every query in `queries` answers in under 500 ms and fills the limit.
fn assert_fast(cache: &Cache, queries: &[&str]) {
    for query in queries {
        let start = std::time::Instant::now();
        let hits = cache.search_all_blocks(query, 50).unwrap();
        let elapsed = start.elapsed();
        eprintln!("^^{query}: {elapsed:?}");
        assert_eq!(hits.len(), 50, "{query}");
        assert!(elapsed.as_millis() < 500, "{query:?} took {elapsed:?}");
    }
}

#[test]
fn a_full_text_query_over_a_large_index_is_fast_enough() {
    // E9: 20,000 units x 5 rows (100,000 rows) in a debug build. A rare
    // word, and common words that match every unit (far more than
    // MAX_RANK_CANDIDATES rows), each answer in under 500 ms.
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    seed_large(&cache, 20_000);

    let start = std::time::Instant::now();
    let hits = cache.search_all_blocks("conflict", 50).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(hits.len(), 21, "i = 0, 997, ... below 20,000");
    assert!(elapsed.as_millis() < 500, "took {elapsed:?}");
    assert_fast(&cache, &["the", "tion", "translation", "number"]);
    // A query under three characters (the LIKE fallback) too.
    assert_fast(&cache, &["th", "of"]);
}

#[test]
fn a_symbol_named_like_the_query_ranks_first_even_among_far_more_matches_than_are_ranked() {
    // `Paragraph` has the last rowids of 100,000 rows that all contain
    // "paragraph": it is outside the first MAX_RANK_CANDIDATES matches,
    // yet it is the first hit, because exact names come from their own
    // index and not from the ranked candidates.
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    seed_large(&cache, 20_000);
    const { assert!(4 * 20_000 > tephra_cache::MAX_RANK_CANDIDATES) };
    let hits = cache.search_all_blocks("paragraph", 50).unwrap();
    assert_eq!(hits.len(), 50);
    assert_eq!(hits[0].kind, "symbol");
    assert_eq!(hits[0].rel_path, "dir99/file19999.md");
    assert_eq!(hits[0].snippet, "struct Paragraph");
    assert!(hits[1..].iter().all(|h| h.kind == "paragraph"));
    // The exact-name row is a hit once, not once as exact and again as a match.
    let all: std::collections::HashSet<_> =
        hits.iter().map(|h| (h.rel_path.clone(), h.line)).collect();
    assert_eq!(all.len(), 50);
}

/// E9 at its stated size: 100,000 files x 5 rows (500,000 rows). Slow to
/// seed in a debug build, so run it on request:
/// `cargo test -p tephra-cache --release --test fts_query -- --ignored`.
#[test]
#[ignore = "seeds 500,000 rows; run with --release --ignored"]
fn a_query_over_a_hundred_thousand_files_is_under_500_ms() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    seed_large(&cache, 100_000);
    assert_fast(&cache, &["the", "tion", "paragraph", "translation", "th"]);
    let start = std::time::Instant::now();
    assert_eq!(cache.search_all_blocks("conflict", 50).unwrap().len(), 50);
    assert!(start.elapsed().as_millis() < 500);
}
