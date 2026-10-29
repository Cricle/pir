//! W11-C TUI gap suite: the five gaps PLAN §0e listed as OPEN.
//!
//! 1. **Rendering hot paths (F2 / F5).** `ChatContainer::set_entries` diffs
//!    TWO-SIDED (a matching prefix AND suffix are retained, only the changed
//!    middle band is re-committed) and patches a changed tool block IN PLACE
//!    through the component's public setters. The tests pin the work per
//!    container ([`ChatContainer::last_diff`]) — the process-wide
//!    `EntryDiffWork` counters are shared by every container in this binary —
//!    and prove the patched tree renders BYTE-IDENTICAL rows to a cold
//!    rebuild, including the pending-phase edge that must fall back to a
//!    rebuild.
//! 2. **Main-screen mouse.** The editor click hit-test (grapheme-aware), the
//!    published [`DockMouseLayout`] regions (editor / overlay / autocomplete
//!    popup) and the autocomplete popup's own item-row click.
//! 3. **The six `tui.altScreen.*` viewport ids** (pageUp/pageDown,
//!    halfPageUp/halfPageDown, lineUp/lineDown): routed, user-keybinding
//!    aware (a settings override — including an empty list — REPLACES the
//!    default), in upstream's key order, with upstream's half-page formula.
//! 4. **Settings menu gaps**: per-model thinking override, automatic
//!    light/dark theme, type-to-search, and the fullscreen wheel-scroll
//!    numeric list (plus its click row).
//! 5. **`terminal.showImages` / `terminal.imageWidthCells`** are live
//!    settings: `ImageSettings::from_terminal` resolves them and the renderer
//!    honors both in a DEFAULT build (`image_preview` always compiles; its
//!    halfblocks renderer rides `image-protocol`, which `full` includes).
//!
//! Every test here fails against the pre-W11-C code. Rendering tests take the
//! `g_tui`-binary theme lock first (HANDOFF §5).

use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::tui::altscreen_search::{route_alt_screen_key, AltKeyRoute, AltScreenState};
use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::autocomplete::{AutocompleteItem, AutocompletePopup, AutocompleteSuggestions};
use pir::tui::core::{Component, DockMouseLayout, MouseRegion, Terminal, Text, TuiCore};
use pir::tui::editor::Editor;
use pir::tui::image_preview::{
    render_image_with_settings, ImageSettings, DEFAULT_IMAGE_WIDTH_CELLS,
};
use pir::tui::keybindings::Keybindings;
use pir::tui::settings_menu::{
    item_action, settings_items, MenuKey, MenuOutcome, ModelThinkingOption, SettingsAction,
    SettingsMenu, SettingsMenuContext,
};
use pir::tui::theme::set_theme;

/// The `g_tui`-binary-wide theme lock (sibling suites swap the process-global
/// palette in parallel).
fn pin_theme() -> std::sync::MutexGuard<'static, ()> {
    let guard = super::tui_system_theme::lock();
    set_theme("dark");
    guard
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

// ---------------------------------------------------------------------------
// 1a. F2 — the two-sided entry diff
// ---------------------------------------------------------------------------

fn notice(text: &str) -> ChatEntry {
    ChatEntry::Notice {
        kind: pir::tui::app::NoticeKind::Status,
        text: text.to_string(),
    }
}

fn tool_entry(args: &Arc<serde_json::Value>, output: &str, pending: bool) -> ChatEntry {
    ChatEntry::ToolExecution {
        call_id: Some("call-42".to_string()),
        tool_name: "bash".to_string(),
        args: Arc::clone(args),
        output: Arc::from(output),
        is_error: false,
        duration_ms: Some(12),
        pending,
        details: None,
        started_at: None,
        preview: None,
        images: Arc::new(Vec::new()),
    }
}

/// Land a result on the tool block AT ITS SLOT, exactly like the event loop
/// (`entries[index]` field writes): the entry's owned buffers keep their
/// addresses, which is what the identity tag compares.
fn land_result(entry: &mut ChatEntry, output: &str) {
    match entry {
        ChatEntry::ToolExecution {
            output: slot,
            pending,
            ..
        } => {
            *slot = Arc::from(output);
            *pending = false;
        }
        other => panic!("expected a tool entry, got {other:?}"),
    }
}

/// Flip the block back to the pending phase in place (a background completion
/// re-opening the block, the direction the component's setters cannot express).
fn reopen_pending(entry: &mut ChatEntry) {
    match entry {
        ChatEntry::ToolExecution {
            output: slot,
            pending,
            ..
        } => {
            *slot = Arc::from("");
            *pending = true;
        }
        other => panic!("expected a tool entry, got {other:?}"),
    }
}

/// A change in the MIDDLE of the transcript retains the matching prefix AND
/// the matching suffix; the pre-F2 longest-prefix diff rebuilt the whole tail
/// (so `suffix_reused` was always 0).
///
/// The entries are mutated IN PLACE: an entry's identity tag is its owned
/// buffer address, exactly like the live transcript (a fresh `Vec` of fresh
/// `String`s would be a full rebuild by construction).
#[test]
fn a_middle_change_retains_both_sides_and_rebuilds_only_the_band() {
    let _guard = pin_theme();
    let mut chat = ChatContainer::new();
    let mut entries = vec![
        notice("first line"),
        notice("second line"),
        notice("third line"),
    ];
    chat.set_entries(&entries);
    // A cold container reports the full rebuild of its first commit.
    assert_eq!(chat.last_diff().prefix_kept, 0);
    assert_eq!(chat.last_diff().suffix_reused, 0);
    assert_eq!(chat.last_diff().entries_rebuilt, 3);

    // Change ONLY the middle entry.
    entries[1] = notice("second line, edited");
    chat.set_entries(&entries);
    let diff = chat.last_diff();
    assert_eq!(diff.prefix_kept, 1, "prefix entry must be retained");
    assert_eq!(diff.suffix_reused, 1, "F2: the suffix must be reused");
    assert_eq!(diff.entries_rebuilt, 1, "only the middle band rebuilds");
    assert_eq!(diff.entries_patched, 0);

    // The rendered frame is exactly the fresh one.
    let mut fresh = ChatContainer::new();
    fresh.set_entries(&entries);
    assert_eq!(chat.render(60), fresh.render(60));
}

/// Inserting BEFORE a stable tail keeps that tail's components (the suffix is
/// reused) and the frame stays byte-identical to a cold rebuild — the F2
/// contract that a result landing on an old block no longer rebuilds every
/// later entry.
#[test]
fn an_insert_before_a_stable_tail_retains_the_suffix() {
    let _guard = pin_theme();
    let args = Arc::new(serde_json::json!({ "command": "sleep 1" }));
    let tool = tool_entry(&args, "done", false);
    let mut chat = ChatContainer::new();
    let mut entries = vec![notice("head"), tool, notice("tail")];
    chat.set_entries(&entries);
    assert_eq!(chat.last_diff().entries_rebuilt, 3);

    // Insert one entry between the head and the (unchanged) tool block. The
    // tool block and the tail are the same entries as before.
    entries.insert(1, notice("inserted"));
    chat.set_entries(&entries);
    let diff = chat.last_diff();
    assert_eq!(diff.prefix_kept, 1);
    assert_eq!(
        diff.suffix_reused, 2,
        "the tool block and tail are retained"
    );
    // Only the inserted row is built; the tool block moved but kept its
    // component.
    assert_eq!(diff.entries_rebuilt, 1);

    let mut fresh = ChatContainer::new();
    fresh.set_entries(&entries);
    assert_eq!(chat.render(60), fresh.render(60));
}

// ---------------------------------------------------------------------------
// 1b. F5 — in-place ToolExecution patching
// ---------------------------------------------------------------------------

/// The hot F5 case: a result landing on a pending block patches the retained
/// component (no rebuild) and the patched tree renders byte-identically to a
/// cold rebuild.
#[test]
fn a_result_landing_patches_the_pending_block_in_place() {
    let _guard = pin_theme();
    let args = Arc::new(serde_json::json!({ "command": "echo hello" }));
    let mut chat = ChatContainer::new();
    let mut entries = vec![tool_entry(&args, "", true)];
    chat.set_entries(&entries);
    assert_eq!(chat.last_diff().entries_rebuilt, 1);

    land_result(&mut entries[0], "hello");
    chat.set_entries(&entries);
    let diff = chat.last_diff();
    assert_eq!(diff.entries_patched, 1, "F5: the block is patched in place");
    assert_eq!(diff.entries_rebuilt, 0);

    let mut fresh = ChatContainer::new();
    fresh.set_entries(&entries);
    let patched = chat.render(80);
    assert_eq!(
        patched,
        fresh.render(80),
        "the patched block must render exactly like a rebuilt one"
    );
    assert!(patched.iter().any(|row| row.contains("hello")));
}

/// A SETTLED→pending flip is not patchable: `update_result` only clears the
/// pending flag, so patching would wrongly keep the settled background. The
/// diff must rebuild, and the rebuilt block must render the pending phase
/// exactly (this is the regression the F5 guard exists for).
#[test]
fn a_settled_to_pending_flip_rebuilds_instead_of_patching() {
    let _guard = pin_theme();
    let args = Arc::new(serde_json::json!({ "command": "echo flip" }));
    let mut chat = ChatContainer::new();
    let mut entries = vec![tool_entry(&args, "done", false)];
    chat.set_entries(&entries);
    assert_eq!(chat.last_diff().entries_rebuilt, 1);

    reopen_pending(&mut entries[0]);
    chat.set_entries(&entries);
    let diff = chat.last_diff();
    assert_eq!(diff.entries_patched, 0, "settled→pending must not patch");
    assert_eq!(diff.entries_rebuilt, 1);

    let mut fresh = ChatContainer::new();
    fresh.set_entries(&entries);
    assert_eq!(
        chat.render(80),
        fresh.render(80),
        "a pending block keeps the pending background"
    );
}

// ---------------------------------------------------------------------------
// 2. Main-screen mouse
// ---------------------------------------------------------------------------

/// The editor click hit-test is grapheme-aware and maps content rows only
/// (upstream `Editor.handleMouse`'s click branch).
#[test]
fn an_editor_click_positions_the_cursor_on_a_content_row() {
    let editor = Editor::new();
    editor.set_text("hello\nworld");
    // Border-framed block: `cursor_position_at` takes CONTENT rows.
    assert_eq!(editor.cursor_position_at(40, 3, 0, 4), Some((0, 3)));
    assert_eq!(editor.cursor_position_at(40, 3, 1, 0), Some((1, 0)));
    // A row past the last visual line is not clickable.
    assert_eq!(editor.cursor_position_at(40, 3, 2, 0), None);

    assert!(editor.set_cursor_position(1, 3));
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (1, 3));
}

/// The published dock layout resolves a click to the region the last frame
/// painted: the editor block, the overlay that replaced it, or the
/// autocomplete popup BELOW it.
#[test]
fn dock_mouse_layout_publishes_editor_overlay_and_popup_regions() {
    let layout = DockMouseLayout {
        editor_row: 10,
        editor_height: 5,
        overlay_row: None,
        overlay_height: 0,
        autocomplete_row: Some(15),
        autocomplete_height: 3,
    };
    let regions = layout.regions();
    assert_eq!(regions.len(), 2, "{regions:?}");
    assert!(matches!(
        regions[0],
        MouseRegion::Editor {
            row: 10,
            height: 5,
            ..
        }
    ));
    assert!(matches!(
        regions[1],
        MouseRegion::Autocomplete { row: 15, height: 3 }
    ));
    assert_eq!(layout.region_at(16, 0), Some(regions[1]));
    // Content rows exclude the top and bottom border.
    assert_eq!(layout.editor_content_row(11), Some(0));
    assert_eq!(layout.editor_content_row(10), None);
    assert_eq!(layout.editor_content_row(14), None);

    // An overlay replaces the editor slot entirely (the popup is not painted).
    let overlay = DockMouseLayout {
        overlay_row: Some(10),
        overlay_height: 12,
        ..layout
    };
    let regions = overlay.regions();
    assert_eq!(
        regions,
        vec![MouseRegion::Selector {
            row: 10,
            height: 12
        }]
    );
    assert_eq!(overlay.editor_content_row(11), None);
}

fn popup(count: usize, max_visible: usize) -> AutocompletePopup {
    let items = (0..count)
        .map(|index| AutocompleteItem::new(format!("item{index}"), format!("item{index}")))
        .collect();
    AutocompletePopup::new(AutocompleteSuggestions {
        items,
        prefix: "item".to_string(),
    })
    .with_max_visible(Some(max_visible as u32))
}

/// A click on a popup ITEM row selects it; the trailing `(n/m)` scroll counter
/// is not an item (upstream `SelectList.handleMouse`).
#[test]
fn an_autocomplete_click_selects_the_item_row_only() {
    let mut popup = popup(5, 3);
    assert_eq!(popup.visible_item_rows(), 3);
    // Three item rows + the scroll counter = four painted lines.
    assert_eq!(popup.render_lines(40).len(), 4);

    assert!(popup.click_row(2));
    assert_eq!(popup.selected, 2);
    // The window recenters on the new selection: row 0 is now item index 1.
    assert!(popup.click_row(0));
    assert_eq!(popup.selected, 1);
    // Row 3 is the scroll counter, not an item.
    assert!(!popup.click_row(3));
    assert_eq!(popup.selected, 1);
}

// ---------------------------------------------------------------------------
// 3. The six `tui.altScreen.*` viewport ids
// ---------------------------------------------------------------------------

fn scroll_core(lines: usize, rows: usize) -> TuiCore {
    let terminal = Terminal::new();
    terminal.set_size(20, rows);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=lines)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::new(transcript)));
    core.render();
    core
}

fn alt_state(settings: serde_json::Value) -> AltScreenState {
    let mut state = AltScreenState::default();
    state.set_keybindings(
        Keybindings::from_settings(Some(&settings)).expect("settings must be an object"),
    );
    state
}

/// An explicit empty list is a settings OVERRIDE that turns the id's default
/// off (upstream `rebuild`'s `userKeys === undefined ? defaultKeys :
/// normalizeKeys(userKeys)`). Before W11-C nothing was stored for it, so the
/// upstream default stayed live.
#[test]
fn an_empty_keybinding_list_unbinds_the_alt_screen_default() {
    let kb = Keybindings::from_settings(Some(&serde_json::json!({
        "tui.altScreen.pageUp": [],
    })))
    .expect("settings must be an object");
    assert_eq!(kb.alt_id_combos("tui.altScreen.pageUp"), Some(Vec::new()));

    let mut core = scroll_core(40, 10);
    let mut state = alt_state(serde_json::json!({ "tui.altScreen.pageUp": [] }));
    core.set_scroll_offset(3);
    let page_up = key(KeyCode::PageUp, KeyModifiers::NONE);
    assert!(matches!(
        route_alt_screen_key(&page_up, &mut core, &mut state, 0),
        AltKeyRoute::Passthrough
    ));
    assert_eq!(core.scroll_offset(), 3, "the unbound key must not scroll");
}

/// Without a keybinding map (and with the upstream empty defaults) the
/// line/half-page ids are unbound — they are user-bindable only.
#[test]
fn line_and_half_page_ids_are_unbound_by_default() {
    let mut core = scroll_core(40, 10);
    let mut state = AltScreenState::default();
    for c in ['u', 'd', 'b', 'e'] {
        let plain = key(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(
            matches!(
                route_alt_screen_key(&plain, &mut core, &mut state, 0),
                AltKeyRoute::Passthrough
            ),
            "ctrl+{c} must not be an alt-screen default"
        );
    }
    assert_eq!(state.last_scroll_step(), None);
}

/// A settings override reaches the router: each of the six ids scrolls by the
/// documented delta once bound.
#[test]
fn settings_overrides_drive_every_viewport_id() {
    let mut core = scroll_core(60, 10);
    let mut state = alt_state(serde_json::json!({
        "tui.altScreen.pageUp": "ctrl+g",
        "tui.altScreen.pageDown": "ctrl+h",
        "tui.altScreen.halfPageUp": "ctrl+j",
        "tui.altScreen.halfPageDown": "ctrl+k",
        "tui.altScreen.lineUp": "ctrl+n",
        "tui.altScreen.lineDown": "ctrl+m",
    }));
    let press = |core: &mut TuiCore, state: &mut AltScreenState, c: char| {
        let event = key(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(matches!(
            route_alt_screen_key(&event, core, state, 0),
            AltKeyRoute::Consumed
        ));
        state.last_scroll_step()
    };
    // rows 10, dock 0 → viewport 10, page step 10 - PAGE_SCROLL_OVERLAP(4).
    assert_eq!(press(&mut core, &mut state, 'g'), Some(6));
    assert_eq!(press(&mut core, &mut state, 'h'), Some(-6));
    // Upstream half page = max(1, floor(viewportHeight / 2)) = 5 (NOT the
    // page step halved, which would be 3).
    assert_eq!(press(&mut core, &mut state, 'j'), Some(5));
    assert_eq!(press(&mut core, &mut state, 'k'), Some(-5));
    assert_eq!(press(&mut core, &mut state, 'n'), Some(1));
    assert_eq!(press(&mut core, &mut state, 'm'), Some(-1));
}

/// The router checks the ids in UPSTREAM's order (`tui-alt-screen.ts`):
/// page → half page → line. Binding the same chord to pageUp and halfPageUp
/// must run the PAGE step.
#[test]
fn the_viewport_ids_run_in_upstream_order() {
    let mut core = scroll_core(60, 10);
    let mut state = alt_state(serde_json::json!({
        "tui.altScreen.pageUp": "ctrl+p",
        "tui.altScreen.halfPageUp": "ctrl+p",
    }));
    let event = key(KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert!(matches!(
        route_alt_screen_key(&event, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert_eq!(
        state.last_scroll_step(),
        Some(6),
        "pageUp is checked before halfPageUp"
    );
}

/// The platform-rebound `tui.altScreen.search` is user-keybinding aware: an
/// override REPLACES the default chord instead of adding to it.
#[test]
fn the_search_toggle_honors_a_settings_override() {
    let mut core = scroll_core(30, 10);
    let mut state = alt_state(serde_json::json!({
        "tui.altScreen.search": "ctrl+r",
    }));
    let default_chord = if pir::config::keybindings::use_windows_keybindings() {
        key(KeyCode::Char('f'), KeyModifiers::CONTROL)
    } else {
        key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )
    };
    assert!(
        matches!(
            route_alt_screen_key(&default_chord, &mut core, &mut state, 0),
            AltKeyRoute::Passthrough
        ),
        "the replaced default chord must no longer toggle search"
    );
    assert!(!state.search.active);

    let overridden = key(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(matches!(
        route_alt_screen_key(&overridden, &mut core, &mut state, 0),
        AltKeyRoute::Consumed
    ));
    assert!(state.search.active);
}

// ---------------------------------------------------------------------------
// 4. Settings menu
// ---------------------------------------------------------------------------

fn menu_context() -> SettingsMenuContext {
    SettingsMenuContext {
        current_theme: Some("dark".to_string()),
        available_themes: vec![
            "dark".to_string(),
            "light".to_string(),
            "solarized-dark".to_string(),
        ],
        models: Vec::new(),
        model_thinking_levels: std::collections::BTreeMap::new(),
        default_thinking_level: "medium".to_string(),
    }
}

fn select_row(menu: &SettingsMenu, label: &str, budget: usize) {
    for _ in 0..budget {
        if menu.selected_label().as_deref() == Some(label) {
            return;
        }
        menu.handle_key(MenuKey::Down);
    }
    panic!(
        "row {label:?} not reached; selected {:?}",
        menu.selected_label()
    );
}

/// Type-to-search filters the main list; Backspace widens it again and Esc
/// clears the query before closing.
#[test]
fn type_to_search_filters_the_settings_rows() {
    let settings = pir::config::settings::Settings::default();
    let menu = SettingsMenu::new(&settings, menu_context());
    let all = menu.rows().len();
    assert!(all > 3);

    for c in "wheel".chars() {
        assert!(matches!(
            menu.handle_key(MenuKey::Char(c)),
            MenuOutcome::None
        ));
    }
    assert_eq!(menu.query(), "wheel");
    let filtered = menu.rows();
    assert!(filtered.len() < all, "the query must narrow the list");
    assert_eq!(
        filtered[0].0, "Fullscreen wheel scrolling",
        "the matching row ranks first, got {filtered:?}"
    );

    menu.handle_key(MenuKey::Backspace);
    assert_eq!(menu.query(), "whee");
    menu.handle_key(MenuKey::Esc);
    assert_eq!(menu.query(), "", "Esc clears the query before closing");
    assert_eq!(menu.rows().len(), all);
}

/// The wheel row is the upstream numeric list: `auto`, 1, 2, 3, 5, 10, plus a
/// stored value outside the presets, and it persists the right JSON.
#[test]
fn the_wheel_row_lists_presets_and_splices_the_stored_value() {
    use pir::config::settings::{FullscreenWheelScrollLines, Settings};
    for (stored, expected_index) in [
        (FullscreenWheelScrollLines::Auto, 0usize),
        (FullscreenWheelScrollLines::Lines(5), 4),
    ] {
        let settings = Settings {
            fullscreen_wheel_scroll_lines: Some(stored),
            ..Settings::default()
        };
        let row = settings_items(&settings, &menu_context())
            .into_iter()
            .find(|item| item.id == "fullscreen-wheel-scroll-lines")
            .expect("the wheel row is always available");
        assert_eq!(
            row.values,
            vec!["auto", "1", "2", "3", "5", "10"],
            "upstream preset list"
        );
        assert_eq!(row.current, expected_index);
        let mut cycled = row.clone();
        cycled.current = 5;
        match item_action(&cycled) {
            SettingsAction::Set { id, value } => {
                assert_eq!(id, "fullscreen-wheel-scroll-lines");
                assert_eq!(value, serde_json::json!(10));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    // A hand-edited value outside the presets is shown instead of snapping.
    let settings = Settings {
        fullscreen_wheel_scroll_lines: Some(FullscreenWheelScrollLines::Lines(7)),
        ..Settings::default()
    };
    let row = settings_items(&settings, &menu_context())
        .into_iter()
        .find(|item| item.id == "fullscreen-wheel-scroll-lines")
        .unwrap();
    assert_eq!(row.values.last().map(String::as_str), Some("7"));
    assert_eq!(row.current, row.values.len() - 1);
    // "auto" is the JSON string, a number stays a number.
    assert_eq!(row.payloads[0], serde_json::json!("auto"));
    assert_eq!(row.payloads[1], serde_json::json!(1));
}

/// The per-model thinking submenu writes AND clears one model's override, and
/// the main row's summary follows.
#[test]
fn the_per_model_thinking_submenu_sets_and_clears_an_override() {
    let settings = pir::config::settings::Settings::default();
    let mut context = menu_context();
    context.models = vec![ModelThinkingOption {
        provider: "anthropic".to_string(),
        id: "claude-test".to_string(),
        levels: vec!["off".to_string(), "low".to_string(), "high".to_string()],
    }];
    let menu = SettingsMenu::new(&settings, context);
    select_row(&menu, "Default thinking level per model", 64);
    assert!(matches!(menu.handle_key(MenuKey::Enter), MenuOutcome::None));
    assert_eq!(
        menu.selected_label().as_deref(),
        Some("anthropic/claude-test")
    );
    assert!(matches!(menu.handle_key(MenuKey::Enter), MenuOutcome::None));
    // The level list starts on the first supported level.
    assert_eq!(menu.selected_label().as_deref(), Some("off"));
    menu.handle_key(MenuKey::Down);
    menu.handle_key(MenuKey::Down);
    assert_eq!(menu.selected_label().as_deref(), Some("high"));
    match menu.handle_key(MenuKey::Enter) {
        MenuOutcome::Apply(SettingsAction::SetModelThinking {
            provider,
            model,
            level,
        }) => {
            assert_eq!(provider, "anthropic");
            assert_eq!(model, "claude-test");
            assert_eq!(level.as_deref(), Some("high"));
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // Back on the model list with the override reflected.
    assert_eq!(
        menu.selected_label().as_deref(),
        Some("anthropic/claude-test (override: high)")
    );
    // Esc closes the submenu; the main row's summary moved.
    assert!(matches!(menu.handle_key(MenuKey::Esc), MenuOutcome::None));
    assert!(
        menu.rows().iter().any(|(label, value)| {
            label == "Default thinking level per model" && value == "anthropic/claude-test=high"
        }),
        "{:?}",
        menu.rows()
    );

    // Re-open the levels: the active level is marked and "(clear override)"
    // is offered; choosing it clears the override.
    assert!(matches!(menu.handle_key(MenuKey::Enter), MenuOutcome::None));
    assert!(matches!(menu.handle_key(MenuKey::Enter), MenuOutcome::None));
    assert_eq!(menu.selected_label().as_deref(), Some("✓ high"));
    menu.handle_key(MenuKey::Down);
    assert_eq!(menu.selected_label().as_deref(), Some("(clear override)"));
    match menu.handle_key(MenuKey::Enter) {
        MenuOutcome::Apply(SettingsAction::SetModelThinking { level, .. }) => {
            assert_eq!(level, None, "clearing writes no level");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

/// The automatic light/dark theme: a `light/dark` setting opens the automatic
/// step directly, and Apply writes the pair built from the two pickers.
#[test]
fn the_automatic_theme_submenu_builds_a_light_dark_pair() {
    use pir::config::settings::Settings;
    let settings = Settings {
        theme: Some("light/dark".to_string()),
        ..Settings::default()
    };
    let menu = SettingsMenu::new(&settings, menu_context());
    select_row(&menu, "Theme", 64);
    assert!(matches!(menu.handle_key(MenuKey::Enter), MenuOutcome::None));
    // Upstream opens the AUTOMATIC step for a pair setting.
    assert_eq!(menu.selected_label().as_deref(), Some("Light theme"));
    menu.handle_key(MenuKey::Right);
    menu.handle_key(MenuKey::Down);
    assert_eq!(menu.selected_label().as_deref(), Some("Dark theme"));
    menu.handle_key(MenuKey::Down);
    assert_eq!(menu.selected_label().as_deref(), Some("Apply"));
    match menu.handle_key(MenuKey::Enter) {
        MenuOutcome::Apply(SettingsAction::SetTheme(pair)) => {
            assert_eq!(pair, "solarized-dark/dark");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert!(matches!(menu.handle_key(MenuKey::Esc), MenuOutcome::Close));
}

/// A click selects the painted settings row under the pointer (the header is
/// border + spacer + title + spacer, plus the search line when filtering).
#[test]
fn a_settings_click_selects_the_painted_row() {
    let settings = pir::config::settings::Settings::default();
    let menu = SettingsMenu::new(&settings, menu_context());
    let rows = menu.rows();
    let first = rows[0].0.clone();
    let second = rows[1].0.clone();
    assert_eq!(menu.selected_label().as_deref(), Some(first.as_str()));
    assert!(!menu.click_row(3), "the title area is not a row");
    assert!(menu.click_row(5), "list row 1 is the second item row");
    assert_eq!(menu.selected_label().as_deref(), Some(second.as_str()));
    assert!(menu.click_row(4), "list row 0 is the first item row");
    assert_eq!(menu.selected_label().as_deref(), Some(first.as_str()));
}

// ---------------------------------------------------------------------------
// 5. `terminal.showImages` / `terminal.imageWidthCells`
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-tui-gaps-w11-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A stored `terminal` object resolves into the renderer settings (defaults:
/// 60 cells, images shown).
#[test]
fn image_settings_resolve_from_the_terminal_settings() {
    use pir::config::settings::TerminalSettings;
    let defaults = ImageSettings::from_terminal(None);
    assert_eq!(defaults.width_cells, DEFAULT_IMAGE_WIDTH_CELLS as usize);
    assert!(defaults.show);

    let configured = ImageSettings::from_terminal(Some(&TerminalSettings {
        show_images: Some(false),
        image_width_cells: Some(24),
        ..TerminalSettings::default()
    }));
    assert_eq!(configured.width_cells, 24);
    assert!(!configured.show);
}

/// `terminal.showImages: false` renders the upstream `[Image: …]` text
/// fallback instead of halfblocks — in a DEFAULT build (the module used to be
/// compiled out entirely unless the non-default `image-protocol` feature was
/// on, which is what made the setting dead).
#[test]
fn show_images_false_renders_the_text_fallback() {
    let dir = temp_dir("fallback");
    let path = dir.join("shot.png");
    std::fs::write(&path, tiny_png(8, 8, [255, 0, 0, 255])).expect("write png");

    let hidden = render_image_with_settings(
        &path,
        10,
        ImageSettings {
            width_cells: 60,
            show: false,
        },
    )
    .expect("fallback renders");
    assert_eq!(hidden.len(), 1);
    assert!(hidden[0].starts_with("[Image: "), "{hidden:?}");
    assert!(hidden[0].contains("image/png"));
    assert!(hidden[0].contains("8x8"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `terminal.imageWidthCells` sets the preview's column budget: a wider budget
/// paints more half-block cells. Needs the `image-protocol` renderer (in
/// `full`); a slim TUI build without it renders the text fallback instead.
#[cfg(feature = "image-protocol")]
#[test]
fn image_width_cells_sets_the_preview_width() {
    let dir = temp_dir("width");
    let path = dir.join("wide.png");
    std::fs::write(&path, tiny_png(16, 16, [0, 128, 255, 255])).expect("write png");

    let narrow = render_image_with_settings(
        &path,
        20,
        ImageSettings {
            width_cells: 4,
            show: true,
        },
    )
    .expect("halfblocks render");
    let wide = render_image_with_settings(
        &path,
        20,
        ImageSettings {
            width_cells: 12,
            show: true,
        },
    )
    .expect("halfblocks render");

    let cells = |lines: &[String]| {
        lines
            .iter()
            .map(|line| line.chars().filter(|c| *c == '\u{2580}').count())
            .max()
            .unwrap_or(0)
    };
    assert_eq!(cells(&narrow), 4, "the 4-cell budget caps the preview");
    assert_eq!(cells(&wide), 12, "the 12-cell budget widens it");
    let _ = std::fs::remove_dir_all(&dir);
}

// -- PNG fixture (zlib "stored" blocks; no dev-dependency) -------------------

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for chunk in raw.chunks(u16::MAX as usize) {
        let last = chunk.len() == raw.len();
        out.push(if last { 0x01 } else { 0x00 });
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// A minimal RGBA PNG with every pixel set to `rgba`.
#[allow(clippy::same_item_push)]
fn tiny_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width * height * 4 + height) as usize);
    for _ in 0..height {
        raw.push(0);
        for _ in 0..width {
            raw.extend_from_slice(&rgba);
        }
    }
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut png, b"IEND", &[]);
    png
}
