//! Session state held by the session registry.
//!
//! This is the live remainder of the former `session_backends` module: the
//! file-backed storage layer and the entry/lane/lease shapes are gone, and
//! the registry only tracks the metadata its snapshots are built from.

/// Live session state: the metadata the protocol snapshots are built from.
pub struct SessionState {
    pub metadata: SessionMetadata,
}

/// Internal session metadata (id + display name + timestamps); the wire
/// `SessionMetadata` is projected from it by `SessionRecord::to_metadata`.
pub struct SessionMetadata {
    pub id: String,
    pub name: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl SessionState {
    pub fn new(id: impl Into<String>, timestamp: u64) -> Self {
        Self {
            metadata: SessionMetadata {
                id: id.into(),
                name: None,
                created_at: timestamp,
                updated_at: timestamp,
            },
        }
    }
}
