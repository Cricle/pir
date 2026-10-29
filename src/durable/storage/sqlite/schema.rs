//! SQLite identifiers for the durable schema.
//!
//! Ports upstream `storage/sqlite/schema.ts` minus the `prefix` option: this
//! backend owns its database file, so namespaced storage (several durable
//! schemas in one database) is deliberately out of scope. Every name is a
//! constant here so the migrations and the runtime queries cannot drift.

/// Table names, matching upstream `TABLE_NAMES` exactly.
pub mod tables {
    /// Migration bookkeeping (one row: schema version).
    pub const DURABLE_SCHEMA: &str = "durable_schema";
    /// ID and sequence watermarks (one row).
    pub const DURABLE_METADATA: &str = "durable_metadata";
    /// Global ID ownership across every record family.
    pub const RECORD_IDS: &str = "record_ids";
    /// Conversation records.
    pub const CONVERSATIONS: &str = "conversations";
    /// Transcript entries.
    pub const ENTRIES: &str = "entries";
    /// Durable task records.
    pub const TASKS: &str = "tasks";
    /// Submission records.
    pub const SUBMISSIONS: &str = "submissions";
    /// Document incarnation records.
    pub const DOCUMENTS: &str = "documents";
    /// Document base/delta revisions.
    pub const DOCUMENT_REVISIONS: &str = "document_revisions";
}

/// Index names, matching upstream `INDEX_NAMES` exactly.
pub mod indexes {
    /// Conversations by owning conversation.
    pub const CONVERSATIONS_BY_OWNER_CONVERSATION: &str = "conversations_by_owner_conversation";
    /// Conversations by owning task.
    pub const CONVERSATIONS_BY_OWNER_TASK: &str = "conversations_by_owner_task";
    /// Entries by conversation.
    pub const ENTRIES_BY_CONVERSATION: &str = "entries_by_conversation";
    /// Head marker entries by conversation.
    pub const ENTRY_HEADS_BY_CONVERSATION: &str = "entry_heads_by_conversation";
    /// Tasks by status.
    pub const TASKS_BY_STATUS: &str = "tasks_by_status";
    /// Tasks by conversation.
    pub const TASKS_BY_CONVERSATION: &str = "tasks_by_conversation";
    /// Tasks by definition kind.
    pub const TASKS_BY_KIND: &str = "tasks_by_kind";
    /// Tasks by abort mark.
    pub const TASKS_BY_ABORT_REQUESTED: &str = "tasks_by_abort_requested";
    /// Tasks by background flag.
    pub const TASKS_BY_BACKGROUND: &str = "tasks_by_background";
    /// Submissions by conversation-scoped request key.
    pub const SUBMISSIONS_BY_REQUEST: &str = "submissions_by_request";
    /// Submissions by conversation.
    pub const SUBMISSIONS_BY_CONVERSATION: &str = "submissions_by_conversation";
    /// Submissions by status.
    pub const SUBMISSIONS_BY_STATUS: &str = "submissions_by_status";
    /// Documents by logical address.
    pub const DOCUMENTS_BY_ADDRESS: &str = "documents_by_address";
    /// Documents by scope.
    pub const DOCUMENTS_BY_SCOPE: &str = "documents_by_scope";
    /// Documents by scope and kind.
    pub const DOCUMENTS_BY_SCOPE_KIND: &str = "documents_by_scope_kind";
    /// Document revisions by kind.
    pub const DOCUMENT_REVISIONS_BY_KIND: &str = "document_revisions_by_kind";
}
