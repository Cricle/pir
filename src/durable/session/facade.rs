//! The host-facing `Session` facade.
//!
//! [`line`](super::line) owns the mutation line,
//! [`transaction`](super::transaction) the staging area,
//! [`assemble`](super::assemble) turns a settled callback into an atomic batch,
//! and [`publication`](super::publication) is the committed fan-out. This module
//! is the surface a host or harness uses: one [`Session`] per open storage,
//! typed commit conveniences, table reads that observe a single committed
//! state, and documents.
//!
//! # Read-on-line
//!
//! Every table read goes through [`SessionHandle::read_on_line`], so a
//! derivation that needs several records (`conversation` + `entry` +
//! `scan_entries`) observes exactly one committed state: the line runs the job
//! between two commits, and the guard refuses every call once the session
//! failed.
//!
//! # Commits
//!
//! [`Session::commit`] mirrors upstream `commit`/`commitWith`: the callback
//! stages work and the line settles it. Entries, conversations and fork copies
//! are staged as direct [`StorageWrite`]s and therefore commit immediately.
//! Tasks, submissions and document drafts are staged on the [`Transaction`]
//! (upstream's `#tasksById` / `#submissions` / `#documents`) and become writes
//! only in assembly.
//!
//! # Assembly
//!
//! Every facade commit ends with [`assemble_into`], which calls
//! [`super::assemble::assemble`]: it plans the staged drafts, mints creation
//! ids, validates owners and task replacements, sweeps terminal task documents,
//! resolves submissions and emits the complete batch. The batch is pushed back
//! onto the transaction, which the line hands to `Storage::commit`, so all
//! staging (table writes, submissions, tasks, document content and
//! retirements) settles in the caller's one commit.
//!
//! Assembly runs in the callback rather than in the line because
//! [`Transaction`] is callback-scoped. The line still builds its publication
//! from the write batch alone (no `DocumentCommitChange`s), so document
//! **publication** awaits that module's `adopt` integration; table changes
//! publish normally.
//!
//! # Typed conveniences
//!
//! `append_entry`, `create_conversation`, `create_task`, `set_task`,
//! `create_submission`, `doc` and `retire_doc` each run exactly one commit and
//! never read a record back after writing it (that would be a
//! [`ReadAfterWrite`](crate::durable::errors::ReadAfterWrite)); the created
//! record is returned from the callback.
//!
//! * `append_entry` mints its id inside the commit, resolves
//!   [`HeadTarget::SelfEntry`] to it and stamps `byTaskId` from the scope.
//! * `create_conversation` validates a task owner against the candidate then
//!   the committed record, prepares fork copies **before** staging the record
//!   (the fork scan is a read), mints each child document id and registers
//!   every source on the transaction.
//! * `create_task` resolves the owning conversation, computes the definition's
//!   checkpoint before the callback (a borrowed `&dyn TaskDefinitionLike`
//!   cannot cross the `'static` boundary) and mints the task id inside it.
//!
//! # Documents
//!
//! `doc` consults the loaded-document cache through
//! [`SessionHandle::maintain`], else reads storage on the line, materializes
//! the value with the definition's migration, runs the caller's edit on the
//! draft, and stages it, so assembly emits a base (version change) or a delta
//! (same version) exactly as upstream `planDocument` does. The cache is
//! invalidated after a successful edit or retirement so the next read reloads
//! the committed value.
//!
//! `snapshot` returns the **committed** value: the loaded cache first, then
//! storage. A draft staged by a transaction that is still open is not
//! observable from here (drafts are revoked when the callback settles); inside
//! a callback the read-your-writes form is [`Session::snapshot_staged`].
//!
//! # Subscriptions
//!
//! [`SessionHandle`] has no synchronous line access and `maintain` is `async`,
//! so `subscribe_commits` keeps host listeners in a facade-owned registry and
//! installs one fan-out listener on the line lazily, on the first commit
//! ([`Session::ensure_ready`]). Registration itself is immediate; only a commit
//! sent directly through [`Session::handle`] before the first facade commit can
//! precede the installation.
//!
//! # Deferred commits (`CommitPersistence`)
//!
//! [`Session::commit_with_options`] and [`Session::doc_with_options`] accept
//! upstream's [`CommitPersistence`]. A deferred commit is holdable only when
//! every write changes an existing conversation document declared
//! `history: "latest"`, `fork: "initial"` at its stored definition version; it
//! is then adopted in memory, published, and resolved, while storage sees it
//! later: before any document read ([`Session::snapshot`],
//! [`Session::snapshot_as_of`], the harness's attach paths) and before any
//! immediate commit, at [`Session::close`], or when its deadline timer fires
//! ([`Session::flush_deferred`] is the explicit form). Commits that create,
//! migrate, copy or retire a document, or that write tables, are stored
//! immediately. Because a held commit is not durable, a crash loses it; only
//! state whose loss is acceptable (streamed progress) should be deferred. The
//! immediate default keeps every existing invariant: the harness, scheduler and
//! task pipeline never defer.
//!
//! Deviation from upstream: a held commit is published to **all** commit
//! listeners when memory adopts it, not only to the Session's internal
//! listeners. The Rust [`CommitPublication`] has no memory/stored discriminator,
//! and the watchers/views in this crate are host listeners; publishing at
//! adoption keeps one monotonic frame stream. The later store publishes
//! nothing, so no observer applies the same operations twice.
//!
//! # Deviations forced by the existing API
//!
//! * `ConversationOwnership`, `SubmissionCreate` and `TaskDefinitionLike` live
//!   here: upstream's structural unions and executable-definition generics have
//!   no `types.rs` counterpart yet. [`TaskDefinitionLike::initial`] defaults to
//!   `null` for the metadata-only [`TaskDefinition`].
//! * `doc`'s edit closure returns `T`. The specified
//!   `FnOnce(&mut JsonObject)` cannot produce the `T` of `Result<T, _>`; the
//!   closure returns it instead.
//! * `snapshot` takes a [`ResolvedAddress`], not a typed token, so it has no
//!   definition to migrate with: it returns the stored value at its stored
//!   version. `doc` is the migration point. `snapshot_as_of` follows the same
//!   rule.
//! * `create_task` needs `I: Into<Value>` (upstream is generic over `I`).
//! * `TaskOptions::submission_ids` names the submissions whose stored trace
//!   carrier a new task inherits; `TaskOptions::trace` overrides it for callers
//!   that already hold a carrier.
//! * `closed` is a future ([`SessionClosed`]) and `subscribe_close` returns an
//!   id rather than an unsubscribe closure, matching this crate's
//!   `subscribe_commits` shape.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use serde_json::Value;
use tokio::sync::{watch, OnceCell};
use tokio::task::JoinHandle;

use crate::durable::context::Context;
use crate::durable::documents::{AnyDocDefinition, DocDefinition, ResolvedAddress};
use crate::durable::errors::{ReportInfo, StorageError};
use crate::durable::harness::telemetry::TraceCarrier;
use crate::durable::ids::{
    ConversationId, DocumentId, EntryId, SubmissionId, TaskId, ROOT_CONVERSATION_ID,
};
use crate::durable::session::documents::LoadedDocument;
use crate::durable::session::forks::{copy_plan, prepare_fork_document_copies};
use crate::durable::session::guard::SessionReport;
use crate::durable::session::line::{
    self, CloseListener, CommitFuture, CommitOptions, IntoJobValue, JobValue, SessionEnd,
    SessionHandle, SessionOptions, SessionState,
};
use crate::durable::session::publication::{run_contained, CommitListener};
use crate::durable::session::transaction::{
    EntryDraft, HeadTarget, StagedDocument, TaskOptions, TaskOwnership, Transaction,
    TransactionScope,
};
use crate::durable::tasks::{Task, TaskDefinition};
use crate::durable::types::{
    ConversationOwner, ConversationParent, ConversationQuery, ConversationRecord, Cursor,
    DocumentPoint, DocumentScope, EntryQuery, EntryRecord, JsonObject, Page, Storage, StorageWrite,
    StoredDocument, SubmissionQuery, SubmissionRecord, SubmissionStatus, SubmissionType, TaskQuery,
    TaskRecord, TaskState,
};

// ─── Facade-owned definition and option types ───────────────────────────────

/// Who owns a conversation created by [`Session::create_conversation`].
///
/// Upstream `ConversationOwnership` is the structural union
/// `{kind: "ownerless"} | {kind: "task", taskId}`; Rust expresses the same
/// choice as an enum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConversationOwnership {
    /// A conversation with no creator edge.
    #[default]
    Ownerless,
    /// A conversation owned by an existing task.
    Task(TaskId),
}

/// The fields of a submission before the session assigns its id.
///
/// Upstream derives this from the discriminated `SubmissionRecord` union. The
/// Rust record is one struct (see its doc comment), so the create form is the
/// same struct without `id`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionCreate {
    /// Conversation the submission belongs to.
    pub conversation_id: ConversationId,
    /// Host-provided deduplication key, scoped to the conversation.
    pub request_id: Option<String>,
    /// Input or passive write.
    pub submission_type: SubmissionType,
    /// Lifecycle status at creation.
    pub status: SubmissionStatus,
    /// Entry the submission placed, when already placed.
    pub entry: Option<EntryId>,
    /// Entry that answered the input, when already answered.
    pub answer: Option<EntryId>,
    /// Why a terminal submission could not be placed or answered.
    pub reason: Option<String>,
    /// Structured detail for `reason`.
    pub detail: Option<Value>,
    /// Trace context of the admission that creates the submission.
    pub trace: Option<TraceCarrier>,
}

impl SubmissionCreate {
    /// A queued submission of `submission_type` in `conversation_id`.
    #[must_use]
    pub fn new(conversation_id: ConversationId, submission_type: SubmissionType) -> Self {
        Self {
            conversation_id,
            request_id: None,
            submission_type,
            status: SubmissionStatus::Queued,
            entry: None,
            answer: None,
            reason: None,
            detail: None,
            trace: None,
        }
    }
}

/// The metadata [`Session::create_task`] needs from a task definition.
///
/// Upstream's `Task` carries executable phases and an `initial(input)` that
/// builds the starting checkpoint. The Rust [`TaskDefinition`] carries only
/// persisted metadata, so this trait adds the one behaviour task creation needs.
pub trait TaskDefinitionLike: Send + Sync {
    /// Registered task definition name.
    fn kind(&self) -> &str;
    /// Definition version persisted with the record.
    fn version(&self) -> i64;
    /// The complete durable state a new task starts from.
    fn initial(&self, input: &Value) -> Value;
}

impl TaskDefinitionLike for TaskDefinition {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn version(&self) -> i64 {
        self.version
    }

    fn initial(&self, _input: &Value) -> Value {
        // The metadata-only definition declares no starting checkpoint.
        Value::Null
    }
}

impl TaskDefinitionLike for Task {
    fn kind(&self) -> &str {
        self.definition.kind()
    }

    fn version(&self) -> i64 {
        self.definition.version()
    }

    fn initial(&self, input: &Value) -> Value {
        self.definition.initial(input)
    }
}

/// The future a [`ConversationCreatedHook`] returns.
pub type ConversationCreatedFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>;

/// Host hook run inside the commit that created a conversation.
///
/// Upstream `SessionImpl.conversationCreated` is an overridable async method; a
/// builder-installed hook is the Rust equivalent. It runs after the
/// conversation record and its fork copies are staged, before the callback
/// returns, so it may stage more work in the same commit. An `Err` fails the
/// commit and stages nothing.
pub type ConversationCreatedHook = dyn for<'a> Fn(&'a mut Transaction, &'a ConversationRecord) -> ConversationCreatedFuture<'a>
    + Send
    + Sync;

/// The awaitable end of a session (upstream `Session.closed`).
///
/// Settles once the session has closed, storage included — after [`Session::close`],
/// or after the first failed storage call, which closes the session itself.
/// A failed session's end carries the first error.
pub struct SessionClosed {
    future: Pin<Box<dyn Future<Output = SessionEnd> + Send>>,
}

impl SessionClosed {
    /// Build the future over a session-end receiver.
    fn new(mut receiver: watch::Receiver<Option<SessionEnd>>) -> Self {
        Self {
            future: Box::pin(async move {
                loop {
                    if let Some(end) = (*receiver.borrow()).clone() {
                        return end;
                    }
                    if receiver.changed().await.is_err() {
                        // The line is gone; without a recorded end there is
                        // nothing left to observe, which is a closed session.
                        return (*receiver.borrow()).clone().unwrap_or(SessionEnd::Closed);
                    }
                }
            }),
        }
    }

    /// Await the session's end.
    pub async fn wait(self) -> SessionEnd {
        self.future.await
    }
}

impl std::fmt::Debug for SessionClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionClosed")
    }
}

impl Future for SessionClosed {
    type Output = SessionEnd;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

// ─── Session ────────────────────────────────────────────────────────────────

/// A host-facing durable session over one storage backend.
///
/// [`Session::open`] starts the mutation line; [`Session::close`] seals
/// admission, closes the backend and joins the line task.
///
/// Cloning shares the one line, the subscription registry, the latched
/// installation result and the join handle: a clone is another view of the
/// **same** session, never a second writer.
#[derive(Clone)]
pub struct Session {
    /// The line handle every operation goes through.
    handle: SessionHandle,
    /// The line task, joined by `close`; shared so every clone joins once.
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Host commit listeners, in registration order.
    subscribers: Arc<Mutex<Vec<(u64, CommitListener)>>>,
    /// The next subscription id.
    next_subscriber: Arc<AtomicU64>,
    /// Whether the fan-out listener is installed on the line.
    ready: Arc<OnceCell<Result<(), StorageError>>>,
    /// The optional conversation-created hook.
    conversation_created: Option<Arc<ConversationCreatedHook>>,
}

impl Session {
    /// Open a session over `storage`.
    #[must_use]
    pub fn open(storage: Arc<dyn Storage>, options: SessionOptions) -> Self {
        Self::open_with_report(storage, options, None)
    }

    /// Open a session that reports failures that never fail their caller
    /// (upstream `HarnessOptions.onReport`); `None` drops them like a plain
    /// `Session`.
    #[must_use]
    pub fn open_with_report(
        storage: Arc<dyn Storage>,
        options: SessionOptions,
        report: Option<SessionReport>,
    ) -> Self {
        Self::open_with_report_and_timers(storage, options, report, None)
    }

    /// [`Session::open_with_report`] with the host timers the session arms its
    /// background work with (upstream `createSession({setTimer})`); `None`
    /// uses the native timer.
    #[must_use]
    pub fn open_with_report_and_timers(
        storage: Arc<dyn Storage>,
        options: SessionOptions,
        report: Option<SessionReport>,
        timers: Option<crate::durable::timers::Timers>,
    ) -> Self {
        let conversation_created = options.conversation_created.clone();
        let (handle, task) = line::open_with_report_and_timers(storage, options, report, timers);
        Self {
            handle,
            task: Arc::new(Mutex::new(Some(task))),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            next_subscriber: Arc::new(AtomicU64::new(1)),
            ready: Arc::new(OnceCell::new()),
            conversation_created,
        }
    }

    /// Install the host hook that runs inside every conversation-creating
    /// commit.
    #[must_use]
    pub fn with_conversation_created(mut self, hook: Arc<ConversationCreatedHook>) -> Self {
        self.conversation_created = Some(hook);
        self
    }

    /// The raw line handle.
    #[must_use]
    pub fn handle(&self) -> &SessionHandle {
        &self.handle
    }

    /// Whether the session already failed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.handle.is_failed()
    }

    /// The first failure that ended the session, if any.
    #[must_use]
    pub fn failure(&self) -> Option<StorageError> {
        self.handle.failure()
    }

    // ─── Commits ────────────────────────────────────────────────────────────

    /// Run one commit with the default [`TransactionScope`].
    pub async fn commit<T, F>(&self, context: Context, run: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.commit_scoped(TransactionScope::default(), context, run)
            .await
    }

    /// Run one commit bound to `scope`, assembling its batch before admission.
    ///
    /// `scope.conversation_id` is the default conversation for
    /// [`Session::create_task`] and `scope.task_id` is stamped as `byTaskId` on
    /// appended entries.
    ///
    /// The callback erases its value exactly once into the reply channel
    /// (`Ok(value.into_job_value())`, i.e. `Box<T>`); this wrapper passes that
    /// `JobValue` through **unchanged** and `T` names what it erased. It must
    /// not box the reply again: `SessionHandle::commit` recovers `T` from the
    /// single erasure (it accepts an already-erased wrapper too, so a double
    /// erasure cannot surface as a spurious type error).
    pub async fn commit_scoped<T, F>(
        &self,
        scope: TransactionScope,
        context: Context,
        run: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.commit_scoped_with_options(scope, context, CommitOptions::default(), run)
            .await
    }

    /// [`Session::commit`] with an explicit [`CommitOptions`].
    ///
    /// `options.persistence = CommitPersistence::Deferred { .. }` implements
    /// upstream's deferred commit: when every write changes an existing
    /// `history: "latest"`, `fork: "initial"` conversation document at its
    /// stored version, the commit is adopted in memory, published and resolved
    /// without storing; it reaches storage on the next document read, the next
    /// immediate commit, [`Session::close`] or its deadline. Anything else is
    /// stored immediately, as before.
    pub async fn commit_with_options<T, F>(
        &self,
        context: Context,
        options: CommitOptions,
        run: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.commit_scoped_with_options(TransactionScope::default(), context, options, run)
            .await
    }

    /// [`Session::commit_scoped`] with an explicit [`CommitOptions`].
    pub async fn commit_scoped_with_options<T, F>(
        &self,
        scope: TransactionScope,
        context: Context,
        options: CommitOptions,
        run: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.ensure_ready().await?;
        // `value` is the callback's already-erased `JobValue`; returning it
        // as-is keeps the erasure at one level and makes `T` the type the
        // line downcasts to (an explicit `T` removes any inference ambiguity).
        self.handle
            .commit_with_options::<T, _>(scope, context, options, move |tx, state| {
                Box::pin(async move {
                    let value = run(tx, state).await?;
                    assemble_into(tx, state).await?;
                    Ok(value)
                })
            })
            .await
    }

    // ─── Table reads (one committed state) ──────────────────────────────────

    /// One conversation, or `None`.
    pub async fn conversation(
        &self,
        id: ConversationId,
        ctx: &Context,
    ) -> Result<Option<ConversationRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let found = state
                        .storage
                        .read(&context, storage.conversation(id))
                        .await?;
                    let value: JobValue = Box::new(found);
                    Ok(value)
                })
            })
            .await
    }

    /// One entry, or `None`.
    pub async fn entry(
        &self,
        id: EntryId,
        ctx: &Context,
    ) -> Result<Option<EntryRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let found = state.storage.read(&context, storage.entry(id)).await?;
                    let value: JobValue = Box::new(found.map(|entry| entry.entry));
                    Ok(value)
                })
            })
            .await
    }

    /// One task, or `None`.
    pub async fn task(
        &self,
        id: TaskId,
        ctx: &Context,
    ) -> Result<Option<TaskRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let found = state.storage.read(&context, storage.task(id)).await?;
                    let value: JobValue = Box::new(found);
                    Ok(value)
                })
            })
            .await
    }

    /// One submission, or `None`.
    pub async fn submission(
        &self,
        id: SubmissionId,
        ctx: &Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let found = state.storage.read(&context, storage.submission(id)).await?;
                    let value: JobValue = Box::new(found);
                    Ok(value)
                })
            })
            .await
    }

    /// One page of a conversation's fork-aware entry history.
    pub async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        ctx: &Context,
    ) -> Result<Page<EntryRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let page = state
                        .storage
                        .read(&context, storage.scan_entries(query, limit, cursor))
                        .await?;
                    let value: JobValue = Box::new(page);
                    Ok(value)
                })
            })
            .await
    }

    /// One page of an ordered task scan.
    pub async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        ctx: &Context,
    ) -> Result<Page<TaskRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let page = state
                        .storage
                        .read(&context, storage.scan_tasks(query, limit, cursor))
                        .await?;
                    let value: JobValue = Box::new(page);
                    Ok(value)
                })
            })
            .await
    }

    /// One page of an ordered conversation scan.
    pub async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        ctx: &Context,
    ) -> Result<Page<ConversationRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let page = state
                        .storage
                        .read(&context, storage.scan_conversations(query, limit, cursor))
                        .await?;
                    let value: JobValue = Box::new(page);
                    Ok(value)
                })
            })
            .await
    }

    /// One page of an ordered submission scan.
    pub async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        ctx: &Context,
    ) -> Result<Page<SubmissionRecord>, StorageError> {
        self.ensure_ready().await?;
        let context = ctx.clone();
        self.handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let page = state
                        .storage
                        .read(&context, storage.scan_submissions(query, limit, cursor))
                        .await?;
                    let value: JobValue = Box::new(page);
                    Ok(value)
                })
            })
            .await
    }

    // ─── Typed commit conveniences (each inside ONE commit) ────────────────

    /// Append one entry and return the created record.
    ///
    /// The id is minted inside the commit; [`HeadTarget::SelfEntry`] resolves
    /// to it and `byTaskId` is stamped from the transaction scope.
    pub async fn append_entry(
        &self,
        conversation: ConversationId,
        draft: EntryDraft,
        ctx: Context,
    ) -> Result<EntryRecord, StorageError> {
        self.commit(ctx, move |tx, state| {
            Box::pin(async move {
                let record = stage_append_entry(tx, state, conversation, draft).await?;
                Ok(record.into_job_value())
            })
        })
        .await
    }

    /// Create (or fork) a conversation and return its record.
    pub async fn create_conversation(
        &self,
        ownership: ConversationOwnership,
        parent: Option<(ConversationId, EntryId)>,
        ctx: Context,
    ) -> Result<ConversationRecord, StorageError> {
        let hook = self.conversation_created.clone();
        self.commit(ctx, move |tx, state| {
            Box::pin(async move {
                let record = stage_create_conversation(tx, state, ownership, parent, hook).await?;
                Ok(record.into_job_value())
            })
        })
        .await
    }

    /// Create the reserved root conversation, or return the one that exists
    /// (upstream `tx.createRootConversation()`).
    ///
    /// Unlike [`Session::create_conversation`], the id is the reserved
    /// [`ROOT_CONVERSATION_ID`] rather than a minted one, so every process that
    /// opens the same storage agrees on which conversation is the root. The
    /// existence check and the creation are one commit; the
    /// [`ConversationCreatedHook`] runs inside it like any other creation.
    pub async fn create_root_conversation(
        &self,
        ctx: Context,
    ) -> Result<ConversationRecord, StorageError> {
        let hook = self.conversation_created.clone();
        self.commit(ctx, move |tx, state| {
            Box::pin(async move {
                let context = tx.context().clone();
                let storage = state.storage.storage();
                if let Some(existing) = state
                    .storage
                    .read(&context, storage.conversation(ROOT_CONVERSATION_ID))
                    .await?
                {
                    return Ok(existing.into_job_value());
                }
                let record = ConversationRecord {
                    id: ROOT_CONVERSATION_ID,
                    parent: None,
                    owner: None,
                };
                tx.stage_conversation(record.clone(), None);
                if let Some(hook) = hook {
                    hook(tx, &record).await?;
                }
                Ok(record.into_job_value())
            })
        })
        .await
    }

    /// Create a task and return its id.
    ///
    /// The conversation comes from the owner task, `options.conversation_id`,
    /// or the commit scope, in that order.
    pub async fn create_task<I>(
        &self,
        definition: &dyn TaskDefinitionLike,
        input: I,
        options: TaskOptions,
        ctx: Context,
    ) -> Result<TaskId, StorageError>
    where
        I: Into<Value> + Send + 'static,
    {
        let input = input.into();
        let snapshot = TaskSnapshot {
            kind: definition.kind().to_owned(),
            version: definition.version(),
            checkpoint: definition.initial(&input),
        };
        self.commit(ctx, move |tx, state| {
            Box::pin(async move {
                let id = stage_create_task(tx, state, snapshot, input, options).await?;
                Ok(id.into_job_value())
            })
        })
        .await
    }

    /// Replace one task record completely.
    ///
    /// Lifecycle times are stamped from the candidate or committed record, as
    /// upstream `setTask` does.
    pub async fn set_task(&self, record: TaskRecord, ctx: Context) -> Result<(), StorageError> {
        self.commit(ctx, move |tx, state| {
            Box::pin(async move {
                stage_set_task(tx, state, record).await?;
                Ok(().into_job_value())
            })
        })
        .await
    }

    /// Create a submission record with a fresh id.
    pub async fn create_submission(
        &self,
        create: SubmissionCreate,
        ctx: Context,
    ) -> Result<SubmissionRecord, StorageError> {
        self.commit(ctx, move |tx, state| {
            Box::pin(async move {
                let record = stage_create_submission(tx, state, create).await?;
                Ok(record.into_job_value())
            })
        })
        .await
    }

    // ─── Documents ──────────────────────────────────────────────────────────

    /// The committed value at `resolved`, if the document is live.
    ///
    /// The loaded cache is consulted first, then storage is read on the line.
    /// The value is returned at its stored version: without a definition there
    /// is nothing to migrate with (use [`Session::doc`] for that).
    pub async fn snapshot(
        &self,
        resolved: &ResolvedAddress,
        ctx: &Context,
    ) -> Result<Option<JsonObject>, StorageError> {
        // A read observes committed state: a held commit is stored before the
        // read, so a caller never sees a value the storage would not return
        // after a reopen.
        self.flush_deferred(ctx).await?;
        self.ensure_ready().await?;
        if let Some(cached) = self.cached_document(&resolved.id).await {
            if cached.record.retired_at.is_none() {
                return Ok(Some(cached.value));
            }
        }
        let address = resolved.address.clone();
        let context = ctx.clone();
        let stored: Option<StoredDocument> = self
            .handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let record = state
                        .storage
                        .read(
                            &context,
                            storage.find_document(address, DocumentPoint::Current),
                        )
                        .await?;
                    let Some(record) = record else {
                        let value: JobValue = Box::new(None::<StoredDocument>);
                        return Ok(value);
                    };
                    let stored = state
                        .storage
                        .read(
                            &context,
                            storage.document(record.id, DocumentPoint::Current),
                        )
                        .await?;
                    let value: JobValue = Box::new(stored);
                    Ok(value)
                })
            })
            .await?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let value = stored.value.clone();
        self.install_document(LoadedDocument {
            address_id: resolved.id.clone(),
            record: stored.record,
            stored_version: stored.version,
            value_version: stored.version,
            deltas_since_base: stored.deltas_since_base,
            value: stored.value,
        })
        .await;
        Ok(Some(value))
    }

    /// The committed value of a conversation document as of one entry
    /// (upstream `Session.snapshotAsOf`).
    ///
    /// `at` is the visible entry the history should be read at, and the value
    /// is the incarnation alive at that entry's commit, materialized at its
    /// stored version. A `with_replay`-style definition migration is not
    /// applied here, exactly like [`Session::snapshot`].
    ///
    /// An entry that is not visible from the document's conversation is a
    /// request error, because such a read has no durable effect and says
    /// nothing about the backend.
    pub async fn snapshot_as_of(
        &self,
        resolved: &ResolvedAddress,
        at: EntryId,
        ctx: &Context,
    ) -> Result<Option<JsonObject>, StorageError> {
        self.flush_deferred(ctx).await?;
        self.ensure_ready().await?;
        let conversation_id = match resolved.address.scope {
            DocumentScope::Conversation { conversation_id } => conversation_id,
            _ => {
                return Err(StorageError::invalid(
                    "Session::snapshot_as_of requires a conversation document",
                ))
            }
        };
        let address = resolved.address.clone();
        let context = ctx.clone();
        let stored: Option<StoredDocument> = self
            .handle
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let Some(entry) = state
                        .storage
                        .read(&context, storage.entry_in(conversation_id, at))
                        .await?
                    else {
                        return Err(StorageError::invalid(format!(
                            "Entry {at} is not visible from conversation {conversation_id}"
                        )));
                    };
                    // A fork child sees inherited entries from the parent
                    // conversation, and the document history must be read at
                    // that conversation.
                    let address = crate::durable::types::DocumentAddress {
                        scope: DocumentScope::Conversation {
                            conversation_id: entry.entry.conversation_id,
                        },
                        ..address
                    };
                    let point = DocumentPoint::Seq(entry.commit_seq);
                    let Some(record) = state
                        .storage
                        .read(&context, storage.find_document(address, point))
                        .await?
                    else {
                        let value: JobValue = Box::new(None::<StoredDocument>);
                        return Ok(value);
                    };
                    let stored = state
                        .storage
                        .read(&context, storage.document(record.id, point))
                        .await?;
                    let value: JobValue = Box::new(stored);
                    Ok(value)
                })
            })
            .await?;
        Ok(stored.map(|stored| stored.value))
    }

    /// The draft this transaction already staged for `resolved`, if any.
    ///
    /// This is the read-your-writes form available **inside** a commit
    /// callback; outside one, use [`Session::snapshot`], which reads only
    /// committed state.
    #[must_use]
    pub fn snapshot_staged(tx: &Transaction, resolved: &ResolvedAddress) -> Option<JsonObject> {
        tx.latest_document(&resolved.id)
            .filter(|document| !document.retire)
            .map(|document| document.value.clone())
    }

    /// Load, create, edit and stage a document in one commit.
    ///
    /// A defined document is materialized through the definition's migration; a
    /// new one starts from `definition.initial_value()`. The edit runs on the
    /// draft and its return value is this call's value.
    pub async fn doc<T, F>(
        &self,
        resolved: ResolvedAddress,
        definition: AnyDocDefinition,
        ctx: Context,
        edit: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut JsonObject) -> T + Send + 'static,
    {
        self.doc_with_options(resolved, definition, ctx, CommitOptions::default(), edit)
            .await
    }

    /// [`Session::doc`] with explicit [`CommitOptions`].
    ///
    /// A deferred edit is held in memory when the document is an existing
    /// `history: "latest"`, `fork: "initial"` conversation document; see
    /// [`Session::commit_with_options`].
    pub async fn doc_with_options<T, F>(
        &self,
        resolved: ResolvedAddress,
        definition: AnyDocDefinition,
        ctx: Context,
        options: CommitOptions,
        edit: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut JsonObject) -> T + Send + 'static,
    {
        self.ensure_ready().await?;
        if resolved.address.kind != definition.common.kind {
            return Err(StorageError::invalid(format!(
                "Document address kind {} does not match definition {}",
                resolved.address.kind, definition.common.kind
            )));
        }
        let cached = self.cached_document(&resolved.id).await;
        let address_id = resolved.id.clone();
        let result = self
            .commit_with_options(ctx, options, move |tx, state| {
                Box::pin(async move {
                    let value =
                        stage_document_edit(tx, state, resolved, definition, cached, edit).await?;
                    Ok(value.into_job_value())
                })
            })
            .await;
        if result.is_ok() {
            self.invalidate_document(address_id).await;
        }
        result
    }

    /// Retire the current incarnation at `resolved`, if one exists.
    pub async fn retire_doc(
        &self,
        resolved: ResolvedAddress,
        ctx: Context,
    ) -> Result<(), StorageError> {
        let address_id = resolved.id.clone();
        let result = self
            .commit(ctx, move |tx, state| {
                Box::pin(async move {
                    stage_retire_document(tx, state, resolved).await?;
                    Ok(().into_job_value())
                })
            })
            .await;
        if result.is_ok() {
            self.invalidate_document(address_id).await;
        }
        result
    }

    // ─── Close and subscriptions ────────────────────────────────────────────

    /// Seal admission, close the backend and join the line task.
    ///
    /// The join runs even when closing the backend fails: the line is gone
    /// either way, and the caller sees the backend's error.
    pub async fn close(&self) -> Result<(), StorageError> {
        let result = self.handle.close().await;
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        result
    }

    /// Write close-time bookkeeping straight to storage without publishing it
    /// (upstream `SessionImpl.writeAtClose`).
    ///
    /// Runs on the line, after every commit admitted before the seal settled,
    /// so a queued abort mark survives; a failed session writes nothing. A
    /// `beforeClose` hook (such as the harness writing its stopped tasks back
    /// as `pending`) calls this before [`Session::close`].
    pub async fn write_at_close<F>(&self, writes: F) -> Result<(), StorageError>
    where
        F: FnOnce(&SessionState<'_>) -> Vec<StorageWrite> + Send + 'static,
    {
        self.handle.write_at_close(writes).await
    }

    /// Store every change a deferred commit holds in memory, now.
    ///
    /// A no-op when nothing is held. Reads ([`Session::snapshot`],
    /// [`Session::snapshot_as_of`]), immediate commits and [`Session::close`]
    /// flush on their own; this is the explicit form.
    pub async fn flush_deferred(&self, _ctx: &Context) -> Result<(), StorageError> {
        self.handle.flush_deferred().await
    }

    /// The awaitable session end (upstream `Session.closed`).
    ///
    /// Settles after [`Session::close`] — or after the first failed storage
    /// call, which ends the session — with [`SessionEnd::Closed`] or
    /// [`SessionEnd::Failed`].
    #[must_use]
    pub fn closed(&self) -> SessionClosed {
        SessionClosed::new(self.handle.closed_receiver())
    }

    /// Register a listener called synchronously when close begins, also when a
    /// failure closes the session (upstream `subscribeClose`).
    ///
    /// The listener must not block or call session APIs; a panic is reported.
    /// Returns the id [`Session::unsubscribe_close`] removes.
    pub fn subscribe_close(&self, listener: CloseListener) -> Result<u64, StorageError> {
        self.handle.subscribe_close(listener)
    }

    /// Remove one close subscription by id. Unknown ids are ignored.
    pub fn unsubscribe_close(&self, id: u64) {
        self.handle.unsubscribe_close(id);
    }

    /// Subscribe to committed publications.
    ///
    /// Listeners run synchronously on the line after the commit settled, in
    /// registration order. A panicking listener is contained and never fails
    /// the session (host listener policy).
    pub fn subscribe_commits(&self, listener: CommitListener) -> Result<u64, StorageError> {
        if self.handle.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        let id = self.next_subscriber.fetch_add(1, Ordering::AcqRel);
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, listener));
        Ok(id)
    }

    /// Remove one subscription by id. Unknown ids are ignored.
    pub fn unsubscribe_commits(&self, id: u64) {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(key, _)| *key != id);
    }

    // ─── Internals ──────────────────────────────────────────────────────────

    /// Install the line-side fan-out listener exactly once.
    ///
    /// The installed listener dispatches to whatever is registered *at commit
    /// time*, so subscriptions added later need no second installation. The
    /// result is latched: a session that cannot install it (already closed)
    /// keeps returning that error instead of retrying.
    async fn ensure_ready(&self) -> Result<(), StorageError> {
        if let Some(result) = self.ready.get() {
            return result.clone();
        }
        let subscribers = self.subscribers.clone();
        let result = self
            .ready
            .get_or_init(|| async {
                self.handle
                    .maintain(move |inner| {
                        let subscribers = subscribers.clone();
                        let report = inner.report.clone();
                        let telemetry = inner.telemetry.clone();
                        let listener: CommitListener = Arc::new(move |publication, context| {
                            let listeners: Vec<CommitListener> = {
                                let registry = subscribers
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                registry
                                    .iter()
                                    .map(|(_, listener)| listener.clone())
                                    .collect()
                            };
                            for listener in &listeners {
                                // A failing host listener never fails the
                                // commit; it is reported (upstream `contained`).
                                if let Err(message) = run_contained(listener, publication, context)
                                {
                                    if let Some(telemetry) = telemetry.as_ref() {
                                        let span = context
                                            .span_handle()
                                            .filter(|span| !span.is_none())
                                            .or_else(|| telemetry.extract(context.trace_carrier()));
                                        telemetry.record_report(
                                            span,
                                            "listener_error",
                                            None,
                                            None,
                                            None,
                                        );
                                    }
                                    report(&message, ReportInfo::ListenerError);
                                }
                            }
                        });
                        inner.commit_listeners.add(listener);
                    })
                    .await
            })
            .await;
        result.clone()
    }

    /// The cached incarnation at `address_id`, touched for retention.
    ///
    /// The slot is a one-shot handoff out of `maintain`: the value is **moved**
    /// into it by the line closure and taken out here, so a cache hit pays only
    /// the single copy needed to hand the cached document to the caller.
    async fn cached_document(&self, address_id: &str) -> Option<LoadedDocument> {
        let slot: Arc<Mutex<Option<LoadedDocument>>> = Arc::new(Mutex::new(None));
        let write = slot.clone();
        let key = address_id.to_owned();
        let installed = self
            .handle
            .maintain(move |inner| {
                let now = inner.now();
                inner.documents.note_use(&key, now);
                // A touch may move the eviction deadline up (a shortened
                // retention), so the armed timer is re-evaluated.
                inner.arm_document_sweep();
                // Upstream `#touchDocument` hands the caller the cached
                // incarnation by reference; a session cache must own its
                // documents, so a hit pays exactly one `LoadedDocument` clone
                // — the copy the caller keeps. `take` leaves `None` behind, so
                // a miss copies nothing.
                let found = inner.documents.get(&key).cloned();
                *write
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = found;
            })
            .await
            .is_ok();
        if !installed {
            return None;
        }
        let found = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        found
    }

    /// Cache a committed incarnation loaded by [`Session::snapshot`].
    ///
    /// An entry already present wins: loading a snapshot must never replace a
    /// fresher cached incarnation.
    async fn install_document(&self, document: LoadedDocument) {
        let address_id = document.address_id.clone();
        let _ = self
            .handle
            .maintain(move |inner| {
                if inner.documents.get(&address_id).is_some() {
                    return;
                }
                let now = inner.now();
                inner.documents.insert(document, now);
                inner.arm_document_sweep();
            })
            .await;
    }

    /// Drop the cached incarnation so the next read reloads committed state.
    ///
    /// A held document is never dropped: its cache value *is* the newest state
    /// (storage does not have it yet), so the hold stays authoritative until it
    /// is stored.
    async fn invalidate_document(&self, address_id: String) {
        let _ = self
            .handle
            .maintain(move |inner| {
                if inner
                    .holds
                    .values()
                    .any(|hold| hold.address_id == address_id)
                {
                    return;
                }
                inner.documents.remove(&address_id);
            })
            .await;
    }
}

// ─── Assembly ───────────────────────────────────────────────────────────────

/// Assemble the callback's staged work and hand the batch to the line.
///
/// Assembly runs on the **line**, not here: `line::run_commit` owns
/// [`assemble_with`](crate::durable::session::assemble::assemble_with) and
/// [`adopt`](crate::durable::session::assemble::adopt), so document writes and
/// retirements become observable publications. A facade that assembled too
/// would drain the transaction's staged documents before the line could plan
/// them, which is exactly the bug this split fixes.
///
/// Kept as a thin alias because [`Session::commit_scoped`] still names the step
/// in its callback chain; the line's pass is the one that plans.
async fn assemble_into(
    _tx: &mut Transaction,
    _state: &SessionState<'_>,
) -> Result<(), StorageError> {
    Ok(())
}

// ─── Staging helpers ────────────────────────────────────────────────────────

/// The definition metadata [`stage_create_task`] needs, resolved before the
/// callback (a borrowed `&dyn TaskDefinitionLike` cannot cross into a
/// `'static` commit callback).
pub(crate) struct TaskSnapshot {
    pub(crate) kind: String,
    pub(crate) version: i64,
    pub(crate) checkpoint: Value,
}

/// The committed or candidate record for `id`, candidate first.
///
/// Reading the committed table here is an internal transaction read (upstream
/// `#currentTask`), not a caller table read, so it does not consult
/// [`Transaction::check_table_read`].
async fn current_task(
    tx: &Transaction,
    state: &SessionState<'_>,
    id: TaskId,
) -> Result<Option<TaskRecord>, StorageError> {
    if let Some(staged) = tx.staged_task(id) {
        return Ok(Some(staged.clone()));
    }
    let context = tx.context().clone();
    let storage = state.storage.storage();
    state.storage.read(&context, storage.task(id)).await
}

/// Inherit trace carriers from the submissions that start a task.
///
/// Upstream `Transaction.createTask` (`transaction.ts:393-405`) uses the first
/// submission's carrier as the task's `trace` and every later submission's
/// carrier as an immutable `links` entry. The first id therefore wins even
/// when its carrier is absent (an absent first carrier means "root"); later
/// ids never replace it.
///
/// A submission created earlier in the same transaction is visible only through
/// [`Transaction::staged_submissions`]; a committed one is read from storage.
async fn inherited_submission_trace(
    tx: &Transaction,
    state: &SessionState<'_>,
    ids: &[SubmissionId],
) -> Result<(Option<TraceCarrier>, Vec<TraceCarrier>), StorageError> {
    if ids.is_empty() {
        return Ok((None, Vec::new()));
    }
    let context = tx.context().clone();
    let storage = state.storage.storage();
    let mut trace = None;
    let mut links = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let carrier = match tx.staged_submissions().find(|record| record.id == *id) {
            Some(record) => record.trace.clone(),
            None => state
                .storage
                .read(&context, storage.submission(*id))
                .await?
                .and_then(|record| record.trace),
        };
        if index == 0 {
            trace = carrier;
        } else if let Some(carrier) = carrier {
            links.push(carrier);
        }
    }
    Ok((trace, links))
}

/// Require that a conversation this transaction did not create exists.
async fn require_conversation(
    tx: &Transaction,
    state: &SessionState<'_>,
    id: ConversationId,
) -> Result<(), StorageError> {
    if tx.creates_conversation(id) {
        return Ok(());
    }
    let context = tx.context().clone();
    let storage = state.storage.storage();
    if state
        .storage
        .read(&context, storage.conversation(id))
        .await?
        .is_none()
    {
        return Err(StorageError::invalid(format!(
            "Conversation {id} does not exist"
        )));
    }
    Ok(())
}

/// Mint one raw id inside a commit (the guard never exempts `mint_id`).
async fn mint_id(state: &SessionState<'_>) -> Result<i64, StorageError> {
    let storage = state.storage.storage();
    state.storage.write(storage.mint_id()).await
}

/// Whether this transaction already scheduled a retirement of `id`.
/// Whether this transaction already retires `id`, by draft or by retire-only
/// plan.
///
/// A direct `DocumentRetire` write is no longer produced by the facade, but the
/// predicate still recognises one so a caller that stages the batch entry
/// itself is not silently double-retired.
fn retires_in_transaction(tx: &Transaction, id: DocumentId) -> bool {
    tx.retires_record(id)
}

/// `append_entry` inside one commit.
async fn stage_append_entry(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    conversation: ConversationId,
    draft: EntryDraft,
) -> Result<EntryRecord, StorageError> {
    require_conversation(tx, state, conversation).await?;
    let id = EntryId::from_raw(mint_id(state).await?);
    let head = match draft.head {
        Some(HeadTarget::SelfEntry) => Some(id),
        Some(HeadTarget::Entry(entry)) => Some(entry),
        None => None,
    };
    let record = EntryRecord {
        id,
        conversation_id: conversation,
        kind: draft.kind,
        model: draft.model,
        data: draft.data,
        head,
        edits: draft.edits,
        by_task_id: tx.scope().task_id,
    };
    tx.push_write(StorageWrite::Entry {
        value: record.clone(),
    });
    Ok(record)
}

/// `create_conversation` inside one commit.
async fn stage_create_conversation(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    ownership: ConversationOwnership,
    parent: Option<(ConversationId, EntryId)>,
    hook: Option<Arc<ConversationCreatedHook>>,
) -> Result<ConversationRecord, StorageError> {
    let id = ConversationId::from_raw(mint_id(state).await?);
    let owner = match ownership {
        ConversationOwnership::Ownerless => None,
        ConversationOwnership::Task(task_id) => {
            let Some(task) = current_task(tx, state, task_id).await? else {
                return Err(StorageError::invalid(format!(
                    "Conversation owner task {task_id} does not exist"
                )));
            };
            Some(ConversationOwner {
                conversation_id: task.conversation_id,
                task_id,
            })
        }
    };
    let record = ConversationRecord {
        id,
        parent: parent.map(|(conversation_id, at)| ConversationParent {
            conversation_id,
            at,
        }),
        owner,
    };
    // The fork scan reads storage, so it runs before any staging, and every
    // copy is registered on the transaction before the conversation write.
    let copies = match parent {
        Some((parent_id, at)) => {
            prepare_fork_document_copies(state.storage, parent_id, at, id, tx.context()).await?
        }
        None => Vec::new(),
    };
    for copy in &copies {
        tx.stage_fork_source_document(copy.source.id);
        // Assembly mints the child incarnation id (a copy is a creation, so it
        // takes the same minting path as a draft) and adoption publishes the
        // resulting `DocumentCommitChange::Copy`.
        tx.stage_plan(copy_plan(copy, DocumentId::from_raw(0)));
    }
    tx.stage_conversation(record.clone(), parent.map(|(parent_id, _)| parent_id));
    if let Some(hook) = hook {
        hook(&mut *tx, &record).await?;
    }
    Ok(record)
}

/// `create_task` inside one commit.
pub(crate) async fn stage_create_task(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    snapshot: TaskSnapshot,
    input: Value,
    options: TaskOptions,
) -> Result<TaskId, StorageError> {
    let mut owner = None;
    if let TaskOwnership::Task(owner_id) = options.ownership {
        let Some(task) = current_task(tx, state, owner_id).await? else {
            return Err(StorageError::invalid(format!(
                "Task owner {owner_id} does not exist"
            )));
        };
        if options.background {
            return Err(StorageError::invalid("A child task cannot be background"));
        }
        if options
            .conversation_id
            .is_some_and(|conversation| conversation != task.conversation_id)
        {
            return Err(StorageError::invalid(format!(
                "A child task lives in its owner's conversation {}",
                task.conversation_id
            )));
        }
        owner = Some(task);
    }
    let conversation_id = owner
        .as_ref()
        .map(|task| task.conversation_id)
        .or(options.conversation_id)
        .or(tx.scope().conversation_id)
        .ok_or_else(|| StorageError::invalid("Task creation requires options.conversation_id"))?;
    require_conversation(tx, state, conversation_id).await?;
    // Upstream: when `submissionIds` is supplied, those records determine both
    // `trace` and `links` and override an explicit `trace` option. Without
    // submission ids, an explicit `trace` (or none) is used and `links` stays
    // empty.
    let (trace, links) = if options.submission_ids.is_empty() {
        (options.trace, Vec::new())
    } else {
        inherited_submission_trace(tx, state, &options.submission_ids).await?
    };
    let id = TaskId::from_raw(mint_id(state).await?);
    let record = TaskRecord {
        id,
        conversation_id,
        kind: snapshot.kind,
        version: snapshot.version,
        input,
        owner: owner.map(|task| task.id),
        background: options.background,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: options.abandon_on_restart.then_some(true),
        started_at: None,
        ended_at: None,
        restarts: None,
        state: TaskState::Pending {
            checkpoint: snapshot.checkpoint,
        },
        memos: None,
        trace,
        links,
    };
    tx.stage_task(record, true);
    Ok(id)
}

/// `set_task` inside one commit.
async fn stage_set_task(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    mut record: TaskRecord,
) -> Result<(), StorageError> {
    // Candidate first, committed record second — the same checks upstream
    // `setTask` makes, plus the assembly-time replacement validation.
    let candidate = tx.staged_task(record.id).cloned();
    if let Some(candidate) = &candidate {
        if candidate.state.is_terminal() {
            return Err(StorageError::invalid(format!(
                "Task {} already has a terminal candidate",
                record.id
            )));
        }
        if candidate.conversation_id != record.conversation_id {
            return Err(StorageError::invalid(format!(
                "Task {} cannot change conversations",
                record.id
            )));
        }
    } else {
        let Some(committed) = current_task(tx, state, record.id).await? else {
            return Err(StorageError::invalid(format!(
                "Task {} does not exist",
                record.id
            )));
        };
        if committed.state.is_terminal() {
            return Err(StorageError::invalid(format!(
                "Task {} is already terminal",
                record.id
            )));
        }
        if committed.conversation_id != record.conversation_id {
            return Err(StorageError::invalid(format!(
                "Task {} cannot change conversations",
                record.id
            )));
        }
    }
    let now = state.now();
    let running = matches!(record.state, TaskState::Running { .. });
    let terminal = record.state.is_terminal();
    let previous = candidate.as_ref();
    record.started_at = previous
        .and_then(|task| task.started_at)
        .or(record.started_at)
        .or_else(|| running.then_some(now));
    record.ended_at = previous
        .and_then(|task| task.ended_at)
        .or(record.ended_at)
        .or_else(|| terminal.then_some(now));
    tx.stage_task(record, false);
    Ok(())
}

/// `create_submission` inside one commit.
async fn stage_create_submission(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    create: SubmissionCreate,
) -> Result<SubmissionRecord, StorageError> {
    require_conversation(tx, state, create.conversation_id).await?;
    let id = SubmissionId::from_raw(mint_id(state).await?);
    let record = SubmissionRecord {
        id,
        conversation_id: create.conversation_id,
        request_id: create.request_id,
        submission_type: create.submission_type,
        status: create.status,
        entry: create.entry,
        answer: create.answer,
        reason: create.reason,
        detail: create.detail,
        trace: create.trace,
    };
    tx.stage_submission(record.clone());
    Ok(record)
}

/// `doc` inside one commit.
async fn stage_document_edit<T, F>(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    resolved: ResolvedAddress,
    definition: AnyDocDefinition,
    cached: Option<LoadedDocument>,
    edit: F,
) -> Result<T, StorageError>
where
    F: FnOnce(&mut JsonObject) -> T + Send,
{
    let ResolvedAddress {
        address,
        id: address_id,
    } = resolved;
    // Read-your-writes: a live draft this transaction staged is the value.
    if let Some(staged) = tx
        .latest_document(&address_id)
        .filter(|staged| !staged.retire)
    {
        let mut value = staged.value.clone();
        let edited = edit(&mut value);
        if let Some(staged) = tx.latest_document_mut(&address_id) {
            staged.value = value;
        }
        return Ok(edited);
    }
    // A staged retirement or a retirement already scheduled in this batch
    // means the next incarnation starts fresh (upstream `skipLoad`).
    let retiring = tx
        .latest_document(&address_id)
        .is_some_and(|staged| staged.retire);

    let semantics = definition.common.semantics;
    let identity = DocDefinition {
        kind: definition.common.kind.clone(),
        version: definition.common.version,
        semantics,
    };
    let mut loaded = None;
    let mut created = false;
    let mut value;
    if let Some(cached) = cached.filter(|cached| {
        !retiring
            && cached.record.retired_at.is_none()
            && !retires_in_transaction(tx, cached.record.id)
    }) {
        // Validates scope and version, then migrates to the definition's shape.
        value = definition.materialize_value(
            &cached.record,
            cached.value_version,
            cached.value.clone(),
        )?;
        loaded = Some(cached);
    } else {
        let context = tx.context().clone();
        let storage = state.storage.storage();
        let found = if retiring {
            None
        } else {
            state
                .storage
                .read(
                    &context,
                    storage.find_document(address.clone(), DocumentPoint::Current),
                )
                .await?
                .filter(|record| !retires_in_transaction(tx, record.id))
        };
        match found {
            Some(record) => {
                let Some(stored) = state
                    .storage
                    .read(
                        &context,
                        storage.document(record.id, DocumentPoint::Current),
                    )
                    .await?
                else {
                    return Err(StorageError::invalid(format!(
                        "Document {} cannot be read",
                        record.id
                    )));
                };
                // `materialize_value` consumes its input, so the committed map
                // moves into it and only the draft/baseline split copies
                // anything: the baseline is the one clone, the draft is the
                // value the caller's edit mutates.
                value = definition.materialize_value(&record, stored.version, stored.value)?;
                loaded = Some(LoadedDocument {
                    address_id: address_id.clone(),
                    record,
                    stored_version: stored.version,
                    value_version: definition.common.version,
                    deltas_since_base: stored.deltas_since_base,
                    value: value.clone(),
                });
            }
            None => {
                // A fresh incarnation needs a live owner (spec §5.5).
                match address.scope {
                    DocumentScope::Conversation { conversation_id } => {
                        require_conversation(tx, state, conversation_id).await?;
                    }
                    DocumentScope::Task { task_id } => {
                        let Some(task) = current_task(tx, state, task_id).await? else {
                            return Err(StorageError::invalid(format!(
                                "Task {task_id} does not exist"
                            )));
                        };
                        if task.state.is_terminal() {
                            return Err(StorageError::invalid(format!(
                                "Task {task_id} is terminal"
                            )));
                        }
                    }
                    DocumentScope::Session => {}
                }
                value = definition.initial_value();
                created = true;
            }
        }
    }
    let edited = edit(&mut value);
    // Assembly plans this: a version change becomes a base, a same-version
    // change a delta, an unchanged same-version draft no write at all.
    tx.stage_document(StagedDocument {
        address,
        address_id,
        definition: Some(identity),
        family: None,
        value,
        created,
        loaded,
        retire: false,
    });
    Ok(edited)
}

/// `retire_doc` inside one commit.
async fn stage_retire_document(
    tx: &mut Transaction,
    state: &SessionState<'_>,
    resolved: ResolvedAddress,
) -> Result<(), StorageError> {
    if tx.latest_document(&resolved.id).is_some() {
        // A draft staged by this transaction retires with it; assembly emits
        // the retirement write for the plan.
        tx.retire_document(&resolved.id);
        return Ok(());
    }
    let context = tx.context().clone();
    let storage = state.storage.storage();
    let Some(record) = state
        .storage
        .read(
            &context,
            storage.find_document(resolved.address, DocumentPoint::Current),
        )
        .await?
    else {
        // Nothing is live at the address.
        return Ok(());
    };
    if retires_in_transaction(tx, record.id) {
        return Ok(());
    }
    // Retirement bypasses `plan_document`, so the fork-source rule is applied
    // here too (assembly checks plans only).
    if tx.fork_source_documents().contains(&record.id) {
        return Err(StorageError::invalid(format!(
            "Cannot change fork source document {} in the fork transaction",
            record.id
        )));
    }
    // Stage a retire-only plan (upstream `retire-only`, `transaction.ts:683`):
    // assembly plans it and adoption publishes the terminal frame. Writing the
    // `document.retire` batch entry directly would retire the row but leave
    // observers — watchers, views and the TUI — unaware.
    tx.stage_retirement(record);
    Ok(())
}
