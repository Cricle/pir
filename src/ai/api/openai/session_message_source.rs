//! File-backed [`MessageSource`] for the print widen fallback.
//!
//! The widen fallback used to call `SessionManager::widen_to_full` (parsing
//! the whole JSONL into `Vec<Arc<FileEntry>>`) and then
//! `build_session_projection` (a second, transcript-sized `Vec<Message>`).
//! For a 100 MB session that is ~205 MB live before the request body is even
//! built. This module keeps the same projection semantics but never retains
//! either aggregate:
//!
//! 1. **Index pass** (one sequential read): parse each line just far enough to
//!    learn `id`/`parentId`/kind and its byte offset. The index is bounded
//!    (one small record per entry) and is dropped after the selected path is
//!    resolved.
//! 2. **Path + retention**: walk leaf→root over the index (the same fallback
//!    and cycle rules as `projection::build_session_path_entries`), then apply
//!    the projection's compaction retention (`[newest compaction]` plus the
//!    kept range and everything after it, retaining system entries only after
//!    the cut) and the latest context edit per target.
//! 3. **Summary pass** (one parse of the selected entries): compute the counts
//!    the replay plan needs. Only system/developer messages are kept (they are
//!    folded into the leading prompt by some APIs), never the transcript.
//! 4. **Iteration**: seek to one selected entry's offset, parse it, convert it
//!    to 0..2 model messages, and hand them out; the entry and its messages are
//!    dropped before the next entry is read.
//!
//! Equivalence is pinned by `tests/lane_e_streamed_widen.rs`: the collected
//! source messages and the serialized request body must equal
//! `build_session_projection().messages` / the slice-built body on a fixture
//! covering every entry kind, context edits, compaction retention and a
//! branched leaf path.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::ai::types::{
    approximate_message_json_bytes, message_has_tool_history, Message, MessageSource,
    MessageSourceSummary, SourceMessage,
};
use crate::session_manager::projection::push_entry_context_messages as projection_push_messages;
use crate::session_manager::{
    parse_entry_line_with_version, ContextEditEntry, FileEntry, SessionManager,
};
/// One selected entry: where its JSONL line lives and which context edit (if
/// any) the projection would apply to it.
struct SelectedEntry {
    offset: u64,
    len: usize,
    edit: Edit,
}

enum Edit {
    Keep,
    Omit,
    Replace(serde_json::Value),
}

/// A replayable, file-backed model transcript for one pinned leaf.
///
/// Construction costs one sequential scan plus one parse of the selected
/// entries; iteration re-reads only the selected entries. Steady-state memory
/// is the bounded per-entry index (offsets/kinds) plus the small replay-plan
/// summary — never the transcript.
pub struct SessionMessageSource {
    path: PathBuf,
    header_version: u32,
    entries: Vec<SelectedEntry>,
    summary: MessageSourceSummary,
}

impl SessionMessageSource {
    /// Build the source for `session`'s file at `leaf_id` (the caller pins the
    /// leaf; later appends are not observed by this source).
    pub fn build(session: &SessionManager, leaf_id: Option<&str>) -> crate::Result<Self> {
        let Some(path) = session.session_file() else {
            return Err(anyhow::anyhow!(
                "session message source needs a persisted session file"
            ));
        };
        Self::build_from_file(path, leaf_id)
    }

    /// Build the source directly from a session JSONL file. Public for tests
    /// and embedders that hold a path but no manager.
    pub fn build_from_file(path: &Path, leaf_id: Option<&str>) -> crate::Result<Self> {
        let scanned = scan_index(path)?;
        let path_ids = resolve_path(&scanned, leaf_id);
        let retained = retained_entry_ids(&scanned, &path_ids);
        let edit_index = retained_context_edits(&scanned, &retained);
        let entries: Vec<SelectedEntry> = retained
            .iter()
            .map(|id| {
                let meta = scanned
                    .index
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("session index lost entry {id}"))?;
                Ok(SelectedEntry {
                    offset: meta.offset,
                    len: meta.len,
                    edit: edit_for(id, &edit_index),
                })
            })
            .collect::<crate::Result<Vec<_>>>()?;

        let summary = summarize_selected(path, scanned.header_version, &entries)?;
        Ok(Self {
            path: path.to_path_buf(),
            header_version: scanned.header_version,
            entries,
            summary,
        })
    }

    /// The number of selected entries (the message count is in
    /// [`MessageSource::summary`]).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

impl MessageSource for SessionMessageSource {
    fn messages(&self) -> Box<dyn Iterator<Item = crate::Result<SourceMessage<'_>>> + '_> {
        Box::new(SessionMessageIter::new(self))
    }

    fn approximate_json_bytes(&self) -> usize {
        self.summary.approximate_json_bytes
    }

    fn summary(&self) -> crate::Result<MessageSourceSummary> {
        Ok(self.summary.clone())
    }
}

struct SessionMessageIter<'a> {
    source: &'a SessionMessageSource,
    file: Option<File>,
    next_entry: usize,
    ready: VecDeque<Message>,
    failed: bool,
}

impl<'a> SessionMessageIter<'a> {
    fn new(source: &'a SessionMessageSource) -> Self {
        Self {
            source,
            file: None,
            next_entry: 0,
            ready: VecDeque::new(),
            failed: false,
        }
    }

    /// Load the next selected entry into `ready`. Returns `Ok(false)` when the
    /// selected list is exhausted.
    fn load_next(&mut self) -> crate::Result<bool> {
        self.ready.clear();
        if self.next_entry >= self.source.entries.len() {
            return Ok(false);
        }
        if self.file.is_none() {
            self.file = Some(File::open(&self.source.path).map_err(|error| {
                anyhow::anyhow!(
                    "cannot read session {}: {error}",
                    self.source.path.display()
                )
            })?);
        }
        let selected = &self.source.entries[self.next_entry];
        self.next_entry += 1;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("session file handle disappeared"))?;
        let entry = read_entry_at(
            file,
            selected.offset,
            selected.len,
            self.source.header_version,
            &self.source.path,
        )?;
        let mut messages = Vec::new();
        push_entry_context_messages(entry, &selected.edit, &mut messages);
        self.ready.extend(messages);
        Ok(true)
    }
}

impl<'a> Iterator for SessionMessageIter<'a> {
    type Item = crate::Result<SourceMessage<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(message) = self.ready.pop_front() {
                return Some(Ok(SourceMessage::Owned(message)));
            }
            if self.failed || self.next_entry >= self.source.entries.len() {
                return None;
            }
            match self.load_next() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Index pass
// ---------------------------------------------------------------------------

struct EntryMeta {
    offset: u64,
    len: usize,
    parent_id: Option<String>,
    /// The id of the nearest preceding parsed NON-header entry in file order —
    /// the row a skipped (unparsable) line was appended after. The path walk
    /// uses it to join the prefix across a hole (PIR-BIN-092), exactly like
    /// `projection::bridging_predecessor` does for the resident list.
    predecessor_id: Option<String>,
    kind: EntryKind,
    /// Newest compaction's `firstKeptEntryId` (compaction entries only).
    first_kept_entry_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Message,
    CustomMessage,
    BranchSummary,
    Compaction,
    ContextEdit,
    State,
    /// A stored message whose role is `system` (the projection's compaction
    /// retention skips these inside the kept range: the checkpoint already
    /// carries the system prompt replay).
    SystemMessage,
}

struct ScannedSession {
    index: std::collections::HashMap<String, EntryMeta>,
    header_version: u32,
    edit_entries: std::collections::HashMap<String, ContextEditEntry>,
    /// The id the projection's stale-leaf fallback would pick: the last
    /// non-header entry in FILE order.
    last_non_header_id: Option<String>,
}

fn scan_index(path: &Path) -> crate::Result<ScannedSession> {
    let file = File::open(path)
        .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut bytes: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    let mut header_version = 1u32;
    let mut seen_header = false;
    let mut index: std::collections::HashMap<String, EntryMeta> = std::collections::HashMap::new();
    let mut edit_entries: std::collections::HashMap<String, ContextEditEntry> =
        std::collections::HashMap::new();
    let mut last_non_header_id: Option<String> = None;
    loop {
        bytes.clear();
        let line_offset = offset;
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        offset += read as u64;
        let mut len = bytes.len();
        if bytes.last() == Some(&b'\n') {
            len -= 1;
        }
        if len == 0 {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes[..len]);
        if !seen_header {
            let Ok(entry) = serde_json::from_str::<FileEntry>(&text) else {
                continue;
            };
            let Some(header) = entry.as_header() else {
                continue;
            };
            header_version = header.version.unwrap_or(1);
            seen_header = true;
            continue;
        }
        // The full parse supplies the exact fields (and the v2→v3 repairs); a
        // line the loader would skip is skipped here too, so path resolution
        // sees the same tree.
        let Some(entry) = parse_entry_line_with_version(&text, Some(header_version)) else {
            continue;
        };
        let Some(id) = entry.id() else {
            continue;
        };
        let id = id.to_owned();
        let kind = match &entry {
            FileEntry::Message(inner) => {
                if inner.message.role() == "system" {
                    EntryKind::SystemMessage
                } else {
                    EntryKind::Message
                }
            }
            FileEntry::CustomMessage(_) => EntryKind::CustomMessage,
            FileEntry::BranchSummary(_) => EntryKind::BranchSummary,
            FileEntry::Compaction(_) => EntryKind::Compaction,
            FileEntry::ContextEdit(_) => EntryKind::ContextEdit,
            _ => EntryKind::State,
        };
        if let FileEntry::ContextEdit(edit) = &entry {
            edit_entries.insert(id.clone(), edit.clone());
        }
        index.insert(
            id.clone(),
            EntryMeta {
                offset: line_offset,
                len,
                parent_id: entry.parent_id().map(str::to_owned),
                predecessor_id: last_non_header_id.clone(),
                kind,
                first_kept_entry_id: match &entry {
                    FileEntry::Compaction(compaction) => {
                        Some(compaction.first_kept_entry_id.clone())
                    }
                    _ => None,
                },
            },
        );
        last_non_header_id = Some(id);
    }
    Ok(ScannedSession {
        index,
        header_version,
        edit_entries,
        last_non_header_id,
    })
}

// ---------------------------------------------------------------------------
// Path + retention
// ---------------------------------------------------------------------------

/// Leaf→root walk over the scanned index, mirroring
/// `projection::build_session_path_entries` (same stale-leaf fallback, same
/// cycle guard, and the same broken-chain bridge — PIR-BIN-092): a `parentId`
/// that is not in the index joins the prefix through the skipped line's
/// predecessor instead of truncating the branch. `parentId: null` (a root)
/// still ends the walk.
fn resolve_path(scanned: &ScannedSession, leaf_id: Option<&str>) -> Vec<String> {
    let mut current = leaf_id
        .filter(|id| scanned.index.contains_key(*id))
        .map(str::to_owned)
        .or_else(|| scanned.last_non_header_id.clone());
    let mut path = Vec::new();
    let mut visited = std::collections::HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id.clone()) {
            break;
        }
        let Some(meta) = scanned.index.get(&id) else {
            break;
        };
        path.push(id);
        current = match &meta.parent_id {
            // A root: the chain ends here (`parentId: null`).
            None => None,
            Some(parent) if scanned.index.contains_key(parent) => Some(parent.clone()),
            // The parent id belongs to a line the loader skipped: continue at
            // the nearest preceding surviving row, exactly like the resident
            // walk. (Upstream pi stops here and silently drops the prefix.)
            Some(_) => meta.predecessor_id.clone(),
        };
    }
    path.reverse();
    path
}

/// The projection's `context_entry_refs` over entry ids: `[newest compaction]`
/// plus the kept range from `firstKeptEntryId` up to it (system messages
/// inside the kept range skipped) plus everything after it`.
fn retained_entry_ids(scanned: &ScannedSession, path: &[String]) -> Vec<String> {
    let compaction_index = path
        .iter()
        .enumerate()
        .filter(|(_, id)| {
            scanned
                .index
                .get(*id)
                .is_some_and(|meta| meta.kind == EntryKind::Compaction)
        })
        .map(|(index, _)| index)
        .next_back();
    let Some(compaction_index) = compaction_index else {
        return path.to_vec();
    };
    let first_kept = scanned.index[&path[compaction_index]]
        .first_kept_entry_id
        .clone()
        .unwrap_or_default();
    let mut retained = Vec::with_capacity(path.len());
    retained.push(path[compaction_index].clone());
    let mut found_first_kept = false;
    for id in &path[..compaction_index] {
        if *id == first_kept {
            found_first_kept = true;
        }
        if found_first_kept
            && scanned
                .index
                .get(id)
                .is_some_and(|meta| meta.kind != EntryKind::SystemMessage)
        {
            retained.push(id.clone());
        }
    }
    retained.extend(path[compaction_index + 1..].iter().cloned());
    retained
}

fn retained_context_edits(
    scanned: &ScannedSession,
    retained: &[String],
) -> std::collections::HashMap<String, ContextEditEntry> {
    let mut edits = std::collections::HashMap::new();
    for id in retained {
        if let Some(edit) = scanned.edit_entries.get(id) {
            // Later edits win (the projection's `context_edit_index` insert
            // order over the retained entries).
            edits.insert(edit.target_id.clone(), edit.clone());
        }
    }
    edits
}

fn edit_for(id: &str, edits: &std::collections::HashMap<String, ContextEditEntry>) -> Edit {
    match edits.get(id) {
        None => Edit::Keep,
        Some(edit) => match &edit.replacement {
            None => Edit::Omit,
            Some(replacement) => Edit::Replace(replacement.content.clone()),
        },
    }
}

// ---------------------------------------------------------------------------
// Summary + iteration
// ---------------------------------------------------------------------------

fn summarize_selected(
    path: &Path,
    header_version: u32,
    entries: &[SelectedEntry],
) -> crate::Result<MessageSourceSummary> {
    let mut file = File::open(path)
        .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
    let mut summary = MessageSourceSummary::default();
    let mut messages: Vec<Message> = Vec::new();
    for selected in entries {
        messages.clear();
        let entry = read_entry_at(
            &mut file,
            selected.offset,
            selected.len,
            header_version,
            path,
        )?;
        push_entry_context_messages(entry, &selected.edit, &mut messages);
        for message in &messages {
            summary.message_count += 1;
            summary.approximate_json_bytes = summary
                .approximate_json_bytes
                .saturating_add(approximate_message_json_bytes(message));
            if matches!(message, Message::System(_) | Message::Developer(_)) {
                summary.instruction_messages.push(message.clone());
            }
            summary.has_tool_history |= message_has_tool_history(message);
        }
    }
    Ok(summary)
}

fn read_entry_at(
    file: &mut File,
    offset: u64,
    len: usize,
    header_version: u32,
    path: &Path,
) -> crate::Result<FileEntry> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)
        .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    parse_entry_line_with_version(&text, Some(header_version)).ok_or_else(|| {
        anyhow::anyhow!(
            "session {} changed while streaming entry at offset {offset}",
            path.display()
        )
    })
}

/// Convert one entry exactly like the projection's
/// `push_context_entry_messages`: a `replacement: null` edit omits the entry,
/// a replacement rewrites each editable message's content.
fn push_entry_context_messages(entry: FileEntry, edit: &Edit, out: &mut Vec<Message>) {
    match edit {
        Edit::Omit => {}
        Edit::Keep => projection_push_messages(&entry, out),
        Edit::Replace(content) => {
            let mut messages = Vec::new();
            projection_push_messages(&entry, &mut messages);
            out.extend(
                messages
                    .into_iter()
                    .map(|message| replace_message_content(message, content)),
            );
        }
    }
}

/// The projection's `replace_message_content` (only user/assistant/toolResult
/// content is editable).
fn replace_message_content(message: Message, content: &serde_json::Value) -> Message {
    let blocks = || crate::session_manager::projection::content_blocks(content);
    match message {
        Message::User(mut user) => {
            user.content = blocks();
            Message::User(user)
        }
        Message::Assistant(mut assistant) => {
            assistant.content = blocks();
            Message::Assistant(assistant)
        }
        Message::ToolResult(mut result) => {
            result.content = blocks();
            Message::ToolResult(result)
        }
        other => other,
    }
}
