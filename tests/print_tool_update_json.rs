//! `tool_execution_update` wire projection (upstream
//! `ToolExecutionUpdateEvent`): JSON/print hosts and the extension dispatch
//! channel share one projector, so the frame shape is pinned here once.
//!
//! The stdout wiring itself is exercised end-to-end by the liveness probe
//! (`pir print --mode json` over a command that outlives the 30 s heartbeat);
//! this file pins the projection the writer serializes.

use pir::agent::tool_execution::tool_update_payload;

#[test]
fn projection_carries_the_call_identity_and_the_partial_result() {
    let partial = serde_json::json!({
        "content": [{ "type": "text", "text": "building…" }],
        "details": {},
        "isError": false,
        "toolCallId": "call-1",
        "toolName": "bash",
        "args": { "command": "make" }
    });
    let payload = tool_update_payload(&partial);
    assert_eq!(payload["type"], "tool_execution_update");
    assert_eq!(payload["toolCallId"], "call-1");
    assert_eq!(payload["toolName"], "bash");
    assert_eq!(payload["args"], serde_json::json!({ "command": "make" }));
    assert_eq!(
        payload["partialResult"], partial,
        "the tool-level snapshot is passed through verbatim"
    );
    assert!(
        payload.get("parentToolCallId").is_none(),
        "a root call carries no parent id: {payload}"
    );
}

#[test]
fn nested_call_ids_expose_the_parent_prefix() {
    let partial = serde_json::json!({
        "content": [],
        "toolCallId": "parent-call/2",
        "toolName": "grep",
        "args": {}
    });
    let payload = tool_update_payload(&partial);
    assert_eq!(payload["toolCallId"], "parent-call/2");
    assert_eq!(payload["parentToolCallId"], "parent-call");
}

#[test]
fn an_unstamped_partial_still_projects_the_result() {
    // A tool that calls its update sink without the scheduler's identity
    // stamps (defensive path): the frame stays well-formed and carries the
    // snapshot.
    let partial = serde_json::json!({ "content": [] });
    let payload = tool_update_payload(&partial);
    assert_eq!(
        payload,
        serde_json::json!({
            "type": "tool_execution_update",
            "partialResult": { "content": [] }
        })
    );
}
