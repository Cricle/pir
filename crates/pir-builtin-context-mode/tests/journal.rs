// Integration tests (kept out of src/).

use pir_builtin_context_mode::*;

#[test]
fn new_derives_byte_count_from_the_body() {
    let input = JournalInput::new("s".into(), "bash", "shell", "abcd");
    assert_eq!(input.bytes, 4);
    assert!(input.ok);
}

#[test]
fn outcome_overrides_ok_and_bytes() {
    let input =
        JournalInput::new("s".into(), "bash", "shell", "abcd").with_outcome(false, Some(2), 9);
    assert!(!input.ok);
    assert_eq!(input.exit_code, Some(2));
    assert_eq!(input.bytes, 9);
}

#[test]
fn budget_check_uses_byte_length() {
    let input = JournalInput::new("s".into(), "k", "t", "abc");
    assert!(input.body_within_budget(3));
    assert!(!input.body_within_budget(2));
}
