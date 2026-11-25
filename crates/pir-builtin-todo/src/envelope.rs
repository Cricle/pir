//! The response envelope: model-facing content plus the persistence snapshot.
//!
//! `details` is `{action, params, tasks, nextId, error?}`. Every successful
//! call embeds the full post-mutation snapshot, which is exactly what
//! transcript replay reads back. Rejections stay in-band
//! (`content = "Error: …"`, `details.error = …`), so `is_error` is false even
//! for a rejected call.

use serde_json::{json, Map as JsonMap, Value as JsonValue};

use crate::format::format_content;
use crate::types::{TaskAction, TodoOp, TodoState};

/// A pure, host-neutral tool response.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoResponse {
    pub content: String,
    pub details: JsonValue,
    pub is_error: bool,
}

impl TodoResponse {
    /// The in-band error message, when this is a rejection.
    pub fn error_message(&self) -> Option<&str> {
        self.details.get("error").and_then(JsonValue::as_str)
    }
}

impl From<TodoResponse> for pir_plugin_api::ToolResult {
    fn from(response: TodoResponse) -> Self {
        Self {
            content: response.content,
            details: response.details,
            is_error: response.is_error,
        }
    }
}

/// Build the tool result. `action_name` is a string so an unknown raw action
/// can be echoed back in `details.action` without inventing a `TaskAction`.
pub fn build_response(
    action_name: &str,
    params: &JsonValue,
    state: &TodoState,
    op: &TodoOp,
) -> TodoResponse {
    let text = format_content(op, state);
    let mut details = JsonMap::new();
    details.insert(
        "action".to_owned(),
        JsonValue::String(action_name.to_owned()),
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
    TodoResponse {
        content: text,
        details: JsonValue::Object(details),
        is_error: false,
    }
}

/// Convenience wrapper for a known action.
pub fn build_response_for(
    action: TaskAction,
    params: &JsonValue,
    state: &TodoState,
    op: &TodoOp,
) -> TodoResponse {
    build_response(action.as_str(), params, state, op)
}
