//! Full model context for a LINEAR paged session, without widening it.
//!
//! A paged manager (`SessionManager::open_paged`) keeps only a byte-bounded
//! tail window resident; the skipped prefix lives on disk. Print mode, whose
//! compaction-disabled path must send the COMPLETE transcript, used to call
//! `widen_to_full` and then `build_session_projection`, which materializes the
//! transcript twice: once as the manager's `Vec<Arc<FileEntry>>` and once as
//! the projection's `Vec<Message>`.
//!
//! This module builds the projection directly from the file in two bounded
//! passes:
//!
//! 1. a structural scan that classifies each line and proves the session is a
//!    single linear chain with no entry kind that contributes/edits context
//!    beyond a plain message (compaction, branch summary, custom message,
//!    context edit, unknown entry → `None`, the caller falls back to widening);
//! 2. a materialization pass that parses one retained message line at a time
//!    and MOVES its `StoredMessage` payload into the aggregate `Vec<Message>`.
//!
//! Peak memory is therefore the conversation itself (one copy) plus one
//! parsed line and two small per-line tables — never the entry list and the
//! message aggregate at the same time. Semantics are identical to
//! `build_session_projection` on a linear chain: file order is path order and
//! every message entry contributes exactly its stored message (the `!!`
//! bash-execution exclusion matches `visit_entry_context_messages`).

use std::io::BufRead;

use serde::Deserialize;

use super::types::{FileEntry, StoredMessage};
use super::{SessionManager, SESSION_READ_BUFFER_SIZE};
use crate::ai::types::Message;

/// The model-visible transcript produced by
/// [`SessionManager::build_paged_prompt_context`].
#[derive(Debug)]
pub struct PagedPromptContext {
    pub messages: Vec<Message>,
}

impl SessionManager {
    /// Build the full model context of a linear paged session without
    /// materializing the skipped prefix as session entries.
    ///
    /// Returns `Ok(None)` when the manager is not paged, or when the session
    /// is not a plain linear chain (a branch, a compaction checkpoint, a
    /// branch summary, a custom message, a context edit, or an unparseable
    /// line). Callers that need the complete semantics in those cases must
    /// widen first; this method never guesses.
    ///
    /// The manager is NOT mutated: it stays paged, so appends keep working
    /// against its resident tail and a later compaction can still widen
    /// explicitly.
    ///
    /// The `session` feature OFF has no JSONL reader and therefore no paged
    /// sessions; the method returns `Ok(None)` and the caller widens (a no-op
    /// there).
    pub fn build_paged_prompt_context(&self) -> crate::Result<Option<PagedPromptContext>> {
        if !self.is_paged() {
            return Ok(None);
        }
        let Some(path) = self.session_file.as_deref() else {
            return Ok(None);
        };
        let Some(scan) = scan_linear_session(path)? else {
            return Ok(None);
        };
        // The manager's leaf must be the final entry the scan walked: a stale
        // leaf id (the file gained an entry after the paged open) would make
        // the projection diverge from the manager's own view.
        if self.leaf_id() != scan.last_id.as_deref() {
            return Ok(None);
        }

        let mut messages: Vec<Message> = Vec::with_capacity(scan.message_count);
        let file = std::fs::File::open(path)
            .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
        let mut reader = std::io::BufReader::with_capacity(SESSION_READ_BUFFER_SIZE, file);
        let mut bytes: Vec<u8> = Vec::new();
        let mut line_index = 0usize;
        loop {
            bytes.clear();
            let read = reader.read_until(b'\n', &mut bytes).map_err(|error| {
                anyhow::anyhow!("cannot read session {}: {error}", path.display())
            })?;
            if read == 0 {
                break;
            }
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            let is_message = scan.message_lines.get(line_index).copied().unwrap_or(false);
            line_index += 1;
            if !is_message {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let Some(FileEntry::Message(entry)) =
                super::parse_entry_line_with_version(&text, Some(3))
            else {
                // The structural scan and the full parse disagree (a line
                // changed under us, or a shape the light scan accepted does
                // not deserialize). Fall back to the exact widening path
                // instead of assembling a partial transcript.
                return Ok(None);
            };
            match entry.message {
                // `!!` bash records stay in the transcript but not in the
                // model context (upstream `convertToLlm`; the projection's
                // `visit_entry_context_messages` applies the same rule).
                StoredMessage::BashExecution(bash) if bash.excluded() => {}
                message => messages.push(message.into()),
            }
        }
        if line_index != scan.message_lines.len() {
            // The file gained or lost lines between the structural pass and the
            // materialization pass. Do not return a transcript assembled from
            // two different file versions.
            return Ok(None);
        }
        Ok(Some(PagedPromptContext { messages }))
    }
}

/// The structural result of the first pass.
struct LinearScan {
    /// One flag per file line (including blank lines, excluding the final
    /// newline): true when the line is a context-contributing message entry.
    message_lines: Vec<bool>,
    message_count: usize,
    last_id: Option<String>,
}

/// The fields the structural pass needs from one line. Unknown message
/// payloads are skipped by serde without allocating their text.
#[derive(Deserialize)]
struct LineMeta {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
}

/// State-only entries that carry a chain link but contribute no context.
fn is_state_entry(kind: &str) -> bool {
    matches!(
        kind,
        "thinking_level_change" | "model_change" | "usage" | "label" | "session_info" | "custom"
    )
}

/// First pass: prove the file is one linear chain of plain message and
/// state-only entries. `Ok(None)` for every shape the streaming builder does
/// not reproduce exactly.
fn scan_linear_session(path: &std::path::Path) -> crate::Result<Option<LinearScan>> {
    let file = std::fs::File::open(path)
        .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(SESSION_READ_BUFFER_SIZE, file);
    let mut bytes: Vec<u8> = Vec::new();
    let mut message_lines: Vec<bool> = Vec::new();
    let mut message_count = 0usize;
    let mut seen_header = false;
    let mut expected_parent: Option<String> = None;
    let mut last_id: Option<String> = None;
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        let text = String::from_utf8_lossy(&bytes);
        if text.trim().is_empty() {
            message_lines.push(false);
            continue;
        }
        let Ok(meta) = serde_json::from_str::<LineMeta>(&text) else {
            // `load_entries_from_file` skips unparseable lines, but the next
            // entry's parent chain still names this line's id. That cannot be
            // reconstructed from a light scan, so widening decides.
            return Ok(None);
        };
        let Some(kind) = meta.kind.as_deref() else {
            return Ok(None);
        };
        if !seen_header {
            if kind != "session" {
                return Ok(None);
            }
            seen_header = true;
            message_lines.push(false);
            continue;
        }
        let Some(id) = meta.id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        // Every entry must parent the previous chain link: a branch, a fork,
        // or a skipped broken line breaks this and takes the widening path.
        if meta.parent_id.as_deref() != expected_parent.as_deref() {
            return Ok(None);
        }
        expected_parent = Some(id.to_owned());
        last_id = Some(id.to_owned());
        match kind {
            "message" => {
                message_lines.push(true);
                message_count += 1;
            }
            kind if is_state_entry(kind) => message_lines.push(false),
            // compaction / branch_summary / custom_message / context_edit /
            // unknown: context semantics this builder does not reproduce.
            _ => return Ok(None),
        }
    }
    if !seen_header {
        return Ok(None);
    }
    Ok(Some(LinearScan {
        message_lines,
        message_count,
        last_id,
    }))
}
