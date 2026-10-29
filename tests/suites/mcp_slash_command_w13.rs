//! The `/mcp` slash command (upstream
//! `extensions/mcp/index.ts`'s `pi.registerCommand("mcp", ...)`, :1258-1283).
//!
//! The command had no in-session surface: pir shipped `pir mcp ...` as a CLI
//! subcommand only, so a user inside the TUI found nothing when upstream told
//! them to `Run /mcp to fix.` These tests pin the native command surface:
//!
//! * registration with upstream's description,
//! * argument parsing identical to upstream's `handler` (three subcommands,
//!   extras rejected with `MCP_USAGE`),
//! * argument completions identical to upstream's `getArgumentCompletions`
//!   (actions, then the servers eligible for the typed action),
//! * the status formatter naming every configured server and its state.
//!
//! Hermetic: the manager is built from in-memory registry entries in a temp
//! agent dir; nothing connects and no `~/.pi` path is written.

#![cfg(feature = "mcp")]

use std::path::PathBuf;
use std::sync::Arc;

use pir::config::mcp::{McpServerConfig, McpServerConfigBase, McpStdioServerConfig};
use pir::mcp::registry::{McpServerEntry, McpServerSource};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Pin `PIR_AGENT_DIR` at `dir` through the SHARED process-wide guard
/// (`tests/common/mod.rs`), like the other `g_agent` suites: a module-local
/// lock would not exclude their pins, and this suite used to leak its dir.
async fn env_guard(dir: &TempDir) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::at(&dir.0, &[]).await
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-slash-{label}-{}-{}",
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

/// A stdio server whose command never exists, for the extension-registered
/// fixture (nothing ever spawns it).
fn extension_stdio_server(command: &str) -> McpServerConfig {
    McpServerConfig::Stdio(McpStdioServerConfig {
        base: McpServerConfigBase::default(),
        command: command.to_owned(),
        ..Default::default()
    })
}

/// Write the global `mcp.json` (the wrapper shape the loader reads) BEFORE the
/// plugin is built, so the plugin's own config pass registers the entries —
/// the real path, not an in-memory shortcut.
fn write_mcp_json(dir: &TempDir, servers: serde_json::Value) {
    std::fs::write(
        dir.0.join("mcp.json"),
        serde_json::json!({ "mcpServers": servers }).to_string(),
    )
    .expect("write mcp.json");
}

fn build_plugin(dir: &TempDir) -> (Arc<McpPlugin>, PluginHost) {
    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    (plugin, host)
}

/// Register one extension-sourced server directly on the manager's registry
/// (no file behind it, like `pi.registerMcpServer`).
fn register_extension_server(plugin: &McpPlugin, name: &str, config: McpServerConfig) {
    plugin.manager().registry().register(McpServerEntry {
        name: name.to_owned(),
        config,
        source: McpServerSource::Extension("<test>".to_owned()),
    });
}

/// Upstream's usage line (index.ts:277).
const MCP_USAGE: &str =
    "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";

const MCP_DESCRIPTION: &str =
    "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure";

// ---------------------------------------------------------------------------
// (a) Registration
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mcp_command_is_registered_with_upstream_description() {
    let dir = TempDir::new("register");
    let _guard = env_guard(&dir).await;

    let (_plugin, host) = build_plugin(&dir);
    let command = host
        .registry()
        .resolved_commands()
        .into_iter()
        .find(|command| command.def.name == "mcp")
        .expect("the mcp builtin registers /mcp");
    assert_eq!(command.def.description.as_deref(), Some(MCP_DESCRIPTION));
    assert!(
        matches!(command.backend, Some(pir::plugins::CommandBackend::Rust(_))),
        "the command runs on the native backend"
    );
}

// ---------------------------------------------------------------------------
// (b) Argument parsing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mcp_parses_upstream_actions_and_rejects_extras() {
    let dir = TempDir::new("parse");
    let _guard = env_guard(&dir).await;

    write_mcp_json(
        &dir,
        serde_json::json!({ "oauth": { "url": "http://127.0.0.1:9/mcp" } }),
    );
    let (_plugin, host) = build_plugin(&dir);

    // The three upstream subcommands are accepted (and fail for a name that
    // does not exist, which is the pre-usage error path).
    let outcome = host
        .dispatch_command("mcp", "login nosuch")
        .await
        .expect("native command");
    assert!(!outcome.ok);
    assert_eq!(
        outcome.error.as_deref(),
        Some("No MCP server named \"nosuch\".")
    );

    let outcome = host
        .dispatch_command("mcp", "logout nosuch")
        .await
        .expect("native command");
    assert!(!outcome.ok);
    assert_eq!(
        outcome.error.as_deref(),
        Some("No MCP server named \"nosuch\".")
    );

    let outcome = host
        .dispatch_command("mcp", "reconnect nosuch")
        .await
        .expect("native command");
    assert!(!outcome.ok);
    assert_eq!(
        outcome.error.as_deref(),
        Some("No MCP server named \"nosuch\".")
    );

    // A headless host cannot sign in: the explicit-name form is accepted and
    // reports the missing UI instead of the usage line.
    let outcome = host
        .dispatch_command("mcp", "login oauth")
        .await
        .expect("native command");
    assert!(!outcome.ok);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|text| text.contains("requires interactive mode")),
        "{outcome:?}"
    );

    // A third field is rejected with upstream's usage string.
    for args in [
        "login oauth extra",
        "logout a b",
        "reconnect a b",
        "bogus",
        "bogus name",
    ] {
        let outcome = host
            .dispatch_command("mcp", args)
            .await
            .expect("native command");
        assert!(!outcome.ok, "{args}: {outcome:?}");
        assert_eq!(outcome.error.as_deref(), Some(MCP_USAGE), "{args}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_enable_disable_apply_to_the_session() {
    let dir = TempDir::new("toggle");
    let _guard = env_guard(&dir).await;

    let (plugin, host) = build_plugin(&dir);
    // Extension-registered servers have no file, so the toggle is session-only
    // (upstream: registered servers apply to the current session).
    register_extension_server(
        &plugin,
        "docs",
        extension_stdio_server("pir-w13-does-not-exist"),
    );

    let outcome = host
        .dispatch_command("mcp", "disable docs")
        .await
        .expect("native command");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        outcome.error.as_deref(),
        Some("Disabled MCP server \"docs\" for this session.")
    );
    assert!(
        !plugin
            .manager()
            .registry()
            .get("docs")
            .expect("entry")
            .config
            .is_enabled(),
        "the session entry is disabled"
    );

    let status = plugin.format_status().await;
    assert!(status.contains("docs"), "{status}");
    assert!(status.contains("disabled"), "{status}");
}

// ---------------------------------------------------------------------------
// (c) Completions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mcp_completions_follow_action_eligibility() {
    let dir = TempDir::new("complete");
    let _guard = env_guard(&dir).await;

    write_mcp_json(
        &dir,
        serde_json::json!({
            "oauth": { "url": "http://127.0.0.1:9/mcp" },
            "stdio": { "command": "pir-w13-does-not-exist" },
            "disabled": { "url": "http://127.0.0.1:9/other", "enabled": false },
        }),
    );
    let (plugin, _host) = build_plugin(&dir);

    // First field: the three upstream actions, prefix-filtered.
    let items = plugin.command_argument_completions("").await;
    let values: Vec<&str> = items.iter().map(|item| item.value.as_str()).collect();
    assert_eq!(values, ["login ", "logout ", "reconnect "]);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
    assert_eq!(labels, ["login", "logout", "reconnect"]);

    let items = plugin.command_argument_completions("l").await;
    assert_eq!(
        items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        ["login ", "logout "]
    );
    assert!(plugin.command_argument_completions("z").await.is_empty());

    // Second field: OAuth servers for login/logout (disabled ones excluded),
    // with the state as the description.
    let items = plugin.command_argument_completions("login ").await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].value, "login oauth");
    assert_eq!(items[0].label, "oauth");
    assert_eq!(items[0].description.as_deref(), Some("configured"));

    // Name filtering.
    let items = plugin.command_argument_completions("login o").await;
    assert_eq!(
        items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        ["login oauth"]
    );
    assert!(plugin
        .command_argument_completions("login st")
        .await
        .is_empty());

    // Reconnect offers only servers with a live connection object. Creating
    // one fails to spawn (a nonexistent command), which still installs the
    // connection the eligibility rule reads.
    assert!(plugin
        .command_argument_completions("reconnect ")
        .await
        .is_empty());
    let _ = plugin.manager().get_client("stdio").await;
    assert!(plugin.manager().has_connection("stdio").await);
    let items = plugin.command_argument_completions("reconnect ").await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].value, "reconnect stdio");

    // A third field is not completed (upstream returns null).
    assert!(plugin
        .command_argument_completions("login oauth extra")
        .await
        .is_empty());
}

// ---------------------------------------------------------------------------
// (d) Status
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mcp_status_names_every_server_and_its_state() {
    let dir = TempDir::new("status");
    let _guard = env_guard(&dir).await;

    write_mcp_json(
        &dir,
        serde_json::json!({
            "docs": { "command": "pir-w13-does-not-exist" },
            "off": { "command": "pir-w13-does-not-exist", "enabled": false },
        }),
    );
    let (plugin, host) = build_plugin(&dir);

    let outcome = host
        .dispatch_command("mcp", "")
        .await
        .expect("native command");
    let text = outcome.error.expect("display text");
    assert!(outcome.ok, "{text}");
    assert!(text.contains("docs"), "{text}");
    assert!(text.contains("off"), "{text}");
    assert!(
        text.contains("configured"),
        "the lazy state is shown: {text}"
    );
    assert!(
        text.contains("disabled"),
        "the disabled state is shown: {text}"
    );

    // The upstream-shaped plain form names the same servers.
    let status = plugin.format_status().await;
    assert!(status.contains("docs:"), "{status}");
    assert!(status.contains("off: disabled"), "{status}");
    assert!(
        status.contains("(direct)"),
        "the exposure is shown: {status}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_status_reports_an_empty_configuration() {
    let dir = TempDir::new("empty");
    let _guard = env_guard(&dir).await;

    let (_plugin, host) = build_plugin(&dir);
    let outcome = host
        .dispatch_command("mcp", "")
        .await
        .expect("native command");
    let text = outcome.error.expect("display text");
    assert!(
        text.contains("No MCP servers configured"),
        "the empty state points at the config files: {text}"
    );
    assert!(text.contains("mcp.json"), "{text}");
}
