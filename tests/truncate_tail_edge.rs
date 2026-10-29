//! Regression tests for `truncate_tail`'s oversized-final-line edge case.
//!
//! Upstream `truncateStringToBytesFromEnd` keeps the longest char-boundary
//! suffix within the byte limit (fewer bytes rather than splitting a
//! character) and falls back to the empty string when the trailing character
//! alone exceeds the limit. The pir port scanned char boundaries in reverse,
//! which kept only the final character.

use pir::coding_agent::truncate::{truncate_tail, TruncatedBy};

#[test]
fn partial_last_line_keeps_the_longest_suffix() {
    let content = format!("short\n{}", "a".repeat(300));
    let result = truncate_tail(&content, 5, 200);

    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.last_line_partial);
    // Regression: the reverse scan kept only the final "a".
    assert_eq!(result.content, "a".repeat(200));
    assert_eq!(result.output_bytes, 200);
    assert_eq!(result.output_lines, 1);
    assert_eq!(result.total_lines, 2);
    assert_eq!(result.total_bytes, content.len());
    assert_eq!(result.last_line_bytes, 300);
}

#[test]
fn partial_cut_lands_on_a_utf8_boundary() {
    // 30 bytes of 3-byte characters; max_bytes = 7 lands inside a character.
    let content = "中".repeat(10);
    let result = truncate_tail(&content, 5, 7);

    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.last_line_partial);
    // Backed up to the boundary: two whole characters (6 bytes <= 7).
    assert_eq!(result.content, "中中");
    assert_eq!(result.output_bytes, 6);
    assert!(result.output_bytes <= 7);
}

#[test]
fn trailing_char_larger_than_take_yields_empty() {
    // 12 bytes of 4-byte characters; take = 2 cannot fit the final character.
    let content = "😀".repeat(3);
    let result = truncate_tail(&content, 5, 2);

    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.last_line_partial);
    // Upstream backs up past the end: empty suffix.
    assert_eq!(result.content, "");
    assert_eq!(result.output_bytes, 0);
    assert_eq!(result.output_lines, 1);
}

#[test]
fn multi_line_tail_truncation_smoke() {
    let content: String = (1..=10).map(|n| format!("l{n}\n")).collect();
    let result = truncate_tail(&content, 3, 50 * 1024);

    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert!(!result.last_line_partial);
    assert_eq!(result.content, "l8\nl9\nl10");
    assert_eq!(result.output_lines, 3);
    assert_eq!(result.total_lines, 10);
}
