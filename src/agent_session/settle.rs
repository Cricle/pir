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
/// A partial settle (a run torn down mid-way) can be RESUMED: the ledger's
/// ordinal state is per settle iteration (reset between iterations), so the
/// next iteration starts clean and the batch persists exactly the messages
/// this run produced that are not yet handled. The ledger itself is per
/// PROMPT RUN, so its bound session identity (`run_session_id`) spans the
/// whole run.
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
    /// Concrete session-manager identity (`SessionManager::id`) the PROMPT RUN
    /// belongs to.
    ///
    /// Bound exactly once at `run_prompt` entry, BEFORE the first projection,
    /// extension handler, preamble write, or sink construction, and read from
    /// the manager actually installed in the slot — never from the cached
    /// `session_id` (a concurrent `replace_session_storage` may not have
    /// refreshed it yet). Run-scoped, NOT cleared by [`Self::reset`]: the
    /// prompt loop re-creates the sink per settle iteration, and a storage
    /// swap landing between two iterations must not re-bind the same run's
    /// later writes to the replacement manager.
    run_session_id: std::sync::Mutex<Option<String>>,
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
            run_session_id: std::sync::Mutex::new(None),
        }
    }
}

impl PersistLedger {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Bind the concrete session-manager identity this prompt run belongs to.
    ///
    /// Called ONCE at `run_prompt` entry, before the first projection,
    /// extension handler, preamble write, or sink construction. The caller
    /// reads the identity from the manager installed in the slot under the
    /// storage lock; binding it there — instead of lazily at the first sink
    /// creation — closes the run-start window in which a storage swap could
    /// otherwise re-bind the whole run (preamble included) to the replacement
    /// session.
    pub(crate) fn bind_run_session_id(&self, session_id: Option<String>) {
        *self
            .run_session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session_id;
    }

    /// The identity bound by [`Self::bind_run_session_id`].
    pub(crate) fn run_session_id(&self) -> Option<String> {
        self.run_session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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

/// Storage view shared by the `turn_end` / `agent_before_settle` adapters.
///
/// `AgentSession` stores an unconditional manager behind its stable slot;
/// the print host's slot wraps `Option` (`None` = `--no-session`). The
/// boundary code only needs "look up this turn's persisted entry ids" and
/// "commit these drafts", so both hosts share the same dispatch instead of
/// print re-implementing the payload/continue/draft contract.
#[derive(Clone, Copy)]
pub(crate) enum BoundarySession<'a> {
    /// AgentSession's manager slot.
    Direct(&'a Arc<tokio::sync::Mutex<SessionManager>>),
    /// Print's shared slot (`None` inside the lock = no storage). Only the
    /// print host constructs it, so a no-print build has no variant.
    #[cfg(feature = "print")]
    Shared(&'a Arc<tokio::sync::Mutex<Option<SessionManager>>>),
}

impl BoundarySession<'_> {
    async fn find_persisted_turn_entry_ids(
        &self,
        message: &AssistantMessage,
        tool_results: &[crate::ai::types::ToolResultMessage],
    ) -> (Option<String>, Vec<String>) {
        match self {
            Self::Direct(session) => {
                let session = session.lock().await;
                session.find_persisted_turn_entry_ids(message, tool_results)
            }
            #[cfg(feature = "print")]
            Self::Shared(session) => {
                let guard = session.lock().await;
                guard
                    .as_ref()
                    .map(|session| session.find_persisted_turn_entry_ids(message, tool_results))
                    .unwrap_or_default()
            }
        }
    }

    async fn commit_drafts(&self, run_session_id: Option<&str>, drafts: &JsonValue) {
        match self {
            Self::Direct(session) => commit_boundary_drafts(session, run_session_id, drafts).await,
            #[cfg(feature = "print")]
            Self::Shared(session) => {
                commit_boundary_drafts_shared(session, run_session_id, drafts).await
            }
        }
    }
}

/// Print-slot twin of [`commit_boundary_drafts`]: the shared slot may hold no
/// manager (`--no-session`), and every write re-checks the run identity under
/// the same lock acquisition as the append.
#[cfg(feature = "print")]
async fn commit_boundary_drafts_shared(
    session: &Arc<tokio::sync::Mutex<Option<SessionManager>>>,
    run_session_id: Option<&str>,
    drafts: &JsonValue,
) {
    let Some(draft_list) = drafts.as_array() else {
        return;
    };
    if draft_list.is_empty() {
        return;
    }
    let mut guard = session.lock().await;
    let Some(manager) = guard.as_mut() else {
        return;
    };
    if let Some(expected) = run_session_id {
        if manager.id() != expected {
            tracing::debug!(
                expected_session_id = expected,
                "dropped boundary drafts after the session storage was swapped"
            );
            return;
        }
    }
    for draft in draft_list {
        let kind = draft
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        if kind == "compaction" {
            let Some(summary) = draft.get("summary").and_then(JsonValue::as_str) else {
                continue;
            };
            drop(guard);
            let result = append_compaction_shared_if_session(
                session,
                run_session_id,
                SharedCompactionAppend {
                    summary: summary.to_owned(),
                    first_kept_entry_id: draft
                        .get("firstKeptEntryId")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned),
                    tokens_before: draft
                        .get("tokensBefore")
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(0),
                    details: draft.get("details").cloned().filter(|d| !d.is_null()),
                },
            )
            .await;
            match result {
                Ok(None) => {
                    tracing::debug!(
                        expected_session_id = run_session_id.unwrap_or("<unknown>"),
                        "dropped a boundary compaction draft after the session storage was \
                         swapped"
                    );
                    return;
                }
                Err(error) => {
                    crate::ext_diag!("[ext] failed to append boundary draft: {error}");
                }
                Ok(Some(_)) => {}
            }
            guard = session.lock().await;
            if let Some(expected) = run_session_id {
                if guard
                    .as_ref()
                    .is_some_and(|manager| manager.id() != expected)
                {
                    tracing::debug!(
                        expected_session_id = expected,
                        "dropped the remaining boundary drafts after the session storage was \
                         swapped"
                    );
                    return;
                }
            }
            continue;
        }
        let Some(manager) = guard.as_mut() else {
            return;
        };
        let result = match kind {
            "custom" => {
                let Some(custom_type) = draft.get("customType").and_then(JsonValue::as_str) else {
                    continue;
                };
                manager.append_custom_entry(
                    custom_type,
                    draft.get("data").cloned().filter(|data| !data.is_null()),
                )
            }
            "custom_message" => {
                let Some(custom_type) = draft.get("customType").and_then(JsonValue::as_str) else {
                    continue;
                };
                manager.append_custom_message_entry(
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
                manager.append_context_edit(target_id, draft.get("replacement").cloned())
            }
            _ => continue,
        };
        if let Err(error) = result {
            crate::ext_diag!("[ext] failed to append boundary draft: {error}");
        }
    }
}

/// The compaction fields one boundary `compaction` draft carries into the
/// print slot's blocking append. The draft shape fixes `usage` to `None` and
/// `from_hook` to `true` (upstream boundary drafts never carry usage), so
/// those two stay constants of the callee instead of arguments every caller
/// repeats.
#[cfg(feature = "print")]
struct SharedCompactionAppend {
    summary: String,
    first_kept_entry_id: Option<String>,
    tokens_before: u64,
    details: Option<JsonValue>,
}

/// Compaction form of [`append_message_if_session`] for the print slot.
#[cfg(feature = "print")]
async fn append_compaction_shared_if_session(
    session: &Arc<tokio::sync::Mutex<Option<SessionManager>>>,
    expected_session_id: Option<&str>,
    append: SharedCompactionAppend,
) -> crate::Result<Option<String>> {
    let session = Arc::clone(session);
    let expected_session_id = expected_session_id.map(str::to_owned);
    tokio::task::spawn_blocking(move || -> crate::Result<Option<String>> {
        let mut guard = session.blocking_lock();
        let Some(manager) = guard.as_mut() else {
            return Ok(None);
        };
        if let Some(expected) = expected_session_id.as_deref() {
            if manager.id() != expected {
                return Ok(None);
            }
        }
        manager
            .append_compaction_with_usage(
                append.summary,
                append.first_kept_entry_id.as_deref(),
                append.tokens_before,
                append.details,
                None,
                true,
            )
            .map(Some)
    })
    .await
    .map_err(|error| anyhow::anyhow!("session compaction append blocking task failed: {error}"))?
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
        run_session_id: Option<String>,
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
            let run_session_id = run_session_id.clone();
            Box::pin(async move {
                // The loop hands over the SAME zero-based turn index it
                // emitted as `turn_start.turnIndex` (upstream shares one
                // `_turnIndex`), so the boundary no longer keeps a separate
                // 1-based `fetch_add(1) + 1` counter that disagreed with
                // `turn_start` on every turn.
                let turn_index = context.turn_index;
                let boundary = session.as_ref().map(BoundarySession::Direct);
                let extension_continue = dispatch_turn_end_boundary(
                    &plugins,
                    boundary,
                    turn_index,
                    &context,
                    pending.load(Ordering::SeqCst) > 0,
                    run_session_id.as_deref(),
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
        run_session_id: Option<&str>,
    ) -> bool {
        let boundary = self.session.as_ref().map(BoundarySession::Direct);
        dispatch_agent_before_settle_boundary(
            plugins,
            boundary,
            context,
            terminal,
            self.pending_messages.load(Ordering::SeqCst) > 0,
            run_session_id,
        )
        .await
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
    ///
    /// Storage-swap isolation (R1-L1-002): the returned sink is bound to the
    /// transcript its run started on. `AgentSession::session` is a STABLE
    /// `Arc<Mutex<SessionManager>>` slot whose CONTENTS
    /// [`AgentSession::replace_session_storage`] replaces; holding that Arc
    /// alone would append a still-streaming old run's `message_end` messages
    /// to the replacement session (an assistant without its user prompt, or an
    /// orphan tool result). The sink therefore uses the concrete manager
    /// IDENTITY bound at RUN START ([`PersistLedger::bind_run_session_id`];
    /// the storage generation is kept for the diagnostic log) and re-checks
    /// it under the SAME lock acquisition as the append. Once the slot names
    /// another session the message is DROPPED (debug-logged), never
    /// redirected: the outgoing transcript keeps every message persisted
    /// before the swap, and the replacement never receives output a new
    /// session never requested.
    /// `None` when no session storage is configured.
    pub(super) fn persist_message_sink(
        &self,
        routed: Arc<std::sync::Mutex<Option<crate::ai::virtual_::VirtualModelRegistration>>>,
        ledger: Arc<PersistLedger>,
    ) -> Option<agent_loop::PersistMessageFn> {
        let session = self.session.clone()?;
        // The manager identity this run's transcript belongs to, bound at
        // `run_prompt` entry (before any projection or preamble write). The
        // slot Arc is stable across a swap (its contents are replaced), so the
        // id — not the Arc — is what pins the sink to one transcript. A lazy
        // fallback to the cached identity only exists for defensive callers
        // that construct the sink outside a prompt run; the prompt path always
        // supplies the run-start value.
        let session_id = ledger.run_session_id().or_else(|| self.session_id());
        // Diagnostics only: a `'static` sink cannot read the live generation
        // (the field is a plain AtomicU64 on the session), so the write-time
        // guard compares the concrete manager identity that every swap
        // replacing the transcript changes.
        let storage_generation = self.session_storage_generation();
        let plugins = self.plugins.clone();
        // Session-level `entry_appended` subscribers (see the payload gate
        // below): cloned into the 'static sink as its own emitter.
        let session_events = Arc::clone(&self.session_events);
        Some(Arc::new(move |mut message: Message| {
            let session = session.clone();
            let session_id = session_id.clone();
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
                // plugin handler nor a session-event subscriber wants THIS
                // message: the message then moves into storage (no clone) and
                // no wire payload is built. Per-message gating keeps a
                // TUI-only host from materializing assistant/toolResult
                // payloads it already rendered live.
                let dispatch_entry_appended_to_plugins = plugins
                    .as_ref()
                    .is_some_and(|plugins| plugins.has_handlers("entry_appended"));
                let wants_subscriber = super::notifications::entry_appended_subscriber_wants(
                    &session_events,
                    &message,
                );
                let dispatch_entry_appended =
                    dispatch_entry_appended_to_plugins || wants_subscriber;
                // Append off the async runtime: the JSONL write is blocking
                // I/O, so the manager lock is acquired on the blocking pool.
                // The `entry_appended` dispatch (a JS handler with a long call
                // timeout) still runs AFTER the append and never holds the
                // storage lock. The identity guard runs inside that same
                // blocking lock acquisition, so a storage swap cannot land
                // between the check and the write.
                let appended = if dispatch_entry_appended {
                    append_message_if_session(&session, session_id.as_deref(), message.clone())
                        .await
                        .map(|entry_id| {
                            entry_id.map(|entry_id| (entry_id, Some((message, wants_subscriber))))
                        })
                } else {
                    append_message_if_session(&session, session_id.as_deref(), message)
                        .await
                        .map(|entry_id| entry_id.map(|entry_id| (entry_id, None)))
                };
                match appended {
                    Ok(Some((entry_id, dispatch_message))) => {
                        // The message is durably in storage: mark its ordinal
                        // handled (the post-run batch skips it) and remember
                        // the entry id (the recovery omission targets it).
                        ledger.handle(ordinal);
                        ledger.record_entry(ordinal, &entry_id);
                        if let Some((message, wants_subscriber)) = &dispatch_message {
                            // One wire payload per entry: the plugin dispatch
                            // and the session-level event share the same
                            // upstream `SessionEntry` shape.
                            crate::agent_session::events::note_entry_appended_wire_build();
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
                            if *wants_subscriber {
                                crate::agent_session::events::emit_session_event_to(
                                    &session_events,
                                    SessionEvent::EntryAppended { entry: entry_json },
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        // The slot now names a different session: a storage
                        // swap replaced the manager this run started on while
                        // the run was still streaming. The message belongs to
                        // the outgoing transcript and must not enter the
                        // replacement, so it is dropped by design (the swap
                        // is an explicit user/host action; the replacement
                        // must stay exactly what its own run produces). Mark
                        // the ordinal handled so the post-run batch does not
                        // retry it against the NEW slot.
                        ledger.handle(ordinal);
                        tracing::debug!(
                            expected_session_id = session_id.as_deref().unwrap_or("<unknown>"),
                            storage_generation,
                            "dropped a message_end write after the session storage was swapped"
                        );
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

/// Append `message` to the session only while the manager inside the shared
/// slot is still the transcript this RUN bound at its start
/// (`expected_session_id`).
///
/// Returns `Ok(None)` when a storage swap installed a different manager while
/// the run was in flight: the message belongs to the outgoing transcript and
/// must not enter the replacement session. The identity check and the append
/// run under ONE `blocking_lock` acquisition, so `replace_session_storage`
/// (which replaces the manager under the same async mutex) can never land
/// between them.
pub(crate) async fn append_message_if_session(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    expected_session_id: Option<&str>,
    message: Message,
) -> crate::Result<Option<String>> {
    let Some(expected_session_id) = expected_session_id else {
        // No cached identity: preserve the plain append (the guard is a
        // best-effort hardening for a mis-set identity cache, not a new
        // failure mode).
        return SessionManager::append_message_off_thread(session, message)
            .await
            .map(Some);
    };
    let session = Arc::clone(session);
    let expected_session_id = expected_session_id.to_owned();
    tokio::task::spawn_blocking(move || -> crate::Result<Option<String>> {
        let mut session = session.blocking_lock();
        if session.id() != expected_session_id {
            return Ok(None);
        }
        session.append_message(message).map(Some)
    })
    .await
    .map_err(|error| anyhow::anyhow!("session append blocking task failed: {error}"))?
}

/// Custom-message form of [`append_message_if_session`] for the run's
/// starting messages (`deliverAs: "nextTurn"` + custom-message runs).
pub(crate) async fn append_custom_message_if_session(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    expected_session_id: Option<&str>,
    custom: CustomRunMessage,
) -> crate::Result<Option<String>> {
    let Some(expected_session_id) = expected_session_id else {
        let session = Arc::clone(session);
        return tokio::task::spawn_blocking(move || -> crate::Result<String> {
            let mut session = session.blocking_lock();
            session.append_custom_message_entry(
                &custom.custom_type,
                custom.content,
                custom.display,
                custom.details,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("session startup blocking task failed: {error}"))?
        .map(Some);
    };
    let session = Arc::clone(session);
    let expected_session_id = expected_session_id.to_owned();
    tokio::task::spawn_blocking(move || -> crate::Result<Option<String>> {
        let mut session = session.blocking_lock();
        if session.id() != expected_session_id {
            return Ok(None);
        }
        session
            .append_custom_message_entry(
                &custom.custom_type,
                custom.content,
                custom.display,
                custom.details,
            )
            .map(Some)
    })
    .await
    .map_err(|error| anyhow::anyhow!("session startup blocking task failed: {error}"))?
}

/// Batch form of [`append_message_if_session`] for the post-run persistence
/// batch. Every message is appended under ONE storage lock acquisition, and
/// the identity check runs in that same acquisition: a storage swap either
/// happens before the check (nothing is written to the replacement) or after
/// the whole batch committed to the run's own transcript.
pub(crate) async fn append_messages_if_session(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    expected_session_id: Option<&str>,
    messages: Vec<Message>,
) -> crate::Result<Option<Vec<String>>> {
    if messages.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let Some(expected_session_id) = expected_session_id else {
        return SessionManager::append_messages_off_thread(session, messages)
            .await
            .map(Some);
    };
    let session = Arc::clone(session);
    let expected_session_id = expected_session_id.to_owned();
    tokio::task::spawn_blocking(move || -> crate::Result<Option<Vec<String>>> {
        let mut session = session.blocking_lock();
        if session.id() != expected_session_id {
            return Ok(None);
        }
        let mut ids = Vec::with_capacity(messages.len());
        for message in messages {
            ids.push(session.append_message(message)?);
        }
        Ok(Some(ids))
    })
    .await
    .map_err(|error| anyhow::anyhow!("session append blocking task failed: {error}"))?
}

/// Named inputs for [`append_compaction_if_session`].
pub(crate) struct CompactionAppend<'a> {
    pub(crate) session: &'a Arc<tokio::sync::Mutex<SessionManager>>,
    pub(crate) expected_session_id: Option<&'a str>,
    pub(crate) summary: String,
    pub(crate) first_kept_entry_id: Option<String>,
    pub(crate) tokens_before: u64,
    pub(crate) details: Option<JsonValue>,
    pub(crate) usage: Option<crate::ai::types::Usage>,
    pub(crate) from_hook: bool,
}

/// Compaction form of [`append_message_if_session`]: the overflow-recovery
/// compaction may only append while the slot still names the transcript the
/// run started on. A swap during summarization must not write the old run's
/// cut (`firstKeptEntryId` from the outgoing transcript) into the replacement
/// session.
pub(crate) async fn append_compaction_if_session(
    input: CompactionAppend<'_>,
) -> crate::Result<Option<String>> {
    let CompactionAppend {
        session,
        expected_session_id,
        summary,
        first_kept_entry_id,
        tokens_before,
        details,
        usage,
        from_hook,
    } = input;
    let Some(expected_session_id) = expected_session_id else {
        return SessionManager::append_compaction_off_thread(
            session,
            crate::session_manager::CompactionAppendArgs {
                summary,
                first_kept_entry_id,
                tokens_before,
                details,
                usage,
                from_hook,
            },
        )
        .await
        .map(Some);
    };
    let session = Arc::clone(session);
    let expected_session_id = expected_session_id.to_owned();
    tokio::task::spawn_blocking(move || -> crate::Result<Option<String>> {
        let mut session = session.blocking_lock();
        if session.id() != expected_session_id {
            return Ok(None);
        }
        session
            .append_compaction_with_usage(
                summary,
                first_kept_entry_id.as_deref(),
                tokens_before,
                details,
                usage,
                from_hook,
            )
            .map(Some)
    })
    .await
    .map_err(|error| anyhow::anyhow!("session compaction append blocking task failed: {error}"))?
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
pub(crate) async fn dispatch_turn_end_boundary(
    plugins: &PluginHost,
    session: Option<BoundarySession<'_>>,
    turn_index: usize,
    context: &FinishTurnContext,
    pending_messages: bool,
    run_session_id: Option<&str>,
) -> bool {
    // No handler: the emit below would be a no-op, but building the payload
    // serializes the ENTIRE current transcript into a `Value` tree first.
    // Keep the "no handler, no tree" contract structural instead of relying
    // on every caller's gate (the prompt loop and print both gate today).
    if !plugins.has_handlers("turn_end") {
        return false;
    }
    let can_continue = context_can_continue(&context.context) || pending_messages;
    // Upstream `TurnEndEvent.messageEntryId` / `toolResultEntryIds`: the
    // transcript entries the per-message sink persisted for this turn,
    // resolved newest-first from the session branch. A message the sink
    // intentionally omitted (overflow recovery) has no entry, so the
    // assistant id stays OMITTED from the wire (never a fabricated/null id)
    // and unmatched tool results simply do not contribute an id, exactly like
    // upstream's `flatMap` over resolvable ids. Store-less sessions have no
    // ids at all.
    let (message_entry_id, tool_result_entry_ids) = match &session {
        Some(session) => {
            // ONE branch-path walk for the whole turn: the per-message lookups
            // rebuilt the session-wide path index once per tool result.
            session
                .find_persisted_turn_entry_ids(&context.message, &context.tool_results)
                .await
        }
        None => (None, Vec::new()),
    };
    // Build the payload object FIELD BY FIELD and MOVE every already-built
    // `Value` into it. `json!` would re-serialize each `Value` expression
    // (`serde_json::to_value(&value)`), deep-copying the whole `toolResults`
    // array and the entire `contextMessages` transcript a second time on
    // every turn. Insertion order is unchanged, and `serde_json` here uses
    // `preserve_order`, so plugins observe the same field order as before.
    let mut payload = serde_json::Map::with_capacity(10);
    payload.insert("type".to_owned(), JsonValue::String("turn_end".to_owned()));
    payload.insert("turnIndex".to_owned(), serde_json::json!(turn_index));
    // Serialize the payload structs directly: `Message::Assistant` /
    // `Message::ToolResult` serialize as the payload itself, so wrapping a
    // CLONE in a `Message` only copied the whole turn's output first.
    payload.insert(
        "message".to_owned(),
        serde_json::to_value(&context.message).unwrap_or(JsonValue::Null),
    );
    payload.insert(
        "toolResults".to_owned(),
        JsonValue::Array(
            context
                .tool_results
                .iter()
                .map(|result| serde_json::to_value(result).unwrap_or(JsonValue::Null))
                .collect(),
        ),
    );
    payload.insert(
        "toolResultEntryIds".to_owned(),
        JsonValue::Array(
            tool_result_entry_ids
                .into_iter()
                .map(JsonValue::String)
                .collect(),
        ),
    );
    payload.insert("entries".to_owned(), JsonValue::Array(Vec::new()));
    payload.insert("continue".to_owned(), JsonValue::Bool(false));
    payload.insert(
        "outcome".to_owned(),
        JsonValue::String(outcome_label(&context.message).to_owned()),
    );
    // `contextMessages` is the full transcript view the plugin contract
    // requires: serialize it exactly once, then move the array into the
    // payload. No second copy is made on the way in.
    let mut context_payload = serde_json::Map::with_capacity(2);
    context_payload.insert(
        "contextMessages".to_owned(),
        JsonValue::Array(
            context
                .context
                .messages
                .iter()
                .map(message_json)
                .collect::<Vec<_>>(),
        ),
    );
    context_payload.insert("canContinue".to_owned(), JsonValue::Bool(can_continue));
    payload.insert("context".to_owned(), JsonValue::Object(context_payload));
    let mut payload = JsonValue::Object(payload);
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
    if let Some(session) = &session {
        session.commit_drafts(run_session_id, &drafts).await;
    }
    if continue_requested && !can_continue {
        crate::ext_diag!("[ext] turn_end requested continuation without runnable model context");
        return false;
    }
    continue_requested
}

/// Shared `agent_before_settle` boundary dispatch (AgentSession + print).
pub(crate) async fn dispatch_agent_before_settle_boundary(
    plugins: &PluginHost,
    session: Option<BoundarySession<'_>>,
    context: &Context,
    terminal: &AssistantMessage,
    pending_messages: bool,
    run_session_id: Option<&str>,
) -> bool {
    // See `dispatch_turn_end_boundary`: a handler-less boundary must not
    // serialize the whole context before discovering there is nobody to
    // receive it.
    if !plugins.has_handlers("agent_before_settle") {
        return false;
    }
    // Upstream canContinue for the pre-settle boundary also accepts an
    // assistant-final context when messages are queued (the re-run picks
    // them up).
    let can_continue = context_can_continue(context) || pending_messages;
    // Build by moves: `json!` re-serializes Vec<Value>/Value expressions,
    // which doubled the whole transcript on every settle boundary.
    let mut context_payload = serde_json::Map::with_capacity(2);
    context_payload.insert(
        "contextMessages".to_owned(),
        JsonValue::Array(context.messages.iter().map(message_json).collect()),
    );
    context_payload.insert("canContinue".to_owned(), JsonValue::Bool(can_continue));
    let mut payload_map = serde_json::Map::with_capacity(5);
    payload_map.insert(
        "type".to_owned(),
        JsonValue::String("agent_before_settle".to_owned()),
    );
    payload_map.insert(
        "outcome".to_owned(),
        JsonValue::String(outcome_label(terminal).to_owned()),
    );
    payload_map.insert("entries".to_owned(), JsonValue::Array(Vec::new()));
    payload_map.insert("continue".to_owned(), JsonValue::Bool(false));
    payload_map.insert("context".to_owned(), JsonValue::Object(context_payload));
    let mut payload = JsonValue::Object(payload_map);
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
    if let Some(session) = &session {
        session.commit_drafts(run_session_id, &drafts).await;
    }
    if continue_requested && !can_continue {
        crate::ext_diag!(
            "[ext] agent_before_settle requested continuation without runnable model context"
        );
        return false;
    }
    continue_requested
}

/// Emit the `session_compact` success event through the ordered plugin host
/// (the shared shape of `agent_session/compact.rs`'s manual and threshold
/// paths). `compaction` is the wire result; `compaction_entry` is the
/// persisted entry when storage assigned one.
///
/// The sole caller is the print host's compaction paths, so a no-print build
/// has no caller for it.
#[cfg(feature = "print")]
pub(crate) async fn emit_session_compact_success(
    plugins: Option<&PluginHost>,
    reason: &str,
    compaction: JsonValue,
    compaction_entry: Option<JsonValue>,
    from_extension: bool,
) {
    let Some(plugins) = plugins else {
        return;
    };
    // Move the already-built `compaction` value: `json!` would serialize it
    // into the literal a second time.
    let mut payload_map = serde_json::Map::with_capacity(6);
    payload_map.insert(
        "type".to_owned(),
        JsonValue::String("session_compact".to_owned()),
    );
    payload_map.insert("reason".to_owned(), JsonValue::String(reason.to_owned()));
    payload_map.insert("willRetry".to_owned(), JsonValue::Bool(false));
    payload_map.insert("fromExtension".to_owned(), JsonValue::Bool(from_extension));
    payload_map.insert("compaction".to_owned(), compaction);
    let mut payload = JsonValue::Object(payload_map);
    if let Some(entry) = compaction_entry {
        payload["compactionEntry"] = entry;
    }
    if let Err(error) = plugins.emit("session_compact", &mut payload).await {
        crate::ext_diag!("[ext] session_compact dispatch failed: {error}");
    }
}

/// Emit `session_compact_failed` to both faces exactly once (the unified host
/// owns the JS adapter; a host-less print caller falls back to its bare
/// runtime handle).
#[cfg(feature = "print")]
pub(crate) async fn emit_session_compact_failed(
    plugins: Option<&PluginHost>,
    runtime: Option<&crate::extensions::ExtensionHandle>,
    reason: &str,
    error_message: Option<&str>,
    aborted: bool,
    will_retry: bool,
    from_extension: bool,
) {
    let mut payload = serde_json::json!({
        "type": "session_compact_failed",
        "reason": reason,
        "aborted": aborted,
        "willRetry": will_retry,
        "fromExtension": from_extension,
    });
    if let Some(message) = error_message {
        payload["errorMessage"] = serde_json::json!(message);
    }
    match plugins {
        Some(host) => {
            if host.has_handlers("session_compact_failed") {
                if let Err(error) = host.emit("session_compact_failed", &mut payload).await {
                    crate::ext_diag!("[ext] session_compact_failed dispatch failed: {error}");
                }
            }
        }
        None => {
            if let Some(runtime) = runtime {
                if runtime.has_handlers("session_compact_failed") {
                    if let Err(error) = runtime.emit("session_compact_failed", &mut payload).await {
                        crate::ext_diag!("[ext] session_compact_failed dispatch failed: {error}");
                    }
                }
            }
        }
    }
}

/// Commit boundary entry drafts to session storage (upstream
/// `_applyBoundaryDrafts`): `custom`, `custom_message`, `context_edit`, and
/// `compaction` drafts are supported.
///
/// `run_session_id` is the identity bound at run start: a draft produced by a
/// run whose storage was swapped away must not enter the replacement session.
/// Every draft is checked against the manager under the same lock acquisition
/// as its append (the compaction draft is re-checked inside the blocking
/// append because its async lock is released across the summarizer-style
/// blocking write).
async fn commit_boundary_drafts(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    run_session_id: Option<&str>,
    drafts: &JsonValue,
) {
    let Some(draft_list) = drafts.as_array() else {
        return;
    };
    if draft_list.is_empty() {
        return;
    }
    let mut guard = session.lock().await;
    if let Some(expected) = run_session_id {
        if guard.id() != expected {
            tracing::debug!(
                expected_session_id = expected,
                "dropped boundary drafts after the session storage was swapped"
            );
            return;
        }
    }
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
            let result = append_compaction_if_session(CompactionAppend {
                session,
                expected_session_id: run_session_id,
                summary: summary.to_owned(),
                first_kept_entry_id: draft
                    .get("firstKeptEntryId")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned),
                tokens_before: draft
                    .get("tokensBefore")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(0),
                details: draft.get("details").cloned().filter(|d| !d.is_null()),
                usage: None,
                // Extension boundary draft: from_hook = true (upstream
                // gates file-op carry-over on `!fromHook`).
                from_hook: true,
            })
            .await;
            match result {
                Ok(None) => {
                    tracing::debug!(
                        expected_session_id = run_session_id.unwrap_or("<unknown>"),
                        "dropped a boundary compaction draft after the session storage was \
                         swapped"
                    );
                    return;
                }
                Err(error) => {
                    crate::ext_diag!("[ext] failed to append boundary draft: {error}");
                }
                Ok(Some(_)) => {}
            }
            guard = session.lock().await;
            // The blocking append released the async lock: a swap may have
            // landed in that window, in which case the remaining drafts must
            // not be written to the replacement either.
            if let Some(expected) = run_session_id {
                if guard.id() != expected {
                    tracing::debug!(
                        expected_session_id = expected,
                        "dropped the remaining boundary drafts after the session storage was \
                         swapped"
                    );
                    return;
                }
            }
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
