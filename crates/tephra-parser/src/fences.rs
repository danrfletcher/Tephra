//! Shared helper: track whether byte offsets fall inside a sorted
//! set of (half-open) ranges — fenced/indented code blocks, inline
//! code spans, and HTML blocks/inline HTML identified by the
//! `pulldown-cmark` pass — so `wikilinks`/`tags`/`blocks` can skip
//! scanning inside them.
//!
//! [`FenceCursor`] assumes `ranges` is sorted ascending by start and
//! is queried with non-decreasing offsets, which holds for every
//! caller: each scans the body strictly left-to-right. It only ever
//! advances forward over the range list (never re-scans from the
//! start), so a full scan touches the range list once overall
//! instead of once per candidate character — O(n + ranges) total
//! instead of the O(candidates * ranges) a linear `any()` check per
//! candidate would cost on a file with many short fenced blocks.

pub(crate) struct FenceCursor<'a> {
    ranges: &'a [(usize, usize)],
    idx: usize,
}

impl<'a> FenceCursor<'a> {
    pub(crate) fn new(ranges: &'a [(usize, usize)]) -> Self {
        FenceCursor { ranges, idx: 0 }
    }

    /// Whether `offset` falls inside one of `ranges`. Callers must
    /// pass non-decreasing `offset` values across calls on the same
    /// cursor — true for every left-to-right scan over a body of
    /// text — or this can report stale (too-advanced) results.
    pub(crate) fn contains(&mut self, offset: usize) -> bool {
        while self.idx < self.ranges.len() && self.ranges[self.idx].1 <= offset {
            self.idx += 1;
        }
        self.idx < self.ranges.len()
            && offset >= self.ranges[self.idx].0
            && offset < self.ranges[self.idx].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_matches_a_naive_linear_scan_for_monotonic_offsets() {
        let ranges = [(5usize, 10), (20, 25), (30, 100)];
        let mut cursor = FenceCursor::new(&ranges);
        let naive = |offset: usize| ranges.iter().any(|&(s, e)| offset >= s && offset < e);
        for offset in 0..120 {
            assert_eq!(cursor.contains(offset), naive(offset), "offset {offset}");
        }
    }

    #[test]
    fn empty_ranges_never_contain_anything() {
        let ranges: [(usize, usize); 0] = [];
        let mut cursor = FenceCursor::new(&ranges);
        assert!(!cursor.contains(0));
        assert!(!cursor.contains(1000));
    }
}
