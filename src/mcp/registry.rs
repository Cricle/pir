//! The MCP server registry and naming helpers (upstream
//! `packages/coding-agent/src/core/mcp-servers.ts`): `mcp__<server>__<tool>`
//! tool naming, `isMcpToolName`, `createToolNameMatcher`, and the registry of
//! servers contributed by `mcp.json` files and `pi.registerMcpServer()`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::config::mcp::McpServerConfig;

/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`
/// (upstream `MAX_TOOL_NAME_LENGTH`).
pub const MAX_TOOL_NAME_LENGTH: usize = 64;

/// Aggregate resource tools (upstream `LIST_MCP_RESOURCES_TOOL` and friends).
pub const LIST_MCP_RESOURCES_TOOL: &str = "list_mcp_resources";
/// Aggregate resource-templates tool.
pub const LIST_MCP_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";
/// Aggregate resource-read tool.
pub const READ_MCP_RESOURCE_TOOL: &str = "read_mcp_resource";

/// The set of MCP resource tools.
pub const MCP_RESOURCE_TOOLS: &[&str] = &[
    LIST_MCP_RESOURCES_TOOL,
    LIST_MCP_RESOURCE_TEMPLATES_TOOL,
    READ_MCP_RESOURCE_TOOL,
];

/// Whether a tool comes from MCP: a server tool (`mcp__<server>__<tool>`) or
/// one of the aggregate resource tools (upstream `isMcpToolName`).
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__") || MCP_RESOURCE_TOOLS.contains(&name)
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by `_`
/// (upstream `mcpNamespace`).
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// The plain `mcp__<server>__<tool>` name: everything outside
/// `[A-Za-z0-9_]` becomes `_` (upstream `createMcpToolName`).
fn plain_mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Short sha-256-derived suffix for deconflicted names (upstream hashes with
/// sha256 and keeps 8 hex characters).
fn name_hash_suffix(server: &str, tool: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(server.as_bytes());
    hasher.update([0u8]);
    hasher.update(tool.as_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The registered tool name for one server tool, sanitized and shortened with
/// a hash suffix when too long or taken (upstream `createMcpToolName`).
pub fn create_mcp_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name = plain_mcp_tool_name(server, tool);
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let hash = name_hash_suffix(server, tool);
    let keep = MAX_TOOL_NAME_LENGTH - hash.len() - 1;
    format!("{}_{hash}", &name[..keep.min(name.len())])
}

/// Assign registry names for one server's whole tool list at once, matching
/// upstream `registerTools`:
/// - tools whose sanitized names collide (`a-b` vs `a_b`) ALL get the hash
///   suffix, so the outcome is order-independent;
/// - an exact duplicate name listed twice keeps the plain name on its FIRST
///   occurrence and hash-suffixes later ones (upstream: `plain` is computed
///   from the deduped tool-name set, and the second registration of the same
///   owner sees the name already taken).
pub fn assign_mcp_tool_names(server: &str, tool_names: &[String]) -> Vec<String> {
    let plain: Vec<String> = tool_names
        .iter()
        .map(|tool| plain_mcp_tool_name(server, tool))
        .collect();
    let mut distinct: std::collections::HashMap<&str, std::collections::BTreeSet<&str>> =
        std::collections::HashMap::new();
    for (index, name) in plain.iter().enumerate() {
        distinct
            .entry(name.as_str())
            .or_default()
            .insert(tool_names[index].as_str());
    }
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    plain
        .iter()
        .enumerate()
        .map(|(index, plain)| {
            // Distinct tool names collapsing to one sanitized name -> hash for
            // every member (upstream's plain-list duplicate check).
            let sanitized_collision = distinct
                .get(plain.as_str())
                .is_some_and(|names| names.len() > 1);
            let suffix = |plain: &str| {
                let hash = name_hash_suffix(server, &tool_names[index]);
                let keep = MAX_TOOL_NAME_LENGTH - hash.len() - 1;
                format!("{}_{hash}", &plain[..keep.min(plain.len())])
            };
            if plain.len() > MAX_TOOL_NAME_LENGTH || sanitized_collision {
                return suffix(plain);
            }
            // First occurrence wins the plain name; a later EXACT duplicate
            // of the same tool name gets the hash suffix (same hash, since
            // the tool name is identical - upstream does the same).
            if seen.insert(plain.clone()) {
                plain.clone()
            } else {
                suffix(plain)
            }
        })
        .collect()
}

/// Compile a tool-name pattern where `*` matches any characters (upstream
/// `toolPatternRegExp`).
fn tool_pattern_regex(pattern: &str) -> Option<regex::Regex> {
    let source = pattern
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    regex::Regex::new(&format!("^{source}$")).ok()
}

/// Whether a tool name matches any of the entries, each an exact name or a
/// `*` pattern (upstream `createToolNameMatcher`). Free function form for
/// one-shot use.
pub fn tool_name_matcher(entries: &[String]) -> ToolNameMatcher {
    ToolNameMatcher::new(entries)
}

/// Whether a tool name matches any of the entries, each an exact name or a
/// `*` pattern (upstream `createToolNameMatcher`).
pub struct ToolNameMatcher {
    names: std::collections::BTreeSet<String>,
    patterns: Vec<regex::Regex>,
}

impl ToolNameMatcher {
    /// Compile a matcher from entries.
    pub fn new(entries: &[String]) -> Self {
        let mut names = std::collections::BTreeSet::new();
        let mut patterns = Vec::new();
        for entry in entries {
            if entry.contains('*') {
                if let Some(regex) = tool_pattern_regex(entry) {
                    patterns.push(regex);
                }
            } else {
                names.insert(entry.clone());
            }
        }
        Self { names, patterns }
    }

    /// Whether `name` matches.
    pub fn matches(&self, name: &str) -> bool {
        self.names.contains(name) || self.patterns.iter().any(|pattern| pattern.is_match(name))
    }
}

/// Exposure of one tool of a server: its `toolExposure` entry (exact name
/// first, then first matching pattern), else the server's `exposure`
/// (upstream `getMcpToolExposure`).
pub fn get_mcp_tool_exposure(
    config: &McpServerConfig,
    tool_name: &str,
) -> crate::config::mcp::McpExposure {
    use crate::config::mcp::McpExposure;
    let base = config.base();
    let default = base.exposure.unwrap_or(McpExposure::Codemode);
    let Some(overrides) = &base.tool_exposure else {
        return default;
    };
    if let Some(exact) = overrides.get(tool_name) {
        return *exact;
    }
    for (pattern, exposure) in overrides {
        if pattern.contains('*')
            && tool_pattern_regex(pattern).is_some_and(|regex| regex.is_match(tool_name))
        {
            return *exposure;
        }
    }
    default
}

/// Where one registered server came from (upstream
/// `RegisteredMcpServer.extensionPath` / the config file path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerSource {
    /// The global `<agentDir>/mcp.json`.
    GlobalConfig(std::path::PathBuf),
    /// The trust-gated project `<cwd>/.pi/mcp.json`.
    ProjectConfig(std::path::PathBuf),
    /// A JS extension's `pi.registerMcpServer()` call.
    Extension(String),
}

/// One registered server (upstream `RegisteredMcpServer`).
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    pub name: String,
    pub config: McpServerConfig,
    pub source: McpServerSource,
}

impl McpServerEntry {
    /// The per-request timeout in milliseconds (default 60s, upstream
    /// `DEFAULT_TIMEOUT_SECONDS`).
    pub fn timeout_ms(&self) -> u64 {
        (self.config.base().timeout.unwrap_or(60)).saturating_mul(1000)
    }
}

/// Change payloads queued for the `mcp_servers_change` event (upstream the
/// runner's `mcp_servers_change` emission).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServersChange {
    /// `registered` or `unregistered`.
    pub action: String,
    /// The affected server name.
    pub server: String,
    /// The full server config for `registered`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<JsonValue>,
    /// Where the server came from.
    pub source: String,
}

/// Registry of configured + registered servers, shared by the `mcp` builtin
/// plugin and the `pir mcp` CLI. Change listeners receive every
/// register/unregister (the plugin forwards them as `mcp_servers_change`).
#[derive(Default)]
pub struct McpServerRegistry {
    servers: std::sync::RwLock<BTreeMap<String, McpServerEntry>>,
    changes: std::sync::RwLock<Vec<McpServersChange>>,
}

impl McpServerRegistry {
    /// An empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register or replace a server, queueing a change event. Config-file
    /// entries win over extension registrations of the same name (upstream:
    /// "A server in `mcp.json` takes precedence over a registered server").
    pub fn register(&self, entry: McpServerEntry) {
        let name = entry.name.clone();
        let config_json = serde_json::to_value(&entry.config).ok();
        let source = source_text(&entry.source);
        let action = {
            let mut servers = self
                .servers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let action = if servers.contains_key(&name) {
                "replaced"
            } else {
                "registered"
            };
            servers.insert(name.clone(), entry);
            action
        };
        self.changes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(McpServersChange {
                action: action.to_owned(),
                server: name,
                config: config_json,
                source,
            });
    }

    /// Register a JS-extension server (`pi.registerMcpServer`): skipped when
    /// a `mcp.json` (global/project) entry of the same *tool namespace*
    /// exists - config files win (upstream `registeredServers` compares
    /// `mcpNamespace`, so the config entry `a-b` also overrides an extension
    /// registration of `a_b`) - and the override is recorded in the change
    /// queue. Returns whether the registration landed.
    pub fn register_extension(&self, entry: McpServerEntry) -> bool {
        if self.config_namespace_clash(&entry.name).is_some() {
            self.changes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(McpServersChange {
                    action: "overridden".to_owned(),
                    server: entry.name.clone(),
                    config: None,
                    source: source_text(&entry.source),
                });
            return false;
        }
        self.register(entry);
        true
    }

    /// The config-file server whose tool namespace equals `name`'s (the
    /// `a-b`/`a_b` collision `mcp_namespace` produces, upstream
    /// `registeredServers`' comparison; an exact name match is the same
    /// namespace). `None` when no `mcp.json` entry claims that namespace.
    pub fn config_namespace_clash(&self, name: &str) -> Option<McpServerEntry> {
        let namespace = mcp_namespace(name);
        self.servers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|existing| {
                mcp_namespace(&existing.name) == namespace
                    && matches!(
                        existing.source,
                        McpServerSource::GlobalConfig(_) | McpServerSource::ProjectConfig(_)
                    )
            })
            .cloned()
    }

    /// Remove a server by name, queueing a change event.
    pub fn unregister(&self, name: &str) {
        let removed = {
            let mut servers = self
                .servers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            servers.remove(name)
        };
        if removed.is_none() {
            return;
        }
        self.changes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(McpServersChange {
                action: "unregistered".to_owned(),
                server: name.to_owned(),
                config: None,
                source: String::new(),
            });
    }

    /// Look up one server.
    pub fn get(&self, name: &str) -> Option<McpServerEntry> {
        self.servers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Snapshot of all servers, in name order.
    pub fn list(&self) -> Vec<McpServerEntry> {
        self.servers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// The extension-registered servers in the upstream `RegisteredMcpServer`
    /// wire shape (`{name, config, extensionPath}`, mcp-servers.ts:307-313) —
    /// the `mcp_servers_change` payload's `servers` array (upstream's
    /// `McpServerRegistry.list()` covers extension registrations only;
    /// mcp.json entries are the builtin's own concern and are not announced).
    pub fn registered_servers(&self) -> Vec<JsonValue> {
        self.servers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|entry| matches!(entry.source, McpServerSource::Extension(_)))
            .map(|entry| {
                serde_json::json!({
                    "name": entry.name,
                    "config": serde_json::to_value(&entry.config).unwrap_or(JsonValue::Null),
                    "extensionPath": match &entry.source {
                        McpServerSource::Extension(path) => path.clone(),
                        // Unreachable after the filter; kept total.
                        McpServerSource::GlobalConfig(_) | McpServerSource::ProjectConfig(_) => {
                            String::new()
                        }
                    },
                })
            })
            .collect()
    }

    /// Drain the queued change events (upstream `mcp_servers_change`
    /// emissions, consumed by the plugin's event flush).
    pub fn drain_changes(&self) -> Vec<McpServersChange> {
        std::mem::take(
            &mut self
                .changes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

fn source_text(source: &McpServerSource) -> String {
    match source {
        McpServerSource::GlobalConfig(path) | McpServerSource::ProjectConfig(path) => {
            path.display().to_string()
        }
        McpServerSource::Extension(path) => path.clone(),
    }
}
