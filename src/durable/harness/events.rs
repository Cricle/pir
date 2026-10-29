//! Coding-agent-shaped events over one conversation's committed publications
//! (spec §9.4).
//!
//! Port of upstream `packages/durable/src/harness/events.ts`: an experimental
//! adapter that translates one conversation's commits into
//! `AgentSessionEvent`-shaped batches. It owns no tracker and no persistence,
//! covers exactly one conversation (not its owned subtree), and emits an event
//! only after the commit that makes it true.
//!
//! # Two halves
//!
//! * [`translate`] is the pure translation: one publication (plus the mount's
//!   operations and the revisions before and after it) becomes one ordered
//!   event batch. Entries, tasks and submissions come from the commit itself;
//!   the in-flight partial, tool slots, compactions and the built-in documents
//!   come from the revisions. [`snapshot_of`] is the total state a consumer
//!   starts from.
//! * [`watch_events`] attaches an [`EventsStream`]: the [`SnapshotEvent`] at
//!   attachment plus a serialized, bounded watch of later batches. Past
//!   [`MAX_PENDING_EVENT_BATCHES`] undelivered batches the backlog is replaced
//!   by one fresh snapshot of the newest state; the stream converges but does
//!   not promise every transition (`Submission`'s wait reports exact outcomes).
//!
//! # The mount
//!
//! Upstream builds on `conversationViews(harness)`; here the mount is
//! [`ConversationViews`], opened over the harness's [`Session`]. `watch_events`
//! takes the mount directly:
//!
//! ```ignore
//! let views = Arc::new(ConversationViews::open(harness.session().clone()).await?);
//! let stream = watch_events(&views, conversation.id(), context).await?;
//! ```
//!
//! # Rust deviations (deliberate, recorded)
//!
//! * **Messages are committed JSON.** A model message is carried as the JSON
//!   the document and entry records store (`EntryRecord::model` is
//!   `Vec<Value>` here; upstream's is `Message[]`). Decode with
//!   `serde_json::from_value::<Message>` when a typed value is wanted.
//! * **The listener is synchronous.** Delivery never nests and is scheduled
//!   with `tokio::spawn` (upstream schedules a microtask); a host outside a
//!   runtime can call [`EventsStream::drain`] explicitly.
//! * **Change detection is by value.** Upstream compares document and message
//!   object identity; committed values are immutable, so value inequality is
//!   the equivalent test. The stream reads the mount's operations first: a
//!   batch that cannot touch the in-flight message proves it unchanged without
//!   a value comparison, and only a batch that cannot tell falls back to one.
//! * **The `held` seed is explicit.** Upstream scans `completing` generations on
//!   the Session line inside the attachment. [`ConversationViews::attach`]'s
//!   factory is synchronous and has no storage handle, so the seed is a
//!   parameter: [`held_generations`] reads it from a [`Session`], and
//!   [`watch_events_with_held`] takes it. [`watch_events`] starts empty, which
//!   can report one extra `turn_end` for a generation that was already
//!   `completing` at attachment and later settles.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ai::types::{AssistantMessage, ContentBlock, Message, Usage};
use crate::durable::context::Context;
use crate::durable::delta::{Op, Seg};
use crate::durable::errors::StorageError;
use crate::durable::harness::agent::AgentState;
use crate::durable::harness::documents::AGENT_KIND;
use crate::durable::harness::inbox::{InboxItem, InboxState, InputMode, INBOX_KIND};
use crate::durable::harness::live::{
    live_from_object, CompactionFailure, CompactionStatus, GenerationState, LiveState,
    NestedToolSlot, SlotProgress, SlotStatus, ToolDiagnostic, ToolSlot, LIVE_KIND,
};
use crate::durable::harness::usage::{UsageState, USAGE_KIND};
use crate::durable::harness::util::scan_all;
use crate::durable::harness::view::{ConversationView, ConversationViews, ViewObserver};
use crate::durable::ids::{ConversationId, SubmissionId, TaskId};
use crate::durable::session::facade::Session;
use crate::durable::session::observation::{WatchEnd, MAX_PENDING_WATCH_FRAMES};
use crate::durable::session::publication::{CommitChange, CommitPublication, TableCommitChange};
use crate::durable::types::{
    DocumentCommitChange, EntryRecord, JsonObject, SubmissionRecord, TaskOutcome, TaskQuery,
    TaskRecord, TaskState, TaskStatus,
};

/// The task kind whose created/completed state bounds a turn.
pub const GENERATION_TASK_KIND: &str = "pi.generation";

/// The task-scoped document family holding a nested call's result (upstream
/// `NestedCallDoc`).
pub const NESTED_CALL_KIND: &str = "pi.tool.nested-call";

/// Undelivered event batches retained before the backlog is replaced by one
/// snapshot (spec §9.4).
pub const MAX_PENDING_EVENT_BATCHES: usize = MAX_PENDING_WATCH_FRAMES;

/// The path of the in-flight assistant message inside a view's operations
/// (upstream `PARTIAL_PATH`).
const PARTIAL_PATH: [&str; 4] = ["docs", "pi.live", "generation", "message"];

// ─── Message changes ────────────────────────────────────────────────────────

/// One change to the in-flight assistant message, relative to that message.
///
/// The wire tags are exactly upstream's (`text_start`, `toolcall_delta`, ...).
///
/// `Message` carries a whole decoded assistant message and is much larger than
/// the delta variants; boxing it would change every construction and match site
/// without changing the wire shape, and a batch holds a handful of changes at
/// most.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum MessageChange {
    /// A text block was inserted at `content_index`.
    TextStart {
        /// Index of the inserted block.
        content_index: usize,
        /// The inserted block.
        block: ContentBlock,
    },
    /// A thinking block was inserted at `content_index`.
    ThinkingStart {
        /// Index of the inserted block.
        content_index: usize,
        /// The inserted block.
        block: ContentBlock,
    },
    /// A block that is neither text nor thinking was inserted at
    /// `content_index` (upstream's `toolcall_start`).
    ToolcallStart {
        /// Index of the inserted block.
        content_index: usize,
        /// The inserted block.
        block: ContentBlock,
    },
    /// Text was appended to the block's `text`.
    TextDelta {
        /// Index of the block.
        content_index: usize,
        /// The appended text.
        delta: String,
    },
    /// Thinking was appended to the block's `thinking`.
    ThinkingDelta {
        /// Index of the block.
        content_index: usize,
        /// The appended text.
        delta: String,
    },
    /// Text was appended to a string inside a tool call's `arguments`.
    ToolcallDelta {
        /// Index of the block.
        content_index: usize,
        /// Path relative to `arguments`.
        path: Vec<Seg>,
        /// The appended text.
        delta: String,
    },
    /// The block changed in a way no delta describes; `block` is its current
    /// value and already holds the batch's later changes to it.
    Block {
        /// Index of the block.
        content_index: usize,
        /// The block's current value.
        block: ContentBlock,
    },
    /// The whole message was replaced (or a change outside its content was
    /// published).
    Message {
        /// The current message.
        message: AssistantMessage,
    },
}

// ─── Tool vocabulary ────────────────────────────────────────────────────────

/// The identifying fields of one tool call's events.
///
/// `tool_call_id` is the provider's ID for a model-issued call and
/// `<parent call ID>/<key>` for a nested one; consumers match it and never
/// parse it. `task_id` is the call's tool task, absent for a call that never
/// got one. A nested call also names its caller.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEventCall {
    /// The call's ID in the transcript and in tool events.
    pub tool_call_id: String,
    /// The tool name.
    pub tool_name: String,
    /// The call's tool task, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// The call that made this nested call, by call ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    /// The call that made this nested call, by tool task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
}

/// A running tool slot's retained-output change: a front trim and then an
/// append of the retained window, or its replacement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolOutputChange {
    /// The retained window was replaced outright.
    Set {
        /// The new window.
        set: String,
    },
    /// A front trim and then an append of the retained window.
    Window {
        /// Characters (UTF-16 code units) dropped from the front.
        #[serde(rename = "trimStart", default, skip_serializing_if = "Option::is_none")]
        trim_start: Option<usize>,
        /// Text appended after the trim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        append: Option<String>,
    },
}

/// A field that is absent, explicitly `null` (removed), or a value.
///
/// Upstream sends `details: null` and `diagnostics: []` when a safe replay
/// clears a running slot's progress, and omits the key otherwise.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Tri<T> {
    /// The field is not part of the event.
    #[default]
    Absent,
    /// The field is present as JSON `null`.
    Null,
    /// The field is present with a value.
    Value(T),
}

impl<T> Tri<T> {
    /// Whether the field is absent from the event.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

fn tri_is_absent<T>(value: &Tri<T>) -> bool {
    value.is_absent()
}

impl<T: Serialize> Serialize for Tri<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Tri<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Option::<T>::deserialize(deserializer)? {
            Some(value) => Ok(Self::Value(value)),
            None => Ok(Self::Null),
        }
    }
}

/// What one `tool_execution_update` carries beside its call identity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolUpdate {
    /// Retained-output change, when the output changed.
    pub output: Option<ToolOutputChange>,
    /// `details` change, when it changed.
    pub details: Tri<Value>,
    /// `diagnostics` change, when it changed.
    pub diagnostics: Tri<Vec<ToolDiagnostic>>,
}

impl ToolUpdate {
    /// Whether no field changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.output.is_none() && self.details.is_absent() && self.diagnostics.is_absent()
    }
}

/// Result of a nested call, as `execute_tool()` returns it (upstream
/// `NestedToolExecutionResult`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolExecutionResult {
    /// The nested call's tool task.
    pub task_id: TaskId,
    /// The tool's value for programs, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    /// Whether the call failed.
    pub is_error: bool,
    /// The last `details()` value, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Diagnostics of the result.
    #[serde(default)]
    pub diagnostics: Vec<ToolDiagnostic>,
    /// Spend of the execution itself, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Execution time of the attempt that produced the result, in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// The stored value of one `pi.tool.nested-call` document (upstream
/// `NestedCallState`): its task, and once it ended, its result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedCallState {
    /// The nested call's tool task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Exactly what `execute_tool()` returns; absent while running, and for a
    /// call the scheduler faulted or orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<NestedToolExecutionResult>,
}

// ─── Snapshot ───────────────────────────────────────────────────────────────

/// How a queued item waits (upstream `InboxItem["mode"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueuedMode {
    /// Steering input.
    Steer,
    /// Follow-up input.
    FollowUp,
    /// A passive entry write.
    Write,
}

/// One queued submission: its ID and how it waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedItem {
    /// The waiting submission.
    pub id: SubmissionId,
    /// How it waits.
    pub mode: QueuedMode,
}

impl QueuedItem {
    fn of(item: &InboxItem) -> Self {
        let mode = match item {
            InboxItem::Input {
                mode: InputMode::Steer,
                ..
            } => QueuedMode::Steer,
            InboxItem::Input {
                mode: InputMode::FollowUp,
                ..
            } => QueuedMode::FollowUp,
            InboxItem::Write { .. } => QueuedMode::Write,
        };
        Self {
            id: item.id(),
            mode,
        }
    }
}

/// The run a snapshot shows: the inputs it settles.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInputs {
    /// Inputs the run task settles.
    pub inputs: Vec<SubmissionId>,
}

/// The payload of the `snapshot` event: everything a consumer holds before
/// applying later events.
///
/// Serialized inside [`AgentEvent::Snapshot`] it carries the `type: "snapshot"`
/// tag from the enum variant; [`EventsStream::snapshot_event`] is the tagged
/// form of the attachment snapshot.
///
/// `generation` is [`GenerationState`] as stored in `pi.live`: `message` is the
/// committed partial's JSON (upstream types it `AssistantMessage`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    /// The conversation's raw active entries.
    pub entries: Vec<EntryRecord>,
    /// The run control, while busy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunInputs>,
    /// The current generation attempt, its partial, retry backoff or deferred
    /// poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationState>,
    /// The current tool round's slots.
    pub tools: Vec<ToolSlot>,
    /// Nested calls of running tools.
    pub nested_tools: Vec<NestedToolSlot>,
    /// Live compactions.
    pub compactions: Vec<CompactionStatus>,
    /// The newest compaction failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<CompactionFailure>,
    /// Waiting submissions.
    pub inbox: Vec<QueuedItem>,
    /// `pi.agent`; `{}` when absent.
    pub agent: AgentState,
    /// `pi.usage`.
    pub usage: UsageState,
}

// ─── Agent events ───────────────────────────────────────────────────────────

/// Experimental agent event, shaped like the coding agent's session events
/// (spec §9.4).
///
/// Every event is relative to the snapshot and the events before it, never to
/// the durable document layout. `snapshot` replaces everything a consumer
/// holds; every other event applies on top of it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    /// The total state at attachment; replaces everything before it.
    Snapshot(SnapshotEvent),
    /// The run appeared, or was replaced by a successor with a different first
    /// input.
    RunStart {
        /// Inputs the run settles.
        inputs: Vec<SubmissionId>,
    },
    /// The previous run ended.
    RunEnd {
        /// Inputs the previous run settled.
        inputs: Vec<SubmissionId>,
    },
    /// A `pi.generation` task was created.
    TurnStart,
    /// A generation's outcome was committed: at `completing` or at terminal,
    /// whichever comes first.
    TurnEnd,
    /// A message started: the first committed partial of an attempt, or, for an
    /// entry with no committed partial, the entry's first model message.
    MessageStart {
        /// The message's committed JSON.
        message: Message,
    },
    /// The in-flight assistant message changed; `usage` is the partial's
    /// current usage, as the coding agent's JSON mode reports it.
    MessageUpdate {
        /// The partial's current usage.
        usage: Usage,
        /// The changes, in operation order.
        changes: Vec<MessageChange>,
    },
    /// A message entry was appended.
    MessageEnd {
        /// The appended entry.
        entry: EntryRecord,
    },
    /// A tool call started running.
    ToolExecutionStart {
        /// The call's identity.
        #[serde(flatten)]
        call: ToolEventCall,
        /// The arguments it runs with.
        args: JsonObject,
    },
    /// A running tool call changed its retained output, details or diagnostics.
    ToolExecutionUpdate {
        /// The call's identity.
        #[serde(flatten)]
        call: ToolEventCall,
        /// Retained-output change, when it changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutputChange>,
        /// `details` change, when it changed; removed details send `null`.
        #[serde(default, skip_serializing_if = "tri_is_absent")]
        details: Tri<Value>,
        /// `diagnostics` change, when it changed; removed diagnostics send `[]`.
        #[serde(default, skip_serializing_if = "tri_is_absent")]
        diagnostics: Tri<Vec<ToolDiagnostic>>,
    },
    /// A tool call ended.
    ///
    /// A model-issued call ends with its result `entry`; a nested call with its
    /// `result`. Both are absent when the tool task faulted or was orphaned, or
    /// when the call's slot left `pi.live` unfinished.
    ToolExecutionEnd {
        /// The call's identity.
        #[serde(flatten)]
        call: ToolEventCall,
        /// The result entry of a model-issued call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<EntryRecord>,
        /// The result of a nested call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<NestedToolExecutionResult>,
    },
    /// `pi.inbox` changed (a retired document reads as empty).
    InboxUpdate {
        /// The waiting submissions.
        items: Vec<QueuedItem>,
    },
    /// A submission record was created or replaced.
    Submission {
        /// The record.
        record: SubmissionRecord,
    },
    /// `pi.live.generation` gained retry backoff.
    AutoRetryStart {
        /// Attempt number.
        attempt: i64,
        /// Harness-clock milliseconds at which the attempt becomes eligible.
        at: i64,
        /// The error that caused the backoff.
        error_message: String,
    },
    /// Retry backoff ended (the next attempt is due, or the generation ended).
    AutoRetryEnd {
        /// The attempt that was backing off.
        attempt: i64,
    },
    /// A deferred provider response moved its poll time.
    DeferredPoll {
        /// Harness-clock milliseconds of the next poll.
        poll_at: i64,
    },
    /// An entry without model messages was appended.
    EntryAppended {
        /// The appended entry.
        entry: EntryRecord,
    },
    /// `pi.agent` changed (a retired document reads as `{}`).
    AgentChanged {
        /// The current state.
        agent: AgentState,
    },
    /// `pi.usage` changed.
    UsageChanged {
        /// The current ledger.
        usage: UsageState,
    },
    /// A conversation task settled `faulted` or `orphaned`.
    TaskFailed {
        /// The task.
        task_id: TaskId,
        /// The task definition kind.
        kind: String,
        /// The failure text.
        message: String,
    },
    /// A compaction status appeared in `pi.live.compactions`.
    CompactionStart {
        /// The compaction task.
        task_id: TaskId,
        /// `manual`, `threshold` or `overflow`.
        reason: String,
        /// Whether a generation waits for it.
        blocking: bool,
    },
    /// A compaction status disappeared from `pi.live.compactions`.
    CompactionEnd {
        /// The compaction task.
        task_id: TaskId,
        /// `manual`, `threshold` or `overflow`.
        reason: String,
        /// Set when the same commit recorded the task in
        /// `pi.live.compactionFailure`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl AgentEvent {
    /// The wire `type` discriminator.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Snapshot(_) => "snapshot",
            Self::RunStart { .. } => "run_start",
            Self::RunEnd { .. } => "run_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
            Self::InboxUpdate { .. } => "inbox_update",
            Self::Submission { .. } => "submission",
            Self::AutoRetryStart { .. } => "auto_retry_start",
            Self::AutoRetryEnd { .. } => "auto_retry_end",
            Self::DeferredPoll { .. } => "deferred_poll",
            Self::EntryAppended { .. } => "entry_appended",
            Self::AgentChanged { .. } => "agent_changed",
            Self::UsageChanged { .. } => "usage_changed",
            Self::TaskFailed { .. } => "task_failed",
            Self::CompactionStart { .. } => "compaction_start",
            Self::CompactionEnd { .. } => "compaction_end",
        }
    }
}

// ─── The mounted revision ───────────────────────────────────────────────────

/// One mounted revision, narrowed to what the events read.
///
/// Mirrors spec §9.3's [`ConversationView`]: the raw active entries plus the
/// built-in documents (`pi.live`, `pi.inbox`, `pi.agent`, `pi.usage`) decoded
/// once per revision (upstream's `parts(view)`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewPort {
    /// Raw active entries, as `ContextView::entries`.
    pub entries: Vec<EntryRecord>,
    /// `pi.live`; the default state when absent.
    pub live: LiveState,
    /// `pi.inbox`; absent when the document is absent.
    pub inbox: Option<InboxState>,
    /// `pi.agent`; absent when the document is absent.
    pub agent: Option<AgentState>,
    /// `pi.usage`; absent when the document is absent.
    pub usage: Option<UsageState>,
}

impl ViewPort {
    /// Narrow one mounted revision (upstream `parts()`).
    #[must_use]
    pub fn from_view(view: &ConversationView) -> Self {
        Self::from_docs(view.entries.to_vec(), |kind| {
            view.docs.get(kind).map(|value| (**value).clone())
        })
    }

    /// Build from raw entries and a lookup of the mounted built-in documents,
    /// keyed by kind (`pi.live`, `pi.inbox`, `pi.agent`, `pi.usage`).
    #[must_use]
    pub fn from_docs<F>(entries: Vec<EntryRecord>, doc: F) -> Self
    where
        F: Fn(&str) -> Option<JsonObject>,
    {
        Self {
            entries,
            live: doc(LIVE_KIND).map_or_else(LiveState::default, |value| live_from_object(&value)),
            inbox: doc(INBOX_KIND).and_then(decode_document),
            agent: doc(AGENT_KIND).and_then(decode_document),
            usage: doc(USAGE_KIND).and_then(decode_document),
        }
    }
}

fn decode_document<T: DeserializeOwned>(value: JsonObject) -> Option<T> {
    serde_json::from_value(Value::Object(value)).ok()
}

/// The total state at attachment, from one mounted revision.
#[must_use]
pub fn snapshot_of(view: &ViewPort) -> SnapshotEvent {
    snapshot_with_entries(view, view.entries.clone())
}

/// [`snapshot_of`] with the entries supplied separately.
///
/// The stream caches the newest entries apart from the docs-only ports it
/// translates, so an overflow snapshot takes them without a port that owns
/// them.
fn snapshot_with_entries(view: &ViewPort, entries: Vec<EntryRecord>) -> SnapshotEvent {
    SnapshotEvent {
        entries,
        run: view.live.run.as_ref().map(|run| RunInputs {
            inputs: run.inputs.clone(),
        }),
        generation: view.live.generation.clone(),
        tools: view.live.tools.clone().unwrap_or_default(),
        nested_tools: view.live.nested_tools.clone().unwrap_or_default(),
        compactions: view.live.compactions.clone().unwrap_or_default(),
        compaction_failure: view.live.compaction_failure.clone(),
        inbox: queued(view.inbox.as_ref()),
        agent: view.agent.clone().unwrap_or_default(),
        usage: view.usage.clone().unwrap_or_default(),
    }
}

// ─── Translation ────────────────────────────────────────────────────────────

/// One mounted tool slot, model-issued or nested.
#[derive(Clone, Copy)]
enum SlotRef<'a> {
    Tool(&'a ToolSlot),
    Nested(&'a NestedToolSlot),
}

impl<'a> SlotRef<'a> {
    fn progress(self) -> &'a SlotProgress {
        match self {
            Self::Tool(slot) => &slot.progress,
            Self::Nested(slot) => &slot.progress,
        }
    }

    fn status(self) -> SlotStatus {
        self.progress().status
    }
}

/// The identifying fields of a slot's tool events.
fn call_of(slot: SlotRef<'_>) -> ToolEventCall {
    let progress = slot.progress();
    match slot {
        SlotRef::Tool(slot) => ToolEventCall {
            tool_call_id: progress.call_id.clone(),
            tool_name: progress.name.clone(),
            task_id: slot.task_id,
            parent_tool_call_id: None,
            parent_task_id: None,
        },
        SlotRef::Nested(slot) => ToolEventCall {
            tool_call_id: progress.call_id.clone(),
            tool_name: progress.name.clone(),
            task_id: Some(slot.task_id),
            parent_tool_call_id: Some(slot.parent_call_id.clone()),
            parent_task_id: Some(slot.parent_task_id),
        },
    }
}

/// A model-issued tool end staged until its result entry is emitted.
struct ToolEnd {
    call: ToolEventCall,
    entry: Option<EntryRecord>,
}

/// Whether one publication carries anything [`translate`] derives from the
/// commit itself rather than from the mount's operation batch.
///
/// With no mount operations and no conversation entry, task or submission, the
/// translation is empty; an observer can skip decoding the mount's documents
/// entirely (exactly the early return at the top of [`translate`]).
fn translates_commit(publication: &CommitPublication, conversation_id: ConversationId) -> bool {
    publication.changes.iter().any(|change| match change {
        CommitChange::Table(TableCommitChange::Entry(entry)) => {
            entry.conversation_id == conversation_id
        }
        CommitChange::Table(TableCommitChange::Task(task)) => {
            task.conversation_id == conversation_id
        }
        CommitChange::Table(TableCommitChange::Submission(submission)) => {
            submission.conversation_id == conversation_id
        }
        _ => false,
    })
}

/// Whether the commit changed this conversation's entries.
///
/// The mount's `entries` advances only for entry changes of its conversation,
/// so this is the condition under which the stream's cached entries go stale.
fn changes_entries(publication: &CommitPublication, conversation_id: ConversationId) -> bool {
    publication.changes.iter().any(|change| {
        matches!(
            change,
            CommitChange::Table(TableCommitChange::Entry(entry))
                if entry.conversation_id == conversation_id
        )
    })
}

/// Every event one publication causes, in the order of spec §9.4.
///
/// `held` is the stream's set of generations whose held outcome already ended
/// their turn; it persists across calls. [`translate`] never mutates any other
/// state of the caller.
#[must_use]
pub fn translate(
    conversation_id: ConversationId,
    before: &ViewPort,
    after: &ViewPort,
    view_ops: &[Op],
    publication: &CommitPublication,
    held: &mut HashSet<TaskId>,
) -> Vec<AgentEvent> {
    let mut entries: Vec<EntryRecord> = Vec::new();
    let mut task_changes: Vec<TaskRecord> = Vec::new();
    let mut submissions: Vec<SubmissionRecord> = Vec::new();
    // Nested results this commit stored, by nested task ID; read now, since
    // the caller's documents may retire next.
    let mut nested_results: HashMap<TaskId, NestedToolExecutionResult> = HashMap::new();

    for change in &publication.changes {
        match change {
            CommitChange::Document(DocumentCommitChange::Document {
                record,
                conversation_id: Some(owner),
                value: Some(value),
                ..
            }) if record.kind == NESTED_CALL_KIND && *owner == conversation_id => {
                if let Ok(state) =
                    serde_json::from_value::<NestedCallState>(Value::Object(value.clone()))
                {
                    if let (Some(task_id), Some(result)) = (state.task_id, state.result) {
                        nested_results.insert(task_id, result);
                    }
                }
            }
            CommitChange::Table(TableCommitChange::Entry(entry))
                if entry.conversation_id == conversation_id =>
            {
                entries.push(entry.clone());
            }
            CommitChange::Table(TableCommitChange::Task(task))
                if task.conversation_id == conversation_id =>
            {
                match task_changes.iter_mut().find(|known| known.id == task.id) {
                    Some(known) => *known = task.clone(),
                    None => task_changes.push(task.clone()),
                }
            }
            CommitChange::Table(TableCommitChange::Submission(submission))
                if submission.conversation_id == conversation_id =>
            {
                submissions.push(submission.clone());
            }
            _ => {}
        }
    }

    if view_ops.is_empty()
        && entries.is_empty()
        && task_changes.is_empty()
        && submissions.is_empty()
    {
        return Vec::new();
    }

    // Entries are appended in ID order; submission records are published in the
    // order the commit first touched them, and events report them by ID.
    submissions.sort_by_key(|record| record.id);

    let mut events: Vec<AgentEvent> = Vec::new();

    let tools_before = before.live.tools.as_deref().unwrap_or(&[]);
    let tools = after.live.tools.as_deref().unwrap_or(&[]);
    let nested_before = before.live.nested_tools.as_deref().unwrap_or(&[]);
    let nested = after.live.nested_tools.as_deref().unwrap_or(&[]);

    // Progress: tool starts.
    for slot in tools
        .iter()
        .map(SlotRef::Tool)
        .chain(nested.iter().map(SlotRef::Nested))
    {
        let previous = match slot {
            SlotRef::Tool(current) => tools_before
                .iter()
                .find(|previous| previous.progress.call_id == current.progress.call_id)
                .map(SlotRef::Tool),
            SlotRef::Nested(current) => nested_before
                .iter()
                .find(|previous| previous.task_id == current.task_id)
                .map(SlotRef::Nested),
        };
        if slot.status() != SlotStatus::Running
            || previous.is_some_and(|previous| previous.status() == SlotStatus::Running)
        {
            continue;
        }
        // A nested slot carries the arguments the call runs with; a
        // model-issued call's are in its intent checkpoint, read in the same
        // commit.
        let args = match slot {
            SlotRef::Nested(current) => current.arguments.clone(),
            SlotRef::Tool(current) => current
                .task_id
                .and_then(|task_id| task_changes.iter().find(|task| task.id == task_id))
                .and_then(|task| checkpoint_arguments(&task.state))
                .unwrap_or_default(),
        };
        events.push(AgentEvent::ToolExecutionStart {
            call: call_of(slot),
            args,
        });
    }

    // Progress: the in-flight message.
    let partial_before = before
        .live
        .generation
        .as_ref()
        .and_then(|generation| generation.message.as_ref());
    let partial = after
        .live
        .generation
        .as_ref()
        .and_then(|generation| generation.message.as_ref());
    if let Some(partial) = partial {
        match partial_before {
            None => {
                if let Some(message) = assistant_of(partial) {
                    events.push(AgentEvent::MessageStart {
                        message: Message::Assistant(message),
                    });
                }
            }
            Some(previous) => {
                // The operations are the exact batch that produced `after`:
                // when they do not touch the in-flight message it is unchanged,
                // and decoding it (or comparing whole values) is unnecessary.
                // A batch that cannot tell keeps the value comparison.
                let changed = partial_changed(view_ops).unwrap_or_else(|| previous != partial);
                if changed {
                    if let Some(message) = assistant_of(partial) {
                        events.push(AgentEvent::MessageUpdate {
                            usage: message.usage,
                            changes: message_changes(view_ops, &message),
                        });
                    }
                }
            }
        }
    }

    // Progress: updates of slots that ran before and still run.
    for (index, slot) in tools.iter().enumerate() {
        let Some(previous) = tools_before
            .iter()
            .find(|previous| previous.progress.call_id == slot.progress.call_id)
        else {
            continue;
        };
        if slot.progress.status != SlotStatus::Running
            || previous.progress.status != SlotStatus::Running
        {
            continue;
        }
        if let Some(update) =
            tool_update(view_ops, "tools", index, &slot.progress, &previous.progress)
        {
            events.push(tool_update_event(call_of(SlotRef::Tool(slot)), update));
        }
    }
    for (index, slot) in nested.iter().enumerate() {
        let Some(previous) = nested_before
            .iter()
            .find(|previous| previous.task_id == slot.task_id)
        else {
            continue;
        };
        if slot.progress.status != SlotStatus::Running
            || previous.progress.status != SlotStatus::Running
        {
            continue;
        }
        if let Some(update) = tool_update(
            view_ops,
            "nestedTools",
            index,
            &slot.progress,
            &previous.progress,
        ) {
            events.push(tool_update_event(call_of(SlotRef::Nested(slot)), update));
        }
    }

    // Progress: retry backoff and deferred polls.
    let generation = after.live.generation.as_ref();
    let generation_before = before.live.generation.as_ref();
    let retry = generation.and_then(|generation| generation.retry.as_ref());
    let retry_before = generation_before.and_then(|generation| generation.retry.as_ref());
    if let (Some(retry), None) = (retry, retry_before) {
        events.push(AgentEvent::AutoRetryStart {
            attempt: generation.map_or(0, |generation| generation.attempt),
            at: retry.at,
            error_message: retry.error.clone(),
        });
    }
    if let (None, Some(previous)) = (retry, generation_before) {
        if previous.retry.is_some() {
            events.push(AgentEvent::AutoRetryEnd {
                attempt: previous.attempt,
            });
        }
    }
    let deferred = generation.and_then(|generation| generation.deferred.as_ref());
    if let Some(deferred) = deferred {
        if generation_before
            .and_then(|generation| generation.deferred.as_ref())
            .map(|deferred| deferred.poll_at)
            != Some(deferred.poll_at)
        {
            events.push(AgentEvent::DeferredPoll {
                poll_at: deferred.poll_at,
            });
        }
    }

    // Tools that end in this commit: a slot that becomes done, one created done
    // (a call not offered), or an unfinished one that vanishes because its run
    // ended. A done slot that vanishes ended earlier.
    let mut tool_ends: Vec<ToolEnd> = Vec::new();
    for previous in tools_before {
        if previous.progress.status == SlotStatus::Done {
            continue;
        }
        match tools
            .iter()
            .find(|slot| slot.progress.call_id == previous.progress.call_id)
        {
            Some(slot) if slot.progress.status == SlotStatus::Done => {
                tool_ends.push(ToolEnd {
                    call: call_of(SlotRef::Tool(slot)),
                    entry: find_entry(&entries, slot.entry),
                });
            }
            Some(_) => {}
            None => {
                // A slot whose run ended in this commit may have had its result
                // appended with it, as for unstarted calls.
                tool_ends.push(ToolEnd {
                    call: call_of(SlotRef::Tool(previous)),
                    entry: result_of(&entries, &previous.progress.call_id).cloned(),
                });
            }
        }
    }
    for slot in tools {
        if slot.progress.status == SlotStatus::Done
            && !tools_before
                .iter()
                .any(|previous| previous.progress.call_id == slot.progress.call_id)
        {
            tool_ends.push(ToolEnd {
                call: call_of(SlotRef::Tool(slot)),
                entry: find_entry(&entries, slot.entry),
            });
        }
    }

    // Nested calls that end in this commit, the same way, children before the
    // calls that made them: the lists hold parents first, so walk them
    // backwards.
    for previous in nested_before.iter().rev() {
        if previous.progress.status == SlotStatus::Done {
            continue;
        }
        match nested.iter().find(|slot| slot.task_id == previous.task_id) {
            Some(slot) if slot.progress.status == SlotStatus::Done => {
                events.push(tool_end_event(
                    call_of(SlotRef::Nested(slot)),
                    None,
                    nested_results.get(&slot.task_id).cloned(),
                ));
            }
            None => {
                events.push(tool_end_event(
                    call_of(SlotRef::Nested(previous)),
                    None,
                    nested_results.get(&previous.task_id).cloned(),
                ));
            }
            Some(_) => {}
        }
    }
    for slot in nested.iter().rev() {
        if slot.progress.status == SlotStatus::Done
            && !nested_before
                .iter()
                .any(|previous| previous.task_id == slot.task_id)
        {
            events.push(tool_end_event(
                call_of(SlotRef::Nested(slot)),
                None,
                nested_results.get(&slot.task_id).cloned(),
            ));
        }
    }

    // Entries in append order; a tool's end directly precedes its result's
    // message, as in the coding agent.
    let mut assistant_appended = false;
    for entry in &entries {
        for end in &tool_ends {
            if end.entry.as_ref().map(|record| record.id) == Some(entry.id) {
                events.push(tool_end_event(end.call.clone(), end.entry.clone(), None));
            }
        }
        let message = entry
            .model
            .as_ref()
            .and_then(|messages| messages.first())
            .and_then(message_of);
        let Some(message) = message else {
            events.push(AgentEvent::EntryAppended {
                entry: entry.clone(),
            });
            continue;
        };
        // A streamed answer already started with its first partial.
        let assistant = matches!(message, Message::Assistant(_));
        let streamed = assistant && partial_before.is_some() && !assistant_appended;
        if assistant {
            assistant_appended = true;
        }
        if !streamed {
            events.push(AgentEvent::MessageStart { message });
        }
        events.push(AgentEvent::MessageEnd {
            entry: entry.clone(),
        });
    }
    // Ends without a result entry: a faulted or orphaned tool, or one whose run
    // ended.
    for end in tool_ends {
        if end.entry.is_none() {
            events.push(tool_end_event(end.call, None, None));
        }
    }

    // Compaction ends, task failures, then turn and run ends.
    let compactions_before = before.live.compactions.as_deref().unwrap_or(&[]);
    let compactions = after.live.compactions.as_deref().unwrap_or(&[]);
    let failure = after.live.compaction_failure.as_ref();
    for status in compactions_before {
        if !compactions
            .iter()
            .any(|current| current.task_id == status.task_id)
        {
            events.push(AgentEvent::CompactionEnd {
                task_id: status.task_id,
                reason: status.reason.clone(),
                error: failure
                    .filter(|failure| failure.task_id == status.task_id)
                    .map(|failure| failure.error.clone()),
            });
        }
    }

    // A generation's turn ends when its outcome is committed: at a `completing`
    // hold or at terminal, whichever comes first, so a successor created at the
    // hold starts after it.
    let mut turn_ended = false;
    for task in &task_changes {
        let status = task.state.status();
        if task.kind == GENERATION_TASK_KIND
            && status == TaskStatus::Completing
            && !held.contains(&task.id)
        {
            held.insert(task.id);
            turn_ended = true;
        }
        if status != TaskStatus::Terminal {
            continue;
        }
        if task.kind == GENERATION_TASK_KIND && !held.remove(&task.id) {
            turn_ended = true;
        }
        let TaskState::Terminal { outcome } = &task.state else {
            continue;
        };
        let message = match outcome {
            TaskOutcome::Faulted { error } => Some(error.message.clone()),
            TaskOutcome::Orphaned { reason } => Some(reason.clone()),
            _ => None,
        };
        if let Some(message) = message {
            events.push(AgentEvent::TaskFailed {
                task_id: task.id,
                kind: task.kind.clone(),
                message,
            });
        }
    }
    if turn_ended {
        events.push(AgentEvent::TurnEnd);
    }

    let run = after.live.run.clone();
    let run_before = before.live.run.clone();
    let run_changed = match (&run, &run_before) {
        (Some(current), Some(previous)) => current.inputs.first() != previous.inputs.first(),
        (None, None) => false,
        _ => true,
    };
    if run_changed {
        if let Some(previous) = &run_before {
            events.push(AgentEvent::RunEnd {
                inputs: previous.inputs.clone(),
            });
        }
    }

    // Submissions, document state, then what began.
    for record in &submissions {
        events.push(AgentEvent::Submission {
            record: record.clone(),
        });
    }
    if after.inbox != before.inbox {
        events.push(AgentEvent::InboxUpdate {
            items: queued(after.inbox.as_ref()),
        });
    }
    if after.agent != before.agent {
        events.push(AgentEvent::AgentChanged {
            agent: after.agent.clone().unwrap_or_default(),
        });
    }
    if after.usage != before.usage {
        events.push(AgentEvent::UsageChanged {
            usage: after.usage.clone().unwrap_or_default(),
        });
    }
    for status in compactions {
        if !compactions_before
            .iter()
            .any(|previous| previous.task_id == status.task_id)
        {
            events.push(AgentEvent::CompactionStart {
                task_id: status.task_id,
                reason: status.reason.clone(),
                blocking: status.blocking,
            });
        }
    }
    if let Some(current) = &run {
        if run_changed {
            events.push(AgentEvent::RunStart {
                inputs: current.inputs.clone(),
            });
        }
        let task_kind = task_changes
            .iter()
            .find(|task| task.id == current.task_id)
            .map(|task| task.kind.as_str());
        if run_before.as_ref().map(|previous| previous.task_id) != Some(current.task_id)
            && task_kind == Some(GENERATION_TASK_KIND)
        {
            events.push(AgentEvent::TurnStart);
        }
    }

    events
}

// ─── View operations ────────────────────────────────────────────────────────

fn tool_end_event(
    call: ToolEventCall,
    entry: Option<EntryRecord>,
    result: Option<NestedToolExecutionResult>,
) -> AgentEvent {
    AgentEvent::ToolExecutionEnd {
        call,
        entry,
        result,
    }
}

fn tool_update_event(call: ToolEventCall, update: ToolUpdate) -> AgentEvent {
    AgentEvent::ToolExecutionUpdate {
        call,
        output: update.output,
        details: update.details,
        diagnostics: update.diagnostics,
    }
}

/// The path of an operation, or `None` for a root replacement.
fn op_path(op: &Op) -> Option<&[Seg]> {
    match op {
        Op::Replace(_) => None,
        Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Truncate(path, _) => {
            Some(path)
        }
        Op::Splice { path, .. } | Op::Move { path, .. } => Some(path),
    }
}

fn seg_key(seg: &Seg) -> Option<&str> {
    match seg {
        Seg::Key(key) => Some(key.as_str()),
        Seg::Index(_) => None,
    }
}

fn starts_with_keys(path: &[Seg], keys: &[&str]) -> bool {
    keys.len() <= path.len()
        && keys
            .iter()
            .zip(path)
            .all(|(key, seg)| seg_key(seg) == Some(*key))
}

/// Whether `path` is a prefix of the partial-message path: the message or one
/// of its ancestors was replaced.
fn is_prefix_of_partial(path: &[Seg]) -> bool {
    path.len() <= PARTIAL_PATH.len()
        && path
            .iter()
            .zip(PARTIAL_PATH.iter())
            .all(|(seg, key)| seg_key(seg) == Some(*key))
}

/// Whether the in-flight message changed, read from the view operations.
///
/// `None` means the batch cannot tell (an empty batch, a root replacement, or a
/// replacement of the message or one of its ancestors): the caller must compare
/// the values. `Some(true)` is a change inside the message and `Some(false)` a
/// batch that cannot touch it, so a publication that only moves tools, run
/// control or unrelated documents neither compares nor decodes the partial.
///
/// The operations are the exact batch that produced the newer revision; a
/// caller that pairs them with an unrelated view falls back to the value
/// comparison whenever the batch touches the message's ancestors.
fn partial_changed(view_ops: &[Op]) -> Option<bool> {
    if view_ops.is_empty() {
        return None;
    }
    let mut changed = false;
    for op in view_ops {
        let path = op_path(op)?;
        if starts_with_keys(path, &PARTIAL_PATH) {
            match op {
                // Operations that change nothing.
                Op::Append(_, text) if text.is_empty() => {}
                Op::Truncate(_, 0) => {}
                Op::Splice {
                    remove: 0, items, ..
                } if items.is_empty() => {}
                // A move can be the identity permutation; compare.
                Op::Move { .. } => return None,
                _ => changed = true,
            }
        } else if is_prefix_of_partial(path) {
            // The message or one of its ancestors was replaced; compare.
            return None;
        }
    }
    Some(changed)
}

/// Translate the view operations on the in-flight message into message changes
/// (spec §9.4).
///
/// Only deltas travel, so throttled partials cause no write amplification on
/// the wire. Any change the operation vocabulary cannot express as a delta
/// sends the affected block (which already holds the batch's later changes to
/// it), or the whole message.
#[must_use]
pub fn message_changes(view_ops: &[Op], message: &AssistantMessage) -> Vec<MessageChange> {
    let whole_message = || {
        vec![MessageChange::Message {
            message: message.clone(),
        }]
    };
    let mut changes: Vec<MessageChange> = Vec::new();
    // A block sent whole already holds every later change to it in this batch.
    let mut whole: HashSet<usize> = HashSet::new();
    for op in view_ops {
        let Some(path) = op_path(op) else {
            // View operations never replace the root.
            continue;
        };
        if !starts_with_keys(path, &PARTIAL_PATH) {
            // The whole message or one of its ancestors was replaced.
            if is_prefix_of_partial(path) {
                return whole_message();
            }
            continue;
        }
        let rest = &path[PARTIAL_PATH.len()..];
        match rest.first().and_then(seg_key) {
            // The usage travels on every update; it is not a content change.
            Some("usage") => continue,
            Some("content") => {}
            _ => return whole_message(),
        }
        if rest.len() == 1 {
            // A splice that inserts blocks without removing any.
            let Op::Splice {
                index,
                remove,
                items,
                ..
            } = op
            else {
                return whole_message();
            };
            if *remove != 0 {
                return whole_message();
            }
            for (offset, value) in items.iter().enumerate() {
                let Ok(block) = serde_json::from_value::<ContentBlock>(value.clone()) else {
                    continue;
                };
                let content_index = *index + offset;
                let change = match &block {
                    ContentBlock::Text(_) => MessageChange::TextStart {
                        content_index,
                        block,
                    },
                    ContentBlock::Thinking(_) => MessageChange::ThinkingStart {
                        content_index,
                        block,
                    },
                    _ => MessageChange::ToolcallStart {
                        content_index,
                        block,
                    },
                };
                changes.push(change);
            }
            continue;
        }
        let Some(Seg::Index(content_index)) = rest.get(1) else {
            return whole_message();
        };
        let content_index = *content_index;
        if whole.contains(&content_index) {
            continue;
        }
        let field = rest.get(2).and_then(seg_key);
        if let Op::Append(_, text) = op {
            if rest.len() == 3 && matches!(field, Some("text" | "thinking")) {
                changes.push(if field == Some("text") {
                    MessageChange::TextDelta {
                        content_index,
                        delta: text.clone(),
                    }
                } else {
                    MessageChange::ThinkingDelta {
                        content_index,
                        delta: text.clone(),
                    }
                });
                continue;
            }
            if field == Some("arguments") {
                changes.push(MessageChange::ToolcallDelta {
                    content_index,
                    path: rest[3..].to_vec(),
                    delta: text.clone(),
                });
                continue;
            }
        }
        whole.insert(content_index);
        match message.content.get(content_index) {
            Some(block) => changes.push(MessageChange::Block {
                content_index,
                block: block.clone(),
            }),
            None => return whole_message(),
        }
    }
    changes
}

/// Output, details and diagnostics changes of a running slot at
/// `list[index]`, from the view operations on it.
///
/// `list` is `"tools"` or `"nestedTools"`, the `pi.live` key the slot lives in
/// (upstream passes the path segments).
#[must_use]
pub fn tool_update(
    view_ops: &[Op],
    list: &'static str,
    index: usize,
    slot: &SlotProgress,
    previous: &SlotProgress,
) -> Option<ToolUpdate> {
    let output_path: Vec<Seg> = vec![
        Seg::Key("docs".to_owned()),
        Seg::Key("pi.live".to_owned()),
        Seg::Key(list.to_owned()),
        Seg::Index(index),
        Seg::Key("output".to_owned()),
    ];
    let mut trim_start = 0usize;
    let mut append = String::new();
    let mut set = false;
    for op in view_ops {
        let Some(path) = op_path(op) else {
            continue;
        };
        if !path.starts_with(&output_path) {
            continue;
        }
        match op {
            Op::Truncate(_, count) => trim_start += count,
            Op::Append(_, text) => append.push_str(text),
            _ => set = true,
        }
    }
    let output = if set || (slot.output != previous.output && trim_start == 0 && append.is_empty())
    {
        Some(ToolOutputChange::Set {
            set: slot.output.clone().unwrap_or_default(),
        })
    } else if trim_start > 0 || !append.is_empty() {
        Some(ToolOutputChange::Window {
            trim_start: (trim_start > 0).then_some(trim_start),
            append: (!append.is_empty()).then_some(append),
        })
    } else {
        None
    };
    // A safe replay clears a running slot's progress: removed details send
    // `null`, removed diagnostics `[]`.
    let details = if slot.details == previous.details {
        Tri::Absent
    } else {
        match &slot.details {
            Some(value) => Tri::Value(value.clone()),
            None => Tri::Null,
        }
    };
    let diagnostics = if slot.diagnostics == previous.diagnostics {
        Tri::Absent
    } else {
        match &slot.diagnostics {
            Some(value) => Tri::Value(value.clone()),
            None => Tri::Value(Vec::new()),
        }
    };
    let update = ToolUpdate {
        output,
        details,
        diagnostics,
    };
    if update.is_empty() {
        None
    } else {
        Some(update)
    }
}

// ─── Small helpers ──────────────────────────────────────────────────────────

fn queued(inbox: Option<&InboxState>) -> Vec<QueuedItem> {
    inbox
        .map(|inbox| inbox.items.iter().map(QueuedItem::of).collect())
        .unwrap_or_default()
}

/// The model message a raw value decodes to, when it is one.
fn message_of(value: &Value) -> Option<Message> {
    serde_json::from_value(value.clone()).ok()
}

/// The assistant message a raw partial decodes to, when it is one.
fn assistant_of(value: &Value) -> Option<AssistantMessage> {
    match message_of(value)? {
        Message::Assistant(message) => Some(message),
        _ => None,
    }
}

/// The entry with `id`, when this commit appended it.
fn find_entry(
    entries: &[EntryRecord],
    id: Option<crate::durable::ids::EntryId>,
) -> Option<EntryRecord> {
    let id = id?;
    entries.iter().find(|entry| entry.id == id).cloned()
}

/// The tool result for `call_id` among `entries`.
fn result_of<'a>(entries: &'a [EntryRecord], call_id: &str) -> Option<&'a EntryRecord> {
    entries.iter().find(|entry| {
        entry
            .model
            .as_ref()
            .and_then(|messages| messages.first())
            .is_some_and(|message| {
                message.get("role").and_then(Value::as_str) == Some("toolResult")
                    && message.get("toolCallId").and_then(Value::as_str) == Some(call_id)
            })
    })
}

/// A task state's intent checkpoint, which carries a tool call's arguments.
fn checkpoint_arguments(state: &TaskState) -> Option<JsonObject> {
    let checkpoint = match state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. }
        | TaskState::Sleeping { checkpoint, .. } => checkpoint,
        // `completing` and `terminal` carry an outcome, not a checkpoint.
        TaskState::Completing { .. } | TaskState::Terminal { .. } => return None,
    };
    checkpoint
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
}

/// The generations whose held outcome already ended their turn: `completing`
/// generation tasks of one conversation (upstream's in-attach scan).
///
/// # Errors
///
/// The scan fails (the session failed or closed).
pub async fn held_generations(
    session: &Session,
    conversation_id: ConversationId,
    context: &Context,
) -> Result<HashSet<TaskId>, StorageError> {
    let query = TaskQuery {
        conversation_id: Some(conversation_id),
        kind: Some(GENERATION_TASK_KIND.to_owned()),
        status: Some(TaskStatus::Completing),
        ..TaskQuery::default()
    };
    let tasks = scan_all(|cursor| session.scan_tasks(query.clone(), 100, cursor, context)).await?;
    Ok(tasks.into_iter().map(|task| task.id).collect())
}

// ─── The stream ─────────────────────────────────────────────────────────────

/// Detaches an event observer; called once when the stream ends.
type ReleaseAction = Box<dyn FnOnce() + Send>;

/// Reports a contained listener failure (upstream `ConversationViews.report`).
type Reporter = Arc<dyn Fn(&str) + Send + Sync>;

type Listener = Arc<dyn Fn(&[AgentEvent], &Context) + Send + Sync>;

struct WatchState {
    pending: VecDeque<(Vec<AgentEvent>, Context)>,
    listener: Option<Listener>,
    started: bool,
    delivering: bool,
    end: Option<WatchEnd>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One serialized, bounded watch of event batches.
///
/// Delivery never nests. Past [`MAX_PENDING_EVENT_BATCHES`] undelivered batches
/// the pending suffix is replaced by one fresh snapshot, exactly like the
/// incident watch of spec §9.2.
struct EventWatch {
    state: Mutex<WatchState>,
    release: Mutex<Option<ReleaseAction>>,
    report: Reporter,
    /// The value an overflow delivers: one snapshot of the newest revision.
    replace: Box<dyn Fn() -> Vec<AgentEvent> + Send + Sync>,
    scheduled: AtomicBool,
    overflowed: AtomicBool,
    closed_rx: tokio::sync::watch::Receiver<Option<WatchEnd>>,
    closed_tx: tokio::sync::watch::Sender<Option<WatchEnd>>,
}

impl EventWatch {
    fn new(report: Reporter, replace: Box<dyn Fn() -> Vec<AgentEvent> + Send + Sync>) -> Self {
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(None);
        Self {
            state: Mutex::new(WatchState {
                pending: VecDeque::new(),
                listener: None,
                started: false,
                delivering: false,
                end: None,
            }),
            release: Mutex::new(None),
            report,
            replace,
            scheduled: AtomicBool::new(false),
            overflowed: AtomicBool::new(false),
            closed_rx,
            closed_tx,
        }
    }

    /// Install the release action, running it at once when the stream already
    /// ended.
    ///
    /// The end flag is re-checked under the release lock: `terminate` records
    /// the end before it takes the slot, so an action stored after the first
    /// check would otherwise never run and its registration would leak.
    fn install_release(&self, release: ReleaseAction) {
        let mut slot = lock(&self.release);
        let ended = lock(&self.state).end.is_some();
        if ended {
            drop(slot);
            release();
            return;
        }
        *slot = Some(release);
    }

    fn start(self: &Arc<Self>, listener: Listener) -> Result<(), StorageError> {
        {
            let mut state = lock(&self.state);
            if state.started {
                return Err(StorageError::invalid(
                    "Agent event stream is already started",
                ));
            }
            if state.end.is_some() {
                return Err(StorageError::invalid("Agent event stream is stopped"));
            }
            state.started = true;
            state.listener = Some(listener);
        }
        self.drain();
        Ok(())
    }

    /// Queue one batch; overflow replaces the undelivered suffix with one
    /// snapshot of the newest state.
    fn advance(self: &Arc<Self>, events: Vec<AgentEvent>, context: Context) {
        {
            let mut state = lock(&self.state);
            if state.end.is_some() {
                return;
            }
            if state.pending.len() >= MAX_PENDING_EVENT_BATCHES {
                state.pending.clear();
                state.pending.push_back(((self.replace)(), context));
                self.overflowed.store(true, Ordering::Release);
            } else {
                state.pending.push_back((events, context));
            }
            if !state.started {
                return;
            }
        }
        self.schedule();
    }

    /// Arrange one delivery pass, off the caller's thread when a runtime runs.
    fn schedule(self: &Arc<Self>) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let watch = Arc::clone(self);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    watch.scheduled.store(false, Ordering::Release);
                    watch.drain();
                });
            }
            Err(_) => {
                self.scheduled.store(false, Ordering::Release);
                self.drain();
            }
        }
    }

    /// Deliver every queued batch now, one at a time.
    fn drain(self: &Arc<Self>) {
        loop {
            {
                let mut state = lock(&self.state);
                if state.delivering || !state.started || state.end.is_some() {
                    return;
                }
                if state.pending.is_empty() {
                    return;
                }
                state.delivering = true;
            }
            let failure = self.deliver_queued();
            {
                lock(&self.state).delivering = false;
            }
            if let Some(reason) = failure {
                self.terminate(reason);
                return;
            }
        }
    }

    /// Deliver queued batches until the queue empties or a listener fails.
    fn deliver_queued(&self) -> Option<WatchEnd> {
        loop {
            let (listener, events, context) = {
                let mut state = lock(&self.state);
                if state.end.is_some() {
                    return None;
                }
                let listener = state.listener.clone()?;
                let (events, context) = state.pending.pop_front()?;
                (listener, events, context)
            };
            let delivery = context.without_abort_signal();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(&events, &delivery);
            })) {
                Ok(()) => {}
                Err(payload) => {
                    let message =
                        format!("Agent event listener failed: {}", panic_message(&payload));
                    (self.report)(&message);
                    return Some(WatchEnd::ListenerError(StorageError::invalid(message)));
                }
            }
        }
    }

    fn terminate(&self, reason: WatchEnd) {
        {
            let mut state = lock(&self.state);
            if state.end.is_some() {
                return;
            }
            state.end = Some(reason.clone());
            state.pending.clear();
            state.listener = None;
        }
        // Taken outside the state lock: `install_release` holds the release
        // lock while it re-checks the end flag, and the action runs outside
        // every lock so a detach cannot re-enter a held mutex.
        let release = lock(&self.release).take();
        if let Some(release) = release {
            release();
        }
        let _ = self.closed_tx.send(Some(reason));
    }

    fn stop(&self) -> WatchEnd {
        self.terminate(WatchEnd::Stopped);
        self.reason().unwrap_or(WatchEnd::Stopped)
    }

    fn reason(&self) -> Option<WatchEnd> {
        lock(&self.state).end.clone()
    }

    fn close_session(&self, failure: Option<StorageError>) {
        match failure {
            Some(error) => self.terminate(WatchEnd::SessionFailed(error)),
            None => self.terminate(WatchEnd::SessionClosed),
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "listener panicked".to_owned())
}

/// The observer installed on the mount: it translates each publication into
/// batches and advances the stream.
struct StreamObserver {
    conversation_id: ConversationId,
    watch: Arc<EventWatch>,
    /// The newest revision's documents, shared with the overflow snapshot and
    /// reused as the next publication's `before`. `translate` never reads
    /// `entries`, so the port carries none; the newest entries live in
    /// [`Self::entries`] for the snapshot only.
    current: Arc<Mutex<Arc<ViewPort>>>,
    /// The newest active entries, refreshed only by the commits that change
    /// them. Entry records are the bulk of a revision, so a publication that
    /// only advances documents does not copy them.
    entries: Arc<Mutex<Arc<Vec<EntryRecord>>>>,
    /// Generations whose held outcome already ended their turn (upstream's
    /// `held`).
    held: Mutex<HashSet<TaskId>>,
}

impl StreamObserver {
    fn new(
        conversation_id: ConversationId,
        watch: Arc<EventWatch>,
        current: Arc<Mutex<Arc<ViewPort>>>,
        entries: Arc<Mutex<Arc<Vec<EntryRecord>>>>,
        held: HashSet<TaskId>,
    ) -> Self {
        Self {
            conversation_id,
            watch,
            current,
            entries,
            held: Mutex::new(held),
        }
    }
}

impl ViewObserver for StreamObserver {
    fn publication(
        &self,
        _before: &ConversationView,
        after: &ConversationView,
        ops: &[Op],
        publication: &CommitPublication,
        context: &Context,
    ) {
        // A publication that neither changed the mount nor carries the commit
        // half of the translation produces no events at all. Skipping it
        // leaves `current` correct: with no operations the mount value is
        // unchanged, so the cached port already is the newest revision.
        if ops.is_empty() && !translates_commit(publication, self.conversation_id) {
            return;
        }
        // `_before` is the revision this observer cached for the previous
        // publication: the mount advances once per publication, in order, so
        // one decode per changed revision replaces decoding both sides plus a
        // third copy for overflow snapshots. The entries the mounts carry are
        // not read here — the events' entries come from the commit — so the
        // port keeps only the documents and the entries cache advances when
        // the commit changes them.
        let before_port = Arc::clone(&lock(&self.current));
        let after_port = Arc::new(ViewPort::from_docs(Vec::new(), |kind| {
            after.docs.get(kind).map(|value| (**value).clone())
        }));
        *lock(&self.current) = Arc::clone(&after_port);
        if changes_entries(publication, self.conversation_id) {
            // The mount's entry list is shared copy-on-write: reuse it rather
            // than copying the whole transcript for the cache.
            *lock(&self.entries) = Arc::clone(&after.entries);
        }
        let mut held = lock(&self.held);
        let events = translate(
            self.conversation_id,
            &before_port,
            &after_port,
            ops,
            publication,
            &mut held,
        );
        drop(held);
        if !events.is_empty() {
            self.watch.advance(events, context.without_abort_signal());
        }
    }

    fn close_session(&self, failure: Option<&StorageError>) {
        self.watch.close_session(failure.cloned());
    }
}

/// A conversation's serialized event batches, one per commit.
pub struct EventsStream {
    snapshot: SnapshotEvent,
    watch: Arc<EventWatch>,
    conversation_id: ConversationId,
    /// The task awaiting the acquisition context's cancellation, when one was
    /// installed.
    cancel: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for EventsStream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl std::fmt::Debug for EventsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventsStream")
            .field("conversation", &self.conversation_id)
            .field("snapshot", &self.snapshot)
            .field("ended", &self.reason())
            .finish_non_exhaustive()
    }
}

impl EventsStream {
    /// The `snapshot` event captured at attachment.
    #[must_use]
    pub fn snapshot(&self) -> &SnapshotEvent {
        &self.snapshot
    }

    /// The attachment snapshot as the wire event (`{"type":"snapshot", ...}`).
    #[must_use]
    pub fn snapshot_event(&self) -> AgentEvent {
        AgentEvent::Snapshot(self.snapshot.clone())
    }

    /// The conversation this stream observes.
    #[must_use]
    pub fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    /// Whether a backlog overflow replaced undelivered batches with a
    /// snapshot.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.watch.overflowed.load(Ordering::Acquire)
    }

    /// Install the sole listener and deliver whatever is pending.
    ///
    /// # Errors
    ///
    /// The stream was already started or already ended.
    pub fn start<F>(&self, listener: F) -> Result<(), StorageError>
    where
        F: Fn(&[AgentEvent], &Context) + Send + Sync + 'static,
    {
        self.watch.start(Arc::new(listener))
    }

    /// Deliver every queued batch now, synchronously.
    ///
    /// An unattached host outside a runtime calls this after each commit; a
    /// runtime host gets the same delivery scheduled by [`EventWatch`]'s
    /// advance.
    pub fn drain(&self) {
        self.watch.drain();
    }

    /// Stop the stream; idempotent. Returns the terminal reason (the first one,
    /// when something ended it earlier).
    pub fn stop(&self) -> WatchEnd {
        if let Some(task) = self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        self.watch.stop()
    }

    /// The terminal reason, once the stream ended.
    #[must_use]
    pub fn reason(&self) -> Option<WatchEnd> {
        self.watch.reason()
    }

    /// Wait for the stream to end and return its reason.
    pub async fn closed(&self) -> WatchEnd {
        let mut receiver = self.watch.closed_rx.clone();
        loop {
            if let Some(end) = receiver.borrow().clone() {
                return end;
            }
            if receiver.changed().await.is_err() {
                return receiver.borrow().clone().unwrap_or(WatchEnd::SessionClosed);
            }
        }
    }
}

/// Attach to one conversation's committed agent events (spec §9.4), with no
/// `held` seed.
///
/// See [`watch_events_with_held`] for the seeded form and the deviation note in
/// the module documentation.
///
/// # Errors
///
/// The conversation does not exist, the session is closed or failed, or the
/// acquisition context is aborted.
pub async fn watch_events(
    views: &Arc<ConversationViews>,
    conversation_id: ConversationId,
    context: Context,
) -> Result<EventsStream, StorageError> {
    watch_events_with_held(views, conversation_id, HashSet::new(), context).await
}

/// Attach to one conversation's committed agent events, seeding the set of
/// generations that already ended their turn (upstream's in-attach scan; use
/// [`held_generations`]).
///
/// The snapshot and the registration for later commits are captured atomically
/// in [`ConversationViews::attach`]; overflow replaces undelivered batches with
/// one snapshot. The acquisition `context` governs the stream's lifetime: an
/// aborted context rejects the attachment, and later cancellation ends the
/// stream with [`WatchEnd::Cancelled`].
///
/// # Errors
///
/// The conversation does not exist, the session is closed or failed, or the
/// acquisition context is aborted.
pub async fn watch_events_with_held(
    views: &Arc<ConversationViews>,
    conversation_id: ConversationId,
    held: HashSet<TaskId>,
    context: Context,
) -> Result<EventsStream, StorageError> {
    context.check().map_err(|error| {
        StorageError::invalid(format!("Agent event attachment cancelled: {error}"))
    })?;

    let current: Arc<Mutex<Arc<ViewPort>>> = Arc::new(Mutex::new(Arc::new(ViewPort::default())));
    let latest_entries: Arc<Mutex<Arc<Vec<EntryRecord>>>> =
        Arc::new(Mutex::new(Arc::new(Vec::new())));
    let snapshot_slot: Arc<Mutex<Option<SnapshotEvent>>> = Arc::new(Mutex::new(None));
    let watch_slot: Arc<Mutex<Option<Arc<EventWatch>>>> = Arc::new(Mutex::new(None));
    let reporter: Reporter = {
        let views: Weak<ConversationViews> = Arc::downgrade(views);
        Arc::new(move |message: &str| {
            if let Some(views) = views.upgrade() {
                views.report(message);
            }
        })
    };

    let attached = {
        let current = Arc::clone(&current);
        let latest_entries = Arc::clone(&latest_entries);
        let snapshot_slot = Arc::clone(&snapshot_slot);
        let watch_slot = Arc::clone(&watch_slot);
        let reporter = Arc::clone(&reporter);
        views
            .attach::<StreamObserver, _>(
                conversation_id,
                move |view| {
                    let mut port = ViewPort::from_view(view);
                    *lock(&snapshot_slot) = Some(snapshot_of(&port));
                    // The entries belong to the snapshot cache from here on;
                    // the port kept for translations holds documents only.
                    *lock(&latest_entries) = Arc::new(std::mem::take(&mut port.entries));
                    *lock(&current) = Arc::new(port);
                    let replace_current = Arc::clone(&current);
                    let replace_entries = Arc::clone(&latest_entries);
                    let watch = Arc::new(EventWatch::new(
                        reporter,
                        Box::new(move || {
                            let docs = lock(&replace_current);
                            let entries = lock(&replace_entries);
                            vec![AgentEvent::Snapshot(snapshot_with_entries(
                                &docs,
                                entries.as_ref().clone(),
                            ))]
                        }),
                    ));
                    *lock(&watch_slot) = Some(Arc::clone(&watch));
                    StreamObserver::new(conversation_id, watch, current, latest_entries, held)
                },
                &context,
            )
            .await?
    };

    let watch = lock(&watch_slot)
        .take()
        .ok_or_else(|| StorageError::invalid("Agent event attachment produced no stream"))?;
    let snapshot = lock(&snapshot_slot)
        .take()
        .ok_or_else(|| StorageError::invalid("Agent event attachment produced no snapshot"))?;

    // Like a watch, the acquisition context governs the stream's lifetime.
    if context.is_aborted() {
        watch.terminate(WatchEnd::Cancelled);
        attached.detach.detach();
        return Err(StorageError::invalid("Agent event attachment cancelled"));
    }
    let detach = attached.detach.clone();
    watch.install_release(Box::new(move || detach.detach()));
    let stream = EventsStream {
        snapshot,
        watch,
        conversation_id,
        cancel: std::sync::Mutex::new(None),
    };
    if let Some(signal) = context.signal().cloned() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let watch = Arc::clone(&stream.watch);
            let task = handle.spawn(async move {
                signal.cancelled().await;
                watch.terminate(WatchEnd::Cancelled);
            });
            *stream
                .cancel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
        }
    }

    Ok(stream)
}
