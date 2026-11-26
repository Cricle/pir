//! Terminal overlay frame contracts: the autocomplete dock's popup rows and
//! the `ctx.ui.custom({blocked})` status mapping.
//!
//! Moved out of `src/terminal.rs` — unit tests do not live in `src/`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use pir_core::plugins::services::{FactoryRenderContext, OverlayComponent};
use pir_tui::autocomplete::{
    AutocompleteItem, AutocompletePopup, AutocompleteSuggestions, AUTOCOMPLETE_MAX_VISIBLE_DEFAULT,
};
use pir_tui::overlay_position::OverlayOptions;
use pir_tui::program_status::{BlockedStatus, ProgramStatusKind};
use pir_tui::terminal::{autocomplete_popup_frame, custom_overlay_blocked, ExternalOverlay};

fn popup(items: usize) -> AutocompletePopup {
    let suggestions = AutocompleteSuggestions {
        items: (0..items)
            .map(|index| AutocompleteItem::new(format!("/cmd{index}"), format!("/cmd{index}")))
            .collect(),
        prefix: "/cmd".to_string(),
    };
    AutocompletePopup::new(suggestions)
}

#[test]
fn popup_rows_borrow_the_slot_instead_of_cloning_the_popup() {
    let slot = Mutex::new(Some(popup(8)));
    let (item_rows, lines) = autocomplete_popup_frame(&slot, false, 40).expect("open popup");
    assert_eq!(item_rows, AUTOCOMPLETE_MAX_VISIBLE_DEFAULT);
    // The visible window plus the `(1/8)` scroll counter.
    assert_eq!(lines.len(), AUTOCOMPLETE_MAX_VISIBLE_DEFAULT + 1);
    // The render borrowed the popup: the slot still owns it afterwards.
    assert!(slot.lock().unwrap().is_some());
}

#[test]
fn popup_frame_is_hidden_when_a_selector_replaces_the_editor() {
    let slot = Mutex::new(Some(popup(2)));
    assert!(autocomplete_popup_frame(&slot, true, 40).is_none());
    assert!(slot.lock().unwrap().is_some());
}

#[test]
fn closed_popup_frame_is_none() {
    let slot: Mutex<Option<AutocompletePopup>> = Mutex::new(None);
    assert!(autocomplete_popup_frame(&slot, false, 40).is_none());
}

#[test]
fn blocked_option_maps_to_a_status() {
    let status = custom_overlay_blocked(&serde_json::json!({
        "blocked": { "kind": "permission", "message": "Allow the write?" }
    }))
    .expect("blocked option");
    assert_eq!(status.kind, ProgramStatusKind::Permission);
    assert_eq!(status.message, "Allow the write?");
}

#[test]
fn blocked_option_accepts_every_spec_kind() {
    for (kind, expected) in [
        ("permission", ProgramStatusKind::Permission),
        ("question", ProgramStatusKind::Question),
        ("auth", ProgramStatusKind::Auth),
    ] {
        let status = custom_overlay_blocked(&serde_json::json!({
            "blocked": { "kind": kind, "message": "m" }
        }))
        .expect("blocked option");
        assert_eq!(status.kind, expected, "kind `{kind}`");
    }
}

#[test]
fn unknown_kind_degrades_to_question_and_missing_message_is_empty() {
    let status = custom_overlay_blocked(&serde_json::json!({
        "blocked": { "kind": "weird" }
    }))
    .expect("blocked option");
    assert_eq!(status.kind, ProgramStatusKind::Question);
    assert_eq!(status.message, "");
}

#[test]
fn absent_blocked_option_reports_nothing() {
    assert!(custom_overlay_blocked(&serde_json::json!({ "component": "k" })).is_none());
    assert!(custom_overlay_blocked(&serde_json::json!({ "blocked": "yes" })).is_none());
}

struct Probe;

impl OverlayComponent for Probe {
    fn render(&self, _context: &FactoryRenderContext<'_>) -> Vec<String> {
        vec!["probe".to_string()]
    }
}

/// v1.5.2 keeps an overlay's blocked status while `setHidden()` hides it:
/// the component still waits for the user, so the terminal must still see
/// `blocked`. Only the interaction ending clears it.
#[test]
fn a_floating_overlay_opts_into_blocked_and_keeps_it_while_hidden() {
    let blocked = BlockedStatus {
        kind: ProgramStatusKind::Auth,
        message: "Sign in".to_string(),
    };
    // `with_position` + `with_blocked` in the order `open_custom_overlay`
    // applies them: an overlay keeps the status (it is NOT dropped).
    let overlay = ExternalOverlay::new(1, "k", Arc::new(Probe), Instant::now())
        .with_position(OverlayOptions::default())
        .with_blocked(Some(blocked.clone()));
    assert_eq!(
        overlay.blocked_status(),
        Some(&blocked),
        "a floating overlay keeps its blocked status"
    );
    overlay.set_hidden(true);
    assert!(overlay.is_hidden());
    assert_eq!(
        overlay.blocked_status(),
        Some(&blocked),
        "hiding does not clear the status"
    );
    overlay.dispose();
}

/// An inline component also keeps the option (v1.5.2 stopped restricting
/// it to the editor-slot path).
#[test]
fn an_inline_component_keeps_its_blocked_status() {
    let blocked = BlockedStatus {
        kind: ProgramStatusKind::Question,
        message: "Pick".to_string(),
    };
    let overlay = ExternalOverlay::new(2, "k", Arc::new(Probe), Instant::now())
        .with_blocked(Some(blocked.clone()));
    assert_eq!(overlay.blocked_status(), Some(&blocked));
    overlay.dispose();
}
