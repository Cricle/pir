//! Capability traits for builtins whose host integration is not expressible
//! through the generic [`AgentPlugin`] hooks alone.
//!
//! The extraction rule is "the host must not know a concrete builtin type".
//! A bare `downcast_ref::<SomeBuiltinPlugin>()` breaks it: it hard-codes one
//! implementation into the host. Each concrete need is therefore named here as
//! a trait, exposed through a defaulted [`AgentPlugin`] accessor
//! ([`AgentPlugin::mcp_capability`](super::plugin::AgentPlugin::mcp_capability)).
//! The builtin returns `Some(self)` from its own `AgentPlugin` impl, so:
//!
//! - the host never names the concrete type;
//! - a replacement builtin supplies the same capability with no host change;
//! - the capability surface is exactly what the trait lists — the builtin's
//!   internals stay private.

#[cfg(feature = "phone")]
use std::sync::Arc;

#[cfg(feature = "mcp")]
use super::host::PluginHost;
#[cfg(feature = "mcp")]
use super::plugin::PluginFuture;

/// The `phone` builtin's session bridge.
///
/// The phone-first UI needs to bind its live session, publish snapshots and
/// observe run boundaries. Every method here is one the session code calls;
/// the relay/WebSocket runtime behind them stays private to the builtin.
#[cfg(feature = "phone")]
pub trait PhoneCapability: Send + Sync {
    /// Bind the live parent session slot (weak, so the session owns its life).
    fn bind_live_session(&self, session: std::sync::Weak<crate::agent_session::AgentSession>);

    /// Ensure a live-session slot exists without binding one.
    fn ensure_live_session_slot(&self);

    fn has_live_session(&self) -> bool;
    fn has_pending_ui_request(&self, session_id: &str) -> bool;
    fn has_bridge(&self) -> bool;
    fn has_published_snapshot(&self) -> bool;

    /// The parallel RPC session pool, when the phone server is running.
    fn phone_pool(&self) -> Option<Arc<crate::plugins::builtin::phone::session::SessionPool>>;

    fn note_run_started(&self);
    fn note_run_settled(&self);
    /// Mark a run that the phone relay drives (its write path), and clear it.
    fn begin_phone_run(&self);
    fn end_phone_run(&self);
    /// The session wrote through the phone bridge (mirror suppression).
    fn note_phone_write(&self);
    fn publish_session_list(&self, list: serde_json::Value);
    fn publish_snapshot(&self, snapshot: serde_json::Value);
    fn publish_status(&self, status: serde_json::Value);
    fn publish_catalog(&self, catalog: serde_json::Value);
    fn publish_client_error(&self, message: &str);
}

/// The `tool-repair` builtin's pipeline config.
///
/// The loop's `message_end` seam needs the lazily loaded package config; it
/// registers no tool/command, so this is its whole host surface.
#[cfg(feature = "tool-repair")]
pub trait ToolRepairCapability: Send + Sync {
    /// The package config (env flag + `extensions/pi-tool-repair.json`).
    fn tool_repair_config(&self) -> &crate::plugins::tool_repair::config::ToolRepairConfig;
}

/// The `mcp` builtin's manager handoff.
///
/// The host's request-auth layer and the MCP tool path need the session's
/// manager handle, and every registry rebuild must flush the
/// `mcp_servers_change` entries the builtin queued.
#[cfg(feature = "mcp")]
pub trait McpCapability: Send + Sync {
    /// The session's MCP manager handle.
    fn mcp_manager_handle(&self) -> crate::plugins::host::McpManagerHandle;

    /// Flush `mcp_servers_change` entries: one copy to the JS face, one to
    /// the native plugins.
    fn flush_mcp_server_changes<'a>(&'a self, host: &'a PluginHost) -> PluginFuture<'a, ()>;

    /// `ensureDiscoveryActive` (upstream index.ts:498-537): which tools the
    /// run's loadout must add for deferred MCP discovery.
    fn discovery_activation(
        &self,
        registry: &crate::agent::tool_execution::ToolRegistry,
        active: &[String],
    ) -> crate::plugins::builtin::mcp::DiscoveryActivation;

    /// Refresh the resource-server set; `true` when it changed.
    fn refresh_resource_servers(&self) -> PluginFuture<'_, bool>;

    /// Wait for one MCP tool call's discovery round-trip.
    fn wait_for_tool_call<'a>(
        &'a self,
        registry: &'a crate::agent::tool_execution::ToolRegistry,
        tool_name: &'a str,
        input: &'a serde_json::Value,
    ) -> PluginFuture<'a, Vec<String>>;

    /// The `mcp_servers` system-prompt section for one prompt build, or `None`
    /// when no server is configured.
    fn prompt_section(&self) -> Option<String>;
}
