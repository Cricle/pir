use super::*;

// ---------------------------------------------------------------------------
// Session-level events outside the agent-loop event enum (upstream
// `AgentSessionEvent` additions: queue_update / compaction_start /
// compaction_end / bash_execution_update / auto_retry_start /
// auto_retry_end, agent-session.ts:196-237)
// ---------------------------------------------------------------------------

/// Handler for [`SessionEvent`] values (upstream `session.subscribe`).
pub type SessionEventHandler = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// One session-level event emitted outside the agent loop's [`AgentEvent`]
/// stream (upstream `AgentSessionEvent`, agent-session.ts:196-237). Hosts
/// subscribe with [`AgentSession::on_session_event`]; the RPC host forwards
/// every event to the stdout protocol (modes/json-event.ts
/// `JsonAgentSessionEvent`).
// Events are short-lived one-shot values built at dispatch points; boxing the
// large variant would add an allocation per tool completion without a measured
// benefit, so the public enum shape stays as-is.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The steering/follow-up queues changed (upstream `queue_update`,
    /// agent-session.ts:1047): both queues' full pending texts.
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
        /// Texts delivered by THIS update (internal host signal; not part of
        /// the upstream `queue_update` wire shape). The TUI uses it to add
        /// the user bubble exactly when the message is actually delivered.
        delivered_steering: Vec<String>,
        delivered_follow_up: Vec<String>,
    },
    /// A compaction run started (upstream `compaction_start`,
    /// agent-session.ts:2767/3119).
    CompactionStart {
        /// `manual` | `threshold` | `overflow` (upstream `reason`).
        reason: &'static str,
    },
    /// A compaction run finished (upstream `compaction_end`,
    /// agent-session.ts:2878/2891/3024/3205/3222).
    CompactionEnd {
        reason: &'static str,
        /// The compaction result on success (upstream `CompactionResult` |
        /// undefined), pre-serialized in its upstream wire shape.
        result: Option<JsonValue>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    /// The whole prompt run settled (upstream `agent_settled`,
    /// AgentSessionEvent union agent-session.ts:203; emitted from
    /// agent-session.ts:1084-1085): after every low-level loop run,
    /// `agent_before_settle` continuation, and overflow recovery, on SUCCESS
    /// and FAILURE alike.
    AgentSettled {
        /// Whether the run stopped because it was aborted.
        aborted: bool,
    },
    /// One entry was appended to the session transcript (upstream
    /// `entry_appended`, AgentSessionEvent union agent-session.ts:210;
    /// emitted at agent-session.ts:496/834/1027/1269/3470).
    EntryAppended {
        /// The appended `SessionEntry` in its upstream wire shape
        /// (`{id, message}` for message entries).
        entry: JsonValue,
    },
    /// The session's display name changed (upstream `session_info_changed`,
    /// AgentSessionEvent union agent-session.ts:211; emitted from
    /// agent-session.ts:4004): `name` is the session's current name, `None`
    /// when it was cleared (the wire omits the field, matching upstream's
    /// `JSON.stringify` of `name: undefined`).
    SessionInfoChanged {
        /// The session's final name; `None` = cleared.
        name: Option<String>,
    },
    /// The effective thinking level changed (upstream
    /// `thinking_level_changed`, AgentSessionEvent union
    /// agent-session.ts:212; emitted from agent-session.ts:2692).
    ThinkingLevelChanged {
        /// The new effective level in its wire form (`ThinkingLevel`, e.g.
        /// `"high"`).
        level: String,
    },
    /// One streaming chunk of an out-of-loop bash execution (upstream
    /// `bash_execution_update`, agent-session.ts:3862).
    BashExecutionUpdate {
        /// The command id (upstream `options.id`; the RPC request id).
        id: Option<String>,
        delta: String,
    },
    /// A background tool call finished, after the turn that started it
    /// (`TOOL-BACKGROUND-SCOPE.md` §2.4.3). The batch that requested the call
    /// already answered with a placeholder; this event carries the REAL
    /// result so hosts can replace the pending block in place.
    BackgroundToolEnd {
        /// Provider tool-call id of the detached call.
        call_id: String,
        /// Tool name.
        tool_name: String,
        /// The requesting call id when another tool made the detached call
        /// (`ctx.executeTool` with `background: true`); `None` for
        /// model-issued calls. Serialized as `parentToolCallId` when present,
        /// the same attribution the nested start/end frames carry.
        parent_call_id: Option<String>,
        /// Whether the real result is an error.
        is_error: bool,
        /// Monotonic execution duration of the real call.
        duration_ms: Option<u64>,
        /// The real result's text output.
        output: String,
        /// The real result's image blocks, carried so a host can rebuild the
        /// full `AgentToolResult` content rather than only `output`;
        /// serialized as `images` when non-empty.
        images: Vec<crate::ai::types::ImageContent>,
        /// The real result's `details` payload, verbatim.
        details: Option<JsonValue>,
        /// The real result's `structuredContent` payload, verbatim.
        structured_content: Option<JsonValue>,
        /// The `tool_result` handler-reported usage for this completion,
        /// parsed with the same lenient rules a foreground call applies to
        /// its tool-result message (`parse_usage_json`). A background result
        /// has no `ToolResultMessage` to carry usage, so the completion event
        /// and the persisted `backgroundToolResult` entry details are its
        /// carriers. Serialized as `usage` when present; `None` when the
        /// handler reported none (or an unparseable value).
        usage: Option<crate::ai::types::Usage>,
    },
    /// A transport retry was scheduled (upstream `auto_retry_start`,
    /// agent-session.ts:220/3804): fired by the request's retry wait before
    /// the backoff sleep, with the 1-based retry ordinal and the
    /// post-`Retry-After` delay.
    AutoRetryStart {
        /// 1-based ordinal of the retry about to run (upstream `attempt`).
        attempt: u32,
        /// The configured retry budget (upstream `maxAttempts`).
        max_attempts: u32,
        /// The wait before the retry (upstream `delayMs`).
        delay_ms: u64,
        /// The error that triggered the retry (upstream `errorMessage`).
        error_message: String,
    },
    /// A retried request resolved (upstream `auto_retry_end`,
    /// agent-session.ts:221/1181/1961/3776): `success` with the retry ordinal
    /// that succeeded, or the final failure / `"Retry cancelled"`.
    AutoRetryEnd {
        /// Whether the retried request eventually succeeded.
        success: bool,
        /// The retry ordinal that succeeded / the number of retries
        /// scheduled (upstream `attempt`).
        attempt: u32,
        /// The final error (upstream `finalError`); `None` on success.
        final_error: Option<String>,
    },
    /// A compaction/branch-summary retry was scheduled (upstream
    /// `summarization_retry_scheduled`, agent-session.ts:223-229/3785-3793):
    /// emitted before the backoff of the summarizer call's own retry.
    SummarizationRetryScheduled {
        /// 1-based retry ordinal (upstream `attempt`).
        attempt: u32,
        /// The summarizer retry budget (upstream `maxAttempts`).
        max_attempts: u32,
        /// The backoff about to be slept (upstream `delayMs`).
        delay_ms: u64,
        /// The error that triggered the retry (upstream `errorMessage`).
        error_message: String,
    },
    /// The backoff elapsed and the summarizer call is being re-issued
    /// (upstream `summarization_retry_attempt_start`, agent-session.ts:230-235).
    SummarizationRetryAttemptStart {
        /// Which summarizer owns the retry (upstream `source`).
        source: SummarizationRetrySource,
    },
    /// The summarizer retry loop closed (upstream
    /// `summarization_retry_finished`, agent-session.ts:236/3800-3802).
    SummarizationRetryFinished,
}

/// The summarizer a retry belongs to (upstream
/// `summarization_retry_attempt_start.source` plus `reason` for compaction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarizationRetrySource {
    /// The `/tree` branch summarizer.
    BranchSummary,
    /// A manual/threshold/overflow compaction summary.
    Compaction {
        /// `manual` | `threshold` | `overflow`.
        reason: &'static str,
    },
}

/// Serialize one [`SessionEvent`] into its upstream JSON wire shape (camelCase
/// fields; `undefined` fields stay omitted, exactly like upstream's
/// `JSON.stringify` of the event objects).
pub fn session_event_to_json(event: &SessionEvent) -> JsonValue {
    use serde_json::json;
    match event {
        SessionEvent::QueueUpdate {
            steering,
            follow_up,
            ..
        } => json!({
            "type": "queue_update",
            "steering": steering,
            "followUp": follow_up,
        }),
        SessionEvent::CompactionStart { reason } => json!({
            "type": "compaction_start",
            "reason": reason,
        }),
        SessionEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        } => {
            let mut value = json!({
                "type": "compaction_end",
                "reason": reason,
                "result": result,
                "aborted": aborted,
                "willRetry": will_retry,
            });
            if let Some(message) = error_message {
                value["errorMessage"] = json!(message);
            }
            value
        }
        SessionEvent::AgentSettled { aborted } => json!({
            // Exact upstream `agent_settled` shape (agent-session.ts:203).
            "type": "agent_settled",
            "aborted": aborted,
        }),
        SessionEvent::EntryAppended { entry } => json!({
            // Exact upstream `entry_appended` shape (agent-session.ts:210).
            "type": "entry_appended",
            "entry": entry,
        }),
        SessionEvent::SessionInfoChanged { name } => {
            let mut value = json!({ "type": "session_info_changed" });
            // `name` rides only when present (upstream omits `undefined`).
            if let Some(name) = name {
                value["name"] = json!(name);
            }
            value
        }
        SessionEvent::ThinkingLevelChanged { level } => json!({
            // Exact upstream `thinking_level_changed` shape
            // (agent-session.ts:212).
            "type": "thinking_level_changed",
            "level": level,
        }),
        SessionEvent::BashExecutionUpdate { id, delta } => {
            let mut value = json!({
                "type": "bash_execution_update",
                "delta": delta,
            });
            if let Some(id) = id {
                value["id"] = json!(id);
            }
            value
        }
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
        } => {
            // snake_case `type` mirrors the other out-of-loop events; the
            // payload fields are camelCase like every wire shape here.
            let mut value = json!({
                "type": "background_tool_end",
                "callId": call_id,
                "toolName": tool_name,
                "isError": is_error,
                "output": output,
            });
            if let Some(parent) = parent_call_id {
                value["parentToolCallId"] = json!(parent);
            }
            if let Some(duration_ms) = duration_ms {
                value["durationMs"] = json!(duration_ms);
            }
            if !images.is_empty() {
                // Upstream result-content block shape (`type: "image"`), so a
                // host can append these to a replacement block's content
                // array without reshaping first.
                value["images"] = JsonValue::Array(
                    images
                        .iter()
                        .map(|image| {
                            json!({
                                "type": "image",
                                "data": image.data,
                                "mimeType": image.mime_type,
                            })
                        })
                        .collect(),
                );
            }
            if let Some(details) = details {
                value["details"] = details.clone();
            }
            if let Some(structured) = structured_content {
                value["structuredContent"] = structured.clone();
            }
            if let Some(usage) = usage {
                // The `ToolResultMessage.usage` wire shape (camelCase Usage,
                // cost included), so a host can attribute the completion's
                // reported usage exactly like a foreground result's.
                value["usage"] = json!(usage);
            }
            value
        }
        SessionEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => json!({
            // Exact upstream `auto_retry_start` shape (agent-session.ts:220).
            "type": "auto_retry_start",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        }),
        SessionEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => {
            let mut value = json!({
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
            });
            // `finalError` rides only when present (upstream omits `undefined`,
            // exactly like the `errorMessage` handling on `compaction_end`).
            if let Some(error) = final_error {
                value["finalError"] = json!(error);
            }
            value
        }
        SessionEvent::SummarizationRetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => json!({
            // Exact upstream `summarization_retry_scheduled` shape
            // (agent-session.ts:223-229).
            "type": "summarization_retry_scheduled",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        }),
        SessionEvent::SummarizationRetryAttemptStart { source } => match source {
            // Upstream union: `{source: "branchSummary"}` or
            // `{source: "compaction", reason}` (agent-session.ts:230-235).
            SummarizationRetrySource::BranchSummary => json!({
                "type": "summarization_retry_attempt_start",
                "source": "branchSummary",
            }),
            SummarizationRetrySource::Compaction { reason } => json!({
                "type": "summarization_retry_attempt_start",
                "source": "compaction",
                "reason": reason,
            }),
        },
        SessionEvent::SummarizationRetryFinished => json!({
            // Exact upstream `summarization_retry_finished` shape
            // (agent-session.ts:236).
            "type": "summarization_retry_finished",
        }),
    }
}

/// Build the summarizer-retry observer the compaction module calls
/// (upstream `_summarizationRetryCallbacks`, agent-session.ts:3781-3804).
///
/// The observer maps the compaction module's retry transitions onto
/// [`SessionEvent`] and fans them out through the session's existing event
/// channel, so hosts that never touch the compaction internals (RPC, TUI,
/// embedders) observe the lifecycle with the upstream wire shapes.
pub(crate) fn summarization_retry_observer(
    handlers: &Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
    source: SummarizationRetrySource,
) -> crate::compaction::SummarizationRetryObserver {
    let events = Arc::clone(handlers);
    Arc::new(move |event| {
        let session_event = match event {
            crate::compaction::SummarizationRetryEvent::Scheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => SessionEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            },
            crate::compaction::SummarizationRetryEvent::AttemptStart => {
                SessionEvent::SummarizationRetryAttemptStart { source }
            }
            crate::compaction::SummarizationRetryEvent::Finished => {
                SessionEvent::SummarizationRetryFinished
            }
        };
        emit_session_event_to(&events, session_event);
    })
}

/// Deliver one [`SessionEvent`] to every subscriber of `handlers` (the
/// snapshot is taken under the lock so a handler that subscribes/unsubscribes
/// re-entrantly cannot deadlock the dispatch).
pub(crate) fn emit_session_event_to(
    handlers: &std::sync::RwLock<Vec<SessionEventHandler>>,
    event: SessionEvent,
) {
    let snapshot = handlers
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    // The LAST handler takes the event by value: only the earlier handlers of
    // a multi-handler fan-out pay for their own clone.
    let mut pending = snapshot.into_iter().peekable();
    while let Some(handler) = pending.next() {
        if pending.peek().is_none() {
            handler(event);
            return;
        }
        handler(event.clone());
    }
}

impl AgentSession {
    /// Subscribe to the session-level events the agent-loop [`AgentEvent`]
    /// stream cannot carry (upstream `session.subscribe`,
    /// agent-session.ts:1047/205-221/237): `queue_update`,
    /// `compaction_start`, `compaction_end`, `bash_execution_update`,
    /// `agent_settled`, `entry_appended`, `session_info_changed`,
    /// `thinking_level_changed`, and the transport retry lifecycle
    /// (`auto_retry_start` / `auto_retry_end`).
    /// Handlers run synchronously on the emitting task — keep them cheap and
    /// hand off to a channel/thread for anything heavier (the RPC host does
    /// exactly that).
    pub fn on_session_event(&self, handler: SessionEventHandler) {
        self.session_events
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handler);
    }

    /// Emit one session-level event to every subscriber (internal seam for
    /// the compaction/queue/bash emission sites).
    pub(crate) fn emit_session_event(&self, event: SessionEvent) {
        emit_session_event_to(&self.session_events, event);
    }

    /// Whether any session-level event subscriber is attached. Used as the
    /// payload gate for `entry_appended`: when neither a plugin handler nor a
    /// session subscriber observes the append, the message moves into storage
    /// without being cloned for a wire payload.
    pub(crate) fn has_session_event_subscribers(&self) -> bool {
        !self
            .session_events
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Whether a compaction run is in flight (upstream `isCompacting`,
    /// agent-session.ts:1623). Covers manual, threshold, and overflow runs.
    pub fn is_compacting(&self) -> bool {
        self.compacting.load(Ordering::SeqCst)
    }
}

/// RAII guard for one compaction run's session-event bookkeeping: flips the
/// `isCompacting` flag on construction and resets it on EVERY exit path, and
/// emits the terminal `compaction_end` when the run's success/cancel paths
/// did not emit one themselves (drop ordering guarantees the end event always
/// follows the `compaction_start`).
pub(super) struct CompactionRunGuard {
    events: Option<Arc<std::sync::RwLock<Vec<SessionEventHandler>>>>,
    flag: Option<Arc<AtomicBool>>,
    /// `manual` | `threshold` | `overflow`.
    reason: &'static str,
    /// Set by every explicit exit path that already emitted its own
    /// `compaction_end`; the guard then only resets the flag.
    end_emitted: bool,
}

impl CompactionRunGuard {
    /// Start one compaction run: set `isCompacting` and emit
    /// `compaction_start` (upstream `_emit({type: "compaction_start"})`).
    /// `flag`/`events` are `None` for tests and host-less runs (no
    /// subscribers, nothing to flip).
    pub(super) fn start(
        events: Option<Arc<std::sync::RwLock<Vec<SessionEventHandler>>>>,
        flag: Option<Arc<AtomicBool>>,
        reason: &'static str,
    ) -> Self {
        if let Some(flag) = &flag {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(events) = &events {
            emit_session_event_to(events, SessionEvent::CompactionStart { reason });
        }
        Self {
            events,
            flag,
            reason,
            end_emitted: false,
        }
    }

    /// Record that the run's terminal `compaction_end` has been emitted
    /// (or that none should be emitted on drop).
    pub(super) fn end_emitted(&mut self) {
        self.end_emitted = true;
    }

    /// Clear the `isCompacting` flag immediately (upstream manual compaction
    /// runs `_clearManualCompactionState()` BEFORE emitting
    /// `compaction_end`, so end listeners observe idle state).
    pub(super) fn clear_now(&self) {
        if let Some(flag) = &self.flag {
            flag.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for CompactionRunGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.flag {
            flag.store(false, Ordering::SeqCst);
        }
        if !self.end_emitted {
            if let Some(events) = &self.events {
                emit_session_event_to(
                    events,
                    SessionEvent::CompactionEnd {
                        reason: self.reason,
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(format!("{} compaction did not complete", self.reason)),
                    },
                );
            }
        }
    }
}

/// Registry-backed stream callback that preserves the full stream lifecycle
/// (upstream dispatches the event stream, not just the final message).
///
/// A `prepare_request`-replaced model takes precedence over the baked model;
/// a replaced thinking level (different from the baked `options.reasoning`)
/// replaces the request's reasoning effort.
pub fn registry_event_stream(
    registry: Arc<ProviderRegistry>,
    model: Model,
    options: StreamOptions,
) -> EventStreamFn {
    let base_thinking = options
        .reasoning
        .unwrap_or(crate::ai::types::ThinkingLevel::Off);
    Box::new(move |request: ProviderRequest| {
        let model = request.model.unwrap_or_else(|| model.clone());
        let mut options = options.clone();
        if request.thinking_level != base_thinking {
            options.reasoning = Some(request.thinking_level);
        }
        // The provider trait takes the context by value; the shared request
        // context is materialized once here (zero-copy when this is the last
        // handle), never deep-cloned at request construction.
        let context = crate::agent::agent_loop::context_into_owned(request.context);
        let stream = registry.stream_resolving_auth(model, context, options);
        Box::pin(async move { stream })
    })
}

// ---------------------------------------------------------------------------
// Event serialization (consumed by the RPC host and embedders)
// ---------------------------------------------------------------------------

/// Serialize one [`AgentEvent`] into its JSON wire shape (used by the RPC
/// mode's `event` lines and available to embedders).
///
/// The `type` strings are the upstream session-event contract
/// (agent-session.ts `_handleAgentEvent` / agent-loop.ts emissions):
/// `StreamStart` → `message_start`, `StreamEvent` → `message_update`,
/// `StreamEnd` → `message_end`, with upstream field names (`message`,
/// `assistantMessageEvent`, `toolCallId`, `toolName`, `isError`,
/// `durationMs`, `parentToolCallId`).
pub fn agent_event_to_json(event: &AgentEvent) -> serde_json::Value {
    use serde_json::json;
    match event {
        // Upstream's agent event is exactly `{ type: "agent_start" }`
        // (`packages/agent/src/types.ts:518`), so the wire carries no context:
        // a session's context can be megabytes, and a consumer that needs it
        // reads the message/request events instead. The structured Rust event
        // keeps the field for in-process listeners.
        AgentEvent::AgentStart { context: _ } => json!({ "type": "agent_start" }),
        AgentEvent::AgentEnd { result } => json!({
            "type": "agent_end",
            "turns": result.turns,
            "aborted": result.aborted,
            "terminal": result.terminal,
            // Upstream carries THIS run's produced messages (the new
            // assistant + tool-result messages), not the full transcript —
            // the extension `agent_end` emit (agent loop) uses the same list.
            "messages": result.new_messages,
            // Upstream `_willRetryAfterAgentEnd` (agent-session.ts:1144,
            // 1200-1214): computed by the loop from the SAME retry counter and
            // structural verdict that drives `auto_retry_*`, never hardcoded.
            "willRetry": result.will_retry,
        }),
        AgentEvent::TurnStart { turn, .. } => json!({
            "type": "turn_start",
            "turn": turn,
        }),
        AgentEvent::TurnEnd {
            turn,
            message,
            tool_results,
        } => json!({
            "type": "turn_end",
            "turn": turn,
            "message": message,
            // Upstream `turn_end.toolResults` (agent-loop.ts:493).
            "toolResults": tool_results,
        }),
        // Non-assistant transcript messages (user/tool-result/custom roles).
        // Assistant messages use `StreamStart`/`StreamEnd`, which serialize as
        // the same upstream `message_start`/`message_end` wire events.
        AgentEvent::MessageStart { turn, message } => json!({
            "type": "message_start",
            "turn": turn,
            "message": message,
        }),
        AgentEvent::MessageEnd { turn, message } => json!({
            "type": "message_end",
            "turn": turn,
            "message": message,
        }),
        AgentEvent::StreamStart { turn, message } => json!({
            "type": "message_start",
            "turn": turn,
            "message": message,
        }),
        AgentEvent::StreamEvent { turn, event } => {
            let partial = crate::agent::agent_loop::assistant_event_partial(event)
                .map(|partial| serde_json::to_value(partial).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::json!({ "role": "assistant" }));
            json!({
                "type": "message_update",
                "turn": turn,
                "message": partial,
                // The cumulative snapshot rides once, as `message` above. The
                // event's own `partial` is the same message, so serializing
                // the derived event would materialize the whole reply a SECOND
                // time per delta (`to_value(event)` keeps `partial` unless
                // removed by hand); the borrowed wire view forwards every
                // other field, keeping this object byte-identical to the old
                // `to_value(event)` output minus the duplicate `partial`.
                "assistantMessageEvent":
                    crate::coding_agent::json::AssistantMessageEventWire::new(event),
            })
        }
        AgentEvent::StreamEnd { turn, message } => json!({
            "type": "message_end",
            "turn": turn,
            "message": message,
        }),
        AgentEvent::ToolExecutionStart { turn, event, args } => match event {
            crate::agent::tool_batch::ToolExecutionEvent::Start {
                call_id,
                tool_name,
                parent_call_id,
                ..
            } => {
                let mut value = json!({
                    "type": "tool_execution_start",
                    "turn": turn,
                    "toolCallId": call_id,
                    "toolName": tool_name,
                    "args": args,
                });
                if let Some(parent) = parent_call_id {
                    value["parentToolCallId"] = json!(parent);
                }
                value
            }
            crate::agent::tool_batch::ToolExecutionEvent::End { .. } => {
                json!({ "type": "tool_execution_start", "turn": turn, "args": args })
            }
        },
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
                let mut value = json!({
                    "type": "tool_execution_end",
                    "turn": turn,
                    "toolCallId": call_id,
                    "toolName": tool_name,
                    "result": result,
                    "isError": is_error,
                });
                if let Some(duration) = duration_ms {
                    value["durationMs"] = json!(duration);
                }
                if let Some(parent) = parent_call_id {
                    value["parentToolCallId"] = json!(parent);
                }
                value
            }
            crate::agent::tool_batch::ToolExecutionEvent::Start { .. } => {
                json!({ "type": "tool_execution_end", "turn": turn, "result": result })
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
            // Upstream `tool_execution_update` shape (agent-loop.ts:792):
            // `parentToolCallId` rides only for nested (tool-issued) calls.
            let mut value = json!({
                "type": "tool_execution_update",
                "turn": turn,
                "toolCallId": call_id,
                "toolName": tool_name,
                "args": args,
                "partialResult": partial,
            });
            if let Some(parent) = parent_call_id {
                value["parentToolCallId"] = json!(parent);
            }
            value
        }
        AgentEvent::Compaction { turn } => json!({
            "type": "compaction",
            "turn": turn,
        }),
    }
}
