//! `pi.ai.cost` fractional USD counter (audit §7 #17).
//!
//! Upstream `packages/ai/src/telemetry.ts` records
//! `cost.add(message.usage.cost.total, {provider, model})` — a JS number, so
//! fractional dollars are preserved. pir's integer `Counter` cannot carry
//! 0.0123 USD; the additive `FloatCounter` instrument records it exactly.
//! These tests drive the real `ProviderRegistry::stream_unchecked` seam with a
//! scripted provider and inspect the in-memory recorder's measurements.

use std::sync::Arc;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Cost, Message, Model,
    ModelCost, ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, Usage,
    UserMessage, UserRole,
};
use pir::telemetry::{Attributes, MetricQuery, Telemetry, TelemetryStore, TestTelemetry};

const PROVIDER_ID: &str = "fake";
const MODEL_ID: &str = "fake-model";
const API_ID: &str = "fake-api";
const COST_TOTAL: f64 = 0.0123;

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
            input: 0.0023,
            output: 0.0075,
            cache_read: 0.0015,
            cache_write: 0.001,
            total: COST_TOTAL,
            ..Cost::default()
        },
    }
}

/// One terminal event plus the start event, like a provider that ends
/// normally.
fn scripted_events() -> Vec<AssistantMessageEvent> {
    let partial = AssistantMessage {
        api: API_ID.into(),
        provider: PROVIDER_ID.into(),
        model: MODEL_ID.into(),
        ..AssistantMessage::default()
    };
    let message = AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "hello".into(),
            text_signature: None,
        })],
        usage: usage(),
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

struct ScriptedProvider {
    events: Vec<AssistantMessageEvent>,
    /// End the stream without a terminal event (`observeResult(undefined)`).
    without_terminal: bool,
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
            vec!["PIR_W17_FAKE_API_KEY".to_string()],
        )]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        for event in &self.events {
            stream.push(event.clone());
        }
        if self.without_terminal {
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

fn test_telemetry() -> (TestTelemetry, Telemetry) {
    let recorder = TestTelemetry::new();
    let telemetry = Telemetry::from_recorder(recorder.clone());
    (recorder, telemetry)
}

fn identity() -> Attributes {
    Attributes::new()
        .with("provider", PROVIDER_ID)
        .with("model", MODEL_ID)
}

#[tokio::test]
async fn fractional_usd_cost_is_recorded_and_exported() {
    let (recorder, telemetry) = test_telemetry();
    let stream = registry(ScriptedProvider {
        events: scripted_events(),
        without_terminal: false,
    })
    .stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(telemetry.start_span_named("agent.turn")),
            ..StreamOptions::default()
        },
    );
    let result = stream
        .drain_and_result()
        .await
        .expect("the scripted stream ends with a message");
    assert_eq!(result.stop_reason, StopReason::Stop);

    // The span still carries the terminal cost attribute (unchanged).
    let span = recorder.span("pi.ai.request").expect("request span");
    assert_eq!(
        span.attributes.get("cost.total"),
        Some(&pir::telemetry::AttributeValue::Float(COST_TOTAL))
    );

    // The fractional counter preserves the exact USD total and its metadata.
    assert_eq!(pir::ai::provider::AI_COST_METRIC, "pi.ai.cost");
    let costs = recorder.find_metrics(&MetricQuery::new(pir::ai::provider::AI_COST_METRIC));
    assert_eq!(costs.len(), 1, "one cost measurement per terminal request");
    let cost = &costs[0];
    assert!((cost.sum - COST_TOTAL).abs() < f64::EPSILON);
    assert_eq!(cost.count, 1);
    assert_eq!(cost.kind, pir::telemetry::MetricKind::Counter);
    assert_eq!(cost.unit.as_deref(), Some("USD"));
    assert_eq!(cost.attributes, identity());

    let snapshot = recorder
        .float_counter_snapshot("pi.ai.cost")
        .expect("fractional counter snapshot");
    assert!((snapshot.total - COST_TOTAL).abs() < f64::EPSILON);
    assert_eq!(snapshot.unit.as_deref(), Some("USD"));
    assert_eq!(
        snapshot.description.as_deref(),
        Some("Reported model cost.")
    );

    // The attribute filter reaches the same row.
    let filtered =
        recorder.find_metrics(&MetricQuery::new("pi.ai.cost").with_attributes(identity()));
    assert_eq!(filtered.len(), 1);

    // The token counter is still an integer instrument with its own rows.
    let tokens = recorder.find_metrics(&MetricQuery::new("pi.ai.tokens"));
    assert_eq!(tokens.len(), 4, "input/output/cache_read/cache_write");
    let token_sum: f64 = tokens.iter().map(|metric| metric.sum).sum();
    assert_eq!(token_sum, 23.0);
    assert!(tokens
        .iter()
        .all(|metric| metric.unit.as_deref() == Some("token")));
}

#[tokio::test]
async fn cost_is_not_recorded_without_a_terminal_message() {
    let (recorder, telemetry) = test_telemetry();
    let stream = registry(ScriptedProvider {
        events: vec![AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage {
                api: API_ID.into(),
                provider: PROVIDER_ID.into(),
                model: MODEL_ID.into(),
                ..AssistantMessage::default()
            }),
        }],
        without_terminal: true,
    })
    .stream_unchecked(
        model(),
        context(),
        StreamOptions {
            telemetry: Some(telemetry.start_span_named("agent.turn")),
            ..StreamOptions::default()
        },
    );
    let _ = stream.drain_and_result().await;

    assert!(recorder
        .find_metrics(&MetricQuery::new("pi.ai.cost"))
        .is_empty());
    assert!(recorder
        .find_metrics(&MetricQuery::new("pi.ai.tokens"))
        .is_empty());
}

#[tokio::test]
async fn no_telemetry_means_no_cost_instrument_work() {
    // Disabled telemetry (`None`) must not allocate or record anything: the
    // observer stays `None`, and a recorder-less facade's float counter is a
    // noop.
    let stream = registry(ScriptedProvider {
        events: scripted_events(),
        without_terminal: false,
    })
    .stream_unchecked(model(), context(), StreamOptions::default());
    let result = stream
        .drain_and_result()
        .await
        .expect("the scripted stream ends with a message");
    assert_eq!(result.stop_reason, StopReason::Stop);

    let noop = Telemetry::noop();
    let recorder = TestTelemetry::new();
    let counter = noop.float_counter(
        "pi.ai.cost",
        &pir::telemetry::InstrumentOptions::new().with_unit("USD"),
    );
    assert!(!counter.is_enabled());
    counter.add(1.5, &Attributes::EMPTY);
    assert!(recorder
        .find_metrics(&MetricQuery::new("pi.ai.cost"))
        .is_empty());
    assert!(!noop.is_enabled());
}
