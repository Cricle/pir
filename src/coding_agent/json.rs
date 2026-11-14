//! `--mode json`: non-TTY JSON event lines on stdout, mirroring upstream
//! `packages/coding-agent/src/modes/json-event.ts` (+ the session-header and
//! event emission of `modes/print-mode.ts` with `mode: "json"`).
//!
//! Upstream contract:
//! - the session header (when persisted) is emitted as the first line;
//! - every session event is emitted as one JSON line;
//! - streaming `message_update` events are converted by [`toJsonEvent`]:
//!   cumulative assistant snapshots (`partial`) are stripped and cumulative
//!   usage is hoisted to the top level — including on the terminal
//!   `done`/`error` events, whose message carries the final usage;
//!   `toolcall_start` events carry the tool-call `id`/`toolName` because the
//!   snapshot that held them is gone.
//!
//! pir maps the upstream session `message_update` onto the agent loop's
//! `AgentEvent::StreamEvent`, and keeps pir's existing event JSON shapes
//! (same serialization the RPC mode uses) for the other lifecycle events.

use crate::agent::agent_loop::AgentEvent;
use crate::agent_session::{agent_event_to_json, session_event_to_json, SessionEvent};
use crate::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, ToolResultMessage,
};

/// Serialize one session-level event (upstream `AgentSessionEvent` members
/// the agent-loop enum cannot carry — `queue_update`, `compaction_start`,
/// `compaction_end`, `bash_execution_update`) into its JSON wire event
/// (modes/json-event.ts streams them through `toJsonEvent` unchanged).
pub fn to_session_event_json(event: &SessionEvent) -> serde_json::Value {
    session_event_to_json(event)
}

/// Convert one agent event into its JSON wire event (upstream `toJsonEvent`).
pub fn to_json_event(event: &AgentEvent) -> serde_json::Value {
    match event {
        AgentEvent::StreamEvent { turn, event } => {
            let mut update = serde_json::json!({
                "type": "message_update",
                "turn": turn,
                "assistantMessageEvent": assistant_message_event_json(event),
            });
            if let Some(usage) = usage_of(event) {
                update["usage"] = usage;
            }
            update
        }
        other => agent_event_to_json(other),
    }
}

/// Serialize one assistant stream event with the cumulative `partial`
/// snapshot removed (upstream `toJsonAssistantMessageEvent`). A
/// `toolcall_start` gains `id`/`toolName` from the tool call at
/// `content_index` in the (now removed) partial message.
///
/// The event is serialized through [`AssistantMessageEventWire`], the
/// borrowed view that never visits `partial`: the old construction
/// (`serde_json::to_value(event)` then `remove("partial")`) materialized the
/// whole accumulated reply into a `Value` tree only to drop it, on every
/// delta of every stream.
fn assistant_message_event_json(event: &AssistantMessageEvent) -> serde_json::Value {
    match serde_json::to_value(AssistantMessageEventWire::new(event)) {
        Ok(value) => value,
        Err(error) => {
            // Serialization of in-memory types cannot realistically fail;
            // degrade to a placeholder instead of panicking.
            serde_json::json!({ "type": "error", "error": error.to_string() })
        }
    }
}

/// Borrowed serialization view of [`AssistantMessageEvent`]: every field of
/// the event's wire shape EXCEPT the cumulative `partial` snapshot (upstream
/// `toJsonAssistantMessageEvent`).
///
/// Both `--mode json` and the RPC host emit every assistant stream event
/// through this view. Serializing the derived event and deleting `partial`
/// afterwards costs O(accumulated reply) per delta; the view forwards only the
/// event's own fields, so the per-delta cost is O(delta).
///
/// The wire shape is byte-identical to the derive's output minus `partial`:
/// the tag comes first, then the variant fields in declaration order, and a
/// `toolcall_start` appends the derived `id`/`toolName` last — exactly where
/// the previous post-processing inserted them.
pub(crate) struct AssistantMessageEventWire<'a> {
    event: &'a AssistantMessageEvent,
}

impl<'a> AssistantMessageEventWire<'a> {
    pub(crate) fn new(event: &'a AssistantMessageEvent) -> Self {
        Self { event }
    }
}

impl serde::Serialize for AssistantMessageEventWire<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self.event {
            AssistantMessageEvent::Start { .. } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "start")?;
                map.end()
            }
            AssistantMessageEvent::TextStart { content_index, .. } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "text_start")?;
                map.serialize_entry("content_index", content_index)?;
                map.end()
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "text_delta")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("delta", delta)?;
                map.end()
            }
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "text_end")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("content", content)?;
                map.end()
            }
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "thinking_start")?;
                map.serialize_entry("content_index", content_index)?;
                map.end()
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "thinking_delta")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("delta", delta)?;
                map.end()
            }
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "thinking_end")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("content", content)?;
                map.end()
            }
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                let identity = match partial.content.get(*content_index) {
                    Some(ContentBlock::ToolCall(tool_call)) => Some(tool_call),
                    _ => None,
                };
                let mut map =
                    serializer.serialize_map(Some(if identity.is_some() { 4 } else { 2 }))?;
                map.serialize_entry("type", "toolcall_start")?;
                map.serialize_entry("content_index", content_index)?;
                if let Some(tool_call) = identity {
                    map.serialize_entry("id", &tool_call.id)?;
                    map.serialize_entry("toolName", &tool_call.name)?;
                }
                map.end()
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "toolcall_delta")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("delta", delta)?;
                map.end()
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "toolcall_end")?;
                map.serialize_entry("content_index", content_index)?;
                map.serialize_entry("tool_call", tool_call)?;
                map.end()
            }
            AssistantMessageEvent::Done { reason, message } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "done")?;
                map.serialize_entry("reason", reason)?;
                map.serialize_entry("message", message)?;
                map.end()
            }
            AssistantMessageEvent::Error { reason, error } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "error")?;
                map.serialize_entry("reason", reason)?;
                map.serialize_entry("error", error)?;
                map.end()
            }
        }
    }
}

/// Cumulative usage of the assistant message carried by the event, when
/// present. Every `message_update` hoists its usage (upstream
/// `toJsonEvent` uses `event.message.usage` unconditionally); the terminal
/// `done`/`error` events carry the authoritative final message.
fn usage_of(event: &AssistantMessageEvent) -> Option<serde_json::Value> {
    let usage = match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => &partial.usage,
        AssistantMessageEvent::Done { message, .. } => &message.usage,
        AssistantMessageEvent::Error { error, .. } => &error.usage,
    };
    serde_json::to_value(usage).ok()
}

/// Borrowed serialization view of [`AgentEvent`]: the exact wire bytes of
/// [`to_json_event`] / [`agent_event_to_json`], written straight from the
/// borrowed event.
///
/// The RPC and JSON print hosts stream every event to one stdout line.
/// Serializing through a `serde_json::Value` first materializes the whole
/// payload (the tool-result text, the new-message list) as an owned tree and
/// escapes it again into the line — `PIR-BIN-099` measured ~4x the payload
/// per face, and ~4 MB per 200 KB tool-result turn. This view performs the
/// identical serialization over the borrowed event, so the line is produced
/// with a single pass into the output buffer.
///
/// Byte identity is the contract: the `StreamEvent` arm mirrors
/// [`to_json_event`]'s own composition (cumulative `partial` stripped, hoisted
/// `usage` appended last) and every other variant mirrors
/// [`agent_event_to_json`] in ITS construction order (the crate builds
/// `serde_json` with `preserve_order`, so the `json!` insertion order IS the
/// wire order); optional keys are appended exactly where the old
/// post-inserts placed them. Pinned per event by
/// `tests/json_stream_wire.rs::every_agent_event_line_matches_the_value_construction`.
pub(crate) struct AgentEventWire<'a> {
    event: &'a AgentEvent,
}

impl<'a> AgentEventWire<'a> {
    pub(crate) fn new(event: &'a AgentEvent) -> Self {
        Self { event }
    }
}

impl serde::Serialize for AgentEventWire<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self.event {
            // `to_json_event`'s own arm for streaming events (the RPC/JSON
            // hosts): the cumulative `partial` snapshot is stripped (it rides
            // once as `message` on `agent_event_to_json`'s arm, never here)
            // and the hoisted `usage` is appended last.
            AgentEvent::StreamEvent { turn, event } => {
                let usage = usage_of(event);
                let mut map =
                    serializer.serialize_map(Some(if usage.is_some() { 4 } else { 3 }))?;
                map.serialize_entry("type", "message_update")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry(
                    "assistantMessageEvent",
                    &AssistantMessageEventWire::new(event),
                )?;
                if let Some(usage) = &usage {
                    map.serialize_entry("usage", usage)?;
                }
                map.end()
            }
            // `agent_event_to_json`'s arms, field for field, in its order.
            AgentEvent::AgentStart { .. } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "agent_start")?;
                map.end()
            }
            AgentEvent::AgentEnd { result } => {
                let mut map = serializer.serialize_map(Some(6))?;
                map.serialize_entry("type", "agent_end")?;
                map.serialize_entry("turns", &result.turns)?;
                map.serialize_entry("aborted", &result.aborted)?;
                map.serialize_entry("terminal", &result.terminal)?;
                map.serialize_entry("messages", &result.new_messages)?;
                map.serialize_entry("willRetry", &result.will_retry)?;
                map.end()
            }
            AgentEvent::TurnStart { turn, .. } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "turn_start")?;
                map.serialize_entry("turn", turn)?;
                map.end()
            }
            AgentEvent::TurnEnd {
                turn,
                message,
                tool_results,
            } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("type", "turn_end")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("message", message)?;
                map.serialize_entry("toolResults", tool_results)?;
                map.end()
            }
            AgentEvent::MessageStart { turn, message } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "message_start")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("message", message)?;
                map.end()
            }
            AgentEvent::MessageEnd { turn, message } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "message_end")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("message", message)?;
                map.end()
            }
            AgentEvent::StreamStart { turn, message } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "message_start")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("message", message)?;
                map.end()
            }
            AgentEvent::StreamEnd { turn, message } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "message_end")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("message", message)?;
                map.end()
            }
            AgentEvent::ToolExecutionStart { turn, event, args } => {
                match event {
                    crate::agent::tool_batch::ToolExecutionEvent::Start {
                        call_id,
                        tool_name,
                        parent_call_id,
                        ..
                    } => {
                        let mut map = serializer
                            .serialize_map(Some(if parent_call_id.is_some() { 6 } else { 5 }))?;
                        map.serialize_entry("type", "tool_execution_start")?;
                        map.serialize_entry("turn", turn)?;
                        map.serialize_entry("toolCallId", call_id)?;
                        map.serialize_entry("toolName", tool_name)?;
                        map.serialize_entry("args", args)?;
                        if let Some(parent) = parent_call_id {
                            map.serialize_entry("parentToolCallId", parent)?;
                        }
                        map.end()
                    }
                    crate::agent::tool_batch::ToolExecutionEvent::End { .. } => {
                        let mut map = serializer.serialize_map(Some(3))?;
                        map.serialize_entry("type", "tool_execution_start")?;
                        map.serialize_entry("turn", turn)?;
                        map.serialize_entry("args", args)?;
                        map.end()
                    }
                }
            }
            AgentEvent::ToolExecutionEnd {
                turn,
                event,
                result,
            } => match event {
                crate::agent::tool_batch::ToolExecutionEvent::End {
                    call_id,
                    tool_name,
                    is_error,
                    duration_ms,
                    parent_call_id,
                    ..
                } => {
                    // `durationMs` then `parentToolCallId`: the old
                    // post-insert order.
                    let optional =
                        duration_ms.is_some() as usize + parent_call_id.is_some() as usize;
                    let mut map = serializer.serialize_map(Some(6 + optional))?;
                    map.serialize_entry("type", "tool_execution_end")?;
                    map.serialize_entry("turn", turn)?;
                    map.serialize_entry("toolCallId", call_id)?;
                    map.serialize_entry("toolName", tool_name)?;
                    map.serialize_entry("result", result)?;
                    map.serialize_entry("isError", is_error)?;
                    if let Some(duration) = duration_ms {
                        map.serialize_entry("durationMs", duration)?;
                    }
                    if let Some(parent) = parent_call_id {
                        map.serialize_entry("parentToolCallId", parent)?;
                    }
                    map.end()
                }
                crate::agent::tool_batch::ToolExecutionEvent::Start { .. } => {
                    let mut map = serializer.serialize_map(Some(3))?;
                    map.serialize_entry("type", "tool_execution_end")?;
                    map.serialize_entry("turn", turn)?;
                    map.serialize_entry("result", result)?;
                    map.end()
                }
            },
            AgentEvent::ToolExecutionUpdate {
                turn,
                call_id,
                tool_name,
                parent_call_id,
                args,
                partial,
            } => {
                let mut map =
                    serializer.serialize_map(Some(if parent_call_id.is_some() { 7 } else { 6 }))?;
                map.serialize_entry("type", "tool_execution_update")?;
                map.serialize_entry("turn", turn)?;
                map.serialize_entry("toolCallId", call_id)?;
                map.serialize_entry("toolName", tool_name)?;
                map.serialize_entry("args", args)?;
                map.serialize_entry("partialResult", partial)?;
                if let Some(parent) = parent_call_id {
                    map.serialize_entry("parentToolCallId", parent)?;
                }
                map.end()
            }
            AgentEvent::Compaction { turn } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "compaction")?;
                map.serialize_entry("turn", turn)?;
                map.end()
            }
        }
    }
}

/// Byte length of `s` as a JSON string literal (quotes included), exactly as
/// `serde_json` escapes it: `\"`, `\\` and the five named control escapes cost
/// one extra byte, every other character below `0x20` is written as `\u00XX`.
/// Non-ASCII bytes are emitted verbatim (UTF-8), so they cost one byte each.
fn json_string_len(s: &str) -> usize {
    let mut len = 2; // the enclosing quotes
    for &byte in s.as_bytes() {
        len += 1 + ESCAPE_EXTRA[byte as usize] as usize;
    }
    len
}

/// Extra bytes `serde_json`'s string escaping adds for one input byte.
static ESCAPE_EXTRA: [u8; 256] = build_escape_extra();

const fn build_escape_extra() -> [u8; 256] {
    let mut table = [0u8; 256];
    table[b'"' as usize] = 1;
    table[b'\\' as usize] = 1;
    table[0x08] = 1; // \b
    table[0x09] = 1; // \t
    table[0x0a] = 1; // \n
    table[0x0c] = 1; // \f
    table[0x0d] = 1; // \r
    let mut byte = 0usize;
    while byte < 0x20 {
        if table[byte] == 0 {
            table[byte] = 5; // \u00XX: six bytes for one input byte
        }
        byte += 1;
    }
    table
}

/// Serialized length of a `serde_json::Value`, walked iteratively (a deeply
/// nested tool payload must not recurse on the stack). Exact for strings,
/// keys and structure; every scalar is counted at its widest form, so the
/// result is an upper bound of the real serialization.
fn json_value_len(value: &serde_json::Value) -> usize {
    stack_value_len(&mut vec![value])
}

/// [`json_value_len`] over an already-seeded worklist: pops each value,
/// sizes it, and pushes the children it still has to visit.
fn stack_value_len(stack: &mut Vec<&serde_json::Value>) -> usize {
    use serde_json::Value;
    let mut total = 0usize;
    while let Some(value) = stack.pop() {
        match value {
            Value::Null | Value::Bool(_) => total += 8,
            Value::Number(_) => total += 32,
            Value::String(text) => total += json_string_len(text),
            Value::Array(items) => {
                total += 2 + items.len().saturating_sub(1);
                stack.extend(items.iter());
            }
            Value::Object(map) => {
                total += 2 + map.len().saturating_sub(1);
                for (key, item) in map {
                    total += json_string_len(key) + 1; // key + ':'
                    stack.push(item);
                }
            }
        }
    }
    total
}

/// Serialized length upper bound of one tool-call content block.
fn tool_call_len(call: &crate::ai::types::ToolCall) -> usize {
    let mut len = 64 + json_string_len(&call.id) + json_string_len(&call.name);
    // The arguments map serializes as one JSON object; walk its values
    // directly (never clone them into a `Value` just to size them).
    len += 2 + call.arguments.len().saturating_sub(1);
    let mut stack: Vec<&serde_json::Value> = Vec::with_capacity(call.arguments.len());
    for (key, value) in &call.arguments {
        len += json_string_len(key) + 1; // key + ':'
        stack.push(value);
    }
    len += stack_value_len(&mut stack);
    if let Some(signature) = &call.thought_signature {
        len += json_string_len(signature) + 24;
    }
    if let Some(namespace) = &call.namespace {
        len += json_string_len(namespace) + 16;
    }
    if let Some(error) = &call.arguments_error {
        len += json_string_len(error) + 24;
    }
    len
}

/// Serialized length upper bound of one content block.
fn content_block_len(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text(text) => {
            json_string_len(&text.text)
                + 48
                + text
                    .text_signature
                    .as_ref()
                    .map_or(0, |s| json_string_len(s))
        }
        ContentBlock::Thinking(thinking) => {
            json_string_len(&thinking.thinking)
                + 64
                + thinking
                    .thinking_signature
                    .as_ref()
                    .map_or(0, |s| json_string_len(s))
        }
        ContentBlock::Image(image) => {
            json_string_len(&image.data) + json_string_len(&image.mime_type) + 64
        }
        ContentBlock::ToolCall(call) => tool_call_len(call),
    }
}

/// Serialized length upper bound of one tool-result message.
fn tool_result_len(message: &ToolResultMessage) -> usize {
    let mut len = 96 + json_string_len(&message.tool_call_id) + json_string_len(&message.tool_name);
    for block in &message.content {
        len += content_block_len(block);
    }
    if let Some(details) = &message.details {
        len += json_value_len(details) + 16;
    }
    if let Some(structured) = &message.structured_content {
        len += json_value_len(structured) + 24;
    }
    for name in &message.added_tool_names {
        len += json_string_len(name);
    }
    len
}

/// Serialized length upper bound of one message.
fn message_len(message: &Message) -> usize {
    match message {
        Message::Assistant(message) => assistant_len(message),
        Message::ToolResult(message) => tool_result_len(message),
        Message::User(message) | Message::System(message) | Message::Developer(message) => {
            let mut len = 128;
            for block in &message.content {
                len += content_block_len(block);
            }
            len
        }
    }
}

/// Serialized length upper bound of one assistant message.
fn assistant_len(message: &AssistantMessage) -> usize {
    let mut len = 256
        + json_string_len(&message.api)
        + json_string_len(&message.provider)
        + json_string_len(&message.model);
    for block in &message.content {
        len += content_block_len(block);
    }
    if let Some(model) = &message.response_model {
        len += json_string_len(model) + 24;
    }
    if let Some(id) = &message.response_id {
        len += json_string_len(id) + 24;
    }
    if let Some(error) = &message.error_message {
        len += json_string_len(error) + 24;
    }
    if let Some(reason) = &message.raw_stop_reason {
        len += json_string_len(reason) + 24;
    }
    if let Some(level) = &message.provider_thinking_level {
        len += json_string_len(level) + 32;
    }
    // Usage/diagnostics and the thinking-level enums: bounded scalar fields.
    len += message.diagnostics.len() * 256;
    len
}

/// Serialized length upper bound of one assistant stream event as the wire
/// writes it (the cumulative `partial` snapshot is NEVER serialized).
fn stream_event_len(event: &AssistantMessageEvent) -> usize {
    match event {
        AssistantMessageEvent::TextEnd { content, .. }
        | AssistantMessageEvent::ThinkingEnd { content, .. } => json_string_len(content) + 64,
        AssistantMessageEvent::ToolCallEnd { tool_call, .. } => tool_call_len(tool_call) + 64,
        AssistantMessageEvent::Done { message, .. } => assistant_len(message) + 64,
        AssistantMessageEvent::Error { error, .. } => assistant_len(error) + 64,
        AssistantMessageEvent::TextDelta { delta, .. }
        | AssistantMessageEvent::ThinkingDelta { delta, .. }
        | AssistantMessageEvent::ToolCallDelta { delta, .. } => json_string_len(delta) + 64,
        _ => 64,
    }
}

impl AgentEventWire<'_> {
    /// Upper bound of the line [`to_json_event_line`] will produce for this
    /// event. Used to size the output buffer before serializing: without it
    /// `serde_json::to_string` starts at 128 bytes and doubles, so a 200 KB
    /// tool-result line asks the allocator for ~0.5 MB across its growth
    /// steps (`PIR-BIN-099`'s per-face attribution) and copies the payload
    /// again on every growth. The bound never affects the wire bytes — a
    /// shorter estimate only means the serializer grows as before.
    fn wire_size_hint(&self) -> usize {
        let hint = match self.event {
            AgentEvent::AgentStart { .. } | AgentEvent::Compaction { .. } => 64,
            AgentEvent::AgentEnd { result } => {
                let mut len = 96 + assistant_len(&result.terminal);
                for message in &result.new_messages {
                    len += message_len(message);
                }
                len
            }
            AgentEvent::TurnStart { .. } => 64,
            AgentEvent::TurnEnd {
                message,
                tool_results,
                ..
            } => {
                let mut len = 64 + assistant_len(message);
                for result in tool_results {
                    len += tool_result_len(result);
                }
                len
            }
            AgentEvent::MessageStart { message, .. } | AgentEvent::MessageEnd { message, .. } => {
                64 + message_len(message)
            }
            AgentEvent::StreamStart { message, .. } | AgentEvent::StreamEnd { message, .. } => {
                64 + assistant_len(message)
            }
            AgentEvent::StreamEvent { event, .. } => {
                // The turn number is a `usize` (at most 20 decimal digits); 24
                // is a safe bound without formatting it into a `String`.
                64 + stream_event_len(event) + 24
            }
            AgentEvent::ToolExecutionStart { event, args, .. } => {
                64 + json_value_len(args) + tool_execution_event_len(event)
            }
            AgentEvent::ToolExecutionEnd { event, result, .. } => {
                64 + json_value_len(result) + tool_execution_event_len(event)
            }
            AgentEvent::ToolExecutionUpdate {
                call_id,
                tool_name,
                args,
                partial,
                ..
            } => {
                64 + json_string_len(call_id)
                    + json_string_len(tool_name)
                    + json_value_len(args)
                    + json_value_len(partial)
            }
        };
        // Safety slack for the fixed envelope fields (turn numbers, flags,
        // timestamps) the per-variant sums do not spell out. Kept small and
        // proportional: a fixed kilobyte per event would itself become the
        // dominant cost of a run with many small events.
        hint.saturating_add(64 + hint / 32)
    }
}

/// Serialized length contribution of a tool execution event (call identity
/// and timing fields; the large payloads are sized by the caller).
fn tool_execution_event_len(event: &crate::agent::tool_batch::ToolExecutionEvent) -> usize {
    match event {
        crate::agent::tool_batch::ToolExecutionEvent::Start {
            call_id,
            tool_name,
            parent_call_id,
            ..
        } => {
            32 + json_string_len(call_id)
                + json_string_len(tool_name)
                + parent_call_id
                    .as_ref()
                    .map_or(0, |id| json_string_len(id) + 24)
        }
        crate::agent::tool_batch::ToolExecutionEvent::End {
            call_id,
            tool_name,
            parent_call_id,
            ..
        } => {
            32 + json_string_len(call_id)
                + json_string_len(tool_name)
                + parent_call_id
                    .as_ref()
                    .map_or(0, |id| json_string_len(id) + 24)
        }
    }
}

/// Serialize one agent event straight to its wire line, byte-identical to
/// `serde_json::to_string(&to_json_event(event))` (pinned by
/// `tests/json_stream_wire.rs::every_agent_event_line_matches_the_value_construction`)
/// without materializing the payload as a `serde_json::Value` first.
///
/// The RPC/JSON fan-out seam (`PIR-BIN-099`): the payload is escaped once,
/// straight from the borrowed event, instead of once into a `Value` tree and
/// again into the line. The line buffer is pre-sized from
/// [`AgentEventWire::wire_size_hint`] so the payload is not copied again by
/// `String` growth.
#[doc(hidden)]
pub fn to_json_event_line(event: &AgentEvent) -> Result<String, serde_json::Error> {
    let wire = AgentEventWire::new(event);
    let mut buffer = Vec::with_capacity(wire.wire_size_hint());
    serde_json::to_writer(&mut buffer, &wire)?;
    // serde_json only ever writes valid UTF-8 for `Serialize` inputs; the
    // checked conversion keeps the function panic-free (and needs no unsafe).
    String::from_utf8(buffer).map_err(|error| {
        serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })
}

/// Emit one session-level event line on stdout — the JSON mode's counterpart
/// of the RPC session-event bridge. The print host uses it for the transport
/// retry lifecycle (`auto_retry_start` / `auto_retry_end`), which upstream
/// streams as ordinary session events (print-mode.ts:108-112
/// `session.subscribe` → `toJsonEvent`). A closed stdout pipe exits quietly;
/// other write failures are reported on stderr, like [`print_event`].
pub fn print_session_event(event: &SessionEvent) {
    let line = match crate::agent_session::to_session_event_line(event) {
        Ok(line) => line,
        Err(error) => {
            let _ = crate::coding_agent::output::write_stderr_line(&format!(
                "pir json: cannot serialize session event: {error}"
            ));
            return;
        }
    };
    write_json_line(&line);
}

/// Emit one already-serialized JSON line on stdout.
///
/// A closed stdout pipe ends the process quietly (`exit 0`): the consumer
/// asked for no more output, and every further event would only fail again.
/// Other write failures are reported once and exit 1. This is the fallible
/// replacement for `println!`, whose panic would be recorded as a crash.
fn write_json_line(line: &str) {
    if let Err(error) = crate::coding_agent::output::write_stdout_line(line) {
        crate::coding_agent::output::exit_stdout_error(error);
    }
}

/// Emit one live `tool_execution_update` line on stdout (upstream
/// `print-mode.ts` subscribes every session event and writes it verbatim, so
/// the tool partial-result stream reaches JSON/SDK consumers too — not just
/// extension handlers). The caller supplies the projected payload
/// (`{type, toolCallId, toolName, args, partialResult}`); a closed stdout pipe
/// exits quietly, other write failures are reported on stderr, like
/// [`print_event`].
pub fn print_tool_update(payload: &serde_json::Value) {
    let line = match serde_json::to_string(payload) {
        Ok(line) => line,
        Err(error) => {
            let _ = crate::coding_agent::output::write_stderr_line(&format!(
                "pir json: cannot serialize tool update: {error}"
            ));
            return;
        }
    };
    write_json_line(&line);
}

/// Emit one JSON event line on stdout (upstream `writeRawStdout`). A closed
/// stdout pipe exits the process quietly; other write failures are reported
/// on stderr. The event stream never panics.
pub fn print_event(event: &AgentEvent) {
    let line = match to_json_event_line(event) {
        Ok(line) => line,
        Err(error) => {
            let _ = crate::coding_agent::output::write_stderr_line(&format!(
                "pir json: cannot serialize event: {error}"
            ));
            return;
        }
    };
    write_json_line(&line);
}

/// Emit the persisted session header as the first JSON line (upstream
/// `print-mode.ts` writes `session.sessionManager.getHeader()` first). The
/// header is serialized in its on-disk shape (`{"type":"session",...}`).
pub fn print_session_header(header: &crate::session_manager::SessionHeader) {
    let Ok(line) =
        serde_json::to_string(&crate::session_manager::FileEntry::Session(header.clone()))
    else {
        return;
    };
    write_json_line(&line);
}

/// Emit the run's diagnostics summary as the final JSON line of a
/// `--mode json` run (enabled by `PI_TIMING=1`): one
/// `{"type":"run_diagnostics", ...}` object (`RunDiagnostics::to_wire_json`),
/// matching the stream's other `type`-tagged frames. A closed stdout pipe
/// exits quietly; other write failures are reported on stderr, like
/// [`print_event`].
pub fn print_run_diagnostics(summary: &crate::diagnostics::RunDiagnostics) {
    let line = match serde_json::to_string(&summary.to_wire_json()) {
        Ok(line) => line,
        Err(error) => {
            let _ = crate::coding_agent::output::write_stderr_line(&format!(
                "pir json: cannot serialize run diagnostics: {error}"
            ));
            return;
        }
    };
    write_json_line(&line);
}

#[cfg(test)]
mod wire_size_tests {
    //! The size hint is a pure capacity optimization: these tests pin that it
    //! never under-sizes the text [`to_json_event_line`] produces (an
    //! under-sized hint only means the serializer grows as before) and that
    //! the escaping math matches `serde_json`'s own output byte for byte.

    use super::*;
    use crate::ai::types::{
        AssistantRole, TextContent, ToolCall, ToolResultRole, UserMessage, UserRole,
    };

    fn text_block(text: String) -> ContentBlock {
        ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })
    }

    #[test]
    fn json_string_len_matches_serde_json_escaping() {
        let cases = [
            "",
            "plain",
            "quote\" and \\ backslash",
            "tab\t nl\n cr\r backspace\u{8} formfeed\u{c}",
            "control \u{0} \u{1f}",
            "del \u{7f}",
            "unicode: é 你好 🚀",
            "mixed \"\n\\\u{2} end",
        ];
        for case in cases {
            let actual = serde_json::to_string(case).expect("serialize string").len();
            assert_eq!(json_string_len(case), actual, "hint drifted for {case:?}");
        }
    }

    #[test]
    fn json_value_len_upper_bounds_serde_json() {
        let values = [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(-12_345_678_901_i64),
            serde_json::json!(1.5e-300),
            serde_json::json!("a \"quoted\"\nstring"),
            serde_json::json!([1, "two", {"three": [4, 5, {"six": null}]}]),
            serde_json::json!({"k": {"nested": ["x", {"deep": "y\nz"}]}}),
        ];
        for value in values {
            let actual = serde_json::to_string(&value)
                .expect("serialize value")
                .len();
            let hint = json_value_len(&value);
            assert!(
                hint >= actual,
                "hint {hint} under-sized the serialization ({actual} bytes) for {value}"
            );
        }
    }

    /// Every event whose payload carries text must get a hint at least as
    /// large as the line the borrowed wire view produces.
    #[test]
    fn wire_size_hint_covers_every_large_event() {
        let long_text = "x".repeat(4096) + "\n\"escaped\t";
        let user = Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block(long_text.clone())],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 0,
        });
        let tool_result = ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "big".into(),
            content: vec![text_block(long_text.clone())],
            details: Some(serde_json::json!({"truncation": {"content": long_text.clone()}})),
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: Some(serde_json::json!({"text": long_text.clone()})),
            nested_calls: None,
            is_error: false,
            duration_ms: None,
            timestamp: 0,
        };
        let assistant = AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![
                text_block(long_text.clone()),
                ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "big".into(),
                    arguments: [("path".to_owned(), serde_json::json!(long_text.clone()))]
                        .into_iter()
                        .collect(),
                    thought_signature: Some("sig".into()),
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: "mock".into(),
            provider: "mock".into(),
            model: "mock-model".into(),
            stop_reason: crate::ai::types::StopReason::Stop,
            ..AssistantMessage::default()
        };
        let tool_execution = crate::agent::tool_batch::ToolExecutionEvent::End {
            index: 0,
            call_id: "call-1".into(),
            tool_name: "big".into(),
            is_error: false,
            duration_ms: Some(3),
            parent_call_id: None,
        };
        let events = [
            AgentEvent::MessageStart {
                turn: 1,
                message: user.clone(),
            },
            AgentEvent::MessageEnd {
                turn: 1,
                message: Message::ToolResult(tool_result.clone()),
            },
            AgentEvent::TurnEnd {
                turn: 1,
                message: assistant.clone(),
                tool_results: vec![tool_result.clone()],
            },
            AgentEvent::StreamStart {
                turn: 1,
                message: assistant.clone(),
            },
            AgentEvent::ToolExecutionStart {
                turn: 1,
                event: crate::agent::tool_batch::ToolExecutionEvent::Start {
                    index: 0,
                    call_id: "call-1".into(),
                    tool_name: "big".into(),
                    parent_call_id: None,
                },
                args: serde_json::json!({"path": long_text}),
            },
            AgentEvent::ToolExecutionEnd {
                turn: 1,
                event: tool_execution,
                result: serde_json::json!({"content": [{"type": "text", "text": "y".repeat(4096)}]}),
            },
            AgentEvent::AgentEnd {
                result: std::sync::Arc::new(crate::agent::agent_loop::LoopResult {
                    terminal: assistant,
                    context: std::sync::Arc::new(crate::ai::types::Context {
                        system_prompt: None,
                        messages: vec![user.clone()],
                        tools: None,
                    }),
                    tool_results: Vec::new(),
                    new_messages: vec![user],
                    preamble: Vec::new(),
                    turns: 1,
                    aborted: false,
                    usage_total: 0,
                    will_retry: false,
                }),
            },
        ];
        for event in &events {
            let line = to_json_event_line(event).expect("borrowed line serializes");
            let hint = AgentEventWire::new(event).wire_size_hint();
            assert!(
                hint >= line.len(),
                "hint {hint} under-sized the {line_len}-byte line for {event:?}",
                line_len = line.len()
            );
            let value_line = serde_json::to_string(&to_json_event(event)).expect("value line");
            assert_eq!(
                line, value_line,
                "the pre-sized line drifted from the Value construction for {event:?}"
            );
        }
    }
}
