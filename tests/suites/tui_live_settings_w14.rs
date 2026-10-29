//! Live `fullscreen*` settings (audit §7 #13 residual).
//!
//! `fullscreenScrollbar` and `fullscreenCopyOnSelect` used to be captured in
//! the TUI's startup snapshot, so a `/settings` change after a runtime
//! `switchTuiMode` kept applying the old values. The TUI now shares a
//! [`LiveAltScreenConfig`] cell that the frame decorator and the mouse/copy
//! paths read per frame/event. These tests pin the cell and the scrollbar
//! reservation helper.

use pir::tui::altscreen_search::{
    transcript_reserves_scrollbar, AltScreenConfig, LiveAltScreenConfig, ScrollbarMode,
};

/// Every field moves live: a snapshot taken before and after a change sees the
/// new values (the decorator takes exactly this snapshot per frame).
#[test]
fn live_fullscreen_settings_follow_runtime_changes() {
    let cell = LiveAltScreenConfig::new(AltScreenConfig::default());
    assert_eq!(
        cell.snapshot(),
        AltScreenConfig {
            scrollbar: ScrollbarMode::Auto,
            copy_on_select: true
        }
    );

    cell.set_scrollbar(ScrollbarMode::Always);
    cell.set_copy_on_select(false);
    let live = cell.snapshot();
    assert_eq!(live.scrollbar, ScrollbarMode::Always);
    assert!(!live.copy_on_select);

    cell.set_scrollbar(ScrollbarMode::Hidden);
    assert_eq!(cell.snapshot().scrollbar, ScrollbarMode::Hidden);
}

/// The transcript column is reserved only for `always`, and the reservation
/// is re-derived from the live values (a settings change while fullscreen
/// flips the flag without a mode switch).
#[test]
fn transcript_scrollbar_reservation_is_derived_from_the_live_cell() {
    let cell = LiveAltScreenConfig::new(AltScreenConfig::default());
    assert!(!transcript_reserves_scrollbar(true, &cell.snapshot()));
    assert!(!transcript_reserves_scrollbar(false, &cell.snapshot()));

    cell.set_scrollbar(ScrollbarMode::Always);
    assert!(transcript_reserves_scrollbar(true, &cell.snapshot()));
    assert!(
        !transcript_reserves_scrollbar(false, &cell.snapshot()),
        "regular mode never reserves the alt-screen column"
    );

    cell.set_scrollbar(ScrollbarMode::Hidden);
    assert!(!transcript_reserves_scrollbar(true, &cell.snapshot()));
}
