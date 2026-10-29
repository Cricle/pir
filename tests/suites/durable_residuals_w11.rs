//! W11-B durable residuals: the native telemetry adapter, the trace chain it
//! carries end to end, and pi-format export id uniqueness.
//!
//! | test | invariant |
//! |---|---|
//! | `disabled_core_installs_no_sink` | `HarnessTelemetry::from_telemetry(Telemetry::noop())` is `none`, so the default path keeps its zero-cost branch |
//! | `native_sink_forwards_spans_links_and_events` | a durable span becomes a core span with its parent, links, attributes and events |
//! | `native_sink_forwards_capped_error_status` | an error end forwards its category and the 500-character-capped diagnostic |
//! | `native_sink_forwards_metrics` | counter, up/down counter and histogram measurements reach the core instruments grouped by their declared attributes |
//! | `native_sink_carries_a_trace_across_an_inject_extract_round_trip` | a carrier taken from one span and extracted by another sink continues the same trace |
//! | `harness_submit_emits_into_the_core_and_persists_a_w3c_carrier` | `Harness::submit` emits `pi.durable.submit` into the core and writes a W3C carrier onto the submission record |
//! | `phase_span_parents_to_the_stored_w3c_carrier` | a phase span extracted from a stored carrier is a child of that trace in the core |
//! | `phase_span_links_every_creation_carrier` | the scheduler consumes `TaskRecord.links`: the first submission's carrier parents the phase span, every later one becomes a span link |
//! | `opaque_ids_never_collide_with_exported_entry_ids` | an opaque line's preserved id never doubles as a folded entry id |
//! | `duplicate_opaque_ids_are_made_distinct` | two opaque lines carrying one id are exported with distinct ids |
//! | `remapped_custom_message_and_branch_summary_round_trip` | non-text `custom_message` content and `branch_summary.fromId` survive import → remap → export → import |
//! | `latest_head_inside_a_tx_sees_writes_staged_before_it` | the boundary head is read from the revision the commit stages against, read-your-writes included |
//! | `latest_head_inside_a_tx_sees_a_concurrent_commit` | a head committed between a pre-read and the commit is still the boundary's start |
//! | `a_commit_reply_is_recovered_from_one_erasure` | the documented reply contract: a callback erases its value once and the caller gets its concrete type back |
//! | `a_double_erased_commit_reply_is_still_recovered` | a wrapper that erases the callback's `JobValue` a second time does not turn the right value into a type error |
//! | `a_commit_reply_of_the_wrong_type_is_rejected` | the typed reply still rejects a value the caller did not ask for |
//! | `runtime_watch_delivers_off_the_commit_line` | a runtime watch listener runs on its delivery task, not synchronously inside the committing line |
//! | `session_close_writes_stopped_tasks_back_pending` | the before-close hook is the production `write_at_close` caller that returns stopped tasks to `pending` |
//!
//! This file is not registered in `tests/g_durable.rs`; the foreman owns suite
//! registration.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use pir::durable::harness::harness::{Harness, HarnessOptions};
use pir::durable::harness::registry::create_registry;
use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::scheduler::{NextTaskState, TaskScheduler, TaskSchedulerOptions};
use pir::durable::harness::submissions::SubmissionDraft;
use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use pir::durable::harness::telemetry::{HarnessTelemetry, SpanHandle, SpanStatus, TraceCarrier};
use pir::durable::ids::{EntryId, ROOT_CONVERSATION_ID};
use pir::durable::pi_format::export::stable_pi_id;
use pir::durable::pi_format::{import_session_text, render_session, ExportInput, OpaqueEntry};
use pir::durable::session::facade::{ConversationOwnership, Session, SubmissionCreate};
use pir::durable::session::line::{JobValue, SessionOptions};
use pir::durable::session::transaction::{
    EntryDraft, HeadTarget, TaskOptions, TaskOwnership, TransactionScope,
};
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    EntryRecord, Storage, StorageWrite, SubmissionRecord, SubmissionStatus, SubmissionType,
    TaskOutcome, TaskState,
};
use pir::durable::{Context, ConversationId, TaskId};
use pir::session_manager::SessionHeader;
use pir::telemetry::{
    propagation, Carrier, SpanId, SpanStatus as CoreSpanStatus, Telemetry, TestTelemetry,
    TraceContext, TraceFlags, TraceId,
};

// ─── helpers ────────────────────────────────────────────────────────────────

/// A W3C carrier for one fresh trace, in the durable `TraceCarrier` shape.
fn w3c_carrier() -> (TraceId, TraceCarrier) {
    let context = TraceContext::new(TraceId::generate(), SpanId::generate(), TraceFlags::SAMPLED);
    let mut carrier = Carrier::new();
    assert!(propagation::inject(&context, &mut carrier));
    (context.trace_id(), TraceCarrier::from_pairs(carrier))
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

async fn open_scheduler_with_telemetry(
    telemetry: HarnessTelemetry,
    tasks: Vec<Task>,
) -> (TaskScheduler, Arc<MemoryStorage>) {
    let clock: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| 1_000);
    let options = TaskSchedulerOptions {
        now: Some(clock.clone()),
        session: Some(SessionOptions {
            now: Some(clock),
            telemetry: Some(telemetry),
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
                trace: None,
            },
        )
        .await
        .expect("create task")
}

// ─── the adapter: disabled path ─────────────────────────────────────────────

#[test]
fn disabled_core_installs_no_sink() {
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::noop());
    assert!(!telemetry.enabled(), "a disabled core installs no sink");
    assert!(
        telemetry.sink().is_none(),
        "no sink object exists on the default path"
    );
    assert_eq!(
        telemetry.start_phase_span(1, "t", "p", "run", None, false, None, &[]),
        SpanHandle::NONE
    );
    assert!(telemetry.inject(SpanHandle::NONE).is_none());
    assert!(telemetry.extract(None).is_none());
}

// ─── the adapter: spans, links, events, status ──────────────────────────────

#[test]
fn native_sink_forwards_spans_links_and_events() {
    let recorder = TestTelemetry::new();
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(recorder.clone()));
    assert!(telemetry.enabled());

    let (parent_trace, parent_carrier) = w3c_carrier();
    let parent = telemetry
        .extract(Some(&parent_carrier))
        .expect("the stored carrier extracts");
    let (link_trace, link_carrier) = w3c_carrier();
    let link = telemetry
        .extract(Some(&link_carrier))
        .expect("the link carrier extracts");

    let span = telemetry.start_phase_span(
        7,
        "generation",
        "start",
        "run",
        Some(2),
        false,
        Some(parent),
        &[pir::durable::harness::telemetry::SpanLink::new(link)],
    );
    assert!(!span.is_none());
    telemetry.checkpoint_changed(Some(span), "tools");
    telemetry.end_phase_span(
        span,
        "generation",
        "start",
        Some("next=tools"),
        None,
        None,
        SpanStatus::Ok,
        Some(12.5),
    );

    let phase = recorder.span("pi.durable.phase").expect("phase span");
    assert_eq!(phase.name, "pi.durable.phase");
    assert_eq!(
        phase.parent_trace_id,
        Some(parent_trace),
        "the extracted carrier is the span's parent trace"
    );
    assert_eq!(phase.links.len(), 1, "the durable link became a core link");
    assert_eq!(phase.links[0].context.trace_id(), link_trace);
    assert!(phase.ended, "end_span ends the core span");
    assert_eq!(
        phase
            .attributes
            .get("task.name")
            .and_then(|value| value.as_str()),
        Some("generation")
    );
    assert_eq!(
        phase
            .attributes
            .get("task.id")
            .and_then(|value| value.as_i64()),
        Some(7)
    );
    assert_eq!(
        phase
            .attributes
            .get("attempt")
            .and_then(|value| value.as_i64()),
        Some(2)
    );
    assert_eq!(
        phase
            .attributes
            .get("outcome")
            .and_then(|value| value.as_str()),
        Some("next=tools"),
        "end attributes are set before the span ends"
    );
    assert_eq!(
        phase
            .events
            .iter()
            .map(|event| event.name.as_str())
            .collect::<Vec<_>>(),
        vec!["pi.durable.checkpoint"],
        "the checkpoint event attaches to the phase span"
    );
    assert_eq!(
        phase.events[0]
            .attributes
            .get("phase")
            .and_then(|value| value.as_str()),
        Some("tools")
    );
    assert_eq!(phase.status, CoreSpanStatus::Ok);
}

#[test]
fn native_sink_forwards_capped_error_status() {
    let recorder = TestTelemetry::new();
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(recorder.clone()));

    let span = telemetry.start_phase_span(1, "t", "p", "run", None, false, None, &[]);
    let long = "x".repeat(900);
    telemetry.end_phase_span(
        span,
        "t",
        "p",
        Some("faulted"),
        None,
        None,
        SpanStatus::error_capped("task_faulted", &long),
        None,
    );

    let phase = recorder.span("pi.durable.phase").expect("phase span");
    let CoreSpanStatus::Error { error } = &phase.status else {
        panic!("the error status is forwarded, got {:?}", phase.status);
    };
    let error = error.as_ref().expect("an error payload");
    assert_eq!(error.name, "task_faulted");
    assert_eq!(
        error.message.chars().count(),
        pir::durable::harness::telemetry::MAX_ERROR_MESSAGE_CHARS,
        "the diagnostic stays capped at 500 characters"
    );
}

// ─── the adapter: metrics ───────────────────────────────────────────────────

#[test]
fn native_sink_forwards_metrics() {
    let recorder = TestTelemetry::new();
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(recorder.clone()));

    telemetry.report_raised("task_faulted");
    telemetry.report_raised("task_faulted");
    telemetry.tasks_active("generation", 1);
    telemetry.tasks_active("generation", -1);
    telemetry.phase_duration("generation", "start", "next=tools", 12.5);

    let reports = recorder
        .counter_snapshot("pi.durable.reports")
        .expect("the report counter exists");
    assert_eq!(reports.total, 2);
    assert_eq!(reports.points.len(), 1, "grouped by the report type");
    assert_eq!(
        reports.points[0]
            .attributes
            .get("type")
            .and_then(|value| value.as_str()),
        Some("task_faulted")
    );

    let active = recorder
        .up_down_counter_snapshot("pi.durable.tasks.active")
        .expect("the active-task counter exists");
    assert_eq!(active.total, 0, "+1 then -1 nets out");

    let duration = recorder
        .histogram_snapshot("pi.durable.phase.duration")
        .expect("the duration histogram exists");
    assert_eq!(duration.count, 1);
    assert_eq!(duration.sum, 12.5);
    assert_eq!(duration.unit.as_deref(), Some("ms"));
    assert_eq!(
        duration.points[0]
            .attributes
            .get("phase")
            .and_then(|value| value.as_str()),
        Some("start")
    );
}

// ─── the adapter: propagation ───────────────────────────────────────────────

#[test]
fn native_sink_carries_a_trace_across_an_inject_extract_round_trip() {
    let recorder = TestTelemetry::new();
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(recorder.clone()));

    let root = telemetry.start_submit_span("input", 3, None, &[]);
    let carrier = telemetry.inject(root).expect("an enabled span injects");
    assert!(
        carrier.get(pir::telemetry::TRACEPARENT_HEADER).is_some(),
        "the carrier is W3C traceparent"
    );

    // A second sink (a restarted process) reads the stored carrier.
    let other_recorder = TestTelemetry::new();
    let other = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(other_recorder.clone()));
    let parent = other.extract(Some(&carrier)).expect("the carrier extracts");
    let child = other.start_phase_span(1, "t", "p", "run", None, false, Some(parent), &[]);
    other.end_phase_span(child, "t", "p", None, None, None, SpanStatus::Ok, None);

    let submit = recorder.span("pi.durable.submit").expect("submit span");
    let phase = other_recorder.span("pi.durable.phase").expect("phase span");
    assert_eq!(
        phase.parent_trace_id,
        Some(submit.trace_id),
        "the extracted carrier continues the admission trace"
    );
    assert_eq!(phase.parent_span_id, Some(submit.span_id));

    telemetry.end_submit_span(root, None, None, SpanStatus::Ok);
}

// ─── end to end: Harness::submit ────────────────────────────────────────────

#[tokio::test]
async fn harness_submit_emits_into_the_core_and_persists_a_w3c_carrier() {
    let recorder = TestTelemetry::new();
    let options = HarnessOptions {
        registry: Some(Arc::new(create_registry())),
        now: Some(Arc::new(|| 2_000)),
        telemetry: Some(HarnessTelemetry::from_telemetry(Telemetry::from_recorder(
            recorder.clone(),
        ))),
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
    recorder.clear();

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

    let submit = recorder.span("pi.durable.submit").expect("submit span");
    assert_eq!(
        submit
            .attributes
            .get("submission.type")
            .and_then(|value| value.as_str()),
        Some("write")
    );
    assert_eq!(
        submit
            .attributes
            .get("conversation.id")
            .and_then(|value| value.as_i64()),
        Some(conversation.as_i64())
    );
    assert!(submit.ended, "the admission span is ended");

    let trace = record
        .trace
        .expect("the submission record persisted the admission carrier");
    assert!(
        trace.get(pir::telemetry::TRACEPARENT_HEADER).is_some(),
        "the stored carrier is W3C traceparent, so another process can extract it"
    );
    // The stored carrier extracts to the admission span's own context.
    let restarted =
        HarnessTelemetry::from_telemetry(Telemetry::from_recorder(TestTelemetry::new()));
    let parent = restarted
        .extract(Some(&trace))
        .expect("a fresh sink extracts the stored carrier");
    assert!(!parent.is_none());

    harness.close().await.expect("close");
}

// ─── end to end: the phase span of a scheduled task ─────────────────────────

#[tokio::test]
async fn phase_span_parents_to_the_stored_w3c_carrier() {
    let recorder = TestTelemetry::new();
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(recorder.clone()));
    let (scheduler, _storage) =
        open_scheduler_with_telemetry(telemetry, vec![completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;

    let (trace_id, carrier) = w3c_carrier();
    let submission = create_submission(&scheduler, conversation, carrier).await;
    let id = create_task(&scheduler, conversation, "done", vec![submission.id]).await;

    let created = scheduler
        .get_task(id)
        .await
        .expect("read task")
        .expect("task exists");
    assert!(
        created.trace.is_some(),
        "the submission carrier is the task trace"
    );

    recorder.clear();
    scheduler.resume();
    let settled = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(id))
        .await
        .expect("task settled in time")
        .expect("wait for task");
    assert!(matches!(settled.state, TaskState::Terminal { .. }));

    let phase = recorder.span("pi.durable.phase").expect("phase span");
    assert_eq!(
        phase.parent_trace_id,
        Some(trace_id),
        "the phase span is a child of the stored submission trace, not a root"
    );
    assert_eq!(
        phase
            .attributes
            .get("task.name")
            .and_then(|value| value.as_str()),
        Some("done")
    );
    assert!(phase.ended, "the phase span is ended with its settlement");

    scheduler.join().await;
}

// ─── end to end: creation links of a scheduled task ─────────────────────────

#[tokio::test]
async fn phase_span_links_every_creation_carrier() {
    // Upstream `Transaction.createTask`: the first submission's carrier is the
    // task's `trace`, every later one an immutable `links` entry. The scheduler
    // must consume both: the phase span parents to the trace and links every
    // creation carrier.
    let recorder = TestTelemetry::new();
    let telemetry = HarnessTelemetry::from_telemetry(Telemetry::from_recorder(recorder.clone()));
    let (scheduler, _storage) =
        open_scheduler_with_telemetry(telemetry, vec![completed_task("done")]).await;
    let conversation = open_conversation(&scheduler).await;

    let (trace_id, first_carrier) = w3c_carrier();
    let (link_trace_id, second_carrier) = w3c_carrier();
    let first = create_submission(&scheduler, conversation, first_carrier).await;
    let second = create_submission(&scheduler, conversation, second_carrier).await;
    let id = create_task(&scheduler, conversation, "done", vec![first.id, second.id]).await;

    let created = scheduler
        .get_task(id)
        .await
        .expect("read task")
        .expect("task exists");
    assert!(created.trace.is_some(), "the first carrier is the trace");
    assert_eq!(created.links.len(), 1, "every later carrier is a link");

    recorder.clear();
    scheduler.resume();
    let settled = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(id))
        .await
        .expect("task settled in time")
        .expect("wait for task");
    assert!(matches!(settled.state, TaskState::Terminal { .. }));

    let phase = recorder.span("pi.durable.phase").expect("phase span");
    assert_eq!(
        phase.parent_trace_id,
        Some(trace_id),
        "the phase span parents to the first submission's carrier"
    );
    assert_eq!(phase.links.len(), 1, "the creation link reached the core");
    assert_eq!(
        phase.links[0].context.trace_id(),
        link_trace_id,
        "the second submission's carrier is the span link"
    );
    scheduler.join().await;
}

// ─── pi format: opaque ids vs folded entry ids ──────────────────────────────

fn header(id: &str) -> String {
    json!({
        "type": "session",
        "version": 3,
        "id": id,
        "cwd": "C:\\repo",
        "timestamp": "1970-01-01T00:00:00.000Z",
    })
    .to_string()
}

fn session_header(id: &str) -> SessionHeader {
    SessionHeader::new(id, "C:\\repo", "1970-01-01T00:00:00.000Z".to_owned())
}

fn user_entry(id: i64, text: &str) -> EntryRecord {
    EntryRecord {
        id: EntryId::from_raw(id),
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "pi.user".to_owned(),
        model: Some(vec![json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
            "timestamp": 1_000,
        })]),
        data: None,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

#[test]
fn opaque_ids_never_collide_with_exported_entry_ids() {
    // `stable_pi_id` folds to the low 32 bits, so this entry's pi id is
    // exactly the literal below — the id the opaque line kept on import.
    let entry_id: i64 = 0x7777_7777;
    assert_eq!(stable_pi_id(entry_id), "77777777");

    let opaque = OpaqueEntry {
        id: "77777777".to_owned(),
        parent_id: None,
        timestamp: "1970-01-01T00:00:01.000Z".to_owned(),
        kind: "mystery_future".to_owned(),
        position: 0,
        raw: json!({
            "type": "mystery_future",
            "id": "77777777",
            "parentId": null,
            "timestamp": "1970-01-01T00:00:01.000Z",
            "payload": { "x": 1 },
        }),
    };
    let entries = vec![user_entry(entry_id, "hello")];
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: session_header("sess-collision"),
        entries: &entries,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: std::slice::from_ref(&opaque),
    };
    let rendered = render_session(&input).expect("render");
    let lines: Vec<Value> = rendered
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect();
    assert_eq!(lines.len(), 3, "header, opaque line, entry");

    let opaque_out = lines
        .iter()
        .find(|line| line["type"] == "mystery_future")
        .expect("the opaque line is re-emitted");
    assert_eq!(
        opaque_out["payload"],
        json!({ "x": 1 }),
        "the opaque payload stays verbatim"
    );
    let entry_out = lines
        .iter()
        .find(|line| line["type"] == "message")
        .expect("the message entry is exported");
    assert_ne!(
        entry_out["id"], opaque_out["id"],
        "the entry took a fresh id instead of shadowing the opaque line"
    );

    let ids: std::collections::BTreeSet<&str> = lines
        .iter()
        .filter(|line| line["type"] != "session")
        .filter_map(|line| line.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids.len(), 2, "every entry line carries a distinct id");
}

#[test]
fn duplicate_opaque_ids_are_made_distinct() {
    let opaque = |position: usize, extra: i64| OpaqueEntry {
        id: "abcdef01".to_owned(),
        parent_id: None,
        timestamp: "1970-01-01T00:00:01.000Z".to_owned(),
        kind: "mystery_future".to_owned(),
        position,
        raw: json!({
            "type": "mystery_future",
            "id": "abcdef01",
            "parentId": null,
            "timestamp": "1970-01-01T00:00:01.000Z",
            "payload": { "n": extra },
        }),
    };
    let entries: Vec<EntryRecord> = Vec::new();
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: session_header("sess-duplicates"),
        entries: &entries,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[opaque(0, 1), opaque(0, 2)],
    };
    let rendered = render_session(&input).expect("render");
    let lines: Vec<Value> = rendered
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect();
    assert_eq!(lines.len(), 3, "header and both opaque lines");
    assert_ne!(
        lines[1]["id"], lines[2]["id"],
        "two opaque lines never share an id"
    );
    assert_eq!(lines[1]["payload"], json!({ "n": 1 }));
    assert_eq!(lines[2]["payload"], json!({ "n": 2 }));
}

// ─── pi format: custom_message content and branch_summary fromId ────────────

fn lines_of(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect()
}

/// Turn imported drafts into records with caller-supplied ids (the ids a real
/// append would have minted), the same conversion a host performs.
fn records_with_ids(
    imported: &pir::durable::pi_format::ImportedSession,
    minted: &[EntryId],
) -> Vec<EntryRecord> {
    imported
        .entries
        .iter()
        .zip(minted)
        .map(|(draft, id)| EntryRecord {
            id: *id,
            conversation_id: ROOT_CONVERSATION_ID,
            kind: draft.kind.clone(),
            model: draft.model.clone(),
            data: draft.data.clone(),
            head: draft.head.map(|head| match head {
                HeadTarget::SelfEntry => *id,
                HeadTarget::Entry(target) => target,
            }),
            edits: draft.edits.clone(),
            by_task_id: None,
        })
        .collect()
}

#[test]
fn remapped_custom_message_and_branch_summary_round_trip() {
    let image_content = json!([
        { "type": "text", "text": "look at this" },
        { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
    ]);
    let text = [
        header("sess-w11"),
        json!({
            "type": "message",
            "id": "aaaaaaaa",
            "parentId": null,
            "timestamp": "1970-01-01T00:00:01.000Z",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "base prompt" }],
                "timestamp": 1_000,
            },
        })
        .to_string(),
        json!({
            "type": "custom_message",
            "id": "bbbbbbbb",
            "parentId": "aaaaaaaa",
            "timestamp": "1970-01-01T00:00:02.000Z",
            "customType": "inject",
            "content": image_content,
            "display": true,
            "details": { "k": 1 },
        })
        .to_string(),
        json!({
            "type": "branch_summary",
            "id": "cccccccc",
            "parentId": "bbbbbbbb",
            "timestamp": "1970-01-01T00:00:03.000Z",
            "fromId": "aaaaaaaa",
            "summary": "branched here",
        })
        .to_string(),
    ]
    .join("\n");

    let mut imported = import_session_text(&text).expect("import");
    assert!(imported.opaque.is_empty());
    assert_eq!(imported.entries.len(), 3);

    // The ids a real append never picks: not 1-based, not contiguous.
    let minted = [
        EntryId::from_raw(9_001),
        EntryId::from_raw(9_002),
        EntryId::from_raw(9_003),
    ];
    imported.remap_entry_ids(&minted).expect("remap");
    assert_eq!(imported.entry_ids["aaaaaaaa"], minted[0]);

    let records = records_with_ids(&imported, &minted);
    let labels = BTreeMap::new();
    let input = ExportInput {
        header: session_header("sess-w11"),
        entries: &records,
        agent: None,
        usage: None,
        session_info: None,
        labels: &labels,
        opaque: &[],
    };
    let rendered = render_session(&input).expect("render");
    let exported = lines_of(&rendered);

    let custom = exported
        .iter()
        .find(|line| line["type"] == "custom_message")
        .expect("the custom message is exported as one");
    assert_eq!(
        custom["content"], image_content,
        "non-text content blocks survive the round trip"
    );
    assert_eq!(custom["display"], true);
    assert_eq!(custom["details"], json!({ "k": 1 }));

    let branch = exported
        .iter()
        .find(|line| line["type"] == "branch_summary")
        .expect("the branch summary is exported");
    assert_eq!(
        branch["fromId"],
        json!(stable_pi_id(minted[0].as_i64())),
        "fromId addresses the exported line of the remapped target"
    );

    // The exported file imports again, and the branch still resolves to the
    // message it was cut from.
    let reimported = import_session_text(&rendered).expect("re-import");
    assert_eq!(reimported.entries.len(), 3);
    let branch_draft = reimported
        .entries
        .iter()
        .find(|draft| {
            draft.data.as_ref().and_then(|data| data.get("kind"))
                == Some(&Value::String("branch".to_owned()))
        })
        .expect("the branch summary round trips as a branch summary");
    let data = branch_draft.data.as_ref().expect("branch data");
    assert_eq!(data["fromId"], json!(stable_pi_id(minted[0].as_i64())));
    assert_eq!(
        data["fromEntry"],
        json!(reimported.entry_ids[&stable_pi_id(minted[0].as_i64())].as_i64()),
        "the durable target resolves to the message's new provisional id"
    );
    let custom_draft = reimported
        .entries
        .iter()
        .find(|draft| {
            draft.data.as_ref().and_then(|data| data.get("source"))
                == Some(&Value::String("custom_message".to_owned()))
        })
        .expect("the custom message round trips");
    let custom_data = custom_draft.data.as_ref().expect("custom data");
    assert_eq!(
        custom_data["content"], image_content,
        "the image block is still there after a second import"
    );
}

// ─── generation boundaries: `latest_head` inside the transaction ────────────

fn head_entry(id: i64, conversation: ConversationId) -> EntryRecord {
    EntryRecord {
        id: EntryId::from_raw(id),
        conversation_id: conversation,
        kind: "pi.user".to_owned(),
        model: Some(vec![json!({
            "role": "user",
            "content": [{ "type": "text", "text": "x" }],
            "timestamp": id,
        })]),
        data: None,
        head: Some(EntryId::from_raw(id)),
        edits: None,
        by_task_id: None,
    }
}

async fn open_session() -> (Session, ConversationId, Arc<MemoryStorage>) {
    let storage = Arc::new(MemoryStorage::new());
    let session = Session::open(storage.clone(), SessionOptions::default());
    let conversation = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            None,
            Context::background(),
        )
        .await
        .expect("create conversation")
        .id;
    (session, conversation, storage)
}

#[tokio::test]
async fn latest_head_inside_a_tx_sees_writes_staged_before_it() {
    // Read-your-writes: an entry the transaction already staged with a head
    // marker wins over the committed marker, so the boundary start is the
    // revision this commit produces. `Session::commit` attaches the backend to
    // the transaction (the line does), which is what makes the read run at all.
    let (session, conversation, _storage) = open_session().await;
    let ctx = Context::background();
    session
        .append_entry(
            conversation,
            EntryDraft {
                kind: "pi.user".to_owned(),
                model: Some(vec![json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "committed" }],
                    "timestamp": 1,
                })]),
                head: Some(HeadTarget::SelfEntry),
                ..EntryDraft::default()
            },
            ctx.clone(),
        )
        .await
        .expect("seed a committed head marker");

    let staged: i64 = 424_242;
    let scope = TransactionScope {
        conversation_id: Some(conversation),
        ..TransactionScope::default()
    };
    let head: Option<EntryId> = session
        .commit_scoped(scope, ctx.clone(), move |tx, _state| {
            Box::pin(async move {
                // Staged first, read second: the staged marker must win.
                tx.push_write(StorageWrite::Entry {
                    value: head_entry(staged, conversation),
                });
                let head = tx.latest_head(conversation).await?;
                Ok(Box::new(head) as JobValue)
            })
        })
        .await
        .expect("commit");
    assert_eq!(
        head,
        Some(EntryId::from_raw(staged)),
        "the staged head wins over the committed marker"
    );

    // A transaction without staged markers reads the committed one, so the
    // same call is also the boundary's committed-revision cut.
    let committed: Option<EntryId> = session
        .commit_scoped(
            TransactionScope {
                conversation_id: Some(conversation),
                ..TransactionScope::default()
            },
            ctx.clone(),
            move |tx, _state| {
                Box::pin(async move {
                    let head = tx.latest_head(conversation).await?;
                    Ok(Box::new(head) as JobValue)
                })
            },
        )
        .await
        .expect("read commit");
    assert_eq!(
        committed,
        Some(EntryId::from_raw(staged)),
        "the marker this commit just wrote is the next commit's committed head"
    );
    session.close().await.expect("close");
}

#[tokio::test]
async fn latest_head_inside_a_tx_sees_a_concurrent_commit() {
    // A head that lands between a phase's pre-read and the phase's commit is
    // still the boundary's start: the read runs inside the transaction, so it
    // observes the line's newest committed revision rather than a stale
    // pre-read. The pre-read value is captured first and must lose.
    let (session, conversation, _storage) = open_session().await;
    let ctx = Context::background();

    let append_head = |text: &'static str, timestamp: i64| {
        let ctx = ctx.clone();
        let session = session.clone();
        async move {
            session
                .append_entry(
                    conversation,
                    EntryDraft {
                        kind: "pi.user".to_owned(),
                        model: Some(vec![json!({
                            "role": "user",
                            "content": [{ "type": "text", "text": text }],
                            "timestamp": timestamp,
                        })]),
                        head: Some(HeadTarget::SelfEntry),
                        ..EntryDraft::default()
                    },
                    ctx,
                )
                .await
                .expect("append head")
                .id
        }
    };

    let pre_read = append_head("first", 1).await;
    // The concurrent commit lands before the task's boundary commits.
    let concurrent = append_head("second", 2).await;
    assert_ne!(pre_read, concurrent);

    let inside: Option<EntryId> = session
        .commit_scoped(
            TransactionScope {
                conversation_id: Some(conversation),
                ..TransactionScope::default()
            },
            ctx.clone(),
            move |tx, _state| {
                Box::pin(async move {
                    let head = tx.latest_head(conversation).await?;
                    Ok(Box::new(head) as JobValue)
                })
            },
        )
        .await
        .expect("commit");
    assert_eq!(
        inside,
        Some(concurrent),
        "the in-transaction read sees the concurrent head, not the stale pre-read {pre_read:?}"
    );
    session.close().await.expect("close");
}

// ─── the typed reply path (contract, not an implementation detail) ───────────

/// The documented contract: a commit callback erases its value **once**
/// (`Ok(Box::new(value) as JobValue)`) and the caller receives its concrete
/// type back. This is the shape every in-tree caller and both `latest_head`
/// tests use, so a regression here would silently break all of them with
/// "commit returned an unexpected value type".
#[tokio::test]
async fn a_commit_reply_is_recovered_from_one_erasure() {
    let (session, _conversation, _storage) = open_session().await;
    let head: Option<EntryId> = session
        .commit(Context::background(), move |_tx, _state| {
            Box::pin(async move { Ok(Box::new(Some(EntryId::from_raw(77))) as JobValue) })
        })
        .await
        .expect("the one-level reply is recovered");
    assert_eq!(head, Some(EntryId::from_raw(77)));
    session.close().await.expect("close");
}

/// A wrapper that boxes the callback's already-erased `JobValue` a second time
/// used to be rejected: the line downcast exactly once, so `Box<Box<T>>` failed
/// with "unexpected value type" even though the inner value was the one the
/// caller asked for. The typed reply accepts the value at either depth, because
/// which layer performed the erasure is not something a caller can observe —
/// both shapes carry the same value.
#[tokio::test]
async fn a_double_erased_commit_reply_is_still_recovered() {
    let (session, _conversation, _storage) = open_session().await;
    // `commit_scoped` passes the callback's `JobValue` through unchanged, so
    // this path is a single erasure.
    let scoped: EntryId = session
        .commit_scoped(
            TransactionScope::default(),
            Context::background(),
            move |_tx, _state| {
                Box::pin(async move { Ok(Box::new(EntryId::from_raw(9)) as JobValue) })
            },
        )
        .await
        .expect("the scoped reply is recovered");
    assert_eq!(scoped, EntryId::from_raw(9));

    // The shape a double erasure produces (`Box<Box<T>>`): a wrapper that
    // erases the callback's reply again, which is exactly what an
    // `IntoJobValue` blanket impl over an already-erased `JobValue` does.
    let erased: Option<EntryId> = session
        .handle()
        .commit(
            TransactionScope::default(),
            Context::background(),
            move |_tx, _state| {
                Box::pin(async move {
                    let inner: JobValue = Box::new(Some(EntryId::from_raw(5)));
                    let value: JobValue = Box::new(inner);
                    Ok(value)
                })
            },
        )
        .await
        .expect("a doubly erased reply is recovered");
    assert_eq!(erased, Some(EntryId::from_raw(5)));
    session.close().await.expect("close");
}

/// The relaxation above must not turn a genuinely wrong type into a pass: a
/// reply of a different concrete type is still rejected with the documented
/// diagnostic.
#[tokio::test]
async fn a_commit_reply_of_the_wrong_type_is_rejected() {
    let (session, _conversation, _storage) = open_session().await;
    let result: Result<EntryId, _> = session
        .commit(Context::background(), move |_tx, _state| {
            Box::pin(async move { Ok(Box::new(1_u32) as JobValue) })
        })
        .await;
    let error = result.expect_err("a u32 is not an EntryId");
    assert!(
        error
            .to_string()
            .contains("commit returned an unexpected value type"),
        "the wrong-type diagnostic is preserved, got {error}"
    );
    session.close().await.expect("close");
}

// ─── runtime document watches deliver off the commit line ───────────────────

/// A listener that counts frames and can block the delivery task, so a test can
/// prove the commit line is never the thread that runs it.
#[derive(Default)]
struct WatchProbe {
    frames: std::sync::atomic::AtomicUsize,
    block: std::sync::atomic::AtomicBool,
    /// The listener parks here while `block` is set. `None` until the test
    /// creates the release channel.
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl WatchProbe {
    fn frames(&self) -> usize {
        self.frames.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn block_until_released(&self) {
        let guard = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(receiver) = guard.as_ref() {
            // Bounded so a broken test cannot hang the suite forever.
            let _ = receiver.recv_timeout(Duration::from_secs(10));
        }
    }
}

/// A task whose phase watches `pi.usage` and stays pending until the test
/// signals it, so the test controls when a watched commit lands.
fn watching_task(probe: Arc<WatchProbe>, done: Arc<tokio::sync::Notify>) -> Task {
    define_task("watcher", 1, |_input| json!({ "phase": "watch" }))
        .phase(
            "watch",
            phase_handler(
                move |_task: &RunningTask, runtime: &TaskRuntime, context: Context| {
                    let runtime = runtime.clone();
                    let probe = probe.clone();
                    let done = done.clone();
                    Box::pin(async move {
                        let address =
                            pir::durable::harness::tool::usage_address(runtime.conversation_id())
                                .expect("usage address");
                        let watch = runtime
                            .watch_doc(&address, move |_value| {
                                probe
                                    .frames
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if probe.block.load(std::sync::atomic::Ordering::SeqCst) {
                                    probe.block_until_released();
                                }
                                true
                            })
                            .await
                            .expect("watch");
                        // Hold the invocation (and its watch) live until the
                        // test has driven its own watched commit.
                        done.notified().await;
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
                        watch.stop();
                    })
                },
            ),
        )
        .build()
        .expect("valid task")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_watch_delivers_off_the_commit_line() {
    let probe = Arc::new(WatchProbe::default());
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    *probe
        .release
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(release_rx);
    let done = Arc::new(tokio::sync::Notify::new());

    let (scheduler, _storage) = open_scheduler_with_telemetry(
        HarnessTelemetry::none(),
        vec![watching_task(probe.clone(), done.clone())],
    )
    .await;
    let conversation = open_conversation(&scheduler).await;
    // The conversation's built-in documents are staged by the scheduler's
    // recovery, but seed `pi.usage` explicitly so the watch always has an
    // incarnation to bind to regardless of when that staging lands.
    scheduler
        .session()
        .doc(
            pir::durable::harness::tool::usage_address(conversation).expect("usage address"),
            pir::durable::harness::usage::USAGE_DEFINITION.clone(),
            Context::background(),
            |value| {
                value.insert("tokens".to_owned(), json!(0));
            },
        )
        .await
        .expect("seed usage");
    let id = create_task(&scheduler, conversation, "watcher", Vec::new()).await;
    scheduler.resume();

    // Wait for the watch's initial frame, which `watch.start` delivers on the
    // phase's own thread.
    let started = tokio::time::timeout(Duration::from_secs(5), async {
        while probe.frames() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(started.is_ok(), "the watch delivered its initial frame");

    // Every later frame blocks the delivery task.
    probe.block.store(true, std::sync::atomic::Ordering::SeqCst);
    let resolved = pir::durable::harness::tool::usage_address(conversation).expect("usage address");
    let committed = tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.session().doc(
            resolved,
            pir::durable::harness::usage::USAGE_DEFINITION.clone(),
            Context::background(),
            |value| {
                value.insert("tokens".to_owned(), json!(7));
            },
        ),
    )
    .await;
    assert!(
        committed.is_ok(),
        "a watched commit must not wait for its listener: the listener runs off the line"
    );
    committed.expect("commit finished").expect("commit");

    // The frame was queued for the (parked) delivery task. A synchronous drain
    // on the commit line would have blocked the commit above instead, so the
    // commit completing proves delivery is off the line; the frame count proves
    // it was actually queued rather than dropped.
    let delivered = tokio::time::timeout(Duration::from_secs(3), async {
        while probe.frames() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        delivered.is_ok(),
        "the commit reached the watch: {} frames",
        probe.frames()
    );

    // Release the listener and settle the task. `block` is cleared first so
    // only the frame parked above ever waits.
    probe
        .block
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = release_tx.send(());
    done.notify_one();
    let settled = tokio::time::timeout(Duration::from_secs(5), scheduler.wait_for_task(id))
        .await
        .expect("task settled in time")
        .expect("wait for task");
    assert!(matches!(settled.state, TaskState::Terminal { .. }));
    scheduler.join().await;
}

// ─── production before-close caller ─────────────────────────────────────────

/// A temporary directory removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("pir-w11b-{label}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..20 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A task that waits for cancellation; its abort handler settles `aborted`.
fn wait_task() -> Task {
    define_task("waiter", 1, |_input| json!({ "phase": "wait" }))
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
                                            reason: Some("close".to_owned()),
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

#[tokio::test]
async fn session_close_writes_stopped_tasks_back_pending() {
    // Closing the session directly (not `TaskScheduler::close`) still runs the
    // scheduler's before-close hook, the production `write_at_close` caller: a
    // task left `running` when the session closes is written back `pending`
    // without counting a restart.
    let dir = TempDir::new("close-write-back");
    let storage = pir::durable::storage::jsonl::JsonlStorage::open(dir.path())
        .await
        .expect("open jsonl storage");
    let storage: Arc<dyn Storage> = Arc::new(storage);
    let options = TaskSchedulerOptions {
        now: Some(Arc::new(|| 1_000)),
        ..TaskSchedulerOptions::default()
    };
    let scheduler = TaskScheduler::open_with(options, storage, vec![wait_task()])
        .await
        .expect("open scheduler");
    let conversation = open_conversation(&scheduler).await;
    let id = create_task(&scheduler, conversation, "waiter", Vec::new()).await;
    scheduler.resume();
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let record = scheduler.get_task(id).await.expect("read").expect("task");
            if matches!(record.state, TaskState::Running { .. }) {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the task started running");
    assert_eq!(running.restarts, None);

    // The direct session close: the hook is the only path that can write back.
    scheduler.session().close().await.expect("close session");

    let reopened = pir::durable::storage::jsonl::JsonlStorage::open(dir.path())
        .await
        .expect("reopen jsonl storage");
    let record = reopened
        .task(id)
        .await
        .expect("read the task")
        .expect("the task survived the close");
    assert!(
        matches!(record.state, TaskState::Pending { .. }),
        "beforeClose wrote the stopped task back pending, got {:?}",
        record.state
    );
    assert!(
        record.restarts.is_none(),
        "a close is not a crash and must not count a restart"
    );
    reopened.close().await.expect("close reopened");
}
