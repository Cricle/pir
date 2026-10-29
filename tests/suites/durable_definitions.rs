//! Conformance tests for document definitions, report info and timers.
//!
//! These cover the behaviour-carrying half of the durable definitions port:
//! `initial` / `migrate` / `checkpointWhen` from upstream `documents.ts`,
//! `ReportInfo` from `types.ts` and the `SetTimer` hook from `timers.ts`.

use pir::durable::{
    define_doc, define_doc_family, delay_steps, CommonDocDefinition, DocCheckpointWhen,
    DocDefinition, DocFamilyDefinition, DocMigrate, DocTokenKind, DocumentError, DocumentFork,
    DocumentHistory, DocumentRecord, DocumentScope, DocumentSemantics, HostCallback, ReportInfo,
    ReportTask, SetTimer, SetTimerFn, StorageError, TaskFaultReason, TaskOutdatedReason,
    TimerCancel, TimerOptions, Timers, MAX_TIMER_DELAY_MS,
};
use pir::durable::{ConversationId, DocumentId, Seq, TaskId};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── helpers ────────────────────────────────────────────────────────────────

fn initial_session() -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("count".to_owned(), json!(0));
    value
}

fn migrate_v1_to_v2(from: i64, value: Value) -> Result<Value, DocumentError> {
    if from != 1 {
        return Err(DocumentError::new(format!("unexpected version {from}")));
    }
    let mut object = value.as_object().cloned().unwrap_or_default();
    let count = object.get("count").and_then(Value::as_i64).unwrap_or(0);
    object.insert("count".to_owned(), json!(count + 10));
    object.insert("migrated".to_owned(), json!(true));
    Ok(Value::Object(object))
}

fn session_definition(version: i64) -> CommonDocDefinition {
    CommonDocDefinition::new("demo", version, DocumentSemantics::Session).expect("valid")
}

fn conversation_record(kind: &str) -> DocumentRecord {
    DocumentRecord {
        id: DocumentId::from_raw(1),
        kind: kind.to_owned(),
        key: None,
        created_at: Seq::from_raw(1),
        retired_at: None,
        scope: DocumentScope::Session,
        history: None,
        fork: None,
    }
}

/// A host timer that records requests and only arms the timers a test asks for.
struct RecordingTimer {
    calls: Mutex<Vec<(i64, TimerOptions)>>,
    decline: bool,
}

impl RecordingTimer {
    fn new(decline: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            decline,
        })
    }

    fn delays(&self) -> Vec<(i64, TimerOptions)> {
        self.calls.lock().unwrap().clone()
    }
}

impl SetTimer for RecordingTimer {
    fn set(
        &self,
        _callback: pir::durable::TimerCallback,
        delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel> {
        self.calls.lock().unwrap().push((delay_ms, options));
        if self.decline {
            return None;
        }
        // Never fires: the test only inspects what the wrapper asked for.
        Some(TimerCancel::new(Box::new(|| {})))
    }
}

fn no_report() -> Arc<dyn Fn(String) + Send + Sync> {
    Arc::new(|_| {})
}

// ── definitions: validation ─────────────────────────────────────────────────

#[test]
fn version_must_be_a_positive_integer() {
    assert!(DocDefinition::session("demo", 0).is_err());
    assert!(DocFamilyDefinition::session("demo", -1).is_err());
    assert!(CommonDocDefinition::session("demo", 0).is_err());
    assert!(DocDefinition::session("demo", 1).is_ok());
    assert!(CommonDocDefinition::session("demo", 1).is_ok());
}

#[test]
fn identity_definition_lifts_into_behaviour_form() {
    let identity = DocDefinition::session("demo", 2).expect("valid");
    let behaviour = identity.behaviour(initial_session, None, None);
    assert_eq!(behaviour.common.kind, "demo");
    assert_eq!(behaviour.common.version, 2);
    assert_eq!(behaviour.initial_value().get("count"), Some(&json!(0)));
    assert!(!behaviour.has_migration());
    assert!(!behaviour.forces_checkpoint(&initial_session()));
    assert_eq!(behaviour.common.semantics, DocumentSemantics::Session);
}

#[test]
fn family_definition_is_marked_as_family() {
    let identity =
        DocFamilyDefinition::conversation_latest("board", 1, DocumentFork::Current).expect("valid");
    let behaviour = identity.behaviour(initial_session, None, None);
    assert!(behaviour.is_family());
    assert_eq!(behaviour.common.semantics.scope_kind(), "conversation");
    assert_eq!(
        behaviour.common.semantics.history(),
        Some(DocumentHistory::Latest)
    );
}

#[test]
fn checkpoint_when_some_and_none() {
    fn always(_: &Map<String, Value>) -> bool {
        true
    }
    let forced =
        session_definition(1).behaviour(initial_session, None, Some(always as DocCheckpointWhen));
    assert!(forced.forces_checkpoint(&initial_session()));

    let never = session_definition(1).behaviour(initial_session, None, None);
    assert!(!never.forces_checkpoint(&initial_session()));
}

#[test]
fn migrate_runs_for_older_versions_only() {
    let definition = session_definition(2).behaviour(
        initial_session,
        Some(migrate_v1_to_v2 as DocMigrate),
        None,
    );

    // Older stored version with a migration: accepted and transformed.
    let migrated = definition
        .migrate_value(json!({"count": 1}), 1)
        .expect("older version migrates");
    assert_eq!(migrated, json!({"count": 11, "migrated": true}));

    // Same version: returned unchanged, migration not called.
    assert_eq!(
        definition
            .migrate_value(json!({"count": 1}), 2)
            .expect("current"),
        json!({"count": 1})
    );

    // Newer stored version: rejected.
    assert!(definition.migrate_value(json!({}), 3).is_err());
    assert!(definition
        .check_record_version(DocumentId::from_raw(1), "demo", 3)
        .is_err());
}

#[test]
fn older_version_without_migrate_is_rejected() {
    let definition = session_definition(2).behaviour(initial_session, None, None);
    assert!(definition
        .check_record_version(DocumentId::from_raw(1), "demo", 1)
        .is_err());
    assert!(definition.migrate_value(json!({"count": 1}), 1).is_err());
    assert!(definition
        .check_record_version(DocumentId::from_raw(1), "demo", 2)
        .is_ok());
}

#[test]
fn definition_materialize_applies_migration_and_checks_scope() {
    let definition = session_definition(2).behaviour(
        initial_session,
        Some(migrate_v1_to_v2 as DocMigrate),
        None,
    );
    let record = conversation_record("demo");
    let value = definition
        .materialize_value(&record, 1, initial_session())
        .expect("migrates");
    assert_eq!(value.get("count"), Some(&json!(10)));
    assert_eq!(value.get("migrated"), Some(&json!(true)));

    // A conversation record does not match a session definition.
    let mut mismatched = record.clone();
    mismatched.scope = DocumentScope::Conversation {
        conversation_id: ConversationId::from_raw(1),
    };
    mismatched.history = Some(DocumentHistory::Latest);
    mismatched.fork = Some(DocumentFork::Current);
    assert!(definition
        .materialize_value(&mismatched, 1, initial_session())
        .is_err());
}

#[test]
fn tokens_derive_their_owner_kind() {
    let session = define_doc(session_definition(1).behaviour(initial_session, None, None));
    assert_eq!(session.kind, DocTokenKind::Session);

    let task = define_doc(CommonDocDefinition::task("demo", 1).unwrap().behaviour(
        initial_session,
        None,
        None,
    ));
    assert_eq!(task.kind, DocTokenKind::Task);

    let family = define_doc_family(
        DocFamilyDefinition::conversation_rewindable("board", 1, DocumentFork::AsOf)
            .unwrap()
            .behaviour(initial_session, None, None),
    );
    assert_eq!(family.kind, DocTokenKind::Conversation);
}

// ── ReportInfo ──────────────────────────────────────────────────────────────

fn report_task() -> ReportTask {
    ReportTask {
        id: TaskId::from_raw(7),
        kind: "tool".to_owned(),
        conversation_id: ConversationId::from_raw(3),
    }
}

fn info_json(info: &ReportInfo) -> Value {
    serde_json::to_value(info).expect("serialize")
}

#[test]
fn report_info_type_strings_match_upstream() {
    let task = report_task();
    let cases: Vec<(ReportInfo, &str)> = vec![
        (ReportInfo::SessionFailed, "session_failed"),
        (
            ReportInfo::TaskFaulted {
                task: task.clone(),
                extension: None,
                reason: TaskFaultReason::Threw,
            },
            "task_faulted",
        ),
        (
            ReportInfo::TaskParked {
                task: task.clone(),
                extension: None,
            },
            "task_parked",
        ),
        (
            ReportInfo::MigrationFailed {
                task: task.clone(),
                extension: None,
            },
            "migration_failed",
        ),
        (
            ReportInfo::TaskOutdated {
                task: task.clone(),
                extension: None,
                reason: TaskOutdatedReason::MissingTask,
            },
            "task_outdated",
        ),
        (
            ReportInfo::TaskError {
                task: task.clone(),
                extension: None,
            },
            "task_error",
        ),
        (
            ReportInfo::ExtensionError {
                extension: None,
                task: None,
                conversation_id: None,
            },
            "extension_error",
        ),
        (
            ReportInfo::HostError {
                callback: HostCallback::Models,
                task: None,
            },
            "host_error",
        ),
        (ReportInfo::ListenerError, "listener_error"),
    ];
    for (info, expected) in cases {
        let value = info_json(&info);
        assert_eq!(value.get("type").and_then(Value::as_str), Some(expected));
        // Round-trips.
        let decoded: ReportInfo = serde_json::from_value(value).expect("decode");
        assert_eq!(decoded, info);
    }
}

#[test]
fn report_info_uses_camel_case_keys_and_numeric_ids() {
    let info = ReportInfo::TaskFaulted {
        task: report_task(),
        extension: Some("pi.todos".to_owned()),
        reason: TaskFaultReason::RestartLimit,
    };
    assert_eq!(
        info_json(&info),
        json!({
            "type": "task_faulted",
            "task": {"id": 7, "kind": "tool", "conversationId": 3},
            "extension": "pi.todos",
            "reason": "restart_limit",
        })
    );

    let host = ReportInfo::HostError {
        callback: HostCallback::OnWake,
        task: Some(report_task()),
    };
    assert_eq!(
        info_json(&host),
        json!({
            "type": "host_error",
            "callback": "onWake",
            "task": {"id": 7, "kind": "tool", "conversationId": 3},
        })
    );

    let extension = ReportInfo::ExtensionError {
        extension: Some("pi.view".to_owned()),
        task: None,
        conversation_id: Some(ConversationId::from_raw(9)),
    };
    assert_eq!(
        info_json(&extension),
        json!({
            "type": "extension_error",
            "extension": "pi.view",
            "conversationId": 9,
        })
    );
}

#[test]
fn report_info_omits_absent_optional_fields() {
    let value = info_json(&ReportInfo::TaskParked {
        task: report_task(),
        extension: None,
    });
    assert_eq!(
        value,
        json!({
            "type": "task_parked",
            "task": {"id": 7, "kind": "tool", "conversationId": 3},
        })
    );
    assert!(!value.as_object().unwrap().contains_key("extension"));
    assert_eq!(
        info_json(&ReportInfo::ListenerError),
        json!({"type": "listener_error"})
    );
}

#[test]
fn host_callback_names_match_upstream() {
    let names: Vec<&str> = [
        HostCallback::Now,
        HostCallback::Settings,
        HostCallback::Registry,
        HostCallback::Env,
        HostCallback::Models,
        HostCallback::SetTimer,
        HostCallback::OnWake,
    ]
    .into_iter()
    .map(HostCallback::name)
    .collect();
    assert_eq!(
        names,
        vec!["now", "settings", "registry", "env", "models", "setTimer", "onWake"]
    );
    for callback in [
        HostCallback::Now,
        HostCallback::SetTimer,
        HostCallback::OnWake,
    ] {
        let encoded = serde_json::to_value(callback).expect("serialize");
        assert_eq!(encoded, Value::String(callback.name().to_owned()));
    }
}

// ── timers ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn foreground_timer_fires() {
    let timers = Timers::native(no_report());
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    let _handle = timers.foreground(
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
        5,
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_stops_a_native_timer() {
    let timers = Timers::native(no_report());
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    let handle = timers.foreground(
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
        30,
    );
    assert!(!handle.is_cancelled());
    handle.cancel();
    assert!(handle.is_cancelled());
    handle.cancel(); // idempotent
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn declined_background_timer_returns_none_and_never_fires() {
    let host = RecordingTimer::new(true);
    let timers = Timers::host_timers(Some(host.clone() as Arc<dyn SetTimer>), no_report());
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    let handle = timers.background(
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
        5,
    );
    assert!(handle.is_none(), "declined background timer has no handle");
    assert_eq!(host.delays(), vec![(5, TimerOptions::BACKGROUND)]);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        fired.load(Ordering::SeqCst),
        0,
        "declined timer never fires"
    );
}

#[tokio::test]
async fn declined_foreground_timer_falls_back_and_reports_once() {
    let host = RecordingTimer::new(true);
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&reports);
    let timers = Timers::host_timers(
        Some(host.clone()),
        Arc::new(move |message: String| sink.lock().unwrap().push(message)),
    );
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let first = timers.foreground(Arc::clone(&callback), 5);
    let _second = timers.foreground(Arc::clone(&callback), 5);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fired.load(Ordering::SeqCst), 2, "both fell back and fired");
    assert!(!first.is_cancelled());
    let reported = reports.lock().unwrap().clone();
    assert_eq!(reported.len(), 1, "the fallback is reported once");
    assert!(reported[0].contains("using the native timer"));
    // The host saw both requests and neither was declined into a handle.
    assert_eq!(host.delays().len(), 2);
}

#[tokio::test]
async fn background_native_timer_is_armed() {
    // Rust has no `unref()`; the native implementation accepts background
    // timers rather than silently dropping the work.
    let timers = Timers::native(no_report());
    let handle = timers.background(Arc::new(|| {}), 1);
    assert!(handle.is_some());
}

#[test]
fn huge_delays_are_chunked() {
    assert_eq!(
        delay_steps(MAX_TIMER_DELAY_MS + 5).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS, 5]
    );
    let steps: Vec<i64> = delay_steps(2 * MAX_TIMER_DELAY_MS + 1).collect();
    assert_eq!(steps, vec![MAX_TIMER_DELAY_MS, MAX_TIMER_DELAY_MS, 1]);
    assert_eq!(steps.iter().sum::<i64>(), 2 * MAX_TIMER_DELAY_MS + 1);
    assert!(steps.iter().all(|step| *step <= MAX_TIMER_DELAY_MS));
}

#[tokio::test]
async fn host_receives_the_full_delay_unchunked() {
    // Chunking is the native implementation's job, matching upstream: a host
    // timer that supports long delays gets the requested value verbatim.
    let host = RecordingTimer::new(false);
    let timers = Timers::host_timers(Some(host.clone() as Arc<dyn SetTimer>), no_report());
    let huge = MAX_TIMER_DELAY_MS + 123;
    let handle = timers.foreground(Arc::new(|| {}), huge);
    assert!(!handle.is_cancelled());
    assert_eq!(host.delays(), vec![(huge, TimerOptions::FOREGROUND)]);
}

#[test]
fn set_timer_fn_adapts_a_closure() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let host = SetTimerFn(move |_callback, delay_ms: i64, options| {
        sink.lock().unwrap().push((delay_ms, options));
        None
    });
    let timers = Timers::new(Some(Arc::new(host) as Arc<dyn SetTimer>), no_report());
    let handle = timers.background(Arc::new(|| {}), 42);
    assert!(handle.is_none());
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![(42, TimerOptions::BACKGROUND)]
    );
}

#[test]
fn document_error_converts_to_invalid_storage_error() {
    let error = DocumentError::new("bad shape");
    assert_eq!(
        StorageError::from(error.clone()),
        StorageError::invalid("bad shape")
    );
    assert_eq!(error.to_string(), "bad shape");
}
