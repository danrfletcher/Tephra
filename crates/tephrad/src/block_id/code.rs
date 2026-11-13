//! `block.ensure_id` for code units (PR-12.F1, G11, G12, G13): give the
//! symbol a line or a name points at an ID comment, and return the ID.
//!
//! Same layout as the prose half in [`super`]: pure functions, each
//! tested on its own, which [`super::BlockIdEngine`] wires to the cache
//! and the filesystem.
//!
//! - [`comment_marker`]: the line comment syntax of a language
//!   ([`COMMENT_STYLES`], one entry for every bundled grammar).
//! - [`resolve_symbol`]: which symbol a line or a name means.
//! - [`plan_code`]: the whole decision for one file's content: the
//!   symbol's existing ID, or the content with one new ID comment
//!   written at the end of its definition line.
//!
//! The ID goes on the line that carries the definition, never on the
//! attribute, decorator or doc-comment line above it (those lines belong
//! to the symbol, so a `line` on one of them still targets it). It is
//! `// ^id` (or `# ^id`, `-- ^id`) after the line's last character, or,
//! when the line already ends with a line comment, ` ^id` at the end of
//! that comment. Nothing else in the file changes: the new content is
//! the old content with one insertion at one line end.
//!
//! Whether the end of the line is somewhere a comment can go is the
//! syntax tree's answer ([`tephra_parser::classify_line_end`]), not a
//! guess from the text: a line that ends inside a string or a block
//! comment, or on a parse error, is refused. As a last check the new
//! content is parsed again and must show the same symbols with the new
//! ID read back on this symbol and nothing else changed, so the marker
//! can never land somewhere the parser would not read it back.

use std::path::Path;

use rand::Rng;
use tephra_parser::{
    classify_line_end, parse_code_unit, DiagnosticKind, Language, LineEnd, ParsedUnit, Symbol,
};

use super::{
    ids_in_file, line_spans, new_id, refused, BlockIdError, BlockTarget, LocateError, Plan,
};

// ---------------------------------------------------------------
// Comment syntax
// ---------------------------------------------------------------

/// The line comment marker of every bundled grammar (G11): `//` for
/// TypeScript, JavaScript (TSX and JSX with them), Rust, Go, Swift, Java,
/// C and C++; `#` for Python, Shell, YAML and TOML; `--` for Lua. A test
/// fails if a [`Language`] has no entry.
pub const COMMENT_STYLES: &[(Language, &str)] = &[
    (Language::TypeScript, "//"),
    (Language::Tsx, "//"),
    (Language::JavaScript, "//"),
    (Language::Jsx, "//"),
    (Language::Rust, "//"),
    (Language::Go, "//"),
    (Language::Swift, "//"),
    (Language::Java, "//"),
    (Language::C, "//"),
    (Language::Cpp, "//"),
    (Language::Python, "#"),
    (Language::Shell, "#"),
    (Language::Yaml, "#"),
    (Language::Toml, "#"),
    (Language::Lua, "--"),
];

/// `lang`'s line comment marker in [`COMMENT_STYLES`], or a refusal
/// ("no line comment syntax") for a language the table has no entry for.
pub fn comment_marker(lang: Language) -> Result<&'static str, BlockIdError> {
    comment_marker_in(COMMENT_STYLES, lang)
}

/// [`comment_marker`] over an explicit table, so the refusal can be
/// tested with a table that lacks a language.
pub fn comment_marker_in(
    table: &[(Language, &'static str)],
    lang: Language,
) -> Result<&'static str, BlockIdError> {
    table
        .iter()
        .find(|(l, _)| *l == lang)
        .map(|(_, marker)| *marker)
        .ok_or_else(|| {
            refused(format!(
                "no line comment syntax for {}: Tephra cannot write a block ID comment there",
                lang.name()
            ))
        })
}

// ---------------------------------------------------------------
// Which symbol
// ---------------------------------------------------------------

/// `Parent.name`, or `name` for a top-level symbol.
fn qualified(symbol: &Symbol) -> String {
    match &symbol.parent {
        Some(parent) => format!("{parent}.{}", symbol.name),
        None => symbol.name.clone(),
    }
}

/// The index in `symbols` of the symbol `target` means in the file `unit`
/// (the id, for messages) of `total_lines` lines.
///
/// - `Line(n)`: the innermost symbol whose range (its attribute,
///   decorator and doc-comment lines included, through the end of its
///   definition) holds line `n`; of equal ranges, the first. No such
///   symbol is "no symbol on line N".
/// - `Symbol(name)`: the symbols named `name`, or whose `Parent.name` is
///   `name`, not counting `impl` blocks when exactly one other symbol
///   is left. Exactly one is the answer; none is "no symbol named X in
///   <unit>"; several are an error listing each as `Parent.name` and its
///   line, which `Parent.name` then settles.
pub fn resolve_symbol(
    symbols: &[Symbol],
    unit: &str,
    total_lines: usize,
    target: &BlockTarget,
) -> Result<usize, BlockIdError> {
    match target {
        BlockTarget::Line(line) => {
            let line = usize::try_from(*line).unwrap_or(usize::MAX);
            if line == 0 {
                return Err(LocateError::LineZero.into());
            }
            if line > total_lines {
                return Err(LocateError::OutOfRange {
                    line,
                    total: total_lines,
                }
                .into());
            }
            symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| s.start_line <= line && line <= s.end_line)
                .min_by_key(|(i, s)| (s.end_line - s.start_line, *i))
                .map(|(i, _)| i)
                .ok_or_else(|| refused(format!("no symbol on line {line}")))
        }
        BlockTarget::Symbol(name) => {
            let found: Vec<usize> = symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| s.name == *name || qualified(s) == *name)
                .map(|(i, _)| i)
                .collect();
            // `impl Foo` is Foo's own implementation, not a second thing
            // named Foo: when one definition is left without the impls,
            // that is the one the name means. (`Foo.method` and a line
            // number still reach the impl's own lines.)
            let defined: Vec<usize> = found
                .iter()
                .copied()
                .filter(|&i| symbols[i].kind != "impl")
                .collect();
            let found = if defined.len() == 1 { defined } else { found };
            match found.as_slice() {
                [] => Err(refused(format!("no symbol named {name} in {unit}"))),
                [only] => Ok(*only),
                many => {
                    let mut listed: Vec<&Symbol> = many.iter().map(|&i| &symbols[i]).collect();
                    listed.sort_by_key(|s| s.line);
                    let list: Vec<String> = listed
                        .iter()
                        .map(|s| format!("{} (line {}, {})", qualified(s), s.line, s.kind))
                        .collect();
                    Err(refused(format!(
                        "{} symbols are named {name} in {unit}: {}; name one as Parent.name, or give its line",
                        many.len(),
                        list.join(", ")
                    )))
                }
            }
        }
    }
}

// ---------------------------------------------------------------
// plan_code: the pure core
// ---------------------------------------------------------------

/// The language of the code file at `path` with this `content` (a
/// shebang names it when the path has no extension).
fn language_of(path: &Path, content: &str) -> Result<Language, BlockIdError> {
    Language::from_path(path, content.lines().next()).ok_or_else(|| {
        refused(format!(
            "no bundled grammar for {}: Tephra cannot find its symbols",
            path.display()
        ))
    })
}

/// How many syntax errors the parse reported.
fn syntax_errors(parsed: &ParsedUnit) -> usize {
    parsed
        .diagnostics
        .iter()
        .filter(|d| d.kind == DiagnosticKind::SyntaxErrors)
        .count()
}

/// Decides what `block.ensure_id` does for `target` in the code file
/// `content` at `path` (`unit` is its id, for messages). Pure: no IO, the
/// random source is injected. The result is [`Plan::Existing`] (nothing to
/// write) or [`Plan::Insert`], whose content differs from `content` by
/// one insertion at the end of the symbol's definition line.
pub fn plan_code<R: Rng + ?Sized>(
    path: &Path,
    unit: &str,
    content: &str,
    target: &BlockTarget,
    rng: &mut R,
) -> Result<Plan, BlockIdError> {
    let lang = language_of(path, content)?;
    let marker = comment_marker(lang)?;

    let parsed = parse_code_unit(path, lang, content);
    if let Some(failure) = parsed.diagnostics.iter().find(|d| {
        matches!(
            d.kind,
            DiagnosticKind::ParseTimeout
                | DiagnosticKind::ParseSkippedTooLarge
                | DiagnosticKind::ParseFailed
        )
    }) {
        return Err(refused(format!(
            "cannot place ID: {unit} could not be parsed ({})",
            failure.message
        )));
    }

    let spans = line_spans(content);
    let index = resolve_symbol(&parsed.symbols, unit, spans.len(), target)?;
    let symbol = &parsed.symbols[index];

    // G12/G13: an ID already on the symbol (ending its definition line,
    // or on a comment line directly above it) is returned as it is.
    if let Some(existing) = parsed.blocks.iter().find(|b| b.symbol == Some(index)) {
        return Ok(Plan::Existing {
            id: existing.block_id.clone(),
        });
    }

    let line = symbol.line;
    let Some(&(start, end)) = spans.get(line - 1) else {
        return Err(refused(format!("line {line} is not in the file")));
    };
    let text = &content[start..end];
    if text.trim_end().ends_with('\\') {
        return Err(refused(format!(
            "cannot place ID: line {line} ends with a backslash continuation, and a comment there would break it"
        )));
    }
    let separator = if text.ends_with(char::is_whitespace) {
        ""
    } else {
        " "
    };

    let id = new_id(&ids_in_file(&parsed), rng)
        .ok_or_else(|| refused("could not draw an unused block ID"))?;
    let insertion = match classify_line_end(lang, content, line) {
        Some(LineEnd::Code) => format!("{separator}{marker} ^{id}"),
        Some(LineEnd::LineComment) => format!("{separator}^{id}"),
        Some(LineEnd::InString) => {
            return Err(refused(format!(
                "cannot place ID: line ends inside a string (line {line}): a comment there would become part of the string"
            )))
        }
        Some(LineEnd::InBlockComment) => {
            return Err(refused(format!(
                "cannot place ID: line ends inside block comment (line {line}): a comment there would become part of it"
            )))
        }
        Some(LineEnd::ParseError) => {
            return Err(refused(format!(
                "cannot place ID: parse error on line {line}"
            )))
        }
        Some(LineEnd::Unparsed) | None => {
            return Err(refused(format!(
                "cannot place ID: line {line} of {unit} could not be parsed"
            )))
        }
    };

    let mut new_content = String::with_capacity(content.len() + insertion.len());
    new_content.push_str(&content[..end]);
    new_content.push_str(&insertion);
    new_content.push_str(&content[end..]);

    // The marker must read back as this symbol's ID, and as nothing else
    // having changed.
    let after = parse_code_unit(path, lang, &new_content);
    let holds_it = after.symbols == parsed.symbols
        && after.blocks.len() == parsed.blocks.len() + 1
        && after
            .blocks
            .iter()
            .filter(|b| b.block_id == id && b.symbol == Some(index))
            .count()
            == 1
        && after.blocks.iter().filter(|b| b.block_id == id).count() == 1
        && syntax_errors(&after) <= syntax_errors(&parsed);
    if !holds_it {
        return Err(refused(format!(
            "cannot place ID: the end of line {line} cannot carry an ID comment (it is inside a string, a literal or markup)"
        )));
    }
    Ok(Plan::Insert { id, new_content })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn rng() -> StdRng {
        StdRng::seed_from_u64(12)
    }

    fn plan(path: &str, content: &str, target: &BlockTarget) -> Result<Plan, BlockIdError> {
        plan_code(Path::new(path), "@t:file", content, target, &mut rng())
    }

    fn symbol(name: &str) -> BlockTarget {
        BlockTarget::Symbol(name.into())
    }

    /// The new content of a successful insert, and its ID.
    fn inserted(path: &str, content: &str, target: &BlockTarget) -> (String, String) {
        match plan(path, content, target) {
            Ok(Plan::Insert { id, new_content }) => (id, new_content),
            other => panic!("{path}: expected an insert, got {other:?}"),
        }
    }

    fn reason(path: &str, content: &str, target: &BlockTarget) -> String {
        match plan(path, content, target) {
            Err(BlockIdError::Refused(m)) => m,
            other => panic!("{path}: expected a refusal, got {other:?}"),
        }
    }

    // -- comment syntax -------------------------------------------------

    #[test]
    fn every_bundled_grammar_has_a_comment_style_and_it_is_the_right_one() {
        for lang in Language::ALL {
            assert!(
                comment_marker(lang).is_ok(),
                "{lang:?} has no entry in COMMENT_STYLES"
            );
        }
        // The table has no entry for a language that is not bundled and
        // none twice.
        assert_eq!(COMMENT_STYLES.len(), Language::ALL.len());
        for (lang, marker) in [
            (Language::TypeScript, "//"),
            (Language::Tsx, "//"),
            (Language::JavaScript, "//"),
            (Language::Jsx, "//"),
            (Language::Rust, "//"),
            (Language::Go, "//"),
            (Language::Swift, "//"),
            (Language::Java, "//"),
            (Language::C, "//"),
            (Language::Cpp, "//"),
            (Language::Python, "#"),
            (Language::Shell, "#"),
            (Language::Yaml, "#"),
            (Language::Toml, "#"),
            (Language::Lua, "--"),
        ] {
            assert_eq!(comment_marker(lang).unwrap(), marker, "{lang:?}");
        }
    }

    #[test]
    fn a_language_with_no_entry_is_refused_with_no_line_comment_syntax() {
        let table = [(Language::Rust, "//")];
        assert_eq!(comment_marker_in(&table, Language::Rust), Ok("//"));
        match comment_marker_in(&table, Language::Python) {
            Err(BlockIdError::Refused(m)) => assert!(m.contains("no line comment syntax"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    // -- placement -------------------------------------------------------

    #[test]
    fn a_plain_line_gets_the_marker_after_a_space_at_its_end() {
        let (id, new) = inserted("a.rs", "fn one() {}\nfn two() {}\n", &symbol("two"));
        assert_eq!(new, format!("fn one() {{}}\nfn two() {{}} // ^{id}\n"));
        let (id, new) = inserted("a.py", "def f():\n    pass\n", &symbol("f"));
        assert_eq!(new, format!("def f(): # ^{id}\n    pass\n"));
        let (id, new) = inserted("a.lua", "local function f()\nend\n", &symbol("f"));
        assert_eq!(new, format!("local function f() -- ^{id}\nend\n"));
    }

    #[test]
    fn a_line_that_already_ends_with_a_comment_gets_the_id_inside_it() {
        let (id, new) = inserted("a.rs", "fn x() { // TODO\n}\n", &symbol("x"));
        assert_eq!(new, format!("fn x() {{ // TODO ^{id}\n}}\n"));
        assert_eq!(new.matches("//").count(), 1, "no second marker");
        let (id, new) = inserted("a.py", "def f():  # note\n    pass\n", &symbol("f"));
        assert_eq!(new, format!("def f():  # note ^{id}\n    pass\n"));
    }

    #[test]
    fn trailing_whitespace_is_kept_and_not_doubled() {
        let (id, new) = inserted("a.rs", "fn x() { \t\n}\n", &symbol("x"));
        assert_eq!(new, format!("fn x() {{ \t// ^{id}\n}}\n"));
    }

    #[test]
    fn an_existing_id_is_returned_whether_trailing_or_on_the_line_above() {
        for (path, content, want) in [
            ("a.rs", "fn x() {} // ^Summary-1\n", "Summary-1"),
            ("a.rs", "// ^above\nfn x() {}\n", "above"),
            ("a.py", "def x(): # ^abc123\n    pass\n", "abc123"),
        ] {
            assert_eq!(
                plan(path, content, &symbol("x")),
                Ok(Plan::Existing { id: want.into() }),
                "{path}"
            );
        }
    }

    #[test]
    fn a_line_anywhere_in_the_range_targets_the_symbol_and_the_inner_one_wins() {
        let src = "/// doc\n#[derive(Debug)]\npub enum E {\n    A,\n}\n\nimpl E {\n    fn m(&self) {}\n}\n";
        for line in [1, 2, 3, 4, 5] {
            let (id, new) = inserted("a.rs", src, &BlockTarget::Line(line));
            assert!(
                new.contains(&format!("pub enum E {{ // ^{id}\n")),
                "line {line}"
            );
        }
        // Line 8 is inside the impl and is the method's own definition.
        let (id, new) = inserted("a.rs", src, &BlockTarget::Line(8));
        assert!(
            new.contains(&format!("fn m(&self) {{}} // ^{id}\n")),
            "{new}"
        );
        // Line 7 is the impl's own.
        let (id, new) = inserted("a.rs", src, &BlockTarget::Line(7));
        assert!(new.contains(&format!("impl E {{ // ^{id}\n")), "{new}");
    }

    // -- refusals -----------------------------------------------------------

    #[test]
    fn each_refusal_names_its_reason_and_writes_nothing() {
        let cases: &[(&str, &str, BlockTarget, &str)] = &[
            (
                "a.py",
                "def f(): \"\"\"doc\n    more\"\"\"\n",
                symbol("f"),
                "line ends inside a string",
            ),
            (
                "a.rs",
                "const R: &str = r#\"abc\ndef\"#;\n",
                symbol("R"),
                "line ends inside a string",
            ),
            (
                "a.js",
                "function f() { return `abc\ndef`; }\n",
                symbol("f"),
                "line ends inside a string",
            ),
            (
                "a.toml",
                "k = \"\"\"\nabc\"\"\"\n",
                symbol("k"),
                "line ends inside a string",
            ),
            (
                "a.yaml",
                "key: \"abc\n  def\"\n",
                symbol("key"),
                "line ends inside a string",
            ),
            (
                "a.c",
                "void f(void) { /* start\n*/ }\n",
                symbol("f"),
                "inside block comment",
            ),
            (
                "A.java",
                "class A { /* start\n*/ }\n",
                symbol("A"),
                "inside block comment",
            ),
            (
                "a.js",
                "function f() { /* start\n*/ }\n",
                symbol("f"),
                "inside block comment",
            ),
            (
                "a.lua",
                "local function f() --[[ start\n]] end\n",
                symbol("f"),
                "inside block comment",
            ),
            (
                "a.c",
                "void f(void) { \\\n}\n",
                symbol("f"),
                "backslash continuation",
            ),
            (
                "a.sh",
                "f() { echo a \\\n  b; }\n",
                symbol("f"),
                "backslash continuation",
            ),
            (
                "a.py",
                "def f(a, \\\n      b):\n    pass\n",
                symbol("f"),
                "backslash continuation",
            ),
            (
                "a.rs",
                "fn a() {}\n",
                symbol("nope"),
                "no symbol named nope in @t:file",
            ),
            (
                "a.rs",
                "fn a() {}\n\nconst B: u8 = 1;\n",
                BlockTarget::Line(2),
                "no symbol on line 2",
            ),
            ("a.rs", "fn a() {}\n", BlockTarget::Line(0), "line 0"),
            ("a.rs", "fn a() {}\n", BlockTarget::Line(5), "out of range"),
            ("a.xyz", "fn a() {}\n", symbol("a"), "no bundled grammar"),
        ];
        for (path, content, target, want) in cases {
            let msg = reason(path, content, target);
            assert!(msg.contains(want), "{path}: {msg}");
        }
    }

    #[test]
    fn a_parse_error_on_the_definition_line_is_refused_and_one_elsewhere_is_not() {
        // Line 1 parses cleanly although line 2 is broken.
        let (id, new) = inserted("a.rs", "fn good() {\n    let x = ;\n}\n", &symbol("good"));
        assert!(
            new.starts_with(&format!("fn good() {{ // ^{id}\n")),
            "{new}"
        );
        // The definition line itself is broken.
        let content = "fn bad( {\n}\n";
        match plan("a.rs", content, &BlockTarget::Line(1)) {
            Err(BlockIdError::Refused(m)) => {
                assert_eq!(m, "cannot place ID: parse error on line 1")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_shared_name_lists_every_symbol_with_its_line_and_parent() {
        let src = "class A:\n    def run(self):\n        pass\n\nclass B:\n    def run(self):\n        pass\n";
        let msg = reason("a.py", src, &symbol("run"));
        assert!(msg.contains("2 symbols are named run"), "{msg}");
        assert!(msg.contains("A.run (line 2, method)"), "{msg}");
        assert!(msg.contains("B.run (line 6, method)"), "{msg}");
        let (id, new) = inserted("a.py", src, &symbol("B.run"));
        assert!(new.contains(&format!("    def run(self): # ^{id}\n        pass\n")));
        assert_eq!(new.matches('^').count(), 1);
        // The qualified name of the other one.
        let (_, new) = inserted("a.py", src, &symbol("A.run"));
        assert!(new.starts_with("class A:\n    def run(self): # ^"), "{new}");
    }

    #[test]
    fn a_type_and_its_impl_block_are_one_name_for_a_symbol_request() {
        let src = "pub struct S;\n\nimpl S {\n    fn f(&self) {}\n}\n";
        let (id, new) = inserted("a.rs", src, &symbol("S"));
        assert!(
            new.starts_with(&format!("pub struct S; // ^{id}\n")),
            "{new}"
        );
        // The impl itself is still reachable by its line.
        let (id, new) = inserted("a.rs", src, &BlockTarget::Line(3));
        assert!(new.contains(&format!("impl S {{ // ^{id}\n")), "{new}");
        // Two impls and no type: still ambiguous.
        let two = "impl S {}\nimpl S {}\n";
        let msg = reason("a.rs", two, &symbol("S"));
        assert!(msg.contains("2 symbols are named S"), "{msg}");
    }

    // -- layout --------------------------------------------------------------

    #[test]
    fn crlf_no_final_newline_and_unicode_are_kept_byte_for_byte() {
        let (id, new) = inserted("a.py", "def é():\r\n    pass\r\n", &symbol("é"));
        assert_eq!(new, format!("def é(): # ^{id}\r\n    pass\r\n"));
        let (id, new) = inserted("a.rs", "fn last() {}", &symbol("last"));
        assert_eq!(new, format!("fn last() {{}} // ^{id}"));
    }

    // -- ids -------------------------------------------------------------------

    #[test]
    fn a_new_id_is_six_lowercase_alphanumerics_over_many_seeded_draws() {
        for seed in 0..300u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let plan = plan_code(
                Path::new("a.rs"),
                "@t:a.rs",
                "fn x() {}\n",
                &symbol("x"),
                &mut rng,
            );
            let Ok(Plan::Insert { id, .. }) = plan else {
                panic!("{plan:?}")
            };
            assert!(
                id.len() == 6
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "{id}"
            );
        }
    }

    #[test]
    fn a_colliding_draw_is_redrawn_in_a_code_file() {
        // The ID the seeded source draws first for this file...
        let mut first = StdRng::seed_from_u64(77);
        let Ok(Plan::Insert { id: taken, .. }) = plan_code(
            Path::new("a.rs"),
            "@t:a.rs",
            "fn x() {}\n",
            &symbol("x"),
            &mut first,
        ) else {
            panic!()
        };
        // ...is already on another symbol of the file, in another case.
        let src = format!("fn used() {{}} // ^{}\nfn x() {{}}\n", taken.to_uppercase());
        let mut again = StdRng::seed_from_u64(77);
        let Ok(Plan::Insert { id, new_content }) =
            plan_code(Path::new("a.rs"), "@t:a.rs", &src, &symbol("x"), &mut again)
        else {
            panic!()
        };
        assert_ne!(id, taken);
        assert!(new_content.contains(&format!("fn x() {{}} // ^{id}")));
    }

    // -- F4 as a property --------------------------------------------------

    #[test]
    fn an_insert_changes_exactly_one_line_and_that_line_only_grows_at_its_end() {
        let sources: &[(&str, &str)] = &[
            ("a.rs", "use a;\n\n/// d\n#[derive(Debug)]\npub enum E { A }\n\nfn f() { // c\n}\n"),
            ("a.py", "import x\n\n@d\ndef f(): # c\n    pass\n\nclass C:\n    def m(self):\n        pass\n"),
            ("a.go", "package p\n\nfunc F() {\n}\n\ntype T struct{}\n"),
            ("a.ts", "export class A {\n  m() {}\n}\nfunction g() {}\n"),
            ("a.yaml", "a: 1\nb:\n  c: 2\n"),
            ("a.toml", "[a]\nb = 1\n[[c]]\nd = 2\n"),
            ("a.sh", "#!/bin/sh\nf() {\n  :\n}\n"),
            ("a.lua", "local function f()\nend\nfunction g() end\n"),
        ];
        for (path, src) in sources {
            let lang = Language::from_path(Path::new(path), src.lines().next()).unwrap();
            let parsed = parse_code_unit(Path::new(path), lang, src);
            assert!(!parsed.symbols.is_empty(), "{path}");
            for s in &parsed.symbols {
                let Ok(Plan::Insert { new_content, .. }) =
                    plan(path, src, &BlockTarget::Line(s.line as u64))
                else {
                    continue;
                };
                let old: Vec<&str> = src.split_inclusive('\n').collect();
                let new: Vec<&str> = new_content.split_inclusive('\n').collect();
                assert_eq!(old.len(), new.len(), "{path} {}", s.name);
                let changed: Vec<usize> = (0..old.len()).filter(|&i| old[i] != new[i]).collect();
                assert_eq!(changed, vec![s.line - 1], "{path} {}", s.name);
                let old_text = old[s.line - 1].trim_end_matches('\n');
                assert!(
                    new[s.line - 1].starts_with(old_text),
                    "{path}: {:?} is not a prefix of {:?}",
                    old_text,
                    new[s.line - 1]
                );
            }
        }
    }
}
