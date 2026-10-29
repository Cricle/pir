//! Projection semantics, ported from upstream
//! `packages/coding-agent/src/core/session-manager.ts`
//! (`buildSessionPath`, `buildContextEntries`, `buildSessionProjection`,
//! `sessionEntryToContextMessages`, `getSessionContextSettings`).
//!
//! The projection is the compaction-aware, context-edit-aware view of the
//! session that the LLM sees: the newest compaction on the leaf path
//! contributes its summary (plus its stored system message) and only its
//! retained tail; `context_edit` entries replace or omit their target's
//! contribution; state entries (usage, labels, model changes, ...) contribute
//! nothing.

use std::borrow::Borrow;
use std::ops::Range;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use super::types::{ContextEditEntry, FileEntry, StoredMessage, StoredSystemMessage};
use crate::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};

/// One projected session entry (upstream `ProjectedSessionEntry`).
///
/// MEMORY SHAPE (pir extension over the TS shape): the entry does NOT own its
/// model-visible messages; it holds the [`Range`] they occupy inside
/// [`SessionProjection::messages`] and shares the raw session entry through an
/// `Arc`. A projection therefore materializes the conversation exactly ONCE
/// (the flat aggregate) instead of once per entry plus once for the aggregate
/// — the per-entry `Vec<Message>` and the `FileEntry` clone this type used to
/// carry were a second and third full copy of a large session.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedSessionEntry {
    /// Raw append-only entry that owns this projected contribution (shared
    /// with the session manager's entry list — a refcount bump, never a
    /// clone of the stored messages).
    pub source_entry: Arc<FileEntry>,
    /// Range of this entry's model-visible messages inside
    /// [`SessionProjection::messages`]. Empty for state-only entries,
    /// omissions, and non-newest compaction entries.
    pub messages: Range<usize>,
}

impl ProjectedSessionEntry {
    /// Number of model-visible messages this entry contributes.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// This entry's contribution, sliced out of the projection's aggregate
    /// message list. Saturating: a range beyond the aggregate yields an empty
    /// slice instead of panicking.
    pub fn messages_in<'a>(&self, projection_messages: &'a [Message]) -> &'a [Message] {
        let end = self.messages.end.min(projection_messages.len());
        let start = self.messages.start.min(end);
        &projection_messages[start..end]
    }
}

/// The active, compaction-aware session view (upstream `SessionProjection`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProjection {
    pub entries: Vec<ProjectedSessionEntry>,
    pub messages: Vec<Message>,
    pub thinking_level: String,
    pub model: Option<(String, String)>,
}

impl SessionProjection {
    /// The model-visible messages of one projected entry, borrowed from the
    /// aggregate list.
    pub fn entry_messages(&self, index: usize) -> &[Message] {
        match self.entries.get(index) {
            Some(entry) => entry.messages_in(&self.messages),
            None => &[],
        }
    }

    /// The per-entry message views paired with their source entries — the
    /// borrowed view the compaction basis walks (no message clone).
    pub fn message_slices(&self) -> impl Iterator<Item = (&FileEntry, Range<usize>)> {
        self.entries
            .iter()
            .map(|entry| (entry.source_entry.as_ref(), entry.messages.clone()))
    }
}

/// Framing constants from upstream `core/messages.ts` (`convertToLlm` wraps
/// the custom summary roles into user text messages with these markers; pir
/// stores the framed text directly since its `Message` union has no
/// `compactionSummary`/`branchSummary` roles).
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Project one selected session entry into model messages (upstream
/// `sessionEntryToContextMessages`, with the custom summary roles rendered as
/// framed user text per pir's `Message` union).
pub fn session_entry_to_context_messages(entry: &FileEntry) -> Vec<Message> {
    match entry {
        // A `!!` bash record stays in the transcript but not in the model
        // context (upstream `convertToLlm` skips `excludeFromContext`).
        FileEntry::Message(inner) => match &inner.message {
            StoredMessage::BashExecution(bash) if bash.excluded() => Vec::new(),
            _ => vec![inner.message.clone().into()],
        },
        FileEntry::CustomMessage(inner) => {
            vec![custom_context_message(&inner.content, entry.timestamp())]
        }
        FileEntry::BranchSummary(inner) => {
            if inner.summary.is_empty() {
                Vec::new()
            } else {
                vec![framed_user_message(
                    BRANCH_SUMMARY_PREFIX,
                    &inner.summary,
                    BRANCH_SUMMARY_SUFFIX,
                    entry.timestamp(),
                )]
            }
        }
        FileEntry::Compaction(inner) => {
            let summary = framed_user_message(
                COMPACTION_SUMMARY_PREFIX,
                &inner.summary,
                COMPACTION_SUMMARY_SUFFIX,
                entry.timestamp(),
            );
            // Upstream replays the stored checkpoint system message as-is
            // (`entry.systemMessage`), including its named sections and tool
            // declarations — a resumed session restores its prompt sections
            // and active tool loadout from this entry.
            match &inner.system_message {
                Some(system) => vec![system.to_message(), summary],
                None => vec![summary],
            }
        }
        // State entries (usage, label, custom, model/thinking changes,
        // session_info, context_edit): no context contribution.
        _ => Vec::new(),
    }
}

/// Render a custom-message entry's content (string or text-block array) as
/// plain text (upstream `contentText`).
pub(crate) fn custom_message_text(content: &JsonValue) -> String {
    match content {
        JsonValue::String(text) => text.clone(),
        JsonValue::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn custom_context_message(content: &JsonValue, timestamp: &str) -> Message {
    // Upstream keeps role "custom" and lets `convertToLlm` turn it into a user
    // message while PRESERVING its content blocks (text AND images). The phone
    // child's interleaved `phone-inline-user-message` relies on that: flattening
    // to text here dropped the attached images from the turn's model context.
    let mut blocks = content_blocks(content);
    if blocks.is_empty() {
        // Legacy/untyped content still projects as its joined text (an empty
        // payload keeps the historical empty text block).
        let text = custom_message_text(content);
        blocks.push(ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        }));
    }
    Message::User(UserMessage {
        role: UserRole::User,
        content: blocks,
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: super::types::ms_from_iso(timestamp).unwrap_or(0),
    })
}

/// The framed user-text message a summary entry contributes to context
/// (upstream `createCompactionSummaryMessage` / `createBranchSummaryMessage`),
/// shared with branch summarization's entry conversion.
pub(crate) fn framed_user_message(
    prefix: &str,
    summary: &str,
    suffix: &str,
    timestamp: &str,
) -> Message {
    let text = format!("{prefix}{summary}{suffix}");
    user_text_message(&text, super::types::ms_from_iso(timestamp).unwrap_or(0))
}

/// [`user_text_message`] that takes ownership of the text (no copy): the
/// streaming summarizer's prompt buffer is moved straight into the request.
pub(crate) fn user_text_message_owned(text: String, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp,
    })
}

pub(crate) fn user_text_message(text: &str, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp,
    })
}

/// Walk from the leaf to the root, oldest first (upstream `buildSessionPath`).
/// `entries` holds every parsed non-header entry; an explicit `None` leaf
/// yields an empty path (upstream `leafId === null`).
///
/// INVARIANT (shared-view walk): the path BORROWS the entries, so a caller
/// that already holds them (the session manager's `Arc<FileEntry>` list) walks
/// them in place — the whole-session deep clone this used to demand on every
/// projection build is gone. Generic over the container (`E: Borrow<FileEntry>`)
/// so both `&[FileEntry]` and `&[Arc<FileEntry>]` walk the same code path.
///
/// INVARIANT (header-transparent fallback): the stale-leaf fallback picks the
/// last NON-header entry, which is exactly what `entries.last()` was over a
/// header-filtered slice — so passing the manager's raw `file_entries` (header
/// first) and passing its filtered `entries()` view produce byte-identical
/// paths. Headers never join the id index either (their `id()` is `None`).
///
/// INVARIANT (cycle-safe): the walk stops at the first repeated id, so a
/// corrupt `parentId` cycle yields a finite path instead of an endless walk
/// (valid sessions are cycles-free: every entry parents an older one).
pub fn build_session_path<'a, E: Borrow<FileEntry>>(
    entries: &'a [E],
    leaf_id: Option<&str>,
) -> Vec<&'a FileEntry> {
    let Some(leaf_id) = leaf_id else {
        return Vec::new();
    };
    let index: std::collections::HashMap<&str, &FileEntry> = entries
        .iter()
        .filter_map(|entry| {
            let entry: &FileEntry = entry.borrow();
            entry.id().map(|id| (id, entry))
        })
        .collect();
    let mut current: Option<&FileEntry> = index.get(leaf_id).copied();
    if current.is_none() {
        // Upstream session-manager.ts:400-404: a leaf id that is not in the
        // index (stale pointer after an external edit) falls back to the last
        // entry instead of yielding an empty path.
        current = entries
            .iter()
            .rev()
            .map(Borrow::borrow)
            .find(|entry: &&FileEntry| !entry.is_header());
    }
    let mut path = Vec::new();
    // A parent chain that revisits an id is corrupt data, not a branch: every
    // writer parents a NEW entry, so a valid path never repeats. Without this
    // guard a cycle (`a -> b -> a`, from a hand-edited or foreign file) makes
    // the walk loop forever and hangs every projection/context build. Cycles
    // are dropped exactly like the tree builder below drops them.
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    while let Some(entry) = current {
        if let Some(id) = entry.id() {
            if !visited.insert(id) {
                break;
            }
        }
        path.push(entry);
        current = entry
            .parent_id()
            .and_then(|parent| index.get(parent))
            .copied();
    }
    path.reverse();
    path
}

/// Settings tracked on the leaf path (upstream `getSessionContextSettings`).
pub fn get_session_context_settings(path: &[&FileEntry]) -> (String, Option<(String, String)>) {
    let mut thinking_level = "off".to_owned();
    let mut model: Option<(String, String)> = None;
    for entry in path {
        match entry {
            FileEntry::ThinkingLevelChange(inner) => {
                thinking_level = inner.thinking_level.clone();
            }
            FileEntry::ModelChange(inner) => {
                model = Some((inner.provider.clone(), inner.model_id.clone()));
            }
            FileEntry::Message(inner) => {
                if let StoredMessage::Assistant(assistant) = &inner.message {
                    model = Some((assistant.provider.clone(), assistant.model.clone()));
                }
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

fn is_system_message_entry(entry: &FileEntry) -> bool {
    match entry {
        FileEntry::Message(inner) => inner.message.role() == "system",
        _ => false,
    }
}

/// The active, compaction-aware entry slice of `path` (upstream
/// `buildContextEntries`): `[newest compaction] + kept entries from
/// firstKeptEntryId up to the compaction + everything after it`, as
/// REFERENCES into the path.
///
/// INVARIANT (no intermediate copy): this is the one place the retained-entry
/// selection runs; it never materializes an owned entry list, so a projection
/// build pays exactly one clone per retained entry (the `source_entry` it must
/// own) instead of the two it used to pay (a temporary `Vec<FileEntry>` that
/// was dropped right after being walked).
fn context_entry_refs<'a>(path: &[&'a FileEntry]) -> Vec<&'a FileEntry> {
    let mut compaction_index: Option<usize> = None;
    for (index, entry) in path.iter().enumerate() {
        if matches!(entry, FileEntry::Compaction(_)) {
            compaction_index = Some(index);
        }
    }
    let Some(compaction_index) = compaction_index else {
        return path.to_vec();
    };
    let first_kept_entry_id = match path[compaction_index] {
        FileEntry::Compaction(inner) => inner.first_kept_entry_id.as_str(),
        // Cannot happen (matched above); treat as no compaction.
        _ => return path.to_vec(),
    };

    let mut context_entries: Vec<&FileEntry> = vec![path[compaction_index]];
    let mut found_first_kept = false;
    for entry in &path[..compaction_index] {
        if entry.id() == Some(first_kept_entry_id) {
            found_first_kept = true;
        }
        // The compaction entry carries the system prompt replay, so retained
        // system messages inside the kept range are skipped (upstream).
        if found_first_kept && !is_system_message_entry(entry) {
            context_entries.push(*entry);
        }
    }
    context_entries.extend(path[compaction_index + 1..].iter().copied());
    context_entries
}

/// The active, compaction-aware entry list (upstream `buildContextEntries`)
/// as an owned list — the public shape, one clone per retained entry. Prefer
/// [`build_session_projection`], which borrows through the same walk.
pub fn build_context_entries(entries: &[FileEntry], leaf_id: Option<&str>) -> Vec<FileEntry> {
    let path = build_session_path(entries, leaf_id);
    context_entry_refs(&path).into_iter().cloned().collect()
}

/// Number of model-visible messages an entry contributes, computed WITHOUT
/// materializing them (the count [`project_context_entry`] would produce).
///
/// The counts are the contract between a projection and its metadata twin
/// ([`crate::compaction::ProjectionEntryMeta`]): `projected_message_count`
/// must equal `project_context_entry(entry, edit).len()` for every entry. A
/// test pins the two against a fixture with every entry kind
/// (`tests/suites/compaction_chunking.rs`).
pub fn projected_message_count(entry: &FileEntry, edit: Option<&ContextEditEntry>) -> usize {
    let base = match entry {
        FileEntry::Message(inner) => match &inner.message {
            // Must mirror `session_entry_to_context_messages` (see the
            // contract above): an excluded bash record contributes nothing.
            StoredMessage::BashExecution(bash) if bash.excluded() => 0,
            _ => 1,
        },
        FileEntry::CustomMessage(_) => 1,
        FileEntry::BranchSummary(inner) => usize::from(!inner.summary.is_empty()),
        FileEntry::Compaction(inner) => {
            if inner.system_message.is_some() {
                2
            } else {
                1
            }
        }
        _ => 0,
    };
    match edit {
        // `replacement: null` omits the target from context entirely.
        Some(edit) if edit.replacement.is_none() => 0,
        _ => base,
    }
}

/// [`build_session_projection`] without materializing the conversation: the
/// per-entry metadata (shared source entry + contributed message COUNT) over
/// the same walk, retention rules, and context edits.
///
/// MEMORY SHAPE (pir extension): the compaction decision path reads the live
/// context's messages in entry order, so it needs the counts, not a copy of
/// the messages. On a huge session this is what keeps "check the compaction
/// threshold" free of a transcript-sized allocation.
pub fn build_projection_entry_meta<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> Vec<crate::compaction::ProjectionEntryMeta> {
    let path = build_session_path(entries, leaf_id);
    let context_entries = context_entry_refs(&path);

    let mut edits: std::collections::HashMap<String, &ContextEditEntry> =
        std::collections::HashMap::new();
    for entry in &context_entries {
        let entry: &FileEntry = entry;
        if let FileEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.clone(), edit);
        }
    }

    let mut metas = Vec::with_capacity(context_entries.len());
    for (index, source_entry) in context_entries.into_iter().enumerate() {
        // Only the newest compaction contributes (its summary/checkpoint);
        // older compactions retained inside the kept range contribute nothing
        // (same rule as `build_session_projection`).
        let is_stale_compaction = matches!(source_entry, FileEntry::Compaction(_)) && index > 0;
        let edit = source_entry.id().and_then(|id| edits.get(id).copied());
        let message_count = if is_stale_compaction {
            0
        } else {
            projected_message_count(source_entry, edit)
        };
        metas.push(crate::compaction::ProjectionEntryMeta {
            source_entry: source_entry.shared_entry(),
            message_count,
        });
    }
    metas
}

fn project_context_entry(entry: &FileEntry, edit: Option<&ContextEditEntry>) -> Vec<Message> {
    let messages = session_entry_to_context_messages(entry);
    let Some(edit) = edit else {
        return messages;
    };
    let Some(replacement) = &edit.replacement else {
        // `replacement: null` omits the target from context entirely.
        return Vec::new();
    };
    messages
        .into_iter()
        .map(|message| replace_message_content(message, &replacement.content))
        .collect()
}

/// Apply a context-edit replacement to one message (upstream: only
/// user/assistant/toolResult/custom roles are editable; a string replacement
/// on assistant/toolResult becomes a single text block; pir also converts
/// string replacements on other roles to a text block because its user
/// content is always block-shaped).
fn replace_message_content(message: Message, content: &JsonValue) -> Message {
    match message {
        Message::User(mut user) => {
            user.content = content_blocks(content);
            Message::User(user)
        }
        Message::Assistant(mut assistant) => {
            assistant.content = content_blocks(content);
            Message::Assistant(assistant)
        }
        Message::ToolResult(mut result) => {
            result.content = content_blocks(content);
            Message::ToolResult(result)
        }
        other => other,
    }
}

/// Convert a JSON content value (string or block array) into typed content
/// blocks, shared by the session projection and the custom-message
/// conversion.
pub(crate) fn content_blocks(content: &JsonValue) -> Vec<ContentBlock> {
    match content {
        JsonValue::String(text) => vec![ContentBlock::Text(TextContent {
            text: text.clone(),
            text_signature: None,
        })],
        JsonValue::Array(items) => items
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Share one session entry as an `Arc` for a projection's `source_entry`.
///
/// Implemented for both entry containers the walk accepts, so the session
/// manager's `Arc<FileEntry>` list hands its OWN handle over (refcount bump,
/// no entry clone) while a plain `&[FileEntry]` walk (tests, export paths)
/// pays the one clone it must.
pub trait SharedEntry: Borrow<FileEntry> {
    fn shared_entry(&self) -> Arc<FileEntry>;
}

impl SharedEntry for Arc<FileEntry> {
    fn shared_entry(&self) -> Arc<FileEntry> {
        Arc::clone(self)
    }
}

impl SharedEntry for FileEntry {
    fn shared_entry(&self) -> Arc<FileEntry> {
        Arc::new(self.clone())
    }
}

/// Build the provenance-preserving, compaction-aware model context (upstream
/// `buildSessionProjection`).
///
/// INVARIANT (single walk, single materialization): the leaf→root path is
/// built ONCE, the retained context entries are selected from it, and the
/// conversation is materialized exactly ONCE — into
/// [`SessionProjection::messages`]. Each projected entry records the message
/// RANGE it contributed and shares its source `Arc<FileEntry>`, so a
/// transcript-sized session is never cloned per entry (the former shape held
/// one `Vec<Message>` per entry plus the aggregate plus a cloned `FileEntry`).
///
/// Generic over the entry container (`E: SharedEntry`) so the session
/// manager projects straight off its `Arc<FileEntry>` list.
pub fn build_session_projection<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> SessionProjection {
    let path = build_session_path(entries, leaf_id);
    let (thinking_level, model) = get_session_context_settings(&path);
    let context_entries = context_entry_refs(&path);

    // Latest context edit per target (upstream: later edits win).
    let mut edits: std::collections::HashMap<String, &ContextEditEntry> =
        std::collections::HashMap::new();
    for entry in &context_entries {
        let entry: &FileEntry = entry;
        if let FileEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.clone(), edit);
        }
    }

    // One shared handle per retained entry (the source `Arc` when the walk
    // was fed the manager's list, a fresh `Arc` for a borrowed slice) plus the
    // flat message aggregate the ranges point into.
    let mut projected: Vec<ProjectedSessionEntry> = Vec::with_capacity(context_entries.len());
    // Exact aggregate capacity (same count contract the metadata twin uses):
    // growing the conversation-sized `Vec` from zero reallocates and copies
    // its element buffer log2(N) times, and the last reallocation briefly holds
    // the old and the new buffer at once — on a 10k-message projection that is
    // a needless multi-megabyte transient on every build. Counting first is
    // one extra O(entries) pass over data already in cache.
    let message_capacity: usize = context_entries
        .iter()
        .enumerate()
        .map(|(index, source_entry)| {
            if matches!(source_entry, FileEntry::Compaction(_)) && index > 0 {
                0
            } else {
                let edit = source_entry.id().and_then(|id| edits.get(id).copied());
                projected_message_count(source_entry, edit)
            }
        })
        .sum();
    let mut messages: Vec<Message> = Vec::with_capacity(message_capacity);
    for (index, source_entry) in context_entries.into_iter().enumerate() {
        // buildContextEntries may retain an older compaction entry because
        // its raw id lies inside the newest retained range; only the
        // newest compaction (first entry of the projected list) contributes
        // a checkpoint and summary.
        let is_stale_compaction = matches!(source_entry, FileEntry::Compaction(_)) && index > 0;
        let contributes: Vec<Message> = if is_stale_compaction {
            Vec::new()
        } else {
            let edit = source_entry.id().and_then(|id| edits.get(id).copied());
            project_context_entry(source_entry, edit)
        };
        let start = messages.len();
        messages.extend(contributes);
        projected.push(ProjectedSessionEntry {
            source_entry: source_entry.shared_entry(),
            messages: start..messages.len(),
        });
    }
    SessionProjection {
        entries: projected,
        messages,
        thinking_level,
        model,
    }
}

/// The current system message of a message list (upstream
/// `getCurrentSystemMessage`): the REPLAY of every system message — content
/// paragraphs joined, named sections patched by name, and the active tool set
/// resolved from the declaration record. `None` when the list carries no
/// system message at all.
///
/// This is what `appendCompaction` stores as the checkpoint's
/// `systemMessage` (upstream `getCurrentSystemMessage(projection.messages)`),
/// so a resumed session restores the prompt text, its sections, and the tool
/// loadout from the checkpoint. Taking only the last system message would
/// drop the base prompt and every earlier declaration.
pub fn current_system_message(messages: &[Message]) -> Option<StoredSystemMessage> {
    let replayed = crate::ai::types::get_current_system_message(messages)?;
    match replayed {
        Message::System(system) => Some(StoredSystemMessage::from_message(&system)),
        _ => None,
    }
}

/// [`current_system_message`] over the session entries, WITHOUT materializing
/// the conversation.
///
/// [`crate::session_manager::SessionManager::append_compaction`] only needs the
/// replayed SYSTEM messages (the prompt text, its sections, and the active tool
/// loadout) to store in the compaction entry. Building the full projection for
/// that walk cloned every user/assistant/toolResult message of the transcript,
/// so every compaction of a large session paid a transient second copy of the
/// whole conversation (the same projection the prompt-start path had just
/// built and dropped). This walks the identical leaf-path / retention /
/// context-edit rules but collects only the system contributions (a handful of
/// prompt-sized messages) and runs the unchanged replay over them.
///
/// Equivalence: a system message has no content-replacement form (the edit
/// replacement path only rewrites user/assistant/toolResult content), so the
/// only edit that matters here is `replacement: null` (omission), which is
/// applied exactly like [`build_session_projection`] applies it.
pub fn current_system_message_from_entries<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> Option<StoredSystemMessage> {
    let path = build_session_path(entries, leaf_id);
    let context_entries = context_entry_refs(&path);

    let mut edits: std::collections::HashMap<String, &ContextEditEntry> =
        std::collections::HashMap::new();
    for entry in &context_entries {
        let entry: &FileEntry = entry;
        if let FileEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.clone(), edit);
        }
    }

    let mut system_messages: Vec<Message> = Vec::new();
    for (index, source_entry) in context_entries.into_iter().enumerate() {
        // Only the newest compaction contributes (same rule as the projection).
        if index > 0 && matches!(source_entry, FileEntry::Compaction(_)) {
            continue;
        }
        if let Some(edit) = source_entry.id().and_then(|id| edits.get(id).copied()) {
            if edit.replacement.is_none() {
                continue;
            }
        }
        match source_entry {
            FileEntry::Message(inner) => match &inner.message {
                // Two wire shapes can carry a system message: the upstream
                // string-content `StoredSystemMessage`, and a `UserMessage`
                // whose role is `system` (a foreign/older writer). The
                // projection's `From<StoredMessage>` accepts both, so the
                // system-only walk must too.
                StoredMessage::System(system) => system_messages.push(system.to_message()),
                StoredMessage::User(user) if user.role == UserRole::System => {
                    system_messages.push(Message::System(user.clone()));
                }
                _ => {}
            },
            FileEntry::Compaction(inner) => {
                if let Some(system) = &inner.system_message {
                    system_messages.push(system.to_message());
                }
            }
            _ => {}
        }
    }
    current_system_message(&system_messages)
}

/// The text of the last assistant message on the live context path, WITHOUT
/// materializing the projection.
///
/// Upstream `getLastAssistantText` walks `session.messages` backwards; the
/// RPC/state handlers call it on every poll, and the old implementation built
/// the whole `SessionProjection` (one clone of every message) just to read one
/// string. This walks the retained entries backwards instead, applies the same
/// context-edit replacement/omission rules, and returns exactly what the
/// projection-based walk returned: the first non-aborted assistant message
/// decides (an all-whitespace one yields `None`, it does not fall through to
/// an older message).
pub fn last_assistant_text_from_entries<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> Option<String> {
    use crate::ai::types::StopReason;

    let path = build_session_path(entries, leaf_id);
    let context_entries = context_entry_refs(&path);

    let mut edits: std::collections::HashMap<String, &ContextEditEntry> =
        std::collections::HashMap::new();
    for entry in &context_entries {
        let entry: &FileEntry = entry;
        if let FileEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.clone(), edit);
        }
    }

    for source_entry in context_entries.into_iter().rev() {
        let FileEntry::Message(inner) = source_entry else {
            continue;
        };
        let StoredMessage::Assistant(assistant) = &inner.message else {
            continue;
        };
        let edit = source_entry.id().and_then(|id| edits.get(id).copied());
        if let Some(edit) = edit {
            if edit.replacement.is_none() {
                // `replacement: null` omits the target from context.
                continue;
            }
        }
        // A replacement on an assistant message rewrites its content
        // (`replace_message_content`); the text walk must see that content,
        // not the stored one.
        let content: Vec<ContentBlock> = match edit.and_then(|edit| edit.replacement.as_ref()) {
            Some(replacement) => content_blocks(&replacement.content),
            None => assistant.content.clone(),
        };
        if assistant.stop_reason == StopReason::Aborted && content.is_empty() {
            continue;
        }
        let text: String = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        let trimmed = text.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
    }
    None
}
