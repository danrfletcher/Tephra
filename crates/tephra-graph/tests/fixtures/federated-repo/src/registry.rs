//! The registry of sources (a fixture: code files that carry block IDs).

/// Why two sources conflict.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictReason { // ^k3f9a2
    DuplicateName,
    OverlappingRoot,
}

// ^f1x7b2
/// Parses the config text.
pub fn parse_config(text: &str) -> usize {
    text.len()
}
