//! `tests/unit/index_rows.rs`: what a parsed file contributes to the
//! `^^` index (G10), through the real parsers: rows per kind, their
//! lines and `^id`s, and the F5 / E12 rules (a string is not a comment,
//! a half-written file still gives its comments and symbols).

use std::path::Path;

use tephra_parser::{parse_code_unit, parse_document_bytes, parse_unit, IndexRow, Language};

use super::documents_support::{
    docx_with_paragraphs, pdf_with_pages, pptx_of_slides, slide_with_body,
};

fn code_rows(lang: Language, name: &str, src: &str) -> Vec<IndexRow> {
    parse_code_unit(Path::new(name), lang, src).index_rows(src)
}

fn of_kind<'a>(rows: &'a [IndexRow], kind: &str) -> Vec<&'a IndexRow> {
    rows.iter().filter(|r| r.kind == kind).collect()
}

#[test]
fn rust_gives_symbol_and_comment_rows_with_split_names() {
    let src = "\
// The reasons a link can conflict.
// Second line.
pub enum ConflictReason {
    A,
}

fn walk_and_write() {}
";
    let rows = code_rows(Language::Rust, "a.rs", src);
    let symbols = of_kind(&rows, "symbol");
    let enum_row = symbols
        .iter()
        .find(|r| r.name.as_deref() == Some("ConflictReason"))
        .expect("enum row");
    assert_eq!(enum_row.line, Some(3));
    assert_eq!(enum_row.label.as_deref(), Some("enum ConflictReason"));
    assert_eq!(enum_row.text, "ConflictReason conflict reason");
    assert!(enum_row.block_id.is_none());
    let walk = symbols
        .iter()
        .find(|r| r.name.as_deref() == Some("walk_and_write"))
        .expect("fn row");
    assert_eq!(walk.text, "walk_and_write walk and write");

    let comments = of_kind(&rows, "comment");
    assert_eq!(comments.len(), 1, "two line comments are one block");
    assert_eq!(comments[0].line, Some(1));
    assert_eq!(
        comments[0].text,
        "The reasons a link can conflict.\nSecond line."
    );
    assert!(of_kind(&rows, "paragraph").is_empty());
    assert!(of_kind(&rows, "heading").is_empty());
}

#[test]
fn a_symbol_with_an_id_comment_carries_the_block_id_and_the_comment_row_does_not() {
    let src = "// ^k3f9a2\nfn tagged() {}\n\n// Loose note. ^lone\nlet_x();\n";
    let rows = code_rows(Language::Rust, "a.rs", src);
    let tagged = rows
        .iter()
        .find(|r| r.name.as_deref() == Some("tagged"))
        .unwrap();
    assert_eq!(tagged.block_id.as_deref(), Some("k3f9a2"));
    // The ID-only comment has no text; the loose one keeps its ID.
    let comments = of_kind(&rows, "comment");
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].text, "Loose note.");
    assert_eq!(comments[0].block_id.as_deref(), Some("lone"));
}

#[test]
fn a_string_literal_is_never_a_comment_row() {
    let src = "const LABEL: &str = \"conflict // not a comment\";\nfn f() { let s = \"/* conflict */\"; }\n";
    let rows = code_rows(Language::Rust, "a.rs", src);
    assert!(of_kind(&rows, "comment").is_empty(), "{rows:?}");
    assert!(rows.iter().all(|r| !r.text.contains("not a comment")));
}

#[test]
fn python_docstring_and_hash_comments_group_separately() {
    let src = "\
# first
# second
def f():
    \"\"\"Does the thing.\"\"\"
    x = 1  # trailing
    # own line
";
    let rows = code_rows(Language::Python, "a.py", src);
    let texts: Vec<(Option<i64>, &str)> = of_kind(&rows, "comment")
        .iter()
        .map(|r| (r.line, r.text.as_str()))
        .collect();
    assert_eq!(
        texts,
        [
            (Some(1), "first\nsecond"),
            (Some(4), "Does the thing."),
            (Some(5), "trailing"),
            (Some(6), "own line"),
        ]
    );
}

#[test]
fn a_half_written_file_still_gives_its_comments_and_symbols() {
    let src =
        "// before the break\nfn ok() {}\nfn broken( {{{\n// after the break\nfn later() {}\n";
    let rows = code_rows(Language::Rust, "a.rs", src);
    assert!(rows
        .iter()
        .any(|r| r.kind == "comment" && r.text == "before the break"));
    assert!(rows
        .iter()
        .any(|r| r.kind == "comment" && r.text == "after the break"));
    assert!(rows
        .iter()
        .any(|r| r.kind == "symbol" && r.name.as_deref() == Some("ok")));
}

#[test]
fn prose_gives_heading_and_paragraph_rows_with_ids_and_duplicates() {
    let src = "\
# Title here

First paragraph about conflict. ^aaa
spans two lines

- a list item ^dup

Another one. ^dup
";
    let unit = parse_unit(Path::new("n.md"), src);
    let rows = unit.index_rows(src);
    let headings = of_kind(&rows, "heading");
    assert_eq!(headings.len(), 1);
    assert_eq!(
        (headings[0].line, headings[0].text.as_str()),
        (Some(1), "Title here")
    );
    assert_eq!(headings[0].name.as_deref(), Some("Title here"));

    let paragraphs = of_kind(&rows, "paragraph");
    let first = paragraphs
        .iter()
        .find(|r| r.line == Some(3))
        .expect("first paragraph");
    assert_eq!(
        first.text,
        "First paragraph about conflict.\nspans two lines"
    );
    // The ID sits on the first line, not the last, so it is not the
    // paragraph's.
    assert!(first.block_id.is_none());
    // A repeated ID is one row per occurrence, each with its own line.
    let dups: Vec<Option<i64>> = paragraphs
        .iter()
        .filter(|r| r.block_id.as_deref() == Some("dup"))
        .map(|r| r.line)
        .collect();
    assert_eq!(dups, [Some(6), Some(8)]);
}

#[test]
fn a_paragraphs_trailing_id_is_the_block_id_and_not_text() {
    let src = "Some words here. ^abc123\n";
    let unit = parse_unit(Path::new("n.md"), src);
    let rows = unit.index_rows(src);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "Some words here.");
    assert_eq!(rows[0].block_id.as_deref(), Some("abc123"));
}

#[test]
fn docx_pptx_and_pdf_rows_carry_their_place_and_never_a_block_id() {
    let docx = parse_document_bytes(
        Path::new("a.docx"),
        &docx_with_paragraphs(&["alpha words", "", "beta words ^notanid"]),
    )
    .unwrap();
    let rows = docx.index_rows();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r.place == Some("paragraph") && r.line.is_none() && r.block_id.is_none()));
    assert_eq!(rows[1].place_n, Some(3), "empty paragraphs still count");

    let pptx = parse_document_bytes(
        Path::new("a.pptx"),
        &pptx_of_slides(&[
            slide_with_body(&["one slide"]),
            slide_with_body(&["two slide"]),
        ]),
    )
    .unwrap();
    let rows = pptx.index_rows();
    assert!(rows.iter().all(|r| r.place == Some("slide")));
    assert!(rows
        .iter()
        .any(|r| r.place_n == Some(2) && r.text == "two slide"));

    let pdf = parse_document_bytes(
        Path::new("a.pdf"),
        &pdf_with_pages(&[&["first page text"], &["second page text"]]),
    )
    .unwrap();
    let rows = pdf.index_rows();
    assert!(rows
        .iter()
        .all(|r| r.place == Some("page") && r.block_id.is_none()));
    assert!(rows
        .iter()
        .any(|r| r.place_n == Some(2) && r.idx == Some(1)));
}

/// A document paragraph over the row limit is several rows at the same
/// place and index, and none of its words is lost.
#[test]
fn a_long_document_paragraph_is_split_into_rows_at_the_same_place() {
    let words: Vec<String> = (0..9000).map(|i| format!("word{i}")).collect();
    let long = words.join(" ");
    assert!(long.len() > 3 * tephra_parser::MAX_ROW_CHARS);
    let docx = parse_document_bytes(
        Path::new("a.docx"),
        &docx_with_paragraphs(&["first", long.as_str()]),
    )
    .unwrap();
    let rows = docx.index_rows();
    let long_rows: Vec<&IndexRow> = rows.iter().filter(|r| r.place_n == Some(2)).collect();
    assert!(long_rows.len() > 1, "{} rows", long_rows.len());
    assert!(long_rows
        .iter()
        .all(|r| r.place == Some("paragraph") && r.block_id.is_none() && r.line.is_none()));
    let got: Vec<&str> = long_rows.iter().flat_map(|r| r.text.split(' ')).collect();
    assert_eq!(got, words.iter().map(String::as_str).collect::<Vec<_>>());
}
