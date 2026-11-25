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
//! - One tool named `todo` with seven actions: `create`, `update`, `batch`,
//!   `list`, `get`, `delete`, `clear`; one command `/todos`. `batch` is a
//!   pir extension (atomic create/update/delete/clear items in one call).
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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::{json, Value as JsonValue};

use crate::agent::tool_batch::{ToolExecutionMode, ToolResult};
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::extensions::{CommandOutcome, SessionContextSnapshot};
use crate::plugins::plugin::{
    AgentPlugin, CommandBackend, EventInterest, PluginContext, PluginEvent, PluginEventOutcome,
    PluginFuture, RegistrationContext, ShortcutBackend,
};
use crate::plugins::services::{
    DialogOptions, FactoryRenderContext, OverlayComponent, OverlayComponentFactory, OverlayDone,
    UiService, WidgetFactory, WidgetPlacement,
};
use crate::plugins::source::PluginInfo;

// ---------------------------------------------------------------------------
// The pure domain lives in `pir-builtin-todo`
// ---------------------------------------------------------------------------
//
// Identity constants, domain types, the status machine, the dependency graph,
// the terminal-text sanitizer, params/batch parsing, the reducer, the response
// envelope, replay and the per-session store are all host-independent, so they
// live in the engine-free `pir-builtin-todo` crate. This module re-exports them
// under the historical `pir::plugins::builtin::todo::*` paths and keeps only
// what needs the engine: the `ToolResult` adapter below, the `todo` tool, the
// `/todos` command, the overlay and the plugin shell.

pub use pir_builtin_todo::{
    apply_mutation, build_response, build_response_for, derive_blocks, detect_cycle,
    entries_fingerprint, format_command_task_line, format_content, format_get_lines,
    format_list_line, format_todos_command, is_transition_valid, parse_batch_items,
    replay_from_entries, sanitize_terminal_text, snapshot_from_details, BatchItem, ReplayScan,
    Task, TaskAction, TaskStatus, TodoOp, TodoParams, TodoResponse, TodoState, TodoStats,
    TodoStatsSnapshot, TodoStore, COMMAND_NAME, DEFAULT_PROMPT_GUIDELINES, DEFAULT_PROMPT_SNIPPET,
    ERR_REQUIRES_INTERACTIVE, MAX_ACTIVE_FORM_CHARS, MAX_BATCH_ITEMS, MAX_DESCRIPTION_CHARS,
    MAX_METADATA_BYTES, MAX_OWNER_CHARS, MAX_SESSION_SLOTS, MAX_SUBJECT_CHARS,
    MAX_TASKS_PER_SESSION, MSG_NO_TODOS, TOOL_DESCRIPTION, TOOL_LABEL, TOOL_NAME,
};

/// The engine-side result for one todo mutation: the crate builds the response
/// envelope, the engine adds its own result fields.
///
/// Keeping this here (rather than in the crate) is what lets the crate stay
/// engine-free: `ToolResult` carries engine-only fields (images, terminate,
/// structured content) a host-neutral package must not know about.
pub fn build_tool_result(
    action: TaskAction,
    params: &JsonValue,
    state: &TodoState,
    op: &TodoOp,
) -> ToolResult {
    let response = build_response(action.as_str(), params, state, op);
    ToolResult {
        content: response.content,
        images: Vec::new(),
        details: Some(response.details),
        structured_content: None,
        // An in-band rejection is still a successful tool call: the model reads
        // the error text and retries (upstream `isError: false`).
        is_error: false,
        terminate: false,
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
    ui: Arc<TodoUi>,
}

impl AgentTool for TodoTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn label(&self) -> &'static str {
        TOOL_LABEL
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
        let ui = Arc::clone(&self.ui);
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
            if action == TaskAction::Batch {
                let items = match parse_batch_items(&args) {
                    Ok(items) => items,
                    Err(message) => {
                        let state = store.state(&store.active_session());
                        return build_tool_result(
                            TaskAction::Batch,
                            &args,
                            &state,
                            &TodoOp::Error { message },
                        );
                    }
                };
                let result = store.execute_batch(&args, &items);
                ui.repaint(&store);
                return engine_result(result);
            }
            let result = store.execute(&args, action, &params);
            // Upstream repaints the overlay at `tool_execution_end`; the
            // native widget is registered through the same UI service, so a
            // re-registration marks it dirty for the next TUI tick.
            ui.repaint(&store);
            engine_result(result)
        })
    }
}

/// The crate's response envelope as an engine tool result.
///
/// The store already returns the envelope (content + `details` snapshot); the
/// engine adds its own result fields here, in one place, so the tool body reads
/// the same whether the crate or a test built the response.
fn engine_result(response: TodoResponse) -> ToolResult {
    ToolResult {
        content: response.content,
        images: Vec::new(),
        details: Some(response.details),
        structured_content: None,
        is_error: false,
        terminate: false,
    }
}

/// The shared property set for one todo operation. `batch` items reuse it, so
/// the nested schema is the same contract as a top-level call (minus
/// `updates`, which only makes sense at the top level).
fn todo_property_schema() -> JsonValue {
    json!({
        "action": {
            "type": "string",
            "enum": ["create", "update", "list", "get", "delete", "clear", "batch"]
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
            "description": "Set this task's status (create/update): one of pending, in_progress, completed, deleted. create defaults to pending; deleted is not a valid creation state. When action is list, filters returned tasks by this status."
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
    })
}

/// The TypeBox `TodoParamsSchema` as JSON Schema plus pir's `batch` extension.
/// The upstream property set, enums and descriptions are unchanged; `updates`
/// and `batch` are additive.
fn todo_parameters_schema() -> JsonValue {
    let mut properties = todo_property_schema();
    if let Some(map) = properties.as_object_mut() {
        map.insert(
            "updates".to_owned(),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": todo_property_schema(),
                    "required": ["action"]
                },
                "description": "Batch only (action:\"batch\"): create/update/delete/clear operations applied in order. The whole batch commits atomically; a failing item leaves the list unchanged. list/get and nested batch are rejected. Max 100 items."
            }),
        );
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": ["action"]
    })
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// Widget key used by the todo overlay (upstream `ctx.ui.setWidget("todos", …)`).
const WIDGET_KEY: &str = "todos";

/// Shared UI cell: the bound [`UiService`] plus the collapse state. The tool
/// and the shortcut both hold this, so a task mutation or `ctrl+shift+t`
/// repaints the same overlay without re-plumbing the plugin host.
struct TodoUi {
    service: RwLock<Option<Arc<dyn UiService>>>,
    collapsed: Arc<AtomicBool>,
    has_ui: Arc<AtomicBool>,
}

impl TodoUi {
    fn new(has_ui: Arc<AtomicBool>) -> Self {
        Self {
            service: RwLock::new(None),
            collapsed: Arc::new(AtomicBool::new(false)),
            has_ui,
        }
    }

    fn bind(&self, service: Option<Arc<dyn UiService>>) {
        *self
            .service
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = service;
    }

    fn repaint(&self, store: &Arc<TodoStore>) {
        if !self.has_ui.load(Ordering::Relaxed) {
            return;
        }
        let Some(ui) = self
            .service
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        ui.set_widget(
            WIDGET_KEY,
            Arc::new(TodoWidget {
                store: Arc::clone(store),
                collapsed: Arc::clone(&self.collapsed),
            }),
            WidgetPlacement::Overlay,
        );
    }

    fn toggle(&self, store: &Arc<TodoStore>) {
        self.collapsed.fetch_xor(true, Ordering::SeqCst);
        self.repaint(store);
    }

    fn clear(&self) {
        let Some(ui) = self
            .service
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        ui.clear_widget(WIDGET_KEY);
    }
}

/// The overlay renderer (upstream `rpiv-todo`'s widget factory): the same
/// grouped list `/todos` prints, or one summary line when collapsed.
struct TodoWidget {
    store: Arc<TodoStore>,
    collapsed: Arc<AtomicBool>,
}

impl WidgetFactory for TodoWidget {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        let width = context.width.max(10);
        let collapsed = self.collapsed.load(Ordering::Relaxed);
        // Format under the lock instead of cloning the whole task list on
        // every render; the closure is pure and only builds the lines.
        let lines = self
            .store
            .with_state(&self.store.active_session(), |state| {
                let Some(text) = format_todos_command(state) else {
                    return Vec::new();
                };
                let clip = |line: &str| -> String {
                    if line.chars().count() <= width {
                        return line.to_owned();
                    }
                    let mut clipped: String = line.chars().take(width.saturating_sub(1)).collect();
                    clipped.push('…');
                    clipped
                };
                if collapsed {
                    let header = text.lines().next().unwrap_or("Todos");
                    return vec![clip(&format!("Todos · {header} · ctrl+shift+t"))];
                }
                let mut lines: Vec<String> = text.lines().map(clip).collect();
                // Keep the overlay from taking the whole dock on a large list.
                const MAX_ROWS: usize = 12;
                if lines.len() > MAX_ROWS {
                    let omitted = lines.len() - (MAX_ROWS - 1);
                    lines.truncate(MAX_ROWS - 1);
                    lines.push(format!("… {omitted} more"));
                }
                lines
            });
        lines.unwrap_or_default()
    }
}

/// Test seam for the overlay renderer: expose the widget lines without
/// constructing a UI service.
#[doc(hidden)]
pub fn widget_lines_for_tests(store: &Arc<TodoStore>, collapsed: bool) -> Vec<String> {
    TodoWidget {
        store: Arc::clone(store),
        collapsed: Arc::new(AtomicBool::new(collapsed)),
    }
    .render(&FactoryRenderContext {
        width: 80,
        theme: None,
        footer_data: None,
        request_render: None,
    })
}

// ---------------------------------------------------------------------------
// Interactive `/todos` overlay
// ---------------------------------------------------------------------------

/// Arrow keys move the selection; `space` advances the status machine, `d`
/// tombstones the selected task, `enter` expands its details.
struct TodoOverlayFactory {
    store: Arc<TodoStore>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl OverlayComponentFactory for TodoOverlayFactory {
    fn create(&self, done: OverlayDone) -> Option<Arc<dyn OverlayComponent>> {
        Some(Arc::new(TodoOverlay {
            store: Arc::clone(&self.store),
            repaint: Arc::clone(&self.repaint),
            selected: AtomicU64::new(0),
            expanded: AtomicBool::new(false),
            done,
        }))
    }
}

struct TodoOverlay {
    store: Arc<TodoStore>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    /// Selected task id (`0` = first visible task).
    selected: AtomicU64,
    expanded: AtomicBool,
    done: OverlayDone,
}

impl TodoOverlay {
    fn close(&self) {
        (self.done)(JsonValue::Null);
    }

    fn selected_state(&self) -> (String, TodoState, Option<Task>) {
        let session = self.store.active_session();
        let state = self.store.state(&session);
        let tasks = visible_tasks(&state);
        let selected = self.selected.load(Ordering::Relaxed);
        let task = tasks
            .iter()
            .find(|task| task.id == selected)
            .or_else(|| tasks.first())
            .map(|task| (*task).clone());
        (session, state, task)
    }

    fn move_selection(&self, delta: isize) {
        let (_, state, _) = self.selected_state();
        let tasks = visible_tasks(&state);
        if tasks.is_empty() {
            return;
        }
        let current = self.selected.load(Ordering::Relaxed);
        let index = tasks
            .iter()
            .position(|task| task.id == current)
            .unwrap_or(0);
        let next = if delta < 0 {
            index.saturating_sub(1)
        } else {
            (index + 1).min(tasks.len() - 1)
        };
        self.selected.store(tasks[next].id, Ordering::Relaxed);
    }

    fn mutate(&self, action: TaskAction, params: TodoParams) {
        let (session, mut state, _) = self.selected_state();
        let op = apply_mutation(&mut state, action, &params);
        if !matches!(op, TodoOp::Error { .. }) {
            self.store.replace_state(&session, state);
            (self.repaint)();
        }
    }

    fn advance_status(&self) {
        let Some(task) = self.selected_state().2 else {
            return;
        };
        let next = match task.status {
            TaskStatus::Pending => TaskStatus::InProgress,
            TaskStatus::InProgress => TaskStatus::Completed,
            _ => return,
        };
        self.mutate(
            TaskAction::Update,
            TodoParams {
                id: Some(task.id),
                status: Some(next),
                ..TodoParams::default()
            },
        );
    }

    fn delete_selected(&self) {
        let Some(task) = self.selected_state().2 else {
            return;
        };
        self.mutate(
            TaskAction::Delete,
            TodoParams {
                id: Some(task.id),
                ..TodoParams::default()
            },
        );
    }
}

fn visible_tasks(state: &TodoState) -> Vec<&Task> {
    state
        .tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Deleted)
        .collect()
}

fn todo_glyph(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "○",
        TaskStatus::InProgress => "◐",
        TaskStatus::Completed => "✓",
        TaskStatus::Deleted => "×",
    }
}

fn clip_line(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn wrap_line(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut current = String::new();
        for word in raw.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.chars().count() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_owned();
            }
            if lines.len() >= max_lines {
                break;
            }
        }
        if !current.is_empty() && lines.len() < max_lines {
            lines.push(current);
        }
        if lines.len() >= max_lines {
            break;
        }
    }
    lines
}

/// Test seam: build the interactive list without a UI service.
#[doc(hidden)]
pub fn todo_overlay_component_for_tests(
    store: Arc<TodoStore>,
    done: OverlayDone,
) -> Arc<dyn OverlayComponent> {
    Arc::new(TodoOverlay {
        store,
        repaint: Arc::new(|| {}),
        selected: AtomicU64::new(0),
        expanded: AtomicBool::new(false),
        done,
    })
}

impl OverlayComponent for TodoOverlay {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        let width = context.width.max(24);
        let (_, state, _) = self.selected_state();
        let tasks = visible_tasks(&state);
        let header = format_todos_command(&state)
            .and_then(|text| text.lines().next().map(str::to_owned))
            .unwrap_or_else(|| MSG_NO_TODOS.to_owned());
        let mut lines = vec![clip_line(&format!("Todos · {header} · ↑/↓ select"), width)];
        if tasks.is_empty() {
            lines.push(MSG_NO_TODOS.to_owned());
            lines.push("Esc closes.".to_owned());
            return lines;
        }
        let selected_id = tasks
            .iter()
            .find(|task| task.id == self.selected.load(Ordering::Relaxed))
            .or_else(|| tasks.first())
            .map(|task| task.id)
            .unwrap_or(0);
        self.selected.store(selected_id, Ordering::Relaxed);
        for task in &tasks {
            let marker = if task.id == selected_id { "▶" } else { " " };
            let row = format_command_task_line(task, todo_glyph(task.status));
            lines.push(clip_line(&format!("{marker} {}", row.trim_start()), width));
        }
        if self.expanded.load(Ordering::Relaxed) {
            if let Some(task) = tasks.iter().find(|task| task.id == selected_id) {
                lines.push(String::new());
                lines.push(clip_line(
                    &format!("#{} {}", task.id, sanitize_terminal_text(&task.subject)),
                    width,
                ));
                lines.push(clip_line(
                    &format!("status: {}", task.status.as_str()),
                    width,
                ));
                if let Some(active_form) = task.active_form.as_deref() {
                    for line in wrap_line(&format!("active: {active_form}"), width, 2) {
                        lines.push(line);
                    }
                }
                if let Some(owner) = task.owner.as_deref() {
                    lines.push(clip_line(&format!("owner: {owner}"), width));
                }
                if let Some(description) = task.description.as_deref() {
                    for line in wrap_line(&format!("description: {description}"), width, 6) {
                        lines.push(line);
                    }
                }
                if let Some(blocked_by) = task.blocked_by.as_ref().filter(|ids| !ids.is_empty()) {
                    lines.push(clip_line(
                        &format!(
                            "blocked by: {}",
                            blocked_by
                                .iter()
                                .map(|id| format!("#{id}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        width,
                    ));
                }
                let blocks = derive_blocks(&state.tasks);
                if let Some(ids) = blocks.get(&task.id).filter(|ids| !ids.is_empty()) {
                    lines.push(clip_line(
                        &format!(
                            "blocks: {}",
                            ids.iter()
                                .map(|id| format!("#{id}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        width,
                    ));
                }
            }
        }
        lines.push(String::new());
        lines.push(
            "↑/↓ move · enter details · space start/complete · d delete · esc close"
                .chars()
                .take(width)
                .collect(),
        );
        lines
    }

    fn handle_input(&self, data: &str) {
        match data {
            "\u{1b}[A" | "\u{1b}OA" => self.move_selection(-1),
            "\u{1b}[B" | "\u{1b}OB" => self.move_selection(1),
            "\r" | "\n" => {
                let expanded = self.expanded.load(Ordering::Relaxed);
                self.expanded.store(!expanded, Ordering::Relaxed);
            }
            " " => self.advance_status(),
            "d" | "D" => self.delete_selected(),
            "\u{1b}" | "q" | "Q" => self.close(),
            _ => {}
        }
    }
}

impl TodoUi {
    /// Open the interactive list. Resolves when the user closes it.
    async fn open_overlay(self: &Arc<Self>, store: &Arc<TodoStore>) -> bool {
        let Some(service) = self
            .service
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return false;
        };
        let repaint = {
            let ui = Arc::clone(self);
            let store = Arc::clone(store);
            Arc::new(move || ui.repaint(&store)) as Arc<dyn Fn() + Send + Sync>
        };
        let factory = Arc::new(TodoOverlayFactory {
            store: Arc::clone(store),
            repaint,
        });
        service
            .custom(
                factory,
                DialogOptions::with_timeout(Duration::from_secs(60 * 60)),
            )
            .await
            .is_ok()
    }
}

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
    /// Bound UI service + overlay collapse state.
    ui: Arc<TodoUi>,
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
        let has_ui = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            info,
            store: Arc::new(TodoStore::new()),
            has_ui: Arc::clone(&has_ui),
            js: RwLock::new(None),
            ui: Arc::new(TodoUi::new(has_ui)),
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
        self.store.stats().pushes.fetch_add(1, Ordering::Relaxed);
        if let Some(session_id) = snapshot.session_id.as_deref().filter(|id| !id.is_empty()) {
            self.store.set_active_session(session_id);
        }
        let Some(entries) = snapshot.entries.as_array() else {
            return;
        };
        let session = self.store.active_session();
        if self.store.replay(&session, entries) {
            // A transcript rewrite (`/tree`, compaction, branch switch) may
            // change the last todo snapshot without any tool call; the cached
            // widget lines must follow it.
            self.ui.repaint(&self.store);
        }
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
            ui: Arc::clone(&self.ui),
        }));

        let store = Arc::clone(&self.store);
        let has_ui = Arc::clone(&self.has_ui);
        let ui = Arc::clone(&self.ui);
        let handler: crate::plugins::CommandHandler = Arc::new(move |_args: &str| {
            let store = Arc::clone(&store);
            let has_ui = Arc::clone(&has_ui);
            let ui = Arc::clone(&ui);
            Box::pin(async move {
                let has_ui = has_ui.load(Ordering::Relaxed);
                let opened = if has_ui {
                    ui.open_overlay(&store).await
                } else {
                    false
                };
                let outcome = if opened {
                    CommandOutcome {
                        ok: true,
                        error: None,
                    }
                } else {
                    todos_command_outcome(&store, has_ui)
                };
                ui.repaint(&store);
                outcome
            })
        });
        ctx.register_command(
            COMMAND_NAME,
            Some("Show all todos on the current branch, grouped by status"),
            Some(CommandBackend::Rust(handler)),
        );

        // The collapse/expand hotkey toggles the native overlay and repaints
        // it immediately.
        let store = Arc::clone(&self.store);
        let ui = Arc::clone(&self.ui);
        let shortcut: crate::plugins::ShortcutHandler = Arc::new(move || {
            let store = Arc::clone(&store);
            let ui = Arc::clone(&ui);
            Box::pin(async move {
                ui.toggle(&store);
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
                        .stats()
                        .handled_events
                        .fetch_add(1, Ordering::Relaxed);
                    // A fresh session start re-arms the replay cache: the next
                    // entries push is authoritative (idempotent — replay only
                    // replaces).
                    self.store.invalidate_replay_cache();
                    self.ui.repaint(&self.store);
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
                            .stats()
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
            self.ui.repaint(&self.store);
            Ok(())
        })
    }

    /// Session teardown: drop every slot (bounded memory; a reused instance
    /// starts clean) and retire the overlay.
    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            self.store.clear();
            self.ui.clear();
            Ok(())
        })
    }

    /// Bind the UI service and install the overlay factory. The context is
    /// rebound after every `/reload`/session replacement, so the latest
    /// service wins.
    fn on_context(&self, ctx: &PluginContext) {
        self.ui.bind(ctx.ui_arc());
        self.ui.repaint(&self.store);
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
