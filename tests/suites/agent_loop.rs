use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{
    run, run_with_options, AgentEvent, EventStreamFn, FinishTurnAction, FinishTurnContext,
    LoopDeps, LoopOptions, MessageEndRepairContext, PersistMessageFn, PrepareNextTurnUpdate,
    PromptMessage,
};
use pir::agent::tool_batch::ToolExecutionEvent;
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::ai::auth::{ApiKeyAuth, InMemoryCredentialStore, MapAuthContext};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("tool-result") })
    }
}

fn tool_call() -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// A tool with a REAL parameter schema — the scheduler's
/// `validate_arguments` preflight (`tool_scheduler/prepare.rs`) rejects any
/// call that does not satisfy it — that records every argument object it
/// executes with. Used to prove argument validation observes the
/// `message_end` replacement (an executed call is the only way a
/// schema-violating raw payload can reach the tool).
struct SchemaProbeTool {
    received: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl AgentTool for SchemaProbeTool {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn description(&self) -> &'static str {
        "schema probe"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer" },
            },
            "required": ["path"],
            "additionalProperties": false,
        }))
    }

    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(args.clone());
        Box::pin(async { ToolResult::ok("probe ran") })
    }
}

#[tokio::test]
async fn loop_appends_turn_messages_and_emits_lifecycle() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assert!(request
                        .context
                        .messages
                        .iter()
                        .any(|m| matches!(m, Message::ToolResult(_))));
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            let label = match event {
                AgentEvent::AgentStart { .. } => "agent_start",
                AgentEvent::AgentEnd { .. } => "agent_end",
                AgentEvent::TurnStart { .. } => "turn_start",
                AgentEvent::TurnEnd { .. } => "turn_end",
                AgentEvent::MessageStart { .. } => "message_start",
                AgentEvent::MessageEnd { .. } => "message_end",
                AgentEvent::StreamStart { .. } => "stream_start",
                AgentEvent::StreamEnd { .. } => "stream_end",
                AgentEvent::ToolExecutionStart { .. } => "tool_start",
                AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
                AgentEvent::ToolExecutionEnd { .. } => "tool_end",
                AgentEvent::StreamEvent { .. } => "stream_event",
                AgentEvent::Compaction { .. } => "compaction",
            };
            events_for_handler.lock().unwrap().push(label);
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.turns, 2);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.terminal.content, vec![text("final")]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let events = events.lock().unwrap();
    assert_eq!(events.first(), Some(&"agent_start"));
    assert_eq!(events.last(), Some(&"agent_end"));
    assert!(events.contains(&"tool_start"));
    assert!(events.contains(&"tool_end"));
}

#[tokio::test]
async fn stream_events_and_compaction_and_steering_are_consumed() {
    let turn = Arc::new(AtomicUsize::new(0));
    let turn_for_stream = turn.clone();
    let compacted = Arc::new(AtomicUsize::new(0));
    let compacted_for_hook = compacted.clone();
    let steered = Arc::new(AtomicUsize::new(0));
    let steered_for_hook = steered.clone();
    let stream: EventStreamFn = Box::new(move |request| {
        let n = turn_for_stream.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert!(request.context.system_prompt.as_deref() == Some("compacted"));
            let stream = AssistantMessageEventStream::new();
            let message = assistant(
                vec![text(if n == 0 { "first" } else { "second" })],
                StopReason::Stop,
            );
            stream.push(AssistantMessageEvent::Start {
                partial: Arc::new(message.clone()),
            });
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    });
    let options = LoopOptions {
        event_stream: Some(stream),
        compaction: Some(Box::new(move |mut context| {
            compacted_for_hook.fetch_add(1, Ordering::SeqCst);
            // A hook that compacts hands back a REBUILT context — a new `Arc`
            // (`CompactionFn` identity semantics; the loop's "did it compact?"
            // check is `Arc::ptr_eq`).
            Arc::make_mut(&mut context).system_prompt = Some("compacted".into());
            Box::pin(async move { context })
        })),
        steering: Some(Box::new(move || {
            let count = steered_for_hook.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if count == 0 {
                    vec!["steer onward".into()]
                } else {
                    Vec::new()
                }
            })
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(LoopDeps::default(), Context::default(), None, options).await;
    // Steering is now consumed BEFORE the first model request (upstream
    // agent-loop.ts:174 / #9548 timing): the queued message lands on turn 1,
    // so the run completes in a single turn.
    assert_eq!(result.turns, 1);
    assert_eq!(result.terminal.content, vec![text("first")]);
    assert_eq!(compacted.load(Ordering::SeqCst), 1);
    // Called once at turn start (delivered) + once at the terminal poll (empty).
    assert_eq!(steered.load(Ordering::SeqCst), 2);
}

struct RegistryProvider {
    calls: AtomicUsize,
    api_key: Arc<Mutex<Option<String>>>,
    model: Model,
}

impl Provider for RegistryProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn name(&self) -> &str {
        "Test provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("test key", vec!["TEST_AGENT_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.api_key.lock().unwrap() = options.api_key;
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant(vec![text("real provider result")], StopReason::Stop),
        ));
        stream
    }
}

fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test model".into(),
        api: "test-api".into(),
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

#[tokio::test]
async fn loop_can_use_real_provider_registry_and_returns_provider_message() {
    let key = Arc::new(Mutex::new(None));
    let provider = Arc::new(RegistryProvider {
        calls: AtomicUsize::new(0),
        api_key: key.clone(),
        model: test_model(),
    });
    let auth = Arc::new(MapAuthContext::new(BTreeMap::from([(
        "TEST_AGENT_KEY".into(),
        "secret".into(),
    )])));
    let registry = Arc::new(ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        auth,
    ));
    let mut registry_owned = (*registry).clone();
    registry_owned.set_provider(provider.clone());
    let registry = Arc::new(registry_owned);

    let deps = LoopDeps::default().with_provider(registry, test_model(), StreamOptions::default());
    let result = run(deps, Context::default(), None).await;
    assert_eq!(result.terminal.content, vec![text("real provider result")]);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(key.lock().unwrap().as_deref(), Some("secret"));
}

// ---------------------------------------------------------------------------
// durationMs: the loop keeps the value the provider stream already stamped
// ---------------------------------------------------------------------------

/// Registry provider whose terminal message starts inside the provider stream
/// (a fresh timestamp), may pre-carry `durationMs`, and takes measurable time
/// so an overwrite by the agent loop would be visible.
struct TimedRegistryProvider {
    model: Model,
    timestamp: Option<u64>,
    duration_ms: Option<u64>,
    delay: std::time::Duration,
}

impl Provider for TimedRegistryProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn name(&self) -> &str {
        "Timed test provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("test key", vec!["TEST_AGENT_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        std::thread::sleep(self.delay);
        let mut message = assistant(vec![text("timed result")], StopReason::Stop);
        message.timestamp = self.timestamp.unwrap_or_else(pir::ai::util::now_ms);
        message.duration_ms = self.duration_ms;
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn timed_provider_registry(
    timestamp: Option<u64>,
    duration_ms: Option<u64>,
    delay: std::time::Duration,
) -> Arc<ProviderRegistry> {
    let provider = Arc::new(TimedRegistryProvider {
        model: test_model(),
        timestamp,
        duration_ms,
        delay,
    });
    let auth = Arc::new(MapAuthContext::new(BTreeMap::from([(
        "TEST_AGENT_KEY".into(),
        "secret".into(),
    )])));
    let mut registry = ProviderRegistry::new(Arc::new(InMemoryCredentialStore::new()), auth);
    registry.set_provider(provider);
    Arc::new(registry)
}

/// A stream-backed turn (`deps.stream` consumes the registry's terminal event)
/// carries the provider-stamped `durationMs`, the same field a direct
/// `ProviderRegistry::complete` caller gets.
#[tokio::test]
async fn loop_keeps_the_stream_stamped_duration_ms() {
    let registry = timed_provider_registry(None, None, std::time::Duration::from_millis(10));
    let deps = LoopDeps::default().with_provider(
        Arc::clone(&registry),
        test_model(),
        StreamOptions::default(),
    );
    let result = run(deps, Context::default(), None).await;
    let loop_duration = result
        .terminal
        .duration_ms
        .expect("the loop terminal carries durationMs");
    assert!(
        loop_duration >= 10,
        "durationMs covers the response: got {loop_duration}"
    );

    // Direct registry path: same terminal shape (present, monotonic ms).
    let direct = registry
        .complete(test_model(), Context::default(), StreamOptions::default())
        .await
        .expect("complete");
    assert!(
        direct.duration_ms.is_some_and(|duration| duration >= 10),
        "direct registry call is timed too: {:?}",
        direct.duration_ms
    );
    assert_eq!(direct.content, result.terminal.content);
}

/// The loop must not overwrite a duration the message already carries: the
/// provider stream (or a forwarding wrapper) owns the measured value.
#[tokio::test]
async fn loop_does_not_overwrite_a_pre_stamped_duration_ms() {
    let registry = timed_provider_registry(None, Some(7777), std::time::Duration::from_millis(20));
    let deps = LoopDeps::default().with_provider(registry, test_model(), StreamOptions::default());
    let result = run(deps, Context::default(), None).await;
    assert_eq!(result.terminal.duration_ms, Some(7777));
}

/// The production path uses `LoopOptions::event_stream`; it must keep the
/// stream's value exactly like the `deps.stream` path above.
#[tokio::test]
async fn event_stream_backed_turn_keeps_the_stream_duration_ms() {
    let registry = timed_provider_registry(None, None, std::time::Duration::from_millis(10));
    let event_registry = Arc::clone(&registry);
    let base_model = test_model();
    let event_stream: EventStreamFn = Box::new(move |request| {
        let registry = Arc::clone(&event_registry);
        let model = request.model.unwrap_or_else(|| base_model.clone());
        Box::pin(async move {
            registry.stream_resolving_auth(model, request.context, StreamOptions::default())
        })
    });
    let deps = LoopDeps {
        model: Some(test_model()),
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        event_stream: Some(event_stream),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert!(
        result
            .terminal
            .duration_ms
            .is_some_and(|duration| duration >= 10),
        "event-stream path carries durationMs: {:?}",
        result.terminal.duration_ms
    );
}

// ---------------------------------------------------------------------------
// Regression: LoopResult::new_messages (P0-1)
// ---------------------------------------------------------------------------

/// A multi-turn run must return EVERY produced message, the intermediate
/// assistant(toolCall) message included, in append order: not only the
/// terminal assistant message plus tool results. Upstream persists every
/// `message_end` during the run (agent-session.ts:1144-1163).
#[tokio::test]
async fn multi_turn_run_returns_all_produced_messages() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 2);
    // The full produced-message list: declaration (declareToolChanges) ->
    // assistant(toolCall) -> toolResult -> assistant(final).
    assert_eq!(result.new_messages.len(), 4);
    assert!(matches!(result.new_messages[0], Message::System(_)));
    assert!(matches!(
        result.new_messages[1],
        Message::Assistant(ref message)
            if message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    ));
    assert!(matches!(
        result.new_messages[2],
        Message::ToolResult(ref result) if result.tool_call_id == "call-1"
    ));
    assert!(matches!(
        result.new_messages[3],
        Message::Assistant(ref message) if message.content == vec![text("final")]
    ));
}

// ---------------------------------------------------------------------------
// Regression: length stop with tool calls continues (P1-2) and the failed
// calls get lifecycle events (P2-8)
// ---------------------------------------------------------------------------

/// A `length` stop with tool calls fails every call (truncated arguments are
/// unsafe to execute) but never terminates the run: the model must see the
/// error results and can re-issue (upstream hasMoreToolCalls=true,
/// agent-loop.ts:264-272). Each failed call emits the same per-call lifecycle
/// events a real execution emits (upstream `failToolCallsFromTruncatedMessage`,
/// agent-loop.ts:478-503).
#[tokio::test]
async fn length_stop_with_tool_calls_fails_calls_and_continues() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::Length)
                } else {
                    assistant(vec![text("recovered")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;
    // The length turn did NOT end the run: the loop continued and the model
    // produced the final answer.
    assert_eq!(result.turns, 2);
    assert_eq!(result.terminal.content, vec![text("recovered")]);
    // The failed call is in the transcript as an error tool result.
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].is_error);
    assert!(matches!(
        result.new_messages[2],
        Message::ToolResult(ref result) if result.is_error
    ));
    // The text is upstream's `failToolCallsFromTruncatedMessage`
    // (agent-loop.ts:489-492): it says why and what to do — re-issue with
    // complete arguments.
    let failed_text = match &result.new_messages[2] {
        Message::ToolResult(ref message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<String>(),
        other => panic!("expected a tool result, got {other:?}"),
    };
    assert_eq!(
        failed_text,
        "Tool call \"echo\" was not executed: the response hit the output token limit, \
         so its arguments may be truncated. Re-issue the tool call with complete arguments."
    );

    // Lifecycle events for the failed call: tool_execution_start and
    // tool_execution_end were both emitted.
    let events = events.lock().unwrap();
    let starts = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
        .count();
    let ends = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
        .count();
    assert_eq!(starts, 1, "missing tool_execution_start: {events:?}");
    assert_eq!(ends, 1, "missing tool_execution_end: {events:?}");
}

/// An explicit `finish_turn` `End` decision still terminates a length turn.
#[tokio::test]
async fn length_stop_honors_explicit_end_decision() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::Length)
                } else {
                    assistant(vec![text("unreachable")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(ToolRegistry::new()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        finish_turn: Some(Arc::new(|_context| {
            Box::pin(async { Some(FinishTurnAction::End) })
        })),
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 1);
    assert_eq!(result.terminal.stop_reason, StopReason::Length);
}

// ---------------------------------------------------------------------------
// Regression: preflight failures emit lifecycle events (P2-9)
// ---------------------------------------------------------------------------

/// Upstream emits `tool_execution_start` for every call before preparation
/// (agent-loop.ts:541-547,597-602); a failed preparation emits an immediate
/// `tool_execution_end` for the error result. A call for an unregistered tool
/// fails preflight and must still produce both events.
#[tokio::test]
async fn preflight_failure_emits_start_and_end_events() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    // The registry has no "missing" tool: preflight fails.
                    assistant(
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "call-missing".into(),
                            name: "missing".into(),
                            arguments: BTreeMap::new(),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        })],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(ToolRegistry::new()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.turns, 2);
    assert!(result.tool_results[0].is_error);
    let events = events.lock().unwrap();
    let mut starts = 0;
    let mut ends = 0;
    for event in events.iter() {
        match event {
            AgentEvent::ToolExecutionStart { event, .. } => {
                starts += 1;
                assert!(matches!(
                    event,
                    ToolExecutionEvent::Start { ref call_id, .. }
                        if call_id == "call-missing"
                ));
            }
            AgentEvent::ToolExecutionEnd { event, .. } => {
                ends += 1;
                assert!(matches!(
                    event,
                    ToolExecutionEvent::End { is_error: true, .. }
                ));
            }
            _ => {}
        }
    }
    assert_eq!(starts, 1, "expected one start event: {events:?}");
    assert_eq!(ends, 1, "expected one end event: {events:?}");
}

// ---------------------------------------------------------------------------
// Regression: StreamEnd emitted once per turn (P2-6)
// ---------------------------------------------------------------------------

/// Only the post-finalization `StreamEnd` is emitted: the stream lifecycle
/// path and the pre-finalization path both lost their duplicate emission.
#[tokio::test]
async fn stream_end_is_emitted_exactly_once_per_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let stream: EventStreamFn = Box::new(move |_request| {
        let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            // Turn 0 issues a tool call so the run takes two turns.
            let message = if n == 0 {
                assistant(vec![tool_call()], StopReason::ToolUse)
            } else {
                assistant(vec![text("second")], StopReason::Stop)
            };
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEvent::Start {
                partial: Arc::new(message.clone()),
            });
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        event_stream: Some(stream),
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(
        deps_for_two_turn_test(tools),
        Context::default(),
        None,
        options,
    )
    .await;
    assert_eq!(result.turns, 2);
    let stream_ends = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, AgentEvent::StreamEnd { .. }))
        .count();
    assert_eq!(stream_ends, 2, "one StreamEnd per turn expected");
}

fn deps_for_two_turn_test(tools: ToolRegistry) -> LoopDeps {
    LoopDeps {
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    }
}

// ---------------------------------------------------------------------------
// Lazy `tool_execution_start.args` for observer-free runs
// ---------------------------------------------------------------------------

/// The start frame's args are materialized only when a subscriber exists.
/// With no host event handler and no extension handler the emitter must not
/// touch the call's arguments at all (zero construction/clone); with a handler
/// the frame keeps the same `args` as before — the gate's post-rewrite input
/// when one was recorded, otherwise the assistant message's call arguments.
#[tokio::test]
async fn tool_progress_args_are_materialized_only_for_subscribers() {
    use pir::agent::agent_loop::{emit_tool_progress_for_test, EventHandler};

    let arguments = BTreeMap::from([
        ("count".to_owned(), serde_json::json!(3)),
        ("path".to_owned(), serde_json::json!("notes.md")),
    ]);
    let message = assistant(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call-args".into(),
            name: "echo".into(),
            arguments: arguments.clone(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        StopReason::ToolUse,
    );
    let start = || ToolExecutionEvent::Start {
        index: 0,
        call_id: "call-args".to_owned(),
        tool_name: "echo".to_owned(),
        parent_call_id: None,
    };
    let inputs = std::collections::HashMap::new();

    // No host handler, no extension host: the args are never constructed.
    let materialized = emit_tool_progress_for_test(
        &None,
        &LoopDeps::default(),
        1,
        &inputs,
        &message,
        start(),
        None,
    )
    .await;
    assert!(
        !materialized,
        "an observer-free run must not construct/clone the call's args"
    );

    // A host subscriber receives the assistant message's fallback args.
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let handler: EventHandler = Arc::new(move |event| {
        events_for_handler.lock().unwrap().push(event);
    });
    let materialized = emit_tool_progress_for_test(
        &Some(handler),
        &LoopDeps::default(),
        2,
        &inputs,
        &message,
        start(),
        None,
    )
    .await;
    assert!(materialized, "a subscriber needs the args");
    {
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1, "one host frame");
        match &captured[0] {
            AgentEvent::ToolExecutionStart { turn, event, args } => {
                assert_eq!(*turn, 2);
                assert!(matches!(
                    event,
                    ToolExecutionEvent::Start { call_id, .. } if call_id == "call-args"
                ));
                assert_eq!(
                    args,
                    &serde_json::json!({ "count": 3, "path": "notes.md" }),
                    "the start frame carries the call's arguments"
                );
            }
            other => panic!("expected a tool start event, got {other:?}"),
        }
    }

    // A `tool_call` gate's post-rewrite input wins over the message fallback,
    // and both faces observe the same rewritten args.
    let rewritten = std::collections::HashMap::from([(
        "call-args".to_owned(),
        serde_json::json!({ "path": "rewritten.md" }),
    )]);
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let handler: EventHandler = Arc::new(move |event| {
        events_for_handler.lock().unwrap().push(event);
    });
    let materialized = emit_tool_progress_for_test(
        &Some(handler),
        &LoopDeps::default(),
        3,
        &rewritten,
        &message,
        start(),
        None,
    )
    .await;
    assert!(materialized);
    let captured = events.lock().unwrap();
    match &captured[0] {
        AgentEvent::ToolExecutionStart { args, .. } => assert_eq!(
            args,
            &serde_json::json!({ "path": "rewritten.md" }),
            "the gate's rewritten args are what the frame carries"
        ),
        other => panic!("expected a tool start event, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Regression: Compaction event only when compaction actually runs (P2-12)
// ---------------------------------------------------------------------------

/// The `compaction` event fires only when the hook actually changed the
/// context: a hook that returns the context unchanged (threshold unmet,
/// cancelled, or failed compaction) emits nothing.
#[tokio::test]
async fn compaction_event_fires_only_when_context_changes() {
    let turn = Arc::new(AtomicUsize::new(0));
    let turn_for_stream = turn.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let stream: EventStreamFn = Box::new(move |_request| {
        let n = turn_for_stream.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            // Turn 0 issues a tool call so the run takes two turns.
            let message = if n == 0 {
                assistant(vec![tool_call()], StopReason::ToolUse)
            } else {
                assistant(vec![text("second")], StopReason::Stop)
            };
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        event_stream: Some(stream),
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        // Compacts on the first turn only; the second turn returns the SAME
        // shared context (identity), so the loop's `Arc::ptr_eq` check reports
        // no compaction.
        compaction: Some(Box::new(move |mut context| {
            Box::pin(async move {
                if context.system_prompt.is_none() {
                    Arc::make_mut(&mut context).system_prompt = Some("compacted".into());
                }
                context
            })
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(
        deps_for_two_turn_test(tools),
        Context::default(),
        None,
        options,
    )
    .await;
    assert_eq!(result.turns, 2);
    let compactions = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, AgentEvent::Compaction { .. }))
        .count();
    assert_eq!(compactions, 1);
}

// ---------------------------------------------------------------------------
// Regression: nested-call usage fold (P2-11)
// ---------------------------------------------------------------------------

/// The tool-result message usage is the `tool_result` handler-reported usage
/// combined fieldwise with the nested calls' recorded usage (upstream
/// `combineUsage`, agent-session.ts:1110-1112).
#[test]
fn nested_usage_combines_fieldwise() {
    let handler = Usage {
        input: 10,
        output: 5,
        cache_read: 2,
        total_tokens: 15,
        ..Default::default()
    };
    let nested = Usage {
        input: 3,
        output: 1,
        total_tokens: 4,
        ..Default::default()
    };
    let combined = pir::agent::nested_tool_calls::combine_usage(handler, nested);
    assert_eq!(combined.input, 13);
    assert_eq!(combined.output, 6);
    assert_eq!(combined.cache_read, 2);
    assert_eq!(combined.total_tokens, 19);
}

// ---------------------------------------------------------------------------
// declareToolChanges / message_start / max_turns / steering-poll timing
// ---------------------------------------------------------------------------

fn echo_tool_definition() -> Tool {
    Tool {
        name: "echo".into(),
        description: "echoes".into(),
        parameters: serde_json::json!({"type": "object"}),
        constrained_sampling: None,
    }
}

/// declareToolChanges (upstream agent-loop.ts:333-375): the first request
/// injects a system message declaring the loadout as `toolsAdded`; a request
/// whose set is unchanged injects nothing.
#[tokio::test]
async fn tool_loadout_changes_are_declared_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run(deps, Context::default(), None).await;
    let declarations: Vec<&Message> = result
        .new_messages
        .iter()
        .filter(|message| matches!(message, Message::System(_)))
        .collect();
    assert_eq!(
        declarations.len(),
        1,
        "one declaration for the changed loadout, none for the unchanged second request: {:?}",
        declarations
    );
    let Message::System(system) = declarations[0] else {
        panic!("expected a system declaration message");
    };
    let added = system
        .tools_added
        .as_ref()
        .expect("toolsAdded on the declaration");
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].name, "echo");
    assert!(system.tools_removed.is_none());
}

/// Replay safety: a transcript that already declares the same loadout (a
/// resumed session) does not re-declare it.
#[tokio::test]
async fn seeded_declaration_is_not_re_declared() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let context = Context {
        messages: vec![Message::System(UserMessage {
            role: UserRole::System,
            content: Vec::new(),
            tools_added: Some(vec![echo_tool_definition()]),
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        ..Context::default()
    };
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async { assistant(vec![text("final")], StopReason::Stop) })
        }),
        tools: Arc::new(tools),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run(deps, context, None).await;
    assert!(
        !result
            .new_messages
            .iter()
            .any(|message| matches!(message, Message::System(_))),
        "an unchanged declared loadout must not be re-declared: {:?}",
        result.new_messages
    );
}

/// message_start (upstream agent-loop.ts:416-421): emitted at stream start
/// with the stream's first partial message; with no stream events, emitted
/// with the empty message before the final one.
#[tokio::test]
async fn message_start_is_emitted_at_stream_start() {
    // Event-stream path: the Start partial becomes the message_start payload.
    let partial = assistant(vec![text("streamed so far")], StopReason::Stop);
    let expected = partial.content.clone();
    let stream: EventStreamFn = Box::new(|_request| {
        Box::pin(async {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEvent::Start {
                partial: Arc::new(assistant(vec![text("streamed so far")], StopReason::Stop)),
            });
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text("done")], StopReason::Stop),
            ));
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        event_stream: Some(stream),
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };
    run_with_options(LoopDeps::default(), Context::default(), None, options).await;
    let starts: Vec<AssistantMessage> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::StreamStart { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "one message_start per request");
    assert_eq!(starts[0].content, expected);

    // Plain-stream path: message_start with the empty message still fires.
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async { assistant(vec![text("final")], StopReason::Stop) })
        }),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };
    run_with_options(deps, Context::default(), None, options).await;
    let starts = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, AgentEvent::StreamStart { .. }))
        .count();
    assert_eq!(starts, 1, "plain streams still emit message_start");
}

/// The upstream loop is unbounded: the default max_turns is usize::MAX.
#[test]
fn max_turns_default_is_unbounded() {
    assert_eq!(LoopDeps::default().max_turns, usize::MAX);
}

/// Steering queued DURING compaction is polled in the SAME turn: the poll
/// runs after the compaction hook (upstream agent-loop.ts:201-206), so the
/// message rides the very next request instead of arriving one turn late.
#[tokio::test]
async fn steering_queued_during_compaction_is_polled_same_turn() {
    let queue = Arc::new(Mutex::new(Vec::new()));
    let queue_for_compaction = queue.clone();
    let queue_for_steering = queue.clone();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_stream = observed.clone();
    let compacted = Arc::new(AtomicUsize::new(0));
    let compacted_for_hook = compacted.clone();
    let stream: EventStreamFn = Box::new(move |request| {
        let observed = observed_for_stream.clone();
        Box::pin(async move {
            for message in &request.context.messages {
                if let Message::User(user) = message {
                    let text = user
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<String>();
                    observed.lock().unwrap().push(text);
                }
            }
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text("done")], StopReason::Stop),
            ));
            stream
        })
    });
    let options = LoopOptions {
        event_stream: Some(stream),
        compaction: Some(Box::new(move |mut context| {
            compacted_for_hook.fetch_add(1, Ordering::SeqCst);
            // Queue a steering message while compaction runs.
            queue_for_compaction
                .lock()
                .unwrap()
                .push("queued during compaction".to_owned());
            Arc::make_mut(&mut context).system_prompt = Some("compacted".into());
            Box::pin(async move { context })
        })),
        steering: Some(Box::new(move || {
            let queue = queue_for_steering.clone();
            Box::pin(async move {
                queue
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(PromptMessage::text)
                    .collect::<Vec<_>>()
            })
        })),
        ..LoopOptions::default()
    };
    let _result = run_with_options(LoopDeps::default(), Context::default(), None, options).await;
    assert_eq!(compacted.load(Ordering::SeqCst), 1);
    let seen = observed.lock().unwrap();
    assert!(
        seen.iter().any(|text| text == "queued during compaction"),
        "message queued during compaction must ride the SAME turn's request: {seen:?}"
    );
}

/// Per-message persistence sink: the loop invokes it with every message it
/// appends (declaration, assistant, tool results), in append order.
#[tokio::test]
async fn persist_sink_receives_every_appended_message() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let persisted_for_sink = persisted.clone();
    let sink: PersistMessageFn = Arc::new(move |message: Message| {
        let persisted = persisted_for_sink.clone();
        Box::pin(async move {
            persisted.lock().unwrap().push(message);
        })
    });
    let options = LoopOptions {
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    let persisted = persisted.lock().unwrap();
    assert_eq!(
        persisted.len(),
        result.new_messages.len(),
        "the sink sees exactly the messages the run appended"
    );
    // Assistant and tool-result messages are both covered.
    assert!(persisted
        .iter()
        .any(|message| matches!(message, Message::ToolResult(_))));
    assert!(persisted
        .iter()
        .any(|message| matches!(message, Message::Assistant(_))));
}

// ---------------------------------------------------------------------------
// `message_update` dispatch is not on the stream's critical path (2026-10-15)
// ---------------------------------------------------------------------------

/// The `message_update` payload built for the extension bridge keeps exactly
/// the values the previous construction produced (`serde_json::to_value(event)`
/// for `assistantMessageEvent`, the message payload for `message`), while
/// reusing the event's own serialized snapshot instead of deep-cloning the
/// message a second time.
#[test]
fn message_update_payload_matches_the_previous_construction() {
    let partial = assistant(vec![text("accumulated")], StopReason::Stop);
    let event = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "chunk".into(),
        partial: Arc::new(partial.clone()),
    };
    let payload = pir::agent::agent_loop::message_update_payload(&event);
    assert_eq!(payload["type"], serde_json::json!("message_update"));
    assert_eq!(
        payload["assistantMessageEvent"],
        serde_json::to_value(&event).expect("event serializes")
    );
    assert_eq!(
        payload["message"],
        serde_json::to_value(Message::Assistant(partial.clone())).expect("message serializes")
    );

    // Terminal events carry no `partial`: the role-only message shape stays.
    let done = AssistantMessageEventStream::done_event(TerminalReason::Stop, partial);
    let payload = pir::agent::agent_loop::message_update_payload(&done);
    assert_eq!(
        payload["message"],
        serde_json::json!({ "role": "assistant" })
    );
    assert_eq!(
        payload["assistantMessageEvent"],
        serde_json::to_value(&done).expect("event serializes")
    );
}

fn stream_message(value: &str) -> AssistantMessage {
    assistant(vec![text(value)], StopReason::Stop)
}

/// With a `message_update` handler installed, the loop must not await a
/// QuickJS round trip per delta: the Rust-side event consumers see each delta
/// as the provider stream produces it, while the JS dispatches catch up in
/// order. The handler here sleeps per update, so the old gating behaviour
/// delayed every Rust-side delta by the full backlog of JS sleeps.
#[tokio::test]
async fn message_update_dispatch_does_not_gate_the_stream() {
    const TOKENS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
    const HANDLER_SLEEP_MS: u64 = 120;
    const PUSH_GAP_MS: u64 = 10;
    /// Loaded-box tolerance for one delta's delivery lag.
    ///
    /// The invariant under test is the GATING signature, which is cumulative:
    /// a loop that awaited a QuickJS round trip per delta would deliver the
    /// last delta only after the handler backlog drains
    /// (`TOKENS.len() * HANDLER_SLEEP_MS` ≈ 960 ms, minus the ~80 ms of stream
    /// pushes), so `worst_lag` there is ~880 ms. Un-gated delivery normally
    /// lags by one push gap plus a wakeup (10-20 ms), but this suite also
    /// shares the box with every other lane: a wave-18 run recorded a single
    /// 96.872 ms wakeup under load. 200 ms is ~2x that worst observation and
    /// still ~4x below the gated signature, so it cannot mask the regression
    /// the test exists for (a return to awaiting dispatch per delta).
    const MAX_DELTA_LAG_MS: u64 = 200;

    let temp = std::env::temp_dir().join(format!(
        "pir-loop-message-update-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let path = temp.join("ext.ts");
    let source = r#"
        export default function (pi) {
            const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
            globalThis.__seen = [];
            pi.on("message_start", (event) => {
                globalThis.__seen.push({ kind: "message_start", role: event.message ? event.message.role : null });
            });
            pi.on("message_update", async (event) => {
                await sleep(SLEEP_MS_PLACEHOLDER);
                const assistantEvent = event.assistantMessageEvent;
                if (assistantEvent && assistantEvent.type === "text_delta") {
                    const block = assistantEvent.partial && assistantEvent.partial.content
                        ? assistantEvent.partial.content[0]
                        : null;
                    globalThis.__seen.push({
                        kind: "update",
                        delta: assistantEvent.delta,
                        partialText: block ? block.text : null,
                        messageRole: event.message ? event.message.role : null,
                    });
                }
            });
            pi.on("message_end", (event) => {
                globalThis.__seen.push({ kind: "message_end", role: event.message ? event.message.role : null });
            });
        }
        "#
    .replace("SLEEP_MS_PLACEHOLDER", &HANDLER_SLEEP_MS.to_string());
    std::fs::write(&path, source).expect("write fixture");
    let runtime = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: temp.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    // Provider stream: one delta every PUSH_GAP_MS, pushed from a task so the
    // loop's consumption has to interleave with real arrival times.
    let pushed = Arc::new(Mutex::new(Vec::<std::time::Instant>::new()));
    let pushed_for_stream = pushed.clone();
    let stream: EventStreamFn = Box::new(move |_request| {
        let pushed_for_stream = pushed_for_stream.clone();
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            let sink = stream.clone();
            tokio::spawn(async move {
                sink.push(AssistantMessageEvent::Start {
                    partial: Arc::new(stream_message("")),
                });
                sink.push(AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: Arc::new(stream_message("")),
                });
                let mut accumulated = String::new();
                for token in TOKENS {
                    tokio::time::sleep(std::time::Duration::from_millis(PUSH_GAP_MS)).await;
                    accumulated.push_str(token);
                    pushed_for_stream
                        .lock()
                        .unwrap()
                        .push(std::time::Instant::now());
                    sink.push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: token.to_string(),
                        partial: Arc::new(stream_message(&accumulated)),
                    });
                }
                sink.push(AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: accumulated.clone(),
                    partial: Arc::new(stream_message(&accumulated)),
                });
                sink.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    stream_message(&accumulated),
                ));
            });
            stream
        })
    });

    let delivered = Arc::new(Mutex::new(Vec::<std::time::Instant>::new()));
    let delivered_for_handler = delivered.clone();
    let options = LoopOptions {
        event_stream: Some(stream),
        events: Some(Arc::new(move |event| {
            if let AgentEvent::StreamEvent {
                event: AssistantMessageEvent::TextDelta { .. },
                ..
            } = &event
            {
                delivered_for_handler
                    .lock()
                    .unwrap()
                    .push(std::time::Instant::now());
            }
        })),
        ..LoopOptions::default()
    };
    let deps = LoopDeps {
        extensions: Some(runtime.clone()),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);

    let pushed_at = pushed.lock().unwrap().clone();
    let delivered_at = delivered.lock().unwrap().clone();
    assert_eq!(pushed_at.len(), TOKENS.len(), "all deltas pushed");
    assert_eq!(
        delivered_at.len(),
        TOKENS.len(),
        "the Rust-side consumer observes every delta"
    );
    let worst_lag = delivered_at
        .iter()
        .zip(&pushed_at)
        .map(|(delivered, pushed)| delivered.saturating_duration_since(*pushed))
        .max()
        .expect("at least one delta");
    assert!(
        worst_lag < std::time::Duration::from_millis(MAX_DELTA_LAG_MS),
        "delta delivery must not wait for the JS round trips (worst lag {worst_lag:?}, \
         budget {MAX_DELTA_LAG_MS} ms; gated delivery accumulates ~880 ms)"
    );

    // JS-visible semantics are unchanged: every update dispatched exactly
    // once, in stream order, each carrying its own cumulative snapshot (no
    // coalescing, nothing dropped, no stale/latest substitution), and the
    // stream's own `message_start` / `message_end` still bracket the deltas
    // (the backlog is drained before any other extension event).
    let seen = runtime
        .eval_json("JSON.stringify(globalThis.__seen)")
        .await
        .expect("probe");
    let seen = seen.as_array().expect("seen array");
    assert_eq!(
        seen.len(),
        TOKENS.len() + 2,
        "message_start + one dispatch per delta + message_end: {seen:?}"
    );
    assert_eq!(seen[0]["kind"].as_str(), Some("message_start"));
    assert_eq!(seen[0]["role"].as_str(), Some("assistant"));
    assert_eq!(
        seen[seen.len() - 1]["kind"].as_str(),
        Some("message_end"),
        "message_end must be dispatched after every message_update: {seen:?}"
    );
    assert_eq!(seen[seen.len() - 1]["role"].as_str(), Some("assistant"));
    let mut accumulated = String::new();
    for (index, entry) in seen[1..seen.len() - 1].iter().enumerate() {
        assert_eq!(entry["kind"].as_str(), Some("update"), "entry {index}");
        accumulated.push_str(TOKENS[index]);
        assert_eq!(entry["delta"].as_str(), Some(TOKENS[index]));
        assert_eq!(
            entry["partialText"].as_str(),
            Some(accumulated.as_str()),
            "update {index} must carry the snapshot accumulated up to it"
        );
        assert_eq!(entry["messageRole"].as_str(), Some("assistant"));
    }

    let _ = std::fs::remove_dir_all(&temp);
}

// ---------------------------------------------------------------------------
// JS `message_end` return value is applied (upstream `emitMessageEnd` parity)
// ---------------------------------------------------------------------------
//
// Upstream `ExtensionRunner.emitMessageEnd` (runner.ts:1151-1190) chains every
// `message_end` handler — each sees the previous handler's replacement, a
// replacement must keep the role, the last one wins — and
// `agent-session.ts:1318-1336` copies the result back onto the live message
// object BEFORE persistence. `pir`'s loop applies the same chained replacement
// (`agent_loop::emit_message_end`): the transcript, the run's produced
// messages, the persistence sink, the terminal, the next provider request and
// the tool scheduler's `validate_arguments` preflight all observe it. An
// empty return (`undefined`, `{message: null}`) leaves the message untouched.
//
// The fixtures live in per-test temp dirs (the repo hygiene gate forbids
// tracked `.ts` files), the same shape `message_update_dispatch_does_not_gate_
// the_stream` uses above.

/// A loaded extension runtime plus the temp dir holding its fixture source.
struct MessageEndFixture {
    runtime: pir::extensions::ExtensionHandle,
    dir: std::path::PathBuf,
}

impl Drop for MessageEndFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write `source` to a temp dir and load it as an inline extension.
async fn message_end_fixture(label: &str, source: &str) -> MessageEndFixture {
    let dir = std::env::temp_dir().join(format!(
        "pir-loop-message-end-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: dir.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    MessageEndFixture { runtime, dir }
}

/// A persistence sink recording every message the loop hands it.
fn recording_sink() -> (PersistMessageFn, Arc<Mutex<Vec<Message>>>) {
    let persisted: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let persisted_for_sink = Arc::clone(&persisted);
    let sink: PersistMessageFn = Arc::new(move |message: Message| {
        let persisted = Arc::clone(&persisted_for_sink);
        Box::pin(async move {
            persisted.lock().unwrap().push(message);
        })
    });
    (sink, persisted)
}

/// The first assistant message in `messages`, cloned.
fn first_assistant(messages: &[Message]) -> AssistantMessage {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.clone()),
            _ => None,
        })
        .expect("an assistant message")
}

/// ④ + ①: a JS `message_end` handler rewrites a tool call's `arguments`; the
/// scheduler's `validate_arguments` preflight runs on the MODIFIED arguments,
/// so a call whose raw arguments violate the tool schema executes anyway with
/// the repaired payload, and the persisted transcript carries the repair.
#[tokio::test]
async fn js_message_end_arguments_are_what_validation_and_the_transcript_see() {
    let fixture = message_end_fixture(
        "args",
        r#"
        export default function (pi) {
            globalThis.__fixed = 0;
            pi.on("message_end", (event) => {
                if (!event.message || event.message.role !== "assistant") return;
                const content = Array.isArray(event.message.content) ? event.message.content : [];
                let changed = false;
                const next = content.map((block) => {
                    if (block.type !== "toolCall" || block.name !== "probe") return block;
                    if (!block.arguments || typeof block.arguments.path === "string") return block;
                    changed = true;
                    return { ...block, arguments: { ...block.arguments, path: "fixed-by-js" } };
                });
                if (!changed) return;
                globalThis.__fixed += 1;
                return { message: { ...event.message, content: next } };
            });
        }
        "#,
    )
    .await;

    let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let (sink, persisted) = recording_sink();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SchemaProbeTool {
        received: Arc::clone(&received),
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "call-probe".into(),
                            name: "probe".into(),
                            arguments: BTreeMap::from([("path".to_owned(), serde_json::json!(7))]),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        })],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(
        result.turns, 2,
        "the failed call must still reach the model"
    );

    // The probe EXECUTED with the repaired arguments: the raw `path: 7`
    // fails the declared schema, so an executed call proves preflight saw the
    // JS replacement (requirement: validation after the `message_end` return).
    let received = received.lock().unwrap().clone();
    assert_eq!(
        received.len(),
        1,
        "the probe ran exactly once: {received:?}"
    );
    assert_eq!(received[0], serde_json::json!({ "path": "fixed-by-js" }));
    let fixed = fixture
        .runtime
        .eval_json("JSON.stringify(globalThis.__fixed)")
        .await
        .expect("probe");
    assert_eq!(fixed, serde_json::json!(1), "one message was repaired");

    // The transcript records the repaired call (persisted at its
    // `message_end`, after the replacement was applied).
    let persisted = persisted.lock().unwrap().clone();
    let tool_call_arguments = persisted
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => {
                assistant.content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall(call) => Some(call.arguments.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("the persisted assistant tool call");
    assert_eq!(
        tool_call_arguments.get("path"),
        Some(&serde_json::json!("fixed-by-js")),
        "the transcript carries the repaired arguments"
    );
    // The call succeeded (not a preflight error result).
    let tool_result_error = persisted.iter().find_map(|message| match message {
        Message::ToolResult(result) => Some(result.is_error),
        _ => None,
    });
    assert_eq!(tool_result_error, Some(false));
}

/// ④ control: without a replacement the same raw arguments fail the preflight
/// and the tool never runs — the unmodified path is byte-for-byte today's
/// behavior.
#[tokio::test]
async fn js_message_end_without_a_replacement_keeps_todays_validation() {
    let fixture = message_end_fixture(
        "args-control",
        r#"
        export default function (pi) {
            globalThis.__calls = 0;
            pi.on("message_end", (event) => {
                // Count PROVIDER assistant messages only: a run stopped at the
                // turn cap later synthesizes one more assistant error terminal.
                if (event.message && event.message.role === "assistant"
                    && event.message.stopReason !== "error") globalThis.__calls += 1;
                return undefined;
            });
            pi.on("message_end", () => ({ message: null }));
        }
        "#,
    )
    .await;

    let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let (sink, persisted) = recording_sink();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SchemaProbeTool {
        received: Arc::clone(&received),
    }));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            Box::pin(async move {
                assistant(
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-probe".into(),
                        name: "probe".into(),
                        arguments: BTreeMap::from([("path".to_owned(), serde_json::json!(7))]),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    StopReason::ToolUse,
                )
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 1,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let _ = run_with_options(deps, Context::default(), None, options).await;

    assert!(
        received.lock().unwrap().is_empty(),
        "the unrepaired call must be rejected before execution"
    );
    let persisted = persisted.lock().unwrap().clone();
    let tool_result_error = persisted.iter().find_map(|message| match message {
        Message::ToolResult(result) => Some(result.is_error),
        _ => None,
    });
    assert_eq!(
        tool_result_error,
        Some(true),
        "the schema violation is the tool result: {persisted:?}"
    );
    // The handlers ran (nothing returned): the untouched message kept its
    // exact bytes — no JS round-trip replacement happened. Exactly ONE
    // assistant `message_end` is dispatched; the cap's synthetic terminal
    // (the scripted stream answers once, so the run cannot continue) is a
    // separate non-assistant message this counter ignores.
    let calls = fixture
        .runtime
        .eval_json("JSON.stringify(globalThis.__calls)")
        .await
        .expect("probe");
    assert_eq!(
        calls,
        serde_json::json!(1),
        "the handlers observed the one provider message"
    );
}

/// ②: a handler that returns nothing, or `{message: null}`, leaves the message
/// bit-identical (the stream's own value, no round-trip canonicalization), and
/// the run is indistinguishable from one with no handlers.
#[tokio::test]
async fn js_message_end_empty_return_leaves_the_message_bit_identical() {
    let fixture = message_end_fixture(
        "noop",
        r#"
        export default function (pi) {
            pi.on("message_end", () => {});
            pi.on("message_end", () => null);
            pi.on("message_end", () => ({ message: null }));
        }
        "#,
    )
    .await;

    let (sink, persisted) = recording_sink();
    let expected = assistant(vec![text("plain")], StopReason::Stop);
    let expected_for_stream = expected.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let expected = expected_for_stream.clone();
            Box::pin(async move { expected })
        }),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 1,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    // The loop stamps `durationMs`/`thinkingLevel` onto the message it
    // finalizes; everything ELSE must be the stream's own value, byte for
    // byte — an applied replacement would carry the JS round trip's shape.
    let mut reference = expected.clone();
    reference.duration_ms = result.terminal.duration_ms;
    reference.thinking_level = result.terminal.thinking_level;
    assert_eq!(
        result.terminal, reference,
        "an empty return must not replace the finalized message"
    );
    let persisted = persisted.lock().unwrap().clone();
    let mut reference = expected;
    reference.duration_ms = first_assistant(&persisted).duration_ms;
    reference.thinking_level = first_assistant(&persisted).thinking_level;
    assert_eq!(
        first_assistant(&persisted),
        reference,
        "the persisted transcript keeps the stream's message"
    );
}

/// ③: the native `message_end_repair` hook (the `tool-repair` builtin's seam)
/// and the JS `message_end` chain form ONE ordered chain over each message —
/// the JS replacement is applied first, the native hook then runs over the
/// replaced message and finds nothing left to change (it returns `None`), so
/// the same repair cannot land twice. The two `tool-repair` FACES themselves
/// never coexist: native-first precedence (asserted here next to the loop
/// chain) drops the JS `pi-tool-repair` entry from the load list.
#[tokio::test]
async fn native_repair_hook_runs_over_the_js_message_end_replacement_once() {
    let fixture = message_end_fixture(
        "chain",
        r#"
        export default function (pi) {
            globalThis.__jsChanged = 0;
            pi.on("message_end", (event) => {
                if (!event.message || event.message.role !== "assistant") return;
                const content = Array.isArray(event.message.content) ? event.message.content : [];
                let changed = false;
                const next = content.map((block) => {
                    if (block.type !== "toolCall" || !block.arguments) return block;
                    if (!Object.prototype.hasOwnProperty.call(block.arguments, "offset")) return block;
                    const { offset, ...rest } = block.arguments;
                    changed = true;
                    return { ...block, arguments: rest };
                });
                if (!changed) return;
                globalThis.__jsChanged += 1;
                return { message: { ...event.message, content: next } };
            });
        }
        "#,
    )
    .await;

    // The native hook is the `tool-repair` builtin's seam
    // (`LoopDeps::message_end_repair`): here it only observes and reports
    // "nothing left to repair" — the JS face already repaired the message.
    let observed = Arc::new(Mutex::new(Vec::<AssistantMessage>::new()));
    let observed_for_hook = Arc::clone(&observed);
    let hook: pir::agent::agent_loop::MessageEndRepairFn = Arc::new(move |context| {
        let observed = Arc::clone(&observed_for_hook);
        Box::pin(async move {
            observed.lock().unwrap().push(context.message);
            None
        })
    });

    let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let (sink, _persisted) = recording_sink();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SchemaProbeTool {
        received: Arc::clone(&received),
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "call-probe".into(),
                            name: "probe".into(),
                            arguments: BTreeMap::from([
                                ("path".to_owned(), serde_json::json!("x")),
                                ("offset".to_owned(), serde_json::Value::Null),
                            ]),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        })],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(fixture.runtime.clone()),
        message_end_repair: Some(hook),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);

    // One invocation per assistant message, and the TOOL-CALL message the
    // hook saw already carried the JS repair — the chain order is JS then
    // native, one pass each.
    let observed = observed.lock().unwrap().clone();
    assert_eq!(
        observed.len(),
        2,
        "one hook invocation per assistant message"
    );
    let tool_call_arguments = observed[0]
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.arguments.clone()),
            _ => None,
        })
        .expect("the hook saw the tool call");
    assert_eq!(
        tool_call_arguments.get("offset"),
        None,
        "the native hook must see the already-repaired arguments: {tool_call_arguments:?}"
    );
    assert_eq!(
        tool_call_arguments.get("path"),
        Some(&serde_json::json!("x"))
    );
    let js_changed = fixture
        .runtime
        .eval_json("JSON.stringify(globalThis.__jsChanged)")
        .await
        .expect("probe");
    assert_eq!(
        js_changed,
        serde_json::json!(1),
        "exactly one message required a repair"
    );
    // The probe executed once with the repaired payload (preflight order).
    let received = received.lock().unwrap().clone();
    assert_eq!(received.len(), 1, "{received:?}");
    assert_eq!(received[0], serde_json::json!({ "path": "x" }));

    // Native-first precedence: with BOTH faces on the plan only the native
    // builtin reaches the load list, so the JS `pi-tool-repair` pipeline never
    // runs beside the builtin's hook.
    use pir::plugins::loader::{partition_paths, tool_repair_precedence, ToolRepairPrecedence};
    assert_eq!(
        tool_repair_precedence(true, true),
        ToolRepairPrecedence::NativeBuiltin
    );
    let (builtins, files) = partition_paths(&[
        std::path::PathBuf::from("builtin:tool-repair"),
        std::path::PathBuf::from("C:/u/.pi/agent/npm/node_modules/pi-tool-repair/tool-repair.ts"),
    ]);
    assert!(builtins.contains(&"tool-repair".to_owned()));
    assert!(
        files.is_empty(),
        "the JS pi-tool-repair entry must be dropped from the load list: {files:?}"
    );
}

// ---------------------------------------------------------------------------
// Lazy, run-scoped active-tool schemas for `message_end_repair`
// ---------------------------------------------------------------------------

/// Counts every `parameters()` read — the observable materialization seam of
/// the loop's lazy [`pir::agent::agent_loop::ActiveToolSchemas`].
struct SchemaAccountingTool {
    parameters_calls: Arc<AtomicUsize>,
}

impl AgentTool for SchemaAccountingTool {
    fn name(&self) -> &'static str {
        "accounted"
    }
    fn description(&self) -> &'static str {
        "counts schema materializations"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        self.parameters_calls.fetch_add(1, Ordering::SeqCst);
        Some(serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }))
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("accounted") })
    }
}

/// One run that produces TWO assistant messages (a one-shot follow-up
/// continues it), so a per-message repair-context cost is paid twice.
/// `context.tools` is predeclared empty: the loop's one-time registry
/// materialization would otherwise read the schemas before any message
/// exists and hide the repair-context count.
async fn run_two_assistant_messages_with_repair(
    repair: Option<pir::agent::agent_loop::MessageEndRepairFn>,
    parameters_calls: Arc<AtomicUsize>,
) {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SchemaAccountingTool { parameters_calls }));
    let injected = Arc::new(AtomicUsize::new(0));
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async move { assistant(vec![text("ok")], StopReason::Stop) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        message_end_repair: repair,
        max_turns: 4,
        follow_up: Some(Box::new(move || {
            let injected = Arc::clone(&injected);
            Box::pin(async move {
                if injected.fetch_add(1, Ordering::SeqCst) == 0 {
                    vec!["continue".to_owned().into()]
                } else {
                    Vec::new()
                }
            })
        })),
        ..LoopDeps::default()
    };
    let context = Context {
        tools: Some(Vec::new()),
        ..Context::default()
    };
    let result = run_with_options(deps, context, None, LoopOptions::default()).await;
    assert_eq!(
        result.turns, 2,
        "two assistant messages must reach the repair seam twice"
    );
}

/// No handler: the loop must not read (or clone) any tool schema at all.
#[tokio::test]
async fn message_end_repair_without_a_handler_reads_no_tool_schemas() {
    let reads = Arc::new(AtomicUsize::new(0));
    run_two_assistant_messages_with_repair(None, Arc::clone(&reads)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the no-handler path clones zero schemas"
    );
}

/// A handler that never touches `active_tools` must not pay for the map: the
/// former eager context rebuilt (name clone + full schema clone per tool)
/// before every invocation, twice here.
#[tokio::test]
async fn message_end_repair_ignoring_active_tools_materializes_nothing() {
    let reads = Arc::new(AtomicUsize::new(0));
    let hook: pir::agent::agent_loop::MessageEndRepairFn =
        Arc::new(|_context| Box::pin(async { None }));
    run_two_assistant_messages_with_repair(Some(hook), Arc::clone(&reads)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "a lazy view that is never read clones zero schemas"
    );
}

/// A handler that reads `active_tools` materializes the map ONCE for the
/// whole run: the second message shares the first message's snapshot.
#[tokio::test]
async fn message_end_repair_materializes_active_tools_once_per_run() {
    let reads = Arc::new(AtomicUsize::new(0));
    let observed_names = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_for_hook = Arc::clone(&observed_names);
    let hook: pir::agent::agent_loop::MessageEndRepairFn =
        Arc::new(move |context: MessageEndRepairContext| {
            let observed = Arc::clone(&observed_for_hook);
            Box::pin(async move {
                let mut names = observed.lock().unwrap();
                for (name, schema) in context.active_tools.iter() {
                    assert!(schema.is_object(), "the schema is the live tool schema");
                    names.push(name.clone());
                }
                None
            })
        });
    run_two_assistant_messages_with_repair(Some(hook), Arc::clone(&reads)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "one materialization per run, not one per assistant message"
    );
    assert_eq!(
        *observed_names.lock().unwrap(),
        vec!["accounted".to_owned(), "accounted".to_owned()],
        "each message saw the same one-entry map"
    );
}

// ---------------------------------------------------------------------------
// Synthetic terminal messages go through the `message_end` chain
// ---------------------------------------------------------------------------

fn event_label(event: AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart { .. } => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart { .. } => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::StreamStart { .. } => "stream_start",
        AgentEvent::StreamEnd { .. } => "stream_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_end",
        AgentEvent::StreamEvent { .. } => "stream_event",
        AgentEvent::Compaction { .. } => "compaction",
    }
}

/// The turn cap's synthetic terminal is a real transcript message: a JS
/// `message_end` handler may replace it, and the replacement is what the
/// terminal, the transcript, the persistence sink, and `turn_end` carry.
/// Event order is unchanged (`stream_start` → `stream_end` → `turn_end`).
#[tokio::test]
async fn turn_cap_terminal_goes_through_the_message_end_chain() {
    let fixture = message_end_fixture(
        "turn-cap",
        r#"
        export default function (pi) {
            globalThis.__replaced = 0;
            pi.on("message_end", (event) => {
                const message = event.message;
                if (!message || message.role !== "assistant") return;
                if (message.stopReason !== "error") return;
                if (!String(message.errorMessage || "").includes("turn limit reached")) return;
                globalThis.__replaced += 1;
                return { message: { ...message, errorMessage: "rewritten-by-message-end" } };
            });
        }
        "#,
    )
    .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let (sink, persisted) = recording_sink();
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { assistant(vec![tool_call()], StopReason::ToolUse) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event_label(event));
        })),
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(result.turns, 2);
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("rewritten-by-message-end"),
        "the handler replacement is the run terminal"
    );
    match result.context.messages.last() {
        Some(Message::Assistant(last)) => assert_eq!(
            last.error_message.as_deref(),
            Some("rewritten-by-message-end"),
            "the transcript records the replacement"
        ),
        other => panic!("the cap terminal is the transcript last message: {other:?}"),
    }
    assert!(
        persisted.lock().unwrap().iter().any(|message| matches!(
            message,
            Message::Assistant(assistant)
                if assistant.error_message.as_deref() == Some("rewritten-by-message-end")
        )),
        "the persisted bytes carry the replacement"
    );
    let replaced = fixture
        .runtime
        .eval_json("JSON.stringify(globalThis.__replaced)")
        .await
        .expect("probe");
    assert_eq!(replaced, serde_json::json!(1), "one replacement dispatch");
    let events = events.lock().unwrap();
    assert_eq!(
        &events[events.len() - 4..],
        &["stream_start", "stream_end", "turn_end", "agent_end"],
        "event order is unchanged: {events:?}"
    );
}

/// The degenerate-loop stop terminal goes through the same chain: a JS
/// `message_end` handler can replace it, and the replacement reaches the
/// terminal, transcript, persistence sink and `turn_end`.
#[tokio::test]
async fn degenerate_stop_terminal_goes_through_the_message_end_chain() {
    let fixture = message_end_fixture(
        "degenerate-stop",
        r#"
        export default function (pi) {
            globalThis.__replaced = 0;
            pi.on("message_end", (event) => {
                const message = event.message;
                if (!message || message.role !== "assistant") return;
                if (message.stopReason !== "error") return;
                if (!String(message.errorMessage || "").includes("identical tool calls")) return;
                globalThis.__replaced += 1;
                return { message: { ...message, errorMessage: "rewritten-degenerate-stop" } };
            });
        }
        "#,
    )
    .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let (sink, persisted) = recording_sink();
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { assistant(vec![tool_call()], StopReason::ToolUse) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 100,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event_label(event));
        })),
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(result.turns, 40, "nudge at 20, stop on the second trip");
    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("rewritten-degenerate-stop"),
        "the handler replacement is the run terminal"
    );
    match result.context.messages.last() {
        Some(Message::Assistant(last)) => assert_eq!(
            last.error_message.as_deref(),
            Some("rewritten-degenerate-stop"),
            "the transcript records the replacement"
        ),
        other => panic!("the stop terminal is the transcript last message: {other:?}"),
    }
    assert!(
        persisted.lock().unwrap().iter().any(|message| matches!(
            message,
            Message::Assistant(assistant)
                if assistant.error_message.as_deref() == Some("rewritten-degenerate-stop")
        )),
        "the persisted bytes carry the replacement"
    );
    let replaced = fixture
        .runtime
        .eval_json("JSON.stringify(globalThis.__replaced)")
        .await
        .expect("probe");
    assert_eq!(replaced, serde_json::json!(1), "one replacement dispatch");
    let events = events.lock().unwrap();
    assert_eq!(
        &events[events.len() - 4..],
        &["stream_start", "stream_end", "turn_end", "agent_end"],
        "event order is unchanged: {events:?}"
    );
}

/// ①: the replacement reaches the run's produced messages, the persistence
/// sink, and the NEXT provider request's context (the loop continues with the
/// replaced message), while a message the handler leaves alone keeps its exact
/// bytes.
#[tokio::test]
async fn js_message_end_replacement_reaches_the_next_request() {
    let fixture = message_end_fixture(
        "next-request",
        r#"
        export default function (pi) {
            pi.on("message_end", (event) => {
                if (!event.message || event.message.role !== "assistant") return;
                const content = Array.isArray(event.message.content) ? event.message.content : [];
                if (!content.some((block) => block.type === "toolCall")) return;
                return {
                    message: {
                        ...event.message,
                        content: [{ type: "text", text: "patched-by-js" }, ...content],
                    },
                };
            });
        }
        "#,
    )
    .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    // The request's context is SHARED (`ProviderRequest.context: Arc<Context>`),
    // so the stream callback records the handle it was handed.
    let contexts = Arc::new(Mutex::new(Vec::<Arc<Context>>::new()));
    let contexts_for_stream = Arc::clone(&contexts);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            contexts_for_stream.lock().unwrap().push(request.context);
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let (sink, persisted) = recording_sink();
    let options = LoopOptions {
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    // The run's produced messages carry the replacement as their first
    // content block (the tool result is the transcript's own message).
    let produced = first_assistant(&result.new_messages);
    assert_eq!(
        produced.content.first(),
        Some(&text("patched-by-js")),
        "the produced assistant message is the replacement"
    );
    assert!(
        produced
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_))),
        "the replacement keeps the tool call: {produced:?}"
    );
    // The persistence sink (the session's append path) saw the replacement...
    let persisted = persisted.lock().unwrap().clone();
    assert_eq!(
        first_assistant(&persisted).content.first(),
        Some(&text("patched-by-js"))
    );
    // ...and so did the NEXT provider request: the transcript the model was
    // asked to continue from contains the replaced message.
    let contexts = contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 2, "two provider requests");
    assert!(
        contexts[1].messages.iter().any(|message| matches!(
            message,
            Message::Assistant(assistant)
                if assistant.content.first() == Some(&text("patched-by-js"))
        )),
        "the second request must continue from the replaced message"
    );
    // The final assistant message (no tool call) is NOT replaced: the handler
    // returns nothing for it, so its bytes are untouched (the loop's own
    // `durationMs`/`thinkingLevel` stamps aside).
    assert_eq!(result.terminal.content, vec![text("final")]);
    let mut reference = assistant(vec![text("final")], StopReason::Stop);
    reference.duration_ms = result.terminal.duration_ms;
    reference.thinking_level = result.terminal.thinking_level;
    assert_eq!(result.terminal, reference);
}

// ---------------------------------------------------------------------------
// JS-round-tripped JSON comparison
// ---------------------------------------------------------------------------

/// Compare a JSON value that crossed the Rust -> QuickJS -> Rust boundary with
/// a value produced by the Rust reference construction, comparing NUMBERS BY
/// VALUE. QuickJS has a single `number` type (a double), so an integral Rust
/// float (`0.0`, `120.0` — every `Usage`/`Cost` field) round-trips as the JS
/// number `0` and comes back as a JSON integer: the two serialize differently
/// (`0` vs `0.0`) while being the same value. That is inherent to the boundary,
/// not a drift; a byte-level comparison would reject every message carrying a
/// zero cost. Everything else (strings, bools, null, key sets, array order)
/// must match exactly.
fn assert_message_json_equal(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    context: &str,
) {
    fn same(a: &serde_json::Value, b: &serde_json::Value) -> bool {
        match (a, b) {
            (serde_json::Value::Number(x), serde_json::Value::Number(y)) => {
                x.as_f64() == y.as_f64()
            }
            (serde_json::Value::Array(x), serde_json::Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y.iter()).all(|(x, y)| same(x, y))
            }
            (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(key, value)| y.get(key).is_some_and(|other| same(value, other)))
            }
            _ => a == b,
        }
    }
    assert!(
        same(actual, expected),
        "{context}: the JS-round-tripped value must equal the reference as a VALUE \
         (numbers by value: JS has one number type)\n actual:   {actual}\n expected: {expected}"
    );
}

// ---------------------------------------------------------------------------
// `message_update` streaming wire (2026-10-15, per-delta cost lane; delta-only
// live-message lane, 2026-10-15)
// ---------------------------------------------------------------------------

/// The per-delta wire (`__pir_dispatchEventReadOnly`'s payload) carries the
/// DELTA, never the cumulative snapshot: `assistantMessageEvent` is the raw
/// event minus its `partial` (plus the internal `block`/`patch` helper the JS
/// accumulator consumes and deletes), `message` is the message's O(1) fields
/// with `content` left as the empty seed the JS accumulator owns, and the two
/// internal top-level keys (`__pirDelta`, `__pirStream`) mark the route and the
/// stream. Every value the reference construction exposes stays reachable: the
/// scalars match the reference `message`, the shipped block is the partial's
/// own block, and the fallback serializer (`message_update_snapshot_json`) IS
/// the reference `message`.
#[test]
fn message_update_stream_wire_carries_the_delta_not_the_snapshot() {
    const SNAPSHOT_BYTES: usize = 8_192;
    let accumulated = "x".repeat(SNAPSHOT_BYTES);
    let partial = assistant(vec![text(&accumulated)], StopReason::Stop);
    let event = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "chunk".into(),
        partial: Arc::new(partial.clone()),
    };
    let mut baselines = pir::agent::agent_loop::ToolArgumentBaselines::new();
    let wire_json = pir::extensions::runtime::message_update_stream_json(&event, 7, &mut baselines);
    // Not a single byte of the accumulated content crosses the boundary.
    assert!(
        !wire_json.contains(&accumulated[..64]),
        "the per-delta wire must not carry the accumulated content"
    );
    let wire: serde_json::Value = serde_json::from_str(&wire_json).expect("stream wire parses");
    let reference = pir::agent::agent_loop::message_update_payload(&event);

    assert_eq!(wire["type"], serde_json::json!("message_update"));
    assert_eq!(wire["__pirDelta"], serde_json::json!(true));
    assert_eq!(wire["__pirStream"], serde_json::json!(7));
    let keys: Vec<&str> = wire
        .as_object()
        .expect("wire object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "type",
            "message",
            "assistantMessageEvent",
            "__pirDelta",
            "__pirStream"
        ],
        "the reference keys keep their order; the internal markers ride last"
    );

    // `assistantMessageEvent`: the reference event minus `partial`. A delta
    // that is not the block's whole content ships no block helper — the JS
    // accumulator appends it to the block it already has.
    let mut expected_event = reference["assistantMessageEvent"].clone();
    expected_event
        .as_object_mut()
        .expect("event object")
        .remove("partial");
    assert_eq!(wire["assistantMessageEvent"], expected_event);
    assert!(
        wire["assistantMessageEvent"].get("block").is_none(),
        "an appendable delta ships no block: {}",
        wire["assistantMessageEvent"]
    );

    // `message`: the reference message with the content replaced by the seed.
    let mut expected_message = reference["message"].clone();
    expected_message["content"] = serde_json::json!([]);
    assert_eq!(wire["message"], expected_message);
    let message_keys: Vec<&str> = wire["message"]
        .as_object()
        .expect("message object")
        .keys()
        .map(String::as_str)
        .collect();
    let reference_keys: Vec<&str> = reference["message"]
        .as_object()
        .expect("reference message object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        message_keys, reference_keys,
        "the live-message view keeps the reference's field order"
    );

    // A delta that IS the block's whole content (the provider opened the block
    // with it) ships the authoritative block, so the JS side can create it.
    let opened = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: accumulated.clone(),
        partial: Arc::new(partial.clone()),
    };
    let wire: serde_json::Value = serde_json::from_str(
        &pir::extensions::runtime::message_update_stream_json(&opened, 7, &mut baselines),
    )
    .expect("opened-block wire parses");
    assert_eq!(
        wire["assistantMessageEvent"]["block"], reference["message"]["content"][0],
        "the shipped block is the partial's own block"
    );
    assert!(
        wire["assistantMessageEvent"].get("patch").is_none(),
        "a text delta never ships an arguments patch"
    );

    // Finalized blocks ship it too (a block can gain fields the delta never
    // carried, e.g. a signature), so the accumulator replaces rather than
    // guesses.
    let finalized = assistant(
        vec![ContentBlock::Text(TextContent {
            text: "done".into(),
            text_signature: Some("sig".into()),
        })],
        StopReason::Stop,
    );
    let end = AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "done".into(),
        partial: Arc::new(finalized.clone()),
    };
    let end_reference = pir::agent::agent_loop::message_update_payload(&end);
    let wire: serde_json::Value = serde_json::from_str(
        &pir::extensions::runtime::message_update_stream_json(&end, 7, &mut baselines),
    )
    .expect("end wire parses");
    assert_eq!(
        wire["assistantMessageEvent"]["block"], end_reference["message"]["content"][0],
        "the end event ships the finalized block"
    );

    // Tool-call deltas: the FIRST delta of an index ships the whole parsed
    // block (the accumulator has no baseline to patch); the block is the
    // provider's mirrored, PARSED arguments — never the raw fragment.
    let tool_partial = assistant(vec![tool_call()], StopReason::Stop);
    let tool_delta = AssistantMessageEvent::ToolCallDelta {
        content_index: 0,
        delta: "{\"a\":1}".into(),
        partial: Arc::new(tool_partial.clone()),
    };
    let wire: serde_json::Value = serde_json::from_str(
        &pir::extensions::runtime::message_update_stream_json(&tool_delta, 7, &mut baselines),
    )
    .expect("tool delta wire parses");
    let tool_reference = pir::agent::agent_loop::message_update_payload(&tool_delta);
    assert_eq!(
        wire["assistantMessageEvent"]["block"], tool_reference["message"]["content"][0],
        "a tool-call delta with no baseline ships the provider's block"
    );

    // The fallback serializer is the reference `message` exactly (the wire
    // tests' reference; the streaming path never materializes it).
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &pir::extensions::runtime::message_update_snapshot_json(&event)
        )
        .expect("fallback snapshot parses"),
        reference["message"],
        "the fallback snapshot is the reference message"
    );

    // Terminal events have no `partial` and no derivable content: there the
    // wire IS the reference payload plus the stream marker, `{role:
    // "assistant"}` fallback included.
    let done = AssistantMessageEventStream::done_event(TerminalReason::Stop, partial);
    let wire: serde_json::Value = serde_json::from_str(
        &pir::extensions::runtime::message_update_stream_json(&done, 7, &mut baselines),
    )
    .expect("terminal wire parses");
    let mut terminal_reference = pir::agent::agent_loop::message_update_payload(&done);
    assert!(
        wire.get("__pirDelta").is_none(),
        "the reference route carries no delta marker: {wire}"
    );
    assert_eq!(wire["__pirStream"], serde_json::json!(7));
    assert_eq!(wire["message"], terminal_reference["message"]);
    assert_eq!(
        wire["assistantMessageEvent"],
        terminal_reference["assistantMessageEvent"]
    );
    terminal_reference["__pirStream"] = serde_json::json!(7);
    assert_eq!(
        wire, terminal_reference,
        "the terminal wire is the reference payload plus the stream key"
    );
}

/// The host's classification (`message_update_stream_shape`) and the type set
/// the JS accumulator knows (`applyMessageUpdateDelta` in `prelude.js.txt`)
/// must agree exactly: a variant this side calls `Delta` must be one the JS
/// switch handles, and every other variant must travel as the reference
/// payload. This is the Rust half of that contract; the JS half is pinned by
/// `message_update_live_object_matches_the_reference_at_every_delta` (every
/// delta type in the script is applied exactly, zero fallbacks).
#[test]
fn message_update_stream_shape_matches_the_js_accumulators_known_types() {
    // The classification itself lives in the (crate-private)
    // `extensions::runtime::dispatch` module; its observable contract is the
    // wire's `__pirDelta` marker, which is what this test pins per variant.
    let partial = Arc::new(assistant(vec![text("t")], StopReason::Stop));
    let delta_shapes: Vec<(&str, AssistantMessageEvent)> = vec![
        (
            "start",
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
        ),
        (
            "text_start",
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            },
        ),
        (
            "text_delta",
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: partial.clone(),
            },
        ),
        (
            "text_end",
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "t".into(),
                partial: partial.clone(),
            },
        ),
        (
            "thinking_start",
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: partial.clone(),
            },
        ),
        (
            "thinking_delta",
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "x".into(),
                partial: partial.clone(),
            },
        ),
        (
            "thinking_end",
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "t".into(),
                partial: partial.clone(),
            },
        ),
        (
            "toolcall_start",
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: partial.clone(),
            },
        ),
        (
            "toolcall_delta",
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{}".into(),
                partial: partial.clone(),
            },
        ),
        (
            "toolcall_end",
            AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: match tool_call() {
                    ContentBlock::ToolCall(call) => call,
                    _ => unreachable!("tool_call() returns a tool call"),
                },
                partial: partial.clone(),
            },
        ),
    ];
    let mut baselines = pir::agent::agent_loop::ToolArgumentBaselines::new();
    for (name, event) in &delta_shapes {
        let wire: serde_json::Value = serde_json::from_str(
            &pir::extensions::runtime::message_update_stream_json(event, 1, &mut baselines),
        )
        .expect("delta wire parses");
        assert_eq!(
            wire["__pirDelta"], true,
            "{name}: a delta line carries the marker"
        );
        assert_eq!(
            wire["assistantMessageEvent"]["type"],
            serde_json::json!(*name),
            "the wire event type is the JS accumulator's switch key"
        );
    }
    for (name, event) in [
        (
            "done",
            AssistantMessageEventStream::done_event(TerminalReason::Stop, stream_message("t")),
        ),
        (
            "error",
            AssistantMessageEventStream::error_event(ErrorReason::Error, stream_message("t")),
        ),
    ] {
        let wire: serde_json::Value = serde_json::from_str(
            &pir::extensions::runtime::message_update_stream_json(&event, 1, &mut baselines),
        )
        .expect("reference wire parses");
        assert!(
            wire.get("__pirDelta").is_none(),
            "{name}: the reference route carries no delta marker"
        );
    }
}

/// The scripted per-delta stream the live-message test pushes through the loop:
/// every `AssistantMessageEvent` shape whose wire is the delta-only one, with
/// partials built the way the providers build them (text/thinking append, a
/// tool-call block whose arguments are the PARSED in-flight JSON, a
/// usage-carrying chunk), then the terminal `done`.
fn live_message_script() -> Vec<AssistantMessageEvent> {
    let usage_zero = Usage::default();
    let usage_mid = Usage {
        input: 120,
        output: 7,
        cache_read: 3,
        cache_write: 0,
        total_tokens: 127,
        ..Usage::default()
    };
    let text_block = |text: &str, signature: Option<&str>| {
        ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: signature.map(str::to_owned),
        })
    };
    let thinking_block = |thinking: &str, signature: Option<&str>| {
        ContentBlock::Thinking(ThinkingContent {
            thinking: thinking.into(),
            thinking_signature: signature.map(str::to_owned),
            redacted: false,
        })
    };
    let tool_block = |arguments: BTreeMap<String, serde_json::Value>| {
        ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })
    };
    let ls_arguments = || {
        let mut arguments = BTreeMap::new();
        arguments.insert("cmd".to_string(), serde_json::json!("ls"));
        arguments
    };
    let partial = |content: Vec<ContentBlock>, usage: &Usage| {
        let mut message = assistant(content, StopReason::Pending);
        message.usage = *usage;
        message.response_id = Some("chatcmpl-1".into());
        message
    };

    // The stream's `start` carries an EMPTY content: every adapter snapshots
    // its accumulator BEFORE reading the response body (openai_stream /
    // anthropic_messages_stream / responses stream / pi_messages stream all
    // push `Start { partial: parser.snapshot() }` on a freshly built parser),
    // so no block exists yet — the empty text block is opened by the
    // `text_start` that follows, a dense append at content_index 0. A `start`
    // already carrying a block would make that `text_start` a NON-dense block
    // opening (index 0 into a one-block accumulator), which the JS accumulator
    // reports as a fallback by design.
    let mut events = vec![AssistantMessageEvent::Start {
        partial: Arc::new(partial(Vec::new(), &usage_zero)),
    }];
    events.push(AssistantMessageEvent::TextStart {
        content_index: 0,
        partial: Arc::new(partial(vec![text_block("", None)], &usage_zero)),
    });
    events.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "Hel".into(),
        partial: Arc::new(partial(vec![text_block("Hel", None)], &usage_zero)),
    });
    events.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "lo".into(),
        partial: Arc::new(partial(vec![text_block("Hello", None)], &usage_zero)),
    });
    events.push(AssistantMessageEvent::ThinkingStart {
        content_index: 1,
        partial: Arc::new(partial(
            vec![text_block("Hello", None), thinking_block("", None)],
            &usage_zero,
        )),
    });
    events.push(AssistantMessageEvent::ThinkingDelta {
        content_index: 1,
        delta: "th".into(),
        partial: Arc::new(partial(
            vec![text_block("Hello", None), thinking_block("th", None)],
            &usage_zero,
        )),
    });
    // A usage-carrying chunk: the cumulative usage rides the message view.
    events.push(AssistantMessageEvent::ThinkingDelta {
        content_index: 1,
        delta: "ink".into(),
        partial: Arc::new(partial(
            vec![text_block("Hello", None), thinking_block("think", None)],
            &usage_mid,
        )),
    });
    events.push(AssistantMessageEvent::ToolCallStart {
        content_index: 2,
        partial: Arc::new(partial(
            vec![
                text_block("Hello", None),
                thinking_block("think", None),
                tool_block(BTreeMap::new()),
            ],
            &usage_mid,
        )),
    });
    events.push(AssistantMessageEvent::ToolCallDelta {
        content_index: 2,
        delta: "{\"cmd\":\"ls\"}".into(),
        partial: Arc::new(partial(
            vec![
                text_block("Hello", None),
                thinking_block("think", None),
                tool_block(ls_arguments()),
            ],
            &usage_mid,
        )),
    });
    events.push(AssistantMessageEvent::ThinkingEnd {
        content_index: 1,
        content: "think".into(),
        partial: Arc::new(partial(
            vec![
                text_block("Hello", None),
                thinking_block("think", Some("sig-t")),
                tool_block(ls_arguments()),
            ],
            &usage_mid,
        )),
    });
    events.push(AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "Hello".into(),
        partial: Arc::new(partial(
            vec![
                text_block("Hello", Some("sig-x")),
                thinking_block("think", Some("sig-t")),
                tool_block(ls_arguments()),
            ],
            &usage_mid,
        )),
    });
    let final_content = vec![
        text_block("Hello", Some("sig-x")),
        thinking_block("think", Some("sig-t")),
        ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: ls_arguments(),
            thought_signature: Some("ts".into()),
            namespace: None,
            arguments_error: None,
        }),
    ];
    let finalized_call = match final_content[2].clone() {
        ContentBlock::ToolCall(call) => call,
        _ => unreachable!("the finalized tool call"),
    };
    events.push(AssistantMessageEvent::ToolCallEnd {
        content_index: 2,
        tool_call: finalized_call,
        partial: Arc::new(partial(final_content.clone(), &usage_mid)),
    });
    let mut final_message = assistant(final_content, StopReason::ToolUse);
    final_message.usage = usage_mid;
    final_message.response_id = Some("chatcmpl-1".into());
    events.push(AssistantMessageEventStream::done_event(
        TerminalReason::ToolUse,
        final_message,
    ));
    events
}

/// A provider stream that pushes `events` once.
fn scripted_stream(events: Vec<AssistantMessageEvent>) -> EventStreamFn {
    let events = Arc::new(Mutex::new(Some(events)));
    Box::new(move |_request| {
        let events = events.lock().unwrap().take();
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            let sink = stream.clone();
            tokio::spawn(async move {
                if let Some(events) = events {
                    for event in events {
                        sink.push(event);
                    }
                }
            });
            stream
        })
    })
}

/// Load one extension fixture into a fresh runtime rooted at a temp dir.
async fn live_message_runtime(
    name: &str,
    source: &str,
) -> (std::path::PathBuf, pir::extensions::ExtensionHandle) {
    let temp = std::env::temp_dir().join(format!(
        "pir-loop-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let path = temp.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: temp.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    (temp, runtime)
}

/// NO DRIFT: the JS-side live message (upstream's live `AgentMessage`: one
/// object per stream, mutated in place per delta) is deep-equal to the
/// reference construction's `message` at EVERY event of a scripted stream that
/// covers text deltas, thinking deltas, tool-call start/delta/end, a
/// usage-carrying chunk and the terminal `done` — and
/// `assistantMessageEvent.partial` is that same object (the reference's
/// `partial`). A recognised delta never asks the host for anything: the
/// `messageUpdateSnapshotJson` host function is GONE (zero host callbacks is
/// the contract), nothing is reported, and the object's identity is stable for
/// the whole stream. Every dispatch carries a stream marker; the delta line
/// carries `__pirDelta`, the terminal payload does not.
#[tokio::test]
async fn message_update_live_object_matches_the_reference_at_every_delta() {
    let source = r#"
        export default function (pi) {
            globalThis.__seen = [];
            globalThis.__routes = [];
            globalThis.__snapshotHostFn = typeof __pir_host.messageUpdateSnapshotJson;
            globalThis.__hostCalls = 0;
            globalThis.__hostCallNames = [];
            globalThis.__callsAtFirstDispatch = -1;
            globalThis.__callsAfterFirstDispatch = -1;
            globalThis.__exportsAfterFirstDispatch = -1;
            globalThis.__callsAtSecondDispatch = -1;
            globalThis.__exportsAtSecondDispatch = -1;
            globalThis.__exportsPerDispatch = [];
            for (const name of Object.keys(__pir_host)) {
                const fn = __pir_host[name];
                if (typeof fn !== "function") continue;
                // `getContextJson` is the ctx DESCRIPTION source every dispatch
                // legitimately reads once (a small session-context JSON, not a
                // registry-sized one); `notify`/`setStatus` are host side
                // effects a handler may legitimately make. Everything else must
                // be zero between dispatches.
                if (name === "getContextJson" || name === "notify" || name === "setStatus") continue;
                try {
                    __pir_host[name] = function (...args) {
                        globalThis.__hostCalls += 1;
                        globalThis.__hostCallNames.push(name);
                        return fn.apply(__pir_host, args);
                    };
                } catch {
                    /* non-writable host props: the call count probe stays as is */
                }
            }
            // The registry export is a JS call, invisible to the host-fn
            // wrapper: the dispatch tail's early-out must keep it at zero for a
            // stream that registers nothing.
            globalThis.__exportCalls = 0;
            const exportRegistry = globalThis.__pir_exportRegistryJson;
            globalThis.__pir_exportRegistryJson = function (...args) {
                globalThis.__exportCalls += 1;
                return exportRegistry.apply(globalThis, args);
            };
            const entry = globalThis.__pir_dispatchEventReadOnly;
            globalThis.__pir_dispatchEventReadOnly = async function (eventName, payloadJson) {
                const isUpdate = eventName === "message_update";
                if (isUpdate && globalThis.__callsAtSecondDispatch < 0
                    && globalThis.__callsAfterFirstDispatch >= 0) {
                    // The FIRST dispatch's HOST-side tail is still ahead of this
                    // point: that tail is the refresh point which publishes the
                    // fixture's own load-time registration (the load path leaves
                    // the publication to the first refresh), so any JS-side
                    // baseline taken inside or right after that dispatch's entry
                    // misses exactly that one export. From the SECOND dispatch
                    // on, every refresh early-outs — that is the window this
                    // baseline opens, and it is asserted by exact equality.
                    globalThis.__callsAtSecondDispatch = globalThis.__hostCalls;
                    globalThis.__exportsAtSecondDispatch = globalThis.__exportCalls;
                }
                if (isUpdate && globalThis.__callsAtFirstDispatch < 0) {
                    globalThis.__callsAtFirstDispatch = globalThis.__hostCalls;
                }
                if (isUpdate) {
                    globalThis.__exportsPerDispatch.push(globalThis.__exportCalls);
                    const payload = JSON.parse(payloadJson);
                    globalThis.__routes.push({
                        delta: payload.__pirDelta === true,
                        hasStream: Object.prototype.hasOwnProperty.call(payload, "__pirStream"),
                        stream: payload.__pirStream ?? null,
                    });
                }
                const out = await entry.call(globalThis, eventName, payloadJson);
                if (isUpdate && globalThis.__callsAfterFirstDispatch < 0) {
                    // Baseline AFTER the first dispatch: the load-time registry
                    // publication legitimately lands in that dispatch's TAIL
                    // (the fixture's `pi.on` bumped handlers_gen while loading
                    // and the first refresh publishes it), so a baseline taken
                    // at the dispatch's START would count it as a per-delta
                    // export. Baselines taken here cover it exactly.
                    globalThis.__callsAfterFirstDispatch = globalThis.__hostCalls;
                    globalThis.__exportsAfterFirstDispatch = globalThis.__exportCalls;
                }
                return out;
            };
            pi.on("message_update", (event) => {
                const assistantEvent = event.assistantMessageEvent;
                globalThis.__seen.push({
                    type: assistantEvent ? assistantEvent.type : null,
                    message: JSON.stringify(event.message),
                    assistantEvent: JSON.stringify(assistantEvent),
                    aliased: assistantEvent !== null && event.message === assistantEvent.partial,
                    blockLeak: assistantEvent !== null
                        && Object.prototype.hasOwnProperty.call(assistantEvent, "block"),
                    patchLeak: assistantEvent !== null
                        && Object.prototype.hasOwnProperty.call(assistantEvent, "patch"),
                    sameObject: globalThis.__live === undefined || event.message === globalThis.__live,
                    payloadKeys: Object.keys(event),
                });
                if (globalThis.__live === undefined) globalThis.__live = event.message;
            });
        }
        "#;
    let (temp, runtime) = live_message_runtime("live-message-reference", source).await;
    let events = live_message_script();
    let options = LoopOptions {
        event_stream: Some(scripted_stream(events.clone())),
        ..LoopOptions::default()
    };
    let deps = LoopDeps {
        extensions: Some(runtime.clone()),
        // One turn: the scripted stream answers ONCE. The turn's ToolUse
        // message is the terminal this test inspects; the cap's own terminal
        // (the run cannot continue on an exhausted script) comes after it.
        max_turns: 1,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    let provider_message = result
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) if assistant.stop_reason != StopReason::Error => {
                Some(assistant)
            }
            _ => None,
        })
        .expect("the turn assistant message");
    assert_eq!(provider_message.stop_reason, StopReason::ToolUse);

    let probe = runtime
        .eval_json(
            "JSON.stringify({seen: globalThis.__seen, routes: globalThis.__routes, \
             snapshotHostFn: globalThis.__snapshotHostFn, hostCalls: globalThis.__hostCalls, \
             callsAtFirstDispatch: globalThis.__callsAtFirstDispatch, \
             callsAfterFirstDispatch: globalThis.__callsAfterFirstDispatch, \
             exportCalls: globalThis.__exportCalls, \
             exportsAfterFirstDispatch: globalThis.__exportsAfterFirstDispatch, \
             callsAtSecondDispatch: globalThis.__callsAtSecondDispatch, \
             exportsAtSecondDispatch: globalThis.__exportsAtSecondDispatch, \
             exportsPerDispatch: globalThis.__exportsPerDispatch, \
             hostCallNames: globalThis.__hostCallNames, \
             fallbacks: globalThis.__pir_messageUpdateFallbacks ?? 0})",
        )
        .await
        .expect("probe");
    let seen = probe["seen"].as_array().expect("seen array");
    assert_eq!(
        seen.len(),
        events.len(),
        "one dispatch per streamed event, in order"
    );
    assert_eq!(
        probe["snapshotHostFn"].as_str(),
        Some("undefined"),
        "the per-dispatch snapshot host function is gone: the host classifies the wire"
    );
    assert!(
        probe["callsAtFirstDispatch"]
            .as_u64()
            .is_some_and(|calls| calls > 0),
        "the probe must have observed the stream: {probe:?}"
    );
    // Exact equality (no slack): after the first dispatch's own tail, a stream
    // that registers nothing must not call a single host function or re-export
    // the registry again.
    assert_eq!(
        probe["hostCalls"], probe["callsAfterFirstDispatch"],
        "no host call may happen after the first dispatch (the per-delta dispatch is one JS entry \
         and pure Rust on either side): {:?}",
        probe["hostCallNames"]
    );
    // Registry exports: AT MOST ONE in the whole run — the publication of the
    // fixture's own load-time registration. The load commit bumps
    // `registry_gen` (the prelude's `markRegistryDirty` at the end of
    // `__pir_loadExtension`), and whichever refresh point first OBSERVES the
    // dirty registry performs the one export: the load path itself when the
    // commit landed before `load()` returned, otherwise the host-side tail of
    // the first dispatch (or of a later one) — a host tail a JS-side baseline
    // cannot see, so the pin is stated over the run instead of over one window.
    // The regression this guards (a per-delta export when nothing registers)
    // grows the count once per dispatch, which both assertions reject.
    assert!(
        probe["exportCalls"].as_u64().unwrap_or_default() <= 1,
        "at most one registry export (the load-time publication) may happen in the run; a \
         per-delta export would grow with the dispatch count: {probe:?}"
    );
    let export_starts: Vec<u64> = probe["exportsPerDispatch"]
        .as_array()
        .expect("per-dispatch exports")
        .iter()
        .map(|value| value.as_u64().unwrap_or_default())
        .collect();
    let growing_windows = export_starts
        .windows(2)
        .filter(|pair| pair[1] > pair[0])
        .count();
    let tail_growth = probe["exportCalls"].as_u64().unwrap_or_default()
        > export_starts.last().copied().unwrap_or_default();
    // WHERE the single publication landed, so a failure names its trigger
    // instead of only its count: the load path itself (published before the
    // first dispatch), the host-side tail of one dispatch (the refresh point
    // that first observed the dirty registry), or the run's tail.
    let located = if export_starts.first().copied().unwrap_or_default() > 0 {
        "the load path (published before the first dispatch)".to_owned()
    } else if let Some(window) = export_starts.windows(2).position(|pair| pair[1] > pair[0]) {
        format!("the host-side tail of dispatch {window}")
    } else if tail_growth {
        "the run's tail (after the last message_update dispatch)".to_owned()
    } else {
        "nowhere".to_owned()
    };
    assert!(
        growing_windows + usize::from(tail_growth) <= 1,
        "at most one window may carry the single load-time publication (trigger located at \
         {located}): {probe:?}"
    );
    assert_eq!(probe["fallbacks"].as_u64(), Some(0));
    let routes = probe["routes"].as_array().expect("routes array");
    assert_eq!(routes.len(), events.len(), "one route per dispatch");
    for (index, (route, event)) in routes.iter().zip(events.iter()).enumerate() {
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        assert_eq!(
            route["delta"].as_bool(),
            Some(!terminal),
            "entry {index}: the host's classification must match the route (delta line vs reference payload)"
        );
        assert_eq!(
            route["hasStream"].as_bool(),
            Some(true),
            "entry {index}: every wire carries the stream marker"
        );
        if index > 0 {
            assert_eq!(
                route["stream"], routes[0]["stream"],
                "entry {index}: one stream id for the whole stream"
            );
        }
    }

    for (index, (entry, event)) in seen.iter().zip(events.iter()).enumerate() {
        let reference = pir::agent::agent_loop::message_update_payload(event);
        assert_eq!(
            entry["type"].as_str(),
            reference["assistantMessageEvent"]["type"].as_str(),
            "event order, entry {index}"
        );
        let message: serde_json::Value =
            serde_json::from_str(entry["message"].as_str().expect("message json"))
                .expect("recorded message parses");
        assert_message_json_equal(
            &message,
            &reference["message"],
            &format!("entry {index} ({})", entry["type"]),
        );
        let assistant_event: serde_json::Value =
            serde_json::from_str(entry["assistantEvent"].as_str().expect("event json"))
                .expect("recorded event parses");
        assert_message_json_equal(
            &assistant_event,
            &reference["assistantMessageEvent"],
            &format!("entry {index} ({}) event view", entry["type"]),
        );
        assert_eq!(
            entry["aliased"].as_bool(),
            Some(!matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            )),
            "entry {index}: `message` and `partial` are one object (per-delta only)"
        );
        assert_eq!(
            entry["blockLeak"].as_bool(),
            Some(false),
            "entry {index}: the internal block helper never reaches a handler"
        );
        let payload_keys: Vec<&str> = entry["payloadKeys"]
            .as_array()
            .expect("payload keys")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(
            payload_keys,
            vec!["type", "message", "assistantMessageEvent"],
            "entry {index}: the payload keeps the reference keys and order"
        );
        // The live object is ONE object for the whole stream (upstream's live
        // message identity; pi-phone keeps it as `liveAssistantMessage`). The
        // terminal event hands the reference payload instead (its `message` is
        // the historical shape), so it is excluded like the alias check above.
        if !matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        ) {
            assert_eq!(
                entry["sameObject"].as_bool(),
                Some(true),
                "entry {index}: the live object identity must not change mid-stream"
            );
        }
        if index == 1 {
            // The first per-delta entry's object came straight from the wire,
            // so its key order is the reference's field order.
            let message_keys: Vec<&str> = message
                .as_object()
                .expect("message object")
                .keys()
                .map(String::as_str)
                .collect();
            let reference_keys: Vec<&str> = reference["message"]
                .as_object()
                .expect("reference message object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(message_keys, reference_keys, "wire field order");
        }
    }
    let _ = std::fs::remove_dir_all(&temp);
}

/// A delta the accumulator cannot apply exactly (here a `*_start` whose
/// `content_index` is beyond the accumulated content — a stream boundary the
/// accumulator missed) must NOT diverge silently: it is reported loudly and the
/// accumulator keeps the authoritative blocks it does have. There is no host
/// snapshot to re-sync from any more (that reply-sized call was removed with
/// the per-delta wire lane): the loud report plus the accumulator's own
/// consistency is the contract, and the next `start` re-anchors it.
#[tokio::test]
async fn message_update_live_object_falls_back_loudly_on_an_unrecognised_delta() {
    let source = r#"
        export default function (pi) {
            globalThis.__seen = [];
            globalThis.__snapshotHostFn = typeof __pir_host.messageUpdateSnapshotJson;
            pi.on("message_update", (event) => {
                globalThis.__seen.push({
                    type: event.assistantMessageEvent ? event.assistantMessageEvent.type : null,
                    message: JSON.stringify(event.message),
                    blocks: event.message && Array.isArray(event.message.content)
                        ? event.message.content.length
                        : -1,
                });
            });
        }
        "#;
    let (temp, runtime) = live_message_runtime("live-message-fallback", source).await;

    // The stream's `start` carries an empty content; the `text_start` that
    // follows announces content_index 3 — the accumulator has no block there
    // and never saw blocks 0..2, so it must fall back (and must NOT fabricate
    // or silently truncate blocks to make the index fit).
    let four_blocks = vec![text("a"), text("b"), text("c"), text("d")];
    let events = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(assistant(Vec::new(), StopReason::Pending)),
        },
        AssistantMessageEvent::TextStart {
            content_index: 3,
            partial: Arc::new(assistant(four_blocks.clone(), StopReason::Pending)),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 3,
            delta: "e".into(),
            partial: Arc::new(assistant(
                vec![text("a"), text("b"), text("c"), text("de")],
                StopReason::Pending,
            )),
        },
        AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant(four_blocks, StopReason::Stop),
        ),
    ];
    let options = LoopOptions {
        event_stream: Some(scripted_stream(events.clone())),
        ..LoopOptions::default()
    };
    let deps = LoopDeps {
        extensions: Some(runtime.clone()),
        max_turns: 1,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);

    let probe = runtime
        .eval_json(
            "JSON.stringify({seen: globalThis.__seen, hostFn: globalThis.__snapshotHostFn, \
             fallbacks: globalThis.__pir_messageUpdateFallbacks ?? 0})",
        )
        .await
        .expect("probe");
    assert_eq!(
        probe["hostFn"].as_str(),
        Some("undefined"),
        "there is no per-dispatch snapshot host function to call: {probe:?}"
    );
    assert!(
        probe["fallbacks"].as_u64().unwrap_or_default() >= 2,
        "the unapplicable start and the delta against the missing block must both be reported: {probe:?}"
    );
    let seen = probe["seen"].as_array().expect("seen array");
    assert_eq!(seen.len(), events.len(), "one dispatch per event");
    // The accumulator never fabricates a block for the gap: the reference has
    // four blocks there, and the live object stays empty until the next
    // `start` re-anchors (the documented residual of removing the host
    // snapshot call).
    assert_eq!(
        seen[1]["blocks"].as_i64(),
        Some(0),
        "a gap start must not silently truncate or fabricate: {seen:?}"
    );
    // The events whose value does not depend on the missing blocks stay
    // reference-equal (the `start` wire carries the initial content; the
    // terminal payload is the reference construction).
    for index in [0usize, 3] {
        let reference = pir::agent::agent_loop::message_update_payload(&events[index]);
        let message: serde_json::Value =
            serde_json::from_str(seen[index]["message"].as_str().expect("message json"))
                .expect("recorded message parses");
        assert_message_json_equal(
            &message,
            &reference["message"],
            &format!("entry {index} ({})", seen[index]["type"]),
        );
    }
    let _ = std::fs::remove_dir_all(&temp);
}

/// A `message_update` handler observes every delta (counted in JS) and cannot
/// change the streamed message: neither the read-only dispatch (no reply) nor
/// the event object's mutation surface may alter what the loop streams. The
/// handler here mutates BOTH views in place and returns a replacement message —
/// the stream, the Rust-side event consumer and the run result must all keep
/// the provider's text.
#[tokio::test]
async fn message_update_handler_observes_every_delta_without_touching_the_stream() {
    const TOKENS: [&str; 6] = ["a", "bb", "ccc", "dddd", "eeeee", "ffffff"];

    let temp = std::env::temp_dir().join(format!(
        "pir-loop-message-update-mutation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let path = temp.join("ext.ts");
    let source = r#"
        export default function (pi) {
            globalThis.__updates = 0;
            globalThis.__seen = [];
            pi.on("message_update", (event) => {
                globalThis.__updates += 1;
                const assistantEvent = event.assistantMessageEvent;
                if (!assistantEvent || assistantEvent.type !== "text_delta") return;
                globalThis.__seen.push({
                    delta: assistantEvent.delta,
                    text: assistantEvent.partial.content[0].text,
                    messageText: event.message.content[0].text,
                });
                // In-place mutations of both views, then a returned
                // replacement: none of it may reach the streamed message.
                event.message.content[0].text = "HACKED";
                assistantEvent.partial.content[0].text = "HACKED";
                return { role: "assistant", content: [{ type: "text", text: "HACKED" }] };
            });
        }
        "#
    .to_owned();
    std::fs::write(&path, source).expect("write fixture");
    let runtime = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: temp.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let stream: EventStreamFn = Box::new(move |_request| {
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            let sink = stream.clone();
            tokio::spawn(async move {
                sink.push(AssistantMessageEvent::Start {
                    partial: Arc::new(stream_message("")),
                });
                sink.push(AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: Arc::new(stream_message("")),
                });
                let mut accumulated = String::new();
                for token in TOKENS {
                    accumulated.push_str(token);
                    sink.push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: token.to_string(),
                        partial: Arc::new(stream_message(&accumulated)),
                    });
                }
                sink.push(AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: accumulated.clone(),
                    partial: Arc::new(stream_message(&accumulated)),
                });
                sink.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    stream_message(&accumulated),
                ));
            });
            stream
        })
    });

    // The Rust-side consumer is the reference for "what the provider streamed":
    // it must see the provider's deltas, not the handler's edits.
    let rust_side = Arc::new(Mutex::new(Vec::<String>::new()));
    let rust_side_for_handler = rust_side.clone();
    let options = LoopOptions {
        event_stream: Some(stream),
        events: Some(Arc::new(move |event| {
            if let AgentEvent::StreamEvent {
                event: AssistantMessageEvent::TextDelta { delta, partial, .. },
                ..
            } = &event
            {
                let text = match &partial.content[0] {
                    ContentBlock::Text(text) => text.text.clone(),
                    _ => String::new(),
                };
                rust_side_for_handler
                    .lock()
                    .unwrap()
                    .push(format!("{delta}:{text}"));
            }
        })),
        ..LoopOptions::default()
    };
    let deps = LoopDeps {
        extensions: Some(runtime.clone()),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);

    let expected: String = TOKENS.concat();
    // The run result is the provider's message.
    assert_eq!(result.terminal.content, vec![text(&expected)]);

    // Every delta reached the JS handler exactly once, each with the snapshot
    // accumulated up to that delta, and both views carried it.
    let seen = runtime
        .eval_json("JSON.stringify({updates: globalThis.__updates, seen: globalThis.__seen})")
        .await
        .expect("probe");
    // Start + TextStart + one per delta + TextEnd + Done: the handler counts
    // every streamed event, TextDelta or not (nothing is coalesced away).
    assert_eq!(
        seen["updates"].as_u64(),
        Some(TOKENS.len() as u64 + 4),
        "one dispatch per streamed event: {seen:?}"
    );
    let seen = seen["seen"].as_array().expect("seen array");
    assert_eq!(seen.len(), TOKENS.len(), "one text_delta each: {seen:?}");
    let mut accumulated = String::new();
    for (index, entry) in seen.iter().enumerate() {
        accumulated.push_str(TOKENS[index]);
        assert_eq!(entry["delta"].as_str(), Some(TOKENS[index]));
        assert_eq!(
            entry["text"].as_str(),
            Some(accumulated.as_str()),
            "assistantMessageEvent.partial at delta {index}"
        );
        assert_eq!(
            entry["messageText"].as_str(),
            Some(accumulated.as_str()),
            "event.message at delta {index} carries the same snapshot"
        );
    }

    // The Rust-side consumer saw the unedited deltas too.
    let rust_side = rust_side.lock().unwrap().clone();
    assert_eq!(rust_side.len(), TOKENS.len());
    for (index, entry) in rust_side.iter().enumerate() {
        let prefix: String = TOKENS[..=index].concat();
        assert_eq!(entry.as_str(), format!("{}:{prefix}", TOKENS[index]));
    }

    let _ = std::fs::remove_dir_all(&temp);
}

// ---------------------------------------------------------------------------
// The dispatch boundary: the provider parser's accumulator stays unique
// ---------------------------------------------------------------------------

/// One OpenAI text-delta SSE chunk carrying `token`.
fn openai_text_chunk(token: &str) -> String {
    format!(
        "data: {{\"id\":\"chatcmpl-1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(token).expect("token serializes")
    )
}

/// The OpenAI-compatible model the SSE parser is driven with.
fn openai_stream_model() -> Model {
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

/// Drive `deltas` REAL SSE text chunks through `OpenAiSseParser` and the agent
/// loop, and return (accumulator deep copies, stream events the loop saw).
async fn run_parser_leg(runtime: Option<pir::extensions::ExtensionHandle>, deltas: usize) -> u64 {
    /// Bounded yield budget before each feed: the loop must have released the
    /// previous event's `partial`. The pre-fix dispatcher (which retained the
    /// event across the JS round trip) can never satisfy this, so the test
    /// fails on the copy count instead of hanging on the wait.
    ///
    /// The budget is generous because the wait competes with every other test
    /// in this binary: with libtest's default thread count the 64-yield budget
    /// was exhausted under load and the leg paid extra copies (measured: the
    /// full `cargo test --test g_agent` failed while the same test passed
    /// alone and under `--test-threads=4`). A regressed dispatcher still never
    /// releases, so the failure mode is unchanged — only the load tolerance is.
    const RELEASE_YIELDS: usize = 4096;
    let copies = Arc::new(AtomicUsize::new(0));
    let copies_for_stream = Arc::clone(&copies);
    let stream: EventStreamFn = Box::new(move |_request| {
        let copies = Arc::clone(&copies_for_stream);
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            let sink = stream.clone();
            tokio::spawn(async move {
                let model = openai_stream_model();
                let mut parser = pir::ai::api::openai::OpenAiSseParser::new(&model);
                sink.push(AssistantMessageEvent::Start {
                    partial: parser.snapshot(),
                });
                let mut ready = Vec::new();
                for index in 0..deltas {
                    // Wait (bounded) until nothing but the parser and this
                    // probe's own temporary holds the accumulated message: with
                    // the fixed dispatch boundary the wire is a String and the
                    // event is dropped at enqueue, so the parser's next
                    // mutation is unique and copies nothing.
                    let mut waited = 0;
                    while waited < RELEASE_YIELDS && Arc::strong_count(&parser.snapshot()) > 2 {
                        tokio::task::yield_now().await;
                        waited += 1;
                    }
                    ready.clear();
                    parser
                        .feed(
                            openai_text_chunk(&format!("tok{index} ")).as_bytes(),
                            &mut ready,
                        )
                        .expect("feed succeeds");
                    for event in ready.drain(..) {
                        sink.push(event);
                    }
                }
                // Let the loop release the last delta before the finalize pass:
                // `finish` mutates the accumulator (stop reason, usage, the
                // finalized text block), and a still-held snapshot would force
                // one more whole-reply copy.
                let mut waited = 0;
                while waited < RELEASE_YIELDS && Arc::strong_count(&parser.snapshot()) > 2 {
                    tokio::task::yield_now().await;
                    waited += 1;
                }
                ready.clear();
                parser
                    .feed(
                        b"data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        &mut ready,
                    )
                    .expect("finish reason feeds");
                parser.finish(&mut ready).expect("finish succeeds");
                // Publish the measurement BEFORE the terminal event: the loop
                // returns as soon as it consumes `done`, so a store placed
                // after the push would race the caller (a missed store reads
                // as 0 copies and silently weakens the assertions below).
                copies.store(parser.copy_count() as usize, Ordering::SeqCst);
                for event in ready.drain(..) {
                    sink.push(event);
                }
            });
            stream
        })
    });
    let options = LoopOptions {
        event_stream: Some(stream),
        ..LoopOptions::default()
    };
    let deps = LoopDeps {
        extensions: runtime,
        max_turns: 1,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    copies.load(Ordering::SeqCst) as u64
}

/// THE core evidence of this lane: a registered `message_update` handler must
/// not add a single accumulator deep copy — the parser's copy-on-write
/// accumulator stays unique, because the dispatch boundary only ever holds the
/// O(delta) wire, never the event (whose `partial` is that same `Arc`). Before
/// the fix the dispatcher queue, the in-flight dispatch and the runtime's
/// snapshot slot all retained the event, so every delta paid a whole-reply
/// deep copy (the ~630 MB of churn on a 10 000-delta stream).
#[tokio::test(flavor = "current_thread")]
async fn message_update_handler_adds_no_provider_parser_copies() {
    let baseline = run_parser_leg(None, 256).await;

    let source = r#"
        export default function (pi) {
            globalThis.__updates = 0;
            pi.on("message_update", () => { __updates += 1; });
        }
        "#;
    let (temp, runtime) = live_message_runtime("parser-copy-pin", source).await;
    let with_handler = run_parser_leg(Some(runtime.clone()), 256).await;
    let with_handler_short = run_parser_leg(Some(runtime.clone()), 64).await;
    let probe = runtime
        .eval_json("JSON.stringify({updates: globalThis.__updates ?? 0})")
        .await
        .expect("probe");

    assert!(
        probe["updates"].as_u64().unwrap_or_default() >= (256 + 64) as u64,
        "the handler must have observed every dispatch of both legs: {probe:?}"
    );
    // No handler at all: the parser's own healthy shape, bounded from its own
    // contract — about ONE copy for a whole text block (a chunk that opens a
    // block AND appends to it copies inside `feed`; see
    // `openai_unread_partial_snapshots_pay_one_copy_per_block_not_per_delta` in
    // the ai_sse suite), plus the finalize pass.
    assert!(
        baseline <= 3,
        "the parser alone must not copy per delta: {baseline} copies over 256 deltas"
    );
    // With a handler the parser is only as unique as the stream reader lets it
    // be: while the loop's event queue still holds a queued event, the next
    // parse copies ONCE (the queue is unbounded today — another lane is making
    // it bounded + backpressured). That is at most ONE copy per delta, so the
    // mechanism-derived ceiling is `baseline + deltas` (plus a small slack for
    // the finalize pass). This pin therefore guards against SUPER-LINEARITY
    // (two or more copies per delta — the shape that produced ~630 MB of churn
    // on a 10 000-delta stream); the per-stream MEMORY bound is the queue
    // lane's cap (its acceptance: noop / read-role / read-content <= 60 MB),
    // not this counter.
    const COPY_SLACK: u64 = 8;
    assert!(
        with_handler <= baseline + 256 + COPY_SLACK,
        "a handler may add at most one accumulator copy per delta: \
         baseline={baseline} with_handler={with_handler}"
    );
    assert!(
        with_handler - baseline <= 256 + COPY_SLACK,
        "the handler leg may not exceed the no-handler leg by more than one copy per delta: \
         baseline={baseline} with_handler={with_handler}"
    );
    // 4x the deltas: the growth must stay within one copy per extra delta.
    assert!(
        with_handler <= with_handler_short + (256 - 64) + COPY_SLACK,
        "the handler leg's copies must not grow faster than one per delta: \
         short={with_handler_short} long={with_handler}"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

// ---------------------------------------------------------------------------
// `toolcall_delta`: the O(delta) arguments patch
// ---------------------------------------------------------------------------

/// The provider mirrors PARSED partial JSON into the tool-call block, so the
/// delta's raw fragment cannot rebuild the block in JS. The host ships the
/// O(delta) `patch` (`arguments[key] += tail`) whenever the change is exactly
/// one string argument gaining a suffix, and the whole parsed block otherwise —
/// and the JS live object is deep-equal to the reference construction at EVERY
/// dispatch, patch or block.
#[tokio::test]
async fn message_update_tool_argument_deltas_patch_or_fall_back_exactly() {
    /// Expected HOST route per dispatch, aligned with
    /// `message_update_stream_shape` (Delta for every non-terminal variant,
    /// Reference for `done`/`error`) and with the helper the wire carries:
    /// "delta" = a delta line with no helper (only the stream's `start`, whose
    /// initial content rides `message.content`), "block" = a delta line
    /// carrying the whole block, "patch" = a delta line carrying the O(delta)
    /// `arguments` patch, "reference" = the full reference payload.
    struct Script {
        events: Vec<AssistantMessageEvent>,
        routes: Vec<&'static str>,
    }
    let tool_block = |arguments: BTreeMap<String, serde_json::Value>| {
        ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })
    };
    let mut arguments: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut script = Script {
        events: Vec::new(),
        routes: Vec::new(),
    };
    let partial = |arguments: &BTreeMap<String, serde_json::Value>| {
        Arc::new(assistant(
            vec![tool_block(arguments.clone())],
            StopReason::Pending,
        ))
    };
    script.events.push(AssistantMessageEvent::Start {
        partial: Arc::new(assistant(Vec::new(), StopReason::Pending)),
    });
    // `Start` classifies as a DELTA line: its wire message carries the initial
    // partial's content, so it ships no `block` helper at all (only the
    // terminal `done` below is the reference payload).
    script.routes.push("delta");
    script.events.push(AssistantMessageEvent::ToolCallStart {
        content_index: 0,
        partial: partial(&arguments),
    });
    script.routes.push("block");
    // Phase 1: one string argument grows, 64 fragments. The first ships the
    // block (the baseline is empty), every later one the patch.
    let mut data = String::new();
    for index in 0..64 {
        data.push_str("ab");
        arguments.insert("data".into(), serde_json::json!(data));
        script.events.push(AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: "ab".into(),
            partial: partial(&arguments),
        });
        script
            .routes
            .push(if index == 0 { "block" } else { "patch" });
    }
    // Phase 2: a second key appears — a key the JS side does not hold is not an
    // extension of one string value, so the whole block must ship.
    arguments.insert("mode".into(), serde_json::json!("r"));
    script.events.push(AssistantMessageEvent::ToolCallDelta {
        content_index: 0,
        delta: "\"mode\":\"r\"".into(),
        partial: partial(&arguments),
    });
    script.routes.push("block");
    // Phase 3: the second key grows — patches again.
    for _ in 0..16 {
        let grown = format!("{}w", arguments["mode"].as_str().unwrap_or_default());
        arguments.insert("mode".into(), serde_json::json!(grown));
        script.events.push(AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: "w".into(),
            partial: partial(&arguments),
        });
        script.routes.push("patch");
    }
    // Phase 4: a NUMERIC value moves — not a string extension, so every one of
    // these ships the block (a patch must never misdescribe a value).
    for step in 0..4 {
        let count = step + 1;
        arguments.insert("count".into(), serde_json::json!(count));
        script.events.push(AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: format!("{count}"),
            partial: partial(&arguments),
        });
        script.routes.push("block");
    }
    // Phase 5: escapes and non-ASCII. The patch is computed on the PARSED
    // values, so the wire's tail carries real characters (JSON-escaped on the
    // wire) and the live object stays reference-equal.
    arguments.insert("note".into(), serde_json::json!("line1\né"));
    script.events.push(AssistantMessageEvent::ToolCallDelta {
        content_index: 0,
        delta: "\"line1\\né\"".into(),
        partial: partial(&arguments),
    });
    script.routes.push("block");
    for _ in 0..16 {
        let grown = format!(
            "{}é\u{1F600}",
            arguments["note"].as_str().unwrap_or_default()
        );
        arguments.insert("note".into(), serde_json::json!(grown));
        script.events.push(AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: "\\u00e9\\ud83d\\ude00".into(),
            partial: partial(&arguments),
        });
        script.routes.push("patch");
    }
    let finalized = match tool_block(arguments.clone()) {
        ContentBlock::ToolCall(call) => call,
        _ => unreachable!("tool_block returns a tool call"),
    };
    script.events.push(AssistantMessageEvent::ToolCallEnd {
        content_index: 0,
        tool_call: finalized,
        partial: partial(&arguments),
    });
    script.routes.push("block");
    script.events.push(AssistantMessageEventStream::done_event(
        TerminalReason::ToolUse,
        assistant(vec![tool_block(arguments.clone())], StopReason::ToolUse),
    ));
    script.routes.push("reference");

    let source = r#"
        export default function (pi) {
            globalThis.__seen = [];
            globalThis.__hostCalls = 0;
            globalThis.__callsAtFirstDispatch = -1;
            globalThis.__callsAfterFirstDispatch = -1;
            globalThis.__exportsAfterFirstDispatch = -1;
            globalThis.__callsAtSecondDispatch = -1;
            globalThis.__exportsAtSecondDispatch = -1;
            globalThis.__exportsPerDispatch = [];
            globalThis.__exportCalls = 0;
            for (const name of Object.keys(__pir_host)) {
                const fn = __pir_host[name];
                if (typeof fn !== "function") continue;
                // Same exclusions as the drift fixture above: the per-dispatch
                // ctx description source and host side effects.
                if (name === "getContextJson" || name === "notify" || name === "setStatus") continue;
                try {
                    __pir_host[name] = function (...args) {
                        globalThis.__hostCalls += 1;
                        return fn.apply(__pir_host, args);
                    };
                } catch {
                    /* non-writable */
                }
            }
            const exportRegistry = globalThis.__pir_exportRegistryJson;
            globalThis.__pir_exportRegistryJson = function (...args) {
                globalThis.__exportCalls += 1;
                return exportRegistry.apply(globalThis, args);
            };
            const entry = globalThis.__pir_dispatchEventReadOnly;
            globalThis.__pir_dispatchEventReadOnly = async function (eventName, payloadJson) {
                const isUpdate = eventName === "message_update";
                if (isUpdate && globalThis.__callsAtSecondDispatch < 0
                    && globalThis.__callsAfterFirstDispatch >= 0) {
                    // Baseline for "after the first dispatch's HOST-side tail"
                    // (see the drift fixture): that tail is the one refresh point
                    // which publishes the load-time registration.
                    globalThis.__callsAtSecondDispatch = globalThis.__hostCalls;
                    globalThis.__exportsAtSecondDispatch = globalThis.__exportCalls;
                }
                if (isUpdate && globalThis.__callsAtFirstDispatch < 0) {
                    globalThis.__callsAtFirstDispatch = globalThis.__hostCalls;
                }
                if (isUpdate) {
                    globalThis.__exportsPerDispatch.push(globalThis.__exportCalls);
                    const payload = JSON.parse(payloadJson);
                    const assistantEvent = payload.assistantMessageEvent;
                    const hasPatch = assistantEvent
                        && Object.prototype.hasOwnProperty.call(assistantEvent, "patch");
                    const hasBlock = assistantEvent
                        && Object.prototype.hasOwnProperty.call(assistantEvent, "block");
                    // The HOST route of this dispatch (`MessageUpdateStreamShape`
                    // + the helper the wire carries):
                    //   "patch"     -> delta line with the O(delta) arguments patch
                    //   "block"     -> delta line carrying the whole block
                    //   "delta"     -> delta line with no helper (the stream's
                    //                  `start`: its initial content rides
                    //                  `message.content`, so no block is needed)
                    //   "reference" -> the full reference payload (terminal events)
                    globalThis.__seen.push({
                        route: payload.__pirDelta === true
                            ? (hasPatch ? "patch" : (hasBlock ? "block" : "delta"))
                            : "reference",
                    });
                }
                const out = await entry.call(globalThis, eventName, payloadJson);
                if (isUpdate && globalThis.__callsAfterFirstDispatch < 0) {
                    // Baseline AFTER the first dispatch (see the drift fixture):
                    // the load-time registry publication lands in its tail.
                    globalThis.__callsAfterFirstDispatch = globalThis.__hostCalls;
                    globalThis.__exportsAfterFirstDispatch = globalThis.__exportCalls;
                }
                return out;
            };
            globalThis.__live = [];
            globalThis.__leaks = 0;
            pi.on("message_update", (event) => {
                const assistantEvent = event.assistantMessageEvent;
                if (assistantEvent
                    && (Object.prototype.hasOwnProperty.call(assistantEvent, "patch")
                        || Object.prototype.hasOwnProperty.call(assistantEvent, "block"))) {
                    globalThis.__leaks += 1;
                }
                globalThis.__live.push(JSON.stringify(event.message));
            });
        }
        "#;
    let (temp, runtime) = live_message_runtime("tool-argument-patch", source).await;
    let options = LoopOptions {
        event_stream: Some(scripted_stream(script.events.clone())),
        ..LoopOptions::default()
    };
    let deps = LoopDeps {
        extensions: Some(runtime.clone()),
        // One turn: the scripted stream answers ONCE. The turn's ToolUse
        // message is the terminal this test inspects; the cap's own terminal
        // (the run cannot continue on an exhausted script) comes after it.
        max_turns: 1,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    let provider_message = result
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) if assistant.stop_reason != StopReason::Error => {
                Some(assistant)
            }
            _ => None,
        })
        .expect("the turn assistant message");
    assert_eq!(provider_message.stop_reason, StopReason::ToolUse);
    assert_eq!(
        provider_message.content,
        vec![tool_block(arguments.clone())],
        "the streamed message is the provider's"
    );

    let probe = runtime
        .eval_json(
            "JSON.stringify({seen: globalThis.__seen, live: globalThis.__live, \
             leaks: globalThis.__leaks, hostCalls: globalThis.__hostCalls, \
             callsAtFirstDispatch: globalThis.__callsAtFirstDispatch, \
             callsAfterFirstDispatch: globalThis.__callsAfterFirstDispatch, \
             exportCalls: globalThis.__exportCalls, \
             exportsAfterFirstDispatch: globalThis.__exportsAfterFirstDispatch, \
             callsAtSecondDispatch: globalThis.__callsAtSecondDispatch, \
             exportsAtSecondDispatch: globalThis.__exportsAtSecondDispatch, \
             exportsPerDispatch: globalThis.__exportsPerDispatch, \
             fallbacks: globalThis.__pir_messageUpdateFallbacks ?? 0})",
        )
        .await
        .expect("probe");
    assert_eq!(
        probe["hostCalls"], probe["callsAfterFirstDispatch"],
        "no host call may happen after the first dispatch (lazy `ctx.tools`, no per-delta export): \
         {probe:?}"
    );
    // Exactly one registry export: the load-time publication (see the drift
    // fixture's comment for why the pin is run-wide and not window-scoped).
    assert!(
        probe["exportCalls"].as_u64().unwrap_or_default() <= 1,
        "at most one registry export (the load-time publication) may happen in the run; a \
         per-delta export would grow with the dispatch count: {probe:?}"
    );
    let export_starts: Vec<u64> = probe["exportsPerDispatch"]
        .as_array()
        .expect("per-dispatch exports")
        .iter()
        .map(|value| value.as_u64().unwrap_or_default())
        .collect();
    let growing_windows = export_starts
        .windows(2)
        .filter(|pair| pair[1] > pair[0])
        .count();
    let tail_growth = probe["exportCalls"].as_u64().unwrap_or_default()
        > export_starts.last().copied().unwrap_or_default();
    // WHERE the single publication landed, so a failure names its trigger
    // instead of only its count: the load path itself (published before the
    // first dispatch), the host-side tail of one dispatch (the refresh point
    // that first observed the dirty registry), or the run's tail.
    let located = if export_starts.first().copied().unwrap_or_default() > 0 {
        "the load path (published before the first dispatch)".to_owned()
    } else if let Some(window) = export_starts.windows(2).position(|pair| pair[1] > pair[0]) {
        format!("the host-side tail of dispatch {window}")
    } else if tail_growth {
        "the run's tail (after the last message_update dispatch)".to_owned()
    } else {
        "nowhere".to_owned()
    };
    assert!(
        growing_windows + usize::from(tail_growth) <= 1,
        "at most one window may carry the single load-time publication (trigger located at \
         {located}): {probe:?}"
    );
    assert_eq!(probe["fallbacks"].as_u64(), Some(0));
    assert_eq!(
        probe["leaks"].as_u64(),
        Some(0),
        "the internal block/patch helpers never reach a handler"
    );
    let live = probe["live"].as_array().expect("live array");
    let seen = probe["seen"].as_array().expect("seen array");
    assert_eq!(
        live.len(),
        script.events.len(),
        "one dispatch per streamed event"
    );
    let mut patches = 0usize;
    let mut blocks = 0usize;
    let mut deltas = 0usize;
    let mut references = 0usize;
    for (index, ((entry, message), expected)) in seen
        .iter()
        .zip(live.iter())
        .zip(script.routes.iter())
        .enumerate()
    {
        assert_eq!(
            entry["route"].as_str(),
            Some(*expected),
            "entry {index}: the host's patch/block/reference route"
        );
        match *expected {
            "patch" => patches += 1,
            "block" => blocks += 1,
            "delta" => deltas += 1,
            _ => references += 1,
        }
        let reference = pir::agent::agent_loop::message_update_payload(&script.events[index]);
        let live_message: serde_json::Value =
            serde_json::from_str(message.as_str().expect("live message json"))
                .expect("live message parses");
        assert_message_json_equal(
            &live_message,
            &reference["message"],
            &format!("entry {index} ({})", entry["route"]),
        );
    }
    // 63 (phase 1) + 16 (phase 3) + 16 (phase 5) growing-string deltas.
    assert_eq!(
        patches, 95,
        "every single-string extension must ride the O(delta) patch"
    );
    // The two block-opening events, the first delta of each new argument shape
    // (phase 2's new key, phase 4's first numeric key), phase 4's four numeric
    // steps and the finalizing `toolcall_end`.
    assert_eq!(blocks, 9, "only non-extension changes ship the whole block");
    assert_eq!(
        deltas, 1,
        "the stream's `start` is a delta line whose message carries the initial content"
    );
    assert_eq!(references, 1, "the terminal event is the reference payload");
    let _ = std::fs::remove_dir_all(&temp);
}

// ---------------------------------------------------------------------------
// Degenerate-loop guard: identical tool executions stop the run
// ---------------------------------------------------------------------------

/// A tool whose result changes every execution (progress), for the negative
/// cases of the guard.
struct ProgressTool {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for ProgressTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "returns a new result every run"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { ToolResult::ok(format!("result-{n}")) })
    }
}

/// The measured runaway shape: the model re-issues the SAME single tool call
/// and the tool returns the SAME model-visible result every turn. Without the
/// guard this runs until `max_turns` (the real leg did 511 turns / 17.4M
/// tokens).
///
/// Nudge-then-stop: the FIRST trip after [`REPEAT_STREAK_STOP`] = 20 turns
/// injects a steering nudge and lets the model try again; the model here
/// ignores it and keeps repeating, so the SECOND trip stops the run. The
/// nudge is a real transcript message visible to the next request.
#[tokio::test]
async fn identical_tool_execution_loop_is_stopped() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let contexts: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let contexts_for_stream = Arc::clone(&contexts);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool)); // always "tool-result"
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            contexts_for_stream
                .lock()
                .unwrap()
                .push(request.context.messages.clone());
            Box::pin(async move { assistant(vec![tool_call()], StopReason::ToolUse) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 100,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    // 20 turns to the first trip, one nudge, then 20 more to the stop.
    assert_eq!(
        result.turns, 40,
        "the guard nudges once, then stops on the second trip"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        40,
        "forty model requests, not a hundred"
    );
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert!(
        result
            .terminal
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("identical tool calls"),
        "the terminal message explains the stop: {:?}",
        result.terminal.error_message
    );
    assert!(
        result.tool_results.len() == 40,
        "each stopped turn still carries its tool result: {}",
        result.tool_results.len()
    );
    // The nudge is in the transcript and reached the request after the first
    // trip (request index 20 is the first after the 20-turn streak).
    let contexts = contexts.lock().unwrap();
    let nudge = |messages: &[Message]| {
        messages.iter().any(|message| match message {
            Message::User(user) => user.content.iter().any(|block| match block {
                ContentBlock::Text(text) => text.text.contains("same tool call"),
                _ => false,
            }),
            _ => false,
        })
    };
    assert!(
        !nudge(&contexts[19]),
        "the first twenty requests carry no nudge"
    );
    assert!(
        nudge(&contexts[20]),
        "the request after the first trip carries the nudge"
    );
}

/// A nudge that WORKS ends the run normally: the model changes approach after
/// the first trip (a final text answer), so no stop terminal is produced.
#[tokio::test]
async fn a_nudge_that_changes_the_approach_ends_the_run_normally() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n < 20 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assistant(vec![text("changed approach")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 100,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        result.turns, 21,
        "20 tool turns, the nudge, then the answer"
    );
    assert_eq!(result.terminal.content, vec![text("changed approach")]);
    assert_ne!(result.terminal.stop_reason, StopReason::Error);
}

/// A result that changes every turn is progress: the guard must not trip even
/// when the call itself is identical for more than twelve turns.
#[tokio::test]
async fn a_changing_tool_result_does_not_trip_the_guard() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ProgressTool {
        runs: Arc::clone(&runs),
    }));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n < 15 {
                    assistant(vec![tool_call()], StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 100,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 16, "15 tool turns plus the final text turn");
    assert_eq!(result.terminal.content, vec![text("final")]);
    assert_eq!(runs.load(Ordering::SeqCst), 15);
}

/// Different arguments with the same result are not a repeat: the digest
/// includes the canonical call arguments. This alternates two calls for 15
/// turns and must reach the final text normally.
#[tokio::test]
async fn different_arguments_do_not_trip_the_guard() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n < 15 {
                    let mut call = match tool_call() {
                        ContentBlock::ToolCall(call) => call,
                        _ => unreachable!(),
                    };
                    call.arguments
                        .insert("n".to_owned(), serde_json::json!(n % 2));
                    assistant(vec![ContentBlock::ToolCall(call)], StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 100,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 16);
    assert_eq!(result.terminal.content, vec![text("final")]);
}

/// A text-only turn between identical tool calls breaks the "consecutive"
/// relation: the guard must reset its streak, so 10 + (text) + 10 identical
/// calls never trips (a false "20 in a row" would end the run).
#[tokio::test]
async fn a_text_only_turn_resets_the_identical_call_streak() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let injected = Arc::new(AtomicUsize::new(0));
    let injected_for_follow_up = Arc::clone(&injected);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match n {
                    10 => assistant(vec![text("pausing")], StopReason::Stop),
                    // total 22: 10 tool turns, text, 10 tool turns, final text.
                    0..=9 | 11..=20 => assistant(vec![tool_call()], StopReason::ToolUse),
                    _ => assistant(vec![text("final")], StopReason::Stop),
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 100,
        follow_up: Some(Box::new(move || {
            let injected = Arc::clone(&injected_for_follow_up);
            Box::pin(async move {
                if injected.fetch_add(1, Ordering::SeqCst) == 0 {
                    vec!["continue".to_owned().into()]
                } else {
                    Vec::new()
                }
            })
        })),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        result.terminal.content,
        vec![text("final")],
        "the guard must not count across the text-only turn: {:?}",
        result.terminal.error_message
    );
    assert_eq!(result.turns, 22);
    // The provider is polled on every no-tool turn (text turn -> "continue",
    // final turn -> none); only the first call injects.
    assert_eq!(injected.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// Agent-level turn retry (upstream `_prepareRetry` / `continue`)
// ---------------------------------------------------------------------------

/// A fast policy so tests never pay the 2 s upstream base delay.
fn fast_retry_policy(max_retries: u32) -> pir::ai::util::AgentRetryPolicy {
    pir::ai::util::AgentRetryPolicy {
        max_retries,
        base_delay_ms: 0,
        max_agent_delay_ms: 0,
    }
}

fn error_assistant(error: &str) -> AssistantMessage {
    let mut message = assistant(Vec::new(), StopReason::Error);
    message.error_message = Some(error.to_owned());
    message
}

/// The terminal message a provider adapter emits for `error`, built by the
/// SAME seam the adapters use (`ai::api::terminal_error_message`): the real
/// flattened text PLUS the structural retry verdict the loop consumes.
fn classified_error(error: &pir::ai::error::AiError) -> AssistantMessage {
    pir::ai::api::terminal_error_message(&test_model(), error, None)
}

/// The OpenAI Responses / Azure variant: the user-facing text is prefixed by
/// the adapter, but the structural verdict is independent of the prefix.
fn prefixed_classified_error(error: &pir::ai::error::AiError, prefix: &str) -> AssistantMessage {
    pir::ai::api::prefixed_terminal_error_message(&test_model(), error, prefix, None)
}

/// P1: a genuine MID-STREAM failure (the stream ends without a terminal
/// event, so the transport never rendered a verdict) is classified
/// `RetryableTurn` and the turn is re-issued instead of ending the run. The
/// failed attempt never reaches the retried request's history, and each
/// attempt's usage counts exactly once.
#[tokio::test]
async fn a_retryable_error_reissues_the_turn_without_the_failed_attempt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_stream = Arc::clone(&requests);
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            requests_for_stream
                .lock()
                .unwrap()
                .push(request.context.messages.clone());
            Box::pin(async move {
                if n == 0 {
                    let mut failure =
                        classified_error(&pir::ai::error::AiError::StreamNotTerminated);
                    failure.usage.total_tokens = 7;
                    failure
                } else {
                    let mut recovered = assistant(vec![text("recovered")], StopReason::Stop);
                    recovered.usage.total_tokens = 11;
                    recovered
                }
            })
        }),
        retry: Some(fast_retry_policy(3)),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "exactly two requests: the failure and the retry"
    );
    assert_eq!(result.terminal.content, vec![text("recovered")]);
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert_eq!(result.turns, 1, "a retry is the SAME turn, not a new one");
    assert_eq!(
        result.usage_total, 18,
        "each attempt's usage counts exactly once"
    );
    assert!(
        result.terminal.diagnostics.is_empty(),
        "the internal retry-class carrier must not leak into the transcript"
    );
    let requests = requests.lock().unwrap();
    assert!(
        !requests[1].iter().any(
            |message| matches!(message, Message::Assistant(a) if a.stop_reason == StopReason::Error)
        ),
        "the failed attempt must not appear in the retried request: {:?}",
        requests[1]
    );
    assert!(
        !result.context.messages.iter().any(
            |message| matches!(message, Message::Assistant(a) if a.stop_reason == StopReason::Error)
        ),
        "the failed attempt must not enter the transcript"
    );
    assert_eq!(
        result
            .new_messages
            .iter()
            .filter(|message| matches!(message, Message::Assistant(_)))
            .count(),
        1,
        "only the successful attempt is produced"
    );
}

/// The retry budget is bounded: a provider that keeps failing with a
/// retryable mid-stream failure ends the run with the terminal error after
/// `max_retries` re-issues.
#[tokio::test]
async fn retry_budget_is_bounded_and_the_final_error_is_terminal() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: Some(fast_retry_policy(2)),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the initial attempt plus two retries"
    );
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("stream ended without a terminal event"),
        "the final provider error is what the host reports"
    );
}

/// Agent-level retry lifecycle (upstream `auto_retry_start` / `auto_retry_end`):
/// a recovered turn reports exactly one start and one successful end, with the
/// upstream attempt/maxAttempts/delayMs/errorMessage fields.
#[tokio::test]
async fn agent_level_retry_reports_start_and_success_end() {
    use pir::agent::agent_loop::{AgentRetryEvent, AgentRetryEventSink};

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let events: Arc<Mutex<Vec<AgentRetryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: AgentRetryEventSink = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    classified_error(&pir::ai::error::AiError::StreamNotTerminated)
                } else {
                    assistant(vec![text("recovered")], StopReason::Stop)
                }
            })
        }),
        retry: Some(fast_retry_policy(3)),
        retry_events: Some(sink),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert!(!result.will_retry, "a recovered run has no pending retry");
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            AgentRetryEvent::Start {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 0,
                error_message: "stream ended without a terminal event".into(),
            },
            AgentRetryEvent::End {
                success: true,
                attempt: 1,
                final_error: None,
            },
        ]
    );
}

/// Exhausting the agent retry budget closes the lifecycle with a failed end
/// whose `finalError` is the terminal provider error (upstream
/// `_handlePostAgentRun`).
#[tokio::test]
async fn agent_level_retry_reports_failure_when_the_budget_is_spent() {
    use pir::agent::agent_loop::{AgentRetryEvent, AgentRetryEventSink};

    let events: Arc<Mutex<Vec<AgentRetryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: AgentRetryEventSink = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            Box::pin(async move { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: Some(fast_retry_policy(2)),
        retry_events: Some(sink),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert!(!result.will_retry, "an exhausted budget cannot retry again");
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            AgentRetryEvent::Start {
                attempt: 1,
                max_attempts: 2,
                delay_ms: 0,
                error_message: "stream ended without a terminal event".into(),
            },
            AgentRetryEvent::Start {
                attempt: 2,
                max_attempts: 2,
                delay_ms: 0,
                error_message: "stream ended without a terminal event".into(),
            },
            AgentRetryEvent::End {
                success: false,
                attempt: 2,
                final_error: Some("stream ended without a terminal event".into()),
            },
        ]
    );
}

/// A cancel during the backoff closes the lifecycle with
/// `"Retry cancelled"` (upstream `_finishCancelledRetry`) and ends the run as
/// an abort.
#[tokio::test]
async fn agent_level_retry_cancelled_during_backoff_reports_retry_cancelled() {
    use pir::agent::agent_loop::{AgentRetryEvent, AgentRetryEventSink};

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let events: Arc<Mutex<Vec<AgentRetryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: AgentRetryEventSink = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: Some(pir::ai::util::AgentRetryPolicy {
            max_retries: 3,
            base_delay_ms: 10_000,
            max_agent_delay_ms: 10_000,
        }),
        retry_events: Some(sink),
        ..LoopDeps::default()
    };
    let signal = AbortSignal::new();
    let abort = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        abort.abort();
    });

    let result = run_with_options(
        deps,
        Context::default(),
        Some(signal),
        LoopOptions::default(),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the retry never ran");
    assert_eq!(result.terminal.stop_reason, StopReason::Aborted);
    assert!(!result.will_retry);
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            AgentRetryEvent::Start {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 10_000,
                error_message: "stream ended without a terminal event".into(),
            },
            AgentRetryEvent::End {
                success: false,
                attempt: 1,
                final_error: Some("Retry cancelled".into()),
            },
        ]
    );
}

/// A successful retry closes its lifecycle exactly once AND restores the
/// retry budget for later turns. The run state used to keep the spent counter,
/// so a later turn could never retry again and every successful turn
/// re-reported `auto_retry_end`.
#[tokio::test]
async fn a_successful_retry_resets_the_budget_for_later_turns() {
    use pir::agent::agent_loop::{AgentRetryEvent, AgentRetryEventSink};

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let events: Arc<Mutex<Vec<AgentRetryEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: AgentRetryEventSink = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let injected = Arc::new(AtomicUsize::new(0));
    let injected_for_follow_up = Arc::clone(&injected);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match n {
                    // Turn 1: one retryable failure, then a recovered response.
                    0 | 2 => classified_error(&pir::ai::error::AiError::StreamNotTerminated),
                    1 => assistant(vec![text("first")], StopReason::Stop),
                    _ => assistant(vec![text("second")], StopReason::Stop),
                }
            })
        }),
        retry: Some(fast_retry_policy(1)),
        retry_events: Some(sink),
        max_turns: 10,
        follow_up: Some(Box::new(move || {
            let injected = Arc::clone(&injected_for_follow_up);
            Box::pin(async move {
                if injected.fetch_add(1, Ordering::SeqCst) == 0 {
                    vec!["continue".to_owned().into()]
                } else {
                    Vec::new()
                }
            })
        })),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.content, vec![text("second")]);
    assert_eq!(result.turns, 2);
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            AgentRetryEvent::Start {
                attempt: 1,
                max_attempts: 1,
                delay_ms: 0,
                error_message: "stream ended without a terminal event".into(),
            },
            AgentRetryEvent::End {
                success: true,
                attempt: 1,
                final_error: None,
            },
            AgentRetryEvent::Start {
                attempt: 1,
                max_attempts: 1,
                delay_ms: 0,
                error_message: "stream ended without a terminal event".into(),
            },
            AgentRetryEvent::End {
                success: true,
                attempt: 1,
                final_error: None,
            },
        ]
    );
}

/// `agent_end.willRetry` is read from the shared run result, not hardcoded;
/// a normal terminal run serializes `false` and a result whose shared retry
/// state still permits a re-issue serializes `true`.
#[tokio::test]
async fn agent_end_will_retry_is_the_shared_run_state() {
    use pir::agent::agent_loop::LoopResult;

    // Real run whose terminal error already spent the budget.
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            Box::pin(async move { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: Some(fast_retry_policy(1)),
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    let frame = pir::agent_session::agent_event_to_json(&AgentEvent::AgentEnd {
        result: Arc::new(result),
    });
    assert_eq!(frame["type"], serde_json::json!("agent_end"));
    assert_eq!(frame["willRetry"], serde_json::json!(false));

    // A result carrying the shared "retry still pending" state is serialized
    // verbatim, proving the field is not hardcoded.
    let pending = Arc::new(LoopResult {
        terminal: error_assistant("overloaded"),
        context: Arc::new(Context::default()),
        tool_results: Vec::new(),
        new_messages: Vec::new(),
        preamble: Vec::new(),
        turns: 0,
        aborted: false,
        usage_total: 0,
        will_retry: true,
    });
    let frame = pir::agent_session::agent_event_to_json(&AgentEvent::AgentEnd { result: pending });
    assert_eq!(frame["willRetry"], serde_json::json!(true));
}

/// A NON-retryable verdict keeps the historical hard exit: one request, the
/// provider's own terminal.
#[tokio::test]
async fn a_non_retryable_error_terminates_without_a_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                classified_error(&pir::ai::error::AiError::NonRetryable(
                    "invalid request: unknown field".into(),
                ))
            })
        }),
        retry: Some(fast_retry_policy(3)),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("non-retryable provider error: invalid request: unknown field")
    );
}

/// An abort is terminal: it is never retried, even with a budget available.
#[tokio::test]
async fn an_aborted_turn_is_never_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let mut message = assistant(Vec::new(), StopReason::Aborted);
                message.error_message = Some("Request was aborted".into());
                message
            })
        }),
        retry: Some(fast_retry_policy(3)),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no retry of an abort");
    assert_eq!(result.terminal.stop_reason, StopReason::Aborted);
}

/// A genuine mid-stream transport failure AFTER the response headers (the
/// transport rendered no verdict and reports a body read failure) is still
/// retried: its structural class is `RetryableTurn`.
#[tokio::test]
async fn a_mid_stream_body_failure_still_retries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    classified_error(&pir::ai::error::AiError::Http(
                        "HTTP body idle timeout after 30s (httpIdleTimeoutMs)".into(),
                    ))
                } else {
                    assistant(vec![text("recovered")], StopReason::Stop)
                }
            })
        }),
        retry: Some(fast_retry_policy(3)),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the mid-stream drop is retried"
    );
    assert_eq!(result.turns, 1);
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
}

/// A transport-EXHAUSTED verdict and an adapter-prefixed non-retryable verdict
/// are NEVER re-issued at the agent level, no matter what their flattened text
/// says ("connection reset", "503", "overloaded" are all retryable-looking).
/// Both cases are built by the real adapter seam, including the OpenAI
/// Responses prefix, and each must produce exactly ONE agent-level round.
#[tokio::test]
async fn a_transport_exhausted_failure_is_not_reissued() {
    let cases = [
        (
            "exhausted transport budget (inner text lost the 'retry exhausted' marker)",
            classified_error(&pir::ai::error::AiError::RetryExhausted {
                attempts: 3,
                message: "connection reset by peer".into(),
            }),
            "connection reset by peer",
        ),
        (
            "adapter-prefixed non-retryable status (text contains 503/overloaded)",
            prefixed_classified_error(
                &pir::ai::error::AiError::NonRetryable("HTTP 503: overloaded".into()),
                "OpenAI API error: ",
            ),
            "OpenAI API error: non-retryable provider error: HTTP 503: overloaded",
        ),
    ];
    for (label, failure, expected_text) in cases {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_stream = Arc::clone(&calls);
        let failure = failure.clone();
        let deps = LoopDeps {
            stream: Box::new(move |_request| {
                calls_for_stream.fetch_add(1, Ordering::SeqCst);
                let failure = failure.clone();
                Box::pin(async move { failure })
            }),
            retry: Some(fast_retry_policy(3)),
            ..LoopDeps::default()
        };
        let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{label}: the transport verdict is not retried again"
        );
        assert_eq!(result.turns, 1, "{label}: exactly one agent-level round");
        assert_eq!(result.terminal.stop_reason, StopReason::Error);
        assert_eq!(
            result.terminal.error_message.as_deref(),
            Some(expected_text),
            "{label}: the user-facing text is unchanged"
        );
        assert!(
            result.terminal.diagnostics.is_empty(),
            "{label}: the internal retry-class carrier must not leak"
        );
    }
}

// ---------------------------------------------------------------------------
// The retry-class carrier never reaches a consumer frame (CL32 review: D1)
// ---------------------------------------------------------------------------

/// The internal `pir_retry_class` carrier rides the terminal error message
/// until the loop takes it. The terminal `message_update` frame is PUBLIC wire
/// (`--mode json` / RPC stdout via `to_json_event`, the session-event wire, and
/// the JS/native `message_update` handler fan-out all serialize it as-is), so
/// it must not contain the marker. The real error text must survive intact.
///
/// The stream is driven through `event_stream` so the loop emits per-event
/// `StreamEvent` frames (the same path the JSON host and extension runtime
/// consume), and the frames are serialized exactly like those consumers do.
#[tokio::test]
async fn the_terminal_error_frame_never_carries_the_retry_class() {
    let terminal = classified_error(&pir::ai::error::AiError::StreamNotTerminated);
    assert!(
        terminal
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.diag_type == pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
        "precondition: the adapter seam attaches the carrier to the terminal message"
    );
    let terminal_for_stream = terminal.clone();
    let frames: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let frames_for_handler = Arc::clone(&frames);
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: Some(fast_retry_policy(0)),
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        event_stream: Some(Box::new(move |_request| {
            let message = terminal_for_stream.clone();
            Box::pin(async move {
                let stream = AssistantMessageEventStream::new();
                stream.push(AssistantMessageEvent::Start {
                    partial: Arc::new(AssistantMessage::default()),
                });
                stream.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: message,
                });
                stream.end(None);
                stream
            })
        })),
        // Capture the event itself: every public consumer (JSON/RPC stdout,
        // session events, the JS/native `message_update` fan-out) builds its
        // frame from this same value, so the assertions below serialize it
        // through each of those consumers.
        events: Some(Arc::new(move |event| {
            frames_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("stream ended without a terminal event"),
        "the real error text survives the take"
    );
    let frames = frames.lock().unwrap();
    let mut terminal_frames = 0usize;
    for event in frames.iter() {
        let AgentEvent::StreamEvent {
            event: assistant_event,
            ..
        } = event
        else {
            continue;
        };
        if !matches!(assistant_event, AssistantMessageEvent::Error { .. }) {
            continue;
        }
        terminal_frames += 1;
        // `--mode json` / RPC stdout.
        let json_stdout = pir::coding_agent::json::to_json_event(event);
        // Session-event wire.
        let session_wire = pir::agent_session::agent_event_to_json(event);
        // JS / native `message_update` reference payload.
        let extension_reference = pir::agent::agent_loop::message_update_payload(assistant_event);
        // Native (Rust) plugin `message_update` compact payload.
        let native_payload = pir::agent::agent_loop::native_message_update_payload(assistant_event);
        // The actual JS streaming wire (`MessageUpdateDispatcher::enqueue`):
        // terminal events serialize through the reference construction, so
        // this is the exact string the QuickJS actor receives.
        let js_stream_wire = pir::extensions::runtime::message_update_stream_json(
            assistant_event,
            1,
            &mut pir::agent::agent_loop::ToolArgumentBaselines::new(),
        );
        for (label, serialized) in [
            ("json/rpc stdout", json_stdout.to_string()),
            ("session events", session_wire.to_string()),
            ("extension message_update", extension_reference.to_string()),
            ("native message_update", native_payload.to_string()),
            ("js message_update stream wire", js_stream_wire),
        ] {
            assert!(
                !serialized.contains(pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
                "{label}: the internal retry-class carrier leaked to the wire: {serialized}"
            );
            assert!(
                !serialized.contains("retryable_turn") && !serialized.contains("non_retryable"),
                "{label}: no verdict spelling may reach the wire: {serialized}"
            );
            assert!(
                serialized.contains("stream ended without a terminal event"),
                "{label}: the real error text must stay in the frame: {serialized}"
            );
        }
    }
    assert_eq!(
        terminal_frames, 1,
        "exactly one terminal error frame is dispatched"
    );
    // The loop still took the verdict before history/persistence: the
    // transcript copy is carrier-free too.
    assert!(
        result.terminal.diagnostics.is_empty(),
        "the loop consumed the carrier"
    );
}

/// R4 (CL38): the FALLBACK `message_start` strip path
/// (`agent_loop.rs` `consume_stream`, the tail after the stream loop) had no
/// test. It is production-reachable: a provider request that fails before the
/// stream produces its first partial (`Start` or any delta) leaves
/// `message_start_emitted == false`, so the terminal error message becomes the
/// `message_start` payload. That message is the one the provider finalizer
/// attached the internal `pir_retry_class` carrier to, and this emission path
/// is the one place the loop strips it with `take_turn_retry_class` purely for
/// serialization (the verdict itself is taken by the caller from
/// `final_message`).
///
/// The pre-Start failure is exactly the shape here: the stream yields only the
/// terminal `Error` event and ends. The assertions cover the in-process
/// `AgentEvent::StreamStart` frame, the extension `message_start` payload the
/// JS/native hosts receive, and the returned terminal (the real error text
/// survives intact).
#[tokio::test]
async fn a_pre_start_request_failure_emits_a_carrier_free_message_start() {
    let terminal = classified_error(&pir::ai::error::AiError::StreamNotTerminated);
    assert!(
        terminal
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.diag_type == pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
        "precondition: the adapter seam attaches the carrier to the terminal message"
    );
    let terminal_for_stream = terminal.clone();

    // A JS `message_start` handler proves the extension payload face is
    // carrier-free too (the same dispatch every JS/native host drives).
    let fixture = message_end_fixture(
        "fallback-start",
        r#"
        export default function (pi) {
            globalThis.__starts = [];
            pi.on("message_start", (event) => {
                globalThis.__starts.push(JSON.stringify(event.message));
            });
        }
        "#,
    )
    .await;

    let frames: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let frames_for_handler = Arc::clone(&frames);
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: Some(fast_retry_policy(0)),
        extensions: Some(fixture.runtime.clone()),
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        // Only a terminal event: no Start, no delta. The loop must fall back
        // to emitting `message_start` with the terminal message.
        event_stream: Some(Box::new(move |_request| {
            let message = terminal_for_stream.clone();
            Box::pin(async move {
                let stream = AssistantMessageEventStream::new();
                stream.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: message,
                });
                stream.end(None);
                stream
            })
        })),
        events: Some(Arc::new(move |event| {
            frames_for_handler.lock().unwrap().push(event);
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(
        result.terminal.error_message.as_deref(),
        Some("stream ended without a terminal event"),
        "the real error text survives the strip"
    );
    // (a) The in-process `StreamStart` frame carries the terminal message with
    // no carrier.
    // Collect owned clones inside a scope so the mutex guard is released
    // before the async probe below (the guard must not span an await).
    let starts: Vec<AssistantMessage> = {
        let frames = frames.lock().unwrap();
        frames
            .iter()
            .filter_map(|event| match event {
                AgentEvent::StreamStart { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        starts.len(),
        1,
        "exactly one fallback message_start is emitted"
    );
    let start = &starts[0];
    assert_eq!(
        start.error_message.as_deref(),
        Some("stream ended without a terminal event"),
        "the fallback message_start carries the real error text"
    );
    assert!(
        !start
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.diag_type == pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
        "the fallback message_start must not carry the retry-class marker: {:?}",
        start.diagnostics
    );
    assert!(
        !serde_json::to_string(start)
            .expect("message serializes")
            .contains(pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
        "nor serialize it onto any wire"
    );

    // (b) The extension `message_start` payload (JS/native handler face).
    let starts = fixture
        .runtime
        .eval_json("JSON.stringify(globalThis.__starts)")
        .await
        .expect("probe");
    let starts = starts.as_array().expect("array of serialized messages");
    assert_eq!(starts.len(), 1, "one extension message_start: {starts:?}");
    let payload = starts[0].as_str().expect("serialized message");
    assert!(
        !payload.contains(pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
        "the extension message_start payload must not carry the marker: {payload}"
    );
    assert!(
        payload.contains("stream ended without a terminal event"),
        "the extension payload keeps the real error text: {payload}"
    );
}

/// Only an explicit `RetryableTurn` verdict may be re-issued. A message with
/// no structural verdict (a synthetic host/embedder stream) keeps the
/// historical behavior: one request, terminal — even if its text looks
/// transient.
#[tokio::test]
async fn an_unclassified_error_is_not_reissued() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { error_assistant("service temporarily unavailable") })
        }),
        retry: Some(fast_retry_policy(3)),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
}

/// A disabled policy (`settings.retry.enabled: false` / `maxRetries: 0`)
/// restores the pre-fix behavior exactly.
#[tokio::test]
async fn a_disabled_retry_policy_ends_the_run_at_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { classified_error(&pir::ai::error::AiError::StreamNotTerminated) })
        }),
        retry: None,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
}

// ---------------------------------------------------------------------------
// Terminal result at the turn cap (completion honesty)
// ---------------------------------------------------------------------------

/// The turn cap must not end the run silently: it synthesizes an identifiable
/// Error terminal (prefix `TURN_CAP_ERROR_PREFIX`) that is persisted into the
/// transcript like any other terminal message.
#[tokio::test]
async fn reaching_the_turn_cap_produces_a_terminal_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let persisted: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let persisted_for_sink = Arc::clone(&persisted);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ProgressTool {
        runs: Arc::new(AtomicUsize::new(0)),
    }));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { assistant(vec![tool_call()], StopReason::ToolUse) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        persist_message: Some(Arc::new(move |message: Message| {
            let persisted = Arc::clone(&persisted_for_sink);
            Box::pin(async move { persisted.lock().unwrap().push(message) })
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(result.turns, 2, "the cap value is unchanged");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly the cap requests");
    assert_eq!(
        result.terminal.stop_reason,
        StopReason::Error,
        "a capped run is incomplete, not a silent success"
    );
    let error = result.terminal.error_message.as_deref().unwrap_or("");
    assert!(
        error.starts_with(pir::agent::agent_loop::TURN_CAP_ERROR_PREFIX),
        "the terminal names the cap: {error}"
    );
    assert!(
        error.contains("incomplete"),
        "the terminal is honest: {error}"
    );
    // Emitted through the same lifecycle a provider message gets, and recorded
    // as the transcript's last message.
    assert!(
        matches!(
            result.context.messages.last(),
            Some(Message::Assistant(last)) if last.stop_reason == StopReason::Error
        ),
        "the cap terminal is the transcript last message"
    );
    let persisted = persisted.lock().unwrap();
    assert!(
        persisted.iter().any(|message| matches!(
            message,
            Message::Assistant(a)
                if a.error_message.as_deref().is_some_and(|e| e.starts_with(pir::agent::agent_loop::TURN_CAP_ERROR_PREFIX))
        )),
        "the cap terminal is persisted: {persisted:?}"
    );
}

/// A run that FINISHES before the cap is untouched: no synthetic terminal.
#[tokio::test]
async fn finishing_before_the_cap_produces_no_synthetic_terminal() {
    let moment = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&moment);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![text("done")], StopReason::Stop)
                } else {
                    unreachable!("no second request after a final answer")
                }
            })
        }),
        max_turns: 5,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 1);
    assert_eq!(result.terminal.content, vec![text("done")]);
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
}

/// The settings gate: `settings.retry` drives the policy. A disabled block
/// yields no policy; the upstream defaults are honoured otherwise.
#[test]
fn settings_retry_policy_gates_and_defaults() {
    use pir::config::settings::{RetrySettings, SettingsManager};

    let policy = SettingsManager::retry_policy_from(&RetrySettings::default())
        .expect("defaults enable retries");
    assert_eq!(policy.max_retries, 3, "upstream maxRetries default");
    assert_eq!(policy.base_delay_ms, 2_000, "upstream baseDelayMs default");
    assert_eq!(policy.max_agent_delay_ms, 60_000);

    let disabled = RetrySettings {
        enabled: Some(false),
        ..RetrySettings::default()
    };
    assert!(SettingsManager::retry_policy_from(&disabled).is_none());

    let zero = RetrySettings {
        max_retries: Some(0),
        ..RetrySettings::default()
    };
    assert!(SettingsManager::retry_policy_from(&zero).is_none());

    let tuned = RetrySettings {
        max_retries: Some(1),
        base_delay_ms: Some(250),
        ..RetrySettings::default()
    };
    let policy = SettingsManager::retry_policy_from(&tuned).expect("a tuned policy");
    assert_eq!(policy.max_retries, 1);
    assert_eq!(policy.base_delay_ms, 250);
}

// ---------------------------------------------------------------------------
// H4b: queue / `prepare_next_turn` messages join the extension
// `message_start`/`message_end` lifecycle (upstream agent-loop.ts:219-225)
// ---------------------------------------------------------------------------

/// A user message appended by a loop hook (the prepare-next-turn fixture).
fn lifecycle_user_message(value: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text(value)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

/// The user-message texts in one recorded request.
fn recorded_user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

/// A steering message queued before the run is delivered through the SAME
/// `message_start`/`message_end` pair as every other transcript message; a
/// `message_end` replacement is what the provider request, the produced list,
/// and the persistence sink record.
#[tokio::test]
async fn steering_message_end_replacement_applies_to_the_request_and_persistence() {
    let fixture = message_end_fixture(
        "steering-lifecycle",
        r#"
        export default function (pi) {
            globalThis.__user_starts = 0;
            globalThis.__user_ends = 0;
            pi.on("message_start", (event) => {
                if (event.message && event.message.role === "user") globalThis.__user_starts += 1;
            });
            pi.on("message_end", (event) => {
                if (!event.message || event.message.role !== "user") return;
                globalThis.__user_ends += 1;
                const text = (event.message.content || []).map((block) => block.text || "").join("");
                if (text !== "queued") return;
                return { message: { ...event.message, content: [{ type: "text", text: "replaced" }] } };
            });
        }
        "#,
    )
    .await;

    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_stream = Arc::clone(&requests);
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            requests_for_stream
                .lock()
                .unwrap()
                .push(request.context.messages.clone());
            Box::pin(async move { assistant(vec![text("ok")], StopReason::Stop) })
        }),
        tools: Arc::new(ToolRegistry::new()),
        extensions: Some(fixture.runtime.clone()),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let queued = Arc::new(Mutex::new(vec!["queued".to_owned()]));
    let queued_for_steering = Arc::clone(&queued);
    let (sink, persisted) = recording_sink();
    let options = LoopOptions {
        steering: Some(Box::new(move || {
            let queued = Arc::clone(&queued_for_steering);
            Box::pin(async move {
                queued
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(PromptMessage::text)
                    .collect::<Vec<_>>()
            })
        })),
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.turns, 1);

    // The request, the produced list, and the persistence sink all see the
    // replacement; the original never enters any of them.
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one provider request");
        assert_eq!(
            recorded_user_texts(&requests[0]),
            vec!["replaced".to_owned()]
        );
        assert_eq!(
            recorded_user_texts(&result.new_messages),
            vec!["replaced".to_owned()],
            "{:?}",
            result.new_messages
        );
        let persisted = persisted.lock().unwrap();
        assert_eq!(recorded_user_texts(&persisted), vec!["replaced".to_owned()]);
    }

    // Both extension faces fired exactly once for the queued user message.
    let counts = fixture
        .runtime
        .eval_json(
            "JSON.stringify({ starts: globalThis.__user_starts, ends: globalThis.__user_ends })",
        )
        .await
        .expect("probe");
    assert_eq!(counts["starts"], serde_json::json!(1), "one message_start");
    assert_eq!(counts["ends"], serde_json::json!(1), "one message_end");
}

/// `prepare_next_turn.messages` go through the same lifecycle: the extension
/// `message_end` replacement is what the next request and the persistence sink
/// record.
#[tokio::test]
async fn prepare_next_turn_message_end_replacement_applies_to_the_next_request() {
    let fixture = message_end_fixture(
        "prepare-next-turn-lifecycle",
        r#"
        export default function (pi) {
            globalThis.__prepared_starts = 0;
            globalThis.__prepared_ends = 0;
            pi.on("message_start", (event) => {
                if (event.message && event.message.role === "user") globalThis.__prepared_starts += 1;
            });
            pi.on("message_end", (event) => {
                if (!event.message || event.message.role !== "user") return;
                globalThis.__prepared_ends += 1;
                const text = (event.message.content || []).map((block) => block.text || "").join("");
                if (text !== "prepared") return;
                return {
                    message: { ...event.message, content: [{ type: "text", text: "prepared-replaced" }] },
                };
            });
        }
        "#,
    )
    .await;

    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_stream = Arc::clone(&requests);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let index = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            requests_for_stream
                .lock()
                .unwrap()
                .push(request.context.messages.clone());
            Box::pin(async move {
                if index == 0 {
                    assistant(vec![text("first")], StopReason::Stop)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(ToolRegistry::new()),
        extensions: Some(fixture.runtime.clone()),
        finish_turn: Some(Arc::new(|context: FinishTurnContext| {
            Box::pin(async move {
                if context.turn_index == 0 {
                    Some(FinishTurnAction::Continue)
                } else {
                    None
                }
            })
        })),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let (sink, persisted) = recording_sink();
    let options = LoopOptions {
        prepare_next_turn: Some(Arc::new(|_context: Arc<FinishTurnContext>| {
            Box::pin(async move {
                Some(PrepareNextTurnUpdate {
                    messages: vec![lifecycle_user_message("prepared")],
                    ..Default::default()
                })
            })
        })),
        persist_message: Some(sink),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.turns, 2);

    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "two provider requests");
        assert_eq!(
            recorded_user_texts(&requests[1]),
            vec!["prepared-replaced".to_owned()],
            "the second request carries the replacement: {:?}",
            requests[1]
        );
        assert_eq!(
            recorded_user_texts(&result.new_messages),
            vec!["prepared-replaced".to_owned()],
            "{:?}",
            result.new_messages
        );
        let persisted = persisted.lock().unwrap();
        assert_eq!(
            recorded_user_texts(&persisted),
            vec!["prepared-replaced".to_owned()]
        );
    }

    let counts = fixture
        .runtime
        .eval_json(
            "JSON.stringify({ starts: globalThis.__prepared_starts, ends: globalThis.__prepared_ends })",
        )
        .await
        .expect("probe");
    assert_eq!(counts["starts"], serde_json::json!(1), "one message_start");
    assert_eq!(counts["ends"], serde_json::json!(1), "one message_end");
}

/// Control: without any extension handler the queued message keeps today's
/// behavior — the host pair is still emitted, and the original text is what
/// the request, the produced list, and persistence see.
#[tokio::test]
async fn queue_message_without_handlers_keeps_the_original_content() {
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_stream = Arc::clone(&requests);
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            requests_for_stream
                .lock()
                .unwrap()
                .push(request.context.messages.clone());
            Box::pin(async move { assistant(vec![text("ok")], StopReason::Stop) })
        }),
        tools: Arc::new(ToolRegistry::new()),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let queued = Arc::new(Mutex::new(vec!["queued".to_owned()]));
    let queued_for_steering = Arc::clone(&queued);
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let (sink, persisted) = recording_sink();
    let options = LoopOptions {
        steering: Some(Box::new(move || {
            let queued = Arc::clone(&queued_for_steering);
            Box::pin(async move {
                queued
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(PromptMessage::text)
                    .collect::<Vec<_>>()
            })
        })),
        persist_message: Some(sink),
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event)
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(result.turns, 1);
    assert_eq!(
        recorded_user_texts(&requests.lock().unwrap()[0]),
        vec!["queued".to_owned()]
    );
    assert_eq!(
        recorded_user_texts(&result.new_messages),
        vec!["queued".to_owned()]
    );
    assert_eq!(
        recorded_user_texts(&persisted.lock().unwrap()),
        vec!["queued".to_owned()]
    );

    // The host lifecycle pair still brackets the queued message.
    let events = events.lock().unwrap();
    let start = events.iter().position(|event| {
        matches!(
            event,
            AgentEvent::MessageStart {
                message: Message::User(_),
                ..
            }
        )
    });
    let end = events.iter().position(|event| {
        matches!(
            event,
            AgentEvent::MessageEnd {
                message: Message::User(_),
                ..
            }
        )
    });
    let (start, end) = (
        start.expect("user message_start"),
        end.expect("user message_end"),
    );
    assert!(
        start < end,
        "message_start must precede message_end: {start} < {end}"
    );
}

// ---------------------------------------------------------------------------
// Terminating tool batch: the inner tool loop stops (`hasMoreToolCalls`
// false), so the ordinary user follow-up queue must still be polled in the
// same run (upstream agent-loop.ts:263-267). A follow-up queued during the
// batch used to wait for the NEXT prompt because only steering and the
// background inbox were polled on the terminate path.
// ---------------------------------------------------------------------------

/// A tool whose result ends the tool phase (`ToolResult::terminate`).
struct TerminatingTool;

impl AgentTool for TerminatingTool {
    fn name(&self) -> &'static str {
        "finish"
    }
    fn description(&self) -> &'static str {
        "finishes the tool phase"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async {
            ToolResult {
                terminate: true,
                ..ToolResult::ok("finished")
            }
        })
    }
}

#[tokio::test]
async fn a_terminating_tool_batch_still_polls_the_user_follow_up_queue() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_stream = Arc::clone(&requests);

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TerminatingTool));
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            requests_for_stream
                .lock()
                .unwrap()
                .push(request.context.messages.clone());
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "finish-1".into(),
                            name: "finish".into(),
                            arguments: BTreeMap::new(),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        })],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("after the follow-up")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        follow_up: Some(Box::new({
            let polls = Arc::new(AtomicUsize::new(0));
            move || {
                let polls = Arc::clone(&polls);
                Box::pin(async move {
                    if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                        vec![PromptMessage::text("queued follow-up")]
                    } else {
                        Vec::new()
                    }
                })
            }
        })),
        max_turns: 4,
        ..LoopDeps::default()
    };

    let result = run(deps, Context::default(), None).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the terminating batch must still drain the user follow-up queue"
    );
    assert_eq!(result.terminal.content, vec![text("after the follow-up")]);
    let requests = requests.lock().unwrap();
    assert_eq!(
        recorded_user_texts(&requests[1]),
        vec!["queued follow-up".to_owned()],
        "the follow-up must enter the next request's context"
    );
}

// ---------------------------------------------------------------------------
// Turn cap vs. tool-call batches (the 100-session stress shape)
// ---------------------------------------------------------------------------

/// A model message carrying several tool calls executes them as ONE batch and
/// consumes ONE turn. The cap counts model requests, never tool calls: a
/// tool-call budget of 3 would stop this run after the first message's batch.
#[tokio::test]
async fn multiple_tool_calls_in_one_message_consume_one_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n < 2 {
                    let content = (0..3)
                        .map(|i| {
                            ContentBlock::ToolCall(ToolCall {
                                id: format!("call-{n}-{i}"),
                                name: "echo".into(),
                                arguments: BTreeMap::from([("i".to_owned(), serde_json::json!(i))]),
                                thought_signature: None,
                                namespace: None,
                                arguments_error: None,
                            })
                        })
                        .collect();
                    assistant(content, StopReason::ToolUse)
                } else {
                    assistant(vec![text("final")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 3,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        result.turns, 3,
        "two 3-call messages plus the final text are three turns"
    );
    assert_eq!(
        result.tool_results.len(),
        6,
        "every call in both batches executed"
    );
    assert_eq!(result.terminal.content, vec![text("final")]);
}

/// The cap terminal carries the effective limit (not the old hard-coded
/// value) and the operator knob, so a truncated soak run is diagnosable from
/// the message alone. The stable prefix stays the harness contract.
#[tokio::test]
async fn turn_cap_error_names_the_configured_limit_and_the_knob() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ProgressTool {
        runs: Arc::new(AtomicUsize::new(0)),
    }));
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async move { assistant(vec![tool_call()], StopReason::ToolUse) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 3,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 3);
    let error = result.terminal.error_message.as_deref().unwrap_or("");
    assert!(
        error.starts_with(pir::agent::agent_loop::TURN_CAP_ERROR_PREFIX),
        "the stable prefix is preserved: {error}"
    );
    assert!(
        error.contains("3 turns"),
        "the message names the configured limit: {error}"
    );
    assert!(
        error.contains("PIR_MAX_TURNS"),
        "the message names the operator knob: {error}"
    );
    assert!(
        error.contains("not one tool call"),
        "the message states the counting rule: {error}"
    );
}

/// The degenerate-loop guard is independent of the turn cap: raising the cap
/// to the non-interactive default must not let 40 identical single-call turns
/// through. Default stays at the measured 20+20 stop.
#[tokio::test]
async fn repeat_guard_still_stops_runs_under_a_raised_turn_cap() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let deps = LoopDeps {
        stream: Box::new(|_request| {
            Box::pin(async move { assistant(vec![tool_call()], StopReason::ToolUse) })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 10_000,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        result.turns, 40,
        "20 turns trip the guard, one nudge, 20 more stop the run"
    );
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert!(
        result
            .terminal
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("identical tool calls"),
        "the stop still names the repeat guard: {:?}",
        result.terminal.error_message
    );
}
