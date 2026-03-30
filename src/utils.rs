//! Internal utility functions shared across the crate.

use crate::parser::VteParser;

/// Truncate a string to at most `max` bytes on a char boundary.
///
/// Canonical implementation — replaces all duplicates from original code.
pub(crate) fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Strip ANSI escape sequences from a string.
///
/// Thin wrapper over VteParser for one-shot use without maintaining parser state.
/// Canonical alternative to codex's standalone `strip_ansi_codes()`.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut parser = VteParser::new();
    parser.parse(s)
}
