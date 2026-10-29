//! Grep tool: content search on ripgrep's own engine (the `grep-searcher` /
//! `grep-regex` / `ignore` crates the `rg` binary is built from, run
//! in-process instead of spawning an external binary), with match limits,
//! per-line truncation, glob filtering, and optional context blocks.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::ai::types::AbortSignal;
use crate::coding_agent::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};

use super::grep_engine::{search_matches, SearchRequest};
use super::util::{max_source_bytes, path_error_message, source_limit_error, workspace_path};

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

/// Owns one call's abort-watcher task. The watcher flips the blocking search's
/// abort flag when the signal fires. Previously its detached `JoinHandle` was
/// simply dropped when the tool future was cancelled while `spawn_blocking`
/// was still pending: the task stayed parked on `signal.cancelled()` forever,
/// leaking one idle task and a signal clone per cancellation. This guard
/// aborts the watcher on every exit path; when the tool future itself is
/// dropped it also sets the abort flag, so the detached blocking search stops
/// at its next checkpoint instead of running to completion.
struct AbortWatcherGuard {
    task: Option<tokio::task::JoinHandle<()>>,
    flag: Arc<AtomicBool>,
}

impl AbortWatcherGuard {
    fn spawn(signal: Option<AbortSignal>, flag: &Arc<AtomicBool>) -> Self {
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

    /// The blocking scan finished normally: stop watching without touching the
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
// Grep tool
// ---------------------------------------------------------------------------

/// Upstream grep.ts DEFAULT_LIMIT: maximum matches returned.
const GREP_DEFAULT_LIMIT: usize = 100;
/// Upstream truncate.ts GREP_MAX_LINE_LENGTH: max chars per match line.
const GREP_MAX_LINE_LENGTH: usize = 500;
/// Upstream applies `truncateHead(rawOutput, { maxLines: Number.MAX_SAFE_INTEGER })`:
/// grep output is byte-limited only, because the match limit already caps the
/// row count. `MAX_SAFE_INTEGER` is mirrored exactly so
/// `details.truncation.maxLines` matches upstream on 64-bit builds.
const GREP_TRUNCATION_MAX_LINES: usize = if usize::BITS >= 64 {
    MAX_SAFE_INTEGER as usize
} else {
    usize::MAX
};
/// Upstream `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Hard caps on the model-provided numeric arguments. `context` / `limit`
/// arrive as JSON numbers (f64) and unchecked casts saturate (`1e300` ->
/// `usize::MAX`), so values are clamped before any integer conversion.
const GREP_MAX_CONTEXT: usize = 1000;
const GREP_MAX_LIMIT: usize = 100_000;
/// Upper bound on the match text retained from one search. It is passed to
/// the searcher ([`super::grep_engine::search_matches`]), which stops
/// retaining whole lines once the budget is reached (so a broad search on
/// long lines cannot balloon memory inside the engine); the consumption loop
/// below repeats the same bound as a second guard. Each retained line also
/// remains bounded by the source-file cap when context formatting re-reads
/// it.
const GREP_MAX_COLLECTED_MATCH_BYTES: usize = 8 * 1024 * 1024;

/// One explicit notice for a numeric argument a hard cap actually reduced.
/// `requested` is the raw JSON number the model sent, kept in its original
/// form (`200000`, `1e300`) so the notice says exactly what was asked for.
/// Returns `None` when the value is absent/non-numeric or already within the
/// cap, which keeps the no-clamp output byte-identical to before.
fn clamp_notice(name: &str, cap: usize, requested: Option<&serde_json::Value>) -> Option<String> {
    let requested = requested?;
    let value = requested.as_f64()?;
    (value > cap as f64).then(|| format!("{name} clamped to {cap}; requested {requested}"))
}

/// One ripgrep match (upstream `matches[]` entries).
struct GrepMatch {
    file_path: String,
    line_number: u64,
    /// The matched line text from the ripgrep event (absent for custom
    /// backends; pir's local ripgrep always provides it).
    line_text: Option<String>,
}

/// Upstream `truncateLine`: cut a match line to max chars with a marker.
/// Returns `(text, was_truncated)`.
fn truncate_line(line: &str) -> (String, bool) {
    if line.chars().count() <= GREP_MAX_LINE_LENGTH {
        (line.to_owned(), false)
    } else {
        (
            format!(
                "{}... [truncated]",
                line.chars().take(GREP_MAX_LINE_LENGTH).collect::<String>()
            ),
            true,
        )
    }
}

/// The renderer's display budget is byte-only (upstream grep.ts:284): the
/// match limit, not a line cutoff, decides how many rows are emitted.
fn push_output_line(
    output_lines: &mut Vec<String>,
    output_bytes: &mut usize,
    line: String,
) -> bool {
    *output_bytes = output_bytes.saturating_add(line.len() + usize::from(!output_lines.is_empty()));
    output_lines.push(line);
    // The crossing line is kept so `truncate_head` below still sees an
    // over-limit input and reports the normal truncation details.
    *output_bytes > DEFAULT_MAX_BYTES
}

/// Render a ripgrep-reported path relative to the search root (directory
/// search) or as a bare file name (single-file search), with posix
/// separators (upstream grep.ts `formatPath`).
fn format_grep_path(file_path: &str, search_path: &Path, is_directory: bool) -> String {
    let reported = Path::new(file_path);
    if is_directory {
        if let Ok(relative) = reported.strip_prefix(search_path) {
            let rendered = relative.to_string_lossy().replace('\\', "/");
            if !rendered.starts_with("..") {
                return rendered;
            }
        }
    }
    reported
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_owned())
}

/// Pluggable filesystem operations for the grep tool (upstream
/// `GrepOperations`, grep.ts:53-68). The in-process search engine stays local
/// (upstream always spawns ripgrep); these cover the directory check and the
/// context-line reads.
pub trait GrepOperations: Send + Sync {
    /// Whether the search path is a directory; an error means it cannot be
    /// inspected (upstream reports "Path not found").
    fn is_directory<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<bool>>;
    /// Read a file for context lines.
    fn read_file<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<String>>;
}

pub struct GrepTool {
    operations: Option<std::sync::Arc<dyn GrepOperations>>,
}

impl GrepTool {
    /// The built-in tool (in-process ripgrep engine).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit grep backend (upstream
    /// `createGrepTool(cwd, { operations })`).
    pub fn with_operations(operations: std::sync::Arc<dyn GrepOperations>) -> Self {
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

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("Search file contents for patterns (respects .gitignore)".to_owned())
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
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
        let glob = args.get("glob").and_then(|v| v.as_str()).map(str::to_owned);
        let ignore_case = args.get("ignoreCase").and_then(|v| v.as_bool());
        let literal = args.get("literal").and_then(|v| v.as_bool());
        // Upstream: `context && context > 0 ? context : 0`. Clamp before the
        // usize cast: NaN/negative -> 0, infinity/huge -> GREP_MAX_CONTEXT.
        // The requested value is kept so a real clamp is reported explicitly
        // below instead of silently shrinking the model's request.
        let context_arg = args.get("context");
        let context = context_arg
            .and_then(|v| v.as_f64())
            .map(|value| value.max(0.0).min(GREP_MAX_CONTEXT as f64) as usize)
            .unwrap_or(0);
        let limit_arg = args.get("limit");
        let effective_limit = limit_arg
            .and_then(|v| v.as_f64())
            .map(|limit| limit.max(1.0).min(GREP_MAX_LIMIT as f64) as usize)
            .unwrap_or(GREP_DEFAULT_LIMIT);
        // Text-only clamp notices (never in `details`): they appear only when
        // the requested value exceeded the hard cap.
        let clamp_notices: Vec<String> = [
            clamp_notice("context", GREP_MAX_CONTEXT, context_arg),
            clamp_notice("limit", GREP_MAX_LIMIT, limit_arg),
        ]
        .into_iter()
        .flatten()
        .collect();
        let cwd = ctx.cwd.clone();
        let aborted = ctx.is_aborted();
        let abort_signal = ctx.signal.clone();
        let operations = self.operations.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error("Operation aborted");
            }
            let search_path = match workspace_path(&cwd, &path) {
                Ok(path) => path,
                Err(error) => return ToolResult::error(error),
            };
            let is_directory = match &operations {
                Some(operations) => match operations.is_directory(&search_path).await {
                    Ok(is_directory) => is_directory,
                    Err(_) => {
                        return ToolResult::error(format!(
                            "Path not found: {}",
                            search_path.display()
                        ))
                    }
                },
                None => {
                    let metadata = match tokio::fs::metadata(&search_path).await {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            return ToolResult::error(path_error_message(&search_path, &error));
                        }
                    };
                    metadata.is_dir()
                }
            };
            // In-process search on ripgrep's own crates (`rg --hidden
            // --line-number` semantics: hidden files searched, .gitignore /
            // .ignore respected, binary files skipped). The walk and match
            // loop are blocking I/O, so they run on a blocking thread with an
            // abort flag checked per match.
            let abort_flag = Arc::new(AtomicBool::new(aborted));
            let mut abort_watcher = AbortWatcherGuard::spawn(abort_signal, &abort_flag);
            let search_pattern = pattern.clone();
            let search_glob = glob.clone();
            let search_root = search_path.clone();
            let search_flag = Arc::clone(&abort_flag);
            // The renderer's only display bound is the 50KB byte budget
            // (upstream grep.ts:284), so the scan's match limit is exactly the
            // caller's `limit`; the collection byte budget below bounds memory.
            let requested_limit = effective_limit;
            let search_limit = requested_limit;
            let searched = tokio::task::spawn_blocking(move || {
                let globs: Vec<String> = search_glob.iter().cloned().collect();
                search_matches(
                    SearchRequest {
                        root: &search_root,
                        is_directory,
                        pattern: &search_pattern,
                        ignore_case: ignore_case == Some(true),
                        literal: literal == Some(true),
                        word: false,
                        globs: &globs,
                        excludes: &[],
                        respect_ignore: true,
                        limit: search_limit,
                        abort: &search_flag,
                    },
                    GREP_MAX_COLLECTED_MATCH_BYTES,
                )
            })
            .await;
            abort_watcher.stop();
            let (raw_matches, raw_limit_reached, engine_byte_limit_hit) = match searched {
                Ok(Ok(found)) => found,
                Ok(Err(error)) => return ToolResult::error(error),
                Err(error) => return ToolResult::error(format!("Failed to run ripgrep: {error}")),
            };
            if abort_flag.load(Ordering::SeqCst) {
                return ToolResult::error("Operation aborted");
            }
            // `search_matches` returns its vector already bounded by the
            // engine-side retained-text budget; the loop below stays as a
            // second guard (and reports the same flag) in case a future
            // backend returns uninterruptible matches.
            let mut collected_bytes = 0usize;
            let mut collected_byte_limit_hit = engine_byte_limit_hit;
            let mut matches: Vec<GrepMatch> = Vec::new();
            for found in raw_matches {
                let next = collected_bytes.saturating_add(found.line_text.len());
                if next > GREP_MAX_COLLECTED_MATCH_BYTES {
                    collected_byte_limit_hit = true;
                    break;
                }
                collected_bytes = next;
                matches.push(GrepMatch {
                    file_path: found.path.to_string_lossy().into_owned(),
                    line_number: found.line_number,
                    line_text: Some(found.line_text),
                });
            }
            let match_limit_reached = raw_limit_reached;

            // Text-only clamp suffix (never part of `details`): reported on
            // its own line so existing structured-output assertions and the
            // no-clamp output stay byte-identical.
            let clamp_suffix = if clamp_notices.is_empty() {
                String::new()
            } else {
                format!("\n[{}]", clamp_notices.join(". "))
            };

            // Upstream: zero matches is a successful result. A byte-capped
            // non-empty result must say why it could not be displayed.
            if matches.is_empty() {
                if collected_byte_limit_hit {
                    return ToolResult::error(format!(
                        "Match data exceeds the {} collection limit; narrow the pattern or path.",
                        format_size(GREP_MAX_COLLECTED_MATCH_BYTES)
                    ));
                }
                let mut output = "No matches found".to_owned();
                output.push_str(&clamp_suffix);
                return ToolResult::ok(output);
            }

            // Format the matches: context == 0 uses the ripgrep match line
            // directly; context > 0 (or a missing line text) re-reads each
            // file for the surrounding block (upstream `formatBlock`). Only
            // the current file is cached: matches arrive grouped by file, and
            // a one-entry cache bounds memory to one source file.
            let mut file_cache: Option<(String, Arc<Vec<String>>)> = None;
            let mut output_lines: Vec<String> = Vec::new();
            let mut output_bytes = 0usize;
            let mut lines_truncated = false;
            // Stop rendering as soon as one line crosses the display budget.
            // The crossing line is kept so `truncate_head` below still sees an
            // over-limit input and reports the normal truncation details.
            'matches: for entry in &matches {
                let relative = format_grep_path(&entry.file_path, &search_path, is_directory);
                let direct = context == 0;
                let direct = direct.then(|| {
                    entry.line_text.as_ref().map(|line_text| {
                        let sanitized = line_text
                            .replace("\r\n", "\n")
                            .replace('\r', "")
                            .trim_end_matches('\n')
                            .to_owned();
                        let (truncated, was_truncated) = truncate_line(&sanitized);
                        lines_truncated |= was_truncated;
                        format!("{}:{}: {}", relative, entry.line_number, truncated)
                    })
                });
                if let Some(line) = direct.flatten() {
                    if push_output_line(&mut output_lines, &mut output_bytes, line) {
                        break 'matches;
                    }
                    continue;
                }
                let file_lines =
                    match read_file_lines(&entry.file_path, &mut file_cache, operations.as_ref())
                        .await
                    {
                        Ok(lines) => lines,
                        Err(error) => {
                            let line = format!("{relative}:{}: ({error})", entry.line_number);
                            if push_output_line(&mut output_lines, &mut output_bytes, line) {
                                break 'matches;
                            }
                            continue;
                        }
                    };
                let start = entry.line_number.saturating_sub(context as u64).max(1);
                let end = entry
                    .line_number
                    .saturating_add(context as u64)
                    .min(file_lines.len() as u64);
                for current in start..=end {
                    let raw = file_lines[(current - 1) as usize].replace('\r', "");
                    let (truncated, was_truncated) = truncate_line(&raw);
                    lines_truncated |= was_truncated;
                    let line = if current == entry.line_number {
                        format!("{relative}:{current}: {truncated}")
                    } else {
                        format!("{relative}-{current}- {truncated}")
                    };
                    if push_output_line(&mut output_lines, &mut output_bytes, line) {
                        break 'matches;
                    }
                }
            }

            let raw_output = output_lines.join("\n");
            let truncation =
                truncate_head(&raw_output, GREP_TRUNCATION_MAX_LINES, DEFAULT_MAX_BYTES);
            // Build `details.truncation` (its `content` field repeats the
            // retained matches for upstream session parity) before moving the
            // retained text out of the truncation result — the old clone kept
            // a second full copy of the match list alive for the whole tail of
            // this function.
            let truncated = truncation.truncated;
            let truncation_details = truncated.then(|| truncation.to_details_json());
            let mut output = truncation.content;
            let mut details = serde_json::Map::new();
            let mut notices: Vec<String> = Vec::new();
            if match_limit_reached {
                notices.push(format!(
                    "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                    effective_limit.saturating_mul(2)
                ));
                details.insert(
                    "matchLimitReached".to_owned(),
                    serde_json::json!(effective_limit),
                );
            }
            if truncated {
                // grep's head truncation is byte-only upstream.
                notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
                details.insert(
                    "truncation".to_owned(),
                    truncation_details.unwrap_or(serde_json::Value::Null),
                );
            }
            if collected_byte_limit_hit {
                notices.push(format!(
                    "{} of match data collected. Narrow the pattern or path for complete results",
                    format_size(GREP_MAX_COLLECTED_MATCH_BYTES)
                ));
                details.insert(
                    "matchByteLimitReached".to_owned(),
                    serde_json::json!(GREP_MAX_COLLECTED_MATCH_BYTES),
                );
            }
            if lines_truncated {
                notices.push(format!(
                    "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
                ));
                details.insert("linesTruncated".to_owned(), serde_json::json!(true));
            }
            if !notices.is_empty() {
                output.push_str(&format!("\n\n[{}]", notices.join(". ")));
            }
            output.push_str(&clamp_suffix);
            let mut result = ToolResult::ok(output);
            if !details.is_empty() {
                result.details = Some(serde_json::Value::Object(details));
            }
            result
        })
    }
}

/// Read a file into lines (CRLF normalized), caching only the current file
/// (upstream `getFileLines`). A failed or oversized read yields a clear error
/// that is rendered in place of the context block.
///
/// The file is read as bytes and decoded with `String::from_utf8_lossy`, not
/// `read_to_string`: ripgrep matches files that are not valid UTF-8, so a
/// lossily-grepped file must still render its context instead of degrading to
/// "(unable to read file)" (upstream grep.ts reads bytes and decodes the same
/// way).
///
/// A one-entry cache is sufficient because ripgrep reports matches grouped by
/// file; it bounds retained line allocations to one source file instead of an
/// unbounded per-run map.
async fn read_file_lines(
    file_path: &str,
    cache: &mut Option<(String, Arc<Vec<String>>)>,
    operations: Option<&Arc<dyn GrepOperations>>,
) -> Result<Arc<Vec<String>>, String> {
    use tokio::io::AsyncReadExt;

    if let Some((cached_path, lines)) = cache.as_ref() {
        if cached_path == file_path {
            return Ok(Arc::clone(lines));
        }
    }
    if let Some(operations) = operations {
        let content = operations
            .read_file(Path::new(file_path))
            .await
            .map_err(|error| format!("cannot read context file '{file_path}': {error}"))?;
        let limit = max_source_bytes();
        if content.len() as u64 > limit {
            return Err(format!(
                "context unavailable for '{file_path}': {}",
                source_limit_error(content.len() as u64)
            ));
        }
        let lines = Arc::new(
            content
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .split('\n')
                .map(str::to_owned)
                .collect::<Vec<String>>(),
        );
        *cache = Some((file_path.to_owned(), Arc::clone(&lines)));
        return Ok(lines);
    }
    let file = tokio::fs::File::open(file_path)
        .await
        .map_err(|error| format!("cannot read context file '{file_path}': {error}"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("cannot read context file '{file_path}': {error}"))?;
    let limit = max_source_bytes();
    if metadata.len() > limit {
        return Err(format!(
            "context unavailable for '{file_path}': {}",
            source_limit_error(metadata.len())
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(limit) as usize);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("cannot read context file '{file_path}': {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "context unavailable for '{file_path}': {}",
            source_limit_error(bytes.len() as u64)
        ));
    }
    let content = String::from_utf8_lossy(&bytes);
    let lines = Arc::new(
        content
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<String>>(),
    );
    *cache = Some((file_path.to_owned(), Arc::clone(&lines)));
    Ok(lines)
}
