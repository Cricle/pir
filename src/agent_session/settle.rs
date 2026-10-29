use super::*;

/// Resumable record of which of a run's produced messages the per-message
/// persistence sink already handled.
///
/// The former shape was a bare `AtomicUsize` counter incremented on every
/// SUCCESSFUL append and read by the post-run batch as `skip(n)`. That is not
/// a prefix: the sink SKIPS overflow-recovery attempts without counting them,
/// so a run whose middle message was omitted made the counter lag the produced
/// list — the batch then re-appended an already-persisted message (a
/// duplicate) or skipped one it had never written (a loss).
///
/// This ledger tracks PER-ORDINAL identity instead: the sink claims one
/// ordinal per call (exactly one call per produced message, in append order)
/// and marks it handled when its append commits. It also records the entry id
/// each committed append returned: the post-run overflow-recovery omission
/// (upstream `_omitRecoveryAttempt`) targets exactly the failed attempt's
/// entries by ordinal, so duplicate or empty tool-call ids can never make the
/// omission hit an unrelated entry. `is_handled` is the batch's only
/// question, so no count is ever compared against a list that raced it.
///
/// A failed append installs a POISON BARRIER at its ordinal (`failed_from`).
/// Later sink calls still claim their ordinal but stop writing: they leave it
/// unhandled so the post-run batch re-appends the whole tail `[barrier..)`
/// in produced order. This is what makes "retry from the failure" exact:
/// the sink never persists a later message out of order (which the batch
/// would then either duplicate or skip), and the batch never re-appends a
/// message the sink already committed.
///
/// A partial settle (a run torn down mid-way) can be RESUMED: the ledger is
/// per settle iteration, so the next iteration starts clean and the batch
/// persists exactly the messages this run produced that are not yet handled.
#[derive(Debug)]
#[doc(hidden)]
pub struct PersistLedger {
    /// Next ordinal the sink will claim (monotone, one per sink call).
    claimed: AtomicUsize,
    /// Length of the handled PREFIX (ordinals `[0, prefix)` are handled).
    prefix: AtomicUsize,
    /// Handled ordinals past `prefix` (a gap below them is still open: an
    /// ordinal the post-run batch owns). Small and bounded by the number of
    /// gaps inside one run, which is tiny in practice.
    ahead: std::sync::Mutex<std::collections::BTreeSet<usize>>,
    /// First ordinal whose append failed, or [`NO_FAILURE`]. Set once; the
    /// sink refuses to persist any ordinal past it until the post-run batch
    /// re-appends the tail in order.
    failed_from: AtomicUsize,
    /// Entry id returned by the committed append for each handled ordinal.
    /// Cleared by [`Self::reset`] with the rest of the iteration state.
    entries: std::sync::Mutex<std::collections::BTreeMap<usize, String>>,
}

/// Sentinel for [`PersistLedger::failed_from`].
const NO_FAILURE: usize = usize::MAX;

impl Default for PersistLedger {
    fn default() -> Self {
        Self {
            claimed: AtomicUsize::new(0),
            prefix: AtomicUsize::new(0),
            ahead: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            failed_from: AtomicUsize::new(NO_FAILURE),
            entries: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }
}

impl PersistLedger {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reset for a new settle iteration: every ordinal is unclaimed, the
    /// prefix is empty and no failure barrier is installed (the iteration's
    /// produced list starts fresh).
    pub(crate) fn reset(&self) {
        self.claimed.store(0, Ordering::SeqCst);
        self.prefix.store(0, Ordering::SeqCst);
        self.failed_from.store(NO_FAILURE, Ordering::SeqCst);
        self.ahead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Claim the next ordinal (one per produced message, in sink order).
    fn claim(&self) -> usize {
        self.claimed.fetch_add(1, Ordering::SeqCst)
    }

    /// Record that `ordinal` needs no further persistence: its append
    /// committed. (The `*_for_test` seams also use this for an intentional
    /// omission.) Advancing contiguously keeps `ahead` small.
    fn handle(&self, ordinal: usize) {
        let mut ahead = self
            .ahead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut prefix = self.prefix.load(Ordering::SeqCst);
        if ordinal < prefix {
            return;
        }
        if ordinal == prefix {
            prefix += 1;
            while ahead.remove(&prefix) {
                prefix += 1;
            }
            self.prefix.store(prefix, Ordering::SeqCst);
        } else {
            ahead.insert(ordinal);
        }
    }

    /// Record the entry id a committed append returned for `ordinal`. The
    /// post-run recovery omission resolves its targets through this map.
    pub(super) fn record_entry(&self, ordinal: usize, entry_id: &str) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ordinal, entry_id.to_owned());
    }

    /// Entry id of the committed append for `ordinal`, when it was persisted.
    pub(super) fn entry_id(&self, ordinal: usize) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&ordinal)
            .cloned()
    }

    /// Install the failure barrier at `ordinal` (idempotent; the earliest
    /// failure wins). Every ordinal past it stays unhandled so the batch
    /// re-appends the tail in produced order.
    fn fail(&self, ordinal: usize) {
        self.failed_from.fetch_min(ordinal, Ordering::SeqCst);
    }

    /// Whether the sink must SKIP its append for `ordinal` because an earlier
    /// append already failed (the batch owns the whole tail from there).
    fn blocked_by_failure(&self, ordinal: usize) -> bool {
        let failed_from = self.failed_from.load(Ordering::SeqCst);
        failed_from != NO_FAILURE && ordinal > failed_from
    }

    /// Whether `ordinal` is already handled (committed append or intentional
    /// omission) and must not be persisted again by the post-run batch.
    ///
    /// This is a per-ordinal question, never `index < count`: a handled
    /// ordinal past a gap (an intentional omission after a failure barrier)
    /// must be skipped exactly like one inside the contiguous prefix.
    pub(super) fn is_handled(&self, ordinal: usize) -> bool {
        // Take the same lock `handle` holds while it advances `prefix`: a
        // lock-free prefix read could observe the old prefix, lose the race
        // with `handle`, and then miss the freshly removed `ahead` entry.
        let ahead = self
            .ahead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ordinal < self.prefix.load(Ordering::SeqCst) {
            return true;
        }
        ahead.contains(&ordinal)
    }
}

/// `#[doc(hidden)]` deterministic seams for the settle-persistence regression
/// tests: the ledger is crate-private by design, but its ordinal/barrier
/// semantics are the property under test.
#[doc(hidden)]
impl PersistLedger {
    pub fn new_for_test() -> Arc<Self> {
        Self::new()
    }

    pub fn claim_for_test(&self) -> usize {
        self.claim()
    }

    pub fn handle_for_test(&self, ordinal: usize) {
        self.handle(ordinal);
    }

    pub fn fail_for_test(&self, ordinal: usize) {
        self.fail(ordinal);
    }

    pub fn blocked_for_test(&self, ordinal: usize) -> bool {
        self.blocked_by_failure(ordinal)
    }

    pub fn is_handled_for_test(&self, ordinal: usize) -> bool {
        self.is_handled(ordinal)
    }

    pub fn reset_for_test(&self) {
        self.reset();
    }

    pub fn prefix_for_test(&self) -> usize {
        self.prefix.load(Ordering::SeqCst)
    }
}

impl AgentSession {
    /// Chain the extension `turn_end` boundary onto the host-installed
    /// `finish_turn` hook (upstream `_installAgentBoundaryHooks`): the
    /// boundary dispatches `turn_end` (entries drafts are committed, `continue`
    /// guarantees one more provider request), then the host hook runs; an
    /// `End` decision from the host hook still wins.
    pub(super) fn turn_end_boundary_hook(
        &self,
        inner: Option<FinishTurnHook>,
    ) -> Option<FinishTurnHook> {
        // No plugin host: the host-installed `finish_turn` hook IS the whole
        // boundary. (The former `self.plugins.clone()?` discarded the hook on
        // plugin-less sessions, so `set_finish_turn` silently did nothing for
        // exactly the embedders/SDK agents that install it without plugins.)
        let Some(plugins) = self.plugins.clone() else {
            return inner;
        };
        if !plugins.has_handlers("turn_end") {
            return inner;
        }
        let session = self.session.clone();
        let pending = Arc::clone(&self.pending_messages);
        Some(Arc::new(move |context: FinishTurnContext| {
            let plugins = Arc::clone(&plugins);
            let session = session.clone();
            let pending = Arc::clone(&pending);
            let inner = inner.clone();
            Box::pin(async move {
                // The loop hands over the SAME zero-based turn index it
                // emitted as `turn_start.turnIndex` (upstream shares one
                // `_turnIndex`), so the boundary no longer keeps a separate
                // 1-based `fetch_add(1) + 1` counter that disagreed with
                // `turn_start` on every turn.
                let turn_index = context.turn_index;
                let extension_continue = dispatch_turn_end_boundary(
                    &plugins,
                    &session,
                    turn_index,
                    &context,
                    pending.load(Ordering::SeqCst) > 0,
                )
                .await;
                if let Some(inner) = inner {
                    // The extension boundary already consumed every borrow it
                    // needs; hand the snapshot over instead of deep-cloning
                    // the turn's assistant message and tool results.
                    if let Some(action) = inner(context).await {
                        return Some(action);
                    }
                }
                if extension_continue {
                    return Some(FinishTurnAction::Continue);
                }
                None
            })
        }))
    }

    /// `agent_before_settle` boundary dispatch: handlers receive the boundary
    /// state (`entries`, `continue`, `context` preview, `outcome`), returned
    /// entry drafts are committed to session storage, and a `continue` result
    /// (honored only when the context can still run) guarantees one more
    /// provider request.
    ///
    /// Takes the two run-result fields the boundary reads (`context` and
    /// `terminal`) rather than the whole [`agent_loop::LoopResult`]: the
    /// caller MOVES the run's produced messages out of that result for
    /// persistence (`prompt_loop.rs`), so the borrow it can offer is exactly
    /// these two fields.
    pub(super) async fn dispatch_agent_before_settle(
        &self,
        plugins: &PluginHost,
        context: &Context,
        terminal: &AssistantMessage,
    ) -> bool {
        // Upstream canContinue for the pre-settle boundary also accepts an
        // assistant-final context when messages are queued (the re-run picks
        // them up).
        let can_continue =
            context_can_continue(context) || self.pending_messages.load(Ordering::SeqCst) > 0;
        let mut payload = serde_json::json!({
            "type": "agent_before_settle",
            "outcome": outcome_label(terminal),
            "entries": [],
            "continue": false,
            "context": {
                "contextMessages": context
                    .messages
                    .iter()
                    .map(message_json)
                    .collect::<Vec<_>>(),
                "canContinue": can_continue,
            },
        });
        let outcome = match plugins.emit("agent_before_settle", &mut payload).await {
            Ok(outcome) => outcome,
            Err(error) => {
                crate::ext_diag!("[ext] agent_before_settle dispatch failed: {error}");
                return false;
            }
        };
        let continue_requested =
            outcome.result.get("continue").and_then(JsonValue::as_bool) == Some(true);
        let drafts = outcome
            .result
            .get("entries")
            .cloned()
            .unwrap_or(JsonValue::Null);
        if let Some(session) = &self.session {
            commit_boundary_drafts(session, &drafts).await;
        }
        if continue_requested && !can_continue {
            crate::ext_diag!(
                "[ext] agent_before_settle requested continuation without runnable model context"
            );
            return false;
        }
        continue_requested
    }

    /// The per-run per-message persistence sink (upstream persists each
    /// `message_end` DURING the run, agent-session.ts:1144-1163): appends
    /// every message the loop produces to session storage as it is appended
    /// to the context, so a mid-run crash loses nothing. Virtual-routing
    /// registration is stamped onto assistant messages at sink time (the
    /// former post-run stamping cannot cover per-message persistence).
    ///
    /// A message that the post-run overflow recovery will OMIT is persisted
    /// here like every other message. Upstream persists the failed attempt at
    /// `message_end` and only then appends `replacement: null` context edits
    /// for the assistant and its tool results (`_omitRecoveryAttempt`);
    /// skipping the assistant in the sink left its synthetic length-stop tool
    /// results without a matching assistant on disk, and every resumed
    /// provider request then failed with an unpaired `toolResult` (#11227).
    /// The post-run recovery in `prompt_loop` records both sides' entry ids
    /// from this ledger and omits them together.
    /// `None` when no session storage is configured.
    pub(super) fn persist_message_sink(
        &self,
        routed: Arc<std::sync::Mutex<Option<crate::ai::virtual_::VirtualModelRegistration>>>,
        ledger: Arc<PersistLedger>,
    ) -> Option<agent_loop::PersistMessageFn> {
        let session = self.session.clone()?;
        let plugins = self.plugins.clone();
        // Session-level `entry_appended` subscribers (see the payload gate
        // below): cloned into the 'static sink as its own emitter.
        let session_events = Arc::clone(&self.session_events);
        let session_event_subscribers = self.has_session_event_subscribers();
        Some(Arc::new(move |mut message: Message| {
            let session = session.clone();
            let plugins = plugins.clone();
            let routed = Arc::clone(&routed);
            let ledger = Arc::clone(&ledger);
            let session_events = Arc::clone(&session_events);
            Box::pin(async move {
                // The loop calls this sink exactly once per message it appends
                // to the run's produced list, in append order, so the call
                // ordinal IS the message's index in that list. Claiming it up
                // front — before any early return — is what makes the ledger a
                // stable per-message identity (see [`PersistLedger`]): the
                // post-run batch asks it which ordinals still need persisting,
                // and the recovery omission asks it for the committed entry
                // id of every message it must omit.
                let ordinal = ledger.claim();
                let routed_registration = routed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let (Message::Assistant(assistant), Some(registration)) =
                    (&mut message, &routed_registration)
                {
                    assistant.provider = registration.provider.clone();
                    assistant.model = registration.id.clone();
                }
                // A previous append already failed: leave this ordinal
                // unhandled so the post-run batch re-appends the whole tail in
                // produced order. Persisting it here would put a later message
                // into storage ahead of the failed one (and the retry would
                // then duplicate or skip it).
                if ledger.blocked_by_failure(ordinal) {
                    return;
                }
                // The `entry_appended` dispatch is skipped when NEITHER a
                // plugin handler nor a session-event subscriber observes it:
                // the message then moves into storage (no clone) and no wire
                // payload is built.
                let dispatch_entry_appended_to_plugins = plugins
                    .as_ref()
                    .is_some_and(|plugins| plugins.has_handlers("entry_appended"));
                let dispatch_entry_appended =
                    dispatch_entry_appended_to_plugins || session_event_subscribers;
                // Append off the async runtime: the JSONL write is blocking
                // I/O, so the manager lock is acquired on the blocking pool.
                // The `entry_appended` dispatch (a JS handler with a long call
                // timeout) still runs AFTER the append and never holds the
                // storage lock.
                let appended = if dispatch_entry_appended {
                    SessionManager::append_message_off_thread(&session, message.clone())
                        .await
                        .map(|entry_id| (entry_id, Some(message)))
                } else {
                    SessionManager::append_message_off_thread(&session, message)
                        .await
                        .map(|entry_id| (entry_id, None))
                };
                match appended {
                    Ok((entry_id, dispatch_message)) => {
                        // The message is durably in storage: mark its ordinal
                        // handled (the post-run batch skips it) and remember
                        // the entry id (the recovery omission targets it).
                        ledger.handle(ordinal);
                        ledger.record_entry(ordinal, &entry_id);
                        if let Some(message) = &dispatch_message {
                            // One wire payload per entry: the plugin dispatch
                            // and the session-level event share the same
                            // upstream `SessionEntry` shape.
                            let entry_json = serde_json::json!({
                                "id": entry_id,
                                "message": message_json(message),
                            });
                            if dispatch_entry_appended_to_plugins {
                                if let Some(plugins) = &plugins {
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
                        }
                    }
                    Err(error) => {
                        // Leave the ordinal unhandled and install the failure
                        // barrier: the post-run batch retries this exact
                        // message and the whole unhandled tail in produced
                        // order, instead of skipping a fixed count (duplicate)
                        // or letting later sink calls land out of order
                        // (duplicate on retry).
                        ledger.fail(ordinal);
                        crate::ext_diag!("[session] per-message persistence failed: {error}");
                    }
                }
            })
        }))
    }
}

/// Whether the boundary context can still run a provider request (upstream
/// `BoundaryContextPreview.canContinue`, simplified): non-empty non-system
/// transcript whose last message is not an assistant turn.
fn context_can_continue(context: &Context) -> bool {
    let has_non_system = context
        .messages
        .iter()
        .any(|message| !matches!(message, Message::System(_)));
    let last_is_assistant = matches!(context.messages.last(), Some(Message::Assistant(_)));
    has_non_system && !last_is_assistant
}

/// Upstream `AgentActivityOutcome` label for a turn's terminal message.
fn outcome_label(message: &AssistantMessage) -> &'static str {
    match message.stop_reason {
        StopReason::Aborted => "aborted",
        StopReason::Error => "error",
        _ => "completed",
    }
}
/// `turn_end` boundary dispatch (upstream `_dispatchTurnEndBoundary`):
/// handlers receive the boundary state (`entries`, `continue`, `context`
/// preview, `outcome`), returned entry drafts are committed to session
/// storage, and a `continue` result (honored only when the context can still
/// run) guarantees one more provider request.
async fn dispatch_turn_end_boundary(
    plugins: &PluginHost,
    session: &Option<Arc<tokio::sync::Mutex<SessionManager>>>,
    turn_index: usize,
    context: &FinishTurnContext,
    pending_messages: bool,
) -> bool {
    let can_continue = context_can_continue(&context.context) || pending_messages;
    // Upstream `TurnEndEvent.messageEntryId` / `toolResultEntryIds`: the
    // transcript entries the per-message sink persisted for this turn,
    // resolved newest-first from the session branch. A message the sink
    // intentionally omitted (overflow recovery) has no entry, so the
    // assistant id stays OMITTED from the wire (never a fabricated/null id)
    // and unmatched tool results simply do not contribute an id, exactly like
    // upstream's `flatMap` over resolvable ids. Store-less sessions have no
    // ids at all.
    let (message_entry_id, tool_result_entry_ids) = match session {
        Some(session) => {
            let session = session.lock().await;
            // ONE branch-path walk for the whole turn: the per-message lookups
            // rebuilt the session-wide path index once per tool result.
            session.find_persisted_turn_entry_ids(&context.message, &context.tool_results)
        }
        None => (None, Vec::new()),
    };
    let mut payload = serde_json::json!({
        "type": "turn_end",
        "turnIndex": turn_index,
        // Serialize the payload structs directly: `Message::Assistant` /
        // `Message::ToolResult` serialize as the payload itself, so wrapping a
        // CLONE in a `Message` only copied the whole turn's output first.
        "message": serde_json::to_value(&context.message).unwrap_or(JsonValue::Null),
        "toolResults": context
            .tool_results
            .iter()
            .map(|result| serde_json::to_value(result).unwrap_or(JsonValue::Null))
            .collect::<Vec<_>>(),
        "toolResultEntryIds": tool_result_entry_ids,
        "entries": [],
        "continue": false,
        "outcome": outcome_label(&context.message),
        "context": {
            "contextMessages": context
                .context
                .messages
                .iter()
                .map(message_json)
                .collect::<Vec<_>>(),
            "canContinue": can_continue,
        },
    });
    if let Some(message_entry_id) = message_entry_id {
        payload["messageEntryId"] = serde_json::json!(message_entry_id);
    }
    let outcome = match plugins.emit("turn_end", &mut payload).await {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::ext_diag!("[ext] turn_end dispatch failed: {error}");
            return false;
        }
    };
    let continue_requested =
        outcome.result.get("continue").and_then(JsonValue::as_bool) == Some(true);
    let drafts = outcome
        .result
        .get("entries")
        .cloned()
        .unwrap_or(JsonValue::Null);
    if let Some(session) = session {
        commit_boundary_drafts(session, &drafts).await;
    }
    if continue_requested && !can_continue {
        crate::ext_diag!("[ext] turn_end requested continuation without runnable model context");
        return false;
    }
    continue_requested
}

/// Commit boundary entry drafts to session storage (upstream
/// `_applyBoundaryDrafts`): `custom`, `custom_message`, `context_edit`, and
/// `compaction` drafts are supported.
async fn commit_boundary_drafts(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    drafts: &JsonValue,
) {
    let Some(draft_list) = drafts.as_array() else {
        return;
    };
    if draft_list.is_empty() {
        return;
    }
    let mut guard = session.lock().await;
    for draft in draft_list {
        let kind = draft
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        // A compaction draft must run on the blocking pool (its projection +
        // first-write fsync are blocking); the async lock is released first so
        // the blocking task can acquire it. Ordering with the other drafts is
        // preserved by committing sequentially.
        if kind == "compaction" {
            let Some(summary) = draft.get("summary").and_then(JsonValue::as_str) else {
                continue;
            };
            drop(guard);
            // Extension boundary draft: from_hook = true (upstream
            // gates file-op carry-over on `!fromHook`).
            let result = SessionManager::append_compaction_off_thread(
                session,
                summary.to_owned(),
                draft
                    .get("firstKeptEntryId")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned),
                draft
                    .get("tokensBefore")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(0),
                draft.get("details").cloned().filter(|d| !d.is_null()),
                None,
                true,
            )
            .await;
            if let Err(error) = result {
                crate::ext_diag!("[ext] failed to append boundary draft: {error}");
            }
            guard = session.lock().await;
            continue;
        }
        let result = match kind {
            "custom" => {
                let Some(custom_type) = draft.get("customType").and_then(JsonValue::as_str) else {
                    continue;
                };
                guard.append_custom_entry(
                    custom_type,
                    draft.get("data").cloned().filter(|data| !data.is_null()),
                )
            }
            "custom_message" => {
                let Some(custom_type) = draft.get("customType").and_then(JsonValue::as_str) else {
                    continue;
                };
                guard.append_custom_message_entry(
                    custom_type,
                    draft
                        .get("content")
                        .cloned()
                        .unwrap_or(JsonValue::Array(Vec::new())),
                    draft
                        .get("display")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false),
                    draft.get("details").cloned().filter(|d| !d.is_null()),
                )
            }
            "context_edit" => {
                let Some(target_id) = draft.get("targetId").and_then(JsonValue::as_str) else {
                    continue;
                };
                guard.append_context_edit(target_id, draft.get("replacement").cloned())
            }
            _ => continue,
        };
        if let Err(error) = result {
            crate::ext_diag!("[ext] failed to append boundary draft: {error}");
        }
    }
    drop(guard);
}
