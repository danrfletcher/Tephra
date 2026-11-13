//! `block.ensure_id` on code units (PR-12.F1: G11, G12, G13, E5, E6, E12,
//! F3, F4) end to end: real temp-dir sources scanned through the real
//! `SourceRegistry` and its cache, the real `BlockIdEngine` writing real
//! files through an `AtomicFs` that records every call, and one fixture
//! per bundled grammar under `tests/fixtures/block-id-code/`.
//!
//! Every refusal is checked to have written nothing: the file's bytes
//! and mtime are read back, `AtomicFs` was never called, the watcher was
//! never paused and no temp file is left. Every write is checked to have
//! changed exactly one line, whose old text is a prefix of its new text.

mod fixtures;

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tephra_cache::Cache;
use tephrad::block_id::{BlockIdEngine, BlockIdError, BlockTarget, EnsureIdWire};
use tephrad::refactor::{AtomicFs, RealFs, WatcherPauseControl};
use tephrad::registry::SourceRegistry;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/block-id-code")
}

// ---------------------------------------------------------------
// The rig
// ---------------------------------------------------------------

/// An [`AtomicFs`] that records every call, does the real thing, and
/// can be told to fail one step.
#[derive(Default)]
struct RecordingFs {
    calls: Mutex<Vec<String>>,
    fail_rename: bool,
}

impl RecordingFs {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn log(&self, what: &str, path: &Path) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{what} {}", path.display()));
    }
}

impl AtomicFs for RecordingFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> std::io::Result<()> {
        self.log("write_new", path);
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: std::fs::Permissions) -> std::io::Result<()> {
        self.log("set_permissions", path);
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> std::io::Result<()> {
        self.log("sync", path);
        RealFs.sync(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.log("rename", to);
        if self.fail_rename {
            return Err(std::io::Error::other("injected rename failure"));
        }
        RealFs.rename(from, to)
    }
}

/// A [`WatcherPauseControl`] that records pause and resume, with the
/// paths the resume was told to forget.
#[derive(Default)]
struct RecordingWatcher {
    log: Mutex<Vec<String>>,
}

impl RecordingWatcher {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl WatcherPauseControl for RecordingWatcher {
    fn pause_all(&self) {
        self.log.lock().unwrap().push("pause".to_string());
    }
    fn resume_all(&self, written: &HashSet<PathBuf>) {
        let mut paths: Vec<String> = written.iter().map(|p| p.display().to_string()).collect();
        paths.sort();
        self.log
            .lock()
            .unwrap()
            .push(format!("resume:{}", paths.join(",")));
    }
}

struct Rig {
    fs: Arc<RecordingFs>,
    watcher: Arc<RecordingWatcher>,
    engine: BlockIdEngine,
    src: fixtures::TempSource,
    _data: tempfile::TempDir,
}

/// A real registry and cache over a temp source holding `files` (bytes,
/// as they are) and a copy of each named fixture, with permissions kept.
fn rig_with(fixtures_to_copy: &[&str], files: &[(&str, &[u8])], fail_rename: bool) -> Rig {
    let data = tempfile::tempdir().unwrap();
    let registry =
        Arc::new(SourceRegistry::new(Cache::open(data.path().join("cache.db")).unwrap()).unwrap());
    let src = fixtures::empty_source();
    for name in fixtures_to_copy {
        std::fs::copy(fixture_dir().join(name), src.root().join(name)).unwrap();
    }
    for (path, bytes) in files {
        src.write_bytes(path, bytes);
    }
    registry.register("notes", src.root()).unwrap();
    let fs = Arc::new(RecordingFs {
        fail_rename,
        ..Default::default()
    });
    let watcher = Arc::new(RecordingWatcher::default());
    let engine = BlockIdEngine::with_parts(registry, fs.clone(), watcher.clone())
        .with_settle(Duration::ZERO);
    Rig {
        fs,
        watcher,
        engine,
        src,
        _data: data,
    }
}

fn rig(fixtures_to_copy: &[&str]) -> Rig {
    rig_with(fixtures_to_copy, &[], false)
}

impl Rig {
    fn path(&self, rel: &str) -> PathBuf {
        self.src.root().join(rel)
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }
    fn snapshot(&self, rel: &str) -> (Vec<u8>, SystemTime, u32) {
        let meta = std::fs::metadata(self.path(rel)).unwrap();
        (
            std::fs::read(self.path(rel)).unwrap(),
            meta.modified().unwrap(),
            meta.permissions().mode(),
        )
    }
    fn by_symbol(&self, rel: &str, symbol: &str) -> Result<EnsureIdWire, BlockIdError> {
        self.engine.ensure_target(
            &format!("@notes:{rel}"),
            &BlockTarget::Symbol(symbol.into()),
        )
    }
    fn by_line(&self, rel: &str, line: usize) -> Result<EnsureIdWire, BlockIdError> {
        self.engine
            .ensure_target(&format!("@notes:{rel}"), &BlockTarget::Line(line as u64))
    }
    /// A refusal: the message, after checking nothing at all was
    /// written, opened for writing, paused for or left behind.
    fn refused(&self, rel: &str, result: Result<EnsureIdWire, BlockIdError>) -> String {
        let before = self.snapshot(rel);
        let calls_before = self.fs.calls();
        let watcher_before = self.watcher.log();
        let err = match result {
            Err(BlockIdError::Refused(m)) => m,
            other => panic!("{rel}: expected a refusal, got {other:?}"),
        };
        assert_eq!(
            before,
            self.snapshot(rel),
            "{rel}: bytes, mtime or mode changed"
        );
        assert_eq!(calls_before, self.fs.calls(), "{rel}: AtomicFs was used");
        assert_eq!(watcher_before, self.watcher.log(), "{rel}: watcher touched");
        assert!(self.temp_files().is_empty(), "{rel}: temp file left");
        err
    }
    fn temp_files(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![self.src.root().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.to_string_lossy().contains(".tephra-tmp-") {
                    found.push(path);
                }
            }
        }
        found
    }
}

fn six_char_id(id: &str) -> bool {
    id.len() == 6
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The 1-based line of `content` that starts with `needle`.
fn line_of(content: &str, needle: &str) -> usize {
    content
        .lines()
        .position(|l| l.starts_with(needle))
        .unwrap_or_else(|| panic!("no line starts with {needle:?}"))
        + 1
}

/// The lines of `content`, each with its own line ending (so a CRLF or
/// a missing final newline is part of the comparison).
fn lines_with_endings(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

/// The F4 property: `after` differs from `before` in exactly one line, and
/// that line's old text (without its line ending) is a prefix of the
/// new, whose remainder is returned.
fn one_line_appended<'a>(before: &str, after: &'a str) -> (usize, &'a str) {
    let old = lines_with_endings(before);
    let new = lines_with_endings(after);
    assert_eq!(old.len(), new.len(), "the line count changed");
    let changed: Vec<usize> = (0..old.len()).filter(|&i| old[i] != new[i]).collect();
    assert_eq!(changed.len(), 1, "more or fewer than one line changed");
    let i = changed[0];
    let ending_len = |s: &str| {
        if s.ends_with("\r\n") {
            2
        } else {
            usize::from(s.ends_with('\n'))
        }
    };
    let old_text = &old[i][..old[i].len() - ending_len(old[i])];
    let new_text = &new[i][..new[i].len() - ending_len(new[i])];
    assert_eq!(
        &old[i][old_text.len()..],
        &new[i][new_text.len()..],
        "the line ending changed"
    );
    assert!(
        new_text.starts_with(old_text),
        "old line {old_text:?} is not a prefix of {new_text:?}"
    );
    (i + 1, &new_text[old_text.len()..])
}

// ---------------------------------------------------------------
// GP6
// ---------------------------------------------------------------

#[test]
fn gp6_symbol_conflict_reason_turns_line_73_into_the_enum_with_a_trailing_id() {
    let rig = rig(&["registry.rs"]);
    let before = rig.read("registry.rs");
    assert!(before.lines().nth(71).unwrap().starts_with("#[derive("));
    assert!(before
        .lines()
        .nth(72)
        .unwrap()
        .starts_with("pub enum ConflictReason {"));

    let wire = rig.by_symbol("registry.rs", "ConflictReason").unwrap();
    let id = wire.id.clone().unwrap();
    assert!(six_char_id(&id), "{id}");
    assert!(wire.created);
    assert_eq!(wire.link, format!("[[@notes:registry.rs#^{id}]]"));

    let after = rig.read("registry.rs");
    let (line, appended) = one_line_appended(&before, &after);
    assert_eq!(line, 73);
    assert_eq!(appended, format!(" // ^{id}"));
    assert_eq!(
        after.lines().nth(72).unwrap(),
        format!("pub enum ConflictReason {{ // ^{id}")
    );

    // Again: the same link, the file untouched.
    let snapshot = rig.snapshot("registry.rs");
    let again = rig.by_symbol("registry.rs", "ConflictReason").unwrap();
    assert_eq!(again.link, wire.link);
    assert!(!again.created);
    assert_eq!(snapshot, rig.snapshot("registry.rs"));
}

#[test]
fn gp6_lines_72_and_73_give_the_same_result_as_the_symbol() {
    for line in [72, 73] {
        let rig = rig(&["registry.rs"]);
        let before = rig.read("registry.rs");
        let wire = rig.by_line("registry.rs", line).unwrap();
        let id = wire.id.unwrap();
        assert!(wire.created, "line {line}");
        assert_eq!(wire.link, format!("[[@notes:registry.rs#^{id}]]"));
        let after = rig.read("registry.rs");
        let (changed, appended) = one_line_appended(&before, &after);
        assert_eq!(changed, 73, "--line {line}: the ID goes on the enum line");
        assert_eq!(appended, format!(" // ^{id}"));

        // Either line, and the name, now find that one ID.
        for again in [
            rig.by_line("registry.rs", 72).unwrap(),
            rig.by_line("registry.rs", 73).unwrap(),
            rig.by_line("registry.rs", 75).unwrap(),
            rig.by_symbol("registry.rs", "ConflictReason").unwrap(),
        ] {
            assert_eq!(again.id.as_deref(), Some(id.as_str()));
            assert!(!again.created);
        }
        assert_eq!(after, rig.read("registry.rs"));
    }
}

// ---------------------------------------------------------------
// One fixture per bundled grammar
// ---------------------------------------------------------------

/// `(fixture, symbol, line-comment marker the ID uses)`: the symbol's
/// definition line ends in code in the fixture.
const PER_LANGUAGE: &[(&str, &str, &str)] = &[
    ("sample.py", "cached", "#"),
    ("sample.ts", "add", "//"),
    ("sample.tsx", "Button", "//"),
    ("sample.js", "greet", "//"),
    ("sample.jsx", "Card", "//"),
    ("sample.rs", "plain", "//"),
    ("sample.go", "Add", "//"),
    ("sample.c", "add", "//"),
    ("sample.cpp", "add", "//"),
    ("Sample.java", "add", "//"),
    ("sample.swift", "add", "//"),
    ("sample.lua", "add", "--"),
    ("sample.yaml", "plain", "#"),
    ("sample.toml", "client", "#"),
    ("script", "hello", "#"),
];

#[test]
fn every_grammar_gets_its_own_comment_marker_on_the_definition_line() {
    // One rig for every fixture: each file is written once, on its own.
    let files: Vec<&str> = PER_LANGUAGE.iter().map(|(f, _, _)| *f).collect();
    let rig = rig(&files);
    for (file, symbol, marker) in PER_LANGUAGE {
        let before = rig.read(file);
        let wire = rig
            .by_symbol(file, symbol)
            .unwrap_or_else(|e| panic!("{file} {symbol}: {e}"));
        let id = wire.id.clone().unwrap();
        assert!(wire.created, "{file}");
        assert!(six_char_id(&id), "{file}: {id}");
        assert_eq!(wire.link, format!("[[@notes:{file}#^{id}]]"), "{file}");

        let after = rig.read(file);
        let (_, appended) = one_line_appended(&before, &after);
        assert_eq!(appended, format!(" {marker} ^{id}"), "{file}");

        // The cache has it already: no waiting for a watcher, the same
        // ID back, nothing written.
        let snapshot = rig.snapshot(file);
        let again = rig.by_symbol(file, symbol).unwrap();
        assert_eq!(again.id.as_deref(), Some(id.as_str()), "{file}");
        assert!(!again.created, "{file}");
        assert_eq!(snapshot, rig.snapshot(file), "{file}");
        assert!(rig.temp_files().is_empty(), "{file}");
    }
}

#[test]
fn a_line_ending_with_a_line_comment_gets_the_id_inside_it() {
    for (file, symbol, comment) in [
        ("sample.py", "spaced", "# keep me"),
        ("sample.go", "Noted", "// TODO"),
        ("sample.rs", "noted", "// TODO"),
    ] {
        let rig = rig(&[file]);
        let before = rig.read(file);
        let id = rig.by_symbol(file, symbol).unwrap().id.unwrap();
        let after = rig.read(file);
        let (line, appended) = one_line_appended(&before, &after);
        assert_eq!(appended, format!(" ^{id}"), "{file}: no second marker");
        assert!(
            after
                .lines()
                .nth(line - 1)
                .unwrap()
                .ends_with(&format!("{comment} ^{id}")),
            "{file}"
        );
    }
}

#[test]
fn the_id_goes_on_the_definition_line_not_on_the_decorator_or_attribute() {
    // Python: `@decorator` above `def`.
    let rig_py = rig(&["sample.py"]);
    let before = rig_py.read("sample.py");
    let decorator = line_of(&before, "@functools");
    let def = line_of(&before, "def cached");
    assert_eq!(def, decorator + 1);
    rig_py.by_line("sample.py", decorator).unwrap();
    let (changed, _) = one_line_appended(&before, &rig_py.read("sample.py"));
    assert_eq!(changed, def);

    // Rust: a doc comment and `#[derive]` above `pub enum`.
    let rig_rs = rig(&["sample.rs"]);
    let before = rig_rs.read("sample.rs");
    let doc = line_of(&before, "/// A reason.");
    let enum_line = line_of(&before, "pub enum Reason");
    let wire = rig_rs.by_line("sample.rs", doc).unwrap();
    assert!(wire.created);
    let (changed, _) = one_line_appended(&before, &rig_rs.read("sample.rs"));
    assert_eq!(changed, enum_line);
    for line in [doc, doc + 1, enum_line, enum_line + 1, enum_line + 3] {
        let again = rig_rs.by_line("sample.rs", line).unwrap();
        assert_eq!(again.id, wire.id, "line {line}");
        assert!(!again.created);
    }
}

#[test]
fn a_line_outside_every_symbol_is_refused_with_no_symbol_on_line_n() {
    let rig = rig(&["sample.py", "sample.rs"]);
    let py = rig.read("sample.py");
    let blank = line_of(&py, "import functools") + 1;
    let msg = rig.refused("sample.py", rig.by_line("sample.py", blank));
    assert!(msg.contains(&format!("no symbol on line {blank}")), "{msg}");
    let msg = rig.refused("sample.py", rig.by_line("sample.py", 1));
    assert!(msg.contains("no symbol on line 1"), "{msg}");
    let msg = rig.refused("sample.py", rig.by_line("sample.py", 9999));
    assert!(msg.contains("out of range"), "{msg}");
}

// ---------------------------------------------------------------
// Symbol lookup by name
// ---------------------------------------------------------------

#[test]
fn a_shared_name_lists_each_symbol_and_the_qualified_name_settles_it() {
    let rig = rig(&["sample.py"]);
    let py = rig.read("sample.py");
    let msg = rig.refused("sample.py", rig.by_symbol("sample.py", "hello"));
    let first = line_of(&py, "    def hello");
    let second = py
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with("    def hello"))
        .nth(1)
        .unwrap()
        .0
        + 1;
    assert!(
        msg.contains(&format!("Greeter.hello (line {first}")),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("Other.hello (line {second}")),
        "{msg}"
    );

    let wire = rig.by_symbol("sample.py", "Other.hello").unwrap();
    assert!(wire.created);
    let (changed, _) = one_line_appended(&py, &rig.read("sample.py"));
    assert_eq!(changed, second);
}

#[test]
fn an_unknown_symbol_is_refused_with_its_name_and_the_unit() {
    let rig = rig(&["sample.py"]);
    let msg = rig.refused("sample.py", rig.by_symbol("sample.py", "nope"));
    assert!(
        msg.contains("no symbol named nope in @notes:sample.py"),
        "{msg}"
    );
}

#[test]
fn a_symbol_request_for_a_prose_file_is_refused() {
    let rig = rig_with(&[], &[("n.md", b"hello\n")], false);
    let msg = rig.refused("n.md", rig.by_symbol("n.md", "hello"));
    assert!(msg.contains("not a code file"), "{msg}");
}

// ---------------------------------------------------------------
// E5, E12 and the added refusals
// ---------------------------------------------------------------

#[test]
fn a_line_ending_inside_a_string_or_block_comment_is_refused_in_every_grammar() {
    const STRING: &str = "line ends inside a string";
    const BLOCK: &str = "inside block comment";
    const BACKSLASH: &str = "backslash";
    let cases = [
        ("sample.py", "documented", STRING),
        ("sample.py", "backslash", BACKSLASH),
        ("sample.rs", "RAW", STRING),
        ("sample.rs", "blk", BLOCK),
        ("sample.js", "tpl", STRING),
        ("sample.js", "blk", BLOCK),
        ("sample.jsx", "Tpl", STRING),
        ("sample.ts", "blk", BLOCK),
        ("sample.tsx", "Tpl", STRING),
        ("sample.go", "Raw", STRING),
        ("sample.c", "blk", BLOCK),
        ("sample.c", "cont", BACKSLASH),
        ("sample.cpp", "blk", BLOCK),
        ("Sample.java", "text", STRING),
        ("Sample.java", "blk", BLOCK),
        ("sample.swift", "multi", STRING),
        ("sample.swift", "blk", BLOCK),
        ("sample.lua", "blk", BLOCK),
        ("sample.lua", "raw", STRING),
        ("sample.yaml", "quoted", STRING),
        ("sample.toml", "text", STRING),
        ("script", "str", STRING),
        ("script", "cont", BACKSLASH),
    ];
    let mut files: Vec<&str> = cases.iter().map(|(f, _, _)| *f).collect();
    files.dedup();
    files.sort();
    files.dedup();
    let rig = rig(&files);
    for (file, symbol, reason) in cases {
        let msg = rig.refused(file, rig.by_symbol(file, symbol));
        assert!(msg.contains(reason), "{file} {symbol}: {msg}");
    }
}

#[test]
fn a_yaml_value_that_never_closes_is_a_parse_error_on_its_line() {
    let rig = rig(&["bad.yaml"]);
    let msg = rig.refused("bad.yaml", rig.by_line("bad.yaml", 1));
    assert!(
        msg.contains("cannot place ID: parse error on line 1")
            || msg.contains("no symbol on line 1"),
        "{msg}"
    );
}

#[test]
fn a_yaml_block_scalar_indicator_takes_the_comment_and_a_hash_inside_a_value_never_does() {
    let rig = rig_with(
        &[],
        &[
            ("a.yaml", b"key: | \n  text\nother: 1\n"),
            ("b.yaml", b"key: 'a#b\nother: 1\n"),
            ("c.yaml", b"key: 'a#b'\n"),
        ],
        false,
    );
    let id = rig.by_symbol("a.yaml", "key").unwrap().id.unwrap();
    // The block scalar indicator's own trailing space is kept.
    assert_eq!(
        rig.read("a.yaml"),
        format!("key: | # ^{id}\n  text\nother: 1\n")
    );

    let msg = rig.refused("b.yaml", rig.by_line("b.yaml", 1));
    assert_eq!(msg, "cannot place ID: parse error on line 1");

    // A closed quote with a `#` in it is code at the line's end.
    let id = rig.by_symbol("c.yaml", "key").unwrap().id.unwrap();
    assert_eq!(rig.read("c.yaml"), format!("key: 'a#b' # ^{id}\n"));
}

#[test]
fn a_parse_error_elsewhere_does_not_stop_an_id_on_a_clean_line() {
    let rig = rig_with(
        &[],
        &[(
            "e.rs",
            b"fn good() {\n    let x = ;\n}\n\nfn broken( {\n}\n",
        )],
        false,
    );
    let wire = rig.by_symbol("e.rs", "good");
    match wire {
        Ok(w) => {
            assert!(w.created);
            let after = rig.read("e.rs");
            assert!(after.starts_with("fn good() { // ^"), "{after}");
        }
        Err(e) => panic!("{e}"),
    }
    // The broken line itself is refused with the parse-error reason, and
    // `refused` checks the file is untouched.
    let msg = rig.refused("e.rs", rig.by_line("e.rs", 5));
    assert_eq!(msg, "cannot place ID: parse error on line 5");
}

#[test]
fn documents_text_other_and_binary_files_are_refused_and_untouched() {
    let docx = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tephra-parser/tests/fixtures/documents/headings.docx"),
    )
    .unwrap();
    let rig = rig_with(
        &[],
        &[
            ("doc.docx", &docx),
            ("doc.pdf", b"%PDF-1.4\n%%EOF\n"),
            ("data.json", b"{\"a\": 1}\n"),
            ("pic.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 0, 0, 0]),
        ],
        false,
    );
    for (file, word) in [
        ("doc.docx", "document"),
        ("doc.pdf", "document"),
        ("data.json", "text file of another type"),
        ("pic.png", "binary"),
    ] {
        let msg = rig.refused(file, rig.by_line(file, 1));
        assert!(msg.contains(word), "{file}: {msg}");
        let msg = rig.refused(file, rig.by_symbol(file, "x"));
        assert!(msg.contains(word), "{file}: {msg}");
    }
}

// ---------------------------------------------------------------
// An existing ID (G12, G13)
// ---------------------------------------------------------------

#[test]
fn an_id_already_on_the_symbol_is_returned_as_typed_and_nothing_is_written() {
    let rig = rig_with(
        &[],
        &[
            // At the end of the definition line, in the form Obsidian
            // accepts (upper case and hyphen), and a `// ^id` comment
            // line directly above a symbol.
            (
                "a.rs",
                b"fn one() {} // ^Summary-1\n\n// ^above\nfn two() {}\n",
            ),
            (
                "b.py",
                b"def f():  # ^f1\n    pass\n\n\n# ^above\ndef g():\n    pass\n",
            ),
        ],
        false,
    );
    let (a, b) = (rig.snapshot("a.rs"), rig.snapshot("b.py"));
    for (file, symbol, want) in [
        ("a.rs", "one", "Summary-1"),
        ("a.rs", "two", "above"),
        ("b.py", "f", "f1"),
        ("b.py", "g", "above"),
    ] {
        let wire = rig.by_symbol(file, symbol).unwrap();
        assert_eq!(wire.id.as_deref(), Some(want), "{file} {symbol}");
        assert!(!wire.created, "{file} {symbol}");
        assert_eq!(wire.link, format!("[[@notes:{file}#^{want}]]"));
    }
    assert_eq!(a, rig.snapshot("a.rs"));
    assert_eq!(b, rig.snapshot("b.py"));
    assert!(rig.fs.calls().is_empty());
}

#[test]
fn a_new_id_never_repeats_one_the_file_already_uses_for_another_symbol() {
    let rig = rig_with(
        &[],
        &[(
            "a.rs",
            b"fn one() {} // ^aaaaaa\nfn two() {}\nfn three() {}\n",
        )],
        false,
    );
    let mut ids = HashSet::from(["aaaaaa".to_string()]);
    for name in ["two", "three"] {
        let id = rig.by_symbol("a.rs", name).unwrap().id.unwrap();
        assert!(six_char_id(&id));
        assert!(ids.insert(id), "an ID was reused");
    }
}

// ---------------------------------------------------------------
// Layout: line endings, final newline, whitespace, Unicode, mode
// ---------------------------------------------------------------

#[test]
fn crlf_stays_crlf_and_a_missing_final_newline_stays_missing() {
    let rig = rig(&["crlf.py", "no_eol.py"]);
    let before = rig.read("crlf.py");
    assert!(before.contains("\r\n") && !before.replace("\r\n", "").contains('\n'));
    let id = rig.by_symbol("crlf.py", "second").unwrap().id.unwrap();
    let after = rig.read("crlf.py");
    assert_eq!(
        after,
        before.replacen("def second():", &format!("def second(): # ^{id}"), 1)
    );
    assert!(
        !after.replace("\r\n", "").contains('\n'),
        "every ending is still CRLF"
    );

    // The last line of a file with no final newline.
    let before = rig.read("no_eol.py");
    assert!(!before.ends_with('\n'));
    let id = rig.by_symbol("no_eol.py", "tail").unwrap().id.unwrap();
    let after = rig.read("no_eol.py");
    assert_eq!(
        after,
        before.replacen("def tail():", &format!("def tail(): # ^{id}"), 1)
    );
    // A symbol whose own line is the last line, with no newline at all.
    let rig = rig_with(&[], &[("last.rs", b"fn last() {}")], false);
    let id = rig.by_symbol("last.rs", "last").unwrap().id.unwrap();
    assert_eq!(rig.read("last.rs"), format!("fn last() {{}} // ^{id}"));
}

#[test]
fn trailing_whitespace_tabs_and_unicode_are_kept_byte_for_byte() {
    let rig = rig_with(
        &[],
        &[
            (
                "w.rs",
                "fn tabbed() {  \t\n\tlet x = 1;\n}\n\nfn café_日本() {}\n".as_bytes(),
            ),
            (
                "w.py",
                "class Café:   \n\tdef méthode(self):\n\t\tpass\n".as_bytes(),
            ),
        ],
        false,
    );
    let before = rig.read("w.rs");
    let id = rig.by_symbol("w.rs", "tabbed").unwrap().id.unwrap();
    let after = rig.read("w.rs");
    // The trailing spaces and tab stay, and the comment follows them
    // without another space.
    assert_eq!(
        after,
        before.replacen(
            "fn tabbed() {  \t",
            &format!("fn tabbed() {{  \t// ^{id}"),
            1
        )
    );

    let before = rig.read("w.rs");
    let id = rig.by_symbol("w.rs", "café_日本").unwrap().id.unwrap();
    assert_eq!(
        rig.read("w.rs"),
        before.replacen(
            "fn café_日本() {}",
            &format!("fn café_日本() {{}} // ^{id}"),
            1
        )
    );

    let before = rig.read("w.py");
    let id = rig.by_symbol("w.py", "Café.méthode").unwrap().id.unwrap();
    assert_eq!(
        rig.read("w.py"),
        before.replacen(
            "\tdef méthode(self):",
            &format!("\tdef méthode(self): # ^{id}"),
            1
        )
    );
}

#[test]
fn permissions_including_the_executable_bit_survive_the_write() {
    let rig = rig(&["script", "sample.py"]);
    assert_eq!(rig.snapshot("script").2 & 0o777, 0o755);
    let before_mode = rig.snapshot("script").2;
    let wire = rig.by_symbol("script", "hello").unwrap();
    assert!(wire.created);
    assert_eq!(rig.snapshot("script").2, before_mode);
    assert_eq!(rig.snapshot("script").2 & 0o111, 0o111);

    // A file with a shebang and no extension is a code file; its first
    // line names the language, so `#` is its marker.
    assert!(rig.read("script").contains("hello() { # ^"));

    // And a plain 0644 stays 0644.
    let mode = rig.snapshot("sample.py").2;
    rig.by_symbol("sample.py", "cached").unwrap();
    assert_eq!(rig.snapshot("sample.py").2, mode);
}

// ---------------------------------------------------------------
// The write path, the watcher and E6
// ---------------------------------------------------------------

#[test]
fn a_write_goes_temp_then_fsync_then_rename_and_the_watcher_forgets_that_path() {
    let rig = rig(&["sample.go"]);
    let abs = std::fs::canonicalize(rig.path("sample.go")).unwrap();
    assert!(rig.by_symbol("sample.go", "Add").unwrap().created);

    let calls = rig.fs.calls();
    let order: Vec<&str> = calls.iter().map(|c| c.split(' ').next().unwrap()).collect();
    assert_eq!(
        order,
        ["write_new", "set_permissions", "sync", "rename"],
        "{calls:?}"
    );
    assert!(calls[0].contains(".tephra-tmp-"), "{calls:?}");
    assert_eq!(calls[3], format!("rename {}", abs.display()));
    assert_eq!(
        rig.watcher.log(),
        vec!["pause".to_string(), format!("resume:{}", abs.display())]
    );
    assert!(rig.temp_files().is_empty());
}

#[test]
fn a_failed_write_reports_it_and_leaves_the_file_and_no_temp_file() {
    let rig = rig_with(&["sample.go"], &[], true);
    let before = rig.snapshot("sample.go");
    let err = rig.by_symbol("sample.go", "Add").unwrap_err();
    assert!(
        matches!(&err, BlockIdError::Refused(m) if m.contains("could not write")),
        "{err:?}"
    );
    assert_eq!(before, rig.snapshot("sample.go"));
    assert!(rig.temp_files().is_empty(), "{:?}", rig.temp_files());
    // The temp file was written and the rename tried: the write path was
    // really exercised.
    assert!(rig.fs.calls().iter().any(|c| c.starts_with("rename")));
}

#[test]
fn a_refused_request_never_touches_the_filesystem_or_the_watcher() {
    let rig = rig(&["sample.py", "sample.c", "script", "sample.yaml"]);
    let before: Vec<_> = ["sample.py", "sample.c", "script", "sample.yaml"]
        .iter()
        .map(|f| rig.snapshot(f))
        .collect();
    let attempts: Vec<(&str, Result<EnsureIdWire, BlockIdError>)> = vec![
        ("sample.py", rig.by_symbol("sample.py", "documented")),
        ("sample.py", rig.by_symbol("sample.py", "nope")),
        ("sample.py", rig.by_symbol("sample.py", "hello")),
        ("sample.py", rig.by_line("sample.py", 1)),
        ("sample.c", rig.by_symbol("sample.c", "blk")),
        ("sample.c", rig.by_symbol("sample.c", "cont")),
        ("script", rig.by_symbol("script", "str")),
        ("sample.yaml", rig.by_symbol("sample.yaml", "quoted")),
    ];
    for (file, result) in attempts {
        assert!(matches!(result, Err(BlockIdError::Refused(_))), "{file}");
    }
    assert!(rig.fs.calls().is_empty(), "AtomicFs was used");
    assert!(rig.watcher.log().is_empty(), "watcher touched");
    let after: Vec<_> = ["sample.py", "sample.c", "script", "sample.yaml"]
        .iter()
        .map(|f| rig.snapshot(f))
        .collect();
    assert_eq!(before, after);
    assert!(rig.temp_files().is_empty());
}

#[test]
fn a_file_changed_since_the_cache_saw_it_is_refused_with_try_again() {
    let rig = rig(&["sample.go"]);
    // Another writer: same size, a later mtime.
    let path = rig.path("sample.go");
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    drop(file);
    let msg = rig.refused("sample.go", rig.by_symbol("sample.go", "Add"));
    assert!(msg.contains("file changed, try again"), "{msg}");

    // And a changed size.
    let rig = rig_with(&["sample.go"], &[], false);
    let mut content = rig.read("sample.go");
    content.push_str("\nfunc Extra() {}\n");
    std::fs::write(rig.path("sample.go"), content).unwrap();
    let msg = rig.refused("sample.go", rig.by_symbol("sample.go", "Add"));
    assert!(msg.contains("file changed, try again"), "{msg}");
}

// ---------------------------------------------------------------
// Fence: F3 and F4
// ---------------------------------------------------------------

#[test]
fn f3_the_link_always_uses_the_hash_caret_form_and_never_a_symbol_anchor() {
    let rig = rig(&["registry.rs", "sample.py"]);
    for (file, symbol) in [("registry.rs", "ConflictReason"), ("sample.py", "cached")] {
        let wire = rig.by_symbol(file, symbol).unwrap();
        let id = wire.id.unwrap();
        assert_eq!(wire.link, format!("[[@notes:{file}#^{id}]]"));
        assert!(!wire.link.contains(&format!("#{symbol}")));
        assert!(!wire.link.contains(&format!("{file}^")));
    }
}

#[test]
fn f4_every_symbol_of_every_fixture_gets_either_a_one_line_prefix_edit_or_a_refusal() {
    let mut written = 0;
    let mut refused = 0;
    let mut names: Vec<String> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for file in &names {
        let content = std::fs::read_to_string(fixture_dir().join(file)).unwrap();
        let lang = tephra_parser::Language::from_path(Path::new(file), content.lines().next())
            .unwrap_or_else(|| panic!("{file} has no grammar"));
        let parsed = tephra_parser::parse_code_unit(Path::new(file), lang, &content);
        assert!(!parsed.symbols.is_empty(), "{file}: no symbols");

        // One rig per file: each symbol's write is compared with the file
        // as the one before it left it.
        let rig = rig(&[file.as_str()]);
        for symbol in &parsed.symbols {
            let before = rig.snapshot(file);
            let before_text = rig.read(file);
            match rig.by_line(file, symbol.line) {
                Ok(wire) if wire.created => {
                    written += 1;
                    let after = rig.read(file);
                    let (line, appended) = one_line_appended(&before_text, &after);
                    assert_eq!(line, symbol.line, "{file} {}", symbol.name);
                    let id = wire.id.unwrap();
                    assert!(
                        appended.ends_with(&format!("^{id}"))
                            && appended.starts_with(char::is_whitespace),
                        "{file} {}: {appended:?}",
                        symbol.name
                    );
                    assert_eq!(after.matches(&format!("^{id}")).count(), 1, "{file}");
                    assert_eq!(before.2, rig.snapshot(file).2, "{file}: mode changed");
                }
                Ok(_) => {
                    assert_eq!(
                        before,
                        rig.snapshot(file),
                        "{file}: existing ID, but written"
                    );
                }
                Err(_) => {
                    refused += 1;
                    assert_eq!(before, rig.snapshot(file), "{file} {}", symbol.name);
                }
            }
            assert!(rig.temp_files().is_empty(), "{file}");
        }
    }
    assert!(written > 40, "only {written} symbols were written");
    assert!(refused > 10, "only {refused} symbols were refused");
}
