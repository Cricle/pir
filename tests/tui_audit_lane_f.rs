//! TUI lane F audit regressions: the transcript window budget must count the
//! payloads a `ChatEntry` actually owns.
//!
//! `entry_payload_bytes` is the proxy the 100k-line transcript window sums.
//! It used to count only `tool_name + output` for a tool block (and only the
//! call name for an assistant `ToolCall`), so a `write`/`edit` call holding a
//! multi-megabyte argument body — or a result `details`/edit preview holding
//! another copy — was treated as a few dozen bytes and materialized far past
//! the window budget. Images had the same shape: a flat 1 KiB per block
//! ignored the base64 payload the entry keeps.

use std::collections::BTreeMap;
use std::sync::Arc;

use pir::ai::types::{AssistantMessage, ContentBlock, ImageContent, TextContent, ToolCall};
use pir::tui::app::ChatEntry;
use pir::tui::terminal::{entry_payload_bytes, tail_window_start};

fn tool_entry(args: serde_json::Value, output: &str) -> ChatEntry {
    ChatEntry::ToolExecution {
        call_id: Some("call-1".to_string()),
        tool_name: "write".to_string(),
        args: Arc::new(args),
        output: Arc::from(output),
        is_error: false,
        duration_ms: None,
        pending: false,
        details: None,
        started_at: None,
        preview: None,
        images: Arc::new(Vec::new()),
    }
}

#[test]
fn tool_call_arguments_are_counted_by_the_window_budget() {
    let body = "x".repeat(200_000);
    let args = serde_json::json!({ "path": "src/main.rs", "content": body });
    let entry = tool_entry(args, "");
    let bytes = entry_payload_bytes(&entry);
    assert!(
        bytes >= 200_000,
        "a write call's content argument must count toward the window budget: {bytes}"
    );
}

#[test]
fn tool_details_preview_and_images_are_counted() {
    let output = "o".repeat(10_000);
    let details = Arc::new(serde_json::json!({ "fullOutput": "d".repeat(50_000) }));
    let image = ImageContent {
        data: "i".repeat(40_000),
        mime_type: "image/png".to_string(),
    };
    let entry = ChatEntry::ToolExecution {
        call_id: Some("call-2".to_string()),
        tool_name: "bash".to_string(),
        args: Arc::new(serde_json::json!({ "command": "run" })),
        output: Arc::from(output),
        is_error: false,
        duration_ms: Some(1),
        pending: false,
        details: Some(details),
        started_at: None,
        preview: Some(pir::tui::app::ToolPreview {
            args_key: "k".repeat(1_000),
            diff: Some("f".repeat(30_000)),
            first_changed_line: Some(1),
            error: None,
        }),
        images: Arc::new(vec![image]),
    };
    let bytes = entry_payload_bytes(&entry);
    assert!(
        bytes >= 10_000 + 50_000 + 40_000 + 30_000 + 1_000,
        "output + details + image data + preview must all be in the budget: {bytes}"
    );
}

#[test]
fn assistant_tool_call_and_image_payloads_are_counted() {
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "content".to_string(),
        serde_json::json!("y".repeat(120_000)),
    );
    let message = AssistantMessage {
        content: vec![
            ContentBlock::ToolCall(ToolCall {
                id: "call-3".to_string(),
                name: "write".to_string(),
                arguments,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
            ContentBlock::Image(ImageContent {
                data: "z".repeat(80_000),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Text(TextContent {
                text: "done".to_string(),
                text_signature: None,
            }),
        ],
        ..Default::default()
    };
    let bytes = entry_payload_bytes(&ChatEntry::Assistant(Arc::new(message)));
    assert!(
        bytes >= 120_000 + 80_000,
        "assistant tool-call arguments and image data must count: {bytes}"
    );
}

#[test]
fn tail_window_skips_an_entry_with_a_large_argument_body() {
    // The huge write call is OLD; the tiny newer entries fit the budget. With
    // the arguments counted the budget binds at the huge entry and the window
    // starts after it; the old shallow count would have pulled it in.
    let mut entries = vec![tool_entry(
        serde_json::json!({ "path": "src/big.rs", "content": "q".repeat(2_000_000) }),
        "",
    )];
    for index in 0..4 {
        entries.push(ChatEntry::User(format!("newer message {index}")));
    }
    let start = tail_window_start(&entries, 0, 512, 16 * 1024);
    assert_eq!(
        start, 1,
        "the 2 MB argument body must keep the old entry out of the tail window"
    );
}
