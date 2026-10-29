//! Typed entry kinds and their guards.
//!
//! Upstream `src/entries.ts` builds a token object per kind whose `is()` method
//! narrows an `EntryRecord` by its `kind` discriminator. Rust has no structural
//! narrowing, so the same information is expressed as [`EntryKind`] values plus
//! the [`is_entry_kind`] guard: the guard returns whether a record carries the
//! kind, and callers read `data` themselves.
//!
//! Each upstream token keeps its name as a unit struct with a `KIND` constant
//! and an `is` guard, so `UserEntry::is(record)` reads like upstream's
//! `UserEntry.is(record)`. The kind strings are persisted verbatim and are part
//! of the public protocol.

use crate::durable::errors::StorageRequestError;
use crate::durable::types::EntryRecord;

/// A typed entry kind whose records are identified by [`EntryKind::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryKind {
    /// The persisted discriminator.
    pub kind: &'static str,
}

impl EntryKind {
    /// Define a typed entry kind. The kind must be a non-empty string.
    pub fn define(kind: &'static str) -> Result<Self, StorageRequestError> {
        if kind.is_empty() {
            return Err(StorageRequestError::new(
                "Entry kind must be a non-empty string",
            ));
        }
        Ok(Self { kind })
    }

    /// Whether `entry` carries this kind.
    pub fn is(self, entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, self)
    }
}

/// Whether `entry` is present and carries `kind`.
pub fn is_entry_kind(entry: Option<&EntryRecord>, kind: EntryKind) -> bool {
    entry.is_some_and(|entry| entry.kind == kind.kind)
}

/// Whether `entry` is present and carries the raw kind string.
pub fn is_kind(entry: Option<&EntryRecord>, kind: &str) -> bool {
    entry.is_some_and(|entry| entry.kind == kind)
}

/// User input: `model` is a one-element user message list. Written by
/// submissions.
pub struct UserEntry;

impl UserEntry {
    /// The persisted discriminator.
    pub const KIND: EntryKind = EntryKind { kind: "pi.user" };

    /// Whether `entry` is a user entry.
    pub fn is(entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, Self::KIND)
    }
}

/// Provider result with any stop reason: `model` is a one-element assistant
/// message list.
pub struct AssistantEntry;

impl AssistantEntry {
    /// The persisted discriminator.
    pub const KIND: EntryKind = EntryKind {
        kind: "pi.assistant",
    };

    /// Whether `entry` is an assistant entry.
    pub fn is(entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, Self::KIND)
    }
}

/// Positional prompt and tool change: `model` is a one-element system message
/// list with empty content.
pub struct SystemEntry;

impl SystemEntry {
    /// The persisted discriminator.
    pub const KIND: EntryKind = EntryKind { kind: "pi.system" };

    /// Whether `entry` is a system entry.
    pub fn is(entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, Self::KIND)
    }
}

/// Tool result: the model content ends with the rendered diagnostics block and
/// `data` holds the structured diagnostics, possibly none.
pub struct ToolResultEntry;

impl ToolResultEntry {
    /// The persisted discriminator.
    pub const KIND: EntryKind = EntryKind {
        kind: "pi.tool-result",
    };

    /// Whether `entry` is a tool-result entry.
    pub fn is(entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, Self::KIND)
    }
}

/// Start of a new context: always `head: "self"`, with `model` absent for a
/// plain reset or a one-element user message list carrying the handoff text.
pub struct ResetEntry;

impl ResetEntry {
    /// The persisted discriminator.
    pub const KIND: EntryKind = EntryKind { kind: "pi.reset" };

    /// Whether `entry` is a reset entry.
    pub fn is(entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, Self::KIND)
    }
}

/// Compaction summary: `model` is a one-element user message list with the
/// wrapped summary, `head` is the first kept entry.
pub struct CompactionEntry;

impl CompactionEntry {
    /// The persisted discriminator.
    pub const KIND: EntryKind = EntryKind {
        kind: "pi.compaction",
    };

    /// Whether `entry` is a compaction entry.
    pub fn is(entry: Option<&EntryRecord>) -> bool {
        is_entry_kind(entry, Self::KIND)
    }
}

/// The user entry kind as a value.
pub const USER_ENTRY: EntryKind = UserEntry::KIND;
/// The assistant entry kind as a value.
pub const ASSISTANT_ENTRY: EntryKind = AssistantEntry::KIND;
/// The system entry kind as a value.
pub const SYSTEM_ENTRY: EntryKind = SystemEntry::KIND;
/// The tool-result entry kind as a value.
pub const TOOL_RESULT_ENTRY: EntryKind = ToolResultEntry::KIND;
/// The reset entry kind as a value.
pub const RESET_ENTRY: EntryKind = ResetEntry::KIND;
/// The compaction entry kind as a value.
pub const COMPACTION_ENTRY: EntryKind = CompactionEntry::KIND;

/// `data` shape of [`TOOL_RESULT_ENTRY`].
///
/// Upstream types the diagnostics as the harness' `ToolDiagnostic`; the harness
/// is out of this port's scope, so the entries are kept as opaque JSON values.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResultData {
    /// Structured diagnostics, possibly empty.
    #[serde(default)]
    pub diagnostics: Vec<serde_json::Value>,
}

/// `data` shape of [`COMPACTION_ENTRY`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompactionData {
    /// Why compaction ran: `manual`, `threshold` or `overflow`.
    ///
    /// Kept as a string rather than an enum so a record written by a newer
    /// producer still deserializes (the log is read leniently).
    pub reason: String,
}

/// A compaction triggered by an explicit request.
pub const COMPACTION_REASON_MANUAL: &str = "manual";
/// A compaction triggered by the context threshold.
pub const COMPACTION_REASON_THRESHOLD: &str = "threshold";
/// A compaction triggered by provider context overflow.
pub const COMPACTION_REASON_OVERFLOW: &str = "overflow";
