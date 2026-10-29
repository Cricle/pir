//! `mcp.json`'s top-level `builtinTools` flag (pir-specific; upstream pi has
//! no such flag).
//!
//! With `builtinTools: true` (the default), a server that does not set
//! `exposure` exposes its tools like builtin tools: the base exposure
//! resolves to `direct`. An explicit `exposure` (`codemode` / `deferred` /
//! `hidden` / `direct`) still wins, per-tool `toolExposure` overrides still
//! win over the base exposure, and `builtinTools: false` restores upstream's
//! `codemode` default.
//!
//! The flag is read only from the wrapper shape (`mcpServers` present), the
//! same rule as `autoEnableCodemode`; in the legacy flat shape a
//! `builtinTools` key is just another server entry.

#![cfg(feature = "mcp")]

use std::path::PathBuf;
use std::sync::Arc;

use pir::agent::tool_execution::{AgentTool, ToolExposure, ToolRegistry};
use pir::config::mcp::{
    effective_resource_exposure, load_mcp_configs, parse_mcp_config_document_ex, McpExposure,
    McpServerConfig,
};
use pir::mcp::manager::{DiscoveredTool, McpManager};
use pir::mcp::protocol::McpTool;
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost};

// ---------------------------------------------------------------------------
// Fixtures
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
            "pir-mcp-builtin-{label}-{}-{}",
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

fn config(document: &str) -> McpServerConfig {
    let parsed = parse_mcp_config_document_ex(document);
    assert!(
        parsed.errors.is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors
    );
    parsed.servers.into_values().next().expect("one server")
}

// ---------------------------------------------------------------------------
// Parsing + effective exposure
// ---------------------------------------------------------------------------

#[test]
fn absent_builtin_tools_defaults_to_direct() {
    let parsed = parse_mcp_config_document_ex(r#"{ "mcpServers": { "a": { "command": "x" } } }"#);
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    assert_eq!(
        parsed.builtin_tools, None,
        "an absent flag keeps the caller's default (true)"
    );
    let server = parsed.servers.get("a").expect("server a");
    assert_eq!(
        server.base().exposure,
        None,
        "parsing must not rewrite the explicit/default distinction"
    );
    assert_eq!(server.effective_exposure(true), McpExposure::Direct);
    assert_eq!(server.effective_exposure(false), McpExposure::Codemode);
}

#[test]
fn explicit_builtin_tools_false_restores_codemode() {
    let parsed = parse_mcp_config_document_ex(
        r#"{ "mcpServers": { "a": { "command": "x" } }, "builtinTools": false }"#,
    );
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    assert_eq!(parsed.builtin_tools, Some(false));
    let server = parsed.servers.get("a").expect("server a");
    assert_eq!(server.effective_exposure(false), McpExposure::Codemode);
}

#[test]
fn explicit_exposure_wins_over_builtin_tools() {
    let server = config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "deferred" } } }"#);
    assert_eq!(server.effective_exposure(true), McpExposure::Deferred);

    let server = config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "codemode" } } }"#);
    assert_eq!(server.effective_exposure(true), McpExposure::Codemode);

    let server = config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "hidden" } } }"#);
    assert_eq!(server.effective_exposure(true), McpExposure::Hidden);
}

#[test]
fn tool_exposure_overrides_still_win_over_the_base() {
    let server = config(
        r#"{
            "mcpServers": {
                "a": { "command": "x", "toolExposure": { "t": "hidden", "read_*": "deferred" } }
            }
        }"#,
    );
    // Per-tool overrides beat the `builtinTools` base default.
    assert_eq!(
        server.effective_tool_exposure("t", true),
        McpExposure::Hidden
    );
    assert_eq!(
        server.effective_tool_exposure("read_file", true),
        McpExposure::Deferred
    );
    // A tool with no override follows the base exposure.
    assert_eq!(
        server.effective_tool_exposure("other", true),
        McpExposure::Direct
    );
    // `configured_exposures` lists the base plus every distinct override.
    let exposures = server.configured_exposures(true);
    assert!(exposures.contains(&McpExposure::Direct), "{exposures:?}");
    assert!(exposures.contains(&McpExposure::Hidden), "{exposures:?}");
    assert!(exposures.contains(&McpExposure::Deferred), "{exposures:?}");
    assert_eq!(exposures.len(), 3, "de-duplicated: {exposures:?}");

    // Under `builtinTools: false` the base is codemode, the overrides stand.
    let exposures = server.configured_exposures(false);
    assert!(exposures.contains(&McpExposure::Codemode), "{exposures:?}");
    assert!(exposures.contains(&McpExposure::Hidden), "{exposures:?}");
    assert!(!exposures.contains(&McpExposure::Direct), "{exposures:?}");
}

#[test]
fn non_boolean_builtin_tools_is_reported_and_treated_as_absent() {
    let parsed = parse_mcp_config_document_ex(
        r#"{ "mcpServers": { "a": { "command": "x" } }, "builtinTools": "yes" }"#,
    );
    assert_eq!(parsed.builtin_tools, None);
    assert!(
        parsed
            .errors
            .iter()
            .any(|error| error.contains("builtinTools must be a boolean")),
        "{:?}",
        parsed.errors
    );
    // Treated as absent: the caller's default (true) still resolves to direct.
    assert_eq!(
        parsed
            .servers
            .get("a")
            .expect("server a")
            .effective_exposure(true),
        McpExposure::Direct
    );
}

#[test]
fn legacy_flat_shape_has_no_flag_slot() {
    // No `mcpServers`: the whole object is the server map, so the flag is
    // absent and the shape is flagged as legacy.
    let parsed = parse_mcp_config_document_ex(r#"{ "docs": { "command": "x" } }"#);
    assert_eq!(parsed.builtin_tools, None);
    assert!(!parsed.wrapper_shape);
    assert!(parsed.servers.contains_key("docs"));

    // A `builtinTools` key in the flat shape is NOT read as a flag: it is
    // parsed as a (here invalid) server entry and reported.
    let parsed =
        parse_mcp_config_document_ex(r#"{ "builtinTools": false, "docs": { "command": "x" } }"#);
    assert_eq!(parsed.builtin_tools, None);
    assert!(!parsed.wrapper_shape);
    assert!(!parsed.servers.contains_key("builtinTools"));
    assert!(
        parsed
            .errors
            .iter()
            .any(|error| error.starts_with("server \"builtinTools\"")),
        "{:?}",
        parsed.errors
    );

    // The wrapper shape is recorded even when the flag is absent.
    let parsed = parse_mcp_config_document_ex(r#"{ "mcpServers": {} }"#);
    assert!(parsed.wrapper_shape);
}

#[test]
fn load_mcp_configs_carries_each_files_flag() {
    let global = TempDir::new("global");
    let project = TempDir::new("project");
    std::fs::write(
        global.0.join("mcp.json"),
        r#"{ "mcpServers": {}, "builtinTools": false }"#,
    )
    .expect("write global mcp.json");
    std::fs::create_dir_all(project.0.join(".pi")).expect("mkdir .pi");
    std::fs::write(
        project.0.join(".pi").join("mcp.json"),
        r#"{ "mcpServers": {}, "builtinTools": true }"#,
    )
    .expect("write project mcp.json");

    let loaded = load_mcp_configs(&global.0, &project.0, true);
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].builtin_tools, Some(false));
    assert_eq!(loaded[1].builtin_tools, Some(true), "project over global");
    assert!(loaded[0].wrapper_shape && loaded[1].wrapper_shape);

    let loaded = load_mcp_configs(&global.0, &project.0, false);
    assert_eq!(loaded.len(), 1, "untrusted project file is not loaded");
    assert_eq!(loaded[0].builtin_tools, Some(false));
}

// ---------------------------------------------------------------------------
// Resource-tool exposure precedence
// ---------------------------------------------------------------------------

#[test]
fn resource_exposure_precedence_under_the_direct_default() {
    let unset = config(r#"{ "mcpServers": { "a": { "command": "x" } } }"#);
    let codemode =
        config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "codemode" } } }"#);
    let deferred =
        config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "deferred" } } }"#);
    let direct = config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "direct" } } }"#);
    let hidden = config(r#"{ "mcpServers": { "a": { "command": "x", "exposure": "hidden" } } }"#);

    assert_eq!(effective_resource_exposure(&[], true), None);
    assert_eq!(
        effective_resource_exposure(std::slice::from_ref(&hidden), true),
        None
    );

    // An unset exposure is `direct` under builtinTools, `codemode` without.
    assert_eq!(
        effective_resource_exposure(std::slice::from_ref(&unset), true),
        Some(McpExposure::Direct)
    );
    assert_eq!(
        effective_resource_exposure(std::slice::from_ref(&unset), false),
        Some(McpExposure::Codemode)
    );

    // direct > codemode > deferred.
    assert_eq!(
        effective_resource_exposure(&[deferred.clone(), codemode.clone()], true),
        Some(McpExposure::Codemode)
    );
    assert_eq!(
        effective_resource_exposure(&[codemode.clone(), direct.clone()], true),
        Some(McpExposure::Direct)
    );
    assert_eq!(
        effective_resource_exposure(&[deferred.clone(), hidden.clone()], true),
        Some(McpExposure::Deferred)
    );
    assert_eq!(
        effective_resource_exposure(&[unset, hidden], true),
        Some(McpExposure::Direct)
    );
}

// ---------------------------------------------------------------------------
// Bridge exposure
// ---------------------------------------------------------------------------

fn bridge_exposure(exposure: McpExposure) -> ToolExposure {
    let manager = McpManager::without_log(std::path::Path::new("."));
    let discovered = DiscoveredTool {
        server: "docs".to_owned(),
        tool: McpTool {
            name: "search".to_owned(),
            ..Default::default()
        },
        exposure: McpExposure::Codemode,
    };
    let registered = pir::mcp::registry::assign_mcp_tool_names("docs", &["search".to_owned()])
        .into_iter()
        .next()
        .expect("name");
    let bridge = pir::plugins::builtin::McpToolBridge::with_effective_exposure(
        manager, discovered, registered, exposure,
    );
    bridge.exposure()
}

#[test]
fn bridge_uses_the_effective_exposure() {
    assert_eq!(bridge_exposure(McpExposure::Direct), ToolExposure::Direct);
    assert_eq!(
        bridge_exposure(McpExposure::Codemode),
        ToolExposure::Deferred
    );
    assert_eq!(
        bridge_exposure(McpExposure::Deferred),
        ToolExposure::Deferred
    );
    assert_eq!(bridge_exposure(McpExposure::Hidden), ToolExposure::Hidden);
}

// ---------------------------------------------------------------------------
// Plugin: flag merge + discovery activation
// ---------------------------------------------------------------------------

fn build_plugin(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> (Arc<McpPlugin>, PluginHost) {
    let plugin = Arc::new(McpPlugin::with_cwd(agent_dir, cwd));
    let host = PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>]);
    (plugin, host)
}

#[tokio::test(flavor = "current_thread")]
async fn default_direct_does_not_activate_codemode() {
    let _guard = env_guard().await;
    let agent = TempDir::new("plugin-default");
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{ "mcpServers": { "docs": { "command": "true" } } }"#,
    )
    .expect("write mcp.json");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);

    let (plugin, _host) = build_plugin(&agent.0, &agent.0);
    assert!(plugin.builtin_tools(), "the flag defaults to true");
    let decision = plugin.discovery_activation(&ToolRegistry::new(), &[]);
    assert!(
        !decision.needs_codemode && !decision.needs_tool_search,
        "a direct-by-default server needs no discovery tool: {decision:?}"
    );
    assert!(decision.activate.is_empty());
    plugin.manager().shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_tools_false_restores_the_codemode_default() {
    let _guard = env_guard().await;
    let agent = TempDir::new("plugin-off");
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{ "mcpServers": { "docs": { "command": "true" } }, "builtinTools": false }"#,
    )
    .expect("write mcp.json");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);

    let (plugin, _host) = build_plugin(&agent.0, &agent.0);
    assert!(!plugin.builtin_tools());
    let decision = plugin.discovery_activation(&ToolRegistry::new(), &[]);
    assert!(
        decision.needs_codemode,
        "an unset exposure falls back to codemode: {decision:?}"
    );
    plugin.manager().shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_exposure_still_drives_activation_under_builtin_tools() {
    let _guard = env_guard().await;
    let agent = TempDir::new("plugin-explicit");
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{
            "mcpServers": {
                "docs": { "command": "true", "exposure": "deferred" },
                "scripted": { "command": "true", "exposure": "codemode" }
            }
        }"#,
    )
    .expect("write mcp.json");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);

    let (plugin, _host) = build_plugin(&agent.0, &agent.0);
    assert!(plugin.builtin_tools());
    let decision = plugin.discovery_activation(&ToolRegistry::new(), &[]);
    assert!(
        decision.needs_codemode && decision.needs_tool_search,
        "{decision:?}"
    );
    plugin.manager().shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_flat_file_keeps_the_codemode_default() {
    let _guard = env_guard().await;
    let agent = TempDir::new("plugin-legacy");
    // Legacy flat shape: no `mcpServers`, so the file has no flag slot and
    // its servers keep upstream's codemode default even though the merged
    // `builtinTools` default is true.
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{ "docs": { "command": "true" } }"#,
    )
    .expect("write mcp.json");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);

    let (plugin, _host) = build_plugin(&agent.0, &agent.0);
    assert!(plugin.builtin_tools(), "the file-level default is true");
    let decision = plugin.discovery_activation(&ToolRegistry::new(), &[]);
    assert!(
        decision.needs_codemode,
        "a legacy flat entry is pinned to codemode: {decision:?}"
    );
    plugin.manager().shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn project_flag_overrides_global() {
    let _guard = env_guard().await;
    let agent = TempDir::new("plugin-merge-agent");
    let cwd = TempDir::new("plugin-merge-cwd");
    std::fs::write(
        agent.0.join("mcp.json"),
        r#"{ "mcpServers": {}, "builtinTools": false }"#,
    )
    .expect("write global mcp.json");
    std::fs::create_dir_all(cwd.0.join(".pi")).expect("mkdir .pi");
    std::fs::write(
        cwd.0.join(".pi").join("mcp.json"),
        r#"{ "mcpServers": {}, "builtinTools": true }"#,
    )
    .expect("write project mcp.json");
    std::env::set_var("PIR_AGENT_DIR", &agent.0);
    std::env::set_var("PIR_TRUST_PROJECT", "1");

    let (plugin, _host) = build_plugin(&agent.0, &cwd.0);
    assert!(
        plugin.builtin_tools(),
        "the trusted project value overrides the global one"
    );
    plugin.manager().shutdown().await;
    std::env::remove_var("PIR_TRUST_PROJECT");
}
