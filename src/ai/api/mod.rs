//! Provider API adapters.
//!
//! Each provider family's adapter rides its `provider-…` cargo feature: the
//! adapter module, its construction arm in
//! [`crate::ai::providers::construction`], and every helper module only it
//! uses are compiled out together. The shared wire layers stay core:
//! [`transform`] (message transforms), [`constrained_sampling`] (strict
//! schema / grammar tool params), and the OpenAI Responses wire codec in
//! [`openai_responses::shared`], whose `parse_partial_json` reader is used by
//! the anthropic/bedrock/mistral streaming parsers and the TUI.
//!
//! `provider-openai` is the backbone: the OpenAI Completions adapter (plus
//! OpenRouter and every OpenAI-compatible variant), the three Responses
//! providers, and the pi-messages adapter (`pi_messages` rides the same
//! feature) all gate on it. Because other adapters borrow a few resolvers
//! from it, those live here in the core instead of inside the adapter module:
//! [`resolve_cache_retention`] (bedrock + responses) and the thinking-budget
//! resolvers (bedrock). The `openai_responses` module therefore keeps only
//! `shared` compiled without the feature, and the construction path reports
//! the disabled feature instead of falling back to another adapter.

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "provider-bedrock")]
pub mod bedrock;
pub mod constrained_sampling;
/// GitHub Copilot dynamic headers, shared by every Copilot-capable adapter.
pub mod copilot_headers;
#[cfg(feature = "provider-google")]
pub mod google;
#[cfg(feature = "provider-google")]
pub mod google_vertex;
#[cfg(feature = "provider-mistral")]
pub mod mistral;
// The plain OpenAI-compatible `/chat/completions` adapter is core: the
// `minimal` profile needs exactly one wire provider, and `reqwest` is already
// an unconditional dependency. `provider-openai` still gates the
// Responses/Codex/pi-messages adapters and the catalog auto-construction of
// every OpenAI-family model.
pub mod openai;
/// OpenAI Responses adapters (`openai-responses`, `openai-codex-responses`,
/// `azure-openai-responses`) plus their shared wire codec.
#[cfg(feature = "provider-openai")]
pub mod openai_responses;
// Without `provider-openai` the three Responses providers and their wire
// modules (`azure`/`codex`/`copilot`/`responses_compat`) are absent, but the
// shared codec stays compiled: `shared::parse_partial_json` is used by the
// anthropic/bedrock/mistral streaming parsers and the TUI, so only the
// adapter entry points are gated (inside `shared`, only
// `build_responses_params` and its `openai`/`responses_compat` dependencies
// are openai-only).
#[cfg(not(feature = "provider-openai"))]
pub mod openai_responses {
    //! The OpenAI Responses shared wire codec, compiled without the adapter
    //! modules in builds that disable `provider-openai`.
    pub mod shared;
}
#[cfg(feature = "provider-openai")]
pub mod pi_messages;
/// Final tool-call argument parsing shared by every wire adapter
/// (upstream `parseToolCallArguments`, #10897/#10944).
pub mod tool_args;
pub mod transform;

/// Bounded-cadence refresh of the in-flight tool-call argument mirror shared
/// by the streaming adapters (the Responses wire codec is core, so this stays
/// compiled in every feature set).
pub(crate) mod partial_mirror;

use serde_json::Value;

use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::{
    AbortSignal, AssistantMessage, CacheRetention, ContentBlock, ErrorReason, Model, StopReason,
    StreamOptions, ThinkingBudgets, ThinkingLevel,
};

use crate::ai::types::ProviderEnv;

/// Read a provider-scoped env value: the request env first, then the process
/// environment (upstream `getProviderEnvValue`). Compiled in every build; the
/// bedrock, google and openai-family adapters are its consumers, so a build
/// with only the remaining families sees it unused.
#[cfg_attr(
    not(any(
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
pub(crate) fn provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(env) = env {
        if let Some(value) = env.get(name) {
            return Some(value.clone());
        }
    }
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Shared adapter glue (SSE framing, stream spawn plumbing, level/effort maps)
// ---------------------------------------------------------------------------

/// Maximum bytes accepted for one SSE line (1 MiB, matching the MCP
/// protocol-frame cap).
///
/// The streaming parsers buffer a partial line across `feed` calls. Without a
/// bound, a provider (or a broken proxy) that never sends `\n` grows the
/// buffer for the lifetime of the stream. Every [`sse_feed_lines`] caller
/// therefore inherits this cap: a complete or partial line at or below it is
/// accepted unchanged, anything larger is a protocol violation that fails the
/// stream with [`AiError::NonRetryable`] instead of exhausting memory. The
/// check runs on the RAW bytes before UTF-8 decoding, so an oversized line
/// never allocates a decoded copy.
pub(crate) const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;

/// Feed one raw SSE body chunk through the shared framing: complete lines
/// (newline and trailing CR stripped) are handed to `on_line` in order, and a
/// trailing partial line — including multi-byte UTF-8 split across chunks —
/// is buffered in `buffer` until the next call.
pub(crate) fn sse_feed_lines(
    chunk: &[u8],
    buffer: &mut Vec<u8>,
    mut on_line: impl FnMut(&str) -> AiResult<()>,
) -> AiResult<()> {
    buffer.extend_from_slice(chunk);
    // INVARIANT (in-place line decode): each complete line is decoded straight
    // out of the accumulation buffer and the consumed prefix is drained ONCE
    // after the loop (or when a line fails), so a body chunk costs zero
    // per-line allocations and one memmove instead of the former
    // `Vec<u8>`-per-line copy plus a `drain(..=position)` that shifted the
    // remaining bytes left on every line. The prefix is drained up to and
    // including the failing line before the error returns, exactly the
    // boundary the per-line drain produced (lines already handed out are gone;
    // lines after the failure stay buffered).
    //
    // Aliasing: `buffer` is a caller-owned `&mut Vec<u8>` (the parsers move it
    // out of their state, `std::mem::take`, before calling), and `on_line`
    // only touches the parser's own state — never this buffer — so holding a
    // `&str` into it across the callback is sound. The borrow checker enforces
    // the same discipline for any future callback.
    let mut consumed = 0usize;
    let mut outcome = Ok(());
    while let Some(relative) = buffer[consumed..].iter().position(|&byte| byte == b'\n') {
        let end = consumed + relative;
        // Cap BEFORE the UTF-8 decode: a hostile/broken provider that sends
        // one enormous complete line must not make the parser allocate a
        // decoded `String` proportional to it. The newline-terminated line is
        // dropped with the failing prefix (the stream is unusable either way).
        if end - consumed > MAX_SSE_LINE_BYTES {
            let line_len = end - consumed;
            consumed = end + 1;
            outcome = Err(AiError::NonRetryable(format!(
                "SSE line exceeds the {MAX_SSE_LINE_BYTES}-byte (1 MiB) cap ({line_len} bytes)"
            )));
            break;
        }
        let line = String::from_utf8_lossy(&buffer[consumed..end]);
        if let Err(error) = on_line(line.trim_end_matches('\r')) {
            consumed = end + 1;
            outcome = Err(error);
            break;
        }
        consumed = end + 1;
    }
    buffer.drain(..consumed);
    if outcome.is_ok() && buffer.len() > MAX_SSE_LINE_BYTES {
        let buffered = buffer.len();
        // Drop the oversized partial line: the stream is unusable, and the
        // caller may keep the parser around while reporting the error.
        buffer.clear();
        return Err(AiError::NonRetryable(format!(
            "SSE line buffer exceeds the {MAX_SSE_LINE_BYTES}-byte (1 MiB) cap \
             without a newline ({buffered} bytes buffered)"
        )));
    }
    outcome
}

/// Rewrite a trailing-event parse failure (clean EOF, no protocol verdict)
/// into the retryable truncation class.
///
/// A `Json` error raised while flushing the LAST unterminated SSE event block
/// means the body ended inside the event's JSON: the transport read a
/// complete HTTP body, so no status verdict applies and the stream is simply
/// truncated. A malformed *terminated* event keeps [`AiError::Json`] (a
/// non-retryable protocol violation) because it never reaches this helper.
pub(crate) fn trailing_event_error(error: AiError) -> AiError {
    match error {
        AiError::Json(source) => AiError::StreamTruncated(format!(
            "stream ended with an incomplete trailing event: {source}"
        )),
        other => other,
    }
}

/// Whether an in-band provider error frame describes a transient failure.
///
/// The classification is structural first: a numeric HTTP status decides
/// (401/403 and other non-retryable 4xx are terminal; 408/409/429/5xx are
/// transient). Without a status, the provider's error code/type and message
/// text are scanned. Authentication/authorization/validation/billing verdicts
/// win over generic transient words, so an `invalid_request_error` that
/// mentions a rate limit stays non-retryable.
pub(crate) fn in_band_error_is_retryable(
    status: Option<u16>,
    code: Option<&str>,
    kind: Option<&str>,
    message: &str,
) -> bool {
    if let Some(status) = status {
        if status == 401 || status == 403 {
            return false;
        }
        if crate::ai::transport::is_retryable(status) {
            return true;
        }
        if (400..500).contains(&status) {
            return false;
        }
    }
    let code = code.unwrap_or("").to_ascii_lowercase();
    let kind = kind.unwrap_or("").to_ascii_lowercase();
    let message = message.to_ascii_lowercase();
    let signals = [code.as_str(), kind.as_str(), message.as_str()];
    const NON_RETRYABLE: &[&str] = &[
        "authentication",
        "unauthenticated",
        "unauthorized",
        "permission",
        "forbidden",
        "accessdenied",
        "access denied",
        "invalid_api_key",
        "invalid api key",
        "incorrect api key",
        "invalid_request",
        "invalid-argument",
        "invalid_argument",
        "invalidargument",
        "validation",
        "bad request",
        "badrequest",
        "insufficient_quota",
        "quota exceeded",
        "out of budget",
        "billing",
        "model_not_found",
    ];
    if signals
        .iter()
        .any(|signal| NON_RETRYABLE.iter().any(|needle| signal.contains(needle)))
    {
        return false;
    }
    const RETRYABLE: &[&str] = &[
        "overloaded",
        "rate_limit",
        "rate limit",
        "ratelimit",
        "too many requests",
        "resource_exhausted",
        "resource exhausted",
        "resourceexhausted",
        "throttl",
        "server_error",
        "server error",
        "internal error",
        "internal_error",
        "internal server",
        "model stream error",
        "service unavailable",
        "service_unavailable",
        "temporarily unavailable",
        "capacity",
        "server_busy",
        "server busy",
        "busy",
        "timeout",
        "timed out",
        "try again",
    ];
    signals
        .iter()
        .any(|signal| RETRYABLE.iter().any(|needle| signal.contains(needle)))
}

/// Build the typed error for a provider-authored in-band error frame.
pub(crate) fn provider_error_frame(
    text: impl Into<String>,
    status: Option<u16>,
    code: Option<&str>,
    kind: Option<&str>,
) -> AiError {
    let text = text.into();
    if in_band_error_is_retryable(status, code, kind, &text) {
        AiError::Retryable(text)
    } else {
        AiError::NonRetryable(text)
    }
}

/// Numeric HTTP status carried by an error object (`code`, `status`, or the
/// nested `error` object's same fields).
fn error_frame_status(error: &Value) -> Option<u16> {
    ["code", "status", "statusCode", "status_code"]
        .iter()
        .find_map(|key| {
            let value = error.get(*key)?;
            value
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .or_else(|| {
                    value
                        .as_str()
                        .and_then(|status| status.trim().parse::<u16>().ok())
                })
        })
}

/// String token of an error object field.
fn error_frame_token(error: &Value, key: &str) -> Option<String> {
    error.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Build the typed error for a structured provider error object.
///
/// Handles the OpenAI/Anthropic/Google/Mistral shapes (`message`, `type`,
/// `code`, `status`, optionally a nested `error` object) while leaving the
/// caller in charge of the display text. Only non-numeric `status` values
/// (e.g. Google's `RESOURCE_EXHAUSTED`) are classification tokens; numeric
/// ones are treated as HTTP statuses.
pub(crate) fn provider_error_frame_from_value(error: &Value, text: String) -> AiError {
    let nested = error.get("error").filter(|nested| nested.is_object());
    let status = error_frame_status(error).or_else(|| nested.and_then(error_frame_status));
    let code = error_frame_token(error, "code")
        .or_else(|| error_frame_token(error, "error_code"))
        .or_else(|| nested.and_then(|error| error_frame_token(error, "code")));
    let kind = error_frame_token(error, "type")
        .or_else(|| error_frame_token(error, "error_type"))
        .or_else(|| {
            error_frame_token(error, "status").filter(|status| status.parse::<u16>().is_err())
        })
        .or_else(|| nested.and_then(|error| error_frame_token(error, "type")))
        .or_else(|| {
            nested.and_then(|error| {
                error_frame_token(error, "status").filter(|status| status.parse::<u16>().is_err())
            })
        });
    provider_error_frame(text, status, code.as_deref(), kind.as_deref())
}

pub(crate) fn sse_data(body: &[u8]) -> Vec<String> {
    // ONE in-place scan of the (lossily decoded) body. The former shape first
    // allocated a normalized copy of the WHOLE body (`str::replace` for
    // CRLF), then a temporary `Vec<&str>` plus a joined `String` per event
    // block. The block/field rules are unchanged: an event ends at a blank
    // line, `data:` fields are trimmed and joined with `\n`, other fields and
    // empty blocks are dropped, and CRLF is accepted like bare LF.
    let text = String::from_utf8_lossy(body);
    let mut events = Vec::new();
    let mut event = String::new();
    let mut fields = 0usize;
    for raw_line in text.split('\n') {
        // `str::lines()` semantics: a line ending in CRLF yields the text
        // without its `\r`; a lone `\r` stays part of the line (and is then
        // trimmed off a `data:` value like any other whitespace).
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            if fields > 0 {
                events.push(std::mem::take(&mut event));
                fields = 0;
            }
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            if fields > 0 {
                event.push('\n');
            }
            event.push_str(data.trim());
            fields += 1;
        }
    }
    if fields > 0 {
        events.push(event);
    }
    events
}

/// The assistant message every adapter stream starts from: provider/model
/// identity plus the current timestamp (upstream base message construction).
pub(crate) fn base_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        timestamp: crate::ai::util::now_ms(),
        ..AssistantMessage::default()
    }
}

/// Map a thinking level to the provider's `effort` string (upstream
/// adapters' `mapThinkingLevelToEffort`): the model's `thinkingLevelMap`
/// override first, then the default low/medium/high ladder. Only the bedrock
/// and anthropic adapters consume it today.
#[cfg_attr(
    not(any(feature = "provider-bedrock", feature = "provider-anthropic")),
    allow(dead_code)
)]
pub(crate) fn map_thinking_level_to_effort(model: &Model, level: Option<ThinkingLevel>) -> String {
    if let Some(level) = level {
        if let Some(mapped) = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(level.wire_key()))
            .cloned()
            .flatten()
        {
            return mapped;
        }
    }
    match level {
        Some(ThinkingLevel::Minimal) | Some(ThinkingLevel::Low) => "low".to_string(),
        Some(ThinkingLevel::Medium) => "medium".to_string(),
        _ => "high".to_string(),
    }
}

/// Apply the per-call retry overrides from [`StreamOptions`] to a cloned
/// provider client (the shared client keeps its own defaults otherwise).
/// Every provider adapter calls this; only a build with no provider family
/// at all sees it unused.
#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-mistral",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
pub(crate) fn apply_stream_retry_overrides(
    client: &mut ProviderRequestClient,
    options: &StreamOptions,
) {
    if let Some(max_retries) = options.max_retries {
        client.config.max_retries = max_retries;
    }
    if let Some(delay_ms) = options.max_retry_delay_ms {
        client.config.max_retry_delay = std::time::Duration::from_millis(delay_ms);
    }
    // Per-request total timeout (upstream `ProviderRequestOptions.timeoutMs`).
    client.set_request_timeout(
        options
            .timeout_ms
            .filter(|timeout| *timeout > 0)
            .map(std::time::Duration::from_millis),
    );
    // Per-request transport injection (upstream `fetch`): every adapter
    // clones its client per request before calling this, so the override
    // never leaks into another request.
    if let Some(fetch) = options.fetch.as_ref() {
        client.set_transport(fetch.0.clone());
    }
    // The retry-wait seam (upstream `RetryCallbacks` / the TUI countdown)
    // travels on `options.hooks.on_retry`, which every adapter passes to the
    // transport call: no config copy is needed here.
}

/// Classify a provider stream's terminal error upstream-faithfully: the
/// request's abort SIGNAL is the source of truth, not the surfaced error value
/// (`output.stopReason = options?.signal?.aborted ? "aborted" : "error"`,
/// `pi/packages/ai/src/api/openai-completions.ts:716`).
///
/// The error value cannot answer the question. A body read that is in flight
/// when the consumer aborts surfaces whatever the transport/parser yields at
/// that instant — a `reqwest`/`Http` error, a clean EOF, or a "stream ended
/// without finish_reason" failure for a server that only ever sent in-protocol
/// keepalives — because the transport only re-checks the signal BETWEEN body
/// reads. All of those are genuine failures, yet the caller's abort is what
/// the terminal message must record.
///
/// [`AiError::Aborted`] remains an abort even without a signal: adapter-internal
/// cancellation (consumer abandonment, retry cancellation) reports it directly.
/// When the signal is NOT aborted this is exactly the previous
/// `AiError::Aborted => Aborted, _ => Error` mapping, so genuine non-abort
/// failures keep their class.
#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-mistral",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
pub(crate) fn terminal_error_classification(
    error: &AiError,
    signal: Option<&AbortSignal>,
) -> (ErrorReason, StopReason) {
    if signal.is_some_and(AbortSignal::is_aborted) || matches!(error, AiError::Aborted) {
        (ErrorReason::Aborted, StopReason::Aborted)
    } else {
        (ErrorReason::Error, StopReason::Error)
    }
}

/// Build the terminal error message a provider adapter emits for `error`.
///
/// This is the exact construction [`finish_provider_stream`] performs: the
/// adapter text (`user_message` + raw metadata; a transport 413 takes
/// upstream's `formatProviderError` shape instead, see
/// [`upstream_http_error_text`]), the terminal `stop_reason`
/// classification, and the structural retry class the agent loop uses for its
/// turn-retry decision (see [`crate::ai::error::TurnRetryClass`]). Public so
/// hosts/embedders and tests that surface an `AiError` directly terminate with
/// the same classified message the real adapters emit.
pub fn terminal_error_message(
    model: &Model,
    error: &AiError,
    signal: Option<&AbortSignal>,
) -> AssistantMessage {
    let text = upstream_http_error_text(model, "", error)
        .unwrap_or_else(|| append_raw_error_metadata(&error.user_message()));
    terminal_error_message_with_text(model, error, text, signal)
}

/// [`terminal_error_message`] for adapters that prefix every failure with
/// their API name (OpenAI Responses `"OpenAI API error: "`, Azure
/// `"Azure OpenAI API error: "`) and append the ChatGPT usage hint, matching
/// upstream's user-facing text. The prefix is part of the message TEXT on
/// purpose; the structural retry class is independent of it.
pub fn prefixed_terminal_error_message(
    model: &Model,
    error: &AiError,
    prefix: &str,
    signal: Option<&AbortSignal>,
) -> AssistantMessage {
    let text = if matches!(error, AiError::Aborted) {
        error.user_message()
    } else if let Some(text) = upstream_http_error_text(model, prefix, error) {
        // Upstream appends the usage hint AFTER `formatProviderError`
        // (openai-responses.ts:253-259); keep that order for the rebuilt
        // upstream-shaped text too.
        crate::ai::util::append_chatgpt_usage_hint(&text)
    } else {
        format!(
            "{prefix}{}",
            crate::ai::util::append_chatgpt_usage_hint(&error.user_message())
        )
    };
    terminal_error_message_with_text(model, error, text, signal)
}

/// The upstream-shaped user-facing text for a transport HTTP failure
/// (upstream `formatProviderError`, error-body.ts:130-137):
/// `<prefix> (<status>): <body>` when the adapter has a prefix, otherwise the
/// raw `<status>: <body>` SDK shape.
///
/// Only the request-size overflow status (`413`) takes this shape. It is the
/// one the provider-independent overflow rule has to recognize through a
/// leading adapter prefix (overflow.ts:64-66, `<provider> API error (413):`);
/// every other status keeps the existing text byte-for-byte, so callers that
/// pin or parse it are unaffected. The provider name follows upstream's
/// Responses convention: the model's provider id, except `openai` itself
/// (`${model.provider === "openai" ? "OpenAI" : model.provider} API error`,
/// openai-responses.ts:253-255). Azure passes its own prefix and is left as-is.
fn upstream_http_error_text(model: &Model, prefix: &str, error: &AiError) -> Option<String> {
    let http = error
        .provider_http_error()
        .filter(|http| http.status == 413)?;
    let body = append_raw_error_metadata(http.body);
    let base = prefix.strip_suffix(": ").unwrap_or(prefix);
    if base.is_empty() {
        return Some(format!("{}: {body}", http.status));
    }
    let base = if base == "OpenAI API error" && model.provider != "openai" {
        format!("{} API error", model.provider)
    } else {
        base.to_string()
    };
    Some(format!("{base} ({}): {body}", http.status))
}

/// Shared construction for [`terminal_error_message`] /
/// [`prefixed_terminal_error_message`].
fn terminal_error_message_with_text(
    model: &Model,
    error: &AiError,
    text: String,
    signal: Option<&AbortSignal>,
) -> AssistantMessage {
    let mut message = crate::ai::provider::error_message(model, &text);
    let (_, stop_reason) = terminal_error_classification(error, signal);
    // The payload message carries the same classification as the event:
    // `error_message()` defaults to `StopReason::Error`, which is only right
    // for a genuine failure.
    message.stop_reason = stop_reason;
    // Structural retry verdict for the agent-level turn retry. The loop takes
    // (and removes) this internal diagnostic before the message is emitted,
    // stored, or persisted, so it never changes the transcript.
    crate::ai::error::attach_turn_retry_class(&mut message, error);
    message
}

/// Terminate an adapter stream after its request future resolved: push the
/// error as a terminal `Error` event (abort mapped to the abort reason) and
/// end the stream when no terminal event was emitted yet. Every provider
/// adapter calls this; only a build with no provider family at all sees it
/// unused.
///
/// The terminal message records the reason it reports: a user abort is
/// `StopReason::Aborted`, a transport/server failure `StopReason::Error`
/// (upstream `output.stopReason = options?.signal?.aborted ? "aborted" :
/// "error"`, `pi/packages/ai/src/api/openai-completions.ts:716`). The abort
/// SIGNAL decides — see [`terminal_error_classification`] — so a stream that
/// ends through an error surfaced while the connection tears down still
/// reports the caller's abort. What this helper cannot see is the adapter's
/// accumulated parser output; the registry forwarding loop fills an empty
/// terminal message with the accumulated stream content (see
/// `ai::provider::StreamedContent`), which is the same seam upstream's
/// in-place mutation of the accumulated `output` reaches.
#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-mistral",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
pub(crate) fn finish_provider_stream(
    sink: &AssistantMessageEventStream,
    result: Result<(), AiError>,
    model: &Model,
    signal: Option<&AbortSignal>,
) {
    if let Err(error) = result {
        let (reason, _) = terminal_error_classification(&error, signal);
        let message = terminal_error_message(model, &error, signal);
        sink.push(AssistantMessageEventStream::error_event(reason, message));
    }
    if !sink.is_done() {
        sink.end(None);
    }
}

/// Awaitable body-chunk sink for provider adapters: feeds the raw chunk
/// through the adapter's incremental parser and pushes the parsed events with
/// producer backpressure ([`AssistantMessageEventStream::push_await`]).
///
/// Replaces the former per-adapter `FnMut(&[u8])` closure that pushed with the
/// synchronous [`AssistantMessageEventStream::push`]: that shape could not
/// wait for queue space, so a consumer slower than the provider made the
/// unbounded queue (and every frozen `partial` snapshot in it) grow to the
/// whole reply. Here the transport read loop awaits `send`, so a full event
/// queue pauses the socket read.
///
/// A chunk larger than [`EventChunkSink::FEED_SLICE_BYTES`] is fed in slices
/// (the parsers accept arbitrary byte splits by design — the transport already
/// splits every frame arbitrarily, and the parser suite pins byte-by-byte
/// feeds). This caps how many events one transport read can materialize at
/// once: a single 64 KiB read burst of 10 000 per-delta frames would otherwise
/// freeze hundreds of full-message snapshots before the first `push_await`
/// even runs, i.e. the retention would scale with the socket read size
/// instead of the queue capacity.
#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-mistral",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
pub(crate) struct EventChunkSink<'a, F> {
    feed: F,
    sink: &'a AssistantMessageEventStream,
    ready: Vec<crate::ai::types::AssistantMessageEvent>,
}

#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-mistral",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
impl<'a, F> EventChunkSink<'a, F>
where
    F: FnMut(&[u8], &mut Vec<crate::ai::types::AssistantMessageEvent>) -> AiResult<()> + Send,
{
    /// Slice size for oversized body chunks (see the type docs).
    pub(crate) const FEED_SLICE_BYTES: usize = 4096;

    pub(crate) fn new(feed: F, sink: &'a AssistantMessageEventStream) -> Self {
        Self {
            feed,
            sink,
            ready: Vec::new(),
        }
    }

    /// Feed one slice through the parser, pushing every produced event with
    /// backpressure before the next slice is parsed.
    ///
    /// Events parsed BEFORE a failure are delivered even when `feed` returns
    /// `Err`: the parser appends events as it walks the chunk, and the
    /// terminal error path (the adapter's finalizer, `StreamedContent`)
    /// reconstructs the failed message from the events that were forwarded.
    /// Dropping the batch would silently lose the deltas parsed earlier in the
    /// same slice on a malformed trailing line.
    async fn feed_slice(&mut self, chunk: &[u8]) -> AiResult<()> {
        self.ready.clear();
        let result = (self.feed)(chunk, &mut self.ready);
        for event in self.ready.drain(..) {
            self.sink.push_await(event).await;
        }
        result
    }
}

#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-bedrock",
        feature = "provider-google",
        feature = "provider-mistral",
        feature = "provider-openai"
    )),
    allow(dead_code)
)]
impl<F> crate::ai::transport::ChunkSink for EventChunkSink<'_, F>
where
    F: FnMut(&[u8], &mut Vec<crate::ai::types::AssistantMessageEvent>) -> AiResult<()> + Send,
{
    fn send<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send + 'a>> {
        Box::pin(async move {
            if chunk.len() <= Self::FEED_SLICE_BYTES {
                return self.feed_slice(chunk).await;
            }
            for slice in chunk.chunks(Self::FEED_SLICE_BYTES) {
                self.feed_slice(slice).await?;
            }
            Ok(())
        })
    }
}

/// Append `error.metadata.raw` to a provider error message when present and
/// not already included (upstream openai-completions.ts:720-727): some
/// providers via OpenRouter put additional information there; pir's transport
/// embeds the raw HTTP error body in the message text, so the metadata is
/// extracted from that embedded JSON body. Public for tests/.
pub fn append_raw_error_metadata(message: &str) -> String {
    let Some(raw) = extract_raw_error_metadata(message) else {
        return message.to_string();
    };
    if message.contains(&raw) {
        return message.to_string();
    }
    format!("{message}\n{raw}")
}

/// Extract the string value of `error.metadata.raw` from a JSON body embedded
/// in an error message (best effort: the embedded body may be truncated, in
/// which case nothing is extracted).
fn extract_raw_error_metadata(message: &str) -> Option<String> {
    let start = message.find('{')?;
    // The body may be truncated by the transport's message cap; scan the
    // remaining text for the `"metadata"` / `"raw"` pair and take the JSON
    // string that follows.
    let tail = &message[start..];
    let metadata_pos = tail.find("\"metadata\"")?;
    let raw_pos = tail[metadata_pos..].find("\"raw\"")? + metadata_pos;
    let after = tail[raw_pos + "\"raw\"".len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    // A complete JSON string value must terminate inside the text.
    let quote = after.strip_prefix('"')?;
    let mut escaped = false;
    let mut out = String::new();
    for ch in quote.chars() {
        if escaped {
            escaped = false;
            out.push(ch);
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
            }
            '"' => {
                return Some(out).filter(|value| !value.is_empty());
            }
            _ => out.push(ch),
        }
    }
    None
}

/// Join the text blocks of a message into one string (adapter glue used by
/// the OpenAI Completions and Google adapters).
#[cfg_attr(
    not(any(feature = "provider-openai", feature = "provider-google")),
    allow(dead_code)
)]
pub(crate) fn joined_text_blocks(blocks: &[ContentBlock], separator: &str) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

// ---------------------------------------------------------------------------
// Cross-adapter resolvers
// ---------------------------------------------------------------------------
//
// These live in the core (not inside the `openai` adapter module) so the
// bedrock adapter and the OpenAI Responses codec keep them when
// `provider-openai` is compiled out. With the feature enabled the `openai`
// module re-exports them under the same names.

/// Prompt-cache retention preference (upstream `resolveCacheRetention`): the
/// explicit request value first, then the `PI_CACHE_RETENTION` provider env
/// (`long`), then `short`. Shared with the bedrock-converse-stream and
/// openai-responses adapters, whose upstreams define the same resolver.
#[cfg_attr(
    not(any(feature = "provider-bedrock", feature = "provider-openai")),
    allow(dead_code)
)]
pub(crate) fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    if let Some(retention) = options.cache_retention {
        return retention;
    }
    if crate::ai::api::provider_env_value("PI_CACHE_RETENTION", options.env.as_ref()).as_deref()
        == Some("long")
    {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// Default thinking token budgets per level (upstream `DEFAULT_THINKING_BUDGETS`);
/// `xhigh`/`max` clamp to `high`, `off` has no budget. Shared with the bedrock
/// adapter, whose thinking field consumes the same ladder.
pub fn default_thinking_budget(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => 16384,
    }
}

/// The thinking token budget for a level, honoring per-request overrides
/// (upstream `thinkingBudgetForLevel`: `thinkingBudgets[level]` first, then
/// `DEFAULT_THINKING_BUDGETS`; `xhigh`/`max` use the `high` budget).
pub fn thinking_budget_for_level(level: ThinkingLevel, budgets: Option<&ThinkingBudgets>) -> u64 {
    let overridden = budgets.and_then(|budgets| match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => budgets.high,
    });
    overridden.unwrap_or_else(|| default_thinking_budget(level))
}
