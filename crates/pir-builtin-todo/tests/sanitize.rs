// Integration tests (kept out of src/).

use pir_builtin_todo::*;

#[test]
fn plain_text_is_unchanged() {
    assert_eq!(sanitize_terminal_text("plain task"), "plain task");
    assert_eq!(sanitize_terminal_text(""), "");
    assert_eq!(sanitize_terminal_text("⛓ #1"), "⛓ #1");
}

#[test]
fn drops_control_sequences_and_flattens_layout() {
    assert_eq!(sanitize_terminal_text("a\u{1b}[31mred"), "ared");
    assert_eq!(sanitize_terminal_text("a\u{1b}]0;title\u{7}bc"), "abc");
    assert_eq!(sanitize_terminal_text("a\u{1b}]unterminated"), "a");
    assert_eq!(sanitize_terminal_text("a\u{9b}31mb"), "ab");
    assert_eq!(sanitize_terminal_text("a\nb\tc\rd"), "a b c d");
    assert_eq!(sanitize_terminal_text("a\u{202e}b\u{200e}c"), "abc");
    // NUL and DEL are dropped like every other control character: a
    // terminal ignores NUL, and truncating there would be C-string
    // semantics.
    assert_eq!(sanitize_terminal_text("a\u{7f}b\u{0}c"), "abc");
    assert_eq!(sanitize_terminal_text("a\u{2028}b"), "a b");
    assert_eq!(sanitize_terminal_text("a\u{2029}b"), "a b");
}

#[test]
fn osc_terminated_by_st_keeps_following_text() {
    assert_eq!(sanitize_terminal_text("a\u{1b}]0;t\u{1b}\\bc"), "abc");
    assert_eq!(sanitize_terminal_text("a\u{9d}0;t\u{9c}bc"), "abc");
}

#[test]
fn dangling_escape_is_dropped() {
    assert_eq!(sanitize_terminal_text("abc\u{1b}"), "abc");
    assert_eq!(sanitize_terminal_text("a\u{1b}Xb"), "ab");
}
