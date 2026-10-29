//! AI request telemetry integration (W11-A): the `pi.ai.request` span, its
//! first-chunk/block events, the request/first-chunk duration histograms and
//! the token counter, W3C trace propagation onto provider request headers, and
//! the fully disabled path.
//!
//! Upstream reference: `packages/ai/src/telemetry.ts` (`AI_TELEMETRY_SCHEMA`,
//! `observeModelRequest`) driven from `packages/ai/src/models.ts`
//! (`stream` / `streamDeferred`). These tests drive the same seam through
//! [`ProviderRegistry::stream`] / [`ProviderRegistry::stream_unchecked`] with
//! the in-memory [`TestTelemetry`] recorder, and inspect the headers the
//! scripted provider actually receives.

use std::sync::{Arc, Mutex};

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Cost, ErrorReason, Message,
    Model, ModelCost, ModelCostRates, ProviderHeaders, StopReason, StreamOptions, TerminalReason,
    TextContent, ThinkingLevel, ToolCall, Usage, UserMessage, UserRole,
};
use pir::telemetry::{
    AttributeValue, Carrier, Propagator, SpanOptions, SpanStatus, Telemetry, TestTelemetry,
    TraceContext, W3cPropagator,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PROVIDER_ID: &str = "fake";
const MODEL_ID: &str = "fake-model";
const API_ID: &str = "fake-api";

fn model() -> Model {
    Model {
        id: MODEL_ID.into(),
        name: "Fake model".into(),
        api: API_ID.into(),
        provider: PROVIDER_ID.into(),
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
        context_window: 8192,
        max_tokens: 512,
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

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 1,
        })],
        tools: None,
    }
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
        text_signature: None,
    })
}

fn tool_call(name: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments: Default::default(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }
}

fn usage() -> Usage {
    Usage {
        input: 11,
        output: 7,
        cache_read: 3,
        cache_write: 2,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 23,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.25,
            total_from_provider: None,
        },
    }
}

/// The event sequence a scripted stream replays: text block, thinking block,
/// a streamed tool call, then the terminal `Done` with usage.
fn scripted_events() -> Vec<AssistantMessageEvent> {
    let mut partial = AssistantMessage {
        api: API_ID.into(),
        provider: PROVIDER_ID.into(),
        model: MODEL_ID.into(),
        ..AssistantMessage::default()
    };
    let tool_call = tool_call("read");
    let mut events = vec![AssistantMessageEvent::Start {
        partial: Arc::new(partial.clone()),
    }];
    events.push(AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "hello".into(),
        partial: Arc::new(partial.clone()),
    });
    events.push(AssistantMessageEvent::ThinkingEnd {
        content_index: 1,
        content: "why".into(),
        partial: Arc::new(partial.clone()),
    });
    events.push(AssistantMessageEvent::ToolCallDelta {
        content_index: 2,
        delta: "{\"path\":".into(),
        partial: Arc::new(partial.clone()),
    });
    events.push(AssistantMessageEvent::ToolCallDelta {
        content_index: 2,
        delta: "\"a\"}".into(),
        partial: Arc::new(partial.clone()),
    });
    events.push(AssistantMessageEvent::ToolCallEnd {
        content_index: 2,
        tool_call: tool_call.clone(),
        partial: Arc::new(partial.clone()),
    });
    partial.content = vec![text("hello"), ContentBlock::ToolCall(tool_call.clone())];
    partial.usage = usage();
    partial.stop_reason = StopReason::ToolUse;
    events.push(AssistantMessageEvent::Done {
        reason: TerminalReason::ToolUse,
        message: partial,
    });
    events
}

/// Provider that replays a fixed script and records every request's headers.
struct ScriptedProvider {
    events: Vec<AssistantMessageEvent>,
    /// Terminate the stream with `end(None)` after the script when the script
    /// carries no terminal event (a provider that ends without a result).
    end_without_terminal: bool,
    headers: Arc<Mutex<Vec<Option<ProviderHeaders>>>>,
}

impl ScriptedProvider {
    fn new(events: Vec<AssistantMessageEvent>) -> (Self, Arc<Mutex<Vec<Option<ProviderHeaders>>>>) {
        Self::build(events, false)
    }

    /// A script with no `Done`/`Error` event: the provider ends its stream
    /// without a terminal message (upstream `observeResult(undefined)`).
    fn without_terminal(
        events: Vec<AssistantMessageEvent>,
    ) -> (Self, Arc<Mutex<Vec<Option<ProviderHeaders>>>>) {
        Self::build(events, true)
    }

    fn build(
        events: Vec<AssistantMessageEvent>,
        end_without_terminal: bool,
    ) -> (Self, Arc<Mutex<Vec<Option<ProviderHeaders>>>>) {
        let headers = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events,
                end_without_terminal,
                headers: Arc::clone(&headers),
            },
            headers,
        )
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn name(&self) -> &str {
        "Fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "fake key",
            vec!["PIR_W11_FAKE_API_KEY".to_string()],
        )]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(options.headers.clone());
        let stream = AssistantMessageEventStream::new();
        for event in &self.events {
            stream.push(event.clone());
        }
        if self.end_without_terminal {
            stream.end(None);
        }
        stream
    }
}

fn registry(provider: ScriptedProvider) -> ProviderRegistry {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));
    registry
}

/// A registry whose auth context resolves the scripted provider's key, for
/// the auth-enforcing [`ProviderRegistry::stream`] path.
fn authenticated_registry(provider: ScriptedProvider) -> ProviderRegistry {
    let env = std::collections::BTreeMap::from([(
        "PIR_W11_FAKE_API_KEY".to_string(),
        "test-key".to_string(),
    )]);
    let mut registry = ProviderRegistry::new(
        Arc::new(pir::ai::auth::InMemoryCredentialStore::new()),
        Arc::new(pir::ai::auth::MapAuthContext::new(env)),
    );
    registry.set_provider(Arc::new(provider));
    registry
}

fn test_telemetry() -> (TestTelemetry, Telemetry) {
    let recorder = TestTelemetry::new();
    let telemetry = Telemetry::from_recorder(recorder.clone());
    (recorder, telemetry)
}

fn int(value: &AttributeValue) -> Option<i64> {
    match value {
        AttributeValue::Int(value) => Some(*value),
        _ => None,
    }
}

fn string(value: &AttributeValue) -> Option<&str> {
    match value {
        AttributeValue::String(value) => Some(value),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Span shape, parentage, events, metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_span_has_upstream_attributes_and_is_a_child_of_the_context_span() {
    let (recorder, telemetry) = test_telemetry();
    let parent = telemetry.start_span_named("agent.turn");

    let (provider, _headers) = ScriptedProvider::new(scripted_events());
    let registry = registry(provider);
    let options = StreamOptions {
        telemetry: Some(parent.clone()),
        reasoning: Some(ThinkingLevel::High),
        ..StreamOptions::default()
    };
    let stream = registry.stream_unchecked(model(), context(), options);
    let result = stream.drain_and_result().await.expect("stream terminates");
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    parent.end();

    let span = recorder
        .span("pi.ai.request")
        .expect("the request span is recorded");
    assert_eq!(
        span.attributes.get("provider"),
        Some(&AttributeValue::String(PROVIDER_ID.into()))
    );
    assert_eq!(
        span.attributes.get("model"),
        Some(&AttributeValue::String(MODEL_ID.into()))
    );
    assert_eq!(
        span.attributes.get("api"),
        Some(&AttributeValue::String(API_ID.into()))
    );
    // Upstream `startAttributes.reasoning` carries the scalar level.
    assert_eq!(
        span.attributes.get("reasoning"),
        Some(&AttributeValue::String("high".into()))
    );
    // Child of the caller's context span: same trace, parent span id = parent.
    assert_eq!(
        span.parent_trace_id,
        Some(parent.trace_context().trace_id())
    );
    assert_eq!(span.parent_span_id, Some(parent.trace_context().span_id()));
    assert_eq!(span.trace_id, parent.trace_context().trace_id());

    // End attributes (upstream `onResult`).
    assert_eq!(
        span.attributes.get("stop"),
        Some(&AttributeValue::String("toolUse".into()))
    );
    assert_eq!(int(span.attributes.get("usage.input").unwrap()), Some(11));
    assert_eq!(int(span.attributes.get("usage.output").unwrap()), Some(7));
    assert_eq!(
        int(span.attributes.get("usage.cache_read").unwrap()),
        Some(3)
    );
    assert_eq!(
        int(span.attributes.get("usage.cache_write").unwrap()),
        Some(2)
    );
    assert_eq!(
        span.attributes.get("cost.total"),
        Some(&AttributeValue::Float(0.25))
    );
    assert_eq!(span.status, SpanStatus::Ok);
    assert!(span.ended);
}

#[tokio::test]
async fn first_chunk_and_block_events_follow_upstream_order() {
    let (recorder, telemetry) = test_telemetry();
    let parent = telemetry.start_span_named("agent.turn");
    let (provider, _headers) = ScriptedProvider::new(scripted_events());
    let registry = registry(provider);
    let stream = registry.stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(parent),
            ..StreamOptions::default()
        },
    );
    stream.drain_and_result().await.expect("stream terminates");

    let span = recorder.span("pi.ai.request").expect("request span");
    let names: Vec<&str> = span
        .events
        .iter()
        .map(|event| event.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "pi.ai.first_chunk",
            "pi.ai.block",
            "pi.ai.block",
            "pi.ai.block"
        ],
        "first_chunk precedes every block event, exactly once"
    );
    // text, thinking, tool_call — with JS `String.length` (UTF-16) char counts.
    let blocks = &span.events[1..];
    assert_eq!(
        string(blocks[0].attributes.get("type").unwrap()),
        Some("text")
    );
    assert_eq!(int(blocks[0].attributes.get("index").unwrap()), Some(0));
    assert_eq!(int(blocks[0].attributes.get("chars").unwrap()), Some(5));
    assert_eq!(
        string(blocks[1].attributes.get("type").unwrap()),
        Some("thinking")
    );
    assert_eq!(int(blocks[1].attributes.get("index").unwrap()), Some(1));
    assert_eq!(int(blocks[1].attributes.get("chars").unwrap()), Some(3));
    assert_eq!(
        string(blocks[2].attributes.get("type").unwrap()),
        Some("tool_call")
    );
    assert_eq!(int(blocks[2].attributes.get("index").unwrap()), Some(2));
    // 8 bytes of `{"path":` plus 4 of `"a"}` accumulated across the deltas.
    assert_eq!(int(blocks[2].attributes.get("chars").unwrap()), Some(12));
    assert_eq!(
        string(blocks[2].attributes.get("tool").unwrap()),
        Some("read")
    );
    // Content is never captured on an event.
    assert!(blocks
        .iter()
        .all(|block| block.attributes.get("content").is_none()));
    // ... and never anywhere else in the recorded span either.
    let rendered = format!("{span:?}");
    assert!(
        !rendered.contains("hello") && !rendered.contains("why"),
        "block content must never reach the span: {rendered}"
    );
}

/// Upstream `counts interleaved tool deltas independently`: the char count is
/// accumulated per content index, and a `toolcall_end` without deltas records
/// zero — the observer never reads the arguments.
#[tokio::test]
async fn interleaved_tool_deltas_count_per_content_index() {
    let partial = Arc::new(AssistantMessage {
        api: API_ID.into(),
        provider: PROVIDER_ID.into(),
        model: MODEL_ID.into(),
        ..AssistantMessage::default()
    });
    let delta = |index: usize, value: &str| AssistantMessageEvent::ToolCallDelta {
        content_index: index,
        delta: value.into(),
        partial: Arc::clone(&partial),
    };
    let end = |index: usize, name: &str| AssistantMessageEvent::ToolCallEnd {
        content_index: index,
        tool_call: tool_call(name),
        partial: Arc::clone(&partial),
    };
    let (recorder, telemetry) = test_telemetry();
    registry(
        ScriptedProvider::new(vec![
            AssistantMessageEvent::Start {
                partial: Arc::clone(&partial),
            },
            delta(2, "ab"),
            delta(3, "123"),
            delta(2, "c"),
            end(3, "lookup"),
            end(2, "lookup"),
            end(4, "other"),
            AssistantMessageEvent::Done {
                reason: TerminalReason::ToolUse,
                message: (*partial).clone(),
            },
        ])
        .0,
    )
    .stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(telemetry.start_span_named("agent.turn")),
            ..StreamOptions::default()
        },
    )
    .drain_and_result()
    .await
    .expect("stream terminates");

    let span = recorder.span("pi.ai.request").expect("request span");
    let blocks: Vec<(String, i64, Option<String>)> = span
        .events
        .iter()
        .skip(1)
        .map(|event| {
            (
                string(event.attributes.get("type").unwrap())
                    .unwrap()
                    .to_owned(),
                int(event.attributes.get("chars").unwrap()).unwrap(),
                event
                    .attributes
                    .get("tool")
                    .and_then(string)
                    .map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        blocks,
        vec![
            ("tool_call".to_string(), 3, Some("lookup".to_string())),
            ("tool_call".to_string(), 3, Some("lookup".to_string())),
            ("tool_call".to_string(), 0, Some("other".to_string())),
        ],
        "deltas accumulate per index; a delta-less end records zero"
    );
}

/// Upstream `observeResult(undefined)`: a stream that ends without a terminal
/// message still ends its span, but records no request-duration measurement
/// and no token usage.
#[tokio::test]
async fn stream_without_terminal_event_ends_the_span_without_measurements() {
    let (recorder, telemetry) = test_telemetry();
    let result = registry(
        ScriptedProvider::without_terminal(vec![
            AssistantMessageEvent::Start {
                partial: Arc::new(AssistantMessage::default()),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "hi".into(),
                partial: Arc::new(AssistantMessage::default()),
            },
        ])
        .0,
    )
    .stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(telemetry.start_span_named("agent.turn")),
            ..StreamOptions::default()
        },
    )
    .drain_and_result()
    .await;
    assert!(
        result.is_none(),
        "a stream without a terminal event has no result"
    );

    let span = recorder.span("pi.ai.request").expect("request span");
    assert!(span.ended, "the span is ended even without a result");
    assert_eq!(
        span.events
            .iter()
            .map(|event| event.name.as_str())
            .collect::<Vec<_>>(),
        vec!["pi.ai.first_chunk", "pi.ai.block"]
    );
    assert_eq!(
        recorder
            .histogram_snapshot("pi.ai.request.duration")
            .expect("instrument was created with the span")
            .count,
        0,
        "no request duration without a terminal message"
    );
    assert!(
        recorder
            .counter_snapshot("pi.ai.tokens")
            .expect("instrument was created with the span")
            .points
            .is_empty(),
        "no token usage without a terminal message"
    );
}

#[tokio::test]
async fn duration_histograms_and_token_counter_record_once_per_request() {
    let (recorder, telemetry) = test_telemetry();
    let (provider, _headers) = ScriptedProvider::new(scripted_events());
    let registry = registry(provider);
    let stream = registry.stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(telemetry.start_span_named("agent.turn")),
            ..StreamOptions::default()
        },
    );
    stream.drain_and_result().await.expect("stream terminates");

    let request_duration = recorder
        .histogram_snapshot("pi.ai.request.duration")
        .expect("request duration histogram");
    assert_eq!(request_duration.count, 1, "one measurement per request");
    assert_eq!(request_duration.unit.as_deref(), Some("ms"));
    let point = request_duration.points.first().expect("one point");
    assert_eq!(
        string(point.attributes.get("provider").unwrap()),
        Some(PROVIDER_ID)
    );
    assert_eq!(
        string(point.attributes.get("model").unwrap()),
        Some(MODEL_ID)
    );
    assert_eq!(
        string(point.attributes.get("stop").unwrap()),
        Some("toolUse")
    );

    let first_chunk = recorder
        .histogram_snapshot("pi.ai.first_chunk.duration")
        .expect("first chunk histogram");
    assert_eq!(first_chunk.count, 1);

    let tokens = recorder
        .counter_snapshot("pi.ai.tokens")
        .expect("token counter");
    assert_eq!(tokens.unit.as_deref(), Some("token"));
    let by_kind = |kind: &str| {
        tokens
            .points
            .iter()
            .find(|point| string(point.attributes.get("kind").unwrap()) == Some(kind))
            .map(|point| point.value)
    };
    assert_eq!(by_kind("input"), Some(11));
    assert_eq!(by_kind("output"), Some(7));
    assert_eq!(by_kind("cache_read"), Some(3));
    assert_eq!(by_kind("cache_write"), Some(2));
    assert_eq!(tokens.points.len(), 4);
}

#[tokio::test]
async fn error_stop_sets_a_bounded_error_status_and_aborted_stays_ok() {
    let message = |stop: StopReason, error: Option<&str>| AssistantMessage {
        api: API_ID.into(),
        provider: PROVIDER_ID.into(),
        model: MODEL_ID.into(),
        stop_reason: stop,
        error_message: error.map(str::to_owned),
        ..AssistantMessage::default()
    };
    let long_error = "x".repeat(900);

    let (recorder, telemetry) = test_telemetry();
    let (provider, _headers) = ScriptedProvider::new(vec![AssistantMessageEvent::Error {
        reason: ErrorReason::Error,
        error: message(StopReason::Error, Some(&long_error)),
    }]);
    registry(provider)
        .stream_unchecked(
            model(),
            context(),
            StreamOptions {
                telemetry: Some(telemetry.start_span_named("agent.turn")),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("stream terminates");
    let error_span = recorder.span("pi.ai.request").expect("request span");
    let SpanStatus::Error {
        error: Some(error), ..
    } = &error_span.status
    else {
        panic!("an `error` stop reason must set the error status");
    };
    assert_eq!(error.name, "provider_error");
    assert_eq!(error.message.len(), 500, "upstream caps the message at 500");
    assert_eq!(
        string(error_span.attributes.get("stop").unwrap()),
        Some("error")
    );

    // `aborted` stays ok (upstream `errorWhen` only names `error`).
    let (recorder, telemetry) = test_telemetry();
    let (provider, _headers) = ScriptedProvider::new(vec![AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error: message(StopReason::Aborted, None),
    }]);
    registry(provider)
        .stream_unchecked(
            model(),
            context(),
            StreamOptions {
                telemetry: Some(telemetry.start_span_named("agent.turn")),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("stream terminates");
    let aborted_span = recorder.span("pi.ai.request").expect("request span");
    assert_eq!(aborted_span.status, SpanStatus::Ok);
    assert_eq!(
        recorder
            .histogram_snapshot("pi.ai.request.duration")
            .expect("request duration")
            .count,
        1
    );
}

// ---------------------------------------------------------------------------
// Setup failures
// ---------------------------------------------------------------------------

/// Upstream `setup failures are part of the logical request`: an unknown
/// provider still opens the request span, records the terminal error event's
/// first chunk, and sets the bounded error status — without leaking the error
/// text into span attributes.
#[tokio::test]
async fn setup_failure_still_opens_the_request_span_and_sets_error_status() {
    let (recorder, telemetry) = test_telemetry();
    let mut unknown = model();
    unknown.provider = "not-registered".into();

    let stream = registry(ScriptedProvider::new(scripted_events()).0).stream_unchecked(
        unknown,
        context(),
        StreamOptions {
            telemetry: Some(telemetry.start_span_named("agent.turn")),
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        },
    );
    let result = stream
        .drain_and_result()
        .await
        .expect("the error stream ends");
    assert_eq!(result.stop_reason, StopReason::Error);

    let span = recorder.span("pi.ai.request").expect("request span");
    assert_eq!(
        span.attributes.get("provider"),
        Some(&AttributeValue::String("not-registered".into()))
    );
    // The terminal error event is observed like any other event, so the first
    // chunk is recorded; the request histogram carries its `error` stop.
    assert_eq!(span.events.len(), 1);
    assert_eq!(span.events[0].name, "pi.ai.first_chunk");
    let SpanStatus::Error {
        error: Some(error), ..
    } = &span.status
    else {
        panic!("the setup failure must set the error status");
    };
    assert_eq!(error.name, "provider_error");
    assert!(
        error.message.contains("Unknown provider"),
        "the status carries the failure message"
    );
    assert!(
        span.attributes.get("error").is_none()
            && !format!("{:?}", span.attributes).contains("Unknown provider"),
        "the error text stays off the span attributes"
    );
    let histogram = recorder
        .histogram_snapshot("pi.ai.request.duration")
        .expect("request duration");
    assert_eq!(histogram.count, 1);
    assert_eq!(
        string(histogram.points[0].attributes.get("stop").unwrap()),
        Some("error")
    );
}

// ---------------------------------------------------------------------------
// Propagation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_headers_carry_traceparent_and_tracestate() {
    let (_recorder, telemetry) = test_telemetry();
    // A caller context with a remote tracestate: injection must forward both.
    let remote = TraceContext::new(
        pir::telemetry::TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap(),
        pir::telemetry::SpanId::from_hex("b7ad6b7169203331").unwrap(),
        pir::telemetry::TraceFlags::SAMPLED,
    )
    .with_trace_state("vendor=value")
    .with_remote(true);
    let parent = telemetry.start_span(SpanOptions::new("agent.turn").with_parent(remote.clone()));

    let (provider, headers) = ScriptedProvider::new(scripted_events());
    let request = registry(provider).stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(parent.clone()),
            headers: Some(ProviderHeaders::from([(
                "x-existing".to_string(),
                "kept".to_string(),
            )])),
            ..StreamOptions::default()
        },
    );
    request.drain_and_result().await.expect("stream terminates");

    let request_span = _recorder.span("pi.ai.request").expect("request span");
    let sent = headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop()
        .flatten()
        .expect("headers were sent");
    let traceparent = sent.get("traceparent").expect("traceparent header");
    // The library's own propagator is the reference rendering.
    let mut expected_carrier = Carrier::new();
    assert!(W3cPropagator.inject(
        &TraceContext::new(
            request_span.trace_id,
            request_span.span_id,
            pir::telemetry::TraceFlags::SAMPLED,
        ),
        &mut expected_carrier,
    ));
    assert_eq!(
        traceparent.as_str(),
        expected_carrier
            .get("traceparent")
            .expect("expected traceparent")
            .as_str()
    );
    assert_eq!(
        sent.get("tracestate").map(String::as_str),
        Some("vendor=value")
    );
    assert_eq!(
        sent.get("x-existing").map(String::as_str),
        Some("kept"),
        "propagation adds headers, it never replaces the caller's"
    );

    // The header parses back to the same trace/span identity through the
    // AI-path extract helper (which delegates to the W3C propagator).
    let extracted =
        pir::ai::provider::extract_trace_context(&sent).expect("the injected headers extract");
    assert_eq!(extracted.trace_id(), request_span.trace_id);
    assert_eq!(extracted.span_id(), request_span.span_id);
    assert!(extracted.is_sampled());
    assert_eq!(extracted.trace_state(), Some("vendor=value"));
    assert_eq!(traceparent.len(), 55, "00-<32>-<16>-<2>");
}

#[tokio::test]
async fn trace_headers_extract_round_trips_through_the_registry_path() {
    // `ProviderRegistry::stream` (the auth path) injects exactly like the
    // unchecked path; the registry's auth context supplies the credential.
    let (recorder, telemetry) = test_telemetry();
    let parent = telemetry.start_span_named("agent.turn");
    let parent_context = parent.trace_context().clone();
    let (provider, headers) = ScriptedProvider::new(vec![AssistantMessageEvent::Done {
        reason: TerminalReason::Stop,
        message: AssistantMessage {
            api: API_ID.into(),
            provider: PROVIDER_ID.into(),
            model: MODEL_ID.into(),
            stop_reason: StopReason::Stop,
            ..AssistantMessage::default()
        },
    }]);
    authenticated_registry(provider)
        .stream(
            model(),
            context(),
            StreamOptions {
                telemetry: Some(parent),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("stream terminates");

    // The auth path opens the same request span as the unchecked path.
    let request_span = recorder.span("pi.ai.request").expect("request span");
    assert_eq!(request_span.trace_id, parent_context.trace_id());
    assert_eq!(request_span.parent_span_id, Some(parent_context.span_id()));
    assert_eq!(request_span.status, SpanStatus::Ok);
    assert!(request_span.ended);

    let sent = headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop()
        .flatten()
        .expect("headers were sent");
    let traceparent = sent.get("traceparent").expect("traceparent header");
    let extracted =
        pir::ai::provider::extract_trace_context(&sent).expect("the injected headers extract");
    assert!(extracted.trace_id().is_valid());
    assert!(extracted.span_id().is_valid());
    assert_eq!(extracted.trace_id().to_string(), traceparent[3..35]);
}

// ---------------------------------------------------------------------------
// Disabled path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn noop_path_records_nothing_and_adds_no_headers() {
    let (recorder, telemetry) = test_telemetry();

    // (a) no telemetry at all
    let (provider, headers) = ScriptedProvider::new(scripted_events());
    registry(provider)
        .stream_unchecked(model(), context(), StreamOptions::default())
        .drain_and_result()
        .await
        .expect("stream terminates");
    // (b) an explicit noop span handle
    let (provider, noop_headers) = ScriptedProvider::new(scripted_events());
    registry(provider)
        .stream_unchecked(
            model(),
            context(),
            StreamOptions {
                telemetry: Some(pir::telemetry::Span::noop()),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("stream terminates");

    assert!(
        recorder.spans().is_empty(),
        "the disabled path starts no span"
    );
    assert_eq!(recorder.metrics_snapshot(), Default::default());
    for headers in [headers, noop_headers] {
        let sent = headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .flatten();
        assert!(
            sent.is_none_or(|headers| {
                !headers.contains_key("traceparent") && !headers.contains_key("tracestate")
            }),
            "the disabled path adds no propagation header"
        );
    }

    // (c) the auth path: same noop behaviour, and the request carries no
    // header map at all (nothing was allocated for propagation).
    let (provider, auth_headers) = ScriptedProvider::new(scripted_events());
    authenticated_registry(provider)
        .stream(model(), context(), StreamOptions::default())
        .drain_and_result()
        .await
        .expect("stream terminates");
    assert!(
        recorder.spans().is_empty(),
        "the auth path starts no span either"
    );
    assert_eq!(recorder.metrics_snapshot(), Default::default());
    assert_eq!(
        auth_headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .flatten(),
        None,
        "no telemetry: the request header map stays absent"
    );

    // Positive control: the same recorder observes an enabled request, so the
    // empty captures above distinguish the disabled path from a feature that
    // does nothing at all.
    let (provider, control_headers) = ScriptedProvider::new(scripted_events());
    registry(provider)
        .stream_unchecked(
            model(),
            context(),
            StreamOptions {
                telemetry: Some(telemetry.start_span_named("agent.turn")),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("stream terminates");
    assert!(
        recorder.span("pi.ai.request").is_some(),
        "an enabled request is recorded by the same recorder"
    );
    let control = control_headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop()
        .flatten()
        .expect("the control request carries headers");
    assert!(
        control.contains_key("traceparent"),
        "the control request carries the propagation header"
    );
}

#[test]
fn context_and_stream_options_serialization_is_unchanged_without_telemetry() {
    // `StreamOptions` is request-scoped runtime state and is never serialized;
    // adding the span handle keeps it out of every persisted format.
    let options = StreamOptions {
        telemetry: Some(pir::telemetry::Span::noop()),
        ..StreamOptions::default()
    };
    assert!(options
        .telemetry
        .as_ref()
        .is_some_and(|span| span.is_noop()));
    // `StreamOptions::default()` has no telemetry: the byte-for-byte default.
    assert!(StreamOptions::default().telemetry.is_none());

    // `Context` keeps its exact upstream wire shape (`systemPrompt`,
    // `messages`, `tools`) — no telemetry key, no span in its equality.
    let context = context();
    let value = serde_json::to_value(&context).expect("Context serializes");
    let object = value.as_object().expect("object");
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["messages"], "bare context: messages only");
    let full = Context {
        system_prompt: Some("sys".into()),
        messages: Vec::new(),
        tools: None,
    };
    let value = serde_json::to_value(&full).expect("Context serializes");
    assert_eq!(value, json!({ "systemPrompt": "sys", "messages": [] }));
    assert!(value.get("telemetry").is_none());
    assert_eq!(
        full,
        Context {
            system_prompt: Some("sys".into()),
            messages: Vec::new(),
            tools: None,
        },
        "Context equality is unaffected by the request-scoped span"
    );
}

// ---------------------------------------------------------------------------
// Usage.cost.totalFromProvider through the registry cost pass + telemetry
// ---------------------------------------------------------------------------

/// Model rates large enough that one million tokens costs a whole dollar, so
/// an accidental catalog re-price is visible.
fn costed_model() -> Model {
    let mut model = model();
    model.cost.rates.input = 1.0;
    model.cost.rates.output = 2.0;
    model
}

fn terminal_usage(input: u64, output: u64, total: f64, billed: Option<bool>) -> Usage {
    Usage {
        input,
        output,
        total_tokens: input + output,
        cost: Cost {
            total,
            total_from_provider: billed,
            ..Cost::default()
        },
        ..Usage::default()
    }
}

fn terminal_events(model: &Model, usage: Usage) -> Vec<AssistantMessageEvent> {
    let partial = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        ..AssistantMessage::default()
    };
    let message = AssistantMessage {
        usage,
        stop_reason: StopReason::Stop,
        ..partial.clone()
    };
    vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(partial),
        },
        AssistantMessageEvent::Done {
            reason: TerminalReason::Stop,
            message,
        },
    ]
}

#[tokio::test]
async fn provider_billed_zero_total_survives_the_registry_cost_pass() {
    let model = costed_model();
    // OpenRouter-style accounting: the provider explicitly billed zero. The
    // registry's estimate pass must NOT overwrite it with the catalog price.
    let (provider, _headers) = ScriptedProvider::new(terminal_events(
        &model,
        terminal_usage(1_000_000, 0, 0.0, Some(true)),
    ));
    let (recorder, telemetry) = test_telemetry();
    let message = registry(provider)
        .stream_unchecked(
            model,
            context(),
            StreamOptions {
                telemetry: Some(telemetry.start_span_named("agent.turn")),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("terminal message");

    assert_eq!(message.usage.cost.total, 0.0);
    assert!(message.usage.cost.is_provider_billed());
    let costs = recorder
        .float_counter_snapshot("pi.ai.cost")
        .expect("cost counter");
    assert_eq!(costs.total, 0.0, "telemetry records the billed zero");
}

#[tokio::test]
async fn absent_or_estimate_zero_total_is_repriced_and_marked_false() {
    let model = costed_model();
    // No provider-reported cost (absent marker): the registry prices it from
    // the catalog and stamps the estimate marker, and telemetry sees the
    // corrected total.
    let (provider, _headers) = ScriptedProvider::new(terminal_events(
        &model,
        terminal_usage(1_000_000, 1_000_000, 0.0, None),
    ));
    let (recorder, telemetry) = test_telemetry();
    let message = registry(provider)
        .stream_unchecked(
            model.clone(),
            context(),
            StreamOptions {
                telemetry: Some(telemetry.start_span_named("agent.turn")),
                ..StreamOptions::default()
            },
        )
        .drain_and_result()
        .await
        .expect("terminal message");

    assert_eq!(message.usage.cost.input, 1.0);
    assert_eq!(message.usage.cost.output, 2.0);
    assert_eq!(message.usage.cost.total, 3.0);
    assert_eq!(message.usage.cost.total_from_provider, Some(false));
    let costs = recorder
        .float_counter_snapshot("pi.ai.cost")
        .expect("cost counter");
    assert_eq!(costs.total, 3.0);

    // An explicit estimate marker with a zero total is repriced the same way.
    let (provider, _headers) = ScriptedProvider::new(terminal_events(
        &model,
        terminal_usage(1_000_000, 0, 0.0, Some(false)),
    ));
    let message = registry(provider)
        .stream_unchecked(model, context(), StreamOptions::default())
        .drain_and_result()
        .await
        .expect("terminal message");
    assert_eq!(message.usage.cost.total, 1.0);
    assert_eq!(message.usage.cost.total_from_provider, Some(false));
}
