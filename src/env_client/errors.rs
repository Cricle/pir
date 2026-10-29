//! Client-side errors (`packages/env/src/errors.ts` + `RemoteError` from
//! `connection.ts`): the daemon's `{ code, message, syscall?, path? }` shape,
//! Node error codes → durable `FileError` codes, and the execution codes.

use std::fmt;

use serde_json::Value;

/// A failure reported by the daemon, with its Node-style code.
///
/// `lost` marks a failure of the connection itself (upstream
/// `isConnectionLost`): the request may succeed once the connection is started
/// again, while a request that was already in flight has an unknown outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
    /// The path the failure happened on, when the daemon reported one.
    pub path: Option<String>,
    /// The connection was lost, not that the daemon refused the request.
    pub lost: bool,
    /// `exec` failures carry the path of the file the output was spilled to.
    pub spill_path: Option<String>,
}

impl RemoteError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> RemoteError {
        RemoteError {
            code: code.into(),
            message: message.into(),
            path: None,
            lost: false,
            spill_path: None,
        }
    }

    /// An error frame's JSON (`{ code, message, syscall?, path? }`); a missing
    /// or non-string code becomes `unknown`, as upstream.
    pub fn from_json(json: &Value) -> RemoteError {
        let code = json
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Remote operation failed");
        RemoteError {
            code: code.to_string(),
            message: message.to_string(),
            path: json.get("path").and_then(Value::as_str).map(str::to_string),
            lost: json.get("lost").and_then(Value::as_bool) == Some(true),
            spill_path: json
                .get("spillPath")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// The connection failed or ended; a request may succeed once it is started again.
    pub fn lost(message: impl Into<String>) -> RemoteError {
        RemoteError {
            lost: true,
            ..RemoteError::new("unknown", message)
        }
    }

    /// The connection was closed by `close()`.
    pub fn closed() -> RemoteError {
        RemoteError::new("unknown", "Connection closed")
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

/// Whether an error means the connection was lost, not that the daemon
/// refused the request (upstream `isConnectionLost`).
pub fn is_connection_lost(error: &RemoteError) -> bool {
    error.lost
}

/// Durable's `FileError` codes (`@earendil-works/pi-durable/env`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    Aborted,
    NotSupported,
    Unknown,
}

impl FileErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FileErrorCode::NotFound => "not_found",
            FileErrorCode::PermissionDenied => "permission_denied",
            FileErrorCode::NotDirectory => "not_directory",
            FileErrorCode::IsDirectory => "is_directory",
            FileErrorCode::Invalid => "invalid",
            FileErrorCode::Aborted => "aborted",
            FileErrorCode::NotSupported => "not_supported",
            FileErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A file operation failure, in durable's `FileError` vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    pub path: Option<String>,
}

impl FileError {
    pub fn new(
        code: FileErrorCode,
        message: impl Into<String>,
        path: impl Into<Option<String>>,
    ) -> FileError {
        FileError {
            code,
            message: message.into(),
            path: path.into(),
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FileError {}

/// Node's error code → durable's `FileError` code (`errors.ts`'s switch).
pub fn file_error_code_for(node_code: &str) -> FileErrorCode {
    match node_code {
        "aborted" => FileErrorCode::Aborted,
        "ENOENT" => FileErrorCode::NotFound,
        "EACCES" | "EPERM" => FileErrorCode::PermissionDenied,
        "ENOTDIR" => FileErrorCode::NotDirectory,
        "EISDIR" => FileErrorCode::IsDirectory,
        "EINVAL" | "SYMLINK" | "NOT_REGULAR" => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    }
}

/// `NodeExecutionEnv`'s mapping of daemon failures to `FileError`s.
pub fn to_file_error(error: &RemoteError, fallback_path: &str) -> FileError {
    FileError {
        code: file_error_code_for(&error.code),
        message: error.message.clone(),
        path: Some(
            error
                .path
                .clone()
                .unwrap_or_else(|| fallback_path.to_string()),
        ),
    }
}

/// An arbitrary client-side failure (an `io::Error`, a resolution error) as a
/// `FileError`, like `errors.ts`'s non-`RemoteError` arm.
pub fn to_file_error_from_message(message: impl Into<String>, fallback_path: &str) -> FileError {
    FileError::new(FileErrorCode::Unknown, message, fallback_path.to_string())
}

/// Durable's `ExecutionError` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

impl ExecutionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionErrorCode::Aborted => "aborted",
            ExecutionErrorCode::Timeout => "timeout",
            ExecutionErrorCode::ShellUnavailable => "shell_unavailable",
            ExecutionErrorCode::SpawnError => "spawn_error",
            ExecutionErrorCode::CallbackError => "callback_error",
            ExecutionErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ExecutionErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A command failure, in durable's `ExecutionError` vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
    /// Output spilled to a file before the failure.
    pub spill_path: Option<String>,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> ExecutionError {
        ExecutionError {
            code,
            message: message.into(),
            spill_path: None,
        }
    }

    /// An `exec` error frame as an `ExecutionError`, with `remote-env.ts`'s
    /// timeout message (`timeout:<seconds>`) and the frame's `spillPath`.
    pub fn from_remote(error: &RemoteError, timeout_seconds: Option<f64>) -> ExecutionError {
        let code = match error.code.as_str() {
            "timeout" => ExecutionErrorCode::Timeout,
            "aborted" => ExecutionErrorCode::Aborted,
            "shell_unavailable" => ExecutionErrorCode::ShellUnavailable,
            "spawn_error" => ExecutionErrorCode::SpawnError,
            _ => ExecutionErrorCode::Unknown,
        };
        let message = match code {
            ExecutionErrorCode::Timeout => format!("timeout:{}", timeout_seconds.unwrap_or(0.0)),
            ExecutionErrorCode::Aborted => "aborted".to_string(),
            ExecutionErrorCode::ShellUnavailable | ExecutionErrorCode::SpawnError => {
                error.message.clone()
            }
            _ => error.message.clone(),
        };
        ExecutionError {
            code,
            message,
            spill_path: error.spill_path.clone(),
        }
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExecutionError {}
