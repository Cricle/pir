//! Upstream-parity tests for the agent loop hooks and tool execution surface:
//! `finish_turn` (continue/end), `prepare_request` replacement, nested tool
//! calls (`ctx`-level nested execution), `durationMs`/`structuredContent`
//! threading, and the `agent_settled {aborted}` extension event.
//!
//! Fixtures follow the fake-provider patterns of `tests/agent_loop.rs` and
//! `tests/agent_session.rs`; no `~/.pi` configuration is consulted.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{
    run_with_options, AgentEvent, FinishTurnAction, FinishTurnContext, LoopDeps, LoopOptions,
    PrepareNextTurnUpdate, PrepareRequestContext, PrepareRequestUpdate,
};
use pir::agent::nested_tool_calls::MAX_NESTED_DEPTH;
use pir::agent::tool_batch::{ToolExecutionEvent, ToolResult};
use pir::agent::tool_execution::{
    AfterToolCallResult, AgentTool, BoxFuture, ToolContext, ToolExposure, ToolHooks, ToolRegistry,
    ToolResultContentBlock,
};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::session_manager::SessionManager;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fake_model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str) -> ContentBlock {
    tool_call_with_args(id, name, serde_json::json!({}))
}

fn tool_call_with_args(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    let arguments = arguments
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn text_of(message: &Message) -> String {
    match message {
        Message::Assistant(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Message::User(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Message::ToolResult(result) => result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that answers every stream with a scripted message and
/// records the model/reasoning each request arrived with.
struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
    observed: Mutex<Vec<(String, Option<ThinkingLevel>)>>,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model("fake-model")]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.observed
            .lock()
            .unwrap()
            .push((model.id.clone(), options.reasoning));
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> (Arc<ProviderRegistry>, Arc<FakeProvider>) {
    let provider = Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
        observed: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::clone(&provider) as Arc<dyn Provider>);
    (Arc::new(registry), provider)
}

// ---------------------------------------------------------------------------
// finish_turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn finish_turn_continue_guarantees_exactly_one_more_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls_for_hook = hook_calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { assistant(vec![text("no tools")], StopReason::Stop) })
        }),
        finish_turn: Some(Arc::new(move |_turn: FinishTurnContext| {
            let n = hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    Some(FinishTurnAction::Continue)
                } else {
                    None
                }
            })
        })),
        max_turns: 8,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    // The `{action: "continue"}` decision forces one context-only request the
    // scheduling could not satisfy; the second turn's `None` ends the run.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.turns, 2);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
    assert_eq!(text_of(&Message::Assistant(result.terminal)), "no tools");
}

#[tokio::test]
async fn finish_turn_end_stops_the_run_immediately() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { assistant(vec![text("once")], StopReason::Stop) })
        }),
        finish_turn: Some(Arc::new(|_turn: FinishTurnContext| {
            Box::pin(async { Some(FinishTurnAction::End) })
        })),
        max_turns: 8,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.turns, 1);
}

#[tokio::test]
async fn finish_turn_runs_before_turn_end_and_receives_turn_state() {
    // Tool batch turn: the hook must observe the tool results and run before
    // `turn_end` is emitted.
    struct EchoTool;
    impl AgentTool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echoes"
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            Box::pin(async { ToolResult::ok("tool-result") })
        }
    }
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));

    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let order_for_hook = order.clone();
    let order_for_events = order.clone();
    let seen_tool_results = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_for_hook = seen_tool_results.clone();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let mut tools_for_deps = tools;
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: {
            tools_for_deps.register(Arc::new(EchoTool));
            Arc::new(tools_for_deps)
        },
        finish_turn: Some(Arc::new(move |turn: FinishTurnContext| {
            let order = Arc::clone(&order_for_hook);
            let seen = Arc::clone(&seen_for_hook);
            Box::pin(async move {
                order.lock().unwrap().push("finish_turn".into());
                for result in &turn.tool_results {
                    seen.lock()
                        .unwrap()
                        .push(text_of(&Message::ToolResult(result.clone())));
                }
                assert!(
                    !turn.new_messages.is_empty(),
                    "new_messages must be populated"
                );
                None
            })
        })),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event: AgentEvent| {
            if matches!(event, AgentEvent::TurnEnd { .. }) {
                order_for_events.lock().unwrap().push("turn_end".into());
            }
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    let order = order.lock().unwrap();
    let finish_index = order
        .iter()
        .position(|label| label == "finish_turn")
        .expect("finish_turn ran");
    let turn_end_index = order
        .iter()
        .position(|label| label == "turn_end")
        .expect("turn_end emitted");
    assert!(
        finish_index < turn_end_index,
        "finish_turn must run before turn_end: {order:?}"
    );
    // The hook observed the turn's tool results.
    assert_eq!(
        seen_tool_results.lock().unwrap().last().map(String::as_str),
        Some("tool-result")
    );
    assert_eq!(result.tool_results.len(), 1);
}

// ---------------------------------------------------------------------------
// prepare_request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prepare_request_runs_on_first_request_and_replaces_context() {
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls_for_hook = hook_calls.clone();
    let seen_first_request = Arc::new(Mutex::new(false));
    let seen_for_stream = seen_first_request.clone();

    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let seen = seen_for_stream.lock().unwrap();
            if !*seen {
                // The first (conversational) request must already carry the
                // hook's replacement.
                let injected = request
                    .context
                    .messages
                    .iter()
                    .any(|message| text_of(message) == "injected by prepare_request");
                assert!(
                    injected,
                    "prepare_request context replacement missing on first request"
                );
            }
            drop(seen);
            Box::pin(async { assistant(vec![text("ok")], StopReason::Stop) })
        }),
        prepare_request: Some(Arc::new(move |request: PrepareRequestContext| {
            let n = hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            assert!(n <= 1, "one-turn run should call prepare_request once");
            let _ = request;
            let mut context = Context::default();
            context.messages.push(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text("injected by prepare_request")],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }));
            Box::pin(async move {
                Some(PrepareRequestUpdate {
                    // A `prepare_request` context replacement is shared, like
                    // the context the hook was handed.
                    context: Some(Arc::new(context)),
                    ..PrepareRequestUpdate::default()
                })
            })
        })),
        max_turns: 2,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 1);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn prepare_request_can_replace_model_and_thinking_level() {
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls_for_hook = hook_calls.clone();
    let (registry, provider) = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Off),
        ..StreamOptions::default()
    };
    // The unchecked stream skips credential resolution (the fake provider
    // declares none) while still recording the model/options per request.
    let mut deps = LoopDeps {
        stream: pir::agent::agent_loop::registry_stream_unchecked(
            registry,
            fake_model("base-model"),
            options,
        ),
        model: Some(fake_model("base-model")),
        thinking_level: ThinkingLevel::Off,
        ..LoopDeps::default()
    };
    deps.prepare_request = Some(Arc::new(move |request: PrepareRequestContext| {
        let n = hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
        // The hook sees the current runtime values.
        assert_eq!(
            request.model.as_ref().map(|model| model.id.as_str()),
            Some("base-model")
        );
        assert_eq!(request.thinking_level, ThinkingLevel::Off);
        Box::pin(async move {
            if n == 0 {
                Some(PrepareRequestUpdate {
                    model: Some(fake_model("replaced-model")),
                    thinking_level: Some(ThinkingLevel::High),
                    ..PrepareRequestUpdate::default()
                })
            } else {
                None
            }
        })
    }));
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    assert_eq!(result.turns, 1);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    // The provider received the replaced model and thinking level.
    let observed = provider.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].0, "replaced-model");
    assert_eq!(observed[0].1, Some(ThinkingLevel::High));
    // The replaced thinking level is recorded on the assistant message.
    assert_eq!(result.terminal.thinking_level, Some(ThinkingLevel::High));
}

#[tokio::test]
async fn thinking_level_is_recorded_on_every_assistant_message() {
    let deps = LoopDeps {
        stream: Box::new(|_| Box::pin(async { assistant(vec![text("ok")], StopReason::Stop) })),
        thinking_level: ThinkingLevel::Medium,
        max_turns: 1,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.terminal.thinking_level, Some(ThinkingLevel::Medium));
    // Monotonic duration is recorded on the finalized assistant message.
    assert!(result.terminal.duration_ms.is_some());
}

// ---------------------------------------------------------------------------
// Nested tool calls
// ---------------------------------------------------------------------------

/// A tool that executes one nested `echo` call through its context.
struct SpawnEchoTool;
impl AgentTool for SpawnEchoTool {
    fn name(&self) -> &'static str {
        "spawn_echo"
    }
    fn description(&self) -> &'static str {
        "spawns a nested echo call"
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let nested = ctx.nested.clone();
        Box::pin(async move {
            let Some(nested) = nested else {
                return ToolResult::error("no nested capability in context");
            };
            let outcome = nested.execute("echo", json!({ "message": "hi" })).await;
            if outcome.is_error {
                return ToolResult::error(format!("nested failed: {}", outcome.result.content));
            }
            ToolResult::ok(format!("parent saw: {}", outcome.result.content))
        })
    }
}

struct EchoTool;
impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes the message argument"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
        }))
    }
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(json!({ "type": "object", "properties": { "length": { "type": "integer" } } }))
    }
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let message = args
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move {
            let mut result = ToolResult::ok(format!("echo:{message}"));
            result.structured_content = Some(json!({ "length": message.chars().count() }));
            result
        })
    }
}

/// A tool that nests itself until the depth bound refuses.
struct RecurseTool;
impl AgentTool for RecurseTool {
    fn name(&self) -> &'static str {
        "recurse"
    }
    fn description(&self) -> &'static str {
        "recurses via nested calls"
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let nested = ctx.nested.clone();
        Box::pin(async move {
            let Some(nested) = nested else {
                return ToolResult::error("no nested capability in context");
            };
            let outcome = nested.execute("recurse", json!({})).await;
            if outcome.is_error {
                ToolResult::error(outcome.result.content)
            } else {
                ToolResult::ok("unreachable: recursion should have been bounded")
            }
        })
    }
}

/// A nested tool that streams one live partial through its context before
/// returning, so the host `tool_execution_update` path is observable.
struct StreamingEchoTool;
impl AgentTool for StreamingEchoTool {
    fn name(&self) -> &'static str {
        "stream_echo"
    }
    fn description(&self) -> &'static str {
        "streams one partial update"
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        if let Some(on_update) = ctx.on_update.as_ref() {
            on_update(json!({
                "content": [{ "type": "text", "text": "half" }],
                "details": { "step": 1 },
                "isError": false,
            }));
        }
        Box::pin(async { ToolResult::ok("streamed") })
    }
}

/// A tool that runs [`StreamingEchoTool`] as a nested call.
struct SpawnStreamTool;
impl AgentTool for SpawnStreamTool {
    fn name(&self) -> &'static str {
        "spawn_stream"
    }
    fn description(&self) -> &'static str {
        "spawns a nested streaming call"
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let nested = ctx.nested.clone();
        Box::pin(async move {
            let Some(nested) = nested else {
                return ToolResult::error("no nested capability in context");
            };
            let outcome = nested.execute("stream_echo", json!({})).await;
            if outcome.is_error {
                ToolResult::error(format!("nested failed: {}", outcome.result.content))
            } else {
                ToolResult::ok(format!("parent saw: {}", outcome.result.content))
            }
        })
    }
}

fn nested_tool_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SpawnEchoTool));
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(RecurseTool));
    tools
}

/// Captured nested tool execution events: (kind, call id, parent call id).
type NestedEventLog = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

#[tokio::test]
async fn nested_tool_calls_get_generated_ids_parent_attribution_and_record() {
    let events: NestedEventLog = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "spawn_echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(nested_tool_registry()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event: AgentEvent| {
            let entry = match event {
                AgentEvent::ToolExecutionStart {
                    event:
                        ToolExecutionEvent::Start {
                            call_id,
                            parent_call_id,
                            ..
                        },
                    ..
                } => ("start".to_owned(), Some(call_id), parent_call_id),
                AgentEvent::ToolExecutionEnd {
                    event:
                        ToolExecutionEvent::End {
                            call_id,
                            parent_call_id,
                            ..
                        },
                    ..
                } => ("end".to_owned(), Some(call_id), parent_call_id),
                _ => return,
            };
            events_for_handler.lock().unwrap().push(entry);
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    // The parent tool saw the nested result.
    let parent_result = &result.tool_results[0];
    assert_eq!(
        text_of(&Message::ToolResult(parent_result.clone())),
        "parent saw: echo:hi"
    );

    // Nested events carry the generated id and `parentToolCallId`.
    let events = events.lock().unwrap();
    let nested_start = events
        .iter()
        .find(|(kind, _call_id, parent)| kind == "start" && parent.as_deref() == Some("call-1"))
        .expect("nested tool_execution_start event");
    assert_eq!(nested_start.1.as_deref(), Some("call-1/0"));
    let nested_end = events
        .iter()
        .find(|(kind, _call_id, parent)| kind == "end" && parent.as_deref() == Some("call-1"))
        .expect("nested tool_execution_end event");
    assert_eq!(nested_end.1.as_deref(), Some("call-1/0"));
    // Model-issued events carry no parent.
    assert!(
        events.iter().any(|(kind, call_id, parent)| kind == "start"
            && call_id.as_deref() == Some("call-1")
            && parent.is_none()),
        "model-issued call must have no parentToolCallId"
    );

    // The parent tool result records the bounded nestedCalls entry.
    let nested_calls = parent_result
        .nested_calls
        .as_ref()
        .expect("nestedCalls record");
    assert!(nested_calls.complete);
    assert_eq!(nested_calls.calls.len(), 1);
    let record = &nested_calls.calls[0];
    assert_eq!(record.id, "call-1/0");
    assert_eq!(record.name, "echo");
    assert_eq!(record.status, pir::ai::types::NestedToolCallStatus::Ok);
    assert!(record.duration_ms.is_some());
    assert_eq!(
        record
            .arguments
            .as_ref()
            .map(|arguments| arguments.get("message").cloned()),
        Some(Some(json!("hi")))
    );

    // The nested echo's structured content reaches the parent's record only
    // through the tool result message of the nested call itself (nested calls
    // are not persisted), so the parent message has no structured content.
    assert!(parent_result.structured_content.is_none());
}

#[tokio::test]
async fn nested_tool_updates_reach_the_host_event_stream_with_parent_attribution() {
    type UpdateLog = Arc<Mutex<Vec<(String, String, Option<String>, Value, Value)>>>;
    let updates: UpdateLog = Arc::new(Mutex::new(Vec::new()));
    let updates_for_handler = Arc::clone(&updates);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SpawnStreamTool));
    tools.register(Arc::new(StreamingEchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call("call-1", "spawn_stream")],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event: AgentEvent| {
            if let AgentEvent::ToolExecutionUpdate {
                call_id,
                tool_name,
                parent_call_id,
                args,
                partial,
                ..
            } = event
            {
                updates_for_handler.lock().unwrap().push((
                    call_id,
                    tool_name,
                    parent_call_id,
                    args,
                    partial,
                ));
            }
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;
    assert_eq!(
        text_of(&Message::ToolResult(result.tool_results[0].clone())),
        "parent saw: streamed"
    );

    let updates = updates.lock().unwrap();
    let nested = updates
        .iter()
        .find(|update| update.0 == "call-1/0")
        .expect("nested tool_execution_update event");
    assert_eq!(nested.1, "stream_echo");
    assert_eq!(
        nested.2.as_deref(),
        Some("call-1"),
        "the nested update must name the immediate caller: {nested:?}"
    );
    assert_eq!(nested.3, json!({}), "the nested call's args");
    assert_eq!(
        nested.4,
        json!({
            "content": [{ "type": "text", "text": "half" }],
            "details": { "step": 1 },
            "isError": false,
        }),
        "the partial result must not carry the scheduler's identity stamps"
    );
}

#[tokio::test]
async fn nested_tool_call_depth_is_bounded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "recurse")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(nested_tool_registry()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    let parent_result = &result.tool_results[0];
    let content = text_of(&Message::ToolResult(parent_result.clone()));
    assert!(
        content.contains("depth exceeded"),
        "recursion must stop at the depth bound: {content}"
    );
    // Every executed level is recorded; the record stays complete.
    let nested_calls = parent_result
        .nested_calls
        .as_ref()
        .expect("nestedCalls record");
    eprintln!("DEPTH DEBUG: {:?}", nested_calls);
    assert!(nested_calls.complete);
    assert_eq!(nested_calls.calls.len(), MAX_NESTED_DEPTH);
    assert_eq!(
        nested_calls.calls[0].status,
        pir::ai::types::NestedToolCallStatus::Error
    );
    assert_eq!(nested_calls.calls[0].id, "call-1/0");
}

#[tokio::test]
async fn nested_calls_flow_through_the_extension_gates_with_parent_attribution() {
    let temp = std::env::temp_dir().join(format!(
        "pir-loop-parity-ext-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let source_path = temp.join("ext.ts");
    std::fs::write(
        &source_path,
        r#"
        export default function (pi) {
            pi.on("tool_call", (event) => { globalThis.__calls = [...(globalThis.__calls ?? []), [event.toolCallId, event.parentToolCallId ?? null]] });
            pi.on("tool_result", (event) => { globalThis.__results = [...(globalThis.__results ?? []), [event.toolCallId, event.parentToolCallId ?? null]] });
        }
        "#,
    )
    .expect("write fixture");
    let config = ExtensionConfig {
        cwd: temp.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    };
    let runtime = ExtensionHandle::start(config).expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&source_path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "spawn_echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(nested_tool_registry()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        extensions: Some(runtime.clone()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(
        text_of(&Message::ToolResult(result.tool_results[0].clone())),
        "parent saw: echo:hi"
    );

    let probe_calls = runtime
        .eval_json("JSON.stringify(globalThis.__calls ?? [])")
        .await
        .expect("probe")
        .to_string();
    let probe_results = runtime
        .eval_json("JSON.stringify(globalThis.__results ?? [])")
        .await
        .expect("probe")
        .to_string();
    let _ = std::fs::remove_dir_all(&temp);

    // The nested call passed the tool_call gate with parentToolCallId set.
    assert!(
        probe_calls.contains(r#"["call-1/0","call-1"]"#),
        "nested tool_call gate payload missing parentToolCallId: {probe_calls}"
    );
    // The nested result flowed through the tool_result gate too.
    assert!(
        probe_results.contains(r#"["call-1/0","call-1"]"#),
        "nested tool_result payload missing parentToolCallId: {probe_results}"
    );
}

// ---------------------------------------------------------------------------
// durationMs + structuredContent threading
// ---------------------------------------------------------------------------

struct SlowTool;
impl AgentTool for SlowTool {
    fn name(&self) -> &'static str {
        "slow"
    }
    fn description(&self) -> &'static str {
        "sleeps briefly"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            ToolResult::ok("slept")
        })
    }
}

#[tokio::test]
async fn tool_results_and_execution_events_carry_monotonic_duration() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SlowTool));
    let durations: Arc<Mutex<Vec<Option<u64>>>> = Arc::new(Mutex::new(Vec::new()));
    let durations_for_handler = durations.clone();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "slow")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        events: Some(Arc::new(move |event: AgentEvent| {
            if let AgentEvent::ToolExecutionEnd {
                event: ToolExecutionEvent::End { duration_ms, .. },
                ..
            } = event
            {
                durations_for_handler.lock().unwrap().push(duration_ms);
            }
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    let tool_result = &result.tool_results[0];
    let duration = tool_result.duration_ms.expect("tool result durationMs");
    assert!(
        duration >= 40,
        "duration must cover the tool sleep: {duration}"
    );
    // `tool_execution_end` carries the same duration.
    let events = durations.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].is_some_and(|value| value >= 40));
}

#[tokio::test]
async fn structured_content_threads_to_tool_result_messages() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call_with_args(
                            "call-1",
                            "echo",
                            json!({ "message": "hi" }),
                        )],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(nested_tool_registry()),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    // The echo tool's machine-readable output reaches the tool result message.
    assert_eq!(
        text_of(&Message::ToolResult(result.tool_results[0].clone())),
        "echo:hi"
    );
    assert_eq!(
        result.tool_results[0].structured_content,
        Some(json!({ "length": 2 }))
    );
}

#[tokio::test]
async fn after_hook_structured_content_follows_upstream_merge_rules() {
    // Case 1: content replaced without structuredContent drops it.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let hooks = ToolHooks {
        before: None,
        after: Some(Box::new(|_prepared, _result, _ctx| {
            Box::pin(async {
                AfterToolCallResult {
                    content: Some("replaced".to_owned()),
                    ..AfterToolCallResult::default()
                }
            })
        })),
    };
    // Run a direct echo (no spawner) so the echo tool's own result message is
    // checked.
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call_with_args(
                            "call-1",
                            "echo",
                            json!({ "message": "hi" }),
                        )],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new({
            let mut tools = ToolRegistry::new();
            tools.register(Arc::new(EchoTool));
            tools
        }),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        hooks: Some(Arc::new(hooks)),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    let tool_result = &result.tool_results[0];
    assert_eq!(
        text_of(&Message::ToolResult(tool_result.clone())),
        "replaced"
    );
    assert!(
        tool_result.structured_content.is_none(),
        "content replaced without structuredContent must drop it"
    );

    // Case 2: structuredContent returned alongside content is kept.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let hooks = ToolHooks {
        before: None,
        after: Some(Box::new(|_prepared, _result, _ctx| {
            Box::pin(async {
                AfterToolCallResult {
                    content: Some("replaced".to_owned()),
                    structured_content: Some(json!({ "kept": true })),
                    ..AfterToolCallResult::default()
                }
            })
        })),
    };
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call_with_args(
                            "call-1",
                            "echo",
                            json!({ "message": "hi" }),
                        )],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new({
            let mut tools = ToolRegistry::new();
            tools.register(Arc::new(EchoTool));
            tools
        }),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        hooks: Some(Arc::new(hooks)),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    let tool_result = &result.tool_results[0];
    assert_eq!(
        tool_result.structured_content,
        Some(json!({ "kept": true }))
    );
}

#[tokio::test]
async fn after_hook_content_blocks_and_usage_reach_the_tool_result_message() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let usage = Usage {
        input: 11,
        output: 4,
        total_tokens: 15,
        ..Usage::default()
    };
    let hooks = ToolHooks {
        before: None,
        after: Some(Box::new(move |_prepared, _result, _ctx| {
            Box::pin(async move {
                AfterToolCallResult {
                    content_blocks: Some(vec![
                        ToolResultContentBlock::Text("first".to_owned()),
                        ToolResultContentBlock::Image(ImageContent {
                            data: "aW1n".to_owned(),
                            mime_type: "image/png".to_owned(),
                        }),
                        ToolResultContentBlock::Text("second".to_owned()),
                    ]),
                    usage: Some(usage),
                    ..AfterToolCallResult::default()
                }
            })
        })),
    };
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call_with_args(
                            "call-1",
                            "echo",
                            json!({ "message": "hi" }),
                        )],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new({
            let mut tools = ToolRegistry::new();
            tools.register(Arc::new(EchoTool));
            tools
        }),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        hooks: Some(Arc::new(hooks)),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    let tool_result = &result.tool_results[0];
    // The block array flattens to the model-facing text plus the image block,
    // exactly like a tool that produced text and images directly.
    assert_eq!(
        text_of(&Message::ToolResult(tool_result.clone())),
        "first\nsecond"
    );
    assert!(
        matches!(
            tool_result.content.get(1),
            Some(ContentBlock::Image(image))
                if image.data == "aW1n" && image.mime_type == "image/png"
        ),
        "the image block is carried on the tool-result message: {:?}",
        tool_result.content
    );
    // The after-hook usage reaches the tool-result message (upstream
    // `createToolResultMessage` copies `result.usage`).
    assert_eq!(tool_result.usage, Some(usage));
}

// ---------------------------------------------------------------------------
// AgentSession passthrough + agent_settled {aborted} + entry_appended
// ---------------------------------------------------------------------------

/// A tool whose run blocks until the gate opens or the context aborts.
#[derive(Clone)]
struct GateTool {
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl AgentTool for GateTool {
    fn name(&self) -> &'static str {
        "gate"
    }
    fn description(&self) -> &'static str {
        "blocks until released"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let aborted = ctx.signal.clone();
        Box::pin(async move {
            loop {
                if aborted.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return ToolResult::error("aborted");
                }
                if *gate.borrow() {
                    return ToolResult::ok("gate open");
                }
                if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        })
    }
}

fn gate_registry(
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GateTool { gate, entered }));
    tools
}

async fn load_extension(temp: &std::path::Path, source: &str) -> ExtensionHandle {
    let path = temp.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let config = ExtensionConfig {
        cwd: temp.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    };
    let runtime = ExtensionHandle::start(config).expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    runtime
}

#[tokio::test]
async fn agent_settled_payload_carries_aborted_flag() {
    let temp = std::env::temp_dir().join(format!(
        "pir-loop-parity-settled-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let runtime = load_extension(
        &temp,
        r#"
        export default function (pi) {
            pi.on("agent_settled", (event) => { globalThis.__settled = event.aborted; });
        }
        "#,
    )
    .await;

    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = calls.clone();
    let (registry, _provider) = fake_registry(Arc::new(move |index, _context| {
        let _ = calls_for_responder.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
        } else {
            assistant(vec![text("after abort")], StopReason::Stop)
        }
    }));
    let session = SessionManager::create(".", &temp).expect("create session");
    let agent = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model("fake-model"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: gate_registry(gate_rx, entered.clone()),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    }));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.prompt("start", Arc::new(|_event| {})).await }
    });
    // Wait for the gate tool to be executing, then abort the run.
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    agent.abort();
    let _ = gate_tx.send(true);
    let terminal = prompt_task
        .await
        .expect("prompt task")
        .expect("prompt result");
    let _ = std::fs::remove_dir_all(&temp);

    // The run settled as aborted and the extension observed `aborted: true`.
    // The terminal message is the tool-use turn that was interrupted.
    assert_eq!(terminal.stop_reason, StopReason::ToolUse);
    let settled = runtime
        .eval_json("JSON.stringify(globalThis.__settled)")
        .await
        .expect("probe")
        .to_string();
    assert_eq!(settled, "true", "agent_settled must carry aborted=true");
}

#[tokio::test]
async fn session_hooks_passthrough_finish_turn_and_prepare_request() {
    let temp = std::env::temp_dir().join(format!(
        "pir-loop-parity-session-hooks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let (registry, _provider) = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("hello")], StopReason::Stop)
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model("fake-model"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    let _ = std::fs::remove_dir_all(&temp);

    // `finish_turn` end: the run stops after the first turn.
    agent.set_finish_turn(Some(Arc::new(|_turn: FinishTurnContext| {
        Box::pin(async { Some(FinishTurnAction::End) })
    })));
    let terminal = agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt");
    assert_eq!(text_of(&Message::Assistant(terminal)), "hello");
    agent.set_finish_turn(None);

    // `prepare_request` replacement: the replaced thinking level is recorded
    // on the assistant message.
    agent.set_prepare_request(Some(Arc::new(|_request: PrepareRequestContext| {
        Box::pin(async move {
            Some(PrepareRequestUpdate {
                thinking_level: Some(ThinkingLevel::XHigh),
                ..PrepareRequestUpdate::default()
            })
        })
    })));
    let terminal = agent
        .prompt("again", Arc::new(|_event| {}))
        .await
        .expect("prompt");
    assert_eq!(terminal.thinking_level, Some(ThinkingLevel::XHigh));
    agent.set_prepare_request(None);
}

#[tokio::test]
async fn entry_appended_fires_for_messages_appended_to_storage() {
    let temp = std::env::temp_dir().join(format!(
        "pir-loop-parity-entry-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let runtime = load_extension(
        &temp,
        r#"
        export default function (pi) {
            pi.on("entry_appended", (event) => { globalThis.__entries = [...(globalThis.__entries ?? []), event.entry.id]; });
        }
        "#,
    )
    .await;

    let (registry, _provider) = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("stored")], StopReason::Stop)
    }));
    let session = SessionManager::create(".", &temp).expect("create session");
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model("fake-model"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    agent
        .prompt("hi", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    let entries = runtime
        .eval_json("JSON.stringify(globalThis.__entries ?? [])")
        .await
        .expect("probe");
    let entries: Vec<String> = serde_json::from_value(entries).unwrap();
    assert!(
        entries.len() >= 2,
        "user + assistant appends must emit entry_appended: {entries:?}"
    );
    assert!(!entries.iter().any(|id| id.is_empty()));
}

// ---------------------------------------------------------------------------
// W16-D11: the executable set is the loadout
// ---------------------------------------------------------------------------

/// A tool that records its own executions, so a test can prove which calls
/// actually ran rather than only what the transcript says.
struct RecordedTool {
    name: &'static str,
    exposure: ToolExposure,
    log: Arc<Mutex<Vec<String>>>,
}

impl AgentTool for RecordedTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "records its execution"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({ "type": "object" }))
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let name = self.name;
        let log = Arc::clone(&self.log);
        Box::pin(async move {
            log.lock().unwrap().push(name.to_owned());
            ToolResult::ok(format!("ran:{name}"))
        })
    }
}

/// W16-D11: a model-issued call resolves against the EXECUTABLE set, which
/// upstream defines as the declared loadout (`this.agent.state.tools =
/// declared`, agent-session.ts:1623; the loop knows only that array,
/// agent-loop.ts:713-723). A registered but UNDECLARED tool — `direct` or
/// `codemode`/`deferred` — is "not found".
///
/// The top-level gate is this request's declared definitions ∪ the
/// `direct` names of [`LoopDeps::nested_callable`] (active `direct` tools, so
/// hook-hidden declarations stay executable). The full `nested_callable` set —
/// every registered `codemode`/`deferred` tool — is the `ctx.executeTool` gate,
/// NOT a top-level widening. This pins the four required outcomes in one
/// batch:
/// - the undeclared `direct` tool fails exactly like an absent tool;
/// - the undeclared registered `deferred` tool fails the same way;
/// - a DECLARED deferred tool (an explicit activation puts it in
///   `context.tools`) still executes;
/// - the full callable set still reaches an undeclared deferred tool through
///   nested `ctx.executeTool`.
#[tokio::test]
async fn undeclared_registered_tool_is_unknown_but_declared_and_nested_paths_work() {
    let ran: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    for (name, exposure) in [
        ("active_direct", ToolExposure::Direct),
        ("latent_direct", ToolExposure::Direct),
        ("deferred_declared", ToolExposure::Deferred),
        ("deferred_latent", ToolExposure::Deferred),
    ] {
        tools.register(Arc::new(RecordedTool {
            name,
            exposure,
            log: Arc::clone(&ran),
        }));
    }
    tools.register(Arc::new(NestedCallerTool {
        log: Arc::clone(&ran),
    }));
    // What a host hands the loop as `LoopDeps::nested_callable`: the active
    // `direct` tools plus every registered `codemode`/`deferred` tool.
    // `latent_direct` is inactive, so it is in neither the active nor the
    // callable set.
    let callable: std::collections::BTreeSet<String> = [
        "active_direct",
        "nested_caller",
        "deferred_declared",
        "deferred_latent",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![
                            tool_call("call-active", "active_direct"),
                            tool_call("call-latent", "latent_direct"),
                            tool_call("call-deferred-declared", "deferred_declared"),
                            tool_call("call-deferred-latent", "deferred_latent"),
                            tool_call("call-nested", "nested_caller"),
                        ],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
        nested_callable: Some(Arc::new(callable)),
        max_turns: 4,
        ..LoopDeps::default()
    };
    // The declared loadout of this request: the two active direct tools and
    // the explicitly activated deferred tool. `deferred_latent` is registered
    // and callable from other tools but never declared.
    let context = Context {
        tools: Some(
            ["active_direct", "nested_caller", "deferred_declared"]
                .iter()
                .map(|name| Tool {
                    name: (*name).into(),
                    description: "declared".into(),
                    parameters: json!({ "type": "object" }),
                    constrained_sampling: None,
                })
                .collect(),
        ),
        ..Context::default()
    };
    let result = run_with_options(deps, context, None, LoopOptions::default()).await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.tool_results.len(), 5);
    let result_for = |id: &str| {
        result
            .tool_results
            .iter()
            .find(|message| message.tool_call_id == id)
            .unwrap_or_else(|| panic!("tool result for {id}"))
            .clone()
    };

    let active = result_for("call-active");
    assert!(!active.is_error, "the declared active direct tool must run");
    assert_eq!(text_of(&Message::ToolResult(active)), "ran:active_direct");

    let latent = result_for("call-latent");
    assert!(
        latent.is_error,
        "an undeclared registered direct tool must be rejected"
    );
    assert!(
        text_of(&Message::ToolResult(latent)) == "Tool latent_direct not found",
        "the rejection must be the scheduler's missing-tool result, naming the tool \
         (upstream agent-loop.ts:720)"
    );

    let declared_deferred = result_for("call-deferred-declared");
    assert!(
        !declared_deferred.is_error,
        "a declared deferred tool stays executable at top level"
    );
    assert_eq!(
        text_of(&Message::ToolResult(declared_deferred)),
        "ran:deferred_declared"
    );

    let latent_deferred = result_for("call-deferred-latent");
    assert!(
        latent_deferred.is_error,
        "an undeclared registered deferred tool must be rejected at top level"
    );
    assert!(
        text_of(&Message::ToolResult(latent_deferred)) == "Tool deferred_latent not found",
        "an undeclared deferred call must fail with the missing-tool result, not the nested gate"
    );

    let nested = result_for("call-nested");
    assert!(
        !nested.is_error,
        "the declared caller must run and reach the deferred tool through ctx.executeTool"
    );
    assert_eq!(
        text_of(&Message::ToolResult(nested)),
        "nested:ran:deferred_latent",
        "the full nested_callable set must remain reachable from ctx.executeTool"
    );

    // Execution proof, not just transcript shape: only the declared calls and
    // the NESTED deferred call ran; the two top-level rejections never did.
    let mut ran = ran.lock().unwrap().clone();
    ran.sort();
    assert_eq!(
        ran,
        vec![
            "active_direct".to_owned(),
            "deferred_declared".to_owned(),
            "deferred_latent".to_owned(),
            "nested_caller".to_owned(),
        ]
    );
}

/// A declared `direct` tool whose body calls another tool through
/// `ctx.executeTool` (the nested scheduler path the full `nested_callable`
/// set gates).
struct NestedCallerTool {
    log: Arc<Mutex<Vec<String>>>,
}

impl AgentTool for NestedCallerTool {
    fn name(&self) -> &'static str {
        "nested_caller"
    }
    fn description(&self) -> &'static str {
        "calls a deferred tool through ctx.executeTool"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let log = Arc::clone(&self.log);
        let nested = ctx.nested.as_ref().map(Arc::clone);
        Box::pin(async move {
            log.lock().unwrap().push("nested_caller".to_owned());
            let Some(nested) = nested else {
                return ToolResult::error("no nested executor in context");
            };
            let outcome = nested.execute("deferred_latent", json!({})).await;
            if outcome.is_error {
                ToolResult::error(format!("nested failed: {}", outcome.result.content))
            } else {
                ToolResult::ok(format!("nested:{}", outcome.result.content))
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Host event surface: `turn_end.toolResults` and the non-assistant
// `message_start`/`message_end` pair around each tool-result message
// (upstream agent-loop.ts:493 and 946-951).
// ---------------------------------------------------------------------------

/// A tool that answers with plain text so the tool-result message is easy to
/// identify in the event stream.
struct EventEchoTool;

impl AgentTool for EventEchoTool {
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

#[tokio::test]
async fn turn_end_carries_tool_results_and_announces_tool_result_messages() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EventEchoTool));
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let index = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if index == 0 {
                    assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        max_turns: 4,
        ..LoopDeps::default()
    };

    let user = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text("hello")],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    });
    let mut context = Context::default();
    context.messages.push(user.clone());

    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event);
        })),
        preamble_messages: vec![user],
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, context, None, options).await;
    assert_eq!(result.turns, 2);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.tool_results[0].tool_call_id, "call-1");

    let events = events.lock().unwrap();

    // 1. `turn_end` carries the turn's tool results (Rust shape
    // `TurnEnd { turn, message, tool_results }`; wire `turn_end.toolResults`).
    let turn_tool_results = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnEnd { tool_results, .. } if !tool_results.is_empty() => {
                Some(tool_results)
            }
            _ => None,
        })
        .expect("a turn_end carrying this turn's tool results");
    assert_eq!(turn_tool_results.len(), 1);
    assert_eq!(turn_tool_results[0].tool_call_id, "call-1");
    assert_eq!(
        text_of(&Message::ToolResult(turn_tool_results[0].clone())),
        "tool-result"
    );

    // 2. The run's starting user message is announced as a non-assistant
    // message pair at turn 0 (`preamble_messages`).
    let user_start = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageStart {
                    turn: 0,
                    message: Message::User(message),
                } if message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text(text) if text.text == "hello")
                })
            )
        })
        .expect("turn-0 message_start for the starting user message");
    let user_end = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageEnd {
                    turn: 0,
                    message: Message::User(message),
                } if message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text(text) if text.text == "hello")
                })
            )
        })
        .expect("turn-0 message_end for the starting user message");
    assert!(
        user_start < user_end,
        "message_start must precede its message_end"
    );

    // 3. The tool-result message has its own pair, after the tool finished
    // and before the next turn's assistant stream (upstream order:
    // tool_execution_end → message_start → message_end → next turn).
    let tool_end = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ToolExecutionEnd {
                    event: ToolExecutionEvent::End { call_id, .. },
                    ..
                } if call_id.as_str() == "call-1"
            )
        })
        .expect("tool_execution_end for call-1");
    let result_start = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageStart {
                    message: Message::ToolResult(message),
                    ..
                } if message.tool_call_id == "call-1"
            )
        })
        .expect("message_start for the tool-result message");
    let result_end = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageEnd {
                    message: Message::ToolResult(message),
                    ..
                } if message.tool_call_id == "call-1"
            )
        })
        .expect("message_end for the tool-result message");
    let next_turn = events
        .iter()
        .position(|event| matches!(event, AgentEvent::StreamStart { turn: 2, .. }))
        .expect("the second turn's assistant stream start");
    let tool_turn_end = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::TurnEnd { tool_results, .. } if !tool_results.is_empty()
            )
        })
        .expect("the tool turn's turn_end");

    assert!(
        tool_end < result_start,
        "tool_execution_end must precede the tool-result message_start \
         (indices tool_end={tool_end}, message_start={result_start})"
    );
    assert!(
        result_start < result_end,
        "message_start must precede its message_end \
         (indices start={result_start}, end={result_end})"
    );
    assert!(
        result_end < tool_turn_end,
        "the tool-result message pair is announced before turn_end \
         (indices end={result_end}, turn_end={tool_turn_end})"
    );
    assert!(
        result_end < next_turn,
        "the next turn starts after the tool-result message_end \
         (indices end={result_end}, next_turn={next_turn})"
    );
}

// ---------------------------------------------------------------------------
// transform_context / prepare_next_turn
// (upstream AgentLoopConfig hooks, agent-loop.ts:194-217,399-401)
// ---------------------------------------------------------------------------

/// A user message used by the hook fixtures below.
fn hook_user_message(message_text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text(message_text)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

/// Tool used by the hook fixtures: returns `tool-result`.
struct HookEchoTool;

impl AgentTool for HookEchoTool {
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

/// `transform_context` runs immediately before EVERY provider request
/// (upstream `streamAssistantResponse`), including the first, and its output is
/// request-local: the run transcript the session persists/replays is never
/// replaced by the transformed value.
#[tokio::test]
async fn transform_context_applies_to_every_request_without_mutating_the_transcript() {
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let transform_calls = Arc::new(AtomicUsize::new(0));
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let seen_marker = Arc::new(Mutex::new(Vec::<bool>::new()));

    let stream_calls_for_stream = Arc::clone(&stream_calls);
    let marker_for_stream = Arc::clone(&seen_marker);
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            stream_calls_for_stream.fetch_add(1, Ordering::SeqCst);
            marker_for_stream.lock().unwrap().push(
                request
                    .context
                    .messages
                    .iter()
                    .any(|message| text_of(message) == "request-only marker"),
            );
            Box::pin(async { assistant(vec![text("reply")], StopReason::Stop) })
        }),
        finish_turn: Some(Arc::new({
            let finish_calls = Arc::clone(&finish_calls);
            move |_turn: FinishTurnContext| {
                let n = finish_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        Some(FinishTurnAction::Continue)
                    } else {
                        None
                    }
                })
            }
        })),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let transform_calls_for_hook = Arc::clone(&transform_calls);
    let options = LoopOptions {
        transform_context: Some(Arc::new(
            move |context: Arc<Context>, _signal: Option<AbortSignal>| {
                transform_calls_for_hook.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    let mut transformed = (*context).clone();
                    transformed
                        .messages
                        .push(hook_user_message("request-only marker"));
                    Arc::new(transformed)
                })
            },
        )),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(
        stream_calls.load(Ordering::SeqCst),
        2,
        "the continuation forced a second request"
    );
    assert_eq!(
        transform_calls.load(Ordering::SeqCst),
        2,
        "transform_context runs before every request, including the first"
    );
    assert!(
        seen_marker.lock().unwrap().iter().all(|seen| *seen),
        "every request saw the transformed context"
    );
    assert!(
        !result
            .context
            .messages
            .iter()
            .any(|message| text_of(message) == "request-only marker"),
        "the transformed context must not leak into the run transcript"
    );
    assert!(
        !result
            .new_messages
            .iter()
            .any(|message| text_of(message) == "request-only marker"),
        "the transformed context must not enter the produced-message list"
    );
}

/// `prepare_next_turn` runs after `turn_end` and before the next turn's
/// request; its context/messages/model/thinking replacements take effect, and
/// its messages get the normal lifecycle plus persistence. It composes with
/// `transform_context` (which runs later, at the request boundary).
#[tokio::test]
async fn prepare_next_turn_runs_after_turn_end_and_shapes_the_next_request() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HookEchoTool));

    let stream_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    // (model id, thinking level, saw prepared note, saw transform marker)
    type RequestObservation = (Option<String>, ThinkingLevel, bool, bool);
    let requests = Arc::new(Mutex::new(Vec::<RequestObservation>::new()));

    let stream_calls_for_stream = Arc::clone(&stream_calls);
    let requests_for_stream = Arc::clone(&requests);
    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let n = stream_calls_for_stream.fetch_add(1, Ordering::SeqCst);
            requests_for_stream.lock().unwrap().push((
                request.model.as_ref().map(|model| model.id.clone()),
                request.thinking_level,
                request
                    .context
                    .messages
                    .iter()
                    .any(|message| text_of(message) == "prepared note"),
                request
                    .context
                    .messages
                    .iter()
                    .any(|message| text_of(message) == "request-only marker"),
            ));
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        model: Some(fake_model("original-model")),
        thinking_level: ThinkingLevel::Off,
        max_turns: 4,
        ..LoopDeps::default()
    };

    let order_for_events = Arc::clone(&order);
    let order_for_hook = Arc::clone(&order);
    let hook_calls_for_hook = Arc::clone(&hook_calls);
    let persisted = Arc::new(Mutex::new(Vec::<String>::new()));
    let persisted_for_sink = Arc::clone(&persisted);
    let options = LoopOptions {
        events: Some(Arc::new(move |event: AgentEvent| {
            if matches!(event, AgentEvent::TurnEnd { .. }) {
                order_for_events.lock().unwrap().push("turn_end".into());
            }
        })),
        // Compaction runs BEFORE prepare_next_turn (upstream session wrapper);
        // the hook must observe the compacted context.
        compaction: Some(Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let mut context = (*context).clone();
                context.system_prompt = Some("compacted".into());
                Arc::new(context)
            })
        })),
        transform_context: Some(Arc::new(
            |context: Arc<Context>, _signal: Option<AbortSignal>| {
                Box::pin(async move {
                    let mut transformed = (*context).clone();
                    transformed
                        .messages
                        .push(hook_user_message("request-only marker"));
                    Arc::new(transformed)
                })
            },
        )),
        prepare_next_turn: Some(Arc::new(move |turn: Arc<FinishTurnContext>| {
            hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            order_for_hook
                .lock()
                .unwrap()
                .push("prepare_next_turn".into());
            assert!(
                turn.message.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::ToolCall(call) if call.id == "call-1"
                )),
                "the snapshot carries the completed assistant message"
            );
            assert_eq!(turn.tool_results.len(), 1);
            assert_eq!(
                text_of(&Message::ToolResult(turn.tool_results[0].clone())),
                "tool-result",
                "the snapshot carries this turn's tool results"
            );
            assert_eq!(
                turn.context.system_prompt.as_deref(),
                Some("compacted"),
                "prepare_next_turn observes the compacted context"
            );
            Box::pin(async move {
                Some(PrepareNextTurnUpdate {
                    messages: vec![hook_user_message("prepared note")],
                    model: Some(fake_model("replacement-model")),
                    thinking_level: Some(ThinkingLevel::High),
                    ..PrepareNextTurnUpdate::default()
                })
            })
        })),
        persist_message: Some(Arc::new(move |message: Message| {
            let persisted = Arc::clone(&persisted_for_sink);
            Box::pin(async move {
                persisted.lock().unwrap().push(text_of(&message));
            })
        })),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(stream_calls.load(Ordering::SeqCst), 2);
    let order = order.lock().unwrap().clone();
    let turn_end_at = order
        .iter()
        .position(|label| label == "turn_end")
        .expect("turn_end emitted");
    let prepare_at = order
        .iter()
        .position(|label| label == "prepare_next_turn")
        .expect("prepare_next_turn ran");
    assert!(
        turn_end_at < prepare_at,
        "prepare_next_turn runs after turn_end for the next turn: {order:?}"
    );

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0.as_deref(), Some("original-model"));
    assert_eq!(requests[0].1, ThinkingLevel::Off);
    assert!(!requests[0].2, "no prepared message on the first request");
    assert!(requests[0].3, "transform_context ran on the first request");
    assert_eq!(
        requests[1].0.as_deref(),
        Some("replacement-model"),
        "the model replacement applies to the next request"
    );
    assert_eq!(
        requests[1].1,
        ThinkingLevel::High,
        "the thinking-level replacement applies to the next request"
    );
    assert!(requests[1].2, "the prepared message rides the next request");
    assert!(
        requests[1].3,
        "transform_context still runs after prepare_next_turn"
    );

    // Prepared messages enter the transcript (unlike transform output) and are
    // persisted exactly once.
    assert_eq!(
        result
            .context
            .messages
            .iter()
            .filter(|message| text_of(message) == "prepared note")
            .count(),
        1
    );
    assert_eq!(
        persisted
            .lock()
            .unwrap()
            .iter()
            .filter(|text| text.as_str() == "prepared note")
            .count(),
        1
    );
    assert!(
        !result
            .context
            .messages
            .iter()
            .any(|message| text_of(message) == "request-only marker"),
        "transform output stays request-local"
    );
}

/// With no hooks — or hooks that change nothing — the run is byte-for-byte the
/// same shape: same request count, same turns, same produced messages.
#[tokio::test]
async fn noop_loop_hooks_preserve_baseline_behavior() {
    async fn run_scripted(with_noop_hooks: bool) -> (usize, usize, Vec<String>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_stream = Arc::clone(&calls);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HookEchoTool));
        let deps = LoopDeps {
            stream: Box::new(move |_request| {
                let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse)
                    } else {
                        assistant(vec![text("done")], StopReason::Stop)
                    }
                })
            }),
            tools: Arc::new(tools),
            max_turns: 4,
            ..LoopDeps::default()
        };
        let options = if with_noop_hooks {
            LoopOptions {
                transform_context: Some(Arc::new(
                    |context: Arc<Context>, _signal: Option<AbortSignal>| {
                        Box::pin(async move { context })
                    },
                )),
                prepare_next_turn: Some(Arc::new(|_turn: Arc<FinishTurnContext>| {
                    Box::pin(async { None })
                })),
                ..LoopOptions::default()
            }
        } else {
            LoopOptions::default()
        };
        let result = run_with_options(deps, Context::default(), None, options).await;
        (
            calls.load(Ordering::SeqCst),
            result.turns,
            // Compare the produced message roles/content, not wall-clock
            // timestamps (the tool-result and declaration messages carry
            // `now_ms()` and would differ between two runs by design).
            result
                .new_messages
                .iter()
                .map(|message| {
                    let role = match message {
                        Message::System(_) => "system",
                        Message::User(_) => "user",
                        Message::Developer(_) => "developer",
                        Message::Assistant(_) => "assistant",
                        Message::ToolResult(_) => "toolResult",
                    };
                    format!("{role}:{}", text_of(message))
                })
                .collect(),
        )
    }

    let baseline = run_scripted(false).await;
    let with_noop_hooks = run_scripted(true).await;
    assert_eq!(baseline, with_noop_hooks);
}

// ---------------------------------------------------------------------------
// Transcript copy-on-write regression guards (deep-audit 2026-10-26 #6)
// ---------------------------------------------------------------------------

/// Regression (deep-audit #6): a completed turn must not leave a second
/// reference into the run's transcript or produced-message list alive between
/// turns. The observable is allocation IDENTITY: cloning an `Arc` refcount-bumps
/// and keeps the pointer, while a copy-on-write clone — the whole-transcript
/// deep copy (every tool output included) the old `last_completed_turn`
/// snapshot forced on EVERY turn, O(turns²) bytes over a run — allocates a new
/// `Context` / `Vec<Message>`.
///
/// `prepare_request` and `finish_turn` hand out live handles and drop them
/// before returning, so with no `prepare_next_turn` hook every turn must keep
/// growing the SAME two allocations in place. An empty steering poll must not
/// change that (it must not force `Arc::make_mut`).
#[tokio::test]
async fn no_prepare_next_turn_keeps_one_transcript_allocation_across_turns() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HookEchoTool));

    let stream_calls = Arc::new(AtomicUsize::new(0));
    let context_ptrs = Arc::new(Mutex::new(Vec::<usize>::new()));
    let produced_ptrs = Arc::new(Mutex::new(Vec::<usize>::new()));
    let message_counts = Arc::new(Mutex::new(Vec::<usize>::new()));

    let stream_calls_for_stream = Arc::clone(&stream_calls);
    let context_ptrs_for_request = Arc::clone(&context_ptrs);
    let message_counts_for_request = Arc::clone(&message_counts);
    let produced_ptrs_for_finish = Arc::clone(&produced_ptrs);

    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let n = stream_calls_for_stream.fetch_add(1, Ordering::SeqCst);
            // Read-only, and the request (plus its context handle) is dropped
            // before this future completes, so the run's own handle is unique
            // at every append.
            let _ = request.context.messages.len();
            Box::pin(async move {
                if n < 2 {
                    assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        prepare_request: Some(Arc::new(move |context: PrepareRequestContext| {
            context_ptrs_for_request
                .lock()
                .unwrap()
                .push(Arc::as_ptr(&context.context) as usize);
            message_counts_for_request
                .lock()
                .unwrap()
                .push(context.context.messages.len());
            Box::pin(async { None })
        })),
        finish_turn: Some(Arc::new(move |turn: FinishTurnContext| {
            produced_ptrs_for_finish
                .lock()
                .unwrap()
                .push(Arc::as_ptr(&turn.new_messages) as usize);
            Box::pin(async { None })
        })),
        max_turns: 8,
        ..LoopDeps::default()
    };
    // Empty every poll: the steering check at the top of each turn must not
    // force copy-on-write when it injects nothing.
    let options = LoopOptions {
        steering: Some(Box::new(|| Box::pin(async { Vec::new() }))),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(stream_calls.load(Ordering::SeqCst), 3, "three requests ran");
    let context_ptrs = context_ptrs.lock().unwrap().clone();
    assert_eq!(context_ptrs.len(), 3);
    assert!(
        context_ptrs.windows(2).all(|pair| pair[0] == pair[1]),
        "every request must observe the SAME transcript allocation; a \
         difference means a turn deep-copied the whole context: {context_ptrs:?}"
    );
    assert_eq!(
        Arc::as_ptr(&result.context) as usize,
        context_ptrs[0],
        "the run result must own the allocation the requests saw"
    );
    assert_eq!(
        *message_counts.lock().unwrap(),
        vec![1, 3, 5],
        "the declaration plus each turn's assistant/tool-result messages grow \
         the one transcript in place"
    );
    let produced_ptrs = produced_ptrs.lock().unwrap().clone();
    assert_eq!(produced_ptrs.len(), 3, "finish_turn ran for every turn");
    assert!(
        produced_ptrs.windows(2).all(|pair| pair[0] == pair[1]),
        "new_messages must keep one allocation too; a second cross-turn \
         reference made every turn clone the whole produced list: \
         {produced_ptrs:?}"
    );
}

/// Regression (deep-audit #6): the `prepare_next_turn` snapshot is a PER-CALL
/// view of the live transcript and produced-message list, not a cross-turn
/// owner. With a no-op hook installed (and an empty steering poll) every turn
/// must still grow the same allocations in place; otherwise merely having the
/// hook deep-copies the whole transcript once per turn.
#[tokio::test]
async fn noop_prepare_next_turn_does_not_pin_the_transcript_allocation() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HookEchoTool));

    let stream_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let context_ptrs = Arc::new(Mutex::new(Vec::<usize>::new()));
    let produced_ptrs = Arc::new(Mutex::new(Vec::<usize>::new()));

    let stream_calls_for_stream = Arc::clone(&stream_calls);
    let hook_calls_for_hook = Arc::clone(&hook_calls);
    let context_ptrs_for_request = Arc::clone(&context_ptrs);
    let produced_ptrs_for_finish = Arc::clone(&produced_ptrs);

    let deps = LoopDeps {
        stream: Box::new(move |request| {
            let n = stream_calls_for_stream.fetch_add(1, Ordering::SeqCst);
            let _ = request.context.messages.len();
            Box::pin(async move {
                if n < 2 {
                    assistant(vec![tool_call("call-1", "echo")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        prepare_request: Some(Arc::new(move |context: PrepareRequestContext| {
            context_ptrs_for_request
                .lock()
                .unwrap()
                .push(Arc::as_ptr(&context.context) as usize);
            Box::pin(async { None })
        })),
        finish_turn: Some(Arc::new(move |turn: FinishTurnContext| {
            produced_ptrs_for_finish
                .lock()
                .unwrap()
                .push(Arc::as_ptr(&turn.new_messages) as usize);
            Box::pin(async { None })
        })),
        max_turns: 8,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        prepare_next_turn: Some(Arc::new(move |turn: Arc<FinishTurnContext>| {
            hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            // Consume the snapshot and drop it before returning: a hook that
            // wants to keep it can clone it itself (and then pays the COW
            // copy, which is the documented contract).
            let _ = turn.tool_results.len();
            Box::pin(async { None })
        })),
        steering: Some(Box::new(|| Box::pin(async { Vec::new() }))),
        ..LoopOptions::default()
    };

    let result = run_with_options(deps, Context::default(), None, options).await;

    assert_eq!(stream_calls.load(Ordering::SeqCst), 3, "three requests ran");
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        2,
        "prepare_next_turn ran before the two following turns"
    );
    let context_ptrs = context_ptrs.lock().unwrap().clone();
    assert_eq!(context_ptrs.len(), 3);
    assert!(
        context_ptrs.windows(2).all(|pair| pair[0] == pair[1]),
        "a no-op prepare_next_turn hook must not force a transcript copy: \
         {context_ptrs:?}"
    );
    assert_eq!(
        Arc::as_ptr(&result.context) as usize,
        context_ptrs[0],
        "the run result must own the allocation the requests saw"
    );
    let produced_ptrs = produced_ptrs.lock().unwrap().clone();
    assert_eq!(produced_ptrs.len(), 3, "finish_turn ran for every turn");
    assert!(
        produced_ptrs.windows(2).all(|pair| pair[0] == pair[1]),
        "the prepared snapshot must not pin new_messages across turns: \
         {produced_ptrs:?}"
    );
}

// ---------------------------------------------------------------------------
// H2: tool event timing — `tool_result` merges in per-tool finalize and the
// `tool_execution_end` frame/message carry the handler's content/isError/usage;
// `tool_call` fires after validation and rewritten input is not re-validated.
// ---------------------------------------------------------------------------

fn h2_temp_dir(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pir-loop-parity-h2-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// `tool_execution_end` observes the handler-merged result (content/isError/
/// usage), not the pre-merge tool output, and the following tool-result
/// message is the same value.
#[tokio::test]
async fn tool_result_merge_precedes_tool_execution_end() {
    struct OriginalTool;
    impl AgentTool for OriginalTool {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn description(&self) -> &'static str {
            "returns the original result"
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            Box::pin(async { ToolResult::ok("original") })
        }
    }

    let temp = h2_temp_dir("end-merge");
    let runtime = load_extension(
        &temp,
        r#"
        export default function (pi) {
          globalThis.__ends = [];
          pi.on("tool_result", (event) => ({
            content: [
              { type: "text", text: "merged:" + event.toolCallId },
              { type: "image", data: "aW1n", mimeType: "image/png" },
            ],
            isError: true,
            usage: { input: 7, output: 3, totalTokens: 10 },
          }));
          pi.on("tool_execution_end", (event) => {
            globalThis.__ends.push({
              id: event.toolCallId,
              isError: event.isError === true,
              text: (event.result.content ?? [])
                .filter((block) => block.type === "text")
                .map((block) => block.text ?? "")
                .join(""),
              images: (event.result.content ?? []).filter((block) => block.type === "image").length,
              usageInput: event.result.usage?.input ?? null,
            });
          });
        }
        "#,
    )
    .await;

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(OriginalTool));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(vec![tool_call("call-1", "probe")], StopReason::ToolUse)
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        extensions: Some(runtime.clone()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event)
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    let (message_is_error, message) = {
        let captured = events.lock().unwrap();
        let (host_is_error, host_result) = captured
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolExecutionEnd {
                    event:
                        ToolExecutionEvent::End {
                            call_id, is_error, ..
                        },
                    result,
                    ..
                } if call_id == "call-1" => Some((*is_error, result.clone())),
                _ => None,
            })
            .expect("tool_execution_end for call-1");
        // The handler turned the result into an error and replaced the content:
        // the end frame must already carry both.
        assert!(host_is_error, "the merged isError must reach the end frame");
        assert_eq!(
            host_result["content"][0]["text"],
            json!("merged:call-1"),
            "the handler's text replacement must reach the end frame: {host_result}"
        );
        assert_eq!(
            host_result["content"][1]["type"],
            json!("image"),
            "the handler's image must reach the end frame: {host_result}"
        );
        assert_eq!(
            host_result["usage"]["input"],
            json!(7),
            "the handler's usage must reach the end frame: {host_result}"
        );

        let (message_is_error, message) = captured
            .iter()
            .find_map(|event| match event {
                AgentEvent::MessageEnd {
                    message: Message::ToolResult(message),
                    ..
                } if message.tool_call_id == "call-1" => Some((message.is_error, message.clone())),
                _ => None,
            })
            .expect("tool-result message_end for call-1");
        (message_is_error, message)
    };

    assert!(message_is_error);
    assert_eq!(
        text_of(&Message::ToolResult(message.clone())),
        "merged:call-1"
    );
    assert!(
        matches!(message.content.get(1), Some(ContentBlock::Image(image)) if image.data == "aW1n"),
        "the image block must reach the tool-result message: {:?}",
        message.content
    );
    let expected_usage = Usage {
        input: 7,
        output: 3,
        total_tokens: 10,
        ..Usage::default()
    };
    assert_eq!(
        message.usage,
        Some(expected_usage),
        "the handler-reported usage must reach the tool-result message"
    );
    assert_eq!(
        result.tool_results[0].usage,
        Some(expected_usage),
        "the run's tool-result list carries the same message"
    );

    let seen = runtime
        .eval_json("JSON.stringify(globalThis.__ends ?? [])")
        .await
        .expect("probe tool_execution_end observations");
    assert_eq!(
        seen,
        json!([{ "id": "call-1", "isError": true, "text": "merged:call-1", "images": 1, "usageInput": 7 }]),
        "the JS tool_execution_end handler saw the merged value: {seen}"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

/// Three parallel calls each finalize independently: every
/// `tool_execution_end` and every tool-result message carries its OWN call id's
/// handler override.
#[tokio::test]
async fn parallel_tool_results_merge_per_call_id() {
    struct WorkTool;
    impl AgentTool for WorkTool {
        fn name(&self) -> &'static str {
            "work"
        }
        fn description(&self) -> &'static str {
            "sleeps for its n argument, then reports it"
        }
        fn run(
            &self,
            args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            let n = args
                .get("n")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(n)).await;
                ToolResult::ok(format!("raw:{n}"))
            })
        }
    }

    let temp = h2_temp_dir("parallel-ids");
    let runtime = load_extension(
        &temp,
        r#"
        export default function (pi) {
          globalThis.__ends = [];
          pi.on("tool_result", (event) => ({
            content: [{ type: "text", text: "merged:" + event.toolCallId }],
          }));
          pi.on("tool_execution_end", (event) => {
            globalThis.__ends.push({
              id: event.toolCallId,
              text: (event.result.content ?? [])
                .filter((block) => block.type === "text")
                .map((block) => block.text ?? "")
                .join(""),
            });
          });
        }
        "#,
    )
    .await;

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WorkTool));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![
                            tool_call_with_args("p1", "work", json!({ "n": 60 })),
                            tool_call_with_args("p2", "work", json!({ "n": 30 })),
                            tool_call_with_args("p3", "work", json!({ "n": 1 })),
                        ],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        extensions: Some(runtime.clone()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = events.clone();
    let options = LoopOptions {
        events: Some(Arc::new(move |event| {
            events_for_handler.lock().unwrap().push(event)
        })),
        ..LoopOptions::default()
    };
    let result = run_with_options(deps, Context::default(), None, options).await;

    {
        let captured = events.lock().unwrap();
        for id in ["p1", "p2", "p3"] {
            let end = captured
                .iter()
                .find_map(|event| match event {
                    AgentEvent::ToolExecutionEnd {
                        event: ToolExecutionEvent::End { call_id, .. },
                        result,
                        ..
                    } if call_id == id => Some(result.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("tool_execution_end for {id}"));
            assert_eq!(
                end["content"][0]["text"],
                json!(format!("merged:{id}")),
                "the end frame for {id} must carry its own merge: {end}"
            );
        }
    }

    assert_eq!(result.tool_results.len(), 3);
    for message in &result.tool_results {
        assert_eq!(
            text_of(&Message::ToolResult(message.clone())),
            format!("merged:{}", message.tool_call_id),
            "the tool-result message must carry its own id's merge"
        );
        assert!(!message.is_error);
    }
    assert_eq!(
        result
            .tool_results
            .iter()
            .map(|message| message.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["p1", "p2", "p3"],
        "results stay in assistant source order"
    );

    let seen = runtime
        .eval_json("JSON.stringify(globalThis.__ends ?? [])")
        .await
        .expect("probe tool_execution_end observations");
    let mut observed = seen.as_array().cloned().unwrap_or_default();
    observed.sort_by_key(|entry| entry["id"].as_str().unwrap_or_default().to_owned());
    assert_eq!(
        observed,
        vec![
            json!({ "id": "p1", "text": "merged:p1" }),
            json!({ "id": "p2", "text": "merged:p2" }),
            json!({ "id": "p3", "text": "merged:p3" }),
        ],
        "every parallel call's end handler observed its own merge: {seen}"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

/// An invalid-argument call fails schema validation before the `tool_call`
/// gate, so no handler observes it and the tool never runs.
#[tokio::test]
async fn invalid_arguments_do_not_trigger_tool_call_handlers() {
    struct StrictTool {
        runs: Arc<AtomicUsize>,
    }
    impl AgentTool for StrictTool {
        fn name(&self) -> &'static str {
            "strict"
        }
        fn description(&self) -> &'static str {
            "requires a text argument"
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false,
            }))
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { ToolResult::ok("ran") })
        }
    }

    let temp = h2_temp_dir("invalid-args");
    let runtime = load_extension(
        &temp,
        r#"
        export default function (pi) {
          globalThis.__calls = [];
          pi.on("tool_call", (event) => {
            globalThis.__calls.push(event.toolCallId);
          });
        }
        "#,
    )
    .await;

    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StrictTool {
        runs: Arc::clone(&runs),
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call_with_args("call-bad", "strict", json!({}))],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        extensions: Some(runtime.clone()),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "an invalid-argument call must not execute"
    );
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].is_error);
    let text = text_of(&Message::ToolResult(result.tool_results[0].clone()));
    assert!(
        text.contains("Validation failed for tool \"strict\"")
            && text.contains("missing required property 'text'"),
        "the model sees the validation failure: {text}"
    );
    let seen = runtime
        .eval_json("JSON.stringify(globalThis.__calls ?? [])")
        .await
        .expect("probe tool_call observations");
    assert_eq!(
        seen,
        json!([]),
        "no tool_call handler may observe an invalid-argument call: {seen}"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

/// A `tool_call` handler rewrites the validated input into a payload the
/// schema would reject; the tool executes with the rewritten value (no second
/// validation) and the call succeeds.
#[tokio::test]
async fn tool_call_rewrite_is_not_revalidated() {
    struct GreetTool {
        seen: Arc<Mutex<Option<serde_json::Value>>>,
    }
    impl AgentTool for GreetTool {
        fn name(&self) -> &'static str {
            "greet"
        }
        fn description(&self) -> &'static str {
            "records its arguments"
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"],
            }))
        }
        fn run(
            &self,
            args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            *self.seen.lock().unwrap() = Some(args.clone());
            let message = args
                .get("message")
                .map(ToString::to_string)
                .unwrap_or_default();
            Box::pin(async move { ToolResult::ok(format!("greet:{message}")) })
        }
    }

    let temp = h2_temp_dir("rewrite");
    let runtime = load_extension(
        &temp,
        r#"
        export default function (pi) {
          pi.on("tool_call", (event) => {
            if (event.toolName === "greet") {
              // Deliberately schema-invalid (the declared type is string):
              // upstream performs no re-validation after mutation.
              event.input.message = 42;
            }
          });
        }
        "#,
    )
    .await;

    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GreetTool {
        seen: Arc::clone(&seen),
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = calls.clone();
    let deps = LoopDeps {
        stream: Box::new(move |_| {
            let n = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    assistant(
                        vec![tool_call_with_args(
                            "call-1",
                            "greet",
                            json!({ "message": "original" }),
                        )],
                        StopReason::ToolUse,
                    )
                } else {
                    assistant(vec![text("done")], StopReason::Stop)
                }
            })
        }),
        tools: Arc::new(tools),
        extensions: Some(runtime),
        max_turns: 4,
        ..LoopDeps::default()
    };
    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    let args = seen.lock().unwrap().clone().expect("the tool ran");
    assert_eq!(
        args["message"],
        json!(42),
        "the tool must execute the handler-rewritten value: {args:?}"
    );
    assert_eq!(result.tool_results.len(), 1);
    assert!(
        !result.tool_results[0].is_error,
        "a rewritten payload must not fail a second validation: {result:?}"
    );
    assert_eq!(
        text_of(&Message::ToolResult(result.tool_results[0].clone())),
        "greet:42"
    );
    let _ = std::fs::remove_dir_all(&temp);
}
