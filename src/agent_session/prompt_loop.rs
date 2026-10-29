use super::*;

/// Hard ceiling on the in-memory transcript of a store-less (ephemeral)
/// session.
///
/// Ephemeral sessions have no durable transcript, so there is no session
/// storage that can carry a compaction boundary and no file that bounds the
/// process lifetime. Every run appends its produced messages to
/// [`AgentSession::history`]; without a ceiling a long-lived embedded/RPC
/// session grows forever. Keep the newest window under both an estimated
/// token count and a message count. 100k estimated tokens is roughly 400k
/// characters — generous enough that normal short sessions never truncate,
/// while still bounding a process that runs for months.
///
/// Persisted sessions are deliberately untouched: their history is the
/// session projection and is bounded by storage/compaction, never by this
/// window.
const MAX_EPHEMERAL_HISTORY_TOKENS: u64 = 100_000;
const MAX_EPHEMERAL_HISTORY_MESSAGES: usize = 1_024;

/// Trim `messages` in place to the newest window that fits both caps.
///
/// The retained window never starts with an assistant/tool-result fragment:
/// after choosing the oldest retained index, the start advances to the next
/// user message. A lone newest message that by itself exceeds a cap is still
/// kept (a message cannot be split); the vector is therefore bounded in
/// practice by `max(max_messages, one huge message)`.
///
/// Exposed for the `tests/suites/agent_session_units.rs` integration suite;
/// not part of the stable facade.
#[doc(hidden)]
pub fn trim_ephemeral_history(messages: &mut Vec<Message>, max_messages: usize, max_tokens: u64) {
    if messages.is_empty() {
        return;
    }
    let mut keep_from = messages.len();
    let mut kept_tokens = 0u64;
    for (index, message) in messages.iter().enumerate().rev() {
        let next_tokens =
            kept_tokens.saturating_add(crate::compaction::estimate_message_tokens(message));
        let next_count = messages.len() - index;
        if next_count > max_messages || next_tokens > max_tokens {
            break;
        }
        kept_tokens = next_tokens;
        keep_from = index;
    }
    if keep_from == messages.len() {
        // The newest message alone exceeds a cap; keep it rather than
        // dropping the entire transcript.
        keep_from = messages.len() - 1;
    }
    if let Some(user_offset) = messages[keep_from..]
        .iter()
        .position(|message| matches!(message, Message::User(_)))
    {
        keep_from += user_offset;
    }
    if keep_from > 0 {
        messages.drain(..keep_from);
        // A pre-fix session may arrive with a huge capacity; release it once
        // instead of retaining the old peak for the rest of the process.
        if messages.capacity() > messages.len().saturating_mul(2).max(64) {
            messages.shrink_to_fit();
        }
    }
}

/// Locate the trailing failed attempt of a run: the last assistant message
/// followed only by tool results that answer one of its tool calls. Returns
/// that assistant's index plus the indices of its derived results, in order.
///
/// This is the message-side half of upstream `_omitRecoveryAttempt`
/// (agent-session.ts:1244-1271), which scans forward from the failed
/// assistant until the next assistant and pairs results by `toolCallId`.
/// Here the scan runs to the END of the list because the assistant must be the
/// LAST one: a length stop with tool calls appends its synthetic error
/// results after the assistant, so `messages.last()` alone missed exactly the
/// #11227 shape. Any other trailing message (a steering prompt injected after
/// the turn, a non-matching result) means the attempt is no longer the run
/// tail and `None` is returned.
///
/// Tool-call ids are compared as exact strings, including the empty id a
/// malformed model response may carry: an empty id matches only an empty id
/// (never a wildcard), and the ordinal-keyed persistence ledger decides which
/// entry each match targets, so duplicate/empty ids cannot misfire.
fn trailing_attempt_ordinals(messages: &[Message]) -> Option<Vec<usize>> {
    let assistant_index = messages
        .iter()
        .rposition(|message| matches!(message, Message::Assistant(_)))?;
    let Message::Assistant(assistant) = &messages[assistant_index] else {
        return None;
    };
    let tool_call_ids: std::collections::BTreeSet<&str> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect();
    let mut ordinals = vec![assistant_index];
    for (index, message) in messages.iter().enumerate().skip(assistant_index + 1) {
        match message {
            Message::ToolResult(result) if tool_call_ids.contains(result.tool_call_id.as_str()) => {
                ordinals.push(index);
            }
            _ => return None,
        }
    }
    Some(ordinals)
}

/// Persist the recovery omission (upstream `_omitRecoveryAttempt`): append a
/// `replacement: null` context edit for every target entry, so the failed
/// assistant and the tool results derived from its calls leave the
/// model-visible projection TOGETHER. The raw transcript keeps the message
/// entries (the durable pair); a resumed session replays the edits, so no
/// provider request ever sees an unpaired `toolResult` (#11227).
///
/// Every target must resolve to a persisted entry before the first edit is
/// written: omitting one side only would leave an assistant whose calls have
/// no results (or a result with no call), which providers reject. The ledger
/// is keyed by produced-message ordinal, so duplicate and empty `toolCallId`s
/// cannot make two targets resolve to the same entry.
async fn omit_recovery_attempt_entries(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    ledger: &PersistLedger,
    ordinals: &[usize],
) {
    if ordinals.is_empty() {
        return;
    }
    let mut entry_ids = Vec::with_capacity(ordinals.len());
    for ordinal in ordinals {
        match ledger.entry_id(*ordinal) {
            Some(entry_id) => entry_ids.push(entry_id),
            None => {
                crate::ext_diag!(
                    "[session] cannot omit the recovery attempt: message {ordinal} has no persisted entry"
                );
                return;
            }
        }
    }
    let mut session = session.lock().await;
    for entry_id in entry_ids {
        if let Err(error) = session.append_context_edit(&entry_id, None) {
            crate::ext_diag!("[session] failed to omit recovery entry {entry_id}: {error}");
            return;
        }
    }
}

/// What starts a run (upstream `BeforeAgentStartTrigger`): a submitted prompt
/// (or `pi.sendUserMessage()`), or an idle custom message sent with
/// `pi.sendMessage(..., { triggerTurn: true })`.
#[derive(Debug, Clone)]
pub(super) enum RunTrigger {
    /// A user prompt, already through the extension `input` event and command
    /// expansion (upstream `prompt()`). `images` carries the phone/RPC image
    /// attachments into the same transform/user-message path (empty for a
    /// text-only prompt).
    User {
        text: String,
        images: Vec<ContentBlock>,
    },
    /// The custom message that starts the run; it becomes the run's starting
    /// message and `before_agent_start` carries it instead of a `prompt`.
    Custom(CustomRunMessage),
}

/// Reserved `CustomRunMessage` type used to carry an idle
/// `pi.sendUserMessage(...)` through the existing host pump for
/// `take_pending_custom_run` (the TUI starts a run for every entry; SDK/RPC
/// hosts that drain the queue get the same behavior). Upstream starts the run
/// immediately inside `prompt()`; pir's notification drain cannot block the
/// host, so the run starts when the host next pumps pending runs — the same
/// contract `pi.sendMessage(..., {triggerTurn: true})` already has.
///
/// The message's `details` carry `{text, images}`; `run_custom_turn` converts
/// it back into a user run, so image blocks survive instead of being flattened
/// into text.
pub(super) const PENDING_USER_RUN_TYPE: &str = "__pir_user_message";

/// Build the reserved pending-run record for an idle user message.
pub(super) fn pending_user_run_message(
    text: String,
    images: Vec<ContentBlock>,
) -> CustomRunMessage {
    let details = serde_json::json!({
        "text": text,
        "images": images,
    });
    CustomRunMessage {
        custom_type: PENDING_USER_RUN_TYPE.to_owned(),
        content: JsonValue::Array(Vec::new()),
        display: false,
        details: Some(details),
    }
}

/// Decode a [`PENDING_USER_RUN_TYPE`] record; `None` for an ordinary custom
/// message or a malformed payload.
fn pending_user_run_parts(message: &CustomRunMessage) -> Option<(String, Vec<ContentBlock>)> {
    if message.custom_type != PENDING_USER_RUN_TYPE {
        return None;
    }
    let details = message.details.as_ref()?;
    let text = details
        .get("text")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned();
    let images = details
        .get("images")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                // Text blocks cannot occur here (the text rides separately);
                // keep the filter so a hand-written details payload cannot
                // inject a second, empty text block.
                .filter(|item| item.get("type").and_then(JsonValue::as_str) == Some("image"))
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    Some((text, images))
}

/// Builds the prompt-start projection in ONE blocking task: widen a paged
/// session, then build the projection and branch snapshot.
///
/// The starting messages are NOT appended here: the loop owns their
/// lifecycle (extension `message_start`/`message_end` with the handler
/// replacement applied) and persists them at their `message_end` through
/// [`AgentSession::preamble_persist_fn`]. The projection therefore covers the
/// prior transcript only; the loop appends the starting messages on top.
///
/// The widen + projection used to run on the async task while it held the
/// session lock; both are blocking work over a transcript-sized file, so the
/// blocking pool now runs the identical sequence under one `blocking_lock`,
/// preserving the lock order (widen → projection → branch) and error
/// propagation.
pub(super) async fn project_session_off_thread(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
) -> crate::Result<(
    crate::session_manager::SessionProjection,
    Vec<crate::compaction::PathEntryMeta>,
)> {
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(
        move || -> crate::Result<(
            crate::session_manager::SessionProjection,
            Vec<crate::compaction::PathEntryMeta>,
        )> {
            let mut session = session.blocking_lock();
            // A session opened PAGED (`SessionManager::open_paged`) holds a
            // tail window only. The AgentSession facade runs the resident
            // compaction pipeline, so it widens to the complete transcript
            // here — never a silently truncated context. (The print host takes
            // the paged fast path instead: `compaction::compact_paged`.) The
            // widening's measured cost and why it is the upstream-shaped
            // fallback rather than a refusal: `SDK-SCOPE.md` §8.
            if session.is_paged() {
                session.widen_to_full()?;
            }
            let projection = session.build_session_projection()?;
            let branch = session.branch_path_meta(None);
            Ok((projection, branch))
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("session startup blocking task failed: {error}"))?
}

/// Build only the session projection's message list on the blocking pool
/// (widen included).
///
/// Continuations and the overflow retry rebuild need the whole projected
/// conversation; building it is transcript-sized blocking work, so it runs on
/// the blocking pool under `blocking_lock` (D6) instead of parking a runtime
/// worker on the async session lock. The widen is the same no-op/full parse as
/// [`project_session_off_thread`]'s first step.
async fn project_session_messages_off_thread(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
) -> crate::Result<Vec<Message>> {
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(move || -> crate::Result<Vec<Message>> {
        let mut session = session.blocking_lock();
        if session.is_paged() {
            session.widen_to_full()?;
        }
        Ok(session.build_session_projection()?.messages)
    })
    .await
    .map_err(|error| anyhow::anyhow!("session projection blocking task failed: {error}"))?
}

/// Widen a PAGED session to the full transcript on the blocking pool.
///
/// The run needs the whole transcript before it pushes the extension session
/// snapshot and builds its projection. The widen is a full file parse, so it
/// must not run on a runtime worker while holding the async session lock
/// (D6): the blocking task takes the lock with `blocking_lock` and widens
/// there, exactly like [`project_session_off_thread`]'s widen step.
pub(super) async fn widen_session_off_thread(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
) -> crate::Result<()> {
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(move || -> crate::Result<()> {
        let mut session = session.blocking_lock();
        if session.is_paged() {
            session.widen_to_full()?;
        }
        Ok(())
    })
    .await
    .map_err(|error| anyhow::anyhow!("session widen blocking task failed: {error}"))?
}

/// Whether the projection contains an assistant message with usage the
/// prompt-start threshold can inspect — the `_findLastAssistantMessage` gate
/// upstream applies before the prompt-start compaction check.
///
/// Upstream scans backwards for the LAST assistant message; a trailing user
/// message (for example a resumed transcript that ends on a question) does not
/// hide the assistant's usage. Walk the projection METADATA (no message
/// materialization): entries already follow the LEAF path, apply compaction
/// retention, and apply `context_edit` edits, so an omitted entry
/// (`message_count == 0`) is skipped exactly like the projection skips it.
/// Only an assistant `message` entry qualifies (custom messages and summaries
/// project as user text).
fn projection_has_assistant_message(entries: &[compaction::ProjectionEntryMeta]) -> bool {
    use crate::session_manager::{FileEntry, StoredMessage};
    entries.iter().any(|entry| {
        entry.message_count > 0
            && matches!(
                entry.source_entry.as_ref(),
                FileEntry::Message(inner) if matches!(inner.message, StoredMessage::Assistant(_))
            )
    })
}

/// Storage shape of one starting message the loop persists at its
/// `message_end` ([`AgentSession::preamble_persist_fn`]): a regular message
/// entry, or the custom-message entry a run started by a custom message (and
/// each pending `deliverAs: "nextTurn"` message) keeps in storage.
#[derive(Clone)]
struct PreambleAppend {
    /// `Some` for a custom-message entry; `None` for a regular message entry.
    custom: Option<CustomRunMessage>,
    /// Wire shape for `entry_appended` (a custom entry keeps its
    /// `role: "custom"` shape; a regular entry uses the finalized message).
    wire: JsonValue,
}

impl AgentSession {
    // ------------------------------------------------------------------
    // Prompt
    // ------------------------------------------------------------------

    /// Run one user prompt through the agent loop.
    ///
    /// Returns the terminal assistant message (including error stop reasons —
    /// inspect `stop_reason` / `error_message` on the message).  Events are
    /// forwarded to `on_event` as the loop runs.
    pub async fn prompt(
        &self,
        text: &str,
        on_event: EventHandler,
    ) -> crate::Result<AssistantMessage> {
        self.start_run(
            RunTrigger::User {
                text: text.to_owned(),
                images: Vec::new(),
            },
            on_event,
        )
        .await
    }

    /// Run one user prompt with image attachments (the phone host's prompt
    /// path): the images ride the same `input` transform and become image
    /// content blocks on the starting user message.
    pub async fn prompt_with_images(
        &self,
        text: &str,
        images: Vec<ContentBlock>,
        on_event: EventHandler,
    ) -> crate::Result<AssistantMessage> {
        self.start_run(
            RunTrigger::User {
                text: text.to_owned(),
                images,
            },
            on_event,
        )
        .await
    }

    /// Run one custom message through the SAME setup as a prompt: compaction
    /// check, `before_agent_start` (with `messageType: "custom"`), tool
    /// loadout, pending `nextTurn` messages, handler messages and the system
    /// prompt patch (upstream `sendCustomMessage(... { triggerTurn: true })`
    /// → `_startRun({ messageType: "custom", message })`).
    ///
    /// This is what makes a run started by a custom message keep the sections
    /// `before_agent_start` handlers added for EVERY request of the run
    /// instead of patching them away after the first tool call.
    pub async fn run_custom_turn(
        &self,
        message: CustomRunMessage,
        on_event: EventHandler,
    ) -> crate::Result<AssistantMessage> {
        // An idle `pi.sendUserMessage(...)` rides this pump as a reserved
        // record (see [`PENDING_USER_RUN_TYPE`]); it is a USER run, not a
        // custom-message run: the images and text belong on the starting user
        // message and `before_agent_start` must carry the `"user"` variant.
        if let Some((text, images)) = pending_user_run_parts(&message) {
            return self
                .start_run(RunTrigger::User { text, images }, on_event)
                .await;
        }
        self.start_run(RunTrigger::Custom(message), on_event).await
    }

    /// Start a run (upstream `_startRun`): the session counts as busy from
    /// here on — during the compaction check and the `before_agent_start`
    /// handlers — so messages sent meanwhile queue into THIS run instead of
    /// starting a second one, `ctx.isIdle()` is false, and an abort requested
    /// meanwhile stops the run once it starts.
    async fn start_run(
        &self,
        trigger: RunTrigger,
        on_event: EventHandler,
    ) -> crate::Result<AssistantMessage> {
        // Native session-level fan-out: install the `SessionEvent` forwarder
        // before this run can emit `queue_update` (steering enqueues),
        // `compaction_start`/`_end`, or `auto_retry_*` — a builtin-only session
        // must deliver them to its Rust plugins exactly once, just like the
        // loop events (`LoopDeps::plugin_events`). Idempotent per session.
        if let Some(plugins) = &self.plugins {
            super::notifications::install_native_session_event_bridge(
                &self.session_events,
                plugins,
            );
        }
        // Claim the session for this run (the single atomic transition every
        // run start shares, mutual exclusion with a manual compaction's own
        // claim). A run during a run keeps the existing rejection; a run
        // during a manual compaction reports the upstream prompt-while-
        // compacting message.
        //
        // A fresh run clears the previous abort request BEFORE the claim
        // (upstream `_startRun` resets `_agentRunAbortRequested`): from the
        // CAS on, the session is RUNNING and `abort()` records the request,
        // so a store performed after the CAS would erase an abort that landed
        // in exactly that window and the run would ignore it. If another
        // owner wins the claim, the old value is restored instead — a second
        // prompt must never swallow the live run's (or compaction's) abort
        // request.
        let previous_abort_request = self.run_abort_requested.swap(false, Ordering::SeqCst);
        if let Err(owner) = self.session_state.compare_exchange(
            SESSION_IDLE,
            SESSION_RUNNING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            if previous_abort_request {
                self.run_abort_requested.store(true, Ordering::SeqCst);
            }
            if owner == SESSION_COMPACTING {
                anyhow::bail!(
                    "Cannot submit a prompt while compaction is in progress. \
                     Wait for compaction to finish and retry."
                );
            }
            anyhow::bail!("agent session is already streaming; queue with steer() instead");
        }
        if std::env::var_os("PIR_DEBUG_RPC").is_some() {
            eprintln!("[rpc-debug] start_run claimed");
        }
        // Install a fresh transition signal as soon as the claim is ours. The
        // previous run's signal stays in the slot until now; dropping it here
        // means an abort that landed before the claim (a no-op between runs)
        // can never look like an abort of THIS run, while an abort that lands
        // from here on either hits this placeholder or is recorded by
        // `abort()`'s post-abort state read. `run_prompt` captures the
        // placeholder and re-checks it after installing the run's own signal.
        {
            let mut slot = self
                .signal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = AbortSignal::new();
        }
        self.run_settling.store(false, Ordering::SeqCst);
        // A new run re-opens the background follow-up enqueue gate (closed by
        // the previous run's settle transition, see `BackgroundRegistry`).
        self.background.open_follow_up_queue();
        // Only a STORE-BACKED run drains queued completion frames at settle
        // (the durable `backgroundToolResult` entry is then the authoritative
        // copy). A store-less session keeps its frames queued because the
        // frame is its only delivery surface; closing the gate there would
        // drop late completions entirely.
        let background_queue_gated = self.session.is_some();
        // Push the live streaming flag so `ctx.waitForIdle()` (upstream
        // ExtensionCommandContext.waitForIdle) observes the in-flight run.
        let js = self
            .plugins
            .as_ref()
            .and_then(|plugins| plugins.js_handle())
            .cloned();
        if let Some(js) = &js {
            js.set_session_streaming(true);
        }
        // Panic/abort-safe reset: the guard releases the run's claim — and
        // clears the abort/settling flags and the extension idle cell — on
        // EVERY exit path. `run_prompt` panicking or the caller's task being
        // aborted (`prompt_task.abort()` — which unwinds nothing but drops
        // this future mid-await) would otherwise leave the session
        // permanently claimed: every later prompt rejected as "already
        // streaming", every `compact()` waiter asleep forever, and
        // extensions' `ctx.waitForIdle()` sleeping to its deadline.
        let guard = SessionStreamingGuard {
            state: Arc::clone(&self.session_state),
            run_abort_requested: Arc::clone(&self.run_abort_requested),
            run_settling: Arc::clone(&self.run_settling),
            background: Arc::clone(&self.background),
            background_queue_gated,
            idle_notify: Arc::clone(&self.idle_notify),
            js,
        };
        // Run diagnostics (`crate::diagnostics`): the session always collects
        // (counter-only work, no per-delta allocation) so hosts can render
        // `last_run_diagnostics()` without an env gate. The wrapper observes
        // every loop event on the way to the host's handler and forwards it
        // unchanged; retried attempts (overflow recovery) accumulate into the
        // same run's turn list.
        let diagnostics = Arc::new(crate::diagnostics::RunDiagnosticsCollector::new(true));
        let on_event = {
            let diagnostics = Arc::clone(&diagnostics);
            let handler = on_event;
            let wrapped: EventHandler = Arc::new(move |event: AgentEvent| {
                diagnostics.record(&event);
                handler(event);
            });
            wrapped
        };
        // Phone host run boundary: ownership flips (a run the phone did not
        // start hands control back to the CLI) and the streaming status is
        // published before the loop starts; the full snapshot follows at the
        // settle boundary below.
        self.note_phone_run_started();
        self.publish_phone_status().await;
        let result = self.run_prompt(trigger, on_event, &guard).await;
        if let Some(summary) = diagnostics.finish() {
            self.set_last_run_diagnostics(summary);
        }
        drop(guard);
        if let Err(error) = &result {
            // Surface the run failure so hosts (and extensions) learn the run
            // died instead of silently observing a stalled streaming flag. The
            // prompt error is ALSO returned to the caller (the TUI renders it),
            // so this line is the durable log record: error level, never
            // collapsed, routed off a TUI frame.
            crate::ext_diag_fatal!("[session] prompt run failed: {error}");
            // Upstream `_runAgentPrompt`'s finally emits `agent_settled` on
            // EVERY exit, including a thrown run; without this the failed-run
            // path (an error out of `run_prompt`, before its own settle
            // emission) leaves settlement observers such as git-checkpoint /
            // pi-pretty waiting for a settle that never arrives. `aborted`
            // mirrors the loop's own flag (the run's abort signal).
            let aborted = self
                .signal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_aborted();
            // Session-level event: unlike the plugin dispatch below, this
            // reaches `AgentSession::on_session_event` subscribers
            // unconditionally.
            self.emit_session_event(SessionEvent::AgentSettled { aborted });
            if let Some(plugins) = &self.plugins {
                emit_plugin_event(
                    plugins,
                    "run_failed",
                    serde_json::json!({
                        "type": "run_failed",
                        "error": error.to_string(),
                    }),
                )
                .await;
                emit_plugin_event(
                    plugins,
                    "agent_settled",
                    serde_json::json!({
                        "type": "agent_settled",
                        "aborted": aborted,
                    }),
                )
                .await;
            }
        }
        // Phone host settle boundary: publish a real snapshot/status/catalog
        // (and the saved-session list) whether the run succeeded or failed.
        self.publish_phone_state().await;
        result
    }

    /// The persistence sink for ONE starting message the loop appends
    /// (upstream persists a custom/user initial message at its `message_end`,
    /// agent-session.ts:1147-1167): a regular message becomes a message entry,
    /// a run started by a custom message (and each pending `deliverAs:
    /// "nextTurn"` message) keeps its custom-message entry shape. The
    /// finalized message — after any extension `message_end` replacement — is
    /// what a regular entry stores.
    ///
    /// The `entry_appended` dispatch runs AFTER the append and outside the
    /// storage lock (upstream `_emit({type: "entry_appended", entry})`), and
    /// the first append failure is recorded in `failure` so the prompt run can
    /// surface it after the loop — the pre-loop startup append used to
    /// propagate it directly.
    ///
    /// `None` for a store-less (ephemeral) session: the loop's `LoopResult::
    /// preamble` list is what the caller appends to the in-memory transcript.
    fn preamble_persist_fn(
        &self,
        append: PreambleAppend,
        failure: Arc<std::sync::Mutex<Option<anyhow::Error>>>,
    ) -> Option<agent_loop::PreamblePersistFn> {
        let session = self.session.clone()?;
        let plugins = self.plugins.clone();
        let session_events = Arc::clone(&self.session_events);
        let session_event_subscribers = self.has_session_event_subscribers();
        Some(Arc::new(move |message: Message| {
            let session = Arc::clone(&session);
            let plugins = plugins.clone();
            let session_events = Arc::clone(&session_events);
            let failure = Arc::clone(&failure);
            let append = append.clone();
            Box::pin(async move {
                let appended = match &append.custom {
                    Some(custom) => {
                        let custom = custom.clone();
                        let session = Arc::clone(&session);
                        let joined =
                            tokio::task::spawn_blocking(move || -> crate::Result<String> {
                                let mut session = session.blocking_lock();
                                session.append_custom_message_entry(
                                    &custom.custom_type,
                                    custom.content,
                                    custom.display,
                                    custom.details,
                                )
                            })
                            .await;
                        match joined {
                            Ok(appended) => appended,
                            Err(error) => Err(anyhow::anyhow!(
                                "session startup blocking task failed: {error}"
                            )),
                        }
                    }
                    None => {
                        SessionManager::append_message_off_thread(&session, message.clone()).await
                    }
                };
                match appended {
                    Ok(entry_id) => {
                        // One wire payload per entry: the plugin dispatch and
                        // the session-level event share the same upstream
                        // `SessionEntry` shape (agent-session.ts:834).
                        let message_wire = match &append.custom {
                            Some(_) => append.wire.clone(),
                            None => message_json(&message),
                        };
                        let entry_json = serde_json::json!({
                            "id": entry_id,
                            "message": message_wire,
                        });
                        if let Some(plugins) = &plugins {
                            if plugins.has_handlers("entry_appended") {
                                emit_plugin_event(
                                    plugins,
                                    "entry_appended",
                                    serde_json::json!({
                                        "type": "entry_appended",
                                        "entry": entry_json.clone(),
                                    }),
                                )
                                .await;
                            }
                        }
                        if session_event_subscribers {
                            crate::agent_session::events::emit_session_event_to(
                                &session_events,
                                SessionEvent::EntryAppended { entry: entry_json },
                            );
                        }
                        Ok(())
                    }
                    Err(error) => {
                        let placeholder =
                            anyhow::anyhow!("starting-message persistence failed: {error}");
                        let mut slot = failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        Err(placeholder)
                    }
                }
            })
        }))
    }

    async fn run_prompt(
        &self,
        trigger: RunTrigger,
        on_event: EventHandler,
        guard: &SessionStreamingGuard,
    ) -> crate::Result<AssistantMessage> {
        let signal = AbortSignal::new();
        // Capture the transition placeholder `start_run` installed right
        // after claiming the session, then install this run's signal. The
        // placeholder is re-checked AFTER the install: an abort that landed
        // between the claim and this point aborted the placeholder, not the
        // new signal, and would otherwise be lost. (The run-abort flag is
        // re-checked too: `abort()` records it with a post-abort state read,
        // so a request overlapping the claim is observed here.)
        let previous_signal = {
            let mut slot = self
                .signal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *slot, signal.clone())
        };
        // An abort that raced the run's start (it landed after the run
        // claimed the session but before this fresh signal existed) must stop
        // THIS run instead of being lost on the previous run's signal —
        // upstream's `agent_start` handler aborts the agent when an abort was
        // requested while the run was still starting.
        if self.run_abort_requested.swap(false, Ordering::SeqCst) || previous_signal.is_aborted() {
            signal.abort();
        }
        // A new prompt invalidates any pending idle-mode cache warm task: the
        // conversation prefix is about to change. Abort the live warm's
        // signal directly (await-able) instead of bumping a polled counter.
        if let Some(warm_signal) = self
            .warm_abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            warm_signal.abort();
        }

        // Extension provider sync: apply `pi.registerProvider` definitions
        // (registered at factory time or during earlier events) to the real
        // provider registry so their models become selectable. Rust builtin
        // providers ride the same unified registry seam. Virtual models
        // (`pi.registerVirtualModel`) ride the overlay alongside them.
        if let Some(plugins) = &self.plugins {
            self.sync_plugin_providers(plugins).await;
            self.sync_plugin_virtual_models(plugins).await;
        }
        let model = self.current_model();

        // A session opened PAGED (`open_paged`) holds only a tail window. The
        // extension snapshot pushed below (`ctx.sessionManager` /
        // getEntries/getTree/getBranch) and every read this run performs must
        // see the whole transcript, so widen BEFORE the snapshot is pushed.
        // The widen is a full file parse: it runs on the blocking pool (the
        // async session lock is not held across it), not on a runtime worker.
        if let Some(session) = &self.session {
            widen_session_off_thread(session).await?;
        }

        // Back the ctx facade with the live session/model state before any
        // event handler runs this prompt (session_start handlers read
        // `ctx.model` too).
        self.push_session_context(&model).await;

        // Extension lifecycle: `session_start` fires exactly once, before the
        // first prompt reaches the model, and a fresh run always starts with a
        // cleared extension abort state.
        if let Some(plugins) = &self.plugins {
            if !self.session_start_sent.swap(true, Ordering::SeqCst) {
                // Whether a `session_start` handler exists decides whether the
                // post-dispatch sync below is needed at all: the sync at the top
                // of this run already applied every registration made before it,
                // and only a `session_start` handler can register something
                // between the two syncs (the `input` event is dispatched AFTER
                // this block). A handler-less session therefore syncs ONCE; the
                // second sync used to re-run every `refreshModels` refresh on
                // the first prompt (duplicate network calls / model-store
                // writes).
                let had_session_start_handlers = plugins.has_handlers("session_start");
                // Skip the fan-out when nobody declared the event: `emit`
                // hands every event to every native plugin regardless of its
                // declared `EventInterest`, and a plugin that only observes
                // `agent_settled` must not be woken (or, if its handler
                // blocks, park the run) on `session_start`.
                if had_session_start_handlers {
                    let mut payload = serde_json::json!({
                        "type": "session_start",
                        "reason": "startup",
                    });
                    if let Err(error) = plugins.emit("session_start", &mut payload).await {
                        crate::ext_diag!("[ext] session_start dispatch failed: {error}");
                    }
                    // Handlers may have registered providers in session_start;
                    // apply them (and the virtual models) before the next
                    // request. Without a handler, there is nothing new to sync.
                    self.sync_plugin_providers(plugins).await;
                    self.sync_plugin_virtual_models(plugins).await;
                }
                // Actionable notifications (setModel, setThinkingLevel,
                // setSessionName, ...) are applied rather than discarded.
                self.emit_resources_discover(plugins, "startup").await;
                self.apply_extension_notifications().await;
            }
            plugins.clear_abort().await;
        }

        // A user prompt goes through the extension `input` event (which may
        // rewrite the text and images) and through `/skill:`/`/template`
        // expansion (upstream `prompt()` does both before `_startRun`). A run
        // started by a custom message has neither: its starting message is the
        // custom message itself (`_startRun({messageType: "custom"})`).
        let (text, input_images) = match &trigger {
            RunTrigger::User { text, images } => {
                // The `input` event may rewrite the prompt text (and images)
                // before they are stored and sent to the model (upstream
                // `emitInput` parity: `images` + `streamingBehavior` ride the
                // payload).
                let input = match &self.plugins {
                    Some(plugins) => {
                        transform_prompt_input_host(plugins, text, images.clone(), None).await
                    }
                    None => PromptInputTransform {
                        text: text.clone(),
                        images: images.clone(),
                    },
                };
                // Expand `/skill:name args` and `/template args` (upstream
                // `prompt()` expands both after the extension `input` event).
                (self.expand_prompt_commands(&input.text), input.images)
            }
            RunTrigger::Custom(_) => (String::new(), Vec::new()),
        };

        // Upstream `_prepareRun` performs the threshold check BEFORE
        // `before_agent_start` and before the starting message is appended
        // (agent-session.ts:1926-1935). The loop's per-turn compaction hook
        // still owns the steady-state timing; this first, session-level check
        // aligns the FIRST prompt of a run: a resumed transcript that already
        // exceeds the threshold compacts before the extension start handlers
        // observe it and before the user message enters the context. The
        // check needs an assistant usage record anywhere on the live leaf
        // path (upstream `_findLastAssistantMessage`); a trailing user message
        // does not hide it.
        //
        // The gate is read off the projection METADATA (leaf path + context
        // edits, no message materialization). Only a transcript that may
        // compact builds the projection here, and that projection is REUSED as
        // the run context below when the hook did not actually compact — the
        // prompt no longer builds the whole transcript twice.
        let mut startup_projection: Option<(
            crate::session_manager::SessionProjection,
            Vec<crate::compaction::PathEntryMeta>,
        )> = None;
        // The prompt-start gate runs the threshold check once before the
        // starting message is persisted. If it actually starts a compaction,
        // the loop's first per-turn check is skipped: a failed append must not
        // be retried in the same run (the contract is "fail once, continue on
        // the unchanged context, retry on the next turn"). Merely calling the
        // hook below the threshold arms nothing.
        let threshold_gate = Arc::new(super::compact::ThresholdRunGate::default());
        if self.compaction_config().is_some() {
            match &self.session {
                Some(session) => {
                    let tail_is_assistant = {
                        let session = session.lock().await;
                        // Projection metadata, not the raw file tail: the
                        // leaf path and context-edit omissions decide whether
                        // an assistant usage record is visible at all.
                        projection_has_assistant_message(&session.projection_entry_meta())
                    };
                    if tail_is_assistant {
                        if let Ok((mut projection, branch)) =
                            project_session_off_thread(session).await
                        {
                            // Defensive re-check against the MATERIALIZED
                            // projection: a context edit can omit every
                            // assistant entry. A trailing user message does
                            // not hide the assistant usage the gate looks at
                            // (upstream `_findLastAssistantMessage`).
                            if projection
                                .messages
                                .iter()
                                .any(|message| matches!(message, Message::Assistant(_)))
                            {
                                let pre_messages = std::mem::take(&mut projection.messages);
                                // Reuse the loop's own threshold hook (same
                                // config read, threshold basis,
                                // `session_before_compact` interception,
                                // persistence, and session events).
                                let hook = self.compaction_hook(
                                    Arc::clone(&self.compaction),
                                    None,
                                    Some(self.threshold_model_for(&model)),
                                    Some(Arc::clone(&threshold_gate)),
                                );
                                let pre_context = Arc::new(Context {
                                    system_prompt: self.system_prompt_snapshot(),
                                    messages: pre_messages,
                                    tools: None,
                                });
                                let returned = hook(Arc::clone(&pre_context)).await;
                                if threshold_gate
                                    .attempted
                                    .load(std::sync::atomic::Ordering::SeqCst)
                                {
                                    threshold_gate
                                        .skip_next
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                }
                                // The hook returns the SAME context on every
                                // non-compaction path; a compaction returns a
                                // rebuilt one (and the prompt-start projection
                                // is then stale).
                                if Arc::ptr_eq(&pre_context, &returned) {
                                    drop(returned);
                                    if let Some(context) = Arc::into_inner(pre_context) {
                                        projection.messages = context.messages;
                                        startup_projection = Some((projection, branch));
                                    }
                                }
                            } else {
                                // Not a compaction candidate after all, but
                                // the projection is exactly the run context.
                                startup_projection = Some((projection, branch));
                            }
                        }
                    }
                }
                None => {
                    let pre_messages = history_owned(self.history_snapshot());
                    if pre_messages
                        .iter()
                        .any(|message| matches!(message, Message::Assistant(_)))
                    {
                        let hook = self.compaction_hook(
                            Arc::clone(&self.compaction),
                            None,
                            Some(self.threshold_model_for(&model)),
                            Some(Arc::clone(&threshold_gate)),
                        );
                        let pre_context = Arc::new(Context {
                            system_prompt: self.system_prompt_snapshot(),
                            messages: pre_messages,
                            tools: None,
                        });
                        let _ = hook(pre_context).await;
                        if threshold_gate
                            .attempted
                            .load(std::sync::atomic::Ordering::SeqCst)
                        {
                            threshold_gate
                                .skip_next
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }
            }
        }

        // `before_agent_start`: handlers may override the system prompt for
        // this run and inject a custom message ahead of the turn. The event is
        // a union on `messageType` (upstream `BeforeAgentStartEvent`): a user
        // run carries `prompt`/`images`, a custom run carries the starting
        // `message` and no `prompt`.
        let mut run_system_prompt = self.system_prompt_snapshot();
        // A `before_agent_start` handler may replace the prompt for this run;
        // that decision is fixed, so the per-iteration refresh below must not
        // overwrite it (D6).
        let mut run_system_prompt_overridden = false;
        // The normalized `BuildSystemPromptOptions` the live prompt was
        // derived from (upstream `_baseSystemPromptOptions`, agent-session.ts:
        // 1735-1750). Handlers may mutate it in place; the renderer below is
        // the same ordered section builder the session itself uses, so
        // `sections`/`selectedTools`/`toolSnippets`/`skills` edits take effect
        // exactly like upstream's `systemPromptOptions`.
        // The snapshot pushes itself into the runtime (cached by a fingerprint),
        // backing `ctx.getSystemPromptOptions()` and the event below.
        let prompt_options = self.system_prompt_options_snapshot();
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("before_agent_start") {
                let mut payload = match &trigger {
                    RunTrigger::User { .. } => serde_json::json!({
                        "type": "before_agent_start",
                        "messageType": "user",
                        "prompt": text,
                        "systemPrompt": self.effective_system_prompt(),
                        "systemPromptOptions": prompt_options.clone(),
                    }),
                    RunTrigger::Custom(message) => serde_json::json!({
                        "type": "before_agent_start",
                        "messageType": "custom",
                        "message": message.to_json(),
                        "systemPrompt": self.effective_system_prompt(),
                        "systemPromptOptions": prompt_options.clone(),
                    }),
                };
                // `images` is optional upstream: an absent key, never null.
                if !input_images.is_empty() {
                    payload["images"] =
                        serde_json::to_value(&input_images).unwrap_or(JsonValue::Array(Vec::new()));
                }
                match plugins.emit("before_agent_start", &mut payload).await {
                    Ok(outcome) => {
                        // A timed-out/stuck handler is isolated per handler and
                        // must not be silent: the run proceeds, but the warning
                        // names the event and the (sanitized) error.
                        for error in &outcome.errors {
                            crate::ext_diag!(
                                "[ext] before_agent_start handler error: {}",
                                error.error
                            );
                        }
                        if let Some(prompt) = outcome.result.get("systemPrompt") {
                            if let Some(prompt) = prompt.as_str() {
                                run_system_prompt = Some(prompt.to_owned());
                                run_system_prompt_overridden = true;
                            }
                        }
                        // The mutable options object is shared across handlers
                        // and rides the payload home (in-place edits and full
                        // replacements alike). When it changed, this run
                        // renders its prompt from the final object with the
                        // ordered section builder (upstream
                        // `_preparePromptAndToolLoadout(result.
                        // systemPromptOptions)`, agent-session.ts:1935-1942);
                        // the forced text a handler returned is already
                        // folded into `forceSystemPrompt`, so the render
                        // honors it.
                        if let Some(options) = payload.get("systemPromptOptions") {
                            if options != &prompt_options {
                                let rendered = crate::extensions::system_prompt_options::
                                    SystemPromptOptions::from_json(options)
                                        .render(self.package_dir.as_deref());
                                run_system_prompt = Some(rendered);
                                run_system_prompt_overridden = true;
                            }
                        }
                        // Handlers may inject custom messages ahead of the
                        // turn. Upstream accumulates a `messages` ARRAY across
                        // handlers (runner.ts:1435,1453) while a single
                        // `message` is the legacy shape: consume both.
                        let mut injected: Vec<String> = Vec::new();
                        if let Some(messages) =
                            outcome.result.get("messages").and_then(JsonValue::as_array)
                        {
                            for message in messages {
                                if let Some(text) = extract_custom_message_text(message) {
                                    injected.push(text);
                                }
                            }
                        }
                        if injected.is_empty() {
                            if let Some(message) = extract_custom_message_text(&outcome.result) {
                                injected.push(message);
                            }
                        }
                        for message in injected {
                            self.enqueue_steering(agent_loop::PromptMessage::text(message));
                        }
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] before_agent_start dispatch failed: {error}")
                    }
                }
            }
            // The session counts as busy while a run starts (upstream
            // `_startRun` sets the flag before `_prepareRun`), so a
            // `pi.sendMessage()` made by a `before_agent_start` handler goes
            // into the STARTING run instead of starting a second one. Drain the
            // notifications the handlers just queued so they join this run's
            // first request (`consume_send_message` steers while busy).
            self.apply_extension_notifications().await;
        }

        let mut stream_options = self.stream_options(Some(signal.clone()));
        if let Some(plugins) = &self.plugins {
            // `before_provider_headers`: handlers mutate `headers` in place;
            // the mutated map flows back through the payload and is applied
            // to the outgoing request. A `null` value deletes the header.
            if plugins.has_handlers("before_provider_headers") {
                let mut payload = serde_json::json!({
                    "type": "before_provider_headers",
                    "headers": stream_options.headers.clone().unwrap_or_default(),
                });
                match plugins.emit("before_provider_headers", &mut payload).await {
                    Ok(_) => {
                        if let Some(headers) = payload.get("headers").and_then(|h| {
                            serde_json::from_value::<
                                std::collections::BTreeMap<String, Option<String>>,
                            >(h.clone())
                            .ok()
                        }) {
                            let mut merged = std::collections::BTreeMap::new();
                            for (name, value) in headers {
                                if let Some(value) = value {
                                    merged.insert(name, value);
                                }
                            }
                            stream_options.headers = Some(merged);
                        }
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] before_provider_headers dispatch failed: {error}");
                    }
                }
            }
        }

        // The steering/follow-up queues are polled below; the per-request
        // transport hooks (`before_provider_request` / `after_provider_response`)
        // ride `StreamOptions` — see `stream_options()`.

        // The run polls the session's live steering/follow-up queues instead
        // of taking ownership of them: a failed prompt can never disable
        // steering/follow-up delivery, and `clear_queue` can drop queued
        // messages while a run is in flight. Each drain applies the mode
        // current at poll time (upstream `PendingMessageQueue.mode`), so a
        // mode change takes effect at the next turn of a running session.
        let steering = self.steering_inbox.clone();
        let follow_up = self.follow_up_inbox.clone();

        // The run's starting message (upstream `_prepareRun`): the user prompt,
        // or the custom message that started the run. Pending
        // `deliverAs: "nextTurn"` custom messages ride alongside it.
        let pending_next_turn = self.take_pending_next_turn_messages();
        let starting = match &trigger {
            RunTrigger::User { .. } => user_message(&text, input_images),
            // A custom message projects into the model context as a user text
            // message (upstream keeps role "custom" and `convertToLlm` renders
            // it as user text; pir renders it directly).
            RunTrigger::Custom(message) => custom_run_user_message(message),
        };
        // The wire shape of the starting message: the custom message carries
        // its own shape (`customType`/`content`/`display`/`details`) for the
        // message events, like the idle `sendMessage` append path.
        let starting_json = match &trigger {
            RunTrigger::User { .. } => message_json(&starting),
            RunTrigger::Custom(message) => message.to_json(),
        };
        // The starting messages are handed to the loop as preamble entries
        // that the loop APPENDS itself: it emits their lifecycle
        // (`agent_start` → `turn_start` → `message_start`/`message_end`), and
        // only the finalized message — after any extension `message_end`
        // replacement — enters the context and session storage (upstream
        // `_handleAgentEvent` persists `message_end` after the extension
        // dispatch). The startup append therefore no longer happens before the
        // run: the projection below covers the prior transcript and the loop
        // appends the starting messages on top.
        let preamble_failure: Arc<std::sync::Mutex<Option<anyhow::Error>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut preamble: Vec<agent_loop::PreambleMessage> =
            Vec::with_capacity(1 + pending_next_turn.len());
        let starting_append = PreambleAppend {
            custom: match &trigger {
                RunTrigger::User { .. } => None,
                RunTrigger::Custom(message) => Some(message.clone()),
            },
            wire: starting_json.clone(),
        };
        preamble.push(agent_loop::PreambleMessage {
            message: starting.clone(),
            wire: match &trigger {
                RunTrigger::User { .. } => None,
                RunTrigger::Custom(_) => Some(starting_json.clone()),
            },
            append: true,
            persist: self.preamble_persist_fn(starting_append, Arc::clone(&preamble_failure)),
        });
        for message in &pending_next_turn {
            let projection = custom_run_user_message(message);
            let wire = message.to_json();
            preamble.push(agent_loop::PreambleMessage {
                message: projection,
                wire: Some(wire.clone()),
                append: true,
                persist: self.preamble_persist_fn(
                    PreambleAppend {
                        custom: Some(message.clone()),
                        wire,
                    },
                    Arc::clone(&preamble_failure),
                ),
            });
        }
        // The loop appends/persists the starting messages at their
        // `message_end`; the context here is the prior transcript only.
        //
        // D4 (stale reuse): `before_agent_start` handlers can append session
        // entries AFTER the gate's projection was built — `setSessionName`,
        // `setModel`, `pi.appendEntry`, or `pi.sendMessage({triggerTurn:
        // false})`, which persists a model-visible custom message. Reusing
        // that projection would drop such a message from this run's first
        // request (the loop's context is the projection plus its own starting
        // messages), so reuse it only while the session still ends on the
        // leaf the projection ended on; otherwise rebuild from the live
        // session. The projection's last entry is the projected path leaf.
        if let Some((projection, _)) = startup_projection.as_ref() {
            if let Some(session) = &self.session {
                let live_leaf = session.lock().await.leaf_id().map(str::to_owned);
                let projected_leaf = projection
                    .entries
                    .last()
                    .and_then(|entry| entry.source_entry.id())
                    .map(str::to_owned);
                if live_leaf != projected_leaf {
                    startup_projection = None;
                }
            }
        }
        let mut projected: Option<ProjectionSnapshot> = None;
        let mut messages = match &self.session {
            Some(session) => match startup_projection.take() {
                // The prompt-start projection was already built for the
                // compaction gate and the gate did NOT compact: reuse it
                // (messages + branch metadata) instead of rebuilding the
                // transcript.
                Some((projection, branch)) => {
                    let metas =
                        compaction::ProjectionEntryMeta::from_projection(&projection.entries);
                    projected = Some((Arc::new(metas), Arc::new(branch)));
                    projection.messages
                }
                None => {
                    let (projection, branch) = project_session_off_thread(session).await?;
                    // Snapshot = per-entry METADATA only (shared source entries +
                    // message counts); the messages are moved into the loop's
                    // context below and the hook slices THAT list, so a
                    // transcript-sized session is materialized exactly once (the
                    // projection aggregate) instead of once per entry plus once
                    // for the aggregate plus once for the branch.
                    let metas =
                        compaction::ProjectionEntryMeta::from_projection(&projection.entries);
                    projected = Some((Arc::new(metas), Arc::new(branch)));
                    projection.messages
                }
            },
            None => {
                let mut history = self
                    .history
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let slot = Arc::make_mut(&mut *history);
                // A store-less session holds appended `triggerTurn: false`
                // custom messages in memory (upstream `_appendCustomMessage`
                // writes them into the session manager); they precede the
                // starting message so they ride the first request, matching
                // the order a persisted session projects them in.
                for message in self.take_pending_setup_messages() {
                    slot.push(custom_run_user_message(&message));
                }
                trim_ephemeral_history(
                    slot,
                    MAX_EPHEMERAL_HISTORY_MESSAGES,
                    MAX_EPHEMERAL_HISTORY_TOKENS,
                );
                slot.clone()
            }
        };

        // The agent loop runs until it settles. An `agent_before_settle`
        // handler returning `continue: true` guarantees exactly one more
        // provider request (upstream boundary semantics), so the loop is
        // re-entered with the refreshed session context (bounded).
        // Run-local record of the virtual registration a request routed under
        // (upstream `_installAgentRequestProjection`). P10: it does NOT stamp
        // the produced messages — those keep the PHYSICAL identity their stream
        // named (upstream `agent-session.ts:614-637`) — it only drives the
        // branch's `model_change` selection recorded at settle.
        let routed_registration: Arc<
            std::sync::Mutex<Option<crate::ai::virtual_::VirtualModelRegistration>>,
        > = Arc::new(std::sync::Mutex::new(None));
        // Per-message persistence LEDGER: the sink claims one ordinal per
        // produced message (upstream persists each `message_end` during the
        // run, agent-session.ts:1144-1163) and the post-run batch asks the
        // ledger per ordinal whether that message is already handled. Per-
        // ordinal identity (not a count) makes the split exact when the sink
        // intentionally omits a message (overflow recovery) or a write fails:
        // a failed write installs a barrier and the batch re-appends the
        // unhandled tail in order. See [`PersistLedger`].
        let persist_ledger = PersistLedger::new();
        let mut first_iteration = true;
        let mut settle_iterations = 0usize;
        // Post-run context-overflow recovery state (upstream
        // `_overflowRecoveryAttempted`, one compact-and-retry attempt per
        // prompt): `recovery_context` carries the compacted retry context.
        let mut overflow_recovery_attempted = false;
        let mut recovery_context: Option<Context> = None;
        // Session-backed overflow retry (upstream `_runAutoCompaction
        // ("overflow")`): one durable compact-and-retry per prompt — the
        // transcript persists WITHOUT the failed attempt, the recovery
        // compaction is appended to session storage as a compaction entry,
        // and the retry context is rebuilt from the session projection.
        let mut overflow_session_retry = false;
        let result = 'settle: loop {
            settle_iterations += 1;
            let messages = if let Some(recovered) = recovery_context.take() {
                // Overflow recovery retry: the compacted context replaces the
                // projection (the failed attempt was omitted from storage).
                recovered.messages
            } else if first_iteration {
                first_iteration = false;
                std::mem::take(&mut messages)
            } else {
                match &self.session {
                    // D6: the projection is transcript-sized blocking work;
                    // build it on the blocking pool, not on a runtime worker
                    // while holding the async session lock. A projection error
                    // keeps the previous tolerant empty-context fallback.
                    Some(session) => project_session_messages_off_thread(session)
                        .await
                        .unwrap_or_default(),
                    None => history_owned(self.history_snapshot()),
                }
            };
            // Re-merge plugin tools and build the model-facing loadout before
            // each settle iteration: `session_start` (and runtime command
            // dispatch) can register tools after construction, and the loadout
            // decides which tools are declared vs callable-only (exposure
            // model).
            //
            // The MCP discovery activation runs first (upstream
            // `ensureDiscoveryActive` in the mcp extension's `session_start`
            // handler): the servers' configured exposures may ask for
            // `codemode`/`tool_search` to be active, and the write must land
            // before this iteration's loadout is built. It is idempotent, so
            // running it per iteration also covers servers registered
            // mid-session.
            self.ensure_discovery_active();
            let loadout = self.build_tool_loadout(self.plugins.as_deref()).await;
            if !run_system_prompt_overridden {
                // D6: the loadout build may have rebuilt the context-derived
                // prompt because the active set changed (tool_search, MCP
                // discovery, `setActiveTools`). Reading it per settle
                // iteration keeps the request's system prompt in step with
                // the declarations the SAME iteration sends; upstream
                // re-runs `_rebuildSystemPrompt` on the change and applies it
                // on the next agent turn. A `before_agent_start` rewrite is
                // this run's fixed choice and is left alone.
                run_system_prompt = self.system_prompt_snapshot();
            }
            // SHARED: the registry is handed to the loop (which clones the Arc
            // once per nested executor) and the nested-callable view; the
            // former `Arc::clone` here duplicated the registry handle, not its
            // contents, so this is only the naming.
            let tools = Arc::clone(&loadout.registry);
            let current_model = self.current_model();

            // Session environment for tools that spawn their own agent runs (the
            // `subagent` builtin): the dispatch defaults (model/thinking) mirror
            // upstream `ctx.model`/`ctx.thinkingLevel`. The UI/trust posture
            // mirrors the headless hosts' no-UI reality: the project-agent
            // confirm gate takes its safe path only when a host declares a UI.
            let session_env =
                Arc::new(crate::agent::tool_execution::ToolSessionEnv {
                    provider_registry: Arc::clone(&self.registry),
                    model: current_model.clone(),
                    thinking: self.current_thinking(),
                    session_id: self.session_id(),
                    session_file: self.session_path(),
                    base_tools: Arc::clone(&self.tools),
                    has_ui: false,
                    project_trusted: true,
                    // Subagent discovery follows THIS session's agent
                    // directory (`<agent-dir>/agents`), so an SDK agent built
                    // with an explicit `.agent_dir(..)` never falls back to
                    // the process `~/.pi/agent/agents`.
                    user_agents_dir: Some(self.agent_dir().join("agents")),
                    // Child sessions are built against the SAME directory
                    // (models.json/auth.json/settings), never the process
                    // environment a later `PIR_AGENT_DIR` change may point
                    // elsewhere.
                    agent_dir: Some(self.agent_dir().to_path_buf()),
                    credential: self.explicit_tool_credential(),
                    on_update: self.tool_update_channel(),
                    live_tools: Some(Arc::clone(&tools)),
                    active_override: Some(Arc::clone(&self.active_tools_override)),
                    // The model-facing background controls act on THIS session's
                    // registry (a subagent's child session has its own, built by
                    // `AgentSession::new`), never a parent's.
                    background: Some(Arc::new(background::SessionBackgroundControl::new(
                        Arc::clone(&self.background),
                    ))
                        as Arc<dyn crate::agent::tool_execution::BackgroundControl>),
                });

            let deps = LoopDeps {
                tools,
                scheduler: self.scheduler.clone(),
                // Host-installed per-tool lifecycle hooks (upstream
                // `AgentLoopConfig.beforeToolCall`/`afterToolCall`). The
                // scheduler already owns the extension/plugin `tool_call` and
                // `tool_result` faces (H2 timing: after validation / before
                // `tool_execution_end`); these hooks run AROUND execution
                // inside the same scheduler pass, so neither face is
                // re-dispatched.
                hooks: self.tool_hooks(),
                follow_up: Some(follow_up_fn(
                    follow_up.clone(),
                    Arc::clone(&self.pending_messages),
                    Arc::clone(&self.follow_up_mode),
                    Arc::clone(&self.queue_mirror),
                    Arc::clone(&self.session_events),
                )),
                background_follow_up: Some({
                    let inbox = self.background_follow_up_inbox.clone();
                    Box::new(move || {
                        let inbox = inbox.clone();
                        Box::pin(
                            async move { inbox.drain(crate::agent_session::QueueMode::All).await },
                        )
                    })
                }),
                cwd: self.cwd.clone(),
                max_turns: self.max_turns,
                // Agent-level turn retry (`settings.retry`, upstream
                // `_prepareRetry`): resolved from the same merged settings the
                // session's other knobs use, and read per prompt so a settings
                // edit applies to the next run. The SDK's transport-level
                // retry override (`set_retry`) is a different budget.
                // `set_auto_retry_enabled(false)` gates BOTH budgets, exactly
                // like upstream's shared `autoRetryEnabled` setting.
                retry: self
                    .auto_retry_enabled()
                    .then(|| self.session_settings().agent_retry_policy())
                    .flatten(),
                // Agent-level retry lifecycle (upstream
                // `_prepareRetry` → `auto_retry_start` / `_finishCancelledRetry`
                // / post-run end, agent-session.ts:220-222,3781-3862). The
                // transport's own retry observers are installed separately on
                // `StreamOptions`; this sink only ever sees the loop's
                // re-issued turns.
                retry_events: Some({
                    let events = Arc::clone(&self.session_events);
                    Arc::new(move |event: agent_loop::AgentRetryEvent| {
                        let session_event = match event {
                            agent_loop::AgentRetryEvent::Start {
                                attempt,
                                max_attempts,
                                delay_ms,
                                error_message,
                            } => SessionEvent::AutoRetryStart {
                                attempt,
                                max_attempts,
                                delay_ms,
                                error_message,
                            },
                            agent_loop::AgentRetryEvent::End {
                                success,
                                attempt,
                                final_error,
                            } => SessionEvent::AutoRetryEnd {
                                success,
                                attempt,
                                final_error,
                            },
                        };
                        super::events::emit_session_event_to(&events, session_event);
                    })
                }),
                extensions: self.extensions.clone(),
                // The unified host's NATIVE plugin fan-out (Rust builtins +
                // inline plugins): `extensions` above is the JS face, and the
                // host is passed separately so a builtin-only plan (plan-only
                // handle, no JS runtime) still delivers agent events to
                // native plugins exactly once (the JS adapter inside the host
                // is skipped by `emit_native`).
                plugin_events: self.plugins.clone(),
                // MCP reach readiness (upstream the mcp extension's
                // `tool_call` handler): each batch waits for the servers its
                // calls may reach before they execute.
                tool_call_wait: self.tool_call_wait_hook(),
                // Assistant `message_end` repair (the `tool-repair` builtin,
                // scope doc §2): the finalized message passes through the
                // native repair pipeline before the terminal assignment,
                // context push, persistence, and tool-call extraction. `None`
                // without the builtin (zero loop change).
                message_end_repair: self.tool_repair_hook(),
                model: Some(current_model.clone()),
                thinking_level: self.current_thinking(),
                finish_turn: self.turn_end_boundary_hook(
                    self.finish_turn
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                ),
                prepare_request: self
                    .virtual_routing_prepare_request(Arc::clone(&routed_registration)),
                nested_callable: Some(Arc::clone(&loadout.nested_callable)),
                session_env: Some(session_env),
                // The session's OWN commit tracker: placeholder gates (A1) and
                // the tool-batch barrier (B1) are runtime-scoped, so a
                // concurrent session can never open this one's gates.
                commit_tracker: Some(self.background.commit_tracker()),
                ..LoopDeps::default()
            };
            let event_stream: EventStreamFn = match &self.event_stream {
                Some(shared) => {
                    let shared = Arc::clone(shared);
                    Box::new(move |request: ProviderRequest| shared(request))
                }
                None => registry_event_stream(
                    Arc::clone(&self.registry),
                    current_model.clone(),
                    // The run bakes one `StreamOptions` snapshot; the stream
                    // closure and the loop's request share it (a clone of the
                    // small options struct, not of the registry or the
                    // transcript).
                    stream_options.clone(),
                ),
            };
            let loop_options = LoopOptions {
                events: Some(on_event.clone()),
                steering: Some(steering_fn(
                    steering.clone(),
                    Arc::clone(&self.pending_messages),
                    Arc::clone(&self.steering_mode),
                    Arc::clone(&self.queue_mirror),
                    Arc::clone(&self.session_events),
                )),
                // Per-message persistence (upstream agent-session.ts:1144-1163):
                // every message the loop appends is written to session storage
                // at its `message_end`, so a mid-run crash loses nothing. The
                // failed overflow/length attempt is persisted here too; the
                // post-run recovery omits it and its derived tool results
                // together (see `omit_recovery_attempt_entries` below).
                //
                // P10: the sink stamps the registration it is handed onto every
                // persisted assistant, so it gets a cell that is never filled.
                // A routed run must keep the PHYSICAL identity its stream named
                // on the stored response (upstream `agent-session.ts:614-637`):
                // the next turn replays the transcript against that same
                // physical model, and a virtual id here made
                // `transform_messages`' `is_same_model` report a different
                // model — dropping every signed thinking block (Anthropic
                // thinking replay, OpenAI-Responses encrypted reasoning) on
                // precisely the routed runs. The virtual SELECTION is recorded
                // separately, as a branch `model_change` after the run.
                persist_message: self.persist_message_sink(
                    Arc::new(std::sync::Mutex::new(None)),
                    Arc::clone(&persist_ledger),
                ),
                // A virtual selection checks the compaction threshold against
                // the model the request is ROUTED to (upstream checks
                // `route.model` in prepareRequest). The hook reads the live
                // config, so `set_auto_compaction` applies immediately.
                compaction: Some(self.compaction_hook(
                    Arc::clone(&self.compaction),
                    projected.take(),
                    Some(self.threshold_model_for(&current_model)),
                    Some(Arc::clone(&threshold_gate)),
                )),
                event_stream: Some(event_stream),
                // Generic loop hooks (upstream `transformContext` /
                // `prepareNextTurn`): the session's own per-turn work
                // (compaction, virtual-model routing, prompt/loadout refresh)
                // rides the dedicated seams above/below, so the generic hooks
                // stay unset for session runs. An embedder that drives
                // `agent_loop` directly can install them.
                transform_context: None,
                prepare_next_turn: None,
                // First iteration only: the starting prompt/next-turn
                // messages the loop appends and persists itself (see the
                // binding above).
                preamble: std::mem::take(&mut preamble),
                preamble_messages: Vec::new(),
            };

            // Each settle iteration persists its own messages: the ledger is
            // per-iteration (the post-run batch below only persists the
            // remainder of THIS iteration's produced messages), and resetting
            // it makes a partial settle resumable — the next iteration starts
            // from a clean prefix instead of trusting a stale count.
            persist_ledger.reset();
            if std::env::var_os("PIR_DEBUG_RPC").is_some() {
                eprintln!("[rpc-debug] entering agent loop");
            }
            let mut result = agent_loop::run_with_options(
                deps,
                Context {
                    system_prompt: run_system_prompt.clone(),
                    messages,
                    // The model-facing loadout: active direct/model-only tools
                    // after loadout-hook rewrites, minus hidden declarations.
                    // `codemode`/`deferred` tools stay callable (registry) but
                    // are NOT declared here (upstream exposure semantics).
                    // MOVED: the loadout is per settle iteration and nothing
                    // reads it after this point, so the former `clone` (one
                    // deep copy of every tool definition per iteration, on the
                    // hot path) is now a move.
                    tools: Some(loadout.definitions),
                },
                Some(signal.clone()),
                loop_options,
            )
            .await;

            // A starting-message append failed inside the loop (the pre-loop
            // startup append used to propagate this directly): surface it as
            // the prompt failure, like the run-start storage error it is.
            if let Some(error) = preamble_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                return Err(error);
            }

            // An abort requested while the run was still starting (during the
            // compaction check or the `before_agent_start` handlers) stops it
            // before its first request — upstream `_handleAgentEvent` aborts
            // the agent on `agent_start` when an abort was requested meanwhile.
            // The run still settles with an aborted assistant message so the
            // transcript records the turn the user cancelled.
            if result.aborted && result.turns == 0 && result.new_messages.is_empty() {
                let aborted = AssistantMessage {
                    api: current_model.api.clone(),
                    provider: current_model.provider.clone(),
                    model: current_model.id.clone(),
                    stop_reason: StopReason::Aborted,
                    timestamp: crate::ai::util::now_ms(),
                    ..AssistantMessage::default()
                };
                result.terminal = aborted.clone();
                result.new_messages.push(Message::Assistant(aborted));
            }

            // Post-run context-overflow recovery (upstream
            // agent-session.ts:3003-3044): a context-overflow error terminal
            // or a recoverable length stop omits the failed attempt, compacts
            // once, and retries a single time per prompt. Ephemeral runs
            // recover on the in-memory context; session-backed runs persist
            // the WHOLE transcript (the failed attempt and its derived tool
            // results included) and then append context-edit omissions for the
            // pair before the durable recovery compaction, so the raw
            // transcript stays an internally consistent pair while the retry —
            // and every later prompt — rebuilds from the compacted projection
            // instead of re-overflowing on the full history.
            if !overflow_recovery_attempted && self.compaction_config().is_some() {
                // The trigger reads the THRESHOLD model (the routed model's
                // window under a virtual selection, matching the compaction
                // hook): the raw virtual entry's window 0 would fire the
                // durable compact-and-retry on every prompt.
                let threshold_model = self.threshold_model_for(&current_model);
                let terminal_overflow = crate::ai::util::is_context_overflow(
                    &result.terminal,
                    Some(threshold_model.context_window),
                );
                let recoverable_length = crate::ai::util::is_recoverable_length(
                    &result.terminal,
                    threshold_model.max_tokens,
                );
                if terminal_overflow || recoverable_length {
                    overflow_recovery_attempted = true;
                    // The failed attempt is "pending" when the transcript
                    // tail is the terminal assistant alone, or the terminal
                    // assistant followed only by the tool results derived from
                    // its tool calls. A length stop with tool calls appends
                    // its synthetic error results BEFORE the run ends (the
                    // finish-turn decision can end it right there), so
                    // checking only `messages.last()` missed exactly the
                    // length+tool-calls shape and left the persisted results
                    // without their assistant (#11227).
                    let failed_attempt_pending =
                        trailing_attempt_ordinals(&result.context.messages).is_some();
                    let last_is_assistant =
                        matches!(result.context.messages.last(), Some(Message::Assistant(_)));
                    if failed_attempt_pending && self.session.is_some() {
                        // Session-backed: fall through so the transcript
                        // below persists the whole run (including the failed
                        // attempt and its results), then the durable omission
                        // and overflow compaction run together.
                        overflow_session_retry = true;
                    } else if last_is_assistant {
                        // Ephemeral (no session): in-memory compact-and-retry.
                        // Overflow run bookkeeping (upstream
                        // `_runAutoCompaction("overflow")` session events).
                        let mut run = CompactionRunGuard::start(
                            Some(Arc::clone(&self.session_events)),
                            Some(Arc::clone(&self.compacting)),
                            "overflow",
                        );
                        let config = self.compaction_config().unwrap_or_default();
                        // Recovery basis: the failed trailing attempt is
                        // dropped before the cut (upstream
                        // `_omitRecoveryAttempt` + `prepareCompaction`).
                        let mut recovery_source = (*result.context).clone();
                        recovery_source.messages.pop();
                        let focus = format!(
                            "Recover from a context-overflow error: {}",
                            result
                                .terminal
                                .error_message
                                .as_deref()
                                .unwrap_or("context overflow")
                        );
                        let refs: Vec<compaction::CutEntryRef<'_>> = recovery_source
                            .messages
                            .iter()
                            .map(compaction::CutEntryRef::from_message)
                            .collect();
                        let prepared = compaction::prepare_compaction_refs(
                            &refs,
                            &[] as &[compaction::PathEntryMeta],
                            &config,
                        );
                        if let Some(prepared) = prepared {
                            // `session_before_compact` (overflow): a handler
                            // may cancel or replace the recovery compaction,
                            // exactly like the manual/threshold paths.
                            let decision = super::compact::dispatch_before_compact(
                                self.plugins.as_ref(),
                                "overflow",
                                true,
                                Some(focus.as_str()),
                                &prepared,
                                &[] as &[FileEntry],
                                signal.is_aborted(),
                            )
                            .await;
                            let mut from_extension = false;
                            let compacted = match decision {
                                super::compact::BeforeCompactDecision::Cancel => {
                                    // Upstream: a handler cancel is an ABORT,
                                    // not a failure (no errorMessage);
                                    // end-then-failed.
                                    self.emit_session_event(SessionEvent::CompactionEnd {
                                        reason: "overflow",
                                        result: None,
                                        aborted: true,
                                        will_retry: false,
                                        error_message: None,
                                    });
                                    self.emit_compact_failed("overflow", None, true, false)
                                        .await;
                                    run.end_emitted();
                                    None
                                }
                                super::compact::BeforeCompactDecision::Replace(replacement) => {
                                    from_extension = true;
                                    Some(Ok(*replacement))
                                }
                                super::compact::BeforeCompactDecision::Proceed => {
                                    let retry_observer = self.summarization_retry_observer(
                                        super::events::SummarizationRetrySource::Compaction {
                                            reason: "overflow",
                                        },
                                    );
                                    Some(
                                        compaction::with_summarization_retry(
                                            retry_observer,
                                            compaction::compact_with_preparation_refs(
                                                prepared,
                                                &current_model,
                                                self.complete_fn(),
                                                Some(focus.as_str()),
                                            ),
                                        )
                                        .await,
                                    )
                                }
                            };
                            if let Some(compacted) = compacted {
                                match compacted {
                                    Ok(compacted) => {
                                        let rebuilt = compaction::rebuilt_context(
                                            &recovery_source,
                                            &compacted,
                                        );
                                        let estimated_tokens_after =
                                            compaction::estimate_context_tokens(&rebuilt.messages)
                                                .tokens;
                                        // The retry decision rides
                                        // `willRetry` (upstream 3205); the wire
                                        // result now exists for the in-memory
                                        // recovery too.
                                        self.emit_session_event(SessionEvent::CompactionEnd {
                                            reason: "overflow",
                                            result: Some(super::compact::compaction_result_wire(
                                                &compacted,
                                                estimated_tokens_after,
                                            )),
                                            aborted: false,
                                            will_retry: true,
                                            error_message: None,
                                        });
                                        run.end_emitted();
                                        // The retry's preamble was consumed by
                                        // the failed attempt; make the rebuilt
                                        // (compacted) context the new
                                        // ephemeral history so the user prompt
                                        // and summary are not lost.
                                        *self
                                            .history
                                            .write()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                            Arc::new(rebuilt.messages.clone());
                                        recovery_context = Some(rebuilt);
                                        continue;
                                    }
                                    Err(error) => {
                                        // End first, failed second (upstream
                                        // catch order, agent-session.ts:
                                        // 3276-3295).
                                        let aborted = signal.is_aborted();
                                        let error_message = if aborted {
                                            None
                                        } else {
                                            Some(format!(
                                                "Context overflow recovery failed: {error}"
                                            ))
                                        };
                                        self.emit_session_event(SessionEvent::CompactionEnd {
                                            reason: "overflow",
                                            result: None,
                                            aborted,
                                            will_retry: false,
                                            error_message: error_message.clone(),
                                        });
                                        self.emit_compact_failed(
                                            "overflow",
                                            error_message,
                                            aborted,
                                            from_extension,
                                        )
                                        .await;
                                        run.end_emitted();
                                    }
                                }
                            }
                        } else {
                            // Upstream returns before `started` when there is
                            // nothing to summarize: no end/failed pair is owed.
                            run.end_emitted();
                        }
                    }
                }
            }

            // P10: a routed run keeps the PHYSICAL identity on its produced
            // messages — the provider/model its stream named — because that is
            // what upstream persists (`agent-session.ts:614-637`: responses
            // name physical models; the virtual SELECTION is recorded
            // separately as a `model_change` entry, appended below). The
            // former virtual-identity stamp made every replayed assistant look
            // like it came from another model, so `transform_messages`'
            // `is_same_model` dropped its signed thinking on the very next
            // turn — losing Anthropic thinking-cache replays and
            // OpenAI-Responses encrypted reasoning on exactly the routed runs.
            let routed = routed_registration
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(registration) = &routed {
                // Persist the registration on the branch (upstream persists
                // virtual-model router state in prepareRequest), so resume
                // keeps the selection routable.
                self.persist_virtual_model_state(registration).await;
            }
            // Persist EVERY message the run produced — intermediate assistant
            // (toolCall) messages included, in append order. The per-message
            // sink above already handled the leading PREFIX of them during the
            // run (upstream agent-session.ts:1144-1163), so this batch only
            // covers the remainder (normally none).
            // INVARIANT (moved, not cloned): the produced messages MOVE out of
            // the run result. Every later consumer takes owned messages
            // (session append, ephemeral history extend, the durable-transcript
            // retry), and `result.new_messages` has no remaining reader — the
            // settle boundary reads `result.context`/`result.terminal` (narrowed
            // signatures below) and the run result itself is not returned to
            // the caller.
            let produced = std::mem::take(&mut result.new_messages);
            // The failed overflow/length attempt and every tool result derived
            // from its tool calls, as ORDINALS into `produced` (the same
            // ordinals the persistence ledger keys). They stay in the raw
            // transcript (persisting the assistant and its results together is
            // what keeps the durable pair consistent); the durable omission
            // below appends `replacement: null` context edits for all of them
            // so neither side is model-visible. Upstream `_omitRecoveryAttempt`
            // pairs them by tool-call id the same way (#11227).
            let omission_ordinals = if overflow_session_retry {
                let target = trailing_attempt_ordinals(&produced);
                if target.is_none() {
                    // The produced list does not end with the failed attempt
                    // (should not happen: the recovery gate checked the same
                    // tail on the context). Recover without an omission rather
                    // than editing an unrelated entry; the persisted pair is
                    // still internally consistent.
                    crate::ext_diag!(
                        "[session] overflow recovery found no trailing attempt to omit"
                    );
                }
                target
            } else {
                None
            };
            let omitted_ordinals: std::collections::BTreeSet<usize> =
                omission_ordinals.iter().flatten().copied().collect();
            // Every PERSISTED assistant message contributes its usage (the
            // omitted failed attempt — an error/length terminal — is not part
            // of the model-visible transcript).
            let produced_tokens: u64 = produced
                .iter()
                .enumerate()
                .filter(|(ordinal, _)| !omitted_ordinals.contains(ordinal))
                .filter_map(|(_, message)| match message {
                    Message::Assistant(assistant) => Some(assistant.usage.total_tokens),
                    _ => None,
                })
                .sum();
            match &self.session {
                Some(session) => {
                    // The dispatch is skipped when NEITHER a plugin handler nor
                    // a session-event subscriber observes it: each message then
                    // moves into storage (no clone) and no wire payload is
                    // built.
                    let dispatch_entry_appended_to_plugins = self
                        .plugins
                        .as_ref()
                        .is_some_and(|plugins| plugins.has_handlers("entry_appended"));
                    let dispatch_entry_appended =
                        dispatch_entry_appended_to_plugins || self.has_session_event_subscribers();
                    // Append everything under the storage lock, then dispatch
                    // the `entry_appended` events after the guard is dropped
                    // (a slow JS handler must not hold the storage lock).
                    //
                    // The JSONL write is blocking file I/O, so the batch runs
                    // on the blocking pool: the storage lock is acquired there
                    // (`blocking_lock`) and the async task never holds it
                    // across the write. One blocking task appends the whole
                    // tail, preserving the previous single-lock batch order.
                    let appended: Vec<(String, Message)> = {
                        let mut to_append = Vec::new();
                        // Skip exactly the messages the sink already handled
                        // (per-ordinal identity: a failed append leaves its
                        // ordinal — and the whole tail — for this loop, in
                        // produced order). The dispatch copy is taken only
                        // when an `entry_appended` consumer exists, exactly
                        // like the previous in-lock loop.
                        let mut dispatch_messages = Vec::new();
                        let mut pending_ordinals = Vec::new();
                        for (ordinal, message) in produced.into_iter().enumerate() {
                            if persist_ledger.is_handled(ordinal) {
                                continue;
                            }
                            if dispatch_entry_appended {
                                dispatch_messages.push(message.clone());
                            }
                            pending_ordinals.push(ordinal);
                            to_append.push(message);
                        }
                        if to_append.is_empty() {
                            Vec::new()
                        } else {
                            let entry_ids =
                                SessionManager::append_messages_off_thread(session, to_append)
                                    .await?;
                            // The recovery omission resolves its targets by
                            // ordinal; record the ids this batch persisted
                            // (the sink records its own committed appends).
                            for (ordinal, entry_id) in pending_ordinals.iter().zip(&entry_ids) {
                                persist_ledger.record_entry(*ordinal, entry_id);
                            }
                            entry_ids.into_iter().zip(dispatch_messages).collect()
                        }
                    };
                    if dispatch_entry_appended {
                        for (entry_id, message) in &appended {
                            // One wire payload per entry: the plugin dispatch
                            // and the session-level event share the same
                            // upstream `SessionEntry` shape.
                            let entry_json = serde_json::json!({
                                "id": entry_id,
                                "message": message_json(message),
                            });
                            if dispatch_entry_appended_to_plugins {
                                if let Some(plugins) = &self.plugins {
                                    emit_plugin_event(
                                        plugins,
                                        "entry_appended",
                                        serde_json::json!({
                                            "type": "entry_appended",
                                            "entry": entry_json.clone(),
                                        }),
                                    )
                                    .await;
                                }
                            }
                            self.emit_session_event(SessionEvent::EntryAppended {
                                entry: entry_json,
                            });
                        }
                    }
                }
                None => {
                    let mut history = self
                        .history
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let slot = Arc::make_mut(&mut *history);
                    // The starting messages the loop appended (finalized by
                    // their `message_end`) precede this run's produced
                    // messages, and only the FIRST settle iteration carries
                    // them.
                    slot.extend(result.preamble.iter().cloned());
                    slot.extend(produced);
                    trim_ephemeral_history(
                        slot,
                        MAX_EPHEMERAL_HISTORY_MESSAGES,
                        MAX_EPHEMERAL_HISTORY_TOKENS,
                    );
                }
            }
            // P10: record the VIRTUAL selection on the branch, separately from
            // the responses (upstream `_recordSelection`, agent-session.ts:625-638).
            // Responses can only name the physical model that answered, so
            // without this entry a resume would restore the physical model and
            // silently drop the virtual selection. pir's
            // `get_session_context_settings` resolves the branch tail
            // last-entry-wins (upstream `getBranchSelection` instead prefers a
            // virtual `model_change` that precedes the response), so the entry
            // is appended AFTER the turn's messages: that is what makes the
            // virtual selection win on the next read without changing the
            // shared projection.
            if let Some(registration) = &routed {
                self.record_virtual_selection(&registration.provider, &registration.id)
                    .await;
            }
            self.turns.fetch_add(result.turns, Ordering::SeqCst);
            self.tokens.fetch_add(produced_tokens, Ordering::SeqCst);

            // Session-backed overflow recovery (upstream `_runAutoCompaction
            // ("overflow")`): persist the recovery compaction as a session
            // entry so the retry — and every later prompt — rebuilds from the
            // compacted projection. The one-attempt guard
            // (`overflow_recovery_attempted`) still bounds this to a single
            // compact-and-retry per prompt.
            if overflow_session_retry {
                overflow_session_retry = false;
                if let Some(session) = &self.session {
                    // Persist the recovery omission FIRST (upstream
                    // `_omitRecoveryAttempt` runs before the overflow
                    // compaction): the failed assistant and every tool result
                    // derived from its calls already entered storage above, so
                    // a pair of context edits removes both from the projection
                    // before the compaction cut is chosen. Persisting only the
                    // assistant was the #11227 orphan-tool-result bug.
                    if let Some(ordinals) = &omission_ordinals {
                        omit_recovery_attempt_entries(session, &persist_ledger, ordinals).await;
                    }
                    let config = self.compaction_config().unwrap_or_default();
                    // Overflow run bookkeeping (upstream
                    // `_runAutoCompaction("overflow")` session events:
                    // `compaction_start` before the run, `compaction_end`
                    // with the wire result / errorMessage on every exit).
                    let mut run = CompactionRunGuard::start(
                        Some(Arc::clone(&self.session_events)),
                        Some(Arc::clone(&self.compacting)),
                        "overflow",
                    );
                    // Cut entries over the FRESH projection: it now includes
                    // the persisted run messages (the failed attempt and its
                    // results were context-edited out above), so
                    // `first_kept_entry_id` lands on a real storage entry id.
                    // The overflow recovery summarizes the FRESH session
                    // projection (it now includes the persisted run messages,
                    // so `first_kept_entry_id` lands on a real storage entry
                    // id). The widen + projection + branch walk are blocking
                    // work over a transcript-sized file, so they run on the
                    // blocking pool under one `blocking_lock` (the same
                    // sequence, without parking a runtime worker or holding
                    // the async session lock across it).
                    let (projection, branch) = project_session_off_thread(session).await?;
                    let mut from_extension = false;
                    let compacted = {
                        let metas =
                            compaction::ProjectionEntryMeta::from_projection(&projection.entries);
                        let refs =
                            compaction::ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
                        match compaction::prepare_compaction_refs(&refs, &branch, &config) {
                            Some(preparation) => {
                                // `session_before_compact` (overflow): a
                                // handler may cancel or replace the recovery
                                // compaction, exactly like the manual/threshold
                                // paths (upstream agent-session.ts:3184-3204).
                                let branch_entries =
                                    if self.plugins.as_ref().is_some_and(|plugins| {
                                        plugins.has_handlers("session_before_compact")
                                    }) {
                                        session
                                            .lock()
                                            .await
                                            .branch_entries(None)
                                            .unwrap_or_default()
                                    } else {
                                        Vec::new()
                                    };
                                let decision = super::compact::dispatch_before_compact(
                                    self.plugins.as_ref(),
                                    "overflow",
                                    true,
                                    None,
                                    &preparation,
                                    &branch_entries,
                                    signal.is_aborted(),
                                )
                                .await;
                                match decision {
                                    super::compact::BeforeCompactDecision::Cancel => {
                                        // Upstream: a handler cancel is an
                                        // ABORT, not a failure (no
                                        // errorMessage); end-then-failed.
                                        self.emit_session_event(SessionEvent::CompactionEnd {
                                            reason: "overflow",
                                            result: None,
                                            aborted: true,
                                            will_retry: false,
                                            error_message: None,
                                        });
                                        self.emit_compact_failed("overflow", None, true, false)
                                            .await;
                                        run.end_emitted();
                                        None
                                    }
                                    super::compact::BeforeCompactDecision::Replace(replacement) => {
                                        from_extension = true;
                                        Some(Ok(*replacement))
                                    }
                                    super::compact::BeforeCompactDecision::Proceed => {
                                        let retry_observer = self.summarization_retry_observer(
                                            super::events::SummarizationRetrySource::Compaction {
                                                reason: "overflow",
                                            },
                                        );
                                        Some(
                                            compaction::with_summarization_retry(
                                                retry_observer,
                                                compaction::compact_with_preparation_refs(
                                                    preparation,
                                                    &current_model,
                                                    self.complete_fn(),
                                                    None,
                                                ),
                                            )
                                            .await,
                                        )
                                    }
                                }
                            }
                            None => {
                                Some(Err("compaction found no messages to summarize".to_string()))
                            }
                        }
                    };
                    match compacted {
                        Some(Ok(compacted)) => {
                            // The compaction append builds the current system
                            // message from the whole projection and fsyncs the
                            // first write: run it on the blocking pool (the
                            // async session lock is acquired inside the
                            // blocking task).
                            let appended_entry_id =
                                match SessionManager::append_compaction_off_thread(
                                    session,
                                    compacted.summary.clone(),
                                    compacted.first_kept_entry_id.clone(),
                                    compacted.tokens_before,
                                    Some(compaction_details(&compacted)),
                                    Some(compacted.usage),
                                    from_extension,
                                )
                                .await
                                {
                                    Ok(entry_id) => entry_id,
                                    Err(error) => {
                                        // Storage did not take the recovery
                                        // compaction: nothing was cut, the failed
                                        // attempt and its omission edits stay as
                                        // persisted, and the retry context is
                                        // never built. Upstream
                                        // `_runAutoCompaction` catches the thrown
                                        // append, emits end-then-failed, returns
                                        // false (no retry) and lets the prompt
                                        // settle on the overflow terminal — do the
                                        // same instead of aborting the prompt with
                                        // the storage error.
                                        let aborted = signal.is_aborted();
                                        let error_message = if aborted {
                                            None
                                        } else {
                                            Some(format!(
                                                "Context overflow recovery failed: {error}"
                                            ))
                                        };
                                        crate::ext_diag!(
                                        "[session] overflow compaction persistence failed: {error}"
                                    );
                                        self.emit_session_event(SessionEvent::CompactionEnd {
                                            reason: "overflow",
                                            result: None,
                                            aborted,
                                            will_retry: false,
                                            error_message: error_message.clone(),
                                        });
                                        self.emit_compact_failed(
                                            "overflow",
                                            error_message,
                                            aborted,
                                            from_extension,
                                        )
                                        .await;
                                        run.end_emitted();
                                        break 'settle result;
                                    }
                                };
                            // The retry context is rebuilt FROM the session
                            // projection (the durable compacted state), never
                            // from the in-memory cut. D6: the projection runs
                            // on the blocking pool instead of a runtime worker
                            // under the async session lock.
                            let messages = project_session_messages_off_thread(session).await?;
                            let estimated_tokens_after =
                                compaction::estimate_context_tokens(&messages).tokens;
                            if let Some(plugins) = &self.plugins {
                                // The full `CompactionResult` for the JS
                                // `ctx.compact({ onComplete })` settler
                                // (`payload.compaction ?? payload.result`);
                                // onError is already complete. The upstream
                                // `compactionEntry` (the persisted entry)
                                // rides alongside it.
                                let mut payload = serde_json::json!({
                                    "type": "session_compact",
                                    "reason": "overflow",
                                    "willRetry": true,
                                    "fromExtension": from_extension,
                                    "compaction": compaction_result_wire(
                                        &compacted,
                                        estimated_tokens_after,
                                    ),
                                });
                                if let Some(entry) = super::compact::compaction_entry_wire(
                                    Some(session),
                                    Some(&appended_entry_id),
                                )
                                .await
                                {
                                    payload["compactionEntry"] = entry;
                                }
                                emit_plugin_event(plugins, "session_compact", payload).await;
                            }
                            self.emit_session_event(SessionEvent::CompactionEnd {
                                reason: "overflow",
                                result: Some(compaction_result_wire(
                                    &compacted,
                                    estimated_tokens_after,
                                )),
                                aborted: false,
                                will_retry: true,
                                error_message: None,
                            });
                            run.end_emitted();
                            recovery_context = Some(Context {
                                system_prompt: run_system_prompt.clone(),
                                messages,
                                tools: None,
                            });
                            continue;
                        }
                        Some(Err(error)) => {
                            // Upstream `_runAutoCompaction("overflow")` catch:
                            // a user abort is the normal aborted outcome (no
                            // errorMessage); every other failure carries the
                            // `Context overflow recovery failed: …` text.
                            // End first, failed second (agent-session.ts:
                            // 3276-3295).
                            let aborted = signal.is_aborted();
                            let error_message = if aborted {
                                None
                            } else {
                                Some(format!("Context overflow recovery failed: {error}"))
                            };
                            self.emit_session_event(SessionEvent::CompactionEnd {
                                reason: "overflow",
                                result: None,
                                aborted,
                                will_retry: false,
                                error_message: error_message.clone(),
                            });
                            self.emit_compact_failed(
                                "overflow",
                                error_message,
                                aborted,
                                from_extension,
                            )
                            .await;
                            run.end_emitted();
                        }
                        // Handler cancel: already emitted end-then-failed.
                        None => {}
                    }
                }
            }

            // Refresh the ctx facade with the post-run state (usage counters,
            // message list) so settlement handlers observe up-to-date data.
            let post_run_model = self.current_model();
            self.push_session_context(&post_run_model).await;

            // Surface extension side effects (notifications/status) captured
            // during the run and apply the actionable ones (session name,
            // model, thinking level, compaction requests, steer/followUp
            // queues, custom entries, labels, active-tools override).
            if self.plugins.is_some() {
                self.apply_extension_notifications().await;
            }

            // Extension command-context actions (upstream print/rpc
            // `commandContextActions`): apply the actions recorded by THIS
            // session — covering every AgentSession-backed host (TUI / RPC /
            // server / SDK), including host command dispatches made while
            // idle, which wait for the next settle to be applied.
            if self.extensions.is_some() {
                for (action, outcome) in self.apply_session_command_actions().await {
                    if let Err(error) = outcome {
                        crate::ext_diag!("[ext] command action {action:?} failed: {error}");
                    }
                }
            }

            // `agent_before_settle` boundary: handlers may append entries and
            // force one more provider request (upstream runner/agent-session
            // BoundaryState semantics).
            let continue_requested = match &self.plugins {
                Some(plugins) if plugins.has_handlers("agent_before_settle") => {
                    // Only the context and the terminal message are read by the
                    // boundary hook; the run result's produced messages moved
                    // into `produced` above already.
                    self.dispatch_agent_before_settle(plugins, &result.context, &result.terminal)
                        .await
                }
                _ => false,
            };
            if continue_requested && settle_iterations < 8 {
                continue;
            }
            if continue_requested {
                crate::ext_diag!("[ext] agent_before_settle continuation cap reached; settling");
            }
            break 'settle result;
        };

        // The run's agent loop reached its terminal result: no further model
        // request will be issued (the `agent_before_settle` continuation is
        // inside the loop) — only the post-run settlement work below remains.
        // Publish that transition so a manual `compact()` arriving now waits
        // for the run's claim to release instead of rejecting as if the agent
        // were still streaming (upstream's idle boundary: `_emitAgentSettled`
        // clears `_isAgentRunActive` before emitting `agent_settled`).
        guard.mark_settling();
        // Late background completions: a completion that lands from now on is
        // persisted but not queued. The enqueue gate was closed by
        // `mark_settling` BEFORE this drain, so a completion that read a stale
        // non-settling flag cannot slip a frame in after the drain (the
        // check-and-send and the close-and-drain are one mutex-ordered
        // transition). Frames already queued by a completion that beat the
        // gate belong to the transcript as their `backgroundToolResult` entry
        // — the loop has no poll left, so a queued frame would be injected
        // into the NEXT prompt on top of the entry's projection (the same
        // completion delivered twice). Drop them here; a store-less session
        // keeps them, because the frame is its only durable surface.
        if self.session.is_some() {
            let dropped = self.background_follow_up_inbox.drain_all().await.len();
            if dropped > 0 {
                tracing::debug!(
                    dropped,
                    "dropped settled-run background frames (already persisted)"
                );
            }
        }

        // Cache warming (upstream `cacheWarming`): at most one warm request
        // per settled run, never on error/abort, and the warm request
        // bypasses the agent loop (no recursion). Both modes spawn a DETACHED
        // bounded task — settling never waits on warming (upstream arms an
        // unref'd timer) — aborted by the next run's startup (run_prompt
        // aborts the registered warm signal) so a warm never overlaps the
        // next streaming request.
        self.maybe_warm_cache(&result.terminal, &signal, run_system_prompt)
            .await;

        // Deferred bash-execution records (upstream `_flushPendingBashMessages`).
        self.flush_pending_bash_records().await;

        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("agent_settled") {
                emit_plugin_event(
                    plugins,
                    "agent_settled",
                    serde_json::json!({
                        "type": "agent_settled",
                        "aborted": result.aborted,
                    }),
                )
                .await;
            }
        }
        // Session-level `agent_settled` (upstream `_emitAgentSettled`,
        // agent-session.ts:1085): emitted after the plugin dispatch so
        // session-event subscribers observe the same terminal settlement on
        // the success path as on the failed-run path above.
        self.emit_session_event(SessionEvent::AgentSettled {
            aborted: result.aborted,
        });

        // Queued steering/follow-up messages live in the inbox consumer task
        // (`Inbox`); they survive this function and are polled by the next
        // run, on every exit path.
        Ok(result.terminal)
    }

    /// Record the VIRTUAL selection on the session branch when the branch does
    /// not already imply it (upstream `_recordSelection`,
    /// `agent-session.ts:625-638`).
    ///
    /// A routed run's responses name the PHYSICAL model (P10), so the virtual
    /// selection exists only here; without this entry a resume restores the
    /// physical model and the next request goes to it directly instead of
    /// through the router.
    ///
    /// The comparison is pir's restore rule rather than upstream's
    /// `getBranchSelection`: [`crate::session_manager::projection::
    /// get_session_context_settings`] resolves a branch tail-last-wins, so a
    /// physical response appended after the entry would hide the virtual
    /// selection from a resume. The entry is therefore re-appended after every
    /// routed turn (one small entry, not the transcript) — the steady state
    /// upstream reaches by preferring a virtual `model_change` over later
    /// responses, which pir's shared projection does not do.
    async fn record_virtual_selection(&self, provider: &str, model_id: &str) {
        let Some(session) = &self.session else {
            return;
        };
        let mut session = session.lock().await;
        let path = crate::session_manager::projection::build_session_path(
            session.resident_entries(),
            session.leaf_id(),
        );
        let recorded = path.iter().rev().find_map(|entry| match entry {
            FileEntry::ModelChange(change) => {
                Some((change.provider.clone(), change.model_id.clone()))
            }
            FileEntry::Message(message) => match &message.message {
                crate::session_manager::StoredMessage::Assistant(assistant) => {
                    Some((assistant.provider.clone(), assistant.model.clone()))
                }
                _ => None,
            },
            _ => None,
        });
        if recorded.as_ref().map(|(p, m)| (p.as_str(), m.as_str())) == Some((provider, model_id)) {
            return;
        }
        if let Err(error) = session.append_model_change(provider, model_id) {
            crate::ext_diag!("[session] failed to record the virtual model selection: {error}");
        }
    }
}

/// Reset-on-drop guard for one `prompt()` run's session claim.
///
/// Owns the run's claim on the session ([`SESSION_RUNNING`]), its
/// abort/settling flags, and the extension runtime's live `ctx.waitForIdle()`
/// cell, and clears them on EVERY exit path: normal return, an error bubbling
/// out of `run_prompt`, a panic unwinding through `prompt`, or the caller's
/// future being dropped mid-await (a host's `prompt_task.abort()` never runs
/// post-await cleanup, but it does drop every local of the aborted future,
/// including this guard). Releasing the claim wakes every `compact()` waiter
/// (upstream `waitForIdle`); without the guard those exits leave the session
/// permanently claimed — "already streaming" rejections forever and the
/// extension idle cell stuck.
struct SessionStreamingGuard {
    state: Arc<AtomicU8>,
    run_abort_requested: Arc<AtomicBool>,
    run_settling: Arc<AtomicBool>,
    /// The session's background registry: `mark_settling` closes its
    /// follow-up enqueue gate BEFORE the settle path drains the inbox.
    background: Arc<background::BackgroundRegistry>,
    /// Whether queued completion frames have a durable copy (store-backed):
    /// only then does settling close the gate and drain. A store-less run
    /// keeps frames queued (they are its only delivery surface).
    background_queue_gated: bool,
    idle_notify: Arc<Notify>,
    js: Option<crate::extensions::ExtensionHandle>,
}

impl SessionStreamingGuard {
    /// Mark the run's agent loop as finished (only post-run work remains), so
    /// a manual `compact()` arriving while the run winds down waits for the
    /// claim instead of rejecting as if the agent were still streaming.
    ///
    /// Store-backed sessions close the background follow-up enqueue gate
    /// here, in the same step as the flag: the settle path drains the inbox
    /// right after this, and the gate makes the close + drain a single-owner
    /// transition with respect to a completion's enqueue (see
    /// [`background::FollowUpQueueState`]).
    fn mark_settling(&self) {
        self.run_settling.store(true, Ordering::SeqCst);
        if self.background_queue_gated {
            self.background.close_follow_up_queue();
        }
    }
}

impl Drop for SessionStreamingGuard {
    fn drop(&mut self) {
        // Ordered teardown, claim LAST:
        // 1. the extension idle cell first — a new run that claims the session
        //    (which can only happen after step 3) must never have its
        //    `set_session_streaming(true)` clobbered by this run's reset;
        // 2. the run-scoped flags while the claim is still held, so a
        //    `compact()` observer can never see RUNNING with the settling flag
        //    already cleared (it would reject as "streaming" instead of
        //    waiting for the release that is one store away);
        // 3. the claim release;
        // 4. the wake-up after the release, so every waiter re-reads IDLE.
        if let Some(js) = &self.js {
            js.set_session_streaming(false);
        }
        self.run_settling.store(false, Ordering::SeqCst);
        self.run_abort_requested.store(false, Ordering::SeqCst);
        self.state.store(SESSION_IDLE, Ordering::SeqCst);
        self.idle_notify.notify_waiters();
    }
}
