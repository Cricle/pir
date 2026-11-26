//! Human-readable formatting: tool content strings and `/todos` text.

use crate::graph::derive_blocks;
use crate::sanitize::sanitize_terminal_text;
use crate::types::{Task, TaskStatus, TodoOp, TodoState};

/// One `list` row: `[status] #id subject [(activeForm)] [⛓ #dep,…]`.
pub fn format_list_line(task: &Task) -> String {
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
        // An empty `activeForm` renders no `(…)` suffix.
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

/// Multi-line `get` presentation; row order is pinned (description,
/// activeForm, blockedBy, blocks, owner).
pub fn format_get_lines(task: &Task, state: &TodoState) -> String {
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

/// The model-facing content string.
pub fn format_content(op: &TodoOp, state: &TodoState) -> String {
    match op {
        TodoOp::Create { task_id } => match state.task(*task_id) {
            Some(task) => format!(
                "Created #{}: {} ({})",
                task.id,
                sanitize_terminal_text(&task.subject),
                task.status.as_str()
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
        TodoOp::Batch { ops } => ops
            .iter()
            .enumerate()
            .map(|(index, op)| format!("{}. {}", index + 1, format_content(op, state)))
            .collect::<Vec<_>>()
            .join("\n"),
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
        TodoOp::Get { id } => match state.task(*id) {
            Some(task) => format_get_lines(task, state),
            // Defensive: the reducer only produces `Get` for a resolved id.
            None => format!("#{id}"),
        },
        TodoOp::Error { message } => format!("Error: {message}"),
    }
}

/// The `/todos` output: a header line with the non-zero counts, then the
/// sections `── Pending ──`, `── In Progress ──`, `── Completed ──` (only
/// non-empty ones). Tombstones are never listed. `None` when the list is
/// empty.
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
/// (two-space indent, four spaces before the chain glyph).
pub fn format_command_task_line(task: &Task, glyph: &str) -> String {
    let form = if task.status == TaskStatus::InProgress {
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
