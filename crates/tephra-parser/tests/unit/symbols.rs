//! G4: one table per bundled grammar. Each fixture under
//! `tests/fixtures/symbols/` holds at least one example of every
//! symbol kind G4 lists for that language, and the table asserts the
//! whole symbol list: `(kind, name, first_line, start, end, parent)`.
//! Real tree-sitter parses, no mocks. Lines are 1-based and the range
//! is `start..=end`.

use std::path::Path;

use tephra_parser::{parse_code_unit, Language};

type Row = (
    &'static str,
    &'static str,
    usize,
    usize,
    usize,
    Option<&'static str>,
);

fn fixture(name: &str) -> (std::path::PathBuf, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/symbols")
        .join(name);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    (path, content)
}

fn check(lang: Language, name: &str, expected: &[Row]) {
    let (path, content) = fixture(name);
    assert_eq!(
        Language::from_path(&path, content.lines().next()),
        Some(lang),
        "{name}"
    );
    let unit = parse_code_unit(&path, lang, &content);
    let got: Vec<Row> = unit
        .symbols
        .iter()
        .map(|s| {
            (
                s.kind,
                leak(&s.name),
                s.line,
                s.start_line,
                s.end_line,
                s.parent.as_deref().map(leak),
            )
        })
        .collect();
    assert_eq!(got, expected, "{name}");
    assert!(
        unit.diagnostics.is_empty(),
        "{name}: {:?}",
        unit.diagnostics
    );
}

/// Table rows are `&'static`; a test may leak the few bytes.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

#[test]
fn rust_rs_symbols() {
    check(
        Language::Rust,
        "rust.rs",
        &[
            ("struct", "Thing", 6, 4, 8, None),
            ("enum", "Reason", 12, 10, 15, None),
            ("trait", "Shape", 17, 17, 23, None),
            ("method", "area", 19, 19, 19, Some("Shape")),
            ("method", "name", 20, 20, 22, Some("Shape")),
            ("impl", "Reason", 25, 25, 31, None),
            ("method", "code", 26, 26, 30, Some("Reason")),
            ("function", "helper", 27, 27, 27, Some("code")),
            ("impl", "Reason", 33, 33, 37, None),
            ("method", "fmt", 34, 34, 36, Some("Reason")),
            ("mod", "inner", 39, 39, 43, None),
            ("const", "LIMIT", 40, 40, 40, Some("inner")),
            ("static", "NAME", 41, 41, 41, Some("inner")),
            ("function", "deep", 42, 42, 42, Some("inner")),
            ("mod", "declared", 45, 45, 45, None),
            ("const", "TOP", 47, 47, 47, None),
            ("static", "COUNTER", 48, 48, 48, None),
            ("type", "Alias", 49, 49, 49, None),
            ("function", "free", 51, 51, 51, None),
        ],
    );
}

#[test]
fn python_py_symbols() {
    check(
        Language::Python,
        "python.py",
        &[
            ("variable", "TOP", 4, 4, 4, None),
            ("variable", "a", 5, 5, 5, None),
            ("variable", "b", 5, 5, 5, None),
            ("variable", "TYPED", 6, 6, 6, None),
            ("variable", "handler", 7, 7, 7, None),
            ("function", "free", 10, 10, 20, None),
            ("class", "Inside", 14, 14, 15, Some("free")),
            ("function", "nested", 17, 17, 18, Some("free")),
            ("function", "decorated", 25, 23, 26, None),
            ("class", "Thing", 29, 29, 44, None),
            ("method", "method", 32, 32, 33, Some("Thing")),
            ("method", "prop", 36, 35, 37, Some("Thing")),
            ("method", "amethod", 39, 39, 40, Some("Thing")),
            ("class", "Sub", 42, 42, 44, Some("Thing")),
            ("method", "deep", 43, 43, 44, Some("Sub")),
            ("function", "coroutine", 47, 47, 48, None),
            ("variable", "CONDITIONAL", 52, 52, 52, None),
        ],
    );
}

#[test]
fn typescript_ts_symbols() {
    check(
        Language::TypeScript,
        "typescript.ts",
        &[
            ("function", "add", 2, 1, 6, None),
            ("function", "helper", 4, 4, 4, Some("add")),
            ("class", "Greeter", 8, 8, 19, None),
            ("method", "constructor", 10, 10, 12, Some("Greeter")),
            ("method", "greet", 13, 13, 15, Some("Greeter")),
            ("method", "title", 16, 16, 18, Some("Greeter")),
            ("interface", "Shape", 21, 21, 23, None),
            ("method", "area", 22, 22, 22, Some("Shape")),
            ("enum", "Colour", 25, 25, 27, None),
            ("type", "Pair", 29, 29, 29, None),
            ("mod", "Util", 31, 31, 34, None),
            ("const", "LIMIT", 32, 32, 32, Some("Util")),
            ("function", "run", 33, 33, 33, Some("Util")),
            ("const", "MAX", 36, 36, 36, None),
            ("variable", "counter", 37, 37, 37, None),
            ("function", "arrow", 38, 38, 38, None),
            ("class", "Decorated", 42, 41, 44, None),
        ],
    );
}

#[test]
fn javascript_js_symbols() {
    check(
        Language::JavaScript,
        "javascript.js",
        &[
            ("function", "add", 2, 1, 5, None),
            ("class", "Greeter", 7, 7, 17, None),
            ("method", "constructor", 8, 8, 10, Some("Greeter")),
            ("method", "greet", 11, 11, 13, Some("Greeter")),
            ("method", "make", 14, 14, 16, Some("Greeter")),
            ("const", "MAX", 19, 19, 19, None),
            ("variable", "counter", 20, 20, 20, None),
            ("variable", "legacy", 21, 21, 21, None),
            ("function", "arrow", 22, 22, 22, None),
            ("function", "fn", 23, 23, 23, None),
        ],
    );
}

#[test]
fn jsx_jsx_symbols() {
    check(
        Language::Jsx,
        "jsx.jsx",
        &[
            ("function", "App", 3, 3, 6, None),
            ("class", "Card", 8, 8, 12, None),
            ("method", "render", 9, 9, 11, Some("Card")),
            ("function", "Button", 14, 14, 14, None),
            ("const", "VERSION", 15, 15, 15, None),
        ],
    );
}

#[test]
fn tsx_tsx_symbols() {
    check(
        Language::Tsx,
        "tsx.tsx",
        &[
            ("interface", "Props", 1, 1, 3, None),
            ("function", "Button", 6, 5, 9, None),
            ("class", "Card", 11, 11, 15, None),
            ("method", "render", 12, 12, 14, Some("Card")),
            ("const", "SIZE", 17, 17, 17, None),
            ("type", "Id", 18, 18, 18, None),
        ],
    );
}

#[test]
fn go_go_symbols() {
    check(
        Language::Go,
        "go.go",
        &[
            ("struct", "Server", 6, 5, 8, None),
            ("interface", "Handler", 10, 10, 12, None),
            ("type", "ID", 14, 14, 14, None),
            ("struct", "Left", 17, 17, 17, None),
            ("type", "Right", 18, 18, 18, None),
            ("const", "Version", 21, 21, 21, None),
            ("const", "A", 24, 24, 24, None),
            ("const", "B", 25, 25, 25, None),
            ("variable", "Debug", 28, 28, 28, None),
            ("variable", "x", 31, 31, 31, None),
            ("variable", "y", 31, 31, 31, None),
            ("function", "Start", 35, 34, 38, None),
            ("method", "Run", 40, 40, 40, Some("Server")),
            ("method", "Name", 42, 42, 42, Some("Server")),
        ],
    );
}

#[test]
fn shell_sh_symbols() {
    check(
        Language::Shell,
        "shell.sh",
        &[
            ("function", "greet", 3, 3, 6, None),
            ("function", "inner", 5, 5, 5, Some("greet")),
            ("function", "build", 8, 8, 10, None),
            ("function", "test_all", 12, 12, 14, None),
        ],
    );
}

#[test]
fn yaml_yaml_symbols() {
    check(
        Language::Yaml,
        "yaml.yaml",
        &[
            ("key", "name", 2, 2, 2, None),
            ("key", "version", 3, 3, 3, None),
            ("key", "dependencies", 4, 4, 6, None),
            ("key", "nested", 7, 7, 8, None),
            ("key", "quoted", 9, 9, 9, None),
            ("key", "second", 11, 11, 11, None),
        ],
    );
}

#[test]
fn toml_toml_symbols() {
    check(
        Language::Toml,
        "toml.toml",
        &[
            ("key", "name", 2, 2, 2, None),
            ("key", "version", 3, 3, 3, None),
            ("table", "package", 5, 5, 7, None),
            ("table", "dependencies.serde", 8, 8, 10, None),
            ("array_table", "bin", 11, 11, 13, None),
            ("array_table", "bin", 14, 14, 15, None),
        ],
    );
}

#[test]
fn c_c_symbols() {
    check(
        Language::C,
        "c.c",
        &[
            ("struct", "Point", 5, 4, 8, None),
            ("enum", "Color", 10, 10, 10, None),
            ("union", "Value", 12, 12, 12, None),
            ("type", "Size", 14, 14, 16, None),
            ("variable", "counter", 18, 18, 18, None),
            ("static", "hidden", 19, 19, 19, None),
            ("const", "ANSWER", 20, 20, 20, None),
            ("function", "add", 26, 25, 29, None),
            ("function", "helper", 31, 31, 31, None),
        ],
    );
}

#[test]
fn cpp_cpp_symbols() {
    check(
        Language::Cpp,
        "cpp.cpp",
        &[
            ("mod", "app", 3, 3, 25, None),
            ("variable", "version", 5, 5, 5, Some("app")),
            ("class", "Widget", 7, 7, 15, Some("app")),
            ("method", "Widget", 9, 9, 9, Some("Widget")),
            ("method", "~Widget", 10, 10, 10, Some("Widget")),
            ("method", "size", 11, 11, 11, Some("Widget")),
            ("method", "draw", 12, 12, 12, Some("Widget")),
            ("struct", "Config", 17, 17, 19, Some("app")),
            ("method", "reset", 18, 18, 18, Some("Config")),
            ("enum", "Mode", 21, 21, 21, Some("app")),
            ("type", "Name", 23, 23, 23, Some("app")),
            ("method", "size", 27, 27, 27, Some("Widget")),
            ("function", "over", 29, 29, 29, None),
            ("function", "over", 30, 30, 30, None),
            ("function", "identity", 33, 32, 33, None),
        ],
    );
}

#[test]
fn sample_java_symbols() {
    check(
        Language::Java,
        "Sample.java",
        &[
            ("class", "Service", 5, 3, 21, None),
            ("method", "Service", 9, 9, 9, Some("Service")),
            ("method", "run", 12, 11, 12, Some("Service")),
            ("method", "run", 14, 14, 14, Some("Service")),
            ("class", "Inner", 16, 16, 18, Some("Service")),
            ("method", "go", 17, 17, 17, Some("Inner")),
            ("enum", "Level", 20, 20, 20, Some("Service")),
            ("method", "high", 20, 20, 20, Some("Level")),
            ("interface", "Shape", 23, 23, 26, None),
            ("method", "area", 24, 24, 24, Some("Shape")),
            ("method", "label", 25, 25, 25, Some("Shape")),
            ("enum", "Direction", 28, 28, 28, None),
            ("record", "Point", 30, 30, 32, None),
            ("method", "sum", 31, 31, 31, Some("Point")),
        ],
    );
}

#[test]
fn swift_swift_symbols() {
    check(
        Language::Swift,
        "swift.swift",
        &[
            ("class", "Person", 4, 3, 11, None),
            ("method", "init", 8, 8, 8, Some("Person")),
            ("method", "greet", 10, 10, 10, Some("Person")),
            ("struct", "Point", 13, 13, 15, None),
            ("method", "norm", 14, 14, 14, Some("Point")),
            ("enum", "Mode", 17, 17, 20, None),
            ("method", "label", 19, 19, 19, Some("Mode")),
            ("interface", "Shape", 22, 22, 24, None),
            ("method", "area", 23, 23, 23, Some("Shape")),
            ("impl", "Person", 26, 26, 28, None),
            ("method", "area", 27, 27, 27, Some("Person")),
            ("class", "Counter", 30, 30, 32, None),
            ("method", "bump", 31, 31, 31, Some("Counter")),
            ("function", "top", 34, 34, 38, None),
            ("function", "nested", 35, 35, 35, Some("top")),
            ("const", "LIMIT", 40, 40, 40, None),
            ("variable", "counter", 41, 41, 41, None),
            ("type", "Name", 42, 42, 42, None),
            ("function", "café", 43, 43, 43, None),
        ],
    );
}

#[test]
fn lua_lua_symbols() {
    check(
        Language::Lua,
        "lua.lua",
        &[
            ("variable", "M", 2, 2, 2, None),
            ("variable", "VERSION", 3, 3, 3, None),
            ("variable", "count", 4, 4, 4, None),
            ("function", "helper", 6, 6, 7, None),
            ("method", "run", 9, 9, 10, Some("M")),
            ("method", "method", 12, 12, 13, Some("M")),
            ("function", "global_fn", 15, 15, 18, None),
            ("function", "inner", 16, 16, 16, Some("global_fn")),
            ("function", "handler", 20, 20, 20, None),
        ],
    );
}
