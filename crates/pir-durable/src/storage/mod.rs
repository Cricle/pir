//! Storage backends and the scan helpers they share.

use std::collections::HashMap;

use crate::errors::StorageError;

pub mod jsonl;
pub mod memory;
pub mod scan;

// `rusqlite` is an optional dependency enabled by this crate's `sqlite`
// feature (default-on); the backend exists exactly when the driver does.
#[cfg(feature = "sqlite")]
pub mod sqlite;

/// The record family an identifier belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableName {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl TableName {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Entry => "entry",
            Self::Task => "task",
            Self::Submission => "submission",
            Self::Document => "document",
        }
    }

    /// Decode the `record_type` text the SQLite backend stores in
    /// `record_ids` (only that backend maps the text back to a family).
    #[cfg(feature = "sqlite")]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "conversation" => Some(Self::Conversation),
            "entry" => Some(Self::Entry),
            "task" => Some(Self::Task),
            "submission" => Some(Self::Submission),
            "document" => Some(Self::Document),
            _ => None,
        }
    }

    /// Whether only one record may ever own this id (the exclusive families);
    /// task/submission ids may be shared across those two families.
    fn is_exclusive(self) -> bool {
        matches!(self, Self::Conversation | Self::Entry | Self::Document)
    }
}

/// Validate one incoming write against the id's existing owner and the
/// batch's earlier claims, then record the claim. Both storage backends run
/// this exact rule; only how `existing` is read differs.
pub(crate) fn check_claimed_id(
    id: i64,
    table: TableName,
    existing: Option<TableName>,
    claimed: &mut HashMap<i64, TableName>,
) -> Result<(), StorageError> {
    if table.is_exclusive() {
        if let Some(existing) = existing {
            return Err(StorageError::invalid(format!(
                "ID {id} already belongs to {}",
                existing.as_str()
            )));
        }
        if claimed.contains_key(&id) {
            return Err(StorageError::invalid(format!(
                "ID {id} is written more than once"
            )));
        }
    } else {
        if let Some(existing) = existing {
            if existing != table {
                return Err(StorageError::invalid(format!(
                    "ID {id} already belongs to {}",
                    existing.as_str()
                )));
            }
        }
        if let Some(earlier) = claimed.get(&id) {
            if *earlier != table {
                return Err(StorageError::invalid(format!(
                    "ID {id} is written as two record types"
                )));
            }
        }
    }
    claimed.insert(id, table);
    Ok(())
}
