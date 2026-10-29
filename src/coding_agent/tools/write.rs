//! Write tool: writes file content, creating parent directories as needed.
//!
//! Local writes go through [`ConfinedTarget::write_atomically`], which is
//! durable on every call: the payload is flushed with `sync_all` before the
//! tool reports success. There is deliberately no settings switch for that
//! fsync (see the `confine` module docs): crash safety is not a configuration
//! tradeoff. Ordinary files are replaced by a same-directory temp file plus
//! rename, so readers never observe a half-written file; only the permission
//! bits survive (ACLs and extended attributes do not — the new inode starts
//! with the directory's defaults). A target with multiple hard links is
//! rewritten in place instead, so every link sees the new bytes and the inode,
//! ACLs, and xattrs are kept, at the cost of atomicity. The remote arm keeps
//! its own temp+flush+rename protocol; it has no hard-link detection.

use std::path::Path;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::ai::types::AbortSignal;

use super::confine::{confinement_error, ConfinedTarget};
use super::util::{
    single_path_arg, workspace_path_parts, workspace_path_remote_checked,
    write_remote_file_atomically,
};

/// Local writes are chunked so a large payload observes cancellation between
/// chunks; the size mirrors the remote layer's `WRITE_CHUNK`.
const LOCAL_WRITE_CHUNK_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// Write tool
// ---------------------------------------------------------------------------

/// Pluggable filesystem operations for the write tool (upstream
/// `WriteOperations`, write.ts:27-42). Path normalization and workspace
/// confinement still run in the tool before the backend is called.
pub trait WriteOperations: Send + Sync {
    /// Write `content` to the file (replacing it if it exists).
    fn write_file<'a>(
        &'a self,
        absolute_path: &'a Path,
        content: &'a str,
    ) -> BoxFuture<'a, std::io::Result<()>>;
    /// Create a directory recursively.
    fn mkdir<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, std::io::Result<()>>;
}

pub struct WriteTool {
    operations: Option<std::sync::Arc<dyn WriteOperations>>,
}

impl WriteTool {
    /// The built-in tool (local filesystem, or the `env_client` remote arm
    /// when configured).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit write backend (upstream
    /// `createWriteTool(cwd, { operations })`). An injected backend wins over
    /// the process-wide `env_client` selection.
    pub fn with_operations(operations: std::sync::Arc<dyn WriteOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("Create or overwrite files".to_owned())
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Use write only for new files or complete rewrites.".to_owned()]
    }
    fn is_mutation_tool(&self) -> bool {
        true
    }
    fn mutation_paths(&self, args: &serde_json::Value, cwd: &Path) -> Vec<std::path::PathBuf> {
        single_path_arg(args, cwd).1
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        }))
    }
    /// Upstream `write.ts:57`: the write tool prefers strict JSON-schema
    /// constrained sampling.
    fn constrained_sampling(&self) -> Option<crate::ai::types::ConstrainedSamplingConfig> {
        Some(crate::ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: crate::ai::types::StrictPreference::Prefer,
        })
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let (path, _) = single_path_arg(args, &ctx.cwd);
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cwd = ctx.cwd.clone();
        let aborted = ctx.is_aborted();
        let signal = ctx.signal.clone();
        // The env indirection layer (`env_client::selection`): `None` — the
        // default — leaves the local path below byte-identical.
        let remote = crate::env_client::configured_remote_env();
        let operations = self.operations.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error("Operation aborted");
            }
            if let Some(operations) = operations {
                return write_with_operations(operations, &cwd, &path, &content, signal).await;
            }
            if let Some(environment) = remote {
                return write_remote(environment, &path, &content, signal).await;
            }
            // The name-based confinement check (parent components, symlink
            // components, root containment) produces the model-facing errors;
            // the write itself then runs through a handle anchored at the
            // verified parent directory, so a component swapped after the check
            // cannot redirect it (no resolve-then-open window).
            let parts = match workspace_path_parts(&cwd, &path) {
                Ok(parts) => parts,
                Err(error) => return ToolResult::error(error),
            };
            let target = match ConfinedTarget::open(&parts.root, &parts.relative, true) {
                Ok(target) => target,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    // Either the handle walk's own policy refusal or a real OS
                    // denial (`EACCES`); `confinement_error` tells them apart.
                    return ToolResult::error(confinement_error(&path, &error));
                }
                Err(error) => {
                    return ToolResult::error(format!(
                        "Cannot create parent directory '{}': {}",
                        parent_display(&path),
                        error
                    ))
                }
            };
            match target
                .write_atomically(content.as_bytes(), signal.as_ref(), LOCAL_WRITE_CHUNK_BYTES)
                .await
            {
                // Upstream e583b290a: byte counts were UTF-16 chars — misleading.
                Ok(true) => ToolResult::ok(format!("Successfully wrote to {path}")),
                Ok(false) => ToolResult::error("Operation aborted"),
                Err(e) => ToolResult::error(format!("Cannot write '{}': {}", path, e)),
            }
        })
    }
}

/// The pluggable-backend arm of the write tool: the same normalized,
/// workspace-confined target the local arm resolves, then the backend creates
/// the parent and writes the content.
async fn write_with_operations(
    operations: std::sync::Arc<dyn WriteOperations>,
    cwd: &Path,
    path: &str,
    content: &str,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let parts = match workspace_path_parts(cwd, path) {
        Ok(parts) => parts,
        Err(error) => return ToolResult::error(error),
    };
    let target = parts.candidate;
    let directory = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf);
    if let Some(directory) = directory {
        if let Err(error) = operations.mkdir(&directory).await {
            return ToolResult::error(format!(
                "Cannot create parent directory '{}': {}",
                parent_display(path),
                error
            ));
        }
    }
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    match operations.write_file(&target, content).await {
        Ok(()) if signal.as_ref().is_some_and(AbortSignal::is_aborted) => {
            ToolResult::error("Operation aborted")
        }
        Ok(()) => ToolResult::ok(format!("Successfully wrote to {path}")),
        Err(error) => ToolResult::error(format!("Cannot write '{}': {}", path, error)),
    }
}

/// The `path`'s parent, for the "cannot create parent directory" message.
fn parent_display(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.display().to_string(),
        _ => ".".to_owned(),
    }
}

/// Wait until the signal aborts; never resolves when no signal is installed.
async fn wait_for_abort(signal: Option<&AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Race one local filesystem await against the abort signal. `None` means the
/// signal fired first; otherwise the await's own result is returned unchanged.
async fn cancellable<T>(
    signal: Option<&AbortSignal>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        value = future => Some(value),
        _ = wait_for_abort(signal) => None,
    }
}

/// The remote arm of the write tool (the env indirection layer's `Some`
/// branch): `RemoteExecutionEnv::writeFile`, whose first `write` request
/// creates missing parents like Node's recursive `mkdir` — the remote
/// equivalent of the local arm's parent creation. The write goes to a temp
/// sibling that is renamed over the target, so an aborted or failed remote
/// write never leaves a truncated file behind (matching the local arm's
/// atomic replacement). The messages are the local arm's, so a model sees the
/// same result shape on either machine.
async fn write_remote(
    environment: std::sync::Arc<crate::env_client::RemoteExecutionEnv>,
    path: &str,
    content: &str,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let context = crate::env_client::Context::new(signal.clone());
    // The shared remote resolver plus the same lstat walk the local arm uses:
    // a dangling symlink on the remote machine must not redirect the write
    // outside the remote workspace.
    let file_path = match cancellable(
        signal.as_ref(),
        workspace_path_remote_checked(&environment, path),
    )
    .await
    {
        Some(Ok(path)) => path,
        Some(Err(error)) => return ToolResult::error(error),
        None => return ToolResult::error("Operation aborted"),
    };
    match write_remote_file_atomically(&environment, &file_path, content.as_bytes(), &context).await
    {
        Ok(()) => ToolResult::ok(format!("Successfully wrote to {path}")),
        Err(error) if error.code == crate::env_client::FileErrorCode::Aborted => {
            ToolResult::error("Operation aborted")
        }
        Err(error) => ToolResult::error(format!("Cannot write '{}': {}", path, error.message)),
    }
}
