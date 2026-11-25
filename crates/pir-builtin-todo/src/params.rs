//! Tool-argument parsing.
//!
//! The JSON Schema rejects malformed shapes before the tool runs; these
//! errors exist for raw/embedder callers that reach the domain directly. A
//! present key with a wrong type is an error; unknown keys are ignored.

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::constants::MAX_BATCH_ITEMS;
use crate::types::{TaskAction, TaskStatus};

/// The parsed tool arguments. Every field is optional except the action
/// (which the caller parses first).
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
    /// Parse the raw argument object.
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

/// One parsed `batch` item: an ordinary mutating action whose reducer runs
/// against a scratch state before the whole batch commits.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchItem {
    pub action: TaskAction,
    pub params: TodoParams,
}

/// Parse `{action:"batch", updates:[...]}`. `updates` must be a non-empty
/// array of create/update/delete/clear objects; list/get are read-only and
/// nested batch calls are rejected. Item errors name the 1-based position.
pub fn parse_batch_items(args: &JsonValue) -> Result<Vec<BatchItem>, String> {
    let Some(updates) = args.get("updates").and_then(JsonValue::as_array) else {
        return Err("batch requires a non-empty updates array".to_owned());
    };
    if updates.is_empty() {
        return Err("batch requires a non-empty updates array".to_owned());
    }
    if updates.len() > MAX_BATCH_ITEMS {
        return Err(format!(
            "batch supports at most {MAX_BATCH_ITEMS} updates (got {})",
            updates.len()
        ));
    }
    let mut items = Vec::with_capacity(updates.len());
    for (index, update) in updates.iter().enumerate() {
        let position = index + 1;
        let Some(object) = update.as_object() else {
            return Err(format!("batch item {position}: expected an object"));
        };
        let action_name = object
            .get("action")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        let Some(action) = TaskAction::parse(action_name) else {
            return Err(format!(
                "batch item {position}: unknown action \"{action_name}\""
            ));
        };
        match action {
            TaskAction::Create | TaskAction::Update | TaskAction::Delete | TaskAction::Clear => {}
            TaskAction::Batch => {
                return Err(format!(
                    "batch item {position}: nested batch is not allowed"
                ))
            }
            TaskAction::List | TaskAction::Get => {
                return Err(format!(
                    "batch item {position}: {action} is read-only; call it directly"
                ))
            }
        }
        let params =
            TodoParams::parse(update).map_err(|error| format!("batch item {position}: {error}"))?;
        items.push(BatchItem { action, params });
    }
    Ok(items)
}
