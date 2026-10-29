//! Audit §7 follow-ups for lane L: `/debug` render-frame dump (#19), selection
//! and multi-message copy (#7), `/scoped-models` search (#15), and
//! `tui.select.*` navigation rebinding (#10).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pir::ai::types::{AssistantMessage, ContentBlock, TextContent};
use pir::tui::app::ChatEntry;
use pir::tui::keybindings::Keybindings;
use pir::tui::slash::{
    assistant_texts, copy_assistant_messages, copy_count_argument, copy_last_assistant_message,
    debug_dump_path, debug_dump_text, debug_message_jsonl, last_assistant_text, CopyOutcome,
    DebugFrameLayout,
};
use pir::tui::terminal::ScopedModelsSelector;
use serde_json::json;

fn assistant_with_text(text: &str) -> ChatEntry {
    ChatEntry::Assistant(Arc::new(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// /debug frame dump (upstream `handleDebugCommand`)
// ---------------------------------------------------------------------------

#[test]
fn debug_dump_contains_terminal_layout_lines_widths_and_messages() {
    let layout = DebugFrameLayout {
        columns: 80,
        rows: 24,
        frame_len: 2,
        windowed: true,
        window_start: 5,
        window_len: 2,
        transcript_end: 7,
        dock_len: 3,
        visible_top: 5,
        frame_version: 9,
    };
    let frame = vec!["plain".to_string(), "\u{1b}[31mred\u{1b}[39m".to_string()];
    let text = debug_dump_text(
        layout,
        &frame,
        &["{\"role\":\"assistant\"}".to_string()],
        "2026-01-01T00:00:00.000Z",
    );
    assert!(
        text.starts_with("Debug output at 2026-01-01T00:00:00.000Z"),
        "{text}"
    );
    assert!(text.contains("Terminal: 80x24"), "{text}");
    assert!(text.contains("Total lines: 2"), "{text}");
    assert!(
        text.contains(
            "Layout: windowed=true window_start=5 window_len=2 transcript_end=7 dock_len=3 visible_top=5 frame_version=9"
        ),
        "{text}"
    );
    assert!(
        text.contains("=== All rendered lines with visible widths ==="),
        "{text}"
    );
    assert!(text.contains("[0] (w=5) \"plain\""), "{text}");
    // ANSI styling is not counted in the visible width, and the raw line is
    // JSON-escaped like upstream's `JSON.stringify(line)`.
    assert!(
        text.contains("[1] (w=3) \"\\u001b[31mred\\u001b[39m\""),
        "{text}"
    );
    assert!(text.contains("=== Agent messages (JSONL) ==="), "{text}");
    assert!(text.contains("{\"role\":\"assistant\"}"), "{text}");
    assert!(text.ends_with('\n'), "dump ends with a newline: {text:?}");
}

#[test]
fn debug_dump_serializes_assistant_messages_as_jsonl() {
    let entries = vec![
        ChatEntry::User("hi".to_string()),
        assistant_with_text("hello"),
        ChatEntry::Notice {
            kind: pir::tui::app::NoticeKind::Status,
            text: "note".to_string(),
        },
    ];
    let rows = debug_message_jsonl(&entries);
    assert_eq!(rows.len(), 1, "only assistant messages are dumped");
    let parsed: serde_json::Value = serde_json::from_str(&rows[0]).expect("valid JSON");
    assert_eq!(parsed["content"][0]["type"], "text");
    assert_eq!(parsed["content"][0]["text"], "hello");
}

#[test]
fn debug_dump_path_lives_under_the_agent_dir() {
    assert_eq!(
        debug_dump_path(std::path::Path::new("agent")),
        std::path::Path::new("agent").join("pir-debug.log")
    );
}

// ---------------------------------------------------------------------------
// /copy: selection-adjacent multi-message copy (upstream `handleCopyCommand`)
// ---------------------------------------------------------------------------

#[test]
fn copy_argument_parses_count_and_all() {
    assert_eq!(copy_count_argument(""), Ok(None));
    assert_eq!(copy_count_argument("  "), Ok(None));
    assert_eq!(copy_count_argument("2"), Ok(Some(2)));
    assert_eq!(copy_count_argument(" all "), Ok(Some(usize::MAX)));
    assert!(copy_count_argument("0").is_err());
    assert!(copy_count_argument("-1").is_err());
    assert!(copy_count_argument("everything").is_err());
    let error = copy_count_argument("0").expect_err("zero is invalid");
    assert!(error.contains("/copy [count|all]"), "{error}");
}

#[test]
fn copy_multiple_assistant_messages_joins_them_oldest_first() {
    let entries = vec![
        ChatEntry::User("hi".to_string()),
        assistant_with_text("one"),
        ChatEntry::User("again".to_string()),
        assistant_with_text("two"),
        assistant_with_text("three"),
    ];

    let mut copied = String::new();
    let outcome = copy_assistant_messages(&entries, None, |text| {
        copied = text.to_string();
        true
    });
    assert_eq!(outcome, CopyOutcome::Copied);
    assert_eq!(copied, "three");

    let mut copied = String::new();
    let outcome = copy_assistant_messages(&entries, Some(2), |text| {
        copied = text.to_string();
        true
    });
    assert_eq!(outcome, CopyOutcome::CopiedMessages(2));
    assert_eq!(copied, "two\n\nthree");

    let mut copied = String::new();
    let outcome = copy_assistant_messages(&entries, Some(usize::MAX), |text| {
        copied = text.to_string();
        true
    });
    assert_eq!(outcome, CopyOutcome::CopiedMessages(3));
    assert_eq!(copied, "one\n\ntwo\n\nthree");

    assert_eq!(
        assistant_texts(&entries, Some(2)),
        vec!["two".to_string(), "three".to_string()]
    );
    assert_eq!(last_assistant_text(&entries).as_deref(), Some("three"));
}

#[test]
fn copy_skips_text_less_assistant_entries() {
    // pir's `last_assistant_text` has always skipped assistant entries with no
    // text blocks (a tool-only turn falls back to the previous text); the
    // multi-message form uses the same filter.
    let entries = vec![
        assistant_with_text("earlier"),
        ChatEntry::Assistant(Arc::new(AssistantMessage {
            content: Vec::new(),
            ..Default::default()
        })),
    ];
    assert_eq!(last_assistant_text(&entries).as_deref(), Some("earlier"));
    let mut copied = String::new();
    assert_eq!(
        copy_assistant_messages(&entries, None, |text| {
            copied = text.to_string();
            true
        }),
        CopyOutcome::Copied
    );
    assert_eq!(copied, "earlier");
    assert_eq!(
        assistant_texts(&entries, Some(1)),
        vec!["earlier".to_string()]
    );
    assert_eq!(
        copy_assistant_messages(&entries, Some(2), |_| true),
        CopyOutcome::Copied
    );
}

#[test]
fn copy_reports_empty_and_unavailable_clipboard() {
    assert_eq!(
        copy_assistant_messages(&[], None, |_| true),
        CopyOutcome::NothingToCopy
    );
    assert_eq!(
        copy_last_assistant_message(&[assistant_with_text("x")], |_| false),
        CopyOutcome::ClipboardUnavailable
    );
    assert_eq!(
        copy_last_assistant_message(&[assistant_with_text("x")], |_| true),
        CopyOutcome::Copied
    );
}

// ---------------------------------------------------------------------------
// /scoped-models search (upstream `searchInput` + `refresh` fuzzy filter)
// ---------------------------------------------------------------------------

fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(provider, id)| ((*provider).to_string(), (*id).to_string()))
        .collect()
}

#[test]
fn scoped_models_search_filters_the_visible_list_and_toggles_visible_rows() {
    let selector = ScopedModelsSelector::new(
        &pairs(&[
            ("openai", "gpt-5"),
            ("openai", "gpt-6"),
            ("anthropic", "opus"),
        ]),
        pir::tui::slash::EnabledModels::all(),
    );
    assert_eq!(selector.visible_refs().len(), 3);
    assert_eq!(selector.query(), "");

    selector.set_query("gpt");
    assert_eq!(
        selector.visible_refs(),
        vec!["openai/gpt-5".to_string(), "openai/gpt-6".to_string()]
    );
    assert_eq!(selector.selected_ref().as_deref(), Some("openai/gpt-5"));

    selector.move_down();
    assert_eq!(selector.selected_ref().as_deref(), Some("openai/gpt-6"));
    // Navigation wraps inside the filtered list (upstream `SelectList`).
    selector.move_down();
    assert_eq!(selector.selected_ref().as_deref(), Some("openai/gpt-5"));
    selector.move_down();

    // Enter toggles the HIGHLIGHTED filtered row.
    let enabled = selector.toggle_selected();
    assert_eq!(
        enabled,
        vec!["openai/gpt-5".to_string(), "anthropic/opus".to_string()]
    );

    let rendered = selector.render(64).join("\n");
    assert!(rendered.contains(" Search: gpt"), "{rendered}");
    assert!(rendered.contains("openai/gpt-6"), "{rendered}");
    assert!(!rendered.contains("anthropic/opus"), "{rendered}");
    assert!(rendered.contains("type to filter"), "{rendered}");

    // A query with no matches renders the upstream placeholder row.
    selector.set_query("zzzz");
    assert!(selector.visible_refs().is_empty());
    assert_eq!(selector.selected_ref(), None);
    let rendered = selector.render(64).join("\n");
    assert!(rendered.contains("No matching models"), "{rendered}");
    assert!(rendered.contains(" Search: zzzz"), "{rendered}");

    // Clearing the query restores the full catalog.
    selector.set_query("");
    assert_eq!(selector.visible_refs().len(), 3);
}

#[test]
fn scoped_models_search_keeps_the_highlighted_ref_across_a_catalog_refresh() {
    let selector = ScopedModelsSelector::new(
        &pairs(&[("openai", "gpt-5"), ("openai", "gpt-6")]),
        pir::tui::slash::EnabledModels::all(),
    );
    selector.set_query("gpt");
    selector.move_down();
    assert_eq!(selector.selected_ref().as_deref(), Some("openai/gpt-6"));
    selector.update_models(
        &pairs(&[
            ("openai", "gpt-5"),
            ("openai", "gpt-6"),
            ("openai", "gpt-7"),
        ]),
        None,
    );
    assert_eq!(selector.selected_ref().as_deref(), Some("openai/gpt-6"));
    assert_eq!(selector.visible_refs().len(), 3);
}

// ---------------------------------------------------------------------------
// Selector navigation rebinding (upstream `tui.select.*`, select-list.ts)
// ---------------------------------------------------------------------------

#[test]
fn tui_select_navigation_ids_honor_settings_overrides() {
    let settings = json!({
        "tui.select.up": "ctrl+n",
    });
    let kb = Keybindings::from_settings_and_file(Some(&settings), None).expect("keybindings");
    let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    assert!(
        kb.matches_id("tui.select.up", &ctrl_n),
        "an override installs the new chord"
    );
    assert!(
        !kb.matches_id("tui.select.up", &up),
        "an override REPLACES the default arrow"
    );
    // Unbound ids keep their upstream defaults.
    assert!(kb.matches_id(
        "tui.select.down",
        &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
    ));
    assert!(kb.matches_id(
        "tui.select.pageDown",
        &KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
    ));
}
