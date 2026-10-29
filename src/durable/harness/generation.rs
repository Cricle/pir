//! The built-in `pi.generation` task (spec §8.3).
//!
//! Port of upstream `packages/durable/src/harness/generation.ts` (774 lines):
//! prepare the positional prompt, request or poll the model, retry, classify
//! the response, run the round's tools, and place the final boundary.
//!
//! # The task
//!
//! [`generation_task`] builds the executable definition from five phases plus
//! the abort handler:
//!
//! | phase | upstream | what it does |
//! |---|---|---|
//! | `prepare` | `prepare` | resolve agent/model, plan `pi.system` entries, threshold compaction, request checkpoint |
//! | `request` | `request` | partial conversion, `before_request` hooks, drive the provider stream, classify |
//! | `retry` | `retry` | durable backoff via [`TaskRuntime::sleep_until`] |
//! | `poll` | `poll` | provider deferred result via [`DeferredPoll`] |
//! | `tools` | `tools` | create/wait for `pi.tool` tasks, finish the round, hand over |
//! | abort | `abort` | cancel a deferred poll, settle unstarted calls `aborted`, end the run |
//!
//! [`GenerationCheckpoint`] is the durable state; its wire shape is upstream's
//! (internally tagged on `phase`, camelCase fields).
//!
//! # Seams
//!
//! Upstream's `TaskRuntime` carries the harness itself: the resolved agent,
//! settings, the model registry, transcript reads, entry appends and typed
//! document drafts. pir's
//! [`TaskRuntime`](crate::durable::harness::scheduler::runtime::TaskRuntime)
//! deliberately stops at the task-state machine (see its module docs: "the
//! agent, model, environment and entry/context helpers belong to the harness
//! lanes that own those types"). This module therefore takes one bundle,
//! [`GenerationHarness`], of narrow traits:
//!
//! * [`AgentSource`] — `runtime.agent(context)`.
//! * [`GenerationModels`] — `runtime.models.getModel` / `streamSimple`, plus
//!   [`DeferredPoll`] for `fetchDeferred` / `cancelDeferred` (the provider
//!   registry has no such methods yet; [`GenerationModels::deferred`] returns
//!   `None` when the host has none).
//! * [`GenerationStore`] — commit-time writes (documents, entries, tasks) and
//!   the committed reads behind `runtime.context` / `runtime.entry` /
//!   `tx.latestHeadMarker`.
//! * [`CompactionTasks`] — `createCompaction` (upstream `compaction.ts`). The
//!   pure helpers (`threshold_compaction`, `select_cut`, `overflow_compaction`,
//!   `compaction_policy`) come from the landed
//!   [`crate::durable::harness::compaction`] module.
//! * [`ToolTasks`] — the tool lane's `appendToolResult` / `addTools` / tool
//!   controls (upstream `tool.ts`, not yet landed).
//! * [`GenerationHooks`] — `beforeRequest`, `afterResponse`, `onYield`,
//!   `afterTools`.
//!
//! # Deliberate deviations
//!
//! * **Commit grouping.** Upstream writes an entry, its usage, the `pi.live`
//!   draft and the next task state in *one* transaction. pir's runtime commit
//!   gives a phase `&mut Transaction` only (no `SessionState`), so entry
//!   appends and document drafts go through [`GenerationStore`], whose
//!   implementation stages into the *caller's* commit. The contract on every
//!   `*_in` method is exactly that: stage into `tx`, never open a commit.
//! * **No committed read *through the store* inside a commit.** The store's
//!   committed reads go through the session line; calling one from inside a
//!   commit callback would wait on the line that is running the callback.
//!   Phases therefore hoist those reads (context, entries) out of the commit
//!   and pass the values in. The **head marker** is the exception: the line
//!   attaches the backend to every commit, so `tx.latest_head` reads the
//!   revision the commit is staged against *without* going through the line —
//!   read-your-writes first, then the newest committed marker. That is why
//!   every boundary places its head from inside the commit (upstream
//!   `tx.latestHeadMarker`); a head committed between the phase's pre-read and
//!   this commit is never missed. The one other value that must be read
//!   "inside" upstream — the request's `cutoff` — is taken as the phase's read
//!   plus the ids of the entries the same commit appends; only display-only
//!   entries can land in between, and they contribute no model messages.
//! * **Partial throttle.** Upstream arms a `setTimer` flush. The port races the
//!   provider stream against a `tokio` sleep in one loop and awaits each flush
//!   inline, so no flush is ever detached: "stop the throttle and await
//!   in-flight commits" holds trivially before the classification commit.
//! * **Deferred responses.** [`DeferredPoll`] is an open seam; when the model
//!   seam offers no poller the `poll` phase settles the run `model_error` with
//!   a documented message instead of fetching.
//! * **`pi.provider`.** The id is minted as a UUIDv4 and staged in the
//!   generation's own commit. The provider lane's
//!   `ensure_provider_session_id` takes a `Session`, which a phase handler does
//!   not have; when the store seam gains a `Session` handle this should call
//!   that helper instead, so the id keeps the upstream UUIDv7 shape.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::types::{
    get_current_tools, AssistantMessage, AssistantMessageEvent, ContentBlock,
    Context as ApiContext, DeferredHandle, Message, Model, StopReason, StreamOptions, TextContent,
    ThinkingLevel, Tool, ToolCall, UserMessage, UserRole,
};
use crate::ai::util::{is_context_overflow, is_retryable_assistant_error, retry_delay_ms};
use crate::durable::documents::{address_id, AnyDocDefinition, DocDefinition};
use crate::durable::errors::StorageError;
use crate::durable::harness::agent::{Agent, ModelRef};
use crate::durable::harness::compaction::{self, CompactionReason, CompactionTrigger};
use crate::durable::harness::context::ContextView;
use crate::durable::harness::inbox::{
    self, Boundary, InboxItem, InboxState, InputEntry, SelectedKind,
};
use crate::durable::harness::live::{
    live_from_object, live_to_object, DeferredStatus, GenerationState, LiveState, RetryStatus,
    RunState, SlotProgress, SlotStatus, ToolSlot, LIVE_DEFINITION,
};
use crate::durable::harness::prompt::{plan_system_entries, render_sections, replay_sections};
use crate::durable::harness::registry::{PromptInput, ToolRegistration};
use crate::durable::harness::scheduler::runtime::{TaskRuntime, TaskRuntimeError};
use crate::durable::harness::scheduler::{NextTaskState, TaskCatalogue};
use crate::durable::harness::settings::{
    AfterFailedRun, ConversationStreamOptions, Settings, ToolExecutionMode,
};
use crate::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
use crate::durable::harness::usage::{usage_add, UsageScope, UsageState};
use crate::durable::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::durable::session::documents::LoadedDocument;
use crate::durable::session::line::SessionState;
use crate::durable::session::transaction::{
    EntryDraft, HeadTarget, StagedDocument, SubmissionChange, Transaction,
};
use crate::durable::types::{
    ContextEdit, DocumentAddress, DocumentPoint, DocumentScope, EntryRecord, JoinPolicy,
    JsonObject, StoredDocument, TaskOutcome, TaskOutcomeError,
};
use crate::durable::Context as DurableContext;

/// Registered task name (spec §8.3).
pub const GENERATION_TASK_NAME: &str = "pi.generation";
/// Definition version.
pub const GENERATION_VERSION: i64 = 1;
/// The tool task kind the `tools` phase creates (upstream `ToolTask`).
pub const TOOL_TASK_NAME: &str = "pi.tool";
/// The compaction task kind preparation creates (upstream `CompactionTask`).
pub const COMPACTION_TASK_NAME: &str = compaction::COMPACTION_TASK_NAME;
/// Default deferred poll interval (upstream `DEFAULT_POLL_AFTER_MS`).
pub const DEFAULT_POLL_AFTER_MS: i64 = 5_000;
/// Settlement reason of an input the run's reset cut before an answer.
pub const REASON_RESET: &str = "reset";

/// Future returned by the harness seams (the same shape as `task::BoxFuture`).
pub type HarnessFuture<'a, T> =
    Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send + 'a>>;

/// Future returned by a hook that cannot fail.
pub type HookFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// One conversation document the generation task reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    /// `pi.live`: run control and generation/tool presentation.
    Live,
    /// `pi.inbox`: queued submissions waiting for a boundary.
    Inbox,
    /// `pi.agent`: model, thinking level, tools, sections, instructions.
    Agent,
    /// `pi.usage`: the conversation's spend ledger.
    Usage,
    /// `pi.provider`: the provider session id.
    Provider,
}

impl DocumentKind {
    /// The persisted kind string.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::Live => "pi.live",
            Self::Inbox => "pi.inbox",
            Self::Agent => "pi.agent",
            Self::Usage => "pi.usage",
            Self::Provider => "pi.provider",
        }
    }
}

// ─── Checkpoint ─────────────────────────────────────────────────────────────

/// The durable state of one generation attempt.
///
/// Wire shape is upstream's: internally tagged on `phase`, camelCase fields
/// (`thinkingLevel`, `streamOptions`, `pollAt`, ...).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum GenerationCheckpoint {
    /// Render the prompt and tool loadout, then build the request.
    #[serde(rename_all = "camelCase")]
    Prepare {
        /// Attempt number, starting at one.
        attempt: i64,
        /// The blocking compaction this generation waits for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Overflow text of the compaction that started `compacted`; checked
        /// once when `prepare` resumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
        /// Set when the blocking threshold started `compacted`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<bool>,
    },
    /// Send the request with the pinned model, thinking level and options.
    #[serde(rename_all = "camelCase")]
    Request {
        /// Attempt number.
        attempt: i64,
        /// The compaction this attempt waited for, carried into classification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Provider/model of the request.
        model: ModelRef,
        /// Thinking level of the request.
        thinking_level: ThinkingLevel,
        /// Settings request options, pinned at preparation.
        stream_options: ConversationStreamOptions,
        /// Newest entry included in the request.
        cutoff: EntryId,
    },
    /// Durable backoff before the next attempt.
    Retry {
        /// Attempt number.
        attempt: i64,
        /// The compaction this attempt waited for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Harness-clock milliseconds at which the next attempt is eligible.
        until: i64,
    },
    /// The provider deferred the response; wake and fetch it at `pollAt`.
    #[serde(rename_all = "camelCase")]
    Poll {
        /// Attempt number.
        attempt: i64,
        /// The compaction this attempt waited for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Provider/model of the request.
        model: ModelRef,
        /// Newest entry included in the request.
        cutoff: EntryId,
        /// Provider-side deferred handle.
        handle: DeferredHandle,
        /// Harness-clock milliseconds of the next poll.
        poll_at: i64,
    },
    /// Waiting on the round's tool tasks, which the generation owns.
    Tools {
        /// The tool-calling answer.
        assistant: EntryId,
        /// Tool tasks created so far, in call order.
        #[serde(default)]
        tools: Vec<TaskId>,
        /// Calls of a sequential round not started yet, in call order.
        #[serde(default)]
        pending: Vec<String>,
    },
}

impl GenerationCheckpoint {
    /// The phase name of this checkpoint.
    #[must_use]
    pub fn phase(&self) -> &'static str {
        match self {
            Self::Prepare { .. } => "prepare",
            Self::Request { .. } => "request",
            Self::Retry { .. } => "retry",
            Self::Poll { .. } => "poll",
            Self::Tools { .. } => "tools",
        }
    }

    /// The compaction this checkpoint waits for, if any.
    #[must_use]
    pub fn compaction_child(&self) -> Option<TaskId> {
        match self {
            Self::Prepare { compacted, .. }
            | Self::Request { compacted, .. }
            | Self::Retry { compacted, .. }
            | Self::Poll { compacted, .. } => *compacted,
            Self::Tools { .. } => None,
        }
    }
}

/// A new generation starts at `prepare`, attempt one.
#[must_use]
pub fn initial_checkpoint() -> Value {
    json!({ "phase": "prepare", "attempt": 1 })
}

/// Parse a running task's checkpoint.
///
/// An unreadable checkpoint is a broken record (a version mismatch without a
/// migration); upstream throws, and so does the port — the scheduler catches
/// the panic and settles the task `faulted`.
fn checkpoint_of(task: &RunningTask) -> GenerationCheckpoint {
    serde_json::from_value(task.checkpoint.clone()).unwrap_or_else(|error| {
        panic!(
            "task {} has an unreadable pi.generation checkpoint: {error}",
            task.id
        )
    })
}

fn checkpoint_value(checkpoint: &GenerationCheckpoint) -> Value {
    serde_json::to_value(checkpoint).expect("a generation checkpoint is JSON-serializable")
}

/// Serialize a checkpoint into the `Value` a next state carries.
fn next_running(checkpoint: GenerationCheckpoint) -> NextTaskState {
    NextTaskState::Running {
        checkpoint: checkpoint_value(&checkpoint),
    }
}

// ─── Seams ──────────────────────────────────────────────────────────────────

/// Resolve the conversation's agent (upstream `runtime.agent(context)`).
///
/// The implementation applies the registry snapshot and the resolved settings
/// to the committed `pi.agent` value with
/// [`resolve_agent`](crate::durable::harness::agent::resolve_agent).
pub trait AgentSource: Send + Sync {
    /// Resolve `state` into the agent this request runs with.
    fn resolve(&self, conversation: ConversationId, state: &JsonObject) -> Agent;
}

/// Model lookup and streaming (upstream `runtime.models`).
pub trait GenerationModels: Send + Sync {
    /// The model behind a provider/model pair, when the catalog knows it.
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model>;

    /// Stream one request (upstream `models.streamSimple`).
    ///
    /// Implementations must encode provider failures as an error terminal
    /// event, never a panic.
    fn stream(
        &self,
        model: &Model,
        context: Arc<ApiContext>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream;

    /// The deferred-response poller, when the host has one.
    ///
    /// This is the open seam: `Provider` has no `fetch_deferred` /
    /// `cancel_deferred` yet, so the default is `None` and the `poll` phase
    /// fails with a documented message.
    fn deferred(&self) -> Option<Arc<dyn DeferredPoll>> {
        None
    }
}

/// Fetch and cancel a provider's deferred response (upstream
/// `models.fetchDeferred` / `models.cancelDeferred`).
pub trait DeferredPoll: Send + Sync {
    /// Fetch the terminal message for `handle`.
    fn fetch<'a>(
        &'a self,
        model: &'a Model,
        handle: &'a DeferredHandle,
    ) -> HarnessFuture<'a, AssistantMessage>;

    /// Cancel a deferred response the run no longer needs.
    fn cancel<'a>(&'a self, model: &'a Model, handle: &'a DeferredHandle) -> HarnessFuture<'a, ()>;
}

/// The harness writes and committed reads one generation phase needs.
///
/// `*_in` methods run inside the generation's own commit and **must stage into
/// `tx`**; the others are committed reads and must never be called from inside
/// a commit callback (they go through the session line).
pub trait GenerationStore: Send + Sync {
    /// The committed value of one conversation document; a missing document
    /// reads as its definition's initial value.
    fn document<'a>(
        &'a self,
        conversation: ConversationId,
        kind: DocumentKind,
    ) -> HarnessFuture<'a, JsonObject>;

    /// The document's value inside `tx` (read-your-writes), falling back to
    /// the committed value.
    fn document_in(
        &self,
        tx: &Transaction,
        conversation: ConversationId,
        kind: DocumentKind,
    ) -> Result<JsonObject, String>;

    /// Stage one conversation document in `tx` (upstream `tx.doc`).
    fn stage_document(
        &self,
        tx: &mut Transaction,
        conversation: ConversationId,
        kind: DocumentKind,
        value: JsonObject,
    ) -> Result<(), String>;

    /// Append one entry in `tx` and return its committed record (upstream
    /// `tx.appendEntry`).
    fn append_entry<'a>(
        &'a self,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        draft: EntryDraft,
        by_task: TaskId,
    ) -> HarnessFuture<'a, EntryRecord>;

    /// Create a task in `tx` (upstream `tx.createTask`); `owner` names the
    /// owning task, `None` a conversation-owned one.
    #[allow(clippy::too_many_arguments)]
    fn create_task<'a>(
        &'a self,
        catalogue: &'a TaskCatalogue,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        kind: &'a str,
        input: Value,
        owner: Option<TaskId>,
        background: bool,
    ) -> HarnessFuture<'a, TaskId>;

    /// The conversation's newest visible entry (upstream
    /// `tx.scanEntries({conversationId}, 1)`).
    fn latest_entry(&self, conversation: ConversationId) -> HarnessFuture<'_, Option<EntryId>>;

    /// The newest visible head marker's `head` (upstream `tx.latestHeadMarker`).
    fn latest_head(&self, conversation: ConversationId) -> HarnessFuture<'_, Option<EntryId>>;

    /// The derived model context at `at` (the current context when absent).
    fn context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
    ) -> HarnessFuture<'_, ContextView>;

    /// One entry by id (upstream `runtime.entry(kind, id)`).
    fn entry(&self, id: EntryId) -> HarnessFuture<'_, Option<EntryRecord>>;
}

/// `createCompaction` (upstream `harness/compaction.ts`).
///
/// The implementation creates a `pi.compaction` task with the ownership and
/// `background` flag [`compaction::compaction_ownership`] returns, and lists
/// [`compaction::compaction_status_of`] in `pi.live.compactions` — all staged
/// into `tx`.
pub trait CompactionTasks: Send + Sync {
    /// Create one compaction task, staged into `tx`.
    fn create<'a>(
        &'a self,
        catalogue: &'a TaskCatalogue,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        reason: CompactionReason,
        owner: Option<TaskId>,
    ) -> HarnessFuture<'a, TaskId>;
}

/// Post-tools controls a tool result requested (upstream `ToolControl`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolControl {
    /// Every tool of the round asked to end the run.
    pub terminate: bool,
    /// Tools that become available after the round.
    pub add_tools: Vec<String>,
    /// Content of a user message handed to a fresh context (the last one in
    /// call order wins).
    pub handoff: Option<Vec<ContentBlock>>,
}

/// The tool lane's helpers (upstream `harness/tool.ts`).
pub trait ToolTasks: Send + Sync {
    /// The control a completed model-issued call requested, if any (upstream
    /// `outcome.result.kind !== "nested"`).
    fn control(&self, outcome: &TaskOutcome) -> Option<ToolControl>;

    /// Append a harness-error result for a call that never ran (upstream
    /// `appendToolResult` with `harnessError`), staged into `tx`.
    fn append_error<'a>(
        &'a self,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        call: &'a ToolCall,
        reason: &'a str,
        message: &'a str,
        timestamp: i64,
    ) -> HarnessFuture<'a, EntryId>;

    /// Add tools the round loaded (upstream `addTools`), staged into `tx`.
    fn add_tools<'a>(
        &'a self,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        names: &'a [String],
    ) -> HarnessFuture<'a, ()>;
}

/// Extension hooks of the generation task (upstream `GenerationHooks`).
pub trait GenerationHooks: Send + Sync {
    /// Before every request attempt; a returned list replaces the messages for
    /// that request only.
    fn before_request(&self, _messages: Vec<Message>) -> HookFuture<'_, Option<Vec<Message>>> {
        Box::pin(async { None })
    }

    /// Every terminal provider message, before classification.
    fn after_response(&self, _message: &AssistantMessage) -> HookFuture<'_, ()> {
        Box::pin(async {})
    }

    /// A final answer; returned content continues the run with that user
    /// message.
    fn on_yield(&self, _message: &AssistantMessage) -> HookFuture<'_, Option<Vec<ContentBlock>>> {
        Box::pin(async { None })
    }

    /// After every tool of the round is terminal.
    fn after_tools(&self, _assistant: EntryId, _results: Vec<EntryId>) -> HookFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The no-op hook set.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoGenerationHooks;

impl GenerationHooks for NoGenerationHooks {}

/// Everything a `pi.generation` task needs from the host.
///
/// The foreman wires one instance per harness and installs
/// [`generation_task`] in the scheduler catalogue.
pub struct GenerationHarness {
    /// Resolved settings, read at each use (upstream `runtime.settings`).
    pub settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    /// Commit-time writes and committed reads.
    pub store: Arc<dyn GenerationStore>,
    /// Agent resolution from `pi.agent`.
    pub agent: Arc<dyn AgentSource>,
    /// Model lookup and streaming.
    pub models: Arc<dyn GenerationModels>,
    /// Extension hooks.
    pub hooks: Arc<dyn GenerationHooks>,
    /// `createCompaction`; absent disables threshold and overflow compaction.
    pub compaction: Option<Arc<dyn CompactionTasks>>,
    /// The tool lane; absent faults a round that offers any tool.
    pub tools: Option<Arc<dyn ToolTasks>>,
}

impl std::fmt::Debug for GenerationHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenerationHarness")
            .field("has_compaction", &self.compaction.is_some())
            .field("has_tools", &self.tools.is_some())
            .finish_non_exhaustive()
    }
}

// ─── Definition ─────────────────────────────────────────────────────────────

/// Build the executable `pi.generation` definition.
#[must_use]
pub fn generation_task(harness: Arc<GenerationHarness>) -> Task {
    macro_rules! phase {
        ($name:literal, $run:ident) => {{
            let harness = harness.clone();
            phase_handler(move |task, runtime, context| {
                let harness = harness.clone();
                Box::pin(async move { $run(&harness, task, runtime, context).await })
            })
        }};
    }

    let abort_harness = harness.clone();
    define_task(GENERATION_TASK_NAME, GENERATION_VERSION, |_input| {
        initial_checkpoint()
    })
    .phase("prepare", phase!("prepare", prepare))
    .phase("request", phase!("request", request))
    .phase("retry", phase!("retry", retry))
    .phase("poll", phase!("poll", poll))
    .phase("tools", phase!("tools", tools))
    .abort(phase_handler(move |task, runtime, context| {
        let harness = abort_harness.clone();
        Box::pin(async move { abort(&harness, task, runtime, context).await })
    }))
    .build()
    .expect("the pi.generation definition is valid")
}

// ─── prepare ────────────────────────────────────────────────────────────────

async fn prepare(
    harness: &Arc<GenerationHarness>,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: DurableContext,
) {
    let GenerationCheckpoint::Prepare {
        attempt,
        compacted,
        overflow,
        threshold: blocked,
    } = checkpoint_of(task)
    else {
        return;
    };
    let conversation = runtime.conversation_id();
    let settings = (harness.settings)();

    let agent_state = match harness
        .store
        .document(conversation, DocumentKind::Agent)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            return fault(format!(
                "prepare could not read the agent document: {error}"
            ))
        }
    };
    let agent = harness.agent.resolve(conversation, &agent_state);
    let Some(model_ref) = agent.model.clone() else {
        return fail_no_model(harness, runtime, None, context).await;
    };
    let Some(model) = harness
        .models
        .get_model(&model_ref.provider, &model_ref.model_id)
    else {
        return fail_no_model(harness, runtime, Some(model_ref), context).await;
    };

    // The blocking compaction this generation waited for decides whether the
    // request can proceed at all (spec §8.3).
    if let Some(child) = compacted {
        let outcomes = match runtime.outcomes(&[child]).await {
            Ok(outcomes) => outcomes,
            Err(error) => {
                return fault(format!(
                    "prepare could not read the compaction outcome: {error}"
                ))
            }
        };
        let outcome = outcomes.first();
        if let Some(text) = &overflow {
            let completed = matches!(
                outcome,
                Some(TaskOutcome::Completed { result }) if compacted_entry_id(result).is_some()
            );
            if !completed {
                return fail_model_error(harness, runtime, text.clone(), context).await;
            }
        }
        // A threshold compaction that found nothing to compact or was declined
        // leaves a request that may still fit. One that failed left a context
        // it judged too large.
        if blocked == Some(true) {
            if let Some(outcome) = outcome {
                if !matches!(outcome, TaskOutcome::Completed { .. }) {
                    let text = compaction_failure(outcome);
                    return fail_model_error(harness, runtime, text, context).await;
                }
            }
        }
    }

    let view = match harness.store.context(conversation, None).await {
        Ok(view) => view,
        Err(error) => return fault(format!("prepare could not read the context: {error}")),
    };
    let shown = replay_sections(&view.messages);
    let input = PromptInput {
        conversation_id: conversation,
        shown: shown.clone(),
    };
    let desired = render_sections(&agent.sections, &input, &shown);
    let declared: Vec<Tool> = agent.tools.iter().map(tool_declaration).collect();
    let now = runtime.now().unwrap_or(0);
    let entries = plan_system_entries(&view, &desired, &declared, now as u64);

    let threshold = if compacted.is_none() {
        let policy = compaction::compaction_policy(&settings, &model_ref);
        let extra: Vec<Message> = entries
            .iter()
            .flat_map(|entry| entry.model.clone().unwrap_or_default())
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect();
        compaction::threshold_compaction(&view, &extra, model.context_window, &policy)
    } else {
        None
    };

    if threshold == Some(CompactionTrigger::Blocking) {
        let Some(compaction) = harness.compaction.clone() else {
            return fault("the compaction seam is not wired".to_owned());
        };
        let catalogue = runtime.registry();
        let task_id = runtime.task_id();
        let _ = runtime
            .commit(
                move |tx, _current| {
                    Box::pin(async move {
                        let child = compaction
                            .create(
                                &catalogue,
                                tx,
                                conversation,
                                CompactionReason::Threshold,
                                Some(task_id),
                            )
                            .await
                            .map_err(TaskRuntimeError::Invalid)?;
                        let checkpoint = GenerationCheckpoint::Prepare {
                            attempt,
                            compacted: Some(child),
                            overflow: None,
                            threshold: Some(true),
                        };
                        Ok(Some(NextTaskState::Waiting {
                            checkpoint: checkpoint_value(&checkpoint),
                            on: vec![child],
                            policy: JoinPolicy::AllSettled,
                        }))
                    })
                },
                context.clone(),
            )
            .await;
        return;
    }

    let background = threshold == Some(CompactionTrigger::Background);
    // Committed read, hoisted: the request's cutoff is the newest entry of the
    // conversation before this phase's own appends.
    let previously_latest = harness
        .store
        .latest_entry(conversation)
        .await
        .unwrap_or(None);
    let host = harness.clone();
    let catalogue = runtime.registry();
    let stream_options = settings.stream.clone();
    let thinking_level = agent.thinking_level;
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    let task_id = current.id;
                    let mut cutoff = previously_latest;
                    for entry in entries {
                        let record = host
                            .store
                            .append_entry(tx, conversation, entry, task_id)
                            .await
                            .map_err(TaskRuntimeError::Invalid)?;
                        cutoff = Some(record.id);
                    }
                    let Some(cutoff) = cutoff else {
                        return Err(TaskRuntimeError::Invalid(format!(
                            "Conversation {conversation} has no entries to send"
                        )));
                    };
                    // Checked in this commit, so a compaction admitted during
                    // preparation counts.
                    if background {
                        let live = load_live_in(&host, tx, conversation)?;
                        if live.compactions.is_none() {
                            if let Some(compaction) = &host.compaction {
                                compaction
                                    .create(
                                        &catalogue,
                                        tx,
                                        conversation,
                                        CompactionReason::Threshold,
                                        None,
                                    )
                                    .await
                                    .map_err(TaskRuntimeError::Invalid)?;
                            }
                        }
                    }
                    let checkpoint = GenerationCheckpoint::Request {
                        attempt,
                        compacted,
                        model: model_ref,
                        thinking_level,
                        stream_options,
                        cutoff,
                    };
                    Ok(Some(next_running(checkpoint)))
                })
            },
            context,
        )
        .await;
}

// ─── request ────────────────────────────────────────────────────────────────

/// What classification needs from the request that produced a message.
struct Request {
    attempt: i64,
    compacted: Option<TaskId>,
    model: ModelRef,
    cutoff: EntryId,
    /// Committed model context through `cutoff`, when the phase already
    /// derived it.
    messages: Option<Vec<Message>>,
    /// Set when the message came from polling, so a still deferred result polls
    /// strictly later.
    poll_at: Option<i64>,
}

async fn request(
    harness: &Arc<GenerationHarness>,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: DurableContext,
) {
    let GenerationCheckpoint::Request {
        attempt,
        compacted,
        model: reference,
        thinking_level,
        stream_options,
        cutoff,
    } = checkpoint_of(task)
    else {
        return;
    };
    let conversation = runtime.conversation_id();

    // Convert an interrupted partial and open the new attempt.
    {
        let host = harness.clone();
        let partial_fault = Arc::new(Mutex::new(None::<String>));
        let fault_slot = partial_fault.clone();
        let _ = runtime
            .commit(
                move |tx, current| {
                    Box::pin(async move {
                        let mut live = load_live_in(&host, tx, conversation)?;
                        convert_partial_in(
                            &host,
                            tx,
                            conversation,
                            current.id,
                            &mut live,
                            &fault_slot,
                        )
                        .await?;
                        live.generation = Some(GenerationState {
                            attempt,
                            message: None,
                            retry: None,
                            deferred: None,
                        });
                        stage_live_in(&host, tx, conversation, &live)?;
                        Ok(None)
                    })
                },
                context.clone(),
            )
            .await;
        let unreadable = partial_fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(message) = unreadable {
            return fault(message);
        }
    }

    let Some(model) = harness
        .models
        .get_model(&reference.provider, &reference.model_id)
    else {
        return fail_no_model(harness, runtime, Some(reference), context).await;
    };

    let view = match harness.store.context(conversation, Some(cutoff)).await {
        Ok(view) => view,
        Err(error) => return fault(format!("request could not read the context: {error}")),
    };
    let mut messages = view.messages.clone();
    if let Some(replaced) = harness.hooks.before_request(messages.clone()).await {
        messages = replaced;
    }

    let session_id =
        match ensure_provider_session_id(harness, runtime, conversation, context.clone()).await {
            Ok(session_id) => session_id,
            Err(error) => {
                return fault(format!(
                    "the provider session id could not be resolved: {error}"
                ))
            }
        };
    let options = stream_options_for(&stream_options, thinking_level, session_id, runtime);

    let Some(message) = stream_response(
        harness, runtime, &model, messages, options, attempt, &context,
    )
    .await
    else {
        // The invocation was aborted mid-stream; the abort invocation (or the
        // reopened run) handles the committed state.
        return;
    };

    let request = Request {
        attempt,
        compacted,
        model: reference,
        cutoff,
        messages: Some(view.messages),
        poll_at: None,
    };
    classify(harness, runtime, request, message, context).await;
}

/// Build `StreamOptions` from the pinned conversation options.
fn stream_options_for(
    options: &ConversationStreamOptions,
    thinking_level: ThinkingLevel,
    session_id: String,
    runtime: &TaskRuntime,
) -> StreamOptions {
    StreamOptions {
        signal: Some((*runtime.signal()).clone()),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        headers: options.headers.clone(),
        metadata: options.metadata.as_ref().map(|metadata| {
            metadata
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        }),
        cache_retention: options.cache_retention,
        session_id: Some(session_id),
        reasoning: if thinking_level == ThinkingLevel::Off {
            None
        } else {
            Some(thinking_level)
        },
        ..StreamOptions::default()
    }
}

/// `ensureProviderSessionId`: read the provider identity, minting and staging
/// one when the conversation has none.
async fn ensure_provider_session_id(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    conversation: ConversationId,
    context: DurableContext,
) -> Result<String, String> {
    let document = harness
        .store
        .document(conversation, DocumentKind::Provider)
        .await?;
    if let Some(id) = document.get("sessionId").and_then(Value::as_str) {
        return Ok(id.to_owned());
    }
    let minted = uuid::Uuid::new_v4().to_string();
    let staged = minted.clone();
    let host = harness.clone();
    runtime
        .commit(
            move |tx, _current| {
                Box::pin(async move {
                    let mut document = host
                        .store
                        .document_in(tx, conversation, DocumentKind::Provider)
                        .map_err(TaskRuntimeError::Invalid)?;
                    if document.get("sessionId").and_then(Value::as_str).is_some() {
                        // First writer wins; this attempt keeps the value it
                        // read before the commit.
                        return Ok(None);
                    }
                    document.insert("sessionId".to_owned(), Value::String(staged));
                    host.store
                        .stage_document(tx, conversation, DocumentKind::Provider, document)
                        .map_err(TaskRuntimeError::Invalid)?;
                    Ok(None)
                })
            },
            context,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(minted)
}

/// Stream one request and return the terminal message.
///
/// Partials commit at most every `progress.partial_interval_ms`, one flush at a
/// time. The throttle stops before the caller's classification commit, and
/// because every flush is awaited inline there is never a detached commit to
/// wait for.
///
/// `None` means the invocation was aborted mid-stream.
async fn stream_response(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    model: &Model,
    messages: Vec<Message>,
    options: StreamOptions,
    attempt: i64,
    context: &DurableContext,
) -> Option<AssistantMessage> {
    let interval = (harness.settings)().progress.partial_interval_ms.max(1);
    let events = harness.models.stream(
        model,
        Arc::new(ApiContext {
            system_prompt: None,
            messages,
            tools: None,
        }),
        options,
    );
    let mut stream = Box::pin(events.clone().fuse());
    let mut next_flush = tokio::time::Instant::now() + Duration::from_millis(interval);
    // The latest cumulative snapshot, held by `Arc`: provider events each own
    // one, so a flush needs no copy of its own.
    let mut pending: Option<Arc<AssistantMessage>> = None;
    let mut terminal: Option<AssistantMessage> = None;

    loop {
        tokio::select! {
            biased;
            event = stream.next() => {
                match event {
                    None => break,
                    Some(AssistantMessageEvent::Done { message, .. })
                    | Some(AssistantMessageEvent::Error { error: message, .. }) => {
                        terminal = Some(message);
                        break;
                    }
                    Some(event) => {
                        // A partial without content, such as the opening
                        // `start` event, shows nothing; a deferred response
                        // never gets past it, so it never leaves a partial.
                        if let Some(partial) = event_partial(&event) {
                            if !partial.content.is_empty() {
                                pending = Some(Arc::clone(partial));
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(next_flush) => {
                if let Some(partial) = pending.take() {
                    if runtime.signal().is_aborted() {
                        return None;
                    }
                    // A rejected commit (the invocation ended) stops the
                    // throttle; the committed state stays consistent.
                    let _ = commit_partial(harness, runtime, attempt, partial.as_ref(), context)
                        .await;
                }
                next_flush = tokio::time::Instant::now() + Duration::from_millis(interval);
            }
        }
    }

    // Stop the throttle: the last pending partial is dropped (the terminal
    // message supersedes it) and nothing is in flight.
    if let Some(message) = terminal {
        return Some(message);
    }
    match events.result().await {
        Some(message) => Some((*message).clone()),
        None if runtime.signal().is_aborted() => None,
        // A provider that ends its stream without a terminal event breaks the
        // contract; the phase throws, and the scheduler settles `faulted`.
        None => panic!(
            "the provider stream for {}/{} ended without a terminal event",
            model.provider, model.id
        ),
    }
}

/// Commit one throttled partial into `pi.live.generation.message`.
async fn commit_partial(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    attempt: i64,
    partial: &AssistantMessage,
    context: &DurableContext,
) -> Result<(), TaskRuntimeError> {
    let host = harness.clone();
    let conversation = runtime.conversation_id();
    let message = serde_json::to_value(partial).unwrap_or(Value::Null);
    runtime
        .commit(
            move |tx, _current| {
                Box::pin(async move {
                    // A flush only changes `generation.attempt` and
                    // `generation.message`. Reading the draft and writing those
                    // two keys leaves every other field of `pi.live` untouched;
                    // decoding and re-encoding the whole document would copy
                    // the rest of it on every throttled flush.
                    let mut live = host
                        .store
                        .document_in(tx, conversation, DocumentKind::Live)
                        .map_err(TaskRuntimeError::Invalid)?;
                    let generation = live
                        .entry("generation".to_owned())
                        .or_insert_with(|| Value::Object(JsonObject::new()));
                    // A malformed slot reads as an absent generation, the same
                    // fallback `live_from_object` applies.
                    if !generation.is_object() {
                        *generation = Value::Object(JsonObject::new());
                    }
                    if let Some(generation) = generation.as_object_mut() {
                        generation.insert("attempt".to_owned(), Value::from(attempt));
                        generation.insert("message".to_owned(), message);
                    }
                    host.store
                        .stage_document(tx, conversation, DocumentKind::Live, live)
                        .map_err(TaskRuntimeError::Invalid)?;
                    Ok(None)
                })
            },
            context.clone(),
        )
        .await
}

/// The cumulative partial snapshot of one non-terminal event.
fn event_partial(event: &AssistantMessageEvent) -> Option<&Arc<AssistantMessage>> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

// ─── classify ───────────────────────────────────────────────────────────────

/// Classify a terminal provider message in one commit that also clears the
/// partial (upstream `classify`).
async fn classify(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    request: Request,
    message: AssistantMessage,
    context: DurableContext,
) {
    // An abort mark or close: the abort invocation or the reopened run handles
    // the committed state.
    if runtime.signal().is_aborted() {
        return;
    }
    let conversation = runtime.conversation_id();
    let now = runtime.now().unwrap_or(0);

    if message.stop_reason == StopReason::Deferred {
        if let Some(handle) = message.deferred.clone() {
            let poll_at = std::cmp::max(
                now + handle
                    .poll_after_ms
                    .map(|value| value as i64)
                    .unwrap_or(DEFAULT_POLL_AFTER_MS),
                request.poll_at.map(|at| at + 1).unwrap_or(i64::MIN),
            );
            let attempt = request.attempt;
            let checkpoint = GenerationCheckpoint::Poll {
                attempt,
                compacted: request.compacted,
                model: request.model.clone(),
                cutoff: request.cutoff,
                handle,
                poll_at,
            };
            let host = harness.clone();
            let _ = runtime
                .commit(
                    move |tx, _current| {
                        Box::pin(async move {
                            let mut live = load_live_in(&host, tx, conversation)?;
                            live.generation = Some(GenerationState {
                                attempt,
                                message: None,
                                retry: None,
                                deferred: Some(DeferredStatus { poll_at }),
                            });
                            stage_live_in(&host, tx, conversation, &live)?;
                            Ok(Some(NextTaskState::Sleeping {
                                checkpoint: checkpoint_value(&checkpoint),
                                until: poll_at,
                            }))
                        })
                    },
                    context,
                )
                .await;
            return;
        }
    }

    harness.hooks.after_response(&message).await;

    let calls: Vec<ToolCall> = message
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    if message.stop_reason == StopReason::ToolUse && !calls.is_empty() {
        return start_tool_round(harness, runtime, request, message, calls, context).await;
    }
    if matches!(
        message.stop_reason,
        StopReason::Stop | StopReason::Length | StopReason::ToolUse
    ) {
        return answer(harness, runtime, message, context).await;
    }

    // The retry and compaction policies govern the next attempt, so they are
    // read now rather than pinned at preparation.
    let settings = (harness.settings)();
    let Some(model) = harness
        .models
        .get_model(&request.model.provider, &request.model.model_id)
    else {
        return fail_no_model(harness, runtime, Some(request.model), context).await;
    };
    let overflow = message.stop_reason == StopReason::Error
        && is_context_overflow(&message, Some(model.context_window));

    if overflow && request.compacted.is_none() {
        let policy = compaction::compaction_policy(&settings, &request.model);
        if policy.enabled {
            if let Ok(view) = harness
                .store
                .context(conversation, Some(request.cutoff))
                .await
            {
                if compaction::overflow_compaction(&view, &policy) {
                    let text = message
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Context overflow".to_owned());
                    return start_overflow_compaction(
                        harness, runtime, request, message, text, context,
                    )
                    .await;
                }
            }
        }
    }

    let retry_policy = &settings.retry;
    // An overflow is never retried: only a compaction can make the next request
    // fit.
    let retry = message.stop_reason == StopReason::Error
        && !overflow
        && is_retryable_assistant_error(&message)
        && retry_policy.enabled
        && request.attempt <= retry_policy.max_retries as i64;
    let until = if retry {
        now + retry_delay_ms(
            retry_policy.base_delay_ms,
            request.attempt.max(1) as u64,
            Some(retry_policy.max_agent_delay_ms),
        ) as i64
    } else {
        0
    };
    let text = message.error_message.clone().unwrap_or_else(|| {
        format!(
            "Model response ended with stop reason {}",
            message.stop_reason
        )
    });

    let host = harness.clone();
    let catalogue = runtime.registry();
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    // Read inside the commit so the boundary is taken from the
                    // revision this commit is staged against (upstream
                    // `tx.latestHeadMarker`), never a stale pre-commit cut.
                    // The session line attaches the backend to every commit, so
                    // this read is authoritative and no pre-read fallback is
                    // kept: a head committed between the phase and this commit
                    // must not be missed.
                    let head = tx.latest_head(conversation).await?;
                    let task_id = current.id;
                    append_assistant_in(&host, tx, conversation, &message, task_id).await?;
                    if retry {
                        let mut live = load_live_in(&host, tx, conversation)?;
                        live.generation = Some(GenerationState {
                            attempt: request.attempt,
                            message: None,
                            retry: Some(RetryStatus {
                                at: until,
                                error: message.error_message.clone().unwrap_or_default(),
                            }),
                            deferred: None,
                        });
                        stage_live_in(&host, tx, conversation, &live)?;
                        let checkpoint = GenerationCheckpoint::Retry {
                            attempt: request.attempt,
                            compacted: request.compacted,
                            until,
                        };
                        return Ok(Some(NextTaskState::Sleeping {
                            checkpoint: checkpoint_value(&checkpoint),
                            until,
                        }));
                    }
                    end_failed_run_in(
                        &host,
                        &catalogue,
                        tx,
                        conversation,
                        task_id,
                        &RunSettlement::Unanswered {
                            reason: "model_error".to_owned(),
                            detail: Some(Value::String(text.clone())),
                        },
                        settings.after_failed_run,
                        head,
                        None,
                    )
                    .await?;
                    Ok(Some(NextTaskState::Terminal {
                        outcome: TaskOutcome::Failed {
                            error: TaskOutcomeError {
                                message: text,
                                detail: Some(json!({ "reason": "model_error" })),
                            },
                            result: None,
                        },
                    }))
                })
            },
            context,
        )
        .await;
}

/// Append the overflowing answer and start a blocking overflow compaction.
async fn start_overflow_compaction(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    request: Request,
    message: AssistantMessage,
    text: String,
    context: DurableContext,
) {
    let conversation = runtime.conversation_id();
    let catalogue = runtime.registry();
    let host = harness.clone();
    let attempt = request.attempt;
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    append_assistant_in(&host, tx, conversation, &message, current.id).await?;
                    let mut live = load_live_in(&host, tx, conversation)?;
                    live.generation = None;
                    stage_live_in(&host, tx, conversation, &live)?;
                    let child = match &host.compaction {
                        Some(compaction) => compaction
                            .create(
                                &catalogue,
                                tx,
                                conversation,
                                CompactionReason::Overflow,
                                Some(current.id),
                            )
                            .await
                            .map_err(TaskRuntimeError::Invalid)?,
                        None => {
                            return Err(TaskRuntimeError::Invalid(
                                "the compaction seam is not wired".to_owned(),
                            ))
                        }
                    };
                    let checkpoint = GenerationCheckpoint::Prepare {
                        attempt,
                        compacted: Some(child),
                        overflow: Some(text),
                        threshold: None,
                    };
                    Ok(Some(NextTaskState::Waiting {
                        checkpoint: checkpoint_value(&checkpoint),
                        on: vec![child],
                        policy: JoinPolicy::AllSettled,
                    }))
                })
            },
            context,
        )
        .await;
}

// ─── retry ──────────────────────────────────────────────────────────────────

async fn retry(
    harness: &Arc<GenerationHarness>,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: DurableContext,
) {
    let GenerationCheckpoint::Retry {
        attempt,
        compacted,
        until,
    } = checkpoint_of(task)
    else {
        return;
    };
    if until > runtime.now().unwrap_or(0) {
        let checkpoint = checkpoint_value(&GenerationCheckpoint::Retry {
            attempt,
            compacted,
            until,
        });
        let _ = runtime.sleep_until(until, checkpoint, context).await;
        return;
    }
    let host = harness.clone();
    let conversation = runtime.conversation_id();
    let _ = runtime
        .commit(
            move |tx, _current| {
                Box::pin(async move {
                    let mut live = load_live_in(&host, tx, conversation)?;
                    live.generation = Some(GenerationState {
                        attempt: attempt + 1,
                        message: None,
                        retry: None,
                        deferred: None,
                    });
                    stage_live_in(&host, tx, conversation, &live)?;
                    Ok(Some(next_running(GenerationCheckpoint::Prepare {
                        attempt: attempt + 1,
                        compacted,
                        overflow: None,
                        threshold: None,
                    })))
                })
            },
            context,
        )
        .await;
}

// ─── poll ───────────────────────────────────────────────────────────────────

async fn poll(
    harness: &Arc<GenerationHarness>,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: DurableContext,
) {
    let GenerationCheckpoint::Poll {
        attempt,
        compacted,
        model: reference,
        cutoff,
        handle,
        poll_at,
    } = checkpoint_of(task)
    else {
        return;
    };
    if poll_at > runtime.now().unwrap_or(0) {
        let checkpoint = checkpoint_value(&GenerationCheckpoint::Poll {
            attempt,
            compacted,
            model: reference,
            cutoff,
            handle,
            poll_at,
        });
        let _ = runtime.sleep_until(poll_at, checkpoint, context).await;
        return;
    }
    let Some(model) = harness
        .models
        .get_model(&reference.provider, &reference.model_id)
    else {
        return fail_no_model(harness, runtime, Some(reference), context).await;
    };
    let Some(deferred) = harness.models.deferred() else {
        // Open seam: the provider registry has no deferred fetch, so the run
        // cannot be completed. Recorded as `model_error` with the reason.
        return fail_model_error(
            harness,
            runtime,
            format!(
                "Deferred responses are not supported by this host: {}/{} returned a deferred handle",
                reference.provider, reference.model_id
            ),
            context,
        )
        .await;
    };
    let message = match deferred.fetch(&model, &handle).await {
        Ok(message) => message,
        Err(error) => return fail_model_error(harness, runtime, error, context).await,
    };
    let request = Request {
        attempt,
        compacted,
        model: reference,
        cutoff,
        messages: None,
        poll_at: Some(poll_at),
    };
    classify(harness, runtime, request, message, context).await;
}

// ─── answer ─────────────────────────────────────────────────────────────────

/// A final answer; the final boundary places queued items (spec §6).
async fn answer(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    message: AssistantMessage,
    context: DurableContext,
) {
    let conversation = runtime.conversation_id();
    let now = runtime.now().unwrap_or(0);
    let settings = (harness.settings)();
    let continuation = harness.hooks.on_yield(&message).await;

    let host = harness.clone();
    let catalogue = runtime.registry();
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    // The boundary's active-range start is read inside the
                    // commit (upstream `tx.latestHeadMarker`), so a head placed
                    // by a commit that landed after this phase's read cannot be
                    // missed.
                    let head = tx.latest_head(conversation).await?;
                    let task_id = current.id;
                    let entry =
                        append_assistant_in(&host, tx, conversation, &message, task_id).await?;
                    let boundary = apply_boundary(
                        &host,
                        tx,
                        conversation,
                        &settings,
                        Boundary::Final,
                        head,
                        now,
                        task_id,
                    )
                    .await?;
                    let outcome = TaskOutcome::Completed {
                        result: json!({ "entryId": entry.id }),
                    };
                    if let Some(content) = continuation {
                        if boundary.users.is_empty() && !boundary.reset {
                            let user = input_draft(
                                Value::Array(
                                    content
                                        .into_iter()
                                        .map(|block| {
                                            serde_json::to_value(block).unwrap_or(Value::Null)
                                        })
                                        .collect(),
                                ),
                                None,
                                now,
                            );
                            host.store
                                .append_entry(tx, conversation, user, task_id)
                                .await
                                .map_err(TaskRuntimeError::Invalid)?;
                            let next = host
                                .store
                                .create_task(
                                    &catalogue,
                                    tx,
                                    conversation,
                                    GENERATION_TASK_NAME,
                                    json!({}),
                                    None,
                                    false,
                                )
                                .await
                                .map_err(TaskRuntimeError::Invalid)?;
                            let mut live = load_live_in(&host, tx, conversation)?;
                            if let Some(run) = live.run.as_mut() {
                                if run.task_id == task_id {
                                    run.task_id = next;
                                }
                            }
                            live.generation = None;
                            stage_live_in(&host, tx, conversation, &live)?;
                            return Ok(Some(NextTaskState::Terminal { outcome }));
                        }
                    }
                    end_run_in(
                        &host,
                        tx,
                        conversation,
                        task_id,
                        &RunSettlement::Done { answer: entry.id },
                    )
                    .await?;
                    if !boundary.users.is_empty() {
                        start_run_in(&host, &catalogue, tx, conversation, &boundary.users).await?;
                    }
                    Ok(Some(NextTaskState::Terminal { outcome }))
                })
            },
            context,
        )
        .await;
}

// ─── tools ──────────────────────────────────────────────────────────────────

/// Append the tool-calling answer and start its tool round (spec §8.3).
async fn start_tool_round(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    mut request: Request,
    message: AssistantMessage,
    calls: Vec<ToolCall>,
    context: DurableContext,
) {
    let conversation = runtime.conversation_id();
    let now = runtime.now().unwrap_or(0);
    let settings = (harness.settings)();
    // `Request` owns the context it streamed and nothing reads it afterwards,
    // so take it rather than copy the message list.
    let messages = match request.messages.take() {
        Some(messages) => messages,
        None => match harness
            .store
            .context(conversation, Some(request.cutoff))
            .await
        {
            Ok(view) => view.messages,
            Err(error) => {
                return fault(format!(
                    "the tool round could not read the context: {error}"
                ))
            }
        },
    };
    let offered: HashSet<String> = get_current_tools(&messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let agent_state = harness
        .store
        .document(conversation, DocumentKind::Agent)
        .await
        .unwrap_or_default();
    let agent = harness.agent.resolve(conversation, &agent_state);
    let sequential = settings.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            offered.contains(&call.name)
                && agent.tool(&call.name).and_then(|tool| tool.execution_mode)
                    == Some(ToolExecutionMode::Sequential)
        });
    let Some(tools) = harness.tools.clone() else {
        return fault("the tool seam is not wired; a tool round cannot start".to_owned());
    };
    let catalogue = runtime.registry();
    let host = harness.clone();
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    let task_id = current.id;
                    let entry =
                        append_assistant_in(&host, tx, conversation, &message, task_id).await?;
                    let mut slots: Vec<ToolSlot> = Vec::new();
                    let mut round: Vec<TaskId> = Vec::new();
                    let mut pending: Vec<String> = Vec::new();
                    for call in &calls {
                        let mut progress = SlotProgress::new(call.id.clone(), call.name.clone());
                        if !offered.contains(&call.name) {
                            let result = tools
                                .append_error(
                                    tx,
                                    conversation,
                                    call,
                                    "tool_unavailable",
                                    &format!("Tool {} is not available", call.name),
                                    now,
                                )
                                .await
                                .map_err(TaskRuntimeError::Invalid)?;
                            progress.status = SlotStatus::Done;
                            slots.push(ToolSlot {
                                progress,
                                task_id: None,
                                entry: Some(result),
                            });
                            continue;
                        }
                        if sequential && !round.is_empty() {
                            pending.push(call.id.clone());
                            slots.push(ToolSlot {
                                progress,
                                task_id: None,
                                entry: None,
                            });
                            continue;
                        }
                        let child = host
                            .store
                            .create_task(
                                &catalogue,
                                tx,
                                conversation,
                                TOOL_TASK_NAME,
                                json!({
                                    "kind": "model",
                                    "assistant": entry.id.as_i64(),
                                    "callId": call.id,
                                }),
                                Some(task_id),
                                false,
                            )
                            .await
                            .map_err(TaskRuntimeError::Invalid)?;
                        round.push(child);
                        slots.push(ToolSlot {
                            progress,
                            task_id: Some(child),
                            entry: None,
                        });
                    }
                    let mut live = load_live_in(&host, tx, conversation)?;
                    live.generation = None;
                    live.tools = Some(slots);
                    stage_live_in(&host, tx, conversation, &live)?;
                    let checkpoint = GenerationCheckpoint::Tools {
                        assistant: entry.id,
                        tools: round.clone(),
                        pending,
                    };
                    Ok(Some(NextTaskState::Waiting {
                        checkpoint: checkpoint_value(&checkpoint),
                        on: round,
                        policy: JoinPolicy::AllSettled,
                    }))
                })
            },
            context,
        )
        .await;
}

async fn tools(
    harness: &Arc<GenerationHarness>,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: DurableContext,
) {
    let GenerationCheckpoint::Tools {
        assistant,
        tools: round,
        pending,
    } = checkpoint_of(task)
    else {
        return;
    };
    let Some((next, rest)) = pending.split_first() else {
        return finish_tool_round(harness, runtime, assistant, round, context).await;
    };
    let next = next.clone();
    let rest = rest.to_vec();
    let conversation = runtime.conversation_id();
    let catalogue = runtime.registry();
    let host = harness.clone();
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    let task_id = current.id;
                    let child = host
                        .store
                        .create_task(
                            &catalogue,
                            tx,
                            conversation,
                            TOOL_TASK_NAME,
                            json!({
                                "kind": "model",
                                "assistant": assistant.as_i64(),
                                "callId": next,
                            }),
                            Some(task_id),
                            false,
                        )
                        .await
                        .map_err(TaskRuntimeError::Invalid)?;
                    let mut live = load_live_in(&host, tx, conversation)?;
                    if let Some(slots) = live.tools.as_mut() {
                        if let Some(slot) = slots
                            .iter_mut()
                            .find(|slot| slot.progress.call_id == next && slot.task_id.is_none())
                        {
                            slot.task_id = Some(child);
                        }
                    }
                    stage_live_in(&host, tx, conversation, &live)?;
                    let mut round = round;
                    round.push(child);
                    let checkpoint = GenerationCheckpoint::Tools {
                        assistant,
                        tools: round,
                        pending: rest,
                    };
                    Ok(Some(NextTaskState::Waiting {
                        checkpoint: checkpoint_value(&checkpoint),
                        on: vec![child],
                        policy: JoinPolicy::AllSettled,
                    }))
                })
            },
            context,
        )
        .await;
}

/// The round's tools are terminal: apply their controls and either end the run
/// at the final boundary or hand it to the next generation (spec §8.5).
async fn finish_tool_round(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    assistant: EntryId,
    round: Vec<TaskId>,
    context: DurableContext,
) {
    let conversation = runtime.conversation_id();
    let now = runtime.now().unwrap_or(0);
    let settings = (harness.settings)();

    let outcomes = match runtime.outcomes(&round).await {
        Ok(outcomes) => outcomes,
        Err(error) => {
            return fault(format!(
                "finish_tool_round could not read the tool outcomes: {error}"
            ))
        }
    };
    let controls: Vec<Option<ToolControl>> = outcomes
        .iter()
        .map(|outcome| {
            harness
                .tools
                .as_ref()
                .and_then(|tools| tools.control(outcome))
        })
        .collect();

    let slots = harness
        .store
        .document(conversation, DocumentKind::Live)
        .await
        .map(|document| live_from_object(&document).tools.unwrap_or_default())
        .unwrap_or_default();
    let results: Vec<EntryId> = slots.iter().filter_map(|slot| slot.entry).collect();
    harness.hooks.after_tools(assistant, results).await;

    let control_of = |task: TaskId| {
        round
            .iter()
            .position(|candidate| *candidate == task)
            .and_then(|index| controls.get(index).cloned().flatten())
    };
    // Every call of the round, including those answered without a task, must
    // ask to terminate.
    let terminate = !slots.is_empty()
        && slots.iter().all(|slot| {
            slot.task_id
                .and_then(control_of)
                .map(|control| control.terminate)
                .unwrap_or(false)
        });
    let added: Vec<String> = controls
        .iter()
        .flatten()
        .flat_map(|control| control.add_tools.clone())
        .collect();
    // The last handoff in call order wins.
    let handoff = controls
        .iter()
        .flatten()
        .filter_map(|control| control.handoff.clone())
        .next_back();
    let ending = terminate || handoff.is_some();

    let host = harness.clone();
    let catalogue = runtime.registry();
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    // Read the boundary's active-range start inside the commit
                    // (upstream `tx.latestHeadMarker`) so it matches the
                    // revision this commit is staged against.
                    let head = tx.latest_head(conversation).await?;
                    let task_id = current.id;
                    let head = match handoff {
                        Some(content) => {
                            let draft = EntryDraft {
                                kind: "pi.reset".to_owned(),
                                model: Some(vec![json!({
                                    "role": "user",
                                    "content": content,
                                    "timestamp": now,
                                })]),
                                head: Some(HeadTarget::SelfEntry),
                                ..EntryDraft::default()
                            };
                            let reset = host
                                .store
                                .append_entry(tx, conversation, draft, task_id)
                                .await
                                .map_err(TaskRuntimeError::Invalid)?;
                            Some(reset.id)
                        }
                        None => head,
                    };
                    if !added.is_empty() {
                        if let Some(tools) = &host.tools {
                            tools
                                .add_tools(tx, conversation, &added)
                                .await
                                .map_err(TaskRuntimeError::Invalid)?;
                        }
                    }
                    let at = if ending {
                        Boundary::Final
                    } else {
                        Boundary::PostTools
                    };
                    let boundary =
                        apply_boundary(&host, tx, conversation, &settings, at, head, now, task_id)
                            .await?;
                    let outcome = TaskOutcome::Completed {
                        result: json!({ "entryId": assistant }),
                    };
                    if ending {
                        end_run_in(
                            &host,
                            tx,
                            conversation,
                            task_id,
                            &RunSettlement::Done { answer: assistant },
                        )
                        .await?;
                        if !boundary.users.is_empty() {
                            start_run_in(&host, &catalogue, tx, conversation, &boundary.users)
                                .await?;
                        }
                    } else if boundary.reset {
                        // The queued reset cut the run's context before an
                        // answer.
                        end_run_in(
                            &host,
                            tx,
                            conversation,
                            task_id,
                            &RunSettlement::Unanswered {
                                reason: REASON_RESET.to_owned(),
                                detail: None,
                            },
                        )
                        .await?;
                        if !boundary.users.is_empty() {
                            start_run_in(&host, &catalogue, tx, conversation, &boundary.users)
                                .await?;
                        }
                    } else {
                        let next = host
                            .store
                            .create_task(
                                &catalogue,
                                tx,
                                conversation,
                                GENERATION_TASK_NAME,
                                json!({}),
                                None,
                                false,
                            )
                            .await
                            .map_err(TaskRuntimeError::Invalid)?;
                        let mut live = load_live_in(&host, tx, conversation)?;
                        live.tools = None;
                        live.nested_tools = None;
                        if let Some(run) = live.run.as_mut() {
                            if run.task_id == task_id {
                                run.inputs.extend(boundary.users.iter().copied());
                                run.task_id = next;
                            }
                        }
                        stage_live_in(&host, tx, conversation, &live)?;
                    }
                    Ok(Some(NextTaskState::Terminal { outcome }))
                })
            },
            context,
        )
        .await;
}

// ─── abort ──────────────────────────────────────────────────────────────────

async fn abort(
    harness: &Arc<GenerationHarness>,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: DurableContext,
) {
    let checkpoint = checkpoint_of(task);
    let conversation = runtime.conversation_id();

    if let GenerationCheckpoint::Poll {
        model: reference,
        handle,
        ..
    } = &checkpoint
    {
        if let (Some(model), Some(deferred)) = (
            harness
                .models
                .get_model(&reference.provider, &reference.model_id),
            harness.models.deferred(),
        ) {
            if let Err(error) = deferred.cancel(&model, handle).await {
                let _ = runtime.report(format!("deferred cancel failed: {error}"));
            }
        }
    }

    // Runs after the round's tool tasks are terminal; calls never started get
    // `aborted` results (spec §8.5).
    let unstarted = match &checkpoint {
        GenerationCheckpoint::Tools {
            assistant, pending, ..
        } if !pending.is_empty() => read_calls(harness, *assistant, pending).await,
        _ => Vec::new(),
    };

    let now = runtime.now().unwrap_or(0);
    let host = harness.clone();
    let partial_fault = Arc::new(Mutex::new(None::<String>));
    let fault_slot = partial_fault.clone();
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    let task_id = current.id;
                    let mut live = load_live_in(&host, tx, conversation)?;
                    convert_partial_in(&host, tx, conversation, task_id, &mut live, &fault_slot)
                        .await?;
                    if let Some(tools) = &host.tools {
                        for call in &unstarted {
                            tools
                                .append_error(
                                    tx,
                                    conversation,
                                    call,
                                    "aborted",
                                    &format!("Tool {} was aborted", call.name),
                                    now,
                                )
                                .await
                                .map_err(TaskRuntimeError::Invalid)?;
                        }
                    }
                    end_run_in(
                        &host,
                        tx,
                        conversation,
                        task_id,
                        &RunSettlement::Unanswered {
                            reason: "aborted".to_owned(),
                            detail: None,
                        },
                    )
                    .await?;
                    Ok(Some(NextTaskState::Terminal {
                        outcome: TaskOutcome::Aborted {
                            reason: Some("aborted".to_owned()),
                            result: None,
                        },
                    }))
                })
            },
            context,
        )
        .await;
    let unreadable = partial_fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(message) = unreadable {
        fault(message);
    }
}

/// The calls `call_ids` of the assistant entry, in the given order.
async fn read_calls(
    harness: &Arc<GenerationHarness>,
    assistant: EntryId,
    call_ids: &[String],
) -> Vec<ToolCall> {
    let Ok(Some(record)) = harness.store.entry(assistant).await else {
        return Vec::new();
    };
    let Some(message) = record
        .model
        .as_deref()
        .and_then(|model| model.first())
        .and_then(|value| serde_json::from_value::<AssistantMessage>(value.clone()).ok())
    else {
        return Vec::new();
    };
    let calls: Vec<ToolCall> = message
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    call_ids
        .iter()
        .filter_map(|id| calls.iter().find(|call| &call.id == id).cloned())
        .collect()
}

// ─── failure ends ───────────────────────────────────────────────────────────

/// Settle the run's inputs `unanswered` with `no_model` and fail.
async fn fail_no_model(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    reference: Option<ModelRef>,
    context: DurableContext,
) {
    let message = match reference {
        None => "No model is configured".to_owned(),
        Some(reference) => format!(
            "Model {}/{} is not available",
            reference.provider, reference.model_id
        ),
    };
    fail_run(
        harness,
        runtime,
        "no_model",
        Value::String(message.clone()),
        message,
        context,
    )
    .await;
}

/// Settle the run's inputs `unanswered` with `model_error` and fail.
async fn fail_model_error(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    text: String,
    context: DurableContext,
) {
    fail_run(
        harness,
        runtime,
        "model_error",
        Value::String(text.clone()),
        text,
        context,
    )
    .await;
}

/// End a failed run and settle its inputs (upstream `failModelError`).
async fn fail_run(
    harness: &Arc<GenerationHarness>,
    runtime: &TaskRuntime,
    reason: &'static str,
    detail: Value,
    message: String,
    context: DurableContext,
) {
    let conversation = runtime.conversation_id();
    let settings = (harness.settings)();
    let host = harness.clone();
    let catalogue = runtime.registry();
    let settlement = RunSettlement::Unanswered {
        reason: reason.to_owned(),
        detail: Some(detail),
    };
    let _ = runtime
        .commit(
            move |tx, current| {
                Box::pin(async move {
                    // The final boundary selects queued inputs against the
                    // active-range start, so read it from the revision this
                    // commit is staged against (upstream
                    // `tx.latestHeadMarker`); a head committed between the
                    // failed phase and this commit must not be missed.
                    let head = tx.latest_head(conversation).await?;
                    end_failed_run_in(
                        &host,
                        &catalogue,
                        tx,
                        conversation,
                        current.id,
                        &settlement,
                        settings.after_failed_run,
                        head,
                        None,
                    )
                    .await?;
                    Ok(Some(NextTaskState::Terminal {
                        outcome: TaskOutcome::Failed {
                            error: TaskOutcomeError {
                                message,
                                detail: Some(json!({ "reason": reason })),
                            },
                            result: None,
                        },
                    }))
                })
            },
            context,
        )
        .await;
}

/// End a failed run, settling its inputs, and do with the inputs queued behind
/// it what `after_failed_run` says (spec §6).
#[allow(clippy::too_many_arguments)]
async fn end_failed_run_in(
    harness: &Arc<GenerationHarness>,
    catalogue: &TaskCatalogue,
    tx: &mut Transaction,
    conversation: ConversationId,
    task_id: TaskId,
    settlement: &RunSettlement,
    policy: AfterFailedRun,
    head: Option<EntryId>,
    boundary_head: Option<EntryId>,
) -> Result<(), TaskRuntimeError> {
    end_run_in(harness, tx, conversation, task_id, settlement).await?;
    if policy == AfterFailedRun::Withdraw {
        let mut inbox = load_inbox_in(harness, tx, conversation)?;
        for id in inbox::withdraw_inputs(&mut inbox) {
            tx.stage_submission_change(
                id,
                SubmissionChange::Unanswered {
                    reason: crate::durable::harness::submissions::REASON_RUN_FAILED.to_owned(),
                    detail: None,
                },
            );
        }
        stage_inbox_in(harness, tx, conversation, &inbox)?;
    }
    if policy == AfterFailedRun::Continue {
        let settings = (harness.settings)();
        let boundary = apply_boundary(
            harness,
            tx,
            conversation,
            &settings,
            Boundary::Final,
            boundary_head.or(head),
            harness_epoch_ms(),
            task_id,
        )
        .await?;
        if !boundary.users.is_empty() {
            start_run_in(harness, catalogue, tx, conversation, &boundary.users).await?;
        }
    }
    Ok(())
}

/// Epoch milliseconds for a boundary placed inside a commit.
///
/// The runtime clock is not reachable from inside a transaction; boundary
/// placement stamps entry timestamps, for which the process clock is
/// equivalent.
fn harness_epoch_ms() -> i64 {
    crate::ai::util::now_ms() as i64
}

/// How a run's inputs settle.
#[derive(Clone, Debug, PartialEq)]
enum RunSettlement {
    /// The run produced an answer entry.
    Done { answer: EntryId },
    /// A failure or cancellation; the reason and optional detail.
    Unanswered {
        reason: String,
        detail: Option<Value>,
    },
}

impl RunSettlement {
    fn change(&self) -> SubmissionChange {
        match self {
            Self::Done { answer } => SubmissionChange::Done { answer: *answer },
            Self::Unanswered { reason, detail } => SubmissionChange::Unanswered {
                reason: reason.clone(),
                detail: detail.clone(),
            },
        }
    }
}

/// `endRun`: settle the run's inputs and clear the generation presentation.
async fn end_run_in(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    task_id: TaskId,
    settlement: &RunSettlement,
) -> Result<(), TaskRuntimeError> {
    let mut live = load_live_in(harness, tx, conversation)?;
    let settles = live.run.as_ref().is_some_and(|run| run.task_id == task_id);
    let run = if settles { live.run.take() } else { None };
    if let Some(run) = run {
        for id in run.inputs {
            tx.stage_submission_change(id, settlement.change());
        }
    }
    live.generation = None;
    live.tools = None;
    live.nested_tools = None;
    stage_live_in(harness, tx, conversation, &live)
}

// ─── boundaries ─────────────────────────────────────────────────────────────

/// What a boundary placed.
struct BoundaryResult {
    users: Vec<SubmissionId>,
    reset: bool,
}

/// Place the queued items a boundary selects (upstream `applyBoundary`).
///
/// Reads `pi.inbox` through the transaction (read-your-writes), places every
/// selected write and user item, settles their submissions and stages the
/// inbox without the selected items. `head` is the active range start; a write
/// targeting an entry before it is stale.
#[allow(clippy::too_many_arguments)]
async fn apply_boundary(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    settings: &Settings,
    at: Boundary,
    head: Option<EntryId>,
    now: i64,
    by_task: TaskId,
) -> Result<BoundaryResult, TaskRuntimeError> {
    let mut inbox = load_inbox_in(harness, tx, conversation)?;
    let selected = inbox::boundary(&inbox, at, settings);
    let reset = inbox::has_queued_reset(&inbox);
    let mut active_range_start = head;
    let mut users: Vec<SubmissionId> = Vec::new();

    for item in &selected {
        let Some(queued) = inbox.items.get(item.index).cloned() else {
            continue;
        };
        match item.kind {
            SelectedKind::Write => {
                let Some(entry) = queued.write_entry().cloned() else {
                    continue;
                };
                if active_range_start.is_some_and(|start| inbox::is_stale_entry(&entry, start)) {
                    tx.stage_submission_change(
                        item.id,
                        SubmissionChange::Unanswered {
                            reason: inbox::REASON_STALE.to_owned(),
                            detail: None,
                        },
                    );
                    continue;
                }
                let draft = entry_draft_from_object(&entry)?;
                let placed_head = draft.head;
                let record = harness
                    .store
                    .append_entry(tx, conversation, draft, by_task)
                    .await
                    .map_err(TaskRuntimeError::Invalid)?;
                match placed_head {
                    Some(HeadTarget::SelfEntry) => active_range_start = Some(record.id),
                    Some(HeadTarget::Entry(id)) => active_range_start = Some(id),
                    None => {}
                }
                tx.stage_submission_change(item.id, SubmissionChange::Placed { entry: record.id });
            }
            SelectedKind::User => {
                let (content, entry) = match &queued {
                    InboxItem::Input { content, entry, .. } => (content.clone(), entry.clone()),
                    InboxItem::Write { .. } => continue,
                };
                let draft = input_draft(content, entry, now);
                let record = harness
                    .store
                    .append_entry(tx, conversation, draft, by_task)
                    .await
                    .map_err(TaskRuntimeError::Invalid)?;
                tx.stage_submission_change(item.id, SubmissionChange::Placed { entry: record.id });
                users.push(item.id);
            }
        }
    }

    inbox::remove_selected(&mut inbox, &selected);
    stage_inbox_in(harness, tx, conversation, &inbox)?;
    Ok(BoundaryResult { users, reset })
}

/// The entry draft of one queued write (`EntryDraft` has no serde derive).
pub(crate) fn entry_draft_from_object(entry: &JsonObject) -> Result<EntryDraft, TaskRuntimeError> {
    let invalid = TaskRuntimeError::Invalid;
    let kind = entry
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("an inbox write has no entry kind".to_owned()))?
        .to_owned();
    let model = match entry.get("model") {
        Some(Value::Array(messages)) => Some(messages.clone()),
        Some(_) => {
            return Err(invalid(
                "an inbox write's model must be an array".to_owned(),
            ))
        }
        None => None,
    };
    let data = entry.get("data").cloned();
    let head = match entry.get("head") {
        Some(Value::String(self_entry)) if self_entry == "self" => Some(HeadTarget::SelfEntry),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(|raw| HeadTarget::Entry(EntryId::from_raw(raw))),
        Some(_) => {
            return Err(invalid(
                "an inbox write's head must be \"self\" or an entry id".to_owned(),
            ))
        }
        None => None,
    };
    let edits = match entry.get("edits") {
        Some(value) => Some(
            serde_json::from_value::<Vec<ContextEdit>>(value.clone())
                .map_err(|error| invalid(format!("an inbox write's edits are invalid: {error}")))?,
        ),
        None => None,
    };
    Ok(EntryDraft {
        kind,
        model,
        data,
        head,
        edits,
    })
}

/// The draft an input is placed as: `entry`'s kind and data, or `pi.user`,
/// carrying the user message (upstream `appendInput`).
pub(crate) fn input_draft(content: Value, entry: Option<InputEntry>, timestamp: i64) -> EntryDraft {
    let blocks: Vec<ContentBlock> =
        match serde_json::from_value::<Vec<ContentBlock>>(content.clone()) {
            Ok(blocks) => blocks,
            // A host that submitted a bare string is one text block.
            Err(_) => vec![ContentBlock::Text(TextContent {
                text: content.as_str().unwrap_or_default().to_owned(),
                text_signature: None,
            })],
        };
    let message = Message::User(UserMessage {
        role: UserRole::User,
        content: blocks,
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: timestamp as u64,
    });
    let model = Some(vec![
        serde_json::to_value(message).expect("a user message is JSON-serializable")
    ]);
    match entry {
        None => EntryDraft {
            kind: "pi.user".to_owned(),
            model,
            ..EntryDraft::default()
        },
        Some(entry) => EntryDraft {
            kind: entry.kind,
            data: entry.data,
            model,
            ..EntryDraft::default()
        },
    }
}

/// Start the next run with `inputs` (upstream `startRun`).
pub(crate) async fn start_run_in(
    harness: &Arc<GenerationHarness>,
    catalogue: &TaskCatalogue,
    tx: &mut Transaction,
    conversation: ConversationId,
    inputs: &[SubmissionId],
) -> Result<(), TaskRuntimeError> {
    let next = harness
        .store
        .create_task(
            catalogue,
            tx,
            conversation,
            GENERATION_TASK_NAME,
            json!({}),
            None,
            false,
        )
        .await
        .map_err(TaskRuntimeError::Invalid)?;
    let mut live = load_live_in(harness, tx, conversation)?;
    assign_run(&mut live, next, inputs);
    stage_live_in(harness, tx, conversation, &live)
}

/// Point `pi.live` at a new generation task (upstream `startRun`'s tail).
pub(crate) fn assign_run(live: &mut LiveState, task_id: TaskId, inputs: &[SubmissionId]) {
    live.run = Some(RunState {
        task_id,
        inputs: inputs.to_vec(),
    });
}

/// Start a conversation-owned generation without a [`GenerationHarness`].
///
/// `Harness::submit` runs admission inside a session commit and has no
/// catalogue or store handle, so the task is created through the session's own
/// `stage_create_task` and `pi.live.run` is staged with the same document
/// pattern the session uses. Returns the new task id.
pub(crate) async fn start_run_in_state(
    state: &SessionState<'_>,
    tx: &mut Transaction,
    conversation: ConversationId,
    inputs: &[SubmissionId],
) -> Result<TaskId, StorageError> {
    let context = tx.context().clone();
    // The committed read precedes the task staging, which writes.
    let (live_value, live_committed) =
        read_conversation_document_state(state, tx, &context, &LIVE_DEFINITION, conversation)
            .await?;
    let snapshot = crate::durable::session::facade::TaskSnapshot {
        kind: GENERATION_TASK_NAME.to_owned(),
        version: GENERATION_VERSION,
        checkpoint: initial_checkpoint(),
    };
    let options = crate::durable::session::transaction::TaskOptions {
        conversation_id: Some(conversation),
        ownership: crate::durable::session::transaction::TaskOwnership::Conversation,
        background: false,
        abandon_on_restart: false,
        submission_ids: inputs.to_vec(),
        trace: None,
    };
    let task =
        crate::durable::session::facade::stage_create_task(tx, state, snapshot, json!({}), options)
            .await?;
    let mut live = live_from_object(&live_value);
    assign_run(&mut live, task, inputs);
    stage_conversation_document_state(
        tx,
        conversation,
        &LIVE_DEFINITION,
        live_to_object(&live),
        live_committed,
    );
    Ok(task)
}

/// Read one conversation document's current value, materialized at its
/// definition's version.
///
/// A draft this transaction already staged wins (read-your-writes); the
/// committed record is returned only for staging an update.
pub(crate) async fn read_conversation_document_state(
    state: &SessionState<'_>,
    tx: &Transaction,
    context: &DurableContext,
    definition: &AnyDocDefinition,
    conversation: ConversationId,
) -> Result<(JsonObject, Option<StoredDocument>), StorageError> {
    let address = conversation_document_address(&definition.common.kind, conversation);
    let id = address_id(&address);
    if let Some(staged) = tx.latest_document(&id) {
        if !staged.retire {
            return Ok((staged.value.clone(), None));
        }
    }
    let storage = state.storage.storage();
    let record = state
        .storage
        .read(
            context,
            storage.find_document(address, DocumentPoint::Current),
        )
        .await?;
    let stored = match record {
        Some(record) => {
            state
                .storage
                .read(context, storage.document(record.id, DocumentPoint::Current))
                .await?
        }
        None => None,
    };
    let value = match &stored {
        Some(stored) => {
            definition.materialize_value(&stored.record, stored.version, stored.value.clone())?
        }
        None => definition.initial_value(),
    };
    Ok((value, stored))
}

/// Stage one conversation document, preserving its loaded incarnation when it
/// already exists.
pub(crate) fn stage_conversation_document_state(
    tx: &mut Transaction,
    conversation: ConversationId,
    definition: &AnyDocDefinition,
    value: JsonObject,
    committed: Option<StoredDocument>,
) {
    let address = conversation_document_address(&definition.common.kind, conversation);
    let id = address_id(&address);
    let retiring = tx.latest_document(&id).is_some_and(|staged| staged.retire);
    if !retiring {
        if let Some(staged) = tx.latest_document_mut(&id) {
            staged.value = value;
            return;
        }
    }
    let loaded = if retiring {
        None
    } else {
        committed.map(|stored| LoadedDocument {
            address_id: id.clone(),
            record: stored.record,
            stored_version: stored.version,
            value_version: definition.common.version,
            deltas_since_base: stored.deltas_since_base,
            value: stored.value,
        })
    };
    let created = loaded.is_none();
    tx.stage_document(StagedDocument {
        address,
        address_id: id,
        definition: Some(DocDefinition {
            kind: definition.common.kind.clone(),
            version: definition.common.version,
            semantics: definition.common.semantics,
        }),
        family: None,
        value,
        created,
        loaded,
        retire: false,
    });
}

/// The address of one conversation document.
fn conversation_document_address(kind: &str, conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: kind.to_owned(),
        scope: DocumentScope::Conversation {
            conversation_id: conversation,
        },
        key: None,
    }
}

// ─── shared helpers ─────────────────────────────────────────────────────────

/// Fault the task with `message`: the scheduler catches the panic and settles
/// the task `faulted`, exactly like an upstream throw from a phase.
fn fault(message: String) {
    panic!("{message}");
}

/// Append a provider result and add its usage (upstream `appendAssistant`).
async fn append_assistant_in(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    message: &AssistantMessage,
    by_task: TaskId,
) -> Result<EntryRecord, TaskRuntimeError> {
    record_usage_in(harness, tx, conversation, message)?;
    harness
        .store
        .append_entry(tx, conversation, assistant_draft(message), by_task)
        .await
        .map_err(TaskRuntimeError::Invalid)
}

fn assistant_draft(message: &AssistantMessage) -> EntryDraft {
    EntryDraft {
        kind: "pi.assistant".to_owned(),
        model: Some(vec![
            serde_json::to_value(message).expect("an assistant message is JSON-serializable")
        ]),
        ..EntryDraft::default()
    }
}

/// Connect a throttled partial to its record (kept for the classification
/// commit's documented ordering).
///
/// Add one response's usage to `pi.usage` (upstream `recordUsage`).
fn record_usage_in(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    message: &AssistantMessage,
) -> Result<(), TaskRuntimeError> {
    let mut usage: UsageState = from_object(
        harness
            .store
            .document_in(tx, conversation, DocumentKind::Usage)
            .map_err(TaskRuntimeError::Invalid)?,
    );
    usage_add(
        &mut usage,
        UsageScope::Models,
        &format!("{}/{}", message.provider, message.model),
        &message.usage,
    );
    harness
        .store
        .stage_document(tx, conversation, DocumentKind::Usage, to_object(&usage))
        .map_err(TaskRuntimeError::Invalid)?;
    Ok(())
}

/// Append a committed partial left by an interrupted, aborted, faulted or
/// orphaned attempt as an aborted assistant entry, then clear it in `live`
/// (upstream `convertPartial` plus the caller's delete).
async fn convert_partial_in(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    by_task: TaskId,
    live: &mut LiveState,
    unreadable: &Mutex<Option<String>>,
) -> Result<(), TaskRuntimeError> {
    let partial = live
        .generation
        .as_mut()
        .and_then(|generation| generation.message.take());
    if let Some(partial) = partial {
        let mut message: AssistantMessage = match serde_json::from_value(partial) {
            Ok(message) => message,
            Err(error) => {
                let message = format!(
                    "the live generation partial is not a valid assistant message: {error}"
                );
                *unreadable
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
                return Err(TaskRuntimeError::Invalid(message));
            }
        };
        message.stop_reason = StopReason::Aborted;
        record_usage_in(harness, tx, conversation, &message)?;
        harness
            .store
            .append_entry(tx, conversation, assistant_draft(&message), by_task)
            .await
            .map_err(TaskRuntimeError::Invalid)?;
    }
    live.generation = None;
    Ok(())
}

fn load_live_in(
    harness: &Arc<GenerationHarness>,
    tx: &Transaction,
    conversation: ConversationId,
) -> Result<LiveState, TaskRuntimeError> {
    Ok(live_from_object(
        &harness
            .store
            .document_in(tx, conversation, DocumentKind::Live)
            .map_err(TaskRuntimeError::Invalid)?,
    ))
}

fn stage_live_in(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    live: &LiveState,
) -> Result<(), TaskRuntimeError> {
    harness
        .store
        .stage_document(tx, conversation, DocumentKind::Live, live_to_object(live))
        .map_err(TaskRuntimeError::Invalid)?;
    Ok(())
}

fn load_inbox_in(
    harness: &Arc<GenerationHarness>,
    tx: &Transaction,
    conversation: ConversationId,
) -> Result<InboxState, TaskRuntimeError> {
    Ok(from_object(
        harness
            .store
            .document_in(tx, conversation, DocumentKind::Inbox)
            .map_err(TaskRuntimeError::Invalid)?,
    ))
}

fn stage_inbox_in(
    harness: &Arc<GenerationHarness>,
    tx: &mut Transaction,
    conversation: ConversationId,
    inbox: &InboxState,
) -> Result<(), TaskRuntimeError> {
    harness
        .store
        .stage_document(tx, conversation, DocumentKind::Inbox, to_object(inbox))
        .map_err(TaskRuntimeError::Invalid)?;
    Ok(())
}

/// Decode a document object, falling back to the type's default.
fn from_object<T: DeserializeOwned + Default>(object: JsonObject) -> T {
    serde_json::from_value(Value::Object(object)).unwrap_or_default()
}

/// Encode a document value as an object.
fn to_object<T: Serialize>(value: &T) -> JsonObject {
    match serde_json::to_value(value) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

/// The declaration a registry tool registers for the model.
fn tool_declaration(tool: &ToolRegistration) -> Tool {
    Tool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
        constrained_sampling: None,
    }
}

/// `CompactionResult.entryId`, when the result carried one.
fn compacted_entry_id(result: &Value) -> Option<EntryId> {
    result
        .get("entryId")
        .and_then(Value::as_i64)
        .map(EntryId::from_raw)
}

/// Error text for a generation whose blocking compaction ended without
/// completing (upstream `compactionFailure`).
fn compaction_failure(outcome: &TaskOutcome) -> String {
    match outcome {
        TaskOutcome::Failed { error, .. } | TaskOutcome::Faulted { error } => {
            format!("Compaction failed: {}", error.message)
        }
        TaskOutcome::Orphaned { reason } => format!("Compaction failed: {reason}"),
        TaskOutcome::Aborted { .. } => "Compaction was aborted".to_owned(),
        TaskOutcome::Completed { .. } => "Compaction completed".to_owned(),
    }
}

#[cfg(any())]
mod _unused {
    // `TaskState` and `NextTaskState` are referenced only through helper
    // signatures; keep the import meaningful for readers of the checkpoint
    // documentation.
}
