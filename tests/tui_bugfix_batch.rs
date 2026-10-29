//! TUI bugfix batch: filtered-selector highlight mapping (Bug 1), editor
//! row-width discipline with long unbroken tokens (Bug 2), quoted
//! `/export` path char boundaries (Bug 3), uppercase text-entry keys
//! (Bug 4) and the extension-dialog timeout countdown (Bug 5). Pure
//! function-layer assertions only; the loop-level sweep lives in
//! `terminal.rs`.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::json;

use pir::tui::core::Component;
use pir::tui::dialogs::{countdown_seconds, dialog_key_from_crossterm, DialogKey, DialogOverlay};
use pir::tui::editor::Editor;
use pir::tui::selector::{SelectItem, SelectList};
use pir::tui::strip_ansi;
use pir::tui::terminal::{is_text_input_key, path_command_argument};
use pir::tui::visible_width;

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

fn item(id: &str) -> SelectItem<String> {
    SelectItem {
        value: id.to_string(),
        id: id.to_string(),
        provider: "prov".to_string(),
        description: String::new(),
        is_current: false,
        is_default: false,
    }
}

// ---------------------------------------------------------------------------
// Bug 1: the highlight follows the FILTERED position
// ---------------------------------------------------------------------------

/// With a query active, `self.selected` is a filtered position; the rendered
/// cursor must mark that filtered row (the old raw-index comparison left the
/// whole list unhighlighted or highlighted the wrong entry).
#[test]
fn filtered_selector_highlights_the_filtered_position() {
    let mut list = SelectList::new(vec![
        item("apple"),
        item("banana"),
        item("cherry"),
        item("apricot"),
    ]);
    list.set_query("ap"); // filtered: [apple, apricot]
    assert_eq!(list.selected_item().map(|i| i.id.as_str()), Some("apple"));
    list.move_down();
    assert_eq!(list.selected_item().map(|i| i.id.as_str()), Some("apricot"));
    let rows: Vec<String> = list
        .render_lines(80)
        .into_iter()
        .map(|row| strip_ansi(&row))
        .collect();
    let cursors: Vec<&String> = rows.iter().filter(|row| row.starts_with("→ ")).collect();
    assert_eq!(cursors.len(), 1, "exactly one highlighted row: {rows:?}");
    assert!(
        cursors[0].contains("apricot"),
        "the cursor marks the filtered selection: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with("  ") && row.contains("apple")),
        "apple stays unselected: {rows:?}"
    );
}

/// `set_query` keeps the selection on the same item when it still matches
/// (all comparisons between old filtered position, raw index and new filtered
/// position must stay in the filtered-position domain).
#[test]
fn selector_query_keeps_the_selected_item_when_it_still_matches() {
    let mut list = SelectList::new(vec![item("apple"), item("banana"), item("cherry")]);
    list.move_down();
    assert_eq!(list.selected_item().map(|i| i.id.as_str()), Some("banana"));
    list.set_query("an"); // matches only banana
    assert_eq!(list.selected_item().map(|i| i.id.as_str()), Some("banana"));
    list.set_query(""); // cleared: the same item stays selected
    assert_eq!(list.selected_item().map(|i| i.id.as_str()), Some("banana"));
    list.set_query("xyz"); // no match: no selection
    assert!(list.selected_item().is_none());
}

// ---------------------------------------------------------------------------
// Bug 2: rendered editor rows are exactly `width` cells
// ---------------------------------------------------------------------------

/// A long unbroken token (URL/path) fills whole wrapped rows. With the
/// padding-aware layout width every rendered row — including the row whose
/// end-of-line cursor overflows one cell into the right padding — measures
/// EXACTLY `width` (the old `width - 1` wrap produced `width + 1` / `width + 2`
/// rows and broke the diff renderer's one-row-per-screen-row assumption).
#[test]
fn editor_rows_measure_exactly_the_render_width() {
    // Token length is an exact multiple of the padded content width (38), so
    // the last row ends with the cursor's virtual space at the boundary.
    let editor = Editor::new();
    editor.set_padding_x(1);
    editor.set_text(&"x".repeat(38 * 3));
    let (lines, _row, _col) = editor.render_with_cursor_rows(40, 5);
    assert_eq!(lines.len(), 5, "border + 3 rows + border");
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(visible_width(line), 40, "row {index}: {line:?}");
    }

    // A URL whose final wrapped piece is short — the plain padding path.
    editor.set_text(&format!(
        "see https://example.com/{}",
        "very-long-path-segment/".repeat(6)
    ));
    let (lines, _row, _col) = editor.render_with_cursor_rows(40, 5);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(visible_width(line), 40, "row {index}: {line:?}");
    }

    // Padding off: the reserved cursor column keeps rows exact too.
    editor.set_padding_x(0);
    let (lines, _row, _col) = editor.render_with_cursor_rows(40, 5);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(visible_width(line), 40, "row {index}: {line:?}");
    }
}

// ---------------------------------------------------------------------------
// Bug 3: quoted path parsing slices on char boundaries
// ---------------------------------------------------------------------------

/// `after_open.find(quote)` returns a BYTE offset; treating it as a char
/// count swallowed the closing quote whenever multibyte text preceded it.
#[test]
fn quoted_export_paths_parse_at_char_boundaries() {
    assert_eq!(
        path_command_argument("/export \"报告.md\"", "export").as_deref(),
        Some("报告.md")
    );
    assert_eq!(
        path_command_argument("/export '空格 名.md'", "export").as_deref(),
        Some("空格 名.md")
    );
    assert_eq!(
        path_command_argument("/export report.md", "export").as_deref(),
        Some("report.md")
    );
    assert_eq!(path_command_argument("/export", "export"), None);
    assert_eq!(path_command_argument("/export   ", "export"), None);
    assert_eq!(
        path_command_argument("/export \"unterminated", "export"),
        None
    );
    assert_eq!(path_command_argument("/export \"\"", "export"), None);
}

// ---------------------------------------------------------------------------
// Bug 4: uppercase characters (Char + SHIFT) reach the text inputs
// ---------------------------------------------------------------------------

/// crossterm reports uppercase A-Z as `Char('A') + SHIFT`; the text-entry
/// gate must accept SHIFT-composed keys and reject only CONTROL/ALT.
#[test]
fn uppercase_and_shift_keys_reach_text_input() {
    assert!(is_text_input_key(&key_event(
        KeyCode::Char('A'),
        KeyModifiers::SHIFT
    )));
    assert!(is_text_input_key(&key_event(
        KeyCode::Char('a'),
        KeyModifiers::NONE
    )));
    assert!(is_text_input_key(&key_event(
        KeyCode::Enter,
        KeyModifiers::SHIFT
    )));
    assert!(!is_text_input_key(&key_event(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL
    )));
    assert!(!is_text_input_key(&key_event(
        KeyCode::Char('a'),
        KeyModifiers::ALT
    )));
    assert!(!is_text_input_key(&key_event(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    )));
    // The dialog key map shares the SHIFT-carrying uppercase shape.
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Char('A'), KeyModifiers::SHIFT),
        Some(DialogKey::Char('A'))
    );
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Char('a'), KeyModifiers::CONTROL),
        None
    );
}

// ---------------------------------------------------------------------------
// Bug 5: dialog timeout countdown (title seconds + expiry)
// ---------------------------------------------------------------------------

/// `Math.ceil(timeoutMs / 1000)` seconds (upstream CountdownTimer).
#[test]
fn countdown_seconds_round_up() {
    assert_eq!(countdown_seconds(Duration::from_millis(5000)), 5);
    assert_eq!(countdown_seconds(Duration::from_millis(5001)), 6);
    assert_eq!(countdown_seconds(Duration::from_millis(1)), 1);
    assert_eq!(countdown_seconds(Duration::ZERO), 0);
    assert_eq!(countdown_seconds(Duration::from_millis(999)), 1);
    assert_eq!(countdown_seconds(Duration::from_millis(1000)), 1);
}

/// A `timeout` in the request options arms the deadline: the title renders
/// `(Ns)` and `expired` flips at the deadline; without a timeout nothing
/// changes.
#[test]
fn dialog_timeout_arms_countdown_and_expiry() {
    let timed =
        DialogOverlay::from_request(1, "input", &json!({ "title": "Name", "timeout": 5000 }));
    assert_eq!(timed.countdown_seconds_at(Instant::now()), Some(5));
    assert!(!timed.expired(Instant::now()));
    assert!(timed.expired(Instant::now() + Duration::from_secs(6)));
    let frame = timed
        .render(60)
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(strip_ansi(&frame).contains("Name (5s)"), "{frame}");

    let plain = DialogOverlay::from_request(
        2,
        "select",
        &json!({ "title": "Pick", "options": ["a", "b"] }),
    );
    assert_eq!(plain.countdown_seconds_at(Instant::now()), None);
    assert!(!plain.expired(Instant::now() + Duration::from_secs(3600)));
    let frame = plain
        .render(60)
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let plain_frame = strip_ansi(&frame);
    assert!(plain_frame.contains("Pick"), "{frame}");
    assert!(!plain_frame.contains("Pick ("), "{frame}");
}
