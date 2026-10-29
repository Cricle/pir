//! Typed SDK events and the bridge from the internal loop/plugin dispatch.
//!
//! [`SdkEvent`] is the stable, typed projection of the internal
//! [`AgentEvent`](crate::agent::agent_loop::AgentEvent), plugin
//! `tool_execution_update` payloads, and the session-level
//! [`SessionEvent`]s (`queue_update`, `compaction_start/_end`,
//! `entry_appended`, `session_info_changed`, `thinking_level_changed`,
//! `bash_execution_update`, `auto_retry_start/_end`,
//! `summarization_retry_scheduled/_attempt_start/_finished`,
//! `background_tool_end`).
//! Fields are typed wherever the internal types are public
//! ([`AssistantMessage`], tool-call ids); raw [`serde_json::Value`] appears
//! only for tool arguments/partial results and entry snapshots, whose shape is
//! tool/transcript-defined.
//!
//! [`SdkEvent::from_session_event`] is the public projection for hosts that
//! subscribe to a session directly; [`crate::sdk::Agent`] uses the same
//! function to mirror session events onto an active prompt stream. Its
//! loop-event counterpart is [`from_loop_event`], which projects the FULL
//! [`AgentEvent`] stream: non-assistant message lifecycles
//! (`TranscriptMessageStart`/`TranscriptMessageEnd`), the assistant stream's
//! tool-call frames, the `done`/`error` terminal frames, and the per-run
//! `AgentEnd`.

use std::sync::{Arc, Mutex};

use serde_json::Value as JsonValue;

use crate::agent::agent_loop::AgentEvent;
use crate::agent_session::SessionEvent;
use crate::ai::types::{
    AssistantMessage, AssistantMessageEvent, ErrorReason, Message, TerminalReason, ToolCall,
};
use crate::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};

use super::outcome::AgentOutcome;

/// One lifecycle event of an agent run, streamed by
/// [`crate::sdk::Agent::prompt_stream`].
///
/// The sequence for a simple one-turn run is
/// `TurnStart → MessageStart → MessageUpdate*/MessageDone → MessageEnd →
/// TurnEnd → AgentEnd → AgentSettled`. The run's starting user message (and
/// any other non-assistant transcript message) is announced before the first
/// `TurnStart` by `TranscriptMessageStart`/`TranscriptMessageEnd`. A
/// tool-calling turn inserts
/// `ToolExecutionStart → ToolExecutionUpdate* → ToolExecutionEnd` between
/// `MessageEnd` and `TurnEnd`; the provider's incremental tool-call frames
/// arrive during the message stream as `ToolCallStart → ToolCallDelta* →
/// ToolCallEnd`, and a failed stream ends it with `MessageError` instead of
/// `MessageDone`. The next round-trip starts with a new `TurnStart`. A
/// threshold compaction surfaces as `Compaction` before the affected
/// `TurnStart`; a prompt that never starts yields `PromptRejected` followed
/// by `AgentSettled`.
///
/// `AgentEnd` is the end of one low-level agent-loop run (upstream
/// `agent_end`); the session may run the loop again after it. `AgentSettled`
/// is the SESSION settle (upstream `agent_settled`), emitted after the whole
/// prompt run — including any `agent_before_settle` continuation or
/// overflow-recovery run — not at the end of each low-level agent-loop run.
///
/// A stream produced by [`crate::sdk::Agent::prompt_stream_with_outcome`]
/// additionally carries [`SdkEvent::AgentOutcome`] immediately before the
/// final `AgentSettled`.
///
/// Delivery is best-effort for intermediate events under consumer lag: the
/// stream rides a bounded channel (configurable per agent), and pushes never
/// block the agent loop — when the channel is full the event is dropped. The
/// terminal events (`AgentSettled`, `AgentOutcome`, and the rejection pair)
/// are delivered with backpressure after the run settles. The stream ends
/// when the sender is closed.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SdkEvent {
    /// A new loop turn started (the model is about to be requested again,
    /// counting every agentic round-trip inside one prompt).
    TurnStart {
        /// One-based index of the turn inside the current prompt run.
        turn_index: usize,
    },

    /// A loop turn finished: the assistant message (and, when it requested
    /// tools, its tool results) completed. Emitted after
    /// [`SdkEvent::MessageEnd`].
    TurnEnd {
        /// One-based index of the turn inside the current prompt run.
        turn_index: usize,
        /// The assistant message that completed the turn.
        message: crate::ai::types::AssistantMessage,
        /// This turn's tool-result messages in assistant source order
        /// (upstream `turn_end.toolResults`); empty when the turn issued no
        /// tool calls (and for synthetic terminal turns).
        tool_results: Vec<crate::ai::types::ToolResultMessage>,
    },

    /// The assistant message stream started.
    MessageStart,

    /// A streaming content delta arrived.
    MessageUpdate {
        /// The incremental text.
        delta: String,
        /// `true` for reasoning/thinking deltas, `false` for answer text.
        thinking: bool,
    },

    /// The assistant message stream ended; the message is complete (it may
    /// still request tool calls).
    MessageEnd {
        /// The completed assistant message.
        message: crate::ai::types::AssistantMessage,
    },

    /// A NON-assistant transcript message entered the run (upstream
    /// `message_start` for system/user/developer/toolResult/custom messages;
    /// the loop announces these outside the assistant provider stream).
    ///
    /// The assistant message lifecycle is [`SdkEvent::MessageStart`] →
    /// [`SdkEvent::MessageUpdate`]/[`SdkEvent::MessageDone`] →
    /// [`SdkEvent::MessageEnd`]. `message` is the role-bearing message
    /// verbatim, so a host can render prompts, tool results, and custom
    /// messages from the lifecycle alone.
    TranscriptMessageStart {
        /// The loop turn the message is announced for; `0` marks the run's
        /// starting prompt/preamble.
        turn: usize,
        /// The announced message (its `role` selects user/system/developer/
        /// toolResult).
        message: Message,
    },

    /// The paired `message_end` for [`SdkEvent::TranscriptMessageStart`]
    /// (upstream `message_end` for non-assistant messages). After a
    /// `message_end` handler replacement this is the value that entered the
    /// transcript.
    TranscriptMessageEnd {
        /// The loop turn the message belongs to.
        turn: usize,
        /// The finalized message.
        message: Message,
    },

    /// A tool-call content block started streaming (upstream
    /// `message_update` with a `toolcall_start` assistant event).
    ToolCallStart {
        /// The loop turn the stream belongs to.
        turn: usize,
        /// Index of the tool-call block in the assistant message content.
        content_index: usize,
    },

    /// One tool-call argument fragment arrived (upstream `message_update`
    /// with a `toolcall_delta` assistant event). Concatenating the deltas in
    /// arrival order yields the provider's JSON arguments; the parsed final
    /// value is [`SdkEvent::ToolCallEnd`]'s `tool_call`.
    ToolCallDelta {
        /// The loop turn the stream belongs to.
        turn: usize,
        /// Index of the tool-call block.
        content_index: usize,
        /// The raw JSON argument fragment.
        delta: String,
    },

    /// A tool-call content block finished streaming (upstream
    /// `message_update` with a `toolcall_end` assistant event).
    ToolCallEnd {
        /// The loop turn the stream belongs to.
        turn: usize,
        /// Index of the tool-call block.
        content_index: usize,
        /// The complete parsed tool call (id, name, arguments).
        tool_call: ToolCall,
    },

    /// The assistant message completed successfully (upstream
    /// `message_update` with a `done` assistant event). Emitted before
    /// [`SdkEvent::MessageEnd`], which carries the loop-finalized message.
    MessageDone {
        /// The loop turn the stream belongs to.
        turn: usize,
        /// The terminal reason (`stop`/`length`/`toolUse`/`deferred`).
        reason: TerminalReason,
        /// The provider's completed message.
        message: AssistantMessage,
    },

    /// The assistant message stream ended in an error (upstream
    /// `message_update` with an `error` assistant event). Emitted before
    /// [`SdkEvent::MessageEnd`], which carries the same errored message after
    /// loop finalization.
    MessageError {
        /// The loop turn the stream belongs to.
        turn: usize,
        /// `error` or `aborted`.
        reason: ErrorReason,
        /// The provider's errored message.
        message: AssistantMessage,
    },

    /// A tool call started executing.
    ToolExecutionStart {
        /// Provider tool-call id.
        call_id: String,
        /// Tool name as requested by the model.
        tool_name: String,
        /// Parent call id for nested (tool-issued) calls.
        parent_call_id: Option<String>,
        /// The call's arguments in the same wire shape as the internal loop's
        /// `tool_execution_start.args` (the model's argument object; `null`
        /// when arguments were truncated or unavailable). Renders the call
        /// from the start frame alone.
        args: JsonValue,
    },

    /// A long-running tool reported a partial result (upstream tool
    /// `onUpdate`). The payload shape is tool-defined.
    ///
    /// Updates are enqueued on the same ordered run channel as loop events and
    /// are best-effort: an update is dropped when that bounded channel is
    /// full. Source-side update dispatch is fire-and-forget, so an update
    /// racing the turn end may be observed after [`SdkEvent::ToolExecutionEnd`];
    /// the end event's `result` is authoritative.
    ToolExecutionUpdate {
        /// Provider tool-call id.
        tool_call_id: String,
        /// Tool name, when the update carries it.
        tool_name: Option<String>,
        /// Parent call id for nested (tool-issued) calls.
        parent_call_id: Option<String>,
        /// The call's arguments in the same wire shape as
        /// [`SdkEvent::ToolExecutionStart::args`] (upstream
        /// `ToolExecutionUpdateEvent.args`); `null` when the source update
        /// carried none. Rendering a progress line needs the call identity, not
        /// a second copy of the start frame, but carrying it here keeps
        /// start/update/end self-describing.
        args: JsonValue,
        /// Partial result JSON (`{content, details, isError}`), tool-defined.
        partial: JsonValue,
    },

    /// A tool call finished executing.
    ToolExecutionEnd {
        /// Provider tool-call id.
        call_id: String,
        /// Tool name as requested by the model.
        tool_name: String,
        /// Whether the result is treated as an error.
        is_error: bool,
        /// Monotonic execution duration, when the tool ran.
        duration_ms: Option<u64>,
        /// Parent call id for nested (tool-issued) calls.
        parent_call_id: Option<String>,
        /// The finished result in upstream `AgentToolResult` wire shape
        /// (`content` block array, optional `details`/`structuredContent`/
        /// `terminate`), the same value as the internal loop's
        /// `tool_execution_end.result`.
        result: JsonValue,
    },

    /// The context was compacted (threshold-triggered inside a run).
    Compaction,

    /// The session's steering/follow-up queues changed (upstream
    /// `queue_update`, agent-session.ts:1047): the full pending texts of both
    /// queues after a push, drain, or clear. The internal host-only
    /// "delivered by this update" lists are not projected — an SDK consumer
    /// sees the queue state, exactly like the upstream event.
    QueueUpdate {
        /// Pending steering message texts (upstream `steering`).
        steering: Vec<String>,
        /// Pending follow-up message texts (upstream `followUp`).
        follow_up: Vec<String>,
    },

    /// A compaction run started (upstream `compaction_start`), with its
    /// trigger: `"manual"`, `"threshold"`, or `"overflow"`.
    CompactionStart {
        /// The trigger (upstream `reason`).
        reason: String,
    },

    /// A compaction run finished (upstream `compaction_end`). `result` is the
    /// upstream `CompactionResult` wire shape on success; `error_message` is
    /// set when the run failed instead.
    CompactionEnd {
        /// The trigger (upstream `reason`).
        reason: String,
        /// The compaction result in its upstream wire shape, when it
        /// succeeded.
        result: Option<JsonValue>,
        /// Whether the run was aborted.
        aborted: bool,
        /// Whether the caller is expected to retry (upstream `willRetry`).
        will_retry: bool,
        /// The failure detail, when the run failed.
        error_message: Option<String>,
    },

    /// One entry was appended to the session transcript (upstream
    /// `entry_appended`): a message entry (`{id, message}`) or a non-message
    /// entry in its wire shape. Use this to mirror the transcript without
    /// polling [`crate::sdk::Agent::transcript`].
    EntryAppended {
        /// The appended `SessionEntry` in its upstream wire shape.
        entry: JsonValue,
    },

    /// The session's display name changed (upstream `session_info_changed`);
    /// `name` is `None` when it was cleared.
    SessionInfoChanged {
        /// The session's current name.
        name: Option<String>,
    },

    /// The effective thinking level changed (upstream
    /// `thinking_level_changed`), in its wire form (`"low"`, `"high"`, ...).
    ThinkingLevelChanged {
        /// The new effective level.
        level: String,
    },

    /// One streaming chunk of an out-of-loop bash execution (upstream
    /// `bash_execution_update`); `id` is the execution id when the caller
    /// supplied one.
    BashExecutionUpdate {
        /// The command id (upstream `id`).
        id: Option<String>,
        /// The incremental output chunk.
        delta: String,
    },

    /// A transport retry was scheduled (upstream `auto_retry_start`): the
    /// request will be retried after `delay_ms`.
    AutoRetryStart {
        /// 1-based ordinal of the retry about to run.
        attempt: u32,
        /// The configured retry budget.
        max_attempts: u32,
        /// The wait before the retry, in milliseconds.
        delay_ms: u64,
        /// The error that triggered the retry.
        error_message: String,
    },

    /// A retried request resolved (upstream `auto_retry_end`).
    AutoRetryEnd {
        /// Whether the retried request eventually succeeded.
        success: bool,
        /// The retry ordinal that succeeded (or the number scheduled).
        attempt: u32,
        /// The final error text; `None` on success.
        final_error: Option<String>,
    },

    /// A summarizer (compaction / branch-summary) retry was scheduled
    /// (upstream `summarization_retry_scheduled`), with the backoff that is
    /// about to be slept.
    SummarizationRetryScheduled {
        /// 1-based retry ordinal.
        attempt: u32,
        /// The summarizer retry budget.
        max_attempts: u32,
        /// The wait before the retry, in milliseconds.
        delay_ms: u64,
        /// The error that triggered the retry.
        error_message: String,
    },

    /// A summarizer retry backoff elapsed and the call is being re-issued
    /// (upstream `summarization_retry_attempt_start`). `source` is
    /// `"branchSummary"` or `"compaction"`; `reason` carries the compaction
    /// trigger (`"manual"`/`"threshold"`/`"overflow"`) for the latter.
    SummarizationRetryAttemptStart {
        /// Which summarizer owns the retry (upstream `source`).
        source: String,
        /// The compaction trigger when `source == "compaction"`.
        reason: Option<String>,
    },

    /// The summarizer retry loop closed (upstream
    /// `summarization_retry_finished`).
    SummarizationRetryFinished,

    /// A background tool call finished after the turn that requested it
    /// (pir extension, `TOOL-BACKGROUND-SCOPE.md` §2.4.3). The batch already
    /// answered with a placeholder; this carries the real result so a host
    /// can replace it in place.
    BackgroundToolEnd {
        /// Provider tool-call id of the detached call.
        call_id: String,
        /// Tool name.
        tool_name: String,
        /// The requesting call id for nested detached calls.
        parent_call_id: Option<String>,
        /// Whether the real result is an error.
        is_error: bool,
        /// Execution duration of the real call.
        duration_ms: Option<u64>,
        /// The real result's text output.
        output: String,
        /// The real result's image blocks.
        images: Vec<crate::ai::types::ImageContent>,
        /// The real result's `details` payload, verbatim.
        details: Option<JsonValue>,
        /// The real result's `structuredContent` payload, verbatim.
        structured_content: Option<JsonValue>,
        /// The handler-reported usage for this completion, if any.
        usage: Option<crate::ai::types::Usage>,
    },

    /// The prompt was rejected before starting (most often because another
    /// run is already streaming on this agent). The rejection detail is
    /// human-readable; no run events follow and the stream ends right after
    /// the trailing [`SdkEvent::AgentSettled`].
    PromptRejected {
        /// Why the prompt did not start.
        message: String,
    },

    /// One low-level agent-loop run ended (upstream `agent_end`). `messages`
    /// is EVERY message that run appended, in append order (the upstream
    /// `agent_end.messages` slice: assistant messages, tool results, and
    /// injected prompts — not the full transcript).
    ///
    /// The session may run the loop again after this event
    /// (`agent_before_settle` continuation, overflow recovery), so this is
    /// not terminal: [`SdkEvent::AgentSettled`] still ends the prompt run.
    AgentEnd {
        /// Messages the run appended, in append order.
        messages: Vec<Message>,
        /// Upstream session `agent_end.willRetry`: whether the session will
        /// re-run the loop after this event (the loop's own retry verdict at
        /// the run boundary).
        will_retry: bool,
    },

    /// The complete terminal outcome of the run (terminal message, tool
    /// results, usage, turns, aborted flag).
    ///
    /// Only [`crate::sdk::Agent::prompt_stream_with_outcome`] emits this
    /// event; it arrives immediately before the final
    /// [`SdkEvent::AgentSettled`]. Plain
    /// [`Agent::prompt_stream`](crate::sdk::Agent::prompt_stream) skips
    /// outcome capture (and its clone of the terminal message and tool
    /// results) entirely.
    AgentOutcome(Box<AgentOutcome>),

    /// The prompt run settled; no more events follow on this stream.
    AgentSettled {
        /// Whether the run stopped because it was aborted.
        aborted: bool,
    },
}

impl SdkEvent {
    /// Project one session-level [`SessionEvent`] into its SDK event.
    ///
    /// [`crate::sdk::Agent`] subscribes to the session event stream and pushes
    /// these onto the active prompt stream, so an embedder that only consumes
    /// [`SdkEvent`] also observes the out-of-loop events (queue changes,
    /// compaction lifecycle, transcript appends, retries, bash output).
    ///
    /// `SessionEvent::AgentSettled` maps to `None`: the SDK stream's terminal
    /// [`SdkEvent::AgentSettled`] is emitted once by the run driver after the
    /// whole prompt settles, and projecting the session event too would
    /// duplicate it. Exposed publicly because a host that subscribes with
    /// [`crate::agent_session::AgentSession::on_session_event`] directly can
    /// reuse the exact same projection.
    pub fn from_session_event(event: &SessionEvent) -> Option<Self> {
        match event {
            SessionEvent::QueueUpdate {
                steering,
                follow_up,
                ..
            } => Some(Self::QueueUpdate {
                steering: steering.clone(),
                follow_up: follow_up.clone(),
            }),
            SessionEvent::CompactionStart { reason } => Some(Self::CompactionStart {
                reason: (*reason).to_owned(),
            }),
            SessionEvent::CompactionEnd {
                reason,
                result,
                aborted,
                will_retry,
                error_message,
            } => Some(Self::CompactionEnd {
                reason: (*reason).to_owned(),
                result: result.clone(),
                aborted: *aborted,
                will_retry: *will_retry,
                error_message: error_message.clone(),
            }),
            SessionEvent::AgentSettled { .. } => None,
            SessionEvent::EntryAppended { entry } => Some(Self::EntryAppended {
                entry: entry.clone(),
            }),
            SessionEvent::SessionInfoChanged { name } => {
                Some(Self::SessionInfoChanged { name: name.clone() })
            }
            SessionEvent::ThinkingLevelChanged { level } => Some(Self::ThinkingLevelChanged {
                level: level.clone(),
            }),
            SessionEvent::BashExecutionUpdate { id, delta } => Some(Self::BashExecutionUpdate {
                id: id.clone(),
                delta: delta.clone(),
            }),
            SessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => Some(Self::AutoRetryStart {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_ms: *delay_ms,
                error_message: error_message.clone(),
            }),
            SessionEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => Some(Self::AutoRetryEnd {
                success: *success,
                attempt: *attempt,
                final_error: final_error.clone(),
            }),
            SessionEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => Some(Self::SummarizationRetryScheduled {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_ms: *delay_ms,
                error_message: error_message.clone(),
            }),
            SessionEvent::SummarizationRetryAttemptStart { .. } => {
                // The source enum lives in the private `agent_session::events`
                // module; read its upstream wire view instead of reaching for
                // the type (the same shape every other face consumes).
                let wire = crate::agent_session::session_event_to_json(event);
                Some(Self::SummarizationRetryAttemptStart {
                    source: wire["source"].as_str().unwrap_or_default().to_owned(),
                    reason: wire
                        .get("reason")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned),
                })
            }
            SessionEvent::SummarizationRetryFinished => Some(Self::SummarizationRetryFinished),
            SessionEvent::BackgroundToolEnd {
                call_id,
                tool_name,
                parent_call_id,
                is_error,
                duration_ms,
                output,
                images,
                details,
                structured_content,
                usage,
            } => Some(Self::BackgroundToolEnd {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                parent_call_id: parent_call_id.clone(),
                is_error: *is_error,
                duration_ms: *duration_ms,
                output: output.clone(),
                images: images.clone(),
                details: details.clone(),
                structured_content: structured_content.clone(),
                usage: *usage,
            }),
        }
    }
}

/// Project one internal loop event into its SDK event, or `None` when the
/// loop event has no SDK-facing projection (`AgentStart` bookkeeping and the
/// assistant stream's content-block start/end markers).
///
/// `AgentEnd` is projected to [`SdkEvent::AgentEnd`], NOT to
/// [`SdkEvent::AgentSettled`]: it marks the end of one low-level agent-loop
/// run, but the session may still issue further provider requests
/// (`agent_before_settle` continuation, overflow recovery) before the prompt
/// run settles. The SDK emits the terminal `AgentSettled` only after
/// [`crate::agent_session::AgentSession::prompt`] returns (upstream
/// `agent_settled`, not `agent_end`).
///
/// `capture_outcome` gates the [`LoopOutcome`] capture on `AgentEnd`: the
/// plain streaming path passes `false`, so the terminal message and the run's
/// tool-result messages are never cloned for an outcome nobody reads. Only
/// [`crate::sdk::Agent::prompt`] and
/// [`crate::sdk::Agent::prompt_stream_with_outcome`] capture.
///
/// `project_agent_end` gates the [`SdkEvent::AgentEnd`] payload: it is `true`
/// only when a live prompt stream can deliver the event, so a non-streaming
/// [`crate::sdk::Agent::prompt`] never clones the run's message slice twice
/// (once for the outcome, once for an event with no sink).
pub(crate) fn from_loop_event(
    event: AgentEvent,
    capture_outcome: bool,
    project_agent_end: bool,
) -> (Option<SdkEvent>, Option<LoopOutcome>) {
    match event {
        AgentEvent::AgentStart { .. } => (None, None),
        AgentEvent::StreamEvent { turn, event } => (stream_delta_event(turn, event), None),
        AgentEvent::TurnStart { turn, .. } => {
            (Some(SdkEvent::TurnStart { turn_index: turn }), None)
        }
        AgentEvent::StreamStart { .. } => (Some(SdkEvent::MessageStart), None),
        AgentEvent::StreamEnd { message, .. } => (Some(SdkEvent::MessageEnd { message }), None),
        // `AgentEvent::MessageStart`/`MessageEnd` announce NON-assistant
        // transcript messages (user/tool-result/custom). The assistant stream
        // lifecycle is `StreamStart`/`StreamEnd` (projected as
        // `MessageStart`/`MessageEnd`); these are projected as the separate
        // `TranscriptMessage*` pair so both lifecycles stay distinct.
        AgentEvent::MessageStart { turn, message } => (
            Some(SdkEvent::TranscriptMessageStart { turn, message }),
            None,
        ),
        AgentEvent::MessageEnd { turn, message } => {
            (Some(SdkEvent::TranscriptMessageEnd { turn, message }), None)
        }
        AgentEvent::TurnEnd {
            turn,
            message,
            tool_results,
        } => (
            Some(SdkEvent::TurnEnd {
                turn_index: turn,
                message,
                tool_results,
            }),
            None,
        ),
        AgentEvent::ToolExecutionStart { event, args, .. } => match event {
            crate::agent::tool_batch::ToolExecutionEvent::Start {
                call_id,
                tool_name,
                parent_call_id,
                ..
            } => (
                Some(SdkEvent::ToolExecutionStart {
                    call_id,
                    tool_name,
                    parent_call_id,
                    args,
                }),
                None,
            ),
            crate::agent::tool_batch::ToolExecutionEvent::End { .. } => (None, None),
        },
        AgentEvent::ToolExecutionEnd { event, result, .. } => match event {
            crate::agent::tool_batch::ToolExecutionEvent::End {
                call_id,
                tool_name,
                is_error,
                duration_ms,
                parent_call_id,
                ..
            } => (
                Some(SdkEvent::ToolExecutionEnd {
                    call_id,
                    tool_name,
                    is_error,
                    duration_ms,
                    parent_call_id,
                    result,
                }),
                None,
            ),
            crate::agent::tool_batch::ToolExecutionEvent::Start { .. } => (None, None),
        },
        AgentEvent::ToolExecutionUpdate {
            call_id,
            tool_name,
            parent_call_id,
            args,
            partial,
            ..
        } => (
            Some(SdkEvent::ToolExecutionUpdate {
                tool_call_id: call_id,
                tool_name: Some(tool_name),
                parent_call_id,
                args,
                partial,
            }),
            None,
        ),
        AgentEvent::Compaction { .. } => (Some(SdkEvent::Compaction), None),
        AgentEvent::AgentEnd { result } => {
            // `result` is shared with the run (and possibly the caller), so a
            // field move is not available. Clone ONLY the owned fields the SDK
            // outcome reports — and only when the caller asked for an outcome:
            // the plain streaming path reads `aborted` and skips the clones
            // entirely. `Arc::unwrap_or_clone` would also duplicate the whole
            // context, which the SDK outcome does not keep.
            let outcome = capture_outcome.then(|| LoopOutcome {
                terminal: result.terminal.clone(),
                tool_results: result.tool_results.clone(),
                messages: result.new_messages.clone(),
                turns: result.turns,
                aborted: result.aborted,
            });
            // Terminal settlement is the caller's job: the session may re-enter
            // the loop (`agent_before_settle` continuation) after this event.
            let event = project_agent_end.then(|| SdkEvent::AgentEnd {
                messages: result.new_messages.clone(),
                // Same verdict the upstream session event carries: the loop
                // already decided whether a retryable terminal is worth
                // re-issuing at the session boundary.
                will_retry: result.will_retry,
            });
            (event, outcome)
        }
    }
}

/// Project a provider stream delta into its SDK event.
///
/// Text/thinking deltas keep their flat [`SdkEvent::MessageUpdate`] shape;
/// the tool-call frames get their own typed variants; `done`/`error` become
/// [`SdkEvent::MessageDone`]/[`SdkEvent::MessageError`]. The stream `start`
/// frame is already the assistant [`SdkEvent::MessageStart`] (the loop emits
/// `StreamStart` with the same partial), and the text/thinking block
/// start/end markers are not projected: the deltas plus the final message
/// carry everything a host renders.
fn stream_delta_event(turn: usize, event: AssistantMessageEvent) -> Option<SdkEvent> {
    match event {
        AssistantMessageEvent::TextDelta { delta, .. } => Some(SdkEvent::MessageUpdate {
            delta,
            thinking: false,
        }),
        AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(SdkEvent::MessageUpdate {
            delta,
            thinking: true,
        }),
        AssistantMessageEvent::ToolCallStart { content_index, .. } => {
            Some(SdkEvent::ToolCallStart {
                turn,
                content_index,
            })
        }
        AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => Some(SdkEvent::ToolCallDelta {
            turn,
            content_index,
            delta,
        }),
        AssistantMessageEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => Some(SdkEvent::ToolCallEnd {
            turn,
            content_index,
            tool_call,
        }),
        AssistantMessageEvent::Done { reason, message } => Some(SdkEvent::MessageDone {
            turn,
            reason,
            message,
        }),
        AssistantMessageEvent::Error { reason, error } => Some(SdkEvent::MessageError {
            turn,
            reason,
            message: error,
        }),
        AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::TextStart { .. }
        | AssistantMessageEvent::TextEnd { .. }
        | AssistantMessageEvent::ThinkingStart { .. }
        | AssistantMessageEvent::ThinkingEnd { .. } => None,
    }
}

/// The terminal state of one prompt run, captured from the loop's
/// `AgentEnd` event (the only place tool results, the run's own message slice,
/// and the aborted flag are available to observers).
pub(crate) struct LoopOutcome {
    pub terminal: crate::ai::types::AssistantMessage,
    pub tool_results: Vec<crate::ai::types::ToolResultMessage>,
    /// Every message the run appended, in append order (upstream
    /// `agent_end.messages`).
    pub messages: Vec<crate::ai::types::Message>,
    pub turns: usize,
    pub aborted: bool,
}

/// The live SDK event sink shared between the per-run event handler, the
/// bridge plugin, and the stream consumer. `None` when no streaming run is in
/// flight (plain [`crate::sdk::Agent::prompt`] leaves it empty).
///
/// The channel is the ONE bounded async channel of the run (no intermediate
/// sync stage and no forwarder thread), so loop events and bridge
/// `tool_execution_update` payloads share a single ordered queue.
pub(crate) type SharedSink = Arc<Mutex<Option<tokio::sync::mpsc::Sender<SdkEvent>>>>;

/// Push one event to the live sink, if a streaming run is attached.
///
/// Non-blocking by contract: [`tokio::sync::mpsc::Sender::try_send`] never
/// awaits, so holding the sink mutex across the push cannot block the agent
/// loop or deadlock a `current_thread` runtime.
///
/// The push happens UNDER the sink lock deliberately (P2-M): a push that
/// cloned the sender and then raced a stream's detach could otherwise enqueue
/// after the terminal `AgentSettled`. With the lock held on both sides, every
/// push either completes before the detach or observes `None` after it — no
/// event can follow the stream's terminal pair.
///
/// Drop policy: the run channel is bounded (default
/// [`super::agent::EVENT_CHANNEL_CAPACITY`], configurable through
/// [`crate::sdk::Agent::set_event_channel_capacity`]). When a slow consumer
/// has filled it, the NEWEST event — this one — is dropped and the run
/// continues; a detached/closed stream is ignored for the same reason.
/// Streaming is consequently best-effort under sustained lag; authoritative
/// terminal state is available from [`crate::sdk::Agent::prompt`]'s
/// [`AgentOutcome`] or, on a stream, from
/// [`crate::sdk::Agent::prompt_stream_with_outcome`]'s
/// [`SdkEvent::AgentOutcome`] (itself best-effort like every streamed event).
pub(crate) fn push_to_sink(sink: &SharedSink, event: SdkEvent) {
    let guard = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(sender) = guard.as_ref() {
        let _ = sender.try_send(event);
    }
}

/// Project a plugin-host `tool_execution_update` payload into an SDK event.
pub(crate) fn tool_update_event(payload: &JsonValue) -> Option<SdkEvent> {
    let partial = payload.get("partialResult")?.clone();
    Some(SdkEvent::ToolExecutionUpdate {
        tool_call_id: payload
            .get("toolCallId")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned(),
        tool_name: payload
            .get("toolName")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        parent_call_id: payload
            .get("parentToolCallId")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        args: payload.get("args").cloned().unwrap_or(JsonValue::Null),
        partial,
    })
}

/// Internal plugin that rides the unified plugin dispatch and forwards
/// `tool_execution_update` events to the live SDK sink. Registered
/// automatically by every SDK-built agent (last in the plugin order).
pub(crate) struct SdkBridgePlugin {
    info: PluginInfo,
    sink: SharedSink,
}

impl SdkBridgePlugin {
    pub fn new(sink: SharedSink) -> Self {
        Self {
            info: PluginInfo::inline("pir-sdk-bridge"),
            sink,
        }
    }
}

impl AgentPlugin for SdkBridgePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["tool_execution_update".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        // Check the event name BEFORE touching the payload: this plugin is
        // attached to every SDK agent and (under the unified dispatch) is
        // handed every event, including `session_tree` whose payload is the
        // whole session forest. Cloning the payload first made every SDK
        // agent a deep-clone consumer of the tree; the borrow parse below
        // costs nothing for unrelated events.
        if event.name.as_ref() == "tool_execution_update" {
            if let Some(sdk_event) = tool_update_event(event.payload) {
                push_to_sink(&self.sink, sdk_event);
            }
        }
        Box::pin(async { PluginEventOutcome::None })
    }
}
