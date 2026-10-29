use super::*;

/// Where a `fork` branches (upstream `fork(entryId, {position})`,
/// agent-session-runtime.ts:263-289; default `"before"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    /// The entry must be a user message; the fork branches at its PARENT
    /// (a `None` parent starts a fresh session with the current one as its
    /// parent link) and the user text is returned as `selectedText`.
    Before,
    /// The fork branches at the entry itself; no `selectedText`.
    At,
}

impl ForkPosition {
    /// Parse the wire string (upstream `position` values); anything but
    /// `"at"` resolves to the upstream default `"before"`.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("at") => ForkPosition::At,
            _ => ForkPosition::Before,
        }
    }
}

/// Outcome of [`AgentSession::fork_entry`] (upstream returns
/// `{cancelled, selectedText}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkEntryOutcome {
    /// A `session_before_fork` handler cancelled the fork (upstream
    /// `{cancelled: true}` — not an error).
    Cancelled,
    /// The fork was applied; `selected_text` carries the user message text
    /// for the `Before` position (upstream `selectedText`).
    Branched { selected_text: Option<String> },
}

impl AgentSession {
    /// The session storage handle, for hosts that need the live
    /// [`SessionManager`] (the RPC host's `get_entries` / `get_tree` /
    /// `fork`).
    pub fn session_manager(&self) -> Option<Arc<tokio::sync::Mutex<SessionManager>>> {
        self.session.clone()
    }

    /// The current storage generation (see
    /// [`Self::session_storage_generation_matches`]): starts at 0 and moves
    /// forward on every successful storage swap.
    pub fn session_storage_generation(&self) -> u64 {
        self.session_storage_generation.load(Ordering::SeqCst)
    }

    /// Whether `generation` still names the transcript currently installed.
    /// Work started against an older transcript (a `!` bash command that
    /// outlived a `/new`/`/resume`/fork) must not record its result into the
    /// replacement session.
    pub fn session_storage_generation_matches(&self, generation: u64) -> bool {
        self.session_storage_generation() == generation
    }

    // Session storage seams (upstream newSession / fork / clone / name)
    // ------------------------------------------------------------------

    /// Replace the persisted session storage (upstream `newSession`, `fork`,
    /// and `clone` swap the session manager on the live runtime). The cached
    /// session identity refreshes; the next prompt run reads its context from
    /// the replacement transcript. Returns `false` when no storage is
    /// configured (`--no-session`: the run stays ephemeral) or when a
    /// `session_before_switch` handler CANCELLED the switch (upstream
    /// cancel/summary semantics, runner.ts:1080-1118).
    ///
    /// Extension seams (upstream runner.ts): `session_before_switch` fires
    /// before the swap (a `cancel: true` result aborts it), and
    /// `session_tree` fires after the swap with the new session's tree.
    ///
    /// The post-swap lifecycle also emits `session_start` with the upstream
    /// reason (`new`/`resume`/`fork`, [`infer_session_start_reason`]) and
    /// `previousSessionFile`, then marks the lazy startup dispatch consumed so
    /// the next prompt cannot deliver a second `session_start`.
    pub async fn replace_session_storage(&self, session: SessionManager) -> bool {
        // The destination file is resolved BEFORE the swap (`session_before_switch`
        // and `session_shutdown` both carry it, upstream
        // `SessionBeforeSwitchEvent.targetSessionFile` /
        // `SessionShutdownEvent.targetSessionFile`); it is omitted when the
        // replacement has no persisted file (in-memory target).
        let target_session_file = session
            .session_file()
            .map(|path| path.display().to_string());
        // `session_before_switch`: a handler may cancel the switch (upstream
        // `SessionBeforeEventResult.cancel` short-circuits the emit loop).
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("session_before_switch") {
                let mut payload = serde_json::json!({
                    "type": "session_before_switch",
                    "reason": "switch",
                });
                if let Some(target) = &target_session_file {
                    payload["targetSessionFile"] = JsonValue::String(target.clone());
                }
                let cancelled = match plugins.emit("session_before_switch", &mut payload).await {
                    Ok(outcome) => {
                        outcome.result.get("cancel").and_then(JsonValue::as_bool) == Some(true)
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] session_before_switch dispatch failed: {error}");
                        false
                    }
                };
                if cancelled {
                    return false;
                }
            }
            // Upstream `teardownCurrent` emits `session_shutdown` for the
            // outgoing session with the destination file before the swap
            // (agent-session-runtime.ts:167-177).
            plugins
                .emit_session_shutdown("switch", target_session_file.as_deref())
                .await;
        }
        self.swap_session_storage(session, None).await
    }

    /// Swap in a replacement session storage WITHOUT the
    /// `session_before_switch` dispatch: refreshes the cached session
    /// identity, fires `session_tree`, and emits the post-swap
    /// `session_start` (`reason` overrides the inferred upstream reason).
    /// Upstream's fork path (agent-session-runtime.ts) swaps the manager
    /// after a `session_before_fork` (NOT a `session_before_switch`), so
    /// [`Self::fork_entry`] swaps through this seam.
    async fn swap_session_storage(&self, session: SessionManager, reason: Option<&str>) -> bool {
        let Some(slot) = &self.session else {
            return false;
        };
        // Background work belongs to the transcript it was started from: a
        // completion that lands after the swap would persist its
        // `backgroundToolResult` frame into the NEW store (the persist closure
        // holds the session slot, not the manager), attributing a call the new
        // transcript never made. The batch barrier is held across the abort +
        // swap, so a completion already delivering finishes against the OLD
        // manager (it acquired the barrier first) and none can observe the
        // half-swapped state; `abort_all` then bumps the registry generation,
        // so every later reservation from the old transcript delivers nothing.
        let tracker = self.background.commit_tracker();
        let _swap_barrier = tracker.begin_batch().await;
        let cancelled = self.background.abort_all();
        if cancelled > 0 {
            tracing::debug!(
                cancelled,
                "aborted in-flight background tool calls on session storage swap"
            );
        }
        // A user `!` bash command still running belongs to the outgoing
        // transcript too: abort it here so its completion cannot race the
        // swap (the generation bump below is the backstop for a completion
        // that already passed its abort check).
        self.abort_bash();
        // Queued completion frames belong to the outgoing transcript: a later
        // run on the new store must not inject them (the durable copy lives in
        // the old session's entries).
        let stale_frames = self.background_follow_up_inbox.drain_all().await.len();
        if stale_frames > 0 {
            tracing::debug!(
                stale_frames,
                "dropped queued background frames on session storage swap"
            );
        }
        // Steering/follow-up queues are per-runtime state in upstream: a
        // switch creates a new session runtime with empty arrays. pir's inbox
        // facade survives the storage swap, so drain it here or the first
        // prompt in the new session receives text queued for the outgoing one
        // (and the dim pending block no longer shows it). `clear_queue` also
        // emits `queue_update`, which clears the TUI's pending display.
        let cleared = self.clear_queue().await;
        if !cleared.steering.is_empty() || !cleared.follow_up.is_empty() {
            tracing::debug!(
                steering = cleared.steering.len(),
                follow_up = cleared.follow_up.len(),
                "dropped queued steering/follow-up messages on session storage swap"
            );
        }
        // Who can observe `session_tree`? With no consumer the payload — and
        // the whole session forest inside it — need not be built at all.
        // Interest-based for native plugins: the SDK bridge plugin is attached
        // to every SDK agent but only declares `tool_execution_update`, so a
        // `has_non_js_plugins` check would materialize the forest for it on
        // every switch and then hand it a value it never reads.
        let rust_consumers = self
            .plugins
            .as_ref()
            .is_some_and(|plugins| plugins.has_native_handlers("session_tree"));
        let dispatch_tree = self
            .plugins
            .as_ref()
            .is_some_and(|plugins| session_tree_consumers(plugins));
        // The outgoing session's leaf backs the event's `oldLeafId` (the
        // switching session is gone after the swap); its file backs
        // `session_start.previousSessionFile`.
        let (old_leaf_id, old_session_file) = {
            let outgoing = slot.lock().await;
            (
                outgoing.leaf_id().map(str::to_owned),
                outgoing.session_file().map(Path::to_path_buf),
            )
        };
        // The upstream `SessionStartEvent.reason`: hosts that know the
        // operation override it; RPC/SDK swaps through the plain
        // `replace_session_storage` and are classified from the incoming
        // manager (fresh create / opened file / fork of the outgoing file).
        let start_reason = reason
            .unwrap_or_else(|| infer_session_start_reason(&session, old_session_file.as_deref()));
        *slot.lock().await = session;
        // The transcript behind this slot changed: long-running work that
        // captured the previous generation must not persist into the new
        // session (checked by its recording seam).
        self.session_storage_generation
            .fetch_add(1, Ordering::SeqCst);
        let (session_id, session_path, tree, leaf_id) = {
            let identity = slot.lock().await;
            (
                Some(identity.id().to_owned()),
                Some(identity.path().to_path_buf()),
                if !dispatch_tree {
                    None
                } else if rust_consumers {
                    // A non-JS plugin receives the payload as a `Value`, built
                    // iteratively: the derived serializer recurses one frame
                    // per entry.
                    Some(SessionTreePayload::Value(
                        crate::session_manager::TreeNode::to_value_iterative(identity.tree()),
                    ))
                } else {
                    // JS only: carry the tree as pre-rendered TEXT. The
                    // reply-free dispatch never turns it back into a `Value`
                    // on this side (the old reply leg re-parsed, cloned and
                    // recursively dropped it).
                    Some(SessionTreePayload::Text(
                        crate::session_manager::TreeNode::forest_to_json_iterative(identity.tree()),
                    ))
                },
                identity.leaf_id().map(str::to_owned),
            )
        };
        *self
            .session_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session_id;
        *self
            .session_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session_path;
        // `session_tree`: the JS side merges the tree of the session that
        // just became active. `newLeafId`/`oldLeafId` follow the upstream
        // event contract (agent-session.ts:4140-4146); `summaryEntry` /
        // `fromExtension` are navigation-only and stay omitted on a switch.
        match (&self.plugins, tree) {
            (Some(plugins), Some(SessionTreePayload::Value(tree))) => {
                // Explicit map inserts, NOT `json!`: the macro routes a
                // `Value` expression through `serde_json::to_value`, a
                // recursive deep CLONE of the tree. Key order matches the old
                // literal.
                let mut payload = serde_json::Map::new();
                payload.insert(
                    String::from("type"),
                    JsonValue::String(String::from("session_tree")),
                );
                payload.insert(String::from("tree"), tree);
                payload.insert(
                    String::from("newLeafId"),
                    leaf_id.map_or(JsonValue::Null, JsonValue::String),
                );
                payload.insert(
                    String::from("oldLeafId"),
                    old_leaf_id.map_or(JsonValue::Null, JsonValue::String),
                );
                let mut payload = JsonValue::Object(payload);
                let payload_json =
                    crate::session_manager::json_depth::write_value_iteratively(&payload);
                plugins
                    .emit_read_only_json("session_tree", payload_json, Some(&mut payload))
                    .await;
                // Iterative teardown: the drop glue walks one frame per entry.
                crate::session_manager::json_depth::drop_value_iteratively(payload);
            }
            (Some(plugins), Some(SessionTreePayload::Text(tree_json))) => {
                let mut payload_json = String::with_capacity(128 + tree_json.len());
                payload_json.push('{');
                let mut first = true;
                crate::session_manager::json_depth::push_field(
                    &mut payload_json,
                    &mut first,
                    "type",
                    "session_tree",
                );
                // Written directly (not through `push_field`): serializing a
                // `Value` wrapping the nesting is the recursion this avoids.
                // Key order matches the old literal.
                payload_json.push(',');
                payload_json.push_str("\"tree\":");
                payload_json.push_str(&tree_json);
                crate::session_manager::json_depth::push_field(
                    &mut payload_json,
                    &mut first,
                    "newLeafId",
                    &leaf_id,
                );
                crate::session_manager::json_depth::push_field(
                    &mut payload_json,
                    &mut first,
                    "oldLeafId",
                    &old_leaf_id,
                );
                payload_json.push('}');
                plugins
                    .emit_read_only_json("session_tree", payload_json, None)
                    .await;
            }
            _ => {}
        }
        // The native host registry is keyed by session id, so every swap must
        // re-key it (the JS `ctx.newSession` path and native
        // `NativeSessionHost` both land here). The `Arc` is only available for
        // sessions built through the `Arc` constructors; a by-value host
        // keeps reporting `Unsupported` honestly.
        let self_arc = self
            .self_weak
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .and_then(|weak| weak.upgrade());
        if let Some(self_arc) = self_arc {
            crate::agent_session::native_host::register_native_session_host(&self_arc);
        }
        // Post-swap lifecycle: upstream `createRuntime` emits
        // `session_start {reason, previousSessionFile?}` for the replacement
        // runtime (agent-session-runtime.ts:210-220, 251, 305). This also
        // consumes the lazy startup dispatch, so the same lifecycle is never
        // delivered twice.
        self.emit_session_start_reason(start_reason, old_session_file.as_deref())
            .await;
        true
    }

    /// Start a new branch from an earlier entry AND append a branch_summary
    /// entry capturing the abandoned path (upstream `branchWithSummary`,
    /// session-manager.ts:1600-1625) — the navigate-tree seam for the RPC/TUI
    /// `/tree`. `entry_id: None` branches from before the first entry.
    /// `None` when no storage is configured.
    pub async fn branch_with_summary(
        &self,
        entry_id: Option<&str>,
        summary: &str,
        usage: Option<crate::ai::types::Usage>,
        from_hook: bool,
    ) -> Option<crate::Result<String>> {
        let session = self.session.as_ref()?;
        let mut session = session.lock().await;
        Some(session.branch_with_summary(
            entry_id,
            summary.to_owned(),
            None,
            if from_hook { Some(true) } else { None },
            usage,
        ))
    }

    /// Navigate to a different point in the session tree (upstream
    /// `navigateTree`, agent-session.ts:3964-4150): collect the abandoned
    /// branch (old leaf back to the common ancestor with the target), when
    /// `summarize` summarize it with the session's provider/model via
    /// `compaction::branch_summarization::generate_branch_summary`, move the
    /// leaf, and persist the summary through
    /// [`SessionManager::branch_with_summary`].
    ///
    /// `session_before_tree` fires after the read-only collection and BEFORE
    /// any summarization or leaf move (upstream agent-session.ts:4036-4081).
    /// The unified ordered dispatch reaches the JS face (prelude
    /// `session_before_tree` arm) and every native plugin exactly once, with
    /// the upstream payload `{type, preparation, signal}` (`preparation.label`
    /// is omitted because this method carries no label option; the `/tree`
    /// command applies its own label after the move). A
    /// `cancel: true` handler result aborts the navigation
    /// ([`TreeNavigation::cancelled`]) with the leaf and transcript untouched,
    /// and no summarizer request; the other upstream result fields mutate the
    /// preparation: `customInstructions` / `replaceInstructions` override the
    /// summarizer inputs, `summary` replaces the generated branch summary
    /// (when `summarize` was requested), and `label` labels the summary entry
    /// (or the target when no summary was attached).
    ///
    /// Semantics (upstream): a user-message / custom-message target puts the
    /// leaf on the target's PARENT and returns its text as `editor_text`;
    /// any other target selects the target itself. A summarizer failure
    /// returns `Err` and leaves the leaf UNCHANGED (upstream throws).
    /// `session_tree` fires after the move (upstream agent-session.ts:4140-
    /// 4146) with `newLeafId`/`oldLeafId` and, when a summary was attached,
    /// its `summaryEntry` plus `fromExtension` (true for a handler-supplied
    /// summary).
    /// `None` when no session storage is configured.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
        replace_instructions: bool,
        reserve_tokens: u64,
    ) -> Option<Result<TreeNavigation, String>> {
        let session = self.session.as_ref()?;
        if self.is_streaming() {
            return Some(Err(
                "Wait for the current response to finish before navigating the session tree."
                    .to_owned(),
            ));
        }

        // Phase 1 (read-only, under the storage lock): collect the abandoned
        // branch and resolve the new leaf position. The old leaf id rides
        // along so phase 3 can detect a leaf moved behind our back.
        let (collected, old_leaf_id, new_leaf_id, editor_text) = {
            let session = session.lock().await;
            let old_leaf_id = session.leaf_id().map(str::to_owned);
            if old_leaf_id.as_deref() == Some(target_id) {
                // No-op: already at the target (upstream `cancelled: false`).
                return Some(Ok(TreeNavigation::default()));
            }
            if session.entry(target_id).is_none() {
                return Some(Err(format!("Entry {target_id} not found")));
            }
            let collected = compaction::branch_summarization::collect_entries_for_branch_summary(
                &session,
                old_leaf_id.as_deref(),
                target_id,
            );
            let (new_leaf_id, editor_text) = match session.entry(target_id) {
                Some(FileEntry::Message(inner)) if inner.message.role() == "user" => (
                    inner.parent_id.clone(),
                    Some(stored_message_text(&inner.message)),
                ),
                Some(FileEntry::CustomMessage(inner)) => (
                    inner.parent_id.clone(),
                    Some(custom_message_text(&inner.content)),
                ),
                _ => (Some(target_id.to_owned()), None),
            };
            (collected, old_leaf_id, new_leaf_id, editor_text)
        };

        // Phase 2: `session_before_tree` (upstream agent-session.ts:4036-
        // 4081): the preparation is complete and NOTHING has been mutated
        // yet, so a handler can cancel the navigation before any
        // summarization or leaf move. The unified ordered dispatch delivers
        // the event to the JS face (prelude's `session_before_tree` arm) and
        // to every native plugin exactly once; a `cancel: true` result
        // short-circuits the ordered list (`MergedEmit`), so later handlers do
        // not run — upstream `emit` semantics.
        //
        // Result mapping (upstream `SessionBeforeTreeResult`):
        // - `cancel: true` => [`TreeNavigation::cancelled`], no summary, no
        //   leaf move;
        // - `customInstructions` / `replaceInstructions` => override the
        //   summarizer inputs;
        // - `summary: {summary, details?, usage?}` => used instead of the
        //   generated branch summary when `summarize` was requested;
        // - `label` => attached to the summary entry, or to the target entry
        //   when no summary was written.
        let mut custom_instructions = custom_instructions.map(str::to_owned);
        let mut replace_instructions = replace_instructions;
        let mut extension_summary: Option<(
            String,
            Option<JsonValue>,
            Option<crate::ai::types::Usage>,
        )> = None;
        let mut label: Option<String> = None;
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("session_before_tree") {
                let mut payload = serde_json::json!({
                    "type": "session_before_tree",
                    "preparation": {
                        "targetId": target_id,
                        "oldLeafId": old_leaf_id,
                        "commonAncestorId": collected.common_ancestor_id,
                        "entriesToSummarize": serde_json::to_value(&collected.entries)
                            .unwrap_or(JsonValue::Null),
                        "userWantsSummary": summarize,
                        "customInstructions": custom_instructions,
                        "replaceInstructions": replace_instructions,
                    },
                    // Upstream passes a live `AbortSignal`; the JSON wire
                    // carries its observable state (the same shape
                    // `session_before_compact` uses).
                    "signal": { "aborted": false },
                });
                match plugins.emit("session_before_tree", &mut payload).await {
                    Ok(outcome) => {
                        if outcome.result.get("cancel").and_then(JsonValue::as_bool) == Some(true) {
                            return Some(Ok(TreeNavigation {
                                cancelled: true,
                                ..TreeNavigation::default()
                            }));
                        }
                        if let Some(value) = outcome
                            .result
                            .get("customInstructions")
                            .and_then(JsonValue::as_str)
                        {
                            custom_instructions = Some(value.to_owned());
                        }
                        if let Some(value) = outcome
                            .result
                            .get("replaceInstructions")
                            .and_then(JsonValue::as_bool)
                        {
                            replace_instructions = value;
                        }
                        if let Some(value) = outcome.result.get("label").and_then(JsonValue::as_str)
                        {
                            label = Some(value.to_owned());
                        }
                        if summarize {
                            if let Some(summary) = outcome.result.get("summary") {
                                if let Some(text) =
                                    summary.get("summary").and_then(JsonValue::as_str)
                                {
                                    extension_summary = Some((
                                        text.to_owned(),
                                        summary
                                            .get("details")
                                            .cloned()
                                            .filter(|details| !details.is_null()),
                                        summary.get("usage").and_then(
                                            crate::agent::tool_execution::parse_usage_json,
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] session_before_tree dispatch failed: {error}")
                    }
                }
            }
        }

        // Phase 3: summarize the abandoned branch (the leaf is not moved on
        // failure, upstream throws before the switch). `summarize` arms the
        // cancellable branch-summary signal FIRST (upstream
        // `this._branchSummaryAbortController = new AbortController()` at the
        // top of `navigateTree`, agent-session.ts:4043): a host-side Esc
        // (`abort_branch_summary`) reaches the in-flight summarizer request
        // while the worker command loop is blocked here. A handler-supplied
        // `summary` replaces the generated one (upstream `extensionSummary`);
        // it bypasses the model call entirely.
        let mut summary_text: Option<String> = None;
        let mut summary_details: Option<JsonValue> = None;
        let mut summary_usage: Option<crate::ai::types::Usage> = None;
        let mut from_extension = false;
        if let Some((text, details, usage)) = extension_summary {
            summary_text = Some(text);
            summary_details = details;
            summary_usage = usage;
            from_extension = true;
        } else if summarize {
            let signal = self.branch_summary_abort.arm();
            if !collected.entries.is_empty() {
                let model = self.current_model();
                let complete = self.complete_fn_for_signal(&model, signal.clone());
                let retry_observer = Some(self.summarization_retry_observer(
                    super::events::SummarizationRetrySource::BranchSummary,
                ));
                let result =
                    compaction::branch_summarization::generate_branch_summary_with_observer(
                        &collected.entries,
                        &model,
                        complete,
                        custom_instructions.as_deref(),
                        replace_instructions,
                        reserve_tokens,
                        retry_observer,
                    )
                    .await;
                if result.aborted || signal.is_aborted() {
                    // Upstream `{cancelled: true, aborted: true}`
                    // (agent-session.ts:4061-4063): the leaf, the transcript,
                    // and the editor are all untouched
                    // (interactive-mode.ts:5676-5680 shows the status and
                    // re-opens the tree selector).
                    return Some(Ok(TreeNavigation {
                        aborted: true,
                        ..TreeNavigation::default()
                    }));
                }
                if let Some(error) = result.error {
                    return Some(Err(error));
                }
                summary_text = result.summary;
                summary_details = Some(serde_json::json!({
                    "readFiles": result.read_files,
                    "modifiedFiles": result.modified_files,
                }));
                summary_usage = result.usage;
            }
        }

        // Phase 4: switch the leaf, attaching the summary at the target
        // position (upstream `branchWithSummary(newLeafId, ...)`; a `None`
        // target id resets the leaf like `resetLeaf`).
        let mut session = session.lock().await;
        // The summarize gap released the storage lock, so a concurrent append
        // could have moved the leaf since phase 1 collected the abandoned
        // branch — making `collected` stale. Fail the navigation rather than
        // attach a summary computed from the wrong entries.
        if let Err(error) = compaction::branch_summarization::ensure_leaf_unchanged(
            session.leaf_id(),
            old_leaf_id.as_deref(),
        ) {
            return Some(Err(error));
        }
        let summary_entry_id = match summary_text {
            Some(text) => match session.branch_with_summary(
                new_leaf_id.as_deref(),
                text,
                summary_details,
                if from_extension { Some(true) } else { None },
                summary_usage,
            ) {
                Ok(id) => Some(id),
                Err(error) => return Some(Err(format!("branch summary failed: {error}"))),
            },
            None => match new_leaf_id {
                Some(id) => {
                    if let Err(error) = session.branch(&id) {
                        return Some(Err(format!("branch failed: {error}")));
                    }
                    None
                }
                None => {
                    session.reset_leaf();
                    None
                }
            },
        };
        // Upstream `label` result field: label the summary entry when one was
        // attached, otherwise the target entry (agent-session.ts:4114-4122).
        if let Some(label) = &label {
            let label_target = summary_entry_id.as_deref().unwrap_or(target_id);
            if let Err(error) = session.set_label(label_target, Some(label.as_str())) {
                crate::ext_diag!("[ext] session_before_tree label ignored: {error}");
            }
        }
        // `session_tree` event data (upstream agent-session.ts:4140-4146):
        // the post-switch leaf, the abandoned leaf, and the serialized
        // summary entry when one was attached. The storage lock is released
        // BEFORE dispatch — a handler's `ctx.appendEntry` / `ctx.navigateTree`
        // host fns take the same lock.
        let new_leaf_id_after = session.leaf_id().map(str::to_owned);
        let summary_entry = summary_entry_id
            .as_deref()
            .and_then(|id| session.entry(id))
            .and_then(|entry| serde_json::to_value(entry).ok());
        drop(session);
        if let Some(plugins) = &self.plugins {
            if session_tree_consumers(plugins) {
                let mut payload = serde_json::json!({
                    "type": "session_tree",
                    "newLeafId": new_leaf_id_after,
                    "oldLeafId": old_leaf_id,
                });
                if let Some(entry) = summary_entry {
                    payload["summaryEntry"] = entry;
                    // Upstream `fromExtension: summaryText ? fromExtension :
                    // undefined`: true only for a handler-supplied
                    // `session_before_tree` summary; a host-generated one
                    // reports false, and without a summary the field stays
                    // omitted.
                    payload["fromExtension"] = JsonValue::Bool(from_extension);
                }
                emit_session_tree(plugins, payload).await;
            }
        }
        Some(Ok(TreeNavigation {
            editor_text,
            summary_entry_id,
            aborted: false,
            cancelled: false,
        }))
    }

    /// The `/tree` branch-summarization cancellation handle (upstream
    /// `_branchSummaryAbortController`, agent-session.ts:408): a clone of the
    /// session's latch, shared with a host so Esc can cancel a summary while
    /// the worker command loop is blocked inside [`Self::navigate_tree`].
    pub fn branch_summary_abort_handle(&self) -> BranchSummaryAbort {
        self.branch_summary_abort.clone()
    }

    /// Cancel the running `/tree` branch summarization (upstream
    /// `abortBranchSummary`, agent-session.ts:2947-2949): the summarizer
    /// stream aborts, the leaf stays put, and `navigate_tree` reports
    /// `aborted: true`. A no-op when no summarization is armed.
    pub fn abort_branch_summary(&self) {
        self.branch_summary_abort.cancel();
    }

    /// Run `f` with mutable access to the live session storage (upstream
    /// `/name`, `/session` read-write access to the session manager). `None`
    /// when no session storage is configured.
    pub async fn with_session_manager<R>(
        &self,
        f: impl FnOnce(&mut SessionManager) -> R,
    ) -> Option<R> {
        let session = self.session.as_ref()?;
        let mut guard = session.lock().await;
        Some(f(&mut guard))
    }

    /// Fork from a transcript entry (upstream `fork(entryId, {position})`,
    /// agent-session-runtime.ts:263-346):
    ///
    /// - `session_before_fork` fires first (agent-session-runtime.ts:150-162);
    ///   a `cancel: true` handler result aborts the fork
    ///   ([`ForkEntryOutcome::Cancelled`], not an error);
    /// - `position: Before` (the default) requires a user-message entry,
    ///   branches at its PARENT, and returns the user text as
    ///   `selectedText`; a `None` parent (fork before the first user
    ///   message) starts a fresh session carrying the current one as its
    ///   `parentSession` link (upstream `SessionManager.create` +
    ///   `newSession({parentSession})`);
    /// - `position: At` branches at the entry itself and returns no text.
    ///
    /// pir forks in place (a leaf move inside the same JSONL tree) where
    /// upstream swaps in a branched session file — the same single-file
    /// model [`Self::navigate_tree`] uses. After a successful branch the
    /// `session_tree` event fires with the moved leaf (upstream fires it
    /// from the post-swap session). `Err` when no storage is attached or
    /// the entry id is invalid ("Invalid entry ID for forking", upstream).
    pub async fn fork_entry(
        &self,
        entry_id: &str,
        position: ForkPosition,
    ) -> Result<ForkEntryOutcome, String> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| "no attached session storage; the run stays ephemeral".to_owned())?;
        // `session_before_fork` (upstream emitBeforeFork,
        // agent-session-runtime.ts:150-162): cancel aborts the fork; a
        // `skipConversationRestore: true` result starts the replacement
        // session WITHOUT restoring the source conversation (upstream
        // `SessionBeforeForkResult.skipConversationRestore`, the
        // before_branch checkpoint-hook contract in CHANGELOG: "create the
        // branched session file without restoring conversation messages").
        let mut skip_conversation_restore = false;
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("session_before_fork") {
                let mut payload = serde_json::json!({
                    "type": "session_before_fork",
                    "entryId": entry_id,
                    "position": match position {
                        ForkPosition::Before => "before",
                        ForkPosition::At => "at",
                    },
                });
                let (cancelled, skip) =
                    match plugins.emit("session_before_fork", &mut payload).await {
                        Ok(outcome) => (
                            outcome.result.get("cancel").and_then(JsonValue::as_bool) == Some(true),
                            outcome
                                .result
                                .get("skipConversationRestore")
                                .and_then(JsonValue::as_bool)
                                == Some(true),
                        ),
                        Err(error) => {
                            crate::ext_diag!("[ext] session_before_fork dispatch failed: {error}");
                            (false, false)
                        }
                    };
                if cancelled {
                    return Ok(ForkEntryOutcome::Cancelled);
                }
                skip_conversation_restore = skip;
            }
        }
        // Phase 1 (read-only, under the storage lock): resolve the branch
        // target and the selected text (upstream agent-session-runtime.ts:
        // 267-289).
        let (old_leaf_id, target, selected_text) = {
            let session = session.lock().await;
            let old_leaf_id = session.leaf_id().map(str::to_owned);
            let entry = session
                .entry(entry_id)
                .ok_or_else(|| "Invalid entry ID for forking".to_owned())?;
            match position {
                ForkPosition::At => (old_leaf_id, Some(entry_id.to_owned()), None),
                ForkPosition::Before => {
                    let FileEntry::Message(inner) = entry else {
                        return Err("Invalid entry ID for forking".to_owned());
                    };
                    if inner.message.role() != "user" {
                        return Err("Invalid entry ID for forking".to_owned());
                    }
                    (
                        old_leaf_id,
                        inner.parent_id.clone(),
                        Some(stored_message_text(&inner.message)),
                    )
                }
            }
        };
        // Phase 2: apply. `None` target (fork before the first user message)
        // starts a fresh session with the current one as its parent link
        // (upstream `SessionManager.create` + `newSession({parentSession})`
        // — only when the session is persisted; an in-memory run gets a
        // fresh parent-less storage like upstream's non-persisted path).
        // A handler-requested conversation-restore skip takes the fresh
        // parent-linked session path: pir forks in place, so there is no
        // copied message set to omit — the only faithful equivalent is a new
        // empty transcript that does not restore the source conversation.
        let target = if skip_conversation_restore {
            None
        } else {
            target
        };
        // The outgoing file backs `session_start.previousSessionFile` (and,
        // for the fresh-session arm, the replacement's parent link).
        let previous_session_file = self
            .with_session_manager(|sm| sm.session_file().map(Path::to_path_buf))
            .await
            .flatten();
        match target {
            Some(target) => {
                let new_leaf_id = match self
                    .with_session_manager(|sm| -> Result<Option<String>, String> {
                        sm.branch(&target)
                            .map_err(|error| format!("fork failed: {error}"))?;
                        Ok(sm.leaf_id().map(str::to_owned))
                    })
                    .await
                {
                    Some(Ok(new_leaf_id)) => new_leaf_id,
                    Some(Err(error)) => return Err(error),
                    None => {
                        return Err(
                            "no attached session storage; the run stays ephemeral".to_owned()
                        )
                    }
                };
                // `session_tree` after the in-place leaf move (the same event
                // [`Self::navigate_tree`] fires; upstream fires it from the
                // post-fork session swap).
                if let Some(plugins) = &self.plugins {
                    if session_tree_consumers(plugins) {
                        emit_session_tree(
                            plugins,
                            serde_json::json!({
                                "type": "session_tree",
                                "newLeafId": new_leaf_id,
                                "oldLeafId": old_leaf_id,
                            }),
                        )
                        .await;
                    }
                }
                // Upstream forks through a NEW runtime, so the forked session
                // observes `session_start {reason: "fork"}` even when pir
                // branches in place (no storage swap to carry the lifecycle).
                self.emit_session_start_reason("fork", previous_session_file.as_deref())
                    .await;
            }
            None => {
                let (cwd, session_dir) = self
                    .with_session_manager(|sm| {
                        (sm.cwd().to_owned(), sm.session_dir().to_path_buf())
                    })
                    .await
                    .ok_or_else(|| {
                        "no attached session storage; the run stays ephemeral".to_owned()
                    })?;
                let mut fresh = SessionManager::create(&cwd, &session_dir)
                    .map_err(|error| format!("fork failed: {error}"))?;
                if let Some(parent) = &previous_session_file {
                    fresh
                        .new_session(Some(crate::session_manager::NewSessionOptions {
                            id: None,
                            parent_session: Some(parent.display().to_string()),
                        }))
                        .map_err(|error| format!("fork failed: {error}"))?;
                }
                if !self.swap_session_storage(fresh, Some("fork")).await {
                    return Err("no attached session storage; the run stays ephemeral".to_owned());
                }
            }
        }
        Ok(ForkEntryOutcome::Branched { selected_text })
    }

    /// Deliver the post-replacement `session_start` lifecycle
    /// (upstream `createRuntime`, agent-session-runtime.ts:210-220/251/305):
    /// `{type: "session_start", reason, previousSessionFile?}` with the
    /// replacement session's context already pushed, then consume the lazy
    /// startup dispatch so the next prompt never sends a second one.
    ///
    /// `reason` is `new` / `resume` / `fork`; `previousSessionFile` is the
    /// outgoing session's file when it had one (upstream's field is present
    /// for exactly those three reasons).
    async fn emit_session_start_reason(&self, reason: &str, previous_session_file: Option<&Path>) {
        // The replacement is the live session now: any later prompt must not
        // replay the `startup` lifecycle (same once-flag the prompt path and
        // `reload` consult).
        self.session_start_sent.store(true, Ordering::SeqCst);
        let Some(plugins) = &self.plugins else {
            return;
        };
        // Handlers read the replacement's identity (ctx.sessionId, session
        // path) and may register providers/models, exactly like the startup
        // block in `prompt_loop`.
        let model = self.current_model();
        self.push_session_context(&model).await;
        if plugins.has_handlers("session_start") {
            let mut payload = serde_json::json!({
                "type": "session_start",
                "reason": reason,
            });
            if let Some(previous) = previous_session_file {
                payload["previousSessionFile"] = JsonValue::String(previous.display().to_string());
            }
            if let Err(error) = plugins.emit("session_start", &mut payload).await {
                crate::ext_diag!("[ext] session_start dispatch failed: {error}");
            }
            self.sync_plugin_providers(plugins).await;
            self.sync_plugin_virtual_models(plugins).await;
        }
    }
}

/// Classify a replacement that did not name its operation (the RPC and SDK
/// hosts call [`AgentSession::replace_session_storage`] directly), matching
/// the upstream `SessionStartEvent.reason` of the operation that produced the
/// incoming manager:
///
/// - a manager whose file does not exist yet is a fresh session
///   ([`SessionManager::create`] writes lazily) => `new`;
/// - an incoming header whose `parentSession` is the OUTGOING session's file
///   is a fork (`fork_from_source` / `create_branched_session` /
///   fork-before-first all link the source that way) => `fork`;
/// - every other existing file is an opened session => `resume`.
///
/// Hosts that know the operation (the session-command plane and the in-place
/// fork) pass the reason explicitly instead.
fn infer_session_start_reason(
    incoming: &SessionManager,
    outgoing_file: Option<&Path>,
) -> &'static str {
    let Some(incoming_file) = incoming.session_file() else {
        // In-memory replacement (SDK `reset`): a fresh transcript.
        return "new";
    };
    if !incoming_file.exists() {
        return "new";
    }
    let parent_is_outgoing = match (incoming.parent_session(), outgoing_file) {
        (Some(parent), Some(outgoing)) => {
            let parent_path = Path::new(parent);
            parent_path == outgoing
                || std::fs::canonicalize(parent_path)
                    .ok()
                    .zip(std::fs::canonicalize(outgoing).ok())
                    .is_some_and(|(parent, outgoing)| parent == outgoing)
        }
        _ => false,
    };
    if parent_is_outgoing {
        "fork"
    } else {
        "resume"
    }
}

/// How the swap carries the `session_tree` forest: pre-rendered text when only
/// the JS runtime can observe it, or the nested `Value` a non-JS plugin
/// receives (built iteratively, never through the derived serializer).
enum SessionTreePayload {
    Text(String),
    Value(JsonValue),
}

/// Whether a `session_tree` emission can reach a consumer.
///
/// INTEREST-BASED for native plugins: only a plugin whose
/// [`AgentPlugin::events`](crate::plugins::AgentPlugin::events) declares
/// `session_tree` counts. `PluginHost::emit` still hands every event to every
/// plugin, but the SDK bridge plugin — attached to every SDK agent — declares
/// only `tool_execution_update`; the old `has_non_js_plugins` check made every
/// SDK agent a `session_tree` consumer, materializing (and, before the bridge
/// learned to check the name first, deep-cloning) the whole session forest on
/// every switch. A native plugin that wants `session_tree` must declare it.
///
/// The JS runtime counts only with a registered handler (its snapshot may
/// conservatively say `true` during the registration dirty window — one
/// emission that finds nothing, never a skipped handler).
fn session_tree_consumers(plugins: &PluginHost) -> bool {
    plugins.has_native_handlers("session_tree")
        || plugins
            .js_handle()
            .is_some_and(|js| js.has_handlers("session_tree"))
}

/// Emit the informational `session_tree` event through the reply-free path.
///
/// The prelude's default arm ignores a `session_tree` handler's return value,
/// so no reply is needed: the payload is rendered to text iteratively (the
/// value may carry the deep session forest) and the read-only entry delivers
/// it without a parse/clone/drop echo on the way back. Non-JS plugins receive
/// the value itself, exactly as [`PluginHost::emit`] would hand it to them;
/// the tree is torn down iteratively for the same reason `serde_json`'s drop
/// glue is unsafe at depth.
async fn emit_session_tree(plugins: &PluginHost, mut payload: JsonValue) {
    let payload_json = crate::session_manager::json_depth::write_value_iteratively(&payload);
    plugins
        .emit_read_only_json("session_tree", payload_json, Some(&mut payload))
        .await;
    crate::session_manager::json_depth::drop_value_iteratively(payload);
}
