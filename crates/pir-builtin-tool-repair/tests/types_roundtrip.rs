//! Neutral DTO wire-shape tests: a core-shaped assistant message must
//! round-trip through the package's DTO without losing provider fields, and the
//! content blocks must keep the core serializers' exact shape.

use serde_json::json;

use pir_builtin_tool_repair::types::{
    AssistantMessage, ContentBlock, StopReason, ThinkingContent, ToolCall,
};

#[test]
fn a_core_shaped_message_round_trips_with_unknown_fields_preserved() {
    let value = json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "hello", "textSignature": "sig"},
            {"type": "thinking", "thinking": "hmm", "redacted": true},
            {"type": "toolCall", "id": "t1", "name": "read",
             "arguments": {"path": "a.txt"}, "thoughtSignature": "ts"},
            {"type": "image", "data": "AA==", "mimeType": "image/png"}
        ],
        "api": "anthropic",
        "provider": "anthropic",
        "model": "claude",
        "responseModel": "claude-2024",
        "usage": {"input": 3, "output": 4, "cacheRead": 0, "cacheWrite": 0},
        "stopReason": "toolUse",
        "timestamp": 123,
        "errorMessage": "old error"
    });

    let message = AssistantMessage::from_json_value(value).expect("core message deserializes");
    assert_eq!(
        message.role,
        pir_builtin_tool_repair::types::AssistantRole::Assistant
    );
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.error_message.as_deref(), Some("old error"));
    assert_eq!(message.content.len(), 4);
    assert_eq!(message.tool_calls().count(), 1);
    assert!(message.has_tool_calls());

    // Interpreted fields are typed, not swept into `extra`; everything else
    // survives verbatim.
    for interpreted in ["role", "content", "stopReason", "errorMessage"] {
        assert!(!message.extra.contains_key(interpreted), "{interpreted}");
    }
    for preserved in [
        "api",
        "provider",
        "model",
        "responseModel",
        "usage",
        "timestamp",
    ] {
        assert!(message.extra.contains_key(preserved), "{preserved}");
    }

    let round = message.to_json_value();
    assert_eq!(round["role"], "assistant");
    assert_eq!(round["api"], "anthropic");
    assert_eq!(round["responseModel"], "claude-2024");
    assert_eq!(round["usage"]["output"], 4);
    assert_eq!(round["stopReason"], "toolUse");
    assert_eq!(round["content"][0]["textSignature"], "sig");
    assert_eq!(round["content"][1]["redacted"], true);
    assert_eq!(round["content"][2]["type"], "toolCall");
    assert_eq!(round["content"][2]["thoughtSignature"], "ts");
    assert_eq!(round["content"][3]["mimeType"], "image/png");
}

#[test]
fn a_minimal_message_defaults_the_role_and_stop_reason() {
    let message = AssistantMessage::from_json_value(json!({"content": []})).expect("minimal");
    assert_eq!(message.stop_reason, StopReason::Pending);
    assert!(message.extra.is_empty());
    assert_eq!(message.to_json_value()["role"], "assistant");
}

#[test]
fn content_blocks_serialize_with_the_core_tags_and_omit_absent_options() {
    assert_eq!(
        serde_json::to_value(ContentBlock::text("hi")).expect("text"),
        json!({"type": "text", "text": "hi"})
    );
    assert_eq!(
        serde_json::to_value(ContentBlock::Thinking(ThinkingContent::new("hmm")))
            .expect("thinking"),
        json!({"type": "thinking", "thinking": "hmm"})
    );

    let call = ToolCall::new("id", "read", [("path".to_owned(), json!("a"))].into());
    assert_eq!(
        serde_json::to_value(ContentBlock::ToolCall(call)).expect("call"),
        json!({"type": "toolCall", "id": "id", "name": "read", "arguments": {"path": "a"}})
    );
}

#[test]
fn optional_tool_call_metadata_round_trips() {
    let value = json!({
        "type": "toolCall",
        "id": "id",
        "name": "read",
        "arguments": {},
        "namespace": "ns",
        "argumentsError": "bad json"
    });
    let block: ContentBlock = serde_json::from_value(value.clone()).expect("deserialize");
    let ContentBlock::ToolCall(call) = &block else {
        panic!("expected a tool call");
    };
    assert_eq!(call.namespace.as_deref(), Some("ns"));
    assert_eq!(call.arguments_error.as_deref(), Some("bad json"));
    assert_eq!(serde_json::to_value(&block).expect("serialize"), value);
}
