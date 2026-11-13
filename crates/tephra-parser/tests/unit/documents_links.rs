//! `tests/unit/documents_links.rs`: the same text in a docx, a pptx and
//! a PDF gives the same `WikiLink` / `MarkdownLink` as `parse_unit`
//! gives on Markdown, plus the document location (G5: one link grammar,
//! no second one).
//!
//! Table-driven: every link form the Markdown parser accepts is one
//! row. The expectation is never written out per row; it is what
//! `parse_unit` returns for the same text as a Markdown note, so a new
//! form needs one row and nothing else.

use std::path::Path;

use tephra_parser::{
    parse_document_bytes, parse_unit, DocLocation, MarkdownLink, ParsedDocument, WikiLink,
};

use super::documents_support::*;

/// `(name, text, wikilinks expected, markdown links expected)`. The
/// counts guard against a row that silently yields nothing in both
/// readings.
const FORMS: &[(&str, &str, usize, usize)] = &[
    ("plain wikilink", "see [[Note]] here", 1, 0),
    ("alias", "see [[Note|the alias]] here", 1, 0),
    ("heading anchor", "see [[Note#Some Heading]] here", 1, 0),
    ("block anchor", "see [[Note#^abc123]] here", 1, 0),
    ("source prefix", "see [[@notes:Note]] here", 1, 0),
    (
        "prefix, anchor and alias",
        "see [[@notes:Note#Heading|shown]] here",
        1,
        0,
    ),
    ("prefix and block anchor", "[[@notes:Note#^id-1]]", 1, 0),
    ("same-file heading", "jump to [[#Heading]]", 1, 0),
    ("unicode name", "see [[café notes]] here", 1, 0),
    ("unicode federated", "see [[@notes:café notes]] here", 1, 0),
    ("two wikilinks", "[[One]] then [[@b:Two|2]]", 2, 0),
    ("markdown link", "see [text](Target.md) here", 0, 1),
    (
        "markdown link, url",
        "see [site](https://example.com/x) here",
        0,
        1,
    ),
    ("markdown link, empty text", "see [](Target.md) here", 0, 1),
    ("both kinds", "[[Note]] and [text](Target.md)", 1, 1),
];

/// A wikilink with the positional fields (line, span) cleared: those
/// depend on where the text sits, not on the grammar.
fn wiki_key(
    w: &WikiLink,
) -> (
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
) {
    (
        w.source_prefix.clone(),
        w.target.clone(),
        w.anchor.clone(),
        w.alias.clone(),
        w.raw.clone(),
    )
}

fn md_key(m: &MarkdownLink) -> (String, String) {
    (m.text.clone(), m.target.clone())
}

type Make = fn(&str) -> Vec<u8>;

fn in_docx(text: &str) -> Vec<u8> {
    docx_with_paragraphs(&[text])
}
fn in_pptx(text: &str) -> Vec<u8> {
    pptx_of_slides(&[slide_with_body(&[text])])
}
fn in_pdf(text: &str) -> Vec<u8> {
    // WinAnsi: `é` is one byte, U+00E9.
    pdf_with_pages(&[&[text]])
}

/// (file name, builder, the location a link in it must have)
fn containers() -> [(&'static str, Make, DocLocation); 3] {
    [
        ("a.docx", in_docx, DocLocation::Paragraph(1)),
        ("a.pptx", in_pptx, DocLocation::Slide(1)),
        ("a.pdf", in_pdf, DocLocation::Page(1)),
    ]
}

#[test]
fn every_link_form_reads_the_same_in_docx_pptx_pdf_as_in_markdown() {
    let mut checked = 0;
    for (form, text, wiki_n, md_n) in FORMS {
        let reference = parse_unit(Path::new("note.md"), text);
        assert_eq!(
            reference.wikilinks.len(),
            *wiki_n,
            "{form}: markdown wikilinks"
        );
        assert_eq!(
            reference.markdown_links.len(),
            *md_n,
            "{form}: markdown links"
        );

        for (name, make, location) in containers() {
            let doc: ParsedDocument = parse_document_bytes(Path::new(name), &make(text))
                .unwrap_or_else(|e| panic!("{form} in {name}: {e}"));

            let got_wiki: Vec<_> = doc.wikilinks.iter().map(|w| wiki_key(&w.link)).collect();
            let want_wiki: Vec<_> = reference.wikilinks.iter().map(wiki_key).collect();
            assert_eq!(got_wiki, want_wiki, "{form} in {name}: wikilinks");

            let got_md: Vec<_> = doc.markdown_links.iter().map(|m| md_key(&m.link)).collect();
            let want_md: Vec<_> = reference.markdown_links.iter().map(md_key).collect();
            assert_eq!(got_md, want_md, "{form} in {name}: markdown links");

            for w in &doc.wikilinks {
                assert_eq!(w.location, location, "{form} in {name}");
                assert_eq!(w.index, 1, "{form} in {name}");
            }
            for m in &doc.markdown_links {
                assert_eq!(m.location, location, "{form} in {name}");
                assert_eq!(m.index, 1, "{form} in {name}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, FORMS.len() * 3);
}

#[test]
fn rows_in_the_cache_shape_match_markdown_rows() {
    // `to_new_links` is what the scan writes: the same (kind, raw) rows,
    // in the same order, as the Markdown note gives.
    for (form, text, _, _) in FORMS {
        let reference = parse_unit(Path::new("note.md"), text);
        let want: Vec<(String, String)> = reference
            .to_new_links(1)
            .iter()
            .map(|l| (l.kind.to_string(), l.to_raw.to_string()))
            .collect();
        for (name, make, _) in containers() {
            let doc = parse_document_bytes(Path::new(name), &make(text)).unwrap();
            let got: Vec<(String, String)> = doc
                .to_new_links(1)
                .iter()
                .map(|l| (l.kind.to_string(), l.to_raw.to_string()))
                .collect();
            assert_eq!(got, want, "{form} in {name}");
        }
    }
}

#[test]
fn text_with_no_link_form_gives_no_links() {
    for text in [
        "no links here",
        "[single bracket] and (parens)",
        "unterminated [[Note",
        "empty [[]] one",
    ] {
        for (name, make, _) in containers() {
            let doc = parse_document_bytes(Path::new(name), &make(text)).unwrap();
            assert!(doc.wikilinks.is_empty(), "{text:?} in {name}");
            assert!(doc.markdown_links.is_empty(), "{text:?} in {name}");
        }
    }
}
