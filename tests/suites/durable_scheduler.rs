//! Conformance tests for the durable task scheduler (spec §5).
//!
//! These exercise the ported `harness/scheduler.ts` against a real
//! [`MemoryStorage`] and the Session facade, on a fake clock. Everything a test
//! observes is committed state: the scheduler's result is the durable task
//! record, so a check that a transition "happened" is a check of what a reopen
//! would read.
//!
//! | test | invariant |
//! |---|---|
//! | `reserve_run_terminal` | one invocation per task; reservation writes `running` |
//! | `phase_throw_faults` | rule 4: an uncaught error writes terminal `faulted` |
//! | `commit_after_end_rejects` | a runtime commit after the invocation ended lands nothing |
//! | `abort_runs_abort_handler` | abort marks a running task and runs its abort handler |
//! | `cascade_marks_children_bottom_up` | abort flows down; a background subtree is a boundary |
//! | `completing_hold_then_promotion` | a terminal outcome with live owned work holds, then finalizes |
//! | `wait_all_settled` / `wait_fail_fast_marks_siblings` | structured concurrency waits |
//! | `wait_cycle_rejected` | the wait-for graph is acyclic |
//! | `sleep_resumes_after_clock` / `sleep_survives_reopen` | sleeping is durable |
//! | `crash_loop_parks_and_retry` | the crash-loop guard and `retry_task` |
//! | `open_counts_restarts_close_does_not` | open/close restart accounting |
//!
//! `MemoryStorage::close` closes the backend, so tests that reopen a scheduler
//! use [`TaskScheduler::join`] (which writes stopped tasks back `pending`) and
//! open a second scheduler over the same, still-open storage. `join` is what a
//! close does to the scheduler; closing the backend is a separate step.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::durable::harness::scheduler::runtime::{TaskRuntime, TaskRuntimeError};
use pir::durable::harness::scheduler::{
    AbortResult, BlockedReason, NextTaskState, TaskInspectionState, TaskScheduler,
    TaskSchedulerOptions,
};
use pir::durable::harness::settings::{RestartLimit, Settings};
use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use pir::durable::ids::{ConversationId, TaskId};
use pir::durable::session::facade::ConversationOwnership;
use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    ConversationRecord, JoinPolicy, Storage, StorageWrite, TaskOutcome, TaskOutcomeError,
    TaskRecord, TaskState,
};
use pir::durable::Context;

// ── harness ────────────────────────────────────────────────────────────────

/// A fake clock the test drives.
#[derive(Clone, Debug)]
struct Clock(Arc<AtomicI64>);

impl Clock {
    fn new(at: i64) -> Self {
        Self(Arc::new(AtomicI64::new(at)))
    }

    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    fn advance(&self, by: i64) {
        self.0.fetch_add(by, Ordering::SeqCst);
    }
}

/// A report sink that records what the scheduler reported.
#[derive(Clone, Default)]
struct Reports {
    seen: Arc<Mutex<Vec<(String, pir::durable::ReportInfo)>>>,
}

impl Reports {
    fn push(&self, message: &str, info: pir::durable::ReportInfo) {
        self.seen.lock().unwrap().push((message.to_owned(), info));
    }

    fn len(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// Open a scheduler over `storage` with the test clock.
async fn open_scheduler(
    storage: Arc<MemoryStorage>,
    tasks: Vec<Task>,
    clock: &Clock,
    settings: Settings,
    reports: Option<Reports>,
) -> TaskScheduler {
    let clock_for_now = clock.clone();
    let settings = Arc::new(settings);
    let report: Option<pir::durable::harness::scheduler::ReportFn> = reports.map(|reports| {
        Arc::new(move |message: &str, info: pir::durable::ReportInfo| reports.push(message, info))
            as pir::durable::harness::scheduler::ReportFn
    });
    let options = TaskSchedulerOptions {
        now: Some(Arc::new(move || clock_for_now.now())),
        settings: Some(Arc::new(move || settings.as_ref().clone())),
        report,
        ..TaskSchedulerOptions::default()
    };
    TaskScheduler::open_with(options, storage, tasks)
        .await
        .expect("open scheduler")
}

/// A test world: storage, scheduler, clock, and one ownerless conversation.
struct World {
    storage: Arc<MemoryStorage>,
    scheduler: TaskScheduler,
    clock: Clock,
    reports: Reports,
    conversation: ConversationId,
}

impl World {
    async fn new(tasks: Vec<Task>) -> Self {
        Self::with_settings(tasks, Settings::default()).await
    }

    async fn with_settings(tasks: Vec<Task>, settings: Settings) -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let clock = Clock::new(1_000);
        let reports = Reports::default();
        let scheduler = open_scheduler(
            storage.clone(),
            tasks,
            &clock,
            settings,
            Some(reports.clone()),
        )
        .await;
        let conversation = scheduler
            .session()
            .create_conversation(
                ConversationOwnership::Ownerless,
                None,
                Context::background(),
            )
            .await
            .expect("create conversation")
            .id;
        Self {
            storage,
            scheduler,
            clock,
            reports,
            conversation,
        }
    }

    fn conversation(&self) -> ConversationId {
        self.conversation
    }

    /// Create a task of `kind` with `input`, in the test conversation.
    async fn create(&self, kind: &str, input: Value) -> TaskId {
        let catalogue = self.scheduler.catalogue().snapshot();
        let definition = catalogue.task(kind).expect("definition installed");
        self.scheduler
            .create_task(
                &definition,
                input,
                TaskOptions {
                    trace: None,
                    conversation_id: Some(self.conversation()),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create task")
    }

    async fn record(&self, id: TaskId) -> TaskRecord {
        self.scheduler
            .get_task(id)
            .await
            .expect("read task")
            .expect("task exists")
    }

    async fn wait_terminal(&self, id: TaskId) -> TaskRecord {
        let record = tokio::time::timeout(Duration::from_secs(5), self.scheduler.wait_for_task(id))
            .await
            .unwrap_or_else(|_| panic!("task {id} did not settle in time"))
            .expect("wait for task");
        assert!(
            matches!(record.state, TaskState::Terminal { .. }),
            "task {id} is {:?}, not terminal",
            record.state
        );
        record
    }

    /// Wait until `predicate` holds for the committed task.
    async fn wait_for<F>(&self, id: TaskId, mut predicate: F) -> TaskRecord
    where
        F: FnMut(&TaskRecord) -> bool,
    {
        for _ in 0..500 {
            let record = self.record(id).await;
            if predicate(&record) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "condition did not hold for task {id}; last record: {:?}",
            self.record(id).await
        );
    }
}

fn outcome_of(record: &TaskRecord) -> TaskOutcome {
    match &record.state {
        TaskState::Terminal { outcome } | TaskState::Completing { outcome } => outcome.clone(),
        other => panic!("expected a settled record, got {other:?}"),
    }
}

fn completed(record: &TaskRecord) -> bool {
    matches!(outcome_of(record), TaskOutcome::Completed { .. })
}

// ── task definitions ───────────────────────────────────────────────────────

/// The shared terminal commit helper: a phase that commits `outcome` and
/// returns.
fn terminal_handler(outcome: TaskOutcome) -> pir::durable::harness::task::PhaseHandler {
    phase_handler(
        move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
            let runtime = runtime.clone();
            let outcome = outcome.clone();
            Box::pin(async move {
                runtime
                    .commit(
                        move |_tx, _current| {
                            Box::pin(async move { Ok(Some(NextTaskState::Terminal { outcome })) })
                        },
                        context,
                    )
                    .await
                    .expect("terminal commit");
            })
        },
    )
}

/// `step`: commits `completed`.
fn step_task() -> Task {
    define_task("step", 1, |_input| json!({ "phase": "start" }))
        .phase(
            "start",
            terminal_handler(TaskOutcome::Completed {
                result: json!({ "ok": true }),
            }),
        )
        .build()
        .expect("valid task")
}

/// `throw`: panics inside its only phase.
fn throw_task() -> Task {
    define_task("throw", 1, |_input| json!({ "phase": "boom" }))
        .phase(
            "boom",
            phase_handler(|_t: &RunningTask, _r: &TaskRuntime, _c: Context| {
                Box::pin(async move {
                    panic!("handler exploded");
                })
            }),
        )
        .build()
        .expect("valid task")
}

/// `stubborn`: returns without committing — rule 6.
fn stubborn_task() -> Task {
    define_task("stubborn", 1, |_input| json!({ "phase": "again" }))
        .phase(
            "again",
            phase_handler(|_t: &RunningTask, _r: &TaskRuntime, _c: Context| {
                Box::pin(async move {})
            }),
        )
        .build()
        .expect("valid task")
}

/// `abortable`: waits for cancellation, and its abort handler commits
/// `aborted`.
fn abortable_task() -> Task {
    define_task("abortable", 1, |_input| json!({ "phase": "wait" }))
        .phase(
            "wait",
            phase_handler(|_t: &RunningTask, runtime: &TaskRuntime, _c: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime.cancelled().await;
                })
            }),
        )
        .abort(terminal_handler(TaskOutcome::Aborted {
            reason: Some("user".to_owned()),
            result: None,
        }))
        .build()
        .expect("valid task")
}

/// `failing`: commits `failed`.
fn failing_task() -> Task {
    define_task("failing", 1, |_input| json!({ "phase": "fail" }))
        .phase(
            "fail",
            terminal_handler(TaskOutcome::Failed {
                error: TaskOutcomeError {
                    message: "nope".to_owned(),
                    detail: None,
                },
                result: None,
            }),
        )
        .build()
        .expect("valid failing task")
}

/// `blocking`: waits for cancellation; its abort handler commits `aborted`.
fn blocking_task() -> Task {
    define_task("blocking", 1, |_input| json!({ "phase": "block" }))
        .phase(
            "block",
            phase_handler(|_t: &RunningTask, runtime: &TaskRuntime, _c: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime.cancelled().await;
                })
            }),
        )
        .abort(terminal_handler(TaskOutcome::Aborted {
            reason: Some("cascade".to_owned()),
            result: None,
        }))
        .build()
        .expect("valid blocking task")
}

/// `slow`: commits `completed` after a short real delay.
fn slow_task() -> Task {
    define_task("slow", 1, |_input| json!({ "phase": "finish" }))
        .phase(
            "finish",
            phase_handler(
                |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!({ "slow": true }),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("slow terminal");
                    })
                },
            ),
        )
        .build()
        .expect("valid slow task")
}

/// A waiter that spawns two children and waits on them with `policy`.
fn waiter_task(policy: JoinPolicy, left: &'static str, right: &'static str) -> Task {
    define_task("waiter", 1, |_input| json!({ "phase": "spawn" }))
        .phase(
            "spawn",
            phase_handler(
                move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let catalogue = runtime.registry();
                        let first = runtime
                            .create_task(
                                &catalogue.task(left).expect("installed"),
                                json!({}),
                                context.clone(),
                            )
                            .await
                            .expect("create first child");
                        let second = runtime
                            .create_task(
                                &catalogue.task(right).expect("installed"),
                                json!({}),
                                context.clone(),
                            )
                            .await
                            .expect("create second child");
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Waiting {
                                            checkpoint: json!({ "phase": "read" }),
                                            on: vec![first, second],
                                            policy,
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("wait");
                    })
                },
            ),
        )
        .phase(
            "read",
            terminal_handler(TaskOutcome::Completed {
                result: json!({ "resumed": true }),
            }),
        )
        .build()
        .expect("valid waiter")
}

// ── reserve → run → terminal ───────────────────────────────────────────────

#[tokio::test]
async fn reserve_run_terminal() {
    let world = World::new(vec![step_task()]).await;
    world.scheduler.resume();
    let id = world.create("step", json!({ "n": 1 })).await;
    let record = world.wait_terminal(id).await;
    assert!(completed(&record));
    assert_eq!(record.version, 1);
    assert_eq!(record.input, json!({ "n": 1 }));
    // A terminal record keeps no checkpoint and no memos (spec §5.3).
    assert!(record.memos.is_none());
    assert!(record.restarts.is_none());
    // Lifecycle stamps come from the harness clock.
    assert_eq!(record.started_at, Some(1_000));
    assert_eq!(record.ended_at, Some(1_000));
}

#[tokio::test]
async fn without_resume_nothing_is_reserved() {
    let world = World::new(vec![step_task()]).await;
    let id = world.create("step", json!({})).await;
    // `open` dispatches nothing; the task stays pending.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let record = world.record(id).await;
    assert!(
        matches!(record.state, TaskState::Pending { .. }),
        "expected pending, got {:?}",
        record.state
    );
}

// ── rule 4 / rule 6 ────────────────────────────────────────────────────────

#[tokio::test]
async fn phase_throw_faults() {
    let world = World::new(vec![throw_task()]).await;
    world.scheduler.resume();
    let id = world.create("throw", json!({})).await;
    let record = world.wait_terminal(id).await;
    match outcome_of(&record) {
        TaskOutcome::Faulted { error } => {
            assert!(
                error.message.contains("handler exploded"),
                "unexpected fault message: {}",
                error.message
            );
        }
        other => panic!("expected faulted, got {other:?}"),
    }
    assert!(world.reports.len() > 0, "a fault is reported");
}

#[tokio::test]
async fn phase_without_progress_faults() {
    let world = World::new(vec![stubborn_task()]).await;
    world.scheduler.resume();
    let id = world.create("stubborn", json!({})).await;
    let record = world.wait_terminal(id).await;
    match outcome_of(&record) {
        TaskOutcome::Faulted { error } => assert!(
            error.message.contains("without durable progress"),
            "unexpected message: {}",
            error.message
        ),
        other => panic!("expected faulted, got {other:?}"),
    }
}

// ── a runtime commit after the end rejects ─────────────────────────────────

#[tokio::test]
async fn commit_after_end_rejects() {
    // The first phase captures the runtime and advances; the second returns
    // without committing, so rule 6 faults inside the step that also ends the
    // invocation. By the time the terminal record is visible, the invocation
    // has ended — and a commit then rejects with `Ended` and lands nothing.
    let captured: Arc<Mutex<Option<TaskRuntime>>> = Arc::new(Mutex::new(None));
    let captured_in = captured.clone();
    let task = define_task("capture", 1, |_input| json!({ "phase": "first" }))
        .phase(
            "first",
            phase_handler(
                move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    *captured_in.lock().unwrap() = Some(runtime.clone());
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Running {
                                            checkpoint: json!({ "phase": "second" }),
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("advance");
                    })
                },
            ),
        )
        .phase(
            "second",
            phase_handler(|_t: &RunningTask, _r: &TaskRuntime, _c: Context| {
                Box::pin(async move {})
            }),
        )
        .build()
        .expect("valid task");

    let world = World::new(vec![task]).await;
    world.scheduler.resume();
    let id = world.create("capture", json!({})).await;
    let terminal = world.wait_terminal(id).await;
    assert!(matches!(outcome_of(&terminal), TaskOutcome::Faulted { .. }));

    let runtime = captured
        .lock()
        .unwrap()
        .clone()
        .expect("handler captured its runtime");
    let error = runtime
        .commit(
            |_tx, _current| {
                Box::pin(async move {
                    Ok(Some(NextTaskState::Terminal {
                        outcome: TaskOutcome::Failed {
                            error: TaskOutcomeError {
                                message: "second".to_owned(),
                                detail: None,
                            },
                            result: None,
                        },
                    }))
                })
            },
            Context::background(),
        )
        .await
        .expect_err("a commit after the end rejects");
    assert!(
        matches!(error, TaskRuntimeError::Ended { task } if task == id),
        "unexpected error: {error}"
    );
    // The first outcome is still the durable one.
    let after = world.record(id).await;
    assert_eq!(after, terminal);
}

#[tokio::test]
async fn memo_is_first_writer_wins() {
    // Two candidates in sequence: the second returns the durable winner.
    let task = define_task("memo", 1, |_input| json!({ "phase": "start" }))
        .phase(
            "start",
            phase_handler(
                |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let first = runtime
                            .memo_candidate("k".to_owned(), json!("first"), context.clone())
                            .await
                            .expect("first memo");
                        let second = runtime
                            .memo_candidate("k".to_owned(), json!("second"), context.clone())
                            .await
                            .expect("second memo");
                        assert_eq!(first, json!("first"));
                        assert_eq!(second, json!("first"));
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!({ "winner": second }),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("terminal");
                    })
                },
            ),
        )
        .build()
        .expect("valid task");
    let world = World::new(vec![task]).await;
    world.scheduler.resume();
    let id = world.create("memo", json!({})).await;
    let record = world.wait_terminal(id).await;
    assert_eq!(
        outcome_of(&record),
        TaskOutcome::Completed {
            result: json!({ "winner": "first" })
        }
    );
}

// ── abort: mark + abort handler ────────────────────────────────────────────

#[tokio::test]
async fn abort_runs_abort_handler() {
    let world = World::new(vec![abortable_task()]).await;
    world.scheduler.resume();
    let id = world.create("abortable", json!({})).await;
    world
        .wait_for(id, |record| {
            matches!(record.state, TaskState::Running { .. })
        })
        .await;
    let result = world.scheduler.abort(id).await.expect("abort");
    assert_eq!(result, AbortResult::Marked);
    let record = world.wait_terminal(id).await;
    match outcome_of(&record) {
        TaskOutcome::Aborted { reason, .. } => {
            assert_eq!(reason.as_deref(), Some("user"));
        }
        other => panic!("expected aborted, got {other:?}"),
    }
    assert!(record.abort_requested, "the mark survives into the receipt");
}

#[tokio::test]
async fn abort_of_unknown_task_rejects() {
    let world = World::new(vec![step_task()]).await;
    let error = world
        .scheduler
        .abort(TaskId::from_raw(9999))
        .await
        .expect_err("unknown task rejects");
    assert!(error.to_string().contains("does not exist"), "{error}");
}

// ── cascade: bottom-up ─────────────────────────────────────────────────────

#[tokio::test]
async fn cascade_marks_children_bottom_up() {
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let order_child = order.clone();
    let order_parent = order.clone();

    let child = define_task("child", 1, |_input| json!({ "phase": "run" }))
        .phase(
            "run",
            phase_handler(|_t: &RunningTask, runtime: &TaskRuntime, _c: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime.cancelled().await;
                })
            }),
        )
        .abort({
            let order = order_child.clone();
            phase_handler(
                move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let order = order.clone();
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        order.lock().unwrap().push("child".to_owned());
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Aborted {
                                                reason: Some("cascade".to_owned()),
                                                result: None,
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("child abort commit");
                    })
                },
            )
        })
        .build()
        .expect("valid child");

    let parent = define_task("parent", 1, |_input| json!({ "phase": "spawn" }))
        .phase(
            "spawn",
            phase_handler(
                |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let catalogue = runtime.registry();
                        let child_id = runtime
                            .create_task(
                                &catalogue.task("child").expect("child installed"),
                                json!({}),
                                context.clone(),
                            )
                            .await
                            .expect("create child");
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Waiting {
                                            checkpoint: json!({ "phase": "after" }),
                                            on: vec![child_id],
                                            policy: JoinPolicy::AllSettled,
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("wait on child");
                    })
                },
            ),
        )
        .phase(
            "after",
            terminal_handler(TaskOutcome::Completed {
                result: json!({ "joined": true }),
            }),
        )
        .abort({
            let order = order_parent.clone();
            phase_handler(
                move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let order = order.clone();
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        order.lock().unwrap().push("parent".to_owned());
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Aborted {
                                                reason: Some("user".to_owned()),
                                                result: None,
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("parent abort commit");
                    })
                },
            )
        })
        .build()
        .expect("valid parent");

    let world = World::new(vec![child, parent]).await;
    world.scheduler.resume();
    let parent_id = world.create("parent", json!({})).await;
    world
        .wait_for(parent_id, |record| {
            matches!(record.state, TaskState::Waiting { .. })
        })
        .await;
    world
        .scheduler
        .abort(parent_id)
        .await
        .expect("abort parent");
    let parent_record = world.wait_terminal(parent_id).await;
    assert!(matches!(
        outcome_of(&parent_record),
        TaskOutcome::Aborted { .. }
    ));
    let order = order.lock().unwrap().clone();
    assert_eq!(
        order,
        vec!["child".to_owned(), "parent".to_owned()],
        "the child's abort handler must run before the parent's"
    );
}

#[tokio::test]
async fn background_subtree_is_not_marked() {
    let world = World::new(vec![abortable_task()]).await;
    world.scheduler.resume();
    let catalogue = world.scheduler.catalogue().snapshot();
    let definition = catalogue.task("abortable").expect("installed");
    let background = world
        .scheduler
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                trace: None,
                conversation_id: Some(world.conversation()),
                ownership: TaskOwnership::Conversation,
                background: true,
                abandon_on_restart: false,
                submission_ids: Vec::new(),
            },
        )
        .await
        .expect("create background task");
    world
        .wait_for(background, |record| {
            matches!(record.state, TaskState::Running { .. })
        })
        .await;
    world
        .scheduler
        .abort_conversation(world.conversation(), false)
        .await
        .expect("abort conversation");
    let record = world.record(background).await;
    assert!(
        !record.abort_requested,
        "a background task is a boundary for ordinary abort"
    );
    assert!(matches!(record.state, TaskState::Running { .. }));
}

// ── completing hold then promotion ─────────────────────────────────────────

#[tokio::test]
async fn completing_hold_then_promotion() {
    let release = Arc::new(tokio::sync::Notify::new());
    let release_child = release.clone();

    let child = define_task("holder", 1, |_input| json!({ "phase": "hold" }))
        .phase(
            "hold",
            phase_handler(
                move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let release = release_child.clone();
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        release.notified().await;
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!({ "child": true }),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("child terminal");
                    })
                },
            ),
        )
        .build()
        .expect("valid child");

    let parent = define_task("finisher", 1, |_input| json!({ "phase": "spawn" }))
        .phase(
            "spawn",
            phase_handler(
                |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let catalogue = runtime.registry();
                        runtime
                            .create_task(
                                &catalogue.task("holder").expect("installed"),
                                json!({}),
                                context.clone(),
                            )
                            .await
                            .expect("create child");
                        runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!({ "parent": true }),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("parent terminal");
                    })
                },
            ),
        )
        .build()
        .expect("valid parent");

    let world = World::new(vec![child, parent]).await;
    world.scheduler.resume();
    let parent_id = world.create("finisher", json!({})).await;
    let held = world
        .wait_for(parent_id, |record| {
            matches!(record.state, TaskState::Completing { .. })
        })
        .await;
    assert!(completed(&held), "the held outcome is final");
    // Release the child; promotion follows its terminal commit.
    release.notify_waiters();
    let promoted = world.wait_terminal(parent_id).await;
    assert!(completed(&promoted));
}

// ── waits ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn wait_all_settled() {
    let world = World::new(vec![
        failing_task(),
        slow_task(),
        waiter_task(JoinPolicy::AllSettled, "failing", "slow"),
    ])
    .await;
    world.scheduler.resume();
    let id = world.create("waiter", json!({})).await;
    let record = world.wait_terminal(id).await;
    assert!(
        completed(&record),
        "allSettled resumes after a failed member, got {:?}",
        outcome_of(&record)
    );
}

#[tokio::test]
async fn wait_fail_fast_marks_siblings() {
    let world = World::new(vec![
        failing_task(),
        blocking_task(),
        waiter_task(JoinPolicy::FailFast, "failing", "blocking"),
    ])
    .await;
    world.scheduler.resume();
    let id = world.create("waiter", json!({})).await;
    let record = world.wait_terminal(id).await;
    assert!(
        completed(&record),
        "the waiter itself is never marked, got {:?}",
        outcome_of(&record)
    );
    // The blocking sibling was abort-marked by `failFast` and settled
    // `aborted`; the waiter itself was not marked.
    assert!(!record.abort_requested, "the waiter is never marked");
}

#[tokio::test]
async fn wait_cycle_rejected() {
    let child = define_task("cycle-child", 1, |_input| json!({ "phase": "wait" }))
        .phase(
            "wait",
            phase_handler(|_t: &RunningTask, _r: &TaskRuntime, _c: Context| {
                Box::pin(async move {})
            }),
        )
        .build()
        .expect("valid cycle child");

    let parent = define_task("cycle-parent", 1, |_input| json!({ "phase": "spawn" }))
        .phase(
            "spawn",
            phase_handler(
                |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let catalogue = runtime.registry();
                        let child = runtime
                            .create_task(
                                &catalogue.task("cycle-child").expect("installed"),
                                json!({}),
                                context.clone(),
                            )
                            .await
                            .expect("create child");
                        // Waiting on itself is the shortest cycle and must be
                        // rejected outright.
                        let result = runtime
                            .commit(
                                move |_tx, current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Waiting {
                                            checkpoint: json!({ "phase": "wait" }),
                                            on: vec![current.id, child],
                                            policy: JoinPolicy::AllSettled,
                                        }))
                                    })
                                },
                                context,
                            )
                            .await;
                        result.expect("wait on self must be rejected");
                    })
                },
            ),
        )
        .phase(
            "wait",
            phase_handler(|_t: &RunningTask, _r: &TaskRuntime, _c: Context| {
                Box::pin(async move {})
            }),
        )
        .build()
        .expect("valid cycle parent");

    let world = World::new(vec![child, parent]).await;
    world.scheduler.resume();
    let id = world.create("cycle-parent", json!({})).await;
    let record = world.wait_terminal(id).await;
    assert!(
        matches!(outcome_of(&record), TaskOutcome::Faulted { .. }),
        "expected the cycle rejection to fault the parent, got {:?}",
        outcome_of(&record)
    );
}

#[tokio::test]
async fn fail_fast_on_foreign_task_rejects() {
    // A waiter may `failFast` only on tasks it owns. Waiting on a
    // conversation-owned task with that policy is rejected, and the rejection
    // faults the waiter under rule 4.
    let foreign = define_task("foreign", 1, |_input| json!({ "phase": "wait" }))
        .phase(
            "wait",
            phase_handler(|_t: &RunningTask, runtime: &TaskRuntime, _c: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime.cancelled().await;
                })
            }),
        )
        .build()
        .expect("valid foreign task");

    let world = World::new(vec![foreign, slow_task()]).await;
    world.scheduler.resume();
    // A conversation-owned task the waiter will not own.
    let foreign_id = {
        let catalogue = world.scheduler.catalogue().snapshot();
        world
            .scheduler
            .create_task(
                &catalogue.task("foreign").expect("installed"),
                json!({}),
                TaskOptions {
                    trace: None,
                    conversation_id: Some(world.conversation()),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create foreign task")
    };
    world
        .wait_for(foreign_id, |record| {
            matches!(record.state, TaskState::Running { .. })
        })
        .await;

    let bad = define_task("bad-waiter", 1, |_input| json!({ "phase": "spawn" }))
        .phase(
            "spawn",
            phase_handler(
                move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        let result = runtime
                            .commit(
                                move |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Waiting {
                                            checkpoint: json!({ "phase": "wait" }),
                                            on: vec![foreign_id],
                                            policy: JoinPolicy::FailFast,
                                        }))
                                    })
                                },
                                context,
                            )
                            .await;
                        result
                            .expect("failFast on a task the waiter does not own must be rejected");
                    })
                },
            ),
        )
        .phase(
            "wait",
            phase_handler(|_t: &RunningTask, _r: &TaskRuntime, _c: Context| {
                Box::pin(async move {})
            }),
        )
        .build()
        .expect("valid waiter");
    world.scheduler.catalogue().install(vec![bad]);
    let id = world.create("bad-waiter", json!({})).await;
    let record = world.wait_terminal(id).await;
    assert!(
        matches!(outcome_of(&record), TaskOutcome::Faulted { .. }),
        "expected the failFast ownership check to fault the waiter, got {:?}",
        outcome_of(&record)
    );
}

// ── sleeping ───────────────────────────────────────────────────────────────

fn sleepy_task() -> Task {
    define_task("sleepy", 1, |_input| json!({ "phase": "sleep" }))
        .phase(
            "sleep",
            phase_handler(
                |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        runtime
                            .sleep_until(
                                runtime.now().expect("clock") + 60_000,
                                json!({ "phase": "wake" }),
                                context,
                            )
                            .await
                            .expect("sleep stored");
                    })
                },
            ),
        )
        .phase(
            "wake",
            terminal_handler(TaskOutcome::Completed {
                result: json!({ "slept": true }),
            }),
        )
        .build()
        .expect("valid task")
}

#[tokio::test]
async fn sleep_resumes_after_clock() {
    let world = World::new(vec![sleepy_task()]).await;
    world.scheduler.resume();
    let id = world.create("sleepy", json!({ "slept": true })).await;
    let sleeping = world
        .wait_for(id, |record| {
            matches!(record.state, TaskState::Sleeping { .. })
        })
        .await;
    let TaskState::Sleeping { until, .. } = sleeping.state else {
        unreachable!()
    };
    assert_eq!(until, 1_000 + 60_000);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(matches!(
        world.record(id).await.state,
        TaskState::Sleeping { .. }
    ));

    world.clock.advance(60_000);
    // `resume()` is the host wakeup: it makes due sleeps runnable.
    world.scheduler.resume();
    let record = world.wait_terminal(id).await;
    assert!(completed(&record));
    assert_eq!(record.input, json!({ "slept": true }));
}

#[tokio::test]
async fn sleep_survives_reopen() {
    let storage = Arc::new(MemoryStorage::new());
    let clock = Clock::new(5_000);
    let scheduler = open_scheduler(
        storage.clone(),
        vec![sleepy_task()],
        &clock,
        Settings::default(),
        None,
    )
    .await;
    let conversation = scheduler
        .session()
        .create_conversation(
            ConversationOwnership::Ownerless,
            None,
            Context::background(),
        )
        .await
        .expect("conversation")
        .id;
    scheduler.resume();
    let id = {
        let catalogue = scheduler.catalogue().snapshot();
        scheduler
            .create_task(
                &catalogue.task("sleepy").expect("installed"),
                json!({}),
                TaskOptions {
                    trace: None,
                    conversation_id: Some(conversation),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create")
    };
    let mut sleeping = None;
    for _ in 0..200 {
        let record = scheduler.get_task(id).await.expect("read").expect("exists");
        if let TaskState::Sleeping { .. } = record.state {
            sleeping = Some(record);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sleeping = sleeping.expect("task parked in a sleep");
    // Stop the scheduler. `join` writes no task back because the sleep holds
    // no invocation; the backend stays open for the reopen.
    scheduler.join().await;

    let reopened = open_scheduler(
        storage,
        vec![sleepy_task()],
        &clock,
        Settings::default(),
        None,
    )
    .await;
    let after = reopened.get_task(id).await.expect("read").expect("exists");
    assert_eq!(after.state, sleeping.state, "the deadline survives reopen");
    clock.advance(120_000);
    reopened.resume();
    let record = tokio::time::timeout(Duration::from_secs(5), reopened.wait_for_task(id))
        .await
        .expect("settled")
        .expect("wait");
    assert!(completed(&record));
}

// ── crash-loop guard ───────────────────────────────────────────────────────

/// One conversation record so a seeded task can reference it.
async fn seed_conversation(storage: &MemoryStorage) -> ConversationId {
    let id = ConversationId::from_raw(1);
    storage
        .commit(vec![StorageWrite::Conversation {
            value: ConversationRecord {
                id,
                parent: None,
                owner: None,
            },
        }])
        .await
        .expect("seed conversation");
    id
}

async fn write_running(
    storage: &MemoryStorage,
    id: TaskId,
    conversation: ConversationId,
    restarts: i64,
) {
    storage
        .commit(vec![StorageWrite::Task {
            value: TaskRecord {
                trace: None,
                links: Vec::new(),
                id,
                conversation_id: conversation,
                kind: "step".to_owned(),
                version: 1,
                input: json!({}),
                owner: None,
                background: false,
                abort_requested: false,
                abort_reason: None,
                abandon_on_restart: None,
                started_at: Some(1_000),
                ended_at: None,
                restarts: Some(restarts),
                state: TaskState::Running {
                    checkpoint: json!({ "phase": "start" }),
                },
                memos: None,
            },
        }])
        .await
        .expect("write running task");
}

#[tokio::test]
async fn crash_loop_parks_and_retry() {
    let storage = Arc::new(MemoryStorage::new());
    let clock = Clock::new(1_000);
    let conversation = seed_conversation(storage.as_ref()).await;
    let id = TaskId::from_raw(5000);
    write_running(storage.as_ref(), id, conversation, 2).await;

    let settings = Settings {
        restart_limit: RestartLimit::Limit(2),
        ..Settings::default()
    };
    let scheduler = open_scheduler(
        storage.clone(),
        vec![step_task()],
        &clock,
        settings,
        Some(Reports::default()),
    )
    .await;
    let record = scheduler.get_task(id).await.expect("read").expect("exists");
    assert_eq!(record.restarts, Some(3));
    assert!(matches!(record.state, TaskState::Pending { .. }));
    let inspection = scheduler.inspect().await.expect("inspect");
    let task = inspection
        .tasks
        .iter()
        .find(|task| task.record.id == id)
        .expect("task is live");
    assert_eq!(
        task.state,
        TaskInspectionState::Blocked {
            reason: BlockedReason::RestartLimit
        }
    );

    // It is not a candidate while parked.
    scheduler.resume();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(matches!(
        scheduler
            .get_task(id)
            .await
            .expect("read")
            .expect("exists")
            .state,
        TaskState::Pending { .. }
    ));

    // `retry_task()` clears the count and the task runs.
    scheduler.retry_task(id).await.expect("retry");
    let record = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(id))
        .await
        .expect("settled")
        .expect("wait");
    assert!(completed(&record));
    assert!(record.restarts.is_none());
}

// ── open counts, close does not ────────────────────────────────────────────

#[tokio::test]
async fn open_counts_restarts_close_does_not() {
    // Open rewrites a surviving `running` record to `pending` with
    // `restarts + 1`.
    let storage = Arc::new(MemoryStorage::new());
    let clock = Clock::new(1_000);
    let conversation = seed_conversation(storage.as_ref()).await;
    let id = TaskId::from_raw(6000);
    write_running(storage.as_ref(), id, conversation, 0).await;
    let scheduler = open_scheduler(
        storage.clone(),
        vec![step_task()],
        &clock,
        Settings::default(),
        None,
    )
    .await;
    let record = scheduler.get_task(id).await.expect("read").expect("exists");
    assert_eq!(record.restarts, Some(1), "open counts the crash");
    assert!(matches!(record.state, TaskState::Pending { .. }));
    scheduler.join().await;

    // Close (join) writes a stopped task back `pending` without a count.
    let world = World::new(vec![abortable_task()]).await;
    world.scheduler.resume();
    let running = world.create("abortable", json!({})).await;
    world
        .wait_for(running, |record| {
            matches!(record.state, TaskState::Running { .. })
        })
        .await;
    world.scheduler.join().await;

    let reopened = open_scheduler(
        world.storage.clone(),
        vec![abortable_task()],
        &world.clock,
        Settings::default(),
        None,
    )
    .await;
    let record = reopened
        .get_task(running)
        .await
        .expect("read")
        .expect("exists");
    assert!(
        matches!(record.state, TaskState::Pending { .. }),
        "close writes the stopped task back as pending, got {:?}",
        record.state
    );
    assert!(
        record.restarts.is_none(),
        "a close is not a crash and must not count a restart"
    );
}
