//! Property tests (proptest): SSE chunk-boundary robustness for the provider
//! stream parsers. Any legal SSE stream — multi-event, a comment line, a
//! multi-line `data:` payload, mixed CRLF/LF endings — must produce exactly
//! the same event sequence when the raw body is cut at arbitrary byte
//! positions into 2-3 segments and fed incrementally as when the whole body
//! is parsed in one batch. Covers the OpenAI, Anthropic, and Google parsers.

use proptest::prelude::*;

use pir::ai::api::anthropic::{parse_anthropic_sse, AnthropicSseParser};
use pir::ai::api::google::{parse_google_sse, GoogleSseParser};
use pir::ai::api::openai::{parse_openai_sse, OpenAiSseParser};
use pir::ai::types::*;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixtures: legal SSE streams with every awkward framing feature mixed in
// ---------------------------------------------------------------------------

/// OpenAI fixture: a comment-only block, a two-line `data:` payload (joined
/// with `\n` into one valid JSON chunk), tool-call argument streaming split
/// across events, `finish_reason`, the usage-only trailing chunk, and
/// `[DONE]`. Line endings deliberately alternate between CRLF and LF.
const OPENAI_FIXTURE: &str = concat!(
    ": comment-only block, dropped by both parse paths\r\n",
    "\r\n",
    "data: {\"id\":\"chatcmpl-42\",\"choices\":[{\"index\":0,\n",
    "data:  \"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\r\n",
    "\r\n",
    "data: {\"id\":\"chatcmpl-42\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-42\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
    "data: {\"id\":\"chatcmpl-42\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Paris\\\"}\"}}]}}]}\r\n\r\n",
    "data: {\"id\":\"chatcmpl-42\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-42\",\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\r\n\r\n",
    "data: [DONE]\n\n",
);

/// Anthropic fixture: `event:` lines (ignored), a comment block, a two-line
/// `data:` payload, text and tool_use blocks, `message_delta` with stop
/// reason and usage, and a `message_stop`. Mixed CRLF/LF endings.
const ANTHROPIC_FIXTURE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_42\",\"usage\":{\"input_tokens\":9,\"output_tokens\":0}}}\r\n",
    "\r\n",
    ": keep-alive comment block\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\n",
    "data:  \"text\":\"Hello\"}}\r\n\r\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\r\n\r\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Paris\\\"}\"}}\r\n\r\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
    "data: {\"type\":\"message_stop\"}\r\n\r\n",
);

/// Google fixture: a comment block, a two-line `data:` payload, a text part,
/// a finish reason, and usage metadata. Mixed CRLF/LF endings.
const GOOGLE_FIXTURE: &str = concat!(
    ": google keep-alive comment\r\n",
    "\r\n",
    "data: {\"responseId\":\"resp-7\",\"candidates\":[{\"content\":{\"role\":\"model\",\n",
    "data:  \"parts\":[{\"text\":\"Hello\"}]}}]}\n\n",
    "data: {\"responseId\":\"resp-7\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" world\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"totalTokenCount\":5}}\r\n\r\n",
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sample_model() -> Model {
    Model {
        id: "test".into(),
        name: "test".into(),
        api: "openai-chat".into(),
        provider: "test".into(),
        base_url: "http://localhost:1".into(),
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

/// Strip `timestamp` fields recursively: parsers stamp wall-clock times into
/// each event's `partial`, which legitimately differ between runs.
fn strip_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("timestamp");
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

/// Cut the body at the given interior byte positions into 2+ segments.
fn split_at_cuts(body: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut cuts: Vec<usize> = cuts
        .iter()
        .copied()
        .filter(|cut| *cut > 0 && *cut < body.len())
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut segments = Vec::new();
    let mut start = 0;
    for cut in cuts {
        segments.push(body[start..cut].to_vec());
        start = cut;
    }
    segments.push(body[start..].to_vec());
    segments
}

fn feed_openai(segments: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = OpenAiSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for segment in segments {
        parser.feed(segment, &mut out).expect("feed succeeds");
    }
    parser.finish(&mut out).expect("finish succeeds");
    out
}

fn feed_anthropic(segments: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = AnthropicSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for segment in segments {
        parser.feed(segment, &mut out).expect("feed succeeds");
    }
    parser.finish(&mut out).expect("finish succeeds");
    out
}

fn feed_google(segments: &[Vec<u8>], model: &Model) -> Vec<AssistantMessageEvent> {
    let mut parser = GoogleSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for segment in segments {
        parser.feed(segment, &mut out).expect("feed succeeds");
    }
    parser.finish(&mut out).expect("finish succeeds");
    out
}

fn assert_events_match(
    batch: &[AssistantMessageEvent],
    chunked: &[AssistantMessageEvent],
) -> Result<(), TestCaseError> {
    let batch_keys: Vec<String> = batch.iter().map(event_key).collect();
    let chunked_keys: Vec<String> = chunked.iter().map(event_key).collect();
    prop_assert_eq!(
        batch_keys,
        chunked_keys,
        "chunked parse diverged from batch"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// OpenAI: arbitrary 2-3 segment splits (including mid-line, mid-CRLF,
    /// and mid multi-byte boundaries) parse exactly like the batch path.
    #[test]
    fn openai_chunked_parse_matches_batch(
        cuts in prop::collection::vec(0..OPENAI_FIXTURE.len(), 1..=2)
    ) {
        let model = sample_model();
        let body = OPENAI_FIXTURE.as_bytes();
        let batch = parse_openai_sse(body, &model).expect("batch parse succeeds");
        prop_assert!(batch.len() > 5, "fixture should produce several events");
        let segments = split_at_cuts(body, &cuts);
        assert_events_match(&batch, &feed_openai(&segments, &model))?;
    }

    /// Anthropic: same property, including `event:` fields, comments, and
    /// multi-line `data:` payloads split across chunk boundaries.
    #[test]
    fn anthropic_chunked_parse_matches_batch(
        cuts in prop::collection::vec(0..ANTHROPIC_FIXTURE.len(), 1..=2)
    ) {
        let model = sample_model();
        let body = ANTHROPIC_FIXTURE.as_bytes();
        let batch = parse_anthropic_sse(body, &model).expect("batch parse succeeds");
        prop_assert!(batch.len() > 5, "fixture should produce several events");
        let segments = split_at_cuts(body, &cuts);
        assert_events_match(&batch, &feed_anthropic(&segments, &model))?;
    }

    /// Google: same property for the Gemini stream parser.
    #[test]
    fn google_chunked_parse_matches_batch(
        cuts in prop::collection::vec(0..GOOGLE_FIXTURE.len(), 1..=2)
    ) {
        let model = sample_model();
        let body = GOOGLE_FIXTURE.as_bytes();
        let batch = parse_google_sse(body, &model).expect("batch parse succeeds");
        prop_assert!(batch.len() > 3, "fixture should produce several events");
        let segments = split_at_cuts(body, &cuts);
        assert_events_match(&batch, &feed_google(&segments, &model))?;
    }
}

// ---------------------------------------------------------------------------
// Deterministic worst cases for the same fixtures
// ---------------------------------------------------------------------------

/// Byte-by-byte feeding splits every line ending and multi-byte sequence at a
/// chunk boundary — the hardest possible segmentation.
#[test]
fn byte_by_byte_matches_batch_on_mixed_endings() {
    type BatchParse =
        fn(&[u8], &Model) -> Result<Vec<AssistantMessageEvent>, pir::ai::error::AiError>;
    type FeedChunks = fn(&[Vec<u8>], &Model) -> Vec<AssistantMessageEvent>;
    let model = sample_model();
    let cases: [(&[u8], BatchParse, FeedChunks); 3] = [
        (OPENAI_FIXTURE.as_bytes(), parse_openai_sse, feed_openai),
        (
            ANTHROPIC_FIXTURE.as_bytes(),
            parse_anthropic_sse,
            feed_anthropic,
        ),
        (GOOGLE_FIXTURE.as_bytes(), parse_google_sse, feed_google),
    ];
    for (body, batch_parse, feed) in cases {
        let batch = batch_parse(body, &model).expect("batch parse succeeds");
        let chunks: Vec<Vec<u8>> = body.iter().map(|byte| vec![*byte]).collect();
        let chunked = feed(&chunks, &model);
        let batch_keys: Vec<String> = batch.iter().map(event_key).collect();
        let chunked_keys: Vec<String> = chunked.iter().map(event_key).collect();
        assert_eq!(batch_keys, chunked_keys, "byte-by-byte parse diverged");
    }
}
