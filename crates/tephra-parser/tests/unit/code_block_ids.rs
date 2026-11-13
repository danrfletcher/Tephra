//! G13: which symbol owns a `^id` comment, and what line the `blocks`
//! row stores. A table per comment style: `//` (Rust), `#` (Python)
//! and `--` (Lua). Real tree-sitter parses, no mocks.

use std::path::Path;

use tephra_parser::{parse_code_unit, DiagnosticKind, Language, ParsedUnit};

fn parse(lang: Language, src: &str) -> ParsedUnit {
    parse_code_unit(Path::new("t"), lang, src)
}

/// `(id, stored line, owning symbol's name or None)` per block.
fn owners(unit: &ParsedUnit) -> Vec<(String, usize, Option<String>)> {
    unit.blocks
        .iter()
        .map(|b| {
            (
                b.block_id.clone(),
                b.line,
                b.symbol.map(|i| unit.symbols[i].name.clone()),
            )
        })
        .collect()
}

struct Case {
    name: &'static str,
    src: &'static str,
    /// `(id, line, owner)`.
    expect: &'static [(&'static str, usize, Option<&'static str>)],
}

fn run(lang: Language, cases: &[Case]) {
    for case in cases {
        let unit = parse(lang, case.src);
        let got = owners(&unit);
        let want: Vec<(String, usize, Option<String>)> = case
            .expect
            .iter()
            .map(|(id, line, owner)| (id.to_string(), *line, owner.map(str::to_string)))
            .collect();
        assert_eq!(got, want, "{lang:?} case `{}`", case.name);
    }
}

#[test]
fn slash_slash_comments() {
    run(
        Language::Rust,
        &[
            Case {
                name: "end of the first line",
                src: "fn walk() { // ^k3f9a2\n    1;\n}\n",
                expect: &[("k3f9a2", 1, Some("walk"))],
            },
            Case {
                name: "own line directly above",
                src: "// ^k3f9a2\nfn walk() {}\n",
                expect: &[("k3f9a2", 2, Some("walk"))],
            },
            Case {
                name: "doc comment and attribute between",
                src: "// ^k3f9a2\n/// Docs.\n#[inline]\nfn walk() {}\n",
                expect: &[("k3f9a2", 4, Some("walk"))],
            },
            Case {
                name: "id inside the doc comment's own line",
                src: "/// Docs. ^k3f9a2\n#[inline]\nfn walk() {}\n",
                expect: &[("k3f9a2", 3, Some("walk"))],
            },
            Case {
                name: "blank line between",
                src: "// ^k3f9a2\n\nfn walk() {}\n",
                expect: &[("k3f9a2", 1, None)],
            },
            Case {
                name: "blank line between a doc comment and the symbol",
                src: "// ^k3f9a2\n/// Docs.\n\nfn walk() {}\n",
                expect: &[("k3f9a2", 1, None)],
            },
            Case {
                name: "an ordinary comment between",
                src: "// ^k3f9a2\n// plain note\nfn walk() {}\n",
                expect: &[("k3f9a2", 1, None)],
            },
            Case {
                name: "end of file",
                src: "fn walk() {}\n// ^k3f9a2\n",
                expect: &[("k3f9a2", 2, None)],
            },
            Case {
                name: "inside a string literal",
                src: "fn walk() {\n    let s = \"see ^abc123\";\n}\n",
                expect: &[],
            },
            Case {
                name: "inside a raw string and a char-like text",
                src: "fn walk() {\n    let s = r#\"x ^abc123\"#;\n}\n",
                expect: &[],
            },
            Case {
                name: "typed id",
                src: "fn walk() {} // ^summary-v2\n",
                expect: &[("summary-v2", 1, Some("walk"))],
            },
            Case {
                name: "block comment at the end of the line",
                src: "fn walk() {} /* ^k3f9a2 */\n",
                expect: &[("k3f9a2", 1, Some("walk"))],
            },
            Case {
                name: "inside a body, not on a symbol line",
                src: "fn walk() {\n    let x = 1; // ^k3f9a2\n}\n",
                expect: &[("k3f9a2", 2, None)],
            },
            Case {
                name: "the id must end the comment",
                src: "fn walk() {} // ^k3f9a2 and more\n",
                expect: &[],
            },
            Case {
                name: "an id on a method's line belongs to the method",
                src: "impl T {\n    // ^a1\n    fn m(&self) {}\n}\n",
                expect: &[("a1", 3, Some("m"))],
            },
            Case {
                name: "two ids for one symbol both attach to it",
                src: "// ^one\nfn walk() {} // ^two\n",
                expect: &[("one", 2, Some("walk")), ("two", 2, Some("walk"))],
            },
        ],
    );
}

#[test]
fn hash_comments() {
    run(
        Language::Python,
        &[
            Case {
                name: "end of the def line",
                src: "def walk():  # ^k3f9a2\n    pass\n",
                expect: &[("k3f9a2", 1, Some("walk"))],
            },
            Case {
                name: "own line above",
                src: "# ^k3f9a2\ndef walk():\n    pass\n",
                expect: &[("k3f9a2", 2, Some("walk"))],
            },
            Case {
                name: "decorator between",
                src: "# ^k3f9a2\n@deco\ndef walk():\n    pass\n",
                expect: &[("k3f9a2", 3, Some("walk"))],
            },
            Case {
                name: "blank line between",
                src: "# ^k3f9a2\n\ndef walk():\n    pass\n",
                expect: &[("k3f9a2", 1, None)],
            },
            Case {
                name: "inside a string and a docstring",
                src: "def walk():\n    \"\"\"Docs ^abc123\"\"\"\n    s = \"see ^abc123\"\n",
                expect: &[],
            },
            Case {
                name: "a module-level assignment owns its id",
                src: "LIMIT = 3  # ^lim-1\n",
                expect: &[("lim-1", 1, Some("LIMIT"))],
            },
            Case {
                name: "end of file",
                src: "x = 1\n# ^k3f9a2",
                expect: &[("k3f9a2", 2, None)],
            },
        ],
    );
}

#[test]
fn dash_dash_comments() {
    run(
        Language::Lua,
        &[
            Case {
                name: "end of the function line",
                src: "function walk() -- ^k3f9a2\nend\n",
                expect: &[("k3f9a2", 1, Some("walk"))],
            },
            Case {
                name: "own line above",
                src: "-- ^k3f9a2\nlocal function walk() end\n",
                expect: &[("k3f9a2", 2, Some("walk"))],
            },
            Case {
                name: "doc comment between",
                src: "-- ^k3f9a2\n--- Docs.\nfunction walk() end\n",
                expect: &[("k3f9a2", 3, Some("walk"))],
            },
            Case {
                name: "blank line between",
                src: "-- ^k3f9a2\n\nfunction walk() end\n",
                expect: &[("k3f9a2", 1, None)],
            },
            Case {
                name: "inside a string",
                src: "local s = \"see ^abc123\"\nlocal t = [[ ^abc123 ]]\n",
                expect: &[],
            },
            Case {
                name: "long bracket comment",
                src: "--[[ ^k3f9a2 ]]\nfunction walk() end\n",
                expect: &[("k3f9a2", 2, Some("walk"))],
            },
        ],
    );
}

#[test]
fn the_character_rule_is_the_prose_one() {
    // Letters, digits and `-`; an underscore or a dot ends the match.
    let unit = parse(
        Language::Rust,
        "fn a() {} // ^abc-123\nfn b() {} // ^a_b\nfn c() {} // ^\n",
    );
    assert_eq!(
        owners(&unit),
        [("abc-123".to_string(), 1, Some("a".to_string()))]
    );
}

#[test]
fn a_repeated_id_keeps_every_occurrence_and_reports_a_duplicate() {
    let unit = parse(
        Language::Rust,
        "// ^k3f9a2\nfn a() {}\n// ^k3f9a2\nfn b() {}\n",
    );
    assert_eq!(
        owners(&unit),
        [
            ("k3f9a2".to_string(), 2, Some("a".to_string())),
            ("k3f9a2".to_string(), 4, Some("b".to_string())),
        ]
    );
    assert!(unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::DuplicateBlockId));
}

#[test]
fn to_new_blocks_with_symbols_names_the_owner_and_to_new_blocks_never_does() {
    let unit = parse(Language::Rust, "// ^one\nfn a() {}\n\n// ^two\n");
    let ids = [100, 200];
    let rows = unit.to_new_blocks_with_symbols(7, &ids);
    let shape: Vec<_> = rows
        .iter()
        .map(|b| (b.block_id, b.line, b.symbol_id))
        .collect();
    assert_eq!(shape, [(Some("one"), 2, Some(100)), (Some("two"), 4, None)]);
    assert!(rows.iter().all(|b| b.unit_id == 7));

    let plain = unit.to_new_blocks(7);
    assert!(plain.iter().all(|b| b.symbol_id.is_none()));
}

#[test]
fn crlf_does_not_leak_into_the_id() {
    let unit = parse(Language::Rust, "// ^k3f9a2\r\nfn a() {} // ^b2\r\n");
    let ids: Vec<_> = unit.blocks.iter().map(|b| b.block_id.as_str()).collect();
    assert_eq!(ids, ["k3f9a2", "b2"]);
    assert!(unit.blocks.iter().all(|b| b.line == 2));
}
