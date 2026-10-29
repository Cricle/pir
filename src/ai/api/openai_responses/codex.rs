//! OpenAI Codex (ChatGPT account) Responses deltas.
//!
//! Rust equivalent of the upstream `openai-codex-responses.ts` deltas over the
//! shared Responses codec: the ChatGPT backend endpoint
//! (`{base}/codex/responses`), JWT `chatgpt_account_id` extraction, the
//! `originator` / `chatgpt-account-id` / `OpenAI-Beta: responses=experimental`
//! headers, and the codex request body (top-level `instructions` instead of a
//! leading system input item, `text.verbosity`, `include` of
//! `reasoning.encrypted_content`, `parallel_tool_calls`).
//!
//! Transport (upstream `websocket-cached`): turns first ride the pooled
//! WebSocket transport in [`codex_ws`], which reuses one connection per
//! session/account and, after a successful turn, sends only the new input
//! items plus `previous_response_id` instead of the full history. Any
//! transport failure before the first provider event falls back to the SSE
//! path below (with a fresh parser, full history); failures after events
//! began are surfaced, matching upstream's transport/non-transport split. A
//! session that fell back keeps using SSE on later turns until
//! [`codex_ws::reset_codex_websocket_state`] (upstream
//! `websocketSseFallbackSessions`).
//! The SSE body is zstd-compressed when the codec is compiled in (upstream
//! always compresses this endpoint); see
//! [`codex_ws::zstd_request_compression_available`].
//!
//! Continuation key: [`crate::ai::types::StreamOptions::session_id`] (the
//! upstream `cacheSessionId`), populated by the agent session from its stable
//! session identity (`AgentSession::stream_options_with_actor_hooks`). When it
//! is absent (a direct adapter call with defaulted options) no continuation is
//! stored and every turn sends the full history. The resolved `transport`
//! setting rides the same request options (see
//! [`codex_ws::METADATA_TRANSPORT`]): `sse` skips the WebSocket attempt
//! entirely, `auto` / `websocket-cached` also reuse the continuation cache, and
//! plain `websocket` pools the connection but always sends the full history.

#[path = "codex_ws.rs"]
pub mod codex_ws;

use serde_json::{json, Value};

use super::shared::{off_map_entry_is_not_null, OPENAI_TOOL_CALL_PROVIDERS};
use crate::ai::api::transform::get_initial_system_prompt;
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::{
    AssistantMessageEvent, Model, ProviderHeaders, StreamOptions, ThinkingLevel,
};

/// Default Codex base URL (upstream `DEFAULT_CODEX_BASE_URL`).
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// JWT claim path carrying the ChatGPT account id (upstream `JWT_CLAIM_PATH`).
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// Resolve the Codex `/codex/responses` endpoint from a base URL (upstream
/// `resolveCodexUrl`). Public for tests/.
pub fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Extract the ChatGPT account id from the JWT access token (upstream
/// `extractAccountId`). Public for tests/.
pub fn extract_account_id(token: &str) -> Result<String, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("Failed to extract accountId from token".to_string());
    }
    let decoded = base64_url_decode(parts[1])
        .ok_or_else(|| "Failed to extract accountId from token".to_string())?;
    let payload: Value = serde_json::from_slice(&decoded)
        .map_err(|_| "Failed to extract accountId from token".to_string())?;
    payload
        .get(JWT_CLAIM_PATH)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "Failed to extract accountId from token".to_string())
}

/// Decode base64url (JWT segment) with padding tolerance.
fn base64_url_decode(segment: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    engine.decode(segment).ok().or_else(|| {
        let mut padded = segment.trim_end_matches('=').to_string();
        let remainder = padded.chars().count() % 4;
        if remainder > 0 {
            padded.push_str(&"=".repeat(4 - remainder));
        }
        base64::engine::general_purpose::URL_SAFE
            .decode(padded)
            .ok()
    })
}

/// Build the Codex SSE request headers (upstream `buildSSEHeaders`).
/// Returns `(header name, value)` pairs; defaults first so caller headers can
/// override them. Public for tests/.
pub fn build_codex_headers(
    model_headers: Option<&std::collections::BTreeMap<String, String>>,
    options_headers: Option<&std::collections::BTreeMap<String, String>>,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> std::collections::BTreeMap<String, String> {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("originator".to_string(), "pi".to_string());
    headers.insert(
        "user-agent".to_string(),
        format!("pir ({})", std::env::consts::OS),
    );
    if let Some(model_headers) = model_headers {
        for (name, value) in model_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    headers.insert("authorization".to_string(), format!("Bearer {token}"));
    headers.insert("chatgpt-account-id".to_string(), account_id.to_string());
    headers.insert(
        "openai-beta".to_string(),
        "responses=experimental".to_string(),
    );
    headers.insert("accept".to_string(), "text/event-stream".to_string());
    headers.insert("content-type".to_string(), "application/json".to_string());
    if let Some(session_id) = session_id {
        headers.insert("session-id".to_string(), session_id.to_string());
        headers.insert("x-client-request-id".to_string(), session_id.to_string());
    }
    headers
}

/// Build the Codex `/codex/responses` request body (upstream
/// `buildRequestBody`). Public for tests/.
pub fn build_codex_body(
    model: &Model,
    context: &crate::ai::types::Context,
    options: &StreamOptions,
) -> Result<Value, crate::ai::error::AiError> {
    use crate::ai::api::openai_responses::responses_compat::ResponsesCompat;
    use crate::ai::api::openai_responses::shared::{
        convert_responses_messages, ConvertResponsesMessagesOptions,
    };
    let compat = ResponsesCompat::from_model(model);
    // Codex defaults strict tool schemas on (upstream
    // `supportsStrictMode ?? true`).
    let supports_strict_mode = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_strict_mode)
        .unwrap_or(true);
    let message_options = ConvertResponsesMessagesOptions {
        include_system_prompt: false,
        supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
        supports_additional_tools: compat.supports_additional_tools,
        supports_tool_search: compat.supports_tool_search,
        supports_strict_mode,
        supports_developer_role: Some(compat.supports_developer_role),
        // Codex manages the thinking level through `reasoning.effort` on the
        // request body below (upstream openai-codex-responses.ts:514-515), not
        // through cache-preserving `configuration_update` items, so there is
        // no mid-conversation effort to send here.
        configuration_update_effort: None,
    };
    let messages =
        convert_responses_messages(model, context, OPENAI_TOOL_CALL_PROVIDERS, &message_options);

    let instructions = get_initial_system_prompt(context)
        .unwrap_or_else(|| "You are a helpful assistant.".to_string());
    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": messages,
        // Upstream `textVerbosity` (default "low").
        "text": {
            "verbosity": options
                .text_verbosity
                .map(|verbosity| verbosity.as_str())
                .unwrap_or("low")
        },
        "include": ["reasoning.encrypted_content"],
        // Upstream `options?.toolChoice ?? "auto"` (openai-codex-responses.ts:562).
        "tool_choice": options
            .tool_choice
            .as_ref()
            .map(crate::ai::types::ToolChoice::openai_responses_json)
            .unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": true,
    });
    // Cache retention: "none" drops the session cache key.
    let cache_retention = options
        .cache_retention
        .unwrap_or(crate::ai::types::CacheRetention::Short);
    if cache_retention != crate::ai::types::CacheRetention::None {
        if let Some(session_id) = &options.session_id {
            body["prompt_cache_key"] = json!(super::shared::clamp_openai_prompt_cache_key(Some(
                session_id
            )));
        }
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }
    // Requested service tier (upstream buildRequestBody:570-572).
    if let Some(service_tier) = &options.service_tier {
        body["service_tier"] = json!(service_tier);
    }
    // In-place tool changes (upstream `resolveTranscriptTools`): Codex models
    // with `supportsAdditionalTools` declare every tool in `additional_tools`
    // items, so the top-level list stays empty; tool-search models freeze it at
    // the last removal/redeclaration and anchor later additions.
    let request_tools = super::shared::resolve_responses_request_tools(
        context,
        compat.supports_mid_convo_system_messages,
        compat.supports_additional_tools,
        compat.supports_tool_search,
    );
    if !request_tools.is_empty() {
        body["tools"] = Value::Array(super::shared::convert_responses_tools(
            &request_tools,
            supports_strict_mode,
        )?);
    }

    // Reasoning emission (upstream `streamSimple` reasoningEffort at
    // openai-codex-responses.ts:514-515 + the buildRequestBody reasoning
    // branch at :582-596): "off" (and an omitted level) bypasses the
    // per-level mapping — the map's own `off` entry wins, "none" when the
    // entry is absent, an explicit `null` entry suppresses the reasoning
    // field entirely, and neither form carries a summary. A real level maps
    // through `thinkingLevelMap` and emits the `summary` marker.
    let requested = options
        .reasoning
        .filter(|level| !matches!(level, ThinkingLevel::Off));
    if let Some(level) = requested {
        body["reasoning"] = json!({
            "effort": map_codex_effort(model, level),
            "summary": options
                .reasoning_summary
                .map(|summary| summary.as_str())
                .unwrap_or("auto"),
        });
    } else if model.reasoning && off_map_entry_is_not_null(model) {
        let effort = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("off"))
            .cloned()
            .flatten()
            .unwrap_or_else(|| "none".to_string());
        body["reasoning"] = json!({"effort": effort});
    }
    Ok(body)
}

/// Map a requested pi thinking level to a Codex effort (upstream
/// `buildRequestBody`: the thinkingLevelMap entry wins, otherwise the raw
/// level). Callers only pass non-`off` levels.
fn map_codex_effort(model: &Model, level: ThinkingLevel) -> String {
    let key = level.wire_key();
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(key))
        .cloned()
        .flatten()
        .unwrap_or_else(|| key.to_string())
}

// ---------------------------------------------------------------------------
// Transport (WebSocket first, SSE fallback)
// ---------------------------------------------------------------------------

/// Run one Codex request: pooled WebSocket first, SSE when the WebSocket
/// fails before emitting any provider event. `body` is the full-history
/// request body; the WebSocket path may reduce it to a continuation
/// (`previous_response_id` + input delta) internally, while the SSE path
/// always sends it whole (upstream's SSE fallback behavior).
#[allow(clippy::too_many_arguments)]
pub async fn stream_codex_responses(
    client: &ProviderRequestClient,
    model: &Model,
    options: &StreamOptions,
    url: &str,
    sse_headers: &ProviderHeaders,
    body: Value,
    account_id: &str,
    token: &str,
    sink: &AssistantMessageEventStream,
) -> Result<(), crate::ai::error::AiError> {
    // `before_provider_request` is a transport-level hook in pir. The
    // WebSocket path bypasses the HTTP transport (and the SSE path may send a
    // zstd-compressed body that the hook cannot parse), so apply it once here
    // on the JSON payload; `stream_codex_sse` clears it from the cloned hooks
    // to avoid a second application.
    let body = match &options.hooks.before_provider_request {
        Some(hook) => hook(body).await,
        None => body,
    };
    let mut parser = new_codex_parser(model, options);
    // `transport = "sse"` is the explicit opt-out: no WebSocket handshake, no
    // fallback-marker bookkeeping, straight to the SSE path (upstream
    // `transport !== "sse"` gates the whole WebSocket branch).
    let transport = codex_ws::transport_options(options);
    if transport.transport == crate::config::settings::TransportSetting::Sse {
        return stream_codex_sse(client, url, sse_headers, body, options, &mut parser, sink).await;
    }
    let ws_url = codex_ws::resolve_codex_websocket_url(url);
    match codex_ws::try_stream_codex_websocket(
        &ws_url,
        &body,
        account_id,
        token,
        model,
        options,
        &mut parser,
        sink,
    )
    .await?
    {
        codex_ws::CodexWsOutcome::Completed => Ok(()),
        codex_ws::CodexWsOutcome::Fallback => {
            // A fresh parser: the WS attempt must not have emitted anything
            // for a fallback to be allowed (it errors otherwise), but its
            // internal state is still unusable after a failed transport.
            let mut parser = new_codex_parser(model, options);
            stream_codex_sse(client, url, sse_headers, body, options, &mut parser, sink).await
        }
    }
}

fn new_codex_parser(model: &Model, options: &StreamOptions) -> super::shared::ResponsesSseParser {
    let mut parser = super::shared::ResponsesSseParser::new(model)
        .with_provider_event_hook(options.hooks.on_provider_stream_event.clone());
    parser.codex_mode = true;
    parser
}

/// The SSE path (identical to the shared `stream_responses` helper) plus the
/// upstream Codex request-body compression: the body is zstd-compressed
/// whenever the codec is compiled in, matching upstream's unconditional
/// `compressRequestBodyZstd` on this endpoint. Without the codec the body goes
/// out uncompressed.
#[allow(clippy::too_many_arguments)]
async fn stream_codex_sse(
    client: &ProviderRequestClient,
    url: &str,
    headers: &ProviderHeaders,
    body: Value,
    options: &StreamOptions,
    parser: &mut super::shared::ResponsesSseParser,
    sink: &AssistantMessageEventStream,
) -> Result<(), crate::ai::error::AiError> {
    // `false`: the consumer is gone — skip the request whose events nobody can
    // read (the transport is never opened).
    if !sink
        .push_await(AssistantMessageEvent::Start {
            partial: parser.snapshot(),
        })
        .await
    {
        return Ok(());
    }
    let payload = serde_json::to_vec(&body)?;
    let compressed = codex_ws::compress_request_body_zstd(&payload);
    let (payload, compressed) = match compressed {
        Some(compressed) => (compressed, true),
        None => (payload, false),
    };
    let mut headers = headers.clone();
    if compressed {
        headers.insert("content-encoding".to_string(), "zstd".to_string());
    }
    // Already applied in `stream_codex_responses` (on the JSON payload, before
    // any WS attempt or compression).
    let mut request_hooks = options.hooks.clone();
    request_hooks.before_provider_request = None;
    let (status, response_headers) = {
        let mut on_chunk = crate::ai::api::EventChunkSink::new(
            |chunk: &[u8], ready: &mut Vec<AssistantMessageEvent>| {
                // Every consumer handle is gone: release the HTTP response
                // instead of draining a body nobody reads (normal abandon).
                if sink.is_abandoned() {
                    return Err(crate::ai::error::AiError::Aborted);
                }
                parser.feed(chunk, ready)
            },
            sink,
        );
        client
            .request_streaming_with(
                reqwest::Method::POST,
                url,
                &headers,
                payload,
                options.signal.as_ref(),
                &request_hooks,
                &mut on_chunk,
            )
            .await?
    };
    // Upstream fires `onResponse` when the HTTP response arrives, before the
    // body is consumed; pir's transport only surfaces status/headers once
    // streaming completes.
    if let Some(on_response) = &request_hooks.on_response {
        on_response(status, &response_headers);
    }
    let mut tail = Vec::new();
    parser.finish(&mut tail)?;
    for event in tail {
        if !sink.push_await(event).await {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Error handling (upstream openai-codex-responses.ts parseErrorResponse)
// ---------------------------------------------------------------------------

/// A parsed Codex HTTP error response (upstream `parseErrorResponse` return
/// shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexErrorResponse {
    /// The message carried into the assistant error text (upstream
    /// `message`): the raw body, overridden by `error.message`, then by the
    /// friendly usage-limit text.
    pub message: String,
    /// The friendly usage-limit message when the response is a ChatGPT
    /// usage-limit error (upstream `friendlyMessage`).
    pub friendly_message: Option<String>,
}

/// Parse a Codex HTTP error body into a message plus optional friendly
/// ChatGPT usage-limit message (upstream `parseErrorResponse`): a JSON
/// `error` object whose `code`/`type` matches a usage-limit code — or any
/// HTTP 429 — becomes
/// `You have hit your ChatGPT usage limit (pro plan). Try again in ~N min.`
/// `now_ms` is the current Unix time in milliseconds (upstream uses
/// `Date.now()`), `resets_at` is Unix seconds. Message-text enhancement
/// only; retry classification is unchanged. Deviation: upstream falls back
/// to `response.statusText`, which the shared transport does not carry — an
/// empty body degrades to `HTTP <status>`. Public for tests/.
pub fn parse_error_response(raw: &str, status: u16, now_ms: u64) -> CodexErrorResponse {
    let mut message = if raw.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        raw.to_string()
    };
    let mut friendly_message: Option<String> = None;
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if let Some(err) = parsed.get("error") {
            let code = err
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| err.get("type").and_then(Value::as_str))
                .unwrap_or_default();
            let code_lower = code.to_ascii_lowercase();
            let usage_limit = status == 429
                || [
                    "usage_limit_reached",
                    "usage_not_included",
                    "rate_limit_exceeded",
                ]
                .iter()
                .any(|fragment| code_lower.contains(fragment));
            if usage_limit {
                let plan = err
                    .get("plan_type")
                    .and_then(Value::as_str)
                    .map(|plan| format!(" ({} plan)", plan.to_lowercase()))
                    .unwrap_or_default();
                let when = err
                    .get("resets_at")
                    .and_then(Value::as_f64)
                    .map(|resets_at| {
                        let mins = ((resets_at * 1000.0 - now_ms as f64) / 60_000.0)
                            .round()
                            .max(0.0) as u64;
                        format!(" Try again in ~{mins} min.")
                    })
                    .unwrap_or_default();
                friendly_message = Some(
                    format!("You have hit your ChatGPT usage limit{plan}.{when}")
                        .trim()
                        .to_string(),
                );
            }
            if let Some(err_message) = err.get("message").and_then(Value::as_str) {
                message = err_message.to_string();
            } else if let Some(friendly) = &friendly_message {
                message = friendly.clone();
            }
        }
    }
    CodexErrorResponse {
        message,
        friendly_message,
    }
}

/// Enhance a codex transport error with the parsed ChatGPT error text
/// (upstream `parseErrorResponse` + `throw new Error(info.friendlyMessage ||
/// info.message)`, openai-codex-responses.ts:435-441): the shared transport
/// reports non-2xx responses as `NonRetryable("HTTP {status}: {body}")`, so
/// the status and body are parsed back out and the body is replaced by the
/// friendly usage-limit text (or the structured `error.message`) when
/// present. `now_ms` is the current Unix time in milliseconds. Message-text
/// enhancement only; retry classification is unchanged. Public for tests/.
pub fn enhance_codex_error(
    error: crate::ai::error::AiError,
    now_ms: u64,
) -> crate::ai::error::AiError {
    use crate::ai::error::AiError;
    let AiError::NonRetryable(message) = &error else {
        return error;
    };
    let Some(rest) = message.strip_prefix("HTTP ") else {
        return error;
    };
    let Some((status_text, body)) = rest.split_once(": ") else {
        return error;
    };
    if body.trim().is_empty() {
        return error;
    }
    let Ok(status) = status_text.parse::<u16>() else {
        return error;
    };
    let parsed = parse_error_response(body, status, now_ms);
    let detail = parsed.friendly_message.unwrap_or(parsed.message);
    AiError::NonRetryable(format!("HTTP {status}: {detail}"))
}
