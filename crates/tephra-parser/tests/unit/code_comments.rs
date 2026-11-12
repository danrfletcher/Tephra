//! `tests/unit/code_comments.rs`: the G4 / E1 rule, run over every
//! bundled grammar through the real `parse_code_unit`: links are read
//! inside comments, block comments, doc comments and Python docstrings,
//! and nowhere else (code, strings, template literals, raw strings,
//! heredocs, YAML/TOML values, Lua long strings).

use std::path::{Path, PathBuf};

use tephra_parser::{parse_code_unit, DiagnosticKind, Language, ParsedUnit};

fn parse(lang: Language, src: &str) -> ParsedUnit {
    let name = format!("sample.{}", lang.name());
    parse_code_unit(Path::new(&name), lang, src)
}

fn wikilinks(unit: &ParsedUnit) -> Vec<(String, usize)> {
    unit.wikilinks
        .iter()
        .map(|w| (w.raw.clone(), w.line))
        .collect()
}

fn pairs(list: &[(&str, usize)]) -> Vec<(String, usize)> {
    list.iter().map(|(r, l)| (r.to_string(), *l)).collect()
}

fn fixture(dir: &str, file: &str) -> (PathBuf, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/code")
        .join(dir)
        .join(file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    (path, text)
}

/// One fixture per grammar (14: TypeScript and TSX are separate
/// grammars): a line comment, a block comment where the language has
/// one, a doc comment / docstring, and decoys in string literals.
/// `(dir, file, language, expected (to_raw, line))`.
type Fixture = (
    &'static str,
    &'static str,
    Language,
    &'static [(&'static str, usize)],
);

const FIXTURES: &[Fixture] = &[
    (
        "python",
        "sample.py",
        Language::Python,
        &[("Doc", 1), ("Line", 2)],
    ),
    (
        "typescript",
        "sample.ts",
        Language::TypeScript,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "tsx",
        "sample.tsx",
        Language::Tsx,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "javascript",
        "sample.js",
        Language::JavaScript,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "rust",
        "sample.rs",
        Language::Rust,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "go",
        "sample.go",
        Language::Go,
        &[("Line", 3), ("Block", 4), ("Doc", 5)],
    ),
    (
        "shell",
        "sample.sh",
        Language::Shell,
        &[("Line", 2), ("Doc", 3)],
    ),
    (
        "yaml",
        "sample.yaml",
        Language::Yaml,
        &[("Line", 1), ("Doc", 2)],
    ),
    (
        "toml",
        "sample.toml",
        Language::Toml,
        &[("Line", 1), ("Doc", 2)],
    ),
    (
        "c",
        "sample.c",
        Language::C,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "cpp",
        "sample.cpp",
        Language::Cpp,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "java",
        "Sample.java",
        Language::Java,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "swift",
        "sample.swift",
        Language::Swift,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
    (
        "lua",
        "sample.lua",
        Language::Lua,
        &[("Line", 1), ("Block", 2), ("Doc", 3)],
    ),
];

#[test]
fn there_is_one_fixture_per_grammar() {
    assert_eq!(FIXTURES.len(), 14);
    // Every language but JSX (which shares the JavaScript grammar) has
    // its own fixture.
    for lang in Language::ALL {
        if lang == Language::Jsx {
            continue;
        }
        assert!(
            FIXTURES.iter().any(|(_, _, l, _)| *l == lang),
            "no fixture for {lang:?}"
        );
    }
}

#[test]
fn every_fixture_gives_exactly_its_expected_links_and_lines() {
    for (dir, file, lang, expected) in FIXTURES {
        let (path, content) = fixture(dir, file);
        // The fixture's own path must map to the language under test.
        assert_eq!(
            Language::from_path(&path, content.lines().next()),
            Some(*lang),
            "{dir}/{file}"
        );
        let unit = parse_code_unit(&path, *lang, &content);
        assert_eq!(wikilinks(&unit), pairs(expected), "{dir}/{file}");
        assert!(unit.markdown_links.is_empty(), "{dir}/{file}");
        assert!(
            unit.diagnostics.is_empty(),
            "{dir}/{file}: {:?}",
            unit.diagnostics
        );
        // F5: no decoy row, whatever the string kind.
        assert!(
            unit.wikilinks.iter().all(|w| !w.raw.contains("Decoy")),
            "{dir}/{file}: a decoy leaked"
        );
        // These fixtures have no `^id` comment, so a code unit here
        // records links and symbols only: no heading, tag, block or
        // paragraph.
        assert!(unit.headings.is_empty() && unit.tags.is_empty());
        assert!(unit.blocks.is_empty() && unit.paragraphs.is_empty());
        assert!(unit.frontmatter.is_none());
    }
}

#[test]
fn every_link_span_is_the_exact_bytes_of_its_brackets() {
    for (dir, file, lang, _) in FIXTURES {
        let (path, content) = fixture(dir, file);
        let unit = parse_code_unit(&path, *lang, &content);
        for w in &unit.wikilinks {
            assert_eq!(
                &content[w.span.0..w.span.1],
                format!("[[{}]]", w.raw),
                "{dir}/{file}"
            );
            let line = 1 + content.as_bytes()[..w.span.0]
                .iter()
                .filter(|b| **b == b'\n')
                .count();
            assert_eq!(w.line, line, "{dir}/{file}: {}", w.raw);
        }
    }
}

#[test]
fn a_jsx_file_reads_comments_but_not_jsx_text() {
    let src = "// c [[Line]]\nconst e = <div>[[DecoyJsx]]</div>;\n{/* in jsx [[Block]] */}\n";
    let unit = parse(Language::Jsx, src);
    assert_eq!(
        wikilinks(&unit),
        pairs(&[("Line", 1)])
            .into_iter()
            .chain(pairs(&[("Block", 3)]))
            .collect::<Vec<_>>()
    );
}

/// Decoys only, per grammar: every one of these must give no link and
/// no diagnostic, in particular no unterminated-wikilink diagnostic.
const DECOYS: &[(Language, &str)] = &[
    (
        Language::Python,
        "x = 1\n\"\"\"[[A]]\"\"\"\ny = arr[[0, 2]]\nz = \"[[x]]\"\nw = f\"[[{x}]]\"\nv = r'[[B]]'\nb = b\"[[C]]\"\n",
    ),
    (
        Language::TypeScript,
        "const a = \"[[A]]\";\nconst b = '[[B]]';\nconst c = `[[C]] ${a}`;\nconst d: number[][] = [[1]];\nconst re = /[[x]]/;\n",
    ),
    (
        Language::Tsx,
        "const a = `[[A]]`;\nconst e = <p>[[B]]</p>;\nconst f = <p title=\"[[C]]\" />;\n",
    ),
    (
        Language::JavaScript,
        "const a = \"[[A]]\";\nconst c = `[[C]]`;\nconst d = [[1, 2]];\n",
    ),
    (
        Language::Jsx,
        "const a = \"[[A]]\";\nconst e = <p>[[B]]</p>;\n",
    ),
    (
        Language::Rust,
        "const A: &str = \"[[A]]\";\nconst B: &str = r#\"[[x]]\"#;\nconst C: &str = r##\"[[y]] \"# [[z]]\"##;\nlet v: Vec<Vec<u8>> = vec![[1]];\nconst D: &[u8] = b\"[[D]]\";\n",
    ),
    (
        Language::Go,
        "var a = \"[[A]]\"\nvar b = `[[B]]\n[[C]]`\nvar c = [][]int{}\n",
    ),
    (
        Language::Shell,
        "if [[ -f \"$f\" ]]; then echo \"[[x]]\"; fi\n[[ $a == b ]] && echo ok\ncat <<EOF\n[[Here]]\n# [[HereHash]]\nEOF\ncat <<'EOF'\n[[Quoted]]\nEOF\necho '[[S]]' \"[[D]]\"\n",
    ),
    (
        Language::Yaml,
        "a: \"[[A]]\"\nb: '[[B]]'\nc: [[C]]\nd: |\n  [[Block]]\n  # [[NotAComment]]\ne: [[x, y], [z]]\n",
    ),
    (
        Language::Toml,
        "a = \"[[A]]\"\nb = '[[B]]'\nc = [[\"x\"], [\"y\"]]\nd = \"\"\"\n[[Multi]]\n# [[NotAComment]]\n\"\"\"\n",
    ),
    (
        Language::C,
        "const char *a = \"[[A]]\";\nint m[2][2] = {{1}};\nchar c = '[';\n",
    ),
    (
        Language::Cpp,
        "const char *a = \"[[A]]\";\nconst char *r = R\"(\n[[Raw]]\n)\";\n[[nodiscard]] int f();\n",
    ),
    (
        Language::Java,
        "class A {\n  String a = \"[[A]]\";\n  String t = \"\"\"\n    [[Block]]\n    \"\"\";\n  int[][] m;\n}\n",
    ),
    (
        Language::Swift,
        "let a = \"[[A]]\"\nlet m = \"\"\"\n[[Multi]]\n\"\"\"\nlet n = #\"[[Raw]]\"#\nlet g = [[1], [2]]\n",
    ),
    (
        Language::Lua,
        "local a = \"[[A]]\"\nlocal s = [[x]]\nlocal t = [==[ [[y]] ]==]\nlocal u = t[t[1]]\n",
    ),
];

#[test]
fn code_strings_values_and_literals_never_give_a_link_or_a_diagnostic() {
    for (lang, src) in DECOYS {
        let unit = parse(*lang, src);
        assert!(
            unit.wikilinks.is_empty() && unit.markdown_links.is_empty(),
            "{lang:?} read a link out of code: {:?} {:?}",
            unit.wikilinks,
            unit.markdown_links
        );
        assert!(
            unit.diagnostics.is_empty(),
            "{lang:?}: {:?}",
            unit.diagnostics
        );
    }
}

#[test]
fn every_grammar_has_a_decoy_case() {
    for lang in Language::ALL {
        assert!(DECOYS.iter().any(|(l, _)| *l == lang), "{lang:?}");
    }
}

#[test]
fn the_same_decoys_with_a_real_comment_beside_them_give_only_the_comment() {
    // A comment leader per language, so each decoy source above is
    // re-checked with one real link added.
    let leader = |lang: Language| match lang {
        Language::Python | Language::Shell | Language::Yaml | Language::Toml => "#",
        Language::Lua => "--",
        _ => "//",
    };
    for (lang, src) in DECOYS {
        let with_comment = format!("{} see [[Real]]\n{src}", leader(*lang));
        let unit = parse(*lang, &with_comment);
        assert_eq!(wikilinks(&unit), pairs(&[("Real", 1)]), "{lang:?}");
    }
}

#[test]
fn shell_test_brackets_and_python_subscripts_give_no_links_or_diagnostics() {
    let sh = parse(Language::Shell, "if [[ -f \"$f\" ]]; then :; fi\n");
    assert!(sh.wikilinks.is_empty() && sh.diagnostics.is_empty());
    let py = parse(Language::Python, "x = arr[[0, 2]]\ny = \"[[x]]\"\n");
    assert!(py.wikilinks.is_empty() && py.diagnostics.is_empty());
}

#[test]
fn lua_block_comment_opener_is_not_a_link_and_only_the_inner_link_is_recorded() {
    let unit = parse(Language::Lua, "--[[ see [[@notes:A]] ]]\nlocal x = 1\n");
    assert_eq!(wikilinks(&unit), pairs(&[("@notes:A", 1)]));
    assert_eq!(unit.wikilinks[0].source_prefix.as_deref(), Some("notes"));
    assert_eq!(unit.wikilinks[0].target, "A");
    // The first `]]` closes the long comment, so the trailing ` ]]` is
    // real invalid Lua: the only diagnostic is that syntax error (E12).
    assert!(
        unit.diagnostics
            .iter()
            .all(|d| d.kind == DiagnosticKind::SyntaxErrors),
        "{:?}",
        unit.diagnostics
    );

    // The same with a level-1 long bracket, where nesting is legal.
    let unit = parse(Language::Lua, "--[=[ see [[B]] ]=]\n");
    assert_eq!(wikilinks(&unit), pairs(&[("B", 1)]));

    // A Lua line comment that merely contains `[[` is read normally.
    let unit = parse(Language::Lua, "-- see [[C]]\n");
    assert_eq!(wikilinks(&unit), pairs(&[("C", 1)]));
}

#[test]
fn python_docstrings_are_the_first_statement_of_a_module_class_or_function() {
    let src = concat!(
        "#!/usr/bin/env python3\n",              // 1
        "\"\"\"Module [[Mod]]\"\"\"\n",          // 2
        "\n",                                    // 3
        "class C:\n",                            // 4
        "    \"\"\"Class [[Cls]]\"\"\"\n",       // 5
        "\n",                                    // 6
        "    def m(self):\n",                    // 7
        "        \"\"\"Method [[Meth]]\"\"\"\n", // 8
        "        return 1\n",                    // 9
        "\n",                                    // 10
        "def f():\n",                            // 11
        "    # a comment first [[Cmt]]\n",       // 12
        "    '''Func [[Func]]'''\n",             // 13
        "\n",                                    // 14
        "async def g():\n",                      // 15
        "    r\"\"\"Raw [[Raw]]\"\"\"\n",        // 16
        "\n",                                    // 17
        "@deco\n",                               // 18
        "def h():\n",                            // 19
        "    \"\"\"Decorated [[Deco]]\"\"\"\n",  // 20
    );
    let unit = parse(Language::Python, src);
    assert_eq!(
        wikilinks(&unit),
        pairs(&[
            ("Mod", 2),
            ("Cls", 5),
            ("Meth", 8),
            ("Cmt", 12),
            ("Func", 13),
            ("Raw", 16),
            ("Deco", 20)
        ])
    );
}

#[test]
fn a_python_string_that_is_not_the_first_statement_is_not_a_docstring() {
    // After `x = 1`.
    let unit = parse(Language::Python, "x = 1\n\"\"\"[[A]]\"\"\"\n");
    assert!(unit.wikilinks.is_empty(), "{:?}", unit.wikilinks);

    // After `return`, and as a function's second statement.
    let src = "def f():\n    x = 1\n    \"\"\"[[B]]\"\"\"\n    return \"\"\"[[C]]\"\"\"\n";
    assert!(parse(Language::Python, src).wikilinks.is_empty());

    // A second string after a real docstring.
    let src = "\"\"\"doc [[Doc]]\"\"\"\n\"\"\"[[Second]]\"\"\"\n";
    assert_eq!(
        wikilinks(&parse(Language::Python, src)),
        pairs(&[("Doc", 1)])
    );

    // An f-string or bytes string in first position is not a docstring.
    for first in [
        "f\"\"\"[[A]]\"\"\"",
        "F'[[A]]'",
        "b\"\"\"[[A]]\"\"\"",
        "rb'[[A]]'",
    ] {
        let src = format!("def f():\n    {first}\n    pass\n");
        assert!(
            parse(Language::Python, &src).wikilinks.is_empty(),
            "{first}"
        );
    }

    // A string inside an expression, a call, a list.
    let src = "print(\"[[A]]\")\nx = [\"[[B]]\"]\ny = \"[[C]]\" + \"[[D]]\"\n";
    assert!(parse(Language::Python, src).wikilinks.is_empty());
}

#[test]
fn two_links_in_one_comment_line_and_the_same_link_twice_give_a_row_each() {
    let src = "// [[A]] and [[B]]\n// [[A]]\nfn f() {} // [[A]]\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        wikilinks(&unit),
        pairs(&[("A", 1), ("B", 1), ("A", 2), ("A", 3)])
    );
    // And as `tephra-cache` rows, one each.
    let rows = unit.to_new_links(7);
    assert_eq!(rows.len(), 4);
    assert!(rows
        .iter()
        .all(|r| r.kind == "wikilink" && r.from_unit_id == 7));
    assert_eq!(
        rows.iter().map(|r| r.to_raw).collect::<Vec<_>>(),
        ["A", "B", "A", "A"]
    );
}

#[test]
fn line_numbers_are_absolute_inside_a_multi_line_block_comment() {
    let src =
        "/*\n * first\n * second [[Two]]\n *\n * fourth [[Four]] and [[Four2]]\n */\nint x;\n";
    let unit = parse(Language::C, src);
    assert_eq!(
        wikilinks(&unit),
        pairs(&[("Two", 3), ("Four", 5), ("Four2", 5)])
    );

    // Same in a Python docstring and a Lua long comment.
    let src = "def f():\n    \"\"\"\n    a\n    [[Three]]\n    \"\"\"\n";
    assert_eq!(
        wikilinks(&parse(Language::Python, src)),
        pairs(&[("Three", 4)])
    );
    let src = "local x\n--[==[\n  a\n  [[Three]]\n]==]\n";
    assert_eq!(
        wikilinks(&parse(Language::Lua, src)),
        pairs(&[("Three", 4)])
    );
}

#[test]
fn crlf_line_endings_give_the_same_links_and_lines_as_lf() {
    for (lang, lf) in [
        (
            Language::Rust,
            "// one [[A]]\n/* two\n   [[B]] */\n/// three [[C]]\nfn f() {}\n",
        ),
        (Language::Python, "\"\"\"doc\n[[A]]\"\"\"\n# [[B]]\nx = 1\n"),
        (Language::Shell, "# [[A]]\necho hi\n# [[B]]\n"),
    ] {
        let crlf = lf.replace('\n', "\r\n");
        let a = parse(lang, lf);
        let b = parse(lang, &crlf);
        assert!(!a.wikilinks.is_empty());
        assert_eq!(wikilinks(&a), wikilinks(&b), "{lang:?}");
        // The raw text carries no stray `\r`.
        assert!(b.wikilinks.iter().all(|w| !w.raw.contains('\r')));
    }
}

#[test]
fn a_link_on_the_last_line_with_no_trailing_newline_is_found() {
    assert_eq!(
        wikilinks(&parse(Language::Rust, "fn f() {}\n// [[Last]]")),
        pairs(&[("Last", 2)])
    );
    assert_eq!(
        wikilinks(&parse(Language::Python, "x = 1\n# [[Last]]")),
        pairs(&[("Last", 2)])
    );
    assert_eq!(
        wikilinks(&parse(Language::Shell, "echo\n# [[Last]]")),
        pairs(&[("Last", 2)])
    );
    assert_eq!(
        wikilinks(&parse(Language::C, "int x;\n/* [[Last]] */")),
        pairs(&[("Last", 2)])
    );
}

#[test]
fn a_non_ascii_link_target_keeps_its_text_line_and_span() {
    let src = "// héllo\n// see [[café notes]] and [[日本語#見出し|別名]]\nfn f() {}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        wikilinks(&unit),
        pairs(&[("café notes", 2), ("日本語#見出し|別名", 2)])
    );
    assert_eq!(unit.wikilinks[1].target, "日本語");
    assert_eq!(unit.wikilinks[1].anchor.as_deref(), Some("見出し"));
    assert_eq!(unit.wikilinks[1].alias.as_deref(), Some("別名"));
    for w in &unit.wikilinks {
        assert_eq!(&src[w.span.0..w.span.1], format!("[[{}]]", w.raw));
    }
}

#[test]
fn a_block_comment_holding_a_markdown_fence_still_gives_its_wikilink() {
    // Code files do not run the Markdown fence exclusion: the comment
    // boundary is the only filter for a wikilink.
    let src = "/* ``` [[x]] ``` */\nint y;\n";
    assert_eq!(wikilinks(&parse(Language::C, src)), pairs(&[("x", 1)]));
    let src = "/*\n```\n[[Fenced]]\n```\n*/\n";
    assert_eq!(wikilinks(&parse(Language::C, src)), pairs(&[("Fenced", 3)]));
}

#[test]
fn a_wikilink_cannot_close_outside_its_comment() {
    // The `]]` after the comment is code.
    let unit = parse(Language::C, "/* [[a */ int b[1]]; \n");
    assert!(unit.wikilinks.is_empty(), "{:?}", unit.wikilinks);
}

#[test]
fn markdown_links_are_read_inside_comments_only() {
    let src = concat!(
        "// see [the plan](Plan.md) now\n",           // 1
        "const char *s = \"[not](decoy.md)\";\n",     // 2
        "/*\n",                                       // 3
        " * more [two](dir/Two.md#h) and [[Wiki]]\n", // 4
        " */\n",                                      // 5
    );
    let unit = parse(Language::C, src);
    let md: Vec<(&str, &str, usize)> = unit
        .markdown_links
        .iter()
        .map(|m| (m.text.as_str(), m.target.as_str(), m.line))
        .collect();
    assert_eq!(md, [("the plan", "Plan.md", 1), ("two", "dir/Two.md#h", 4)]);
    assert_eq!(wikilinks(&unit), pairs(&[("Wiki", 4)]));
    // Each one has the exact span of its destination.
    for m in &unit.markdown_links {
        let (s, e) = m.span.expect("inline link has a span");
        assert_eq!(&src[s..e], m.target);
    }
    // And they become `markdown`-kind rows after the wikilinks.
    let rows = unit.to_new_links(1);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, "wikilink");
    assert_eq!(rows[1].kind, "markdown");
    assert_eq!(rows[2].to_raw, "dir/Two.md#h");
}

#[test]
fn markdown_links_in_an_indented_python_docstring_are_read() {
    let src = "def f():\n    \"\"\"Summary.\n\n    See [docs](Docs.md).\n    \"\"\"\n";
    let unit = parse(Language::Python, src);
    assert_eq!(unit.markdown_links.len(), 1);
    assert_eq!(unit.markdown_links[0].target, "Docs.md");
    assert_eq!(unit.markdown_links[0].line, 4);
}

#[test]
fn a_symbol_anchor_is_stored_as_a_heading_anchor_with_no_symbol_resolution() {
    // F3: `[[scan.rs#walk_and_write]]` is an ordinary link with an
    // anchor; this PR adds nothing that treats the anchor as a symbol.
    let unit = parse(
        Language::Rust,
        "// see [[scan.rs#walk_and_write]]\nfn f() {}\n",
    );
    assert_eq!(unit.wikilinks.len(), 1);
    let w = &unit.wikilinks[0];
    assert_eq!(w.raw, "scan.rs#walk_and_write");
    assert_eq!(w.target, "scan.rs");
    assert_eq!(w.anchor.as_deref(), Some("walk_and_write"));
    assert!(w.source_prefix.is_none() && w.alias.is_none());
    assert!(unit.blocks.is_empty());
    // A block-id anchor is also just a link (`^` marks it).
    let unit = parse(Language::Rust, "// [[@tephra:a.rs#^k3f9a2]]\n");
    assert_eq!(unit.wikilinks[0].source_prefix.as_deref(), Some("tephra"));
    assert_eq!(unit.wikilinks[0].anchor.as_deref(), Some("^k3f9a2"));
}

#[test]
fn rust_doc_comment_forms_are_comments() {
    let src = "//! inner [[Inner]]\n/// outer [[Outer]]\n/** block outer [[BlockOuter]] */\n/*! block inner [[BlockInner]] */\nfn f() {}\n";
    assert_eq!(
        wikilinks(&parse(Language::Rust, src)),
        pairs(&[
            ("Inner", 1),
            ("Outer", 2),
            ("BlockOuter", 3),
            ("BlockInner", 4)
        ])
    );
}

#[test]
fn comments_in_every_position_are_read() {
    // Trailing, between statements, inside a function body.
    let src = "fn f() {\n    let x = 1; // trailing [[T]]\n    // inside [[I]]\n}\n";
    assert_eq!(
        wikilinks(&parse(Language::Rust, src)),
        pairs(&[("T", 2), ("I", 3)])
    );
    let src = "function f() {\n  return 1; /* [[A]] */\n}\n";
    assert_eq!(
        wikilinks(&parse(Language::JavaScript, src)),
        pairs(&[("A", 2)])
    );
}

#[test]
fn an_empty_file_and_a_file_with_no_comment_give_nothing() {
    for lang in Language::ALL {
        let unit = parse(lang, "");
        assert!(
            unit.wikilinks.is_empty() && unit.diagnostics.is_empty(),
            "{lang:?}"
        );
    }
    assert!(parse(Language::Rust, "fn main() {}\n").wikilinks.is_empty());
}

#[test]
fn the_unit_holds_the_content_hash_of_the_bytes_it_was_given() {
    let a = parse(Language::Rust, "// [[A]]\n");
    let b = parse(Language::Rust, "// [[A]]\n");
    let c = parse(Language::Rust, "// [[B]]\n");
    assert_eq!(a.content_hash, b.content_hash);
    assert_ne!(a.content_hash, c.content_hash);
    assert_eq!(a.content_hash.len(), 64);
}
