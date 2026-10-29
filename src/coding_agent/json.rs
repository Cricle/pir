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
use crate::ai::types::{AssistantMessageEvent, ContentBlock};

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

/// Emit one session-level event line on stdout — the JSON mode's counterpart
/// of the RPC session-event bridge. The print host uses it for the transport
/// retry lifecycle (`auto_retry_start` / `auto_retry_end`), which upstream
/// streams as ordinary session events (print-mode.ts:108-112
/// `session.subscribe` → `toJsonEvent`). Write failures are reported on
/// stderr, like [`print_event`].
pub fn print_session_event(event: &SessionEvent) {
    use std::io::Write;
    let line = match serde_json::to_string(&to_session_event_json(event)) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("pir json: cannot serialize session event: {error}");
            return;
        }
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        eprintln!("pir json: stdout write failed: {error}");
    }
}

/// Emit one live `tool_execution_update` line on stdout (upstream
/// `print-mode.ts` subscribes every session event and writes it verbatim, so
/// the tool partial-result stream reaches JSON/SDK consumers too — not just
/// extension handlers). The caller supplies the projected payload
/// (`{type, toolCallId, toolName, args, partialResult}`); write failures are
/// reported on stderr, like [`print_event`].
pub fn print_tool_update(payload: &serde_json::Value) {
    use std::io::Write;
    let line = match serde_json::to_string(payload) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("pir json: cannot serialize tool update: {error}");
            return;
        }
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        eprintln!("pir json: stdout write failed: {error}");
    }
}

/// Emit one JSON event line on stdout (upstream `writeRawStdout`). Write
/// failures are reported on stderr; the event stream never panics.
pub fn print_event(event: &AgentEvent) {
    use std::io::Write;
    let line = match serde_json::to_string(&to_json_event(event)) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("pir json: cannot serialize event: {error}");
            return;
        }
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        eprintln!("pir json: stdout write failed: {error}");
    }
}

/// Emit the persisted session header as the first JSON line (upstream
/// `print-mode.ts` writes `session.sessionManager.getHeader()` first). The
/// header is serialized in its on-disk shape (`{"type":"session",...}`).
pub fn print_session_header(header: &crate::session_manager::SessionHeader) {
    use std::io::Write;
    let Ok(line) =
        serde_json::to_string(&crate::session_manager::FileEntry::Session(header.clone()))
    else {
        return;
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        eprintln!("pir json: stdout write failed: {error}");
    }
}

/// Emit the run's diagnostics summary as the final JSON line of a
/// `--mode json` run (enabled by `PI_TIMING=1`): one
/// `{"type":"run_diagnostics", ...}` object (`RunDiagnostics::to_wire_json`),
/// matching the stream's other `type`-tagged frames. Write failures are
/// reported on stderr, like [`print_event`].
pub fn print_run_diagnostics(summary: &crate::diagnostics::RunDiagnostics) {
    use std::io::Write;
    let line = match serde_json::to_string(&summary.to_wire_json()) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("pir json: cannot serialize run diagnostics: {error}");
            return;
        }
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(error) = writeln!(handle, "{line}") {
        eprintln!("pir json: stdout write failed: {error}");
    }
}
