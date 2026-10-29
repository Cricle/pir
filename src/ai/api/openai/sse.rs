//! Incremental SSE parsing for OpenAI-compatible `/chat/completions` streams.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;

use super::reasoning::{append_openai_reasoning_detail, is_openai_reasoning_detail};
use crate::ai::api::partial_mirror::{apply_json_changes, JsonChange, StreamingJsonMirror};
use crate::ai::api::tool_args::FinalToolArguments;
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::*;

/// Maximum bytes retained for one not-yet-terminated SSE line (1 MiB, matching
/// the MCP protocol-frame cap); the shared `sse_feed_lines` framing enforces it
/// for every adapter, and this parser keeps its complete-line check as defense
/// in depth.
pub(crate) use crate::ai::api::MAX_SSE_LINE_BYTES;

/// Parse a full SSE response body into assistant message events.
/// Public for tests/ and non-streaming callers; feeds the same
/// [`OpenAiSseParser`] state machine as the incremental streaming path.
pub fn parse_openai_sse(body: &[u8], model: &Model) -> Result<Vec<AssistantMessageEvent>, AiError> {
    let mut parser = OpenAiSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for data in crate::ai::api::sse_data(body) {
        parser.handle_data(&data, &mut out)?;
    }
    parser.finish(&mut out)?;
    Ok(out)
}

/// Incremental SSE parser for OpenAI-compatible `/chat/completions` streams.
///
/// Both the batch path ([`parse_openai_sse`]) and the streaming path drive the
/// same state machine: [`feed`](Self::feed) splits raw body chunks into SSE
/// lines (buffering partial lines — including multi-byte UTF-8 — across chunk
/// boundaries), [`feed_line`](Self::feed_line) accumulates `data:` fields per
/// event block, and [`handle_data`](Self::handle_data) maps one data payload to
/// events. [`finish`](Self::finish) flushes a trailing unterminated event block
/// and emits the block end / done events. Public for tests/.
pub struct OpenAiSseParser {
    /// Body bytes received but not yet forming a complete line.
    buffer: Vec<u8>,
    /// `data:` field lines accumulated for the current event block.
    data_lines: Vec<String>,
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// ([`Self::message_mut`]) so handing out a partial snapshot is O(1) and
    /// a delta that nobody has snapshotted is applied in place — instead of
    /// the previous deep clone of the whole accumulated message per delta.
    message: Arc<AssistantMessage>,
    /// Provider id of the model being streamed (the opencode-go reasoning
    /// field rename is provider-specific).
    model_provider: String,
    /// The model's catalog rates, so a usage chunk can be priced as it
    /// arrives (upstream `parseChunkUsage` calls `calculateCost`, including
    /// the OpenRouter billed-cost overlay).
    model_cost: ModelCost,
    /// Whether the model routes through OpenRouter (upstream
    /// `isOpenRouterModel`); OpenRouter's usage chunks carry billed cost
    /// accounting.
    openrouter: bool,
    text_started: bool,
    thinking_started: bool,
    // Content indices of the (single) text/thinking blocks inside
    // `message.content`. Blocks are appended when they start streaming
    // (upstream `ensureTextBlock` / `ensureThinkingBlock`), so a block's
    // content index equals its position in the partial snapshots.
    text_index: usize,
    thinking_index: usize,
    // Provider tool-call index -> (content index, id, name, accumulated arguments).
    tools: HashMap<usize, ToolEntry>,
    // Tool-call blocks keyed by id for providers that omit the `index` field
    // (upstream `toolCallBlocksById`).
    tools_by_id: HashMap<String, usize>,
    // A wire `index` that arrived after its block was already created under
    // another key (id-first, index-later chunks): resolved to the block's
    // canonical key so later chunks carrying that index update the same block
    // (upstream binds `toolCallBlocksByIndex` when `state.streamIndex` is set).
    tool_index_aliases: HashMap<usize, usize>,
    // Synthetic map keys for tool calls that carry neither index nor id
    // (descending from usize::MAX so they never collide with wire indices).
    next_synthetic_tool_key: usize,
    // `reasoning_details` are replay metadata, not user-visible stream deltas.
    // Keep them in memory during streaming and serialize once when the block
    // is finalized (upstream `streamedReasoningDetails`).
    streamed_reasoning_details: Option<Vec<Value>>,
    /// Whether `compat.supports_finish_reason` is set on the model.
    supports_finish_reason: bool,
    finish: StopReason,
    has_finish_reason: bool,
    /// Upstream `options.onProviderStreamEvent`: each parsed provider chunk
    /// before pir normalization.
    stream_event_hook: Option<crate::ai::types::StreamHook>,
    /// How many times a mutation had to copy the accumulator because a
    /// snapshot (or a `Weak`) was outstanding. One per block start is the
    /// healthy shape; one per delta means a consumer is holding the partial.
    copies: u64,
}

#[derive(Debug)]
struct ToolEntry {
    content_index: usize,
    id: String,
    name: String,
    arguments: String,
    /// Grammar/custom tool: arguments carry the raw `custom.input` string.
    custom: bool,
    /// Incremental reader that turns every argument fragment into structural
    /// changes for the block's parsed arguments (upstream Unreleased
    /// `ToolCallArgumentsStream`). The raw `arguments` stay the accumulation
    /// source of truth for the strict final parse.
    parsed: StreamingJsonMirror,
}

/// Parsed-argument update for one tool-call fragment, queued because the
/// `ToolEntry` borrow cannot overlap the message mutation.
enum InFlightMirror {
    /// Structural changes produced by the incremental JSON reader.
    Json(Vec<JsonChange>),
    /// Raw grammar-tool input appended to the block's `input` string.
    Custom(String),
}

impl OpenAiSseParser {
    pub fn new(model: &Model) -> Self {
        let compat = super::compat_detect::resolve_compat(model);
        Self {
            buffer: Vec::new(),
            data_lines: Vec::new(),
            message: Arc::new(base_message(model)),
            model_provider: model.provider.clone(),
            model_cost: model.cost.clone(),
            openrouter: crate::ai::util::is_openrouter_model(model),
            text_started: false,
            thinking_started: false,
            text_index: 0,
            thinking_index: 0,
            tools: HashMap::new(),
            tools_by_id: HashMap::new(),
            tool_index_aliases: HashMap::new(),
            next_synthetic_tool_key: usize::MAX,
            streamed_reasoning_details: None,
            supports_finish_reason: compat.supports_finish_reason,
            finish: StopReason::Stop,
            has_finish_reason: false,
            stream_event_hook: None,
            copies: 0,
        }
    }

    /// Attach the upstream `onProviderStreamEvent` observer (each parsed
    /// provider chunk before pir normalization).
    pub fn with_stream_hook(mut self, hook: Option<crate::ai::types::StreamHook>) -> Self {
        self.stream_event_hook = hook;
        self
    }

    /// Snapshot of the message accumulated so far (e.g. for the Start event).
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`. Every later
    /// mutation goes through [`Self::message_mut`], which copies-on-write, so
    /// the handed-out value stays the snapshot taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    /// Snapshot for the delta phase — the same O(1) `Arc` hand-out as
    /// Diagnostics: how many accumulator copies this parser has paid (see the
    /// `copies` field). O(1), no allocation — the streaming-diagnostics surface
    /// and the COW tests read it.
    #[doc(hidden)]
    pub fn copy_count(&self) -> u64 {
        self.copies
    }

    /// [`Self::snapshot`] (kept as a separate name for the delta call sites).
    fn partial(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
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

    /// Feed one raw body chunk; pushes events for every complete SSE event
    /// block. Partial lines (and multi-byte characters split across chunks)
    /// are buffered until a newline arrives, bounded by
    /// [`MAX_SSE_LINE_BYTES`].
    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result =
            crate::ai::api::sse_feed_lines(chunk, &mut buffer, |line| self.feed_line(line, events));
        self.buffer = buffer;
        result?;
        if self.buffer.len() > MAX_SSE_LINE_BYTES {
            let buffered = self.buffer.len();
            // Drop the oversized partial line: the stream is unusable, and the
            // caller may keep the parser around while reporting the error.
            self.buffer.clear();
            return Err(AiError::NonRetryable(format!(
                "OpenAI SSE line buffer exceeds the {MAX_SSE_LINE_BYTES}-byte (1 MiB) cap \
                 without a newline ({buffered} bytes buffered)"
            )));
        }
        Ok(())
    }

    /// Feed one complete SSE line (newline and trailing CR already stripped).
    pub fn feed_line(
        &mut self,
        line: &str,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        // A complete line is dispatched as soon as it arrives, so it does not
        // accumulate across feeds; cap it anyway so one oversized line cannot
        // bypass the partial-line bound above.
        if line.len() > MAX_SSE_LINE_BYTES {
            return Err(AiError::NonRetryable(format!(
                "OpenAI SSE line exceeds the {MAX_SSE_LINE_BYTES}-byte (1 MiB) cap ({} bytes)",
                line.len()
            )));
        }
        if line.is_empty() {
            // Blank line: end of an event block, dispatch accumulated data.
            return self.flush_event(events);
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.trim().to_string());
        }
        // Other SSE fields (`event:`, `id:`, comments) carry no data for this
        // wire format; the JSON payloads self-describe via their `type`/shape.
        Ok(())
    }

    /// Dispatch the accumulated `data:` fields of one event block.
    fn flush_event(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if self.data_lines.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data_lines).join("\n");
        self.handle_data(&data, events)
    }

    /// Map one SSE data payload to events (shared with the batch path).
    fn handle_data(&mut self, data: &str, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)?;
        // Upstream `options.onProviderStreamEvent`: the adapter-owned chunk is
        // read-only here.
        if let Some(hook) = &self.stream_event_hook {
            hook(&value);
        }
        // OpenAI-compatible providers (including Azure and proxies) report
        // stream failures as an in-band `{"error": {...}}` frame. Dropping it
        // would turn a provider 429/5xx into a silent empty completion.
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            let text = openai_error_frame_text(error);
            return Err(crate::ai::api::provider_error_frame_from_value(error, text));
        }
        // OpenAI documents ChatCompletionChunk.id as the unique chat completion
        // identifier; every chunk of a completion carries the same id.
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            // Rewriting the same completion id is not a message change (it
            // must not force a copy-on-write below).
            if self.message.response_id.as_deref() != Some(id) {
                self.message_mut().response_id = Some(id.to_string());
            }
        }
        // Upstream `output.responseModel ||= chunk.model`: the first
        // non-empty model that differs from the requested model wins.
        if let Some(response_model) = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != self.message.model)
        {
            if self.message.response_model.is_none() {
                self.message_mut().response_model = Some(response_model.to_string());
            }
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let usage = parse_chunk_usage(usage, &self.model_cost, self.openrouter);
            self.message_mut().usage = usage;
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return Ok(());
        };
        // Fallback: some providers (e.g. Moonshot) return usage in
        // choice.usage instead of the standard chunk.usage. A standard chunk
        // commonly carries `"usage": null`, so an absent OR null top-level
        // usage must fall through — the top-level parse above already ignores
        // null.
        if value.get("usage").is_none_or(|usage| usage.is_null()) {
            if let Some(usage) = choice.get("usage").filter(|usage| !usage.is_null()) {
                let usage = parse_chunk_usage(usage, &self.model_cost, self.openrouter);
                self.message_mut().usage = usage;
            }
        }
        // Upstream `if (choice.finish_reason)` (openai-completions.ts:581):
        // an empty finish_reason is not a terminal signal, so it must not map
        // to an error stop (some providers send `""` on keep-alive chunks).
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            self.message_mut().raw_stop_reason = Some(reason.into());
            self.finish = map_stop(reason);
            self.has_finish_reason = true;
            if self.finish == StopReason::Error {
                self.message_mut().error_message =
                    Some(format!("Provider finish_reason: {reason}"));
            }
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        // Handle reasoning before content: some proxies (e.g. LiteLLM)
        // aggregate reasoning and the answer into one delta, and the thinking
        // block must precede the text block (upstream ordering fix, #10926).
        // Some endpoints return reasoning in reasoning_content (llama.cpp),
        // reasoning (other OpenAI-compatible endpoints), or reasoning_text.
        // Pick the FIRST NON-EMPTY field per chunk to avoid duplication (e.g.
        // chutes.ai returns reasoning_content and reasoning with the same
        // content) and to reach reasoning_text when the earlier fields are
        // present but empty (upstream openai-completions.ts reasoningFields).
        let reasoning_value = ["reasoning_content", "reasoning", "reasoning_text"]
            .iter()
            .find_map(|field| {
                delta
                    .get(*field)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(|value| (*field, value))
            });
        if let Some((field, value)) = reasoning_value {
            // Upstream `ensureThinkingBlock(reasoningField)` records WHICH
            // field carried the reasoning so replay puts the text back under
            // the same name (llama.cpp / gpt-oss servers). opencode-go streams
            // it as `reasoning` but expects `reasoning_content` back.
            let signature = if self.model_provider == "opencode-go" && field == "reasoning" {
                "reasoning_content"
            } else {
                field
            };
            self.ensure_thinking_start(signature, out);
            let thinking_index = self.thinking_index;
            if let Some(ContentBlock::Thinking(block)) =
                self.message_mut().content.get_mut(thinking_index)
            {
                block.thinking.push_str(value);
            }
            out.push(AssistantMessageEvent::ThinkingDelta {
                content_index: self.thinking_index,
                delta: value.into(),
                partial: self.partial(),
            });
        }
        // OpenRouter-style structured reasoning deltas: consecutive
        // text/summary deltas are merged into logical entries, while
        // encrypted entries remain opaque and discrete.
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details {
                if !is_openai_reasoning_detail(detail) {
                    continue;
                }
                // Upstream `ensureThinkingBlock("")` for reasoning_details.
                self.ensure_thinking_start("", out);
                append_openai_reasoning_detail(
                    self.streamed_reasoning_details.get_or_insert_with(Vec::new),
                    detail,
                );
            }
        }
        if let Some(value) = delta.get("content").and_then(Value::as_str) {
            // Empty content deltas must not open a text block (upstream
            // `if (textDelta)` truthiness check).
            if !value.is_empty() {
                if !self.text_started {
                    // Append the block at start (upstream `ensureTextBlock`): the
                    // content index equals its position in the partial snapshots.
                    self.text_index = self.message.content.len();
                    self.message_mut()
                        .content
                        .push(ContentBlock::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                    out.push(AssistantMessageEvent::TextStart {
                        content_index: self.text_index,
                        partial: self.partial(),
                    });
                    self.text_started = true;
                }
                let text_index = self.text_index;
                if let Some(ContentBlock::Text(block)) =
                    self.message_mut().content.get_mut(text_index)
                {
                    block.text.push_str(value);
                }
                out.push(AssistantMessageEvent::TextDelta {
                    content_index: self.text_index,
                    delta: value.into(),
                    partial: self.partial(),
                });
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.handle_tool_call_delta(call, out)?;
            }
        }
        Ok(())
    }

    /// Map one streaming tool-call delta onto its block (upstream
    /// `ensureToolCallBlock`): blocks are keyed by the wire `index` when
    /// present, otherwise by the tool-call id, otherwise a fresh synthetic
    /// block is allocated.
    fn handle_tool_call_delta(
        &mut self,
        call: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let raw_wire_index = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize);
        let wire_index = raw_wire_index.map(|index| {
            self.tool_index_aliases
                .get(&index)
                .copied()
                .unwrap_or(index)
        });
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let function_name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let custom_name = call
            .get("custom")
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_custom = call.get("function").is_none() && call.get("custom").is_some();

        let key = match wire_index {
            Some(index) => {
                // A wire index may arrive after the block was already created
                // from an id-only chunk (providers that send the id first).
                // Reuse that block and bind the index to it instead of
                // appending a duplicate (upstream `ensureToolCallBlock`:
                // index lookup, then id lookup, then create).
                if self.tools.contains_key(&index) {
                    index
                } else if !call_id.is_empty() {
                    match self.tools_by_id.get(&call_id) {
                        Some(existing) => *existing,
                        None => index,
                    }
                } else {
                    index
                }
            }
            None => {
                // No wire index: match an existing block by id; without an id,
                // reuse the single in-flight anonymous block; else allocate.
                if !call_id.is_empty() {
                    match self.tools_by_id.get(&call_id) {
                        Some(existing) => *existing,
                        None => self.allocate_tool_key(),
                    }
                } else {
                    self.allocate_tool_key()
                }
            }
        };
        if let Some(raw_index) = raw_wire_index {
            if raw_index != key {
                self.tool_index_aliases.insert(raw_index, key);
            }
        }

        // Ensure the block exists (with its ToolCallStart event). The block is
        // appended to the message at creation (upstream `ensureToolCallBlock`),
        // so content indices follow creation order and the partial snapshots
        // carry the in-flight call.
        if !self.tools.contains_key(&key) {
            let ci = self.message.content.len();
            let name = if function_name.is_empty() {
                custom_name
            } else {
                function_name
            };
            self.message_mut()
                .content
                .push(ContentBlock::ToolCall(ToolCall {
                    id: call_id.clone(),
                    name: name.to_string(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }));
            self.tools.insert(
                key,
                ToolEntry {
                    content_index: ci,
                    id: call_id.clone(),
                    name: name.to_string(),
                    arguments: String::new(),
                    custom: is_custom,
                    parsed: StreamingJsonMirror::new(),
                },
            );
            if !call_id.is_empty() {
                self.tools_by_id.insert(call_id.clone(), key);
            }
            out.push(AssistantMessageEvent::ToolCallStart {
                content_index: ci,
                partial: self.partial(),
            });
        }
        let mut delta_text = String::new();
        // Parsed-argument update to mirror back into the message block
        // (block content index plus the queued change set).
        let mut in_flight: Option<(usize, InFlightMirror)> = None;
        if let Some(entry) = self.tools.get_mut(&key) {
            if entry.id.is_empty() && !call_id.is_empty() {
                entry.id = call_id.clone();
            }
            let name = if function_name.is_empty() {
                custom_name
            } else {
                function_name
            };
            if entry.name.is_empty() && !name.is_empty() {
                entry.name = name.to_string();
            }
            let chunk = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .or_else(|| {
                    call.get("custom")
                        .and_then(|c| c.get("input"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default();
            entry.arguments.push_str(chunk);
            delta_text = chunk.to_string();
            // Upstream parses every argument delta immediately
            // (`block.arguments = input.arguments.append(delta)`,
            // openai-completions.ts Unreleased): the partial mirror must never
            // lag behind the deltas. The port reads the fragment once and
            // applies its structural changes to the block (O(fragment) per
            // delta, O(arguments) per call) instead of re-parsing the whole
            // accumulated text per delta. An empty chunk is ignored like
            // upstream's truthiness check; the finalized arguments still come
            // from one strict parse of the complete raw buffer.
            if !chunk.is_empty() {
                let update = if entry.custom {
                    Some(InFlightMirror::Custom(chunk.to_string()))
                } else {
                    let changes = entry.parsed.append(chunk);
                    (!changes.is_empty()).then_some(InFlightMirror::Json(changes))
                };
                in_flight = update.map(|update| (entry.content_index, update));
            }
        }
        // An id/name that arrives after the block started is mirrored into the
        // in-message block so partial snapshots and the finalized call agree.
        if let Some(entry) = self.tools.get(&key) {
            let (block_index, id, name) =
                (entry.content_index, entry.id.clone(), entry.name.clone());
            if !id.is_empty() {
                self.tools_by_id.insert(id.clone(), key);
            }
            if let Some(ContentBlock::ToolCall(block)) =
                self.message_mut().content.get_mut(block_index)
            {
                if block.id != id || block.name != name {
                    block.id = id;
                    block.name = name;
                }
            }
        }
        // Mirror the fragment's parsed arguments into the block. Function
        // calls apply the incremental reader's changes in order; grammar
        // (custom) calls append the raw input to the same `{"input": ...}`
        // shape the final pass produces. Both leave the message (and so the
        // snapshot) untouched when nothing changed.
        if let Some((block_index, update)) = in_flight {
            if let Some(ContentBlock::ToolCall(block)) =
                self.message_mut().content.get_mut(block_index)
            {
                match update {
                    InFlightMirror::Json(changes) => {
                        apply_json_changes(&mut block.arguments, changes);
                    }
                    InFlightMirror::Custom(delta) => match block.arguments.get_mut("input") {
                        Some(Value::String(input)) => input.push_str(&delta),
                        _ => {
                            block
                                .arguments
                                .insert("input".to_string(), Value::String(delta));
                        }
                    },
                }
            }
        }
        let ci = self
            .tools
            .get(&key)
            .map(|entry| entry.content_index)
            .unwrap_or(0);
        out.push(AssistantMessageEvent::ToolCallDelta {
            content_index: ci,
            delta: delta_text,
            partial: self.partial(),
        });
        Ok(())
    }

    fn allocate_tool_key(&mut self) -> usize {
        let key = self.next_synthetic_tool_key;
        self.next_synthetic_tool_key = key.wrapping_sub(1);
        key
    }

    /// Append the thinking block (with its `ThinkingStart` event) if not
    /// started yet (upstream `ensureThinkingBlock`), recording the reasoning
    /// field that produced it in the signature slot for replay. Returns the
    /// block's content index.
    fn ensure_thinking_start(
        &mut self,
        signature: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> usize {
        if !self.thinking_started {
            self.thinking_index = self.message.content.len();
            self.message_mut()
                .content
                .push(ContentBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: if signature.is_empty() {
                        None
                    } else {
                        Some(signature.to_string())
                    },
                    redacted: false,
                }));
            out.push(AssistantMessageEvent::ThinkingStart {
                content_index: self.thinking_index,
                partial: self.partial(),
            });
            self.thinking_started = true;
        }
        self.thinking_index
    }

    /// Flush a trailing event block not terminated by a blank line, finalize
    /// the in-flight blocks in place (upstream `finishBlock`) and emit their
    /// end events in content order, then terminate with the `Done` event.
    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        self.flush_event(out)
            .map_err(crate::ai::api::trailing_event_error)?;
        // Serialize accumulated structured reasoning details into the thinking
        // block's signature slot (upstream `applyStreamedReasoningDetails` at
        // block end).
        if self.thinking_started {
            // Build the serialized signature first: the details borrow must
            // not overlap the accumulator's mutable borrow below.
            let signature = self
                .streamed_reasoning_details
                .as_ref()
                .map(|details| serde_json::to_string(details).unwrap_or_default());
            if let Some(signature) = signature {
                let thinking_index = self.thinking_index;
                if let Some(ContentBlock::Thinking(block)) =
                    self.message_mut().content.get_mut(thinking_index)
                {
                    block.thinking_signature = Some(signature);
                }
            }
        }
        // Finalize tool-call arguments in place (upstream `finishBlock`); the
        // block was appended at start, only the parsed arguments change.
        let mut tools: Vec<ToolEntry> = std::mem::take(&mut self.tools).into_values().collect();
        tools.sort_by_key(|entry| entry.content_index);
        for entry in &tools {
            let parsed = self.finalize_tool_arguments(entry);
            if let Some(ContentBlock::ToolCall(tool_call)) =
                self.message_mut().content.get_mut(entry.content_index)
            {
                tool_call.arguments = parsed.arguments;
                tool_call.arguments_error = parsed.error;
                tool_call.id = entry.id.clone();
                tool_call.name = entry.name.clone();
            }
        }
        // End events in content (creation) order — the order upstream's final
        // `for (const block of blocks) finishBlock(block)` pass uses. Each
        // carries the same O(1) `Arc` snapshot of the finalized message.
        for (content_index, block) in self.message.content.iter().enumerate() {
            match block {
                ContentBlock::Thinking(thinking) => {
                    out.push(AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content: thinking.thinking.clone(),
                        partial: Arc::clone(&self.message),
                    });
                }
                ContentBlock::Text(text) => {
                    out.push(AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text.text.clone(),
                        partial: Arc::clone(&self.message),
                    });
                }
                ContentBlock::ToolCall(tool_call) => {
                    out.push(AssistantMessageEvent::ToolCallEnd {
                        content_index,
                        tool_call: tool_call.clone(),
                        partial: Arc::clone(&self.message),
                    });
                }
                _ => {}
            }
        }
        // Upstream post-stream checks (openai-completions.ts 695-703): without
        // finish_reason support the stop reason is inferred from the content;
        // error stop reasons terminate the stream as an error; with finish
        // reason support a missing one is a stream failure.
        if !self.has_finish_reason && !self.supports_finish_reason {
            self.finish = if self
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
            {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
        }
        if self.finish == StopReason::Error {
            return Err(AiError::NonRetryable(
                self.message_mut()
                    .error_message
                    .take()
                    .unwrap_or_else(|| "Provider returned an error stop reason".to_string()),
            ));
        }
        if self.supports_finish_reason && !self.has_finish_reason {
            // A clean EOF without the terminal signal is a truncated stream,
            // not a provider verdict: retryable at the agent level (the text
            // stays the upstream-parity wording).
            return Err(AiError::StreamTruncated(
                "Stream ended without finish_reason".to_string(),
            ));
        }
        self.message_mut().stop_reason = self.finish;
        let terminal = match self.finish {
            StopReason::ToolUse => TerminalReason::ToolUse,
            StopReason::Length => TerminalReason::Length,
            _ => TerminalReason::Stop,
        };
        // The terminal event owns its message: reclaim the accumulator when
        // every emitted snapshot was already dropped, otherwise copy once
        // (the end events in `out` may still hold it).
        let message = std::mem::replace(&mut self.message, Arc::new(AssistantMessage::default()));
        let message = match Arc::try_unwrap(message) {
            Ok(message) => message,
            Err(shared) => (*shared).clone(),
        };
        out.push(AssistantMessageEvent::Done {
            reason: terminal,
            message,
        });
        Ok(())
    }

    /// Finalize the accumulated tool-call argument JSON at block end
    /// (upstream `parseToolCallArguments`, #10897/#10944): strict parse with
    /// only malformed string escapes repaired. A truncated or otherwise
    /// invalid final document keeps the call with empty arguments and an
    /// `argumentsError`, so the stream completes and the model receives a
    /// tool-result error it can correct on the next turn.
    fn finalize_tool_arguments(&self, entry: &ToolEntry) -> FinalToolArguments {
        if entry.custom {
            let mut arguments = BTreeMap::new();
            arguments.insert("input".to_string(), Value::String(entry.arguments.clone()));
            return FinalToolArguments {
                arguments,
                error: None,
            };
        }
        crate::ai::api::tool_args::parse_final_tool_arguments(&entry.arguments)
    }
}

fn base_message(model: &Model) -> AssistantMessage {
    crate::ai::api::base_message(model)
}

/// Display text for an OpenAI-compatible in-band error object. The provider's
/// own message is preserved; the code/type is appended when present so the
/// terminal error stays diagnosable.
fn openai_error_frame_text(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("Provider returned an error");
    let mut text = format!("OpenAI API error: {message}");
    if let Some(code) = error.get("code").and_then(|code| match code {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }) {
        text.push_str(&format!(" ({code})"));
    }
    text
}
fn map_stop(reason: &str) -> StopReason {
    match reason {
        "stop" | "end" => StopReason::Stop,
        "length" => StopReason::Length,
        "function_call" | "tool_calls" => StopReason::ToolUse,
        // content_filter, network_error, and any provider-specific reason
        // surface as errors carrying the raw finish reason (upstream
        // mapStopReason). "max_tokens" is NOT an upstream value; unknown
        // reasons are errors.
        _ => StopReason::Error,
    }
}
/// Usage accounting for a streaming usage chunk (upstream `parseChunkUsage`):
/// cached tokens are cache reads subtracted from the prompt total, with the
/// provider-specific placement quirks (OpenAI/OpenRouter
/// `prompt_tokens_details.cached_tokens`, DeepSeek `prompt_cache_hit_tokens`,
/// Kimi top-level `cached_tokens`) and OpenRouter's separate
/// `cache_write_tokens`.
///
/// Upstream also prices the chunk here (`calculateCost`) and lets OpenRouter's
/// billed accounting replace the estimate total (`applyOpenRouterReportedCost`);
/// `model_cost`/`openrouter` carry that context.
fn parse_chunk_usage(value: &Value, model_cost: &ModelCost, openrouter: bool) -> Usage {
    let prompt_tokens = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let details = value.get("prompt_tokens_details");
    let cache_read = details
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| value.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| value.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = details
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Follow documented OpenAI/OpenRouter semantics: cached_tokens is
    // cache-read tokens (hits); writes are never subtracted from it.
    let input = prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let output = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = value
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: (reasoning > 0).then_some(reasoning),
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    crate::ai::types::calculate_cost_with(model_cost, &mut usage);
    if openrouter {
        crate::ai::util::apply_openrouter_reported_cost(&mut usage, value);
    }
    usage
}
