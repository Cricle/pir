//! W8-D SDK/plugin residual-closure tests.
//!
//! Covers:
//! - `AgentBuilder` applying the settings defaults (`defaultProvider`,
//!   `defaultModel`, `defaultTools`) when the caller did not set them, with
//!   explicit builder choices winning;
//! - plugin `register()` failures being observable (host ledger + SDK build
//!   error) while every other plugin keeps loading and dispatching;
//! - native tool calls going through the host's ordered plugin path with the
//!   caller's [`ToolContext`] and the tool's result images;
//! - the event-result merge keeping every payload field: `input` images
//!   survive a later text-only transform, and `before_agent_start` messages
//!   accumulate without duplication;
//! - `SdkEvent` tool payload completeness (`args` on start/update, `result`
//!   on end) and the terminal `AgentOutcome` event.
//!
//! The settings-default tests redirect `PIR_AGENT_DIR` at a scratch directory
//! through `tests/common/mod.rs::AgentDirEnvGuard`, which holds the ONE
//! binary-wide agent-dir env lock (a module-local lock cannot exclude the other
//! suites of the merged `g_agent` binary).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{json, Value};

use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{gate_tool_calls_with_host, AgentTool, BoxFuture, ToolContext};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, ImageContent, Model, ModelCost, ModelCostRates,
    StopReason, StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall, Usage,
};
use pir::plugins::{
    AgentPlugin, EventInterest, InputAction, MergedEmit, PluginEvent, PluginEventOutcome,
    PluginFuture, PluginHost, PluginInfo, RegistrationContext,
};
use pir::sdk::{AgentBuilder, SdkError, SdkEvent};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-plugins-w8-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pin `PIR_AGENT_DIR` at `dir` with `PI_OFFLINE=1` for the test's duration.
/// `PIR_AGENT_DIR` is process-global while the merged `g_agent` binary runs
/// tests in parallel threads, so this uses the binary-wide lock in
/// `tests/common/mod.rs::AgentDirEnvGuard` — the same lock every other
/// agent-dir suite takes. Restores both variables (panic included).
async fn pin_agent_dir(dir: &Path) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::at(dir, &[("PI_OFFLINE", "1")]).await
}

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

/// Minimal provider for `with_registry` builds; never streams in these tests.
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
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let message = AssistantMessage {
            api: "fake-api".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            stop_reason: StopReason::Stop,
            usage: Usage::default(),
            ..Default::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// (a) AgentBuilder settings defaults
// ---------------------------------------------------------------------------

/// Write a models.json with one authenticated provider and two models, plus a
/// settings.json naming the saved default and `defaultTools`.
fn write_default_settings(agent_dir: &Path) {
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "w8d": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "w8d-test-key",
                    "models": [
                        { "id": "w8d-default" },
                        { "id": "w8d-other" }
                    ]
                }
            }
        }))
        .expect("serialize models.json"),
    )
    .expect("write models.json");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultProvider": "w8d",
            "defaultModel": "w8d-default",
            "defaultTools": ["beta"]
        }))
        .expect("serialize settings.json"),
    )
    .expect("write settings.json");
}

#[tokio::test(flavor = "current_thread")]
async fn builder_applies_settings_defaults_and_explicit_choices_win() {
    let agent_dir = TempDir::new("agent-defaults");
    let cwd = TempDir::new("cwd-defaults");
    write_default_settings(&agent_dir.0);
    let _env = pin_agent_dir(&agent_dir.0).await;

    // No explicit model/tools: defaultProvider/defaultModel select the model,
    // settings defaultTools selects the declared tool set.
    let agent = AgentBuilder::new()
        .no_session()
        .no_compaction()
        .cwd(cwd.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .tool(
            "alpha",
            "alpha tool",
            json!({ "type": "object" }),
            |_args| async { ToolResult::ok("alpha") },
        )
        .tool(
            "beta",
            "beta tool",
            json!({ "type": "object" }),
            |_args| async { ToolResult::ok("beta") },
        )
        .build()
        .await
        .expect("build with settings defaults");

    assert_eq!(agent.model().provider, "w8d");
    assert_eq!(agent.model().id, "w8d-default");
    let names: Vec<String> = agent.tools().into_iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        vec!["beta".to_owned()],
        "settings defaultTools must be the active set when the caller did not call .tools()"
    );

    // Explicit model + explicit tool list beat both settings defaults.
    let explicit = AgentBuilder::new()
        .no_session()
        .no_compaction()
        .cwd(cwd.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .model_in_provider("w8d", "w8d-other")
        .tools(["alpha"])
        .tool(
            "alpha",
            "alpha tool",
            json!({ "type": "object" }),
            |_args| async { ToolResult::ok("alpha") },
        )
        .tool(
            "beta",
            "beta tool",
            json!({ "type": "object" }),
            |_args| async { ToolResult::ok("beta") },
        )
        .build()
        .await
        .expect("build with explicit choices");

    assert_eq!(explicit.model().provider, "w8d");
    assert_eq!(explicit.model().id, "w8d-other");
    let explicit_names: Vec<String> = explicit.tools().into_iter().map(|tool| tool.name).collect();
    assert_eq!(explicit_names, vec!["alpha".to_owned()]);
}

// ---------------------------------------------------------------------------
// (b) Plugin register() failures are observable, other plugins keep loading
// ---------------------------------------------------------------------------

struct FailingPlugin {
    info: PluginInfo,
}

impl FailingPlugin {
    fn new(name: &str) -> Self {
        Self {
            info: PluginInfo::inline(name),
        }
    }
}

impl AgentPlugin for FailingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, _ctx: &mut RegistrationContext<'_>) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("intentional register failure"))
    }
}

/// A surviving plugin that records every dispatched event.
struct RecordingPlugin {
    info: PluginInfo,
    seen: Arc<AtomicUsize>,
}

impl RecordingPlugin {
    fn new(name: &str, seen: Arc<AtomicUsize>) -> Self {
        Self {
            info: PluginInfo::inline(name),
            seen,
        }
    }
}

impl AgentPlugin for RecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["w8d_probe".to_owned()])
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { PluginEventOutcome::None })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn register_failure_is_reported_and_other_plugins_still_dispatch() {
    let seen = Arc::new(AtomicUsize::new(0));
    let plugins: Vec<Arc<dyn AgentPlugin>> = vec![
        Arc::new(FailingPlugin::new("failing")),
        Arc::new(RecordingPlugin::new("survivor", Arc::clone(&seen))),
    ];
    let host = PluginHost::build(None, plugins);

    let failures = host.registration_failures();
    assert_eq!(
        failures.len(),
        1,
        "one failing registration, ledged: {failures:?}"
    );
    assert_eq!(failures[0].path, "<inline:failing>");
    assert!(
        failures[0].message.contains("intentional register failure"),
        "the ledger carries the plugin's error: {:?}",
        failures[0].message
    );
    assert!(!host.all_plugins_registered());

    let paths: Vec<&str> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["<inline:survivor>"],
        "the failed plugin is isolated; the surviving plugin still loads"
    );

    let mut payload = json!({ "type": "w8d_probe" });
    host.emit("w8d_probe", &mut payload)
        .await
        .expect("dispatch survives a sibling's registration failure");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_build_surfaces_a_plugin_register_failure() {
    let result = AgentBuilder::new()
        .with_registry(fake_registry(), fake_model())
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_plugin(Arc::new(FailingPlugin::new("failing")))
        .build()
        .await;
    let error = match result {
        Ok(_) => panic!("a failed plugin registration must fail the SDK build"),
        Err(error) => error,
    };

    match error {
        SdkError::Other(message) => {
            assert!(
                message.contains("<inline:failing>"),
                "build error names the plugin: {message}"
            );
            assert!(
                message.contains("intentional register failure"),
                "build error carries the register error: {message}"
            );
        }
        other => panic!("expected SdkError::Other, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (c) Native tool calls: ordered host path, caller context, images
// ---------------------------------------------------------------------------

/// Native tool that records the caller's context and returns an image.
struct ContextProbeTool {
    seen_cwd: Arc<Mutex<Option<PathBuf>>>,
    seen_background: Arc<Mutex<Option<bool>>>,
}

impl AgentTool for ContextProbeTool {
    fn name(&self) -> &'static str {
        "native-context-probe"
    }

    fn description(&self) -> &'static str {
        "records the ToolContext and returns an image"
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        *self.seen_cwd.lock().unwrap() = Some(ctx.cwd.clone());
        *self.seen_background.lock().unwrap() = Some(ctx.background);
        let mut result = ToolResult::ok("probe");
        result.images.push(ImageContent {
            data: "aW1n".to_owned(),
            mime_type: "image/png".to_owned(),
        });
        Box::pin(async move { result })
    }
}

struct ToolHostPlugin {
    info: PluginInfo,
    tool: Arc<dyn AgentTool>,
}

impl AgentPlugin for ToolHostPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> Result<(), anyhow::Error> {
        ctx.register_tool(Arc::clone(&self.tool));
        Ok(())
    }
}

struct StaticTextTool {
    name: &'static str,
    text: &'static str,
}

impl AgentTool for StaticTextTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "static text probe"
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = self.text;
        Box::pin(async move { ToolResult::ok(text) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_tool_call_passes_caller_context_and_keeps_images() {
    let seen_cwd = Arc::new(Mutex::new(None));
    let seen_background = Arc::new(Mutex::new(None));
    let host = PluginHost::build(
        None,
        vec![Arc::new(ToolHostPlugin {
            info: PluginInfo::inline("native-tools"),
            tool: Arc::new(ContextProbeTool {
                seen_cwd: Arc::clone(&seen_cwd),
                seen_background: Arc::clone(&seen_background),
            }),
        })],
    );

    let caller_cwd = TempDir::new("caller-cwd");
    let mut ctx = ToolContext::new(caller_cwd.0.clone());
    ctx.background = true;
    let outcome = host
        .call_tool_with_context("native-context-probe", "call-1", &json!({}), &ctx)
        .await
        .expect("native tool runs");

    assert_eq!(
        seen_cwd.lock().unwrap().as_deref(),
        Some(caller_cwd.0.as_path()),
        "the native tool must see the CALLER's ToolContext cwd, not the host default"
    );
    assert_eq!(
        *seen_background.lock().unwrap(),
        Some(true),
        "the background flag must ride the caller's context"
    );
    assert_eq!(
        outcome.images.len(),
        1,
        "structured image output must survive the host call"
    );
    assert_eq!(outcome.images[0].mime_type, "image/png");
    assert!(
        outcome
            .result
            .content
            .iter()
            .any(|item| item.kind == "image" && item.data.as_deref() == Some("aW1n")),
        "the wire result must also carry the image block: {:?}",
        outcome.result.content
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_tool_resolution_uses_plugin_order() {
    let host = PluginHost::build(
        None,
        vec![
            Arc::new(ToolHostPlugin {
                info: PluginInfo::inline("first"),
                tool: Arc::new(StaticTextTool {
                    name: "duplicate-probe",
                    text: "first",
                }),
            }),
            Arc::new(ToolHostPlugin {
                info: PluginInfo::inline("second"),
                tool: Arc::new(StaticTextTool {
                    name: "duplicate-probe",
                    text: "second",
                }),
            }),
        ],
    );

    let outcome = host
        .call_tool_with_context(
            "duplicate-probe",
            "call-dup",
            &json!({}),
            &ToolContext::new("."),
        )
        .await
        .expect("first registration wins");
    assert_eq!(outcome.result.content[0].text, "first");
}

/// Plugin that records `tool_call` dispatches and optionally blocks.
struct GatePlugin {
    info: PluginInfo,
    order: Arc<Mutex<Vec<String>>>,
    marker: &'static str,
    block: bool,
}

impl AgentPlugin for GatePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["tool_call".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.order.lock().unwrap().push(self.marker.to_owned());
        let is_probe = event.tool_name() == Some("gated-probe");
        let outcome = if self.block && is_probe {
            PluginEventOutcome::ToolCall {
                block: true,
                reason: Some(format!("blocked by {}", self.marker)),
                terminate: false,
            }
        } else {
            PluginEventOutcome::None
        };
        Box::pin(async move { outcome })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_tool_call_gate_walks_the_hosts_ordered_plugin_list() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![
            Arc::new(GatePlugin {
                info: PluginInfo::inline("gate-a"),
                order: Arc::clone(&order),
                marker: "a",
                block: false,
            }),
            Arc::new(GatePlugin {
                info: PluginInfo::inline("gate-b"),
                order: Arc::clone(&order),
                marker: "b",
                block: true,
            }),
            Arc::new(GatePlugin {
                info: PluginInfo::inline("gate-c"),
                order: Arc::clone(&order),
                marker: "c",
                block: false,
            }),
        ],
    );

    let gated = gate_tool_calls_with_host(
        None,
        Some(&host),
        vec![AgentToolCall::new(
            0,
            "call-gate",
            "gated-probe",
            json!({ "value": 1 }),
        )],
        None,
    )
    .await;

    assert_eq!(
        order.lock().unwrap().as_slice(),
        ["a", "b"],
        "the native gate must walk the host list in order and stop at the first block"
    );
    assert_eq!(gated.calls.len(), 0);
    assert_eq!(gated.blocked.len(), 1);
    assert_eq!(gated.blocked[0].call_id, "call-gate");
    assert_eq!(gated.blocked[0].tool_name, "gated-probe");
    assert!(
        gated.blocked[0].result.content.contains("blocked by b"),
        "the block reason rides the synthesized result: {:?}",
        gated.blocked[0].result.content
    );
}

// ---------------------------------------------------------------------------
// (d) Event-result merge keeps every payload field
// ---------------------------------------------------------------------------

#[test]
fn input_transform_merge_keeps_images_from_an_earlier_handler() {
    let images = json!([{ "data": "aW1n", "mimeType": "image/png" }]);
    let mut acc = MergedEmit::new();
    acc.merge(
        "input",
        "<inline:a>",
        PluginEventOutcome::Input {
            action: InputAction::Transform,
            text: Some("one".to_owned()),
            images: Some(images.clone()),
        },
    );
    acc.merge(
        "input",
        "<inline:b>",
        PluginEventOutcome::Input {
            action: InputAction::Transform,
            text: Some("two".to_owned()),
            images: None,
        },
    );

    assert_eq!(acc.result["action"], json!("transform"));
    assert_eq!(acc.result["text"], json!("two"));
    assert_eq!(
        acc.result["images"], images,
        "a later text-only transform must not drop the earlier handler's images"
    );
}

#[test]
fn before_agent_start_merge_accumulates_messages_without_duplicates() {
    let first = json!({ "customType": "one", "content": [] });
    let second = json!({ "customType": "two", "content": [] });

    let mut acc = MergedEmit::new();
    acc.merge(
        "before_agent_start",
        "<inline:a>",
        PluginEventOutcome::BeforeAgentStart {
            system_prompt: None,
            message: Some(first.clone()),
            messages: Vec::new(),
        },
    );
    // A handler that reports the complete list (the JS adapter's shape): the
    // list already contains `message`, so the merge must not append it twice.
    acc.merge(
        "before_agent_start",
        "<inline:b>",
        PluginEventOutcome::BeforeAgentStart {
            system_prompt: None,
            message: Some(second.clone()),
            messages: vec![second.clone()],
        },
    );

    assert_eq!(acc.result["message"], second);
    assert_eq!(
        acc.result["messages"],
        json!([first, second]),
        "messages accumulate in handler order, with no duplicate of the single `message`"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn host_input_merge_keeps_images_mutated_in_place() {
    struct MutatingInputPlugin {
        info: PluginInfo,
    }
    impl AgentPlugin for MutatingInputPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["input".to_owned()])
        }
        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            event.payload["images"] = json!([{ "data": "bXV0", "mimeType": "image/jpeg" }]);
            let text = event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move {
                PluginEventOutcome::Input {
                    action: InputAction::Transform,
                    text: Some(format!("{text}!")),
                    images: None,
                }
            })
        }
    }

    let host = PluginHost::build(
        None,
        vec![Arc::new(MutatingInputPlugin {
            info: PluginInfo::inline("input-mutator"),
        })],
    );
    let mut payload = json!({ "type": "input", "text": "hi", "images": [] });
    let outcome = host.emit("input", &mut payload).await.expect("emit input");
    assert_eq!(outcome.result["text"], json!("hi!"));
    assert_eq!(
        outcome.result["images"],
        json!([{ "data": "bXV0", "mimeType": "image/jpeg" }]),
        "in-place payload image mutations must reach the merged result"
    );
}

// ---------------------------------------------------------------------------
// (e) SdkEvent payload completeness (args / result / outcome)
// ---------------------------------------------------------------------------

type Responder = Arc<dyn Fn(usize) -> AssistantMessage + Send + Sync>;

/// Provider that answers each request with the next scripted message.
struct ScriptedProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Scripted fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn scripted_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(ScriptedProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

fn tool_call_block(id: &str, name: &str, arguments: BTreeMap<String, Value>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        usage: Usage::default(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_tool_events_carry_args_result_and_terminal_outcome() {
    let registry = scripted_registry(Arc::new(|index| {
        if index == 0 {
            let mut arguments = BTreeMap::new();
            arguments.insert("text".to_owned(), json!("hello"));
            assistant(
                vec![tool_call_block("call-1", "probe", arguments)],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text_block("done")], StopReason::Stop)
        }
    }));
    let agent = AgentBuilder::new()
        .with_registry(registry, fake_model())
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .tool(
            "probe",
            "probe tool",
            json!({ "type": "object" }),
            |_args| async { ToolResult::ok("probe-result") },
        )
        .build()
        .await
        .expect("scripted build");

    let events: Vec<SdkEvent> = agent.prompt_stream_with_outcome("go").collect().await;

    let start_args = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::ToolExecutionStart {
                call_id,
                tool_name,
                args,
                ..
            } if call_id == "call-1" => {
                assert_eq!(tool_name, "probe");
                Some(args.clone())
            }
            _ => None,
        })
        .expect("tool_execution_start carries the model's arguments");
    assert_eq!(start_args, json!({ "text": "hello" }));

    let end_result = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::ToolExecutionEnd {
                call_id,
                is_error,
                result,
                ..
            } if call_id == "call-1" => {
                assert!(!is_error);
                Some(result.clone())
            }
            _ => None,
        })
        .expect("tool_execution_end carries the wire result");
    assert!(
        end_result["content"]
            .as_array()
            .is_some_and(|content| content
                .iter()
                .any(|item| item["type"] == "text" && item["text"] == "probe-result")),
        "tool result payload: {end_result}"
    );

    let settled = events
        .iter()
        .position(|event| matches!(event, SdkEvent::AgentSettled { .. }))
        .expect("AgentSettled");
    assert!(
        matches!(events[settled.saturating_sub(1)], SdkEvent::AgentOutcome(_)),
        "the terminal outcome must arrive immediately before AgentSettled: {events:?}"
    );
    let outcome = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentOutcome(outcome) => Some(outcome),
            _ => None,
        })
        .expect("AgentOutcome payload");
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.message_text().as_deref(), Some("done"));
    assert_eq!(outcome.tool_result_texts(), vec!["probe-result".to_owned()]);
}
