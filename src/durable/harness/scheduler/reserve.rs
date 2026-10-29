//! The reservation pass: from a durable record to a live invocation.
//!
//! Ports upstream `harness/scheduler.ts`'s `#reserve` (`:1466-1574`), including
//! the abandonment marks it applies first, the wake-due sleeps and unparked
//! tasks it makes candidates, the candidate filters, definition fit and
//! migration, the `running` write and the in-memory invocation created **on the
//! line**.
//!
//! # One commit
//!
//! Every decision in this module stages inside one
//! [`Session::commit_scoped`] callback:
//!
//! ```text
//! abandoned -> abort mark "restart" (and reserve nothing this pass)
//! wake due sleeps; unpark raised limits
//! for each runnable candidate:
//!   skip if completing, already invoked, waiting on live work, not due,
//!          parked, or below an owner with cancellation intent
//!   settle an abort invocation cut off at the restart limit as faulted
//!   resolve the definition:
//!     blocked  -> (abort mode only) orphan, otherwise stay put
//!     migrated -> write running with the migrated record
//!   write running; create the Invocation, registered in `invocations`
//! ```
//!
//! Creating the invocation on the line is what makes "one invocation per task"
//! true: a mark or a later pass sees it, and close joins it. A failed
//! reservation commit removes every invocation it created and puts those tasks
//! back in `runnable`.
//!
//! [`Session::commit_scoped`]: crate::durable::session::facade::Session::commit_scoped

use std::sync::Arc;

use crate::durable::errors::StorageError;
use crate::durable::harness::scheduler::reconcile::{resolve, ResolvedTask};
use crate::durable::harness::scheduler::{
    below_cancelled, checkpoint_of, faulted_with_detail, node_of, not_due, owned_live, parent_of,
    with_abort_mark, with_state, Invocation, InvocationMode, NextTaskState, Reservation,
    SchedulerInner, REASON_RESTART,
};
use crate::durable::ids::TaskId;
use crate::durable::types::{TaskRecord, TaskState};

impl SchedulerInner {
    /// Reserve every eligible task in one commit (upstream `#reserve`).
    ///
    /// Orphan abort-marked tasks no definition can take, unless abandoned after
    /// a restart. The first pass after open only abort-marks the abandoned
    /// tasks, so later passes see the marks.
    pub(super) async fn reserve(self: &Arc<Self>) -> Result<Vec<Reservation>, StorageError> {
        let inner = self.clone();
        // Invocations created inside the callback. A rejected commit lands
        // nothing, so they are removed outside it (a callback's in-memory
        // effects are not rolled back with the transaction).
        let created: Arc<std::sync::Mutex<Vec<TaskId>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let created_out = created.clone();
        let inner_for_commit = inner.clone();
        let committed = self
            .commit(move |tx, state| {
                let inner = inner_for_commit.clone();
                let created_out = created_out.clone();
                Box::pin(async move {
                    if !inner.with_state(|indexes| indexes.enabled) || inner.is_closing() {
                        return Ok((Vec::new(), Vec::new(), Vec::new()));
                    }
                    // Abandonment marks first: nothing of an abandoned task or
                    // below it runs a phase again. The pass then reserves
                    // nothing, so later passes see the marks.
                    let abandoned: Vec<TaskId> =
                        inner.with_state(|indexes| indexes.abandoned.iter().copied().collect());
                    if !abandoned.is_empty() {
                        for id in &abandoned {
                            if let Some(record) = inner.live_record(*id) {
                                if !record.abort_requested {
                                    inner.stage(tx, with_abort_mark(&record, Some(REASON_RESTART)));
                                }
                            }
                        }
                        return Ok((Vec::new(), Vec::new(), abandoned));
                    }

                    inner.load_scopes_in(state, false).await?;
                    // Every pass picks up sleeps whose time has come: a pass
                    // started by any change may be the first after a deadline.
                    inner.wake_due();
                    unpark_raised(&inner);

                    let candidates: Vec<TaskRecord> = inner.with_state(|indexes| {
                        indexes
                            .runnable
                            .iter()
                            .filter_map(|id| indexes.live.get(id).cloned())
                            .collect()
                    });
                    let mut reservations: Vec<Reservation> = Vec::new();
                    let mut faulted: Vec<(TaskRecord, String, Option<String>)> = Vec::new();
                    for record in candidates {
                        // Upstream's candidate filters, in order. Each one
                        // leaves the runnable set, so a later pass reconsiders
                        // the task only when its record changes.
                        let Some(live) = classify(&inner, record) else {
                            continue;
                        };
                        let id = live.id;
                        let mode = if live.abort_requested {
                            InvocationMode::Abort
                        } else {
                            InvocationMode::Run
                        };
                        // Work below an owner with cancellation intent waits
                        // for its cascade mark instead of running a phase;
                        // schedule the cascade, which may not have run yet.
                        if mode == InvocationMode::Run
                            && !live.background
                            && inner.with_state(|indexes| {
                                below_cancelled(indexes, parent_of(node_of(&live)))
                            })
                        {
                            inner.with_state(|indexes| indexes.cascade_pending = true);
                            inner.request_reconcile();
                            continue;
                        }
                        // An abort handler cut off `restartLimit` times in a row
                        // would run forever; nothing is left to park it for.
                        if mode == InvocationMode::Abort && inner.abort_cut_off(&live) {
                            let message = format!(
                                "Abort handler of task {id} ({}) was cut off {} times",
                                live.kind,
                                live.restarts.unwrap_or(0)
                            );
                            inner
                                .terminate(
                                    tx,
                                    state,
                                    &live,
                                    faulted_with_detail(
                                        message.clone(),
                                        serde_json::json!({ "reason": "restart_limit" }),
                                    ),
                                )
                                .await?;
                            let extension = inner.installer(&live.kind);
                            faulted.push((live, message, extension));
                            continue;
                        }
                        let ResolvedTask {
                            task,
                            catalogue,
                            mut record,
                            migrates,
                        } = match resolve(&inner, &live) {
                            Ok(resolution) => resolution,
                            Err(reason) => {
                                // An abandoned task waits for its definition,
                                // so its abort handler can clean up.
                                if mode == InvocationMode::Abort && live.abort_reason.is_none() {
                                    inner
                                        .terminate(
                                            tx,
                                            state,
                                            &live,
                                            super::orphaned(reason.as_str().to_owned()),
                                        )
                                        .await?;
                                }
                                continue;
                            }
                        };
                        // A child task created without submission ids has no
                        // carrier of its own. Per F29 it inherits its owner's
                        // `trace` — that task's first-submission carrier — so
                        // its phase spans parent to the chain that created it.
                        // A child never inherits `links`: those are its own
                        // creation-time submissions and it has none. Done
                        // before the dispatch write below, so the `running`
                        // record carries the carrier.
                        if record.trace.is_none() {
                            if let Some(owner) = record.owner {
                                record.trace = inner
                                    .with_state(|indexes| indexes.live.get(&owner)?.trace.clone());
                            }
                        }
                        // Migration runs atomically with `pending` / `waiting` /
                        // `sleeping` -> `running` (spec §5.4).
                        let dispatching =
                            migrates || !matches!(live.state, TaskState::Running { .. });
                        if dispatching {
                            let next = NextTaskState::Running {
                                checkpoint: checkpoint_of(&record.state).clone(),
                            };
                            inner.stage(tx, with_state(&record, next.to_state()));
                        } else if record.trace != live.trace {
                            // Already `running` (a re-dispatch after a crash):
                            // the carrier still has to become durable.
                            inner.stage(tx, record.clone());
                        }
                        // Registered on the line, so marks and later
                        // reservations see it and close joins it; the step that
                        // ends it makes the task a candidate again.
                        inner.with_state(|indexes| {
                            indexes.runnable.remove(&id);
                        });
                        let resumed = inner.telemetry.enabled()
                            && inner.with_state(|indexes| indexes.resumed.contains(&id));
                        let invocation = Arc::new(Invocation::new(
                            &record,
                            mode,
                            catalogue.installer(&record.kind).map(str::to_owned),
                            resumed,
                        ));
                        inner.with_state(|indexes| {
                            indexes.invocations.insert(id, invocation.clone());
                        });
                        created_out
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(id);
                        reservations.push(Reservation {
                            invocation,
                            task,
                            catalogue,
                        });
                    }
                    Ok((reservations, faulted, Vec::new()))
                })
            })
            .await;
        let (reservations, faulted, abandoned) = match committed {
            Ok(value) => value,
            Err(error) => {
                // The commit landed nothing. Remove every invocation this pass
                // created and make those tasks candidates again, so the next
                // pass retries them.
                for id in created
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .drain(..)
                {
                    inner.with_state(|indexes| {
                        indexes.invocations.remove(&id);
                        if indexes.live.contains_key(&id) {
                            indexes.runnable.insert(id);
                        }
                    });
                }
                return Err(error);
            }
        };

        // The first reservation of a task loaded at open consumed its
        // "resumed" marker; later invocations of the same task in this process
        // are not resumed. Only maintained while telemetry can observe it.
        if inner.telemetry.enabled() {
            for reservation in &reservations {
                let id = reservation.invocation.id;
                inner.with_state(|indexes| {
                    indexes.resumed.remove(&id);
                });
            }
        }
        for (record, message, extension) in faulted {
            self.report(
                message,
                crate::durable::errors::ReportInfo::TaskFaulted {
                    task: self.about_task(&record),
                    extension,
                    reason: crate::durable::errors::TaskFaultReason::RestartLimit,
                },
            );
        }
        // Marked, or already marked or gone: done with them. Reserve again, now
        // seeing the marks.
        for id in abandoned {
            self.with_state(|indexes| {
                indexes.abandoned.remove(&id);
                indexes.dirty = true;
            });
        }
        Ok(reservations)
    }
}

/// Apply the candidate filters to one runnable record.
///
/// Returns the live record when it should be reserved, or `None` after taking
/// it out of the runnable set.
fn classify(inner: &SchedulerInner, record: TaskRecord) -> Option<TaskRecord> {
    let id = record.id;
    let drop = |indexes: &mut super::Indexes| {
        indexes.runnable.remove(&id);
    };
    // An invocation is already live for it (one invocation per task).
    if inner.with_state(|indexes| indexes.invocations.contains_key(&id)) {
        inner.with_state(drop);
        return None;
    }
    // It is holding its outcome; the finalize pass owns it.
    if matches!(record.state, TaskState::Completing { .. }) {
        inner.with_state(drop);
        return None;
    }
    // It returns once what it waits for ends, its record changes, or the work
    // below it ends.
    if inner.with_state(|indexes| waiting_on_live(indexes, &record)) {
        inner.with_state(drop);
        return None;
    }
    // It returns at the first pass after its time comes, which the timer,
    // `resume()` or any change starts.
    if not_due(&record, inner.now()) {
        inner.with_state(drop);
        return None;
    }
    // It returns when `retry_task()` clears its restart count, or at a pass
    // after the restart limit rose.
    if inner.parked(&record) {
        inner.with_state(|indexes| {
            indexes.runnable.remove(&id);
            indexes.parked.insert(id);
        });
        return None;
    }
    let Some(live) = inner.live_record(id) else {
        inner.with_state(drop);
        return None;
    };
    Some(live)
}

/// The live tasks a task waits for before its next invocation (upstream
/// `#waitingOn`).
fn waiting_on_live(indexes: &super::Indexes, record: &TaskRecord) -> bool {
    if record.abort_requested {
        return !owned_live(indexes, record.id, None, true, None).is_empty();
    }
    match &record.state {
        TaskState::Waiting { on, .. } => on.iter().any(|id| indexes.live.contains_key(id)),
        _ => false,
    }
}

/// Make parked tasks that a raised `settings.restartLimit` no longer parks
/// candidates again (upstream `#unparkRaised`).
fn unpark_raised(inner: &SchedulerInner) {
    inner.with_state(|indexes| {
        let parked: Vec<TaskId> = indexes.parked.iter().copied().collect();
        for id in parked {
            let Some(record) = indexes.live.get(&id) else {
                indexes.parked.remove(&id);
                continue;
            };
            if inner.parked(record) {
                continue;
            }
            indexes.parked.remove(&id);
            indexes.runnable.insert(id);
        }
    });
}
