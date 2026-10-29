//! The `tool-repair` builtin's loop wiring: the `message_end` repair hook the
//! session (and print mode) hands to [`LoopDeps::message_end_repair`],
//! upstream `pi-tool-repair`'s `message_end` handler applied natively (scope
//! doc §2).
//!
//! This module is the feature-independent `None`-able seam: it stays compiled
//! in every build, while the implementation it names (the `plugins::tool_repair`
//! module — plugin + pure repair core) is type-level gated with the
//! `tool-repair` cargo feature. Without the feature both hook builders below
//! return `None` (the builtin cannot be registered, so that is the same value
//! the feature-on build produces for a host without the plugin), and the
//! agent loop's `message_end` shape is unchanged.
//!
//! Ordering inside one assistant `message_end` (the chain the loop runs):
//! the JS `message_end` handler chain first — its chained replacement is
//! applied to the message (`agent_loop::emit_message_end`, upstream
//! `emitMessageEnd` plus the agent-session application) — then this native
//! repair hook over that result, then the terminal assignment, the transcript
//! push, persistence, and the `validate_arguments` preflight of the extracted
//! tool calls. That is upstream's single ordered handler chain, with the
//! native builtin standing in for the JS `pi-tool-repair` handler; the two
//! never run together (native builtin wins; the JS package is only loaded
//! when the builtin is disabled —
//! [`crate::plugins::loader::tool_repair_precedence`]).
//!
//! The plugin owns the pipeline
//! (`ToolRepairPlugin` → `repair_message_with_grammar`); this module only
//! applies it at the loop seam, the same shape the `mcp` builtin's reach hook
//! rides ([`crate::agent_session::mcp_reach`]): the host resolves the concrete
//! builtin by downcast, resolves the run's live state (model id, active-tool
//! schemas), and calls the pure repair core. The plugin's config stays lazily
//! loaded (`OnceLock`): the FIRST hook invocation triggers the
//! `extensions/pi-tool-repair.json` read, and only when a hook exists at all —
//! no I/O at construction, none on the repair-free path.
//!
//! No-plugin invariance: a host without a plugin host, or without the
//! `tool-repair` builtin in its dispatch list, builds no hook and the loop
//! sees `None` (zero change, agent_loop.rs) — and so does a build without the
//! `tool-repair` feature (the builtin is not compiled there).

use super::*;

#[cfg(feature = "tool-repair")]
use crate::plugins::tool_repair::repair::LiveToolSchema;
#[cfg(feature = "tool-repair")]
use crate::plugins::tool_repair::ToolRepairPlugin;

impl AgentSession {
    /// The `tool-repair` builtin's `message_end` repair hook for the loop
    /// ([`crate::agent::agent_loop::LoopDeps::message_end_repair`]), or `None`
    /// when the session has no plugin host or no `tool-repair` builtin in it.
    ///
    /// The hook resolves the plugin once per message (the host's dispatch list
    /// is fixed, so the lookup is stable), reads the lazily-loaded config
    /// (first access performs the `extensions/pi-tool-repair.json` I/O), and
    /// runs the full pipeline over the finalized message in place.
    #[cfg(feature = "tool-repair")]
    pub fn tool_repair_hook(&self) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
        let plugins = self.plugins.clone()?;
        tool_repair_plugin_of(&plugins)?;
        Some(Arc::new(
            move |context: crate::agent::agent_loop::MessageEndRepairContext| {
                let plugins = Arc::clone(&plugins);
                Box::pin(async move {
                    let plugin = tool_repair_plugin_of(&plugins)?;
                    let config = plugin.config();
                    let model = context.model.as_ref().map(|model| model.id.clone());
                    let known_tools: std::collections::BTreeSet<String> = context
                        .active_tools
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect();
                    let mut message = context.message;
                    let outcome = crate::plugins::tool_repair::repair::repair_message_with_grammar(
                        &mut message,
                        model.as_deref(),
                        &context
                            .active_tools
                            .iter()
                            .map(|(name, parameters)| {
                                LiveToolSchema::new(name.clone(), parameters.clone())
                            })
                            .collect::<Vec<LiveToolSchema>>(),
                        &config.grammar,
                        &known_tools,
                        &crate::agent::tool_scheduler::tool_repair_schema_check,
                        config.debug,
                    );
                    // Commit only a changed message: `Unchanged` and
                    // `Unrepairable` both leave the stream's bytes intact (the
                    // unrepairable candidate is discarded so the host surfaces
                    // the original validation error).
                    if outcome.changed() {
                        Some(message)
                    } else {
                        None
                    }
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

/// The `tool-repair` builtin in a plugin host's ordered dispatch list, if
/// loaded (the same lookup `agent_session::mcp_reach` makes for the `mcp`
/// builtin).
#[cfg(feature = "tool-repair")]
fn tool_repair_plugin_of(plugins: &PluginHost) -> Option<&ToolRepairPlugin> {
    plugins
        .plugins()
        .iter()
        .find_map(|plugin| plugin.as_any()?.downcast_ref::<ToolRepairPlugin>())
}

/// The `tool-repair` builtin's `message_end` repair hook for a bare plugin
/// host — the print host's twin of [`AgentSession::tool_repair_hook`]: print
/// mode drives [`crate::agent::agent_loop`] itself and builds no
/// [`AgentSession`], so the host-shaped hook is what
/// `print_loop_deps` wires. Same resolution, same lazy config access, same
/// commit-only-when-changed contract.
#[cfg(all(feature = "print", feature = "tool-repair"))]
pub(crate) fn host_tool_repair_hook(
    plugins: Option<&Arc<PluginHost>>,
) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
    let plugins = Arc::clone(plugins?);
    tool_repair_plugin_of(&plugins)?;
    Some(Arc::new(
        move |context: crate::agent::agent_loop::MessageEndRepairContext| {
            let plugins = Arc::clone(&plugins);
            Box::pin(async move {
                let plugin = tool_repair_plugin_of(&plugins)?;
                let config = plugin.config();
                let model = context.model.as_ref().map(|model| model.id.clone());
                let known_tools: std::collections::BTreeSet<String> = context
                    .active_tools
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect();
                let mut message = context.message;
                let outcome = crate::plugins::tool_repair::repair::repair_message_with_grammar(
                    &mut message,
                    model.as_deref(),
                    &context
                        .active_tools
                        .iter()
                        .map(|(name, parameters)| {
                            LiveToolSchema::new(name.clone(), parameters.clone())
                        })
                        .collect::<Vec<LiveToolSchema>>(),
                    &config.grammar,
                    &known_tools,
                    &crate::agent::tool_scheduler::tool_repair_schema_check,
                    config.debug,
                );
                if outcome.changed() {
                    Some(message)
                } else {
                    None
                }
            })
        },
    ))
}

/// The same print-host hook in a build without the `tool-repair` feature: no
/// builtin exists to drive the pipeline, so the host seam is an always-`None`
/// hook holder. Same signature as the feature-on function, so
/// `coding_agent/print.rs` (`print_loop_deps`) stays feature-independent.
#[cfg(all(feature = "print", not(feature = "tool-repair")))]
pub(crate) fn host_tool_repair_hook(
    _plugins: Option<&Arc<PluginHost>>,
) -> Option<crate::agent::agent_loop::MessageEndRepairFn> {
    None
}
