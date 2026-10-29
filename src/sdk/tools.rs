//! Tool factories for SDK embedders (upstream `createCodingTools`,
//! `createReadTool`, `createReadOnlyTools`, ... — `core/sdk.ts:336-415`).
//!
//! The built-in coding tools are registered by
//! [`crate::coding_agent::tools::register_core_tools`] under per-tool cargo
//! features. These factories hand out the same tool objects the CLI registers
//! (one implementation, no duplicated schemas or descriptions); a factory
//! whose tool is not compiled into the build fails with the honest
//! missing-feature diagnostic instead of returning a silently toolless SDK.
//!
//! Deviation from upstream: the factories take no `cwd`. Upstream captures
//! the working directory in the tool object; pir's built-in tools resolve it
//! from the run's [`ToolContext`](crate::agent::tool_execution::ToolContext),
//! so one tool object serves every session cwd. A host that wants a fixed cwd
//! builds the session with
//! [`CreateAgentSessionOptions::cwd`](crate::sdk::CreateAgentSessionOptions).

use std::sync::Arc;

use crate::agent::tool_execution::{AgentTool, ToolRegistry};
use crate::coding_agent::tools::{register_core_tools, CORE_TOOL_NAMES};

use super::error::SdkError;

/// One registry carrying exactly the core tools compiled into this build.
fn core_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

/// The cargo feature that gates one tool name (for the missing-feature
/// diagnostic).
fn tool_feature(name: &str) -> &'static str {
    match name {
        "read" => "tool-read",
        "bash" => "tool-bash",
        "powershell" => "tool-powershell",
        "edit" => "tool-edit",
        "write" => "tool-write",
        "ls" => "tool-ls",
        "grep" | "find" => "grep-find",
        "background_status" | "background_abort" => "tool-background",
        _ => "tools",
    }
}

fn missing_tool(name: &str) -> SdkError {
    SdkError::Unsupported(format!(
        "the `{name}` tool is not compiled into this build (enable the `{}` cargo feature)",
        tool_feature(name)
    ))
}

/// Every core coding tool compiled into this build (upstream
/// `createCodingTools`).
///
/// A trimmed build returns the subset
/// [`CORE_TOOL_NAMES`] reports — the exact set `register_core_tools` registers,
/// never a silent partial set from a feature that is off for one requested
/// tool. Fails only when the build compiled no core tool at all.
pub fn create_coding_tools() -> Result<Vec<Arc<dyn AgentTool>>, SdkError> {
    let registry = core_registry();
    if registry.is_empty() {
        return Err(SdkError::Unsupported(
            "this build compiled no built-in coding tools (enable the `tools` cargo feature or an \
             individual `tool-*` leaf)"
                .to_owned(),
        ));
    }
    Ok(registry.iter().cloned().collect())
}

/// The read-only tool set (upstream `createReadOnlyTools`: read, grep, find,
/// ls). Fails when any requested tool is not compiled in.
pub fn create_read_only_tools() -> Result<Vec<Arc<dyn AgentTool>>, SdkError> {
    create_tools(&["read", "grep", "find", "ls"])
}

/// One named core tool, or the missing-feature error.
pub fn create_tool(name: &str) -> Result<Arc<dyn AgentTool>, SdkError> {
    core_registry().get(name).ok_or_else(|| missing_tool(name))
}

/// `read` (upstream `createReadTool`).
pub fn create_read_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("read")
}

/// `bash` (upstream `createBashTool`).
pub fn create_bash_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("bash")
}

/// `powershell` (upstream `createPowerShellTool`).
pub fn create_powershell_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("powershell")
}

/// `edit` (upstream `createEditTool`).
pub fn create_edit_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("edit")
}

/// `write` (upstream `createWriteTool`).
pub fn create_write_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("write")
}

/// `grep` (upstream `createGrepTool`).
pub fn create_grep_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("grep")
}

/// `find` (upstream `createFindTool`).
pub fn create_find_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("find")
}

/// `ls` (upstream `createLsTool`).
pub fn create_ls_tool() -> Result<Arc<dyn AgentTool>, SdkError> {
    create_tool("ls")
}

/// Resolve a list of tool names in registry order (requested order is not
/// preserved: the factories mirror the registry's own ordering).
fn create_tools(names: &[&str]) -> Result<Vec<Arc<dyn AgentTool>>, SdkError> {
    let registry = core_registry();
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| registry.get(name).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(SdkError::Unsupported(format!(
            "the requested tools are not compiled into this build: {} (enable the matching \
             `tool-*` cargo features)",
            missing
                .iter()
                .map(|name| format!("`{name}` ({})", tool_feature(name)))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(registry
        .iter()
        .filter(|tool| names.contains(&tool.name()))
        .cloned()
        .collect())
}

/// Whether a core tool is compiled into this build.
pub fn has_core_tool(name: &str) -> bool {
    CORE_TOOL_NAMES.contains(&name)
}
