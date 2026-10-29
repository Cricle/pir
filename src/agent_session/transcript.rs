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
        let messages: Vec<Message> = page
            .entries()
            .iter()
            .flat_map(|entry| {
                crate::session_manager::projection::session_entry_to_context_messages(entry)
            })
            .collect();
        Ok(Some((messages, page.page_start())))
    }

    /// The cursor [`AgentSession::older_transcript_messages`] continues from:
    /// the oldest resident entry of a paged session, `None` otherwise.
    pub async fn older_transcript_cursor(&self) -> Option<u64> {
        let session = self.session.as_ref()?;
        session.lock().await.prefix_cursor()
    }

    /// The transcript as RPC/`get_messages` sees it (upstream
    /// `session.messages`): the current branch projection for persisted
    /// sessions, the in-memory history for ephemeral ones.
    pub async fn transcript(&self) -> Vec<Message> {
        match &self.session {
            Some(session) => session
                .lock()
                .await
                .build_session_projection()
                .map(|projection| projection.messages)
                .unwrap_or_default(),
            None => history_owned(self.history_snapshot()),
        }
    }

    /// The transcript as a shared snapshot: zero-copy for ephemeral sessions
    /// ([`Self::history_snapshot`]); the session branch wraps its freshly
    /// built projection. Read paths that only iterate or measure use this.
    pub(super) async fn transcript_snapshot(&self) -> Arc<Vec<Message>> {
        match &self.session {
            Some(session) => Arc::new(
                session
                    .lock()
                    .await
                    .build_session_projection()
                    .map(|projection| projection.messages)
                    .unwrap_or_default(),
            ),
            None => self.history_snapshot(),
        }
    }

    /// [`Self::transcript`] serialized to upstream `AgentMessage` JSON.
    pub async fn transcript_json(&self) -> Vec<JsonValue> {
        self.transcript_snapshot()
            .await
            .iter()
            .map(message_json)
            .collect()
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
        // The projection owns its `Arc<FileEntry>` sources, so the session
        // lock is released before anything is serialized.
        let projection = {
            let manager = session.lock().await;
            manager.build_session_projection()
        };
        let Ok(projection) = projection else {
            return Vec::new();
        };
        // Latest context edit per target, over the same retained entries the
        // projection selected (upstream `buildSessionProjection`).
        let mut edits: std::collections::HashMap<&str, &crate::session_manager::ContextEditEntry> =
            std::collections::HashMap::new();
        for projected in &projection.entries {
            if let crate::session_manager::FileEntry::ContextEdit(edit) =
                projected.source_entry.as_ref()
            {
                edits.insert(edit.target_id.as_str(), edit);
            }
        }
        let mut messages: Vec<JsonValue> = Vec::new();
        for (index, projected) in projection.entries.iter().enumerate() {
            let entry = projected.source_entry.as_ref();
            // Only the newest compaction contributes (the stale ones retained
            // inside its kept range contribute nothing), like the projection.
            if index > 0 && matches!(entry, crate::session_manager::FileEntry::Compaction(_)) {
                continue;
            }
            let edit = entry.id().and_then(|id| edits.get(id).copied());
            messages.extend(full_entry_messages(entry, edit));
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
fn full_entry_messages(
    entry: &crate::session_manager::FileEntry,
    edit: Option<&crate::session_manager::ContextEditEntry>,
) -> Vec<JsonValue> {
    use crate::session_manager::{FileEntry, StoredMessage};
    match entry {
        FileEntry::Message(inner) => {
            // Deviation: excluded `!!` records stay out of this view (see
            // `full_messages_json`).
            if let StoredMessage::BashExecution(bash) = &inner.message {
                if bash.excluded() {
                    return Vec::new();
                }
            }
            let Ok(mut value) = serde_json::to_value(&inner.message) else {
                return Vec::new();
            };
            if let Some(edit) = edit {
                if !apply_context_edit(&mut value, edit) {
                    return Vec::new();
                }
            }
            vec![value]
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
                    return Vec::new();
                }
            }
            vec![value]
        }
        FileEntry::BranchSummary(inner) => {
            if inner.summary.is_empty() {
                return Vec::new();
            }
            vec![serde_json::json!({
                "role": "branchSummary",
                "summary": &inner.summary,
                "fromId": &inner.from_id,
                "timestamp": crate::session_manager::ms_from_iso(&inner.timestamp)
                    .unwrap_or(0),
            })]
        }
        FileEntry::Compaction(inner) => {
            let mut messages = Vec::new();
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
            messages
        }
        // State entries (usage, label, custom, model/thinking changes,
        // session_info, context_edit): no message record.
        _ => Vec::new(),
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
