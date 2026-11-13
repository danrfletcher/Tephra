//! `classify_line_end`: what the end of a line of a code file is, for
//! every bundled grammar, from the real tree-sitter parse.

use tephra_parser::{classify_line_end, Language, LineEnd};

use Language::*;
use LineEnd::*;

/// `(language, content, line, expected)`.
type Case = (Language, &'static str, usize, LineEnd);

fn check(cases: &[Case]) {
    for (lang, content, line, want) in cases {
        assert_eq!(
            classify_line_end(*lang, content, *line),
            Some(*want),
            "{lang:?} line {line} of {content:?}"
        );
    }
}

#[test]
fn a_line_that_ends_in_code_is_code_in_every_language() {
    check(&[
        (Python, "def f(a):\n    pass\n", 1, Code),
        (TypeScript, "function f() {\n}\n", 1, Code),
        (Tsx, "const A = () => <div/>;\n", 1, Code),
        (JavaScript, "function f() {\n}\n", 1, Code),
        (Jsx, "const A = () => <div/>;\n", 1, Code),
        (Rust, "pub enum E {\n    A,\n}\n", 1, Code),
        (Go, "package p\nfunc f() {\n}\n", 2, Code),
        (Shell, "foo() {\n  echo hi\n}\n", 1, Code),
        (Yaml, "key: value\nother: 1\n", 1, Code),
        (Toml, "[server]\nport = 1\n", 1, Code),
        (C, "void f(void) {\n}\n", 1, Code),
        (Cpp, "void f() {\n}\n", 1, Code),
        (Java, "class A {\n}\n", 1, Code),
        (Swift, "func f() {\n}\n", 1, Code),
        (Lua, "local function f()\nend\n", 1, Code),
    ]);
}

#[test]
fn a_line_that_ends_with_a_line_comment_says_so() {
    check(&[
        (Python, "def f(): # TODO\n    pass\n", 1, LineComment),
        (Rust, "fn x() { // TODO\n}\n", 1, LineComment),
        (TypeScript, "function f() { // TODO\n}\n", 1, LineComment),
        (JavaScript, "function f() { // TODO\n}\n", 1, LineComment),
        (Go, "func f() { // TODO\n}\n", 1, LineComment),
        (Shell, "foo() { # TODO\n}\n", 1, LineComment),
        (Yaml, "key: value # TODO\n", 1, LineComment),
        (Toml, "key = 1 # TODO\n", 1, LineComment),
        (C, "void f(void) { // TODO\n}\n", 1, LineComment),
        (Cpp, "void f() { // TODO\n}\n", 1, LineComment),
        (Java, "class A { // TODO\n}\n", 1, LineComment),
        (Swift, "func f() { // TODO\n}\n", 1, LineComment),
        (Lua, "local function f() -- TODO\nend\n", 1, LineComment),
        // No final newline, and trailing whitespace after the comment.
        (Rust, "fn x() {} // TODO", 1, LineComment),
        (Rust, "fn x() { // TODO  \n}\n", 1, LineComment),
    ]);
}

#[test]
fn a_block_comment_that_closes_on_the_line_is_code() {
    check(&[
        (C, "int x; /* done */\n", 1, Code),
        (JavaScript, "function f() { /* done */\n}\n", 1, Code),
        (Lua, "local function f() --[[ done ]]\nend\n", 1, Code),
        (Swift, "func f() { /* done */\n}\n", 1, Code),
    ]);
}

#[test]
fn a_line_that_ends_inside_a_block_comment_says_so() {
    check(&[
        (C, "int x; /* start\nend */\n", 1, InBlockComment),
        (Cpp, "int x; /* start\nend */\n", 1, InBlockComment),
        (Java, "class A { /* start\n*/ }\n", 1, InBlockComment),
        (
            JavaScript,
            "function f() { /* start\n*/ }\n",
            1,
            InBlockComment,
        ),
        (
            TypeScript,
            "function f() { /* start\n*/ }\n",
            1,
            InBlockComment,
        ),
        (Go, "func f() { /* start\n*/ }\n", 1, InBlockComment),
        (Rust, "fn f() { /* start\n*/ }\n", 1, InBlockComment),
        (Swift, "func f() { /* start\n*/ }\n", 1, InBlockComment),
        (
            Lua,
            "local function f() --[[ start\nmore ]] end\n",
            1,
            InBlockComment,
        ),
        (
            Lua,
            "local function f() --[==[ start\nmore ]==] end\n",
            1,
            InBlockComment,
        ),
    ]);
}

#[test]
fn a_line_that_ends_inside_a_multi_line_string_says_so() {
    check(&[
        (Python, "def f(): \"\"\"doc\n    more\"\"\"\n", 1, InString),
        (Rust, "const S: &str = r#\"abc\ndef\"#;\n", 1, InString),
        (Rust, "const S: &str = \"abc\ndef\";\n", 1, InString),
        (JavaScript, "const t = `abc\ndef`;\n", 1, InString),
        (TypeScript, "const t = `abc\ndef`;\n", 1, InString),
        (Tsx, "const t = `abc\ndef`;\n", 1, InString),
        (Toml, "key = \"\"\"\nabc\"\"\"\n", 1, InString),
        (Yaml, "key: \"abc\n  def\"\n", 1, InString),
        (Yaml, "key: 'abc\n  def'\n", 1, InString),
        (Yaml, "key: first\n  second\n", 1, InString),
        (Go, "package p\nvar s = `abc\ndef`\n", 2, InString),
        (
            Java,
            "class A { String s = \"\"\"\n  hi\n  \"\"\"; }\n",
            1,
            InString,
        ),
        (Swift, "let s = \"\"\"\nabc\n\"\"\"\n", 1, InString),
        (Lua, "local s = [[abc\ndef]]\n", 1, InString),
        (Shell, "x=\"a\nb\"\n", 1, InString),
        (Shell, "x='a\nb'\n", 1, InString),
    ]);
}

#[test]
fn a_yaml_block_scalar_indicator_ends_its_line_in_code() {
    check(&[
        (Yaml, "key: |\n  text\n", 1, Code),
        (Yaml, "key: |-\n  text\n", 1, Code),
        (Yaml, "key: >\n  text\n", 1, Code),
        (Yaml, "key: | \n  text\n", 1, Code),
    ]);
}

#[test]
fn a_break_between_tokens_of_one_construct_is_code() {
    check(&[
        (Python, "def f(a,\n      b):\n    pass\n", 1, Code),
        (Python, "x = (\"abc\"\n     \"def\")\n", 1, Code),
        (Toml, "k = [1,\n 2]\n", 1, Code),
        (Rust, "fn f(\n    a: u8,\n) {}\n", 1, Code),
        (Cpp, "template <typename T>\nvoid f() {}\n", 1, Code),
    ]);
}

#[test]
fn a_parse_error_on_the_line_is_reported_and_one_elsewhere_is_not() {
    check(&[
        // The line itself is broken.
        (Rust, "fn broken( {\n}\n", 1, ParseError),
        (Yaml, "key: 'a#b\nother: 1\n", 1, ParseError),
        // The break is on line 2; line 1 parses cleanly.
        (
            Rust,
            "fn good() { // ok\n    let x = ;\n}\n",
            1,
            LineComment,
        ),
        (Rust, "fn good() {\n    let x = ;\n}\n", 1, Code),
        (Python, "def good():\n    x = = 1\n", 1, Code),
    ]);
}

#[test]
fn a_line_that_does_not_exist_is_none() {
    assert_eq!(classify_line_end(Rust, "fn a() {}\n", 0), None);
    assert_eq!(classify_line_end(Rust, "fn a() {}\n", 2), None);
    assert_eq!(classify_line_end(Rust, "", 1), None);
}

#[test]
fn crlf_lines_are_classified_by_their_text_not_their_ending() {
    check(&[
        (Rust, "fn x() { // TODO\r\n}\r\n", 1, LineComment),
        (Rust, "fn x() {\r\n}\r\n", 1, Code),
        (
            Python,
            "def f(): \"\"\"doc\r\n    more\"\"\"\r\n",
            1,
            InString,
        ),
        (C, "int x; /* start\r\nend */\r\n", 1, InBlockComment),
    ]);
}

#[test]
fn a_file_too_big_to_parse_is_unparsed() {
    let big = format!(
        "fn a() {{}}\n{}",
        "// padding line\n".repeat(tephra_parser::MAX_CODE_PARSE_BYTES / 16 + 1)
    );
    assert_eq!(classify_line_end(Rust, &big, 1), Some(Unparsed));
}
