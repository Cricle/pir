//! Unified plugin framework tests: Rust builtins + QuickJS JS extensions
//! share ONE registry, ONE ordered event dispatch, and ONE merge layer.
//!
//! Covers: mixed-registry order + backends, event merge semantics, tool_call
//! blocking from a Rust plugin, replaceable omission, `builtin:` resolution
//! (disabled + unknown-name error), command `name:2` dedup, notification
//! consumption (sendMessage → steer queue; appendEntry → custom entry),
//! `setActiveTools` persistence, subdirectory discovery, and the real
//! toolCallId pass-through into JS tool execution.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, ThinkingLevel, ToolCall,
};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::{
    AgentPlugin, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost, PluginInfo,
    RegistrationContext, ToolBackend,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);
impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-plugins-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("plugins-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Write a JS extension source and load it into a runtime actor (following
/// the repo hygiene rule: fixtures are generated, never committed).
async fn load_runtime(dir: &TempDir, sources: &[(&str, &str)]) -> ExtensionHandle {
    let runtime = ExtensionHandle::start(test_config_at(&dir.0)).expect("runtime");
    let mut paths = Vec::new();
    for (name, source) in sources {
        let path = dir.0.join(name);
        std::fs::write(&path, source).expect("write fixture");
        paths.push(path);
    }
    let result = runtime.load(&paths).await.expect("load extensions");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    runtime
}

/// A minimal inline Rust plugin used as the "future Rust builtin" stand-in:
/// it registers a native tool, transforms `input` events (appending a
/// marker), and blocks `tool_call`s for a reserved tool name.
struct RustProbePlugin {
    info: PluginInfo,
}

impl RustProbePlugin {
    fn new(name: &str) -> Self {
        Self {
            info: PluginInfo::inline(name),
        }
    }
}

struct ProbeRustTool;

impl AgentTool for ProbeRustTool {
    fn name(&self) -> &'static str {
        "rust-probe-tool"
    }
    fn description(&self) -> &'static str {
        "native probe"
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("from-rust") })
    }
}

impl AgentPlugin for RustProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(ProbeRustTool));
        let handler: pir::plugins::CommandHandler = Arc::new(
            |_args: &str| -> PluginFuture<'static, pir::extensions::CommandOutcome> {
                Box::pin(async {
                    pir::extensions::CommandOutcome {
                        ok: true,
                        error: None,
                    }
                })
            },
        );
        ctx.register_command(
            "rust-cmd",
            Some("native probe command"),
            Some(pir::plugins::CommandBackend::Rust(handler)),
        );
        Ok(())
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["input".into(), "tool_call".into()])
    }

    fn on_event<'a>(&'a self, mut event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            match event.name.as_ref() {
                "input" => {
                    let text = event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    event.payload["text"] = json!(format!("{text}[rust]"));
                    PluginEventOutcome::Input {
                        action: pir::plugins::InputAction::Transform,
                        text: Some(format!("{text}[rust]")),
                        images: None,
                    }
                }
                "tool_call" => {
                    if event.tool_name() == Some("blocked-tool") {
                        PluginEventOutcome::ToolCall {
                            block: true,
                            reason: Some("reserved by the rust probe".into()),
                            terminate: false,
                        }
                    } else {
                        // Mutate the input in place: later handlers must
                        // observe the rewrite (upstream chaining).
                        if let Some(input) = event.tool_call_input_mut() {
                            if let Some(object) = input.as_object_mut() {
                                object.insert("rustSeen".into(), json!(true));
                            }
                        }
                        PluginEventOutcome::None
                    }
                }
                _ => PluginEventOutcome::None,
            }
        })
    }
}

const JS_BASIC: &str = r#"
    export default function (pi) {
        pi.registerTool({
            name: "js-tool",
            description: "js probe",
            parameters: { type: "object" },
            execute: async () => ({ content: [{ type: "text", text: "from-js" }] }),
        });
        pi.registerCommand("js-cmd", { handler: async () => {} });
        pi.on("input", (event) => {
            return { action: "transform", text: event.text + "[js]" };
        });
    }
"#;

// ---------------------------------------------------------------------------
// Unified registry: mixed Rust + JS plugins, order, backends
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn unified_registry_mixes_rust_and_js_plugins_in_path_order() {
    let temp = TempDir::new("mixed");
    let runtime = load_runtime(&temp, &[("ext.ts", JS_BASIC)]).await;
    runtime.set_path_plan(vec![temp.0.join("ext.ts")]);

    let host = PluginHost::build(
        Some(runtime.clone()),
        vec![Arc::new(RustProbePlugin::new("rust-probe"))],
    );

    // Dispatch order follows the path plan: the JS adapter at the position of
    // its first file path, inline factories last.
    let paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "<inline:js-extensions>".to_owned(),
            "<inline:rust-probe>".to_owned(),
        ]
    );

    // Shared registry: a JS tool and a native Rust tool coexist with their
    // correct backends.
    let registry = host.registry();
    let tools = registry.resolved_tools();
    let js_tool = tools
        .iter()
        .find(|(def, _)| def.name == "js-tool")
        .expect("js tool registered");
    assert!(matches!(js_tool.1, ToolBackend::Js { .. }));
    let rust_tool = tools
        .iter()
        .find(|(def, _)| def.name == "rust-probe-tool")
        .expect("rust tool registered");
    assert!(matches!(rust_tool.1, ToolBackend::Rust(_)));

    // Both command backends dispatch.
    let rust = host
        .dispatch_command("rust-cmd", "")
        .await
        .expect("rust cmd");
    assert!(rust.ok, "{rust:?}");
    let js = host.dispatch_command("js-cmd", "").await.expect("js cmd");
    assert!(js.ok, "{js:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn event_merge_walks_the_ordered_list_with_upstream_semantics() {
    let temp = TempDir::new("merge");
    let runtime = load_runtime(&temp, &[("ext.ts", JS_BASIC)]).await;
    runtime.set_path_plan(vec![temp.0.join("ext.ts")]);
    let host = PluginHost::build(
        Some(runtime),
        vec![Arc::new(RustProbePlugin::new("rust-probe"))],
    );

    // `input`: JS handler first (adapter at the JS path position), Rust
    // inline last — transforms chain in dispatch order.
    let mut payload = json!({ "type": "input", "text": "x", "source": "rpc" });
    let outcome = host.emit("input", &mut payload).await.expect("emit input");
    assert_eq!(outcome.result["action"], json!("transform"));
    assert_eq!(payload["text"], json!("x[js][rust]"));

    // `tool_call`: a Rust plugin blocks a reserved tool name on the shared
    // dispatch (the same merge layer the JS gate uses).
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "call-1",
        "toolName": "blocked-tool",
        "input": {},
    });
    let outcome = host
        .emit("tool_call", &mut payload)
        .await
        .expect("emit gate");
    assert_eq!(outcome.result["block"], json!(true));
    assert_eq!(
        outcome.result["reason"],
        json!("reserved by the rust probe")
    );

    // `tool_call` for an allowed tool: the Rust plugin's in-place input
    // mutation flows through the shared payload.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "call-2",
        "toolName": "js-tool",
        "input": {"k": 1},
    });
    let outcome = host
        .emit("tool_call", &mut payload)
        .await
        .expect("emit gate");
    // No handler produced a result → merged outcome is Null (no block).
    assert_ne!(outcome.result["block"], json!(true));
    assert_eq!(payload["input"]["rustSeen"], json!(true));
    assert_eq!(payload["input"]["k"], json!(1));
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_builtin_names_error_with_upstream_text() {
    let temp = TempDir::new("unknown-builtin");
    let runtime = load_runtime(&temp, &[("ext.ts", "export default (pi) => { void pi; };")]).await;
    runtime.set_path_plan(vec![PathBuf::from("builtin:nope"), temp.0.join("ext.ts")]);
    let host = PluginHost::build(Some(runtime), Vec::new());
    let messages: Vec<String> = host
        .diagnostics()
        .into_iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.path, diagnostic.message))
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message == "builtin:nope: Unknown built-in extension: builtin:nope"),
        "{messages:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn replaceable_builtin_is_omitted_by_a_js_registration() {
    let temp = TempDir::new("replaceable");
    let source = r#"
        export default function (pi) {
            pi.registerTool({
                name: "codemode",
                description: "js replacement for the codemode builtin",
                parameters: { type: "object" },
                execute: async () => ({ content: [{ type: "text", text: "from-js" }] }),
            });
            pi.registerCommand("js-cmd", { handler: async () => {} });
        }
    "#;
    let runtime = load_runtime(&temp, &[("replacer.ts", source)]).await;
    runtime.set_path_plan(vec![
        PathBuf::from("builtin:codemode"),
        temp.0.join("replacer.ts"),
    ]);
    let host = PluginHost::build(Some(runtime), Vec::new());

    // The builtin codemode (replaceable, claims tool `codemode`) is omitted;
    // the JS extension survives with the upstream warning.
    let remaining: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert_eq!(remaining, vec!["<inline:js-extensions>".to_owned()]);
    let warnings: Vec<String> = host
        .diagnostics()
        .into_iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("built-in extension `codemode` was not loaded")
        })
        .map(|diagnostic| diagnostic.message)
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", warnings);
    assert!(
        warnings[0].contains("registers tool `codemode`"),
        "{}",
        warnings[0]
    );

    // The surviving JS extension dispatches its command.
    let js = host.dispatch_command("js-cmd", "").await.expect("js cmd");
    assert!(js.ok, "{js:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_commands_get_occurrence_invocation_names() {
    let temp = TempDir::new("dedup");
    let source_a = r#"
        export default function (pi) { pi.registerCommand("dup", { handler: async () => {} }); }
    "#;
    let source_b = r#"
        export default function (pi) { pi.registerCommand("dup", { handler: async () => {} }); }
    "#;
    let runtime = load_runtime(&temp, &[("a.ts", source_a), ("b.ts", source_b)]).await;
    let host = PluginHost::build(Some(runtime), Vec::new());
    let names: Vec<String> = host
        .registry()
        .resolved_commands()
        .into_iter()
        .map(|command| command.invocation_name)
        .collect();
    assert_eq!(names, vec!["dup:1".to_owned(), "dup:2".to_owned()]);
}

// ---------------------------------------------------------------------------
// Session-host consumption: notifications, setActiveTools, toolCallId
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake".into(),
        api: "fake".into(),
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

fn assistant_message(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
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

/// Records every request context so tests can assert steering delivery and
/// tool results.
#[derive(Clone)]
struct ContextRecorder(Arc<Mutex<Vec<Context>>>);

struct RecordingFake {
    calls: std::sync::atomic::AtomicUsize,
    recorder: ContextRecorder,
}

impl Provider for RecordingFake {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.recorder
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        let stream = AssistantMessageEventStream::new();
        if self.calls() == 0 {
            // First turn: call the JS tool so the toolCallId round-trips.
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant_message(
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-parent-1".into(),
                        name: "js-tool".into(),
                        arguments: Default::default(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    StopReason::ToolUse,
                ),
            ));
        } else {
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant_message(vec![], StopReason::Stop),
            ));
        }
        self.bump();
        stream
    }
}
impl RecordingFake {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn bump(&self) {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn session_host_consumes_notifications_and_set_active_tools() {
    let temp = TempDir::new("consume");
    let runtime = load_runtime(
        &temp,
        &[
            ("ext.ts", JS_BASIC),
            (
                "notify.ts",
                r#"
        export default function (pi) {
            pi.on("session_start", () => {
                pi.sendMessage(
                    { customType: "note", content: [{ type: "text", text: "queued" }] },
                    { deliverAs: "steer" },
                );
                pi.appendEntry("audit", { k: 1 });
                pi.setActiveTools(["js-tool"]);
            });
            // `ctx.appendEntry` must report the PERSISTED entry through
            // `entry_appended` (upstream agent-session.ts:3465-3470).
            pi.on("entry_appended", (event) => {
                const entry = event.entry;
                if (entry && entry.type === "custom" && entry.customType === "audit") {
                    globalThis.__pirAuditAppend = {
                        id: entry.id,
                        customType: entry.customType,
                        data: entry.data,
                    };
                }
            });
        }
    "#,
            ),
        ],
    )
    .await;
    runtime.set_path_plan(vec![temp.0.join("ext.ts"), temp.0.join("notify.ts")]);

    let recorder = ContextRecorder(Arc::new(Mutex::new(Vec::new())));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingFake {
        calls: std::sync::atomic::AtomicUsize::new(0),
        recorder: recorder.clone(),
    }));

    // Session storage for the custom-entry assertion.
    let session_dir = TempDir::new("sessions");
    let session =
        pir::session_manager::SessionManager::create("proj", &session_dir.0).expect("session");

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ProbeRustTool));
    // The real core tool set: the JS view must carry their real schemas, and
    // a Rust-backend plugin tool must appear next to them.
    pir::coding_agent::tools::register_core_tools(&mut tools);
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: Some(session),
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(runtime.clone()),
            tool_selection: None,
        },
        vec![Arc::new(RustProbePlugin::new("rust-probe"))],
    );
    agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    // sendMessage with deliverAs "steer" queued a steering message that was
    // delivered inside the same run: the second model request's context
    // contains the queued text.
    {
        let contexts = recorder
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(contexts.len() >= 2, "expected a steering continuation");
        let last = contexts.last().expect("context");
        let serialized = serde_json::to_string(last).expect("context json");
        assert!(
            serialized.contains("queued"),
            "steering message should reach the model context: {serialized}"
        );
    }

    // setActiveTools persisted in runtime state: the JS facade sees the
    // override and the merged registry keeps only the requested tools.
    let active = probe(&runtime, "__pir_host.getActiveToolsJson()").await;
    assert_eq!(active, json!(["js-tool"]));
    // Core tools are visible in getAllTools even with no extension tool for
    // them (core tools ride the host-pushed def list), with their REAL
    // description and parameter schema — the old placeholder
    // (`description: "core tool"`, `parameters: null`) is what the JS side's
    // schema consumers (e.g. tool repair) choked on.
    let all = probe(&runtime, "__pir_host.getAllToolsJson()").await;
    let entries = all.as_array().expect("all tools");
    let names: Vec<String> = entries
        .iter()
        .filter_map(|def| def.get("name").and_then(Value::as_str))
        .map(String::from)
        .collect();
    assert!(names.contains(&"js-tool".to_owned()), "{names:?}");
    // The core `read` row carries exactly the schema the model sees.
    let mut reference = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut reference);
    let expected = pir::coding_agent::tools::tool_definitions(&reference);
    let read_expected = expected
        .iter()
        .find(|def| def.name == "read")
        .expect("core read def");
    let read = entries
        .iter()
        .find(|def| def.get("name").and_then(Value::as_str) == Some("read"))
        .expect("core read row in the JS view");
    assert_eq!(
        read.get("parameters"),
        Some(&read_expected.parameters),
        "the JS view carries the tool's real schema"
    );
    assert_ne!(
        read.get("description").and_then(Value::as_str),
        Some("core tool"),
        "the placeholder description is gone"
    );
    // A Rust-backend plugin tool is present with its real description (it used
    // to be absent from the JS view entirely).
    let rust = entries
        .iter()
        .find(|def| def.get("name").and_then(Value::as_str) == Some("rust-probe-tool"))
        .expect("Rust plugin tool row in the JS view");
    assert_eq!(
        rust.get("description").and_then(Value::as_str),
        Some("native probe")
    );

    // `pi.appendEntry` persisted the custom entry AND emitted
    // `entry_appended` with the stored entry (id assigned by the store).
    let appended = probe(
        &runtime,
        "JSON.stringify(globalThis.__pirAuditAppend ?? null)",
    )
    .await;
    assert_eq!(
        appended["customType"],
        json!("audit"),
        "entry_appended carries the custom entry: {appended}"
    );
    assert_eq!(appended["data"], json!({ "k": 1 }), "{appended}");
    assert!(
        appended["id"].as_str().is_some_and(|id| !id.is_empty()),
        "entry_appended carries the persisted id: {appended}"
    );

    // The custom entry landed in session storage (`appendEntry`).
    // (Re-open the session file: the facade consumed it during the run.)
    let session_files = std::fs::read_dir(&session_dir.0)
        .expect("session dir")
        .filter_map(std::result::Result::ok)
        .count();
    assert!(session_files >= 1, "session file exists");
    let contents = std::fs::read_dir(&session_dir.0)
        .expect("session dir")
        .filter_map(std::result::Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).expect("session file"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        contents.contains("audit"),
        "custom entry persisted: {contents}"
    );
    assert!(
        contents.contains("\"k\":1") || contents.contains("\"k\": 1"),
        "{contents}"
    );
}

/// The real tool call id reaches JS tool execution (upstream
/// `createToolContext(toolCallId)`); nested ids keep the `<parent>/<n>`
/// shape.
#[tokio::test(flavor = "current_thread")]
async fn js_tool_receives_the_real_tool_call_id() {
    let temp = TempDir::new("call-id");
    let source = r#"
        export default function (pi) {
            pi.registerTool({
                name: "js-tool",
                description: "returns its tool call id",
                parameters: { type: "object" },
                execute: async (toolCallId) => ({
                    content: [{ type: "text", text: toolCallId }],
                }),
            });
        }
    "#;
    let runtime = load_runtime(&temp, &[("id.ts", source)]).await;

    let recorder = ContextRecorder(Arc::new(Mutex::new(Vec::new())));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingFake {
        calls: std::sync::atomic::AtomicUsize::new(0),
        recorder: recorder.clone(),
    }));

    let tools = ToolRegistry::new();
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let contexts = recorder
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let last = contexts.last().expect("second request context");
    let serialized = serde_json::to_string(last).expect("context json");
    assert!(
        serialized.contains("call-parent-1"),
        "the JS tool echoed its real tool call id into the result: {serialized}"
    );
}

/// Tool defs reaching the JS view: core builtins carry their REAL description
/// and parameter schema (upstream sends real schemas for every builtin), and
/// Rust-backend plugin tools (`rust-probe-tool` here; `subagent`, `codemode`,
/// `mcp__*` ... in production) are present instead of missing entirely. The
/// JS-side probe mirrors pi-tool-repair's live-schema filter — a tool is
/// repairable only when `tool.parameters` is an object schema — which used to
/// skip every core tool because the host sent `parameters: null`.
#[tokio::test(flavor = "current_thread")]
async fn core_and_rust_plugin_tool_defs_reach_the_js_view() {
    let temp = TempDir::new("js-tool-view");
    let runtime = load_runtime(
        &temp,
        &[(
            "schemas.ts",
            r#"
        export default function (pi) {
            pi.registerTool({
                name: "js-tool",
                description: "js probe",
                parameters: { type: "object", properties: { n: { type: "integer" } } },
                execute: async () => ({ content: [{ type: "text", text: "from-js" }] }),
            });
            pi.registerCommand("tool-schemas", {
                handler: () => {
                    // The pi-tool-repair filter shape: only a tool with an
                    // object parameter schema can be repaired, and the schema
                    // itself travels with the row.
                    globalThis.__repairable = pi
                        .getAllTools()
                        .filter((tool) => tool.parameters && tool.parameters.type === "object")
                        .map((tool) => ({ name: tool.name, parameters: tool.parameters }));
                    globalThis.__activeNames = pi.getActiveTools();
                    globalThis.__allDescriptions = Object.fromEntries(
                        pi.getAllTools().map((tool) => [tool.name, tool.description]),
                    );
                },
            });
        }
    "#,
        )],
    )
    .await;
    runtime.set_path_plan(vec![temp.0.join("schemas.ts")]);

    let recorder = ContextRecorder(Arc::new(Mutex::new(Vec::new())));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingFake {
        calls: std::sync::atomic::AtomicUsize::new(0),
        recorder: recorder.clone(),
    }));

    let mut tools = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut tools);
    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: Some(runtime.clone()),
            tool_selection: None,
        },
        vec![Arc::new(RustProbePlugin::new("rust-probe"))],
    );
    agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    runtime
        .dispatch_command("tool-schemas", "")
        .await
        .expect("command");

    // The JS-side filter sees real schemas: the core builtins (invisible
    // before this fix) alongside the JS tool.
    let repairable = probe(&runtime, "JSON.stringify(globalThis.__repairable)").await;
    let schemas = repairable.as_array().expect("schema rows");
    let read = schemas
        .iter()
        .find(|row| row["name"] == json!("read"))
        .expect("core read schema reached the JS filter");
    assert_eq!(
        read["parameters"]["properties"]["path"]["type"],
        json!("string"),
        "the core tool's real schema, not a placeholder"
    );
    // The JS-visible row carries EXACTLY the parameters the core registry
    // advertises to the model.
    let mut reference = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut reference);
    let reference_defs = pir::coding_agent::tools::tool_definitions(&reference);
    let read_expected = reference_defs
        .iter()
        .find(|def| def.name == "read")
        .expect("core read def");
    assert_eq!(
        read.get("parameters"),
        Some(&read_expected.parameters),
        "the JS view carries the tool's real schema"
    );
    assert!(schemas.iter().any(|row| row["name"] == json!("js-tool")));

    // The names-only active view now also lists the host-pushed defs: core
    // builtins and the Rust plugin row (the JS tool already rode `tool_defs`).
    let active = probe(&runtime, "JSON.stringify(globalThis.__activeNames)").await;
    let names: Vec<String> = active
        .as_array()
        .expect("active names")
        .iter()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    for expected in ["read", "bash", "rust-probe-tool", "js-tool"] {
        assert!(
            names.contains(&expected.to_owned()),
            "{expected} missing from the JS active view: {names:?}"
        );
    }

    // Real descriptions ride the same rows: the core row is not the old
    // "core tool" placeholder and the Rust plugin row carries its own.
    let descriptions = probe(&runtime, "JSON.stringify(globalThis.__allDescriptions)").await;
    assert_ne!(
        descriptions.get("read").and_then(Value::as_str),
        Some("core tool"),
        "the placeholder description is gone"
    );
    assert_eq!(
        descriptions.get("rust-probe-tool").and_then(Value::as_str),
        Some("native probe")
    );
}

/// Evaluate a JS expression whose result is JSON text (`eval_json` converts
/// the JS value to a `String` first): host fns returning JSON strings are
/// probed directly, plain JS values (arrays/objects) must be wrapped in
/// `JSON.stringify`.
async fn probe(runtime: &ExtensionHandle, code: &str) -> Value {
    runtime.eval_json(code).await.expect("probe eval")
}

// ---------------------------------------------------------------------------
// Subdirectory discovery
// ---------------------------------------------------------------------------

#[test]
fn subdirectory_discovery_scans_foo_index_and_manifests() {
    use pir::extensions::discover_extensions;

    let temp = TempDir::new("subdirs");
    let root = temp.0.join("extensions");
    // Subdirectory with index.ts.
    std::fs::create_dir_all(root.join("foo")).expect("foo");
    std::fs::write(
        root.join("foo").join("index.ts"),
        "export default () => {};",
    )
    .expect("foo idx");
    // Subdirectory with a package.json manifest.
    std::fs::create_dir_all(root.join("bar")).expect("bar");
    std::fs::write(
        root.join("bar").join("package.json"),
        r#"{"pi":{"extensions":["./entry.js"]}}"#,
    )
    .expect("bar manifest");
    std::fs::write(
        root.join("bar").join("entry.js"),
        "export default () => {};",
    )
    .expect("bar entry");
    // Subdirectory without entries contributes nothing.
    std::fs::create_dir_all(root.join("empty")).expect("empty");

    let result = discover_extensions(&[root]);
    let mut names: Vec<String> = result
        .extensions
        .iter()
        .map(|extension| {
            extension
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["entry.js".to_owned(), "index.ts".to_owned()]);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
}

#[test]
fn default_paths_are_project_before_global() {
    use pir::extensions::loader::default_extension_paths;

    let agent_dir = Path::new("C:/u/.pi/agent");
    let project_dir = Path::new("D:/proj");
    let untrusted = default_extension_paths(agent_dir, project_dir, false);
    assert_eq!(untrusted, vec![agent_dir.join("extensions")]);
    let trusted = default_extension_paths(agent_dir, project_dir, true);
    assert_eq!(
        trusted,
        vec![
            project_dir.join(".pi").join("extensions"),
            agent_dir.join("extensions"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Loader semantics: builtin: filtering and --no-extensions ordering
// ---------------------------------------------------------------------------

#[test]
fn builtin_paths_partition_filter_and_order() {
    use pir::plugins::{filter_disabled_builtins, order_extension_paths, partition_paths};

    let paths = vec![
        PathBuf::from("cli.ts"),
        PathBuf::from("builtin:subagent"),
        PathBuf::from("builtin:mcp"),
    ];
    let (builtins, files) = partition_paths(&paths);
    assert_eq!(builtins, vec!["subagent".to_owned(), "mcp".to_owned()]);
    assert_eq!(files, vec![PathBuf::from("cli.ts")]);

    let mut disabled = std::collections::BTreeSet::new();
    disabled.insert("mcp".to_owned());
    let filtered = filter_disabled_builtins(&paths, &disabled);
    assert_eq!(
        filtered,
        vec![PathBuf::from("cli.ts"), PathBuf::from("builtin:subagent"),]
    );

    // Factory builtins are default-enabled and appended AFTER the discovered
    // file paths (upstream `package-manager.ts:986-1001` adds the builtin loop
    // after every file resource, so a file extension wins first-wins
    // conflicts). `builtin:subagent` in the list above is the real builtin;
    // the factory order is subagent, mcp, codemode, python, context-mode,
    // tool-search, tool-repair, todo — python and context-mode are pir-only,
    // `todo` is the npm `@juicesharp/rpiv-todo` port appended last, and
    // `phone` stays default-disabled (not inserted), see
    // `src/plugins/builtin.rs` and `docs/todo-builtin.md`.

    let ordered = order_extension_paths(
        &[PathBuf::from("cli.ts")],
        &[PathBuf::from("defaults")],
        false,
    );
    assert_eq!(
        ordered,
        vec![
            PathBuf::from("cli.ts"),
            PathBuf::from("defaults"),
            PathBuf::from("builtin:subagent"),
            PathBuf::from("builtin:mcp"),
            PathBuf::from("builtin:codemode"),
            PathBuf::from("builtin:python"),
            PathBuf::from("builtin:context-mode"),
            PathBuf::from("builtin:tool-search"),
            PathBuf::from("builtin:tool-repair"),
            PathBuf::from("builtin:todo"),
        ]
    );
    let cli_only = order_extension_paths(
        &[PathBuf::from("cli.ts")],
        &[PathBuf::from("defaults")],
        true,
    );
    assert_eq!(cli_only, vec![PathBuf::from("cli.ts")]);
}

/// Native-first `tool-repair` precedence through the real CLI plan shape
/// (`order_extension_paths` output → the disabled-builtin filter → the
/// runtime load list split): with the builtin on the plan the JS
/// `pi-tool-repair` package's entry is dropped from the load list; when
/// `disabledBuiltinExtensions` removes the builtin, the JS copy is allowed
/// back (the fallback — never both faces off).
#[test]
fn tool_repair_precedence_through_the_loader_pipeline() {
    use pir::plugins::{filter_disabled_builtins, order_extension_paths, partition_paths};

    let js_entry = PathBuf::from("C:/u/.pi/agent/npm/node_modules/pi-tool-repair/tool-repair.ts");
    let other = PathBuf::from("C:/u/.pi/agent/extensions/other.ts");
    let ordered = order_extension_paths(&[], &[js_entry.clone(), other.clone()], false);
    assert!(ordered.contains(&PathBuf::from("builtin:tool-repair")));

    // Normal case: the default builtins survive, so the JS copy is
    // suppressed — the effective plan no longer carries its entry and the
    // load list split cannot hand it to the runtime.
    let effective = filter_disabled_builtins(&ordered, &std::collections::BTreeSet::new());
    assert!(!effective.contains(&js_entry));
    let (builtins, files) = partition_paths(&effective);
    assert!(builtins.iter().any(|name| name == "tool-repair"));
    assert!(!files.contains(&js_entry));
    assert!(files.contains(&other), "unrelated file paths survive");

    // Disabled builtin: the JS copy is allowed back on the plan and stays in
    // the load list.
    let disabled = std::collections::BTreeSet::from(["tool-repair".to_owned()]);
    let effective = filter_disabled_builtins(&ordered, &disabled);
    assert!(effective.contains(&js_entry));
    let (builtins, files) = partition_paths(&effective);
    assert!(!builtins.iter().any(|name| name == "tool-repair"));
    assert!(files.contains(&js_entry));
}

// ---------------------------------------------------------------------------
// Panic isolation on the ordered event dispatch
// ---------------------------------------------------------------------------

/// A native plugin whose handler panics: the dispatch boundary must turn it
/// into an isolated error, not unwind the caller's task.
struct PanickingPlugin {
    info: PluginInfo,
}

impl AgentPlugin for PanickingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::All
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async { panic!("native plugin boom") })
    }
}

/// The plugin after the panicking one still runs, in order.
struct RecordingPlugin {
    info: PluginInfo,
    ran: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentPlugin for RecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::All
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { PluginEventOutcome::None })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_plugin_panic_is_isolated_and_later_plugins_still_run() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let plugins: Vec<Arc<dyn AgentPlugin>> = vec![
        Arc::new(PanickingPlugin {
            info: PluginInfo::inline("panics"),
        }),
        Arc::new(RecordingPlugin {
            info: PluginInfo::inline("records"),
            ran: Arc::clone(&ran),
        }),
    ];
    let host = PluginHost::build(None, plugins);
    let mut payload = json!({ "type": "session_start", "reason": "startup" });
    let outcome = host
        .emit("session_start", &mut payload)
        .await
        .expect("a plugin panic must not unwind the dispatcher");
    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.error.contains("panicked")
                && error.error.contains("native plugin boom")),
        "the panic must be reported as an isolated handler error: {:?}",
        outcome.errors
    );
    assert!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        "plugins after a panicking one must still receive the event in order"
    );
}
