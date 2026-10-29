//! Ls tool: directory listing with an entry cap, `/` directory suffixes, and
//! the upstream truncation notices (upstream `core/tools/ls.ts`).

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::coding_agent::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};

use super::util::{path_error_message, workspace_path};

// ---------------------------------------------------------------------------
// Ls tool
// ---------------------------------------------------------------------------

/// Upstream ls.ts DEFAULT_LIMIT: maximum entries returned.
const LS_DEFAULT_LIMIT: usize = 500;
/// Hard cap on the model-provided `limit` (a JSON number parsed as `f64`):
/// unchecked casts saturate (`1e300` -> `usize::MAX`), so clamp before use.
const LS_MAX_LIMIT: usize = 100_000;

/// Pluggable directory-listing operations (upstream `LsOperations`,
/// ls.ts:34-52). Path normalization and workspace confinement still run in
/// the tool before the backend is called.
pub trait LsOperations: Send + Sync {
    /// Whether the path exists.
    fn exists<'a>(&'a self, absolute_path: &'a std::path::Path) -> BoxFuture<'a, bool>;
    /// Whether the path is a directory; an error means it cannot be inspected.
    fn is_directory<'a>(
        &'a self,
        absolute_path: &'a std::path::Path,
    ) -> BoxFuture<'a, std::io::Result<bool>>;
    /// The directory entry names.
    fn readdir<'a>(
        &'a self,
        absolute_path: &'a std::path::Path,
    ) -> BoxFuture<'a, std::io::Result<Vec<String>>>;
}

pub struct LsTool {
    operations: Option<std::sync::Arc<dyn LsOperations>>,
}

impl LsTool {
    /// The built-in tool (local filesystem).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit listing backend (upstream
    /// `createLsTool(cwd, { operations })`).
    pub fn with_operations(operations: std::sync::Arc<dyn LsOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The pluggable-backend arm of the ls tool: the same normalized,
/// workspace-confined directory prefix and the same sorting/limit/truncation
/// shaping as the local arm.
async fn list_with_operations(
    operations: std::sync::Arc<dyn LsOperations>,
    cwd: &std::path::Path,
    path: &str,
    effective_limit: usize,
    abort_signal: Option<crate::ai::types::AbortSignal>,
) -> ToolResult {
    use crate::ai::types::AbortSignal;

    let list_path = match workspace_path(cwd, path) {
        Ok(path) => path,
        Err(error) => return ToolResult::error(error),
    };
    if !operations.exists(&list_path).await {
        return ToolResult::error(format!("Path not found: {}", list_path.display()));
    }
    if abort_signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    match operations.is_directory(&list_path).await {
        Ok(true) => {}
        Ok(false) => return ToolResult::error(format!("Not a directory: {}", list_path.display())),
        Err(error) => return ToolResult::error(path_error_message(&list_path, &error)),
    }
    let mut names = match operations.readdir(&list_path).await {
        Ok(names) => names,
        Err(error) => {
            return ToolResult::error(format!(
                "Cannot read directory '{}': {error}",
                list_path.display()
            ))
        }
    };
    names.sort_by_cached_key(|name| name.to_lowercase());

    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    let mut unstatable_count = 0usize;
    let mut unstatable_examples: Vec<String> = Vec::new();
    for name in &names {
        if abort_signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return ToolResult::error("Operation aborted");
        }
        if results.len() >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = list_path.join(name);
        match operations.is_directory(&full_path).await {
            Ok(true) => results.push(format!("{name}/")),
            Ok(false) => results.push(name.clone()),
            Err(error) => {
                unstatable_count += 1;
                if unstatable_examples.len() < 5 {
                    unstatable_examples.push(format!("{name} ({error})"));
                }
                // Keep the entry visible; only its file type is unknown.
                results.push(name.clone());
            }
        }
    }
    if names.is_empty() {
        return ToolResult::ok("(empty directory)");
    }

    let raw_output = results.join("\n");
    let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
    // `details.truncation` embeds its own copy of the retained text (upstream
    // session-format parity), so build that JSON while `content` is still
    // whole, then MOVE the retained output into the result. Cloning here left
    // a third full copy of every listing alive until the function returned.
    let truncated = truncation.truncated;
    let truncation_details = truncated.then(|| truncation.to_details_json());
    let mut output = truncation.content;
    let mut details = serde_json::Map::new();
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{effective_limit} entries limit reached. Use limit={} for more",
            effective_limit.saturating_mul(2)
        ));
        details.insert(
            "entryLimitReached".to_owned(),
            serde_json::json!(effective_limit),
        );
    }
    if truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".to_owned(),
            truncation_details.unwrap_or(serde_json::Value::Null),
        );
    }
    if unstatable_count > 0 {
        let noun = if unstatable_count == 1 {
            "entry"
        } else {
            "entries"
        };
        let mut notice = format!("Could not inspect {unstatable_count} {noun}");
        if !unstatable_examples.is_empty() {
            notice.push_str(": ");
            notice.push_str(&unstatable_examples.join(", "));
            if unstatable_count > unstatable_examples.len() {
                notice.push_str(", ...");
            }
        }
        notices.push(notice);
        details.insert(
            "unstatableEntries".to_owned(),
            serde_json::json!(unstatable_count),
        );
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let mut result = ToolResult::ok(output);
    if !details.is_empty() {
        result.details = Some(serde_json::Value::Object(details));
    }
    result
}

impl AgentTool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn description(&self) -> &'static str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first)."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("List directory contents".to_owned())
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
            },
            "required": []
        }))
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let effective_limit = args
            .get("limit")
            .and_then(|v| v.as_f64())
            .map(|limit| limit.max(1.0).min(LS_MAX_LIMIT as f64) as usize)
            .unwrap_or(LS_DEFAULT_LIMIT);
        let cwd = ctx.cwd.clone();
        let aborted = ctx.is_aborted();
        let abort_signal = ctx.signal.clone();
        let operations = self.operations.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error("Operation aborted");
            }
            if let Some(operations) = operations {
                return list_with_operations(
                    operations,
                    &cwd,
                    &path,
                    effective_limit,
                    abort_signal,
                )
                .await;
            }
            let list_path = match workspace_path(&cwd, &path) {
                Ok(path) => path,
                Err(error) => return ToolResult::error(error),
            };
            // Upstream: an existence check, then a directory check, with the
            // ENOENT-style error shapes.
            let metadata = match tokio::fs::metadata(&list_path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    return ToolResult::error(path_error_message(&list_path, &error));
                }
            };
            if !metadata.is_dir() {
                return ToolResult::error(format!("Not a directory: {}", list_path.display()));
            }
            let mut entries = match tokio::fs::read_dir(&list_path).await {
                Ok(entries) => entries,
                Err(error) => {
                    return ToolResult::error(format!(
                        "Cannot read directory '{}': {error}",
                        list_path.display()
                    ))
                }
            };
            // Collect every name, then sort: stopping after `limit + 1`
            // dirents would truncate in filesystem order and could omit
            // alphabetically earlier entries that appeared later in the
            // directory.
            let mut names: Vec<String> = Vec::new();
            loop {
                if abort_signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                {
                    return ToolResult::error("Operation aborted");
                }
                match entries.next_entry().await {
                    Err(error) => {
                        return ToolResult::error(format!(
                            "Cannot read directory '{}': {error}",
                            list_path.display()
                        ))
                    }
                    Ok(None) => break,
                    Ok(Some(entry)) => {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            // Sort alphabetically, case-insensitive. `sort_by_cached_key`
            // lowercases each name once instead of once per comparison.
            names.sort_by_cached_key(|name| name.to_lowercase());

            // Format entries with directory indicators. Unstat-able entries
            // (dangling symlinks, permission failures, races) are still
            // listed by name and reported, rather than silently dropped.
            let mut results: Vec<String> = Vec::new();
            let mut entry_limit_reached = false;
            let mut unstatable_count = 0usize;
            let mut unstatable_examples: Vec<String> = Vec::new();
            for name in &names {
                if abort_signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                {
                    return ToolResult::error("Operation aborted");
                }
                if results.len() >= effective_limit {
                    entry_limit_reached = true;
                    break;
                }
                let full_path = list_path.join(name);
                match tokio::fs::metadata(&full_path).await {
                    Ok(meta) if meta.is_dir() => results.push(format!("{name}/")),
                    Ok(_) => results.push(name.clone()),
                    Err(error) => {
                        unstatable_count += 1;
                        if unstatable_examples.len() < 5 {
                            unstatable_examples.push(format!("{name} ({error})"));
                        }
                        // Keep the entry visible; only its file type is
                        // unknown.
                        results.push(name.clone());
                    }
                }
            }

            // Upstream: an empty directory is a successful "(empty directory)"
            // result. Non-empty directories are never reported as empty, even
            // when every entry failed to stat.
            if names.is_empty() {
                return ToolResult::ok("(empty directory)");
            }

            let raw_output = results.join("\n");
            // Byte truncation only: the entry count is already capped.
            let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
            // Same ownership shape as the local arm above: build the
            // truncation details JSON first, then move `content` out (no
            // third copy of the listing).
            let truncated = truncation.truncated;
            let truncation_details = truncated.then(|| truncation.to_details_json());
            let mut output = truncation.content;
            let mut details = serde_json::Map::new();
            let mut notices: Vec<String> = Vec::new();
            if entry_limit_reached {
                notices.push(format!(
                    "{effective_limit} entries limit reached. Use limit={} for more",
                    effective_limit.saturating_mul(2)
                ));
                details.insert(
                    "entryLimitReached".to_owned(),
                    serde_json::json!(effective_limit),
                );
            }
            if truncated {
                notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
                details.insert(
                    "truncation".to_owned(),
                    truncation_details.unwrap_or(serde_json::Value::Null),
                );
            }
            if unstatable_count > 0 {
                let noun = if unstatable_count == 1 {
                    "entry"
                } else {
                    "entries"
                };
                let mut notice = format!("Could not inspect {unstatable_count} {noun}");
                if !unstatable_examples.is_empty() {
                    notice.push_str(": ");
                    notice.push_str(&unstatable_examples.join(", "));
                    if unstatable_count > unstatable_examples.len() {
                        notice.push_str(", ...");
                    }
                }
                notices.push(notice);
                details.insert(
                    "unstatableEntries".to_owned(),
                    serde_json::json!(unstatable_count),
                );
            }
            if !notices.is_empty() {
                output.push_str(&format!("\n\n[{}]", notices.join(". ")));
            }
            let mut result = ToolResult::ok(output);
            if !details.is_empty() {
                result.details = Some(serde_json::Value::Object(details));
            }
            result
        })
    }
}
