//! `tests/unit/determinism.rs`: identical input parsed twice
//! (sequentially and from two threads) produces a byte-identical
//! `ParsedUnit` and hash.

use std::path::Path;
use std::sync::Arc;

use tephra_parser::parse_unit;

const SAMPLE: &str = "---\ntitle: Determinism\nstatus: active\n---\n\n# Heading\n\nSee [[Other#^anchor|Alias]] and a #tag/nested here. ^block-id\n";

#[test]
fn sequential_reparse_of_identical_content_is_byte_identical_including_hash() {
    let first = parse_unit(Path::new("sample.md"), SAMPLE);
    let second = parse_unit(Path::new("sample.md"), SAMPLE);
    assert_eq!(first, second);
    assert_eq!(first.content_hash, second.content_hash);
}

#[test]
fn concurrent_parse_from_two_threads_produces_identical_output() {
    let content: Arc<String> = Arc::new(SAMPLE.to_string());

    let c1 = Arc::clone(&content);
    let t1 = std::thread::spawn(move || parse_unit(Path::new("sample.md"), &c1));
    let c2 = Arc::clone(&content);
    let t2 = std::thread::spawn(move || parse_unit(Path::new("sample.md"), &c2));

    let r1 = t1.join().expect("thread 1");
    let r2 = t2.join().expect("thread 2");

    assert_eq!(r1, r2);
    assert_eq!(r1.content_hash, r2.content_hash);
}

#[test]
fn content_hash_is_independent_of_path() {
    let a = parse_unit(Path::new("a.md"), SAMPLE);
    let b = parse_unit(Path::new("totally/different/path/b.md"), SAMPLE);
    assert_eq!(a.content_hash, b.content_hash);
}

#[test]
fn different_content_hashes_differently() {
    let a = parse_unit(Path::new("a.md"), "one");
    let b = parse_unit(Path::new("a.md"), "two");
    assert_ne!(a.content_hash, b.content_hash);
}

#[test]
fn parsed_unit_is_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<tephra_parser::ParsedUnit>();
}
