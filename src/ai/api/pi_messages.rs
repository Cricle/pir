//! The `pi-messages` provider API (the Radius gateway protocol, upstream
//! `packages/ai/src/api/pi-messages.ts`): `POST {baseUrl}/messages` with
//! `{model, context: {messages}, options}` and an SSE stream of
//! index-addressed content events (`text_delta` / `toolcall_delta` / ...).
//!
//! The wire event protocol mirrors pir's [`AssistantMessageEvent`] (the Rust
//! port of upstream's assistant event stream), so the adapter is a thin
//! SSE→event converter: the transcript is forwarded verbatim (no message
//! transformation — tools travel inside the messages as `toolsAdded` system
//! messages; upstream's `TranscriptContext` has no `tools` field), usage
//! arrives fully computed in the terminal event, and `Bearer` auth is
//! attached by the registry (env key / Radius OAuth) before dispatch.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use serde_json::Value;

use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;

/// The Radius provider's API key auth (upstream `providers/radius.ts`:
/// `envApiKeyAuth("Radius API key", ["RADIUS_API_KEY"])`; the Radius OAuth
/// flow resolves into `options.api_key` before dispatch).
const API_KEY_ENV: &str = "RADIUS_API_KEY";

pub struct PiMessagesProvider {
    provider_id: String,
    display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
}

impl PiMessagesProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Radius".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
        })
    }

    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = client;
        self
    }
}

impl Provider for PiMessagesProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }
    fn name(&self) -> &str {
        &self.display_name
    }
    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "Radius API key",
            vec![API_KEY_ENV.to_owned()],
        )]
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
        let events = output.producer();
        let mut client = self.client.clone();
        crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
        let model = model.clone();
        tokio::spawn(async move {
            let result = pi_messages_stream(&client, &model, &context, &options, &events).await;
            if events.is_abandoned() {
                // Normal abandon: nobody is reading, so report nothing.
                events.end(None);
                return;
            }
            crate::ai::api::finish_provider_stream(
                &events,
                result,
                &model,
                options.signal.as_ref(),
            );
        });
        output
    }
}

// ---------------------------------------------------------------------------
// Request (upstream pi-messages.ts:347-399)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct PiMessagesRequest<'a> {
    model: &'a str,
    context: PiMessagesContext<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<PiMessagesRequestOptions>,
}

#[derive(serde::Serialize)]
struct PiMessagesContext<'a> {
    messages: &'a [Message],
}

#[derive(serde::Serialize)]
struct PiMessagesRequestOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    /// Thinking level string (`off|minimal|low|medium|high|xhigh`); serde
    /// renames on [`crate::ai::types::ThinkingLevel`] already match.
    #[serde(rename = "reasoning", skip_serializing_if = "Option::is_none")]
    reasoning: Option<crate::ai::types::ThinkingLevel>,
    #[serde(rename = "cacheRetention", skip_serializing_if = "Option::is_none")]
    cache_retention: Option<String>,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// String or object form of the tool choice (upstream pi-messages passes
    /// `ToolChoice` through: `auto`/`none`/`required` or a function object).
    #[serde(rename = "toolChoice", skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
}

/// `cacheRetention` default (upstream pi-messages.ts:347-353): the
/// `PI_CACHE_RETENTION=long` env makes `None` mean `long`.
fn effective_cache_retention(
    options: &StreamOptions,
    env: Option<&crate::ai::types::ProviderEnv>,
) -> Option<String> {
    match options.cache_retention {
        Some(CacheRetention::None) => Some("none".into()),
        Some(CacheRetention::Short) => Some("short".into()),
        Some(CacheRetention::Long) => Some("long".into()),
        None => {
            let from_env = crate::ai::api::provider_env_value("PI_CACHE_RETENTION", env);
            if from_env.as_deref() == Some("long") {
                Some("long".into())
            } else {
                None
            }
        }
    }
}

/// Build the request body and headers (public for tests/).
pub fn build_pi_messages_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<(Value, BTreeMap<String, String>)> {
    let Some(api_key) = options.api_key.as_deref().filter(|key| !key.is_empty()) else {
        return Err(AiError::Authentication(format!(
            "No API key provided for provider \"{}\"",
            model.provider
        )));
    };
    let tool_choice = options
        .tool_choice
        .as_ref()
        .map(crate::ai::types::ToolChoice::openai_chat_json);
    let request = PiMessagesRequest {
        model: &model.id,
        context: PiMessagesContext {
            messages: &context.messages,
        },
        options: Some(PiMessagesRequestOptions {
            temperature: options.temperature,
            max_tokens: options.max_tokens,
            reasoning: options.reasoning,
            cache_retention: effective_cache_retention(options, options.env.as_ref()),
            session_id: options.session_id.clone(),
            tool_choice,
        }),
    };
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_owned(), format!("Bearer {api_key}"));
    headers.insert("accept".to_owned(), "text/event-stream".to_owned());
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    if let Some(provider_headers) = &options.headers {
        for (name, value) in provider_headers {
            headers.insert(name.clone(), value.clone());
        }
    }
    let mut body = serde_json::to_value(&request)?;
    // Upstream `options.onPayload` may replace the whole payload.
    if let Some(on_payload) = &options.hooks.on_payload {
        on_payload(&mut body);
    }
    Ok((body, headers))
}

/// The dispatch URL: `{baseUrl}/messages` (pi-messages.ts:370), with
/// `?debug=1` when the debug hook flag is set (pi-messages.ts:371-373).
pub fn pi_messages_url(model: &Model, options: &StreamOptions) -> String {
    let mut url = format!("{}/messages", model.base_url.trim_end_matches('/'));
    if options.debug.unwrap_or(false) {
        url.push_str("?debug=1");
    }
    url
}

// ---------------------------------------------------------------------------
// Wire events (upstream PiMessagesEvent, pi-messages.ts:54-87)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PiMessagesEvent {
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
        content: String,
        #[serde(rename = "contentSignature")]
        content_signature: Option<String>,
    },
    ThinkingStart,
    ThinkingDelta {
        delta: String,
    },
    ThinkingEnd {
        content: String,
        #[serde(rename = "thinkingSignature")]
        thinking_signature: Option<String>,
        #[serde(default)]
        redacted: Option<bool>,
    },
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    ToolcallDelta {
        /// Wire `contentIndex` (upstream pi-messages.ts:69); optional because
        /// older payloads omit it (the delta then keys on the most recent
        /// tool-call block).
        #[serde(rename = "contentIndex", default)]
        content_index: Option<usize>,
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
        #[serde(rename = "responseId")]
        response_id: Option<String>,
        #[serde(default)]
        rewrite: Option<Value>,
    },
    Error {
        reason: String,
        usage: Usage,
        #[serde(rename = "errorMessage")]
        error_message: Option<String>,
        #[serde(rename = "responseId")]
        response_id: Option<String>,
        #[serde(default)]
        rewrite: Option<Value>,
    },
}

/// Terminal `done.reason` values (upstream `"stop"|"length"|"toolUse"`).
fn terminal_reason(reason: &str) -> StopReason {
    match reason {
        "toolUse" => StopReason::ToolUse,
        "length" => StopReason::Length,
        _ => StopReason::Stop,
    }
}

/// The incremental tool-call argument parser reuses the shared
/// partial-JSON handler (same reuse as mistral.rs's argument deltas).
fn parse_tool_arguments(partial: &str) -> BTreeMap<String, Value> {
    serde_json::from_value::<BTreeMap<String, Value>>(
        crate::ai::api::openai_responses::shared::parse_partial_json(partial),
    )
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Converter: wire events → AssistantMessageEvents
// ---------------------------------------------------------------------------

/// The converter state: the cumulative assistant message plus the raw
/// tool-call JSON fragments (upstream keeps the same structures;
/// `toolcall_delta` fragments append to a per-block string that is re-parsed
/// incrementally). Public for tests/.
pub struct PiMessagesParser {
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// ([`Self::message_mut`]) so partial snapshots are O(1) hand-outs instead
    /// of a deep clone of the whole accumulated message per delta.
    ///
    /// `message.content` is the dense projection: one entry per block, in
    /// ascending wire-`contentIndex` order (the order the indices imply, not
    /// the arrival order). It is mutated IN PLACE: [`Self::put_block`]
    /// touches one slot, the delta arms touch exactly the block they address
    /// (via [`Self::slot_of`]), so a delta costs O(delta) and never a rebuild
    /// of every accumulated block.
    message: Arc<AssistantMessage>,
    /// Wire `contentIndex` of each block, parallel to `message.content` and
    /// ascending, so `binary_search` maps a wire index to its projection slot.
    slot_keys: Vec<usize>,
    tool_json: BTreeMap<usize, String>,
    /// Bounded-cadence refresh of the parsed arguments mirrored into the
    /// block (the raw `tool_json` stays the accumulation source of truth).
    tool_mirror: BTreeMap<usize, crate::ai::api::partial_mirror::MirrorCadence>,
    /// `true` once a terminal `done`/`error` event was applied.
    terminated: bool,
    /// Raw wire-event observer (upstream `onProviderStreamEvent`).
    raw_event_hook: Option<crate::ai::types::StreamHook>,
    buffer: VecDeque<u8>,
    /// How many times a mutation had to copy the accumulator because a
    /// snapshot (or a `Weak`) was outstanding. One per block start that shares
    /// a chunk with its first delta is the healthy shape; one per delta means
    /// a consumer is holding the partial.
    copies: u64,
    /// How many content blocks the dense projection had to materialize: one
    /// per inserted/replaced block (the text/thinking/tool-call block starts
    /// and the text/tool-call block ends). In-place delta updates — including
    /// `thinking_end` — count nothing.
    projected_blocks: u64,
}

impl PiMessagesParser {
    pub fn new(model: &Model) -> Self {
        Self {
            message: Arc::new(crate::ai::api::base_message(model)),
            slot_keys: Vec::new(),
            tool_json: BTreeMap::new(),
            tool_mirror: BTreeMap::new(),
            terminated: false,
            raw_event_hook: None,
            buffer: VecDeque::new(),
            copies: 0,
            projected_blocks: 0,
        }
    }

    /// Install the raw wire-event observer (upstream
    /// `onProviderStreamEvent`).
    fn with_raw_event_hook(mut self, hook: crate::ai::types::StreamHook) -> Self {
        self.raw_event_hook = Some(hook);
        self
    }

    /// Mutable access to the accumulated message, copy-on-write: while a
    /// previously handed-out snapshot is still alive the message is cloned
    /// once for this mutation; once every reader has dropped its `Arc` the
    /// message is mutated in place, so one delta costs O(delta) instead of a
    /// deep clone of the accumulated message.
    #[inline]
    fn message_mut(&mut self) -> &mut AssistantMessage {
        // `Arc::make_mut` copies when the accumulator is shared with an
        // outstanding snapshot — and, deliberately, also while a `Weak` to it
        // exists. The count makes that observable for the diagnostics surface
        // (and for the tests that pin "one copy per block start, none per
        // delta" without perturbing the measurement by holding a `Weak`).
        if Arc::get_mut(&mut self.message).is_none() {
            self.copies += 1;
        }
        Arc::make_mut(&mut self.message)
    }

    /// Projection slot of the block keyed `key`, if present.
    fn slot_of(&self, key: usize) -> Option<usize> {
        self.slot_keys.binary_search(&key).ok()
    }

    /// Insert (at its ascending-key slot) or replace the single block keyed
    /// `key`. The wire addresses blocks by `contentIndex` while the projection
    /// stays dense, so the key->slot mapping lives in `slot_keys`.
    fn put_block(&mut self, key: usize, block: ContentBlock) {
        self.projected_blocks += 1;
        match self.slot_keys.binary_search(&key) {
            Ok(slot) => self.message_mut().content[slot] = block,
            Err(slot) => {
                self.slot_keys.insert(slot, key);
                self.message_mut().content.insert(slot, block);
            }
        }
    }

    /// Snapshot of the message accumulated so far.
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`; every later
    /// mutation goes through [`Self::message_mut`], which copies-on-write, so
    /// the handed-out value stays the snapshot taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    /// Diagnostics: how many times a mutation had to copy the accumulator
    /// (see the `copies` field). O(1), no allocation — the COW tests read it.
    #[doc(hidden)]
    pub fn copy_count(&self) -> u64 {
        self.copies
    }

    /// Diagnostics: how many content blocks the dense projection has had to
    /// materialize (see the `projected_blocks` field). A delta that nobody has
    /// snapshotted mutates its block in place and counts nothing, so this
    /// grows with the stream's block starts/ends, never with the delta count
    /// or the accumulated text. O(1), no allocation.
    #[doc(hidden)]
    pub fn projection_work(&self) -> u64 {
        self.projected_blocks
    }

    fn push_rewrite_diagnostic(&mut self, rewrite: &Value) {
        let mut details = BTreeMap::new();
        if let Some(object) = rewrite.as_object() {
            for (key, value) in object {
                details.insert(key.clone(), value.clone());
            }
        }
        self.message_mut().diagnostics.push(DiagnosticInfo {
            diag_type: "pi_messages_rewrite".to_owned(),
            timestamp: crate::ai::util::now_ms(),
            error_message: None,
            error_name: None,
            error_stack: None,
            error_code: None,
            details: Some(details),
        });
    }

    fn apply(&mut self, event: &PiMessagesEvent) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        match event {
            PiMessagesEvent::Start => {}
            PiMessagesEvent::TextStart { content_index } => {
                self.put_block(
                    *content_index,
                    ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                );
                out.push(AssistantMessageEvent::TextStart {
                    content_index: *content_index,
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::TextDelta {
                content_index,
                delta,
            } => {
                if let Some(slot) = self.slot_of(*content_index) {
                    if let Some(ContentBlock::Text(text)) = self.message_mut().content.get_mut(slot)
                    {
                        text.text.push_str(delta);
                    }
                }
                out.push(AssistantMessageEvent::TextDelta {
                    content_index: *content_index,
                    delta: delta.clone(),
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::TextEnd {
                content_index,
                content,
                content_signature,
            } => {
                self.put_block(
                    *content_index,
                    ContentBlock::Text(TextContent {
                        text: content.clone(),
                        text_signature: content_signature.clone(),
                    }),
                );
                out.push(AssistantMessageEvent::TextEnd {
                    content_index: *content_index,
                    content: content.clone(),
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::ThinkingStart => {
                let index = self.slot_keys.last().map_or(0, |key| key + 1);
                self.put_block(
                    index,
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }),
                );
                out.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::ThinkingDelta { delta } => {
                // Upstream keys thinking deltas by the block's content index;
                // the wire carries only text/tool-call indices, so the port
                // derives it from the projection length (the last block's key,
                // for the contiguous indices the wire uses).
                let index = self.message.content.len().saturating_sub(1);
                if let Some(slot) = self.slot_of(index) {
                    if let Some(ContentBlock::Thinking(thinking)) =
                        self.message_mut().content.get_mut(slot)
                    {
                        thinking.thinking.push_str(delta);
                    }
                }
                out.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: delta.clone(),
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::ThinkingEnd {
                content,
                thinking_signature,
                redacted,
            } => {
                let index = self.message.content.len().saturating_sub(1);
                if let Some(slot) = self.slot_of(index) {
                    if let Some(ContentBlock::Thinking(thinking)) =
                        self.message_mut().content.get_mut(slot)
                    {
                        thinking.thinking = content.clone();
                        thinking.thinking_signature = thinking_signature.clone();
                        thinking.redacted = redacted.unwrap_or(false);
                    }
                }
                out.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: content.clone(),
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::ToolcallStart {
                content_index,
                id,
                tool_name,
            } => {
                self.tool_json.insert(*content_index, String::new());
                self.put_block(
                    *content_index,
                    ContentBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: tool_name.clone(),
                        arguments: BTreeMap::new(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    }),
                );
                out.push(AssistantMessageEvent::ToolCallStart {
                    content_index: *content_index,
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::ToolcallDelta {
                content_index,
                delta,
            } => {
                // Upstream keys deltas by the wire `contentIndex`
                // (pi-messages.ts:254-259); a payload without it falls back to
                // the most recent tool-call block.
                let index = content_index.unwrap_or_else(|| {
                    self.message
                        .content
                        .iter()
                        .zip(&self.slot_keys)
                        .filter_map(|(block, key)| match block {
                            ContentBlock::ToolCall(_) => Some(*key),
                            _ => None,
                        })
                        .next_back()
                        .unwrap_or_default()
                });
                // Accumulate the raw fragments and re-parse at a bounded
                // cadence (first fragment, then every 8 KiB): rebuilding the
                // whole value per fragment is O(args²) per call. The block end
                // carries the authoritative finalized call.
                self.tool_json.entry(index).or_default().push_str(delta);
                let refresh = self.tool_mirror.entry(index).or_default().record(delta);
                if refresh {
                    let fragment = self.tool_json.get(&index).cloned().unwrap_or_default();
                    if let Some(slot) = self.slot_of(index) {
                        if let Some(ContentBlock::ToolCall(call)) =
                            self.message_mut().content.get_mut(slot)
                        {
                            call.arguments = parse_tool_arguments(&fragment);
                        }
                    }
                }
                out.push(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta: delta.clone(),
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::ToolcallEnd {
                content_index,
                tool_call,
            } => {
                // The backend normally parses the call and may already carry
                // `argumentsError`. If it does not, validate the raw fragments
                // accumulated here: malformed text keeps the call with `{}`
                // plus the parse error instead of trusting the wire arguments.
                let mut tool_call = tool_call.clone();
                if let Some(raw) = self.tool_json.remove(content_index) {
                    if tool_call.arguments_error.is_none() {
                        let parsed = crate::ai::api::tool_args::parse_final_tool_arguments(&raw);
                        if let Some(error) = parsed.error {
                            tool_call.arguments = BTreeMap::new();
                            tool_call.arguments_error = Some(error);
                        }
                    }
                }
                self.put_block(*content_index, ContentBlock::ToolCall(tool_call.clone()));
                out.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: *content_index,
                    tool_call: tool_call.clone(),
                    partial: self.snapshot(),
                });
            }
            PiMessagesEvent::Done {
                reason,
                usage,
                response_id,
                rewrite,
                ..
            } => {
                if let Some(rewrite) = rewrite {
                    self.push_rewrite_diagnostic(rewrite);
                }
                let message = self.message_mut();
                message.usage = *usage;
                message.response_id = response_id.clone();
                let stop_reason = terminal_reason(reason);
                message.stop_reason = stop_reason;
                self.terminated = true;
                // The wire reason carries through to the event (upstream
                // pi-messages.ts:205 `reason: event.reason`).
                out.push(AssistantMessageEvent::Done {
                    reason: match stop_reason {
                        StopReason::ToolUse => TerminalReason::ToolUse,
                        StopReason::Length => TerminalReason::Length,
                        _ => TerminalReason::Stop,
                    },
                    message: (*self.message).clone(),
                });
            }
            PiMessagesEvent::Error {
                reason,
                usage,
                error_message,
                response_id,
                rewrite,
                ..
            } => {
                if let Some(rewrite) = rewrite {
                    self.push_rewrite_diagnostic(rewrite);
                }
                let message = self.message_mut();
                message.usage = *usage;
                message.response_id = response_id.clone();
                message.error_message = error_message.clone();
                message.stop_reason = if reason == "aborted" {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                self.terminated = true;
                out.push(AssistantMessageEvent::Error {
                    reason: if reason == "aborted" {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: (*self.message).clone(),
                });
            }
        }
        out
    }

    /// Feed raw (possibly partial) SSE bytes; pushes completed events into
    /// `out`. Blank-line-separated blocks carry the payload on their FIRST
    /// `data:` line (upstream pi-messages.ts:314-316 takes `find`, not join);
    /// `data: [DONE]` is ignored. Public for tests/.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        self.buffer.extend(chunk.iter().copied());
        let mut text: Vec<u8> = Vec::with_capacity(self.buffer.len());
        for byte in self.buffer.drain(..) {
            text.push(if byte == b'\r' { b'\n' } else { byte });
        }
        // Only complete blocks (up to the last blank line) may be decoded:
        // the trailing bytes can end inside an unfinished multi-byte UTF-8
        // character, which a lossy round-trip would corrupt permanently. The
        // tail stays in raw bytes until the next chunk completes it
        // (upstream decodes incrementally with `{stream: true}`).
        let complete_end = text
            .windows(2)
            .rposition(|pair| pair == b"\n\n")
            .map_or(0, |index| index + 2);
        let complete = String::from_utf8_lossy(&text[..complete_end]);
        for block in complete.split("\n\n") {
            self.handle_block(block, out)?;
        }
        self.buffer.extend(text[complete_end..].iter().copied());
        // An unterminated block is buffered until its closing blank line. The
        // shared SSE framing caps one unterminated line at
        // [`crate::ai::api::MAX_SSE_LINE_BYTES`]; this parser buffers whole
        // blocks (JSON events are single-line payloads), so the same cap must
        // bound the trailing block as well. Without it the only bound was the
        // transport's 16 MiB response cap: a provider (or proxy) that never
        // emits a blank line could accumulate the whole body and, worse, keep
        // extending the buffer across `feed` calls past that cap.
        if self.buffer.len() > crate::ai::api::MAX_SSE_LINE_BYTES {
            let buffered = self.buffer.len();
            // The stream is unusable from here: release the oversized block
            // so a caller that keeps the parser does not retain it.
            self.buffer.clear();
            return Err(AiError::NonRetryable(format!(
                "pi-messages: SSE block buffer exceeds the {}-byte (1 MiB) cap \
                 without a blank line ({buffered} bytes buffered)",
                crate::ai::api::MAX_SSE_LINE_BYTES
            )));
        }
        Ok(())
    }

    /// Apply one blank-line-separated SSE block (the payload rides on the
    /// block's first `data:` line, pi-messages.ts:314-316).
    fn handle_block(&mut self, block: &str, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if block.trim().is_empty() {
            return Ok(());
        }
        let Some(data) = block.lines().find(|line| line.starts_with("data:")) else {
            return Ok(());
        };
        let payload = data.trim_start_matches("data:").trim();
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(());
        }
        let raw: Value = serde_json::from_str(payload)
            .map_err(|error| AiError::NonRetryable(format!("pi-messages: bad event: {error}")))?;
        if let Some(hook) = &self.raw_event_hook {
            hook(&raw);
        }
        let event: PiMessagesEvent = serde_json::from_value(raw)
            .map_err(|error| AiError::NonRetryable(format!("pi-messages: bad event: {error}")))?;
        out.extend(self.apply(&event));
        Ok(())
    }

    /// Flush a trailing event block not terminated by a blank line, then
    /// require a terminal event (upstream flushes the reader buffer at stream
    /// end, pi-messages.ts:302-307, and rejects a missing terminal event at
    /// :423). Public for tests/.
    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let tail: Vec<u8> = self.buffer.drain(..).collect();
        self.handle_block(&String::from_utf8_lossy(&tail), out)?;
        if self.terminated {
            Ok(())
        } else {
            Err(AiError::StreamNotTerminated)
        }
    }
}

/// Parse a complete SSE buffer into events (batch path; public for tests/).
pub fn parse_pi_messages_sse(body: &[u8], model: &Model) -> AiResult<Vec<AssistantMessageEvent>> {
    let mut parser = PiMessagesParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    parser.feed(body, &mut out)?;
    parser.finish(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Stream driver
// ---------------------------------------------------------------------------

async fn pi_messages_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> AiResult<()> {
    let (body, headers) = build_pi_messages_request(model, context, options)?;
    let url = pi_messages_url(model, options);

    let mut parser = PiMessagesParser::new(model);
    if let Some(hook) = &options.hooks.on_provider_stream_event {
        parser = parser.with_raw_event_hook(Arc::clone(hook));
    }
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
    {
        let mut on_chunk = crate::ai::api::EventChunkSink::new(
            |chunk: &[u8], ready: &mut Vec<AssistantMessageEvent>| {
                // Every consumer handle is gone: release the HTTP response
                // instead of draining a body nobody reads (normal abandon).
                if sink.is_abandoned() {
                    return Err(AiError::Aborted);
                }
                parser.feed(chunk, ready)
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
            .await?;
    }
    // Push the events of a trailing block that arrived without its closing
    // blank line before the terminal-event check (finish handles both).
    let mut tail = Vec::new();
    parser.finish(&mut tail)?;
    for event in tail {
        if !sink.push_await(event).await {
            break;
        }
    }
    Ok(())
}
