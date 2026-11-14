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

use pir::agent::agent_loop::{AgentEvent, LoopResult};
use pir::agent::tool_batch::ToolExecutionEvent;
use pir::agent_session::{
    agent_event_to_json, session_event_to_json, to_session_event_line, SessionEvent,
    SummarizationRetrySource,
};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, ErrorReason, ImageContent, Message,
    TerminalReason, TextContent, ThinkingContent, ToolCall, ToolResultMessage, ToolResultRole,
    Usage, UserMessage, UserRole,
};
use pir::coding_agent::json::{to_json_event, to_json_event_line};
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

// ---------------------------------------------------------------------------
// PIR-BIN-099 — the borrowed wire line the RPC/JSON fan-out streams
// ---------------------------------------------------------------------------

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 3,
    })
}

fn tool_result_message() -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        content: vec![text_block("line \"one\"\n\u{4f60}\u{597d}")],
        details: Some(json!({ "truncation": { "content": "tail \"x\"\n" } })),
        usage: Some(Usage {
            input: 3,
            output: 4,
            cache_read: 5,
            cache_write: 6,
            cache_write_1h: Some(7),
            reasoning: Some(8),
            total_tokens: 9,
            ..Usage::default()
        }),
        added_tool_names: vec!["added".to_string()],
        structured_content: Some(json!({ "answer": [1, 2.5, null] })),
        nested_calls: None,
        is_error: false,
        duration_ms: Some(11),
        timestamp: 12,
    }
}

/// One `AgentEvent` of EVERY variant. Conditional wire fields appear both
/// absent and present where the old construction appended them after the
/// fact (`parentToolCallId`, `durationMs`), both impossible arm pairings are
/// covered (a `Start` event on `ToolExecutionEnd` and vice versa), and the
/// payloads carry characters that need JSON escaping.
fn agent_event_corpus() -> Vec<AgentEvent> {
    let context = Arc::new(pir::ai::types::Context {
        system_prompt: Some("sys \"q\"\n\u{4f60}\u{597d}".to_string()),
        messages: vec![user_message("hello \"world\"\n\u{4f60}\u{597d}")],
        tools: None,
    });
    let assistant = message_with(vec![text_block("answer \"quoted\"\n")]);
    let result_message = tool_result_message();
    let loop_result = LoopResult {
        terminal: assistant.clone(),
        context: Arc::clone(&context),
        tool_results: vec![result_message.clone()],
        new_messages: vec![
            Message::Assistant(assistant.clone()),
            Message::ToolResult(result_message.clone()),
        ],
        preamble: vec![user_message("preamble")],
        turns: 2,
        aborted: false,
        usage_total: 9,
        will_retry: true,
    };
    let partial = Arc::new(message_with(vec![
        text_block("hello"),
        tool_block("call-1", "read", "src/main.rs"),
    ]));
    let start = |parent: Option<&str>| ToolExecutionEvent::Start {
        index: 0,
        call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        parent_call_id: parent.map(str::to_string),
    };
    let end = |parent: Option<&str>, duration: Option<u64>| ToolExecutionEvent::End {
        index: 0,
        call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        is_error: false,
        duration_ms: duration,
        parent_call_id: parent.map(str::to_string),
    };
    vec![
        AgentEvent::AgentStart {
            context: Arc::clone(&context),
        },
        AgentEvent::AgentEnd {
            result: Arc::new(loop_result),
        },
        AgentEvent::TurnStart {
            turn: 1,
            context: Arc::clone(&context),
        },
        AgentEvent::TurnEnd {
            turn: 1,
            message: assistant.clone(),
            tool_results: vec![result_message.clone()],
        },
        AgentEvent::TurnEnd {
            turn: 2,
            message: assistant.clone(),
            tool_results: Vec::new(),
        },
        AgentEvent::MessageStart {
            turn: 0,
            message: user_message("start \"x\""),
        },
        AgentEvent::MessageEnd {
            turn: 0,
            message: user_message("end \"x\""),
        },
        AgentEvent::MessageEnd {
            turn: 1,
            message: Message::ToolResult(result_message.clone()),
        },
        AgentEvent::StreamStart {
            turn: 1,
            message: assistant.clone(),
        },
        AgentEvent::StreamEnd {
            turn: 1,
            message: assistant.clone(),
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "d".into(),
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: AssistantMessageEvent::ToolCallStart {
                content_index: 1,
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::StreamEvent {
            turn: 1,
            event: AssistantMessageEvent::Done {
                reason: TerminalReason::Stop,
                message: assistant.clone(),
            },
        },
        AgentEvent::ToolExecutionStart {
            turn: 1,
            event: start(None),
            args: json!({ "path": "big.txt", "limit": 2 }),
        },
        AgentEvent::ToolExecutionStart {
            turn: 1,
            event: start(Some("call-0")),
            args: json!({}),
        },
        AgentEvent::ToolExecutionStart {
            turn: 1,
            event: end(Some("call-0"), Some(5)),
            args: json!({ "fallback": true }),
        },
        AgentEvent::ToolExecutionEnd {
            turn: 1,
            event: end(None, None),
            result: json!({ "content": [{ "type": "text", "text": "r \"q\"" }] }),
        },
        AgentEvent::ToolExecutionEnd {
            turn: 1,
            event: end(Some("call-0"), Some(5)),
            result: json!({ "content": [] }),
        },
        AgentEvent::ToolExecutionEnd {
            turn: 1,
            event: start(None),
            result: json!({ "content": [], "fallback": true }),
        },
        AgentEvent::ToolExecutionUpdate {
            turn: 1,
            call_id: "call-1".into(),
            tool_name: "bash".into(),
            parent_call_id: None,
            args: json!({ "command": "echo \"x\"" }),
            partial: json!({ "chunk": "x" }),
        },
        AgentEvent::ToolExecutionUpdate {
            turn: 1,
            call_id: "call-1".into(),
            tool_name: "bash".into(),
            parent_call_id: Some("call-0".into()),
            args: json!({}),
            partial: json!({ "chunk": "y" }),
        },
        AgentEvent::Compaction { turn: 2 },
    ]
}

/// One `SessionEvent` of EVERY variant, with every conditional wire field on
/// both sides of its presence test.
fn session_event_corpus() -> Vec<SessionEvent> {
    let entry = json!({
        "id": "entry-1",
        "message": {
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "read",
            "content": [{ "type": "text", "text": "line \"one\"\n\u{4f60}\u{597d}" }],
            "isError": false,
            "timestamp": 1,
        },
    });
    vec![
        SessionEvent::QueueUpdate {
            steering: vec!["a \"q\"".into()],
            follow_up: vec!["b".into()],
            delivered_steering: vec!["c".into()],
            delivered_follow_up: Vec::new(),
        },
        SessionEvent::CompactionStart {
            reason: "threshold",
        },
        SessionEvent::CompactionEnd {
            reason: "manual",
            result: Some(json!({ "summary": "s \"q\"" })),
            aborted: false,
            will_retry: true,
            error_message: None,
        },
        SessionEvent::CompactionEnd {
            reason: "overflow",
            result: None,
            aborted: true,
            will_retry: false,
            error_message: Some("boom \"x\"".into()),
        },
        SessionEvent::AgentSettled { aborted: false },
        SessionEvent::EntryAppended {
            entry: entry.clone(),
        },
        SessionEvent::SessionInfoChanged {
            name: Some("name \"q\"".into()),
        },
        SessionEvent::SessionInfoChanged { name: None },
        SessionEvent::HistoryTrimmed {
            dropped_messages: 1,
            retained_messages: 2,
            max_messages: 3,
            max_tokens: 4,
            reason: "messages",
            total_dropped_messages: 5,
        },
        SessionEvent::ThinkingLevelChanged {
            level: "high".into(),
        },
        SessionEvent::BashExecutionUpdate {
            id: Some("id-1".into()),
            delta: "d \"x\"\n".into(),
        },
        SessionEvent::BashExecutionUpdate {
            id: None,
            delta: "d".into(),
        },
        SessionEvent::BackgroundToolEnd {
            call_id: "call-1".into(),
            tool_name: "bash".into(),
            parent_call_id: Some("call-0".into()),
            is_error: true,
            duration_ms: Some(9),
            output: "out \"q\"\n\u{4f60}\u{597d}".into(),
            images: vec![ImageContent {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }],
            details: Some(json!({ "exitCode": 1 })),
            structured_content: Some(json!({ "k": [1, 2] })),
            usage: Some(Usage::default()),
        },
        SessionEvent::BackgroundToolEnd {
            call_id: "call-2".into(),
            tool_name: "read".into(),
            parent_call_id: None,
            is_error: false,
            duration_ms: None,
            output: String::new(),
            images: Vec::new(),
            details: None,
            structured_content: None,
            usage: None,
        },
        SessionEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 500,
            error_message: "e".into(),
        },
        SessionEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        },
        SessionEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("final".into()),
        },
        SessionEvent::SummarizationRetryScheduled {
            attempt: 1,
            max_attempts: 2,
            delay_ms: 10,
            error_message: "e".into(),
        },
        SessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationRetrySource::BranchSummary,
        },
        SessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationRetrySource::Compaction { reason: "manual" },
        },
        SessionEvent::SummarizationRetryFinished,
    ]
}

/// The borrowed agent-event line is byte-identical to BOTH text productions
/// the RPC/JSON hosts used before: the `serde_json` serialization of the
/// `Value` construction (print mode / the probe corpus) and the stack-safe
/// iterative writer the RPC output task runs over that `Value`.
#[test]
fn every_agent_event_line_matches_the_value_construction() {
    for event in agent_event_corpus() {
        let value = to_json_event(&event);
        let value_line = serde_json::to_string(&value).expect("value line serializes");
        let borrowed = to_json_event_line(&event).expect("borrowed line serializes");
        assert_eq!(
            borrowed, value_line,
            "the borrowed wire line drifted from the Value construction for {event:?}"
        );
        assert_eq!(
            borrowed,
            pir::session_manager::json_depth::write_value_iteratively(&value),
            "the borrowed wire line drifted from the RPC writer's iterative text for {event:?}"
        );
    }
}

/// The borrowed session-event line is byte-identical to the `Value`
/// construction and the iterative writer, for every `SessionEvent` variant.
#[test]
fn every_session_event_line_matches_the_value_construction() {
    for event in session_event_corpus() {
        let value = session_event_to_json(&event);
        let value_line = serde_json::to_string(&value).expect("value line serializes");
        let borrowed = to_session_event_line(&event).expect("borrowed line serializes");
        assert_eq!(
            borrowed, value_line,
            "the borrowed wire line drifted from the Value construction for {event:?}"
        );
        assert_eq!(
            borrowed,
            pir::session_manager::json_depth::write_value_iteratively(&value),
            "the borrowed wire line drifted from the RPC writer's iterative text for {event:?}"
        );
    }
}
