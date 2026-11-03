//! W10-J: native `ToolService::execute_tool` context + image fidelity.
//!
//! These pin the plugin-services half of the W8-D ordered native-call path:
//!
//! - `ctx.tools.execute_tool` on a live session hands a native plugin tool
//!   the session's real [`ToolContext`] — abort signal, session environment,
//!   nested-call executor, update channel, background flag and cwd — instead
//!   of the host's cwd-only synthetic context;
//! - a native tool's image result survives into the finalized tool result
//!   (`ToolResult.images`), alongside `details` / `structuredContent`;
//! - callers without a context (the legacy `PluginHost::call_tool` wrapper)
//!   keep their pre-W10 behavior: a synthetic context and a successful wire
//!   result.
//!
//! The suite is intentionally unregistered; the foreman registers it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, Context, ImageContent, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, ThinkingLevel,
};
use pir::plugins::plugin::services::ExecuteToolOptions;
use pir::plugins::plugin::{AgentPlugin, PluginContext, RegistrationContext};
use pir::plugins::{
    EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost, PluginInfo,
};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One tool execution's observations, recorded by [`ProbeTool`].
#[derive(Debug)]
struct SeenContext {
    aborted: bool,
    background: bool,
    cwd: PathBuf,
    session_model: Option<String>,
    has_session_env: bool,
    has_nested: bool,
    has_update_channel: bool,
}

/// Native plugin tool that records the [`ToolContext`] it ran under and,
/// when `image` is set, returns an image plus details/structured content.
struct ProbeTool {
    name: &'static str,
    seen: Arc<Mutex<Option<SeenContext>>>,
    image: bool,
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "W10-J native ToolService context probe"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object", "properties": {} }))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SeenContext {
            aborted: ctx.is_aborted(),
            background: ctx.background,
            cwd: ctx.cwd.clone(),
            session_model: ctx.session_env.as_ref().map(|env| env.model.id.clone()),
            has_session_env: ctx.session_env.is_some(),
            has_nested: ctx.nested.is_some(),
            has_update_channel: ctx.update_channel().is_some(),
        });
        let mut result = ToolResult::ok("probe-ok");
        if self.image {
            result.images.push(ImageContent {
                data: "aW1n".to_owned(),
                mime_type: "image/png".to_owned(),
            });
            result.details = Some(json!({ "detail": true }));
            result.structured_content = Some(json!({ "structured": true }));
        }
        Box::pin(async move { result })
    }
}

/// Inline plugin registering the probes and capturing its bound context.
struct ProbePlugin {
    info: PluginInfo,
    tools: Vec<Arc<dyn AgentTool>>,
    contexts: Mutex<Vec<PluginContext>>,
}

impl ProbePlugin {
    fn new(name: &str, tools: Vec<Arc<dyn AgentTool>>) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            tools,
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the host bound a context")
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        for tool in &self.tools {
            ctx.register_tool(Arc::clone(tool));
        }
        Ok(())
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

/// Keyless provider that never runs (the tests exercise tools directly).
struct IdleProvider;

impl Provider for IdleProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            pir::ai::types::TerminalReason::Stop,
            AssistantMessage {
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            },
        ));
        stream
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

/// Provider whose stream stays open until the test releases it, so the
/// session remains SESSION_RUNNING across a `session.abort()`.
struct HangingProvider {
    /// Fired once `stream()` was entered (the run has claimed the session and
    /// installed its signal).
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Provider for HangingProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Hanging fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        let sink = stream.clone();
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        tokio::spawn(async move {
            started.notify_one();
            release.notified().await;
            sink.push(AssistantMessageEventStream::done_event(
                pir::ai::types::TerminalReason::Stop,
                AssistantMessage {
                    api: "openai-completions".to_owned(),
                    provider: "fake".to_owned(),
                    model: "model-a".to_owned(),
                    stop_reason: StopReason::Stop,
                    ..AssistantMessage::default()
                },
            ));
        });
        stream
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
}

fn fake_model() -> Model {
    Model {
        id: "model-a".to_owned(),
        name: "Model A".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "fake".to_owned(),
        base_url: "http://fake.invalid/v1".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
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

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-services-w10-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn session_config(cwd: &Path) -> AgentSessionConfig {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Low,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: Some("W10-J plugin services test prompt".to_owned()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn live_session(cwd: &Path, plugin: Arc<ProbePlugin>) -> (Arc<AgentSession>, Arc<ProbePlugin>) {
    let session = AgentSession::new_arc_with_plugins(
        session_config(cwd),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    (session, plugin)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `execute_tool` must hand the native plugin tool the session's real
/// context: abort signal, session environment, nested executor, update
/// channel, background flag and cwd.
#[tokio::test(flavor = "current_thread")]
async fn native_plugin_tool_sees_session_context_from_execute_tool() {
    let temp = TempDir::new("context");
    let seen = Arc::new(Mutex::new(None));
    let plugin = ProbePlugin::new(
        "w10-context",
        vec![Arc::new(ProbeTool {
            name: "context-probe",
            seen: Arc::clone(&seen),
            image: false,
        })],
    );
    let (session, plugin) = live_session(temp.path(), plugin);
    let context = plugin.last_context();
    let tools = context
        .require_tools()
        .expect("the live session binds a ToolService");

    // An abort BETWEEN runs is a no-op for the next run: the signal slot is
    // only installed by a run/compaction start and keeps the aborted state
    // afterwards, so an idle service call must get a FRESH signal instead of
    // the stale aborted one (which made bash/write/edit fail with
    // "Operation aborted" until the next prompt).
    session.abort();
    let outcome = tools
        .execute_tool(
            "caller-context",
            "context-probe",
            json!({}),
            ExecuteToolOptions::default(),
        )
        .await
        .expect("the native plugin tool runs");
    assert_eq!(outcome.tool_call_id, "caller-context/0");
    assert!(!outcome.is_error);
    assert_eq!(outcome.result.content, "probe-ok");

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the probe tool recorded its context");
    assert!(
        !seen.aborted,
        "an idle abort must not leak into a later service call's context"
    );
    assert_eq!(seen.cwd, temp.path(), "the tool sees the session cwd");
    assert_eq!(
        seen.session_model.as_deref(),
        Some("model-a"),
        "the session environment carries the live model"
    );
    assert!(seen.has_session_env, "session_env must be populated");
    assert!(
        seen.has_nested,
        "the tool must be able to execute nested calls"
    );
    assert!(
        seen.has_update_channel,
        "the live partial-update channel must be wired"
    );
    assert!(
        !seen.background,
        "a default service call must not mark the tool background"
    );
}

/// While a run is in flight, `session.abort()` must still reach the native
/// service tool context. The service call now runs through the same scheduler
/// as a model-issued call, so an aborted run refuses the call before the tool
/// body runs (the model-issued batch's fail-closed abort semantics) instead of
/// handing the tool an already-aborted signal. The provider stream is held
/// open so the session stays RUNNING across the assertion.
#[tokio::test(flavor = "current_thread")]
async fn running_abort_refuses_the_native_service_tool_call() {
    let temp = TempDir::new("running-abort");
    let seen = Arc::new(Mutex::new(None));
    let plugin = ProbePlugin::new(
        "w10-running-abort",
        vec![Arc::new(ProbeTool {
            name: "run-probe",
            seen: Arc::clone(&seen),
            image: false,
        })],
    );
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HangingProvider {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    }));
    let session = AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Low,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 2,
            system_prompt: Some("W10-J running abort test prompt".to_owned()),
            cwd: Some(temp.path().to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let context = plugin.last_context();
    let tools = context
        .require_tools()
        .expect("the live session binds a ToolService");

    let prompt_session = Arc::clone(&session);
    let prompt =
        tokio::spawn(async move { prompt_session.prompt("go", Arc::new(|_event| {})).await });
    // Wait for the provider request: the session has claimed RUNNING and the
    // run's own signal is in the slot (no start-up race with `abort()`).
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the provider stream never started");
    assert!(session.is_streaming(), "the run must be active");

    session.abort();
    // The provider stream stays open, so the session is still RUNNING here:
    // the slot signal (now aborted) reaches the scheduler exactly like a
    // model-issued call, and the call is finalized as an abort error instead
    // of running the tool with an already-cancelled signal.
    let outcome = tools
        .execute_tool(
            "caller-running",
            "run-probe",
            json!({}),
            ExecuteToolOptions::default(),
        )
        .await
        .expect("an aborted service call still returns a typed outcome");
    assert!(
        outcome.is_error,
        "an aborted run must refuse the service tool call"
    );
    assert!(
        outcome.result.content.contains("Operation aborted"),
        "the refusal must name the abort: {:?}",
        outcome.result.content
    );
    assert!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none(),
        "the tool body must not run after the run was aborted"
    );

    release.notify_one();
    let _ = prompt.await.expect("the prompt task joins");
}

/// A native tool's image result must survive the host call and land in the
/// finalized tool result, with details/structuredContent preserved.
#[tokio::test(flavor = "current_thread")]
async fn native_plugin_tool_images_survive_into_finalized_result() {
    let temp = TempDir::new("images");
    let seen = Arc::new(Mutex::new(None));
    let plugin = ProbePlugin::new(
        "w10-images",
        vec![Arc::new(ProbeTool {
            name: "image-probe",
            seen: Arc::clone(&seen),
            image: true,
        })],
    );
    let (_session, plugin) = live_session(temp.path(), plugin);
    let context = plugin.last_context();
    let tools = context
        .require_tools()
        .expect("the live session binds a ToolService");

    let outcome = tools
        .execute_tool(
            "caller-image",
            "image-probe",
            json!({}),
            ExecuteToolOptions::default(),
        )
        .await
        .expect("the native plugin tool runs");

    assert!(!outcome.is_error);
    assert_eq!(outcome.result.content, "probe-ok");
    assert_eq!(
        outcome.result.images.len(),
        1,
        "the tool's image block must survive into ToolResult.images"
    );
    assert_eq!(outcome.result.images[0].data, "aW1n");
    assert_eq!(outcome.result.images[0].mime_type, "image/png");
    assert_eq!(
        outcome.result.details,
        Some(json!({ "detail": true })),
        "details must be preserved"
    );
    assert_eq!(
        outcome.result.structured_content,
        Some(json!({ "structured": true })),
        "structuredContent must be preserved"
    );
}

/// The legacy `PluginHost::call_tool` wrapper (no caller context) keeps its
/// synthetic cwd-only context and its wire-result behavior.
#[tokio::test(flavor = "current_thread")]
async fn legacy_call_tool_without_context_still_works() {
    let seen = Arc::new(Mutex::new(None));
    let plugin = ProbePlugin::new(
        "w10-legacy",
        vec![Arc::new(ProbeTool {
            name: "legacy-probe",
            seen: Arc::clone(&seen),
            image: true,
        })],
    );
    let host = PluginHost::build(None, vec![plugin as Arc<dyn AgentPlugin>]);

    let result = host
        .call_tool("legacy-probe", "legacy-1", &json!({}))
        .await
        .expect("the legacy wrapper still dispatches native tools");
    assert!(
        result
            .content
            .iter()
            .any(|item| item.kind == "text" && item.text == "probe-ok"),
        "the legacy wire result keeps its text body: {:?}",
        result.content
    );
    assert!(
        result
            .content
            .iter()
            .any(|item| item.kind == "image" && item.data.as_deref() == Some("aW1n")),
        "the legacy wire result keeps image blocks: {:?}",
        result.content
    );

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the probe tool recorded its context");
    assert!(
        !seen.aborted && !seen.background,
        "the no-context wrapper keeps the synthetic defaults"
    );
    assert!(
        !seen.has_session_env && !seen.has_nested,
        "the no-context wrapper must not fabricate a session context"
    );
}

// ---------------------------------------------------------------------------
// P2-03: the FIRST `execute_tool` call goes through the scheduler/gate
// ---------------------------------------------------------------------------

/// Native plugin declaring both tool contracts and recording every payload.
struct HookProbePlugin {
    info: PluginInfo,
    tools: Vec<Arc<dyn AgentTool>>,
    contexts: Mutex<Vec<PluginContext>>,
    tool_calls: Mutex<Vec<Value>>,
    tool_results: Mutex<Vec<Value>>,
    result_notify: Arc<Notify>,
}

impl HookProbePlugin {
    fn new(name: &str, tools: Vec<Arc<dyn AgentTool>>) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            tools,
            contexts: Mutex::new(Vec::new()),
            tool_calls: Mutex::new(Vec::new()),
            tool_results: Mutex::new(Vec::new()),
            result_notify: Arc::new(Notify::new()),
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the host bound a context")
    }

    fn tool_calls(&self) -> Vec<Value> {
        self.tool_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn tool_results(&self) -> Vec<Value> {
        self.tool_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AgentPlugin for HookProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        for tool in &self.tools {
            ctx.register_tool(Arc::clone(tool));
        }
        Ok(())
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["tool_call".to_owned(), "tool_result".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        match event.name.as_ref() {
            "tool_call" => self
                .tool_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.payload.clone()),
            "tool_result" => {
                self.tool_results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.payload.clone());
                self.result_notify.notify_one();
            }
            _ => {}
        }
        Box::pin(async { PluginEventOutcome::None })
    }
}

struct HookProbeTool;

impl AgentTool for HookProbeTool {
    fn name(&self) -> &'static str {
        "hook-probe"
    }

    fn description(&self) -> &'static str {
        "P2-03 tool_call/tool_result hook probe"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object", "properties": {} }))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("hook-ok") })
    }
}

/// Tool that records the background flag it observed and signals when run.
struct BackgroundProbeTool {
    ran: Arc<Notify>,
    background: Arc<Mutex<Option<bool>>>,
}

impl AgentTool for BackgroundProbeTool {
    fn name(&self) -> &'static str {
        "bg-probe"
    }

    fn description(&self) -> &'static str {
        "P2-03 background detach probe"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object", "properties": {} }))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        *self
            .background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.background);
        self.ran.notify_one();
        Box::pin(async { ToolResult::ok("bg-ok") })
    }
}

fn hook_session(cwd: &Path, plugin: Arc<HookProbePlugin>) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(session_config(cwd), vec![plugin as Arc<dyn AgentPlugin>])
}

/// The first `ToolService::execute_tool` call must run the same `tool_call`
/// and `tool_result` contracts a model-issued call does. Before P2-03 the
/// seam called the plugin host / core tool directly and no hook observed the
/// call.
#[tokio::test(flavor = "current_thread")]
async fn execute_tool_first_call_dispatches_tool_call_and_tool_result_hooks() {
    let temp = TempDir::new("hooks");
    let plugin = HookProbePlugin::new("w10-hooks", vec![Arc::new(HookProbeTool)]);
    let _session = hook_session(temp.path(), Arc::clone(&plugin));
    let context = plugin.last_context();
    let tools = context
        .require_tools()
        .expect("the live session binds a ToolService");

    let outcome = tools
        .execute_tool(
            "caller-hooks",
            "hook-probe",
            json!({ "value": 1 }),
            ExecuteToolOptions::default(),
        )
        .await
        .expect("the native plugin tool runs");
    assert_eq!(outcome.tool_call_id, "caller-hooks/0");
    assert!(!outcome.is_error);
    assert_eq!(outcome.result.content, "hook-ok");

    let calls = plugin.tool_calls();
    assert_eq!(calls.len(), 1, "tool_call must fire exactly once");
    assert_eq!(calls[0]["toolName"], "hook-probe");
    assert_eq!(calls[0]["toolCallId"], "caller-hooks/0");
    assert_eq!(calls[0]["input"], json!({ "value": 1 }));

    let results = plugin.tool_results();
    assert_eq!(results.len(), 1, "tool_result must fire exactly once");
    assert_eq!(results[0]["toolName"], "hook-probe");
    assert_eq!(results[0]["toolCallId"], "caller-hooks/0");
    assert_eq!(results[0]["isError"], false);
    assert_eq!(
        results[0]["content"][0]["text"], "hook-ok",
        "tool_result carries the real result: {:?}",
        results[0]
    );
}

/// `options.background` must detach through the session's background sink,
/// exactly like a model-issued `background: true` call: `execute_tool`
/// returns the placeholder immediately, the tool runs with
/// `ToolContext.background == true`, and the real `tool_result` fires once at
/// completion with the same call id.
#[tokio::test(flavor = "current_thread")]
async fn execute_tool_background_detaches_and_delivers_tool_result() {
    let temp = TempDir::new("background");
    let ran = Arc::new(Notify::new());
    let background = Arc::new(Mutex::new(None));
    let plugin = HookProbePlugin::new(
        "w10-background",
        vec![Arc::new(BackgroundProbeTool {
            ran: Arc::clone(&ran),
            background: Arc::clone(&background),
        })],
    );
    let session = hook_session(temp.path(), Arc::clone(&plugin));
    let context = plugin.last_context();
    let tools = context
        .require_tools()
        .expect("the live session binds a ToolService");

    let outcome = tools
        .execute_tool(
            "caller-bg",
            "bg-probe",
            json!({}),
            ExecuteToolOptions { background: true },
        )
        .await
        .expect("the detached call returns its placeholder");
    assert_eq!(outcome.tool_call_id, "caller-bg/0");
    assert!(!outcome.is_error);
    assert!(
        outcome.result.content.contains("running in the background"),
        "a background service call must return the detach placeholder: {:?}",
        outcome.result.content
    );

    tokio::time::timeout(Duration::from_secs(5), ran.notified())
        .await
        .expect("the detached tool must actually run");
    assert_eq!(
        *background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some(true),
        "the detached call's context must carry background = true"
    );

    tokio::time::timeout(Duration::from_secs(5), plugin.result_notify.notified())
        .await
        .expect("the real tool_result must fire at completion");
    let results = plugin.tool_results();
    assert_eq!(
        results.len(),
        1,
        "the detached completion must fire exactly one tool_result"
    );
    assert_eq!(results[0]["toolCallId"], "caller-bg/0");
    assert_eq!(
        results[0]["content"][0]["text"], "bg-ok",
        "the completion's tool_result must carry the REAL result: {:?}",
        results[0]
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while session.background_call_count() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the detached call must settle"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
