//! Tool-call display unit tests (batch 5): diff-text rendering, the edit
//! preview input parser, the Elapsed/Took footers, and the edit block's
//! `renderShell: "self"` full-width background (upstream renderers/edit.ts).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use pir::tui::ansi::{strip_ansi, visible_width};
use pir::tui::app::{ChatContainer, ChatEntry, RawRender, RawToolRender, ToolPreview};
use pir::tui::core::Component;
use pir::tui::messages::{edit_preview_input, render_diff_text, ToolExecutionComponent};
use pir::tui::renderers::ToolRenderShell;
use pir::tui::theme::set_theme;

const WIDTH: usize = 40;

fn tools_expanded() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

// --- render_diff_text (components/diff.ts:79-147) ---------------------------

#[test]
fn render_diff_text_renders_context_removed_added_and_skip_markers() {
    let diff = " 3 keep\n-4 old words\n+4 new words\n  ...\n 8 more";
    let rendered = render_diff_text(diff);
    let lines: Vec<String> = strip_ansi(&rendered)
        .split('\n')
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 5, "one row per input line: {rendered:?}");
    assert_eq!(lines[0], " 3 keep", "context line keeps number + content");
    assert_eq!(lines[1], "-4 old words", "single removed line");
    assert_eq!(lines[2], "+4 new words", "single added line");
    assert_eq!(
        lines[3], "  ...",
        "skip marker parses with an empty line number (diff.ts backtracking)"
    );
    assert_eq!(lines[4], " 8 more");

    // A single removed + added pair gets word-level inverse highlighting.
    assert!(
        rendered.contains("\x1b[7m"),
        "intra-line inverse on the changed pair: {rendered:?}"
    );
}

#[test]
fn render_diff_text_multi_line_blocks_show_no_inverse() {
    let diff = "-1 aaa\n-2 bbb\n+1 ccc\n+2 ddd";
    let rendered = render_diff_text(diff);
    let stripped = strip_ansi(&rendered);
    let lines: Vec<&str> = stripped.split('\n').collect();
    assert_eq!(
        lines,
        vec!["-1 aaa", "-2 bbb", "+1 ccc", "+2 ddd"],
        "all removed lines first, then all added"
    );
    assert!(
        !rendered.contains("\x1b[7m"),
        "no intra-line inverse outside a single -/+ pair: {rendered:?}"
    );
}

#[test]
fn render_diff_text_keeps_unparseable_lines_verbatim() {
    let rendered = render_diff_text("plain\n+5 tail");
    let stripped = strip_ansi(&rendered);
    let lines: Vec<&str> = stripped.split('\n').collect();
    assert_eq!(lines[0], "plain", "unprefixed line kept verbatim");
    assert_eq!(lines[1], "+5 tail");
}

// --- edit_preview_input (renderers/edit.ts:64-85) ---------------------------

#[test]
fn edit_preview_input_prefers_path_over_file_path() {
    let args = serde_json::json!({
        "path": "a.txt",
        "file_path": "b.txt",
        "edits": [{ "oldText": "x", "newText": "y" }],
    });
    let input = edit_preview_input(&args).expect("input");
    assert_eq!(input.path, "a.txt");
    assert_eq!(input.edits, vec![("x".to_string(), "y".to_string(), false)]);
    assert_eq!(
        input.args_key,
        serde_json::json!({
            "path": "a.txt",
            "edits": [{ "oldText": "x", "newText": "y", "replaceAll": false }],
        })
        .to_string()
    );
}

#[test]
fn edit_preview_input_falls_back_to_file_path_and_legacy_pair() {
    let args = serde_json::json!({ "file_path": "b.txt", "oldText": "a", "newText": "b" });
    let input = edit_preview_input(&args).expect("input");
    assert_eq!(input.path, "b.txt");
    assert_eq!(input.edits, vec![("a".to_string(), "b".to_string(), false)]);
}

#[test]
fn edit_preview_input_keeps_replace_all() {
    let args = serde_json::json!({
        "path": "p",
        "edits": [{ "oldText": "a", "newText": "b", "replaceAll": true }],
    });
    let input = edit_preview_input(&args).expect("input");
    assert_eq!(input.edits, vec![("a".to_string(), "b".to_string(), true)]);
    assert!(
        input.args_key.contains("\"replaceAll\":true"),
        "replaceAll recorded in the args key: {}",
        input.args_key
    );
}

#[test]
fn edit_preview_input_rejects_empty_path_and_invalid_edits() {
    // Empty path is falsy upstream → no preview.
    assert!(edit_preview_input(&serde_json::json!({
        "path": "", "oldText": "a", "newText": "b"
    }))
    .is_none());
    // No path at all.
    assert!(edit_preview_input(&serde_json::json!({ "oldText": "a", "newText": "b" })).is_none());
    // A non-string edits[] entry is invalid → falls back to the legacy pair.
    let fallback = edit_preview_input(&serde_json::json!({
        "path": "p",
        "edits": [{ "oldText": 1, "newText": "y" }],
        "oldText": "a",
        "newText": "b",
    }))
    .expect("legacy fallback");
    assert_eq!(
        fallback.edits,
        vec![("a".to_string(), "b".to_string(), false)]
    );
    // Nothing usable → None.
    assert!(edit_preview_input(&serde_json::json!({ "path": "p" })).is_none());
}

// --- Elapsed / Took footers (renderers/bash.ts:104-110) ---------------------

#[test]
fn pending_shell_shows_elapsed_and_settled_shows_took() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "sleep 5" })),
        tools_expanded(),
    );
    component.set_meta(None, Some(Instant::now()), None);
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("Elapsed "),
        "pending footer shows the live clock: {rendered}"
    );

    // A recorded duration wins over the clock once settled.
    component.update_result(Arc::from("done"), false, Some(1500));
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("Took 1.5s"),
        "settled footer uses durationMs: {rendered}"
    );
    assert!(!rendered.contains("Elapsed"), "{rendered}");
}

#[test]
fn settled_shell_without_duration_falls_back_to_the_clock() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "sleep 1" })),
        tools_expanded(),
    );
    component.set_meta(None, Some(Instant::now()), None);
    component.update_result(Arc::from("done"), false, None);
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("Took "),
        "Took falls back to the startedAt clock: {rendered}"
    );
}

// --- codemode timing (renderers/codemode.ts + render-utils.ts, #10708) ------

/// The codemode row shows `Elapsed` while the script runs and the total
/// `Took` once it finished, like bash rows (upstream `toolTimerText`).
#[test]
fn codemode_row_shows_elapsed_while_running_and_took_once_finished() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = ToolExecutionComponent::new(
        "codemode",
        Arc::new(serde_json::json!({ "code": "return 1" })),
        tools_expanded(),
    );
    component.set_meta(None, Some(Instant::now()), None);
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("Elapsed "),
        "the running codemode row shows the live clock: {rendered}"
    );

    // The recorded duration wins once the script settled.
    component.update_result(Arc::from("Script completed"), false, Some(5100));
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("Took 5.1s"),
        "the finished codemode row shows the total: {rendered}"
    );
    assert!(!rendered.contains("Elapsed"), "{rendered}");
}

// --- edit block (renderers/edit.ts, renderShell: "self") --------------------

fn edit_component(args: serde_json::Value) -> ToolExecutionComponent {
    ToolExecutionComponent::new("edit", Arc::new(args), tools_expanded())
}

fn matching_preview(args: &serde_json::Value, diff: &str) -> Option<ToolPreview> {
    edit_preview_input(args).map(|input| ToolPreview {
        args_key: input.args_key,
        diff: Some(diff.to_string()),
        first_changed_line: Some(1),
        error: None,
    })
}

/// Every painted row of the edit block measures exactly the render width and
/// the blank spacer rows carry the block background (no stripes).
#[test]
fn edit_block_rows_paint_the_full_width_background() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old", "newText": "new" }],
    });
    let component = edit_component(args.clone());
    component.set_meta(
        None,
        Some(Instant::now()),
        matching_preview(&args, " 1 unchanged\n-1 old\n+1 new"),
    );
    component.update_result(Arc::from(""), false, None);
    let lines = component.render(WIDTH);
    for (row, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue; // the outer Spacer(1), intentionally unpainted
        }
        assert_eq!(visible_width(line), WIDTH, "row {row}: {line:?}");
        if line.trim().is_empty() {
            assert!(
                line.contains("\x1b[48;"),
                "blank row {row} carries the block background: {line:?}"
            );
        }
    }
    let rendered = strip_ansi(&lines.join("\n"));
    assert!(
        rendered.contains("+1 new"),
        "preview diff shown: {rendered}"
    );
}

/// A preview whose args key no longer matches the block args is stale and
/// must not be rendered (upstream `previewArgsKey` guard).
#[test]
fn edit_block_ignores_a_stale_preview() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old", "newText": "new" }],
    });
    let component = edit_component(args);
    component.set_meta(
        None,
        Some(Instant::now()),
        Some(ToolPreview {
            args_key: "{\"path\":\"other.rs\",\"edits\":[]}".to_string(),
            diff: Some("-1 stale\n+1 stale".to_string()),
            first_changed_line: Some(1),
            error: None,
        }),
    );
    let lines = component.render(WIDTH);
    let rendered = strip_ansi(&lines.join("\n"));
    assert!(
        !rendered.contains("stale"),
        "stale preview diff never renders: {rendered}"
    );
}

/// A resolved preview error renders inside the box instead of the diff.
#[test]
fn edit_block_renders_the_preview_error() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old", "newText": "new" }],
    });
    let component = edit_component(args.clone());
    component.set_meta(
        None,
        Some(Instant::now()),
        Some(ToolPreview {
            args_key: edit_preview_input(&args).expect("input").args_key,
            diff: None,
            first_changed_line: None,
            error: Some("oldText not found".to_string()),
        }),
    );
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("oldText not found"),
        "preview error shown: {rendered}"
    );
    assert!(
        !rendered.contains("+1"),
        "no diff when the preview errored: {rendered}"
    );
}

/// Legacy replays without stored details/preview keep the args-rebuilt body
/// inside the box.
#[test]
fn edit_block_legacy_replay_keeps_the_args_body() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [{ "oldText": "old", "newText": "new" }],
    });
    let component = edit_component(args);
    component.update_result(Arc::from("edited ok"), false, None);
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.contains("-old") && rendered.contains("+new"),
        "legacy diff body rebuilt from args: {rendered}"
    );
}

// --- renderShell: "default" (upstream ToolDefinition.renderShell) ----------

fn raw_tool(lines: &[&str], shell: Option<ToolRenderShell>, pending: bool) -> ChatEntry {
    ChatEntry::Raw {
        call_id: Some("call-shell".into()),
        lines: lines.iter().map(|line| line.to_string()).collect(),
        render: Some(RawRender::Tool(RawToolRender {
            call_id: Some("call-shell".into()),
            tool_name: "extension-tool".into(),
            args: Arc::new(serde_json::json!({})),
            output: Arc::from("out"),
            is_error: false,
            details: None,
            expanded: false,
            width: WIDTH,
            theme_generation: pir::tui::theme::Theme::generation(),
            shell,
            pending,
            images: Arc::new(Vec::new()),
            image_lines: Arc::new(Vec::new()),
        })),
    }
}

/// `renderShell: "default"` wraps the renderer's lines in the standard
/// colored tool shell (upstream `contentBox`): paddingX 1, paddingY 1 and the
/// phase background on every row.
#[test]
fn default_render_shell_paints_the_standard_tool_block() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let mut chat = ChatContainer::new();
    chat.set_entries(&[raw_tool(
        &["renderer row", ""],
        Some(ToolRenderShell::Default),
        false,
    )]);
    let rows = chat.render(WIDTH);
    let rendered = rows.join("\n");
    assert!(
        rendered.contains("renderer row"),
        "the renderer output stays visible: {rendered:?}"
    );
    assert!(
        rendered.contains("\x1b[48;"),
        "the default shell paints the tool background: {rendered:?}"
    );
    // Every shell row (padding and content) is background-painted and
    // measures the frame width; content rows carry the one-cell inset.
    let mut content_row = None;
    for (index, row) in rows.iter().enumerate() {
        let plain = strip_ansi(row);
        if !plain.trim().is_empty() {
            assert_eq!(
                visible_width(row),
                WIDTH,
                "shell row must fill the width: {row:?}"
            );
        }
        if plain.contains("renderer row") {
            content_row = Some(plain.clone());
        }
        if plain.trim().is_empty() && index > 0 {
            // Row 0 is the outer (unpainted) Spacer; every other blank row is
            // the painted Box padding row or an empty renderer row.
            assert!(
                row.contains("\x1b[48;"),
                "blank shell row must be painted: {row:?}"
            );
        }
    }
    let content_row = content_row.expect("content row rendered");
    assert!(
        content_row.starts_with(" renderer row"),
        "Box paddingX = 1: {content_row:?}"
    );
    assert_eq!(visible_width(&content_row), WIDTH);
}

/// `renderShell: "self"` (and a legacy `None`) keep the bare renderer lines:
/// no injected background or padding.
#[test]
fn self_render_shell_keeps_the_bare_lines() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    for shell in [Some(ToolRenderShell::SelfShell), None] {
        let mut chat = ChatContainer::new();
        chat.set_entries(&[raw_tool(&["self row"], shell, false)]);
        let rendered = chat.render(WIDTH).join("\n");
        assert!(
            rendered.contains("self row"),
            "shell {shell:?}: {rendered:?}"
        );
        assert!(
            !rendered.contains("\x1b[48;"),
            "shell {shell:?} must not inject the tool background: {rendered:?}"
        );
    }
}

/// A partial (`isPartial`) default-shell block paints `toolPendingBg`; the
/// settled phase flips to success/error without re-rendering the lines.
#[test]
fn default_render_shell_uses_the_pending_background_while_partial() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let pending_bg = pir::tui::theme::with_theme(|theme| {
        theme.bg(pir::tui::theme::ThemeColor::ToolPendingBg, "probe")
    });
    let pending_open = pending_bg
        .strip_suffix("\x1b[49m")
        .expect("background painter closer")
        .trim_end_matches("probe");
    assert!(pending_open.contains("\x1b[48;"));
    let mut chat = ChatContainer::new();
    chat.set_entries(&[raw_tool(&["partial"], Some(ToolRenderShell::Default), true)]);
    let rendered = chat.render(WIDTH).join("\n");
    assert!(
        rendered.contains(pending_open),
        "the pending shell uses toolPendingBg: {rendered:?}"
    );
}
