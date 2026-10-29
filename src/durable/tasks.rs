//! Task definitions.
//!
//! Upstream `src/tasks.ts` is eight lines: `defineTask` wraps a definition in a
//! token the registry accepts. The harness execution types are out of this
//! port's scope, so the definition here carries the metadata storage needs —
//! kind, version and the live-phase names — plus the input shape as opaque
//! JSON.

use crate::durable::errors::StorageRequestError;

/// Metadata of one executable task kind.
///
/// The upstream generic parameters (`I`, `S`, `R`, `H`) describe the input,
/// phase state, result and host object. Without the harness those are opaque
/// JSON values, so a single non-generic definition covers every registered
/// task; the version and kind are what the storage layer persists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDefinition {
    /// Registered task definition name, persisted on every record.
    pub kind: String,
    /// Definition version used to migrate live input and checkpoints.
    pub version: i64,
    /// The phases this task can run, in declaration order.
    pub phases: Vec<String>,
}

impl TaskDefinition {
    /// Define an executable task. The kind must be non-empty, the version
    /// positive and at least one phase must be declared.
    pub fn define(
        kind: impl Into<String>,
        version: i64,
        phases: Vec<String>,
    ) -> Result<Self, StorageRequestError> {
        let kind = kind.into();
        if kind.is_empty() {
            return Err(StorageRequestError::new(
                "Task kind must be a non-empty string",
            ));
        }
        if version < 1 {
            return Err(StorageRequestError::new(
                "Task version must be a positive integer",
            ));
        }
        if phases.is_empty() {
            return Err(StorageRequestError::new(
                "Task definition must declare at least one phase",
            ));
        }
        Ok(Self {
            kind,
            version,
            phases,
        })
    }
}

/// A task kind ready to be registered in a harness registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// The definition.
    pub definition: TaskDefinition,
}

/// Define an executable task. Register it in the registry so a harness can run
/// tasks of its kind.
pub fn define_task(definition: TaskDefinition) -> Task {
    Task { definition }
}
