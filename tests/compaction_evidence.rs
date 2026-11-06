//! Integration tests for the deterministic compaction evidence block.

use std::collections::BTreeMap;

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ToolCall, ToolResultMessage,
    ToolResultRole, Usage,
};
use pir::compaction::evidence::{
    collect_evidence, format_evidence_block, EVIDENCE_HEADER, MAX_EVIDENCE_BYTES,
};

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant_with_call(
    id: &str,
    name: &str,
    arguments: BTreeMap<String, serde_json::Value>,
) -> Message {
    Message::Assistant(AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        timestamp: 2,
        ..Default::default()
    })
}

fn path_args(path: &str) -> BTreeMap<String, serde_json::Value> {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_string(), serde_json::json!(path));
    arguments
}

fn command_args(command: &str) -> BTreeMap<String, serde_json::Value> {
    let mut arguments = BTreeMap::new();
    arguments.insert("command".to_string(), serde_json::json!(command));
    arguments
}

fn tool_result(call_id: &str, tool_name: &str, text: &str, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: tool_name.into(),
        content: vec![text_block(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error,
        duration_ms: None,
        timestamp: 3,
    })
}

#[test]
fn evidence_block_collects_files_commands_and_failures() {
    let messages = vec![
        assistant_with_call("r1", "read", path_args("README.md")),
        tool_result("r1", "read", "# readme", false),
        assistant_with_call("e1", "edit", path_args("src/lib.rs")),
        tool_result("e1", "edit", "edit applied", false),
        assistant_with_call("b1", "bash", command_args("cargo check")),
        tool_result(
            "b1",
            "bash",
            "Checking pir v0.1.0\nFinished dev profile\n\nCommand exited with code 0",
            false,
        ),
        assistant_with_call(
            "b2",
            "bash",
            command_args("cargo test --test compaction_evidence"),
        ),
        tool_result(
            "b2",
            "bash",
            "test result: FAILED. 0 passed\n\nCommand exited with code 101",
            true,
        ),
    ];

    let block = collect_evidence(&messages);
    let rendered = format_evidence_block(&block);

    assert!(rendered.contains(EVIDENCE_HEADER), "{rendered}");
    assert!(rendered.contains("### Modified files"), "{rendered}");
    assert!(rendered.contains("- src/lib.rs"), "{rendered}");
    assert!(rendered.contains("### Read files"), "{rendered}");
    assert!(rendered.contains("- README.md"), "{rendered}");
    assert!(rendered.contains("### Commands & results"), "{rendered}");
    assert!(rendered.contains("`cargo check` -> exit 0"), "{rendered}");
    assert!(
        rendered.contains("`cargo test --test compaction_evidence` -> exit 101"),
        "{rendered}"
    );
    assert!(rendered.contains("### Failed checks"), "{rendered}");
    assert!(
        rendered.contains("cargo test --test compaction_evidence: exit 101"),
        "{rendered}"
    );
}

#[test]
fn empty_messages_return_empty_string() {
    let block = collect_evidence(&[]);
    assert_eq!(format_evidence_block(&block), "");
}

#[test]
fn oversized_evidence_is_truncated_to_8_kib() {
    let mut messages = Vec::new();
    for index in 0..1000 {
        let id = format!("r{index}");
        let path = format!("src/generated/very_long_module_name_{index:04}_file.rs");
        messages.push(assistant_with_call(&id, "read", path_args(&path)));
    }

    let rendered = format_evidence_block(&collect_evidence(&messages));
    assert!(
        rendered.starts_with("\n\n## Evidence (deterministic)"),
        "{rendered}"
    );
    assert!(
        rendered.len() <= MAX_EVIDENCE_BYTES,
        "len={}",
        rendered.len()
    );
    assert!(rendered.contains("[Evidence truncated"), "{rendered}");
    // Still valid UTF-8 by construction; ensure the cut was not mid-char.
    assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
}
