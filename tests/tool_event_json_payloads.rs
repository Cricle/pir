//! Wire payloads on the tool lifecycle frames: `tool_execution_start.args` and
//! `tool_execution_end.result` (upstream `ToolExecutionStartEvent` /
//! `ToolExecutionEndEvent`), plus the result's `AgentToolResult` shape shared
//! by the extension dispatch and the JSON/print session event.
//!
//! Before this, the JSON frames carried only call identity and the extension
//! payload carried `content` as a bare string; both now match upstream
//! (`content` is the text/image block array, optional `details`/
//! `structuredContent`/`terminate` are omitted when unset).

use serde_json::json;

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_batch::{ToolExecutionEvent, ToolResult};
use pir::agent::tool_execution::tool_result_event_json;
use pir::agent_session::agent_event_to_json;
use pir::ai::types::ImageContent;

fn result_event(call_id: &str, tool: &str, result: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolExecutionEnd {
        turn: 3,
        event: ToolExecutionEvent::End {
            index: 0,
            call_id: call_id.to_owned(),
            tool_name: tool.to_owned(),
            is_error: false,
            duration_ms: Some(42),
            parent_call_id: None,
        },
        result,
    }
}

#[test]
fn result_wire_shape_is_the_upstream_block_array() {
    let mut result = ToolResult::ok("hello");
    result.details = Some(json!({ "exit_code": 0 }));
    result.structured_content = Some(json!({ "answer": 42 }));
    result.images = vec![ImageContent {
        data: "aGVsbG8=".to_owned(),
        mime_type: "image/png".to_owned(),
    }];

    let wire = tool_result_event_json(&result);
    assert_eq!(
        wire["content"],
        json!([
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
        ])
    );
    assert_eq!(wire["details"], json!({ "exit_code": 0 }));
    assert_eq!(wire["structuredContent"], json!({ "answer": 42 }));
}

#[test]
fn optional_result_fields_are_omitted_when_unset() {
    let wire = tool_result_event_json(&ToolResult::ok("plain"));
    assert_eq!(
        wire,
        json!({ "content": [{ "type": "text", "text": "plain" }] }),
        "an absent details/structuredContent must not serialize as null"
    );
}

#[test]
fn terminate_is_carried_only_when_set() {
    let mut result = ToolResult::ok("stop the batch");
    result.terminate = true;
    let wire = tool_result_event_json(&result);
    assert_eq!(
        wire["terminate"],
        json!(true),
        "upstream reads finalized.result.terminate; the flag must reach the wire"
    );
    assert_eq!(wire["content"][0]["text"], "stop the batch");

    // The same flag must survive the `tool_execution_end` frame a JSON/SDK
    // consumer inspects.
    let frame = agent_event_to_json(&result_event("call-3", "bash", wire));
    assert_eq!(frame["result"]["terminate"], json!(true));

    // `terminate: false` is upstream's `undefined`: the key stays absent
    // rather than serializing as `false`.
    let wire = tool_result_event_json(&ToolResult::ok("keep going"));
    assert!(
        wire.get("terminate").is_none(),
        "terminate must be omitted when false: {wire}"
    );
}

#[test]
fn start_frame_carries_the_call_arguments() {
    let event = AgentEvent::ToolExecutionStart {
        turn: 1,
        event: ToolExecutionEvent::Start {
            index: 0,
            call_id: "call-1".to_owned(),
            tool_name: "bash".to_owned(),
            parent_call_id: None,
        },
        args: json!({ "command": "ls" }),
    };
    let frame = agent_event_to_json(&event);
    assert_eq!(frame["type"], "tool_execution_start");
    assert_eq!(frame["toolCallId"], "call-1");
    assert_eq!(frame["toolName"], "bash");
    assert_eq!(frame["args"], json!({ "command": "ls" }));
}

#[test]
fn end_frame_carries_the_result_blocks() {
    let event = result_event(
        "call-2",
        "read",
        tool_result_event_json(&ToolResult::ok("file body")),
    );
    let frame = agent_event_to_json(&event);
    assert_eq!(frame["type"], "tool_execution_end");
    assert_eq!(frame["toolCallId"], "call-2");
    assert_eq!(frame["isError"], false);
    assert_eq!(frame["durationMs"], 42);
    assert_eq!(
        frame["result"],
        json!({ "content": [{ "type": "text", "text": "file body" }] }),
        "the end frame carries the same AgentToolResult shape as extensions"
    );
}

#[test]
fn nested_end_frames_keep_the_parent_identity_and_result() {
    let event = result_event(
        "call-parent/1",
        "grep",
        tool_result_event_json(&ToolResult::ok("match")),
    );
    let frame = agent_event_to_json(&event);
    assert_eq!(frame["toolCallId"], "call-parent/1");
    assert_eq!(
        frame["result"]["content"][0]["text"], "match",
        "a nested result is projected exactly like a root one"
    );
}
