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
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use super::types::{ContextEditEntry, FileEntry, StoredMessage, StoredSystemMessage};
use super::SessionManager;
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
    let mut messages = Vec::new();
    visit_entry_context_messages(entry, &mut |message| messages.push(message));
    messages
}

/// The projection of one entry, delivered message-by-message: the one
/// implementation [`session_entry_to_context_messages`] and the aggregate
/// builders share, so a projection never allocates a throwaway `Vec<Message>`
/// per entry (thousands of small allocations on a long session).
fn visit_entry_context_messages(entry: &FileEntry, visit: &mut impl FnMut(Message)) {
    match entry {
        // A `!!` bash record stays in the transcript but not in the model
        // context (upstream `convertToLlm` skips `excludeFromContext`).
        FileEntry::Message(inner) => match &inner.message {
            StoredMessage::BashExecution(bash) if bash.excluded() => {}
            _ => visit(inner.message.clone().into()),
        },
        FileEntry::CustomMessage(inner) => {
            visit(custom_context_message(&inner.content, entry.timestamp()))
        }
        FileEntry::BranchSummary(inner) => {
            if !inner.summary.is_empty() {
                visit(framed_user_message(
                    BRANCH_SUMMARY_PREFIX,
                    &inner.summary,
                    BRANCH_SUMMARY_SUFFIX,
                    entry.timestamp(),
                ));
            }
        }
        FileEntry::Compaction(inner) => {
            // Upstream replays the stored checkpoint system message as-is
            // (`entry.systemMessage`), including its named sections and tool
            // declarations — a resumed session restores its prompt sections
            // and active tool loadout from this entry.
            if let Some(system) = &inner.system_message {
                visit(system.to_message());
            }
            visit(framed_user_message(
                COMPACTION_SUMMARY_PREFIX,
                &inner.summary,
                COMPACTION_SUMMARY_SUFFIX,
                entry.timestamp(),
            ));
        }
        // State entries (usage, label, custom, model/thinking changes,
        // session_info, context_edit): no context contribution.
        _ => {}
    }
}

/// Append one entry's context messages (after its context edit, if any) to an
/// aggregate list. `replacement: null` omits the entry entirely; a
/// replacement rewrites each message's editable content exactly like
/// [`replace_message_content`].
fn push_context_entry_messages(
    entry: &FileEntry,
    edit: Option<&ContextEditEntry>,
    out: &mut Vec<Message>,
) {
    match edit {
        None => visit_entry_context_messages(entry, &mut |message| out.push(message)),
        Some(edit) => {
            let Some(replacement) = &edit.replacement else {
                return;
            };
            visit_entry_context_messages(entry, &mut |message| {
                out.push(replace_message_content(message, &replacement.content));
            });
        }
    }
}

/// Append one entry's UNEDITED context messages to an aggregate list — the
/// push-based form of [`session_entry_to_context_messages`] for callers that
/// aggregate several entries (paged load-more pages): one shared output Vec
/// instead of a throwaway per-entry `Vec<Message>` allocation each.
pub(crate) fn push_entry_context_messages(entry: &FileEntry, out: &mut Vec<Message>) {
    push_context_entry_messages(entry, None, out);
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

/// Id → resident position for the session manager's current `file_entries`
/// slice. Maintained in lockstep with the manager's `by_id` (`build_index` /
/// `append_entry` / `new_session`), so every per-prompt leaf→root walk resolves
/// the parent chain through this ONE map instead of rebuilding an index per
/// call.
pub(crate) type EntryPositions = std::collections::HashMap<String, usize>;

/// Lookup head for the leaf→root walk.
///
/// `Borrowed` is the per-call index the public entry points build from their
/// own slice (no id allocation, the pre-R1 shape). `Persistent` borrows the
/// session manager's [`EntryPositions`] so the per-prompt walks (projection,
/// branch snapshot, compaction metadata, append-only delta) share one index.
/// A persistent slot is VALIDATED (the entry at the recorded position must
/// still carry the id); a stale/foreign map therefore degrades to the
/// unindexed lookup and the walk keeps today's behavior.
enum LeafPathIndex<'a, 'p, E> {
    Borrowed(std::collections::HashMap<&'a str, usize>, &'a [E]),
    Persistent(&'p EntryPositions, &'a [E]),
}

/// The per-call borrowed index (one slot per non-header entry). Kept separate
/// so the `None` (public/unindexed) path allocates no id strings, exactly the
/// pre-R1 walk.
fn borrowed_positions<E: Borrow<FileEntry>>(
    entries: &[E],
) -> std::collections::HashMap<&str, usize> {
    let mut positions = std::collections::HashMap::with_capacity(entries.len());
    for (position, entry) in entries.iter().enumerate() {
        if let Some(id) = entry.borrow().id() {
            positions.insert(id, position);
        }
    }
    positions
}

impl<'a, 'p, E: Borrow<FileEntry>> LeafPathIndex<'a, 'p, E> {
    fn entries(&self) -> &'a [E] {
        match self {
            LeafPathIndex::Borrowed(_, entries) | LeafPathIndex::Persistent(_, entries) => entries,
        }
    }

    fn position(&self, id: &str) -> Option<usize> {
        match self {
            LeafPathIndex::Borrowed(positions, _) => positions.get(id).copied(),
            LeafPathIndex::Persistent(positions, _) => positions.get(id).copied(),
        }
    }

    /// `(position, entry)` for `id`, or `None` when the id is absent OR the
    /// recorded position no longer carries it (stale/foreign index).
    fn resolve(&self, id: &str) -> Option<(usize, &'a E)> {
        let position = self.position(id)?;
        let entry = self.entries().get(position)?;
        if entry.borrow().id() == Some(id) {
            Some((position, entry))
        } else {
            None
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.resolve(id).is_some()
    }
}

/// Walk from the leaf to the root, oldest first (upstream `buildSessionPath`),
/// keeping the ORIGINAL container elements in the returned path.
///
/// MEMORY SHAPE: returning `&E` (rather than `&FileEntry`) is what lets a
/// projection builder recover the manager's `Arc<FileEntry>` handle from each
/// path member instead of deep-cloning the entry into a fresh `Arc`. When the
/// sole walk projected every element to `&FileEntry`,
/// `SharedEntry::shared_entry` resolved to the `FileEntry` impl and cloned
/// every retained entry (message bodies, tool results and all) on every
/// projection build — the exact copy the `SessionProjection` memory shape is
/// documented to avoid.
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
///
/// INVARIANT (broken-chain bridge, PIR-BIN-092): a `parentId` that is not in
/// the index does NOT truncate the branch. The loader skips a line that fails
/// deserialization (it stays in `unparsed_raw_lines` for verbatim rewrites),
/// which leaves the NEXT surviving entry pointing at an id the index never
/// saw; upstream pi stops there (`current = entry.parentId ? index.get(...) :
/// undefined`), silently dropping every earlier row from the transcript and
/// the model context. Here the walk joins across the hole to the nearest
/// surviving NON-header entry BEFORE the orphan in file order — the row the
/// skipped line itself was appended after (the session file is append-only,
/// so file order IS chain order) — and keeps going, so one bad row can no
/// longer erase the prefix. A `parentId: null` root still ends the chain
/// (nothing is missing there), headers never join it, and the repeated-id
/// guard above still bounds a corrupt bridge. This intentionally goes beyond
/// upstream's own stop-at-the-hole behavior; the loader warns about every
/// skipped line so the loss of that one row is never silent.
///
/// `bridge_missing_parents` gates that bridge: it must be OFF for a WINDOWED
/// slice (a paged manager's resident tail), where a parent outside the slice
/// means "the rest of the branch is on disk", not "a line was skipped". The
/// windowed walk therefore stops at the window boundary — the documented
/// [`SessionManager::branch_entries`] contract the prefix streamers
/// (`fork_user_messages_streamed`, the paged consumers) depend on. A slice
/// that starts at file line 0 with no skipped lines can never hit the arm.
fn build_session_path_entries<'a, E: Borrow<FileEntry>>(
    entries: &'a [E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> Vec<&'a E> {
    let Some(leaf_id) = leaf_id else {
        return Vec::new();
    };
    // Positions travel with the entries: the broken-chain bridge below needs
    // the FILE POSITION of the entry whose parent is missing (the row before
    // it in file order is the one the skipped line was appended after). A
    // caller that already holds the session manager's persistent
    // [`EntryPositions`] (every per-prompt path) passes it here instead of
    // rebuilding a throwaway index per walk.
    let index = match positions {
        Some(positions) => LeafPathIndex::Persistent(positions, entries),
        None => LeafPathIndex::Borrowed(borrowed_positions(entries), entries),
    };
    let mut current: Option<(usize, &E)> = index.resolve(leaf_id);
    if current.is_none() {
        // Upstream session-manager.ts:400-404: a leaf id that is not in the
        // index (stale pointer after an external edit) falls back to the last
        // entry instead of yielding an empty path.
        current = entries.iter().enumerate().rev().find(|(_, entry)| {
            let entry: &FileEntry = (**entry).borrow();
            !entry.is_header()
        });
    }
    let mut path = Vec::new();
    // A parent chain that revisits an id is corrupt data, not a branch: every
    // writer parents a NEW entry, so a valid path never repeats. Without this
    // guard a cycle (`a -> b -> a`, from a hand-edited or foreign file) makes
    // the walk loop forever and hangs every projection/context build. Cycles
    // are dropped exactly like the tree builder below drops them.
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    while let Some((position, entry)) = current {
        let entry_ref: &FileEntry = entry.borrow();
        if let Some(id) = entry_ref.id() {
            if !visited.insert(id) {
                break;
            }
        }
        path.push(entry);
        current = match entry_ref.parent_id() {
            // `parentId: null` is a root, not a hole: the chain ends here
            // exactly like upstream.
            None => None,
            Some(parent) => match index.resolve(parent) {
                Some(slot) => Some(slot),
                // A parent id the index never saw: the line holding it was
                // skipped (unparsable) and the walk joins across the hole
                // instead of dropping the prefix (see the invariant above) —
                // but only when the slice is the complete file. In a windowed
                // slice the missing parent is the link into the streamed
                // prefix, and bridging would silently widen the branch.
                None if bridge_missing_parents => bridging_predecessor(position, entries),
                None => None,
            },
        };
    }
    path.reverse();
    // PIR-SDK-024: a live-tail trim pins the newest compaction checkpoint in
    // the resident list while the entries between it and the tail window stay
    // on disk. The parent walk therefore stops at the first window entry (its
    // parent is skipped), and the checkpoint would drop out of the projection.
    // The checkpoint IS the model-visible replacement for that skipped prefix,
    // so attach the newest resident compaction OLDER than the walk's first
    // entry as the path root. A full (non-paged) load always has an unbroken
    // parent chain, so this only fires for a trimmed window.
    if let Some(first) = path.first() {
        let first_entry: &FileEntry = (*first).borrow();
        let chain_broke = first_entry
            .parent_id()
            .is_some_and(|parent| !index.contains(parent));
        if chain_broke {
            let first_id = first_entry.id();
            let mut anchor = None;
            for entry in entries {
                let file: &FileEntry = (*entry).borrow();
                if file.id() == first_id {
                    break;
                }
                if matches!(file, FileEntry::Compaction(_)) {
                    anchor = Some(entry);
                }
            }
            if let Some(anchor) = anchor {
                path.insert(0, anchor);
            }
        }
    }
    path
}

/// The entry an unlinkable `parentId` joins across (PIR-BIN-092): the nearest
/// surviving NON-header entry BEFORE `position` in file order — the row the
/// skipped line was appended after.
///
/// The session file is append-only and single-writer, so consecutive lines are
/// the true parent relationship; the skipped line sat between `entries[..position]`'s
/// last member and this entry. When no such predecessor exists (the orphan is
/// the file's first entry, or only a header precedes it) the chain ends, which
/// is exactly the old behavior for the window/file head. The file-streamed
/// OpenAI body walk (`session_message_source::resolve_path`) mirrors this rule
/// over its own scan index (one `predecessor_id` per entry), and a fixture
/// test pins the two walks to the same messages.
pub(crate) fn bridging_predecessor<E: Borrow<FileEntry>>(
    position: usize,
    entries: &[E],
) -> Option<(usize, &E)> {
    let mut slot = position.min(entries.len());
    while slot > 0 {
        slot -= 1;
        let entry = &entries[slot];
        let entry_ref: &FileEntry = entry.borrow();
        // Headers carry no chain identity (`id()` is None) and never join the
        // path (the header-transparency invariant above).
        if entry_ref.is_header() {
            continue;
        }
        return Some((slot, entry));
    }
    None
}

/// [`build_session_path_entries`] projected to `&FileEntry` — the public
/// borrowed view callers that only need entry contents have always used.
/// Missing parents are bridged (PIR-BIN-092): pass the whole file's entries.
pub fn build_session_path<'a, E: Borrow<FileEntry>>(
    entries: &'a [E],
    leaf_id: Option<&str>,
) -> Vec<&'a FileEntry> {
    build_session_path_indexed(entries, leaf_id, true, None)
}

/// [`build_session_path`] over the session manager's persistent
/// [`EntryPositions`]: the same walk with no per-call index build.
/// `bridge_missing_parents` selects the file-wide PIR-BIN-092 bridge
/// (`true`) or the windowed stop-at-boundary rule (`false`).
pub(crate) fn build_session_path_indexed<'a, E: Borrow<FileEntry>>(
    entries: &'a [E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> Vec<&'a FileEntry> {
    build_session_path_entries(entries, leaf_id, bridge_missing_parents, positions)
        .into_iter()
        .map(|entry| {
            let entry: &FileEntry = (*entry).borrow();
            entry
        })
        .collect()
}

/// [`build_session_path`] for a WINDOWED slice (a paged manager's resident
/// tail): the walk stops where the parent leaves the slice instead of joining
/// across it, exactly the pre-PIR-BIN-092 contract the prefix streamers rely
/// on. Use this whenever `entries` may be a tail window.
pub fn build_session_path_windowed<'a, E: Borrow<FileEntry>>(
    entries: &'a [E],
    leaf_id: Option<&str>,
) -> Vec<&'a FileEntry> {
    build_session_path_indexed(entries, leaf_id, false, None)
}

/// [`build_session_path`] keeping the caller's SHARED handles: the same walk
/// (bridge rules included via `bridge_missing_parents`), but each path member
/// stays an `Arc<FileEntry>` (one refcount bump) instead of being projected to
/// `&FileEntry`. The branch-snapshot builder
/// ([`SessionManager::branch_path_shared`]) uses this so it never clones an
/// entry id.
pub(crate) fn build_session_path_shared(
    entries: &[Arc<FileEntry>],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> Vec<Arc<FileEntry>> {
    build_session_path_entries(entries, leaf_id, bridge_missing_parents, positions)
        .into_iter()
        .map(Arc::clone)
        .collect()
}

/// Settings tracked on the leaf path (upstream `getSessionContextSettings`).
pub fn get_session_context_settings<E: Borrow<FileEntry>>(
    path: &[&E],
) -> (String, Option<(String, String)>) {
    let mut thinking_level = "off".to_owned();
    let mut model: Option<(String, String)> = None;
    for entry in path {
        let entry: &FileEntry = (*entry).borrow();
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
/// selection runs; it never materializes an owned entry list, and it keeps the
/// caller's container references (`&E`) so an `Arc<FileEntry>` list never pays
/// a per-entry deep clone just to select its retained range.
fn context_entry_refs<'a, E: Borrow<FileEntry>>(path: &[&'a E]) -> Vec<&'a E> {
    let mut compaction_index: Option<usize> = None;
    for (index, entry) in path.iter().enumerate() {
        let entry: &FileEntry = (*entry).borrow();
        if matches!(entry, FileEntry::Compaction(_)) {
            compaction_index = Some(index);
        }
    }
    let Some(compaction_index) = compaction_index else {
        return path.to_vec();
    };
    let first_kept_entry_id = match (*path[compaction_index]).borrow() {
        FileEntry::Compaction(inner) => inner.first_kept_entry_id.as_str(),
        // Cannot happen (matched above); treat as no compaction.
        _ => return path.to_vec(),
    };

    let mut context_entries: Vec<&E> = vec![path[compaction_index]];
    let mut found_first_kept = false;
    for entry in &path[..compaction_index] {
        let file: &FileEntry = (*entry).borrow();
        if file.id() == Some(first_kept_entry_id) {
            found_first_kept = true;
        }
        // The compaction entry carries the system prompt replay, so retained
        // system messages inside the kept range are skipped (upstream).
        if found_first_kept && !is_system_message_entry(file) {
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
    let path = build_session_path_entries(entries, leaf_id, true, None);
    context_entry_refs(&path).into_iter().cloned().collect()
}

/// Latest context edit per target over the retained entries (upstream: later
/// edits win). Returns `None` when the retained range has no `context_edit`
/// entry at all — the common case — so projection builds skip a hash map and
/// every per-entry id lookup entirely.
fn context_edit_index<'a, E: Borrow<FileEntry>>(
    context_entries: &[&'a E],
) -> Option<std::collections::HashMap<String, &'a ContextEditEntry>> {
    let mut edits: Option<std::collections::HashMap<String, &'a ContextEditEntry>> = None;
    for entry in context_entries {
        let entry: &'a FileEntry = (*entry).borrow();
        if let FileEntry::ContextEdit(edit) = entry {
            edits
                .get_or_insert_with(std::collections::HashMap::new)
                .insert(edit.target_id.clone(), edit);
        }
    }
    edits
}

/// The edit targeting `entry`, when the retained range carries any edits.
fn context_edit_for<'a>(
    entry: &FileEntry,
    edits: Option<&std::collections::HashMap<String, &'a ContextEditEntry>>,
) -> Option<&'a ContextEditEntry> {
    edits.and_then(|edits| entry.id().and_then(|id| edits.get(id).copied()))
}

/// The leaf path plus its retained context entries and edit index — the one
/// walk every projection/context reader shares.
/// The leaf path / retained context / edit index of `entries` — the shared
/// first step of every projection reader.
///
/// `bridge_missing_parents` is the caller's slice contract (PIR-BIN-101):
/// `true` for the whole file (the PIR-BIN-092 broken-chain bridge), `false`
/// for a WINDOWED slice (a paged/live-trimmed resident tail), where a parent
/// outside the slice means "the rest of the branch is on disk" and the walk
/// must stop at the boundary — the same rule
/// [`SessionManager::projected_branch_path`] pins with `!is_paged()`.
/// Bridging a window would join to the previous non-header ROW IN THE SLICE,
/// which can belong to an abandoned branch (the model-context bug this flag
/// closes).
fn retained_context<'a, E: Borrow<FileEntry>>(
    entries: &'a [E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> (
    Vec<&'a E>,
    Vec<&'a E>,
    Option<std::collections::HashMap<String, &'a ContextEditEntry>>,
) {
    let path = build_session_path_entries(entries, leaf_id, bridge_missing_parents, positions);
    let context_entries = context_entry_refs(&path);
    let edits = context_edit_index(&context_entries);
    (path, context_entries, edits)
}

/// Number of model-visible messages an entry contributes, computed WITHOUT
/// materializing them (the count [`push_context_entry_messages`] produces).
///
/// The counts are the contract between a projection and its metadata twin
/// ([`crate::compaction::ProjectionEntryMeta`]): `projected_message_count`
/// must equal the number of messages [`push_context_entry_messages`] appends
/// for every entry. A test pins the two against a fixture with every entry
/// kind
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
    build_projection_entry_meta_indexed(entries, leaf_id, true, None)
}

/// [`build_projection_entry_meta`] over the manager's persistent
/// [`EntryPositions`] (the per-prompt compaction-metadata path).
/// `bridge_missing_parents`: the slice contract (see [`retained_context`]).
pub(crate) fn build_projection_entry_meta_indexed<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> Vec<crate::compaction::ProjectionEntryMeta> {
    let (_path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, positions);

    let mut metas = Vec::with_capacity(context_entries.len());
    for (index, source_entry) in context_entries.iter().enumerate() {
        let entry: &FileEntry = (*source_entry).borrow();
        // Only the newest compaction contributes (its summary/checkpoint);
        // older compactions retained inside the kept range contribute nothing
        // (same rule as `build_session_projection`).
        let is_stale_compaction = matches!(entry, FileEntry::Compaction(_)) && index > 0;
        let edit = context_edit_for(entry, edits.as_ref());
        let message_count = if is_stale_compaction {
            0
        } else {
            projected_message_count(entry, edit)
        };
        metas.push(crate::compaction::ProjectionEntryMeta {
            // `source_entry` here is `&E`; for the manager's `Arc<FileEntry>`
            // list this is a refcount bump, not an entry clone.
            source_entry: (*source_entry).shared_entry(),
            message_count,
        });
    }
    metas
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
/// RANGE it contributed and SHARES its source `Arc<FileEntry>` (a refcount
/// bump when the walk was fed the manager's list), so a transcript-sized
/// session is never cloned per entry.
///
/// Generic over the entry container (`E: SharedEntry`) so the session
/// manager projects straight off its `Arc<FileEntry>` list.
pub fn build_session_projection<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> SessionProjection {
    build_session_projection_indexed(entries, leaf_id, true, None)
}

/// [`build_session_projection`] over the manager's persistent
/// [`EntryPositions`] (the per-prompt projection build).
/// `bridge_missing_parents`: the slice contract (see [`retained_context`]).
pub(crate) fn build_session_projection_indexed<E: SharedEntry>(
    entries: &[E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> SessionProjection {
    let (path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, positions);
    let (thinking_level, model) = get_session_context_settings(&path);

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
            let entry: &FileEntry = (*source_entry).borrow();
            if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
                0
            } else {
                let edit = context_edit_for(entry, edits.as_ref());
                projected_message_count(entry, edit)
            }
        })
        .sum();
    let mut messages: Vec<Message> = Vec::with_capacity(message_capacity);
    for (index, source_entry) in context_entries.iter().enumerate() {
        let entry: &FileEntry = (*source_entry).borrow();
        // buildContextEntries may retain an older compaction entry because
        // its raw id lies inside the newest retained range; only the
        // newest compaction (first entry of the projected list) contributes
        // a checkpoint and summary.
        let start = messages.len();
        if !(matches!(entry, FileEntry::Compaction(_)) && index > 0) {
            let edit = context_edit_for(entry, edits.as_ref());
            push_context_entry_messages(entry, edit, &mut messages);
        }
        projected.push(ProjectedSessionEntry {
            source_entry: (*source_entry).shared_entry(),
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

/// [`build_session_projection`]'s LLM-visible message aggregate ONLY — the
/// messages-only twin for whole-history readers (`AgentSession::transcript`,
/// continuation rebuilds, cache-warm snapshots) that do not need provenance.
///
/// MEMORY SHAPE (pir extension): `build_session_projection` also materializes
/// one [`ProjectedSessionEntry`] per retained entry (an `Arc<FileEntry>`
/// refcount bump plus a `Range`) for callers that map messages back to source
/// entries. A messages-only reader paid that metadata vec for nothing. This
/// walk applies the exact same leaf-path / retention / context-edit rules but
/// writes only the flat `Vec<Message>` (the one copy the public API must
/// return; there is no second aggregate and no per-entry vec).
pub fn build_session_messages<E: Borrow<FileEntry>>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> Vec<Message> {
    build_session_messages_indexed(entries, leaf_id, true, None)
}

/// [`build_session_messages`] over the manager's persistent [`EntryPositions`]
/// (the whole-history readers' per-prompt rebuild).
/// `bridge_missing_parents`: the slice contract (see [`retained_context`]).
pub(crate) fn build_session_messages_indexed<E: Borrow<FileEntry>>(
    entries: &[E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> Vec<Message> {
    let (_path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, positions);
    let message_capacity: usize = context_entries
        .iter()
        .enumerate()
        .map(|(index, source_entry)| {
            let entry: &FileEntry = (*source_entry).borrow();
            if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
                0
            } else {
                projected_message_count(entry, context_edit_for(entry, edits.as_ref()))
            }
        })
        .sum();
    let mut messages: Vec<Message> = Vec::with_capacity(message_capacity);
    for (index, source_entry) in context_entries.iter().enumerate() {
        let entry: &FileEntry = (*source_entry).borrow();
        if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
            continue;
        }
        push_context_entry_messages(
            entry,
            context_edit_for(entry, edits.as_ref()),
            &mut messages,
        );
    }
    messages
}

/// The per-entry serialized-message FRAGMENTS of one projection, aligned with
/// the projection's `entries` (index i = the `{entry, messages}` view of
/// `projection.entries[i]`), plus the flat aggregate those fragments build.
///
/// MEMORY SHAPE (PIR-SDK-027 round 2, v3): the snapshot push renders the
/// projection as pre-rendered JSON text and must also publish the native
/// `entries` contract (`SessionContextSnapshot::entries: Arc<JsonValue>` —
/// the todo plugin reads `snapshot.entries.as_array()`). Measured cost terms
/// (`examples/transcript_large_probe` `breakdown` mode, 8k-message session):
/// a full aggregate `to_value` is ~200k allocs / ~16.8 MB per push, a deep
/// `Value` clone of it ~200k / ~21.3 MB, and rebuilding the aggregate OUT of
/// per-entry fragment values ~200k / ~31.3 MB (clone per element + the
/// re-serialization) — every variant of "rebuild the aggregate per push"
/// costs more than the text work it replaces. So the fragments cache the
/// aggregate VALUE and the aggregate TEXT alongside the per-entry views:
/// - append push: the cached `Arc` is shared for `entries` (zero re-clone),
///   the appended messages' values are built once (they are new), the
///   aggregate text is the cached text + the appended messages' text, and
///   each view is its cached fragment text (existing) or the appended text
///   (new) — nothing retained is serialized twice;
/// - structure change: fragments + aggregate + text rebuild from the
///   projection (one pass per entry — the OLD shape's per-push cost, now
///   paid once per structure change).
///
/// Per-entry message views are immutable per entry: a plain append adds new
/// entries and never rewrites an old one (a `context_edit` is itself a new
/// entry that changes the projection, which moves the whole key — validated
/// by the caller's session key).
#[derive(Debug, Clone)]
pub struct ProjectionEntryFragments {
    /// `Some(value)` = the entry's message slice as an owned JSON array
    /// value; `None` = slice serialization failed (the splice falls back to
    /// serializing the slice again — the aggregate fallback covers it).
    pub fragments: Vec<Option<JsonValue>>,
    /// The FLAT aggregate (every projected message in entry order) the
    /// snapshot publishes as its `entries` field. `None` = a fragment failed
    /// when this set was built (the caller falls back to the old per-push
    /// `to_value`); built once per set, shared by `Arc` across pushes.
    pub aggregate: Option<Arc<JsonValue>>,
    /// The aggregate's serialized text (the `messages` field of the
    /// projection object). Cached with [`Self::aggregate`]: re-serializing
    /// the 4.3 MB aggregate was ~120k allocs / ~7.7 MB per push.
    pub aggregate_text: Option<String>,
    /// The FULL `{entries:[...],"messages":[...]}` projection object text the
    /// snapshot push splices into the wire. Cached so an append SPICES the
    /// new views in (one string copy) instead of re-serializing every
    /// retained view (`{"entry":...}` per view is ~15 allocs; ~120k per push
    /// on an 8k-entry session). `None` = not cached (the caller re-renders).
    pub projection_json: Option<String>,
}

impl ProjectionEntryFragments {
    /// Fragment for one projected entry, or `None` when its slice
    /// serialization failed (the splice re-serializes).
    pub fn fragment(&self, index: usize) -> Option<&JsonValue> {
        self.fragments.get(index).and_then(|slot| slot.as_ref())
    }

    /// The one serialization pass per appended entry: the fragment value
    /// (cached for future pushes) and its text (spliced now).
    pub fn build_fragment(
        projection: &SessionProjection,
        index: usize,
    ) -> Option<(JsonValue, String)> {
        let entry = projection.entries.get(index)?;
        let slice = entry.messages_in(&projection.messages);
        match serde_json::to_value(slice) {
            Ok(value) => {
                let text = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
                Some((value, text))
            }
            Err(_) => None,
        }
    }

    /// The FLAT aggregate over the per-entry fragment arrays: element-wise
    /// concat in entry order (wrapping the arrays themselves would produce a
    /// nested `[[...]]` — the wire shape the byte-equivalence test pins is a
    /// flat message list). `None` when any fragment is missing (the caller
    /// falls back to a per-push aggregate `to_value`).
    pub fn flat_aggregate(&self) -> Option<JsonValue> {
        let mut items = Vec::with_capacity(self.fragments.len());
        for slot in &self.fragments {
            match slot {
                Some(JsonValue::Array(items_of_entry)) => {
                    items.extend(items_of_entry.iter().cloned())
                }
                Some(other) => items.push(other.clone()),
                None => return None,
            }
        }
        Some(JsonValue::Array(items))
    }
}

/// Build fragments for a FULL projection (first push / structure change):
/// one `to_value` per projected entry, then the aggregate + its text built
/// once from the fragments. The old round-1 shape paid the aggregate
/// `to_value` AND the per-entry splices on EVERY push; this pays them once
/// per structure change.
pub fn build_projection_entry_fragments(
    projection: &SessionProjection,
    js_face: bool,
) -> ProjectionEntryFragments {
    let fragments: Vec<Option<JsonValue>> = (0..projection.entries.len())
        .map(|index| ProjectionEntryFragments::build_fragment(projection, index).map(|(v, _)| v))
        .collect();
    let set = ProjectionEntryFragments {
        fragments,
        aggregate: None,
        aggregate_text: None,
        projection_json: None,
    };
    match set.flat_aggregate() {
        Some(aggregate) => {
            // A native-only host (no JS `ctx.sessionManager`) reads the
            // aggregate `entries` VALUE only: the aggregate text and the
            // `{entries, messages}` projection object text have no reader
            // and are not rendered/retained.
            if !js_face {
                return ProjectionEntryFragments {
                    aggregate: Some(Arc::new(aggregate)),
                    aggregate_text: None,
                    projection_json: None,
                    ..set
                };
            }
            let text = aggregate.to_string();
            let json = projection_object_text(projection, &set.fragments, Some(&text));
            ProjectionEntryFragments {
                aggregate: Some(Arc::new(aggregate)),
                aggregate_text: Some(text),
                projection_json: Some(json),
                ..set
            }
        }
        None => set,
    }
}

/// The `{entries:[...views],"messages":[<aggregate text>]}` object text,
/// rendered from the projection's borrowed slices (byte-identical to the
/// old per-push splice). Called on the FULL build only — appends splice the
/// cached text.
fn projection_object_text(
    projection: &SessionProjection,
    fragments: &[Option<JsonValue>],
    aggregate_text: Option<&str>,
) -> String {
    let entries_len = aggregate_text.map_or(0, str::len);
    let mut out = String::with_capacity(entries_len * 2 + 32);
    out.push_str("{\"entries\":[");
    let mut first_entry = true;
    for (index, entry) in projection.entries.iter().enumerate() {
        if first_entry {
            first_entry = false;
        } else {
            out.push(',');
        }
        out.push_str("{\"entry\":");
        out.push_str(
            &serde_json::to_string(entry.source_entry.as_ref())
                .unwrap_or_else(|_| "null".to_owned()),
        );
        out.push_str(",\"messages\":");
        match fragments
            .get(index)
            .and_then(|slot| slot.as_ref())
            .map(serde_json::to_string)
        {
            Some(Ok(text)) => out.push_str(&text),
            _ => {
                let slice = entry.messages_in(&projection.messages);
                match serde_json::to_string(slice) {
                    Ok(text) => out.push_str(&text),
                    Err(_) => out.push_str("null"),
                }
            }
        }
        out.push('}');
    }
    out.push_str("],\"messages\":");
    out.push_str(aggregate_text.unwrap_or("null"));
    out.push('}');
    out
}

/// Extend cached fragments by the appended entries' fragments (the projection
/// index contract: `cached.len() == appended_from`, entries only append).
///
/// The cached aggregate VALUE rides the old `Arc`'s items (a clone per
/// message — the one term the native `Arc<JsonValue>` contract forces; every
/// alternative measured equal or worse, see the struct docs). The cached
/// aggregate TEXT and the cached PROJECTION TEXT are SPLICED (one copy of
/// the cached strings + the appended texts) instead of re-serializing ~8k
/// retained messages and ~8k retained views per push.
pub fn extend_projection_entry_fragments(
    cached: ProjectionEntryFragments,
    projection: &SessionProjection,
    appended_from: usize,
    js_face: bool,
) -> ProjectionEntryFragments {
    debug_assert!(cached.fragments.len() <= appended_from);
    // A shrink (branch/trim) reaches here only through the caller's guard;
    // guard again so a debug_assert-enabled release build cannot underflow.
    if cached.fragments.len() > appended_from {
        return build_projection_entry_fragments(projection, js_face);
    }
    // One `build_fragment` per appended entry: the value joins the cached
    // fragment slots AND its text feeds both text splices below. The +2
    // capacity headroom keeps the extended fragment vec at ONE allocation
    // (a single push append is the common delta).
    // One `build_fragment` per appended entry: the value joins the cached
    // fragment slots; its text (JS splice fuel only) is built when a JS face
    // exists. A native-only host reads the aggregate `entries` VALUE, so the
    // per-entry text is skipped outright.
    let mut appended: Vec<Option<(JsonValue, String)>> =
        Vec::with_capacity(projection.entries.len().saturating_sub(appended_from));
    for index in appended_from..projection.entries.len() {
        appended.push(ProjectionEntryFragments::build_fragment(projection, index));
    }
    let appended_count = appended.iter().filter(|slot| slot.is_some()).count();
    let mut set = cached;
    let mut new_texts: Vec<String> = Vec::with_capacity(if js_face { appended.len() } else { 0 });
    let mut new_slots: Vec<Option<JsonValue>> = Vec::with_capacity(appended.len());
    for slot in appended {
        match slot {
            Some((value, text)) => {
                if js_face {
                    new_texts.push(text);
                }
                new_slots.push(Some(value));
            }
            None => new_slots.push(None),
        }
    }
    set.fragments.reserve(new_slots.len());
    set.fragments.extend(new_slots);
    let appended_some = appended_from < projection.entries.len() && appended_count > 0;
    if appended_some {
        // Aggregate VALUE: the flat concat over the fragment slots — the
        // appended slots are the new values, the retained slots are the SAME
        // values the old aggregate held (no re-serialization; the per-message
        // clone is the one term the native `Arc<JsonValue>` contract forces).
        let mut items: Vec<JsonValue> = Vec::with_capacity(set.fragments.len() + 8);
        let mut aggregate_ok = true;
        for slot in set.fragments.iter() {
            match slot {
                Some(JsonValue::Array(items_of_entry)) => {
                    items.extend(items_of_entry.iter().cloned())
                }
                Some(other) => items.push(other.clone()),
                None => {
                    aggregate_ok = false;
                    break;
                }
            }
        }
        if aggregate_ok {
            if !js_face {
                // No JS reader: keep the text slots cleared (a later JS-face
                // push re-renders them from the cached fragments, the same
                // fallback a set without a cached text already takes).
                set.aggregate = Some(Arc::new(JsonValue::Array(items)));
                set.aggregate_text = None;
                set.projection_json = None;
                return set;
            }
            let text = match &set.aggregate_text {
                // Splice: `[...cached items]` + `,` + the appended entries'
                // MESSAGE texts (inside their fragment arrays — each fragment
                // text is `[m1,...]`, so the inner part joins the flat list).
                // An EMPTY cached aggregate (`"[]"`) cannot splice: stripping
                // its closing bracket leaves `[`, and the first appended
                // message would get a leading comma — invalid JSON in the
                // pushed envelope (PIR-BIN-106; reachable whenever the
                // retained path holds only zero-message entries, e.g. a new
                // session's startup rows). The fallback re-renders the whole
                // aggregate instead.
                Some(cached_text) if cached_text.len() >= 2 && cached_text != "[]" => {
                    let mut text = String::with_capacity(
                        cached_text.len() + new_texts.iter().map(|t| t.len() + 1).sum::<usize>(),
                    );
                    text.push_str(&cached_text[..cached_text.len() - 1]);
                    for new_text in &new_texts {
                        // A state entry (label, model change) projects ZERO
                        // messages: its fragment text is the empty array
                        // `[]`, which contributes NOTHING to the flat
                        // aggregate — pushing `,` + the unwrapped empty
                        // string would produce a trailing comma.
                        let inner = new_text
                            .strip_prefix('[')
                            .and_then(|rest| rest.strip_suffix(']'));
                        if let Some(inner) = inner {
                            if inner.is_empty() {
                                continue;
                            }
                            text.push(',');
                            text.push_str(inner);
                        } else {
                            text.push(',');
                            text.push_str(new_text);
                        }
                    }
                    text.push(']');
                    text
                }
                _ => JsonValue::Array(items.clone()).to_string(),
            };
            set.aggregate = Some(Arc::new(JsonValue::Array(items)));
            set.aggregate_text = Some(text);
            // The projection object text: splice the appended views in
            // before `],"messages":[` (the cached json ends with the
            // aggregate text, so the views' close boundary is found by
            // trimming the known tail: `],"messages":[<agg text>]}`).
            let new_views_len: usize = new_texts
                .iter()
                .map(|t| t.len() + 3) // `,` + `{"entry":...}` wrapper is
                // rendered below from the projection.
                .sum();
            set.projection_json = match (&set.projection_json, &set.aggregate_text) {
                (Some(cached_json), Some(_)) if cached_json.len() > 16 => {
                    // Find the boundary: the LAST `],"messages":[` (the
                    // views close with `}],"messages":[`; the marker's
                    // `]` CLOSES the entries array). The splice keeps
                    // everything BEFORE that closing bracket, appends the
                    // new views, and re-attaches the marker + aggregate.
                    let marker = "],\"messages\":[";
                    match cached_json.rfind(marker) {
                        Some(boundary) => {
                            let mut json =
                                String::with_capacity(cached_json.len() + new_views_len + 64);
                            // Up to (excluding) the closing `]` of the
                            // entries array — the appended views join it.
                            json.push_str(&cached_json[..boundary]);
                            for (index, entry) in
                                projection.entries[appended_from..].iter().enumerate()
                            {
                                json.push_str(",{\"entry\":");
                                json.push_str(
                                    &serde_json::to_string(entry.source_entry.as_ref())
                                        .unwrap_or_else(|_| "null".to_owned()),
                                );
                                json.push_str(",\"messages\":");
                                if let Some(new_text) = new_texts.get(index) {
                                    json.push_str(new_text);
                                } else {
                                    let slice = entry.messages_in(&projection.messages);
                                    match serde_json::to_string(slice) {
                                        Ok(text) => json.push_str(&text),
                                        Err(_) => json.push_str("null"),
                                    }
                                }
                                json.push('}');
                            }
                            // The aggregate text carries its own
                            // `[...]` wrapper and closing bracket, so
                            // the tail is the field name plus the
                            // object's closing brace only.
                            json.push_str("],\"messages\":");
                            json.push_str(set.aggregate_text.as_deref().unwrap_or("null"));
                            json.push('}');
                            Some(json)
                        }
                        None => Some(projection_object_text(
                            projection,
                            &set.fragments,
                            set.aggregate_text.as_deref(),
                        )),
                    }
                }
                _ => Some(projection_object_text(
                    projection,
                    &set.fragments,
                    set.aggregate_text.as_deref(),
                )),
            };
        } else {
            match set.flat_aggregate() {
                Some(aggregate) => {
                    if js_face {
                        let text = aggregate.to_string();
                        set.projection_json = Some(projection_object_text(
                            projection,
                            &set.fragments,
                            Some(&text),
                        ));
                        set.aggregate_text = Some(text);
                    } else {
                        set.aggregate_text = None;
                        set.projection_json = None;
                    }
                    set.aggregate = Some(Arc::new(aggregate));
                }
                None => {
                    set.aggregate = None;
                    set.aggregate_text = None;
                    set.projection_json = None;
                }
            }
        }
    }
    set
}

/// [`build_session_projection`]'s LLM-visible messages serialized to the
/// upstream `AgentMessage` JSON shape, WITHOUT materializing the aggregate
/// `Vec<Message>`.
///
/// MEMORY SHAPE (pir extension): `AgentSession::transcript_json` (RPC
/// `get_messages`, the SDK `transcript_json`, `/export`) only needs JSON
/// values. Building the projection first cloned every message of the resident
/// transcript, then serialized the clone and dropped it. This walks the same
/// retained entries and serializes each plain `message` entry straight from
/// its stored payload (byte-identical JSON: both sides use the same role-tagged
/// structs), materializing a `Message` only for the entries whose model view
/// differs from the stored record (bash executions, custom/system messages,
/// summaries, context edits).
///
/// Takes the WHOLE FILE's entries (the PIR-BIN-092 bridge applies when
/// `bridge_missing_parents` is true); a paged caller widens first
/// ([`SessionManager::widen_to_full`], PIR-SDK-025) and passes the flag that
/// matches whatever slice it actually holds — a FAILED widen must pass the
/// windowed rule, or the walk grafts abandoned-branch rows (PIR-BIN-103).
pub fn build_session_context_json<E: Borrow<FileEntry>>(
    entries: &[E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
) -> Vec<JsonValue> {
    let (_path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, None);
    // Exact output capacity: the message-count contract the projection and
    // its metadata twin share. One pass over entries already in cache, no
    // growth reallocations on a transcript-sized resident window.
    let capacity: usize = context_entries
        .iter()
        .enumerate()
        .map(|(index, source_entry)| {
            let entry: &FileEntry = (*source_entry).borrow();
            if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
                0
            } else {
                projected_message_count(entry, context_edit_for(entry, edits.as_ref()))
            }
        })
        .sum();
    let mut out: Vec<JsonValue> = Vec::with_capacity(capacity);
    // Reused scratch for the entries whose model view is not their stored
    // record: one allocation across the whole walk instead of one per entry.
    let mut scratch: Vec<Message> = Vec::new();
    for (index, source_entry) in context_entries.iter().enumerate() {
        let entry: &FileEntry = (*source_entry).borrow();
        if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
            continue;
        }
        let edit = context_edit_for(entry, edits.as_ref());
        if edit.is_some_and(|edit| edit.replacement.is_none()) {
            continue;
        }
        if edit.is_none() {
            if let FileEntry::Message(inner) = entry {
                match &inner.message {
                    // The stored payload IS the upstream message object for
                    // these roles; serialize it by reference.
                    StoredMessage::Assistant(_)
                    | StoredMessage::ToolResult(_)
                    | StoredMessage::User(_) => {
                        out.push(message_json_value_from_stored(&inner.message));
                        continue;
                    }
                    // A `!!` bash record stays out of the model context.
                    StoredMessage::BashExecution(bash) if bash.excluded() => continue,
                    _ => {}
                }
            }
        }
        scratch.clear();
        push_context_entry_messages(entry, edit, &mut scratch);
        out.extend(scratch.iter().map(message_json_value));
    }
    out
}

fn message_json_value(message: &Message) -> JsonValue {
    serde_json::to_value(message).unwrap_or(JsonValue::Null)
}

fn message_json_value_from_stored(message: &StoredMessage) -> JsonValue {
    serde_json::to_value(message).unwrap_or(JsonValue::Null)
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
pub fn current_system_message_from_entries<E: Borrow<FileEntry>>(
    entries: &[E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
) -> Option<StoredSystemMessage> {
    let (_path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, None);

    let mut system_messages: Vec<Message> = Vec::new();
    for (index, source_entry) in context_entries.iter().enumerate() {
        let entry: &FileEntry = (*source_entry).borrow();
        // Only the newest compaction contributes (same rule as the projection).
        if index > 0 && matches!(entry, FileEntry::Compaction(_)) {
            continue;
        }
        if let Some(edit) = context_edit_for(entry, edits.as_ref()) {
            if edit.replacement.is_none() {
                continue;
            }
        }
        match entry {
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
pub fn last_assistant_text_from_entries<E: Borrow<FileEntry>>(
    entries: &[E],
    leaf_id: Option<&str>,
) -> Option<String> {
    last_assistant_text_from_entries_indexed(entries, leaf_id, true, None)
}

/// [`last_assistant_text_from_entries`] over the manager's persistent
/// [`EntryPositions`] (the RPC state-poll path).
/// `bridge_missing_parents`: the slice contract (see [`retained_context`]).
pub(crate) fn last_assistant_text_from_entries_indexed<E: Borrow<FileEntry>>(
    entries: &[E],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> Option<String> {
    use crate::ai::types::StopReason;

    let (_path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, positions);

    for source_entry in context_entries.iter().rev() {
        let entry: &FileEntry = (*source_entry).borrow();
        let FileEntry::Message(inner) = entry else {
            continue;
        };
        let StoredMessage::Assistant(assistant) = &inner.message else {
            continue;
        };
        let edit = context_edit_for(entry, edits.as_ref());
        if let Some(edit) = edit {
            if edit.replacement.is_none() {
                // `replacement: null` omits the target from context.
                continue;
            }
        }
        // A replacement on an assistant message rewrites its content
        // (`replace_message_content`); the text walk must see that content,
        // not the stored one. The stored content is read by reference — the
        // old `assistant.content.clone()` copied the whole message (tool
        // results included) on every poll just to join its text blocks.
        let replaced = edit
            .and_then(|edit| edit.replacement.as_ref())
            .map(|replacement| content_blocks(&replacement.content));
        let content: &[ContentBlock] = replaced.as_deref().unwrap_or(&assistant.content);
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

// ---------------------------------------------------------------------------
// RSS-B: incremental prompt projection cache
// ---------------------------------------------------------------------------

/// A snapshot of the last successfully built prompt projection, keyed by the
/// session head it describes.
///
/// The messages are moved in/out of the cache — never cloned on the steady
/// path — so the next prompt only materializes the entries appended since
/// [`Self::leaf_entry`]. `leaf_entry` is the exact `Arc` the projection ended
/// on; comparing it with [`Arc::ptr_eq`] also rejects a reloaded/replaced
/// session that happens to reuse ids.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CachedSessionProjection {
    pub(crate) leaf_id: Option<String>,
    pub(crate) leaf_entry: Option<Arc<FileEntry>>,
    pub(crate) messages: Vec<Message>,
}

/// The proof [`extend_cached_session_projection`] leaves behind for the
/// run-end park — the R3 token that turns the park validation from two
/// transcript-sized walks into an O(delta) one.
///
/// The extend validated, for the projection it returned at `leaf_id`:
/// every entry appended since the previous parked leaf was append-safe and
/// its materialized message count matched the projection contract, and the
/// retained prefix satisfied the count contract. Together those prove
/// `message_count == projected_message_count_for_leaf(leaf_id)` — the number
/// the full park validation would re-derive — so the park only has to
/// re-check what THIS run appended after the leaf.
///
/// The token is withheld (the extend returns `None` for it) whenever the
/// retained context carries a `context_edit`: an edit could name a target
/// that does not exist yet (hand-edited file), which no delta-only walk can
/// price; the park then runs the unchanged full validation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParkToken {
    /// The leaf the extended projection ended on. The park's delta walk stops
    /// as soon as it reaches this entry — everything up to and including it
    /// is the prefix the extend already proved. `None` is the empty-session
    /// snapshot: the whole leaf path is the delta.
    pub(crate) leaf_id: Option<String>,
    /// Model-visible message count of the extended projection, proven equal
    /// to a full build's count at `leaf_id` (see the type docs).
    pub(crate) message_count: usize,
    /// The manager's storage generation at the extend (PIR-BIN-102/104): the
    /// proof holds for the slice contract it was built under, and every
    /// slice-identity change — a concurrent widen, a trim, a reload, a
    /// replacement — goes through `build_index`, which moves this generation.
    /// Comparing it makes the guard cover a widen followed by a re-trim to a
    /// NEW window (same `is_paged()` boolean, different resident base), and
    /// [`SessionManager::validate_park_delta`] then refuses the fast path and
    /// runs the full validation under the CURRENT semantics.
    pub(crate) storage_generation: u64,
}

/// Outcome of [`SessionManager::build_session_projection_with_cache`].
pub(crate) struct CachedProjectionOutcome {
    pub(crate) projection: SessionProjection,
    /// Whether the projection was served by extending the cached snapshot
    /// (drives `projection_reuse_count`; `projection_build_count` moves only
    /// on the full-build fallback).
    pub(crate) reused: bool,
    /// Park proof for the run-end validation; `None` means the park must run
    /// the full validation (full build, retained context edit, empty cache).
    pub(crate) park_token: Option<ParkToken>,
}

/// Build a projection by extending `cached` with the entries appended after
/// its leaf. Returns `None` — the caller must fall back to
/// [`build_session_projection`] — when the session did not grow by pure
/// append:
///
/// - the cached leaf is no longer on the live leaf path (branch switch,
///   rewrite, storage swap);
/// - the appended range contains a `compaction` or `context_edit` (both change
///   the meaning of already-projected messages);
/// - the cached message count no longer matches the retained-entry count
///   contract (defensive: any stale/foreign snapshot).
///
/// The walk below is metadata-only over the OLD entries; only the appended
/// entries run [`push_context_entry_messages`]. That is the per-turn
/// transcript-sized materialization H1 removed.
///
/// On success it also returns the [`ParkToken`] the run-end park validates
/// against (`None` when the retained context carries a context edit; see the
/// token docs).
pub(crate) fn extend_cached_session_projection(
    entries: &[Arc<FileEntry>],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
    storage_generation: u64,
    cached: CachedSessionProjection,
) -> Option<(SessionProjection, Option<ParkToken>)> {
    let CachedSessionProjection {
        leaf_id: cached_leaf_id,
        leaf_entry: cached_leaf_entry,
        mut messages,
    } = cached;

    // One leaf-path walk feeds both the append delta and the retained range —
    // the same selection a full build uses, without touching a single message.
    let (path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, positions);

    let cached_path_index: Option<usize> = match (&cached_leaf_entry, &cached_leaf_id) {
        (Some(leaf_entry), Some(_)) => Some(
            path.iter()
                .position(|entry| Arc::ptr_eq(leaf_entry, *entry))?,
        ),
        (None, None) => {
            // The empty-session snapshot can only extend an empty message
            // aggregate; everything below must then be an append contribution.
            if !messages.is_empty() {
                return None;
            }
            None
        }
        _ => return None,
    };

    let appended_path = match cached_path_index {
        Some(index) => &path[index + 1..],
        None => &path[..],
    };
    for source in appended_path {
        let entry: &FileEntry = (*source).borrow();
        if matches!(
            entry,
            FileEntry::Compaction(_) | FileEntry::ContextEdit(_) | FileEntry::Session(_)
        ) {
            return None;
        }
    }

    // The cached leaf itself must still be a retained context entry (state
    // entries are retained with an empty range; only filtered system entries
    // inside a kept range disappear, and those can never be the cached leaf of
    // a successful build).
    let cached_context_index: Option<usize> = match (&cached_leaf_entry, cached_path_index) {
        (Some(leaf_entry), Some(_)) => Some(
            context_entries
                .iter()
                .position(|entry| Arc::ptr_eq(leaf_entry, *entry))?,
        ),
        (None, None) => None,
        _ => return None,
    };

    // Message-count contract: the cached aggregate must be exactly what a full
    // build would have produced for the retained prefix ending at the cached
    // leaf. This catches a stale cache whose leaf entry is shared but whose
    // path/settings changed around it.
    if let Some(cached_index) = cached_context_index {
        let mut prefix_count = 0usize;
        for (index, source) in context_entries.iter().enumerate() {
            if index > cached_index {
                break;
            }
            let entry: &FileEntry = (*source).borrow();
            if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
                continue;
            }
            prefix_count += projected_message_count(entry, context_edit_for(entry, edits.as_ref()));
        }
        if prefix_count != messages.len() {
            return None;
        }
    }

    let mut projected: Vec<ProjectedSessionEntry> = Vec::with_capacity(context_entries.len());
    let mut cursor = 0usize;
    for (index, source) in context_entries.iter().enumerate() {
        let entry: &FileEntry = (*source).borrow();
        let is_stale_compaction = matches!(entry, FileEntry::Compaction(_)) && index > 0;
        let edit = context_edit_for(entry, edits.as_ref());
        let is_appended = cached_context_index.is_none_or(|cached_index| index > cached_index);
        let start = cursor;
        if !is_stale_compaction {
            if is_appended {
                let before = messages.len();
                push_context_entry_messages(entry, edit, &mut messages);
                let expected = projected_message_count(entry, edit);
                if messages.len() - before != expected {
                    // The count contract is broken; never publish a projection
                    // whose entry ranges disagree with its messages.
                    return None;
                }
                cursor = messages.len();
            } else {
                cursor += projected_message_count(entry, edit);
            }
        }
        projected.push(ProjectedSessionEntry {
            source_entry: (*source).shared_entry(),
            messages: start..cursor,
        });
    }
    if cursor != messages.len() {
        return None;
    }

    let (thinking_level, model) = get_session_context_settings(&path);
    let projection = SessionProjection {
        entries: projected,
        messages,
        thinking_level,
        model,
    };
    // R3: the park proof. Everything above proved this aggregate's count
    // equals a full build at `leaf_id`; a retained context edit withholds the
    // token (see [`ParkToken`]) so the park falls back to the full validation.
    let park_token = edits.is_none().then(|| ParkToken {
        leaf_id: leaf_id.map(str::to_owned),
        message_count: projection.messages.len(),
        storage_generation,
    });
    Some((projection, park_token))
}

/// The number of model-visible messages a full projection of `leaf_id` would
/// contain, computed without materializing any message. Used to validate a
/// run's final context before it is parked in the incremental cache.
pub(crate) fn projected_message_count_for_leaf(
    entries: &[Arc<FileEntry>],
    leaf_id: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> usize {
    let (_path, context_entries, edits) =
        retained_context(entries, leaf_id, bridge_missing_parents, positions);
    context_entries
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let entry: &FileEntry = (*source).borrow();
            if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
                0
            } else {
                projected_message_count(entry, context_edit_for(entry, edits.as_ref()))
            }
        })
        .sum()
}

/// Whether the leaf-path segment `(from_leaf, to_leaf]` contains only entries
/// whose contribution can be appended to an existing projection. Compaction
/// and context edits invalidate; state entries and plain messages/summaries
/// are append-safe.
pub(crate) fn path_delta_is_append_only(
    entries: &[Arc<FileEntry>],
    from_leaf: Option<&str>,
    to_leaf: Option<&str>,
    bridge_missing_parents: bool,
    positions: Option<&EntryPositions>,
) -> bool {
    let path = build_session_path_entries(entries, to_leaf, bridge_missing_parents, positions);
    let start = match from_leaf {
        None => 0,
        Some(from) => match path.iter().position(|entry| {
            let entry: &FileEntry = (**entry).borrow();
            entry.id() == Some(from)
        }) {
            Some(index) => index + 1,
            None => return false,
        },
    };
    path[start..].iter().all(|source| {
        let entry: &FileEntry = (*source).borrow();
        !matches!(
            entry,
            FileEntry::Compaction(_) | FileEntry::ContextEdit(_) | FileEntry::Session(_)
        )
    })
}

impl SessionManager {
    /// RSS-B projection entry point: reuse `cached` when the session grew by
    /// pure append, else fall back to the full builder. The outcome carries
    /// the reuse flag plus the [`ParkToken`] the run-end park validates
    /// against (`None` = full validation).
    ///
    /// `projection_build_count` deliberately counts FULL builds only; the
    /// caller tracks reuses separately (`projection_reuse_count`).
    pub(crate) fn build_session_projection_with_cache(
        &self,
        cached: Option<CachedSessionProjection>,
    ) -> CachedProjectionOutcome {
        if let Some(cached) = cached {
            if let Some((projection, park_token)) = extend_cached_session_projection(
                &self.file_entries,
                self.leaf_id.as_deref(),
                !self.is_paged(),
                Some(&self.entry_positions),
                self.storage_generation(),
                cached,
            ) {
                return CachedProjectionOutcome {
                    projection,
                    reused: true,
                    park_token,
                };
            }
        }
        self.projection_builds.fetch_add(1, Ordering::Relaxed);
        CachedProjectionOutcome {
            projection: build_session_projection_indexed(
                &self.file_entries,
                self.leaf_id.as_deref(),
                !self.is_paged(),
                Some(&self.entry_positions),
            ),
            reused: false,
            park_token: None,
        }
    }

    /// R3: the run-end park validation over the delta the prompt-start extend
    /// did NOT already prove — the entries this run appended after the
    /// token's leaf ([`park_delta_validates`]).
    pub(crate) fn validate_park_delta(
        &self,
        token: &ParkToken,
        leaf_id: Option<&str>,
        run_start_leaf: Option<&str>,
        parked_message_count: usize,
    ) -> Option<bool> {
        // PIR-BIN-102/104: the proof holds for the SLICE IDENTITY it was
        // built under, not merely for the `is_paged` boolean. The token
        // records the storage generation at the extend; every slice-identity
        // change (widen, trim, reload, replacement) rebuilds the index and
        // moves it. This covers the boolean-restoring transition too (a
        // widen followed by a re-trim into a NEW window): a mismatch falls
        // back to the full validation (None) instead of concluding on a
        // proof with stale slice semantics.
        if token.storage_generation != self.storage_generation() {
            return None;
        }
        park_delta_validates(
            &self.file_entries,
            leaf_id,
            run_start_leaf,
            !self.is_paged(),
            &self.entry_positions,
            token,
            parked_message_count,
        )
    }

    /// PIR-BIN-104 in-crate differential probe: mint a park token through the
    /// real extend path on a live-trimmed manager, append `appended`, then
    /// perform the window transition no current caller reaches — widen to
    /// full, then re-trim into a NEW window — and report
    /// `(guard_decision, fast, full)` for the park at the new leaf.
    ///
    /// `guard_decision == None` means the seam refused the fast path and the
    /// caller runs the unchanged full validation. The pre-fix hole reported
    /// `(Some(true), Some(true), false)`: the boolean guard passed on the
    /// restored `is_paged()` while the token's proven base described the OLD
    /// window (fast accepts, full rejects).
    ///
    /// The caller must have a flushed, live-tail-enabled manager; every
    /// appended message must project exactly ONE model-visible message.
    /// Returns `None` when the fixture premise does not hold (no token, an
    /// append/widen/trim failure) — test-only, so the caller asserts.
    #[doc(hidden)]
    #[cfg(feature = "session")]
    pub fn park_guard_widen_then_retrim_probe_for_test(
        &mut self,
        appended: &[crate::ai::types::Message],
        retrim_entries: usize,
    ) -> Option<(Option<bool>, Option<bool>, bool)> {
        let first = self.build_session_projection_with_cache(None);
        let leaf_entry = self.leaf_id.as_deref().and_then(|id| {
            self.file_entries
                .iter()
                .find(|entry| entry.id() == Some(id))
                .cloned()
        });
        let cached = CachedSessionProjection {
            leaf_id: self.leaf_id.clone(),
            leaf_entry,
            messages: first.projection.messages,
        };
        let extended = self.build_session_projection_with_cache(Some(cached));
        let token = extended.park_token.clone()?;
        let base = extended.projection.messages.len();
        for message in appended {
            self.append_message(message.clone()).ok()?;
        }
        let parked = base + appended.len();
        let run_start_leaf = token.leaf_id.clone();
        self.widen_to_full().ok()?;
        self.trim_resident_tail(usize::MAX, retrim_entries).ok()?;
        let leaf_id = self.leaf_id.clone();
        let guard = self.validate_park_delta(
            &token,
            leaf_id.as_deref(),
            run_start_leaf.as_deref(),
            parked,
        );
        let fast = park_delta_validates(
            &self.file_entries,
            leaf_id.as_deref(),
            run_start_leaf.as_deref(),
            !self.is_paged(),
            &self.entry_positions,
            &token,
            parked,
        );
        let full = self.projected_message_count_for_leaf(leaf_id.as_deref()) == parked
            && self.path_delta_is_append_only(run_start_leaf.as_deref(), leaf_id.as_deref());
        Some((guard, fast, full))
    }
}

/// R3: validate a completed run's aggregate against the prompt-start
/// [`ParkToken`] in O(delta) instead of the full transcript.
///
/// Walks parent pointers from `leaf_id` (the run's final leaf) DOWN to
/// `run_start_leaf` (the run's start anchor) through the persistent index with
/// the SAME rules `build_session_path_entries` uses (validated id→position
/// resolution, the PIR-BIN-092 broken-chain bridge, the repeated-id cycle
/// guard). That walk passes the token's leaf on the way and stops there for
/// nothing: it is exactly the range today's
/// [`path_delta_is_append_only`] slices out of a full path, and exactly the
/// base today's [`projected_message_count_for_leaf`] needs
///
/// - every entry between the anchor and the leaf is append-safe (a
///   compaction / context_edit / session entry means a full projection at
///   `leaf_id` is not the extended aggregate plus this delta — `Some(false)`,
///   the same rejection the full checks produce);
/// - entries above the token's leaf are the run's own delta; their projected
///   contribution plus the token's proven count must reconcile the parked
///   aggregate — the same absolute equality the full count check computes,
///   reached from the extend's base (no retained `context_edit` can exist; see
///   [`ParkToken`]). Entries between the anchor and the token's leaf are
///   already priced inside that base and are only append-safety-checked.
///
/// `None` means the walk cannot conclude — `leaf_id` is not resolvable, the
/// anchor or the token's leaf is no longer on the live path (branch reset /
/// rewrite / window slide), the chain cycles first, or the token is the
/// empty-session snapshot. The caller must then run the unchanged full
/// validation, so park behavior is never weaker than before.
///
/// `bridge_missing_parents` is the slice contract (PIR-BIN-101, see
/// [`retained_context`]): on a WINDOWED slice the walk never bridges, so a
/// window whose chain leaves the slice before the anchor resolves to `None`
/// (cannot prove) instead of concluding over off-path rows.
pub(crate) fn park_delta_validates(
    entries: &[Arc<FileEntry>],
    leaf_id: Option<&str>,
    run_start_leaf: Option<&str>,
    bridge_missing_parents: bool,
    positions: &EntryPositions,
    token: &ParkToken,
    parked_message_count: usize,
) -> Option<bool> {
    let index = LeafPathIndex::Persistent(positions, entries);
    // The park resolved `leaf_id` from the resident entries before calling;
    // a position map that disagrees means a foreign/stale index, so leave the
    // answer to the full validation (it falls back to the last non-header
    // entry, the same path builder rule).
    let leaf_id = leaf_id?;
    let Some(token_leaf_id) = token.leaf_id.as_deref() else {
        // Empty-session snapshot: no proven count base to anchor a split at.
        return None;
    };
    let mut current = Some(index.resolve(leaf_id)?);
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut delta_count = 0usize;
    let mut passed_token_leaf = false;
    while let Some((position, source)) = current {
        let entry: &FileEntry = (*source).borrow();
        if let Some(id) = entry.id() {
            if !visited.insert(id) {
                // Cycle: the path builder stops at the repeat too; what that
                // truncated path means is the full validation's call.
                return None;
            }
        }
        // The run's start anchor ends the (run_start_leaf, leaf_id] range
        // (exclusive of the anchor), exactly as the full check slices it.
        if entry.id() == run_start_leaf {
            if !passed_token_leaf && entry.id() != Some(token_leaf_id) {
                // The extend's leaf is not on this path: the base is
                // meaningless, let the full validation decide.
                return None;
            }
            return Some(token.message_count + delta_count == parked_message_count);
        }
        if matches!(
            entry,
            FileEntry::Compaction(_) | FileEntry::ContextEdit(_) | FileEntry::Session(_)
        ) {
            return Some(false);
        }
        if !passed_token_leaf {
            if entry.id() == Some(token_leaf_id) {
                passed_token_leaf = true;
            } else {
                delta_count += projected_message_count(entry, None);
            }
        }
        current = match entry.parent_id() {
            None => None,
            Some(parent) => match index.resolve(parent) {
                Some(slot) => Some(slot),
                // PIR-BIN-101: only a whole-file slice may bridge a missing
                // parent; on a window the parent is the link into the
                // streamed prefix and nothing across it can be proven.
                None if bridge_missing_parents => bridging_predecessor(position, entries),
                None => return None,
            },
        };
    }
    // The chain ended before the anchor. A run that started from an empty
    // session (no anchor) owns the whole path — today's `from_leaf: None`
    // rule — but the extend's leaf must still have been on it.
    if run_start_leaf.is_none() && passed_token_leaf {
        return Some(token.message_count + delta_count == parked_message_count);
    }
    None
}
