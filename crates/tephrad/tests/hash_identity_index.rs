//! Content hash and file identity (`units.hash`, `units.inode`,
//! `units.device`) filled by the real scan and refreshed by the real
//! change-batch path the watcher calls, over a tempdir. Hard-link,
//! symlink, empty-file, over-cap and unreadable fixtures are created
//! here. No mocks: real files, real SQLite, real `tephra-parser`.
//!
//! The expected hashes below are SHA-256 digests worked out with
//! Python's `hashlib`, not with the code under test.

// Unix only: inodes, hard links, symlinks and mode bits. The code under
// test is gated so a non-Unix build compiles and stores null identity.
#![cfg(unix)]

mod fixtures;

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tephra_cache::{Cache, SourceStatus, UnitRow};
use tephrad::file_kind::MAX_INDEXED_BYTES;
use tephrad::ignore::IgnoreRules;
use tephrad::registry::SourceRegistry;
use tephrad::scan;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// SHA-256 of `hello\n`.
const HELLO_SHA256: &str = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";
/// SHA-256 of `b"caf\xe9 \xff\xfe not utf8 \xc3\x28\n"` (not valid UTF-8).
const NOT_UTF8_SHA256: &str = "a131fcc2db92cecc24df160d5df699d65b27222aa90a80f7632e36477f6388ed";
/// SHA-256 of `[0x89, 0x50, 0x4e, 0x47, 0, 0xff, 0xfe, 0xc3, 0x28, 0x80]`.
const BLOB_SHA256: &str = "0e42c37c1c8da592d9c9b7413cb6353ec35f6c24dfe6225155ad62c9184eddba";
/// SHA-256 of `MAX_INDEXED_BYTES` NUL bytes.
const AT_CAP_NULS_SHA256: &str = "cd52d81e25f372e6fa4db2c0dfceb59862c1969cab17096da352b34950c973cc";

const NOT_UTF8: &[u8] = b"caf\xe9 \xff\xfe not utf8 \xc3\x28\n";
const BLOB: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0, 0xff, 0xfe, 0xc3, 0x28, 0x80];

struct Env {
    _cache_dir: tempfile::TempDir,
    cache: Cache,
    source_id: i64,
    root: PathBuf,
}

fn env_for(fixture: &fixtures::TempSource) -> Env {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");
    // Same canonicalisation `register` does, so paths line up on macOS.
    let root = fixture.root().canonicalize().expect("canonicalize");
    let source_id = cache
        .register_source("s", &root.to_string_lossy(), SourceStatus::Active)
        .expect("register_source");
    Env {
        _cache_dir: cache_dir,
        cache,
        source_id,
        root,
    }
}

impl Env {
    fn scan(&self) -> scan::ScanStats {
        scan::scan_source(
            &self.cache,
            self.source_id,
            &self.root,
            &IgnoreRules::default(),
        )
        .expect("scan_source")
    }

    /// What the watcher does with a batch of changed paths.
    fn reindex(&self, rels: &[&str]) -> scan::ChangeOutcome {
        let changed: HashSet<PathBuf> = rels.iter().map(|r| self.root.join(r)).collect();
        scan::apply_change_batch(
            &self.cache,
            self.source_id,
            &self.root,
            &changed,
            &IgnoreRules::default(),
        )
        .expect("apply_change_batch")
    }

    fn unit(&self, rel: &str) -> Option<UnitRow> {
        self.cache
            .get_unit_by_rel_path(self.source_id, rel)
            .expect("get_unit_by_rel_path")
    }

    fn row(&self, rel: &str) -> UnitRow {
        self.unit(rel)
            .unwrap_or_else(|| panic!("{rel} should be indexed"))
    }
}

fn running_as_root() -> bool {
    unsafe {
        extern "C" {
            fn geteuid() -> u32;
        }
        geteuid() == 0
    }
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

#[test]
fn a_freshly_indexed_file_has_a_hash_an_inode_and_a_device() {
    let fixture = fixtures::empty_source();
    fixture.write("note.md", "hello\n");
    fixture.write("src/lib.rs", "fn main() {}\n");
    fixture.write("data.txt", "just text\n");
    fixture.write_bytes("blob.bin", BLOB);
    let env = env_for(&fixture);
    env.scan();

    for rel in ["note.md", "src/lib.rs", "data.txt", "blob.bin"] {
        let row = env.row(rel);
        let meta = fs::metadata(env.root.join(rel)).unwrap();
        assert_eq!(row.inode, Some(meta.ino()), "{rel}");
        assert_eq!(row.device, Some(meta.dev()), "{rel}");
        let hash = row.hash.unwrap_or_else(|| panic!("{rel} has no hash"));
        assert_eq!(hash.len(), 64, "{rel}");
        assert!(hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }
    // Known digests, from outside the code under test.
    assert_eq!(env.row("note.md").hash.as_deref(), Some(HELLO_SHA256));
    assert_eq!(env.row("blob.bin").hash.as_deref(), Some(BLOB_SHA256));
    assert_eq!(env.row("blob.bin").kind, "binary");
}

#[test]
fn the_same_bytes_in_two_files_give_the_same_hash_and_different_identities() {
    let fixture = fixtures::empty_source();
    fixture.write("one.md", "hello\n");
    fixture.write("sub/two.md", "hello\n");
    fixture.write("three.txt", "hello\n");
    fixture.write("other.md", "hello!\n");
    let env = env_for(&fixture);
    env.scan();

    let hashes: Vec<_> = ["one.md", "sub/two.md", "three.txt"]
        .iter()
        .map(|r| env.row(r).hash)
        .collect();
    assert!(hashes.iter().all(|h| h.as_deref() == Some(HELLO_SHA256)));
    assert_ne!(env.row("other.md").hash.as_deref(), Some(HELLO_SHA256));

    // Two different files on one volume never share an identity.
    let ids: HashSet<_> = ["one.md", "sub/two.md", "three.txt", "other.md"]
        .iter()
        .map(|r| {
            let row = env.row(r);
            (row.device.unwrap(), row.inode.unwrap())
        })
        .collect();
    assert_eq!(ids.len(), 4);

    let found = env.cache.units_by_hash(HELLO_SHA256).unwrap();
    let mut paths: Vec<_> = found.iter().map(|r| r.rel_path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["one.md", "sub/two.md", "three.txt"]);
}

/// The stored hash is the parser's own SHA-256 of a UTF-8 file, so the two
/// can never drift: `ParsedUnit::content_hash` (prose and code) and
/// `ParsedDocument::content_hash` (a document).
#[test]
fn the_stored_hash_equals_the_parsers_sha256_for_utf8_files() {
    let fixture = fixtures::empty_source();
    let md = "# Title\n\nLinks to [[Other]] and café — naïve. ^abc123\n";
    let rs = "// see [[Other]]\nfn main() {}\n";
    fixture.write("n.md", md);
    fixture.write("c.rs", rs);
    let docx = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tephra-parser/tests/fixtures/documents/headings.docx"),
    )
    .unwrap();
    fixture.write_bytes("d.docx", &docx);
    let env = env_for(&fixture);
    env.scan();

    let parsed_md = tephra_parser::parse_unit(Path::new("n.md"), md);
    assert_eq!(
        env.row("n.md").hash.as_deref(),
        Some(parsed_md.content_hash.as_str())
    );

    let lang = tephrad::file_kind::language_for("c.rs", rs.as_bytes()).expect("rust grammar");
    let parsed_rs = tephra_parser::parse_code_unit(Path::new("c.rs"), lang, rs);
    assert_eq!(
        env.row("c.rs").hash.as_deref(),
        Some(parsed_rs.content_hash.as_str())
    );

    let parsed_doc = tephra_parser::parse_document_bytes(Path::new("d.docx"), &docx).unwrap();
    assert_eq!(env.row("d.docx").kind, "document");
    assert_eq!(
        env.row("d.docx").hash.as_deref(),
        Some(parsed_doc.content_hash.as_str())
    );

    // And the exported helper is that same function.
    assert_eq!(
        tephra_parser::content_hash_bytes(md.as_bytes()),
        parsed_md.content_hash
    );
}

#[test]
fn editing_a_file_changes_its_hash_on_the_next_index_and_touching_its_mtime_does_not() {
    let fixture = fixtures::empty_source();
    fixture.write("a.md", "first\n");
    fixture.write("b.md", "stays\n");
    let env = env_for(&fixture);
    env.scan();
    let a0 = env.row("a.md");
    let b0 = env.row("b.md");

    // mtime only: same bytes, a different time.
    let path = env.root.join("a.md");
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
    env.reindex(&["a.md"]);
    let a1 = env.row("a.md");
    assert_eq!(a1.mtime, Some(1_600_000_000), "the new mtime was picked up");
    assert_ne!(a1.mtime, a0.mtime);
    assert_eq!(a1.hash, a0.hash, "a touch leaves the hash alone");
    assert_eq!((a1.inode, a1.device), (a0.inode, a0.device));

    // An edit: other bytes.
    fixture.write("a.md", "second, longer\n");
    env.reindex(&["a.md"]);
    let a2 = env.row("a.md");
    assert_ne!(a2.hash, a0.hash, "an edit changes the hash");
    assert!(a2.hash.is_some());
    assert_eq!(a2.size, Some("second, longer\n".len() as i64));
    // An in-place write keeps the inode.
    assert_eq!(a2.inode, a0.inode);

    assert_eq!(env.row("b.md"), b0, "an untouched file's row is unchanged");
}

#[test]
fn the_real_watcher_refreshes_the_hash_of_only_the_file_that_changed() {
    let fixture = fixtures::empty_source();
    fixture.write("a.md", "first\n");
    fixture.write("b.md", "stays\n");
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    let mut events = registry.subscribe();
    registry.register("notes", fixture.root()).unwrap();

    let read = |rel: &str| -> UnitRow {
        let cache = Cache::open(&cache_path).unwrap();
        let source = cache.get_source("notes").unwrap().unwrap();
        cache.get_unit_by_rel_path(source.id, rel).unwrap().unwrap()
    };
    let (a0, b0) = (read("a.md"), read("b.md"));
    assert!(a0.hash.is_some() && a0.inode.is_some() && a0.device.is_some());

    fixture.write("a.md", "edited by the watcher test\n");
    // Wait for the edit to be indexed, not for the first event: the OS can
    // hand the watcher the files written to set the source up as a batch
    // of its own after it started, and that batch re-indexes both files
    // before the edit's batch comes.
    let fired = wait_until(Duration::from_secs(10), || {
        while events.try_recv().is_ok() {}
        read("a.md").hash != a0.hash
    });
    assert!(fired, "the edit was never indexed");

    let (a1, b1) = (read("a.md"), read("b.md"));
    assert!(a1.hash.is_some());
    assert_eq!((a1.inode, a1.device), (a0.inode, a0.device));
    // b.md was not written: its row says the same of the file. (A batch
    // for the setup writes may have indexed it again under a new row id,
    // which is not a change to the file.)
    assert_eq!(
        (b1.hash, b1.mtime, b1.size, b1.inode, b1.device),
        (b0.hash, b0.mtime, b0.size, b0.inode, b0.device)
    );
    registry.unregister("notes").unwrap();
}

#[test]
fn a_file_over_the_cap_has_identity_and_no_hash_and_too_large_stays_set() {
    let fixture = fixtures::empty_source();
    let over = fixture.root().join("over.dat");
    {
        let f = fs::File::create(&over).unwrap();
        f.set_len(MAX_INDEXED_BYTES + 1).unwrap();
    }
    // Exactly at the cap is read and hashed.
    {
        let f = fs::File::create(fixture.root().join("at_cap.dat")).unwrap();
        f.set_len(MAX_INDEXED_BYTES).unwrap();
    }
    let mut over_md = b"# Big\n".to_vec();
    over_md.resize(MAX_INDEXED_BYTES as usize + 1, b'x');
    fixture.write_bytes("over.md", &over_md);
    let env = env_for(&fixture);
    env.scan();

    for rel in ["over.dat", "over.md"] {
        let row = env.row(rel);
        let meta = fs::metadata(env.root.join(rel)).unwrap();
        assert!(row.too_large, "{rel}");
        assert_eq!(row.hash, None, "{rel}");
        assert_eq!(
            (row.device, row.inode),
            (Some(meta.dev()), Some(meta.ino())),
            "{rel}"
        );
        let by_identity = env.cache.units_by_identity(meta.dev(), meta.ino()).unwrap();
        assert_eq!(by_identity.len(), 1, "{rel}");
        assert_eq!(by_identity[0].rel_path, rel);
    }
    let at_cap = env.row("at_cap.dat");
    assert!(!at_cap.too_large);
    assert_eq!(at_cap.hash.as_deref(), Some(AT_CAP_NULS_SHA256));

    // A file that grows past the cap loses its hash on the next index and
    // keeps its identity; one that shrinks back gets it again.
    let f = fs::OpenOptions::new()
        .write(true)
        .open(env.root.join("at_cap.dat"))
        .unwrap();
    f.set_len(MAX_INDEXED_BYTES + 1).unwrap();
    drop(f);
    env.reindex(&["at_cap.dat"]);
    let grown = env.row("at_cap.dat");
    assert!(grown.too_large);
    assert_eq!(grown.hash, None);
    assert_eq!(grown.inode, at_cap.inode);
    let f = fs::OpenOptions::new()
        .write(true)
        .open(env.root.join("at_cap.dat"))
        .unwrap();
    f.set_len(MAX_INDEXED_BYTES).unwrap();
    drop(f);
    env.reindex(&["at_cap.dat"]);
    let back = env.row("at_cap.dat");
    assert!(!back.too_large);
    assert_eq!(back.hash.as_deref(), Some(AT_CAP_NULS_SHA256));
}

#[test]
fn an_empty_file_is_hashed_as_zero_bytes_and_two_empty_files_are_both_found() {
    let fixture = fixtures::empty_source();
    fixture.write("empty-a.md", "");
    fixture.write("sub/empty-b.txt", "");
    fixture.write("not-empty.md", "x\n");
    let env = env_for(&fixture);
    env.scan();

    for rel in ["empty-a.md", "sub/empty-b.txt"] {
        let row = env.row(rel);
        assert_eq!(row.size, Some(0), "{rel}");
        assert_eq!(row.hash.as_deref(), Some(EMPTY_SHA256), "{rel}");
    }
    let found = env.cache.units_by_hash(EMPTY_SHA256).unwrap();
    let mut paths: Vec<_> = found.iter().map(|r| r.rel_path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["empty-a.md", "sub/empty-b.txt"]);
}

#[test]
fn non_utf8_files_are_hashed_from_their_bytes_with_no_lossy_conversion() {
    let fixture = fixtures::empty_source();
    fixture.write_bytes("latin1.txt", NOT_UTF8);
    fixture.write_bytes("latin1.md", NOT_UTF8);
    fixture.write_bytes("latin1.rs", NOT_UTF8);
    fixture.write_bytes("blob.bin", BLOB);
    let env = env_for(&fixture);
    env.scan();

    for rel in ["latin1.txt", "latin1.md", "latin1.rs"] {
        let row = env.row(rel);
        assert_eq!(
            row.hash.as_deref(),
            Some(NOT_UTF8_SHA256),
            "{rel} ({})",
            row.kind
        );
        assert!(!row.too_large);
    }
    let blob = env.row("blob.bin");
    assert_eq!(
        (blob.kind.as_str(), blob.hash.as_deref()),
        ("binary", Some(BLOB_SHA256))
    );
    // The kinds that took the non-UTF-8 bytes are the existing ones.
    assert_eq!(env.row("latin1.md").kind, "binary");
}

#[test]
fn a_text_other_file_is_hashed() {
    let fixture = fixtures::empty_source();
    fixture.write("notes.log", "hello\n");
    let env = env_for(&fixture);
    env.scan();
    let row = env.row("notes.log");
    assert_eq!(row.kind, "text-other");
    assert_eq!(row.hash.as_deref(), Some(HELLO_SHA256));
}

#[test]
fn hard_links_share_one_identity_and_a_different_file_does_not() {
    let fixture = fixtures::empty_source();
    fixture.write("original.md", "hello\n");
    fixture.write("separate.md", "hello\n");
    fs::hard_link(
        fixture.root().join("original.md"),
        fixture.root().join("linked.md"),
    )
    .unwrap();
    let env = env_for(&fixture);
    env.scan();

    let (o, l, s) = (
        env.row("original.md"),
        env.row("linked.md"),
        env.row("separate.md"),
    );
    assert_eq!((o.device, o.inode), (l.device, l.inode));
    assert_ne!((o.device, o.inode), (s.device, s.inode));
    assert_eq!(o.hash, s.hash, "same bytes, still a different file");

    let found = env
        .cache
        .units_by_identity(o.device.unwrap(), o.inode.unwrap())
        .unwrap();
    let mut paths: Vec<_> = found.iter().map(|r| r.rel_path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["linked.md", "original.md"]);
}

/// A symlink is never indexed (the scan's existing rule: it is not content
/// of the source), so the identity the scan records is that of the real
/// file it reads, and a dangling link, which has nothing to read, neither
/// aborts the scan nor leaves a row.
#[test]
fn symlinks_are_not_indexed_and_a_dangling_one_does_not_abort_the_scan() {
    let fixture = fixtures::empty_source();
    fixture.write("target.md", "hello\n");
    fixture.write("after.md", "after\n");
    std::os::unix::fs::symlink("target.md", fixture.root().join("link.md")).unwrap();
    std::os::unix::fs::symlink("missing.md", fixture.root().join("dangling.md")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/elsewhere", fixture.root().join("zz-dangling"))
        .unwrap();
    let env = env_for(&fixture);
    let stats = env.scan();

    assert_eq!(stats.units_written, 2);
    assert!(env.unit("link.md").is_none());
    assert!(env.unit("dangling.md").is_none());
    assert!(env.unit("zz-dangling").is_none());
    let target = env.row("target.md");
    let meta = fs::metadata(env.root.join("link.md")).unwrap(); // follows the link
    assert_eq!(
        (target.device, target.inode),
        (Some(meta.dev()), Some(meta.ino()))
    );
    assert_eq!(target.hash.as_deref(), Some(HELLO_SHA256));
    assert!(
        env.unit("after.md").is_some(),
        "the walk went on past the links"
    );

    // The same through the change-batch path.
    let outcome = env.reindex(&["link.md", "dangling.md"]);
    assert_eq!(outcome.stats.units_written, 0);
    assert!(env.unit("link.md").is_none());
}

#[test]
fn a_file_that_vanishes_before_it_is_read_is_skipped_without_a_panic() {
    let fixture = fixtures::empty_source();
    fixture.write("gone.md", "hello\n");
    fixture.write("kept.md", "kept\n");
    let env = env_for(&fixture);
    env.scan();
    assert!(env.unit("gone.md").is_some());

    // The watcher was told about the file; by the time it looks, it is gone.
    fs::remove_file(env.root.join("gone.md")).unwrap();
    env.reindex(&["gone.md", "never-existed.md"]);
    assert!(env.unit("gone.md").is_none());
    assert!(env.unit("never-existed.md").is_none());
    assert!(env.row("kept.md").hash.is_some());

    // And in a full scan, a file removed after the previous scan is gone
    // from the rows and the others are re-stamped.
    let stats = scan::rescan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .expect("rescan");
    assert_eq!(stats.units_written, 1);
}

/// Today a file the scan cannot open is skipped with a warning and gets no
/// row at all (pinned by `file_kinds.rs`); the scan carries on and no hash
/// is invented for it.
#[test]
fn an_unreadable_file_does_not_crash_the_scan_and_gets_no_hash() {
    let fixture = fixtures::empty_source();
    fixture.write("good.md", "hello\n");
    fixture.write("secret.md", "hidden\n");
    fixture.write("secret.bin", "hidden\n");
    let secrets = ["secret.md", "secret.bin"].map(|p| fixture.root().join(p));
    for p in &secrets {
        fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let readable_anyway = fs::read(&secrets[0]).is_ok();
    let env = env_for(&fixture);
    let result = scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    );
    let batch = {
        let changed: HashSet<PathBuf> = secrets.iter().cloned().collect();
        scan::apply_change_batch(
            &env.cache,
            env.source_id,
            &env.root,
            &changed,
            &IgnoreRules::default(),
        )
    };
    for p in &secrets {
        fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
    }
    result.expect("one unreadable file must not abort the scan");
    batch.expect("nor the change batch");

    assert_eq!(env.row("good.md").hash.as_deref(), Some(HELLO_SHA256));
    if readable_anyway || running_as_root() {
        return;
    }
    for rel in ["secret.md", "secret.bin"] {
        assert!(env.unit(rel).is_none(), "{rel}: skipped, as before");
        assert!(env
            .cache
            .units_by_hash(HELLO_SHA256)
            .unwrap()
            .iter()
            .all(|r| r.rel_path != rel));
    }
}

#[test]
fn a_rescan_fills_hash_and_identity_that_a_rebuild_left_empty() {
    // The upgrade path: a rebuild empties the online rows and an offline
    // source's come back with no hash or identity and `needs_rescan = 1`;
    // the next online scan fills them.
    let fixture = fixtures::empty_source();
    fixture.write("a.md", "hello\n");
    fixture.write("b.txt", "hello\n");
    let mut env = env_for(&fixture);
    env.scan();
    let before = env.row("a.md");
    assert!(before.hash.is_some());

    env.cache
        .transition_source_status("s", SourceStatus::Offline)
        .unwrap();
    env.cache
        .rebuild(&tephra_cache::FaultInjector::none())
        .unwrap();
    let carried = env.row("a.md");
    assert_eq!(
        (carried.hash.clone(), carried.inode, carried.device),
        (None, None, None)
    );
    assert!(carried.needs_rescan);
    assert!(env.cache.units_by_hash(HELLO_SHA256).unwrap().is_empty());

    env.cache
        .transition_source_status("s", SourceStatus::Stale)
        .unwrap();
    scan::rescan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .unwrap();
    let after = env.row("a.md");
    assert!(!after.needs_rescan);
    assert_eq!(after.hash.as_deref(), Some(HELLO_SHA256));
    assert_eq!((after.inode, after.device), (before.inode, before.device));
    assert_eq!(env.cache.units_by_hash(HELLO_SHA256).unwrap().len(), 2);
}
