//! Builtin audit P1-2: codemode's upstream exposure/defaultActive parity and
//! its one-load settings lifecycle.
//!
//! Upstream registers `codemode` as `exposure: "model-only"` with
//! `defaultActive: false` (`extensions/codemode/index.js:27`,
//! `tool.js:253`): a default session does not declare it, an explicit
//! activation (`--tools codemode`, `defaultTools`, `setActiveTools`, MCP
//! discovery) does, and an inactive codemode cannot be called (upstream
//! executes model calls against the declared set only).
//!
//! The settings tests count the caching-path loads through
//! [`pir::plugins::codemode::settings_load_count`]: constructing the builtin
//! plugin reads settings once, script executions read none, and `/reload`
//! reads once more (also picking up a changed `codemode.mode`). The agent dir
//! is a temp directory (`PIR_AGENT_DIR`), never `~/.pi`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pir::agent::agent_loop::{AgentEvent, LoopResult};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall,
};
use pir::coding_agent::ToolSelection;
use pir::config::settings::CodemodeMode;
use pir::plugins::codemode::{
    new_codemode_plugin, settings_load_count, CodemodeConfig, CodemodePlugin, CodemodeTool,
};
use pir::plugins::PluginHost;

/// Serializes this binary's tests: they share the process-wide settings-load
/// counter and the `PIR_AGENT_DIR` environment variable.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-codemode-exposure-{label}-{}-{}",
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

/// Restores `PIR_AGENT_DIR` when the test ends (panic included).
struct AgentDirGuard(Option<std::ffi::OsString>);

impl AgentDirGuard {
    fn set(dir: &std::path::Path) -> Self {
        let previous = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", dir);
        Self(previous)
    }
}

impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
    }
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

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
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

fn codemode_call(id: &str, code: &str) -> ContentBlock {
    let mut arguments = BTreeMap::new();
    arguments.insert("code".to_owned(), serde_json::Value::String(code.into()));
    tool_call(id, "codemode", arguments)
}

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that records every request's declared tool names and
/// answers from the scripted responder.
struct FakeProvider {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<Vec<String>>>>,
    respond: Responder,
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
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let declared = context
            .tools
            .iter()
            .flatten()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        self.requests.lock().expect("requests").push(declared);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

struct FakeHarness {
    registry: Arc<ProviderRegistry>,
    requests: Arc<Mutex<Vec<Vec<String>>>>,
}

fn fake_registry(respond: Responder) -> FakeHarness {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        requests: Arc::clone(&requests),
        respond,
    }));
    FakeHarness {
        registry: Arc::new(registry),
        requests,
    }
}

/// A trivial direct tool the mode-`"only"` test hides from declarations.
struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echoes its text argument"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        }))
    }
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let value = args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move { ToolResult::ok(value) })
    }
}

fn selection(enabled: &[&str]) -> ToolSelection {
    ToolSelection {
        enabled: enabled.iter().map(|name| (*name).to_owned()).collect(),
        modifiers: Vec::new(),
        uses_default_tools: false,
    }
}

fn session_config(
    registry: Arc<ProviderRegistry>,
    tools: ToolRegistry,
    cwd: &TempDir,
    selection: Option<ToolSelection>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: Some("test system prompt".into()),
        cwd: Some(cwd.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    }
}

async fn run_prompt(agent: &Arc<AgentSession>, text: &str) -> LoopResult {
    let log: Arc<Mutex<Vec<Arc<LoopResult>>>> = Arc::default();
    let sink = Arc::clone(&log);
    agent
        .prompt(
            text,
            Arc::new(move |event: AgentEvent| {
                if let AgentEvent::AgentEnd { result } = event {
                    sink.lock().expect("log").push(result);
                }
            }),
        )
        .await
        .expect("prompt");
    let result = log.lock().expect("log").pop().expect("agent end");
    Arc::unwrap_or_clone(result)
}

fn tool_result_text(result: &LoopResult, name: &str) -> (bool, String) {
    let message = result
        .tool_results
        .iter()
        .find(|message| message.tool_name == name)
        .unwrap_or_else(|| panic!("{name} tool result message"));
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (message.is_error, text)
}

fn request_names(requests: &Arc<Mutex<Vec<Vec<String>>>>) -> Vec<Vec<String>> {
    requests.lock().expect("requests").clone()
}

// ---------------------------------------------------------------------------
// 1. Exposure values
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn codemode_reports_upstream_model_only_and_inactive_default() {
    let _lock = TEST_LOCK.lock().await;
    let tool = CodemodeTool::default();
    assert_eq!(
        tool.exposure(),
        ToolExposure::ModelOnly,
        "upstream `tool.js:253` exposure: model-only"
    );
    assert!(
        !tool.default_active(),
        "upstream `index.js:27` defaultActive: false"
    );

    // The registered definition carries the same values to the registry.
    let host = PluginHost::build(None, vec![new_codemode_plugin()]);
    let def = host
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "codemode")
        .expect("codemode registered");
    assert_eq!(def.exposure, "model-only");
    assert!(!def.default_active);
}

// ---------------------------------------------------------------------------
// 2. Default session: not declared, not callable
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn default_session_neither_declares_nor_executes_codemode() {
    let _lock = TEST_LOCK.lock().await;
    let temp = TempDir::new("default");
    let harness = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![codemode_call("call-1", "return 1;")],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(
            Arc::clone(&harness.registry),
            ToolRegistry::new(),
            &temp,
            None,
        ),
        vec![new_codemode_plugin()],
    ));

    let result = run_prompt(&agent, "run codemode").await;
    let requests = request_names(&harness.requests);
    assert!(
        !requests.is_empty(),
        "the session must have issued a model request"
    );
    assert!(
        requests[0].iter().all(|name| name != "codemode"),
        "a default session must not declare codemode: {:?}",
        requests[0]
    );
    let (is_error, content) = tool_result_text(&result, "codemode");
    assert!(is_error, "an inactive codemode call must fail: {content}");
    assert!(
        content.contains("Tool codemode not found"),
        "the inactive tool reports the scheduler's not-found text, naming the tool: {content}"
    );
}

// ---------------------------------------------------------------------------
// 3. Explicit selection: declared and executable
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn selected_codemode_is_declared_and_runs() {
    let _lock = TEST_LOCK.lock().await;
    let temp = TempDir::new("selected");
    let harness = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![codemode_call("call-1", "return 21 * 2;")],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(
            Arc::clone(&harness.registry),
            ToolRegistry::new(),
            &temp,
            Some(selection(&["codemode"])),
        ),
        vec![new_codemode_plugin()],
    ));

    let result = run_prompt(&agent, "run codemode").await;
    let requests = request_names(&harness.requests);
    assert_eq!(
        requests[0],
        vec!["codemode".to_owned()],
        "an explicitly selected codemode is declared (the loadout declares every active non-hidden tool)"
    );
    let (is_error, content) = tool_result_text(&result, "codemode");
    assert!(!is_error, "the selected codemode must run: {content}");
    assert!(content.contains("Script completed"), "content: {content}");
    assert!(content.contains("42"), "content: {content}");
}

// ---------------------------------------------------------------------------
// 4. mode "only" still rewrites the loadout while codemode is active
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mode_only_hides_direct_declarations_for_an_active_codemode() {
    let _lock = TEST_LOCK.lock().await;
    let temp = TempDir::new("mode-only");
    let harness = fake_registry(Arc::new(|index, _context| {
        if index == 0 {
            assistant(
                vec![codemode_call(
                    "call-1",
                    "return await tools.echo({ text: 'from-script' });",
                )],
                StopReason::ToolUse,
            )
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        }
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let plugin = CodemodePlugin::with_config(CodemodeConfig {
        mode: CodemodeMode::Only,
        ..CodemodeConfig::default()
    });
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(
            Arc::clone(&harness.registry),
            tools,
            &temp,
            Some(selection(&["codemode", "echo"])),
        ),
        vec![Arc::new(plugin)],
    ));

    let result = run_prompt(&agent, "run codemode").await;
    let requests = request_names(&harness.requests);
    assert!(
        requests[0].iter().any(|name| name == "codemode"),
        "codemode stays declared in mode `only`: {:?}",
        requests[0]
    );
    assert!(
        requests[0].iter().all(|name| name != "echo"),
        "mode `only` hides the direct tool's declaration: {:?}",
        requests[0]
    );
    let (is_error, content) = tool_result_text(&result, "codemode");
    assert!(!is_error, "content: {content}");
    assert!(
        content.contains("from-script"),
        "the hidden tool stays callable from scripts: {content}"
    );
}

// ---------------------------------------------------------------------------
// 5. Settings load once per construction, zero per call, once more on /reload
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn settings_load_once_per_construction_and_once_more_on_reload() {
    let _lock = TEST_LOCK.lock().await;
    let agent_dir = TempDir::new("agent-dir");
    let temp = TempDir::new("cwd");
    let _env = AgentDirGuard::set(&agent_dir.0);
    std::fs::write(
        agent_dir.0.join("settings.json"),
        json!({ "codemode": { "mode": "on" } }).to_string(),
    )
    .expect("write initial settings");

    let before = settings_load_count();
    let plugin = new_codemode_plugin();
    assert_eq!(
        settings_load_count(),
        before + 1,
        "plugin construction resolves settings exactly once"
    );

    // Two codemode calls in one prompt; then a third call after `/reload`.
    let harness = fake_registry(Arc::new(|index, _context| match index {
        0 => assistant(
            vec![codemode_call("call-1", "return 'one';")],
            StopReason::ToolUse,
        ),
        1 => assistant(
            vec![codemode_call("call-2", "return 'two';")],
            StopReason::ToolUse,
        ),
        2 => assistant(vec![text("done")], StopReason::Stop),
        3 => assistant(
            vec![codemode_call(
                "call-3",
                "return await tools.echo({ text: 'after-reload' });",
            )],
            StopReason::ToolUse,
        ),
        _ => assistant(vec![text("done")], StopReason::Stop),
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let requests = Arc::clone(&harness.requests);
    let agent = Arc::new(AgentSession::new_with_plugins(
        session_config(
            Arc::clone(&harness.registry),
            tools,
            &temp,
            Some(selection(&["codemode", "echo"])),
        ),
        vec![plugin],
    ));

    let first = run_prompt(&agent, "two scripts").await;
    assert!(tool_result_text(&first, "codemode").1.contains("one"));
    assert_eq!(
        settings_load_count(),
        before + 1,
        "script executions must not read settings"
    );
    let pre_reload_requests = requests.lock().expect("requests").len();

    // `/reload` re-reads once and the changed mode takes effect: the next
    // loadout hides the direct echo declaration (mode `only`).
    std::fs::write(
        agent_dir.0.join("settings.json"),
        json!({ "codemode": { "mode": "only" } }).to_string(),
    )
    .expect("write changed settings");
    agent
        .plugin_host()
        .expect("the inline plugin built a host")
        .reload()
        .await
        .expect("reload");
    assert_eq!(
        settings_load_count(),
        before + 2,
        "`/reload` resolves settings exactly once more"
    );

    let second = run_prompt(&agent, "after reload").await;
    assert_eq!(
        settings_load_count(),
        before + 2,
        "the post-reload script execution must not read settings"
    );
    let after_reload = request_names(&requests);
    let reload_request = after_reload
        .get(pre_reload_requests)
        .expect("a post-reload model request");
    assert!(
        reload_request.iter().any(|name| name == "codemode"),
        "codemode is still declared after the reload: {reload_request:?}"
    );
    assert!(
        reload_request.iter().all(|name| name != "echo"),
        "the reloaded `codemode.mode: \"only\"` hides echo's declaration: {reload_request:?}"
    );
    let (is_error, content) = tool_result_text(&second, "codemode");
    assert!(!is_error, "content: {content}");
    assert!(
        content.contains("after-reload"),
        "the hidden tool stays callable from scripts after the reload: {content}"
    );
}
