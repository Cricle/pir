use super::*;

/// Per-run threshold gate shared by the prompt-start check and the loop's
/// first per-turn check. `attempted` records that a real compaction run
/// started (not merely that the threshold hook was called); the prompt loop
/// then arms `skip_next` so a failed persistence append is not retried before
/// the first request.
#[derive(Default)]
pub(super) struct ThresholdRunGate {
    pub(super) attempted: std::sync::atomic::AtomicBool,
    pub(super) skip_next: std::sync::atomic::AtomicBool,
}

impl AgentSession {
    // ------------------------------------------------------------------
    // Compaction / retry toggles (upstream setters)
    // ------------------------------------------------------------------

    /// Whether automatic compaction is currently enabled (upstream
    /// `autoCompactionEnabled`).
    pub fn auto_compaction_enabled(&self) -> bool {
        self.compaction_config().is_some()
    }

    /// Enable or disable automatic compaction (upstream
    /// `setAutoCompactionEnabled`). Enabling keeps the configured thresholds;
    /// disabling removes the run's compaction hook.
    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        let mut config = self
            .compaction
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if enabled {
            if config.is_none() {
                *config = Some(CompactionConfig::default());
            }
        } else {
            *config = None;
        }
    }
    // ------------------------------------------------------------------
    // Manual compaction
    // ------------------------------------------------------------------

    /// Compact the conversation now (RPC `compact` / manual triggers).
    ///
    /// On success the compaction entry is appended to session storage (cut
    /// point persisted as the upstream `firstKeptEntryId`, `None` mapping to
    /// a self-retaining entry) and the ephemeral transcript (if any) is
    /// replaced by the rebuilt message list.  Returns
    /// [`Err`](anyhow::Error) when nothing can be compacted or the
    /// summarizer call fails.
    ///
    /// Session-event parity (upstream manual `compact`,
    /// agent-session.ts:2767/2878/2891): `compaction_start` fires before
    /// anything can fail and `compaction_end` fires on every exit path, with
    /// the wire `CompactionResult` on success and `errorMessage`
    /// (`Compaction failed: …`) on failure; `isCompacting` covers the run.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> crate::Result<CompactionResult> {
        self.compact_with_estimated_tokens(custom_instructions)
            .await
            .map(|(result, _)| result)
    }

    /// Cancel an in-progress compaction (upstream `abortCompaction`,
    /// agent-session.ts:2978) — manual AND automatic.
    ///
    /// - MANUAL: the claim is [`SESSION_COMPACTING`]; the fresh compaction
    ///   signal installed by [`Self::compact`] is aborted, exactly as before.
    /// - AUTOMATIC (threshold): the armed per-trigger signal is fired. The
    ///   RUN's signal is deliberately left alone, so the turn continues to its
    ///   model request and later turns are unaffected; the armed slot is
    ///   cleared as the compaction exits and every future trigger arms a fresh
    ///   signal.
    ///
    /// Returns whether a compaction was running and the cancel reached it.
    pub fn abort_compaction(&self) -> bool {
        if self.session_state() == SESSION_COMPACTING {
            self.signal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .abort();
            return true;
        }
        // Threshold compaction runs INSIDE the prompt, so the session claim is
        // SESSION_RUNNING; the armed slot is the only signal that belongs to
        // it. Taking it makes the guard's clear a no-op and guarantees a
        // subsequent `abort_compaction` cannot fire the same (ended) signal.
        let armed = self
            .auto_compaction_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match armed {
            Some((_, signal)) => {
                signal.abort();
                true
            }
            None => false,
        }
    }

    /// [`Self::compact`] plus the post-compaction context estimate (upstream
    /// `CompactionResult.estimatedTokensAfter`), so the RPC `compact` response
    /// can carry the full upstream shape.
    pub async fn compact_with_estimated_tokens(
        &self,
        custom_instructions: Option<&str>,
    ) -> crate::Result<(CompactionResult, u64)> {
        // Upstream `compact()` opens with `await this.abort()`: a manual
        // `/compact` issued while the model is streaming STOPS the response
        // and then compacts. The claim below then waits for the settling run
        // instead of rejecting it.
        if self.session_state() == SESSION_RUNNING {
            self.abort();
        }
        // Claim the session for the manual compaction. A genuinely streaming
        // run is rejected with the honest error; a run that is already on its
        // way out — aborted, or past its terminal loop result — is WAITED for,
        // because it settles without further user action. The claim is also the
        // mutual exclusion with `start_run`, so no prompt can start while the
        // compaction runs (upstream: "Cannot submit a prompt while compaction
        // is in progress…").
        let claim = self.claim_manual_compaction().await?;
        // The manual compaction owns the session now: install a fresh
        // compaction signal before anything can await, so `abort()` (which
        // aborts the signal the session currently holds) reaches the
        // summarizer request instead of dying on the previous run's signal.
        // An abort that landed between the claim CAS and this install is
        // recorded by `abort()`'s post-abort state read (the state is
        // SESSION_COMPACTING); honor it by aborting the fresh signal too.
        let compaction_signal = AbortSignal::new();
        *self
            .signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = compaction_signal.clone();
        if self.run_abort_requested.swap(false, Ordering::SeqCst) {
            compaction_signal.abort();
        }
        let mut run = CompactionRunGuard::start(
            Some(Arc::clone(&self.session_events)),
            Some(Arc::clone(&self.compacting)),
            "manual",
        );
        let retry_observer = self.summarization_retry_observer(
            super::events::SummarizationRetrySource::Compaction { reason: "manual" },
        );
        match compaction::with_summarization_retry(
            retry_observer,
            self.compact_inner(custom_instructions, compaction_signal.clone()),
        )
        .await
        {
            Ok((result, estimated_tokens_after)) => {
                // Upstream clears the manual compaction state BEFORE the
                // terminal event, so end listeners observe idle state.
                run.clear_now();
                claim.release();
                self.emit_session_event(SessionEvent::CompactionEnd {
                    reason: "manual",
                    result: Some(compaction_result_wire(&result, estimated_tokens_after)),
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                run.end_emitted();
                Ok((result, estimated_tokens_after))
            }
            Err(failure) => {
                // Upstream `compact` catch order: `compaction_end` first, then
                // the extension `session_compact_failed` (agent-session.ts:
                // 2948-2968). An abort requested during the summarization is
                // the normal aborted outcome, not a failed compaction: the end
                // event carries `aborted` and no errorMessage.
                run.clear_now();
                claim.release();
                self.emit_session_event(SessionEvent::CompactionEnd {
                    reason: "manual",
                    result: None,
                    aborted: failure.aborted,
                    will_retry: false,
                    error_message: failure.error_message.clone(),
                });
                self.emit_compact_failed(
                    "manual",
                    failure.error_message,
                    failure.aborted,
                    failure.from_extension,
                )
                .await;
                run.end_emitted();
                Err(failure.error)
            }
        }
    }

    /// Try once to claim the session for a manual compaction
    /// ([`SESSION_IDLE`] → [`SESSION_COMPACTING`]). The compare-exchange is
    /// the mutual-exclusion point with `start_run`'s own claim: from idle,
    /// exactly one of the two wins.
    fn try_claim_compaction(&self) -> bool {
        self.session_state
            .compare_exchange(
                SESSION_IDLE,
                SESSION_COMPACTING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Claim the session for a manual compaction. A streaming run is aborted
    /// first (upstream `compact()` opens with `await this.abort()`) and then
    /// waited for; a concurrent manual compaction is rejected with its own
    /// honest error.
    async fn claim_manual_compaction(&self) -> crate::Result<ManualCompactionClaim> {
        loop {
            if self.try_claim_compaction() {
                return Ok(ManualCompactionClaim {
                    state: Arc::clone(&self.session_state),
                    run_abort_requested: Arc::clone(&self.run_abort_requested),
                    idle_notify: Arc::clone(&self.idle_notify),
                });
            }
            match self.session_state() {
                SESSION_RUNNING => {
                    // A run claimed the session between `compact()`'s state
                    // read and this CAS; request the abort here too so the
                    // claim cannot spin on a run that never observes it.
                    if !self.run_abort_requested.load(Ordering::SeqCst) {
                        self.abort();
                    }
                    // The run is on its way out: wait for its claim to
                    // release. No new run can start meanwhile — the claim is
                    // still held — and re-claiming below re-checks the state
                    // instead of assuming it.
                    self.wait_until_idle().await;
                }
                SESSION_COMPACTING => {
                    anyhow::bail!("cannot compact while a compaction is in progress");
                }
                // A concurrent claimant won the session from idle between the
                // failed CAS and this read; re-evaluate.
                _ => continue,
            }
        }
    }

    /// Wait until the session claim returns to idle. The waiter registers a
    /// `Notify` interest BEFORE re-checking the claim (`enable`), so a
    /// release between the check and the await can never be missed — no
    /// busy-wait and no sleep.
    async fn wait_until_idle(&self) {
        loop {
            let notified = self.idle_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.session_state() == SESSION_IDLE {
                return;
            }
            notified.await;
        }
    }

    /// The manual compaction body ([`Self::compact`] wraps it with the
    /// `compaction_start`/`compaction_end` session events). Returns the
    /// compaction result plus the estimated token count of the rebuilt
    /// context (upstream `estimatedTokensAfter`).
    async fn compact_inner(
        &self,
        custom_instructions: Option<&str>,
        compaction_signal: AbortSignal,
    ) -> Result<(CompactionResult, u64), CompactFailure> {
        // Borrowed cut basis: session-backed compaction runs over the
        // projection's per-entry METADATA zipped with its own message list
        // (one materialization of the conversation, no `CutEntry` copy, no
        // branch copy); ephemeral compaction treats every in-memory message as
        // its own entry. The `refs` below borrow both the metadata and the
        // context's messages for the whole body.
        let mut metas: Vec<compaction::ProjectionEntryMeta> = Vec::new();
        let mut branch: Vec<compaction::PathEntryMeta> = Vec::new();
        let context = match &self.session {
            Some(session) => {
                // A manual compaction summarizes the WHOLE transcript. The
                // widen + projection + branch walk are blocking work over a
                // transcript-sized file, so they run on the blocking pool under
                // one `blocking_lock` — the async session lock is never held
                // across them and a runtime worker is not parked on the walk.
                let (projection, branch_path) =
                    super::prompt_loop::project_session_off_thread(session).await?;
                branch = branch_path;
                metas = compaction::ProjectionEntryMeta::from_projection(&projection.entries);
                Context {
                    system_prompt: self.system_prompt_snapshot(),
                    messages: projection.messages,
                    tools: None,
                }
            }
            None => Context {
                system_prompt: self.system_prompt_snapshot(),
                messages: history_owned(self.history_snapshot()),
                tools: None,
            },
        };
        let refs: Vec<compaction::CutEntryRef<'_>> = if metas.is_empty() {
            context
                .messages
                .iter()
                .map(compaction::CutEntryRef::from_message)
                .collect()
        } else {
            compaction::ProjectionEntryMeta::cut_refs(&metas, &context.messages)
        };
        let model = self.current_model();
        let config = self.compaction_config().unwrap_or_default();
        let preparation = match compaction::prepare_compaction_refs(&refs, &branch, &config) {
            Some(preparation) => preparation,
            None => {
                // Upstream distinguishes the two undefined-preparation causes
                // (agent-session.ts `compact`): the newest branch entry already
                // being a compaction is "Already compacted", anything else —
                // including the keep-recent window covering the whole
                // conversation of a small session — is "Nothing to compact
                // (session too small)". Both surface as the caught error's
                // `Compaction failed: …` text on the failed event.
                if branch.last().is_some_and(|entry| entry.is_compaction) {
                    return Err(CompactFailure::new(
                        anyhow::anyhow!("already compacted"),
                        Some("Compaction failed: already compacted".to_owned()),
                        false,
                        false,
                    ));
                }
                return Err(CompactFailure::new(
                    anyhow::anyhow!("nothing to compact (session too small)"),
                    Some("Compaction failed: nothing to compact (session too small)".to_owned()),
                    false,
                    false,
                ));
            }
        };
        // `session_before_compact` (reason: manual): a handler may cancel or
        // REPLACE the compaction (upstream `SessionBeforeCompactResult`).
        let mut extension_compaction: Option<CompactionResult> = None;
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("session_before_compact") {
                // The full branch view enters the payload only when a handler
                // exists (upstream `branchEntries`); the newest compaction on
                // it carries the previous summary's persisted file-operation
                // details for the `fileOps` payload (upstream
                // `extractFileOperations`).
                let branch_entries = match &self.session {
                    Some(session) => session
                        .lock()
                        .await
                        .branch_entries(None)
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                let mut payload = serde_json::json!({
                    "type": "session_before_compact",
                    "customInstructions": custom_instructions,
                    "reason": "manual",
                    "willRetry": false,
                    "preparation": borrowed_preparation_summary(
                        &preparation,
                        previous_compaction_details(&branch_entries),
                    ),
                    "branchEntries": serde_json::to_value(&branch_entries)
                        .unwrap_or(JsonValue::Null),
                    "signal": { "aborted": compaction_signal.is_aborted() },
                });
                match plugins.emit("session_before_compact", &mut payload).await {
                    Ok(outcome) => {
                        if outcome.result.get("cancel").and_then(JsonValue::as_bool) == Some(true) {
                            // Upstream: an extension-cancelled compaction is
                            // an ABORT, not a failure — the failed event
                            // carries no errorMessage. `fromExtension` stays
                            // false (upstream sets it only for a REPLACEMENT
                            // compaction, agent-session.ts:2804-2810). The
                            // failed event fires from the wrapper AFTER the
                            // terminal `compaction_end` (upstream catch order).
                            return Err(CompactFailure::new(
                                anyhow::anyhow!("compaction cancelled by an extension"),
                                None,
                                true,
                                false,
                            ));
                        }
                        extension_compaction =
                            parse_borrowed_extension_compaction(&outcome.result, &preparation);
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] session_before_compact dispatch failed: {error}")
                    }
                }
            }
        }
        let result = match extension_compaction {
            Some(result) => result,
            None => match compaction::compact_with_preparation_refs(
                preparation,
                &model,
                self.complete_fn_for_signal(&model, compaction_signal.clone()),
                custom_instructions,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    let aborted = compaction_signal.is_aborted();
                    let error_message = if aborted {
                        None
                    } else {
                        Some(format!("Compaction failed: {error}"))
                    };
                    // Emitted by the wrapper after `compaction_end`
                    // (upstream catch order).
                    return Err(CompactFailure::new(
                        anyhow::Error::msg(error),
                        error_message,
                        aborted,
                        false,
                    ));
                }
            },
        };
        let appended_entry_id = match &self.session {
            Some(session) => Some(
                SessionManager::append_compaction_off_thread(
                    session,
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    Some(compaction_details(&result)),
                    Some(result.usage),
                    false,
                )
                .await?,
            ),
            None => None,
        };
        let rebuilt = compaction::rebuilt_context(&context, &result);
        // Measure before moving the messages out: the rebuilt context is not
        // returned on this path, so its message vec becomes the new history
        // allocation instead of a full-transcript clone. Only an ephemeral
        // session has an in-memory history to replace; a store-backed session
        // re-reads its projection from the compaction entry and must not keep
        // a second transcript-sized copy alive here.
        let estimated_tokens_after = compaction::estimate_context_tokens(&rebuilt.messages).tokens;
        if self.session.is_none() {
            *self
                .history
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(rebuilt.messages);
        }
        if let Some(plugins) = &self.plugins {
            // Upstream `SessionCompactEvent.compactionEntry` is the persisted
            // `CompactionEntry` (session-manager shape: type/id/parentId/
            // timestamp/firstKeptEntryId/tokensBefore/details/usage). It rides
            // ALONGSIDE the historical `compaction` result field, which the JS
            // `ctx.compact({ onComplete })` settler reads (`payload.compaction
            // ?? payload.result`) and which existing consumers still use.
            // Without persisted storage there is no entry to describe, so the
            // field is omitted rather than fabricated.
            let mut payload = serde_json::json!({
                "type": "session_compact",
                "reason": "manual",
                "willRetry": false,
                "fromExtension": false,
                "compaction": compaction_result_wire(&result, estimated_tokens_after),
            });
            if let Some(entry) =
                compaction_entry_wire(self.session.as_ref(), appended_entry_id.as_deref()).await
            {
                payload["compactionEntry"] = entry;
            }
            emit_plugin_event(plugins, "session_compact", payload).await;
        }
        Ok((result, estimated_tokens_after))
    }
    /// The live automatic-compaction config (cloned), or `None` when the
    /// session has compaction disabled entirely.
    pub(super) fn compaction_config(&self) -> Option<CompactionConfig> {
        self.compaction
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    /// Build the loop compaction hook.  When the trigger fires, the cut point
    /// is computed over the projected entries (the prompt-start projection
    /// snapshot plus the run's newer, not-yet-persisted messages), the
    /// summarizer call(s) run (history + turn prefix for split turns), and on
    /// success the compaction entry is appended to session storage with the
    /// cut point persisted as the upstream `firstKeptEntryId`. The extension
    /// `session_before_compact` / `session_compact` /
    /// `session_compact_failed` events (reason: `threshold`) fire around it.
    ///
    /// Trigger and cut-list basis (upstream agent-session.ts:766-774 +
    /// 2973-2981 + 3053-3058): the threshold is checked against the PROJECTED
    /// context (`estimate_projected_context_tokens`) whenever a projection and
    /// branch are available, so stale pre-compaction usage and context edits
    /// are respected. After a successful compaction the prompt-start snapshot
    /// is invalidated: later turns of the run rebuild the projection from the
    /// SESSION (whose leaf path now carries the compaction boundary), so the
    /// post-compaction context is the basis and the pre-compaction span is
    /// never re-summarized.
    ///
    /// `threshold_model` overrides the model the threshold is checked against
    /// (a virtual selection checks the ROUTED model's context window).
    pub(super) fn compaction_hook(
        &self,
        shared: Arc<RwLock<Option<CompactionConfig>>>,
        projected: Option<ProjectionSnapshot>,
        threshold_model: Option<Model>,
        // Per-run threshold gate shared by the prompt-start check and the
        // loop's first per-turn check. `attempted` is set only when the gate
        // actually starts a compaction; the prompt loop then arms
        // `skip_next` so a failed append is not retried before the first
        // request (the "fail once, continue unchanged, retry next turn"
        // contract).
        gate: Option<Arc<ThresholdRunGate>>,
    ) -> CompactionFn {
        let model = threshold_model.unwrap_or_else(|| self.current_model());
        // The summarizer completes against the same (routed) model the
        // threshold was checked against, and carries the RUN's abort signal:
        // a threshold compaction happens inside the prompt run, so Esc must
        // reach the in-flight summarizer request, not only the loop between
        // requests.
        let signal = self
            .signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let complete = self.complete_fn_for_signal(&model, signal.clone());
        let session = self.session.clone();
        let history = self.history.clone();
        let plugins = self.plugins.clone();
        // Session-event seam: the threshold run emits `compaction_start` /
        // `compaction_end` (upstream reason: "threshold") and covers
        // `isCompacting`.
        let session_events = self.session_events.clone();
        let compacting = self.compacting.clone();
        // Flipped after a successful compaction: the prompt-start snapshot no
        // longer matches the rebuilt context, so later turns re-read the
        // session projection (upstream compacts against the live session).
        let snapshot_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The per-session slot `abort_compaction()` fires for a THRESHOLD
        // compaction; armed fresh at every trigger (see the body).
        let auto_cancel = Arc::clone(&self.auto_compaction_cancel);
        Box::new(move |context| {
            // Read the config before constructing the large compaction future.
            // Disabled compaction is the common case (and every run without a
            // session-level config): building/polling that future anyway would
            // make each turn pay for it. The read is still per request, so a
            // `set_auto_compaction` flip applies on the next threshold check.
            let config = match shared
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                Some(config) if config.enabled => config,
                Some(_) | None => {
                    // Match the old order: a per-run skip flag was consumed by
                    // the threshold check even when compaction was disabled.
                    if let Some(gate) = &gate {
                        gate.skip_next
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    return Box::pin(std::future::ready(context));
                }
            };
            let model = model.clone();
            let complete = Arc::clone(&complete);
            let signal = signal.clone();
            let session = session.clone();
            let history = history.clone();
            let plugins = plugins.clone();
            let projected = projected.clone();
            let snapshot_stale = Arc::clone(&snapshot_stale);
            let session_events = Arc::clone(&session_events);
            let compacting = Arc::clone(&compacting);
            let auto_cancel = Arc::clone(&auto_cancel);
            let gate = gate.clone();
            Box::pin(async move {
                // Threshold basis: the prompt-start projection snapshot
                // plus the run's newer (not yet projected) messages while the
                // snapshot is fresh; after a compaction (or when no snapshot
                // exists) the live session's per-entry metadata is the basis,
                // falling back to the raw context for ephemeral runs.
                //
                // MEMORY SHAPE (pir extension): the basis is METADATA ONLY
                // (source `Arc<FileEntry>` + contributed message count) paired
                // with slices of the live context the hook was handed. The
                // trigger check, cut-point selection and summarization read
                // the conversation exactly once — the copy of the whole
                // projection (`build_session_projection` + one `CutEntry` per
                // entry) and of the branch this hook used to build on every
                // threshold-triggering request is gone, as is the
                // `materialize_projected_tail` copy of the not-yet-projected
                // tail.
                let snapshot_fresh = projected.is_some()
                    && !snapshot_stale.load(std::sync::atomic::Ordering::SeqCst);
                let mut owned_metas: Vec<compaction::ProjectionEntryMeta> = Vec::new();
                let mut owned_branch: Vec<compaction::PathEntryMeta> = Vec::new();
                if !snapshot_fresh {
                    if let Some(session) = session.as_ref() {
                        let session = session.lock().await;
                        owned_metas = session.projection_entry_meta();
                        owned_branch = session.branch_path_meta(None);
                    }
                }
                let (metas, branch): (
                    &[compaction::ProjectionEntryMeta],
                    &[compaction::PathEntryMeta],
                ) = match (&projected, snapshot_fresh) {
                    (Some((metas, branch)), true) => (metas.as_slice(), branch.as_slice()),
                    _ => (owned_metas.as_slice(), owned_branch.as_slice()),
                };
                // The borrowed cut basis: entry metadata zipped with the live
                // context's own message slices; a metadata-less basis (no
                // session storage) treats every message as its own entry.
                let refs: Vec<compaction::CutEntryRef<'_>> = if metas.is_empty() {
                    context
                        .messages
                        .iter()
                        .map(compaction::CutEntryRef::from_message)
                        .collect()
                } else {
                    compaction::ProjectionEntryMeta::cut_refs(metas, &context.messages)
                };
                // Trigger (upstream `_checkCompaction`): projected tokens when
                // a projection/branch is available — this skips usage that
                // predates the latest compaction/context-edit boundary and
                // estimates by characters when the projection contains edits
                // — else the plain message-list estimate.
                let plain_estimate =
                    branch.is_empty() && refs.iter().all(|entry| !entry.is_compaction());
                let context_tokens = if plain_estimate {
                    compaction::estimate_context_tokens(&context.messages).tokens
                } else {
                    compaction::estimate_projected_context_tokens(refs.as_slice(), branch).tokens
                };
                if gate.as_ref().is_some_and(|gate| {
                    gate.skip_next
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                }) {
                    return context;
                }
                if !compaction::should_compact_tokens(context_tokens, model.context_window, &config)
                {
                    return context;
                }
                // The threshold is met: plan the cut over the BORROWED basis
                // (no clone of the projection, the branch, the summarized span,
                // or the kept tail).
                let prepared = match compaction::prepare_compaction_refs(&refs, branch, &config) {
                    Some(prepared) => prepared,
                    None => return context,
                };
                if let Some(gate) = &gate {
                    gate.attempted
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                // Arm THIS trigger's own cancellation. A fresh signal per
                // threshold compaction means `abort_compaction()` cancels only
                // this summarizer call: the run signal is untouched (the turn
                // continues to its model request) and a later turn arms a new
                // signal. The guard clears the slot on every exit path.
                let (cancel_generation, auto_signal) = arm_auto_compaction_cancel(&auto_cancel);
                let _cancel_guard =
                    AutoCompactionCancelGuard::new(Arc::clone(&auto_cancel), cancel_generation);
                // Both the run's signal (Esc/abort) and the automatic
                // compaction's own signal make this compaction "aborted".
                let compaction_aborted = || signal.is_aborted() || auto_signal.is_aborted();
                if compaction_aborted() {
                    // Cancelled between the trigger check and the run start:
                    // no compaction_start/end pair is owed (the run never
                    // began), and the caller's turn continues unchanged.
                    return context;
                }
                // Threshold run bookkeeping (upstream agent-session.ts:3119:
                // `compaction_start` fires once the preparation succeeded; the
                // guard resets `isCompacting` and covers panic/unwinding exits).
                let mut run = CompactionRunGuard::start(
                    Some(Arc::clone(&session_events)),
                    Some(Arc::clone(&compacting)),
                    "threshold",
                );
                // `session_before_compact`: cancel skips this compaction, a
                // `compaction` replacement is used verbatim (reason:
                // threshold, upstream parity payload).
                let mut extension_compaction: Option<CompactionResult> = None;
                if let Some(plugins) = &plugins {
                    if plugins.has_handlers("session_before_compact") {
                        // The full branch enters the extension payload only
                        // when a handler exists (upstream `branchEntries`).
                        let branch_entries = match session.as_ref() {
                            Some(session) => session
                                .lock()
                                .await
                                .branch_entries(None)
                                .unwrap_or_default(),
                            None => Vec::new(),
                        };
                        let mut payload = serde_json::json!({
                            "type": "session_before_compact",
                            "customInstructions": null,
                            "reason": "threshold",
                            "willRetry": false,
                            "preparation": borrowed_preparation_summary(
                                &prepared,
                                previous_compaction_details(&branch_entries),
                            ),
                            "branchEntries": serde_json::to_value(&branch_entries).unwrap_or(JsonValue::Null),
                            "signal": { "aborted": compaction_aborted() },
                        });
                        match plugins.emit("session_before_compact", &mut payload).await {
                            Ok(outcome) => {
                                if outcome.result.get("cancel").and_then(JsonValue::as_bool)
                                    == Some(true)
                                {
                                    // Upstream reports the cancel as an aborted
                                    // `compaction_end` without an errorMessage,
                                    // followed by `session_compact_failed`
                                    // (aborted; `fromExtension` stays false —
                                    // upstream sets it only for a replacement,
                                    // agent-session.ts:3136-3140). The catch
                                    // order is end-then-failed (agent-session.ts:
                                    // 3276-3289).
                                    emit_session_event_to(
                                        &session_events,
                                        SessionEvent::CompactionEnd {
                                            reason: "threshold",
                                            result: None,
                                            aborted: true,
                                            will_retry: false,
                                            error_message: None,
                                        },
                                    );
                                    emit_plugin_event(
                                        plugins,
                                        "session_compact_failed",
                                        serde_json::json!({
                                            "type": "session_compact_failed",
                                            "reason": "threshold",
                                            "aborted": true,
                                            "willRetry": false,
                                            "fromExtension": false,
                                        }),
                                    )
                                    .await;
                                    run.end_emitted();
                                    return context;
                                }
                                extension_compaction =
                                    parse_borrowed_extension_compaction(&outcome.result, &prepared);
                            }
                            Err(error) => {
                                crate::ext_diag!(
                                    "[ext] session_before_compact dispatch failed: {error}"
                                )
                            }
                        }
                    }
                }
                // A cancel that landed while `session_before_compact` was
                // dispatching stops here, before any summarizer request or
                // extension-provided replacement is applied. Same aborted
                // event shape as the handler-cancel branch above.
                if compaction_aborted() {
                    // End first, failed second (upstream catch order).
                    emit_session_event_to(
                        &session_events,
                        SessionEvent::CompactionEnd {
                            reason: "threshold",
                            result: None,
                            aborted: true,
                            will_retry: false,
                            error_message: None,
                        },
                    );
                    if let Some(plugins) = &plugins {
                        emit_plugin_event(
                            plugins,
                            "session_compact_failed",
                            serde_json::json!({
                                "type": "session_compact_failed",
                                "reason": "threshold",
                                "aborted": true,
                                "willRetry": false,
                                "fromExtension": false,
                            }),
                        )
                        .await;
                    }
                    run.end_emitted();
                    return context;
                }
                let compacted = match extension_compaction {
                    Some(result) => {
                        let from_extension = true;
                        let appended_entry_id = if let Some(session) = &session {
                            // The compaction append builds the current system
                            // message from the whole projection and fsyncs the
                            // first write: run it on the blocking pool.
                            match SessionManager::append_compaction_off_thread(
                                session,
                                result.summary.clone(),
                                result.first_kept_entry_id.clone(),
                                result.tokens_before,
                                Some(compaction_details(&result)),
                                // Extension-provided usage is persisted with
                                // the compaction entry (upstream
                                // agent-session.ts:3164-3168).
                                Some(result.usage),
                                // Extension-generated: gates file-op
                                // carry-over in the next compaction's
                                // preparation (upstream `fromHook`).
                                true,
                            )
                            .await
                            {
                                Ok(entry_id) => Some(entry_id),
                                Err(error) => {
                                    // Storage did not take the compaction: the
                                    // in-memory context must stay the
                                    // pre-compaction one (and the prompt-start
                                    // snapshot fresh), so the next turn retries
                                    // instead of sending a context whose
                                    // summary/leaf boundary is not persisted.
                                    // Upstream `appendCompaction` throws into
                                    // the `_runAutoCompaction` catch.
                                    let aborted = compaction_aborted();
                                    let error_message = if aborted {
                                        None
                                    } else {
                                        Some(format!("Auto-compaction failed: {error}"))
                                    };
                                    crate::ext_diag!(
                                        "[session] extension compaction persistence failed: {error}"
                                    );
                                    emit_threshold_compaction_persistence_failure(
                                        &session_events,
                                        &plugins,
                                        &mut run,
                                        error_message,
                                        aborted,
                                        from_extension,
                                    )
                                    .await;
                                    return context;
                                }
                            }
                        } else {
                            None
                        };
                        // The rebuilt context is shorter than the prompt-start
                        // snapshot: later turns must re-read the session
                        // projection, never re-extend the stale snapshot.
                        snapshot_stale.store(true, std::sync::atomic::Ordering::SeqCst);
                        let rebuilt = compaction::rebuilt_context(&context, &result);
                        let estimated_tokens_after =
                            compaction::estimate_context_tokens(&rebuilt.messages).tokens;
                        // Ephemeral runs keep the rebuilt transcript in memory;
                        // a store-backed session re-reads its projection from
                        // the compaction entry, so retaining a copy here would
                        // defeat the projection's shared-Arc memory shape.
                        if session.is_none() {
                            *history
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Arc::new(rebuilt.messages.clone());
                        }
                        if let Some(plugins) = &plugins {
                            // Upstream `compactionEntry` rides alongside the
                            // historical `compaction` result field (see the
                            // manual path above).
                            let mut payload = serde_json::json!({
                                "type": "session_compact",
                                "reason": "threshold",
                                "willRetry": false,
                                "fromExtension": from_extension,
                                "compaction": compaction_result_wire(
                                    &result,
                                    estimated_tokens_after,
                                ),
                            });
                            if let Some(entry) = compaction_entry_wire(
                                session.as_ref(),
                                appended_entry_id.as_deref(),
                            )
                            .await
                            {
                                payload["compactionEntry"] = entry;
                            }
                            emit_plugin_event(plugins, "session_compact", payload).await;
                        }
                        emit_session_event_to(
                            &session_events,
                            SessionEvent::CompactionEnd {
                                reason: "threshold",
                                result: Some(compaction_result_wire(
                                    &result,
                                    estimated_tokens_after,
                                )),
                                aborted: false,
                                will_retry: false,
                                error_message: None,
                            },
                        );
                        run.end_emitted();
                        return Arc::new(rebuilt);
                    }
                    None => {
                        // The base completion carries the RUN's signal (Esc
                        // reaches the summarizer). The extra select layer
                        // below makes the automatic compaction's own signal
                        // cancel the request too, without touching the run.
                        let complete = auto_cancellable_complete(complete, auto_signal.clone());
                        let retry_observer = super::events::summarization_retry_observer(
                            &session_events,
                            super::events::SummarizationRetrySource::Compaction {
                                reason: "threshold",
                            },
                        );
                        match compaction::with_summarization_retry(
                            retry_observer,
                            compaction::compact_with_preparation_refs(
                                prepared, &model, complete, None,
                            ),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(error) => {
                                // Esc (or abort_compaction) during the
                                // summarizer request: the cancelled compaction
                                // is the normal aborted outcome (no
                                // errorMessage), and the run's own abort
                                // handling settles the prompt.
                                let aborted = compaction_aborted();
                                let error_message = if aborted {
                                    None
                                } else {
                                    Some(format!("Auto-compaction failed: {error}"))
                                };
                                // End first, failed second (upstream catch
                                // order, agent-session.ts:3276-3289).
                                emit_session_event_to(
                                    &session_events,
                                    SessionEvent::CompactionEnd {
                                        reason: "threshold",
                                        result: None,
                                        aborted,
                                        will_retry: false,
                                        error_message: error_message.clone(),
                                    },
                                );
                                if let Some(plugins) = &plugins {
                                    emit_plugin_event(
                                        plugins,
                                        "session_compact_failed",
                                        serde_json::json!({
                                            "type": "session_compact_failed",
                                            "reason": "threshold",
                                            "errorMessage": error_message,
                                            "aborted": aborted,
                                            "willRetry": false,
                                            "fromExtension": false,
                                        }),
                                    )
                                    .await;
                                }
                                run.end_emitted();
                                return context;
                            }
                        }
                    }
                };
                let appended_entry_id = if let Some(session) = &session {
                    // Blocking projection + first-write fsync: off-thread.
                    match SessionManager::append_compaction_off_thread(
                        session,
                        compacted.summary.clone(),
                        compacted.first_kept_entry_id.clone(),
                        compacted.tokens_before,
                        Some(compaction_details(&compacted)),
                        Some(compacted.usage),
                        false,
                    )
                    .await
                    {
                        Ok(entry_id) => Some(entry_id),
                        Err(error) => {
                            // Same contract as the extension branch above: no
                            // rebuilt context, no stale snapshot, and the
                            // terminal end-then-failed pair — the next turn
                            // retries the compaction.
                            let aborted = compaction_aborted();
                            let error_message = if aborted {
                                None
                            } else {
                                Some(format!("Auto-compaction failed: {error}"))
                            };
                            crate::ext_diag!(
                                "[session] threshold compaction persistence failed: {error}"
                            );
                            emit_threshold_compaction_persistence_failure(
                                &session_events,
                                &plugins,
                                &mut run,
                                error_message,
                                aborted,
                                false,
                            )
                            .await;
                            return context;
                        }
                    }
                } else {
                    None
                };
                // The rebuilt context is shorter than the prompt-start
                // snapshot: later turns must re-read the session projection
                // (whose leaf path now carries the compaction boundary), never
                // re-extend the stale pre-compaction snapshot.
                snapshot_stale.store(true, std::sync::atomic::Ordering::SeqCst);
                let rebuilt = compaction::rebuilt_context(&context, &compacted);
                let estimated_tokens_after =
                    compaction::estimate_context_tokens(&rebuilt.messages).tokens;
                // See the extension-result branch above: only an ephemeral
                // session owns an in-memory history.
                if session.is_none() {
                    *history
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Arc::new(rebuilt.messages.clone());
                }
                if let Some(plugins) = &plugins {
                    // Upstream `compactionEntry` rides alongside the
                    // historical `compaction` result field (see the manual
                    // path above).
                    let mut payload = serde_json::json!({
                        "type": "session_compact",
                        "reason": "threshold",
                        "willRetry": false,
                        "fromExtension": false,
                        "compaction": compaction_result_wire(
                            &compacted,
                            estimated_tokens_after,
                        ),
                    });
                    if let Some(entry) =
                        compaction_entry_wire(session.as_ref(), appended_entry_id.as_deref()).await
                    {
                        payload["compactionEntry"] = entry;
                    }
                    emit_plugin_event(plugins, "session_compact", payload).await;
                }
                emit_session_event_to(
                    &session_events,
                    SessionEvent::CompactionEnd {
                        reason: "threshold",
                        result: Some(compaction_result_wire(&compacted, estimated_tokens_after)),
                        aborted: false,
                        will_retry: false,
                        error_message: None,
                    },
                );
                run.end_emitted();
                Arc::new(rebuilt)
            })
        })
    }
}

/// Terminal failure pair for a THRESHOLD compaction whose session append
/// failed: `compaction_end` first (with the upstream
/// `Auto-compaction failed: …` text), then the extension
/// `session_compact_failed`, exactly the upstream `_runAutoCompaction` catch
/// order (agent-session.ts:3272-3296).
///
/// The caller then returns the ORIGINAL context and leaves the prompt-start
/// snapshot fresh, so the same storage state the hook just read stays valid and
/// the next turn re-runs the threshold check and retries the compaction.
async fn emit_threshold_compaction_persistence_failure(
    session_events: &Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
    plugins: &Option<Arc<crate::plugins::PluginHost>>,
    run: &mut CompactionRunGuard,
    error_message: Option<String>,
    aborted: bool,
    from_extension: bool,
) {
    emit_session_event_to(
        session_events,
        SessionEvent::CompactionEnd {
            reason: "threshold",
            result: None,
            aborted,
            will_retry: false,
            error_message: error_message.clone(),
        },
    );
    if let Some(plugins) = plugins {
        emit_plugin_event(
            plugins,
            "session_compact_failed",
            serde_json::json!({
                "type": "session_compact_failed",
                "reason": "threshold",
                "errorMessage": error_message,
                "aborted": aborted,
                "willRetry": false,
                "fromExtension": from_extension,
            }),
        )
        .await;
    }
    run.end_emitted();
}

/// One failed manual compaction, carrying the fields the terminal
/// `session_compact_failed` event needs so the wrapper can emit it AFTER
/// `compaction_end` (upstream `compact` catch order, agent-session.ts:2948-2968)
/// instead of from the failure site.
struct CompactFailure {
    error: anyhow::Error,
    error_message: Option<String>,
    aborted: bool,
    from_extension: bool,
}

impl CompactFailure {
    fn new(
        error: anyhow::Error,
        error_message: Option<String>,
        aborted: bool,
        from_extension: bool,
    ) -> Self {
        Self {
            error,
            error_message,
            aborted,
            from_extension,
        }
    }
}

impl From<anyhow::Error> for CompactFailure {
    /// Storage/projection failures inherit the upstream catch shape
    /// (`Compaction failed: …`, not aborted).
    fn from(error: anyhow::Error) -> Self {
        let error_message = Some(format!("Compaction failed: {error}"));
        Self::new(error, error_message, false, false)
    }
}

/// Holds the session's manual-compaction claim for one
/// [`AgentSession::compact`] call. Released explicitly before the terminal
/// `compaction_end` (upstream `_clearManualCompactionState()`, so end
/// listeners observe idle state) and again on drop for every other exit path;
/// both releases are compare-exchanges, so a stale release can never steal a
/// claim a newer run or compaction took in between.
struct ManualCompactionClaim {
    state: Arc<AtomicU8>,
    /// Cleared before the claim is released (same order as the run guard):
    /// an abort that cancelled the compaction must not leak a stale request
    /// into the next run's `run_abort_requested` check.
    run_abort_requested: Arc<AtomicBool>,
    idle_notify: Arc<Notify>,
}

impl ManualCompactionClaim {
    /// Release the claim and wake every waiter. Idempotent.
    fn release(&self) {
        // Only the owner may clear: an idempotent second release (explicit
        // release then Drop) must not touch a newer owner's request.
        if self.state.load(Ordering::SeqCst) == SESSION_COMPACTING {
            self.run_abort_requested.store(false, Ordering::SeqCst);
            if self
                .state
                .compare_exchange(
                    SESSION_COMPACTING,
                    SESSION_IDLE,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                self.idle_notify.notify_waiters();
            }
        }
    }
}

impl Drop for ManualCompactionClaim {
    fn drop(&mut self) {
        self.release();
    }
}

/// Monotonic generation for [`AutoCompactionCancelGuard`]: a stale guard must
/// never clear a NEWER trigger's armed signal.
static AUTO_COMPACTION_CANCEL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Arm one threshold compaction's own cancellation signal (a fresh signal per
/// trigger, so a cancel can never leak into a later turn) and return
/// `(generation, signal)`.
fn arm_auto_compaction_cancel(slot: &Arc<Mutex<Option<(u64, AbortSignal)>>>) -> (u64, AbortSignal) {
    let generation = AUTO_COMPACTION_CANCEL_SEQ.fetch_add(1, Ordering::Relaxed);
    let signal = AbortSignal::new();
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((generation, signal.clone()));
    (generation, signal)
}

/// Clears the automatic-compaction cancel slot when the armed compaction
/// exits (success, failure, or cancellation). Only the owner may clear: an
/// `abort_compaction()` that already took the slot leaves it empty, and a
/// newer trigger's pair is never disturbed.
struct AutoCompactionCancelGuard {
    slot: Arc<Mutex<Option<(u64, AbortSignal)>>>,
    generation: u64,
}

impl AutoCompactionCancelGuard {
    fn new(slot: Arc<Mutex<Option<(u64, AbortSignal)>>>, generation: u64) -> Self {
        Self { slot, generation }
    }
}

impl Drop for AutoCompactionCancelGuard {
    fn drop(&mut self) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            *slot = None;
        }
    }
}

/// Wrap a compaction completion callback so an automatic-compaction cancel
/// (its own signal) aborts the in-flight summarizer request by dropping it.
///
/// The base callback keeps the RUN's signal: Esc still reaches the summarizer
/// through the provider options, exactly as before. This layer only adds the
/// second, independently fired signal. `tokio::select!` is used instead of
/// spawning a watcher so nothing outlives one summarizer call.
fn auto_cancellable_complete(base: Arc<CompleteFn>, cancel: AbortSignal) -> Arc<CompleteFn> {
    Arc::new(Box::new(move |context| {
        let future = base(context);
        let cancel = cancel.clone();
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    Err("automatic compaction cancelled".to_owned())
                }
                result = future => result,
            }
        })
    }))
}

/// Wire summary of a [`compaction::CompactionPreparation`] (upstream
/// `CompactionPreparation`, compaction.ts:757-775: `firstKeptEntryId`,
/// `messagesToSummarize`, `turnPrefixMessages`, `isSplitTurn`, `tokensBefore`,
/// `previousSummary?`, `fileOps`, `settings`) over the BORROWED preparation
/// the session hooks work with — the spans serialize from their references, so
/// building the payload clones nothing beyond the wire values (only a
/// registered `session_before_compact` handler reaches it).
///
/// `previousSummary` is omitted when no previous compaction exists (upstream
/// optional field). `fileOps` carries the read/written/edited path sets as
/// arrays (the JSON bridge cannot ship JS `Set` objects); the previous
/// compaction's persisted details contribute exactly like upstream
/// `extractFileOperations`. `settings` mirrors the upstream camelCase
/// `CompactionSettings` (`enabled` / `reserveTokens` / `keepRecentTokens`).
fn borrowed_preparation_summary(
    preparation: &compaction::BorrowedCompactionPreparation<'_>,
    previous_details: Option<&JsonValue>,
) -> JsonValue {
    let mut payload = serde_json::Map::with_capacity(11);
    payload.insert(
        "firstKeptEntryId".to_owned(),
        serde_json::to_value(&preparation.first_kept_entry_id).unwrap_or(JsonValue::Null),
    );
    payload.insert(
        "firstKeptIndex".to_owned(),
        JsonValue::from(preparation.first_kept_message_index),
    );
    payload.insert(
        "tokensBefore".to_owned(),
        JsonValue::from(preparation.tokens_before),
    );
    payload.insert(
        "isSplitTurn".to_owned(),
        JsonValue::Bool(preparation.is_split_turn),
    );
    payload.insert(
        "messagesToSummarize".to_owned(),
        serde_json::to_value(&preparation.messages_to_summarize).unwrap_or(JsonValue::Null),
    );
    payload.insert(
        "keptMessages".to_owned(),
        serde_json::to_value(&preparation.kept_messages).unwrap_or(JsonValue::Null),
    );
    payload.insert(
        "turnPrefixMessages".to_owned(),
        serde_json::to_value(&preparation.turn_prefix_messages).unwrap_or(JsonValue::Null),
    );
    if let Some(previous_summary) = &preparation.previous_summary {
        payload.insert(
            "previousSummary".to_owned(),
            JsonValue::String(previous_summary.clone()),
        );
    }
    payload.insert(
        "fileOps".to_owned(),
        preparation_file_ops_json(preparation, previous_details),
    );
    payload.insert(
        "settings".to_owned(),
        serde_json::json!({
            "enabled": preparation.settings.enabled,
            "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        }),
    );
    JsonValue::Object(payload)
}

/// The newest PI-generated compaction's persisted `details` on the branch
/// (upstream `extractFileOperations` reads the previous compaction entry's
/// `readFiles` / `modifiedFiles`; extension-generated entries are skipped,
/// exactly like upstream's `!prevCompaction.fromHook`).
fn previous_compaction_details(entries: &[FileEntry]) -> Option<&JsonValue> {
    entries.iter().rev().find_map(|entry| match entry {
        FileEntry::Compaction(compaction) if compaction.from_hook != Some(true) => {
            compaction.details.as_ref()
        }
        _ => None,
    })
}

/// Upstream `FileOperations` for the summarized spans (plus the split-turn
/// prefix and the previous compaction's carried-over lists). The sets travel
/// as sorted arrays because the JSON bridge has no `Set`.
fn preparation_file_ops_json(
    preparation: &compaction::BorrowedCompactionPreparation<'_>,
    previous_details: Option<&JsonValue>,
) -> JsonValue {
    use std::collections::BTreeSet;

    let mut read: BTreeSet<String> = BTreeSet::new();
    let mut written: BTreeSet<String> = BTreeSet::new();
    let mut edited: BTreeSet<String> = BTreeSet::new();
    if let Some(details) = previous_details {
        if let Some(paths) = details.get("readFiles").and_then(JsonValue::as_array) {
            read.extend(
                paths
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned),
            );
        }
        if let Some(paths) = details.get("modifiedFiles").and_then(JsonValue::as_array) {
            edited.extend(
                paths
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned),
            );
        }
    }
    for message in preparation
        .messages_to_summarize
        .iter()
        .chain(preparation.turn_prefix_messages.iter())
    {
        collect_message_file_ops(message, &mut read, &mut written, &mut edited);
    }
    serde_json::json!({
        "read": read.into_iter().collect::<Vec<String>>(),
        "written": written.into_iter().collect::<Vec<String>>(),
        "edited": edited.into_iter().collect::<Vec<String>>(),
    })
}

/// [`compaction::extract_file_ops_from_messages`] over one message, keeping
/// the read/written/edited sets apart for the wire payload (the compaction
/// module's own sets are private and already merged into a carried-over
/// `FileOperations`).
fn collect_message_file_ops(
    message: &Message,
    read: &mut std::collections::BTreeSet<String>,
    written: &mut std::collections::BTreeSet<String>,
    edited: &mut std::collections::BTreeSet<String>,
) {
    match message {
        Message::ToolResult(result) => {
            if let Some(calls) = result
                .details
                .as_ref()
                .and_then(|details| details.get("nestedCalls"))
                .and_then(|nested| nested.get("calls"))
                .and_then(JsonValue::as_array)
            {
                for call in calls {
                    collect_file_op(
                        call.get("name").and_then(JsonValue::as_str).unwrap_or(""),
                        call.get("arguments"),
                        read,
                        written,
                        edited,
                    );
                }
            }
        }
        Message::Assistant(assistant) => {
            for block in &assistant.content {
                let ContentBlock::ToolCall(call) = block else {
                    continue;
                };
                let arguments = serde_json::to_value(&call.arguments).unwrap_or_default();
                collect_file_op(&call.name, Some(&arguments), read, written, edited);
            }
        }
        _ => {}
    }
}

fn collect_file_op(
    tool_name: &str,
    args: Option<&JsonValue>,
    read: &mut std::collections::BTreeSet<String>,
    written: &mut std::collections::BTreeSet<String>,
    edited: &mut std::collections::BTreeSet<String>,
) {
    let Some(path) = args
        .and_then(|args| args.get("path"))
        .and_then(JsonValue::as_str)
    else {
        return;
    };
    if path.is_empty() {
        return;
    }
    match tool_name {
        "read" => {
            read.insert(path.to_owned());
        }
        "write" => {
            written.insert(path.to_owned());
        }
        "edit" => {
            edited.insert(path.to_owned());
        }
        _ => {}
    }
}

/// The outcome of a `session_before_compact` dispatch (upstream
/// `SessionBeforeCompactResult`): proceed with the built-in summary, cancel
/// the run, or use the handler's replacement compaction verbatim.
pub(super) enum BeforeCompactDecision {
    Proceed,
    Cancel,
    Replace(Box<CompactionResult>),
}

/// Dispatch `session_before_compact` for one compaction run (upstream
/// `_runAutoCompaction` / manual `compact` interception). Shared by the
/// overflow recovery paths; the manual/threshold paths emit the same payload
/// inline. Returns [`BeforeCompactDecision::Proceed`] when no handler exists
/// or the dispatch fails (fail-open, exactly like the existing paths).
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_before_compact(
    plugins: Option<&Arc<PluginHost>>,
    reason: &'static str,
    will_retry: bool,
    custom_instructions: Option<&str>,
    preparation: &compaction::BorrowedCompactionPreparation<'_>,
    branch_entries: &[FileEntry],
    aborted: bool,
) -> BeforeCompactDecision {
    let Some(plugins) = plugins else {
        return BeforeCompactDecision::Proceed;
    };
    if !plugins.has_handlers("session_before_compact") {
        return BeforeCompactDecision::Proceed;
    }
    let mut payload = serde_json::json!({
        "type": "session_before_compact",
        "customInstructions": custom_instructions,
        "reason": reason,
        "willRetry": will_retry,
        "preparation": borrowed_preparation_summary(
            preparation,
            previous_compaction_details(branch_entries),
        ),
        "branchEntries": serde_json::to_value(branch_entries).unwrap_or(JsonValue::Null),
        "signal": { "aborted": aborted },
    });
    match plugins.emit("session_before_compact", &mut payload).await {
        Ok(outcome) => {
            if outcome.result.get("cancel").and_then(JsonValue::as_bool) == Some(true) {
                BeforeCompactDecision::Cancel
            } else if let Some(result) =
                parse_borrowed_extension_compaction(&outcome.result, preparation)
            {
                BeforeCompactDecision::Replace(Box::new(result))
            } else {
                BeforeCompactDecision::Proceed
            }
        }
        Err(error) => {
            crate::ext_diag!("[ext] session_before_compact dispatch failed: {error}");
            BeforeCompactDecision::Proceed
        }
    }
}

/// [`parse_extension_compaction`] over the borrowed preparation: identical
/// parsing, with the borrowed kept messages cloned ONCE into the retained tail
/// (the rebuilt context owns it).
fn parse_borrowed_extension_compaction(
    result: &JsonValue,
    preparation: &compaction::BorrowedCompactionPreparation<'_>,
) -> Option<CompactionResult> {
    parse_extension_compaction(
        result,
        preparation.first_kept_message_index,
        preparation.tokens_before,
        preparation
            .kept_messages
            .iter()
            .map(|message| (*message).clone())
            .collect(),
    )
}

/// Parse a `session_before_compact` handler's `compaction` replacement
/// (upstream `SessionBeforeCompactResult.compaction`): `{summary,
/// firstKeptEntryId, tokensBefore, usage?, details?}`. The kept messages from
/// the underlying preparation become the retained tail so the rebuilt context
/// keeps the post-cut conversation.
fn parse_extension_compaction(
    result: &JsonValue,
    first_kept_message_index: usize,
    tokens_before: u64,
    retained_tail: Vec<Message>,
) -> Option<CompactionResult> {
    let compaction = result.get("compaction")?;
    let summary = compaction.get("summary")?.as_str()?.to_owned();
    Some(CompactionResult {
        summary,
        tokens_before: compaction
            .get("tokensBefore")
            .and_then(JsonValue::as_u64)
            .unwrap_or(tokens_before),
        usage: compaction
            .get("usage")
            .and_then(crate::agent::tool_execution::parse_usage_json)
            .unwrap_or_default(),
        first_kept_index: first_kept_message_index,
        first_kept_entry_id: compaction
            .get("firstKeptEntryId")
            .and_then(JsonValue::as_str)
            .map(String::from),
        retained_tail,
        read_files: compaction
            .get("details")
            .and_then(|details| details.get("readFiles"))
            .and_then(JsonValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        modified_files: compaction
            .get("details")
            .and_then(|details| details.get("modifiedFiles"))
            .and_then(JsonValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
    })
}
/// The upstream `SessionCompactEvent.compactionEntry` payload: the persisted
/// [`FileEntry::Compaction`] entry in its session-file wire shape (upstream
/// `CompactionEntry`: `type: "compaction"`, `id`, `parentId`, `timestamp`,
/// `summary`, `firstKeptEntryId`, `tokensBefore`, plus `details`/`usage`/
/// `systemMessage` when set).
///
/// Only a persisted entry can be reported: a store-less (ephemeral) session
/// has no entry, so the caller omits `compactionEntry` instead of fabricating
/// ids and timestamps.
pub(crate) async fn compaction_entry_wire(
    session: Option<&Arc<tokio::sync::Mutex<SessionManager>>>,
    entry_id: Option<&str>,
) -> Option<JsonValue> {
    let entry_id = entry_id?;
    let session = session?;
    let session = session.lock().await;
    let entry = session.entry(entry_id)?;
    serde_json::to_value(entry).ok()
}

/// The wire `CompactionResult` object (upstream `CompactionResult`,
/// agent-session.ts:2868-2875: summary / firstKeptEntryId / tokensBefore /
/// estimatedTokensAfter / usage / details).
///
/// Shared by the `compaction_end` session event and the RPC `compact`
/// response (upstream rpc-mode.ts returns the whole `CompactionResult`), so
/// the two surfaces can never drift: `usage` is the summarizer response's
/// usage (zeroes when the provider reported none) and `details` the
/// `CompactionDetails` file lists.
pub(crate) fn compaction_result_wire(
    result: &CompactionResult,
    estimated_tokens_after: u64,
) -> JsonValue {
    serde_json::json!({
        "summary": result.summary,
        "firstKeptEntryId": result.first_kept_entry_id,
        "tokensBefore": result.tokens_before,
        "estimatedTokensAfter": estimated_tokens_after,
        "usage": serde_json::to_value(result.usage).unwrap_or(JsonValue::Null),
        "details": compaction_details(result),
    })
}

/// Compaction entry `details` object: the file-operation lists upstream
/// persists alongside the summary (`CompactionDetails`).
pub(crate) fn compaction_details(result: &CompactionResult) -> JsonValue {
    serde_json::json!({
        "readFiles": result.read_files,
        "modifiedFiles": result.modified_files,
    })
}
