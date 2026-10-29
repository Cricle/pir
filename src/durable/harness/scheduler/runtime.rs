//! The invocation runtime a phase handler uses.
//!
//! Port of the `#runtime(invocation, phase)` closure in upstream
//! `harness/scheduler.ts` (`:2049-2465`), narrowed to the surface this lane's
//! harness has: commits, memos, sleeps, timers, document reads and watches,
//! task reads, owned tasks, outcomes, conversations, owned aborts, the clock
//! and reporting. The agent, model, environment and entry/context helpers
//! belong to the harness lanes that own those types.
//!
//! # The gate (spec §5.1, §5.4)
//!
//! Every durable operation goes through [`TaskRuntime::gated`]: one commit on
//! the session line that re-reads the task and checks, in order,
//!
//! 1. the invocation has not ended ([`TaskRuntimeError::Ended`]);
//! 2. the session is not closing ([`TaskRuntimeError::Closed`]);
//! 3. the task is still `running` ([`TaskRuntimeError::Terminal`] or
//!    [`TaskRuntimeError::NotRunning`]);
//! 4. a run-mode task carries no durable abort mark
//!    ([`TaskRuntimeError::AbortMarked`]).
//!
//! Only then is the caller's change invoked and staged. A rejection therefore
//! lands nothing: the line aborts the transaction, and the assembly never sees
//! it.
//!
//! `memo(name, candidate)` reads the winner and stages the candidate in the
//! same commit, so concurrent candidates return the same durable winner.
//!
//! # After the end
//!
//! Every operation rejects once the invocation ended, even while the session
//! stays open (spec §5.4). That is stronger than a check on the task record:
//! detached leftovers of a handler can no longer write anything.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::Notify;

use crate::ai::types::AbortSignal;
use crate::durable::context::Context;
use crate::durable::documents::ResolvedAddress;
use crate::durable::errors::{ReportFrom, ReportInfo, ReportTask, StorageError};
use crate::durable::harness::scheduler::{
    checkpoint_of, memo_of, InvocationMode, NextTaskState, SchedulerInner,
};
use crate::durable::harness::task::RunningTask;
use crate::durable::harness::telemetry::SpanHandle;
use crate::durable::harness::tool::ToolHost;
use crate::durable::ids::{ConversationId, DocumentId, TaskId};
use crate::durable::session::observation::{CommittedWatch, WatchEnd};
use crate::durable::session::publication::{
    CommitChange as SessionCommitChange, CommitPublication,
};
use crate::durable::session::transaction::TransactionScope;
use crate::durable::session::Transaction;
use crate::durable::timers::TimerHandle;
use crate::durable::types::{
    DocumentCommitChange, DocumentPoint, JsonObject, StoredDocument, TaskOutcome, TaskRecord,
    TaskState,
};

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Why a runtime operation was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskRuntimeError {
    /// The invocation has ended.
    #[error("Task {task} invocation has ended")]
    Ended {
        /// The task.
        task: TaskId,
    },
    /// The session is closing.
    #[error("Session is closing")]
    Closed,
    /// The task no longer exists (it is terminal).
    #[error("Task {task} is terminal")]
    Terminal {
        /// The task.
        task: TaskId,
    },
    /// The task left `running` without becoming terminal.
    #[error("Task {task} is {status}")]
    NotRunning {
        /// The task.
        task: TaskId,
        /// The state's status.
        status: String,
    },
    /// The task carries a durable abort mark.
    #[error("Task {task} has a durable abort mark")]
    AbortMarked {
        /// The task.
        task: TaskId,
    },
    /// A contract violation detected before or during staging.
    #[error("{0}")]
    Invalid(String),
    /// The storage failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl TaskRuntimeError {
    /// The plain error the line reports for this rejection.
    fn as_storage(&self) -> StorageError {
        StorageError::Request(crate::durable::errors::StorageRequestError::new(
            self.to_string(),
        ))
    }
}

// ─── Runtime ────────────────────────────────────────────────────────────────

/// The boxed future a [`TaskRuntime::commit`] callback returns.
pub type CommitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<NextTaskState>, TaskRuntimeError>> + Send + 'a>>;

/// The handle a phase handler receives.
///
/// Cheap to clone: it holds an `Arc` of the scheduler and of its invocation, so
/// a handler can move a clone into a detached task. Every clone observes the
/// same invocation end.
#[derive(Clone)]
pub struct TaskRuntime {
    pub(super) inner: Arc<SchedulerInner>,
    pub(super) invocation: Arc<super::Invocation>,
    pub(super) task_id: TaskId,
    pub(super) conversation_id: ConversationId,
    pub(super) catalogue: super::TaskCatalogue,
}

impl std::fmt::Debug for TaskRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskRuntime")
            .field("task_id", &self.task_id)
            .field("conversation_id", &self.conversation_id)
            .finish_non_exhaustive()
    }
}

impl TaskRuntime {
    pub(super) fn new(
        inner: Arc<SchedulerInner>,
        invocation: Arc<super::Invocation>,
        catalogue: super::TaskCatalogue,
    ) -> Self {
        Self {
            task_id: invocation.id,
            conversation_id: invocation.conversation_id,
            inner,
            invocation,
            catalogue,
        }
    }

    /// The task identifier.
    #[must_use]
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// The conversation that owns the task.
    #[must_use]
    pub fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    /// The invocation signal; aborts when the invocation ends.
    #[must_use]
    pub fn signal(&self) -> Arc<AbortSignal> {
        self.invocation.signal.clone()
    }

    /// Resolve when the invocation ends.
    pub async fn cancelled(&self) {
        self.invocation.signal.cancelled().await;
    }

    /// The catalogue snapshot this phase resolves against.
    #[must_use]
    pub fn registry(&self) -> super::TaskCatalogue {
        self.catalogue.clone()
    }

    /// The harness clock.
    pub fn now(&self) -> Result<i64, TaskRuntimeError> {
        self.check_live()?;
        Ok(self.inner.now())
    }

    /// Forward a non-fatal failure to the report sink.
    ///
    /// The report is a `task_error` about the task's own code, naming the
    /// extension that installed the definition it runs (upstream
    /// `runtime.report(error)`).
    pub fn report(&self, message: impl Into<String>) -> Result<(), TaskRuntimeError> {
        self.report_from(message, None)
    }

    /// Forward a non-fatal failure to the report sink, naming whose code
    /// failed (upstream `runtime.report(error, from)`).
    ///
    /// Without `from` this is a `task_error` about the task's own code; an
    /// extension names the code the task called, a host callback the host
    /// operation it called.
    pub fn report_from(
        &self,
        message: impl Into<String>,
        from: Option<ReportFrom>,
    ) -> Result<(), TaskRuntimeError> {
        self.check_live()?;
        self.inner.report_with_span(
            self.invocation.phase_span(),
            message,
            ReportInfo::task_report(self.report_task(), self.invocation.extension.clone(), from),
        );
        Ok(())
    }

    // ─── Commit ─────────────────────────────────────────────────────────────

    /// Commit a change to the task's own state, gated on the invocation and the
    /// durable record (spec §5.1).
    ///
    /// The callback may stage any transaction writes. It returns the task's
    /// next state, or `None` to leave the state unchanged. A `terminal` state
    /// becomes `completing` while ordinary owned work below is live, judged on
    /// the commit's candidates; a `waiting` state is validated first; a
    /// `sleeping` state must name a valid time. A state identical to the
    /// current one keeps the restart count; any other is progress and clears
    /// it.
    pub async fn commit<F>(&self, change: F, context: Context) -> Result<(), TaskRuntimeError>
    where
        F: for<'a> FnOnce(&'a mut Transaction, &'a RunningTask) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.gated(
            move |tx, record| {
                Box::pin(async move {
                    let running = RunningTask::from_record(record).ok_or_else(|| {
                        TaskRuntimeError::Invalid("task is not running".to_owned())
                    })?;
                    let next = change(tx, &running).await?;
                    Ok(((), next))
                })
            },
            context,
        )
        .await
        .map(|_| ())
    }

    /// Commit `sleeping` directly (spec §5.6).
    ///
    /// Resolves once the sleep is stored, not when the time comes. An abort
    /// handler cannot sleep.
    pub async fn sleep_until(
        &self,
        until: i64,
        checkpoint: Value,
        context: Context,
    ) -> Result<(), TaskRuntimeError> {
        self.gated(
            move |_tx, _record| {
                Box::pin(
                    async move { Ok(((), Some(NextTaskState::Sleeping { checkpoint, until }))) },
                )
            },
            context,
        )
        .await
        .map(|_| ())
    }

    /// Read one memo from the committed record; `None` when absent.
    pub async fn memo(
        &self,
        name: &str,
        _context: Context,
    ) -> Result<Option<Value>, TaskRuntimeError> {
        self.check_live()?;
        if let Some(record) = self.inner.live_record(self.task_id) {
            return Ok(memo_of(Some(&record), name));
        }
        Ok(memo_of(
            self.inner.read_task(self.task_id).await?.as_ref(),
            name,
        ))
    }

    /// First-writer-wins memo insertion in one commit.
    ///
    /// A concurrent candidate returns the durable winner, not its own value.
    pub async fn memo_candidate(
        &self,
        name: String,
        candidate: Value,
        context: Context,
    ) -> Result<Value, TaskRuntimeError> {
        self.gated(
            move |tx, record| {
                Box::pin(async move {
                    if let Some(winner) = memo_of(Some(record), &name) {
                        return Ok((winner, None));
                    }
                    let mut next = record.clone();
                    next.memos
                        .get_or_insert_with(Default::default)
                        .insert(name.clone(), candidate.clone());
                    tx.stage_task(next, false);
                    Ok((candidate, None))
                })
            },
            context,
        )
        .await
    }

    /// Arm a timer for pacing work inside a phase (spec §5.6).
    ///
    /// The invocation's end disarms it; a call after the end rejects.
    pub fn set_timer(
        &self,
        callback: impl Fn() + Send + Sync + 'static,
        delay_ms: i64,
    ) -> Result<Arc<TimerHandle>, TaskRuntimeError> {
        self.check_live()?;
        let invocation = self.invocation.clone();
        let timer = Arc::new(self.inner.timers.foreground(
            Arc::new(move || {
                if !invocation.ended() {
                    callback();
                }
            }),
            delay_ms,
        ));
        self.invocation.add_timer(timer.clone());
        Ok(timer)
    }

    // ─── Reads ──────────────────────────────────────────────────────────────

    /// One committed task record.
    pub async fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>, TaskRuntimeError> {
        self.check_live()?;
        Ok(self.inner.read_task(id).await?)
    }

    /// The live tasks this task owns directly, in id order.
    pub async fn owned_tasks(&self) -> Result<Vec<TaskRecord>, TaskRuntimeError> {
        self.check_live()?;
        Ok(self.inner.with_state(|state| {
            state
                .live
                .values()
                .filter(|record| record.owner == Some(self.task_id))
                .cloned()
                .collect()
        }))
    }

    /// The outcomes of terminal tasks, in order; rejects when one is not
    /// terminal (spec §5.5). Used after a wait.
    pub async fn outcomes(&self, ids: &[TaskId]) -> Result<Vec<TaskOutcome>, TaskRuntimeError> {
        self.check_live()?;
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(record) = self.inner.read_task(*id).await? else {
                return Err(TaskRuntimeError::Invalid(format!(
                    "Task {id} does not exist"
                )));
            };
            match record.state {
                TaskState::Terminal { outcome } => outcomes.push(outcome),
                _ => {
                    return Err(TaskRuntimeError::Invalid(format!(
                        "Task {id} is not terminal"
                    )))
                }
            }
        }
        Ok(outcomes)
    }

    /// A handle of an existing conversation.
    pub async fn conversation(
        &self,
        id: ConversationId,
    ) -> Result<Option<super::ConversationHandle>, TaskRuntimeError> {
        self.check_live()?;
        if self.inner.read_conversation(id).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(super::ConversationHandle {
            inner: self.inner.clone(),
            id,
        }))
    }

    /// Create a child task of this task, inside a commit, and return its id.
    ///
    /// The child lives in this task's conversation and is owned by this task
    /// (`spec §5.5`: a child task is never background). The session's assembly
    /// rejects the creation when this task is `completing`, terminal, or
    /// abort-marked — the durable rule that "new owned work needs a live
    /// owner" — so an abort handler cannot create a child this way.
    pub async fn create_task(
        &self,
        definition: &Arc<dyn crate::durable::harness::task::ErasedTask>,
        input: Value,
        context: Context,
    ) -> Result<TaskId, TaskRuntimeError> {
        self.check_live()?;
        let adapter = ErasedDefinition {
            kind: definition.name().to_owned(),
            version: definition.version(),
            initial: definition.as_definition().initial,
        };
        // Thread the parent carrier into the child's creation record so its
        // phase spans descend from the same chain. The invocation carrier is
        // the task's stored `trace` (first submission or inherited owner
        // carrier); a context that already holds one is the more immediate
        // parent. Without a carrier this is a `None` move, no allocation.
        let trace = self
            .invocation
            .trace_carrier()
            .cloned()
            .or_else(|| context.trace_carrier().cloned());
        let id = self
            .inner
            .session
            .create_task(
                &adapter,
                input,
                crate::durable::session::transaction::TaskOptions {
                    conversation_id: Some(self.conversation_id),
                    ownership: crate::durable::session::transaction::TaskOwnership::Task(
                        self.task_id,
                    ),
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                    trace,
                },
                context,
            )
            .await?;
        Ok(id)
    }

    /// Abort a task this task owns, resolving once it is terminal.
    ///
    /// Rejects for a task another task owns. The owner's own cleanup keeps a
    /// `restart` mark.
    pub async fn abort_owned(&self, id: TaskId, _context: Context) -> Result<(), TaskRuntimeError> {
        self.check_live()?;
        let Some(record) = self.inner.read_task(id).await? else {
            return Err(TaskRuntimeError::Invalid(format!(
                "Task {id} does not exist"
            )));
        };
        if record.owner != Some(self.task_id) {
            return Err(TaskRuntimeError::Invalid(format!(
                "Task {id} is not owned by task {}",
                self.task_id
            )));
        }
        if record.state.is_terminal() {
            return Ok(());
        }
        self.inner.abort(id, true).await?;
        let _ = self.inner.wait_for_task(id).await?;
        Ok(())
    }

    // ─── Documents ──────────────────────────────────────────────────────────

    /// The committed value at `resolved`, if the document exists.
    pub async fn snapshot(
        &self,
        resolved: &ResolvedAddress,
    ) -> Result<Option<JsonObject>, TaskRuntimeError> {
        self.check_live()?;
        Ok(self
            .inner
            .session
            .snapshot(resolved, &Context::background())
            .await?)
    }

    /// The committed incarnation and value at `resolved`, if it exists.
    pub async fn document(
        &self,
        resolved: &ResolvedAddress,
    ) -> Result<Option<StoredDocument>, TaskRuntimeError> {
        self.check_live()?;
        let address = resolved.address.clone();
        let inner = self.inner.clone();
        Ok(inner
            .read_on_line(move |state| {
                Box::pin(async move {
                    let context = Context::background();
                    let storage = state.storage.storage();
                    let Some(record) = state
                        .storage
                        .read(
                            &context,
                            storage.find_document(address, DocumentPoint::Current),
                        )
                        .await?
                    else {
                        return Ok(None);
                    };
                    state
                        .storage
                        .read(
                            &context,
                            storage.document(record.id, DocumentPoint::Current),
                        )
                        .await
                })
            })
            .await?)
    }

    /// Watch one document incarnation.
    ///
    /// The listener receives every committed revision of the current
    /// incarnation (and its retirement) until it returns `false`, the watch is
    /// stopped, or the invocation ends. The watch is tracked on the invocation,
    /// so the invocation's end stops it even when the caller forgets to.
    pub async fn watch_doc<F>(
        &self,
        resolved: &ResolvedAddress,
        listener: F,
    ) -> Result<DocumentWatch, TaskRuntimeError>
    where
        F: Fn(&JsonObject) -> bool + Send + Sync + 'static,
    {
        self.check_live()?;
        let address = resolved.address.clone();
        let session = self.inner.session.clone();
        let report = self.inner.report.clone();
        // One line job reads the current committed revision and registers the
        // commit subscription at the same point on the line. The line runs
        // jobs one at a time, so no commit can land between the read and the
        // registration: a commit published before this job is already in the
        // value read here, and every commit published after it reaches the
        // listener.
        let watch = self
            .inner
            .read_on_line(move |state| {
                Box::pin(async move {
                    let context = Context::background();
                    let storage = state.storage.storage();
                    let Some(record) = state
                        .storage
                        .read(
                            &context,
                            storage.find_document(address, DocumentPoint::Current),
                        )
                        .await?
                    else {
                        return Ok(None);
                    };
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
                    let incarnation = record.id;
                    let initial = Some(stored.value);
                    let watch = CommittedWatch::new(initial.clone(), stored.version);
                    // A watch starts with the revision it was built from:
                    // queue it as the first frame, versioned like the value it
                    // carries, so `start` delivers the current revision before
                    // any later commit.
                    watch
                        .core()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .advance(initial, Vec::new(), stored.version, context.clone());
                    // The commit listener only queues and wakes; the host
                    // listener runs on the delivery task, never on this line.
                    let delivery = WatchDelivery::new(watch.clone(), report.clone());
                    let advance = delivery.clone();
                    let subscription =
                        session.subscribe_commits(Arc::new(move |publication, context| {
                            advance_document_watch(&advance, publication, incarnation, context);
                        }))?;
                    Ok(Some((watch, delivery, subscription)))
                })
            })
            .await?;
        let Some((watch, delivery, subscription)) = watch else {
            return Err(TaskRuntimeError::Invalid(format!(
                "Document {} does not exist",
                resolved.id
            )));
        };
        let listener = Arc::new(listener);
        // `start` installs the listener and drains the initial frame
        // synchronously; only then may the delivery task run, so its drain can
        // never overlap that first pass.
        watch.start(move |frame| match &frame.value {
            Some(value) => listener(value),
            None => false,
        });
        delivery.start();
        let watch = DocumentWatch {
            watch,
            delivery,
            session: self.inner.session.clone(),
            subscription,
            stopped: Arc::new(AtomicBool::new(false)),
        };
        self.invocation.add_watch(watch.clone());
        Ok(watch)
    }

    // ─── Gated commit ───────────────────────────────────────────────────────

    /// Run one gated commit. The change receives the freshly read record.
    async fn gated<T, F>(&self, change: F, context: Context) -> Result<T, TaskRuntimeError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a TaskRecord) -> ResultFuture<'a, T>
            + Send
            + 'static,
    {
        self.check_live()?;
        let inner = self.inner.clone();
        let invocation = self.invocation.clone();
        let task_id = self.task_id;
        let scope = TransactionScope {
            conversation_id: Some(self.conversation_id),
            task_id: Some(self.task_id),
        };
        let typed: Arc<Mutex<Option<TaskRuntimeError>>> = Arc::new(Mutex::new(None));
        let typed_out = typed.clone();
        // The event's span: the handler context carries the phase span; the
        // invocation's handle covers commits that pass a background context.
        let telemetry_enabled = self.inner.telemetry.enabled();
        let phase_span = if telemetry_enabled {
            context
                .span_handle()
                .filter(|span| !span.is_none())
                .or_else(|| self.invocation.phase_span())
                .or_else(|| self.inner.telemetry.extract(context.trace_carrier()))
        } else {
            None
        };
        // A checkpoint event is recorded only when the commit lands, so the
        // phase is captured inside. The cell exists only with a sink.
        let changed_phase: Option<Arc<Mutex<Option<String>>>> =
            telemetry_enabled.then(|| Arc::new(Mutex::new(None)));
        let changed_phase_out = changed_phase.clone();
        let outcome: Result<T, StorageError> = self
            .inner
            .session
            .commit_scoped(scope, context, move |tx, _state| {
                let inner = inner.clone();
                let invocation = invocation.clone();
                let typed = typed_out.clone();
                let changed_phase_out = changed_phase_out.clone();
                Box::pin(async move {
                    // 1. The invocation has not ended.
                    if invocation.ended() {
                        let error = TaskRuntimeError::Ended { task: task_id };
                        *typed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.clone());
                        return Err(error.as_storage());
                    }
                    // 2. The session is not closing.
                    if inner.is_closing()
                        || inner.session.handle().is_closing()
                        || inner.session.handle().is_failed()
                    {
                        let error = TaskRuntimeError::Closed;
                        *typed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.clone());
                        return Err(error.as_storage());
                    }
                    // 3. The task is still `running`.
                    let Some(current) = inner.live_record(task_id) else {
                        let error = TaskRuntimeError::Terminal { task: task_id };
                        *typed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.clone());
                        return Err(error.as_storage());
                    };
                    if !matches!(current.state, TaskState::Running { .. }) {
                        let error = TaskRuntimeError::NotRunning {
                            task: task_id,
                            status: current.state.status().to_string(),
                        };
                        *typed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.clone());
                        return Err(error.as_storage());
                    }
                    // 4. A run-mode task carries no durable abort mark.
                    if invocation.mode == InvocationMode::Run && current.abort_requested {
                        let error = TaskRuntimeError::AbortMarked { task: task_id };
                        *typed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.clone());
                        return Err(error.as_storage());
                    }
                    let (value, next) = match change(tx, &current).await {
                        Ok(value) => value,
                        Err(error) => {
                            *typed
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(error.clone());
                            return Err(error.as_storage());
                        }
                    };
                    if let Some(next) = next {
                        // `pi.durable.checkpoint` marks a running phase change:
                        // the invocation continues, so only `Running` counts.
                        let changed = changed_phase_out.as_ref().and_then(|_| match &next {
                            NextTaskState::Running { checkpoint } => {
                                let phase = checkpoint.get("phase").and_then(Value::as_str);
                                let current_phase = checkpoint_of(&current.state)
                                    .get("phase")
                                    .and_then(Value::as_str);
                                if phase != current_phase {
                                    phase.map(str::to_owned)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        });
                        if let Err(error) = inner
                            .commit_state(tx, _state, &invocation, &current, next)
                            .await
                        {
                            *typed
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(error.clone());
                            return Err(error.as_storage());
                        }
                        if let (Some(slot), Some(phase)) = (&changed_phase_out, changed) {
                            *slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(phase);
                        }
                    }
                    Ok(Box::new(value) as crate::durable::session::line::JobValue)
                })
            })
            .await;
        match outcome {
            Ok(value) => {
                if let Some(changed) = changed_phase {
                    let phase = changed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(phase) = phase {
                        self.inner.telemetry.checkpoint_changed(phase_span, &phase);
                    }
                }
                Ok(value)
            }
            Err(error) => {
                let typed = typed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                Err(typed.unwrap_or(TaskRuntimeError::Storage(error)))
            }
        }
    }

    fn check_live(&self) -> Result<(), TaskRuntimeError> {
        if self.invocation.ended() {
            return Err(TaskRuntimeError::Ended { task: self.task_id });
        }
        Ok(())
    }

    fn report_task(&self) -> ReportTask {
        ReportTask {
            id: self.task_id,
            kind: self.invocation.kind.clone(),
            conversation_id: self.conversation_id,
        }
    }
}

// ─── Tool phase telemetry ───────────────────────────────────────────────────

impl ToolHost for TaskRuntime {
    /// Publish the `tool.name` / `tool.call_id` end attributes of the phase
    /// span this runtime's invocation is running under.
    ///
    /// `settle` calls this from inside the handler, before the scheduler ends
    /// the span; `SchedulerInner::finish_phase` takes the pair. The check is
    /// one branch on an absent sink, so a host without telemetry allocates
    /// nothing.
    fn tool_phase_end(
        &self,
        _span: Option<SpanHandle>,
        _task_id: TaskId,
        tool_name: &str,
        call_id: &str,
    ) {
        if self.inner.telemetry.enabled() {
            self.invocation.set_phase_tool(tool_name, call_id);
        }
    }
}

/// The future a [`TaskRuntime::gated`] change returns.
pub(super) type ResultFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<(T, Option<NextTaskState>), TaskRuntimeError>> + Send + 'a>>;

// ─── Document watches ───────────────────────────────────────────────────────

/// A document watch acquired through the runtime.
///
/// Dropping it does not stop the watch; call [`DocumentWatch::stop`] or let the
/// invocation end stop it.
///
/// # Delivery is off the line
///
/// The commit listener only queues the frame and wakes the delivery task
/// (`Notify`), which drains the queue in one place. No host listener ever runs
/// on the mutation line, so a slow or blocking watcher cannot stall commits and
/// a listener that re-enters the session cannot deadlock against the line that
/// would be delivering to it. Ordering is unchanged: frames queue FIFO under
/// the watch's core lock and only the delivery task pops them.
#[derive(Clone)]
pub struct DocumentWatch {
    watch: CommittedWatch,
    delivery: Arc<WatchDelivery>,
    session: crate::durable::session::facade::Session,
    subscription: u64,
    stopped: Arc<AtomicBool>,
}

/// The delivery half of one runtime watch: the queue, its wake signal and the
/// task that drains it.
///
/// `handle` is `None` only when the watch was acquired outside a tokio
/// runtime; then `wake` drains inline, the only delivery available without a
/// task (and there is no commit line to stall in that configuration).
struct WatchDelivery {
    watch: CommittedWatch,
    signal: Notify,
    handle: Option<tokio::runtime::Handle>,
    /// The scheduler's report sink, used for a panic contained in the delivery
    /// machinery itself (a listener panic already ends the watch `ListenerError`).
    report: crate::durable::harness::scheduler::ReportFn,
}

impl WatchDelivery {
    fn new(
        watch: CommittedWatch,
        report: crate::durable::harness::scheduler::ReportFn,
    ) -> Arc<Self> {
        Arc::new(Self {
            watch,
            signal: Notify::new(),
            handle: tokio::runtime::Handle::try_current().ok(),
            report,
        })
    }

    /// Queue a frame or end signal and wake the delivery task.
    ///
    /// `notify_one` stores a permit while the task is running, so a wake
    /// between a pass finishing and the next `notified().await` is not lost.
    fn wake(&self) {
        match &self.handle {
            Some(_) => self.signal.notify_one(),
            None => self.drain(),
        }
    }

    /// Wake a parked task so it can observe that the watch ended and exit.
    fn notify_stop(&self) {
        if self.handle.is_some() {
            self.signal.notify_one();
        }
    }

    /// Deliver every queued frame, containing a listener panic.
    ///
    /// `CommittedWatch::drain` already contains each frame's listener (a panic
    /// ends the watch `ListenerError`), so this outer `catch_unwind` covers a
    /// panic in the draining machinery itself and keeps the task alive.
    fn drain(&self) {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.watch.drain();
        })) {
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "document watch listener panicked".to_owned());
            (self.report)(&message, ReportInfo::ListenerError);
        }
    }

    /// Run the delivery loop until the watch ends or every holder is gone.
    fn start(self: &Arc<Self>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let weak = Arc::downgrade(self);
        drop(handle.spawn(async move {
            loop {
                let Some(delivery) = weak.upgrade() else {
                    return;
                };
                if delivery.watch.is_stopped() {
                    // One last pass: the terminal retirement frame (or the
                    // frame queued just before a stop) must still be delivered.
                    delivery.drain();
                    return;
                }
                delivery.signal.notified().await;
                let Some(delivery) = weak.upgrade() else {
                    return;
                };
                delivery.drain();
                if delivery.watch.is_stopped() {
                    return;
                }
            }
        }));
    }
}

impl std::fmt::Debug for DocumentWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentWatch")
            .field("stopped", &self.is_stopped())
            .finish_non_exhaustive()
    }
}

impl DocumentWatch {
    /// The newest committed value, or `None` after retirement.
    #[must_use]
    pub fn value(&self) -> Option<JsonObject> {
        self.watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .value()
            .cloned()
    }

    /// Whether the watch was stopped or ended.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire) || self.watch.is_stopped()
    }

    /// Stop the watch. Idempotent.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.session.unsubscribe_commits(self.subscription);
        self.watch.stop(WatchEnd::Stopped);
        // A parked delivery task must observe the end and exit.
        self.delivery.notify_stop();
    }
}

/// Advance a runtime document watch with one committed publication.
///
/// Runs on the line as a commit listener, so it only queues the frame (FIFO
/// under the watch's core lock) and wakes the delivery task; the host listener
/// itself always runs off the line.
fn advance_document_watch(
    delivery: &Arc<WatchDelivery>,
    publication: &CommitPublication,
    incarnation: DocumentId,
    context: &Context,
) {
    let mut advanced = false;
    for change in &publication.changes {
        let SessionCommitChange::Document(DocumentCommitChange::Document {
            record,
            version,
            value,
            ops,
            ..
        }) = change
        else {
            continue;
        };
        if record.id != incarnation {
            continue;
        }
        delivery
            .watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advance(
                value.clone(),
                ops.clone(),
                version.unwrap_or(0),
                context.clone(),
            );
        advanced = true;
    }
    if advanced {
        delivery.wake();
    }
}

/// A task definition presented to `Session::create_task`, which takes metadata
/// (`TaskDefinitionLike`) rather than the erasure the scheduler runs.
struct ErasedDefinition {
    kind: String,
    version: i64,
    initial: crate::durable::harness::task::InitialFn,
}

impl crate::durable::session::facade::TaskDefinitionLike for ErasedDefinition {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn version(&self) -> i64 {
        self.version
    }

    fn initial(&self, input: &Value) -> Value {
        (self.initial)(input)
    }
}
