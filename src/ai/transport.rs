//! Shared HTTP transport for AI providers.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{Stream, StreamExt};
use reqwest::{header::HeaderMap, Method};
use serde::Serialize;

use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{
    AbortSignal, AutoRetryEnd, AutoRetryStart, ProviderStreamHooks, RetryNotice, RetryOutcome,
};

/// Awaitable sink for streamed response body chunks.
///
/// `request_streaming` historically took a synchronous `FnMut(&[u8])`
/// callback, which cannot pause the body read loop: a consumer slower than
/// the provider then accumulates the whole reply somewhere downstream (for
/// assistant streams, one frozen cumulative snapshot per delta). Providers
/// whose per-chunk work can apply backpressure implement this trait instead
/// and call [`ProviderRequestClient::request_streaming_with`] /
/// [`ProviderRequestClient::post_json_streaming_with`]: the read loop awaits
/// [`ChunkSink::send`], so a full downstream event queue stops reading from
/// the socket instead of buffering. The synchronous closure form keeps
/// working through the unchanged [`ProviderRequestClient::request_streaming`]
/// entry (utility HTTP paths that only accumulate bytes).
pub trait ChunkSink: Send {
    /// Deliver one body chunk. The read loop stays paused until the returned
    /// future resolves; an `Err` aborts the request exactly like an error
    /// returned from the synchronous callback used to.
    fn send<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send + 'a>>;
}

/// Binds the legacy synchronous chunk closure to the awaitable [`ChunkSink`]
/// seam (the closure never waits, so the returned future is immediately
/// ready).
struct SyncFnSink<'a>(&'a mut (dyn FnMut(&[u8]) -> AiResult<()> + Send));

impl ChunkSink for SyncFnSink<'_> {
    fn send<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send + 'a>> {
        Box::pin(std::future::ready((self.0)(chunk)))
    }
}

// ---------------------------------------------------------------------------
// `before_provider_request` payload hook (per-request seam)
// ---------------------------------------------------------------------------
//
// Upstream pi dispatches the `before_provider_request` extension event with
// the serialized provider request payload and uses the (possibly replaced)
// result. pir carries the async hook on the per-request
// [`ProviderStreamHooks`] (inside `StreamOptions`): the extension host
// (agent_session) populates it when extensions registered a handler, and each
// provider request applies exactly the hooks of ITS session — concurrent
// sessions can never overwrite (or leak into) each other, and a finished run
// leaves nothing behind for cache-warm/compaction requests to trip over.

/// Async payload hook: receives the parsed request payload, returns the
/// (possibly replaced) payload.
pub type BeforeProviderRequestHook = std::sync::Arc<
    dyn Fn(serde_json::Value) -> crate::agent::tool_execution::BoxFuture<'static, serde_json::Value>
        + Send
        + Sync,
>;

/// Apply the per-request `before_provider_request` hook to a serialized JSON
/// body. Without a hook — or when the body is not JSON (never the case for AI
/// provider requests) — the body passes through unchanged.
async fn apply_before_provider_request(body: Vec<u8>, hooks: &ProviderStreamHooks) -> Vec<u8> {
    let Some(hook) = hooks.before_provider_request.as_ref() else {
        return body;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let replaced = hook(value).await;
    serde_json::to_vec(&replaced).unwrap_or(body)
}

// ---------------------------------------------------------------------------
// `after_provider_response` observer hook (per-request seam)
// ---------------------------------------------------------------------------
//
// Upstream pi fires the `after_provider_response` extension event once the
// final (post-retry) provider HTTP response has been received and before its
// body stream is consumed (`options.onResponse` in the api adapters, e.g.
// packages/ai/src/api/anthropic-messages.ts:657, openai-completions.ts:383;
// handled in packages/coding-agent/src/core/sdk.ts:387-396 via
// `runner.emit({type:"after_provider_response", status, headers})`). pir
// carries the observer on the per-request [`ProviderStreamHooks`] like
// [`apply_before_provider_request`]. No hook → the transport is untouched.

/// Async observer: receives the final response status and headers.
pub type AfterProviderResponseHook = std::sync::Arc<
    dyn Fn(u16, BTreeMap<String, String>) -> crate::agent::tool_execution::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Notify the per-request observer, when installed, that the final
/// non-retried response was received. Awaited so handlers complete before the
/// body stream flows.
async fn apply_after_provider_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    hooks: &ProviderStreamHooks,
) {
    if let Some(hook) = hooks.after_provider_response.as_ref() {
        hook(status, headers.clone()).await;
    }
}

/// Configuration for provider HTTP requests.
#[derive(Debug, Clone)]
pub struct ProviderRequestConfig {
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub max_retries: u32,
    pub max_retry_delay: Duration,
    /// Proxy URL for all provider HTTP traffic (upstream `httpProxy` setting,
    /// applied as reqwest proxy; `None` uses reqwest's environment defaults).
    pub proxy_url: Option<String>,
    /// HTTP idle timeout (settings `httpIdleTimeoutMs`): bounds the wait for
    /// the response headers and the wait between successive body chunk reads
    /// (upstream wires the value into the undici dispatcher's
    /// `headersTimeout`/`bodyTimeout`, where the timer resets on every
    /// read-progress event — long SSE streams are never killed by total
    /// duration, only by silence). `None` disables it. Approximation: reqwest
    /// has no per-read idle knob, so the streaming path applies
    /// `tokio::time::timeout` around the header wait and each chunk read
    /// (semantically equivalent); the per-request total `.timeout()` is NOT
    /// applied to streamed responses for the same reason.
    pub http_idle_timeout: Option<Duration>,
}

/// Default transport retry count for the shared provider client.
const DEFAULT_PROVIDER_MAX_RETRIES: u32 = 2;

/// Default retry count for the ChatGPT Codex endpoint (upstream
/// `openai-codex-responses.ts` `DEFAULT_MAX_RETRIES = 0`): the backend
/// reports usage-limit 429s with minute-scale reset windows that
/// transport-level retries only duplicate.
pub const CODEX_DEFAULT_MAX_RETRIES: u32 = 0;

/// How much of a non-2xx response body is surfaced in the error message
/// (upstream `MAX_PROVIDER_ERROR_BODY_CHARS`, error-body.ts:18).
///
/// The previous 500-char cut could discard the tail of a large JSON error body
/// — exactly where overflow (`request_too_large`, "prompt is too long") and
/// retry ("overloaded", "rate limit") keywords often sit — so a transport
/// failure that upstream classifies (compact-and-retry or turn retry) looked
/// permanent here. The cap and the visible truncation marker now mirror
/// upstream `truncateErrorText`.
pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

impl Default for ProviderRequestConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_response_bytes: 16 * 1024 * 1024,
            max_retries: DEFAULT_PROVIDER_MAX_RETRIES,
            max_retry_delay: Duration::from_secs(60),
            proxy_url: Self::env_proxy_url(),
            http_idle_timeout: Self::env_http_idle_timeout(),
        }
    }
}

/// Whether the URL targets the ChatGPT Codex responses endpoint
/// (`{base}/codex/responses`, the shape the `openai_responses` adapter's
/// `resolve_codex_url` produces). Public for tests/.
pub fn is_codex_responses_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.trim_end_matches('/').ends_with("/codex/responses")
}

impl ProviderRequestConfig {
    /// The proxy URL for Pi-managed HTTP clients: the settings `httpProxy`
    /// (global scope) first, then `PI_HTTP_PROXY` / `HTTPS_PROXY` /
    /// `HTTP_PROXY` environment fallbacks (upstream applies the setting as
    /// HTTP_PROXY/HTTPS_PROXY).
    ///
    /// Public to the crate: every Pi-owned reqwest client must resolve its
    /// proxy here. reqwest is built with `default-features = false` (no
    /// `system-proxy`), so a bare `Client::builder()` ignores env/OS proxies;
    /// a client that skips this helper has no egress behind a proxy.
    pub(crate) fn env_proxy_url() -> Option<String> {
        if let Some(proxy) = crate::config::settings::current_http_proxy() {
            return Some(proxy);
        }
        [
            "PI_HTTP_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    }

    /// The idle timeout for Pi-managed HTTP clients: the global-scope
    /// `httpIdleTimeoutMs` setting (upstream `configureHttpDispatcher` reads
    /// the same value; default 300000 = 5 min, `0` disabled).
    fn env_http_idle_timeout() -> Option<Duration> {
        let ms = crate::config::settings::current_http_idle_timeout_ms();
        (ms > 0).then(|| Duration::from_millis(ms))
    }
}

// ---------------------------------------------------------------------------
// HTTP transport seam
// ---------------------------------------------------------------------------
//
// Everything the transport guarantees — the retry loop, backoff and
// `Retry-After` handling, abort polling, idle timeouts, the streaming read
// path and its awaitable [`ChunkSink`] backpressure — lives in
// [`ProviderRequestClient`]. The one thing it cannot own itself is the
// socket, so every attempt executes through this seam: production installs
// the reqwest implementation ([`ReqwestTransport`], the exact request/send/
// body-read path the transport always had), and tests install a scripted
// fake, which makes mid-stream failures, stalls and backpressure
// deterministic without a local HTTP server.
//
// The seam carries only what the loop already worked with — a request with
// validated headers, and a response head (status + headers) plus a body
// stream. Classification inputs stay identical: a send failure is an
// [`AiError::Http`] from the implementation (as the direct `reqwest::Error`
// mapping produced before) and a body-stream `Err` is that same
// [`AiError::Http`] shape, so the pre-body/retryable/mid-body rules read the
// same values they always did.

/// One response-body chunk.
///
/// Type-erased behind `AsRef<[u8]>` because reqwest yields `bytes::Bytes`
/// frames and pir has no direct `bytes` dependency to name that type. Boxing
/// a `Bytes` value is a refcounted-handle move, NOT a payload copy, so the
/// streaming path keeps the allocation behavior of the direct
/// `bytes_stream()` it replaced (a `Vec<u8>` chunk type would pay one memcpy
/// per frame of every provider stream).
pub struct HttpBodyChunk(Box<dyn AsRef<[u8]> + Send + 'static>);

impl HttpBodyChunk {
    /// Wrap one byte block (the reqwest implementation passes a `Bytes`).
    pub fn new(bytes: impl AsRef<[u8]> + Send + 'static) -> Self {
        Self(Box::new(bytes))
    }

    /// The chunk bytes.
    pub fn as_bytes(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

impl From<Vec<u8>> for HttpBodyChunk {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl std::fmt::Debug for HttpBodyChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HttpBodyChunk({} bytes)", self.as_bytes().len())
    }
}

/// One prepared HTTP request handed to [`HttpTransport::execute`]: headers
/// already validated (validation failures keep failing the request fast,
/// before any attempt is made), body owned by the attempt.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// One executed response: status, headers, and the body as a stream of
/// [`HttpBodyChunk`]s. A stream `Err` is a body read failure carrying the
/// same [`AiError::Http`] message the direct reqwest read produced.
pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: HttpBodyStream,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &"<stream>")
            .finish()
    }
}

/// Body stream of an [`HttpResponse`].
pub type HttpBodyStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<HttpBodyChunk, AiError>> + Send + 'static>>;

/// The transport execution seam.
///
/// [`ProviderRequestClient`] performs one execution per attempt through this
/// trait; [`Self::execute`] futures are awaited under the header idle timeout
/// and their responses streamed exactly like the direct reqwest client did.
/// The trait is object safe (`Arc<dyn HttpTransport>`) and requires
/// `Send + Sync + 'static` so the provider adapters keep spawning their
/// request future unchanged ([`ProviderRequestClient`] stays `Send + Sync`).
pub trait HttpTransport: Send + Sync + 'static {
    /// Execute one request. An `Err` is a pre-body failure (connect error,
    /// TLS, malformed URL) and flows through the retry loop's classification
    /// exactly like a reqwest send error did.
    fn execute(
        &self,
        request: HttpRequest,
    ) -> crate::agent::tool_execution::BoxFuture<'_, AiResult<HttpResponse>>;
}

/// The default [`HttpTransport`]: reqwest over the platform TLS stack, built
/// with the client's connect timeout and optional proxy.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build the shared reqwest client — the construction
    /// [`ProviderRequestClient::new`] has always used.
    pub fn new(config: &ProviderRequestConfig) -> AiResult<Self> {
        let mut builder = reqwest::Client::builder().connect_timeout(config.timeout);
        if let Some(proxy_url) = &config.proxy_url {
            let proxy = reqwest::Proxy::all(proxy_url)
                .map_err(|error| AiError::Http(format!("invalid proxy URL: {error}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder.build().map_err(|e| AiError::Http(e.to_string()))?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn execute(
        &self,
        request: HttpRequest,
    ) -> crate::agent::tool_execution::BoxFuture<'_, AiResult<HttpResponse>> {
        Box::pin(async move {
            let HttpRequest {
                method,
                url,
                headers,
                body,
            } = request;
            let response = self
                .client
                .request(method, url)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|error| AiError::Http(error.to_string()))?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = response.bytes_stream().map(|item| {
                item.map_err(|error| AiError::Http(error.to_string()))
                    .map(HttpBodyChunk::new)
            });
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(body),
            })
        })
    }
}

/// A reusable provider HTTP client with bounded retries and response reads.
#[derive(Clone)]
pub struct ProviderRequestClient {
    /// The attempt executor: [`ReqwestTransport`] by default ([`Self::new`]),
    /// or an arbitrary transport via [`Self::with_transport`].
    transport: Arc<dyn HttpTransport>,
    pub config: ProviderRequestConfig,
    /// Per-client payload diagnostics (see [`Self::payload_diagnostics`]).
    ///
    /// MEMORY SHAPE (pir extension): a JSON request body is materialized once
    /// (it has to be — `reqwest` owns the bytes it sends) and, when a retry is
    /// still possible, KEPT so the next attempt reuses it; the former shape
    /// cloned the whole body unconditionally on every attempt, a second
    /// full-size copy of a summarizer chunk per call. The counters make that
    /// property testable: a single-attempt request must report one allocation
    /// and ZERO copies.
    payload: Arc<PayloadDiagnostics>,
    /// Per-request total timeout (upstream `StreamOptions.timeoutMs`, set by
    /// the adapter from `options.timeout_ms`). Reset on this client clone, so
    /// the shared client keeps its own `None` default. `None` = bounded only
    /// by the idle timeout.
    request_timeout: Option<Duration>,
}

/// Which limit bounded a transport wait (see
/// [`ProviderRequestClient::wait_limit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutKind {
    Idle,
    Request,
}

/// Counters behind [`ProviderRequestClient::payload_diagnostics`].
#[derive(Debug, Default)]
struct PayloadDiagnostics {
    allocations: std::sync::atomic::AtomicU64,
    copies: std::sync::atomic::AtomicU64,
}

/// One client's payload-construction counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PayloadDiagnosticsSnapshot {
    /// Request bodies serialized (`serde_json::to_vec` successes).
    pub allocations: u64,
    /// Full-body copies made to keep a body available for a retry. A request
    /// with no retry budget and a request on its last possible attempt make
    /// none.
    pub copies: u64,
}

impl ProviderRequestClient {
    /// Build a client over the default reqwest transport (connect timeout and
    /// optional proxy come from `config`; unchanged pre-seam construction).
    pub fn new(config: ProviderRequestConfig) -> AiResult<Self> {
        let transport = Arc::new(ReqwestTransport::new(&config)?);
        Ok(Self::with_transport(config, transport))
    }

    /// [`Self::new`] over an arbitrary [`HttpTransport`] (additive seam): the
    /// retry loop, backoff and `Retry-After` handling, observability hooks,
    /// idle timeouts and the streaming read path are identical for every
    /// transport. Tests script a fake transport here; production keeps the
    /// reqwest default.
    pub fn with_transport(
        config: ProviderRequestConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            transport,
            config,
            payload: Arc::new(PayloadDiagnostics::default()),
            request_timeout: None,
        }
    }

    /// Set a per-request total timeout (upstream `options.timeoutMs`).
    /// Applied to the header wait and every body read of each attempt.
    pub fn set_request_timeout(&mut self, timeout: Option<Duration>) {
        self.request_timeout = timeout;
    }

    /// Builder form of [`Self::set_request_timeout`].
    pub fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Replace the attempt executor for this client clone (upstream
    /// per-request `fetch` injection).
    pub fn set_transport(&mut self, transport: Arc<dyn HttpTransport>) {
        self.transport = transport;
    }

    /// The payload-construction counters of this client (see
    /// [`PayloadDiagnosticsSnapshot`]).
    pub fn payload_diagnostics(&self) -> PayloadDiagnosticsSnapshot {
        PayloadDiagnosticsSnapshot {
            allocations: self
                .payload
                .allocations
                .load(std::sync::atomic::Ordering::Relaxed),
            copies: self
                .payload
                .copies
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Reset the payload counters (test seam).
    pub fn reset_payload_diagnostics(&self) {
        self.payload
            .allocations
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .copies
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Serialize one JSON request body, counting the allocation.
    fn serialize_payload<T: Serialize>(&self, body: &T) -> AiResult<Vec<u8>> {
        let payload = serde_json::to_vec(body)?;
        self.payload
            .allocations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(payload)
    }

    pub fn with_default_config() -> AiResult<Self> {
        Self::new(ProviderRequestConfig::default())
    }

    /// The retry budget for one request: the client's configured count,
    /// except that the ChatGPT Codex endpoint starts from
    /// [`CODEX_DEFAULT_MAX_RETRIES`] (upstream
    /// `openai-codex-responses.ts` `DEFAULT_MAX_RETRIES = 0`) while the client
    /// still carries the shared transport default — i.e. when neither the
    /// provider constructor nor the per-call `options.max_retries` override
    /// set an explicit count. Detection is by request URL, so an explicit
    /// override equal to the shared default (2) is indistinguishable and also
    /// maps to the Codex default (the URL probe itself,
    /// [`is_codex_responses_url`], is public for tests/).
    fn effective_max_retries(&self, url: &str) -> u32 {
        if is_codex_responses_url(url) && self.config.max_retries == DEFAULT_PROVIDER_MAX_RETRIES {
            CODEX_DEFAULT_MAX_RETRIES
        } else {
            self.config.max_retries
        }
    }

    /// Incrementally stream a provider HTTP response body.
    ///
    /// Same retry semantics as [`Self::request`] (connect errors, retryable
    /// statuses, `retry-after`, abort polling) with one difference: body chunks
    /// are handed to `on_chunk` as they arrive instead of being buffered. The
    /// retry logic never double-delivers chunks: it only retries while the
    /// failure happened before any chunk reached `on_chunk` (connect error,
    /// retryable status, or a mid-body read failure on the first chunk of a
    /// successful response); once a chunk has been delivered, any failure is
    /// fatal. Error-status bodies are never delivered to `on_chunk`; they are
    /// buffered to build the same `NonRetryable` message as the buffered path.
    ///
    /// This synchronous-callback entry never waits on `on_chunk`. Providers
    /// that must apply backpressure use
    /// [`request_streaming_with`](Self::request_streaming_with).
    ///
    /// Returns the final status and response headers.
    // Eight parameters mirror the streaming call shape of the buffered
    // `request` path plus the chunk callback; a params struct would touch
    // every provider transport for no behavioral gain.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_streaming(
        &self,
        method: Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Vec<u8>,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut (dyn FnMut(&[u8]) -> AiResult<()> + Send),
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let mut sink = SyncFnSink(on_chunk);
        self.request_streaming_with(method, url, headers, body, signal, hooks, &mut sink)
            .await
    }

    /// [`request_streaming`](Self::request_streaming) with an awaitable chunk
    /// sink: the body read loop awaits [`ChunkSink::send`] for every chunk, so
    /// a sink that waits on downstream capacity (a full assistant event queue)
    /// pauses the socket read instead of buffering the reply. Same retry and
    /// abort semantics as the synchronous entry.
    ///
    /// Observable retry lifecycle (upstream `auto_retry_start` /
    /// `auto_retry_end`, agent-session.ts:220-222): every retry wait fires
    /// [`ProviderStreamHooks::on_retry_start`] with the 1-based attempt, the
    /// retry budget, the post-`Retry-After` delay and the triggering error,
    /// and a request that scheduled at least one retry fires
    /// [`ProviderStreamHooks::on_retry_end`] exactly once when its final
    /// attempt resolves (success / final failure / cancellation). The hooks
    /// are pure observers — retry counts, backoff computation, `Retry-After`
    /// priority and cancellation semantics are untouched.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_streaming_with<C: ChunkSink>(
        &self,
        method: Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Vec<u8>,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        self.request_streaming_with_no_retry_statuses(
            method,
            url,
            headers,
            body,
            signal,
            hooks,
            &[],
            on_chunk,
        )
        .await
    }

    /// [`request_streaming_with`](Self::request_streaming_with) with upstream
    /// `noRetryStatuses`: a listed HTTP status is never retried even when the
    /// status policy (or an `x-should-retry: true` header) would retry it. The
    /// response still takes the ordinary non-2xx path, so the caller observes
    /// the same `NonRetryable("HTTP <status>: ...")` error it would after an
    /// exhausted retry budget. An empty list is exactly
    /// [`request_streaming_with`](Self::request_streaming_with).
    #[allow(clippy::too_many_arguments)]
    async fn request_streaming_with_no_retry_statuses<C: ChunkSink>(
        &self,
        method: Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Vec<u8>,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        no_retry_statuses: &[u16],
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        // The retry loop is unchanged; this wrapper only adds the
        // `auto_retry_end` observation once the final attempt resolved.
        // `retries_started` is the upstream `_retryAttempt` counter (number of
        // scheduled retries), incremented exactly when a retry wait starts.
        let mut retries_started: u32 = 0;
        let outcome = self
            .request_streaming_attempts(
                method,
                url,
                headers,
                body,
                signal,
                hooks,
                no_retry_statuses,
                on_chunk,
                &mut retries_started,
            )
            .await;
        if retries_started > 0 {
            if let Some(hook) = hooks.on_retry_end.as_ref() {
                // A user cancellation reports "Retry cancelled" (upstream
                // `_finishCancelledRetry`); any other terminal error reports
                // its message like upstream's final `errorMessage`.
                let end = match &outcome {
                    Ok(_) => AutoRetryEnd {
                        success: true,
                        attempt: retries_started,
                        final_error: None,
                    },
                    Err(AiError::RetryCancelled { .. }) | Err(AiError::Aborted) => AutoRetryEnd {
                        success: false,
                        attempt: retries_started,
                        final_error: Some("Retry cancelled".to_owned()),
                    },
                    Err(error) => AutoRetryEnd {
                        success: false,
                        attempt: retries_started,
                        final_error: Some(error.to_string()),
                    },
                };
                hook(&end);
            }
        }
        outcome
    }

    /// The retry loop behind [`Self::request_streaming_with`] (same behavior
    /// as the pre-seam implementation). `retries_started` counts the scheduled
    /// retry waits for the `auto_retry_end` observation.
    #[allow(clippy::too_many_arguments)]
    async fn request_streaming_attempts<C: ChunkSink>(
        &self,
        method: Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Vec<u8>,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        no_retry_statuses: &[u16],
        on_chunk: &mut C,
        retries_started: &mut u32,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        // `before_provider_request` hook: transform the payload once, before
        // the retry loop (retries reuse the transformed body).
        let body = apply_before_provider_request(body, hooks).await;
        // Retry budget for this request: the Codex endpoint defaults to zero
        // retries when neither the client construction nor the stream options
        // set an explicit count (see [`ProviderRequestClient::effective_max_retries`]).
        let max_retries = self.effective_max_retries(url);
        // The body is MOVED into the request on the attempt that cannot be
        // followed by another one (the retry budget is exhausted); only an
        // attempt that must keep the body for a possible retry pays a copy.
        // The former shape cloned the whole body on EVERY attempt, so the
        // common no-retry request carried a second full-size copy of it (a
        // summarizer chunk is ~0.3 MB; a 248-chunk span paid it 248 times).
        let mut body_slot: Option<Vec<u8>> = Some(body);
        let mut attempt = 0;
        'attempt: loop {
            if let Some(signal) = signal {
                if signal.is_aborted() {
                    return Err(AiError::Aborted);
                }
            }
            let payload = if attempt >= max_retries {
                body_slot.take().unwrap_or_default()
            } else if let Some(body) = body_slot.as_ref() {
                self.payload
                    .copies
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                body.clone()
            } else {
                Vec::new()
            };
            let deadline = self
                .request_timeout
                // A caller-supplied `timeoutMs` this large would overflow the
                // instant; degrade to "no total deadline" (the idle timeout
                // still bounds the wait) instead of panicking.
                .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout));
            let request = Self::build_request(&method, url, headers, payload)?;
            // Upstream bounds the header wait with the undici
            // `headersTimeout` (the idle setting, reset per progress event).
            let response = match self.send_with_idle(request, deadline, signal).await {
                Ok(response) => response,
                Err(error) => {
                    // A terminal verdict from the transport seam (an abort or
                    // an error the implementation already classified as
                    // non-retryable, e.g. a custom `options.fetch` transport)
                    // must fail at once: retrying it would sleep through a
                    // backoff and duplicate the request, and the final
                    // `RetryExhausted` would hide the real verdict.
                    if is_terminal_request_error(&error) {
                        return Err(error);
                    }
                    if attempt < max_retries {
                        self.backoff(
                            attempt,
                            &error.to_string(),
                            None,
                            None,
                            signal,
                            hooks,
                            retries_started,
                        )
                        .await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(AiError::RetryExhausted {
                        attempts: attempt + 1,
                        message: error.to_string(),
                    });
                }
            };
            let status = response.status;
            let retry_after = response
                .headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let retry_after_ms = response
                .headers
                .get("retry-after-ms")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let headers_out = response
                .headers
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|value| (k.to_string(), value.to_string()))
                })
                .collect::<BTreeMap<_, _>>();
            if !no_retry_statuses.contains(&status)
                && is_retryable_response(status, &headers_out)
                && attempt < max_retries
            {
                self.backoff(
                    attempt,
                    // The retry path does not drain the error body; the
                    // status is the cheap, deterministic trigger summary
                    // (upstream surfaces the SDK's status-bearing message).
                    &format!("HTTP {status}"),
                    retry_after_ms.as_deref(),
                    retry_after.as_deref(),
                    signal,
                    hooks,
                    retries_started,
                )
                .await?;
                attempt += 1;
                continue;
            }
            if !(200..300).contains(&status) {
                // Buffer the error body for the message (mirrors the buffered
                // path); it is never delivered to `on_chunk`, so mid-body read
                // failures may still retry the whole request.
                let mut stream = response.body;
                // Error bodies are truncated to the upstream
                // [`MAX_PROVIDER_ERROR_BODY_CHARS`] window for the message;
                // a small initial capacity avoids repeated growth for the
                // common short-body case without penalizing large ones.
                let mut bytes = Vec::with_capacity(1024);
                loop {
                    if let Some(signal) = signal {
                        if signal.is_aborted() {
                            return Err(AiError::Aborted);
                        }
                    }
                    let chunk = match self.next_body_chunk(&mut stream, deadline, signal).await {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(error) => {
                            // A terminal verdict (abort or non-retryable
                            // transport error) never re-issues the request.
                            if is_terminal_request_error(&error) {
                                return Err(error);
                            }
                            // A noRetryStatuses status never re-issues the
                            // request, not even for a body-read failure: the
                            // upstream policy fails the whole call once the
                            // status itself is refused.
                            if !no_retry_statuses.contains(&status) && attempt < max_retries {
                                self.backoff(
                                    attempt,
                                    &error.to_string(),
                                    retry_after_ms.as_deref(),
                                    retry_after.as_deref(),
                                    signal,
                                    hooks,
                                    retries_started,
                                )
                                .await?;
                                attempt += 1;
                                continue 'attempt;
                            }
                            return Err(error);
                        }
                    };
                    let chunk = chunk.as_bytes();
                    if bytes.len().saturating_add(chunk.len()) > self.config.max_response_bytes {
                        return Err(AiError::ResponseTooLarge {
                            limit: self.config.max_response_bytes,
                        });
                    }
                    bytes.extend_from_slice(chunk);
                }
                let message = truncate_provider_error_body(&String::from_utf8_lossy(&bytes));
                // The `HTTP <status>: <body>` text shape is load-bearing:
                // adapter parsers (anthropic strict-tools fallback, codex
                // error enhancement) and `AiError::provider_http_error` read
                // it, and the finalizer (`ai::api::terminal_error_message*`)
                // renders the upstream-facing 413 shape from the same values.
                return Err(AiError::NonRetryable(format!("HTTP {status}: {message}")));
            }
            // `after_provider_response` hook: the final (non-retried) 2xx
            // response is known here — after the retry decision, before the
            // body stream is consumed. Upstream only reaches its `onResponse`
            // callback when the provider SDK returned the response
            // successfully (a non-2xx throws inside the SDK), so the error
            // status branch above never fires the event there either.
            apply_after_provider_response(status, &headers_out, hooks).await;
            // Success: stream the body through `on_chunk`. Nothing has been
            // delivered yet at the start of the attempt, so a read failure
            // before the first chunk may still retry; after that it is fatal.
            // Each chunk read runs under the idle timeout (upstream undici
            // `bodyTimeout`), so a long-lived SSE stream is only killed when
            // it stops making progress — never by total duration.
            let mut delivered = false;
            let mut total = 0usize;
            let mut stream = response.body;
            loop {
                if let Some(signal) = signal {
                    if signal.is_aborted() {
                        return Err(AiError::Aborted);
                    }
                }
                let chunk = match self.next_body_chunk(&mut stream, deadline, signal).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(error) => {
                        if is_terminal_request_error(&error) {
                            return Err(error);
                        }
                        if !delivered && attempt < max_retries {
                            self.backoff(
                                attempt,
                                &error.to_string(),
                                retry_after_ms.as_deref(),
                                retry_after.as_deref(),
                                signal,
                                hooks,
                                retries_started,
                            )
                            .await?;
                            attempt += 1;
                            continue 'attempt;
                        }
                        return Err(error);
                    }
                };
                let chunk = chunk.as_bytes();
                total = total.saturating_add(chunk.len());
                if total > self.config.max_response_bytes {
                    return Err(AiError::ResponseTooLarge {
                        limit: self.config.max_response_bytes,
                    });
                }
                on_chunk.send(chunk).await?;
                delivered = true;
            }
            return Ok((status, headers_out));
        }
    }

    /// [`post_json_streaming`](Self::post_json_streaming) with an awaitable
    /// chunk sink; see [`request_streaming_with`](Self::request_streaming_with).
    pub async fn post_json_streaming_with<T: Serialize, C: ChunkSink>(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &T,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let payload = self.serialize_payload(body)?;
        self.request_streaming_with(Method::POST, url, headers, payload, signal, hooks, on_chunk)
            .await
    }

    /// [`post_json_streaming`](Self::post_json_streaming) with per-request
    /// `noRetryStatuses` (upstream `provider-retry.ts:121`): a listed status
    /// fails the request immediately, without consuming the retry budget or
    /// sleeping through a backoff, while every other status keeps the unchanged
    /// retry policy. An empty list is exactly [`Self::post_json_streaming`].
    #[allow(clippy::too_many_arguments)]
    pub async fn post_json_streaming_with_no_retry_statuses<T: Serialize>(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &T,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        no_retry_statuses: &[u16],
        on_chunk: &mut (dyn FnMut(&[u8]) -> AiResult<()> + Send),
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let payload = self.serialize_payload(body)?;
        let mut sink = SyncFnSink(on_chunk);
        self.request_streaming_with_no_retry_statuses(
            Method::POST,
            url,
            headers,
            payload,
            signal,
            hooks,
            no_retry_statuses,
            &mut sink,
        )
        .await
    }

    pub async fn post_json_streaming<T: Serialize>(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &T,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut (dyn FnMut(&[u8]) -> AiResult<()> + Send),
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let payload = self.serialize_payload(body)?;
        let mut sink = SyncFnSink(on_chunk);
        self.request_streaming_with(
            Method::POST,
            url,
            headers,
            payload,
            signal,
            hooks,
            &mut sink,
        )
        .await
    }

    /// Validate the caller headers and pack one attempt's request for the
    /// seam. Header validation is the only failure here and keeps its
    /// fail-fast, never-retried behavior.
    ///
    /// No per-request total `.timeout()` is attached anywhere: every caller
    /// streams, and a total timer would kill long-lived SSE streams
    /// regardless of progress (upstream bounds streams with the undici idle
    /// timeouts only). The request is bounded by the connect timeout, the
    /// header idle timeout, and the per-chunk idle timeout instead.
    fn build_request(
        method: &Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Vec<u8>,
    ) -> AiResult<HttpRequest> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            // An empty value is the upstream `null` suppression marker: the
            // header shadows a default but must not be sent.
            if value.is_empty() {
                continue;
            }
            let key = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| AiError::Http(e.to_string()))?;
            let val = reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| AiError::Http(e.to_string()))?;
            map.insert(key, val);
        }
        Ok(HttpRequest {
            method: method.clone(),
            url: url.to_string(),
            headers: map,
            body,
        })
    }

    /// The error text for an expired per-request total timeout (upstream
    /// `options.timeoutMs`).
    ///
    /// The wait limit handed to the timer may be a 1ms polling floor (the
    /// remaining budget had already run out when a read started) or a partial
    /// remainder. Neither is the user-facing budget: report the CONFIGURED
    /// `timeoutMs`, and say explicitly that the total deadline was exceeded
    /// when the timer was only polling an already-expired budget. The 1ms
    /// floor stays internal to the timer, never in the message.
    fn request_timeout_error(&self, deadline: Option<tokio::time::Instant>) -> AiError {
        let configured = self.request_timeout.unwrap_or_default().as_millis();
        let expired = deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
        if expired {
            AiError::Http(format!(
                "HTTP request timed out: total deadline of {configured}ms exceeded (timeoutMs)"
            ))
        } else {
            AiError::Http(format!(
                "HTTP request timed out after {configured}ms (timeoutMs)"
            ))
        }
    }

    /// Await the response send under the idle timeout and/or the per-request
    /// total timeout (upstream undici `headersTimeout` + `options.timeoutMs`):
    /// elapsed ⇒ an [`AiError::Http`] timeout error, which the retry loop
    /// treats like any other pre-body failure. Send errors arrive pre-mapped
    /// from the transport implementation.
    ///
    /// A caller abort cancels the in-flight wait at once (upstream `fetch`
    /// rejects when its `signal` aborts): without racing the signal here a
    /// silent server would keep the request — and an already-cancelled turn —
    /// parked until the idle/total timer fired.
    async fn send_with_idle(
        &self,
        request: HttpRequest,
        deadline: Option<tokio::time::Instant>,
        signal: Option<&AbortSignal>,
    ) -> Result<HttpResponse, AiError> {
        let send = self.transport.execute(request);
        let wait = async {
            match self.wait_limit(deadline) {
                Some((limit, TimeoutKind::Request)) => {
                    match tokio::time::timeout(limit, send).await {
                        Ok(response) => response,
                        Err(_) => Err(self.request_timeout_error(deadline)),
                    }
                }
                Some((limit, TimeoutKind::Idle)) => match tokio::time::timeout(limit, send).await {
                    Ok(response) => response,
                    Err(_) => Err(AiError::Http(format!(
                        "HTTP headers idle timeout after {}s (httpIdleTimeoutMs)",
                        limit.as_secs()
                    ))),
                },
                None => send.await,
            }
        };
        match signal {
            Some(signal) => tokio::select! {
                outcome = wait => outcome,
                _ = signal.cancelled() => Err(AiError::Aborted),
            },
            None => wait.await,
        }
    }

    /// The next body chunk under the idle and/or total timeout (upstream
    /// undici `bodyTimeout`, reset on every read-progress event, plus the
    /// request's total `timeoutMs`). `Ok(None)` ends the body; `Err` is a
    /// timeout or a stream failure, both flowing through the streaming path's
    /// retry rules. Stream errors arrive pre-mapped from the transport
    /// implementation (reqwest maps its `reqwest::Error` to [`AiError::Http`]
    /// with the same message the direct read produced).
    ///
    /// The caller abort races the read (see [`Self::send_with_idle`]): a
    /// stalled body must not delay a cancellation until the idle timer.
    async fn next_body_chunk(
        &self,
        stream: &mut HttpBodyStream,
        deadline: Option<tokio::time::Instant>,
        signal: Option<&AbortSignal>,
    ) -> Result<Option<HttpBodyChunk>, AiError> {
        let wait = async {
            match self.wait_limit(deadline) {
                Some((limit, TimeoutKind::Request)) => {
                    match tokio::time::timeout(limit, stream.next()).await {
                        Ok(next) => Ok(next),
                        Err(_) => Err(self.request_timeout_error(deadline)),
                    }
                }
                Some((limit, TimeoutKind::Idle)) => {
                    match tokio::time::timeout(limit, stream.next()).await {
                        Ok(next) => Ok(next),
                        Err(_) => Err(AiError::Http(format!(
                            "HTTP body idle timeout after {}s (httpIdleTimeoutMs)",
                            limit.as_secs()
                        ))),
                    }
                }
                None => Ok(stream.next().await),
            }
        };
        let next = match signal {
            Some(signal) => tokio::select! {
                next = wait => next,
                _ = signal.cancelled() => return Err(AiError::Aborted),
            },
            None => wait.await,
        };
        match next {
            Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
            Ok(Some(Err(error))) => Err(error),
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The tighter of the idle timeout and the remaining request-timeout
    /// budget, with which one bounds the wait.
    fn wait_limit(
        &self,
        deadline: Option<tokio::time::Instant>,
    ) -> Option<(Duration, TimeoutKind)> {
        let remaining = deadline.map(|deadline| {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .max(Duration::from_millis(1))
        });
        match (self.config.http_idle_timeout, remaining) {
            (Some(idle), Some(remaining)) if remaining < idle => {
                Some((remaining, TimeoutKind::Request))
            }
            (Some(idle), _) => Some((idle, TimeoutKind::Idle)),
            (None, Some(remaining)) => Some((remaining, TimeoutKind::Request)),
            (None, None) => None,
        }
    }

    /// Compute and wait out one retry delay (`error_message` is the
    /// `auto_retry_start` trigger summary; the wait itself is unchanged).
    #[allow(clippy::too_many_arguments)]
    async fn backoff(
        &self,
        attempt: u32,
        error_message: &str,
        retry_after_ms: Option<&str>,
        retry_after: Option<&str>,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        retries_started: &mut u32,
    ) -> AiResult<()> {
        // Upstream provider-retry: `retry-after-ms` wins over `retry-after`,
        // and a server-requested delay above the configured cap fails the
        // request immediately instead of silently clamping it.
        if let Some(delay) = server_retry_delay(retry_after_ms, retry_after) {
            let max = self.config.max_retry_delay;
            if max > Duration::ZERO && delay > max {
                return Err(AiError::NonRetryable(format!(
                    "Server requested {}s retry delay (max: {}s).",
                    delay.as_secs_f64().ceil() as u64,
                    max.as_secs_f64().ceil() as u64
                )));
            }
            return self
                .wait_backoff(
                    delay,
                    attempt,
                    error_message,
                    signal,
                    hooks,
                    retries_started,
                )
                .await;
        }
        self.wait_backoff(
            exponential_backoff_delay(attempt),
            attempt,
            error_message,
            signal,
            hooks,
            retries_started,
        )
        .await
    }

    /// Wait out one retry delay. An installed `on_retry` hook owns the wait
    /// (upstream `_prepareRetry`'s abortable sleep plus the TUI's
    /// `RetryStatusIndicator` countdown): the transport only bounds it with
    /// the request's abort signal and maps a cancellation to
    /// [`AiError::RetryCancelled`] (upstream `abortRetry()` ends the retry
    /// without retrying). Without a hook the plain sleep keeps the previous
    /// behaviour.
    ///
    /// Regardless of [`ProviderStreamHooks::on_retry`], the wait first fires
    /// [`ProviderStreamHooks::on_retry_start`] (upstream `auto_retry_start`
    /// / `onRetryScheduled`: emitted before the sleep, with the post-
    /// `Retry-After` delay) and increments `retries_started` (the upstream
    /// `_retryAttempt` counter behind `auto_retry_end`).
    #[allow(clippy::too_many_arguments)]
    async fn wait_backoff(
        &self,
        delay: Duration,
        attempt: u32,
        error_message: &str,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        retries_started: &mut u32,
    ) -> AiResult<()> {
        // Upstream emits the retry ordinal 1-based (`this._retryAttempt++`
        // before `retryDelayMs`/`auto_retry_start`); the transport's
        // `attempt` is the 0-based index of the attempt that just failed.
        let attempt = attempt.saturating_add(1);
        *retries_started = retries_started.saturating_add(1);
        if let Some(hook) = hooks.on_retry_start.as_ref() {
            hook(&AutoRetryStart {
                attempt,
                max_attempts: self.config.max_retries,
                delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                error_message: error_message.to_owned(),
            });
        }
        let Some(hook) = hooks.on_retry.hook() else {
            return self.sleep_backoff(delay, signal).await;
        };
        let notice = RetryNotice {
            attempt,
            max_attempts: self.config.max_retries,
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        };
        let wait = hook(notice);
        let outcome = match signal {
            Some(signal) => tokio::select! {
                outcome = wait => outcome,
                _ = signal.cancelled() => return Err(AiError::Aborted),
            },
            None => wait.await,
        };
        match outcome {
            RetryOutcome::Proceed => Ok(()),
            RetryOutcome::Cancelled => Err(AiError::RetryCancelled {
                attempts: notice.attempt,
            }),
        }
    }

    /// Sleep for the given delay, cut short the moment the abort signal
    /// fires (await-able — no polling).
    async fn sleep_backoff(&self, delay: Duration, signal: Option<&AbortSignal>) -> AiResult<()> {
        match signal {
            Some(signal) => {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => Ok(()),
                    _ = signal.cancelled() => Err(AiError::Aborted),
                }
            }
            None => {
                tokio::time::sleep(delay).await;
                Ok(())
            }
        }
    }
}

/// HTTP statuses that retry per the OpenAI/Anthropic SDK policy. Public for
/// tests/ (upstream `isRetryableProviderError`).
pub fn is_retryable(status: u16) -> bool {
    // Upstream SDK policy (and pir's own doc comment): 408/409/429 and 5xx
    // only. 425 is not retryable.
    matches!(status, 408 | 409 | 429) || (500..600).contains(&status)
}

/// Whether a failure surfaced by the transport seam is a terminal verdict that
/// must never consume the retry budget.
///
/// The built-in [`ReqwestTransport`] only produces [`AiError::Http`] for
/// transport failures, so its errors keep the established retry policy. An
/// injected [`HttpTransport`] (upstream `fetch` override / extension client)
/// can return any [`AiError`]: an abort or an error the implementation already
/// classified as non-retryable is a verdict, and re-sending would sleep through
/// a backoff, duplicate the request, and finally report `RetryExhausted`
/// instead of the real failure.
fn is_terminal_request_error(error: &AiError) -> bool {
    matches!(
        error,
        AiError::Aborted | AiError::RetryCancelled { .. } | AiError::NonRetryable(_)
    )
}

/// Retry decision honoring the provider-supplied `x-should-retry` header
/// (upstream OpenAI/Anthropic SDK policy: an explicit `true`/`false` wins over
/// the status-based default).
fn is_retryable_response(status: u16, headers: &BTreeMap<String, String>) -> bool {
    match headers.get("x-should-retry").map(String::as_str) {
        Some("true") => true,
        Some("false") => false,
        _ => is_retryable(status),
    }
}

/// Render a non-2xx response body for the error message with the upstream
/// truncation contract (error-body.ts:139-142 `truncateErrorText`): bodies up
/// to [`MAX_PROVIDER_ERROR_BODY_CHARS`] pass through unchanged, longer bodies
/// are cut on a char boundary and marked with
/// `... [truncated <n> chars]` (the char count is Rust-`char` based; upstream
/// counts UTF-16 code units, which only differs for astral-plane characters).
/// Public for tests/.
pub fn truncate_provider_error_body(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(MAX_PROVIDER_ERROR_BODY_CHARS).collect();
    let truncated = chars.count();
    if truncated == 0 {
        head
    } else {
        format!("{head}... [truncated {truncated} chars]")
    }
}

/// Server-requested retry delay from the `retry-after-ms` header first, then
/// `retry-after` (upstream provider-retry order). `None` when neither header
/// parses, which callers replace with exponential backoff. Public for tests/.
pub fn server_retry_delay(
    retry_after_ms: Option<&str>,
    retry_after: Option<&str>,
) -> Option<Duration> {
    if let Some(delay) = retry_after_ms.and_then(parse_retry_after_ms) {
        return Some(delay);
    }
    retry_after.and_then(parse_retry_after)
}

/// Seconds as a [`Duration`], saturating at [`Duration::MAX`] instead of
/// panicking.
///
/// `Duration::from_secs_f64` panics when the value overflows the duration
/// range, and the value comes straight from a server-controlled header
/// (`Retry-After: 1e300` is a parseable, finite `f64`). A delay that large is
/// clamped to `Duration::MAX`, which the caller's `max_retry_delay` guard
/// turns into the same "server requested a delay above the cap" failure it
/// produces for any other oversized delay.
fn duration_from_secs(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
}

/// Parse a `Retry-After-Ms` header value (milliseconds, fractional allowed).
fn parse_retry_after_ms(value: &str) -> Option<Duration> {
    let ms = value.trim().parse::<f64>().ok()?;
    if ms.is_finite() && ms >= 0.0 {
        Some(duration_from_secs(ms / 1000.0))
    } else {
        None
    }
}

/// Exponential backoff for attempt n (0-based, upstream provider-retry):
/// `min(0.5 * 2^n, 8s)` shrunk by up to 25% jitter. Public for tests/.
pub fn exponential_backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    let base_ms = 500_u64.saturating_mul(1_u64 << shift).min(8_000);
    Duration::from_millis(apply_backoff_jitter(base_ms))
}

/// Shrink a delay by up to 25% (upstream `* (1 - Math.random() * 0.25)`).
/// pir has no random dependency; the jitter factor is derived from the clock's
/// sub-second nanos.
fn apply_backoff_jitter(ms: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let random = (nanos % 1000) as f64 / 1000.0;
    let factor = 1.0 - random * 0.25;
    (ms as f64 * factor) as u64
}

/// Parse a `Retry-After` header value: delay-seconds (fractional allowed) or
/// HTTP-date. Returns `None` for unparseable values, which callers turn into
/// exponential backoff (upstream provider-retry behavior). Public for tests/.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    // Delay-seconds form (upstream also accepts fractional seconds).
    if let Ok(seconds) = value.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return Some(duration_from_secs(seconds));
        }
        return None;
    }
    // HTTP-date form (upstream: `Date.parse(retryAfter) - Date.now()`).
    let at = parse_http_date(value)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let at = Duration::from_secs(at);
    if at <= now {
        return Some(Duration::ZERO);
    }
    Some(at - now)
}

/// Parse an RFC 1123 HTTP-date (`Wed, 21 Oct 2015 07:28:00 GMT`) into Unix
/// seconds. Also used by the remote-catalog `Last-Modified` gate
/// (`ai::providers::dynamic`, upstream `Date.parse` in milliseconds).
pub(crate) fn parse_http_date(value: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let rest = value.split_once(',')?.1.trim();
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.trim_end_matches(',').parse().ok()?;
    let month = parts.next()?;
    // `get(..3)` instead of `[..3]`: the month token is server-controlled, and
    // byte 3 of a multi-byte string is not necessarily a character boundary
    // (slicing there panics).
    let month_prefix = month.get(..3)?;
    let month_index = MONTHS
        .iter()
        .position(|m| month_prefix.eq_ignore_ascii_case(m))?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    // Field sanity bounds: an out-of-range IMF-fixdate field is invalid input
    // (`Date.parse` yields NaN) and must not reach the calendar arithmetic with
    // values that can overflow it.
    if !(1..=31).contains(&day)
        || !(0..=9999).contains(&year)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    // Days since Unix epoch via the civil-calendar algorithm.
    let days = days_from_civil(year, month_index as i64 + 1, day);
    // Pre-epoch dates are valid HTTP-dates whose `Date.parse(...) - Date.now()`
    // is negative; upstream clamps that delay at zero, so they map to 0 rather
    // than underflowing the unsigned day/second arithmetic.
    let seconds = i128::from(days)
        .checked_mul(86_400)?
        .checked_add(i128::from(hour) * 3_600 + i128::from(minute) * 60 + i128::from(second))?;
    Some(u64::try_from(seconds.max(0)).unwrap_or(u64::MAX))
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
