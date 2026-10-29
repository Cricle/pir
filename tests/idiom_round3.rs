//! Idiom round 3 regression tests: the typed replacements for the sentinel
//! values polished this round —
//! - `SearchState.selected: Option<usize>` (was a `usize::MAX` sentinel),
//! - `ConfirmDialog::handle_key` → `Option<ConfirmOutcome>` (was
//!   `Option<Result<bool, ()>>`),
//! - `AuthSelector::mode()` → typed `AuthMode` (was a `&'static str`).

use pir::tui::altscreen_search::{refresh_search, AltScreenState, SearchState};
use pir::tui::command_dialogs::{
    AuthMode, AuthProviderKind, AuthProviderOption, AuthSelector, ConfirmDialog, ConfirmKey,
    ConfirmOutcome,
};

#[test]
fn search_selected_is_option_typed() {
    // Fresh query: nothing selected yet → the box shows `/m` (no current
    // match) and selected_match() is None.
    let mut state = SearchState {
        query: "x".to_string(),
        ..SearchState::default()
    };
    assert_eq!(state.selected, None);
    assert!(state.selected_match().is_none());
    assert_eq!(state.result_text(), "No matches");
    // With matches but no navigation: `/2`.
    state.matches = (0..2)
        .map(|row| pir::tui::altscreen_search::SearchMatch {
            segments: vec![pir::tui::altscreen_search::SearchSegment {
                row,
                start_col: 0,
                end_col: 1,
            }],
        })
        .collect();
    assert_eq!(state.result_text(), "/2");
    // Navigate to the second match: `2/2` and the match resolves.
    state.selected = Some(1);
    assert_eq!(state.result_text(), "2/2");
    assert!(state.selected_match().is_some());
}

#[test]
fn refresh_search_leaves_no_selection_on_first_query() {
    let frame = vec!["one match here".to_string()];
    let mut state = AltScreenState::default();
    state.search.active = true;
    state.search.query = "match".to_string();
    state.search.frame_version = u64::MAX;
    assert!(refresh_search(&mut state, &frame, 0, 1));
    // First search on this query: nothing selected (upstream
    // `selectedIndex = -1`) — now `None` instead of `usize::MAX`.
    assert_eq!(state.search.selected, None);
}

#[test]
fn confirm_dialog_outcomes_are_typed() {
    let dialog = ConfirmDialog::new("t".to_string(), "m".to_string());
    // Arrow keys only move the highlight.
    assert_eq!(dialog.handle_key(ConfirmKey::Left), None);
    // Enter answers with the highlighted value; Esc cancels.
    assert_eq!(
        dialog.handle_key(ConfirmKey::Enter),
        Some(ConfirmOutcome::Answered(false))
    );
    assert_eq!(
        dialog.handle_key(ConfirmKey::Yes),
        Some(ConfirmOutcome::Answered(true))
    );
    assert_eq!(
        dialog.handle_key(ConfirmKey::Esc),
        Some(ConfirmOutcome::Cancelled)
    );
}

#[test]
fn auth_selector_mode_is_typed_with_upstream_name() {
    let login = AuthSelector::login(vec![AuthProviderOption {
        id: "anthropic".to_string(),
        label: "anthropic".to_string(),
        status: String::new(),
        status_kind: None,
        kind: AuthProviderKind::OAuth,
        subscription: false,
        description: None,
    }]);
    assert_eq!(login.mode(), AuthMode::Login);
    assert_eq!(login.mode().as_str(), "login");

    let logout = AuthSelector::logout(Vec::new());
    assert_eq!(logout.mode(), AuthMode::Logout);
    assert_eq!(logout.mode().as_str(), "logout");
}
