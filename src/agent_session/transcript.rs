use super::*;

impl AgentSession {
    /// Snapshot of model/thinking/turn/token state.
    pub fn state(&self) -> SessionSnapshot {
        let model = self.current_model();
        SessionSnapshot {
            model: model.id.clone(),
            provider: model.provider.clone(),
            thinking: self.current_thinking(),
            turns: self.turns.load(Ordering::SeqCst),
            tokens: self.tokens.load(Ordering::SeqCst),
            streaming: self.is_streaming(),
            session_id: self
                .session_id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            session_path: self
                .session_path
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }

    // ------------------------------------------------------------------
    // State accessors (upstream AgentSession getters used by RPC mode)
    // ------------------------------------------------------------------

    /// Native status-line entries published by this session's plugin host
    /// ([`crate::plugins::AgentPlugin::statuses`]), in plugin registration
    /// order then key.
    ///
    /// This is the host-side status snapshot: it reflects every enabled Rust
    /// builtin (the `phone` builtin's live `📱 ●/○ phone ...` entry included)
    /// whether or not a JS extension runtime is attached. JS extensions keep
    /// their `ctx.ui.setStatus` notifications on the runtime's notification
    /// channel; merge the two with
    /// [`crate::plugins::host::merge_statuses`] when assembling a footer.
    /// Empty when the session has no plugin host (no extensions/inline
    /// plugins). Poll it at status-refresh time, never per rendered frame.
    pub fn plugin_statuses(&self) -> Vec<(String, String)> {
        self.plugins
            .as_ref()
            .map(|plugins| plugins.statuses())
            .unwrap_or_default()
    }

    /// The current model (upstream `session.model`).
    pub fn model(&self) -> Model {
        self.current_model()
    }

    /// Push the live session-context snapshot once, for hosts that attach an
    /// existing transcript outside the prompt path (`--continue` startup,
    /// `/resume`, `/import`).
    ///
    /// [`Self::push_session_context`] runs before every prompt and after every
    /// run; the attach itself had no push, so native plugins that replay
    /// persisted state from `entries` (the `todo` builtin) or bind the session
    /// identity (the `memory` builtin) stayed empty until the first prompt.
    /// The snapshot is the same one the pre-prompt path builds: `entries`,
    /// `session_id`, `session_path`, `cwd` and the current model.
    pub(crate) async fn push_session_context_on_attach(&self) -> bool {
        self.push_session_context(&self.model()).await
    }

    /// The requested thinking level (upstream `session.thinkingLevel`).
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.current_thinking()
    }

    /// The persisted session id, when the session is backed by storage.
    /// Interior-mutable: refreshed by [`Self::replace_session_storage`].
    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The session file path, when the session is backed by storage.
    /// Interior-mutable: refreshed by [`Self::replace_session_storage`].
    pub fn session_path(&self) -> Option<PathBuf> {
        self.session_path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The current session name (upstream `session.sessionName`), if any.
    pub async fn session_name(&self) -> Option<String> {
        match &self.session {
            Some(session) => {
                let session = session.lock().await;
                session.session_name()
            }
            None => self
                .session_name
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// Set the session display name (upstream `/name` →
    /// `sessionManager.appendSessionInfo`). Persisted when the session has
    /// storage, kept in memory otherwise. Returns the normalized name the
    /// session reports back (upstream warns when it differs from the input).
    pub async fn set_session_name(&self, name: &str) -> crate::Result<Option<String>> {
        self.set_session_name_opt(Some(name)).await
    }

    /// The ONE session-name mutator (upstream `setSessionName`,
    /// agent-session.ts:4002-4006): append the `session_info` entry, then emit
    /// `session_info_changed` on BOTH faces — the session event subscribers
    /// and the extension event — carrying the name the session manager now
    /// reports (normalized: newlines collapsed, trimmed, empty = cleared).
    /// `None` clears the name.
    ///
    /// Every entry point (TUI `/name`, RPC `set_session_name`, the SDK and the
    /// extension `pi.setSessionName`) funnels here, so the extension payload
    /// can never diverge from the persisted name.
    pub async fn set_session_name_opt(&self, name: Option<&str>) -> crate::Result<Option<String>> {
        // The lock is released before the emit: a synchronous subscriber must
        // not observe (or deadlock on) the storage lock.
        let final_name = match self.session.as_ref() {
            Some(session) => {
                let mut session = session.lock().await;
                session.set_session_name(name)?;
                session.session_name()
            }
            None => {
                let value = name.map(str::to_owned);
                *self
                    .session_name
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = value.clone();
                value
            }
        };
        // Upstream `session_info_changed` (agent-session.ts:4004) is emitted on
        // the session event stream AND through the extension runner, both with
        // the manager-reported name.
        self.emit_session_event(SessionEvent::SessionInfoChanged {
            name: final_name.clone(),
        });
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("session_info_changed") {
                let mut payload = serde_json::json!({ "type": "session_info_changed" });
                // `name` rides only when present (upstream omits `undefined`).
                if let Some(name) = &final_name {
                    payload["name"] = serde_json::json!(name);
                }
                emit_plugin_event(plugins, "session_info_changed", payload).await;
            }
        }
        Ok(final_name)
    }

    /// ONE OLDER chunk of a PAGED session's skipped prefix, in
    /// branch-projection form (`Message`s), plus the cursor to continue from
    /// (the TUI's "load more while scrolling up" seam).
    ///
    /// `cursor` comes from [`AgentSession::older_transcript_cursor`] and moves
    /// strictly older with every call; `None` means "not a paged session" or
    /// "reached the file start". The page is parsed with the byte/entry caps
    /// the caller passes (never a whole-prefix scan), and its entries are
    /// projected with the SAME per-entry rules the live transcript uses
    /// (`session_entry_to_context_messages`), so older blocks are exactly what
    /// a full load would show for that range — no compaction/context-edit
    /// interpretation is invented here (a paged session whose prefix holds a
    /// context edit is refused by the compaction path, and the TUI shows the
    /// windowed band only).
    pub async fn older_transcript_messages(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Option<(Vec<Message>, u64)>> {
        let Some(session) = self.session.as_ref() else {
            return Ok(None);
        };
        let session = session.lock().await;
        let Some(page) = session.read_older_page(cursor, max_bytes, max_entries)? else {
            return Ok(None);
        };
        // Push-based projection: one shared aggregate instead of a throwaway
        // `Vec<Message>` per entry (`flat_map` over per-entry collects
        // allocated and freed one small Vec per entry — a page of ~750 entries
        // paid ~750 allocations per load-more step).
        let mut messages: Vec<Message> = Vec::new();
        for entry in page.entries() {
            crate::session_manager::projection::push_entry_context_messages(entry, &mut messages);
        }
        Ok(Some((messages, page.page_start())))
    }

    /// The cursor [`AgentSession::older_transcript_messages`] continues from:
    /// the oldest resident entry of a paged session, `None` otherwise.
    pub async fn older_transcript_cursor(&self) -> Option<u64> {
        let session = self.session.as_ref()?;
        session.lock().await.prefix_cursor()
    }

    /// Ask the session manager to parse the page at `cursor` on a background
    /// thread so the next [`AgentSession::older_transcript_messages`] call is
    /// a cache hit. Returns whether a prefetch was started (a repeat call for
    /// the same cursor/caps is deduped by the page cache). The UI's near-top
    /// scroll trigger calls this; it never parses on the caller's thread.
    pub async fn prefetch_older_transcript(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let session = session.lock().await;
        session.prefetch_older_page(cursor, max_bytes, max_entries)
    }

    /// Whether the page at `cursor` is already in the manager's page cache
    /// (test/diagnostic; useful to wait for a prefetch without sleeping on a
    /// fixed timeout).
    pub async fn is_older_transcript_cached(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        session
            .lock()
            .await
            .is_older_page_cached(cursor, max_bytes, max_entries)
    }

    /// Cache counters of a paged session, when this build has the paged reader.
    #[cfg(feature = "session")]
    pub async fn older_transcript_cache_stats(
        &self,
    ) -> Option<crate::session_manager::PageCacheStats> {
        let session = self.session.as_ref()?;
        session.lock().await.older_page_cache_stats()
    }

    /// The transcript as RPC/`get_messages` sees it (upstream
    /// `session.messages`): the current branch projection for persisted
    /// sessions, the in-memory history for ephemeral ones.
    pub async fn transcript(&self) -> Vec<Message> {
        match &self.session {
            Some(session) => {
                let mut session = session.lock().await;
                // A live-tail session is bounded only BETWEEN prompts, and a
                // session opened tail-first (`SessionMode::Existing` of a
                // >page-size file) holds a 1024-entry window until the first
                // prompt widens it. Both are PAGED windows: a public
                // whole-history read must widen them, or it silently returns
                // the resident page (PIR-SDK-018 / PIR-SDK-025).
                #[cfg(feature = "session")]
                if session.is_paged() {
                    let _ = session.widen_to_full();
                }
                // Messages-only projection: the public return type is an
                // owned `Vec<Message>` (one copy is required), but the
                // per-entry provenance metadata (`Arc` bump + Range per
                // retained entry) of `build_session_projection` is not.
                session.build_session_messages()
            }
            None => history_owned(self.history_snapshot()),
        }
    }

    /// [`Self::transcript`] serialized to upstream `AgentMessage` JSON.
    ///
    /// MEMORY SHAPE: a persisted session walks its retained entries and
    /// serializes each plain stored message by reference
    /// ([`crate::session_manager::projection::build_session_context_json`]) —
    /// the old shape built the full projection (`Vec<Message>`, a clone of
    /// every message) only to serialize and drop it. An ephemeral session
    /// serializes its shared history snapshot directly, without the
    /// `history_owned` copy.
    pub async fn transcript_json(&self) -> Vec<JsonValue> {
        match &self.session {
            Some(session) => {
                let mut manager = session.lock().await;
                // Whole-history JSON read: widen both live-trimmed and
                // tail-first paged managers (PIR-SDK-025).
                #[cfg(feature = "session")]
                if manager.is_paged() {
                    let _ = manager.widen_to_full();
                }
                crate::session_manager::projection::build_session_context_json(
                    manager.resident_entries(),
                    manager.leaf_id(),
                )
            }
            None => self.history_snapshot().iter().map(message_json).collect(),
        }
    }

    /// The COMPLETE session message view (upstream `session.messages`): the
    /// compaction-aware branch projection with every message record in its
    /// own upstream role, not the LLM-flattened projection.
    ///
    /// Unlike [`Self::transcript_json`] (the `convertToLlm`-style view, where
    /// custom records become user text and summaries are framed user
    /// messages), this returns the records upstream keeps in
    /// `agent.state.messages`:
    ///
    /// - `message` entries verbatim — `user`/`assistant`/`toolResult`/
    ///   `system`, plus the custom roles `bashExecution` and `custom` a
    ///   session file can carry;
    /// - `custom_message` entries as `{role:"custom", customType, content,
    ///   display, details?, timestamp}` (extension `pi.sendMessage` records,
    ///   including non-`display` UI messages);
    /// - `branch_summary` entries as `{role:"branchSummary", summary,
    ///   fromId, timestamp}` and the newest `compaction` entry as its stored
    ///   `system` replay plus `{role:"compactionSummary", summary,
    ///   tokensBefore, timestamp}`.
    ///
    /// Context edits apply exactly as upstream (`projectContextEntry`): a
    /// `replacement: null` omits the target, and a replacement `content`
    /// rewrites user/assistant/toolResult/custom records.
    ///
    /// Deviation (pinned by `tests/rpc_bash_record_fix.rs`): a `!!`
    /// `excludeFromContext` bash record stays OUT of this view. Upstream's
    /// `session.messages` carries it and only `convertToLlm` filters it; pir's
    /// RPC `get_messages` has always answered the context view, so the
    /// excluded record stays reachable through `get_entries`/the TUI instead
    /// of suddenly appearing in an RPC client's message list.
    ///
    /// An ephemeral (`--no-session`) session has only its in-memory history;
    /// that history is already the LLM projection, so it is serialized as-is.
    pub async fn full_messages_json(&self) -> Vec<JsonValue> {
        let Some(session) = self.session.as_ref() else {
            return history_owned(self.history_snapshot())
                .iter()
                .map(message_json)
                .collect();
        };
        // Only the projection's per-entry METADATA is needed: this view
        // re-serializes every retained source entry itself, so materializing
        // `SessionProjection::messages` would deep-clone the whole
        // conversation and then drop it unused. The metadata walk applies the
        // identical leaf-path / retention / context-edit rules and hands back
        // the manager's `Arc<FileEntry>` handles (a refcount bump each), so
        // the session lock is released before anything is serialized.
        let metas = {
            let mut manager = session.lock().await;
            // Whole-history view: widen live-trimmed AND tail-first paged
            // managers (PIR-SDK-025).
            #[cfg(feature = "session")]
            if manager.is_paged() {
                let _ = manager.widen_to_full();
            }
            manager.projection_entry_meta()
        };
        // Latest context edit per target, over the same retained entries the
        // projection selected (upstream `buildSessionProjection`).
        let mut edits: std::collections::HashMap<&str, &crate::session_manager::ContextEditEntry> =
            std::collections::HashMap::new();
        for meta in &metas {
            if let crate::session_manager::FileEntry::ContextEdit(edit) = meta.source_entry.as_ref()
            {
                edits.insert(edit.target_id.as_str(), edit);
            }
        }
        // `message_count` is exactly the number of records
        // `full_entry_messages` emits for the same entry/edit pair (the
        // contract `projected_message_count` pins), so this is the exact
        // capacity — no growth reallocations on a long transcript.
        let mut messages: Vec<JsonValue> =
            Vec::with_capacity(metas.iter().map(|meta| meta.message_count).sum());
        for (index, meta) in metas.iter().enumerate() {
            let entry = meta.source_entry.as_ref();
            // Only the newest compaction contributes (the stale ones retained
            // inside its kept range contribute nothing), like the projection.
            if index > 0 && matches!(entry, crate::session_manager::FileEntry::Compaction(_)) {
                continue;
            }
            let edit = entry.id().and_then(|id| edits.get(id).copied());
            push_full_entry_messages(entry, edit, &mut messages);
        }
        messages
    }

    /// Number of messages in the current transcript (upstream
    /// `get_state.messageCount`).
    ///
    /// Uses the projection's per-entry METADATA (shared source entries +
    /// contributed message counts) instead of [`Self::transcript_snapshot`]:
    /// the count never needs the message bodies, so a transcript-sized
    /// session is not materialized just to be measured.
    pub async fn message_count(&self) -> usize {
        match &self.session {
            Some(session) => session
                .lock()
                .await
                .projection_entry_meta()
                .iter()
                .map(|entry| entry.message_count)
                .sum(),
            None => self.history_snapshot().len(),
        }
    }

    /// Queued-but-not-yet-delivered steering/follow-up messages (upstream
    /// `session.pendingMessageCount`).
    pub fn pending_message_count(&self) -> usize {
        self.pending_messages.load(Ordering::SeqCst)
    }

    /// The text of the last assistant message (upstream
    /// `getLastAssistantText`): aborted messages without content are skipped,
    /// and an all-whitespace result reads as `None`.
    ///
    /// Persisted sessions walk the retained session entries instead of
    /// materializing the transcript projection: this is a poll-path read (RPC
    /// `get_last_assistant_text` / the state snapshot) that only needs one
    /// string, and building the whole conversation for it was a
    /// transcript-sized copy per poll.
    pub async fn last_assistant_text(&self) -> Option<String> {
        let Some(session) = &self.session else {
            return last_assistant_text_in(&self.history_snapshot());
        };
        let manager = session.lock().await;
        manager.last_assistant_text()
    }
}

/// The message-list walk behind [`AgentSession::last_assistant_text`]
/// (ephemeral history).
fn last_assistant_text_in(messages: &[Message]) -> Option<String> {
    for message in messages.iter().rev() {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        if assistant.stop_reason == StopReason::Aborted && assistant.content.is_empty() {
            continue;
        }
        let text = assistant
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
        return None;
    }
    None
}

/// One retained session entry as upstream `session.messages` records
/// (`sessionEntryToContextMessages`): message records verbatim, custom
/// messages and summaries in their own roles.
fn push_full_entry_messages(
    entry: &crate::session_manager::FileEntry,
    edit: Option<&crate::session_manager::ContextEditEntry>,
    messages: &mut Vec<JsonValue>,
) {
    use crate::session_manager::{FileEntry, StoredMessage};
    match entry {
        FileEntry::Message(inner) => {
            // Deviation: excluded `!!` records stay out of this view (see
            // `full_messages_json`).
            if let StoredMessage::BashExecution(bash) = &inner.message {
                if bash.excluded() {
                    return;
                }
            }
            let Ok(mut value) = serde_json::to_value(&inner.message) else {
                return;
            };
            if let Some(edit) = edit {
                if !apply_context_edit(&mut value, edit) {
                    return;
                }
            }
            messages.push(value);
        }
        FileEntry::CustomMessage(inner) => {
            let mut value = serde_json::json!({
                "role": "custom",
                "customType": &inner.custom_type,
                "content": &inner.content,
                "display": inner.display,
                "timestamp": crate::session_manager::ms_from_iso(&inner.timestamp)
                    .unwrap_or(0),
            });
            if let Some(details) = &inner.details {
                value["details"] = details.clone();
            }
            if let Some(edit) = edit {
                if !apply_context_edit(&mut value, edit) {
                    return;
                }
            }
            messages.push(value);
        }
        FileEntry::BranchSummary(inner) => {
            if inner.summary.is_empty() {
                return;
            }
            messages.push(serde_json::json!({
                "role": "branchSummary",
                "summary": &inner.summary,
                "fromId": &inner.from_id,
                "timestamp": crate::session_manager::ms_from_iso(&inner.timestamp)
                    .unwrap_or(0),
            }));
        }
        FileEntry::Compaction(inner) => {
            // The checkpoint's stored system message replays first (upstream
            // `entry.systemMessage ? [entry.systemMessage, summary] : [summary]`).
            if let Some(system) = &inner.system_message {
                if let Ok(value) = serde_json::to_value(system) {
                    messages.push(value);
                }
            }
            messages.push(serde_json::json!({
                "role": "compactionSummary",
                "summary": &inner.summary,
                "tokensBefore": inner.tokens_before,
                "timestamp": crate::session_manager::ms_from_iso(&inner.timestamp)
                    .unwrap_or(0),
            }));
        }
        // State entries (usage, label, custom, model/thinking changes,
        // session_info, context_edit): no message record.
        _ => {}
    }
}

/// Apply one context edit to a full-view record (upstream
/// `projectContextEntry`): `false` means the record is omitted
/// (`replacement: null`); otherwise the replacement content is written into
/// the editable roles only (`user`/`assistant`/`toolResult`/`custom`), with a
/// string replacement on assistant/toolResult wrapped as one text block.
fn apply_context_edit(
    message: &mut JsonValue,
    edit: &crate::session_manager::ContextEditEntry,
) -> bool {
    let Some(replacement) = &edit.replacement else {
        return false;
    };
    let role = message
        .get("role")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_owned();
    if !matches!(
        role.as_str(),
        "user" | "assistant" | "toolResult" | "custom"
    ) {
        return true;
    }
    let content =
        if matches!(role.as_str(), "assistant" | "toolResult") && replacement.content.is_string() {
            JsonValue::Array(vec![serde_json::json!({
                "type": "text",
                "text": replacement.content.as_str().unwrap_or_default(),
            })])
        } else {
            replacement.content.clone()
        };
    if let Some(object) = message.as_object_mut() {
        object.insert("content".to_owned(), content);
    }
    true
}
