//! End-to-end `builtinTools` / `toolExposure` tests against a REAL stdio MCP
//! server.
//!
//! `tests/mcp_builtin_tools.rs` covers the *activation decision* (`command:
//! "true"` never connects). These tests run the same public path the session
//! does — `mcp.json` -> `McpPlugin::register` -> `session_start` discovery
//! through the real rmcp stdio transport -> registry rebuild -> model-visible
//! tool definitions / tool call — and assert that the exposure resolved at
//! registration is the one the model actually sees.
//!
//! The mock server is this very test binary re-invoked with a dedicated entry
//! test (`mock_mcp_server_child`), guarded by `PIR_BUILTIN_E2E_MCP_SERVER=1`,
//! the same pattern as `tests/mcp_integration.rs::mock_mcp_server_child`.

#![cfg(feature = "mcp")]

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_execution::{
    callable_tools, declared_tools, AgentTool, ToolExposure, ToolRegistry,
};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost, ToolBackend};

// ---------------------------------------------------------------------------
// Env serialization + temp dirs (copied from tests/mcp_builtin_tools.rs)
// ---------------------------------------------------------------------------

/// `PIR_AGENT_DIR` is process-wide; the MCP plugin reads it in `register()`.
/// Every env-mutating test in this binary serializes on this lock.
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
            "pir-mcp-builtin-e2e-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// The mock stdio MCP server (child entry of this very test binary)
// ---------------------------------------------------------------------------

const CHILD_GUARD: &str = "PIR_BUILTIN_E2E_MCP_SERVER";

fn mock_server_spawn() -> (String, Vec<String>) {
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "mock_mcp_server_child".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

fn write_mock_line(reply: &Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

/// Line-delimited JSON-RPC over stdin/stdout. Non-JSON lines (the libtest
/// banner the child prints on stdout) are skipped.
fn serve_mock_requests() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").cloned() else {
            continue; // notification
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "builtin-e2e-mock", "version": "1.0.0" },
            }),
            "ping" => json!({}),
            "tools/list" => json!({
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo the input text",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "text": { "type": "string" } },
                            "required": ["text"],
                        },
                    },
                    {
                        "name": "other",
                        "description": "The second mock tool",
                        "inputSchema": { "type": "object", "properties": {} },
                    },
                ],
            }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if name == "other" {
                    json!({ "content": [{ "type": "text", "text": "other:ok" }] })
                } else {
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "echo:{}",
                                args.get("text").and_then(Value::as_str).unwrap_or("")
                            ),
                        }],
                    })
                }
            }
            _ => {
                write_mock_line(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" },
                }));
                continue;
            }
        };
        write_mock_line(&json!({ "jsonrpc": "2.0", "id": id, "result": reply }));
    }
}

/// With `PIR_BUILTIN_E2E_MCP_SERVER=1` this test IS the MCP server (blocking
/// JSON-RPC loop); otherwise it is an ordinary no-op test.
#[test]
fn mock_mcp_server_child() {
    if std::env::var(CHILD_GUARD).as_deref() != Ok("1") {
        return;
    }
    serve_mock_requests();
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A real connection to the mock server. `env` is the child's environment.
fn server_config() -> Value {
    let (command, args) = mock_server_spawn();
    json!({
        "command": command,
        "args": args,
        "env": { CHILD_GUARD: "1" },
        "timeout": 20,
    })
}

fn write_mcp_json(agent_dir: &Path, document: Value) {
    std::fs::write(agent_dir.join("mcp.json"), document.to_string()).expect("write mcp.json");
}

fn build_host(agent_dir: &Path, cwd: &Path) -> (Arc<McpPlugin>, PluginHost) {
    let plugin = Arc::new(McpPlugin::with_cwd(agent_dir, cwd));
    let host = PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>]);
    (plugin, host)
}

/// Build the host, run the session-start discovery (the real connect), and
/// return the plugin + host with the registry already rebuilt.
async fn start_session(agent_dir: &Path, cwd: &Path) -> (Arc<McpPlugin>, PluginHost) {
    let (plugin, host) = build_host(agent_dir, cwd);
    let mut payload = json!({ "type": "session_start" });
    host.emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    (plugin, host)
}

/// The registered Rust tool behind one registry name (the actual
/// [`AgentTool`], whose `exposure()` the scheduler reads).
fn registered_tool(host: &PluginHost, name: &str) -> Arc<dyn AgentTool> {
    host.registry()
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == name)
        .and_then(|(_, backend)| match backend {
            ToolBackend::Rust(tool) => Some(Arc::clone(tool)),
            ToolBackend::Js { .. } => None,
        })
        .unwrap_or_else(|| panic!("tool `{name}` is not a registered Rust tool"))
}

/// The exposure the session's scheduler observes for `name`.
fn exposure_of(host: &PluginHost, name: &str) -> ToolExposure {
    registered_tool(host, name).exposure()
}

/// The tool definitions actually declared to the model, computed exactly like
/// the session does: the registry's default-active set (`active_names(None)`,
/// i.e. no `--tools` override) filtered through `declared_tools` (hidden tools
/// never join a session registry).
fn model_visible_tools(host: &PluginHost) -> Vec<String> {
    let mut registry = ToolRegistry::new();
    for (_, backend) in host.registry().resolved_tools() {
        if let ToolBackend::Rust(tool) = backend {
            registry.register(Arc::clone(tool));
        }
    }
    let active = registry.active_names(None);
    declared_tools(&registry, &active)
        .into_iter()
        .map(|tool| tool.name().to_string())
        .collect()
}

/// The tools callable from other tools (`ctx.executeTool()`): active direct
/// tools plus every registered codemode/deferred tool.
fn callable_tool_names(host: &PluginHost) -> Vec<String> {
    let mut registry = ToolRegistry::new();
    for (_, backend) in host.registry().resolved_tools() {
        if let ToolBackend::Rust(tool) = backend {
            registry.register(Arc::clone(tool));
        }
    }
    let active = registry.active_names(None);
    callable_tools(&registry, &active)
        .into_iter()
        .map(|tool| tool.name().to_string())
        .collect()
}

fn registry_names(host: &PluginHost) -> Vec<String> {
    host.registry()
        .resolved_tools()
        .into_iter()
        .map(|(def, _)| def.name.clone())
        .collect()
}

fn exposure_label(host: &PluginHost, name: &str) -> String {
    host.tool_defs()
        .into_iter()
        .find(|def| def.name == name)
        .map(|def| def.exposure)
        .unwrap_or_else(|| "<missing>".to_owned())
}

fn called_text(result: &pir::extensions::ToolResult) -> String {
    result
        .content
        .iter()
        .map(|item| item.text.clone())
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// Scenario 1: no `exposure`, no `builtinTools` -> direct, model-visible,
// really callable through the real stdio server.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn default_direct_connects_and_declares_the_tool_to_the_model() {
    let _guard = env_guard().await;
    let agent = TempDir::new("default-direct");
    let cwd = TempDir::new("default-direct-cwd");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);
    write_mcp_json(
        &agent.0,
        json!({ "mcpServers": { "mock": server_config() } }),
    );

    let (plugin, host) = start_session(&agent.0, &cwd.0).await;
    assert!(plugin.builtin_tools(), "the flag defaults to true");

    // The real connection was made and the tools joined the registry.
    let states = plugin.manager().connection_states().await;
    assert_eq!(
        states.get("mock"),
        Some(&pir::mcp::manager::ConnectionState::Connected),
        "the mock server is really connected: {states:?}"
    );
    let names = registry_names(&host);
    assert!(
        names.contains(&"mcp__mock__echo".to_owned()),
        "registry names: {names:?}"
    );

    // The scheduler-visible exposure is Direct (both on the AgentTool and in
    // the registry definition).
    assert_eq!(exposure_of(&host, "mcp__mock__echo"), ToolExposure::Direct);
    assert_eq!(exposure_label(&host, "mcp__mock__echo"), "direct");

    // ... and the model-visible declaration list contains it.
    let visible = model_visible_tools(&host);
    assert!(
        visible.contains(&"mcp__mock__echo".to_owned()),
        "model-visible tools: {visible:?}"
    );

    // The tool call round-trips through the real child process.
    let result = host
        .call_tool("mcp__mock__echo", "e2e-1", &json!({ "text": "hello" }))
        .await
        .expect("call_tool");
    assert_eq!(called_text(&result), "echo:hello");

    plugin.manager().shutdown().await;
}

// ---------------------------------------------------------------------------
// Scenario 2: `builtinTools: false` -> upstream's codemode default.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn builtin_tools_false_hides_the_tool_from_the_direct_list() {
    let _guard = env_guard().await;
    let agent = TempDir::new("builtin-false");
    let cwd = TempDir::new("builtin-false-cwd");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);
    write_mcp_json(
        &agent.0,
        json!({
            "mcpServers": { "mock": server_config() },
            "builtinTools": false,
        }),
    );

    let (plugin, host) = start_session(&agent.0, &cwd.0).await;
    assert!(!plugin.builtin_tools(), "the flag is read as false");

    let names = registry_names(&host);
    assert!(
        names.contains(&"mcp__mock__echo".to_owned()),
        "the tool is registered regardless of exposure: {names:?}"
    );

    // `McpExposure::Codemode` maps to `ToolExposure::Deferred` at the agent
    // boundary (src/plugins/builtin/mcp.rs `tool_exposure`): the tool is not
    // declared to the model, but stays reachable from codemode/tool search.
    assert_eq!(
        exposure_of(&host, "mcp__mock__echo"),
        ToolExposure::Deferred,
        "codemode exposure is never declared by default"
    );
    assert_eq!(exposure_label(&host, "mcp__mock__echo"), "deferred");

    let visible = model_visible_tools(&host);
    assert!(
        !visible.contains(&"mcp__mock__echo".to_owned()),
        "a codemode tool must not be in the direct list: {visible:?}"
    );
    let callable = callable_tool_names(&host);
    assert!(
        callable.contains(&"mcp__mock__echo".to_owned()),
        "a codemode tool stays reachable through executeTool: {callable:?}"
    );

    // Reachable means really callable against the live server.
    let result = host
        .call_tool(
            "mcp__mock__echo",
            "e2e-2",
            &json!({ "text": "via-codemode" }),
        )
        .await
        .expect("codemode tool call");
    assert_eq!(called_text(&result), "echo:via-codemode");

    plugin.manager().shutdown().await;
}

// ---------------------------------------------------------------------------
// Scenario 3: explicit `exposure: "hidden"` beats `builtinTools: true`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn explicit_hidden_stays_hidden_under_builtin_tools() {
    let _guard = env_guard().await;
    let agent = TempDir::new("explicit-hidden");
    let cwd = TempDir::new("explicit-hidden-cwd");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);
    let mut config = server_config();
    config["exposure"] = json!("hidden");
    write_mcp_json(
        &agent.0,
        json!({
            "mcpServers": { "mock": config },
            "builtinTools": true,
        }),
    );

    let (plugin, host) = start_session(&agent.0, &cwd.0).await;
    assert!(plugin.builtin_tools());

    let names = registry_names(&host);
    assert!(
        names.contains(&"mcp__mock__echo".to_owned()),
        "the hidden tool is still registered: {names:?}"
    );
    assert_eq!(
        exposure_of(&host, "mcp__mock__echo"),
        ToolExposure::Hidden,
        "an explicit exposure wins over builtinTools"
    );
    assert_eq!(exposure_label(&host, "mcp__mock__echo"), "hidden");

    let visible = model_visible_tools(&host);
    assert!(
        !visible.contains(&"mcp__mock__echo".to_owned()),
        "hidden tools are never declared: {visible:?}"
    );
    let callable = callable_tool_names(&host);
    assert!(
        !callable.contains(&"mcp__mock__echo".to_owned()),
        "hidden tools are not callable either: {callable:?}"
    );

    plugin.manager().shutdown().await;
}

// ---------------------------------------------------------------------------
// Scenario 4: per-tool `toolExposure` overrides the direct base default.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn tool_exposure_override_hides_one_tool_and_keeps_the_other_direct() {
    let _guard = env_guard().await;
    let agent = TempDir::new("tool-override");
    let cwd = TempDir::new("tool-override-cwd");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);
    let mut config = server_config();
    config["toolExposure"] = json!({ "echo": "hidden" });
    write_mcp_json(
        &agent.0,
        json!({
            "mcpServers": { "mock": config },
            "builtinTools": true,
        }),
    );

    let (plugin, host) = start_session(&agent.0, &cwd.0).await;

    assert_eq!(
        exposure_of(&host, "mcp__mock__echo"),
        ToolExposure::Hidden,
        "the per-tool override wins over the direct base"
    );
    assert_eq!(
        exposure_of(&host, "mcp__mock__other"),
        ToolExposure::Direct,
        "a tool with no override follows the direct base"
    );
    assert_eq!(exposure_label(&host, "mcp__mock__echo"), "hidden");
    assert_eq!(exposure_label(&host, "mcp__mock__other"), "direct");

    let visible = model_visible_tools(&host);
    assert!(
        !visible.contains(&"mcp__mock__echo".to_owned()),
        "the overridden tool is not declared: {visible:?}"
    );
    assert!(
        visible.contains(&"mcp__mock__other".to_owned()),
        "the other tool stays declared: {visible:?}"
    );

    // The direct sibling still answers through the real server.
    let result = host
        .call_tool("mcp__mock__other", "e2e-4", &json!({}))
        .await
        .expect("direct sibling call");
    assert_eq!(called_text(&result), "other:ok");

    plugin.manager().shutdown().await;
}
