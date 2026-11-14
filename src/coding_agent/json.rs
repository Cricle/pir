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

/// Serialize one agent event straight to its wire line, byte-identical to
/// `serde_json::to_string(&to_json_event(event))` (pinned by
/// `tests/json_stream_wire.rs::every_agent_event_line_matches_the_value_construction`)
/// without materializing the payload as a `serde_json::Value` first.
///
/// The RPC/JSON fan-out seam (`PIR-BIN-099`): the payload is escaped once,
/// straight from the borrowed event, instead of once into a `Value` tree and
/// again into the line.
#[doc(hidden)]
pub fn to_json_event_line(event: &AgentEvent) -> Result<String, serde_json::Error> {
    serde_json::to_string(&AgentEventWire::new(event))
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
