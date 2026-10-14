//! `tests/unit/tags.rs`: flat and nested tags, unicode/emoji tags,
//! tag-like text inside code fences (must not be parsed as a tag),
//! and the context rules a tag must satisfy (R1): a `#` only starts
//! a tag at line start or after whitespace, wikilink anchors and
//! link/image destinations are never tags, and an all-digit `#tag`
//! is never a tag.

use std::path::Path;

use tephra_parser::parse_unit;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/units/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read fixture")
}

#[test]
fn flat_tag_is_extracted() {
    let unit = parse_unit(Path::new("t.md"), "A paragraph with a #flat tag in it.");
    assert_eq!(unit.tags.len(), 1);
    assert_eq!(unit.tags[0].tag, "flat");
}

#[test]
fn nested_tag_is_one_token_not_split_on_slash() {
    let content = fixture("nested-unicode-tags-with-fence.md");
    let unit = parse_unit(Path::new("nested-unicode-tags-with-fence.md"), &content);

    assert!(unit.tags.iter().any(|t| t.tag == "parent/child/grandchild"));
}

#[test]
fn unicode_and_emoji_tags_parse_as_single_tokens() {
    let content = fixture("nested-unicode-tags-with-fence.md");
    let unit = parse_unit(Path::new("nested-unicode-tags-with-fence.md"), &content);

    assert!(unit.tags.iter().any(|t| t.tag == "emoji🎉tag"));
    assert!(unit.tags.iter().any(|t| t.tag == "日本語タグ"));
}

#[test]
fn tag_like_text_inside_a_fenced_code_block_is_never_parsed_as_a_tag() {
    let content = fixture("nested-unicode-tags-with-fence.md");
    let unit = parse_unit(Path::new("nested-unicode-tags-with-fence.md"), &content);

    assert!(unit.tags.iter().all(|t| t.tag != "not-a-tag"));
}

#[test]
fn a_wikilink_heading_anchor_is_never_a_tag() {
    let content = fixture("federated-wikilink.md");
    let unit = parse_unit(Path::new("federated-wikilink.md"), &content);

    // [[Note#Heading]] and [[@atlas-repo:README#Section|Display]]
    // must never leak "Heading"/"Section" as tags — see R1.
    assert!(
        unit.tags.is_empty(),
        "expected no tags, got {:?}",
        unit.tags
    );
}

#[test]
fn a_hash_mid_word_is_never_a_tag() {
    let unit = parse_unit(Path::new("t.md"), "See foo#bar for details.");
    assert!(
        unit.tags.is_empty(),
        "expected no tags, got {:?}",
        unit.tags
    );
}

#[test]
fn a_url_fragment_in_a_markdown_link_destination_is_never_a_tag() {
    let unit = parse_unit(
        Path::new("t.md"),
        "See [x](https://example.com/page#frag) for details.",
    );
    assert!(
        unit.tags.is_empty(),
        "expected no tags, got {:?}",
        unit.tags
    );
    assert_eq!(unit.markdown_links.len(), 1);
}

#[test]
fn an_anchor_fragment_in_an_image_destination_is_never_a_tag() {
    let unit = parse_unit(Path::new("t.md"), "An image: ![i](pic.png#x) here.");
    assert!(
        unit.tags.is_empty(),
        "expected no tags, got {:?}",
        unit.tags
    );
}

#[test]
fn an_all_digit_hash_token_is_never_a_tag() {
    let unit = parse_unit(Path::new("t.md"), "Ticket #123 is open.");
    assert!(
        unit.tags.is_empty(),
        "expected no tags, got {:?}",
        unit.tags
    );
}

#[test]
fn a_tag_mixing_digits_and_letters_is_still_a_tag() {
    let unit = parse_unit(Path::new("t.md"), "Build #123abc passed.");
    assert!(unit.tags.iter().any(|t| t.tag == "123abc"));
}

#[test]
fn a_tag_at_the_very_start_of_the_body_is_still_a_tag() {
    let unit = parse_unit(Path::new("t.md"), "#start-of-file tag.");
    assert!(unit.tags.iter().any(|t| t.tag == "start-of-file"));
}

#[test]
fn a_tag_like_token_inside_an_inline_code_span_is_never_a_tag() {
    let unit = parse_unit(Path::new("t.md"), "See `#inline` and `[[InCode]]` here.");
    assert!(unit.tags.is_empty(), "got {:?}", unit.tags);
}

#[test]
fn a_tag_like_token_inside_an_indented_code_block_is_never_a_tag() {
    let content = "Some text.\n\n    #indented [[Ind]] ^ind\n\nMore text.\n";
    let unit = parse_unit(Path::new("t.md"), content);
    assert!(unit.tags.is_empty(), "got {:?}", unit.tags);
}

#[test]
fn a_tag_like_token_inside_an_html_comment_is_never_a_tag() {
    let content = "Before.\n\n<!-- #hidden [[Hid]] -->\n\nAfter.\n";
    let unit = parse_unit(Path::new("t.md"), content);
    assert!(unit.tags.is_empty(), "got {:?}", unit.tags);
}
