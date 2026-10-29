//! Feature-parity wave tests for the editor + autocomplete port
//! (`src/tui/editor.rs`, `src/tui/autocomplete.rs`, `src/tui/terminal.rs`):
//!
//! - large-paste folding (`[paste #N …]` markers, editor.ts:1298-1326,
//!   1090-1105, 1395-1419)
//! - path-paste space prefix (editor.ts:1290-1296)
//! - word navigation semantics (word-navigation.ts via an Intl word
//!   segmentation approximation)
//! - sticky preferred visual column + wrap-aware vertical moves
//!   (editor.ts:1479-1612)
//! - visual-line history gating flags (editor.ts:926-949)
//! - the backslash-Enter submit gate (editor.ts:1353-1362)
//! - autocomplete trigger model: Tab force + single auto-apply, slash
//!   first-line restriction, `skill:` two-pass ordering, `#` trigger parity
//!   (autocomplete.ts:298-299, 338-377, 849-860)

use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::tui::autocomplete::{
    CombinedAutocompleteProvider, SlashCommand, DEFAULT_TRIGGER_CHARACTERS,
};
use pir::tui::editor::{Editor, EditorState};
use pir::tui::keybindings::Keybindings;
use pir::tui::terminal::should_submit_on_backslash_enter;

// ---------------------------------------------------------------------------
// Fix 1: large-paste folding
// ---------------------------------------------------------------------------

const PASTE_A: &str =
    "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo";
const PASTE_B: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11";

/// A paste above the fold thresholds (11 lines) becomes a marker; the full
/// content is only reachable through [`Editor::expanded_text`] and submit.
#[test]
fn large_paste_folds_into_marker_and_expands_on_submit() {
    let editor = Editor::new();
    editor.set_text("before ");
    let cleaned = editor.paste_text(PASTE_A);
    assert_eq!(cleaned, PASTE_A, "the cleaned paste is still reported");
    assert_eq!(editor.get_text(), "before [paste #1 +11 lines]");
    assert_eq!(editor.expanded_text(), format!("before {PASTE_A}"));
    // Submit expands all markers back to the full text (upstream
    // `submitValue` → `expandPasteMarkers`).
    assert_eq!(
        editor.submit().as_deref(),
        Some(format!("before {PASTE_A}").as_str())
    );
    // The registry is cleared on submit.
    assert_eq!(editor.expanded_text(), "");
}

/// A single-line paste above the character threshold folds with the
/// `chars` marker form.
#[test]
fn large_single_line_paste_folds_by_chars() {
    let editor = Editor::new();
    let body = "x".repeat(1200);
    editor.paste_text(&body);
    let text = editor.get_text();
    assert_eq!(text, format!("[paste #1 {} chars]", body.chars().count()));
    assert_eq!(editor.expanded_text(), body);
}

/// A paste under both thresholds is inserted verbatim.
#[test]
fn small_paste_is_not_folded() {
    let editor = Editor::new();
    let body = (0..9)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.paste_text(&body);
    assert_eq!(editor.get_text(), body);
    assert_eq!(editor.expanded_text(), body);
}

/// Backspace at the end of a marker deletes the whole marker atomically.
#[test]
fn backspace_into_marker_deletes_whole_marker() {
    let editor = Editor::new();
    editor.paste_text(PASTE_A); // [paste #1 +11 lines]
    editor.insert_text(" tail");
    assert_eq!(editor.get_text(), "[paste #1 +11 lines] tail");
    for _ in 0..5 {
        editor.backspace(); // eats " tail"
    }
    assert_eq!(editor.get_text(), "[paste #1 +11 lines]");
    editor.backspace();
    assert_eq!(editor.get_text(), "", "the marker is one atomic unit");
    assert_eq!(editor.expanded_text(), "");
    // The freed id is reused for the next large paste (upstream decrements
    // `pasteCounter`).
    editor.paste_text(PASTE_A);
    assert_eq!(editor.get_text(), "[paste #1 +11 lines]");
}

/// Backspacing into the FIRST of two markers renumbers the second
/// (`[paste #2 …]` → `[paste #1 …]`) while keeping its expansion intact.
#[test]
fn backspace_into_first_marker_renumbers_the_second() {
    let editor = Editor::new();
    editor.paste_text(PASTE_B); // [paste #1 +11 lines]
    editor.insert_text(" ");
    editor.paste_text(PASTE_A); // [paste #2 +11 lines]
    assert_eq!(
        editor.get_text(),
        "[paste #1 +11 lines] [paste #2 +11 lines]"
    );
    // Cursor is after the #2 marker: step left over it atomically, then
    // over the space, landing at the end of marker #1.
    editor.move_cursor_left();
    assert_eq!(editor.state_snapshot().cursor_col, 21);
    editor.move_cursor_left();
    assert_eq!(editor.state_snapshot().cursor_col, 20);
    editor.backspace();
    assert_eq!(
        editor.get_text(),
        " [paste #1 +11 lines]",
        "deleting marker #1 renumbers the following #2 down to #1"
    );
    assert_eq!(editor.expanded_text(), format!(" {PASTE_A}"));
}

/// Undo restores both the marker text and its registry entry.
#[test]
fn undo_restores_deleted_marker_and_registry() {
    let editor = Editor::new();
    editor.paste_text(PASTE_A);
    editor.backspace();
    assert_eq!(editor.get_text(), "");
    editor.undo();
    assert_eq!(editor.get_text(), "[paste #1 +11 lines]");
    assert_eq!(editor.expanded_text(), PASTE_A);
}

// ---------------------------------------------------------------------------
// Fix 2: paste space-prefix
// ---------------------------------------------------------------------------

#[test]
fn path_paste_after_word_char_gets_leading_space() {
    let editor = Editor::new();
    editor.set_text("run"); // cursor at end (col 3)
    editor.paste_text("/usr/bin/x");
    assert_eq!(editor.get_text(), "run /usr/bin/x");

    let editor = Editor::new();
    editor.set_text("see"); // cursor at end
    editor.paste_text("~/notes.md");
    assert_eq!(editor.get_text(), "see ~/notes.md");

    let editor = Editor::new();
    editor.set_text("cfg"); // cursor at end
    editor.paste_text("./local.conf");
    assert_eq!(editor.get_text(), "cfg ./local.conf");
}

#[test]
fn paste_after_space_or_non_word_char_has_no_leading_space() {
    // After a space: unchanged.
    let editor = Editor::new();
    editor.set_text("run ");
    editor.paste_text("/usr/bin/x");
    assert_eq!(editor.get_text(), "run /usr/bin/x");
    // Not path-like: unchanged.
    let editor = Editor::new();
    editor.set_text("run");
    editor.paste_text("fast");
    assert_eq!(editor.get_text(), "runfast");
    // Empty editor (cursor at col 0): unchanged.
    let editor = Editor::new();
    editor.paste_text("/usr/bin/x");
    assert_eq!(editor.get_text(), "/usr/bin/x");
}

// ---------------------------------------------------------------------------
// Fix 3: word navigation semantics (word-navigation.ts)
// ---------------------------------------------------------------------------

fn cursor_col(editor: &Editor) -> usize {
    editor.state_snapshot().cursor_col
}

/// Upstream `findWordBackward` table: dotted / coloned / punctuated words
/// (the upstream `word-navigation.test.ts` expectations).
#[test]
fn word_left_matches_upstream_segmentation() {
    // "foo.bar": the word-like segment "foo.bar" stops after its last
    // embedded ASCII punctuation character.
    let editor = Editor::new();
    editor.set_text("foo.bar");
    editor.move_word_left();
    assert_eq!(cursor_col(&editor), 4);
    editor.move_word_left();
    assert_eq!(cursor_col(&editor), 3);
    editor.move_word_left();
    assert_eq!(cursor_col(&editor), 0);

    // "foo:bar" behaves identically.
    let editor = Editor::new();
    editor.set_text("foo:bar");
    for expected in [4, 3, 0] {
        editor.move_word_left();
        assert_eq!(cursor_col(&editor), expected);
    }

    // "foo...bar": punctuation runs are their own segments.
    let editor = Editor::new();
    editor.set_text("foo...bar");
    for expected in [6, 3, 0] {
        editor.move_word_left();
        assert_eq!(cursor_col(&editor), expected);
    }

    // "path/to/file".
    let editor = Editor::new();
    editor.set_text("path/to/file");
    for expected in [8, 7, 5, 4, 0] {
        editor.move_word_left();
        assert_eq!(cursor_col(&editor), expected);
    }

    // "hello world": plain words.
    let editor = Editor::new();
    editor.set_text("hello world");
    for expected in [6, 0] {
        editor.move_word_left();
        assert_eq!(cursor_col(&editor), expected);
    }
}

/// Apostrophes stay inside word segments (upstream MidNumLet) but the ASCII
/// punctuation boundary refinement still splits the move.
#[test]
fn word_left_on_apostrophes_and_hyphens() {
    // "can't": one word-like segment; backward stops after the apostrophe.
    let editor = Editor::new();
    editor.set_text("can't");
    for expected in [4, 3, 0] {
        editor.move_word_left();
        assert_eq!(cursor_col(&editor), expected);
    }
    // "e-mail": the hyphen splits (upstream segments "e", "-", "mail").
    let editor = Editor::new();
    editor.set_text("e-mail");
    for expected in [2, 1, 0] {
        editor.move_word_left();
        assert_eq!(cursor_col(&editor), expected);
    }
}

/// Upstream `findWordForward` table.
#[test]
fn word_right_matches_upstream_segmentation() {
    let editor = Editor::new();
    editor.set_text("foo.bar");
    editor.move_cursor_home();
    for expected in [3, 4, 7] {
        editor.move_word_right();
        assert_eq!(cursor_col(&editor), expected);
    }

    let editor = Editor::new();
    editor.set_text("path/to/file");
    editor.move_cursor_home();
    for expected in [4, 5, 7, 8, 12] {
        editor.move_word_right();
        assert_eq!(cursor_col(&editor), expected);
    }

    let editor = Editor::new();
    editor.set_text("foo...bar");
    editor.move_cursor_home();
    for expected in [3, 6, 9] {
        editor.move_word_right();
        assert_eq!(cursor_col(&editor), expected);
    }
}

/// Delete-word follows the same segmentation.
#[test]
fn delete_word_backward_follows_segmentation() {
    let editor = Editor::new();
    editor.set_text("foo.bar");
    editor.delete_word_backward();
    assert_eq!(editor.get_text(), "foo.");
    editor.delete_word_backward();
    assert_eq!(editor.get_text(), "foo");
    editor.delete_word_backward();
    assert_eq!(editor.get_text(), "");
}

// ---------------------------------------------------------------------------
// Fix 4: sticky preferred visual column + wrap-aware vertical moves
// ---------------------------------------------------------------------------

const WRAPPED: &str = "0123456789abcdefghij";
/// Render at width 12: padding_x 1 → layout width 10, so `WRAPPED` (20
/// chars) wraps into two 10-char visual lines and "xy" is the third.
fn wrap_editor() -> Editor {
    let editor = Editor::new();
    editor.set_text(&format!("{WRAPPED}\nxy"));
    editor.render_with_cursor_rows(12, 10);
    editor
}

/// A horizontal cursor position mid-line is preserved across a wrap
/// boundary (sticky column starts from the current visual column).
#[test]
fn vertical_move_keeps_visual_column_across_wrap() {
    let editor = wrap_editor();
    // Cursor on visual line 0, column 3.
    editor.apply_state(vec![WRAPPED.to_string(), "xy".to_string()], 0, 3);
    editor.move_cursor_down();
    let state = editor.state_snapshot();
    assert_eq!(
        state.cursor_line, 0,
        "second visual line of the same logical line"
    );
    assert_eq!(
        state.cursor_col, 13,
        "visual column 3 kept on the second chunk"
    );
    editor.move_cursor_down();
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 1);
    assert_eq!(state.cursor_col, 2, "clamped to the shorter target line");
    editor.move_cursor_up();
    let state = editor.state_snapshot();
    assert_eq!(
        state.cursor_col, 13,
        "the clamped column became the sticky column"
    );
}

/// The sticky preferred column is set by a horizontal move at the editor's
/// end and honored by the next vertical move (upstream decision table
/// cases 3-5).
#[test]
fn sticky_preferred_column_set_by_horizontal_move() {
    let editor = wrap_editor();
    // Cursor at the very end of the editor (line 1, col 2).
    editor.apply_state(vec![WRAPPED.to_string(), "xy".to_string()], 1, 2);
    // Right at the end: no motion, but the visual column is remembered.
    editor.move_cursor_right();
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (1, 2));
    editor.move_cursor_up();
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 0);
    assert_eq!(
        state.cursor_col, 12,
        "preferred visual column 2 on the second chunk"
    );
}

// ---------------------------------------------------------------------------
// Fix 5: visual-line history gating
// ---------------------------------------------------------------------------

#[test]
fn visual_line_flags_across_wrapped_lines() {
    let editor = wrap_editor();
    // Cursor at the end (line 1): last visual line only.
    assert!(editor.is_on_last_visual_line());
    assert!(!editor.is_on_first_visual_line());
    // First visual line.
    editor.apply_state(vec![WRAPPED.to_string(), "xy".to_string()], 0, 3);
    assert!(editor.is_on_first_visual_line());
    assert!(!editor.is_on_last_visual_line());
    // Second (wrapped) visual line of logical line 0: neither.
    editor.apply_state(vec![WRAPPED.to_string(), "xy".to_string()], 0, 15);
    assert!(!editor.is_on_first_visual_line());
    assert!(!editor.is_on_last_visual_line());
}

/// Up/down history browsing still works through the visual gates on a
/// single-visual-line editor.
#[test]
fn history_browse_uses_visual_gates() {
    let editor = Editor::new();
    editor.set_text("older");
    assert_eq!(editor.submit().as_deref(), Some("older"));
    editor.set_text("newer");
    assert_eq!(editor.submit().as_deref(), Some("newer"));
    editor.set_text("draft");
    // On the first (and only) visual line with col 0... the draft cursor is
    // at the END, so Up moves within the line: place the cursor at col 0
    // like the terminal gate does before browsing.
    editor.move_cursor_home();
    editor.history_navigate(-1);
    assert_eq!(editor.get_text(), "newer");
    assert!(editor.is_browsing_history());
    editor.history_navigate(1);
    editor.history_navigate(1);
    assert_eq!(editor.get_text(), "draft");
}

// ---------------------------------------------------------------------------
// Fix 6: backslash-Enter submit gate
// ---------------------------------------------------------------------------

fn enter_key(shift: bool) -> KeyEvent {
    let modifiers = if shift {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::empty()
    };
    KeyEvent::new(KeyCode::Enter, modifiers)
}

fn backslash_state() -> EditorState {
    EditorState {
        lines: vec!["run\\".to_string()],
        cursor_line: 0,
        cursor_col: 4,
    }
}

/// The workaround only fires when shift+enter is actually bound to submit
/// (upstream `hasShiftEnter` via the `tui.input.submit` registry id).
#[test]
fn backslash_gate_off_without_shift_enter_submit_binding() {
    let defaults = Keybindings::new();
    assert!(
        !should_submit_on_backslash_enter(&defaults, &enter_key(false), &backslash_state()),
        "default bindings (submit = enter only) keep the gate off"
    );
}

#[test]
fn backslash_gate_on_when_shift_enter_bound_to_submit() {
    let bindings = Keybindings::from_settings(Some(&serde_json::json!({
        "tui.input.submit": ["enter", "shift+enter"]
    })))
    .expect("settings parse");

    assert!(should_submit_on_backslash_enter(
        &bindings,
        &enter_key(false),
        &backslash_state()
    ));
    // A real shift+enter keypress is NOT a plain enter (upstream
    // `matchesKey(data, "enter")` with modifier 0).
    assert!(!should_submit_on_backslash_enter(
        &bindings,
        &enter_key(true),
        &backslash_state()
    ));
    // No backslash before the cursor → no workaround.
    let plain = EditorState {
        lines: vec!["run".to_string()],
        cursor_line: 0,
        cursor_col: 3,
    };
    assert!(!should_submit_on_backslash_enter(
        &bindings,
        &enter_key(false),
        &plain
    ));
    // Ctrl+J sends the LF shape non-kitty terminals map to plain enter.
    let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert!(should_submit_on_backslash_enter(
        &bindings,
        &ctrl_j,
        &backslash_state()
    ));
}

// ---------------------------------------------------------------------------
// Fix 7: autocomplete trigger model
// ---------------------------------------------------------------------------

fn skill_provider() -> CombinedAutocompleteProvider {
    CombinedAutocompleteProvider::new(
        PathBuf::from("."),
        vec![
            SlashCommand::new("model", "Select a model"),
            SlashCommand::new("skill:lab", "Lab skill"),
            SlashCommand::new("skill:bbb", "Bbb skill"),
        ],
    )
}

/// Tab-forced file completion is gated by `shouldTriggerFileCompletion`
/// (upstream `autocomplete.ts:849-860`): a bare slash command at line start
/// is not a file completion target.
#[test]
fn tab_force_gated_by_should_trigger_file_completion() {
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let lines = vec!["/mo".to_string()];
    assert!(
        !provider.should_trigger_file_completion(&lines, 0, 3),
        "bare slash command blocks forced file completion"
    );
    let lines = vec!["say /x".to_string()];
    assert!(provider.should_trigger_file_completion(&lines, 0, 6));
    let lines = vec!["/cmd ".to_string()];
    assert!(
        !provider.should_trigger_file_completion(&lines, 0, 5),
        "the trailing space trims away: still a bare slash command (upstream parity)"
    );
}

/// The slash menu is restricted to the FIRST editor line (upstream
/// `isSlashMenuAllowed`, `editor.ts:2189-2204`).
#[test]
fn slash_menu_restricted_to_first_editor_line() {
    let provider = skill_provider();
    let lines = vec!["first line".to_string(), "/lb".to_string()];
    assert!(
        provider.suggestions(&lines, 1, 3, false).is_none(),
        "slash completion on line 1 is suppressed"
    );
    // Line 0 still completes.
    let lines = vec!["/lb".to_string()];
    assert!(provider.suggestions(&lines, 0, 3, false).is_some());
}

/// `skill:` commands complete by bare name first, then by full name
/// (upstream two-pass ordering, `autocomplete.ts:338-377`).
#[test]
fn skill_bare_name_two_pass_ordering() {
    let provider = skill_provider();
    // Query "lb": "skill:lab" matches by bare name ("lab"); "skill:bbb"
    // only by its full name. The bare-name match comes first regardless of
    // fuzzy scores.
    let lines = vec!["/lb".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 3, false)
        .expect("skill suggestions");
    let values: Vec<&str> = suggestions
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert_eq!(values, vec!["skill:lab", "skill:bbb"]);

    // A query that only matches full names still surfaces them.
    let lines = vec!["/skill:b".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 8, false)
        .expect("full-name suggestions");
    let values: Vec<&str> = suggestions
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert!(values.contains(&"skill:lab"));
    assert!(values.contains(&"skill:bbb"));
}

/// The `#` trigger character (upstream `DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS`)
/// never opens the built-in popup: the combined provider serves no
/// suggestions for `#` tokens (only extension wrapper providers do).
#[test]
fn hash_trigger_yields_no_builtin_suggestions() {
    assert!(DEFAULT_TRIGGER_CHARACTERS.contains(&'#'));
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let lines = vec!["#tag".to_string()];
    assert!(provider.suggestions(&lines, 0, 4, false).is_none());
    let lines = vec!["#".to_string()];
    assert!(provider.suggestions(&lines, 0, 1, false).is_none());
}

/// Forced (Tab) completion from an arbitrary token still extracts the token
/// as the path prefix (upstream `extractPathPrefix` force path).
#[test]
fn tab_force_completes_plain_tokens() {
    let base = std::env::temp_dir().join("pir-tab-force-token");
    let src = base.join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("main.rs"), "fn main() {}").expect("write");
    let provider = CombinedAutocompleteProvider::new(base.clone(), Vec::new());
    let lines = vec!["sr".to_string()];
    let suggestions = provider
        .suggestions(&lines, 0, 2, true)
        .expect("forced suggestions");
    assert!(
        suggestions
            .items
            .iter()
            .any(|item| item.value.starts_with("src")),
        "forced Tab lists the matching entry: {:?}",
        suggestions.items
    );
    // Natural trigger without a path-like token: nothing.
    assert!(provider.suggestions(&lines, 0, 2, false).is_none());
    std::fs::remove_dir_all(&base).ok();
}

/// Resource-provider (skill command) registration merges into the combined
/// provider and unregisters cleanly.
#[test]
fn resource_provider_registration_merges_skill_commands() {
    use pir::tui::autocomplete::{register_resource_command_provider, CommandProvider};

    struct Skills;
    impl CommandProvider for Skills {
        fn commands(&self) -> Vec<SlashCommand> {
            vec![SlashCommand::new("skill:parity", "From the parity wave")]
        }
    }
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    register_resource_command_provider(Some(Arc::new(Skills)));
    let lines = vec!["/skill:pa".to_string()];
    let suggestions = provider.suggestions(&lines, 0, 9, false).expect("merged");
    assert!(
        suggestions
            .items
            .iter()
            .any(|item| item.value == "skill:parity"),
        "registered skill command completes"
    );
    register_resource_command_provider(None);
}
