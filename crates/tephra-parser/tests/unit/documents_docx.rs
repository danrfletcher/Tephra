//! `tests/unit/documents_docx.rs`: G5 for `.docx` through the real
//! `parse_document_bytes`: runs joined per paragraph, heading styles
//! mapped to levels, hyperlinks ignored, paragraph indices, and the
//! failure cases.

use std::path::Path;

use tephra_parser::{
    parse_document_bytes, DocLocation, DocumentError, DocumentFormat, ParsedDocument,
};

use super::documents_support::*;

fn parse(name: &str, bytes: &[u8]) -> Result<ParsedDocument, DocumentError> {
    parse_document_bytes(Path::new(name), bytes)
}

fn headings(doc: &ParsedDocument) -> Vec<(u8, &str, usize)> {
    doc.headings
        .iter()
        .map(|h| (h.level, h.text.as_str(), h.location.number()))
        .collect()
}

#[test]
fn heading_styles_map_to_levels_and_title_is_level_one() {
    let doc = parse("headings.docx", &fixture("headings.docx")).unwrap();
    assert_eq!(doc.format, DocumentFormat::Docx);
    assert_eq!(
        headings(&doc),
        [
            (1, "Quarterly report", 1),
            (1, "Overview", 2),
            (2, "Details", 4),
            // `Kop3` is only a heading because styles.xml names it
            // `heading 3`.
            (3, "A style known only by name", 6),
        ]
    );
    assert!(doc
        .headings
        .iter()
        .all(|h| matches!(h.location, DocLocation::Paragraph(_))));
}

#[test]
fn paragraphs_keep_their_document_position_and_skip_empty_ones() {
    let doc = parse("headings.docx", &fixture("headings.docx")).unwrap();
    let got: Vec<(usize, &str)> = doc
        .paragraphs
        .iter()
        .map(|p| (p.index, p.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1, "Quarterly report"),
            (2, "Overview"),
            (3, "See [[Target]] and [a doc](Target.md) for more."),
            (4, "Details"),
            // paragraph 5 is empty: not listed, but 6 is still 6
            (6, "A style known only by name"),
            (7, "visible text of a hyperlink"),
            (8, "field result text"),
            (9, "Ünï © café [[@notes:café notes|alias]] end"),
        ]
    );
    for p in &doc.paragraphs {
        assert_eq!(p.location, DocLocation::Paragraph(p.index));
    }
}

#[test]
fn links_carry_their_paragraph_and_hyperlinks_are_not_links() {
    let doc = parse("headings.docx", &fixture("headings.docx")).unwrap();
    let wiki: Vec<(&str, usize)> = doc
        .wikilinks
        .iter()
        .map(|w| (w.link.raw.as_str(), w.location.number()))
        .collect();
    assert_eq!(wiki, [("Target", 3), ("@notes:café notes|alias", 9)]);
    let alias = &doc.wikilinks[1].link;
    assert_eq!(alias.source_prefix.as_deref(), Some("notes"));
    assert_eq!(alias.target, "café notes");
    assert_eq!(alias.alias.as_deref(), Some("alias"));

    let md: Vec<(&str, &str, usize)> = doc
        .markdown_links
        .iter()
        .map(|m| {
            (
                m.link.text.as_str(),
                m.link.target.as_str(),
                m.location.number(),
            )
        })
        .collect();
    assert_eq!(md, [("a doc", "Target.md", 3)]);

    // The fixture has a `w:hyperlink` whose relationship target is
    // `[[RelTarget]]` and a HYPERLINK field code naming
    // `[[FieldCodeTarget]]`: neither is a link (Dan: A).
    let all: String = doc
        .wikilinks
        .iter()
        .map(|w| w.link.raw.clone())
        .chain(doc.markdown_links.iter().map(|m| m.link.target.clone()))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!all.contains("RelTarget"), "{all}");
    assert!(!all.contains("FieldCodeTarget"), "{all}");
    assert_eq!(doc.wikilinks.len() + doc.markdown_links.len(), 3);
}

#[test]
fn a_link_split_across_runs_is_one_link() {
    let doc = parse("split-runs.docx", &fixture("split-runs.docx")).unwrap();
    assert_eq!(doc.paragraphs.len(), 1);
    assert_eq!(doc.paragraphs[0].text, "see [[@notes:Tephra test]] done");
    assert_eq!(doc.wikilinks.len(), 1);
    let link = &doc.wikilinks[0].link;
    assert_eq!(link.raw, "@notes:Tephra test");
    assert_eq!(link.source_prefix.as_deref(), Some("notes"));
    assert_eq!(link.target, "Tephra test");
    assert_eq!(doc.wikilinks[0].location, DocLocation::Paragraph(1));
}

#[test]
fn entities_tabs_and_breaks_in_runs_are_text() {
    let body = r#"<w:p><w:r><w:t>a &amp; b &lt;c&gt; &#233;</w:t><w:tab/><w:t>x</w:t></w:r></w:p>"#;
    let doc = parse("a.docx", &docx_of_body(body)).unwrap();
    assert_eq!(doc.paragraphs[0].text, "a & b <c> é\tx");
}

#[test]
fn deleted_text_and_fallback_copies_are_not_read_twice() {
    let body = concat!(
        r#"<w:p><w:r><w:t>kept </w:t></w:r><w:del><w:r><w:delText>[[Gone]]</w:delText></w:r></w:del></w:p>"#,
        r#"<w:p><mc:AlternateContent xmlns:mc="m"><mc:Choice><w:r><w:t>[[Once]]</w:t></w:r></mc:Choice>"#,
        r#"<mc:Fallback><w:r><w:t>[[Once]]</w:t></w:r></mc:Fallback></mc:AlternateContent></w:p>"#,
    );
    let doc = parse("a.docx", &docx_of_body(body)).unwrap();
    let raws: Vec<&str> = doc.wikilinks.iter().map(|w| w.link.raw.as_str()).collect();
    assert_eq!(raws, ["Once"]);
}

#[test]
fn a_text_box_paragraph_is_its_own_paragraph() {
    let body = concat!(
        r#"<w:p><w:r><w:t>outer</w:t></w:r><w:r><w:txbxContent>"#,
        r#"<w:p><w:r><w:t>inner [[Boxed]]</w:t></w:r></w:p></w:txbxContent></w:r></w:p>"#,
    );
    let doc = parse("a.docx", &docx_of_body(body)).unwrap();
    let got: Vec<(usize, &str)> = doc
        .paragraphs
        .iter()
        .map(|p| (p.index, p.text.as_str()))
        .collect();
    assert_eq!(got, [(2, "inner [[Boxed]]"), (1, "outer")]);
    assert_eq!(doc.wikilinks[0].location, DocLocation::Paragraph(2));
}

#[test]
fn an_empty_body_is_a_document_with_nothing_in_it() {
    let doc = parse("empty.docx", &fixture("empty.docx")).unwrap();
    assert!(doc.headings.is_empty());
    assert!(doc.paragraphs.is_empty());
    assert!(doc.wikilinks.is_empty() && doc.markdown_links.is_empty());
    assert!(doc.diagnostics.is_empty(), "no warning for an empty docx");
}

#[test]
fn corrupt_zips_and_parts_are_errors_not_panics() {
    // truncated mid-archive
    assert!(matches!(
        parse("truncated.docx", &fixture("truncated.docx")),
        Err(DocumentError::Corrupt(_))
    ));
    // not a zip at all
    assert!(matches!(
        parse("a.docx", b"plain text, not a zip"),
        Err(DocumentError::Corrupt(_))
    ));
    assert!(matches!(
        parse("a.docx", b""),
        Err(DocumentError::Corrupt(_))
    ));
    // a zip with no word/document.xml
    let no_document = zip_of(&[("word/other.xml", "<x/>".to_string())]);
    match parse("a.docx", &no_document) {
        Err(DocumentError::Corrupt(m)) => assert!(m.contains("word/document.xml"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
    // malformed XML: a mismatched end tag, and one cut off inside an element
    for xml in ["<w:p><w:r><w:t>a</w:r></w:t></w:p>", "<w:p><w:r><w:t>a"] {
        let bytes = docx_of_body(xml);
        assert!(
            matches!(parse("a.docx", &bytes), Err(DocumentError::Corrupt(_))),
            "{xml}"
        );
    }
}

#[test]
fn every_prefix_of_a_docx_is_an_error_or_a_parse_never_a_panic() {
    let full = fixture("headings.docx");
    for cut in (0..full.len()).step_by(7) {
        let _ = parse("a.docx", &full[..cut]);
    }
}

#[test]
fn a_zip_bomb_part_is_refused_not_inflated() {
    // 70 MiB of zeros deflates to ~70 KiB; the part cap is 64 MiB.
    let zeros = "0".repeat(70 * 1024 * 1024);
    let bomb = zip_of(&[("word/document.xml", format!("<w:p>{zeros}</w:p>"))]);
    assert!(bomb.len() < 2 * 1024 * 1024, "the bomb itself is small");
    match parse("bomb.docx", &bomb) {
        Err(DocumentError::Corrupt(m)) => assert!(m.contains("inflates past"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn the_extension_picks_the_format_and_a_stranger_is_unsupported() {
    assert!(matches!(
        parse("notes.txt", b"hello"),
        Err(DocumentError::Unsupported)
    ));
    let doc = parse("UPPER.DOCX", &fixture("split-runs.docx")).unwrap();
    assert_eq!(doc.format, DocumentFormat::Docx);
}

/// F6: the extractor opens only the named XML parts. A `word/media`
/// part (PNG, JPG, MP3, MP4 by name) whose bytes spell a link is never
/// read: no paragraph, heading or link carries it.
#[test]
fn media_parts_are_never_opened() {
    let media: Vec<(&str, String)> = [
        "word/media/image1.png",
        "word/media/photo.jpg",
        "word/media/clip.mp3",
        "word/media/clip.mp4",
    ]
    .into_iter()
    .map(|name| (name, "see [[MediaOnly]] and [m](MediaOnly.md)".to_string()))
    .collect();
    let body = r#"<w:p><w:r><w:t>See [[Real]] here</w:t></w:r></w:p>"#;
    let bytes = docx_of_body_with_parts(body, &media);

    // The archive really carries the media parts.
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    assert!(zip.by_name("word/media/image1.png").is_ok());

    let doc = parse("media.docx", &bytes).unwrap();
    let paragraphs: Vec<&str> = doc.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(paragraphs, ["See [[Real]] here"]);
    let wiki: Vec<&str> = doc.wikilinks.iter().map(|w| w.link.raw.as_str()).collect();
    assert_eq!(wiki, ["Real"]);
    assert!(doc.markdown_links.is_empty());
    assert!(doc.headings.is_empty());
    assert!(doc.diagnostics.is_empty());
}
