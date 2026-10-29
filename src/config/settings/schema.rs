use super::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Nested settings types (upstream settings-schema.ts)
// ---------------------------------------------------------------------------

/// Transport protocol selection (upstream `transport`).
///
/// Wire values are lowercase (`auto`, `sse`, `websocket`, `websocket-cached`),
/// matching the published settings schema and the `websockets` → `transport`
/// migration. Without `rename_all` these serialized as `Auto`/`Sse`/…, so a
/// real settings file carrying `"transport": "sse"` deserialized to nothing and
/// was silently dropped with a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportSetting {
    Auto,
    Sse,
    Websocket,
    #[serde(rename = "websocket-cached")]
    WebsocketCached,
}

/// Where `/share` uploads the session (upstream `sessionShareDestination`,
/// `cea257439`). `Auto` prefers Radius when it is authenticated and otherwise
/// creates a secret GitHub gist; `Gist` always gists; `Radius` requires Radius
/// authentication and never falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionShareDestination {
    #[default]
    Auto,
    Gist,
    Radius,
}

/// Steering queueing mode (upstream `steeringMode`/`followUpMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    All,
    OneAtATime,
}

/// Follow-up queueing mode (upstream `followUpMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FollowUpMode {
    All,
    OneAtATime,
}

/// Per-model compaction token overrides (upstream `CompactionModelOverride`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompactionModelOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
}

/// Compaction settings (upstream `CompactionSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<BTreeMap<String, CompactionModelOverride>>,
}

impl CompactionSettings {
    /// The merged compaction defaults (upstream `SETTINGS_DEFAULTS.compaction`).
    pub fn defaults() -> Self {
        Self {
            enabled: Some(defaults::COMPACTION_ENABLED),
            reserve_tokens: Some(defaults::COMPACTION_RESERVE_TOKENS),
            keep_recent_tokens: Some(defaults::COMPACTION_KEEP_RECENT_TOKENS),
            model_overrides: None,
        }
    }
}

/// Branch summary settings (upstream `BranchSummarySettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_prompt: Option<bool>,
}

/// Provider/SDK retry settings (upstream `ProviderRetrySettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetrySettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
}

/// Retry settings (upstream `RetrySettings`; the legacy `maxDelayMs` field is
/// migrated into `provider.maxRetryDelayMs` on load and never stored).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRetrySettings>,
}

/// Timeout-style setting: a non-negative millisecond count or `"disabled"`
/// (upstream `timeoutSetting`); `0` and `"disabled"` both disable the timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutSetting {
    Milliseconds(u64),
    Disabled,
}

impl Serialize for TimeoutSetting {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TimeoutSetting::Milliseconds(ms) => serializer.serialize_u64(*ms),
            TimeoutSetting::Disabled => serializer.serialize_str("disabled"),
        }
    }
}

impl<'de> Deserialize<'de> for TimeoutSetting {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Number(number) => number
                .as_u64()
                .map(TimeoutSetting::Milliseconds)
                .ok_or_else(|| serde::de::Error::custom("expected a non-negative number")),
            Value::String(text) if text == "disabled" => Ok(TimeoutSetting::Disabled),
            _ => Err(serde::de::Error::custom(
                "expected a non-negative number or \"disabled\"",
            )),
        }
    }
}

impl TimeoutSetting {
    /// The effective millisecond value, or `None` when disabled.
    pub fn effective_ms(self) -> Option<u64> {
        match self {
            TimeoutSetting::Milliseconds(ms) if ms > 0 => Some(ms),
            _ => None,
        }
    }
}

/// `bool | "auto"` capability switches (upstream terminal `hyperlinks` /
/// `trueColor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoBool {
    Enabled(bool),
    Auto,
}

impl Serialize for AutoBool {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AutoBool::Enabled(enabled) => serializer.serialize_bool(*enabled),
            AutoBool::Auto => serializer.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for AutoBool {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Bool(enabled) => Ok(AutoBool::Enabled(enabled)),
            Value::String(text) if text == "auto" => Ok(AutoBool::Auto),
            _ => Err(serde::de::Error::custom("expected a boolean or \"auto\"")),
        }
    }
}

/// Inline image protocol override (upstream terminal `images`):
/// `"kitty"`, `"iterm2"`, `"auto"`, or `false` (off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalImagesMode {
    Kitty,
    Iterm2,
    Auto,
    Off,
}

impl Serialize for TerminalImagesMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TerminalImagesMode::Kitty => serializer.serialize_str("kitty"),
            TerminalImagesMode::Iterm2 => serializer.serialize_str("iterm2"),
            TerminalImagesMode::Auto => serializer.serialize_str("auto"),
            TerminalImagesMode::Off => serializer.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for TerminalImagesMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) => match text.as_str() {
                "kitty" => Ok(TerminalImagesMode::Kitty),
                "iterm2" => Ok(TerminalImagesMode::Iterm2),
                "auto" => Ok(TerminalImagesMode::Auto),
                _ => Err(serde::de::Error::custom(
                    "expected \"kitty\", \"iterm2\", \"auto\", or false",
                )),
            },
            Value::Bool(false) => Ok(TerminalImagesMode::Off),
            _ => Err(serde::de::Error::custom(
                "expected \"kitty\", \"iterm2\", \"auto\", or false",
            )),
        }
    }
}

/// Terminal settings (upstream `TerminalSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clear_on_shrink: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_terminal_progress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hyperlinks: Option<AutoBool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<TerminalImagesMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub true_color: Option<AutoBool>,
}

/// Image pipeline settings (upstream `ImageSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
}

/// Per-level thinking token budgets (upstream `ThinkingBudgetsSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgetsSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

/// Markdown rendering settings (upstream `MarkdownSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_block_indent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<MermaidRenderingMode>,
}

/// Mermaid diagram rendering mode (upstream `markdown.mermaid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MermaidRenderingMode {
    Off,
    Final,
    Streaming,
}

/// Warning toggles (upstream `WarningSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WarningSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_extra_usage: Option<bool>,
}

/// Codemode presentation mode (upstream `codemode.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeMode {
    On,
    Only,
}

/// Codemode settings (upstream `CodemodeSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<CodemodeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_budget: Option<u32>,
    /// QuickJS heap cap for one codemode script execution, in MiB
    /// (pir-only; default 128 MiB, `0` = no cap). Applied to both sandbox
    /// engines (the in-process thread sandbox and the worker process), so a
    /// runaway script cannot exhaust the host's memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_memory_mb: Option<u32>,
}

/// QuickJS resource limits of the extension runtime (pir-only
/// `extensionLimits`). Every field is optional; unset fields keep the engine
/// defaults (16 MiB stack, 30 s call timeout, 30 min hook timeout, no heap
/// cap). Read from the GLOBAL settings scope only — project settings must not
/// be able to loosen the host's engine limits (see the schema description).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionLimitsSettings {
    /// QuickJS heap cap in MiB; `0` = no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    /// JS stack in MiB; `0` keeps the default, values above 16 are clamped to
    /// 16 (rquickjs treats larger stacks as "no stack check").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_mb: Option<u64>,
    /// GC threshold in MiB; `0` = no threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_threshold_mb: Option<u64>,
    /// Per-call dispatch/tool timeout in milliseconds; `0` keeps the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_timeout_ms: Option<u64>,
    /// Reserved-prefix hook timeout in milliseconds; `0` keeps the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_timeout_ms: Option<u64>,
    /// Budget for the `before_agent_start` lifecycle dispatch in milliseconds;
    /// `0` keeps the default (10 s). When it elapses the host stops waiting,
    /// the run proceeds, and the late handler's completion is discarded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_agent_start_timeout_ms: Option<u64>,
}

/// Per-tool execution settings (pir-only `tools` object; upstream has no such
/// key and ignores it — the top-level schema allows additional properties).
/// Every field is optional and off by default, so an absent object is exactly
/// upstream semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolsSettings {
    /// `tools.commandTimeoutMs`: default timeout for a bash/powershell
    /// command that does not pass its own `timeout`, in milliseconds. Unset
    /// applies the pir default (120 s, a deliberate deviation from upstream's
    /// no-default-timeout); `0` disables the cap (upstream semantics); an
    /// explicit per-call `timeout` always wins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_timeout_ms: Option<u64>,
}

/// npm or git package source (upstream `PackageSource`): either a bare source
/// string or an object filtering the loaded resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    Source(String),
    Filtered {
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        autoload: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extensions: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skills: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompts: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        themes: Option<Vec<String>>,
    },
}

/// Default project trust policy (upstream `defaultProjectTrust`, global-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultProjectTrust {
    Ask,
    Always,
    Never,
}

/// Startup output verbosity (upstream `quietStartup`): `false`, `true`, or
/// `"header"` (keep only the startup header). Unrecognized strings are
/// rejected at parse time and recorded as a load error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuietStartup {
    Enabled(bool),
    Header,
}

impl Serialize for QuietStartup {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            QuietStartup::Enabled(enabled) => serializer.serialize_bool(*enabled),
            QuietStartup::Header => serializer.serialize_str("header"),
        }
    }
}

impl<'de> Deserialize<'de> for QuietStartup {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Bool(enabled) => Ok(QuietStartup::Enabled(enabled)),
            Value::String(text) if text == "header" => Ok(QuietStartup::Header),
            _ => Err(serde::de::Error::custom("expected a boolean or \"header\"")),
        }
    }
}

/// TUI mode (upstream `tuiMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

/// Fullscreen exit behavior (upstream `fullscreenExitOutput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FullscreenExitOutput {
    Transcript,
    ResumeHint,
}

/// Fullscreen scrollbar visibility (upstream `fullscreenScrollbar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FullscreenScrollbar {
    Auto,
    Always,
    Hidden,
}

/// Double-escape action (upstream `doubleEscapeAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoubleEscapeAction {
    Fork,
    Tree,
    None,
}

/// Default `/tree` filter (upstream `treeFilterMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TreeFilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

/// Cache-warming profile (upstream `cacheWarming`, global-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingMode {
    Off,
    Streaming,
    Idle,
}

/// Tolerant reader for the `disabledBuiltinExtensions` list.
///
/// The documented shape is an array of names (`["mcp", "context-mode"]`). A
/// bare string is accepted as a one-name list, `null` is "unset", and non-string
/// entries inside the array are ignored (one bad element must not drop the
/// whole key). Any other top-level type is an error, which the manager records
/// as a per-field warning and falls back to "unset" — exactly the treatment
/// every neighbour field gets.
pub fn deserialize_optional_string_list<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Null => Ok(None),
        Value::String(name) => Ok(Some(vec![name])),
        Value::Array(entries) => Ok(Some(
            entries
                .into_iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect(),
        )),
        _ => Err(serde::de::Error::custom(
            "expected an array of built-in extension names",
        )),
    }
}

/// Parsed `disabledBuiltinExtensions` names from one raw settings value.
///
/// Scope-aware readers use this instead of the typed field so they can union
/// the scopes (see `SettingsManager::disabled_builtins`); it applies the same
/// tolerance as [`deserialize_optional_string_list`] and additionally drops
/// blank names.
pub fn disabled_builtin_names(value: &Value) -> Vec<String> {
    let names = match value {
        Value::String(name) => vec![name.as_str()],
        Value::Array(entries) => entries.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}
