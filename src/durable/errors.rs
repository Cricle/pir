//! Error types for the durable storage contract.
//!
//! Upstream `src/errors.ts` uses class hierarchies; here every error is a typed
//! value. [`StorageError`] is the only error the [`Storage`](crate::durable::Storage)
//! trait exposes, so callers never have to downcast or inspect `anyhow` chains.
//! The individual variants mirror the upstream classes one-for-one.

use crate::durable::ids::{ConversationId, TaskId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A transaction read a table after its first table write.
///
/// The Rust subset has no `Tx` surface yet (it belongs to the Session layer), so
/// this type is exported for parity and used by future callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAfterWrite {
    /// The method that was called too late.
    pub method: String,
}

impl fmt::Display for ReadAfterWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Tx.{}() cannot read tables after the first table write",
            self.method
        )
    }
}

impl std::error::Error for ReadAfterWrite {}

/// A storage read rejected an invalid request without any durable effect.
///
/// Examples: an unknown conversation, a cursor produced by another scan, or a
/// historical read of a document that keeps only its current state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct StorageRequestError {
    /// Human-readable explanation.
    pub message: String,
}

impl StorageRequestError {
    /// Build a request error from anything displayable.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A JSONL log could not be decoded or its committed state is inconsistent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct JsonlCorruptionError {
    /// Human-readable explanation.
    pub message: String,
    /// The underlying decoder error, when there was one.
    pub cause: Option<String>,
}

impl JsonlCorruptionError {
    /// Build a corruption error without a cause.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
        }
    }

    /// Build a corruption error with a cause rendered as text.
    pub fn with_cause(message: impl Into<String>, cause: impl fmt::Display) -> Self {
        Self {
            message: message.into(),
            cause: Some(cause.to_string()),
        }
    }
}

/// A JSONL storage failed an IO step and must be reopened.
///
/// Every later operation rejects with this error: the in-memory view can no
/// longer be trusted to match what reached the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("JSONL storage is poisoned and must be reopened: {cause}")]
pub struct JsonlStoragePoisonedError {
    /// The first failure that poisoned the storage.
    pub cause: String,
}

impl JsonlStoragePoisonedError {
    /// Build a poisoned error from the first failure.
    pub fn new(cause: impl Into<String>) -> Self {
        Self {
            cause: cause.into(),
        }
    }
}

/// The session failed after a storage error and must be reopened.
///
/// Upstream `SessionFailed` carries the original error as `cause`; the Rust
/// port carries the [`StorageError`] itself rather than a rendered string, so a
/// caller can classify the failure (a poisoned backend versus corruption)
/// without parsing text. The `Box` breaks the `StorageError::SessionFailed`
/// cycle: the variant exists so a failed session's later calls report the same
/// type through the storage contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Session failed after a storage error; close and reopen it: {cause}")]
pub struct SessionFailed {
    /// The first error, verbatim.
    pub cause: StorageError,
}

/// A submission reached a busy conversation and was not admitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConversationBusy {
    /// The conversation that was busy.
    pub conversation_id: ConversationId,
}

impl fmt::Display for ConversationBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Conversation {} is busy", self.conversation_id)
    }
}

/// Every error a [`Storage`](crate::durable::Storage) operation can return.
///
/// Reads may fail with [`StorageError::Request`] only for an invalid request;
/// any other variant is fatal for the storage instance (the JSONL backend
/// poisons itself).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// The request was invalid and had no durable effect.
    #[error("{0}")]
    Request(#[from] StorageRequestError),
    /// The durable log is corrupt.
    #[error("{0}")]
    Corruption(#[from] JsonlCorruptionError),
    /// The backend failed an IO step and must be reopened.
    #[error("{0}")]
    Poisoned(#[from] JsonlStoragePoisonedError),
    /// The storage was closed.
    #[error("{0}")]
    Closed(String),
    /// A filesystem operation failed.
    #[error("{0}")]
    Io(String),
    /// A record was rejected by the storage contract.
    #[error("{0}")]
    Invalid(String),
    /// The open Session failed; every call rejects with this until it is
    /// reopened (upstream `SessionFailed`, caused by an earlier fatal error).
    ///
    /// Boxed because `SessionFailed` carries the original [`StorageError`];
    /// without the indirection the two types would have infinite size. The
    /// `From<SessionFailed>` impl below keeps `SessionFailed { .. }.into()`
    /// working for callers.
    #[error("{0}")]
    SessionFailed(Box<SessionFailed>),
}

impl From<SessionFailed> for StorageError {
    fn from(error: SessionFailed) -> Self {
        Self::SessionFailed(Box::new(error))
    }
}

impl StorageError {
    /// A closed-storage error naming the backend.
    pub fn closed(backend: &str) -> Self {
        Self::Closed(format!("{backend} is closed"))
    }

    /// An IO error with a context prefix.
    pub fn io(action: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::Io(format!("JSONL {action} failed: {error}"))
    }

    /// A contract violation.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// Whether this error is fatal for the storage instance.
    ///
    /// A `SessionFailed` is the *consequence* of a fatal error, not a new one;
    /// callers that ask "may I keep using this backend?" treat it as fatal too.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Self::Request(_))
    }
}

/// A document definition rejected a value outside storage.
///
/// Upstream `migrate` and `initial` may throw; this value is that throw. It is
/// not a [`StorageError`] because a definition is plain user code that runs
/// before any storage call, and the Session decides whether the failure is
/// reported as [`ReportInfo::MigrationFailed`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DocumentError {
    /// Human-readable explanation.
    pub message: String,
}

impl DocumentError {
    /// Build a document error from anything displayable.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<DocumentError> for StorageError {
    fn from(error: DocumentError) -> Self {
        Self::Invalid(error.message)
    }
}

// ─── Reports ────────────────────────────────────────────────────────────────

/// The task a report is about, matching upstream `ReportTask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportTask {
    /// The task.
    pub id: TaskId,
    /// The task's registered name.
    pub kind: String,
    /// The conversation that owns the task.
    pub conversation_id: ConversationId,
}

/// A host callback, by its `HarnessOptions` name.
///
/// Upstream `HostCallback` is a string union; the wire values are the names
/// below, written in camelCase (`setTimer`, `onWake`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostCallback {
    /// The Harness clock.
    Now,
    /// Host settings.
    Settings,
    /// The task registry.
    Registry,
    /// The environment builder.
    Env,
    /// A provider call.
    Models,
    /// A host timer.
    SetTimer,
    /// The wake notification.
    OnWake,
}

impl HostCallback {
    /// The `HarnessOptions` name, matching the upstream string.
    pub fn name(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::Settings => "settings",
            Self::Registry => "registry",
            Self::Env => "env",
            Self::Models => "models",
            Self::SetTimer => "setTimer",
            Self::OnWake => "onWake",
        }
    }
}

/// Whose code a task's `runtime.report()` is about when it is not the task's
/// own, matching upstream `ReportFrom`.
///
/// A default report (no `from`) is a `task_error`; [`Self::Extension`] names
/// extension code the task called, such as a section, and [`Self::Host`] names
/// a host callback, such as the environment builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportFrom {
    /// Extension code the task called; `extension` names it when known.
    Extension {
        /// The extension whose code failed, when known.
        extension: Option<String>,
    },
    /// A host callback the task called.
    Host {
        /// The failing callback.
        callback: HostCallback,
    },
}

/// Why a task ended `faulted`, matching upstream's `reason` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFaultReason {
    /// A phase threw.
    Threw,
    /// The abort handler was cut off `settings.restartLimit` times.
    RestartLimit,
}

/// Why a running task is outdated, matching upstream's `reason` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutdatedReason {
    /// The definition was uninstalled.
    MissingTask,
    /// The replacement definition cannot take over the task's state.
    IncompatibleTask,
}

/// What a report to `HarnessOptions.onReport` is about.
///
/// Ported from upstream `src/types.ts` `ReportInfo`; the `type` strings are the
/// upstream values exactly. Upstream changelog durable 1.3.0 (Unreleased):
/// `onReport(error, info)`: `info` says what happened in `type`:
/// `session_failed`; `task_faulted`, `task_parked`, `migration_failed`,
/// `task_outdated`, or `task_error` with the task and the extension that
/// installed it; `extension_error` with the extension's name; `host_error` with
/// the failing `HostCallback`; or `listener_error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ReportInfo {
    /// The error failed the Session, which closes.
    SessionFailed,
    /// The task ended `faulted`.
    TaskFaulted {
        /// The task.
        task: ReportTask,
        /// The extension whose code failed, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        /// Why it faulted.
        reason: TaskFaultReason,
    },
    /// The restart limit parked the task until `Harness.retryTask()`.
    TaskParked {
        /// The task.
        task: ReportTask,
        /// The extension whose code failed, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// The task's stored state could not be migrated.
    MigrationFailed {
        /// The task.
        task: ReportTask,
        /// The extension whose code failed, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// A running task keeps running under an uninstalled or incompatible
    /// definition.
    TaskOutdated {
        /// The task.
        task: ReportTask,
        /// The extension whose code failed, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        /// Why it is outdated.
        reason: TaskOutdatedReason,
    },
    /// The task's own `runtime.report()`, or a failure in running it.
    TaskError {
        /// The task.
        task: ReportTask,
        /// The extension that installed the definition the task ran, when
        /// known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// A hook, section, wrapper or tool's invalid structured output failed.
    ExtensionError {
        /// The extension's name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        /// The task that ran it, when inside a task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<ReportTask>,
        /// The conversation whose agent was resolved, outside a task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<ConversationId>,
    },
    /// A host callback threw.
    HostError {
        /// The failing callback.
        callback: HostCallback,
        /// The task that called it, if one did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<ReportTask>,
    },
    /// A commit, close, watch, view or document listener threw.
    ListenerError,
}

impl ReportInfo {
    /// The report a task's `runtime.report()` sends (upstream
    /// `report(error, from)`).
    ///
    /// `extension` is the installer of the definition the task ran, which a
    /// default (`task_error`) report names; an explicit [`ReportFrom`] names
    /// whose code failed instead: the extension the task called, or the host
    /// callback it called.
    #[must_use]
    pub fn task_report(
        task: ReportTask,
        extension: Option<String>,
        from: Option<ReportFrom>,
    ) -> Self {
        match from {
            None => Self::TaskError { task, extension },
            Some(ReportFrom::Extension { extension }) => Self::ExtensionError {
                extension,
                task: Some(task),
                conversation_id: None,
            },
            Some(ReportFrom::Host { callback }) => Self::HostError {
                callback,
                task: Some(task),
            },
        }
    }
}

/// The message of an error value, rendered without any chance of failing.
pub fn error_message(error: &(dyn std::error::Error + 'static)) -> String {
    error.to_string()
}
