//! `streamProxy`: route model requests through an application server.
//!
//! Upstream export: `packages/agent/src/proxy.ts` (`streamProxy`), re-exported
//! from `@earendil-works/pi-agent-core`. Apps that keep provider credentials on
//! a server use it as the agent's `streamFn`: every request is POSTed to
//! `{proxyUrl}/api/stream` with the app's bearer token, the server calls the
//! provider, and the response is an SSE stream of "proxy events" — the
//! assistant event protocol with the cumulative `partial` field stripped to
//! save bandwidth. The client rebuilds the partial message from those events
//! (upstream `processProxyEvent`).
//!
//! pir's equivalent lives on the provider seam the SDK already exposes:
//! [`stream_proxy`] is the direct analogue of upstream's function (same three
//! arguments), and [`ProxyProvider`] adapts it to
//! [`Provider`](crate::ai::provider::Provider) so it can be handed to
//! [`AgentBuilder::with_registry`](crate::sdk::AgentBuilder::with_registry) or
//! [`AgentBuilder::proxy`](crate::sdk::AgentBuilder::proxy).
//!
//! The wire protocol is fixed by upstream:
//!
//! ```text
//! POST {proxyUrl}/api/stream
//! Authorization: Bearer {authToken}
//! Content-Type: application/json
//!
//! { "model": {...}, "context": {...}, "options": {...} }
//! ```
//!
//! and the response body is `data: {proxy event}` lines. `options` carries the
//! same serializable subset upstream forwards (`temperature`, `samplingParams`,
//! `maxTokens`, `reasoning`, `cacheRetention`, `sessionId`, `headers`,
//! `metadata`, `thinkingBudgets`, `maxRetryDelayMs`); everything else in
//! [`StreamOptions`] (the abort signal, retry budget, transport hooks) stays
//! client-local.
//!
//! The decoder is deliberately local rather than reusing the `pi-messages`
//! adapter: that adapter is compiled only under `provider-openai`, while this
//! export must exist in every build, and the proxy protocol differs where it
//! matters (`providerThinkingLevel` on the terminal event, `contentSignature`
//! on thinking blocks, and the "connection closed without a terminal event"
//! error). The pieces that are protocol-neutral are reused: the shared SSE
//! line framing, the streaming JSON mirror for tool-call arguments, the
//! backpressured chunk sink, and the transport's retry/abort machinery.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context,
    ErrorReason, Model, ProviderHeaders, StopReason, StreamOptions, TerminalReason, TextContent,
    ThinkingBudgets, ThinkingContent, ThinkingLevel, ToolCall, Usage,
};

/// Path appended to the proxy base URL (upstream `streamProxy`).
const PROXY_STREAM_PATH: &str = "/api/stream";

/// Error reported when the server closes the stream without a terminal event
/// (upstream `proxy.ts`). Kept verbatim so consumers can match on it.
pub const PROXY_CONNECTION_CLOSED_MESSAGE: &str =
    "Connection closed by proxy server before the response completed";

/// Transport half of upstream `ProxyStreamOptions`: where the server is and
/// which bearer token it issued.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    /// Proxy server base URL (e.g. `https://genai.example.com`); requests go
    /// to `{proxy_url}/api/stream`.
    pub proxy_url: String,
    /// Bearer token the proxy server authenticates the client with.
    pub auth_token: String,
}

impl ProxyEndpoint {
    /// Configure a proxy endpoint.
    pub fn new(proxy_url: impl Into<String>, auth_token: impl Into<String>) -> Self {
        Self {
            proxy_url: proxy_url.into(),
            auth_token: auth_token.into(),
        }
    }

    /// The dispatch URL (`{proxy_url}/api/stream`).
    pub fn stream_url(&self) -> String {
        format!(
            "{}{PROXY_STREAM_PATH}",
            self.proxy_url.trim_end_matches('/')
        )
    }
}

/// Options for [`stream_proxy`], the Rust equivalent of upstream
/// `ProxyStreamOptions`.
///
/// Upstream extends its serializable stream options with `proxyUrl`,
/// `authToken`, and `signal`. Here the proxy coordinates live on the struct
/// and the remaining knobs are the crate's [`StreamOptions`]: the
/// serializable subset is forwarded to the server, while `signal`, retries,
/// and hooks stay local (see the module docs).
#[derive(Clone, Default)]
pub struct ProxyStreamOptions {
    /// Proxy server base URL.
    pub proxy_url: String,
    /// Bearer token for the proxy server.
    pub auth_token: String,
    /// Local stream options; only the proxy-serializable subset is sent.
    pub options: StreamOptions,
    /// Transport client override (`None`: the shared default provider client).
    /// Symmetric with [`ProxyProvider::with_client`]: tests inject a scripted
    /// transport, and embedders that need a custom transport, proxy, or retry
    /// defaults supply their own client instead of the process-wide one.
    client: Option<ProviderRequestClient>,
}

impl std::fmt::Debug for ProxyEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The bearer token is a credential: never print it, only whether one
        // is set (same rule as `ProxyStreamOptions` below and the crate's
        // other secret-bearing `Debug` impls).
        formatter
            .debug_struct("ProxyEndpoint")
            .field("proxy_url", &self.proxy_url)
            .field("auth_token", &TokenPresence(!self.auth_token.is_empty()))
            .finish()
    }
}

/// Renders "set"/"unset" instead of a secret in `Debug` output.
struct TokenPresence(bool);

impl std::fmt::Debug for TokenPresence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(if self.0 { "<redacted>" } else { "<unset>" })
    }
}

impl std::fmt::Debug for ProxyStreamOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted for the same reason as `ProxyEndpoint`: this struct owns a
        // bearer token, and `StreamOptions` carries a per-request `api_key`
        // that a derived impl would print as well.
        formatter
            .debug_struct("ProxyStreamOptions")
            .field("proxy_url", &self.proxy_url)
            .field("auth_token", &TokenPresence(!self.auth_token.is_empty()))
            .field("options", &RedactedStreamOptions(&self.options))
            .field("client", &self.client.is_some())
            .finish()
    }
}

/// `StreamOptions` with its `api_key` masked (the proxy never forwards it,
/// but a derived `Debug` would still print the credential).
struct RedactedStreamOptions<'a>(&'a StreamOptions);

impl std::fmt::Debug for RedactedStreamOptions<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Clone-and-mask rather than mirroring every field by hand: a field
        // added to `StreamOptions` later is still reported, and can never be
        // silently dropped from this diagnostic.
        let mut options = self.0.clone();
        options.api_key = options.api_key.as_ref().map(|_| "<redacted>".to_owned());
        std::fmt::Debug::fmt(&options, formatter)
    }
}

impl ProxyStreamOptions {
    /// Configure the proxy coordinates with default local stream options.
    pub fn new(proxy_url: impl Into<String>, auth_token: impl Into<String>) -> Self {
        Self {
            proxy_url: proxy_url.into(),
            auth_token: auth_token.into(),
            options: StreamOptions::default(),
            client: None,
        }
    }

    /// Attach local stream options (abort signal, retry budget, hooks, and
    /// the forwarded request knobs).
    #[must_use]
    pub fn with_stream_options(mut self, options: StreamOptions) -> Self {
        self.options = options;
        self
    }

    /// Replace the HTTP client (tests inject a scripted transport; embedders
    /// may tune retries, timeouts, and the transport itself).
    #[must_use]
    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = Some(client);
        self
    }

    fn endpoint(&self) -> ProxyEndpoint {
        ProxyEndpoint {
            proxy_url: self.proxy_url.clone(),
            auth_token: self.auth_token.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// The proxy-serializable subset of [`StreamOptions`] (upstream
/// `ProxySerializableStreamOptions`).
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyRequestOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sampling_params: Option<BTreeMap<String, JsonValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_retention: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<ProviderHeaders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<BTreeMap<String, JsonValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budgets: Option<ThinkingBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retry_delay_ms: Option<u64>,
}

impl ProxyRequestOptions {
    fn from_stream_options(options: &StreamOptions) -> Self {
        Self {
            temperature: options.temperature,
            sampling_params: options.sampling_params.clone(),
            max_tokens: options.max_tokens,
            reasoning: options.reasoning,
            cache_retention: options.cache_retention.map(|retention| match retention {
                CacheRetention::None => "none",
                CacheRetention::Short => "short",
                CacheRetention::Long => "long",
            }),
            session_id: options.session_id.clone(),
            headers: options.headers.clone(),
            metadata: options.metadata.clone(),
            thinking_budgets: options.thinking_budgets,
            max_retry_delay_ms: options.max_retry_delay_ms,
        }
    }
}

#[derive(serde::Serialize)]
struct ProxyRequest<'a> {
    model: &'a Model,
    context: &'a Context,
    options: ProxyRequestOptions,
}

/// Build the request body and headers (upstream `streamProxy`'s fetch call).
fn build_proxy_request<'a>(
    model: &'a Model,
    context: &'a Context,
    options: &StreamOptions,
    auth_token: &str,
) -> (ProxyRequest<'a>, BTreeMap<String, String>) {
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_owned(), format!("Bearer {auth_token}"));
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    (
        ProxyRequest {
            model,
            context,
            options: ProxyRequestOptions::from_stream_options(options),
        },
        headers,
    )
}

// ---------------------------------------------------------------------------
// Wire events (upstream `ProxyAssistantMessageEvent`)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProxyEvent {
    Start,
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "contentSignature", default)]
        content_signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "contentSignature", default)]
        content_signature: Option<String>,
    },
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    ToolcallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
    },
    Done {
        reason: String,
        usage: Usage,
        #[serde(rename = "providerThinkingLevel", default)]
        provider_thinking_level: Option<String>,
    },
    Error {
        reason: String,
        #[serde(rename = "errorMessage", default)]
        error_message: Option<String>,
        usage: Usage,
        #[serde(rename = "providerThinkingLevel", default)]
        provider_thinking_level: Option<String>,
    },
    /// An event type this build does not know. Upstream's `processProxyEvent`
    /// switches on the type and simply does nothing for anything unrecognized
    /// (so a newer server can add event types without breaking older clients),
    /// and the frame's remaining fields are ignored with it. A frame that is
    /// not a JSON object, or a KNOWN type with a malformed payload, still
    /// fails the stream: that is a protocol error, not a version skew.
    #[serde(other)]
    Unknown,
}

/// Map the proxy's terminal success reason (`stop|length|toolUse`).
fn terminal_reason(reason: &str) -> TerminalReason {
    match reason {
        "length" => TerminalReason::Length,
        "toolUse" => TerminalReason::ToolUse,
        _ => TerminalReason::Stop,
    }
}

/// Parse the incrementally accumulated tool-call arguments (upstream
/// `parseStreamingJson(...) || {}`); malformed fragments mirror `{}` until
/// the authoritative `toolcall_end` arrives.
fn parse_tool_arguments(partial: &str) -> BTreeMap<String, JsonValue> {
    serde_json::from_value::<BTreeMap<String, JsonValue>>(
        crate::ai::api::openai_responses::shared::parse_partial_json(partial),
    )
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Decoder: proxy events -> assistant message events
// ---------------------------------------------------------------------------

/// Rebuilds the cumulative assistant message from partial-stripped proxy
/// events (upstream `processProxyEvent` + the `partial` bookkeeping in
/// `streamProxy`).
struct ProxyDecoder {
    /// The accumulated message behind an `Arc`, mutated copy-on-write so each
    /// streamed event hands out a snapshot instead of a deep clone.
    message: Arc<AssistantMessage>,
    /// Wire `contentIndex` of every projected block, ascending, so a block
    /// addressed by index maps to its dense projection slot.
    slot_keys: Vec<usize>,
    /// Raw tool-call argument fragments per wire content index.
    tool_json: BTreeMap<usize, String>,
    /// Bounded-cadence mirror refresh for the parsed arguments.
    tool_mirror: BTreeMap<usize, crate::ai::api::partial_mirror::MirrorCadence>,
    /// `true` once a terminal `done`/`error` was applied.
    terminated: bool,
    /// Trailing bytes of an SSE line not yet newline-terminated.
    buffer: Vec<u8>,
}

impl ProxyDecoder {
    fn new(model: &Model) -> Self {
        Self {
            message: Arc::new(crate::ai::api::base_message(model)),
            slot_keys: Vec::new(),
            tool_json: BTreeMap::new(),
            tool_mirror: BTreeMap::new(),
            terminated: false,
            buffer: Vec::new(),
        }
    }

    /// Mutable access to the accumulator, copy-on-write while a snapshot is
    /// outstanding.
    #[inline]
    fn message_mut(&mut self) -> &mut AssistantMessage {
        Arc::make_mut(&mut self.message)
    }

    /// O(1) snapshot of the message accumulated so far.
    fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    fn slot_of(&self, key: usize) -> Option<usize> {
        self.slot_keys.binary_search(&key).ok()
    }

    /// Insert (at its ascending-key slot) or replace the single block keyed
    /// `key`.
    fn put_block(&mut self, key: usize, block: ContentBlock) {
        match self.slot_keys.binary_search(&key) {
            Ok(slot) => self.message_mut().content[slot] = block,
            Err(slot) => {
                self.slot_keys.insert(slot, key);
                self.message_mut().content.insert(slot, block);
            }
        }
    }

    /// Apply one wire event, returning the assistant events it produces.
    fn apply(&mut self, event: ProxyEvent) -> AiResult<Vec<AssistantMessageEvent>> {
        let mut out = Vec::new();
        match event {
            ProxyEvent::Start => out.push(AssistantMessageEvent::Start {
                partial: self.snapshot(),
            }),
            ProxyEvent::TextStart { content_index } => {
                self.put_block(
                    content_index,
                    ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                );
                out.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::TextDelta {
                content_index,
                delta,
            } => {
                let Some(slot) = self.slot_of(content_index) else {
                    return Err(proxy_protocol_error("text_delta for missing content"));
                };
                match self.message_mut().content.get_mut(slot) {
                    Some(ContentBlock::Text(text)) => text.text.push_str(&delta),
                    _ => return Err(proxy_protocol_error("text_delta for non-text content")),
                }
                out.push(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::TextEnd {
                content_index,
                content_signature,
            } => {
                let Some(slot) = self.slot_of(content_index) else {
                    return Err(proxy_protocol_error("text_end for missing content"));
                };
                let content = match self.message_mut().content.get_mut(slot) {
                    Some(ContentBlock::Text(text)) => {
                        text.text_signature = content_signature;
                        text.text.clone()
                    }
                    _ => return Err(proxy_protocol_error("text_end for non-text content")),
                };
                out.push(AssistantMessageEvent::TextEnd {
                    content_index,
                    content,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::ThinkingStart { content_index } => {
                self.put_block(
                    content_index,
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }),
                );
                out.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::ThinkingDelta {
                content_index,
                delta,
            } => {
                let Some(slot) = self.slot_of(content_index) else {
                    return Err(proxy_protocol_error("thinking_delta for missing content"));
                };
                match self.message_mut().content.get_mut(slot) {
                    Some(ContentBlock::Thinking(thinking)) => thinking.thinking.push_str(&delta),
                    _ => {
                        return Err(proxy_protocol_error(
                            "thinking_delta for non-thinking content",
                        ))
                    }
                }
                out.push(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::ThinkingEnd {
                content_index,
                content_signature,
            } => {
                let Some(slot) = self.slot_of(content_index) else {
                    return Err(proxy_protocol_error("thinking_end for missing content"));
                };
                let content = match self.message_mut().content.get_mut(slot) {
                    Some(ContentBlock::Thinking(thinking)) => {
                        thinking.thinking_signature = content_signature;
                        thinking.thinking.clone()
                    }
                    _ => {
                        return Err(proxy_protocol_error(
                            "thinking_end for non-thinking content",
                        ))
                    }
                };
                out.push(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::ToolcallStart {
                content_index,
                id,
                tool_name,
            } => {
                self.tool_json.insert(content_index, String::new());
                self.put_block(
                    content_index,
                    ContentBlock::ToolCall(ToolCall {
                        id,
                        name: tool_name,
                        arguments: BTreeMap::new(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    }),
                );
                out.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::ToolcallDelta {
                content_index,
                delta,
            } => {
                let Some(slot) = self.slot_of(content_index) else {
                    return Err(proxy_protocol_error("toolcall_delta for missing content"));
                };
                if !matches!(
                    self.message.content.get(slot),
                    Some(ContentBlock::ToolCall(_))
                ) {
                    return Err(proxy_protocol_error(
                        "toolcall_delta for non-toolCall content",
                    ));
                }
                self.tool_json
                    .entry(content_index)
                    .or_default()
                    .push_str(&delta);
                let refresh = self
                    .tool_mirror
                    .entry(content_index)
                    .or_default()
                    .record(&delta);
                if refresh {
                    let fragment = self
                        .tool_json
                        .get(&content_index)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(ContentBlock::ToolCall(call)) =
                        self.message_mut().content.get_mut(slot)
                    {
                        call.arguments = parse_tool_arguments(&fragment);
                    }
                }
                out.push(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::ToolcallEnd {
                content_index,
                tool_call,
            } => {
                // The end frame is authoritative (it is the only place extra
                // metadata such as `namespace` arrives); when the addressed
                // block is not a tool call the event produces nothing, like
                // upstream's `processProxyEvent`.
                let is_tool_call = self.slot_of(content_index).is_some_and(|slot| {
                    matches!(
                        self.message.content.get(slot),
                        Some(ContentBlock::ToolCall(_))
                    )
                });
                if !is_tool_call {
                    return Ok(out);
                }
                self.tool_json.remove(&content_index);
                self.tool_mirror.remove(&content_index);
                self.put_block(content_index, ContentBlock::ToolCall(tool_call.clone()));
                out.push(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial: self.snapshot(),
                });
            }
            ProxyEvent::Done {
                reason,
                usage,
                provider_thinking_level,
            } => {
                let message = self.message_mut();
                message.usage = usage;
                message.stop_reason = match terminal_reason(&reason) {
                    TerminalReason::Length => StopReason::Length,
                    TerminalReason::ToolUse => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
                message.provider_thinking_level = provider_thinking_level;
                self.terminated = true;
                out.push(AssistantMessageEvent::Done {
                    reason: terminal_reason(&reason),
                    message: (*self.message).clone(),
                });
            }
            ProxyEvent::Error {
                reason,
                error_message,
                usage,
                provider_thinking_level,
            } => {
                let aborted = reason == "aborted";
                let message = self.message_mut();
                message.usage = usage;
                message.error_message = error_message;
                message.provider_thinking_level = provider_thinking_level;
                message.stop_reason = if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                self.terminated = true;
                out.push(AssistantMessageEvent::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: (*self.message).clone(),
                });
            }
            ProxyEvent::Unknown => {}
        }
        Ok(out)
    }

    /// Feed raw SSE bytes; completed `data:` lines are applied in order.
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result =
            crate::ai::api::sse_feed_lines(chunk, &mut buffer, |line| self.handle_line(line, out));
        self.buffer = buffer;
        result
    }

    /// Apply one complete SSE line.
    fn handle_line(&mut self, line: &str, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let event: ProxyEvent = serde_json::from_str(data)
            .map_err(|error| proxy_protocol_error(&format!("bad proxy event: {error}")))?;
        out.extend(self.apply(event)?);
        Ok(())
    }

    /// Flush a trailing event not newline-terminated, then surface the
    /// upstream "connection closed" error when no terminal event arrived.
    /// `aborted` comes from the request signal, not from this decoder: a clean
    /// EOF racing a caller abort is still the caller's abort (upstream
    /// `output.stopReason = options?.signal?.aborted ? "aborted" : "error"`).
    fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>, aborted: bool) -> AiResult<()> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            self.handle_line(&line, out)?;
        }
        if !self.terminated {
            let message = self.message_mut();
            message.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            message.error_message = Some(PROXY_CONNECTION_CLOSED_MESSAGE.to_owned());
            self.terminated = true;
            out.push(AssistantMessageEvent::Error {
                reason: if aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                },
                error: (*self.message).clone(),
            });
        }
        Ok(())
    }

    /// Apply a transport failure to the accumulated partial (upstream's
    /// `catch` block) and return the terminal message for the error event.
    fn fail(&mut self, aborted: bool, message_text: &str) -> AssistantMessage {
        let message = self.message_mut();
        message.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        message.error_message = Some(message_text.to_owned());
        self.terminated = true;
        (*self.message).clone()
    }
}

fn proxy_protocol_error(message: &str) -> AiError {
    AiError::NonRetryable(format!("proxy: {message}"))
}

/// Rewrite a transport HTTP failure into the upstream proxy wording: the
/// transport reports `HTTP {status}: {body}`; upstream reports
/// `Proxy error: {body.error}` (or `Proxy error: {status} {statusText}`).
/// Public wording is documented but not load-bearing.
fn proxy_error_message(error: &AiError) -> String {
    if let AiError::NonRetryable(message) = error {
        if let Some(rest) = message.strip_prefix("HTTP ") {
            let (status, body) = rest.split_once(": ").unwrap_or((rest, ""));
            if let Ok(value) = serde_json::from_str::<JsonValue>(body) {
                if let Some(error_text) = value.get("error").and_then(JsonValue::as_str) {
                    return format!("Proxy error: {error_text}");
                }
            }
            return format!("Proxy error: {status}");
        }
    }
    error.user_message()
}

// ---------------------------------------------------------------------------
// Stream driver
// ---------------------------------------------------------------------------

async fn proxy_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    endpoint: &ProxyEndpoint,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> AiResult<()> {
    let (body, headers) = build_proxy_request(model, context, options, &endpoint.auth_token);
    let url = endpoint.stream_url();
    let mut decoder = ProxyDecoder::new(model);
    let outcome = {
        let mut on_chunk = crate::ai::api::EventChunkSink::new(
            |chunk: &[u8], ready: &mut Vec<AssistantMessageEvent>| {
                // Every consumer handle is gone: release the HTTP response
                // instead of draining a body nobody reads (normal abandon).
                if sink.is_abandoned() {
                    return Err(AiError::Aborted);
                }
                decoder.feed(chunk, ready)
            },
            sink,
        );
        client
            .post_json_streaming_with(
                &url,
                &headers,
                &body,
                options.signal.as_ref(),
                &options.hooks,
                &mut on_chunk,
            )
            .await
    };
    match outcome {
        Ok(_) => {
            let mut tail = Vec::new();
            // A trailing (not newline-terminated) frame that fails to decode
            // must not discard the partial: deliver the events decoded before
            // the bad frame, then report the failure through the same
            // `decoder.fail` path a transport error uses. (The old `?` left
            // `finish_provider_stream` to build an EMPTY error message, so
            // every delta that had already streamed was lost.)
            let aborted = signal_aborted(options.signal.as_ref());
            let decode_error = decoder.finish(&mut tail, aborted).err();
            for event in tail {
                if !sink.push_await(event).await {
                    // Abandoned: nothing to report to a reader that is gone.
                    return Ok(());
                }
            }
            if let Some(error) = decode_error {
                push_failure(&mut decoder, sink, options.signal.as_ref(), &error).await;
            }
            Ok(())
        }
        Err(error) => {
            // One transport failure ends the run: the accumulated partial is
            // reported with the error, never silently discarded. An abandoned
            // stream has no reader, so there is nothing to report.
            if sink.is_abandoned() {
                return Ok(());
            }
            push_failure(&mut decoder, sink, options.signal.as_ref(), &error).await;
            Ok(())
        }
    }
}

/// True when the caller's request signal is aborted at finish time.
fn signal_aborted(signal: Option<&AbortSignal>) -> bool {
    signal.is_some_and(AbortSignal::is_aborted)
}

/// Terminate the stream through [`ProxyDecoder::fail`]: the accumulated
/// partial is reported as the terminal `Error` event with `error`'s wording,
/// so a failed decode never discards what streamed before it.
///
/// The classification is the shared
/// [`terminal_error_classification`](crate::ai::api::terminal_error_classification):
/// the signal decides, so the keepalive-storm / clean-EOF races that surface a
/// non-abort error still record the caller's abort.
async fn push_failure(
    decoder: &mut ProxyDecoder,
    sink: &AssistantMessageEventStream,
    signal: Option<&AbortSignal>,
    error: &AiError,
) {
    let (reason, _) = crate::ai::api::terminal_error_classification(error, signal);
    let aborted = matches!(reason, ErrorReason::Aborted);
    let message_text = proxy_error_message(error);
    let message = decoder.fail(aborted, &message_text);
    sink.push_await(AssistantMessageEvent::Error {
        reason,
        error: message,
    })
    .await;
}

/// Stream one model request through a proxy server — the Rust equivalent of
/// upstream `streamProxy(model, context, options)`.
///
/// The server owns provider credentials: `options.auth_token` is sent as the
/// bearer token, the full model and context travel in the request body, and
/// the response is decoded with the client-side partial reconstruction the
/// proxy protocol requires (the server strips cumulative `partial` snapshots).
/// Terminal semantics match upstream: a `done`/`error` event ends the stream,
/// and a clean EOF without one surfaces as an error carrying
/// [`PROXY_CONNECTION_CLOSED_MESSAGE`].
///
/// The transport is the shared provider client (so retries, timeouts, and the
/// settings proxy apply exactly like a direct provider request); pass
/// [`ProxyStreamOptions::with_client`] to supply a different one.
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> Result<(), pir::sdk::SdkError> {
/// use pir::ai::types::{Context, Model};
/// use pir::sdk::{stream_proxy, ProxyStreamOptions};
/// # let model: Model = unimplemented!();
/// # let context = Context::default();
/// let events = stream_proxy(
///     &model,
///     context,
///     ProxyStreamOptions::new("https://genai.example.com", "proxy-token"),
/// );
/// # let _ = events;
/// # Ok(())
/// # }
/// ```
pub fn stream_proxy(
    model: &Model,
    context: Context,
    options: ProxyStreamOptions,
) -> AssistantMessageEventStream {
    let output = AssistantMessageEventStream::new();
    // Producer handle, not a consumer clone: a consumer that abandons the
    // stream must be observable so this task cannot park forever.
    let sink = output.producer();
    let model = model.clone();
    let endpoint = options.endpoint();
    let stream_options = options.options;
    // A caller-supplied client wins over the shared default (the same seam
    // `ProxyProvider::with_client` exposes, so both entry points accept an
    // injected transport). Each call applies the per-call retry overrides to
    // ITS OWN client clone: `max_retries`/`max_retry_delay_ms` on
    // [`StreamOptions`] are the same local knobs every adapter honors, and a
    // shared client must not accumulate them across calls.
    let client = match options.client {
        Some(client) => client,
        None => match ProviderRequestClient::with_default_config() {
            Ok(client) => client,
            Err(error) => {
                let message = crate::ai::provider::error_message(&model, &error.user_message());
                sink.push(AssistantMessageEventStream::error_event(
                    ErrorReason::Error,
                    message,
                ));
                sink.end(None);
                return output;
            }
        },
    };
    let mut client = client;
    crate::ai::api::apply_stream_retry_overrides(&mut client, &stream_options);
    tokio::spawn(async move {
        let result =
            proxy_stream(&client, &model, &context, &endpoint, &stream_options, &sink).await;
        if sink.is_abandoned() {
            // Normal abandon: nobody is reading, so report nothing.
            sink.end(None);
            return;
        }
        crate::ai::api::finish_provider_stream(
            &sink,
            result,
            &model,
            stream_options.signal.as_ref(),
        );
    });
    output
}

/// A [`Provider`] that routes every request through a proxy server
/// (upstream's `streamProxy` used as the agent's `streamFn`).
///
/// The provider is keyless at the registry level (`api_key_auth` is empty and
/// the proxy token travels on [`ProxyEndpoint`]), so it fits the SDK's
/// `with_registry` dispatch, which streams without consulting configured auth.
/// Its [`Provider::id`] is the first model's provider id: register it under
/// the provider the served models name.
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> Result<(), pir::sdk::SdkError> {
/// use pir::ai::types::Model;
/// use pir::sdk::{AgentBuilder, ProxyEndpoint, ProxyProvider};
/// # let model: Model = unimplemented!();
/// let provider = ProxyProvider::new(
///     ProxyEndpoint::new("https://genai.example.com", "proxy-token"),
///     vec![model.clone()],
/// )?;
/// let agent = AgentBuilder::new()
///     .proxy(provider, model)
///     .no_session()
///     .build()
///     .await?;
/// # let _ = agent;
/// # Ok(())
/// # }
/// ```
pub struct ProxyProvider {
    id: String,
    endpoint: ProxyEndpoint,
    models: Vec<Model>,
    client: ProviderRequestClient,
}

impl ProxyProvider {
    /// Build a proxy provider serving `models` (all must name the same
    /// provider).
    pub fn new(endpoint: ProxyEndpoint, models: Vec<Model>) -> AiResult<Self> {
        let Some(first) = models.first() else {
            return Err(AiError::NonRetryable(
                "ProxyProvider requires at least one model".to_owned(),
            ));
        };
        let id = first.provider.clone();
        if let Some(other) = models.iter().find(|model| model.provider != id) {
            return Err(AiError::NonRetryable(format!(
                "ProxyProvider models must share one provider id (`{id}` vs `{}`)",
                other.provider
            )));
        }
        Ok(Self {
            id,
            endpoint,
            models,
            client: ProviderRequestClient::with_default_config()?,
        })
    }

    /// Replace the HTTP client (tests inject a scripted transport; embedders
    /// may tune retries and timeouts).
    #[must_use]
    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = client;
        self
    }

    /// The proxy coordinates this provider streams through.
    pub fn endpoint(&self) -> &ProxyEndpoint {
        &self.endpoint
    }
}

impl Provider for ProxyProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        "Proxy"
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // The proxy server owns provider credentials; the bearer token is
        // carried by the endpoint, not by the registry's auth resolution.
        Vec::new()
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_shared(model, Arc::new(context), options)
    }

    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let output = AssistantMessageEventStream::new();
        // Producer handle, not a consumer clone: a consumer that abandons the
        // stream must be observable so this task cannot park forever.
        let sink = output.producer();
        let mut client = self.client.clone();
        crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
        let model = model.clone();
        let endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            let result = proxy_stream(&client, &model, &context, &endpoint, &options, &sink).await;
            if sink.is_abandoned() {
                // Normal abandon: nobody is reading, so report nothing.
                sink.end(None);
                return;
            }
            crate::ai::api::finish_provider_stream(&sink, result, &model, options.signal.as_ref());
        });
        output
    }
}
