//! Canonical task, state, action and reducer-outcome types.
//!
//! The wire representation is the persistence format: a replayed snapshot is
//! byte-comparable because absent optional fields are omitted.

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

/// Task status. Same→same is an accepted no-op, so this enum carries every
/// state a task can be asked to enter.
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
    /// All statuses, in state-machine display order.
    pub const ALL: [TaskStatus; 4] = [
        TaskStatus::Pending,
        TaskStatus::InProgress,
        TaskStatus::Completed,
        TaskStatus::Deleted,
    ];

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

/// The tool actions. `batch` applies several mutations in one call instead of
/// spending one full turn per change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskAction {
    Create,
    Update,
    List,
    Get,
    Delete,
    Clear,
    Batch,
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
            TaskAction::Batch => "batch",
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
            "batch" => Some(TaskAction::Batch),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One task. Optional fields are omitted from the wire when absent, which is
/// what makes a replayed snapshot byte-comparable.
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

/// Canonical per-session state: the task list plus the next id to assign. No
/// derived caches live here; selectors are pure functions over it.
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
    /// Fresh empty state (`nextId: 1`).
    pub fn empty() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }

    /// The task with `id`, if it exists (tombstones included).
    pub fn task(&self, id: u64) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    /// The live (non-deleted) tasks.
    pub fn visible_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(|task| task.status != TaskStatus::Deleted)
    }
}

/// Reducer outcome. Closed tagged union: the formatting layer switches over
/// it exhaustively.
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
    /// state at format time.
    Get {
        id: u64,
    },
    Clear {
        count: usize,
    },
    /// Several mutations committed as one atomic batch.
    Batch {
        ops: Vec<TodoOp>,
    },
    Error {
        message: String,
    },
}

impl TodoOp {
    /// An `Error` outcome with an owned message.
    pub fn error(message: impl Into<String>) -> Self {
        TodoOp::Error {
            message: message.into(),
        }
    }

    /// Whether this outcome is an in-band rejection.
    pub fn is_error(&self) -> bool {
        matches!(self, TodoOp::Error { .. })
    }
}
