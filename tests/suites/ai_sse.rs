//! Provider SSE tests: incremental-vs-batch equivalence for the SSE parsers
//! plus an end-to-end check that provider streams surface events as they
//! arrive.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use pir::ai::api::anthropic::{parse_anthropic_sse, AnthropicSseParser};
use pir::ai::api::google::{parse_google_sse, GoogleSseParser};
use pir::ai::api::mistral::parse_mistral_sse;
use pir::ai::api::openai::{parse_openai_sse, OpenAiCompatibleProvider, OpenAiSseParser};
use pir::ai::api::openai_responses::shared::ResponsesSseParser;
use pir::ai::error::{AiError, TurnRetryClass};
use pir::ai::provider::Provider;
use pir::ai::types::*;
use serde_json::{json, Value};

/// Mirror of the adapter's private `MAX_SSE_LINE_BYTES` bound (1 MiB).
const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;

fn sample_model() -> Model {
    Model {
        id: "test".into(),
        name: "test".into(),
        api: "openai-chat".into(),
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

fn ctx(text: &str) -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        ..Context::default()
    }
}

// ---------------------------------------------------------------------------
// Incremental vs batch SSE equivalence
// ---------------------------------------------------------------------------

/// OpenAI fixture: thinking deltas, structured `reasoning_details`, multi-byte
/// text, a two-chunk tool call, `finish_reason`, the usage-only trailing chunk
/// (`choices: []`) and `[DONE]`.
const OPENAI_FIXTURE: &str = r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Think"}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"ing hard","signature":"sig1"}]}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"你好 "}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"world 🌍"}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":"}}]}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}

data: [DONE]

"#;

/// Anthropic fixture: `event:` lines (ignored), text and tool_use blocks,
/// `message_delta` with stop reason and usage, plus a `message_stop`.
const ANTHROPIC_FIXTURE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":12,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}

data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}

data: {"type":"message_stop"}

"#;

/// Strip `timestamp` fields recursively: parsers stamp wall-clock times into
/// each event's `partial`, which legitimately differ between runs.
///
/// `durationMs` is stripped too: the live stream's terminal message is timed
/// by `AssistantMessageEventStream` (upstream `event-stream.ts`), while the
/// batch parser path produces raw events with no stream to time them.
fn strip_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("timestamp");
            map.remove("durationMs");
            for (_, child) in map.iter_mut() {
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

/// Canonical comparable form of one event (timestamps stripped).
fn event_key(event: &AssistantMessageEvent) -> String {
    let mut value = serde_json::to_value(event).expect("event serializes");
    strip_timestamps(&mut value);
    serde_json::to_string(&value).expect("canonical form serializes")
}

fn assert_same_events(batch: &[AssistantMessageEvent], incremental: &[AssistantMessageEvent]) {
    assert_eq!(
        batch.len(),
        incremental.len(),
        "event count mismatch (batch vs incremental)"
    );
    for (index, (batch_event, stream_event)) in batch.iter().zip(incremental.iter()).enumerate() {
        assert_eq!(
            event_key(batch_event),
            event_key(stream_event),
            "event {index} mismatch (batch vs incremental)"
        );
    }
}

/// Deterministic LCG so the random chunk splits are reproducible.
fn random_splits(body: &[u8], seed: u64, max_step: usize) -> Vec<Vec<u8>> {
    let mut state = seed | 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < body.len() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let step = 1 + ((state >> 33) as usize) % max_step;
        let end = (start + step).min(body.len());
        chunks.push(body[start..end].to_vec());
        start = end;
    }
    chunks
}

fn feed_openai(chunks: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = OpenAiSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for chunk in chunks {
        parser.feed(chunk, &mut out).expect("feed succeeds");
    }
    parser.finish(&mut out).expect("finish succeeds");
    out
}

fn feed_anthropic(chunks: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = AnthropicSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for chunk in chunks {
        parser.feed(chunk, &mut out).expect("feed succeeds");
    }
    parser.finish(&mut out).expect("finish succeeds");
    out
}

#[test]
fn openai_incremental_matches_batch_random_splits() {
    let model = sample_model();
    let body = OPENAI_FIXTURE.as_bytes();
    let batch = parse_openai_sse(body, &model).expect("batch parse succeeds");
    assert!(batch.len() > 5, "fixture should produce several events");
    for seed in 1..=5u64 {
        let chunks = random_splits(body, seed, 7);
        let incremental = feed_openai(&chunks, &model);
        assert_same_events(&batch, &incremental);
    }
}

#[test]
fn openai_incremental_matches_batch_byte_by_byte() {
    let model = sample_model();
    let body = OPENAI_FIXTURE.as_bytes();
    let batch = parse_openai_sse(body, &model).expect("batch parse succeeds");
    // 1-byte chunks: every line, CRLF pair and multi-byte character is split
    // across chunk boundaries.
    let chunks = body.iter().map(|byte| vec![*byte]).collect::<Vec<_>>();
    let incremental = feed_openai(&chunks, &model);
    assert_same_events(&batch, &incremental);
}

#[test]
fn openai_incremental_matches_batch_crlf_line_endings() {
    let model = sample_model();
    let crlf = OPENAI_FIXTURE.replace('\n', "\r\n");
    let body = crlf.as_bytes();
    let batch = parse_openai_sse(body, &model).expect("batch parse succeeds");
    let chunks = random_splits(body, 7, 5);
    let incremental = feed_openai(&chunks, &model);
    assert_same_events(&batch, &incremental);
}

#[test]
fn openai_incremental_matches_batch_without_trailing_blank_line() {
    let model = sample_model();
    // Final event block not terminated by a blank line: the parser must
    // dispatch it on finish().
    let body = OPENAI_FIXTURE.trim_end_matches('\n').as_bytes();
    let batch = parse_openai_sse(body, &model).expect("batch parse succeeds");
    let chunks = random_splits(body, 11, 9);
    let incremental = feed_openai(&chunks, &model);
    assert_same_events(&batch, &incremental);
}

#[test]
fn openai_sse_line_buffer_is_capped_at_one_mib() {
    let model = sample_model();
    let mut events = Vec::new();

    // A partial line at exactly the cap is still accepted (the cap is a
    // strict upper bound), so legal large chunks keep parsing.
    let mut parser = OpenAiSseParser::new(&model);
    parser
        .feed(&vec![b'x'; MAX_SSE_LINE_BYTES], &mut events)
        .expect("a cap-sized partial line is accepted");

    // One further byte without a newline exceeds the retained-line bound.
    let error = parser
        .feed(b"x", &mut events)
        .expect_err("an unbounded partial line must fail the stream");
    assert!(
        matches!(error, AiError::NonRetryable(_)),
        "the cap violation is a non-retryable protocol error: {error:?}"
    );
    let text = error.to_string();
    assert!(
        text.contains("1 MiB"),
        "the error names the 1 MiB cap: {text}"
    );
    assert!(
        text.contains("without a newline"),
        "the error explains the partial-line condition: {text}"
    );

    // A complete over-cap line is rejected too, instead of being buffered.
    let mut parser = OpenAiSseParser::new(&model);
    let mut oversized = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    oversized.push(b'\n');
    let error = parser
        .feed(&oversized, &mut events)
        .expect_err("an over-cap complete line must fail the stream");
    assert!(error.to_string().contains("1 MiB"), "{error}");

    // Normal input is unaffected.
    let mut parser = OpenAiSseParser::new(&model);
    let mut events = Vec::new();
    parser
        .feed(
            b"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
            &mut events,
        )
        .expect("a normal SSE event still parses");
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::TextDelta { delta, .. } if delta == "ok"
    )));
}

/// The partial-line cap lives in the shared `sse_feed_lines` framing, so every
/// adapter inherits it. The Anthropic, Google and Responses parsers used to
/// call the framing directly without the OpenAI parser's bound: a provider
/// that never sends `\n` grew the buffer for the lifetime of the stream.
#[test]
fn every_sse_parser_caps_an_unterminated_line() {
    let model = sample_model();
    let oversized = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    let assert_capped = |error: AiError, parser: &str| {
        assert!(
            matches!(error, AiError::NonRetryable(_)),
            "{parser}: the cap violation must be non-retryable: {error:?}"
        );
        let text = error.to_string();
        assert!(
            text.contains("1 MiB") && text.contains("without a newline"),
            "{parser}: the error names the cap and the condition: {text}"
        );
    };

    let mut anthropic = AnthropicSseParser::new(&model);
    let mut events = Vec::new();
    assert_capped(
        anthropic
            .feed(&oversized, &mut events)
            .expect_err("Anthropic cap"),
        "anthropic",
    );

    let mut google = pir::ai::api::google::GoogleSseParser::new(&model);
    let mut events = Vec::new();
    assert_capped(
        google
            .feed(&oversized, &mut events)
            .expect_err("Google cap"),
        "google",
    );

    let mut responses = pir::ai::api::openai_responses::shared::ResponsesSseParser::new(&model);
    let mut events = Vec::new();
    assert_capped(
        responses
            .feed(&oversized, &mut events)
            .expect_err("Responses cap"),
        "responses",
    );

    // The oversized line is dropped, not retained: a normal event fed
    // afterwards still parses (the stream error is terminal for the request,
    // but the parser state stays bounded and usable).
    anthropic
        .feed(
            b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"ok\"}}\n\n",
            &mut events,
        )
        .expect("the parser keeps working after the cap error");
}

#[test]
fn openai_usage_only_trailing_chunk_is_applied() {
    let model = sample_model();
    let body = OPENAI_FIXTURE.as_bytes();
    let batch = parse_openai_sse(body, &model).expect("batch parse succeeds");
    let incremental = feed_openai(&[body.to_vec()], &model);
    assert_same_events(&batch, &incremental);
    for events in [&batch, &incremental] {
        let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
            panic!("stream must end with Done");
        };
        // Upstream parseChunkUsage: cached tokens are cache reads subtracted
        // from the prompt total; totalTokens = input+output+cacheRead.
        assert_eq!(message.usage.input, 7);
        assert_eq!(message.usage.output, 5);
        assert_eq!(message.usage.cache_read, 3);
        assert_eq!(message.usage.reasoning, Some(2));
        assert_eq!(message.usage.total_tokens, 15);
        assert_eq!(message.response_id.as_deref(), Some("chatcmpl-1"));
    }
}

#[test]
fn anthropic_incremental_matches_batch_random_splits() {
    let model = sample_model();
    let body = ANTHROPIC_FIXTURE.as_bytes();
    let batch = parse_anthropic_sse(body, &model).expect("batch parse succeeds");
    assert!(batch.len() > 5, "fixture should produce several events");
    for seed in 1..=5u64 {
        let chunks = random_splits(body, seed, 7);
        let incremental = feed_anthropic(&chunks, &model);
        assert_same_events(&batch, &incremental);
    }
}

#[test]
fn anthropic_incremental_matches_batch_byte_by_byte() {
    let model = sample_model();
    let body = ANTHROPIC_FIXTURE.as_bytes();
    let batch = parse_anthropic_sse(body, &model).expect("batch parse succeeds");
    let chunks = body.iter().map(|byte| vec![*byte]).collect::<Vec<_>>();
    let incremental = feed_anthropic(&chunks, &model);
    assert_same_events(&batch, &incremental);
}

#[test]
fn anthropic_incremental_finalizes_tool_call_and_stop_reason() {
    let model = sample_model();
    let body = ANTHROPIC_FIXTURE.as_bytes();
    let batch = parse_anthropic_sse(body, &model).expect("batch parse succeeds");
    let incremental = feed_anthropic(&[body.to_vec()], &model);
    assert_same_events(&batch, &incremental);
    for events in [&batch, &incremental] {
        let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
            panic!("stream must end with Done");
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage.input, 12);
        assert_eq!(message.usage.output, 7);
        assert_eq!(message.response_id.as_deref(), Some("msg_1"));
        let Some(ContentBlock::ToolCall(call)) = message
            .content
            .iter()
            .find(|block| matches!(block, ContentBlock::ToolCall(_)))
        else {
            panic!("tool call finalized");
        };
        assert_eq!(call.name, "get_weather");
        assert_eq!(
            call.arguments.get("city").and_then(Value::as_str),
            Some("Paris")
        );
    }
}

/// End-to-end incremental check: a local HTTP server drips the SSE body in
/// pieces; the first text delta must be observable on the provider stream
/// while the server is still mid-body (no full-body buffering), and the final
/// event sequence must equal the batch parse of the same body.
#[test]
fn openai_provider_streams_events_incrementally() {
    let body_parts = [
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello \"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"incremental\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"id\":\"chatcmpl-9\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\ndata: [DONE]\n\n",
    ];
    let full_body = body_parts.concat();
    let finished = Arc::new(AtomicBool::new(false));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, server_handle) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server_finished = finished.clone();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("client connects");
            // TCP_NODELAY keeps the server's small writes from waiting on the
            // client's delayed ACK (which would serialize the body to the end).
            socket.set_nodelay(true).expect("set nodelay");
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch).await; // request head/body (ignored)
            // Chunked transfer encoding: hyper surfaces content-length bodies
            // as one merged chunk on this stack, while chunked frames are
            // delivered as they arrive - which is what this test observes.
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
            socket.write_all(head.as_bytes()).await.unwrap();
            for part in &body_parts {
                let frame = format!("{:x}\r\n{}\r\n", part.len(), part);
                socket.write_all(frame.as_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
            server_finished.store(true, Ordering::SeqCst);
        });
        (addr, handle)
    });
    let url = format!("http://{addr}");
    let provider = OpenAiCompatibleProvider::new("test", url.clone(), vec![]).unwrap();
    let mut provider = provider;
    let mut model = sample_model();
    model.base_url = url;
    provider.models.push(model.clone());
    let mut stream =
        rt.block_on(async { provider.stream(&model, ctx("hi"), StreamOptions::default()) });
    let events = rt.block_on(async {
        let mut events = Vec::new();
        let mut saw_delta_before_body_complete = false;
        while let Some(event) = stream.next().await {
            if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
                // The server sleeps between body parts; observing a delta
                // proves chunks are parsed as they arrive.
                if !finished.load(Ordering::SeqCst) {
                    saw_delta_before_body_complete = true;
                }
            }
            events.push(event);
        }
        assert!(
            saw_delta_before_body_complete,
            "first text delta must arrive while the body is still streaming"
        );
        events
    });
    rt.block_on(async { server_handle.await.unwrap() });
    let batch = parse_openai_sse(full_body.as_bytes(), &model).expect("batch parse succeeds");
    assert_same_events(&batch, &events);
}

// ---------------------------------------------------------------------------
// Partial-snapshot sharing (perf regression, 2026-10-15)
// ---------------------------------------------------------------------------
//
// Every non-terminal stream event carries a full cumulative `partial`
// snapshot. The parsers keep the accumulated message behind an `Arc` and
// mutate it copy-on-write, so:
//   1. a consumer that drops the snapshot pays nothing for it (the next delta
//      mutates the one accumulator allocation in place — no deep clone of the
//      whole message per delta), and
//   2. a consumer that KEEPS a snapshot still observes exactly the message
//      accumulated up to that delta (the mutation copies-on-write).
// The previous implementation deep-cloned the whole accumulated message into
// a fresh `Arc` per delta (O(message) per delta, O(message²) per reply).

/// One OpenAI text-delta SSE chunk carrying `token`.
fn openai_text_chunk(token: &str) -> String {
    format!(
        "data: {{\"id\":\"chatcmpl-1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(token).expect("token serializes")
    )
}

fn partial_text(partial: &AssistantMessage) -> &str {
    partial
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn text_delta_partial(events: &[AssistantMessageEvent]) -> Option<Arc<AssistantMessage>> {
    events.iter().find_map(|event| match event {
        AssistantMessageEvent::TextDelta { partial, .. } => Some(Arc::clone(partial)),
        _ => None,
    })
}

/// An unread snapshot must not force a copy: with the consumer reading and
/// dropping each delta's snapshot, the parser pays exactly ONE accumulator
/// copy for a whole text block (the `TextStart` snapshot forces it) and none
/// per delta.
///
/// Note why this uses the parser's own copy counter instead of a `Weak`-based
/// probe: `Arc::make_mut` copies while ANY `Weak` to the allocation exists, so
/// a probe that watches the snapshots through `Weak` would itself cause one
/// copy per delta — it would measure its own interference. The counter is
/// observably better: it is incremented only when a copy really happens.
#[test]
fn openai_unread_partial_snapshots_pay_one_copy_per_block_not_per_delta() {
    let model = sample_model();
    let mut parser = OpenAiSseParser::new(&model);

    let mut expected = String::new();
    for index in 0..20 {
        let token = format!("t{index} ");
        let mut events = Vec::new();
        parser
            .feed(openai_text_chunk(&token).as_bytes(), &mut events)
            .expect("feed succeeds");
        let snapshot = text_delta_partial(&events).expect("text delta");
        expected.push_str(&token);
        assert_eq!(partial_text(&snapshot), expected, "delta {index} snapshot");
        drop(snapshot);
        drop(events);
    }
    assert_eq!(
        parser.copy_count(),
        1,
        "one copy for the first block's TextStart snapshot, none per delta"
    );

    // A consumer that KEEPS a snapshot forces copy-on-write: the next delta may
    // not mutate the held value.
    let mut events = Vec::new();
    parser
        .feed(openai_text_chunk("held ").as_bytes(), &mut events)
        .expect("feed succeeds");
    let held = text_delta_partial(&events).expect("text delta");
    assert_eq!(partial_text(&held), format!("{expected}held "));
    let copies_before = parser.copy_count();

    let mut events = Vec::new();
    parser
        .feed(openai_text_chunk("more").as_bytes(), &mut events)
        .expect("feed succeeds");
    let next = text_delta_partial(&events).expect("text delta");
    assert_eq!(partial_text(&next), format!("{expected}held more"));
    assert_eq!(
        partial_text(&held),
        format!("{expected}held "),
        "the held snapshot stays frozen"
    );
    assert_eq!(
        parser.copy_count(),
        copies_before + 1,
        "holding a snapshot costs exactly one copy on the next mutation"
    );
}

/// A reader that holds every snapshot keeps seeing exactly the message
/// accumulated up to each delta — later deltas copy-on-write instead of
/// mutating that snapshot.
#[test]
fn openai_held_partial_snapshots_stay_complete_and_frozen() {
    let model = sample_model();
    let mut parser = OpenAiSseParser::new(&model);
    let mut all = Vec::new();
    for token in ["a", "b", "c", "d"] {
        parser
            .feed(openai_text_chunk(token).as_bytes(), &mut all)
            .expect("feed succeeds");
    }
    // A terminal chunk so `finish` sees the finish reason it requires.
    parser
        .feed(
            b"data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            &mut all,
        )
        .expect("feed finish reason");
    parser.finish(&mut all).expect("finish succeeds");

    let deltas: Vec<Arc<AssistantMessage>> = all
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { partial, .. } => Some(Arc::clone(partial)),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 4, "four text deltas");
    let texts: Vec<&str> = deltas.iter().map(|partial| partial_text(partial)).collect();
    assert_eq!(
        texts,
        vec!["a", "ab", "abc", "abcd"],
        "each delta's snapshot stays the message accumulated up to that delta, read after later deltas"
    );
    assert_eq!(
        partial_text(&deltas[0]),
        "a",
        "the first held snapshot must stay frozen after later deltas"
    );
}

/// The same held-snapshot invariant for the Anthropic parser (a second
/// adapter on the same copy-on-write mechanism).
#[test]
fn anthropic_held_partial_snapshots_stay_complete_and_frozen() {
    let mut model = sample_model();
    model.api = "anthropic-messages".into();
    let mut parser = AnthropicSseParser::new(&model);
    let mut all = Vec::new();
    let body = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","model":"test","usage":{"input_tokens":3,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"y"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}

event: message_stop
data: {"type":"message_stop"}

"#;
    parser.feed(body.as_bytes(), &mut all).expect("feed");
    parser.finish(&mut all).expect("finish");

    let deltas: Vec<Arc<AssistantMessage>> = all
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { partial, .. } => Some(Arc::clone(partial)),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 2, "two text deltas: {all:?}");
    assert_eq!(partial_text(&deltas[0]), "x");
    assert_eq!(partial_text(&deltas[1]), "xy");
}

// ---------------------------------------------------------------------------
// OpenAI streaming tool arguments: per-delta incremental mirror
// ---------------------------------------------------------------------------

/// One OpenAI tool-call SSE event carrying `arguments` for tool index 0.
fn openai_tool_arguments_chunk(arguments: &str) -> Vec<u8> {
    let chunk = json!({
        "id": "chatcmpl-tool",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_stream",
                    "type": "function",
                    "function": {"name": "write", "arguments": arguments},
                }]
            }
        }]
    });
    format!("data: {chunk}\n\n").into_bytes()
}

fn tool_arguments(partial: &AssistantMessage) -> BTreeMap<String, Value> {
    partial
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.arguments.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Feed one argument fragment and return the partial arguments mirrored by
/// the resulting `toolcall_delta` event.
fn feed_tool_arguments(parser: &mut OpenAiSseParser, arguments: &str) -> BTreeMap<String, Value> {
    let mut events = Vec::new();
    parser
        .feed(&openai_tool_arguments_chunk(arguments), &mut events)
        .expect("tool-call feed succeeds");
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallDelta { partial, .. } => Some(tool_arguments(partial)),
            _ => None,
        })
        .unwrap_or_default()
}

/// Feed the terminal `finish_reason` chunk and finish the stream; returns the
/// finalized assistant message.
fn finish_tool_stream(parser: &mut OpenAiSseParser) -> AssistantMessage {
    let mut events = Vec::new();
    parser
        .feed(
            b"data: {\"id\":\"chatcmpl-tool\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            &mut events,
        )
        .expect("finish_reason feed succeeds");
    parser.finish(&mut events).expect("finish succeeds");
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("done event")
}

fn expected_arguments(value: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(value).expect("object of arguments")
}

#[test]
fn openai_tool_arguments_mirror_tracks_every_delta() {
    let model = sample_model();
    let mut parser = OpenAiSseParser::new(&model);
    let steps: &[(&str, Value)] = &[
        (r#"{"city""#, json!({})),
        (":", json!({})),
        (r#"""#, json!({"city": ""})),
        ("Par", json!({"city": "Par"})),
        (r#"is""#, json!({"city": "Paris"})),
        (r#","days":"#, json!({"city": "Paris"})),
        ("3", json!({"city": "Paris", "days": 3})),
        (
            r#","nested":{"#,
            json!({"city": "Paris", "days": 3, "nested": {}}),
        ),
        (
            r#""ok":tr"#,
            json!({"city": "Paris", "days": 3, "nested": {"ok": true}}),
        ),
        (
            "u",
            json!({"city": "Paris", "days": 3, "nested": {"ok": true}}),
        ),
        (
            r#"e,"items":[1"#,
            json!({"city": "Paris", "days": 3, "nested": {"ok": true, "items": [1]}}),
        ),
        (
            "0,2",
            json!({"city": "Paris", "days": 3, "nested": {"ok": true, "items": [10, 2]}}),
        ),
        (
            "0]}",
            json!({"city": "Paris", "days": 3, "nested": {"ok": true, "items": [10, 20]}}),
        ),
        (
            r#","note":"do"#,
            json!({
                "city": "Paris",
                "days": 3,
                "nested": {"ok": true, "items": [10, 20]},
                "note": "do"
            }),
        ),
        (
            r#"ne"}"#,
            json!({
                "city": "Paris",
                "days": 3,
                "nested": {"ok": true, "items": [10, 20]},
                "note": "done"
            }),
        ),
    ];
    for (index, (delta, expected)) in steps.iter().enumerate() {
        let actual = feed_tool_arguments(&mut parser, delta);
        assert_eq!(
            actual,
            expected_arguments(expected.clone()),
            "delta {index} ({delta:?}) must be visible in the partial immediately"
        );
    }

    let message = finish_tool_stream(&mut parser);
    let ContentBlock::ToolCall(call) = &message.content[0] else {
        panic!("expected a tool block: {:?}", message.content);
    };
    let complete =
        r#"{"city":"Paris","days":3,"nested":{"ok":true,"items":[10,20]},"note":"done"}"#;
    let expected: BTreeMap<String, Value> =
        serde_json::from_str(complete).expect("complete arguments parse");
    assert_eq!(
        call.arguments, expected,
        "the finalized arguments must match the full strict parse"
    );
    assert!(call.arguments_error.is_none(), "{:?}", call.arguments_error);
}

#[test]
fn openai_tool_arguments_mirror_handles_escapes_and_unicode_across_deltas() {
    let model = sample_model();
    let mut parser = OpenAiSseParser::new(&model);
    // The JSON text below is split mid-escape (`\u00` / `e9`), mid-surrogate
    // pair (`\ud83d` / `\ude00`) and includes literal UTF-8 (`世界`).
    let steps: &[(&str, &str)] = &[
        (r#"{"text":"quote:\""#, "quote:\""),
        (r#" slash:\\"#, "quote:\" slash:\\"),
        (
            r#" newline:\n tab:\t unicode:\u00"#,
            "quote:\" slash:\\ newline:\n tab:\t unicode:",
        ),
        (
            r#"e9 emoji:\ud83d"#,
            "quote:\" slash:\\ newline:\n tab:\t unicode:é emoji:",
        ),
        (
            r#"\ude00 raw:"#,
            "quote:\" slash:\\ newline:\n tab:\t unicode:é emoji:😀 raw:",
        ),
        (
            r#"世界"}"#,
            "quote:\" slash:\\ newline:\n tab:\t unicode:é emoji:😀 raw:世界",
        ),
    ];
    for (index, (delta, expected)) in steps.iter().enumerate() {
        let actual = feed_tool_arguments(&mut parser, delta);
        assert_eq!(
            actual,
            expected_arguments(json!({"text": expected})),
            "escape delta {index} ({delta:?}) partial"
        );
    }

    let message = finish_tool_stream(&mut parser);
    let ContentBlock::ToolCall(call) = &message.content[0] else {
        panic!("expected a tool block: {:?}", message.content);
    };
    assert_eq!(
        call.arguments.get("text").and_then(Value::as_str),
        Some(steps[steps.len() - 1].1)
    );
    assert!(call.arguments_error.is_none(), "{:?}", call.arguments_error);
}

#[test]
fn openai_tool_arguments_mirror_keeps_unclosed_json_states_stable() {
    let model = sample_model();
    let mut parser = OpenAiSseParser::new(&model);
    let steps: &[(&str, Value)] = &[
        (
            r#"{"path":"src/main.rs","content":"fn main() {"#,
            json!({"path": "src/main.rs", "content": "fn main() {"}),
        ),
        (
            "}",
            json!({"path": "src/main.rs", "content": "fn main() {}"}),
        ),
        (
            r#"""#,
            json!({"path": "src/main.rs", "content": "fn main() {}"}),
        ),
        // A trailing backslash holds the unfinished escape back; the visible
        // string stays at the characters read so far.
        (
            r#","truncated":"a\"#,
            json!({"path": "src/main.rs", "content": "fn main() {}", "truncated": "a"}),
        ),
        // The escape completes as a newline.
        (
            r#"n""#,
            json!({"path": "src/main.rs", "content": "fn main() {}", "truncated": "a\n"}),
        ),
        (
            r#","num":12"#,
            json!({
                "path": "src/main.rs",
                "content": "fn main() {}",
                "truncated": "a\n",
                "num": 12
            }),
        ),
        // The unterminated number keeps growing in the mirror.
        (
            "3",
            json!({
                "path": "src/main.rs",
                "content": "fn main() {}",
                "truncated": "a\n",
                "num": 123
            }),
        ),
    ];
    for (index, (delta, expected)) in steps.iter().enumerate() {
        let actual = feed_tool_arguments(&mut parser, delta);
        assert_eq!(
            actual,
            expected_arguments(expected.clone()),
            "unclosed delta {index} ({delta:?}) partial"
        );
    }

    // The stream ends with the root object still open: the partial mirror had
    // values, but the finalized call reports the strict parse failure instead
    // of keeping a partial prefix.
    let message = finish_tool_stream(&mut parser);
    let ContentBlock::ToolCall(call) = &message.content[0] else {
        panic!("expected a tool block: {:?}", message.content);
    };
    assert!(call.arguments.is_empty(), "{:?}", call.arguments);
    let error = call.arguments_error.as_deref().unwrap_or_default();
    assert!(
        !error.is_empty(),
        "truncated arguments must report an error"
    );
    assert!(
        error.contains("Received arguments:"),
        "the error carries the raw text: {error}"
    );
}

#[test]
fn openai_tool_arguments_mirror_does_not_lag_beyond_eight_kibibytes() {
    let model = sample_model();
    let mut parser = OpenAiSseParser::new(&model);
    assert_eq!(
        feed_tool_arguments(&mut parser, r#"{"text":""#),
        expected_arguments(json!({"text": ""}))
    );
    let fragment = "0123456789abcdefghijklmnopqrstuvwxyz".repeat(9); // 324 bytes
    let mut accumulated = String::new();
    for index in 0..40 {
        accumulated.push_str(&fragment);
        let actual = feed_tool_arguments(&mut parser, &fragment);
        assert_eq!(
            actual,
            expected_arguments(json!({"text": accumulated})),
            "delta {index} ({} bytes accumulated) must not lag",
            accumulated.len()
        );
    }
    assert!(accumulated.len() > 8 * 1024);
    let actual = feed_tool_arguments(&mut parser, r#""}"#);
    assert_eq!(actual, expected_arguments(json!({"text": accumulated})));
    let message = finish_tool_stream(&mut parser);
    let ContentBlock::ToolCall(call) = &message.content[0] else {
        panic!("expected a tool block: {:?}", message.content);
    };
    assert_eq!(
        call.arguments.get("text").and_then(Value::as_str),
        Some(accumulated.as_str())
    );
}

#[test]
fn openai_tool_arguments_final_matches_strict_parse_for_arbitrary_splits() {
    let model = sample_model();
    let raw = r#"{"alpha":"a\n\"b\\c","beta":-12.5e2,"gamma":[true,false,null,{"deep":[1,2,3]}],"delta":{"emoji":"\ud83d\ude00","text":"世界"}}"#;
    let expected: BTreeMap<String, Value> =
        serde_json::from_str(raw).expect("reference JSON parses");
    for split_size in [1usize, 2, 3, 5, 7, 11, 64] {
        let mut parser = OpenAiSseParser::new(&model);
        let mut index = 0;
        while index < raw.len() {
            let mut end = (index + split_size).min(raw.len());
            while end < raw.len() && !raw.is_char_boundary(end) {
                end += 1;
            }
            let _ = feed_tool_arguments(&mut parser, &raw[index..end]);
            index = end;
        }
        let message = finish_tool_stream(&mut parser);
        let ContentBlock::ToolCall(call) = &message.content[0] else {
            panic!("expected a tool block: {:?}", message.content);
        };
        assert_eq!(
            call.arguments, expected,
            "split size {split_size}: finalized arguments must match the strict parse"
        );
        assert!(
            call.arguments_error.is_none(),
            "split size {split_size}: {:?}",
            call.arguments_error
        );
    }
}

/// Feeding the SSE body one byte at a time (which splits multi-byte UTF-8 and
/// escape sequences at the transport layer) must reach the same final
/// arguments as the whole-body parse.
#[test]
fn openai_tool_arguments_byte_by_byte_sse_feed_matches_batch() {
    let model = sample_model();
    let raw = r#"{"text":"caf\u00e9 \ud83d\ude00 世界","nested":{"n":1}}"#;
    let chunk = json!({
        "id": "chatcmpl-tool",
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_stream",
                "function": {"name": "write", "arguments": raw},
            }]}
        }]
    });
    let body = format!(
        "data: {chunk}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    );
    let events = parse_openai_sse(body.as_bytes(), &model).expect("batch parse");
    let mut byte_events = Vec::new();
    let mut parser = OpenAiSseParser::new(&model);
    for byte in body.as_bytes() {
        parser
            .feed(std::slice::from_ref(byte), &mut byte_events)
            .expect("byte feed");
    }
    parser.finish(&mut byte_events).expect("byte finish");
    let final_arguments = |events: &[AssistantMessageEvent]| {
        let message = events
            .iter()
            .rev()
            .find_map(|event| match event {
                AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("done event");
        match &message.content[0] {
            ContentBlock::ToolCall(call) => call.arguments.clone(),
            other => panic!("expected a tool block: {other:?}"),
        }
    };
    let expected: BTreeMap<String, Value> =
        serde_json::from_str(raw).expect("reference JSON parses");
    assert_eq!(final_arguments(&events), expected);
    assert_eq!(final_arguments(&byte_events), expected);
}

// ---------------------------------------------------------------------------
// In-band provider error frames (P1): never silently dropped; transient
// classes are retryable, auth/validation failures are not.
// ---------------------------------------------------------------------------

fn assert_retryable(error: &AiError, context: &str) {
    assert_eq!(
        error.turn_retry_class(),
        TurnRetryClass::RetryableTurn,
        "{context} must be retryable: {error:?}"
    );
}

fn assert_non_retryable(error: &AiError, context: &str) {
    assert_eq!(
        error.turn_retry_class(),
        TurnRetryClass::NonRetryable,
        "{context} must be non-retryable: {error:?}"
    );
}

#[test]
fn openai_sse_in_band_error_frames_are_classified() {
    let model = sample_model();

    let rate_limit = br#"data: {"error":{"message":"Rate limit reached","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#;
    let error = parse_openai_sse(rate_limit, &model).expect_err("error frame must fail the stream");
    assert!(
        error.to_string().contains("Rate limit reached"),
        "the provider message survives: {error}"
    );
    assert_retryable(&error, "OpenAI rate_limit_error");

    let numeric_429 = br#"data: {"error":{"code":429,"message":"Too many requests"}}"#;
    let error = parse_openai_sse(numeric_429, &model).expect_err("numeric 429 frame");
    assert_retryable(&error, "OpenAI numeric 429");

    let auth = br#"data: {"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
    let error = parse_openai_sse(auth, &model).expect_err("auth error frame");
    assert_non_retryable(&error, "OpenAI invalid_api_key");
}

#[test]
fn anthropic_sse_error_events_are_classified() {
    let model = sample_model();

    let overloaded = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let error = parse_anthropic_sse(overloaded, &model).expect_err("overloaded event");
    assert_retryable(&error, "Anthropic overloaded_error");

    // Some relays drop the `event: error` name; the payload's own type is
    // enough to fail the stream instead of being ignored.
    let no_event_name =
        br#"data: {"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}}"#;
    let error = parse_anthropic_sse(no_event_name, &model).expect_err("typed error payload");
    assert_retryable(&error, "Anthropic typed error payload");

    let invalid = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad request\"}}\n\n";
    let error = parse_anthropic_sse(invalid, &model).expect_err("invalid request event");
    assert_non_retryable(&error, "Anthropic invalid_request_error");
}

#[test]
fn google_sse_error_frames_are_classified() {
    let model = sample_model();

    let exhausted = br#"data: {"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED"}}"#;
    let error = parse_google_sse(exhausted, &model).expect_err("429 frame");
    assert_retryable(&error, "Google RESOURCE_EXHAUSTED");
    assert!(
        error
            .user_message()
            .contains("Google API error 429 (RESOURCE_EXHAUSTED)"),
        "{error}"
    );

    let invalid =
        br#"data: {"error":{"code":400,"message":"Invalid argument","status":"INVALID_ARGUMENT"}}"#;
    let error = parse_google_sse(invalid, &model).expect_err("400 frame");
    assert_non_retryable(&error, "Google INVALID_ARGUMENT");
}

#[test]
fn responses_sse_error_events_are_classified() {
    let model = sample_model();

    let server_error =
        br#"data: {"type":"error","code":"server_error","message":"The server had an error"}"#;
    let error = parse_responses_sse_for_test(server_error, &model);
    assert_retryable(&error, "Responses server_error");

    let rate_limited = br#"data: {"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"Rate limit reached"}}}"#;
    let error = parse_responses_sse_for_test(rate_limited, &model);
    assert_retryable(&error, "Responses response.failed rate_limit_exceeded");

    let invalid = br#"data: {"type":"error","code":"invalid_request_error","message":"bad"}"#;
    let error = parse_responses_sse_for_test(invalid, &model);
    assert_non_retryable(&error, "Responses invalid_request_error");
}

/// Drive the public Responses parser like its streaming entry point.
fn parse_responses_sse_for_test(body: &[u8], model: &Model) -> AiError {
    let mut parser = ResponsesSseParser::new(model);
    let mut events = Vec::new();
    // Terminate the data line but not the event block: finish() flushes it and
    // surfaces the provider error.
    let mut framed = body.to_vec();
    framed.push(b'\n');
    parser.feed(&framed, &mut events).expect("line is buffered");
    parser
        .finish(&mut events)
        .expect_err("error frame must fail")
}

#[test]
fn mistral_sse_in_band_error_frames_are_classified() {
    let model = sample_model();

    let rate_limited =
        br#"data: {"error":{"message":"Rate limit exceeded","type":"rate_limit_error"}}"#;
    let error = parse_mistral_sse(rate_limited, &model).expect_err("rate limit frame");
    assert!(
        error.to_string().contains("Rate limit exceeded"),
        "the provider message survives: {error}"
    );
    assert_retryable(&error, "Mistral rate_limit_error");

    let invalid = br#"data: {"error":{"message":"Invalid API key","type":"authentication_error"}}"#;
    let error = parse_mistral_sse(invalid, &model).expect_err("auth frame");
    assert_non_retryable(&error, "Mistral authentication_error");
}

// ---------------------------------------------------------------------------
// Trailing SSE JSON parse failure on a clean EOF is truncation, not a
// non-retryable malformed-message verdict.
// ---------------------------------------------------------------------------

#[test]
fn trailing_sse_json_parse_failure_maps_to_stream_truncated() {
    let model = sample_model();

    let mut parser = OpenAiSseParser::new(&model);
    let mut events = Vec::new();
    parser
        .feed(b"data: {\"choices\":\n", &mut events)
        .expect("incomplete line is buffered");
    let error = parser.finish(&mut events).expect_err("truncated JSON");
    assert!(
        matches!(error, AiError::StreamTruncated(_)),
        "OpenAI trailing JSON must be StreamTruncated: {error:?}"
    );
    assert_retryable(&error, "OpenAI trailing JSON");

    let mut parser = AnthropicSseParser::new(&model);
    let mut events = Vec::new();
    parser
        .feed(b"data: {\"type\":\"content_block_delta\",\n", &mut events)
        .expect("incomplete line is buffered");
    let error = parser.finish(&mut events).expect_err("truncated JSON");
    assert!(
        matches!(error, AiError::StreamTruncated(_)),
        "Anthropic trailing JSON must be StreamTruncated: {error:?}"
    );

    let mut parser = GoogleSseParser::new(&model);
    let mut events = Vec::new();
    parser
        .feed(b"data: {\"candidates\":\n", &mut events)
        .expect("incomplete line is buffered");
    let error = parser.finish(&mut events).expect_err("truncated JSON");
    assert!(
        matches!(error, AiError::StreamTruncated(_)),
        "Google trailing JSON must be StreamTruncated: {error:?}"
    );

    let mut parser = ResponsesSseParser::new(&model);
    let mut events = Vec::new();
    parser
        .feed(b"data: {\"type\":\"response.created\",\n", &mut events)
        .expect("incomplete line is buffered");
    let error = parser.finish(&mut events).expect_err("truncated JSON");
    assert!(
        matches!(error, AiError::StreamTruncated(_)),
        "Responses trailing JSON must be StreamTruncated: {error:?}"
    );
}

// ---------------------------------------------------------------------------
// The complete-line cap lives in the shared framing, before UTF-8 decode.
// ---------------------------------------------------------------------------

#[test]
fn shared_sse_framing_caps_complete_lines_for_every_parser() {
    let model = sample_model();
    let mut oversized = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    oversized.push(b'\n');

    let mut anthropic = AnthropicSseParser::new(&model);
    let mut events = Vec::new();
    let error = anthropic
        .feed(&oversized, &mut events)
        .expect_err("Anthropic complete over-cap line");
    assert!(matches!(error, AiError::NonRetryable(_)), "{error:?}");
    assert!(error.to_string().contains("SSE line exceeds"), "{error}");

    let mut google = GoogleSseParser::new(&model);
    let mut events = Vec::new();
    let error = google
        .feed(&oversized, &mut events)
        .expect_err("Google complete over-cap line");
    assert!(error.to_string().contains("SSE line exceeds"), "{error}");

    let mut responses = ResponsesSseParser::new(&model);
    let mut events = Vec::new();
    let error = responses
        .feed(&oversized, &mut events)
        .expect_err("Responses complete over-cap line");
    assert!(error.to_string().contains("SSE line exceeds"), "{error}");

    // Exactly at the cap is still accepted (the cap is a strict bound).
    let mut at_cap = vec![b'x'; MAX_SSE_LINE_BYTES];
    at_cap.push(b'\n');
    let mut anthropic = AnthropicSseParser::new(&model);
    let mut events = Vec::new();
    anthropic
        .feed(&at_cap, &mut events)
        .expect("a cap-sized complete line is accepted");
}
