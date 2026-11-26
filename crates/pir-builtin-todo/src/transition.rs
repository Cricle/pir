//! The task status machine.

use crate::types::TaskStatus;

/// Whether `from → to` is legal. A transition to the current status is always
/// accepted (a same→same request is an accepted no-op).
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
