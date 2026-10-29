//! Pooled WebSocket transport for the ChatGPT Codex Responses endpoint.
//!
//! Rust port of upstream `packages/ai/src/api/openai-codex-responses.ts`
//! (1.3.0, `websocket-cached`): one pooled connection per
//! `(session, account, url)`, a connection-scoped continuation cache that
//! turns the next turn into `previous_response_id` + only the new input
//! items, one reconnect attempt on close, the SSE fallback the caller runs
//! when the WebSocket fails before it emitted any event, and a per-session
//! SSE-fallback marker so a session that already failed never retries the
//! WebSocket transport (upstream `websocketSseFallbackSessions`).
//!
//! Wire shape:
//! - URL: the resolved `/codex/responses` URL with `https`→`wss` /
//!   `http`→`ws`.
//! - Handshake headers: the SSE headers minus `accept`/`content-type`,
//!   with `OpenAI-Beta: responses_websockets=2026-02-06` and the request
//!   id in `session-id`/`x-client-request-id` (upstream
//!   `buildWebSocketHeaders`).
//! - Frames: one JSON text frame `{"type":"response.create", ...body}`.
//!   Upstream sends the body uncompressed over WebSocket (zstd request
//!   compression is an SSE-endpoint feature); this module mirrors that.
//! - Responses arrive as JSON text frames in the Responses event shape and
//!   are fed through [`ResponsesSseParser`] unchanged (codex mode), so the
//!   emitted [`AssistantMessageEvent`] stream is identical to the SSE
//!   path for the same scripted events.
//!
//! Continuation: after a successful turn the full request body, the
//! response id and the assistant output items are cached on the pooled
//! connection. The next request sends the cached `previous_response_id` and
//! only the input suffix beyond `last_request_body.input + last_response_items`
//! when everything else in the body matches; otherwise (unknown session, a
//! changed system prompt/tools/model, a new connection, or a mismatched
//! history) the full history is sent and the cache is dropped. This is the
//! upstream `getCachedWebSocketInputDelta`/`buildCachedWebSocketRequestBody`
//! logic.
//!
//! Transport selection (upstream `StreamOptions.transport`, settings
//! `transport`): `sse` skips this module entirely, `auto` and
//! `websocket-cached` reuse the pooled connection's `previous_response_id`
//! continuation, and plain `websocket` keeps the pooled connection but always
//! sends the full history (upstream `useCachedContext = transport ===
//! "websocket-cached" || transport === "auto"`).
//!
//! The connect handshake uses the session's resolved
//! `websocketConnectTimeoutMs` when present, else
//! [`DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS`]; an explicit `0` disables the
//! handshake timeout, exactly like upstream `normalizeTimeoutMs` plus the
//! `connectTimeoutMs > 0` guard.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::types::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, CacheRetention, Context,
    Message as AiMessage, Model, StreamOptions,
};
use crate::config::settings::TransportSetting;

use crate::ai::api::openai_responses::shared::{
    clamp_openai_prompt_cache_key, convert_responses_messages, ConvertResponsesMessagesOptions,
    ResponsesSseParser, OPENAI_TOOL_CALL_PROVIDERS,
};

/// WebSocket `OpenAI-Beta` value (upstream
/// `OPENAI_BETA_RESPONSES_WEBSOCKETS`).
pub const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";

/// Upstream `DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS`.
pub const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;

/// [`StreamOptions::metadata`] key carrying the resolved `transport` setting
/// (upstream `StreamOptions.transport`). pir has no typed transport field on
/// the shared request options, so the session hands the resolved value to the
/// adapter through the metadata bag every other adapter ignores; the key is
/// the upstream option name so the mapping stays obvious. Values are the
/// [`TransportSetting`] wire strings (`auto`, `sse`, `websocket`,
/// `websocket-cached`); an absent key means `auto`.
pub const METADATA_TRANSPORT: &str = "transport";

/// [`StreamOptions::metadata`] key carrying the resolved
/// `websocketConnectTimeoutMs` setting (upstream
/// `StreamOptions.websocketConnectTimeoutMs`): the connect/open-handshake
/// timeout only. `0` disables it; an absent key keeps
/// [`DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS`].
pub const METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS: &str = "websocketConnectTimeoutMs";

/// The transport knobs of one Codex request, resolved from
/// [`StreamOptions::metadata`] (see [`METADATA_TRANSPORT`] /
/// [`METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS`]). Public for tests/.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexTransportOptions {
    /// The settings `transport` value (`auto` when unset).
    pub transport: TransportSetting,
    /// The connect-handshake timeout; `None` disables it (an explicit `0`).
    pub connect_timeout: Option<Duration>,
}

impl Default for CodexTransportOptions {
    fn default() -> Self {
        Self {
            transport: TransportSetting::Auto,
            connect_timeout: Some(Duration::from_millis(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS)),
        }
    }
}

impl CodexTransportOptions {
    /// Whether this request may use the connection-scoped continuation cache
    /// (upstream `useCachedContext`): `auto` and `websocket-cached` do, plain
    /// `websocket` never does.
    pub fn uses_continuation_cache(self) -> bool {
        matches!(
            self.transport,
            TransportSetting::Auto | TransportSetting::WebsocketCached
        )
    }
}

/// Resolve [`CodexTransportOptions`] from a request's options. Public for
/// tests/.
pub fn transport_options(options: &StreamOptions) -> CodexTransportOptions {
    let metadata = options.metadata.as_ref();
    let transport = metadata
        .and_then(|metadata| metadata.get(METADATA_TRANSPORT))
        .and_then(|value| serde_json::from_value::<TransportSetting>(value.clone()).ok())
        .unwrap_or(TransportSetting::Auto);
    let connect_timeout = match metadata
        .and_then(|metadata| metadata.get(METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS))
        .and_then(Value::as_u64)
    {
        // Upstream arms the handshake timer only for `connectTimeoutMs > 0`
        // (`normalizeTimeoutMs` maps "disabled" to 0).
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_millis(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS)),
    };
    CodexTransportOptions {
        transport,
        connect_timeout,
    }
}

/// Upstream `SESSION_WEBSOCKET_CACHE_TTL_MS` (idle eviction).
const SESSION_WEBSOCKET_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// Upstream `SESSION_WEBSOCKET_MAX_AGE_MS` (per-connection age cap).
const SESSION_WEBSOCKET_MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// Upstream `REQUEST_COMPRESSION_ZSTD_LEVEL`. Gated with `extensions`: the
/// `zstd` codec rides that feature, so the no-`extensions` stub below never
/// reads it.
#[cfg(feature = "extensions")]
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;

const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
/// Close code the Codex backend uses for an oversized frame (upstream
/// `WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE`).
const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// What [`try_stream_codex_websocket`] did.
pub enum CodexWsOutcome {
    /// The response stream completed (all events are already on the sink).
    Completed,
    /// The WebSocket failed before it emitted any event; the caller should
    /// run the SSE path with a fresh parser.
    Fallback,
}

/// Resolve the Codex WebSocket URL from a base URL or an already-resolved
/// `/codex/responses` URL (upstream `resolveCodexWebSocketUrl`).
pub fn resolve_codex_websocket_url(base_url: &str) -> String {
    let http = super::resolve_codex_url(base_url);
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http
    }
}

/// Build the Codex WebSocket handshake headers (upstream
/// `buildWebSocketHeaders`). Returns `(header name, value)` pairs.
pub fn build_codex_websocket_headers(
    model_headers: Option<&BTreeMap<String, String>>,
    options_headers: Option<&BTreeMap<String, String>>,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> BTreeMap<String, String> {
    let mut headers =
        super::build_codex_headers(model_headers, options_headers, account_id, token, None);
    // Upstream `buildWebSocketHeaders`: the SSE-specific headers are dropped
    // and the WebSocket beta value + request id are set after the caller's
    // headers, so they cannot be overridden.
    headers.remove("accept");
    headers.remove("content-type");
    headers.remove("openai-beta");
    headers.insert(
        "openai-beta".to_string(),
        OPENAI_BETA_RESPONSES_WEBSOCKETS.to_string(),
    );
    headers.insert("x-client-request-id".to_string(), request_id.to_string());
    headers.insert("session-id".to_string(), request_id.to_string());
    headers
}

/// The continuation/pool key for a request: the cached session id, unless
/// prompt caching is disabled (upstream `cacheSessionId`). Clamped exactly
/// like the `prompt_cache_key` body field.
pub fn continuation_key(options: &StreamOptions) -> Option<String> {
    if options.cache_retention == Some(CacheRetention::None) {
        return None;
    }
    clamp_openai_prompt_cache_key(options.session_id.as_deref()).filter(|key| !key.is_empty())
}

/// The pool/fallback key for a raw session id: the same clamp
/// [`continuation_key`] applies, so teardown never misses a session just
/// because its id is longer than the `prompt_cache_key` limit. An empty id
/// maps to `None` (nothing was ever pooled under it).
pub fn session_continuation_key(session_id: &str) -> Option<String> {
    clamp_openai_prompt_cache_key(Some(session_id)).filter(|key| !key.is_empty())
}

/// Close pooled WebSocket connections (`None` closes every session); mirrors
/// upstream `closeOpenAICodexWebSocketSessions`, which does not by itself
/// clear the session's SSE-fallback marker. pir runs this at session teardown
/// (upstream registers it as the session resource cleanup), so the marker is
/// cleared with the sockets: a torn-down session must not poison a later one
/// that reuses the id. [`reset_codex_websocket_state`] clears the markers
/// without touching live sockets, matching upstream
/// `resetOpenAICodexWebSocketDebugStats`.
///
/// The argument is the RAW session id: the pool and the SSE-fallback marker
/// are keyed by the clamped [`continuation_key`], so it is clamped here (a
/// caller passing an already-clamped key is unaffected).
pub async fn close_codex_websocket_sessions(session_id: Option<&str>) {
    close_codex_websocket_sessions_sync(session_id);
}

/// Synchronous [`close_codex_websocket_sessions`], for teardown paths that
/// cannot await (`AgentSession`'s `Drop`). Both forms share the same
/// poison-recovering pool locks, which are never held across an `await`.
pub fn close_codex_websocket_sessions_sync(session_id: Option<&str>) {
    let key = session_id.and_then(session_continuation_key);
    match session_id {
        Some(_) => {
            // An id that clamps to nothing (empty) has no pool entry.
            let Some(key) = key else {
                return;
            };
            // Retiring the key and removing its connections happen under the
            // same pool lock: a lease in flight for this key is marked retired
            // in the same critical section, so its release closes the socket
            // instead of re-pooling it (see [`Lease::release`]).
            lock_pool().retire_session(&key);
            lock_fallback_sessions().remove(&key);
        }
        None => {
            lock_pool().retire_all();
            lock_fallback_sessions().clear();
        }
    }
}

/// Close `session_id`'s pooled connections from a teardown path that cannot
/// wait for them (the session's `Drop`): the close runs on the runtime that is
/// driving the process. Without a runtime the process is already on its way
/// out and the sockets close with it, so the call is a documented no-op.
pub fn spawn_close_codex_websocket_sessions(session_id: Option<String>) {
    let Some(session_id) = session_id else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!(
            target: "pir::ai::codex_ws",
            "no tokio runtime at session teardown; pooled Codex WebSocket connections close with the process"
        );
        return;
    };
    handle.spawn(async move {
        close_codex_websocket_sessions(Some(&session_id)).await;
    });
}

/// Clear the SSE-fallback markers (upstream
/// `resetOpenAICodexWebSocketDebugStats`): the next request for `session_id`
/// (or every session when `None`) tries the WebSocket transport again. Public
/// for hosts and tests. The id is clamped like the pool key.
pub async fn reset_codex_websocket_state(session_id: Option<&str>) {
    match session_id.and_then(session_continuation_key) {
        Some(key) => {
            lock_fallback_sessions().remove(&key);
        }
        None if session_id.is_none() => {
            lock_fallback_sessions().clear();
        }
        None => {}
    }
}

/// Sessions that fell back to SSE after a transport failure (upstream
/// `websocketSseFallbackSessions`): later turns for the same cache session skip
/// the WebSocket attempt entirely instead of paying a failed handshake each
/// time.
fn sse_fallback_sessions() -> &'static Mutex<HashSet<String>> {
    static SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn lock_fallback_sessions() -> MutexGuard<'static, HashSet<String>> {
    sse_fallback_sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    let Some(session_id) = session_id else {
        return false;
    };
    lock_fallback_sessions().contains(session_id)
}

fn record_websocket_failure(session_id: Option<&str>) {
    if let Some(session_id) = session_id {
        lock_fallback_sessions().insert(session_id.to_string());
    }
}

/// Whether this build can zstd-compress the SSE fallback request body.
pub fn zstd_request_compression_available() -> bool {
    cfg!(feature = "extensions")
}

/// zstd-compress a request body at the upstream level (3), when the codec is
/// compiled in. The Codex backend accepts `content-encoding: zstd` on the
/// SSE responses endpoint, and upstream compresses unconditionally there
/// (`compressRequestBodyZstd`, openai-codex-responses.ts:380-384) — there is
/// no server-negotiated opt-out on this path.
pub fn compress_request_body_zstd(payload: &[u8]) -> Option<Vec<u8>> {
    compress_zstd_impl(payload)
}

#[cfg(feature = "extensions")]
fn compress_zstd_impl(payload: &[u8]) -> Option<Vec<u8>> {
    zstd::stream::encode_all(payload, REQUEST_COMPRESSION_ZSTD_LEVEL).ok()
}

#[cfg(not(feature = "extensions"))]
fn compress_zstd_impl(_payload: &[u8]) -> Option<Vec<u8>> {
    // `zstd` currently rides the `extensions` feature. A provider-openai-only
    // build needs `provider-openai = [..., "dep:zstd"]` in Cargo.toml before
    // this can be enabled there; until then the body goes out uncompressed,
    // matching upstream's fallback when compression is unavailable.
    None
}

/// Drive one Codex turn over the pooled WebSocket transport.
///
/// Emits `Start` and all parser events on `sink` on success; on a transport
/// failure before any event was emitted it returns `Fallback` with the sink
/// untouched (the caller must use a fresh parser for SSE: this attempt may
/// have mutated `parser` only if events were emitted, and then it errors
/// instead). Non-transport failures (Codex `error`/`response.failed` events,
/// invalid JSON frames) are returned as errors and never fall back, matching
/// upstream's `isCodexNonTransportError` split.
#[allow(clippy::too_many_arguments)]
pub async fn try_stream_codex_websocket(
    ws_url: &str,
    full_body: &Value,
    account_id: &str,
    token: &str,
    model: &Model,
    options: &StreamOptions,
    parser: &mut ResponsesSseParser,
    sink: &AssistantMessageEventStream,
) -> AiResult<CodexWsOutcome> {
    let key = continuation_key(options);
    let pool_key = key.as_ref().map(|key| PoolKey {
        session_id: key.clone(),
        account_id: account_id.to_string(),
        url: ws_url.to_string(),
    });
    let request_id = key
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let headers = build_codex_websocket_headers(
        model.headers.as_ref(),
        options.headers.as_ref(),
        account_id,
        token,
        &request_id,
    );
    let idle_timeout = options
        .timeout_ms
        .filter(|timeout| *timeout > 0)
        .map(Duration::from_millis);
    let transport = transport_options(options);
    // Upstream `useCachedContext`: `websocket-cached` and `auto` reuse the
    // connection's continuation; plain `websocket` always sends the full
    // history over the pooled connection.
    let use_cached_context = transport.uses_continuation_cache();

    let mut retried_missing_continuation = false;
    let mut retried_connection_limit = false;
    let mut reconnected = false;
    // Upstream `startEmitted`: the `Start` event is request-scoped, so a
    // retried attempt never emits a second one.
    let mut start_emitted = false;

    // Upstream `websocketDisabledForSession`: a session that already fell back
    // to SSE never retries the WebSocket transport (no failed handshake per
    // turn). `transport = "sse"` is the caller's explicit opt-out: no
    // handshake and no fallback-marker bookkeeping (upstream's
    // `transport !== "sse"` gate covers both).
    if transport.transport == TransportSetting::Sse {
        return Ok(CodexWsOutcome::Fallback);
    }
    if websocket_sse_fallback_active(key.as_deref()) {
        return Ok(CodexWsOutcome::Fallback);
    }

    'attempt: loop {
        let mut lease = match acquire(
            pool_key.as_ref(),
            ws_url,
            &headers,
            transport.connect_timeout,
            options.signal.as_ref(),
        )
        .await
        {
            Ok(lease) => lease,
            Err(AcquireError::Aborted) => return Err(AiError::Aborted),
            Err(AcquireError::Transport(reason)) => {
                // No handshake, no request bytes: safe to fall back to SSE.
                tracing::debug!(
                    target: "pir::ai::codex_ws",
                    "Codex WebSocket connect failed, falling back to SSE: {reason}"
                );
                // Upstream `recordWebSocketFailure`: later turns of this
                // session skip the WebSocket transport entirely.
                record_websocket_failure(key.as_deref());
                return Ok(CodexWsOutcome::Fallback);
            }
        };
        // COW: without a reusable continuation the full history is sent from
        // the caller's body by reference; cloning a transcript-sized request
        // `Value` here was paid on every fresh WebSocket turn just to
        // serialize it.
        let request_body = match lease
            .continuation
            .as_ref()
            .filter(|_| use_cached_context)
            .and_then(|continuation| build_cached_ws_request_body(full_body, continuation))
        {
            Some(body) => std::borrow::Cow::Owned(body),
            None => {
                // A stale/unusable continuation is dropped; the full history
                // is sent on this connection (upstream clears it the same
                // way before building the request).
                lease.continuation = None;
                std::borrow::Cow::Borrowed(full_body)
            }
        };
        let frame = encode_response_create_frame(request_body.as_ref());

        if let Some(socket) = lease.socket.as_mut() {
            if let Err(error) = socket.send(Message::Text(frame)).await {
                let _reason = format!("Codex WebSocket send failed: {error}");
                lease.release(false, None).await;
                if !reconnected {
                    reconnected = true;
                    continue 'attempt;
                }
                record_websocket_failure(key.as_deref());
                return Ok(CodexWsOutcome::Fallback);
            }
        }

        // `started` is attempt-scoped like upstream `websocketStarted`: a
        // retried attempt starts a fresh transport, but `start_emitted` keeps
        // the single `Start` of the request.
        let mut started = false;
        loop {
            let incoming = {
                let Some(socket) = lease.socket.as_mut() else {
                    return Err(AiError::Http("Codex WebSocket socket missing".to_string()));
                };
                read_message(socket, idle_timeout, options.signal.as_ref()).await
            };
            let text = match incoming {
                Incoming::Aborted => {
                    lease.release(false, None).await;
                    return Err(AiError::Aborted);
                }
                Incoming::Transport(reason)
                | Incoming::Timeout(reason)
                | Incoming::Closed(reason) => {
                    lease.release(false, None).await;
                    if started {
                        return Err(AiError::Http(reason));
                    }
                    if !reconnected {
                        // Reconnect on close: one fresh connection with the
                        // full history (the dead connection's continuation is
                        // lost with it).
                        reconnected = true;
                        continue 'attempt;
                    }
                    record_websocket_failure(key.as_deref());
                    return Ok(CodexWsOutcome::Fallback);
                }
                Incoming::Message(Message::Ping(payload)) => {
                    if let Some(socket) = lease.socket.as_mut() {
                        let _ = socket.send(Message::Pong(payload)).await;
                    }
                    continue;
                }
                Incoming::Message(Message::Pong(_)) | Incoming::Message(Message::Frame(_)) => {
                    continue;
                }
                Incoming::Message(Message::Close(frame)) => {
                    let reason = close_reason(frame);
                    lease.release(false, None).await;
                    if started {
                        return Err(AiError::Http(reason));
                    }
                    if !reconnected {
                        reconnected = true;
                        continue 'attempt;
                    }
                    record_websocket_failure(key.as_deref());
                    return Ok(CodexWsOutcome::Fallback);
                }
                Incoming::Message(Message::Text(text)) => text,
                Incoming::Message(Message::Binary(bytes)) => match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(error) => {
                        lease.release(false, None).await;
                        return Err(AiError::NonRetryable(format!(
                            "Invalid Codex WebSocket frame: {error}"
                        )));
                    }
                },
            };

            let value: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    lease.release(false, None).await;
                    return Err(AiError::NonRetryable(format!(
                        "Invalid Codex WebSocket JSON: {error}"
                    )));
                }
            };
            let event_type = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            if event_type == "error" || event_type == "response.failed" {
                let code = codex_event_code(&value).unwrap_or_default();
                if code == PREVIOUS_RESPONSE_NOT_FOUND_CODE
                    && !retried_missing_continuation
                    && !start_emitted
                {
                    // Upstream retries once with the full history on a stale
                    // `previous_response_id`. Only BEFORE the request's
                    // `Start` went out: a retry after that would push a second
                    // response stream onto an already-started parser and hand
                    // a suspect socket back to the pool, so the connection is
                    // closed instead (`keep = false`) and no retry runs.
                    retried_missing_continuation = true;
                    lease.release(false, None).await;
                    continue 'attempt;
                }
                if code == WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE
                    && !started
                    && !retried_connection_limit
                {
                    // Upstream `connectionLimitBeforeStart`: only a limit hit
                    // before any event may be retried on a fresh connection.
                    retried_connection_limit = true;
                    lease.release(false, None).await;
                    continue 'attempt;
                }
            }

            let start_snapshot = (!start_emitted).then(|| parser.snapshot());
            let mut ready = Vec::new();
            if let Err(error) = parser.handle_data(&text, &mut ready) {
                lease.release(false, None).await;
                return Err(error);
            }
            if let Some(partial) = start_snapshot {
                start_emitted = true;
                started = true;
                // `false`: the consumer abandoned the stream — close the socket
                // (never pool an unfinished response) and stop reading.
                if !sink
                    .push_await(AssistantMessageEvent::Start { partial })
                    .await
                {
                    lease.release(false, None).await;
                    return Ok(CodexWsOutcome::Completed);
                }
            } else if !ready.is_empty() {
                // A retried attempt after events: mark it so a later transport
                // failure does not fall back to SSE (which would duplicate
                // deltas). Upstream's `websocketStarted` equivalent.
                started = true;
            }
            for event in ready {
                if !sink.push_await(event).await {
                    lease.release(false, None).await;
                    return Ok(CodexWsOutcome::Completed);
                }
            }
            if matches!(
                event_type.as_str(),
                "response.completed" | "response.done" | "response.incomplete"
            ) {
                break;
            }
        }

        // Terminal event seen. Freeze the assistant message for the
        // continuation baseline before `finish` consumes the accumulator.
        let snapshot = parser.snapshot();
        let mut tail = Vec::new();
        if let Err(error) = parser.finish(&mut tail) {
            lease.release(false, None).await;
            return Err(error);
        }
        for event in tail {
            if !sink.push_await(event).await {
                // The consumer is gone: release the connection without
                // storing a continuation nobody will read.
                lease.release(false, None).await;
                return Ok(CodexWsOutcome::Completed);
            }
        }

        let keep = options
            .signal
            .as_ref()
            .is_none_or(|signal| !signal.is_aborted());
        // `websocket` (no cached context) keeps the pooled connection alive
        // but never stores a continuation, matching upstream's
        // `useCachedContext &&` guard around the cache write.
        let continuation = if keep && use_cached_context {
            snapshot
                .response_id
                .as_ref()
                .map(|response_id| Continuation {
                    last_request_body: full_body.clone(),
                    last_response_id: response_id.clone(),
                    last_response_items: assistant_response_items(model, &snapshot),
                })
        } else {
            None
        };
        // The terminal `Done` is already on the sink (it came out of `tail`
        // above), so the consumer may wake and run the session teardown before
        // this release executes: `keep` alone cannot make a teardown final,
        // the lease's pool slot does (see [`Lease::release`]).
        lease.release(keep, continuation).await;
        return Ok(CodexWsOutcome::Completed);
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

static POOL: OnceLock<Mutex<PoolState>> = OnceLock::new();

fn pool() -> &'static Mutex<PoolState> {
    POOL.get_or_init(|| Mutex::new(PoolState::default()))
}

/// The pool lock, poison-recovering. A `std::sync::Mutex` (not tokio's) so the
/// teardown path — including `AgentSession`'s synchronous `Drop` — can close
/// pooled sockets without a runtime; the guard is never held across an
/// `await`, so a synchronous lock costs nothing on the request path. A panic
/// while holding it leaves the state structurally intact (only removals,
/// single inserts and counter bumps happen under it), so the poison is
/// recovered.
fn lock_pool() -> MutexGuard<'static, PoolState> {
    pool().lock().unwrap_or_else(PoisonError::into_inner)
}

/// The pooled connections plus one slot per lease in flight, so a teardown can
/// reach a lease it cannot see and stop it from re-pooling its socket.
///
/// Both live under one lock on purpose: `acquire` takes a pooled entry and
/// enrols its lease in one critical section, and a teardown removes the pooled
/// entries and retires the matching leases in one critical section, so no
/// interleaving can observe or act on one half without the other. Slot storage
/// is bounded by the number of concurrent requests (a slot is removed when its
/// lease is released); nothing accumulates per torn-down session the way a
/// per-key tombstone would.
#[derive(Default)]
struct PoolState {
    connections: HashMap<PoolKey, PooledConnection>,
    /// In-flight leases by ticket id ([`PoolState::next_ticket`] mints them).
    leases: HashMap<u64, LeaseSlot>,
    next_ticket: u64,
}

/// One in-flight lease, as seen from the pool.
struct LeaseSlot {
    key: PoolKey,
    /// A teardown covered this key after the lease was handed out: the socket
    /// is closed on release instead of being pooled.
    retired: bool,
}

impl PoolState {
    /// Give a lease its slot and return its ticket. Called under the lock,
    /// together with taking the key's pooled entry, so a teardown is either
    /// wholly before (this lease connects fresh and may pool: that is the
    /// legitimate reuse path) or wholly after (the slot exists and gets
    /// retired).
    fn enroll(&mut self, key: &PoolKey) -> u64 {
        self.next_ticket += 1;
        self.leases.insert(
            self.next_ticket,
            LeaseSlot {
                key: key.clone(),
                retired: false,
            },
        );
        self.next_ticket
    }

    /// Close every pooled connection for `session_id` and retire the leases in
    /// flight for it.
    fn retire_session(&mut self, session_id: &str) {
        self.connections
            .retain(|key, _| key.session_id != session_id);
        for slot in self.leases.values_mut() {
            if slot.key.session_id == session_id {
                slot.retired = true;
            }
        }
    }

    /// Close every pooled connection and retire every lease in flight.
    fn retire_all(&mut self) {
        self.connections.clear();
        for slot in self.leases.values_mut() {
            slot.retired = true;
        }
    }

    /// Drop the lease's slot: the ticket lives exactly as long as its lease.
    /// Returns whether a teardown retired it while it was in flight.
    fn cancel(&mut self, ticket: u64) -> bool {
        // An absent slot cannot happen (every lease is enrolled by `acquire`
        // and cancelled once by `release`); treat it as retired anyway so an
        // unexpected state never pools a socket a teardown may have wanted.
        self.leases
            .remove(&ticket)
            .map(|slot| slot.retired)
            .unwrap_or(true)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    session_id: String,
    account_id: String,
    url: String,
}

/// Upstream `CachedWebSocketContinuationState`.
struct Continuation {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

/// Upstream `CachedWebSocketConnection`.
struct PooledConnection {
    socket: WsStream,
    created_at: Instant,
    idle_since: Instant,
    continuation: Option<Continuation>,
}

struct Lease {
    socket: Option<WsStream>,
    key: Option<PoolKey>,
    created_at: Instant,
    continuation: Option<Continuation>,
    /// The pool slot of this lease (absent when there is no pool key). Taken
    /// by `release`; whatever is left is cancelled by `Drop`, so an aborted
    /// task never leaves a slot behind.
    ticket: Option<u64>,
}

impl Lease {
    async fn release(mut self, keep: bool, continuation: Option<Continuation>) {
        let Some(socket) = self.socket.take() else {
            return;
        };
        // No ticket means no pool key either (`acquire` enrols exactly the
        // keyed leases): nothing to insert and nothing a teardown could have
        // retired, so the request path stays lock-free for that case.
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let key = self.key.take();
        let mut socket = Some(socket);
        let mut pool = lock_pool();
        // A teardown for this session that landed after this lease was handed
        // out owns the decision: the socket it removed stays removed. The
        // consumer observes the turn's terminal event (and may run a teardown)
        // as soon as the producer's last `push_await` wakes it — BEFORE this
        // release runs on the producer task — so an unchecked insert here used
        // to re-add the very socket the teardown had just closed, and the
        // session's next turn silently reused a connection it asked to drop.
        // The retired flag also covers a teardown that lands mid-turn (session
        // `Drop` while a request is still streaming). The legitimate reuse
        // path (no teardown since acquire) inserts exactly as before.
        let retired = pool.cancel(ticket);
        if keep && !retired {
            if let (Some(key), Some(pooled)) = (key, socket.take()) {
                pool.connections.insert(
                    key,
                    PooledConnection {
                        socket: pooled,
                        created_at: self.created_at,
                        idle_since: Instant::now(),
                        continuation,
                    },
                );
            }
        }
        drop(pool);
        // Outside the lock: closing a connection is not a pool-data operation,
        // and `socket` is `None` when it was handed to the pool above.
        drop(socket);
    }
}

impl Drop for Lease {
    /// A lease can be dropped without `release` (the task was aborted, or a
    /// future returned early); its pool slot must not outlive it. The socket
    /// closes with the drop either way — nothing is pooled.
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            lock_pool().cancel(ticket);
        }
    }
}

/// Owns an enrolled pool slot until a [`Lease`] takes it over: `acquire` awaits
/// the handshake with this guard live, so a future cancelled mid-connect (task
/// abort, `select!`) cancels its slot instead of leaking it.
struct TicketGuard(Option<u64>);

impl TicketGuard {
    /// Hand the slot to the [`Lease`] that will own it. `None` only if the
    /// guard was already empty (unreachable today: `acquire` arms it once and
    /// disarms it once), and it means exactly what an absent lease ticket
    /// means — no pool slot to release.
    fn disarm(mut self) -> Option<u64> {
        self.0.take()
    }
}

impl Drop for TicketGuard {
    fn drop(&mut self) {
        if let Some(ticket) = self.0.take() {
            lock_pool().cancel(ticket);
        }
    }
}

enum AcquireError {
    Transport(String),
    Aborted,
}

async fn acquire(
    key: Option<&PoolKey>,
    url: &str,
    headers: &BTreeMap<String, String>,
    connect_timeout: Option<Duration>,
    signal: Option<&AbortSignal>,
) -> Result<Lease, AcquireError> {
    if let Some(key) = key {
        let now = Instant::now();
        // Scoped: the (non-`Send`) pool guard must be gone before the handshake
        // await below, or the whole request future stops being `Send`.
        let (ticket, pooled) = {
            let mut pool = lock_pool();
            pool.connections.retain(|_, entry| {
                now.saturating_duration_since(entry.created_at) < SESSION_WEBSOCKET_MAX_AGE
                    && now.saturating_duration_since(entry.idle_since) < SESSION_WEBSOCKET_CACHE_TTL
            });
            // Enrolled in the same critical section that takes the pooled
            // entry: a teardown is then either wholly before this acquire (no
            // lease exists yet, so the fresh socket may be pooled — the
            // legitimate reuse path) or wholly after (the slot exists, and is
            // retired).
            let ticket = pool.enroll(key);
            (ticket, pool.connections.remove(key))
        };
        if let Some(entry) = pooled {
            return Ok(Lease {
                socket: Some(entry.socket),
                key: Some(key.clone()),
                created_at: entry.created_at,
                continuation: entry.continuation,
                ticket: Some(ticket),
            });
        }
        // Owned across the handshake `await` so a cancelled `acquire` cancels
        // its slot (`disarm` only on the success path).
        let guard = TicketGuard(Some(ticket));
        let socket = connect_ws(url, headers, connect_timeout, signal).await;
        return match socket {
            Ok(socket) => Ok(Lease {
                socket: Some(socket),
                key: Some(key.clone()),
                created_at: Instant::now(),
                continuation: None,
                ticket: guard.disarm(),
            }),
            Err(error) => Err(match error {
                ConnectError::Aborted => AcquireError::Aborted,
                ConnectError::Transport(message) => AcquireError::Transport(message),
            }),
        };
    }

    let socket = connect_ws(url, headers, connect_timeout, signal)
        .await
        .map_err(|error| match error {
            ConnectError::Aborted => AcquireError::Aborted,
            ConnectError::Transport(message) => AcquireError::Transport(message),
        })?;
    Ok(Lease {
        socket: Some(socket),
        key: None,
        created_at: Instant::now(),
        continuation: None,
        ticket: None,
    })
}

enum ConnectError {
    Transport(String),
    Aborted,
}

async fn connect_ws(
    url: &str,
    headers: &BTreeMap<String, String>,
    connect_timeout: Option<Duration>,
    signal: Option<&AbortSignal>,
) -> Result<WsStream, ConnectError> {
    let mut request = url.into_client_request().map_err(|error| {
        ConnectError::Transport(format!("invalid Codex WebSocket URL: {error}"))
    })?;
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            ConnectError::Transport(format!("invalid header name {name}: {error}"))
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            ConnectError::Transport(format!("invalid header value for {name}: {error}"))
        })?;
        request.headers_mut().insert(header_name, header_value);
    }
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return Err(ConnectError::Aborted);
    }

    // `None` is the settings-disabled timeout (`websocketConnectTimeoutMs: 0`),
    // exactly like upstream's `if (connectTimeoutMs > 0)` guard: the handshake
    // then has no timer and is bounded only by the abort signal.
    let connect = async {
        let connected = match connect_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, connect_async(request)).await {
                Ok(Ok(connected)) => connected,
                Ok(Err(error)) => {
                    return Err(ConnectError::Transport(format!(
                        "Codex WebSocket connect failed: {error}"
                    )))
                }
                Err(_) => {
                    return Err(ConnectError::Transport(format!(
                        "Codex WebSocket connect timeout after {}ms",
                        timeout.as_millis()
                    )))
                }
            },
            None => connect_async(request).await.map_err(|error| {
                ConnectError::Transport(format!("Codex WebSocket connect failed: {error}"))
            })?,
        };
        Ok(connected.0)
    };
    let result = if let Some(signal) = signal {
        tokio::select! {
            _ = signal.cancelled() => return Err(ConnectError::Aborted),
            result = connect => result,
        }
    } else {
        connect.await
    };
    result
}

// ---------------------------------------------------------------------------
// Frames and continuation
// ---------------------------------------------------------------------------

fn encode_response_create_frame(body: &Value) -> String {
    /// Serialize `{"type":"response.create", ..body}` WITHOUT cloning the
    /// body: the former shape built a fresh `Map` and deep-copied every key
    /// and value (the whole transcript sized body) before stringifying it.
    struct Frame<'a> {
        body: &'a serde_json::Map<String, Value>,
    }

    impl serde::Serialize for Frame<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap as _;
            // Same merge rule as the former `Map::insert` loop: a body-level
            // `type` key replaces the envelope value in the FIRST position,
            // and the key is not emitted twice.
            let body_type = self.body.get("type").cloned();
            let mut map = serializer.serialize_map(Some(self.body.len() + 1))?;
            match &body_type {
                Some(value) => map.serialize_entry("type", value)?,
                None => map.serialize_entry("type", "response.create")?,
            }
            for (key, value) in self.body {
                if key != "type" {
                    map.serialize_entry(key, value)?;
                }
            }
            map.end()
        }
    }

    match body.as_object() {
        Some(body) => serde_json::to_string(&Frame { body })
            .unwrap_or_else(|_| r#"{"type":"response.create"}"#.to_string()),
        // Non-object bodies cannot occur for a Responses request body; keep
        // the former envelope rather than panicking.
        None => r#"{"type":"response.create"}"#.to_string(),
    }
}

/// Build a continuation request from the full body (upstream
/// `buildCachedWebSocketRequestBody`): `previous_response_id` plus only the
/// input suffix beyond the cached baseline. `None` means the cache cannot be
/// used and the full body must be sent.
fn build_cached_ws_request_body(full_body: &Value, continuation: &Continuation) -> Option<Value> {
    if !body_without_input_eq(full_body, &continuation.last_request_body) {
        return None;
    }
    // Borrow both input arrays: the baseline only needs to be COMPARED
    // against, never materialized. The former shape cloned the whole current
    // input array and the whole cached input array on every turn (two
    // transcript-sized copies) just to compute the delta suffix.
    let current_input = full_body
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let cached_input = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let response_items = continuation.last_response_items.as_slice();
    let baseline_len = cached_input.len() + response_items.len();
    if current_input.len() < baseline_len {
        return None;
    }
    if current_input[..cached_input.len()] != cached_input[..] {
        return None;
    }
    if current_input[cached_input.len()..baseline_len] != response_items[..] {
        return None;
    }
    // The delta is the only part this turn must own in the new request body.
    let delta = current_input[baseline_len..].to_vec();
    let mut body = full_body.clone();
    body["previous_response_id"] = json!(continuation.last_response_id);
    body["input"] = Value::Array(delta);
    Some(body)
}

/// Whether two request bodies are equal ignoring `input` and
/// `previous_response_id` — WITHOUT cloning either body. The former
/// `body_without_input` cloned both transcript-sized values and removed the
/// two keys just to compare the rest.
fn body_without_input_eq(left: &Value, right: &Value) -> bool {
    const OMITTED: [&str; 2] = ["input", "previous_response_id"];
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return left == right;
    };
    let left_len = left
        .keys()
        .filter(|key| !OMITTED.contains(&key.as_str()))
        .count();
    let right_len = right
        .keys()
        .filter(|key| !OMITTED.contains(&key.as_str()))
        .count();
    left_len == right_len
        && left
            .iter()
            .filter(|(key, _)| !OMITTED.contains(&key.as_str()))
            .all(|(key, value)| right.get(key) == Some(value))
}

fn assistant_response_items(model: &Model, message: &AssistantMessage) -> Vec<Value> {
    let context = Context {
        system_prompt: None,
        messages: vec![AiMessage::Assistant(message.clone())],
        tools: None,
    };
    let options = ConvertResponsesMessagesOptions {
        include_system_prompt: false,
        ..Default::default()
    };
    convert_responses_messages(model, &context, OPENAI_TOOL_CALL_PROVIDERS, &options)
        .into_iter()
        .filter(|item| {
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            item_type != "function_call_output" && item_type != "custom_tool_call_output"
        })
        .collect()
}

fn codex_event_code(value: &Value) -> Option<&str> {
    value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        })
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

enum Incoming {
    Message(Message),
    Transport(String),
    Timeout(String),
    Closed(String),
    Aborted,
}

async fn read_message(
    socket: &mut WsStream,
    idle_timeout: Option<Duration>,
    signal: Option<&AbortSignal>,
) -> Incoming {
    async fn next_with_idle(
        socket: &mut WsStream,
        idle_timeout: Option<Duration>,
    ) -> Result<Option<Result<Message, WsError>>, ()> {
        match idle_timeout {
            Some(timeout) => tokio::time::timeout(timeout, socket.next())
                .await
                .map_err(|_| ()),
            None => Ok(socket.next().await),
        }
    }

    let next = if let Some(signal) = signal {
        tokio::select! {
            _ = signal.cancelled() => return Incoming::Aborted,
            next = next_with_idle(socket, idle_timeout) => next,
        }
    } else {
        next_with_idle(socket, idle_timeout).await
    };
    match next {
        Err(()) => Incoming::Timeout(format!(
            "Codex WebSocket idle timeout after {}ms",
            idle_timeout.unwrap_or_default().as_millis()
        )),
        Ok(None) => Incoming::Closed("WebSocket closed before response.completed".to_string()),
        Ok(Some(Err(error))) => Incoming::Transport(format!("Codex WebSocket error: {error}")),
        Ok(Some(Ok(message))) => Incoming::Message(message),
    }
}

fn close_reason(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame<'_>>) -> String {
    match frame {
        Some(frame) => {
            let code = u16::from(frame.code);
            let mut reason_text = if frame.reason.is_empty() {
                String::new()
            } else {
                format!(" {}", frame.reason)
            };
            // Upstream `extractWebSocketCloseError`: a bare 1009 close is
            // reported as "message too big".
            if reason_text.is_empty() && code == WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE {
                reason_text = " message too big".to_string();
            }
            format!("WebSocket closed {code}{reason_text}")
        }
        None => "WebSocket closed".to_string(),
    }
}
