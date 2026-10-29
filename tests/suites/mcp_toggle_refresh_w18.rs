//! W18 QFIX-MCP: `/mcp enable|disable|remove` and exposure changes must
//! refresh the plugin registry themselves.
//!
//! The bug: the native actions updated the manager and the session
//! activation, but never marked the host-visible `tools_dirty` flag and
//! `PluginHost::dispatch_command` never ran `refresh_registry_if_changed`
//! after a native handler. Consequences pinned here:
//!
//! * disable → an explicit rebuild registered the bridges `hidden`, but the
//!   enable right after did not rebuild, so the bridges stayed hidden;
//! * remove forgot the discovery snapshot, so a later enable could not
//!   recreate the bridges at all (`/mcp remove` leaves the file entry in
//!   place, so enable restores it);
//! * an exposure change only moved on a manual rebuild.
//!
//! Every scenario drives the REAL native command through
//! [`AgentSession::dispatch_plugin_command`] (the TUI worker seam) against the
//! existing mock stdio server fixture, with the server defined in `mcp.json`
//! so the remove → enable restore path reads a real file.

#![cfg(feature = "mcp")]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::ThinkingLevel;
use pir::config::mcp::McpExposure;
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost};

use super::mcp_residuals_w13::{prompt_test_model, runtime, Tree, MOCK_HEALTHY};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The global `mcp.json` entry for the mock stdio server (wrapper shape, the
/// one the config loader and `persist_server_patch` both understand).
fn write_mcp_json(dir: &Path) {
    let (command, args) = super::mcp_residuals_w13::mock_spawn();
    let server = json!({
        "command": command,
        "args": args,
        "env": {
            "PIR_W13_MOCK": MOCK_HEALTHY,
            "PIR_W13_DELAY_MS": "0",
        },
        "timeout": 60,
    });
    std::fs::write(
        dir.join("mcp.json"),
        json!({ "mcpServers": { "docs": server } }).to_string(),
    )
    .expect("write mcp.json");
}

fn build_session(cwd: &Path, plugin: Arc<McpPlugin>) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: prompt_test_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: Some("mcp toggle refresh test".to_owned()),
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![plugin as Arc<dyn AgentPlugin>],
    )
}

/// The `mcp__<server>__*` bridge the registration pass assigned.
fn bridge_name(host: &PluginHost, server: &str) -> String {
    let namespace = format!("mcp__{server}__");
    host.registry()
        .resolved_tools()
        .into_iter()
        .map(|(def, _)| def.name.clone())
        .find(|name| name.starts_with(&namespace))
        .unwrap_or_else(|| panic!("no registered bridge for {server}"))
}

/// The bridge's registered exposure string in the host registry, if any.
fn bridge_exposure(host: &PluginHost, bridge: &str) -> Option<String> {
    host.registry()
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == bridge)
        .map(|(def, _)| def.exposure.clone())
}

// ---------------------------------------------------------------------------
// (a) disable -> rebuild -> enable restores visibility/callability
// ---------------------------------------------------------------------------

#[test]
fn disable_then_rebuild_then_enable_restores_the_bridge_without_a_manual_rebuild() {
    let tree = Tree::new("toggle-enable");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        write_mcp_json(&tree.0);
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        let session = build_session(&tree.0, Arc::clone(&plugin));
        let host = session.plugin_host_arc().expect("plugin host");
        // The host build runs the plugin's config pass (the server entry
        // exists only then); discovery publishes the tool snapshot and the
        // rebuild registers the bridges.
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        host.rebuild_registry().await;
        let bridge = bridge_name(&host, "docs");

        // The configured bridge starts direct/declared/callable.
        assert_eq!(bridge_exposure(&host, &bridge).as_deref(), Some("direct"));
        let (registry, declared) = session.active_tool_snapshot(Some(&host));
        assert!(declared.contains(&bridge), "starts declared: {declared:?}");
        assert!(registry.get(&bridge).is_some(), "starts callable");

        // `/mcp disable docs`: the command's own refresh hides the bridge
        // (no explicit rebuild in the test yet).
        let outcome = session
            .dispatch_plugin_command("mcp", "disable docs")
            .await
            .expect("native mcp command");
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(
            bridge_exposure(&host, &bridge).as_deref(),
            Some("hidden"),
            "disable hides the bridge during the command's refresh"
        );
        let (registry, declared) = session.active_tool_snapshot(Some(&host));
        assert!(!declared.contains(&bridge), "hidden is not declared");
        assert!(registry.get(&bridge).is_none(), "hidden is not callable");

        // An explicit rebuild keeps the disabled state (the file/override is
        // the truth).
        host.rebuild_registry().await;
        assert_eq!(bridge_exposure(&host, &bridge).as_deref(), Some("hidden"));

        // `/mcp enable docs`: the command re-discovers and its refresh
        // rebuilds, so the bridge is visible/callable WITHOUT another manual
        // rebuild.
        let outcome = session
            .dispatch_plugin_command("mcp", "enable docs")
            .await
            .expect("native mcp command");
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(
            bridge_exposure(&host, &bridge).as_deref(),
            Some("direct"),
            "enable restores the configured exposure"
        );
        let (registry, declared) = session.active_tool_snapshot(Some(&host));
        assert!(
            declared.contains(&bridge),
            "the enabled bridge is declared: {declared:?}"
        );
        assert!(
            registry.get(&bridge).is_some(),
            "the enabled bridge is callable"
        );

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// (b) remove -> enable restores discovery and registration
// ---------------------------------------------------------------------------

#[test]
fn remove_then_enable_restores_discovery_and_registration() {
    let tree = Tree::new("toggle-remove");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        write_mcp_json(&tree.0);
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        let session = build_session(&tree.0, Arc::clone(&plugin));
        let host = session.plugin_host_arc().expect("plugin host");
        // The host build runs the plugin's config pass (the server entry
        // exists only then); discovery publishes the tool snapshot and the
        // rebuild registers the bridges.
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        host.rebuild_registry().await;
        let bridge = bridge_name(&host, "docs");
        assert!(bridge_exposure(&host, &bridge).is_some());

        // `/mcp remove docs`: unregistered, snapshot forgotten, and the
        // command's refresh drops the bridge (the config pass must not
        // resurrect it through the session override).
        let outcome = session
            .dispatch_plugin_command("mcp", "remove docs")
            .await
            .expect("native mcp command");
        assert!(outcome.ok, "{outcome:?}");
        assert!(
            plugin.manager().registry().get("docs").is_none(),
            "remove unregisters the live entry"
        );
        assert_eq!(
            bridge_exposure(&host, &bridge),
            None,
            "the removed server's bridge leaves the host registry"
        );

        // `/mcp enable docs` restores the file entry AND re-discovers it, so
        // the rebuild that follows the command registers the bridges again.
        let outcome = session
            .dispatch_plugin_command("mcp", "enable docs")
            .await
            .expect("native mcp command");
        assert!(outcome.ok, "{outcome:?}");
        assert!(
            plugin.manager().registry().get("docs").is_some(),
            "enable restores the file entry"
        );
        assert_eq!(
            bridge_exposure(&host, &bridge).as_deref(),
            Some("direct"),
            "the restored bridge is registered direct"
        );
        let (registry, declared) = session.active_tool_snapshot(Some(&host));
        assert!(declared.contains(&bridge), "restored bridge is declared");
        assert!(
            registry.get(&bridge).is_some(),
            "restored bridge is callable"
        );

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// (c) an exposure change takes effect on the next command/refresh
// ---------------------------------------------------------------------------

#[test]
fn exposure_change_takes_effect_on_the_next_native_command() {
    let tree = Tree::new("toggle-exposure");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        write_mcp_json(&tree.0);
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        let session = build_session(&tree.0, Arc::clone(&plugin));
        let host = session.plugin_host_arc().expect("plugin host");
        // The host build runs the plugin's config pass (the server entry
        // exists only then); discovery publishes the tool snapshot and the
        // rebuild registers the bridges.
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        host.rebuild_registry().await;
        let bridge = bridge_name(&host, "docs");
        assert_eq!(bridge_exposure(&host, &bridge).as_deref(), Some("direct"));

        // The manager's exposure picker path (no text subcommand exists).
        plugin
            .set_exposure_for_test("docs", McpExposure::Codemode)
            .expect("exposure change");

        // No manual rebuild: the next native command's refresh applies it.
        let outcome = session
            .dispatch_plugin_command("mcp", "list")
            .await
            .expect("native mcp command");
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(
            bridge_exposure(&host, &bridge).as_deref(),
            Some("deferred"),
            "codemode exposure re-registers the bridge deferred"
        );
        let (registry, declared) = session.active_tool_snapshot(Some(&host));
        assert!(
            !declared.contains(&bridge),
            "a deferred bridge is not declared: {declared:?}"
        );
        assert!(
            registry.get(&bridge).is_some(),
            "a deferred bridge stays callable from discovery tools"
        );

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}
