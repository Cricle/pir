//! The built-in `pi.compaction` task (spec §8.7).
//!
//! Port of upstream `packages/durable/src/harness/compaction.ts` (483 lines).
//! A compaction selects an old prefix of the model context, summarizes it, and
//! places a summary entry whose `head` is the first kept entry. Raw history
//! stays in storage.
//!
//! # Shapes
//!
//! | upstream | here |
//! |---|---|
//! | `CompactionInput { reason, instructions? }` | [`CompactionInput`] |
//! | `SummaryRequest` | [`SummaryRequest`] |
//! | `CompactionCheckpoint` (`select` / `summarize` / `retry`) | [`CompactionCheckpoint`] |
//! | `CompactionResult { entryId?, submissionId? }` | [`CompactionResult`] |
//! | `CompactionHooks.beforeCompact` | [`CompactionHooks::before_compact`] |
//! | `selectCut` / `summarizedMessages` / `estimateContext` | [`select_cut`] / [`summarized_messages`] / [`estimate_context`] |
//! | `serializeConversation` / prompts | [`serialize_conversation`] / [`summary_prompt`] |
//! | `compactionPolicy` | [`compaction_policy`] |
//! | `createCompaction` | [`create_compaction_in`] (+ [`compaction_ownership`], [`compaction_status_of`]) |
//! | `addCompactionStatus` etc. (`live.ts`) | [`add_compaction_status`] etc. |
//! | `thresholdCompaction` (`generation.ts`) | [`threshold_compaction`] |
//!
//! # The task
//!
//! [`compaction_task`] builds the executable definition: phases `select`,
//! `summarize` and `retry`, plus the abort handler, wired to a
//! [`CompactionHost`] and an ordered list of [`CompactionHooks`]. `select`
//! resolves the agent and settings, derives the context, selects the cut, runs
//! `beforeCompact`, and either completes, places a hook-supplied summary, or
//! pins the summarization request into the checkpoint. `summarize` derives the
//! summarized range again at the pinned `tail`, asks the host for one
//! completion, and classifies it in one commit (usage, live status, sleeping,
//! placement or failure). `retry` sleeps until `until` through
//! [`TaskRuntime::sleep_until`] and returns to `summarize` with the next
//! attempt. The abort handler removes the status and ends `aborted`.
//!
//! # Two seams
//!
//! This port's [`TaskRuntime`] is deliberately narrow: it owns commits, sleeps,
//! timers, document reads and task reads, but not the harness services
//! upstream's `runtime.agent`, `runtime.models`, `runtime.context`,
//! `runtime.settings` and `runtime.hooks` provide. Those are expressed here as
//! one host trait:
//!
//! * [`CompactionHost`] — agent and model resolution, committed context reads,
//!   the pinned summarization completion, and the placement commit (direct
//!   append or write-submission admission, including the `stale` settlement).
//! * [`CompactionHooks`] — the extension `beforeCompact` surface; generation
//!   consumes this trait later, so it is defined here rather than in
//!   `generation.rs`.
//!
//! The host is captured by [`compaction_task`]'s handlers; the harness that
//! owns the [`Session`](crate::durable::session::facade::Session) implements
//! it.
//!
//! # Deliberate deviations from upstream
//!
//! * **Placement is its own commit.** pir's `TaskRuntime::commit` exposes no
//!   in-commit document read (upstream's `tx.doc`), so the host appends the
//!   summary (or admits the write submission) and the task then commits its
//!   terminal outcome. Upstream places the entry in the same commit that
//!   removes the status and completes. A crash between the two re-runs
//!   `summarize`; a conversation-owned placement is deduplicated by the write's
//!   request ID `compaction:<taskId>`, and a blocking append is required to be
//!   idempotent per task (documented on [`CompactionHost::place`]).
//! * **`pi.live` and `pi.usage` drafts are read just before the commit** and
//!   staged with that committed value (a draft staged by the same transaction
//!   wins). Upstream reads the documents inside the commit; a concurrent
//!   writer in the read/commit window is overwritten. The live state only
//!   carries compaction presentation here, so the window is narrow but real.
//! * **`after_compact` is a pir extension.** Upstream's `CompactionHooks` has
//!   only `beforeCompact`. The lane brief requires `after_compact`, so it is a
//!   notification hook that runs after the host placed the summary and before
//!   the terminal commit; it cannot change the outcome.
//! * **Token estimation is local.** Upstream imports `estimateMessageTokens`
//!   and `calculateContextTokens` from pi-ai `utils/estimate` (3.5 chars/token,
//!   4800 chars per image, system text plus sections plus tools).
//!   `crate::compaction::algorithm` estimates at 4 chars/token for the
//!   coding-agent algorithm, so it is deliberately *not* reused:
//!   [`estimate_message_tokens`] and [`calculate_context_tokens`] implement the
//!   pi-ai semantics here. Character counts use Unicode scalar values where
//!   upstream counts UTF-16 code units.
//! * **Host errors commit a `failed` outcome** with
//!   `detail.reason = "host_error"` instead of throwing into the scheduler's
//!   `faulted` rule. Extension-hook errors are reported through
//!   [`TaskRuntime::report`] and skipped, exactly as upstream's `hooks.each`.
//! * **Phase failures are structured, not panics.** Each phase body returns
//!   [`CompactionTaskError`]; the `run_phase` boundary converts it into a
//!   terminal `failed` outcome whose `detail.reason` distinguishes corrupt
//!   stored state (`invalid_input`, `invalid_checkpoint`), a rejected runtime
//!   operation (`runtime_error`), an unreadable document
//!   (`document_read_error`), a host callback (`host_error`) and a caught
//!   internal invariant (`internal_error`). The boundary also `catch_unwind`s
//!   the handler future, so a violated invariant cannot unwind through the
//!   scheduler's actor task. An abort, or a rejection that says the invocation
//!   no longer owns the task, is left to the scheduler's own rules.
//! * **`estimate_context` locates the measured message by value.** Upstream
//!   uses `lastIndexOf` on object identity; the port borrows the message from
//!   the view and selects the last value-equal assistant message instead.
//! * The live-document helpers upstream keeps in `live.ts`
//!   (`addCompactionStatus`, `compactionStatus`, `removeCompactionStatus`,
//!   `failCompactionStatus`) live here because `live.rs` in this port carries
//!   only the document shape.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ai::types::{
    AbortSignal, AssistantMessage, CacheRetention, ContentBlock, Message, StopReason, TextContent,
    ThinkingLevel, ToolResultMessage, ToolResultRole, Usage, UserMessage, UserRole,
};
use crate::ai::util::{is_retryable_assistant_error, retry_delay_ms};
use crate::durable::context::Context;
use crate::durable::documents::{
    AnyDocDefinition, DocDefinition, DocOwner, DocumentSemantics, ResolvedAddress,
};
use crate::durable::entries::{
    CompactionEntry, COMPACTION_REASON_MANUAL, COMPACTION_REASON_OVERFLOW,
    COMPACTION_REASON_THRESHOLD,
};
use crate::durable::harness::agent::ModelRef;
use crate::durable::harness::context::{ContextView, MISSING_RESULT_TEXT};
use crate::durable::harness::generation::{
    CompactionTasks, DocumentKind, GenerationStore, HarnessFuture,
};
use crate::durable::harness::live::{
    live_from_object, live_to_object, CompactionFailure, CompactionStatus, LiveState, RetryStatus,
    LIVE_DEFINITION, LIVE_KIND, LIVE_VERSION,
};
use crate::durable::harness::scheduler::{
    panic_message, NextTaskState, TaskCatalogue, TaskRuntime, TaskRuntimeError,
};
use crate::durable::harness::settings::{
    CompactionPolicy, ConversationRetryPolicy, ConversationStreamOptions, Settings,
};
use crate::durable::harness::task::{
    define_task, phase_handler, BoxFuture, RunningTask, Task, TaskDefinitionError,
};
use crate::durable::harness::usage::{
    usage_add, UsageScope, UsageState, USAGE_DEFINITION, USAGE_KIND,
};
use crate::durable::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::durable::session::transaction::{
    EntryDraft, HeadTarget, StagedDocument, TaskOwnership, Transaction,
};
use crate::durable::session::LoadedDocument;
use crate::durable::types::{
    DocumentFork, EntryRecord, JsonObject, StoredDocument, TaskOutcome, TaskOutcomeError,
};

// ─── Task identity and prompts ──────────────────────────────────────────────

/// Registered task name of the built-in compaction task.
pub const COMPACTION_TASK_NAME: &str = "pi.compaction";

/// Definition version of the built-in compaction task.
pub const COMPACTION_TASK_VERSION: i64 = 1;

/// Longest tool result text a serialized summary source keeps.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Prefix of the user message a summary entry carries.
pub const SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Suffix of the user message a summary entry carries.
pub const SUMMARY_SUFFIX: &str = "\n</summary>";

/// The summarizer's system prompt (verbatim from upstream).
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// The summarizer's instruction prompt (verbatim from upstream).
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work. If the conversation starts with an earlier summary, preserve its information and fold the newer messages into it.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// Chars-per-token of pi-ai's `utils/estimate.ts`, which upstream durable
/// imports. `crate::compaction::algorithm` pins 4.0 for the *coding-agent*
/// algorithm and is deliberately not reused (spec §8.7 sizes compactions with
/// pi-ai's estimator).
pub const CHARS_PER_TOKEN: f64 = 3.5;

/// Character weight of an image block (upstream `ESTIMATED_IMAGE_CHARS`).
pub const ESTIMATED_IMAGE_CHARS: usize = 4800;

// ─── Task input, checkpoint and result ──────────────────────────────────────

/// Why a compaction runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    /// An explicit request (`Conversation.compact()`).
    #[default]
    Manual,
    /// The context threshold was crossed.
    Threshold,
    /// A provider request overflowed the context.
    Overflow,
}

impl CompactionReason {
    /// The persisted wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => COMPACTION_REASON_MANUAL,
            Self::Threshold => COMPACTION_REASON_THRESHOLD,
            Self::Overflow => COMPACTION_REASON_OVERFLOW,
        }
    }

    /// The reason a wire string names, if any.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            COMPACTION_REASON_MANUAL => Some(Self::Manual),
            COMPACTION_REASON_THRESHOLD => Some(Self::Threshold),
            COMPACTION_REASON_OVERFLOW => Some(Self::Overflow),
            _ => None,
        }
    }

    /// Whether `Conversation.compact()` started it.
    #[must_use]
    pub fn is_manual(self) -> bool {
        self == Self::Manual
    }
}

impl std::fmt::Display for CompactionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The task input of one compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionInput {
    /// Why the compaction runs.
    pub reason: CompactionReason,
    /// Optional focus text appended to the summarization prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl CompactionInput {
    /// A compaction with no extra instructions.
    #[must_use]
    pub fn new(reason: CompactionReason) -> Self {
        Self {
            reason,
            instructions: None,
        }
    }
}

/// The pinned summarization request (spec §8.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    /// Summarization attempt number, starting at one.
    pub attempt: i64,
    /// The model the request uses.
    pub model: ModelRef,
    /// The thinking level used as `reasoning`.
    pub thinking_level: ThinkingLevel,
    /// The settings' stream options, pinned at selection time.
    pub stream_options: ConversationStreamOptions,
    /// Output token budget.
    pub max_tokens: u64,
    /// Newest entry of the context the range was selected from.
    pub tail: EntryId,
    /// First entry kept verbatim; the summary's `head`.
    pub first_kept: EntryId,
}

/// The task checkpoint of one compaction: `select`, `summarize` or `retry`.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionCheckpoint {
    /// Nothing selected yet.
    Select,
    /// Summarize the pinned range.
    Summarize(SummaryRequest),
    /// Sleep until `until`, then summarize again.
    Retry {
        /// The pinned request.
        request: SummaryRequest,
        /// Harness-clock milliseconds at which the attempt becomes eligible.
        until: i64,
    },
}

impl CompactionCheckpoint {
    /// The checkpoint's phase name (`select`, `summarize`, `retry`).
    #[must_use]
    pub fn phase(&self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Summarize(_) => "summarize",
            Self::Retry { .. } => "retry",
        }
    }

    /// The phase's pinned request, for `summarize` and `retry`.
    #[must_use]
    pub fn request(&self) -> Option<&SummaryRequest> {
        match self {
            Self::Select => None,
            Self::Summarize(request) => Some(request),
            Self::Retry { request, .. } => Some(request),
        }
    }

    /// Serialize to the durable `Value` form: `{"phase": ..., ...request}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut object = match self {
            Self::Select => Map::new(),
            Self::Summarize(request) => request_object(request),
            Self::Retry { request, until } => {
                let mut object = request_object(request);
                object.insert("until".to_owned(), Value::from(*until));
                object
            }
        };
        object.insert("phase".to_owned(), Value::String(self.phase().to_owned()));
        Value::Object(object)
    }

    /// Parse the durable `Value` form.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "a compaction checkpoint must be a JSON object".to_owned())?;
        let phase = object
            .get("phase")
            .and_then(Value::as_str)
            .ok_or_else(|| "a compaction checkpoint needs a string `phase`".to_owned())?;
        match phase {
            "select" => Ok(Self::Select),
            "summarize" => Ok(Self::Summarize(parse_request(object)?)),
            "retry" => {
                let until = object
                    .get("until")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "a retry checkpoint needs a numeric `until`".to_owned())?;
                Ok(Self::Retry {
                    request: parse_request(object)?,
                    until,
                })
            }
            other => Err(format!("unknown compaction checkpoint phase {other:?}")),
        }
    }
}

fn request_object(request: &SummaryRequest) -> Map<String, Value> {
    match serde_json::to_value(request) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

fn parse_request(object: &Map<String, Value>) -> Result<SummaryRequest, String> {
    serde_json::from_value(Value::Object(object.clone()))
        .map_err(|error| format!("invalid compaction summary request: {error}"))
}

/// The result of one compaction: the blocking summary entry, or the write
/// submission a conversation-owned compaction admitted. Both are absent when
/// nothing was compacted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    /// Entry a blocking compaction appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<EntryId>,
    /// Write submission a conversation-owned compaction admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<SubmissionId>,
}

impl CompactionResult {
    /// The durable outcome payload (`{}` when nothing was compacted).
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()))
    }

    /// Parse a stored result value.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid compaction result: {error}"))
    }
}

// ─── Live document helpers (upstream `live.ts`) ─────────────────────────────

/// Add the status of a compaction task created in this commit.
///
/// Statuses stay in task ID order, which appending preserves: task IDs ascend
/// in commit order and removal keeps the order.
pub fn add_compaction_status(live: &mut LiveState, status: CompactionStatus) {
    live.compactions.get_or_insert_with(Vec::new).push(status);
}

/// The status of compaction task `task_id`, if listed.
#[must_use]
pub fn compaction_status(live: &LiveState, task_id: TaskId) -> Option<&CompactionStatus> {
    live.compactions
        .as_ref()?
        .iter()
        .find(|status| status.task_id == task_id)
}

/// The mutable status of compaction task `task_id`, if listed.
#[must_use]
pub fn compaction_status_mut(
    live: &mut LiveState,
    task_id: TaskId,
) -> Option<&mut CompactionStatus> {
    live.compactions
        .as_mut()?
        .iter_mut()
        .find(|status| status.task_id == task_id)
}

/// Remove the status of compaction task `task_id`, and the list once empty.
pub fn remove_compaction_status(live: &mut LiveState, task_id: TaskId) {
    let Some(statuses) = &mut live.compactions else {
        return;
    };
    statuses.retain(|status| status.task_id != task_id);
    if statuses.is_empty() {
        live.compactions = None;
    }
}

/// Remove the status of compaction task `task_id` and record that it failed
/// with `error` as the newest `compactionFailure`.
pub fn fail_compaction_status(live: &mut LiveState, task_id: TaskId, error: &str) {
    if let Some(status) = compaction_status(live, task_id) {
        live.compaction_failure = Some(CompactionFailure {
            task_id: status.task_id,
            reason: status.reason.clone(),
            error: error.to_owned(),
        });
    }
    remove_compaction_status(live, task_id);
}

/// The status a compaction task starts with (attempt 1, no retry), as the
/// commit that creates the task writes it.
#[must_use]
pub fn compaction_status_of(
    task_id: TaskId,
    reason: CompactionReason,
    blocking: bool,
) -> CompactionStatus {
    CompactionStatus {
        task_id,
        reason: reason.as_str().to_owned(),
        blocking,
        attempt: 1,
        retry: None,
    }
}

/// The ownership and `background` flag a new compaction task gets (spec §8.7):
/// a blocking child of `owner`, or a conversation-owned task that is
/// `background` unless it is `manual`.
#[must_use]
pub fn compaction_ownership(
    input: &CompactionInput,
    owner: Option<TaskId>,
) -> (TaskOwnership, bool) {
    match owner {
        Some(owner) => (TaskOwnership::Task(owner), false),
        None => (TaskOwnership::Conversation, !input.reason.is_manual()),
    }
}

/// The write-submission request ID of a conversation-owned summary.
#[must_use]
pub fn compaction_request_id(task_id: TaskId) -> String {
    format!("compaction:{task_id}")
}

// ─── Creation (upstream `createCompaction`) ─────────────────────────────────

/// Create a `pi.compaction` task together with its `pi.live` status in `tx`
/// (upstream `createCompaction`, `compaction.ts:246`).
///
/// `owner` is the generation that waits for it (a blocking compaction); without
/// one it is conversation-owned, and `background` unless the reason is manual
/// ([`compaction_ownership`]). The task, its status and the staged `pi.live`
/// draft all belong to the caller's commit, so a crash cannot leave a runnable
/// task without its status.
///
/// The `pi.live` read is read-your-writes: a draft the same transaction already
/// staged wins over the committed value.
///
/// # Errors
///
/// Returns the store's error when `pi.live` cannot be read or staged, or the
/// task cannot be created (an uninstalled `pi.compaction` definition included).
pub async fn create_compaction_in(
    store: &dyn GenerationStore,
    catalogue: &TaskCatalogue,
    tx: &mut Transaction,
    conversation: ConversationId,
    input: CompactionInput,
    owner: Option<TaskId>,
) -> Result<TaskId, String> {
    let (ownership, background) = compaction_ownership(&input, owner);
    let task_owner = match ownership {
        TaskOwnership::Task(task_id) => Some(task_id),
        TaskOwnership::Conversation => None,
    };
    // Table reads happen before the first table write, so `pi.live` is read
    // before the task creation; `document_in` is the commit's read-your-writes
    // view.
    let mut live = live_from_object(&store.document_in(tx, conversation, DocumentKind::Live)?);
    let value = serde_json::to_value(&input)
        .map_err(|error| format!("invalid compaction input: {error}"))?;
    let task_id = store
        .create_task(
            catalogue,
            tx,
            conversation,
            COMPACTION_TASK_NAME,
            value,
            task_owner,
            background,
        )
        .await?;
    let status = compaction_status_of(task_id, input.reason, task_owner.is_some());
    add_compaction_status(&mut live, status);
    store.stage_document(tx, conversation, DocumentKind::Live, live_to_object(&live))?;
    Ok(task_id)
}

/// Create a `pi.compaction` task from a host commit (`Harness::compact`).
///
/// Same semantics as [`create_compaction_in`], but the committed `pi.live`
/// read comes from [`SessionState`](crate::durable::session::line::SessionState)
/// instead of a [`GenerationStore`] mirror: the host facade has no store, and a
/// store would need a committed-document mirror this creator does not. The
/// read runs before `stage_create_task`, so the transaction's
/// read-before-write rule holds.
///
/// # Errors
///
/// Returns a description when `pi.live` cannot be read or staged, or the task
/// cannot be created.
pub(crate) async fn create_compaction_in_state(
    state: &crate::durable::session::line::SessionState<'_>,
    tx: &mut Transaction,
    conversation: ConversationId,
    input: CompactionInput,
    owner: Option<TaskId>,
) -> Result<TaskId, String> {
    let (ownership, background) = compaction_ownership(&input, owner);
    let task_owner = match ownership {
        TaskOwnership::Task(task_id) => Some(task_id),
        TaskOwnership::Conversation => None,
    };
    let address = ResolvedAddress::new(
        LIVE_KIND,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .map_err(|error| error.to_string())?;
    let address_id = address.id.clone();
    // Read-your-writes: a draft this commit already staged wins. Otherwise the
    // committed revision is read through the commit's state; an absent document
    // is created from its initial value ({}).
    let (mut live, loaded, created) = match tx.latest_document(&address_id) {
        Some(staged) => (
            live_from_object(&staged.value),
            staged.loaded.clone(),
            staged.created,
        ),
        None => {
            let context = tx.context().clone();
            let storage = state.storage.storage();
            let record = state
                .storage
                .read(
                    &context,
                    storage.find_document(
                        address.address.clone(),
                        crate::durable::types::DocumentPoint::Current,
                    ),
                )
                .await
                .map_err(|error| error.to_string())?;
            match record {
                Some(record) => {
                    let stored = state
                        .storage
                        .read(
                            &context,
                            storage
                                .document(record.id, crate::durable::types::DocumentPoint::Current),
                        )
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "pi.live current revision is missing".to_owned())?;
                    // Match `read_conversation_document_state`: materialize the
                    // stored value at the definition's version before parsing or
                    // staging it. A new-version base then carries the migrated
                    // shape, and `value_version` names that shape.
                    let value = LIVE_DEFINITION
                        .materialize_value(&record, stored.version, stored.value.clone())
                        .map_err(|error| error.to_string())?;
                    let loaded = LoadedDocument {
                        address_id: address_id.clone(),
                        record,
                        stored_version: stored.version,
                        value_version: LIVE_VERSION,
                        deltas_since_base: stored.deltas_since_base,
                        value: value.clone(),
                    };
                    (live_from_object(&value), Some(loaded), false)
                }
                None => (LiveState::default(), None, true),
            }
        }
    };
    let value = serde_json::to_value(&input)
        .map_err(|error| format!("invalid compaction input: {error}"))?;
    let snapshot = crate::durable::session::facade::TaskSnapshot {
        kind: COMPACTION_TASK_NAME.to_owned(),
        version: COMPACTION_TASK_VERSION,
        checkpoint: initial_checkpoint(&value),
    };
    let options = crate::durable::session::transaction::TaskOptions {
        conversation_id: Some(conversation),
        ownership,
        background,
        abandon_on_restart: false,
        submission_ids: Vec::new(),
        trace: None,
    };
    let task_id =
        crate::durable::session::facade::stage_create_task(tx, state, snapshot, value, options)
            .await
            .map_err(|error| error.to_string())?;
    let status = compaction_status_of(task_id, input.reason, task_owner.is_some());
    add_compaction_status(&mut live, status);
    tx.stage_document(StagedDocument {
        address: address.address,
        address_id,
        definition: Some(DocDefinition {
            kind: LIVE_KIND.to_owned(),
            version: LIVE_VERSION,
            semantics: LIVE_DEFINITION.common.semantics,
        }),
        family: None,
        value: live_to_object(&live),
        created,
        loaded,
        retire: false,
    });
    Ok(task_id)
}

/// [`CompactionTasks`] over a [`GenerationStore`], for
/// `GenerationHarness.compaction`.
///
/// The catalogue arrives with each [`create`](CompactionTasks::create) call
/// (the runtime's current registry), so the adapter only carries the store.
pub struct StoreCompactionTasks {
    store: Arc<dyn GenerationStore>,
}

impl StoreCompactionTasks {
    /// A creator that stages into the caller's commit through `store`.
    #[must_use]
    pub fn new(store: Arc<dyn GenerationStore>) -> Self {
        Self { store }
    }
}

impl CompactionTasks for StoreCompactionTasks {
    fn create<'a>(
        &'a self,
        catalogue: &'a TaskCatalogue,
        tx: &'a mut Transaction,
        conversation: ConversationId,
        reason: CompactionReason,
        owner: Option<TaskId>,
    ) -> HarnessFuture<'a, TaskId> {
        Box::pin(async move {
            create_compaction_in(
                &*self.store,
                catalogue,
                tx,
                conversation,
                CompactionInput::new(reason),
                owner,
            )
            .await
        })
    }
}

// ─── Placement ──────────────────────────────────────────────────────────────

/// What the host does with a summary.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryPlacement {
    /// The entry a blocking compaction appends: `pi.compaction` with `head` at
    /// the first kept entry, one user message holding the wrapped summary, and
    /// `data.reason`.
    pub entry: EntryDraft,
    /// Request ID of the write submission (`compaction:<taskId>`).
    pub request_id: String,
    /// First kept entry; the draft's `head`.
    pub first_kept: EntryId,
    /// Whether a generation waits for the summary: append it directly.
    pub blocking: bool,
    /// Whether the summary cuts before the current context's start. A
    /// conversation-owned placement settles `unanswered` with reason `stale`
    /// instead of appending (spec §6).
    pub stale: bool,
}

/// Where a summary landed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlacedSummary {
    /// Entry a blocking compaction appended.
    pub entry_id: Option<EntryId>,
    /// Submission a conversation-owned compaction admitted.
    pub submission_id: Option<SubmissionId>,
}

impl PlacedSummary {
    /// The task result for this placement.
    #[must_use]
    pub fn result(self) -> CompactionResult {
        CompactionResult {
            entry_id: self.entry_id,
            submission_id: self.submission_id,
        }
    }
}

/// The active range start of a context: the newest head marker's `head`.
///
/// A summary whose own `head` is older would bring back history that marker
/// cut, so it settles `stale` (spec §6).
#[must_use]
pub fn active_range_start(view: &ContextView) -> Option<EntryId> {
    view.head.as_ref().and_then(|head| head.head)
}

/// Whether a summary cutting at `first_kept` predates the active range start.
#[must_use]
pub fn summary_is_stale(first_kept: EntryId, active_range_start: Option<EntryId>) -> bool {
    active_range_start.is_some_and(|start| first_kept < start)
}

/// What a host does with a chosen summary (spec §6 and §8.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementAction {
    /// A blocking compaction appends the entry directly.
    Append,
    /// A conversation-owned compaction admits the entry as a write
    /// submission with request ID `compaction:<taskId>`.
    Admit,
    /// A conversation-owned summary cuts before the current context's start:
    /// it becomes a write submission settled `unanswered` with reason
    /// `stale`, and nothing is appended.
    SettleStale,
}

/// Decide [`PlacementAction`] from blocking-ness and staleness.
///
/// A blocking compaction never settles stale: its generation holds the run, so
/// nothing else writes the conversation between selection and placement.
#[must_use]
pub fn placement_action(blocking: bool, stale: bool) -> PlacementAction {
    if blocking {
        return PlacementAction::Append;
    }
    if stale {
        PlacementAction::SettleStale
    } else {
        PlacementAction::Admit
    }
}

/// The summary message text: the wrapped summary.
#[must_use]
pub fn wrap_summary(summary: &str) -> String {
    format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}")
}

fn summary_message(summary: &str, now: u64) -> Value {
    let message = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: wrap_summary(summary),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: now,
    });
    serde_json::to_value(message).unwrap_or(Value::Null)
}

fn summary_data(reason: CompactionReason) -> Value {
    serde_json::json!({ "reason": reason.as_str() })
}

/// The entry draft a blocking placement appends: `pi.compaction` with `head` at
/// the first kept entry, one user message holding the wrapped summary, and
/// `data.reason`.
#[must_use]
pub fn summary_entry_draft(
    first_kept: EntryId,
    summary: &str,
    reason: CompactionReason,
    now: u64,
) -> EntryDraft {
    EntryDraft {
        kind: CompactionEntry::KIND.kind.to_owned(),
        model: Some(vec![summary_message(summary, now)]),
        data: Some(summary_data(reason)),
        head: Some(HeadTarget::Entry(first_kept)),
        edits: None,
    }
}

/// [`summary_entry_draft`] as the plain JSON object a write submission carries.
#[must_use]
pub fn summary_entry(
    first_kept: EntryId,
    summary: &str,
    reason: CompactionReason,
    now: u64,
) -> JsonObject {
    let mut object = Map::new();
    object.insert(
        "kind".to_owned(),
        Value::String(CompactionEntry::KIND.kind.to_owned()),
    );
    object.insert(
        "head".to_owned(),
        serde_json::to_value(first_kept).unwrap_or(Value::Null),
    );
    object.insert(
        "model".to_owned(),
        Value::Array(vec![summary_message(summary, now)]),
    );
    object.insert("data".to_owned(), summary_data(reason));
    object
}

// ─── Hooks ──────────────────────────────────────────────────────────────────

/// What a `beforeCompact` hook sees (upstream's inline hook argument).
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionDraft {
    /// Why the compaction runs.
    pub reason: CompactionReason,
    /// The active entries the summary replaces, head marker first.
    pub entries: Vec<EntryRecord>,
    /// Their model context, the summarizer's source.
    pub messages: Vec<Message>,
    /// First entry kept verbatim.
    pub first_kept: EntryId,
    /// Focus text from the task input.
    pub instructions: Option<String>,
}

/// A `beforeCompact` decision. The first decision wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionDecision {
    /// Complete without compacting.
    Decline,
    /// Place this summary instead of asking the model.
    Supply(String),
}

/// The extension hooks of the built-in compaction task.
///
/// Upstream dispatches one handler per selected extension in extension order
/// and keeps the first decision; a [`Vec`] of these is that ordered dispatch,
/// and one element is one handler. A handler that fails is reported through
/// [`TaskRuntime::report`] and skipped, exactly like upstream's `hooks.each`.
///
/// `after_compact` is a pir extension (upstream has no `afterCompact`): it runs
/// after the host placed a summary and before the terminal commit, and cannot
/// change the outcome.
pub trait CompactionHooks: Send + Sync {
    /// After range selection, before summarizing.
    fn before_compact<'a>(
        &'a self,
        compaction: &'a CompactionDraft,
        context: &'a Context,
    ) -> BoxFuture<'a, Result<Option<CompactionDecision>, String>>;

    /// After a summary was placed.
    fn after_compact<'a>(
        &'a self,
        placed: &'a PlacedSummary,
        context: &'a Context,
    ) -> BoxFuture<'a, ()> {
        let _ = (placed, context);
        Box::pin(async {})
    }
}

/// A hooks object that never decides and never notifies.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCompactionHooks;

impl CompactionHooks for NoCompactionHooks {
    fn before_compact<'a>(
        &'a self,
        _compaction: &'a CompactionDraft,
        _context: &'a Context,
    ) -> BoxFuture<'a, Result<Option<CompactionDecision>, String>> {
        Box::pin(async { Ok(None) })
    }
}

// ─── Host seam ──────────────────────────────────────────────────────────────

/// The agent fields compaction reads (upstream `runtime.agent(context)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionAgent {
    /// Model the conversation uses; absent fails with `no_model`.
    pub model: Option<ModelRef>,
    /// Thinking level of the summarization request.
    pub thinking_level: ThinkingLevel,
}

impl Default for CompactionAgent {
    fn default() -> Self {
        Self {
            model: None,
            thinking_level: ThinkingLevel::Off,
        }
    }
}

/// The model fields compaction reads (upstream `models.getModel(...)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactionModel {
    /// Maximum output tokens; `0` means "the model does not cap it".
    pub max_tokens: u64,
}

/// One pinned summarization request for the host's provider bridge
/// (upstream `models.completeSimple`).
#[derive(Clone, Debug)]
pub struct CompactionCompletion {
    /// The conversation the compaction runs for.
    pub conversation_id: ConversationId,
    /// The model to complete with; the host resolves it through its catalogue.
    pub model: ModelRef,
    /// The two summarization messages: the system prompt and the serialized
    /// transcript in a user message.
    pub messages: Vec<Message>,
    /// The pinned stream options, with `deferred` removed and
    /// `cacheRetention: "none"`.
    pub stream_options: ConversationStreamOptions,
    /// Output token budget.
    pub max_tokens: u64,
    /// Thinking level, forwarded as `reasoning` unless `off`.
    pub thinking_level: ThinkingLevel,
    /// The invocation signal; a request cut short by it must not commit a
    /// response.
    pub signal: Arc<AbortSignal>,
}

/// The harness services a compaction run needs beyond [`TaskRuntime`].
///
/// Upstream reads these from `runtime.agent`, `runtime.models`,
/// `runtime.context`, `runtime.settings`, `runtime.hooks`, `tx.doc`,
/// `admitSubmission` and `tx.appendEntry`. The port's runtime does not carry
/// them yet, so they are one host seam; the harness that owns the
/// [`Session`](crate::durable::session::facade::Session) implements it.
pub trait CompactionHost: Send + Sync {
    /// Resolved settings, read at each use.
    fn settings(&self) -> Settings;

    /// The conversation's agent resolution.
    fn agent(&self, conversation_id: ConversationId) -> Option<CompactionAgent>;

    /// The model a reference names, if the catalogue knows it.
    fn model(&self, reference: &ModelRef) -> Option<CompactionModel>;

    /// The committed model context at `at` (the current context when absent).
    ///
    /// A failure is committed as the task's `failed` outcome with reason
    /// `host_error`.
    fn context<'a>(
        &'a self,
        conversation_id: ConversationId,
        at: Option<EntryId>,
        context: Context,
    ) -> BoxFuture<'a, Result<ContextView, String>>;

    /// The start of the conversation's current active range (the newest head
    /// marker's `head`), for the `stale` check at placement time.
    fn active_range_start<'a>(
        &'a self,
        conversation_id: ConversationId,
        context: Context,
    ) -> BoxFuture<'a, Result<Option<EntryId>, String>>;

    /// One non-streaming summarization completion.
    ///
    /// The host attaches the conversation's persisted provider session ID
    /// (upstream `ensureProviderSessionId`), which this port's provider lane
    /// owns, and translates the pinned options into its provider request.
    fn complete<'a>(
        &'a self,
        request: CompactionCompletion,
        context: Context,
    ) -> BoxFuture<'a, Result<AssistantMessage, String>>;

    /// Place the summary.
    ///
    /// [`placement_action`] decides [`PlacementAction::Append`] or
    /// [`PlacementAction::Admit`]/[`PlacementAction::SettleStale`]. An append
    /// returns `entry_id` and must not append twice for the same task; an
    /// admission follows spec §6 with `request_id` and returns `submission_id`
    /// (a stale settlement still has a submission: it is terminal
    /// `unanswered` with reason `stale`).
    fn place<'a>(
        &'a self,
        conversation_id: ConversationId,
        placement: &'a SummaryPlacement,
        context: Context,
    ) -> BoxFuture<'a, Result<PlacedSummary, String>>;
}

// ─── Token estimation (pi-ai `utils/estimate.ts`) ───────────────────────────

/// Total context tokens from a usage record (upstream
/// `calculateContextTokens`): `totalTokens`, else the component sum.
#[must_use]
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

/// Estimated tokens of a text at pi-ai's 3.5 chars/token.
#[must_use]
pub fn estimate_text_tokens(text: &str) -> u64 {
    ceil_div(text.chars().count(), CHARS_PER_TOKEN)
}

fn ceil_div(chars: usize, per_token: f64) -> u64 {
    (chars as f64 / per_token).ceil() as u64
}

fn estimate_tools_tokens(tools: Option<&Value>) -> u64 {
    match tools {
        None | Some(Value::Null) => 0,
        Some(tools) => match serde_json::to_string(tools) {
            Ok(json) => estimate_text_tokens(&json),
            Err(_) => 0,
        },
    }
}

fn estimate_content_chars(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.chars().count(),
            ContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
            ContentBlock::Thinking(thinking) => thinking.thinking.chars().count(),
            ContentBlock::ToolCall(call) => {
                let arguments = serde_json::to_string(&call.arguments).unwrap_or_default();
                call.name.chars().count() + arguments.chars().count()
            }
        })
        .sum()
}

/// Estimate the token count of one message (upstream `estimateMessageTokens`).
///
/// A system message counts its rendered prompt (content plus sections) and its
/// added/removed tools; user and tool-result content counts text and a fixed
/// weight per image; an assistant message counts text, thinking and tool-call
/// names plus JSON arguments. A `developer` message counts like a user message
/// (pir has that role; pi-ai's estimator does not).
#[must_use]
pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            let text = crate::ai::types::system_message_text(system);
            let added = system
                .tools_added
                .as_ref()
                .and_then(|tools| serde_json::to_value(tools).ok());
            let removed = system
                .tools_removed
                .as_ref()
                .and_then(|tools| serde_json::to_value(tools).ok());
            estimate_text_tokens(&text)
                .saturating_add(estimate_tools_tokens(added.as_ref()))
                .saturating_add(estimate_tools_tokens(removed.as_ref()))
        }
        Message::User(user) | Message::Developer(user) => {
            ceil_div(estimate_content_chars(&user.content), CHARS_PER_TOKEN)
        }
        Message::Assistant(assistant) => {
            let chars: usize = assistant
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text(text) => text.text.chars().count(),
                    ContentBlock::Thinking(thinking) => thinking.thinking.chars().count(),
                    ContentBlock::ToolCall(call) => {
                        let arguments = serde_json::to_string(&call.arguments).unwrap_or_default();
                        call.name.chars().count() + arguments.chars().count()
                    }
                    ContentBlock::Image(_) => 0,
                })
                .sum();
            ceil_div(chars, CHARS_PER_TOKEN)
        }
        Message::ToolResult(result) => {
            ceil_div(estimate_content_chars(&result.content), CHARS_PER_TOKEN)
        }
    }
}

/// Size of a request over `view` followed by `extra` (upstream
/// `estimateContext`): the usage of the newest assistant appended after the
/// head marker, whose request included the marker, plus estimates of the
/// messages after it; without one, estimates of every message.
#[must_use]
pub fn estimate_context(view: &ContextView, extra: &[Message]) -> u64 {
    let after = view.head.as_ref().map_or(i64::MIN, |head| head.id.as_i64());
    let mut measured: Option<&AssistantMessage> = None;
    for index in (0..view.entries.len()).rev() {
        if view.entries[index].id.as_i64() <= after {
            continue;
        }
        if let Some(assistant) =
            view.contributions[index]
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::Assistant(assistant)
                        if calculate_context_tokens(&assistant.usage) > 0 =>
                    {
                        Some(assistant)
                    }
                    _ => None,
                })
        {
            measured = Some(assistant);
            break;
        }
    }
    let from = measured.map_or(0, |measured| {
        view.messages
            .iter()
            .rposition(|message| match message {
                // `timestamp` is part of equality, so this rejects almost
                // every other assistant before the value comparison.
                Message::Assistant(found) => {
                    found.timestamp == measured.timestamp && found == measured
                }
                _ => false,
            })
            .map_or(0, |index| index + 1)
    });
    let mut tokens = measured.map_or(0, |measured| calculate_context_tokens(&measured.usage));
    for message in view.messages.iter().skip(from) {
        tokens = tokens.saturating_add(estimate_message_tokens(message));
    }
    for message in extra {
        tokens = tokens.saturating_add(estimate_message_tokens(message));
    }
    tokens
}

// ─── Range selection ────────────────────────────────────────────────────────

/// Index in `view.entries` of the first entry a summary keeps, or `None` when
/// there is nothing to compact (spec §8.7).
///
/// Walks back from the tail until `keep_recent_tokens` are kept, then cuts at
/// the first candidate at or after that entry (the newest candidate when none
/// follows): an entry whose contribution starts with a user or assistant
/// message, never a tool result or a system entry, and never a user entry that
/// a result of the preceding assistant's calls still follows. Nothing is
/// compacted when the sum never reaches the threshold, or when no non-marker
/// entry before the cut contributes a model message.
#[must_use]
pub fn select_cut(view: &ContextView, keep_recent_tokens: u64) -> Option<usize> {
    let contributions = &view.contributions;
    let start = usize::from(view.head.is_some());
    let candidates: Vec<usize> = (start..contributions.len())
        .filter(|index| is_candidate(contributions, *index))
        .collect();
    let mut kept: u64 = 0;
    let mut cut: Option<usize> = None;
    for index in (start..contributions.len()).rev() {
        for message in &contributions[index] {
            kept = kept.saturating_add(estimate_message_tokens(message));
        }
        if kept < keep_recent_tokens {
            continue;
        }
        cut = candidates
            .iter()
            .copied()
            .find(|candidate| *candidate >= index)
            .or_else(|| candidates.last().copied());
        break;
    }
    let cut = cut?;
    for contribution in &contributions[start..cut] {
        if !contribution.is_empty() {
            return Some(cut);
        }
    }
    None
}

fn is_candidate(contributions: &[Vec<Message>], index: usize) -> bool {
    let Some(first) = contributions[index].first() else {
        return false;
    };
    match first {
        Message::Assistant(_) => true,
        Message::User(_) => {
            // A result of the preceding assistant's calls that follows this
            // entry, before the next assistant, belongs before it.
            let mut calls: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for before in (0..index).rev() {
                let assistant =
                    contributions[before]
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            Message::Assistant(assistant) => Some(assistant),
                            _ => None,
                        });
                let Some(assistant) = assistant else {
                    continue;
                };
                calls = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(call.id.as_str()),
                        _ => None,
                    })
                    .collect();
                break;
            }
            if calls.is_empty() {
                return true;
            }
            for (after, messages) in contributions.iter().enumerate().skip(index) {
                for (position, message) in messages.iter().enumerate() {
                    match message {
                        Message::Assistant(_) if after > index || position > 0 => return true,
                        Message::ToolResult(result)
                            if calls.contains(result.tool_call_id.as_str()) =>
                        {
                            return false;
                        }
                        _ => {}
                    }
                }
            }
            true
        }
        // System (and any other) contributions never begin a cut candidate.
        _ => false,
    }
}

/// Model messages of the entries before `cut`: the head marker first, ordered
/// like model context (spec §2.1, rules 7 and 8).
#[must_use]
pub fn summarized_messages(view: &ContextView, cut: usize) -> Vec<Message> {
    let flat: Vec<Message> = view
        .contributions
        .iter()
        .take(cut)
        .flat_map(|messages| messages.iter().cloned())
        .collect();
    order_tool_results(flat)
}

/// Place each assistant's tool results directly after it in call order, and
/// synthesize a missing result (spec §2.1, rule 7).
///
/// `context.rs` carries the same ordering over provenance-tracked messages,
/// but it is private and also applies the baseline rule; the summarizer's
/// source needs only this half.
#[must_use]
pub fn order_tool_results(messages: Vec<Message>) -> Vec<Message> {
    let mut slots: Vec<Option<Message>> = messages.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(slots.len());
    for index in 0..slots.len() {
        if matches!(slots[index], Some(Message::ToolResult(_)) | None) {
            continue;
        }
        // A non-result message is emitted where it stands. An assistant's
        // calls and timestamp are copied first, so the message itself can move
        // out of `slots` instead of being cloned.
        let (calls, timestamp) = match &slots[index] {
            Some(Message::Assistant(assistant)) => (
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                assistant.timestamp,
            ),
            _ => (Vec::new(), 0),
        };
        let Some(message) = slots[index].take() else {
            continue;
        };
        ordered.push(message);
        if calls.is_empty() {
            continue;
        }
        let mut results: BTreeMap<&str, usize> = BTreeMap::new();
        for (candidate_index, slot) in slots.iter().enumerate().skip(index + 1) {
            match slot {
                Some(Message::Assistant(_)) => break,
                Some(Message::ToolResult(result)) => {
                    results
                        .entry(result.tool_call_id.as_str())
                        .or_insert(candidate_index);
                }
                _ => {}
            }
        }
        let targets: Vec<Option<usize>> = calls
            .iter()
            .map(|(id, _)| results.get(id.as_str()).copied())
            .collect();
        // A result is moved out of `slots` on its last use and cloned for the
        // earlier ones, so duplicate call ids keep the duplicates they had.
        let mut uses: BTreeMap<usize, usize> = BTreeMap::new();
        for target in targets.iter().flatten() {
            *uses.entry(*target).or_default() += 1;
        }
        for ((id, name), target) in calls.into_iter().zip(targets) {
            match target {
                Some(result_index) => {
                    let last = uses.get_mut(&result_index).is_some_and(|remaining| {
                        *remaining -= 1;
                        *remaining == 0
                    });
                    let found = if last {
                        slots[result_index].take()
                    } else {
                        slots[result_index].clone()
                    };
                    if let Some(found) = found {
                        ordered.push(found);
                    }
                }
                None => ordered.push(missing_result(&id, &name, timestamp)),
            }
        }
    }
    ordered
}

fn missing_result(id: &str, name: &str, timestamp: u64) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        content: vec![ContentBlock::Text(TextContent {
            text: MISSING_RESULT_TEXT.to_owned(),
            text_signature: None,
        })],
        details: Some(serde_json::json!({ "reason": "missing_result" })),
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: true,
        duration_ms: None,
        timestamp,
    })
}

// ─── Triggers ───────────────────────────────────────────────────────────────

/// Which threshold compaction generation starts before its request (spec
/// §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Above `contextWindow - reserveTokens`: the generation waits for it.
    Blocking,
    /// Above `blocking - backgroundTokens`: an independent background
    /// compaction.
    Background,
}

/// The compaction policy of `model`: its `modelOverrides` entry over the
/// harness-wide policy, field by field (upstream `compactionPolicy`).
#[must_use]
pub fn compaction_policy(settings: &Settings, model: &ModelRef) -> CompactionPolicy {
    settings.compaction_for(&model.key())
}

/// Which threshold compaction preparation starts before its request, if any
/// (upstream `thresholdCompaction`).
///
/// `planned` is the model context already planned for the request (upstream
/// passes the planned entries' own model lists flattened); it only
/// participates in the size estimate.
#[must_use]
pub fn threshold_compaction(
    view: &ContextView,
    planned: &[Message],
    context_window: u64,
    policy: &CompactionPolicy,
) -> Option<CompactionTrigger> {
    if !policy.enabled || context_window == 0 {
        return None;
    }
    // Upstream subtracts in JavaScript numbers, where the thresholds may go
    // negative; i64 keeps that arithmetic exact instead of saturating at zero.
    let tokens = i64::try_from(estimate_context(view, planned)).unwrap_or(i64::MAX);
    let blocking = i64::try_from(context_window)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(policy.reserve_tokens).unwrap_or(i64::MAX));
    let background =
        blocking.saturating_sub(i64::try_from(policy.background_tokens).unwrap_or(i64::MAX));
    let over = if tokens > blocking {
        Some(CompactionTrigger::Blocking)
    } else if policy.background_tokens > 0 && tokens > background {
        Some(CompactionTrigger::Background)
    } else {
        None
    }?;
    select_cut(view, policy.keep_recent_tokens)?;
    Some(over)
}

/// Whether a provider context overflow starts a blocking compaction: the
/// policy is enabled and range selection finds a cut.
#[must_use]
pub fn overflow_compaction(view: &ContextView, policy: &CompactionPolicy) -> bool {
    policy.enabled && select_cut(view, policy.keep_recent_tokens).is_some()
}

// ─── Summarization request and classification ───────────────────────────────

/// The output budget of one summarization attempt:
/// `min(floor(0.8 * reserveTokens), model.maxTokens)`, the model's value used
/// only when positive.
#[must_use]
pub fn summary_max_tokens(reserve_tokens: u64, model_max_tokens: u64) -> u64 {
    let budget = (u128::from(reserve_tokens) * 4 / 5) as u64;
    if model_max_tokens > 0 {
        budget.min(model_max_tokens)
    } else {
        budget
    }
}

/// The pinned stream options of a summarization request: `deferred` removed
/// and `cacheRetention: "none"` (spec §8.7).
#[must_use]
pub fn summary_request_options(
    stream_options: &ConversationStreamOptions,
) -> ConversationStreamOptions {
    let mut options = stream_options.clone();
    options.deferred = None;
    options.cache_retention = Some(CacheRetention::None);
    options
}

/// The summarizer's user message: the serialized conversation, the prompt, and
/// any instructions.
#[must_use]
pub fn summary_prompt(messages: &[Message], instructions: Option<&str>) -> String {
    let focus = instructions.map_or_else(String::new, |instructions| {
        format!("\n\nAdditional focus: {instructions}")
    });
    format!(
        "<conversation>\n{}\n</conversation>\n\n{SUMMARIZATION_PROMPT}{focus}",
        serialize_conversation(messages)
    )
}

/// Join the text blocks of one message's content with newlines.
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Messages as plain text, so the summarizer reads a transcript instead of
/// continuing it.
///
/// System messages are omitted. Thinking is omitted too: providers return
/// summaries of the real reasoning, and replayed thinking is often not part of
/// the live context, so including it can make the request larger than the
/// context it summarizes. Visible text, tool calls and tool results record what
/// happened; tool results are truncated to [`TOOL_RESULT_MAX_CHARS`].
#[must_use]
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message {
            Message::User(user) | Message::Developer(user) => {
                let text = content_text(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let text: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(format!(
                            "{}({})",
                            call.name,
                            call.arguments
                                .iter()
                                .map(|(key, value)| format!(
                                    "{key}={}",
                                    serde_json::to_string(value)
                                        .unwrap_or_else(|_| "undefined".to_owned())
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {}", text.join("\n")));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = content_text(&result.content);
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            Message::System(_) => {}
        }
    }
    parts.join("\n\n")
}

/// Truncate text to `max_chars`, reporting how much was dropped.
///
/// Upstream slices at a UTF-16 length; this counts Unicode scalar values.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    let length = text.chars().count();
    if length <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!(
        "{head}\n\n[... {} more characters truncated]",
        length - max_chars
    )
}

/// The summary of a clean `stop` with text and no tool call; anything else is
/// not a summary.
#[must_use]
pub fn summary_text(message: &AssistantMessage) -> Option<String> {
    if message.stop_reason != StopReason::Stop
        || message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    {
        return None;
    }
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Why a response is not a summary (upstream `summaryFailure`).
#[must_use]
pub fn summary_failure(message: &AssistantMessage) -> String {
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        return format!(
            "Summarization failed: {}",
            message
                .error_message
                .clone()
                .unwrap_or_else(|| message.stop_reason.to_string())
        );
    }
    if message.stop_reason == StopReason::Length {
        return "Summarization hit the token limit; the summary is incomplete".to_owned();
    }
    if message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    {
        return "Summarization attempted to call a tool".to_owned();
    }
    "Summarization produced no text".to_owned()
}

/// When a failed summarization attempt should retry: `until` on the harness
/// clock, or `None` to fail (spec §8.7).
#[must_use]
pub fn retry_until(
    message: &AssistantMessage,
    policy: &ConversationRetryPolicy,
    attempt: i64,
    now: i64,
) -> Option<i64> {
    let retry = message.stop_reason == StopReason::Error
        && is_retryable_assistant_error(message)
        && policy.enabled
        && attempt <= i64::from(policy.max_retries);
    if !retry {
        return None;
    }
    let delay = retry_delay_ms(
        policy.base_delay_ms,
        u64::try_from(attempt).unwrap_or(1).max(1),
        Some(policy.max_agent_delay_ms),
    );
    Some(now.saturating_add(i64::try_from(delay).unwrap_or(i64::MAX)))
}

// ─── The task ───────────────────────────────────────────────────────────────

fn initial_checkpoint(_input: &Value) -> Value {
    CompactionCheckpoint::Select.to_value()
}

/// Build the executable `pi.compaction` task.
///
/// The phase handlers capture `host` and `hooks`, so one definition exists per
/// harness. The registered metadata (name, version, phases) matches upstream's
/// `CompactionTask`.
pub fn compaction_task(
    host: Arc<dyn CompactionHost>,
    hooks: Vec<Arc<dyn CompactionHooks>>,
) -> Result<Task, TaskDefinitionError> {
    let select_host = host.clone();
    let select_hooks = hooks.clone();
    let summarize_host = host.clone();
    let summarize_hooks = hooks.clone();
    let retry_host = host;

    define_task(
        COMPACTION_TASK_NAME,
        COMPACTION_TASK_VERSION,
        initial_checkpoint,
    )
    .phase(
        "select",
        phase_handler(move |task, runtime, context| {
            let host = select_host.clone();
            let hooks = select_hooks.clone();
            Box::pin(async move {
                // One boundary per phase: a typed error or a caught panic
                // settles the task durably here, never as unwinding through
                // the scheduler's actor task.
                run_phase(runtime, context.clone(), async move {
                    select_phase(&*host, &hooks, task, runtime, context).await
                })
                .await;
            })
        }),
    )
    .phase(
        "summarize",
        phase_handler(move |task, runtime, context| {
            let host = summarize_host.clone();
            let hooks = summarize_hooks.clone();
            Box::pin(async move {
                run_phase(runtime, context.clone(), async move {
                    summarize_phase(&*host, &hooks, task, runtime, context).await
                })
                .await;
            })
        }),
    )
    .phase(
        "retry",
        phase_handler(move |task, runtime, context| {
            let host = retry_host.clone();
            Box::pin(async move {
                run_phase(runtime, context.clone(), async move {
                    retry_phase(&*host, task, runtime, context).await
                })
                .await;
            })
        }),
    )
    .abort(phase_handler(move |task, runtime, context| {
        Box::pin(async move {
            run_phase(runtime, context.clone(), async move {
                abort_phase(task, runtime, context).await
            })
            .await;
        })
    }))
    .build()
}

// ── Phase plumbing ─────────────────────────────────────────────────────────

/// Why a compaction phase stopped without a durable next state.
///
/// Phase bodies return this instead of panicking; [`run_phase`] turns it into a
/// durable terminal `failed` outcome whose `detail.reason` is
/// [`reason`](Self::reason) and, for the classes that used to be panics, one
/// `task_error` report. A caller can therefore tell a corrupted stored record
/// (`invalid_input`, `invalid_checkpoint`) from a rejected runtime operation
/// (`runtime_error`), an unreadable document (`document_read_error`), a host
/// callback failure (`host_error`) or a caught internal invariant
/// (`internal_error`).
///
/// A [`Runtime`](Self::Runtime) rejection that means the invocation no longer
/// owns the task (ended, closed, terminal, not running, abort-marked) is *not*
/// a failure: the scheduler's own rules settle the task, exactly as an
/// upstream throw would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionTaskError {
    /// The stored task input does not deserialize into [`CompactionInput`].
    InvalidInput {
        /// The task.
        task: TaskId,
        /// The serde explanation.
        detail: String,
    },
    /// The stored checkpoint is not a valid checkpoint for its phase.
    InvalidCheckpoint {
        /// The task.
        task: TaskId,
        /// The parse or phase explanation.
        detail: String,
    },
    /// A gated runtime operation (commit, sleep, task or document read) was
    /// rejected.
    Runtime(TaskRuntimeError),
    /// A host callback rejected the phase (agent/context/complete/place).
    Host {
        /// The host's explanation, verbatim.
        detail: String,
    },
    /// A document the phase needs (by kind) could not be read or staged.
    Document {
        /// The document kind (`pi.live`, `pi.usage`).
        kind: &'static str,
        /// The storage or materialization explanation.
        detail: String,
    },
    /// The invocation was aborted while the host completion was in flight.
    ///
    /// Not a failure: the abort mark (or the ended invocation) settles the
    /// task through the scheduler's abort rule, which is what upstream's
    /// `throwIfAborted` throw achieves.
    Aborted {
        /// The task.
        task: TaskId,
    },
    /// An internal invariant was violated (a bug). Returned only by
    /// [`run_phase`]'s `catch_unwind`, never by a handler.
    InternalInvariant {
        /// The panic payload, rendered.
        detail: String,
    },
}

impl CompactionTaskError {
    /// A host-callback failure with its message verbatim.
    #[must_use]
    pub fn host(detail: impl Into<String>) -> Self {
        Self::Host {
            detail: detail.into(),
        }
    }

    fn document(kind: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Document {
            kind,
            detail: error.to_string(),
        }
    }

    /// The `detail.reason` string of the terminal `failed` outcome.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::InvalidInput { .. } => "invalid_input",
            Self::InvalidCheckpoint { .. } => "invalid_checkpoint",
            Self::Runtime(_) => "runtime_error",
            Self::Host { .. } => "host_error",
            Self::Document { .. } => "document_read_error",
            Self::Aborted { .. } => "aborted",
            Self::InternalInvariant { .. } => "internal_error",
        }
    }

    /// Whether the runtime rejection says this invocation no longer owns the
    /// task, so only the scheduler's rules may decide its state.
    fn releases_task(&self) -> bool {
        match self {
            Self::Runtime(error) => matches!(
                error,
                TaskRuntimeError::Ended { .. }
                    | TaskRuntimeError::Closed
                    | TaskRuntimeError::Terminal { .. }
                    | TaskRuntimeError::NotRunning { .. }
                    | TaskRuntimeError::AbortMarked { .. }
            ),
            _ => false,
        }
    }
}

impl std::fmt::Display for CompactionTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput { task, detail } => {
                write!(f, "compaction task {task} has an invalid input: {detail}")
            }
            Self::InvalidCheckpoint { task, detail } => {
                write!(
                    f,
                    "compaction task {task} has an invalid checkpoint: {detail}"
                )
            }
            Self::Runtime(error) => write!(f, "compaction runtime operation failed: {error}"),
            Self::Host { detail } => f.write_str(detail),
            Self::Document { kind, detail } => {
                write!(f, "compaction: cannot read {kind}: {detail}")
            }
            Self::Aborted { task } => {
                write!(f, "compaction task {task} was aborted during summarization")
            }
            Self::InternalInvariant { detail } => {
                write!(f, "compaction task internal invariant violated: {detail}")
            }
        }
    }
}

impl std::error::Error for CompactionTaskError {}

impl From<TaskRuntimeError> for CompactionTaskError {
    fn from(error: TaskRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// Run one phase body at the task boundary.
///
/// The two failure shapes a handler can produce both stop here: its typed
/// [`CompactionTaskError`], and a panic from an internal invariant (a violated
/// `expect`/index/`unreachable!`). Catching the panic here keeps unwinding out
/// of the scheduler's actor task and converts it into the same structured
/// failure path as any other phase error.
async fn run_phase<F>(runtime: &TaskRuntime, context: Context, body: F)
where
    F: Future<Output = Result<(), CompactionTaskError>>,
{
    match AssertUnwindSafe(body).catch_unwind().await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => settle_phase_error(runtime, context, error).await,
        Err(payload) => {
            let error = CompactionTaskError::InternalInvariant {
                detail: panic_message(payload),
            };
            settle_phase_error(runtime, context, error).await;
        }
    }
}

/// Settle a phase error durably, without unwinding.
///
/// A cooperative abort and a rejection that says the invocation ended or no
/// longer owns the task are left to the scheduler: it ends the invocation
/// through the abort/terminal rules, and a commit here would be rejected too.
/// Every other class is recorded as a terminal `failed` outcome (its
/// `detail.reason` names the class) and — for the classes that used to be
/// panics — one report.
async fn settle_phase_error(runtime: &TaskRuntime, context: Context, error: CompactionTaskError) {
    if matches!(error, CompactionTaskError::Aborted { .. }) || error.releases_task() {
        return;
    }
    let message = error.to_string();
    let reason = error.reason();
    // Host failures were always a durable `failed` outcome (`host_error`), not
    // a panic, so they keep their quiet path.
    if !matches!(error, CompactionTaskError::Host { .. }) {
        // The report reaches the sink even when the durable commit below is
        // rejected (a closing session, a lost task).
        let _ = runtime.report(&message);
    }
    // Record the failure: remove the status and present it in `pi.live`.
    // When `pi.live` itself is the unreadable document, the full failure
    // cannot be staged; the bare terminal failure still settles the task with
    // the structured reason.
    if fail_task(runtime, message.clone(), reason, context.clone())
        .await
        .is_err()
    {
        let _ = commit_bare_failure(runtime, message, reason, context).await;
    }
}

fn compaction_input(task: &RunningTask) -> Result<CompactionInput, CompactionTaskError> {
    serde_json::from_value(task.input.clone()).map_err(|error| CompactionTaskError::InvalidInput {
        task: task.id,
        detail: error.to_string(),
    })
}

fn checkpoint_of(task: &RunningTask) -> Result<CompactionCheckpoint, CompactionTaskError> {
    CompactionCheckpoint::from_value(&task.checkpoint).map_err(|detail| {
        CompactionTaskError::InvalidCheckpoint {
            task: task.id,
            detail,
        }
    })
}

/// A runtime operation the phase cannot continue without.
fn required<T>(result: Result<T, TaskRuntimeError>) -> Result<T, CompactionTaskError> {
    Ok(result?)
}

fn now(runtime: &TaskRuntime) -> Result<i64, CompactionTaskError> {
    required(runtime.now())
}

fn terminal_completed(result: CompactionResult) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: result.to_value(),
        },
    }
}

fn terminal_failed(message: String, reason: &'static str) -> NextTaskState {
    NextTaskState::Terminal {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message,
                detail: Some(serde_json::json!({ "reason": reason })),
            },
            result: None,
        },
    }
}

async fn commit_next(
    runtime: &TaskRuntime,
    context: Context,
    next: NextTaskState,
) -> Result<(), CompactionTaskError> {
    runtime
        .commit(
            move |_tx, _task| Box::pin(async move { Ok(Some(next)) }),
            context,
        )
        .await?;
    Ok(())
}

/// Whether the task is owned by another task: a blocking compaction, whose
/// generation holds the run and waits.
async fn is_blocking(runtime: &TaskRuntime) -> Result<bool, CompactionTaskError> {
    Ok(required(runtime.get_task(runtime.task_id()).await)?
        .and_then(|record| record.owner)
        .is_some())
}

// ── Document drafts ────────────────────────────────────────────────────────

/// A document read immediately before the commit that stages its new value.
///
/// `TaskRuntime::commit` exposes no in-commit document read (upstream's
/// `tx.doc`), so the committed value is captured first; a draft the same
/// transaction already staged wins at staging time. `pi.live` and `pi.usage`
/// are written this way.
struct DocumentDraft {
    resolved: ResolvedAddress,
    definition: AnyDocDefinition,
    committed: Option<StoredDocument>,
}

impl DocumentDraft {
    async fn read(
        runtime: &TaskRuntime,
        resolved: ResolvedAddress,
        definition: AnyDocDefinition,
    ) -> Result<Self, TaskRuntimeError> {
        let committed = runtime.document(&resolved).await?;
        Ok(Self {
            resolved,
            definition,
            committed,
        })
    }

    /// The committed (or initial) value, materialized at the definition's
    /// version.
    fn current(&self) -> Result<JsonObject, TaskRuntimeError> {
        match &self.committed {
            None => Ok(self.definition.initial_value()),
            Some(stored) => self
                .definition
                .materialize_value(&stored.record, stored.version, stored.value.clone())
                .map_err(TaskRuntimeError::Storage),
        }
    }

    fn identity(&self) -> DocDefinition {
        DocDefinition {
            kind: self.definition.common.kind.clone(),
            version: self.definition.common.version,
            semantics: self.definition.common.semantics,
        }
    }

    /// Stage `value` as this document's new draft in the open commit.
    fn stage(&self, tx: &mut Transaction, value: JsonObject) -> Result<(), TaskRuntimeError> {
        if let Some(staged) = tx.latest_document_mut(&self.resolved.id) {
            if !staged.retire {
                staged.value = value;
                return Ok(());
            }
        }
        let loaded = self.committed.as_ref().map(|stored| LoadedDocument {
            address_id: self.resolved.id.clone(),
            record: stored.record.clone(),
            stored_version: stored.version,
            value_version: self.definition.common.version,
            deltas_since_base: stored.deltas_since_base,
            value: stored.value.clone(),
        });
        tx.stage_document(StagedDocument {
            address: self.resolved.address.clone(),
            address_id: self.resolved.id.clone(),
            definition: Some(self.identity()),
            family: None,
            value,
            created: self.committed.is_none(),
            loaded,
            retire: false,
        });
        Ok(())
    }
}

fn conversation_document(
    kind: &str,
    conversation: ConversationId,
) -> Result<ResolvedAddress, TaskRuntimeError> {
    ResolvedAddress::new(
        kind,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .map_err(TaskRuntimeError::Storage)
}

/// The `pi.live` address of a conversation, or a runtime fault when the
/// address cannot be resolved (a contract violation: a conversation ID always
/// resolves).
fn live_document(conversation: ConversationId) -> Result<ResolvedAddress, TaskRuntimeError> {
    conversation_document(LIVE_KIND, conversation)
}

fn usage_document(conversation: ConversationId) -> Result<ResolvedAddress, TaskRuntimeError> {
    conversation_document(USAGE_KIND, conversation)
}

async fn live_draft(
    runtime: &TaskRuntime,
    conversation: ConversationId,
) -> Result<DocumentDraft, CompactionTaskError> {
    let resolved = required(live_document(conversation))?;
    DocumentDraft::read(runtime, resolved, LIVE_DEFINITION.clone())
        .await
        .map_err(|error| CompactionTaskError::document(LIVE_KIND, error))
}

async fn usage_draft(
    runtime: &TaskRuntime,
    conversation: ConversationId,
) -> Result<DocumentDraft, CompactionTaskError> {
    let resolved = required(usage_document(conversation))?;
    DocumentDraft::read(runtime, resolved, USAGE_DEFINITION.clone())
        .await
        .map_err(|error| CompactionTaskError::document(USAGE_KIND, error))
}

fn live_state_of(value: &JsonObject) -> LiveState {
    live_from_object(value)
}

fn usage_state_of(value: &JsonObject) -> UsageState {
    serde_json::from_value(Value::Object(value.clone())).unwrap_or_default()
}

fn usage_object(state: &UsageState) -> JsonObject {
    match serde_json::to_value(state) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

// ── Phase helpers ──────────────────────────────────────────────────────────

/// Remove the task's status and complete with `{}` (nothing to compact, or a
/// hook declined).
async fn complete(runtime: &TaskRuntime, context: Context) -> Result<(), CompactionTaskError> {
    let task_id = runtime.task_id();
    let draft = live_draft(runtime, runtime.conversation_id()).await?;
    runtime
        .commit(
            move |tx, _task| {
                Box::pin(async move {
                    let mut state = live_state_of(&draft.current()?);
                    remove_compaction_status(&mut state, task_id);
                    draft.stage(tx, live_to_object(&state))?;
                    Ok(Some(terminal_completed(CompactionResult::default())))
                })
            },
            context,
        )
        .await?;
    Ok(())
}

async fn fail_no_model(
    runtime: &TaskRuntime,
    reference: Option<&ModelRef>,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let message = match reference {
        None => "No model is configured".to_owned(),
        Some(reference) => format!(
            "Model {}/{} is not available",
            reference.provider, reference.model_id
        ),
    };
    fail_task(runtime, message, "no_model", context).await
}

/// Fail the task durably: set `compactionFailure`, remove the status, and
/// commit the terminal `failed` outcome in the same commit (spec §8.7).
async fn fail_task(
    runtime: &TaskRuntime,
    message: String,
    reason: &'static str,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let task_id = runtime.task_id();
    let draft = live_draft(runtime, runtime.conversation_id()).await?;
    runtime
        .commit(
            move |tx, _task| {
                Box::pin(async move {
                    let mut state = live_state_of(&draft.current()?);
                    fail_compaction_status(&mut state, task_id, &message);
                    draft.stage(tx, live_to_object(&state))?;
                    Ok(Some(terminal_failed(message, reason)))
                })
            },
            context,
        )
        .await?;
    Ok(())
}

/// Commit the terminal `failed` outcome without reading `pi.live`.
///
/// The fallback for a phase failure whose document read is itself the failing
/// operation (`document_read_error`): the task still settles with the
/// structured reason, and the (unreadable) live document is left untouched.
async fn commit_bare_failure(
    runtime: &TaskRuntime,
    message: String,
    reason: &'static str,
    context: Context,
) -> Result<(), CompactionTaskError> {
    runtime
        .commit(
            move |_tx, _task| Box::pin(async move { Ok(Some(terminal_failed(message, reason))) }),
            context,
        )
        .await?;
    Ok(())
}

/// Run the `after_compact` notification hooks. They cannot change the outcome.
async fn notify_after_compact(
    hooks: &[Arc<dyn CompactionHooks>],
    placed: &PlacedSummary,
    context: &Context,
) {
    for hook in hooks {
        hook.after_compact(placed, context).await;
    }
}

/// Build the placement for one chosen summary.
///
/// A blocking compaction never settles stale: its generation holds the run, so
/// nothing writes the conversation between selection and placement. A
/// conversation-owned one asks the host for the current active range start and
/// marks the placement stale when the cut predates it.
async fn build_placement(
    host: &dyn CompactionHost,
    runtime: &TaskRuntime,
    reason: CompactionReason,
    first_kept: EntryId,
    summary: &str,
    timestamp: u64,
    context: &Context,
) -> Result<SummaryPlacement, CompactionTaskError> {
    let conversation = runtime.conversation_id();
    let blocking = is_blocking(runtime).await?;
    let stale = if blocking {
        false
    } else {
        summary_is_stale(
            first_kept,
            host.active_range_start(conversation, context.clone())
                .await
                .map_err(CompactionTaskError::host)?,
        )
    };
    Ok(SummaryPlacement {
        entry: summary_entry_draft(first_kept, summary, reason, timestamp),
        request_id: compaction_request_id(runtime.task_id()),
        first_kept,
        blocking,
        stale,
    })
}

/// Place a chosen summary and commit the terminal outcome in the same commit
/// that removes the status and clears the newest failure. `usage` is the
/// summarization attempt's spend, recorded in that commit when present.
async fn place_and_complete(
    host: &dyn CompactionHost,
    hooks: &[Arc<dyn CompactionHooks>],
    runtime: &TaskRuntime,
    placement: SummaryPlacement,
    usage: Option<(DocumentDraft, String, Usage)>,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let conversation = runtime.conversation_id();
    let task_id = runtime.task_id();
    let placed = host
        .place(conversation, &placement, context.clone())
        .await
        .map_err(CompactionTaskError::host)?;
    notify_after_compact(hooks, &placed, &context).await;
    // The placement itself is the host's commit above; this commit decides the
    // outcome.
    let draft = live_draft(runtime, conversation).await?;
    runtime
        .commit(
            move |tx, _task| {
                Box::pin(async move {
                    if let Some((usage, key, tokens)) = &usage {
                        record_summarization_usage(tx, usage, key, tokens)?;
                    }
                    let mut state = live_state_of(&draft.current()?);
                    remove_compaction_status(&mut state, task_id);
                    state.compaction_failure = None;
                    draft.stage(tx, live_to_object(&state))?;
                    Ok(Some(terminal_completed(placed.result())))
                })
            },
            context,
        )
        .await?;
    Ok(())
}

async fn place_summary(
    host: &dyn CompactionHost,
    hooks: &[Arc<dyn CompactionHooks>],
    runtime: &TaskRuntime,
    reason: CompactionReason,
    first_kept: EntryId,
    summary: &str,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let timestamp = u64::try_from(now(runtime)?).unwrap_or(0);
    let placement = build_placement(
        host, runtime, reason, first_kept, summary, timestamp, &context,
    )
    .await?;
    place_and_complete(host, hooks, runtime, placement, None, context).await
}

async fn select_phase(
    host: &dyn CompactionHost,
    hooks: &[Arc<dyn CompactionHooks>],
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let conversation = runtime.conversation_id();
    let input = compaction_input(task)?;
    let agent = host.agent(conversation).ok_or_else(|| {
        CompactionTaskError::host(format!("Conversation {conversation} has no agent"))
    })?;
    let Some(reference) = agent.model.clone() else {
        return fail_no_model(runtime, None, context).await;
    };
    let Some(model) = host.model(&reference) else {
        return fail_no_model(runtime, Some(&reference), context).await;
    };
    let settings = host.settings();
    let policy = compaction_policy(&settings, &reference);
    let view = host
        .context(conversation, None, context.clone())
        .await
        .map_err(CompactionTaskError::host)?;
    let Some(cut) = select_cut(&view, policy.keep_recent_tokens) else {
        return complete(runtime, context).await;
    };
    let Some(first_entry) = view.entries.get(cut) else {
        return complete(runtime, context).await;
    };
    let first_kept = first_entry.id;
    let mut decision = None;
    if !hooks.is_empty() {
        let draft = CompactionDraft {
            reason: input.reason,
            entries: view.entries.iter().take(cut).cloned().collect(),
            messages: summarized_messages(&view, cut),
            first_kept,
            instructions: input.instructions.clone(),
        };
        for hook in hooks {
            if decision.is_some() {
                continue;
            }
            match hook.before_compact(&draft, &context).await {
                Ok(found) => decision = found,
                Err(error) => {
                    let _ = runtime.report(format!("Compaction hook failed: {error}"));
                }
            }
        }
    }
    match decision {
        Some(CompactionDecision::Decline) => complete(runtime, context).await,
        Some(CompactionDecision::Supply(summary)) => {
            place_summary(
                host,
                hooks,
                runtime,
                input.reason,
                first_kept,
                &summary,
                context,
            )
            .await
        }
        None => {
            let tail = view
                .entries
                .iter()
                .map(|entry| entry.id)
                .fold(first_kept, |tail, id| tail.max(id));
            let request = SummaryRequest {
                attempt: 1,
                model: reference,
                thinking_level: agent.thinking_level,
                stream_options: settings.stream.clone(),
                max_tokens: summary_max_tokens(policy.reserve_tokens, model.max_tokens),
                tail,
                first_kept,
            };
            let checkpoint = CompactionCheckpoint::Summarize(request);
            commit_next(
                runtime,
                context,
                NextTaskState::Running {
                    checkpoint: checkpoint.to_value(),
                },
            )
            .await
        }
    }
}

/// The two-message summarization request (spec §8.7).
fn completion_messages(
    view: &ContextView,
    cut: usize,
    instructions: Option<&str>,
    now: u64,
) -> Vec<Message> {
    vec![
        Message::System(UserMessage {
            role: UserRole::System,
            content: vec![ContentBlock::Text(TextContent {
                text: SUMMARIZATION_SYSTEM_PROMPT.to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: now,
        }),
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: summary_prompt(&summarized_messages(view, cut), instructions),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: now,
        }),
    ]
}

/// Index of `first_kept` in `view.entries`.
///
/// Upstream `findIndex` returns -1 when the entry is gone, and JavaScript's
/// `slice(0, -1)` then drops the last entry; that quirk is mirrored.
fn cut_at(view: &ContextView, first_kept: EntryId) -> usize {
    view.entries
        .iter()
        .position(|entry| entry.id == first_kept)
        .unwrap_or_else(|| view.entries.len().saturating_sub(1))
}

async fn summarize_phase(
    host: &dyn CompactionHost,
    hooks: &[Arc<dyn CompactionHooks>],
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let conversation = runtime.conversation_id();
    let task_id = runtime.task_id();
    let input = compaction_input(task)?;
    let checkpoint = checkpoint_of(task)?;
    // The dispatcher only routes `phase: "summarize"` here, so a checkpoint
    // that parsed to any other shape is a stored-state inconsistency, not a
    // reachable runtime condition.
    let Some(request) = checkpoint.request().cloned() else {
        return Err(CompactionTaskError::InvalidCheckpoint {
            task: task_id,
            detail: "summarize phase reached without a pinned request".to_owned(),
        });
    };
    let Some(_model) = host.model(&request.model) else {
        return fail_no_model(runtime, Some(&request.model), context).await;
    };
    // The context at `tail` is immutable, so this is the range `select` chose.
    let view = host
        .context(conversation, Some(request.tail), context.clone())
        .await
        .map_err(CompactionTaskError::host)?;
    let cut = cut_at(&view, request.first_kept);
    let timestamp = u64::try_from(now(runtime)?).unwrap_or(0);
    let messages = completion_messages(&view, cut, input.instructions.as_deref(), timestamp);
    let completion = CompactionCompletion {
        conversation_id: conversation,
        model: request.model.clone(),
        messages,
        stream_options: summary_request_options(&request.stream_options),
        max_tokens: request.max_tokens,
        thinking_level: request.thinking_level,
        signal: runtime.signal(),
    };
    let message = host
        .complete(completion, context.clone())
        .await
        .map_err(CompactionTaskError::host)?;
    // An abort mark or close: the abort invocation, or the reopened task,
    // handles the committed state. Upstream throws here (`throwIfAborted`);
    // the scheduler's abort rule ends the invocation without a fault.
    if runtime.signal().is_aborted() {
        return Err(CompactionTaskError::Aborted { task: task_id });
    }
    let settings = host.settings();
    let until = retry_until(&message, &settings.retry, request.attempt, now(runtime)?);
    let summary = summary_text(&message);
    let usage_key = format!("{}/{}", message.provider, message.model);
    let usage_tokens = message.usage;
    let usage = usage_draft(runtime, conversation).await?;

    match (summary, until) {
        (Some(summary), _) => {
            let placement = build_placement(
                host,
                runtime,
                input.reason,
                request.first_kept,
                &summary,
                timestamp,
                &context,
            )
            .await?;
            place_and_complete(
                host,
                hooks,
                runtime,
                placement,
                Some((usage, usage_key, usage_tokens)),
                context,
            )
            .await
        }
        (None, Some(until)) => {
            let checkpoint = CompactionCheckpoint::Retry { request, until };
            let error = message.error_message.clone().unwrap_or_default();
            let live = live_draft(runtime, conversation).await?;
            runtime
                .commit(
                    move |tx, _task| {
                        Box::pin(async move {
                            record_summarization_usage(tx, &usage, &usage_key, &usage_tokens)?;
                            let mut state = live_state_of(&live.current()?);
                            if let Some(status) = compaction_status_mut(&mut state, task_id) {
                                status.retry = Some(RetryStatus { at: until, error });
                            }
                            live.stage(tx, live_to_object(&state))?;
                            Ok(Some(NextTaskState::Sleeping {
                                checkpoint: checkpoint.to_value(),
                                until,
                            }))
                        })
                    },
                    context,
                )
                .await?;
            Ok(())
        }
        (None, None) => {
            let text = summary_failure(&message);
            let live = live_draft(runtime, conversation).await?;
            runtime
                .commit(
                    move |tx, _task| {
                        Box::pin(async move {
                            record_summarization_usage(tx, &usage, &usage_key, &usage_tokens)?;
                            let mut state = live_state_of(&live.current()?);
                            fail_compaction_status(&mut state, task_id, &text);
                            live.stage(tx, live_to_object(&state))?;
                            Ok(Some(terminal_failed(text, "model_error")))
                        })
                    },
                    context,
                )
                .await?;
            Ok(())
        }
    }
}

/// Add one summarization attempt's usage to `pi.usage` (spec §8.6). Failed and
/// aborted attempts count too.
fn record_summarization_usage(
    tx: &mut Transaction,
    usage: &DocumentDraft,
    key: &str,
    tokens: &Usage,
) -> Result<(), TaskRuntimeError> {
    let mut state = usage_state_of(&usage.current()?);
    usage_add(&mut state, UsageScope::Models, key, tokens);
    usage.stage(tx, usage_object(&state))
}

async fn retry_phase(
    _host: &dyn CompactionHost,
    task: &RunningTask,
    runtime: &TaskRuntime,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let task_id = runtime.task_id();
    let checkpoint = checkpoint_of(task)?;
    // The dispatcher only routes `phase: "retry"` here, so a parsed
    // non-retry checkpoint is a stored-state inconsistency, not a reachable
    // runtime condition.
    let CompactionCheckpoint::Retry { request, until } = checkpoint else {
        return Err(CompactionTaskError::InvalidCheckpoint {
            task: task_id,
            detail: "retry phase reached without a retry checkpoint".to_owned(),
        });
    };
    if until > now(runtime)? {
        // An early wake (or a record of an earlier version) sleeps again.
        runtime
            .sleep_until(until, task.checkpoint.clone(), context)
            .await?;
        return Ok(());
    }
    let attempt = request.attempt.saturating_add(1);
    let next = CompactionCheckpoint::Summarize(SummaryRequest { attempt, ..request });
    let live = live_draft(runtime, runtime.conversation_id()).await?;
    runtime
        .commit(
            move |tx, _task| {
                Box::pin(async move {
                    let mut state = live_state_of(&live.current()?);
                    if let Some(status) = compaction_status_mut(&mut state, task_id) {
                        status.attempt = attempt;
                        status.retry = None;
                    }
                    live.stage(tx, live_to_object(&state))?;
                    Ok(Some(NextTaskState::Running {
                        checkpoint: next.to_value(),
                    }))
                })
            },
            context,
        )
        .await?;
    Ok(())
}

async fn abort_phase(
    _task: &RunningTask,
    runtime: &TaskRuntime,
    context: Context,
) -> Result<(), CompactionTaskError> {
    let task_id = runtime.task_id();
    let live = live_draft(runtime, runtime.conversation_id()).await?;
    runtime
        .commit(
            move |tx, _task| {
                Box::pin(async move {
                    let mut state = live_state_of(&live.current()?);
                    remove_compaction_status(&mut state, task_id);
                    live.stage(tx, live_to_object(&state))?;
                    Ok(Some(NextTaskState::Terminal {
                        outcome: TaskOutcome::Aborted {
                            reason: None,
                            result: None,
                        },
                    }))
                })
            },
            context,
        )
        .await?;
    Ok(())
}
