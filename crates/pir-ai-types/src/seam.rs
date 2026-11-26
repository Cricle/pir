//! The provider HTTP transport seam.
//!
//! Only the *interface* lives here so the value types in this crate can
//! reference it (`StreamOptions::fetch` accepts an `Arc<dyn HttpTransport>`,
//! `ProviderStreamHooks` owns the request/response hooks). The built-in
//! `reqwest` implementation, the retry loop and the streaming body plumbing
//! stay in `pir-core`'s `ai::transport`, which re-exports these items.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::Stream;
use reqwest::header::HeaderMap;
use reqwest::Method;

use crate::error::{AiError, AiResult};
use crate::stream::ProviderStreamHooks;
use crate::BoxFuture;

/// Awaitable sink for streamed response body chunks.
///
/// `request_streaming` historically took a synchronous `FnMut(&[u8])`
/// callback, which cannot pause the body read loop. Providers whose per-chunk
/// work can apply backpressure implement this trait instead, so a full
/// downstream event queue stops reading from the socket instead of buffering.
pub trait ChunkSink: Send {
    /// Deliver one body chunk. The read loop stays paused until the returned
    /// future resolves; an `Err` aborts the request.
    fn send<'a>(&'a mut self, chunk: &'a [u8]) -> BoxFuture<'a, AiResult<()>>;
}

/// Async payload hook: receives the parsed request payload, returns the
/// (possibly replaced) payload.
pub type BeforeProviderRequestHook =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, serde_json::Value> + Send + Sync>;

/// Async observer: receives the final response status and headers.
pub type AfterProviderResponseHook =
    Arc<dyn Fn(u16, BTreeMap<String, String>) -> BoxFuture<'static, ()> + Send + Sync>;

/// One byte block of a streamed response body.
///
/// Boxed so the crate does not need a direct `bytes` dependency: boxing a
/// `Bytes` value is a refcounted-handle move, not a payload copy, so the
/// streaming path keeps the allocation behavior of a direct `bytes_stream()`.
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
/// already validated, body owned by the attempt.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// Request method.
    pub method: Method,
    /// Absolute request URL.
    pub url: String,
    /// Validated request headers.
    pub headers: HeaderMap,
    /// Serialized request body.
    pub body: Vec<u8>,
}

/// One executed response: status, headers, and the body as a stream of
/// [`HttpBodyChunk`]s.
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// Streamed response body.
    pub body: HttpBodyStream,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// The streamed response body.
pub type HttpBodyStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<HttpBodyChunk, AiError>> + Send + 'static>>;

/// The transport execution seam.
///
/// Object safe (`Arc<dyn HttpTransport>`) and `Send + Sync + 'static` so the
/// provider adapters keep spawning their request future unchanged.
pub trait HttpTransport: Send + Sync + 'static {
    /// Execute one request. An `Err` is a pre-body failure (connect error,
    /// TLS, malformed URL) and flows through the retry loop's classification.
    fn execute(&self, request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>>;
}

/// Notify the per-request observer, when installed, that the final
/// non-retried response was received. Awaited so handlers complete before the
/// body stream flows.
pub async fn apply_after_provider_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    hooks: &ProviderStreamHooks,
) {
    if let Some(hook) = hooks.after_provider_response.as_ref() {
        hook(status, headers.clone()).await;
    }
}
