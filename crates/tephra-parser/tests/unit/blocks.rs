//! `tests/unit/blocks.rs`: `^block-id` anchor extraction and
//! duplicate block IDs within one file flagged as a diagnostic.

use std::path::Path;

use tephra_parser::{parse_unit, DiagnosticKind};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/units/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read fixture")
}

#[test]
fn trailing_block_anchor_is_extracted_with_its_line_number() {
    let content = "A paragraph that ends in a block anchor. ^my-block\n\nMore text.";
    let unit = parse_unit(Path::new("b.md"), content);

    assert_eq!(unit.blocks.len(), 1);
    assert_eq!(unit.blocks[0].block_id, "my-block");
    assert_eq!(unit.blocks[0].line, 1);
}

#[test]
fn duplicate_block_ids_within_one_file_are_flagged() {
    let content = fixture("duplicate-blocks.md");
    let unit = parse_unit(Path::new("duplicate-blocks.md"), &content);

    assert_eq!(unit.blocks.len(), 2);
    assert!(unit.blocks.iter().all(|b| b.block_id == "dup-id"));

    let diag = unit
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::DuplicateBlockId)
        .expect("duplicate block id diagnostic");
    assert_eq!(diag.line, unit.blocks[1].line);
}

#[test]
fn anchor_is_kept_when_the_line_starts_with_inline_code() {
    // R6: the caret itself sits outside the excluded (code) range,
    // even though the line *starts* inside one — must not be
    // dropped just because `line_start` falls in `excluded_ranges`.
    let content = "`npm install` fetches deps ^install";
    let unit = parse_unit(Path::new("b.md"), content);

    assert_eq!(unit.blocks.len(), 1);
    assert_eq!(unit.blocks[0].block_id, "install");
}

#[test]
fn anchor_is_kept_when_the_line_starts_with_inline_html() {
    let content = "<span>x</span> text ^spanid";
    let unit = parse_unit(Path::new("b.md"), content);

    assert_eq!(unit.blocks.len(), 1);
    assert_eq!(unit.blocks[0].block_id, "spanid");
}

#[test]
fn anchor_inside_a_multiline_inline_code_span_is_not_recorded() {
    // R6: a caret that merely *looks* standalone on its own line is
    // still inside the excluded range when that range (an inline
    // code span here) started on an earlier line and closes later.
    let content = "x `foo ^incode\nbar` tail";
    let unit = parse_unit(Path::new("b.md"), content);

    assert!(
        unit.blocks.is_empty(),
        "expected no block anchors, got {:?}",
        unit.blocks
    );
}

#[test]
fn anchor_inside_a_multiline_html_comment_is_not_recorded() {
    let content = "y <!-- note ^hid\n--> after";
    let unit = parse_unit(Path::new("b.md"), content);

    assert!(
        unit.blocks.is_empty(),
        "expected no block anchors, got {:?}",
        unit.blocks
    );
}

// ---- G3: paragraph blocks, ID attachment, every occurrence kept ----

/// `(start_line, end_line, block_id)` per paragraph.
fn paras(file: &str, content: &str) -> Vec<(usize, usize, Option<String>)> {
    parse_unit(Path::new(file), content)
        .paragraphs
        .into_iter()
        .map(|p| (p.start_line, p.end_line, p.block_id))
        .collect()
}

fn para(start: usize, end: usize, id: Option<&str>) -> (usize, usize, Option<String>) {
    (start, end, id.map(str::to_string))
}

#[test]
fn markdown_paragraph_line_ranges_and_id_attachment() {
    let src = "first line\nsecond line ^two-line\n\nsingle\n\nlast ^end";
    assert_eq!(
        paras("a.md", src),
        [
            para(1, 2, Some("two-line")),
            para(4, 4, None),
            para(6, 6, Some("end"))
        ]
    );
}

#[test]
fn a_missing_final_newline_does_not_shift_the_last_end_line() {
    for file in ["a.md", "a.txt", "a.org", "a.rst", "a.adoc"] {
        let with = paras(file, "a\n\nb\nc\n");
        let without = paras(file, "a\n\nb\nc");
        assert_eq!(with, without, "{file}");
        assert_eq!(with.last().unwrap().1, 4, "{file}");
    }
}

#[test]
fn markdown_list_items_are_their_own_blocks() {
    let src = "- one ^i1\n- two\n  continued ^i2\n- three\n  - nested ^i4\n\npara\n";
    assert_eq!(
        paras("a.md", src),
        [
            para(1, 1, Some("i1")),
            para(2, 3, Some("i2")),
            para(4, 4, None),
            para(5, 5, Some("i4")),
            para(7, 7, None),
        ]
    );
}

#[test]
fn markdown_fences_tables_frontmatter_and_headings_are_not_paragraphs() {
    let src = "---\ntitle: T\n---\n\n# Heading ^h\n\n```\ncode ^c\n```\n\n| a | b |\n|---|---|\n| 1 | 2 ^t |\n\nreal\n";
    assert_eq!(paras("a.md", src), [para(15, 15, None)]);
}

#[test]
fn an_id_off_a_paragraphs_last_line_stays_a_bare_anchor_with_its_own_row() {
    let src = "# Heading ^h1\n\nline ^mid\nmore text\n\n| a |\n|---|\n| 1 |\n";
    let unit = parse_unit(Path::new("a.md"), src);
    // The heading's ID is an anchor (and only that);
    // the middle-of-paragraph one is an anchor too: the paragraph
    // itself ends on a line without an ID.
    let anchors: Vec<_> = unit
        .blocks
        .iter()
        .map(|b| (b.block_id.as_str(), b.line))
        .collect();
    assert_eq!(anchors, [("h1", 1), ("mid", 3)]);
    assert_eq!(unit.paragraphs.len(), 1);
    assert_eq!(unit.paragraphs[0].block_id, None);

    // The rows written to the cache: the paragraph, and one row per
    // loose anchor.
    let rows: Vec<_> = unit
        .to_new_blocks(1)
        .iter()
        .map(|r| (r.block_id, r.line, r.end_line))
        .collect();
    assert_eq!(
        rows,
        [
            (Some("h1"), 1, Some(1)),
            (None, 3, Some(4)),
            (Some("mid"), 3, Some(3)),
        ]
    );
}

#[test]
fn every_occurrence_of_a_duplicate_id_is_kept_with_its_own_line_in_every_format() {
    for file in ["a.md", "a.txt", "a.org", "a.rst", "a.adoc"] {
        let unit = parse_unit(Path::new(file), "one ^dup\n\ntwo ^dup\n\nthree ^dup\n");
        let lines: Vec<_> = unit.blocks.iter().map(|b| b.line).collect();
        assert_eq!(lines, [1, 3, 5], "{file}");
        let dup_diags = unit
            .diagnostics
            .iter()
            .filter(|d| d.kind == DiagnosticKind::DuplicateBlockId)
            .count();
        assert_eq!(dup_diags, 2, "{file}");
        // And as cache rows: three, each with its own range.
        let rows: Vec<_> = unit
            .to_new_blocks(7)
            .iter()
            .map(|r| (r.block_id, r.line, r.end_line))
            .collect();
        assert_eq!(
            rows,
            [
                (Some("dup"), 1, Some(1)),
                (Some("dup"), 3, Some(3)),
                (Some("dup"), 5, Some(5))
            ],
            "{file}"
        );
    }
}

#[test]
fn txt_org_rst_adoc_paragraphs_are_runs_of_non_blank_lines() {
    for file in ["a.txt", "a.org", "a.rst", "a.adoc"] {
        let src = "alpha\nbeta ^ab\n\n\n  \ngamma\n";
        assert_eq!(
            paras(file, src),
            [para(1, 2, Some("ab")), para(6, 6, None)],
            "{file}"
        );
    }
}

#[test]
fn crlf_and_lf_copies_give_identical_lines_and_ranges() {
    let docs = [
        (
            "a.md",
            "# H\n\nfirst\nsecond ^a\n\n- x\n- y ^b\n\n```\nfence ^no\n```\n\nlast [[L]]\n",
        ),
        ("a.txt", "first\nsecond ^a\n\nthird [[L]]\n"),
        (
            "a.org",
            "* H\nfirst ^a\n\n#+BEGIN_SRC\ncode [[no]] ^no\n#+END_SRC\n\nlast [[L]]\n",
        ),
        (
            "a.rst",
            "Head\n====\n\nfirst ^a\n\n::\n\n    lit [[no]]\n\nlast [[L]]\n",
        ),
        (
            "a.adoc",
            "= H\n\nfirst ^a\n\n----\ncode [[no]] ^no\n----\n\nlast [[L]]\n",
        ),
    ];
    for (file, lf) in docs {
        let crlf = lf.replace('\n', "\r\n");
        let a = parse_unit(Path::new(file), lf);
        let b = parse_unit(Path::new(file), &crlf);
        let shape = |u: &tephra_parser::ParsedUnit| {
            (
                u.headings
                    .iter()
                    .map(|h| (h.level, h.text.clone(), h.line))
                    .collect::<Vec<_>>(),
                u.blocks
                    .iter()
                    .map(|x| (x.block_id.clone(), x.line))
                    .collect::<Vec<_>>(),
                u.paragraphs
                    .iter()
                    .map(|p| (p.start_line, p.end_line, p.block_id.clone()))
                    .collect::<Vec<_>>(),
                u.wikilinks
                    .iter()
                    .map(|w| (w.raw.clone(), w.line))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(shape(&a), shape(&b), "{file}");
        assert!(!shape(&a).2.is_empty(), "{file}");
    }
}

#[test]
fn native_literal_regions_are_skipped_in_every_non_markdown_format() {
    let cases = [
        (
            "a.org",
            "before\n#+BEGIN_SRC rust\n[[no1]] ^no1\n* not heading\n#+END_SRC\nafter [[yes]]\n#+begin_example\n[[no2]]\n#+end_example\n",
            vec![(1, 1), (6, 6)],
        ),
        (
            "a.rst",
            "before\n\nintro::\n\n    [[no1]] ^no1\n\n    more\n\n.. code-block:: python\n   :linenos:\n\n   [[no2]] ^no2\n\nafter [[yes]]\n",
            vec![(1, 1), (3, 3), (14, 14)],
        ),
        (
            "a.adoc",
            "before\n\n----\n[[no1]] ^no1\n= not heading\n----\n\n....\n[[no2]]\n....\n\nafter [[yes]]\n",
            vec![(1, 1), (12, 12)],
        ),
    ];
    for (file, src, want_paras) in cases {
        let unit = parse_unit(Path::new(file), src);
        let raws: Vec<_> = unit.wikilinks.iter().map(|w| w.raw.as_str()).collect();
        assert_eq!(raws, ["yes"], "{file}");
        assert!(unit.blocks.is_empty(), "{file}: {:?}", unit.blocks);
        assert!(unit.headings.is_empty(), "{file}: {:?}", unit.headings);
        let got: Vec<_> = unit
            .paragraphs
            .iter()
            .map(|p| (p.start_line, p.end_line))
            .collect();
        assert_eq!(got, want_paras, "{file}");
    }
}

#[test]
fn an_unterminated_literal_region_runs_to_the_end_of_the_file() {
    for (file, src) in [
        ("a.org", "keep\n#+BEGIN_SRC\n[[no]] ^no\n\nstill code\n"),
        ("a.adoc", "keep\n\n----\n[[no]] ^no\n\nstill code\n"),
        (
            "a.rst",
            "keep\n\n.. code-block::\n\n   [[no]] ^no\n\n   still code\n",
        ),
        ("a.rst", "keep::\n\n    [[no]] ^no\n"),
    ] {
        let unit = parse_unit(Path::new(file), src);
        assert!(unit.wikilinks.is_empty(), "{file}");
        assert!(unit.blocks.is_empty(), "{file}");
        assert_eq!(unit.paragraphs.len(), 1, "{file}: {:?}", unit.paragraphs);
        assert_eq!(unit.paragraphs[0].start_line, 1, "{file}");
    }
}

#[test]
fn an_empty_or_whitespace_only_prose_file_has_nothing() {
    for file in ["a.md", "a.txt", "a.org", "a.rst", "a.adoc"] {
        for src in ["", "\n", "  \n\t\n   ", "\r\n\r\n"] {
            let u = parse_unit(Path::new(file), src);
            assert!(
                u.wikilinks.is_empty() && u.markdown_links.is_empty(),
                "{file}"
            );
            assert!(u.headings.is_empty() && u.blocks.is_empty(), "{file}");
            assert!(u.paragraphs.is_empty(), "{file}: {src:?}");
        }
    }
}

#[test]
fn unicode_link_text_round_trips_byte_for_byte() {
    let src = "voir [[Café ☕ notes|日本語]] et [x](dossier/café.md)\n";
    for file in ["a.txt", "café notes.org", "a.rst", "a.adoc", "a.md"] {
        let unit = parse_unit(Path::new(file), src);
        let raws: Vec<_> = unit.wikilinks.iter().map(|w| w.raw.as_str()).collect();
        assert_eq!(raws, ["Café ☕ notes|日本語"], "{file}");
        assert_eq!(unit.markdown_links[0].target, "dossier/café.md", "{file}");
    }
}

#[test]
fn rst_transitions_and_org_rules_are_not_paragraphs() {
    assert_eq!(
        paras("a.rst", "before\n\n----\n\nafter\n"),
        [para(1, 1, None), para(5, 5, None)]
    );
    assert_eq!(
        paras("a.org", "before\n\n-----\n\nafter\n"),
        [para(1, 1, None), para(5, 5, None)]
    );
    // A dashed line that is part of a text run is text.
    assert_eq!(paras("a.txt", "before\n----\nafter\n"), [para(1, 3, None)]);
}
