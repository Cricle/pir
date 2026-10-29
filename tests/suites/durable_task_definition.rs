//! `durable::harness::task` — task definitions, validation and migration.
//!
//! Ports upstream `src/tasks.ts`'s `defineTask` and the `TaskDefinition` /
//! `RunningTask` surface from `harness/types.ts`. This suite covers:
//!
//! * `define_task`/`TaskBuilder` validation and the `TaskDefinitionError`
//!   variants (including the duplicate-phase hole described below);
//! * `Migrate::Checkpoint` and `Migrate::Full`;
//! * `RunningTask::from_record` / `for_abort` / `phase`;
//! * `TaskDefinition::phase_for` / `knows_phase` / `can_reserve_version`;
//! * the erased [`ErasedTask`] surface.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use std::any::Any;
use std::sync::Arc;

use serde_json::{json, Value};

use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::task::{
    define_task, phase_handler, Migrate, Migrated, PhaseHandler, RunningTask, TaskDefinitionError,
};
use pir::durable::harness::telemetry::TraceCarrier;
use pir::durable::ids::{ConversationId, TaskId};
use pir::durable::types::{JoinPolicy, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState};
use pir::durable::Context;

// ── helpers ────────────────────────────────────────────────────────────────

fn initial(_input: &Value) -> Value {
    json!({ "phase": "start" })
}

fn noop_handler() -> PhaseHandler {
    phase_handler(
        |_task: &RunningTask, _runtime: &TaskRuntime, _context: Context| Box::pin(async {}),
    )
}

fn completed() -> TaskOutcome {
    TaskOutcome::Completed {
        result: json!({ "ok": true }),
    }
}

fn failed() -> TaskOutcome {
    TaskOutcome::Failed {
        error: TaskOutcomeError {
            message: "boom".to_owned(),
            detail: None,
        },
        result: None,
    }
}

fn record(state: TaskState) -> TaskRecord {
    let mut memos = serde_json::Map::new();
    memos.insert("note".to_owned(), json!("kept"));

    TaskRecord {
        trace: None,
        links: Vec::new(),
        id: TaskId::from_raw(7),
        conversation_id: ConversationId::from_raw(3),
        kind: "demo.task".to_owned(),
        version: 2,
        input: json!({ "q": 1 }),
        owner: None,
        background: true,
        abort_requested: true,
        abort_reason: Some("restart".to_owned()),
        abandon_on_restart: Some(false),
        started_at: Some(100),
        ended_at: None,
        restarts: Some(1),
        state,
        memos: Some(memos),
    }
}

fn checkpoint(phase: &str) -> Value {
    json!({ "phase": phase, "step": 1 })
}

// ── record serde ───────────────────────────────────────────────────────────

#[test]
fn task_links_round_trip_and_old_json_reads_as_empty() {
    let mut original = record(TaskState::Terminal {
        outcome: completed(),
    });
    original.trace = Some(TraceCarrier::from_pairs([("trace", "root")]));
    original.links = vec![
        TraceCarrier::from_pairs([("link", "first")]),
        TraceCarrier::from_pairs([("link", "second")]),
    ];

    let encoded = serde_json::to_value(&original).expect("serialize task");
    assert_eq!(encoded["trace"], json!({ "trace": "root" }));
    assert_eq!(
        encoded["links"],
        json!([{ "link": "first" }, { "link": "second" }]),
        "links persist as an array of carrier objects"
    );
    let decoded: TaskRecord = serde_json::from_value(encoded).expect("deserialize task");
    assert_eq!(decoded, original);

    // A task written before `links` existed (or by an upstream producer with
    // no links) must load with an empty vector, not fail.
    let mut old = serde_json::to_value(&original).expect("serialize task");
    let object = old.as_object_mut().expect("task object");
    object.remove("links");
    object.remove("trace");
    let old: TaskRecord = serde_json::from_value(old).expect("deserialize old task");
    assert!(old.links.is_empty());
    assert!(old.trace.is_none());
}

// ── define_task / TaskBuilder validation ───────────────────────────────────

#[test]
fn define_task_accepts_a_valid_definition() {
    let task = define_task("demo.task", 3, initial)
        .phase("start", noop_handler())
        .phase("finish", noop_handler())
        .abort(noop_handler())
        .build()
        .expect("a valid definition builds");

    let definition = task.definition();
    assert_eq!(definition.name, "demo.task");
    assert_eq!(definition.version, 3);
    assert_eq!(
        definition.phases.keys().cloned().collect::<Vec<_>>(),
        vec!["finish".to_owned(), "start".to_owned()]
    );
    assert!(definition.abort.is_some());
    assert!(definition.migrate.is_none());
    assert!(definition.hooks.is_none());
    assert_eq!(
        (definition.initial)(&json!({ "ignored": true })),
        initial(&json!({}))
    );
}

#[test]
fn empty_name_is_rejected_before_every_other_check() {
    // Empty name, version 0 and no phases together still reports the name.
    assert_eq!(
        define_task("", 0, initial).build().unwrap_err(),
        TaskDefinitionError::EmptyName
    );
}

#[test]
fn non_positive_versions_are_rejected() {
    for (name, version) in [("zero", 0), ("negative", -1), ("very-negative", i64::MIN)] {
        assert_eq!(
            define_task(name, version, initial)
                .phase("start", noop_handler())
                .build()
                .unwrap_err(),
            TaskDefinitionError::InvalidVersion {
                task: name.to_owned()
            }
        );
    }

    // A huge positive version is valid; only stored-version handover is
    // bounded by the definition's own version.
    let huge = define_task("huge", i64::MAX, initial)
        .phase("start", noop_handler())
        .build()
        .expect("i64::MAX is a positive version");
    assert_eq!(huge.definition().version, i64::MAX);
}

#[test]
fn a_definition_without_phases_is_rejected() {
    assert_eq!(
        define_task("no-phases", 1, initial).build().unwrap_err(),
        TaskDefinitionError::NoPhases {
            task: "no-phases".to_owned()
        }
    );
}

#[test]
fn an_empty_phase_name_is_rejected() {
    assert_eq!(
        define_task("empty-phase", 1, initial)
            .phase("", noop_handler())
            .build()
            .unwrap_err(),
        TaskDefinitionError::EmptyPhase {
            task: "empty-phase".to_owned()
        }
    );
}

#[test]
fn duplicate_phase_names_are_rejected_by_build() {
    let first = noop_handler();
    let second = noop_handler();

    // Deliberate deviation (DURABLE-HANDOFF §6b): upstream silently keeps the
    // last handler; the builder records the repeat and `build` refuses it —
    // running the wrong phase handler is worse than refusing to build.
    let error = define_task("duplicate", 1, initial)
        .phase("start", first.clone())
        .phase("start", second.clone())
        .build()
        .expect_err("a duplicate phase name must be rejected");
    assert!(
        matches!(
            error,
            TaskDefinitionError::DuplicatePhase { ref task, ref phase }
                if task == "duplicate" && phase == "start"
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn error_messages_name_the_offending_task_and_phase() {
    assert_eq!(
        TaskDefinitionError::EmptyName.to_string(),
        "Task name must be a non-empty string"
    );
    assert_eq!(
        TaskDefinitionError::InvalidVersion {
            task: "job".to_owned()
        }
        .to_string(),
        "Task job version must be a positive integer"
    );
    assert_eq!(
        TaskDefinitionError::NoPhases {
            task: "job".to_owned()
        }
        .to_string(),
        "Task job must declare at least one phase"
    );
    assert_eq!(
        TaskDefinitionError::DuplicatePhase {
            task: "job".to_owned(),
            phase: "start".to_owned()
        }
        .to_string(),
        "Task job declares phase start twice"
    );
    assert_eq!(
        TaskDefinitionError::EmptyPhase {
            task: "job".to_owned()
        }
        .to_string(),
        "Task job has an empty phase name"
    );
}

// ── Migrate ────────────────────────────────────────────────────────────────

fn migrate_checkpoint(from: i64, checkpoint: &Value) -> Result<Value, String> {
    if from < 1 {
        return Err(format!("unsupported version {from}"));
    }
    Ok(json!({
        "phase": checkpoint.get("phase").cloned().unwrap_or(Value::Null),
        "migratedFrom": from
    }))
}

fn migrate_full(from: i64, input: &Value, checkpoint: &Value) -> Result<Migrated, String> {
    let keep = input
        .get("keep")
        .cloned()
        .ok_or_else(|| "missing keep".to_owned())?;
    Ok(Migrated {
        input: json!({ "keep": keep, "from": from }),
        checkpoint: json!({
            "phase": checkpoint.get("phase").cloned().unwrap_or(Value::Null),
            "migratedFrom": from
        }),
    })
}

#[test]
fn checkpoint_migration_carries_the_input_over_verbatim() {
    let migration = Migrate::Checkpoint(migrate_checkpoint);

    let migrated = migration
        .apply(
            3,
            &json!({ "q": 1 }),
            &json!({ "phase": "start", "extra": true }),
        )
        .expect("the checkpoint migration applies");

    assert_eq!(migrated.input, json!({ "q": 1 }));
    assert_eq!(
        migrated.checkpoint,
        json!({ "phase": "start", "migratedFrom": 3 })
    );
}

#[test]
fn checkpoint_migration_propagates_the_error() {
    let migration = Migrate::Checkpoint(migrate_checkpoint);

    assert_eq!(
        migration.apply(0, &json!({}), &json!({})).unwrap_err(),
        "unsupported version 0"
    );
}

#[test]
fn full_migration_transforms_input_and_checkpoint() {
    let migration = Migrate::Full(migrate_full);

    let migrated = migration
        .apply(
            4,
            &json!({ "keep": 7, "old": true }),
            &json!({ "phase": "execute" }),
        )
        .expect("the full migration applies");

    assert_eq!(
        migrated,
        Migrated {
            input: json!({ "keep": 7, "from": 4 }),
            checkpoint: json!({ "phase": "execute", "migratedFrom": 4 }),
        }
    );
}

#[test]
fn full_migration_propagates_the_error() {
    let migration = Migrate::Full(migrate_full);

    assert_eq!(
        migration
            .apply(2, &json!({ "other": 1 }), &json!({}))
            .unwrap_err(),
        "missing keep"
    );
}

#[test]
fn migrate_debug_names_the_form() {
    assert_eq!(
        format!("{:?}", Migrate::Checkpoint(migrate_checkpoint)),
        "checkpoint"
    );
    assert_eq!(format!("{:?}", Migrate::Full(migrate_full)), "full");
}

// ── RunningTask ────────────────────────────────────────────────────────────

#[test]
fn from_record_projects_a_running_record() {
    let projected = record(TaskState::Running {
        checkpoint: checkpoint("execute"),
    });
    let running = RunningTask::from_record(&projected).expect("running state projects");

    assert_eq!(running.id, TaskId::from_raw(7));
    assert_eq!(running.conversation_id, ConversationId::from_raw(3));
    assert_eq!(running.kind, "demo.task");
    assert_eq!(running.version, 2);
    assert_eq!(running.input, json!({ "q": 1 }));
    assert_eq!(running.checkpoint, checkpoint("execute"));
    assert_eq!(running.memos, projected.memos);
    assert!(running.abort_requested);
    assert_eq!(running.abort_reason.as_deref(), Some("restart"));
    assert!(running.background);
    assert_eq!(running.phase(), Some("execute"));
}

#[test]
fn from_record_returns_none_for_every_non_running_state() {
    let states = [
        TaskState::Pending {
            checkpoint: checkpoint("start"),
        },
        TaskState::Waiting {
            checkpoint: checkpoint("start"),
            on: vec![TaskId::from_raw(9)],
            policy: JoinPolicy::AllSettled,
        },
        TaskState::Sleeping {
            checkpoint: checkpoint("start"),
            until: 123,
        },
        TaskState::Completing {
            outcome: completed(),
        },
        TaskState::Terminal { outcome: failed() },
    ];

    for state in states {
        assert!(
            RunningTask::from_record(&record(state)).is_none(),
            "only `running` projects to a RunningTask"
        );
    }
}

#[test]
fn phase_requires_a_string_phase_field() {
    for (checkpoint_value, expected) in [
        (json!({ "phase": "execute" }), Some("execute")),
        (json!({ "phase": "prepare", "step": 2 }), Some("prepare")),
        (json!({}), None),
        (json!({ "phase": 4 }), None),
        (json!({ "phase": null }), None),
        (json!({ "phase": ["execute"] }), None),
    ] {
        let running = RunningTask::from_record(&record(TaskState::Running {
            checkpoint: checkpoint_value,
        }))
        .expect("running state projects");
        assert_eq!(running.phase(), expected);
    }
}

#[test]
fn for_abort_projects_live_and_terminal_states() {
    let cases: [(TaskState, Value); 6] = [
        (
            TaskState::Pending {
                checkpoint: checkpoint("pending"),
            },
            checkpoint("pending"),
        ),
        (
            TaskState::Running {
                checkpoint: checkpoint("running"),
            },
            checkpoint("running"),
        ),
        (
            TaskState::Waiting {
                checkpoint: checkpoint("waiting"),
                on: vec![TaskId::from_raw(9)],
                policy: JoinPolicy::FailFast,
            },
            checkpoint("waiting"),
        ),
        (
            TaskState::Sleeping {
                checkpoint: checkpoint("sleeping"),
                until: 456,
            },
            checkpoint("sleeping"),
        ),
        (
            TaskState::Completing {
                outcome: completed(),
            },
            Value::Null,
        ),
        (TaskState::Terminal { outcome: failed() }, Value::Null),
    ];

    for (state, expected_checkpoint) in cases {
        let projected = record(state);
        let running = RunningTask::for_abort(&projected).expect("for_abort projects every state");

        assert_eq!(running.id, projected.id);
        assert_eq!(running.checkpoint, expected_checkpoint);
        assert_eq!(running.input, projected.input);
        assert_eq!(running.version, projected.version);
        assert!(running.abort_requested);
        assert_eq!(running.abort_reason.as_deref(), Some("restart"));
        assert!(running.background);
    }
}

// ── TaskDefinition ─────────────────────────────────────────────────────────

#[test]
fn phase_for_returns_the_declared_handler_and_knows_phase_matches() {
    let start = noop_handler();
    let finish = noop_handler();
    let task = define_task("phased", 1, initial)
        .phase("start", start.clone())
        .phase("finish", finish)
        .build()
        .expect("valid definition");
    let definition = task.definition();

    let found = definition
        .phase_for(&json!({ "phase": "start", "step": 1 }))
        .expect("`start` is declared");
    assert!(Arc::ptr_eq(found, &start));

    assert!(definition
        .phase_for(&json!({ "phase": "missing" }))
        .is_none());
    assert!(definition.phase_for(&json!({})).is_none());
    assert!(definition.phase_for(&json!({ "phase": 7 })).is_none());
    assert!(definition.phase_for(&json!({ "phase": null })).is_none());

    assert!(definition.knows_phase(&json!({ "phase": "start" })));
    assert!(definition.knows_phase(&json!({ "phase": "finish" })));
    assert!(!definition.knows_phase(&json!({ "phase": "missing" })));
    assert!(!definition.knows_phase(&json!({ "phase": 7 })));
    assert!(!definition.knows_phase(&json!({})));
}

#[test]
fn can_reserve_version_requires_the_same_version_or_a_newer_migration() {
    let migrating_task = define_task("migrating", 2, initial)
        .phase("start", noop_handler())
        .migrate(migrate_checkpoint)
        .build()
        .expect("valid definition");
    let migrating = migrating_task.definition();

    assert!(
        migrating.can_reserve_version(2),
        "same version is always reserved"
    );
    assert!(
        migrating.can_reserve_version(1),
        "an older stored version migrates forward"
    );
    assert!(migrating.can_reserve_version(0));
    assert!(migrating.can_reserve_version(-5));
    assert!(
        !migrating.can_reserve_version(3),
        "a newer stored version cannot be taken over"
    );
    assert!(!migrating.can_reserve_version(i64::MAX));

    let fixed_task = define_task("fixed", 2, initial)
        .phase("start", noop_handler())
        .build()
        .expect("valid definition");
    let fixed = fixed_task.definition();

    assert!(fixed.can_reserve_version(2));
    assert!(!fixed.can_reserve_version(1), "no migration, no handover");
    assert!(!fixed.can_reserve_version(0));
    assert!(!fixed.can_reserve_version(3));
}

// ── erased surface ─────────────────────────────────────────────────────────

#[test]
fn erased_task_exposes_the_definition_surface() {
    let hooks: Arc<dyn Any + Send + Sync> = Arc::new(41u32);
    let task = define_task(
        "erased",
        3,
        |input: &Value| json!({ "phase": "start", "seen": input }),
    )
    .phase("start", noop_handler())
    .abort(noop_handler())
    .migrate(migrate_checkpoint)
    .hooks(hooks)
    .build()
    .expect("valid definition");

    let erased = Arc::new(task.clone()).erased_shared();

    assert_eq!(erased.name(), "erased");
    assert_eq!(erased.version(), 3);
    assert_eq!(
        erased.initial_state(&json!(9)),
        json!({ "phase": "start", "seen": 9 })
    );
    assert!(erased.phase_for(&json!({ "phase": "start" })).is_some());
    assert!(erased.phase_for(&json!({ "phase": "missing" })).is_none());
    assert!(erased.abort_handler().is_some());
    assert!(matches!(erased.migration(), Some(Migrate::Checkpoint(_))));
    assert!(erased.can_reserve_version(2));
    assert!(!erased.can_reserve_version(4));
    assert_eq!(erased.as_definition().name, "erased");
    assert_eq!(
        erased
            .hooks()
            .expect("hooks are attached")
            .downcast_ref::<u32>(),
        Some(&41)
    );

    // `erased` consumes the token; the same definition is also reachable by
    // cloning it first.
    let owned = task.clone().erased();
    assert_eq!(owned.name(), "erased");
    assert_eq!(format!("{:?}", task), "Task(\"erased\")");
}
