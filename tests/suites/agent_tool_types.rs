//! Tests for tool batch types and events.

use pir::agent::tool_batch::*;

#[test]
fn execution_mode_default_is_parallel() {
    assert_eq!(ToolExecutionMode::default(), ToolExecutionMode::Parallel);
}

#[test]
fn agent_tool_call_constructs_with_index() {
    let call = AgentToolCall::new(0, "call-1", "read", serde_json::json!({"path": "a.txt"}));
    assert_eq!(call.index, 0);
    assert_eq!(call.call_id, "call-1");
    assert_eq!(call.tool_name, "read");
    assert_eq!(
        call.arguments.get("path").and_then(|v| v.as_str()),
        Some("a.txt")
    );
}

#[test]
fn tool_result_ok_and_error() {
    let ok = ToolResult::ok("success");
    assert!(!ok.is_error);
    assert!(!ok.terminate);
    assert_eq!(ok.content, "success");

    let err = ToolResult::error("fail");
    assert!(err.is_error);
    assert_eq!(err.content, "fail");
}

#[test]
fn tool_result_error_terminate() {
    let r = ToolResult::error_terminate("fatal");
    assert!(r.is_error);
    assert!(r.terminate);
}

#[test]
fn finalized_tool_call_is_error_and_terminate() {
    let f = FinalizedToolCall {
        index: 0,
        call_id: "c1".into(),
        tool_name: "t".into(),
        result: ToolResult::error_terminate("fatal"),
        usage: None,
        duration_ms: None,
    };
    assert!(f.is_error());
    assert!(f.terminate());
}

#[test]
fn tool_batch_outcome_should_terminate_all_true() {
    let outcome = ToolBatchOutcome {
        results: vec![
            FinalizedToolCall {
                index: 0,
                call_id: "c1".into(),
                tool_name: "t".into(),
                result: ToolResult {
                    content: "".into(),
                    images: Vec::new(),
                    details: None,
                    structured_content: None,
                    is_error: true,
                    terminate: true,
                },
                usage: None,
                duration_ms: None,
            },
            FinalizedToolCall {
                index: 1,
                call_id: "c2".into(),
                tool_name: "t".into(),
                result: ToolResult {
                    content: "".into(),
                    images: Vec::new(),
                    details: None,
                    structured_content: None,
                    is_error: false,
                    terminate: true,
                },
                usage: None,
                duration_ms: None,
            },
        ],
        ..ToolBatchOutcome::default()
    };
    assert!(outcome.should_terminate());
}

#[test]
fn tool_batch_outcome_should_not_terminate_mixed() {
    let outcome = ToolBatchOutcome {
        results: vec![
            FinalizedToolCall {
                index: 0,
                call_id: "c1".into(),
                tool_name: "t".into(),
                result: ToolResult {
                    content: "".into(),
                    images: Vec::new(),
                    details: None,
                    structured_content: None,
                    is_error: false,
                    terminate: true,
                },
                usage: None,
                duration_ms: None,
            },
            FinalizedToolCall {
                index: 1,
                call_id: "c2".into(),
                tool_name: "t".into(),
                result: ToolResult {
                    content: "".into(),
                    images: Vec::new(),
                    details: None,
                    structured_content: None,
                    is_error: false,
                    terminate: false,
                },
                usage: None,
                duration_ms: None,
            },
        ],
        ..ToolBatchOutcome::default()
    };
    assert!(!outcome.should_terminate());
}

#[test]
fn tool_batch_outcome_empty_does_not_terminate() {
    let outcome = ToolBatchOutcome::default();
    assert!(!outcome.should_terminate());
    assert!(outcome.is_empty());
}

#[test]
fn tool_execution_events_construct() {
    let start = ToolExecutionEvent::Start {
        index: 0,
        call_id: "c1".into(),
        tool_name: "read".into(),
        parent_call_id: None,
    };
    let end = ToolExecutionEvent::End {
        index: 0,
        call_id: "c1".into(),
        tool_name: "read".into(),
        is_error: false,
        duration_ms: None,
        parent_call_id: None,
    };
    assert!(matches!(start, ToolExecutionEvent::Start { .. }));
    assert!(matches!(end, ToolExecutionEvent::End { .. }));
}

#[test]
fn prepared_tool_call_holds_fields() {
    let call = AgentToolCall::new(1, "c1", "write", serde_json::json!({"path": "b.txt"}));
    let prepared = PreparedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        validated_args: serde_json::json!({"path": "b.txt"}),
        mutation_paths: vec![],
        execution_mode: ToolExecutionMode::Parallel,
    };
    assert_eq!(prepared.index, 1);
    assert_eq!(prepared.tool_name, "write");
}

#[test]
fn tool_result_details_are_optional() {
    let r = ToolResult::ok("done");
    assert!(r.details.is_none());
    let r2 = ToolResult {
        content: "x".into(),
        images: Vec::new(),
        details: Some(serde_json::json!({"key": "val"})),
        structured_content: None,
        is_error: false,
        terminate: false,
    };
    assert_eq!(
        r2.details.unwrap().get("key").and_then(|v| v.as_str()),
        Some("val")
    );
}
