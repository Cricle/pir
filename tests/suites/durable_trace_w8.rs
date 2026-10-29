//! W8-A durable tracing residuals: the submit carrier, the task carrier and
//! its creation-time links, and child-task carrier threading.
//!
//! | test | invariant |
//! |---|---|
//! | `submit_span_carrier_reaches_the_submission_record` | `Harness::submit` injects its span into the admission commit, so the submission record persists it |
//! | `phase_span_parents_to_the_task_trace_and_links_its_links` | `TaskRecord.trace` is the phase span's parent and every `TaskRecord.links` carrier becomes a span link |
//! | `creation_links_are_not_repeated_after_a_restart` | `TaskRecord.links` is read through `Invocation.links`, which is empty once `started_at` says the task already ran |
//! | `child_task_threads_the_parent_carrier` | a child created by a handler carries the parent task's carrier instead of `trace: None` |
//! | `child_created_without_a_carrier_inherits_the_owner_at_dispatch` | a child record created with `trace: None` inherits the owner's carrier at reservation, so its phase span is not a root |
//! | `abort_phase_span_parents_to_the_task_trace` | abort phase spans use the same carrier as run phase spans |
//! | `phase_span_is_a_root_without_a_carrier` | a task with no trace starts a root phase span with no links (the `None` no-op path) |
//! | `telemetry_disabled_leaves_task_carriers_empty` | with no sink no carrier is persisted, so the disabled path stays allocation-free |
//!
//! This file is intentionally not registered in `tests/g_durable.rs`; the
//! foreman owns the aggregator.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::durable::harness::harness::{Harness, HarnessOptions};
use pir::durable::harness::registry::create_registry;
use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::scheduler::{
    AbortResult, NextTaskState, TaskScheduler, TaskSchedulerOptions,
};
use pir::durable::harness::submissions::SubmissionDraft;
use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use pir::durable::harness::telemetry::{
    RecordedAttributeValue, RecordedSpan, RecordingTelemetry, SpanHandle, TraceCarrier,
};
use pir::durable::session::facade::{ConversationOwnership, SubmissionCreate};
use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
use pir::durable::session::SessionOptions;
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    Storage, SubmissionRecord, SubmissionStatus, SubmissionType, TaskOutcome, TaskRecord, TaskState,
};
use pir::durable::{Context, ConversationId, TaskId};

/// A carrier the recording sink can extract (its inject format is one
/// `pi.recorded.span` entry holding the span id).
fn carrier(span: u64) -> TraceCarrier {
    TraceCarrier::from_pairs([(RecordingTelemetry::CARRIER_KEY, span.to_string())])
}

fn object(value: Value) -> pir::durable::JsonObject {
    value.as_object().cloned().expect("a JSON object")
}

/// The `task.id` start attribute of a recorded span.
fn span_task_id(span: &RecordedSpan) -> Option<i64> {
    span.start_attributes.iter().find_map(|attribute| {
        if attribute.key != "task.id" {
            return None;
        }
        match &attribute.value {
            RecordedAttributeValue::Integer(id) => Some(*id),
            _ => None,
        }
    })
}

/// A string start attribute of a recorded span.
fn span_attr<'a>(span: &'a RecordedSpan, key: &str) -> Option<&'a str> {
    span.start_attributes.iter().find_map(|attribute| {
        if attribute.key != key {
            return None;
        }
        match &attribute.value {
            RecordedAttributeValue::String(value) => Some(value.as_str()),
            _ => None,
        }
    })
}

/// A task whose only phase commits `completed`.
fn completed_task(name: &str) -> Task {
    define_task(name, 1, |_input| json!({ "phase": "start" }))
        .phase(
            "start",
            phase_handler(
                |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        runtime
                            .commit(
                                |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!({ "ok": true }),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("terminal commit");
                    })
                },
            ),
        )
        .build()
        .expect("valid task")
}

/// A task that waits for cancellation and settles `aborted` through its abort
/// handler.
fn abortable_task(name: &str) -> Task {
    define_task(name, 1, |_input| json!({ "phase": "wait" }))
        .phase(
            "wait",
            phase_handler(
                |_task: &RunningTask, runtime: &TaskRuntime, _context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        runtime.cancelled().await;
                    })
                },
            ),
        )
        .abort(phase_handler(
            |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime
                        .commit(
                            |_tx, _current| {
                                Box::pin(async move {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Aborted {
                                            reason: Some("test".to_owned()),
                                            result: None,
                                        },
                                    }))
                                })
                            },
                            context,
                        )
                        .await
                        .expect("abort commit");
                })
            },
        ))
        .build()
        .expect("valid task")
}

async fn create_submission(
    scheduler: &TaskScheduler,
    conversation: ConversationId,
    trace: TraceCarrier,
) -> SubmissionRecord {
    let mut create = SubmissionCreate::new(conversation, SubmissionType::Write);
    create.status = SubmissionStatus::Queued;
    create.trace = Some(trace);
    scheduler
        .session()
        .create_submission(create, Context::background())
        .await
        .expect("create submission")
}

/// Open a paused scheduler whose session emits into `recording`.
async fn open_scheduler_async(
    recording: &RecordingTelemetry,
    tasks: Vec<Task>,
) -> (TaskScheduler, Arc<MemoryStorage>) {
    let clock: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| 1_000);
    let options = TaskSchedulerOptions {
        now: Some(clock.clone()),
        session: Some(SessionOptions {
            now: Some(clock),
            telemetry: Some(recording.handle()),
            ..SessionOptions::default()
        }),
        ..TaskSchedulerOptions::default()
    };
    let storage = Arc::new(MemoryStorage::new());
    let scheduler = TaskScheduler::open_with(options, storage.clone(), tasks)
        .await
        .expect("open scheduler");
    (scheduler, storage)
}

/// Open a paused scheduler without a telemetry sink.
async fn open_scheduler_without_telemetry(tasks: Vec<Task>) -> (TaskScheduler, Arc<MemoryStorage>) {
    let clock: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| 1_000);
    let options = TaskSchedulerOptions {
        now: Some(clock.clone()),
        session: Some(SessionOptions {
            now: Some(clock),
            ..SessionOptions::default()
        }),
        ..TaskSchedulerOptions::default()
    };
    let storage = Arc::new(MemoryStorage::new());
    let scheduler = TaskScheduler::open_with(options, storage.clone(), tasks)
        .await
        .expect("open scheduler");
    (scheduler, storage)
}

async fn open_conversation(scheduler: &TaskScheduler) -> ConversationId {
    scheduler
        .session()
        .create_conversation(
            ConversationOwnership::Ownerless,
            None,
            Context::background(),
        )
        .await
        .expect("create conversation")
        .id
}

async fn create_task(
    scheduler: &TaskScheduler,
    conversation: ConversationId,
    kind: &str,
    trace: Option<TraceCarrier>,
    submission_ids: Vec<pir::durable::SubmissionId>,
) -> TaskId {
    let definition = scheduler
        .catalogue()
        .snapshot()
        .task(kind)
        .expect("definition installed");
    scheduler
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                conversation_id: Some(conversation),
                ownership: TaskOwnership::Conversation,
                background: false,
                abandon_on_restart: false,
                submission_ids,
                trace,
            },
        )
        .await
        .expect("create task")
}

async fn read_task(scheduler: &TaskScheduler, id: TaskId) -> TaskRecord {
    scheduler
        .get_task(id)
        .await
        .expect("read task")
        .expect("task exists")
}

async fn wait_terminal(scheduler: &TaskScheduler, id: TaskId) -> TaskRecord {
    let record = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(id))
        .await
        .expect("task settled in time")
        .expect("wait for task");
    assert!(
        matches!(record.state, TaskState::Terminal { .. }),
        "task {id} is {:?}, not terminal",
        record.state
    );
    record
}

#[tokio::test]
async fn submit_span_carrier_reaches_the_submission_record() {
    let recording = RecordingTelemetry::new();
    let options = HarnessOptions {
        registry: Some(Arc::new(create_registry())),
        now: Some(Arc::new(|| 2_000)),
        telemetry: Some(recording.handle()),
        ..HarnessOptions::default()
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let harness = Harness::open(storage, options, Context::background())
        .await
        .expect("open harness");
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create conversation")
        .id();
    recording.clear();

    let record = harness
        .submit(
            conversation,
            SubmissionDraft::write(object(json!({
                "kind": "app.note",
                "data": { "text": "carrier" }
            }))),
            ctx,
        )
        .await
        .expect("submit");
    assert_eq!(record.status, SubmissionStatus::Done);

    let submit_span = recording
        .spans()
        .into_iter()
        .find(|span| span.descriptor.name == "pi.durable.submit")
        .expect("the submit span was started");
    let trace = record
        .trace
        .expect("the submission record persisted the submit span's carrier");
    let expected = submit_span.handle.id().to_string();
    assert_eq!(
        trace.get(RecordingTelemetry::CARRIER_KEY),
        Some(expected.as_str()),
        "the submission carries the admission span, not its caller"
    );

    harness.close().await.expect("close");
}

#[tokio::test]
async fn phase_span_parents_to_the_task_trace_and_links_its_links() {
    let recording = RecordingTelemetry::new();
    let (scheduler, _storage) =
        open_scheduler_async(&recording, vec![completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;

    let first = create_submission(&scheduler, conversation, carrier(901)).await;
    let second = create_submission(&scheduler, conversation, carrier(902)).await;
    let id = create_task(
        &scheduler,
        conversation,
        "done",
        None,
        vec![first.id, second.id],
    )
    .await;

    // F29: the first submission's carrier is the task trace; later ones are
    // creation-time links.
    let created = read_task(&scheduler, id).await;
    assert_eq!(created.trace, Some(carrier(901)));
    assert_eq!(created.links, vec![carrier(902)]);

    recording.clear();
    scheduler.resume();
    let settled = wait_terminal(&scheduler, id).await;
    assert!(matches!(
        settled.state,
        pir::durable::TaskState::Terminal { .. }
    ));

    let phase = recording
        .spans()
        .into_iter()
        .find(|span| span.descriptor.name == "pi.durable.phase")
        .expect("the phase span was started");
    assert_eq!(
        phase.parent,
        Some(SpanHandle::new(901)),
        "the task trace carrier is the phase span's parent"
    );
    assert_eq!(
        phase
            .links
            .iter()
            .map(|link| link.context)
            .collect::<Vec<_>>(),
        vec![SpanHandle::new(902)],
        "every creation-time link is a span link"
    );

    scheduler.join().await;
}

#[tokio::test]
async fn creation_links_are_not_repeated_after_a_restart() {
    let recording = RecordingTelemetry::new();
    let (scheduler, _storage) =
        open_scheduler_async(&recording, vec![completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;

    let first = create_submission(&scheduler, conversation, carrier(911)).await;
    let second = create_submission(&scheduler, conversation, carrier(912)).await;
    let id = create_task(
        &scheduler,
        conversation,
        "done",
        None,
        vec![first.id, second.id],
    )
    .await;

    // A record whose `started_at` is already set represents a task reserved in
    // an earlier process: upstream `Invocation.links` is `undefined` then, so
    // the creation spans must not be repeated on the restarted run.
    let mut created = read_task(&scheduler, id).await;
    assert_eq!(created.links, vec![carrier(912)]);
    created.started_at = Some(1);
    scheduler
        .session()
        .set_task(created, Context::background())
        .await
        .expect("mark the task as started");

    recording.clear();
    scheduler.resume();
    let _ = wait_terminal(&scheduler, id).await;

    let phase = recording
        .spans()
        .into_iter()
        .find(|span| span.descriptor.name == "pi.durable.phase")
        .expect("the phase span was started");
    assert_eq!(
        phase.parent,
        Some(SpanHandle::new(911)),
        "the stored trace still parents the restarted run"
    );
    assert!(
        phase.links.is_empty(),
        "creation-time links belong to the first run only"
    );

    scheduler.join().await;
}

#[tokio::test]
async fn child_task_threads_the_parent_carrier() {
    let recording = RecordingTelemetry::new();
    let child_slot: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));

    // The owner stays `running` while its child is created and dispatched, so
    // the creation commit sees a live parent invocation carrier.
    let slot = child_slot.clone();
    let holder = define_task("holder", 1, |_input| json!({ "phase": "hold" }))
        .phase(
            "hold",
            phase_handler(
                move |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    let slot = slot.clone();
                    Box::pin(async move {
                        let child = runtime.registry().task("done").expect("child definition");
                        let id = runtime
                            .create_task(&child, json!({}), context.clone())
                            .await
                            .expect("create child task");
                        *slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
                        runtime.cancelled().await;
                    })
                },
            ),
        )
        .abort(phase_handler(
            |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime
                        .commit(
                            |_tx, _current| {
                                Box::pin(async move {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Aborted {
                                            reason: Some("test".to_owned()),
                                            result: None,
                                        },
                                    }))
                                })
                            },
                            context,
                        )
                        .await
                        .expect("abort commit");
                })
            },
        ))
        .build()
        .expect("valid holder task");

    let (scheduler, _storage) =
        open_scheduler_async(&recording, vec![holder, completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;
    let parent_carrier = carrier(4_242);
    let parent = create_task(
        &scheduler,
        conversation,
        "holder",
        Some(parent_carrier.clone()),
        Vec::new(),
    )
    .await;
    recording.clear();
    scheduler.resume();

    // Wait for the handler to publish the child id.
    let mut child = None;
    for _ in 0..500 {
        if let Some(id) = *child_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            child = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child = child.expect("the parent created its child");

    let created = read_task(&scheduler, child).await;
    assert_eq!(
        created.trace,
        Some(parent_carrier),
        "the child's creation record carries the parent carrier, not None"
    );
    assert!(
        created.links.is_empty(),
        "a child created without submission ids has no links"
    );

    let settled = wait_terminal(&scheduler, child).await;
    assert!(matches!(
        settled.state,
        pir::durable::TaskState::Terminal { .. }
    ));

    // And its phase span parents to that carrier.
    let phase = recording
        .spans()
        .into_iter()
        .find(|span| {
            span.descriptor.name == "pi.durable.phase" && span_task_id(span) == Some(child.as_i64())
        })
        .expect("the child's phase span was started");
    assert_eq!(phase.parent, Some(SpanHandle::new(4_242)));

    let _ = scheduler.abort(parent).await.expect("abort parent");
    scheduler.join().await;
}

#[tokio::test]
async fn child_created_without_a_carrier_inherits_the_owner_at_dispatch() {
    let recording = RecordingTelemetry::new();
    let (scheduler, _storage) = open_scheduler_async(
        &recording,
        vec![abortable_task("holder"), completed_task("done")],
    )
    .await;
    let conversation = open_conversation(&scheduler).await;
    let parent = create_task(
        &scheduler,
        conversation,
        "holder",
        Some(carrier(5_151)),
        Vec::new(),
    )
    .await;
    recording.clear();
    scheduler.resume();

    // A host that creates a child record directly can pass `trace: None`; the
    // reservation pass is the safety net that inherits the owner's carrier.
    // Wait until the owner is live so task assembly accepts the child.
    for _ in 0..500 {
        if matches!(
            read_task(&scheduler, parent).await.state,
            TaskState::Running { .. }
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let definition = scheduler
        .catalogue()
        .snapshot()
        .task("done")
        .expect("definition installed");
    let child = scheduler
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                conversation_id: Some(conversation),
                ownership: TaskOwnership::Task(parent),
                background: false,
                abandon_on_restart: false,
                submission_ids: Vec::new(),
                trace: None,
            },
        )
        .await
        .expect("create child");

    let settled = wait_terminal(&scheduler, child).await;
    assert_eq!(
        settled.trace,
        Some(carrier(5_151)),
        "reservation stored the owner carrier on the child record"
    );

    let phase = recording
        .spans()
        .into_iter()
        .find(|span| {
            span.descriptor.name == "pi.durable.phase" && span_task_id(span) == Some(child.as_i64())
        })
        .expect("the child's phase span was started");
    assert_eq!(
        phase.parent,
        Some(SpanHandle::new(5_151)),
        "a child created with trace: None still parents to the owner chain"
    );

    let _ = scheduler.abort(parent).await.expect("abort parent");
    scheduler.join().await;
}

#[tokio::test]
async fn abort_phase_span_parents_to_the_task_trace() {
    let recording = RecordingTelemetry::new();
    let (scheduler, _storage) =
        open_scheduler_async(&recording, vec![abortable_task("abortable")]).await;
    let conversation = open_conversation(&scheduler).await;
    let parent_carrier = carrier(777);
    let id = create_task(
        &scheduler,
        conversation,
        "abortable",
        Some(parent_carrier),
        Vec::new(),
    )
    .await;
    recording.clear();
    scheduler.resume();

    // Wait until the run phase started, then abort.
    for _ in 0..500 {
        let record = read_task(&scheduler, id).await;
        if matches!(record.state, TaskState::Running { .. })
            && recording
                .spans()
                .iter()
                .any(|span| span_attr(span, "kind") == Some("run"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let result = scheduler.abort(id).await.expect("abort");
    assert_eq!(result, AbortResult::Marked);
    let _ = wait_terminal(&scheduler, id).await;

    let abort = recording
        .spans()
        .into_iter()
        .find(|span| {
            span.descriptor.name == "pi.durable.phase" && span_attr(span, "kind") == Some("abort")
        })
        .expect("the abort phase span was started");
    assert_eq!(
        abort.parent,
        Some(SpanHandle::new(777)),
        "abort phase spans use the task's stored carrier"
    );

    scheduler.join().await;
}

#[tokio::test]
async fn phase_span_is_a_root_without_a_carrier() {
    let recording = RecordingTelemetry::new();
    let (scheduler, _storage) =
        open_scheduler_async(&recording, vec![completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;
    let id = create_task(&scheduler, conversation, "done", None, Vec::new()).await;
    recording.clear();
    scheduler.resume();
    let _ = wait_terminal(&scheduler, id).await;

    let phase = recording
        .spans()
        .into_iter()
        .find(|span| span.descriptor.name == "pi.durable.phase")
        .expect("the phase span was started");
    assert_eq!(
        phase.parent, None,
        "a task without a trace starts a root phase span"
    );
    assert!(phase.links.is_empty(), "a task without links has no links");

    scheduler.join().await;
}

#[tokio::test]
async fn telemetry_disabled_leaves_task_carriers_empty() {
    let child_slot: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let slot = child_slot.clone();
    let holder = define_task("holder", 1, |_input| json!({ "phase": "hold" }))
        .phase(
            "hold",
            phase_handler(
                move |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    let slot = slot.clone();
                    Box::pin(async move {
                        let child = runtime.registry().task("done").expect("child definition");
                        let id = runtime
                            .create_task(&child, json!({}), context.clone())
                            .await
                            .expect("create child task");
                        *slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
                        runtime.cancelled().await;
                    })
                },
            ),
        )
        .build()
        .expect("valid holder task");

    let (scheduler, _storage) =
        open_scheduler_without_telemetry(vec![holder, completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;
    let parent = create_task(&scheduler, conversation, "holder", None, Vec::new()).await;
    scheduler.resume();

    let mut child = None;
    for _ in 0..500 {
        if let Some(id) = *child_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            child = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child = child.expect("the parent created its child");

    assert!(
        read_task(&scheduler, parent).await.trace.is_none(),
        "without a sink the parent persists no carrier"
    );
    assert!(
        read_task(&scheduler, child).await.trace.is_none(),
        "a None parent carrier stays None and costs nothing"
    );
    let _ = scheduler.abort(parent).await.expect("abort parent");
    scheduler.join().await;
}
