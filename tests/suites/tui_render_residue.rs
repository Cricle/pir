//! TUI residue regression tests: every frame line must measure exactly the
//! render width.
//!
//! A width+1 line wraps in the terminal; the wrap inserts an extra physical
//! row that the diff renderer's one-line-per-screen-row bookkeeping
//! (`hardware_cursor_row` / `previous_viewport_top`) cannot see, leaving
//! stale/duplicated rows on screen. Two historical sources: assistant
//! markdown lines padded to `width` and then prefixed with `outputPad`
//! spaces again, and tool-box spacer rows painted with a single background
//! cell instead of the full width.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pir::ai::types::{AssistantMessage, ContentBlock, TextContent};
use pir::tui::ansi::{strip_ansi, visible_width};
use pir::tui::core::{Component, Terminal, TuiCore};
use pir::tui::editor::Editor;
use pir::tui::messages::{AssistantMessageComponent, ToolExecutionComponent};
use pir::tui::theme::set_theme;

const WIDTH: usize = 40;

/// Upstream `SEGMENT_RESET` (tui.ts:423).
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

struct StaticLines(&'static [&'static str]);

impl Component for StaticLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.iter().map(|s| s.to_string()).collect()
    }
}

fn assistant_component(blocks: Vec<ContentBlock>, output_pad: usize) -> AssistantMessageComponent {
    let message = AssistantMessage {
        content: blocks,
        ..AssistantMessage::default()
    };
    AssistantMessageComponent::new_from(Arc::new(message), false, output_pad)
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

/// Assistant text renders through Markdown with `outputPad` as its own
/// paddingX (upstream `new Markdown(text, outputPad, 0, …)`); every line
/// comes back full-width padded and must not be prefixed a second time.
#[test]
fn assistant_markdown_lines_measure_exactly_the_render_width() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = assistant_component(
        vec![text_block(
            "hello **world**\n\nsecond paragraph with `code`\n\n- item\n- item",
        )],
        1,
    );
    let lines = component.render(WIDTH);
    assert!(lines.len() > 3, "the message rendered content: {lines:?}");
    for line in &lines {
        if line.is_empty() {
            continue; // the leading Spacer(1)
        }
        assert_eq!(visible_width(line), WIDTH, "line {line:?}");
    }
}

/// Long unbroken tokens wrap to the padded content width, and the final
/// piece still pads to exactly the full width.
#[test]
fn assistant_wrapped_lines_measure_exactly_the_render_width() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = assistant_component(vec![text_block(&"x".repeat(3 * WIDTH + 7))], 1);
    let lines = component.render(WIDTH);
    for line in &lines {
        if line.is_empty() {
            continue;
        }
        assert_eq!(visible_width(line), WIDTH, "line {line:?}");
    }
}

/// `outputPad: 0` (settings `outputPad = 0`) still renders exact-width
/// lines — the padding lives in Markdown, not in a prefix.
#[test]
fn assistant_lines_with_zero_output_pad_measure_exactly_the_render_width() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = assistant_component(vec![text_block("plain text line")], 0);
    for line in component.render(WIDTH) {
        if line.is_empty() {
            continue;
        }
        assert_eq!(visible_width(&line), WIDTH, "line {line:?}");
    }
}

/// Tool-box rows: the block background must span the full width on every
/// row of the box — including the spacer rows (blank-looking). A 1-cell
/// painted spacer shows as a horizontal stripe of terminal-default
/// background inside the block.
#[test]
fn tool_box_rows_paint_the_full_width_background() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "echo one; echo two" })),
        Arc::new(AtomicBool::new(false)),
    );
    component.update_result(Arc::from("one\ntwo"), false, Some(320));
    let lines = component.render(WIDTH);
    for line in &lines {
        if line.is_empty() {
            continue; // the outer Spacer(1), intentionally unpainted
        }
        assert_eq!(visible_width(line), WIDTH, "box row {line:?}");
        if line.trim().is_empty() {
            assert!(
                line.contains("\x1b[48;"),
                "blank box row carries the block background: {line:?}"
            );
        }
    }
}

/// Expanded shell output matches upstream (`new Text(`\n${styledOutput}`)`):
/// blank line + all output lines, with NO collapse-key hint (that hint
/// belongs to the `!` user-bash block only, bash-execution.ts).
#[test]
fn expanded_shell_output_has_no_collapse_hint() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "seq 1 12" })),
        Arc::new(AtomicBool::new(true)),
    );
    let output = (1..=12)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    component.update_result(Arc::from(output.as_str()), false, Some(420));
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(
        rendered.lines().any(|line| line.trim() == "1")
            && rendered.lines().any(|line| line.trim() == "12"),
        "output shown expanded: {rendered}"
    );
    assert!(
        !rendered.contains("to collapse"),
        "no collapse hint in the tool box: {rendered}"
    );
    assert!(
        !rendered.contains("to expand"),
        "nothing is hidden while expanded: {rendered}"
    );
}

/// The pending block shows the call line; no result rows before the result
/// arrives (batch 5 layers the live `Elapsed` clock on top).
#[test]
fn pending_shell_block_renders_call_line_only() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let component = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({ "command": "sleep 1" })),
        Arc::new(AtomicBool::new(false)),
    );
    let rendered = strip_ansi(&component.render(WIDTH).join("\n"));
    assert!(rendered.contains("$ sleep 1"), "call line: {rendered}");
    assert!(!rendered.contains("Took"), "no result tail while pending");
}

/// Every painted frame line ends with the segment reset (`ESC[0m` + OSC-8
/// close, upstream `applyLineResets`): the diff renderer erases rows with
/// `2K`, which clears cells but not attributes, so an unterminated style in
/// one row would bleed into the next row's clear.
#[test]
fn painted_frame_lines_end_with_the_segment_reset() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let mut core = TuiCore::new(Terminal::new());
    core.push_content(Box::new(StaticLines(&["alpha", "beta"])));
    core.render();
    let painted = core.last_screen_lines();
    assert_eq!(painted.len(), 2);
    for line in &painted {
        assert!(
            line.ends_with(SEGMENT_RESET),
            "line must end with the segment reset: {line:?}"
        );
    }
    // The search corpus (`last_frame`) stays plain — no reset suffix.
    for line in core.last_frame_lines() {
        assert!(
            !line.contains(SEGMENT_RESET),
            "corpus stays plain: {line:?}"
        );
    }
}

/// The hardware cursor is hidden by default (the editor's reverse-video
/// cell is the visible cursor; upstream `showHardwareCursor` defaults off)
/// and shown at the editor cell when enabled.
#[test]
fn hardware_cursor_hidden_by_default_and_shown_when_enabled() {
    let mut core = TuiCore::new(Terminal::new());
    core.push_content(Box::new(StaticLines(&["frame"])));
    core.render();
    assert!(!core.cursor_visible(), "hidden by default");

    let cell = Arc::new(std::sync::Mutex::new(Some((0usize, 3usize))));
    core.set_dock_cursor_cell(Arc::clone(&cell));
    core.set_dock_height(1);
    core.set_show_hardware_cursor(true);
    core.push_content(Box::new(StaticLines(&["changed"])));
    core.render();
    assert!(core.cursor_visible(), "shown when showHardwareCursor is on");

    // A selector overlay replaces the editor: no cursor cell → hidden again.
    *cell.lock().unwrap() = None;
    core.push_content(Box::new(StaticLines(&["again"])));
    core.render();
    assert!(
        !core.cursor_visible(),
        "hidden while no editor cell is recorded"
    );
}

/// With the hardware cursor shown the editor drops its reverse-video fake
/// cursor (the terminal cursor is drawn at the cell); the cell keeps its
/// layout and the cursor column is unchanged.
#[test]
fn editor_fake_cursor_suppressed_when_hardware_cursor_shown() {
    let editor = Editor::new();
    editor.set_text("abc");
    let (lines, _row, col) = editor.render_with_cursor_rows(20, 3);
    assert!(
        lines.iter().any(|line| line.contains("\x1b[7m")),
        "fake cursor by default: {lines:?}"
    );

    editor.set_show_hardware_cursor(true);
    let (lines, _row, col_hw) = editor.render_with_cursor_rows(20, 3);
    assert!(
        !lines.iter().any(|line| line.contains("\x1b[7m")),
        "no fake cursor with the hardware cursor shown: {lines:?}"
    );
    assert_eq!(col, col_hw, "cursor cell position is unchanged");
    for line in &lines {
        assert_eq!(visible_width(line), 20, "row width unchanged: {line:?}");
    }
}
