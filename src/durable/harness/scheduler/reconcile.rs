//! Ownership reconciliation, held outcomes, waits, aborts and wakeups.
//!
//! Ports upstream `harness/scheduler.ts`'s ownership half: `#reconcile`
//! (`:739-800`), `#finalize` (`:802-844`), `#terminate` (`:1938-1951`),
//! `#commitState` (`:1953-1982`), `#validateWait` (`:1984-2030`), `#waitCycle`
//! (`:2030-2075`), `abort` (`:544-580`), `abortConversation` (`:596-624`),
//! `#walkDown`, `#ownedLive`, `#belowCancelled`, `#inScope` and `wake`.
//!
//! # Two commits, on purpose
//!
//! Abort marks caused by an owner are derived in a commit of their *own*, never
//! in the owner's commit: that commit may already have written the tasks
//! involved, and the durable record is the intent, so a crash between them
//! loses nothing (spec §5.4). The same pass finalizes every `completing` task
//! whose ordinary owned work is gone.
//!
//! # The hold
//!
//! A terminal outcome a task commits, or one the scheduler writes (`faulted`,
//! `orphaned`), is stored as `completing` while ordinary owned work is live,
//! judged **on the commit's candidate records** — so work the same commit
//! creates below the task holds it too (spec §5.5, rule 1). [`finalize`] then
//! turns each such record into `terminal` once nothing is live below it, in
//! rounds so a holder of many ending tasks is walked once.
//!
//! [`finalize`]: SchedulerInner::finalize

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use crate::durable::context::Context;
use crate::durable::errors::{ReportInfo, StorageError};
use crate::durable::harness::scheduler::runtime::TaskRuntimeError;
use crate::durable::harness::scheduler::{
    checkpoint_of, failed_outcome, has_owned_live, json_equal, nearest_live_above, node_of,
    owned_live, parent_of, progressed, walk_down, with_abort_mark, with_state, AbortResult,
    BlockedReason, InvocationMode, NextTaskState, Node, Overlay, SchedulerInner,
    TaskInspectionState, Wake, REASON_RESTART,
};
use crate::durable::harness::task::ErasedTask;
use crate::durable::ids::{ConversationId, TaskId};
use crate::durable::session::line::SessionState;
use crate::durable::session::Transaction;
use crate::durable::types::{JoinPolicy, TaskOutcome, TaskRecord, TaskState};

impl SchedulerInner {
    // ─── Reconcile ──────────────────────────────────────────────────────────

    /// One commit that applies what committed records imply (spec §5.4, §5.5).
    ///
    /// Derives abort marks below live owners with cancellation intent, applies
    /// `failFast` marks, and finalizes every `completing` task whose ordinary
    /// owned work is gone. Resolves idle waiters the loaded edges decide.
    pub(super) async fn reconcile(self: &Arc<Self>) {
        let cascade = self.with_state(|state| std::mem::replace(&mut state.cascade_pending, false));
        let checks: Vec<TaskId> = self.with_state(|state| {
            let checks = state.fail_fast_checks.iter().copied().collect();
            state.fail_fast_checks.clear();
            checks
        });
        let inner = self.clone();
        let result = self
            .commit(move |tx, session| {
                let inner = inner.clone();
                Box::pin(async move {
                    if inner.is_closing() {
                        return Ok(());
                    }
                    inner.load_scopes_in(session, cascade).await?;
                    // Collected first, so a request wins over a `restart` mark
                    // of the same pass.
                    let mut marks: BTreeMap<TaskId, (TaskRecord, Option<&'static str>)> =
                        BTreeMap::new();
                    let mut mark = |record: &TaskRecord, reason: Option<&'static str>| {
                        if let Some((_, staged_reason)) = marks.get(&record.id) {
                            if staged_reason.is_none() {
                                return;
                            }
                        }
                        if record.abort_requested
                            && (record.abort_reason.is_none() || reason.is_some())
                        {
                            return;
                        }
                        marks.insert(record.id, (record.clone(), reason));
                    };
                    // A snapshot: `walk_down` runs with the indexes locked, so
                    // its `enter` predicate must not take the same lock again
                    // (`with_state` is not reentrant). The pass never mutates
                    // `intent`, so the snapshot is exact.
                    let intent: BTreeSet<TaskId> = inner.with_state(|state| state.intent.clone());
                    for id in intent.iter().copied() {
                        let Some(owner) = inner.live_record(id) else {
                            continue;
                        };
                        // Only an owner abandoned after a restart, and nothing
                        // else, passes its reason on.
                        let restart = owner.abort_reason.as_deref() == Some(REASON_RESTART)
                            && !failed_outcome(&owner);
                        let enter = |below: TaskId| !intent.contains(&below);
                        let walk = inner.with_state(|state| {
                            walk_down(state, Node::Task(id), None, false, Some(&enter))
                        });
                        for item in walk {
                            let Some(live) = item.live else {
                                continue;
                            };
                            if item.node.background {
                                continue;
                            }
                            mark(&live, if restart { Some(REASON_RESTART) } else { None });
                        }
                    }
                    for id in checks {
                        let Some(waiter) = inner.live_record(id) else {
                            continue;
                        };
                        let TaskState::Waiting { on, .. } = &waiter.state else {
                            continue;
                        };
                        if !any_failed(&inner, session, on).await? {
                            continue;
                        }
                        // Every other live task: the failed one keeps its own
                        // outcome.
                        for member in on {
                            if let Some(record) = inner.live_record(*member) {
                                if !failed_outcome(&record) {
                                    mark(&record, None);
                                }
                            }
                        }
                    }
                    for (record, reason) in marks.values() {
                        inner.stage(tx, with_abort_mark(record, *reason));
                    }
                    inner.finalize(tx).await?;
                    Ok(())
                })
            })
            .await;
        if let Err(error) = result {
            self.fail_scheduling(&error);
        }
        self.schedule_wake();
        self.settle_idle();
    }

    /// Write the terminal record of every `completing` task without live
    /// ordinary owned work (spec §5.5, rule 1).
    ///
    /// Finalizing one can free its owner, so this repeats over the commit's
    /// candidates in rounds until nothing changes.
    pub(super) async fn finalize(&self, tx: &mut Transaction) -> Result<(), StorageError> {
        let mut round: Vec<TaskId> = self.with_state(|indexes| {
            let checks = indexes.finalize_checks.iter().copied().collect();
            indexes.finalize_checks.clear();
            checks
        });
        let mut done: BTreeSet<TaskId> = BTreeSet::new();
        let mut overlay = Overlay::of(tx);
        while !round.is_empty() {
            let mut next: BTreeSet<TaskId> = BTreeSet::new();
            for id in round {
                let record = overlay
                    .tasks
                    .get(&id)
                    .cloned()
                    .or_else(|| self.live_record(id));
                let Some(record) = record else {
                    continue;
                };
                if !matches!(record.state, TaskState::Completing { .. }) || done.contains(&id) {
                    continue;
                }
                let busy = self
                    .with_state(|indexes| has_owned_live(indexes, id, Some(&overlay), Some(&done)));
                if busy {
                    continue;
                }
                let TaskState::Completing { outcome } = record.state.clone() else {
                    continue;
                };
                let terminal = with_state(&record, TaskState::Terminal { outcome });
                self.stage(tx, terminal.clone());
                overlay.tasks.insert(id, terminal);
                done.insert(id);
                let above = self.with_state(|indexes| {
                    nearest_live_above(indexes, node_of(&record), Some(&overlay))
                });
                if let Some(above) = above {
                    if matches!(above.state, TaskState::Completing { .. }) {
                        next.insert(above.id);
                    }
                }
            }
            round = next.into_iter().collect();
        }
        Ok(())
    }

    // ─── Outcomes ───────────────────────────────────────────────────────────

    /// Write an outcome the scheduler decided (spec §5.5, rule 4).
    ///
    /// While the task's ordinary owned work is live it holds as `completing`;
    /// otherwise it is terminal at once.
    pub(super) async fn terminate(
        self: &Arc<Self>,
        tx: &mut Transaction,
        state: &SessionState<'_>,
        record: &TaskRecord,
        outcome: TaskOutcome,
    ) -> Result<(), StorageError> {
        self.load_scopes_in(state, false).await?;
        let overlay = Overlay::of(tx);
        let busy =
            self.with_state(|indexes| has_owned_live(indexes, record.id, Some(&overlay), None));
        let next = if busy {
            TaskState::Completing { outcome }
        } else {
            TaskState::Terminal { outcome }
        };
        self.stage(tx, with_state(&progressed(record), next));
        Ok(())
    }

    /// Replace a running task's state with what it committed (spec §5.1, §5.5).
    ///
    /// A terminal state holds as `completing` while ordinary owned work is
    /// live, judged on the commit's candidates, so work the same commit creates
    /// below the task counts. A wait is validated first; a sleep's `until` is a
    /// time on the harness clock.
    pub(super) async fn commit_state(
        self: &Arc<Self>,
        tx: &mut Transaction,
        state: &SessionState<'_>,
        invocation: &super::Invocation,
        current: &TaskRecord,
        next: NextTaskState,
    ) -> Result<(), TaskRuntimeError> {
        if let NextTaskState::Waiting { on, policy, .. } = &next {
            self.validate_wait(tx, state, invocation, current, on, *policy)
                .await?;
        }
        if let NextTaskState::Sleeping { until, .. } = &next {
            if invocation.mode == InvocationMode::Abort {
                return Err(TaskRuntimeError::Invalid(format!(
                    "Abort handler of task {} cannot sleep",
                    current.id
                )));
            }
            if *until < 0 {
                return Err(TaskRuntimeError::Invalid(format!(
                    "Task {} cannot sleep until {}: not a time in milliseconds",
                    current.id, until
                )));
            }
        }
        if let NextTaskState::Terminal { outcome } = &next {
            let overlay = Overlay::of(tx);
            self.load_scopes_in(state, false).await?;
            // The chains above the commit's new tasks must be known before a
            // walk down can see them.
            let parents: Vec<TaskId> = overlay
                .tasks
                .values()
                .filter(|record| self.with_state(|indexes| !indexes.live.contains_key(&record.id)))
                .map(|record| parent_of(node_of(record)))
                .filter_map(|up| match up {
                    super::Up::Task(id) => Some(id),
                    super::Up::Conversation(_) => None,
                })
                .collect();
            for id in parents {
                self.load_chain_in(state, super::Up::Task(id)).await?;
            }
            let busy = self
                .with_state(|indexes| has_owned_live(indexes, current.id, Some(&overlay), None));
            if busy {
                self.stage(
                    tx,
                    with_state(
                        &progressed(current),
                        TaskState::Completing {
                            outcome: outcome.clone(),
                        },
                    ),
                );
                return Ok(());
            }
        }
        let next_state = next.to_state();
        // A state the task committed that differs from its current one is
        // progress: its restart count starts over. The same state committed
        // again, for example as intent before an effect, is not.
        //
        // Upstream compares the whole state; its `running` states never carry
        // the retry counter, which lives on the record here, so
        // serialization-comparison stays meaningful. `CurrentPoint`-style
        // float values are not part of task state.
        let same = json_equal(
            &serde_json::to_value(&current.state).unwrap_or(Value::Null),
            &serde_json::to_value(&next_state).unwrap_or(Value::Null),
        );
        let base = if same {
            current.clone()
        } else {
            progressed(current)
        };
        self.stage(tx, with_state(&base, next_state));
        Ok(())
    }

    /// A wait names existing tasks, and `failFast` only tasks the waiter owns
    /// (spec §5.5). The wait-for graph is acyclic.
    async fn validate_wait(
        self: &Arc<Self>,
        tx: &mut Transaction,
        state: &SessionState<'_>,
        invocation: &super::Invocation,
        current: &TaskRecord,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> Result<(), TaskRuntimeError> {
        if invocation.mode == InvocationMode::Abort {
            return Err(TaskRuntimeError::Invalid(format!(
                "Abort handler of task {} cannot wait",
                current.id
            )));
        }
        let overlay = Overlay::of(tx);
        let mut targets: Vec<TaskId> = Vec::new();
        for id in on {
            let member = match overlay.tasks.get(id).cloned() {
                Some(member) => Some(member),
                None => match self.live_record(*id) {
                    Some(member) => Some(member),
                    None => read_task_in(state, *id).await?,
                },
            };
            let Some(member) = member else {
                return Err(TaskRuntimeError::Invalid(format!(
                    "Task {id} does not exist"
                )));
            };
            if policy == JoinPolicy::FailFast && member.owner != Some(current.id) {
                return Err(TaskRuntimeError::Invalid(format!(
                    "Task {} can wait failFast only on tasks it owns; {id} is not one",
                    current.id
                )));
            }
            // A live child already holds the waiter (task-owned tasks are never
            // background): waiting on it adds no edge. A terminal task has no
            // edges.
            if !member.state.is_terminal() && member.owner != Some(current.id) {
                targets.push(*id);
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        if let Some(cycle) = self
            .wait_cycle(state, current.id, &targets, &overlay)
            .await?
        {
            return Err(TaskRuntimeError::Invalid(format!(
                "Task {} cannot wait on {}: {} is a wait cycle",
                current.id,
                cycle[0],
                std::iter::once(current.id)
                    .chain(cycle.iter().copied())
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            )));
        }
        Ok(())
    }

    /// The path from a member of `on` back to `task` in the wait-for graph, if
    /// the wait would close a cycle (spec §5.5).
    ///
    /// An edge `X -> Y` means X cannot settle before Y: Y is in the `on` of a
    /// waiting X that is not abort-marked, or Y is live ordinary owned work of
    /// X, which holds X as `completing`.
    async fn wait_cycle(
        self: &Arc<Self>,
        state: &SessionState<'_>,
        task: TaskId,
        on: &[TaskId],
        overlay: &Overlay,
    ) -> Result<Option<Vec<TaskId>>, StorageError> {
        // Owned work is found by walks down, which need every chain loaded.
        self.load_scopes_in(state, false).await?;
        let mut from: BTreeMap<TaskId, Option<TaskId>> = BTreeMap::new();
        let mut walked: BTreeSet<TaskId> = BTreeSet::new();
        let mut pending: Vec<TaskId> = Vec::new();
        fn reach(
            from: &mut BTreeMap<TaskId, Option<TaskId>>,
            pending: &mut Vec<TaskId>,
            id: TaskId,
            via: Option<TaskId>,
        ) {
            if from.contains_key(&id) {
                return;
            }
            from.insert(id, via);
            pending.push(id);
        }
        for id in on {
            reach(&mut from, &mut pending, *id, None);
        }
        while let Some(id) = pending.pop() {
            if id == task {
                let mut steps = vec![id];
                let mut at = from.get(&id).copied().flatten();
                while let Some(node) = at {
                    steps.push(node);
                    at = from.get(&node).copied().flatten();
                }
                steps.reverse();
                return Ok(Some(steps));
            }
            let record = overlay
                .tasks
                .get(&id)
                .cloned()
                .or_else(|| self.live_record(id));
            let Some(record) = record else {
                continue;
            };
            if record.state.is_terminal() {
                continue;
            }
            // An abort-marked task waits only for its owned work, so its `on`
            // is no edge.
            if let TaskState::Waiting { on, .. } = &record.state {
                if !record.abort_requested {
                    for member in on {
                        reach(&mut from, &mut pending, *member, Some(id));
                    }
                }
            }
            if !walked.insert(id) {
                continue;
            }
            // Below a walked task the walk does not go again: its own walk
            // covered what lies below.
            let walk = self.with_state(|indexes| {
                let enter = |below: TaskId| !walked.contains(&below);
                walk_down(indexes, Node::Task(id), Some(overlay), false, Some(&enter))
            });
            let mut found: Vec<TaskId> = Vec::new();
            for item in walk {
                if item.live.is_none() || item.node.background {
                    continue;
                }
                found.push(item.id);
                reach(&mut from, &mut pending, item.id, Some(id));
            }
            for below in found {
                walked.insert(below);
            }
        }
        Ok(None)
    }

    // ─── Abort ──────────────────────────────────────────────────────────────

    /// Commit the abort mark, or settle a task no definition can take as
    /// `orphaned`, then join the run invocation seen on the line (spec §5.4).
    pub(super) async fn abort(
        self: &Arc<Self>,
        id: TaskId,
        keep_restart: bool,
    ) -> Result<AbortResult, StorageError> {
        let inner = self.clone();
        let result = self
            .commit(move |tx, state| {
                let inner = inner.clone();
                Box::pin(async move {
                    let Some(current) = read_task_in(state, id).await? else {
                        return Err(StorageError::invalid(format!("Task {id} does not exist")));
                    };
                    if current.state.is_terminal() {
                        return Ok((AbortResult::Terminal, None));
                    }
                    let invocation =
                        inner.with_state(|indexes| indexes.invocations.get(&id).cloned());
                    // A restart-marked task has no run invocation; it only
                    // waits for its abort.
                    if keep_restart && current.abort_reason.as_deref() == Some(REASON_RESTART) {
                        return Ok((AbortResult::Marked, None));
                    }
                    if invocation.is_none()
                        && !matches!(current.state, TaskState::Completing { .. })
                    {
                        inner.load_scopes_in(state, false).await?;
                        let busy =
                            inner.with_state(|indexes| has_owned_live(indexes, id, None, None));
                        if !busy {
                            if let Err(reason) = resolve(&inner, &current) {
                                let outcome = super::orphaned(reason.as_str().to_owned());
                                inner.terminate(tx, state, &current, outcome).await?;
                                return Ok((AbortResult::Marked, None));
                            }
                        }
                    }
                    if !current.abort_requested || current.abort_reason.is_some() {
                        inner.stage(tx, with_abort_mark(&current, None));
                    }
                    Ok((AbortResult::Marked, invocation))
                })
            })
            .await?;
        // The commit listener signalled the run; join it.
        if let Some(invocation) = &result.1 {
            if invocation.mode == InvocationMode::Run {
                invocation.wait_done().await;
            }
        }
        Ok(result.0)
    }

    /// Mark every live non-background task ordinary traversal from the
    /// conversation reaches, and signal the runs (spec §5.4).
    ///
    /// With `background`, traversal crosses background boundaries.
    pub(super) async fn mark_conversation(
        self: &Arc<Self>,
        conversation: ConversationId,
        background: bool,
    ) -> Result<Vec<TaskId>, StorageError> {
        let inner = self.clone();
        let result = self
            .commit(move |tx, state| {
                let inner = inner.clone();
                Box::pin(async move {
                    inner.load_scopes_in(state, false).await?;
                    let walk = inner.with_state(|indexes| {
                        walk_down(
                            indexes,
                            Node::Conversation(conversation),
                            None,
                            background,
                            None,
                        )
                    });
                    let mut reached: Vec<TaskId> = Vec::new();
                    let mut signal: Vec<Arc<super::Invocation>> = Vec::new();
                    for item in walk {
                        let Some(live) = item.live else {
                            continue;
                        };
                        if item.node.background && !background {
                            continue;
                        }
                        reached.push(item.id);
                        if !live.abort_requested || live.abort_reason.is_some() {
                            inner.stage(tx, with_abort_mark(&live, None));
                        }
                        if let Some(invocation) =
                            inner.with_state(|indexes| indexes.invocations.get(&item.id).cloned())
                        {
                            if invocation.mode == InvocationMode::Run {
                                signal.push(invocation);
                            }
                        }
                    }
                    Ok((reached, signal))
                })
            })
            .await?;
        for invocation in &result.1 {
            invocation.signal.abort();
        }
        Ok(result.0)
    }

    // ─── Waiting ────────────────────────────────────────────────────────────

    /// Waits for a terminal receipt (spec §5.4). Checks and registers on the
    /// line so no terminal publication falls between them.
    pub(super) async fn wait_for_task(
        self: &Arc<Self>,
        id: TaskId,
    ) -> Result<TaskRecord, StorageError> {
        let inner = self.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let slot = Arc::new(std::sync::Mutex::new(Some(sender)));
        let registered = self
            .read_on_line(move |state| {
                let inner = inner.clone();
                let slot = slot.clone();
                Box::pin(async move {
                    if state.storage.is_closing() {
                        return Err(StorageError::closed("Session"));
                    }
                    if inner.with_state(|indexes| indexes.live.contains_key(&id)) {
                        let sender = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        if let Some(sender) = sender {
                            inner
                                .task_waiters
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .entry(id)
                                .or_default()
                                .push(sender);
                        }
                        return Ok(true);
                    }
                    Ok(false)
                })
            })
            .await?;
        if registered {
            return receiver
                .await
                .map_err(|_| StorageError::closed("Session"))?;
        }
        match self.read_task(id).await? {
            Some(record) => Ok(record),
            None => Err(StorageError::invalid(format!("Task {id} does not exist"))),
        }
    }

    /// Resolve when ordinary traversal from the conversation, or from every
    /// ownerless conversation, reaches no live non-background task.
    ///
    /// Registration happens before the idle check, both under the lock
    /// [`settle_idle`](SchedulerInner::settle_idle) and `seal` hold while they
    /// take the waiters: a pass that settles the scope can neither miss this
    /// waiter nor resolve it twice, and a close cannot strand it. A scope that
    /// is already idle (or closing) settles here without waiting.
    pub(super) async fn wait_for_idle(
        self: &Arc<Self>,
        conversation: Option<ConversationId>,
    ) -> Result<(), StorageError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut idle = self
                .idle_waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.is_closing() {
                return Err(StorageError::closed("Session"));
            }
            idle.push((conversation, sender));
            if self.idle(conversation) {
                // Only this call's entry can be last: the lock was held since
                // before the push, and a settle pass takes the whole vector
                // under it. Dropping the sender with the entry settles nothing
                // else.
                let _ = idle.pop();
                return Ok(());
            }
        }
        // The waiter is registered before this pass is queued, so the pass
        // settles it; scheduling after the check loses no wake.
        self.schedule_reconcile();
        receiver
            .await
            .map_err(|_| StorageError::closed("Session"))?
    }
}

/// Whether any of `ids` holds or ended with an outcome other than `completed`.
async fn any_failed(
    inner: &SchedulerInner,
    state: &SessionState<'_>,
    ids: &[TaskId],
) -> Result<bool, StorageError> {
    for id in ids {
        let record = match inner.live_record(*id) {
            Some(record) => Some(record),
            None => read_task_in(state, *id).await?,
        };
        if record.as_ref().is_some_and(failed_outcome) {
            return Ok(true);
        }
    }
    Ok(false)
}

// ─── Inspection and wake ────────────────────────────────────────────────────

/// What the scheduler would do with `record` (upstream `#inspectTask`).
pub(super) fn inspect_task(
    inner: &SchedulerInner,
    record: &TaskRecord,
    catalogue: &super::TaskCatalogue,
) -> TaskInspectionState {
    if inner.with_state(|indexes| indexes.invocations.contains_key(&record.id)) {
        return TaskInspectionState::Running;
    }
    if matches!(record.state, TaskState::Completing { .. }) {
        return TaskInspectionState::Completing;
    }
    let on = waiting_on(inner, record);
    if !on.is_empty() {
        return TaskInspectionState::Waiting { on };
    }
    if super::not_due(record, inner.now()) {
        if let TaskState::Sleeping { until, .. } = &record.state {
            return TaskInspectionState::Sleeping { until: *until };
        }
    }
    if inner.parked(record) {
        return TaskInspectionState::Blocked {
            reason: BlockedReason::RestartLimit,
        };
    }
    match fit(inner, record, catalogue) {
        Err(reason) => TaskInspectionState::Blocked { reason },
        Ok(fit) => TaskInspectionState::Ready {
            migrates: fit.migrates,
        },
    }
}

/// Live tasks a task waits for before its next invocation (spec §5.5).
///
/// Its live ordinary owned work when abort-marked, since abort runs bottom-up;
/// otherwise the live part of the `on` of a wait.
pub(super) fn waiting_on(inner: &SchedulerInner, record: &TaskRecord) -> Vec<TaskId> {
    if record.abort_requested {
        return inner.with_state(|indexes| owned_live(indexes, record.id, None, false, None));
    }
    match &record.state {
        TaskState::Waiting { on, .. } => inner.with_state(|indexes| {
            on.iter()
                .filter(|id| indexes.live.contains_key(id))
                .copied()
                .collect()
        }),
        _ => Vec::new(),
    }
}

/// When the scheduler next needs a running process (spec §5.6).
pub(super) fn wake(inner: &SchedulerInner) -> Wake {
    let now = inner.now();
    let catalogue = inner.catalogue.snapshot();
    let (invocations, reconciling, live): (usize, bool, Vec<TaskRecord>) =
        inner.with_state(|indexes| {
            (
                indexes.invocations.len(),
                indexes.cascade_pending
                    || !indexes.finalize_checks.is_empty()
                    || !indexes.fail_fast_checks.is_empty(),
                indexes.live.values().cloned().collect(),
            )
        });
    let mut active = invocations > 0 || reconciling;
    let mut at: Option<i64> = None;
    for record in live {
        if matches!(record.state, TaskState::Completing { .. }) {
            continue;
        }
        if matches!(record.state, TaskState::Waiting { .. })
            && !record.abort_requested
            && !waiting_on(inner, &record).is_empty()
        {
            continue;
        }
        if let TaskState::Sleeping { until, .. } = record.state {
            if !record.abort_requested && until > now {
                at = Some(at.map_or(until, |current| current.min(until)));
                continue;
            }
        }
        if active {
            continue;
        }
        if inner.parked(&record) {
            continue;
        }
        // An abort invocation waits for the live work below it, which counts
        // itself.
        if record.abort_requested
            && inner
                .with_state(|indexes| !owned_live(indexes, record.id, None, true, None).is_empty())
        {
            continue;
        }
        // A task no definition can take waits for a catalogue change, except an
        // abort-marked one, which is orphaned.
        if (!record.abort_requested || record.abort_reason.is_some())
            && fit(inner, &record, &catalogue).is_err()
        {
            continue;
        }
        active = true;
    }
    Wake { active, at }
}

// ─── Definition fit and migration ───────────────────────────────────────────

/// A definition that can take a record, or why none can.
pub(super) struct Fit {
    /// Whether the definition is newer and has a migration to run.
    pub migrates: bool,
}

/// Decide whether `record`'s definition can take it, without running any task
/// code (upstream `#fit`).
pub(super) fn fit(
    inner: &SchedulerInner,
    record: &TaskRecord,
    catalogue: &super::TaskCatalogue,
) -> Result<Fit, BlockedReason> {
    let Some(task) = catalogue.task(&record.kind) else {
        return Err(BlockedReason::MissingTask);
    };
    let version = task.version();
    if version == record.version {
        return Ok(Fit { migrates: false });
    }
    if version < record.version {
        return Err(BlockedReason::TaskTooOld);
    }
    // A failed migration is retried only once another definition resolves.
    let failed = inner.with_state(|indexes| indexes.failed_migrations.get(&record.id).cloned());
    if failed.is_some_and(|(identity, _)| identity == catalogue.identity(&record.kind)) {
        return Err(BlockedReason::MigrationFailed);
    }
    Ok(Fit { migrates: true })
}

/// A resolved definition and the record at its version (upstream `Resolution`).
pub(super) struct ResolvedTask {
    /// The definition that will run.
    pub task: Arc<dyn ErasedTask>,
    /// The catalogue snapshot it came from.
    pub catalogue: super::TaskCatalogue,
    /// The record to write `running`, migrated when needed.
    pub record: TaskRecord,
    /// Whether a migration ran.
    pub migrates: bool,
}

/// Resolve a record's definition, migrating an older stored version
/// (upstream `#resolve`).
pub(super) fn resolve(
    inner: &Arc<SchedulerInner>,
    record: &TaskRecord,
) -> Result<ResolvedTask, BlockedReason> {
    let catalogue = inner.catalogue.snapshot();
    let fit = fit(inner, record, &catalogue)?;
    let task = catalogue
        .task(&record.kind)
        .ok_or(BlockedReason::MissingTask)?;
    if !fit.migrates {
        return Ok(ResolvedTask {
            task,
            catalogue,
            record: record.clone(),
            migrates: false,
        });
    }
    let Some(migration) = task.migration() else {
        let message = format!(
            "Task {} version {} has no migration from {}",
            record.kind,
            task.version(),
            record.version
        );
        inner.with_state(|indexes| {
            indexes.failed_migrations.insert(
                record.id,
                (catalogue.identity(&record.kind), message.clone()),
            );
        });
        inner.report(
            message,
            ReportInfo::MigrationFailed {
                task: inner.about_task(record),
                extension: catalogue.installer(&record.kind).map(str::to_owned),
            },
        );
        // A task blocked on a definition no longer keeps the wake active.
        inner.schedule_wake();
        return Err(BlockedReason::MigrationFailed);
    };
    match migration.apply(record.version, &record.input, checkpoint_of(&record.state)) {
        Ok(migrated) => {
            let mut next = record.clone();
            next.version = task.version();
            next.input = migrated.input;
            next.state = with_checkpoint(&record.state, migrated.checkpoint);
            Ok(ResolvedTask {
                task,
                catalogue,
                record: next,
                migrates: true,
            })
        }
        Err(message) => {
            inner.with_state(|indexes| {
                indexes.failed_migrations.insert(
                    record.id,
                    (catalogue.identity(&record.kind), message.clone()),
                );
            });
            inner.report(
                message,
                ReportInfo::MigrationFailed {
                    task: inner.about_task(record),
                    extension: catalogue.installer(&record.kind).map(str::to_owned),
                },
            );
            // A task blocked on a failed migration no longer keeps the wake
            // active.
            inner.schedule_wake();
            Err(BlockedReason::MigrationFailed)
        }
    }
}

/// Replace a checkpoint-bearing state's checkpoint.
fn with_checkpoint(state: &TaskState, checkpoint: Value) -> TaskState {
    match state {
        TaskState::Pending { .. } => TaskState::Pending { checkpoint },
        TaskState::Running { .. } => TaskState::Running { checkpoint },
        TaskState::Waiting { on, policy, .. } => TaskState::Waiting {
            checkpoint,
            on: on.clone(),
            policy: *policy,
        },
        TaskState::Sleeping { until, .. } => TaskState::Sleeping {
            checkpoint,
            until: *until,
        },
        other => other.clone(),
    }
}

/// Read one task directly from the guarded storage, inside a commit callback.
pub(super) async fn read_task_in(
    state: &SessionState<'_>,
    id: TaskId,
) -> Result<Option<TaskRecord>, StorageError> {
    let storage = state.storage.storage();
    state
        .storage
        .read(&Context::background(), storage.task(id))
        .await
}
