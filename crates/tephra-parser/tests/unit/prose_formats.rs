//! `tests/unit/prose_formats.rs`: the G3 matrix of prose format ×
//! construct, driven by the rule rather than a hand-written list per
//! format.
//!
//! The rule, stated once:
//! - wikilinks (plain, alias, heading anchor, federated block link),
//!   Markdown links, `^id` anchors and paragraphs are read from every
//!   prose format, and give the same rows as in Markdown;
//! - `#tag`s are read from Markdown only;
//! - org / rst / adoc native link syntax is read in none of them.
//!
//! Each construct is one source line plus the rows the *Markdown*
//! reading of it gives. [`expected`] derives what every other format
//! must give by applying the rule. Every case goes through the real
//! `parse_unit` and the real `to_new_links` row builder.

use std::path::Path;

use tephra_parser::{parse_unit, ParsedUnit, ProseFormat};

/// One file name per format, with the case variants the spec names.
/// (The format is what the rule is keyed on; the name only has to
/// select it.)
const FILES: [(&str, ProseFormat); 9] = [
    ("a.md", ProseFormat::Markdown),
    ("a.markdown", ProseFormat::Markdown),
    ("A.MD", ProseFormat::Markdown),
    ("a.txt", ProseFormat::Text),
    ("NOTES.TXT", ProseFormat::Text),
    ("Plan.Org", ProseFormat::Org),
    ("a.org", ProseFormat::Org),
    ("a.rst", ProseFormat::Rst),
    ("a.adoc", ProseFormat::Adoc),
];

/// What a construct yields. Links are `(kind, raw)` as stored in the
/// `links` table.
#[derive(Debug, Default, PartialEq)]
struct Rows {
    links: Vec<(String, String)>,
    tags: Vec<String>,
    block_ids: Vec<String>,
}

struct Construct {
    name: &'static str,
    /// One line of source.
    line: &'static str,
    /// The rows Markdown gives for it.
    markdown: Rows,
}

fn links(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(k, r)| (k.to_string(), r.to_string()))
        .collect()
}

fn constructs() -> Vec<Construct> {
    vec![
        Construct {
            name: "wikilink",
            line: "see [[Target]] here",
            markdown: Rows {
                links: links(&[("wikilink", "Target")]),
                ..Rows::default()
            },
        },
        Construct {
            name: "wikilink alias",
            line: "see [[Target|alias]] here",
            markdown: Rows {
                links: links(&[("wikilink", "Target|alias")]),
                ..Rows::default()
            },
        },
        Construct {
            name: "wikilink heading anchor",
            line: "see [[Target#Heading]] here",
            markdown: Rows {
                links: links(&[("wikilink", "Target#Heading")]),
                ..Rows::default()
            },
        },
        Construct {
            name: "federated block link",
            line: "see [[@other:Target#^blk-1]] here",
            markdown: Rows {
                links: links(&[("wikilink", "@other:Target#^blk-1")]),
                ..Rows::default()
            },
        },
        Construct {
            name: "markdown link",
            line: "see [text](path.md) here",
            markdown: Rows {
                links: links(&[("markdown", "path.md")]),
                ..Rows::default()
            },
        },
        Construct {
            name: "tag",
            line: "a #project/x note",
            markdown: Rows {
                tags: vec!["project/x".to_string()],
                ..Rows::default()
            },
        },
        Construct {
            name: "block id",
            line: "a paragraph ^blk-9",
            markdown: Rows {
                block_ids: vec!["blk-9".to_string()],
                ..Rows::default()
            },
        },
        Construct {
            name: "long block id",
            line: "a paragraph ^a-very-long-block-id-that-obsidian-still-accepts-0123456789",
            markdown: Rows {
                block_ids: vec![
                    "a-very-long-block-id-that-obsidian-still-accepts-0123456789".to_string(),
                ],
                ..Rows::default()
            },
        },
        Construct {
            name: "plain paragraph",
            line: "nothing special here",
            markdown: Rows::default(),
        },
    ]
}

/// The rule: what `markdown`'s rows become in `format`.
fn expected(format: ProseFormat, markdown: &Rows) -> Rows {
    Rows {
        links: markdown.links.clone(),
        // Tags are read from Markdown only.
        tags: if format == ProseFormat::Markdown {
            markdown.tags.clone()
        } else {
            Vec::new()
        },
        block_ids: markdown.block_ids.clone(),
    }
}

fn rows_of(unit: &ParsedUnit) -> Rows {
    Rows {
        links: unit
            .to_new_links(1)
            .iter()
            .map(|l| (l.kind.to_string(), l.to_raw.to_string()))
            .collect(),
        tags: unit.tags.iter().map(|t| t.tag.clone()).collect(),
        block_ids: unit.blocks.iter().map(|b| b.block_id.clone()).collect(),
    }
}

#[test]
fn every_format_gives_the_rows_the_g3_rule_says_for_every_construct() {
    let mut checked = 0;
    for (file, format) in FILES {
        assert_eq!(ProseFormat::from_path(Path::new(file)), format, "{file}");
        for c in constructs() {
            let unit = parse_unit(Path::new(file), &format!("{}\n", c.line));
            assert_eq!(
                rows_of(&unit),
                expected(format, &c.markdown),
                "{file}: {}",
                c.name
            );
            // Every construct is a single one-line paragraph.
            assert_eq!(unit.paragraphs.len(), 1, "{file}: {}", c.name);
            assert_eq!(
                (unit.paragraphs[0].start_line, unit.paragraphs[0].end_line),
                (1, 1),
                "{file}: {}",
                c.name
            );
            // A trailing `^id` lands on the paragraph's row.
            assert_eq!(
                unit.paragraphs[0].block_id.as_deref(),
                c.markdown.block_ids.first().map(String::as_str),
                "{file}: {}",
                c.name
            );
            checked += 1;
        }
    }
    assert_eq!(checked, FILES.len() * constructs().len());
}

#[test]
fn the_same_construct_gives_the_same_line_numbers_in_every_format() {
    // Three paragraphs, the links on lines 1, 3 and 5.
    let body = "one [[A]]\n\ntwo [x](b.md)\n\nthree [[C|c]] ^id3\n";
    let reference = {
        let u = parse_unit(Path::new("a.md"), body);
        (
            u.wikilinks.iter().map(|w| w.line).collect::<Vec<_>>(),
            u.markdown_links.iter().map(|m| m.line).collect::<Vec<_>>(),
            u.blocks.iter().map(|b| b.line).collect::<Vec<_>>(),
            u.paragraphs
                .iter()
                .map(|p| (p.start_line, p.end_line))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(reference.0, vec![1, 5]);
    assert_eq!(reference.1, vec![3]);
    assert_eq!(reference.2, vec![5]);
    assert_eq!(reference.3, vec![(1, 1), (3, 3), (5, 5)]);
    for (file, _) in FILES {
        let u = parse_unit(Path::new(file), body);
        let got = (
            u.wikilinks.iter().map(|w| w.line).collect::<Vec<_>>(),
            u.markdown_links.iter().map(|m| m.line).collect::<Vec<_>>(),
            u.blocks.iter().map(|b| b.line).collect::<Vec<_>>(),
            u.paragraphs
                .iter()
                .map(|p| (p.start_line, p.end_line))
                .collect::<Vec<_>>(),
        );
        assert_eq!(got, reference, "{file}");
    }
}

/// Native link syntax: `(file, source line)` per format. The rule is
/// the same for all of them: no link row, no tag row.
const NATIVE_LINKS: [(&str, &str); 14] = [
    ("a.org", "see [[file:x.org][X]] here"),
    ("a.org", "see [[https://example.com/a][Site]] here"),
    ("a.org", "see [[id:1234-abcd]] here"),
    ("a.org", "see [[file:x.org]] here"),
    ("a.org", "see [[ID:1234-abcd][Idea]] here"),
    ("a.org", "#+TITLE: A title"),
    ("a.org", "a #tagged line"),
    ("a.rst", "see `Site <https://example.com>`_ here"),
    ("a.rst", "see :doc:`other` here"),
    ("a.rst", "a #tagged line"),
    ("a.adoc", "see link:other.adoc[Other] here"),
    ("a.adoc", "see xref:other.adoc#sec[Other] here"),
    ("a.adoc", "see <<anchor>> here and #marked text# and #tag"),
    ("a.txt", "a #tagged line"),
];

#[test]
fn org_rst_and_adoc_native_link_syntax_and_non_markdown_tags_give_no_rows() {
    for (file, line) in NATIVE_LINKS {
        let unit = parse_unit(Path::new(file), &format!("{line}\n"));
        assert!(
            unit.to_new_links(1).is_empty(),
            "{file}: {line}: {:?}",
            unit.wikilinks
        );
        assert!(unit.tags.is_empty(), "{file}: {line}");
    }
}

#[test]
fn a_plain_wikilink_in_an_org_file_still_counts_next_to_native_links() {
    let unit = parse_unit(
        Path::new("a.org"),
        "[[Foo]] and [[file:x.org][X]] and [[Bar|b]]\n",
    );
    let raws: Vec<_> = unit.wikilinks.iter().map(|w| w.raw.as_str()).collect();
    assert_eq!(raws, ["Foo", "Bar|b"]);
}

#[test]
fn a_wikilink_in_double_quotes_in_a_txt_is_a_link() {
    // E1's no-links rule is about code; prose quotes are prose.
    let unit = parse_unit(Path::new("a.txt"), "He said \"[[x]]\" twice.\n");
    let raws: Vec<_> = unit.wikilinks.iter().map(|w| w.raw.as_str()).collect();
    assert_eq!(raws, ["x"]);
}

#[test]
fn markdown_keeps_tags_and_frontmatter_but_the_other_formats_have_neither() {
    let body = "---\ntitle: T\n---\n\n#tag here\n";
    let md = parse_unit(Path::new("a.md"), body);
    assert!(md.frontmatter.is_some());
    assert_eq!(md.tags.len(), 1);
    for file in ["a.txt", "a.org", "a.rst", "a.adoc"] {
        let u = parse_unit(Path::new(file), body);
        assert!(u.frontmatter.is_none(), "{file}");
        assert!(u.tags.is_empty(), "{file}");
    }
}

#[test]
fn txt_org_rst_adoc_wikilinks_use_the_one_wikilink_grammar() {
    // The anchor/alias split is the Markdown grammar's, not a copy:
    // the parsed pieces match across formats.
    let src = "[[@src:Dir/Note#^b1|shown]]\n";
    let md = parse_unit(Path::new("a.md"), src);
    for file in ["a.txt", "a.org", "a.rst", "a.adoc"] {
        let u = parse_unit(Path::new(file), src);
        assert_eq!(u.wikilinks, md.wikilinks, "{file}");
    }
}
