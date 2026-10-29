//! Shared utility functions for the AI module.

use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

use crate::ai::types::{AssistantMessage, JsonValue, Model, StopReason, Usage};

/// A regex compiled from a compile-time-constant pattern.
///
/// The patterns in this module are upstream-authored string literals; a
/// malformed one is a test-time failure. At runtime a compile failure
/// degrades to "never matches" (explicit handling instead of `expect`), so
/// constructing the static patterns is infallible.
#[derive(Debug, Clone)]
struct StaticRegex {
    inner: Option<Regex>,
}

impl StaticRegex {
    fn new(pattern: &str) -> Self {
        Self {
            inner: Regex::new(pattern).ok(),
        }
    }

    fn is_match(&self, text: &str) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|regex| regex.is_match(text))
    }
}

/// Current Unix timestamp in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Context overflow classification (upstream packages/ai/src/utils/overflow.ts)
// ---------------------------------------------------------------------------

/// Regex patterns detecting context overflow errors per provider (upstream
/// `OVERFLOW_PATTERNS`; keep in sync with its provider table: Anthropic
/// "prompt is too long", z.ai CN code-1261 "Prompt too long"/"Prompt exceeds
/// max length", Anthropic 413 `request_too_large`, OpenAI/LiteLLM, Google
/// Gemini, xAI, Groq, OpenRouter (incl. Poolside), Together AI, llama.cpp,
/// LM Studio, GitHub Copilot, MiniMax, Kimi For Coding, DS4, Mistral,
/// Xiaomi MiMo, DashScope/Qwen, Ollama, a provider-independent HTTP 413
/// request-size rule, and generic fallbacks).
fn overflow_patterns() -> &'static [StaticRegex] {
    static PATTERNS: std::sync::OnceLock<Vec<StaticRegex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"prompt (?:is )?too long",   // Anthropic and z.ai token overflow
            r"prompt exceeds max length", // z.ai CN endpoint token overflow
            r"request_too_large",         // Anthropic request byte-size overflow (HTTP 413)
            r"input is too long for requested model", // Amazon Bedrock
            r"exceeds the context window", // OpenAI (Completions & Responses API)
            r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))", // OpenAI-compatible proxies (LiteLLM)
            r"input token count.*exceeds the maximum", // Google (Gemini)
            r"maximum prompt length is \d+",           // xAI (Grok)
            r"reduce the length of the messages",      // Groq
            r"maximum context length is \d+ tokens",   // OpenRouter (most backends)
            r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
            r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", // Together AI
            r"exceeds the limit of \d+",           // GitHub Copilot
            r"exceeds the available context size", // llama.cpp server
            r"greater than the context length",    // LM Studio
            r"context window exceeds limit",       // MiniMax
            r"exceeded model token limit",         // Kimi For Coding
            r"too large for model with \d+ maximum context length", // Mistral
            r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", // DS4 server
            r"model_context_window_exceeded", // z.ai non-standard finish_reason
            r"prompt too long; exceeded (?:max )?context length", // Ollama explicit overflow
            r"range of input length should be", // DashScope / Qwen Token Plan
            r"context[_ ]length[_ ]exceeded", // Generic fallback
            r"too many tokens",               // Generic fallback
            r"token limit exceeded",          // Generic fallback
            // HTTP 413 means request-size overflow regardless of provider or
            // body text, so Copilot's large-image / large-body failures take
            // the compact-and-retry path (#11016). Match either
            // `formatProviderError`'s "<provider> API error (413):" prefix or
            // a raw status at the start / end of the message, not a status
            // mentioned elsewhere in the body. The transport's real 413 is
            // rendered into one of these forms by
            // `ai::api::terminal_error_message` / `_prefixed_` (the adapter
            // finalizer), so the rule is reachable from actual failures — not
            // only from handcrafted text. The prefix is any run of characters
            // that cannot cross a `:` (which would let a `(400)` message
            // swallow a later `(413)` mention) or a line break; it is
            // deliberately not an ASCII word class because provider ids are
            // extension-chosen and may contain `.`/spaces (`pi.registerProvider`
            // accepts `my.provider`).
            r"^(?:[^\r\n:]+ API error \(413\):|413(?:\s|:|$))",
        ]
        .iter()
        .map(|pattern| StaticRegex::new(&format!("(?i){pattern}")))
        .collect()
    })
}

/// Cerebras returns bodyless 400/413 responses for oversized requests
/// (upstream `CEREBRAS_BODYLESS_OVERFLOW_PATTERN`, scoped to that provider).
fn cerebras_bodyless_overflow_pattern() -> &'static StaticRegex {
    static PATTERN: std::sync::OnceLock<StaticRegex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| StaticRegex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)"))
}

/// Patterns indicating non-overflow errors (rate limiting, server errors)
/// excluded from overflow detection (upstream `NON_OVERFLOW_PATTERNS`).
fn non_overflow_patterns() -> &'static [StaticRegex] {
    static PATTERNS: std::sync::OnceLock<Vec<StaticRegex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // AWS Bedrock non-overflow errors (human-readable prefixes from
            // formatBedrockError).
            r"^(Throttling error|Service unavailable):",
            r"rate limit",        // Generic rate limiting
            r"too many requests", // Generic HTTP 429 style
        ]
        .iter()
        .map(|pattern| StaticRegex::new(&format!("(?i){pattern}")))
        .collect()
    })
}

/// Check if an assistant message represents a context overflow error (upstream
/// `isContextOverflow`): error-message pattern matching, z.ai-style silent
/// overflow (successful response whose input usage exceeds the window), and
/// Xiaomi MiMo-style length stops with zero output on a filled window.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1: Check error message patterns.
    if message.stop_reason == StopReason::Error {
        if let Some(error_message) = &message.error_message {
            let is_non_overflow = non_overflow_patterns()
                .iter()
                .any(|pattern| pattern.is_match(error_message));
            if !is_non_overflow {
                if overflow_patterns()
                    .iter()
                    .any(|pattern| pattern.is_match(error_message))
                {
                    return true;
                }
                if message.provider == "cerebras"
                    && cerebras_bodyless_overflow_pattern().is_match(error_message)
                {
                    return true;
                }
            }
        }
    }

    let Some(context_window) = context_window else {
        return false;
    };
    let input_tokens = message.usage.input + message.usage.cache_read;

    // Case 2: Silent overflow (z.ai style) - successful but usage exceeds context.
    if message.stop_reason == StopReason::Stop && input_tokens > context_window {
        return true;
    }

    // Case 3: Length-stop overflow (Xiaomi MiMo style) - server truncates
    // oversized input to fit the context window, leaving no room for output.
    if message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input_tokens >= (context_window as f64 * 0.99) as u64
    {
        return true;
    }

    false
}

/// Check whether a length stop ended below the caller or model's intended
/// output limit (upstream `isRecoverableLength`); such responses may be
/// retried after a bounded compact-and-retry attempt.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

// ---------------------------------------------------------------------------
// Transient-error classification (upstream packages/ai/src/utils/retry.ts)
// ---------------------------------------------------------------------------

/// Subscription/account limit errors that must fail fast even though they
/// often arrive wrapped in HTTP 429 (upstream
/// `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN`).
fn non_retryable_provider_limit_pattern() -> &'static StaticRegex {
    static PATTERN: std::sync::OnceLock<StaticRegex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        provider_error_pattern(&[
            // OpenCode Go/free-tier limits returned as 429 JSON error types by
            // OpenCode's Zen API; subscription/account limits, not transient
            // throttles.
            "GoUsageLimitError",
            "FreeUsageLimitError",
            "Monthly usage limit reached",
            "available balance",
            // Generic quota/budget/billing exhaustion (`insufficient_quota` is
            // OpenAI's quota/billing error code).
            "insufficient_quota",
            "out of budget",
            "quota exceeded",
            "billing",
            // Sign in with ChatGPT: the subscription's shared usage limit,
            // which resets after hours rather than seconds.
            "subscription_sharing_usage_limit_exceeded",
            // Sign in with ChatGPT on a ChatGPT Business plan: a usage limit
            // set by the workspace admin; only the admin can raise it
            // (retry.ts:30).
            "subscription_sharing_workplace_individual_usage_limit_exceeded",
        ])
    })
}

/// Transient provider, transport, and stream failures worth retrying (upstream
/// `RETRYABLE_PROVIDER_ERROR_PATTERN`, including "model is at capacity",
/// "server_busy"/"servers are currently busy", HTTP/2 "pending stream has been
/// canceled", and premature stream endings).
fn retryable_provider_error_pattern() -> &'static StaticRegex {
    static PATTERN: std::sync::OnceLock<StaticRegex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        provider_error_pattern(&[
            // Generic provider load and server-side transient failures.
            "overloaded",
            "server_busy",
            "servers are currently busy",
            "currently experiencing high demand",
            "model is at capacity",
            "rate.?limit",
            "too many requests",
            // HTTP status codes are matched by
            // [`contains_retryable_status_code`] instead of this alternation:
            // upstream guards them with ASCII lookaround assertions, which the
            // regex crate does not support (#11057).
            // Also matches "The service is temporarily unavailable" (#10965).
            "service.?(?:is.?)?(?:temporarily.?)?unavailable",
            "server.?error",
            "internal.?error",
            // Wrapper/provider text for transient upstream failures, including
            // OpenRouter "Provider returned error" responses and gateway
            // wording such as "The upstream provider returned an error"
            // (upstream #10842: the `(?:an.?)?` alternative).
            "provider.?returned.?(?:an.?)?error",
            "exceeded request buffer limit while retrying upstream",
            // Gateway upstream failures relayed as OpenAI Responses
            // `response.failed` codes without an HTTP status (upstream #10920,
            // drift item D34): the same wording appears as an error code and as
            // prose ("Stream was interrupted", "Upstream HTTP/2 stream failed").
            "unable to forward upstream",
            "upstream.?request.?failed",
            "upstream_stream_read_error",
            "stream was interrupted",
            "http/?2 stream failed",
            // Network, proxy, and fetch transport failures.
            "network.?error",
            "connection.?error",
            "connection.?refused",
            "connection.?lost",
            "other side closed",
            "fetch failed",
            // node-fetch system errors, e.g. the Google ADC OAuth token
            // refresh used by google-vertex; the reason is often empty
            // (#10996, drift D37).
            r"request to \S+ failed, reason:",
            "getaddrinfo",
            "ENOTFOUND",
            "EAI_AGAIN",
            "upstream.?connect",
            "reset before headers",
            "socket hang up",
            "socket connection was closed",
            "timed? out",
            "timeout",
            "terminated",
            // WebSocket transports can report close/error text instead of
            // HTTP/fetch text.
            "websocket.?closed",
            "websocket.?error",
            // Premature stream endings from SDKs and transports: Anthropic
            // "stream ended without ...", Bedrock/Smithy HTTP/2 no-response
            // errors and ERR_HTTP2_STREAM_CANCEL ("pending stream has been
            // canceled").
            "ended without",
            "stream ended before message_stop",
            "stream ended before a terminal response event",
            "http2 request did not get a response",
            // Codex CLI mid-stream disconnect relayed by proxies that wrap
            // it, e.g. "stream disconnected before completion: stream closed
            // before response.completed" (#11057, retry.ts:99).
            "stream disconnected before completion",
            "pending stream has been canceled",
            // Provider-requested retry delay cap failures flow through the
            // outer retry policy so callers can surface/abort the backoff.
            "retry delay",
            // Explicit retry guidance emitted mid-stream by OpenAI Responses
            // and Bedrock stream exceptions.
            "you can retry your request",
            "try your request again",
            "please retry your request",
            // gRPC based providers (e.g. NVIDIA NIM).
            "ResourceExhausted",
            // Sign in with ChatGPT: usage or user data temporarily
            // unavailable; can arrive mid-stream without an HTTP 503.
            "subscription_sharing_usage_unavailable",
            "subscription_sharing_user_unavailable",
        ])
    })
}

/// HTTP status codes classified as transient (upstream retry.ts:43).
const RETRYABLE_HTTP_STATUS_CODES: [&str; 7] = ["429", "500", "502", "503", "504", "520", "524"];

/// Lookaround-free equivalent of upstream's
/// `(?<![a-z0-9])(?:429|500|502|503|504|520|524)(?![a-z0-9])` (retry.ts:43;
/// matched case-insensitively, which does not affect digits).
///
/// A candidate occurrence only counts when neither adjacent character is
/// ASCII alphanumeric, so a status embedded in a request id
/// (`req_011CU503xyz`) or a byte count (`5242880 bytes`) is not retried
/// (#11057). Non-ASCII neighbours are delimiters, matching upstream's
/// ASCII-only lookaround. Scanning instead of regex keeps the behaviour
/// without lookaround support.
fn contains_retryable_status_code(text: &str) -> bool {
    RETRYABLE_HTTP_STATUS_CODES.iter().any(|code| {
        text.match_indices(code).any(|(start, _)| {
            let before = text[..start].chars().next_back();
            let after = text[start + code.len()..].chars().next();
            before.is_none_or(|ch| !ch.is_ascii_alphanumeric())
                && after.is_none_or(|ch| !ch.is_ascii_alphanumeric())
        })
    })
}

/// Join pattern fragments into one case-insensitive regex (upstream
/// `buildProviderErrorPattern`; fragments are upstream-authored regex pieces,
/// so they are joined verbatim, not escaped). Compile failures degrade to
/// "never matches" (see [`StaticRegex`]).
fn provider_error_pattern(fragments: &[&str]) -> StaticRegex {
    StaticRegex::new(&format!("(?i){}", fragments.join("|")))
}

/// Classifies whether a failed assistant message looks like a transient
/// provider or transport error, so callers can decide if the last assistant
/// turn should be restarted (upstream `isRetryableAssistantError`). Callers
/// should handle context overflow separately before applying their own retry
/// budget.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    if non_retryable_provider_limit_pattern().is_match(error_message) {
        return false;
    }
    retryable_provider_error_pattern().is_match(error_message)
        || contains_retryable_status_code(error_message)
}

/// Per-attempt delay for the agent-level retry policy (upstream
/// `retryDelayMs`): `baseDelayMs * 2^(attempt-1)`, capped at
/// `max_agent_delay_ms` (default 60 seconds).
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

pub fn retry_delay_ms(base_delay_ms: u64, attempt: u64, max_agent_delay_ms: Option<u64>) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    let delay = base_delay_ms.saturating_mul(1u64 << shift);
    delay.min(max_agent_delay_ms.unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS))
}

/// Agent-level turn retry policy (upstream `RetrySettings` as consumed by
/// `AgentSession._prepareRetry`, agent-session.ts:3792-3827): a turn that ends
/// with a retryable [`StopReason::Error`] is re-issued up to `max_retries`
/// times with [`retry_delay_ms`] backoff.
///
/// Built from the host settings by
/// `SettingsManager::agent_retry_policy`; the agent loop resolves it lazily,
/// only when an error turn actually arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRetryPolicy {
    /// Retries allowed per run (upstream `maxRetries`, default 3).
    pub max_retries: u32,
    /// First backoff (upstream `baseDelayMs`, default 2 s); doubled per attempt.
    pub base_delay_ms: u64,
    /// Ceiling for one backoff (upstream `maxAgentDelayMs`, default 60 s).
    pub max_agent_delay_ms: u64,
}

// ---------------------------------------------------------------------------
// ChatGPT usage-limit friendly messages (upstream packages/ai error handling)
// ---------------------------------------------------------------------------

/// The ChatGPT usage dashboard (upstream `CHATGPT_USAGE_URL`,
/// openai-responses.ts:34).
pub const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// Append the ChatGPT usage-dashboard hint to an error message carrying the
/// "Sign in with ChatGPT" shared-usage-limit error code (upstream
/// openai-responses.ts:226-229: "Sign in with ChatGPT shares the
/// subscription's usage limit with other apps" — the limit resets after hours
/// rather than seconds, so point users at the dashboard). Message-text
/// enhancement only; retry classification is unchanged. Public for tests/.
pub fn append_chatgpt_usage_hint(message: &str) -> String {
    if message.contains("subscription_sharing_usage_limit_exceeded") {
        format!("{message}\nCheck your ChatGPT usage: {CHATGPT_USAGE_URL}")
    } else {
        message.to_string()
    }
}

// ---------------------------------------------------------------------------
// OpenRouter billed-cost accounting (upstream utils/openrouter-cost.ts)
// ---------------------------------------------------------------------------

/// Whether `model` routes through OpenRouter (upstream `isOpenRouterModel`):
/// the provider id or a base URL containing `openrouter.ai`.
pub fn is_openrouter_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains("openrouter.ai")
}

/// A finite, non-negative number, mirroring upstream `isValidCost`.
fn valid_cost(value: Option<&JsonValue>) -> Option<f64> {
    value
        .and_then(JsonValue::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// Replace the catalog total with OpenRouter's billed cost when the response
/// contains complete, valid accounting (upstream
/// `applyOpenRouterReportedCost`).
///
/// `reported` is the raw provider `usage` object: `cost` (amount charged to
/// the user's account), `is_byok`, and for BYOK requests `cost_details`
/// carrying the upstream provider cost. The per-component estimates are left
/// untouched; only `total` and `totalFromProvider` change.
///
/// Returns `true` when the billed total was applied.
pub fn apply_openrouter_reported_cost(usage: &mut Usage, reported: &JsonValue) -> bool {
    let Some(reported_cost) = valid_cost(reported.get("cost")) else {
        return false;
    };
    match reported.get("is_byok").and_then(JsonValue::as_bool) {
        Some(false) => {
            usage.cost.total = reported_cost;
            usage.cost.total_from_provider = Some(true);
            true
        }
        Some(true) => {
            let Some(details) = reported
                .get("cost_details")
                .filter(|value| value.is_object())
            else {
                return false;
            };
            // Upstream: a null aggregate falls back to prompt + completion
            // costs; otherwise the aggregate must itself be a valid cost.
            let upstream_cost = if details
                .get("upstream_inference_cost")
                .is_none_or(JsonValue::is_null)
            {
                let prompt = valid_cost(details.get("upstream_inference_prompt_cost"));
                let completions = valid_cost(details.get("upstream_inference_completions_cost"));
                match (prompt, completions) {
                    (Some(prompt), Some(completions)) => prompt + completions,
                    _ => return false,
                }
            } else {
                match valid_cost(details.get("upstream_inference_cost")) {
                    Some(cost) => cost,
                    None => return false,
                }
            };
            usage.cost.total = reported_cost + upstream_cost;
            usage.cost.total_from_provider = Some(true);
            true
        }
        // Absent/unknown BYOK marker: leave the estimate alone.
        _ => false,
    }
}
