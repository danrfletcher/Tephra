//! `tests/unit/documents_pdf.rs`: G5 for `.pdf` through the real
//! `parse_document_bytes`: links from the text layer with page numbers,
//! the outline as headings with depth, no text layer, encrypted, and
//! link annotations ignored.

use std::path::Path;

use tephra_parser::{
    parse_document_bytes, DiagnosticKind, DocLocation, DocumentError, ParsedDocument,
};

use super::documents_support::*;

fn parse(bytes: &[u8]) -> Result<ParsedDocument, DocumentError> {
    parse_document_bytes(Path::new("a.pdf"), bytes)
}

#[test]
fn links_from_the_text_layer_carry_their_page() {
    let doc = parse(&fixture("text.pdf")).unwrap();
    let wiki: Vec<(&str, usize)> = doc
        .wikilinks
        .iter()
        .map(|w| (w.link.raw.as_str(), w.location.number()))
        .collect();
    assert_eq!(
        wiki,
        [
            ("Target", 1),
            ("@notes:Tephra test#Intro|shown", 1),
            ("Target#^abc123", 2),
            ("Other", 2),
        ]
    );
    assert!(doc
        .wikilinks
        .iter()
        .all(|w| matches!(w.location, DocLocation::Page(_))));
    let fed = &doc.wikilinks[1].link;
    assert_eq!(fed.source_prefix.as_deref(), Some("notes"));
    assert_eq!(fed.target, "Tephra test");
    assert_eq!(fed.anchor.as_deref(), Some("Intro"));
    assert_eq!(fed.alias.as_deref(), Some("shown"));
    assert_eq!(doc.wikilinks[2].link.anchor.as_deref(), Some("^abc123"));

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
    assert_eq!(md, [("doc link", "Target.md", 1)]);
}

#[test]
fn paragraphs_are_the_text_lines_numbered_within_their_page() {
    let doc = parse(&fixture("text.pdf")).unwrap();
    let got: Vec<(usize, usize, &str)> = doc
        .paragraphs
        .iter()
        .map(|p| (p.location.number(), p.index, p.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1, 1, "Chapter one"),
            (1, 2, "See [[Target]] for details"),
            (1, 3, "A [doc link](Target.md) here"),
            (1, 4, "Federated [[@notes:Tephra test#Intro|shown]] too"),
            (2, 1, "Chapter two"),
            (2, 2, "Block [[Target#^abc123]] and [[Other]]"),
        ]
    );
    assert!(doc.diagnostics.is_empty());
}

#[test]
fn the_outline_becomes_headings_with_depth_as_level() {
    let doc = parse(&fixture("text.pdf")).unwrap();
    let got: Vec<(u8, &str, usize)> = doc
        .headings
        .iter()
        .map(|h| (h.level, h.text.as_str(), h.location.number()))
        .collect();
    assert_eq!(
        got,
        [
            (1, "Chapter one", 1),
            (2, "Section 1.1", 1),
            (1, "Chapter two", 2),
        ]
    );
    assert!(doc
        .headings
        .iter()
        .all(|h| matches!(h.location, DocLocation::Page(_))));
}

fn page_lines(content: &str) -> (ParsedDocument, Vec<String>) {
    let doc = parse(&pdf_with_content_streams(&[content.as_bytes().to_vec()])).unwrap();
    let lines = doc.paragraphs.iter().map(|p| p.text.clone()).collect();
    (doc, lines)
}

#[test]
fn runs_in_separate_text_objects_on_one_baseline_stay_one_line_and_one_link() {
    // A font change mid-line, each run in its own BT..ET with an
    // absolute Tm on the same baseline (what Word and LibreOffice write).
    let (doc, lines) = page_lines(
        "BT /F1 12 Tf 1 0 0 1 72 700 Tm (See [[) Tj ET \
         BT /F2 12 Tf 1 0 0 1 100 700 Tm (Target) Tj ET \
         BT /F1 12 Tf 1 0 0 1 140 700 Tm (]] here) Tj ET",
    );
    assert_eq!(lines, ["See [[Target]] here"]);
    let wiki: Vec<&str> = doc.wikilinks.iter().map(|w| w.link.raw.as_str()).collect();
    assert_eq!(wiki, ["Target"]);
    assert!(doc.diagnostics.is_empty());
}

#[test]
fn a_new_baseline_starts_a_new_line_whatever_the_operators() {
    let (_, lines) = page_lines(
        "BT /F1 12 Tf 1 0 0 1 72 700 Tm (first) Tj ET \
         BT /F1 12 Tf 1 0 0 1 72 686 Tm (second) Tj ET \
         BT /F1 12 Tf 72 672 Td (third) Tj 0 -14 Td (fourth) Tj ET",
    );
    assert_eq!(lines, ["first", "second", "third", "fourth"]);
}

#[test]
fn a_small_vertical_shift_is_a_superscript_not_a_new_line() {
    let (_, lines) = page_lines(
        "BT /F1 12 Tf 72 700 Td (x) Tj /F1 8 Tf 0 4 Td (2) Tj 0 -4 Td /F1 12 Tf ( = 4) Tj ET",
    );
    assert_eq!(lines, ["x2 = 4"]);
}

#[test]
fn a_sideways_td_gap_is_a_word_space_but_a_glyph_run_boundary_is_not() {
    // "See" is about 18 pt wide, so a 30 pt step leaves a word gap.
    let (doc, lines) = page_lines("BT /F1 12 Tf 72 700 Td (See) Tj 30 0 Td ([[Target]]) Tj ET");
    assert_eq!(lines, ["See [[Target]]"]);
    assert_eq!(doc.wikilinks.len(), 1);

    // `[[` is about 12 pt wide: a 12 pt step continues the same word.
    let (doc, lines) =
        page_lines("BT /F1 12 Tf 72 700 Td ([[) Tj 12 0 Td (Note) Tj 24 0 Td (]]) Tj ET");
    assert_eq!(lines, ["[[Note]]"]);
    assert_eq!(doc.wikilinks[0].link.raw, "Note");
}

#[test]
fn a_space_is_not_doubled_when_the_text_already_has_one() {
    let (_, lines) = page_lines("BT /F1 12 Tf 72 700 Td (See ) Tj 60 0 Td (there) Tj ET");
    assert_eq!(lines, ["See there"]);
}

#[test]
fn a_stretch_kern_after_a_real_space_glyph_does_not_double_the_space() {
    // CoreText justifies a line with a real space glyph followed by a
    // large negative kern: `( ) -112.8 (Sp)`.
    let (doc, lines) = page_lines(
        "BT /F1 12 Tf 72 700 Td [([[Word ) -150 (Spacing]]) -150 ( more) -150 (text )] TJ ET",
    );
    assert_eq!(lines, ["[[Word Spacing]] more text"]);
    let wiki: Vec<&str> = doc.wikilinks.iter().map(|w| w.link.raw.as_str()).collect();
    assert_eq!(wiki, ["Word Spacing"]);
    assert_eq!(doc.wikilinks[0].link.target, "Word Spacing");

    // The space glyph can be its own string, and the kern can come first.
    let (doc, lines) =
        page_lines("BT /F1 12 Tf 72 700 Td [([[Word) -112.8 ( ) -112.8 (Spacing]]) -200] TJ ET");
    assert_eq!(lines, ["[[Word Spacing]]"]);
    assert_eq!(doc.wikilinks[0].link.target, "Word Spacing");
}

#[test]
fn a_stretch_kern_between_words_is_still_one_word_space() {
    let (_, lines) = page_lines("BT /F1 12 Tf 72 700 Td [-150 (See) -150 (there) -150] TJ ET");
    assert_eq!(lines, ["See there"]);
}

/// A `/Widths` table for char codes 32..=126: capitals 0.70 em except
/// `W` and `M` (0.94), lower case 0.50 em except `i` and `l` (0.22),
/// everything else (space, punctuation, brackets) 0.28 em.
fn width_of(ch: char) -> f32 {
    match ch {
        'W' | 'M' => 944.0,
        'A'..='Z' => 700.0,
        'i' | 'l' => 222.0,
        'a'..='z' => 500.0,
        _ => 278.0,
    }
}

/// A TrueType font dictionary with a `/Widths` table for [`width_of`].
fn wide_font() -> String {
    let widths: Vec<String> = (32u8..=126)
        .map(|b| width_of(char::from(b)).to_string())
        .collect();
    format!(
        "<< /Type /Font /Subtype /TrueType /BaseFont /Wide /Encoding /WinAnsiEncoding \
         /FirstChar 32 /LastChar 126 /Widths [{}] >>",
        widths.join(" ")
    )
}

/// How far `text` advances in `wide_font` at `size` points.
fn advance_of(text: &str, size: f32) -> f32 {
    text.chars().map(width_of).sum::<f32>() / 1000.0 * size
}

fn lines_with_fonts(content: &str, fonts: &[(&str, String)]) -> (ParsedDocument, Vec<String>) {
    let doc = parse(&pdf_with_fonts(&[content.as_bytes().to_vec()], fonts, &[])).unwrap();
    let lines = doc.paragraphs.iter().map(|p| p.text.clone()).collect();
    (doc, lines)
}

#[test]
fn a_long_wide_run_followed_by_a_tm_placed_run_in_another_font_is_one_link() {
    // 40 capitals, about 0.7 em each: the half-em-per-glyph guess would
    // end 8 em short and call the next run a word gap, giving the link
    // target " Target".
    let head = "MEETING NOTES FOR QUARTERLY PLANNING: [[";
    let tail_x = 72.0 + advance_of(head, 12.0);
    let target_x = tail_x;
    let after_x = target_x + 36.0; // "Target" in Helvetica-Bold at 12 pt is about 36 pt
    let content = format!(
        "BT /W1 12 Tf 1 0 0 1 72 700 Tm ({head}) Tj ET \
         BT /F2 12 Tf 1 0 0 1 {target_x} 700 Tm (Target) Tj ET \
         BT /W1 12 Tf 1 0 0 1 {after_x} 700 Tm (]] for more.) Tj ET"
    );
    let (doc, lines) = lines_with_fonts(&content, &[("W1", wide_font())]);
    assert_eq!(
        lines,
        ["MEETING NOTES FOR QUARTERLY PLANNING: [[Target]] for more."]
    );
    let raw: Vec<&str> = doc
        .wikilinks
        .iter()
        .map(|w| w.link.target.as_str())
        .collect();
    assert_eq!(raw, ["Target"]);
    assert!(doc.diagnostics.is_empty());
}

#[test]
fn thirty_wide_glyphs_before_a_link_do_not_put_a_space_in_it() {
    let head = format!("{} [[", "W".repeat(30));
    let next_x = 72.0 + advance_of(&head, 12.0);
    let content = format!(
        "BT /W1 12 Tf 1 0 0 1 72 700 Tm ({head}) Tj ET \
         BT /F2 12 Tf 1 0 0 1 {next_x} 700 Tm (Wide) Tj ET \
         BT /W1 12 Tf 1 0 0 1 {} 700 Tm (]]) Tj ET",
        next_x + 25.0
    );
    let (doc, _) = lines_with_fonts(&content, &[("W1", wide_font())]);
    assert_eq!(doc.wikilinks.len(), 1);
    assert_eq!(doc.wikilinks[0].link.target, "Wide");
}

#[test]
fn narrow_glyphs_do_not_glue_words_that_only_placement_separates() {
    // 40 `i`s are about 107 pt, the guess would say 240 pt: the run ends
    // well before the guess, and a real word gap follows it.
    let run = "i".repeat(40);
    let next_x = 72.0 + advance_of(&run, 12.0) + 4.0;
    let content = format!(
        "BT /W1 12 Tf 1 0 0 1 72 700 Tm ({run}) Tj ET \
         BT /W1 12 Tf 1 0 0 1 {next_x} 700 Tm (word) Tj ET"
    );
    let (_, lines) = lines_with_fonts(&content, &[("W1", wide_font())]);
    assert_eq!(lines, [format!("{run} word")]);
}

#[test]
fn a_run_that_starts_where_the_last_ends_has_no_space_between() {
    let content = format!(
        "BT /W1 12 Tf 1 0 0 1 72 700 Tm (ab) Tj ET \
         BT /W1 12 Tf 1 0 0 1 {} 700 Tm (cd) Tj ET",
        72.0 + advance_of("ab", 12.0)
    );
    let (_, lines) = lines_with_fonts(&content, &[("W1", wide_font())]);
    assert_eq!(lines, ["abcd"]);
}

#[test]
fn tc_tw_and_tz_move_where_a_run_ends() {
    // Tc 2 on every code, Tw 5 on every space, Tz 50 halves it all.
    let text = "a b c";
    let end = 72.0 + (advance_of(text, 12.0) + 5.0 * 2.0 + 5.0 * 2.0) * 0.5;
    let content = format!(
        "BT /W1 12 Tf 2 Tc 5 Tw 50 Tz 1 0 0 1 72 700 Tm ({text}) Tj ET \
         BT /W1 12 Tf 1 0 0 1 {end} 700 Tm (d) Tj ET"
    );
    // Codes: 5 -> Tc 2 each = 10, Tw: 2 spaces = 10. Total extra 20.
    let (_, lines) = lines_with_fonts(&content, &[("W1", wide_font())]);
    assert_eq!(lines, ["a b cd"]);

    // The same placement without Tc/Tw/Tz knowledge would not match:
    // starting 8 pt later is a word gap.
    let content = format!(
        "BT /W1 12 Tf 2 Tc 5 Tw 50 Tz 1 0 0 1 72 700 Tm ({text}) Tj ET \
         BT /W1 12 Tf 1 0 0 1 {} 700 Tm (d) Tj ET",
        end + 8.0
    );
    let (_, lines) = lines_with_fonts(&content, &[("W1", wide_font())]);
    assert_eq!(lines, ["a b c d"]);
}

#[test]
fn tj_adjustments_move_where_a_run_ends() {
    // `500` in a TJ pulls the pen back 6 pt at 12 pt, so the first run
    // ends 6 pt before the sum of its glyph widths. A run 3 pt past that
    // is a word gap; one right at it is not.
    let end = 72.0 + advance_of("ab", 12.0) - 6.0;
    let run = |x: f32| {
        let content = format!(
            "BT /W1 12 Tf 1 0 0 1 72 700 Tm [(a) 500 (b)] TJ ET \
             BT /W1 12 Tf 1 0 0 1 {x} 700 Tm (c) Tj ET"
        );
        lines_with_fonts(&content, &[("W1", wide_font())]).1
    };
    assert_eq!(run(end), ["abc"]);
    assert_eq!(run(end + 3.0), ["ab c"]);
}

#[test]
fn a_cid_font_reads_widths_from_w_and_dw() {
    let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
/CMapName /Adobe-Identity-UCS def /CMapType 2 def \
1 begincodespacerange <0000> <FFFF> endcodespacerange \
1 beginbfrange <0000> <FFFF> <0000> endbfrange endcmap end end";
    let mut stream = format!("<< /Length {} >>\nstream\n", cmap.len()).into_bytes();
    stream.extend_from_slice(cmap);
    stream.extend_from_slice(b"\nendstream");
    // 'W' (0x57) = 900 and '[' ']' via a range = 300; everything else DW 1000.
    let font = "<< /Type /Font /Subtype /Type0 /BaseFont /Cid /Encoding /Identity-H \
        /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Cid \
        /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
        /DW 1000 /W [87 [900] 91 91 300 93 93 300] >>] /ToUnicode @OBJ0 0 R >>"
        .to_string();
    // "WWWWWW[[" = 6 * 900 + 2 * 300 = 6.0 em = 72 pt at 12; the half-em guess
    // would say 4 em.
    let end = 72.0 + 72.0;
    let content = format!(
        "BT /C1 12 Tf 1 0 0 1 72 700 Tm <005700570057005700570057005B005B> Tj ET \
         BT /F2 12 Tf 1 0 0 1 {end} 700 Tm (Target) Tj ET \
         BT /F2 12 Tf 1 0 0 1 {} 700 Tm (]]) Tj ET",
        end + 36.0
    );
    let doc = parse(&pdf_with_fonts(
        &[content.into_bytes()],
        &[("C1", font)],
        &[stream],
    ))
    .unwrap();
    assert_eq!(doc.wikilinks.len(), 1, "{:?}", doc.paragraphs);
    assert_eq!(doc.wikilinks[0].link.target, "Target");
}

#[test]
fn a_cm_translation_moves_text_to_a_new_line() {
    let (_, lines) = page_lines(
        "q 1 0 0 1 72 700 cm BT /F1 12 Tf (one) Tj ET Q \
         q 1 0 0 1 72 680 cm BT /F1 12 Tf (two) Tj ET Q",
    );
    assert_eq!(lines, ["one", "two"]);
}

#[test]
fn link_annotations_are_not_links() {
    // text.pdf's page 1 has a /Link annotation whose URI is
    // `[[AnnotationTarget]]`.
    let raw = fixture("text.pdf");
    assert!(
        String::from_utf8_lossy(&raw).contains("[[AnnotationTarget]]"),
        "the fixture really carries the annotation"
    );
    let doc = parse(&raw).unwrap();
    assert!(doc
        .wikilinks
        .iter()
        .all(|w| !w.link.raw.contains("AnnotationTarget")));
    assert!(doc
        .paragraphs
        .iter()
        .all(|p| !p.text.contains("AnnotationTarget")));
}

#[test]
fn a_pdf_with_no_text_layer_has_no_text_no_links_and_one_diagnostic() {
    let doc = parse(&fixture("no-text.pdf")).unwrap();
    assert!(doc.paragraphs.is_empty());
    assert!(doc.wikilinks.is_empty() && doc.markdown_links.is_empty());
    let kinds: Vec<_> = doc.diagnostics.iter().map(|d| d.kind).collect();
    assert_eq!(kinds, [DiagnosticKind::NoTextLayer]);
}

#[test]
fn a_pdf_with_zero_pages_is_empty_without_a_warning() {
    let doc = parse(&fixture("empty.pdf")).unwrap();
    assert!(doc.headings.is_empty() && doc.paragraphs.is_empty());
    assert!(doc.wikilinks.is_empty() && doc.markdown_links.is_empty());
    assert!(doc.diagnostics.is_empty());
}

#[test]
fn an_encrypted_pdf_needing_a_password_is_encrypted() {
    // The fixture is a real encrypted PDF: the right password opens it
    // and shows the text, so `Encrypted` below is about the password and
    // not about a damaged file.
    let opened = lopdf::Document::load_mem_with_options(
        &fixture("encrypted.pdf"),
        lopdf::LoadOptions::with_password("s3cret"),
    )
    .expect("user password opens the fixture");
    assert_eq!(opened.get_pages().len(), 2);
    assert_eq!(
        parse(&fixture("encrypted.pdf")).unwrap_err(),
        DocumentError::Encrypted
    );
}

#[test]
fn garbage_and_truncated_pdfs_are_errors_not_panics() {
    assert!(matches!(
        parse(&fixture("garbage.pdf")),
        Err(DocumentError::Corrupt(_))
    ));
    assert!(matches!(parse(b""), Err(DocumentError::Corrupt(_))));
    let full = fixture("text.pdf");
    for cut in (0..full.len()).step_by(13) {
        let _ = parse(&full[..cut]);
    }
}

#[test]
fn a_generated_300_page_pdf_is_read_and_timed() {
    let lines: Vec<String> = (1..=300)
        .map(|n| format!("see [[Page{n}]] please"))
        .collect();
    let pages: Vec<[&str; 2]> = lines.iter().map(|l| ["filler text", l.as_str()]).collect();
    let page_refs: Vec<&[&str]> = pages.iter().map(|p| p.as_slice()).collect();
    let bytes = pdf_with_pages(&page_refs);

    let started = std::time::Instant::now();
    let doc = parse(&bytes).unwrap();
    eprintln!(
        "300-page PDF: {} bytes parsed in {:?}",
        bytes.len(),
        started.elapsed()
    );

    assert_eq!(doc.paragraphs.len(), 600);
    assert_eq!(doc.wikilinks.len(), 300);
    for (i, w) in doc.wikilinks.iter().enumerate() {
        assert_eq!(w.link.raw, format!("Page{}", i + 1));
        assert_eq!(w.location, DocLocation::Page(i + 1));
    }
}

/// Not a check: writes `tests/fixtures/documents/encrypted.pdf` when
/// asked (`UPDATE_FIXTURES=1`). The file key is random, so the bytes
/// differ on every run, which is why this is not part of `generate.py`
/// and the output is committed. The user password is `s3cret`; the
/// document is `text.pdf`.
#[test]
fn regenerate_encrypted_fixture() {
    use lopdf::{Document, EncryptionState, EncryptionVersion, Permissions};

    if std::env::var("UPDATE_FIXTURES").as_deref() != Ok("1") {
        return;
    }
    let mut doc = Document::load_mem(&fixture("text.pdf")).unwrap();
    // Encryption keys derive from the file ID, which text.pdf lacks.
    let id = lopdf::Object::string_literal("tephra-fixture-id-0001");
    doc.trailer.set("ID", vec![id.clone(), id]);
    let version = EncryptionVersion::V2 {
        document: &doc,
        owner_password: "owner-s3cret",
        user_password: "s3cret",
        key_length: 128,
        permissions: Permissions::all(),
    };
    let state = EncryptionState::try_from(version).unwrap();
    doc.encrypt(&state).unwrap();
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    std::fs::write(fixture_path("encrypted.pdf"), out).unwrap();
}

/// F6: reading a scanned PDF as nothing is only honest if no OCR engine
/// can be reached from the build. Checks the committed lockfile.
#[test]
fn no_ocr_crate_is_in_the_lockfile() {
    let lock = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"),
    )
    .expect("read Cargo.lock");
    for line in lock.lines().filter(|l| l.starts_with("name = ")) {
        let name = line.to_ascii_lowercase();
        assert!(
            !name.contains("ocr") && !name.contains("tesseract"),
            "an OCR crate is in Cargo.lock: {line}"
        );
    }
}
