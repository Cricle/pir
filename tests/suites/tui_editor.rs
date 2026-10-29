//! Editor input-history browsing (upstream `navigateHistory`).

use pir::tui::editor::Editor;

#[test]
fn history_browse_matches_upstream_navigation() {
    let editor = Editor::new();
    editor.set_text("first");
    assert_eq!(editor.submit().as_deref(), Some("first"));
    editor.set_text("second");
    assert_eq!(editor.submit().as_deref(), Some("second"));
    // In-progress draft before browsing starts.
    editor.set_text("draft text");
    assert!(!editor.is_browsing_history());

    // Up: newest entry with the cursor at the start of the first line
    // (upstream navigateHistory direction -1 → placement "start").
    editor.history_navigate(-1);
    assert!(editor.is_browsing_history());
    assert_eq!(editor.get_text(), "second");
    let state = editor.state_snapshot();
    assert_eq!((state.cursor_line, state.cursor_col), (0, 0));

    // Up again: older entry; one more Up is a no-op (bounds check).
    editor.history_navigate(-1);
    assert_eq!(editor.get_text(), "first");
    editor.history_navigate(-1);
    assert_eq!(editor.get_text(), "first");

    // Down: newest entry with the cursor at the end of the last line.
    editor.history_navigate(1);
    assert_eq!(editor.get_text(), "second");
    let state = editor.state_snapshot();
    assert_eq!(state.cursor_line, 0);
    assert_eq!(state.cursor_col, "second".chars().count());

    // Down past the newest: the draft is restored and browsing ends.
    editor.history_navigate(1);
    assert_eq!(editor.get_text(), "draft text");
    assert!(!editor.is_browsing_history());
}

#[test]
fn insert_into_a_multiline_draft_preserves_every_other_line() {
    let editor = Editor::new();
    let text = (0..200)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.set_text(&text);
    assert!(editor.set_cursor_position(0, 0));

    editor.insert_char('X');
    let state = editor.state_snapshot();
    assert_eq!(state.lines.len(), 200);
    assert_eq!(state.lines[0], "Xline 0");
    assert_eq!(state.lines[199], "line 199");
    assert_eq!((state.cursor_line, state.cursor_col), (0, 1));

    // Multi-line insertion at a line start: the first piece joins the line,
    // the last joins its remainder, and every line after it stays untouched.
    assert!(editor.set_cursor_position(0, 0));
    editor.insert_text("a\nb");
    let state = editor.state_snapshot();
    assert_eq!(state.lines.len(), 201);
    assert_eq!(state.lines[0], "a");
    assert_eq!(state.lines[1], "bXline 0");
    assert_eq!(state.lines[2], "line 1");
    assert_eq!(state.lines[200], "line 199");
    assert_eq!((state.cursor_line, state.cursor_col), (1, 1));
}

#[test]
fn folded_paste_survives_edits_undo_and_submit() {
    let editor = Editor::new();
    // Above the fold threshold: stored in the paste registry as one marker.
    let payload = "0123456789".repeat(300);
    editor.paste_text(&payload);
    assert!(editor.get_text().contains("[paste #"));
    assert!(editor.expanded_text().contains(&payload));

    // A whitespace edit pushes an undo snapshot; the registry is cloned into
    // it. The shared payload must still expand after the edit and the undo.
    editor.insert_char(' ');
    assert!(editor.expanded_text().contains(&payload));
    editor.undo();
    let submitted = editor.submit().expect("the expanded paste submits");
    assert_eq!(submitted, payload);
}

#[test]
fn history_is_newest_first_and_deduped() {
    let editor = Editor::new();
    for text in ["a", "b", "b"] {
        editor.set_text(text);
        editor.submit();
    }
    editor.history_navigate(-1);
    assert_eq!(editor.get_text(), "b");
    editor.history_navigate(-1);
    assert_eq!(editor.get_text(), "a");
}
