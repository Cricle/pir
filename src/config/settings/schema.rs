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

/// Session retention / garbage collection settings (pir extension). All
/// bounds are optional; `enabled` gates the automatic startup pass, while the
/// explicit `pir sessions prune` command can run a one-shot pass regardless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionRetentionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl SessionRetentionSettings {
    /// The merged defaults (opt-in; 30 days / 500 sessions / 2 GiB).
    pub fn defaults() -> Self {
        Self {
            enabled: Some(defaults::SESSION_RETENTION_ENABLED),
            max_age_days: Some(defaults::SESSION_RETENTION_MAX_AGE_DAYS),
            max_sessions: Some(defaults::SESSION_RETENTION_MAX_SESSIONS),
            max_bytes: Some(defaults::SESSION_RETENTION_MAX_BYTES),
        }
    }
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

/// The transport-level knobs `settings.retry.provider.*` resolves to, ready to
/// install on a request's [`crate::ai::types::StreamOptions`] (upstream
/// `ProviderRetrySettings` as `sdk.ts:341-352` maps it onto every request).
///
/// `max_retries` mirrors the upstream `maxRetries ?? 0` resolution
/// (`retryProviderRequest`, provider-retry.ts:107-128): an unconfigured run
/// sends exactly one provider attempt per agent turn, and the agent-level
/// 2/4/8 s ladder owns the transient 408/409/429/5xx budget. The other two
/// fields stay `None` when the user wrote nothing — the transport keeps its
/// 60 s cap / no total timeout — which is also what lets an explicit per-call
/// override (`set_retry` / the SDK `retry()`) stay effective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderRetryOverrides {
    /// `retry.provider.maxRetries` as written, resolved with the upstream
    /// `?? 0` default: `None` only on a hand-built value (`from_settings`
    /// always fills it). `Some(0)` means exactly one request per attempt.
    pub max_retries: Option<u32>,
    /// `retry.provider.maxRetryDelayMs` as written (`Some(0)` disables the
    /// cap).
    pub max_retry_delay_ms: Option<u64>,
    /// `retry.provider.timeoutMs` as written (`Some(0)` disables the total
    /// timeout).
    pub timeout_ms: Option<u64>,
}

impl ProviderRetryOverrides {
    /// Whether this value carries no transport knob at all: only a hand-built
    /// `default()` (embedder/test seam) is empty — a settings-derived value
    /// always carries the resolved `max_retries`.
    pub fn is_empty(&self) -> bool {
        self.max_retries.is_none() && self.max_retry_delay_ms.is_none() && self.timeout_ms.is_none()
    }

    /// Resolve from a possibly absent `retry` block. An absent
    /// `provider.maxRetries` resolves to the upstream `?? 0` default
    /// (`Some(0)`: one provider attempt per turn, the agent-level ladder owns
    /// the transient budget); an explicit value, including `0`, still wins.
    /// The other two fields stay `None` when unwritten (the transport's 60 s
    /// cap / no total timeout).
    pub fn from_settings(retry: Option<&RetrySettings>) -> Self {
        let provider = retry.and_then(|retry| retry.provider.as_ref());
        Self {
            max_retries: Some(
                provider
                    .and_then(|provider| provider.max_retries)
                    .unwrap_or(0),
            ),
            max_retry_delay_ms: provider.and_then(|provider| provider.max_retry_delay_ms),
            timeout_ms: provider.and_then(|provider| provider.timeout_ms),
        }
    }

    /// Layer these settings under an explicit per-call override (upstream
    /// `options.maxRetries ?? providerRetrySettings.maxRetries`): the
    /// caller-supplied option always wins, including the SDK embedder's
    /// `set_retry` / `retry()`.
    pub fn apply_to(self, options: &mut crate::ai::types::StreamOptions) {
        if options.max_retries.is_none() {
            options.max_retries = self.max_retries;
        }
        if options.max_retry_delay_ms.is_none() {
            options.max_retry_delay_ms = self.max_retry_delay_ms;
        }
        if options.timeout_ms.is_none() {
            options.timeout_ms = self.timeout_ms;
        }
    }
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

/// Where the `memory` builtin keeps its cm store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MemoryStoreMode {
    /// Same directory as the session transcript (`<session-dir>/cm`); when a
    /// session has no file, the default session directory for the cwd.
    #[default]
    Session,
    /// `<project-root>/cm` (team-shared, CLI-compatible).
    Workspace,
    /// `<agent-dir>/memory/<project-hash>/cm` (personal, outside the repo).
    AgentDir,
    /// The explicit `path` setting.
    Path,
}

/// Write policy of the `memory` builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MemoryWriteMode {
    /// Normal writes through `remember`/`frame`/`absorb`/`finish`.
    #[default]
    Explicit,
    /// Open the store read-only; every write action reports an error.
    ReadOnly,
}

/// How `input` auto-recall decides whether (and how much) to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MemoryAutoRecallPolicy {
    /// Never inject (equivalent to `autoRecall: false`).
    Off,
    /// Always use the configured full budget/max-items.
    Always,
    /// Adaptive: questions and long prompts get the full brief; plain short
    /// edit prompts get no injection, while continuation prompts get only the
    /// active task frame (no memory records).
    #[default]
    Auto,
}

/// The `memory` builtin's settings (`memory` object in settings.json).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySettings {
    /// Store location policy; default `session`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<MemoryStoreMode>,
    /// Explicit cm file path used when `store = "path"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Append a budgeted `<memory-context>` block to each user prompt.
    /// Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_recall: Option<bool>,
    /// Auto-recall selection policy; default `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_recall_policy: Option<MemoryAutoRecallPolicy>,
    /// Minimum top BM25 score for automatic recall; when no frame exists.
    /// Default 1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_recall_min_score: Option<f64>,
    /// Prompts shorter than this many characters skip automatic recall in
    /// `auto` mode. Default 40.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_recall_min_prompt_chars: Option<u32>,
    /// Inject a stable frame brief into the system prompt once per session.
    /// Default false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_brief: Option<bool>,
    /// Token budget for automatic recall. Default 600.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recall_budget_tokens: Option<u32>,
    /// Maximum number of memories in automatic recall. Default 3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recall_max_items: Option<u32>,
    /// Write policy; default `explicit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<MemoryWriteMode>,
    /// Checkpoint after a turn that wrote memory. Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_on_settle: Option<bool>,
    /// Reject `source` anchors outside the project root. Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_sources: Option<bool>,
    /// Maximum cm file size before writes are rejected. Default 256 MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_store_bytes: Option<u64>,
    /// Keep the disposable `cm.cache` next to the store. Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_cache: Option<bool>,
    /// Allow `wiki generate` / `wiki review` to spawn external agent commands.
    /// Default false (the rest of the wiki surface is always available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_external_commands: Option<bool>,
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
    /// `tools.broadSearchGuard`: reject obvious whole-filesystem searches
    /// (`find /`, recursive grep/rg over `/tmp`, ...) before spawning a shell.
    /// Defaults to true; set false to restore the unrestricted historical
    /// shell.
    #[serde(rename = "broadSearchGuard", skip_serializing_if = "Option::is_none")]
    pub broad_search_guard: Option<bool>,
}

/// Performance profile for the agent loop. `Balanced` is the historical
/// behavior; `Efficient` opts into a smaller default tool loadout, bounded
/// tool results, and turn/token budgets. It is never selected implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PerformanceProfile {
    #[default]
    Balanced,
    Efficient,
}

/// Prompt-cache mode for OpenAI-compatible providers. `Auto` only sends
/// cache hints when the provider declares support; `Off` never sends them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PromptCacheMode {
    #[default]
    Auto,
    Off,
}

/// Opt-in performance controls (pir-only `performance` object). Every field
/// is optional; with no object, or with `profile: "balanced"`, the session
/// uses the historical behavior byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PerformanceSettings {
    /// `performance.profile`: `balanced` (default) or `efficient`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<PerformanceProfile>,
    /// Maximum model-visible text bytes per tool result. `0`/unset keeps the
    /// historical unlimited behavior on `balanced`; `efficient` defaults to
    /// 32 KiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result_max_bytes: Option<usize>,
    /// Soft turn budget: inject a convergence nudge, never terminate. `0`
    /// disables it; `efficient` defaults to 24.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_turn_budget: Option<usize>,
    /// Hard turn budget. `0` keeps the host's existing cap; `efficient`
    /// defaults to 48 for non-interactive runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hard_turn_budget: Option<usize>,
    /// Per-run cumulative token budget. `0` disables it; `efficient` defaults
    /// to 250k.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// Total project-context budget in bytes. `0`/unset keeps the existing
    /// per-file behavior; `efficient` defaults to 64 KiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_files_max_bytes: Option<usize>,
    /// `performance.promptCache`: `auto` (default) or `off`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<PromptCacheMode>,
}

impl PerformanceSettings {
    /// Process-level profile override for hosts that do not carry a
    /// `SettingsManager` (library/SDK/subagent paths). Settings-file values
    /// still win when a host passes them explicitly.
    pub fn from_env() -> Self {
        Self::default().with_env_profile()
    }

    /// Apply the process-level profile override to an already parsed settings
    /// object. File values win for every field except the profile, which the
    /// explicit env switch is allowed to change.
    pub fn with_env_profile(mut self) -> Self {
        if let Ok(value) = std::env::var("PIR_PERF_PROFILE") {
            self.profile = match value.trim().to_ascii_lowercase().as_str() {
                "efficient" => Some(PerformanceProfile::Efficient),
                "balanced" => Some(PerformanceProfile::Balanced),
                _ => self.profile,
            };
        }
        self
    }

    /// The selected profile (balanced when absent).
    pub fn profile(&self) -> PerformanceProfile {
        self.profile.unwrap_or_default()
    }

    /// Effective per-result text cap. `balanced` keeps unlimited unless the
    /// user explicitly sets a positive value; `efficient` defaults to 32 KiB.
    pub fn effective_tool_result_max_bytes(&self) -> usize {
        match self.tool_result_max_bytes {
            Some(value) => value,
            None if self.profile() == PerformanceProfile::Efficient => 32 * 1024,
            None => 0,
        }
    }

    /// Effective soft turn budget (0 = disabled).
    pub fn effective_soft_turn_budget(&self) -> usize {
        match self.soft_turn_budget {
            Some(value) => value,
            None if self.profile() == PerformanceProfile::Efficient => 24,
            None => 0,
        }
    }

    /// Effective hard turn budget (0 = host default).
    pub fn effective_hard_turn_budget(&self) -> usize {
        match self.hard_turn_budget {
            Some(value) => value,
            None if self.profile() == PerformanceProfile::Efficient => 48,
            None => 0,
        }
    }

    /// Effective cumulative token budget (0 = disabled).
    pub fn effective_max_total_tokens(&self) -> u64 {
        match self.max_total_tokens {
            Some(value) => value,
            None if self.profile() == PerformanceProfile::Efficient => 250_000,
            None => 0,
        }
    }

    /// Effective project-context budget (0 = existing behavior).
    pub fn effective_context_files_max_bytes(&self) -> usize {
        match self.context_files_max_bytes {
            Some(value) => value,
            None if self.profile() == PerformanceProfile::Efficient => 64 * 1024,
            None => 0,
        }
    }

    /// Whether prompt-cache hints may be sent.
    pub fn prompt_cache_enabled(&self) -> bool {
        self.prompt_cache.unwrap_or_default() == PromptCacheMode::Auto
    }
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

/// One prefixed `extensions` entry that controls a built-in extension:
/// `-builtin:<name>`, `+builtin:<name>`, or `!builtin:<pattern>` (upstream
/// settings.md:163, package-manager.ts:980-1001). Returns the control prefix
/// and the target path (`"builtin:<name>"`), or `None` for a plain entry (a
/// source, never a filter) or a prefixed entry that does not name a builtin.
pub fn extension_builtin_control(entry: &str) -> Option<(char, &str)> {
    let control = entry.chars().next()?;
    if !matches!(control, '-' | '+' | '!') {
        return None;
    }
    let target = &entry[control.len_utf8()..];
    let name = target.strip_prefix("builtin:")?;
    (!name.trim().is_empty()).then_some((control, target))
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
