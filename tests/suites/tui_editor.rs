//! Editor input-history browsing (upstream `navigateHistory`).

use std::collections::VecDeque;

use crossterm::event::Event;
use pir::tui::core::Component;
use pir::tui::editor::Editor;
use pir::tui::terminal::coalesce_pending_input;

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

// ---------------------------------------------------------------------------
// Paste performance / incremental layout (TUI-P3)
// ---------------------------------------------------------------------------

fn ten_thousand_lines(separator: &str, line: impl Fn(usize) -> String) -> String {
    (0..10_000).map(line).collect::<Vec<_>>().join(separator)
}

/// A 10k-line paste must be cleaned in one borrow-and-count pass (the plain
/// input allocates nothing), stored once, and returned once — with the full
/// first/last/line-count content intact through the fold + expand path.
#[test]
fn ten_thousand_line_paste_has_bounded_copies_and_complete_content() {
    let editor = Editor::new();
    let input = ten_thousand_lines("\n", |index| format!("line {index:05} payload"));
    let depth_before = editor.undo_depth();

    let inserted = editor.paste_text(&input);
    assert_eq!(inserted, input, "the cleaned paste is reported verbatim");

    let report = editor
        .last_paste_report()
        .expect("paste_text records its accounting");
    assert_eq!(report.input_bytes, input.len());
    assert_eq!(report.output_bytes, input.len());
    assert_eq!(report.output_chars, input.chars().count());
    assert_eq!(report.line_count, 10_000);
    assert_eq!(
        report.cleaning_allocations, 0,
        "a clean paste must borrow the input, not allocate a second buffer"
    );
    assert_eq!(report.undo_snapshots, 1, "one undo snapshot for the paste");
    assert_eq!(editor.undo_depth(), depth_before + 1);
    // Cleaning copies nothing, the registry stores one Arc copy, and the
    // returned String is the only other full-size copy.
    assert!(
        report.copied_bytes <= input.len() * 2 + 256,
        "10k-line paste copied {} bytes (input {})",
        report.copied_bytes,
        input.len()
    );

    assert_eq!(editor.get_text(), "[paste #1 +10000 lines]");
    let expanded = editor.expanded_text();
    let lines: Vec<&str> = expanded.lines().collect();
    assert_eq!(lines.len(), 10_000);
    assert_eq!(lines.first().copied(), Some("line 00000 payload"));
    assert_eq!(lines.last().copied(), Some("line 09999 payload"));
    assert_eq!(editor.submit().as_deref(), Some(input.as_str()));
}

/// The same 10k-line payload with CRLF + tabs allocates exactly one cleaning
/// buffer and produces the same normalized text the old multi-pass pipeline
/// did, still within a small copy bound.
#[test]
fn ten_thousand_line_crlf_tab_paste_allocates_one_cleaning_buffer() {
    let editor = Editor::new();
    let input = ten_thousand_lines("\r\n", |index| format!("row {index:05}\tvalue"));
    let expected = ten_thousand_lines("\n", |index| format!("row {index:05}    value"));

    let inserted = editor.paste_text(&input);
    assert_eq!(inserted, expected);
    let report = editor.last_paste_report().expect("paste accounting");
    assert_eq!(report.cleaning_allocations, 1);
    assert_eq!(report.output_bytes, expected.len());
    assert_eq!(report.output_chars, expected.chars().count());
    assert_eq!(report.line_count, 10_000);
    assert!(
        report.copied_bytes <= expected.len() * 2 + 256,
        "normalizing copies {} bytes for a {} byte payload",
        report.copied_bytes,
        expected.len()
    );
    assert_eq!(editor.expanded_text(), expected);
}

/// Cleaner parity: CSI-u decode, CRLF/lone CR normalization, tab expansion,
/// C0 filtering and an undecodable CSI-u tail must all happen in one pass.
#[test]
fn single_pass_paste_cleaning_matches_upstream_semantics() {
    let editor = Editor::new();
    let inserted = editor.paste_text("a\tb\r\n\x1b[106;5uc\x01d\x1b[999;5ue");
    assert_eq!(inserted, "a    b\n\ncd[999;5ue");
    assert_eq!(editor.get_text(), inserted);
}

/// The wrap cache must be reused on a steady frame and a paste into a 10k-line
/// draft must rebuild only the edited line (and only the new lines for a
/// multiline insert), never the whole draft.
#[test]
fn layout_cache_reuses_unchanged_draft_and_paste_rebuilds_only_touched_lines() {
    let editor = Editor::new();
    let draft = ten_thousand_lines("\n", |index| format!("line {index:05} payload"));
    editor.set_text(&draft);

    let before_first = editor.layout_cache_stats();
    let _ = editor.render_with_cursor_rows(80, 5);
    let first = editor.layout_cache_stats();
    assert_eq!(first.full_builds - before_first.full_builds, 1);
    assert_eq!(first.lines_built - before_first.lines_built, 10_000);

    let _ = editor.render_with_cursor_rows(80, 5);
    let steady = editor.layout_cache_stats();
    assert_eq!(
        steady.full_builds, first.full_builds,
        "an unchanged draft must not rebuild the layout"
    );
    assert_eq!(
        steady.lines_built, first.lines_built,
        "an unchanged draft must not rebuild any line plan"
    );
    assert_eq!(steady.cache_hits - first.cache_hits, 1);
    assert!(
        steady.rendered_lines - first.rendered_lines <= 5,
        "only the visible window is materialized: {} rows",
        steady.rendered_lines - first.rendered_lines
    );

    // Single-line paste at the cursor (end of the draft): exactly one plan.
    editor.paste_text(" tail");
    let after_single = editor.layout_cache_stats();
    assert_eq!(
        after_single.incremental_updates - steady.incremental_updates,
        1
    );
    assert_eq!(after_single.lines_built - steady.lines_built, 1);
    assert_eq!(after_single.full_builds, steady.full_builds);
    assert_eq!(
        editor.get_text().lines().last(),
        Some("line 09999 payload tail")
    );

    let _ = editor.render_with_cursor_rows(80, 5);
    let rendered = editor.layout_cache_stats();
    assert_eq!(rendered.full_builds, steady.full_builds);
    assert_eq!(rendered.cache_hits - steady.cache_hits, 1);

    // Five-line (below-fold) paste: five new/touched plans, tail reused.
    let before_multi = editor.layout_cache_stats();
    editor.paste_text("a\nb\nc\nd\ne");
    let after_multi = editor.layout_cache_stats();
    assert_eq!(
        after_multi.incremental_updates - before_multi.incremental_updates,
        1
    );
    assert_eq!(after_multi.lines_built - before_multi.lines_built, 5);
    assert_eq!(after_multi.full_builds, before_multi.full_builds);
    let text = editor.get_text();
    assert_eq!(text.lines().count(), 10_004);
    assert_eq!(text.lines().next(), Some("line 00000 payload"));
    assert_eq!(text.lines().last(), Some("e"));
}

/// `Component::content_revision` moves on content/style/cursor changes but
/// stays put across a steady render, which is what lets the frame cache serve
/// the editor rows while a token stream repaints only the transcript.
#[test]
fn editor_content_revision_tracks_render_inputs() {
    let editor = Editor::new();
    let revision = || Component::content_revision(&editor).expect("editor reports a revision");
    let initial = revision();
    assert_eq!(revision(), initial, "no change ⇒ same revision");
    let _ = editor.render_with_cursor_rows(80, 5);
    assert_eq!(revision(), initial, "rendering is not a content change");

    editor.paste_text("tail");
    let after_paste = revision();
    assert_ne!(after_paste, initial, "paste moves the revision");

    editor.set_cursor_position(0, 0);
    assert_ne!(revision(), after_paste, "cursor moves move the revision");

    editor.set_border_color("\x1b[31m".to_string());
    assert_ne!(revision(), after_paste, "style inputs move the revision");
}

/// Consecutive `Event::Paste` chunks in the input queue are one editor
/// insertion, so a bracketless paste costs one undo snapshot/autocomplete
/// invalidation instead of one per chunk.
#[test]
fn adjacent_paste_events_coalesce_into_one_insertion() {
    let mut pending = VecDeque::new();
    pending.push_back(Event::Paste("alpha ".to_string()));
    pending.push_back(Event::Paste("beta".to_string()));
    pending.push_back(Event::Paste(" gamma".to_string()));
    assert!(coalesce_pending_input(&mut pending, 0));
    assert_eq!(pending.len(), 1);
    match pending.pop_front() {
        Some(Event::Paste(text)) => assert_eq!(text, "alpha beta gamma"),
        other => panic!("expected one merged paste, got {other:?}"),
    }
    assert!(pending.is_empty());
}

/// Even when the terminal delivers a bracketless paste as many `paste_text`
/// calls, consecutive chunks share the first undo snapshot; the draft grows
/// linearly (in-place `insert_str`) instead of cloning it per chunk.
#[test]
fn consecutive_paste_chunks_share_one_undo_snapshot() {
    let editor = Editor::new();
    for _ in 0..500 {
        editor.paste_text("0123456789");
    }
    assert_eq!(editor.get_text().len(), 5_000);
    assert_eq!(editor.undo_depth(), 1, "one snapshot for the whole burst");
    editor.undo();
    assert_eq!(editor.get_text(), "");
    assert_eq!(editor.undo_depth(), 0);
}
