//! Independent, host-neutral core of the `mcp` builtin.
//!
//! The MCP runtime — server lifecycle, rmcp transports, OAuth, tool exposure
//! and the registry bridge — stays host-side because it owns process and
//! network state. This crate owns the pure naming rules the host and the relay
//! both need: the `<server>__<tool>` namespace and the `mcp__<server>__<tool>`
//! tool prefix, plus exposure parsing.
//!
//! It depends on `pir-plugin-api` and third-party crates only — never on
//! `pir-core`.

#![deny(unsafe_code)]

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// Prefix of every model-facing MCP tool name.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// Separator between server and tool, and inside a namespaced server id.
pub const MCP_NAME_SEPARATOR: &str = "__";

/// Exposure mode of one MCP server or tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpExposure {
    /// Declared to the model every turn.
    #[default]
    Inline,
    /// Reachable through `codemode`/`tool_search`.
    Deferred,
    /// Registered but not declared.
    Hidden,
}

impl McpExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }

    /// Parse a wire value; unknown spellings fall back to [`Self::Inline`].
    pub fn parse(value: &str) -> Self {
        match value {
            "deferred" => Self::Deferred,
            "hidden" => Self::Hidden,
            _ => Self::Inline,
        }
    }
}

/// Namespace of one server's tools: `mcp__<server>` with `-` replaced by `_`
/// (upstream `mcpNamespace`). Case is preserved and every other character is
/// left alone — [`plain_tool_name`] is the one that sanitizes.
pub fn mcp_namespace(server: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{}", server.replace('-', "_"))
}

/// The plain `mcp__<server>__<tool>` name: everything outside `[A-Za-z0-9_]`
/// becomes `_` (upstream `createMcpToolName`).
pub fn plain_tool_name(server: &str, tool: &str) -> String {
    format!("{}{MCP_NAME_SEPARATOR}{tool}", mcp_namespace(server))
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The model-facing tool name for one server tool (the plain, sanitized form;
/// the registry applies length/conflict suffixes on top).
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    plain_tool_name(server, tool)
}

/// Whether `name` is an MCP tool name.
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with(MCP_TOOL_PREFIX)
}

/// Split a model-facing MCP tool name into `(server, tool)`.
pub fn split_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(MCP_TOOL_PREFIX)?
        .split_once(MCP_NAME_SEPARATOR)
}

/// Resolve the effective exposure for one tool: a per-tool override wins over
/// the server default, and an explicitly name-selected tool is inline.
pub fn resolve_tool_exposure(
    server_default: McpExposure,
    tool_override: Option<McpExposure>,
    selected: bool,
) -> McpExposure {
    if selected {
        return McpExposure::Inline;
    }
    tool_override.unwrap_or(server_default)
}

/// Minimal protocol-facing shell for the mcp builtin.
pub struct McpPlugin {
    info: PluginInfo,
}

impl McpPlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(PLUGIN_NAME),
        }
    }
}

impl BuiltinPlugin for McpPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = McpPlugin,
    name = "mcp",
    factory = new_mcp_plugin,
}
