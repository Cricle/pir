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
use pir::plugins::{PluginHost, PluginInfo};

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

    // The session abort signal must reach the tool's context.
    session.abort();
    let outcome = tools
        .execute_tool(
            "caller-context",
            "context-probe",
            json!({}),
            ExecuteToolOptions { background: true },
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
    assert!(seen.aborted, "the session abort signal reaches the tool");
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
        seen.background,
        "options.background must reach the tool's context"
    );
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
