//! The durable transaction: one commit callback's staging area.
//!
//! Port of upstream `session/transaction.ts`. A [`Transaction`] is created by
//! the mutation line for exactly one callback, handed to it as `&mut dyn Tx`,
//! and then either **settled** (success: prepare every draft, assemble the
//! atomic batch) or **failed** (abort every draft; nothing reaches storage).
//!
//! # The rules (spec §4, §3)
//!
//! * **Table reads before the first table write.** Any table read after a
//!   table write throws [`ReadAfterWrite`]. Document access and
//!   read-your-writes stay available either way.
//! * **Creation methods return the created record.** Callers never read back
//!   what they just created (a read-back would be a table read).
//! * **Drafts are fully revoked at settlement.** A draft used after its
//!   callback settled is an error, not a stale write.
//! * **Owners are validated on the final candidates.** New owned work needs a
//!   live owner: not `completing`, terminal, or abort-marked.
//! * **Terminal tasks retire every task document**, including ones this
//!   transaction created.
//! * **Checkpoint predicates run last**, after every validation, so selection
//!   performs no storage read.
//!
//! # Rows are `serde_json::Value`
//!
//! Upstream `Tx` is generic over the record types; the Rust port keeps typed
//! records (`EntryRecord`, `TaskRecord`, …) for the table writes and uses
//! `serde_json::Value` only where a task's input/checkpoint/outcome payload is
//! genuinely opaque. `Model`-facing message values stay `Value` on the wire, as
//! upstream does.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;

use crate::durable::context::Context;
use crate::durable::documents::{DocDefinition, DocFamilyDefinition, ResolvedAddress};
use crate::durable::errors::{ReadAfterWrite, StorageError};
use crate::durable::harness::telemetry::TraceCarrier;
use crate::durable::ids::{ConversationId, DocumentId, EntryId, SubmissionId, TaskId};
use crate::durable::types::{
    ConversationRecord, DocumentAddress, DocumentPoint, DocumentScope, JsonObject, Storage,
    StorageWrite, StoredDocument, SubmissionRecord, TaskRecord, TaskState,
};

/// Defaults a commit binds to.
///
/// Upstream `TransactionScope`: the conversation `create_task` defaults to, and
/// the task stamped as `byTaskId` on appended entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransactionScope {
    /// Default conversation for `create_task`.
    pub conversation_id: Option<ConversationId>,
    /// Task whose runtime commit this is.
    pub task_id: Option<TaskId>,
}

/// An entry before it has an id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntryDraft {
    /// Application-defined discriminator.
    pub kind: String,
    /// Messages contributed to model context.
    pub model: Option<Vec<Value>>,
    /// JSON payload.
    pub data: Option<Value>,
    /// `Some(HeadTarget::Self)` makes this entry the new head marker
    /// (upstream `head: "self"`); `Some(HeadTarget::Entry(id))` selects an
    /// existing entry; `None` inherits.
    pub head: Option<HeadTarget>,
    /// Context-only overrides of earlier visible entries.
    pub edits: Option<Vec<crate::durable::types::ContextEdit>>,
}

/// Where an appended entry's `head` points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadTarget {
    /// The entry being appended (`head: "self"`).
    SelfEntry,
    /// An existing entry.
    Entry(EntryId),
}

/// Options for creating a task inside a transaction.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskOptions {
    /// Conversation that owns a top-level task. Defaults to the transaction
    /// scope; required when neither is set.
    pub conversation_id: Option<ConversationId>,
    /// Owning task, for a child task.
    pub ownership: TaskOwnership,
    /// Excluded from idle waits and aborts.
    pub background: bool,
    /// Abort the task when a later harness starts scheduling after a restart.
    pub abandon_on_restart: bool,
    /// Submission ids whose trace carriers the task inherits (harness-level).
    ///
    /// The first submission's carrier becomes [`TaskRecord::trace`]; every
    /// later submission's carrier becomes one [`TaskRecord::links`] entry.
    /// Supplying ids therefore overrides `trace`, as upstream `createTask`
    /// does.
    pub submission_ids: Vec<SubmissionId>,
    /// Trace context to store when `submission_ids` is empty.
    pub trace: Option<TraceCarrier>,
}

/// Who owns a new task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskOwnership {
    /// The task's conversation owns it (a top-level task).
    #[default]
    Conversation,
    /// Another task of the same conversation owns it.
    Task(TaskId),
}

/// A submission settlement staged by a task.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionChange {
    /// A queued submission was placed at its entry.
    Placed {
        /// The entry it was placed as.
        entry: EntryId,
    },
    /// A placed input was answered.
    Done {
        /// The answering entry.
        answer: EntryId,
    },
    /// An input could not be answered.
    Unanswered {
        /// Why.
        reason: String,
        /// Structured detail.
        detail: Option<Value>,
    },
}

/// The mutation surface one commit callback uses.
///
/// Deliberately the **concrete** type, not a trait object: a commit callback
/// needs the staging helpers (`stage_document`, `retire_document`, …) that an
/// object-safe subset could not carry, and the line is the only place a
/// `Transaction` is created. Upstream's `Tx` interface exists because
/// TypeScript callbacks cannot see the concrete class; Rust gives the line and
/// the callback the same module boundary, so the concrete type is both simpler
/// and stricter (a callback physically cannot reach a `SessionHandle`).
///
/// Staged state for one transaction.
///
/// The visible struct is deliberately small: staging maps are private, and the
/// public surface is the handful of operations a commit callback needs.
pub struct Transaction {
    scope: TransactionScope,
    context: Context,
    table_write: bool,
    sealed: bool,
    writes: Vec<StorageWrite>,
    created_conversations: Vec<ConversationId>,
    fork_source_conversations: Vec<ConversationId>,
    fork_source_documents: Vec<DocumentId>,
    tasks: BTreeMap<i64, CandidateTask>,
    submissions: HashMap<i64, SubmissionRecord>,
    submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    documents: Vec<StagedDocument>,
    latest_document_by_address: HashMap<String, usize>,
    retirements: Vec<crate::durable::types::DocumentRecord>,
    plans: Vec<crate::durable::session::assemble::DocumentPlan>,
    /// The backend this transaction commits to, when the line attached it.
    ///
    /// Exists so a commit callback can read a committed record *inside* the
    /// commit (upstream `tx.latestHeadMarker`): the line is the only writer, so
    /// a direct backend read taken while the callback runs observes exactly the
    /// revision the commit is based on. It must never be used to open a commit.
    storage: Option<CommitStorage>,
}

/// The backend and failure latch a commit-time read uses.
///
/// The line holds the commit while the read runs, so close cannot race it
/// (there is nothing to track as in flight); the failure rule still applies,
/// which is why the latch travels with the backend.
#[derive(Clone)]
struct CommitStorage {
    backend: Arc<dyn Storage>,
    latch: Arc<dyn crate::durable::session::guard::FailureLatch>,
}

/// One task staged by this transaction.
#[derive(Debug, Default)]
struct CandidateTask {
    /// The record as it will be written.
    record: Option<TaskRecord>,
    /// Whether the record is a creation (no committed predecessor).
    created: bool,
    /// Whether the record was read from storage (memoized).
    read: Option<TaskRecord>,
}

/// One document acquisition, creation or retirement staged by this
/// transaction.
#[derive(Debug)]
pub struct StagedDocument {
    /// Logical address.
    pub address: DocumentAddress,
    /// Stable address identity.
    pub address_id: String,
    /// Definition (absent for a definition-free fork copy).
    pub definition: Option<DocDefinition>,
    /// Family definition (absent for a singleton).
    pub family: Option<DocFamilyDefinition>,
    /// The draft value the callback mutated.
    pub value: serde_json::Map<String, Value>,
    /// Whether this incarnation is newly created.
    pub created: bool,
    /// The loaded incarnation this draft forked, when it existed.
    pub loaded: Option<crate::durable::session::documents::LoadedDocument>,
    /// Whether the transaction asked to retire it.
    pub retire: bool,
}

impl StagedDocument {
    /// The definition's kind/version, whichever definition kind was used.
    #[must_use]
    pub fn kind_and_version(&self) -> (&str, i64) {
        match (&self.definition, &self.family) {
            (Some(definition), _) => (&definition.kind, definition.version),
            (None, Some(family)) => (&family.kind, family.version),
            (None, None) => ("", 1),
        }
    }
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("table_write", &self.table_write)
            .field("sealed", &self.sealed)
            .field("writes", &self.writes.len())
            .field("tasks", &self.tasks.len())
            .field("documents", &self.documents.len())
            .finish()
    }
}

impl Transaction {
    /// Begin a transaction for one callback.
    #[must_use]
    pub fn new(context: Context, scope: TransactionScope) -> Self {
        Self {
            scope,
            context,
            table_write: false,
            sealed: false,
            writes: Vec::new(),
            created_conversations: Vec::new(),
            fork_source_conversations: Vec::new(),
            fork_source_documents: Vec::new(),
            tasks: BTreeMap::new(),
            submissions: HashMap::new(),
            submission_changes: Vec::new(),
            documents: Vec::new(),
            latest_document_by_address: HashMap::new(),
            retirements: Vec::new(),
            plans: Vec::new(),
            storage: None,
        }
    }

    /// Attach the backend so commit-time reads (see [`Self::latest_head`]) can
    /// run against the revision this commit is based on.
    #[must_use]
    pub fn with_storage(
        mut self,
        backend: Arc<dyn Storage>,
        latch: Arc<dyn crate::durable::session::guard::FailureLatch>,
    ) -> Self {
        self.storage = Some(CommitStorage { backend, latch });
        self
    }

    /// Whether the line attached a backend, so commit-time reads
    /// ([`Self::latest_head`]) reflect committed state.
    ///
    /// `false` only for a bare transaction a unit test built directly; a
    /// staging call then has to read the backend itself.
    #[must_use]
    pub fn has_commit_storage(&self) -> bool {
        self.storage.is_some()
    }

    /// The newest visible head marker's `head`, read **inside** the commit
    /// (upstream `tx.latestHeadMarker`).
    ///
    /// Read-your-writes first: an entry this transaction already staged with a
    /// `head` marker wins over committed state. Otherwise the backend is read
    /// directly, while the line is held, so the value is the committed revision
    /// this commit is based on — never a stale pre-commit cut.
    ///
    /// Returns `Ok(None)` when the line did not attach a backend (unit tests
    /// constructing a bare transaction).
    pub async fn latest_head(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<EntryId>, StorageError> {
        for write in self.writes.iter().rev() {
            if let StorageWrite::Entry { value } = write {
                if value.conversation_id == conversation {
                    if let Some(head) = value.head {
                        return Ok(Some(head));
                    }
                }
            }
        }
        let Some(commit_storage) = &self.storage else {
            return Ok(None);
        };
        let record = commit_storage
            .backend
            .find_latest_head_marker(conversation, None)
            .await;
        match record {
            Ok(record) => Ok(record.and_then(|entry| entry.head)),
            Err(error) => {
                // The failure rule applies to a commit-time read exactly as it
                // does through the guard: an invalid request is exempt, every
                // other error ends the session.
                if let Some(failure) = commit_storage.latch.failure() {
                    return Err(crate::durable::errors::SessionFailed { cause: failure }.into());
                }
                if !matches!(error, StorageError::Request(_)) {
                    commit_storage.latch.fail(error.clone());
                }
                Err(error)
            }
        }
    }

    /// The committed value of one document incarnation at `address`, read
    /// **inside** the commit.
    ///
    /// This is the document analogue of [`Self::latest_head`]: the line holds
    /// the commit while the read runs, so it observes exactly the revision the
    /// commit is based on. Upstream reaches the same state through
    /// `tx.doc()`'s load path; the Rust facade's `doc` helper stages a
    /// read-write draft, so callers that only need a committed value (such as
    /// the conversation-created hook copying the owner's `pi.agent`) use this
    /// instead.
    ///
    /// Returns `Ok(None)` when the line did not attach a backend (unit tests
    /// constructing a bare transaction) or when no incarnation is live at the
    /// address.
    pub async fn committed_document(
        &self,
        address: DocumentAddress,
    ) -> Result<Option<JsonObject>, StorageError> {
        let Some(commit_storage) = &self.storage else {
            return Ok(None);
        };
        let backend = &commit_storage.backend;
        let found = match backend.find_document(address, DocumentPoint::Current).await {
            Ok(found) => found,
            Err(error) => return Err(self.commit_read_error(commit_storage, error)),
        };
        let Some(record) = found else {
            return Ok(None);
        };
        let stored: Option<StoredDocument> =
            match backend.document(record.id, DocumentPoint::Current).await {
                Ok(stored) => stored,
                Err(error) => return Err(self.commit_read_error(commit_storage, error)),
            };
        Ok(stored.map(|stored| stored.value))
    }

    /// Apply the failure rule to a commit-time read error.
    ///
    /// An invalid request has no durable effect and is exempt; every other
    /// error fails the session, exactly as a guarded read would.
    fn commit_read_error(
        &self,
        commit_storage: &CommitStorage,
        error: StorageError,
    ) -> StorageError {
        if let Some(failure) = commit_storage.latch.failure() {
            return crate::durable::errors::SessionFailed { cause: failure }.into();
        }
        if !matches!(error, StorageError::Request(_)) {
            commit_storage.latch.fail(error.clone());
        }
        error
    }

    /// The context the callback ran with (used for storage reads).
    #[must_use]
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The scope defaults.
    #[must_use]
    pub fn scope(&self) -> TransactionScope {
        self.scope
    }

    /// Whether the callback already made a table write.
    #[must_use]
    pub fn has_table_write(&self) -> bool {
        self.table_write
    }

    /// Whether the transaction has settled (successfully or not).
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Enforce "table reads before the first table write".
    pub fn check_table_read(&self, method: &str) -> Result<(), StorageError> {
        if self.table_write {
            return Err(StorageError::Invalid(
                ReadAfterWrite {
                    method: method.to_owned(),
                }
                .to_string(),
            ));
        }
        Ok(())
    }

    /// Stage a table write; records that the callback wrote.
    pub fn push_write(&mut self, write: StorageWrite) {
        self.table_write = true;
        self.writes.push(write);
    }

    /// Stage a conversation creation (also registers it for later checks).
    pub fn stage_conversation(
        &mut self,
        record: ConversationRecord,
        forked_from: Option<ConversationId>,
    ) {
        if let Some(parent) = forked_from {
            self.fork_source_conversations.push(parent);
        }
        self.created_conversations.push(record.id);
        self.push_write(StorageWrite::Conversation { value: record });
    }

    /// Register a fork source document that this transaction must not mutate.
    pub fn stage_fork_source_document(&mut self, id: DocumentId) {
        self.fork_source_documents.push(id);
    }

    /// Stage a fully prepared document plan.
    ///
    /// Assembly has no way to build a fork copy (a [`StagedDocument`] carries
    /// no source incarnation), so `#stageConversation` plans the copy itself
    /// and hands the plan over. Assembly appends staged plans after the ones it
    /// derived from drafts, and adoption publishes them like any other —
    /// without this, a fork's copies would be written to storage and never
    /// observed.
    pub fn stage_plan(&mut self, plan: crate::durable::session::assemble::DocumentPlan) {
        self.plans.push(plan);
    }

    /// The plans staged directly by a callback.
    #[must_use]
    pub fn staged_plans(&self) -> &[crate::durable::session::assemble::DocumentPlan] {
        &self.plans
    }

    /// Whether a conversation is being created by this transaction.
    #[must_use]
    pub fn creates_conversation(&self, id: ConversationId) -> bool {
        self.created_conversations.contains(&id)
    }

    /// The conversation ids this transaction created.
    #[must_use]
    pub fn created_conversations(&self) -> &[ConversationId] {
        &self.created_conversations
    }

    /// The fork-source conversation ids.
    #[must_use]
    pub fn fork_source_conversations(&self) -> &[ConversationId] {
        &self.fork_source_conversations
    }

    /// The fork-source document ids.
    #[must_use]
    pub fn fork_source_documents(&self) -> &[DocumentId] {
        &self.fork_source_documents
    }

    /// Stage a task record (creation or replacement).
    pub fn stage_task(&mut self, record: TaskRecord, created: bool) {
        let id = record.id.as_i64();
        let entry = self.tasks.entry(id).or_default();
        if entry
            .record
            .as_ref()
            .is_some_and(|existing| existing.state.is_terminal())
        {
            // Upstream rejects a terminal candidate being replaced. Silently
            // ignoring the write is the only non-panicking option here: a
            // terminal task stays terminal, so a stale caller cannot resurrect
            // it by staging a running/completed record after the fact.
            return;
        }
        entry.created = created || entry.created;
        entry.record = Some(record);
        self.table_write = true;
    }

    /// Memoize the committed record read for `id`.
    pub fn cache_task_read(&mut self, record: TaskRecord) {
        let id = record.id.as_i64();
        self.tasks.entry(id).or_default().read = Some(record);
    }

    /// The staged candidate for `id`, if this transaction wrote it.
    #[must_use]
    pub fn staged_task(&self, id: TaskId) -> Option<&TaskRecord> {
        self.tasks
            .get(&id.as_i64())
            .and_then(|entry| entry.record.as_ref())
    }

    /// Every task this transaction created or replaced.
    #[must_use]
    pub fn staged_tasks(&self) -> Vec<&TaskRecord> {
        self.tasks
            .values()
            .filter_map(|entry| entry.record.as_ref())
            .collect()
    }

    /// Whether the staged record is a creation.
    #[must_use]
    pub fn task_is_created(&self, id: TaskId) -> bool {
        self.tasks
            .get(&id.as_i64())
            .is_some_and(|entry| entry.created)
    }

    /// Stage a submission record.
    pub fn stage_submission(&mut self, record: SubmissionRecord) {
        self.table_write = true;
        self.submissions.insert(record.id.as_i64(), record);
    }

    /// Stage a submission settlement or placement.
    pub fn stage_submission_change(&mut self, id: SubmissionId, change: SubmissionChange) {
        self.table_write = true;
        self.submission_changes.push((id, change));
    }

    /// Stage a document draft.
    pub fn stage_document(&mut self, document: StagedDocument) -> usize {
        let index = self.documents.len();
        self.latest_document_by_address
            .insert(document.address_id.clone(), index);
        self.documents.push(document);
        index
    }

    /// The latest staged document at `address_id`.
    #[must_use]
    pub fn latest_document(&self, address_id: &str) -> Option<&StagedDocument> {
        self.latest_document_by_address
            .get(address_id)
            .and_then(|index| self.documents.get(*index))
    }

    /// The latest staged document at `address_id`, mutably.
    pub fn latest_document_mut(&mut self, address_id: &str) -> Option<&mut StagedDocument> {
        let index = *self.latest_document_by_address.get(address_id)?;
        self.documents.get_mut(index)
    }

    /// Every staged document.
    #[must_use]
    pub fn documents(&self) -> &[StagedDocument] {
        &self.documents
    }

    /// Stage a retirement of a committed incarnation that has no draft in this
    /// transaction (upstream `retire-only`, `transaction.ts:153`/`:683`).
    ///
    /// A retirement discovered without an acquisition carries only the record:
    /// there is no draft to mutate and no content to prepare. Assembly turns it
    /// into a [`DocumentPlan::retirement`](crate::durable::session::assemble::DocumentPlan::retirement),
    /// which is what makes the retirement observable through adoption.
    pub fn stage_retirement(&mut self, record: crate::durable::types::DocumentRecord) {
        self.retirements.push(record);
    }

    /// Retire-only records staged by this transaction.
    #[must_use]
    pub fn staged_retirements(&self) -> &[crate::durable::types::DocumentRecord] {
        &self.retirements
    }

    /// Whether this transaction retires `id`, by draft or by retire-only plan.
    #[must_use]
    pub fn retires_record(&self, id: crate::durable::ids::DocumentId) -> bool {
        self.retirements
            .iter()
            .any(|record| record.id == id)
            || self
                .writes
                .iter()
                .any(|write| matches!(write, StorageWrite::DocumentRetire { id: retired } if *retired == id))
            || self
                .documents
                .iter()
                .any(|document| document.retire && document.loaded.as_ref().is_some_and(|loaded| loaded.record.id == id))
    }

    /// Mark a staged document for retirement.
    pub fn retire_document(&mut self, address_id: &str) {
        if let Some(document) = self.latest_document_mut(address_id) {
            document.retire = true;
        }
    }

    /// The submission changes staged so far.
    #[must_use]
    pub fn submission_changes(&self) -> &[(SubmissionId, SubmissionChange)] {
        &self.submission_changes
    }

    /// The submission records staged so far.
    pub fn staged_submissions(&self) -> impl Iterator<Item = &SubmissionRecord> {
        self.submissions.values()
    }

    /// Seal the transaction: no further staging is allowed.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// The table writes staged so far (the order the callback made them).
    #[must_use]
    pub fn writes(&self) -> &[StorageWrite] {
        &self.writes
    }

    /// Take the staged writes for assembly (drains the vector).
    pub fn take_writes(&mut self) -> Vec<StorageWrite> {
        std::mem::take(&mut self.writes)
    }

    /// Whether this transaction staged nothing durable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
            && self.tasks.is_empty()
            && self.submissions.is_empty()
            && self.submission_changes.is_empty()
            && self.documents.is_empty()
    }

    /// The scope of a resolved address.
    #[must_use]
    pub fn address_scope(resolved: &ResolvedAddress) -> DocumentScope {
        resolved.address.scope
    }

    /// Build the storage create for a staged document, given its new id.
    #[must_use]
    pub fn document_create(
        document: &StagedDocument,
        id: DocumentId,
        semantics: crate::durable::documents::DocumentSemantics,
    ) -> crate::durable::types::DocumentCreate {
        crate::durable::documents::document_create(semantics, &document.address, id)
    }

    /// The semantics of the definition that produced this staged document.
    ///
    /// A singleton and a family carry the same three semantic fields; the
    /// family definition is used only when no singleton was staged (a
    /// definition-free fork copy has neither and returns `None`).
    #[must_use]
    pub fn document_semantics(
        document: &StagedDocument,
    ) -> Option<crate::durable::documents::DocumentSemantics> {
        if let Some(definition) = &document.definition {
            return Some(definition.semantics);
        }
        document.family.as_ref().map(|family| family.semantics)
    }

    /// The status of the staged task state, for the terminal-document sweep.
    #[must_use]
    pub fn staged_terminal_tasks(&self) -> Vec<TaskId> {
        self.tasks
            .values()
            .filter_map(|entry| entry.record.as_ref())
            .filter(|record| record.state.is_terminal())
            .map(|record| record.id)
            .collect()
    }

    /// Whether a staged task state is `running`.
    #[must_use]
    pub fn staged_task_is_running(&self, id: TaskId) -> bool {
        self.staged_task(id)
            .is_some_and(|record| matches!(record.state, TaskState::Running { .. }))
    }
}
