//! Shared HTTP transport for AI providers.
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
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

/// Apply the per-request `before_provider_request` hook to a request value.
///
/// Adapters that already hold the request as a `Value` (the OpenAI body path
/// when a hook is installed) call this before serializing, so the hook never
/// forces a second transcript-sized tree and the transport can be handed a
/// hook-free `ProviderStreamHooks` clone. Without a hook the value is returned
/// unchanged.
pub(crate) async fn apply_before_provider_request_value(
    value: serde_json::Value,
    hooks: &ProviderStreamHooks,
) -> serde_json::Value {
    match hooks.before_provider_request.as_ref() {
        Some(hook) => hook(value).await,
        None => value,
    }
}

/// Apply the per-request `before_provider_request` hook to a serialized JSON
/// body. Without a hook — or when the body is not JSON (never the case for AI
/// provider requests) — the body passes through unchanged.
async fn apply_before_provider_request(body: Vec<u8>, hooks: &ProviderStreamHooks) -> Vec<u8> {
    if hooks.before_provider_request.is_none() {
        return body;
    }
    // The parsed Value owns every string it carries, so the serialized body
    // can be released BEFORE the hook runs: keeping it alive through the hook
    // and the replacement serialization held a transcript-sized `Vec` next to
    // the transcript-sized `Value` and the new `Vec` (three copies of the same
    // request on a resumed session). The replacement is always serialized from
    // the hook's returned Value, exactly like before; only the fallback to the
    // pre-hook bytes on a serializer error is gone — `Value` serialization
    // cannot fail for a JSON tree (the writer is an in-memory Vec).
    let reserved = body.len();
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    drop(body);
    let replaced = apply_before_provider_request_value(value, hooks).await;
    let mut payload = Vec::with_capacity(reserved);
    if serde_json::to_writer(&mut payload, &replaced).is_err() {
        // Unreachable for a `Value` (map keys are strings, the writer is an
        // in-memory Vec); fail closed instead of sending a partial body.
        payload.clear();
    }
    payload
}

/// [`apply_before_provider_request`] over a pooled payload.
///
/// The hook path must hand the bytes to `serde_json`, so the scratch is
/// consumed rather than returned to the pool; the replacement body is
/// unpooled. The overwhelmingly common no-hook path returns the guard
/// untouched and keeps the allocation reusable.
async fn apply_before_provider_request_payload(
    payload: SerializedPayload,
    hooks: &ProviderStreamHooks,
) -> SerializedPayload {
    if hooks.before_provider_request.is_none() {
        return payload;
    }
    SerializedPayload::from_vec(apply_before_provider_request(payload.into_vec(), hooks).await)
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

/// Build a reqwest client for Pi-managed auxiliary HTTP traffic (OAuth token
/// endpoints, remote catalogs, AWS credential endpoints).
///
/// reqwest is built with `default-features = false` (no `system-proxy`), so a
/// bare `Client::builder()` silently ignores env/OS proxies. Every such client
/// resolves its proxy through [`ProviderRequestConfig::env_proxy_url`] and
/// honours the standard `NO_PROXY`/`no_proxy` exclusions. The builder itself
/// leaves redirect handling at reqwest's default; clients that carry provider
/// credentials (the provider transport and OAuth HTTP client) must add
/// `redirect(Policy::none())` themselves so 301/302/307/308 cannot forward
/// `x-api-key`, `api-key`, `x-goog-api-key` or `authorization` to another host.
pub(crate) fn pi_reqwest_client_builder() -> AiResult<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder();
    if let Some(proxy_url) = ProviderRequestConfig::env_proxy_url() {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|error| AiError::Http(format!("invalid proxy URL: {error}")))?
            .no_proxy(reqwest::NoProxy::from_env());
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

/// Read a response body under a hard size cap: an oversized `Content-Length`
/// is rejected before any read, and the streamed read (`Response::chunk`)
/// stops at `limit` even when the header is absent (chunked) or lies, so the
/// whole body can never be buffered before the check. `what` labels the
/// failure for the read error path.
pub(crate) async fn read_bounded_response_body(
    mut response: reqwest::Response,
    limit: usize,
    what: &str,
) -> AiResult<String> {
    if let Some(length) = response.content_length() {
        if length > limit as u64 {
            return Err(AiError::ResponseTooLarge { limit });
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        AiError::Http(format!("reading the {what} response body failed: {error}"))
    })? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(AiError::ResponseTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    // Match `Response::text()`: lossy UTF-8 and a stripped leading BOM.
    let body = String::from_utf8_lossy(&body);
    Ok(body.strip_prefix('\u{feff}').unwrap_or(&body).to_string())
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
        let mut builder = reqwest::Client::builder()
            .connect_timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(proxy_url) = &config.proxy_url {
            let proxy = reqwest::Proxy::all(proxy_url)
                .map_err(|error| AiError::Http(format!("invalid proxy URL: {error}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder.build().map_err(|e| AiError::Http(e.to_string()))?;
        Ok(Self { client })
    }

    /// Build one reqwest request for the retry loop. `reqwest::Body::from(Bytes)`
    /// keeps the serialized payload in one refcounted allocation; the caller's
    /// [`SerializedPayload`] guard remains alive through the request, so a
    /// retry shares that allocation (`Request::try_clone` is a refcount bump,
    /// never a body copy). Header validation matches
    /// [`ProviderRequestClient::build_request`], so both transports fail fast
    /// on the same inputs.
    fn prepare(
        &self,
        method: &Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: tokio_util::bytes::Bytes,
    ) -> AiResult<reqwest::Request> {
        let headers = build_header_map(headers)?;
        self.client
            .request(method.clone(), url)
            .headers(headers)
            .body(body)
            .build()
            .map_err(|error| AiError::Http(error.to_string()))
    }

    /// Build one reqwest request whose body is streamed from `path`.
    ///
    /// The file is opened fresh for every attempt (a streaming body cannot be
    /// shared via `Request::try_clone`). A buffered reader keeps the read
    /// buffer off the transcript-sized heap; the explicit Content-Length
    /// preserves the wire shape of the Vec-backed path (no chunked encoding).
    fn prepare_file(
        &self,
        method: &Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        path: &Path,
        len: u64,
    ) -> AiResult<reqwest::Request> {
        let mut headers = build_header_map(headers)?;
        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from(len),
        );
        let file = std::fs::File::open(path)?;
        let file = tokio::fs::File::from_std(file);
        let stream = futures_util::stream::unfold(file, |mut file| async move {
            use tokio::io::AsyncReadExt as _;
            let mut buffer = vec![0u8; 64 * 1024];
            match file.read(&mut buffer).await {
                Ok(0) => None,
                Ok(read) => {
                    buffer.truncate(read);
                    Some((Ok::<_, std::io::Error>(buffer), file))
                }
                Err(error) => Some((Err(error), file)),
            }
        });
        self.client
            .request(method.clone(), url)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(stream))
            .build()
            .map_err(|error| AiError::Http(error.to_string()))
    }

    /// Send one already prepared reqwest request. Callers replay via
    /// [`reqwest::Request::try_clone`], which keeps the body shared.
    fn execute_prepared(
        &self,
        request: reqwest::Request,
    ) -> crate::agent::tool_execution::BoxFuture<'_, AiResult<HttpResponse>> {
        Box::pin(async move {
            let response = self
                .client
                .execute(request)
                .await
                .map_err(|error| AiError::Http(error.to_string()))?;
            Ok(into_http_response(response))
        })
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
            Ok(into_http_response(response))
        })
    }
}

/// Map one reqwest response head into the transport seam response.
fn into_http_response(response: reqwest::Response) -> HttpResponse {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.bytes_stream().map(|item| {
        item.map_err(|error| AiError::Http(error.to_string()))
            .map(HttpBodyChunk::new)
    });
    HttpResponse {
        status,
        headers,
        body: Box::pin(body),
    }
}

/// Validate and pack caller headers for either transport. An empty value is
/// the upstream `null` suppression marker: the header shadows a default but
/// must not be sent. Invalid names/values fail fast (never retried).
fn build_header_map(headers: &BTreeMap<String, String>) -> AiResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if value.is_empty() {
            continue;
        }
        let key = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| AiError::Http(error.to_string()))?;
        let val = reqwest::header::HeaderValue::from_str(value)
            .map_err(|error| AiError::Http(error.to_string()))?;
        map.insert(key, val);
    }
    Ok(map)
}

/// One prepared request for an attempt.
///
/// With a retry budget, the DEFAULT reqwest transport keeps the serialized
/// body in one reusable `Bytes` allocation owned by the `reqwest::Request`;
/// every attempt replays the request through `try_clone`, which only bumps the
/// body's refcount. The injection seam keeps the public `HttpRequest` shape
/// (owned `Vec` body): only its attempts that still have a retry budget pay a
/// body copy, exactly like before this optimization.
enum PreparedRequest {
    Reqwest(reqwest::Request),
    Seam(HttpRequest),
}

/// Where the serialized request body lives across the retry loop.
enum AttemptSource {
    /// No retry budget: prepare the single attempt lazily from the payload
    /// guard kept alive by `request_streaming_attempts`.
    Fresh,
    /// Retry budget over the injection seam: one `HttpRequest` owns the body.
    Seam(HttpRequest),
    /// Retry budget over the default reqwest transport: one request owns a
    /// reusable `Bytes` body shared by every attempt.
    Replay(reqwest::Request),
    /// A temp-file-serialized body: each attempt opens the file fresh (a
    /// streaming reqwest body cannot be cloned), keeping the body off the heap.
    File { path: PathBuf, len: u64 },
}

/// The attempt executor selected at construction: the default reqwest client
/// (zero-copy request replay) or an injected [`HttpTransport`] (public seam).
#[derive(Clone)]
enum AttemptTransport {
    Reqwest(Arc<ReqwestTransport>),
    Seam(Arc<dyn HttpTransport>),
}

impl AttemptTransport {
    fn prepare(
        &self,
        method: &Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &SerializedPayload,
    ) -> AiResult<PreparedRequest> {
        match self {
            Self::Reqwest(transport) => transport
                .prepare(method, url, headers, body.shared_bytes())
                .map(PreparedRequest::Reqwest),
            // The public injection seam is Vec-shaped (test-only). It copies
            // once into its own `HttpRequest`; the pooled scratch still stays
            // alive and is reused by the next request.
            Self::Seam(_) => {
                ProviderRequestClient::build_request(method, url, headers, body.as_bytes().to_vec())
                    .map(PreparedRequest::Seam)
            }
        }
    }

    /// Prepare one attempt whose body lives in a temp file. The default
    /// transport streams the file; the Vec-shaped injection seam reads it
    /// back (test-only, and the file is bounded by the caller's threshold).
    fn prepare_file(
        &self,
        method: &Method,
        url: &str,
        headers: &BTreeMap<String, String>,
        path: &Path,
        len: u64,
    ) -> AiResult<PreparedRequest> {
        match self {
            Self::Reqwest(transport) => transport
                .prepare_file(method, url, headers, path, len)
                .map(PreparedRequest::Reqwest),
            Self::Seam(_) => {
                let body = std::fs::read(path)?;
                ProviderRequestClient::build_request(method, url, headers, body)
                    .map(PreparedRequest::Seam)
            }
        }
    }

    fn execute(
        &self,
        request: PreparedRequest,
    ) -> crate::agent::tool_execution::BoxFuture<'_, AiResult<HttpResponse>> {
        match (self, request) {
            (Self::Reqwest(transport), PreparedRequest::Reqwest(request)) => {
                transport.execute_prepared(request)
            }
            (Self::Seam(transport), PreparedRequest::Seam(request)) => transport.execute(request),
            // Both halves are produced by the same variant, so this arm is
            // unreachable; fail closed with a typed error instead of panicking.
            _ => Box::pin(async {
                Err(AiError::Http(
                    "internal transport error: request kind does not match transport".to_string(),
                ))
            }),
        }
    }
}

/// One serialized request body stored in a temp file, for bodies too large to
/// keep resident as a `Vec` through the request.
///
/// The file is created exclusively in the platform temp directory, written by
/// [`ProviderRequestClient::serialize_payload_to_file`], streamed by the
/// default reqwest transport (reopened per retry attempt), and removed on
/// drop. The injection seam reads it back into an [`HttpRequest`] because that
/// public seam is Vec-shaped and is test-only.
#[derive(Debug)]
pub struct SerializedPayloadFile {
    path: PathBuf,
    len: u64,
}

impl SerializedPayloadFile {
    /// The temp file holding the serialized body.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The serialized body length in bytes (the request's `Content-Length`).
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the serialized body is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for SerializedPayloadFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Distinguishes concurrent temp bodies in one process.
static PROVIDER_BODY_FILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The request body handed to one streaming attempt loop.
enum StreamRequestBody<'a> {
    /// A serialized body plus the guard that owns its scratch allocation.
    Bytes(SerializedPayload),
    File(&'a SerializedPayloadFile),
}

/// The request-shape half of one streaming call: method, target, caller
/// headers, and the per-request cancellation/hook context.
///
/// Every streaming entry point takes this bundle, so a request's shape is
/// assembled once and threaded through the retry loop and its private helpers
/// without repeating the same five positional values in each signature.
pub struct StreamingRequest<'a> {
    method: Method,
    url: &'a str,
    headers: &'a BTreeMap<String, String>,
    signal: Option<&'a AbortSignal>,
    hooks: &'a ProviderStreamHooks,
}

impl<'a> StreamingRequest<'a> {
    /// Bundle one request's method, URL, headers and per-request signal/hooks.
    pub fn new(
        method: Method,
        url: &'a str,
        headers: &'a BTreeMap<String, String>,
        signal: Option<&'a AbortSignal>,
        hooks: &'a ProviderStreamHooks,
    ) -> Self {
        Self {
            method,
            url,
            headers,
            signal,
            hooks,
        }
    }
}

/// The inputs of one retry decision: the attempt that just failed, its
/// trigger summary, the server-requested delay headers, and the
/// cancellation/hook context (see [`ProviderRequestClient::backoff`]).
struct RetryAttempt<'a> {
    /// 0-based index of the attempt that just failed.
    attempt: u32,
    /// The `auto_retry_start` trigger summary.
    error_message: &'a str,
    /// `retry-after-ms` header value (`None` when absent or unparseable).
    retry_after_ms: Option<&'a str>,
    /// `retry-after` header value (`None` when absent or unparseable).
    retry_after: Option<&'a str>,
    signal: Option<&'a AbortSignal>,
    hooks: &'a ProviderStreamHooks,
}

/// A reusable provider HTTP client with bounded retries and response reads.
#[derive(Clone)]
pub struct ProviderRequestClient {
    /// The attempt executor: the default [`ReqwestTransport`]
    /// ([`Self::new`]), or an arbitrary transport via
    /// [`Self::with_transport`].
    transport: AttemptTransport,
    pub config: ProviderRequestConfig,
    /// Per-client payload diagnostics (see [`Self::payload_diagnostics`]).
    ///
    /// MEMORY SHAPE (pir extension): a small JSON request body is materialized
    /// into a reusable scratch allocation; a transcript-sized one
    /// ([`Self::serialize_payload_to_file`]) is written to a temp file and
    /// streamed instead. The default reqwest transport replays a `Bytes` body
    /// by cloning the `reqwest::Request`, which shares that allocation
    /// (refcount only), so the common case — a successful first attempt under
    /// the default two-retry budget — pays ZERO body copies; a file body
    /// reopens the file per attempt. The injection seam (`with_transport`)
    /// keeps its historical behavior: an attempt that must keep the body for a
    /// possible retry pays one `Vec` copy. The counters make the property
    /// testable.
    payload: Arc<PayloadDiagnostics>,
    /// Free serialization buffers for this provider. Clones share the pool;
    /// a concurrent request checks one buffer out for its whole lifetime, so
    /// two requests can never write into the same mutable buffer. Bounded:
    /// at most [`PAYLOAD_SCRATCH_MAX_BUFFERS`] free buffers and no buffer
    /// larger than [`PAYLOAD_SCRATCH_MAX_RETAINED_BYTES`] is retained.
    payload_scratch: Arc<PayloadScratchPool>,
    /// Per-request total timeout (upstream `StreamOptions.timeoutMs`, set by
    /// the adapter from `options.timeout_ms`). Reset on this client clone, so
    /// the shared client keeps its own `None` default. `None` = bounded only
    /// by the idle timeout.
    request_timeout: Option<Duration>,
}

/// Maximum number of idle serialization buffers kept by one provider client.
/// Two covers the common case (a stream request plus a cache-warmer/compaction
/// request) without pinning memory for a wide fan-out; extra concurrent
/// requests allocate on demand and are released normally afterwards.
const PAYLOAD_SCRATCH_MAX_BUFFERS: usize = 2;

/// Largest idle scratch buffer retained. A larger body is sent and then its
/// allocation is released instead of being pinned for the provider's lifetime;
/// the OpenAI streamed path already switches to a temp file at 8 MiB.
const PAYLOAD_SCRATCH_MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// Staging capacity for the temp-file serializer. File bodies are written
/// incrementally through this buffer instead of `BufWriter`'s per-call 256 KiB
/// allocation, so the file path also reuses one pooled allocation across
/// turns. A write larger than the staging window bypasses it and goes straight
/// to the file; the buffer itself never grows to the body size.
const PAYLOAD_FILE_BUFFER_BYTES: usize = 256 * 1024;

/// Bounded pool of reusable request-body serialization buffers.
///
/// The lock is only held while checking a buffer out or returning it — never
/// across serialization and never across an await. A checked-out buffer is
/// exclusively owned by one [`SerializedPayload`] guard, which is what makes
/// concurrent requests safe: they either get distinct buffers from the pool or
/// allocate a new one.
#[derive(Default)]
struct PayloadScratchPool {
    free: std::sync::Mutex<Vec<tokio_util::bytes::BytesMut>>,
}

impl PayloadScratchPool {
    fn checkout(&self) -> Option<tokio_util::bytes::BytesMut> {
        self.free
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .pop()
    }

    fn release(&self, bytes: tokio_util::bytes::Bytes) {
        let Ok(buffer) = bytes.try_into_mut() else {
            // An HTTP layer still holds a byte handle (or the allocation was
            // not promotable). Drop it instead of copying; the next request
            // simply allocates a fresh scratch.
            return;
        };
        self.release_buffer(buffer);
    }

    fn release_buffer(&self, mut buffer: tokio_util::bytes::BytesMut) {
        buffer.clear();
        if buffer.capacity() == 0 || buffer.capacity() > PAYLOAD_SCRATCH_MAX_RETAINED_BYTES {
            return;
        }
        let mut free = self
            .free
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if free.len() < PAYLOAD_SCRATCH_MAX_BUFFERS {
            free.push(buffer);
        }
    }
}

/// A bounded staging writer over a pooled scratch buffer.
///
/// `serde_json::to_writer` emits many small fragments; buffering them avoids a
/// syscall per fragment while keeping the allocation reusable. When a single
/// fragment is at least the staging window and nothing is buffered, it is
/// written directly to the file so a multi-megabyte string never gets copied
/// through the scratch.
struct ScratchFileWriter<'a> {
    file: &'a mut std::fs::File,
    buffer: &'a mut tokio_util::bytes::BytesMut,
}

impl ScratchFileWriter<'_> {
    fn flush_buffer(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            std::io::Write::write_all(self.file, &self.buffer[..])?;
            self.buffer.clear();
        }
        Ok(())
    }
}

impl std::io::Write for ScratchFileWriter<'_> {
    fn write(&mut self, mut data: &[u8]) -> std::io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            if self.buffer.is_empty() && data.len() >= PAYLOAD_FILE_BUFFER_BYTES {
                std::io::Write::write_all(self.file, data)?;
                break;
            }
            let space = PAYLOAD_FILE_BUFFER_BYTES.saturating_sub(self.buffer.len());
            if space == 0 {
                self.flush_buffer()?;
                continue;
            }
            let take = space.min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == PAYLOAD_FILE_BUFFER_BYTES {
                self.flush_buffer()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_buffer()?;
        self.file.flush()
    }
}

/// One serialized request body, backed by a pooled scratch allocation.
///
/// The guard keeps the buffer checked out for the whole request (including the
/// retry and response-read loops) and returns it to the pool on drop. The
/// public [`Self::as_bytes`] slice is stable while the guard lives; the
/// default reqwest transport clones the inner [`Bytes`] handle (refcount only)
/// rather than the payload bytes.
pub struct SerializedPayload {
    bytes: tokio_util::bytes::Bytes,
    pool: Option<Arc<PayloadScratchPool>>,
}

impl SerializedPayload {
    fn from_vec(bytes: Vec<u8>) -> Self {
        Self {
            bytes: tokio_util::bytes::Bytes::from(bytes),
            pool: None,
        }
    }

    fn from_pooled(bytes: tokio_util::bytes::Bytes, pool: Arc<PayloadScratchPool>) -> Self {
        Self {
            bytes,
            pool: Some(pool),
        }
    }

    /// The serialized bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The serialized body length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the serialized body is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn shared_bytes(&self) -> tokio_util::bytes::Bytes {
        self.bytes.clone()
    }

    /// Consume the guard and return an owned `Vec` without returning the
    /// allocation to the pool (used by the `before_provider_request` hook,
    /// which must hand the bytes to `serde_json` and replace them).
    fn into_vec(mut self) -> Vec<u8> {
        self.pool = None;
        let bytes = std::mem::replace(&mut self.bytes, tokio_util::bytes::Bytes::new());
        match bytes.try_into_mut() {
            Ok(buffer) => buffer.into(),
            Err(bytes) => bytes.to_vec(),
        }
    }
}

impl Drop for SerializedPayload {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.release(std::mem::replace(
                &mut self.bytes,
                tokio_util::bytes::Bytes::new(),
            ));
        }
    }
}

impl std::ops::Deref for SerializedPayload {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for SerializedPayload {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
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
    bytes_copied: std::sync::atomic::AtomicU64,
    body_bytes: std::sync::atomic::AtomicU64,
    body_alloc_bytes: std::sync::atomic::AtomicU64,
    body_reuses: std::sync::atomic::AtomicU64,
}

/// One client's payload-construction counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PayloadDiagnosticsSnapshot {
    /// Request bodies serialized (`serde_json::to_writer` successes).
    pub allocations: u64,
    /// Full-body copies made to keep a body available for a retry. The default
    /// reqwest transport makes none (it replays one shared `Bytes` body); the
    /// injection seam copies only on attempts that still have a retry budget.
    pub copies: u64,
    /// Serialized request-body bytes produced (heap body length, or temp-file
    /// length for the file-backed path).
    pub body_bytes: u64,
    /// New body-buffer capacity allocated by the serialization scratch. A warm
    /// pool makes this flat after the first request instead of growing with
    /// every turn. It counts the final capacity of each growth, not the
    /// intermediate buffers a `Vec`-style doubling frees; it is a stable
    /// allocation-pressure metric, not an allocator byte-exact total.
    pub body_alloc_bytes: u64,
    /// Serializations served by a checked-out scratch buffer that already had
    /// enough capacity: no new body buffer was allocated and none grew.
    pub body_reuses: u64,
}

impl ProviderRequestClient {
    /// Build a client over the default reqwest transport (connect timeout and
    /// optional proxy come from `config`; unchanged pre-seam construction).
    pub fn new(config: ProviderRequestConfig) -> AiResult<Self> {
        let transport = Arc::new(ReqwestTransport::new(&config)?);
        Ok(Self {
            transport: AttemptTransport::Reqwest(transport),
            config,
            payload: Arc::new(PayloadDiagnostics::default()),
            payload_scratch: Arc::new(PayloadScratchPool::default()),
            request_timeout: None,
        })
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
            transport: AttemptTransport::Seam(transport),
            config,
            payload: Arc::new(PayloadDiagnostics::default()),
            payload_scratch: Arc::new(PayloadScratchPool::default()),
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
        self.transport = AttemptTransport::Seam(transport);
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
            body_bytes: self
                .payload
                .body_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            body_alloc_bytes: self
                .payload
                .body_alloc_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            body_reuses: self
                .payload
                .body_reuses
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Total serialized request-body bytes produced.
    pub fn payload_body_bytes(&self) -> u64 {
        self.payload
            .body_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total new body-buffer capacity allocated by serialization scratches.
    pub fn payload_body_alloc_bytes(&self) -> u64 {
        self.payload
            .body_alloc_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of serializations served by an already-sized scratch buffer.
    pub fn payload_body_reuses(&self) -> u64 {
        self.payload
            .body_reuses
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total request-body bytes copied to keep a request replayable (a
    /// diagnostic; zero for every request over the default reqwest transport).
    pub fn payload_bytes_copied(&self) -> u64 {
        self.payload
            .bytes_copied
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reset the payload counters (test seam).
    pub fn reset_payload_diagnostics(&self) {
        self.payload
            .allocations
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .copies
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .bytes_copied
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .body_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .body_alloc_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .body_reuses
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Serialize one JSON request body into a pooled scratch buffer.
    fn serialize_payload<T: Serialize>(&self, body: &T) -> AiResult<SerializedPayload> {
        self.serialize_payload_with_capacity(body, 0)
    }

    /// [`Self::serialize_payload`] with a caller-supplied capacity hint.
    ///
    /// A transcript-sized provider body (tens of MiB) otherwise grows its
    /// output buffer through several reallocations; the last one briefly holds
    /// both the old and the new buffer. Callers that can estimate the wire
    /// size (the OpenAI streamed body) reserve it once. The buffer comes from
    /// a bounded per-provider pool and is returned only when the
    /// [`SerializedPayload`] guard drops (after the request), so a warm pool
    /// turns every later turn into a single `serde_json::to_writer` over the
    /// same allocation instead of a fresh `Vec`.
    pub(crate) fn serialize_payload_with_capacity<T: Serialize>(
        &self,
        body: &T,
        capacity: usize,
    ) -> AiResult<SerializedPayload> {
        // Check out a buffer, or allocate one at the caller's estimate. A
        // fresh estimate-sized buffer avoids the log2 growth sequence on the
        // first turn; later turns reuse the retained capacity.
        let (mut payload, reused) = match self.payload_scratch.checkout() {
            Some(buffer) => (buffer, true),
            None => (tokio_util::bytes::BytesMut::with_capacity(capacity), false),
        };
        let before_capacity = if reused { payload.capacity() } else { 0 };
        payload.clear();
        if capacity > payload.capacity() {
            payload.reserve(capacity.saturating_sub(payload.len()));
        }
        {
            let mut writer = tokio_util::bytes::BufMut::writer(&mut payload);
            serde_json::to_writer(&mut writer, body)?;
        }
        let body_len = payload.len();
        let final_capacity = payload.capacity();
        if final_capacity > before_capacity {
            // `final_capacity` is the size of the final growth step. The
            // intermediate log2 buffers a fresh Vec frees on the way are
            // intentionally not counted: this metric answers "did the scratch
            // grow this request?", and must stay flat across warm turns.
            self.payload
                .body_alloc_bytes
                .fetch_add(final_capacity as u64, std::sync::atomic::Ordering::Relaxed);
        } else if reused {
            self.payload
                .body_reuses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.payload
            .allocations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.payload
            .body_bytes
            .fetch_add(body_len as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(SerializedPayload::from_pooled(
            payload.freeze(),
            Arc::clone(&self.payload_scratch),
        ))
    }

    /// Serialize one JSON request body directly into a temp file.
    ///
    /// Used by the OpenAI streamed body when the estimated wire size is large
    /// enough that holding the serialized bytes would dominate the peak: the
    /// body is written with a bounded buffer, and the default transport
    /// streams the file into reqwest (reopening it for each retry attempt)
    /// instead of keeping a transcript-sized `Vec` alive. The error type is
    /// the same as [`Self::serialize_payload_with_capacity`]; a serialization
    /// failure removes the partially written file through the guard's drop.
    pub fn serialize_payload_to_file<T: Serialize>(
        &self,
        body: &T,
    ) -> AiResult<SerializedPayloadFile> {
        // The file path streams through the same bounded scratch pool as the
        // in-memory path. The buffer only holds one 256 KiB staging window;
        // `ScratchFileWriter` bypasses it for larger fragments, so a
        // transcript-sized body never becomes a heap body.
        let mut scratch = self.payload_scratch.checkout().unwrap_or_default();
        let reused = scratch.capacity() > 0;
        let before_capacity = scratch.capacity();
        scratch.clear();
        if scratch.capacity() < PAYLOAD_FILE_BUFFER_BYTES {
            scratch.reserve(PAYLOAD_FILE_BUFFER_BYTES);
        }
        let result = (|| -> AiResult<SerializedPayloadFile> {
            let mut last_error = None;
            for _ in 0..16 {
                let nonce =
                    PROVIDER_BODY_FILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "pir-provider-body-{}-{nonce}.json",
                    std::process::id()
                ));
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        let written = (|| -> AiResult<u64> {
                            {
                                let mut writer = ScratchFileWriter {
                                    file: &mut file,
                                    buffer: &mut scratch,
                                };
                                serde_json::to_writer(&mut writer, body)?;
                                writer.flush()?;
                            }
                            Ok(file.metadata()?.len())
                        })();
                        match written {
                            Ok(len) => {
                                self.payload
                                    .allocations
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                self.payload
                                    .body_bytes
                                    .fetch_add(len, std::sync::atomic::Ordering::Relaxed);
                                return Ok(SerializedPayloadFile { path, len });
                            }
                            Err(error) => {
                                // The success guard does not exist yet: remove
                                // the partial body before reporting the
                                // failure.
                                let _ = std::fs::remove_file(&path);
                                return Err(error);
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(last_error
                .unwrap_or_else(|| std::io::Error::other("cannot create provider body temp file"))
                .into())
        })();
        let final_capacity = scratch.capacity();
        if final_capacity > before_capacity {
            self.payload
                .body_alloc_bytes
                .fetch_add(final_capacity as u64, std::sync::atomic::Ordering::Relaxed);
        } else if reused {
            self.payload
                .body_reuses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.payload_scratch.release_buffer(scratch);
        result
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
    pub async fn request_streaming(
        &self,
        request: StreamingRequest<'_>,
        body: Vec<u8>,
        on_chunk: &mut (dyn FnMut(&[u8]) -> AiResult<()> + Send),
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let mut sink = SyncFnSink(on_chunk);
        self.request_streaming_with(request, body, &mut sink).await
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
    pub async fn request_streaming_with<C: ChunkSink>(
        &self,
        request: StreamingRequest<'_>,
        body: Vec<u8>,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        self.request_payload_streaming_with(request, SerializedPayload::from_vec(body), on_chunk)
            .await
    }

    /// [`request_streaming_with`](Self::request_streaming_with) over an
    /// already-serialized pooled payload (the adapters' JSON path).
    async fn request_payload_streaming_with<C: ChunkSink>(
        &self,
        request: StreamingRequest<'_>,
        body: SerializedPayload,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        self.request_streaming_with_no_retry_statuses(
            request,
            StreamRequestBody::Bytes(body),
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
    async fn request_streaming_with_no_retry_statuses<C: ChunkSink>(
        &self,
        request: StreamingRequest<'_>,
        body: StreamRequestBody<'_>,
        no_retry_statuses: &[u16],
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        // The retry loop is unchanged; this wrapper only adds the
        // `auto_retry_end` observation once the final attempt resolved.
        // `retries_started` is the upstream `_retryAttempt` counter (number of
        // scheduled retries), incremented exactly when a retry wait starts.
        let hooks = request.hooks;
        let mut retries_started: u32 = 0;
        let outcome = self
            .request_streaming_attempts(
                request,
                body,
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
    async fn request_streaming_attempts<C: ChunkSink>(
        &self,
        request: StreamingRequest<'_>,
        body: StreamRequestBody<'_>,
        no_retry_statuses: &[u16],
        on_chunk: &mut C,
        retries_started: &mut u32,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let StreamingRequest {
            method,
            url,
            headers,
            signal,
            hooks,
        } = request;
        // `before_provider_request` hook: transform the payload once, before
        // the retry loop (retries reuse the transformed body). A file body is
        // read back only on this hook path: the hook's contract is a parsed
        // `Value`, so a body it must inspect cannot stay streaming. The
        // OpenAI streamed caller uses the file entry only when no such hook is
        // installed, keeping the no-hook path allocation-bounded.
        let body = match body {
            StreamRequestBody::Bytes(payload) => StreamRequestBody::Bytes(
                apply_before_provider_request_payload(payload, hooks).await,
            ),
            StreamRequestBody::File(file) if hooks.before_provider_request.is_some() => {
                let bytes = std::fs::read(file.path())?;
                StreamRequestBody::Bytes(SerializedPayload::from_vec(
                    apply_before_provider_request(bytes, hooks).await,
                ))
            }
            file => file,
        };
        // Retry budget for this request: the Codex endpoint defaults to zero
        // retries when neither the client construction nor the stream options
        // set an explicit count (see [`ProviderRequestClient::effective_max_retries`]).
        let max_retries = self.effective_max_retries(url);
        // The serialized body guard lives in `payload` for the WHOLE loop: it
        // keeps the scratch allocation checked out of the pool while any
        // attempt (or the response read) is alive, then returns it on drop.
        // With NO retry budget the single attempt is prepared lazily from the
        // guard. With a retry budget the default reqwest transport prepares
        // one request whose reusable `Bytes` body every attempt shares via
        // `Request::try_clone` (refcount only). The injection seam keeps the
        // public `HttpRequest` shape, so only ITS attempts that must keep the
        // body for a possible retry pay one `Vec` copy (the last attempt
        // consumes the original). A file body reopens the file per attempt.
        let (payload, file_source) = match body {
            StreamRequestBody::Bytes(payload) => (Some(payload), None),
            StreamRequestBody::File(file) => (None, Some((file.path().to_path_buf(), file.len()))),
        };
        let mut source = match file_source {
            Some((path, len)) => Some(AttemptSource::File { path, len }),
            None if max_retries == 0 => Some(AttemptSource::Fresh),
            None => {
                let payload = payload.as_ref().ok_or_else(|| {
                    AiError::Http("internal request body state: missing payload".to_string())
                })?;
                Some(
                    match self.transport.prepare(&method, url, headers, payload)? {
                        PreparedRequest::Reqwest(request) => AttemptSource::Replay(request),
                        PreparedRequest::Seam(request) => AttemptSource::Seam(request),
                    },
                )
            }
        };
        let mut attempt = 0;
        'attempt: loop {
            if let Some(signal) = signal {
                if signal.is_aborted() {
                    return Err(AiError::Aborted);
                }
            }
            let request = match source.take() {
                Some(AttemptSource::Fresh) => {
                    let payload = payload.as_ref().ok_or_else(|| {
                        AiError::Http("internal request body state: missing payload".to_string())
                    })?;
                    self.transport.prepare(&method, url, headers, payload)?
                }
                Some(AttemptSource::Seam(request)) if attempt < max_retries => {
                    self.payload
                        .copies
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.payload.bytes_copied.fetch_add(
                        request.body.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    let attempted = request.clone();
                    source = Some(AttemptSource::Seam(request));
                    PreparedRequest::Seam(attempted)
                }
                Some(AttemptSource::Seam(request)) => PreparedRequest::Seam(request),
                Some(AttemptSource::Replay(request)) => {
                    // Shares the request body's `Bytes` allocation; this is a
                    // refcount bump, not a payload copy.
                    let attempted = request.try_clone().ok_or_else(|| {
                        AiError::Http(
                            "request body cannot be replayed for a retry (non-reusable body)"
                                .to_string(),
                        )
                    })?;
                    source = Some(AttemptSource::Replay(request));
                    PreparedRequest::Reqwest(attempted)
                }
                Some(AttemptSource::File { path, len }) => {
                    // A streaming body cannot be cloned: reopen the file for
                    // this attempt and keep the source for a possible retry.
                    source = Some(AttemptSource::File {
                        path: path.clone(),
                        len,
                    });
                    self.transport
                        .prepare_file(&method, url, headers, &path, len)?
                }
                None => {
                    return Err(AiError::Http(
                        "internal transport error: request body was already consumed".to_string(),
                    ))
                }
            };
            let deadline = self
                .request_timeout
                // A caller-supplied `timeoutMs` this large would overflow the
                // instant; degrade to "no total deadline" (the idle timeout
                // still bounds the wait) instead of panicking.
                .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout));
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
                            RetryAttempt {
                                attempt,
                                error_message: &error.to_string(),
                                retry_after_ms: None,
                                retry_after: None,
                                signal,
                                hooks,
                            },
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
                    RetryAttempt {
                        attempt,
                        // The retry path does not drain the error body; the
                        // status is the cheap, deterministic trigger summary
                        // (upstream surfaces the SDK's status-bearing message).
                        error_message: &format!("HTTP {status}"),
                        retry_after_ms: retry_after_ms.as_deref(),
                        retry_after: retry_after.as_deref(),
                        signal,
                        hooks,
                    },
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
                                    RetryAttempt {
                                        attempt,
                                        error_message: &error.to_string(),
                                        retry_after_ms: retry_after_ms.as_deref(),
                                        retry_after: retry_after.as_deref(),
                                        signal,
                                        hooks,
                                    },
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
                                RetryAttempt {
                                    attempt,
                                    error_message: &error.to_string(),
                                    retry_after_ms: retry_after_ms.as_deref(),
                                    retry_after: retry_after.as_deref(),
                                    signal,
                                    hooks,
                                },
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
        self.request_payload_streaming_with(
            StreamingRequest::new(Method::POST, url, headers, signal, hooks),
            payload,
            on_chunk,
        )
        .await
    }

    /// [`post_json_streaming_with`](Self::post_json_streaming_with) with an
    /// already-serialized `Vec` body.
    ///
    /// Kept for callers that built the bytes themselves; those bytes are not
    /// returned to the provider scratch pool (the caller owns the `Vec`).
    pub async fn post_payload_streaming_with<C: ChunkSink>(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        payload: Vec<u8>,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        self.request_payload_streaming_with(
            StreamingRequest::new(Method::POST, url, headers, signal, hooks),
            SerializedPayload::from_vec(payload),
            on_chunk,
        )
        .await
    }

    /// [`post_payload_streaming_with`](Self::post_payload_streaming_with)
    /// over a pooled payload. Used by the OpenAI streamed body so it can
    /// reserve the exact output size once instead of growing a buffer through
    /// several reallocations. The payload carries its scratch guard: it stays
    /// checked out until this request's response is fully consumed.
    pub(crate) async fn post_serialized_payload_streaming_with<C: ChunkSink>(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        payload: SerializedPayload,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        self.request_payload_streaming_with(
            StreamingRequest::new(Method::POST, url, headers, signal, hooks),
            payload,
            on_chunk,
        )
        .await
    }

    /// [`post_payload_streaming_with`](Self::post_payload_streaming_with)
    /// with the body in a temp file. The default transport streams the file
    /// (reopening it for each retry attempt); the injection seam reads it into
    /// its Vec-shaped [`HttpRequest`]. `before_provider_request` still sees the
    /// complete parsed body: this entry is only used when that hook is absent,
    /// and falls back to the bytes path if a caller passes one.
    pub async fn post_payload_file_streaming_with<C: ChunkSink>(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        payload: &SerializedPayloadFile,
        signal: Option<&AbortSignal>,
        hooks: &ProviderStreamHooks,
        on_chunk: &mut C,
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        self.request_streaming_with_no_retry_statuses(
            StreamingRequest {
                method: Method::POST,
                url,
                headers,
                signal,
                hooks,
            },
            StreamRequestBody::File(payload),
            &[],
            on_chunk,
        )
        .await
    }
    /// [`post_json_streaming`](Self::post_json_streaming) with per-request
    /// `noRetryStatuses` (upstream `provider-retry.ts:121`): a listed status
    /// fails the request immediately, without consuming the retry budget or
    /// sleeping through a backoff, while every other status keeps the unchanged
    /// retry policy. An empty list is exactly [`Self::post_json_streaming`].
    pub async fn post_json_streaming_with_no_retry_statuses<T: Serialize>(
        &self,
        request: StreamingRequest<'_>,
        body: &T,
        no_retry_statuses: &[u16],
        on_chunk: &mut (dyn FnMut(&[u8]) -> AiResult<()> + Send),
    ) -> AiResult<(u16, BTreeMap<String, String>)> {
        let payload = self.serialize_payload(body)?;
        let mut sink = SyncFnSink(on_chunk);
        self.request_streaming_with_no_retry_statuses(
            request,
            StreamRequestBody::Bytes(payload),
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
        self.request_payload_streaming_with(
            StreamingRequest::new(Method::POST, url, headers, signal, hooks),
            payload,
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
        Ok(HttpRequest {
            method: method.clone(),
            url: url.to_string(),
            headers: build_header_map(headers)?,
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
        request: PreparedRequest,
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
    async fn backoff(&self, retry: RetryAttempt<'_>, retries_started: &mut u32) -> AiResult<()> {
        // Upstream provider-retry: `retry-after-ms` wins over `retry-after`,
        // and a server-requested delay above the configured cap fails the
        // request immediately instead of silently clamping it.
        if let Some(delay) = server_retry_delay(retry.retry_after_ms, retry.retry_after) {
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
                    retry.attempt,
                    retry.error_message,
                    retry.signal,
                    retry.hooks,
                    retries_started,
                )
                .await;
        }
        self.wait_backoff(
            exponential_backoff_delay(retry.attempt),
            retry.attempt,
            retry.error_message,
            retry.signal,
            retry.hooks,
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
