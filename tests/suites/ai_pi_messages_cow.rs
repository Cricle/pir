//! pi-messages parser: the copy-on-write accumulator and its incremental
//! dense projection.
//!
//! `PiMessagesParser` addresses blocks by the wire `contentIndex` while the
//! message projection its partial snapshots expose stays dense in ascending
//! key order. The projection used to be REBUILT on every delta (each
//! accumulated block deep-cloned into a fresh `Vec`, O(total content) per
//! delta); it is now materialized once per block start/end and mutated in
//! place per delta, so a delta whose snapshots were dropped costs O(delta),
//! independent of the accumulated blocks and text.
//!
//! The tests pin both counters: `copy_count()` (how many mutations had to copy
//! the `Arc` accumulator because a snapshot was outstanding) and
//! `projection_work()` (how many blocks the dense projection materialized —
//! zero for an in-place delta). Note why the copy counter exists instead of a
//! `Weak`-based probe: `Arc::make_mut` copies while ANY `Weak` to the
//! allocation exists, so such a probe would itself create the copy it looks
//! for and measure its own interference.

use std::sync::Arc;

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

/// One `data: <event>` SSE block.
fn sse(event: Value) -> String {
    format!("data: {event}\n\n")
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

/// Feed one single-event chunk.
fn feed_chunk(parser: &mut PiMessagesParser, event: Value) -> Vec<AssistantMessageEvent> {
    let mut out = Vec::new();
    parser
        .feed(sse(event).as_bytes(), &mut out)
        .expect("feed succeeds");
    out
}

/// The partial carried by the last event of `events` (`None` for the terminal
/// events, whose message/error payload is not a `partial`).
fn last_partial(events: &[AssistantMessageEvent]) -> Option<Arc<AssistantMessage>> {
    match events.last()? {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(Arc::clone(partial)),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

/// The text of the first text block of `message` (empty when absent).
fn text_of(message: &AssistantMessage) -> &str {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

/// The texts of every text block, in projection order.
fn texts_of(message: &AssistantMessage) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// The `done` message of an event list.
fn done_message(events: &[AssistantMessageEvent]) -> AssistantMessage {
    match events.last() {
        Some(AssistantMessageEvent::Done { message, .. }) => message.clone(),
        other => panic!("expected a terminal Done event, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Dropped snapshots: no per-delta copy and no per-delta projection rebuild
// ---------------------------------------------------------------------------

/// With the consumer reading and dropping each delta's snapshot, the counters
/// stay at the block-start values no matter how many deltas or how much text
/// accumulates. The previous projection rebuilt every accumulated block per
/// delta, so `projection_work()` would have grown by one per delta here.
#[test]
fn pi_messages_dropped_snapshots_keep_the_delta_cost_flat() {
    let model = sample_model();
    let mut parser = PiMessagesParser::new(&model);

    // `text_start` shares a chunk with its first delta (one of the wire
    // shapes): the block-start snapshot is still queued when the delta
    // mutates, so the accumulator is copied exactly once and the projection
    // materializes exactly one block.
    let mut events = Vec::new();
    let chunk: String = [
        sse(json!({"type": "text_start", "contentIndex": 0})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "t0 "})),
    ]
    .concat();
    parser.feed(chunk.as_bytes(), &mut events).expect("feed");
    assert_eq!(events.len(), 2, "start + delta");
    // The block-start partial stays frozen at the empty text.
    let start_partial = last_partial(&events[..1]).expect("text_start partial");
    assert_eq!(text_of(&start_partial), "");
    assert_eq!(
        text_of(&last_partial(&events).expect("delta partial")),
        "t0 "
    );
    drop(start_partial);
    drop(events);
    assert_eq!(
        parser.copy_count(),
        1,
        "the in-chunk block start forces exactly one accumulator copy"
    );
    assert_eq!(parser.projection_work(), 1, "the block is projected once");

    // One event per chunk, snapshot read then dropped: neither counter may
    // move with the delta count or the accumulated text (~600 bytes here).
    let mut expected = String::from("t0 ");
    for index in 1..=100 {
        let token = format!("t{index} ");
        let events = feed_chunk(
            &mut parser,
            json!({"type": "text_delta", "contentIndex": 0, "delta": token}),
        );
        let snapshot = last_partial(&events).expect("delta partial");
        expected.push_str(&token);
        assert_eq!(text_of(&snapshot), expected, "delta {index} snapshot");
        drop(snapshot);
        drop(events);
    }
    assert_eq!(
        parser.copy_count(),
        1,
        "no copy per delta while snapshots are dropped"
    );
    assert_eq!(
        parser.projection_work(),
        1,
        "the old rebuild cloned every accumulated block on every delta"
    );
}

/// `projection_work()` counts block starts/ends, not deltas: three blocks fed
/// by 90 deltas materialize three times, and the final message still carries
/// every block in ascending-key order.
#[test]
fn pi_messages_projection_work_tracks_blocks_not_deltas() {
    let model = sample_model();
    let mut parser = PiMessagesParser::new(&model);

    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_json = String::new();
    for index in 0..30 {
        if index == 0 {
            drop(feed_chunk(
                &mut parser,
                json!({"type": "text_start", "contentIndex": 0}),
            ));
        }
        let token = format!("t{index} ");
        drop(feed_chunk(
            &mut parser,
            json!({"type": "text_delta", "contentIndex": 0, "delta": token}),
        ));
        text.push_str(&token);
    }
    drop(feed_chunk(&mut parser, json!({"type": "thinking_start"})));
    for index in 0..30 {
        let token = format!("h{index} ");
        drop(feed_chunk(
            &mut parser,
            json!({"type": "thinking_delta", "delta": token}),
        ));
        thinking.push_str(&token);
    }
    drop(feed_chunk(
        &mut parser,
        json!({"type": "toolcall_start", "contentIndex": 2, "id": "call_1", "toolName": "search"}),
    ));
    for fragment in ["{\"q\":", "\"rust\"", "}"] {
        drop(feed_chunk(
            &mut parser,
            json!({"type": "toolcall_delta", "contentIndex": 2, "delta": fragment}),
        ));
        tool_json.push_str(fragment);
    }
    // Sanity: the fixture built complete JSON (so the final arguments parse).
    assert_eq!(tool_json, "{\"q\":\"rust\"}");

    assert_eq!(
        parser.projection_work(),
        3,
        "one materialization per block start; 90 deltas add none"
    );
    assert_eq!(
        parser.copy_count(),
        0,
        "no snapshot is alive when a mutation runs"
    );

    let message = parser.snapshot();
    assert_eq!(message.content.len(), 3, "text, thinking, tool call");
    assert_eq!(texts_of(&message), vec![text.as_str()]);
    match &message.content[1] {
        ContentBlock::Thinking(block) => assert_eq!(block.thinking, thinking),
        other => panic!("expected a thinking block, got {other:?}"),
    }
    match &message.content[2] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.name, "search");
            assert_eq!(call.arguments.get("q"), Some(&json!("rust")));
        }
        other => panic!("expected a tool-call block, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Held snapshots: frozen + complete (the existing COW contract)
// ---------------------------------------------------------------------------

/// A consumer that KEEPS every snapshot still observes exactly the message
/// accumulated at that delta: later mutations copy-on-write instead of
/// touching the held value. Each held delta therefore costs exactly one
/// accumulator copy — the price of the retained snapshots, not of the delta.
#[test]
fn pi_messages_held_snapshots_stay_complete_and_frozen() {
    let model = sample_model();
    let mut parser = PiMessagesParser::new(&model);
    let tokens = ["a", "b", "c", "d"];

    let events = feed_chunk(
        &mut parser,
        json!({"type": "text_start", "contentIndex": 0}),
    );
    let mut held = vec![last_partial(&events).expect("text_start partial")];
    drop(events);
    assert_eq!(text_of(&held[0]), "", "the block start snapshot is empty");

    for (index, token) in tokens.iter().enumerate() {
        let events = feed_chunk(
            &mut parser,
            json!({"type": "text_delta", "contentIndex": 0, "delta": *token}),
        );
        let snapshot = last_partial(&events).expect("delta partial");
        assert_eq!(
            text_of(&snapshot),
            tokens[..=index].concat(),
            "delta {index} snapshot is complete"
        );
        held.push(snapshot);
        drop(events);
    }
    // Every held snapshot shows exactly the prefix from its own delta.
    let mut prefix = String::new();
    for (index, snapshot) in held.iter().enumerate() {
        if index > 0 {
            prefix.push_str(tokens[index - 1]);
        }
        assert_eq!(text_of(snapshot), prefix, "snapshot {index} stays frozen");
    }
    assert_eq!(
        parser.copy_count(),
        tokens.len() as u64,
        "one copy per retained delta, none extra"
    );

    // The terminal mutation copies once more while the last snapshot is held;
    // the terminal message is complete and the held snapshots stay frozen.
    let events = feed_chunk(
        &mut parser,
        json!({"type": "done", "reason": "stop", "usage": usage_json()}),
    );
    let message = done_message(&events);
    assert_eq!(text_of(&message), "abcd");
    assert_eq!(parser.copy_count(), tokens.len() as u64 + 1);
    assert_eq!(
        text_of(&held[4]),
        "abcd",
        "the last held snapshot is unchanged"
    );
    assert_eq!(text_of(&held[0]), "");
}

// ---------------------------------------------------------------------------
// Projection identity vs the batch path (multi-block, non-contiguous index)
// ---------------------------------------------------------------------------

/// Streaming the same body one event per chunk (snapshots dropped) yields the
/// same dense projection the batch path produces, including the block order
/// for a non-contiguous wire `contentIndex` (tool call at 5 after text at 0).
#[test]
fn pi_messages_incremental_projection_matches_the_batch_message() {
    let model = sample_model();
    let body: String = [
        sse(json!({"type": "text_start", "contentIndex": 0})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "He"})),
        sse(json!({"type": "text_delta", "contentIndex": 0, "delta": "llo"})),
        sse(json!({
            "type": "text_end",
            "contentIndex": 0,
            "content": "Hello",
            "contentSignature": "sig-0"
        })),
        sse(json!({"type": "thinking_start"})),
        sse(json!({"type": "thinking_delta", "delta": "hm"})),
        sse(json!({"type": "thinking_delta", "delta": "m"})),
        sse(json!({
            "type": "thinking_end",
            "content": "hmm",
            "thinkingSignature": "sig-1",
            "redacted": false
        })),
        sse(json!({
            "type": "toolcall_start",
            "contentIndex": 5,
            "id": "call_1",
            "toolName": "search"
        })),
        sse(json!({"type": "toolcall_delta", "contentIndex": 5, "delta": "{\"q\":"})),
        sse(json!({"type": "toolcall_delta", "contentIndex": 5, "delta": "\"rust\"}"})),
        sse(json!({
            "type": "toolcall_end",
            "contentIndex": 5,
            "toolCall": {"id": "call_1", "name": "search", "arguments": {"q": "rust"}}
        })),
        sse(json!({"type": "done", "reason": "stop", "usage": usage_json()})),
    ]
    .concat();

    let batch = parse_pi_messages_sse(body.as_bytes(), &model).expect("batch parses");
    let batch_message = done_message(&batch);

    let mut parser = PiMessagesParser::new(&model);
    let blocks: Vec<&str> = body
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .collect();
    let (done_block, head) = blocks.split_last().expect("body has blocks");
    let mut streamed = None;
    for block in head {
        // Drop the previous chunk's snapshot BEFORE the next feed: no
        // mutation may run while a snapshot is outstanding, so the counters
        // below describe the dropped-snapshot shape.
        drop(streamed.take());
        let mut events = Vec::new();
        parser
            .feed(format!("{block}\n\n").as_bytes(), &mut events)
            .expect("feed succeeds");
        assert_eq!(events.len(), 1, "one event per block");
        streamed = last_partial(&events);
        drop(events);
    }
    let streamed = streamed.expect("a partial was streamed");
    assert_eq!(
        streamed.content, batch_message.content,
        "the incremental projection equals the batch one"
    );
    assert!(matches!(streamed.content[0], ContentBlock::Text(_)));
    assert!(matches!(streamed.content[1], ContentBlock::Thinking(_)));
    match &streamed.content[2] {
        ContentBlock::ToolCall(call) => {
            assert_eq!(call.arguments.get("q"), Some(&json!("rust")))
        }
        other => panic!("expected a tool-call block, got {other:?}"),
    }
    drop(streamed);
    // The terminal block runs with no snapshot outstanding.
    let mut events = Vec::new();
    parser
        .feed(format!("{done_block}\n\n").as_bytes(), &mut events)
        .expect("feed succeeds");
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
    assert_eq!(
        parser.projection_work(),
        5,
        "one per block start/end; thinking_end updates in place"
    );
    assert_eq!(
        parser.copy_count(),
        0,
        "no snapshot is held across a mutation"
    );
}

#[test]
fn pi_messages_unclosed_sse_block_is_bounded() {
    let model = sample_model();
    let mut parser = PiMessagesParser::new(&model);

    // A block whose payload never gets its closing blank line: the parser
    // buffers it across `feed` calls, so the shared 1 MiB line cap must apply
    // here too. (`data:` + a partial JSON payload that is never completed.)
    let prefix = b"data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"";
    let mut chunk = prefix.to_vec();
    chunk.resize(prefix.len() + 1024 * 1024, b'x');

    let mut events = Vec::new();
    let error = parser
        .feed(&chunk, &mut events)
        .expect_err("an unterminated block past the cap must fail the stream");
    match error {
        AiError::NonRetryable(message) => {
            assert!(
                message.contains("1 MiB") && message.contains("blank line"),
                "expected the bounded-block diagnostic, got {message}"
            );
        }
        other => panic!("expected a typed NonRetryable error, got {other:?}"),
    }
    assert!(
        events.is_empty(),
        "the oversized block contains no complete event"
    );

    // The oversized bytes were released, not retained: the parser remains
    // usable for the driver's error reporting path.
    let events = feed_chunk(
        &mut parser,
        json!({"type": "text_start", "contentIndex": 0}),
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::TextStart { .. })
    ));
}

/// The projection is keyed by the wire index, not by arrival order (the
/// previous BTreeMap-backed rebuild had the same ascending order): a block at
/// `contentIndex` 2 arriving before `contentIndex` 0 still lands behind it,
/// and deltas address the wire index, not the position.
#[test]
fn pi_messages_out_of_order_wire_indices_project_in_ascending_key_order() {
    let model = sample_model();
    let mut parser = PiMessagesParser::new(&model);

    drop(feed_chunk(
        &mut parser,
        json!({"type": "text_start", "contentIndex": 2}),
    ));
    drop(feed_chunk(
        &mut parser,
        json!({"type": "text_start", "contentIndex": 0}),
    ));

    let events = feed_chunk(
        &mut parser,
        json!({"type": "text_delta", "contentIndex": 2, "delta": "two"}),
    );
    let partial = last_partial(&events).expect("delta partial");
    assert_eq!(
        texts_of(&partial),
        vec!["", "two"],
        "index 2 is projected second even though it arrived first"
    );
    drop(partial);
    drop(events);

    let events = feed_chunk(
        &mut parser,
        json!({"type": "text_delta", "contentIndex": 0, "delta": "zero"}),
    );
    let partial = last_partial(&events).expect("delta partial");
    assert_eq!(texts_of(&partial), vec!["zero", "two"]);
    assert_eq!(parser.copy_count(), 0);
    assert_eq!(parser.projection_work(), 2, "two block starts");
}
