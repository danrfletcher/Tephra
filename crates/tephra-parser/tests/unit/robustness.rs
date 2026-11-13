//! Remaining edge & failure cases not tied to one specific extraction
//! category: an empty file parses without error, and non-UTF-8 bytes
//! produce a typed error rather than a panic. The multi-megabyte,
//! bounded-time/bounded-memory case lives in `tests/unit/large_file.rs`
//! (see `tephra_parser::RECOMMENDED_MAX_FILE_BYTES`'s doc comment).

use std::path::Path;

use tephra_parser::{parse_unit, parse_unit_bytes};

#[test]
fn empty_file_parses_to_an_empty_parsed_unit_without_error_or_panic() {
    let unit = parse_unit(Path::new("empty.md"), "");

    assert!(unit.frontmatter.is_none());
    assert!(unit.headings.is_empty());
    assert!(unit.wikilinks.is_empty());
    assert!(unit.markdown_links.is_empty());
    assert!(unit.tags.is_empty());
    assert!(unit.blocks.is_empty());
    assert!(unit.diagnostics.is_empty());
}

#[test]
fn non_utf8_bytes_produce_a_typed_error_not_a_panic() {
    let invalid = vec![0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72]; // "foo" + invalid bytes + "bar"
    let result = parse_unit_bytes(Path::new("binary.md"), &invalid);

    let err = result.expect_err("non-UTF-8 input must be a typed error, not a panic or Ok");
    assert_eq!(err.valid_up_to, 3);
}

#[test]
fn valid_utf8_bytes_parse_the_same_as_the_str_entry_point() {
    let content = "# Heading\n\nSome #tag here.";
    let via_bytes = parse_unit_bytes(Path::new("x.md"), content.as_bytes()).expect("valid utf8");
    let via_str = parse_unit(Path::new("x.md"), content);
    assert_eq!(via_bytes, via_str);
}

// ---- G3: every prose format ----

const PROSE_FILES: [&str; 6] = ["r.md", "r.txt", "r.org", "r.rst", "r.adoc", "R.TXT"];

/// Deterministic pseudo-random text built from the constructs every
/// format's rules key on, so the generated input actually reaches the
/// heading, literal-region, link and anchor code (not just random
/// letters).
fn noisy(seed: u64, len: usize) -> String {
    const PIECES: [&str; 40] = [
        "*",
        "**",
        "* ",
        "= ",
        "== ",
        "=",
        "====",
        "----",
        "....",
        "::",
        ".. code-block::",
        "#+BEGIN_SRC",
        "#+END_SRC",
        "#+begin_example",
        "[[",
        "]]",
        "][",
        "[[a|b]]",
        "[t](p.md)",
        "^id",
        " ^id",
        "#tag",
        "`",
        "```",
        "\n",
        "\n",
        "\n\n",
        "\r\n",
        "  ",
        "\t",
        "    ",
        "é",
        "☕",
        "日本",
        "\u{feff}",
        "a",
        "word ",
        "<<x>>",
        "|===",
        "+--+",
    ];
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut out = String::new();
    while out.len() < len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push_str(PIECES[((state >> 33) as usize) % PIECES.len()]);
    }
    out
}

#[test]
fn arbitrary_valid_utf8_never_panics_in_any_prose_format() {
    for seed in 0..200u64 {
        let content = noisy(seed, 600);
        for file in PROSE_FILES {
            let unit = parse_unit(Path::new(file), &content);
            // Whatever came out, line numbers stay inside the file.
            let lines = content.matches('\n').count() + 1;
            for p in &unit.paragraphs {
                assert!(
                    p.start_line >= 1 && p.start_line <= p.end_line,
                    "{file} seed {seed}"
                );
                assert!(p.end_line <= lines, "{file} seed {seed}");
            }
            for h in &unit.headings {
                assert!(h.line >= 1 && h.line <= lines, "{file} seed {seed}");
            }
        }
    }
}

#[test]
fn every_non_utf8_prose_input_is_a_typed_error() {
    for file in PROSE_FILES {
        let err = parse_unit_bytes(Path::new(file), &[b'o', b'k', 0xff, 0xfe, b'x'])
            .expect_err("not UTF-8");
        assert_eq!(err.valid_up_to, 2, "{file}");
    }
}

#[test]
fn deeply_nested_org_stars_and_a_huge_rst_adornment_never_panic() {
    let stars = format!("{} deep\n{}\n", "*".repeat(100_000), "*".repeat(10_000));
    let adorn = "=".repeat(10_000);
    let rst = format!("{adorn}\nT\n{adorn}\n\n{adorn}\n");
    for file in PROSE_FILES {
        let _ = parse_unit(Path::new(file), &stars);
        let _ = parse_unit(Path::new(file), &rst);
    }
}

#[test]
fn a_utf8_bom_on_a_non_markdown_prose_file_is_not_text() {
    let unit = parse_unit(Path::new("b.org"), "\u{feff}* Head\nbody\n");
    assert_eq!(unit.headings.len(), 1);
    assert_eq!(unit.headings[0].text, "Head");
    assert_eq!(unit.headings[0].line, 1);
}
