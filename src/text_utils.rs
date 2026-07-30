//! Text utilities module.
//!
//! Provides pure functions for common text processing operations:
//! word counting, line counting, truncation, and slug generation.

/// Count whitespace-separated words in a string.
///
/// Returns the number of words delimited by Unicode whitespace.
/// An empty string or a string containing only whitespace returns 0.
pub fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// Count newline-separated lines in a string.
///
/// Uses `lines()` which splits on `\n` (and `\r\n`).
/// Returns 0 for an empty string.
pub fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    s.lines().count()
}

/// Truncate a string to at most `max_chars` characters.
///
/// If the string length exceeds `max_chars`, it is cut at the nearest
/// UTF-8 character boundary (using `char_indices`) and `"..."` is appended.
/// If the string fits within `max_chars`, it is returned unchanged.
/// `max_chars` of 0 produces `"..."` for any non-empty input.
pub fn truncate(s: &str, max_chars: usize) -> String {
    // If the string fits within max_chars, return it unchanged.
    if s.chars().count() <= max_chars {
        return s.to_string();
    }

    // Find the byte offset of the character at position max_chars,
    // so we never split mid-character in UTF-8.
    let cut_byte = s
        .char_indices()
        .nth(max_chars)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len());

    let mut result = String::from(&s[..cut_byte]);
    result.push_str("...");
    result
}

/// Convert a string into a URL-friendly slug.
///
/// Lowercases the string, replaces each run of non-alphanumeric characters
/// with a single hyphen, and trims leading/trailing hyphens.
pub fn slugify(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_hyphen = false;

    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                result.push(lower);
            }
            prev_was_hyphen = false;
        } else {
            // Replace runs of non-alphanumeric chars with a single hyphen.
            if !prev_was_hyphen && !result.is_empty() {
                result.push('-');
                prev_was_hyphen = true;
            }
        }
    }

    // Trim trailing hyphen if present.
    if result.ends_with('-') {
        result.pop();
    }

    result
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{count_lines, slugify, truncate, word_count};

    // ---- word_count ----

    #[test]
    fn word_count_basic() {
        assert_eq!(word_count("hello world"), 2);
        assert_eq!(word_count("one two three four"), 4);
        assert_eq!(word_count("single"), 1);
    }

    #[test]
    fn word_count_edge_cases() {
        // Empty string -> 0
        assert_eq!(word_count(""), 0);
        // Only whitespace -> 0
        assert_eq!(word_count("   \t\n  "), 0);
        // Multiple spaces between words collapse correctly
        assert_eq!(word_count("  hello   world  "), 2);
    }

    #[test]
    fn word_count_unicode() {
        // Unicode word characters are handled by split_whitespace.
        assert_eq!(word_count("héllo wörld"), 2);
        assert_eq!(word_count("你好 世界"), 2);
    }

    // ---- count_lines ----

    #[test]
    fn count_lines_basic() {
        assert_eq!(count_lines("line1\nline2\nline3"), 3);
        assert_eq!(count_lines("single line"), 1);
        assert_eq!(count_lines("a\nb"), 2);
    }

    #[test]
    fn count_lines_empty() {
        // Empty string -> 0
        assert_eq!(count_lines(""), 0);
    }

    #[test]
    fn count_lines_trailing_newline() {
        // "a\n" -> lines() yields ["a"], so 1 line.
        assert_eq!(count_lines("a\n"), 1);
        // "a\nb\n" -> ["a", "b"], 2 lines.
        assert_eq!(count_lines("a\nb\n"), 2);
        // Just a newline -> [""], 1 line.
        assert_eq!(count_lines("\n"), 1);
    }

    #[test]
    fn count_lines_crlf() {
        // \r\n is treated as a single line break by lines().
        assert_eq!(count_lines("a\r\nb\r\nc"), 3);
    }

    // ---- truncate ----

    #[test]
    fn truncate_no_change_when_short() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exact", 5), "exact");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world", 5), "hello...");
        assert_eq!(truncate("abcdef", 3), "abc...");
    }

    #[test]
    fn truncate_boundary_zero() {
        // max_chars = 0 -> "..." for any non-empty input.
        assert_eq!(truncate("abc", 0), "...");
        // Empty input with max 0 -> empty (no truncation needed).
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_unicode_no_split() {
        // Each emoji is 4 bytes in UTF-8; ensure we don't break mid-character.
        assert_eq!(truncate("😀😃😄", 2), "😀😃...");
        // Truncating to 0 gives just "..."
        assert_eq!(truncate("😀😃😄", 0), "...");
        // Truncating to 3 (exact length) returns unchanged.
        assert_eq!(truncate("😀😃😄", 3), "😀😃😄");
    }

    // ---- slugify ----

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Foo Bar Baz"), "foo-bar-baz");
    }

    #[test]
    fn slugify_empty() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_special_chars_and_collapse() {
        // Multiple non-alphanumeric chars collapse to a single hyphen.
        assert_eq!(slugify("Hello---World"), "hello-world");
        assert_eq!(slugify("Hello   World"), "hello-world");
        assert_eq!(slugify("Hello!@#World"), "hello-world");
    }

    #[test]
    fn slugify_trims_hyphens() {
        // Leading/trailing non-alphanumeric chars are trimmed.
        assert_eq!(slugify("---Hello World---"), "hello-world");
        assert_eq!(slugify("!!!test!!!"), "test");
    }

    #[test]
    fn slugify_unicode() {
        // Unicode alphanumeric chars are kept (lowercased).
        assert_eq!(slugify("Héllo Wörld"), "héllo-wörld");
        // Numbers are alphanumeric.
        assert_eq!(slugify("Test 123"), "test-123");
    }
}