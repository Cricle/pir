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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::config::mcp::{McpServerConfig, McpServerConfigBase, McpStdioServerConfig};
use pir::mcp::registry::{McpServerEntry, McpServerSource};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::services::{
    DialogOptions, ExtensionMode, NotifyLevel, PluginServices, SelectOption, ServiceFuture,
    ServiceResult, SessionService, UiService,
};
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

// ---------------------------------------------------------------------------
// (e) The manager's project-scope actions and Tools view (upstream
//     `serverMenu` / `showTools`, index.ts:820-899).
// ---------------------------------------------------------------------------

/// A UI service that plays scripted `select` answers in order (`None` once
/// the script is exhausted, i.e. the user closes the menu) and records every
/// menu plus every notice.
struct ScriptedUi {
    answers: Mutex<std::collections::VecDeque<String>>,
    selects: Mutex<Vec<(String, Vec<SelectOption>)>>,
    notices: Mutex<Vec<(NotifyLevel, String)>>,
}

impl ScriptedUi {
    fn new(answers: &[&str]) -> Self {
        Self {
            answers: Mutex::new(answers.iter().map(|answer| (*answer).to_owned()).collect()),
            selects: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
        }
    }

    fn selects(&self) -> Vec<(String, Vec<SelectOption>)> {
        self.selects.lock().unwrap().clone()
    }

    fn notices(&self) -> Vec<(NotifyLevel, String)> {
        self.notices.lock().unwrap().clone()
    }
}

impl UiService for ScriptedUi {
    fn notify(&self, level: NotifyLevel, message: &str) {
        self.notices
            .lock()
            .unwrap()
            .push((level, message.to_owned()));
    }

    fn select(
        &self,
        title: &str,
        options: &[SelectOption],
        _dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        self.selects
            .lock()
            .unwrap()
            .push((title.to_owned(), options.to_vec()));
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move { Ok(answer) })
    }
}

/// The interactive host mode the manager path requires (`manager_ui`:
/// upstream's `ctx.mode === "tui" && ctx.hasUI`).
struct InteractiveSession;

impl SessionService for InteractiveSession {
    fn mode(&self) -> ExtensionMode {
        ExtensionMode::Interactive
    }

    fn has_ui(&self) -> bool {
        true
    }
}

/// Trust the project at `dir` so `/mcp` offers the project-scope actions
/// (upstream `projectConfig !== undefined`, index.ts:308-311).
fn trust_project(dir: &TempDir) {
    pir::config::trust::ProjectTrustStore::new(&dir.0)
        .set(&dir.0, Some(true))
        .expect("record the project trust decision");
}

fn bind_interactive(host: &PluginHost, ui: &Arc<ScriptedUi>) {
    host.bind_context(
        PluginServices::new()
            .with_session(Arc::new(InteractiveSession))
            .with_ui(ui.clone())
            .build(),
    );
}

fn menu_labels(selects: &[(String, Vec<SelectOption>)], title_prefix: &str) -> Vec<String> {
    selects
        .iter()
        .find(|(title, _)| title.starts_with(title_prefix))
        .map(|(_, options)| options.iter().map(|option| option.label.clone()).collect())
        .unwrap_or_else(|| panic!("no menu titled {title_prefix:?}: {selects:?}"))
}

#[tokio::test(flavor = "current_thread")]
async fn manager_disable_in_this_project_writes_the_project_override() {
    let dir = TempDir::new("project-disable");
    let _guard = env_guard(&dir).await;
    write_mcp_json(
        &dir,
        serde_json::json!({ "docs": { "command": "pir-w13-does-not-exist" } }),
    );
    trust_project(&dir);
    let (plugin, host) = build_plugin(&dir);
    let ui = Arc::new(ScriptedUi::new(&["docs", "disable-project"]));
    bind_interactive(&host, &ui);

    let outcome = host
        .dispatch_command("mcp", "")
        .await
        .expect("native command");
    assert!(outcome.ok, "{outcome:?}");

    // Upstream writes the minimal override into the trusted project's
    // `mcp.json` (created when missing), not a server definition.
    let project_path = dir.0.join(".pi").join("mcp.json");
    let project = std::fs::read_to_string(&project_path).expect("project mcp.json written");
    let value: serde_json::Value = serde_json::from_str(&project).expect("valid JSON");
    assert_eq!(
        value["mcpServers"]["docs"],
        serde_json::json!({ "enabled": false }),
        "Disable in this project writes `enabled: false` as an override: {project}"
    );

    // pi's label, in pi's position: after Disable, before pir's Remove extra.
    let labels = menu_labels(&ui.selects(), "MCP server docs");
    assert_eq!(
        labels,
        [
            "Exposure: direct",
            "Disable",
            "Disable in this project",
            "Remove for this session",
            "Back"
        ],
        "the manager items follow upstream's order"
    );
    // Upstream's description text for the action, in the success notice.
    assert!(
        ui.notices().iter().any(|(level, text)| {
            *level == NotifyLevel::Info
                && text == "Disabled MCP server \"docs\" (saved to the project mcp.json)."
        }),
        "{:?}",
        ui.notices()
    );

    // The live entry follows the override: project-sourced and disabled (the
    // next config pass reads the same effective entry).
    let entry = plugin.manager().registry().get("docs").expect("entry");
    assert!(!entry.config.is_enabled());
    assert!(
        matches!(entry.source, McpServerSource::ProjectConfig(_)),
        "{:?}",
        entry.source
    );
}

#[tokio::test(flavor = "current_thread")]
async fn manager_enable_in_this_project_writes_the_enabling_override() {
    let dir = TempDir::new("project-enable");
    let _guard = env_guard(&dir).await;
    write_mcp_json(
        &dir,
        serde_json::json!({
            "docs": { "command": "pir-w13-does-not-exist", "enabled": false }
        }),
    );
    trust_project(&dir);
    let (_plugin, host) = build_plugin(&dir);
    let ui = Arc::new(ScriptedUi::new(&["docs", "enable-project"]));
    bind_interactive(&host, &ui);

    let outcome = host
        .dispatch_command("mcp", "")
        .await
        .expect("native command");
    assert!(outcome.ok, "{outcome:?}");

    let project = std::fs::read_to_string(dir.0.join(".pi").join("mcp.json"))
        .expect("project mcp.json written");
    let value: serde_json::Value = serde_json::from_str(&project).expect("valid JSON");
    assert_eq!(
        value["mcpServers"]["docs"],
        serde_json::json!({ "enabled": true }),
        "a created override keeps the explicit `enabled: true` (upstream keepDefaults): {project}"
    );
    // The disabled branch offers Enable then Enable in this project.
    let labels = menu_labels(&ui.selects(), "MCP server docs");
    assert_eq!(labels, ["Enable", "Enable in this project", "Back"]);
    assert!(
        ui.notices().iter().any(|(level, text)| {
            *level == NotifyLevel::Info
                && text == "Enabled MCP server \"docs\" (saved to the project mcp.json)."
        }),
        "{:?}",
        ui.notices()
    );
}

/// Upstream `updateMcpServerConfig`'s `keepDefaults` (config.ts:184-192): an
/// override entry keeps explicit values, because the override REPLACES the
/// global server's defaults. The GLOBAL Enable action on a project-override
/// entry therefore writes `enabled: true` instead of stripping the key —
/// stripping it would silently un-pin the project against a later global
/// `enabled: false`.
#[tokio::test(flavor = "current_thread")]
async fn global_enable_on_a_project_override_keeps_the_explicit_value() {
    let dir = TempDir::new("override-keep-defaults");
    let _guard = env_guard(&dir).await;
    write_mcp_json(
        &dir,
        serde_json::json!({ "docs": { "command": "pir-w13-does-not-exist" } }),
    );
    // The project override lives in `<cwd>/.pi/mcp.json`; the directory is the
    // host's (it is created before the file is written).
    std::fs::create_dir_all(dir.0.join(".pi")).expect("mkdir .pi");
    std::fs::write(
        dir.0.join(".pi").join("mcp.json"),
        serde_json::json!({ "mcpServers": { "docs": { "enabled": false } } }).to_string(),
    )
    .expect("write project mcp.json");
    trust_project(&dir);
    let (plugin, host) = build_plugin(&dir);

    let outcome = host
        .dispatch_command("mcp", "enable docs")
        .await
        .expect("native command");
    assert!(outcome.ok, "{outcome:?}");

    let project = std::fs::read_to_string(dir.0.join(".pi").join("mcp.json"))
        .expect("project mcp.json readable");
    let value: serde_json::Value = serde_json::from_str(&project).expect("valid JSON");
    assert_eq!(
        value["mcpServers"]["docs"],
        serde_json::json!({ "enabled": true }),
        "an override keeps the explicit enabling value (upstream keepDefaults): {project}"
    );
    let entry = plugin.manager().registry().get("docs").expect("entry");
    assert!(entry.config.is_enabled());
    assert!(
        matches!(entry.source, McpServerSource::ProjectConfig(_)),
        "{:?}",
        entry.source
    );
}

#[tokio::test(flavor = "current_thread")]
async fn manager_tools_view_lists_each_tools_effective_exposure() {
    let dir = TempDir::new("tools-view");
    let _guard = env_guard(&dir).await;
    let (command, args) = super::mcp_residuals_w13::mock_spawn();
    std::fs::write(
        dir.0.join("mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "docs": {
                    "command": command,
                    "args": args,
                    "env": { "PIR_W13_MOCK": super::mcp_residuals_w13::MOCK_HEALTHY },
                    "toolExposure": { "work": "hidden" },
                }
            }
        })
        .to_string(),
    )
    .expect("write mcp.json");
    trust_project(&dir);
    let (plugin, host) = build_plugin(&dir);
    let ui = Arc::new(ScriptedUi::new(&["docs", "tools", "back"]));
    bind_interactive(&host, &ui);
    // The discovery call below (spawn the healthy mock stdio server, MCP
    // handshake, tools/list) measured 954 ms when this test was instrumented
    // and run once on 2026-11-14 (loaded 9-agent machine, `zz_merged_6`).
    // 5 s is ~5x that observed cost: still enough headroom for a slow CI
    // machine, but a genuine hang (no handshake ever completes) now fails in
    // 5 s instead of the old blanket 30 s safety net.
    plugin
        .manager()
        .discover_all_bounded(Duration::from_secs(5))
        .await;

    let outcome = host
        .dispatch_command("mcp", "")
        .await
        .expect("native command");
    assert!(outcome.ok, "{outcome:?}");

    // Upstream's connected-server order: Tools before Reconnect, exposure
    // before Disable, the project toggle after Disable.
    let labels = menu_labels(&ui.selects(), "MCP server docs");
    assert_eq!(
        labels,
        [
            "Tools",
            "Reconnect",
            "Exposure: direct",
            "Disable",
            "Disable in this project",
            "Remove for this session",
            "Back"
        ]
    );

    // The Tools view lists the tool with its effective `toolExposure` (the
    // base is direct; `work` overrides it to hidden).
    let selects = ui.selects();
    let tools_menu = selects
        .iter()
        .find(|(title, _)| title == "Tools of docs")
        .expect("the Tools view opened");
    assert_eq!(
        tools_menu
            .1
            .iter()
            .map(|option| option.label.as_str())
            .collect::<Vec<_>>(),
        ["work [hidden]", "Back"]
    );
    assert_eq!(tools_menu.1[0].value, "work");

    plugin.manager().shutdown().await;
}
