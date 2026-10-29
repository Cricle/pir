//! MCP discovery auto-activation (upstream `ensureDiscoveryActive`,
//! `extensions/mcp/index.ts:498-537`).
//!
//! A server whose configured exposure is `codemode` (the default) makes the
//! session activate the `codemode` tool; a `deferred` exposure activates
//! `tool_search`. Without that activation the MCP tools are callable only from
//! tools the model never sees, so a default session with `mcp.json` could not
//! use them at all.
//!
//! The tests pin the state machine end to end on a real [`AgentSession`]:
//!
//! - a codemode-exposure server owns `codemode` and the next loadout declares
//!   it; a deferred server owns `tool_search`;
//! - `autoEnableCodemode: false` suppresses only the codemode half;
//! - removing/disabling the last server removes only the names this plugin
//!   added (an explicit `--tools codemode` selection survives);
//! - repeated rebuilds neither duplicate the names nor flip ownership
//!   (checked by the removal still removing them);
//! - `/mcp` lists each server with its tool count, and its `enable`/`disable`/
//!   `remove` actions update the active set immediately and across rebuilds.
//!
//! The servers point at a nonexistent command: activation depends only on the
//! configured exposure, and the spawn failure is immediate, so no MCP process
//! is needed (the session-start discovery is allowed to fail).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
};
use pir::coding_agent::ToolSelection;
use pir::config::mcp::McpServerConfig;
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{instantiate_builtin, AgentPlugin, PluginHost};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `PIR_AGENT_DIR` is process-wide; the MCP plugin reads it in `register()`.
/// Every test in this binary serializes on this lock.
async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    use tokio::sync::OnceCell;
    static ENV_LOCK: OnceCell<tokio::sync::Mutex<()>> = OnceCell::const_new();
    ENV_LOCK
        .get_or_init(|| async { tokio::sync::Mutex::new(()) })
        .await
        .lock()
        .await
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-auto-{label}-{}-{}",
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

/// Records the tool names each request declares (`context.tools` IS the
/// declared loadout) and answers with a final text message.
struct CapturingProvider {
    calls: AtomicUsize,
    captured: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Provider for CapturingProvider {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(
                context
                    .tools
                    .as_ref()
                    .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                    .unwrap_or_default(),
            );
        let message = AssistantMessage {
            api: "fake".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: "done".into(),
                text_signature: None,
            })],
            stop_reason: StopReason::Stop,
            timestamp: 1,
            usage: Usage::default(),
            ..AssistantMessage::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// Write the global `mcp.json` (`PIR_AGENT_DIR` points at `dir`).
fn write_mcp_json(dir: &Path, auto_enable_codemode: bool, servers: Value) {
    let document = json!({
        "autoEnableCodemode": auto_enable_codemode,
        "mcpServers": servers,
    });
    std::fs::write(dir.join("mcp.json"), document.to_string()).expect("write mcp.json");
}

/// A server entry that never connects: the missing command fails the spawn
/// immediately, which is irrelevant for activation (only the exposure counts).
fn server(exposure: &str) -> Value {
    json!({
        "command": "pir-test-nonexistent-mcp-server-13579",
        "exposure": exposure,
        "timeout": 1,
    })
}

/// The active-tool sets a session wrote, in write order.
type CapturedToolSets = Arc<Mutex<Vec<Vec<String>>>>;

fn build_session(
    cwd: &Path,
    plugins: Vec<Arc<dyn AgentPlugin>>,
    tool_selection: Option<ToolSelection>,
) -> (Arc<AgentSession>, CapturedToolSets) {
    let captured: CapturedToolSets = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CapturingProvider {
        calls: AtomicUsize::new(0),
        captured: Arc::clone(&captured),
    }));
    let session = AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 2,
            system_prompt: Some("mcp auto-enable".into()),
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection,
        },
        plugins,
    );
    (session, captured)
}

fn builtin(name: &str) -> Arc<dyn AgentPlugin> {
    instantiate_builtin(name).unwrap_or_else(|| panic!("builtin {name}"))
}

/// Run one prompt and report the tools its request declared.
async fn declared_tools(
    session: &AgentSession,
    captured: &Arc<Mutex<Vec<Vec<String>>>>,
) -> Vec<String> {
    session
        .prompt("go", Arc::new(|_| {}))
        .await
        .expect("prompt");
    captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .unwrap_or_default()
}

/// The rebuild/flush point: `emit` ends with
/// [`PluginHost::refresh_registry_if_changed`], which calls the MCP plugin's
/// `flush_server_changes` (where the activation recomputes).
async fn emit_settled(host: &PluginHost) {
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
}

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|name| (*name).to_string()).collect()
}

/// The `/mcp` enable/disable helper mirrors the plugin's config patch.
fn config_with_enabled(config: &McpServerConfig, enabled: bool) -> McpServerConfig {
    let mut next = config.clone();
    match &mut next {
        McpServerConfig::Stdio(stdio) => stdio.base.enabled = Some(enabled),
        McpServerConfig::Http(http) => http.base.enabled = Some(enabled),
    }
    next
}

// ---------------------------------------------------------------------------
// Exposure-driven activation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn codemode_exposure_auto_activates_codemode_in_the_next_loadout() {
    let _guard = env_guard().await;
    let dir = TempDir::new("codemode");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    write_mcp_json(&dir.0, true, json!({ "docs": server("codemode") }));

    let mcp = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let (session, captured) = build_session(
        &dir.0,
        vec![builtin("codemode"), builtin("tool-search"), mcp.clone()],
        None,
    );

    assert!(
        mcp.manager().registry().get("docs").is_some(),
        "the config pass registered the server"
    );
    assert_eq!(
        mcp.auto_activated_tools(),
        names(&["codemode"]),
        "the plugin owns the codemode activation"
    );
    let active = mcp.active_tools_snapshot().expect("a context is bound");
    assert!(
        active.iter().any(|name| name == "codemode"),
        "codemode joins the session's active set: {active:?}"
    );

    let declared = declared_tools(&session, &captured).await;
    assert!(
        declared.iter().any(|name| name == "codemode"),
        "the next loadout declares codemode: {declared:?}"
    );
    assert!(
        !declared.iter().any(|name| name == "tool_search"),
        "a codemode-exposure server does not activate tool_search: {declared:?}"
    );
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_exposure_auto_activates_tool_search() {
    let _guard = env_guard().await;
    let dir = TempDir::new("deferred");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    write_mcp_json(&dir.0, true, json!({ "search": server("deferred") }));

    let mcp = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let (session, captured) = build_session(
        &dir.0,
        vec![builtin("codemode"), builtin("tool-search"), mcp.clone()],
        None,
    );

    assert_eq!(mcp.auto_activated_tools(), names(&["tool_search"]));
    let active = mcp.active_tools_snapshot().expect("a context is bound");
    assert!(active.iter().any(|name| name == "tool_search"));
    assert!(
        !active.iter().any(|name| name == "codemode"),
        "a deferred-only server does not activate codemode: {active:?}"
    );

    let declared = declared_tools(&session, &captured).await;
    assert!(
        declared.iter().any(|name| name == "tool_search"),
        "the next loadout declares tool_search: {declared:?}"
    );
    assert!(!declared.iter().any(|name| name == "codemode"));
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_enable_codemode_false_leaves_codemode_inactive() {
    let _guard = env_guard().await;
    let dir = TempDir::new("no-auto");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    write_mcp_json(&dir.0, false, json!({ "docs": server("codemode") }));

    let mcp = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let (session, captured) = build_session(
        &dir.0,
        vec![builtin("codemode"), builtin("tool-search"), mcp.clone()],
        None,
    );

    assert!(
        mcp.auto_activated_tools().is_empty(),
        "autoEnableCodemode:false suppresses the codemode activation"
    );
    let active = mcp.active_tools_snapshot().expect("a context is bound");
    assert!(!active.iter().any(|name| name == "codemode"));

    let declared = declared_tools(&session, &captured).await;
    assert!(
        !declared.iter().any(|name| name == "codemode"),
        "the loadout stays without codemode: {declared:?}"
    );
    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// Ownership: removal, stability, precedence
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn removing_or_disabling_servers_removes_only_auto_added_names() {
    let _guard = env_guard().await;
    let dir = TempDir::new("ownership");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    write_mcp_json(
        &dir.0,
        true,
        json!({
            "docs": server("codemode"),
            "search": server("deferred"),
        }),
    );

    let mcp = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let (session, _captured) = build_session(
        &dir.0,
        vec![builtin("codemode"), builtin("tool-search"), mcp.clone()],
        None,
    );
    let host = session.plugin_host_arc().expect("plugin host");

    assert_eq!(
        mcp.auto_activated_tools(),
        names(&["codemode", "tool_search"]),
        "both discovery tools are owned"
    );

    // Removal of the codemode server drops only codemode.
    mcp.manager().registry().unregister("docs");
    emit_settled(&host).await;
    assert_eq!(mcp.auto_activated_tools(), names(&["tool_search"]));
    let active = mcp.active_tools_snapshot().expect("context");
    assert!(!active.iter().any(|name| name == "codemode"));
    assert!(
        active.iter().any(|name| name == "tool_search"),
        "tool_search's server is still enabled: {active:?}"
    );

    // Two more rebuild/flush cycles: no duplicate, ownership still `Owned`
    // (a flipped ownership would keep codemode on the later removal).
    emit_settled(&host).await;
    emit_settled(&host).await;
    assert_eq!(mcp.auto_activated_tools(), names(&["tool_search"]));
    let active = mcp.active_tools_snapshot().expect("context");
    assert_eq!(
        active
            .iter()
            .filter(|name| name.as_str() == "tool_search")
            .count(),
        1,
        "no duplicate active entry: {active:?}"
    );

    // Disabling the last server removes tool_search too.
    let mut entry = mcp
        .manager()
        .registry()
        .get("search")
        .expect("registered server");
    entry.config = config_with_enabled(&entry.config, false);
    mcp.manager().registry().register(entry);
    emit_settled(&host).await;
    assert!(mcp.auto_activated_tools().is_empty());
    let active = mcp.active_tools_snapshot().expect("context");
    assert!(!active.iter().any(|name| name == "tool_search"));
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_explicit_codemode_selection_is_never_removed() {
    let _guard = env_guard().await;
    let dir = TempDir::new("external");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    write_mcp_json(&dir.0, true, json!({ "docs": server("codemode") }));

    let mcp = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let selection = ToolSelection {
        enabled: names(&["codemode"]),
        modifiers: Vec::new(),
        uses_default_tools: false,
    };
    let (session, _captured) = build_session(
        &dir.0,
        vec![builtin("codemode"), builtin("tool-search"), mcp.clone()],
        Some(selection),
    );
    let host = session.plugin_host_arc().expect("plugin host");

    assert!(
        mcp.auto_activated_tools().is_empty(),
        "codemode was already active (user selection): not owned"
    );
    assert!(mcp
        .active_tools_snapshot()
        .expect("context")
        .iter()
        .any(|name| name == "codemode"));

    // The server disappears, but the user's explicit selection stays.
    mcp.manager().registry().unregister("docs");
    emit_settled(&host).await;
    assert!(mcp.auto_activated_tools().is_empty());
    assert!(
        mcp.active_tools_snapshot()
            .expect("context")
            .iter()
            .any(|name| name == "codemode"),
        "the user's --tools selection is never removed by auto-activation"
    );
    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// `/mcp`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mcp_command_lists_servers_with_tool_counts_and_updates_activation() {
    let _guard = env_guard().await;
    let dir = TempDir::new("command");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    write_mcp_json(&dir.0, true, json!({ "docs": server("codemode") }));

    let mcp = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let (session, _captured) = build_session(
        &dir.0,
        vec![builtin("codemode"), builtin("tool-search"), mcp.clone()],
        None,
    );
    let host = session.plugin_host_arc().expect("plugin host");

    // The report: each server, state/exposure/source, and its tool count.
    let report = session
        .dispatch_plugin_command("mcp", "")
        .await
        .expect("native mcp command");
    let text = report.error.expect("display text");
    assert!(text.contains("docs"), "the server is listed: {text}");
    assert!(
        text.contains("0 tools"),
        "the discovered tool count is shown: {text}"
    );
    assert!(text.contains("codemode"), "the exposure is shown: {text}");

    // `/mcp disable` drops the auto-added codemode immediately...
    let outcome = session
        .dispatch_plugin_command("mcp", "disable docs")
        .await
        .expect("disable");
    assert!(outcome.ok, "{outcome:?}");
    assert!(
        mcp.auto_activated_tools().is_empty(),
        "a disabled server no longer needs codemode"
    );
    assert!(!mcp
        .active_tools_snapshot()
        .expect("context")
        .iter()
        .any(|name| name == "codemode"));

    // ... and a rebuild does not resurrect it (the session override holds).
    host.rebuild_registry().await;
    emit_settled(&host).await;
    assert!(mcp.auto_activated_tools().is_empty());

    // `/mcp enable` restores it.
    let outcome = session
        .dispatch_plugin_command("mcp", "enable docs")
        .await
        .expect("enable");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(mcp.auto_activated_tools(), names(&["codemode"]));

    // `/mcp remove` unregisters + drops it; `enable` can restore it from
    // `mcp.json` within the session.
    let outcome = session
        .dispatch_plugin_command("mcp", "remove docs")
        .await
        .expect("remove");
    assert!(outcome.ok, "{outcome:?}");
    assert!(mcp.manager().registry().get("docs").is_none());
    assert!(mcp.auto_activated_tools().is_empty());
    let outcome = session
        .dispatch_plugin_command("mcp", "enable docs")
        .await
        .expect("restore");
    assert!(outcome.ok, "{outcome:?}");
    assert!(mcp.manager().registry().get("docs").is_some());
    assert_eq!(mcp.auto_activated_tools(), names(&["codemode"]));
    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// Direct-exposure bridges vs a JS `setActiveTools` snapshot
// ---------------------------------------------------------------------------
//
// Upstream activates a `direct` tool when it registers, so a connected MCP
// server's bridges stay in the active set no matter what an unrelated
// extension's `setActiveTools` snapshot enumerates. pir's active set is an
// override list: such a snapshot used to prune every `mcp__*` name out of the
// resolved set AND out of the run registry (`merge_plugin_tools`' override
// filter), so the model kept a declaration it could not call and every MCP
// call failed with "Tool not found" until something rewrote the store (the
// incident: a declaration that removed 155 tools, every `mcp__mymcp__*`
// included, while the server stayed connected).
//
// These tests pin the source-distinguishing rule: a DEFAULT-tools session keeps
// the bridges, an explicit `--tools`/`--no-tools` selection never gains them,
// and a `-name` exclusion outranks registration-time activation.
//
// Hermetic: no server is ever spawned. The bridge is built through the same
// `McpToolBridge::with_effective_exposure` path `McpPlugin::register` uses; the
// server entry only has to exist (enabled) in the manager's registry for the
// namespace gate.

mod direct_bridge {
    use super::*;

    use pir::agent::tool_batch::ToolResult;
    use pir::agent::tool_execution::{
        AgentTool, BoxFuture, ToolContext, ToolExposure, ToolNamespace,
    };
    use pir::config::mcp::McpExposure;
    use pir::extensions::{ExtensionConfig, ExtensionHandle};
    use pir::mcp::manager::DiscoveredTool;
    use pir::mcp::protocol::McpTool;
    use pir::mcp::registry::{McpServerEntry, McpServerSource};
    use pir::plugins::builtin::McpToolBridge;
    use pir::plugins::plugin::RegistrationContext;
    use pir::plugins::source::PluginInfo;

    /// A bridge for `server`'s tool at `exposure`, built exactly like
    /// `McpPlugin::register` builds it.
    fn bridge(
        plugin: &McpPlugin,
        server: &str,
        tool: &str,
        exposure: McpExposure,
    ) -> McpToolBridge {
        let discovered = DiscoveredTool {
            server: server.to_owned(),
            tool: McpTool {
                name: tool.to_owned(),
                description: Some(format!("{server} {tool}")),
                ..Default::default()
            },
            exposure,
        };
        let registered = pir::mcp::registry::assign_mcp_tool_names(server, &[tool.to_owned()])
            .into_iter()
            .next()
            .expect("assigned name");
        McpToolBridge::with_effective_exposure(
            plugin.manager().clone(),
            discovered,
            registered,
            exposure,
        )
    }

    /// The extension tool an unrelated package registered: a name that is NOT
    /// an MCP tool (the extension snapshot's shape).
    struct WebSearchTool;

    impl AgentTool for WebSearchTool {
        fn name(&self) -> &'static str {
            "web_search"
        }
        fn description(&self) -> &'static str {
            "extension web search"
        }
        fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
            Box::pin(async { ToolResult::ok("searched") })
        }
    }

    /// Registers a fixed tool list on the host build (another package's
    /// registrations appearing alongside `mcp`).
    struct ToolSetPlugin {
        info: PluginInfo,
        tools: Vec<Arc<dyn AgentTool>>,
    }

    impl AgentPlugin for ToolSetPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
            for tool in &self.tools {
                ctx.register_tool(Arc::clone(tool));
            }
            Ok(())
        }
    }

    /// A JS runtime whose only job is to write the `setActiveTools` store (an
    /// extension reload's snapshot). It registers no tools of its own: the
    /// snapshot's names need not exist for the store to prune the active set,
    /// which is exactly the incident's shape.
    fn runtime_at(dir: &Path) -> ExtensionHandle {
        ExtensionHandle::start(ExtensionConfig {
            cwd: dir.to_path_buf(),
            mode: "headless".to_owned(),
            has_ui: false,
            model: Some("fake-model".to_owned()),
            session_id: Some("mcp-direct-bridge".to_owned()),
            system_prompt: String::new(),
            is_idle: true,
            limits: Some(Default::default()),
        })
        .expect("extension runtime")
    }

    /// The JS runtime's `setActiveTools([...])`: the same store
    /// `setActiveToolsJson` writes and `PluginHost::active_tools_override`
    /// reads. The host fn takes the JSON ARRAY ENCODED AS A STRING (the prelude
    /// serializes before the crossing), so the argument is JSON-quoted.
    async fn write_js_override(session: &AgentSession, names: &[&str]) {
        let runtime = session
            .extensions_handle()
            .expect("the session has a JS runtime");
        let array = serde_json::to_string(names).expect("encode names");
        let argument = serde_json::to_string(&array).expect("encode argument");
        // `eval_json` reads a STRING result and JSON-parses it; the host fn
        // returns `undefined`, so the comma expression evaluates the call and
        // then yields a JSON string.
        runtime
            .eval_json(&format!(
                "(__pir_host.setActiveToolsJson({argument}), JSON.stringify({{ ok: true }}))"
            ))
            .await
            .expect("setActiveTools");
    }

    /// Build the session: the real `mcp` plugin (so the manager owns the
    /// server entry), a bridge registered through the plugin path, one
    /// extension tool, and a JS runtime that writes `setActiveTools`.
    fn session_with_bridge(
        dir: &Path,
        selection: Option<ToolSelection>,
        exposure: McpExposure,
        server: &str,
    ) -> (Arc<AgentSession>, Arc<McpPlugin>, Arc<dyn AgentTool>) {
        let mcp = Arc::new(McpPlugin::with_cwd(dir, dir));
        // The server the bridge belongs to (registered, never connected). An
        // EXTENSION source: the config pass only removes stale `mcp.json`
        // entries, so this registration survives the host's rebuilds.
        mcp.manager().registry().register(McpServerEntry {
            name: server.to_owned(),
            config: serde_json::from_value(json!({ "command": "true" })).expect("config"),
            source: McpServerSource::Extension("mcp-direct-bridge-test".to_owned()),
        });
        let bridge: Arc<dyn AgentTool> = Arc::new(bridge(&mcp, server, "search", exposure));

        let mut tools = ToolRegistry::new();
        pir::coding_agent::register_core_tools(&mut tools);
        let captured: CapturedToolSets = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(CapturingProvider {
            calls: AtomicUsize::new(0),
            captured,
        }));
        let session = AgentSession::new_arc_with_plugins(
            AgentSessionConfig {
                provider_registry: Arc::new(registry),
                model: fake_model(),
                api_key: None,
                thinking: ThinkingLevel::Off,
                tools,
                session: None,
                compaction: None,
                max_turns: 2,
                system_prompt: Some("mcp direct bridges".into()),
                cwd: Some(dir.to_path_buf()),
                event_stream: None,
                extensions: Some(runtime_at(dir)),
                tool_selection: selection,
            },
            vec![
                Arc::new(ToolSetPlugin {
                    info: PluginInfo::inline("web-extension"),
                    tools: vec![Arc::new(WebSearchTool), Arc::clone(&bridge)],
                }) as Arc<dyn AgentPlugin>,
                mcp.clone() as Arc<dyn AgentPlugin>,
            ],
        );
        (session, mcp, bridge)
    }

    fn active(session: &AgentSession) -> Vec<String> {
        let (_, active) = session.active_tool_snapshot(session.plugin_host());
        active
    }

    fn run_registry_names(session: &AgentSession) -> Vec<String> {
        session
            .merged_tool_registry_for_test(session.plugin_host().expect("plugin host"))
            .0
            .names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// (a) An extension's `setActiveTools` snapshot naming only its own tools
    /// must not deactivate a registered direct MCP bridge.
    ///
    /// Revert caught (two halves, both asserted):
    /// - the resolution: `active_tool_snapshot` loses `mcp__mymcp__search`, so
    ///   the model is told the tool does not exist;
    /// - the run registry: `merge_plugin_tools` prunes the bridge for the JS
    ///   override, so even a name in the active set would execute as "Tool not
    ///   found" (the incident's exact symptom).
    #[tokio::test(flavor = "current_thread")]
    async fn a_js_snapshot_without_mcp_names_keeps_direct_bridges_active() {
        let _guard = env_guard().await;
        let dir = TempDir::new("direct-keep");
        std::env::set_var("PIR_AGENT_DIR", &dir.0);
        let (session, mcp, bridge) =
            session_with_bridge(&dir.0, None, McpExposure::Direct, "mymcp");

        // Precondition: the bridge is registered, direct, and active.
        let name = bridge.name().to_owned();
        assert_eq!(name, "mcp__mymcp__search");
        let before = active(&session);
        assert!(
            before.contains(&name),
            "a direct bridge is active before any snapshot: {before:?}"
        );

        // The extension reload's snapshot: its own tools, no MCP names.
        write_js_override(&session, &["web_search"]).await;
        mcp.recompute_discovery_activation();

        let after = active(&session);
        assert!(
            after.contains(&name),
            "the direct MCP bridge stays active through the snapshot: {after:?}"
        );
        assert!(
            after.iter().any(|tool| tool == "web_search"),
            "the extension's own tool is kept: {after:?}"
        );
        // The run registry still carries it, so a call is resolvable.
        let registry = run_registry_names(&session);
        assert!(
            registry.iter().any(|tool| tool == &name),
            "the bridge is still in the run registry: {registry:?}"
        );

        // The recompute did not PIN the session cell: the JS store keeps its
        // precedence, so a later `setActiveTools` from the extension is still
        // visible (a cell write here would mask it forever).
        write_js_override(&session, &["web_search", "read"]).await;
        assert!(
            active(&session).iter().any(|tool| tool == "read"),
            "the JS store still wins after a recompute: {:?}",
            active(&session)
        );

        // The incident's recovery declaration (restored by a later reload)
        // changes nothing.
        write_js_override(&session, &["web_search", &name]).await;
        mcp.recompute_discovery_activation();
        assert!(active(&session).contains(&name));
        session.shutdown().await;
    }

    /// (b) An explicit `--tools` selection stays authoritative: the rule never
    /// widens it with registration-time MCP names.
    #[tokio::test(flavor = "current_thread")]
    async fn an_explicit_selection_never_gains_direct_bridges() {
        let _guard = env_guard().await;
        let dir = TempDir::new("direct-explicit");
        std::env::set_var("PIR_AGENT_DIR", &dir.0);
        let selection = ToolSelection {
            enabled: names(&["read"]),
            modifiers: Vec::new(),
            uses_default_tools: false,
        };
        let (session, mcp, bridge) =
            session_with_bridge(&dir.0, Some(selection), McpExposure::Direct, "mymcp");
        let name = bridge.name().to_owned();

        let resolved = active(&session);
        assert!(
            !resolved.contains(&name),
            "an explicit --tools selection is not widened: {resolved:?}"
        );
        assert!(
            resolved.iter().any(|tool| tool == "read"),
            "the selection itself is active: {resolved:?}"
        );

        // And the recompute keeps it that way.
        mcp.recompute_discovery_activation();
        let resolved = active(&session);
        assert!(
            !resolved.contains(&name),
            "the recompute keeps an explicit selection intact: {resolved:?}"
        );
        session.shutdown().await;
    }

    /// A `-name` exclusion outranks registration-time activation, even on a
    /// default-tools session (the resolver applies the same precedence to the
    /// base set).
    #[tokio::test(flavor = "current_thread")]
    async fn an_excluded_bridge_is_not_reactivated() {
        let _guard = env_guard().await;
        let dir = TempDir::new("direct-excluded");
        std::env::set_var("PIR_AGENT_DIR", &dir.0);
        let selection = ToolSelection {
            enabled: names(&["read"]),
            modifiers: names(&["-mcp__mymcp__search"]),
            uses_default_tools: true,
        };
        let (session, mcp, bridge) =
            session_with_bridge(&dir.0, Some(selection), McpExposure::Direct, "mymcp");
        let name = bridge.name().to_owned();

        mcp.recompute_discovery_activation();
        let resolved = active(&session);
        assert!(
            !resolved.contains(&name),
            "a -name exclusion outranks registration-time activation: {resolved:?}"
        );
        session.shutdown().await;
    }

    /// A codemode/deferred bridge is NOT part of the rule: only `direct`
    /// exposure activates on registration upstream (`McpToolBridge` maps both
    /// MCP discovery exposures to the registry's `Deferred`).
    #[tokio::test(flavor = "current_thread")]
    async fn a_non_direct_bridge_is_not_unioned() {
        let _guard = env_guard().await;
        let dir = TempDir::new("direct-codemode");
        std::env::set_var("PIR_AGENT_DIR", &dir.0);
        let (session, mcp, bridge) =
            session_with_bridge(&dir.0, None, McpExposure::Codemode, "mymcp");
        let name = bridge.name().to_owned();

        // Without any override the bridge is registered with its discovery
        // exposure and NOT active (the upstream contract for non-direct tools:
        // callable from codemode, never declared by default).
        let registry = session
            .merged_tool_registry_for_test(session.plugin_host().expect("plugin host"))
            .0;
        assert_eq!(
            registry.get(&name).map(|tool| tool.exposure()),
            Some(ToolExposure::Deferred)
        );
        assert!(
            !active(&session).contains(&name),
            "a codemode bridge activates by default? {name}"
        );

        // A JS snapshot naming other tools does not bring it in either.
        write_js_override(&session, &["web_search"]).await;
        mcp.recompute_discovery_activation();
        let resolved = active(&session);
        assert!(
            !resolved.contains(&name),
            "a codemode bridge is reachable, not active: {resolved:?}"
        );
        session.shutdown().await;
    }

    /// A disabled server's bridges are not re-activated: disabling disconnects
    /// but keeps the entry, and waking its tools would contradict the user's
    /// `/mcp disable`. The JS snapshot first takes the bridge out of the
    /// default set, so only the union could bring it back — which is exactly
    /// what must not happen.
    #[tokio::test(flavor = "current_thread")]
    async fn a_disabled_servers_bridges_are_not_woken() {
        let _guard = env_guard().await;
        let dir = TempDir::new("direct-disabled");
        std::env::set_var("PIR_AGENT_DIR", &dir.0);
        let (session, mcp, bridge) =
            session_with_bridge(&dir.0, None, McpExposure::Direct, "mymcp");
        let name = bridge.name().to_owned();

        let entry = mcp.manager().registry().get("mymcp").expect("entry");
        mcp.manager().registry().register(McpServerEntry {
            config: config_with_enabled(&entry.config, false),
            ..entry
        });
        write_js_override(&session, &["web_search"]).await;

        mcp.recompute_discovery_activation();
        let resolved = active(&session);
        assert!(
            !resolved.contains(&name),
            "a disabled server's tools are not activated: {resolved:?}"
        );
        session.shutdown().await;
    }

    /// The fixture speaks the real MCP namespace: the registered name
    /// sanitizes the server segment, which is what the enabled-server gate
    /// matches.
    #[test]
    fn the_fixture_speaks_the_mcp_namespace() {
        let plugin = McpPlugin::with_cwd(&std::env::temp_dir(), &std::env::temp_dir());
        let hyphenated = bridge(&plugin, "my-mcp", "search", McpExposure::Direct);
        let namespace: ToolNamespace = hyphenated.namespace().expect("namespace");
        assert_eq!(namespace.name, "mcp__my_mcp");
        // `plain_mcp_tool_name` sanitizes the whole name, server segment too.
        assert_eq!(hyphenated.name(), "mcp__my_mcp__search");
        assert_eq!(hyphenated.exposure(), ToolExposure::Direct);
    }
}
