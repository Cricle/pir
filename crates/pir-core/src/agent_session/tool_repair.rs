//! The `tool-repair` builtin's `message_end` seam: the engine adapter around
//! the engine-free repair pipeline.
//!
//! The pure pipeline (aliases, config, grammar recovery, live-schema repair,
//! phantom normalization) lives in `pir-builtin-tool-repair`. This module owns
//! only what needs the engine: finding the builtin in a host's dispatch list,
//! resolving the run's live state (model id, active-tool schemas), and
//! translating the engine's `AssistantMessage` to and from the crate's neutral
//! DTO.
//!
//! ## Why a DTO round-trip instead of a second implementation
//!
//! The repair passes read and rewrite `content`, `stop_reason` and
//! `error_message` only. The crate mirrors exactly those fields and preserves
//! every other message key (`api`, `provider`, `model`, `usage`, `diagnostics`,
//! provider-specific extras) in a flattened map, so one
//! `to_json_value` / `from_json_value` pair round-trips a core message without
//! losing provider state. The alternative — a generic "repair closure" over the
//! core type — would put the engine's message model back into the crate.
//!
//! ## Cost
//!
//! The round-trip happens once per finalized assistant message, and only when
//! the builtin is loaded and `message_end_repair` is wired. A session without
//! the builtin builds no hook at all (the `None` case both feature shapes
//! return), so the repair-free path pays nothing.

use super::*;

/// Convert one engine message to the crate DTO, run `pass`, and convert back.
///
/// Returns `None` when the pass reported no committed change (the caller then
/// keeps the original message byte-for-byte) or when the message cannot be
/// represented as the DTO — which cannot happen for a message this engine
/// produced, but is handled as "no change" rather than a panic on the reply
/// path.
#[cfg(feature = "tool-repair")]
fn run_repair_pass<F>(
    message: &mut crate::ai::types::AssistantMessage,
    run: F,
) -> Option<crate::ai::types::AssistantMessage>
where
    F: FnOnce(&mut pir_builtin_tool_repair::types::AssistantMessage) -> bool,
{
    let dto = pir_builtin_tool_repair::types::AssistantMessage::from_json_value(
        serde_json::to_value(&*message).ok()?,
    )
    .ok()?;
    let mut repaired = dto;
    if !run(&mut repaired) {
        return None;
    }
    let value = repaired.to_json_value();
    let mut restored: crate::ai::types::AssistantMessage = serde_json::from_value(value).ok()?;
    // `Deferred` handles are engine-side only (the DTO preserves them as an
    // opaque extra field, but a repair never touches them); keep the original
    // so an in-flight deferred reply stays usable.
    restored.deferred = message.deferred.take();
    Some(restored)
}

impl AgentSession {
    /// The `tool-repair` builtin's `message_end` repair hook for the loop
    /// ([`crate::agent::agent_loop::LoopDeps::message_end_repair`]), or `None`
    /// when the session has no plugin host or no `tool-repair` builtin in it.
    ///
    /// The hook resolves the plugin once per message (the host's dispatch list
    /// is fixed, so the lookup is stable), reads the lazily-loaded config
    /// (first access performs the `extensions/pi-tool-repair.json` I/O), and
    /// runs the full pipeline over the finalized message.
    #[cfg(feature = "tool-repair")]
    pub fn tool_repair_hook(&self) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
        let plugins = self.plugins.clone()?;
        tool_repair_plugin_of(&plugins)?;
        Some(Arc::new(
            move |context: crate::agent::agent_loop::MessageEndRepairContext| {
                let plugins = Arc::clone(&plugins);
                Box::pin(async move {
                    let plugin = tool_repair_plugin_of(&plugins)?;
                    let config = plugin.tool_repair_config();
                    let model = context.model.as_ref().map(|model| model.id.clone());
                    let repair_tools = context.active_tools.repair_tool_set();
                    let mut message = context.message;
                    let repaired = run_repair_pass(&mut message, |dto| {
                        pir_builtin_tool_repair::repair::repair_message_with_grammar(
                            dto,
                            model.as_deref(),
                            repair_tools.schemas.as_slice(),
                            &config.grammar,
                            &repair_tools.names,
                            &crate::agent::tool_scheduler::tool_repair_schema_check,
                            config.debug,
                        )
                        // Commit only a changed message: `Unchanged` and
                        // `Unrepairable` both leave the stream's bytes intact
                        // (the unrepairable candidate is discarded so the host
                        // surfaces the original validation error).
                        .changed()
                    });
                    repaired
                })
            },
        ))
    }

    /// The same accessor in a build without the `tool-repair` feature: the
    /// builtin (and its plugin type) is not compiled, so no host can hold it
    /// and the hook is always `None`. The signature is identical to the
    /// feature-on method, so the loop's `message_end_repair` slot (and every
    /// caller: `agent_session/prompt_loop.rs`) stays feature-independent.
    #[cfg(not(feature = "tool-repair"))]
    pub fn tool_repair_hook(&self) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
        None
    }
}

/// The `tool-repair` builtin's capability in a plugin host's ordered dispatch
/// list, if loaded (the same capability lookup `agent_session::mcp_reach`
/// makes for the `mcp` builtin).
#[cfg(feature = "tool-repair")]
fn tool_repair_plugin_of(
    plugins: &PluginHost,
) -> Option<&dyn crate::plugins::capabilities::ToolRepairCapability> {
    plugins
        .plugins()
        .iter()
        .find_map(|plugin| plugin.tool_repair_capability())
}

/// The `tool-repair` builtin's `message_end` repair hook for a bare plugin
/// host — the print host's twin of [`AgentSession::tool_repair_hook`]: print
/// mode drives [`crate::agent::agent_loop`] itself and builds no
/// [`AgentSession`], so the host-shaped hook is what
/// `print_loop_deps` wires. Same resolution, same lazy config access, same
/// commit-only-when-changed contract.
#[cfg(all(feature = "print", feature = "tool-repair"))]
pub fn host_tool_repair_hook(
    plugins: Option<&Arc<PluginHost>>,
) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
    let plugins = Arc::clone(plugins?);
    tool_repair_plugin_of(&plugins)?;
    Some(Arc::new(
        move |context: crate::agent::agent_loop::MessageEndRepairContext| {
            let plugins = Arc::clone(&plugins);
            Box::pin(async move {
                let plugin = tool_repair_plugin_of(&plugins)?;
                let config = plugin.tool_repair_config();
                let model = context.model.as_ref().map(|model| model.id.clone());
                let repair_tools = context.active_tools.repair_tool_set();
                let mut message = context.message;
                run_repair_pass(&mut message, |dto| {
                    pir_builtin_tool_repair::repair::repair_message_with_grammar(
                        dto,
                        model.as_deref(),
                        repair_tools.schemas.as_slice(),
                        &config.grammar,
                        &repair_tools.names,
                        &crate::agent::tool_scheduler::tool_repair_schema_check,
                        config.debug,
                    )
                    .changed()
                })
            })
        },
    ))
}

/// The same print-host hook in a build without the `tool-repair` feature: no
/// builtin exists to drive the pipeline, so the host seam is an always-`None`
/// hook holder. Same signature as the feature-on function, so
/// `coding_agent/print.rs` (`print_loop_deps`) stays feature-independent.
#[cfg(all(feature = "print", not(feature = "tool-repair")))]
pub fn host_tool_repair_hook(
    _plugins: Option<&Arc<PluginHost>>,
) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
    None
}
