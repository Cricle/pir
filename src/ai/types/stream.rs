//! Stream options, usage accounting, and event-stream protocol types.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use super::messages::{AssistantMessage, ToolCall};
use super::model::ThinkingLevel;
use super::JsonValue;

// ---------------------------------------------------------------------------
// Usage & cost
// ---------------------------------------------------------------------------

/// Token cost breakdown.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
    /// True when `total` is the billed cost reported by the provider. False
    /// when it is a catalog estimate; absent also means false (upstream
    /// `Usage.cost.totalFromProvider`).
    #[serde(
        default,
        rename = "totalFromProvider",
        skip_serializing_if = "Option::is_none"
    )]
    pub total_from_provider: Option<bool>,
}

impl Default for Cost {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
            total_from_provider: None,
        }
    }
}

impl Cost {
    /// Whether `total` is the provider's billed cost (rather than a catalog
    /// estimate). `None` (absent) counts as false, like upstream.
    pub fn is_provider_billed(&self) -> bool {
        self.total_from_provider == Some(true)
    }
}

/// Token usage statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    /// Subset of `cache_write` written with 1h retention (Anthropic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "cacheWrite1h")]
    pub cache_write_1h: Option<u64>,
    /// Reasoning/thinking tokens, when reported by the provider.
    /// This is a subset of `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    pub cost: Cost,
}

/// How a thinking-capable provider returns thinking content
/// (upstream `AnthropicThinkingDisplay` / `BedrockThinkingDisplay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThinkingDisplay {
    /// Thinking blocks carry summarized thinking text (pir's default).
    #[serde(rename = "summarized")]
    Summarized,
    /// Thinking blocks return an empty thinking field; the encrypted
    /// signature still travels back for multi-turn continuity.
    #[serde(rename = "omitted")]
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingDisplay::Summarized => "summarized",
            ThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// OpenAI Responses `reasoning.summary` request value (upstream
/// `OpenAIResponsesOptions.reasoningSummary` / `OpenAICodexResponsesOptions.
/// reasoningSummary`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReasoningSummary {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "concise")]
    Concise,
    #[serde(rename = "detailed")]
    Detailed,
    /// Codex-only: disable reasoning summaries.
    #[serde(rename = "off")]
    Off,
    /// Codex-only: enable (provider default granularity).
    #[serde(rename = "on")]
    On,
}

impl ReasoningSummary {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningSummary::Auto => "auto",
            ReasoningSummary::Concise => "concise",
            ReasoningSummary::Detailed => "detailed",
            ReasoningSummary::Off => "off",
            ReasoningSummary::On => "on",
        }
    }
}

/// OpenAI Responses `text.verbosity` request value (upstream
/// `OpenAICodexResponsesOptions.textVerbosity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextVerbosity {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
}

impl TextVerbosity {
    pub fn as_str(&self) -> &'static str {
        match self {
            TextVerbosity::Low => "low",
            TextVerbosity::Medium => "medium",
            TextVerbosity::High => "high",
        }
    }
}

// ---------------------------------------------------------------------------
// Stop reason
// ---------------------------------------------------------------------------

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum StopReason {
    /// Still generating (streaming partial).
    #[serde(rename = "pending")]
    Pending,
    /// Model stopped naturally.
    #[serde(rename = "stop")]
    Stop,
    /// Max tokens reached.
    #[serde(rename = "length")]
    Length,
    /// Model requested tool use.
    #[serde(rename = "toolUse")]
    ToolUse,
    /// An error occurred.
    #[serde(rename = "error")]
    Error,
    /// Request was aborted.
    #[serde(rename = "aborted")]
    Aborted,
    /// Response was deferred (async).
    #[serde(rename = "deferred")]
    Deferred,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopReason::Pending => write!(f, "pending"),
            StopReason::Stop => write!(f, "stop"),
            StopReason::Length => write!(f, "length"),
            StopReason::ToolUse => write!(f, "toolUse"),
            StopReason::Error => write!(f, "error"),
            StopReason::Aborted => write!(f, "aborted"),
            StopReason::Deferred => write!(f, "deferred"),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider request options
// ---------------------------------------------------------------------------

/// Provider-scoped environment overrides.
pub type ProviderEnv = BTreeMap<String, String>;

/// Provider HTTP headers.
///
/// An empty-string value marks a SUPPRESSED header (upstream
/// `ProviderHeaders` values may be `null`): the header shadows any
/// default/provider header with the same name (case-insensitive) and is not
/// sent on the wire. See [`suppress_header`] and [`merge_headers`].
pub type ProviderHeaders = BTreeMap<String, String>;

/// Insert a header under its exact name, replacing any existing header with
/// the same name case-insensitively (upstream `mergeHeaders`).
pub fn insert_header(headers: &mut ProviderHeaders, name: &str, value: impl Into<String>) {
    let existing = headers
        .keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned();
    if let Some(existing) = existing {
        headers.remove(&existing);
    }
    headers.insert(name.to_string(), value.into());
}

/// Merge `overlay` over `base`, with overlay entries winning per header name
/// case-insensitively. A `None` value removes the header; `Some("")`
/// suppresses it (kept in the map as the suppression marker).
pub fn merge_headers(
    base: Option<&ProviderHeaders>,
    overlay: Option<&ProviderHeaders>,
) -> Option<ProviderHeaders> {
    if base.is_none() && overlay.is_none() {
        return None;
    }
    let mut merged = base.cloned().unwrap_or_default();
    for (name, value) in overlay.into_iter().flatten() {
        insert_header(&mut merged, name, value.clone());
    }
    Some(merged)
}

/// Mark `name` as suppressed (upstream `null` header value): later default or
/// provider headers with that name are not sent.
pub fn suppress_header(headers: &mut ProviderHeaders, name: &str) {
    insert_header(headers, name, String::new());
}

/// Look up a header value case-insensitively (last exact-name insertion is
/// irrelevant: [`insert_header`] keeps names unique case-insensitively).
pub fn header_value<'a>(headers: Option<&'a ProviderHeaders>, name: &str) -> Option<&'a str> {
    headers?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Whether a header is present at all (a suppressed header counts as present).
pub fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    header_value(headers, name).is_some()
}

/// Whether a header is present with a non-empty value.
pub fn has_non_empty_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    header_value(headers, name).is_some_and(|value| !value.trim().is_empty())
}

/// Drop suppressed (empty) header values before a request goes on the wire.
pub fn finalize_headers(headers: ProviderHeaders) -> ProviderHeaders {
    headers
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

/// Tool-choice preference for a single request (upstream `ToolChoice` plus the
/// richer per-API shapes: Anthropic/Bedrock accept `any` and a named tool,
/// OpenAI accepts `required` and a named function).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    /// Force the model to call at least one tool (OpenAI `required`,
    /// Anthropic/Bedrock/Google `any`).
    Any,
    /// OpenAI alias of [`ToolChoice::Any`].
    Required,
    /// Force one named tool.
    Tool {
        name: String,
    },
}

impl ToolChoice {
    /// The simple string wire value. Named-tool choices have no simple string
    /// form and map to `"auto"`; prefer [`Self::simple_value`] to detect it.
    pub fn wire_value(&self) -> &'static str {
        self.simple_value().unwrap_or("auto")
    }

    /// The string wire value when the choice is expressible as one
    /// (`auto`/`none`/`any`/`required`); `None` for a named tool.
    pub fn simple_value(&self) -> Option<&'static str> {
        match self {
            ToolChoice::Auto => Some("auto"),
            ToolChoice::None => Some("none"),
            ToolChoice::Any => Some("any"),
            ToolChoice::Required => Some("required"),
            ToolChoice::Tool { .. } => None,
        }
    }

    /// The forced tool name, when this is a named-tool choice.
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            ToolChoice::Tool { name } => Some(name),
            _ => None,
        }
    }

    /// Anthropic Messages `tool_choice` (upstream
    /// `anthropic-messages.ts:1316-1320`).
    pub fn anthropic_json(&self) -> JsonValue {
        match self {
            ToolChoice::Auto => serde_json::json!({"type": "auto"}),
            ToolChoice::None => serde_json::json!({"type": "none"}),
            ToolChoice::Any | ToolChoice::Required => serde_json::json!({"type": "any"}),
            ToolChoice::Tool { name } => serde_json::json!({"type": "tool", "name": name}),
        }
    }

    /// Bedrock Converse `toolConfig.toolChoice` (upstream
    /// `bedrock-converse-stream.ts:1175-1186`); `none` is handled by dropping
    /// `toolConfig`.
    pub fn bedrock_json(&self) -> JsonValue {
        match self {
            ToolChoice::Auto => serde_json::json!({"auto": {}}),
            ToolChoice::None => serde_json::json!("none"),
            ToolChoice::Any | ToolChoice::Required => serde_json::json!({"any": {}}),
            ToolChoice::Tool { name } => serde_json::json!({"tool": {"name": name}}),
        }
    }

    /// OpenAI Chat Completions `tool_choice` (upstream
    /// `openai-completions.ts`: `"auto" | "none" | "required" | {type:
    /// "function", function: {name}}`).
    pub fn openai_chat_json(&self) -> JsonValue {
        match self {
            ToolChoice::Auto => serde_json::json!("auto"),
            ToolChoice::None => serde_json::json!("none"),
            ToolChoice::Any | ToolChoice::Required => serde_json::json!("required"),
            ToolChoice::Tool { name } => {
                serde_json::json!({"type": "function", "function": {"name": name}})
            }
        }
    }

    /// OpenAI Responses `tool_choice` (upstream `openai-responses.ts`:
    /// `"auto" | "none" | "required" | {type: "function", name}`).
    pub fn openai_responses_json(&self) -> JsonValue {
        match self {
            ToolChoice::Auto => serde_json::json!("auto"),
            ToolChoice::None => serde_json::json!("none"),
            ToolChoice::Any | ToolChoice::Required => serde_json::json!("required"),
            ToolChoice::Tool { name } => serde_json::json!({"type": "function", "name": name}),
        }
    }

    /// Google function-calling mode (upstream `google-shared.ts:484-494`).
    pub fn google_mode(&self) -> &'static str {
        match self {
            ToolChoice::Auto => "AUTO",
            ToolChoice::None => "NONE",
            ToolChoice::Any | ToolChoice::Required | ToolChoice::Tool { .. } => "ANY",
        }
    }

    /// Mistral `toolChoice` (upstream `mistral-conversations.ts:535,896`).
    pub fn mistral_json(&self) -> JsonValue {
        match self {
            ToolChoice::Auto => serde_json::json!("auto"),
            ToolChoice::None => serde_json::json!("none"),
            ToolChoice::Any => serde_json::json!("any"),
            ToolChoice::Required => serde_json::json!("required"),
            ToolChoice::Tool { name } => {
                serde_json::json!({"type": "function", "function": {"name": name}})
            }
        }
    }
}

impl serde::Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ToolChoice::Auto => serializer.serialize_str("auto"),
            ToolChoice::None => serializer.serialize_str("none"),
            ToolChoice::Any => serializer.serialize_str("any"),
            ToolChoice::Required => serializer.serialize_str("required"),
            ToolChoice::Tool { name } => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("ToolChoice", 2)?;
                state.serialize_field("type", "tool")?;
                state.serialize_field("name", name)?;
                state.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for ToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = JsonValue::deserialize(deserializer)?;
        match value {
            JsonValue::String(choice) => match choice.as_str() {
                "auto" => Ok(ToolChoice::Auto),
                "none" => Ok(ToolChoice::None),
                "any" => Ok(ToolChoice::Any),
                "required" => Ok(ToolChoice::Required),
                other => Err(D::Error::unknown_variant(
                    other,
                    &["auto", "none", "any", "required"],
                )),
            },
            JsonValue::Object(object) => {
                // `{type: "tool", name}` (Anthropic shape), `{type:
                // "function", name}` (Responses shape), and `{type:
                // "function", function: {name}}` (Chat Completions/Mistral).
                let name = object.get("name").and_then(JsonValue::as_str).or_else(|| {
                    object
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(JsonValue::as_str)
                });
                match name {
                    Some(name) => Ok(ToolChoice::Tool {
                        name: name.to_string(),
                    }),
                    None => Err(D::Error::custom(
                        "tool choice object is missing a tool name",
                    )),
                }
            }
            _ => Err(D::Error::custom(
                "tool choice must be a string or a tool-choice object",
            )),
        }
    }
}

/// Async header transform (upstream `ModelsRequestTransforms.transformHeaders`):
/// receives the fully assembled provider/model/auth/request headers and
/// returns the headers to dispatch.
pub type HeaderTransform = std::sync::Arc<
    dyn Fn(
            ProviderHeaders,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = crate::ai::error::AiResult<ProviderHeaders>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

/// Optional [`HeaderTransform`] slot with a non-`dyn` `Debug`.
#[derive(Clone, Default)]
pub struct HeaderTransformHook {
    hook: Option<HeaderTransform>,
}

impl HeaderTransformHook {
    pub fn new(hook: HeaderTransform) -> Self {
        Self { hook: Some(hook) }
    }

    pub fn is_installed(&self) -> bool {
        self.hook.is_some()
    }

    pub fn hook(&self) -> Option<&HeaderTransform> {
        self.hook.as_ref()
    }
}

impl std::fmt::Debug for HeaderTransformHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeaderTransformHook")
            .field("installed", &self.hook.is_some())
            .finish()
    }
}

/// Per-request fetch/transport injection (upstream `FetchFunction`).
///
/// Wraps the transport seam so a caller can drive one request through an
/// alternate HTTP implementation. `None` (the default) keeps the adapter's
/// shared reqwest transport.
#[derive(Clone)]
pub struct FetchTransport(pub std::sync::Arc<dyn crate::ai::transport::HttpTransport>);

impl std::fmt::Debug for FetchTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FetchTransport(..)")
    }
}

/// Token budgets per thinking level for token-based providers (upstream
/// `ThinkingBudgets`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

/// Payload observer: may inspect or replace the request payload before it is
/// serialized (upstream `onPayload`; mutating in place models both).
pub type StreamHookMut = std::sync::Arc<dyn Fn(&mut JsonValue) + Send + Sync>;
/// Read-only variant (adapter-owned parsed event data).
pub type StreamHook = std::sync::Arc<dyn Fn(&JsonValue) + Send + Sync>;
/// Response observer: `(status, headers)` (upstream `onResponse`).
pub type ResponseHook = std::sync::Arc<dyn Fn(u16, &BTreeMap<String, String>) + Send + Sync>;

// ---------------------------------------------------------------------------
// Transport-retry seam (upstream auto-retry + `RetryStatusIndicator`)
// ---------------------------------------------------------------------------

/// A scheduled transport retry (upstream `auto_retry_start` payload,
/// agent-session.ts:3802-3807): `attempt` is the 1-based ordinal of the retry
/// about to run, `max_attempts` the configured retry budget, `delay_ms` the
/// wait before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryNotice {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
}

/// Upstream `auto_retry_start` payload (agent-session.ts:220/3804-3810, also
/// `retryAssistantCall`'s `onRetryScheduled`): the retry about to be waited
/// out, with the error that triggered it. The transport fires an
/// [`AutoRetryStartHook`] before each retry wait — since the retry loop lives
/// in the transport, this is the single observation point for hosts that
/// cannot own the wait ([`RetryWaitHook`] hosts keep their countdown seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRetryStart {
    /// 1-based ordinal of the retry about to run (upstream `attempt`).
    pub attempt: u32,
    /// The configured retry budget (upstream `maxAttempts`).
    pub max_attempts: u32,
    /// The wait before the retry, after `Retry-After` handling (upstream
    /// `delayMs`).
    pub delay_ms: u64,
    /// The error that triggered the retry (upstream `errorMessage`): the
    /// transport error for a failed send/body read, `HTTP {status}` for a
    /// retryable response (the error body is not drained on the retry path).
    pub error_message: String,
}

/// Upstream `auto_retry_end` payload (agent-session.ts:221/1181/1961/3776,
/// also `retryAssistantCall`'s `onRetryFinished`): the outcome of a request
/// that scheduled at least one retry. Fired exactly once per request, after
/// its final attempt resolves (success, final failure, or cancellation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRetryEnd {
    /// Whether the retried request eventually succeeded.
    pub success: bool,
    /// The retry ordinal that succeeded / the number of retries scheduled
    /// (upstream `attempt`).
    pub attempt: u32,
    /// The final error (upstream `finalError`; omitted on success). A user
    /// cancellation reports `"Retry cancelled"` like upstream's
    /// `_finishCancelledRetry`.
    pub final_error: Option<String>,
}

/// `auto_retry_start` observer: fired before each retry wait. Observation
/// only — the transport's retry decision, backoff and abort semantics are
/// identical with or without an installed hook.
pub type AutoRetryStartHook = std::sync::Arc<dyn Fn(&AutoRetryStart) + Send + Sync>;

/// `auto_retry_end` observer: fired once when a request that scheduled at
/// least one retry resolves. Observation only.
pub type AutoRetryEndHook = std::sync::Arc<dyn Fn(&AutoRetryEnd) + Send + Sync>;

/// What the retry wait should do (see [`RetryHook`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Wait out the delay and retry (or give up on the retry count).
    Proceed,
    /// The user cancelled the retry (upstream `abortRetry()`): the wait ends
    /// immediately and the request fails with
    /// [`AiError::RetryCancelled`](crate::ai::error::AiError::RetryCancelled).
    Cancelled,
}

/// The retry-wait seam: the transport hands a scheduled retry to the hook,
/// which OWNS the wait so a host can render the upstream
/// `RetryStatusIndicator` countdown and cancel it on a keypress (upstream
/// `_prepareRetry`'s abortable `sleep(delayMs, controller.signal)`, with the
/// abort controller fired by `abortRetry()`). The transport still honours the
/// request's own abort signal around the hook; without an installed hook it
/// sleeps on its own (the pre-existing behaviour).
pub type RetryHook = std::sync::Arc<
    dyn Fn(RetryNotice) -> crate::agent::tool_execution::BoxFuture<'static, RetryOutcome>
        + Send
        + Sync,
>;

/// Optional [`RetryHook`] slot with a non-`dyn` `Debug` (payload hooks must
/// not print the closure) — carried by [`StreamOptions`] and
/// [`ProviderRequestConfig`](crate::ai::transport::ProviderRequestConfig).
#[derive(Clone, Default)]
pub struct RetryWaitHook {
    hook: Option<RetryHook>,
}

impl RetryWaitHook {
    pub fn new(hook: RetryHook) -> Self {
        Self { hook: Some(hook) }
    }

    pub fn is_installed(&self) -> bool {
        self.hook.is_some()
    }

    pub fn hook(&self) -> Option<&RetryHook> {
        self.hook.as_ref()
    }
}

impl std::fmt::Debug for RetryWaitHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryWaitHook")
            .field("installed", &self.hook.is_some())
            .finish()
    }
}

/// Observable transport hooks (upstream `ProviderRequestOptions.onPayload`,
/// `StreamOptions.onResponse`, `StreamOptions.onProviderStreamEvent`, plus the
/// per-request extension and retry seams). Every slot defaults to `None`,
/// which costs nothing at the call sites.
#[derive(Clone, Default)]
pub struct ProviderStreamHooks {
    /// Inspect (or replace, by mutating) the request payload before it is
    /// serialized (upstream `onPayload`; a return value replaces the payload
    /// there, so mutating in place models both).
    pub on_payload: Option<StreamHookMut>,
    /// Invoked once the HTTP response status/headers are known (upstream
    /// `onResponse`).
    pub on_response: Option<ResponseHook>,
    /// Invoked for each parsed provider stream event before pir normalization
    /// (upstream `onProviderStreamEvent`; adapter-owned data, read-only).
    pub on_provider_stream_event: Option<StreamHook>,
    /// Transport-level async `before_provider_request` payload hook
    /// (per-request: carried on the StreamOptions instead of the former
    /// process-wide transport slot, so concurrent sessions never overwrite —
    /// or leak into — each other's extension dispatch). Installed by the
    /// session host when an extension registered a handler.
    pub before_provider_request: Option<crate::ai::transport::BeforeProviderRequestHook>,
    /// Transport-level async `after_provider_response` observer (per-request,
    /// same rationale as [`ProviderStreamHooks::before_provider_request`]).
    pub after_provider_response: Option<crate::ai::transport::AfterProviderResponseHook>,
    /// The retry-wait seam (upstream auto-retry countdown + `abortRetry`):
    /// when installed, the transport hands each scheduled retry to the hook,
    /// which performs the wait and may cancel it. Unset = the transport
    /// sleeps on its own (see [`RetryWaitHook`]).
    pub on_retry: RetryWaitHook,
    /// `auto_retry_start` observer ([`AutoRetryStart`]): fired before each
    /// retry wait. Independent of [`ProviderStreamHooks::on_retry`] (a host
    /// that only observes, without owning the wait, installs this one).
    pub on_retry_start: Option<AutoRetryStartHook>,
    /// `auto_retry_end` observer ([`AutoRetryEnd`]): fired once when a
    /// request that scheduled at least one retry resolves.
    pub on_retry_end: Option<AutoRetryEndHook>,
}

impl std::fmt::Debug for ProviderStreamHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderStreamHooks")
            .field("on_payload", &self.on_payload.is_some())
            .field("on_response", &self.on_response.is_some())
            .field(
                "on_provider_stream_event",
                &self.on_provider_stream_event.is_some(),
            )
            .field(
                "before_provider_request",
                &self.before_provider_request.is_some(),
            )
            .field(
                "after_provider_response",
                &self.after_provider_response.is_some(),
            )
            .field("on_retry", &self.on_retry)
            .field("on_retry_start", &self.on_retry_start.is_some())
            .field("on_retry_end", &self.on_retry_end.is_some())
            .finish()
    }
}

/// Bedrock request-level configuration (upstream `BedrockOptions`).
///
/// Every field overrides the corresponding environment-derived value for this
/// request only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct BedrockRequestOptions {
    /// AWS region (overrides `AWS_REGION`/`AWS_DEFAULT_REGION`).
    pub region: Option<String>,
    /// AWS shared profile (overrides `AWS_PROFILE` for credential resolution).
    pub profile: Option<String>,
    /// Cost-allocation tags attached to the inference request (upstream
    /// `requestMetadata`).
    pub request_metadata: Option<BTreeMap<String, String>>,
    /// Bedrock API-key bearer token (overrides
    /// `AWS_BEARER_TOKEN_BEDROCK`).
    pub bearer_token: Option<String>,
}

/// Redacting `Debug`: the Bedrock bearer token is a credential.
impl fmt::Debug for BedrockRequestOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockRequestOptions")
            .field("region", &self.region)
            .field("profile", &self.profile)
            .field("request_metadata", &self.request_metadata)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Azure OpenAI request-level endpoint configuration (upstream
/// `AzureEndpointOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AzureRequestOptions {
    /// `api-version` query value (overrides `AZURE_OPENAI_API_VERSION`).
    pub api_version: Option<String>,
    /// Resource name (builds `https://{name}.openai.azure.com/openai/v1`).
    pub resource_name: Option<String>,
    /// Full base URL (overrides `AZURE_OPENAI_BASE_URL` and the model).
    pub base_url: Option<String>,
    /// Deployment name (overrides the model-id deployment map).
    pub deployment_name: Option<String>,
}

/// Stream-specific options.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    /// Requested thinking level; emitted as `reasoning_effort` for reasoning
    /// models on the OpenAI-compatible path (see
    /// `api::openai::apply_reasoning_effort`).
    pub reasoning: Option<ThinkingLevel>,
    /// How thinking content is returned (upstream `thinkingDisplay` on the
    /// Anthropic/Bedrock adapters). `None` keeps pir's historical
    /// `"summarized"` default.
    pub thinking_display: Option<ThinkingDisplay>,
    /// Whether to request the Anthropic interleaved-thinking beta for
    /// non-adaptive thinking models (upstream `interleavedThinking`).
    /// `None`/`Some(true)` keep pir's historical always-on behavior.
    pub interleaved_thinking: Option<bool>,
    /// OpenAI Responses `reasoning.summary` (upstream `reasoningSummary`).
    /// `None` keeps pir's historical `"auto"` default.
    pub reasoning_summary: Option<ReasoningSummary>,
    /// OpenAI Responses `text.verbosity` (upstream `textVerbosity`).
    /// `None` keeps pir's historical `"low"` default.
    pub text_verbosity: Option<TextVerbosity>,
    /// Arbitrary sampling parameters merged into the request body as-is, after
    /// the named request fields, so keys here override them (upstream
    /// `StreamOptions.samplingParams`). Merged over the model's
    /// `samplingParams` per key; only applied by OpenAI-compatible adapters.
    pub sampling_params: Option<BTreeMap<String, JsonValue>>,
    pub metadata: Option<BTreeMap<String, JsonValue>>,
    /// Per-request tool-choice override (upstream
    /// `OpenAICompletionsOptions.toolChoice`; sent as `tool_choice`).
    pub tool_choice: Option<ToolChoice>,
    /// Maximum provider long-poll duration for a deferred fetch, in
    /// milliseconds (upstream `DeferredFetchOptions.wait`). `None`/`0`
    /// performs one status check.
    pub wait: Option<u64>,
    /// Per-request client identity (upstream `ProviderRequestOptions.identity`):
    /// `User-Agent` and provider attribution headers. Overrides the
    /// registry-wide identity field by field.
    pub identity: Option<crate::ai::types::ClientIdentity>,
    /// Transform the fully assembled headers before dispatch (upstream
    /// `ModelsRequestTransforms.transformHeaders`).
    pub transform_headers: crate::ai::types::HeaderTransformHook,
    /// Per-request HTTP transport injection (upstream `fetch`).
    pub fetch: Option<crate::ai::types::FetchTransport>,
    /// Bedrock request-level configuration (region/profile/request
    /// metadata/bearer token).
    pub bedrock: Option<crate::ai::types::BedrockRequestOptions>,
    /// Azure OpenAI request-level endpoint configuration.
    pub azure: Option<crate::ai::types::AzureRequestOptions>,
    /// Per-request thinking token budgets (upstream
    /// `OpenAICompletionsOptions.thinkingBudgets`), consumed when the model
    /// sets a `thinkingTokenBudgetField`.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Requested OpenAI service tier (upstream `service_tier` on the
    /// Responses API; `flex`/`priority`/`fast` repricing applies to usage
    /// cost).
    pub service_tier: Option<String>,
    /// Ask the pi-messages gateway for debug metadata (upstream
    /// `PiMessagesOptions.debug`, pi-messages.ts:36-37); appended as
    /// `?debug=1`. Other adapters ignore it.
    pub debug: Option<bool>,
    /// Observable transport hooks (payload / response / raw stream events).
    pub hooks: ProviderStreamHooks,
    /// Explicit parent span for telemetry produced by this logical request
    /// (upstream `ProviderRequestOptions.telemetry`).
    ///
    /// `None` (the default) is the fully disabled path: no span is started, no
    /// metric is recorded, and no propagation header is added to the provider
    /// request. `StreamOptions` is request-scoped runtime state (never
    /// serialized), so the span handle stays out of every persisted format;
    /// adding the field with a `None` default keeps every existing constructor
    /// and `..Default::default()` call site compiling unchanged.
    ///
    /// [`crate::ai::Context`] deliberately does NOT carry this handle: upstream
    /// `Context` is the transcript (`systemPrompt`/`messages`/`tools`), while
    /// the parent span rides the request options. Keeping it off `Context`
    /// preserves the context's serde shape and its `PartialEq` comparison.
    pub telemetry: Option<crate::telemetry::Span>,
}

/// Redacting `Debug`: `api_key` is replaced by a marker. Request-scoped
/// options are routinely logged by tracing/error paths; the credential must
/// never appear there.
impl fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamOptions")
            .field("signal", &self.signal)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field(
                "env_keys",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .field(
                "header_keys",
                &self
                    .headers
                    .as_ref()
                    .map(|headers| headers.keys().collect::<Vec<_>>()),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field("reasoning", &self.reasoning)
            .field("thinking_display", &self.thinking_display)
            .field("interleaved_thinking", &self.interleaved_thinking)
            .field("reasoning_summary", &self.reasoning_summary)
            .field("text_verbosity", &self.text_verbosity)
            .field("sampling_params", &self.sampling_params)
            .field("metadata", &self.metadata)
            .field("tool_choice", &self.tool_choice)
            .field("wait", &self.wait)
            .field("identity", &self.identity)
            .field("transform_headers", &self.transform_headers)
            .field("fetch", &self.fetch)
            .field("bedrock", &self.bedrock)
            .field("azure", &self.azure)
            .field("thinking_budgets", &self.thinking_budgets)
            .field("service_tier", &self.service_tier)
            .field("debug", &self.debug)
            .field("hooks", &self.hooks)
            .field("telemetry", &self.telemetry)
            .finish()
    }
}

/// Provider-neutral options accepted by the simple request entry points
/// (upstream `SimpleStreamOptions`).
///
/// The simple entry points ([`crate::ai::provider::ProviderRegistry::stream_simple`]
/// and `complete_simple`) take the same [`StreamOptions`] surface but resolve
/// the model-derived defaults upstream applies in every API's `streamSimple`:
/// the requested thinking level is clamped to the model's supported levels
/// (`getSupportedThinkingLevels`/`clampThinkingLevel`) and
/// `thinking_budgets` are consumed by the adapters' budget-based thinking
/// paths.
pub type SimpleStreamOptions = StreamOptions;

/// Prompt cache retention preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CacheRetention {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "short")]
    Short,
    #[serde(rename = "long")]
    Long,
}

// ---------------------------------------------------------------------------
// Event stream protocol
// ---------------------------------------------------------------------------

/// Events emitted by an `AssistantMessageEventStream`.
///
/// NOTE (memory review 2026-10): the `partial` snapshot of each non-terminal
/// event is a full `AssistantMessage` by design — each delta event carries a
/// cumulative snapshot of the stream so far, so the producer-side deep copy
/// per delta is inherent to the wire protocol (every event needs its own
/// value). The field is `Arc<AssistantMessage>` so consumers (agent loop,
/// session host, RPC/TUI fan-out, queued streams) share one allocation
/// instead of deep-copying per observer, and so adapter branches whose
/// provider delta leaves the message unchanged (e.g. tool-call JSON argument
/// deltas) can hand out the same `Arc` zero-copy. Serde forwards through
/// `Arc<T>` transparently: outbound JSON is byte-identical to the plain
/// value it replaced (inbound deserialization requires serde's `rc`
/// feature).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum AssistantMessageEvent {
    /// Stream started (first event).
    #[serde(rename = "start")]
    Start {
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Text content block started.
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Text delta chunk.
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        delta: String,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Text content block ended.
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Thinking content block started.
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Thinking delta chunk.
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        delta: String,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Thinking content block ended.
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Tool call block started.
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Tool call delta chunk (JSON arguments).
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        delta: String,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Tool call block ended with full ToolCall.
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(alias = "contentIndex")]
        content_index: usize,
        #[serde(alias = "toolCall")]
        tool_call: ToolCall,
        #[serde(default)]
        partial: Arc<AssistantMessage>,
    },
    /// Stream completed successfully.
    #[serde(rename = "done")]
    Done {
        reason: TerminalReason,
        message: AssistantMessage,
    },
    /// Stream ended with an error.
    #[serde(rename = "error")]
    Error {
        reason: ErrorReason,
        error: AssistantMessage,
    },
}

/// Terminal reasons for successful completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TerminalReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "deferred")]
    Deferred,
}

/// Error reasons for error termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ErrorReason {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

// ---------------------------------------------------------------------------
// Abort signal
// ---------------------------------------------------------------------------

/// An abort signal carrying an await-able cancellation primitive.
///
/// Internally a shared [`tokio_util::sync::CancellationToken`]: `abort()`
/// wakes every pending [`AbortSignal::cancelled`] waiter immediately, while
/// `is_aborted()` keeps the previous synchronous flag semantics so passive
/// checkpoint checks are unchanged.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    token: tokio_util::sync::CancellationToken,
}

impl Default for AbortSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortSignal {
    /// Create a new abort signal in the un-aborted state.
    pub fn new() -> Self {
        Self {
            token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Returns true if the operation has been aborted.
    pub fn is_aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Abort the operation, waking every pending [`AbortSignal::cancelled`]
    /// waiter immediately.
    pub fn abort(&self) {
        self.token.cancel();
    }

    /// Resolve as soon as the signal is aborted.
    ///
    /// Clones share one token, so a waiter holding any clone observes an
    /// `abort()` on any other clone. The future never resolves spuriously:
    /// it borrows `self`, so the token cannot be dropped out from under it.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Check if aborted; returns an error if so.
    pub fn check(&self) -> Result<(), AbortedError> {
        if self.is_aborted() {
            Err(AbortedError)
        } else {
            Ok(())
        }
    }

    /// A signal aborted as soon as ANY of `signals` is aborted (upstream
    /// `AbortSignal.any`). Already-aborted inputs abort the result
    /// immediately; the waiters are cheap detached tasks, so call this from an
    /// async (tokio) context.
    pub fn any(signals: impl IntoIterator<Item = AbortSignal>) -> AbortSignal {
        let combined = AbortSignal::new();
        for signal in signals {
            if signal.is_aborted() {
                combined.abort();
                return combined;
            }
            let combined_clone = combined.clone();
            tokio::spawn(async move {
                signal.cancelled().await;
                combined_clone.abort();
            });
        }
        combined
    }
}

/// Error returned when an operation was aborted.
#[derive(Debug, Clone, Copy)]
pub struct AbortedError;

impl std::fmt::Display for AbortedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation aborted")
    }
}

impl std::error::Error for AbortedError {}
