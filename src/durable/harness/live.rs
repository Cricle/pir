//! The built-in `pi.live` document (spec §8.2).
//!
//! Port of upstream `packages/durable/src/harness/live.ts` (the document shape
//! and slot helpers). `pi.live` carries run control and the presentation of the
//! current generation and tool round:
//!
//! * `run` — the task settling the run's inputs, and those inputs; present
//!   exactly while busy;
//! * `generation` — the current attempt, its committed partial, retry backoff
//!   and deferred poll;
//! * `tools` — the current round's slots in call order;
//! * `nestedTools` — nested calls of running tools, in creation order, so task
//!   IDs ascend (found by binary search);
//! * `compactions` and `compactionFailure` — live compaction presentation.
//!
//! The checkpoint rule commits a complete base whenever nothing runs: no
//! `generation` and no tool or nested slot in `running`. A slot holds output
//! only while running, so every base stays small.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::durable::documents::{AnyDocDefinition, CommonDocDefinition};
use crate::durable::ids::{EntryId, SubmissionId, TaskId};
use crate::durable::types::{DocumentFork, JsonObject};

/// The persisted document kind.
pub const LIVE_KIND: &str = "pi.live";
/// The persisted document version.
pub const LIVE_VERSION: i64 = 1;

/// A remark about a tool call for the model and the UI, such as truncation or
/// a spill path; never part of the tool's data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    /// `info`, `warn` or `error`.
    pub severity: String,
    /// Human-readable text.
    pub message: String,
    /// Stable machine code, when the tool sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Lifecycle of one tool slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotStatus {
    /// Not started yet.
    Pending,
    /// Executing; the only status that keeps a change off a base.
    Running,
    /// Settled, with its result entry or summary.
    Done,
}

impl SlotStatus {
    /// Whether this status is `running`.
    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

/// What a running tool call publishes, shared by model-issued and nested calls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotProgress {
    /// The call's ID in tool events.
    pub call_id: String,
    /// The tool name.
    pub name: String,
    /// Lifecycle state.
    pub status: SlotStatus,
    /// Retained running output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// UTF-8 bytes the output bounds dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    /// Lines the output bounds dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_lines: Option<u64>,
    /// Last `details()` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Diagnostics recorded through `api.diagnostic()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
}

impl SlotProgress {
    /// A pending slot for a call.
    pub fn new(call_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            status: SlotStatus::Pending,
            output: None,
            dropped_bytes: None,
            dropped_lines: None,
            details: None,
            diagnostics: None,
        }
    }
}

/// Presentation of one tool call of the current round.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSlot {
    /// Shared progress fields.
    #[serde(flatten)]
    pub progress: SlotProgress,
    /// The call's tool task; absent for a call not started yet (sequential
    /// round) and for a call its request did not offer, which starts `done`
    /// with the `entry` generation wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Result entry once done; absent when the tool task faulted or was
    /// orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
}

/// How a nested call ended: an error flag, its execution time, its spend, and up
/// to 500 characters of error text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSummary {
    /// Whether the call failed.
    pub is_error: bool,
    /// Execution time, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The call's spend, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::ai::types::Usage>,
    /// Up to 500 characters of error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Presentation of one nested call: a call a running tool made through
/// `executeTool()`.
///
/// `call_id` is `<parentCallId>/<key>`, the call's ID in tool events and unique
/// because keys contain no `/`. The Harness relates slots by task ID:
/// `task_id` is the call's tool task, `parent_task_id` the calling one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSlot {
    /// Shared progress fields.
    #[serde(flatten)]
    pub progress: SlotProgress,
    /// The call's parent call ID.
    pub parent_call_id: String,
    /// The calling tool task, model-issued or nested.
    pub parent_task_id: TaskId,
    /// The call's tool task.
    pub task_id: TaskId,
    /// What the call was made with, replaced by the arguments it runs with once
    /// it starts executing.
    pub arguments: JsonObject,
    /// Once done: how the call ended; the result lives in the caller's task
    /// documents, never here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<NestedToolSummary>,
}

/// Presentation of one live compaction task (spec §8.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStatus {
    /// The compaction task.
    pub task_id: TaskId,
    /// `manual`, `threshold` or `overflow`.
    pub reason: String,
    /// Whether a generation waits for it: a compaction the generation owns.
    pub blocking: bool,
    /// Summarization attempt number.
    pub attempt: i64,
    /// Durable backoff before the next summarization attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
}

/// A durable backoff before the next attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStatus {
    /// Harness-clock milliseconds at which the attempt becomes eligible.
    pub at: i64,
    /// The error that caused the backoff.
    pub error: String,
}

/// The newest compaction that failed since a compaction last produced a
/// summary, so a client can show it after the task ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailure {
    /// The failed compaction task.
    pub task_id: TaskId,
    /// `manual`, `threshold` or `overflow`.
    pub reason: String,
    /// The failed or faulted outcome's error message, or why the scheduler
    /// orphaned the task.
    pub error: String,
}

/// Run control: the task that settles the run's inputs, and those inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunState {
    /// The run task.
    pub task_id: TaskId,
    /// Inputs the run task settles.
    pub inputs: Vec<SubmissionId>,
}

/// Presentation of the current generation attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationState {
    /// Attempt number, starting at one.
    pub attempt: i64,
    /// Committed throttled partial of the in-flight response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
    /// Durable backoff before the next attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
    /// Provider-side deferred response being polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredStatus>,
}

/// A deferred provider response being polled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredStatus {
    /// Harness-clock milliseconds of the next poll.
    pub poll_at: i64,
}

/// Built-in live conversation state: run control and presentation of the
/// current generation and tool round.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    /// Run control; present exactly while busy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunState>,
    /// Presentation of the current generation attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationState>,
    /// The current tool round in call order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
    /// Nested calls of running tool calls, in creation order, so a parent
    /// precedes its children and task IDs ascend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_tools: Option<Vec<NestedToolSlot>>,
    /// Live compaction tasks in task ID order; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions: Option<Vec<CompactionStatus>>,
    /// The newest compaction failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<CompactionFailure>,
}

impl LiveState {
    /// An empty live state, the initial value of a new incarnation.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Initial value of a new `pi.live` incarnation: `{}`.
fn live_initial() -> JsonObject {
    JsonObject::new()
}

/// `pi.live`'s checkpoint predicate: a complete base whenever nothing runs
/// (spec §8.2).
///
/// Nothing runs when `generation` is absent and no tool or nested slot is
/// `running`. That holds while idle, in the commit handing a generation over to
/// its tool round, and between tools, so the delta chain spans at most one
/// generation or one round's overlapping tool execution.
fn live_checkpoint_when(value: &JsonObject) -> bool {
    if value.contains_key("generation") {
        return false;
    }
    let running = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|slots| {
                slots
                    .iter()
                    .any(|slot| slot.get("status").and_then(Value::as_str) == Some("running"))
            })
    };
    !running("tools") && !running("nestedTools")
}

/// The built-in `pi.live` definition (spec §8.2).
///
/// Kind `pi.live`, version 1, conversation scope, `history: "latest"`,
/// `fork: "initial"`, initial `{}`, checkpoint whenever nothing runs.
///
/// A `LazyLock` because the definition carries function pointers and building
/// it validates the version, which is not a `const` operation.
pub static LIVE_DEFINITION: LazyLock<AnyDocDefinition> = LazyLock::new(|| {
    AnyDocDefinition::define(
        CommonDocDefinition::conversation_latest(LIVE_KIND, LIVE_VERSION, DocumentFork::Initial)
            .expect("the pi.live definition version is valid"),
        live_initial,
        None,
        Some(live_checkpoint_when),
    )
});

/// Which list holds a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotLocation {
    /// An index into [`LiveState::tools`].
    Tools(usize),
    /// An index into [`LiveState::nested_tools`].
    Nested(usize),
}

/// The slot of tool task `task_id`, model-issued or nested, if `pi.live` still
/// lists it.
///
/// Nested slots are found by binary search: a tool may make thousands of nested
/// calls, each of which looks up its slot in several commits. Their task IDs
/// ascend — each slot is appended in the commit that creates its task, task IDs
/// ascend in commit order, and removal keeps the order.
#[must_use]
pub fn find_tool_slot(state: &LiveState, task_id: TaskId) -> Option<SlotLocation> {
    if let Some(tools) = &state.tools {
        if let Some(index) = tools.iter().position(|slot| slot.task_id == Some(task_id)) {
            return Some(SlotLocation::Tools(index));
        }
    }
    find_nested_slot(state, task_id).map(SlotLocation::Nested)
}

/// The index of nested slot `task_id` in [`LiveState::nested_tools`].
#[must_use]
pub fn find_nested_slot(state: &LiveState, task_id: TaskId) -> Option<usize> {
    let nested = state.nested_tools.as_deref()?;
    let mut low = 0usize;
    let mut high = nested.len();
    while low < high {
        let middle = low + (high - low) / 2;
        match nested[middle].task_id.cmp(&task_id) {
            std::cmp::Ordering::Equal => return Some(middle),
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
        }
    }
    None
}

/// Mark the slot of `task_id` running. Returns whether it was found.
pub fn set_slot_running(state: &mut LiveState, task_id: TaskId) -> bool {
    match slot_progress_mut(state, task_id) {
        Some(progress) => {
            progress.status = SlotStatus::Running;
            true
        }
        None => false,
    }
}

/// Mark the slot of `task_id` done and clear its running progress.
///
/// The caller sets a model-issued call's result `entry`, or a nested call's
/// `summary`; neither is set when the task faulted or was orphaned.
pub fn set_slot_done(state: &mut LiveState, task_id: TaskId) -> bool {
    match slot_progress_mut(state, task_id) {
        Some(progress) => {
            progress.status = SlotStatus::Done;
            clear_progress(progress);
            true
        }
        None => false,
    }
}

/// Remove the nested slots below tool task `task_id`, transitively.
///
/// This is upstream `removeNestedSlots()`: a call's nested slots leave when the
/// call settles. The list holds parents before their children (creation order),
/// so one forward pass collects every descendant. Model-issued slots stay in
/// [`LiveState::tools`] until the generation's `tools` phase drops the whole
/// round; `tools` slots are marked done by [`set_slot_done`] instead.
///
/// Returns whether anything was removed.
pub fn set_slot_removed(state: &mut LiveState, task_id: TaskId) -> bool {
    let Some(nested) = &mut state.nested_tools else {
        return false;
    };
    let mut removed_ids = vec![task_id];
    let mut removed = false;
    let mut index = 0;
    while index < nested.len() {
        if removed_ids.contains(&nested[index].parent_task_id) {
            removed_ids.push(nested[index].task_id);
            nested.remove(index);
            removed = true;
        } else {
            index += 1;
        }
    }
    if nested.is_empty() {
        state.nested_tools = None;
    }
    removed
}

/// Remove what a tool published while running; its result entry or a rerun
/// replaces it.
pub fn clear_progress(slot: &mut SlotProgress) {
    slot.output = None;
    slot.dropped_bytes = None;
    slot.dropped_lines = None;
    slot.details = None;
    slot.diagnostics = None;
}

/// Output omitted immediately before a chunk, as an environment given an
/// output window reports it (upstream `ShellOutputSkip`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputSkip {
    /// UTF-8 byte length of the omitted text.
    pub bytes: u64,
    /// Newlines (U+000A) in the omitted text.
    pub newlines: u64,
}

/// Apply one reported output chunk to a slot.
///
/// `output` is written as one string field: a head window grows by an append,
/// so the document diff for a commit is the chunk, not the whole window. A
/// chunk carrying `skipped` output follows a jump — the environment dropped
/// everything before it — so the prior window is replaced by the chunk, and
/// the dropped counts accumulate.
pub fn apply_output_chunk(slot: &mut SlotProgress, text: &str, skipped: Option<OutputSkip>) {
    match skipped {
        Some(skipped) => {
            // Saturating: the counters are a diagnostic ("how much of the
            // stream the environment dropped"), and a skipped report is
            // application-supplied. A wrapping or panicking add would turn a
            // hostile or merely buggy byte count into a panic on the commit
            // path; `harness::usage` accumulates the same way.
            if skipped.bytes > 0 {
                slot.dropped_bytes = Some(
                    slot.dropped_bytes
                        .unwrap_or(0)
                        .saturating_add(skipped.bytes),
                );
            }
            if skipped.newlines > 0 {
                slot.dropped_lines = Some(
                    slot.dropped_lines
                        .unwrap_or(0)
                        .saturating_add(skipped.newlines),
                );
            }
            slot.output = Some(text.to_owned());
        }
        None => {
            slot.output.get_or_insert_with(String::new).push_str(text);
        }
    }
}

fn slot_progress_mut(state: &mut LiveState, task_id: TaskId) -> Option<&mut SlotProgress> {
    let location = {
        if let Some(tools) = &state.tools {
            tools
                .iter()
                .position(|slot| slot.task_id == Some(task_id))
                .map(SlotLocation::Tools)
        } else {
            None
        }
        .or_else(|| {
            state
                .nested_tools
                .as_ref()
                .and_then(|nested| nested.iter().position(|slot| slot.task_id == task_id))
                .map(SlotLocation::Nested)
        })
    }?;
    match location {
        SlotLocation::Tools(index) => state.tools.as_mut().map(|tools| &mut tools[index].progress),
        SlotLocation::Nested(index) => state
            .nested_tools
            .as_mut()
            .map(|nested| &mut nested[index].progress),
    }
}

/// Render a `pi.live` state as the document's JSON object.
pub fn live_to_object(state: &LiveState) -> JsonObject {
    match serde_json::to_value(state) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

/// Decode a `pi.live` document value, tolerating unknown fields.
pub fn live_from_object(value: &JsonObject) -> LiveState {
    serde_json::from_value(Value::Object(value.clone())).unwrap_or_default()
}
