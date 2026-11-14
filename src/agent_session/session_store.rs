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

    /// Replace the persisted session storage on the live runtime (upstream
    /// `newSession` / `switchSession` / `importFromJsonl`). The cached
    /// session identity refreshes; the next prompt run reads its context from
    /// the replacement transcript. Returns `false` when no storage is
    /// configured (`--no-session`: the run stays ephemeral) or when a
    /// `session_before_switch` handler CANCELLED the switch (upstream
    /// cancel/summary semantics, runner.ts:1080-1118).
    ///
    /// Extension seams: `session_before_switch` fires before the swap with
    /// the upstream operation reason — `new` for a fresh session, `resume`
    /// for an opened file (agent-session-runtime.ts:133-148, :204/:231; the
    /// reason union is `"new" | "resume"`, extensions/types.ts:771-775) — and
    /// a `cancel: true` result aborts it; `session_shutdown` follows with the
    /// same reason (upstream `teardownCurrent("new"|"resume", target)`,
    /// agent-session-runtime.ts:167-178, :212/:245). Upstream emits NEITHER
    /// `session_tree` (only `navigateTree` produces it, agent-session.ts:4200-
    /// 4204) NOR `queue_update` (`teardownCurrent` disposes the outgoing
    /// queues silently and the replacement starts empty, PIR-BIN-049) for a
    /// replacement, so both announcements are off on this seam.
    ///
    /// A file linked to the outgoing session through `parentSession` is NOT a
    /// fork here: switching back to a session you forked FROM is a plain
    /// resume upstream (`switchSession`/`importFromJsonl` hardcode `resume`,
    /// agent-session-runtime.ts:204/:212/:218, :379/:391/:397 — PIR-BIN-110).
    /// Only [`Self::replace_session_storage_forked`], called by the clone /
    /// fork-at-current seam, carries the fork flavor.
    ///
    /// The post-swap lifecycle emits `session_start` with the same upstream
    /// reason (`new`/`resume`, [`infer_session_start_reason`]) and
    /// `previousSessionFile`, then marks the lazy startup dispatch consumed so
    /// the next prompt cannot deliver a second `session_start`.
    pub async fn replace_session_storage(&self, session: SessionManager) -> bool {
        self.replace_session_storage_with_kind(session, false).await
    }

    /// The clone / fork-at-current replacement seam: the same swap, but the
    /// replacement reports the `"fork"` operation reason and keeps the
    /// historical switch-shaped inventory — `session_before_switch{switch}`
    /// plus `session_tree` + `queue_update` — that predates the upstream
    /// fork seam. Upstream routes a fork through `session_before_fork` (never
    /// `session_before_switch`, agent-session-runtime.ts:150-165/:267) and
    /// emits neither tree nor queue; moving these callers onto that seam is
    /// rpc.rs / session_commands.rs plumbing, the recorded PIR-BIN-049
    /// residual.
    pub async fn replace_session_storage_forked(&self, session: SessionManager) -> bool {
        self.replace_session_storage_with_kind(session, true).await
    }

    /// [`Self::replace_session_storage`] with the seam's fork flavor: the
    /// clone / fork-at-current callers are the ONLY ones that want the
    /// legacy switch-shaped pre-swap events plus the two extra announcements;
    /// every operation classified `new`/`resume` emits exactly upstream's
    /// replacement lifecycle.
    async fn replace_session_storage_with_kind(
        &self,
        session: SessionManager,
        fork_at_current: bool,
    ) -> bool {
        // The destination file is resolved BEFORE the swap (`session_shutdown`
        // carries it, upstream `SessionShutdownEvent.targetSessionFile`); it
        // is omitted when the replacement has no persisted file (in-memory
        // target).
        let target_session_file = session
            .session_file()
            .map(|path| path.display().to_string());
        // Upstream carries ONE operation reason through the whole
        // replacement: `newSession` => "new", `switchSession`/`importFromJsonl`
        // => "resume" (agent-session-runtime.ts:204/231/379), delivered to
        // BOTH the pre-swap `session_before_switch` (:133-148) and
        // `session_shutdown` (:167-178, :212/:245). `"switch"` and `"fork"`
        // are not switch-seam reasons. The incoming manager names the
        // operation: a not-yet-written file is a fresh session
        // (`SessionManager::create` writes lazily) => "new"; every existing
        // file — parent-linked or not — is an opened session => "resume"
        // ([`infer_session_start_reason`], the same reason the post-swap
        // `session_start` reports). The fork flavor is the CALLER's intent,
        // never the file shape (PIR-BIN-110).
        let reason = if fork_at_current {
            "fork"
        } else {
            infer_session_start_reason(&session)
        };
        // A `fork`-flavored replacement reaches this seam only from the
        // clone / fork-at-current callers (RPC `fork` without entryId and
        // `clone` -> `fork_current`, JS `ctx.fork` at the current position,
        // TUI `/fork`·`/clone`). Upstream routes a fork through its OWN
        // replacement seam and emits `session_before_fork` — never
        // `session_before_switch` (agent-session-runtime.ts:150-165, :267),
        // with `session_shutdown`/`session_start` reason `"fork"`
        // (:299/:320/:334, :305/:326/:345) and NEITHER `session_tree` NOR
        // `queue_update` (PIR-BIN-049). Moving those callers onto the fork
        // seam is rpc.rs / session_commands.rs plumbing (out of this file's
        // scope), so they keep the historical switch-shaped emission —
        // including the two extra events — until that lands. This is the
        // recorded PIR-BIN-049 residual.
        let fork_flavored = fork_at_current;
        let (pre_swap_reason, emit_session_tree, emit_queue_update) = if fork_flavored {
            ("switch", true, true)
        } else {
            (reason, false, false)
        };
        // `session_before_switch`: a handler may cancel the switch (upstream
        // `SessionBeforeEventResult.cancel` short-circuits the emit loop).
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("session_before_switch") {
                let mut payload = serde_json::json!({
                    "type": "session_before_switch",
                    "reason": pre_swap_reason,
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
            // (agent-session-runtime.ts:167-178).
            plugins
                .emit_session_shutdown(pre_swap_reason, target_session_file.as_deref())
                .await;
        }
        self.swap_session_storage_with(session, Some(reason), emit_session_tree, emit_queue_update)
            .await
    }

    /// [`Self::replace_session_storage`] / [`Self::fork_entry`] swap helper
    /// with explicit control over the two observer announcements a swap can
    /// add beyond the upstream replacement lifecycle:
    ///
    /// - `emit_session_tree`: upstream emits `session_tree` from `navigateTree`
    ///   ONLY (agent-session.ts:4200-4204); an upstream replacement
    ///   (`newSession` / `switchSession` / `fork` → `createRuntime`) emits just
    ///   `session_shutdown {reason}` plus
    ///   `session_start {reason, previousSessionFile}`. Both
    ///   [`Self::fork_entry`] and the `new`/`resume` arms of
    ///   [`Self::replace_session_storage`] pass `false` (PIR-BIN-049).
    /// - `emit_queue_update`: upstream's replacement disposes the outgoing
    ///   session TOGETHER with its steering/follow-up queues
    ///   (`teardownCurrent` → `dispose`, agent-session-runtime.ts:167-178)
    ///   and starts the replacement runtime with empty queues, so no
    ///   `queue_update` is emitted for `new`/`resume`/`fork`; the drain below
    ///   is silent in that case ([`Self::drain_queues_without_update`],
    ///   PIR-BIN-049). Only the fork-flavored incoming arm of
    ///   [`Self::replace_session_storage`] still passes `true` (the
    ///   clone/fork-at-current residual; see the call site).
    async fn swap_session_storage_with(
        &self,
        session: SessionManager,
        reason: Option<&str>,
        emit_session_tree: bool,
        emit_queue_update: bool,
    ) -> bool {
        let Some(slot) = &self.session else {
            return false;
        };
        // Background work belongs to the transcript it was started from: a
        // completion that lands after the swap would persist its
        // `backgroundToolResult` frame into the NEW store (the persist closure
        // holds the session slot, not the manager), attributing a call the new
        // transcript never made. `abort_all` bumps the generation so later
        // reservations deliver nothing. If a tool batch is already open it is
        // the CURRENT run's own barrier (`begin_batch` is not reentrant);
        // waiting on it here deadlocks the swap, so skip the wait — the
        // generation bump still drops that batch's parked completions. With no
        // batch open, take the barrier to order against an in-flight
        // background completion.
        let tracker = self.background.commit_tracker();
        let _swap_barrier = if tracker.batch_is_open() {
            None
        } else {
            Some(tracker.wait_for_batch_boundary().await)
        };
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
        // prompt in the new session receives text queued for the outgoing one.
        // The `new`/`resume` and fork arms drain silently, exactly like
        // upstream's disposed session, whose replacement starts with empty
        // queues and emits no `queue_update` at all (PIR-BIN-049); only the
        // fork-flavored clone residual below still carries the historical
        // `clear_queue` snapshot.
        let cleared = if emit_queue_update {
            self.clear_queue().await
        } else {
            self.drain_queues_without_update().await
        };
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
        let rust_consumers = emit_session_tree
            && self
                .plugins
                .as_ref()
                .is_some_and(|plugins| plugins.has_native_handlers("session_tree"));
        let dispatch_tree = emit_session_tree
            && self
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
        // The upstream `SessionStartEvent.reason`: both live callers pass it
        // explicitly ([`Self::replace_session_storage`] classifies the
        // incoming manager; [`Self::fork_entry`] knows the operation); a
        // caller that passes `None` falls back to the same classification
        // (fresh create / opened file / fork of the outgoing file).
        let start_reason = reason.unwrap_or_else(|| infer_session_start_reason(&session));
        let (session_id, session_path) = {
            let mut slot_guard = slot.lock().await;
            *slot_guard = session;
            // P2-02: read the replacement's identity in the SAME critical
            // section as the assignment and refresh the cache before the
            // (potentially transcript-sized) tree build below. Updating the
            // cache only after the tree walk left a window in which a
            // concurrent run start could still read the OUTGOING id from the
            // cache while the slot already named the replacement (its sink
            // then bound the old id and silently dropped every write). The
            // prompt path now also binds from the manager itself, but the
            // cached identity must never lag the slot by a whole tree build.
            (
                Some(slot_guard.id().to_owned()),
                Some(slot_guard.path().to_path_buf()),
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
        // The transcript behind this slot changed: long-running work that
        // captured the previous generation must not persist into the new
        // session (checked by its recording seam). The per-message persistence
        // sink is pinned to the manager identity bound at run start
        // (settle.rs `persist_message_sink`), so it drops an in-flight old
        // run's `message_end`/tool-result writes once this swap lands instead
        // of appending them to the replacement transcript.
        self.session_storage_generation
            .fetch_add(1, Ordering::SeqCst);
        let (tree, leaf_id) = {
            let identity = slot.lock().await;
            (
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
        // A storage swap attaches a transcript before any prompt: push the
        // same session-context snapshot the pre-prompt path builds so native
        // plugins replay from the NEW entries (the `todo` builtin's
        // `on_session_context`), not the outgoing session's. `session_start`
        // above is what invalidates the todo replay cache, so this order is
        // required (push after it).
        self.push_session_context(&self.current_model()).await;
        true
    }

    /// Drain the steering/follow-up queues WITHOUT emitting `queue_update`
    /// (the fork seam's queue drop, PIR-BIN-049).
    ///
    /// Upstream's fork replaces the whole session runtime: `teardownCurrent`
    /// disposes the outgoing `AgentSession` (its `_steeringMessages` /
    /// `_followUpMessages` die with it, agent-session-runtime.ts:167-177) and
    /// `createRuntime` starts the replacement from a fresh manager with empty
    /// queues (agent-session-runtime.ts:262-350) — no `queue_update` is
    /// delivered for a fork. pir's inbox facade survives the storage swap, so
    /// the swap still has to drop the queued messages (or the first prompt in
    /// the new session would receive text queued for the outgoing one), but
    /// the drop is silent. The bookkeeping mirrors [`Self::clear_queue`] minus
    /// the event emission.
    async fn drain_queues_without_update(&self) -> ClearedQueue {
        let steering = self.steering_inbox.drain_all().await;
        let follow_up = self.follow_up_inbox.drain_all().await;
        let dropped = steering.len() + follow_up.len();
        if dropped > 0 {
            let _ =
                self.pending_messages
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                        Some(current.saturating_sub(dropped))
                    });
        }
        {
            let mut mirror = self
                .queue_mirror
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mirror.steering.clear();
            mirror.follow_up.clear();
        }
        ClearedQueue {
            steering: steering
                .into_iter()
                .map(|prompt| prompt.text_content())
                .collect(),
            follow_up: follow_up
                .into_iter()
                .map(|prompt| prompt.text_content())
                .collect(),
        }
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
                // The entries span is serialized ONCE and moved into the
                // preparation object; `json!` would deep-copy the whole
                // summarized span a second time.
                let entries_json =
                    serde_json::to_value(&collected.entries).unwrap_or(JsonValue::Null);
                let mut preparation = serde_json::Map::with_capacity(7);
                preparation.insert(
                    "targetId".to_owned(),
                    JsonValue::String(target_id.to_owned()),
                );
                preparation.insert(
                    "oldLeafId".to_owned(),
                    old_leaf_id
                        .as_ref()
                        .map_or(JsonValue::Null, |id| JsonValue::String(id.clone())),
                );
                preparation.insert(
                    "commonAncestorId".to_owned(),
                    collected
                        .common_ancestor_id
                        .clone()
                        .map_or(JsonValue::Null, JsonValue::String),
                );
                preparation.insert("entriesToSummarize".to_owned(), entries_json);
                preparation.insert("userWantsSummary".to_owned(), JsonValue::Bool(summarize));
                preparation.insert(
                    "customInstructions".to_owned(),
                    custom_instructions
                        .as_deref()
                        .map_or(JsonValue::Null, |text| JsonValue::String(text.to_owned())),
                );
                preparation.insert(
                    "replaceInstructions".to_owned(),
                    JsonValue::Bool(replace_instructions),
                );
                let mut payload_map = serde_json::Map::with_capacity(3);
                payload_map.insert(
                    "type".to_owned(),
                    JsonValue::String("session_before_tree".to_owned()),
                );
                payload_map.insert("preparation".to_owned(), JsonValue::Object(preparation));
                // Upstream passes a live `AbortSignal`; the JSON wire
                // carries its observable state (the same shape
                // `session_before_compact` uses).
                payload_map.insert("signal".to_owned(), serde_json::json!({ "aborted": false }));
                let mut payload = JsonValue::Object(payload_map);
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
                // SNAP11 P2-S8-4 residual: branch summarization is a session
                // request like any other. Resolve the provider (and any OAuth
                // credential refresh) through THIS session's overlay so a live
                // subagent child sharing the registry cannot redirect the
                // parent's `/tree` summary to its own extension runtime.
                let complete = self.complete_fn_for_signal_session_scoped(&model, signal.clone());
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
        // #11075: the branch the navigation moved to owns the active tool set.
        // Replay its system declarations after the leaf switch; an explicit
        // selection or a branch without a recorded loadout keeps the current
        // set (upstream `_restoreToolsFromTranscript`).
        let branch_history: Vec<Message> = session
            .branch_entries(new_leaf_id_after.as_deref())
            .unwrap_or_default()
            .iter()
            .flat_map(crate::session_manager::projection::session_entry_to_context_messages)
            .collect();
        let summary_entry = summary_entry_id
            .as_deref()
            .and_then(|id| session.entry(id))
            .and_then(|entry| serde_json::to_value(entry).ok());
        drop(session);
        self.restore_branch_active_tools(&branch_history);
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
    /// agent-session-runtime.ts:262-350):
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
    /// A persisted fork creates a NEW session file (upstream
    /// `SessionManager.open(currentSessionFile, sessionDir)` +
    /// `createBranchedSession(targetLeafId)`): the replacement holds only the
    /// root→target path, its header carries `parentSession = <source file>`,
    /// and the source file is never appended to again. The replacement is
    /// swapped in through the fork storage seam, which emits
    /// `session_shutdown {reason:"fork"}` for the outgoing session and
    /// `session_start {reason:"fork", previousSessionFile}` for the new one.
    /// `session_tree` is NOT emitted (upstream's fork swaps a session file;
    /// the tree event belongs to `navigateTree`, PIR-BIN-049).
    ///
    /// `Err` when no storage is attached, the entry id is invalid ("Invalid
    /// entry ID for forking", upstream), or a persisted session has never
    /// been saved ("This session has not been saved yet…", upstream).
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
        // 274-287).
        let (target, selected_text) = {
            let session = session.lock().await;
            let entry = session
                .entry(entry_id)
                .ok_or_else(|| "Invalid entry ID for forking".to_owned())?;
            match position {
                ForkPosition::At => (Some(entry_id.to_owned()), None),
                ForkPosition::Before => {
                    let FileEntry::Message(inner) = entry else {
                        return Err("Invalid entry ID for forking".to_owned());
                    };
                    if inner.message.role() != "user" {
                        return Err("Invalid entry ID for forking".to_owned());
                    }
                    (
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
        // parent-linked session path: the upstream contract creates the
        // branched session file without restoring conversation messages, so
        // the faithful equivalent is a new empty transcript that does not
        // restore the source conversation.
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
                let (source_file, session_dir) = self
                    .with_session_manager(|sm| {
                        (
                            sm.session_file().map(Path::to_path_buf),
                            sm.session_dir().to_path_buf(),
                        )
                    })
                    .await
                    .ok_or_else(|| {
                        "no attached session storage; the run stays ephemeral".to_owned()
                    })?;
                let Some(source_file) = source_file else {
                    // Non-persisted storage: upstream branches the SAME
                    // in-memory manager (`createBranchedSession` returns
                    // `undefined` and replaces the entries in memory), so the
                    // transcript is moved in place and the cached identity
                    // follows the new session id.
                    let identity = self
                        .with_session_manager(|sm| -> Result<(String, Option<PathBuf>), String> {
                            sm.create_branched_session(&target)
                                .map_err(|error| format!("fork failed: {error}"))?;
                            Ok((sm.id().to_owned(), sm.session_file().map(Path::to_path_buf)))
                        })
                        .await
                        .ok_or_else(|| {
                            "no attached session storage; the run stays ephemeral".to_owned()
                        })??;
                    // The branch is a different transcript: background work
                    // started on the old one must not land in it (same
                    // generation guard the storage swap applies).
                    self.background.abort_all();
                    self.abort_bash();
                    self.session_storage_generation
                        .fetch_add(1, Ordering::SeqCst);
                    *self
                        .session_id
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity.0);
                    *self
                        .session_path
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = identity.1;
                    // Upstream's `teardownCurrent` disposes the outgoing
                    // session, so the steering/follow-up queues queued for
                    // the outgoing transcript die with it and the branch
                    // starts empty — silently, with no `queue_update`
                    // (PIR-BIN-049). pir's inbox facade survives the
                    // in-memory branch, so drop them here too.
                    let cleared = self.drain_queues_without_update().await;
                    if !cleared.steering.is_empty() || !cleared.follow_up.is_empty() {
                        tracing::debug!(
                            steering = cleared.steering.len(),
                            follow_up = cleared.follow_up.len(),
                            "dropped queued steering/follow-up messages on a non-persisted fork"
                        );
                    }
                    if let Some(plugins) = &self.plugins {
                        plugins.emit_session_shutdown("fork", None).await;
                    }
                    self.emit_session_start_reason("fork", previous_session_file.as_deref())
                        .await;
                    return Ok(ForkEntryOutcome::Branched { selected_text });
                };
                // Persisted (upstream agent-session-runtime.ts:312-320):
                // reopen the CURRENT file as a second manager and branch the
                // root→target path into a NEW session file. Reading the file
                // (not the live manager) also makes `/fork` work on a paged
                // session, whose resident window is not the session.
                if !source_file.exists() {
                    return Err("This session has not been saved yet. \
                                Send a message before cloning or forking it."
                        .to_owned());
                }
                let mut branched =
                    SessionManager::open_with(&source_file, Some(&session_dir), None)
                        .map_err(|error| format!("fork failed: {error}"))?;
                branched
                    .create_branched_session(&target)
                    .map_err(|error| format!("fork failed: {error}"))?;
                // Upstream `teardownCurrent("fork", sessionManager.getSessionFile())`
                // runs AFTER the new file exists and names it as the
                // destination. The outgoing session is still live here, so a
                // plugin persisting from the shutdown handler writes the
                // OUTGOING file; the post-swap `session_start {reason:"fork"}`
                // describes the replacement.
                let target_session_file = branched
                    .session_file()
                    .map(|path| path.display().to_string());
                if let Some(plugins) = &self.plugins {
                    plugins
                        .emit_session_shutdown("fork", target_session_file.as_deref())
                        .await;
                }
                // Upstream `createBranchedSession` + `createRuntime` emits
                // neither `session_tree` nor `queue_update` for a fork: the
                // outgoing session (and its queues) is disposed before the
                // replacement runtime starts (PIR-BIN-049).
                if !self
                    .swap_session_storage_with(branched, Some("fork"), false, false)
                    .await
                {
                    return Err("no attached session storage; the run stays ephemeral".to_owned());
                }
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
                // The fresh file is the replacement destination upstream
                // `teardownCurrent` receives (`sessionManager.getSessionFile()`).
                let target_session_file =
                    fresh.session_file().map(|path| path.display().to_string());
                if let Some(plugins) = &self.plugins {
                    plugins
                        .emit_session_shutdown("fork", target_session_file.as_deref())
                        .await;
                }
                // Fresh parent-linked fork arm: same upstream replacement
                // shape as the branched file (no `session_tree`, no
                // `queue_update`, PIR-BIN-049).
                if !self
                    .swap_session_storage_with(fresh, Some("fork"), false, false)
                    .await
                {
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
/// the upstream operation reason of the manager that produced the incoming
/// storage. The same classification now feeds BOTH the pre-swap
/// `session_before_switch` / `session_shutdown` reasons and the post-swap
/// `SessionStartEvent.reason` (upstream carries one reason across the whole
/// replacement, agent-session-runtime.ts:133-148, :167-178, :204/:231):
///
/// - a manager whose file does not exist yet is a fresh session
///   ([`SessionManager::create`] writes lazily) => `new`;
/// - every existing file is an opened session => `resume`. This includes a
///   file linked to the outgoing session through `parentSession`: switching
///   back to a session you forked FROM is a plain resume upstream
///   (`switchSession`/`importFromJsonl` hardcode `resume`,
///   agent-session-runtime.ts:204/:212/:218, :379/:391/:397). The fork flavor
///   is the clone / fork-at-current CALLER's intent
///   ([`AgentSession::replace_session_storage_forked`]), never the file shape
///   (PIR-BIN-110).
///
/// Hosts that know the operation (the session-command plane and the entry
/// fork) pass the reason explicitly instead.
fn infer_session_start_reason(incoming: &SessionManager) -> &'static str {
    let Some(incoming_file) = incoming.session_file() else {
        // In-memory replacement (SDK `reset`): a fresh transcript.
        return "new";
    };
    if !incoming_file.exists() {
        return "new";
    }
    "resume"
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
