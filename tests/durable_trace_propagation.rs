//! Trace propagation across the durable harness: submit span -> submission
//! record -> task record -> phase span, plus F29 creation-time links.
//!
//! This is a standalone integration target (the `tests/suites/durable_*`
//! aggregator is foreman-owned); it exercises only the public durable API.
//!
//! | test | invariant |
//! |---|---|
//! | `submit_persists_the_submit_span_carrier` | `Harness::submit` injects its span into the admission commit, so the submission record persists it |
//! | `phase_span_parents_to_the_task_trace_and_links_its_links` | a task's `trace` is the phase span's parent and its `links` become span links |
//! | `child_task_inherits_its_owners_trace_carrier` | a child created with `trace: None` inherits its owner's first-submission carrier at dispatch |
//! | `task_record_trace_and_links_preserve_the_wire_shape` | `trace`/`links` are omitted when absent and old JSON reads as empty |

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::durable::harness::harness::{Harness, HarnessOptions};
use pir::durable::harness::registry::create_registry;
use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::scheduler::{NextTaskState, TaskScheduler, TaskSchedulerOptions};
use pir::durable::harness::submissions::SubmissionDraft;
use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use pir::durable::harness::telemetry::{RecordingTelemetry, SpanHandle, TraceCarrier};
use pir::durable::session::facade::{ConversationOwnership, SubmissionCreate};
use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
use pir::durable::session::SessionOptions;
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    Storage, SubmissionRecord, SubmissionStatus, SubmissionType, TaskOutcome, TaskRecord,
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

#[tokio::test]
async fn submit_persists_the_submit_span_carrier() {
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
    let definition = scheduler
        .catalogue()
        .snapshot()
        .task("done")
        .expect("definition installed");
    let id = scheduler
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                conversation_id: Some(conversation),
                ownership: TaskOwnership::Conversation,
                background: false,
                abandon_on_restart: false,
                submission_ids: vec![first.id, second.id],
                trace: None,
            },
        )
        .await
        .expect("create task");

    // F29: the first submission's carrier is the task trace; later ones are
    // creation-time links.
    let created = scheduler
        .get_task(id)
        .await
        .expect("read")
        .expect("task exists");
    assert_eq!(created.trace, Some(carrier(901)));
    assert_eq!(created.links, vec![carrier(902)]);

    recording.clear();
    scheduler.resume();
    let settled = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(id))
        .await
        .expect("task settled in time")
        .expect("wait for task");
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
async fn child_task_inherits_its_owners_trace_carrier() {
    let recording = RecordingTelemetry::new();
    let child_slot: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));

    // The owner stays `running` while its child is created and dispatched, so
    // the dispatch-time inheritance has a live owner record to read.
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
        open_scheduler_async(&recording, vec![holder, completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;
    let parent_carrier = carrier(4_242);
    let definition = scheduler
        .catalogue()
        .snapshot()
        .task("holder")
        .expect("definition installed");
    let _parent = scheduler
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                conversation_id: Some(conversation),
                ownership: TaskOwnership::Conversation,
                background: false,
                abandon_on_restart: false,
                submission_ids: Vec::new(),
                trace: Some(parent_carrier.clone()),
            },
        )
        .await
        .expect("create parent");
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

    let settled = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(child))
        .await
        .expect("child settled in time")
        .expect("wait for child");
    assert_eq!(
        settled.trace,
        Some(parent_carrier),
        "the child inherited its owner's first-submission carrier"
    );

    // And its phase span parents to that carrier.
    let phase = recording
        .spans()
        .into_iter()
        .find(|span| {
            span.descriptor.name == "pi.durable.phase" && span_task_id(span) == Some(child.as_i64())
        })
        .expect("the child's phase span was started");
    assert_eq!(phase.parent, Some(SpanHandle::new(4_242)));

    scheduler.join().await;
}

/// The `task.id` start attribute of a recorded span.
fn span_task_id(span: &pir::durable::harness::telemetry::RecordedSpan) -> Option<i64> {
    span.start_attributes.iter().find_map(|attribute| {
        if attribute.key != "task.id" {
            return None;
        }
        match &attribute.value {
            pir::durable::harness::telemetry::RecordedAttributeValue::Integer(id) => Some(*id),
            _ => None,
        }
    })
}

#[test]
fn task_record_trace_and_links_preserve_the_wire_shape() {
    // Old JSON without either field reads as empty.
    let old: TaskRecord = serde_json::from_value(json!({
        "id": 1,
        "conversationId": 2,
        "kind": "done",
        "version": 1,
        "input": null,
        "background": false,
        "abortRequested": false,
        "state": { "status": "pending", "checkpoint": null }
    }))
    .expect("old JSON reads");
    assert!(old.trace.is_none());
    assert!(old.links.is_empty());

    // Absent fields are omitted; present ones are written as objects.
    let encoded = serde_json::to_value(&old).expect("encode");
    assert!(encoded.get("trace").is_none(), "trace is omitted when None");
    assert!(
        encoded.get("links").is_none(),
        "links are omitted when empty"
    );

    let mut with_trace = old;
    with_trace.trace = Some(carrier(7));
    with_trace.links = vec![carrier(8)];
    let encoded = serde_json::to_value(&with_trace).expect("encode");
    assert_eq!(
        encoded
            .get("trace")
            .and_then(|trace| trace.get(RecordingTelemetry::CARRIER_KEY))
            .and_then(Value::as_str),
        Some("7")
    );
    assert_eq!(
        encoded.get("links").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
}
