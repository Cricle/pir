//! Run-diagnostics collector tests (`src/diagnostics.rs`) plus the
//! `AgentSession::last_run_diagnostics()` accessor.
//!
//! The pure-collector tests feed synthetic agent events through
//! `RunDiagnosticsCollector::record_at` with explicit `Instant`s, so every
//! duration is exact and deterministic — no sleeps, no real clock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::agent::agent_loop::{AgentEvent, LoopResult};
use pir::agent::tool_batch::ToolExecutionEvent;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, ModelCost,
    ModelCostRates, StopReason, TerminalReason, TextContent, ThinkingLevel, Usage,
};
use pir::diagnostics::RunDiagnosticsCollector;

// ---------------------------------------------------------------------------
// Synthetic event helpers
// ---------------------------------------------------------------------------

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    }
}

fn assistant_with_usage(usage: Usage) -> AssistantMessage {
    AssistantMessage {
        usage,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, total: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: total,
        ..Usage::default()
    }
}

fn text_delta(delta: &str) -> AgentEvent {
    AgentEvent::StreamEvent {
        turn: 1,
        event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: delta.into(),
            partial: Arc::new(AssistantMessage::default()),
        },
    }
}

fn tool_end(name: &str, ms: Option<u64>, is_error: bool, parent: Option<&str>) -> AgentEvent {
    AgentEvent::ToolExecutionEnd {
        turn: 1,
        event: ToolExecutionEvent::End {
            index: 0,
            call_id: format!("call-{name}"),
            tool_name: name.into(),
            is_error,
            duration_ms: ms,
            parent_call_id: parent.map(str::to_owned),
        },
        result: serde_json::Value::Null,
    }
}

fn turn_start() -> AgentEvent {
    AgentEvent::TurnStart {
        turn: 1,
        context: Arc::new(context()),
    }
}

fn stream_start() -> AgentEvent {
    AgentEvent::StreamStart {
        turn: 1,
        message: AssistantMessage::default(),
    }
}

fn stream_end(usage: Usage) -> AgentEvent {
    AgentEvent::StreamEnd {
        turn: 1,
        message: assistant_with_usage(usage),
    }
}

fn turn_end() -> AgentEvent {
    AgentEvent::TurnEnd {
        turn: 1,
        message: AssistantMessage::default(),
        tool_results: Vec::new(),
    }
}

fn agent_start() -> AgentEvent {
    AgentEvent::AgentStart {
        context: Arc::new(context()),
    }
}

fn agent_end() -> AgentEvent {
    AgentEvent::AgentEnd {
        result: Arc::new(LoopResult {
            terminal: AssistantMessage::default(),
            context: Arc::new(context()),
            tool_results: Vec::new(),
            new_messages: Vec::new(),
            preamble: Vec::new(),
            turns: 1,
            aborted: false,
            usage_total: 0,
            will_retry: false,
        }),
    }
}

/// Feed one event at `base + ms` (the deterministic test seam).
fn feed(collector: &RunDiagnosticsCollector, base: Instant, ms: u64, event: &AgentEvent) {
    collector.record_at(event, base + Duration::from_millis(ms));
}

/// A deterministic two-turn run:
///
/// turn 1: request at +100ms, first delta at +300ms (200ms TTFT), stream ends
/// at +1000ms (900ms), two tool calls (`bash` 4000ms, nested `read` 12ms),
/// turn ends at +1100ms; usage 100/50/20/5 (175).
/// turn 2: request at +1300ms, first delta at +1350ms (50ms TTFT), stream ends
/// at +2000ms (700ms), no tools, turn ends at +2100ms; usage 10/2/0/0 (12).
/// Run wall clock: +0ms → +2200ms.
fn two_turn_run() -> pir::diagnostics::RunDiagnostics {
    let collector = RunDiagnosticsCollector::new(true);
    let base = Instant::now();
    for (ms, event) in [
        (0, agent_start()),
        (0, turn_start()),
        (100, stream_start()),
        (
            110,
            AgentEvent::StreamEvent {
                turn: 1,
                event: AssistantMessageEvent::Start {
                    partial: Arc::new(AssistantMessage::default()),
                },
            },
        ),
        (300, text_delta("a")),
        (310, text_delta("b")),
        (
            320,
            AgentEvent::StreamEvent {
                turn: 1,
                event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "t".into(),
                    partial: Arc::new(AssistantMessage::default()),
                },
            },
        ),
        (1000, stream_end(usage(100, 50, 20, 5, 175))),
        (
            1010,
            AgentEvent::ToolExecutionStart {
                turn: 1,
                event: ToolExecutionEvent::Start {
                    index: 0,
                    call_id: "call-bash".into(),
                    tool_name: "bash".into(),
                    parent_call_id: None,
                },
                args: serde_json::Value::Null,
            },
        ),
        (1030, tool_end("bash", Some(4000), false, None)),
        (1040, tool_end("read", Some(12), false, Some("call-bash"))),
        (1100, turn_end()),
        (1200, turn_start()),
        (1300, stream_start()),
        (
            1350,
            AgentEvent::StreamEvent {
                turn: 1,
                event: AssistantMessageEvent::ToolCallDelta {
                    content_index: 0,
                    delta: "{}".into(),
                    partial: Arc::new(AssistantMessage::default()),
                },
            },
        ),
        (2000, stream_end(usage(10, 2, 0, 0, 12))),
        (2100, turn_end()),
        (2200, agent_end()),
    ] {
        feed(&collector, base, ms, &event);
    }
    collector.finish().expect("two-turn run recorded")
}

// ---------------------------------------------------------------------------
// Pure collector tests
// ---------------------------------------------------------------------------

#[test]
fn collector_derives_ttft_stream_deltas_tools_and_usage() {
    let diagnostics = two_turn_run();

    assert_eq!(diagnostics.wall_ms, 2200);
    assert_eq!(diagnostics.turns.len(), 2);

    let first = &diagnostics.turns[0];
    assert_eq!(first.turn, 1);
    // request (StreamStart) +100ms → first delta +300ms.
    assert_eq!(first.provider_ms, 200);
    // StreamStart +100ms → StreamEnd +1000ms.
    assert_eq!(first.stream_ms, 900);
    // TurnStart +0ms → TurnEnd +1100ms (provider + stream + tools).
    assert_eq!(first.total_ms, 1100);
    // text_delta x2 + thinking_delta x1 (the `start` event is not a delta).
    assert_eq!(first.deltas, 3);
    assert_eq!(
        first
            .tool_calls
            .iter()
            .map(|call| (call.name.as_str(), call.ms, call.is_error, call.nested))
            .collect::<Vec<_>>(),
        vec![
            ("bash", Some(4000), false, false),
            ("read", Some(12), false, true)
        ]
    );
    assert_eq!(first.usage.input, 100);
    assert_eq!(first.usage.output, 50);
    assert_eq!(first.usage.cache_read, 20);
    assert_eq!(first.usage.cache_write, 5);
    assert_eq!(first.usage.total_tokens, 175);

    let second = &diagnostics.turns[1];
    assert_eq!(second.turn, 2);
    assert_eq!(second.provider_ms, 50);
    assert_eq!(second.stream_ms, 700);
    assert_eq!(second.total_ms, 900);
    assert_eq!(second.deltas, 1);
    assert!(second.tool_calls.is_empty());
    assert_eq!(second.usage.total_tokens, 12);

    let total = &diagnostics.total;
    assert_eq!(total.turns, 2);
    assert_eq!(total.provider_ms, 250);
    assert_eq!(total.stream_ms, 1600);
    assert_eq!(total.total_ms, 2000);
    assert_eq!(total.deltas, 4);
    assert_eq!(total.tool_calls, 2);
    assert_eq!(total.usage.input, 110);
    assert_eq!(total.usage.output, 52);
    assert_eq!(total.usage.cache_read, 20);
    assert_eq!(total.usage.cache_write, 5);
    assert_eq!(total.usage.total_tokens, 187);

    let slowest_turn = diagnostics.slowest_turn.as_ref().expect("slowest turn");
    assert_eq!((slowest_turn.turn, slowest_turn.total_ms), (1, 1100));
    let slowest_tool = diagnostics.slowest_tool.as_ref().expect("slowest tool");
    assert_eq!(
        (
            slowest_tool.name.as_str(),
            slowest_tool.ms,
            slowest_tool.turn
        ),
        ("bash", 4000, 1)
    );
}

#[test]
fn collector_counts_no_delta_streams_and_untimed_tool_calls() {
    let collector = RunDiagnosticsCollector::new(true);
    let base = Instant::now();
    for (ms, event) in [
        (0, agent_start()),
        (0, turn_start()),
        // An immediate provider error: StreamStart (the loop's fallback) and
        // StreamEnd arrive with no delta between them.
        (200, stream_start()),
        (200, stream_end(usage(0, 0, 0, 0, 0))),
        // A preflight/validation failure carries no duration.
        (210, tool_end("bash", None, true, None)),
        (5000, turn_end()),
        (5100, agent_end()),
    ] {
        feed(&collector, base, ms, &event);
    }
    let diagnostics = collector.finish().expect("run recorded");
    let turn = &diagnostics.turns[0];
    assert_eq!(turn.provider_ms, 0, "no delta means no TTFT measurement");
    assert_eq!(turn.stream_ms, 0);
    assert_eq!(turn.total_ms, 5000);
    assert_eq!(turn.deltas, 0);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].ms, None);
    assert!(turn.tool_calls[0].is_error);
    // The untimed call is never the slowest tool.
    assert!(diagnostics.slowest_tool.is_none());
    assert_eq!(
        diagnostics.slowest_turn.as_ref().map(|turn| turn.turn),
        Some(1)
    );
}

#[test]
fn collector_is_inert_when_disabled() {
    let collector = RunDiagnosticsCollector::new(false);
    assert!(!collector.enabled());
    let base = Instant::now();
    for (ms, event) in [
        (0, agent_start()),
        (0, turn_start()),
        (100, stream_start()),
        (200, text_delta("a")),
        (1000, stream_end(usage(1, 1, 0, 0, 2))),
        (1100, turn_end()),
        (1200, agent_end()),
    ] {
        feed(&collector, base, ms, &event);
    }
    assert!(
        collector.finish().is_none(),
        "a disabled collector records nothing"
    );
}

#[test]
fn collector_numbers_turns_across_agent_runs() {
    // A print invocation with two prompts runs two agent runs through ONE
    // collector; the turn numbers stay consecutive over the whole stream and
    // the wall clock spans both runs.
    let collector = RunDiagnosticsCollector::new(true);
    let base = Instant::now();
    for (ms, event) in [
        (0, agent_start()),
        (0, turn_start()),
        (100, stream_start()),
        (150, text_delta("a")),
        (600, stream_end(usage(1, 1, 0, 0, 2))),
        (700, turn_end()),
        (800, agent_end()),
        (1000, agent_start()),
        (1000, turn_start()),
        (1100, stream_start()),
        (1150, text_delta("b")),
        (1600, stream_end(usage(1, 1, 0, 0, 2))),
        (1700, turn_end()),
        (1800, agent_end()),
    ] {
        feed(&collector, base, ms, &event);
    }
    let diagnostics = collector.finish().expect("run recorded");
    assert_eq!(
        diagnostics
            .turns
            .iter()
            .map(|turn| turn.turn)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(diagnostics.wall_ms, 1800);
    assert_eq!(diagnostics.total.turns, 2);
}

#[test]
fn collector_renders_a_compact_aligned_block() {
    let rendered = two_turn_run().render();
    let expected = "\
--- Run Diagnostics ---
 turn  provider  stream   total  deltas  tools
    1     200ms   900ms  1100ms       3  2 calls: bash 4000ms, read 12ms
    2      50ms   700ms   900ms       1  -
TOTAL     250ms  1600ms  2000ms       4  2 calls
usage: input=110 output=52 cache_read=20 cache_write=5 total=187
slowest_turn: 1 (1100ms)
slowest_tool: bash (4000ms, turn 1)";
    assert_eq!(rendered, expected);
}

#[test]
fn collector_wire_json_is_a_typed_frame() {
    let diagnostics = two_turn_run();
    let value = diagnostics.to_wire_json();
    assert_eq!(value["type"], "run_diagnostics");
    assert_eq!(value["wallMs"], 2200);
    assert_eq!(value["turns"][0]["turn"], 1);
    assert_eq!(value["turns"][0]["providerMs"], 200);
    assert_eq!(value["turns"][0]["streamMs"], 900);
    assert_eq!(value["turns"][0]["totalMs"], 1100);
    assert_eq!(value["turns"][0]["deltas"], 3);
    assert_eq!(value["turns"][0]["toolCalls"][0]["name"], "bash");
    assert_eq!(value["turns"][0]["toolCalls"][0]["ms"], 4000);
    assert_eq!(value["turns"][0]["toolCalls"][0]["isError"], false);
    assert_eq!(value["turns"][0]["toolCalls"][1]["nested"], true);
    assert_eq!(value["turns"][0]["usage"]["cacheRead"], 20);
    assert_eq!(value["turns"][0]["usage"]["totalTokens"], 175);
    assert_eq!(value["total"]["turns"], 2);
    assert_eq!(value["total"]["toolCalls"], 2);
    assert_eq!(value["total"]["usage"]["totalTokens"], 187);
    assert_eq!(value["slowestTurn"]["turn"], 1);
    assert_eq!(value["slowestTool"]["name"], "bash");

    // The plain summary round-trips through serde (the shape hosts persist).
    let plain = serde_json::to_value(&diagnostics).expect("serialize summary");
    let restored: pir::diagnostics::RunDiagnostics =
        serde_json::from_value(plain).expect("deserialize summary");
    assert_eq!(restored, diagnostics);
}

#[test]
fn empty_runs_have_no_summary() {
    let collector = RunDiagnosticsCollector::new(true);
    let base = Instant::now();
    feed(&collector, base, 0, &agent_start());
    feed(&collector, base, 10, &agent_end());
    assert!(
        collector.finish().is_none(),
        "events without a turn produce no summary"
    );
}

// ---------------------------------------------------------------------------
// Session accessor
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
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
        max_tokens: 256,
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

/// One scripted assistant turn: `start` → one `text_delta` → `done` with a
/// fixed usage. `streams` counts provider calls.
struct ScriptedProvider {
    streams: AtomicUsize,
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: pir::ai::types::StreamOptions,
    ) -> AssistantMessageEventStream {
        self.streams.fetch_add(1, Ordering::SeqCst);
        let message = AssistantMessage {
            api: "fake-api".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: "hello".into(),
                text_signature: None,
            })],
            usage: usage(30, 7, 1, 2, 40),
            stop_reason: StopReason::Stop,
            timestamp: 1,
            ..AssistantMessage::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage::default()),
        });
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hello".into(),
            partial: Arc::new(message.clone()),
        });
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

#[tokio::test]
async fn session_stores_last_run_diagnostics() {
    let provider = Arc::new(ScriptedProvider {
        streams: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider.clone());
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(std::env::temp_dir()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    assert!(
        agent.last_run_diagnostics().is_none(),
        "nothing stored before the first run"
    );

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handler: pir::agent_session::EventHandler = {
        let log = Arc::clone(&log);
        Arc::new(move |event: AgentEvent| {
            log.lock().expect("event log").push(
                pir::agent_session::agent_event_to_json(&event)["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
        })
    };
    agent.prompt("hi", handler).await.expect("prompt");

    // The host handler still saw the full lifecycle (the wrapper forwards).
    let kinds = log.lock().expect("event log").clone();
    assert_eq!(kinds.first().map(String::as_str), Some("agent_start"));
    assert_eq!(kinds.last().map(String::as_str), Some("agent_end"));

    let diagnostics = agent
        .last_run_diagnostics()
        .expect("summary stored after the run");
    assert_eq!(diagnostics.turns.len(), 1);
    let turn = &diagnostics.turns[0];
    assert_eq!(turn.turn, 1);
    assert_eq!(turn.deltas, 1);
    assert!(turn.tool_calls.is_empty());
    assert_eq!(turn.usage.input, 30);
    assert_eq!(turn.usage.output, 7);
    assert_eq!(turn.usage.cache_read, 1);
    assert_eq!(turn.usage.cache_write, 2);
    assert_eq!(turn.usage.total_tokens, 40);
    assert_eq!(diagnostics.total.usage.total_tokens, 40);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 1);

    // A second run replaces the stored summary (fresh collector per run).
    agent
        .prompt("again", Arc::new(|_| {}))
        .await
        .expect("prompt");
    let second = agent
        .last_run_diagnostics()
        .expect("summary stored after the second run");
    assert_eq!(second.turns.len(), 1);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 2);
}
