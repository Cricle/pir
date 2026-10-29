//! Tests for the SDK facade (`pir::sdk`): builder resolution, typed events,
//! tools, plugins, sessions, and error mapping. The model is faked with a
//! scripted provider (same pattern as `tests/agent_session.rs`); no network
//! and no `~/.pi` configuration is consulted on the fake-provider paths.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::sdk::{Agent, AgentBuilder, Endpoint, SdkError, SdkEvent, SessionMode};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fakes (same shape as tests/agent_session.rs)
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    assistant_with_usage(content, stop_reason, 0)
}

fn assistant_with_usage(
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
    total_tokens: u64,
) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        usage: Usage {
            total_tokens,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str, arguments: BTreeMap<String, serde_json::Value>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that answers every stream with a scripted message.
struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
    /// API keys seen on the stream options (P2-11: key precedence probe).
    seen_keys: SeenKeys,
}

impl Provider for FakeProvider {
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
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen_keys.lock().unwrap().push(options.api_key.clone());
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        // Surface the text content as a streaming delta so the typed event
        // stream sees the full lifecycle (start/delta/end).
        if let Some(ContentBlock::Text(text)) = message
            .content
            .iter()
            .find(|block| matches!(block, ContentBlock::Text(_)))
        {
            stream.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.text.clone(),
                partial: Arc::new(message.clone()),
            });
        }
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    fake_registry_with_keys(respond).0
}

/// Shared record of the API keys observed on stream options.
type SeenKeys = Arc<Mutex<Vec<Option<String>>>>;

/// Registry whose provider records the API key of every stream request into
/// the returned shared recorder.
fn fake_registry_with_keys(respond: Responder) -> (Arc<ProviderRegistry>, SeenKeys) {
    let mut registry = ProviderRegistry::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
        seen_keys: Arc::clone(&seen),
    }));
    (Arc::new(registry), seen)
}

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
        Some(serde_json::json!({"type": "object"}))
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

/// A tool that records its invocations and answers with its argument.
struct EchoTool {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "word_count"
    }
    fn description(&self) -> &'static str {
        "Count the words in a text"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }))
    }
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let count = args["text"]
            .as_str()
            .map(|value| value.split_whitespace().count())
            .unwrap_or(0);
        Box::pin(async move { ToolResult::ok(format!("{count} words")) })
    }
}

async fn wait_for_tool_entry(entered: &Arc<AtomicUsize>) {
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("tool never started");
}

/// Per-test temp session dir.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn user_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

/// Per-test temp working directory.
fn temp_cwd() -> std::path::PathBuf {
    TempDir::new("cwd").0.clone()
}

fn agent_with(
    registry: Arc<ProviderRegistry>,
    tools: Vec<Arc<dyn AgentTool>>,
) -> impl std::future::Future<Output = Agent> {
    let mut builder = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .no_session()
        .no_compaction()
        .cwd(temp_cwd())
        .thinking_level(ThinkingLevel::Off);
    for tool in tools {
        builder = builder.with_tool(tool);
    }
    async move { builder.build().await.expect("build agent") }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_endpoint_builds_and_reports_model() {
    let agent = AgentBuilder::new()
        .endpoint(Endpoint::new(
            "http://localhost:9/v1",
            pir::sdk::ApiKind::OpenAiCompletions,
            "endpoint-model",
        ))
        .api_key("sk-test")
        .no_session()
        .build()
        .await
        .expect("endpoint build");
    let model = agent.model();
    assert_eq!(model.id, "endpoint-model");
    assert_eq!(model.provider, "sdk-endpoint");
    assert_eq!(model.to_string(), "sdk-endpoint/endpoint-model");
    assert!(
        agent.session_path().is_none(),
        "no_session must not persist"
    );
    assert_eq!(agent.usage_totals(), pir::sdk::UsageSnapshot::default());
}

#[tokio::test]
async fn unknown_model_pattern_is_no_model_error() {
    let error = AgentBuilder::new()
        .model("zz-definitely-not-a-model-xyzzy")
        .no_session()
        .build()
        .await
        .expect_err("unknown model must fail resolution");
    assert!(
        matches!(error, SdkError::NoModel { .. }),
        "expected NoModel, got: {error}"
    );
}

#[tokio::test]
async fn custom_tool_runs_end_to_end() {
    let runs = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn AgentTool> = Arc::new(EchoTool {
        runs: Arc::clone(&runs),
    });
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "text".to_owned(),
        serde_json::json!("the quick brown fox jumps"),
    );
    let registry = fake_registry(Arc::new(move |index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "word_count", arguments.clone())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("counted for you")], StopReason::Stop)
        }
    }));
    let agent = agent_with(registry, vec![tool]).await;

    let outcome = agent.prompt("count the words").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("counted for you"));
    assert_eq!(outcome.turns, 2);
    assert!(!outcome.aborted);
    let tool_texts = outcome.tool_result_texts();
    assert!(
        tool_texts.iter().any(|value| value == "5 words"),
        "custom tool result missing: {tool_texts:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "custom tool must have run");
}

#[tokio::test]
async fn typed_event_stream_order() {
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("hello world")], StopReason::Stop)
    }));
    let agent = agent_with(registry, Vec::new()).await;

    let events: Vec<SdkEvent> = agent.prompt_stream("hi").collect::<Vec<_>>().await;
    let labels: Vec<&'static str> = events
        .iter()
        .map(|event| match event {
            SdkEvent::TurnStart { .. } => "turn_start",
            SdkEvent::TurnEnd { .. } => "turn_end",
            SdkEvent::MessageStart => "message_start",
            SdkEvent::MessageUpdate { .. } => "message_update",
            SdkEvent::MessageDone { .. } => "message_done",
            SdkEvent::MessageEnd { .. } => "message_end",
            SdkEvent::TranscriptMessageStart { .. } => "transcript_message_start",
            SdkEvent::TranscriptMessageEnd { .. } => "transcript_message_end",
            SdkEvent::ToolExecutionStart { .. } => "tool_start",
            SdkEvent::ToolExecutionUpdate { .. } => "tool_update",
            SdkEvent::ToolExecutionEnd { .. } => "tool_end",
            SdkEvent::Compaction => "compaction",
            SdkEvent::AgentEnd { .. } => "agent_end",
            SdkEvent::AgentSettled { .. } => "settled",
            _ => "other",
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            // Upstream run order: `agent_start` (not projected), then the
            // first `turn_start`, then the starting user message's transcript
            // pair (initial `message_*` events), then the assistant stream
            // lifecycle, its terminal `message_done`, the low-level
            // `agent_end`, and the session's terminal `agent_settled` (H6 SDK
            // projection).
            "turn_start",
            "transcript_message_start",
            "transcript_message_end",
            "message_start",
            "message_update",
            "message_done",
            "message_end",
            "turn_end",
            "agent_end",
            "settled"
        ]
    );
    // Typed payloads: the delta carries the full text; the settled event
    // reports a non-aborted run.
    let mut deltas = events.iter().filter_map(|event| match event {
        SdkEvent::MessageUpdate { delta, thinking } => Some((delta.clone(), *thinking)),
        _ => None,
    });
    assert_eq!(deltas.next(), Some(("hello world".to_owned(), false)));
    let settled = events.last().expect("settled");
    assert!(matches!(settled, SdkEvent::AgentSettled { aborted: false }));
}

/// A tool whose result terminates the current loop run (`terminate: true`).
/// The context then ends on a tool result, which is what an
/// `agent_before_settle` continuation requires (`context_can_continue`).
struct StopTool;

impl AgentTool for StopTool {
    fn name(&self) -> &'static str {
        "stop"
    }
    fn description(&self) -> &'static str {
        "stop the loop run"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async {
            let mut result = ToolResult::ok("stop now");
            result.terminate = true;
            result
        })
    }
}

/// `AgentSettled` is the SESSION settle, not the end of one low-level loop
/// run (upstream: use `agent_settled`, not `agent_end`). An
/// `agent_before_settle` continuation makes `AgentSession::prompt` re-enter
/// the loop; the stream must expose both runs and exactly ONE terminal
/// `AgentSettled`, after the continuation.
#[tokio::test]
async fn settle_continuation_keeps_one_terminal_agent_settled() {
    let settle_calls = Arc::new(AtomicUsize::new(0));
    let settle_calls_handler = Arc::clone(&settle_calls);
    let registry = fake_registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![tool_call("call-1", "stop", BTreeMap::new())],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text("after settle")], StopReason::Stop),
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .with_tool(Arc::new(StopTool))
        .on_event("agent_before_settle", move |_payload| {
            // Continue once; the second boundary lets the run settle.
            if settle_calls_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                pir::plugins::PluginEventOutcome::Generic(serde_json::json!({"continue": true}))
            } else {
                pir::plugins::PluginEventOutcome::None
            }
        })
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .cwd(temp_cwd())
        .build()
        .await
        .expect("build agent");

    let events: Vec<SdkEvent> = agent.prompt_stream("hi").collect::<Vec<_>>().await;
    assert_eq!(
        settle_calls.load(Ordering::SeqCst),
        2,
        "the continuation plus the final boundary must both dispatch: {events:?}"
    );
    // Both loop runs are on the stream: the tool-calling run and the
    // continuation's text run.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SdkEvent::TurnStart { .. }))
            .count(),
        2,
        "both loop runs must stream: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(event, SdkEvent::MessageEnd { message }
            if message.content.iter().any(|block| matches!(block, ContentBlock::Text(item) if item.text == "after settle")))),
        "the continuation run's answer must arrive before the terminal event: {events:?}"
    );
    let settled_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event, SdkEvent::AgentSettled { .. }).then_some(index)
        })
        .collect();
    assert_eq!(
        settled_positions,
        vec![events.len() - 1],
        "exactly one AgentSettled, as the last event: {events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(SdkEvent::AgentSettled { aborted: false })
    ));
}

#[tokio::test]
async fn steer_delivers_during_run() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_for_responder = Arc::clone(&observed);
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "gate", BTreeMap::new())],
                StopReason::ToolUse,
            )
        } else {
            observed_for_responder
                .lock()
                .unwrap()
                .extend(user_texts(context));
            assistant(vec![text("after steer")], StopReason::Stop)
        }
    }));
    let gate_tool: Arc<dyn AgentTool> = Arc::new(GateTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    });
    let agent = agent_with(registry, vec![gate_tool]).await;

    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("start").await }
    });
    wait_for_tool_entry(&entered).await;
    agent.steer("redirect now");
    let _ = gate_tx.send(true);
    let outcome = prompt_task.await.expect("join").expect("prompt");

    assert_eq!(outcome.message_text().as_deref(), Some("after steer"));
    let seen = observed.lock().unwrap();
    assert!(
        seen.iter().any(|value| value == "redirect now"),
        "steering message never reached the model request: {seen:?}"
    );
}

#[tokio::test]
async fn abort_settles_stream_with_aborted_flag() {
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(
            vec![tool_call("call-1", "gate", BTreeMap::new())],
            StopReason::ToolUse,
        )
    }));
    let gate_tool: Arc<dyn AgentTool> = Arc::new(GateTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    });
    let agent = agent_with(registry, vec![gate_tool]).await;

    // Drain the stream on a task; the loop only emits tool events after the
    // batch completes, so tool entry is awaited via the shared counter.
    let drain_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut events = Box::pin(agent.prompt_stream("start"));
            let mut settled = None;
            while let Some(event) = events.as_mut().next().await {
                if matches!(event, SdkEvent::AgentSettled { .. }) {
                    settled = Some(event);
                    break;
                }
            }
            settled
        }
    });
    wait_for_tool_entry(&entered).await;
    agent.abort();
    let settled = drain_task.await.expect("drain join");
    assert!(
        matches!(settled, Some(SdkEvent::AgentSettled { aborted: true })),
        "abort must settle the stream with aborted=true, got {settled:?}"
    );
    assert!(!agent.is_streaming(), "the next run gets a fresh signal");
}

#[tokio::test]
async fn session_persists_upstream_format_and_no_session_skips() {
    // Session enabled: the transcript lands in an upstream-format JSONL file.
    let temp = TempDir::new("session");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("persisted!")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .session(SessionMode::Dir(temp.0.clone()))
        .no_compaction()
        .cwd(temp.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");
    agent.prompt("hello session").await.expect("prompt");
    let path = agent.session_path().expect("session path");
    let reopened = SessionManager::open(&path).expect("reopen session file");
    let rendered = reopened
        .build_context_entries()
        .expect("context entries")
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            Message::Assistant(assistant) => Some(
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<String>>();
    assert!(
        rendered.iter().any(|value| value == "hello session"),
        "user message missing from session storage: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|value| value == "persisted!"),
        "assistant message missing from session storage: {rendered:?}"
    );
    agent.shutdown().await.expect("shutdown");

    // no_session: nothing is persisted.
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ephemeral")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .no_session()
        .no_compaction()
        .cwd(TempDir::new("no-session").0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");
    agent.prompt("ephemeral prompt").await.expect("prompt");
    assert!(agent.session_path().is_none());
    assert_eq!(agent.usage_totals().turns, 1);
}

/// A plugin that registers one tool through the unified plugin system.
struct PluginEchoTool {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for PluginEchoTool {
    fn name(&self) -> &'static str {
        "plugin_echo"
    }
    fn description(&self) -> &'static str {
        "Echoes via the plugin registry"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }))
    }
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let value = args["value"].as_str().unwrap_or("").to_owned();
        Box::pin(async move { ToolResult::ok(format!("plugin:{value}")) })
    }
}

struct PluginWithTool {
    info: pir::plugins::PluginInfo,
    runs: Arc<AtomicUsize>,
}

impl PluginWithTool {
    fn new(runs: Arc<AtomicUsize>) -> Self {
        Self {
            info: pir::plugins::PluginInfo::inline("plugin-with-tool"),
            runs,
        }
    }
}

impl pir::plugins::AgentPlugin for PluginWithTool {
    fn info(&self) -> &pir::plugins::PluginInfo {
        &self.info
    }
    fn register(&self, ctx: &mut pir::plugins::RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(PluginEchoTool {
            runs: Arc::clone(&self.runs),
        }));
        Ok(())
    }
}

#[tokio::test]
async fn plugin_tool_is_callable_by_the_agent() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut arguments = BTreeMap::new();
    arguments.insert("value".to_owned(), serde_json::json!("through-plugin"));
    // The plugin-registered tool must be visible to the model (declared on
    // the request), not merely executable.
    let seen_tools: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_tools_for_responder = Arc::clone(&seen_tools);
    let registry = fake_registry(Arc::new(move |index, context| {
        if index == 0 {
            if let Some(tools) = &context.tools {
                *seen_tools_for_responder.lock().unwrap() =
                    tools.iter().map(|tool| tool.name.clone()).collect();
            }
            assistant(
                vec![tool_call("call-1", "plugin_echo", arguments.clone())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("plugin done")], StopReason::Stop)
        }
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .no_session()
        .no_compaction()
        .cwd(TempDir::new("plugin").0.clone())
        .thinking_level(ThinkingLevel::Off)
        .with_plugin(Arc::new(PluginWithTool::new(Arc::clone(&runs))))
        .build()
        .await
        .expect("build");

    let outcome = agent.prompt("call the plugin tool").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("plugin done"));
    let declared = seen_tools.lock().unwrap().clone();
    assert!(
        declared.iter().any(|name| name == "plugin_echo"),
        "the plugin-registered tool must be declared to the model: {declared:?}"
    );
    let tool_texts = outcome.tool_result_texts();
    assert!(
        tool_texts
            .iter()
            .any(|value| value == "plugin:through-plugin"),
        "plugin tool result missing: {tool_texts:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// SDK plugin wiring: builtins by name (`.builtin(...)` / `.builtins([...])`).
// The resolution/diagnostic helper behavior a TRIMMED build exercises is
// pinned in `builtin_feature_diagnostics_are_pinned` (tests run under the
// default `full` feature set, where no family is compiled out).
// ---------------------------------------------------------------------------

/// `.builtin("subagent")` attaches the real builtin, whose tool the model
/// sees declared (the same seam the other SDK tests observe the request
/// through; no child session is spawned).
#[cfg(feature = "subagent")]
#[tokio::test]
async fn builtin_by_name_registers_its_tool_for_the_model() {
    let seen_tools: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_tools_for_responder = Arc::clone(&seen_tools);
    let registry = fake_registry(Arc::new(move |_index, context| {
        if let Some(tools) = &context.tools {
            *seen_tools_for_responder.lock().unwrap() =
                tools.iter().map(|tool| tool.name.clone()).collect();
        }
        assistant(vec![text("no subagent needed")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .no_session()
        .no_compaction()
        .cwd(temp_cwd())
        .thinking_level(ThinkingLevel::Off)
        .builtin("subagent")
        .build()
        .await
        .expect("a full build attaches the subagent builtin");
    agent.prompt("hi").await.expect("prompt");
    let declared = seen_tools.lock().unwrap().clone();
    assert!(
        declared.iter().any(|name| name == "subagent"),
        "the subagent builtin's tool must be declared to the model: {declared:?}"
    );
    agent.shutdown().await.expect("shutdown");
}

/// `.agent_dir(..)` must re-root the `subagent` builtin: both the tool
/// description and the per-run discovery fallback use `<agent-dir>/agents`,
/// never the process `~/.pi/agent/agents` (D2 isolation for two SDK agents
/// in one process). The tool call in this test passes no mode, so the tool
/// returns its "Available agents" text straight from discovery — no child
/// session is spawned.
#[cfg(feature = "subagent")]
#[tokio::test]
async fn explicit_agent_dir_reroots_the_subagent_builtin() {
    let temp = TempDir::new("subagent-agent-dir");
    let agents_dir = temp.0.join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create agents dir");
    std::fs::write(
        agents_dir.join("isolated.md"),
        "---\nname: isolated-agent\ndescription: Lives in the temp agent dir\n---\nBe isolated.\n",
    )
    .expect("write agent fixture");

    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "subagent", BTreeMap::new())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("subagent reported")], StopReason::Stop)
        }
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .agent_dir(temp.0.clone())
        .no_session()
        .no_compaction()
        .cwd(temp_cwd())
        .thinking_level(ThinkingLevel::Off)
        .builtin("subagent")
        .build()
        .await
        .expect("build with the subagent builtin");

    let subagent = agent
        .tools()
        .into_iter()
        .find(|tool| tool.name == "subagent")
        .expect("subagent tool declared");
    let expected_dir = agents_dir.display().to_string();
    assert!(
        subagent.description.contains(&expected_dir),
        "the subagent description must name the agent's OWN agents dir \
         ({expected_dir}), got: {}",
        subagent.description
    );

    let outcome = agent.prompt("dispatch a subagent").await.expect("prompt");
    let texts = outcome.tool_result_texts();
    assert!(
        texts.iter().any(|text| text.contains("isolated-agent")),
        "subagent discovery must read the agent's OWN agents dir: {texts:?}"
    );
    assert_eq!(outcome.message_text().as_deref(), Some("subagent reported"));
    agent.shutdown().await.expect("shutdown");
}
/// the model sees the `todo` tool declared, two `todo` calls in one batch run
/// sequentially (call order = id order), the results land in the session file
/// with their persistence snapshot, and a FRESH plugin instance replays that
/// transcript back to the same state — the session save → load contract.
#[cfg(feature = "todo")]
#[tokio::test]
async fn todo_builtin_declares_its_tool_and_replays_the_session_transcript() {
    let temp = TempDir::new("todo-roundtrip");
    let mut create_one = BTreeMap::new();
    create_one.insert("action".to_owned(), serde_json::json!("create"));
    create_one.insert("subject".to_owned(), serde_json::json!("first task"));
    let mut create_two = BTreeMap::new();
    create_two.insert("action".to_owned(), serde_json::json!("create"));
    create_two.insert("subject".to_owned(), serde_json::json!("second task"));
    create_two.insert("blockedBy".to_owned(), serde_json::json!([1]));

    let seen_tools: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_responder = Arc::clone(&seen_tools);
    let registry = fake_registry(Arc::new(move |index, context| {
        if let Some(tools) = &context.tools {
            *seen_for_responder.lock().unwrap() =
                tools.iter().map(|tool| tool.name.clone()).collect();
        }
        if index == 0 {
            assistant(
                vec![
                    tool_call("call-1", "todo", create_one.clone()),
                    tool_call("call-2", "todo", create_two.clone()),
                ],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("tracked")], StopReason::Stop)
        }
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .session(SessionMode::Dir(temp.0.clone()))
        .no_compaction()
        .cwd(temp.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .builtin("todo")
        .build()
        .await
        .expect("a full build attaches the todo builtin");
    agent.prompt("track this work").await.expect("prompt");

    let declared = seen_tools.lock().unwrap().clone();
    assert!(
        declared.iter().any(|name| name == "todo"),
        "the todo builtin's tool must be declared to the model: {declared:?}"
    );

    // Session save: the persisted transcript carries the tool results.
    let path = agent.session_path().expect("session path");
    let reopened = SessionManager::open(&path).expect("reopen session file");
    let entries = reopened.build_context_entries().expect("context entries");
    let entries_json = serde_json::to_value(&entries).expect("entries serialize");
    agent.shutdown().await.expect("shutdown");

    // Session load: a fresh instance (the `/reload`/`/resume` shape) replays
    // the transcript alone.
    use pir::plugins::AgentPlugin as _;
    let plugin = Arc::new(pir::plugins::builtin::todo::TodoPlugin::new());
    plugin.on_session_context(&pir::extensions::SessionContextSnapshot {
        entries: entries_json,
        session_id: Some("round-trip".to_owned()),
        ..Default::default()
    });
    let state = plugin.state();
    assert_eq!(
        state.tasks.len(),
        2,
        "both creates must replay from the transcript: {state:?}"
    );
    assert_eq!(state.tasks[0].id, 1);
    assert_eq!(state.tasks[0].subject, "first task");
    assert_eq!(state.tasks[1].blocked_by, Some(vec![1]));
    assert_eq!(state.next_id, 3, "the id counter is part of the snapshot");
}

/// Unknown names (bare and `builtin:`-prefixed) fail `build()` with the
/// upstream wording, and every missing name is reported — never a silent
/// no-op.
#[tokio::test]
async fn unknown_builtin_name_fails_build_with_upstream_error() {
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));
    let error = AgentBuilder::new()
        .with_registry(Arc::clone(&registry), fake_model())
        .no_session()
        .builtin("zz-not-a-builtin")
        .build()
        .await
        .expect_err("unknown builtin names must fail the build");
    assert_eq!(
        error.to_string(),
        "Unknown built-in extension: builtin:zz-not-a-builtin"
    );

    let error = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .no_session()
        .builtin("builtin:zz-one")
        .builtins(["zz-two"])
        .build()
        .await
        .expect_err("unknown builtin names must fail the build");
    let message = error.to_string();
    assert!(
        message.contains("Unknown built-in extension: builtin:zz-one"),
        "{message}"
    );
    assert!(
        message.contains("Unknown built-in extension: builtin:zz-two"),
        "{message}"
    );
}

/// P2-K: `.builtin(...)` must obey the agent directory's settings gates
/// (`pythonTool: false` / `disabledBuiltinExtensions`), exactly like the
/// extension plan does. A requested builtin that settings disable fails
/// `build()` naming the setting — never a silently absent plugin.
#[cfg(any(feature = "subagent", feature = "python"))]
#[tokio::test]
async fn builtin_by_name_obeys_settings_gates() {
    let temp = TempDir::new("builtin-settings-gate");
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("unused")], StopReason::Stop)
    }));

    #[cfg(feature = "subagent")]
    {
        std::fs::write(
            temp.0.join("settings.json"),
            r#"{ "disabledBuiltinExtensions": ["subagent"] }"#,
        )
        .expect("write settings.json");
        let error = AgentBuilder::new()
            .with_registry(Arc::clone(&registry), fake_model())
            .agent_dir(temp.0.clone())
            .no_session()
            .builtin("subagent")
            .build()
            .await
            .expect_err("a settings-disabled builtin must fail the build");
        let message = error.to_string();
        assert!(
            message.contains("builtin:subagent"),
            "the diagnostic must name the builtin: {message}"
        );
        assert!(
            message.contains("disabled by settings"),
            "the diagnostic must name the settings gate: {message}"
        );
        assert!(
            message.contains(&temp.0.display().to_string()),
            "the diagnostic must name the agent directory: {message}"
        );
    }

    #[cfg(feature = "python")]
    {
        std::fs::write(temp.0.join("settings.json"), r#"{ "pythonTool": false }"#)
            .expect("write settings.json");
        let error = AgentBuilder::new()
            .with_registry(Arc::clone(&registry), fake_model())
            .agent_dir(temp.0.clone())
            .no_session()
            .builtin("python")
            .build()
            .await
            .expect_err("pythonTool: false must fail an explicit .builtin(\"python\")");
        let message = error.to_string();
        assert!(
            message.contains("builtin:python") && message.contains("disabled by settings"),
            "the diagnostic must name the builtin and the setting: {message}"
        );
    }
}

/// The diagnostic the SDK records for a builtin whose cargo feature is off
/// (`builtin::missing_builtin_message`) and the set of names treated as real
/// builtins. A compiled-out family cannot be attached under the default
/// `full` test build, so the helper's name → feature mapping is pinned
/// directly: every entry must name its feature, and unknown names must keep
/// the upstream wording (`--no-default-features` / single-family builds use
/// exactly this fork of the message).
#[test]
fn builtin_feature_diagnostics_are_pinned() {
    use pir::plugins::builtin::{builtin_names, missing_builtin_message};

    // Every name the factory list registers in THIS build resolves: the
    // SDK's `.builtin(name)` gate is `builtin_extension_def(name).is_some()`.
    for name in builtin_names() {
        assert!(
            pir::plugins::instantiate_builtin(name).is_some(),
            "registered builtin `{name}` must instantiate"
        );
    }

    // The full name → feature matrix (`builtin::BUILTIN_FEATURES`): a
    // trimmed build's message names the feature that is off.
    for (name, feature) in [
        ("subagent", "subagent"),
        ("tool-repair", "tool-repair"),
        ("tool-search", "tool-search"),
        ("mcp", "mcp"),
        ("python", "python"),
        ("codemode", "extensions"),
        ("todo", "todo"),
    ] {
        let message = missing_builtin_message(name);
        assert!(
            message.contains(&format!("builtin:{name}")),
            "`{name}` diagnostic must name the builtin: {message}"
        );
        assert!(
            message.contains(&format!("the `{feature}` feature is off")),
            "`{name}` diagnostic must name the `{feature}` feature: {message}"
        );
    }

    assert_eq!(
        missing_builtin_message("nope"),
        "Unknown built-in extension: builtin:nope"
    );
}

#[tokio::test]
async fn concurrent_prompt_is_an_error_and_aborted_runs_yield_error_variant() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "gate", BTreeMap::new())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let gate_tool: Arc<dyn AgentTool> = Arc::new(GateTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    });
    let agent = agent_with(registry, vec![gate_tool]).await;

    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("first").await }
    });
    wait_for_tool_entry(&entered).await;

    // A second prompt while one is in flight fails instead of interleaving.
    let error = agent.prompt("second").await.expect_err("second prompt");
    assert!(
        matches!(error, SdkError::Other(ref message) if message.contains("already streaming")),
        "expected the already-streaming error, got: {error}"
    );

    // Abort before any assistant content: SdkError::Aborted.
    agent.abort();
    let result = prompt_task.await.expect("join");
    assert!(
        matches!(result, Err(SdkError::Aborted)),
        "expected SdkError::Aborted, got: {result:?}"
    );
    let _ = gate_tx.send(true);
}

// ---------------------------------------------------------------------------
// P1-1: the bridge plugin's sink must be the agent's own sink slot, so
// plugin-dispatched tool_execution_update payloads reach the stream.
// ---------------------------------------------------------------------------

const UPDATE_TOOL_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "progress",
    description: "emits a tool execution update",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, onUpdate) {
      onUpdate({ content: [{ type: "text", text: "halfway" }], isError: false });
      return { content: [{ type: "text", text: "done" }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn tool_execution_update_flows_through_the_stream() {
    let dir = TempDir::new("update-ext");
    let extension = dir.0.join("update_tool.ts");
    std::fs::write(&extension, UPDATE_TOOL_TS).expect("write fixture");
    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "progress", BTreeMap::new())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("finished")], StopReason::Stop)
        }
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .extension_paths(vec![extension])
        .no_session()
        .no_compaction()
        .cwd(dir.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");

    let events: Vec<SdkEvent> = agent
        .prompt_stream("use the progress tool")
        .collect::<Vec<_>>()
        .await;
    let labels: Vec<&'static str> = events
        .iter()
        .map(|event| match event {
            SdkEvent::TurnStart { .. } => "turn_start",
            SdkEvent::TurnEnd { .. } => "turn_end",
            SdkEvent::MessageStart => "message_start",
            SdkEvent::MessageUpdate { .. } => "message_update",
            SdkEvent::MessageEnd { .. } => "message_end",
            SdkEvent::ToolExecutionStart { .. } => "tool_start",
            SdkEvent::ToolExecutionUpdate { .. } => "tool_update",
            SdkEvent::ToolExecutionEnd { .. } => "tool_end",
            SdkEvent::Compaction => "compaction",
            SdkEvent::PromptRejected { .. } => "rejected",
            SdkEvent::AgentSettled { .. } => "settled",
            _ => "other",
        })
        .collect();
    eprintln!("EVENT ORDER: {labels:?}");
    let updates: Vec<&SdkEvent> = events
        .iter()
        .filter(|event| matches!(event, SdkEvent::ToolExecutionUpdate { .. }))
        .collect();
    assert!(
        !updates.is_empty(),
        "no ToolExecutionUpdate reached the stream (regression: the bridge plugin's sink was disconnected); order: {labels:?}"
    );
    if let Some(SdkEvent::ToolExecutionUpdate {
        tool_call_id,
        partial,
        ..
    }) = updates.first()
    {
        assert_eq!(tool_call_id, "call-1");
        assert_eq!(partial["content"][0]["text"], "halfway");
    } else {
        unreachable!("filtered above");
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::AgentSettled { .. })),
        "the stream must settle, order: {labels:?}",
    );
}

// ---------------------------------------------------------------------------
// P2-3: a second concurrent prompt_stream must not steal the in-flight run's
// sink and must terminate with PromptRejected + AgentSettled.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_prompt_stream_is_rejected_with_terminal_events() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![tool_call("call-1", "gate", BTreeMap::new())],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let gate_tool: Arc<dyn AgentTool> = Arc::new(GateTool {
        gate: gate_rx,
        entered: Arc::clone(&entered),
    });
    let agent = agent_with(registry, vec![gate_tool]).await;

    // First stream owns the sink while its run is parked in the gate tool.
    let first = tokio::spawn({
        let agent = agent.clone();
        async move {
            agent
                .prompt_stream("start")
                .collect::<Vec<SdkEvent>>()
                .await
        }
    });
    wait_for_tool_entry(&entered).await;

    // The second stream is rejected before starting: it yields the typed
    // rejection and a terminal settled event instead of ending silently.
    let second: Vec<SdkEvent> = agent.prompt_stream("second").collect::<Vec<_>>().await;
    assert!(
        second
            .iter()
            .any(|event| matches!(event, SdkEvent::PromptRejected { message }
                if message.contains("already streaming"))),
        "expected a PromptRejected event, got: {second:?}"
    );
    assert!(
        matches!(
            second.last(),
            Some(SdkEvent::AgentSettled { aborted: false })
        ),
        "a rejected stream must still settle: {second:?}"
    );

    // The in-flight run is untouched: it settles with aborted=true.
    agent.abort();
    let first_events = first.await.expect("first stream join");
    assert!(
        matches!(
            first_events.last(),
            Some(SdkEvent::AgentSettled { aborted: true })
        ),
        "first stream must settle aborted: {first_events:?}"
    );
    let _ = gate_tx.send(true);
}

// ---------------------------------------------------------------------------
// P2-13: shutdown is bounded and post-shutdown prompts are rejected.
// ---------------------------------------------------------------------------

/// A tool that never returns and ignores the abort signal (simulating a
/// provider/tool wedge).
struct WedgedTool {
    entered: Arc<AtomicUsize>,
}

impl AgentTool for WedgedTool {
    fn name(&self) -> &'static str {
        "wedge"
    }
    fn description(&self) -> &'static str {
        "never returns, ignores aborts"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    }
}

#[tokio::test]
async fn shutdown_bounds_a_wedged_run_and_rejects_later_prompts() {
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(
            vec![tool_call("call-1", "wedge", BTreeMap::new())],
            StopReason::ToolUse,
        )
    }));
    let wedge: Arc<dyn AgentTool> = Arc::new(WedgedTool {
        entered: Arc::clone(&entered),
    });
    let agent = agent_with(registry, vec![wedge]).await;

    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("start").await }
    });
    wait_for_tool_entry(&entered).await;

    // The wedged run ignores the abort: shutdown must give up with an error
    // after the bounded wait instead of busy-polling forever.
    let shutdown = tokio::spawn({
        let agent = agent.clone();
        async move { agent.shutdown().await }
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), shutdown)
        .await
        .expect("shutdown must return (bounded)")
        .expect("join");
    assert!(
        matches!(&result, Err(SdkError::Other(message)) if message.contains("timed out")),
        "shutdown must time out on a wedged run, got: {result:?}"
    );

    // Post-shutdown prompts are rejected.
    let later = agent.prompt("after shutdown").await;
    assert!(
        matches!(&later, Err(SdkError::Other(message)) if message.contains("shut down")),
        "post-shutdown prompt must be rejected, got: {later:?}"
    );
    // Detach the wedged run task; it never settles by design.
    run.abort();
}

/// A stream built BEFORE `shutdown()` must not start a run afterwards. The
/// spawned prompt task does not poll until the caller yields, so the natural
/// "create stream, await shutdown, consume stream" order used to start a run
/// against a torn-down plugin surface: the shutdown flag must be read when the
/// task actually runs, not when `prompt_stream` returns.
#[tokio::test(flavor = "current_thread")]
async fn prompt_stream_created_before_shutdown_does_not_start_a_run() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = Arc::clone(&calls);
    let registry = fake_registry(Arc::new(move |_index, _context| {
        calls_for_provider.fetch_add(1, Ordering::SeqCst);
        assistant(vec![text("late")], StopReason::Stop)
    }));
    let agent = agent_with(registry, Vec::new()).await;

    let events = agent.prompt_stream("late prompt");
    agent.shutdown().await.expect("shutdown");

    let collected: Vec<SdkEvent> = events.collect().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a shutdown agent must not issue a provider request for a pre-created stream"
    );
    assert!(
        matches!(
            collected.first(),
            Some(SdkEvent::PromptRejected { message }) if message.contains("shut down")
        ),
        "the pre-created stream must report the shutdown rejection: {collected:?}"
    );
    assert!(
        matches!(
            collected.last(),
            Some(SdkEvent::AgentSettled { aborted: false })
        ),
        "the rejected stream still settles: {collected:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-2: usage_totals must fold EVERY turn's usage, not only the terminal
// message's.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_totals_sum_every_turn_of_a_run() {
    let registry = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant_with_usage(
                vec![tool_call("call-1", "word_count", BTreeMap::new())],
                StopReason::ToolUse,
                100,
            )
        } else {
            assistant_with_usage(vec![text("done")], StopReason::Stop, 50)
        }
    }));
    let tool: Arc<dyn AgentTool> = Arc::new(EchoTool {
        runs: Arc::new(AtomicUsize::new(0)),
    });
    let agent = agent_with(registry, vec![tool]).await;

    let outcome = agent.prompt("count words").await.expect("prompt");
    assert_eq!(outcome.turns, 2);
    // The outcome carries the TERMINAL message's usage only...
    assert_eq!(outcome.usage.total_tokens, 50);
    // ...while the session totals fold every turn of the run.
    let totals = agent.usage_totals();
    assert_eq!(totals.turns, 2);
    assert_eq!(
        totals.total_tokens, 150,
        "multi-turn usage must accumulate (regression: only the terminal message counted)"
    );
}

// ---------------------------------------------------------------------------
// P2-11: conflicting model setups and zero context limits are build errors.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conflicting_model_setups_are_rejected() {
    let endpoint = Endpoint::new(
        "http://localhost:9/v1",
        pir::sdk::ApiKind::OpenAiCompletions,
        "m",
    );
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));

    let error = AgentBuilder::new()
        .endpoint(endpoint.clone())
        .model("some-model")
        .no_session()
        .build()
        .await
        .expect_err("endpoint + model must be rejected");
    assert!(
        error
            .to_string()
            .contains("conflicting model configuration"),
        "{error}"
    );

    let error = AgentBuilder::new()
        .endpoint(endpoint.clone())
        .with_registry(Arc::clone(&registry), fake_model())
        .no_session()
        .build()
        .await
        .expect_err("endpoint + registry must be rejected");
    assert!(error
        .to_string()
        .contains("conflicting model configuration"));

    let error = AgentBuilder::new()
        .model("some-model")
        .with_registry(registry, fake_model())
        .no_session()
        .build()
        .await
        .expect_err("model + registry must be rejected");
    assert!(error
        .to_string()
        .contains("conflicting model configuration"));
}

#[tokio::test]
async fn zero_context_limits_are_rejected() {
    let mut endpoint = Endpoint::new(
        "http://localhost:9/v1",
        pir::sdk::ApiKind::OpenAiCompletions,
        "m",
    );
    endpoint.context_window = 0;
    let error = AgentBuilder::new()
        .endpoint(endpoint)
        .no_session()
        .build()
        .await
        .expect_err("zero context_window must be rejected");
    assert!(error.to_string().contains("non-zero"), "{error}");

    let mut endpoint = Endpoint::new(
        "http://localhost:9/v1",
        pir::sdk::ApiKind::OpenAiCompletions,
        "m",
    );
    endpoint.max_tokens = 0;
    let error = AgentBuilder::new()
        .endpoint(endpoint)
        .no_session()
        .build()
        .await
        .expect_err("zero max_tokens must be rejected");
    assert!(error.to_string().contains("non-zero"), "{error}");

    let mut model = fake_model();
    model.context_window = 0;
    let error = AgentBuilder::new()
        .with_registry(
            fake_registry(Arc::new(|_index, _context| {
                assistant(vec![text("ok")], StopReason::Stop)
            })),
            model,
        )
        .no_session()
        .build()
        .await
        .expect_err("zero model context_window must be rejected");
    assert!(error.to_string().contains("non-zero"), "{error}");
}

// ---------------------------------------------------------------------------
// P2-11: the builder api_key reaches the stream options.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn builder_api_key_reaches_stream_options() {
    let (registry, seen) = fake_registry_with_keys(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .api_key("sk-builder-override")
        .no_session()
        .no_compaction()
        .cwd(temp_cwd())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");
    agent.prompt("hi").await.expect("prompt");
    let keys = seen.lock().unwrap().clone();
    assert_eq!(
        keys,
        vec![Some("sk-builder-override".to_owned())],
        "the builder api_key must reach the provider stream options"
    );
}

// ---------------------------------------------------------------------------
// P2-9: SdkError classification preserves the error sources.
// ---------------------------------------------------------------------------

#[test]
fn sdk_error_classification_preserves_sources() {
    // AI-layer errors classify as Provider.
    let error = SdkError::from(anyhow::anyhow!(pir::ai::error::AiError::UnknownProvider(
        "zz".to_owned()
    )));
    assert!(matches!(error, SdkError::Provider(_)), "{error}");

    // Top-level I/O errors keep their own display text.
    let error = SdkError::from(anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "disk on fire",
    )));
    assert!(
        matches!(&error, SdkError::Io(io)
            if io.kind() == std::io::ErrorKind::PermissionDenied && io.to_string() == "disk on fire"),
        "regression: the io error text was rebuilt from the anyhow chain: {error}"
    );

    // Context-wrapped I/O errors keep the io error's display, not the full
    // chain text.
    let error = SdkError::from(
        anyhow::Error::new(std::io::Error::other("disk on fire"))
            .context("failed to persist session entry"),
    );
    assert!(
        matches!(&error, SdkError::Io(io) if io.to_string() == "disk on fire"),
        "{error}"
    );
}

#[tokio::test]
async fn session_storage_failures_classify_through_the_run_boundary() {
    let temp = TempDir::new("readonly-session");
    let mut session = SessionManager::create(".", &temp.0).expect("create session");
    // Materialize the file (it is created lazily on the first message).
    session
        .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
            role: pir::ai::types::UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: "seed".to_owned(),
                    text_signature: None,
                },
            )],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        }))
        .expect("seed");
    let path = session.path().to_path_buf();
    drop(session);

    // Make the session file read-only so the run's first append fails.
    let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() & !0o222);
    }
    #[cfg(windows)]
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).expect("set read-only");

    // Root bypasses Unix mode bits, so the append below would succeed and the
    // test could not exercise the storage-failure classifier (the CI runner
    // and normal developer accounts are non-root). The behavioural assertion
    // is still covered there.
    #[cfg(unix)]
    if std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
    {
        return;
    }

    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .session(SessionMode::Existing(path.clone()))
        .no_compaction()
        .cwd(temp.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");

    let error = agent
        .prompt("hi")
        .await
        .expect_err("a read-only session file must fail the run");
    // The run-boundary classifier maps session storage failures (neither AI
    // nor I/O typed errors) to SdkError::Session.
    assert!(
        matches!(&error, SdkError::Session(message) if message.contains("cannot append to session")),
        "storage failures must classify as Session, got: {error}"
    );

    // Restore writability so the temp dir can be cleaned up on Windows
    // (`from_mode` clears the read-only flag via the write bits, which keeps
    // clippy's `permissions_set_readonly_false` lint happy).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
        permissions.set_mode(permissions.mode() | 0o222);
        std::fs::set_permissions(&path, permissions).expect("restore permissions");
    }
    #[cfg(windows)]
    {
        // `set_readonly(false)` is the only stable way to restore writability
        // on Windows (`PermissionsExt::from_mode` is unstable there).
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions).expect("restore permissions");
        }
    }
}

// ---------------------------------------------------------------------------
// System prompt: explicit replacement vs. append-only addendum
// ---------------------------------------------------------------------------

/// Build an SDK agent whose fake provider records the system prompt of every
/// request. `explicit`/`append` map to `.system_prompt(..)` /
/// `.append_system_prompt(..)` exactly as given.
async fn prompt_capturing_agent(
    explicit: Option<&str>,
    append: Option<&str>,
) -> (Agent, Arc<Mutex<Vec<Option<String>>>>, TempDir) {
    let cwd = TempDir::new("sdk-prompt");
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_provider = Arc::clone(&seen);
    let registry = fake_registry(Arc::new(move |_index, context| {
        seen_for_provider
            .lock()
            .unwrap()
            .push(context.system_prompt.clone());
        assistant(vec![text("ok")], StopReason::Stop)
    }));
    let mut builder = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .core_tools(true)
        .no_session()
        .no_compaction()
        .cwd(cwd.0.clone())
        .thinking_level(ThinkingLevel::Off);
    if let Some(prompt) = explicit {
        builder = builder.system_prompt(prompt);
    }
    if let Some(text) = append {
        builder = builder.append_system_prompt(text);
    }
    let agent = builder.build().await.expect("build agent");
    (agent, seen, cwd)
}

/// Run one prompt and return the system prompt the request carried.
async fn observed_system_prompt(agent: &Agent, seen: &Arc<Mutex<Vec<Option<String>>>>) -> String {
    agent.prompt("go").await.expect("prompt");
    seen.lock()
        .unwrap()
        .first()
        .cloned()
        .flatten()
        .expect("the request carries a system prompt")
}

/// Upstream `appendSystemPrompt` on its own is the `addendum` of the DEFAULT
/// prompt (`system-prompt.ts:172`), not a `customPrompt`: the SDK builder must
/// not let `.append_system_prompt(..)` alone become the WHOLE prompt and flip
/// the session into "explicit override" mode (which would drop the base
/// `<tools>`/`<rules>`/`<cwd>` sections and stop active-set rebuilds).
#[tokio::test]
async fn append_only_extends_the_derived_system_prompt() {
    let _env = crate::common::AgentDirEnvGuard::scratch("sdk-append-prompt", None).await;
    let (agent, seen, _cwd) = prompt_capturing_agent(None, Some("APPENDIX-FROM-CALLER")).await;
    let prompt = observed_system_prompt(&agent, &seen).await;

    assert!(
        prompt.contains("<tools>"),
        "the derived base prompt stays in place: {prompt}"
    );
    assert!(
        prompt.contains("<rules>") && prompt.contains("<cwd>"),
        "the append must not replace the base sections: {prompt}"
    );
    assert!(
        prompt.trim_end().ends_with("APPENDIX-FROM-CALLER"),
        "the appendix is the last section: {prompt}"
    );
    assert_ne!(
        prompt.trim(),
        "APPENDIX-FROM-CALLER",
        "append-only must never become the whole prompt: {prompt}"
    );
}

/// An explicit `.system_prompt(..)` still replaces the derived prompt.
#[tokio::test]
async fn explicit_system_prompt_still_replaces_the_derived_prompt() {
    let _env = crate::common::AgentDirEnvGuard::scratch("sdk-explicit-prompt", None).await;
    let (agent, seen, _cwd) = prompt_capturing_agent(Some("CALLER PROMPT"), None).await;
    let prompt = observed_system_prompt(&agent, &seen).await;

    assert_eq!(prompt, "CALLER PROMPT");
    assert!(
        !prompt.contains("<tools>"),
        "an explicit prompt is not extended with the base sections: {prompt}"
    );
}

/// `.system_prompt(..).append_system_prompt(..)` keeps appending to the
/// EXPLICIT prompt (the pre-existing contract).
#[tokio::test]
async fn explicit_system_prompt_with_append_keeps_the_explicit_prompt() {
    let _env = crate::common::AgentDirEnvGuard::scratch("sdk-explicit-append", None).await;
    let (agent, seen, _cwd) =
        prompt_capturing_agent(Some("CALLER PROMPT"), Some("CALLER APPENDIX")).await;
    let prompt = observed_system_prompt(&agent, &seen).await;

    assert_eq!(prompt, "CALLER PROMPT\n\nCALLER APPENDIX");
    assert!(
        !prompt.contains("<tools>"),
        "the derived base prompt stays suppressed: {prompt}"
    );
}

/// No explicit prompt and no appendix: the session sends the derived base
/// prompt (the P7 behavior append-only must not regress).
#[tokio::test]
async fn no_explicit_prompt_yields_the_derived_system_prompt() {
    let _env = crate::common::AgentDirEnvGuard::scratch("sdk-derived-prompt", None).await;
    let (agent, seen, _cwd) = prompt_capturing_agent(None, None).await;
    let prompt = observed_system_prompt(&agent, &seen).await;

    assert!(
        prompt.contains("<tools>") && prompt.contains("<rules>") && prompt.contains("<cwd>"),
        "the derived prompt carries the base sections: {prompt}"
    );
    assert!(
        !prompt.contains("APPENDIX"),
        "no addendum was configured: {prompt}"
    );
}

// ---------------------------------------------------------------------------
// Resumed VIRTUAL-model sessions: the builder resolves `projection.model`
// BEFORE `AgentSession::new` runs its constructor-time restore, so the resume
// path restores the branch's virtual registrations first (shared helper).
// ---------------------------------------------------------------------------

/// The `pi.virtual-model-state` custom entry the session persists for a virtual
/// model (`persist_virtual_model_state`). `virtual-x` routes to the PHYSICAL
/// `fake/fake-other`, while `settings.json` defaults to `fake/fake-model` — so
/// a resumed session that really restored the virtual selection routes to
/// `fake-other`, and one that fell back to the default asks for `fake-model`.
fn virtual_state() -> serde_json::Value {
    serde_json::json!({
        "id": "virtual-x",
        "provider": "fake",
        "name": "Virtual X",
        "physical": { "provider": "fake", "id": "fake-other" },
    })
}

/// A models.json with one authenticated provider carrying `fake-model` and
/// `fake-other`, served by the loopback endpoint `base_url`.
fn write_resume_models(agent_dir: &std::path::Path, base_url: &str) {
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "providers": {
                "fake": {
                    "api": "openai-completions",
                    "baseUrl": format!("{base_url}/v1"),
                    "apiKey": "sdk-resume-virtual-key",
                    "models": [{ "id": "fake-model" }, { "id": "fake-other" }]
                }
            }
        }))
        .expect("serialize models.json"),
    )
    .expect("write models.json");
}

/// A session recording `fake/virtual-x`: optionally its registration entry,
/// always the `model_change` a routed turn leaves behind.
fn record_virtual_session(
    dir: &std::path::Path,
    registration: Option<serde_json::Value>,
) -> std::path::PathBuf {
    let mut storage =
        SessionManager::create(&dir.display().to_string(), dir).expect("create session");
    if let Some(state) = registration {
        storage
            .append_custom_entry(
                pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY,
                Some(serde_json::json!({
                    "provider": "fake",
                    "modelId": "virtual-x",
                    "state": state,
                })),
            )
            .expect("append virtual state");
    }
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("hello")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append user");
    storage
        .append_model_change("fake", "virtual-x")
        .expect("append virtual selection");
    storage.path().to_path_buf()
}

/// A loopback OpenAI-completions endpoint that answers every request with one
/// text delta and records the request body's `model` field.
fn resume_endpoint() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind endpoint");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let models_for_thread = Arc::clone(&models);
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            let header_end = loop {
                let Ok(count) = stream.read(&mut chunk) else {
                    break 0;
                };
                if count == 0 {
                    break 0;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            if header_end == 0 {
                continue;
            }
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let Ok(count) = stream.read(&mut chunk) else {
                    break;
                };
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes[header_end..]) {
                if let Some(model) = body.get("model").and_then(serde_json::Value::as_str) {
                    models_for_thread.lock().unwrap().push(model.to_owned());
                }
            }
            let body = concat!(
                "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"routed\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (base_url, models)
}

/// Resuming a branch whose recorded selection is VIRTUAL must route to that
/// virtual id, not the environment default: the registration persisted next to
/// the `model_change` is restored into the registry overlay BEFORE the
/// projection model is resolved, so the agent carries the virtual selection and
/// the next prompt routes to the virtual model's PHYSICAL target.
#[tokio::test(flavor = "current_thread")]
async fn resumed_session_restores_the_recorded_virtual_model_selection() {
    let (base_url, requested_models) = resume_endpoint();
    let _env = crate::common::AgentDirEnvGuard::scratch(
        "sdk-resume-virtual",
        Some(r#"{"defaultProvider":"fake","defaultModel":"fake-model"}"#),
    )
    .await;
    let agent_dir = _env.dir().expect("scratch dir").to_path_buf();
    write_resume_models(&agent_dir, &base_url);
    let sessions = TempDir::new("resume-virtual");
    let path = record_virtual_session(&sessions.0, Some(virtual_state()));

    let agent = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .session(SessionMode::Existing(path))
        .no_compaction()
        .cwd(sessions.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("the persisted registration must restore the virtual selection");

    assert_eq!(
        (agent.model().provider.as_str(), agent.model().id.as_str()),
        ("fake", "virtual-x"),
        "a resume must restore the recorded VIRTUAL selection, not the default model"
    );

    agent.prompt("hello").await.expect("routed prompt");
    assert_eq!(
        requested_models.lock().unwrap().clone(),
        vec!["fake-other".to_owned()],
        "the prompt must route through the virtual selection to its physical target"
    );

    // `attach_session` swaps storage on a live agent: the same restore-before-
    // resolve rule applies there (the branch's virtual registration must be in
    // the overlay before `find_model` looks the recorded id up).
    let other = TempDir::new("resume-virtual-attach");
    let attach_path = record_virtual_session(&other.0, Some(virtual_state()));
    let fresh = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .session(SessionMode::Dir(other.0.clone()))
        .no_compaction()
        .cwd(other.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("fresh agent");
    assert_eq!(fresh.model().id, "fake-model", "the fresh default first");
    fresh
        .attach_session(&attach_path)
        .await
        .expect("attach the virtual session");
    assert_eq!(
        (fresh.model().provider.as_str(), fresh.model().id.as_str()),
        ("fake", "virtual-x"),
        "attach_session must restore the branch's virtual selection too"
    );
}

/// A branch states a virtual `model_change` whose persisted registration cannot
/// be rebuilt (here: the entry omits `physical`): the builder surfaces the
/// model-resolution error instead of silently keeping its default.
#[tokio::test(flavor = "current_thread")]
async fn unresolvable_recorded_virtual_model_fails_the_resume() {
    let _env = crate::common::AgentDirEnvGuard::scratch(
        "sdk-resume-virtual-bad",
        Some(r#"{"defaultProvider":"fake","defaultModel":"fake-model"}"#),
    )
    .await;
    let agent_dir = _env.dir().expect("scratch dir").to_path_buf();
    write_resume_models(&agent_dir, "http://127.0.0.1:9");
    let sessions = TempDir::new("resume-virtual-bad");
    // Malformed: `physical` is required by `VirtualModelRegistration::from_wire`.
    let path = record_virtual_session(
        &sessions.0,
        Some(serde_json::json!({ "id": "virtual-x", "provider": "fake" })),
    );

    let error = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .session(SessionMode::Existing(path))
        .no_compaction()
        .cwd(sessions.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect_err("an unresolvable virtual selection must fail the resume");
    assert!(
        matches!(&error, SdkError::NoModel { message }
            if message.contains("virtual-x") && message.contains("cannot be restored")),
        "the error must name the virtual id and the broken registration, got: {error}"
    );
}

/// Non-virtual resumes are unchanged: the recorded physical model still wins,
/// and a recorded id the catalog no longer serves stays fail-soft (the resolved
/// default) rather than failing the build.
#[tokio::test(flavor = "current_thread")]
async fn physical_resume_behavior_is_unchanged() {
    let _env = crate::common::AgentDirEnvGuard::scratch(
        "sdk-resume-physical",
        Some(r#"{"defaultProvider":"fake","defaultModel":"fake-model"}"#),
    )
    .await;
    let agent_dir = _env.dir().expect("scratch dir").to_path_buf();
    write_resume_models(&agent_dir, "http://127.0.0.1:9");

    let sessions = TempDir::new("resume-physical");
    let mut storage =
        SessionManager::create(&sessions.0.display().to_string(), &sessions.0).expect("session");
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("hello")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append user");
    storage
        .append_model_change("fake", "fake-other")
        .expect("record a physical switch");
    let path = storage.path().to_path_buf();
    drop(storage);
    let agent = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .session(SessionMode::Existing(path))
        .no_compaction()
        .cwd(sessions.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("a recorded physical model resumes");
    assert_eq!(
        (agent.model().provider.as_str(), agent.model().id.as_str()),
        ("fake", "fake-other"),
        "the recorded physical selection still wins over the default"
    );

    // A stale physical id (no catalog entry, no registration): fail-soft.
    let stale = TempDir::new("resume-physical-stale");
    let mut storage =
        SessionManager::create(&stale.0.display().to_string(), &stale.0).expect("session");
    storage
        .append_model_change("fake", "retired-model")
        .expect("record a stale model");
    let path = storage.path().to_path_buf();
    drop(storage);
    let agent = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .session(SessionMode::Existing(path))
        .no_compaction()
        .cwd(stale.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("a stale physical id must not fail the resume");
    assert_eq!(
        agent.model().id,
        "fake-model",
        "a stale physical id keeps the resolved default"
    );
}

/// P2-L: a strict `:thinking` suffix on `.model(..)` is an explicit choice
/// and must survive resume/attach, exactly like `.thinking_level(..)`.
/// Without the suffix the branch's recorded level still wins (unpinned).
#[tokio::test(flavor = "current_thread")]
async fn model_thinking_suffix_is_pinned_on_resume_and_attach() {
    // Every builder below passes `.agent_dir()` explicitly, so this fixture
    // does not need the process-wide PIR_AGENT_DIR pin (and must not hold its
    // lock while three async builds run).
    let agent_dir = TempDir::new("sdk-thinking-suffix");
    let agent_dir = agent_dir.0.clone();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProvider":"fake","defaultModel":"fake-model"}"#,
    )
    .expect("write settings.json");
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::json!({
            "providers": {
                "fake": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "sdk-thinking-suffix-key",
                    "models": [{ "id": "fake-model", "reasoning": true }]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    let sessions = TempDir::new("thinking-suffix");
    let mut storage =
        SessionManager::create(&sessions.0.display().to_string(), &sessions.0).expect("session");
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("hello")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append user");
    storage
        .append_thinking_level_change("low")
        .expect("record the branch thinking level");
    let path = storage.path().to_path_buf();
    drop(storage);

    // Resume: the explicit suffix (high) wins over the recorded low.
    let pinned = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .model("fake/fake-model:high")
        .session(SessionMode::Existing(path.clone()))
        .no_compaction()
        .cwd(sessions.0.clone())
        .build()
        .await
        .expect("resume with a thinking suffix");
    assert_eq!(
        pinned.thinking_level(),
        ThinkingLevel::High,
        "the strict `:thinking` suffix must be pinned on resume"
    );

    // No suffix: the recorded branch level still restores (unchanged rule).
    let unpinned = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .model("fake/fake-model")
        .session(SessionMode::Existing(path.clone()))
        .no_compaction()
        .cwd(sessions.0.clone())
        .build()
        .await
        .expect("resume without a thinking suffix");
    assert_eq!(
        unpinned.thinking_level(),
        ThinkingLevel::Low,
        "without an explicit suffix the recorded branch level still wins"
    );

    // attach_session on a live agent: the suffixed level survives the swap.
    let fresh = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .model("fake/fake-model:high")
        .session(SessionMode::Dir(sessions.0.clone()))
        .no_compaction()
        .cwd(sessions.0.clone())
        .build()
        .await
        .expect("fresh agent with a thinking suffix");
    fresh
        .attach_session(&path)
        .await
        .expect("attach the recorded session");
    assert_eq!(
        fresh.thinking_level(),
        ThinkingLevel::High,
        "the strict `:thinking` suffix must be pinned across attach_session"
    );

    // Explicit `.thinking_level(..)` keeps its documented precedence.
    let explicit = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .model("fake/fake-model:high")
        .thinking_level(ThinkingLevel::Off)
        .no_session()
        .no_compaction()
        .cwd(sessions.0.clone())
        .build()
        .await
        .expect("explicit thinking level wins");
    assert_eq!(explicit.thinking_level(), ThinkingLevel::Off);
}

/// `attach_session` restores the recorded model, and the thinking level must
/// be re-clamped against it even when the branch records NO explicit level:
/// `Agent::thinking_level` promises the effective level, and carrying the old
/// model's `high` onto a non-reasoning restored model would report a level the
/// live model cannot use (the next request would silently send `off`).
#[tokio::test(flavor = "current_thread")]
async fn attach_session_reclamps_thinking_when_the_branch_records_no_level() {
    let _env = crate::common::AgentDirEnvGuard::scratch(
        "sdk-attach-thinking-clamp",
        Some(
            r#"{"defaultProvider":"fake","defaultModel":"reasoning-model",
                "defaultThinkingLevel":"high"}"#,
        ),
    )
    .await;
    let agent_dir = _env.dir().expect("scratch dir").to_path_buf();
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::json!({
            "providers": {
                "fake": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "sdk-attach-thinking-clamp-key",
                    "models": [
                        { "id": "reasoning-model", "reasoning": true },
                        { "id": "plain-model" }
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    let sessions = TempDir::new("attach-thinking-clamp");
    let model_select_sources: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_sources = Arc::clone(&model_select_sources);
    let agent = AgentBuilder::new()
        .agent_dir(&agent_dir)
        .session(SessionMode::Dir(sessions.0.clone()))
        .no_compaction()
        .cwd(sessions.0.clone())
        .on_event("model_select", move |payload| {
            if let Some(source) = payload["source"].as_str() {
                captured_sources.lock().unwrap().push(source.to_owned());
            }
        })
        .build()
        .await
        .expect("build agent");
    assert_eq!(agent.model().id, "reasoning-model");
    assert_eq!(
        agent.thinking_level(),
        ThinkingLevel::High,
        "the settings default starts the reasoning model at high"
    );

    // The branch switches to a model that does NOT reason and records no
    // thinking-level change of its own.
    let mut storage =
        SessionManager::create(&sessions.0.display().to_string(), &sessions.0).expect("session");
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("hello")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append user");
    storage
        .append_model_change("fake", "plain-model")
        .expect("record the model switch");
    let path = storage.path().to_path_buf();
    drop(storage);

    agent.attach_session(&path).await.expect("attach session");
    assert_eq!(agent.model().id, "plain-model");
    assert_eq!(
        model_select_sources.lock().unwrap().clone(),
        vec!["restore".to_owned()],
        "attaching a session reports the upstream `model_select` source `restore`, \
         not a user-initiated `set`"
    );
    assert_eq!(
        agent.thinking_level(),
        ThinkingLevel::Off,
        "the restored non-reasoning model must clamp the carried level to off \
         even without a recorded thinking-level entry"
    );
}

// ---------------------------------------------------------------------------
// Session switches must also refuse while a MANUAL COMPACTION owns the session
// ---------------------------------------------------------------------------

/// A plugin that parks `session_before_compact` until the gate opens.
struct CompactGatePlugin {
    info: pir::plugins::PluginInfo,
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl pir::plugins::AgentPlugin for CompactGatePlugin {
    fn info(&self) -> &pir::plugins::PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["session_before_compact".to_owned()])
    }

    fn on_event<'a>(
        &'a self,
        event: pir::plugins::PluginEvent<'a>,
    ) -> pir::plugins::PluginFuture<'a, pir::plugins::PluginEventOutcome> {
        if event.name.as_ref() != "session_before_compact" {
            return Box::pin(async { pir::plugins::PluginEventOutcome::None });
        }
        let entered = Arc::clone(&self.entered);
        let mut gate = self.gate.clone();
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if *gate.borrow() {
                        break;
                    }
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            pir::plugins::PluginEventOutcome::None
        })
    }
}

/// `is_streaming()` is false during a manual compaction, but the session still
/// owns its storage: a transcript swap there would let the compaction append
/// its summary to the replacement session. Every switch path must refuse, and
/// accept again once the compaction settles.
#[tokio::test(flavor = "current_thread")]
async fn session_switches_are_rejected_while_a_manual_compaction_is_in_flight() {
    let dir = TempDir::new("compact-switch-guard");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));

    // Seed a compactable transcript in a real session file.
    let mut storage =
        SessionManager::create(&dir.0.display().to_string(), &dir.0).expect("create session");
    for index in 0..2u64 {
        storage
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![text(&format!("seed-{index}-{}", "x".repeat(400)))],
                tools_added: None,
                tools_removed: None,
                timestamp: index + 1,
                sections: None,
            }))
            .expect("seed user message");
        storage
            .append_message(Message::Assistant(assistant(
                vec![text(&format!("reply-{index}-{}", "y".repeat(400)))],
                StopReason::Stop,
            )))
            .expect("seed assistant message");
    }
    let path = storage.path().to_path_buf();
    drop(storage);

    let registry = fake_registry(Arc::new(|_index, context| {
        if context.system_prompt.as_deref()
            == Some(pir::compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT)
        {
            return assistant(vec![text("COMPACT SUMMARY")], StopReason::Stop);
        }
        assistant(vec![text("unexpected")], StopReason::Stop)
    }));
    let agent = Arc::new(
        AgentBuilder::new()
            .with_registry(registry, fake_model())
            .session(SessionMode::Existing(path.clone()))
            .compaction(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 8,
            })
            .cwd(dir.0.clone())
            .thinking_level(ThinkingLevel::Off)
            .with_plugin(Arc::new(CompactGatePlugin {
                info: pir::plugins::PluginInfo::inline("compact-gate"),
                gate: gate_rx,
                entered: Arc::clone(&entered),
            }))
            .build()
            .await
            .expect("build agent"),
    );

    let compact = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.compact().await }
    });
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        entered.load(Ordering::SeqCst) > 0,
        "the compaction never reached session_before_compact"
    );
    assert!(
        !agent.is_streaming(),
        "a manual compaction is not a prompt run"
    );

    for (label, error) in [
        ("new_session", agent.new_session().await.err()),
        ("reset", agent.reset().await.err()),
        ("attach_session", agent.attach_session(&path).await.err()),
        ("continue_session", agent.continue_session().await.err()),
    ] {
        let error = error.unwrap_or_else(|| panic!("{label} must be refused mid-compaction"));
        assert!(
            matches!(&error, SdkError::Other(message) if message.contains("compaction")),
            "{label} must name the compaction, got: {error}"
        );
    }

    // A prompt during the compaction is rejected by the session claim. That is
    // a run-path PRECONDITION, not a storage failure: `SdkError::Session` must
    // keep meaning "the transcript could not be persisted".
    let busy = agent
        .prompt("blocked while compacting")
        .await
        .expect_err("a prompt during a compaction must be rejected");
    assert!(
        matches!(&busy, SdkError::Other(message) if message.contains("compaction is in progress")),
        "the compaction rejection is a precondition, not a session storage error: {busy:?}"
    );

    let _ = gate_tx.send(true);
    let result = compact.await.expect("compact join").expect("compaction");
    assert!(
        !result.summary.is_empty(),
        "the compaction produced a summary"
    );

    // The guard is not permanent: the switch works once the compaction ends.
    agent.new_session().await.expect("switch after compaction");
}
