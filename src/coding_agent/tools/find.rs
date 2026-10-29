//! Find tool: file-name glob search on ripgrep's walker (the `ignore` crate
//! — the same engine `fd` is built from, run in-process instead of spawning
//! an external binary), respecting .gitignore, with a result cap (upstream
//! find.ts default path).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::coding_agent::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};

use super::util::{path_error_message, workspace_path};

// ---------------------------------------------------------------------------
// Abort watcher
// ---------------------------------------------------------------------------

/// Abort-watcher tasks currently alive for this tool. Integration tests use
/// the [`AbortWatcherGuard`] seam to assert a cancelled call returns here.
static LIVE_ABORT_WATCHERS: AtomicUsize = AtomicUsize::new(0);

/// Decrements [`LIVE_ABORT_WATCHERS`] when the watcher future is dropped. The
/// guard is moved into the spawned task, so it also covers an abort before the
/// task's first poll.
struct WatcherLiveness;

impl WatcherLiveness {
    fn track() -> Self {
        LIVE_ABORT_WATCHERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for WatcherLiveness {
    fn drop(&mut self) {
        LIVE_ABORT_WATCHERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Owns one call's abort-watcher task. The watcher flips the blocking walk's
/// abort flag when the signal fires. Previously its detached `JoinHandle` was
/// simply dropped when the tool future was cancelled while `spawn_blocking`
/// was still pending: the task stayed parked on `signal.cancelled()` forever,
/// leaking one idle task and a signal clone per cancellation. This guard
/// aborts the watcher on every exit path; when the tool future itself is
/// dropped it also sets the abort flag, so the detached blocking walk stops at
/// its next checkpoint instead of running to completion.
struct AbortWatcherGuard {
    task: Option<tokio::task::JoinHandle<()>>,
    flag: Arc<AtomicBool>,
}

impl AbortWatcherGuard {
    fn spawn(signal: Option<crate::ai::types::AbortSignal>, flag: &Arc<AtomicBool>) -> Self {
        let task = signal.map(|signal| {
            let flag = Arc::clone(flag);
            let liveness = WatcherLiveness::track();
            tokio::spawn(async move {
                let _liveness = liveness;
                signal.cancelled().await;
                flag.store(true, Ordering::SeqCst);
            })
        });
        Self {
            task,
            flag: Arc::clone(flag),
        }
    }

    /// The blocking walk finished normally: stop watching without touching the
    /// flag (setting it would turn a completed search into an abort error).
    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for AbortWatcherGuard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.flag.store(true, Ordering::SeqCst);
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Find tool
// ---------------------------------------------------------------------------

/// Upstream find.ts DEFAULT_LIMIT: maximum results returned.
const FIND_DEFAULT_LIMIT: usize = 1000;
/// Hard cap on the model-provided `limit` (a JSON number parsed as `f64`):
/// unchecked casts saturate (`1e300` -> `usize::MAX`), so clamp before use.
const FIND_MAX_LIMIT: usize = 100_000;

/// Relativize a find result against the search root, normalize it to posix
/// separators, and preserve fd's trailing separator for directory results
/// (upstream `relativizeFindResultPath`).
fn relativize_find_result_path(result_path: &Path, search_path: &Path, is_dir: bool) -> String {
    let relative = match result_path.strip_prefix(search_path) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().into_owned(),
        _ => result_path.to_string_lossy().into_owned(),
    };
    let mut rendered = relative.replace('\\', "/");
    if is_dir && !rendered.ends_with('/') {
        rendered.push('/');
    }
    rendered
}

/// Whether a `.git` entry (directory or file, e.g. a worktree pointer) exists
/// at `dir` or any of its ancestors (upstream find.ts insideGitRepo walk).
fn is_inside_git_repo(search_path: &Path) -> bool {
    let mut current: Option<&Path> = Some(search_path);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// Search parameters for [`find_sync`].
struct FindRequest<'a> {
    root: &'a Path,
    pattern: &'a str,
    limit: usize,
    abort: &'a AtomicBool,
}

/// Blocking walker core (fd --glob --hidden equivalent): collect up to
/// `limit` matching paths (with their directory flag), respecting .gitignore
/// everywhere (fd's --no-require-git outside repos). Returns the paths in
/// walk order and whether the cap stopped the walk.
fn find_sync(request: FindRequest<'_>) -> Result<(Vec<(PathBuf, bool)>, bool), String> {
    let FindRequest {
        root,
        pattern,
        limit,
        abort,
    } = request;
    let mut builder = ignore::WalkBuilder::new(root);
    // `fd --hidden`: include hidden files/directories (the walker's default
    // SKIPS them: `hidden(true)` means "ignore hidden", walk.rs docs); ignore
    // rules stay on.
    builder.hidden(false);
    // fd never descends into VCS internals. Prune `.git` during the walk
    // instead of filtering result paths: result filtering also discarded every
    // hit when the search root itself was inside `.git`. Depth 0 is exempt so
    // explicitly searching a `.git` directory still works.
    builder.filter_entry(|entry| entry.depth() == 0 || entry.file_name() != OsStr::new(".git"));
    // Honor .gitignore outside a git repository too (upstream passes
    // --no-require-git when the search path is not inside a repo).
    if !is_inside_git_repo(root) {
        builder.require_git(false);
    }

    // fd's glob matching, via the ignore crate's gitignore-glob overrides
    // (maintained inside ripgrep's workspace): a pattern without a separator
    // matches the file name at any depth; a pattern WITH a separator is
    // matched against the full path with an implicit `**/` prefix, so
    // `src/**/*.spec.ts` also matches `nested/src/**/*.spec.ts` (upstream
    // find.ts:204-213).
    let effective_pattern =
        if pattern.contains('/') && !pattern.starts_with('/') && !pattern.starts_with("**/") {
            format!("**/{pattern}")
        } else {
            pattern.to_owned()
        };
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    overrides
        .add(&effective_pattern)
        .map_err(|error| format!("Invalid glob: {error}"))?;
    let overrides = overrides
        .build()
        .map_err(|error| format!("Invalid glob: {error}"))?;

    let mut found: Vec<(PathBuf, bool)> = Vec::new();
    let mut first_walk_error: Option<String> = None;
    for entry in builder.build() {
        if abort.load(Ordering::SeqCst) {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // Remember the first walk error: if the walk produced no
                // result at all, the caller must get a real error rather than
                // "No files found" (an unreadable root used to look empty).
                if first_walk_error.is_none() {
                    first_walk_error = Some(error.to_string());
                }
                continue;
            }
        };
        // The root itself is not a find result; fd only lists entries that
        // actually match the pattern.
        if entry.depth() == 0 {
            continue;
        }
        let is_dir = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        // Match against the posix relative path so '/' patterns hold on
        // Windows too. `false`: the trailing separator is display-only and
        // must not leak into glob matching.
        let relative = relativize_find_result_path(entry.path(), root, false);
        let relative = PathBuf::from(&relative);
        if !overrides.matched(&relative, is_dir).is_whitelist() {
            continue;
        }
        // Reaching the limit is not by itself proof of truncation: continue
        // until one more match is seen. A directory with exactly `limit`
        // matches must not produce a "limit reached" notice.
        if found.len() >= limit {
            return Ok((found, true));
        }
        found.push((entry.path().to_path_buf(), is_dir));
    }
    // No result and an unreadable root/tree: surface the first walk error so
    // the caller reports a failure instead of "No files found". Matches win
    // over partial walk errors, matching fd (it only fails when it has no
    // output).
    if found.is_empty() {
        if let Some(error) = first_walk_error {
            return Err(format!("Failed to search '{}': {error}", root.display()));
        }
    }
    Ok((found, false))
}

/// Options for a pluggable [`FindOperations::glob`] call.
pub struct FindGlobOptions {
    /// Glob patterns to exclude (upstream passes node_modules/.git).
    pub ignore: Vec<String>,
    /// Maximum number of results.
    pub limit: usize,
}

/// Pluggable file-search operations (upstream `FindOperations`,
/// find.ts:52-68). Path normalization and workspace confinement still run in
/// the tool before the backend is called.
pub trait FindOperations: Send + Sync {
    /// Whether the search path exists.
    fn exists<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, bool>;
    /// Find paths matching `pattern` below `cwd`.
    fn glob<'a>(
        &'a self,
        pattern: &'a str,
        cwd: &'a Path,
        options: FindGlobOptions,
    ) -> BoxFuture<'a, Result<Vec<String>, String>>;
}

pub struct FindTool {
    operations: Option<std::sync::Arc<dyn FindOperations>>,
}

impl FindTool {
    /// The built-in tool (in-process ripgrep walker).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit search backend (upstream
    /// `createFindTool(cwd, { operations })`).
    pub fn with_operations(operations: std::sync::Arc<dyn FindOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }

    /// Test seam: abort-watcher tasks currently alive for this tool. A
    /// cancelled call must leave this at its pre-call baseline (integration
    /// tests live in `tests/`).
    #[doc(hidden)]
    pub fn live_abort_watchers_for_tests() -> usize {
        LIVE_ABORT_WATCHERS.load(Ordering::SeqCst)
    }
}

impl Default for FindTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Relativize a custom-backend result against the search root, posix-slashed,
/// preserving a trailing separator (upstream `relativizeFindResultPath`).
fn relativize_custom_find_result(result: &str, search_path: &Path) -> String {
    let had_trailing_separator = result.ends_with('/') || result.ends_with('\\');
    let path = Path::new(result);
    let relative = if path.is_absolute() {
        path.strip_prefix(search_path)
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|_| result.to_owned())
    } else {
        result.to_owned()
    };
    let mut rendered = relative.replace('\\', "/");
    if had_trailing_separator && !rendered.ends_with('/') {
        rendered.push('/');
    }
    rendered
}

/// The pluggable-backend arm of the find tool: the confined search root, then
/// the backend's `glob` results relativized and rendered exactly like the
/// local walker's.
async fn find_with_operations(
    operations: std::sync::Arc<dyn FindOperations>,
    pattern: &str,
    search_path: &Path,
    effective_limit: usize,
    abort_signal: Option<crate::ai::types::AbortSignal>,
) -> ToolResult {
    use crate::ai::types::AbortSignal;

    if !operations.exists(search_path).await {
        return ToolResult::error(format!("Path not found: {}", search_path.display()));
    }
    if abort_signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    let results = match operations
        .glob(
            pattern,
            search_path,
            FindGlobOptions {
                ignore: vec!["**/node_modules/**".to_owned(), "**/.git/**".to_owned()],
                limit: effective_limit,
            },
        )
        .await
    {
        Ok(results) => results,
        Err(error) => return ToolResult::error(error),
    };
    if abort_signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    if results.is_empty() {
        return ToolResult::ok("No files found matching pattern");
    }
    let relativized: Vec<String> = results
        .iter()
        .map(|result| relativize_custom_find_result(result, search_path))
        .collect();
    let limit_reached = relativized.len() >= effective_limit;
    let raw_output = relativized.join("\n");
    let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
    // `details.truncation` embeds its own copy of the retained paths (upstream
    // session parity), so build it first and MOVE `content` into the result.
    // The old clone held a second full result list until function return.
    let truncated = truncation.truncated;
    let truncation_details = truncated.then(|| truncation.to_details_json());
    let mut result_output = truncation.content;
    let mut details = serde_json::Map::new();
    let mut notices: Vec<String> = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
            effective_limit.saturating_mul(2)
        ));
        details.insert(
            "resultLimitReached".to_owned(),
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
    if !notices.is_empty() {
        result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let mut result = ToolResult::ok(result_output);
    if !details.is_empty() {
        result.details = Some(serde_json::Value::Object(details));
    }
    result
}

impl AgentTool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }
    fn description(&self) -> &'static str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first)."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("Find files by glob pattern (respects .gitignore)".to_owned())
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            },
            "required": ["pattern"]
        }))
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let effective_limit = args
            .get("limit")
            .and_then(|v| v.as_f64())
            .map(|limit| limit.max(1.0).min(FIND_MAX_LIMIT as f64) as usize)
            .unwrap_or(FIND_DEFAULT_LIMIT);
        let cwd = ctx.cwd.clone();
        let aborted = ctx.is_aborted();
        let abort_signal = ctx.signal.clone();
        let operations = self.operations.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error("Operation aborted");
            }
            let search_path: PathBuf = match workspace_path(&cwd, &path) {
                Ok(path) => path,
                Err(error) => return ToolResult::error(error),
            };
            if let Some(operations) = operations {
                return find_with_operations(
                    operations,
                    &pattern,
                    &search_path,
                    effective_limit,
                    abort_signal,
                )
                .await;
            }
            // Existence/permission guard shared with grep/ls: a missing or
            // unreadable path must be an error, not "No files found". Upstream
            // fd also rejects a non-directory search path.
            let metadata = match tokio::fs::metadata(&search_path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    return ToolResult::error(path_error_message(&search_path, &error));
                }
            };
            if !metadata.is_dir() {
                return ToolResult::error(format!("Not a directory: {}", search_path.display()));
            }

            // The walk is blocking I/O; run it on a blocking thread with the
            // abort flag checked per entry.
            let abort_flag = Arc::new(AtomicBool::new(aborted));
            let mut abort_watcher = AbortWatcherGuard::spawn(abort_signal, &abort_flag);
            let walk_root = search_path.clone();
            let walk_pattern = pattern.clone();
            let walk_flag = Arc::clone(&abort_flag);
            let walked = tokio::task::spawn_blocking(move || {
                find_sync(FindRequest {
                    root: &walk_root,
                    pattern: &walk_pattern,
                    limit: effective_limit,
                    abort: &walk_flag,
                })
            })
            .await;
            abort_watcher.stop();
            let (paths, limit_reached) = match walked {
                Ok(Ok(found)) => found,
                Ok(Err(error)) => return ToolResult::error(error),
                Err(error) => return ToolResult::error(format!("find failed: {error}")),
            };
            if abort_flag.load(Ordering::SeqCst) {
                return ToolResult::error("Operation aborted");
            }

            let relativized: Vec<String> = paths
                .iter()
                .map(|(found, is_dir)| relativize_find_result_path(found, &search_path, *is_dir))
                .collect();
            // Upstream: an empty result set is a successful result.
            if relativized.is_empty() {
                return ToolResult::ok("No files found matching pattern");
            }

            let raw_output = relativized.join("\n");
            let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
            // Same ownership shape as the pre-spawn find arm: details JSON
            // first, then move the retained paths into the result.
            let truncated = truncation.truncated;
            let truncation_details = truncated.then(|| truncation.to_details_json());
            let mut result_output = truncation.content;
            let mut details = serde_json::Map::new();
            let mut notices: Vec<String> = Vec::new();
            if limit_reached {
                notices.push(format!(
                    "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
                    effective_limit.saturating_mul(2)
                ));
                details.insert(
                    "resultLimitReached".to_owned(),
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
            if !notices.is_empty() {
                result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
            }
            let mut result = ToolResult::ok(result_output);
            if !details.is_empty() {
                result.details = Some(serde_json::Value::Object(details));
            }
            result
        })
    }
}
