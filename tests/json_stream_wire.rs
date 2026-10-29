//! Pins the JSON/RPC assistant-stream wire while the per-delta path stops
//! serializing the cumulative `partial` snapshot.
//!
//! Both `--mode json` (`coding_agent/json.rs::to_json_event`) and the RPC host
//! (same function) emit `assistantMessageEvent` without `partial`. The old
//! construction serialized the derived event into a `Value` tree — cumulative
//! reply included — and then removed the field, so every delta paid
//! O(accumulated reply). The borrowed wire view forwards every field except
//! `partial`; these tests lock the resulting bytes:
//!
//! - every variant keeps today's exact shape and key order (the reference is
//!   the derive-based construction with `partial` removed);
//! - a large accumulated snapshot never reaches the JSON-mode wire;
//! - `agent_event_to_json` carries the snapshot once (as `message`), never a
//!   second time inside `assistantMessageEvent`.

use std::collections::BTreeMap;
use std::sync::Arc;

use pir::agent::agent_loop::AgentEvent;
use pir::agent_session::agent_event_to_json;
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, ErrorReason, TerminalReason,
    TextContent, ThinkingContent, ToolCall, Usage,
};
use pir::coding_agent::json::to_json_event;
use serde_json::{json, Value};

fn message_with(content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "test-api".to_string(),
        provider: "test-provider".to_string(),
        model: "test-model".to_string(),
        usage: Usage {
            input: 1,
            output: 2,
            total_tokens: 10,
            ..Usage::default()
        },
        ..AssistantMessage::default()
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

fn thinking_block(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: thinking.to_string(),
        thinking_signature: None,
        redacted: false,
    })
}

fn tool_call(id: &str, name: &str, path: &str) -> ToolCall {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_string(), json!(path));
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }
}

fn tool_block(id: &str, name: &str, path: &str) -> ContentBlock {
    ContentBlock::ToolCall(tool_call(id, name, path))
}

/// The `assistantMessageEvent` subtree of the JSON-mode/RPC wire event.
fn wire_event(event: &AssistantMessageEvent) -> Value {
    let agent = AgentEvent::StreamEvent {
        turn: 0,
        event: event.clone(),
    };
    let value = to_json_event(&agent);
    assert_eq!(value["type"], "message_update");
    assert_eq!(value["turn"], 0);
    assert!(
        value["usage"].is_object(),
        "every message_update hoists its usage"
    );
    value["assistantMessageEvent"].clone()
}

fn assert_wire(event: &AssistantMessageEvent, expected: Value) {
    let actual = wire_event(event);
    assert!(
        actual.get("partial").is_none(),
        "partial must never be on the wire: {actual}"
    );
    assert_eq!(actual, expected, "wire shape drifted");
    // Embedders diff serialized bytes, so key ORDER is part of the contract.
    assert_eq!(
        serde_json::to_string(&actual).unwrap(),
        serde_json::to_string(&expected).unwrap(),
        "wire bytes (and field order) drifted for {}",
        actual["type"]
    );
}

/// The events covering every `AssistantMessageEvent` variant, with a partial
/// that carries a tool call at `content_index` 2 (for `toolcall_start`'s
/// derived `id`/`toolName`).
fn variants() -> Vec<AssistantMessageEvent> {
    let partial = Arc::new(message_with(vec![
        text_block("hello"),
        thinking_block("hmm"),
        tool_block("call-1", "read", "src/main.rs"),
    ]));
    let terminal = message_with(vec![text_block("done")]);
    let failed = message_with(vec![]);
    vec![
        AssistantMessageEvent::Start {
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hello".into(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "hello".into(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ThinkingStart {
            content_index: 1,
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ThinkingDelta {
            content_index: 1,
            delta: "hmm".into(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ThinkingEnd {
            content_index: 1,
            content: "hmm".into(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ToolCallStart {
            content_index: 2,
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ToolCallDelta {
            content_index: 2,
            delta: "{\"path\":".into(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ToolCallEnd {
            content_index: 2,
            tool_call: tool_call("call-1", "read", "src/main.rs"),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::Done {
            reason: TerminalReason::Stop,
            message: terminal,
        },
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: failed,
        },
    ]
}

#[test]
fn every_assistant_event_keeps_todays_exact_wire_shape() {
    let partial = Arc::new(message_with(vec![
        text_block("hello"),
        thinking_block("hmm"),
        tool_block("call-1", "read", "src/main.rs"),
    ]));
    let terminal = message_with(vec![text_block("done")]);
    let failed = message_with(vec![]);

    assert_wire(
        &AssistantMessageEvent::Start {
            partial: Arc::clone(&partial),
        },
        json!({ "type": "start" }),
    );
    assert_wire(
        &AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::clone(&partial),
        },
        json!({ "type": "text_start", "content_index": 0 }),
    );
    assert_wire(
        &AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hello".to_string(),
            partial: Arc::clone(&partial),
        },
        json!({ "type": "text_delta", "content_index": 0, "delta": "hello" }),
    );
    assert_wire(
        &AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "hello".to_string(),
            partial: Arc::clone(&partial),
        },
        json!({ "type": "text_end", "content_index": 0, "content": "hello" }),
    );
    assert_wire(
        &AssistantMessageEvent::ThinkingStart {
            content_index: 1,
            partial: Arc::clone(&partial),
        },
        json!({ "type": "thinking_start", "content_index": 1 }),
    );
    assert_wire(
        &AssistantMessageEvent::ThinkingDelta {
            content_index: 1,
            delta: "hmm".to_string(),
            partial: Arc::clone(&partial),
        },
        json!({ "type": "thinking_delta", "content_index": 1, "delta": "hmm" }),
    );
    assert_wire(
        &AssistantMessageEvent::ThinkingEnd {
            content_index: 1,
            content: "hmm".to_string(),
            partial: Arc::clone(&partial),
        },
        json!({ "type": "thinking_end", "content_index": 1, "content": "hmm" }),
    );
    assert_wire(
        &AssistantMessageEvent::ToolCallStart {
            content_index: 2,
            partial: Arc::clone(&partial),
        },
        json!({
            "type": "toolcall_start",
            "content_index": 2,
            "id": "call-1",
            "toolName": "read",
        }),
    );
    assert_wire(
        &AssistantMessageEvent::ToolCallDelta {
            content_index: 2,
            delta: "{\"path\":".to_string(),
            partial: Arc::clone(&partial),
        },
        json!({ "type": "toolcall_delta", "content_index": 2, "delta": "{\"path\":" }),
    );
    assert_wire(
        &AssistantMessageEvent::ToolCallEnd {
            content_index: 2,
            tool_call: tool_call("call-1", "read", "src/main.rs"),
            partial: Arc::clone(&partial),
        },
        json!({
            "type": "toolcall_end",
            "content_index": 2,
            "tool_call": {
                "type": "toolCall",
                "id": "call-1",
                "name": "read",
                "arguments": { "path": "src/main.rs" },
            },
        }),
    );
    assert_wire(
        &AssistantMessageEvent::Done {
            reason: TerminalReason::Stop,
            message: terminal.clone(),
        },
        json!({
            "type": "done",
            "reason": "stop",
            "message": &terminal,
        }),
    );
    assert_wire(
        &AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: failed.clone(),
        },
        json!({
            "type": "error",
            "reason": "error",
            "error": &failed,
        }),
    );
}

#[test]
fn every_assistant_event_matches_the_derive_minus_partial() {
    // `message_update_payload` is the reference construction: the fully
    // derived event tree (partial included). Removing `partial` yields exactly
    // the bytes the pre-fix wire emitted for every variant.
    for event in variants() {
        let reference = pir::agent::agent_loop::message_update_payload(&event);
        let mut expected = reference["assistantMessageEvent"].clone();
        let expected_object = expected
            .as_object_mut()
            .expect("assistantMessageEvent is an object");
        expected_object.remove("partial");
        // The JSON-mode post-processing adds the tool call's identity to
        // `toolcall_start` because the snapshot that held it is stripped.
        if let AssistantMessageEvent::ToolCallStart {
            content_index,
            partial,
        } = &event
        {
            if let Some(ContentBlock::ToolCall(call)) = partial.content.get(*content_index) {
                expected_object.insert("id".into(), json!(call.id));
                expected_object.insert("toolName".into(), json!(call.name));
            }
        }
        let actual = wire_event(&event);
        assert_eq!(actual, expected, "shape drifted for {}", event_name(&event));
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "bytes/order drifted for {}",
            event_name(&event)
        );
    }
}

fn event_name(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

#[test]
fn json_mode_delta_is_bounded_by_the_delta_not_the_accumulated_reply() {
    // 1 MiB of accumulated text carried by the partial snapshot. The whole
    // reply must not be serialized for a per-delta event.
    let marker_payload = format!("MARKER{}", "x".repeat(1024 * 1024));
    let partial = Arc::new(message_with(vec![text_block(&marker_payload)]));
    let agent = AgentEvent::StreamEvent {
        turn: 0,
        event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "y".into(),
            partial,
        },
    };
    let out = serde_json::to_string(&to_json_event(&agent)).expect("wire serializes");
    assert!(
        !out.contains("MARKER"),
        "the JSON-mode wire serialized the cumulative partial"
    );
    assert!(
        out.len() < 4096,
        "per-delta output must stay O(delta), got {} bytes",
        out.len()
    );
}

#[test]
fn agent_event_carries_the_snapshot_once_as_message() {
    let marker_payload = format!("MARKER{}", "x".repeat(1024 * 1024));
    let partial = Arc::new(message_with(vec![text_block(&marker_payload)]));
    let expected_message = serde_json::to_value(&*partial).expect("snapshot serializes");
    let agent = AgentEvent::StreamEvent {
        turn: 0,
        event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "y".into(),
            partial,
        },
    };

    let value = agent_event_to_json(&agent);
    assert_eq!(value["message"], expected_message);
    assert!(
        value["assistantMessageEvent"].get("partial").is_none(),
        "assistantMessageEvent must not repeat the cumulative snapshot"
    );
    assert_eq!(
        value["assistantMessageEvent"],
        to_json_event(&agent)["assistantMessageEvent"],
        "the embedder and JSON-mode event views must agree"
    );

    let out = serde_json::to_string(&value).expect("wire serializes");
    assert_eq!(
        out.matches("MARKER").count(),
        1,
        "the snapshot rides once (as `message`), not again inside assistantMessageEvent"
    );
}
