//! `tests/unit/code_robustness.rs`: E12 broken inputs, the parse time
//! bound, and determinism of `parse_code_unit`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tephra_parser::{
    parse_code_unit, parse_code_unit_within, DiagnosticKind, Language, ParsedUnit,
    CODE_PARSE_BUDGET, MAX_CODE_PARSE_BYTES,
};

fn parse(lang: Language, src: &str) -> ParsedUnit {
    parse_code_unit(Path::new("broken"), lang, src)
}

fn raws(unit: &ParsedUnit) -> Vec<(&str, usize)> {
    unit.wikilinks
        .iter()
        .map(|w| (w.raw.as_str(), w.line))
        .collect()
}

/// A half-written file per grammar: a valid comment with a link, then
/// broken code, then a second comment with a link (E12). `parse_code_unit`
/// must return normally and give both links.
const BROKEN: &[(Language, &str)] = &[
    (
        Language::Rust,
        "// [[Before]]\nfn broken( {\n    let x = ;\n// [[After]]\n",
    ),
    (
        Language::Rust,
        "// [[Before]]\nfn f() {\n    if x {\n// [[After]]\n",
    ),
    (
        Language::Python,
        "# [[Before]]\ndef f(:\n    x = = 1\n# [[After]]\n",
    ),
    (
        Language::TypeScript,
        "// [[Before]]\nfunction f( {\n  let = ;\n// [[After]]\n",
    ),
    (
        Language::Tsx,
        "// [[Before]]\nconst e = (1 + ;\n// [[After]]\n",
    ),
    (
        Language::JavaScript,
        "// [[Before]]\nfunction f( {\n  if (\n// [[After]]\n",
    ),
    (
        Language::Jsx,
        "// [[Before]]\nconst e = (1 + ;\n// [[After]]\n",
    ),
    (
        Language::Go,
        "// [[Before]]\nfunc f( {\n\tx := \n// [[After]]\n",
    ),
    (
        Language::Shell,
        "# [[Before]]\nif [[ -f x ; then\nfor in do\n# [[After]]\n",
    ),
    (Language::Yaml, "# [[Before]]\nkey: {a: 1\n# [[After]]\n"),
    (
        Language::Toml,
        "# [[Before]]\nkey = = 1\n[table\n# [[After]]\n",
    ),
    (
        Language::C,
        "// [[Before]]\nint f( {\n  x = ;\n// [[After]]\n",
    ),
    (
        Language::Cpp,
        "// [[Before]]\nclass A { int f( ;\n// [[After]]\n",
    ),
    (
        Language::Java,
        "// [[Before]]\nclass A { void f( { int = ;\n// [[After]]\n",
    ),
    (
        Language::Swift,
        "// [[Before]]\nfunc f( {\n  let = \n// [[After]]\n",
    ),
    (
        Language::Lua,
        "-- [[Before]]\nfunction f()\n  local x = \n-- [[After]]\n",
    ),
];

#[test]
fn a_half_written_file_still_gives_the_comments_before_and_after_the_error() {
    for (lang, src) in BROKEN {
        let unit = parse(*lang, src);
        let found: Vec<&str> = unit.wikilinks.iter().map(|w| w.raw.as_str()).collect();
        assert_eq!(found, ["Before", "After"], "{lang:?}: {src:?}");
        assert!(
            unit.diagnostics.is_empty(),
            "{lang:?}: {:?}",
            unit.diagnostics
        );
    }
}

#[test]
fn a_comment_inside_a_tree_sitter_error_node_is_still_taken() {
    // The comment sits between a function header and the garbage that
    // makes the whole region an ERROR node.
    let src = "fn f(a b c {\n    // [[Inside]]\n    @@@ ###\n    /* [[Block]] */\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(raws(&unit), [("Inside", 2), ("Block", 4)]);
}

#[test]
fn every_prefix_of_every_fixture_parses_without_a_panic() {
    // A file cut off at any character boundary is a half-written file.
    let fixtures = [
        ("python", "sample.py", Language::Python),
        ("typescript", "sample.ts", Language::TypeScript),
        ("tsx", "sample.tsx", Language::Tsx),
        ("javascript", "sample.js", Language::JavaScript),
        ("rust", "sample.rs", Language::Rust),
        ("go", "sample.go", Language::Go),
        ("shell", "sample.sh", Language::Shell),
        ("yaml", "sample.yaml", Language::Yaml),
        ("toml", "sample.toml", Language::Toml),
        ("c", "sample.c", Language::C),
        ("cpp", "sample.cpp", Language::Cpp),
        ("java", "Sample.java", Language::Java),
        ("swift", "sample.swift", Language::Swift),
        ("lua", "sample.lua", Language::Lua),
    ];
    for (dir, file, lang) in fixtures {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/code")
            .join(dir)
            .join(file);
        let full = std::fs::read_to_string(&path).expect("read fixture");
        let whole = parse_code_unit(&path, lang, &full);
        for cut in 0..=full.len() {
            if !full.is_char_boundary(cut) {
                continue;
            }
            let unit = parse_code_unit(&path, lang, &full[..cut]);
            // Whatever a prefix yields, its spans are real bytes of
            // the prefix and no link is invented.
            for w in &unit.wikilinks {
                assert_eq!(
                    &full[w.span.0..w.span.1],
                    format!("[[{}]]", w.raw),
                    "{dir} cut at {cut}"
                );
            }
            assert!(unit.wikilinks.len() <= whole.wikilinks.len(), "{dir} {cut}");
        }
    }
}

#[test]
fn arbitrary_garbage_never_panics_in_any_grammar() {
    // A deterministic spread of nasty inputs, including NUL, lone
    // surrogates' UTF-8 neighbours, huge tokens and unbalanced
    // openers.
    let inputs: Vec<String> = vec![
        String::new(),
        "\0\0\0".into(),
        "[[[[[[[[[[".into(),
        "]]]]]]]]]]".into(),
        "/*".into(),
        "/* [[a".into(),
        "\"\"\"".into(),
        "'''[[".into(),
        "--[==[ [[a]]".into(),
        "<<EOF\n[[a]]".into(),
        "r#\"[[x]]".into(),
        "`${".into(),
        "\u{FEFF}// [[Bom]]\n".into(),
        "é".repeat(10_000),
        "{".repeat(5_000),
        "(".repeat(5_000),
        "[".repeat(5_000),
        "// [[A]]\r\r\r\n\r".into(),
    ];
    for lang in Language::ALL {
        for input in &inputs {
            let unit = parse(lang, input);
            // No panic; whatever it found is well-formed.
            for w in &unit.wikilinks {
                assert!(!w.raw.is_empty());
                assert!(input.is_char_boundary(w.span.0) && input.is_char_boundary(w.span.1));
            }
        }
    }
}

#[test]
fn a_bom_before_the_first_comment_does_not_break_it() {
    let unit = parse(Language::Rust, "\u{FEFF}// [[Bom]]\nfn f() {}\n");
    assert_eq!(raws(&unit), [("Bom", 1)]);
}

/// `size` bytes (at least) of minified-looking JavaScript on one line,
/// with a comment in front and one at the end.
fn minified_js(size: usize) -> String {
    let mut src = String::with_capacity(size + 64);
    src.push_str("/* [[Front]] */");
    while src.len() < size {
        src.push_str("var a=function(b){return b+1};");
    }
    src.push_str("//[[Tail]]");
    assert!(!src.contains('\n'));
    src
}

#[test]
fn a_parse_past_its_budget_is_abandoned_with_no_links_and_a_diagnostic() {
    // Under the size limit, so it reaches tree-sitter and the budget
    // is what stops it.
    let src = minified_js(MAX_CODE_PARSE_BYTES - 1024);
    let started = Instant::now();
    let unit = parse_code_unit_within(
        Path::new("min.js"),
        Language::JavaScript,
        &src,
        Duration::from_millis(1),
    );
    let elapsed = started.elapsed();
    assert!(
        unit.wikilinks.is_empty() && unit.markdown_links.is_empty(),
        "a timed-out parse records no links"
    );
    let timeouts: Vec<_> = unit
        .diagnostics
        .iter()
        .filter(|d| d.kind == DiagnosticKind::ParseTimeout)
        .collect();
    assert_eq!(timeouts.len(), 1, "{:?}", unit.diagnostics);
    assert!(timeouts[0].message.contains("javascript"));
    // The hash is still of the whole content.
    assert_eq!(unit.content_hash.len(), 64);
    // It stopped promptly, not after parsing the whole file.
    assert!(
        elapsed < Duration::from_secs(5),
        "abandoning took {elapsed:?}"
    );
}

#[test]
fn a_file_over_the_size_limit_is_not_parsed() {
    // E12: a 15 MB single-line minified file. tree-sitter would need
    // about 1 GB for it, so it must never be handed over. (The peak
    // memory of the scan is asserted in `tephrad/tests/code_memory.rs`,
    // a test binary of its own: RSS is process-wide, and tests here
    // run in parallel.)
    let src = minified_js(15 * 1024 * 1024);
    assert!(src.len() > MAX_CODE_PARSE_BYTES);
    let unit = parse_code_unit(Path::new("min.js"), Language::JavaScript, &src);

    assert!(
        unit.wikilinks.is_empty() && unit.markdown_links.is_empty(),
        "a skipped file records no links"
    );
    let skipped: Vec<_> = unit
        .diagnostics
        .iter()
        .filter(|d| d.kind == DiagnosticKind::ParseSkippedTooLarge)
        .collect();
    assert_eq!(skipped.len(), 1, "{:?}", unit.diagnostics);
    assert!(skipped[0].message.contains("javascript"));
    assert!(unit
        .diagnostics
        .iter()
        .all(|d| d.kind != DiagnosticKind::ParseTimeout));
    // The unit is still complete: it carries the whole content's hash.
    assert_eq!(unit.content_hash.len(), 64);
    // No wall-clock bound here: the hash of 15 MB is the only work and
    // its time depends on machine load. The diagnostic and the empty
    // links above already show nothing was handed to tree-sitter.
}

#[test]
fn the_size_limit_is_exact_a_file_at_it_parses_and_one_byte_over_does_not() {
    // `// [[A]]\n` then blank padding, so the parse itself is cheap
    // and only the size decides.
    fn file_of(size: usize) -> String {
        let mut src = String::from("// [[A]]\n");
        src.push_str(&" ".repeat(size - src.len()));
        assert_eq!(src.len(), size);
        src
    }
    let at = file_of(MAX_CODE_PARSE_BYTES);
    let unit = parse_code_unit(Path::new("a.js"), Language::JavaScript, &at);
    assert_eq!(raws(&unit), [("A", 1)], "{:?}", unit.diagnostics);
    assert!(unit.diagnostics.is_empty(), "{:?}", unit.diagnostics);

    let over = file_of(MAX_CODE_PARSE_BYTES + 1);
    let unit = parse_code_unit(Path::new("a.js"), Language::JavaScript, &over);
    assert!(unit.wikilinks.is_empty());
    assert_eq!(
        unit.diagnostics.iter().map(|d| d.kind).collect::<Vec<_>>(),
        [DiagnosticKind::ParseSkippedTooLarge]
    );
}

#[test]
fn every_grammar_skips_an_over_limit_file_the_same_way() {
    for lang in Language::ALL {
        let comment = match lang {
            Language::Python | Language::Shell | Language::Yaml | Language::Toml => "# [[A]]\n",
            Language::Lua => "-- [[A]]\n",
            _ => "// [[A]]\n",
        };
        let mut src = String::from(comment);
        src.push_str(&" ".repeat(MAX_CODE_PARSE_BYTES));
        let unit = parse_code_unit(Path::new("big"), lang, &src);
        assert!(unit.wikilinks.is_empty(), "{lang:?}");
        assert!(
            unit.diagnostics
                .iter()
                .any(|d| d.kind == DiagnosticKind::ParseSkippedTooLarge),
            "{lang:?}: {:?}",
            unit.diagnostics
        );
    }
}

#[test]
fn the_default_budget_bounds_a_file_just_under_the_size_limit() {
    let src = minified_js(MAX_CODE_PARSE_BYTES - 1024);
    let started = Instant::now();
    let unit = parse_code_unit(Path::new("min.js"), Language::JavaScript, &src);
    let elapsed = started.elapsed();
    assert!(
        elapsed < CODE_PARSE_BUDGET + Duration::from_secs(10),
        "a 2 MB line took {elapsed:?}, budget {CODE_PARSE_BUDGET:?}"
    );
    let timed_out = unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::ParseTimeout);
    if timed_out {
        assert!(unit.wikilinks.is_empty());
    } else {
        // It parsed within budget: both comments are found.
        let found: Vec<&str> = unit.wikilinks.iter().map(|w| w.raw.as_str()).collect();
        assert_eq!(found, ["Front", "Tail"]);
    }
}

#[test]
fn a_generous_budget_changes_nothing_for_a_normal_file() {
    let src = "// [[A]]\nfn f() {}\n";
    let quick = parse_code_unit_within(
        Path::new("a.rs"),
        Language::Rust,
        src,
        Duration::from_secs(60),
    );
    assert_eq!(
        quick,
        parse_code_unit(Path::new("a.rs"), Language::Rust, src)
    );
    assert!(quick.diagnostics.is_empty());
}

#[test]
fn deeply_nested_code_does_not_overflow_the_stack() {
    let src = format!(
        "// [[Top]]\n{}1{}\n",
        "(".repeat(60_000),
        ")".repeat(60_000)
    );
    let unit = parse_code_unit(Path::new("deep.js"), Language::JavaScript, &src);
    // Either it parsed (the comment is found) or the budget stopped
    // it; it never crashes.
    let timed_out = unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::ParseTimeout);
    assert!(
        timed_out || raws(&unit) == [("Top", 1)],
        "{:?}",
        unit.diagnostics
    );
}

#[test]
fn identical_bytes_give_an_identical_parsed_unit() {
    let src = "// [[A]] and [x](y.md)\n/* [[B]] */\nfn f() {}\n";
    let first = parse(Language::Rust, src);
    let second = parse(Language::Rust, src);
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    // The path is carried, but the content hash does not depend on it.
    let other = parse_code_unit(Path::new("elsewhere/x.rs"), Language::Rust, src);
    assert_eq!(first.content_hash, other.content_hash);
    assert_eq!(first.wikilinks, other.wikilinks);
}

#[test]
fn concurrent_parses_from_two_threads_are_identical() {
    let content: Arc<String> = Arc::new("# [[A]]\n\"\"\"[[Doc]]\"\"\"\n".to_string());
    let run = |c: Arc<String>| {
        std::thread::spawn(move || parse_code_unit(Path::new("x.py"), Language::Python, &c))
    };
    let t1 = run(Arc::clone(&content));
    let t2 = run(Arc::clone(&content));
    let (r1, r2) = (t1.join().expect("thread 1"), t2.join().expect("thread 2"));
    assert_eq!(r1, r2);
    // The leading `#` line is a comment, not a statement, so the
    // string after it is still the module docstring: both are read.
    assert_eq!(raws(&r1), [("A", 1), ("Doc", 2)]);
}

/// Compile-time: `parse_code_unit` and its result are `Send + Sync`,
/// matching the assertion on `parse_unit`'s `ParsedUnit`.
#[test]
fn parse_code_unit_and_its_result_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    assert_send_sync(&parse_code_unit);
    assert_send_sync(&parse_code_unit_within);
    let unit = parse(Language::Rust, "");
    assert_send_sync(&unit);
    assert_send_sync(&Language::Rust);
}

#[test]
fn a_shell_heredoc_cut_off_before_its_terminator_gives_no_link_from_its_text() {
    // The grammar's error recovery reads a `#` line of an unterminated
    // heredoc as a comment; it is heredoc text (a string) and gives no
    // link. A real comment after the terminator still does.
    let open = "# [[Before]]\ncat <<EOF\n[[Text]]\n# [[TextHash]]\n";
    assert_eq!(raws(&parse(Language::Shell, open)), [("Before", 1)]);
    let closed = format!("{open}EOF\n# [[After]]\n");
    assert_eq!(
        raws(&parse(Language::Shell, &closed)),
        [("Before", 1), ("After", 6)]
    );
    // A comment on the heredoc's own start line is a comment.
    let same_line = "cat <<EOF # [[Start]]\nbody\nEOF\n";
    assert_eq!(raws(&parse(Language::Shell, same_line)), [("Start", 1)]);
}
