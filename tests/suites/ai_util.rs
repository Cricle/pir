//! Tests for AI utility functions.

use pir::ai::util::now_ms;

#[test]
fn now_ms_returns_current_time() {
    let t = now_ms();
    // Should be greater than 2025-01-01 UTC
    assert!(t > 1735689600000, "now_ms={t} should be > 2025-01-01");
}

#[test]
fn now_ms_is_monotonic() {
    let a = now_ms();
    let b = now_ms();
    assert!(b >= a, "now_ms should be monotonic: {b} >= {a}");
}

#[test]
fn now_ms_is_positive() {
    assert!(now_ms() > 0);
}
