//! `line_regions`: the non-paragraph regions of a prose file, by line.

use std::path::Path;

use tephra_parser::{line_regions, parse_unit, LineRegion, RegionKind};

fn regions(path: &str, content: &str) -> Vec<(RegionKind, usize, usize)> {
    line_regions(Path::new(path), content)
        .into_iter()
        .map(|r: LineRegion| (r.kind, r.start_line, r.end_line))
        .collect()
}

#[test]
fn markdown_frontmatter_code_table_and_headings_by_line() {
    let content = "---\ntitle: T\n---\n# Head\n\ntext\n\n```rust\nlet x = 1;\n\nlet y = 2;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nSetext\n======\n\n    indented\n";
    assert_eq!(
        regions("n.md", content),
        vec![
            (RegionKind::Frontmatter, 1, 3),
            (RegionKind::Heading(0), 4, 4),
            (RegionKind::CodeBlock, 8, 12),
            (RegionKind::Table, 14, 16),
            (RegionKind::Heading(1), 18, 19),
            (RegionKind::CodeBlock, 21, 21),
        ]
    );
}

#[test]
fn no_line_of_a_region_is_ever_in_a_paragraph() {
    let content =
        "---\nk: v\n---\n# H\n\npara\none\n\n```\ncode\n```\n\n- item ^id\n\n| a |\n|---|\n| 1 |\n";
    let parsed = parse_unit(Path::new("n.md"), content);
    for r in line_regions(Path::new("n.md"), content) {
        for p in &parsed.paragraphs {
            assert!(
                p.end_line < r.start_line || p.start_line > r.end_line,
                "paragraph {}-{} overlaps {:?}",
                p.start_line,
                p.end_line,
                r
            );
        }
    }
}

#[test]
fn a_crlf_file_has_the_same_line_numbers() {
    let lf = "# H\n\n```\ncode\n```\n";
    let crlf = lf.replace('\n', "\r\n");
    assert_eq!(regions("n.md", lf), regions("n.md", &crlf));
}

#[test]
fn org_rst_adoc_literal_blocks_and_titles() {
    assert_eq!(
        regions(
            "n.org",
            "* Heading\nbody\n\n#+BEGIN_SRC rust\ncode\n#+END_SRC\n"
        ),
        vec![
            (RegionKind::Heading(0), 1, 1),
            (RegionKind::CodeBlock, 4, 6),
        ]
    );
    let rst = regions("n.rst", "Title\n=====\n\nbody\n");
    assert_eq!(rst, vec![(RegionKind::Heading(0), 1, 2)]);
    assert_eq!(
        regions("n.adoc", "== Title\n\nbody\n\n----\ncode\n----\n"),
        vec![
            (RegionKind::Heading(0), 1, 1),
            (RegionKind::CodeBlock, 5, 7),
        ]
    );
}

#[test]
fn a_plain_text_file_has_no_regions() {
    assert!(regions("n.txt", "one\ntwo\n\nthree\n").is_empty());
    assert!(regions("n.md", "").is_empty());
}
