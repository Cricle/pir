//! W13-Y: the MCP status surface — the server-side truth any MCP manager UI
//! (the `/mcp` slash command, a TUI panel, an RPC consumer) displays.
//!
//! Upstream's `/mcp` shows, per server: a lifecycle state (configured /
//! connecting / connected / failed / disconnected / needs sign-in /
//! disabled), the effective exposure, the tool count, and the connection's
//! error; plus the file-level config problems (`configErrors`) and the
//! extension registrations an `mcp.json` entry shadows (`overridden`), and it
//! has a plain-text render (`formatStatus`, `extensions/mcp/index.ts:1017`).
//!
//! These tests pin [`McpManager::status_snapshot`], [`McpServerStatus`] and
//! [`McpStatusSnapshot::format_status`] against that shape:
//!
//! * **(a)** every state is reported distinctly, and an unconnected server
//!   reports `configured` instead of blocking on a handshake;
//! * **(b)** a `mcp.json` parse error reaches the snapshot and the text;
//! * **(c)** exposure resolution matches `effective_tool_exposure` for a
//!   `direct`, a `codemode` and a per-tool-override case;
//! * **(d)** the text formatter names every server with its state and count.
//!
//! Hermetic: in-memory configs only. No server process is ever spawned — the
//! states are driven through the manager's own seams (`connection_states`
//! is read-only, and the only connection states reachable without a spawn are
//! exercised via a deliberately unspawnable server whose connect fails fast)
//! — and every assertion that would need a live handshake is written against
//! the snapshot the state machine produces, not against a server.

#![cfg(feature = "mcp")]

use std::path::PathBuf;

use pir::config::mcp::{McpExposure, McpServerConfig};
use pir::mcp::manager::{ConnectionState, McpManager, McpServerStatus, McpStatusSnapshot};
use pir::mcp::registry::{McpServerEntry, McpServerSource};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn config(value: serde_json::Value) -> McpServerConfig {
    serde_json::from_value(value).expect("valid server config")
}

fn entry(name: &str, value: serde_json::Value, source: McpServerSource) -> McpServerEntry {
    McpServerEntry {
        name: name.to_owned(),
        config: config(value),
        source,
    }
}

/// A source path that never exists on disk, so no test reads or writes it.
fn source_path() -> PathBuf {
    PathBuf::from("C:/pir-w13-y/mcp.json")
}

fn global_source() -> McpServerSource {
    McpServerSource::GlobalConfig(source_path())
}

/// A manager with a no-op log and no agent dir: nothing touches the disk.
fn manager() -> std::sync::Arc<McpManager> {
    McpManager::without_log(std::path::Path::new("C:/pir-w13-y"))
}

/// The snapshot under upstream's default `builtinTools: false` (an entry
/// without an explicit `exposure` resolves to `codemode`).
async fn snapshot(manager: &McpManager) -> McpStatusSnapshot {
    manager.status_snapshot(|_| false).await
}

fn server<'a>(snapshot: &'a McpStatusSnapshot, name: &str) -> &'a McpServerStatus {
    snapshot
        .servers
        .iter()
        .find(|server| server.name == name)
        .unwrap_or_else(|| panic!("server {name} missing from the snapshot"))
}

// ---------------------------------------------------------------------------
// (a) Per-server state
// ---------------------------------------------------------------------------

/// A configured server that was never used reports `configured` and `Idle`:
/// a status read must never start a handshake (upstream `describeState`'s
/// "starting" is a connect already queued, which pir spells `Connecting`).
#[tokio::test(flavor = "current_thread")]
async fn unconnected_server_reports_configured_without_connecting() {
    let manager = manager();
    manager.registry().register(entry(
        "docs",
        serde_json::json!({ "command": "true" }),
        global_source(),
    ));

    let snapshot = snapshot(&manager).await;
    let docs = server(&snapshot, "docs");
    assert_eq!(docs.state, ConnectionState::Idle);
    assert_eq!(docs.state_label(), "configured");
    assert!(docs.enabled);
    assert_eq!(docs.tool_count(), 0, "no discovery ran");
    assert_eq!(docs.transport, "stdio");
    assert_eq!(docs.error(), None);
    // The status call itself must not have created a connection object.
    assert!(
        !manager.has_connection("docs").await,
        "a status read is a snapshot, not a connect"
    );
    manager.shutdown().await;
}

/// `enabled: false` is reported as `disabled` regardless of the connection
/// state (upstream `formatStatus`'s `!isEnabled(server)` arm wins).
#[tokio::test(flavor = "current_thread")]
async fn disabled_server_reports_disabled() {
    let manager = manager();
    manager.registry().register(entry(
        "off",
        serde_json::json!({ "command": "true", "enabled": false }),
        global_source(),
    ));

    let snapshot = snapshot(&manager).await;
    let off = server(&snapshot, "off");
    assert_eq!(off.state, ConnectionState::Idle);
    assert!(!off.enabled);
    assert_eq!(off.state_label(), "disabled");
    assert!(
        snapshot
            .format_status()
            .contains("off: disabled (codemode)"),
        "the text form names the disabled state: {}",
        snapshot.format_status()
    );
    manager.shutdown().await;
}

/// The three connection states reachable without a live server report their
/// own labels, and only the failing ones carry an error. `NeedsAuth` is the
/// upstream `needs-auth` state (`markNeedsAuth`, runtime.ts:326) that
/// `ConnectionState::Failed` used to swallow.
#[test]
fn connection_states_have_distinct_labels() {
    let cases = [
        (ConnectionState::Idle, "configured", None),
        (ConnectionState::Connecting, "connecting", None),
        (ConnectionState::Connected, "connected", None),
        (
            ConnectionState::Disconnected { error: None },
            "disconnected, reconnects on next call",
            None,
        ),
        (
            ConnectionState::Disconnected {
                error: Some("transport closed\nstack".to_owned()),
            },
            // Upstream `describeState`'s default arm is the bare state; only
            // `failed` folds the error into the label.
            "disconnected, reconnects on next call",
            Some("transport closed\nstack"),
        ),
        (
            ConnectionState::Failed {
                error: "boom\nmore".to_owned(),
            },
            "failed: boom",
            Some("boom\nmore"),
        ),
        (
            ConnectionState::NeedsAuth {
                message: "MCP server \"docs\" requires sign-in. Run `pir mcp login docs`"
                    .to_owned(),
            },
            "needs sign-in",
            Some("MCP server \"docs\" requires sign-in. Run `pir mcp login docs`"),
        ),
        (ConnectionState::Closed, "closed", None),
    ];
    for (state, label, error) in cases {
        let status = McpServerStatus {
            name: "docs".to_owned(),
            config: config(serde_json::json!({ "command": "true" })),
            builtin_tools: false,
            exposure: McpExposure::Codemode,
            enabled: true,
            tools: Vec::new(),
            transport: "stdio".to_owned(),
            source: global_source(),
            state: state.clone(),
        };
        assert_eq!(status.state_label(), label, "state {state:?}");
        // The text form's state word never folds the error in.
        let word = status.state_word();
        assert!(!word.contains('\n'), "state word stays one line: {word}");
        if !matches!(state, ConnectionState::Failed { .. }) {
            assert_eq!(word, label, "state {state:?}");
        }
        assert_eq!(status.error(), error, "state {state:?}");
    }
}

/// The upstream `formatStatus` shape for a server that needs a sign-in names
/// the login command (index.ts:1025-1026), not a failure.
#[test]
fn needs_auth_line_points_at_the_login_command() {
    let status = McpStatusSnapshot {
        servers: vec![McpServerStatus {
            name: "remote".to_owned(),
            config: config(serde_json::json!({ "url": "https://example.invalid/mcp" })),
            builtin_tools: false,
            exposure: McpExposure::Deferred,
            enabled: true,
            tools: Vec::new(),
            transport: "http".to_owned(),
            source: global_source(),
            state: ConnectionState::NeedsAuth {
                message: "requires sign-in".to_owned(),
            },
        }],
        config_errors: Vec::new(),
        overridden: Vec::new(),
        agent_dir: PathBuf::from("C:/pir-w13-y"),
    };
    let text = status.format_status();
    assert_eq!(
        text,
        "remote: needs sign-in, run /mcp login remote (deferred)"
    );
}

/// A failed connect is reported with its error text: the state comes from the
/// manager's own state machine, driven here through a server whose command
/// cannot exist so the connect fails immediately (no network, no shell).
#[tokio::test(flavor = "current_thread")]
async fn failed_connect_surfaces_its_error_in_the_snapshot() {
    let manager = manager();
    manager.registry().register(entry(
        "broken",
        serde_json::json!({
            "command": "pir-test-nonexistent-w13y-command",
            "timeout": 1,
        }),
        global_source(),
    ));
    // The connect fails fast (no such executable); the failure is recorded.
    let error = manager
        .discover_server("broken")
        .await
        .expect_err("must fail");
    assert!(!error.to_string().is_empty());

    let snapshot = snapshot(&manager).await;
    let broken = server(&snapshot, "broken");
    assert!(
        matches!(broken.state, ConnectionState::Failed { .. }),
        "state: {:?}",
        broken.state
    );
    assert!(
        broken.state_label().starts_with("failed: "),
        "label: {}",
        broken.state_label()
    );
    // The text form: bare `failed` on the row, the error indented under it.
    let text = snapshot.format_status();
    assert!(text.contains("broken: failed (codemode)"), "{text}");
    assert!(text.contains("\n    "), "the error is indented: {text}");
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// (b) Config errors
// ---------------------------------------------------------------------------

/// A `mcp.json` parse error reaches the snapshot and the text form (upstream
/// `configErrors` -> `formatStatus`'s `config error:` lines).
#[tokio::test(flavor = "current_thread")]
async fn config_parse_errors_are_surfaced() {
    let manager = manager();
    // The parse problem is produced by the real loader, then handed to the
    // manager through the same seam the plugin uses.
    let (_, errors) =
        pir::config::mcp::parse_mcp_config(r#"{ "mcpServers": { "docs": { "command": 42 } } }"#);
    assert!(!errors.is_empty(), "the loader reports the bad entry");
    manager.set_config_errors(
        errors
            .iter()
            .map(|error| format!("C:/pir-w13-y/mcp.json: {error}"))
            .collect(),
    );
    assert_eq!(manager.config_errors().len(), errors.len());

    let snapshot = snapshot(&manager).await;
    assert_eq!(snapshot.config_errors.len(), errors.len());
    let text = snapshot.format_status();
    assert!(
        text.contains("config error: C:/pir-w13-y/mcp.json:"),
        "the text form names the config problem: {text}"
    );
    assert!(
        text.contains("docs"),
        "the offending server is named: {text}"
    );
    manager.shutdown().await;
}

/// The empty-state message names the config location, even when the only
/// problem is a config error (upstream `formatStatus`'s first arm requires
/// no servers, no errors and no overrides).
#[test]
fn empty_state_names_the_config_files() {
    let status = McpStatusSnapshot {
        servers: Vec::new(),
        config_errors: Vec::new(),
        overridden: Vec::new(),
        agent_dir: PathBuf::from("C:/pir-w13-y"),
    };
    let text = status.format_status();
    assert!(text.starts_with("No MCP servers configured."), "{text}");
    assert!(text.contains("mcp.json"), "{text}");
    assert!(text.contains(".pi/mcp.json"), "{text}");

    // With a config error but no servers, the error lines are still printed.
    let with_error = McpStatusSnapshot {
        servers: Vec::new(),
        config_errors: vec!["C:/pir-w13-y/mcp.json: mcp.json is not valid JSON".to_owned()],
        overridden: Vec::new(),
        agent_dir: PathBuf::from("C:/pir-w13-y"),
    };
    assert_eq!(
        with_error.format_status(),
        "config error: C:/pir-w13-y/mcp.json: mcp.json is not valid JSON"
    );
}

/// Shadowed extension registrations are printed as `overridden:` lines
/// (upstream `formatStatus`'s tail).
#[test]
fn overridden_registrations_are_printed() {
    let status = McpStatusSnapshot {
        servers: Vec::new(),
        config_errors: Vec::new(),
        overridden: vec![
            "\"docs\" registered by /ext/docs.js is overridden by \"docs\" in C:/mcp.json"
                .to_owned(),
        ],
        agent_dir: PathBuf::from("C:/pir-w13-y"),
    };
    assert_eq!(
        status.format_status(),
        "overridden: \"docs\" registered by /ext/docs.js is overridden by \"docs\" in C:/mcp.json"
    );
}

// ---------------------------------------------------------------------------
// (c) Exposure
// ---------------------------------------------------------------------------

/// Exposure resolution through the status row matches
/// `effective_tool_exposure` for the three cases upstream's manager shows: an
/// explicit `direct` server, the `codemode` default, and a `toolExposure`
/// override that differs from its base.
#[tokio::test(flavor = "current_thread")]
async fn exposure_resolution_matches_effective_tool_exposure() {
    let manager = manager();
    manager.registry().register(entry(
        "inline",
        serde_json::json!({ "command": "true", "exposure": "direct" }),
        global_source(),
    ));
    manager.registry().register(entry(
        // No `exposure`: under `builtinTools: false` this is upstream's
        // `codemode` default.
        "scripted",
        serde_json::json!({ "command": "true" }),
        global_source(),
    ));
    manager.registry().register(entry(
        "mixed",
        serde_json::json!({
            "command": "true",
            "exposure": "codemode",
            "toolExposure": { "search": "direct", "deploy_*": "deferred" },
        }),
        global_source(),
    ));

    let snapshot = snapshot(&manager).await;

    let inline = server(&snapshot, "inline");
    assert_eq!(inline.exposure, McpExposure::Direct);
    assert_eq!(
        inline.exposure,
        inline.config.effective_tool_exposure("anything", false)
    );

    let scripted = server(&snapshot, "scripted");
    assert_eq!(
        scripted.exposure,
        McpExposure::Codemode,
        "the upstream default"
    );

    let mixed = server(&snapshot, "mixed");
    assert_eq!(mixed.exposure, McpExposure::Codemode);
    for tool in ["search", "deploy_prod", "other"] {
        assert_eq!(
            mixed.exposure_for_tool(tool),
            mixed.config.effective_tool_exposure(tool, false),
            "tool {tool} must resolve exactly like effective_tool_exposure"
        );
    }
    // And the concrete expectations the resolution has to produce.
    assert_eq!(mixed.exposure_for_tool("search"), McpExposure::Direct);
    assert_eq!(
        mixed.exposure_for_tool("deploy_prod"),
        McpExposure::Deferred
    );
    assert_eq!(mixed.exposure_for_tool("other"), McpExposure::Codemode);
    manager.shutdown().await;
}

/// Under `builtinTools: true` (pir's default) a server without an explicit
/// exposure resolves to `direct` — the flag the plugin records per file, and
/// the one the snapshot accepts.
#[tokio::test(flavor = "current_thread")]
async fn builtin_tools_default_is_honored() {
    let manager = manager();
    manager.registry().register(entry(
        "docs",
        serde_json::json!({ "command": "true" }),
        global_source(),
    ));

    let codemode_default = manager.status_snapshot(|_| false).await;
    assert_eq!(
        server(&codemode_default, "docs").exposure,
        McpExposure::Codemode
    );

    let direct_default = manager.status_snapshot(|_| true).await;
    let docs = server(&direct_default, "docs");
    assert_eq!(docs.exposure, McpExposure::Direct);
    assert!(docs.builtin_tools);
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// (d) The text formatter
// ---------------------------------------------------------------------------

/// `format_status` names every server with its state and tool count, in the
/// upstream content shape: `<name>: <state>, <n> tools (<exposure>)`, with the
/// error indented under the line (index.ts:1021-1041).
#[tokio::test(flavor = "current_thread")]
async fn text_formatter_names_every_server_with_state_and_count() {
    let manager = manager();
    manager.registry().register(entry(
        "alpha",
        serde_json::json!({ "command": "true", "exposure": "direct" }),
        global_source(),
    ));
    manager.registry().register(entry(
        "beta",
        serde_json::json!({ "url": "https://example.invalid/mcp" }),
        McpServerSource::ProjectConfig(PathBuf::from("C:/pir-w13-y/.pi/mcp.json")),
    ));
    manager.registry().register(entry(
        "gamma",
        serde_json::json!({ "command": "true", "enabled": false }),
        global_source(),
    ));
    manager.set_config_errors(vec![
        "C:/pir-w13-y/mcp.json: server \"delta\": missing field `command`".to_owned(),
    ]);

    let mut snapshot = snapshot(&manager).await;
    // A connected server with a discovered tool list, so the count is shown.
    let alpha = snapshot
        .servers
        .iter_mut()
        .find(|server| server.name == "alpha")
        .expect("alpha");
    alpha.state = ConnectionState::Connected;
    alpha.tools = vec!["search".to_owned(), "fetch".to_owned()];
    snapshot.overridden = vec![
        "\"docs\" registered by /ext/docs.js is overridden by \"docs\" in C:/pir-w13-y/mcp.json"
            .to_owned(),
    ];

    let text = snapshot.format_status();
    let lines: Vec<&str> = text.lines().collect();
    // alpha: connected, two tools, its exposure.
    assert_eq!(lines[0], "alpha: connected, 2 tools (direct)");
    // beta: configured (never used), no count while not connected.
    assert_eq!(lines[1], "beta: configured (codemode)");
    // gamma: disabled wins over the connection state.
    assert_eq!(lines[2], "gamma: disabled (codemode)");
    // Then the config error and the shadowed registration.
    assert_eq!(
        lines[3],
        "config error: C:/pir-w13-y/mcp.json: server \"delta\": missing field `command`"
    );
    assert!(lines[4].starts_with("overridden: \"docs\" registered by /ext/docs.js"));
    assert_eq!(lines.len(), 5, "exactly one line per row: {text}");

    // Every server is named.
    for name in ["alpha", "beta", "gamma"] {
        assert!(text.contains(name), "{name} missing from: {text}");
    }
    manager.shutdown().await;
}

/// Singular/plural tool counts and a multi-line error's indentation follow
/// upstream's `formatStatus` (line 1027 and 1035).
#[test]
fn text_formatter_pluralizes_and_indents_errors() {
    let mut status = McpStatusSnapshot {
        servers: vec![McpServerStatus {
            name: "docs".to_owned(),
            config: config(serde_json::json!({ "command": "true" })),
            builtin_tools: false,
            exposure: McpExposure::Direct,
            enabled: true,
            tools: vec!["search".to_owned()],
            transport: "stdio".to_owned(),
            source: global_source(),
            state: ConnectionState::Connected,
        }],
        config_errors: Vec::new(),
        overridden: Vec::new(),
        agent_dir: PathBuf::from("C:/pir-w13-y"),
    };
    assert_eq!(status.format_status(), "docs: connected, 1 tool (direct)");

    status.servers[0].state = ConnectionState::Failed {
        error: "MCP server \"docs\" failed to connect: boom\nstack line 1\nstack line 2".to_owned(),
    };
    // The bare state word on the row, the whole error indented underneath —
    // upstream `formatStatus` (index.ts:1028-1037).
    assert_eq!(
        status.format_status(),
        "docs: failed (direct)\n    \
         MCP server \"docs\" failed to connect: boom\n    \
         stack line 1\n    stack line 2"
    );
}
