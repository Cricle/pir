//! `mcp.json` support: the MCP server configuration schema and loader
//! (upstream `core/mcp-servers.ts`). The MCP client runtime itself is not part
//! of this stream — this module defines the wire format, validates entries,
//! and loads servers from the global agent dir plus the trust-gated project
//! `<cwd>/.pi/mcp.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a server's tools are exposed (upstream `McpExposure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpExposure {
    Codemode,
    Deferred,
    Direct,
    Hidden,
}

/// Fields shared by every server config (upstream `McpServerConfigBase`).
///
/// Unknown keys at the server level are rejected by [`McpServerConfig`]'s
/// manual `Deserialize` (see `KNOWN_SERVER_KEYS` there): serde's flatten
/// machinery would otherwise swallow them instead of applying this struct's
/// `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct McpServerConfigBase {
    /// Default: `direct` when the file's `builtinTools` flag is true (the
    /// default), else upstream's `codemode`. The field itself stays `None`
    /// when the file does not set it, so "explicit vs default" remains
    /// distinguishable (see [`Self::effective_exposure`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<McpExposure>,
    /// What the server offers, in a sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-tool exposure overrides keyed by tool name or `*` pattern.
    #[serde(rename = "toolExposure", skip_serializing_if = "Option::is_none")]
    pub tool_exposure: Option<BTreeMap<String, McpExposure>>,
    /// Set to `false` to keep the entry without connecting. Default: `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Per-request timeout in seconds. Default: 60.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// OAuth client settings for servers without dynamic client registration
/// (upstream `McpOAuthConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct McpOAuthConfig {
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// May reference environment variables (`${NAME}`) or commands (`!cmd`).
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(rename = "callbackPort", skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    #[serde(rename = "callbackUrl", skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(rename = "clientName", skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(rename = "clientRegistration", skip_serializing_if = "Option::is_none")]
    pub client_registration: Option<McpClientRegistration>,
    #[serde(
        rename = "authServerMetadataUrl",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_server_metadata_url: Option<String>,
}

/// Client registration strategy (upstream `clientRegistration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpClientRegistration {
    Dcr,
    Cimd,
}

/// A stdio MCP server (upstream `McpStdioServerConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpStdioServerConfig {
    #[serde(flatten)]
    pub base: McpServerConfigBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Values may reference environment variables (`${NAME}`) or commands
    /// (`!cmd`), resolved by the MCP runtime when the server starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Relative paths resolve against the session working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// An HTTP (streamable) MCP server (upstream `McpHttpServerConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpHttpServerConfig {
    #[serde(flatten)]
    pub base: McpServerConfigBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub url: String,
    /// Values may reference environment variables (`${NAME}`) or commands
    /// (`!cmd`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
    /// Send the token of a pi provider instead of OAuth. Not allowed in
    /// project `mcp.json` files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuthConfig>,
}

/// Provider-token auth (upstream `auth: { provider }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAuthConfig {
    pub provider: String,
}

/// One server entry: stdio or HTTP (upstream `McpServerConfig`).
///
/// Dispatched manually on `command` (stdio) vs `url` (http) because serde's
/// untagged enums do not compose with `#[serde(flatten)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Stdio(McpStdioServerConfig),
    Http(McpHttpServerConfig),
}

/// Every key accepted at the server-entry level (the union of the base
/// fields and the stdio/HTTP wrapper fields). `deny_unknown_fields` on the
/// flattened base cannot see keys the wrapper consumes or buffers
/// (serde's flatten machinery swallows unknown keys), so unknown-key
/// rejection is enforced manually here — otherwise a typo'd `commnd` would
/// surface as a confusing "url must be a non-empty string".
const KNOWN_SERVER_KEYS: &[&str] = &[
    "exposure",
    "description",
    "toolExposure",
    "enabled",
    "timeout",
    "type",
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "headers",
    "oauth",
    "auth",
];

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(map) = value.as_object() {
            for key in map.keys() {
                if !KNOWN_SERVER_KEYS.contains(&key.as_str()) {
                    return Err(serde::de::Error::custom(format!(
                        "unknown field `{key}`, expected one of {}",
                        KNOWN_SERVER_KEYS.join(", ")
                    )));
                }
            }
        }
        let looks_stdio = value.get("command").is_some();
        let looks_http = value.get("url").is_some();
        if looks_stdio && !looks_http {
            return serde_json::from_value(value)
                .map(McpServerConfig::Stdio)
                .map_err(serde::de::Error::custom);
        }
        serde_json::from_value(value)
            .map(McpServerConfig::Http)
            .map_err(serde::de::Error::custom)
    }
}

impl McpServerConfigBase {
    /// The exposure used when the entry does not set `exposure` itself:
    /// `direct` under `builtinTools: true` (pir's default — the tools are
    /// declared to the model like builtin tools), upstream's `codemode`
    /// under `builtinTools: false`.
    ///
    /// An explicit `exposure` always wins; [`McpServerConfigBase::exposure`]
    /// keeps the `None` marker so callers can still tell the two apart.
    pub fn effective_exposure(&self, builtin_tools: bool) -> McpExposure {
        self.exposure.unwrap_or(if builtin_tools {
            McpExposure::Direct
        } else {
            McpExposure::Codemode
        })
    }

    /// The per-tool override that matches `tool_name`, if any: the exact
    /// name first, then the first `*` pattern in key order (upstream
    /// `getMcpToolExposure`).
    pub fn explicit_tool_exposure(&self, tool_name: &str) -> Option<McpExposure> {
        let overrides = self.tool_exposure.as_ref()?;
        if let Some(exposure) = overrides.get(tool_name) {
            return Some(*exposure);
        }
        overrides.iter().find_map(|(pattern, exposure)| {
            (pattern.contains('*') && wildcard_matches(pattern, tool_name)).then_some(*exposure)
        })
    }

    /// The effective exposure of one tool: its `toolExposure` override when
    /// present, else the base [`Self::effective_exposure`].
    pub fn effective_tool_exposure(&self, tool_name: &str, builtin_tools: bool) -> McpExposure {
        self.explicit_tool_exposure(tool_name)
            .unwrap_or_else(|| self.effective_exposure(builtin_tools))
    }

    /// Every exposure surface the entry asks for (upstream
    /// `configuredExposures`): the base exposure under `builtin_tools` plus
    /// each distinct per-tool override.
    pub fn configured_exposures(&self, builtin_tools: bool) -> Vec<McpExposure> {
        let mut exposures = vec![self.effective_exposure(builtin_tools)];
        if let Some(overrides) = &self.tool_exposure {
            for exposure in overrides.values() {
                if !exposures.contains(exposure) {
                    exposures.push(*exposure);
                }
            }
        }
        exposures
    }
}

/// `*`-wildcard match used by `toolExposure` patterns (upstream
/// `createToolNameMatcher`'s regex semantics: `*` matches any run of
/// characters, the whole name must match).
fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let first = parts[0];
    let last = parts[parts.len() - 1];
    if first.len() + last.len() > text.len() || !text.starts_with(first) || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    true
}

impl McpServerConfig {
    /// The shared base fields.
    pub fn base(&self) -> &McpServerConfigBase {
        match self {
            McpServerConfig::Stdio(config) => &config.base,
            McpServerConfig::Http(config) => &config.base,
        }
    }

    /// Whether the entry is enabled (default `true`).
    pub fn is_enabled(&self) -> bool {
        self.base().enabled.unwrap_or(true)
    }

    /// The base exposure under the file-level `builtinTools` default (see
    /// [`McpServerConfigBase::effective_exposure`]).
    pub fn effective_exposure(&self, builtin_tools: bool) -> McpExposure {
        self.base().effective_exposure(builtin_tools)
    }

    /// The effective exposure of one tool (see
    /// [`McpServerConfigBase::effective_tool_exposure`]).
    pub fn effective_tool_exposure(&self, tool_name: &str, builtin_tools: bool) -> McpExposure {
        self.base()
            .effective_tool_exposure(tool_name, builtin_tools)
    }

    /// Every configured exposure surface (see
    /// [`McpServerConfigBase::configured_exposures`]).
    pub fn configured_exposures(&self, builtin_tools: bool) -> Vec<McpExposure> {
        self.base().configured_exposures(builtin_tools)
    }
}

/// The widest effective BASE exposure among `configs` (upstream
/// `syncResourceTools`' precedence: `direct` > `codemode` > `deferred`;
/// `hidden` entries contribute nothing). `None` for an empty list or a list
/// whose servers all resolve to `hidden`.
pub fn effective_resource_exposure(
    configs: &[McpServerConfig],
    builtin_tools: bool,
) -> Option<McpExposure> {
    effective_resource_exposure_for(configs.iter().map(|config| (config, builtin_tools)))
}

/// [`effective_resource_exposure`] with a per-server `builtinTools` value
/// (a config-file entry's shape decides whether the file-level flag applies
/// to it; see [`LoadedMcpConfig::wrapper_shape`]).
pub fn effective_resource_exposure_for<'a>(
    configs: impl IntoIterator<Item = (&'a McpServerConfig, bool)>,
) -> Option<McpExposure> {
    let mut exposure = None;
    for (config, builtin_tools) in configs {
        match config.effective_exposure(builtin_tools) {
            McpExposure::Direct => return Some(McpExposure::Direct),
            McpExposure::Codemode => exposure = Some(McpExposure::Codemode),
            McpExposure::Deferred => {
                if exposure.is_none() {
                    exposure = Some(McpExposure::Deferred);
                }
            }
            McpExposure::Hidden => {}
        }
    }
    exposure
}

/// The parsed server entries of one `mcp.json`: `{ "<serverName>": config }`.
/// On disk this map lives either at the top level (pir's legacy flat shape)
/// or under the upstream `mcpServers` key (see [`parse_mcp_config`]).
pub type McpServersConfig = BTreeMap<String, McpServerConfig>;

/// The keys a project entry may use when it overrides a global server instead
/// of defining one (upstream `OVERRIDE_KEYS`).
pub const OVERRIDE_KEYS: &[&str] = &["enabled", "exposure", "toolExposure"];

/// Which `mcp.json` a body was read from (upstream `readConfigFile`'s
/// `scope`). Project files get the override and credential rules; the global
/// file defines servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConfigScope {
    Global,
    Project,
}

/// Whether two server names share one tool namespace: `mcp__<server>` with
/// `-` replaced by `_` (upstream `mcpNamespace`,
/// `crate::mcp::registry::mcp_namespace`). The constant prefix drops out, so
/// comparing the sanitized names is equivalent. Kept local because
/// `config::mcp` compiles without the `mcp` feature.
fn same_namespace(left: &str, right: &str) -> bool {
    left.replace('-', "_") == right.replace('-', "_")
}

/// Whether an entry overrides a server defined elsewhere instead of defining
/// one: a project entry without `command`, `url`, or `type` (upstream
/// `isOverride`, config.ts:78-80).
fn is_override_entry(value: &serde_json::Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    !map.contains_key("command") && !map.contains_key("url") && !map.contains_key("type")
}

/// Apply a project override to the global server it names (upstream
/// `{ ...base.config, ...value }`): only `enabled`, `exposure`, and
/// `toolExposure` change; every other field — including credentials the
/// project could not set itself — comes from the base entry.
fn merge_override(
    name: &str,
    base: &McpServerConfig,
    value: &serde_json::Map<String, serde_json::Value>,
) -> Result<McpServerConfig, String> {
    let mut merged = base.clone();
    let fields = match &mut merged {
        McpServerConfig::Stdio(config) => &mut config.base,
        McpServerConfig::Http(config) => &mut config.base,
    };
    if let Some(enabled) = value.get("enabled") {
        let enabled = enabled
            .as_bool()
            .ok_or_else(|| format!("server \"{name}\": enabled must be a boolean"))?;
        fields.enabled = Some(enabled);
    }
    if let Some(exposure) = value.get("exposure") {
        let exposure = serde_json::from_value::<McpExposure>(exposure.clone()).map_err(|_| {
            format!("server \"{name}\": exposure must be one of codemode, deferred, direct, hidden")
        })?;
        fields.exposure = Some(exposure);
    }
    if let Some(tool_exposure) = value.get("toolExposure") {
        let Some(map) = tool_exposure.as_object() else {
            return Err(format!(
                "server \"{name}\": toolExposure must map tool names to exposures"
            ));
        };
        let mut parsed = BTreeMap::new();
        for (tool, exposure) in map {
            let exposure =
                serde_json::from_value::<McpExposure>(exposure.clone()).map_err(|_| {
                    format!(
                        "server \"{name}\": toolExposure \"{tool}\" must be one of codemode, deferred, direct, hidden"
                    )
                })?;
            parsed.insert(tool.clone(), exposure);
        }
        fields.tool_exposure = Some(parsed);
    }
    Ok(merged)
}

/// Validate a server entry (upstream `validateMcpServersConfig` rules).
/// Returns the problem, or `None` when valid.
pub fn validate_server(name: &str, config: &McpServerConfig) -> Option<String> {
    let server_name_ok = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if !server_name_ok {
        return Some(format!("server \"{name}\": name must match [A-Za-z0-9_-]+"));
    }
    match config {
        McpServerConfig::Stdio(config) => {
            if config.command.trim().is_empty() {
                return Some(format!(
                    "server \"{name}\": command must be a non-empty string"
                ));
            }
            None
        }
        McpServerConfig::Http(config) => {
            let legacy_sse = config.r#type.as_deref() == Some("sse");
            if legacy_sse {
                return Some(format!(
                    "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
                ));
            }
            if config.url.trim().is_empty() {
                return Some(format!("server \"{name}\": url must be a non-empty string"));
            }
            if let Some(auth) = &config.auth {
                if auth.provider.trim().is_empty() {
                    return Some(format!(
                        "server \"{name}\": auth.provider must be a non-empty string"
                    ));
                }
            }
            None
        }
    }
}

/// The parsed body of one `mcp.json` file: the server entries plus the
/// top-level flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpConfigDocument {
    pub servers: McpServersConfig,
    /// The file's top-level `autoEnableCodemode` (wrapper shape only).
    pub auto_enable_codemode: Option<bool>,
    /// The file's top-level `builtinTools` (wrapper shape only). `None`
    /// means the file does not set it; callers keep their default (`true`).
    pub builtin_tools: Option<bool>,
    /// Whether the body used the upstream wrapper shape (`mcpServers`
    /// present). The legacy flat shape has no flag slot, so its servers keep
    /// upstream's `codemode` default whatever the file-level flag says.
    pub wrapper_shape: bool,
    /// Parse/validation problems.
    pub errors: Vec<String>,
}

/// Parse one `mcp.json` body (BOM tolerated). Accepts both file shapes:
/// - the upstream wrapper `{ "mcpServers": { name: config } }` (preferred —
///   what upstream pi writes and requires, extensions/mcp/config.ts:101-107,
///   optionally alongside the boolean flags `autoEnableCodemode` and
///   `builtinTools`), and
/// - pir's legacy flat map `{ name: config }`, kept for backward
///   compatibility with files pir wrote before the wrapper shape.
///
/// Returns the parsed servers plus one validation problem per invalid entry;
/// entries that fail validation are dropped from the result. The top-level
/// flags are dropped; use [`parse_mcp_config_document_ex`] to read them.
pub fn parse_mcp_config(contents: &str) -> (McpServersConfig, Vec<String>) {
    let document = parse_mcp_config_document_ex(contents);
    (document.servers, document.errors)
}

/// [`parse_mcp_config`] plus the file's top-level `autoEnableCodemode` flag
/// (upstream `LoadedMcpConfig.autoEnableCodemode`): `Some` only when the file
/// is in the wrapper shape and carries a boolean value — an absent flag keeps
/// the caller's default, a wrong type is reported as an error and treated as
/// absent (upstream config.ts:105-106).
pub fn parse_mcp_config_document(contents: &str) -> (McpServersConfig, Option<bool>, Vec<String>) {
    let document = parse_mcp_config_document_ex(contents);
    (
        document.servers,
        document.auto_enable_codemode,
        document.errors,
    )
}

/// [`parse_mcp_config_document`] plus the file's top-level `builtinTools`
/// flag. Both flags are read ONLY in the wrapper shape (`mcpServers`
/// present), matching upstream; in the legacy flat shape every top-level key
/// is a server entry, so a `builtinTools` key there is not a flag.
///
/// `builtinTools` is pir-specific (not upstream): when it resolves to `true`
/// (the default), a server without an explicit `exposure` defaults to
/// `direct` instead of `codemode`. A wrong type is reported as an error and
/// treated as absent.
///
/// This entry point applies the GLOBAL-file rules only; a project body is
/// parsed by [`load_mcp_configs`] with [`McpConfigScope::Project`], which
/// also resolves override-only entries against the global server they name.
pub fn parse_mcp_config_document_ex(contents: &str) -> McpConfigDocument {
    let mut accumulated = McpServersConfig::new();
    parse_mcp_config_document_scoped(contents, McpConfigScope::Global, &mut accumulated)
}

/// Parse one `mcp.json` body with its scope and the servers accepted so far
/// (global file first, then the project file — upstream `readConfigFile`'s
/// `state.servers`).
///
/// A project entry without `command`/`url`/`type` overrides the same-named
/// server defined earlier instead of defining one (upstream config.ts:109-121):
///
/// - only [`OVERRIDE_KEYS`] may be set; any other key is an error
///   (credentials included, so a repository cannot add or redirect them);
/// - the entry needs a base server, `enabled`, `exposure`, and
///   `toolExposure` are merged onto it, and the rest of the base is kept;
/// - an HTTP entry in the project file may not set `auth`: the provider token
///   is a credential, and only the global file may point it at a server.
///
/// Names whose `mcpNamespace` collides (`a-b` vs `a_b`) would share one tool
/// namespace, so the later entry is rejected (upstream config.ts:128-133).
fn parse_mcp_config_document_scoped(
    contents: &str,
    scope: McpConfigScope,
    accumulated: &mut McpServersConfig,
) -> McpConfigDocument {
    let text = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut document = McpConfigDocument::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        document
            .errors
            .push("mcp.json is not valid JSON".to_string());
        return document;
    };
    let Some(map) = value.as_object() else {
        document
            .errors
            .push("mcp.json must be an object of server configs".to_string());
        return document;
    };
    // The `mcpServers` key decides the shape: present (even empty) means the
    // upstream wrapper; absent means the legacy flat map. A non-object
    // `mcpServers` is the upstream loader error (config.ts:101-104).
    document.wrapper_shape = map.contains_key("mcpServers");
    let server_map: Option<&serde_json::Map<String, serde_json::Value>> =
        match map.get("mcpServers") {
            Some(serde_json::Value::Object(wrapper)) => Some(wrapper),
            Some(_) => {
                document
                    .errors
                    .push("expected an object with an \"mcpServers\" object".to_string());
                return document;
            }
            None => None,
        };
    if map.contains_key("mcpServers") {
        // The optional flags (config.ts:105-106 for `autoEnableCodemode`): a
        // boolean is consumed, a wrong type is reported so a typo surfaces
        // instead of being ignored.
        if let Some(auto) = map.get("autoEnableCodemode") {
            match auto.as_bool() {
                Some(value) => document.auto_enable_codemode = Some(value),
                None => document
                    .errors
                    .push("autoEnableCodemode must be a boolean".to_string()),
            }
        }
        if let Some(builtin_tools) = map.get("builtinTools") {
            match builtin_tools.as_bool() {
                Some(value) => document.builtin_tools = Some(value),
                None => document
                    .errors
                    .push("builtinTools must be a boolean".to_string()),
            }
        }
    }
    let entries: Box<dyn Iterator<Item = (&String, &serde_json::Value)>> = match server_map {
        Some(wrapper) => Box::new(wrapper.iter()),
        None => Box::new(map.iter()),
    };
    for (name, entry) in entries {
        if scope == McpConfigScope::Project && is_override_entry(entry) {
            let Some(value) = entry.as_object() else {
                document
                    .errors
                    .push(format!("server \"{name}\" must be an object"));
                continue;
            };
            let Some(base) = accumulated.get(name) else {
                document.errors.push(format!(
                    "server \"{name}\" needs \"command\" or \"url\", or a global server to override"
                ));
                continue;
            };
            let extra: Vec<&String> = value
                .keys()
                .filter(|key| !OVERRIDE_KEYS.contains(&key.as_str()))
                .collect();
            if !extra.is_empty() {
                document.errors.push(format!(
                    "server \"{name}\": an override can only set {}",
                    OVERRIDE_KEYS.join(", ")
                ));
                continue;
            }
            match merge_override(name, base, value) {
                Ok(config) => match validate_server(name, &config) {
                    Some(problem) => document.errors.push(problem),
                    None => {
                        accumulated.insert(name.clone(), config.clone());
                        document.servers.insert(name.clone(), config);
                    }
                },
                Err(problem) => document.errors.push(problem),
            }
            continue;
        }
        match serde_json::from_value::<McpServerConfig>(entry.clone()) {
            Ok(config) => match validate_server(name, &config) {
                Some(problem) => document.errors.push(problem),
                None => {
                    // A project entry may not carry a provider credential
                    // (upstream config.ts:134-137).
                    if scope == McpConfigScope::Project
                        && matches!(&config, McpServerConfig::Http(http) if http.auth.is_some())
                    {
                        document.errors.push(format!(
                            "server \"{name}\": auth is only allowed in the global mcp.json"
                        ));
                        continue;
                    }
                    // Names that differ only in `-` and `_` would share a tool
                    // namespace (upstream config.ts:128-133).
                    if let Some(clash) = accumulated
                        .keys()
                        .find(|other| other.as_str() != name && same_namespace(other, name))
                    {
                        document
                            .errors
                            .push(format!("server \"{name}\" conflicts with \"{clash}\""));
                        continue;
                    }
                    accumulated.insert(name.clone(), config.clone());
                    document.servers.insert(name.clone(), config);
                }
            },
            Err(error) => document.errors.push(format!("server \"{name}\": {error}")),
        }
    }
    document
}

/// The global `mcp.json` path (`<agentDir>/mcp.json`).
pub fn global_mcp_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("mcp.json")
}

/// The project `mcp.json` path (`<cwd>/.pi/mcp.json`); loaded only when the
/// project is trusted.
pub fn project_mcp_path(cwd: &Path) -> PathBuf {
    cwd.join(".pi").join("mcp.json")
}

/// One loaded MCP configuration source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMcpConfig {
    /// Which file the servers came from.
    pub path: PathBuf,
    /// The servers this file contributes, in name order. A project file's
    /// override-only entry (`{ "enabled": false }`) appears here as the
    /// MERGED config of the global server it names (upstream keeps the base
    /// entry and records the override path alongside it), so consumers that
    /// merge files in order already see the effective entry last.
    pub servers: McpServersConfig,
    /// The file's top-level `autoEnableCodemode` (upstream
    /// `LoadedMcpConfig.autoEnableCodemode`); `None` when the file does not
    /// set it (or is not in the wrapper shape), so the caller keeps its
    /// default. Later files override earlier ones (project over global).
    pub auto_enable_codemode: Option<bool>,
    /// The file's top-level `builtinTools` (wrapper shape only); `None` when
    /// the file does not set it, so the caller keeps its default (`true`).
    /// Later files override earlier ones (project over global).
    pub builtin_tools: Option<bool>,
    /// Whether the file used the upstream wrapper shape (`mcpServers`
    /// present). Legacy flat files keep upstream's `codemode` default for
    /// their servers: the shape has no `builtinTools` slot, so the
    /// file-level flag does not apply to them.
    pub wrapper_shape: bool,
    /// Parse/validation problems for this file.
    pub errors: Vec<String>,
}

/// Load the global plus (when trusted) project MCP configs. Later entries win
/// per server name (project over global, matching the settings merge order).
///
/// A project entry without `command`/`url`/`type` is an override of the
/// same-named global server (upstream config.ts:9-12,109-121): the project
/// file's entry carries the merged config, so the last-wins merge applies it;
/// an override may only set `enabled`, `exposure`, and `toolExposure`, and a
/// project entry may not set `auth` at all. Names colliding in their
/// `-`/`_`-insensitive namespace are rejected as conflicts.
///
/// `agent_dir` must be the caller's ALREADY-RESOLVED agent directory, not a
/// fresh environment read: the MCP manager resolves it once at construction
/// and derives its log and OAuth credential store from the same value, so
/// discovering the global file from a second, later resolution would split
/// servers from their sign-in state and diagnostics (see
/// `crate::mcp::manager`'s "One root per manager"). [`crate::mcp::manager::McpManager`]
/// passes its own [`crate::mcp::manager::McpManager::agent_dir`] here.
pub fn load_mcp_configs(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> Vec<LoadedMcpConfig> {
    let mut loaded = Vec::new();
    // The servers accepted so far, global file first (upstream `state.servers`):
    // the project file sees them as override bases and as namespace-conflict
    // candidates.
    let mut accumulated = McpServersConfig::new();
    for (path, scope, allow) in [
        (global_mcp_path(agent_dir), McpConfigScope::Global, true),
        (
            project_mcp_path(cwd),
            McpConfigScope::Project,
            project_trusted,
        ),
    ] {
        if !allow {
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            // An existing but unreadable file (permissions, invalid UTF-8, a
            // directory) is a config-error diagnostic, not "no servers": the
            // user must see why their configured servers vanished.
            Err(error) => {
                loaded.push(LoadedMcpConfig {
                    path,
                    servers: McpServersConfig::new(),
                    auto_enable_codemode: None,
                    builtin_tools: None,
                    wrapper_shape: false,
                    errors: vec![format!("cannot read mcp.json: {error}")],
                });
                continue;
            }
        };
        let document = parse_mcp_config_document_scoped(&contents, scope, &mut accumulated);
        loaded.push(LoadedMcpConfig {
            path,
            servers: document.servers,
            auto_enable_codemode: document.auto_enable_codemode,
            builtin_tools: document.builtin_tools,
            wrapper_shape: document.wrapper_shape,
            errors: document.errors,
        });
    }
    loaded
}
