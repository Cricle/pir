//! Transaction assembly and adoption.
//!
//! Port of upstream `session/transaction.ts`'s `#assemble` (`:784-876`),
//! `planDocument` (`:1016-1063`), `publishes` (`:1069-1071`),
//! `#rejectForkSourceWrites` (`:904-923`), `#validateOwners` (`:884-902`),
//! the task-replacement validation inside `#assemble` (`:793-801`), the
//! terminal-task document sweep (`:803-834`), the publication-ownership
//! resolution (`:836-848`), `applySubmissionChange` (`:69-81`) and `adopt`
//! (`:728-782`).
//!
//! Assembly is the phase between a settled commit callback and storage
//! admission. It turns every staged draft into an immutable plan, validates
//! everything that needs committed state (owners, task replacements), settles
//! submissions, and produces the exact write batch. Nothing here reaches
//! storage except **reads**, and no plan is observable until
//! [`adopt`] runs after [`Storage::commit`](crate::durable::Storage::commit)
//! succeeded.
//!
//! # Ordering rules (spec §4)
//!
//! * Every staged document is planned, in staging order (`:787-790`).
//! * Fork-source writes are rejected before any other validation (`:791`).
//! * Owners are validated on the **final candidate** record (`:792`, `:884-902`).
//! * Task replacements are validated against committed state (`:793-801`).
//! * Terminal settlement retires every document of the task scope, including
//!   ones created in this batch, and scans storage for the rest with a page
//!   size of 256 (`:803-834`).
//! * Publication ownership is resolved **before** storage admission so
//!   adoption performs no reads (`:836-848`).
//! * Submission changes resolve against the transaction's latest candidate
//!   record, falling back to committed state (`:850-855`).
//! * Writes are emitted as: the callback's own table writes, staged
//!   submissions, staged tasks, then each plan's content write and its retire
//!   write (`:857-875`).
//! * Checkpoint predicates run **last**, after every validation (`:862-871`).
//!
//! # What `StagedDocument` does not carry
//!
//! Upstream's `DocumentEntry` holds the *behaviour* definition
//! (`AnyDocDefinition`, with `checkpointWhen`) and a `DocumentTarget` that names
//! either the minted incarnation id or the fork-copy source. The Rust
//! [`StagedDocument`] holds only the identity form ([`DocDefinition`] /
//! [`DocFamilyDefinition`]) and no id or copy source. Three consequences, all
//! reported as API gaps to the transaction owner:
//!
//! 1. **Checkpoint predicates** are not reachable from a staged document, so
//!    [`assemble`] runs with no predicates and [`assemble_with`] accepts a
//!    [`DocBehaviours`] resolver. [`plan_document`] never consults a predicate;
//!    it emits a delta and assembly upgrades it to a base, exactly where
//!    upstream evaluates `checkpointWhen` (`:863-872`).
//! 2. **Creation ids** are minted by assembly ([`Storage::mint_id`], through
//!    the failure guard) and stamped onto the plan, mirroring upstream
//!    `#acquire`'s mint under the same "never reuse an id" rule. A creation
//!    plan returned directly by [`plan_document`] carries
//!    [`DocumentId::from_raw(0)`] until [`DocumentPlan::stamp_created_id`] runs.
//! 3. **Fork copies** cannot be planned from staging because the source
//!    incarnation is not recorded on the staged document. [`adopt`] still emits
//!    the `document.copy` publication for a copy plan built by the fork path,
//!    and `DocumentPlan` exposes the constructors a fork path needs.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::durable::chord::diff_objects;
use crate::durable::context::Context;
use crate::durable::delta::Op;
use crate::durable::documents::{address_id, document_create, DocCheckpointWhen};
use crate::durable::errors::StorageError;
use crate::durable::ids::{ConversationId, DocumentId, Seq, SubmissionId, TaskId};
use crate::durable::session::guard::GuardedStorage;
use crate::durable::session::line::SessionState;
use crate::durable::session::transaction::{StagedDocument, SubmissionChange, Transaction};
use crate::durable::types::{
    DocumentCommitChange, DocumentContent, DocumentCopySource, DocumentCreate, DocumentFork,
    DocumentPoint, DocumentPointQuery, DocumentQuery, DocumentRecord, DocumentScope, JsonObject,
    StorageWrite, SubmissionRecord, SubmissionStatus, SubmissionType, TaskRecord, TaskState,
};

/// Upstream `INTERNAL_SCAN_PAGE_SIZE`: the page size of the terminal-task
/// document sweep (`transaction.ts:824`). Internal to the scan; never
/// observable.
const INTERNAL_SCAN_PAGE_SIZE: usize = 256;

// ─── Committed state view ───────────────────────────────────────────────────

/// Committed state assembly may read.
///
/// `task` is the committed half of upstream `#committedTask` (`:1008-1012`);
/// `conversation_exists` completes the same view for callers that resolve a
/// conversation before assembly. Assembly itself does not need it: upstream
/// validates a conversation when a callback stages a conversation-scoped
/// operation (`transaction.ts:970-975`), not during assembly.
pub trait TaskLookup: Send + Sync {
    /// The committed record for one task, when it exists.
    fn task(&self, id: TaskId) -> Option<&TaskRecord>;

    /// Whether a conversation is visible to this view.
    fn conversation_exists(&self, id: ConversationId) -> bool;
}

/// The inputs assembly reads from committed state.
///
/// Upstream passes the whole `TransactionHost`; Rust passes only what assembly
/// actually consults, so a caller (or a test) can supply a detached snapshot.
#[derive(Clone, Copy)]
pub struct AssemblyInput<'a> {
    /// Committed tasks and conversations.
    pub committed: &'a dyn TaskLookup,
    /// The session clock (`host.now()`, `transaction.ts:103`). Assembly itself
    /// stamps nothing — lifecycle times are stamped when a task record is
    /// replaced (`#stampTimes`, `:981-992`) — but the value is part of the
    /// host surface assembly is handed.
    pub now: i64,
}

/// A detached snapshot of the committed records assembly needs.
#[derive(Clone, Debug, Default)]
struct TaskSnapshot {
    tasks: HashMap<i64, TaskRecord>,
    conversations: HashSet<i64>,
}

impl TaskSnapshot {
    fn insert(&mut self, record: TaskRecord) {
        self.conversations.insert(record.conversation_id.as_i64());
        self.tasks.insert(record.id.as_i64(), record);
    }

    fn note_conversation(&mut self, id: ConversationId) {
        self.conversations.insert(id.as_i64());
    }
}

impl TaskLookup for TaskSnapshot {
    fn task(&self, id: TaskId) -> Option<&TaskRecord> {
        self.tasks.get(&id.as_i64())
    }

    fn conversation_exists(&self, id: ConversationId) -> bool {
        self.conversations.contains(&id.as_i64())
    }
}

/// Documentation-only resolver for definition behaviour. See the module docs.
pub struct NoDocBehaviours;

impl DocBehaviours for NoDocBehaviours {}

/// The definition behaviour assembly needs but a [`StagedDocument`] cannot
/// carry.
///
/// Upstream's `DocumentPlan.change.definition` is the erased
/// `AnyDocDefinition`; the port's staged document keeps only the identity form,
/// so the predicate is resolved here by logical address and kind. A resolver
/// that returns `None` (the default) declares "no checkpoint predicate",
/// which is what upstream's `undefined` means.
pub trait DocBehaviours: Send + Sync {
    /// The `checkpointWhen` predicate of the definition at `address_id`, if it
    /// declares one.
    fn checkpoint_when(&self, address_id: &str, kind: &str) -> Option<DocCheckpointWhen> {
        let _ = (address_id, kind);
        None
    }
}

// ─── Plans ──────────────────────────────────────────────────────────────────

/// The record half of a plan.
///
/// Upstream `DocumentPlan.record` is `DocumentCreate | DocumentRecord`: a
/// creation has no committed lifetime yet, a loaded or scanned incarnation
/// already carries `createdAt`.
#[derive(Clone, Debug, PartialEq)]
pub enum DocumentCreateOrRecord {
    /// A new incarnation, stamped by storage at commit.
    Create(DocumentCreate),
    /// An already committed incarnation.
    Record(DocumentRecord),
}

impl DocumentCreateOrRecord {
    /// The incarnation identifier.
    #[must_use]
    pub fn id(&self) -> DocumentId {
        match self {
            Self::Create(record) => record.id,
            Self::Record(record) => record.id,
        }
    }

    /// The stable document kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::Create(record) => &record.kind,
            Self::Record(record) => &record.kind,
        }
    }

    /// The ownership scope.
    #[must_use]
    pub fn scope(&self) -> DocumentScope {
        match self {
            Self::Create(record) => record.scope,
            Self::Record(record) => record.scope,
        }
    }

    /// The conversation history semantics.
    #[must_use]
    pub fn history(&self) -> Option<crate::durable::types::DocumentHistory> {
        match self {
            Self::Create(record) => record.history,
            Self::Record(record) => record.history,
        }
    }

    /// The fork semantics.
    #[must_use]
    pub fn fork(&self) -> Option<DocumentFork> {
        match self {
            Self::Create(record) => record.fork,
            Self::Record(record) => record.fork,
        }
    }

    /// The storage create record, for a creation.
    #[must_use]
    pub fn as_create(&self) -> Option<&DocumentCreate> {
        match self {
            Self::Create(record) => Some(record),
            Self::Record(_) => None,
        }
    }
}

/// What a plan writes, classified (`transaction.ts:1016-1063`).
///
/// `None` is a plan that writes no content: a loaded incarnation with no
/// operations at the same version, or a retirement-only entry.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanContent {
    /// `document.create`: a new incarnation and its initial base.
    Create(DocumentCreate),
    /// `document.copy`: a definition-free fork copy.
    Copy {
        /// The new incarnation.
        record: DocumentCreate,
        /// The exact source point.
        source: DocumentCopySource,
    },
    /// `document.change`: content applied to an existing incarnation.
    Change {
        /// The incarnation.
        id: DocumentId,
        /// The new content.
        content: DocumentContent,
    },
    /// No content write.
    None,
}

impl PlanContent {
    /// Classify the content write of `plan`.
    #[must_use]
    pub fn of(plan: &DocumentPlan) -> Self {
        match &plan.content {
            Some(StorageWrite::DocumentCreate { record, .. }) => Self::Create(record.clone()),
            Some(StorageWrite::DocumentCopy { record, source }) => Self::Copy {
                record: record.clone(),
                source: *source,
            },
            Some(StorageWrite::DocumentChange { id, content }) => Self::Change {
                id: *id,
                content: content.clone(),
            },
            _ => Self::None,
        }
    }

    /// Turn the classification into the storage write. `initial` supplies the
    /// base a creation requires; it is ignored for every other variant
    /// (upstream `planDocument` builds the base from the prepared value).
    #[must_use]
    pub fn into_write(self, initial: DocumentContent) -> Option<StorageWrite> {
        match self {
            Self::Create(record) => Some(StorageWrite::DocumentCreate {
                record,
                content: initial,
            }),
            Self::Copy { record, source } => Some(StorageWrite::DocumentCopy { record, source }),
            Self::Change { id, content } => Some(StorageWrite::DocumentChange { id, content }),
            Self::None => None,
        }
    }
}

/// What one staged incarnation writes and publishes, decided once before
/// storage admission (`transaction.ts:170-191`).
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentPlan {
    /// Stable identity of the logical address (`plan.addressId`).
    pub address_id: String,
    /// The incarnation, new or committed.
    pub record: DocumentCreateOrRecord,
    /// Whether this commit retires the incarnation.
    pub retire: bool,
    /// The content write: creation, copy, or change; absent when only a
    /// retirement is written.
    pub content: Option<StorageWrite>,
    /// The owning conversation, resolved before storage admission so adoption
    /// performs no reads (`transaction.ts:836-848`).
    pub conversation_id: Option<ConversationId>,
    /// Definition version of the prepared revision (`change.version`).
    pub version: i64,
    /// Whether the prepared revision stays at the loaded incarnation's stored
    /// definition version (upstream `loaded.storedVersion === version`).
    ///
    /// A commit may be held in memory only when this is true: a migration
    /// stores a base at once, and a creation, copy or retirement is never
    /// deferrable. A checkpoint-upgraded base at the stored version is still
    /// `true` (upstream holds it and stores a base later).
    pub same_version: bool,
    /// The prepared value (`change.prepared.value`) of a **delta**.
    ///
    /// A base carries its prepared value inside [`Self::content`] instead; both
    /// forms are read through [`Self::publication_value`]. Keeping one owner
    /// per revision is what removes the former third copy of every written
    /// document.
    pub new_value: Option<JsonObject>,
    /// The prepared operations (`change.prepared.ops`), relative to the loaded
    /// revision. Empty for a creation, a copy or a retirement.
    pub ops: Vec<Op>,
}

impl DocumentPlan {
    /// Whether the plan creates a new incarnation.
    #[must_use]
    pub fn is_creation(&self) -> bool {
        matches!(self.record, DocumentCreateOrRecord::Create(_))
    }

    /// The incarnation identifier.
    #[must_use]
    pub fn record_id(&self) -> DocumentId {
        self.record.id()
    }

    /// The document kind.
    #[must_use]
    pub fn record_kind(&self) -> &str {
        self.record.kind()
    }

    /// The ownership scope.
    #[must_use]
    pub fn record_scope(&self) -> DocumentScope {
        self.record.scope()
    }

    /// The fork semantics.
    #[must_use]
    pub fn record_fork(&self) -> Option<DocumentFork> {
        self.record.fork()
    }

    /// Stamp the minted incarnation id onto a creation, and onto its content
    /// write. Returns whether the plan is a creation.
    pub fn stamp_created_id(&mut self, id: DocumentId) -> bool {
        let DocumentCreateOrRecord::Create(record) = &mut self.record else {
            return false;
        };
        record.id = id;
        match &mut self.content {
            Some(StorageWrite::DocumentCreate {
                record: content_record,
                ..
            }) => content_record.id = id,
            // A fork copy is a creation too: its write carries the same record
            // the plan does, so the minted id has to reach both.
            Some(StorageWrite::DocumentCopy {
                record: content_record,
                ..
            }) => content_record.id = id,
            _ => {}
        }
        true
    }

    /// A retirement-only plan for an incarnation discovered by a scan
    /// (`transaction.ts:828`).
    #[must_use]
    pub fn retirement(record: DocumentRecord) -> Self {
        let address = crate::durable::types::DocumentAddress {
            kind: record.kind.clone(),
            scope: record.scope,
            key: record.key.clone(),
        };
        Self {
            address_id: address_id(&address),
            record: DocumentCreateOrRecord::Record(record),
            retire: true,
            content: None,
            conversation_id: None,
            version: 0,
            same_version: false,
            new_value: None,
            ops: Vec::new(),
        }
    }

    /// A definition-free fork copy of one parent incarnation
    /// (`prepareForkDocumentCopies` → `#stageConversation`).
    ///
    /// A copy carries no prepared value: storage materializes the child from
    /// the source at the recorded point. It is still a plan so that adoption
    /// publishes `DocumentCommitChange::Copy` and the document cache learns the
    /// new incarnation; staging it as a bare write would commit the copy and
    /// tell nobody.
    #[must_use]
    pub fn copy(record: DocumentCreate, source: DocumentCopySource) -> Self {
        let address = crate::durable::types::DocumentAddress {
            kind: record.kind.clone(),
            scope: record.scope,
            key: record.key.clone(),
        };
        let conversation_id = record.scope.conversation_id();
        Self {
            address_id: address_id(&address),
            record: DocumentCreateOrRecord::Create(record.clone()),
            retire: false,
            content: Some(StorageWrite::DocumentCopy { record, source }),
            conversation_id,
            version: 0,
            same_version: false,
            new_value: None,
            ops: Vec::new(),
        }
    }

    /// The prepared value adoption publishes (`change.prepared.value`).
    ///
    /// A base's prepared value *is* the base the content write carries, so a
    /// base (a version transition, a checkpoint upgrade or a creation) keeps
    /// only that one copy; a delta carries no value in its content write, so
    /// its prepared revision lives in [`Self::new_value`]. Adoption clones this
    /// once into the publication — the write and the observers each own the
    /// revision, which is the minimum two owners the batch/publication split
    /// allows.
    #[must_use]
    pub fn publication_value(&self) -> Option<&JsonObject> {
        if let Some(value) = &self.new_value {
            return Some(value);
        }
        match &self.content {
            Some(StorageWrite::DocumentChange {
                content: DocumentContent::Base { value, .. },
                ..
            })
            | Some(StorageWrite::DocumentCreate {
                content: DocumentContent::Base { value, .. },
                ..
            }) => Some(value),
            _ => None,
        }
    }

    /// Whether adoption publishes this plan (`publishes`, `:1069-1071`): every
    /// retirement, every creation or copy, and a loaded incarnation that writes
    /// content — which includes a migration-only base so observers of the older
    /// shape receive the new value.
    #[must_use]
    pub fn publishes(&self) -> bool {
        self.retire || self.is_creation() || self.content.is_some()
    }
}

// ─── planDocument ───────────────────────────────────────────────────────────

/// Plan one staged document (`planDocument`, `transaction.ts:1016-1063`).
///
/// Returns `None` when the staged entry has no target: a retirement request
/// that never resolved a committed incarnation carries no record to retire.
///
/// A loaded incarnation is diffed against its committed value
/// ([`diff_objects`] stands in for upstream's tracker `Prepared`, borrowing both
/// maps so the diff itself copies nothing). The
/// version rule is upstream's (`:1046-1052`): **a version change stores a base
/// even with empty operations**, otherwise only a non-empty operation batch
/// writes a delta, and an unchanged same-version value writes nothing.
///
/// A creation is planned with [`DocumentId::from_raw(0)`]; assembly mints the
/// real id and calls [`DocumentPlan::stamp_created_id`], because a
/// [`StagedDocument`] carries no id (see the module docs).
#[must_use]
pub fn plan_document(staged: &StagedDocument) -> Option<DocumentPlan> {
    let (_, version) = staged.kind_and_version();
    let address_id = staged.address_id.clone();

    if let Some(loaded) = &staged.loaded {
        let id = loaded.record.id;
        let record = DocumentCreateOrRecord::Record(loaded.record.clone());
        // An unchanged same-version draft writes and publishes nothing. This is
        // the no-op `doc()` (a draft staged with the value it loaded), and it
        // pays no copy at all: the plan keeps neither a value nor operations.
        if loaded.stored_version == version && loaded.value == staged.value {
            return Some(DocumentPlan {
                address_id,
                record,
                retire: staged.retire,
                content: None,
                conversation_id: None,
                version,
                same_version: true,
                new_value: None,
                ops: Vec::new(),
            });
        }
        // The diff borrows both maps directly ([`diff_objects`]): the former
        // shape wrapped each side in a `Value::Object`, deep-cloning the whole
        // document twice per plan. The draft is copied exactly once here, into
        // the write (a base) or into `new_value` (a delta), which is the one
        // owned revision the plan has to keep.
        let ops = diff_objects(&loaded.value, &staged.value);
        // A version change stores a base even without operations; otherwise
        // only a change stores a delta (`transaction.ts:1043-1052`). The test
        // is `!=`, not `<`: ANY version change (a definition downgrade
        // included) must write a base, because a delta is only valid at the
        // version the stored revision chain was written at.
        let (content, new_value) = if loaded.stored_version != version {
            // The write owns the revision; the publication reads it back from
            // the write ([`DocumentPlan::publication_value`]) instead of the
            // plan keeping a second copy.
            (
                Some(DocumentContent::Base {
                    version,
                    value: staged.value.clone(),
                }),
                None,
            )
        } else if ops.is_empty() {
            (None, Some(staged.value.clone()))
        } else {
            // The write's op batch and the plan's are separate owned lists; the
            // publication reuses the plan's batch.
            (
                Some(DocumentContent::Delta {
                    version,
                    ops: ops.clone(),
                }),
                Some(staged.value.clone()),
            )
        };
        return Some(DocumentPlan {
            address_id,
            record,
            retire: staged.retire,
            content: content.map(|content| StorageWrite::DocumentChange { id, content }),
            conversation_id: None,
            version,
            same_version: loaded.stored_version == version,
            new_value,
            ops,
        });
    }

    // No loaded incarnation: only a creation has a target (`:1020-1031`).
    // A definition-free fork copy needs a source that the staged document does
    // not carry, so it cannot be planned here (module docs, gap 3).
    if !staged.created {
        return None;
    }
    let semantics = Transaction::document_semantics(staged)?;
    let record = document_create(semantics, &staged.address, DocumentId::from_raw(0));
    // One owned revision: the creation's base, which the publication reads back
    // through [`DocumentPlan::publication_value`].
    let content = Some(StorageWrite::DocumentCreate {
        record: record.clone(),
        content: DocumentContent::Base {
            version,
            value: staged.value.clone(),
        },
    });
    Some(DocumentPlan {
        address_id,
        record: DocumentCreateOrRecord::Create(record),
        retire: staged.retire,
        content,
        conversation_id: None,
        version,
        same_version: false,
        new_value: None,
        ops: Vec::new(),
    })
}

// ─── assemble ───────────────────────────────────────────────────────────────

/// Assemble the atomic batch for one settled transaction (`#assemble`,
/// `transaction.ts:784-876`).
///
/// Runs with no definition behaviour; a caller that can resolve
/// `checkpointWhen` uses [`assemble_with`].
pub async fn assemble(
    tx: &mut Transaction,
    session: &SessionState<'_>,
    storage: &GuardedStorage,
) -> Result<(Vec<StorageWrite>, Vec<DocumentPlan>), StorageError> {
    assemble_with(tx, session, storage, &NoDocBehaviours).await
}

/// [`assemble`] with the definition behaviour a [`StagedDocument`] cannot
/// carry.
pub async fn assemble_with(
    tx: &mut Transaction,
    session: &SessionState<'_>,
    storage: &GuardedStorage,
    behaviours: &dyn DocBehaviours,
) -> Result<(Vec<StorageWrite>, Vec<DocumentPlan>), StorageError> {
    let context = tx.context().clone();

    // Plan every staged incarnation (`:787-790`), then every retire-only
    // record (`:1038`): a retirement discovered without a draft carries only
    // its committed record, and adopting it is what makes the retirement
    // observable.
    let mut plans: Vec<DocumentPlan> = tx.documents().iter().filter_map(plan_document).collect();
    // Plans a callback prepared itself (a fork's definition-free copies, which
    // carry a source incarnation no `StagedDocument` can hold).
    for plan in tx.staged_plans() {
        if !plans
            .iter()
            .any(|existing| existing.address_id == plan.address_id)
        {
            plans.push(plan.clone());
        }
    }
    for record in tx.staged_retirements() {
        if !plans
            .iter()
            .any(|plan| plan.retire && plan.record_id() == record.id)
        {
            plans.push(DocumentPlan::retirement(record.clone()));
        }
    }

    // Upstream mints an incarnation id while acquiring the draft (`:636`);
    // here the plan carries no id yet, so assembly mints it under the guard.
    for plan in &mut plans {
        if plan.is_creation() {
            let raw = storage.write(storage.storage().mint_id()).await?;
            plan.stamp_created_id(DocumentId::from_raw(raw));
        }
    }

    // Fork-source writes are rejected before anything else (`:791`).
    reject_fork_source_writes(tx, &plans)?;

    let snapshot = load_committed(tx, storage, &context, &plans).await?;
    let input = AssemblyInput {
        committed: &snapshot,
        now: session.now(),
    };

    // Owners are judged on their final candidate (`:792`, `:884-902`).
    validate_owners(tx, input.committed)?;
    // Task replacements are validated against committed state (`:793-801`).
    validate_task_replacements(tx, input.committed)?;

    // Terminal settlement retires every task document (`:803-834`).
    apply_terminal_retirement(tx, storage, &context, &mut plans).await?;

    // Publication ownership is resolved before storage admission (`:836-848`).
    resolve_publication_conversations(tx, input.committed, &mut plans);

    // Submission changes resolve against the latest candidate (`:850-855`).
    resolve_submissions(tx, storage, &context).await?;

    // Emit the batch in upstream order (`:857-875`).
    let mut writes = tx.take_writes();
    let mut submissions: Vec<SubmissionRecord> = tx.staged_submissions().cloned().collect();
    // Upstream iterates the insertion-ordered map; the port's map is unordered,
    // so the batch is emitted in ascending id order for determinism.
    submissions.sort_by_key(|record| record.id.as_i64());
    for record in submissions {
        writes.push(StorageWrite::Submission { value: record });
    }
    let tasks: Vec<TaskRecord> = tx.staged_tasks().into_iter().cloned().collect();
    for record in tasks {
        writes.push(StorageWrite::Task { value: record });
    }
    for plan in &mut plans {
        // Checkpoint predicates run last, after every validation (`:862-872`),
        // and turn a delta into a base. Only a loaded incarnation has a
        // definition (a scanned retirement plan has no content).
        if let Some(StorageWrite::DocumentChange {
            id,
            content: DocumentContent::Delta { version, .. },
        }) = &plan.content
        {
            if matches!(plan.record, DocumentCreateOrRecord::Record(_)) {
                if let (Some(predicate), Some(value)) = (
                    behaviours.checkpoint_when(&plan.address_id, plan.record_kind()),
                    plan.publication_value(),
                ) {
                    if predicate(value) {
                        let base = value.clone();
                        plan.content = Some(StorageWrite::DocumentChange {
                            id: *id,
                            content: DocumentContent::Base {
                                version: *version,
                                value: base,
                            },
                        });
                        // The base now owns the prepared revision; dropping the
                        // delta's copy keeps the upgraded plan at one value.
                        plan.new_value = None;
                    }
                }
            }
        }
        // The plan keeps its content: `line.rs` classifies the adopted write
        // (`is_base`) from `plan.content` after `adopt` runs. A base is
        // therefore the one case where the batch and the plan both own the
        // revision, which the batch/publication split cannot avoid.
        if let Some(content) = plan.content.clone() {
            writes.push(content);
        }
        if plan.retire {
            writes.push(StorageWrite::DocumentRetire {
                id: plan.record_id(),
            });
        }
    }
    Ok((writes, plans))
}

/// Load the committed tasks assembly needs into one snapshot.
///
/// Upstream reads them lazily through `#committedTask` (`:1008-1012`); the port
/// pre-loads exactly the ids assembly can consult — staged tasks, owner tasks
/// named by a staged conversation or task creation, and tasks owning a planned
/// document scope — so the rest of assembly is synchronous.
async fn load_committed(
    tx: &Transaction,
    storage: &GuardedStorage,
    context: &Context,
    plans: &[DocumentPlan],
) -> Result<TaskSnapshot, StorageError> {
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for record in tx.staged_tasks() {
        ids.insert(record.id.as_i64());
        if tx.task_is_created(record.id) {
            if let Some(owner) = record.owner {
                ids.insert(owner.as_i64());
            }
        }
    }
    for write in tx.writes() {
        if let StorageWrite::Conversation { value } = write {
            if let Some(owner) = &value.owner {
                ids.insert(owner.task_id.as_i64());
            }
        }
    }
    for plan in plans {
        if let DocumentScope::Task { task_id } = plan.record_scope() {
            ids.insert(task_id.as_i64());
        }
    }

    let mut snapshot = TaskSnapshot::default();
    for raw in ids {
        let id = TaskId::from_raw(raw);
        if let Some(record) = storage.read(context, storage.storage().task(id)).await? {
            snapshot.insert(record);
        }
    }
    for id in tx.created_conversations() {
        snapshot.note_conversation(*id);
    }
    for plan in plans {
        if let DocumentScope::Conversation { conversation_id } = plan.record_scope() {
            snapshot.note_conversation(conversation_id);
        }
    }
    Ok(snapshot)
}

/// The latest candidate record of one task, falling back to committed state
/// (`#currentTask`, `transaction.ts:1004-1006`). Never a caller table read.
fn current_task<'a>(
    tx: &'a Transaction,
    committed: &'a dyn TaskLookup,
    id: TaskId,
) -> Option<&'a TaskRecord> {
    tx.staged_task(id).or_else(|| committed.task(id))
}

/// Reject writes that would mutate a fork source (`#rejectForkSourceWrites`,
/// `transaction.ts:904-923`).
fn reject_fork_source_writes(tx: &Transaction, plans: &[DocumentPlan]) -> Result<(), StorageError> {
    let documents = tx.fork_source_documents();
    let conversations = tx.fork_source_conversations();
    for plan in plans {
        if plan.content.is_none() && !plan.retire {
            continue;
        }
        let id = plan.record_id();
        if documents.contains(&id) {
            return Err(StorageError::invalid(format!(
                "Cannot change fork source document {id} in the fork transaction"
            )));
        }
        if let DocumentScope::Conversation { conversation_id } = plan.record_scope() {
            if conversations.contains(&conversation_id)
                && plan.record_fork() == Some(DocumentFork::Current)
            {
                return Err(StorageError::invalid(format!(
                    "Cannot fork conversation {conversation_id} while changing its current-policy documents"
                )));
            }
        }
    }
    Ok(())
}

/// Validate the owners of new owned work (`#validateOwners`,
/// `transaction.ts:884-902`).
///
/// New owned work needs a live owner, judged on the owner's final candidate:
/// not `completing`, terminal, or abort-marked. A task therefore cannot create
/// owned work in the commit that finishes it (spec §5.5).
fn validate_owners(tx: &Transaction, committed: &dyn TaskLookup) -> Result<(), StorageError> {
    let mut owners: Vec<(&'static str, TaskId)> = Vec::new();
    for write in tx.writes() {
        if let StorageWrite::Conversation { value } = write {
            if let Some(owner) = &value.owner {
                owners.push(("Conversation owner task", owner.task_id));
            }
        }
    }
    for record in tx.staged_tasks() {
        if tx.task_is_created(record.id) {
            if let Some(owner) = record.owner {
                owners.push(("Task owner", owner));
            }
        }
    }
    for (what, task_id) in owners {
        let Some(task) = current_task(tx, committed, task_id) else {
            return Err(StorageError::invalid(format!(
                "{what} {task_id} does not exist"
            )));
        };
        if task.state.is_terminal() {
            return Err(StorageError::invalid(format!(
                "{what} {task_id} is terminal"
            )));
        }
        if matches!(task.state, TaskState::Completing { .. }) {
            return Err(StorageError::invalid(format!(
                "{what} {task_id} is completing"
            )));
        }
        if task.abort_requested {
            return Err(StorageError::invalid(format!(
                "{what} {task_id} is abort-marked"
            )));
        }
    }
    Ok(())
}

/// Validate task replacements against committed state (`transaction.ts:793-801`).
fn validate_task_replacements(
    tx: &Transaction,
    committed: &dyn TaskLookup,
) -> Result<(), StorageError> {
    for record in tx.staged_tasks() {
        if tx.task_is_created(record.id) {
            continue;
        }
        let Some(current) = committed.task(record.id) else {
            return Err(StorageError::invalid(format!(
                "Task {} does not exist",
                record.id
            )));
        };
        if current.state.is_terminal() {
            return Err(StorageError::invalid(format!(
                "Task {} is already terminal",
                record.id
            )));
        }
        if current.conversation_id != record.conversation_id {
            return Err(StorageError::invalid(format!(
                "Task {} cannot change conversations",
                record.id
            )));
        }
    }
    Ok(())
}

/// Retire every document of a terminal task scope (`transaction.ts:803-834`).
///
/// Plans created by this batch are retired in place; the remaining incarnations
/// are discovered by scanning storage, page size 256, and become
/// retirement-only plans. A task created by this transaction has no committed
/// documents to scan for.
async fn apply_terminal_retirement(
    tx: &Transaction,
    storage: &GuardedStorage,
    context: &Context,
    plans: &mut Vec<DocumentPlan>,
) -> Result<(), StorageError> {
    let terminal: Vec<TaskId> = tx
        .staged_tasks()
        .into_iter()
        .filter(|record| record.state.is_terminal())
        .map(|record| record.id)
        .collect();
    if terminal.is_empty() {
        return Ok(());
    }

    let mut retiring: HashSet<i64> = HashSet::new();
    for plan in plans.iter_mut() {
        let DocumentScope::Task { task_id } = plan.record_scope() else {
            continue;
        };
        if !terminal.contains(&task_id) {
            continue;
        }
        plan.retire = true;
        retiring.insert(plan.record_id().as_i64());
    }

    for task_id in terminal {
        if tx.task_is_created(task_id) {
            continue;
        }
        let mut cursor = None;
        loop {
            let page = storage
                .read(
                    context,
                    storage.storage().scan_documents(
                        DocumentQuery {
                            scope: DocumentScope::Task { task_id },
                            at: DocumentPointQuery::from(DocumentPoint::Current),
                            kind: None,
                        },
                        INTERNAL_SCAN_PAGE_SIZE,
                        cursor,
                    ),
                )
                .await?;
            for record in page.items {
                if !retiring.insert(record.id.as_i64()) {
                    continue;
                }
                plans.push(DocumentPlan::retirement(record));
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    Ok(())
}

/// Resolve each publishing plan's owning conversation before storage admission
/// (`transaction.ts:836-848`).
///
/// A conversation document owns its own conversation; a task document takes the
/// conversation of its task's latest candidate record (falling back to
/// committed), so a task created in this batch publishes into its conversation.
fn resolve_publication_conversations(
    tx: &Transaction,
    committed: &dyn TaskLookup,
    plans: &mut [DocumentPlan],
) {
    for plan in plans {
        if !plan.publishes() {
            continue;
        }
        match plan.record_scope() {
            DocumentScope::Conversation { conversation_id } => {
                plan.conversation_id = Some(conversation_id);
            }
            DocumentScope::Task { task_id } => {
                plan.conversation_id =
                    current_task(tx, committed, task_id).map(|record| record.conversation_id);
            }
            DocumentScope::Session => {}
        }
    }
}

/// Resolve staged submission changes (`applySubmissionChange`,
/// `transaction.ts:69-81`, applied at `:850-855`).
///
/// Each change resolves against the transaction's latest candidate record,
/// falling back to committed state, so it is not a caller table read and works
/// after the first table write.
async fn resolve_submissions(
    tx: &mut Transaction,
    storage: &GuardedStorage,
    context: &Context,
) -> Result<(), StorageError> {
    let changes: Vec<(SubmissionId, SubmissionChange)> = tx.submission_changes().to_vec();
    for (id, change) in changes {
        let candidate = tx
            .staged_submissions()
            .find(|record| record.id == id)
            .cloned();
        let current = match candidate {
            Some(record) => record,
            None => match storage
                .read(context, storage.storage().submission(id))
                .await?
            {
                Some(record) => record,
                None => {
                    return Err(StorageError::invalid(format!(
                        "Submission {id} does not exist"
                    )));
                }
            },
        };
        let next = apply_submission_change(&current, &change)?;
        if next != current {
            tx.stage_submission(next);
        }
    }
    Ok(())
}

/// Complete record after applying one change (`applySubmissionChange`,
/// `transaction.ts:69-81`).
///
/// Placement turns a queued input `placed` and a queued write `done`; only a
/// placed input can be answered; a settled record stays.
pub fn apply_submission_change(
    current: &SubmissionRecord,
    change: &SubmissionChange,
) -> Result<SubmissionRecord, StorageError> {
    use SubmissionStatus as Status;
    if matches!(current.status, Status::Done | Status::Unanswered) {
        return Ok(current.clone());
    }
    match change {
        SubmissionChange::Placed { entry } => {
            if current.status != Status::Queued {
                return Err(StorageError::invalid(format!(
                    "Submission {} is not queued",
                    current.id
                )));
            }
            let status = match current.submission_type {
                SubmissionType::Input => Status::Placed,
                SubmissionType::Write => Status::Done,
            };
            Ok(SubmissionRecord {
                status,
                entry: Some(*entry),
                answer: None,
                reason: None,
                detail: None,
                ..current.clone()
            })
        }
        SubmissionChange::Done { answer } => {
            if current.status != Status::Placed {
                return Err(StorageError::invalid(format!(
                    "Submission {} is not a placed input",
                    current.id
                )));
            }
            Ok(SubmissionRecord {
                status: Status::Done,
                answer: Some(*answer),
                reason: None,
                detail: None,
                ..current.clone()
            })
        }
        SubmissionChange::Unanswered { reason, detail } => Ok(SubmissionRecord {
            status: Status::Unanswered,
            answer: None,
            reason: Some(reason.clone()),
            detail: detail.clone(),
            ..current.clone()
        }),
    }
}

// ─── adopt ──────────────────────────────────────────────────────────────────

/// Adopt every plan after storage settled (`adopt`, `transaction.ts:728-782`).
///
/// Stamps `createdAt` on a creation and `retiredAt` on a retirement with the
/// committed `seq`, then describes the publication:
///
/// * a retirement emits a `document` change with no version, a `null` value and
///   empty operations (`:760-761`);
/// * a copy emits `document.copy` with its exact source (`:762-768`);
/// * every other publishing plan emits a `document` change carrying the adopted
///   value and operations — empty operations for a creation (`:769-778`).
#[must_use]
pub fn adopt(plans: &[DocumentPlan], seq: Seq) -> Vec<DocumentCommitChange> {
    let mut publications = Vec::with_capacity(plans.len());
    for plan in plans {
        let mut record = match &plan.record {
            DocumentCreateOrRecord::Create(create) => DocumentRecord {
                id: create.id,
                kind: create.kind.clone(),
                key: create.key.clone(),
                created_at: seq,
                retired_at: None,
                scope: create.scope,
                history: create.history,
                fork: create.fork,
            },
            DocumentCreateOrRecord::Record(record) => record.clone(),
        };
        if plan.retire {
            record.retired_at = Some(seq);
            publications.push(DocumentCommitChange::Document {
                record,
                conversation_id: plan.conversation_id,
                version: None,
                value: None,
                ops: Vec::new(),
            });
            continue;
        }
        // Classify by borrowing the write: `PlanContent::of` would clone the
        // whole content (a base value or an op batch) just to find a copy.
        match &plan.content {
            Some(StorageWrite::DocumentCopy { source, .. }) => {
                let conversation_id = plan
                    .conversation_id
                    .or_else(|| record.scope.conversation_id())
                    .unwrap_or_default();
                publications.push(DocumentCommitChange::Copy {
                    record,
                    conversation_id,
                    source: *source,
                });
            }
            _ if plan.publishes() => {
                let ops = if plan.is_creation() {
                    Vec::new()
                } else {
                    plan.ops.clone()
                };
                let conversation_id = plan
                    .conversation_id
                    .or_else(|| record.scope.conversation_id());
                publications.push(DocumentCommitChange::Document {
                    record,
                    conversation_id,
                    version: Some(plan.version),
                    // The publication owns its revision; a base's write owns
                    // the plan's only copy, so this is the one clone the
                    // batch/publication split requires.
                    value: plan.publication_value().cloned(),
                    ops,
                });
            }
            _ => {}
        }
    }
    publications
}
