//! E12 (memory): a scan over oversized code files must not blow up
//! the daemon's memory. tree-sitter builds a whole syntax tree in
//! memory, about 75 to 95 times the file's size, so a 15 MB
//! single-line minified `.js` (or an ordinary 15 MB `.py`) would peak
//! above 1 GB if it were parsed. Such files must be recorded as code
//! units with no links, and the good files next to them must still
//! give theirs.
//!
//! This test is alone in its own test binary on purpose: peak RSS is
//! process-wide, so no other test may allocate while it runs. The peak
//! is read from `getrusage` (`ru_maxrss`), which sees the high-water
//! mark even if the memory was freed before a sample could be taken.

mod fixtures;

use std::time::{Duration, Instant};

use tephra_cache::Cache;
use tephrad::registry::SourceRegistry;

/// Peak resident set size of this process so far, in bytes.
fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage failed");
    let raw = usage.ru_maxrss as u64;
    // macOS reports bytes, Linux kilobytes.
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

#[test]
fn e12_oversized_code_files_are_recorded_without_links_and_without_a_memory_blow_up() {
    const MB15: usize = 15 * 1024 * 1024;

    let mut js = String::with_capacity(MB15 + 64);
    js.push_str("/* [[@notes:Tephra test]] */");
    while js.len() < MB15 {
        js.push_str("var a=function(b){return b+1};");
    }
    let mut py = String::with_capacity(MB15 + 64);
    py.push_str("# [[@notes:Tephra test]]\n");
    while py.len() < MB15 {
        py.push_str("x = [1, 2, 3]  # c\n");
    }

    let vault = fixtures::empty_source();
    vault.write("Tephra test.md", "# T\n");
    let repo = fixtures::empty_source();
    repo.write("dist/min.js", &js);
    repo.write("data/big.py", &py);
    repo.write("good.rs", "// [[@notes:Tephra test]]\n");
    // The two inputs are now on disk; drop the copies so they are not
    // counted as the scan's own memory.
    drop(js);
    drop(py);

    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", vault.root()).unwrap();

    let baseline = peak_rss_bytes();
    let started = Instant::now();
    registry.register("tephra", repo.root()).unwrap();
    let elapsed = started.elapsed();
    let peak = peak_rss_bytes();

    assert!(
        elapsed < Duration::from_secs(30),
        "the scan took {elapsed:?}"
    );
    // Reading each 15 MB file into a String and hashing it costs a few
    // tens of MB at most. A parse would add over 1 GB. 256 MB leaves
    // room for the allocator and the cache without letting a parse in.
    let grew = peak.saturating_sub(baseline);
    assert!(
        grew < 256 * 1024 * 1024,
        "scanning two 15 MB code files grew peak RSS by {grew} bytes \
         (baseline {baseline}, peak {peak})"
    );

    for rel in ["dist/min.js", "data/big.py"] {
        let row = fixtures::unit_rows(&cache_path, "tephra", rel).unwrap();
        assert_eq!(row.kind, "code", "{rel} is still a code unit");
        assert!(row.links.is_empty(), "{rel}: {:?}", row.links);
    }
    let good = fixtures::unit_rows(&cache_path, "tephra", "good.rs").unwrap();
    assert_eq!(
        good.links.len(),
        1,
        "the good file next to them is unaffected"
    );

    registry.unregister("tephra").unwrap();
    registry.unregister("notes").unwrap();
}
