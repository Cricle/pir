//! D23 print tail: print mode's `LoopDeps` carry the MCP reach wait hook.
//!
//! The interactive path wires `LoopDeps::tool_call_wait` through
//! `AgentSession::tool_call_wait_hook` (agent_session/mcp_reach.rs); print
//! mode drives `agent_loop` itself, so this pins that
//! [`pir::coding_agent::print::print_loop_deps`] hands the loop the same hook
//! — sourced from the unified plugin host
//! ([`print_plugin_host`](pir::coding_agent::print::print_plugin_host) +
//! [`print_tool_call_wait`](pir::coding_agent::print::print_tool_call_wait))
//! — and that a tool batch's calls reach the MCP plugin's
//! `wait_for_tool_call` before the tool executes.
//!
//! D23 (f): print's registry is core + JS-extension tools, so the reaching
//! tool in the end-to-end cases is a REAL JS extension tool whose
//! `reachesTools` hook lives in QuickJS — its reach crosses the boundary
//! (registry-export snapshot + live per-call probe) and the hook waits for the
//! server it names before the batch runs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

use pir::agent::agent_loop::{run_with_options, LoopOptions};
use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{
    merge_extension_tools, AgentTool, BoxFuture, ToolContext, ToolReach, ToolRegistry,
};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, StopReason, TextContent, ThinkingLevel, ToolCall,
    Usage,
};
use pir::coding_agent::print::{print_loop_deps, print_plugin_host, print_tool_call_wait};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::mcp::manager::McpManager;
use pir::mcp::registry::{McpServerEntry, McpServerSource};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-print-wait-{label}-{}-{}",
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

/// Sets `PIR_AGENT_DIR` for the guard's lifetime, restoring the previous value
/// on drop (the MCP plugin's config pass reads the agent dir).
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

/// Env-var-dependent tests share one process; serialize them.
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Registers a server that cannot start, so a wait fails fast and still
/// proves it reached the manager (`has_connection` is set when the connection
/// object is created, before the transport spawn).
fn register_bogus_server(manager: &McpManager, name: &str) {
    manager.registry().register(McpServerEntry {
        name: name.to_owned(),
        config: serde_json::from_value(json!({
            "command": "pir-print-wait-does-not-exist",
            "args": [],
            "timeout": 1,
        }))
        .expect("config"),
        // Extension-sourced: the plugin's `register()` pass unregisters
        // config-sourced entries whose `mcp.json` definition disappeared, and
        // this server is registered directly.
        source: McpServerSource::Extension("print-wait-test".to_owned()),
    });
}

// ---------------------------------------------------------------------------
// A JS extension tool's reach (D23 (f))
// ---------------------------------------------------------------------------

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("print-wait-js".to_string()),
        system_prompt: "base system prompt".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// A JS extension with two reaching tools — the `reachesTools` hooks stay in
/// QuickJS: `docs_proxy` names the `docs` server through its namespace alias,
/// `js_router` derives the server from the call's input — and both record
/// their execution, so the tests can prove the tool ran after the wait.
const JS_REACH_EXTENSION: &str = r#"
export default function (pi) {
  globalThis.__printWaitExecuted = [];
  pi.registerTool({
    name: "docs_proxy",
    description: "Proxies the docs MCP server.",
    parameters: { type: "object", properties: {} },
    namespace: { name: "mcp__docs", aliases: ["docs"] },
    reachesTools: () => ["docs"],
    async execute() {
      globalThis.__printWaitExecuted.push("docs_proxy");
      return { content: [{ type: "text", text: "proxied" }] };
    },
  });
  pi.registerTool({
    name: "js_router",
    description: "Routes the call to the server its input names.",
    parameters: { type: "object", properties: { target: { type: "string" } } },
    reachesTools: (input) => (typeof input.target === "string" ? [input.target] : []),
    async execute() {
      globalThis.__printWaitExecuted.push("js_router");
      return { content: [{ type: "text", text: "routed" }] };
    },
  });
}
"#;

/// Start a runtime with [`JS_REACH_EXTENSION`] loaded and its path plan
/// recorded the way print mode records it (`builtin:mcp` + the extension
/// path), so [`print_plugin_host`] builds the production host shape: the mcp
/// builtin plus the JS adapter over this handle.
async fn js_print_runtime(cwd: &Path) -> ExtensionHandle {
    let path = cwd.join("ext.ts");
    std::fs::write(&path, JS_REACH_EXTENSION).expect("write fixture");
    let runtime = ExtensionHandle::start(test_config_at(cwd)).expect("runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    runtime.set_path_plan(vec![PathBuf::from("builtin:mcp"), path]);
    runtime
}

/// A tool that declares it may reach the `mcp__docs` namespace and records,
/// when it runs, whether the server's connection object already exists (the
/// wait's observable effect).
struct ReachingRecorder {
    plugin: Arc<McpPlugin>,
    executed: Arc<Mutex<Vec<String>>>,
    waited_before_execution: Arc<AtomicBool>,
}

impl AgentTool for ReachingRecorder {
    fn name(&self) -> &'static str {
        "reacher"
    }

    fn description(&self) -> &'static str {
        "declares a reach and records when it executes"
    }

    fn reaches_tools(&self, _input: &Value) -> Option<ToolReach> {
        Some(ToolReach::Names(vec!["mcp__docs".to_owned()]))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let plugin = Arc::clone(&self.plugin);
        let executed = Arc::clone(&self.executed);
        let waited = Arc::clone(&self.waited_before_execution);
        Box::pin(async move {
            if plugin.manager().has_connection("docs").await {
                waited.store(true, Ordering::SeqCst);
            }
            executed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("reacher".to_owned());
            ToolResult::ok("recorded")
        })
    }
}

fn registry_with(tool: Arc<dyn AgentTool>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    registry
}

/// A two-message script: one `tool_name` tool call, then a plain stop, so the
/// loop executes exactly one batch. Assigned to `LoopDeps::stream` (the field
/// is public; `print_loop_deps` leaves the provider to `with_provider_*`).
fn scripted_stream(
    tool_name: &'static str,
    calls: Arc<AtomicUsize>,
) -> Box<
    dyn Fn(
            pir::agent::agent_loop::ProviderRequest,
        ) -> pir::agent::tool_execution::BoxFuture<'static, AssistantMessage>
        + Send
        + Sync,
> {
    Box::new(move |_request| {
        let index = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if index == 0 {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: tool_name.into(),
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
    })
}

// ---------------------------------------------------------------------------
// The hook source
// ---------------------------------------------------------------------------

#[test]
fn print_tool_call_wait_needs_the_mcp_builtin() {
    // No plugin host at all (an extension-free plan).
    assert!(print_tool_call_wait(None).is_none());
    // A host without the `mcp` builtin (the `--no-mcp` shape).
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    assert!(
        print_tool_call_wait(Some(&host)).is_none(),
        "no mcp builtin: no hook"
    );
    // `print_plugin_host` mirrors the session's construction: no runtime, no
    // host.
    assert!(print_plugin_host(None).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn print_tool_call_wait_awaits_the_mcp_plugin() {
    let _env = env_lock().lock().await;
    let agent = TempDir::new("hook-agent");
    let cwd = TempDir::new("hook-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let mcp = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    register_bogus_server(mcp.manager(), "docs");
    let host = Arc::new(PluginHost::build(
        None,
        vec![Arc::clone(&mcp) as Arc<dyn AgentPlugin>],
    ));
    let hook = print_tool_call_wait(Some(&host)).expect("the mcp builtin wires the hook");
    // Nothing connected yet.
    assert!(!mcp.manager().has_connection("docs").await);

    let registry = Arc::new(registry_with(Arc::new(ReachingRecorder {
        plugin: Arc::clone(&mcp),
        executed: Arc::new(Mutex::new(Vec::new())),
        waited_before_execution: Arc::new(AtomicBool::new(false)),
    })));
    hook(
        Arc::clone(&registry),
        vec![AgentToolCall::new(0, "call-1", "reacher", json!({}))],
    )
    .await;
    assert!(
        mcp.manager().has_connection("docs").await,
        "the hook awaited the reachable server"
    );
}

/// D23 (f), end to end: the reaching tools in print's registry are REAL JS
/// extension tools whose `reachesTools` hooks live in QuickJS. The reach
/// crosses into the Rust side (the registry-export snapshot), the hook waits
/// for the servers the hook names — the static name for one tool, the call's
/// input for the other — and only those servers' connections are created.
#[tokio::test(flavor = "current_thread")]
async fn print_tool_call_wait_awaits_a_js_extension_tools_server() {
    let _env = env_lock().lock().await;
    let agent = TempDir::new("js-hook-agent");
    let cwd = TempDir::new("js-hook-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let runtime = js_print_runtime(&cwd.0).await;
    let host = print_plugin_host(Some(&runtime)).expect("the runtime builds the print host");
    let manager = host
        .mcp_manager()
        .expect("the path plan loads the mcp builtin")
        .clone();
    register_bogus_server(&manager, "docs");
    register_bogus_server(&manager, "other");
    let hook = print_tool_call_wait(Some(&host)).expect("the mcp builtin wires the hook");

    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &runtime);
    let registry = Arc::new(registry);
    assert!(!manager.has_connection("docs").await);

    // `docs_proxy`'s hook returns `["docs"]`: the docs server is awaited ...
    hook(
        Arc::clone(&registry),
        vec![AgentToolCall::new(0, "call-1", "docs_proxy", json!({}))],
    )
    .await;
    assert!(
        manager.has_connection("docs").await,
        "the JS tool's reach reached the MCP plugin through the print host"
    );
    assert!(
        !manager.has_connection("other").await,
        "a server the JS reach does not name is never waited for"
    );

    // ... and `js_router`'s hook reads the CALL's input: the same registry
    // reaches the other server for another call (upstream
    // `tool.reachesTools?.(event.input)`).
    hook(
        Arc::clone(&registry),
        vec![AgentToolCall::new(
            0,
            "call-2",
            "js_router",
            json!({ "target": "other" }),
        )],
    )
    .await;
    assert!(manager.has_connection("other").await);
}

// ---------------------------------------------------------------------------
// The print loop deps
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn print_loop_deps_run_batches_through_the_wait_hook() {
    let _env = env_lock().lock().await;
    let agent = TempDir::new("loop-agent");
    let cwd = TempDir::new("loop-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let mcp = Arc::new(McpPlugin::with_cwd(&agent.0, &cwd.0));
    register_bogus_server(mcp.manager(), "docs");
    let host = Arc::new(PluginHost::build(
        None,
        vec![Arc::clone(&mcp) as Arc<dyn AgentPlugin>],
    ));
    let hook = print_tool_call_wait(Some(&host)).expect("the mcp builtin wires the hook");

    let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let waited_before_execution = Arc::new(AtomicBool::new(false));
    let tool: Arc<dyn AgentTool> = Arc::new(ReachingRecorder {
        plugin: Arc::clone(&mcp),
        executed: Arc::clone(&executed),
        waited_before_execution: Arc::clone(&waited_before_execution),
    });

    let mut deps = print_loop_deps(
        Arc::new(registry_with(tool)),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        cwd.0.clone(),
        None,
        None,
        Some(hook),
        // This pin is about the reach wait seam, not reasoning.
        ThinkingLevel::Off,
    );
    deps.stream = scripted_stream("reacher", Arc::new(AtomicUsize::new(0)));

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 2, "tool turn + final turn");
    assert_eq!(
        executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        ["reacher".to_owned()],
        "the batch executed"
    );
    assert!(
        waited_before_execution.load(Ordering::SeqCst),
        "the wait hook ran before the tool executed"
    );
    assert!(
        mcp.manager().has_connection("docs").await,
        "the batch's reach reached the MCP plugin through the print deps"
    );
}

/// The same loop path with a REAL JS extension tool in the run registry: the
/// tool's reach crosses from QuickJS, the loop's wait hook connects the server
/// it names before the batch executes, and the JS tool then runs (its `execute`
/// records the call).
#[tokio::test(flavor = "current_thread")]
async fn print_loop_deps_run_js_batches_through_the_wait_hook() {
    let _env = env_lock().lock().await;
    let agent = TempDir::new("js-loop-agent");
    let cwd = TempDir::new("js-loop-cwd");
    let _vars = EnvVars::set(&[("PIR_AGENT_DIR", &agent.0.to_string_lossy())]);

    let runtime = js_print_runtime(&cwd.0).await;
    let host = print_plugin_host(Some(&runtime)).expect("the runtime builds the print host");
    let manager = host
        .mcp_manager()
        .expect("the path plan loads the mcp builtin")
        .clone();
    register_bogus_server(&manager, "docs");
    register_bogus_server(&manager, "other");
    let hook = print_tool_call_wait(Some(&host)).expect("the mcp builtin wires the hook");

    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &runtime);
    let mut deps = print_loop_deps(
        Arc::new(registry),
        ToolScheduler::new(ToolSchedulerConfig::default()),
        cwd.0.clone(),
        None,
        None,
        Some(hook),
        ThinkingLevel::Off,
    );
    deps.stream = scripted_stream("docs_proxy", Arc::new(AtomicUsize::new(0)));

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;
    assert_eq!(result.turns, 2, "tool turn + final turn");
    assert!(
        manager.has_connection("docs").await,
        "the JS tool's reach reached the MCP plugin through the print deps"
    );
    assert!(!manager.has_connection("other").await);
    let executed = runtime
        .eval_json("JSON.stringify(globalThis.__printWaitExecuted ?? [])")
        .await
        .expect("probe the JS tool's execution record");
    assert_eq!(
        executed,
        json!(["docs_proxy"]),
        "the JS tool executed after the wait"
    );
}
