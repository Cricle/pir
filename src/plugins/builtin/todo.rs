//! The `todo` builtin — the npm `@juicesharp/rpiv-todo` package (v2.12.0)
//! ported natively: the model-facing `todo` tool, the `/todos` slash command,
//! and transcript-backed task-list persistence.
//!
//! Upstream tracking point: `@juicesharp/rpiv-todo` 2.12.0 (installed at
//! `~/.pi/agent/npm/node_modules/@juicesharp/rpiv-todo`). Behaviour truth:
//! `docs/tool-schema.md` (the tool contract — actions, parameters, status
//! machine, dependency edges, the response envelope, and the exact error
//! strings), `todo.ts` (registration + `/todos`), `state/state-reducer.ts`,
//! `state/invariants.ts`, `state/task-graph.ts`, `state/replay.ts`,
//! `state/store.ts`, `tool/{types,response-envelope,sanitize}.ts`, and
//! `view/format.ts`. See `docs/todo-builtin.md` for the full deviation table.
//!
//! ## The contract (hard)
//!
//! - One tool named `todo` with six actions: `create`, `update`, `list`,
//!   `get`, `delete`, `clear`; one command `/todos`.
//! - Task status machine: `pending → in_progress|completed|deleted`,
//!   `in_progress → pending|completed|deleted`, `completed → deleted`,
//!   `deleted` terminal; a same→same transition is an accepted no-op.
//! - `delete` tombstones (the task stays for historic `blockedBy` edges);
//!   `clear` drops everything and resets the id counter to 1.
//! - Dependency edges: `blockedBy` (validated: dangling, tombstoned,
//!   self-block, cycle), `addBlockedBy`/`removeBlockedBy` additive on update,
//!   reverse `blocks` edges derived on `get`.
//! - `metadata` merges key-by-key; a `null` value deletes that key; an empty
//!   record drops the field entirely.
//! - The response envelope is `{content, details}` with
//!   `details = {action, params, tasks, nextId, error?}` — the persistence
//!   format (every successful call embeds the full post-mutation snapshot).
//! - Rejections are in-band: `content[0].text = "Error: <message>"` and
//!   `details.error = <message>`, with the task state untouched.
//!
//! ## Persistence: replay from the session transcript
//!
//! Upstream rebuilds the list by walking the session branch and taking the
//! LAST `todo` tool result's `details` snapshot (last-write-wins) on
//! `session_start` / `session_compact` / `session_tree`. pir maps that to
//! [`AgentPlugin::on_session_context`], whose
//! [`SessionContextSnapshot::entries`](crate::extensions::SessionContextSnapshot)
//! carries the serialized transcript (`Vec<Message>` JSON) plus the session
//! id; the host pushes it before every prompt and after every run. The
//! `tool_result` transcript message carries `toolName: "todo"` and the
//! envelope under `details`, so the same filter works unchanged
//! ([`replay_from_entries`]).
//!
//! ## Live state does not depend on any event
//!
//! Two seams own the state, and neither is the `tool_result` event:
//!
//! 1. **The tool call itself.** [`TodoTool::run`] reaches
//!    [`TodoStore::execute`], which applies the mutation to the active
//!    session's slot and builds the envelope. This is upstream's
//!    `execute() → commitState` seam (upstream's `tool_execution_end` handler
//!    only repaints the overlay).
//! 2. **Transcript replay** in [`TodoPlugin::on_session_context`] (above),
//!    which rebuilds the slot from the LAST `todo` result in the serialized
//!    transcript.
//!
//! The `tool_result` EVENT is a defensive second source only, never a
//! dependency. [`TodoPlugin::events`] declares exactly `session_start` — the
//! declaration doubles as the agent-loop fan-out switch
//! (`has_native_handlers("tool_result")`), and declaring it would clone every
//! batch result's payload only to re-apply state this plugin already
//! committed. Under that declaration the host's `emit` / `emit_native` /
//! `emit_read_only_json` all skip this plugin for `tool_result`
//! (`native_plugin_wants_event` in [`crate::plugins::host`]); that skip is
//! INTENDED and changes nothing observable, because no state depends on the
//! delivery. The `tool_result` match arm in [`TodoPlugin::on_event`] stays as
//! an idempotent whole-state REPLACE for a dispatcher that invokes
//! `on_event` directly — the hosted result walk in
//! [`crate::agent::tool_execution::dispatch_hosted_event`] does exactly that
//! once another plugin turns that path on — but it is never required for
//! correctness. `tool_call` is deliberately NOT consumed: it fires before
//! execution and carries only input, so applying it there would double-apply
//! the call.
//!
//! Replay is only attempted when it can change something:
//!
//! - the push carries no `entries` array (a model-only host) → nothing;
//! - the entries' fingerprint equals the one the slot was replayed from →
//!   nothing (this is what makes a live commit survive a subsequent
//!   unchanged-transcript push, and keeps the idempotence of the
//!   pre-prompt/post-run pair to one scan per run);
//! - otherwise → a cheap fingerprint pass over the entries followed by ONE
//!   deserialization of the last matching snapshot; the state is REPLACED
//!   (never merged) when the fingerprint moved.
//!
//! ## Memory / CPU
//!
//! - [`TodoPlugin::events`] lists only `session_start`, so the host's
//!   `has_handlers` gate keeps `message_update` (and every other unrelated
//!   event, including the per-batch `tool_result` fan-out) off the plugin's
//!   dispatch path; the handler matches on the event name first and touches
//!   no state for anything else. Zero per-delta and zero per-result work.
//!   `PluginHost::emit` and `emit_native` honor the declaration and skip this
//!   plugin for `tool_result`; only a caller that invokes `on_event` directly
//!   (the hosted result walk in
//!   [`crate::agent::tool_execution::dispatch_hosted_event`], when another
//!   plugin turned that path on) still reaches the defensive branch. The
//!   plugin never switches the agent loop's hosted result path on.
//! - Per-session state is O(tasks). Records are bounded:
//!   [`MAX_TASKS_PER_SESSION`], [`MAX_METADATA_BYTES`],
//!   [`MAX_SUBJECT_CHARS`], [`MAX_DESCRIPTION_CHARS`],
//!   [`MAX_ACTIVE_FORM_CHARS`], [`MAX_OWNER_CHARS`].
//! - The sessions map is bounded (LRU, [`MAX_SESSION_SLOTS`]) and cleared on
//!   `on_shutdown`.
//! - A replay pass is one O(entries) filtering pass plus an allocation-free
//!   structural hash of the matching `details`; an unchanged push performs
//!   ZERO deserializations, and a moved fingerprint exactly ONE O(tasks)
//!   deserialization (the last matching snapshot). The pass runs at push
//!   frequency (≤2 per run), never per streamed delta.
//! - The reducer mutates in place after validating (no whole-state clone);
//!   only the task being updated is cloned for the no-op comparison.
//!
//! ## v1 scope not done (documented, not defects)
//!
//! - The TUI panel and the `ctrl+shift+t` collapse/expand behaviour: the
//!   shortcut is REGISTERED (name + description) so a UI lane can bind it;
//!   there is no overlay renderer, and pir's TUI does not dispatch Rust
//!   builtin shortcuts today.
//! - i18n: English only, like the upstream English fallbacks.
//! - The `~/.config/rpiv-todo/config.json` settings (`maxWidgetLines`,
//!   `collapseKey`, `guidance`): not read (no overlay rows to budget, no
//!   shortcut to rebind). The built-in prompt snippet/guidelines are the ones
//!   upstream ships as defaults.
//! - `/todos` text is returned through
//!   [`CommandOutcome::error`](crate::extensions::CommandOutcome), the only
//!   host-visible text channel a Rust command has today (the `mcp` builtin's
//!   precedent). Print mode prints it; the TUI currently renders extension
//!   command text only for failed outcomes and does not list Rust builtin
//!   commands as extension commands, so `/todos` is not surfaced there yet —
//!   the same host gap `/mcp` has.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map as JsonMap, Value as JsonValue};

use crate::agent::tool_batch::{ToolExecutionMode, ToolResult};
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::extensions::{CommandOutcome, SessionContextSnapshot};
use crate::plugins::plugin::{
    AgentPlugin, CommandBackend, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture,
    RegistrationContext, ShortcutBackend,
};
use crate::plugins::source::PluginInfo;

// ---------------------------------------------------------------------------
// Identity — verbatim string boundaries. `todo` is the persistence key: the
// replay filter matches `toolName == "todo"` in the transcript, so the name
// must never change.
// ---------------------------------------------------------------------------

/// The registered tool name (upstream `TOOL_NAME`).
pub const TOOL_NAME: &str = "todo";
/// The tool's display label (upstream `TOOL_LABEL`).
pub const TOOL_LABEL: &str = "Todo";
/// The slash command (upstream `COMMAND_NAME`).
pub const COMMAND_NAME: &str = "todos";
/// `/todos` in a session without a UI (upstream `ERR_REQUIRES_INTERACTIVE`).
pub const ERR_REQUIRES_INTERACTIVE: &str = "/todos requires interactive mode";
/// `/todos` on an empty list (upstream `MSG_NO_TODOS`).
pub const MSG_NO_TODOS: &str = "No todos yet. Ask the agent to add some!";

/// Upstream `DEFAULT_PROMPT_SNIPPET`.
pub const DEFAULT_PROMPT_SNIPPET: &str = "Manage a task list to track multi-step progress";

/// Upstream `DEFAULT_PROMPT_GUIDELINES` (eight bullets, verbatim).
pub const DEFAULT_PROMPT_GUIDELINES: [&str; 8] = [
    "Use `todo` for complex work with 3+ steps, when the user gives you a list of tasks, or immediately after receiving new instructions to capture requirements. Skip it for single trivial tasks and purely conversational requests.",
    "When starting a task from the todo list, mark it in_progress BEFORE beginning work. Mark it completed IMMEDIATELY when done — never batch completions. Exactly one task in_progress at a time.",
    "Never mark a task completed if tests are failing, the implementation is partial, or you hit unresolved errors — keep it in_progress and create a new task for the blocker instead.",
    "Task status is a 4-state machine: pending → in_progress → completed, plus deleted as a tombstone. Pass activeForm (present-continuous label, e.g. 'researching existing tool') when marking in_progress.",
    "To change a task's status, call update with the task id and the target status, e.g. {\"action\":\"update\",\"id\":3,\"status\":\"completed\"} or {\"action\":\"update\",\"id\":3,\"status\":\"in_progress\",\"activeForm\":\"writing tests\"}. status is the field that changes the task; an update without a mutable field (status or another) is rejected.",
    "Use blockedBy to express dependencies (A is blocked by B). On create, pass blockedBy as the initial set. On update, use addBlockedBy / removeBlockedBy (additive merge — do not resend the full array). Cycles are rejected.",
    "list hides tombstoned (deleted) tasks by default; pass includeDeleted:true to see them. Pass status to filter by a single status.",
    "Subject must be short and imperative (e.g. 'Research existing tool'); description is for long-form detail. activeForm is a present-continuous label shown while in_progress.",
];

/// The tool description the model sees (upstream `registerTodoTool`'s
/// `description`, verbatim).
pub const TOOL_DESCRIPTION: &str = "Manage a task list for tracking multi-step progress. Actions: create (new task), update (change status/fields/dependencies), list (all tasks, optionally filtered by status), get (single task details), delete (tombstone), clear (reset all). Status: pending → in_progress → completed, plus deleted tombstone. Use this to plan and track multi-step work like research, design, and implementation.";

/// Maximum tasks per session (`create` past this is rejected in-band).
/// Upstream is unbounded; the cap is what makes "state is O(tasks) with
/// bounded records" a bounded-memory guarantee — see `docs/todo-builtin.md`.
pub const MAX_TASKS_PER_SESSION: usize = 1000;
/// Maximum serialized size of one task's `metadata` (bytes).
pub const MAX_METADATA_BYTES: usize = 16 * 1024;
/// Maximum `subject` length (characters).
pub const MAX_SUBJECT_CHARS: usize = 1024;
/// Maximum `description` length (characters).
pub const MAX_DESCRIPTION_CHARS: usize = 8 * 1024;
/// Maximum `activeForm` length (characters).
pub const MAX_ACTIVE_FORM_CHARS: usize = 256;
/// Maximum `owner` length (characters).
pub const MAX_OWNER_CHARS: usize = 256;
/// Maximum session slots kept in memory (LRU). One session host has one
/// plugin instance, so this only bounds hosts that reuse one instance across
/// session switches.
pub const MAX_SESSION_SLOTS: usize = 8;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Task status (upstream `TaskStatus`). Same→same is an accepted no-op, so
/// this enum carries every state a task can be asked to enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl TaskStatus {
    /// The wire string (also the `details.tasks[].status` value).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Deleted => "deleted",
        }
    }

    /// Parse a wire string (`None` for anything else).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            "deleted" => Some(TaskStatus::Deleted),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The six tool actions (upstream `TaskAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskAction {
    Create,
    Update,
    List,
    Get,
    Delete,
    Clear,
}

impl TaskAction {
    /// The wire string (`details.action`).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskAction::Create => "create",
            TaskAction::Update => "update",
            TaskAction::List => "list",
            TaskAction::Get => "get",
            TaskAction::Delete => "delete",
            TaskAction::Clear => "clear",
        }
    }

    /// Parse a wire string (`None` for anything else).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "create" => Some(TaskAction::Create),
            "update" => Some(TaskAction::Update),
            "list" => Some(TaskAction::List),
            "get" => Some(TaskAction::Get),
            "delete" => Some(TaskAction::Delete),
            "clear" => Some(TaskAction::Clear),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One task. Field order matches upstream's `Task` interface declaration
/// (`id, subject, description?, activeForm?, status, blockedBy?, owner?,
/// metadata?`); optional fields are omitted from the wire when absent, which
/// is what makes a replayed snapshot byte-comparable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Task {
    pub id: u64,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        rename = "activeForm",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
    pub status: TaskStatus,
    #[serde(rename = "blockedBy", default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Vec<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap<String, JsonValue>>,
}

/// Canonical per-session state (upstream `TaskState`): the task list plus the
/// next id to assign. No derived caches live here; selectors are pure
/// functions over it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoState {
    pub tasks: Vec<Task>,
    #[serde(rename = "nextId")]
    pub next_id: u64,
}

impl Default for TodoState {
    fn default() -> Self {
        Self::empty()
    }
}

impl TodoState {
    /// Fresh empty state (`nextId: 1`), upstream `EMPTY_STATE`.
    pub fn empty() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }
}

/// Reducer outcome (upstream `Op`). Closed tagged union: the response
/// envelope's formatter switches over it exhaustively.
#[derive(Debug, Clone, PartialEq)]
pub enum TodoOp {
    Create {
        task_id: u64,
    },
    Update {
        id: u64,
        from: TaskStatus,
        to: TaskStatus,
        changed: bool,
    },
    Delete {
        id: u64,
        subject: String,
    },
    List {
        status_filter: Option<TaskStatus>,
        include_deleted: bool,
    },
    /// The `get` action carries only the id; the task is resolved from the
    /// state at format time (upstream's `create` branch does the same), which
    /// keeps the enum's largest variant small.
    Get {
        id: u64,
    },
    Clear {
        count: usize,
    },
    Error {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Status machine + dependency graph (state/invariants.ts, state/task-graph.ts)
// ---------------------------------------------------------------------------

/// Whether `from → to` is legal. A transition to the current status is always
/// accepted (upstream `isTransitionValid`).
pub fn is_transition_valid(from: TaskStatus, to: TaskStatus) -> bool {
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        (TaskStatus::Pending, TaskStatus::InProgress)
            | (TaskStatus::Pending, TaskStatus::Completed)
            | (TaskStatus::Pending, TaskStatus::Deleted)
            | (TaskStatus::InProgress, TaskStatus::Pending)
            | (TaskStatus::InProgress, TaskStatus::Completed)
            | (TaskStatus::InProgress, TaskStatus::Deleted)
            | (TaskStatus::Completed, TaskStatus::Deleted)
    )
}

/// Whether merging `new_blocked_by` into `task_id`'s edges would close a
/// cycle (upstream `detectCycle`). Iterative DFS (explicit stack) so a
/// 1000-task chain can never overflow the call stack; O(V+E).
pub fn detect_cycle(tasks: &[Task], task_id: u64, new_blocked_by: &[u64]) -> bool {
    let index: BTreeMap<u64, usize> = tasks
        .iter()
        .enumerate()
        .map(|(position, task)| (task.id, position))
        .collect();
    let mut adjacency: Vec<Vec<usize>> = Vec::with_capacity(tasks.len());
    for task in tasks {
        let mut deps: Vec<u64> = task.blocked_by.clone().unwrap_or_default();
        if task.id == task_id {
            for dep in new_blocked_by {
                if !deps.contains(dep) {
                    deps.push(*dep);
                }
            }
        }
        // Edges to ids that are not tasks (dangling) carry no out-edges, like
        // upstream's absent map entries.
        adjacency.push(
            deps.iter()
                .filter_map(|dep| index.get(dep).copied())
                .collect(),
        );
    }

    // 0 = white (unvisited), 1 = gray (on stack), 2 = black (done).
    let mut color = vec![0_u8; tasks.len()];
    // The loop variable is the DFS root index, not a slice cursor: both
    // `color` and `adjacency` are indexed by it, so the range form is the
    // clearest shape here.
    #[allow(clippy::needless_range_loop)]
    for start in 0..tasks.len() {
        if color[start] != 0 {
            continue;
        }
        color[start] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(node, cursor)) = stack.last() {
            if cursor < adjacency[node].len() {
                let next = adjacency[node][cursor];
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                match color[next] {
                    1 => return true,
                    0 => {
                        color[next] = 1;
                        stack.push((next, 0));
                    }
                    _ => {}
                }
            } else {
                color[node] = 2;
                stack.pop();
            }
        }
    }
    false
}

/// Inverse edges: which tasks list a given id in their `blockedBy`
/// (upstream `deriveBlocks`). O(edges).
pub fn derive_blocks(tasks: &[Task]) -> BTreeMap<u64, Vec<u64>> {
    let mut blocks: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for task in tasks {
        for dep in task.blocked_by.as_deref().unwrap_or(&[]) {
            blocks.entry(*dep).or_default().push(task.id);
        }
    }
    blocks
}

// ---------------------------------------------------------------------------
// Terminal-text sanitizer (tool/sanitize.ts)
// ---------------------------------------------------------------------------

/// Remove terminal control characters from model-controlled task text before
/// it reaches a renderer: complete CSI/OSC sequences are dropped whole,
/// newlines/tabs become spaces, and bidi controls are removed. Single pass,
/// O(chars), no allocation beyond the output.
pub fn sanitize_terminal_text(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        match character {
            // CSI: ESC '[' or the C1 single-byte introducer.
            '\u{001b}' | '\u{009b}'
                if chars.get(index + 1) == Some(&'[') || character == '\u{009b}' =>
            {
                index += if character == '\u{001b}' { 2 } else { 1 };
                // Parameters: 0x30-0x3F; intermediates: 0x20-0x2F; final: 0x40-0x7E.
                while let Some(&current) = chars.get(index) {
                    index += 1;
                    if ('\u{40}'..='\u{7e}').contains(&current) {
                        break;
                    }
                }
            }
            // OSC: ESC ']' or the C1 introducer; an unterminated OSC swallows
            // the rest of the string, matching a real terminal.
            '\u{009d}' | '\u{001b}'
                if character == '\u{009d}' || chars.get(index + 1) == Some(&']') =>
            {
                index += if character == '\u{001b}' { 2 } else { 1 };
                while let Some(&current) = chars.get(index) {
                    index += 1;
                    if current == '\u{0007}' || current == '\u{009c}' {
                        break;
                    }
                    if current == '\u{001b}' {
                        // Optional String Terminator 'ESC \'.
                        if chars.get(index) == Some(&'\\') {
                            index += 1;
                        }
                        break;
                    }
                }
            }
            // Any remaining two-character ESC sequence.
            '\u{001b}' => {
                index += if chars.get(index + 1).is_some() { 2 } else { 1 };
            }
            // Unicode line/paragraph separators join lines like \n.
            '\u{2028}' | '\u{2029}' => {
                out.push(' ');
                index += 1;
            }
            // C0/C1 controls: newline, carriage return and tab become spaces.
            _ if character.is_control() && !matches!(character, '\n' | '\r' | '\t') => {
                index += 1;
            }
            // Bidi embedding/override/isolate controls and LRM/RLM marks.
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                index += 1;
            }
            '\n' | '\r' | '\t' => {
                out.push(' ');
                index += 1;
            }
            _ => {
                out.push(character);
                index += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Params (tool/types.ts)
// ---------------------------------------------------------------------------

/// The parsed tool arguments. Every field is optional except the action
/// (which the caller parses first); a malformed field is rejected with an
/// in-band `invalid params: …` message instead of being silently dropped
/// (the JSON Schema rejects those shapes before the tool runs, so the
/// messages are for raw/embedder callers).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TodoParams {
    pub subject: Option<String>,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub status: Option<TaskStatus>,
    pub blocked_by: Option<Vec<u64>>,
    pub add_blocked_by: Option<Vec<u64>>,
    pub remove_blocked_by: Option<Vec<u64>>,
    pub owner: Option<String>,
    pub metadata: Option<JsonMap<String, JsonValue>>,
    pub id: Option<u64>,
    pub include_deleted: Option<bool>,
}

impl TodoParams {
    /// Parse the raw argument object. Unknown keys are ignored (upstream's
    /// open params bag); a present key with a wrong type is an error.
    pub fn parse(args: &JsonValue) -> Result<Self, String> {
        let Some(object) = args.as_object() else {
            return Err("invalid params: expected an object".to_owned());
        };
        let string = |key: &str| -> Result<Option<String>, String> {
            match object.get(key) {
                None | Some(JsonValue::Null) => Ok(None),
                Some(JsonValue::String(value)) => Ok(Some(value.clone())),
                Some(_) => Err(format!("invalid params: {key} must be a string")),
            }
        };
        let number_list = |key: &str| -> Result<Option<Vec<u64>>, String> {
            match object.get(key) {
                None | Some(JsonValue::Null) => Ok(None),
                Some(JsonValue::Array(items)) => {
                    let mut ids = Vec::with_capacity(items.len());
                    for item in items {
                        match item.as_u64() {
                            Some(id) => ids.push(id),
                            None => {
                                return Err(format!(
                                    "invalid params: {key} must be an array of task ids"
                                ))
                            }
                        }
                    }
                    Ok(Some(ids))
                }
                Some(_) => Err(format!(
                    "invalid params: {key} must be an array of task ids"
                )),
            }
        };
        let status = match object.get("status") {
            None | Some(JsonValue::Null) => None,
            Some(JsonValue::String(value)) => Some(TaskStatus::parse(value).ok_or_else(|| {
                "invalid params: status must be one of pending, in_progress, completed, deleted"
                    .to_string()
            })?),
            Some(_) => return Err(
                "invalid params: status must be one of pending, in_progress, completed, deleted"
                    .to_owned(),
            ),
        };
        let id =
            match object.get("id") {
                None | Some(JsonValue::Null) => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    "invalid params: id must be a non-negative integer".to_owned()
                })?),
            };
        let include_deleted = match object.get("includeDeleted") {
            None | Some(JsonValue::Null) => None,
            Some(JsonValue::Bool(value)) => Some(*value),
            Some(_) => return Err("invalid params: includeDeleted must be a boolean".to_owned()),
        };
        let metadata = match object.get("metadata") {
            None | Some(JsonValue::Null) => None,
            Some(JsonValue::Object(map)) => Some(map.clone()),
            Some(_) => return Err("invalid params: metadata must be an object".to_owned()),
        };
        Ok(Self {
            subject: string("subject")?,
            description: string("description")?,
            active_form: string("activeForm")?,
            status,
            blocked_by: number_list("blockedBy")?,
            add_blocked_by: number_list("addBlockedBy")?,
            remove_blocked_by: number_list("removeBlockedBy")?,
            owner: string("owner")?,
            metadata,
            id,
            include_deleted,
        })
    }
}

// ---------------------------------------------------------------------------
// Reducer (state/state-reducer.ts)
// ---------------------------------------------------------------------------

fn error_op(message: impl Into<String>) -> TodoOp {
    TodoOp::Error {
        message: message.into(),
    }
}

/// Upstream's `update` guard: at least one mutable field must be present
/// (`id` alone is rejected).
fn has_mutation(params: &TodoParams) -> bool {
    params.subject.is_some()
        || params.description.is_some()
        || params.active_form.is_some()
        || params.status.is_some()
        || params.owner.is_some()
        || params.metadata.is_some()
        || params
            .add_blocked_by
            .as_ref()
            .is_some_and(|ids| !ids.is_empty())
        || params
            .remove_blocked_by
            .as_ref()
            .is_some_and(|ids| !ids.is_empty())
}

/// Upstream's `taskChanged`: did this update change anything? `blockedBy` is
/// order-sensitive; metadata compares as JSON.
fn task_changed(before: &Task, after: &Task) -> bool {
    before.subject != after.subject
        || before.status != after.status
        || before.description != after.description
        || before.active_form != after.active_form
        || before.owner != after.owner
        || before.blocked_by != after.blocked_by
        || before.metadata != after.metadata
}

/// Byte size of a JSON value without serializing it (for the metadata bound).
fn json_size(value: &JsonValue) -> usize {
    match value {
        JsonValue::Null => 4,
        JsonValue::Bool(_) => 5,
        JsonValue::Number(number) => number.to_string().len(),
        JsonValue::String(text) => text.len() + 2,
        JsonValue::Array(items) => 2 + items.iter().map(|item| json_size(item) + 1).sum::<usize>(),
        JsonValue::Object(map) => object_size(map),
    }
}

/// Byte size of an object's entries (`{}` = 2), no allocation.
fn object_size(map: &JsonMap<String, JsonValue>) -> usize {
    2 + map
        .iter()
        .map(|(key, value)| key.len() + 2 + json_size(value) + 2)
        .sum::<usize>()
}

fn text_bound_error(field: &str, value: &str, limit: usize) -> Option<String> {
    if value.chars().count() > limit {
        Some(format!("{field} too long (limit {limit} characters)"))
    } else {
        None
    }
}

fn metadata_bound_error(metadata: Option<&JsonMap<String, JsonValue>>) -> Option<String> {
    let metadata = metadata?;
    if object_size(metadata) > MAX_METADATA_BYTES {
        Some(format!(
            "metadata too large (limit {MAX_METADATA_BYTES} bytes per task)"
        ))
    } else {
        None
    }
}

/// Apply one mutation to `state` in place and report the outcome (upstream
/// `applyTaskMutation`). Validation runs first and every rejection returns
/// without touching the state, so a rejected call leaves the list untouched —
/// the same observable contract as upstream's pure reducer.
///
/// The in-place shape is the Rust-side CPU choice (no whole-state clone per
/// call); the returned [`TodoOp`] carries everything the envelope needs.
pub fn apply_mutation(state: &mut TodoState, action: TaskAction, params: &TodoParams) -> TodoOp {
    match action {
        TaskAction::Create => {
            // The subject is stored as given (leading/trailing space kept,
            // upstream parity); only the emptiness check trims.
            let Some(subject) = params
                .subject
                .as_deref()
                .filter(|subject| !subject.trim().is_empty())
                .map(str::to_owned)
            else {
                return error_op("subject required for create");
            };
            if let Some(error) = text_bound_error("subject", &subject, MAX_SUBJECT_CHARS) {
                return error_op(error);
            }
            if let Some(description) = &params.description {
                if let Some(error) =
                    text_bound_error("description", description, MAX_DESCRIPTION_CHARS)
                {
                    return error_op(error);
                }
            }
            if let Some(active_form) = &params.active_form {
                if let Some(error) =
                    text_bound_error("activeForm", active_form, MAX_ACTIVE_FORM_CHARS)
                {
                    return error_op(error);
                }
            }
            if let Some(owner) = &params.owner {
                if let Some(error) = text_bound_error("owner", owner, MAX_OWNER_CHARS) {
                    return error_op(error);
                }
            }
            if let Some(metadata) = &params.metadata {
                if let Some(error) = metadata_bound_error(Some(metadata)) {
                    return error_op(error);
                }
            }
            if let Some(blocked_by) = params.blocked_by.as_ref().filter(|ids| !ids.is_empty()) {
                for dep in blocked_by {
                    match state.tasks.iter().find(|task| task.id == *dep) {
                        None => return error_op(format!("blockedBy: #{dep} not found")),
                        Some(task) if task.status == TaskStatus::Deleted => {
                            return error_op(format!("blockedBy: #{dep} is deleted"))
                        }
                        Some(_) => {}
                    }
                }
            }
            if state.tasks.len() >= MAX_TASKS_PER_SESSION {
                return error_op(format!(
                    "task limit reached (limit {MAX_TASKS_PER_SESSION} tasks per session)"
                ));
            }
            let task = Task {
                id: state.next_id,
                subject,
                // Upstream assigns description/activeForm/blockedBy/owner on
                // create only when truthy; `metadata` follows JS truthiness,
                // where `{}` is truthy and is therefore stored as-is.
                description: params
                    .description
                    .clone()
                    .filter(|description| !description.is_empty()),
                active_form: params
                    .active_form
                    .clone()
                    .filter(|active_form| !active_form.is_empty()),
                status: TaskStatus::Pending,
                blocked_by: params
                    .blocked_by
                    .clone()
                    .filter(|blocked_by| !blocked_by.is_empty()),
                owner: params.owner.clone().filter(|owner| !owner.is_empty()),
                metadata: params.metadata.clone(),
            };
            let task_id = task.id;
            state.tasks.push(task);
            state.next_id = state.next_id.saturating_add(1);
            TodoOp::Create { task_id }
        }

        TaskAction::Update => {
            let Some(id) = params.id else {
                return error_op("id required for update");
            };
            let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
                return error_op(format!("#{id} not found"));
            };
            if !has_mutation(params) {
                return error_op(
                    "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy",
                );
            }

            let before = state.tasks[index].clone();
            let mut new_status = before.status;
            if let Some(target) = params.status {
                if !is_transition_valid(before.status, target) {
                    return error_op(format!("illegal transition {} → {}", before.status, target));
                }
                new_status = target;
            }

            let mut new_blocked_by = before.blocked_by.clone().unwrap_or_default();
            if let Some(remove) = params
                .remove_blocked_by
                .as_ref()
                .filter(|ids| !ids.is_empty())
            {
                new_blocked_by.retain(|dep| !remove.contains(dep));
            }
            if let Some(add) = params.add_blocked_by.as_ref().filter(|ids| !ids.is_empty()) {
                for dep in add {
                    if *dep == id {
                        return error_op(format!("cannot block #{id} on itself"));
                    }
                    match state.tasks.iter().find(|task| task.id == *dep) {
                        None => return error_op(format!("addBlockedBy: #{dep} not found")),
                        Some(task) if task.status == TaskStatus::Deleted => {
                            return error_op(format!("addBlockedBy: #{dep} is deleted"))
                        }
                        Some(_) => {}
                    }
                    if !new_blocked_by.contains(dep) {
                        new_blocked_by.push(*dep);
                    }
                }
                if detect_cycle(&state.tasks, id, &new_blocked_by) {
                    return error_op("addBlockedBy would create a cycle in the blockedBy graph");
                }
            }

            let mut new_metadata = before.metadata.clone();
            if let Some(patch) = &params.metadata {
                let mut merged = new_metadata.clone().unwrap_or_default();
                for (key, value) in patch {
                    if value.is_null() {
                        merged.remove(key);
                    } else {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                new_metadata = if merged.is_empty() {
                    None
                } else {
                    Some(merged)
                };
            }

            // Bounds on the post-update record (never on the pre-image).
            if let Some(subject) = &params.subject {
                if let Some(error) = text_bound_error("subject", subject, MAX_SUBJECT_CHARS) {
                    return error_op(error);
                }
            }
            if let Some(description) = &params.description {
                if let Some(error) =
                    text_bound_error("description", description, MAX_DESCRIPTION_CHARS)
                {
                    return error_op(error);
                }
            }
            if let Some(active_form) = &params.active_form {
                if let Some(error) =
                    text_bound_error("activeForm", active_form, MAX_ACTIVE_FORM_CHARS)
                {
                    return error_op(error);
                }
            }
            if let Some(owner) = &params.owner {
                if let Some(error) = text_bound_error("owner", owner, MAX_OWNER_CHARS) {
                    return error_op(error);
                }
            }
            if let Some(error) = metadata_bound_error(new_metadata.as_ref()) {
                return error_op(error);
            }

            let mut updated = before.clone();
            updated.status = new_status;
            if let Some(subject) = &params.subject {
                updated.subject = subject.clone();
            }
            if let Some(description) = &params.description {
                updated.description = Some(description.clone());
            }
            if let Some(active_form) = &params.active_form {
                updated.active_form = Some(active_form.clone());
            }
            if let Some(owner) = &params.owner {
                updated.owner = Some(owner.clone());
            }
            updated.blocked_by = if new_blocked_by.is_empty() {
                None
            } else {
                Some(new_blocked_by)
            };
            updated.metadata = new_metadata;

            let changed = task_changed(&before, &updated);
            state.tasks[index] = updated;
            TodoOp::Update {
                id,
                from: before.status,
                to: new_status,
                changed,
            }
        }

        TaskAction::List => TodoOp::List {
            status_filter: params.status,
            include_deleted: params.include_deleted == Some(true),
        },

        TaskAction::Get => {
            let Some(id) = params.id else {
                return error_op("id required for get");
            };
            match state.tasks.iter().find(|task| task.id == id) {
                Some(_) => TodoOp::Get { id },
                None => error_op(format!("#{id} not found")),
            }
        }

        TaskAction::Delete => {
            let Some(id) = params.id else {
                return error_op("id required for delete");
            };
            let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
                return error_op(format!("#{id} not found"));
            };
            if state.tasks[index].status == TaskStatus::Deleted {
                return error_op(format!("#{id} is already deleted"));
            }
            state.tasks[index].status = TaskStatus::Deleted;
            TodoOp::Delete {
                id,
                subject: state.tasks[index].subject.clone(),
            }
        }

        TaskAction::Clear => {
            let count = state.tasks.len();
            state.tasks.clear();
            state.next_id = 1;
            TodoOp::Clear { count }
        }
    }
}

// ---------------------------------------------------------------------------
// Response envelope (tool/response-envelope.ts)
// ---------------------------------------------------------------------------

/// One `list` row: `[status] #id subject [(activeForm)] [⛓ #dep,…]`.
fn format_list_line(task: &Task) -> String {
    let block = task
        .blocked_by
        .as_ref()
        .filter(|blocked_by| !blocked_by.is_empty())
        .map(|blocked_by| {
            format!(
                " ⛓ {}",
                blocked_by
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .unwrap_or_default();
    let form = if task.status == TaskStatus::InProgress {
        // Upstream's `t.activeForm` truthiness check: an empty string renders
        // no `(…)` suffix.
        task.active_form
            .as_deref()
            .filter(|active_form| !active_form.is_empty())
            .map(|active_form| format!(" ({})", sanitize_terminal_text(active_form)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!(
        "[{}] #{} {}{}{}",
        task.status,
        task.id,
        sanitize_terminal_text(&task.subject),
        form,
        block
    )
}

/// Multi-line `get` presentation; row order pinned by upstream
/// `response-envelope.ts:23-36` (description, activeForm, blockedBy, blocks,
/// owner).
fn format_get_lines(task: &Task, state: &TodoState) -> String {
    let blocks = derive_blocks(&state.tasks)
        .get(&task.id)
        .cloned()
        .unwrap_or_default();
    let mut lines = vec![format!(
        "#{} [{}] {}",
        task.id,
        task.status,
        sanitize_terminal_text(&task.subject)
    )];
    if let Some(description) = task.description.as_deref().filter(|text| !text.is_empty()) {
        lines.push(format!(
            "  description: {}",
            sanitize_terminal_text(description)
        ));
    }
    if let Some(active_form) = task.active_form.as_deref().filter(|text| !text.is_empty()) {
        lines.push(format!(
            "  activeForm: {}",
            sanitize_terminal_text(active_form)
        ));
    }
    if let Some(blocked_by) = task.blocked_by.as_ref().filter(|ids| !ids.is_empty()) {
        lines.push(format!(
            "  blockedBy: {}",
            blocked_by
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !blocks.is_empty() {
        lines.push(format!(
            "  blocks: {}",
            blocks
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(owner) = task.owner.as_deref().filter(|text| !text.is_empty()) {
        lines.push(format!("  owner: {}", sanitize_terminal_text(owner)));
    }
    lines.join("\n")
}

/// The model-facing content string (upstream `formatContent`), byte-compatible
/// with the table in `docs/tool-schema.md` § Content strings.
pub fn format_content(op: &TodoOp, state: &TodoState) -> String {
    match op {
        TodoOp::Create { task_id } => match state.tasks.iter().find(|task| task.id == *task_id) {
            Some(task) => format!(
                "Created #{}: {} (pending)",
                task.id,
                sanitize_terminal_text(&task.subject)
            ),
            None => format!("Created #{task_id}"),
        },
        TodoOp::Update {
            id,
            from,
            to,
            changed,
        } => {
            if !changed {
                format!("No change: #{id} already matches the requested values (status: {to})")
            } else if from != to {
                format!("Updated #{id} ({from} → {to})")
            } else {
                format!("Updated #{id}")
            }
        }
        TodoOp::Delete { id, subject } => {
            format!("Deleted #{id}: {}", sanitize_terminal_text(subject))
        }
        TodoOp::Clear { count } => format!("Cleared {count} tasks"),
        TodoOp::List {
            status_filter,
            include_deleted,
        } => {
            let view: Vec<&Task> = state
                .tasks
                .iter()
                .filter(|task| *include_deleted || task.status != TaskStatus::Deleted)
                .filter(|task| status_filter.is_none_or(|status| task.status == status))
                .collect();
            if view.is_empty() {
                "No tasks".to_owned()
            } else {
                view.iter()
                    .map(|task| format_list_line(task))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        TodoOp::Get { id } => match state.tasks.iter().find(|task| task.id == *id) {
            Some(task) => format_get_lines(task, state),
            // Defensive: the reducer only produces `Get` for a resolved id.
            None => format!("#{id}"),
        },
        TodoOp::Error { message } => format!("Error: {message}"),
    }
}

/// Build the tool result: `content` is the human-readable summary, `details`
/// is the persistence + replay snapshot
/// (`{action, params, tasks, nextId, error?}`).
///
/// Rejections stay in-band (`content = "Error: …"`, `details.error`), exactly
/// like upstream, so `is_error` is false even for a rejected call — the model
/// reads the prefix, not the transport flag.
pub fn build_tool_result(
    action: TaskAction,
    params: &JsonValue,
    state: &TodoState,
    op: &TodoOp,
) -> ToolResult {
    let text = format_content(op, state);
    let mut details = JsonMap::new();
    details.insert(
        "action".to_owned(),
        JsonValue::String(action.as_str().to_owned()),
    );
    details.insert("params".to_owned(), params.clone());
    details.insert(
        "tasks".to_owned(),
        serde_json::to_value(&state.tasks).unwrap_or(JsonValue::Array(Vec::new())),
    );
    details.insert("nextId".to_owned(), json!(state.next_id));
    if let TodoOp::Error { message } = op {
        details.insert("error".to_owned(), JsonValue::String(message.clone()));
    }
    ToolResult {
        content: text,
        images: Vec::new(),
        details: Some(JsonValue::Object(details)),
        structured_content: None,
        is_error: false,
        terminate: false,
    }
}

// ---------------------------------------------------------------------------
// Replay (state/replay.ts)
// ---------------------------------------------------------------------------

/// One replay scan over the transcript entries.
#[derive(Debug, Clone)]
pub struct ReplayScan {
    /// Cheap change detector: `(entries.len(), per-match signature stream)`,
    /// where each matching entry contributes its `toolCallId`, `timestamp`,
    /// and a structural hash of its snapshot `details` (no deserialization).
    pub fingerprint: u64,
    /// The last entry whose `details` parsed as a task snapshot.
    pub state: Option<TodoState>,
    /// How many entries were examined (the cost counter).
    pub entries_visited: u64,
    /// How many `todo` tool results were seen.
    pub matches: u64,
    /// How many task snapshots were deserialized for [`Self::state`]
    /// (normally 1 when any matching snapshot exists, 0 otherwise).
    pub deserializations: u64,
}

/// Cheap first phase: the fingerprint plus the index of the LAST matching
/// result, with no deserialization and no allocation.
#[derive(Debug, Clone)]
struct ReplayIndex {
    fingerprint: u64,
    last_match: Option<usize>,
    entries_visited: u64,
    matches: u64,
}

/// FNV-1a over bytes (the fingerprint hash).
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FNV-1a over one byte (kind tags for the structural JSON hash).
fn fnv1a_byte(byte: u8, hash: u64) -> u64 {
    fnv1a(&[byte], hash)
}

/// Allocation-free structural hash of a JSON value for the change detector.
/// It only needs to move when the snapshot's meaning moves; tagging the JSON
/// kinds keeps `1`/`"1"`/`true` distinct.
fn hash_json_value(value: &JsonValue, mut hash: u64) -> u64 {
    match value {
        JsonValue::Null => fnv1a_byte(b'n', hash),
        JsonValue::Bool(flag) => fnv1a_byte(if *flag { b't' } else { b'f' }, hash),
        JsonValue::Number(number) => {
            if let Some(value) = number.as_u64() {
                hash = fnv1a_byte(b'u', hash);
                fnv1a(&value.to_le_bytes(), hash)
            } else if let Some(value) = number.as_i64() {
                hash = fnv1a_byte(b'i', hash);
                fnv1a(&value.to_le_bytes(), hash)
            } else if let Some(value) = number.as_f64() {
                hash = fnv1a_byte(b'd', hash);
                fnv1a(&value.to_bits().to_le_bytes(), hash)
            } else {
                fnv1a_byte(b'?', hash)
            }
        }
        JsonValue::String(text) => {
            hash = fnv1a_byte(b's', hash);
            fnv1a(text.as_bytes(), hash)
        }
        JsonValue::Array(items) => {
            hash = fnv1a_byte(b'[', hash);
            for item in items {
                hash = hash_json_value(item, hash);
            }
            fnv1a_byte(b']', hash)
        }
        JsonValue::Object(map) => {
            hash = fnv1a_byte(b'{', hash);
            for (key, item) in map {
                hash = fnv1a_byte(b'k', hash);
                hash = fnv1a(key.as_bytes(), hash);
                hash = hash_json_value(item, hash);
            }
            fnv1a_byte(b'}', hash)
        }
    }
}

/// Whether `details` matches the persisted snapshot shape (upstream
/// `isTaskDetails`: a `tasks` array plus a numeric `nextId`).
fn is_task_details(value: &JsonValue) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("tasks").is_some_and(JsonValue::is_array)
        && object.get("nextId").is_some_and(JsonValue::is_u64)
}

/// Parse one `details` envelope into a task snapshot (the persistence shape:
/// a `tasks` array plus a numeric `nextId`). `None` for anything else —
/// upstream's `isTaskDetails` shape check, plus the task deserialization the
/// JS side does implicitly.
pub fn snapshot_from_details(details: &JsonValue) -> Option<TodoState> {
    if !is_task_details(details) {
        return None;
    }
    let tasks = serde_json::from_value::<Vec<Task>>(details.get("tasks")?.clone()).ok()?;
    let next_id = details.get("nextId").and_then(JsonValue::as_u64)?;
    Some(TodoState { tasks, next_id })
}

/// Phase one: walk the transcript entries once, compute the fingerprint, and
/// remember the LAST `todo` tool result whose `details` match the persisted
/// shape. No snapshot is deserialized here, so an unchanged re-push costs one
/// structural hash of the matching details and nothing else.
///
/// The fingerprint streams `(toolCallId, timestamp, details hash)` of every
/// matching entry plus the entry count, so it moves when a result is
/// appended, dropped, edited, or truncated — and stays put for rewrites that
/// cannot affect the task list (a non-`todo` message edit of the same
/// length).
fn scan_entries(entries: &[JsonValue]) -> ReplayIndex {
    let mut hash = fnv1a(&(entries.len() as u64).to_le_bytes(), 0xcbf2_9ce4_8422_2325);
    let mut matches = 0_u64;
    let mut last_match: Option<usize> = None;
    for (index, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            continue;
        };
        if object.get("role").and_then(JsonValue::as_str) != Some("toolResult") {
            continue;
        }
        if object.get("toolName").and_then(JsonValue::as_str) != Some(TOOL_NAME) {
            continue;
        }
        let Some(details) = object.get("details") else {
            continue;
        };
        if !is_task_details(details) {
            continue;
        }
        matches += 1;
        last_match = Some(index);
        hash = match object.get("toolCallId").and_then(JsonValue::as_str) {
            Some(call_id) => fnv1a(call_id.as_bytes(), fnv1a_byte(b'c', hash)),
            None => fnv1a_byte(b'C', hash),
        };
        hash = match object.get("timestamp").and_then(JsonValue::as_u64) {
            Some(timestamp) => fnv1a(&timestamp.to_le_bytes(), fnv1a_byte(b't', hash)),
            None => fnv1a_byte(b'T', hash),
        };
        hash = hash_json_value(details, hash);
    }
    ReplayIndex {
        fingerprint: hash,
        last_match,
        entries_visited: entries.len() as u64,
        matches,
    }
}

/// Phase two: deserialize the last matching snapshot, walking backwards only
/// if that entry is shape-valid but malformed (defensive, upstream's shape
/// check keeps the previous candidate). Returns the snapshot and the number
/// of deserialization attempts, so the store can expose the cost.
fn snapshot_from_last_match(
    entries: &[JsonValue],
    last_match: Option<usize>,
) -> (Option<TodoState>, u64) {
    let Some(last) = last_match else {
        return (None, 0);
    };
    let mut deserializations = 0_u64;
    for entry in entries[..=last].iter().rev() {
        let Some(object) = entry.as_object() else {
            continue;
        };
        if object.get("role").and_then(JsonValue::as_str) != Some("toolResult") {
            continue;
        }
        if object.get("toolName").and_then(JsonValue::as_str) != Some(TOOL_NAME) {
            continue;
        }
        let Some(details) = object.get("details") else {
            continue;
        };
        if !is_task_details(details) {
            continue;
        }
        deserializations += 1;
        if let Some(snapshot) = snapshot_from_details(details) {
            return (Some(snapshot), deserializations);
        }
    }
    (None, deserializations)
}

/// Two-phase replay for one-shot callers: the cheap fingerprint pass, then
/// the LAST matching snapshot (last-write-wins, upstream `replayFromBranch`).
/// [`TodoStore::replay`] instead compares the fingerprint first and calls
/// [`snapshot_from_last_match`] only when it moved, so an unchanged re-push
/// performs zero deserializations.
pub fn replay_from_entries(entries: &[JsonValue]) -> ReplayScan {
    let index = scan_entries(entries);
    let (state, deserializations) = snapshot_from_last_match(entries, index.last_match);
    ReplayScan {
        fingerprint: index.fingerprint,
        state,
        entries_visited: index.entries_visited,
        matches: index.matches,
        deserializations,
    }
}

// ---------------------------------------------------------------------------
// Store (state/store.ts)
// ---------------------------------------------------------------------------

/// Counters backing the CPU assertions (and useful diagnostics). All are
/// monotonic; tests compare deltas.
#[derive(Debug, Default)]
pub struct TodoStats {
    /// `on_session_context` pushes seen.
    pub pushes: AtomicU64,
    /// Entries passes performed (one per push whose entries are an array).
    pub scans: AtomicU64,
    /// Pushes whose fingerprint equaled the cached one (state untouched).
    pub replay_skips: AtomicU64,
    /// Entries examined across all passes (the O(entries) cost).
    pub entries_visited: AtomicU64,
    /// Task snapshots deserialized by replay. An unchanged re-push performs
    /// zero; a moved fingerprint performs exactly one for a well-formed
    /// transcript (the last matching snapshot).
    pub deserializations: AtomicU64,
    /// Slot replacements (replay or live commit).
    pub state_touches: AtomicU64,
    /// Tool calls executed.
    pub tool_calls: AtomicU64,
    /// `on_event` dispatches that did work (not the name-mismatch returns).
    pub handled_events: AtomicU64,
}

/// Plain snapshot of [`TodoStats`] for assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TodoStatsSnapshot {
    pub pushes: u64,
    pub scans: u64,
    pub replay_skips: u64,
    pub entries_visited: u64,
    pub deserializations: u64,
    pub state_touches: u64,
    pub tool_calls: u64,
    pub handled_events: u64,
}

impl TodoStats {
    /// Read every counter (relaxed: counters, not synchronization).
    pub fn snapshot(&self) -> TodoStatsSnapshot {
        TodoStatsSnapshot {
            pushes: self.pushes.load(Ordering::Relaxed),
            scans: self.scans.load(Ordering::Relaxed),
            replay_skips: self.replay_skips.load(Ordering::Relaxed),
            entries_visited: self.entries_visited.load(Ordering::Relaxed),
            deserializations: self.deserializations.load(Ordering::Relaxed),
            state_touches: self.state_touches.load(Ordering::Relaxed),
            tool_calls: self.tool_calls.load(Ordering::Relaxed),
            handled_events: self.handled_events.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
struct SessionSlot {
    state: TodoState,
    /// Fingerprint of the entries this slot was replayed from; `None` = never
    /// replayed (or invalidated by `session_start` / `/reload`).
    fingerprint: Option<u64>,
    /// LRU stamp.
    touched: u64,
}

/// Per-session task slots plus the active-session pointer.
///
/// The map is the single mutation seam (replay, tool commits, eviction); the
/// reducer stays pure. One session host owns one plugin instance, so slots
/// only multiply in hosts that reuse an instance across session switches.
pub struct TodoStore {
    sessions: Mutex<BTreeMap<String, SessionSlot>>,
    active: RwLock<String>,
    stats: TodoStats,
    lru: AtomicU64,
}

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
            active: RwLock::new(String::new()),
            stats: TodoStats::default(),
            lru: AtomicU64::new(0),
        }
    }

    /// The session key live tool calls commit to (last `on_session_context`
    /// session id, `""` before the first push).
    pub fn active_session(&self) -> String {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Select the session slot live tool calls commit to.
    pub fn set_active_session(&self, session: &str) {
        *self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session.to_owned();
    }

    /// Counters for diagnostics and tests.
    pub fn stats(&self) -> &TodoStats {
        &self.stats
    }

    /// Test/profiling probe (`#[doc(hidden)]`): the number of live session
    /// slots (bounded by [`MAX_SESSION_SLOTS`]).
    #[doc(hidden)]
    pub fn session_slot_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Current state of a session (a fresh empty state when the slot is
    /// absent — never stored by the read).
    pub fn state(&self, session: &str) -> TodoState {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .map(|slot| slot.state.clone())
            .unwrap_or_default()
    }

    /// Commit a whole state (replay replacement or a test seam).
    pub fn replace_state(&self, session: &str, state: TodoState) {
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, session);
        let entry = sessions
            .entry(session.to_owned())
            .or_insert_with(|| SessionSlot {
                state: TodoState::empty(),
                fingerprint: None,
                touched: stamp,
            });
        entry.state = state;
        entry.touched = stamp;
        self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
    }

    /// Invalidate the active slot's replay cache so the next entries push is
    /// authoritative (`session_start`, `/reload`).
    fn invalidate_replay_cache(&self) {
        let active = self.active_session();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = sessions.get_mut(&active) {
            slot.fingerprint = None;
        }
    }

    /// Drop every slot (`on_shutdown`).
    fn clear(&self) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.set_active_session("");
    }

    /// Drop one session's slot (memory hygiene for a reused instance).
    pub fn evict_session(&self, session: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
    }

    /// LRU-evict the least recently touched slot when the map is at capacity
    /// (never the slot being written).
    fn evict_over_capacity(sessions: &mut BTreeMap<String, SessionSlot>, keep: &str) {
        while sessions.len() >= MAX_SESSION_SLOTS {
            let victim = sessions
                .iter()
                .filter(|(key, _)| key.as_str() != keep)
                .min_by_key(|(_, slot)| slot.touched)
                .map(|(key, _)| key.clone());
            match victim {
                Some(victim) => {
                    sessions.remove(&victim);
                }
                None => break,
            }
        }
    }

    /// Replay one entries push into `session`'s slot: a cheap fingerprint pass
    /// first, and a state replacement — with exactly one deserialization of
    /// the last matching snapshot — only when the fingerprint moved (or the
    /// cache was invalidated). An unchanged re-push performs zero
    /// deserializations. Returns whether the state was replaced.
    pub fn replay(&self, session: &str, entries: &[JsonValue]) -> bool {
        self.stats.scans.fetch_add(1, Ordering::Relaxed);
        let index = scan_entries(entries);
        self.stats
            .entries_visited
            .fetch_add(index.entries_visited, Ordering::Relaxed);
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, session);
        let slot = sessions
            .entry(session.to_owned())
            .or_insert_with(|| SessionSlot {
                state: TodoState::empty(),
                fingerprint: None,
                touched: stamp,
            });
        slot.touched = stamp;
        if slot.fingerprint == Some(index.fingerprint) {
            self.stats.replay_skips.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // Phase two runs only on a moved fingerprint: deserialize the LAST
        // matching snapshot instead of every historical one.
        let (state, deserializations) = snapshot_from_last_match(entries, index.last_match);
        self.stats
            .deserializations
            .fetch_add(deserializations, Ordering::Relaxed);
        slot.state = state.unwrap_or_default();
        slot.fingerprint = Some(index.fingerprint);
        self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Run one tool call against the active session slot.
    fn execute(&self, args: &JsonValue, action: TaskAction, params: &TodoParams) -> ToolResult {
        self.stats.tool_calls.fetch_add(1, Ordering::Relaxed);
        let session = self.active_session();
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, &session);
        let slot = sessions.entry(session).or_insert_with(|| SessionSlot {
            state: TodoState::empty(),
            fingerprint: None,
            touched: stamp,
        });
        slot.touched = stamp;
        let op = apply_mutation(&mut slot.state, action, params);
        if !matches!(op, TodoOp::Error { .. }) {
            self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
        }
        // Serialize under the lock: one O(tasks) pass, no state clone.
        build_tool_result(action, args, &slot.state, &op)
    }
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// The `todo` tool. Sequential execution mode: two `todo` calls in one batch
/// must not interleave their read-modify-write, and model-visible id
/// assignment (`create A`, `create B` → ids in call order) stays
/// deterministic. Background execution is refused: a detached call's result
/// would land after the turn that scheduled it, outside the ordering the list
/// semantics assume.
struct TodoTool {
    store: Arc<TodoStore>,
}

impl AgentTool for TodoTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn description(&self) -> &'static str {
        TOOL_DESCRIPTION
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(todo_parameters_schema())
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(DEFAULT_PROMPT_SNIPPET.to_owned())
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        DEFAULT_PROMPT_GUIDELINES
            .iter()
            .map(|guideline| (*guideline).to_owned())
            .collect()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn supports_background(&self) -> bool {
        false
    }

    fn run(&self, args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        // Clone the (small) argument object so the returned future is
        // `'static`; the mutation itself is synchronous under the store lock.
        let args = args.clone();
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let action_name = args.get("action").and_then(JsonValue::as_str).unwrap_or("");
            let Some(action) = TaskAction::parse(action_name) else {
                let state = store.state(&store.active_session());
                let op = TodoOp::Error {
                    message: format!("invalid params: unknown action \"{action_name}\""),
                };
                // `details.action` carries the raw string here: the envelope
                // contract names a `TaskAction`, and this path only exists for
                // a caller the JSON Schema never let through.
                let mut result = build_tool_result(TaskAction::List, &args, &state, &op);
                if let Some(details) = result.details.as_mut().and_then(JsonValue::as_object_mut) {
                    details.insert(
                        "action".to_owned(),
                        JsonValue::String(action_name.to_owned()),
                    );
                }
                return result;
            };
            let params = match TodoParams::parse(&args) {
                Ok(params) => params,
                Err(message) => {
                    let state = store.state(&store.active_session());
                    return build_tool_result(action, &args, &state, &TodoOp::Error { message });
                }
            };
            store.execute(&args, action, &params)
        })
    }
}

/// The TypeBox `TodoParamsSchema` as JSON Schema: same property set, same
/// enums, same descriptions (LLM-facing copy, verbatim). Key order inside the
/// object follows the serde_json map ordering; the schema's meaning is
/// unchanged.
fn todo_parameters_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["create", "update", "list", "get", "delete", "clear"]
            },
            "subject": {
                "type": "string",
                "description": "Task subject line (required for create)"
            },
            "description": {
                "type": "string",
                "description": "Long-form task description"
            },
            "activeForm": {
                "type": "string",
                "description": "Present-continuous spinner label shown while status is in_progress (e.g. 'writing tests')"
            },
            "status": {
                "type": "string",
                "enum": ["pending", "in_progress", "completed", "deleted"],
                "description": "Set this task's status (update): one of pending, in_progress, completed, deleted. When action is list, filters returned tasks by this status."
            },
            "blockedBy": {
                "type": "array",
                "items": { "type": "number" },
                "description": "Initial blockedBy ids (create only)"
            },
            "addBlockedBy": {
                "type": "array",
                "items": { "type": "number" },
                "description": "Task ids to add to blockedBy (update only, additive merge)"
            },
            "removeBlockedBy": {
                "type": "array",
                "items": { "type": "number" },
                "description": "Task ids to remove from blockedBy (update only, additive merge)"
            },
            "owner": {
                "type": "string",
                "description": "Agent/owner assigned to this task"
            },
            "metadata": {
                "type": "object",
                "additionalProperties": true,
                "description": "Arbitrary metadata; pass null value for a key to delete that key on update"
            },
            "id": {
                "type": "number",
                "description": "Task id (required for update, get, delete)"
            },
            "includeDeleted": {
                "type": "boolean",
                "description": "If true, list action returns deleted (tombstoned) tasks as well. Default: false."
            }
        },
        "required": ["action"]
    })
}

// ---------------------------------------------------------------------------
// `/todos` rendering (todo.ts registerTodosCommand + view/format.ts)
// ---------------------------------------------------------------------------

/// The `/todos` output: a header line with the non-zero counts, then the
/// sections `── Pending ──`, `── In Progress ──`, `── Completed ──` (only
/// non-empty ones). Tombstones are never listed. `None` when the list is
/// empty (the caller prints [`MSG_NO_TODOS`]).
pub fn format_todos_command(state: &TodoState) -> Option<String> {
    let visible: Vec<&Task> = state
        .tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Deleted)
        .collect();
    if visible.is_empty() {
        return None;
    }
    let pending: Vec<&Task> = visible
        .iter()
        .copied()
        .filter(|task| task.status == TaskStatus::Pending)
        .collect();
    let in_progress: Vec<&Task> = visible
        .iter()
        .copied()
        .filter(|task| task.status == TaskStatus::InProgress)
        .collect();
    let completed: Vec<&Task> = visible
        .iter()
        .copied()
        .filter(|task| task.status == TaskStatus::Completed)
        .collect();

    let mut header: Vec<String> = Vec::new();
    if !completed.is_empty() {
        header.push(format!("{}/{} completed", completed.len(), visible.len()));
    }
    if !in_progress.is_empty() {
        header.push(format!("{} in progress", in_progress.len()));
    }
    if !pending.is_empty() {
        header.push(format!("{} pending", pending.len()));
    }

    let mut lines: Vec<String> = vec![header.join(" · ")];
    for (title, glyph, group) in [
        ("── Pending ──", "○", &pending),
        ("── In Progress ──", "◐", &in_progress),
        ("── Completed ──", "✓", &completed),
    ] {
        if group.is_empty() {
            continue;
        }
        lines.push(title.to_owned());
        for task in group {
            lines.push(format_command_task_line(task, glyph));
        }
    }
    Some(lines.join("\n"))
}

/// One `/todos` row: `  ○ #4 subject [(activeForm)] [    ⛓ #dep,…]`
/// (upstream `formatCommandTaskLine`: two-space indent, four spaces before
/// the chain glyph).
fn format_command_task_line(task: &Task, glyph: &str) -> String {
    let form = if task.status == TaskStatus::InProgress {
        // Upstream's `t.activeForm` truthiness check: an empty string renders
        // no `(…)` suffix.
        task.active_form
            .as_deref()
            .filter(|active_form| !active_form.is_empty())
            .map(|active_form| format!(" ({})", sanitize_terminal_text(active_form)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let block = task
        .blocked_by
        .as_ref()
        .filter(|ids| !ids.is_empty())
        .map(|ids| {
            format!(
                "    ⛓ {}",
                ids.iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .unwrap_or_default();
    format!(
        "  {glyph} #{} {}{}{}",
        task.id,
        sanitize_terminal_text(&task.subject),
        form,
        block
    )
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// The `todo` builtin plugin.
pub struct TodoPlugin {
    info: PluginInfo,
    store: Arc<TodoStore>,
    /// Whether the session has a UI (the `/todos` interactive gate). Kept as a
    /// shared cell, not read from the handle at call time, because the
    /// command handler is built during `register` — which the host may run
    /// before it hands over the runtime handle.
    has_ui: Arc<std::sync::atomic::AtomicBool>,
    /// The JS runtime handle, when the session has one (kept for parity with
    /// the `mcp` builtin's attach seam and for future notify routing).
    js: RwLock<Option<crate::extensions::ExtensionHandle>>,
}

impl Default for TodoPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoPlugin {
    pub fn new() -> Self {
        let mut info = PluginInfo::builtin("todo");
        // Replaceable: a user extension registering a `todo` tool or `todos`
        // command omits this builtin (upstream `replaceable` factory flag).
        info.replaceable = super::builtin_extension_def("todo").is_some_and(|def| def.replaceable);
        Self {
            info,
            store: Arc::new(TodoStore::new()),
            has_ui: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            js: RwLock::new(None),
        }
    }

    /// The store (tests and embedders that need the live task state).
    pub fn store(&self) -> &Arc<TodoStore> {
        &self.store
    }

    /// The active session's current state.
    pub fn state(&self) -> TodoState {
        self.store.state(&self.store.active_session())
    }

    /// Counters backing the CPU/perf assertions.
    pub fn stats(&self) -> &TodoStats {
        self.store.stats()
    }

    /// Whether the session reports a UI (interactive `/todos`).
    pub fn has_ui(&self) -> bool {
        self.has_ui.load(Ordering::Relaxed)
    }

    /// Handle one `on_session_context` push: select the active session, then
    /// replay when the push carries transcript entries that changed since the
    /// slot's last replay. A model-only push (no `entries` array) is a no-op —
    /// live tool commits stand.
    fn ingest_context(&self, snapshot: &SessionContextSnapshot) {
        self.store.stats.pushes.fetch_add(1, Ordering::Relaxed);
        if let Some(session_id) = snapshot.session_id.as_deref().filter(|id| !id.is_empty()) {
            self.store.set_active_session(session_id);
        }
        let Some(entries) = snapshot.entries.as_array() else {
            return;
        };
        let session = self.store.active_session();
        self.store.replay(&session, entries);
    }
}

/// The `/todos` outcome for a state and UI posture.
///
/// `ok` is false only for the non-interactive rejection; the grouped list
/// rides `error`, the only host-visible text channel a Rust command has today
/// (the `mcp` builtin's precedent — see the module docs).
pub fn todos_command_outcome(store: &TodoStore, has_ui: bool) -> CommandOutcome {
    if !has_ui {
        return CommandOutcome {
            ok: false,
            error: Some(ERR_REQUIRES_INTERACTIVE.to_owned()),
        };
    }
    let state = store.state(&store.active_session());
    match format_todos_command(&state) {
        Some(text) => CommandOutcome {
            ok: true,
            error: Some(text),
        },
        None => CommandOutcome {
            ok: true,
            error: Some(MSG_NO_TODOS.to_owned()),
        },
    }
}

impl AgentPlugin for TodoPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(TodoTool {
            store: Arc::clone(&self.store),
        }));

        let store = Arc::clone(&self.store);
        let has_ui = Arc::clone(&self.has_ui);
        let handler: crate::plugins::CommandHandler = Arc::new(move |_args: &str| {
            let store = Arc::clone(&store);
            let has_ui = Arc::clone(&has_ui);
            Box::pin(async move { todos_command_outcome(&store, has_ui.load(Ordering::Relaxed)) })
        });
        ctx.register_command(
            COMMAND_NAME,
            Some("Show all todos on the current branch, grouped by status"),
            Some(CommandBackend::Rust(handler)),
        );

        // The collapse/expand hotkey: registered as a DEFINITION only (no
        // overlay renderer exists in v1 — see the module docs). A Rust no-op
        // handler keeps the registry entry well-formed for a UI lane to bind.
        let shortcut: crate::plugins::ShortcutHandler = Arc::new(|| {
            Box::pin(async {
                CommandOutcome {
                    ok: true,
                    error: None,
                }
            })
        });
        ctx.register_shortcut(
            "ctrl+shift+t",
            Some("Collapse or expand the todo overlay"),
            Some(ShortcutBackend::Rust(shortcut)),
        );
        Ok(())
    }

    /// `session_start` re-arms the replay cache. `tool_result` is deliberately
    /// NOT declared: the live tool commit is authoritative, and declaring it
    /// would switch the agent loop's hosted result fan-out on for every batch
    /// (`has_native_handlers("tool_result")`), cloning every result's
    /// `details`/`structuredContent` just so this plugin can replace state it
    /// already committed. The declaration is also the dispatch fence: the
    /// host's `emit` / `emit_native` skip this plugin for `tool_result`
    /// (`native_plugin_wants_event`), which is intended and costs no
    /// correctness. The idempotent branch in `on_event` stays as a
    /// compatibility path for a dispatcher that calls `on_event` directly
    /// (the hosted result walk in
    /// [`crate::agent::tool_execution::dispatch_hosted_event`] does).
    /// `message_update` (and every other unrelated event) never reaches the
    /// plugin through the gated loop paths, and the handler matches on the
    /// name before touching anything: zero per-delta and zero per-result work.
    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_start".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            match event.name.as_ref() {
                "session_start" => {
                    self.store
                        .stats
                        .handled_events
                        .fetch_add(1, Ordering::Relaxed);
                    // A fresh session start re-arms the replay cache: the next
                    // entries push is authoritative (idempotent — replay only
                    // replaces).
                    self.store.invalidate_replay_cache();
                }
                "tool_result"
                    // Defensive second source, never a dependency: this
                    // plugin does not declare the event, so the host's
                    // `emit`/`emit_native` skip it here; the branch only runs
                    // for a dispatcher that calls `on_event` directly (the
                    // hosted result walk, when ANOTHER plugin turned that
                    // path on). A matching envelope is a whole-state REPLACE
                    // — the same value the tool commit published — so a
                    // duplicate is indistinguishable from the live write and
                    // cannot double-apply (unlike a `tool_call` subscription,
                    // which would).
                    if event.payload.get("toolName").and_then(JsonValue::as_str) == Some(TOOL_NAME) =>
                {
                    if let Some(state) =
                        event.payload.get("details").and_then(snapshot_from_details)
                    {
                        let session = self.store.active_session();
                        self.store.replace_state(&session, state);
                        self.store
                            .stats
                            .handled_events
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
            PluginEventOutcome::None
        })
    }

    fn on_session_context(&self, snapshot: &SessionContextSnapshot) {
        self.ingest_context(snapshot);
    }

    /// `/reload` parity: the store survives (the session continues), but the
    /// replay cache is invalidated so the next authoritative push re-derives
    /// the state from the transcript.
    fn on_reload(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            self.store.invalidate_replay_cache();
            Ok(())
        })
    }

    /// Session teardown: drop every slot (bounded memory; a reused instance
    /// starts clean).
    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            self.store.clear();
            Ok(())
        })
    }

    /// Host hook: cache the UI posture and keep the runtime handle (only the
    /// handle's existence is used today; a UI-notify route would ride it).
    fn on_runtime_attached(&self, js: Option<crate::extensions::ExtensionHandle>) {
        self.has_ui.store(
            js.as_ref().is_some_and(|handle| handle.config().has_ui),
            Ordering::Relaxed,
        );
        *self
            .js
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = js;
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Factory for
/// [`super::BUILTIN_EXTENSION_FACTORIES`](super::BUILTIN_EXTENSION_FACTORIES).
pub fn new_todo_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(TodoPlugin::new())
}
