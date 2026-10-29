//! Running phase handlers: the step → phase → step loop.
//!
//! Ports upstream `harness/scheduler.ts`'s `#run` (`:1711-1736`), `#step`
//! (`:1863-1905`), `#decide` (`:1740-1800`) and `#runAbort` (`:1802-1826`), plus
//! `#end` (`:2077-2090`).
//!
//! # One commit per decision
//!
//! A **step** is exactly one commit on the session line. It re-reads the task,
//! and
//!
//! * rule 1 — the task is no longer `running` (terminal, `completing`,
//!   `waiting`, or `sleeping`), or
//! * rule 2 — the session is closing,
//!
//! ends the invocation there without a write. Otherwise [`decide`] applies the
//! remaining rules and may stage a write in that same commit:
//!
//! * rule 3 — a run-mode task carries a durable abort mark: end, no write (the
//!   abort invocation follows once owned work drains);
//! * an owner's cancellation intent reaches it: end, no write (reservation
//!   holds it back until its cascade mark lands);
//! * rule 4 — the handler threw: write terminal `faulted` (or `completing`);
//! * rule 5 — the checkpoint changed: hand over to a replacement definition
//!   that can take the task, writing `pending` with the checkpoint;
//! * rule 6 — the checkpoint did not change: write terminal `faulted` — no
//!   durable progress.
//!
//! A runtime commit the invocation queued earlier therefore either lands before
//! the step and counts, or reaches the line after it and rejects (the runtime's
//! gate). Ending happens inside the step callback, before a fault's settlement.
//!
//! # Handlers and panics
//!
//! Upstream catches a thrown handler with `try`/`catch`. A Rust phase handler
//! reports a failure by panicking; the port catches a panic with
//! `catch_unwind` on the handler future and treats its payload as rule 4's
//! throw.
//!
//! # The abort invocation
//!
//! It runs the abort handler once. After it settles, a step applies rules 1, 2
//! and 4; a handler that returns without a terminal outcome faults the task. A
//! task whose definition carries no abort handler faults with the same message,
//! because no code can decide the outcome.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;

use crate::durable::errors::{ReportTask, StorageError};
use crate::durable::harness::scheduler::runtime::TaskRuntime;
use crate::durable::harness::scheduler::{
    below_cancelled, checkpoint_of, faulted, json_equal, node_of, panic_message, parent_of,
    with_state, Invocation, InvocationMode, Reservation, SchedulerInner,
};
use crate::durable::harness::task::RunningTask;
use crate::durable::harness::telemetry::{SpanHandle, SpanLink, SpanStatus};
use crate::durable::ids::TaskId;
use crate::durable::session::Transaction;
use crate::durable::types::{TaskOutcome, TaskRecord, TaskState};

/// The outcome of the phase that just returned, judged by the next step.
#[derive(Clone)]
struct PhaseResult {
    checkpoint: serde_json::Value,
    failure: Option<String>,
}

/// A step decision: continue with the next phase, end the invocation, or end
/// it by writing `faulted`.
enum Decision {
    /// Run the next phase.
    Continue,
    /// End without a write.
    End,
    /// End and write terminal `faulted` with this message.
    Fault(String),
}

/// What a phase produced.
enum PhaseOutcome {
    /// The handler returned; the step judges the (unchanged) checkpoint.
    Continue(serde_json::Value),
    /// The handler threw.
    Threw(String),
}

/// One phase invocation and the open span it runs under, when telemetry is
/// installed. Everything here is built only with a live sink.
struct PhaseRun {
    outcome: PhaseOutcome,
    active: Option<ActivePhase>,
}

/// An open `pi.durable.phase` span and the values its end needs.
struct ActivePhase {
    span: SpanHandle,
    task_name: String,
    phase: String,
    started: Instant,
}

/// One step's value: the next record to run, and the fault it wrote, if any.
struct StepResult {
    next: Option<TaskRecord>,
    fault: Option<String>,
}

impl SchedulerInner {
    /// Run phase handlers, each preceded by a step that decides on the line
    /// whether the invocation continues (upstream `#run`).
    pub(super) async fn run(self: &Arc<Self>, reservation: Reservation) -> Result<(), String> {
        let invocation = reservation.invocation.clone();
        let runtime = TaskRuntime::new(
            self.clone(),
            invocation.clone(),
            reservation.catalogue.clone(),
        );
        let first = {
            let inner = self.clone();
            let invocation = invocation.clone();
            let reservation = reservation.clone();
            self.step(&invocation, move |tx, current| {
                decide(&inner, tx, &current, None, &reservation)
            })
            .await
        };
        let mut current = first.next;
        while let Some(task) = current.clone() {
            if self.is_closing() {
                break;
            }
            let PhaseRun { outcome, active } = {
                let inner = self.clone();
                let invocation = invocation.clone();
                let reservation = reservation.clone();
                self.phase(&invocation, &task, runtime.clone(), &reservation, inner)
                    .await
            };
            match outcome {
                PhaseOutcome::Continue(checkpoint) => {
                    let previous = PhaseResult {
                        checkpoint,
                        failure: None,
                    };
                    let inner = self.clone();
                    let invocation = invocation.clone();
                    let reservation = reservation.clone();
                    let step = self
                        .step(&invocation, move |tx, current| {
                            decide(&inner, tx, &current, Some(previous.clone()), &reservation)
                        })
                        .await;
                    if let Some(active) = active {
                        let record = self.phase_record(&step.next, invocation.id).await;
                        self.finish_phase(
                            &invocation,
                            active,
                            record.as_ref(),
                            step.fault.as_deref(),
                        );
                    }
                    invocation.set_phase_span(None);
                    current = step.next;
                }
                PhaseOutcome::Threw(message) => {
                    let previous = PhaseResult {
                        checkpoint: checkpoint_of(&task.state).clone(),
                        failure: Some(message),
                    };
                    let inner = self.clone();
                    let invocation = invocation.clone();
                    let reservation = reservation.clone();
                    let step = self
                        .step(&invocation, move |tx, current| {
                            decide(&inner, tx, &current, Some(previous.clone()), &reservation)
                        })
                        .await;
                    if let Some(active) = active {
                        let record = self.phase_record(&step.next, invocation.id).await;
                        self.finish_phase(
                            &invocation,
                            active,
                            record.as_ref(),
                            step.fault.as_deref(),
                        );
                    }
                    invocation.set_phase_span(None);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The record a settled phase left behind, for the span's outcome.
    ///
    /// Only called while a phase span is open, so the extra read costs nothing
    /// on the no-sink path.
    async fn phase_record(&self, next: &Option<TaskRecord>, id: TaskId) -> Option<TaskRecord> {
        if let Some(record) = next {
            return Some(record.clone());
        }
        if let Some(record) = self.live_record(id) {
            return Some(record);
        }
        self.read_task(id).await.ok().flatten()
    }

    /// Start `pi.durable.phase` for one handler invocation.
    ///
    /// The task's stored trace carrier (the first submission that created it)
    /// is the span's parent when the sink can extract one. The invocation's
    /// creation-time link carriers (the task's other first-run submissions;
    /// upstream `Invocation.links`, which is `None` once the task has started)
    /// become span links. Both are empty/root when telemetry is disabled, where
    /// this method returns before allocating anything.
    fn start_phase(
        &self,
        invocation: &Invocation,
        running: &RunningTask,
        record: &TaskRecord,
        kind: &str,
    ) -> Option<ActivePhase> {
        if !self.telemetry.enabled() {
            return None;
        }
        // A tool pair belongs to exactly one phase. Drop any leftover before
        // this phase's handler can publish its own, so a pair whose span never
        // ended cannot leak into the next phase.
        invocation.take_phase_tool();
        let phase = running.phase().unwrap_or("<missing>");
        let attempt = running
            .checkpoint
            .get("attempt")
            .and_then(serde_json::Value::as_i64);
        let parent = self.telemetry.extract(record.trace.as_ref());
        // O(links): one small Vec per phase, empty (no allocation) for the
        // common task created by a single submission, and empty again after a
        // restart because creation spans are only repeated by the first run.
        let creation_links = invocation.links();
        let mut links = Vec::with_capacity(creation_links.len());
        for carrier in creation_links {
            if let Some(context) = self.telemetry.extract(Some(carrier)) {
                links.push(SpanLink::new(context));
            }
        }
        let span = self.telemetry.start_phase_span(
            running.id.as_i64(),
            &running.kind,
            phase,
            kind,
            attempt,
            invocation.resumed,
            parent,
            &links,
        );
        if span.is_none() {
            return None;
        }
        Some(ActivePhase {
            span,
            task_name: running.kind.clone(),
            phase: phase.to_owned(),
            started: Instant::now(),
        })
    }

    /// End a phase span with its outcome, status, duration and, when the phase
    /// ran a tool, the `tool.name` / `tool.call_id` pair the host published at
    /// settlement.
    fn finish_phase(
        &self,
        invocation: &Invocation,
        active: ActivePhase,
        record: Option<&TaskRecord>,
        fault: Option<&str>,
    ) {
        let millis = active.started.elapsed().as_secs_f64() * 1000.0;
        let (outcome, status) = self.settlement(record, fault);
        // Taking consumes the pair with the phase it belongs to. A later phase
        // starts by clearing the slot (see `start_phase`), so a stale pair can
        // never reach a different span.
        let (tool_name, tool_call_id) = match invocation.take_phase_tool() {
            Some((name, call_id)) => (Some(name), Some(call_id)),
            None => (None, None),
        };
        self.telemetry.end_phase_span(
            active.span,
            &active.task_name,
            &active.phase,
            Some(&outcome),
            tool_name.as_deref(),
            tool_call_id.as_deref(),
            status,
            Some(millis),
        );
    }

    /// The `outcome` attribute and span status of a settled phase.
    fn settlement<'a>(
        &self,
        record: Option<&'a TaskRecord>,
        fault: Option<&'a str>,
    ) -> (String, SpanStatus<'a>) {
        if let Some(message) = fault {
            return (
                "faulted".to_owned(),
                SpanStatus::error_capped("task_faulted", message),
            );
        }
        let Some(record) = record else {
            return ("done".to_owned(), SpanStatus::Ok);
        };
        match &record.state {
            TaskState::Running { checkpoint } | TaskState::Pending { checkpoint } => {
                let aborted = matches!(&record.state, TaskState::Running { .. })
                    && (self.is_closing()
                        || record.abort_requested
                        || self.with_state(|indexes| {
                            below_cancelled(indexes, parent_of(node_of(record)))
                        }));
                if aborted {
                    ("aborted".to_owned(), SpanStatus::Ok)
                } else {
                    let phase = checkpoint
                        .get("phase")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("<missing>");
                    (format!("next={phase}"), SpanStatus::Ok)
                }
            }
            TaskState::Waiting { .. } => ("waiting".to_owned(), SpanStatus::Ok),
            TaskState::Sleeping { .. } => ("sleeping".to_owned(), SpanStatus::Ok),
            TaskState::Completing { outcome } | TaskState::Terminal { outcome } => match outcome {
                TaskOutcome::Completed { .. } => ("done".to_owned(), SpanStatus::Ok),
                TaskOutcome::Aborted { .. } => ("aborted".to_owned(), SpanStatus::Ok),
                TaskOutcome::Faulted { error } => (
                    "faulted".to_owned(),
                    SpanStatus::error_capped("task_faulted", &error.message),
                ),
                TaskOutcome::Failed { error, .. } => (
                    "faulted".to_owned(),
                    SpanStatus::error_capped("task_failed", &error.message),
                ),
                TaskOutcome::Orphaned { reason } => (
                    "faulted".to_owned(),
                    SpanStatus::error_capped("task_orphaned", reason),
                ),
            },
        }
    }

    /// Run one phase handler (upstream's `#phase` body).
    async fn phase(
        self: &Arc<Self>,
        invocation: &Arc<Invocation>,
        task: &TaskRecord,
        runtime: TaskRuntime,
        reservation: &Reservation,
        _inner: Arc<Self>,
    ) -> PhaseRun {
        let Some(running) = RunningTask::from_record(task) else {
            return PhaseRun {
                outcome: PhaseOutcome::Threw("task is not running".to_owned()),
                active: None,
            };
        };
        let Some(handler) = reservation.task.phase_for(&running.checkpoint) else {
            let phase = running.phase().unwrap_or("<missing>").to_owned();
            return PhaseRun {
                outcome: PhaseOutcome::Threw(format!(
                    "Task {} ({}) has no handler for phase {phase}",
                    task.id, task.kind
                )),
                active: None,
            };
        };
        // The span opens at phase entry, after the handler is resolved, and
        // its handle rides the handler context so nested runtime events attach
        // to it. `handler_context` does the `with_span_handle` attachment.
        let active = self.start_phase(invocation, &running, task, InvocationMode::Run.as_str());
        invocation.set_phase_span(active.as_ref().map(|phase| phase.span));
        let context = invocation.handler_context();
        let future = handler(&running, &runtime, context);
        let outcome = match AssertUnwindSafe(future).catch_unwind().await {
            Ok(()) => PhaseOutcome::Continue(running.checkpoint.clone()),
            Err(payload) => PhaseOutcome::Threw(panic_message(payload)),
        };
        PhaseRun { outcome, active }
    }

    /// One synchronous decision commit on the session line (upstream `#step`).
    ///
    /// A task that is no longer `running` (rule 1) or a closing session (rule 2)
    /// ends the invocation without a write; otherwise `decide` may stage a write
    /// and report whether the invocation continues. A rejected step also ends
    /// the invocation, and the task stays `running` for the next reservation.
    async fn step<F>(self: &Arc<Self>, invocation: &Arc<Invocation>, decide: F) -> StepResult
    where
        F: FnOnce(&mut Transaction, TaskRecord) -> Decision + Send + 'static,
    {
        let inner = self.clone();
        let handle = invocation.clone();
        let fault_report: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let faulted_out = fault_report.clone();
        let result = self
            .commit(move |tx, state| {
                let inner = inner.clone();
                let invocation = handle.clone();
                let faulted_out = faulted_out.clone();
                Box::pin(async move {
                    let running = inner
                        .live_record(invocation.id)
                        .filter(|record| matches!(record.state, TaskState::Running { .. }));
                    let decision = match running {
                        Some(record) if !inner.is_closing() => decide(tx, record),
                        _ => Decision::End,
                    };
                    match decision {
                        Decision::Continue => Ok(inner.live_record(invocation.id)),
                        Decision::End => {
                            inner.end(&invocation);
                            Ok(None)
                        }
                        Decision::Fault(message) => {
                            inner.end(&invocation);
                            let outcome = faulted(message.clone());
                            if let Some(record) = inner.live_record(invocation.id) {
                                inner.terminate(tx, state, &record, outcome.clone()).await?;
                            }
                            *faulted_out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message);
                            Ok(None)
                        }
                    }
                })
            })
            .await;
        match result {
            Ok(value) => {
                let fault = fault_report
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(message) = &fault {
                    // Reported once the fault is durable: a host may count it.
                    // The phase span is still open here, so the report event
                    // attaches to it when telemetry is installed.
                    self.report_with_span(
                        invocation.phase_span(),
                        message.clone(),
                        crate::durable::errors::ReportInfo::TaskFaulted {
                            task: ReportTask {
                                id: invocation.id,
                                kind: invocation.kind.clone(),
                                conversation_id: invocation.conversation_id,
                            },
                            extension: invocation.extension.clone(),
                            reason: crate::durable::errors::TaskFaultReason::Threw,
                        },
                    );
                }
                StepResult { next: value, fault }
            }
            Err(error) => {
                self.end(invocation);
                if !matches!(error, StorageError::Request(_)) {
                    self.fail_scheduling(&error);
                }
                StepResult {
                    next: None,
                    fault: None,
                }
            }
        }
    }

    /// Run the abort handler once (upstream `#runAbort`).
    pub(super) async fn run_abort(
        self: &Arc<Self>,
        reservation: Reservation,
    ) -> Result<(), String> {
        let invocation = reservation.invocation.clone();
        if self.is_closing() {
            return Ok(());
        }
        let Some(current) = self.live_record(invocation.id) else {
            return Ok(());
        };
        let runtime = TaskRuntime::new(
            self.clone(),
            invocation.clone(),
            reservation.catalogue.clone(),
        );
        let message = format!(
            "Abort handler of task {} returned without a terminal outcome",
            invocation.id
        );
        let Some(handler) = reservation.task.abort_handler() else {
            let fallback = message;
            let _ = self
                .step(&invocation, move |_tx, _current| Decision::Fault(fallback))
                .await;
            return Ok(());
        };
        let mut active = None;
        let failure = match RunningTask::for_abort(&current) {
            Some(running) => {
                active = self.start_phase(
                    &invocation,
                    &running,
                    &current,
                    InvocationMode::Abort.as_str(),
                );
                invocation.set_phase_span(active.as_ref().map(|phase| phase.span));
                let context = invocation.handler_context();
                let future = handler(&running, &runtime, context);
                match AssertUnwindSafe(future).catch_unwind().await {
                    Ok(()) => None,
                    Err(payload) => Some(panic_message(payload)),
                }
            }
            None => None,
        };
        let fault = failure.clone();
        let fallback = message;
        let step = self
            .step(&invocation, move |_tx, _current| {
                Decision::Fault(failure.clone().unwrap_or_else(|| fallback.clone()))
            })
            .await;
        if let Some(active) = active {
            let record = self.phase_record(&step.next, invocation.id).await;
            self.finish_phase(
                &invocation,
                active,
                record.as_ref(),
                step.fault.as_deref().or(fault.as_deref()),
            );
        }
        invocation.set_phase_span(None);
        Ok(())
    }
}

/// The precedence rules for a run invocation, on the line (upstream `#decide`).
///
/// Rules 1 (terminal, `completing`, `waiting`, or `sleeping`) and 2 (closing)
/// are applied by [`SchedulerInner::step`].
fn decide(
    inner: &Arc<SchedulerInner>,
    tx: &mut Transaction,
    current: &TaskRecord,
    previous: Option<PhaseResult>,
    reservation: &Reservation,
) -> Decision {
    // 3. abort mark: end; a fresh abort invocation starts once the task's
    // ordinary owned work is gone.
    if current.abort_requested {
        return Decision::End;
    }
    // An owner's cancellation intent ends it too, before its cascade marks it;
    // reservation then holds it back.
    if !current.background
        && inner.with_state(|indexes| below_cancelled(indexes, parent_of(node_of(current))))
    {
        inner.with_state(|indexes| indexes.cascade_pending = true);
        inner.request_reconcile();
        return Decision::End;
    }
    let Some(previous) = previous else {
        return Decision::Continue;
    };
    // 4. uncaught error.
    if let Some(message) = previous.failure {
        return Decision::Fault(message);
    }
    // 6. no durable progress.
    let checkpoint = checkpoint_of(&current.state);
    if json_equal(checkpoint, &previous.checkpoint) {
        return Decision::Fault(format!(
            "Task {} phase {} returned without durable progress",
            current.kind,
            previous
                .checkpoint
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<missing>")
        ));
    }
    // 5. progress: refresh the snapshot; hand over to a replacement definition
    // that can take the task.
    let catalogue = inner.catalogue.snapshot();
    let replacement = catalogue.task(&current.kind);
    let changed = replacement
        .as_ref()
        .is_none_or(|task| !Arc::ptr_eq(task, &reservation.task));
    if changed {
        if let Some(task) = replacement.clone() {
            if task.can_reserve_version(current.version) {
                let checkpoint = checkpoint_of(&current.state).clone();
                inner.stage(tx, with_state(current, TaskState::Pending { checkpoint }));
                return Decision::End;
            }
        }
        // The invocation keeps running under its old definition. It
        // reconsiders at its next boundary.
        inner.report_with_span(
            reservation.invocation.phase_span(),
            format!(
                "Task {} keeps running under its old {} definition",
                current.id, current.kind
            ),
            crate::durable::errors::ReportInfo::TaskOutdated {
                task: inner.about_task(current),
                extension: reservation.invocation.extension.clone(),
                reason: if replacement.is_none() {
                    crate::durable::errors::TaskOutdatedReason::MissingTask
                } else {
                    crate::durable::errors::TaskOutdatedReason::IncompatibleTask
                },
            },
        );
    }
    Decision::Continue
}
