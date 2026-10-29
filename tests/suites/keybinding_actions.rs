//! Task G10 (upstream P1-11): the action ids missing from pir's keybinding
//! registries — `app.thinking.save`, `app.message.copy`, the six
//! `app.models.*` selector actions, the seven `app.tree.filter.*` modes and
//! the three `tui.altScreen.*` prompt-navigation ids — registered with
//! upstream-verbatim defaults (`core/keybindings.ts:37-197`).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use pir::config::keybindings::{
    default_keybinding_definitions, default_keys_for, is_valid_chord, use_windows_keybindings,
};
use pir::tui::keybindings::{Keybindings, TuiAction};

/// Every chord in the G10 registry must parse with the shared key parser.
#[test]
fn g10_default_chords_all_parse() {
    for definition in default_keybinding_definitions() {
        assert!(
            !definition.default_keys.is_empty(),
            "{} must define at least one default chord",
            definition.id
        );
        for chord in &definition.default_keys {
            assert!(
                is_valid_chord(chord),
                "{} default chord {chord:?} must parse",
                definition.id
            );
        }
    }
}

/// The registry carries the upstream ids pir's TuiAction does not mirror:
/// the 18 G10 additions plus the two tree label ids wired through
/// `app.tree.editLabel` / `app.tree.toggleLabelTimestamp`.
#[test]
fn g10_registry_has_the_missing_upstream_ids() {
    let ids: Vec<&str> = default_keybinding_definitions()
        .iter()
        .map(|definition| definition.id)
        .collect();
    let expected: &[&str] = &[
        "app.editor.external",
        "app.message.dequeue",
        "app.thinking.save",
        "app.message.copy",
        "app.models.save",
        "app.tree.editLabel",
        "app.tree.toggleLabelTimestamp",
        "app.tree.foldOrUp",
        "app.tree.unfoldOrDown",
        "app.models.enableAll",
        "app.models.clearAll",
        "app.models.toggleProvider",
        "app.models.reorderUp",
        "app.models.reorderDown",
        "app.tree.filter.default",
        "app.tree.filter.noTools",
        "app.tree.filter.userOnly",
        "app.tree.filter.labeledOnly",
        "app.tree.filter.all",
        "app.tree.filter.cycleForward",
        "app.tree.filter.cycleBackward",
        "tui.altScreen.previousPrompt",
        "tui.altScreen.nextPrompt",
        "tui.altScreen.search",
    ];
    assert_eq!(ids.len(), expected.len());
    for id in expected {
        assert!(ids.contains(id), "missing action id {id}");
    }
}

/// Platform-independent defaults are verbatim from upstream
/// `core/keybindings.ts`; the `tui.altScreen.*` ids follow the
/// `windowsKeybindings` branch (keybindings.ts:186-196).
#[test]
fn g10_defaults_match_upstream_verbatim() {
    let expect = |id: &str| -> Vec<String> {
        default_keys_for(id).unwrap_or_else(|| panic!("{id} must be registered"))
    };
    assert_eq!(expect("app.thinking.save"), vec!["ctrl+s"]);
    assert_eq!(expect("app.message.copy"), vec!["ctrl+x"]);
    assert_eq!(expect("app.models.save"), vec!["ctrl+s"]);
    assert_eq!(expect("app.tree.editLabel"), vec!["shift+l"]);
    assert_eq!(expect("app.tree.toggleLabelTimestamp"), vec!["shift+t"]);
    assert_eq!(expect("app.models.enableAll"), vec!["ctrl+a"]);
    assert_eq!(expect("app.models.clearAll"), vec!["ctrl+x"]);
    assert_eq!(expect("app.models.toggleProvider"), vec!["ctrl+p"]);
    assert_eq!(expect("app.models.reorderUp"), vec!["alt+up"]);
    assert_eq!(expect("app.models.reorderDown"), vec!["alt+down"]);
    assert_eq!(expect("app.tree.filter.default"), vec!["ctrl+d"]);
    assert_eq!(expect("app.tree.filter.noTools"), vec!["ctrl+t"]);
    assert_eq!(expect("app.tree.filter.userOnly"), vec!["ctrl+u"]);
    assert_eq!(expect("app.tree.filter.labeledOnly"), vec!["ctrl+l"]);
    assert_eq!(expect("app.tree.filter.all"), vec!["ctrl+a"]);
    assert_eq!(expect("app.tree.filter.cycleForward"), vec!["ctrl+o"]);
    assert_eq!(
        expect("app.tree.filter.cycleBackward"),
        vec!["shift+ctrl+o"]
    );
    let windows = use_windows_keybindings();
    if windows {
        assert_eq!(expect("tui.altScreen.previousPrompt"), vec!["ctrl+up"]);
        assert_eq!(expect("tui.altScreen.nextPrompt"), vec!["ctrl+down"]);
        assert_eq!(expect("tui.altScreen.search"), vec!["ctrl+f"]);
    } else {
        assert_eq!(
            expect("tui.altScreen.previousPrompt"),
            vec!["ctrl+shift+up", "ctrl+up"]
        );
        assert_eq!(
            expect("tui.altScreen.nextPrompt"),
            vec!["ctrl+shift+down", "ctrl+down"]
        );
        assert_eq!(expect("tui.altScreen.search"), vec!["ctrl+shift+f"]);
    }
}

/// Unknown ids resolve to `None`.
#[test]
fn g10_default_keys_for_unknown_id_is_none() {
    assert!(default_keys_for("app.nonexistent").is_none());
    assert!(default_keys_for("tui.input.submit").is_none());
}

/// The wireable G10 action (`app.message.copy`) is bound to ctrl+x in the
/// TUI keybinding map and dispatches through `TuiAction::CopyMessage`.
#[test]
fn g10_copy_message_binding_matches_ctrl_x() {
    let kb = Keybindings::new();
    let key = |code: KeyCode, modifiers: KeyModifiers| KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    assert!(kb.matches(
        TuiAction::CopyMessage,
        &key(KeyCode::Char('x'), KeyModifiers::CONTROL)
    ));
    // Unmodified x must not trigger the copy.
    assert!(!kb.matches(
        TuiAction::CopyMessage,
        &key(KeyCode::Char('x'), KeyModifiers::NONE)
    ));
}
