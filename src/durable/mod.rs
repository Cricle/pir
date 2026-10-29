//! Durable conversation, task and document runtime — the native port of
//! upstream `@earendil-works/pi-durable`.
//!
//! # What this module is
//!
//! One rule: *nothing is visible before its commit is stored*. Conversations,
//! transcript entries, tasks, submissions and mutable JSON documents are
//! committed as one atomic batch; a task state machine resumes at its last
//! committed phase after a crash; documents are observed only in their
//! committed revisions.
//!
//! Since 2026-10-20 this is **core**, not a cargo feature. It is a *second
//! harness beside* `agent_loop`/`agent_session`, not the engine under it: pir
//! mirrors upstream's `pi-durable` / `pi-coding-agent` split, where neither
//! package depends on the other (see `UPDATING.md` A2a and
//! `docs/durable-spec.md`).
//!
//! | module | upstream | contents |
//! |---|---|---|
//! | [`ids`] | `src/ids.ts`, `src/types.ts` | branded IDs, [`Seq`] |
//! | [`errors`] | `src/errors.ts`, `storage/jsonl/storage.ts` | typed storage errors |
//! | [`entries`] | `src/entries.ts` | entry kinds and guards |
//! | [`types`] | `src/types.ts` (subset) | records, writes, queries, [`Storage`] |
//! | [`delta`] | `@earendil-works/chord/delta` (subset) | document operation batches |
//! | [`documents`] | `src/documents.ts` | definitions, addresses, typed checks |
//! | [`tasks`] | `src/tasks.ts` | task definition metadata |
//! | [`timers`] | `src/timers.ts` | host timer wrapper |
//! | [`truncate`] | `src/truncate.ts` | head truncation |
//! | [`search`] | `src/search.ts` | BM25 ranking |
//! | [`storage::scan`] | `src/storage/scan.ts` | cursor/order resolution |
//! | [`storage::memory`] | `src/storage/memory.ts` | in-memory backend |
//! | [`storage::jsonl`] | `src/storage/jsonl/storage.ts` | append-only JSONL backend |
//! | [`storage::sqlite`] | `src/storage/sqlite/{storage,schema,migrations}.ts` | WAL-backed SQLite backend |
//! | [`context`] | `@earendil-works/chord/context` | abort signal + span carrier |
//! | [`chord`] | `@earendil-works/chord/delta` | revision diffing, trusted apply |
//! | [`session`] | `src/session/**` | mutation line, `Tx`, assembly, publication, watches |
//! | [`harness`] | `src/harness/**` | settings, registry, agent, scheduler, built-in tasks |
//!
//! # Port status (2026-10-20)
//!
//! | area | state |
//! |---|---|
//! | entries + storage core (`ids` … `storage::jsonl`) | ported, conformance-tested |
//! | `context`, `chord` (diff/trusted apply) | ported (the subset the kernel needs) |
//! | `session/**` — mutation line, `Tx`, assembly, publication, observation, forks | ported |
//! | `harness/**` — settings, registry, agent, inbox, submissions, context, prompt, live, usage, scheduler, tasks | ported |
//! | `harness/generation`, `tool`, `compaction` | the built-in tasks, bridged to `crate::ai` / `crate::coding_agent` |
//! | `pi_format/**` | the pi version-3 JSONL import/export bridge |
//! | `storage/sqlite/**` | ported: WAL, schema v1/v2, atomic commits, scans/watermarks; conformance-tested |
//!
//! # Deliberately not ported
//!
//! * `codemode/**`, `tool-search/**`, `tools/**`, `images/**`, `env/**`,
//!   `testing/**` — pir already owns native equivalents (`plugins::codemode`,
//!   `plugins::tool_search`, `coding_agent::tools`, the `image-tools` feature,
//!   `env_client`/`env_daemon`, and the suites under `tests/`). Carrying the
//!   upstream copies would create a second implementation of each.
//! * chord's host/object/link/hot-reload/websocket layers — only the delta
//!   subset is needed by a single-process harness. See `docs/durable-spec.md`.
//! * `storage/sqlite/{database,node,cloudflare}.ts` and the namespaced `prefix`
//!   option — `rusqlite` is the only runtime here and this backend owns its
//!   database file.
//!
//! Normative reference: `docs/durable-spec.md` (a pir-side digest of upstream
//! `packages/durable/docs/spec.md`, with the Rust deviations called out).
//!
//! # Semantics worth knowing
//!
//! * **Atomic batches.** `commit` returns only after the main marker line is
//!   written (and flushed when `fsync` is on); later reads observe the batch.
//!   A crash before the marker leaves orphan sidecar lines that recovery
//!   truncates.
//! * **Poisoning.** An IO failure during `commit` poisons the instance; every
//!   later operation fails until it is reopened. Reads never poison.
//! * **Lenient reads, exact writes.** Records serialize with `camelCase` keys
//!   and omit absent optional fields. Unknown fields are ignored when reading,
//!   so a log written by a newer producer still opens.
//! * **No `~/.pi`.** The caller supplies the storage root.
//! * **Zero new dependencies**: serde, serde_json, thiserror and tokio were
//!   already in the dependency graph.

pub mod chord;
pub mod context;
pub mod delta;
pub mod documents;
pub mod entries;
pub mod errors;
pub mod harness;
pub mod ids;
pub mod search;
pub mod session;
pub mod storage;
pub mod tasks;
pub mod timers;
pub mod truncate;
pub mod types;

pub use chord::{apply_immutable_trusted, diff_revisions, overlap};
pub use context::Context;
pub use delta::{apply, apply_immutable_batches, DeltaError, Op, Path, Seg};
pub use documents::{
    address_id, check_record_scope, check_record_version, check_stored_record_scope, define_doc,
    define_doc_family, document_create, materialize_document, materialize_document_value,
    AnyDocDefinition, AnyDocFamilyDefinition, CommonDocDefinition, DocCheckpointWhen,
    DocDefinition, DocFamilyDefinition, DocFamilyToken, DocInitial, DocMigrate, DocOwner, DocToken,
    DocTokenKind, DocumentSemantics, ResolvedAddress,
};
pub use entries::{
    is_entry_kind, is_kind, AssistantEntry, CompactionData, CompactionEntry, EntryKind, ResetEntry,
    SystemEntry, ToolResultData, ToolResultEntry, UserEntry, ASSISTANT_ENTRY, COMPACTION_ENTRY,
    RESET_ENTRY, SYSTEM_ENTRY, TOOL_RESULT_ENTRY, USER_ENTRY,
};
pub use errors::{
    error_message, ConversationBusy, DocumentError, HostCallback, JsonlCorruptionError,
    JsonlStoragePoisonedError, ReadAfterWrite, ReportInfo, ReportTask, SessionFailed, StorageError,
    StorageRequestError, TaskFaultReason, TaskOutdatedReason,
};
pub use harness::{
    create_registry, resolve_agent, resolve_settings, AfterFailedRun, Agent, AgentChange,
    AgentState, Change, CompactionPolicy, Extension, HarnessSettings, QueueMode, Registry,
    RegistrySnapshot, RestartLimit, Settings, ToolExecutionMode,
};
pub use ids::{
    id_from_number, seq_from_number, ConversationId, ConversationIdKind, DocumentId,
    DocumentIdKind, EntryId, EntryIdKind, Id, IdKind, Seq, SubmissionId, SubmissionIdKind, TaskId,
    TaskIdKind, ROOT_CONVERSATION_ID,
};
pub use search::{rank_documents, tokenize, SearchDocument};
pub use storage::jsonl::{JsonlStorage, JsonlStorageOptions, FORMAT_VERSION, MAIN_FILE};
pub use storage::memory::{MemoryStorage, PreparedCommit};
pub use storage::scan::{next_cursor, page_of, scan_start, ScanStart};
#[cfg(any(feature = "extensions", feature = "context-mode"))]
pub use storage::sqlite::{SqliteStorage, SqliteStorageOptions};
pub use tasks::{define_task, Task, TaskDefinition};
pub use timers::{
    delay_steps, native_timer, SetTimer, SetTimerFn, TimerCallback, TimerCancel, TimerHandle,
    TimerOptions, Timers, MAX_TIMER_DELAY_MS,
};
pub use truncate::{
    format_size, truncate_head, truncate_head_of, utf8_byte_length, Totals, TruncatedBy,
    TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};
pub use types::Storage;
pub use types::{
    decode_id, ContextEdit, ContextEditAction, ConversationOwner, ConversationParent,
    ConversationQuery, ConversationRecord, CurrentPoint, Cursor, DocumentAddress,
    DocumentCommitChange, DocumentContent, DocumentCopySource, DocumentCreate, DocumentFork,
    DocumentHistory, DocumentPoint, DocumentPointQuery, DocumentQuery, DocumentRecord,
    DocumentScope, Entry, EntryQuery, EntryRecord, EntryWithSeq, JoinPolicy, JsonObject, Page,
    ScanOrder, StorageWrite, StoredDocument, SubmissionQuery, SubmissionRecord, SubmissionStatus,
    SubmissionType, TaskOutcome, TaskOutcomeError, TaskQuery, TaskRecord, TaskState, TaskStatus,
    TypedEntry,
};

pub mod pi_format;
pub use pi_format::{
    import_session_text, read_session_file, render_session, write_session, ExportError,
    ExportInput, ImportError, ImportedSession, OpaqueEntry,
};
