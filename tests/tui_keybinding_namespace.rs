//! `tui.*` namespace alignment (upstream `packages/tui/src/keybindings.ts`):
//! the registry only carries ids pir's TUI owns a behavior for, defaults are
//! upstream-verbatim, settings overrides under the canonical ids resolve onto
//! the wired [`TuiAction`]s, and the mapped action defaults stay equivalent to
//! the previously hardcoded behavior (tests/suites/tui_units.rs contract).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use pir::tui::keybindings::{
    parse_key, tui_default_keys_for, tui_keybinding_definition, Keybindings, TuiAction,
    TUI_KEYBINDING_DEFINITIONS,
};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

/// The upstream-verbatim expectations (id, default chords, description) for
/// the registered subset — transcribed from `TUI_KEYBINDINGS`
/// (`packages/tui/src/keybindings.ts:71-210`).
const EXPECTED: &[(&str, &[&str], &str)] = &[
    ("tui.editor.cursorUp", &["up"], "Move cursor up"),
    ("tui.editor.cursorDown", &["down"], "Move cursor down"),
    (
        "tui.editor.historyPrevious",
        &[],
        "Select previous prompt history entry",
    ),
    (
        "tui.editor.historyNext",
        &[],
        "Select next prompt history entry",
    ),
    (
        "tui.editor.cursorLeft",
        &["left", "ctrl+b"],
        "Move cursor left",
    ),
    (
        "tui.editor.cursorRight",
        &["right", "ctrl+f"],
        "Move cursor right",
    ),
    (
        "tui.editor.cursorWordLeft",
        &["alt+left", "ctrl+left", "alt+b"],
        "Move cursor word left",
    ),
    (
        "tui.editor.cursorWordRight",
        &["alt+right", "ctrl+right", "alt+f"],
        "Move cursor word right",
    ),
    (
        "tui.editor.cursorLineStart",
        &["home", "ctrl+a"],
        "Move to line start",
    ),
    (
        "tui.editor.cursorLineEnd",
        &["end", "ctrl+e"],
        "Move to line end",
    ),
    (
        "tui.editor.jumpForward",
        &["ctrl+]"],
        "Jump forward to character",
    ),
    (
        "tui.editor.jumpBackward",
        &["ctrl+alt+]"],
        "Jump backward to character",
    ),
    (
        "tui.editor.deleteCharBackward",
        &["backspace"],
        "Delete character backward",
    ),
    (
        "tui.editor.deleteCharForward",
        &["delete", "ctrl+d"],
        "Delete character forward",
    ),
    (
        "tui.editor.deleteWordBackward",
        &["ctrl+w", "alt+backspace"],
        "Delete word backward",
    ),
    (
        "tui.editor.deleteWordForward",
        &["alt+d", "alt+delete"],
        "Delete word forward",
    ),
    (
        "tui.editor.deleteToLineStart",
        &["ctrl+u"],
        "Delete to line start",
    ),
    (
        "tui.editor.deleteToLineEnd",
        &["ctrl+k"],
        "Delete to line end",
    ),
    // pir's editor owns the kill-ring/undo/page behavior (editor kill-ring),
    // so these upstream ids are registered like upstream.
    ("tui.editor.yank", &["ctrl+y"], "Yank"),
    ("tui.editor.yankPop", &["alt+y"], "Yank pop"),
    ("tui.editor.undo", &["ctrl+-"], "Undo"),
    ("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"], "Page up"),
    (
        "tui.editor.pageDown",
        &["pageDown", "ctrl+pageDown"],
        "Page down",
    ),
    (
        "tui.input.newLine",
        &["shift+enter", "ctrl+j"],
        "Insert newline",
    ),
    ("tui.input.submit", &["enter"], "Submit input"),
    ("tui.input.tab", &["tab"], "Tab / autocomplete"),
    ("tui.select.up", &["up"], "Move selection up"),
    ("tui.select.down", &["down"], "Move selection down"),
    ("tui.select.pageUp", &["pageUp"], "Selection page up"),
    ("tui.select.pageDown", &["pageDown"], "Selection page down"),
    ("tui.select.confirm", &["enter"], "Confirm selection"),
    (
        "tui.select.cancel",
        &["escape", "ctrl+c"],
        "Cancel selection",
    ),
];

/// Ids pir does NOT own a behavior for must stay out of the registry.
const NOT_REGISTERED: &[&str] = &[
    "tui.input.copy",
    "tui.altScreen.pageUp",
    "tui.altScreen.pageDown",
    "tui.altScreen.halfPageUp",
    "tui.altScreen.halfPageDown",
    "tui.altScreen.lineUp",
    "tui.altScreen.lineDown",
    "tui.altScreen.previousPrompt",
    "tui.altScreen.nextPrompt",
    "tui.altScreen.search",
    "tui.altScreen.searchNext",
    "tui.altScreen.searchPrevious",
    "tui.altScreen.searchClose",
    "tui.altScreen.top",
    "tui.altScreen.bottom",
];

#[test]
fn registry_matches_upstream_ids_defaults_and_descriptions() {
    let definitions = TUI_KEYBINDING_DEFINITIONS;
    assert_eq!(definitions.len(), EXPECTED.len());
    for (definition, (id, default_keys, description)) in definitions.iter().zip(EXPECTED) {
        assert_eq!(definition.id, *id);
        assert_eq!(definition.default_keys, *default_keys);
        assert_eq!(definition.description, *description);
        assert_eq!(
            tui_default_keys_for(id)
                .unwrap_or_else(|| panic!("{id} must resolve"))
                .len(),
            default_keys.len()
        );
    }
}

#[test]
fn unowned_upstream_ids_are_not_registered() {
    for id in NOT_REGISTERED {
        assert!(
            tui_keybinding_definition(id).is_none(),
            "{id} must not be registered"
        );
        assert!(tui_default_keys_for(id).is_none());
    }
}

#[test]
fn registry_default_chords_all_parse() {
    for definition in TUI_KEYBINDING_DEFINITIONS {
        for chord in definition.default_keys {
            assert!(
                parse_key(chord).is_some(),
                "{} default chord {chord:?} must parse",
                definition.id
            );
        }
    }
}

/// The canonical id → action mapping is exactly these seven.
#[test]
fn canonical_id_mapping_covers_the_wired_actions() {
    let expected: &[(&str, TuiAction)] = &[
        ("tui.input.submit", TuiAction::Submit),
        ("tui.input.newLine", TuiAction::Newline),
        ("tui.input.tab", TuiAction::AutocompleteAccept),
        ("tui.select.up", TuiAction::AutocompleteUp),
        ("tui.select.down", TuiAction::AutocompleteDown),
        ("tui.editor.historyPrevious", TuiAction::HistoryPrevious),
        ("tui.editor.historyNext", TuiAction::HistoryNext),
    ];
    for (id, action) in expected {
        assert_eq!(action.upstream_id(), Some(*id));
        assert_eq!(TuiAction::from_upstream_id(id), Some(*action));
    }
    // Pir-specific / app-level actions have no tui.* id.
    for action in [
        TuiAction::Abort,
        TuiAction::Clear,
        TuiAction::Exit,
        TuiAction::Paste,
    ] {
        assert!(action.upstream_id().is_none());
        assert!(TuiAction::from_upstream_id(action.name()).is_none());
    }
}

/// Dispatch equivalence: with no settings, every mapped action matches exactly
/// its canonical id's upstream default chords (and pir keeps `alt+up`/`alt+down`
/// history defaults on the action layer, where upstream's id default is empty).
#[test]
fn default_dispatch_equivalent_to_upstream_id_defaults() {
    let kb = Keybindings::new();
    let wired: [(&str, TuiAction); 5] = [
        ("tui.input.submit", TuiAction::Submit),
        ("tui.input.newLine", TuiAction::Newline),
        ("tui.input.tab", TuiAction::AutocompleteAccept),
        ("tui.select.up", TuiAction::AutocompleteUp),
        ("tui.select.down", TuiAction::AutocompleteDown),
    ];
    for (id, action) in wired {
        let chords = tui_default_keys_for(id).expect("registered id");
        assert!(
            !chords.is_empty(),
            "{id} must contribute non-empty live defaults"
        );
        for chord in chords {
            let combo = parse_key(&chord).expect("parses");
            let event = key(combo.code, modifiers_of(&combo));
            assert!(
                kb.matches(action, &event),
                "{id}: {action:?} must match default chord {chord:?}"
            );
        }
    }
    // The id-keyed view of a default map resolves the registry defaults too.
    let event = key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(kb.matches_id("tui.select.confirm", &event));
    assert!(kb.matches_id("tui.input.submit", &event));
    assert!(kb.matches_id("tui.select.cancel", &key(KeyCode::Esc, KeyModifiers::NONE)));
    // Empty upstream history defaults: registered, but no default chord.
    assert!(kb
        .keys_for_id("tui.editor.historyPrevious")
        .is_some_and(|combos| combos.is_empty()));
    assert!(!kb.matches_id("tui.editor.historyPrevious", &event));
}

#[test]
fn settings_overrides_under_tui_ids_apply_to_wired_actions() {
    let settings = serde_json::json!({
        "tui.input.submit": "ctrl+n",
        "tui.input.newLine": "ctrl+m",
        "tui.input.tab": "ctrl+i",
        "tui.select.up": "ctrl+e",
        "tui.select.down": "ctrl+y",
        "tui.editor.historyPrevious": "ctrl+r"
    });
    let kb = Keybindings::from_settings(Some(&settings)).expect("valid object");
    assert!(kb.matches(
        TuiAction::Submit,
        &key(KeyCode::Char('n'), KeyModifiers::CONTROL)
    ));
    assert!(!kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(kb.matches(
        TuiAction::Newline,
        &key(KeyCode::Char('m'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::AutocompleteAccept,
        &key(KeyCode::Char('i'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::AutocompleteUp,
        &key(KeyCode::Char('e'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::AutocompleteDown,
        &key(KeyCode::Char('y'), KeyModifiers::CONTROL)
    ));
    assert!(kb.matches(
        TuiAction::HistoryPrevious,
        &key(KeyCode::Char('r'), KeyModifiers::CONTROL)
    ));
    // The id-keyed view carries the same override.
    assert!(kb.matches_id(
        "tui.input.submit",
        &key(KeyCode::Char('n'), KeyModifiers::CONTROL)
    ));
}

/// The canonical `tui.*` id wins over the legacy camelCase name when both are
/// present, regardless of entry order.
#[test]
fn canonical_id_wins_over_legacy_name() {
    for settings in [
        serde_json::json!({ "submit": "ctrl+o", "tui.input.submit": "ctrl+n" }),
        serde_json::json!({ "tui.input.submit": "ctrl+n", "submit": "ctrl+o" }),
    ] {
        let kb = Keybindings::from_settings(Some(&settings)).expect("valid object");
        assert!(kb.matches(
            TuiAction::Submit,
            &key(KeyCode::Char('n'), KeyModifiers::CONTROL)
        ));
        assert!(!kb.matches(
            TuiAction::Submit,
            &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
        ));
    }
}

/// Overrides under ids pir does not own a behavior for are ignored; overrides
/// under registered ids without a wired action are still queryable per id.
#[test]
fn unowned_ids_ignored_and_unwired_ids_queryable_per_id() {
    let settings = serde_json::json!({
        "tui.editor.yank": "ctrl+y",
        "tui.altScreen.top": "ctrl+home",
        "tui.editor.cursorLeft": "ctrl+q",
        "tui.select.confirm": "ctrl+enter"
    });
    let kb = Keybindings::from_settings(Some(&settings)).expect("valid object");
    // yank IS registered now (pir owns the kill-ring behavior): the override
    // is stored per id on top of the upstream default.
    assert!(tui_keybinding_definition("tui.editor.yank").is_some());
    assert!(kb.matches_id(
        "tui.editor.yank",
        &key(KeyCode::Char('y'), KeyModifiers::CONTROL)
    ));
    assert!(kb.keys_for_id("tui.altScreen.top").is_none());
    // Registered but unwired: stored per id, no action is disturbed.
    let ctrl_q = key(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(kb.matches_id("tui.editor.cursorLeft", &ctrl_q));
    assert!(!kb.matches_id(
        "tui.editor.cursorLeft",
        &key(KeyCode::Left, KeyModifiers::NONE)
    ));
    assert!(kb.matches_id(
        "tui.select.confirm",
        &key(KeyCode::Enter, KeyModifiers::CONTROL)
    ));
    for action in TuiAction::all() {
        // On Windows FollowUp's upstream default IS ctrl+q, so it is bound
        // by default and the per-id override does not disturb it.
        if pir::config::keybindings::use_windows_keybindings() && *action == TuiAction::FollowUp {
            continue;
        }
        assert!(
            !kb.matches(*action, &ctrl_q),
            "{} must stay unbound",
            action.name()
        );
    }
}

/// Same key-sequence guarantee as the suites contract: with no settings and
/// with an untouched settings object, the previously hardcoded behavior holds
/// (submit=enter, newline=shift+enter, history=alt+up/down, …).
#[test]
fn previous_hardcoded_behavior_preserved() {
    for kb in [
        Keybindings::new(),
        Keybindings::from_settings(Some(&serde_json::json!({
            "submit": ["ctrl+enter", "enter"],
            "nonexistent": "f1"
        })))
        .expect("valid object"),
    ] {
        assert!(kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!kb.matches(TuiAction::Submit, &key(KeyCode::Enter, KeyModifiers::SHIFT)));
        assert!(kb.matches(
            TuiAction::Newline,
            &key(KeyCode::Enter, KeyModifiers::SHIFT)
        ));
        // `app.message.followUp` resolves to ctrl+q on Windows (upstream
        // windowsKeybindings), alt+enter elsewhere.
        if pir::config::keybindings::use_windows_keybindings() {
            assert!(kb.matches(
                TuiAction::FollowUp,
                &key(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ));
        } else {
            assert!(kb.matches(TuiAction::FollowUp, &key(KeyCode::Enter, KeyModifiers::ALT)));
        }
        assert!(!kb.matches(TuiAction::Newline, &key(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(kb.matches(TuiAction::Abort, &key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(kb.matches(
            TuiAction::Clear,
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
        assert!(kb.matches(
            TuiAction::Exit,
            &key(KeyCode::Char('d'), KeyModifiers::CONTROL)
        ));
        assert!(kb.matches(
            TuiAction::ThinkingCycle,
            &key(KeyCode::Tab, KeyModifiers::SHIFT)
        ));
        assert!(kb.matches(
            TuiAction::ModelSelector,
            &key(KeyCode::Char('l'), KeyModifiers::CONTROL)
        ));
        assert!(kb.matches(
            TuiAction::HistoryPrevious,
            &key(KeyCode::Up, KeyModifiers::ALT)
        ));
        assert!(kb.matches(
            TuiAction::HistoryNext,
            &key(KeyCode::Down, KeyModifiers::ALT)
        ));
        // `app.clipboard.pasteImage` resolves to alt+v on Windows (upstream
        // windowsKeybindings), ctrl+v elsewhere.
        if pir::config::keybindings::use_windows_keybindings() {
            assert!(kb.matches(
                TuiAction::Paste,
                &key(KeyCode::Char('v'), KeyModifiers::ALT)
            ));
        } else {
            assert!(kb.matches(
                TuiAction::Paste,
                &key(KeyCode::Char('v'), KeyModifiers::CONTROL)
            ));
        }
    }
}

/// The registry must not leak colliding default chords into the action-level
/// map: `ctrl+up`/`ctrl+down` bind no action (suites scroll-actions contract).
#[test]
fn registry_does_not_bind_scrolling_chords_to_actions() {
    let kb = Keybindings::new();
    let ctrl_up = key(KeyCode::Up, KeyModifiers::CONTROL);
    let ctrl_down = key(KeyCode::Down, KeyModifiers::CONTROL);
    for action in TuiAction::all() {
        assert!(
            !kb.matches(*action, &ctrl_up) && !kb.matches(*action, &ctrl_down),
            "{} unexpectedly bound to a scroll chord",
            action.name()
        );
    }
}

fn modifiers_of(combo: &pir::tui::keybindings::KeyCombo) -> KeyModifiers {
    let mut modifiers = KeyModifiers::NONE;
    if combo.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if combo.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if combo.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    modifiers
}
