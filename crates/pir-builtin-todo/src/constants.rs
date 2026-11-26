//! Protocol identity strings and record bounds.
//!
//! These are the string boundaries shared with the host: `todo` is the
//! persistence key used by transcript replay, so the tool name must never
//! change. The limits are the domain-side bounds that keep a task list's
//! memory and one call's work predictable.

/// The registered tool name. Also the replay filter key.
pub const TOOL_NAME: &str = "todo";
/// The tool's display label.
pub const TOOL_LABEL: &str = "Todo";
/// The slash command name.
pub const COMMAND_NAME: &str = "todos";
/// `/todos` in a session without a UI.
pub const ERR_REQUIRES_INTERACTIVE: &str = "/todos requires interactive mode";
/// `/todos` on an empty list.
pub const MSG_NO_TODOS: &str = "No todos yet. Ask the agent to add some!";

/// One-line prompt snippet describing the tool.
pub const DEFAULT_PROMPT_SNIPPET: &str = "Manage a task list to track multi-step progress";

/// Prompt guidelines (eight bullets), with batching and small-step guidance.
pub const DEFAULT_PROMPT_GUIDELINES: [&str; 8] = [
    "Use `todo` for complex work with 3+ steps, when the user gives you a list of tasks, or immediately after receiving new instructions to capture requirements. Skip it for single trivial tasks and purely conversational requests.",
    "Use one `batch` call for multiple todo changes (for example, complete the finished task and start the next task together), and use `create` with `status` or `update` to move a task directly between pending, in_progress and completed. Keep exactly one task in_progress at a time. Prefer small change → test → change loops over one large untested rewrite, and never spend a whole turn on a todo-only update when you can attach it to the next action.",
    "Never mark a task completed if tests are failing, the implementation is partial, or you hit unresolved errors — keep it in_progress and create a new task for the blocker instead.",
    "Task status is a 4-state machine: pending → in_progress → completed, plus deleted as a tombstone. Pass activeForm (present-continuous label, e.g. 'researching existing tool') when marking in_progress.",
    "To change a task's status, call update with the task id and the target status, e.g. {\"action\":\"update\",\"id\":3,\"status\":\"completed\"} or {\"action\":\"update\",\"id\":3,\"status\":\"in_progress\",\"activeForm\":\"writing tests\"}. status is the field that changes the task; an update without a mutable field (status or another) is rejected. For several changes, pass them as {\"action\":\"batch\",\"updates\":[{\"action\":\"update\",\"id\":3,\"status\":\"completed\"},{\"action\":\"create\",\"subject\":\"Next task\",\"status\":\"in_progress\"}]}.",
    "Use blockedBy to express dependencies (A is blocked by B). On create, pass blockedBy as the initial set. On update, use addBlockedBy / removeBlockedBy (additive merge — do not resend the full array). Cycles are rejected.",
    "list hides tombstoned (deleted) tasks by default; pass includeDeleted:true to see them. Pass status to filter by a single status.",
    "Subject must be short and imperative (e.g. 'Research existing tool'); description is for long-form detail. activeForm is a present-continuous label shown while in_progress.",
];

/// The tool description the model sees.
pub const TOOL_DESCRIPTION: &str = "Manage a task list for tracking multi-step progress. Actions: create (new task, optional status), update (change status/fields/dependencies), batch (apply several create/update/delete/clear operations in one atomic call), list (all tasks, optionally filtered by status), get (single task details), delete (tombstone), clear (reset all). Status: pending → in_progress → completed, plus deleted tombstone. Use this to plan and track multi-step work like research, design, and implementation; batch related status changes instead of making a separate call for each.";

/// Maximum tasks per session (create past this is rejected in-band).
pub const MAX_TASKS_PER_SESSION: usize = 1000;
/// Maximum operations in one `batch` call.
pub const MAX_BATCH_ITEMS: usize = 100;
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
/// Maximum session slots kept by [`TodoStore`](crate::store::TodoStore) (LRU).
pub const MAX_SESSION_SLOTS: usize = 8;
