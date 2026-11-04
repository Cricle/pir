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

// ---------------------------------------------------------------------------
// FIX-B13: the PNG transcode cache is bounded by entries AND total bytes
// ---------------------------------------------------------------------------

/// The process-wide transcode cache must evict from the tail until both the
/// entry bound and the total-byte bound hold. A negative cache entry (`None`)
/// costs zero bytes; re-inserting the same key replaces its old entry instead
/// of double-counting.
#[test]
fn png_cache_is_bounded_by_entry_count_and_total_bytes() {
    use pir::tui::terminal_image::{
        png_cache_clear_for_test, png_cache_insert_for_test, png_cache_stats_for_test,
    };

    const MAX_ENTRIES: usize = 32;
    const SMALL_BYTES: usize = 1000;

    // Entry bound only (byte budget generous): 40 inserts leave the newest 32.
    png_cache_clear_for_test();
    for key in 0..40u64 {
        png_cache_insert_for_test(key, Some("x".repeat(10)), MAX_ENTRIES, 1 << 20);
    }
    let (entries, bytes) = png_cache_stats_for_test();
    assert_eq!(entries, MAX_ENTRIES, "entry bound trims the tail");
    assert_eq!(bytes, MAX_ENTRIES * 10, "the newest 32 entries are kept");

    // Negative-cache entries (`None`) cost zero bytes.
    png_cache_insert_for_test(999, None, MAX_ENTRIES, 1 << 20);
    let (entries, bytes) = png_cache_stats_for_test();
    assert_eq!(entries, MAX_ENTRIES);
    assert_eq!(
        bytes,
        MAX_ENTRIES * 10 - 10,
        "None counts as zero bytes (one 10-byte entry was displaced)"
    );

    // Byte bound only: every entry is 300 bytes, so a 1000-byte budget keeps
    // at most three — the newest ones — no matter the entry bound.
    png_cache_clear_for_test();
    for key in 0..10u64 {
        png_cache_insert_for_test(key, Some("z".repeat(300)), MAX_ENTRIES, SMALL_BYTES);
        let (entries, bytes) = png_cache_stats_for_test();
        assert!(entries <= 3, "byte budget caps the entry count: {entries}");
        assert!(bytes <= SMALL_BYTES, "total bytes stay bounded: {bytes}");
    }
    let (entries, bytes) = png_cache_stats_for_test();
    assert_eq!(entries, 3);
    assert_eq!(bytes, 900);

    // Same key: replacement, not double-counting.
    png_cache_clear_for_test();
    for _ in 0..5 {
        png_cache_insert_for_test(7, Some("y".repeat(100)), MAX_ENTRIES, 1 << 20);
    }
    png_cache_insert_for_test(7, Some("y".repeat(100)), MAX_ENTRIES, 1 << 20);
    let (entries, bytes) = png_cache_stats_for_test();
    assert_eq!(entries, 1, "one entry per key");
    assert_eq!(bytes, 100, "the old size was deducted");

    png_cache_clear_for_test();
}

// ---------------------------------------------------------------------------
// FIX-D09: streamed tool-call arguments are released at AgentEnd
// ---------------------------------------------------------------------------

/// The worker bridge caches `toolcall_*` arguments for `tool_execution_start`
/// /`_end` and for calls whose `toolcall_end` was cut off; the cache lives in
/// the handler closure for the whole TUI session, so `AgentEvent::AgentEnd`
/// must clear it. This drives the REAL handler through
/// `WorkerEventBridgeProbe`: the lifecycle below exercises the
/// `args_by_call.or_else(known_args)` fallback at `AgentEnd`, and the final
/// `AgentEnd` must empty both maps — deleting the handler's
/// `release_tool_args_on_agent_end` call makes this test fail.
#[test]
fn tool_arg_cache_survives_a_run_and_is_released_at_agent_end() {
    use pir::agent::agent_loop::{AgentEvent, LoopResult};
    use pir::agent::tool_batch::ToolExecutionEvent;
    use pir::ai::types::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Context, TextContent, ToolCall,
        ToolResultMessage, ToolResultRole,
    };
    use pir::tui::terminal::WorkerEventBridgeProbe;

    let mut bridge = WorkerEventBridgeProbe::new();
    // A `write` call whose arguments carry a whole file body.
    let args = json!({ "path": "src/main.rs", "content": "x".repeat(256 * 1024) });

    // ToolCallStart: the partial assistant message carries the call; the
    // handler records its parsed arguments and emits the live update.
    let mut partial = AssistantMessage::default();
    partial.content.push(ContentBlock::ToolCall(ToolCall {
        id: "call-1".to_owned(),
        name: "write".to_owned(),
        arguments: args
            .as_object()
            .expect("object args")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }));
    bridge.handle(AgentEvent::StreamEvent {
        turn: 0,
        event: AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            partial: std::sync::Arc::new(partial),
        },
    });
    {
        let cache = bridge
            .tool_args()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(cache.get("call-1"), Some(&args));
        assert!(cache.raw_len() == 0, "a parsed start needs no raw buffer");
    }
    // A call whose `toolcall_end` was cut off keeps its raw buffer until the
    // run settles; `AgentEnd` must release this map too.
    bridge
        .tool_args()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .raw_buffer("call-1")
        .push_str("{\"path\":");

    // ToolExecutionStart / ToolExecutionEnd: the cache is the only source of
    // the args (the execution events carry none / the result omits them).
    bridge.handle(AgentEvent::ToolExecutionStart {
        turn: 0,
        event: ToolExecutionEvent::Start {
            index: 0,
            call_id: "call-1".to_owned(),
            tool_name: "write".to_owned(),
            parent_call_id: None,
        },
        args: serde_json::Value::Null,
    });
    bridge.handle(AgentEvent::ToolExecutionEnd {
        turn: 0,
        event: ToolExecutionEvent::End {
            index: 0,
            call_id: "call-1".to_owned(),
            tool_name: "write".to_owned(),
            is_error: false,
            duration_ms: Some(1),
            parent_call_id: None,
        },
        result: json!({ "content": [{ "type": "text", "text": "written" }] }),
    });
    let updates = bridge.take_tool_updates();
    assert_eq!(
        updates.len(),
        1,
        "ToolExecutionEnd renders one block update"
    );
    assert_eq!(updates[0].0.as_deref(), Some("call-1"));
    assert_eq!(
        updates[0].1, args,
        "the cached args survive ToolExecutionEnd for the AgentEnd fallback"
    );

    // AgentEnd: `new_messages` is empty, so the run's assistant message does
    // not supply `args_by_call`; the block must fall back to the cache
    // (`args_by_call.or_else(known_args)`) BEFORE the cleanup releases it.
    bridge.handle(AgentEvent::AgentEnd {
        result: std::sync::Arc::new(LoopResult {
            terminal: AssistantMessage::default(),
            context: std::sync::Arc::new(Context::default()),
            tool_results: vec![ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: "call-1".to_owned(),
                tool_name: "write".to_owned(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "written".to_owned(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                structured_content: None,
                nested_calls: None,
                is_error: false,
                duration_ms: Some(1),
                timestamp: 0,
            }],
            new_messages: Vec::new(),
            preamble: Vec::new(),
            turns: 1,
            aborted: false,
            usage_total: 0,
            will_retry: false,
        }),
    });
    let updates = bridge.take_tool_updates();
    assert_eq!(updates.len(), 1, "AgentEnd renders the run's tool results");
    assert_eq!(updates[0].0.as_deref(), Some("call-1"));
    assert_eq!(
        updates[0].1, args,
        "AgentEnd must fall back to the known args after ToolExecutionEnd"
    );
    assert!(
        bridge
            .tool_args()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "AgentEnd must clear both the args and the raw-buffer maps"
    );

    // Run 2 reuses the same bridge (same TUI session): a new call's args
    // resolve normally once the old run's entries are gone.
    bridge.handle(AgentEvent::StreamEvent {
        turn: 0,
        event: AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: ToolCall {
                id: "call-2".to_owned(),
                name: "read".to_owned(),
                arguments: [("path".to_owned(), json!("src/lib.rs"))]
                    .into_iter()
                    .collect(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            },
            partial: std::sync::Arc::new(AssistantMessage::default()),
        },
    });
    let cache = bridge
        .tool_args()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        cache.get("call-2"),
        Some(&json!({ "path": "src/lib.rs" })),
        "the same cache keeps working for the next run"
    );
    assert!(cache.get("call-1").is_none());
}

/// A background call's persisted tool result is the synthesized PLACEHOLDER.
/// The TUI card is flipped by `SessionEvent::BackgroundToolEnd`; the
/// `AgentEnd` replay must not overwrite a real completion (or re-open the
/// pending phase) with that placeholder. `ToolExecutionEnd` already skips the
/// `details.background` marker; this pins the same rule on the run-settle
/// replay through the REAL worker handler.
#[test]
fn agent_end_does_not_replay_a_background_placeholder_result() {
    use pir::agent::agent_loop::{AgentEvent, LoopResult};
    use pir::ai::types::{
        AssistantMessage, ContentBlock, Context, TextContent, ToolResultMessage, ToolResultRole,
    };
    use pir::tui::terminal::WorkerEventBridgeProbe;

    let mut bridge = WorkerEventBridgeProbe::new();
    bridge.handle(AgentEvent::AgentEnd {
        result: std::sync::Arc::new(LoopResult {
            terminal: AssistantMessage::default(),
            context: std::sync::Arc::new(Context::default()),
            tool_results: vec![
                ToolResultMessage {
                    role: ToolResultRole::ToolResult,
                    tool_call_id: "call-bg".to_owned(),
                    tool_name: "bash".to_owned(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "Tool 'bash' is running in the background (callId: call-bg). Its result will arrive as a separate message; do not wait for it.".to_owned(),
                        text_signature: None,
                    })],
                    details: Some(json!({
                        "background": true,
                        "callId": "call-bg",
                        "toolName": "bash"
                    })),
                    usage: None,
                    added_tool_names: Vec::new(),
                    structured_content: None,
                    nested_calls: None,
                    is_error: false,
                    duration_ms: None,
                    timestamp: 0,
                },
                ToolResultMessage {
                    role: ToolResultRole::ToolResult,
                    tool_call_id: "call-fg".to_owned(),
                    tool_name: "bash".to_owned(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "foreground done".to_owned(),
                        text_signature: None,
                    })],
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    structured_content: None,
                    nested_calls: None,
                    is_error: false,
                    duration_ms: Some(3),
                    timestamp: 0,
                },
            ],
            new_messages: Vec::new(),
            preamble: Vec::new(),
            turns: 1,
            aborted: false,
            usage_total: 0,
            will_retry: false,
        }),
    });

    let updates = bridge.take_tool_updates();
    assert_eq!(
        updates.len(),
        1,
        "only the real result is replayed: {updates:?}"
    );
    assert_eq!(updates[0].0.as_deref(), Some("call-fg"));
}

/// A background completion is the TUI's only "result arrived" signal (the
/// turn ended on a placeholder). The production session-event bridge must
/// turn `BackgroundToolEnd` into the same `ToolUpdate` a foreground result
/// uses, carrying the REAL output so the card flips in place.
#[test]
fn background_tool_end_bridge_reports_the_real_output() {
    use pir::agent_session::SessionEvent;
    use pir::tui::terminal::SessionEventBridgeProbe;

    let mut probe = SessionEventBridgeProbe::new();
    probe.handle(SessionEvent::BackgroundToolEnd {
        call_id: "call-bg".to_owned(),
        tool_name: "subagent".to_owned(),
        parent_call_id: None,
        is_error: false,
        duration_ms: Some(12),
        output: "CHILD_DONE".to_owned(),
        images: Vec::new(),
        details: Some(json!({ "mode": "single" })),
        structured_content: None,
        usage: None,
    });

    let updates = probe.take_tool_updates();
    assert_eq!(
        updates,
        vec![(Some("call-bg".to_owned()), "CHILD_DONE".to_owned(), false)],
        "the bridge must update the placeholder card with the real result"
    );
}

/// The two maps are keyed independently: `remove_raw` only drops the raw
/// fallback buffer of the finished call, never another call's entries.
#[test]
fn tool_arg_cache_tracks_calls_independently() {
    use pir::tui::terminal::ToolArgCache;

    let mut cache = ToolArgCache::new();
    cache.insert("a", json!({ "n": 1 }));
    cache.insert("b", json!({ "n": 2 }));
    cache.raw_buffer("a").push('{');
    cache.raw_buffer("b").push('{');
    assert_eq!(cache.len(), 4);

    cache.remove_raw("a");
    assert_eq!(cache.raw_len(), 1);
    assert_eq!(cache.get("a"), Some(&json!({ "n": 1 })));
    assert_eq!(cache.get("b"), Some(&json!({ "n": 2 })));

    cache.clear();
    assert!(cache.is_empty());
}

// ---------------------------------------------------------------------------
// FIX-B05: Windows console QuickEdit is cleared and restored
// ---------------------------------------------------------------------------

/// `disable_windows_quick_edit` clears `ENABLE_QUICK_EDIT_MODE` on the console
/// input handle and `restore_windows_quick_edit` puts the captured mode back.
/// A redirected stdin / no console is the documented skip (CI); a console
/// that already has QuickEdit off has nothing to restore.
#[cfg(windows)]
#[test]
fn windows_quick_edit_is_cleared_and_restored() {
    use crossterm_winapi::{ConsoleMode, Handle, HandleType};
    use pir::tui::stdin_buffer::{disable_windows_quick_edit, restore_windows_quick_edit};

    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;

    let Ok(handle) = Handle::new(HandleType::InputHandle) else {
        return;
    };
    let console = ConsoleMode::from(handle);
    let Ok(before) = console.mode() else {
        return;
    };
    if before & ENABLE_QUICK_EDIT_MODE == 0 {
        // Windows Terminal / redirected console: nothing to disable, and the
        // restore must stay a safe no-op.
        assert!(!disable_windows_quick_edit());
        restore_windows_quick_edit();
        return;
    }

    assert!(disable_windows_quick_edit());
    let during = console.mode().expect("console mode after disable");
    assert_eq!(
        during & ENABLE_QUICK_EDIT_MODE,
        0,
        "QuickEdit bit cleared while the TUI runs"
    );

    restore_windows_quick_edit();
    let after = console.mode().expect("console mode after restore");
    assert_eq!(
        after & ENABLE_QUICK_EDIT_MODE,
        ENABLE_QUICK_EDIT_MODE,
        "QuickEdit bit restored after cleanup"
    );
}
