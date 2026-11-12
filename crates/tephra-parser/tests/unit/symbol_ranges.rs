//! G4 ranges: attribute, decorator and doc-comment ownership, the
//! reported-line rule, the blank-line cut-off, CRLF, Unicode, nesting,
//! duplicate names, empty files and a large generated file.

use std::path::Path;
use std::time::{Duration, Instant};

use tephra_parser::{parse_code_unit, Language, ParsedUnit, Symbol};

fn parse(lang: Language, src: &str) -> ParsedUnit {
    parse_code_unit(Path::new("t"), lang, src)
}

/// `(kind, name, line, start, end)` of every symbol.
fn shape(unit: &ParsedUnit) -> Vec<(&str, &str, usize, usize, usize)> {
    unit.symbols
        .iter()
        .map(|s| (s.kind, s.name.as_str(), s.line, s.start_line, s.end_line))
        .collect()
}

fn find<'a>(unit: &'a ParsedUnit, name: &str) -> &'a Symbol {
    unit.symbols
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol {name}: {:?}", unit.symbols))
}

#[test]
fn rust_attributes_and_doc_comments_are_in_the_range_and_the_line_is_the_fn() {
    let src = "/// Doc one.\n/// Doc two.\n#[derive(Debug)]\n#[allow(dead_code)]\npub struct S;\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(shape(&unit), [("struct", "S", 5, 1, 5)]);
}

#[test]
fn rust_inner_doc_and_block_doc_comments_count_too() {
    let src = "/** Block doc. */\nfn a() {}\n\n/*! inner */\nfn b() {}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        shape(&unit),
        [("function", "a", 2, 1, 2), ("function", "b", 5, 4, 5)]
    );
}

#[test]
fn a_blank_line_cuts_the_range_off() {
    let src = "/// Belongs to nobody.\n\n#[inline]\n\nfn f() {}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(shape(&unit), [("function", "f", 5, 5, 5)]);

    // An ordinary comment is not documentation either.
    let unit = parse(Language::Rust, "// just a note\nfn g() {}\n");
    assert_eq!(shape(&unit), [("function", "g", 2, 2, 2)]);
}

#[test]
fn a_trailing_doc_comment_of_the_previous_item_is_not_taken() {
    let src = "fn a() {} /// trailing\nfn b() {}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        shape(&unit),
        [("function", "a", 1, 1, 1), ("function", "b", 2, 2, 2)]
    );
}

#[test]
fn a_rust_attribute_sharing_its_line_with_a_definition_is_not_taken_by_the_next_one() {
    let src = "#[test] fn a() {}\nfn b() {}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        shape(&unit),
        [("function", "a", 1, 1, 1), ("function", "b", 2, 2, 2)]
    );
}

#[test]
fn a_typescript_decorated_member_on_one_line_is_not_taken_by_the_next_member() {
    let src = "class C {\n  @Input() name: string;\n  ngOnInit() {}\n}\n";
    let unit = parse(Language::TypeScript, src);
    let init = find(&unit, "ngOnInit");
    assert_eq!((init.line, init.start_line, init.end_line), (3, 3, 3));
}

#[test]
fn a_java_annotated_method_on_one_line_is_not_taken_by_the_next_method() {
    let src = "class A {\n  @Override public String toString() { return \"\"; }\n  public void x() {}\n}\n";
    let unit = parse(Language::Java, src);
    let x = find(&unit, "x");
    assert_eq!((x.line, x.start_line, x.end_line), (3, 3, 3));
    let to_string = find(&unit, "toString");
    assert_eq!((to_string.start_line, to_string.end_line), (2, 2));
}

#[test]
fn attributes_followed_only_by_attributes_or_comments_still_belong_to_the_next_definition() {
    let src = "#[a] #[b] // note\nfn f() {}\n\n#[c] /* x */\n#[d]\nfn g() {}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        shape(&unit),
        [("function", "f", 2, 1, 2), ("function", "g", 6, 4, 6)]
    );

    let src = "class C {\n  @A() @B()\n  m() {}\n}\n";
    let unit = parse(Language::TypeScript, src);
    let m = find(&unit, "m");
    assert_eq!((m.line, m.start_line), (3, 2));
}

#[test]
fn python_decorators_are_in_the_range_but_the_line_is_the_def() {
    let src = "@one\n@two(1,\n     2)\ndef f():\n    pass\n\n@d\nclass C:\n    pass\n";
    let unit = parse(Language::Python, src);
    assert_eq!(
        shape(&unit),
        [("function", "f", 4, 1, 5), ("class", "C", 8, 7, 9)]
    );
}

#[test]
fn jsdoc_and_decorators_are_in_the_typescript_range() {
    let src = "/**\n * Docs.\n */\n@Component({})\nexport class C {}\n";
    let unit = parse(Language::TypeScript, src);
    assert_eq!(shape(&unit), [("class", "C", 5, 1, 5)]);
}

#[test]
fn a_java_class_with_only_annotations_above_it_reports_the_class_line() {
    let src = "@Foo\n@Bar\nclass Only {}\n";
    let unit = parse(Language::Java, src);
    assert_eq!(shape(&unit), [("class", "Only", 3, 1, 3)]);
}

#[test]
fn a_cpp_template_header_is_above_the_definition_line() {
    let src = "template <typename T>\nT id(T v) { return v; }\n";
    let unit = parse(Language::Cpp, src);
    assert_eq!(shape(&unit), [("function", "id", 2, 1, 2)]);
}

#[test]
fn go_doc_comment_block_is_in_the_range() {
    let src = "package p\n\n// Start does it.\n// More.\nfunc Start() {}\n";
    let unit = parse(Language::Go, src);
    assert_eq!(shape(&unit), [("function", "Start", 5, 3, 5)]);
}

#[test]
fn lua_doc_comments_are_in_the_range() {
    let src = "--- Adds.\n-- not doc\nfunction add() end\n";
    let unit = parse(Language::Lua, src);
    // `---` is documentation; the plain `--` line directly above the
    // function cuts the chain, since it is not.
    assert_eq!(shape(&unit), [("function", "add", 3, 3, 3)]);
}

#[test]
fn crlf_line_endings_give_the_same_lines_as_lf() {
    let lf = "/// Doc.\n#[derive(Debug)]\nstruct S {\n    a: u8,\n}\n\nfn f() {}\n";
    let crlf = lf.replace('\n', "\r\n");
    let a = parse(Language::Rust, lf);
    let b = parse(Language::Rust, &crlf);
    assert_eq!(shape(&a), shape(&b));
    assert_eq!(
        shape(&b),
        [("struct", "S", 3, 1, 5), ("function", "f", 7, 7, 7)]
    );

    let py = "@d\r\ndef f():\r\n    pass\r\n\r\nclass C:\r\n    pass\r\n";
    let unit = parse(Language::Python, py);
    assert_eq!(
        shape(&unit),
        [("function", "f", 2, 1, 3), ("class", "C", 5, 5, 6)]
    );
}

#[test]
fn unicode_names_and_multibyte_text_keep_lines_correct() {
    let py = "# héllo wörld — ünïcode 日本語\ndef café():\n    s = \"日本語\"\n    return s\n\n\ndef naïve():\n    pass\n";
    let unit = parse(Language::Python, py);
    assert_eq!(
        shape(&unit),
        [
            ("function", "café", 2, 2, 4),
            ("function", "naïve", 7, 7, 8)
        ]
    );

    let swift = "// 日本語 🎉\nfunc café() {}\nfunc 日本() {}\n";
    let unit = parse(Language::Swift, swift);
    assert_eq!(
        shape(&unit),
        [("function", "café", 2, 2, 2), ("function", "日本", 3, 3, 3)]
    );
}

#[test]
fn nested_named_functions_and_classes_are_recorded_with_their_parent() {
    let src = "def outer():\n    class Inner:\n        def m(self):\n            pass\n    def helper():\n        pass\n    local = 1\n    fn = lambda: 1\n";
    let unit = parse(Language::Python, src);
    let rows: Vec<_> = unit
        .symbols
        .iter()
        .map(|s| (s.kind, s.name.as_str(), s.parent.as_deref()))
        .collect();
    assert_eq!(
        rows,
        [
            ("function", "outer", None),
            ("class", "Inner", Some("outer")),
            ("method", "m", Some("Inner")),
            ("function", "helper", Some("outer")),
        ],
        "locals and a lambda assigned inside a function are not variables"
    );
}

#[test]
fn a_closure_assigned_inside_a_rust_fn_is_not_a_symbol() {
    let src = "fn f() {\n    let c = || 1;\n    const LOCAL: u8 = 1;\n    fn inner() {}\n}\n";
    let unit = parse(Language::Rust, src);
    assert_eq!(
        shape(&unit),
        [("function", "f", 1, 1, 5), ("function", "inner", 4, 4, 4)]
    );
    assert_eq!(find(&unit, "inner").parent.as_deref(), Some("f"));
}

#[test]
fn only_module_level_python_assignments_are_variables() {
    let src = "A = 1\nif True:\n    B = 2\nclass C:\n    D = 3\ndef f():\n    E = 4\nfor i in range(3):\n    pass\n";
    let unit = parse(Language::Python, src);
    let names: Vec<_> = unit.symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["A", "B", "C", "f"]);
}

#[test]
fn duplicate_names_each_keep_their_own_row() {
    let rust = "enum X { A }\nimpl X {\n    fn a(&self) {}\n}\nimpl std::fmt::Display for X {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}\n";
    let unit = parse(Language::Rust, rust);
    let kinds: Vec<_> = unit
        .symbols
        .iter()
        .filter(|s| s.name == "X")
        .map(|s| s.kind)
        .collect();
    assert_eq!(kinds, ["enum", "impl", "impl"]);

    let java = "class A {\n  void m() {}\n  void m(int a) {}\n  void m(String s) {}\n}\n";
    let unit = parse(Language::Java, java);
    assert_eq!(unit.symbols.iter().filter(|s| s.name == "m").count(), 3);

    let cpp = "int over(int a) { return a; }\nint over(double a) { return 1; }\n";
    let unit = parse(Language::Cpp, cpp);
    assert_eq!(unit.symbols.len(), 2);
}

#[test]
fn rust_impl_name_is_the_type_even_with_generics_paths_and_references() {
    let src = "impl<T> Foo<T> {}\nimpl a::b::Bar for &Baz {}\nimpl Tr for Vec<u8> {}\n";
    let unit = parse(Language::Rust, src);
    let names: Vec<_> = unit.symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Foo", "Baz", "Vec"]);
}

#[test]
fn an_empty_file_and_a_comment_only_file_have_no_symbols_and_no_diagnostics() {
    for (lang, src) in [
        (Language::Rust, ""),
        (Language::Rust, "// only a comment\n/* and another */\n"),
        (Language::Python, "# nothing\n"),
        (Language::Python, ""),
        (Language::Yaml, ""),
        (Language::Toml, "# comment\n"),
        (Language::Lua, "-- c\n"),
        (Language::Shell, "# c\n"),
    ] {
        let unit = parse(lang, src);
        assert!(unit.symbols.is_empty(), "{lang:?} {src:?}");
        assert!(unit.diagnostics.is_empty(), "{lang:?} {src:?}");
    }
}

#[test]
fn a_ten_thousand_line_generated_file_gives_every_symbol_within_the_time_bound() {
    let mut src = String::new();
    for i in 0..2500 {
        src.push_str(&format!(
            "/// Doc {i}.\n#[derive(Debug)]\npub struct S{i} {{\n    a: u8,\n}}\n\nimpl S{i} {{\n    fn m{i}(&self) {{}}\n}}\n\n"
        ));
    }
    assert!(src.lines().count() >= 10_000);
    let started = Instant::now();
    let unit = parse(Language::Rust, &src);
    let took = started.elapsed();
    assert_eq!(unit.symbols.len(), 7500);
    let last = unit.symbols.last().unwrap();
    assert_eq!((last.name.as_str(), last.line), ("m2499", 24_998));
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn reparsing_gives_identical_symbols() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/symbols/registry.rs"),
    )
    .unwrap();
    let a = parse(Language::Rust, &src);
    let b = parse(Language::Rust, &src);
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(a.blocks, b.blocks);
}

#[test]
fn registry_rs_from_origin_main_has_the_spec_symbols() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/symbols/registry.rs");
    let src = std::fs::read_to_string(path).unwrap();
    let unit = parse(Language::Rust, &src);
    let enum_ = unit
        .symbols
        .iter()
        .find(|s| s.kind == "enum" && s.name == "ConflictReason")
        .expect("enum ConflictReason");
    assert_eq!((enum_.line, enum_.start_line, enum_.end_line), (73, 72, 81));
    let imp = unit
        .symbols
        .iter()
        .find(|s| s.kind == "impl" && s.name == "ConflictReason")
        .expect("impl ConflictReason");
    assert_eq!(imp.line, 83);
    let fmt = unit
        .symbols
        .iter()
        .find(|s| s.kind == "method" && s.name == "fmt")
        .expect("method fmt");
    assert_eq!(fmt.parent.as_deref(), Some("ConflictReason"));
    assert!(unit.diagnostics.is_empty(), "{:?}", unit.diagnostics);
}
