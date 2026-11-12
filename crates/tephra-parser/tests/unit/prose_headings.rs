//! `tests/unit/prose_headings.rs`: native heading rules for org, rst
//! and adoc, Markdown's unchanged ATX/setext levels, `.txt` having
//! none, and every false-heading case.

use std::path::Path;

use tephra_parser::parse_unit;

/// `(level, text, line)` for each heading of `content` parsed as
/// `file`.
fn headings(file: &str, content: &str) -> Vec<(u8, String, usize)> {
    parse_unit(Path::new(file), content)
        .headings
        .into_iter()
        .map(|h| (h.level, h.text, h.line))
        .collect()
}

fn h(level: u8, text: &str, line: usize) -> (u8, String, usize) {
    (level, text.to_string(), line)
}

#[test]
fn markdown_keeps_atx_and_setext_levels() {
    let src = "# One\n\nTwo\n===\n\nThree\n---\n\n### Four\n";
    assert_eq!(
        headings("a.md", src),
        [
            h(1, "One", 1),
            h(1, "Two", 3),
            h(2, "Three", 6),
            h(3, "Four", 9)
        ]
    );
}

#[test]
fn txt_has_no_headings_whatever_it_looks_like() {
    let src = "# Not\n\n* Not\n\n= Not\n\nNot\n===\n";
    assert!(headings("a.txt", src).is_empty());
    assert!(headings("NOTES.TXT", src).is_empty());
}

#[test]
fn org_level_is_the_number_of_leading_stars_followed_by_a_space() {
    let src = "* One\ntext\n** Two\n*** Three\n****** Six\n******* Seven\n";
    assert_eq!(
        headings("a.org", src),
        [
            h(1, "One", 1),
            h(2, "Two", 3),
            h(3, "Three", 4),
            h(6, "Six", 5),
            h(7, "Seven", 6)
        ]
    );
    assert_eq!(headings("Plan.Org", "* Upper\n"), [h(1, "Upper", 1)]);
}

#[test]
fn org_false_headings() {
    for src in [
        "some *bold* text\n",
        "*bold* at line start\n",
        "*NoSpace\n",
        "  * indented item\n",
        "text\n*\n",
        "* \n",
        "a\n * b\n",
    ] {
        assert!(headings("a.org", src).is_empty(), "{src:?}");
    }
}

#[test]
fn adoc_level_is_the_number_of_equals_before_a_space() {
    let src = "= Doc\n\n== Two\n=== Three\n====== Six\n";
    assert_eq!(
        headings("a.adoc", src),
        [
            h(1, "Doc", 1),
            h(2, "Two", 3),
            h(3, "Three", 4),
            h(6, "Six", 5)
        ]
    );
}

#[test]
fn adoc_false_headings() {
    for src in [
        "====\nexample\n====\n",
        "=Title\n",
        "======= Seven\n",
        "= \n",
        "text = more\n",
        " = indented\n",
    ] {
        assert!(headings("a.adoc", src).is_empty(), "{src:?}");
    }
}

#[test]
fn rst_underline_gives_a_heading_at_least_as_wide_as_the_title() {
    let src = "Title\n=====\n\nbody\n\nSub\n---\n";
    assert_eq!(headings("a.rst", src), [h(1, "Title", 1), h(2, "Sub", 6)]);
    // Wider is fine.
    assert_eq!(headings("a.rst", "T\n=======\n"), [h(1, "T", 1)]);
}

#[test]
fn rst_levels_follow_the_order_adornment_styles_first_appear() {
    // `-` appears first, so `-` is level 1 and `=` is level 2, even
    // though `=` is the "usual" top level.
    let src = "First\n-----\n\nSecond\n======\n\nThird\n-----\n\nFourth\n~~~~~~\n";
    assert_eq!(
        headings("a.rst", src),
        [
            h(1, "First", 1),
            h(2, "Second", 4),
            h(1, "Third", 7),
            h(3, "Fourth", 10)
        ]
    );
}

#[test]
fn rst_overline_plus_underline_is_its_own_style() {
    let src = "=====\nTitle\n=====\n\nSection\n=======\n\nSub\n---\n\n=====\nOther\n=====\n";
    assert_eq!(
        headings("a.rst", src),
        [
            h(1, "Title", 2),
            h(2, "Section", 5),
            h(3, "Sub", 8),
            h(1, "Other", 12)
        ]
    );
}

#[test]
fn rst_mixed_adornment_order() {
    // overline `#`, overline `*`, underline `=`, underline `-`, then
    // `*` overline again: levels 1,2,3,4,2.
    let src = "\
#####
Part
#####

*******
Chapter
*******

Section
=======

Subsection
----------

*********
Chapter 2
*********
";
    let got = headings("a.rst", src);
    assert_eq!(
        got,
        [
            h(1, "Part", 2),
            h(2, "Chapter", 6),
            h(3, "Section", 9),
            h(4, "Subsection", 12),
            h(2, "Chapter 2", 16)
        ]
    );
}

#[test]
fn rst_false_headings() {
    for (name, src) in [
        ("transition", "para one\n\n----\n\npara two\n"),
        ("transition after text", "para\n\n--------\n"),
        (
            "table border",
            "+-----+-----+\n| a   | b   |\n+-----+-----+\n",
        ),
        (
            "simple table",
            "=====  =====\nA      B\n=====  =====\n1      2\n=====  =====\n",
        ),
        ("short underline", "A long title here\n=====\n"),
        ("short overlined", "====\nA long title here\n====\n"),
        ("mismatched over/under", "=====\nTitle\n-----\n"),
        (
            "paragraph continuation",
            "first line of a paragraph\nsecond line\n------\n",
        ),
        ("indented title", "  Title\n  =====\n"),
    ] {
        assert!(headings("a.rst", src).is_empty(), "{name}: {src:?}");
    }
}

#[test]
fn heading_lines_never_become_paragraphs_in_any_format() {
    let cases = [
        ("a.org", "* Head\nbody\n", vec![(2, 2)]),
        ("a.adoc", "= Head\nbody\n", vec![(2, 2)]),
        ("a.rst", "Head\n====\nbody\n", vec![(3, 3)]),
        ("a.md", "# Head\nbody\n", vec![(2, 2)]),
        // A `.txt` has no headings, so the same line is just prose.
        ("a.txt", "# Head\nbody\n", vec![(1, 2)]),
    ];
    for (file, src, want) in cases {
        let u = parse_unit(Path::new(file), src);
        let got: Vec<_> = u
            .paragraphs
            .iter()
            .map(|p| (p.start_line, p.end_line))
            .collect();
        assert_eq!(got, want, "{file}");
    }
}

#[test]
fn unicode_heading_text_round_trips() {
    assert_eq!(
        headings("a.org", "* Café ☕ 日本語\n"),
        [h(1, "Café ☕ 日本語", 1)]
    );
    assert_eq!(
        headings("a.adoc", "== Ünïcode — ok\n"),
        [h(2, "Ünïcode — ok", 1)]
    );
    // rst widths are counted in characters, not bytes.
    assert_eq!(headings("a.rst", "Café\n====\n"), [h(1, "Café", 1)]);
    assert_eq!(headings("a.md", "# Café ☕\n"), [h(1, "Café ☕", 1)]);
}

#[test]
fn deeply_nested_org_stars_do_not_panic_and_do_not_wrap_the_level() {
    let src = format!("{} deep\n", "*".repeat(5000));
    let got = headings("a.org", &src);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, u8::MAX);
}

#[test]
fn a_ten_thousand_character_rst_adornment_is_fine() {
    let bar = "=".repeat(10_000);
    let src = format!("Title\n{bar}\n\n{bar}\nOver\n{bar}\n");
    let got = headings("a.rst", &src);
    assert_eq!(got, [h(1, "Title", 1), h(2, "Over", 5)]);
}
