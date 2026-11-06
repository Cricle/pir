//! Regression: deep, model/MCP-controlled JSON and Markdown must render in the
//! TUI worker's stack budget without overflowing it (incident
//! `2026-11-06T0822-mcp-tui-stack-overflow`).
//!
//! These cases pin the three halves of the fix:
//! - every thread that hosts an `AgentSession` (TUI worker, server prompt,
//!   SDK prompt stream) declares an explicit 12–16 MiB stack, and the source
//!   guard below fails if that `.stack_size(..)` application is removed;
//! - tool args/details and Markdown are depth-limited/iterative before
//!   rendering, so a pathological payload cannot consume an unbounded number
//!   of frames;
//! - deep `serde_json::Value` trees are released iteratively instead of
//!   through serde_json's recursive `Drop`.
//!
//! Runs as its own integration target serialized by the caller:
//! `cargo test --test tui_mcp_stack_overflow -- --test-threads=1`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pir::ai::types::{AssistantMessage, ContentBlock, TextContent};
use pir::tui::core::Component;
use pir::tui::messages::{AssistantMessageComponent, ToolExecutionComponent};

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    }
}

/// `depth` nested single-element arrays around `leaf`.
fn nested_json(depth: usize, leaf: serde_json::Value) -> serde_json::Value {
    let mut value = leaf;
    for _ in 0..depth {
        value = serde_json::Value::Array(vec![value]);
    }
    value
}

/// Run one case on a thread with the pre-fix platform default stack (2 MiB).
/// A stack overflow on Windows aborts the process (it is not a catchable
/// panic), so `join()` succeeding is the actual overflow assertion.
fn on_two_mib_stack(name: &str, case: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(2 * 1024 * 1024)
        .spawn(case)
        .expect("spawn deep-render thread")
        .join()
        .expect("deep render must not panic");
}

#[test]
fn deep_mcp_result_renders_without_stack_overflow() {
    // 8k levels is far past serde_json's *deserialize* recursion limit but is
    // exactly the shape a tool can build programmatically (MCP structured
    // content is a `serde_json::Value`). The render-only sanitizer must cut
    // it at MAX_JSON_RENDER_DEPTH instead of recursing 8k times, and the
    // original tree must be released iteratively (no `mem::forget` escape
    // hatch is used here: dropping a deep `Value` is itself recursive).
    on_two_mib_stack("tui-mcp-deep-render", || {
        let args = Arc::new(nested_json(
            8_000,
            serde_json::json!({"text": "hello from mcp"}),
        ));
        let details = Arc::new(nested_json(
            8_000,
            serde_json::json!({"server": "qa", "tool": "qa_echo"}),
        ));
        // Expanded so the bounded args formatter prints the whole 64-level
        // prefix plus the elision marker; the collapsed preview truncates at
        // 100 chars and would hide the marker behind the opening brackets.
        let tool = ToolExecutionComponent::new(
            "mcp__qa__qa_echo",
            args,
            Arc::new(AtomicBool::new(true)),
        );
        tool.set_meta(Some(details), None, None);
        tool.update_result(Arc::from("MCP_ECHO:hello from mcp"), false, None);
        // The formatter itself (before line wrapping) carries the complete
        // visible elision marker.
        let direct = pir::tui::messages::format_tool_call_line(
            "mcp__qa__qa_echo",
            &nested_json(8_000, serde_json::json!({"text": "hello from mcp"})),
            true,
        );
        assert!(
            direct.contains("nested JSON truncated for display"),
            "the bounded args formatter lost the elision marker (len {})",
            direct.len()
        );

        let rows = tool.render(160);
        let joined = rows.join("\n");
        assert!(!rows.is_empty(), "the deep MCP result rendered no rows");
        assert!(
            joined.contains("MCP_ECHO:hello from mcp"),
            "the tool result text is missing from the render: {joined:.400}"
        );
        // At a 160-column terminal the marker wraps across two rows; both
        // halves must be present (and not silently replaced by an empty tree).
        assert!(
            joined.contains("nested JSON") && joined.contains("truncated for display"),
            "the deep args tree was not visibly elided at MAX_JSON_RENDER_DEPTH: {joined:.400}"
        );
    });
}

#[test]
fn deep_multiline_blockquote_degrades_to_text() {
    // The old cap inspected only the FIRST line of a quote run. Progressive
    // nesting (`>` on line 1, `> >` on line 2, …) strips one marker per
    // parse, so every recursion level looked shallow; 1000 lines overflowed
    // a 2 MiB stack. The whole-run max-depth check must degrade the run to a
    // paragraph without recursing.
    on_two_mib_stack("tui-deep-blockquote", || {
        let mut text = String::new();
        for line in 0..1_000usize {
            text.push_str(&"> ".repeat(line + 1));
            text.push_str(&format!("deep quote {line}\n"));
        }
        text.push_str("plain tail marker\n");

        let message =
            AssistantMessageComponent::new_from(Arc::new(assistant_message(&text)), false, 1);
        let rows = message.render(100);
        let joined = rows.join("\n");
        assert!(!rows.is_empty(), "the deep blockquote rendered no rows");
        assert!(
            joined.contains("deep quote 999"),
            "the deepest blockquote line is missing: {joined:.400}"
        );
        assert!(
            joined.contains("plain tail marker"),
            "the document tail after the quote is missing: {joined:.400}"
        );
    });
}

#[test]
fn deep_latex_and_inline_nesting_degrade_to_text() {
    // LaTeX `{...}` groups recurse through `parse_sequence` once per brace
    // (4000 levels overflowed 2 MiB); inline links recurse through
    // `render_inline_at` (2000 levels). Both must stop at their depth cap and
    // keep the text visible.
    on_two_mib_stack("tui-deep-latex-inline", || {
        let mut braces = String::new();
        braces.push_str("$$");
        braces.push_str(&"{".repeat(4_000));
        braces.push_str("latex_deep_leaf");
        braces.push_str(&"}".repeat(4_000));
        braces.push_str("$$\n\n");

        let links = format!("{}focus_leaf{}", "[".repeat(2_000), "](u)".repeat(2_000));
        let text = format!("{braces}{links}");
        let message =
            AssistantMessageComponent::new_from(Arc::new(assistant_message(&text)), false, 1);
        let rows = message.render(100);
        let joined = rows.join("\n");
        assert!(
            !rows.is_empty(),
            "the deep LaTeX/inline document rendered no rows"
        );
        assert!(
            joined.contains("latex_deep_leaf"),
            "the deep LaTeX expression was not kept as raw text: {joined:.400}"
        );
        assert!(
            joined.contains("focus_leaf"),
            "the deeply nested inline link lost its text: {joined:.400}"
        );
    });
}

#[test]
fn deep_value_drop_is_iterative() {
    // 100k nested array levels overflow `serde_json::Value::drop` on a 2 MiB
    // stack (and on the old 12 MiB worker). Building the tree is iterative;
    // constructing the component must consume the original owner through the
    // iterative release instead of dropping it recursively.
    on_two_mib_stack("tui-deep-value-drop", || {
        let args = Arc::new(nested_json(
            100_000,
            serde_json::json!({"text": "deep-drop-leaf"}),
        ));
        let tool = ToolExecutionComponent::new(
            "mcp__qa__deep_drop",
            args,
            // Expanded: the marker is the render-content assertion.
            Arc::new(AtomicBool::new(true)),
        );
        let rows = tool.render(160);
        let joined = rows.join("\n");
        assert!(!rows.is_empty(), "the deep Value rendered no rows");
        assert!(
            joined.contains("nested JSON") && joined.contains("truncated for display"),
            "the deep Value was not visibly elided: {joined:.400}"
        );
        // `tool` (and its shallow sanitized state) drops here on the 2 MiB
        // thread; the original 100k tree was already released by `new`.
    });
}

#[test]
fn agent_thread_stack_budgets_are_wired() {
    const TWELVE_MIB: usize = 12 * 1024 * 1024;
    const SIXTEEN_MIB: usize = 16 * 1024 * 1024;

    // The constants are the single source of truth for each spawn site.
    assert!(pir::tui::terminal::WORKER_THREAD_STACK_BYTES >= TWELVE_MIB);
    assert!(pir::tui::terminal::WORKER_THREAD_STACK_BYTES <= SIXTEEN_MIB);
    assert!(pir::server::SERVER_AGENT_TASK_STACK_BYTES >= TWELVE_MIB);
    assert!(pir::server::SERVER_AGENT_TASK_STACK_BYTES <= SIXTEEN_MIB);
    assert!(pir::sdk::SDK_AGENT_TASK_STACK_BYTES >= TWELVE_MIB);
    assert!(pir::sdk::SDK_AGENT_TASK_STACK_BYTES <= SIXTEEN_MIB);

    // Sensitivity guard: these assertions fail if the `.stack_size(..)`
    // application is deleted from any agent-hosting spawn site, which the
    // behavioral cases cannot detect (they run on their own test threads).
    let terminal = include_str!("../src/tui/terminal.rs");
    assert!(
        terminal.contains(".stack_size(WORKER_THREAD_STACK_BYTES)"),
        "pir-tui-worker spawn lost its explicit stack_size"
    );
    let server = include_str!("../src/server/session.rs");
    assert!(
        server.contains(".stack_size(SERVER_AGENT_TASK_STACK_BYTES)"),
        "server agent task lost its explicit stack_size"
    );
    let sdk = include_str!("../src/sdk/agent.rs");
    assert!(
        sdk.contains(".stack_size(SDK_AGENT_TASK_STACK_BYTES)"),
        "SDK agent task lost its explicit stack_size"
    );
    // And the server/SDK agent runs must use the dedicated helper, not a
    // runtime-worker `tokio::spawn`.
    assert!(server.contains("spawn_agent_task(async move"));
    assert!(sdk.contains("spawn_agent_task(async move"));
}
