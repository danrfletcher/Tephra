#!/usr/bin/env python3
"""Regenerates the small committed document fixtures in this directory.

Run from anywhere:  python3 generate.py

Everything is deterministic (fixed zip timestamps, no random ids), so a
regeneration with no edits changes no bytes. `encrypted.pdf` is not made
here: encryption needs a random file key, so `documents_pdf.rs` writes
it (see the `regenerate_encrypted_fixture` test there).

Fixtures (what each is for):
  headings.docx    Title + Heading1..3, a style named by styles.xml only,
                   a w:hyperlink and a HYPERLINK field (neither is a link),
                   a link in an ordinary paragraph, an empty paragraph.
  split-runs.docx  one link split over three w:r runs.
  truncated.docx   headings.docx cut off mid-archive.
  empty.docx       a body with no paragraph text.
  deck.pptx        three slides listed in sldIdLst order (the zip stores
                   them in a different order); titles, a body link, a
                   table link, a link only in speaker notes.
  empty.pptx       no slides.
  text.pdf         two pages of text with links, a two-level outline and
                   a link annotation.
  no-text.pdf      one page that draws a rectangle and has no text.
  empty.pdf        zero pages.
  garbage.pdf      not a PDF.
"""
import io
import os
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
DATE = (2026, 1, 1, 0, 0, 0)


def write_zip(name, parts):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for part, data in parts:
            info = zipfile.ZipInfo(part, DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)
    data = buf.getvalue()
    with open(os.path.join(HERE, name), "wb") as f:
        f.write(data)
    return data


W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'


def esc(t):
    return t.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def run(t):
    return f'<w:r><w:t xml:space="preserve">{esc(t)}</w:t></w:r>'


def para(*runs, style=None):
    ppr = f'<w:pPr><w:pStyle w:val="{style}"/></w:pPr>' if style else ""
    return f"<w:p>{ppr}{''.join(runs)}</w:p>"


CONTENT_TYPES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
    '<Default Extension="xml" ContentType="application/xml"/></Types>'
)
ROOT_RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
    '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}"/>'
    "</Relationships>"
)

STYLES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    f"<w:styles {W}>"
    '<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style>'
    '<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style>'
    '<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style>'
    '<w:style w:type="paragraph" w:styleId="Kop3"><w:name w:val="heading 3"/></w:style>'
    '<w:style w:type="paragraph" w:styleId="Normal"><w:name w:val="Normal"/></w:style>'
    "</w:styles>"
)


def docx(body, rels=""):
    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f"<w:document {W}><w:body>{body}</w:body></w:document>"
    )
    doc_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f"{rels}</Relationships>"
    )
    return [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS.format(target="word/document.xml")),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", STYLES),
    ]


def make_docx():
    hyperlink = (
        '<w:p><w:hyperlink r:id="rId9">'
        + run("visible text of a hyperlink")
        + "</w:hyperlink></w:p>"
    )
    field = (
        "<w:p>"
        '<w:r><w:fldChar w:fldCharType="begin"/></w:r>'
        '<w:r><w:instrText xml:space="preserve"> HYPERLINK "[[FieldCodeTarget]]" </w:instrText></w:r>'
        '<w:r><w:fldChar w:fldCharType="separate"/></w:r>'
        + run("field result text")
        + '<w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>'
    )
    body = "".join(
        [
            para(run("Quarterly report"), style="Title"),
            para(run("Overview"), style="Heading1"),
            para(run("See [[Target]] and [a doc](Target.md) for more.")),
            para(run("Details"), style="Heading2"),
            para(),
            para(run("A style known only by name"), style="Kop3"),
            hyperlink,
            field,
            para(run("Ünï © café [[@notes:café notes|alias]] end")),
        ]
    )
    rels = (
        '<Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" '
        'Target="[[RelTarget]]" TargetMode="External"/>'
    )
    return write_zip("headings.docx", docx(body, rels))


def make_split_runs():
    body = para(run("see "), run("[["), run("@notes:Tephra"), run(" test]]"), run(" done"))
    write_zip("split-runs.docx", docx(body))


def make_empty_docx():
    write_zip("empty.docx", docx(para() + para(run("   "))))


PRES_NS = (
    'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" '
    'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"'
)


def a_p(text):
    return f'<a:p><a:r><a:t>{esc(text)}</a:t></a:r></a:p>'


def sp(ph, *paras):
    nv = f'<p:nvPr><p:ph type="{ph}"/></p:nvPr>' if ph else "<p:nvPr/>"
    return (
        f'<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>{nv}</p:nvSpPr>'
        f"<p:spPr/><p:txBody><a:bodyPr/>{''.join(paras)}</p:txBody></p:sp>"
    )


def slide(*shapes):
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f"<p:sld {PRES_NS}><p:cSld><p:spTree>{''.join(shapes)}</p:spTree></p:cSld></p:sld>"
    )


def table(text):
    return (
        "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc>"
        f"<a:txBody><a:bodyPr/>{a_p(text)}</a:txBody></a:tc></a:tr></a:tbl>"
        "</a:graphicData></a:graphic></p:graphicFrame>"
    )


def make_pptx(name, slides, order, notes=None):
    """slides: {part: xml}; order: list of (rId, part) in sldIdLst order."""
    ids = "".join(
        f'<p:sldId id="{256 + i}" r:id="{rid}"/>' for i, (rid, _) in enumerate(order)
    )
    presentation = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f"<p:presentation {PRES_NS}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"
    )
    rels = "".join(
        f'<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/{part}"/>'
        for rid, part in order
    )
    pres_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f"{rels}</Relationships>"
    )
    parts = [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS.format(target="ppt/presentation.xml")),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
    ]
    for part, xml in slides.items():
        parts.append((f"ppt/slides/{part}", xml))
    for part, xml in (notes or {}).items():
        parts.append((f"ppt/notesSlides/{part}", xml))
    write_zip(name, parts)


def make_deck():
    slides = {
        # zip order is slide3, slide1, slide2; the list order is 1, 2, 3
        "slide3.xml": slide(
            sp("title", a_p("Closing")),
            sp(None, a_p("Table below")),
            table("cell link [[TableTarget]]"),
        ),
        "slide1.xml": slide(
            sp("ctrTitle", a_p("Deck title")),
            sp("subTitle", a_p("subtitle")),
        ),
        "slide2.xml": slide(
            sp("title", a_p("Agenda")),
            sp(None, a_p("Read [[Target]] first"), a_p("and [the doc](Target.md)")),
        ),
    }
    notes = {
        "notesSlide2.xml": slide(sp("body", a_p("speaker only [[NotesOnly]]"))),
    }
    make_pptx(
        "deck.pptx",
        slides,
        [("rId11", "slide1.xml"), ("rId12", "slide2.xml"), ("rId13", "slide3.xml")],
        notes,
    )


def make_empty_pptx():
    make_pptx("empty.pptx", {}, [])


def pdf_bytes(objects, root):
    """objects: {num: bytes body}. Writes a classic-xref PDF."""
    out = bytearray(b"%PDF-1.4\n")
    offsets = {}
    for num in sorted(objects):
        offsets[num] = len(out)
        out += f"{num} 0 obj\n".encode() + objects[num] + b"\nendobj\n"
    xref = len(out)
    size = max(objects) + 1
    out += f"xref\n0 {size}\n".encode()
    out += b"0000000000 65535 f \n"
    for num in range(1, size):
        out += f"{offsets[num]:010d} 00000 n \n".encode()
    out += (
        f"trailer\n<< /Size {size} /Root {root} 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    )
    return bytes(out)


def stream(data):
    return b"<< /Length %d >>\nstream\n" % len(data) + data + b"\nendstream"


def pdf_text(lines):
    ops = ["BT", "/F1 12 Tf", "14 TL", "72 720 Td"]
    for line in lines:
        ops.append("(%s) Tj" % line.replace("(", "\\(").replace(")", "\\)"))
        ops.append("T*")
    ops.append("ET")
    return "\n".join(ops).encode()


def make_text_pdf():
    page1 = pdf_text(
        [
            "Chapter one",
            "See [[Target]] for details",
            "A [doc link](Target.md) here",
            "Federated [[@notes:Tephra test#Intro|shown]] too",
        ]
    )
    page2 = pdf_text(["Chapter two", "Block [[Target#^abc123]] and [[Other]]"])
    objects = {
        1: b"<< /Type /Catalog /Pages 2 0 R /Outlines 7 0 R >>",
        2: b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>",
        3: b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 9 0 R >> >> /Contents 4 0 R /Annots [10 0 R] >>",
        4: stream(page1),
        5: b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 9 0 R >> >> /Contents 6 0 R >>",
        6: stream(page2),
        7: b"<< /Type /Outlines /First 8 0 R /Last 11 0 R /Count 3 >>",
        8: b"<< /Title (Chapter one) /Parent 7 0 R /Next 11 0 R /First 12 0 R /Last 12 0 R /Count 1 /Dest [3 0 R /Fit] >>",
        9: b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        10: b"<< /Type /Annot /Subtype /Link /Rect [72 700 200 714] /Border [0 0 0] /A << /S /URI /URI ([[AnnotationTarget]]) >> >>",
        11: b"<< /Title (Chapter two) /Parent 7 0 R /Prev 8 0 R /Dest [5 0 R /Fit] >>",
        12: b"<< /Title (Section 1.1) /Parent 8 0 R /Dest [3 0 R /Fit] >>",
    }
    with open(os.path.join(HERE, "text.pdf"), "wb") as f:
        f.write(pdf_bytes(objects, 1))


def make_no_text_pdf():
    content = b"0.5 g\n100 100 300 300 re\nf\n"
    objects = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        3: b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents 4 0 R >>",
        4: stream(content),
    }
    with open(os.path.join(HERE, "no-text.pdf"), "wb") as f:
        f.write(pdf_bytes(objects, 1))


def make_empty_pdf():
    objects = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids [] /Count 0 >>",
    }
    with open(os.path.join(HERE, "empty.pdf"), "wb") as f:
        f.write(pdf_bytes(objects, 1))


def main():
    full = make_docx()
    with open(os.path.join(HERE, "truncated.docx"), "wb") as f:
        f.write(full[: len(full) // 2])
    make_split_runs()
    make_empty_docx()
    make_deck()
    make_empty_pptx()
    make_text_pdf()
    make_no_text_pdf()
    make_empty_pdf()
    with open(os.path.join(HERE, "garbage.pdf"), "wb") as f:
        f.write(b"this is not a pdf\n")


if __name__ == "__main__":
    main()
