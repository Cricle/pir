//! The pure reducer: validate, then mutate the state in place.
//!
//! Every rejection returns without touching the state, so a rejected call
//! leaves the list exactly as it was.

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::constants::{
    MAX_ACTIVE_FORM_CHARS, MAX_DESCRIPTION_CHARS, MAX_METADATA_BYTES, MAX_OWNER_CHARS,
    MAX_SUBJECT_CHARS, MAX_TASKS_PER_SESSION,
};
use crate::graph::detect_cycle;
use crate::params::TodoParams;
use crate::transition::is_transition_valid;
use crate::types::{Task, TaskAction, TaskStatus, TodoOp, TodoState};

/// At least one mutable field must be present (`id` alone is rejected).
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

/// Did this update change anything? `blockedBy` is order-sensitive; metadata
/// compares as JSON.
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

/// Apply one mutation to `state` in place and report the outcome.
///
/// Validation runs first and every rejection returns without touching the
/// state. `batch` is executed by [`TodoStore`](crate::store::TodoStore),
/// which applies this reducer to a scratch state before committing
/// atomically.
pub fn apply_mutation(state: &mut TodoState, action: TaskAction, params: &TodoParams) -> TodoOp {
    match action {
        TaskAction::Batch => TodoOp::error("batch requires a non-empty updates array"),
        TaskAction::Create => {
            // The subject is stored as given (leading/trailing space kept);
            // only the emptiness check trims.
            let Some(subject) = params
                .subject
                .as_deref()
                .filter(|subject| !subject.trim().is_empty())
                .map(str::to_owned)
            else {
                return TodoOp::error("subject required for create");
            };
            if let Some(error) = text_bound_error("subject", &subject, MAX_SUBJECT_CHARS) {
                return TodoOp::error(error);
            }
            if let Some(description) = &params.description {
                if let Some(error) =
                    text_bound_error("description", description, MAX_DESCRIPTION_CHARS)
                {
                    return TodoOp::error(error);
                }
            }
            if let Some(active_form) = &params.active_form {
                if let Some(error) =
                    text_bound_error("activeForm", active_form, MAX_ACTIVE_FORM_CHARS)
                {
                    return TodoOp::error(error);
                }
            }
            if let Some(owner) = &params.owner {
                if let Some(error) = text_bound_error("owner", owner, MAX_OWNER_CHARS) {
                    return TodoOp::error(error);
                }
            }
            if let Some(metadata) = &params.metadata {
                if let Some(error) = metadata_bound_error(Some(metadata)) {
                    return TodoOp::error(error);
                }
            }
            if let Some(blocked_by) = params.blocked_by.as_ref().filter(|ids| !ids.is_empty()) {
                for dep in blocked_by {
                    match state.tasks.iter().find(|task| task.id == *dep) {
                        None => return TodoOp::error(format!("blockedBy: #{dep} not found")),
                        Some(task) if task.status == TaskStatus::Deleted => {
                            return TodoOp::error(format!("blockedBy: #{dep} is deleted"))
                        }
                        Some(_) => {}
                    }
                }
            }
            if state.tasks.len() >= MAX_TASKS_PER_SESSION {
                return TodoOp::error(format!(
                    "task limit reached (limit {MAX_TASKS_PER_SESSION} tasks per session)"
                ));
            }
            // A task may be created directly in its working or finished
            // state. Tombstones are not a creation state.
            let status = match params.status {
                None => TaskStatus::Pending,
                Some(TaskStatus::Deleted) => {
                    return TodoOp::error("cannot create a deleted task");
                }
                Some(status) => status,
            };
            let task = Task {
                id: state.next_id,
                subject,
                description: params
                    .description
                    .clone()
                    .filter(|description| !description.is_empty()),
                active_form: params
                    .active_form
                    .clone()
                    .filter(|active_form| !active_form.is_empty()),
                status,
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
                return TodoOp::error("id required for update");
            };
            let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
                return TodoOp::error(format!("#{id} not found"));
            };
            if !has_mutation(params) {
                return TodoOp::error(
                    "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy",
                );
            }

            let before = state.tasks[index].clone();
            let mut new_status = before.status;
            if let Some(target) = params.status {
                if !is_transition_valid(before.status, target) {
                    return TodoOp::error(format!(
                        "illegal transition {} → {}",
                        before.status, target
                    ));
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
                        return TodoOp::error(format!("cannot block #{id} on itself"));
                    }
                    match state.tasks.iter().find(|task| task.id == *dep) {
                        None => return TodoOp::error(format!("addBlockedBy: #{dep} not found")),
                        Some(task) if task.status == TaskStatus::Deleted => {
                            return TodoOp::error(format!("addBlockedBy: #{dep} is deleted"))
                        }
                        Some(_) => {}
                    }
                    if !new_blocked_by.contains(dep) {
                        new_blocked_by.push(*dep);
                    }
                }
                if detect_cycle(&state.tasks, id, &new_blocked_by) {
                    return TodoOp::error(
                        "addBlockedBy would create a cycle in the blockedBy graph",
                    );
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
                    return TodoOp::error(error);
                }
            }
            if let Some(description) = &params.description {
                if let Some(error) =
                    text_bound_error("description", description, MAX_DESCRIPTION_CHARS)
                {
                    return TodoOp::error(error);
                }
            }
            if let Some(active_form) = &params.active_form {
                if let Some(error) =
                    text_bound_error("activeForm", active_form, MAX_ACTIVE_FORM_CHARS)
                {
                    return TodoOp::error(error);
                }
            }
            if let Some(owner) = &params.owner {
                if let Some(error) = text_bound_error("owner", owner, MAX_OWNER_CHARS) {
                    return TodoOp::error(error);
                }
            }
            if let Some(error) = metadata_bound_error(new_metadata.as_ref()) {
                return TodoOp::error(error);
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
                return TodoOp::error("id required for get");
            };
            match state.tasks.iter().find(|task| task.id == id) {
                Some(_) => TodoOp::Get { id },
                None => TodoOp::error(format!("#{id} not found")),
            }
        }

        TaskAction::Delete => {
            let Some(id) = params.id else {
                return TodoOp::error("id required for delete");
            };
            let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
                return TodoOp::error(format!("#{id} not found"));
            };
            if state.tasks[index].status == TaskStatus::Deleted {
                return TodoOp::error(format!("#{id} is already deleted"));
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
