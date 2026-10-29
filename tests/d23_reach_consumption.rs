//! D23 — the Rust **consumption** side of `reachesTools` + namespace
//! `aliases` (upstream `24382bedd` "decouple MCP from codemode", `8bb180bec`):
//!
//! - `ToolNamespace.aliases` travels the whole Rust path (JS wire def →
//!   `ToolRegistry`/`PluginRegistry` def → MCP bridge namespace);
//! - the MCP side reads a tool's reach to wait for the servers it may reach
//!   (upstream `reachesServer`, `extensions/mcp/index.ts:224-228`) and to
//!   auto-activate the discovery tool a server's configured exposure asks for
//!   (upstream `ensureDiscoveryActive`, index.ts:498-537);
//! - the two host-side consumers are wired: the session applies the
//!   activation before each run's loadout (core tools and the `--tools`
//!   selection stay active, `autoEnableCodemode` comes from `mcp.json`) and
//!   hands the loop a `tool_call` wait hook; the loop awaits it per batch;
//! - the aggregate resource tools (`list_mcp_resources`,
//!   `list_mcp_resource_templates`, `read_mcp_resource`) are registered and
//!   exposure-gated by the servers that have resources;
//! - `tool_search` declares `reachesTools: () => "all"` and `codemode`
//!   declares its script reach (upstream `getScriptReach`, execute.ts:362-369);
//! - the namespace-alias lookup rule (upstream `isNamespaceName`,
//!   execute.ts:559-568) finds an MCP namespace by its server alias, both for
//!   `describeNamespace` queries and for a JS tool's reach words;
//! - **(f) a JS extension tool's `reachesTools` hook crosses the boundary**:
//!   the live function stays in QuickJS, the registry export carries the
//!   hook's empty-input snapshot into `ExtensionTool` (so the activation gate
//!   and the synchronous registry consumers see the declaration), and the
//!   tool-call wait resolves the LIVE per-call value through the runtime.
//!
//! The extension-side register/`getAllTools` round trip lives in
//! `tests/upstream_w2_drift.rs` (`d23_*`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::agent_loop::{run_with_options, LoopDeps, LoopOptions};
use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{
    merge_extension_tools, merge_plugin_tools, AgentTool, BoxFuture, ToolContext, ToolExposure,
    ToolNamespace, ToolReach, ToolRegistry,
};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, Usage,
};
use pir::config::mcp::{load_mcp_configs, parse_mcp_config_document, McpExposure, McpServerConfig};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::mcp::manager::{DiscoveredTool, McpManager};
use pir::mcp::protocol::McpTool;
use pir::mcp::registry::{McpServerEntry, McpServerSource, MCP_RESOURCE_TOOLS};
use pir::plugins::builtin::{
    configured_exposures, mcp_server_namespace, mcp_tool_owner, namespace_matches, reaches_server,
    reaches_server_by_alias, resource_exposure, script_reach, McpPlugin, McpToolBridge,
    CODEMODE_TOOL_NAME, TOOL_SEARCH_TOOL_NAME,
};
use pir::plugins::plugin::AgentPlugin;
use pir::plugins::{instantiate_builtin, PluginHost};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-d23-reach-{label}-{}-{}",
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

/// A tool with a fixed reach (or none): the Rust-level stand-in for a JS
/// extension's `reachesTools` hook, used to exercise the MCP consumers without
/// a runtime. The real JS extension tools (whose hook lives in QuickJS) are
/// covered in the (f) section below.
struct ReachTool {
    name: &'static str,
    reach: Option<ReachKind>,
}

enum ReachKind {
    /// `() => "all"` — `tool_search`.
    All,
    /// `(input) => getScriptReach(input.code)` — `codemode`.
    Script,
    /// A fixed word list.
    Words(Vec<String>),
}

impl AgentTool for ReachTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "reach test tool"
    }
    fn reaches_tools(&self, input: &Value) -> Option<ToolReach> {
        self.reach.as_ref().map(|reach| match reach {
            ReachKind::All => ToolReach::All,
            ReachKind::Script => script_reach(input.get("code").unwrap_or(&Value::Null)),
            ReachKind::Words(words) => ToolReach::Names(words.clone()),
        })
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// A tool that declares a namespace with aliases (a Rust tool, so the
/// `PluginRegistry` def path is exercised too).
struct AliasedNamespaceTool;

impl AgentTool for AliasedNamespaceTool {
    fn name(&self) -> &'static str {
        "aliased"
    }
    fn description(&self) -> &'static str {
        "declares a namespace with aliases"
    }
    fn namespace(&self) -> Option<ToolNamespace> {
        Some(ToolNamespace {
            name: "mcp__docs".to_owned(),
            aliases: vec!["docs".to_owned()],
            description: Some("Docs server".to_owned()),
            instructions: None,
        })
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

fn registry_with(tools: Vec<Arc<dyn AgentTool>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

fn stdio_config(exposure: Option<McpExposure>) -> McpServerConfig {
    let mut value = json!({ "command": "true" });
    if let Some(exposure) = exposure {
        value["exposure"] = serde_json::to_value(exposure).expect("exposure");
    }
    serde_json::from_value(value).expect("config")
}

fn register_server(plugin: &McpPlugin, name: &str, exposure: Option<McpExposure>) {
    plugin.manager().registry().register(McpServerEntry {
        name: name.to_owned(),
        config: stdio_config(exposure),
        source: McpServerSource::GlobalConfig(PathBuf::from("mcp.json")),
    });
}

// ---------------------------------------------------------------------------
// aliases on ToolNamespace / the plugin registry def
// ---------------------------------------------------------------------------

#[test]
fn d23_tool_namespace_serializes_aliases_and_skips_them_when_empty() {
    let without = ToolNamespace {
        name: "mcp__docs".to_owned(),
        ..Default::default()
    };
    let wire = serde_json::to_value(&without).expect("serialize");
    assert!(
        wire.get("aliases").is_none(),
        "an empty alias list stays off the wire (skip_serializing_if): {wire}"
    );
    // Both fields the typed Rust mirror now models.
    let with = ToolNamespace {
        name: "mcp__docs".to_owned(),
        aliases: vec!["docs".to_owned()],
        description: Some("Docs server".to_owned()),
        instructions: None,
    };
    let wire = serde_json::to_value(&with).expect("serialize");
    assert_eq!(wire["aliases"], json!(["docs"]));
    let parsed: ToolNamespace = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(parsed, with);
    // Missing aliases (older payloads, JS tools without them) deserialize.
    let parsed: ToolNamespace =
        serde_json::from_value(json!({ "name": "mcp__docs", "description": "Docs" }))
            .expect("deserialize without aliases");
    assert!(parsed.aliases.is_empty());
}

#[test]
fn d23_namespace_aliases_reach_the_plugin_registry_def() {
    // `RegistrationContext::register_tool` builds the `ToolDef` the host and
    // the JS `getAllTools` face read; the namespace aliases must survive it.
    let host = PluginHost::build(None, vec![Arc::new(ToolRegistrationPlugin)]);
    let defs = host.tool_defs();
    let def = defs
        .iter()
        .find(|def| def.name == "aliased")
        .expect("registered def");
    let namespace = def.namespace.as_ref().expect("namespace");
    assert_eq!(namespace.name, "mcp__docs");
    assert_eq!(namespace.aliases, vec!["docs".to_owned()]);
    assert_eq!(namespace.description.as_deref(), Some("Docs server"));
}

/// Registers [`AliasedNamespaceTool`] through the plugin path.
struct ToolRegistrationPlugin;

impl pir::plugins::plugin::AgentPlugin for ToolRegistrationPlugin {
    fn info(&self) -> &pir::plugins::PluginInfo {
        static INFO: std::sync::OnceLock<pir::plugins::PluginInfo> = std::sync::OnceLock::new();
        INFO.get_or_init(|| pir::plugins::PluginInfo::builtin("d23-test"))
    }
    fn register(
        &self,
        ctx: &mut pir::plugins::plugin::RegistrationContext<'_>,
    ) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(AliasedNamespaceTool));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The MCP namespace (aliases: [server]) + namespace lookup rule
// ---------------------------------------------------------------------------

fn bridge_namespace(server: &str) -> ToolNamespace {
    let manager = McpManager::without_log(Path::new("."));
    let discovered = DiscoveredTool {
        server: server.to_owned(),
        tool: McpTool {
            name: "search".to_owned(),
            ..Default::default()
        },
        exposure: McpExposure::Codemode,
    };
    let registered = pir::mcp::registry::assign_mcp_tool_names(server, &["search".to_owned()])
        .into_iter()
        .next()
        .expect("name");
    McpToolBridge::new(manager, discovered, registered)
        .namespace()
        .expect("namespace")
}

#[test]
fn d23_mcp_namespace_carries_the_server_alias() {
    let namespace = bridge_namespace("docs");
    assert_eq!(namespace.name, "mcp__docs");
    assert_eq!(
        namespace.aliases,
        vec!["docs".to_owned()],
        "the MCP bridge declares `aliases: [server]` (upstream registerTools)"
    );
}

#[test]
fn d23_namespace_lookup_accepts_name_alias_and_script_identifier() {
    let docs = bridge_namespace("docs");
    // `describeNamespace("docs")` finds `mcp__docs` (the upstream comment).
    assert!(namespace_matches(&docs, "docs"));
    assert!(namespace_matches(&docs, "mcp__docs"));
    assert!(!namespace_matches(&docs, "mcp__other"));

    // A hyphenated server name: the alias must match as written and as the
    // script identifier (upstream `toCodemodeIdentifier` normalization).
    let radius = bridge_namespace("dev-radius");
    assert_eq!(radius.name, "mcp__dev_radius");
    assert_eq!(radius.aliases, vec!["dev-radius".to_owned()]);
    assert!(namespace_matches(&radius, "dev-radius"));
    assert!(namespace_matches(&radius, "dev_radius"));
    assert!(namespace_matches(&radius, "mcp__dev_radius"));
    assert!(namespace_matches(&radius, "mcp__dev-radius"));
    assert!(!namespace_matches(&radius, "dev_other"));

    // No aliases: only the name (and its identifier form) matches.
    let plain = ToolNamespace {
        name: "mcp__plain".to_owned(),
        ..Default::default()
    };
    assert!(namespace_matches(&plain, "mcp__plain"));
    assert!(!namespace_matches(&plain, "plain"));
}

// ---------------------------------------------------------------------------
// reachesServer
// ---------------------------------------------------------------------------

#[test]
fn d23_reaches_server_matches_like_upstream() {
    // `"all"` reaches every server.
    assert!(reaches_server(&ToolReach::All, "docs"));
    assert!(reaches_server(&ToolReach::All, "anything"));

    // A word reaches a server whose NAMESPACE it contains (over-matching is
    // allowed, upstream `word.includes(namespace)`).
    let reach = ToolReach::Names(vec!["tools.mcp__docs__search".to_owned()]);
    assert!(reaches_server(&reach, "docs"));
    assert!(!reaches_server(&reach, "other"));

    // The rule is `word.contains(mcpNamespace(server))`: a bare `docs` word
    // reaches no server whose namespace is `mcp__docs` (the word must contain
    // the namespace, not the reverse), while the namespace itself does.
    let words = ToolReach::Names(vec!["docs".to_owned()]);
    assert!(!reaches_server(&words, "docs"));
    assert!(!reaches_server(&words, "mcp__docs"));
    assert!(reaches_server(
        &ToolReach::Names(vec!["mcp__docs".to_owned()]),
        "docs"
    ));

    // Sanitized namespace: the word must carry the sanitized form.
    let hyphenated = ToolReach::Names(vec!["mcp__dev_radius".to_owned()]);
    assert!(reaches_server(&hyphenated, "dev-radius"));
    assert!(!reaches_server(&ToolReach::Names(Vec::new()), "docs"));

    // `word.contains(namespace)`: a longer word matches too.
    assert!(reaches_server(
        &ToolReach::Names(vec!["whatever_mcp__docs_trailing".to_owned()]),
        "docs"
    ));
}

// ---------------------------------------------------------------------------
// (f) the JS extension tool's reach: QuickJS -> registry export -> ExtensionTool
// ---------------------------------------------------------------------------

fn test_config_at(cwd: &Path, label: &str) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some(format!("d23-{label}")),
        system_prompt: "base system prompt".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// A runtime with one extension loaded from `source`.
async fn load_runtime(dir: &TempDir, label: &str, source: &str) -> ExtensionHandle {
    let path = dir.0.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let runtime = ExtensionHandle::start(test_config_at(&dir.0, label)).expect("runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    runtime
}

/// A JS extension whose tools declare reaches: a `codemode`-named tool that
/// reaches everything, a proxy naming the `docs` MCP server in its own words,
/// and a router whose reach is a function of the call's input (the live-only
/// case the export snapshot cannot answer).
const JS_REACH_EXTENSION: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "codemode",
    label: "codemode",
    description: "Another extension's codemode tool.",
    parameters: { type: "object", properties: {} },
    defaultActive: false,
    reachesTools: () => "all",
    async execute() { return { content: [] }; },
  });
  pi.registerTool({
    name: "docs_proxy",
    description: "Proxies the docs MCP server.",
    parameters: { type: "object", properties: {} },
    defaultActive: false,
    namespace: { name: "mcp__docs", aliases: ["docs"] },
    reachesTools: () => ["docs"],
    async execute() { return { content: [] }; },
  });
  pi.registerTool({
    name: "js_router",
    description: "Routes the call to the server its input names.",
    parameters: { type: "object", properties: { target: { type: "string" } } },
    defaultActive: false,
    reachesTools: (input) => (typeof input.target === "string" ? [input.target] : []),
    async execute() { return { content: [] }; },
  });
}
"#;

/// The live `reachesTools` hook cannot cross the JSON boundary, so the registry
/// export probes it (empty input, the same probe `ensureDiscoveryActive` makes
/// on Rust tools) and `ExtensionTool` carries the snapshot — every Rust-side
/// consumer now sees a JS tool's declaration instead of `None`.
#[tokio::test(flavor = "current_thread")]
async fn d23_js_tool_reach_crosses_from_quickjs_into_the_rust_registry() {
    let temp = TempDir::new("js-reach-ext");
    let runtime = load_runtime(&temp, "js-reach", JS_REACH_EXTENSION).await;

    // The runtime's own def snapshot (`__pir_exportRegistryJson`).
    let defs = runtime.tool_defs();
    let def = defs
        .iter()
        .find(|def| def.name == "docs_proxy")
        .expect("registered def");
    assert_eq!(
        def.reaches_tools
            .as_ref()
            .map(|snapshot| snapshot.0.clone()),
        Some(json!(["docs"])),
        "the export carries the hook's value for the empty-input probe"
    );
    let codemode = defs
        .iter()
        .find(|def| def.name == CODEMODE_TOOL_NAME)
        .expect("codemode def");
    assert_eq!(
        codemode
            .reaches_tools
            .as_ref()
            .map(|snapshot| snapshot.0.clone()),
        Some(json!("all"))
    );

    // Both host paths (the session's `merge_plugin_tools` and print's
    // `merge_extension_tools`) register the JS tools as `ExtensionTool`s.
    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &runtime);
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());
    merge_plugin_tools(&mut registry, &host, None);
    for name in [CODEMODE_TOOL_NAME, "docs_proxy", "js_router"] {
        let tool = registry.get(name).expect("registered");
        assert!(
            tool.as_any()
                .and_then(|any| any.downcast_ref::<pir::agent::tool_execution::ExtensionTool>())
                .is_some(),
            "{name} is a JS extension tool"
        );
    }
    assert_eq!(
        registry
            .get(CODEMODE_TOOL_NAME)
            .expect("tool")
            .reaches_tools(&json!({})),
        Some(ToolReach::All)
    );
    assert_eq!(
        registry
            .get("docs_proxy")
            .expect("tool")
            .reaches_tools(&json!({})),
        Some(ToolReach::Names(vec!["docs".to_owned()])),
        "a JS tool's reach is no longer `None` on the Rust side"
    );
    // The input-dependent hook snapshots its empty-input answer: the
    // DECLARATION survives (empty words), the call's words come from the live
    // hook.
    assert_eq!(
        registry
            .get("js_router")
            .expect("tool")
            .reaches_tools(&json!({ "target": "docs" })),
        Some(ToolReach::Names(Vec::new()))
    );

    // The activation gate (upstream `ensureDiscoveryActive`): the JS
    // `codemode` declares a reach, so a codemode-exposure server activates it
    // (the upstream test "activates another extension's codemode tool when it
    // reaches other tools").
    let mcp = McpPlugin::with_cwd(&temp.0, &temp.0);
    register_server(&mcp, "docs", Some(McpExposure::Codemode));
    let decision = mcp.discovery_activation(&registry, &[]);
    assert!(
        decision.has_codemode,
        "the JS tool's declaration is visible"
    );
    assert_eq!(decision.activate, vec![CODEMODE_TOOL_NAME.to_owned()]);

    // ... and a JS tool of the same name WITHOUT the hook is never activated
    // (upstream "does not activate another extension's tool named codemode").
    let plain_dir = TempDir::new("js-no-reach-ext");
    let plain = load_runtime(
        &plain_dir,
        "js-no-reach",
        r#"
        export default function (pi) {
          pi.registerTool({
            name: "codemode",
            description: "Another extension's codemode tool.",
            parameters: { type: "object", properties: {} },
            async execute() { return { content: [] }; },
          });
        }
        "#,
    )
    .await;
    let mut plain_registry = ToolRegistry::new();
    merge_extension_tools(&mut plain_registry, &plain);
    let plain_decision = mcp.discovery_activation(&plain_registry, &[]);
    assert!(!plain_decision.has_codemode);
    assert!(plain_decision.activate.is_empty());
}

/// The D23 alias rule for the JS side's reach words: a word that NAMES the
/// server — its alias (`docs`), its namespace (`mcp__docs`), or the sanitized
/// script identifier (`dev_radius` for `dev-radius`) — reaches it, the same
/// lookup `describeNamespace` uses.
#[test]
fn d23_js_reach_words_name_servers_through_aliases() {
    let temp = TempDir::new("js-alias");
    let plugin = McpPlugin::with_cwd(&temp.0, &temp.0);
    register_server(&plugin, "docs", None);
    register_server(&plugin, "dev-radius", None);
    register_server(&plugin, "other", None);

    // The namespace the alias rule reads: the bridge's own shape.
    let docs = mcp_server_namespace("docs");
    assert_eq!(docs.name, "mcp__docs");
    assert_eq!(docs.aliases, vec!["docs".to_owned()]);
    assert_eq!(mcp_server_namespace("dev-radius").name, "mcp__dev_radius");

    // Upstream's rule stays as it is: the word must CONTAIN the namespace.
    let bare = ToolReach::Names(vec!["docs".to_owned()]);
    assert!(!reaches_server(&bare, "docs"));
    assert!(reaches_server(
        &ToolReach::Names(vec!["mcp__docs".to_owned()]),
        "docs"
    ));

    // The alias rule: both spellings of the name reach the server.
    let words = |words: &[&str]| ToolReach::Names(words.iter().map(|w| (*w).to_owned()).collect());
    assert!(reaches_server_by_alias(&bare, "docs"));
    assert!(reaches_server_by_alias(&words(&["mcp__docs"]), "docs"));
    for word in ["dev-radius", "dev_radius", "mcp__dev_radius"] {
        assert!(
            reaches_server_by_alias(&words(&[word]), "dev-radius"),
            "`{word}` names the dev-radius server"
        );
    }
    assert!(!reaches_server_by_alias(&bare, "other"));
    assert!(!reaches_server_by_alias(&words(&["nobody"]), "docs"));
    assert!(reaches_server_by_alias(&ToolReach::All, "other"));

    // The wait follows the same resolution: a JS tool naming one server waits
    // for exactly that server (a `"all"` reach waits for every enabled one).
    let wait = |reach: ToolReach| plugin.waiting_servers_for_reach("docs_proxy", Some(&reach));
    assert_eq!(wait(words(&["docs"])), vec!["docs".to_owned()]);
    assert_eq!(wait(words(&["dev_radius"])), vec!["dev-radius".to_owned()]);
    assert_eq!(
        wait(words(&["docs", "other"])),
        vec!["docs".to_owned(), "other".to_owned()]
    );
    assert!(wait(words(&["nobody"])).is_empty());
    assert_eq!(
        plugin.waiting_servers_for_reach(CODEMODE_TOOL_NAME, Some(&ToolReach::All)),
        vec![
            "dev-radius".to_owned(),
            "docs".to_owned(),
            "other".to_owned()
        ],
        "`\"all\"` waits for every enabled server (registry order)"
    );
    // `None` (upstream's `reach === undefined`) falls through to the
    // resource/owner rules: a plain JS tool name waits for nothing.
    assert!(plugin
        .waiting_servers_for_reach("docs_proxy", None)
        .is_empty());
}

/// The tool-call wait resolves the LIVE hook for JS tools: the export snapshot
/// cannot know the call, the runtime can — the same tool reaches a different
/// server per call (upstream `tool.reachesTools?.(event.input)`), and the
/// awaited server's connection object exists before the call runs.
#[tokio::test(flavor = "current_thread")]
async fn d23_wait_for_tool_call_resolves_a_js_reach_per_call() {
    let temp = TempDir::new("js-live");
    let runtime = load_runtime(&temp, "js-live", JS_REACH_EXTENSION).await;
    let plugin = McpPlugin::with_cwd(&temp.0, &temp.0);
    plugin.attach_js(Some(runtime.clone()));
    register_bogus_server(&plugin, "docs");
    register_bogus_server(&plugin, "other");
    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &runtime);
    assert!(!plugin.manager().has_connection("docs").await);

    let waited = plugin
        .wait_for_tool_call(&registry, "js_router", &json!({ "target": "mcp__docs" }))
        .await;
    assert_eq!(waited, vec!["docs".to_owned()]);
    assert!(
        plugin.manager().has_connection("docs").await,
        "the wait connected (or attempted) the reached server"
    );
    assert!(!plugin.manager().has_connection("other").await);

    // The same tool, another input: the live hook answers per call.
    let waited = plugin
        .wait_for_tool_call(&registry, "js_router", &json!({ "target": "other" }))
        .await;
    assert_eq!(waited, vec!["other".to_owned()]);

    // A static `"all"` hook waits for every enabled server.
    let waited = plugin
        .wait_for_tool_call(&registry, CODEMODE_TOOL_NAME, &json!({}))
        .await;
    assert_eq!(waited, vec!["docs".to_owned(), "other".to_owned()]);

    // An input the hook answers with no words waits for nothing.
    let waited = plugin
        .wait_for_tool_call(&registry, "js_router", &json!({}))
        .await;
    assert!(waited.is_empty());

    plugin.manager().shutdown().await;
}

/// A hook that cannot answer the export's empty-input probe — it throws on
/// `{}`, or answers with a Promise — still DECLARES its reach: the snapshot is
/// the presence marker (`true`), the activation gate sees it, and the
/// synchronous registry face reports a declared reach with no words. The wait
/// resolves the real value per call through the live probe, which also proves
/// the snapshot was not the only input.
#[tokio::test(flavor = "current_thread")]
async fn d23_js_reach_hook_that_rejects_the_probe_still_declares_its_reach() {
    let temp = TempDir::new("js-probe");
    let runtime = load_runtime(
        &temp,
        "js-probe",
        r#"
        export default function (pi) {
          pi.registerTool({
            name: "codemode",
            description: "Scans the script source, like the builtin codemode.",
            parameters: { type: "object", properties: {} },
            // Throws on the empty probe (`input.code` is undefined).
            reachesTools: (input) => input.code.split(" "),
            async execute() { return { content: [] }; },
          });
          pi.registerTool({
            name: "async_router",
            description: "Answers with a Promise (invalid for the sync contract).",
            parameters: { type: "object", properties: {} },
            defaultActive: false,
            reachesTools: async (input) => [input.target],
            async execute() { return { content: [] }; },
          });
        }
        "#,
    )
    .await;
    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &runtime);

    // Presence survives the failed probe; the words are empty (the snapshot has
    // none to give).
    assert_eq!(
        registry
            .get(CODEMODE_TOOL_NAME)
            .expect("tool")
            .reaches_tools(&json!({ "code": "await tools.mcp__docs__search({})" })),
        Some(ToolReach::Names(Vec::new()))
    );
    let plugin = McpPlugin::with_cwd(&temp.0, &temp.0);
    register_server(&plugin, "docs", Some(McpExposure::Codemode));
    assert!(
        plugin.discovery_activation(&registry, &[]).has_codemode,
        "the activation gate reads the declaration, not the probe value"
    );

    // With the runtime attached, the LIVE hook answers the real call: the
    // script names `mcp__docs`, so the docs server is awaited.
    plugin.attach_js(Some(runtime.clone()));
    register_bogus_server(&plugin, "other");
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            CODEMODE_TOOL_NAME,
            &json!({ "code": "await tools.mcp__docs__search({})" }),
        )
        .await;
    assert_eq!(waited, vec!["docs".to_owned()]);
    assert!(plugin.manager().has_connection("docs").await);

    // An ASYNC hook cannot answer the synchronous probe: the wait falls back to
    // the snapshot (declared, no words) — the documented residual boundary.
    let waited = plugin
        .wait_for_tool_call(&registry, "async_router", &json!({ "target": "mcp__docs" }))
        .await;
    assert!(waited.is_empty(), "a Promise answer is not awaited");

    plugin.manager().shutdown().await;
}

// ---------------------------------------------------------------------------
// The reaching tools' declarations (tool_search / codemode)
// ---------------------------------------------------------------------------

#[test]
fn d23_tool_search_declares_all_and_codemode_its_script_reach() {
    let host = PluginHost::build(
        None,
        vec![
            instantiate_builtin("tool-search").expect("tool-search builtin"),
            instantiate_builtin("codemode").expect("codemode builtin"),
        ],
    );
    let tools: Vec<(String, Arc<dyn AgentTool>)> = host
        .registry()
        .resolved_tools()
        .into_iter()
        .filter_map(|(def, backend)| match backend {
            pir::plugins::ToolBackend::Rust(tool) => Some((def.name.clone(), tool.clone())),
            pir::plugins::ToolBackend::Js { .. } => None,
        })
        .collect();

    let tool_search = tools
        .iter()
        .find(|(name, _)| name == TOOL_SEARCH_TOOL_NAME)
        .map(|(_, tool)| tool.clone())
        .expect("tool_search tool");
    assert_eq!(
        tool_search.reaches_tools(&json!({ "query": "anything" })),
        Some(ToolReach::All),
        "upstream `reachesTools: () => \"all\"`"
    );

    let codemode = tools
        .iter()
        .find(|(name, _)| name == CODEMODE_TOOL_NAME)
        .map(|(_, tool)| tool.clone())
        .expect("codemode tool");
    match codemode.reaches_tools(&json!({ "code": "await tools.mcp__docs__search({ q: 1 })" })) {
        Some(ToolReach::Names(words)) => assert!(
            words.iter().any(|word| word == "mcp__docs__search"),
            "the script's words are the reach: {words:?}"
        ),
        other => panic!("expected the script's words, got {other:?}"),
    }
    assert_eq!(
        codemode.reaches_tools(&json!({ "code": "const hits = await searchTools('docs')" })),
        Some(ToolReach::All),
        "a discovery global reaches every tool"
    );
    assert_eq!(
        codemode.reaches_tools(&json!({})),
        Some(ToolReach::Names(Vec::new())),
        "a missing `code` reaches nothing (upstream `[]`), but the hook exists"
    );
}

#[test]
fn d23_script_reach_word_scan_matches_upstream() {
    // `code.match(/\w+/g)`: words split at non-word characters, `_` is a word
    // character.
    assert_eq!(
        script_reach(&json!("tools['mcp__docs__' + name]({})")),
        ToolReach::Names(vec![
            "tools".to_owned(),
            "mcp__docs__".to_owned(),
            "name".to_owned(),
        ])
    );
    // The discovery globals are word-boundary matched (`\b(\w+)\b`).
    assert_eq!(
        script_reach(&json!("await describeNamespace('docs')")),
        ToolReach::All
    );
    assert_eq!(script_reach(&json!("await ALL_TOOLS")), ToolReach::All);
    assert_eq!(
        script_reach(&json!("await describeTool('x')")),
        ToolReach::All
    );
    let not_a_global = script_reach(&json!("const mySearchTools = 1"));
    assert!(
        matches!(&not_a_global, ToolReach::Names(words) if !words.is_empty()),
        "a longer identifier is not the discovery global: {not_a_global:?}"
    );
    // Non-string input (a script-less call) reaches nothing.
    assert_eq!(script_reach(&Value::Null), ToolReach::Names(Vec::new()));
}

// ---------------------------------------------------------------------------
// ensureDiscoveryActive
// ---------------------------------------------------------------------------

#[test]
fn d23_mcp_discovery_activation_follows_configured_exposures() {
    let host = PluginHost::build(
        None,
        vec![
            instantiate_builtin("tool-search").expect("tool-search builtin"),
            instantiate_builtin("codemode").expect("codemode builtin"),
        ],
    );
    let mut reaching = ToolRegistry::new();
    merge_plugin_tools(&mut reaching, &host, None);

    let temp = TempDir::new("activation");
    let plugin = McpPlugin::with_cwd(&temp.0, &temp.0);

    // No servers: nothing to activate.
    assert!(plugin
        .discovery_tools_to_activate(&reaching, &[])
        .is_empty());

    // A server whose tools are reached through codemode: codemode activates
    // (the `autoEnableCodemode` default is true).
    register_server(&plugin, "docs", Some(McpExposure::Codemode));
    assert_eq!(
        plugin.discovery_tools_to_activate(&reaching, &[]),
        vec![CODEMODE_TOOL_NAME.to_owned()]
    );
    // Already active: no duplicate.
    assert!(plugin
        .discovery_tools_to_activate(&reaching, &[CODEMODE_TOOL_NAME.to_owned()])
        .is_empty());

    // A `deferred` server activates tool_search instead.
    register_server(&plugin, "deferred-server", Some(McpExposure::Deferred));
    assert_eq!(
        plugin.discovery_tools_to_activate(&reaching, &[]),
        vec![
            CODEMODE_TOOL_NAME.to_owned(),
            TOOL_SEARCH_TOOL_NAME.to_owned()
        ]
    );
    // `direct`/`hidden` exposures ask for nothing: both servers disabled.
    plugin.manager().registry().unregister("docs");
    plugin.manager().registry().unregister("deferred-server");
    register_server(&plugin, "direct-server", Some(McpExposure::Direct));
    assert!(plugin
        .discovery_tools_to_activate(&reaching, &[])
        .is_empty());

    // The gate: no registered tool of the name declares a reach (another
    // extension's tool of the same name is never activated).
    register_server(&plugin, "docs", Some(McpExposure::Codemode));
    register_server(&plugin, "deferred-server", Some(McpExposure::Deferred));
    let mut plain = ToolRegistry::new();
    plain.register(Arc::new(ReachTool {
        name: "codemode",
        reach: None,
    }));
    plain.register(Arc::new(ReachTool {
        name: "tool_search",
        reach: None,
    }));
    let decision = plugin.discovery_activation(&plain, &[]);
    assert!(decision.activate.is_empty());
    assert!(decision.needs_codemode && decision.needs_tool_search);
    assert!(!decision.has_codemode && !decision.has_tool_search);
    // A name-only match with a reach on the OTHER tool activates only that one.
    let mut partial = ToolRegistry::new();
    partial.register(Arc::new(ReachTool {
        name: "codemode",
        reach: Some(ReachKind::Script),
    }));
    assert_eq!(
        plugin.discovery_tools_to_activate(&partial, &[]),
        vec![CODEMODE_TOOL_NAME.to_owned()]
    );
}

#[test]
fn d23_configured_exposures_include_per_tool_overrides() {
    let mut value = json!({ "command": "true", "exposure": "hidden" });
    value["toolExposure"] = json!({ "search": "deferred", "read": "direct" });
    let config: McpServerConfig = serde_json::from_value(value).expect("config");
    let exposures = configured_exposures(&config);
    assert!(exposures.contains(&McpExposure::Hidden));
    assert!(exposures.contains(&McpExposure::Deferred));
    assert!(exposures.contains(&McpExposure::Direct));
    assert!(!exposures.contains(&McpExposure::Codemode));
    // The base exposure defaults to codemode (upstream `exposureOf`).
    let default_config: McpServerConfig =
        serde_json::from_value(json!({ "command": "true" })).expect("config");
    assert_eq!(
        configured_exposures(&default_config),
        vec![McpExposure::Codemode]
    );
}

// ---------------------------------------------------------------------------
// The tool_call wait: which servers a call reaches
// ---------------------------------------------------------------------------

#[test]
fn d23_waiting_servers_follow_reach_resource_and_owner_rules() {
    let temp = TempDir::new("waiting");
    let plugin = McpPlugin::with_cwd(&temp.0, &temp.0);
    register_server(&plugin, "docs", None);
    register_server(&plugin, "other", None);

    // A reaching tool waits for the servers it reaches (upstream the mcp
    // `tool_call` handler's first branch).
    let codemode = registry_with(vec![Arc::new(ReachTool {
        name: CODEMODE_TOOL_NAME,
        reach: Some(ReachKind::Script),
    })]);
    assert_eq!(
        plugin.waiting_servers(
            &codemode,
            CODEMODE_TOOL_NAME,
            &json!({ "code": "await tools.mcp__docs__search({})" })
        ),
        vec!["docs".to_owned()]
    );
    assert_eq!(
        plugin.waiting_servers(
            &codemode,
            CODEMODE_TOOL_NAME,
            &json!({ "code": "await searchTools('anything')" })
        ),
        vec!["docs".to_owned(), "other".to_owned()]
    );

    // A tool that reaches nothing waits for nothing.
    let plain = registry_with(vec![Arc::new(ReachTool {
        name: "plain",
        reach: None,
    })]);
    assert!(plugin
        .waiting_servers(&plain, "plain", &json!({}))
        .is_empty());
    // An unregistered tool name waits for nothing.
    assert!(plugin
        .waiting_servers(&codemode, "missing", &json!({}))
        .is_empty());

    // The other two reach tiers: a fixed word array (a JS hook returning a
    // name list) and `"all"` (tool_search).
    let words = registry_with(vec![Arc::new(ReachTool {
        name: "words",
        reach: Some(ReachKind::Words(vec!["tools.mcp__docs__search".to_owned()])),
    })]);
    assert_eq!(
        plugin.waiting_servers(&words, "words", &json!({})),
        vec!["docs".to_owned()]
    );
    let all = registry_with(vec![Arc::new(ReachTool {
        name: TOOL_SEARCH_TOOL_NAME,
        reach: Some(ReachKind::All),
    })]);
    assert_eq!(
        plugin.waiting_servers(&all, TOOL_SEARCH_TOOL_NAME, &json!({ "query": "x" })),
        vec!["docs".to_owned(), "other".to_owned()]
    );

    // The aggregate resource tools reach every enabled server.
    let resource_name = MCP_RESOURCE_TOOLS[0];
    let mut resources = registry_with(vec![Arc::new(ReachTool {
        name: "list_mcp_resources",
        reach: None,
    })]);
    resources.register(Arc::new(ReachTool {
        name: "list_mcp_resource_templates",
        reach: None,
    }));
    assert_eq!(
        plugin.waiting_servers(&resources, resource_name, &json!({})),
        vec!["docs".to_owned(), "other".to_owned()]
    );

    // A disabled server is never waited for.
    plugin.manager().registry().register(McpServerEntry {
        name: "off".to_owned(),
        config: serde_json::from_value(json!({ "command": "true", "enabled": false }))
            .expect("config"),
        source: McpServerSource::GlobalConfig(PathBuf::from("mcp.json")),
    });
    assert_eq!(
        plugin.waiting_servers(
            &codemode,
            CODEMODE_TOOL_NAME,
            &json!({ "code": "await searchTools('x')" })
        ),
        vec!["docs".to_owned(), "other".to_owned()]
    );
}

#[test]
fn d23_mcp_tool_owner_resolves_the_registered_name_assignment() {
    // The owner map upstream keeps at registration time, recomputed from the
    // discovered snapshot: the deterministic name assignment decides.
    let mut discovered = std::collections::BTreeMap::new();
    discovered.insert(
        "docs".to_owned(),
        vec![
            DiscoveredTool {
                server: "docs".to_owned(),
                tool: McpTool {
                    name: "search".to_owned(),
                    ..Default::default()
                },
                exposure: McpExposure::Codemode,
            },
            DiscoveredTool {
                server: "docs".to_owned(),
                tool: McpTool {
                    name: "read-page".to_owned(),
                    ..Default::default()
                },
                exposure: McpExposure::Codemode,
            },
        ],
    );
    discovered.insert(
        "dev-radius".to_owned(),
        vec![DiscoveredTool {
            server: "dev-radius".to_owned(),
            tool: McpTool {
                name: "search".to_owned(),
                ..Default::default()
            },
            exposure: McpExposure::Deferred,
        }],
    );
    assert_eq!(
        mcp_tool_owner(&discovered, "mcp__docs__search").as_deref(),
        Some("docs")
    );
    assert_eq!(
        mcp_tool_owner(&discovered, "mcp__docs__read_page").as_deref(),
        Some("docs"),
        "sanitized tool names resolve to their server"
    );
    assert_eq!(
        mcp_tool_owner(&discovered, "mcp__dev_radius__search").as_deref(),
        Some("dev-radius"),
        "a hyphenated server name resolves through its sanitized namespace"
    );
    assert_eq!(mcp_tool_owner(&discovered, "mcp__nobody__search"), None);
    assert_eq!(mcp_tool_owner(&discovered, "read"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn d23_wait_for_tool_call_awaits_the_reachable_servers_only() {
    let temp = TempDir::new("wait");
    let plugin = McpPlugin::with_cwd(&temp.0, &temp.0);
    // A server that cannot start: the wait must swallow the failure (upstream
    // `server.ready = ready.catch(() => undefined)`) and still report the
    // servers it waited for.
    plugin.manager().registry().register(McpServerEntry {
        name: "docs".to_owned(),
        config: serde_json::from_value(json!({
            "command": "pir-d23-does-not-exist",
            "args": [],
            "timeout": 1,
        }))
        .expect("config"),
        source: McpServerSource::GlobalConfig(PathBuf::from("mcp.json")),
    });
    let registry = registry_with(vec![Arc::new(ReachTool {
        name: CODEMODE_TOOL_NAME,
        reach: Some(ReachKind::Script),
    })]);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            CODEMODE_TOOL_NAME,
            &json!({ "code": "await tools.mcp__docs__search({})" }),
        )
        .await;
    assert_eq!(waited, vec!["docs".to_owned()]);

    // A call that reaches nothing waits for nothing (and never connects).
    let plain = registry_with(vec![Arc::new(ReachTool {
        name: "plain",
        reach: None,
    })]);
    assert!(plugin
        .wait_for_tool_call(&plain, "plain", &json!({}))
        .await
        .is_empty());

    plugin.manager().shutdown().await;
}

// ---------------------------------------------------------------------------
// The registry surface the MCP consumers read
// ---------------------------------------------------------------------------

#[test]
fn d23_registry_reach_lookup_is_per_call_input() {
    // The reach hook is a function of the call's input (upstream
    // `tool.reachesTools?.(event.input)`), so the same tool can reach every
    // server for one call and one server for the next.
    let registry = registry_with(vec![Arc::new(ReachTool {
        name: CODEMODE_TOOL_NAME,
        reach: Some(ReachKind::Script),
    })]);
    let tool = registry.get(CODEMODE_TOOL_NAME).expect("tool");
    assert_eq!(
        tool.reaches_tools(&json!({ "code": "await searchTools('x')" })),
        Some(ToolReach::All)
    );
    assert!(matches!(
        tool.reaches_tools(&json!({ "code": "await tools.mcp__docs__search({})" })),
        Some(ToolReach::Names(_))
    ));
    let plain = ReachTool {
        name: "plain",
        reach: None,
    };
    assert_eq!(plain.reaches_tools(&json!({})), None);
}

// ---------------------------------------------------------------------------
// The host side: session activation, the loop wait hook, the config field,
// and the aggregate resource tools
//
// Harness notes: the MCP plugin is attached as an inline plugin
// (`AgentSession::new_with_plugins`), so no JS runtime is needed; the agent
// dir is redirected per test through `PIR_AGENT_DIR` (the plugin's global
// `mcp.json` lookup), guarded by `env_lock` against the process-global env.
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

/// Serializes the tests that redirect process-global env vars (cargo runs the
/// tests of one binary in threads). Async so the guard can outlive the awaited
/// shutdowns without holding a `std::sync` guard across an `.await`.
async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Sets env vars for the guard's lifetime, restoring the previous values on
/// drop.
struct EnvVars(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvVars {
    fn set(pairs: &[(&'static str, &str)]) -> Self {
        let mut previous = Vec::new();
        for (key, value) in pairs {
            previous.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self(previous)
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A core (base-registry) tool: the surface the activation must not truncate.
struct CoreTool {
    name: &'static str,
}

impl AgentTool for CoreTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "core test tool"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({"type": "object"}))
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// A provider answering every stream with a final text message and recording
/// the tool names the run asked for (one entry per request).
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

/// A session with `inline` plugins attached, a capturing provider, the given
/// core tools, an optional extension runtime, and an optional host-resolved
/// `--tools` selection.
fn session_with_plugins(
    cwd: &Path,
    captured: &Arc<Mutex<Vec<Vec<String>>>>,
    core: &[&'static str],
    selection: Option<Vec<String>>,
    extensions: Option<ExtensionHandle>,
    inline: Vec<Arc<dyn AgentPlugin>>,
) -> Arc<AgentSession> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CapturingProvider {
        calls: AtomicUsize::new(0),
        captured: Arc::clone(captured),
    }));
    let mut tools = ToolRegistry::new();
    for name in core {
        tools.register(Arc::new(CoreTool { name }));
    }
    Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("base system prompt".into()),
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions,
            tool_selection: selection.map(|enabled| pir::coding_agent::ToolSelection {
                enabled,
                // Explicit host list: no `+name`/`-name` entries, no resume
                // restore (upstream `usesDefaultTools` is false for it).
                ..Default::default()
            }),
        },
        inline,
    ))
}

fn captured_tool_names(captured: &Arc<Mutex<Vec<Vec<String>>>>, index: usize) -> Vec<String> {
    captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(index)
        .cloned()
        .unwrap_or_default()
}

/// Registers a server that cannot start, so a wait fails fast and still
/// proves it reached the manager (`has_connection` is set when the connection
/// object is created, before the transport spawn).
fn register_bogus_server(plugin: &McpPlugin, name: &str) {
    plugin.manager().registry().register(McpServerEntry {
        name: name.to_owned(),
        config: serde_json::from_value(json!({
            "command": "pir-d23-does-not-exist",
            "args": [],
            "timeout": 1,
        }))
        .expect("config"),
        // Extension-sourced: the plugin's `register()` pass unregisters
        // config-sourced entries whose `mcp.json` definition disappeared, and
        // this server is registered directly.
        source: McpServerSource::Extension("d23-test".to_owned()),
    });
}

/// Same, with an exposure: the session-level tests connect (and fail) fast at
/// `session_start` while the configured exposure drives the activation.
fn register_unstartable_server(plugin: &McpPlugin, name: &str, exposure: McpExposure) {
    plugin.manager().registry().register(McpServerEntry {
        name: name.to_owned(),
        config: serde_json::from_value(json!({
            "command": "pir-d23-does-not-exist",
            "args": [],
            "timeout": 1,
            "exposure": serde_json::to_value(exposure).expect("exposure"),
        }))
        .expect("config"),
        // Extension-sourced (see `register_bogus_server`).
        source: McpServerSource::Extension("d23-test".to_owned()),
    });
}

/// The Rust tool objects of a plugin host, by name.
fn rust_tool(host: &PluginHost, name: &str) -> Arc<dyn AgentTool> {
    host.registry()
        .resolved_tools()
        .into_iter()
        .find_map(|(def, backend)| match backend {
            pir::plugins::ToolBackend::Rust(tool) if def.name == name => Some(tool.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} is registered as a Rust tool"))
}

// ---------------------------------------------------------------------------
// (c) `autoEnableCodemode`: the real config field
// ---------------------------------------------------------------------------

#[test]
fn d23_auto_enable_codemode_parses_and_merges_project_over_global() {
    let (servers, auto, errors) = parse_mcp_config_document(
        r#"{ "mcpServers": { "docs": { "command": "x" } }, "autoEnableCodemode": false }"#,
    );
    assert!(servers.contains_key("docs"));
    assert_eq!(auto, Some(false));
    assert!(errors.is_empty(), "{errors:?}");
    // Absent flag: the caller keeps its default.
    let (_, auto, errors) = parse_mcp_config_document(r#"{ "mcpServers": {} }"#);
    assert_eq!(auto, None);
    assert!(errors.is_empty());
    // Wrong type: reported and treated as absent.
    let (_, auto, errors) =
        parse_mcp_config_document(r#"{ "mcpServers": {}, "autoEnableCodemode": "yes" }"#);
    assert_eq!(auto, None);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("autoEnableCodemode must be a boolean")),
        "{errors:?}"
    );
    // The legacy flat shape has no flag slot (`autoEnableCodemode` would be a
    // server entry there).
    let (_, auto, _) = parse_mcp_config_document(r#"{ "docs": { "command": "x" } }"#);
    assert_eq!(auto, None);

    // The per-file loader carries each file's value: the project file (read
    // second) overrides the global one at the consumption site.
    let global = TempDir::new("auto-global");
    let project = TempDir::new("auto-project");
    std::fs::write(
        global.0.join("mcp.json"),
        r#"{ "mcpServers": {}, "autoEnableCodemode": false }"#,
    )
    .expect("write global mcp.json");
    std::fs::create_dir_all(project.0.join(".pi")).expect("mkdir .pi");
    std::fs::write(
        project.0.join(".pi").join("mcp.json"),
        r#"{ "mcpServers": {}, "autoEnableCodemode": true }"#,
    )
    .expect("write project mcp.json");

    let loaded = load_mcp_configs(&global.0, &project.0, true);
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].auto_enable_codemode, Some(false));
    assert_eq!(loaded[1].auto_enable_codemode, Some(true));
    // Untrusted: the project file is not loaded at all.
    let loaded = load_mcp_configs(&global.0, &project.0, false);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].auto_enable_codemode, Some(false));
}

#[tokio::test(flavor = "current_thread")]
async fn d23_auto_enable_codemode_field_gates_the_plugin_activation() {
    let _env = env_lock().await;
    let agent = TempDir::new("auto-plugin-agent");
    let cwd = TempDir::new("auto-plugin-cwd");
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{
            "mcpServers": {
                "docs": { "command": "true", "exposure": "codemode" },
                "deferred-server": { "command": "true", "exposure": "deferred" }
            },
            "autoEnableCodemode": false
        }"#,
    )
    .expect("write mcp.json");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let plugin = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    let host = PluginHost::build(
        None,
        vec![
            plugin.clone() as Arc<dyn AgentPlugin>,
            instantiate_builtin("codemode").expect("codemode builtin"),
            instantiate_builtin("tool-search").expect("tool-search builtin"),
        ],
    );
    let mut reaching = ToolRegistry::new();
    merge_plugin_tools(&mut reaching, &host, None);

    let decision = plugin.discovery_activation(&reaching, &[]);
    assert!(
        !decision.auto_enable_codemode,
        "the mcp.json flag reaches the decision"
    );
    assert!(decision.needs_codemode && decision.needs_tool_search);
    assert_eq!(
        decision.activate,
        vec![TOOL_SEARCH_TOOL_NAME.to_owned()],
        "autoEnableCodemode false suppresses only codemode"
    );
    plugin.manager().shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn d23_auto_enable_codemode_project_value_wins() {
    let _env = env_lock().await;
    let agent = TempDir::new("auto-project-agent");
    let cwd = TempDir::new("auto-project-cwd");
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{ "mcpServers": {}, "autoEnableCodemode": false }"#,
    )
    .expect("write global mcp.json");
    std::fs::create_dir_all(cwd.0.join(".pi")).expect("mkdir .pi");
    std::fs::write(
        cwd.0.join(".pi").join("mcp.json"),
        r#"{
            "mcpServers": { "docs": { "command": "true", "exposure": "codemode" } },
            "autoEnableCodemode": true
        }"#,
    )
    .expect("write project mcp.json");
    let _vars = EnvVars::set(&[
        ("PIR_AGENT_DIR", &agent.0.to_string_lossy()),
        ("PIR_TRUST_PROJECT", "1"),
    ]);

    let plugin = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    let host = PluginHost::build(
        None,
        vec![
            plugin.clone() as Arc<dyn AgentPlugin>,
            instantiate_builtin("codemode").expect("codemode builtin"),
            instantiate_builtin("tool-search").expect("tool-search builtin"),
        ],
    );
    let mut reaching = ToolRegistry::new();
    merge_plugin_tools(&mut reaching, &host, None);

    let decision = plugin.discovery_activation(&reaching, &[]);
    assert!(
        decision.auto_enable_codemode,
        "the trusted project value overrides the global one"
    );
    assert_eq!(decision.activate, vec![CODEMODE_TOOL_NAME.to_owned()]);
    plugin.manager().shutdown().await;
}

// ---------------------------------------------------------------------------
// (e) the aggregate resource tools
// ---------------------------------------------------------------------------

#[test]
fn d23_resource_tools_are_registered_and_hidden_without_resources() {
    let temp = TempDir::new("resource-registration");
    let plugin = Arc::new(McpPlugin::with_cwd(&temp.0, &temp.0));
    let host = PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>]);
    for name in MCP_RESOURCE_TOOLS {
        let tool = rust_tool(&host, name);
        assert_eq!(
            tool.exposure(),
            ToolExposure::Hidden,
            "{name} is registered but unreachable while no server has resources"
        );
        assert_eq!(
            tool.annotations()
                .and_then(|annotations| annotations.read_only_hint),
            Some(true),
            "{name} is read-only (upstream annotations)"
        );
        assert_eq!(
            tool.execution_mode(),
            pir::agent::tool_batch::ToolExecutionMode::Sequential
        );
        // The resource branch of the wait resolves them by NAME, not through a
        // `reachesTools` declaration.
        assert_eq!(tool.reaches_tools(&json!({})), None);
    }
    // The parameters follow upstream: the listing tools take `server` +
    // `cursor`, the read tool requires `server` + `uri`.
    let listing = rust_tool(&host, "list_mcp_resources")
        .parameters()
        .expect("parameters");
    assert!(listing["properties"]["server"].is_object());
    assert!(listing["properties"]["cursor"].is_object());
    let read = rust_tool(&host, "read_mcp_resource")
        .parameters()
        .expect("parameters");
    assert_eq!(read["required"], json!(["server", "uri"]));
}

#[test]
fn d23_resource_tool_exposure_follows_the_widest_server_exposure() {
    let hidden = stdio_config(Some(McpExposure::Hidden));
    let deferred = stdio_config(Some(McpExposure::Deferred));
    let codemode = stdio_config(Some(McpExposure::Codemode));
    let direct = stdio_config(Some(McpExposure::Direct));
    let default = stdio_config(None);
    assert_eq!(resource_exposure(&[]), None);
    assert_eq!(resource_exposure(std::slice::from_ref(&hidden)), None);
    assert_eq!(
        resource_exposure(&[default]),
        Some(McpExposure::Codemode),
        "the base exposure defaults to codemode"
    );
    assert_eq!(
        resource_exposure(&[deferred.clone(), codemode.clone()]),
        Some(McpExposure::Codemode)
    );
    assert_eq!(
        resource_exposure(&[codemode, direct.clone()]),
        Some(McpExposure::Direct)
    );
    assert_eq!(
        resource_exposure(&[deferred, hidden]),
        Some(McpExposure::Deferred)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn d23_resource_tools_report_argument_and_no_resource_errors() {
    let temp = TempDir::new("resource-run");
    let plugin = Arc::new(McpPlugin::with_cwd(&temp.0, &temp.0));
    let host = PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>]);
    let ctx = ToolContext::new(temp.0.clone());

    // No server has resources: the aggregate listing is an empty payload, not
    // an error (upstream lists every server with resources).
    let list = rust_tool(&host, "list_mcp_resources");
    let result = list.run(&json!({}), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .map(|payload| &payload["resources"]),
        Some(&json!([]))
    );
    assert!(
        result.content.contains("\"resources\":[]"),
        "{}",
        result.content
    );

    // `cursor` without `server` is a user error (upstream throws).
    let result = list.run(&json!({ "cursor": "c1" }), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("cursor can only be used"),
        "{}",
        result.content
    );
    // A named server without resources reports the available ones.
    let result = list.run(&json!({ "server": "nope" }), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("has no resources"),
        "{}",
        result.content
    );
    // Non-string arguments are rejected.
    let result = list.run(&json!({ "server": 7 }), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("server must be a string"),
        "{}",
        result.content
    );

    // The read tool mirrors upstream's argument order.
    let read = rust_tool(&host, "read_mcp_resource");
    let result = read.run(&json!({}), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("server must be provided"),
        "{}",
        result.content
    );
    let result = read.run(&json!({ "server": "nope" }), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("uri must be provided"),
        "{}",
        result.content
    );

    // The templates tool lists through the client's
    // `resources/templates/list`; with no server connected the listing is an
    // empty payload, exactly like the resources tool above (D23 tail: the
    // client method now exists, so the old explicit "no such capability"
    // error is gone).
    let templates = rust_tool(&host, "list_mcp_resource_templates");
    let result = templates.run(&json!({}), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .map(|payload| &payload["resourceTemplates"]),
        Some(&json!([]))
    );
    assert!(
        result.content.contains("\"resourceTemplates\":[]"),
        "{}",
        result.content
    );
    // Its argument errors mirror the resources tool.
    let result = templates.run(&json!({ "cursor": "c1" }), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("cursor can only be used"),
        "{}",
        result.content
    );
    let result = templates.run(&json!({ "server": 7 }), &ctx).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("server must be a string"),
        "{}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// (a) the session applies the activation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn d23_session_activation_keeps_core_tools_and_the_explicit_selection() {
    let _env = env_lock().await;
    let agent = TempDir::new("session-activation-agent");
    let cwd = TempDir::new("session-activation-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let mcp = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    register_unstartable_server(&mcp, "docs", McpExposure::Codemode);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let session = session_with_plugins(
        &cwd.0,
        &captured,
        &["noop", "second"],
        // `--tools noop,second`: the explicit selection must survive the
        // activation instead of being shadowed by it.
        Some(vec!["noop".to_owned(), "second".to_owned()]),
        None,
        vec![
            mcp.clone() as Arc<dyn AgentPlugin>,
            instantiate_builtin("codemode").expect("codemode builtin"),
            instantiate_builtin("tool-search").expect("tool-search builtin"),
        ],
    );
    session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let tools = captured_tool_names(&captured, 0);
    assert!(tools.contains(&"noop".to_owned()), "{tools:?}");
    assert!(
        tools.contains(&"second".to_owned()),
        "the explicit --tools selection is not shadowed: {tools:?}"
    );
    assert!(
        tools.contains(&CODEMODE_TOOL_NAME.to_owned()),
        "the codemode server activated the discovery tool: {tools:?}"
    );
    assert!(
        !tools.contains(&TOOL_SEARCH_TOOL_NAME.to_owned()),
        "no deferred server: tool_search stays inactive: {tools:?}"
    );
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn d23_session_activation_is_a_noop_without_mcp_servers() {
    let _env = env_lock().await;
    let agent = TempDir::new("session-noop-agent");
    let cwd = TempDir::new("session-noop-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let mcp = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let session = session_with_plugins(
        &cwd.0,
        &captured,
        &["noop"],
        Some(vec!["noop".to_owned()]),
        None,
        vec![
            mcp.clone() as Arc<dyn AgentPlugin>,
            instantiate_builtin("codemode").expect("codemode builtin"),
        ],
    );
    session
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");
    let tools = captured_tool_names(&captured, 0);
    assert_eq!(
        tools,
        vec!["noop".to_owned()],
        "no server asks for a discovery tool: the selection is untouched"
    );
    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// (b) the tool_call wait hook
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn d23_loop_awaits_the_wait_hook_before_executing_the_batch() {
    let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let waited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let executed_for_tool = Arc::clone(&executed);
    let waited_for_hook = Arc::clone(&waited);
    let executed_for_hook = Arc::clone(&executed);

    /// Records its own execution, so the hook can assert it ran first.
    struct RecordingTool {
        executed: Arc<Mutex<Vec<String>>>,
    }

    impl AgentTool for RecordingTool {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn description(&self) -> &'static str {
            "records its execution"
        }
        fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
            let executed = Arc::clone(&self.executed);
            Box::pin(async move {
                executed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("recorder".to_owned());
                ToolResult::ok("recorded")
            })
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(RecordingTool {
        executed: executed_for_tool,
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_stream = Arc::clone(&calls);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            let index = calls_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if index == 0 {
                    AssistantMessage {
                        api: "fake".into(),
                        provider: "fake".into(),
                        model: "fake-model".into(),
                        content: vec![ContentBlock::ToolCall(pir::ai::types::ToolCall {
                            id: "call-1".into(),
                            name: "recorder".into(),
                            arguments: std::collections::BTreeMap::new(),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        })],
                        stop_reason: StopReason::ToolUse,
                        timestamp: 1,
                        usage: Usage::default(),
                        ..AssistantMessage::default()
                    }
                } else {
                    AssistantMessage {
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
                    }
                }
            })
        }),
        tools: Arc::new(tools),
        max_turns: 4,
        tool_call_wait: Some(Arc::new(
            move |registry: Arc<ToolRegistry>, calls: Vec<AgentToolCall>| {
                let waited = Arc::clone(&waited_for_hook);
                let executed = Arc::clone(&executed_for_hook);
                Box::pin(async move {
                    assert!(
                        executed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_empty(),
                        "the wait must run before the tool executes"
                    );
                    for call in calls {
                        assert!(
                            registry.get(&call.tool_name).is_some(),
                            "the hook receives the run's registry"
                        );
                        waited
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(call.tool_name.clone());
                    }
                })
            },
        )),
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 2, "tool turn + final turn");
    assert_eq!(
        waited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        ["recorder".to_owned()],
        "the hook saw the batch's call"
    );
    assert_eq!(
        executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        ["recorder".to_owned()],
        "the batch still executed"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn d23_session_wires_the_wait_hook_to_the_mcp_plugin() {
    let _env = env_lock().await;
    let agent = TempDir::new("session-wait-agent");
    let cwd = TempDir::new("session-wait-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let mcp = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    register_bogus_server(&mcp, "docs");
    register_bogus_server(&mcp, "other");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let session = session_with_plugins(
        &cwd.0,
        &captured,
        &["noop"],
        None,
        None,
        vec![mcp.clone() as Arc<dyn AgentPlugin>],
    );

    let hook = session
        .tool_call_wait_hook()
        .expect("the hook is wired when the mcp builtin is loaded");
    // No session_start ran yet: nothing connected.
    assert!(!mcp.manager().has_connection("docs").await);
    let registry = Arc::new(registry_with(vec![Arc::new(ReachTool {
        name: "reacher",
        reach: Some(ReachKind::Words(vec!["mcp__docs".to_owned()])),
    })]));
    hook(
        Arc::clone(&registry),
        vec![AgentToolCall::new(0, "call-1", "reacher", json!({}))],
    )
    .await;
    assert!(
        mcp.manager().has_connection("docs").await,
        "the wait connected (or attempted) the reached server"
    );
    assert!(
        !mcp.manager().has_connection("other").await,
        "a server the reach does not match is never waited for"
    );

    // A session without the mcp builtin has no hook at all.
    let no_mcp = session_with_plugins(
        &cwd.0,
        &Arc::new(Mutex::new(Vec::new())),
        &["noop"],
        None,
        None,
        vec![instantiate_builtin("codemode").expect("codemode builtin")],
    );
    assert!(no_mcp.tool_call_wait_hook().is_none());
    session.shutdown().await;
}

/// The session's wait hook + a JS extension tool: the reach snapshot travels
/// from QuickJS into the run registry, the loop's hook waits for the server the
/// JS tool names, and the server's connection exists before the tool runs.
#[tokio::test(flavor = "current_thread")]
async fn d23_session_wait_hook_waits_for_a_js_tools_server() {
    let _env = env_lock().await;
    let agent = TempDir::new("js-session-agent");
    let cwd = TempDir::new("js-session-cwd");
    let ext_dir = TempDir::new("js-session-ext");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);
    let runtime = load_runtime(&ext_dir, "js-session", JS_REACH_EXTENSION).await;

    let mcp = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    register_bogus_server(&mcp, "docs");
    register_bogus_server(&mcp, "other");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let session = session_with_plugins(
        &cwd.0,
        &captured,
        &["noop"],
        None,
        Some(runtime.clone()),
        vec![mcp.clone() as Arc<dyn AgentPlugin>],
    );

    let hook = session
        .tool_call_wait_hook()
        .expect("the hook is wired when the mcp builtin is loaded");
    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &runtime);
    assert!(!mcp.manager().has_connection("docs").await);
    hook(
        Arc::new(registry),
        vec![AgentToolCall::new(0, "call-1", "docs_proxy", json!({}))],
    )
    .await;
    assert!(
        mcp.manager().has_connection("docs").await,
        "the JS tool's reach reached the MCP wait"
    );
    assert!(
        !mcp.manager().has_connection("other").await,
        "a server the JS reach does not name is never waited for"
    );
    session.shutdown().await;
}
