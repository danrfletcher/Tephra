//! Shared helper: is a byte offset inside one of a set of
//! (half-open) ranges? Used by `wikilinks`/`tags`/`blocks` to skip
//! scanning inside fenced code blocks identified by the
//! `pulldown-cmark` pass.

pub(crate) fn in_any_range(ranges: &[(usize, usize)], offset: usize) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| offset >= start && offset < end)
}
