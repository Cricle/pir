//! Gate semantics of the OSC 11 terminal query (theme.rs):
//! - the process-wide gate is claimed only when a query is actually
//!   initiated (after the TTY / `PIR_DISABLE_TERMINAL_QUERY` checks pass),
//!   so a first call that fails those checks does not burn the one-query
//!   budget;
//! - claiming is first-caller-wins under concurrency (exactly one claimant
//!   passes), preserving the previous `OnceLock::set` guarantee.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::tui::theme::{osc11_query_gate_claimed, query_terminal_background, try_claim_query_gate};

/// Serializes tests that touch the process-global env var or the real
/// process-wide gate (cargo test runs tests on parallel threads).
static ENV_GATE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn gate_claims_exactly_once() {
    let gate = AtomicBool::new(false);
    assert!(try_claim_query_gate(&gate));
    assert!(gate.load(Ordering::SeqCst), "claimed gate is set");
    assert!(!try_claim_query_gate(&gate), "second claim must lose");
    assert!(gate.load(Ordering::SeqCst), "gate stays set after retry");
}

#[test]
fn gate_concurrent_claims_have_single_winner() {
    let gate = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let gate = Arc::clone(&gate);
        handles.push(std::thread::spawn(move || try_claim_query_gate(&gate)));
    }
    let mut winners = 0;
    for handle in handles {
        if handle.join().expect("claim thread panicked") {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "exactly one thread may win the claim");
    assert!(gate.load(Ordering::SeqCst));
}

#[test]
fn disabled_env_check_does_not_consume_gate() {
    let _guard = ENV_GATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A claim from a previous test in this binary would make the assertion
    // below meaningless; a claimed gate here means the environment ran a
    // real query, so skip rather than report a false failure.
    if osc11_query_gate_claimed() {
        return;
    }
    std::env::set_var("PIR_DISABLE_TERMINAL_QUERY", "1");
    let background = query_terminal_background(Duration::from_millis(10));
    std::env::remove_var("PIR_DISABLE_TERMINAL_QUERY");
    assert!(background.is_none(), "disabled env must skip the query");
    assert!(
        !osc11_query_gate_claimed(),
        "a failed check must not consume the one-query budget"
    );
}

#[test]
fn non_tty_check_does_not_consume_gate() {
    let _guard = ENV_GATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if osc11_query_gate_claimed() {
        return;
    }
    let first = query_terminal_background(Duration::from_millis(10));
    let second = query_terminal_background(Duration::from_millis(10));
    if osc11_query_gate_claimed() {
        // A real query was initiated: this environment has TTY stdin and
        // stdout, and consuming the budget there is correct behavior.
        let _ = (first, second);
        return;
    }
    assert!(
        first.is_none() && second.is_none(),
        "non-TTY calls must take the fallback path"
    );
    // The failed checks left the budget intact: a later caller (a real TTY
    // scenario appearing after the first non-TTY call) can still claim it.
    let gate = AtomicBool::new(false);
    assert!(
        try_claim_query_gate(&gate),
        "gate semantics still claimable"
    );
    assert!(
        !osc11_query_gate_claimed(),
        "the real process gate must remain unclaimed after failed checks"
    );
}
