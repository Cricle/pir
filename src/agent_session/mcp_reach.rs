//! MCP reach wiring: the two host-side consumers of a tool's `reachesTools`
//! declaration, upstream `packages/coding-agent/src/extensions/mcp/index.ts`.
//!
//! The MCP side's decisions live in the `mcp` builtin
//! ([`crate::plugins::builtin::McpPlugin`]) as pure functions of the tool
//! registry plus the server registry. This module is the **application** of
//! those decisions at the places upstream applies them:
//!
//! 1. [`AgentSession::ensure_discovery_active`] — upstream
//!    `ensureDiscoveryActive` (index.ts:498-537): activate the discovery tool
//!    a server's configured exposure asks for (`codemode` for `codemode`
//!    unless `autoEnableCodemode` is false, `tool_search` for `deferred`), and
//!    warn once when neither is reachable. Upstream runs it in its
//!    `session_start` handler; pir runs it at the run's loadout build, which
//!    fires after `session_start` and again on every settle iteration, so
//!    servers registered mid-session (a `pi.registerMcpServer` from a JS
//!    handler) are covered too. Idempotent: already-active names are skipped.
//! 2. [`AgentSession::tool_call_wait_hook`] — the `tool_call` handler
//!    (index.ts:1191-1209): before a batch executes, wait for the servers its
//!    calls may reach so their tools are registered in time. The loop calls
//!    the hook at its tool seam ([`crate::agent::agent_loop::LoopDeps`]);
//!    [`crate::plugins::builtin::McpPlugin::wait_for_tool_call`] carries the
//!    reach → server resolution and swallows connection failures. The hook
//!    then anchors MCP discovery publication: it refreshes the plugin
//!    registry ([`PluginHost::refresh_registry_if_changed`]), so a discovered
//!    tool snapshot that a mid-run `tools/list_changed` refresh marked dirty
//!    ([`crate::mcp::manager::McpManager`]) is rebuilt into the registry
//!    before the next loadout. This is the seam for it because the `mcp`
//!    plugin's interest is narrowed to `session_start`, so the host's
//!    tail-of-emit refresh runs only for events some plugin handles; the
//!    manager's dirty sites cannot do it themselves (no host reference), and
//!    the per-batch seam is where upstream waits for a call's servers, i.e.
//!    exactly where the registry must be current.
//!
//! ## Why the activation is written to the session's override cell
//!
//! Upstream's `pi.setActiveTools([...pi.getActiveTools(), ...activate])`
//! writes ONE active-set store. pir has three stores in a fixed precedence
//! chain ([`AgentSession::resolve_active_override`]): the session cell (the
//! `tool_search` builtin's matches), the JS runtime's `setActiveTools` store,
//! and the host-resolved `--tools` selection. The activation is written to the
//! **session cell** as the union of the effectively-active names with the
//! names to activate, because:
//!
//! - the JS store feeds `merge_plugin_tools`, whose override PRUNES every
//!   merged name outside it (core tools included) — writing the activation
//!   there would delete the core tools from the run registry;
//! - the cell wins the chain, so writing it with the union keeps the resolved
//!   explicit selection (`--tools`) active instead of shadowing it.
//!
//! The union is computed from the same resolution the loadout uses, so the
//! result is exactly "the currently active set plus the discovery tools", the
//! upstream semantics. A JS `setActiveTools` call AFTER the activation is
//! masked by the cell (the `tool_search` builtin's documented trade-off for
//! the same store); the next `ensure_discovery_active` re-unions whatever the
//! cell holds.

use super::*;

#[cfg(feature = "mcp")]
use crate::plugins::builtin::McpPlugin;

impl AgentSession {
    /// Upstream `ensureDiscoveryActive` (index.ts:498-537), applied at the
    /// run's loadout build. Returns the names this call activated (empty when
    /// there was nothing to do), so hosts and tests can observe it.
    #[cfg(feature = "mcp")]
    pub(super) fn ensure_discovery_active(&self) -> Vec<String> {
        let Some(plugins) = self.plugins.as_deref() else {
            return Vec::new();
        };
        let Some(mcp) = mcp_plugin_of(plugins) else {
            return Vec::new();
        };
        // The registry the run will see: base (core) tools + plugin tools,
        // hidden tools removed, the JS override's prune applied — the exact
        // surface the loadout's active set is drawn from.
        let registry = self.merged_tool_registry(plugins);
        let active = registry.active_names(self.resolve_active_override(Some(plugins)).as_deref());
        let decision = mcp.discovery_activation(&registry, &active);
        let mut reachable = active;
        for name in &decision.activate {
            if !reachable.iter().any(|active| active == name) {
                reachable.push(name.clone());
            }
        }
        // Upstream only writes when there is something to activate
        // (`if (activate.length > 0) pi.setActiveTools(...)`).
        if !decision.activate.is_empty() {
            *self
                .active_tools_override
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reachable.clone());
        }
        if !decision.needs_codemode && !decision.needs_tool_search {
            return decision.activate;
        }
        // A registered reaching tool that is now active makes the exposure
        // reachable; nothing to warn about.
        if decision.has_codemode
            && reachable
                .iter()
                .any(|name| name == crate::plugins::builtin::CODEMODE_TOOL_NAME)
        {
            return decision.activate;
        }
        if decision.has_tool_search
            && reachable
                .iter()
                .any(|name| name == crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME)
        {
            return decision.activate;
        }
        // One warning per session (upstream `warnedUnreachable`, reset at
        // `session_start`). Upstream emits `ctx.ui.notify(..., "warning")`;
        // headless hosts print the same `[ext] warning: ...` line the
        // presentation-notification drain uses.
        if self.mcp_unreachable_warned.swap(true, Ordering::SeqCst) {
            return decision.activate;
        }
        let reason =
            if decision.needs_codemode && decision.has_codemode && !decision.auto_enable_codemode {
                " (autoEnableCodemode is false)"
            } else {
                ""
            };
        crate::ext_diag!(
            "[ext] warning: MCP tools are only reachable from the codemode or tool_search tool, but neither is active{reason}; they cannot be called."
        );
        decision.activate
    }

    /// The MCP reach wait hook for the loop
    /// ([`crate::agent::agent_loop::LoopDeps::tool_call_wait`]), or `None`
    /// when the session has no plugin host or no `mcp` builtin in it.
    ///
    /// The hook resolves the plugin once per batch (the host's dispatch list
    /// is fixed, so the lookup is stable) and awaits
    /// [`McpPlugin::wait_for_tool_call`] per call, sequentially — upstream's
    /// handler awaits `waitForServers` for the batch's call.
    #[cfg(feature = "mcp")]
    pub fn tool_call_wait_hook(&self) -> Option<crate::agent::agent_loop::ToolCallWaitFn> {
        let plugins = self.plugins.clone()?;
        mcp_plugin_of(&plugins)?;
        Some(Arc::new(
            move |registry: Arc<ToolRegistry>,
                  calls: Vec<crate::agent::tool_batch::AgentToolCall>| {
                let plugins = Arc::clone(&plugins);
                Box::pin(async move {
                    let Some(mcp) = mcp_plugin_of(&plugins) else {
                        return;
                    };
                    for call in &calls {
                        mcp.wait_for_tool_call(&registry, &call.tool_name, &call.arguments)
                            .await;
                    }
                    // Discovery-publication anchor (module docs item 2): the
                    // wait may have connected servers, and an earlier MCP call
                    // in this run may have refreshed the discovered tool
                    // snapshot (`tools/list_changed` -> `tools_dirty`). The
                    // cache refresh runs first, mirroring `McpPlugin::on_event`
                    // ahead of the host's rebuild (the resource tools'
                    // exposure feeds that rebuild's registration pass), then
                    // the host consumes the flags and rebuilds.
                    mcp.refresh_resource_servers().await;
                    plugins.refresh_registry_if_changed().await;
                })
            },
        ))
    }
}

/// Fallbacks for builds without the `mcp` feature: there is no `mcp` builtin
/// to downcast to, so no server can ever be registered — nothing to activate
/// and no server to wait for.
#[cfg(not(feature = "mcp"))]
impl AgentSession {
    pub(super) fn ensure_discovery_active(&self) -> Vec<String> {
        Vec::new()
    }

    pub fn tool_call_wait_hook(&self) -> Option<crate::agent::agent_loop::ToolCallWaitFn> {
        None
    }
}

/// The `mcp` builtin in a plugin host's ordered dispatch list, if loaded.
#[cfg(feature = "mcp")]
fn mcp_plugin_of(plugins: &PluginHost) -> Option<&McpPlugin> {
    plugins
        .plugins()
        .iter()
        .find_map(|plugin| plugin.as_any()?.downcast_ref::<McpPlugin>())
}
