//! W18 QFIX-MCP: MCP bridge activation is a REGISTRATION-TIME union that
//! must be prunable, and `--exclude-tools` patterns must survive late
//! discovery.
//!
//! Upstream `setEnabled(false)` calls `hideTools(server)` and
//! `setExposure(...)` filters the server's non-`direct` tools out of the
//! active set (extensions/mcp/index.ts); the direct-exposure bridges
//! themselves are activated on registration. pir's `McpPlugin::recompute`
//! used to start from the session's current active set and only PUSH names,
//! so a bridge it had materialized into the `setActiveTools` cell stayed
//! declared after its server was disabled or its exposure no longer resolved
//! to `direct` — every call then failed with `McpError::ServerDisabled`.
//!
//! `--exclude-tools 'mcp__docs__*'` is resolved by the shared selection
//! resolver into `-name` modifiers for the names registered at that moment;
//! the `*` entries were dropped, so a bridge discovered afterwards was not
//! excluded and was unioned into the active set anyway. Upstream consults a
//! name MATCHER (`_excludedTools`, `createToolNameMatcher`) at registration.
//!
//! Every scenario builds a real session around
//! [`crate::mcp_residuals_w13::McpPlugin`] and the existing mock stdio server
//! fixture (no sleeps, no network): a `direct` server (`docs`) and, where a
//! recompute WRITE is needed to materialize the bridges into the active-set
//! cell, a second `codemode` server whose discovery tool activation forces
//! one.

#![cfg(feature = "mcp")]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pir::agent::tool_execution::{ToolExposure, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::ThinkingLevel;
use pir::coding_agent::tool_selection::{resolve_tool_selection_with_availability, AvailableTool};
use pir::coding_agent::ToolSelection;
use pir::config::mcp::{McpExposure, McpServerConfig};
use pir::mcp::manager::McpManager;
use pir::mcp::registry::{assign_mcp_tool_names, McpServerEntry};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{instantiate_builtin, AgentPlugin, PluginHost};

use super::mcp_residuals_w13::{prompt_test_model, register_mock, runtime, Tree, MOCK_HEALTHY};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Rewrite one registry entry's `enabled` flag (what `/mcp disable` does to
/// the live entry before its recompute).
fn set_enabled(manager: &McpManager, server: &str, enabled: bool) {
    let entry = manager.registry().get(server).expect("registered server");
    let mut config = entry.config.clone();
    match &mut config {
        McpServerConfig::Stdio(stdio) => stdio.base.enabled = Some(enabled),
        McpServerConfig::Http(http) => http.base.enabled = Some(enabled),
    }
    manager
        .registry()
        .register(McpServerEntry { config, ..entry });
}

/// Rewrite one registry entry's base exposure (what `/mcp setExposure` /
/// the manager's exposure picker does before its recompute).
fn set_exposure(manager: &McpManager, server: &str, exposure: McpExposure) {
    let entry = manager.registry().get(server).expect("registered server");
    let mut config = entry.config.clone();
    match &mut config {
        McpServerConfig::Stdio(stdio) => stdio.base.exposure = Some(exposure),
        McpServerConfig::Http(http) => http.base.exposure = Some(exposure),
    }
    manager
        .registry()
        .register(McpServerEntry { config, ..entry });
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

fn build_session(
    cwd: &Path,
    plugins: Vec<Arc<dyn AgentPlugin>>,
    tool_selection: Option<ToolSelection>,
) -> Arc<AgentSession> {
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
            system_prompt: Some("mcp prune test".to_owned()),
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection,
        },
        plugins,
    )
}

fn codemode_builtin() -> Arc<dyn AgentPlugin> {
    instantiate_builtin("codemode").expect("codemode builtin")
}

/// The session's DECLARED names (the loadout the model sees) at this instant.
fn declared(session: &AgentSession, host: &PluginHost) -> Vec<String> {
    session.active_tool_snapshot(Some(host)).1
}

// ---------------------------------------------------------------------------
// (a) A disabled server's bridges leave the active set and the declaration
// ---------------------------------------------------------------------------

#[test]
fn disabling_a_server_prunes_its_materialized_bridges() {
    let tree = Tree::new("prune-disable");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        register_mock(plugin.manager(), "docs", MOCK_HEALTHY, 60, 0, &[]);
        register_mock(plugin.manager(), "other", MOCK_HEALTHY, 60, 0, &[]);
        set_exposure(plugin.manager(), "other", McpExposure::Codemode);
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        let session = build_session(
            &tree.0,
            vec![codemode_builtin(), plugin.clone() as Arc<dyn AgentPlugin>],
            None,
        );
        let host = session.plugin_host_arc().expect("plugin host");
        let bridge = bridge_name(&host, "docs");

        // The codemode activation wrote the active set, materializing the
        // direct bridge into the raw `setActiveTools` cell.
        let active = plugin.active_tools_snapshot().expect("context");
        assert!(
            active.iter().any(|name| name == &bridge),
            "the direct bridge starts active: {active:?}"
        );
        assert!(
            plugin.auto_activated_bridges().contains(&bridge),
            "the plugin owns the materialized bridge: {:?}",
            plugin.auto_activated_bridges()
        );

        // `/mcp disable docs`: the live entry flips, then recompute runs.
        set_enabled(plugin.manager(), "docs", false);
        let active = plugin
            .recompute_discovery_activation()
            .expect("bound context");
        assert!(
            !active.iter().any(|name| name == &bridge),
            "a disabled server's bridge leaves the active set: {active:?}"
        );
        assert!(
            !plugin.auto_activated_bridges().contains(&bridge),
            "the plugin no longer owns the pruned bridge"
        );

        // The rebuild re-registers the bridge as `hidden` (upstream
        // `hideTools`), so it is neither declared nor callable.
        host.rebuild_registry().await;
        let (registry, declared_names) = session.active_tool_snapshot(Some(&host));
        assert!(
            !declared_names.iter().any(|name| name == &bridge),
            "a disabled server's bridge is not declared: {declared_names:?}"
        );
        let host_def = host
            .registry()
            .resolved_tools()
            .into_iter()
            .find(|(def, _)| def.name == bridge)
            .map(|(def, _)| def.exposure.clone())
            .expect("the bridge stays registered (hidden, upstream hideTools)");
        assert_eq!(host_def, "hidden", "the disabled server's bridge is hidden");
        assert!(
            registry.get(&bridge).is_none(),
            "a hidden tool is removed from the run registry (not callable)"
        );

        // A second recompute (the rebuild-tail path) stays pruned.
        let active = plugin
            .recompute_discovery_activation()
            .expect("bound context");
        assert!(!active.iter().any(|name| name == &bridge));

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// (b) A direct -> codemode exposure change removes the direct bridge
// ---------------------------------------------------------------------------

#[test]
fn changing_exposure_to_codemode_prunes_the_direct_bridge() {
    let tree = Tree::new("prune-exposure");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        register_mock(plugin.manager(), "docs", MOCK_HEALTHY, 60, 0, &[]);
        register_mock(plugin.manager(), "other", MOCK_HEALTHY, 60, 0, &[]);
        set_exposure(plugin.manager(), "other", McpExposure::Codemode);
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        let session = build_session(
            &tree.0,
            vec![codemode_builtin(), plugin.clone() as Arc<dyn AgentPlugin>],
            None,
        );
        let host = session.plugin_host_arc().expect("plugin host");
        let bridge = bridge_name(&host, "docs");
        assert!(plugin
            .active_tools_snapshot()
            .expect("context")
            .iter()
            .any(|name| name == &bridge));

        // `/mcp setExposure docs codemode`, then the immediate recompute.
        set_exposure(plugin.manager(), "docs", McpExposure::Codemode);
        let active = plugin
            .recompute_discovery_activation()
            .expect("bound context");
        assert!(
            !active.iter().any(|name| name == &bridge),
            "a non-direct bridge leaves the active set: {active:?}"
        );
        assert!(!plugin.auto_activated_bridges().contains(&bridge));

        // The next rebuild registers it with the deferred exposure; it stays
        // undeclared because nothing activated it.
        host.rebuild_registry().await;
        let (registry, declared_names) = session.active_tool_snapshot(Some(&host));
        assert!(
            !declared_names.iter().any(|name| name == &bridge),
            "a codemode-exposure tool is not declared: {declared_names:?}"
        );
        assert_eq!(
            registry.get(&bridge).expect("bridge").exposure(),
            ToolExposure::Deferred
        );

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// (c) An explicit --tools selection is authoritative
// ---------------------------------------------------------------------------

#[test]
fn an_explicitly_selected_bridge_is_never_pruned() {
    let tree = Tree::new("prune-explicit");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        register_mock(plugin.manager(), "docs", MOCK_HEALTHY, 60, 0, &[]);
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        let bridge = assign_mcp_tool_names("docs", &["work".to_owned()]).remove(0);
        let selection = ToolSelection {
            enabled: vec![bridge.clone()],
            modifiers: Vec::new(),
            uses_default_tools: false,
        };
        let session = build_session(
            &tree.0,
            vec![plugin.clone() as Arc<dyn AgentPlugin>],
            Some(selection),
        );
        let host = session.plugin_host_arc().expect("plugin host");
        assert_eq!(bridge_name(&host, "docs"), bridge);

        set_enabled(plugin.manager(), "docs", false);
        let active = plugin
            .recompute_discovery_activation()
            .expect("bound context");
        assert!(
            active.iter().any(|name| name == &bridge),
            "an explicit --tools name is never removed by the plugin: {active:?}"
        );
        assert!(
            plugin.auto_activated_bridges().is_empty(),
            "an explicit selection is not plugin-owned"
        );

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// (d) A settings defaultTools entry naming an MCP tool stays
// ---------------------------------------------------------------------------

#[test]
fn a_settings_selected_bridge_survives_an_unrelated_prune() {
    let tree = Tree::new("prune-settings");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        register_mock(plugin.manager(), "docs", MOCK_HEALTHY, 60, 0, &[]);
        register_mock(plugin.manager(), "other", MOCK_HEALTHY, 60, 0, &[]);
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        let docs_bridge = assign_mcp_tool_names("docs", &["work".to_owned()]).remove(0);
        let other_bridge = assign_mcp_tool_names("other", &["work".to_owned()]).remove(0);
        // The settings `defaultTools` list is a DEFAULT-tools derivation, so
        // the plugin may prune names it owns but never the user's selection.
        // Both servers expose `direct` bridges; only the disabled one goes.
        let selection = ToolSelection {
            enabled: vec!["read".to_owned(), docs_bridge.clone()],
            modifiers: Vec::new(),
            uses_default_tools: true,
        };
        let session = build_session(
            &tree.0,
            vec![plugin.clone() as Arc<dyn AgentPlugin>],
            Some(selection),
        );
        let host = session.plugin_host_arc().expect("plugin host");
        assert_eq!(bridge_name(&host, "docs"), docs_bridge);

        // Disabling the OTHER server must prune only that server's bridge.
        set_enabled(plugin.manager(), "other", false);
        let active = plugin
            .recompute_discovery_activation()
            .expect("bound context");
        assert!(
            active.iter().any(|name| name == &docs_bridge),
            "the settings-selected bridge stays: {active:?}"
        );
        assert!(
            !active.iter().any(|name| name == &other_bridge),
            "the disabled server's bridge is pruned: {active:?}"
        );

        host.rebuild_registry().await;
        let declared_names = declared(&session, &host);
        assert!(
            declared_names.iter().any(|name| name == &docs_bridge),
            "still declared after the rebuild: {declared_names:?}"
        );

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// (e)/(f) `--exclude-tools`: globs survive late discovery, exact names too
// ---------------------------------------------------------------------------

/// Resolve an exclude-only selection against a set that does NOT contain the
/// MCP bridge (the bridge registers later, like discovery after startup).
fn late_exclude_selection(pattern: &str) -> ToolSelection {
    let available = vec![AvailableTool::name_only("read")];
    resolve_tool_selection_with_availability(
        None,
        &[],
        &[pattern.to_owned()],
        false,
        false,
        &available,
        &pir::config::settings::DEFAULT_TOOL_NAMES,
    )
    .expect("selection resolves")
    .expect("a selection is requested")
}

#[test]
fn a_glob_exclusion_covers_a_bridge_discovered_after_startup() {
    let tree = Tree::new("prune-glob");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        let selection = late_exclude_selection("mcp__docs__*");
        assert!(
            selection
                .modifiers
                .iter()
                .any(|entry| entry == "-mcp__docs__*"),
            "the pattern is carried as a matcher entry: {:?}",
            selection.modifiers
        );

        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        register_mock(plugin.manager(), "docs", MOCK_HEALTHY, 60, 0, &[]);
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        let session = build_session(
            &tree.0,
            vec![plugin.clone() as Arc<dyn AgentPlugin>],
            Some(selection),
        );
        let host = session.plugin_host_arc().expect("plugin host");
        let bridge = bridge_name(&host, "docs");

        let active = plugin.active_tools_snapshot().expect("context");
        assert!(
            !active.iter().any(|name| name == &bridge),
            "a late-discovered bridge matching the glob is not activated: {active:?}"
        );
        assert!(
            plugin.auto_activated_bridges().is_empty(),
            "an excluded bridge is never owned"
        );
        let declared_names = declared(&session, &host);
        assert!(!declared_names.iter().any(|name| name == &bridge));

        // A recompute (the rebuild-tail path) does not resurrect it.
        let active = plugin
            .recompute_discovery_activation()
            .expect("bound context");
        assert!(!active.iter().any(|name| name == &bridge));

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}

#[test]
fn an_exact_exclusion_still_excludes_a_late_bridge() {
    let tree = Tree::new("prune-exact");
    runtime().block_on(async {
        let _guard = crate::common::AgentDirEnvGuard::at(&tree.0, &[]).await;
        let selection = late_exclude_selection("mcp__docs__work");
        assert!(
            selection
                .modifiers
                .iter()
                .any(|entry| entry == "-mcp__docs__work"),
            "the exact name keeps its `-name` entry: {:?}",
            selection.modifiers
        );

        let plugin = Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0));
        register_mock(plugin.manager(), "docs", MOCK_HEALTHY, 60, 0, &[]);
        plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        let session = build_session(
            &tree.0,
            vec![plugin.clone() as Arc<dyn AgentPlugin>],
            Some(selection),
        );
        let host = session.plugin_host_arc().expect("plugin host");
        let bridge = bridge_name(&host, "docs");
        assert_eq!(bridge, "mcp__docs__work");

        let active = plugin.active_tools_snapshot().expect("context");
        assert!(
            !active.iter().any(|name| name == &bridge),
            "the exact exclusion still wins: {active:?}"
        );
        assert!(!declared(&session, &host).iter().any(|name| name == &bridge));

        session.shutdown().await;
        plugin.manager().shutdown().await;
    });
}
