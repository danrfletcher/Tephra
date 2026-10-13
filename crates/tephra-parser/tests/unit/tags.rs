//! `tests/unit/tags.rs`: flat and nested tags, unicode/emoji tags,
//! tag-like text inside code fences (must not be parsed as a tag).

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
