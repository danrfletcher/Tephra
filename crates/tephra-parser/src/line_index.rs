//! Byte-offset <-> 1-indexed line-number helpers shared by every
//! extraction pass.

/// 1-indexed line number of the byte at `offset` within `text`. Used
/// for one-off lookups (frontmatter has at most a handful of
/// diagnostics per file). `offset` is clamped to `text.len()` so an
/// off-the-end offset (e.g. an empty range at end-of-file) never
/// panics.
pub(crate) fn line_number_at(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    1 + text.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// A precomputed index of newline byte-offsets, for O(log n)
/// offset-to-line lookups. Built once per body and then queried once
/// per extracted item (heading, link, tag, ...) — `line_number_at`
/// above is O(offset) per call, which would make a single
/// extraction pass over a large file with many matches quadratic
/// overall; this amortizes the newline scan to a single O(n) pass.
pub(crate) struct LineIndex {
    newline_offsets: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let newline_offsets = text
            .as_bytes()
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| if b == b'\n' { Some(i) } else { None })
            .collect();
        LineIndex { newline_offsets }
    }

    /// 1-indexed line number of the byte at `offset`.
    pub(crate) fn line_number(&self, offset: usize) -> usize {
        1 + self.newline_offsets.partition_point(|&nl| nl < offset)
    }
}
