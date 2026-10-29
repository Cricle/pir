//! The MCP client on top of the official `rmcp` SDK (upstream
//! `packages/mcp/src/client.ts`).
//!
//! rmcp owns everything that used to be hand-rolled in this module: the
//! JSON-RPC 2.0 framing, the initialize handshake with protocol version
//! negotiation, request-id bookkeeping and pending-map response correlation,
//! and both transports (child-process stdio and streamable HTTP with SSE).
//! What remains here is pir's session surface:
//!
//! - typed connection entry points ([`McpClient::connect_stdio`] and
//!   [`McpClient::connect_http`]) built from resolved server configs;
//! - the negotiated server snapshot (protocol version, capabilities,
//!   instructions, server info) mirroring upstream's `initialize` result
//!   fields;
//! - the server-request surface: `roots/list` from the configured root
//!   source and `setRequestHandler` for `ping`, `roots/list`,
//!   `elicitation/create` and arbitrary methods, with the server's
//!   `notifications/cancelled` bridged to the handler's abort signal;
//! - per-operation deadlines ([`McpError::Timeout`]) around connect and every
//!   request (upstream `requestTimeoutMs`); a request deadline is an *idle*
//!   deadline that server progress restarts, and an elapsed deadline or an
//!   aborted caller cancels the request server-side
//!   (`notifications/cancelled`, upstream `requestInternal`);
//! - rmcp -> pir protocol-type conversion so the builtin plugin and the CLI
//!   keep their upstream-shaped result types;
//! - pir's stdio shutdown semantics: rmcp closes the transport (stdin close,
//!   grace, kill) and pir sweeps the whole process tree afterwards
//!   (upstream `killProcessTree`);
//! - a notification surface ([`McpClient::take_notifications`]) that records
//!   the server's `*/list_changed` / `resources/updated` / logging
//!   (`notifications/message`) events for the manager's refresh + log path
//!   (upstream runtime.ts wires exactly this set: log forwarding at line 386,
//!   tool/resource refresh at lines 399-404; `prompts/list_changed` and
//!   `resources/updated` are recorded but have no upstream consumer).

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, ClientNotification,
    ClientRequest, ElicitRequestParams, ElicitResult, GetPromptRequestParams, Implementation,
    ListToolsResult, ProgressToken, ReadResourceRequestParams, RootsCapabilities, ServerResult,
};
use rmcp::service::{
    NotificationContext, RequestContext, RequestHandle, RoleClient, RunningService,
};
use rmcp::{ClientHandler, Peer, ServiceError, ServiceExt};

use serde_json::Value as JsonValue;

use crate::ai::types::AbortSignal;

use super::error::McpError;
use super::http;
use super::log::McpLog;
use super::protocol::{
    CallToolResult, ContentBlock, GetPromptResult, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, LoggingMessage, McpPrompt, McpResource, McpResourceTemplate, McpTool,
    PromptMessage, PromptsCapability, ReadResourceResult, ResourceContents, ResourcesCapability,
    ServerCapabilities, ToolsCapability,
};
use super::stdio;

/// Client identity sent with `initialize` (upstream `Implementation`).
pub const CLIENT_NAME: &str = "pir";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound on `tools/list` pagination before giving up (upstream
/// `MAX_LIST_PAGES`).
const MAX_LIST_PAGES: usize = 1_000;

/// How long a read-only request waits before its single transient retry
/// (upstream `CONNECT_RETRY_DELAYS_MS[0]`, applied by runtime.ts's
/// `withClient(run, readOnly)`).
const READ_ONLY_RETRY_DELAY: Duration = Duration::from_millis(250);

/// The transient HTTP status carried by a transport error whose canonical
/// reason contains "timeout" (`408 Request Timeout`, `504 Gateway Timeout`).
/// `http::transport_error_from_chain` classifies those as a request timeout
/// before it looks for the HTTP status, but upstream's `isTransientError`
/// retries them like every other 408/429/5xx.
fn transient_status_in_transport_message(message: &str) -> Option<u16> {
    let position = message.find("HTTP ")?;
    let rest = &message[position + 5..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() != 3 {
        return None;
    }
    let status: u16 = digits.parse().ok()?;
    // The status must stand alone (a body snippet cannot fake it).
    if !matches!(
        rest[3..].chars().next(),
        None | Some(' ') | Some(':') | Some('\r') | Some('\n')
    ) {
        return None;
    }
    http::is_transient_status(status).then_some(status)
}

/// Whether a failed request is worth one more attempt for a READ-ONLY
/// operation (upstream runtime.ts:70-75 `isTransientError`): HTTP
/// 408/429/5xx except 501, plus network-level (mid-flight reset) failures.
/// Write operations never ask this question: the server may already have run
/// them.
fn is_transient_read_error(error: &McpError) -> bool {
    match error {
        McpError::Http { status, .. } => http::is_transient_status(*status),
        // Upstream also retries `TypeError` (a fetch-level network failure);
        // pir maps mid-flight connection resets to the reset-class transport
        // error. A 408/504 status line contains "timeout", so it reaches this
        // variant first; recover the status from the message.
        McpError::Transport(message) => {
            http::is_transient_transport_message(message)
                || transient_status_in_transport_message(message).is_some()
        }
        _ => false,
    }
}

/// Run one read-only request, retrying it once after a transient failure
/// (upstream runtime.ts:274-301 `withClient(run, readOnly = true)`). Only
/// read-only operations use this; `tools/call` is dispatched at most once.
async fn with_read_only_retry<T, F, Fut>(run: F) -> Result<T, McpError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, McpError>>,
{
    match run().await {
        Err(error) if is_transient_read_error(&error) => {
            tokio::time::sleep(READ_ONLY_RETRY_DELAY).await;
            run().await
        }
        result => result,
    }
}

/// Server-bound shutdown wait for the rmcp service close (rmcp itself waits
/// up to 3s for a stdio child to exit before killing it).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(8);

/// Source of the process-unique client instance ids (readiness cache key of
/// the manager). Never reused, so a stale readiness record can never match a
/// replacement client.
static NEXT_CLIENT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// One filesystem root advertised to a server (`roots/list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRoot {
    /// `file://` (or another scheme) URI.
    pub uri: String,
    /// Optional display name; omitted from the wire when `None`.
    pub name: Option<String>,
}

impl McpRoot {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[allow(deprecated)] // roots are SEP-2577-deprecated but still spoken by upstream clients
    fn to_rmcp(&self) -> rmcp::model::Root {
        let mut root = rmcp::model::Root::new(self.uri.clone());
        root.name = self.name.clone();
        root
    }
}

/// A dynamic `roots/list` source (upstream's `() => Root[] | Promise<Root[]>`).
pub type RootsProvider = Arc<dyn Fn() -> BoxFuture<'static, Vec<McpRoot>> + Send + Sync>;

/// Options fixed for one MCP connection (upstream `McpClientOptions`).
///
/// `roots` is announced as the `roots` client capability and answers the
/// server's `roots/list` requests. `request_timeout` is the default deadline
/// for every request; the `timeout` argument of the connect functions remains
/// the default when this is `None`, preserving the old behavior.
#[derive(Clone, Default)]
pub struct McpClientOptions {
    pub roots: Option<RootsProvider>,
    pub request_timeout: Option<Duration>,
}

impl McpClientOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fixed root list (upstream `roots: Root[]`).
    pub fn with_roots(mut self, roots: Vec<McpRoot>) -> Self {
        self.roots = Some(Arc::new(move || {
            let roots = roots.clone();
            Box::pin(std::future::ready(roots))
        }));
        self
    }

    /// A dynamic root list (upstream `roots: () => Root[]`).
    pub fn with_dynamic_roots<F, Fut>(mut self, roots: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<McpRoot>> + Send + 'static,
    {
        self.roots = Some(Arc::new(move || Box::pin(roots())));
        self
    }

    /// Default per-request deadline (upstream `requestTimeoutMs`).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }
}

/// Per-request options (upstream `McpRequestOptions`).
#[derive(Clone, Default)]
pub struct McpRequestOptions {
    /// Cancels the request server-side with `notifications/cancelled`.
    pub signal: Option<AbortSignal>,
    /// Overrides the client's default deadline for this request. `None` uses
    /// the client default; `Duration::ZERO` disables the deadline.
    pub timeout: Option<Duration>,
    /// Receives the server's `notifications/progress` for this request.
    pub on_progress: Option<ProgressCallback>,
}

impl McpRequestOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_signal(mut self, signal: AbortSignal) -> Self {
        self.signal = Some(signal);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_progress<F>(mut self, on_progress: F) -> Self
    where
        F: Fn(McpProgress) + Send + Sync + 'static,
    {
        self.on_progress = Some(Arc::new(on_progress));
        self
    }
}

/// One progress notification for a request (upstream `ProgressNotification`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpProgress {
    /// The request's progress token, as JSON (number or string).
    pub progress_token: JsonValue,
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

/// Callback receiving progress notifications.
pub type ProgressCallback = Arc<dyn Fn(McpProgress) + Send + Sync>;

/// A handler for one server-initiated request method (upstream
/// `setRequestHandler`). `params` is the request's JSON (or `None`) and
/// `signal` is aborted when the server cancels the request.
pub type ClientRequestHandler = Arc<
    dyn Fn(Option<JsonValue>, AbortSignal) -> BoxFuture<'static, Result<JsonValue, McpError>>
        + Send
        + Sync,
>;

struct HandlerEntry {
    id: u64,
    handler: ClientRequestHandler,
}

/// Shared server-request handlers. The client and its rmcp handler hold the
/// same map, so `set_request_handler` works while the connection is live.
type RequestHandlers = Arc<RwLock<HashMap<String, HandlerEntry>>>;

/// Removes a handler installed by [`McpClient::set_request_handler`] on drop
/// (upstream returns a dispose function; the guard replaces it).
pub struct RequestHandlerGuard {
    handlers: RequestHandlers,
    method: String,
    id: u64,
}

impl Drop for RequestHandlerGuard {
    fn drop(&mut self) {
        let mut handlers = self
            .handlers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handlers
            .get(&self.method)
            .is_some_and(|entry| entry.id == self.id)
        {
            handlers.remove(&self.method);
        }
    }
}

/// The discovery-invalidation signal of one live connection
/// (`notifications/tools/list_changed`).
///
/// The manager caches each server's readiness after a successful
/// `tools/list` and compares the generation recorded by that discovery with
/// the current value here: an equal generation is the *fast path* (no
/// round trip, no tool-list clone), a newer generation marks the server stale
/// and the next tool call re-lists. The counter lives next to the client
/// handler that receives the notification, so an event arriving between two
/// calls is observed without draining (and consuming) the manager's
/// notification log buffer.
#[derive(Debug, Default)]
pub struct DiscoveryInvalidation {
    tools_generation: AtomicU64,
}

impl DiscoveryInvalidation {
    /// The number of `notifications/tools/list_changed` events recorded on
    /// this connection so far.
    pub fn tools_generation(&self) -> u64 {
        self.tools_generation.load(Ordering::SeqCst)
    }

    /// Record one `tools/list_changed` (client handler only).
    fn mark_tools_changed(&self) {
        self.tools_generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// A server notification that pir records for the manager's refresh/log path
/// (upstream runtime.ts `onNotification` wiring).
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// `notifications/tools/list_changed`.
    ToolsListChanged,
    /// `notifications/prompts/list_changed`.
    PromptsListChanged,
    /// `notifications/resources/list_changed`.
    ResourcesListChanged,
    /// `notifications/resources/updated`.
    ResourceUpdated {
        /// The URI of the updated resource.
        uri: String,
    },
    /// `notifications/message` (upstream runtime.ts:386 forwards the payload
    /// to the server log).
    LoggingMessage(LoggingMessage),
}

/// Hard cap on recorded-but-not-yet-drained notifications per connection.
/// A server (or a misbehaving one) can legally emit notifications at any rate;
/// without a bound the buffer grows until the manager next drains it, which
/// may be never for a server that is never called. Oldest events are dropped
/// first: `tools/list_changed` also advances [`DiscoveryInvalidation`], so a
/// dropped one still marks the server stale, and logging/resource/prompt
/// events are advisory.
const MAX_PENDING_NOTIFICATIONS: usize = 256;

/// Approximate byte cap on the recorded notification buffer (see
/// [`MAX_PENDING_NOTIFICATIONS`]); bounds a flood of large
/// `notifications/message` payloads between drains.
const MAX_PENDING_NOTIFICATION_BYTES: usize = 256 * 1024;

/// Per-message cap on a logging notification's `data` payload. Oversized
/// values are rendered as JSON text and truncated, so one message cannot
/// consume the whole buffer and the MCP log stays readable.
const MAX_LOGGING_MESSAGE_DATA_BYTES: usize = 8 * 1024;

/// Bounded notification buffer: a FIFO of `(retained_bytes, event)` with both
/// a count and a byte cap. `take` drains it in arrival order.
#[derive(Default)]
struct NotificationBuffer {
    events: VecDeque<(usize, ServerEvent)>,
    bytes: usize,
}

impl NotificationBuffer {
    fn record(&mut self, event: ServerEvent) {
        let event = match event {
            ServerEvent::LoggingMessage(message) => {
                ServerEvent::LoggingMessage(bound_logging_message(message))
            }
            other => other,
        };
        let size = server_event_bytes(&event);
        self.events.push_back((size, event));
        self.bytes = self.bytes.saturating_add(size);
        while self.events.len() > MAX_PENDING_NOTIFICATIONS
            || self.bytes > MAX_PENDING_NOTIFICATION_BYTES
        {
            let Some((evicted_size, _)) = self.events.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted_size);
        }
    }

    fn take(&mut self) -> Vec<ServerEvent> {
        self.bytes = 0;
        std::mem::take(&mut self.events)
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }
}

/// Approximate retained size of one recorded event.
fn server_event_bytes(event: &ServerEvent) -> usize {
    match event {
        ServerEvent::ToolsListChanged
        | ServerEvent::PromptsListChanged
        | ServerEvent::ResourcesListChanged => 8,
        ServerEvent::ResourceUpdated { uri } => uri.len(),
        ServerEvent::LoggingMessage(message) => {
            message.level.as_ref().map_or(0, String::len)
                + message.logger.as_ref().map_or(0, String::len)
                + message
                    .data
                    .as_ref()
                    .map(|data| match data {
                        JsonValue::String(text) => text.len(),
                        other => other.to_string().len(),
                    })
                    .unwrap_or(0)
        }
    }
}

/// Bound one logging message's payload (see [`MAX_LOGGING_MESSAGE_DATA_BYTES`]).
/// Non-string payloads become their JSON text, matching how
/// [`McpLog::write_notification`] renders them.
fn bound_logging_message(mut message: LoggingMessage) -> LoggingMessage {
    if let Some(data) = message.data.take() {
        let mut text = match data {
            JsonValue::String(text) => text,
            other => other.to_string(),
        };
        truncate_utf8_tail(&mut text, MAX_LOGGING_MESSAGE_DATA_BYTES);
        message.data = Some(JsonValue::String(text));
    }
    message
}

/// Truncate `text` to at most `max` bytes at a character boundary, keeping a
/// short marker so the log shows the value was cut.
fn truncate_utf8_tail(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    const MARKER: &str = "...[truncated]";
    let mut end = max.saturating_sub(MARKER.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(MARKER);
}

/// One live request's progress registration, keyed by the `progressToken`
/// rmcp stamped on that request.
///
/// A `notifications/progress` carrying this token restarts the request's idle
/// deadline (upstream resets its request timer on progress, so a server that
/// keeps reporting progress is never cut off mid-work) and invokes the
/// request's `on_progress` callback. The request owns the
/// [`tokio::sync::watch::Receiver`]; the handler only bumps the sender and
/// calls the callback. A watch is used rather than a oneshot/`Notify` because
/// its version change is retained until observed, so a notification arriving
/// between two polls of the receiver still restarts the deadline exactly once.
#[derive(Clone)]
struct ProgressEntry {
    reset: tokio::sync::watch::Sender<u64>,
    on_progress: Option<ProgressCallback>,
}

type ProgressResets = Arc<Mutex<HashMap<ProgressToken, ProgressEntry>>>;

/// Removes one request's progress entry when the request future ends, so a
/// select arm that returns early (timeout, abort) cannot leak the entry.
struct ProgressRegistration {
    map: ProgressResets,
    token: ProgressToken,
}

impl Drop for ProgressRegistration {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.token);
    }
}

/// The handler options the rmcp client handler is built with: roots and the
/// shared server-request hook map (upstream `McpClient`'s constructor state).
struct HandlerOptions {
    roots: Option<RootsProvider>,
    handlers: RequestHandlers,
}

/// pir's client handler: records notifications (list-changed and logging),
/// restarts a caller's idle request deadline on `notifications/progress`,
/// answers server-initiated requests (ping, roots/list, custom methods), and
/// identifies pir with the initialize handshake.
#[derive(Default)]
struct PirClientHandler {
    notifications: Arc<Mutex<NotificationBuffer>>,
    /// Readiness invalidation shared with the manager: `tools/list_changed`
    /// bumps it so the manager's per-server cache is re-listed on the next
    /// call without touching the notification buffer.
    invalidation: Arc<DiscoveryInvalidation>,
    /// In-flight cancellable requests by progress token (see
    /// [`ProgressRegistration`]).
    progress_resets: ProgressResets,
    roots: Option<RootsProvider>,
    handlers: RequestHandlers,
}

impl PirClientHandler {
    fn new(options: HandlerOptions) -> Self {
        Self {
            notifications: Arc::default(),
            invalidation: Arc::default(),
            progress_resets: Arc::default(),
            roots: options.roots,
            handlers: options.handlers,
        }
    }

    fn record(&self, event: ServerEvent) {
        let mut pending = self
            .notifications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.record(event);
    }

    fn handler(&self, method: &str) -> Option<ClientRequestHandler> {
        self.handlers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(method)
            .map(|entry| Arc::clone(&entry.handler))
    }

    /// Invoke one registered handler with a cancellation bridge from rmcp's
    /// request context. The bridge task ends when the handler does, so a
    /// long-lived connection does not accumulate watcher tasks.
    async fn call_handler(
        &self,
        handler: &ClientRequestHandler,
        params: Option<JsonValue>,
        context: RequestContext<RoleClient>,
    ) -> Result<JsonValue, McpError> {
        let signal = AbortSignal::new();
        let token = context.ct.clone();
        let done = AbortSignal::new();
        let bridge_signal = signal.clone();
        let bridge_done = done.clone();
        let bridge = tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => bridge_signal.abort(),
                _ = bridge_done.cancelled() => {}
            }
        });
        let result = handler(params, signal).await;
        done.abort();
        if !bridge.is_finished() {
            bridge.abort();
        }
        result
    }
}

fn into_error_data(error: McpError) -> rmcp::model::ErrorData {
    match error {
        McpError::Server {
            code,
            message,
            data,
        } => rmcp::model::ErrorData::new(
            rmcp::model::ErrorCode(code.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
            message,
            data,
        ),
        other => rmcp::model::ErrorData::internal_error(other.to_string(), None),
    }
}

impl ClientHandler for PirClientHandler {
    /// `ping` from the server. Upstream installs a `ping` handler returning
    /// `{}`; an explicit `set_request_handler("ping", ...)` replaces it.
    async fn ping(
        &self,
        context: RequestContext<RoleClient>,
    ) -> Result<(), rmcp::model::ErrorData> {
        if let Some(handler) = self.handler("ping") {
            let _ = self
                .call_handler(&handler, None, context)
                .await
                .map_err(into_error_data)?;
        }
        Ok(())
    }

    /// `roots/list` from the server: an explicit handler wins, then the
    /// configured root source, then an empty list (rmcp's default).
    ///
    /// rmcp 3.5 deprecates the Roots API (SEP-2577) but leaves
    /// `ClientHandler::list_roots` in place with `ListRootsResult` in its
    /// signature; there is no non-deprecated equivalent to implement against.
    #[allow(deprecated)]
    async fn list_roots(
        &self,
        context: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::model::ErrorData> {
        if let Some(handler) = self.handler("roots/list") {
            let value = self
                .call_handler(&handler, None, context)
                .await
                .map_err(into_error_data)?;
            return serde_json::from_value(value).map_err(|error| {
                rmcp::model::ErrorData::invalid_params(
                    format!("Invalid roots/list handler result: {error}"),
                    None,
                )
            });
        }
        let roots = match &self.roots {
            Some(provider) => provider().await,
            None => Vec::new(),
        };
        Ok(rmcp::model::ListRootsResult::new(
            roots.iter().map(McpRoot::to_rmcp).collect(),
        ))
    }

    /// A server-initiated `elicitation/create`: an explicit handler wins; the
    /// default declines, as rmcp's default does.
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::model::ErrorData> {
        if let Some(handler) = self.handler("elicitation/create") {
            let params = serde_json::to_value(&request)
                .map_err(|error| rmcp::model::ErrorData::internal_error(error.to_string(), None))?;
            let value = self
                .call_handler(&handler, Some(params), context)
                .await
                .map_err(into_error_data)?;
            return serde_json::from_value(value).map_err(|error| {
                rmcp::model::ErrorData::invalid_params(
                    format!("Invalid elicitation/create handler result: {error}"),
                    None,
                )
            });
        }
        Ok(ElicitResult::new(rmcp::model::ElicitationAction::Decline))
    }

    /// Any other server-initiated request method (`setRequestHandler`).
    async fn on_custom_request(
        &self,
        request: rmcp::model::CustomRequest,
        context: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::CustomResult, rmcp::model::ErrorData> {
        let Some(handler) = self.handler(&request.method) else {
            return Err(rmcp::model::ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("Method not found: {}", request.method),
                None,
            ));
        };
        let value = self
            .call_handler(&handler, request.params, context)
            .await
            .map_err(into_error_data)?;
        Ok(rmcp::model::CustomResult::new(value))
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.invalidation.mark_tools_changed();
        self.record(ServerEvent::ToolsListChanged);
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record(ServerEvent::PromptsListChanged);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record(ServerEvent::ResourcesListChanged);
    }

    async fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record(ServerEvent::ResourceUpdated { uri: params.uri });
    }

    // rmcp marks logging deprecated after SEP-2577, but upstream pi forwards
    // `notifications/message` to the server log (runtime.ts:386), so pir
    // keeps the typed listener.
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let level = serde_json::to_value(params.level)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned));
        self.record(ServerEvent::LoggingMessage(LoggingMessage {
            level,
            logger: params.logger,
            data: Some(params.data),
        }));
    }

    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        // Restart the matching request's idle deadline and deliver the
        // caller's progress callback. An unknown token (a request that
        // already ended, or one this client never issued) is ignored: the map
        // only holds the cancellable requests in flight.
        let entry = self
            .progress_resets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&params.progress_token)
            .cloned();
        let Some(entry) = entry else {
            return;
        };
        entry
            .reset
            .send_modify(|version| *version = version.wrapping_add(1));
        if let Some(on_progress) = &entry.on_progress {
            let notification = McpProgress {
                progress_token: serde_json::to_value(&params.progress_token)
                    .unwrap_or(JsonValue::Null),
                progress: params.progress,
                total: params.total,
                message: params.message,
            };
            on_progress(notification);
        }
    }

    fn get_info(&self) -> ClientConfig {
        let mut capabilities = ClientCapabilities::default();
        if self.roots.is_some() || self.handler("roots/list").is_some() {
            capabilities.roots = Some(RootsCapabilities::default());
        }
        ClientConfig::new(
            capabilities,
            Implementation::new(CLIENT_NAME, CLIENT_VERSION),
        )
    }
}

/// The live rmcp client service plus pir's connection bookkeeping.
pub struct McpClient {
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, PirClientHandler>>>,
    /// Per-request deadline (upstream `requestTimeoutMs`).
    timeout: Duration,
    /// Connection options (roots source, default request timeout).
    options: McpClientOptions,
    /// Server-request handlers shared with the rmcp handler
    /// ([`McpClient::set_request_handler`]).
    handlers: RequestHandlers,
    /// Notification surface shared with the client handler (bounded count and
    /// bytes; see [`NotificationBuffer`]).
    notifications: Arc<Mutex<NotificationBuffer>>,
    /// In-flight cancellable requests by progress token, shared with the
    /// client handler (see [`ProgressRegistration`]).
    progress_resets: ProgressResets,
    /// The `tools/list_changed` generation of this connection (the manager's
    /// readiness cache compares it with the generation it recorded when it
    /// last listed the tools).
    invalidation: Arc<DiscoveryInvalidation>,
    /// Process-unique identity of this client instance: the manager's
    /// readiness record is only valid for the instance that produced it, so
    /// a reconnect (replacement client) invalidates the cached tool list.
    instance_id: u64,
    /// The stdio server's pid, for the post-shutdown process-tree sweep.
    child_pid: Option<u32>,
    /// Bounded stderr tail of a stdio server (None for HTTP), shared with the
    /// drain task. Upstream `StdioTransport.stderr` / `connectFailed`: the
    /// manager appends it to a connect-failure message so a server that dies
    /// during `initialize` explains why.
    stderr_tail: Option<Arc<Mutex<Vec<u8>>>>,
    /// Shared overflow flag of the transport's bounded reader: the stdio
    /// frame reader, or the streamable-HTTP client's plain-JSON body reader.
    /// When a read stops at the configured cap it records the violation and
    /// the transport closes; the request paths below turn that generic close
    /// into [`McpError::MessageTooLarge`].
    frame_cap: Option<Arc<super::stdio::FrameCapState>>,
    /// The negotiated protocol version (set after `initialize`).
    protocol_version: Option<String>,
    /// Server capabilities from `initialize`.
    capabilities: Option<ServerCapabilities>,
    /// Server instructions from `initialize`.
    instructions: Option<String>,
    /// Server identification from `initialize`.
    server_info: Option<super::protocol::Implementation>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpClient")
            .field("protocol_version", &self.protocol_version)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawn the configured stdio server, run the `initialize` handshake
    /// through rmcp, and return the connected client. Any failure before the
    /// handshake completes tears the child process down (rmcp kills the
    /// transport's child on drop).
    pub async fn connect_stdio(
        options: stdio::StdioSpawnOptions,
        log: Option<Arc<McpLog>>,
        server_name: &str,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Self::connect_stdio_with_options(
            options,
            log,
            server_name,
            timeout,
            McpClientOptions::default(),
        )
        .await
    }

    /// [`McpClient::connect_stdio`] with roots / request-timeout options
    /// (upstream `McpClientOptions`).
    pub async fn connect_stdio_with_options(
        options: stdio::StdioSpawnOptions,
        log: Option<Arc<McpLog>>,
        server_name: &str,
        timeout: Duration,
        client_options: McpClientOptions,
    ) -> Result<Self, McpError> {
        let connection = stdio::spawn(options, log, server_name)?;
        Self::connect_stdio_spawned_with_options(connection, timeout, client_options).await
    }

    /// Connect an ALREADY-SPAWNED stdio server ([`stdio::spawn`]). The
    /// manager uses this form so it still holds the shared stderr tail when
    /// the handshake fails and can append the server's last diagnostics to
    /// the failure message (upstream runtime.ts `connectFailed`).
    pub async fn connect_stdio_spawned(
        connection: stdio::StdioConnection,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Self::connect_stdio_spawned_with_options(connection, timeout, McpClientOptions::default())
            .await
    }

    /// [`McpClient::connect_stdio_spawned`] with connection options.
    pub async fn connect_stdio_spawned_with_options(
        connection: stdio::StdioConnection,
        timeout: Duration,
        client_options: McpClientOptions,
    ) -> Result<Self, McpError> {
        let stdio::StdioConnection {
            transport,
            child_pid,
            frame_cap,
            stderr_tail,
            ..
        } = connection;
        Self::connect_service(
            transport,
            timeout,
            child_pid,
            Some(frame_cap),
            Some(stderr_tail),
            client_options,
        )
        .await
    }

    /// Run the `initialize` handshake against a streamable HTTP endpoint
    /// through rmcp (session-id handling, SSE responses, and expiry recovery
    /// are rmcp's).
    pub async fn connect_http(
        url: String,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Self::connect_http_with_auth(url, headers, None, timeout).await
    }

    /// [`McpClient::connect_http`] with connection options.
    pub async fn connect_http_with_options(
        url: String,
        headers: Vec<(String, String)>,
        timeout: Duration,
        client_options: McpClientOptions,
    ) -> Result<Self, McpError> {
        Self::connect_http_with_auth_and_options(url, headers, None, timeout, client_options).await
    }

    /// [`McpClient::connect_http`] with an OAuth access token attached as
    /// `Authorization: Bearer <token>` (upstream's transport auth provider
    /// puts the same header on every request).
    pub async fn connect_http_with_auth(
        url: String,
        headers: Vec<(String, String)>,
        auth_token: Option<String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Self::connect_http_with_auth_and_options(
            url,
            headers,
            auth_token,
            timeout,
            McpClientOptions::default(),
        )
        .await
    }

    /// [`McpClient::connect_http_with_auth`] with connection options.
    pub async fn connect_http_with_auth_and_options(
        url: String,
        headers: Vec<(String, String)>,
        auth_token: Option<String>,
        timeout: Duration,
        client_options: McpClientOptions,
    ) -> Result<Self, McpError> {
        let config = http::streamable_http_config_with_auth(url, headers, auth_token)?;
        // The plain-JSON branch of rmcp's HTTP client buffers the whole body
        // (`response.bytes()`); pir passes a client that enforces the same
        // 1 MiB bound the stdio reader uses and records overflows in the
        // shared cap state, so the request path reports `MessageTooLarge`
        // instead of allocating an unbounded response.
        let frame_cap =
            super::stdio::FrameCapState::new(super::transport::DEFAULT_MAX_MESSAGE_BYTES);
        let client = http::bounded_http_client(Arc::clone(&frame_cap))?;
        let transport = rmcp::transport::StreamableHttpClientTransport::with_client(client, config);
        Self::connect_service(
            transport,
            timeout,
            None,
            Some(frame_cap),
            None,
            client_options,
        )
        .await
    }

    /// Serve the client over a ready transport, bounded by `timeout`.
    #[allow(clippy::too_many_arguments)]
    async fn connect_service<T, E, A>(
        transport: T,
        timeout: Duration,
        child_pid: Option<u32>,
        frame_cap: Option<Arc<super::stdio::FrameCapState>>,
        stderr_tail: Option<Arc<Mutex<Vec<u8>>>>,
        client_options: McpClientOptions,
    ) -> Result<Self, McpError>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let handlers: RequestHandlers = Arc::new(RwLock::new(HashMap::new()));
        let handler = PirClientHandler::new(HandlerOptions {
            roots: client_options.roots.clone(),
            handlers: Arc::clone(&handlers),
        });
        let notifications = Arc::clone(&handler.notifications);
        let invalidation = Arc::clone(&handler.invalidation);
        let progress_resets = Arc::clone(&handler.progress_resets);
        let service = match tokio::time::timeout(timeout, handler.serve(transport)).await {
            Ok(Ok(service)) => service,
            Ok(Err(error)) => {
                return Err(frame_cap_violation(&frame_cap)
                    .unwrap_or_else(|| http::map_initialize_error(error)));
            }
            // Dropping the serve future drops the transport, which kills a
            // stdio child (upstream client.ts closes the transport on every
            // handshake failure path). The tree kill reaches children the
            // child spawned (`.cmd`/`npx`/`uvx` shims).
            Err(_) => {
                if let Some(pid) = child_pid {
                    stdio::terminate_process_tree(pid);
                }
                return Err(
                    frame_cap_violation(&frame_cap).unwrap_or(McpError::Timeout {
                        timeout_ms: timeout.as_millis() as u64,
                    }),
                );
            }
        };
        Self::finish_connect(
            service,
            notifications,
            invalidation,
            progress_resets,
            timeout,
            child_pid,
            frame_cap,
            stderr_tail,
            client_options,
            handlers,
        )
        .await
    }

    /// Snapshot the negotiated server info, reject unsupported protocol
    /// versions, and assemble the client.
    #[allow(clippy::too_many_arguments)]
    async fn finish_connect(
        service: RunningService<RoleClient, PirClientHandler>,
        notifications: Arc<Mutex<NotificationBuffer>>,
        invalidation: Arc<DiscoveryInvalidation>,
        progress_resets: ProgressResets,
        timeout: Duration,
        child_pid: Option<u32>,
        frame_cap: Option<Arc<super::stdio::FrameCapState>>,
        stderr_tail: Option<Arc<Mutex<Vec<u8>>>>,
        client_options: McpClientOptions,
        handlers: RequestHandlers,
    ) -> Result<Self, McpError> {
        let mut protocol_version = None;
        let mut capabilities = None;
        let mut instructions = None;
        let mut server_info = None;
        if let Some(info) = service.peer_info() {
            // Servers answer with their own latest version; reject versions
            // we do not support and drop the connection (upstream client.ts:
            // `SUPPORTED_PROTOCOL_VERSIONS` check + close).
            let version = info.protocol_version.as_str().to_owned();
            if !super::protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str()) {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, service.cancel()).await;
                // `cancel` consumes the service and awaits its task, which drops
                // the stdio transport and runs its `ChildGuard`: the child was
                // already tree-killed and reaped there. A second pid-based
                // sweep after that point could signal a RECYCLED process group
                // (pid reuse), so it is deliberately not repeated here.
                return Err(McpError::Protocol(format!(
                    "MCP server selected unsupported protocol version {version}"
                )));
            }
            protocol_version = Some(version);
            capabilities = Some(convert_server_capabilities(&info.capabilities));
            instructions = info
                .instructions
                .clone()
                .filter(|text| !text.trim().is_empty());
            server_info =
                info.server_info
                    .clone()
                    .map(|implementation| super::protocol::Implementation {
                        name: implementation.name,
                        version: implementation.version,
                        title: implementation.title,
                    });
        }
        Ok(Self {
            service: tokio::sync::Mutex::new(Some(service)),
            timeout: client_options.request_timeout.unwrap_or(timeout),
            options: client_options,
            handlers,
            notifications,
            invalidation,
            progress_resets,
            instance_id: NEXT_CLIENT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            child_pid,
            stderr_tail,
            frame_cap,
            protocol_version,
            capabilities,
            instructions,
            server_info,
        })
    }

    /// The bounded tail of a stdio server's stderr, trimmed (upstream
    /// `StdioTransport.stderr`; runtime.ts appends it to the connect-failure
    /// message). `None` for HTTP transports and for a server that wrote
    /// nothing but whitespace.
    pub fn stderr_tail(&self) -> Option<String> {
        let tail = self.stderr_tail.as_ref()?;
        let bytes = tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Process-unique identity of this client instance (readiness cache key;
    /// a replacement connection gets a new id).
    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }

    /// The negotiated protocol version (set after `initialize`).
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// Server capabilities from `initialize`.
    pub fn server_capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }

    /// Whether the server offers resources, from its `initialize`
    /// capabilities (upstream `McpServerConnection.hasResources`). Resources
    /// are NOT listed at connect time (upstream #10526): listing every one can
    /// take long (a large drive, say) and used to keep the server's tools out
    /// of the first prompt. The aggregate resource tools list them on demand.
    pub fn has_resources(&self) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.resources.is_some())
    }

    /// Server instructions from `initialize`.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    /// Server identification from `initialize`.
    pub fn server_info(&self) -> Option<&super::protocol::Implementation> {
        self.server_info.as_ref()
    }

    /// Recorded server notifications, draining the buffer (upstream
    /// `handleMessage`'s notification branch). The buffer is bounded; the
    /// oldest events are dropped when a server floods it between drains (see
    /// [`NotificationBuffer`]).
    pub async fn take_notifications(&self) -> Vec<ServerEvent> {
        let mut pending = self
            .notifications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.take()
    }

    /// Answer one server-initiated request method (upstream
    /// `setRequestHandler`). The handler receives the request's params (or
    /// `None`) and an abort signal tied to the server's
    /// `notifications/cancelled`; its JSON result becomes the response
    /// result. Dropping the returned guard removes the handler.
    ///
    /// `ping`, `roots/list` and `elicitation/create` are dispatched to a
    /// registered handler first; every other method reaches the handler
    /// through rmcp's custom-request path.
    pub fn set_request_handler<F, Fut>(
        &self,
        method: impl Into<String>,
        handler: F,
    ) -> RequestHandlerGuard
    where
        F: Fn(Option<JsonValue>, AbortSignal) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<JsonValue, McpError>> + Send + 'static,
    {
        static NEXT_HANDLER_ID: AtomicU64 = AtomicU64::new(1);
        let method = method.into();
        let id = NEXT_HANDLER_ID.fetch_add(1, Ordering::Relaxed);
        let handler: ClientRequestHandler =
            Arc::new(move |params, signal| Box::pin(handler(params, signal)));
        self.handlers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(method.clone(), HandlerEntry { id, handler });
        RequestHandlerGuard {
            handlers: Arc::clone(&self.handlers),
            method,
            id,
        }
    }

    /// The configured roots source, if any.
    pub fn roots(&self) -> Option<&RootsProvider> {
        self.options.roots.as_ref()
    }

    /// The connection's discovery-invalidation signal (see
    /// [`DiscoveryInvalidation`]): the manager records its generation after a
    /// successful `tools/list` and treats a newer generation as "re-list
    /// before the next call".
    pub fn discovery_invalidation(&self) -> &Arc<DiscoveryInvalidation> {
        &self.invalidation
    }

    /// Whether the underlying transport dropped (the next request on the
    /// manager path reconnects).
    pub async fn transport_is_closed(&self) -> bool {
        let guard = self.service.lock().await;
        match guard.as_ref() {
            Some(service) => service.is_transport_closed(),
            None => true,
        }
    }

    /// The effective deadline for one request: the per-request override, else
    /// the client default. Upstream treats a non-positive `timeoutMs` as "no
    /// timer"; a zero duration maps to a practically unbounded sleep here.
    fn effective_timeout(&self, options: &McpRequestOptions) -> Duration {
        match options.timeout {
            Some(timeout) if timeout.is_zero() => Duration::from_secs(10 * 365 * 24 * 3600),
            Some(timeout) => timeout,
            None => self.timeout,
        }
    }

    /// Run one CANCELLABLE request against the live peer, bounded by the
    /// request's idle deadline (upstream `McpClient.requestInternal`,
    /// `packages/mcp/src/client.ts:403-445`).
    ///
    /// An aborted caller or an elapsed deadline **cancels the request on the
    /// server** with `notifications/cancelled` (rmcp's `RequestHandle::cancel`
    /// sends exactly that), so a long request stops server-side instead of
    /// running on unobserved. `initialize` is never cancellable (`cancellable:
    /// false` upstream); no request this client issues is `initialize`, so the
    /// rule is stated once here rather than per call site.
    ///
    /// Two invariants this body exists to keep:
    /// - the DISPATCH runs inside the same select as the response: rmcp's
    ///   request send awaits a bounded channel and the transport write, so a
    ///   peer whose channel is blocked must not outlast the deadline or the
    ///   caller's abort signal;
    /// - the deadline is an IDLE deadline: a `notifications/progress` naming
    ///   this request restarts it, so a server that reports progress is never
    ///   cut off mid-work (upstream resets its request timer the same way).
    ///
    /// The response error is mapped through [`http::map_service_error`]: a
    /// server-side JSON-RPC error must reach the caller as
    /// [`McpError::Server`], never as a bare closure the manager would retry
    /// (a retried `tools/call` repeats its side effects).
    async fn request_cancellable<F, Fut>(
        &self,
        options: &McpRequestOptions,
        operation: F,
    ) -> Result<ServerResult, McpError>
    where
        F: FnOnce(Peer<RoleClient>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<RequestHandle<RoleClient>, ServiceError>> + Send,
    {
        if options.signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return Err(McpError::Aborted);
        }
        let peer = {
            let guard = self.service.lock().await;
            match guard.as_ref() {
                Some(service) => service.peer().clone(),
                None => return Err(McpError::ConnectionClosed),
            }
        };
        let signal = options.signal.clone();
        let cancelled = async {
            match &signal {
                Some(signal) => signal.cancelled().await,
                // Without a caller signal this branch is never selected;
                // `pending` keeps the select's shape uniform.
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancelled);
        // The deadline is armed BEFORE the dispatch, not only around the
        // response: rmcp's request send awaits a bounded (1024-slot) channel
        // and the transport write, so a peer whose writer is blocked (a
        // server that stopped reading stdin) must not outlast the request
        // deadline. The same deadline then bounds the response wait below;
        // progress can only restart it once the request has a token.
        let timeout = self.effective_timeout(options);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut handle = tokio::select! {
            dispatched = operation(peer) => match dispatched {
                Ok(handle) => handle,
                Err(error) => {
                    return Err(frame_cap_violation(&self.frame_cap)
                        .unwrap_or_else(|| http::map_service_error(error)))
                }
            },
            () = &mut cancelled => return Err(McpError::Aborted),
            () = &mut deadline => {
                return Err(frame_cap_violation(&self.frame_cap).unwrap_or(McpError::Timeout {
                    timeout_ms: timeout.as_millis() as u64,
                }));
            }
        };
        // Register this request's progress reset and callback BEFORE awaiting
        // the response so no notification can slip between dispatch and
        // registration (rmcp stamps the progress token on the request itself).
        // The drop guard removes the entry on every exit — response, timeout,
        // abort.
        let (reset, mut resets) = tokio::sync::watch::channel(0u64);
        self.progress_resets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                handle.progress_token.clone(),
                ProgressEntry {
                    reset,
                    on_progress: options.on_progress.clone(),
                },
            );
        let _registration = ProgressRegistration {
            map: Arc::clone(&self.progress_resets),
            token: handle.progress_token.clone(),
        };
        let mut progress_live = true;
        loop {
            tokio::select! {
                response = &mut handle.rx => {
                    // The response channel closing is rmcp's `TransportClosed`
                    // (the service task ended): a typed connection error, never
                    // a bare closure the manager would retry.
                    let response = match response {
                        Ok(response) => response,
                        Err(_) => {
                            return Err(frame_cap_violation(&self.frame_cap)
                                .unwrap_or(McpError::ConnectionClosed))
                        }
                    };
                    return response.map_err(|error| {
                        frame_cap_violation(&self.frame_cap)
                            .unwrap_or_else(|| http::map_service_error(error))
                    });
                }
                () = &mut deadline => {
                    // The deadline elapsed: tell the server, then report the
                    // timeout (upstream `armTimeout` -> `cancelPending`).
                    let _ = handle.cancel(Some("Request timed out".to_string())).await;
                    return Err(frame_cap_violation(&self.frame_cap).unwrap_or(McpError::Timeout {
                        timeout_ms: timeout.as_millis() as u64,
                    }));
                }
                changed = resets.changed(), if progress_live => {
                    if changed.is_err() {
                        // The sender was dropped without the request ending
                        // (it cannot, but stay defensive): stop watching and
                        // let the response/deadline arms finish the request.
                        progress_live = false;
                    } else {
                        // Progress for this request: restart the idle deadline.
                        deadline
                            .as_mut()
                            .reset(tokio::time::Instant::now() + timeout);
                    }
                }
                () = &mut cancelled => {
                    // The caller gave up: stop the server-side work (upstream
                    // `onAbort` -> `cancelPending(..., reason)`).
                    let _ = handle.cancel(Some("Aborted".to_string())).await;
                    return Err(McpError::Aborted);
                }
            }
        }
    }

    /// The live peer, or the typed closed-connection error.
    async fn peer(&self) -> Result<Peer<RoleClient>, McpError> {
        let guard = self.service.lock().await;
        match guard.as_ref() {
            Some(service) => Ok(service.peer().clone()),
            None => Err(McpError::ConnectionClosed),
        }
    }

    /// The typed wrong-result-shape error for one method.
    fn unexpected_result(method: &str) -> McpError {
        McpError::Protocol(format!("{method} returned an unexpected result shape"))
    }

    /// `ping` with per-request options.
    pub async fn ping_with_options(&self, options: &McpRequestOptions) -> Result<(), McpError> {
        self.request_cancellable(options, |peer| async move {
            peer.send_cancellable_request(
                ClientRequest::PingRequest(rmcp::model::PingRequest::default()),
                rmcp::service::PeerRequestOptions::no_options(),
            )
            .await
        })
        .await
        .map(|_| ())
    }

    /// `ping` (bounded by the request timeout).
    pub async fn ping(&self) -> Result<(), McpError> {
        self.ping_with_options(&McpRequestOptions::default()).await
    }

    /// `tools/list` with an optional pagination cursor and per-request options.
    ///
    /// Read-only: a transient HTTP failure is retried once (upstream
    /// `withClient(..., readOnly)`).
    pub async fn list_tools_with_options(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<super::protocol::ListToolsResult, McpError> {
        with_read_only_retry(|| self.list_tools_page(cursor, options)).await
    }

    /// One `tools/list` round trip (no retry).
    async fn list_tools_page(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<super::protocol::ListToolsResult, McpError> {
        let mut params = rmcp::model::PaginatedRequestParams::default();
        params.cursor = cursor.map(str::to_owned);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::ListToolsRequest(rmcp::model::ListToolsRequest::with_param(
                        params,
                    )),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        let page: ListToolsResult = match result {
            ServerResult::ListToolsResult(page) => page,
            _ => return Err(Self::unexpected_result("tools/list")),
        };
        Ok(super::protocol::ListToolsResult {
            tools: page.tools.into_iter().map(convert_tool).collect(),
            next_cursor: page.next_cursor,
        })
    }

    /// `tools/list` with an optional pagination cursor.
    pub async fn list_tools(
        &self,
        cursor: Option<&str>,
    ) -> Result<super::protocol::ListToolsResult, McpError> {
        self.list_tools_with_options(cursor, &McpRequestOptions::default())
            .await
    }

    /// Every tool page, following `nextCursor` (upstream `listAll`): errors
    /// past the page cap and on a repeated cursor instead of silently
    /// truncating.
    pub async fn list_all_tools(&self) -> Result<Vec<McpTool>, McpError> {
        self.list_all_tools_with_options(&McpRequestOptions::default())
            .await
    }

    /// [`McpClient::list_all_tools`] with per-page request options.
    pub async fn list_all_tools_with_options(
        &self,
        options: &McpRequestOptions,
    ) -> Result<Vec<McpTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors: HashSet<String> = HashSet::new();
        for _ in 0..MAX_LIST_PAGES {
            let page = self
                .list_tools_with_options(cursor.as_deref(), options)
                .await?;
            tools.extend(page.tools);
            match page.next_cursor.filter(|next| !next.is_empty()) {
                Some(next) => {
                    if !seen_cursors.insert(next.clone()) {
                        return Err(McpError::Protocol(format!(
                            "tools/list returned duplicate cursor: {next}"
                        )));
                    }
                    cursor = Some(next);
                }
                None => return Ok(tools),
            }
        }
        Err(McpError::Protocol(format!(
            "tools/list exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    /// `tools/call`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: &JsonValue,
    ) -> Result<CallToolResult, McpError> {
        self.call_tool_with_options(name, arguments, &McpRequestOptions::default())
            .await
    }

    /// [`McpClient::call_tool`] with the caller's abort signal: cancelling it
    /// sends `notifications/cancelled` for this request and reports
    /// [`McpError::Aborted`] (upstream `callTool({ signal })`).
    pub async fn call_tool_with_signal(
        &self,
        name: &str,
        arguments: &JsonValue,
        signal: Option<&AbortSignal>,
    ) -> Result<CallToolResult, McpError> {
        let options = McpRequestOptions {
            signal: signal.cloned(),
            ..Default::default()
        };
        self.call_tool_with_options(name, arguments, &options).await
    }

    /// [`McpClient::call_tool`] with per-request progress, timeout, and abort
    /// options (upstream `callTool(name, args, options)`).
    pub async fn call_tool_with_options(
        &self,
        name: &str,
        arguments: &JsonValue,
        options: &McpRequestOptions,
    ) -> Result<CallToolResult, McpError> {
        let arguments = match arguments.as_object() {
            Some(object) => object.clone(),
            None => serde_json::Map::new(),
        };
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(params)),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        let response: CallToolResponse = match result {
            ServerResult::CallToolResult(result) => CallToolResponse::Complete(result),
            ServerResult::InputRequiredResult(result) => CallToolResponse::InputRequired(result),
            ServerResult::CreateTaskResult(result) => CallToolResponse::Task(result),
            _ => return Err(Self::unexpected_result("tools/call")),
        };
        match response {
            CallToolResponse::Complete(result) => Ok(convert_call_tool_result(result)),
            // The model-visible tool surface has no multi-round input flow;
            // an `input_required` answer is a protocol error like any other
            // unexpected result shape.
            _ => Err(Self::unexpected_result("tools/call")),
        }
    }

    /// `resources/list` with an optional pagination cursor and per-request
    /// options.
    ///
    /// Read-only: a transient HTTP failure is retried once (upstream
    /// `withClient(..., readOnly)`).
    pub async fn list_resources_with_options(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<ListResourcesResult, McpError> {
        with_read_only_retry(|| self.list_resources_page(cursor, options)).await
    }

    /// One `resources/list` round trip (no retry).
    ///
    /// rmcp's typed `Resource` requires `name`, so a server that omits it makes
    /// the whole page fall through to rmcp's catch-all result. Upstream accepts
    /// the entry and uses the URI as its name
    /// (`packages/mcp/src/client.ts:112-120`), so the raw page is parsed
    /// leniently here instead of failing every listed resource.
    async fn list_resources_page(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<ListResourcesResult, McpError> {
        let mut params = rmcp::model::PaginatedRequestParams::default();
        params.cursor = cursor.map(str::to_owned);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::ListResourcesRequest(
                        rmcp::model::ListResourcesRequest::with_param(params),
                    ),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        match result {
            ServerResult::ListResourcesResult(page) => Ok(ListResourcesResult {
                resources: page.resources.into_iter().map(convert_resource).collect(),
                next_cursor: page.next_cursor,
            }),
            ServerResult::CustomResult(value) => resources_page_from_raw(&value.0),
            _ => Err(Self::unexpected_result("resources/list")),
        }
    }

    /// `resources/list` with an optional pagination cursor.
    pub async fn list_resources(
        &self,
        cursor: Option<&str>,
    ) -> Result<ListResourcesResult, McpError> {
        self.list_resources_with_options(cursor, &McpRequestOptions::default())
            .await
    }

    /// `resources/templates/list` with an optional pagination cursor and
    /// per-request options (upstream `withoutTemplates`): a server that does
    /// not implement the method (JSON-RPC method-not-found) yields an empty
    /// listing instead of an error, so one template-less server does not fail
    /// a listing run.
    ///
    /// Read-only: a transient HTTP failure is retried once (upstream
    /// `withClient(..., readOnly)`).
    pub async fn list_resource_templates_with_options(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        with_read_only_retry(|| self.list_resource_templates_page(cursor, options)).await
    }

    /// One `resources/templates/list` round trip (no retry). Like
    /// [`McpClient::list_resources_page`], a template without the
    /// spec-required `name` is accepted and named by its `uriTemplate`.
    async fn list_resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let mut params = rmcp::model::PaginatedRequestParams::default();
        params.cursor = cursor.map(str::to_owned);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::ListResourceTemplatesRequest(
                        rmcp::model::ListResourceTemplatesRequest::with_param(params),
                    ),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await;
        let result = match result {
            Ok(result) => result,
            Err(McpError::Server { code, .. }) if code == crate::mcp::jsonrpc::METHOD_NOT_FOUND => {
                return Ok(ListResourceTemplatesResult::default());
            }
            Err(error) => return Err(error),
        };
        match result {
            ServerResult::ListResourceTemplatesResult(page) => Ok(ListResourceTemplatesResult {
                resource_templates: page
                    .resource_templates
                    .into_iter()
                    .map(convert_resource_template)
                    .collect(),
                next_cursor: page.next_cursor,
            }),
            ServerResult::CustomResult(value) => templates_page_from_raw(&value.0),
            _ => Err(Self::unexpected_result("resources/templates/list")),
        }
    }

    /// `resources/templates/list` with an optional pagination cursor.
    pub async fn list_resource_templates(
        &self,
        cursor: Option<&str>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.list_resource_templates_with_options(cursor, &McpRequestOptions::default())
            .await
    }

    /// `resources/read` with per-request options.
    ///
    /// Read-only: a transient HTTP failure is retried once (upstream
    /// `withClient(..., readOnly)`).
    pub async fn read_resource_with_options(
        &self,
        uri: &str,
        options: &McpRequestOptions,
    ) -> Result<ReadResourceResult, McpError> {
        with_read_only_retry(|| self.read_resource_once(uri, options)).await
    }

    /// One `resources/read` round trip (no retry).
    async fn read_resource_once(
        &self,
        uri: &str,
        options: &McpRequestOptions,
    ) -> Result<ReadResourceResult, McpError> {
        let params = ReadResourceRequestParams::new(uri);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::ReadResourceRequest(rmcp::model::ReadResourceRequest::new(
                        params,
                    )),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        match result {
            ServerResult::ReadResourceResult(result) => Ok(ReadResourceResult {
                contents: result
                    .contents
                    .into_iter()
                    .map(convert_resource_contents)
                    .collect(),
            }),
            _ => Err(Self::unexpected_result("resources/read")),
        }
    }

    /// `resources/read`.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        self.read_resource_with_options(uri, &McpRequestOptions::default())
            .await
    }

    /// `prompts/list` with an optional pagination cursor and per-request
    /// options.
    pub async fn list_prompts_with_options(
        &self,
        cursor: Option<&str>,
        options: &McpRequestOptions,
    ) -> Result<ListPromptsResult, McpError> {
        let mut params = rmcp::model::PaginatedRequestParams::default();
        params.cursor = cursor.map(str::to_owned);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::ListPromptsRequest(rmcp::model::ListPromptsRequest::with_param(
                        params,
                    )),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        let page = match result {
            ServerResult::ListPromptsResult(page) => page,
            _ => return Err(Self::unexpected_result("prompts/list")),
        };
        Ok(ListPromptsResult {
            prompts: page.prompts.into_iter().map(convert_prompt).collect(),
            next_cursor: page.next_cursor,
        })
    }

    /// `prompts/list` with an optional pagination cursor.
    pub async fn list_prompts(&self, cursor: Option<&str>) -> Result<ListPromptsResult, McpError> {
        self.list_prompts_with_options(cursor, &McpRequestOptions::default())
            .await
    }

    /// `prompts/get` with per-request options.
    pub async fn get_prompt_with_options(
        &self,
        name: &str,
        arguments: &JsonValue,
        options: &McpRequestOptions,
    ) -> Result<GetPromptResult, McpError> {
        let mut params = GetPromptRequestParams::new(name);
        params.arguments = Some(match arguments.as_object() {
            Some(object) => object.clone(),
            None => serde_json::Map::new(),
        });
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::GetPromptRequest(rmcp::model::GetPromptRequest::new(params)),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        match result {
            ServerResult::GetPromptResult(result) => Ok(GetPromptResult {
                description: result.description,
                messages: result
                    .messages
                    .into_iter()
                    .map(convert_prompt_message)
                    .collect(),
            }),
            _ => Err(Self::unexpected_result("prompts/get")),
        }
    }

    /// `prompts/get`.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: &JsonValue,
    ) -> Result<GetPromptResult, McpError> {
        self.get_prompt_with_options(name, arguments, &McpRequestOptions::default())
            .await
    }

    /// Send one arbitrary JSON-RPC request (upstream `McpClient.request`).
    /// Standard result shapes are returned as their JSON payload.
    pub async fn request(
        &self,
        method: &str,
        params: Option<JsonValue>,
        options: &McpRequestOptions,
    ) -> Result<JsonValue, McpError> {
        let request = rmcp::model::CustomRequest::new(method, params);
        let result = self
            .request_cancellable(options, |peer| async move {
                peer.send_cancellable_request(
                    ClientRequest::CustomRequest(request),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
            })
            .await?;
        serde_json::to_value(result)
            .map_err(|error| McpError::Protocol(format!("cannot encode result: {error}")))
    }

    /// Send one arbitrary JSON-RPC notification (upstream
    /// `McpClient.notify`). Notifications have no response and no deadline.
    pub async fn notify(&self, method: &str, params: Option<JsonValue>) -> Result<(), McpError> {
        let peer = self.peer().await?;
        peer.send_notification(ClientNotification::CustomNotification(
            rmcp::model::CustomNotification::new(method, params),
        ))
        .await
        .map_err(|error| {
            frame_cap_violation(&self.frame_cap).unwrap_or_else(|| http::map_service_error(error))
        })
    }

    /// Close the transport and sweep the process tree (idempotent). rmcp
    /// closes the write side and waits for the child; dropping the stdio
    /// transport runs its `ChildGuard`, which tree-kills and reaps the child.
    ///
    /// The pid-based sweep is therefore only the BACKSTOP for a close that did
    /// not finish within [`CLOSE_TIMEOUT`] (the transport, and its guard, are
    /// still alive then, so the pid cannot have been recycled). After a
    /// completed close the guard already reaped the child, and re-signaling
    /// `child_pid` could reach an unrelated process group.
    pub async fn close(&self) {
        let service = self.service.lock().await.take();
        let Some(mut service) = service else {
            return;
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, service.close())
            .await
            .is_err()
        {
            if let Some(pid) = self.child_pid {
                stdio::terminate_process_tree(pid);
            }
        }
    }
}

/// The typed frame-cap error when the stdio reader stopped at its bound.
/// rmcp reports a read failure as a closed transport (it logs the underlying
/// error and returns `None`); the shared flag turns that generic close into
/// the error the caller can act on.
fn frame_cap_violation(state: &Option<Arc<super::stdio::FrameCapState>>) -> Option<McpError> {
    state
        .as_ref()
        .filter(|state| state.overflowed())
        .map(|state| McpError::MessageTooLarge {
            limit: state.limit(),
        })
}

/// Validate the `initialize` result's required shape (upstream
/// `validateInitializeResult`): `protocolVersion` must be a string,/// `capabilities` an object, and `serverInfo` an object with string
/// `name` and `version`. rmcp enforces the same constraints by typed
/// deserialization during the handshake; this check is retained as the
/// upstream-parity reference used by the strict tests.
pub fn validate_initialize_result(result: &JsonValue) -> Result<(), McpError> {
    let invalid = || McpError::Protocol("Invalid MCP initialize result".to_owned());
    let Some(object) = result.as_object() else {
        return Err(invalid());
    };
    if !object
        .get("protocolVersion")
        .is_some_and(JsonValue::is_string)
    {
        return Err(invalid());
    }
    if !object.get("capabilities").is_some_and(JsonValue::is_object) {
        return Err(invalid());
    }
    let Some(server_info) = object.get("serverInfo").and_then(JsonValue::as_object) else {
        return Err(invalid());
    };
    if !server_info.get("name").is_some_and(JsonValue::is_string)
        || !server_info.get("version").is_some_and(JsonValue::is_string)
    {
        return Err(invalid());
    }
    if object
        .get("instructions")
        .is_some_and(|value| !value.is_string())
    {
        return Err(invalid());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rmcp -> pir protocol conversions
// ---------------------------------------------------------------------------

/// Convert rmcp's server capabilities (upstream `ServerCapabilities`).
fn convert_server_capabilities(
    capabilities: &rmcp::model::ServerCapabilities,
) -> ServerCapabilities {
    ServerCapabilities {
        tools: capabilities.tools.as_ref().map(|tools| ToolsCapability {
            list_changed: tools.list_changed,
        }),
        resources: capabilities
            .resources
            .as_ref()
            .map(|resources| ResourcesCapability {
                subscribe: resources.subscribe,
                list_changed: resources.list_changed,
            }),
        prompts: capabilities
            .prompts
            .as_ref()
            .map(|prompts| PromptsCapability {
                list_changed: prompts.list_changed,
            }),
        logging: capabilities
            .logging
            .as_ref()
            .map(|logging| JsonValue::Object(logging.clone())),
    }
}

/// Convert one rmcp tool (upstream `Tool`).
fn convert_tool(tool: rmcp::model::Tool) -> McpTool {
    McpTool {
        name: tool.name.to_string(),
        title: tool.title,
        description: tool.description.map(|description| description.to_string()),
        input_schema: JsonValue::Object((*tool.input_schema).clone()),
        output_schema: tool
            .output_schema
            .map(|schema| JsonValue::Object((*schema).clone())),
        annotations: tool
            .annotations
            .map(|annotations| super::protocol::ToolAnnotations {
                title: annotations.title,
                read_only_hint: annotations.read_only_hint,
                destructive_hint: annotations.destructive_hint,
                idempotent_hint: annotations.idempotent_hint,
                open_world_hint: annotations.open_world_hint,
            }),
    }
}

/// Convert one rmcp `tools/call` result (upstream `CallToolResult`).
fn convert_call_tool_result(result: rmcp::model::CallToolResult) -> CallToolResult {
    CallToolResult {
        content: result
            .content
            .into_iter()
            .map(convert_content_block)
            .collect(),
        structured_content: result.structured_content,
        is_error: result.is_error,
        meta: result.meta.map(|meta| JsonValue::Object(meta.0)),
    }
}

/// Convert one rmcp content block (upstream `ContentBlock`).
fn convert_content_block(block: rmcp::model::ContentBlock) -> ContentBlock {
    match block {
        rmcp::model::ContentBlock::Text(text) => ContentBlock {
            kind: "text".into(),
            text: Some(text.text),
            ..ContentBlock::default()
        },
        rmcp::model::ContentBlock::Image(image) => ContentBlock {
            kind: "image".into(),
            data: Some(image.data),
            mime_type: Some(image.mime_type),
            ..ContentBlock::default()
        },
        rmcp::model::ContentBlock::Audio(audio) => ContentBlock {
            kind: "audio".into(),
            data: Some(audio.data),
            mime_type: Some(audio.mime_type),
            ..ContentBlock::default()
        },
        rmcp::model::ContentBlock::Resource(embedded) => ContentBlock {
            kind: "resource".into(),
            resource: Some(convert_resource_contents(embedded.resource)),
            ..ContentBlock::default()
        },
        rmcp::model::ContentBlock::ResourceLink(resource) => ContentBlock {
            kind: "resource_link".into(),
            uri: Some(resource.uri),
            name: Some(resource.name),
            title: resource.title,
            description: resource.description,
            mime_type: resource.mime_type,
            size: resource.size,
            ..ContentBlock::default()
        },
        // Future block kinds degrade to an empty block (upstream falls
        // through to the default content conversion).
        _ => ContentBlock::default(),
    }
}

/// Convert one rmcp resource-contents value (upstream `TextResourceContents`
/// / `BlobResourceContents`).
fn convert_resource_contents(contents: rmcp::model::ResourceContents) -> ResourceContents {
    match contents {
        rmcp::model::ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => ResourceContents {
            uri,
            mime_type,
            text: Some(text),
            blob: None,
        },
        rmcp::model::ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => ResourceContents {
            uri,
            mime_type,
            text: None,
            blob: Some(blob),
        },
        _ => ResourceContents::default(),
    }
}

/// Convert one rmcp resource (upstream `Resource`, both for `resources/list`
/// and `resource_link` blocks).
fn convert_resource(resource: rmcp::model::Resource) -> McpResource {
    McpResource {
        uri: resource.uri,
        name: resource.name,
        title: resource.title,
        description: resource.description,
        mime_type: resource.mime_type,
        size: resource.size,
    }
}

/// Convert one rmcp resource template (`resources/templates/list`).
fn convert_resource_template(template: rmcp::model::ResourceTemplate) -> McpResourceTemplate {
    McpResourceTemplate {
        uri_template: template.uri_template,
        name: template.name,
        title: template.title,
        description: template.description,
        mime_type: template.mime_type,
    }
}

/// One optional string field of a raw JSON object.
fn raw_string(value: &JsonValue, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
}

/// One paginated page's `nextCursor`, normalized like upstream
/// `validateListPage`: `null` and `""` mean "no more pages".
fn raw_next_cursor(value: &JsonValue, method: &str) -> Result<Option<String>, McpError> {
    match value.get("nextCursor") {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(cursor)) if cursor.is_empty() => Ok(None),
        Some(JsonValue::String(cursor)) => Ok(Some(cursor.clone())),
        Some(_) => Err(McpError::Protocol(format!(
            "{method} returned an invalid cursor"
        ))),
    }
}

/// One `resources/list` entry parsed without rmcp's required `name`
/// (upstream `isResource`/`toResource`, `packages/mcp/src/client.ts:112-120`):
/// a missing name falls back to the URI, an explicit non-string name is an
/// invalid entry.
fn resource_from_raw(value: &JsonValue) -> Option<McpResource> {
    let uri = value.get("uri").and_then(JsonValue::as_str)?;
    let name = match value.get("name") {
        None => uri.to_owned(),
        Some(JsonValue::String(name)) => name.clone(),
        Some(_) => return None,
    };
    Some(McpResource {
        uri: uri.to_owned(),
        name,
        title: raw_string(value, "title"),
        description: raw_string(value, "description"),
        mime_type: raw_string(value, "mimeType"),
        size: value.get("size").and_then(JsonValue::as_u64),
    })
}

/// A raw `resources/list` page (the lenient path used when rmcp's typed
/// `Resource` rejects an entry, commonly one without `name`).
fn resources_page_from_raw(value: &JsonValue) -> Result<ListResourcesResult, McpError> {
    let items = value
        .get("resources")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            McpError::Protocol("resources/list returned an unexpected result shape".to_owned())
        })?;
    let mut resources = Vec::with_capacity(items.len());
    for item in items {
        resources.push(resource_from_raw(item).ok_or_else(|| {
            McpError::Protocol("resources/list returned an invalid entry".to_owned())
        })?);
    }
    Ok(ListResourcesResult {
        resources,
        next_cursor: raw_next_cursor(value, "resources/list")?,
    })
}

/// One `resources/templates/list` entry parsed without rmcp's required
/// `name`; a missing name falls back to the `uriTemplate`.
fn template_from_raw(value: &JsonValue) -> Option<McpResourceTemplate> {
    let uri_template = value.get("uriTemplate").and_then(JsonValue::as_str)?;
    let name = match value.get("name") {
        None => uri_template.to_owned(),
        Some(JsonValue::String(name)) => name.clone(),
        Some(_) => return None,
    };
    Some(McpResourceTemplate {
        uri_template: uri_template.to_owned(),
        name,
        title: raw_string(value, "title"),
        description: raw_string(value, "description"),
        mime_type: raw_string(value, "mimeType"),
    })
}

/// A raw `resources/templates/list` page (see [`resources_page_from_raw`]).
fn templates_page_from_raw(value: &JsonValue) -> Result<ListResourceTemplatesResult, McpError> {
    let items = value
        .get("resourceTemplates")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            McpError::Protocol(
                "resources/templates/list returned an unexpected result shape".to_owned(),
            )
        })?;
    let mut resource_templates = Vec::with_capacity(items.len());
    for item in items {
        resource_templates.push(template_from_raw(item).ok_or_else(|| {
            McpError::Protocol("resources/templates/list returned an invalid entry".to_owned())
        })?);
    }
    Ok(ListResourceTemplatesResult {
        resource_templates,
        next_cursor: raw_next_cursor(value, "resources/templates/list")?,
    })
}

/// Convert one rmcp prompt (upstream `Prompt`).
fn convert_prompt(prompt: rmcp::model::Prompt) -> McpPrompt {
    McpPrompt {
        name: prompt.name,
        title: prompt.title,
        description: prompt.description,
        arguments: prompt.arguments.map(|arguments| {
            arguments
                .into_iter()
                .map(|argument| super::protocol::PromptArgument {
                    name: argument.name,
                    description: argument.description,
                    required: argument.required,
                })
                .collect()
        }),
    }
}

/// Convert one rmcp prompt message (upstream `PromptMessage`).
fn convert_prompt_message(message: rmcp::model::PromptMessage) -> PromptMessage {
    let role = match message.role {
        rmcp::model::Role::User => "user",
        rmcp::model::Role::Assistant => "assistant",
    };
    PromptMessage {
        role: role.to_owned(),
        content: Some(convert_content_block(message.content)),
    }
}
