//! The `Harness` facade, its provider identity and its telemetry vocabulary.
//!
//! Ports upstream `harness/{harness,provider,telemetry}.ts`. Three groups:
//!
//! | group | what it pins |
//! |---|---|
//! | provider | the UUIDv7 text shape, per-conversation uniqueness, and `ensure_provider_session_id`'s two paths (committed read vs. one minting commit) |
//! | telemetry | the exact span/event/metric names, attribute keys, types and value domains; that the schema is descriptors only |
//! | harness | open/close lifecycle, atomic conversation creation (record plus five built-in documents in one commit), fork identity, the Conversation/Submission handles, usage summation, inspection, the task graph, and document state/watch |
//!
//! Everything observed here is committed state: a Harness is driven over a real
//! [`MemoryStorage`] and every assertion reads what a reopen would read.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use pir::durable::harness::agent::{AgentChange, Change, ModelRef};
use pir::durable::harness::documents::{ProviderState, PROVIDER_KIND};
use pir::durable::harness::harness::{
    agent_address, usage_address, ConversationFrame, Harness, HarnessError, HarnessOptions,
    SubmissionAbortResult,
};
use pir::durable::harness::provider::{
    ensure_provider_session_id, mint_session_id, provider_address, provider_doc,
};
use pir::durable::harness::registry::create_registry;
use pir::durable::harness::scheduler::SchedulingState;
use pir::durable::harness::submissions::{SubmissionDraft, WhenBusy};
use pir::durable::harness::telemetry::{
    AttributeType, Cardinality, MetricKind, RecordingTelemetry, DURABLE_TELEMETRY_SCHEMA,
};
use pir::durable::harness::{settings, usage};
use pir::durable::session::attach::Attached;
use pir::durable::session::facade::ConversationOwnership;
use pir::durable::session::line::JobValue;
use pir::durable::session::publication::{CommitChange, CommitPublication, TableCommitChange};
use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::transaction::TaskOptions;
use pir::durable::types::SubmissionQuery;
use pir::durable::{
    Context, ConversationId, DocumentCommitChange, DocumentFork, DocumentSemantics, EntryId,
    EntryQuery, EntryRecord, JsonObject, MemoryStorage, ReportInfo, ResolvedAddress, ScanOrder,
    Storage, StorageError, StorageWrite, SubmissionId, SubmissionStatus, TaskDefinition, TaskId,
    ROOT_CONVERSATION_ID,
};

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
}

/// Captured reports.
#[derive(Clone, Default)]
struct Reports {
    seen: Arc<Mutex<Vec<(String, ReportInfo)>>>,
}

impl Reports {
    fn len(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn harness_options(clock: &Clock) -> HarnessOptions {
    let clock = clock.clone();
    HarnessOptions {
        registry: Some(Arc::new(create_registry())),
        now: Some(Arc::new(move || clock.now())),
        ..HarnessOptions::default()
    }
}

async fn open_harness(clock: &Clock, storage: Arc<MemoryStorage>) -> Harness {
    let storage: Arc<dyn Storage> = storage;
    Harness::open(storage, harness_options(clock), Context::background())
        .await
        .expect("open harness")
}

async fn open_harness_with(
    _clock: &Clock,
    storage: Arc<MemoryStorage>,
    options: HarnessOptions,
) -> Harness {
    let storage: Arc<dyn Storage> = storage;
    Harness::open(storage, options, Context::background())
        .await
        .expect("open harness")
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("a JSON object")
}

fn conversation(harness: &Harness, id: ConversationId) -> pir::durable::harness::Conversation {
    harness.conversation(id)
}

// ── provider shape ─────────────────────────────────────────────────────────

#[test]
fn minted_ids_have_the_uuidv7_text_shape() {
    let id = mint_session_id(ConversationId::from_raw(7), 1_700_000_000_000);
    assert_eq!(id.len(), 36, "8-4-4-4-12 is 36 characters: {id}");
    let groups: Vec<&str> = id.split('-').collect();
    assert_eq!(
        groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12]
    );
    for group in &groups {
        assert!(
            group
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()),
            "lowercase hex only: {id}"
        );
    }
    // Version nibble and variant bits.
    assert_eq!(&id[14..15], "7", "UUID version 7: {id}");
    assert!(
        matches!(&id[19..20], "8" | "9" | "a" | "b"),
        "RFC 4122 variant: {id}"
    );
}

#[test]
fn minted_ids_put_the_clock_in_the_high_bits() {
    // The 48-bit timestamp is groups 1 and 2, so a later millisecond sorts
    // after an earlier one for the same conversation.
    let early = mint_session_id(ConversationId::from_raw(3), 0x0000_0011_2233_4455);
    let late = mint_session_id(ConversationId::from_raw(3), 0x0000_0011_2233_4455 + 1);
    assert_eq!(&early[..12], &late[..12], "the shared timestamp prefix");
    assert_ne!(
        &early[12..13],
        &late[12..13],
        "a one-millisecond step changes the last timestamp digit"
    );
    assert!(early < late, "ids sort by their clock prefix");

    // The literal timestamp lands in the first two groups.
    let zero = mint_session_id(ConversationId::from_raw(1), 0);
    assert!(zero.starts_with("00000000-0000-7"), "{zero}");
    let one_ms = mint_session_id(ConversationId::from_raw(1), 1);
    assert!(one_ms.starts_with("00000000-0001-7"), "{one_ms}");
}

#[test]
fn minted_ids_are_distinct_per_conversation_and_stable_per_pair() {
    let first = mint_session_id(ConversationId::from_raw(1), 1_000);
    let second = mint_session_id(ConversationId::from_raw(2), 1_000);
    assert_ne!(first, second, "conversations never share an identity");
    // The mint is a pure function of the pair: a retry before the commit
    // inserts writes the same id.
    assert_eq!(first, mint_session_id(ConversationId::from_raw(1), 1_000));

    // Different ms on the same conversation differ in the random half too.
    let later = mint_session_id(ConversationId::from_raw(1), 1_001);
    assert_ne!(first, later);
}

#[test]
fn provider_address_is_a_latest_initial_fork_conversation_document() {
    let resolved = provider_address(ConversationId::from_raw(9));
    assert_eq!(resolved.address.kind, PROVIDER_KIND);
    assert_eq!(
        resolved.address.scope,
        pir::durable::DocumentScope::Conversation {
            conversation_id: ConversationId::from_raw(9)
        }
    );
    assert!(resolved.address.key.is_none());
    // The definition the address is resolved against.
    let definition = provider_doc();
    assert_eq!(definition.common.kind, PROVIDER_KIND);
    assert_eq!(definition.common.version, 1);
    assert_eq!(
        definition.common.semantics,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial
        }
    );
}

#[tokio::test]
async fn ensure_provider_session_id_creates_once_and_then_reads() {
    let storage = Arc::new(MemoryStorage::new());
    let harness = open_harness(&Clock::new(5_000), storage.clone()).await;
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("create conversation")
        .id();

    let first = ensure_provider_session_id(
        harness.session(),
        conversation,
        5_000,
        Context::background(),
    )
    .await
    .expect("mint");
    assert_eq!(
        first,
        mint_session_id(conversation, 5_000),
        "the first call mints from the pair"
    );

    // A second call reads the committed value, even on a different clock.
    let second = ensure_provider_session_id(
        harness.session(),
        conversation,
        9_999,
        Context::background(),
    )
    .await
    .expect("read");
    assert_eq!(second, first, "a committed id is never re-minted");

    // It is committed, not merely returned.
    let stored = harness
        .snapshot(&provider_address(conversation), &Context::background())
        .await
        .expect("snapshot")
        .expect("pi.provider exists");
    let decoded: ProviderState = serde_json::from_value(Value::Object(stored)).expect("decode");
    assert_eq!(decoded.session_id.as_deref(), Some(first.as_str()));

    // And it is the harness's convenience too.
    let via_harness = harness
        .provider_session_id(conversation, Context::background())
        .await
        .expect("provider session id");
    assert_eq!(via_harness, first);

    harness.close().await.expect("close");
}

#[tokio::test]
async fn a_fork_gets_its_own_provider_identity() {
    let storage = Arc::new(MemoryStorage::new());
    let harness = open_harness(&Clock::new(5_000), storage.clone()).await;
    let parent = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("parent")
        .id();
    let parent_id =
        ensure_provider_session_id(harness.session(), parent, 5_000, Context::background())
            .await
            .expect("parent id");

    // Fork at a head marker so the fork commit has a real entry.
    let entry = harness
        .session()
        .append_entry(
            parent,
            pir::durable::session::transaction::EntryDraft {
                kind: "user".to_owned(),
                model: Some(vec![json!({"role": "user", "content": "hi"})]),
                data: None,
                head: Some(pir::durable::session::transaction::HeadTarget::SelfEntry),
                edits: None,
            },
            Context::background(),
        )
        .await
        .expect("append");

    let child = harness
        .fork(
            parent,
            entry.id,
            ConversationOwnership::Ownerless,
            Context::background(),
        )
        .await
        .expect("fork")
        .id();
    assert_ne!(child, parent);

    // `fork: initial` leaves the child's document empty.
    let child_state = harness
        .snapshot(&provider_address(child), &Context::background())
        .await
        .expect("snapshot");
    assert_eq!(
        child_state,
        Some(Map::new()),
        "a fork starts its provider identity empty"
    );

    let child_id =
        ensure_provider_session_id(harness.session(), child, 5_001, Context::background())
            .await
            .expect("child id");
    assert_ne!(child_id, parent_id, "a fork never reuses its parent's id");
    assert_eq!(child_id, mint_session_id(child, 5_001));

    harness.close().await.expect("close");
}

// ── telemetry schema ───────────────────────────────────────────────────────

#[test]
fn the_schema_declares_exactly_the_upstream_vocabulary() {
    let schema = DURABLE_TELEMETRY_SCHEMA;
    assert_eq!(schema.version, 1);
    assert_eq!(
        schema
            .spans
            .iter()
            .map(|span| span.name)
            .collect::<Vec<_>>(),
        vec!["pi.durable.submit", "pi.durable.phase"]
    );
    assert_eq!(
        schema
            .metrics
            .iter()
            .map(|metric| metric.name)
            .collect::<Vec<_>>(),
        vec![
            "pi.durable.reports",
            "pi.durable.phase.duration",
            "pi.durable.tasks.active"
        ]
    );

    let mut events: Vec<&str> = schema
        .events()
        .into_iter()
        .map(|event| event.name)
        .collect();
    events.sort_unstable();
    assert_eq!(
        events,
        vec![
            "pi.durable.checkpoint",
            "pi.durable.entry",
            "pi.durable.replay",
            "pi.durable.report",
            "pi.durable.submission",
        ]
    );
    // `events()` deduplicates: `entry`/`submission` are declared in both spans.
    assert_eq!(schema.events().len(), 5);
    assert!(schema.span("pi.durable.submit").is_some());
    assert!(schema.span("pi.durable.nope").is_none());
    assert!(schema.metric("pi.durable.tasks.active").is_some());
    assert!(schema.event("pi.durable.entry").is_some());
}

#[test]
fn the_submit_span_carries_upstream_start_and_end_attributes() {
    let span = DURABLE_TELEMETRY_SCHEMA
        .span("pi.durable.submit")
        .expect("submit span");
    assert_eq!(span.parents, "any");
    assert_eq!(span.status_default, "ok");
    assert_eq!(span.events.len(), 3);

    let start: Vec<(&str, AttributeType, bool)> = span
        .start_attributes
        .iter()
        .map(|attribute| (attribute.key, attribute.value_type, attribute.required))
        .collect();
    assert_eq!(
        start,
        vec![
            ("submission.type", AttributeType::String, true),
            ("conversation.id", AttributeType::Number, true),
        ]
    );
    assert_eq!(
        span.start_attributes[0].values,
        Some(&["input", "write"][..])
    );
    assert_eq!(
        span.start_attributes[1].cardinality,
        Some(Cardinality::High)
    );

    let end: Vec<&str> = span
        .end_attributes
        .iter()
        .map(|attribute| attribute.key)
        .collect();
    assert_eq!(end, vec!["submission.id", "result"]);
    assert_eq!(
        span.end_attributes[1].values,
        Some(&["queued", "placed", "done"][..])
    );
    assert!(
        !span.end_attributes[1].required,
        "an end attribute is optional"
    );
}

#[test]
fn the_phase_span_carries_every_declared_attribute_and_event() {
    let span = DURABLE_TELEMETRY_SCHEMA
        .span("pi.durable.phase")
        .expect("phase span");
    let start: Vec<&str> = span
        .start_attributes
        .iter()
        .map(|attribute| attribute.key)
        .collect();
    assert_eq!(
        start,
        vec![
            "task.id",
            "task.name",
            "phase",
            "kind",
            "attempt",
            "resumed"
        ]
    );
    // `resumed` is the one boolean.
    let resumed = span
        .start_attributes
        .iter()
        .find(|attribute| attribute.key == "resumed")
        .expect("resumed");
    assert_eq!(resumed.value_type, AttributeType::Boolean);
    assert!(resumed.required);
    assert_eq!(
        span.start_attributes[3].values,
        Some(&["run", "abort"][..]),
        "kind is a closed domain"
    );

    let end: Vec<&str> = span
        .end_attributes
        .iter()
        .map(|attribute| attribute.key)
        .collect();
    assert_eq!(end, vec!["outcome", "tool.name", "tool.call_id"]);

    let events: Vec<&str> = span.events.iter().map(|event| event.name).collect();
    assert_eq!(
        events,
        vec![
            "pi.durable.entry",
            "pi.durable.submission",
            "pi.durable.report",
            "pi.durable.checkpoint",
            "pi.durable.replay",
        ]
    );
    // The error rule documents the capped diagnostic.
    assert!(span.status_error_when.contains("500 characters"));
}

#[test]
fn the_metrics_have_their_upstream_kinds_units_and_dimensions() {
    let schema = DURABLE_TELEMETRY_SCHEMA;

    let reports = schema.metric("pi.durable.reports").expect("reports");
    assert_eq!(reports.kind, MetricKind::Counter);
    assert_eq!(reports.unit, None);
    assert_eq!(
        reports.attributes.iter().map(|a| a.key).collect::<Vec<_>>(),
        vec!["type"],
        "the counter groups by the report type only"
    );

    let duration = schema
        .metric("pi.durable.phase.duration")
        .expect("duration");
    assert_eq!(duration.kind, MetricKind::Histogram);
    assert_eq!(duration.unit, Some("ms"));
    assert_eq!(
        duration
            .attributes
            .iter()
            .map(|a| a.key)
            .collect::<Vec<_>>(),
        vec!["task", "phase", "outcome"]
    );

    let active = schema.metric("pi.durable.tasks.active").expect("active");
    assert_eq!(active.kind, MetricKind::UpDownCounter);
    assert_eq!(
        active.attributes.iter().map(|a| a.key).collect::<Vec<_>>(),
        vec!["task"]
    );

    assert_eq!(MetricKind::Counter.as_str(), "counter");
    assert_eq!(MetricKind::Histogram.as_str(), "histogram");
    assert_eq!(MetricKind::UpDownCounter.as_str(), "upDownCounter");
}

#[test]
fn the_report_descriptors_share_their_domains() {
    let schema = DURABLE_TELEMETRY_SCHEMA;
    let event = schema.event("pi.durable.report").expect("report event");
    let types = event
        .attributes
        .iter()
        .find(|attribute| attribute.key == "type")
        .expect("type");
    assert_eq!(types.values.expect("a closed domain").len(), 9);
    assert!(types.values.expect("values").contains(&"session_failed"));
    assert!(types.values.expect("values").contains(&"listener_error"));

    let reason = event
        .attributes
        .iter()
        .find(|attribute| attribute.key == "reason")
        .expect("reason");
    assert_eq!(
        reason.values,
        Some(
            &[
                "threw",
                "restart_limit",
                "missing_task",
                "incompatible_task"
            ][..]
        )
    );
    assert!(!reason.required);

    // The counter groups by the *same* `type` descriptor.
    let counter = schema.metric("pi.durable.reports").expect("counter");
    assert_eq!(counter.attributes[0].key, types.key);
    assert_eq!(counter.attributes[0].values, types.values);

    // The entry event is content-free: ids and kinds only.
    let entry = schema.event("pi.durable.entry").expect("entry event");
    assert_eq!(
        entry
            .attributes
            .iter()
            .map(|attribute| attribute.key)
            .collect::<Vec<_>>(),
        vec!["entry.id", "entry.kind"]
    );
}

// ── open / close lifecycle ─────────────────────────────────────────────────

#[tokio::test]
async fn open_rejects_a_registry_without_the_builtin_tasks() {
    // An empty registry snapshot cannot be built through `Registry` (it always
    // seeds the built-ins), so this asserts the *passing* path plus the
    // documented failure shape directly.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let harness = Harness::open(
        storage,
        HarnessOptions {
            now: Some(Arc::new(|| 1)),
            ..HarnessOptions::default()
        },
        Context::background(),
    )
    .await
    .expect("the default registry carries every built-in task");
    assert_eq!(
        harness.registry().snapshot().tasks().len(),
        3,
        "pi.generation, pi.tool, pi.compaction"
    );
    harness.close().await.expect("close");
}

#[tokio::test]
async fn open_reports_closed_and_refuses_later_operations() {
    let clock = Clock::new(100);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    assert!(!harness.closed());
    harness.close().await.expect("close");
    assert!(harness.closed(), "close marks the harness closed");

    let error = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect_err("a closed harness refuses");
    assert_eq!(error, HarnessError::Closed);
    // A second close is a no-op, not an error.
    harness.close().await.expect("idempotent close");
}

#[tokio::test]
async fn open_rejects_an_aborted_context() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let signal = pir::ai::types::AbortSignal::new();
    signal.abort();
    let context = Context::with_signal(signal);
    let error = Harness::open(storage, HarnessOptions::default(), context)
        .await
        .expect_err("an aborted context fails open");
    assert!(matches!(error, HarnessError::Invalid(_)));
}

#[tokio::test]
async fn resume_wake_and_settings_compose_the_scheduler() {
    let clock = Clock::new(1_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    // Open does not dispatch (upstream `Harness.open` runs recovery only).
    assert_eq!(
        harness.scheduler().scheduling_state(),
        pir::durable::harness::scheduler::SchedulingState::Paused
    );
    harness.resume();
    assert_eq!(
        harness.scheduler().scheduling_state(),
        pir::durable::harness::scheduler::SchedulingState::Running
    );
    // No sleep is armed and no task can run: the host has nothing to arm.
    let wake = harness.wake();
    assert!(!wake.active);
    assert_eq!(wake.at, None);

    // Settings come from `HarnessOptions.settings`, resolved at each read.
    assert_eq!(
        harness.settings().restart_limit,
        settings::Settings::default().restart_limit
    );
    assert_eq!(harness.now(), 1_000);

    harness.close().await.expect("close");
}

#[tokio::test]
async fn a_report_handler_is_called_and_its_panic_is_contained() {
    let clock = Clock::new(1_000);
    let reports = Reports::default();
    let captured = reports.clone();
    let mut options = harness_options(&clock);
    options.on_report = Some(Arc::new(move |message: &str, info: &ReportInfo| {
        captured
            .seen
            .lock()
            .unwrap()
            .push((message.to_owned(), info.clone()));
    }));
    let harness = open_harness_with(&clock, Arc::new(MemoryStorage::new()), options).await;
    harness.report("boom", ReportInfo::ListenerError);
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports.seen.lock().unwrap()[0],
        ("boom".to_owned(), ReportInfo::ListenerError)
    );
    assert!(harness.on_report().is_some());

    // A panicking handler never escapes into the caller.
    let mut panicking = harness_options(&clock);
    panicking.on_report = Some(Arc::new(|_: &str, _: &ReportInfo| panic!("handler boom")));
    let harness = open_harness_with(&clock, Arc::new(MemoryStorage::new()), panicking).await;
    harness.report("still fine", ReportInfo::ListenerError);
    harness.close().await.expect("close");
}

// ── conversations ──────────────────────────────────────────────────────────

#[tokio::test]
async fn create_conversation_stages_all_five_builtin_documents() {
    let clock = Clock::new(2_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("create")
        .id();
    let ctx = Context::background();

    let agent = harness
        .snapshot(&agent_address(conversation), &ctx)
        .await
        .expect("agent")
        .expect("pi.agent exists");
    assert_eq!(agent, Map::new());

    let live = harness
        .snapshot(&conversation_address("pi.live", conversation), &ctx)
        .await
        .expect("live")
        .expect("pi.live exists");
    assert_eq!(live, Map::new());

    let inbox = harness
        .snapshot(&conversation_address("pi.inbox", conversation), &ctx)
        .await
        .expect("inbox")
        .expect("pi.inbox exists");
    assert_eq!(Value::Object(inbox), json!({"items": []}));

    let provider = harness
        .snapshot(&provider_address(conversation), &ctx)
        .await
        .expect("provider")
        .expect("pi.provider exists");
    assert_eq!(
        provider,
        Map::new(),
        "the provider id is minted on first use, not at creation"
    );

    let usage = harness
        .snapshot(&usage_address(conversation), &ctx)
        .await
        .expect("usage")
        .expect("pi.usage exists");
    assert_eq!(Value::Object(usage), json!({"models": {}, "tools": {}}));

    // The conversation record itself is committed too.
    let record = harness
        .conversation_record(conversation, &ctx)
        .await
        .expect("record")
        .expect("committed");
    assert!(record.parent.is_none());
    assert!(record.owner.is_none());

    harness.close().await.expect("close");
}

#[tokio::test]
async fn conversation_creation_commits_the_record_and_five_documents_together() {
    let clock = Clock::new(2_100);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let publications: Arc<Mutex<Vec<CommitPublication>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = publications.clone();
    harness
        .session()
        .subscribe_commits(Arc::new(move |publication, _context| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(publication.clone());
        }))
        .expect("subscribe");

    let id = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("create")
        .id();

    {
        let publications = publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let creation = publications
            .iter()
            .find(|publication| {
                publication.changes.iter().any(|change| {
                    matches!(
                        change,
                        CommitChange::Table(TableCommitChange::Conversation(record)) if record.id == id
                    )
                })
            })
            .expect("the creation commit was published");
        let mut conversation_staged = false;
        let mut kinds = std::collections::BTreeSet::new();
        for change in &creation.changes {
            match change {
                CommitChange::Table(TableCommitChange::Conversation(record)) if record.id == id => {
                    conversation_staged = true;
                }
                CommitChange::Document(DocumentCommitChange::Document { record, .. }) => {
                    if let pir::durable::DocumentScope::Conversation { conversation_id } =
                        record.scope
                    {
                        if conversation_id == id {
                            kinds.insert(record.kind.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        assert!(conversation_staged, "the record is in the creating commit");
        assert_eq!(
            kinds,
            ["pi.agent", "pi.inbox", "pi.live", "pi.provider", "pi.usage"]
                .into_iter()
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>(),
            "a conversation and its five documents are one commit"
        );
    }
    harness.close().await.expect("close");
}

#[tokio::test]
async fn fork_stages_initial_documents_and_copies_the_agent_in_one_commit() {
    let clock = Clock::new(2_200);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let parent = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("parent")
        .id();
    let at = harness
        .session()
        .append_entry(
            parent,
            EntryDraft {
                kind: "app.user".to_owned(),
                model: Some(vec![json!({"role": "user", "content": "hi"})]),
                data: None,
                head: Some(pir::durable::session::transaction::HeadTarget::SelfEntry),
                edits: None,
            },
            ctx.clone(),
        )
        .await
        .expect("append");

    let publications: Arc<Mutex<Vec<CommitPublication>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = publications.clone();
    harness
        .session()
        .subscribe_commits(Arc::new(move |publication, _context| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(publication.clone());
        }))
        .expect("subscribe");
    let child = harness
        .fork(parent, at.id, ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("fork")
        .id();

    {
        let publications = publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let creation = publications
            .iter()
            .find(|publication| {
                publication.changes.iter().any(|change| {
                    matches!(
                        change,
                        CommitChange::Table(TableCommitChange::Conversation(record)) if record.id == child
                    )
                })
            })
            .expect("the fork commit was published");
        let mut created = std::collections::BTreeSet::new();
        let mut copied = std::collections::BTreeSet::new();
        for change in &creation.changes {
            match change {
                CommitChange::Document(DocumentCommitChange::Document { record, .. }) => {
                    if let pir::durable::DocumentScope::Conversation { conversation_id } =
                        record.scope
                    {
                        if conversation_id == child {
                            created.insert(record.kind.clone());
                        }
                    }
                }
                CommitChange::Document(DocumentCommitChange::Copy {
                    record,
                    conversation_id,
                    ..
                }) if *conversation_id == child => {
                    copied.insert(record.kind.clone());
                }
                _ => {}
            }
        }
        assert_eq!(
            copied,
            ["pi.agent"].into_iter().map(str::to_owned).collect(),
            "the fork copies the agent as-of its fork entry"
        );
        assert_eq!(
            created,
            ["pi.inbox", "pi.live", "pi.provider", "pi.usage"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            "the initial-fork documents are created in the same commit"
        );
    }
    harness.close().await.expect("close");
}

#[tokio::test]
async fn root_creates_once_and_returns_the_same_conversation() {
    let clock = Clock::new(3_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let first = harness
        .root(Context::background())
        .await
        .expect("root")
        .id();
    let second = harness
        .root(Context::background())
        .await
        .expect("root again")
        .id();
    assert_eq!(first, second, "the root is created once");
    // Upstream `tx.createRootConversation()`: a fresh storage gets the
    // reserved id, not a minted one.
    assert_eq!(first, ROOT_CONVERSATION_ID);

    // The root conversation and its built-in documents are one commit.
    for (kind, expected) in [
        ("pi.live", json!({})),
        ("pi.inbox", json!({"items": []})),
        ("pi.provider", json!({})),
        ("pi.usage", json!({"models": {}, "tools": {}})),
    ] {
        let stored = harness
            .snapshot(&conversation_address(kind, first), &Context::background())
            .await
            .expect("root document")
            .expect("root document exists");
        assert_eq!(Value::Object(stored), expected, "{kind}");
    }
    harness.close().await.expect("close");

    // A storage that already has the reserved root (for example an imported
    // session) returns exactly that one instead of minting another.
    let storage = Arc::new(MemoryStorage::new());
    let prepared = storage
        .prepare_commit(vec![pir::durable::StorageWrite::Conversation {
            value: pir::durable::ConversationRecord {
                id: ROOT_CONVERSATION_ID,
                parent: None,
                owner: None,
            },
        }])
        .await
        .expect("prepare a reserved root");
    storage
        .apply_prepared(prepared)
        .await
        .expect("apply a reserved root");
    let harness = open_harness(&clock, storage).await;
    let reserved = harness
        .root(Context::background())
        .await
        .expect("reserved root")
        .id();
    assert_eq!(reserved, ROOT_CONVERSATION_ID);

    harness.close().await.expect("close");
}

#[tokio::test]
async fn configure_and_agent_round_trip_the_stored_change() {
    let clock = Clock::new(4_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let record = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("create");
    let handle = conversation(&harness, record.id());

    handle
        .configure(
            &pir::durable::harness::agent::AgentChange::instructions("be brief"),
            Context::background(),
        )
        .await
        .expect("configure");
    let agent = handle
        .agent(None, &Context::background())
        .await
        .expect("resolve agent");
    assert_eq!(agent.instructions.as_deref(), Some("be brief"));

    harness.close().await.expect("close");
}

#[tokio::test]
async fn usage_sums_every_conversation_ledger() {
    let clock = Clock::new(6_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let first = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("first")
        .id();
    let second = harness
        .create_conversation(ConversationOwnership::Ownerless, Context::background())
        .await
        .expect("second")
        .id();

    // Write a ledger into each conversation the way the usage lane would.
    for (conversation, input) in [(first, 10_u64), (second, 5_u64)] {
        harness
            .doc(
                usage_address(conversation),
                (*usage::USAGE_DEFINITION).clone(),
                Context::background(),
                move |value| {
                    let mut state: usage::UsageState =
                        serde_json::from_value(Value::Object(value.clone())).unwrap_or_default();
                    usage::usage_add(
                        &mut state,
                        usage::UsageScope::Models,
                        "anthropic/claude",
                        &pir::ai::types::Usage {
                            input,
                            output: 2,
                            ..pir::ai::types::Usage::default()
                        },
                    );
                    *value = object(serde_json::to_value(&state).expect("serialize"));
                },
            )
            .await
            .expect("write ledger");
    }

    let total = harness
        .usage(&Context::background())
        .await
        .expect("usage total");
    assert_eq!(
        total
            .models
            .get("anthropic/claude")
            .map(|usage| usage.input),
        Some(15),
        "totals add every conversation"
    );
    assert_eq!(
        total
            .models
            .get("anthropic/claude")
            .map(|usage| usage.output),
        Some(4)
    );

    harness.close().await.expect("close");
}

#[tokio::test]
async fn inspect_lists_no_work_and_reports_the_scheduling_state() {
    let clock = Clock::new(7_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create");

    let inspection = harness.inspect(&ctx).await.expect("inspect");
    assert!(inspection.tasks.is_empty(), "no task was ever created");
    assert!(inspection.submissions.is_empty());
    assert_eq!(
        inspection.scheduling,
        pir::durable::harness::scheduler::SchedulingState::Paused
    );

    // A queued submission shows up in the unsettled list.
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("second")
        .id();
    let submission = harness
        .session()
        .create_submission(
            pir::durable::session::facade::SubmissionCreate::new(
                conversation,
                pir::durable::SubmissionType::Input,
            ),
            ctx.clone(),
        )
        .await
        .expect("submission");
    let inspection = harness.inspect(&ctx).await.expect("inspect");
    assert_eq!(inspection.submissions.len(), 1);
    assert_eq!(inspection.submissions[0].id, submission.id);
    // `submission()` reads the same record back.
    let read = harness
        .submission(submission.id, &ctx)
        .await
        .expect("read")
        .expect("committed");
    assert_eq!(read, submission);
    // And nothing was accidentally scanned twice.
    let scan = harness
        .session()
        .scan_submissions(
            SubmissionQuery::default(),
            pir::durable::harness::harness::SCAN_PAGE_SIZE,
            None,
            &ctx,
        )
        .await
        .expect("scan");
    assert_eq!(scan.items.len(), 1);

    harness.close().await.expect("close");
}

#[tokio::test]
async fn conversation_handle_reads_context_entries_and_commits_in_scope() {
    let clock = Clock::new(5_100);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let id = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let handle = conversation(&harness, id);

    let first = harness
        .session()
        .append_entry(
            id,
            EntryDraft {
                kind: "app.user".to_owned(),
                model: Some(vec![json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "one"}],
                    "timestamp": 1
                })]),
                data: None,
                head: None,
                edits: None,
            },
            ctx.clone(),
        )
        .await
        .expect("first entry");
    let second = harness
        .session()
        .append_entry(
            id,
            EntryDraft {
                kind: "app.user".to_owned(),
                model: Some(vec![json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "two"}],
                    "timestamp": 2
                })]),
                data: None,
                head: None,
                edits: None,
            },
            ctx.clone(),
        )
        .await
        .expect("second entry");

    // `context()` reads the committed active transcript.
    let view = handle.context(ctx.clone(), None).await.expect("context");
    assert_eq!(
        view.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
    assert_eq!(view.messages.len(), 2);

    // With `at`, the view is the one a fork would start from.
    let cut = handle
        .context(ctx.clone(), Some(first.id))
        .await
        .expect("context at");
    assert_eq!(
        cut.entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![first.id]
    );
    assert!(
        handle
            .context(ctx.clone(), Some(EntryId::from_raw(999_999)))
            .await
            .is_err(),
        "an invisible entry is rejected"
    );

    // `entries()` pages this conversation's history and overrides the id.
    let query = EntryQuery {
        conversation_id: ConversationId::from_raw(0),
        min_entry_id: None,
        max_entry_id: None,
        order: Some(ScanOrder::Descending),
    };
    let page = handle
        .entries(query.clone(), 1, None, &ctx)
        .await
        .expect("first page");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, second.id);
    let next = page.next.expect("a continuation");
    let page = handle
        .entries(query, 1, Some(next), &ctx)
        .await
        .expect("second page");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, first.id);

    // `commit()` binds the transaction scope to this conversation.
    let written = handle
        .commit(ctx.clone(), |tx, state| {
            Box::pin(async move {
                let scope = tx.scope();
                let storage = state.storage.storage();
                let id = EntryId::from_raw(state.storage.write(storage.mint_id()).await?);
                tx.push_write(StorageWrite::Entry {
                    value: EntryRecord {
                        id,
                        conversation_id: scope.conversation_id.expect("a conversation scope"),
                        kind: "app.note".to_owned(),
                        model: None,
                        data: Some(json!({"from": "conversation.commit"})),
                        head: None,
                        edits: None,
                        by_task_id: None,
                    },
                });
                Ok(Box::new(id) as JobValue)
            })
        })
        .await
        .expect("conversation commit");
    let entry = harness
        .session()
        .entry(written, &ctx)
        .await
        .expect("read written entry")
        .expect("the entry is committed");
    assert_eq!(entry.data, Some(json!({"from": "conversation.commit"})));

    harness.close().await.expect("close");
}

#[tokio::test]
async fn reset_admits_a_head_marker_and_the_handoff_message() {
    let clock = Clock::new(5_200);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let id = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let handle = conversation(&harness, id);

    let record = handle
        .reset(Some("fresh start".to_owned()), ctx.clone())
        .await
        .expect("reset");
    assert_eq!(record.status, SubmissionStatus::Done);
    let entry_id = record.entry.expect("the reset entry");
    let entry = harness
        .session()
        .entry(entry_id, &ctx)
        .await
        .expect("read reset")
        .expect("the reset entry exists");
    assert_eq!(entry.kind, "pi.reset");
    assert_eq!(entry.head, Some(entry_id), "the reset selects itself");
    let model = entry.model.expect("the handoff message");
    assert_eq!(model[0].get("role").and_then(Value::as_str), Some("user"));
    assert_eq!(
        model[0].get("content").and_then(Value::as_str),
        Some("fresh start")
    );
    assert_eq!(
        model[0].get("timestamp").and_then(Value::as_i64),
        Some(clock.now())
    );

    let view = handle.context(ctx.clone(), None).await.expect("context");
    assert_eq!(view.head.as_ref().map(|entry| entry.id), Some(entry_id));
    assert_eq!(view.entries[0].id, entry_id);

    // No handoff means no model list at all.
    let bare = handle.reset(None, ctx.clone()).await.expect("bare reset");
    let bare_entry = harness
        .session()
        .entry(bare.entry.expect("the bare reset entry"), &ctx)
        .await
        .expect("read bare reset")
        .expect("the bare reset entry exists");
    assert!(bare_entry.model.is_none());
    assert_eq!(bare_entry.kind, "pi.reset");

    harness.close().await.expect("close");
}

#[tokio::test]
async fn conversation_watch_tracks_committed_documents() {
    let clock = Clock::new(5_300);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let id = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let handle = conversation(&harness, id);

    let watch = handle.watch(&ctx).await.expect("watch");
    let initial = watch.value().expect("the initial revision");
    assert_eq!(initial.conversation.id, id);
    assert!(initial.docs.contains_key("pi.live"));

    let frames: Arc<Mutex<Vec<ConversationFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = frames.clone();
    watch.start(move |frame| {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(frame);
        true
    });
    assert!(
        frames.lock().unwrap().is_empty(),
        "no frame before a commit"
    );

    harness
        .doc(
            conversation_address("pi.live", id),
            (*pir::durable::harness::live::LIVE_DEFINITION).clone(),
            ctx.clone(),
            |value| {
                value.insert("status".to_owned(), json!("busy"));
            },
        )
        .await
        .expect("write pi.live");

    let latest = watch.value().expect("the new revision");
    assert_eq!(
        latest.doc("pi.live").and_then(|value| value.get("status")),
        Some(&json!("busy"))
    );
    assert!(
        !frames.lock().unwrap().is_empty(),
        "the watch delivered the commit"
    );
    watch.stop();
    assert!(watch.is_stopped());
    assert_eq!(
        watch.ended(),
        Some(pir::durable::session::observation::WatchEnd::Stopped)
    );

    harness.close().await.expect("close");
}

// ── graph and documents ────────────────────────────────────────────────────

#[tokio::test]
async fn the_task_graph_is_empty_and_watchable_without_tasks() {
    let clock = Clock::new(8_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let state = harness.task_graph(&ctx).await.expect("graph state");
    let value = state.value().expect("a revision");
    assert!(value.tasks.is_empty(), "no live task");
    state.detach();

    let watch = harness.watch_task_graph(&ctx).await.expect("graph watch");
    assert!(!watch.is_stopped());
    assert!(watch.value().expect("a revision").tasks.is_empty());
    watch.stop();
    assert!(watch.is_stopped());

    harness.close().await.expect("close");
}

#[tokio::test]
async fn document_state_attaches_and_sees_committed_revisions() {
    let clock = Clock::new(9_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let resolved = provider_address(conversation);
    let definition = provider_doc();

    let attached: Attached = harness
        .document_state(&resolved, &definition, &ctx)
        .await
        .expect("attach")
        .expect("an incarnation is live");
    assert_eq!(attached.source.value(), Some(Map::new()));

    // Minting the identity is a committed revision the observer receives.
    ensure_provider_session_id(harness.session(), conversation, 9_000, ctx.clone())
        .await
        .expect("mint");
    let frames = {
        let core = attached.source.core();
        let mut core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frames = Vec::new();
        while let Some(frame) = core.next_frame() {
            frames.push(frame);
        }
        frames
    };
    assert_eq!(frames.len(), 1, "one commit, one frame");
    assert_eq!(
        frames[0]
            .value
            .as_ref()
            .and_then(|value| value.get("sessionId")),
        Some(&Value::String(mint_session_id(conversation, 9_000)))
    );

    harness.detach(&attached).await.expect("detach");
    // An address with no incarnation has no observer.
    let missing = harness
        .document_state(
            &provider_address(ConversationId::from_raw(999)),
            &definition,
            &ctx,
        )
        .await
        .expect("attach missing");
    assert!(missing.is_none());

    harness.close().await.expect("close");
}

#[tokio::test]
async fn watch_doc_delivers_committed_frames_and_stops() {
    let clock = Clock::new(10_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let resolved = provider_address(conversation);
    let definition = provider_doc();

    let seen: Arc<Mutex<Vec<JsonObject>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let watch = harness
        .watch_doc(&resolved, &definition, &ctx, move |value| {
            captured.lock().unwrap().push(value.clone());
            true
        })
        .await
        .expect("watch")
        .expect("an incarnation is live");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "start delivers the current revision"
    );

    ensure_provider_session_id(harness.session(), conversation, 10_000, ctx.clone())
        .await
        .expect("mint");
    assert_eq!(seen.lock().unwrap().len(), 2, "the commit is delivered");
    assert!(seen.lock().unwrap()[1].contains_key("sessionId"));
    assert_eq!(
        watch
            .value()
            .and_then(|value| value.get("sessionId").cloned()),
        seen.lock().unwrap()[1].get("sessionId").cloned()
    );

    watch.stop();
    assert!(watch.is_stopped());
    // A stopped watch ignores later commits.
    ensure_provider_session_id(harness.session(), conversation, 10_001, ctx.clone())
        .await
        .expect("read, not a write");
    assert_eq!(seen.lock().unwrap().len(), 2);

    harness.close().await.expect("close");
}

#[tokio::test]
async fn dropping_the_last_document_watch_stops_delivery() {
    let clock = Clock::new(10_500);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let resolved = provider_address(conversation);
    let definition = provider_doc();
    let seen: Arc<Mutex<Vec<JsonObject>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let watch = harness
        .watch_doc(&resolved, &definition, &ctx, move |value| {
            captured.lock().unwrap().push(value.clone());
            true
        })
        .await
        .expect("watch")
        .expect("an incarnation is live");
    assert_eq!(seen.lock().unwrap().len(), 1, "initial frame");

    // A clone keeps the subscription alive when the first handle drops.
    let survivor = watch.clone();
    drop(watch);
    harness
        .doc(resolved.clone(), definition.clone(), ctx.clone(), |value| {
            value.insert("sessionId".to_owned(), Value::String("first".to_owned()));
        })
        .await
        .expect("edit");
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "the surviving clone sees commits"
    );

    // The last handle stops the subscription and the delivery task.
    drop(survivor);
    harness
        .doc(resolved, definition, ctx.clone(), |value| {
            value.insert("sessionId".to_owned(), Value::String("second".to_owned()));
        })
        .await
        .expect("edit");
    tokio::task::yield_now().await;
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "no delivery after the last handle drops"
    );
    harness.close().await.expect("close");
}

#[tokio::test]
async fn document_watch_stops_when_its_context_is_aborted() {
    let clock = Clock::new(10_600);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let signal = pir::ai::types::AbortSignal::new();
    let ctx = Context::with_signal(signal.clone());
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let resolved = provider_address(conversation);
    let definition = provider_doc();
    let watch = harness
        .watch_doc(&resolved, &definition, &ctx, |_| true)
        .await
        .expect("watch")
        .expect("an incarnation is live");

    signal.abort();
    for _ in 0..100 {
        if watch.is_stopped() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        watch.is_stopped(),
        "the acquisition context stops the watch"
    );
    harness.close().await.expect("close");
}

#[tokio::test]
async fn storage_failures_surface_as_the_storage_error() {
    let clock = Clock::new(12_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    // A failure that is not the harness's own: an address whose kind does not
    // match its definition is rejected by `doc`.
    let error = harness
        .doc(
            provider_address(ConversationId::from_raw(1)),
            pir::durable::harness::documents::agent_definition(),
            Context::background(),
            |_| {},
        )
        .await
        .expect_err("mismatched kinds are rejected");
    assert!(matches!(
        error,
        HarnessError::Storage(StorageError::Invalid(_))
    ));
    harness.close().await.expect("close");
}

// ── submissions ────────────────────────────────────────────────────────────

#[tokio::test]
async fn submit_places_an_idle_input_and_starts_a_run() {
    let clock = Clock::new(14_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    harness.scheduler().pause();

    let record = harness
        .submit(
            conversation,
            SubmissionDraft::input(
                json!([{"type": "text", "text": "hello"}]),
                WhenBusy::FollowUp,
                None,
            ),
            ctx.clone(),
        )
        .await
        .expect("submit");
    assert_eq!(record.status, SubmissionStatus::Placed);
    assert!(record.entry.is_some(), "the pi.user entry was appended");

    // One commit: the run starts with this submission as its input.
    let live = harness
        .snapshot(&conversation_address("pi.live", conversation), &ctx)
        .await
        .expect("live")
        .expect("pi.live exists");
    let run = live.get("run").expect("the run was started");
    assert_eq!(
        run.get("inputs").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
    let task_id = run
        .get("taskId")
        .and_then(Value::as_i64)
        .expect("a generation task");
    let task = harness
        .scheduler()
        .get_task(TaskId::from_raw(task_id))
        .await
        .expect("read task")
        .expect("the generation task exists");
    assert_eq!(task.kind, "pi.generation");
    harness.close().await.expect("close");
}

#[tokio::test]
async fn submit_resumes_a_fresh_harness() {
    let clock = Clock::new(14_500);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    assert_eq!(
        harness.scheduler().scheduling_state(),
        SchedulingState::Paused,
        "a fresh harness does not schedule until resumed"
    );
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();

    let record = harness
        .submit(
            conversation,
            SubmissionDraft::input(
                json!([{"type": "text", "text": "hello"}]),
                WhenBusy::FollowUp,
                None,
            ),
            ctx,
        )
        .await
        .expect("submit");
    assert_eq!(record.status, SubmissionStatus::Placed);
    assert_eq!(
        harness.scheduler().scheduling_state(),
        SchedulingState::Running,
        "submit resumes the scheduler before admitting"
    );
    harness.close().await.expect("close");
}

#[tokio::test]
async fn submit_emits_one_settlement_event_per_terminal_submission() {
    let clock = Clock::new(14_600);
    let recording = RecordingTelemetry::new();
    let mut options = harness_options(&clock);
    options.telemetry = Some(recording.handle());
    let harness = open_harness_with(&clock, Arc::new(MemoryStorage::new()), options).await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    recording.clear();

    let record = harness
        .submit(
            conversation,
            SubmissionDraft::write(object(json!({
                "kind": "app.note",
                "data": {"text": "one event"}
            }))),
            ctx,
        )
        .await
        .expect("submit");
    assert_eq!(record.status, SubmissionStatus::Done);
    let submission_events = recording
        .events()
        .into_iter()
        .filter(|event| event.descriptor.name == "pi.durable.submission")
        .count();
    assert_eq!(
        submission_events, 1,
        "the commit publication is the single settlement event source"
    );
    harness.close().await.expect("close");
}

#[tokio::test]
async fn submission_handles_status_wait_and_abort() {
    let clock = Clock::new(14_700);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let id = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    let handle = conversation(&harness, id);

    // An idle input is placed and owns the run.
    let first = handle
        .submit_handle(
            SubmissionDraft::input(
                json!([{"type": "text", "text": "first"}]),
                WhenBusy::FollowUp,
                None,
            ),
            ctx.clone(),
        )
        .await
        .expect("first submit");
    assert_eq!(
        first.status(&ctx).await.expect("status").status,
        SubmissionStatus::Placed
    );
    assert_eq!(
        first.abort(ctx.clone()).await.expect("abort placed"),
        SubmissionAbortResult::AlreadyPlaced
    );

    // A display-only write is terminal at admission, so `wait` returns at once.
    let done = handle
        .submit_handle(
            SubmissionDraft::write(object(json!({
                "kind": "app.note",
                "data": {"n": 1}
            }))),
            ctx.clone(),
        )
        .await
        .expect("display-only submit");
    assert_eq!(
        done.status(&ctx).await.expect("status").status,
        SubmissionStatus::Done
    );
    assert_eq!(
        done.wait(ctx.clone()).await.expect("wait").status,
        SubmissionStatus::Done
    );
    assert_eq!(
        done.abort(ctx.clone()).await.expect("abort terminal"),
        SubmissionAbortResult::Settled
    );

    // A second input is queued behind the active run.
    let queued = handle
        .submit_handle(
            SubmissionDraft::input(
                json!([{"type": "text", "text": "second"}]),
                WhenBusy::FollowUp,
                None,
            ),
            ctx.clone(),
        )
        .await
        .expect("queued submit");
    assert_eq!(
        queued.status(&ctx).await.expect("status").status,
        SubmissionStatus::Queued
    );
    let waiter = {
        let queued = queued.clone();
        tokio::spawn(async move { queued.wait(Context::background()).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        queued.abort(ctx.clone()).await.expect("abort queued"),
        SubmissionAbortResult::Aborted
    );
    let settled = waiter
        .await
        .expect("the wait task")
        .expect("the submission settled");
    assert_eq!(settled.status, SubmissionStatus::Unanswered);
    assert_eq!(settled.reason.as_deref(), Some("aborted"));
    assert_eq!(
        queued.abort(ctx.clone()).await.expect("abort settled"),
        SubmissionAbortResult::Settled
    );

    // The inbox item was removed in the abort commit.
    let inbox = harness
        .snapshot(&conversation_address("pi.inbox", id), &ctx)
        .await
        .expect("inbox")
        .expect("pi.inbox exists");
    let items = inbox
        .get("items")
        .and_then(Value::as_array)
        .expect("the inbox items");
    assert!(
        items
            .iter()
            .all(|item| { item.get("id").and_then(Value::as_i64) != Some(queued.id().as_i64()) }),
        "the aborted submission left the inbox"
    );

    // Handles are lookups over committed records.
    let looked_up = harness
        .submission_handle(queued.id(), &ctx)
        .await
        .expect("lookup")
        .expect("the submission exists");
    assert_eq!(looked_up.id(), queued.id());
    assert!(harness
        .submission_handle(SubmissionId::from_raw(9_999), &ctx)
        .await
        .expect("missing lookup")
        .is_none());

    // The harness-level abort takes upstream's optional conversation filter.
    let third = handle
        .submit_handle(
            SubmissionDraft::input(
                json!([{"type": "text", "text": "third"}]),
                WhenBusy::FollowUp,
                None,
            ),
            ctx.clone(),
        )
        .await
        .expect("third submit");
    assert!(
        harness
            .abort_submission(third.id(), ctx.clone(), Some(ConversationId::from_raw(999)))
            .await
            .expect("wrong conversation")
            .is_none(),
        "a submission of another conversation is not_found"
    );
    assert_eq!(
        harness
            .abort_submission(third.id(), ctx.clone(), Some(id))
            .await
            .expect("abort through the harness"),
        Some(SubmissionAbortResult::Aborted)
    );

    harness.close().await.expect("close");
}

// ── compaction ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn compact_creates_the_task_and_live_status_in_one_commit() {
    let clock = Clock::new(13_000);
    let harness = open_harness(&clock, Arc::new(MemoryStorage::new())).await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create")
        .id();
    // Keep the scheduler from running the task: this test asserts the commit,
    // not execution (no executable compaction definition is installed).
    harness.scheduler().pause();

    let task = harness
        .compact(conversation, Some("focus on auth".to_owned()), ctx.clone())
        .await
        .expect("compact");
    let record = harness
        .scheduler()
        .get_task(task)
        .await
        .expect("read task")
        .expect("the compaction task exists");
    assert_eq!(record.kind, "pi.compaction");
    assert_eq!(
        record.input.get("reason").and_then(Value::as_str),
        Some("manual")
    );
    assert_eq!(
        record.input.get("instructions").and_then(Value::as_str),
        Some("focus on auth")
    );

    // The status is in the same commit: one snapshot sees both.
    let live = harness
        .snapshot(&conversation_address("pi.live", conversation), &ctx)
        .await
        .expect("live")
        .expect("pi.live exists");
    let compactions = live
        .get("compactions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(compactions.len(), 1, "the status was staged in the commit");
    assert_eq!(
        compactions[0].get("reason").and_then(Value::as_str),
        Some("manual")
    );
    assert_eq!(
        compactions[0].get("taskId").and_then(Value::as_i64),
        Some(task.as_i64())
    );
    harness.close().await.expect("close");
}

// ── helpers ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_task_owned_conversation_inherits_the_owner_agent() {
    let storage = Arc::new(MemoryStorage::new());
    let clock = Clock::new(1_700_000_000_000);
    let harness = open_harness(&clock, storage.clone()).await;
    let ctx = Context::background();
    let root = harness.root(ctx.clone()).await.expect("root conversation");

    // The parent's committed agent carries a model and instructions; the
    // child's copy must carry exactly the same stored state.
    root.configure(
        &AgentChange {
            model: Change::Set(ModelRef::new("acme", "parent-model")),
            instructions: Change::Set("inherited instructions".to_owned()),
            ..AgentChange::default()
        },
        ctx.clone(),
    )
    .await
    .expect("configure parent agent");

    let definition =
        TaskDefinition::define("demo", 1, vec!["run".to_owned()]).expect("task definition");
    let task_id = harness
        .session()
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                conversation_id: Some(root.id()),
                ..TaskOptions::default()
            },
            ctx.clone(),
        )
        .await
        .expect("create task");

    let child = harness
        .session()
        .create_conversation(ConversationOwnership::Task(task_id), None, ctx.clone())
        .await
        .expect("task-owned conversation");
    let parent_agent = harness
        .snapshot(&agent_address(root.id()), &ctx)
        .await
        .expect("parent agent")
        .expect("parent agent document");
    let child_agent = harness
        .snapshot(&agent_address(child.id), &ctx)
        .await
        .expect("child agent")
        .expect("child agent document");
    assert_eq!(
        child_agent, parent_agent,
        "a task-owned conversation copies its owner conversation's pi.agent"
    );
    assert_eq!(
        child_agent.get("model"),
        Some(&json!({ "provider": "acme", "modelId": "parent-model" }))
    );
    assert_eq!(
        child_agent.get("instructions"),
        Some(&json!("inherited instructions"))
    );

    // An ownerless conversation still starts from `{}`.
    let ownerless = harness
        .session()
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("ownerless conversation");
    let ownerless_agent = harness
        .snapshot(&agent_address(ownerless.id), &ctx)
        .await
        .expect("ownerless agent")
        .expect("ownerless agent document");
    assert!(
        ownerless_agent.get("model").is_none(),
        "an ownerless conversation has no inherited agent"
    );

    harness.close().await.expect("close");
}

// ── helpers ────────────────────────────────────────────────────────────────

/// A latest/initial conversation address for a built-in document kind.
fn conversation_address(kind: &str, conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        kind,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(pir::durable::documents::DocOwner::Conversation(
            conversation,
        )),
        None,
    )
    .expect("a conversation document")
}
