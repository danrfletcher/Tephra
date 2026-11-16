//! `tests/unit/frontmatter_links.rs`: links in frontmatter properties
//! are reported with exact byte spans (G7). A table over every form
//! the rename fix has to rewrite (a quoted string, a list item, an
//! unquoted `up: [[Note]]`, a value nested in a map), each run as it
//! is, with CRLF line endings, with a BOM, and with both: every span
//! must slice to `[[raw]]` of the original bytes.

use std::path::Path;

use tephra_parser::{parse_unit, ParsedUnit};

/// One frontmatter form: the YAML lines (LF) and the raw text of every
/// link they hold, in order.
struct Form {
    name: &'static str,
    yaml: &'static str,
    links: &'static [&'static str],
}

const FORMS: &[Form] = &[
    Form {
        name: "double-quoted string",
        yaml: "related: \"[[Training Plan]]\"\n",
        links: &["Training Plan"],
    },
    Form {
        name: "single-quoted string",
        yaml: "related: '[[Training Plan]]'\n",
        links: &["Training Plan"],
    },
    Form {
        name: "unquoted value",
        yaml: "up: [[Training Plan]]\n",
        links: &["Training Plan"],
    },
    Form {
        name: "list item, block style",
        yaml: "related:\n  - \"[[Training Plan]]\"\n  - \"[[Goals]]\"\n",
        links: &["Training Plan", "Goals"],
    },
    Form {
        name: "list item, unquoted",
        yaml: "related:\n  - [[Training Plan]]\n",
        links: &["Training Plan"],
    },
    Form {
        name: "list, flow style",
        yaml: "related: [\"[[Training Plan]]\", \"[[Goals]]\"]\n",
        links: &["Training Plan", "Goals"],
    },
    Form {
        name: "nested map value",
        yaml: "project:\n  plan: \"[[Training Plan]]\"\n  owner:\n    note: [[Dan]]\n",
        links: &["Training Plan", "Dan"],
    },
    Form {
        name: "alias, anchor and block anchor are kept in raw",
        yaml: "a: \"[[Training Plan|plan]]\"\nb: \"[[Training Plan#Goals]]\"\nc: \"[[Training Plan#^k3f9a2]]\"\n",
        links: &[
            "Training Plan|plan",
            "Training Plan#Goals",
            "Training Plan#^k3f9a2",
        ],
    },
    Form {
        name: "@source prefix and extension",
        yaml: "a: \"[[@notes:Training Plan]]\"\nb: \"[[Training Plan.md]]\"\n",
        links: &["@notes:Training Plan", "Training Plan.md"],
    },
    Form {
        name: "two links in one value",
        yaml: "see: \"[[Training Plan]] and [[Goals]]\"\n",
        links: &["Training Plan", "Goals"],
    },
    Form {
        name: "multi-byte text before the link",
        yaml: "titre: \"café — [[Training Plan]]\"\n",
        links: &["Training Plan"],
    },
    Form {
        name: "a quoted # before the link is not a comment",
        yaml: "a: \"# [[Training Plan]]\"\n",
        links: &["Training Plan"],
    },
    Form {
        name: "a YAML comment is not a link",
        yaml: "# see [[Old Note]]\na: \"[[Training Plan]]\" # and [[Other Note]]\n",
        links: &["Training Plan"],
    },
    Form {
        name: "a nested YAML list of numbers is not a link",
        yaml: "matrix: [[1, 2], [3, 4]]\nup: [[Training Plan]]\n",
        links: &["Training Plan"],
    },
];

/// `yaml` as a whole note: the frontmatter, a body with its own link
/// (so a body span is checked next to the frontmatter ones), then the
/// line endings and BOM of the variant.
fn note(yaml: &str, crlf: bool, bom: bool) -> String {
    let body = "# Title\n\nBody link [[Body Target]].\n";
    let mut text = format!("---\n{yaml}---\n{body}");
    if crlf {
        text = text.replace('\n', "\r\n");
    }
    if bom {
        text.insert(0, '\u{FEFF}');
    }
    text
}

fn variants() -> [(&'static str, bool, bool); 4] {
    [
        ("LF", false, false),
        ("CRLF", true, false),
        ("BOM", false, true),
        ("BOM+CRLF", true, true),
    ]
}

fn parse(text: &str) -> ParsedUnit {
    parse_unit(Path::new("Note.md"), text)
}

#[test]
fn every_frontmatter_span_slices_to_its_own_link() {
    for form in FORMS {
        for (variant, crlf, bom) in variants() {
            let text = note(form.yaml, crlf, bom);
            let unit = parse(&text);
            let found: Vec<&str> = unit
                .wikilinks
                .iter()
                .map(|w| w.raw.as_str())
                .filter(|r| *r != "Body Target")
                .collect();
            assert_eq!(found, form.links, "{} / {variant}: links", form.name);
            for w in &unit.wikilinks {
                assert_eq!(
                    &text[w.span.0..w.span.1],
                    format!("[[{}]]", w.raw),
                    "{} / {variant}: span of {:?}",
                    form.name,
                    w.raw
                );
            }
        }
    }
}

#[test]
fn frontmatter_links_come_before_the_body_link_and_report_their_own_line() {
    let text = note(
        "up: [[Training Plan]]\nrelated:\n  - \"[[Goals]]\"\n",
        false,
        false,
    );
    let unit = parse(&text);
    let order: Vec<(&str, usize)> = unit
        .wikilinks
        .iter()
        .map(|w| (w.raw.as_str(), w.line))
        .collect();
    // Line 1 is the opening `---`; the YAML starts on line 2.
    assert_eq!(
        order,
        vec![("Training Plan", 2), ("Goals", 4), ("Body Target", 8)]
    );
}

#[test]
fn spans_are_exact_after_a_multibyte_bom_note_with_crlf() {
    let text = note("titre: \"café — [[Training Plan]]\"\n", true, true);
    let unit = parse(&text);
    let w = &unit.wikilinks[0];
    assert_eq!(w.raw, "Training Plan");
    // Rewriting by span changes only the link bytes.
    let mut edited = text.clone();
    edited.replace_range(w.span.0..w.span.1, "[[Plan]]");
    assert_eq!(edited, text.replace("[[Training Plan]]", "[[Plan]]"));
}

#[test]
fn a_file_without_frontmatter_or_with_an_unterminated_block_has_no_frontmatter_links() {
    let none = parse("up: [[Training Plan]]\n");
    // Not frontmatter: a plain body line, found by the body scan.
    assert_eq!(none.wikilinks.len(), 1);
    assert_eq!(none.wikilinks[0].span, (4, 21));

    let unterminated = parse("---\nup: [[Training Plan]]\n");
    assert!(unterminated.frontmatter.is_none());
    assert_eq!(unterminated.wikilinks.len(), 1);
}

#[test]
fn frontmatter_links_are_stored_as_cache_link_rows() {
    // The cache stores one edge per link, so a frontmatter link is a
    // backlink like any other.
    let unit = parse(&note("up: [[Training Plan]]\n", false, false));
    let rows = unit.to_new_links(1);
    let raws: Vec<&str> = rows.iter().map(|r| r.to_raw).collect();
    assert_eq!(raws, vec!["Training Plan", "Body Target"]);
}
