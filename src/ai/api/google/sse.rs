//! Incremental SSE parsing for the Google Generative AI
//! `models/{model}:streamGenerateContent?alt=sse` stream.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::shared::{is_thinking_part, map_stop_reason, retain_thought_signature};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::*;

/// Parse a full SSE response body into assistant message events.
/// Public for tests/ and non-streaming callers; feeds the same
/// [`GoogleSseParser`] state machine as the incremental streaming path.
pub fn parse_google_sse(body: &[u8], model: &Model) -> Result<Vec<AssistantMessageEvent>, AiError> {
    let mut parser = GoogleSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for data in crate::ai::api::sse_data(body) {
        parser.handle_data(&data, &mut out)?;
    }
    parser.finish(&mut out)?;
    Ok(out)
}

/// Incremental SSE parser for Google Generative AI streams.
///
/// Both the batch path ([`parse_google_sse`]) and the streaming path drive the
/// same state machine: [`feed`](Self::feed) splits raw body chunks into SSE
/// lines (buffering partial lines — including multi-byte UTF-8 — across chunk
/// boundaries), [`feed_line`](Self::feed_line) accumulates `data:` fields per
/// event block, and [`handle_data`](Self::handle_data) maps one
/// `GenerateContentResponse` payload to events (mirroring the upstream
/// adapter's chunk loop: text/thought parts, function calls, finish reasons,
/// usage metadata). [`finish`](Self::finish) flushes a trailing unterminated
/// event block and emits the block end / done events. Public for tests/.
pub struct GoogleSseParser {
    /// Body bytes received but not yet forming a complete line.
    buffer: Vec<u8>,
    /// `data:` field lines accumulated for the current event block.
    data_lines: Vec<String>,
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// (`Arc::make_mut(&mut self.message)`) so partial snapshots are O(1)
    /// hand-outs and a delta nobody snapshotted is applied in place — instead
    /// of the previous deep clone of the whole accumulated message per delta.
    message: Arc<AssistantMessage>,
    /// The in-flight text or thinking block (upstream `currentBlock`). The
    /// block itself is appended to `message.content` at start, so partial
    /// snapshots carry it and `index` is its position there.
    current: Option<CurrentBlock>,
    saw_finish: bool,
    finish: StopReason,
    tool_call_counter: u64,
    /// Attached upstream `onProviderStreamEvent` observer (each parsed
    /// `GenerateContentResponse` before pir normalization; read-only).
    provider_event_hook: Option<crate::ai::types::StreamHook>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone)]
struct CurrentBlock {
    kind: BlockKind,
    /// Accumulated text (thinking text for thinking blocks).
    text: String,
    /// Last non-empty `thoughtSignature` seen on this block's parts.
    signature: Option<String>,
    /// Content index assigned when the block started.
    index: usize,
}

impl GoogleSseParser {
    pub fn new(model: &Model) -> Self {
        Self {
            buffer: Vec::new(),
            data_lines: Vec::new(),
            message: Arc::new(base_message(model)),
            current: None,
            saw_finish: false,
            finish: StopReason::Stop,
            tool_call_counter: 0,
            provider_event_hook: None,
        }
    }

    /// Attach the upstream `onProviderStreamEvent` observer (each parsed
    /// provider chunk before pir normalization). Public for tests/.
    pub fn with_provider_event_hook(mut self, hook: Option<crate::ai::types::StreamHook>) -> Self {
        self.provider_event_hook = hook;
        self
    }

    /// Snapshot of the message accumulated so far (e.g. for the Start event).
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`; every later
    /// mutation copies-on-write, so the handed-out value stays the snapshot
    /// taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    /// Feed one raw body chunk; pushes events for every complete SSE event
    /// block. Partial lines (and multi-byte characters split across chunks)
    /// are buffered until a newline arrives.
    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result =
            crate::ai::api::sse_feed_lines(chunk, &mut buffer, |line| self.feed_line(line, events));
        self.buffer = buffer;
        result
    }

    /// Feed one complete SSE line (newline and trailing CR already stripped).
    pub fn feed_line(
        &mut self,
        line: &str,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if line.is_empty() {
            // Blank line: end of an event block, dispatch accumulated data.
            return self.flush_event(events);
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.trim().to_string());
        }
        // Other SSE fields (`event:`, `id:`, comments) carry no data for this
        // wire format; payloads self-describe via their JSON shape.
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

    /// Map one `data:` payload (a `GenerateContentResponse`) to events
    /// (shared with the batch path).
    pub fn handle_data(
        &mut self,
        data: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let value: Value = serde_json::from_str(data)?;
        // Upstream `options.onProviderStreamEvent`: the parsed provider chunk
        // is read-only here (google-generative-ai.ts:107, google-vertex.ts:116).
        if let Some(hook) = &self.provider_event_hook {
            hook(&value);
        }
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            let text = google_error_message(error);
            return Err(crate::ai::api::provider_error_frame_from_value(error, text));
        }
        // Keep the first non-empty responseId from the stream (upstream
        // `output.responseId ||= chunk.responseId`).
        if self.message.response_id.is_none() {
            if let Some(id) = value.get("responseId").and_then(Value::as_str) {
                if !id.is_empty() {
                    Arc::make_mut(&mut self.message).response_id = Some(id.to_string());
                }
            }
        }
        if let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        {
            if let Some(parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    self.handle_part(part, out)?;
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.saw_finish = true;
                Arc::make_mut(&mut self.message).raw_stop_reason = Some(reason.to_string());
                self.finish = map_stop_reason(reason);
            }
        } else if let Some(blocked) = value
            .get("promptFeedback")
            .and_then(|feedback| feedback.get("blockReason"))
            .and_then(Value::as_str)
        {
            // No candidates and a prompt-level block: nothing further will
            // arrive, surface it as a provider error.
            return Err(AiError::NonRetryable(format!("Prompt blocked: {blocked}")));
        }
        if let Some(usage) = value.get("usageMetadata") {
            Arc::make_mut(&mut self.message).usage = parse_google_usage(usage);
        }
        Ok(())
    }

    /// Map one streamed `Part` to events (upstream adapter's part loop).
    fn handle_part(&mut self, part: &Value, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            let is_thinking = is_thinking_part(part);
            let signature = part.get("thoughtSignature").and_then(Value::as_str);
            let kind = if is_thinking {
                BlockKind::Thinking
            } else {
                BlockKind::Text
            };
            let switch = match &self.current {
                None => true,
                Some(current) => current.kind != kind,
            };
            if switch {
                self.close_current_block(out);
                // Upstream pushes the block into the message at start
                // (`output.content.push(currentBlock)`,
                // google-generative-ai.ts:138-146), so the content index is
                // the block's position and every partial snapshot carries the
                // in-flight block.
                let index = self.message.content.len();
                let message = Arc::make_mut(&mut self.message);
                message.content.push(match kind {
                    BlockKind::Text => ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                    BlockKind::Thinking => ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }),
                });
                self.current = Some(CurrentBlock {
                    kind,
                    text: String::new(),
                    signature: None,
                    index,
                });
                match kind {
                    BlockKind::Text => out.push(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: Arc::clone(&self.message),
                    }),
                    BlockKind::Thinking => out.push(AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: Arc::clone(&self.message),
                    }),
                }
            }
            let Some(current) = self.current.as_mut() else {
                return Ok(());
            };
            current.text.push_str(text);
            current.signature = retain_thought_signature(current.signature.as_deref(), signature);
            let (index, signature) = (current.index, current.signature.clone());
            // Write the delta through to the in-message block so the emitted
            // partial carries the streaming text and its latest signature.
            match kind {
                BlockKind::Text => {
                    if let Some(ContentBlock::Text(block)) =
                        Arc::make_mut(&mut self.message).content.get_mut(index)
                    {
                        block.text.push_str(text);
                        block.text_signature = signature;
                    }
                }
                BlockKind::Thinking => {
                    if let Some(ContentBlock::Thinking(block)) =
                        Arc::make_mut(&mut self.message).content.get_mut(index)
                    {
                        block.thinking.push_str(text);
                        block.thinking_signature = signature;
                    }
                }
            }
            match kind {
                BlockKind::Text => out.push(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: text.to_string(),
                    partial: Arc::clone(&self.message),
                }),
                BlockKind::Thinking => out.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: text.to_string(),
                    partial: Arc::clone(&self.message),
                }),
            }
        }
        if let Some(function_call) = part.get("functionCall") {
            self.close_current_block(out);
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let provided_id = function_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            let duplicate = provided_id.as_deref().is_some_and(|id| {
                self.message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolCall(call) if call.id == id))
            });
            self.tool_call_counter = self.tool_call_counter.saturating_add(1);
            let id = match provided_id {
                Some(id) if !duplicate => id,
                // Generate a unique id when missing or duplicated (upstream
                // `${name}_${Date.now()}_${++counter}`).
                _ => format!(
                    "{}_{}_{}",
                    name,
                    crate::ai::util::now_ms(),
                    self.tool_call_counter
                ),
            };
            // Google delivers `functionCall.args` already decoded as JSON (the
            // SSE payload itself is JSON), so there is no raw argument text to
            // re-parse. A present non-object value is still malformed per the
            // shared policy: keep the call with `{}` plus `argumentsError`
            // instead of silently dropping the value. Absent/null args are the
            // upstream `?? {}` empty case.
            let (arguments, arguments_error): (BTreeMap<String, Value>, Option<String>) =
                match function_call.get("args") {
                    None | Some(Value::Null) => (BTreeMap::new(), None),
                    Some(Value::Object(map)) => (
                        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        None,
                    ),
                    Some(other) => (
                        BTreeMap::new(),
                        Some(format!(
                            "Tool arguments must be a JSON object\n\nReceived arguments:\n{other}"
                        )),
                    ),
                };
            let tool_call = ToolCall {
                id,
                name,
                arguments,
                thought_signature: part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                namespace: None,
                arguments_error,
            };
            // Upstream pushes the tool-call block into the message before the
            // start/delta/end events (`output.content.push(toolCall)` then the
            // three pushes, google-generative-ai.ts:211-219), so every event's
            // `partial` already carries the block.
            let index = self.message.content.len();
            Arc::make_mut(&mut self.message)
                .content
                .push(ContentBlock::ToolCall(tool_call.clone()));
            let partial = Arc::clone(&self.message);
            out.push(AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial: Arc::clone(&partial),
            });
            let delta = serde_json::to_string(&tool_call.arguments)?;
            out.push(AssistantMessageEvent::ToolCallDelta {
                content_index: index,
                delta,
                partial: Arc::clone(&partial),
            });
            out.push(AssistantMessageEvent::ToolCallEnd {
                content_index: index,
                tool_call: tool_call.clone(),
                partial,
            });
        }
        Ok(())
    }

    /// Close the in-flight text/thinking block: emit its end event. The block
    /// itself was appended to the message when it started (upstream appends at
    /// creation and only pushes the end event here).
    fn close_current_block(&mut self, out: &mut Vec<AssistantMessageEvent>) {
        let Some(current) = self.current.take() else {
            return;
        };
        match current.kind {
            BlockKind::Text => {
                out.push(AssistantMessageEvent::TextEnd {
                    content_index: current.index,
                    content: current.text,
                    partial: Arc::clone(&self.message),
                });
            }
            BlockKind::Thinking => {
                out.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: current.index,
                    content: current.text,
                    partial: Arc::clone(&self.message),
                });
            }
        }
    }

    /// Flush a trailing event block not terminated by a blank line, close any
    /// in-flight block, then emit the terminal `Done` event. Mirrors
    /// upstream's post-stream checks: a stream without a finish reason and a
    /// blocked/errored finish reason both fail (surfacing as stream errors).
    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        self.flush_event(out)
            .map_err(crate::ai::api::trailing_event_error)?;
        if !self.saw_finish {
            // A clean EOF without a finish reason is a truncated stream (the
            // transport read a complete body, so no HTTP verdict applies):
            // retryable at the agent level.
            return Err(AiError::StreamTruncated(
                "Google stream ended without a finish reason".to_string(),
            ));
        }
        if matches!(self.finish, StopReason::Error) {
            let raw = self
                .message
                .raw_stop_reason
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_string());
            return Err(AiError::NonRetryable(format!(
                "Provider stopped with: {raw}"
            )));
        }
        self.close_current_block(out);
        // STOP with tool calls means the model asked for a tool turn.
        let has_tool_call = self
            .message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        if has_tool_call && self.finish == StopReason::Stop {
            self.finish = StopReason::ToolUse;
        }
        // The end events above may still hold the previous snapshot, so this
        // finalization copies-on-write once; the terminal event then reclaims
        // the accumulator without another copy.
        Arc::make_mut(&mut self.message).stop_reason = self.finish;
        let terminal = match self.finish {
            StopReason::ToolUse => TerminalReason::ToolUse,
            StopReason::Length => TerminalReason::Length,
            _ => TerminalReason::Stop,
        };
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
}

fn base_message(model: &Model) -> AssistantMessage {
    crate::ai::api::base_message(model)
}

/// Compose an error message from a Google API error payload
/// (`{"error": {"code", "message", "status"}}`).
fn google_error_message(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_u64);
    let status = error.get("status").and_then(Value::as_str);
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    match (code, status) {
        (Some(code), Some(status)) => format!("Google API error {code} ({status}): {message}"),
        (Some(code), None) => format!("Google API error {code}: {message}"),
        (None, Some(status)) => format!("Google API error ({status}): {message}"),
        (None, None) => format!("Google API error: {message}"),
    }
}

/// Parse `usageMetadata` into pir usage (upstream adapter's usage mapping):
/// `input = promptTokenCount - cachedContentTokenCount`,
/// `output = candidatesTokenCount + thoughtsTokenCount`.
fn parse_google_usage(value: &Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let prompt = field("promptTokenCount");
    let cached = field("cachedContentTokenCount");
    let candidates = field("candidatesTokenCount");
    let thoughts = field("thoughtsTokenCount");
    let input = prompt.saturating_sub(cached);
    let output = candidates.saturating_add(thoughts);
    Usage {
        input,
        output,
        cache_read: cached,
        reasoning: (thoughts > 0).then_some(thoughts),
        total_tokens: value
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(input + output),
        ..Usage::default()
    }
}
