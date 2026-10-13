//! `tests/unit/large_file.rs`: a single file far larger than a
//! typical note parses within a bounded time/memory budget (see
//! `tephra_parser::RECOMMENDED_MAX_FILE_BYTES`'s doc comment) — both
//! without fences and with heavy fence usage (R3: fenced-range
//! lookups must stay close to linear in the number of fences, not
//! `O(matches * fences)`, or a file of many short fenced blocks plus
//! tagged paragraphs blows both the time and memory budget even
//! though a same-sized fence-free file stays comfortably within it).

use std::path::Path;
use std::time::{Duration, Instant};

use tephra_parser::{parse_unit, ParsedUnit, RECOMMENDED_MAX_FILE_BYTES};

/// Best-effort current resident set size, in bytes, of this process.
/// `None` if it couldn't be determined (e.g. no `/proc` and no `ps`
/// on this platform/sandbox) — callers should treat that as "skip
/// the memory assertion", not a test failure, since this crate's own
/// correctness never depends on being able to measure this.
fn current_rss_bytes() -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
                return Some(kb * 1024);
            }
        }
    }

    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let kb: u64 = text.trim().parse().ok()?;
    Some(kb * 1024)
}

/// Repeats `unit_block` until the result is at least
/// `RECOMMENDED_MAX_FILE_BYTES` bytes — the documented practical cap
/// — rather than some fraction of it, so this test actually exercises
/// the size the doc comment advertises.
fn repeat_to_cap(unit_block: &str) -> String {
    let repeats = RECOMMENDED_MAX_FILE_BYTES / unit_block.len() + 1;
    let content = unit_block.repeat(repeats);
    assert!(content.len() >= RECOMMENDED_MAX_FILE_BYTES);
    content
}

const FENCE_FREE_UNIT_BLOCK: &str =
    "# Section\n\nSome text with a [[Note#^anchor|Alias]] link and a #tag/nested here. ^block-id\n\n";

const FENCE_HEAVY_UNIT_BLOCK: &str = "# Section\n\n```\nfn code() {}\n```\n\nSome text with a [[Note#^anchor|Alias]] link and a #tag/nested here. ^block-id\n\n";

fn assert_extracted_everything(unit: &ParsedUnit) {
    assert!(!unit.headings.is_empty());
    assert!(!unit.wikilinks.is_empty());
    assert!(!unit.tags.is_empty());
    assert!(!unit.blocks.is_empty());
}

#[test]
fn a_fence_free_multi_megabyte_file_parses_within_a_bounded_time_budget() {
    // Built from realistic repeating content (headings, tags, links)
    // rather than filler bytes, so every extraction pass actually
    // runs over real matches the whole way through.
    let large_content = repeat_to_cap(FENCE_FREE_UNIT_BLOCK);

    let start = Instant::now();
    let unit = parse_unit(Path::new("large.md"), &large_content);
    let elapsed = start.elapsed();

    assert_extracted_everything(&unit);
    assert!(
        elapsed < Duration::from_secs(8),
        "parse_unit took {elapsed:?} on a {}-byte fence-free file, exceeding the bounded budget",
        large_content.len()
    );
}

#[test]
fn a_fence_heavy_multi_megabyte_file_parses_within_a_bounded_time_budget() {
    // Many short fenced blocks interleaved with tagged/linked
    // paragraphs: before the R3 fix, `fences::in_any_range` scanned
    // every fence linearly for every wikilink/tag/block candidate,
    // making this O(matches * fences) instead of amortized O(n). A
    // same-sized real-world 6 MB fence-heavy file took ~23.4s before
    // the fix (vs. 0.41s for an equivalent fence-free file).
    let large_content = repeat_to_cap(FENCE_HEAVY_UNIT_BLOCK);

    let start = Instant::now();
    let unit = parse_unit(Path::new("large-fenced.md"), &large_content);
    let elapsed = start.elapsed();

    assert_extracted_everything(&unit);
    assert!(
        elapsed < Duration::from_secs(8),
        "parse_unit took {elapsed:?} on a {}-byte fence-heavy file, exceeding the bounded budget",
        large_content.len()
    );
}

#[test]
fn a_fence_heavy_file_costs_no_more_time_or_memory_than_a_same_sized_fence_free_file() {
    // The absolute RSS of parsing a multi-MB file includes baseline
    // process/allocator overhead unrelated to this crate, so this
    // asserts the *relative* cost instead: parsing a fence-heavy
    // file must not cost dramatically more time or memory than an
    // equivalently-sized fence-free file. Before the R3 fix this
    // ratio was wildly lopsided (seconds vs. tens of seconds,
    // megabytes vs. hundreds of megabytes); after it, both files are
    // dominated by the same O(n) scan over the body, so the ratio
    // should be close to 1. Both parses run in this one test
    // function, back-to-back, so a parallel test-runner thread
    // allocating memory elsewhere can't skew the RSS comparison.
    let fence_free_content = repeat_to_cap(FENCE_FREE_UNIT_BLOCK);
    let fence_heavy_content = repeat_to_cap(FENCE_HEAVY_UNIT_BLOCK);

    let rss_before_free = current_rss_bytes();
    let free_start = Instant::now();
    let free_unit = parse_unit(Path::new("large.md"), &fence_free_content);
    let free_elapsed = free_start.elapsed();
    let rss_after_free = current_rss_bytes();

    let rss_before_heavy = current_rss_bytes();
    let heavy_start = Instant::now();
    let heavy_unit = parse_unit(Path::new("large-fenced.md"), &fence_heavy_content);
    let heavy_elapsed = heavy_start.elapsed();
    let rss_after_heavy = current_rss_bytes();

    assert_extracted_everything(&free_unit);
    assert_extracted_everything(&heavy_unit);

    // Generous multiplier: fences add some real overhead (the
    // CommonMark pass itself has more events to emit), but nothing
    // close to the >50x blowup the pre-fix O(matches * fences)
    // behavior produced.
    let time_bound = free_elapsed.mul_f64(6.0).max(Duration::from_millis(500));
    assert!(
        heavy_elapsed < time_bound,
        "fence-heavy parse took {heavy_elapsed:?} vs. {free_elapsed:?} for an equivalent \
         fence-free file — more than the {time_bound:?} bound, suggesting a quadratic-in-fences \
         regression"
    );

    if let (Some(fb), Some(fa), Some(hb), Some(ha)) = (
        rss_before_free,
        rss_after_free,
        rss_before_heavy,
        rss_after_heavy,
    ) {
        let free_grew = fa.saturating_sub(fb);
        let heavy_grew = ha.saturating_sub(hb);
        // Same generous multiplier, with a floor so two very small
        // (near-zero, possibly already-resident) deltas don't make
        // the ratio check meaningless.
        let mem_bound = (free_grew.max(32 * 1024 * 1024) as f64 * 6.0) as u64;
        assert!(
            heavy_grew < mem_bound,
            "fence-heavy parse grew RSS by {heavy_grew} bytes vs. {free_grew} bytes for an \
             equivalent fence-free file — more than the {mem_bound}-byte bound, suggesting a \
             quadratic-in-fences regression"
        );
    }
}
