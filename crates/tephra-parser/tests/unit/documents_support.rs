//! Shared helpers for the `documents_*` unit tests: the committed
//! fixtures, and builders that put arbitrary text into a real docx,
//! pptx or PDF (real archive / real xref, parsed by the real
//! `parse_document_bytes`, no mocks).

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/documents")
        .join(name)
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

pub fn xml_escape(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A zip of `(part name, bytes)`.
pub fn zip_of(parts: &[(&str, String)]) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in parts {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buf.into_inner()
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
const PRES_NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;

/// A docx whose body is the given raw `w:p` XML.
pub fn docx_of_body(body: &str) -> Vec<u8> {
    docx_of_body_with_parts(body, &[])
}

/// A docx whose body is the given raw `w:p` XML, plus extra parts (for
/// example `word/media/...`).
pub fn docx_of_body_with_parts(body: &str, extra: &[(&str, String)]) -> Vec<u8> {
    let mut parts = vec![(
        "word/document.xml",
        format!(r#"<?xml version="1.0"?><w:document {W}><w:body>{body}</w:body></w:document>"#),
    )];
    parts.extend(extra.iter().cloned());
    zip_of(&parts)
}

/// A docx with one plain paragraph per entry of `paragraphs`.
pub fn docx_with_paragraphs(paragraphs: &[&str]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|t| {
            format!(
                r#"<w:p><w:r><w:t xml:space="preserve">{}</w:t></w:r></w:p>"#,
                xml_escape(t)
            )
        })
        .collect();
    docx_of_body(&body)
}

/// A pptx from raw slide XML, listed in `slides` order.
pub fn pptx_of_slides(slides: &[String]) -> Vec<u8> {
    pptx_of_slides_with_parts(slides, &[])
}

/// A pptx from raw slide XML, plus extra parts (for example
/// `ppt/media/...`).
pub fn pptx_of_slides_with_parts(slides: &[String], extra: &[(&str, String)]) -> Vec<u8> {
    let ids: String = (0..slides.len())
        .map(|i| format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 1))
        .collect();
    let rels: String = (0..slides.len())
        .map(|i| {
            format!(
                r#"<Relationship Id="rId{}" Target="slides/slide{}.xml"/>"#,
                i + 1,
                i + 1
            )
        })
        .collect();
    let mut parts: Vec<(String, String)> = vec![
        (
            "ppt/presentation.xml".to_string(),
            format!(r#"<p:presentation {PRES_NS}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#),
        ),
        (
            "ppt/_rels/presentation.xml.rels".to_string(),
            format!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
            ),
        ),
    ];
    for (i, s) in slides.iter().enumerate() {
        parts.push((format!("ppt/slides/slide{}.xml", i + 1), s.clone()));
    }
    let mut refs: Vec<(&str, String)> =
        parts.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
    refs.extend(extra.iter().cloned());
    zip_of(&refs)
}

/// A slide with one body shape holding one `a:p` per entry.
pub fn slide_with_body(paragraphs: &[&str]) -> String {
    let ps: String = paragraphs
        .iter()
        .map(|t| format!("<a:p><a:r><a:t>{}</a:t></a:r></a:p>", xml_escape(t)))
        .collect();
    format!(
        r#"<p:sld {PRES_NS}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="b"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/>{ps}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    )
}

/// A PDF, one page per entry, one text line per entry of a page. Text
/// is shown with Helvetica and `WinAnsiEncoding`; a char above U+00FF
/// is not supported.
pub fn pdf_with_pages(pages: &[&[&str]]) -> Vec<u8> {
    let streams: Vec<Vec<u8>> = pages
        .iter()
        .map(|lines| {
            let mut content: Vec<u8> = b"BT /F1 12 Tf 14 TL 72 720 Td\n".to_vec();
            for line in *lines {
                content.push(b'(');
                for ch in line.chars() {
                    let b = u8::try_from(u32::from(ch)).expect("WinAnsi char");
                    if matches!(b, b'(' | b')' | b'\\') {
                        content.push(b'\\');
                    }
                    content.push(b);
                }
                content.extend_from_slice(b") Tj T*\n");
            }
            content.extend_from_slice(b"ET");
            content
        })
        .collect();
    pdf_with_content_streams(&streams)
}

/// A PDF with one page per raw content stream. `/F1` is Helvetica and
/// `/F2` Helvetica-Bold, both `WinAnsiEncoding`, so a stream can change
/// font mid-line and lay text out with any operators.
pub fn pdf_with_content_streams(streams: &[Vec<u8>]) -> Vec<u8> {
    pdf_with_fonts(streams, &[], &[])
}

/// [`pdf_with_content_streams`] with more fonts in every page's
/// resources: `(resource name, font dictionary)`, e.g. one with its own
/// `/Widths`. `extra_objects` are more objects (a `/ToUnicode` stream,
/// say); `@OBJ0`, `@OBJ1`, ... in a font dictionary is replaced by the
/// object number of the one at that index.
pub fn pdf_with_fonts(
    streams: &[Vec<u8>],
    extra_fonts: &[(&str, String)],
    extra_objects: &[Vec<u8>],
) -> Vec<u8> {
    let pages = streams;
    let mut objects: Vec<(usize, Vec<u8>)> = Vec::new();
    let font_id = 3 + pages.len() * 2;
    let kids: String = (0..pages.len())
        .map(|i| format!("{} 0 R ", 3 + i * 2))
        .collect();
    objects.push((1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()));
    objects.push((
        2,
        format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).into_bytes(),
    ));
    let extra: String = extra_fonts
        .iter()
        .enumerate()
        .map(|(k, (name, _))| format!("/{name} {} 0 R ", font_id + 2 + k))
        .collect();
    let first_object = font_id + 2 + extra_fonts.len();
    for (k, (_, dict)) in extra_fonts.iter().enumerate() {
        let mut dict = dict.clone();
        for j in 0..extra_objects.len() {
            dict = dict.replace(&format!("@OBJ{j}"), &(first_object + j).to_string());
        }
        objects.push((font_id + 2 + k, dict.into_bytes()));
    }
    for (j, body) in extra_objects.iter().enumerate() {
        objects.push((first_object + j, body.clone()));
    }
    for (i, content) in pages.iter().enumerate() {
        let page_id = 3 + i * 2;
        let content_id = page_id + 1;
        objects.push((
            page_id,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R /F2 {} 0 R {extra}>> >> /Contents {content_id} 0 R >>",
                font_id + 1
            )
            .into_bytes(),
        ));
        let mut obj = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        obj.extend_from_slice(content);
        obj.extend_from_slice(b"\nendstream");
        objects.push((content_id, obj));
    }
    objects.push((
        font_id,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ));
    objects.push((
        font_id + 1,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ));
    objects.sort_by_key(|(id, _)| *id);

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (id, body) in &objects {
        offsets.push(out.len());
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let size = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}
