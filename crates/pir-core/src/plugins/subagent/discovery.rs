//! Agent discovery: the pure rules live in `pir-builtin-subagent`.
//!
//! Frontmatter splitting, tool-list normalization, agent-markdown parsing, the
//! directory walk, the nearest-`.pi/agents` lookup and the scope/upsert merge
//! are all host-independent, so they live in the engine-free crate. This module
//! re-exports them under the historical `super::discovery::…` paths and keeps
//! the two pieces that need the engine:
//!
//! - [`load_agents_from_dir`], whose only extra behavior is the engine-routed
//!   diagnostic for an oversized agent file;
//! - [`BUILTIN_AGENT_MARKDOWN`], the presets embedded from this crate's
//!   `assets/`.

use std::path::Path;

pub use pir_builtin_subagent::{
    agent_list, agent_names_quoted, find_nearest_project_agents_dir, parse_agent_markdown,
    parse_tool_list, split_frontmatter, AgentConfig, AgentFrontmatter, AgentScope, AgentSource,
    Discovery,
};

/// Load every agent markdown in `dir` (sorted, bounded by the engine's context
/// file cap), reporting an oversized file through the routed diagnostic.
///
/// The parsing itself is [`pir_builtin_subagent::load_agents_from_dir`]; this
/// wrapper exists because the size limit and the diagnostic channel are the
/// engine's, and the crate must not depend on either.
pub fn load_agents_from_dir(dir: &Path, source: AgentSource) -> Vec<AgentConfig> {
    let limit = crate::config::settings::MAX_CONTEXT_FILE_BYTES;
    warn_oversized_agents(dir, limit);
    pir_builtin_subagent::load_agents_from_dir(dir, source, limit)
}

/// Emit the oversize diagnostic the crate deliberately cannot.
fn warn_oversized_agents(dir: &Path, limit: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
    {
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if size > limit {
            tracing::warn!(
                path = %path.display(),
                size,
                limit,
                "skipping oversized subagent markdown file"
            );
        }
    }
}

/// Built-in agent definitions shipped with the binary. A fresh install has no
/// `~/.pi/agent/agents` directory, so without these the `subagent` tool can
/// only answer "Available agents: none" and is unusable out of the box.
/// `model` is intentionally omitted: every built-in inherits the parent
/// session's active model, so no provider-specific model id can make a
/// default agent fail to resolve. User/project agents with the same name
/// override these through the normal upsert order.
const BUILTIN_AGENT_MARKDOWN: &[&str] = &[
    include_str!("../../../assets/agents/scout.md"),
    include_str!("../../../assets/agents/planner.md"),
    include_str!("../../../assets/agents/reviewer.md"),
    include_str!("../../../assets/agents/worker.md"),
];

/// Discover the agents a dispatch may use.
///
/// Re-read on every call so mid-session edits take effect; in `both` scope a
/// project agent overrides a same-named user agent. The shipped defaults apply
/// only when no agent file exists anywhere.
pub fn discover_agents(cwd: &Path, scope: AgentScope, user_agents_dir: &Path) -> Discovery {
    pir_builtin_subagent::discover_agents(
        cwd,
        scope,
        user_agents_dir,
        crate::config::settings::MAX_CONTEXT_FILE_BYTES,
        pir_builtin_subagent::builtin_agents_from(BUILTIN_AGENT_MARKDOWN),
    )
}
