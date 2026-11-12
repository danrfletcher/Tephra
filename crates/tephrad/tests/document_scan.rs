//! G5 / E4 / F4 / F6 / F7 through the real path: real documents (the
//! committed fixtures of `tephra-parser`) in a real temp source, the
//! real scan, the real watcher, the real `SourceRegistry`, a real
//! `tephra_graph::UnitGraph` built from the cache, and the real
//! `RefactorEngine`. No mocks.
//!
//! Backlinks are read from a `UnitGraph` rebuilt from the cache file,
//! which is what `graph.get_backlinks` answers from (the RPC wiring
//! itself is covered by `rpc_live_graph.rs`; this PR adds no method).

mod fixtures;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tephra_cache::{Cache, SourceStatus};
use tephra_graph::UnitGraph;
use tephrad::file_kind::MAX_INDEXED_BYTES;
use tephrad::ignore::IgnoreRules;
use tephrad::refactor::{ApplyOutcome, RefactorEngine, WatcherPauseControl, DOCUMENT_READ_ONLY};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{GraphBackend, SharedGraph, SourceBackend};
use tephrad::scan;

// ------------------------------------------------------------ helpers

fn doc_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tephra-parser/tests/fixtures/documents")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One process-wide log sink. A per-test `with_default` subscriber is
/// not reliable here: `tracing` caches whether a callsite is enabled,
/// and a test running in parallel with no subscriber can switch the
/// callsite off for everyone. So the subscriber is installed once for
/// the whole binary and each test reads only the lines that name its
/// own temp directory.
#[derive(Clone, Default)]
struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

fn global_log() -> &'static LogBuf {
    static LOG: std::sync::OnceLock<LogBuf> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        buf
    })
}

/// The log lines that mention `root` (one test's own source).
fn log_lines_for(root: &Path) -> Vec<String> {
    let needle = root.display().to_string();
    String::from_utf8(global_log().0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter(|l| l.contains(&needle))
        .map(str::to_string)
        .collect()
}

struct Env {
    _cache_dir: tempfile::TempDir,
    cache: Cache,
    cache_path: PathBuf,
    source_id: i64,
    root: PathBuf,
}

fn env_for(fixture: &fixtures::TempSource) -> Env {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");
    // Same canonicalisation `register` does, so paths line up on macOS.
    let root = fixture.root().canonicalize().expect("canonicalize");
    let source_id = cache
        .register_source("s", &root.to_string_lossy(), SourceStatus::Active)
        .expect("register_source");
    Env {
        _cache_dir: cache_dir,
        cache,
        cache_path,
        source_id,
        root,
    }
}

fn unit(env: &Env, rel: &str) -> tephra_cache::UnitRow {
    env.cache
        .get_unit_by_rel_path(env.source_id, rel)
        .expect("get_unit_by_rel_path")
        .unwrap_or_else(|| panic!("{rel} is a unit"))
}

// ------------------------------------------------- good and broken docs

/// A source holding two good documents, one of each broken kind, and a
/// note after them.
fn mixed_source() -> fixtures::TempSource {
    let src = fixtures::empty_source();
    src.write("Target.md", "# Target\n");
    src.write_bytes("docs/report.docx", &doc_fixture("headings.docx"));
    src.write_bytes("docs/deck.pptx", &doc_fixture("deck.pptx"));
    src.write_bytes("docs/text.pdf", &doc_fixture("text.pdf"));
    // Broken, each differently:
    src.write_bytes("broken/scanned.pdf", &doc_fixture("no-text.pdf"));
    src.write_bytes("broken/locked.pdf", &doc_fixture("encrypted.pdf"));
    src.write_bytes("broken/truncated.docx", &doc_fixture("truncated.docx"));
    src.write_bytes("broken/garbage.pdf", &doc_fixture("garbage.pdf"));
    src.write_bytes("broken/empty.pptx", b"");
    // Sorts after every document: the scan must carry on to it.
    src.write("zzz-after.md", "# After\n\n[[Target]]\n");
    src
}

const BROKEN: [&str; 5] = [
    "broken/scanned.pdf",
    "broken/locked.pdf",
    "broken/truncated.docx",
    "broken/garbage.pdf",
    "broken/empty.pptx",
];

#[test]
fn a_scan_of_good_and_broken_documents_succeeds_caches_the_good_and_warns_once_per_bad() {
    let fixture = mixed_source();
    let env = env_for(&fixture);

    global_log();
    let stats = scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .expect("a bad document never fails the scan");

    // ScanStats counts every file: 1 + 3 good + 5 broken + 1.
    assert_eq!(stats.units_written, 10, "{stats:?}");

    // The note after the documents was indexed.
    assert_eq!(unit(&env, "zzz-after.md").kind, "prose");

    // Good documents: kind `document`, links and headings in the cache.
    let report = fixtures::unit_rows(&env.cache_path, "s", "docs/report.docx").unwrap();
    assert_eq!(report.kind, "document");
    assert_eq!(
        report.links,
        vec![
            ("wikilink".to_string(), "Target".to_string()),
            (
                "wikilink".to_string(),
                "@notes:café notes|alias".to_string()
            ),
            ("markdown".to_string(), "Target.md".to_string()),
        ]
    );
    assert_eq!(
        report.headings,
        vec![
            (1, "Quarterly report".to_string(), 1),
            (1, "Overview".to_string(), 2),
            (2, "Details".to_string(), 4),
            (3, "A style known only by name".to_string(), 6),
        ]
    );
    // Paragraphs are `blocks` rows with no block id: (None, number, index).
    assert!(report.blocks.iter().all(|(id, _, _)| id.is_none()));
    assert_eq!(report.blocks.len(), 8);
    assert_eq!(report.tags, Vec::<String>::new());

    let deck = fixtures::unit_rows(&env.cache_path, "s", "docs/deck.pptx").unwrap();
    assert_eq!(deck.kind, "document");
    assert_eq!(
        deck.headings,
        vec![
            (1, "Deck title".to_string(), 1),
            (1, "Agenda".to_string(), 2),
            (1, "Closing".to_string(), 3),
        ]
    );
    assert_eq!(deck.links.len(), 3, "{:?}", deck.links);

    let pdf = fixtures::unit_rows(&env.cache_path, "s", "docs/text.pdf").unwrap();
    assert_eq!(pdf.kind, "document");
    assert_eq!(
        pdf.headings,
        vec![
            (1, "Chapter one".to_string(), 1),
            (2, "Section 1.1".to_string(), 1),
            (1, "Chapter two".to_string(), 2),
        ]
    );
    assert_eq!(pdf.links.len(), 5, "{:?}", pdf.links);

    // Broken ones: still a `document` unit, by name only.
    for rel in BROKEN {
        let rows = fixtures::unit_rows(&env.cache_path, "s", rel)
            .unwrap_or_else(|| panic!("{rel} is a unit"));
        assert_eq!(rows.kind, "document", "{rel}");
        assert!(
            rows.links.is_empty()
                && rows.headings.is_empty()
                && rows.blocks.is_empty()
                && rows.tags.is_empty(),
            "{rel} is name-only: {rows:?}"
        );
        let u = unit(&env, rel);
        assert!(!u.too_large, "{rel}");
        assert!(u.size.is_some(), "{rel} keeps its size");
    }

    // One warning per broken document, naming its path and the reason;
    // none for a good one.
    let lines = log_lines_for(&env.root);
    let warns: Vec<&String> = lines.iter().filter(|l| l.contains("WARN")).collect();
    for (rel, reason) in [
        ("broken/scanned.pdf", "no text layer"),
        ("broken/locked.pdf", "encrypted"),
        ("broken/truncated.docx", "not a readable zip"),
        ("broken/garbage.pdf", "cannot parse the PDF"),
        ("broken/empty.pptx", "not a readable zip"),
    ] {
        let mine: Vec<&&String> = warns.iter().filter(|l| l.contains(rel)).collect();
        assert_eq!(mine.len(), 1, "{rel}: {lines:?}");
        assert!(
            mine[0].contains(reason),
            "{rel} should say {reason:?}: {}",
            mine[0]
        );
    }
    for rel in ["docs/report.docx", "docs/deck.pptx", "docs/text.pdf"] {
        assert!(
            !lines.iter().any(|l| l.contains(rel)),
            "{rel} is good and logs nothing: {lines:?}"
        );
    }
    assert_eq!(warns.len(), BROKEN.len(), "{lines:?}");
}

#[test]
fn a_rescan_and_a_change_batch_treat_documents_the_same_as_a_scan() {
    let fixture = mixed_source();
    let env = env_for(&fixture);
    scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .unwrap();
    let before = fixtures::unit_rows(&env.cache_path, "s", "docs/report.docx").unwrap();

    let stats = scan::rescan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .expect("rescan succeeds");
    assert_eq!(stats.units_written, 10);
    assert_eq!(
        fixtures::unit_rows(&env.cache_path, "s", "docs/report.docx").unwrap(),
        before
    );

    // A batch that names a document re-reads it: swap the file's bytes
    // for another docx and the old rows are replaced, not added to.
    fixture.write_bytes("docs/report.docx", &doc_fixture("split-runs.docx"));
    let changed: HashSet<PathBuf> = [env.root.join("docs/report.docx")].into_iter().collect();
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &changed,
        &IgnoreRules::default(),
    )
    .expect("batch applies");
    let after = fixtures::unit_rows(&env.cache_path, "s", "docs/report.docx").unwrap();
    assert_eq!(
        after.links,
        vec![("wikilink".to_string(), "@notes:Tephra test".to_string())]
    );
    assert!(after.headings.is_empty());

    // ... and a document turning corrupt keeps its unit but loses its rows.
    fixture.write_bytes("docs/report.docx", &doc_fixture("truncated.docx"));
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &changed,
        &IgnoreRules::default(),
    )
    .unwrap();
    let broken = fixtures::unit_rows(&env.cache_path, "s", "docs/report.docx").unwrap();
    assert_eq!(broken.kind, "document");
    assert!(broken.links.is_empty() && broken.headings.is_empty() && broken.blocks.is_empty());
}

#[test]
fn a_document_over_the_cap_never_reaches_the_extractor() {
    // E3/G7: a valid docx whose file is padded past the 20 MiB cap
    // (sparse, so cheap). It keeps kind `document`, is flagged
    // `too_large`, has no rows and is not parsed (so no warning about
    // it being unreadable either).
    let fixture = fixtures::empty_source();
    let mut bytes = doc_fixture("headings.docx");
    bytes.resize(bytes.len() + 16, 0);
    fixture.write_bytes("big.docx", &bytes);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(fixture.root().join("big.docx"))
        .unwrap();
    f.set_len(MAX_INDEXED_BYTES + 1).unwrap();
    drop(f);
    fixture.write_bytes("small.docx", &doc_fixture("headings.docx"));
    let env = env_for(&fixture);

    global_log();
    let stats = scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .unwrap();
    assert_eq!(stats.units_written, 2);

    let big = unit(&env, "big.docx");
    assert_eq!((big.kind.as_str(), big.too_large), ("document", true));
    let rows = fixtures::unit_rows(&env.cache_path, "s", "big.docx").unwrap();
    assert!(rows.links.is_empty() && rows.headings.is_empty() && rows.blocks.is_empty());
    assert!(!fixtures::unit_rows(&env.cache_path, "s", "small.docx")
        .unwrap()
        .links
        .is_empty());

    let lines = log_lines_for(&env.root);
    let about_big: Vec<&String> = lines.iter().filter(|l| l.contains("big.docx")).collect();
    assert_eq!(about_big.len(), 1, "{lines:?}");
    assert!(about_big[0].contains("INFO") && about_big[0].contains("size limit"));
    assert!(!lines.iter().any(|l| l.contains("WARN")), "{lines:?}");
}

#[test]
fn a_symlinked_document_is_never_read() {
    // The scan never follows a symlink out of the source, documents
    // included.
    let outside = fixtures::empty_source();
    outside.write_bytes("secret.docx", &doc_fixture("headings.docx"));
    let fixture = fixtures::empty_source();
    fixture.write("a.md", "# a\n");
    std::os::unix::fs::symlink(
        outside.root().join("secret.docx"),
        fixture.root().join("linked.docx"),
    )
    .unwrap();
    let env = env_for(&fixture);
    scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .unwrap();
    assert!(env
        .cache
        .get_unit_by_rel_path(env.source_id, "linked.docx")
        .unwrap()
        .is_none());
}

// ------------------------------------------------ the graph and the watcher

struct World {
    notes: fixtures::TempSource,
    docs: fixtures::TempSource,
    cache_path: PathBuf,
    registry: Arc<SourceRegistry>,
    _cache_dir: tempfile::TempDir,
}

/// Source `notes`: `Target.md` and `café notes.md`, with the three good
/// documents (`report.docx`, `deck.pptx`, `text.pdf`) and the scanned
/// PDF alongside them. Source `docs`: copies of the good documents, to
/// show an unprefixed link resolves only in its own source (F7).
fn world() -> World {
    let notes = fixtures::empty_source();
    notes.write("Target.md", "# Target\n\n## Intro\n");
    notes.write("café notes.md", "# café notes\n");
    notes.write("Tephra test.md", "# Tephra test\n\n## Intro\n");
    notes.write_bytes("report.docx", &doc_fixture("headings.docx"));
    notes.write_bytes("deck.pptx", &doc_fixture("deck.pptx"));
    notes.write_bytes("text.pdf", &doc_fixture("text.pdf"));
    notes.write_bytes("scanned.pdf", &doc_fixture("no-text.pdf"));

    let docs = fixtures::empty_source();
    docs.write_bytes("report.docx", &doc_fixture("headings.docx"));
    docs.write_bytes("deck.pptx", &doc_fixture("deck.pptx"));

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry = Arc::new(
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
    );
    registry.register("notes", notes.root()).expect("notes");
    registry.register("docs", docs.root()).expect("docs");
    World {
        notes,
        docs,
        cache_path,
        registry,
        _cache_dir: cache_dir,
    }
}

impl World {
    fn graph(&self) -> UnitGraph {
        let cache = Cache::open(&self.cache_path).expect("open cache for graph");
        UnitGraph::build_from_cache(&cache).expect("build graph")
    }

    /// Every referring unit of `id`, one entry per link row, sorted.
    fn backlink_sources(&self, id: &str) -> Vec<String> {
        let mut from: Vec<String> = self
            .graph()
            .get_backlinks_all(id)
            .into_iter()
            .map(|e| e.from)
            .collect();
        from.sort();
        from.dedup();
        from
    }

    fn finish(self) {
        self.registry.unregister("notes").unwrap();
        self.registry.unregister("docs").unwrap();
    }
}

#[test]
fn backlinks_of_a_note_list_the_docx_pptx_and_pdf_that_link_to_it_and_not_the_scanned_pdf() {
    let w = world();
    assert_eq!(
        w.backlink_sources("@notes:Target.md"),
        vec![
            "@notes:deck.pptx".to_string(),
            "@notes:report.docx".to_string(),
            "@notes:text.pdf".to_string(),
        ]
    );
    // The scanned PDF is a unit (by name) with no links at all.
    assert!(fixtures::unit_rows(&w.cache_path, "notes", "scanned.pdf")
        .unwrap()
        .links
        .is_empty());
    w.finish();
}

#[test]
fn f7_an_unprefixed_link_in_a_document_resolves_only_in_its_own_source_and_a_federated_one_across()
{
    let w = world();
    // `docs` has no `Target.md`, so the unprefixed `[[Target]]` in
    // docs' documents resolves nowhere: they are not backlinks of
    // `@notes:Target.md` (only the `notes` copies are).
    let from_target = w.backlink_sources("@notes:Target.md");
    assert!(
        !from_target.iter().any(|f| f.starts_with("@docs:")),
        "{from_target:?}"
    );
    // The federated, non-ASCII `[[@notes:café notes|alias]]` in both
    // `report.docx` copies reaches `notes`.
    assert_eq!(
        w.backlink_sources("@notes:café notes.md"),
        vec![
            "@docs:report.docx".to_string(),
            "@notes:report.docx".to_string()
        ]
    );
    // ... as does the pptx/pdf federated `[[@notes:Tephra test#Intro|shown]]`.
    assert!(w
        .backlink_sources("@notes:Tephra test.md")
        .contains(&"@notes:text.pdf".to_string()));
    w.finish();
}

#[test]
fn a_watcher_modify_replaces_a_documents_links_and_headings_and_a_delete_removes_them() {
    let w = world();
    let target = "@notes:Target.md";
    assert!(w
        .backlink_sources(target)
        .contains(&"@notes:report.docx".to_string()));

    // Modify: report.docx becomes a different document that links
    // elsewhere. The old link and headings are replaced.
    w.notes
        .write_bytes("report.docx", &doc_fixture("split-runs.docx"));
    assert!(
        wait_until(Duration::from_secs(20), || {
            !w.backlink_sources(target)
                .contains(&"@notes:report.docx".to_string())
                && w.backlink_sources("@notes:Tephra test.md")
                    .contains(&"@notes:report.docx".to_string())
        }),
        "the changed document's links never replaced the old ones: {:?} / {:?}",
        w.backlink_sources(target),
        w.backlink_sources("@notes:Tephra test.md")
    );
    let rows = fixtures::unit_rows(&w.cache_path, "notes", "report.docx").unwrap();
    assert_eq!(rows.kind, "document");
    assert!(rows.headings.is_empty(), "old headings are gone: {rows:?}");
    assert_eq!(rows.links.len(), 1);

    // Delete: the unit, its links and its headings go.
    w.notes.remove("deck.pptx");
    assert!(
        wait_until(Duration::from_secs(20), || {
            !w.backlink_sources(target)
                .contains(&"@notes:deck.pptx".to_string())
        }),
        "a deleted document is still a backlink: {:?}",
        w.backlink_sources(target)
    );
    assert!(fixtures::unit_rows(&w.cache_path, "notes", "deck.pptx").is_none());

    // A document that turns corrupt on disk keeps its unit by name
    // only and loses every old row.
    w.notes.write_bytes("text.pdf", &doc_fixture("garbage.pdf"));
    assert!(
        wait_until(Duration::from_secs(20), || {
            fixtures::unit_rows(&w.cache_path, "notes", "text.pdf")
                .is_some_and(|r| r.links.is_empty() && r.headings.is_empty())
        }),
        "a document turned corrupt kept its old rows"
    );
    assert_eq!(
        fixtures::unit_rows(&w.cache_path, "notes", "text.pdf")
            .unwrap()
            .kind,
        "document"
    );
    let _ = &w.docs;
    w.finish();
}

// ------------------------------------------------------ F4: read-only

struct Stamp {
    bytes: Vec<u8>,
    mtime: SystemTime,
}

fn stamp(path: &Path) -> Stamp {
    Stamp {
        bytes: std::fs::read(path).unwrap(),
        mtime: std::fs::metadata(path).unwrap().modified().unwrap(),
    }
}

#[test]
fn f4_documents_keep_their_bytes_and_mtime_through_scan_watcher_churn_and_a_refactor() {
    let w = world();
    let docs = ["report.docx", "deck.pptx", "text.pdf", "scanned.pdf"];
    let before: Vec<Stamp> = docs
        .iter()
        .map(|d| stamp(&w.notes.root().join(d)))
        .collect();

    // Watcher churn: unrelated files change around the documents.
    w.notes.write("churn.md", "# one\n");
    w.notes.write("churn.md", "# two [[Target]]\n");
    w.notes.write("Target.md", "# Target\n\n## Intro\n\nmore\n");
    assert!(
        wait_until(Duration::from_secs(20), || {
            w.backlink_sources("@notes:Target.md")
                .contains(&"@notes:churn.md".to_string())
        }),
        "the watcher never caught up"
    );

    // A refactor that renames the note every document links to.
    let shared = Arc::new(SharedGraph::new(w.graph()));
    let engine = RefactorEngine::new(
        shared as Arc<dyn GraphBackend>,
        w.registry.clone() as Arc<dyn SourceBackend>,
        w.registry.clone() as Arc<dyn WatcherPauseControl>,
    );
    let preview = engine
        .preview("notes", "Target.md", "Renamed.md")
        .expect("preview");

    // The prose that links to it is rewritten; each document that links
    // to it is skipped, read-only, and named.
    let changed_files: HashSet<&str> = preview.changes.iter().map(|c| c.file.as_str()).collect();
    assert!(
        changed_files.contains("@notes:churn.md"),
        "{changed_files:?}"
    );
    assert!(
        changed_files
            .iter()
            .all(|f| !f.ends_with(".docx") && !f.ends_with(".pptx") && !f.ends_with(".pdf")),
        "a document is never in the changes: {changed_files:?}"
    );
    let mut skipped: Vec<&str> = preview.skipped.iter().map(|s| s.file.as_str()).collect();
    skipped.sort();
    assert_eq!(
        skipped,
        vec!["@notes:deck.pptx", "@notes:report.docx", "@notes:text.pdf"]
    );
    assert!(preview
        .skipped
        .iter()
        .all(|s| s.error == DOCUMENT_READ_ONLY));

    match engine
        .apply("notes", "Target.md", "Renamed.md")
        .expect("apply")
    {
        ApplyOutcome::Applied { files_changed } => assert!(files_changed >= 1),
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(std::fs::read_to_string(w.notes.root().join("churn.md"))
        .unwrap()
        .contains("Renamed"));

    for (d, b) in docs.iter().zip(&before) {
        let now = stamp(&w.notes.root().join(d));
        assert!(now.bytes == b.bytes, "{d} bytes changed");
        assert_eq!(now.mtime, b.mtime, "{d} mtime changed");
    }
    w.finish();
}
