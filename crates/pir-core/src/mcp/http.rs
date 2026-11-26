//! The streamable HTTP transport on top of the official `rmcp` SDK (upstream
//! `packages/mcp/src/transports/streamable-http.ts`).
//!
//! rmcp's streamable HTTP client owns the POST/SSE plumbing, session-id
//! capture and replay, protocol-version headers, session-expiry recovery, and
//! SSE event parsing (upstream's `checkResponse`/`captureSession`/
//! `consumeSseStream` all live inside rmcp now). This module keeps pir's
//! transport-error classification for the connection manager's retry policy,
//! builds the rmcp transport from a pir server config, and supplies
//! [`BoundedHttpClient`]: rmcp 3.5's reqwest backend buffers a plain-JSON
//! response with `response.bytes()` (unbounded), while pir's backend enforces
//! [`super::transport::DEFAULT_MAX_MESSAGE_BYTES`] on `Content-Length` and on
//! the streamed body, and decodes SSE events under the same bound:
//!
//! - `connection reset`-class failures are the only transient ones and get
//!   retried with backoff (upstream `isTransientStatus` covers 408/429/5xx);
//! - connect failures (DNS, refused, unreachable) and timeouts carry their
//!   cause and are not retried;
//! - a non-2xx response whose body is not a JSON-RPC error surfaces as a
//!   typed [`McpError::Http`] with the status and body snippet.
//!
//! Documented deviation from upstream: the server-to-client GET SSE stream
//! is owned by rmcp; servers that do not implement it (405) are handled by
//! rmcp itself.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::stream::{BoxStream, Stream, StreamExt};
use reqwest::header::{HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::ClientInitializeError;
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};

use super::error::McpError;
use super::stdio::FrameCapState;
use super::transport::DEFAULT_MAX_MESSAGE_BYTES;

/// Statuses worth treating as transient (upstream `isTransientStatus`).
pub fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 429 || (status >= 500 && status != 501)
}

/// Marker prefix written into `McpError::Transport` messages for
/// reset-class failures; only those are retried by the connection manager
/// (`is_transient_transport_message`).
const TRANSPORT_RESET_PREFIX: &str = "connection reset";

/// Whether the flattened cause chain describes a failure to establish the
/// connection (DNS, refused, unreachable) rather than a mid-flight reset.
fn is_connect_cause(chain: &str) -> bool {
    let lowered = chain.to_lowercase();
    lowered.contains("os error 10061")          // WSAECONNREFUSED (Windows)
        || lowered.contains("os error 10065")   // WSAEHOSTUNREACH
        || lowered.contains("os error 10051")   // WSAENETUNREACH
        || lowered.contains("os error 11001")   // WSAHOST_NOT_FOUND
        || lowered.contains("os error 111")     // ECONNREFUSED (Unix)
        || lowered.contains("os error 113")     // EHOSTUNREACH
        || lowered.contains("connection refused")
        || lowered.contains("dns error")
        || lowered.contains("failed to lookup")
        || lowered.contains("name or service not known")
        || lowered.contains("temporary failure in name resolution")
        || lowered.contains("nodename nor servname")
}

/// The flattened `Display` chain of an error (a top-level message alone
/// hides the OS cause).
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = error.to_string();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = current {
        if let Some(source) = error.source() {
            chain.push_str("; ");
            chain.push_str(&source.to_string());
            current = Some(source);
        } else {
            current = None;
        }
    }
    chain
}

/// Flatten a rmcp dynamic transport error (the transport name plus the full
/// cause chain of the wrapped transport error).
fn flatten_dynamic(error: &rmcp::transport::DynamicTransportError) -> String {
    error_chain(error.error.as_ref())
}

/// Whether a `McpError::Transport` message describes a transient
/// (retry-worthy) failure.
pub fn is_transient_transport_message(message: &str) -> bool {
    message.starts_with(TRANSPORT_RESET_PREFIX)
}

/// Pull a `HTTP <status>: <body>` failure out of a flattened chain (rmcp
/// reports non-2xx POST responses this way when the body is not a JSON-RPC
/// error).
fn parse_http_failure(chain: &str) -> Option<McpError> {
    let position = chain.find("HTTP ")?;
    let rest = &chain[position + 5..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() != 3 {
        return None;
    }
    let status: u16 = digits.parse().ok()?;
    let body = rest
        .split_once(": ")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    // Bound the carried snippet (upstream `checkResponse` trimmed too).
    let body = body.chars().take(8 * 1024).collect();
    Some(McpError::Http { status, body })
}

/// Map a flattened transport failure chain onto a typed error that carries
/// the cause (upstream surfaces the underlying error instead of collapsing
/// every failure into "connection closed"). `connection reset`-class
/// failures are the only transient ones: DNS/connect/timeout failures do not
/// become faster by retrying.
///
/// rmcp's `StreamableHttpError::Client` variant does not link the wrapped
/// reqwest error as `#[source]`, so the flattened chain alone cannot
/// distinguish a connect-phase failure from a mid-flight one. The typed
/// dynamic error is inspected first (upstream classifies on the error kind,
/// not the message) and the chain-based fallback stays for non-HTTP
/// transports.
fn transport_error_from_dynamic(error: &rmcp::transport::DynamicTransportError) -> McpError {
    let chain = flatten_dynamic(error);
    if let Some(StreamableHttpError::Client(reqwest_error)) = error
        .error
        .as_ref()
        .downcast_ref::<StreamableHttpError<reqwest::Error>>(
    ) {
        if reqwest_error.is_timeout() {
            return McpError::Transport(format!("request timed out: {chain}"));
        }
        if reqwest_error.is_connect() {
            // DNS resolution failure, connection refused, unreachable host.
            return McpError::Transport(format!("connection failed: {chain}"));
        }
    }
    transport_error_from_chain(chain)
}

fn transport_error_from_chain(chain: String) -> McpError {
    let lowered = chain.to_lowercase();
    if lowered.contains("timed out") || lowered.contains("timeout") {
        McpError::Transport(format!("request timed out: {chain}"))
    } else if is_connect_cause(&chain) {
        // DNS resolution failure, connection refused, unreachable host.
        McpError::Transport(format!("connection failed: {chain}"))
    } else if lowered.contains("authorization required") {
        // rmcp's `StreamableHttpError::AuthRequired` (401 with a
        // `WWW-Authenticate` challenge; upstream `McpAuthRequiredError`).
        McpError::Http {
            status: 401,
            body: chain,
        }
    } else if lowered.contains("insufficient scope") {
        // rmcp's `StreamableHttpError::InsufficientScope` (403 asking for
        // more scope; upstream `needsAuthorization`'s step-up case).
        McpError::Http {
            status: 403,
            body: chain,
        }
    } else if let Some(error) = parse_http_failure(&chain) {
        error
    } else {
        // Mid-flight failures: connection reset, closed before message
        // completed, body/decode errors.
        McpError::Transport(format!("{TRANSPORT_RESET_PREFIX}: {chain}"))
    }
}

/// Map a rmcp JSON-RPC error payload onto the typed server error.
pub(crate) fn server_error(error: rmcp::model::ErrorData) -> McpError {
    McpError::Server {
        code: error.code.0 as i64,
        message: error.message.to_string(),
        data: error.data,
    }
}

/// Map a failed client initialization (the rmcp `initialize` handshake) onto
/// pir's typed errors.
pub fn map_initialize_error(error: ClientInitializeError) -> McpError {
    match error {
        ClientInitializeError::JsonRpcError(error) => server_error(error),
        ClientInitializeError::TransportError { error, .. } => {
            transport_error_from_dynamic(&error)
        }
        ClientInitializeError::ConnectionClosed(_) => McpError::ConnectionClosed,
        ClientInitializeError::NoCompatibleProtocolVersion {
            client_supported,
            server_supported,
        } => McpError::Protocol(format!(
            "no compatible MCP protocol version (client: {client_supported:?}, server: {server_supported:?})"
        )),
        other => McpError::Protocol(other.to_string()),
    }
}

/// Map a rmcp service error (a mid-session request failure) onto pir's
/// typed errors.
pub(crate) fn map_service_error(error: rmcp::ServiceError) -> McpError {
    match error {
        rmcp::ServiceError::McpError(error) => server_error(error),
        rmcp::ServiceError::TransportClosed => McpError::ConnectionClosed,
        rmcp::ServiceError::TransportSend(error) => transport_error_from_dynamic(&error),
        rmcp::ServiceError::Timeout { timeout } => McpError::Timeout {
            timeout_ms: timeout.as_millis() as u64,
        },
        other => McpError::Protocol(other.to_string()),
    }
}

/// Build the rmcp streamable HTTP transport config for `url` with resolved
/// (plain) custom headers (upstream `createDefaultTransport`'s HTTP branch).
///
/// Header types come from pir's `reqwest` re-export of the `http` crate,
/// which resolves to the same single `http` 1.x build rmcp's transport uses,
/// so the maps are type-compatible. The transport itself is constructed by
/// the caller via `StreamableHttpClientTransport::from_config` (its reqwest
/// client type is rmcp-internal and not nameable here).
pub fn streamable_http_config(
    url: String,
    headers: Vec<(String, String)>,
) -> Result<StreamableHttpClientTransportConfig, McpError> {
    streamable_http_config_with_auth(url, headers, None)
}

/// [`streamable_http_config`] with an OAuth access token. rmcp sends it as
/// `Authorization: Bearer <token>` on every request (upstream attaches the
/// same header; `auth_header` takes the token without the `Bearer ` prefix).
pub fn streamable_http_config_with_auth(
    url: String,
    headers: Vec<(String, String)>,
    auth_token: Option<String>,
) -> Result<StreamableHttpClientTransportConfig, McpError> {
    let mut custom_headers = HashMap::new();
    for (name, value) in headers {
        // Invalid names/values are skipped, matching the previous transport's
        // behavior; rmcp additionally rejects its reserved protocol headers
        // (Accept, Mcp-Session-Id, Last-Event-Id) with a per-request error.
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            custom_headers.insert(name, value);
        }
    }
    // rmcp defaults: stateless servers tolerated, transparent re-handshake
    // on session expiry, no connection pooling stalls. The SSE event cap is
    // set to the same 1 MiB protocol-frame bound the stdio reader enforces
    // (`super::transport::DEFAULT_MAX_MESSAGE_BYTES`); [`BoundedHttpClient`]
    // applies the same cap to plain-JSON bodies (declared length and stream).
    let config = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(custom_headers)
        .session_recovery_timeout(Duration::from_secs(10))
        .max_sse_event_size(super::transport::DEFAULT_MAX_MESSAGE_BYTES);
    Ok(match auth_token.filter(|token| !token.is_empty()) {
        Some(token) => config.auth_header(token),
        None => config,
    })
}

/// The MIME type of an SSE response.
const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
/// The MIME type of a plain-JSON response.
const JSON_MIME_TYPE: &str = "application/json";
/// Upper bound on the body preview carried in a parse error.
const JSON_BODY_PREVIEW_BYTES: usize = 256;

/// A reqwest client for rmcp's streamable HTTP transport that bounds every
/// plain-`application/json` response body.
///
/// rmcp 3.5's reqwest backend reads a non-SSE response with
/// `response.bytes()`, which buffers the whole body regardless of size; a
/// malicious or broken server could answer a request with an arbitrarily
/// large JSON document and exhaust the host. This wrapper implements
/// [`StreamableHttpClient`] by delegating streams and session deletes to the
/// inner reqwest client, while its POST path:
///
/// - rejects a declared `Content-Length` above the cap before reading;
/// - reads chunk-by-chunk and stops at the cap (so a chunked body is bounded
///   too), recording the violation in the shared [`FrameCapState`] so the
///   client request path reports [`McpError::MessageTooLarge`];
/// - parses an SSE response through a bounded event decoder (the SSE path is
///   already incremental; the cap bounds one event, as rmcp's configured
///   `max_sse_event_size` does).
#[derive(Clone)]
pub struct BoundedHttpClient {
    inner: reqwest::Client,
    cap: Arc<FrameCapState>,
}

/// Build the bounded HTTP client used by [`super::client::McpClient::connect_http`]
/// (rmcp's default client policy: no idle pooling stalls, no redirects that
/// could replay custom headers).
pub fn bounded_http_client(cap: Arc<FrameCapState>) -> Result<BoundedHttpClient, McpError> {
    let inner = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            McpError::Protocol(format!("cannot build the MCP HTTP client: {error}"))
        })?;
    Ok(BoundedHttpClient { inner, cap })
}

#[doc(hidden)]
/// Test seam: a bounded client with an explicit body cap plus its overflow
/// state. Production builds the pair through [`bounded_http_client`].
pub fn bounded_http_client_for_tests(limit: usize) -> (BoundedHttpClient, Arc<FrameCapState>) {
    let cap = FrameCapState::new(limit);
    (
        BoundedHttpClient {
            inner: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            cap: Arc::clone(&cap),
        },
        cap,
    )
}

/// Reject custom headers rmcp reserves for its own protocol handling.
fn apply_custom_headers(
    mut builder: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, StreamableHttpError<reqwest::Error>> {
    for (name, value) in custom_headers {
        if ["accept", "mcp-session-id", "last-event-id"]
            .iter()
            .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
        {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

/// Extract a `scope=` parameter from a `WWW-Authenticate` value (rmcp's
/// helper is crate-private; the same unquoted and quoted forms are accepted).
fn extract_scope(header: &str) -> Option<String> {
    let lowered = header.to_ascii_lowercase();
    let start = lowered.find("scope=")? + "scope=".len();
    let rest = &header[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some(quoted[..end].to_owned());
    }
    let end = rest
        .find(|character: char| character == ',' || character.is_ascii_whitespace())
        .unwrap_or(rest.len());
    (!rest[..end].is_empty()).then(|| rest[..end].to_owned())
}

/// Read a response body under `limit` bytes. `Content-Length` is checked
/// before reading; the streaming check covers chunked bodies and servers that
/// declare a smaller length than they send.
async fn read_body_bounded(
    mut response: reqwest::Response,
    cap: &FrameCapState,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    let limit = cap.limit();
    if let Some(length) = response.content_length() {
        if length > limit as u64 {
            cap.mark_overflowed();
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("MCP HTTP response Content-Length {length} exceeds the {limit}-byte cap"),
            )));
        }
    }
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > limit {
                    cap.mark_overflowed();
                    return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                        format!("MCP HTTP response body exceeds the {limit}-byte cap"),
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(error) => return Err(StreamableHttpError::Client(error)),
        }
    }
}

/// One bounded response read for the non-success branch.
async fn read_error_body(
    response: reqwest::Response,
    cap: &FrameCapState,
) -> Result<String, StreamableHttpError<reqwest::Error>> {
    let body = read_body_bounded(response, cap).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Short lossy preview of a body that failed to parse as JSON-RPC (mirrors
/// rmcp's diagnostic without carrying the whole body).
fn json_rpc_body_preview(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if text.len() <= JSON_BODY_PREVIEW_BYTES {
        return text.into_owned();
    }
    let mut end = JSON_BODY_PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn parse_server_json_rpc(body: &[u8]) -> Result<ServerJsonRpcMessage, String> {
    serde_json::from_slice::<ServerJsonRpcMessage>(body).map_err(|error| {
        format!(
            "could not parse JSON response as ServerJsonRpcMessage: {error}: {}",
            json_rpc_body_preview(body)
        )
    })
}

impl BoundedHttpClient {
    async fn post_bounded(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
        let mut request = self
            .inner
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(auth_header) = auth_token {
            request = request.bearer_auth(auth_header);
        }
        request = apply_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id.as_ref() {
            request = request.header("Mcp-Session-Id", session_id.as_ref());
        }
        let response = request.json(&message).send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(header) = response.headers().get(WWW_AUTHENTICATE) {
                let header = header
                    .to_str()
                    .map_err(|_| {
                        StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                            "invalid www-authenticate header value".to_owned(),
                        ))
                    })?
                    .to_owned();
                return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                    header,
                )));
            }
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(header) = response.headers().get(WWW_AUTHENTICATE) {
                let header = header
                    .to_str()
                    .map_err(|_| {
                        StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                            "invalid www-authenticate header value".to_owned(),
                        ))
                    })?
                    .to_owned();
                return Err(StreamableHttpError::InsufficientScope(
                    InsufficientScopeError::new(header.clone(), extract_scope(&header)),
                ));
            }
        }
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let content_length = response.content_length();
        let response_session = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // Some servers answer notification/response POSTs with an empty 200;
        // treat it like 202 Accepted (rmcp does the same).
        if status.is_success()
            && content_length == Some(0)
            && matches!(
                &message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            let body = read_error_body(response, &self.cap).await?;
            if content_type
                .as_deref()
                .is_some_and(|value| value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()))
            {
                if let Ok(parsed @ JsonRpcMessage::Error(_)) =
                    parse_server_json_rpc(body.as_bytes())
                {
                    return Ok(StreamableHttpPostResponse::Json(parsed, response_session));
                }
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }
        match content_type.as_deref() {
            Some(value)
                if value
                    .as_bytes()
                    .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) =>
            {
                let stream =
                    BoundedSseStream::new(response, max_sse_event_size, Arc::clone(&self.cap));
                Ok(StreamableHttpPostResponse::Sse(
                    stream.boxed(),
                    response_session,
                ))
            }
            Some(value) if value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
                let body = read_body_bounded(response, &self.cap).await?;
                match parse_server_json_rpc(&body) {
                    Ok(parsed) => Ok(StreamableHttpPostResponse::Json(parsed, response_session)),
                    Err(error) => {
                        // A notification/response/error POST does not await a
                        // reply; an unusable body there means Accepted (rmcp's
                        // rule), while a request body must carry a reply.
                        if matches!(
                            &message,
                            ClientJsonRpcMessage::Notification(_)
                                | ClientJsonRpcMessage::Response(_)
                                | ClientJsonRpcMessage::Error(_)
                        ) {
                            Ok(StreamableHttpPostResponse::Accepted)
                        } else {
                            Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                                error,
                            )))
                        }
                    }
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<SseEvent, SseError>>, StreamableHttpError<Self::Error>>
    {
        self.inner
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<SseEvent, SseError>>, StreamableHttpError<Self::Error>>
    {
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.inner
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_bounded(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            DEFAULT_MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_bounded(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            max_sse_event_size,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Bounded SSE decoding
// ---------------------------------------------------------------------------

/// Extracts the event type of a `BoxedSseResponse` so this module can build
/// `sse_stream::Sse` values without naming the `sse-stream` crate (an
/// rmcp-internal dependency, not a direct one). The projection normalizes to
/// the concrete event type.
///
/// Public only because [`BoundedHttpClient`]'s [`StreamableHttpClient`] impl
/// names the projected type; not part of pir's supported surface.
#[doc(hidden)]
pub trait SseStreamItem {
    type Event;
}

impl<T, E> SseStreamItem for BoxStream<'static, Result<T, E>> {
    type Event = T;
}

#[doc(hidden)]
pub type SseEvent = <BoxedSseResponse as SseStreamItem>::Event;

/// One oversize SSE event (reported through [`SseError::Body`]).
#[derive(Debug)]
struct SseEventTooLarge {
    limit: usize,
}

impl std::fmt::Display for SseEventTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "SSE event exceeds the maximum size of {} bytes",
            self.limit
        )
    }
}

impl std::error::Error for SseEventTooLarge {}

/// A parse failure: either the per-event cap (which the stream reports as a
/// transport-cap violation) or a malformed SSE line.
enum SseParseError {
    TooLarge,
    Protocol(SseError),
}

/// Incremental SSE parser with a per-event raw-byte cap. It follows the same
/// line/field rules as `sse_stream` (comments, `data`/`event`/`id`/`retry`,
/// CR/LF/CRLF, BOM) while bounding the bytes retained for one event.
struct BoundedSseParser {
    limit: usize,
    line: Vec<u8>,
    line_started: bool,
    event_bytes: usize,
    line_is_comment: bool,
    skip_leading_lf: bool,
    first_line: bool,
    current: Option<SseEvent>,
    ready: VecDeque<SseEvent>,
}

impl BoundedSseParser {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            line: Vec::new(),
            line_started: false,
            event_bytes: 0,
            line_is_comment: false,
            skip_leading_lf: false,
            first_line: true,
            current: None,
            ready: VecDeque::new(),
        }
    }

    fn pop(&mut self) -> Option<SseEvent> {
        self.ready.pop_front()
    }

    /// Account one line-content byte; returns whether the byte must be
    /// retained (comment bytes are discarded so a huge comment cannot grow
    /// the line buffer).
    fn track(&mut self, byte: u8) -> Result<bool, SseParseError> {
        if !self.line_started {
            self.line_started = true;
            if byte == b':' {
                self.line_is_comment = true;
            }
        }
        if self.line_is_comment {
            return Ok(false);
        }
        self.event_bytes = self.event_bytes.saturating_add(1);
        if self.event_bytes > self.limit {
            return Err(SseParseError::TooLarge);
        }
        Ok(true)
    }

    fn feed(&mut self, mut bytes: &[u8]) -> Result<(), SseParseError> {
        if self.skip_leading_lf {
            self.skip_leading_lf = false;
            if bytes.first() == Some(&b'\n') {
                bytes = &bytes[1..];
            }
        }
        while let Some((index, byte)) = bytes
            .iter()
            .enumerate()
            .find(|(_, byte)| matches!(**byte, b'\n' | b'\r'))
        {
            for &content in &bytes[..index] {
                if self.track(content)? {
                    self.line.push(content);
                }
            }
            self.finish_line()?;
            let terminator = *byte;
            bytes = &bytes[index + 1..];
            if terminator == b'\r' {
                if bytes.first() == Some(&b'\n') {
                    bytes = &bytes[1..];
                } else if bytes.is_empty() {
                    self.skip_leading_lf = true;
                }
            }
        }
        for &content in bytes {
            if self.track(content)? {
                self.line.push(content);
            }
        }
        Ok(())
    }

    fn finish_line(&mut self) -> Result<(), SseParseError> {
        if !self.line_started {
            if let Some(event) = self.current.take() {
                self.ready.push_back(event);
            }
            self.event_bytes = 0;
            return Ok(());
        }
        let mut line = std::mem::take(&mut self.line);
        let comment = self.line_is_comment;
        self.line_started = false;
        self.line_is_comment = false;
        if comment {
            return Ok(());
        }
        if self.first_line {
            self.first_line = false;
            if let Some(stripped) = line.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
                line = stripped.to_vec();
            }
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Err(SseParseError::Protocol(SseError::InvalidLine));
        };
        let field = &line[..colon];
        let value = &line[colon + 1..];
        let value = value.strip_prefix(b" ").unwrap_or(value);
        let text = std::str::from_utf8(value)
            .map_err(|error| SseParseError::Protocol(SseError::Utf8Parse(error)))?;
        match field {
            b"data" => {
                let event = self.current.get_or_insert_with(Default::default);
                match event.data.as_mut() {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(text);
                    }
                    None => event.data = Some(text.to_owned()),
                }
            }
            b"event" => {
                let event = self.current.get_or_insert_with(Default::default);
                if event.event.is_some() {
                    return Err(SseParseError::Protocol(SseError::DuplicatedEventLine));
                }
                event.event = Some(text.to_owned());
            }
            b"id" => {
                // Per spec: an id containing U+0000 NULL is ignored.
                if !value.contains(&0) {
                    let event = self.current.get_or_insert_with(Default::default);
                    if event.id.is_some() {
                        return Err(SseParseError::Protocol(SseError::DuplicatedIdLine));
                    }
                    event.id = Some(text.to_owned());
                }
            }
            b"retry" => {
                let retry = text
                    .trim()
                    .parse::<u64>()
                    .map_err(|error| SseParseError::Protocol(SseError::IntParse(error)))?;
                let event = self.current.get_or_insert_with(Default::default);
                if event.retry.is_some() {
                    return Err(SseParseError::Protocol(SseError::DuplicatedRetry));
                }
                event.retry = Some(retry);
            }
            _ => return Err(SseParseError::Protocol(SseError::InvalidLine)),
        }
        Ok(())
    }
}

/// The bounded SSE event stream returned from a POST response.
struct BoundedSseStream {
    source: BoxStream<'static, Result<Vec<u8>, reqwest::Error>>,
    parser: BoundedSseParser,
    cap: Arc<FrameCapState>,
    done: bool,
}

impl BoundedSseStream {
    fn new(response: reqwest::Response, max_event_bytes: usize, cap: Arc<FrameCapState>) -> Self {
        let source = response
            .bytes_stream()
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .boxed();
        Self {
            source,
            parser: BoundedSseParser::new(max_event_bytes),
            cap,
            done: false,
        }
    }
}

impl Stream for BoundedSseStream {
    type Item = Result<SseEvent, SseError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.parser.pop() {
                return Poll::Ready(Some(Ok(event)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match this.source.as_mut().poll_next(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => match this.parser.feed(&chunk) {
                    Ok(()) => {}
                    Err(SseParseError::TooLarge) => {
                        this.done = true;
                        this.cap.mark_overflowed();
                        return Poll::Ready(Some(Err(SseError::Body(Box::new(
                            SseEventTooLarge {
                                limit: this.parser.limit,
                            },
                        )))));
                    }
                    Err(SseParseError::Protocol(error)) => {
                        this.done = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                },
                Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(SseError::Body(Box::new(error)))));
                }
                Poll::Ready(None) => {
                    this.done = true;
                }
            }
        }
    }
}
