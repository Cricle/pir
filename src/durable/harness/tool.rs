//! The built-in `pi.tool` task (spec §8, upstream `harness/tool.ts`).
//!
//! One tool call — model-issued or nested — is one durable task. The definition
//! has two phases and one abort handler:
//!
//! ```text
//! call     resolve, validate, beforeTool, commit intent, execute, afterTool, settle
//! execute  recovery only: rerun when the stored and the current policy both say safe,
//!          otherwise settle an `interrupted` error result from the durable partial
//! abort    settle `aborted` / `interrupted` / `abandoned`, saying whether the call may have run
//! ```
//!
//! # What a call produces
//!
//! * A **model-issued** call appends one `pi.tool-result` entry and ends with
//!   `{ kind: "model", entryId, control? }`; the model reads the entry.
//! * A **nested** call — one a running tool made through
//!   [`ToolExecutionApi::execute_tool`] — writes its result into its caller's
//!   [`nested_call_definition`] family member and ends with `{ kind: "nested" }`.
//!   The caller reads that member, so the receipt stays small and the result
//!   retires with the caller.
//!
//! # The join points
//!
//! Upstream's task reads the conversation's agent, its entries, its settings and
//! its environment through the `Harness`, and runs a tool through the
//! registration's `execute()`. This port's [`TaskRuntime`] exposes none of those
//! (they belong to the harness-facade lane), so the task takes a [`ToolHost`]
//! carrying exactly the callbacks it needs:
//!
//! | host member | upstream source |
//! |---|---|
//! | [`ToolHost::agent`] | `runtime.agent()` — the tools a request offers, and those nested calls resolve among |
//! | [`ToolHost::entry`] | `runtime.entry(AssistantEntry, …)` |
//! | [`ToolHost::execute`] | `ToolRegistration.execute` (the wave-3D `AgentTool` adapter) |
//! | [`ToolHost::validate_arguments`] | `validateToolArguments` (the preflight schema walk) |
//! | [`ToolHost::output_limits`] | `ToolRegistration.outputLimits` |
//! | [`ToolHost::structured_output_schema`] | `ToolRegistration.structuredOutputSchema` |
//! | [`ToolHost::mint_id`] | `Tx` id minting, inside the commit |
//! | [`ToolHost::settings`] | `runtime.settings` |
//! | [`ToolHost::hooks`] | the `pi.tool` hook registrations the agent resolved |
//! | [`ToolHost::after_hook_extensions`] | the extension behind each `afterTool` handler |
//! | [`ToolHost::telemetry`] | `context.telemetry` (tool-side events) |
//! | [`ToolHost::tool_phase_end`] | `context.telemetry.setAttributes` (phase-end `tool.name` / `tool.call_id`) |
//!
//! The tool task itself never downcasts an implementation: [`ToolHost::execute`]
//! receives the registration and a [`ToolExecutionApi`], which is the seam wave
//! 3D binds to `crate::agent::tool_execution::AgentTool`.
//!
//! # Durable documents
//!
//! * the caller's `pi.tool.nested-call` family (task-scoped, one member per
//!   nested-call key);
//! * `pi.live` — the call's slot (`tools[]` or `nestedTools[]`), written on the
//!   `progress.output_interval_ms` throttle;
//! * `pi.usage` — the call's own spend, added in its settling commit.
//!
//! # Reading before writing
//!
//! Upstream reads documents on the line inside the commit (`tx.doc`). This
//! port's [`Transaction`] cannot read storage, and [`TaskRuntime::commit`] hands
//! a callback no [`SessionState`](crate::durable::session::line::SessionState),
//! so every document this module writes is read through
//! [`TaskRuntime::document`] immediately before the commit that stages it. Two
//! tool tasks settling in the same round can therefore each stage a `pi.live`
//! value read before the other landed; the port keeps a per-conversation writer
//! lock around that read-then-commit pair so the two never interleave. The
//! other writers are a tool's own progress commits, which are stopped before
//! settlement.
//!
//! # Deliberate simplifications
//!
//! * **No environment.** `ExecutionEnv`/`Models` are not ported; `env` and
//!   `models` are absent from [`ToolExecutionApi`]. Wave 3D supplies whatever a
//!   tool needs through [`ToolHost::execute`].
//! * **Nested creation is two commits.** A [`Transaction`] cannot mint an id,
//!   so the child task is created first ([`TaskRuntime::create_task`]) and the
//!   caller's index member plus the nested slot are written in the following
//!   commit. Upstream does all three in one commit; a crash between the two
//!   leaves a live child the caller does not index, which its settling scan
//!   still aborts.
//! * **`wait_for_task` polls.** [`TaskRuntime`] exposes no scheduler wait
//!   entry point: the event-driven wait lives on the scheduler's private handle
//!   (`SchedulerInner::wait_for_task`, `pub(super)` to the scheduler module),
//!   not on the runtime a tool receives. The nested child is therefore
//!   observed through [`TaskRuntime::get_task`] every
//!   [`TERMINAL_POLL_INTERVAL`]; the wait races that sleep against
//!   [`TaskRuntime::cancelled`], so an aborted invocation wakes it immediately
//!   instead of waiting out the interval.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ai::types::{
    ContentBlock, Message, TextContent, ToolCall, ToolResultMessage, ToolResultRole, Usage,
};
use crate::durable::chord::overlap;
use crate::durable::context::Context;
use crate::durable::documents::{
    AnyDocFamilyDefinition, CommonDocDefinition, DocDefinition, DocFamilyDefinition, DocOwner,
    DocumentSemantics, ResolvedAddress,
};
use crate::durable::entries::{ToolResultData, ToolResultEntry};
use crate::durable::errors::ReportFrom;
use crate::durable::harness::agent::{tool_extension, Agent};
use crate::durable::harness::json::assign_json;
use crate::durable::harness::live::{
    find_tool_slot, live_from_object, NestedToolSummary, SlotLocation, SlotProgress, LIVE_KIND,
    LIVE_VERSION,
};
use crate::durable::harness::registry::{ReplayPolicy, ToolRegistration};
use crate::durable::harness::scheduler::runtime::{TaskRuntime, TaskRuntimeError};
use crate::durable::harness::scheduler::{json_equal, panic_message, NextTaskState, TaskCatalogue};
use crate::durable::harness::settings::Settings;
use crate::durable::harness::task::{
    define_task, phase_handler, BoxFuture, ErasedTask, Migrated, RunningTask, Task,
    TaskDefinitionError,
};
use crate::durable::harness::telemetry::{HarnessTelemetry, SpanHandle};
use crate::durable::harness::usage::{
    usage_add, UsageScope, UsageState, USAGE_KIND, USAGE_VERSION,
};
use crate::durable::ids::{ConversationId, EntryId, TaskId};
use crate::durable::session::{LoadedDocument, StagedDocument, Transaction};
use crate::durable::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::durable::types::{
    DocumentAddress, DocumentFork, EntryRecord, JsonObject, StorageWrite, StoredDocument,
    TaskOutcome, TaskOutcomeError, TaskRecord, TaskState,
};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Registered definition name (spec §8).
pub const TOOL_TASK_NAME: &str = "pi.tool";
/// Definition version; version 1 had only model-issued calls.
pub const TOOL_TASK_VERSION: i64 = 2;

/// Memo recording that a tool task made at least one nested call, in any
/// attempt, committed before the first admission of each attempt.
pub const MADE_NESTED_CALLS: &str = "pi.tool.madeNestedCalls";

/// Persisted kind of the nested-call document family.
pub const NESTED_CALL_KIND: &str = "pi.tool.nested-call";
/// Persisted version of the family member shape.
pub const NESTED_CALL_VERSION: i64 = 1;

/// Progress commits pause proportionally to what they wrote: 100 KiB/s.
pub const PROGRESS_BYTES_PER_SECOND: u64 = 100 * 1024;

/// A nested result's summary keeps at most this many characters of error text.
pub const MAX_SUMMARY_ERROR_CHARS: usize = 500;

// ─── Task input, checkpoint and result ──────────────────────────────────────

/// A model-issued call, read from its assistant entry, or a nested call a
/// running tool made through `execute_tool()`, whose call the input carries
/// because no entry holds it.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolTaskInput {
    /// A call the model made; `assistant` is the entry that holds it.
    Model {
        /// The assistant entry carrying the call.
        assistant: EntryId,
        /// The call's id within that entry.
        call_id: String,
    },
    /// A call a running tool made.
    Nested {
        /// The calling tool task.
        parent: TaskId,
        /// The call id of the calling tool call.
        parent_call_id: String,
        /// The call's key within its caller; its id is `<parentCallId>/<key>`.
        key: String,
        /// The call itself.
        call: ToolCall,
        /// `false`: commit no running output, details or diagnostics to the
        /// slot; the result still gets them.
        progress: bool,
    },
}

impl ToolTaskInput {
    /// Whether this is a nested call.
    #[must_use]
    pub fn is_nested(&self) -> bool {
        matches!(self, Self::Nested { .. })
    }

    /// Whether the call publishes running progress.
    #[must_use]
    pub fn streams_progress(&self) -> bool {
        match self {
            Self::Model { .. } => true,
            Self::Nested { progress, .. } => *progress,
        }
    }

    /// Parse a stored input value.
    ///
    /// `progress` defaults to `true` (upstream's `progress?: false`), and a
    /// version-1 input without `kind` is a model-issued call.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "Tool task input must be an object".to_owned())?;
        if object.get("kind").and_then(Value::as_str) == Some("nested") {
            let parent = object
                .get("parent")
                .and_then(Value::as_i64)
                .ok_or_else(|| "Nested tool input has no parent".to_owned())?;
            let parent_call_id = string_field(object, "parentCallId")?;
            let key = string_field(object, "key")?;
            let call_value = object
                .get("call")
                .ok_or_else(|| "Nested tool input has no call".to_owned())?;
            let call: ToolCall = serde_json::from_value(call_value.clone())
                .map_err(|error| format!("Nested tool input call is invalid: {error}"))?;
            let progress = object
                .get("progress")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            return Ok(Self::Nested {
                parent: TaskId::from_raw(parent),
                parent_call_id,
                key,
                call,
                progress,
            });
        }
        Ok(Self::Model {
            assistant: EntryId::from_raw(
                object
                    .get("assistant")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "Tool task input has no assistant entry".to_owned())?,
            ),
            call_id: string_field(object, "callId")?,
        })
    }

    /// Render the input for the durable record, with upstream's camelCase keys.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Model { assistant, call_id } => json!({
                "kind": "model",
                "assistant": assistant.as_i64(),
                "callId": call_id,
            }),
            Self::Nested {
                parent,
                parent_call_id,
                key,
                call,
                progress,
            } => {
                let mut value = json!({
                    "kind": "nested",
                    "parent": parent.as_i64(),
                    "parentCallId": parent_call_id,
                    "key": key,
                    "call": call,
                });
                if !progress {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("progress".to_owned(), Value::Bool(false));
                    }
                }
                value
            }
        }
    }
}

/// A call's arguments as the durable JSON object a slot and checkpoint hold.
#[must_use]
pub fn arguments_object(arguments: &std::collections::BTreeMap<String, Value>) -> JsonObject {
    arguments
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// A durable JSON object as a call's arguments.
#[must_use]
pub fn object_arguments(object: &JsonObject) -> std::collections::BTreeMap<String, Value> {
    object
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Tool task input has no {key}"))
}

/// The durable checkpoint of a tool task.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolTaskCheckpoint {
    /// The call is being resolved; nothing is recorded yet.
    Call,
    /// Durable intent: the final arguments and the replay policy recorded
    /// before execution.
    Execute {
        /// The validated, hook-adjusted arguments the call runs with.
        arguments: JsonObject,
        /// The resolved tool's replay policy.
        replay: ReplayPolicy,
    },
}

impl ToolTaskCheckpoint {
    /// The checkpoint's durable JSON form.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Call => json!({ "phase": "call" }),
            Self::Execute { arguments, replay } => json!({
                "phase": "execute",
                "arguments": arguments,
                "replay": replay_name(*replay),
            }),
        }
    }

    /// Parse a stored checkpoint. Anything unrecognized is [`Self::Call`], so
    /// the call phase re-resolves rather than faults.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::Call;
        };
        if object.get("phase").and_then(Value::as_str) != Some("execute") {
            return Self::Call;
        }
        let arguments = object
            .get("arguments")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let replay = match object.get("replay").and_then(Value::as_str) {
            Some("safe") => ReplayPolicy::Safe,
            _ => ReplayPolicy::Unsafe,
        };
        Self::Execute { arguments, replay }
    }

    /// The phase name this checkpoint runs.
    #[must_use]
    pub fn phase(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Execute { .. } => "execute",
        }
    }
}

fn replay_name(replay: ReplayPolicy) -> &'static str {
    match replay {
        ReplayPolicy::Safe => "safe",
        ReplayPolicy::Unsafe => "unsafe",
    }
}

/// A model-issued call's result is its transcript entry; a nested call's result
/// lives in its caller's nested-call family member, so the receipt stays small.
///
/// A version-1 task that was already holding its outcome as `completing` when
/// the Harness upgraded finishes with a version-1 result, `{ entryId, control? }`
/// without `kind`; [`ToolTaskResult::from_value`] reads that as
/// [`Self::Model`].
#[derive(Clone, Debug, PartialEq)]
pub enum ToolTaskResult {
    /// A model-issued call, settled as a `pi.tool-result` entry.
    Model {
        /// The appended result entry.
        entry_id: EntryId,
        /// Post-tools controls the result requested.
        control: Option<ToolControl>,
    },
    /// A nested call; its result is in the caller's family member.
    Nested,
}

impl ToolTaskResult {
    /// Parse a stored result, accepting the version-1 shape without `kind`.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        match object.get("kind").and_then(Value::as_str) {
            Some("nested") => Some(Self::Nested),
            Some("model") | None => Some(Self::Model {
                entry_id: EntryId::from_raw(object.get("entryId").and_then(Value::as_i64)?),
                control: object
                    .get("control")
                    .cloned()
                    .and_then(|control| serde_json::from_value(control).ok()),
            }),
            Some(_) => None,
        }
    }

    /// Render the result for the durable record.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Model { entry_id, control } => {
                let mut value = json!({ "kind": "model", "entryId": entry_id.as_i64() });
                if let Some(control) = control {
                    if let Ok(control) = serde_json::to_value(control) {
                        if let Some(object) = value.as_object_mut() {
                            object.insert("control".to_owned(), control);
                        }
                    }
                }
                value
            }
            Self::Nested => json!({ "kind": "nested" }),
        }
    }

    /// The result entry of a model-issued call, if this is one.
    #[must_use]
    pub fn entry_id(&self) -> Option<EntryId> {
        match self {
            Self::Model { entry_id, .. } => Some(*entry_id),
            Self::Nested => None,
        }
    }

    /// The post-tools controls of a model-issued call, if any.
    #[must_use]
    pub fn control(&self) -> Option<&ToolControl> {
        match self {
            Self::Model { control, .. } => control.as_ref(),
            Self::Nested => None,
        }
    }
}

/// Post-tools controls a tool result may request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolControl {
    /// Tools the round's results ask to add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_tools: Option<Vec<String>>,
    /// End the run without another model request when every result of the
    /// round sets it to `true`; `false` is a no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    /// Hand the run to this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

impl ToolControl {
    /// A control that adds tools.
    #[must_use]
    pub fn with_add_tools(tools: Vec<String>) -> Self {
        Self {
            add_tools: Some(tools),
            ..Self::default()
        }
    }

    /// A control that terminates the run.
    #[must_use]
    pub fn terminate() -> Self {
        Self {
            terminate: Some(true),
            ..Self::default()
        }
    }
}

/// A remark about a call for the model and the UI, such as truncation or a
/// spill path; never part of the tool's data.
///
/// This is the same value `pi.live` carries; re-exported so every harness
/// surface names one type.
pub use crate::durable::harness::live::ToolDiagnostic;

// ─── Nested-call document family ────────────────────────────────────────────

/// One nested call of a running tool: its task, and once it ended, its result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedCallState {
    /// The call's tool task; absent only inside the commit that creates the
    /// call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Exactly what `execute_tool()` returns; absent while running, and for a
    /// call the scheduler faulted or orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// The nested calls of a tool call: a task-scoped family of the calling task,
/// keyed by the nested call's key.
///
/// A rerun therefore finds the calls it already made, and the caller reads each
/// result. One document per call, so no commit rewrites the others. Retires
/// with the caller.
#[must_use]
pub fn nested_call_definition() -> AnyDocFamilyDefinition {
    CommonDocDefinition::task(NESTED_CALL_KIND, NESTED_CALL_VERSION)
        .expect("the pi.tool.nested-call definition version is valid")
        .behaviour_family(JsonObject::new, None, None)
}

/// The identity form of [`nested_call_definition`], as staging needs it.
#[must_use]
pub fn nested_call_identity() -> DocFamilyDefinition {
    DocFamilyDefinition::task(NESTED_CALL_KIND, NESTED_CALL_VERSION)
        .expect("the pi.tool.nested-call definition version is valid")
}

/// The address of nested call `key` of caller `parent`.
pub fn nested_call_address(parent: TaskId, key: &str) -> Result<ResolvedAddress, String> {
    ResolvedAddress::new(
        NESTED_CALL_KIND,
        DocumentSemantics::Task,
        Some(DocOwner::Task(parent)),
        Some(key.to_owned()),
    )
    .map_err(|error| error.to_string())
}

// ─── Tool results ───────────────────────────────────────────────────────────

/// What a tool returns, on three channels: `output` for the model,
/// `structured_output` for programs, and `details` for UIs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionResult {
    /// For the model. Empty: the retained `output()` text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ContentBlock>,
    /// For programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    /// Whether the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// For UIs; absent: the last `details()` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Added after those recorded through `api.diagnostic()`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ToolDiagnostic>,
    /// Spend of the execution itself, such as a model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Post-tools controls; only a model-issued call applies them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ToolControl>,
}

impl ToolExecutionResult {
    /// A successful result from model-facing text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            output: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            ..Self::default()
        }
    }

    /// A successful result with a structured value for programs.
    #[must_use]
    pub fn structured(value: Value) -> Self {
        Self {
            structured_output: Some(value),
            ..Self::default()
        }
    }

    /// Whether this result is an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.is_error.unwrap_or(false)
    }

    /// The text items of the output, joined with newlines.
    #[must_use]
    pub fn output_text(&self) -> String {
        self.output
            .iter()
            .filter_map(|item| match item {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Result of a nested call, as `execute_tool()` returns it.
///
/// Programs read `structured_output`; the output for the model is not kept, and
/// `control` does not apply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolExecutionResult {
    /// The nested call's tool task.
    pub task_id: TaskId,
    /// The tool's value for programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    /// Whether the call failed.
    pub is_error: bool,
    /// UI details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// What went wrong, or remarks about the call.
    pub diagnostics: Vec<ToolDiagnostic>,
    /// The call's spend, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Execution time of the attempt that produced the result; absent when it
    /// never executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl NestedToolExecutionResult {
    /// A failed nested result with one diagnostic.
    #[must_use]
    pub fn error(task_id: TaskId, diagnostic: ToolDiagnostic) -> Self {
        Self {
            task_id,
            structured_output: None,
            is_error: true,
            details: None,
            diagnostics: vec![diagnostic],
            usage: None,
            duration_ms: None,
        }
    }
}

/// An error result the Harness writes itself: no content and one `error`
/// diagnostic with `code`.
#[must_use]
pub fn harness_error(code: &str, message: impl Into<String>) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Vec::new(),
        structured_output: None,
        is_error: Some(true),
        details: None,
        diagnostics: vec![tool_diagnostic(code, message)],
        usage: None,
        control: None,
    }
}

/// One `error`-severity diagnostic.
#[must_use]
pub fn tool_diagnostic(code: &str, message: impl Into<String>) -> ToolDiagnostic {
    ToolDiagnostic {
        severity: "error".to_owned(),
        message: message.into(),
        code: Some(code.to_owned()),
    }
}

/// The Harness's truncation diagnostic; `retain` is unknown when rebuilt from a
/// slot after recovery.
#[must_use]
pub fn truncated_diagnostic(
    dropped_bytes: u64,
    dropped_lines: u64,
    retain: Option<Retain>,
) -> ToolDiagnostic {
    let kept = match retain {
        None => String::new(),
        Some(Retain::Head) => " to its beginning".to_owned(),
        Some(Retain::Tail) => " to its end".to_owned(),
    };
    ToolDiagnostic {
        severity: "warn".to_owned(),
        message: format!(
            "Output truncated{kept}: {dropped_lines} lines, {dropped_bytes} bytes dropped"
        ),
        code: Some("truncated".to_owned()),
    }
}

/// Build an error result from a slot's durable partial output, details, and
/// diagnostics.
#[must_use]
pub fn from_slot(slot: Option<&SlotProgress>, code: &str, message: &str) -> ToolExecutionResult {
    let mut diagnostics = slot
        .and_then(|slot| slot.diagnostics.clone())
        .unwrap_or_default();
    let dropped_bytes = slot.and_then(|slot| slot.dropped_bytes).unwrap_or(0);
    if dropped_bytes > 0 {
        diagnostics.push(truncated_diagnostic(
            dropped_bytes,
            slot.and_then(|slot| slot.dropped_lines).unwrap_or(0),
            None,
        ));
    }
    diagnostics.push(tool_diagnostic(code, message));
    let output = match slot.and_then(|slot| slot.output.as_deref()) {
        None | Some("") => Vec::new(),
        Some(text) => vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
    };
    ToolExecutionResult {
        output,
        structured_output: None,
        is_error: Some(true),
        details: slot.and_then(|slot| slot.details.clone()),
        diagnostics,
        usage: None,
        control: None,
    }
}

// ─── Output limits and buffering ────────────────────────────────────────────

/// Which end of a stream a bounded window keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Retain {
    /// Keep the first lines (the default).
    #[default]
    Head,
    /// Keep the last lines.
    Tail,
}

/// Retention limits of one output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputLimits {
    /// Maximum UTF-8 bytes kept.
    pub max_bytes: usize,
    /// Maximum lines kept.
    pub max_lines: usize,
    /// Which end is kept.
    pub retain: Retain,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            retain: Retain::Head,
        }
    }
}

impl OutputLimits {
    /// Limits over the head of a stream.
    #[must_use]
    pub fn head(max_bytes: usize, max_lines: usize) -> Self {
        Self {
            max_bytes,
            max_lines,
            retain: Retain::Head,
        }
    }

    /// Limits over the tail of a stream.
    #[must_use]
    pub fn tail(max_bytes: usize, max_lines: usize) -> Self {
        Self {
            max_bytes,
            max_lines,
            retain: Retain::Tail,
        }
    }
}

/// Output a producer omitted before a chunk, measured on the decoded text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkippedOutput {
    /// UTF-8 byte length of the omitted text.
    pub bytes: u64,
    /// Newlines (U+000A) in the omitted text.
    pub newlines: u64,
    /// Whether the omitted text ends with a newline.
    pub ends_with_newline: bool,
}

/// The window a tool's environment reports to, for its own output truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputWindow {
    /// Maximum UTF-8 bytes retained.
    pub max_bytes: usize,
    /// Maximum lines retained.
    pub max_lines: usize,
    /// Minimum pause between progress commits.
    pub min_interval_ms: u64,
    /// Bytes per second of a progress commit's own pause.
    pub bytes_per_second: u64,
}

/// Retained output and what the limits dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundedOutput {
    /// The retained text.
    pub text: String,
    /// UTF-8 bytes the whole stream lost.
    pub dropped_bytes: u64,
    /// Lines the whole stream lost.
    pub dropped_lines: u64,
}

/// An exact slice of a text within the limits, and what it left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSlice {
    /// The kept text.
    pub text: String,
    /// UTF-8 bytes kept.
    pub bytes: u64,
    /// UTF-8 bytes dropped.
    pub dropped_bytes: u64,
    /// Lines dropped.
    pub dropped_lines: u64,
}

/// Remove control characters that break display and transcripts; tabs and
/// newlines stay.
#[must_use]
pub fn sanitize_output(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            let code = *ch as u32;
            !((code <= 0x08) || (0x0b..=0x1f).contains(&code) || (0xfff9..=0xfffb).contains(&code))
        })
        .collect()
}

/// The number of lines in `bytes`: newlines plus a final unterminated line.
fn line_count(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    newlines + u64::from(bytes[bytes.len() - 1] != b'\n')
}

/// The last character boundary at or before `index`.
fn character_end(bytes: &[u8], index: usize) -> usize {
    let mut end = index.min(bytes.len());
    while end > 0 && end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    end
}

/// The first character boundary at or after `index`.
fn character_start(bytes: &[u8], index: usize) -> usize {
    let mut start = index.min(bytes.len());
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    start
}

/// Bound `text` to whole lines within the limits: the first lines for
/// [`Retain::Head`], the last for [`Retain::Tail`]. The result is an exact
/// slice, trailing newline included. A single line longer than `max_bytes` is
/// cut at the byte limit on a character boundary.
#[must_use]
pub fn bound_output(text: &str, limits: OutputLimits) -> OutputSlice {
    let bytes = text.as_bytes();
    let (from, to) = match limits.retain {
        Retain::Head => head_range(bytes, limits),
        Retain::Tail => tail_range(bytes, limits),
    };
    let kept = &bytes[from..to];
    OutputSlice {
        text: String::from_utf8_lossy(kept).into_owned(),
        bytes: kept.len() as u64,
        dropped_bytes: (bytes.len() - kept.len()) as u64,
        dropped_lines: line_count(bytes) - line_count(kept),
    }
}

fn head_range(bytes: &[u8], limits: OutputLimits) -> (usize, usize) {
    if limits.max_lines == 0 || limits.max_bytes == 0 {
        return (0, 0);
    }
    let mut end = bytes.len();
    let mut lines = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines += 1;
            if lines == limits.max_lines {
                end = index + 1;
                break;
            }
        }
    }
    if end > limits.max_bytes {
        let window = &bytes[..limits.max_bytes.min(bytes.len())];
        end = match window.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) => newline + 1,
            None => character_end(bytes, limits.max_bytes),
        };
    }
    (0, end)
}

fn tail_range(bytes: &[u8], limits: OutputLimits) -> (usize, usize) {
    if limits.max_lines == 0 || limits.max_bytes == 0 {
        return (bytes.len(), bytes.len());
    }
    // A trailing newline ends the last line rather than starting another.
    let last = if bytes.last() == Some(&b'\n') {
        bytes.len().checked_sub(2)
    } else {
        bytes.len().checked_sub(1)
    };
    let mut start = 0usize;
    let mut lines = 1usize;
    let mut index = last.and_then(|last| bytes[..=last].iter().rposition(|byte| *byte == b'\n'));
    while let Some(newline) = index {
        if lines == limits.max_lines {
            start = newline + 1;
            break;
        }
        lines += 1;
        index = if newline == 0 {
            None
        } else {
            bytes[..newline].iter().rposition(|byte| *byte == b'\n')
        };
    }
    if bytes.len() - start > limits.max_bytes {
        let from = bytes.len() - limits.max_bytes;
        let newline = bytes[from.saturating_sub(1)..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| from.saturating_sub(1) + offset);
        start = match newline {
            Some(newline) if newline + 1 < bytes.len() => newline + 1,
            _ => character_start(bytes, from),
        };
    }
    (start, bytes.len())
}

/// Bounded running output of one stream.
///
/// Accepting a chunk costs time proportional to the chunk: head retention stops
/// storing once the window is full, and tail retention drops stored text the
/// window no longer needs. Counts of the whole stream are kept so the dropped
/// totals stay exact.
#[derive(Clone, Debug)]
pub struct OutputBuffer {
    limits: OutputLimits,
    stored: String,
    stored_newlines: u64,
    full: bool,
    total_bytes: u64,
    total_newlines: u64,
    ends_with_newline: bool,
}

impl OutputBuffer {
    /// A buffer with `limits`.
    #[must_use]
    pub fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            stored: String::new(),
            stored_newlines: 0,
            full: false,
            total_bytes: 0,
            total_newlines: 0,
            ends_with_newline: true,
        }
    }

    /// The limits this buffer keeps.
    #[must_use]
    pub fn limits(&self) -> OutputLimits {
        self.limits
    }

    /// Accept a chunk; returns whether anything was counted.
    ///
    /// `skipped` is output omitted right before the chunk; only tail retention
    /// accepts it, and it replaces whatever was stored.
    pub fn push(&mut self, chunk: &str, skipped: Option<SkippedOutput>) -> bool {
        if let Some(skipped) = skipped {
            debug_assert_eq!(
                self.limits.retain,
                Retain::Tail,
                "Skipped output requires tail retention"
            );
            if skipped.bytes > 0 {
                self.total_bytes = self.total_bytes.saturating_add(skipped.bytes);
                self.total_newlines = self.total_newlines.saturating_add(skipped.newlines);
                self.ends_with_newline = skipped.ends_with_newline;
                self.stored.clear();
                self.stored_newlines = 0;
            }
        }
        if chunk.is_empty() {
            return skipped.is_some();
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        let newlines = chunk.bytes().filter(|byte| *byte == b'\n').count() as u64;
        self.total_newlines = self.total_newlines.saturating_add(newlines);
        self.ends_with_newline = chunk.ends_with('\n');
        if self.full {
            return true;
        }
        self.stored.push_str(chunk);
        self.stored_newlines += newlines;
        match self.limits.retain {
            Retain::Head => {
                self.full = self.stored.len() > self.limits.max_bytes
                    || self.stored_newlines >= self.limits.max_lines as u64;
            }
            Retain::Tail => self.trim_tail(),
        }
        true
    }

    /// Drop stored text the tail window no longer needs, keeping a one-byte and
    /// one-line margin so a later window's first line can still be found.
    fn trim_tail(&mut self) {
        loop {
            let over_bytes = self.stored.len() > self.limits.max_bytes + 1;
            let over_lines = self.stored_newlines > self.limits.max_lines as u64 + 1;
            if !over_bytes && !over_lines {
                return;
            }
            let mut drop = 0usize;
            if over_lines {
                if let Some(newline) = self.stored.find('\n') {
                    drop = newline + 1;
                }
            }
            if over_bytes {
                let mut at = self.stored.len() - (self.limits.max_bytes + 1);
                while at < self.stored.len() && !self.stored.is_char_boundary(at) {
                    at += 1;
                }
                drop = drop.max(at);
            }
            if drop == 0 || drop >= self.stored.len() {
                return;
            }
            self.stored.drain(..drop);
            self.stored_newlines = self.stored.bytes().filter(|byte| *byte == b'\n').count() as u64;
        }
    }

    /// Flush an incomplete trailing character; text is stored as a `String`, so
    /// there is never one to flush. The method exists for parity upstream.
    pub fn end(&mut self) {}

    /// Retained, sanitized output and what the limits dropped from the whole
    /// stream.
    #[must_use]
    pub fn snapshot(&self) -> BoundedOutput {
        let kept = bound_output(&self.stored, self.limits);
        let total_lines = self.total_newlines + u64::from(!self.ends_with_newline);
        let kept_lines = line_count(kept.text.as_bytes());
        BoundedOutput {
            text: sanitize_output(&kept.text),
            dropped_bytes: self.total_bytes.saturating_sub(kept.bytes),
            dropped_lines: total_lines.saturating_sub(kept_lines),
        }
    }
}

// ─── Progress pacing ────────────────────────────────────────────────────────

type Waiter = tokio::sync::oneshot::Sender<Result<(), String>>;

/// Throttled progress commits: the first change after an idle period commits at
/// once; each commit then delays the next by at least `min_interval_ms` and by
/// its written size at [`PROGRESS_BYTES_PER_SECOND`]. At most one commit is in
/// flight; changes made meanwhile coalesce into the next one.
///
/// Port of chord's `Progress`. The pacing timer is a `tokio` sleep rather than
/// the harness clock's `setTimer`: the interval only paces display commits and
/// must never be durable work. `now` still reads the harness clock, so a test
/// clock controls whether a commit is due.
pub struct Progress {
    inner: Arc<ProgressInner>,
}

struct ProgressInner {
    write: Arc<dyn Fn() -> BoxFuture<'static, Result<u64, String>> + Send + Sync>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
    min_interval_ms: u64,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    state: Mutex<ProgressState>,
    settled: tokio::sync::Notify,
}

#[derive(Default)]
struct ProgressState {
    waiters: Vec<Waiter>,
    in_flight: bool,
    pacing: bool,
    next_at: i64,
    dirty: bool,
    stopped: bool,
}

impl Progress {
    /// Build a progress pacer.
    pub fn new(
        write: Arc<dyn Fn() -> BoxFuture<'static, Result<u64, String>> + Send + Sync>,
        on_error: Arc<dyn Fn(String) + Send + Sync>,
        min_interval_ms: u64,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(ProgressInner {
                write,
                on_error,
                min_interval_ms,
                now,
                state: Mutex::new(ProgressState::default()),
                settled: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Schedule a commit.
    pub fn mark(&self) {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.dirty = true;
        }
        self.schedule();
    }

    /// Schedule a commit; the receiver settles with the commit that includes
    /// this change.
    pub fn mark_and_wait(&self) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.waiters.push(sender);
            state.dirty = true;
        }
        self.schedule();
        receiver
    }

    /// Stop committing and wait for the commit in flight; returns the waiters
    /// the terminal commit must settle.
    pub async fn stop(&self) -> Vec<Waiter> {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopped = true;
        }
        loop {
            let notified = self.inner.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let in_flight = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .in_flight;
            if !in_flight {
                break;
            }
            notified.await;
        }
        std::mem::take(
            &mut self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .waiters,
        )
    }

    fn schedule(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopped || state.pacing || state.in_flight {
            return;
        }
        let wait = state.next_at - (self.inner.now)();
        if wait <= 0 {
            drop(state);
            self.flush();
            return;
        }
        state.pacing = true;
        drop(state);
        let inner = self.inner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait.max(0) as u64)).await;
            {
                let mut state = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.pacing = false;
            }
            // Re-checks the clock: a timer that fires early waits for the rest.
            Progress { inner }.schedule();
        });
    }

    fn flush(&self) {
        let waiters = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.stopped || !state.dirty {
                return;
            }
            state.dirty = false;
            state.in_flight = true;
            std::mem::take(&mut state.waiters)
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let started = (inner.now)();
            let result = (inner.write)().await;
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.in_flight = false;
            match result {
                Ok(bytes) => {
                    let paced = bytes.saturating_mul(1000) / PROGRESS_BYTES_PER_SECOND;
                    state.next_at = started + paced.max(inner.min_interval_ms) as i64;
                    for waiter in waiters {
                        let _ = waiter.send(Ok(()));
                    }
                }
                Err(error) => {
                    state.next_at = started + inner.min_interval_ms as i64;
                    for waiter in waiters {
                        let _ = waiter.send(Err(error.clone()));
                    }
                    (inner.on_error)(error);
                }
            }
            drop(state);
            inner.settled.notify_waiters();
            Progress { inner }.schedule();
        });
    }
}

// ─── Document staging ───────────────────────────────────────────────────────

/// The documents a tool task stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocKind {
    Live,
    Usage,
    NestedCall,
}

/// The address of the conversation's `pi.live` document.
pub fn live_address(conversation: ConversationId) -> Result<ResolvedAddress, String> {
    ResolvedAddress::new(
        LIVE_KIND,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .map_err(|error| error.to_string())
}

/// The address of the conversation's `pi.usage` document.
pub fn usage_address(conversation: ConversationId) -> Result<ResolvedAddress, String> {
    ResolvedAddress::new(
        USAGE_KIND,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .map_err(|error| error.to_string())
}

fn doc_identity(kind: DocKind) -> DocDefinition {
    match kind {
        DocKind::Live => DocDefinition {
            kind: LIVE_KIND.to_owned(),
            version: LIVE_VERSION,
            semantics: DocumentSemantics::ConversationLatest {
                fork: DocumentFork::Initial,
            },
        },
        DocKind::Usage => DocDefinition {
            kind: USAGE_KIND.to_owned(),
            version: USAGE_VERSION,
            semantics: DocumentSemantics::ConversationLatest {
                fork: DocumentFork::Initial,
            },
        },
        DocKind::NestedCall => DocDefinition {
            kind: NESTED_CALL_KIND.to_owned(),
            version: NESTED_CALL_VERSION,
            semantics: DocumentSemantics::Task,
        },
    }
}

/// Split a committed read into the mutable draft and the loaded baseline the
/// plan diffs against.
///
/// The draft and the baseline must both own the value, so this is the one copy
/// a read costs: the record, the versions and the delta count move out of the
/// read, and the baseline keeps a clone of the map the caller edits.
fn split_stored(
    stored: StoredDocument,
    address_id: &str,
    version: i64,
) -> (JsonObject, LoadedDocument) {
    let StoredDocument {
        record,
        version: stored_version,
        value,
        deltas_since_base,
    } = stored;
    let loaded = LoadedDocument {
        address_id: address_id.to_owned(),
        record,
        stored_version,
        value_version: version,
        deltas_since_base,
        value: value.clone(),
    };
    (value, loaded)
}

/// One document read just before the commit that writes it.
///
/// A read is a read-modify-write: [`Draft::read`] pays exactly one copy of the
/// document (the draft, against the baseline the plan diffs), and
/// [`Draft::into_staged`] moves the draft into the transaction, so staging adds
/// no further copy.
struct Draft {
    address_id: String,
    address: DocumentAddress,
    value: JsonObject,
    created: bool,
    loaded: Option<LoadedDocument>,
    kind: DocKind,
    family: Option<DocFamilyDefinition>,
}

impl Draft {
    /// Read a conversation document, or start from its initial value.
    async fn read(
        runtime: &TaskRuntime,
        address: &ResolvedAddress,
        kind: DocKind,
    ) -> Result<Self, TaskRuntimeError> {
        let stored = runtime.document(address).await?;
        let version = match kind {
            DocKind::Live => LIVE_VERSION,
            DocKind::Usage => USAGE_VERSION,
            DocKind::NestedCall => NESTED_CALL_VERSION,
        };
        let (value, created, loaded) = match stored {
            Some(stored) => {
                let (value, loaded) = split_stored(stored, &address.id, version);
                (value, false, Some(loaded))
            }
            None => (
                if kind == DocKind::Usage {
                    usage_initial_value()
                } else {
                    JsonObject::new()
                },
                true,
                None,
            ),
        };
        Ok(Self {
            address_id: address.id.clone(),
            address: address.address.clone(),
            value,
            created,
            loaded,
            kind,
            family: None,
        })
    }

    /// Read one member of the caller's nested-call family, or start from `{}`.
    async fn read_member(
        runtime: &TaskRuntime,
        parent: TaskId,
        key: &str,
    ) -> Result<Self, TaskRuntimeError> {
        let address = nested_call_address(parent, key).map_err(TaskRuntimeError::Invalid)?;
        let stored = runtime.document(&address).await?;
        let (value, created, loaded) = match stored {
            Some(stored) => {
                let (value, loaded) = split_stored(stored, &address.id, NESTED_CALL_VERSION);
                (value, false, Some(loaded))
            }
            None => (JsonObject::new(), true, None),
        };
        Ok(Self {
            address_id: address.id.clone(),
            address: address.address.clone(),
            value,
            created,
            loaded,
            kind: DocKind::NestedCall,
            family: Some(nested_call_identity()),
        })
    }

    /// Move this draft into the transaction's staged document.
    fn into_staged(self) -> StagedDocument {
        StagedDocument {
            address: self.address,
            address_id: self.address_id,
            definition: Some(doc_identity(self.kind)),
            family: self.family,
            value: self.value,
            created: self.created,
            loaded: self.loaded,
            retire: false,
        }
    }

    /// Stage this draft on a transaction, moving its value and baseline.
    fn stage(self, tx: &mut Transaction) {
        tx.stage_document(self.into_staged());
    }
}

fn usage_initial_value() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("models".to_owned(), json!({}));
    value.insert("tools".to_owned(), json!({}));
    value
}

// ─── `pi.live` slot helpers on the raw document ─────────────────────────────

fn slot_mut(
    raw: &mut JsonObject,
    location: SlotLocation,
) -> Option<&mut serde_json::Map<String, Value>> {
    let (list, index) = match location {
        SlotLocation::Tools(index) => ("tools", index),
        SlotLocation::Nested(index) => ("nestedTools", index),
    };
    raw.get_mut(list)?
        .as_array_mut()?
        .get_mut(index)?
        .as_object_mut()
}

/// The call's slot as committed, for building a result from durable partial
/// output.
fn slot_progress(raw: &JsonObject, task_id: TaskId) -> Option<SlotProgress> {
    let state = live_from_object(raw);
    match find_tool_slot(&state, task_id)? {
        SlotLocation::Tools(index) => state.tools?.get(index).map(|slot| slot.progress.clone()),
        SlotLocation::Nested(index) => state
            .nested_tools?
            .get(index)
            .map(|slot| slot.progress.clone()),
    }
}

/// Upstream `removeNestedSlots`: drop every nested slot below `task_id`,
/// transitively. The parent's own tool slot stays.
fn remove_nested_slots(raw: &mut JsonObject, task_id: TaskId) {
    let Some(nested) = raw.get_mut("nestedTools").and_then(Value::as_array_mut) else {
        return;
    };
    let mut removed = vec![task_id];
    let mut index = 0usize;
    while index < nested.len() {
        let parent = nested[index]
            .get("parentTaskId")
            .and_then(Value::as_i64)
            .map(TaskId::from_raw);
        if parent.is_some_and(|parent| removed.contains(&parent)) {
            if let Some(child) = nested[index].get("taskId").and_then(Value::as_i64) {
                removed.push(TaskId::from_raw(child));
            }
            nested.remove(index);
        } else {
            index += 1;
        }
    }
    if nested.is_empty() {
        raw.remove("nestedTools");
    }
}

/// Mark a slot done and clear its running progress.
fn finish_slot(slot: &mut serde_json::Map<String, Value>) {
    slot.insert("status".to_owned(), Value::String("done".to_owned()));
    for key in [
        "output",
        "droppedBytes",
        "droppedLines",
        "details",
        "diagnostics",
    ] {
        slot.remove(key);
    }
}

// ─── Diagnostics and the tool-result entry ──────────────────────────────────

/// Render the diagnostics block the model sees at the end of a result.
#[must_use]
pub fn render_diagnostics(diagnostics: &[ToolDiagnostic]) -> String {
    let body = diagnostics
        .iter()
        .map(|diagnostic| format!("[{}] {}", diagnostic.severity, diagnostic.message))
        .collect::<Vec<_>>()
        .join("\n");
    format!("<harness>\n{body}\n</harness>")
}

/// Metadata of one appended result entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolResultMeta {
    /// Harness-clock milliseconds of the result.
    pub timestamp: i64,
    /// Execution time of the attempt that produced it.
    pub duration_ms: Option<u64>,
}

/// Append a `pi.tool-result` entry to the transcript.
///
/// The content ends with the rendered diagnostics, so the stored message is
/// exactly what the model sees; `data` keeps the structured list. The entry id
/// is passed in because a [`Transaction`] cannot mint one; the caller mints it
/// through its host first.
pub fn append_tool_result(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    entry_id: EntryId,
    call: &ToolCall,
    result: &ToolExecutionResult,
    meta: ToolResultMeta,
    by_task_id: Option<TaskId>,
) -> Result<EntryRecord, String> {
    let diagnostics = result.diagnostics.clone();
    let mut content = result.output.clone();
    if !diagnostics.is_empty() {
        content.push(ContentBlock::Text(TextContent {
            text: render_diagnostics(&diagnostics),
            text_signature: None,
        }));
    }
    let message = Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        details: result.details.clone(),
        usage: result.usage,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: result.is_error(),
        duration_ms: meta.duration_ms,
        timestamp: meta.timestamp.max(0) as u64,
    });
    let model = serde_json::to_value(&message).map_err(|error| error.to_string())?;
    let data = serde_json::to_value(ToolResultData {
        diagnostics: diagnostics
            .iter()
            .map(|diagnostic| serde_json::to_value(diagnostic).unwrap_or(Value::Null))
            .collect(),
    })
    .map_err(|error| error.to_string())?;
    let record = EntryRecord {
        id: entry_id,
        conversation_id,
        kind: ToolResultEntry::KIND.kind.to_owned(),
        model: Some(vec![model]),
        data: Some(data),
        head: None,
        edits: None,
        by_task_id,
    };
    tx.push_write(StorageWrite::Entry {
        value: record.clone(),
    });
    Ok(record)
}

/// Add `usage` to `pi.usage.tools[tool]` in the commit that stores the result.
/// A ledger whose stored value cannot be read is replaced rather than failing
/// the call.
fn record_tool_usage(value: &mut JsonObject, tool: &str, usage: &Usage) {
    let mut state: UsageState =
        serde_json::from_value(Value::Object(value.clone())).unwrap_or_else(|_| UsageState::new());
    usage_add(&mut state, UsageScope::Tools, tool, usage);
    if let Ok(Value::Object(updated)) = serde_json::to_value(&state) {
        *value = updated;
    }
}

// ─── `ToolHost`: the join points ────────────────────────────────────────────

/// A call as tool hooks see it; `parent` is set for a nested call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolHookCall {
    /// The call.
    pub call: ToolCall,
    /// The calling tool call, for a nested call.
    pub parent: Option<ParentCall>,
}

/// The calling tool call of a nested call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentCall {
    /// The calling tool task.
    pub task_id: TaskId,
    /// The calling tool call's id.
    pub call_id: String,
}

impl ToolHookCall {
    /// The call itself.
    #[must_use]
    pub fn as_call(&self) -> &ToolCall {
        &self.call
    }

    /// The call's id (`<parentCallId>/<key>` for a nested call).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.call.id
    }

    /// The call's tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.call.name
    }
}

/// What a `before_tool` hook decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolDecision {
    /// Replace the call's arguments.
    pub arguments: Option<JsonObject>,
    /// Block the call with this reason.
    pub block: Option<String>,
}

impl BeforeToolDecision {
    /// A decision that blocks the call.
    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            arguments: None,
            block: Some(reason.into()),
        }
    }

    /// A decision that replaces the call's arguments.
    #[must_use]
    pub fn with_arguments(arguments: JsonObject) -> Self {
        Self {
            arguments: Some(arguments),
            block: None,
        }
    }
}

/// What hooks may read: committed documents and the asking task's memos.
#[derive(Clone)]
pub struct ToolHookApi {
    runtime: TaskRuntime,
}

impl ToolHookApi {
    /// The task that asks.
    #[must_use]
    pub fn task_id(&self) -> TaskId {
        self.runtime.task_id()
    }

    /// The conversation.
    #[must_use]
    pub fn conversation_id(&self) -> ConversationId {
        self.runtime.conversation_id()
    }

    /// Read one memo.
    pub async fn memo(&self, name: &str, context: Context) -> Result<Option<Value>, String> {
        self.runtime
            .memo(name, context)
            .await
            .map_err(|error| error.to_string())
    }

    /// Read one committed document.
    pub async fn snapshot(&self, address: &ResolvedAddress) -> Result<Option<JsonObject>, String> {
        self.runtime
            .snapshot(address)
            .await
            .map_err(|error| error.to_string())
    }
}

/// A `beforeTool` hook. `Ok(None)` declines to decide.
pub type BeforeToolHook = Arc<
    dyn for<'a> Fn(
            ToolHookCall,
            &'a ToolHookApi,
            Context,
        ) -> BoxFuture<'a, Result<Option<BeforeToolDecision>, String>>
        + Send
        + Sync,
>;

/// An `afterTool` hook. `Ok(None)` keeps the result.
pub type AfterToolHook = Arc<
    dyn for<'a> Fn(
            ToolHookCall,
            ToolExecutionResult,
            &'a ToolHookApi,
            Context,
        ) -> BoxFuture<'a, Result<Option<ToolExecutionResult>, String>>
        + Send
        + Sync,
>;

/// Hooks of the built-in tool task, for model-issued and nested calls.
///
/// The hooks lane turns its opaque `HookRegistration` handlers into these; the
/// dispatch stays out of this module and the semantics stay in it.
#[derive(Clone, Default)]
pub struct ToolHooks {
    /// `beforeTool` handlers, in invocation order.
    pub before: Vec<BeforeToolHook>,
    /// `afterTool` handlers, in invocation order.
    pub after: Vec<AfterToolHook>,
}

impl ToolHooks {
    /// Whether any handler is installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty()
    }
}

/// Everything the tool task needs from the harness beside its runtime.
///
/// Every member has a default so a test can implement only what it exercises.
pub trait ToolHost: Send + Sync + 'static {
    /// The conversation's resolved agent, as the task's phase resolves it.
    fn agent<'a>(
        &'a self,
        runtime: &'a TaskRuntime,
        context: Context,
    ) -> BoxFuture<'a, Result<Agent, String>> {
        let _ = (runtime, context);
        Box::pin(async { Err("the tool host resolves no agent".to_owned()) })
    }

    /// One committed entry.
    fn entry<'a>(
        &'a self,
        runtime: &'a TaskRuntime,
        id: EntryId,
        context: Context,
    ) -> BoxFuture<'a, Result<Option<EntryRecord>, String>> {
        let _ = (runtime, id, context);
        Box::pin(async { Err("the tool host reads no entries".to_owned()) })
    }

    /// Repair a call's arguments before validation.
    fn prepare_arguments(
        &self,
        _tool: &ToolRegistration,
        arguments: JsonObject,
    ) -> Result<JsonObject, String> {
        Ok(arguments)
    }

    /// Validate (and coerce) a call's arguments against the tool's schema.
    ///
    /// The default accepts the arguments as given; the wave-3D adapter installs
    /// the preflight schema walk ([`crate::agent::tool_scheduler`]'s
    /// `validate_schema_value`).
    fn validate_arguments(
        &self,
        _tool: &ToolRegistration,
        _call: &ToolCall,
        arguments: &JsonObject,
    ) -> Result<JsonObject, String> {
        Ok(arguments.clone())
    }

    /// The output limits a call runs under.
    fn output_limits(&self, _tool: &ToolRegistration) -> OutputLimits {
        OutputLimits::default()
    }

    /// The tool's structured-output schema, when it declares one.
    fn structured_output_schema(&self, _tool: &ToolRegistration) -> Option<Value> {
        None
    }

    /// Why `value` breaks `schema`, or `None` when it keeps the contract.
    fn check_structured_output(&self, _schema: &Value, _value: &Value) -> Option<String> {
        None
    }

    /// Run the tool. An `Err` is a throw: the task ends `failed`.
    fn execute<'a>(
        &'a self,
        tool: &'a ToolRegistration,
        arguments: JsonObject,
        api: &'a ToolExecutionApi<'a>,
        context: Context,
    ) -> BoxFuture<'a, Result<ToolExecutionResult, String>> {
        let _ = (arguments, api, context);
        Box::pin(async move {
            Err(format!(
                "Tool {} has no implementation in this host",
                tool.name
            ))
        })
    }

    /// Mint one record id, outside the commit that uses it.
    ///
    /// A transaction cannot mint; a caller mints here and stores the id. A
    /// crash between the mint and the commit only wastes the id.
    fn mint_id<'a>(&'a self, runtime: &'a TaskRuntime) -> BoxFuture<'a, Result<i64, String>> {
        let _ = runtime;
        Box::pin(async { Err("the tool host mints no ids".to_owned()) })
    }

    /// The resolved settings, read at each use.
    fn settings(&self, _runtime: &TaskRuntime) -> Settings {
        Settings::default()
    }

    /// The host's telemetry handle for tool-side emissions.
    ///
    /// The tool task emits `pi.durable.replay` through this handle. The
    /// default carries no sink, so [`HarnessTelemetry::replay_answered`] is a
    /// branch and allocates nothing.
    fn telemetry(&self) -> HarnessTelemetry {
        HarnessTelemetry::none()
    }

    /// Record the `tool.name` / `tool.call_id` end attributes of the
    /// `pi.durable.phase` span a `pi.tool` handler ran under.
    ///
    /// `settle` calls this for every run and abort phase, with the span handle
    /// from the handler's [`Context`](crate::durable::context::Context) and
    /// the call's name and id. The scheduler owns the span and ends it in
    /// `SchedulerInner::finish_phase`; its
    /// [`HarnessTelemetry::end_phase_span`] call takes the pair as
    /// `tool_name` / `tool_call_id`. The default is a no-op, so a host without
    /// telemetry pays one virtual call.
    fn tool_phase_end(
        &self,
        _span: Option<SpanHandle>,
        _task_id: TaskId,
        _tool_name: &str,
        _call_id: &str,
    ) {
    }

    /// The `pi.tool` hooks the agent's extensions register.
    fn hooks(&self, _agent: &Agent) -> ToolHooks {
        ToolHooks::default()
    }

    /// The extension that registered each `afterTool` hook, parallel to
    /// [`ToolHooks::after`] as [`Self::hooks`] returns it.
    ///
    /// A failing `afterTool` hook is reported as an `extension_error`, and
    /// these names are what the report names (upstream
    /// `runtime.report(error, { extension })`). An implementor whose hooks
    /// come from
    /// [`hooks_for_task`](crate::durable::harness::agent::hooks_for_task)
    /// pairs each handler with its registration's extension name, in the same
    /// order as `ToolHooks::after`. The default leaves the identities unknown:
    /// the failure is still an `extension_error`, reported without a name.
    fn after_hook_extensions(&self, _agent: &Agent) -> Vec<Option<String>> {
        Vec::new()
    }
}

/// The `Any` carrier a task definition holds, so a phase handler can reach the
/// host again.
struct ToolHostHolder(Arc<dyn ToolHost>);

fn host_for(runtime: &TaskRuntime) -> Result<Arc<dyn ToolHost>, String> {
    let catalogue = runtime.registry();
    let definition = catalogue
        .task(TOOL_TASK_NAME)
        .ok_or_else(|| format!("Task definition {TOOL_TASK_NAME} is not installed"))?;
    let hooks = definition
        .hooks()
        .ok_or_else(|| format!("Task definition {TOOL_TASK_NAME} carries no host"))?;
    hooks
        .downcast::<ToolHostHolder>()
        .map(|holder| holder.0.clone())
        .map_err(|_| format!("Task definition {TOOL_TASK_NAME} carries a foreign host"))
}

// ─── The tool execution API ─────────────────────────────────────────────────

/// Options of a nested call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecuteToolOptions {
    /// An explicit key; defaults to the call's position among this
    /// invocation's nested calls.
    pub key: Option<String>,
    /// `false`: publish nothing to the slot while running.
    pub progress: Option<bool>,
}

/// What a tool reports while it runs.
struct Reported {
    output: OutputBuffer,
    limits: OutputLimits,
    diagnostics: Vec<ToolDiagnostic>,
    details: Option<Value>,
}

/// Operations available to one tool invocation.
///
/// Every asynchronous operation rejects after the invocation ends; the
/// synchronous ones (`output`, `diagnostic`, `retained_output`) panic with the
/// upstream message, which the scheduler turns into a task fault.
pub struct ToolExecutionApi<'a> {
    runtime: &'a TaskRuntime,
    host: &'a dyn ToolHost,
    input: &'a ToolTaskInput,
    call: &'a ToolHookCall,
    reported: Arc<Mutex<Reported>>,
    progress: &'a Progress,
    ended: &'a AtomicBool,
    sequence: AtomicU64,
    made_memo: tokio::sync::Mutex<()>,
    /// The host's telemetry handle, cloned once per execution. With no sink
    /// every emission is one branch and allocates nothing.
    telemetry: HarnessTelemetry,
}

impl<'a> ToolExecutionApi<'a> {
    fn assert_live(&self) -> Result<(), TaskRuntimeError> {
        if self.ended.load(Ordering::Acquire) {
            return Err(TaskRuntimeError::Invalid(format!(
                "Tool call {} has settled",
                self.call.id()
            )));
        }
        Ok(())
    }

    fn assert_live_sync(&self) {
        assert!(
            !self.ended.load(Ordering::Acquire),
            "Tool call {} has settled",
            self.call.id()
        );
    }

    /// The tool task.
    #[must_use]
    pub fn task_id(&self) -> TaskId {
        self.runtime.task_id()
    }

    /// The conversation.
    #[must_use]
    pub fn conversation_id(&self) -> ConversationId {
        self.runtime.conversation_id()
    }

    /// The call's id.
    #[must_use]
    pub fn call_id(&self) -> &str {
        self.call.id()
    }

    /// The task catalogue snapshot this call resolves against.
    #[must_use]
    pub fn registry(&self) -> TaskCatalogue {
        self.runtime.registry()
    }

    /// The host that owns this call.
    #[must_use]
    pub fn host(&self) -> &dyn ToolHost {
        self.host
    }

    /// The conversation's agent.
    pub async fn agent(&self, context: Context) -> Result<Agent, TaskRuntimeError> {
        self.assert_live()?;
        self.host
            .agent(self.runtime, context)
            .await
            .map_err(TaskRuntimeError::Invalid)
    }

    /// The harness clock.
    pub fn now(&self) -> Result<i64, TaskRuntimeError> {
        self.runtime.now()
    }

    /// Append running output; it becomes the result's `output` when the result
    /// omits it.
    pub fn output(&self, chunk: &str, skipped: Option<SkippedOutput>) {
        self.assert_live_sync();
        let pushed = {
            let mut reported = self
                .reported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reported.output.push(chunk, skipped)
        };
        if pushed {
            self.progress.mark();
        }
    }

    /// The tail this call's output keeps and the pace of its progress commits;
    /// `None` when the tool keeps the head of its output.
    #[must_use]
    pub fn output_window(&self) -> Option<OutputWindow> {
        let limits = self
            .reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .limits;
        if limits.retain != Retain::Tail {
            return None;
        }
        Some(OutputWindow {
            max_bytes: limits.max_bytes,
            max_lines: limits.max_lines,
            min_interval_ms: self.host.settings(self.runtime).progress.output_interval_ms,
            bytes_per_second: PROGRESS_BYTES_PER_SECOND,
        })
    }

    /// The output retained so far, as the model will see it when the result
    /// omits `output`, and whether earlier output was dropped.
    #[must_use]
    pub fn retained_output(&self) -> (String, bool) {
        self.assert_live_sync();
        let snapshot = self
            .reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .output
            .snapshot();
        (snapshot.text, snapshot.dropped_bytes > 0)
    }

    /// Record a model-visible remark about this call.
    pub fn diagnostic(&self, diagnostic: ToolDiagnostic) {
        self.assert_live_sync();
        {
            let mut reported = self
                .reported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reported.diagnostics.push(strict_diagnostic(diagnostic));
        }
        self.progress.mark();
    }

    /// Replace running details; the last value becomes the result details when
    /// the result omits `details`.
    pub async fn details(&self, value: Value, context: Context) -> Result<(), TaskRuntimeError> {
        self.assert_live()?;
        if context.is_aborted() {
            return Err(TaskRuntimeError::Invalid("aborted".to_owned()));
        }
        {
            let mut reported = self
                .reported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reported.details = Some(value);
        }
        let committed = self.progress.mark_and_wait();
        // Cancelling the wait leaves the update in place; the commit's own
        // outcome stays observed.
        tokio::select! {
            result = committed => {
                result
                    .map_err(|error| TaskRuntimeError::Invalid(error.to_string()))?
                    .map_err(TaskRuntimeError::Invalid)
            }
            () = context.cancelled() => Ok(()),
        }
    }

    /// Stage a change in one runtime commit and return its value.
    pub async fn commit<T, F>(&self, change: F, context: Context) -> Result<T, TaskRuntimeError>
    where
        T: Send + 'static,
        F: for<'b> FnOnce(&'b mut Transaction) -> BoxFuture<'b, Result<T, String>> + Send + 'static,
    {
        self.assert_live()?;
        let slot = Arc::new(Mutex::new(None::<T>));
        let slot_out = slot.clone();
        self.runtime
            .commit(
                move |tx, _record| {
                    Box::pin(async move {
                        let value = change(tx).await.map_err(TaskRuntimeError::Invalid)?;
                        *slot_out
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
                        Ok(None)
                    })
                },
                context,
            )
            .await?;
        let value = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        value.ok_or_else(|| TaskRuntimeError::Invalid("the commit produced no value".to_owned()))
    }

    /// Read one memo.
    pub async fn memo(
        &self,
        name: &str,
        context: Context,
    ) -> Result<Option<Value>, TaskRuntimeError> {
        self.assert_live()?;
        self.runtime.memo(name, context).await
    }

    /// First-writer-wins memo insertion in one commit.
    pub async fn memo_candidate(
        &self,
        name: &str,
        candidate: Value,
        context: Context,
    ) -> Result<Value, TaskRuntimeError> {
        self.assert_live()?;
        self.runtime
            .memo_candidate(name.to_owned(), candidate, context)
            .await
    }

    /// Create a child task and return its id.
    pub async fn create_task(
        &self,
        task: &Arc<dyn ErasedTask>,
        input: Value,
        context: Context,
    ) -> Result<TaskId, TaskRuntimeError> {
        self.assert_live()?;
        self.runtime.create_task(task, input, context).await
    }

    /// One committed task record.
    pub async fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>, TaskRuntimeError> {
        self.assert_live()?;
        self.runtime.get_task(id).await
    }

    /// Wait for a task this call owns to become terminal.
    pub async fn wait_for_task(&self, id: TaskId) -> Result<TaskRecord, TaskRuntimeError> {
        self.assert_live()?;
        wait_for_terminal(self.runtime, id).await
    }

    /// One committed document value.
    pub async fn snapshot(
        &self,
        address: &ResolvedAddress,
    ) -> Result<Option<JsonObject>, TaskRuntimeError> {
        self.assert_live()?;
        self.runtime.snapshot(address).await
    }

    /// Watch one document incarnation.
    pub async fn watch_doc<F>(
        &self,
        address: &ResolvedAddress,
        listener: F,
    ) -> Result<crate::durable::harness::scheduler::DocumentWatch, TaskRuntimeError>
    where
        F: Fn(&JsonObject) -> bool + Send + Sync + 'static,
    {
        self.assert_live()?;
        self.runtime.watch_doc(address, listener).await
    }

    /// Run tool `name` as a nested call of this one and wait for its result.
    ///
    /// The nested call is its own `pi.tool` task, owned by this call: it
    /// resolves the tool among the conversation's callable tools, validates,
    /// runs the tool hooks (which see `call.parent`), applies output limits and
    /// the tool's replay policy, and reports progress in `pi.live.nestedTools`.
    pub async fn execute_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        options: ExecuteToolOptions,
        context: Context,
    ) -> Result<NestedToolExecutionResult, TaskRuntimeError> {
        self.assert_live()?;
        let key = match options.key {
            Some(key) => {
                check_key(&key).map_err(TaskRuntimeError::Invalid)?;
                key
            }
            None => (self.sequence.fetch_add(1, Ordering::SeqCst) + 1).to_string(),
        };
        // The memo first, once per attempt; a failed one is tried again by the
        // next call.
        {
            let _guard = self.made_memo.lock().await;
            self.runtime
                .memo_candidate(
                    MADE_NESTED_CALLS.to_owned(),
                    Value::Bool(true),
                    context.clone(),
                )
                .await?;
        }
        let progress = options.progress.unwrap_or(self.input.streams_progress());
        let (id, replayed) = start_nested_call(
            self.runtime,
            name,
            arguments,
            NestedAdmission {
                parent_call_id: self.call.id().to_owned(),
                key: key.clone(),
                progress,
            },
            context.clone(),
        )
        .await?;
        if replayed {
            // The member already held this key's result before the call was
            // admitted: `pi.durable.replay`. Observational only; the event
            // attaches to the calling phase's span.
            self.telemetry
                .replay_answered(context.span_handle().filter(|span| !span.is_none()), name);
        }
        let settled = wait_for_terminal(self.runtime, id).await?;
        let address =
            nested_call_address(self.runtime.task_id(), &key).map_err(TaskRuntimeError::Invalid)?;
        let stored = self
            .runtime
            .snapshot(&address)
            .await?
            .and_then(|value| value.get("result").cloned());
        match stored {
            Some(result) => serde_json::from_value(result).map_err(|error| {
                TaskRuntimeError::Invalid(format!(
                    "Nested call {key} stored an invalid result: {error}"
                ))
            }),
            // A copy the tool may change; a nested call the scheduler faulted
            // or orphaned stored none.
            None => Ok(fallback_result(id, name, &settled.state)),
        }
    }
}

/// Drop a diagnostic's empty code, matching `omitUndefinedProperties`.
fn strict_diagnostic(mut diagnostic: ToolDiagnostic) -> ToolDiagnostic {
    if diagnostic.code.is_none() {
        diagnostic.code = None;
    }
    diagnostic
}

/// First and maximum interval between [`wait_for_terminal`] checks.
///
/// The scheduler's event-driven `wait_for_task` is not reachable from a tool's
/// [`TaskRuntime`] (its handle is private to the scheduler module), so the
/// child's committed record is the only observable. The interval starts short
/// (a fast nested call is observed within one tick) and DOUBLES up to the cap:
/// a long-running child (a minutes-long subagent or shell task) costs one
/// committed read per half second instead of twenty per second — the poll is
/// only active while a nested call is in flight, never while idle.
const TERMINAL_POLL_INITIAL: Duration = Duration::from_millis(50);
const TERMINAL_POLL_MAX: Duration = Duration::from_millis(500);

/// Wait for a task to become terminal.
///
/// [`TaskRuntime`] has no arbitrary-task wait on its scheduler handle, so this
/// polls the committed record with an ADAPTIVE interval (see
/// [`TERMINAL_POLL_INITIAL`]). The sleep races [`TaskRuntime::cancelled`] so an
/// abort or an invocation end wakes the loop immediately; the interval only
/// bounds how long a completion may take to be observed.
async fn wait_for_terminal(
    runtime: &TaskRuntime,
    id: TaskId,
) -> Result<TaskRecord, TaskRuntimeError> {
    let mut interval = TERMINAL_POLL_INITIAL;
    loop {
        let Some(record) = runtime.get_task(id).await? else {
            return Err(TaskRuntimeError::Invalid(format!(
                "Task {id} does not exist"
            )));
        };
        if record.state.is_terminal() {
            return Ok(record);
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            // The invocation ended: stop waiting now instead of sleeping out
            // the interval, keeping abort/cancellation responsive.
            () = runtime.cancelled() => {}
        }
        interval = (interval * 2).min(TERMINAL_POLL_MAX);
        // Rejects after the invocation ends.
        runtime.now()?;
    }
}

/// What the caller gets for a nested call without a stored result: one the
/// scheduler faulted or orphaned.
fn fallback_result(task_id: TaskId, name: &str, state: &TaskState) -> NestedToolExecutionResult {
    let (code, message) = match state {
        TaskState::Terminal { outcome } | TaskState::Completing { outcome } => match outcome {
            TaskOutcome::Faulted { error } => {
                ("faulted", format!("Tool {name} failed: {}", error.message))
            }
            TaskOutcome::Orphaned { reason } => (
                "orphaned",
                format!("Tool {name} could not resume: {reason}"),
            ),
            _ => ("no_result", format!("Tool {name} ended without a result")),
        },
        _ => ("no_result", format!("Tool {name} ended without a result")),
    };
    let error = harness_error(code, message);
    NestedToolExecutionResult {
        task_id,
        structured_output: None,
        is_error: true,
        details: None,
        diagnostics: error.diagnostics,
        usage: None,
        duration_ms: None,
    }
}
// ─── The task ───────────────────────────────────────────────────────────────

/// Initial checkpoint of a new tool task.
fn tool_initial(_input: &Value) -> Value {
    ToolTaskCheckpoint::Call.to_value()
}

/// Version 1 had only model-issued calls; version 2 adds `kind` to the input.
fn migrate_tool_task(_from: i64, input: &Value, checkpoint: &Value) -> Result<Migrated, String> {
    let input = ToolTaskInput::from_value(input)?;
    Ok(Migrated {
        input: input.to_value(),
        checkpoint: ToolTaskCheckpoint::from_value(checkpoint).to_value(),
    })
}

/// Build the built-in `pi.tool` task around `host`.
///
/// Version 2, phases `call` and `execute`, one abort handler.
pub fn tool_task(host: Arc<dyn ToolHost>) -> Result<Task, TaskDefinitionError> {
    define_task(TOOL_TASK_NAME, TOOL_TASK_VERSION, tool_initial)
        .phase("call", phase_handler(run_call_phase))
        .phase("execute", phase_handler(run_execute_phase))
        .abort(phase_handler(run_abort_phase))
        .migrate_with_input(migrate_tool_task)
        .hooks(Arc::new(ToolHostHolder(host)))
        .build()
}

/// `call`: resolve, validate, hook, record intent, execute, settle.
fn run_call_phase<'a>(
    task: &'a RunningTask,
    runtime: &'a TaskRuntime,
    context: Context,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        let host = host_or_panic(runtime);
        let input = input_or_panic(&task.input);
        let call = read_call_or_panic(runtime, host.as_ref(), &input, context.clone()).await;
        let tool = match resolve_tool(runtime, host.as_ref(), &input, call.name(), context.clone())
            .await
        {
            Ok(Some(tool)) => tool,
            Ok(None) => {
                let error = harness_error(
                    "tool_unavailable",
                    format!("Tool {} is not available", call.name()),
                );
                settle(
                    runtime,
                    host.as_ref(),
                    &input,
                    &call,
                    Ending::Completed,
                    error,
                    context,
                    None,
                )
                .await;
                return;
            }
            Err(message) => panic!("{message}"),
        };
        let mut args = match prepare_and_validate(
            host.as_ref(),
            &tool,
            &call,
            arguments_object(&call.as_call().arguments),
        ) {
            Ok(arguments) => arguments,
            Err(message) => {
                let error = harness_error("invalid_arguments", message);
                settle(
                    runtime,
                    host.as_ref(),
                    &input,
                    &call,
                    Ending::Completed,
                    error,
                    context,
                    None,
                )
                .await;
                return;
            }
        };
        // `beforeTool`: the first block wins, otherwise `arguments` replace the
        // call's arguments; a throw blocks.
        let hooks = hooks_for(runtime, host.as_ref(), context.clone()).await;
        let api = ToolHookApi {
            runtime: runtime.clone(),
        };
        let mut block: Option<String> = None;
        for hook in &hooks.hooks.before {
            if block.is_some() {
                break;
            }
            match hook(call.clone(), &api, context.clone()).await {
                Ok(Some(decision)) => {
                    if decision.block.is_some() {
                        block = decision.block;
                    } else if let Some(arguments) = decision.arguments {
                        args = arguments;
                    }
                }
                Ok(None) => {}
                Err(message) => {
                    if runtime.signal().is_aborted() {
                        panic!("{message}");
                    }
                    block = Some(message);
                }
            }
        }
        if let Some(reason) = block {
            let error = harness_error("blocked", format!("Tool call blocked: {reason}"));
            settle(
                runtime,
                host.as_ref(),
                &input,
                &call,
                Ending::Completed,
                error,
                context,
                None,
            )
            .await;
            return;
        }
        let final_args = match prepare_and_validate(host.as_ref(), &tool, &call, args) {
            Ok(arguments) => strict_object(arguments),
            Err(message) => {
                let error = harness_error("invalid_arguments", message);
                settle(
                    runtime,
                    host.as_ref(),
                    &input,
                    &call,
                    Ending::Completed,
                    error,
                    context,
                    None,
                )
                .await;
                return;
            }
        };
        let replay = tool.replay;
        // A nested slot shows the arguments the call runs with; compared with
        // the input the slot got at admission.
        let changed = match &input {
            ToolTaskInput::Nested { call: original, .. } => !json_equal(
                &Value::Object(arguments_object(&original.arguments)),
                &Value::Object(final_args.clone()),
            ),
            ToolTaskInput::Model { .. } => false,
        };
        let intent = ToolTaskCheckpoint::Execute {
            arguments: final_args.clone(),
            replay,
        }
        .to_value();
        // Read before the commit: a transaction cannot read storage.
        let mut draft = match read_live(runtime, runtime.conversation_id()).await {
            Ok(draft) => draft,
            Err(error) => panic!("{error}"),
        };
        let task_id = runtime.task_id();
        if let Some(location) = find_tool_slot(&live_from_object(&draft.value), task_id) {
            if let Some(slot) = slot_mut(&mut draft.value, location) {
                slot.insert("status".to_owned(), Value::String("running".to_owned()));
                if changed && slot.contains_key("parentTaskId") {
                    slot.insert("arguments".to_owned(), Value::Object(final_args.clone()));
                }
            }
        }
        if let Err(error) = stage_draft(
            runtime,
            draft,
            Some(NextTaskState::Running { checkpoint: intent }),
            context.clone(),
        )
        .await
        {
            panic!("{error}");
        }
        run(
            runtime,
            host.as_ref(),
            &input,
            &call,
            &tool,
            final_args,
            context,
        )
        .await;
    })
}

/// `execute`: recovery after intent; rerun only when the stored and current
/// policy both say `safe`.
fn run_execute_phase<'a>(
    task: &'a RunningTask,
    runtime: &'a TaskRuntime,
    context: Context,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        let host = host_or_panic(runtime);
        let input = input_or_panic(&task.input);
        let (args, replay) = match ToolTaskCheckpoint::from_value(&task.checkpoint) {
            ToolTaskCheckpoint::Execute { arguments, replay } => (arguments, replay),
            // Not the phase a `call` checkpoint runs; resolve it again.
            ToolTaskCheckpoint::Call => {
                return run_call_phase(task, runtime, context).await;
            }
        };
        let call = read_call_or_panic(runtime, host.as_ref(), &input, context.clone()).await;
        let tool = match resolve_tool(runtime, host.as_ref(), &input, call.name(), context.clone())
            .await
        {
            Ok(tool) => tool,
            Err(message) => panic!("{message}"),
        };
        let replay_safe_tool = if replay == ReplayPolicy::Safe {
            tool.as_ref()
                .filter(|tool| tool.replay == ReplayPolicy::Safe)
        } else {
            None
        };
        if let Some(tool) = replay_safe_tool {
            // The rerun reports from scratch; clear what the interrupted attempt
            // published.
            let task_id = runtime.task_id();
            if let Ok(mut draft) = read_live(runtime, runtime.conversation_id()).await {
                if let Some(location) = find_tool_slot(&live_from_object(&draft.value), task_id) {
                    if let Some(slot) = slot_mut(&mut draft.value, location) {
                        clear_progress_raw(slot);
                    }
                }
                let _ = stage_draft(runtime, draft, None, context.clone()).await;
            }
            run(runtime, host.as_ref(), &input, &call, tool, args, context).await;
            return;
        }
        let message = format!(
            "Tool {} was interrupted and may have partially run",
            call.name()
        );
        // `failed` records cancellation intent, so the call's owned
        // conversations, left unsupervised, are aborted.
        // The result comes from the slot's durable partial.
        let slot = read_live(runtime, runtime.conversation_id())
            .await
            .ok()
            .and_then(|draft| slot_progress(&draft.value, runtime.task_id()));
        let result = from_slot(slot.as_ref(), "interrupted", &message);
        settle(
            runtime,
            host.as_ref(),
            &input,
            &call,
            Ending::Failed {
                message: message.clone(),
            },
            result,
            context,
            None,
        )
        .await;
    })
}

/// `abort`: settle, saying whether the call may have run.
fn run_abort_phase<'a>(
    task: &'a RunningTask,
    runtime: &'a TaskRuntime,
    context: Context,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        let host = host_or_panic(runtime);
        let input = input_or_panic(&task.input);
        let call = read_call_or_panic(runtime, host.as_ref(), &input, context.clone()).await;
        let name = call.name();
        let (code, message) = if task.abort_reason.as_deref() != Some("restart") {
            ("aborted", format!("Tool {name} was aborted"))
        } else if matches!(
            ToolTaskCheckpoint::from_value(&task.checkpoint),
            ToolTaskCheckpoint::Execute { .. }
        ) {
            (
                "interrupted",
                format!("Tool {name} was interrupted by a restart and may have partially run"),
            )
        } else {
            (
                "abandoned",
                format!("Tool {name} was not started: its caller ended with a restart"),
            )
        };
        let slot = read_live(runtime, runtime.conversation_id())
            .await
            .ok()
            .and_then(|draft| slot_progress(&draft.value, runtime.task_id()));
        let result = from_slot(slot.as_ref(), code, &message);
        settle(
            runtime,
            host.as_ref(),
            &input,
            &call,
            Ending::Aborted,
            result,
            context,
            None,
        )
        .await;
    })
}

fn host_or_panic(runtime: &TaskRuntime) -> Arc<dyn ToolHost> {
    match host_for(runtime) {
        Ok(host) => host,
        Err(message) => panic!("{message}"),
    }
}

fn input_or_panic(value: &Value) -> ToolTaskInput {
    match ToolTaskInput::from_value(value) {
        Ok(input) => input,
        Err(message) => panic!("{message}"),
    }
}

/// Remove what a tool published while running.
fn clear_progress_raw(slot: &mut serde_json::Map<String, Value>) {
    for key in [
        "output",
        "droppedBytes",
        "droppedLines",
        "details",
        "diagnostics",
    ] {
        slot.remove(key);
    }
}

// ─── Reading the call and resolving the tool ────────────────────────────────

/// The call: from the input for a nested call, which hooks see with its parent,
/// else from the assistant entry.
async fn read_call(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    input: &ToolTaskInput,
    context: Context,
) -> Result<ToolHookCall, String> {
    if let ToolTaskInput::Nested {
        parent,
        parent_call_id,
        call,
        ..
    } = input
    {
        return Ok(ToolHookCall {
            call: call.clone(),
            parent: Some(ParentCall {
                task_id: *parent,
                call_id: parent_call_id.clone(),
            }),
        });
    }
    let ToolTaskInput::Model { assistant, call_id } = input else {
        unreachable!("every non-nested input is model-issued");
    };
    let entry = host.entry(runtime, *assistant, context).await?;
    let message = match entry
        .as_ref()
        .and_then(|entry| entry.model.as_ref())
        .and_then(|model| model.first())
    {
        Some(value) => Some(Message::deserialize(value).map_err(|error| {
            format!(
                "Entry {} has an unreadable model message: {error}",
                assistant.as_i64()
            )
        })?),
        None => None,
    };
    let call = message.and_then(|message| match message {
        Message::Assistant(assistant) => {
            assistant.content.into_iter().find_map(|block| match block {
                ContentBlock::ToolCall(call) if call.id == *call_id => Some(call),
                _ => None,
            })
        }
        _ => None,
    });
    match call {
        Some(call) => Ok(ToolHookCall { call, parent: None }),
        None => Err(format!(
            "Entry {} has no tool call {call_id}",
            assistant.as_i64()
        )),
    }
}

async fn read_call_or_panic(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    input: &ToolTaskInput,
    context: Context,
) -> ToolHookCall {
    match read_call(runtime, host, input, context).await {
        Ok(call) => call,
        Err(message) => panic!("{message}"),
    }
}

/// The called tool: among the tools the model is offered for a model-issued
/// call, else among those tools may call.
async fn resolve_tool(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    input: &ToolTaskInput,
    name: &str,
    context: Context,
) -> Result<Option<ToolRegistration>, String> {
    let agent = host.agent(runtime, context).await?;
    let found = if input.is_nested() {
        agent.callable.iter().find(|tool| tool.name == name)
    } else {
        agent.tools.iter().find(|tool| tool.name == name)
    };
    Ok(found.cloned())
}

/// Repair, then validate and coerce a call's arguments.
fn prepare_and_validate(
    host: &dyn ToolHost,
    tool: &ToolRegistration,
    call: &ToolHookCall,
    arguments: JsonObject,
) -> Result<JsonObject, String> {
    let prepared = host.prepare_arguments(tool, arguments)?;
    host.validate_arguments(tool, call.as_call(), &prepared)
}

/// Strict JSON: drop absent values, which the slot and the checkpoint cannot
/// hold.
fn strict_object(value: JsonObject) -> JsonObject {
    match serde_json::to_value(Value::Object(value)) {
        Ok(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

/// The `pi.tool` hooks the conversation's agent registers, with the extension
/// behind each `afterTool` handler.
#[derive(Default)]
struct ResolvedToolHooks {
    hooks: ToolHooks,
    /// Parallel to `hooks.after`; a short list leaves the tail unknown.
    after_extensions: Vec<Option<String>>,
}

async fn hooks_for(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    context: Context,
) -> ResolvedToolHooks {
    match host.agent(runtime, context).await {
        Ok(agent) => ResolvedToolHooks {
            hooks: host.hooks(&agent),
            after_extensions: host.after_hook_extensions(&agent),
        },
        Err(_) => ResolvedToolHooks::default(),
    }
}

/// Read the conversation's `pi.live` document just before a commit writes it.
async fn read_live(
    runtime: &TaskRuntime,
    conversation: ConversationId,
) -> Result<Draft, TaskRuntimeError> {
    let address = live_address(conversation).map_err(TaskRuntimeError::Invalid)?;
    Draft::read(runtime, &address, DocKind::Live).await
}

// ─── Executing and settling ─────────────────────────────────────────────────

/// How a tool task ends; the result entry is appended either way.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ending {
    /// The call ran to a result, error or not.
    Completed,
    /// A caller aborted it.
    Aborted,
    /// Execution threw or was interrupted; records cancellation intent.
    Failed {
        /// Why the task ended `failed`.
        message: String,
    },
}

/// Execute with the resolved implementation, then settle its result.
async fn run(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    tool: &ToolRegistration,
    args: JsonObject,
    context: Context,
) {
    let limits = host.output_limits(tool);
    let reported = Arc::new(Mutex::new(Reported {
        output: OutputBuffer::new(limits),
        limits,
        diagnostics: Vec::new(),
        details: None,
    }));
    let streams = input.streams_progress();
    let ended = Arc::new(AtomicBool::new(false));
    let interval = host.settings(runtime).progress.output_interval_ms;
    let progress = publish_progress(
        runtime,
        reported.clone(),
        streams,
        interval,
        context.clone(),
    );

    let api = ToolExecutionApi {
        runtime,
        host,
        input,
        call,
        reported: reported.clone(),
        progress: &progress,
        ended: &ended,
        sequence: AtomicU64::new(0),
        made_memo: tokio::sync::Mutex::new(()),
        telemetry: host.telemetry(),
    };

    let mut ending = Ending::Completed;
    let started = Instant::now();
    let executed = AssertUnwindSafe(host.execute(tool, args, &api, context.clone()))
        .catch_unwind()
        .await;
    let duration_ms = Some(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
    let result = match executed {
        Ok(Ok(result)) => result,
        Ok(Err(message)) => {
            // A throw ends the task `failed`, which cancels what the call owns;
            // it no longer supervises it. The error text is already in the
            // result.
            ending = Ending::Failed {
                message: format!("Tool {} threw", call.name()),
            };
            ToolExecutionResult {
                output: Vec::new(),
                structured_output: None,
                is_error: Some(true),
                details: None,
                diagnostics: vec![tool_diagnostic("tool_error", message)],
                usage: None,
                control: None,
            }
        }
        Err(payload) => {
            if runtime.signal().is_aborted() {
                let message = panic_message(payload);
                let pending = progress.stop().await;
                ended.store(true, Ordering::Release);
                for waiter in pending {
                    let _ = waiter.send(Err(message.clone()));
                }
                panic!("{message}");
            }
            ending = Ending::Failed {
                message: format!("Tool {} threw", call.name()),
            };
            ToolExecutionResult {
                output: Vec::new(),
                structured_output: None,
                is_error: Some(true),
                details: None,
                diagnostics: vec![tool_diagnostic("tool_error", panic_message(payload))],
                usage: None,
                control: None,
            }
        }
    };
    ended.store(true, Ordering::Release);
    {
        let mut reported = reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reported.output.end();
    }
    // Details still waiting for a progress commit settle with the terminal
    // commit, the final flush.
    let pending = progress.stop().await;
    let snapshot = {
        let reported = reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ReportedSnapshot {
            output: reported.output.snapshot(),
            limits: reported.limits,
            diagnostics: reported.diagnostics.clone(),
            details: reported.details.clone(),
        }
    };
    let settled = final_result(
        runtime,
        host,
        input,
        call,
        tool,
        result,
        &snapshot,
        context.clone(),
    )
    .await;
    settle(
        runtime,
        host,
        input,
        call,
        ending,
        settled,
        context,
        duration_ms,
    )
    .await;
    for waiter in pending {
        let _ = waiter.send(Ok(()));
    }
}

fn utf8_len(text: &str) -> u64 {
    text.len() as u64
}

fn details_json(details: Option<&Value>) -> String {
    serde_json::to_string(&details.cloned().unwrap_or(Value::Null))
        .unwrap_or_else(|_| "null".to_owned())
}

/// What the last progress commit wrote.
#[derive(Clone, Default)]
struct Written {
    text: String,
    details: Option<Value>,
    diagnostics: usize,
}

/// What a running tool reported through its api.
#[derive(Clone)]
struct ReportedSnapshot {
    output: BoundedOutput,
    limits: OutputLimits,
    diagnostics: Vec<ToolDiagnostic>,
    details: Option<Value>,
}

/// Throttled commits of what the tool reported into its slot, in `pi.live`.
/// Each writes only what changed since the last one; none when `enabled` is
/// false.
fn publish_progress(
    runtime: &TaskRuntime,
    reported: Arc<Mutex<Reported>>,
    enabled: bool,
    interval_ms: u64,
    context: Context,
) -> Progress {
    let runtime_for_write = runtime.clone();
    let written = Arc::new(Mutex::new(Written::default()));
    let write: Arc<dyn Fn() -> BoxFuture<'static, Result<u64, String>> + Send + Sync> =
        Arc::new(move || {
            let runtime = runtime_for_write.clone();
            let reported = reported.clone();
            let written = written.clone();
            let context = context.clone();
            Box::pin(async move {
                if !enabled {
                    return Ok(0);
                }
                // Capture everything synchronously: the tool keeps reporting
                // while the commit is in flight.
                let previous = written
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let (snapshot, current, added) = {
                    let reported = reported
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let snapshot = reported.output.snapshot();
                    let current = Written {
                        text: snapshot.text.clone(),
                        details: reported.details.clone(),
                        diagnostics: reported.diagnostics.len(),
                    };
                    let added = reported
                        .diagnostics
                        .get(previous.diagnostics.min(reported.diagnostics.len())..)
                        .unwrap_or_default()
                        .to_vec();
                    (snapshot, current, added)
                };
                let details_changed = current.details != previous.details;
                // What the commit writes, as the differ sees the string: an
                // append, or a trim plus the text after the shared part.
                let mut bytes = 0u64;
                if current.text != previous.text {
                    let shared = if current.text.starts_with(previous.text.as_str()) {
                        previous.text.len()
                    } else {
                        overlap(&previous.text, &current.text, 65_536)
                    };
                    let from = shared.min(current.text.len());
                    bytes += utf8_len(&current.text[from..]);
                }
                if details_changed {
                    bytes += utf8_len(&details_json(current.details.as_ref()));
                }
                if !added.is_empty() {
                    bytes += utf8_len(
                        &serde_json::to_string(&added).unwrap_or_else(|_| "[]".to_owned()),
                    );
                }
                let conversation = runtime.conversation_id();
                let task_id = runtime.task_id();
                let mut draft = read_live(&runtime, conversation)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(location) = find_tool_slot(&live_from_object(&draft.value), task_id) {
                    if let Some(slot) = slot_mut(&mut draft.value, location) {
                        // REMINDER: assign `output` as one string field, so the
                        // differ sees an append rather than the whole window.
                        if slot.get("output").and_then(Value::as_str).unwrap_or("") != current.text
                        {
                            slot.insert("output".to_owned(), Value::String(current.text.clone()));
                        }
                        if snapshot.dropped_bytes > 0 {
                            slot.insert(
                                "droppedBytes".to_owned(),
                                Value::from(snapshot.dropped_bytes),
                            );
                        }
                        if snapshot.dropped_lines > 0 {
                            slot.insert(
                                "droppedLines".to_owned(),
                                Value::from(snapshot.dropped_lines),
                            );
                        }
                        // Details leaf by leaf, and new diagnostics appended.
                        if details_changed {
                            match current.details.clone() {
                                Some(details) => assign_json(slot, "details", details),
                                None => {
                                    slot.remove("details");
                                }
                            }
                        }
                        if !added.is_empty() {
                            let list = slot
                                .entry("diagnostics".to_owned())
                                .or_insert_with(|| Value::Array(Vec::new()));
                            if let Some(list) = list.as_array_mut() {
                                for diagnostic in &added {
                                    if let Ok(value) = serde_json::to_value(diagnostic) {
                                        list.push(value);
                                    }
                                }
                            }
                        }
                    }
                }
                stage_draft(&runtime, draft, None, context).await?;
                *written
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = current;
                Ok(bytes)
            })
        });
    let on_error: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_message: String| {});
    // Upstream's `progressClock()`: after the invocation ends nothing more
    // commits, and the clock answers with the last time it read.
    let now = {
        let runtime = runtime.clone();
        let last = std::sync::atomic::AtomicI64::new(0);
        Arc::new(move || {
            if let Ok(value) = runtime.now() {
                last.store(value, Ordering::SeqCst);
            }
            last.load(Ordering::SeqCst)
        }) as Arc<dyn Fn() -> i64 + Send + Sync>
    };
    Progress::new(write, on_error, interval_ms, now)
}

/// Stage one draft on one runtime commit, optionally with the task's next
/// state.
async fn stage_draft(
    runtime: &TaskRuntime,
    draft: Draft,
    next: Option<NextTaskState>,
    context: Context,
) -> Result<(), String> {
    runtime
        .commit(
            move |tx, _record| {
                Box::pin(async move {
                    draft.stage(tx);
                    Ok(next)
                })
            },
            context,
        )
        .await
        .map_err(|error| error.to_string())
}

// ─── The settled result ─────────────────────────────────────────────────────

/// What programs get of a tool's output when it declares no schema: one text
/// item as its string, one image as itself, nothing as `""`, and anything else
/// as the content list.
#[must_use]
pub fn output_value(output: &[ContentBlock]) -> Value {
    match output {
        [] => Value::String(String::new()),
        [only] => match only {
            ContentBlock::Text(text) => Value::String(text.text.clone()),
            other => serde_json::to_value(other).unwrap_or(Value::Null),
        },
        many => serde_json::to_value(many).unwrap_or(Value::Null),
    }
}

/// Why a result's `structured_output` breaks the tool's contract, or `None` when
/// it keeps it.
fn structured_output_error(
    host: &dyn ToolHost,
    tool: &ToolRegistration,
    result: &ToolExecutionResult,
) -> Option<String> {
    let schema = host.structured_output_schema(tool);
    let value = result.structured_output.as_ref();
    match (schema, value) {
        (None, None) => None,
        (None, Some(_)) => Some(format!(
            "Tool {} returned structuredOutput but declares no structuredOutputSchema",
            tool.name
        )),
        (Some(_), None) => {
            if result.is_error() {
                None
            } else {
                Some(format!("Tool {} returned no structuredOutput", tool.name))
            }
        }
        (Some(schema), Some(value)) => host.check_structured_output(&schema, value).map(|why| {
            format!(
                "Tool {} returned structuredOutput that does not match its schema: {why}",
                tool.name
            )
        }),
    }
}

/// The settled result: the tool's result with the retained output and last
/// details as fallbacks, its diagnostics after those reported through the api,
/// `afterTool` applied, explicit text bounded, and `structured_output` checked
/// against the tool's schema, with the Harness's diagnostics last.
///
/// A nested call whose `structured_output` breaks the contract gets an error
/// result without it; a model-issued call only loses it, and the break is
/// reported to the host.
#[allow(clippy::too_many_arguments)]
async fn final_result(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    tool: &ToolRegistration,
    result: ToolExecutionResult,
    reported: &ReportedSnapshot,
    context: Context,
) -> ToolExecutionResult {
    let mut harness: Vec<ToolDiagnostic> = Vec::new();
    let retained = if result.output.is_empty() {
        Some(reported.output.clone())
    } else {
        None
    };
    let output: Vec<ContentBlock> = match &retained {
        None => result.output.clone(),
        Some(retained) if retained.text.is_empty() => Vec::new(),
        Some(retained) => vec![ContentBlock::Text(TextContent {
            text: retained.text.clone(),
            text_signature: None,
        })],
    };
    let mut diagnostics = reported.diagnostics.clone();
    diagnostics.extend(result.diagnostics.iter().cloned());
    let mut settled = ToolExecutionResult {
        output: output.clone(),
        structured_output: result.structured_output.clone(),
        is_error: result.is_error,
        details: result.details.clone().or_else(|| reported.details.clone()),
        diagnostics,
        usage: result.usage,
        control: result.control.clone(),
    };
    let hooks = hooks_for(runtime, host, context.clone()).await;
    let api = ToolHookApi {
        runtime: runtime.clone(),
    };
    for (index, hook) in hooks.hooks.after.iter().enumerate() {
        match hook(call.clone(), settled.clone(), &api, context.clone()).await {
            Ok(Some(replaced)) => settled = replaced,
            Ok(None) => {}
            Err(message) => {
                if runtime.signal().is_aborted() {
                    panic!("{message}");
                }
                // The hook belongs to an extension, so the failure is an
                // `extension_error` naming it (upstream
                // `runtime.report(error, { extension })`), not a `task_error`
                // about the task's own code.
                let extension = hooks.after_extensions.get(index).cloned().flatten();
                runtime
                    .report_from(
                        format!("afterTool hook failed: {message}"),
                        Some(ReportFrom::Extension { extension }),
                    )
                    .unwrap_or(());
            }
        }
    }
    // The retained output's truncation applies only while `afterTool` kept that
    // output.
    if let Some(retained) = &retained {
        if retained.dropped_bytes > 0 && settled.output == output {
            harness.push(truncated_diagnostic(
                retained.dropped_bytes,
                retained.dropped_lines,
                Some(reported.limits.retain),
            ));
        }
    }
    let bounded = bound_content(&settled.output, reported.limits);
    if bounded.dropped_bytes > 0 {
        harness.push(truncated_diagnostic(
            bounded.dropped_bytes,
            bounded.dropped_lines,
            Some(reported.limits.retain),
        ));
    }
    if let Some(broken) = structured_output_error(host, tool, &settled) {
        settled.structured_output = None;
        if input.is_nested() {
            settled.is_error = Some(true);
            harness.push(tool_diagnostic("invalid_structured_output", broken));
        } else {
            // The model never sees structured output, so its call stands; the
            // host learns of the broken tool. Only names the extension: a
            // failed resolution leaves the report without the name.
            let extension = host
                .agent(runtime, context)
                .await
                .ok()
                .and_then(|agent| tool_extension(&agent, &tool.name).map(str::to_owned));
            runtime
                .report_from(broken, Some(ReportFrom::Extension { extension }))
                .unwrap_or(());
        }
    }
    // Programs get the tool's own value: without a schema, its bounded output.
    // The model reads only `output`, so model-issued calls skip both.
    if input.is_nested() && host.structured_output_schema(tool).is_none() {
        settled.structured_output = Some(output_value(&bounded.content));
    }
    // A schema tool's error result without structured output has its text only
    // in the output, which programs never get: it becomes an error diagnostic,
    // as a thrown error's message does, unless one says what went wrong already.
    if input.is_nested()
        && host.structured_output_schema(tool).is_some()
        && settled.is_error()
        && settled.structured_output.is_none()
        && !settled
            .diagnostics
            .iter()
            .chain(harness.iter())
            .any(|diagnostic| diagnostic.severity == "error")
    {
        let text = bounded
            .content
            .iter()
            .filter_map(|item| match item {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            harness.push(tool_diagnostic("tool_error", text));
        }
    }
    settled.output = bounded.content;
    settled.diagnostics.extend(harness);
    settled
}

/// Bound the text of result content. When the joined text exceeds the limits,
/// the text items are replaced by one bounded item at the position of the first
/// (head) or last (tail) text item; other content is kept.
struct BoundedContent {
    content: Vec<ContentBlock>,
    dropped_bytes: u64,
    dropped_lines: u64,
}

fn bound_content(content: &[ContentBlock], limits: OutputLimits) -> BoundedContent {
    let joined = content
        .iter()
        .filter_map(|item| match item {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let bounded = bound_output(&joined, limits);
    if bounded.dropped_bytes == 0 {
        return BoundedContent {
            content: content.to_vec(),
            dropped_bytes: 0,
            dropped_lines: 0,
        };
    }
    let keep = match limits.retain {
        Retain::Head => content
            .iter()
            .position(|item| matches!(item, ContentBlock::Text(_))),
        Retain::Tail => content
            .iter()
            .rposition(|item| matches!(item, ContentBlock::Text(_))),
    };
    let mut result = Vec::with_capacity(content.len());
    for (index, item) in content.iter().enumerate() {
        match item {
            ContentBlock::Text(text) if Some(index) == keep => {
                result.push(ContentBlock::Text(TextContent {
                    text: bounded.text.clone(),
                    text_signature: text.text_signature.clone(),
                }));
            }
            ContentBlock::Text(_) => {}
            other => result.push(other.clone()),
        }
    }
    BoundedContent {
        content: result,
        dropped_bytes: bounded.dropped_bytes,
        dropped_lines: bounded.dropped_lines,
    }
}

// ─── Settling ───────────────────────────────────────────────────────────────

/// A nested call's result, as stored and returned: strict JSON, without
/// `output`, which only the model reads, and without `control`, which only
/// model-issued calls apply.
#[must_use]
pub fn nested_result(
    task_id: TaskId,
    result: &ToolExecutionResult,
    duration_ms: Option<u64>,
) -> NestedToolExecutionResult {
    NestedToolExecutionResult {
        task_id,
        structured_output: result.structured_output.clone(),
        is_error: result.is_error(),
        details: result.details.clone(),
        diagnostics: result.diagnostics.clone(),
        usage: result.usage,
        duration_ms,
    }
}

/// How a nested call ended, for its slot.
#[must_use]
pub fn summary_of(
    result: &NestedToolExecutionResult,
    output: &[ContentBlock],
) -> NestedToolSummary {
    let error = if result.is_error {
        let mut text = error_text_of(result, output);
        if text.chars().count() > MAX_SUMMARY_ERROR_CHARS {
            text = text.chars().take(MAX_SUMMARY_ERROR_CHARS).collect();
        }
        text
    } else {
        String::new()
    };
    NestedToolSummary {
        is_error: result.is_error,
        duration_ms: result.duration_ms,
        usage: result.usage,
        error: (!error.is_empty()).then_some(error),
    }
}

/// The error messages of a nested result, else the text items of its output.
fn error_text_of(result: &NestedToolExecutionResult, output: &[ContentBlock]) -> String {
    let errors: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == "error")
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    if !errors.is_empty() {
        return errors.join("\n");
    }
    output
        .iter()
        .filter_map(|item| match item {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A document read the settling commit depends on.
///
/// A read failure faults the task (the scheduler catches the panic and settles
/// it `faulted`); treating the document as absent would silently skip aborting
/// the call's nested tasks, marking its slot done, or recording its usage.
/// Only a successful read is a value: none of these reads has an absent case
/// (a missing document reads as its initial value).
fn settle_read<T>(read: Result<T, TaskRuntimeError>, what: &str) -> T {
    match read {
        Ok(value) => value,
        Err(error) => panic!("settle could not read {what}: {error}"),
    }
}

/// Commit the tool's terminal state and mark its slot done.
///
/// A model-issued call appends its result entry and ends with the entry ID; a
/// nested call writes its result to the caller's family member, records its
/// usage, and ends with a small receipt. Nested calls this call left running are
/// aborted first, so they report into their slots before the slots below the
/// call leave `pi.live`.
#[allow(clippy::too_many_arguments)]
async fn settle(
    runtime: &TaskRuntime,
    host: &dyn ToolHost,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    ending: Ending,
    result: ToolExecutionResult,
    context: Context,
    duration_ms: Option<u64>,
) {
    let task_id = runtime.task_id();
    let conversation = runtime.conversation_id();
    // Every live nested call the task owns, also those an earlier attempt made
    // and this one never called again. Mark them all before waiting for any: one
    // may run until a sibling is cancelled.
    let owned = settle_read(runtime.owned_tasks().await, "the task's nested calls");
    let nested_ids: Vec<TaskId> = owned
        .iter()
        .filter(|record| {
            record.kind == TOOL_TASK_NAME
                && ToolTaskInput::from_value(&record.input).is_ok_and(|input| input.is_nested())
        })
        .map(|record| record.id)
        .collect();
    for id in &nested_ids {
        let _ = runtime.abort_owned(*id, context.clone()).await;
    }
    let made_nested = runtime
        .memo(MADE_NESTED_CALLS, context.clone())
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let timestamp = runtime.now().unwrap_or(0);
    let entry_id = if input.is_nested() {
        None
    } else {
        match host.mint_id(runtime).await {
            Ok(id) => Some(EntryId::from_raw(id)),
            Err(message) => panic!("{message}"),
        }
    };

    // Every document the settling commit writes is read here: a transaction
    // cannot read storage.
    let partial = result;
    let mut live_draft = Some(settle_read(
        read_live(runtime, conversation).await,
        "pi.live",
    ));
    let mut member = match input {
        ToolTaskInput::Nested { parent, key, .. } => Some(settle_read(
            Draft::read_member(runtime, *parent, key).await,
            "the nested-call family member",
        )),
        ToolTaskInput::Model { .. } => None,
    };
    let mut usage_draft = match partial.usage {
        Some(_) => match usage_address(conversation) {
            Ok(address) => Some(settle_read(
                Draft::read(runtime, &address, DocKind::Usage).await,
                "pi.usage",
            )),
            Err(_) => None,
        },
        None => None,
    };
    let nested = input
        .is_nested()
        .then(|| nested_result(task_id, &partial, duration_ms));

    let outcome = match &ending {
        Ending::Completed => TaskOutcome::Completed {
            result: Value::Null,
        },
        Ending::Aborted => TaskOutcome::Aborted {
            reason: None,
            result: None,
        },
        Ending::Failed { message } => TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: message.clone(),
                detail: None,
            },
            result: None,
        },
    };
    let task_result = match (input, entry_id) {
        (ToolTaskInput::Nested { .. }, _) => ToolTaskResult::Nested,
        (ToolTaskInput::Model { .. }, Some(entry_id)) => ToolTaskResult::Model {
            entry_id,
            // Tools build control objects freely; a call that did not complete
            // does not apply them.
            control: if matches!(ending, Ending::Completed) {
                strict_control(partial.control.clone())
            } else {
                None
            },
        },
        (ToolTaskInput::Model { .. }, None) => ToolTaskResult::Nested,
    };
    let task_result_value = task_result.to_value();
    // The commit closure is `'static`: borrow nothing from the phase.
    let nested_input = input.is_nested();
    let call = call.as_call().clone();
    let call_name = call.name.clone();
    // Phase-end telemetry: the scheduler ends the phase span after this
    // handler returns, so hand it the tool identity while the resolved call is
    // still in hand. Skipped entirely (no sink, no attributes) when the host
    // has no telemetry.
    if host.telemetry().enabled() {
        let span = context.span_handle().filter(|span| !span.is_none());
        host.tool_phase_end(span, task_id, &call.name, &call.id);
    }

    let commit = runtime
        .commit(
            move |tx, _record| {
                Box::pin(async move {
                    if let Some(mut live) = live_draft.take() {
                        if let Some(location) =
                            find_tool_slot(&live_from_object(&live.value), task_id)
                        {
                            if let Some(slot) = slot_mut(&mut live.value, location) {
                                let nested_slot = slot.contains_key("parentTaskId");
                                if nested_slot == nested_input {
                                    finish_slot(slot);
                                    match (&nested, nested_slot) {
                                        (Some(nested), true) => {
                                            if let Ok(summary) = serde_json::to_value(summary_of(
                                                nested,
                                                &partial.output,
                                            )) {
                                                slot.insert("summary".to_owned(), summary);
                                            }
                                        }
                                        (_, false) => {
                                            if let Some(entry_id) = entry_id {
                                                slot.insert(
                                                    "entry".to_owned(),
                                                    Value::from(entry_id.as_i64()),
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if made_nested {
                            remove_nested_slots(&mut live.value, task_id);
                        }
                        live.stage(tx);
                    }
                    if let (Some(mut member), Some(nested)) = (member.take(), nested.as_ref()) {
                        if let Ok(value) = serde_json::to_value(nested) {
                            member.value.insert("result".to_owned(), value);
                        }
                        member.stage(tx);
                    }
                    if let (Some(mut usage), Some(spend)) = (usage_draft.take(), partial.usage) {
                        record_tool_usage(&mut usage.value, &call_name, &spend);
                        usage.stage(tx);
                    }
                    if let (Some(entry_id), false) = (entry_id, nested_input) {
                        if let Err(message) = append_tool_result(
                            tx,
                            conversation,
                            entry_id,
                            &call,
                            &partial,
                            ToolResultMeta {
                                timestamp,
                                duration_ms,
                            },
                            Some(task_id),
                        ) {
                            return Err(TaskRuntimeError::Invalid(message));
                        }
                    }
                    let outcome = fill_outcome_result(outcome, task_result_value.clone());
                    Ok(Some(NextTaskState::Terminal { outcome }))
                })
            },
            context,
        )
        .await;
    if let Err(error) = commit {
        panic!("{error}");
    }
}

fn fill_outcome_result(outcome: TaskOutcome, task_result: Value) -> TaskOutcome {
    match outcome {
        TaskOutcome::Completed { .. } => TaskOutcome::Completed {
            result: task_result,
        },
        TaskOutcome::Aborted { reason, .. } => TaskOutcome::Aborted {
            reason,
            result: Some(task_result),
        },
        TaskOutcome::Failed { error, .. } => TaskOutcome::Failed {
            error,
            result: Some(task_result),
        },
        other => other,
    }
}

/// Drop a control object's absent keys, so the task result is strict JSON.
fn strict_control(control: Option<ToolControl>) -> Option<ToolControl> {
    control.and_then(|control| {
        let value = serde_json::to_value(&control).ok()?;
        serde_json::from_value(value).ok()
    })
}

/// A nested call's admission.
struct NestedAdmission {
    parent_call_id: String,
    key: String,
    progress: bool,
}

/// Create nested call `key` of the call `parent_call_id`.
///
/// A key already made returns that call's task when it names the same tool and
/// arguments. The child task is created first, because a transaction cannot mint
/// its id; the caller's index member and the nested slot follow in a second
/// commit.
///
/// The boolean is true when the key was already made *and* its member held the
/// result: this call is answered from that stored result (`pi.durable.replay`),
/// not by waiting for a fresh execution.
async fn start_nested_call(
    runtime: &TaskRuntime,
    name: &str,
    args: JsonObject,
    admission: NestedAdmission,
    context: Context,
) -> Result<(TaskId, bool), TaskRuntimeError> {
    let key = admission.key;
    let call = ToolCall {
        id: format!("{}/{key}", admission.parent_call_id),
        name: name.to_owned(),
        arguments: object_arguments(&args),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    };
    let task_id = runtime.task_id();
    // Opened, or checked as already made.
    let mut member = Draft::read_member(runtime, task_id, &key).await?;
    if let Some(existing) = member
        .value
        .get("taskId")
        .and_then(Value::as_i64)
        .map(TaskId::from_raw)
    {
        let record = runtime.get_task(existing).await?;
        let same = record
            .as_ref()
            .and_then(|record| ToolTaskInput::from_value(&record.input).ok())
            .is_some_and(|input| match input {
                ToolTaskInput::Nested { call: original, .. } => {
                    original.name == name
                        && json_equal(
                            &Value::Object(arguments_object(&original.arguments)),
                            &Value::Object(arguments_object(&call.arguments)),
                        )
                }
                ToolTaskInput::Model { .. } => false,
            });
        if !same {
            return Err(TaskRuntimeError::Invalid(format!(
                "Nested call {} was already made with another tool or other arguments",
                call.id
            )));
        }
        let replayed = member.value.get("result").is_some();
        return Ok((existing, replayed));
    }
    let catalogue = runtime.registry();
    let definition = catalogue.task(TOOL_TASK_NAME).ok_or_else(|| {
        TaskRuntimeError::Invalid(format!("Task definition {TOOL_TASK_NAME} is not installed"))
    })?;
    let input = ToolTaskInput::Nested {
        parent: task_id,
        parent_call_id: admission.parent_call_id.clone(),
        key: key.clone(),
        call: call.clone(),
        progress: admission.progress,
    };
    let created = runtime
        .create_task(&definition, input.to_value(), context.clone())
        .await?;
    // The index member and the slot, in one commit.
    let conversation = runtime.conversation_id();
    member
        .value
        .insert("taskId".to_owned(), Value::from(created.as_i64()));
    let mut live_draft = read_live(runtime, conversation).await?;
    let slot = json!({
        "callId": call.id,
        "parentCallId": admission.parent_call_id,
        "parentTaskId": task_id.as_i64(),
        "name": name,
        "taskId": created.as_i64(),
        "arguments": Value::Object(arguments_object(&call.arguments)),
        "status": "pending",
    });
    let list = live_draft
        .value
        .entry("nestedTools".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(list) = list.as_array_mut() {
        list.push(slot);
    }
    runtime
        .commit(
            move |tx, _record| {
                Box::pin(async move {
                    member.stage(tx);
                    live_draft.stage(tx);
                    Ok(None)
                })
            },
            context,
        )
        .await?;
    Ok((created, false))
}

/// Explicit nested call keys are path segments of the call ID, so IDs of
/// different nested calls never collide, and are never plain positive integers,
/// which default keys use. `__proto__` stays reserved.
pub fn check_key(key: &str) -> Result<(), String> {
    let positive_integer = !key.is_empty()
        && key.as_bytes()[0] != b'0'
        && key.bytes().all(|byte| byte.is_ascii_digit());
    if key.is_empty() || key.contains('/') || key == "__proto__" || positive_integer {
        return Err(format!(
            "Nested call key {key:?} must be non-empty, without \"/\", not \"__proto__\", and not a positive integer"
        ));
    }
    Ok(())
}
