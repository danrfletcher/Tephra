//! `tests/unit/documents_pptx.rs`: G5 for `.pptx` through the real
//! `parse_document_bytes`: slide order, titles as headings, body links
//! with slide numbers, speaker notes ignored, malformed slide XML.

use std::path::Path;

use tephra_parser::{parse_document_bytes, DocLocation, DocumentError, ParsedDocument};

use super::documents_support::*;

fn parse(name: &str, bytes: &[u8]) -> Result<ParsedDocument, DocumentError> {
    parse_document_bytes(Path::new(name), bytes)
}

#[test]
fn slides_are_numbered_in_sldidlst_order_not_zip_order() {
    // The zip stores slide3, slide1, slide2; sldIdLst says 1, 2, 3.
    let doc = parse("deck.pptx", &fixture("deck.pptx")).unwrap();
    let titles: Vec<(u8, &str, usize)> = doc
        .headings
        .iter()
        .map(|h| (h.level, h.text.as_str(), h.location.number()))
        .collect();
    assert_eq!(
        titles,
        [(1, "Deck title", 1), (1, "Agenda", 2), (1, "Closing", 3)]
    );
    assert!(doc
        .headings
        .iter()
        .all(|h| matches!(h.location, DocLocation::Slide(_))));
}

#[test]
fn body_and_table_links_carry_their_slide_and_notes_are_not_read() {
    let doc = parse("deck.pptx", &fixture("deck.pptx")).unwrap();
    let wiki: Vec<(&str, usize)> = doc
        .wikilinks
        .iter()
        .map(|w| (w.link.raw.as_str(), w.location.number()))
        .collect();
    assert_eq!(wiki, [("Target", 2), ("TableTarget", 3)]);
    assert!(doc
        .wikilinks
        .iter()
        .all(|w| matches!(w.location, DocLocation::Slide(_))));

    let md: Vec<(&str, usize)> = doc
        .markdown_links
        .iter()
        .map(|m| (m.link.target.as_str(), m.location.number()))
        .collect();
    assert_eq!(md, [("Target.md", 2)]);

    // `[[NotesOnly]]` is only in ppt/notesSlides/notesSlide2.xml.
    let text: String = doc.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert!(!text.contains("NotesOnly"), "{text}");
    assert!(doc.wikilinks.iter().all(|w| w.link.raw != "NotesOnly"));
}

#[test]
fn paragraphs_are_numbered_within_their_slide() {
    let doc = parse("deck.pptx", &fixture("deck.pptx")).unwrap();
    let got: Vec<(usize, usize, &str)> = doc
        .paragraphs
        .iter()
        .map(|p| (p.location.number(), p.index, p.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1, 1, "Deck title"),
            (1, 2, "subtitle"),
            (2, 1, "Agenda"),
            (2, 2, "Read [[Target]] first"),
            (2, 3, "and [the doc](Target.md)"),
            (3, 1, "Closing"),
            (3, 2, "Table below"),
            (3, 3, "cell link [[TableTarget]]"),
        ]
    );
}

#[test]
fn a_slide_with_two_title_runs_is_one_heading() {
    let slide = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Two </a:t></a:r><a:r><a:t>parts</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
    let doc = parse("a.pptx", &pptx_of_slides(&[slide.to_string()])).unwrap();
    assert_eq!(doc.headings.len(), 1);
    assert_eq!(doc.headings[0].text, "Two parts");
}

#[test]
fn a_deck_with_no_slides_is_empty_without_a_warning() {
    let doc = parse("empty.pptx", &fixture("empty.pptx")).unwrap();
    assert!(doc.headings.is_empty() && doc.paragraphs.is_empty());
    assert!(doc.wikilinks.is_empty() && doc.markdown_links.is_empty());
    assert!(doc.diagnostics.is_empty());
}

#[test]
fn malformed_slide_xml_is_an_error() {
    for bad in [
        "<p:sld><a:p><a:r><a:t>x</a:r></a:t></a:p></p:sld>",
        "<p:sld><a:p><a:r><a:t>cut off",
    ] {
        match parse("a.pptx", &pptx_of_slides(&[bad.to_string()])) {
            Err(DocumentError::Corrupt(m)) => assert!(m.contains("slide1.xml"), "{m}"),
            other => panic!("expected Corrupt for {bad:?}, got {other:?}"),
        }
    }
}

#[test]
fn a_missing_presentation_or_slide_part_is_an_error() {
    let no_presentation = zip_of(&[("ppt/other.xml", "<x/>".to_string())]);
    assert!(matches!(
        parse("a.pptx", &no_presentation),
        Err(DocumentError::Corrupt(_))
    ));
    // sldIdLst names a slide whose part is absent
    let presentation = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#;
    let rels =
        r#"<Relationships><Relationship Id="rId1" Target="slides/slide1.xml"/></Relationships>"#;
    let missing = zip_of(&[
        ("ppt/presentation.xml", presentation.to_string()),
        ("ppt/_rels/presentation.xml.rels", rels.to_string()),
    ]);
    match parse("a.pptx", &missing) {
        Err(DocumentError::Corrupt(m)) => assert!(m.contains("slides/slide1.xml"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn truncated_pptx_never_panics() {
    let full = fixture("deck.pptx");
    for cut in (0..full.len()).step_by(11) {
        let _ = parse("a.pptx", &full[..cut]);
    }
}

/// F6: the extractor opens only the named XML parts. A `ppt/media`
/// part (PNG, JPG, MP3, MP4 by name) whose bytes spell a link is never
/// read: no paragraph, heading or link carries it.
#[test]
fn media_parts_are_never_opened() {
    let media: Vec<(&str, String)> = [
        "ppt/media/image1.png",
        "ppt/media/photo.jpg",
        "ppt/media/clip.mp3",
        "ppt/media/clip.mp4",
    ]
    .into_iter()
    .map(|name| (name, "see [[MediaOnly]] and [m](MediaOnly.md)".to_string()))
    .collect();
    let bytes = pptx_of_slides_with_parts(&[slide_with_body(&["See [[Real]] here"])], &media);

    // The archive really carries the media parts.
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    assert!(zip.by_name("ppt/media/image1.png").is_ok());

    let doc = parse("media.pptx", &bytes).unwrap();
    let paragraphs: Vec<&str> = doc.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(paragraphs, ["See [[Real]] here"]);
    let wiki: Vec<&str> = doc.wikilinks.iter().map(|w| w.link.raw.as_str()).collect();
    assert_eq!(wiki, ["Real"]);
    assert!(doc.markdown_links.is_empty());
    assert!(doc.headings.is_empty());
    assert!(doc.diagnostics.is_empty());
}
