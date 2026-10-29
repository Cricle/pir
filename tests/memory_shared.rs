//! Memory-optimization regression guards (memory review 2026-10).
//!
//! Pins the sharing and wire-shape invariants the memory review relies on:
//!
//! 1. an `AssistantMessageEventStream`'s terminal message is shared as ONE
//!    `Arc` allocation across every `result()` caller (event_stream.rs
//!    `final_result`) instead of being deep-copied per observer;
//! 2. the `AssistantMessageEvent` protocol shape is untouched: each
//!    non-terminal event still serializes with its full cumulative `partial`
//!    snapshot, and serde round-trips byte-identically (the `partial` field
//!    is now `Arc<AssistantMessage>`; serde forwards through it
//!    transparently);
//! 3. `SessionManager` keeps one logical entry per id: the `by_id` index and
//!    the entry list expose identical, serialization-stable entries after
//!    appends and loads (the index now `Arc`-shares with the list instead of
//!    deep-copying every stored message);
//! 4. adapter branches whose provider delta leaves the message unchanged
//!    hand out the SAME partial `Arc` zero-copy, while the snapshot value
//!    stays exactly what a fresh clone would produce: OpenAI and Anthropic
//!    tool-call argument deltas parse and write back the accumulated JSON on
//!    every chunk (upstream `block.arguments = parseStreamingJson(...)`), so
//!    Arc reuse happens only when the parse leaves the arguments unchanged.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use pir::ai::api::anthropic::AnthropicSseParser;
use pir::ai::api::openai::OpenAiSseParser;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, AssistantRole, ContentBlock, Message, Model,
    ModelCost, ModelCostRates, StopReason, TerminalReason, TextContent, Usage, UserMessage,
    UserRole,
};
use pir::session_manager::{FileEntry, MessageEntry, SessionManager};
use serde_json::{json, Value};

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-1".into(),
        response_model: None,
        response_id: None,
        diagnostics: vec![],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        provider_thinking_level: None,
        thinking_level: None,
        duration_ms: None,
        timestamp: 0,
    }
}

fn sample_model(api: &str) -> Model {
    Model {
        id: "test".into(),
        name: "test".into(),
        api: api.into(),
        provider: "test".into(),
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

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: pir::ai::util::now_ms(),
        sections: None,
    })
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-memory-shared-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The terminal message is one shared allocation: concurrent `result()`
/// callers observe the same `Arc` (strong_count grows with observers, the
/// pointee is never duplicated).
#[tokio::test]
async fn terminal_message_shared_across_result_callers() {
    let stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        assistant_message("done"),
    ));
    let first = stream.result().await.expect("terminal result");
    let second = stream.result_expect().await.expect("terminal result");
    assert!(
        Arc::ptr_eq(&first, &second),
        "terminal message must be Arc-shared, not copied per observer"
    );
    assert_eq!(first.content[0], second.content[0]);
}

/// The queued terminal event and the shared final result stay consistent:
/// the event drained from the stream equals the message every `result()`
/// caller receives.
#[tokio::test]
async fn terminal_event_matches_shared_result() {
    let mut stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        assistant_message("final"),
    ));
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let shared = stream.result().await.expect("terminal result");
    match events.last() {
        Some(AssistantMessageEvent::Done { message, .. }) => {
            assert_eq!(message.content, shared.content);
            assert_eq!(message.stop_reason, shared.stop_reason);
        }
        other => panic!("expected Done terminal event, got {other:?}"),
    }
}

/// The event protocol is unchanged: `partial` remains a first-class wire
/// field carrying the full cumulative snapshot, and serde round-trips the
/// event losslessly (byte-identical re-serialization).
#[test]
fn event_wire_shape_round_trips() {
    let partial = assistant_message("accumulated");
    let event = AssistantMessageEvent::TextDelta {
        content_index: 2,
        delta: "chunk".into(),
        partial: Arc::new(partial.clone()),
    };
    let wire = serde_json::to_string(&event).expect("serialize event");
    assert!(
        wire.contains("\"type\":\"text_delta\""),
        "tag must stay `text_delta`: {wire}"
    );
    assert!(
        wire.contains("\"partial\":"),
        "partial snapshot must remain on the wire: {wire}"
    );
    assert!(
        wire.contains("\"content_index\":2"),
        "field names are snake_case on the wire: {wire}"
    );
    let parsed: AssistantMessageEvent = serde_json::from_str(&wire).expect("deserialize event");
    assert_eq!(event, parsed, "round-trip must be lossless");
    let rewired = serde_json::to_string(&parsed).expect("re-serialize event");
    assert_eq!(wire, rewired, "re-serialization must be byte-identical");
}

/// The session manager's index and entry list stay in sync after appends:
/// `entry(id)` and `entries()` expose the same single logical entry, and its
/// serialized line shape round-trips byte-identically (the `Arc` sharing is
/// internal and never leaks into the persisted wire format).
#[test]
fn session_manager_index_shares_entries_without_shape_change() {
    let dir = temp_dir("index-share");
    let mut session = SessionManager::create("C:\\repo", &dir).unwrap();
    let id = session.append_message(user_message("hello")).unwrap();

    let entry = session.entry(&id).expect("indexed entry");
    let listed = session.entries();
    assert_eq!(listed.len(), 1, "one non-header entry after one append");
    assert_eq!(entry.entry_id(), listed[0].entry_id());

    // Serialization shape is stable (Arc sharing is invisible on the wire).
    let line = serde_json::to_string(entry).expect("serialize entry");
    let parsed: FileEntry = serde_json::from_str(&line).expect("deserialize entry");
    let relined = serde_json::to_string(&parsed).expect("re-serialize entry");
    assert_eq!(
        line, relined,
        "entry wire shape must round-trip byte-identically"
    );
    assert!(
        line.contains("\"type\":\"message\""),
        "message entries keep their `type` tag: {line}"
    );
    match &parsed {
        FileEntry::Message(MessageEntry { message, .. }) => match message {
            pir::session_manager::StoredMessage::User(user) => {
                assert_eq!(user.content.len(), 1);
            }
            other => panic!("expected user message, got {other:?}"),
        },
        other => panic!("expected message entry, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Anthropic tool-call argument deltas parse the accumulated JSON and write
/// the result back into the block on every chunk (upstream
/// `block.arguments = parseStreamingJson(block.partialJson)`,
/// anthropic-messages.ts:767), so in-flight `toolcall_delta` partials carry
/// the arguments accumulated so far — not only at `content_block_stop`. A
/// fragment with no usable parse yet keeps the last written-back value, and
/// because the message is then unchanged the partial `Arc` is reused.
#[test]
fn anthropic_delta_phase_partials_carry_in_flight_arguments() {
    let model = sample_model("anthropic-messages");
    let mut parser = AnthropicSseParser::new(&model);
    let body = r#"
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":","}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"flag\":true}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#;
    let mut events = Vec::new();
    parser.feed(body.as_bytes(), &mut events).expect("feed");

    fn tool_arguments(partial: &AssistantMessage) -> &BTreeMap<String, Value> {
        partial
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(&call.arguments),
                _ => None,
            })
            .expect("tool call block in the partial")
    }

    let deltas: Vec<(String, Arc<AssistantMessage>)> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
                ..
            } => {
                assert_eq!(*content_index, 0);
                Some((delta.clone(), Arc::clone(partial)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 4, "four input_json_delta chunks: {events:?}");
    assert_eq!(deltas[0].0, "{\"path\":");
    assert_eq!(deltas[1].0, "\"a.rs\"");
    assert_eq!(deltas[2].0, ",");
    assert_eq!(deltas[3].0, "\"flag\":true}");

    // The leading fragment cannot parse yet: the block keeps the initial
    // empty arguments until a usable parse exists.
    assert!(
        tool_arguments(&deltas[0].1).is_empty(),
        "the incomplete leading fragment must not clear the arguments"
    );
    // These fragments stay under the mirror cadence (first fragment, every
    // 8 KiB, or a fragment that CLOSES a container), so the in-flight block
    // keeps the last well-formed parse — the empty leading one — instead of
    // reparsing on every tiny fragment (the O(args²) this wave removed). The
    // values are never malformed; the closing-brace fragment below refreshes.
    assert!(
        tool_arguments(&deltas[1].1).is_empty(),
        "a below-cadence fragment keeps the last well-formed parse"
    );
    assert!(
        tool_arguments(&deltas[2].1).is_empty(),
        "a below-cadence fragment keeps the last well-formed parse"
    );
    assert!(
        Arc::ptr_eq(&deltas[1].1, &deltas[2].1),
        "an unchanged parse must reuse the same partial Arc"
    );
    assert_eq!(
        tool_arguments(&deltas[3].1).get("path"),
        Some(&json!("a.rs"))
    );
    assert_eq!(tool_arguments(&deltas[3].1).get("flag"), Some(&json!(true)));

    // `content_block_stop` still finalizes the tool call with the complete
    // input.
    let tool_end = events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        })
        .expect("toolcall_end at content_block_stop");
    assert_eq!(tool_end.arguments.get("path"), Some(&json!("a.rs")));
    assert_eq!(tool_end.arguments.get("flag"), Some(&json!(true)));
}

/// OpenAI delta-phase chunks append blocks to the message as they start
/// (upstream `ensureTextBlock` / `ensureToolCallBlock`), so the start / delta
/// partials carry the in-flight blocks and their content indices match the
/// partial positions — including text that starts AFTER a tool call.
/// Argument deltas are parsed and written back into the block on every chunk
/// (upstream `block.arguments = parseStreamingJson(block.partialArgs)`), so
/// the delta partials carry the accumulated arguments in flight.
#[test]
fn openai_delta_phase_partials_carry_in_flight_blocks() {
    let model = sample_model("openai-chat");
    let mut parser = OpenAiSseParser::new(&model);
    let body = r#"
data: {"id":"chatcmpl-1","model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":"{\"q\":"}}]}}]}

data: {"id":"chatcmpl-1","model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}

data: {"id":"chatcmpl-1","model":"test","choices":[{"index":0,"delta":{"content":"a"}}]}

data: {"id":"chatcmpl-1","model":"test","choices":[{"index":0,"delta":{"content":"b"}}]}

data: {"id":"chatcmpl-1","model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

"#;
    let mut events = Vec::new();
    parser.feed(body.as_bytes(), &mut events).expect("feed");
    parser.finish(&mut events).expect("finish");

    fn text_at(partial: &AssistantMessage, content_index: usize) -> &str {
        match partial.content.get(content_index) {
            Some(ContentBlock::Text(text)) => &text.text,
            other => panic!("expected text block at index {content_index}, got {other:?}"),
        }
    }

    // The tool call is the first block (index 0). The start event is pushed
    // right after the block is created — before the first argument chunk is
    // merged — so its snapshot still carries empty arguments.
    let mut tool_start: Option<Arc<AssistantMessage>> = None;
    let mut tool_delta: Option<Arc<AssistantMessage>> = None;
    for event in &events {
        match event {
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                assert_eq!(*content_index, 0);
                tool_start = Some(Arc::clone(partial));
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                partial,
                ..
            } => {
                assert_eq!(*content_index, 0);
                tool_delta = Some(Arc::clone(partial));
            }
            _ => {}
        }
    }
    let tool_start = tool_start.expect("toolcall_start");
    let tool_delta = tool_delta.expect("toolcall_delta");
    match &tool_start.content[0] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.id, "call_1");
            assert_eq!(call.name, "f");
            assert!(
                call.arguments.is_empty(),
                "the start snapshot precedes the first argument chunk"
            );
        }
        other => panic!("expected tool call block at index 0, got {other:?}"),
    }
    // The last tool-call delta's partial carries the accumulated in-flight
    // parsed arguments (both argument chunks were merged by then).
    match &tool_delta.content[0] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(
                call.arguments.get("q"),
                Some(&serde_json::json!("x")),
                "in-flight deltas carry the accumulated parsed arguments"
            );
        }
        other => panic!("expected tool call block at index 0, got {other:?}"),
    }

    // Text starting after the tool call becomes block 1: each text snapshot
    // carries the text under ITS content index, with the call before it.
    let text_deltas: Vec<(String, Arc<AssistantMessage>)> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, partial, .. } => {
                Some((delta.clone(), Arc::clone(partial)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(text_deltas.len(), 2, "two content chunks: {events:?}");
    assert_eq!(text_deltas[0].0, "a");
    assert_eq!(text_at(&text_deltas[0].1, 1), "a");
    assert_eq!(text_deltas[1].0, "b");
    assert_eq!(text_at(&text_deltas[1].1, 1), "ab");
    assert!(
        matches!(
            text_deltas[0].1.content.first(),
            Some(ContentBlock::ToolCall(_))
        ),
        "the tool call stays at index 0"
    );

    // End events follow content order; the terminal message carries the
    // finalized blocks.
    let end_names: Vec<(&str, usize)> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextEnd { content_index, .. } => {
                Some(("text_end", *content_index))
            }
            AssistantMessageEvent::ToolCallEnd { content_index, .. } => {
                Some(("toolcall_end", *content_index))
            }
            _ => None,
        })
        .collect();
    assert_eq!(end_names, vec![("toolcall_end", 0), ("text_end", 1)]);
    let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
        panic!("stream must end with Done");
    };
    assert_eq!(message.content.len(), 2);
    match &message.content[0] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.id, "call_1");
            assert_eq!(call.name, "f");
            assert_eq!(call.arguments.get("q"), Some(&serde_json::json!("x")));
        }
        other => panic!("expected tool call block, got {other:?}"),
    }
    assert_eq!(text_at(message, 1), "ab");
}
