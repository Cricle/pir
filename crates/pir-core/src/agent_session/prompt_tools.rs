//! The declared/hidden tool sets a context-derived system prompt renders.
//!
//! Upstream `_rebuildSystemPrompt` passes `selectedTools: validToolNames` and
//! `hiddenTools: [..._hiddenDeclarations]` (agent-session.ts:1705-1731). The
//! same two sets are needed in four places — session build/reload, the
//! session-context snapshot, subagent children, and print mode — so they live
//! here instead of being re-derived at each call site.

use crate::agent::tool_execution::{callable_tools, declared_tools, ToolLoadout, ToolRegistry};
use crate::system_prompt::ToolInfo;

/// The tool list the context-derived system prompt renders: the session's
/// DECLARED (active) tools, in declaration order — upstream
/// `_rebuildSystemPrompt`'s `selectedTools: validToolNames`.
///
/// `hidden`-exposure tools never reach `registry` (the merged registry drops
/// them) and [`declared_tools`] guards them again, so a registered-but-inactive
/// tool (`codemode`/`deferred`, a `default_active: false` core tool) is not
/// advertised either. Rendering exactly this set is what keeps the prompt's
/// `<tools>`/`<rules>` sections identical to the request's
/// `Context.tools` / `loadout.definitions` (D6).
pub(crate) fn declared_tool_infos(registry: &ToolRegistry, active: &[String]) -> Vec<ToolInfo> {
    declared_tools(registry, active)
        .iter()
        .map(|tool| ToolInfo::from_agent_tool(tool.as_ref()))
        .collect()
}

/// The hidden declarations the prompt's `hiddenTools` list carries: the union
/// of every DECLARED tool's Rust [`AgentTool::prepare_loadout`] result
/// (upstream `_rebuildSystemPrompt`'s `hiddenTools: [..._hiddenDeclarations]`,
/// where `_hiddenDeclarations` is what `_applyToolLoadout` collected).
///
/// [`ToolLoadout`] needs the declared and callable sets because a hook may
/// classify by exposure (`codemode` mode `"only"` hides the active `direct`
/// tools that remain callable). The hook is called for declared tools only,
/// exactly as the run loadout and print do.
///
/// JS `prepareLoadout` hooks are async while session construction is
/// synchronous; they stay unrouted here and only affect the per-request
/// declarations. The in-tree hook users that hide declarations (`codemode`)
/// and warm shells (`bash`/`powershell`) are Rust, so the prompt's hidden set
/// is complete for them.
///
/// This is a prompt RENDER, not a run loadout, so it runs under
/// [`crate::coding_agent::tools::shell_pool::without_prewarm`]: the bash hook
/// warms the persistent shell pool from `prepare_loadout`, and the render pass
/// must leave that warm-up for the first real loadout instead of spawning the
/// member (and its dedicated runtime) before any prompt.
///
/// [`AgentTool::prepare_loadout`]: crate::agent::tool_execution::AgentTool::prepare_loadout
pub(crate) fn rust_hidden_declarations(registry: &ToolRegistry, active: &[String]) -> Vec<String> {
    crate::coding_agent::tools::shell_pool::without_prewarm(|| {
        let loadout = ToolLoadout::from_registry(
            registry,
            declared_tools(registry, active),
            callable_tools(registry, active),
        );
        if loadout.declared.is_empty() {
            return Vec::new();
        }
        loadout.rust_hook_changes().1.into_iter().collect()
    })
}
