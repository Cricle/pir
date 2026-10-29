//! Conformance tests for the built-in `pi.generation` task (spec §8.3).
//!
//! These exercise the ported `harness/generation.ts` against a real
//! [`MemoryStorage`] and the scheduler, on a fake clock, with fakes for the
//! task's seams ([`GenerationStore`], [`GenerationModels`], [`AgentSource`],
//! [`GenerationHooks`], [`CompactionTasks`], [`ToolTasks`]). Everything a test
//! observes is committed state or a seam call the fake recorded.
//!
//! | test | invariant |
//! |---|---|
//! | `phase_sequence_answers_and_settles_inputs` | prepare → request → classify → final boundary |
//! | `partial_throttle_commits_at_the_interval` | one partial commit per interval, none when the response beats it |
//! | `retry_backoff_sleeps_then_prepares_next_attempt` | a retryable error sleeps `retry_delay_ms`, then retries |
//! | `overflow_compacts_and_retries_once` | overflow → blocking compaction → prepare → request |
//! | `blocking_compaction_failure_settles_model_error` | a failed blocking compaction settles `unanswered`/`model_error` |
//! | `poll_without_a_deferred_seam_settles_model_error` | the documented open seam |
//! | `final_boundary_starts_the_next_run` | a queued follow-up starts a successor generation |
//! | `tool_round_hands_the_run_to_the_next_generation` | `tools` → postTools boundary → handover |
//! | `abort_cancels_the_deferred_poll_and_settles_aborted` | the abort protocol |

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context as ApiContext, DeferredHandle,
    Message, Model, ModelCost, ModelCostRates, StopReason, StreamOptions, TextContent,
    ThinkingLevel, ToolCall, Usage,
};
use pir::durable::documents::{address_id, DocDefinition};
use pir::durable::harness::agent::{Agent, ModelRef};
use pir::durable::harness::compaction::CompactionReason;
use pir::durable::harness::context::{derive_context, ContextView};
use pir::durable::harness::documents::conversation_definitions;
use pir::durable::harness::generation::{
    generation_task, AgentSource, CompactionTasks, DeferredPoll, DocumentKind, GenerationHarness,
    GenerationHooks, GenerationModels, GenerationStore, ToolControl, ToolTasks,
    COMPACTION_TASK_NAME, GENERATION_TASK_NAME, TOOL_TASK_NAME,
};
use pir::durable::harness::inbox::INBOX_KIND;
use pir::durable::harness::live::{live_from_object, LiveState, RunState, LIVE_KIND};
use pir::durable::harness::registry::{PromptSection, ToolRegistration};
use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::scheduler::{
    NextTaskState, ReportFn, TaskScheduler, TaskSchedulerOptions,
};
use pir::durable::harness::settings::Settings;
use pir::durable::harness::task::{define_task, phase_handler, Task};
use pir::durable::harness::usage::USAGE_KIND;
use pir::durable::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use pir::durable::session::documents::LoadedDocument;
use pir::durable::session::facade::{ConversationOwnership, Session, SubmissionCreate};
use pir::durable::session::transaction::{
    EntryDraft, StagedDocument, TaskOptions, TaskOwnership, Transaction,
};
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    DocumentAddress, DocumentPoint, DocumentScope, EntryQuery, EntryRecord, JsonObject, ScanOrder,
    Storage, SubmissionStatus, SubmissionType, TaskOutcome, TaskOutcomeError, TaskRecord,
    TaskState,
};
use pir::durable::Context;

// ── clock and world ────────────────────────────────────────────────────────

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

/// The documents of one conversation, read committed.
async fn read_document(
    storage: &Arc<MemoryStorage>,
    conversation: ConversationId,
    kind: &str,
) -> Option<JsonObject> {
    let address = DocumentAddress {
        kind: kind.to_owned(),
        scope: DocumentScope::Conversation {
            conversation_id: conversation,
        },
        key: None,
    };
    let record = storage
        .find_document(address, DocumentPoint::Current)
        .await
        .ok()??;
    storage
        .document(record.id, DocumentPoint::Current)
        .await
        .ok()?
        .map(|stored| stored.value)
}

// ── the store fake ─────────────────────────────────────────────────────────

/// A `GenerationStore` over the session's storage.
///
/// Committed reads go straight to storage for the first read and to a mirror
/// the commit listener keeps up to date thereafter, so they stay legal from
/// inside a commit callback (which must not wait on the session line). Writes
/// stage into the caller's transaction, which is the seam's contract.
struct Store {
    storage: Arc<MemoryStorage>,
    /// Committed documents by address id, kept by the commit listener.
    committed: Mutex<std::collections::HashMap<String, LoadedDocument>>,
}

impl Store {
    fn address(conversation: ConversationId, kind: DocumentKind) -> DocumentAddress {
        DocumentAddress {
            kind: kind.kind().to_owned(),
            scope: DocumentScope::Conversation {
                conversation_id: conversation,
            },
            key: None,
        }
    }

    fn semantics(kind: DocumentKind) -> pir::durable::DocumentSemantics {
        conversation_definitions()
            .into_iter()
            .find(|(name, _)| *name == kind.kind())
            .map(|(_, definition)| definition.common.semantics)
            .expect("a built-in conversation document")
    }

    fn initial(kind: DocumentKind) -> JsonObject {
        conversation_definitions()
            .into_iter()
            .find(|(name, _)| *name == kind.kind())
            .map(|(_, definition)| definition.initial_value())
            .expect("a built-in conversation document")
    }

    async fn committed(
        &self,
        conversation: ConversationId,
        kind: DocumentKind,
    ) -> Result<Option<LoadedDocument>, String> {
        let address = Self::address(conversation, kind);
        let address_id_value = address_id(&address);
        if let Some(committed) = self
            .committed
            .lock()
            .unwrap()
            .get(&address_id_value)
            .cloned()
        {
            return Ok(Some(committed));
        }
        let Some(record) = self
            .storage
            .find_document(address.clone(), DocumentPoint::Current)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let Some(stored) = self
            .storage
            .document(record.id, DocumentPoint::Current)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(LoadedDocument {
            address_id: address_id(&address),
            record,
            stored_version: stored.version,
            value_version: stored.version,
            deltas_since_base: stored.deltas_since_base,
            value: stored.value,
        }))
    }

    fn stage(
        &self,
        tx: &mut Transaction,
        conversation: ConversationId,
        kind: DocumentKind,
        value: JsonObject,
    ) -> Result<(), String> {
        let address = Self::address(conversation, kind);
        let address_id = address_id(&address);
        // Read-your-writes: a draft this commit already staged is the draft.
        if let Some(staged) = tx.latest_document_mut(&address_id) {
            staged.value = value;
            return Ok(());
        }
        // Otherwise the committed mirror is the base; a document in neither is
        // a creation.
        let loaded = self.committed.lock().unwrap().get(&address_id).cloned();
        let created = loaded.is_none();
        tx.stage_document(StagedDocument {
            address,
            address_id,
            definition: Some(DocDefinition {
                kind: kind.kind().to_owned(),
                version: 1,
                semantics: Self::semantics(kind),
            }),
            family: None,
            value,
            created,
            loaded,
            retire: false,
        });
        Ok(())
    }
}

impl GenerationStore for Store {
    fn document<'a>(
        &'a self,
        conversation: ConversationId,
        kind: DocumentKind,
    ) -> pir::durable::harness::generation::HarnessFuture<'a, JsonObject> {
        Box::pin(async move {
            Ok(self
                .committed(conversation, kind)
                .await?
                .map(|loaded| loaded.value)
                .unwrap_or_else(|| Store::initial(kind)))
        })
    }

    fn document_in(
        &self,
        tx: &Transaction,
        conversation: ConversationId,
        kind: DocumentKind,
    ) -> Result<JsonObject, String> {
        let address_id = address_id(&Self::address(conversation, kind));
        if let Some(staged) = tx.latest_document(&address_id) {
            return Ok(staged.value.clone());
        }
        // Read-your-writes miss: the mirror holds the committed value. A
        // document that is still absent is its definition's initial value.
        Ok(self
            .committed
            .lock()
            .unwrap()
            .get(&address_id)
            .map(|loaded| loaded.value.clone())
            .unwrap_or_else(|| Store::initial(kind)))
    }

    fn stage_document(
        &self,
        tx: &mut Transaction,
        conversation: ConversationId,
        kind: DocumentKind,
        value: JsonObject,
    ) -> Result<(), String> {
        self.stage(tx, conversation, kind, value)
    }

    fn append_entry<'a>(
        &'a self,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        draft: EntryDraft,
        by_task: TaskId,
    ) -> pir::durable::harness::generation::HarnessFuture<'a, EntryRecord> {
        Box::pin(async move {
            let raw = self
                .storage
                .mint_id()
                .await
                .map_err(|error| error.to_string())?;
            let id = EntryId::from_raw(raw);
            let head = match draft.head {
                Some(pir::durable::session::transaction::HeadTarget::SelfEntry) => Some(id),
                Some(pir::durable::session::transaction::HeadTarget::Entry(entry)) => Some(entry),
                None => None,
            };
            let record = EntryRecord {
                id,
                conversation_id: conversation,
                kind: draft.kind,
                model: draft.model,
                data: draft.data,
                head,
                edits: draft.edits,
                by_task_id: Some(by_task),
            };
            tx.push_write(pir::durable::types::StorageWrite::Entry {
                value: record.clone(),
            });
            Ok(record)
        })
    }

    fn create_task<'a>(
        &'a self,
        catalogue: &'a pir::durable::harness::scheduler::TaskCatalogue,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        kind: &'a str,
        input: Value,
        owner: Option<TaskId>,
        background: bool,
    ) -> pir::durable::harness::generation::HarnessFuture<'a, TaskId> {
        Box::pin(async move {
            let definition = catalogue
                .task(kind)
                .ok_or_else(|| format!("task {kind} is not installed"))?;
            let raw = self
                .storage
                .mint_id()
                .await
                .map_err(|error| error.to_string())?;
            let id = TaskId::from_raw(raw);
            let record = TaskRecord {
                trace: None,
                links: Vec::new(),
                id,
                conversation_id: conversation,
                kind: kind.to_owned(),
                version: definition.version(),
                input: input.clone(),
                owner,
                background,
                abort_requested: false,
                abort_reason: None,
                abandon_on_restart: None,
                started_at: None,
                ended_at: None,
                restarts: None,
                state: TaskState::Pending {
                    checkpoint: definition.initial_state(&input),
                },
                memos: None,
            };
            tx.stage_task(record, true);
            Ok(id)
        })
    }

    fn latest_entry(
        &self,
        conversation: ConversationId,
    ) -> pir::durable::harness::generation::HarnessFuture<'_, Option<EntryId>> {
        Box::pin(async move {
            let page = self
                .storage
                .scan_entries(
                    EntryQuery {
                        conversation_id: conversation,
                        min_entry_id: None,
                        max_entry_id: None,
                        order: Some(ScanOrder::Descending),
                    },
                    1,
                    None,
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(page.items.first().map(|entry| entry.id))
        })
    }

    fn latest_head(
        &self,
        conversation: ConversationId,
    ) -> pir::durable::harness::generation::HarnessFuture<'_, Option<EntryId>> {
        Box::pin(async move {
            Ok(self
                .storage
                .find_latest_head_marker(conversation, None)
                .await
                .map_err(|error| error.to_string())?
                .and_then(|entry| entry.head))
        })
    }

    fn context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
    ) -> pir::durable::harness::generation::HarnessFuture<'_, ContextView> {
        Box::pin(async move {
            let mut entries: Vec<EntryRecord> = Vec::new();
            let mut cursor = None;
            loop {
                let page = self
                    .storage
                    .scan_entries(
                        EntryQuery {
                            conversation_id: conversation,
                            min_entry_id: None,
                            max_entry_id: None,
                            order: Some(ScanOrder::Ascending),
                        },
                        64,
                        cursor,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                entries.extend(page.items);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            Ok(derive_context(&entries, at))
        })
    }

    fn entry(
        &self,
        id: EntryId,
    ) -> pir::durable::harness::generation::HarnessFuture<'_, Option<EntryRecord>> {
        Box::pin(async move {
            Ok(self
                .storage
                .entry(id)
                .await
                .map_err(|error| error.to_string())?
                .map(|entry| entry.entry))
        })
    }
}

// ── model and agent fakes ──────────────────────────────────────────────────

fn mock_model() -> Model {
    Model {
        id: "model-1".into(),
        name: "Model one".into(),
        api: "mock".into(),
        provider: "mock".into(),
        base_url: "http://127.0.0.1:1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 8_192,
        max_tokens: 1_024,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// What the recorded provider answers with, per request.
#[derive(Clone)]
enum Reply {
    /// Stream `deltas`, `gap_ms` apart, then a clean stop.
    Text {
        deltas: Vec<&'static str>,
        gap_ms: u64,
    },
    /// One tool call.
    Call(ToolCall),
    /// A retryable provider error.
    Error(&'static str),
}

struct Models {
    replies: Mutex<VecDeque<Reply>>,
    requests: AtomicUsize,
    #[allow(dead_code)]
    contexts: Mutex<Vec<Vec<Message>>>,
    deferred: Option<Arc<dyn DeferredPoll>>,
}

impl Models {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
            deferred: None,
        })
    }

    fn with_deferred(replies: Vec<Reply>, deferred: Arc<dyn DeferredPoll>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
            deferred: Some(deferred),
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn text_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "mock".into(),
        provider: "mock".into(),
        model: "model-1".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        usage: Usage::default(),
        ..Default::default()
    }
}

fn error_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "mock".into(),
        provider: "mock".into(),
        model: "model-1".into(),
        stop_reason: StopReason::Error,
        error_message: Some(text.to_owned()),
        timestamp: 1,
        ..Default::default()
    }
}

fn call_message(call: ToolCall) -> AssistantMessage {
    AssistantMessage {
        api: "mock".into(),
        provider: "mock".into(),
        model: "model-1".into(),
        content: vec![ContentBlock::ToolCall(call)],
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        ..Default::default()
    }
}

impl GenerationModels for Models {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        (provider == "mock" && model_id == "model-1").then(mock_model)
    }

    fn stream(
        &self,
        _model: &Model,
        context: Arc<ApiContext>,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context.messages.clone());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Error("no scripted reply left"));
        let stream = AssistantMessageEventStream::new();
        let output = stream.clone();
        tokio::spawn(async move {
            match reply {
                Reply::Text { deltas, gap_ms } => {
                    let mut text = String::new();
                    for delta in deltas {
                        text.push_str(delta);
                        let mut partial = text_message(&text);
                        partial.stop_reason = StopReason::Pending;
                        output
                            .push_await(AssistantMessageEvent::TextDelta {
                                content_index: 0,
                                delta: delta.to_owned(),
                                partial: Arc::new(partial),
                            })
                            .await;
                        if gap_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(gap_ms)).await;
                        }
                    }
                    output.push(AssistantMessageEvent::Done {
                        reason: pir::ai::types::TerminalReason::Stop,
                        message: text_message(&text),
                    });
                }
                Reply::Call(call) => {
                    output.push(AssistantMessageEvent::Done {
                        reason: pir::ai::types::TerminalReason::ToolUse,
                        message: call_message(call),
                    });
                }
                Reply::Error(message) => {
                    output.push(AssistantMessageEvent::Error {
                        reason: pir::ai::types::ErrorReason::Error,
                        error: error_message(message),
                    });
                }
            }
        });
        stream
    }

    fn deferred(&self) -> Option<Arc<dyn DeferredPoll>> {
        self.deferred.clone()
    }
}

/// An agent source with one model and an optional tool list.
struct Agents {
    tools: Vec<ToolRegistration>,
    model: Option<ModelRef>,
}

impl Agents {
    fn new(tools: Vec<ToolRegistration>) -> Arc<Self> {
        Arc::new(Self {
            tools,
            model: Some(ModelRef::new("mock", "model-1")),
        })
    }

    fn without_model() -> Arc<Self> {
        Arc::new(Self {
            tools: Vec::new(),
            model: None,
        })
    }
}

impl AgentSource for Agents {
    fn resolve(&self, _conversation: ConversationId, _state: &JsonObject) -> Agent {
        Agent {
            model: self.model.clone(),
            thinking_level: ThinkingLevel::Off,
            extensions: Vec::new(),
            tools: self.tools.clone(),
            callable: Vec::new(),
            deferred: Vec::new(),
            sections: vec![PromptSection::text("system", "You are a test agent.")],
            instructions: None,
            cwd: None,
        }
    }
}

// ── compaction and tool fakes ──────────────────────────────────────────────

/// `createCompaction`: creates a real child task of the fake compaction
/// definition, which settles `completed` with `entryId` or `failed`.
struct Compactions {
    child: &'static str,
    storage: Arc<MemoryStorage>,
    created: Mutex<Vec<(CompactionReason, Option<TaskId>)>>,
}

impl Compactions {
    fn new(child: &'static str, storage: Arc<MemoryStorage>) -> Arc<Self> {
        Arc::new(Self {
            child,
            storage,
            created: Mutex::new(Vec::new()),
        })
    }

    fn created(&self) -> Vec<(CompactionReason, Option<TaskId>)> {
        self.created.lock().unwrap().clone()
    }
}

impl CompactionTasks for Compactions {
    fn create<'a>(
        &'a self,
        catalogue: &'a pir::durable::harness::scheduler::TaskCatalogue,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        reason: CompactionReason,
        owner: Option<TaskId>,
    ) -> pir::durable::harness::generation::HarnessFuture<'a, TaskId> {
        Box::pin(async move {
            self.created.lock().unwrap().push((reason, owner));
            let definition = catalogue
                .task(self.child)
                .ok_or_else(|| format!("{} is not installed", self.child))?;
            let raw = self
                .storage
                .mint_id()
                .await
                .map_err(|error| error.to_string())?;
            let id = TaskId::from_raw(raw);
            let input = json!({ "reason": reason.as_str() });
            tx.stage_task(
                TaskRecord {
                    trace: None,
                    links: Vec::new(),
                    id,
                    conversation_id: conversation,
                    kind: self.child.to_owned(),
                    version: definition.version(),
                    input: input.clone(),
                    owner,
                    background: owner.is_none(),
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: None,
                    started_at: None,
                    ended_at: None,
                    restarts: None,
                    state: TaskState::Pending {
                        checkpoint: definition.initial_state(&input),
                    },
                    memos: None,
                },
                true,
            );
            Ok(id)
        })
    }
}

/// The tool seam fake: records calls and reports no controls.
#[derive(Default)]
struct Tools {
    errors: Mutex<Vec<(String, String)>>,
    added: Mutex<Vec<String>>,
}

impl ToolTasks for Tools {
    fn control(&self, _outcome: &TaskOutcome) -> Option<ToolControl> {
        None
    }

    fn append_error<'a>(
        &'a self,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        call: &'a ToolCall,
        reason: &'a str,
        message: &'a str,
        timestamp: i64,
    ) -> pir::durable::harness::generation::HarnessFuture<'a, EntryId> {
        Box::pin(async move {
            self.errors
                .lock()
                .unwrap()
                .push((reason.to_owned(), message.to_owned()));
            let raw = 900_000 + self.errors.lock().unwrap().len() as i64;
            let id = EntryId::from_raw(raw);
            tx.push_write(pir::durable::types::StorageWrite::Entry {
                value: EntryRecord {
                    id,
                    conversation_id: conversation,
                    kind: "pi.tool-result".to_owned(),
                    model: Some(vec![json!({
                        "role": "toolResult",
                        "toolCallId": call.id,
                        "toolName": call.name,
                        "content": [{"type": "text", "text": message}],
                        "isError": true,
                        "timestamp": timestamp,
                    })]),
                    data: None,
                    head: None,
                    edits: None,
                    by_task_id: None,
                },
            });
            Ok(id)
        })
    }

    fn add_tools<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _conversation: ConversationId,
        names: &'a [String],
    ) -> pir::durable::harness::generation::HarnessFuture<'a, ()> {
        Box::pin(async move {
            self.added.lock().unwrap().extend(names.iter().cloned());
            Ok(())
        })
    }
}

// ── the world ──────────────────────────────────────────────────────────────

/// A generation task installed beside the scheduler, over one conversation.
struct World {
    storage: Arc<MemoryStorage>,
    scheduler: TaskScheduler,
    session: Session,
    clock: Clock,
    conversation: ConversationId,
    store: Arc<Store>,
    compactions: Option<Arc<Compactions>>,
}

impl World {
    async fn new(models: Arc<Models>, tools: Arc<Tools>, settings: Settings) -> Self {
        Self::build(
            models,
            tools,
            false,
            settings,
            Agents::new(Vec::new()),
            None,
        )
        .await
    }

    async fn build(
        models: Arc<Models>,
        tools: Arc<Tools>,
        wanted_compaction: bool,
        settings: Settings,
        agents: Arc<Agents>,
        extra_tasks: Option<Vec<Task>>,
    ) -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let compactions =
            wanted_compaction.then(|| Compactions::new(COMPACTION_TASK_NAME, storage.clone()));
        let clock = Clock::new(10_000);
        let settings = Arc::new(Mutex::new(settings));
        let store = Arc::new(Store {
            storage: storage.clone(),
            committed: Mutex::new(Default::default()),
        });
        struct Observer;
        impl GenerationHooks for Observer {}

        let harness = Arc::new(GenerationHarness {
            settings: {
                let settings = settings.clone();
                Arc::new(move || settings.lock().unwrap().clone())
            },
            store: store.clone(),
            agent: agents,
            models: models.clone(),
            hooks: Arc::new(Observer),
            compaction: compactions.clone().map(|c| c as Arc<dyn CompactionTasks>),
            tools: Some(tools.clone() as Arc<dyn ToolTasks>),
        });

        let mut tasks = vec![generation_task(harness)];
        if let Some(extra) = extra_tasks {
            tasks.extend(extra);
        }
        let clock_for_now = clock.clone();
        let settings_for_scheduler = settings.clone();
        let report: ReportFn = Arc::new(|_, _| {});
        let options = TaskSchedulerOptions {
            now: Some(Arc::new(move || clock_for_now.now())),
            settings: Some(Arc::new(move || {
                settings_for_scheduler.lock().unwrap().clone()
            })),
            report: Some(report),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, storage.clone(), tasks)
            .await
            .expect("open scheduler");
        let session = scheduler.session().clone();
        {
            let store = store.clone();
            session
                .subscribe_commits(Arc::new(move |publication, _context| {
                    use pir::durable::session::publication::CommitChange;
                    use pir::durable::types::DocumentCommitChange;
                    for change in &publication.changes {
                        let CommitChange::Document(DocumentCommitChange::Document {
                            record,
                            value,
                            version,
                            ..
                        }) = change
                        else {
                            continue;
                        };
                        let address = DocumentAddress {
                            kind: record.kind.clone(),
                            scope: record.scope,
                            key: record.key.clone(),
                        };
                        let id = address_id(&address);
                        let mut committed = store.committed.lock().unwrap();
                        match value {
                            Some(value) => {
                                let version = version.unwrap_or(1);
                                committed.insert(
                                    id.clone(),
                                    LoadedDocument {
                                        address_id: id,
                                        record: record.clone(),
                                        stored_version: version,
                                        value_version: version,
                                        deltas_since_base: 0,
                                        value: value.clone(),
                                    },
                                );
                            }
                            None => {
                                committed.remove(&id);
                            }
                        }
                    }
                }))
                .expect("subscribe to commits");
        }
        let conversation = session
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
            session,
            clock,
            conversation,
            store,
            compactions,
        }
    }

    /// Write one conversation document through a real commit.
    async fn write_document(&self, kind: &str, value: JsonObject) {
        let store = self.store.clone();
        let conversation = self.conversation;
        let kind = match kind {
            LIVE_KIND => DocumentKind::Live,
            INBOX_KIND => DocumentKind::Inbox,
            USAGE_KIND => DocumentKind::Usage,
            other => panic!("unsupported document {other}"),
        };
        let _: () = self
            .session
            .commit(Context::background(), move |tx, _state| {
                Box::pin(async move {
                    store
                        .stage(tx, conversation, kind, value)
                        .expect("stage document");
                    Ok(Box::new(()) as pir::durable::session::line::JobValue)
                })
            })
            .await
            .expect("write document");
    }

    async fn live(&self) -> LiveState {
        read_document(&self.storage, self.conversation, LIVE_KIND)
            .await
            .map(|value| live_from_object(&value))
            .unwrap_or_default()
    }

    /// Create a generation task with a custom starting checkpoint.
    async fn create_with(&self, checkpoint: Value) -> TaskId {
        let catalogue = self.scheduler.catalogue().snapshot();
        let definition = catalogue
            .task(GENERATION_TASK_NAME)
            .expect("generation installed");
        let id = self
            .scheduler
            .create_task(
                &definition,
                json!({}),
                TaskOptions {
                    trace: None,
                    conversation_id: Some(self.conversation),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create generation task");
        let mut record = self
            .scheduler
            .get_task(id)
            .await
            .expect("read task")
            .expect("task exists");
        record.state = TaskState::Pending { checkpoint };
        self.session
            .set_task(record, Context::background())
            .await
            .expect("set checkpoint");
        id
    }

    async fn record(&self, id: TaskId) -> TaskRecord {
        self.scheduler
            .get_task(id)
            .await
            .expect("read task")
            .expect("task exists")
    }

    async fn wait_terminal(&self, id: TaskId) -> TaskRecord {
        let record =
            tokio::time::timeout(Duration::from_secs(10), self.scheduler.wait_for_task(id))
                .await
                .unwrap_or_else(|_| panic!("task {id} did not settle; last {:?}", id))
                .expect("wait for task");
        assert!(
            matches!(record.state, TaskState::Terminal { .. }),
            "task {id} is {:?}",
            record.state
        );
        record
    }

    async fn wait_for<F>(&self, id: TaskId, mut predicate: F) -> TaskRecord
    where
        F: FnMut(&TaskRecord) -> bool,
    {
        for _ in 0..1_000 {
            let record = self.record(id).await;
            if predicate(&record) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "condition did not hold for task {id}; last {:?}",
            self.record(id).await
        );
    }

    /// A placed input the run can settle, plus the live run that names it.
    async fn start_run(&self, task: TaskId) -> SubmissionId {
        let submission = self
            .session
            .create_submission(
                SubmissionCreate {
                    status: SubmissionStatus::Placed,
                    ..SubmissionCreate::new(self.conversation, SubmissionType::Input)
                },
                Context::background(),
            )
            .await
            .expect("create submission")
            .id;
        self.write_document(
            LIVE_KIND,
            live_to_object(&LiveState {
                run: Some(RunState {
                    task_id: task,
                    inputs: vec![submission],
                }),
                ..LiveState::default()
            }),
        )
        .await;
        submission
    }

    async fn submission(&self, id: SubmissionId) -> pir::durable::types::SubmissionRecord {
        self.session
            .submission(id, &Context::background())
            .await
            .expect("read submission")
            .expect("submission exists")
    }
}

fn live_to_object(live: &LiveState) -> JsonObject {
    match serde_json::to_value(live) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

// ── task definitions used as children ──────────────────────────────────────

/// A `pi.compaction`-shaped child: commits `completed` with `entryId`.
fn completing_compaction() -> Task {
    define_task(
        COMPACTION_TASK_NAME,
        1,
        |_input| json!({ "phase": "select" }),
    )
    .phase(
        "select",
        phase_handler(
            |_task: &pir::durable::harness::task::RunningTask,
             runtime: &TaskRuntime,
             context: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime
                        .commit(
                            |_tx, _current| {
                                Box::pin(async move {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: json!({ "entryId": 77 }),
                                        },
                                    }))
                                })
                            },
                            context,
                        )
                        .await
                        .expect("compaction completes");
                })
            },
        ),
    )
    .build()
    .expect("valid fake compaction")
}

/// A `pi.compaction`-shaped child that fails with `message`.
fn failing_compaction(message: &'static str) -> Task {
    define_task(
        COMPACTION_TASK_NAME,
        1,
        |_input| json!({ "phase": "select" }),
    )
    .phase(
        "select",
        phase_handler(
            move |_task: &pir::durable::harness::task::RunningTask,
                  runtime: &TaskRuntime,
                  context: Context| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime
                        .commit(
                            move |_tx, _current| {
                                Box::pin(async move {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Failed {
                                            error: TaskOutcomeError {
                                                message: message.to_owned(),
                                                detail: None,
                                            },
                                            result: None,
                                        },
                                    }))
                                })
                            },
                            context,
                        )
                        .await
                        .expect("compaction fails");
                })
            },
        ),
    )
    .build()
    .expect("valid failing compaction")
}

/// A `pi.tool`-shaped child that commits `completed`.
fn completing_tool() -> Task {
    define_task(TOOL_TASK_NAME, 1, |_input| json!({ "phase": "call" }))
        .phase(
            "call",
            phase_handler(
                |_task: &pir::durable::harness::task::RunningTask,
                 runtime: &TaskRuntime,
                 context: Context| {
                    let runtime = runtime.clone();
                    Box::pin(async move {
                        runtime
                            .commit(
                                |_tx, _current| {
                                    Box::pin(async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: json!({ "kind": "model" }),
                                            },
                                        }))
                                    })
                                },
                                context,
                            )
                            .await
                            .expect("tool completes");
                    })
                },
            ),
        )
        .build()
        .expect("valid fake tool")
}

// ── helpers ────────────────────────────────────────────────────────────────

fn text_of(message: &AssistantMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    })
}

async fn entry_messages(
    storage: &Arc<MemoryStorage>,
    conversation: ConversationId,
) -> Vec<EntryRecord> {
    let page = storage
        .scan_entries(
            EntryQuery {
                conversation_id: conversation,
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            256,
            None,
        )
        .await
        .expect("scan entries");
    page.items
}

fn base_settings() -> Settings {
    Settings {
        progress: pir::durable::harness::settings::ProgressPolicy {
            partial_interval_ms: 60_000,
            output_interval_ms: 60_000,
        },
        ..Settings::default()
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn phase_sequence_answers_and_settles_inputs() {
    let models = Models::new(vec![Reply::Text {
        deltas: vec!["hello"],
        gap_ms: 0,
    }]);
    let world = World::new(models.clone(), Arc::new(Tools::default()), base_settings()).await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();

    let record = world.wait_terminal(task).await;
    let TaskOutcome::Completed { result } = outcome_of(&record) else {
        panic!("expected completed, got {:?}", record.state);
    };
    let answer = EntryId::from_raw(result["entryId"].as_i64().expect("an answer entry id"));

    // The assistant entry is committed with the model's text.
    let entries = entry_messages(&world.storage, world.conversation).await;
    let assistant = entries
        .iter()
        .find(|entry| entry.id == answer)
        .expect("the answer entry is committed");
    assert_eq!(assistant.kind, "pi.assistant");
    let message: AssistantMessage =
        serde_json::from_value(assistant.model.as_ref().unwrap()[0].clone()).unwrap();
    assert_eq!(text_of(&message).as_deref(), Some("hello"));

    // `prepare` planned a `pi.system` entry before the request.
    assert!(
        entries.iter().any(|entry| entry.kind == "pi.system"),
        "prepare must append the positional system entry"
    );
    assert_eq!(models.requests(), 1);

    // `pi.live` is back to idle and the input is answered.
    let live = world.live().await;
    assert!(live.run.is_none(), "the run is settled");
    assert!(live.generation.is_none(), "the generation is cleared");
    let submission = world.submission(input).await;
    assert_eq!(submission.status, SubmissionStatus::Done);
    assert_eq!(submission.answer, Some(answer));

    // `pi.usage` recorded the response.
    let usage = read_document(&world.storage, world.conversation, USAGE_KIND)
        .await
        .expect("pi.usage was written");
    assert!(usage
        .get("models")
        .is_some_and(|models| !models.as_object().unwrap().is_empty()));
}

fn outcome_of(record: &TaskRecord) -> TaskOutcome {
    match &record.state {
        TaskState::Terminal { outcome } | TaskState::Completing { outcome } => outcome.clone(),
        other => panic!("expected a settled record, got {other:?}"),
    }
}

#[tokio::test]
async fn partial_throttle_commits_at_the_interval() {
    // A 60 s interval cannot be reached by a fast answer: no partial lands,
    // and the classification commit still writes the terminal entry.
    let models = Models::new(vec![Reply::Text {
        deltas: vec!["a", "b", "c"],
        gap_ms: 0,
    }]);
    let world = World::new(models, Arc::new(Tools::default()), base_settings()).await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();
    world.wait_terminal(task).await;
    assert_eq!(world.submission(input).await.status, SubmissionStatus::Done);

    // With a 1 ms interval and deltas 30 ms apart, partials do commit.
    let mut settings = base_settings();
    settings.progress = pir::durable::harness::settings::ProgressPolicy {
        partial_interval_ms: 1,
        output_interval_ms: 1,
    };
    let models = Models::new(vec![Reply::Text {
        deltas: vec!["one", "two", "three", "four"],
        gap_ms: 30,
    }]);
    let world = World::new(models, Arc::new(Tools::default()), settings).await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    world.start_run(task).await;
    world.scheduler.resume();

    // Observe the committed partial while the request is in flight.
    let mut seen_partial = false;
    for _ in 0..400 {
        let live = world.live().await;
        if live
            .generation
            .as_ref()
            .and_then(|generation| generation.message.as_ref())
            .is_some()
        {
            seen_partial = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(seen_partial, "a throttled partial must be committed");

    let record = world.wait_terminal(task).await;
    assert!(matches!(outcome_of(&record), TaskOutcome::Completed { .. }));
    let live = world.live().await;
    assert!(
        live.generation.is_none(),
        "classification clears the partial"
    );
}

#[tokio::test]
async fn retry_backoff_sleeps_then_prepares_next_attempt() {
    let models = Models::new(vec![
        Reply::Error("503 service unavailable"),
        Reply::Text {
            deltas: vec!["recovered"],
            gap_ms: 0,
        },
    ]);
    let mut settings = base_settings();
    settings.retry = pir::durable::harness::settings::ConversationRetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 2_000,
        max_agent_delay_ms: 60_000,
    };
    let world = World::new(models.clone(), Arc::new(Tools::default()), settings).await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    world.start_run(task).await;
    world.scheduler.resume();

    let sleeping = world
        .wait_for(task, |record| {
            matches!(record.state, TaskState::Sleeping { .. })
        })
        .await;
    let TaskState::Sleeping { until, checkpoint } = sleeping.state else {
        unreachable!()
    };
    assert_eq!(
        until,
        world.clock.now() + 2_000,
        "retry_delay_ms(attempt 1)"
    );
    assert_eq!(checkpoint["phase"], "retry");
    // The failed attempt's usage is recorded and its message is committed.
    let live = world.live().await;
    assert!(live
        .generation
        .as_ref()
        .is_some_and(|generation| generation.retry.is_some()));

    world.clock.advance(2_000);
    world.scheduler.resume();
    let record = world.wait_terminal(task).await;
    assert!(matches!(outcome_of(&record), TaskOutcome::Completed { .. }));
    assert_eq!(models.requests(), 2, "the retry resends the request");
}

#[tokio::test]
async fn overflow_compacts_and_retries_once() {
    let models = Models::new(vec![
        Reply::Error("prompt is too long: 213462 tokens > 200000 maximum"),
        Reply::Text {
            deltas: vec!["after compaction"],
            gap_ms: 0,
        },
    ]);
    let wanted_compaction = true;
    let mut settings = base_settings();
    // Keep the threshold out of the way so only the overflow compacts, and let
    // any non-empty prefix be a candidate cut.
    settings.compaction.keep_recent_tokens = 1;
    settings.compaction.reserve_tokens = 1;
    settings.compaction.background_tokens = 0;
    let world = World::build(
        models.clone(),
        Arc::new(Tools::default()),
        wanted_compaction,
        settings,
        Agents::new(Vec::new()),
        Some(vec![completing_compaction()]),
    )
    .await;
    // Seed a transcript with a prefix the compaction could cut.
    for text in ["first", "answer", "second"] {
        world
            .session
            .append_entry(
                world.conversation,
                EntryDraft {
                    kind: "pi.user".to_owned(),
                    model: Some(vec![json!({
                        "role": "user",
                        "content": [{"type": "text", "text": text}],
                        "timestamp": 1,
                    })]),
                    ..EntryDraft::default()
                },
                Context::background(),
            )
            .await
            .expect("append seed entry");
    }
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();

    let record = world.wait_terminal(task).await;
    assert!(
        matches!(outcome_of(&record), TaskOutcome::Completed { .. }),
        "expected the retried request to answer, got {:?}",
        record.state
    );
    assert_eq!(models.requests(), 2, "one overflow, one compacted retry");
    assert_eq!(
        world.compactions.as_ref().unwrap().created().len(),
        1,
        "exactly one overflow compaction"
    );
    assert_eq!(world.submission(input).await.status, SubmissionStatus::Done);
}

#[tokio::test]
async fn blocking_compaction_failure_settles_model_error() {
    let models = Models::new(vec![Reply::Text {
        deltas: vec!["unused"],
        gap_ms: 0,
    }]);
    let world = World::build(
        models,
        Arc::new(Tools::default()),
        false,
        base_settings(),
        Agents::new(Vec::new()),
        Some(vec![failing_compaction("summary exploded")]),
    )
    .await;
    // A generation resuming from a blocking threshold compaction.
    let child = {
        let catalogue = world.scheduler.catalogue().snapshot();
        let definition = catalogue.task(COMPACTION_TASK_NAME).expect("compaction");
        world
            .scheduler
            .create_task(
                &definition,
                json!({}),
                TaskOptions {
                    trace: None,
                    conversation_id: Some(world.conversation),
                    ownership: TaskOwnership::Conversation,
                    background: true,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create compaction")
    };
    let task = world
        .create_with(json!({
            "phase": "prepare",
            "attempt": 1,
            "compacted": child,
            "threshold": true,
        }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();
    // The compaction is terminal before preparation resumes (a wait only
    // resumes once every task it names is terminal).
    world.wait_terminal(child).await;

    let record = world.wait_terminal(task).await;
    let TaskOutcome::Failed { error, .. } = outcome_of(&record) else {
        panic!("expected failed, got {:?}", record.state);
    };
    assert_eq!(error.message, "Compaction failed: summary exploded");
    assert_eq!(error.detail, Some(json!({ "reason": "model_error" })));
    let submission = world.submission(input).await;
    assert_eq!(submission.status, SubmissionStatus::Unanswered);
    assert_eq!(submission.reason.as_deref(), Some("model_error"));
    assert_eq!(
        submission.detail,
        Some(Value::String(
            "Compaction failed: summary exploded".to_owned()
        ))
    );
}

#[tokio::test]
async fn poll_without_a_deferred_seam_settles_model_error() {
    let models = Models::new(Vec::new());
    let world = World::new(models, Arc::new(Tools::default()), base_settings()).await;
    let task = world
        .create_with(json!({
            "phase": "poll",
            "attempt": 1,
            "model": { "provider": "mock", "modelId": "model-1" },
            "cutoff": 1,
            "handle": {
                "provider": "mock",
                "modelId": "model-1",
                "api": "mock",
                "id": "deferred-1",
            },
            "pollAt": 0,
        }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();

    let record = world.wait_terminal(task).await;
    let TaskOutcome::Failed { error, .. } = outcome_of(&record) else {
        panic!("expected failed, got {:?}", record.state);
    };
    assert!(
        error
            .message
            .contains("Deferred responses are not supported"),
        "documented seam message, got {}",
        error.message
    );
    assert_eq!(
        world.submission(input).await.status,
        SubmissionStatus::Unanswered
    );
}

#[tokio::test]
async fn final_boundary_starts_the_next_run() {
    // The first reply settles the original run; the successor's reply stays
    // open (a 60 s gap before its terminal event) so the placed follow-up is
    // observed before the successor can answer it.
    let models = Models::new(vec![
        Reply::Text {
            deltas: vec!["done"],
            gap_ms: 0,
        },
        Reply::Text {
            deltas: vec!["later"],
            gap_ms: 60_000,
        },
    ]);
    let world = World::new(models, Arc::new(Tools::default()), base_settings()).await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    let input = world.start_run(task).await;

    // A follow-up waiting for the final boundary.
    let follow_up = world
        .session
        .create_submission(
            SubmissionCreate::new(world.conversation, SubmissionType::Input),
            Context::background(),
        )
        .await
        .expect("create follow-up")
        .id;
    let mut inbox = pir::durable::harness::inbox::InboxState::default();
    inbox
        .items
        .push(pir::durable::harness::inbox::InboxItem::Input {
            id: follow_up,
            mode: pir::durable::harness::inbox::InputMode::FollowUp,
            content: json!([{"type": "text", "text": "and then?"}]),
            entry: None,
        });
    world
        .write_document(
            INBOX_KIND,
            match serde_json::to_value(&inbox) {
                Ok(Value::Object(object)) => object,
                _ => Map::new(),
            },
        )
        .await;
    // Start the run only after the follow-up is queued: the mock answers with
    // no gap, so resuming earlier would settle the run before the write lands.
    world.scheduler.resume();

    world.wait_terminal(task).await;
    assert_eq!(world.submission(input).await.status, SubmissionStatus::Done);
    // The placed follow-up belongs to a successor run.
    assert_eq!(
        world.submission(follow_up).await.status,
        SubmissionStatus::Placed
    );
    let live = world.live().await;
    let run = live.run.expect("a successor run started");
    assert_ne!(run.task_id, task);
    assert_eq!(run.inputs, vec![follow_up]);
}

#[tokio::test]
async fn tool_round_hands_the_run_to_the_next_generation() {
    let call = ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    };
    let models = Models::new(vec![
        Reply::Call(call),
        Reply::Text {
            deltas: vec!["after the tool"],
            gap_ms: 0,
        },
    ]);
    let agents = Agents::new(vec![ToolRegistration::new("echo")]);
    let world = World::build(
        models.clone(),
        Arc::new(Tools::default()),
        false,
        base_settings(),
        agents,
        Some(vec![completing_tool()]),
    )
    .await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();

    let record = world.wait_terminal(task).await;
    assert!(
        matches!(outcome_of(&record), TaskOutcome::Completed { .. }),
        "the tool round completes the generation, got {:?}",
        record.state
    );
    // The run moved to a successor generation and the round's slots are gone.
    let live = world.live().await;
    let run = live.run.expect("the run continues");
    assert_ne!(run.task_id, task);
    assert!(live.tools.is_none(), "postTools clears the round");
    assert_eq!(run.inputs, vec![input]);
    // The successor asks the model again, and the round's tool task ran.
    let successor = world.wait_terminal(run.task_id).await;
    assert!(matches!(
        outcome_of(&successor),
        TaskOutcome::Completed { .. }
    ));
    assert_eq!(models.requests(), 2);
}

#[tokio::test]
async fn abort_cancels_the_deferred_poll_and_settles_aborted() {
    struct Poller {
        cancelled: Arc<AtomicUsize>,
    }
    impl DeferredPoll for Poller {
        fn fetch<'a>(
            &'a self,
            _model: &'a Model,
            _handle: &'a DeferredHandle,
        ) -> pir::durable::harness::generation::HarnessFuture<'a, AssistantMessage> {
            Box::pin(async { Err("never fetched".to_owned()) })
        }

        fn cancel<'a>(
            &'a self,
            _model: &'a Model,
            _handle: &'a DeferredHandle,
        ) -> pir::durable::harness::generation::HarnessFuture<'a, ()> {
            Box::pin(async move {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }
    let cancelled = Arc::new(AtomicUsize::new(0));
    let models = Models::with_deferred(
        Vec::new(),
        Arc::new(Poller {
            cancelled: cancelled.clone(),
        }),
    );
    let world = World::new(models, Arc::new(Tools::default()), base_settings()).await;
    let task = world
        .create_with(json!({
            "phase": "poll",
            "attempt": 1,
            "model": { "provider": "mock", "modelId": "model-1" },
            "cutoff": 1,
            "handle": {
                "provider": "mock",
                "modelId": "model-1",
                "api": "mock",
                "id": "deferred-1",
            },
            "pollAt": i64::MAX / 2,
        }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();

    world
        .wait_for(task, |record| {
            matches!(record.state, TaskState::Sleeping { .. })
        })
        .await;
    world.scheduler.abort(task).await.expect("abort the task");
    let record = world.wait_terminal(task).await;
    assert!(
        matches!(outcome_of(&record), TaskOutcome::Aborted { .. }),
        "expected aborted, got {:?}",
        record.state
    );
    assert_eq!(cancelled.load(Ordering::SeqCst), 1, "the poll is cancelled");
    let submission = world.submission(input).await;
    assert_eq!(submission.status, SubmissionStatus::Unanswered);
    assert_eq!(submission.reason.as_deref(), Some("aborted"));
    assert!(world.live().await.run.is_none());
}

#[tokio::test]
async fn no_model_fails_before_any_request() {
    let models = Models::new(Vec::new());
    let world = World::build(
        models.clone(),
        Arc::new(Tools::default()),
        false,
        base_settings(),
        Agents::without_model(),
        None,
    )
    .await;
    let task = world
        .create_with(json!({ "phase": "prepare", "attempt": 1 }))
        .await;
    let input = world.start_run(task).await;
    world.scheduler.resume();

    let record = world.wait_terminal(task).await;
    let TaskOutcome::Failed { error, .. } = outcome_of(&record) else {
        panic!("expected failed, got {:?}", record.state);
    };
    assert_eq!(error.message, "No model is configured");
    assert_eq!(error.detail, Some(json!({ "reason": "no_model" })));
    assert_eq!(
        world.submission(input).await.reason.as_deref(),
        Some("no_model")
    );
    assert_eq!(models.requests(), 0);
}
