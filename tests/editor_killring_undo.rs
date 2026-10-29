//! Function-level tests for the editor kill-ring / undo port
//! (`src/tui/editor.rs`, upstream `kill-ring.ts` / `undo-stack.ts` and the
//! wiring in `components/editor.ts:1614-1777, 1996-2132`).
//!
//! Pure editor operations only — no terminal required.

use pir::tui::editor::Editor;

/// Ctrl+U (deleteToLineStart) feeds the kill ring; Ctrl+Y (yank) pastes the
/// newest kill at the cursor. `set_text` places the cursor at the END
/// (upstream `cursorPlacement: "end"`, `editor.ts:1115-1127`), so a bare
/// Ctrl+U kills the whole line.
#[test]
fn kill_to_line_start_then_yank_restores_text() {
    let editor = Editor::new();
    editor.set_text("hello world");
    editor.delete_to_line_start();
    assert_eq!(editor.get_text(), "");
    editor.yank();
    assert_eq!(editor.get_text(), "hello world");
}

/// Ctrl+K (deleteToLineEnd) appends to the ring (forward deletion).
#[test]
fn kill_to_line_end_then_yank() {
    let editor = Editor::new();
    editor.set_text("hello world");
    editor.move_cursor_home();
    editor.delete_to_line_end();
    assert_eq!(editor.get_text(), "");
    editor.yank();
    assert_eq!(editor.get_text(), "hello world");
}

/// Consecutive kills accumulate into ONE ring entry (backward deletion
/// prepends, so the entry reads in original text order).
#[test]
fn consecutive_kills_accumulate_into_single_entry() {
    let editor = Editor::new();
    editor.set_text("one two three");
    editor.move_cursor_home();
    editor.delete_word_forward(); // kills "one "
                                  // From col 0 of " two three" the word-forward scan skips the leading
                                  // whitespace first (upstream `findWordForward`), killing " two".
    editor.delete_word_forward();
    assert_eq!(editor.get_text(), " three");
    editor.yank();
    // Yank INSERTS the accumulated entry "one two" at the cursor (col 0),
    // restoring the full original line.
    assert_eq!(editor.get_text(), "one two three");
}

/// A cursor move between kills breaks accumulation: two separate ring
/// entries, and Alt+Y (yankPop) rotates between them.
#[test]
fn yank_pop_rotates_between_separate_kills() {
    let editor = Editor::new();
    editor.set_text("hello world");
    editor.delete_word_backward(); // kills "world", leaving "hello "
    editor.move_cursor_left(); // breaks the kill chain, col 5
    editor.delete_word_backward(); // kills "hello", leaving " "
    assert_eq!(editor.get_text(), " ");
    editor.yank();
    assert_eq!(editor.get_text(), "hello ");
    editor.yank_pop();
    assert_eq!(editor.get_text(), "world ");
    editor.yank_pop();
    assert_eq!(editor.get_text(), "hello ");
}

/// yankPop only fires immediately after a yank, and only with more than one
/// ring entry (upstream `editor.ts:2013-2015`).
#[test]
fn yank_pop_requires_prior_yank_and_multiple_entries() {
    let editor = Editor::new();
    editor.set_text("abc");
    editor.delete_to_line_start();
    // Single ring entry: yank-pop is a no-op even after a yank.
    editor.yank();
    assert_eq!(editor.get_text(), "abc");
    editor.yank_pop();
    assert_eq!(editor.get_text(), "abc");

    // No prior yank: yank-pop is a no-op.
    editor.move_cursor_end();
    editor.delete_to_line_start(); // second ring entry
    editor.yank_pop();
    assert_eq!(editor.get_text(), "");
}

/// Yank pastes at the cursor; a kill from a middle line yanks back into
/// place.
#[test]
fn yank_restores_text_at_cursor() {
    let editor = Editor::new();
    editor.set_text("alpha\nbeta");
    editor.move_cursor_home(); // line 1, col 0
    editor.delete_to_line_end(); // kills "beta" on line 1
    assert_eq!(editor.get_text(), "alpha\n");
    editor.backspace(); // joins: "alpha", cursor col 5
    assert_eq!(editor.get_text(), "alpha");
    editor.yank();
    assert_eq!(editor.get_text(), "alphabeta");
}

/// Undo restores the pre-deletion state after a kill.
#[test]
fn undo_after_kill() {
    let editor = Editor::new();
    editor.set_text("hello world");
    editor.delete_to_line_start();
    // Cursor is at the end after set_text, so Ctrl+U kills the whole line.
    assert_eq!(editor.get_text(), "");
    editor.undo();
    assert_eq!(editor.get_text(), "hello world");
}

/// Fish-style undo coalescing (upstream `insertCharacter`,
/// `editor.ts:1201-1214`): consecutive word characters are one undo unit;
/// a space captures the state before itself, so one undo removes the
/// space plus the word typed after it.
#[test]
fn undo_coalesces_word_characters_and_splits_at_spaces() {
    let editor = Editor::new();
    editor.set_text("abc");
    editor.insert_char('x');
    editor.insert_char('y');
    editor.insert_char(' ');
    editor.insert_char('z');
    editor.insert_char('w');
    assert_eq!(editor.get_text(), "abcxy zw");
    editor.undo(); // removes " zw" (space + word, one unit)
    assert_eq!(editor.get_text(), "abcxy");
    editor.undo(); // removes "xy" (one word unit)
    assert_eq!(editor.get_text(), "abc");
    editor.undo(); // restores the pre-set_text state
    assert_eq!(editor.get_text(), "");
    // Undoing an empty stack is a no-op.
    editor.undo();
    assert_eq!(editor.get_text(), "");
}

/// Newlines and backspaces are individually undoable.
#[test]
fn undo_newline_and_backspace() {
    let editor = Editor::new();
    editor.set_text("ab");
    editor.newline();
    let state = editor.state_snapshot();
    assert_eq!(state.lines, vec!["ab".to_string(), String::new()]);
    editor.undo();
    let state = editor.state_snapshot();
    assert_eq!(state.lines, vec!["ab".to_string()]);
    assert_eq!(state.cursor_line, 0);
    assert_eq!(state.cursor_col, 2);

    editor.move_cursor_end();
    editor.backspace();
    assert_eq!(editor.get_text(), "a");
    editor.undo();
    assert_eq!(editor.get_text(), "ab");
}

/// Undo restores the cursor position along with the text.
#[test]
fn undo_restores_cursor() {
    let editor = Editor::new();
    editor.set_text("abcdef");
    editor.move_cursor_home();
    editor.delete_word_forward(); // deletes the whole word under the cursor
    assert_eq!(editor.get_text(), "");
    editor.undo();
    let state = editor.state_snapshot();
    assert_eq!(editor.get_text(), "abcdef");
    assert_eq!(state.cursor_col, 0);
}

/// Inserted (pasted) text is one atomic undo unit.
#[test]
fn insert_text_is_one_undo_unit() {
    let editor = Editor::new();
    editor.set_text("ab");
    editor.insert_text("cd");
    assert_eq!(editor.get_text(), "abcd");
    editor.undo();
    assert_eq!(editor.get_text(), "ab");
}

/// Submitting clears the undo stack (upstream `submitValue`,
/// `editor.ts:1373`).
#[test]
fn submit_clears_undo_stack() {
    let editor = Editor::new();
    editor.set_text("hello");
    assert_eq!(editor.submit().as_deref(), Some("hello"));
    editor.undo();
    assert_eq!(editor.get_text(), "");
}

/// Yank and yank-pop push their own undo snapshots.
#[test]
fn yank_and_yank_pop_are_undoable() {
    let editor = Editor::new();
    editor.set_text("hello world");
    editor.move_word_left(); // col 6; kill "hello "
    editor.delete_to_line_start();
    editor.yank(); // text "hello world" again
    editor.undo();
    assert_eq!(editor.get_text(), "world");
    editor.yank();
    editor.yank_pop();
    assert_eq!(editor.get_text(), "hello world"); // pop re-yanks same entry
    editor.undo();
    assert_eq!(editor.get_text(), "world");
}

/// Page scrolling moves the cursor by `max(5, rows × 30%)` lines, clamped
/// to the first/last line (upstream `pageScroll`, `editor.ts:1957-1971`;
/// pir moves in logical lines).
#[test]
fn page_up_and_page_down_move_cursor_by_page() {
    let editor = Editor::new();
    let lines: Vec<String> = (0..30).map(|i| format!("line{i}")).collect();
    editor.set_text(&lines.join("\n"));
    let rows = 24; // page = max(5, 7) = 7
    editor.page_up(rows);
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 22); // 29 - 7
    editor.page_up(rows);
    editor.page_up(rows);
    editor.page_up(rows);
    editor.page_up(rows); // 1 - 7 clamps at 0
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 0);
    editor.page_down(rows);
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 7);
    editor.page_down(rows);
    editor.page_down(rows);
    editor.page_down(rows);
    editor.page_down(rows); // 28 + 7 clamps at 29
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 29);
}
