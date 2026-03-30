//! Internal utility functions shared across the crate.

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

