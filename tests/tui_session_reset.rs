//! `/clear` session-switch + per-session view-state reset tests.
//!
//! Bug: `/clear` only wiped the visible transcript. Upstream
//! `handleClearCommand` calls `runtimeHost.newSession()`, so the model
//! context/session file and the footer's per-session numbers must reset too.
//! The effect itself lives in `terminal.rs` (crossterm-bound); these tests pin
//! the two pure seams: the slash routing predicate and the footer-field
//! reset contract shared by every rebind (`/clear`, `/new`, `/resume`,
//! `/fork`, `/clone`, `/import`, `/tree`, `/sessions` tab switches).

use pir::tui::app::UsageTotals;
use pir::tui::slash::{action_starts_new_session, builtin_action, SlashAction};
use pir::tui::terminal::clear_session_view_fields;

/// `/clear` and `/new` are the same command upstream: both call
/// `runtimeHost.newSession()`. The dispatcher routes them through one guarded
/// arm (`action_starts_new_session` → `start_new_session`), so this predicate
/// is the routing contract.
#[test]
fn clear_and_new_both_route_to_a_new_session() {
    assert_eq!(builtin_action("clear"), Some(SlashAction::Clear));
    assert_eq!(builtin_action("new"), Some(SlashAction::New));
    assert!(action_starts_new_session(SlashAction::Clear));
    assert!(action_starts_new_session(SlashAction::New));

    // Other session commands attach/fork/replay instead of creating a fresh
    // session file, so they must not take the `/clear` route.
    assert!(!action_starts_new_session(SlashAction::Resume));
    assert!(!action_starts_new_session(SlashAction::Fork));
    assert!(!action_starts_new_session(SlashAction::Clone));
    assert!(!action_starts_new_session(SlashAction::Sessions));
    assert!(!action_starts_new_session(SlashAction::Compact));
}

/// The footer/counters reset that every rebind runs: a stale token/cost
/// total, context percentage, session name, cache-hit rate or routed-model
/// marker from the outgoing session must not survive `/clear`.
#[test]
fn session_view_reset_drops_usage_context_and_session_name() {
    let mut usage = UsageTotals {
        input: 12_345,
        output: 678,
        cache_read: 90,
        cache_write: 45,
        cost: 1.25,
    };
    let mut context_percent = Some(63.5);
    let mut session_name = Some("outgoing session".to_string());
    let mut cache_hit_rate = Some(0.75);
    let mut last_routed = Some(("anthropic".to_string(), Some("claude-sonnet".to_string())));

    clear_session_view_fields(
        &mut usage,
        &mut context_percent,
        &mut session_name,
        &mut cache_hit_rate,
        &mut last_routed,
    );

    assert_eq!(usage, UsageTotals::default());
    assert_eq!(context_percent, None);
    assert_eq!(session_name, None);
    assert_eq!(cache_hit_rate, None);
    assert_eq!(last_routed, None);
}

/// A second reset is a no-op (all fields are already at their defaults).
#[test]
fn session_view_reset_is_idempotent() {
    let mut usage = UsageTotals::default();
    let mut context_percent = None;
    let mut session_name = None;
    let mut cache_hit_rate = None;
    let mut last_routed = None;

    for _ in 0..2 {
        clear_session_view_fields(
            &mut usage,
            &mut context_percent,
            &mut session_name,
            &mut cache_hit_rate,
            &mut last_routed,
        );
        assert_eq!(usage, UsageTotals::default());
        assert_eq!(context_percent, None);
        assert_eq!(session_name, None);
        assert_eq!(cache_hit_rate, None);
        assert_eq!(last_routed, None);
    }
}
