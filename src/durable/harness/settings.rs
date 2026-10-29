//! Harness-wide run settings and their resolution.
//!
//! Ports the settings half of upstream `harness/types.ts` (`HarnessSettings`,
//! `Settings`, `ConversationStreamOptions`, `ConversationRetryPolicy`,
//! `CompactionPolicy`, `ProgressPolicy`, `AfterFailedRun`, `QueueMode`,
//! `ToolExecutionMode`) and `harness/agent.ts`'s `resolveSettings` /
//! `DEFAULT_*` constants.
//!
//! Two families of type are kept apart, exactly as upstream does:
//!
//! * the **partial** input types ([`HarnessSettings`], [`ProgressSettings`],
//!   [`ConversationRetrySettings`], [`CompactionSettings`] and
//!   [`PartialCompactionPolicy`]) hold only what a host set, so "absent"
//!   stays distinguishable from "set to the default value";
//! * the **resolved** types ([`Settings`], [`ProgressPolicy`],
//!   [`ConversationRetryPolicy`], [`CompactionPolicy`]) hold every field over
//!   its built-in default.
//!
//! Only [`AgentState`](crate::durable::harness::AgentState) is persisted; the
//! settings are host configuration and are deliberately not serialized here.
//! [`HarnessSettings::extensions`] holds registry [`Extension`] values (code)
//! and therefore cannot be a JSON record.
//!
//! ## Deliberate deviations from upstream
//!
//! * Upstream `restartLimit` and `documentRetentionMs` are JavaScript numbers,
//!   so `Infinity` is expressible. The restart limit is typed as
//!   [`RestartLimit`] (`Infinite` or `Limit(n)`) instead of `Option<u32>`: an
//!   `Option<u32>` cannot distinguish "unset, use 5" from "never park", and
//!   JSON cannot carry infinity at all. Millisecond retentions stay `f64` so
//!   `f64::INFINITY` keeps its upstream meaning.

use crate::ai::types::CacheRetention;
use crate::durable::harness::registry::Extension;
use crate::durable::types::JsonObject;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default `settings.restartLimit` (upstream `DEFAULT_RESTART_LIMIT`).
pub const DEFAULT_RESTART_LIMIT: u32 = 5;

/// Default `settings.contextRetentionMs`: ten minutes.
pub const DEFAULT_CONTEXT_RETENTION_MS: f64 = 600_000.0;

/// Default `settings.documentRetentionMs`: ten minutes.
pub const DEFAULT_DOCUMENT_RETENTION_MS: f64 = 600_000.0;

/// Default pause between commits of the answer being generated.
pub const DEFAULT_PARTIAL_INTERVAL_MS: u64 = 100;

/// Default pause between commits of running tool output.
pub const DEFAULT_OUTPUT_INTERVAL_MS: u64 = 100;

/// Default compaction thresholds (upstream `DEFAULT_COMPACTION_POLICY`).
pub const DEFAULT_COMPACTION_ENABLED: bool = true;
/// Room kept free for the answer.
pub const DEFAULT_COMPACTION_RESERVE_TOKENS: u64 = 16_384;
/// Approximate size of the recent context a summary keeps verbatim.
pub const DEFAULT_COMPACTION_KEEP_RECENT_TOKENS: u64 = 20_000;
/// Head start of background compaction below the blocking threshold.
pub const DEFAULT_COMPACTION_BACKGROUND_TOKENS: u64 = 32_768;

/// Default durable generation attempt retries (upstream `DEFAULT_RETRY_POLICY`).
pub const DEFAULT_RETRY_ENABLED: bool = true;
/// Attempts after the first one.
pub const DEFAULT_RETRY_MAX_RETRIES: u32 = 3;
/// First backoff delay.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 2_000;
/// Cap on the agent's own backoff delay.
pub const DEFAULT_RETRY_MAX_AGENT_DELAY_MS: u64 = 60_000;

// ─── Enumerations ───────────────────────────────────────────────────────────

/// How many consecutive restarts a task survives before it is parked.
///
/// Upstream types this as `number` with `Infinity` for "never park"; the JSON
/// wire form of [`RestartLimit::Infinite`] is `null` (which is also what
/// `JSON.stringify(Infinity)` produces), so nothing numeric ever overflows.
/// A [`RestartLimit::Limit`] below `1` is not a valid limit and resolves to
/// the build-in default, matching upstream's `limit >= 1` guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RestartLimit {
    /// Park after this many consecutive restarts without progress.
    Limit(u32),
    /// Never park; `Harness.retryTask()` is not needed because the task always
    /// runs again.
    Infinite,
}

impl Default for RestartLimit {
    fn default() -> Self {
        Self::Limit(DEFAULT_RESTART_LIMIT)
    }
}

/// Whether the tools of one round run at once or one after another in call
/// order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    /// Run the round's tools concurrently.
    #[default]
    Parallel,
    /// Run the round's tools one after another in call order.
    Sequential,
}

/// How many queued items of one mode a boundary places: the first, or all of
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    /// Place every queued item of the mode at the boundary.
    All,
    /// Place only the first queued item of the mode.
    #[default]
    OneAtATime,
}

/// What a failed run does with the inputs queued behind it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AfterFailedRun {
    /// Leave them queued until the next input or queued write.
    #[default]
    Keep,
    /// Start the next run with them, as an answer would.
    Continue,
    /// Settle them `unanswered` with reason `run_failed`.
    Withdraw,
}

/// Protocol a provider request uses (upstream pi-ai `Transport`).
///
/// `config::TransportSetting` is the settings-file twin; this enum keeps the
/// pi-ai wire strings (`"sse"`, `"websocket-cached"`, ...) so a stream options
/// value serializes exactly as upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamTransport {
    /// Let the provider adapter choose.
    Auto,
    /// Server-sent events over HTTP.
    Sse,
    /// A WebSocket.
    Websocket,
    /// A pooled, connection-scoped WebSocket.
    #[serde(rename = "websocket-cached")]
    WebsocketCached,
}

/// Provider-side deferred result window (upstream pi-ai `deferred.window`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferredWindow {
    /// Fifteen minutes.
    #[serde(rename = "15m")]
    FifteenMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
    /// Twenty-four hours.
    #[serde(rename = "24h")]
    TwentyFourHours,
}

/// pi-ai's `deferred` request option: a flag or a window object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamDeferred {
    /// `true` asks for deferred results with the provider's default window;
    /// `false` disables them.
    Enabled(bool),
    /// `{ window?: "15m" | "1h" | "24h" }`; absent keeps the provider default.
    Window {
        /// How long the provider may defer the result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<DeferredWindow>,
    },
}

// ─── Stream options ─────────────────────────────────────────────────────────

/// Curated pi-ai request options; absent fields use pi-ai defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamOptions {
    /// Transport the adapter uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<StreamTransport>,
    /// Whole-request timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Provider/SDK retries inside one request attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Cap on a provider/SDK retry delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    /// Extra request headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Provider metadata attached to the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
    /// Prompt cache retention preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    /// Deferred-response request option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<StreamDeferred>,
}

// ─── Policy halves ──────────────────────────────────────────────────────────

/// Durable generation attempt retries as a host wrote them (every field
/// optional).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRetrySettings {
    /// Whether retries happen at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Attempts after the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// First backoff delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<u64>,
    /// Cap on the agent's own backoff delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
}

/// Durable generation attempt retries, every field resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRetryPolicy {
    /// Whether retries happen at all.
    pub enabled: bool,
    /// Attempts after the first one.
    pub max_retries: u32,
    /// First backoff delay.
    pub base_delay_ms: u64,
    /// Cap on the agent's own backoff delay.
    pub max_agent_delay_ms: u64,
}

impl Default for ConversationRetryPolicy {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_RETRY_ENABLED,
            max_retries: DEFAULT_RETRY_MAX_RETRIES,
            base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            max_agent_delay_ms: DEFAULT_RETRY_MAX_AGENT_DELAY_MS,
        }
    }
}

impl ConversationRetryPolicy {
    /// Merge a host's partial policy over the defaults, field by field.
    pub fn merged(settings: Option<&ConversationRetrySettings>) -> Self {
        let defaults = Self::default();
        let Some(settings) = settings else {
            return defaults;
        };
        Self {
            enabled: settings.enabled.unwrap_or(defaults.enabled),
            max_retries: settings.max_retries.unwrap_or(defaults.max_retries),
            base_delay_ms: settings.base_delay_ms.unwrap_or(defaults.base_delay_ms),
            max_agent_delay_ms: settings
                .max_agent_delay_ms
                .unwrap_or(defaults.max_agent_delay_ms),
        }
    }
}

/// How often running progress is committed, every field resolved.
///
/// Each progress commit is a storage write; a host whose storage is remote can
/// commit less often, so live answers and tool output appear in larger steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPolicy {
    /// Minimum pause between commits of the answer being generated.
    pub partial_interval_ms: u64,
    /// Minimum pause between commits of running tool output.
    pub output_interval_ms: u64,
}

impl Default for ProgressPolicy {
    fn default() -> Self {
        Self {
            partial_interval_ms: DEFAULT_PARTIAL_INTERVAL_MS,
            output_interval_ms: DEFAULT_OUTPUT_INTERVAL_MS,
        }
    }
}

/// [`ProgressPolicy`] as a host wrote it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSettings {
    /// Minimum pause between commits of the answer being generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_interval_ms: Option<u64>,
    /// Minimum pause between commits of running tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_interval_ms: Option<u64>,
}

impl ProgressPolicy {
    /// Merge a host's partial policy over the defaults, field by field, so an
    /// explicitly unset interval keeps its default.
    pub fn merged(settings: Option<&ProgressSettings>) -> Self {
        let defaults = Self::default();
        let Some(settings) = settings else {
            return defaults;
        };
        Self {
            partial_interval_ms: settings
                .partial_interval_ms
                .unwrap_or(defaults.partial_interval_ms),
            output_interval_ms: settings
                .output_interval_ms
                .unwrap_or(defaults.output_interval_ms),
        }
    }
}

/// Compaction thresholds that differ between models; every field optional.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialCompactionPolicy {
    /// Threshold and overflow compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Room kept free for the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    /// Approximate size of the recent context a summary keeps verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Background compaction head start; `0` disables background compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tokens: Option<u64>,
}

/// Harness-wide compaction settings as a host wrote them.
///
/// `model_overrides` is keyed by exact `"provider/modelId"`; each field an
/// entry sets replaces the harness-wide one for that model, so small- and
/// large-window models can share one harness.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    /// Threshold and overflow compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Room kept free for the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    /// Approximate size of the recent context a summary keeps verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Background compaction head start; `0` disables background compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tokens: Option<u64>,
    /// Per-model field-by-field overrides of the values above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<BTreeMap<String, PartialCompactionPolicy>>,
}

/// Automatic compaction thresholds, every harness-wide field resolved.
///
/// `model_overrides` is carried alongside because upstream's resolved
/// `Settings.compaction` is `CompactionPolicy & { modelOverrides }`; use
/// [`resolve_compaction_for`] to obtain the leaf policy for one model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPolicy {
    /// Threshold and overflow compaction.
    pub enabled: bool,
    /// Room kept free for the answer.
    pub reserve_tokens: u64,
    /// Approximate size of the recent context a summary keeps verbatim.
    pub keep_recent_tokens: u64,
    /// Background compaction head start; `0` disables background compaction.
    pub background_tokens: u64,
    /// Per-model field-by-field overrides of the values above.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_overrides: BTreeMap<String, PartialCompactionPolicy>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_COMPACTION_ENABLED,
            reserve_tokens: DEFAULT_COMPACTION_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_COMPACTION_KEEP_RECENT_TOKENS,
            background_tokens: DEFAULT_COMPACTION_BACKGROUND_TOKENS,
            model_overrides: BTreeMap::new(),
        }
    }
}

impl CompactionPolicy {
    /// The policy for one model: its `"provider/modelId"` override applied
    /// field by field over the harness-wide values.
    ///
    /// The returned policy is a leaf: `model_overrides` is empty, because the
    /// map selects an entry rather than describing one. Upstream's
    /// `compactionPolicy()` drops the map the same way.
    pub fn for_model(&self, model: &str) -> Self {
        let Some(overrides) = self.model_overrides.get(model) else {
            return Self::leaf(self);
        };
        Self {
            enabled: overrides.enabled.unwrap_or(self.enabled),
            reserve_tokens: overrides.reserve_tokens.unwrap_or(self.reserve_tokens),
            keep_recent_tokens: overrides
                .keep_recent_tokens
                .unwrap_or(self.keep_recent_tokens),
            background_tokens: overrides
                .background_tokens
                .unwrap_or(self.background_tokens),
            model_overrides: BTreeMap::new(),
        }
    }

    fn leaf(policy: &Self) -> Self {
        Self {
            enabled: policy.enabled,
            reserve_tokens: policy.reserve_tokens,
            keep_recent_tokens: policy.keep_recent_tokens,
            background_tokens: policy.background_tokens,
            model_overrides: BTreeMap::new(),
        }
    }
}

/// The compaction policy for `model` (exact `"provider/modelId"`), applying
/// that model's override field by field.
pub fn resolve_compaction_for(policy: &CompactionPolicy, model: &str) -> CompactionPolicy {
    policy.for_model(model)
}

// ─── Harness settings ───────────────────────────────────────────────────────

/// Harness-wide run policy as the host supplies it.
///
/// Read at every resolution and never copied; an unset field follows the
/// built-in default documented on [`Settings`].
#[derive(Clone, Debug, Default)]
pub struct HarnessSettings {
    /// Default extension selection; absent: every installed extension, in
    /// install order.
    pub extensions: Option<Vec<Extension>>,
    /// Curated provider request options.
    pub stream: Option<ConversationStreamOptions>,
    /// Durable generation attempt retries.
    pub retry: Option<ConversationRetrySettings>,
    /// Compaction thresholds and per-model overrides.
    pub compaction: Option<CompactionSettings>,
    /// How often running progress is committed.
    pub progress: Option<ProgressSettings>,
    /// Whether a tool round runs at once or one after another.
    pub tool_execution: Option<ToolExecutionMode>,
    /// How many steering inputs a boundary places.
    pub steering_mode: Option<QueueMode>,
    /// How many follow-up inputs a boundary places.
    pub follow_up_mode: Option<QueueMode>,
    /// How long an idle conversation keeps its last context read in memory;
    /// `0` drops it once idle.
    pub context_retention_ms: Option<f64>,
    /// How long a loaded document no watch observes stays in memory;
    /// `0` drops it right after use and `f64::INFINITY` keeps it.
    pub document_retention_ms: Option<f64>,
    /// What a failed run does with the inputs queued behind it.
    pub after_failed_run: Option<AfterFailedRun>,
    /// How many consecutive restarts park a task.
    pub restart_limit: Option<RestartLimit>,
}

/// Resolved settings: every field over its built-in default, object fields
/// merged.
///
/// Defaults, matching upstream `Settings`:
///
/// | field | default |
/// |---|---|
/// | `extensions` | `None`: every installed extension, in install order |
/// | `stream` | [`ConversationStreamOptions::default`] |
/// | `retry` | enabled, 3 retries, 2000 ms base, 60000 ms agent cap |
/// | `compaction` | enabled, 16384 / 20000 / 32768 tokens, no model overrides |
/// | `progress` | 100 ms / 100 ms |
/// | `tool_execution` | [`ToolExecutionMode::Parallel`] |
/// | `steering_mode`, `follow_up_mode` | [`QueueMode::OneAtATime`] |
/// | `context_retention_ms`, `document_retention_ms` | 600000 |
/// | `after_failed_run` | [`AfterFailedRun::Keep`] |
/// | `restart_limit` | [`RestartLimit::Limit`]`(5)` |
#[derive(Clone, Debug)]
pub struct Settings {
    /// Default extension selection; absent: every installed extension.
    pub extensions: Option<Vec<Extension>>,
    /// Curated provider request options.
    pub stream: ConversationStreamOptions,
    /// Durable generation attempt retries.
    pub retry: ConversationRetryPolicy,
    /// Compaction thresholds, plus the per-model override map.
    pub compaction: CompactionPolicy,
    /// How often running progress is committed.
    pub progress: ProgressPolicy,
    /// Whether a tool round runs at once or one after another.
    pub tool_execution: ToolExecutionMode,
    /// How many steering inputs a boundary places.
    pub steering_mode: QueueMode,
    /// How many follow-up inputs a boundary places.
    pub follow_up_mode: QueueMode,
    /// How long an idle conversation keeps its last context read in memory.
    pub context_retention_ms: f64,
    /// How long an unobserved loaded document stays in memory.
    pub document_retention_ms: f64,
    /// What a failed run does with the inputs queued behind it.
    pub after_failed_run: AfterFailedRun,
    /// How many consecutive restarts park a task.
    pub restart_limit: RestartLimit,
}

impl Default for Settings {
    fn default() -> Self {
        resolve_settings(&HarnessSettings::default())
    }
}

impl Settings {
    /// The compaction policy for `model` (exact `"provider/modelId"`), with
    /// that model's override applied field by field.
    pub fn compaction_for(&self, model: &str) -> CompactionPolicy {
        resolve_compaction_for(&self.compaction, model)
    }
}

/// Resolve the host settings: every field over its built-in default, object
/// fields merged.
///
/// Matches upstream `resolveSettings`:
///
/// * `stream` is used as given (absent: empty);
/// * `retry`, `compaction` and `progress` merge field by field, so an
///   explicitly unset member keeps its default;
/// * an out-of-range `document_retention_ms` (negative or NaN) keeps the
///   default, while `0` and `f64::INFINITY` are honored;
/// * a `restart_limit` below 1 keeps the default of 5.
pub fn resolve_settings(settings: &HarnessSettings) -> Settings {
    Settings {
        extensions: settings.extensions.clone(),
        stream: settings.stream.clone().unwrap_or_default(),
        retry: ConversationRetryPolicy::merged(settings.retry.as_ref()),
        compaction: resolve_compaction(settings.compaction.as_ref()),
        progress: ProgressPolicy::merged(settings.progress.as_ref()),
        tool_execution: settings.tool_execution.unwrap_or_default(),
        steering_mode: settings.steering_mode.unwrap_or_default(),
        follow_up_mode: settings.follow_up_mode.unwrap_or_default(),
        // Upstream uses `??`, so any number (including 0 and Infinity) is
        // taken as given.
        context_retention_ms: settings
            .context_retention_ms
            .unwrap_or(DEFAULT_CONTEXT_RETENTION_MS),
        document_retention_ms: document_retention(settings.document_retention_ms),
        after_failed_run: settings.after_failed_run.unwrap_or_default(),
        restart_limit: restart_limit(settings.restart_limit),
    }
}

/// A retention as given when it is a number of milliseconds, `0` or more
/// (`f64::INFINITY` keeps documents); otherwise the default.
fn document_retention(ms: Option<f64>) -> f64 {
    match ms {
        Some(ms) if ms >= 0.0 => ms,
        _ => DEFAULT_DOCUMENT_RETENTION_MS,
    }
}

/// A restart limit as given when it is a number of restarts, `1` or more;
/// otherwise the default. `Infinite` never parks.
fn restart_limit(limit: Option<RestartLimit>) -> RestartLimit {
    match limit {
        Some(RestartLimit::Limit(limit)) if limit >= 1 => RestartLimit::Limit(limit),
        Some(RestartLimit::Infinite) => RestartLimit::Infinite,
        _ => RestartLimit::Limit(DEFAULT_RESTART_LIMIT),
    }
}

/// Merge a host's partial compaction settings over the defaults, field by
/// field, keeping the override map.
fn resolve_compaction(settings: Option<&CompactionSettings>) -> CompactionPolicy {
    let defaults = CompactionPolicy::default();
    let Some(settings) = settings else {
        return defaults;
    };
    CompactionPolicy {
        enabled: settings.enabled.unwrap_or(defaults.enabled),
        reserve_tokens: settings.reserve_tokens.unwrap_or(defaults.reserve_tokens),
        keep_recent_tokens: settings
            .keep_recent_tokens
            .unwrap_or(defaults.keep_recent_tokens),
        background_tokens: settings
            .background_tokens
            .unwrap_or(defaults.background_tokens),
        model_overrides: settings.model_overrides.clone().unwrap_or_default(),
    }
}
