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
    AssistantMessage, AssistantMessageEvent, ContentBlock, Cost, DiagnosticInfo, ErrorReason,
    ImageContent, Message, NestedToolCallRecord, NestedToolCallStatus, NestedToolCalls, StopReason,
    TerminalReason, TextContent, ThinkingContent, ThinkingLevel, ToolCall, ToolResultMessage,
    ToolResultRole, Usage, UserMessage, UserRole,
};
use pir::coding_agent::json::{
    json_string_len, json_value_len, to_json_event, to_json_event_line, AgentEventWire,
};
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

// ---------------------------------------------------------------------------
// PIR-BIN-099 hardening: deterministic randomized/adversarial byte-identity
// corpus
// ---------------------------------------------------------------------------
//
// The hand-built corpora above pin exact shapes. These generators sweep the
// same seam over payloads chosen to break any escaping or pre-sizing
// shortcut: every C0 control byte, quotes/backslashes, DEL and the C1 range,
// multi-byte UTF-8 (U+2028/U+2029, emoji, CJK, combining marks), 100 KB+
// tool-result texts, deeply nested Values, empty strings, boundary numbers
// (i64::MIN/MAX, u64::MAX, 1e-300, 0.0, -0.0, f64::MAX) and exotic object
// keys. The RNG is a tiny wrapping LCG, so the corpus is fixed, reproducible
// and dependency-free.

/// Tiny deterministic 64-bit LCG (Knuth's MMIX constants): same sequence on
/// every platform, so the generated corpus below is a constant.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    /// Uniform-ish index into `0..bound`; `bound` must be non-zero.
    fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "LCG bound must be non-zero");
        (self.next_u64() % bound as u64) as usize
    }

    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 0
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const AGENT_EVENT_VARIANTS: usize = 13;
const AGENT_ROUNDS: usize = 32;
const SESSION_EVENT_VARIANTS: usize = 15;
const SESSION_ROUNDS: usize = 24;
/// Every 8th round carries the 100 KB+ payload family.
const LONG_EVERY: usize = 8;
const AGENT_CORPUS_SEED: u64 = 0x00B1_0099_5EED_0007;
const SESSION_CORPUS_SEED: u64 = 0x00B1_0099_5EED_0013;
const DEEP_VALUE_DEPTH: usize = 128;

const INDEXES: &[usize] = &[0, 1, 2, usize::MAX];
const TURNS: &[usize] = &[0, 1, 2, 7, usize::MAX, usize::MAX - 1];
const U64_BOUNDARIES: &[u64] = &[0, 1, i64::MAX as u64, u64::MAX, u64::MAX - 1];
const U32_BOUNDARIES: &[u32] = &[0, 1, u32::MAX, u32::MAX - 1];
const F64_BOUNDARIES: &[f64] = &[
    0.0,
    -0.0,
    1e-300,
    f64::MIN_POSITIVE,
    f64::MAX,
    123.456,
    -1.0,
];
const TERMINAL_REASONS: &[TerminalReason] = &[
    TerminalReason::Stop,
    TerminalReason::Length,
    TerminalReason::ToolUse,
    TerminalReason::Deferred,
];
const ERROR_REASONS: &[ErrorReason] = &[ErrorReason::Error, ErrorReason::Aborted];
const STOP_REASONS: &[StopReason] = &[
    StopReason::Pending,
    StopReason::Stop,
    StopReason::Length,
    StopReason::ToolUse,
    StopReason::Error,
    StopReason::Aborted,
    StopReason::Deferred,
];
const THINKING_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const NESTED_STATUSES: &[NestedToolCallStatus] = &[
    NestedToolCallStatus::Ok,
    NestedToolCallStatus::Error,
    NestedToolCallStatus::Unfinished,
];
const COMPACTION_REASONS: &[&str] = &["manual", "threshold", "overflow"];
const HISTORY_REASONS: &[&str] = &["messages", "tokens"];

/// The payload alphabet: every byte family JSON escaping has to survive, plus
/// the two "nothing to escape" extremes (the empty string, and the Unicode
/// line separators serde_json must leave verbatim).
fn nasty_pieces() -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    // Every C0 control byte 0x00..=0x1f (`\b`/`\t`/`\n`/`\f`/`\r` plus the
    // \u00XX family).
    pieces.push((0u8..=0x1f_u8).map(char::from).collect());
    // DEL and the C1 range: multi-byte UTF-8 that must NOT be escaped.
    pieces.push("\u{7f}\u{80}\u{9f}".to_string());
    // Quotes and backslashes, including a trailing backslash.
    pieces.push("\"quoted\" \\back\\\\slash\\".to_string());
    pieces.push("\\\\\\\"\"\"".to_string());
    // U+2028 / U+2029: legal raw JSON, never escaped.
    pieces.push("\u{2028}\u{2029}".to_string());
    // Astral-plane UTF-8 including a ZWJ cluster and variation selectors.
    pieces.push("🦀😀👩🏽👩🎤🎉".to_string());
    pieces.push("Ω≈ç√∫˜µ≤≥÷".to_string());
    pieces.push("héllo wörld".to_string());
    pieces.push("你好，世界；こんにちは".to_string());
    // Combining marks, BOM, soft hyphen, zero-width space.
    pieces.push("a\u{300}b\u{301}\u{feff}\u{ad}\u{200b}".to_string());
    // ANSI wrapper (0x1b is a control byte).
    pieces.push("\u{1b}[31mred\u{1b}[0m".to_string());
    // Whitespace-only and empty.
    pieces.push(" \t\u{b}\u{c}\n\r ".to_string());
    pieces.push(String::new());
    pieces
}

const NASTY_KEYS: &[&str] = &[
    "",
    "k",
    "键",
    "ключ",
    "🔑",
    "a b",
    "a\"b",
    "a\\b",
    "line\nbreak",
    "\u{0}nul",
    "\u{7f}del",
    "\u{2028}sep",
    "tab\tkey",
    "🦀emoji",
];

fn int_value(value: i64) -> Value {
    Value::Number(value.into())
}

fn u64_value(value: u64) -> Value {
    Value::Number(value.into())
}

fn float_value(value: f64) -> Value {
    Value::Number(serde_json::Number::from_f64(value).expect("boundary float is finite"))
}

fn random_key(rng: &mut Lcg) -> String {
    (*rng.pick(NASTY_KEYS)).to_string()
}

/// A random composite string from the nasty alphabet (0..3 pieces).
fn random_string(rng: &mut Lcg, pieces: &[String]) -> String {
    let count = rng.below(4);
    let mut out = String::new();
    for _ in 0..count {
        out.push_str(rng.pick(pieces));
    }
    out
}

/// 100 KB+ of text (the pre-sizing hint's large-payload case), seeded with the
/// nasty alphabet. The empty piece is never used as filler.
fn long_string(rng: &mut Lcg, pieces: &[String]) -> String {
    let target = 100 * 1024 + rng.below(2 * 1024);
    let mut out = String::with_capacity(target + 64);
    out.push_str("LONG");
    while out.len() < target {
        let piece = rng.pick(pieces);
        out.push_str(if piece.is_empty() { "x" } else { piece });
    }
    out.push_str("END");
    out
}

fn text_for(rng: &mut Lcg, pieces: &[String], long: bool) -> String {
    if long {
        long_string(rng, pieces)
    } else {
        random_string(rng, pieces)
    }
}

/// `Some(text)` with probability `1/chance` (the `long` flag selects the
/// 100 KB+ alphabet when the text materializes).
fn maybe_string(rng: &mut Lcg, pieces: &[String], long: bool, chance: usize) -> Option<String> {
    if rng.below(chance) == 0 {
        Some(text_for(rng, pieces, long))
    } else {
        None
    }
}

/// Attach one 100 KB+ string to a `Value` payload when this round carries the
/// long-payload family. The value travels through both serialization faces;
/// the extra key is ordinary data, not a shape concession.
fn with_long_payload(value: Value, rng: &mut Lcg, pieces: &[String], long: bool) -> Value {
    if !long {
        return value;
    }
    let mut map = match value {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("wrapped".to_string(), other);
            map
        }
    };
    map.insert(
        "longPayload".to_string(),
        Value::String(long_string(rng, pieces)),
    );
    Value::Object(map)
}

fn random_strings(rng: &mut Lcg, pieces: &[String], max: usize) -> Vec<String> {
    (0..rng.below(max + 1))
        .map(|_| random_string(rng, pieces))
        .collect()
}

/// A pathologically deep Value (arrays and objects alternating) with a nasty
/// leaf: the direct writer, the `Value` construction and the iterative RPC
/// writer must all walk it identically.
fn deep_value(depth: usize) -> Value {
    let mut value: Value = Value::String("\u{0}\u{1f}leaf\u{2028}🦀\"\\\\".to_string());
    for level in 0..depth {
        value = if level % 2 == 0 {
            Value::Array(vec![value])
        } else {
            let mut map = serde_json::Map::new();
            map.insert(format!("lvl{level}\u{0}键"), value);
            Value::Object(map)
        };
    }
    value
}

/// Random `Value` with boundary scalars, unicode keys and bounded nesting.
fn random_value(rng: &mut Lcg, pieces: &[String], depth: usize) -> Value {
    if depth > 0 && rng.below(10) >= 6 {
        return if rng.bool() {
            let len = rng.below(5);
            Value::Array(
                (0..len)
                    .map(|_| random_value(rng, pieces, depth - 1))
                    .collect(),
            )
        } else {
            let len = rng.below(5);
            let mut map = serde_json::Map::new();
            for _ in 0..len {
                map.insert(random_key(rng), random_value(rng, pieces, depth - 1));
            }
            Value::Object(map)
        };
    }
    match rng.below(12) {
        0 => int_value(i64::MIN),
        1 => int_value(i64::MAX),
        2 => u64_value(u64::MAX),
        3 => float_value(1e-300),
        4 => float_value(0.0),
        5 => float_value(-0.0),
        6 => float_value(f64::MAX),
        7 => Value::Null,
        8 => Value::Bool(rng.bool()),
        9 => Value::String(random_key(rng)),
        _ => Value::String(random_string(rng, pieces)),
    }
}

fn random_cost(rng: &mut Lcg) -> Cost {
    Cost {
        input: *rng.pick(F64_BOUNDARIES),
        output: *rng.pick(F64_BOUNDARIES),
        cache_read: *rng.pick(F64_BOUNDARIES),
        cache_write: *rng.pick(F64_BOUNDARIES),
        total: *rng.pick(F64_BOUNDARIES),
        total_from_provider: if rng.bool() { Some(rng.bool()) } else { None },
    }
}

fn random_usage(rng: &mut Lcg) -> Usage {
    Usage {
        input: *rng.pick(U64_BOUNDARIES),
        output: rng.next_u64(),
        cache_read: *rng.pick(U64_BOUNDARIES),
        cache_write: rng.next_u64(),
        cache_write_1h: if rng.bool() {
            Some(*rng.pick(U64_BOUNDARIES))
        } else {
            None
        },
        reasoning: if rng.bool() {
            Some(*rng.pick(U64_BOUNDARIES))
        } else {
            None
        },
        total_tokens: *rng.pick(U64_BOUNDARIES),
        cost: random_cost(rng),
    }
}

fn random_diagnostic(rng: &mut Lcg, pieces: &[String]) -> DiagnosticInfo {
    let mut details = BTreeMap::new();
    for _ in 0..rng.below(3) {
        details.insert(random_key(rng), random_value(rng, pieces, 3));
    }
    DiagnosticInfo {
        diag_type: random_string(rng, pieces),
        timestamp: *rng.pick(U64_BOUNDARIES),
        error_message: maybe_string(rng, pieces, false, 2),
        error_name: maybe_string(rng, pieces, false, 3),
        error_stack: maybe_string(rng, pieces, false, 3),
        error_code: if rng.bool() {
            Some(random_value(rng, pieces, 2))
        } else {
            None
        },
        details: if details.is_empty() {
            None
        } else {
            Some(details)
        },
    }
}

fn random_diagnostics(rng: &mut Lcg, pieces: &[String]) -> Vec<DiagnosticInfo> {
    (0..rng.below(3))
        .map(|_| random_diagnostic(rng, pieces))
        .collect()
}

fn random_tool_call(rng: &mut Lcg, pieces: &[String]) -> ToolCall {
    let mut arguments = BTreeMap::new();
    for _ in 0..rng.below(4) {
        arguments.insert(random_key(rng), random_value(rng, pieces, 4));
    }
    if rng.below(3) == 0 {
        arguments.insert(
            "\u{0}\"deep\u{2028}🦀".to_string(),
            deep_value(DEEP_VALUE_DEPTH),
        );
    }
    ToolCall {
        id: random_string(rng, pieces),
        name: random_string(rng, pieces),
        arguments,
        thought_signature: maybe_string(rng, pieces, false, 3),
        namespace: maybe_string(rng, pieces, false, 3),
        arguments_error: maybe_string(rng, pieces, false, 3),
    }
}

fn random_block(rng: &mut Lcg, pieces: &[String]) -> ContentBlock {
    match rng.below(4) {
        0 => ContentBlock::Text(TextContent {
            text: random_string(rng, pieces),
            text_signature: maybe_string(rng, pieces, false, 4),
        }),
        1 => ContentBlock::Thinking(ThinkingContent {
            thinking: random_string(rng, pieces),
            thinking_signature: maybe_string(rng, pieces, false, 4),
            redacted: rng.bool(),
        }),
        2 => ContentBlock::Image(ImageContent {
            data: random_string(rng, pieces),
            mime_type: random_string(rng, pieces),
        }),
        _ => ContentBlock::ToolCall(random_tool_call(rng, pieces)),
    }
}

fn random_blocks(rng: &mut Lcg, pieces: &[String], max: usize) -> Vec<ContentBlock> {
    (0..rng.below(max + 1))
        .map(|_| random_block(rng, pieces))
        .collect()
}

fn long_text_block(rng: &mut Lcg, pieces: &[String]) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: long_string(rng, pieces),
        text_signature: None,
    })
}

fn random_assistant_with_content(
    rng: &mut Lcg,
    pieces: &[String],
    content: Vec<ContentBlock>,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: random_string(rng, pieces),
        provider: random_string(rng, pieces),
        model: random_string(rng, pieces),
        response_model: maybe_string(rng, pieces, false, 3),
        response_id: maybe_string(rng, pieces, false, 3),
        diagnostics: random_diagnostics(rng, pieces),
        usage: random_usage(rng),
        stop_reason: *rng.pick(STOP_REASONS),
        error_message: maybe_string(rng, pieces, false, 3),
        raw_stop_reason: maybe_string(rng, pieces, false, 4),
        end_turn: if rng.bool() { Some(rng.bool()) } else { None },
        provider_thinking_level: maybe_string(rng, pieces, false, 4),
        thinking_level: if rng.bool() {
            Some(*rng.pick(THINKING_LEVELS))
        } else {
            None
        },
        duration_ms: if rng.bool() {
            Some(*rng.pick(U64_BOUNDARIES))
        } else {
            None
        },
        timestamp: *rng.pick(U64_BOUNDARIES),
        ..AssistantMessage::default()
    }
}

fn random_assistant_message(rng: &mut Lcg, pieces: &[String], long: bool) -> AssistantMessage {
    let mut content = random_blocks(rng, pieces, 3);
    if long {
        content.push(long_text_block(rng, pieces));
    }
    random_assistant_with_content(rng, pieces, content)
}

fn random_user_message(
    rng: &mut Lcg,
    pieces: &[String],
    long: bool,
    role: UserRole,
) -> UserMessage {
    let mut content = random_blocks(rng, pieces, 2);
    if long {
        content.push(long_text_block(rng, pieces));
    }
    let sections = if rng.bool() {
        let mut sections = BTreeMap::new();
        for _ in 0..rng.below(3) {
            sections.insert(random_key(rng), maybe_string(rng, pieces, false, 2));
        }
        Some(sections)
    } else {
        None
    };
    UserMessage {
        role,
        content,
        tools_added: None,
        tools_removed: None,
        sections,
        timestamp: *rng.pick(U64_BOUNDARIES),
    }
}

fn random_nested_calls(rng: &mut Lcg, pieces: &[String]) -> NestedToolCalls {
    let calls = (0..rng.below(3))
        .map(|_| {
            let arguments = if rng.bool() {
                let mut args = BTreeMap::new();
                for _ in 0..rng.below(3) {
                    args.insert(random_key(rng), random_value(rng, pieces, 3));
                }
                Some(args)
            } else {
                None
            };
            NestedToolCallRecord {
                id: random_string(rng, pieces),
                name: random_string(rng, pieces),
                arguments,
                arguments_bytes: if rng.bool() {
                    Some(*rng.pick(U64_BOUNDARIES))
                } else {
                    None
                },
                status: *rng.pick(NESTED_STATUSES),
                duration_ms: if rng.bool() {
                    Some(*rng.pick(U64_BOUNDARIES))
                } else {
                    None
                },
                error: maybe_string(rng, pieces, false, 3),
            }
        })
        .collect();
    NestedToolCalls {
        calls,
        complete: rng.bool(),
    }
}

fn random_tool_result(rng: &mut Lcg, pieces: &[String], long: bool) -> ToolResultMessage {
    let mut content = random_blocks(rng, pieces, 2);
    if long {
        content.push(long_text_block(rng, pieces));
    }
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: random_string(rng, pieces),
        tool_name: random_string(rng, pieces),
        content,
        details: if rng.bool() {
            Some(random_value(rng, pieces, 5))
        } else {
            None
        },
        usage: if rng.bool() {
            Some(random_usage(rng))
        } else {
            None
        },
        added_tool_names: random_strings(rng, pieces, 3),
        structured_content: if rng.bool() {
            Some(random_value(rng, pieces, 5))
        } else {
            None
        },
        nested_calls: if rng.bool() {
            Some(Box::new(random_nested_calls(rng, pieces)))
        } else {
            None
        },
        is_error: rng.bool(),
        duration_ms: if rng.bool() {
            Some(*rng.pick(U64_BOUNDARIES))
        } else {
            None
        },
        timestamp: *rng.pick(U64_BOUNDARIES),
    }
}

fn random_message(rng: &mut Lcg, pieces: &[String], long: bool) -> Message {
    match rng.below(5) {
        0 => Message::User(random_user_message(rng, pieces, long, UserRole::User)),
        1 => Message::System(random_user_message(rng, pieces, long, UserRole::System)),
        2 => Message::Developer(random_user_message(rng, pieces, long, UserRole::Developer)),
        3 => Message::Assistant(random_assistant_message(rng, pieces, long)),
        _ => Message::ToolResult(random_tool_result(rng, pieces, long)),
    }
}

fn random_context(rng: &mut Lcg, pieces: &[String], long: bool) -> Arc<pir::ai::types::Context> {
    let messages = (0..rng.below(4))
        .map(|_| random_message(rng, pieces, false))
        .collect();
    Arc::new(pir::ai::types::Context {
        system_prompt: if long {
            Some(long_string(rng, pieces))
        } else {
            maybe_string(rng, pieces, false, 2)
        },
        messages,
        tools: None,
    })
}

fn random_tool_execution_event(rng: &mut Lcg, pieces: &[String]) -> ToolExecutionEvent {
    if rng.bool() {
        ToolExecutionEvent::Start {
            index: *rng.pick(TURNS),
            call_id: random_string(rng, pieces),
            tool_name: random_string(rng, pieces),
            parent_call_id: maybe_string(rng, pieces, false, 2),
        }
    } else {
        ToolExecutionEvent::End {
            index: *rng.pick(TURNS),
            call_id: random_string(rng, pieces),
            tool_name: random_string(rng, pieces),
            is_error: rng.bool(),
            duration_ms: if rng.bool() {
                Some(*rng.pick(U64_BOUNDARIES))
            } else {
                None
            },
            parent_call_id: maybe_string(rng, pieces, false, 2),
        }
    }
}

fn random_content_values(rng: &mut Lcg, pieces: &[String]) -> Vec<Value> {
    (0..rng.below(3))
        .map(|_| serde_json::to_value(random_block(rng, pieces)).expect("content block serializes"))
        .collect()
}

/// The upstream `AgentToolResult` wire shape (`content` block array plus
/// optional payloads); long rounds attach a 100 KB+ string.
fn random_tool_result_value(rng: &mut Lcg, pieces: &[String], long: bool) -> Value {
    let mut result = serde_json::Map::new();
    result.insert(
        "content".to_string(),
        Value::Array(random_content_values(rng, pieces)),
    );
    if rng.bool() {
        result.insert("details".to_string(), random_value(rng, pieces, 5));
    }
    if rng.below(4) == 0 {
        result.insert(
            "structuredContent".to_string(),
            deep_value(DEEP_VALUE_DEPTH),
        );
    }
    if rng.bool() {
        result.insert("isError".to_string(), Value::Bool(rng.bool()));
    }
    with_long_payload(Value::Object(result), rng, pieces, long)
}

/// One assistant stream event; `variant` cycles 0..12 so a full corpus round
/// covers every `AssistantMessageEvent` arm (including both `toolcall_start`
/// derivations: a `ToolCall` exactly at `content_index`, and an index with no
/// ToolCall behind it).
fn random_assistant_event(
    rng: &mut Lcg,
    pieces: &[String],
    long: bool,
    variant: usize,
) -> AssistantMessageEvent {
    match variant % 12 {
        0 => AssistantMessageEvent::Start {
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        1 => AssistantMessageEvent::TextStart {
            content_index: *rng.pick(INDEXES),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        2 => AssistantMessageEvent::TextDelta {
            content_index: *rng.pick(INDEXES),
            delta: text_for(rng, pieces, long),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        3 => AssistantMessageEvent::TextEnd {
            content_index: *rng.pick(INDEXES),
            content: text_for(rng, pieces, long),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        4 => AssistantMessageEvent::ThinkingStart {
            content_index: *rng.pick(INDEXES),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        5 => AssistantMessageEvent::ThinkingDelta {
            content_index: *rng.pick(INDEXES),
            delta: text_for(rng, pieces, long),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        6 => AssistantMessageEvent::ThinkingEnd {
            content_index: *rng.pick(INDEXES),
            content: text_for(rng, pieces, long),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        7 => {
            if rng.bool() {
                let mut content = random_blocks(rng, pieces, 2);
                let content_index = content.len();
                content.push(ContentBlock::ToolCall(random_tool_call(rng, pieces)));
                AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: Arc::new(random_assistant_with_content(rng, pieces, content)),
                }
            } else {
                AssistantMessageEvent::ToolCallStart {
                    content_index: *rng.pick(INDEXES),
                    partial: Arc::new(random_assistant_message(rng, pieces, long)),
                }
            }
        }
        8 => AssistantMessageEvent::ToolCallDelta {
            content_index: *rng.pick(INDEXES),
            delta: text_for(rng, pieces, long),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        9 => AssistantMessageEvent::ToolCallEnd {
            content_index: *rng.pick(INDEXES),
            tool_call: random_tool_call(rng, pieces),
            partial: Arc::new(random_assistant_message(rng, pieces, long)),
        },
        10 => AssistantMessageEvent::Done {
            reason: *rng.pick(TERMINAL_REASONS),
            message: random_assistant_message(rng, pieces, long),
        },
        _ => AssistantMessageEvent::Error {
            reason: *rng.pick(ERROR_REASONS),
            error: random_assistant_message(rng, pieces, long),
        },
    }
}

/// One `AgentEvent`; `variant` is the index into the 13 top-level variants.
fn random_agent_event(
    rng: &mut Lcg,
    pieces: &[String],
    variant: usize,
    long: bool,
    round: usize,
) -> AgentEvent {
    let turn = *rng.pick(TURNS);
    match variant % AGENT_EVENT_VARIANTS {
        0 => AgentEvent::AgentStart {
            context: random_context(rng, pieces, long),
        },
        1 => {
            let context = random_context(rng, pieces, long);
            let terminal = random_assistant_message(rng, pieces, long);
            let tool_results = (0..rng.below(3))
                .map(|_| random_tool_result(rng, pieces, long))
                .collect();
            let new_message_count = rng.below(4);
            let mut new_messages = Vec::with_capacity(new_message_count);
            for _ in 0..new_message_count {
                let message_long = long && rng.bool();
                new_messages.push(random_message(rng, pieces, message_long));
            }
            let preamble = (0..rng.below(3))
                .map(|_| random_message(rng, pieces, false))
                .collect();
            AgentEvent::AgentEnd {
                result: Arc::new(LoopResult {
                    terminal,
                    context,
                    tool_results,
                    new_messages,
                    preamble,
                    turns: *rng.pick(TURNS),
                    aborted: rng.bool(),
                    usage_total: *rng.pick(U64_BOUNDARIES),
                    will_retry: rng.bool(),
                }),
            }
        }
        2 => AgentEvent::TurnStart {
            turn,
            context: random_context(rng, pieces, long),
        },
        3 => AgentEvent::TurnEnd {
            turn,
            message: random_assistant_message(rng, pieces, long),
            tool_results: (0..rng.below(3))
                .map(|_| random_tool_result(rng, pieces, long))
                .collect(),
        },
        4 => AgentEvent::MessageStart {
            turn,
            message: random_message(rng, pieces, long),
        },
        5 => AgentEvent::MessageEnd {
            turn,
            message: random_message(rng, pieces, long),
        },
        6 => AgentEvent::StreamStart {
            turn,
            message: random_assistant_message(rng, pieces, long),
        },
        7 => AgentEvent::StreamEnd {
            turn,
            message: random_assistant_message(rng, pieces, long),
        },
        8 => AgentEvent::StreamEvent {
            turn,
            event: random_assistant_event(rng, pieces, long, round),
        },
        9 => {
            let args = random_value(rng, pieces, 5);
            AgentEvent::ToolExecutionStart {
                turn,
                event: random_tool_execution_event(rng, pieces),
                args: with_long_payload(args, rng, pieces, long),
            }
        }
        10 => AgentEvent::ToolExecutionEnd {
            turn,
            event: random_tool_execution_event(rng, pieces),
            result: random_tool_result_value(rng, pieces, long),
        },
        11 => {
            let args = random_value(rng, pieces, 5);
            AgentEvent::ToolExecutionUpdate {
                turn,
                call_id: random_string(rng, pieces),
                tool_name: random_string(rng, pieces),
                parent_call_id: maybe_string(rng, pieces, false, 2),
                args: with_long_payload(args, rng, pieces, long),
                partial: random_tool_result_value(rng, pieces, long),
            }
        }
        _ => AgentEvent::Compaction { turn },
    }
}

/// The top-level variant name of an `AgentEvent` (for the coverage assert).
fn agent_event_kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart { .. } => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart { .. } => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::StreamStart { .. } => "stream_start",
        AgentEvent::StreamEnd { .. } => "stream_end",
        AgentEvent::StreamEvent { .. } => "stream_event",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::Compaction { .. } => "compaction",
    }
}

const AGENT_EVENT_KINDS: &[&str] = &[
    "agent_start",
    "agent_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_end",
    "stream_start",
    "stream_end",
    "stream_event",
    "tool_execution_start",
    "tool_execution_end",
    "tool_execution_update",
    "compaction",
];

/// One `SessionEvent`; `variant` is the index into the 15 top-level variants.
fn random_session_event(
    rng: &mut Lcg,
    pieces: &[String],
    variant: usize,
    long: bool,
) -> SessionEvent {
    match variant % SESSION_EVENT_VARIANTS {
        0 => SessionEvent::QueueUpdate {
            steering: random_strings(rng, pieces, 4),
            follow_up: random_strings(rng, pieces, 3),
            delivered_steering: random_strings(rng, pieces, 2),
            delivered_follow_up: random_strings(rng, pieces, 2),
        },
        1 => SessionEvent::CompactionStart {
            reason: *rng.pick(COMPACTION_REASONS),
        },
        2 => {
            let result = if rng.bool() {
                let value = random_value(rng, pieces, 5);
                Some(with_long_payload(value, rng, pieces, long))
            } else {
                None
            };
            SessionEvent::CompactionEnd {
                reason: *rng.pick(COMPACTION_REASONS),
                result,
                aborted: rng.bool(),
                will_retry: rng.bool(),
                error_message: if long {
                    Some(long_string(rng, pieces))
                } else {
                    maybe_string(rng, pieces, false, 2)
                },
            }
        }
        3 => SessionEvent::AgentSettled {
            aborted: rng.bool(),
        },
        4 => SessionEvent::EntryAppended {
            entry: random_entry(rng, pieces, long),
        },
        5 => SessionEvent::SessionInfoChanged {
            name: if long {
                Some(long_string(rng, pieces))
            } else {
                maybe_string(rng, pieces, false, 2)
            },
        },
        6 => SessionEvent::HistoryTrimmed {
            dropped_messages: *rng.pick(TURNS),
            retained_messages: *rng.pick(TURNS),
            max_messages: *rng.pick(TURNS),
            max_tokens: *rng.pick(U64_BOUNDARIES),
            reason: *rng.pick(HISTORY_REASONS),
            total_dropped_messages: *rng.pick(TURNS),
        },
        7 => SessionEvent::ThinkingLevelChanged {
            level: text_for(rng, pieces, long),
        },
        8 => SessionEvent::BashExecutionUpdate {
            id: maybe_string(rng, pieces, false, 2),
            delta: text_for(rng, pieces, long),
        },
        9 => {
            let details = if rng.bool() {
                let value = random_value(rng, pieces, 5);
                Some(with_long_payload(value, rng, pieces, long))
            } else {
                None
            };
            let structured_content = if rng.bool() {
                let value = random_value(rng, pieces, 5);
                Some(with_long_payload(value, rng, pieces, long))
            } else {
                None
            };
            SessionEvent::BackgroundToolEnd {
                call_id: random_string(rng, pieces),
                tool_name: random_string(rng, pieces),
                parent_call_id: maybe_string(rng, pieces, false, 2),
                is_error: rng.bool(),
                duration_ms: if rng.bool() {
                    Some(*rng.pick(U64_BOUNDARIES))
                } else {
                    None
                },
                output: text_for(rng, pieces, long),
                images: (0..rng.below(3))
                    .map(|_| ImageContent {
                        data: random_string(rng, pieces),
                        mime_type: random_string(rng, pieces),
                    })
                    .collect(),
                details,
                structured_content,
                usage: if rng.bool() {
                    Some(random_usage(rng))
                } else {
                    None
                },
            }
        }
        10 => SessionEvent::AutoRetryStart {
            attempt: *rng.pick(U32_BOUNDARIES),
            max_attempts: *rng.pick(U32_BOUNDARIES),
            delay_ms: *rng.pick(U64_BOUNDARIES),
            error_message: text_for(rng, pieces, long),
        },
        11 => SessionEvent::AutoRetryEnd {
            success: rng.bool(),
            attempt: *rng.pick(U32_BOUNDARIES),
            final_error: if rng.bool() {
                Some(text_for(rng, pieces, long))
            } else {
                None
            },
        },
        12 => SessionEvent::SummarizationRetryScheduled {
            attempt: *rng.pick(U32_BOUNDARIES),
            max_attempts: *rng.pick(U32_BOUNDARIES),
            delay_ms: *rng.pick(U64_BOUNDARIES),
            error_message: text_for(rng, pieces, long),
        },
        13 => SessionEvent::SummarizationRetryAttemptStart {
            source: if rng.bool() {
                SummarizationRetrySource::BranchSummary
            } else {
                SummarizationRetrySource::Compaction {
                    reason: *rng.pick(COMPACTION_REASONS),
                }
            },
        },
        _ => SessionEvent::SummarizationRetryFinished,
    }
}

/// The `{id, message}` entry `entry_appended` carries, built like the hosts
/// build it (the message in its serialized wire shape).
fn random_entry(rng: &mut Lcg, pieces: &[String], long: bool) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("id".to_string(), Value::String(text_for(rng, pieces, long)));
    let message = random_message(rng, pieces, long);
    entry.insert(
        "message".to_string(),
        serde_json::to_value(&message).expect("message serializes"),
    );
    if rng.bool() {
        entry.insert(
            "parentId".to_string(),
            Value::String(random_string(rng, pieces)),
        );
    }
    if rng.bool() {
        let details = random_value(rng, pieces, 4);
        entry.insert(
            "details".to_string(),
            with_long_payload(details, rng, pieces, long),
        );
    }
    Value::Object(entry)
}

const SESSION_EVENT_WIRE_TYPES: &[&str] = &[
    "queue_update",
    "compaction_start",
    "compaction_end",
    "agent_settled",
    "entry_appended",
    "session_info_changed",
    "history_trimmed",
    "thinking_level_changed",
    "bash_execution_update",
    "background_tool_end",
    "auto_retry_start",
    "auto_retry_end",
    "summarization_retry_scheduled",
    "summarization_retry_attempt_start",
    "summarization_retry_finished",
];

/// Byte-for-byte equality with a bounded diagnostic: a mismatch on a 100 KB+
/// payload must not dump megabytes into the test output.
fn assert_lines_identical(label: &str, index: usize, borrowed: &str, expected: &str) {
    if borrowed == expected {
        return;
    }
    let first_diff = borrowed
        .bytes()
        .zip(expected.bytes())
        .position(|(left, right)| left != right)
        .unwrap_or(borrowed.len().min(expected.len()));
    let window = |text: &str| {
        let mut start = first_diff.saturating_sub(24);
        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = (first_diff + 24).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        format!("{:?}", &text[start..end])
    };
    panic!(
        "wire byte mismatch for {label} at corpus index {index}: borrowed {} bytes, \
         expected {} bytes, first differing byte {first_diff}; borrowed near {}, \
         expected near {}",
        borrowed.len(),
        expected.len(),
        window(borrowed),
        window(expected),
    );
}

/// The bytes each event's line must carry (corpus self-check: the generated
/// payloads must actually reach the wire with these families intact).
#[derive(Default)]
struct NastyByteWitnesses {
    escaped_control: bool,
    raw_del: bool,
    raw_u2028: bool,
    emoji: bool,
    quote_backslash: bool,
    longest_line: usize,
}

impl NastyByteWitnesses {
    fn witness(&mut self, line: &str) {
        self.longest_line = self.longest_line.max(line.len());
        self.escaped_control |= line.contains("\\u0000") && line.contains("\\u001f");
        self.raw_del |= line.contains('\u{7f}');
        self.raw_u2028 |= line.contains('\u{2028}');
        self.emoji |= line.contains('🦀');
        self.quote_backslash |= line.contains("\\\"") && line.contains("\\\\");
    }

    fn assert_complete(&self) {
        assert!(
            self.escaped_control,
            "corpus must carry escaped C0 control bytes (\\u0000/\\u001f)"
        );
        assert!(
            self.raw_del,
            "corpus must carry raw DEL (0x7f is not escaped by serde_json)"
        );
        assert!(
            self.raw_u2028,
            "corpus must carry raw U+2028 (serde_json leaves it verbatim)"
        );
        assert!(self.emoji, "corpus must carry astral-plane UTF-8");
        assert!(
            self.quote_backslash,
            "corpus must carry escaped quotes and backslashes"
        );
        assert!(
            self.longest_line > 100 * 1024,
            "corpus must actually produce 100 KB+ lines, longest {}",
            self.longest_line
        );
    }
}

/// PIR-BIN-099 hardening: hundreds of generated `AgentEvent`s across every
/// variant with adversarial payloads; the pre-sized borrowed wire line must
/// equal the `serde_json` serialization of the `Value` construction (and the
/// RPC writer's iterative text) byte for byte, for every single event.
#[test]
fn randomized_agent_event_corpus_matches_the_value_construction_byte_for_byte() {
    let pieces = nasty_pieces();
    let mut rng = Lcg::new(AGENT_CORPUS_SEED);
    let mut events: Vec<AgentEvent> = Vec::with_capacity(AGENT_ROUNDS * AGENT_EVENT_VARIANTS);
    let mut kind_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut stream_counts: BTreeMap<&'static str, usize> = BTreeMap::new();

    for round in 0..AGENT_ROUNDS {
        let long = round % LONG_EVERY == LONG_EVERY - 1;
        for variant in 0..AGENT_EVENT_VARIANTS {
            let event = random_agent_event(&mut rng, &pieces, variant, long, round);
            *kind_counts.entry(agent_event_kind(&event)).or_default() += 1;
            if let AgentEvent::StreamEvent { event: inner, .. } = &event {
                *stream_counts.entry(event_name(inner)).or_default() += 1;
            }
            events.push(event);
        }
    }

    assert_eq!(events.len(), AGENT_ROUNDS * AGENT_EVENT_VARIANTS);
    assert_eq!(
        kind_counts.len(),
        AGENT_EVENT_VARIANTS,
        "the corpus must cover every top-level AgentEvent variant"
    );
    for kind in AGENT_EVENT_KINDS {
        assert_eq!(
            kind_counts.get(kind).copied(),
            Some(AGENT_ROUNDS),
            "corpus coverage for {kind}"
        );
    }
    assert_eq!(
        stream_counts.len(),
        12,
        "the corpus must cover every AssistantMessageEvent variant: {stream_counts:?}"
    );

    let mut witnesses = NastyByteWitnesses::default();
    for (index, event) in events.iter().enumerate() {
        let value = to_json_event(event);
        let value_line = serde_json::to_string(&value).expect("value line serializes");
        let borrowed = to_json_event_line(event).expect("borrowed line serializes");
        witnesses.witness(&borrowed);
        assert_lines_identical("agent event", index, &borrowed, &value_line);
        let iterative = pir::session_manager::json_depth::write_value_iteratively(&value);
        assert_lines_identical(
            "agent event (iterative writer)",
            index,
            &borrowed,
            &iterative,
        );
    }
    witnesses.assert_complete();
}

/// The session-event sibling: the same deterministic randomized corpus
/// discipline over every `SessionEvent` variant, including the 100 KB+
/// `entry_appended`/`background_tool_end` payloads the session-line
/// pre-sizing (`SessionEvent::wire_size_hint`) was added for.
#[test]
fn randomized_session_event_corpus_matches_the_value_construction_byte_for_byte() {
    let pieces = nasty_pieces();
    let mut rng = Lcg::new(SESSION_CORPUS_SEED);
    let mut events: Vec<SessionEvent> = Vec::with_capacity(SESSION_ROUNDS * SESSION_EVENT_VARIANTS);
    let mut type_counts: BTreeMap<String, usize> = BTreeMap::new();

    for round in 0..SESSION_ROUNDS {
        let long = round % LONG_EVERY == LONG_EVERY - 1;
        for variant in 0..SESSION_EVENT_VARIANTS {
            let event = random_session_event(&mut rng, &pieces, variant, long);
            let wire_type = session_event_to_json(&event)["type"]
                .as_str()
                .unwrap_or("<missing>")
                .to_string();
            *type_counts.entry(wire_type).or_default() += 1;
            events.push(event);
        }
    }

    assert_eq!(events.len(), SESSION_ROUNDS * SESSION_EVENT_VARIANTS);
    assert_eq!(
        type_counts.len(),
        SESSION_EVENT_VARIANTS,
        "the corpus must cover every SessionEvent variant: {type_counts:?}"
    );
    for kind in SESSION_EVENT_WIRE_TYPES {
        assert_eq!(
            type_counts.get(*kind).copied(),
            Some(SESSION_ROUNDS),
            "corpus coverage for {kind}"
        );
    }

    let mut witnesses = NastyByteWitnesses::default();
    for (index, event) in events.iter().enumerate() {
        let value = session_event_to_json(event);
        let value_line = serde_json::to_string(&value).expect("value line serializes");
        let borrowed = to_session_event_line(event).expect("borrowed line serializes");
        witnesses.witness(&borrowed);
        assert_lines_identical("session event", index, &borrowed, &value_line);
        let iterative = pir::session_manager::json_depth::write_value_iteratively(&value);
        assert_lines_identical(
            "session event (iterative writer)",
            index,
            &borrowed,
            &iterative,
        );
    }
    witnesses.assert_complete();
}

#[cfg(test)]
mod wire_size_tests {
    //! The size hint is a pure capacity optimization: these tests pin that it
    //! never under-sizes the text [`to_json_event_line`] produces (an
    //! under-sized hint only means the serializer grows as before) and that
    //! the escaping math matches `serde_json`'s own output byte for byte.

    use super::*;
    use pir::ai::types::{
        AssistantRole, TextContent, ToolCall, ToolResultRole, UserMessage, UserRole,
    };

    fn text_block(text: String) -> ContentBlock {
        ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })
    }

    #[test]
    fn json_string_len_matches_serde_json_escaping() {
        let cases = [
            "",
            "plain",
            "quote\" and \\ backslash",
            "tab\t nl\n cr\r backspace\u{8} formfeed\u{c}",
            "control \u{0} \u{1f}",
            "del \u{7f}",
            "unicode: é 你好 🚀",
            "mixed \"\n\\\u{2} end",
        ];
        for case in cases {
            let actual = serde_json::to_string(case).expect("serialize string").len();
            assert_eq!(json_string_len(case), actual, "hint drifted for {case:?}");
        }
    }

    #[test]
    fn json_value_len_upper_bounds_serde_json() {
        let values = [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(-12_345_678_901_i64),
            serde_json::json!(1.5e-300),
            serde_json::json!("a \"quoted\"\nstring"),
            serde_json::json!([1, "two", {"three": [4, 5, {"six": null}]}]),
            serde_json::json!({"k": {"nested": ["x", {"deep": "y\nz"}]}}),
        ];
        for value in values {
            let actual = serde_json::to_string(&value)
                .expect("serialize value")
                .len();
            let hint = json_value_len(&value);
            assert!(
                hint >= actual,
                "hint {hint} under-sized the serialization ({actual} bytes) for {value}"
            );
        }
    }

    /// Every event whose payload carries text must get a hint at least as
    /// large as the line the borrowed wire view produces.
    #[test]
    fn wire_size_hint_covers_every_large_event() {
        let long_text = "x".repeat(4096) + "\n\"escaped\t";
        let user = Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block(long_text.clone())],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 0,
        });
        let tool_result = ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "big".into(),
            content: vec![text_block(long_text.clone())],
            details: Some(serde_json::json!({"truncation": {"content": long_text.clone()}})),
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: Some(serde_json::json!({"text": long_text.clone()})),
            nested_calls: None,
            is_error: false,
            duration_ms: None,
            timestamp: 0,
        };
        let assistant = AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![
                text_block(long_text.clone()),
                ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "big".into(),
                    arguments: [("path".to_owned(), serde_json::json!(long_text.clone()))]
                        .into_iter()
                        .collect(),
                    thought_signature: Some("sig".into()),
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: "mock".into(),
            provider: "mock".into(),
            model: "mock-model".into(),
            stop_reason: pir::ai::types::StopReason::Stop,
            ..AssistantMessage::default()
        };
        let tool_execution = pir::agent::tool_batch::ToolExecutionEvent::End {
            index: 0,
            call_id: "call-1".into(),
            tool_name: "big".into(),
            is_error: false,
            duration_ms: Some(3),
            parent_call_id: None,
        };
        let events = [
            AgentEvent::MessageStart {
                turn: 1,
                message: user.clone(),
            },
            AgentEvent::MessageEnd {
                turn: 1,
                message: Message::ToolResult(tool_result.clone()),
            },
            AgentEvent::TurnEnd {
                turn: 1,
                message: assistant.clone(),
                tool_results: vec![tool_result.clone()],
            },
            AgentEvent::StreamStart {
                turn: 1,
                message: assistant.clone(),
            },
            AgentEvent::ToolExecutionStart {
                turn: 1,
                event: pir::agent::tool_batch::ToolExecutionEvent::Start {
                    index: 0,
                    call_id: "call-1".into(),
                    tool_name: "big".into(),
                    parent_call_id: None,
                },
                args: serde_json::json!({"path": long_text}),
            },
            AgentEvent::ToolExecutionEnd {
                turn: 1,
                event: tool_execution,
                result: serde_json::json!({"content": [{"type": "text", "text": "y".repeat(4096)}]}),
            },
            AgentEvent::AgentEnd {
                result: std::sync::Arc::new(pir::agent::agent_loop::LoopResult {
                    terminal: assistant,
                    context: std::sync::Arc::new(pir::ai::types::Context {
                        system_prompt: None,
                        messages: vec![user.clone()],
                        tools: None,
                    }),
                    tool_results: Vec::new(),
                    new_messages: vec![user],
                    preamble: Vec::new(),
                    turns: 1,
                    aborted: false,
                    usage_total: 0,
                    will_retry: false,
                }),
            },
        ];
        for event in &events {
            let line = to_json_event_line(event).expect("borrowed line serializes");
            let hint = AgentEventWire::new(event).wire_size_hint();
            assert!(
                hint >= line.len(),
                "hint {hint} under-sized the {line_len}-byte line for {event:?}",
                line_len = line.len()
            );
            let value_line = serde_json::to_string(&to_json_event(event)).expect("value line");
            assert_eq!(
                line, value_line,
                "the pre-sized line drifted from the Value construction for {event:?}"
            );
        }
    }
}

#[cfg(test)]
mod session_wire_size_tests {
    //! The session-line sibling of [`wire_size_tests`]: the
    //! [`SessionEvent::wire_size_hint`] pre-sizing of `to_session_event_line`
    //! (the `entry_appended` growth churn w2 left as the PIR-BIN-099
    //! follow-up) is a pure capacity optimization. These tests pin that the
    //! hint never under-sizes the text the production seam produces for the
    //! payload-carrying variants (an under-sized hint only means the
    //! serializer grows as before) and that the pre-sized line still matches
    //! the `Value` construction byte for byte.

    use super::*;
    use pir::agent_session::SummarizationRetrySource;
    use pir::ai::types::{
        AssistantRole, Cost, ImageContent, StopReason, TextContent, ToolResultRole, Usage,
        UserMessage, UserRole,
    };

    fn text_block(text: String) -> ContentBlock {
        ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })
    }

    /// The `{id, message}` entry the print/RPC hosts build for
    /// `entry_appended`: the appended message in its serialized wire shape.
    fn entry_with(message: &Message) -> serde_json::Value {
        serde_json::json!({
            "id": "entry-1",
            "message": serde_json::to_value(message).expect("message wire shape"),
        })
    }

    /// Every session event whose payload can carry real bytes must get a hint
    /// at least as large as the line the production seam produces.
    #[test]
    fn session_wire_size_hint_covers_every_payload_carrying_event() {
        let long_text = "x".repeat(4096) + "\n\"escaped\t\u{7f}";
        let user = Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block(long_text.clone())],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 0,
        });
        let assistant = Message::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![text_block(long_text.clone())],
            api: "mock".into(),
            provider: "mock".into(),
            model: "mock-model".into(),
            stop_reason: StopReason::Stop,
            ..AssistantMessage::default()
        });
        let tool_result = Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "big".into(),
            content: vec![text_block(long_text.clone())],
            details: Some(serde_json::json!({"truncation": {"content": long_text.clone()}})),
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: Some(serde_json::json!({"text": long_text.clone()})),
            nested_calls: None,
            is_error: false,
            duration_ms: Some(5),
            timestamp: 0,
        });

        let events = [
            // `entry_appended` with the appended message in its wire shape:
            // the large tool-result line the issue measured, plus the other
            // message kinds the same host path appends.
            SessionEvent::EntryAppended {
                entry: entry_with(&tool_result),
            },
            SessionEvent::EntryAppended {
                entry: entry_with(&user),
            },
            SessionEvent::EntryAppended {
                entry: entry_with(&assistant),
            },
            SessionEvent::QueueUpdate {
                steering: vec![long_text.clone(), "next".into()],
                follow_up: vec!["follow".repeat(256)],
                delivered_steering: vec![long_text.clone()],
                delivered_follow_up: Vec::new(),
            },
            SessionEvent::CompactionStart {
                reason: "threshold",
            },
            SessionEvent::CompactionEnd {
                reason: "overflow",
                result: Some(serde_json::json!({
                    "summary": long_text.clone(),
                    "firstKeptEntryId": "entry-1",
                })),
                aborted: false,
                will_retry: true,
                error_message: Some("retryable \u{1} error".into()),
            },
            SessionEvent::AgentSettled { aborted: false },
            SessionEvent::SessionInfoChanged {
                name: Some("named session".into()),
            },
            SessionEvent::SessionInfoChanged { name: None },
            SessionEvent::ThinkingLevelChanged {
                level: "high".into(),
            },
            SessionEvent::HistoryTrimmed {
                dropped_messages: 12_345,
                retained_messages: 678,
                max_messages: 900,
                max_tokens: u64::MAX,
                reason: "tokens",
                total_dropped_messages: usize::MAX,
            },
            SessionEvent::BashExecutionUpdate {
                id: Some("bash-1".into()),
                delta: long_text.clone(),
            },
            SessionEvent::BackgroundToolEnd {
                call_id: "call-9".into(),
                tool_name: "big".into(),
                parent_call_id: Some("call-parent".into()),
                is_error: false,
                duration_ms: Some(41),
                output: long_text.clone(),
                images: vec![ImageContent {
                    data: "a".repeat(1024),
                    mime_type: "image/png".into(),
                }],
                details: Some(
                    serde_json::json!({"content": [{"type": "text", "text": long_text.clone()}]}),
                ),
                structured_content: Some(serde_json::json!({"structured": long_text.clone()})),
                // The widest `Usage` scalar forms: u64 max, f64 extremes.
                usage: Some(Usage {
                    input: u64::MAX,
                    output: u64::MAX,
                    cache_read: u64::MAX,
                    cache_write: u64::MAX,
                    cache_write_1h: Some(u64::MAX),
                    reasoning: Some(u64::MAX),
                    total_tokens: u64::MAX,
                    cost: Cost {
                        input: f64::MAX,
                        output: -f64::MAX,
                        cache_read: f64::MIN_POSITIVE,
                        cache_write: -0.0,
                        total: f64::MAX,
                        total_from_provider: Some(true),
                    },
                }),
            },
            SessionEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 500,
                error_message: "transport \u{2} error".into(),
            },
            SessionEvent::AutoRetryEnd {
                success: false,
                attempt: 3,
                final_error: Some("final \u{3}".into()),
            },
            SessionEvent::SummarizationRetryScheduled {
                attempt: 2,
                max_attempts: 4,
                delay_ms: 10,
                error_message: "summary error".into(),
            },
            SessionEvent::SummarizationRetryAttemptStart {
                source: SummarizationRetrySource::BranchSummary,
            },
            SessionEvent::SummarizationRetryAttemptStart {
                source: SummarizationRetrySource::Compaction { reason: "manual" },
            },
            SessionEvent::SummarizationRetryFinished,
        ];
        for event in &events {
            let line = pir::agent_session::to_session_event_line(event)
                .expect("session event line serializes");
            let hint = event.wire_size_hint();
            assert!(
                hint >= line.len(),
                "hint {hint} under-sized the {line_len}-byte line for {event:?}",
                line_len = line.len()
            );
            let value_line = serde_json::to_string(&session_event_to_json(event))
                .expect("value line serializes");
            assert_eq!(
                line, value_line,
                "the pre-sized line drifted from the Value construction for {event:?}"
            );
        }
    }
}
