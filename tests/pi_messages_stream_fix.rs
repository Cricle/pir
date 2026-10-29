//! pi-messages stream-adapter fixes (upstream
//! `packages/ai/src/api/pi-messages.ts`): chunk-safe UTF-8 framing, cumulative
//! tool-call argument parsing, the end-of-stream flush, and the terminal
//! `done.reason` carry-through.
//!
//! The incremental tests drive [`PiMessagesParser`] exactly like the stream
//! driver does (seed `Start`, `feed` per transport chunk, then `finish`).

use std::collections::BTreeMap;

use pir::ai::api::pi_messages::{parse_pi_messages_sse, PiMessagesParser};
use pir::ai::error::AiError;
use pir::ai::types::*;
use serde_json::{json, Value};

fn sample_model() -> Model {
    Model {
        id: "pi-model".into(),
        name: "Pi model".into(),
        api: "pi-messages".into(),
        provider: "radius".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 1024,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// The wire usage object of a terminal event.
fn usage_json() -> Value {
    json!({
        "input": 12,
        "output": 3,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 15,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0},
    })
}

/// One `data: <event>` SSE block.
fn sse(event: Value) -> String {
    format!("data: {event}\n\n")
}

fn sse_done(reason: &str) -> String {
    sse(json!({"type": "done", "reason": reason, "usage": usage_json()}))
}

/// Drive the incremental parser exactly like the stream driver does.
fn feed_chunks(chunks: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = PiMessagesParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for chunk in chunks {
        parser.feed(chunk, &mut out).expect("feed succeeds");
    }
    parser.finish(&mut out).expect("finish succeeds");
    out
}

/// One byte per chunk: every multi-byte UTF-8 character and every framing
/// token is split across chunk boundaries.
fn byte_chunks(body: &str) -> Vec<Vec<u8>> {
    body.as_bytes().iter().map(|byte| vec![*byte]).collect()
}

fn delta_text(events: &[AssistantMessageEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn done_of(events: &[AssistantMessageEvent]) -> (TerminalReason, AssistantMessage) {
    match events.last() {
        Some(AssistantMessageEvent::Done { reason, message }) => (*reason, message.clone()),
        other => panic!("expected a terminal Done event, got {other:?}"),
    }
}

fn toolcall_delta_indices(events: &[AssistantMessageEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolCallDelta { content_index, .. } => Some(*content_index),
            _ => None,
        })
        .collect()
}

/// The tool-call arguments carried by the `occurrence`-th `toolcall_delta`
/// event's partial snapshot.
fn partial_tool_arguments(
    events: &[AssistantMessageEvent],
    occurrence: usize,
) -> BTreeMap<String, Value> {
    let partial = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolCallDelta { partial, .. } => Some(partial.clone()),
            _ => None,
        })
        .nth(occurrence)
        .expect("toolcall delta present");
    partial
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.arguments.clone()),
            _ => None,
        })
        .expect("tool call block in the partial")
}

fn toolcall_block(message: &AssistantMessage) -> ToolCall {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("tool call block present")
}

fn args(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Chunked UTF-8 (CJK + emoji split across chunks)
// ---------------------------------------------------------------------------

#[test]
fn chunked_utf8_cjk_and_emoji_survive_byte_by_byte_feeds() {
    let model = sample_model();
    let body: String = [
        sse(json!({"type": "text_start", "contentIndex": 0})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "你好"})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "🌍世界"})),
        sse_done("stop"),
    ]
    .concat();
    // Byte-by-byte feeding splits every CJK code point and the emoji across
    // chunk boundaries; a lossy decode of the buffered tail would corrupt
    // them permanently (or fail the JSON parse outright).
    let events = feed_chunks(&byte_chunks(&body), &model);
    assert_eq!(delta_text(&events), "你好🌍世界");
    let (_, message) = done_of(&events);
    let Some(ContentBlock::Text(text)) = message.content.first() else {
        panic!("text block expected, got {:?}", message.content);
    };
    assert_eq!(text.text, "你好🌍世界");
    // The single-feed batch path agrees.
    let batch = parse_pi_messages_sse(body.as_bytes(), &model).expect("batch parses");
    assert_eq!(delta_text(&batch), "你好🌍世界");
}

// ---------------------------------------------------------------------------
// 2. Cumulative tool-call argument parsing
// ---------------------------------------------------------------------------

#[test]
fn toolcall_deltas_accumulate_fragments_and_pin_the_final_value() {
    let model = sample_model();
    let body: String = [
        sse(json!({
            "type": "toolcall_start",
            "contentIndex": 0,
            "id": "call_1",
            "toolName": "get_weather"
        })),
        sse(json!({"type": "toolcall_delta", "contentIndex": 0, "delta": "{\"city\":\"Par"})),
        sse(json!({"type": "toolcall_delta", "contentIndex": 0, "delta": "is\",\"days\":3}"})),
        sse(json!({
            "type": "toolcall_end",
            "contentIndex": 0,
            "toolCall": {
                "id": "call_1",
                "name": "get_weather",
                "arguments": {"city": "Paris", "days": 3}
            }
        })),
        sse_done("toolUse"),
    ]
    .concat();
    let events = feed_chunks(&byte_chunks(&body), &model);
    assert_eq!(toolcall_delta_indices(&events), vec![0, 0]);
    // First fragment: the open string literal repairs to its partial value.
    assert_eq!(
        partial_tool_arguments(&events, 0),
        args(&[("city", json!("Par"))])
    );
    // The second fragment closes the object, so the mirror refreshes at that
    // boundary: the delta carries the complete accumulated arguments (the
    // throttle only skips below-cadence mid-object fragments).
    assert_eq!(
        partial_tool_arguments(&events, 1),
        args(&[("city", json!("Paris")), ("days", json!(3))])
    );
    let (reason, message) = done_of(&events);
    assert_eq!(reason, TerminalReason::ToolUse);
    assert_eq!(
        toolcall_block(&message).arguments,
        args(&[("city", json!("Paris")), ("days", json!(3))])
    );
}

#[test]
fn toolcall_delta_keys_by_wire_content_index_with_toolcall_fallback() {
    let model = sample_model();
    // The delta's wire `contentIndex` (5) differs from the last content-block
    // position (1): keying by position would miss the tool-call block.
    let body: String = [
        sse(json!({"type": "text_start", "contentIndex": 0})),
        sse(json!({"type": "text_end", "contentIndex": 0, "content": "note"})),
        sse(json!({
            "type": "toolcall_start",
            "contentIndex": 5,
            "id": "call_9",
            "toolName": "search"
        })),
        sse(json!({"type": "toolcall_delta", "contentIndex": 5, "delta": "{\"q\":\"x\"}"})),
        sse_done("stop"),
    ]
    .concat();
    let events = feed_chunks(&byte_chunks(&body), &model);
    assert_eq!(toolcall_delta_indices(&events), vec![5]);
    assert_eq!(
        partial_tool_arguments(&events, 0),
        args(&[("q", json!("x"))])
    );

    // Payloads without the wire field fall back to the most recent tool-call
    // block.
    let body: String = [
        sse(json!({
            "type": "toolcall_start",
            "contentIndex": 0,
            "id": "call_8",
            "toolName": "search"
        })),
        sse(json!({"type": "toolcall_delta", "delta": "{\"q\":\"y\"}"})),
        sse_done("stop"),
    ]
    .concat();
    let events = feed_chunks(&byte_chunks(&body), &model);
    assert_eq!(toolcall_delta_indices(&events), vec![0]);
    assert_eq!(
        partial_tool_arguments(&events, 0),
        args(&[("q", json!("y"))])
    );
}

// ---------------------------------------------------------------------------
// 3. End-of-stream flush (missing trailing blank line)
// ---------------------------------------------------------------------------

#[test]
fn final_block_without_trailing_blank_line_is_flushed_at_finish() {
    let model = sample_model();
    // The `done` block arrives without its closing blank line (and, in the
    // second variant, without any trailing newline at all).
    for suffix in ["", "\n"] {
        let body: String = [
            sse(json!({"type": "text_start", "contentIndex": 0})),
            sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "hi"})),
            format!(
                "data: {}{suffix}",
                json!({"type": "done", "reason": "stop", "usage": usage_json()})
            ),
        ]
        .concat();
        let events = feed_chunks(&byte_chunks(&body), &model);
        let (reason, message) = done_of(&events);
        assert_eq!(reason, TerminalReason::Stop, "suffix {suffix:?}");
        assert_eq!(message.stop_reason, StopReason::Stop, "suffix {suffix:?}");
        assert_eq!(delta_text(&events), "hi");
        // The batch entry point shares the same finish path.
        let batch = parse_pi_messages_sse(body.as_bytes(), &model).expect("batch parses");
        let (reason, _) = done_of(&batch);
        assert_eq!(reason, TerminalReason::Stop, "suffix {suffix:?}");
    }
}

#[test]
fn stream_without_a_terminal_event_still_fails() {
    let model = sample_model();
    let no_done = sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "x"}));
    let error = parse_pi_messages_sse(no_done.as_bytes(), &model).expect_err("no terminal event");
    assert!(matches!(error, AiError::StreamNotTerminated));
    // The incremental path (feed then finish) fails the same way.
    let mut parser = PiMessagesParser::new(&model);
    let mut out = Vec::new();
    parser
        .feed(no_done.as_bytes(), &mut out)
        .expect("feed succeeds");
    let error = parser.finish(&mut out).expect_err("no terminal event");
    assert!(matches!(error, AiError::StreamNotTerminated));
}

// ---------------------------------------------------------------------------
// 4. Terminal `done.reason` carry-through
// ---------------------------------------------------------------------------

#[test]
fn done_reason_length_and_tool_use_are_carried_through() {
    let model = sample_model();
    for (wire, terminal, stop) in [
        ("stop", TerminalReason::Stop, StopReason::Stop),
        ("length", TerminalReason::Length, StopReason::Length),
        ("toolUse", TerminalReason::ToolUse, StopReason::ToolUse),
    ] {
        let body = sse_done(wire);
        let events = parse_pi_messages_sse(body.as_bytes(), &model).expect("parses");
        let (reason, message) = done_of(&events);
        assert_eq!(reason, terminal, "wire reason {wire}");
        assert_eq!(message.stop_reason, stop, "wire reason {wire}");
    }
}

// ---------------------------------------------------------------------------
// Incremental vs batch equivalence
// ---------------------------------------------------------------------------

/// Strip `timestamp` fields recursively: parsers stamp wall-clock times into
/// each event's `partial`, which legitimately differ between runs.
fn strip_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("timestamp");
            for child in map.values_mut() {
                strip_timestamps(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_timestamps(child);
            }
        }
        _ => {}
    }
}

fn event_key(event: &AssistantMessageEvent) -> String {
    let mut value = serde_json::to_value(event).expect("event serializes");
    strip_timestamps(&mut value);
    serde_json::to_string(&value).expect("canonical form serializes")
}

#[test]
fn incremental_feeding_matches_the_batch_path_across_split_boundaries() {
    let model = sample_model();
    let body: String = [
        sse(json!({"type": "text_start", "contentIndex": 0})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "He"})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "llo 你好 🌍"})),
        sse(json!({"type": "text_end", "contentIndex": 0, "content": "Hello 你好 🌍"})),
        sse(json!({"type": "thinking_start"})),
        sse(json!({"type": "thinking_delta", "delta": "hmm "})),
        sse(json!({"type": "thinking_end", "content": "hmm done", "thinkingSignature": "sig-1"})),
        sse(json!({
            "type": "toolcall_start",
            "contentIndex": 2,
            "id": "call_1",
            "toolName": "search"
        })),
        sse(json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "{\"q\":\"ru"})),
        sse(json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "st\"}"})),
        sse(json!({
            "type": "toolcall_end",
            "contentIndex": 2,
            "toolCall": {"id": "call_1", "name": "search", "arguments": {"q": "rust"}}
        })),
        sse_done("length"),
    ]
    .concat();
    let batch = parse_pi_messages_sse(body.as_bytes(), &model).expect("batch parses");
    let chunks: Vec<Vec<u8>> = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let incremental = feed_chunks(&chunks, &model);
    assert_eq!(batch.len(), incremental.len(), "event count mismatch");
    for (index, (batch_event, stream_event)) in batch.iter().zip(&incremental).enumerate() {
        assert_eq!(
            event_key(batch_event),
            event_key(stream_event),
            "event {index} mismatch (batch vs incremental)"
        );
    }
}
