//! `ctx_execute` / `ctx_execute_file` / `ctx_batch_execute`: the "think in
//! code" execution tools.
//!
//! All run JavaScript on the plugin's shared (lazy) QuickJS sandbox, the same
//! engine and node compatibility suite `pir js` and JS extensions use. Only
//! captured stdout enters the tool result; an uncaught exception or a nonzero
//! exit is reported as an error. `ctx_execute_file` additionally confines the
//! path to the session cwd and exposes it as `CTX_FILE` / `FILE_PATH`, so the
//! raw file never enters context either. `ctx_batch_execute` runs 1-16
//! scripts sequentially, auto-indexes every output, and returns only
//! query-matched sections (or a follow-up hint) plus per-script counts.
//!
//! `intent`: when stdout exceeds 5 KiB the output is indexed and the tool
//! returns BM25-matched section titles + snippets instead of the dump, so a
//! large derivation stays recall-by-topic rather than recency-in-context.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use super::index::{
    IndexDoc, SearchHit, DEFAULT_SEARCH_LIMIT, MAX_FILE_BYTES, MAX_QUERY_BYTES, MAX_SEARCH_QUERIES,
};
use super::js::{JsJob, JsOutcome};
use super::{
    ContextModeState, AUTO_INDEX_RESULTS, AUTO_INDEX_THRESHOLD_BYTES, DEFAULT_TIMEOUT_SECS,
    MAX_TIMEOUT_SECS,
};
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::ai::types::AbortSignal;

/// Execution languages the sandbox supports. Upstream context-mode also
/// spawns `python`, `shell` and `powershell` runtimes; this port deliberately
/// keeps the one runtime that is already embedded (JavaScript/TypeScript) and
/// sends shell work to the `bash` tool.
const SUPPORTED_LANGUAGE_ERROR: &str = "context-mode: unsupported language; this build runs `javascript` (default) or `typescript`. Use the bash tool for shell commands.";

fn parse_language(args: &serde_json::Value) -> Result<String, String> {
    match args.get("language") {
        None => Ok("javascript".to_owned()),
        Some(serde_json::Value::String(language))
            if matches!(language.as_str(), "javascript" | "js" | "typescript" | "ts") =>
        {
            Ok(if language == "ts" || language == "typescript" {
                "typescript".to_owned()
            } else {
                "javascript".to_owned()
            })
        }
        Some(_) => Err(SUPPORTED_LANGUAGE_ERROR.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Shared execution path
// ---------------------------------------------------------------------------

/// One sandbox job whose only model-facing output is stdout.
struct ExecuteRequest {
    job: JsJob,
    timeout: Duration,
    /// Bytes the model supplied to the sandbox (code, plus the file path for
    /// `ctx_execute_file`), for `ctx_stats`.
    bytes_in: usize,
    /// Source label used when `intent` auto-indexes the output.
    source: String,
    intent: Option<String>,
    signal: Option<AbortSignal>,
    is_file: bool,
}

async fn execute(state: Arc<ContextModeState>, request: ExecuteRequest) -> ToolResult {
    let ExecuteRequest {
        job,
        timeout,
        bytes_in,
        source,
        intent,
        signal,
        is_file,
    } = request;

    let cwd = job.cwd.clone();
    let label = job
        .code
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect::<String>();
    let file_path = job
        .env
        .iter()
        .find(|(key, _)| key == "CTX_FILE")
        .map(|(_, value)| value.clone());
    state.bind_scope(&cwd);

    let result = state.js.run(job, timeout, signal).await;
    {
        let (ok, exit_code, bytes, body) = match &result {
            Ok(outcome) => (
                !outcome.is_error(),
                Some(outcome.exit_code as i64),
                outcome.stdout.len() + outcome.stderr.len(),
                if outcome.stdout.is_empty() {
                    first_line(&outcome.stderr)
                } else {
                    first_line(&outcome.stdout)
                },
            ),
            Err(error) => (false, None, 0, error.clone()),
        };
        let body = if label.is_empty() {
            body
        } else {
            format!("{label} -> {body}")
        };
        state
            .journal(
                "exec",
                if is_file {
                    "ctx_execute_file"
                } else {
                    "ctx_execute"
                },
                file_path,
                ok,
                exit_code,
                bytes,
                body,
            )
            .await;
    }
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            return finish(
                &state,
                ToolResult::error(format!("context-mode: {error}")),
                bytes_in,
                is_file,
            )
        }
    };

    if outcome.aborted {
        return finish(
            &state,
            ToolResult::error("context-mode: execution aborted"),
            bytes_in,
            is_file,
        );
    }
    if outcome.timed_out {
        return finish(
            &state,
            ToolResult::error(format!(
                "context-mode: execution timed out after {:.0}s",
                timeout.as_secs_f64()
            )),
            bytes_in,
            is_file,
        );
    }
    if outcome.exit_code != 0 {
        let mut content = outcome.stdout;
        if !content.is_empty() && !outcome.stderr.is_empty() {
            content.push('\n');
        }
        content.push_str(&outcome.stderr);
        if content.is_empty() {
            content = format!(
                "context-mode: script exited with code {}",
                outcome.exit_code
            );
        }
        return finish(&state, ToolResult::error(content), bytes_in, is_file);
    }

    // A clean exit with stderr diagnostics (console.error / process.stderr):
    // stdout is the result; stderr is appended only when there is no stdout,
    // so warnings do not displace the answer.
    let stdout = if outcome.stdout.is_empty() && !outcome.stderr.is_empty() {
        outcome.stderr
    } else {
        outcome.stdout
    };
    if stdout.is_empty() {
        return finish(&state, ToolResult::ok("(no output)"), bytes_in, is_file);
    }

    let intent = intent.filter(|intent| !intent.trim().is_empty());
    if let Some(intent) = intent {
        if stdout.len() > AUTO_INDEX_THRESHOLD_BYTES {
            let text = match auto_index_intent(&state, &stdout, &intent, &source).await {
                Ok(text) => text,
                // Indexing is best effort: a DB failure must not swallow the
                // derivation, so fall back to the capped stdout.
                Err(error) => {
                    format!("{stdout}\n\n[context-mode: could not auto-index the output: {error}]")
                }
            };
            return finish(&state, ToolResult::ok(text), bytes_in, is_file);
        }
    }

    finish(&state, ToolResult::ok(stdout), bytes_in, is_file)
}

/// First non-empty line, capped for the journal body.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(no output)")
        .chars()
        .take(160)
        .collect()
}

fn finish(
    state: &ContextModeState,
    result: ToolResult,
    bytes_in: usize,
    is_file: bool,
) -> ToolResult {
    let bytes_out = result.content.len();
    if is_file {
        state.stats.record_file_execution(bytes_in, bytes_out);
    } else {
        state.stats.record_execution(bytes_in, bytes_out);
    }
    result
}

/// Index a large stdout and return titles + snippets, not the dump.
async fn auto_index_intent(
    state: &Arc<ContextModeState>,
    stdout: &str,
    intent: &str,
    source: &str,
) -> anyhow::Result<String> {
    let doc = IndexDoc::from_text(source.to_owned(), "text".to_owned(), stdout);
    let report = state.index.index_documents(vec![doc]).await?;
    state.stats.record_auto_index();
    let search = state
        .index
        .search(
            vec![intent.to_owned()],
            Some(source.to_owned()),
            None,
            AUTO_INDEX_RESULTS,
        )
        .await?;

    let mut text = format!(
        "Indexed {} section(s) from \"{source}\" into the knowledge base.",
        report.chunks
    );
    match search.and_then(|response| response.queries.into_iter().next()) {
        Some(query) if !query.results.is_empty() => {
            text.push_str(&format!(
                "\n\nTop {} match(es) for intent \"{intent}\":",
                query.results.len()
            ));
            for hit in query.results {
                // Titles + ~500-char snippets only: the point is to expose
                // the shape of the output, not to paste it back.
                text.push_str(&format!(
                    "\n- {} [lines {}-{}, source: {}]\n  {}",
                    hit.title, hit.line_start, hit.line_end, hit.source, hit.snippet
                ));
            }
        }
        _ => {
            text.push_str(&format!(
                "\n\nNo sections matched intent \"{intent}\" in the indexed output."
            ));
        }
    }
    text.push_str(&format!(
        "\n\nUse ctx_search(queries: [...], source: \"{source}\") to retrieve specific sections."
    ));
    Ok(text)
}

fn parse_timeout(args: &serde_json::Value) -> Duration {
    // Upstream calls the deadline `timeoutMs`; accept both shapes, preferring
    // the upstream millisecond field when a caller supplies it.
    let seconds = args
        .get("timeoutMs")
        .and_then(serde_json::Value::as_f64)
        .filter(|millis| *millis > 0.0)
        .map(|millis| (millis / 1_000.0).min(MAX_TIMEOUT_SECS))
        .or_else(|| {
            args.get("timeout")
                .and_then(serde_json::Value::as_f64)
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| seconds.min(MAX_TIMEOUT_SECS))
        })
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs_f64(seconds)
}

fn parse_intent(args: &serde_json::Value) -> Option<String> {
    args.get("intent")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|intent| !intent.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// ctx_execute
// ---------------------------------------------------------------------------

pub struct CtxExecuteTool {
    state: Arc<ContextModeState>,
}

impl CtxExecuteTool {
    pub fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxExecuteTool {
    fn name(&self) -> &'static str {
        super::CTX_EXECUTE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Run JavaScript in the embedded sandbox (the same QuickJS + node compatibility runtime as `pir js`: fs, fetch, path, timers, require/import). Only stdout enters context — the data your code reads or derives stays inside the sandbox. \
Think in code: write analysis code, not data dumps; filter, count, aggregate, parse and compare inside the script and print only the answer. \
Each call runs as a fresh module; an uncaught exception or nonzero exit returns the error stack. \
stdout and stderr are each capped at 1 MiB; the default deadline is 30s, max 120s. \
Pass intent when the output may be large: stdout over 5 KiB is indexed into the local knowledge base and only BM25-matched section titles + snippets come back; retrieve more with ctx_search. \
Use this when you intend to derive an answer FROM data and the output shape is not known before execution. Not for file editing, navigation, or short observational commands whose entire output you will read anyway."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": ["javascript", "typescript"],
                    "description": "Source language. Default `javascript`. TypeScript is transpiled by the embedded swc suite; shell work belongs to the bash tool."
                },
                "code": {
                    "type": "string",
                    "description": "JavaScript/TypeScript source to execute. Use console.log (or process.stdout.write) for the findings that should enter context; everything else stays in the sandbox."
                },
                "file": {
                    "type": "string",
                    "description": "Optional confined file exposed as process.env.CTX_FILE / FILE_PATH (upstream shape; `ctx_execute_file` is the explicit alias). Must resolve inside the session cwd."
                },
                "timeout": {
                    "type": "number",
                    "description": "Max execution time in seconds. Default 30; values above 120 are clamped."
                },
                "timeoutMs": {
                    "type": "number",
                    "description": "Upstream-shaped millisecond deadline; wins over `timeout` when both are set (max 120000)."
                },
                "intent": {
                    "type": "string",
                    "description": "What you are looking for in the output. When set and stdout exceeds 5 KiB, the output is indexed and only matching section titles + snippets are returned; use specific technical terms."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        // The sandbox can read/write the filesystem and fetch URLs;
        // informational, like any execution tool.
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let language = parse_language(args);
        let code = args
            .get("code")
            .and_then(serde_json::Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .map(str::to_owned);
        // Upstream folds the file case into `ctx_execute({file})`; the
        // dedicated `ctx_execute_file` tool stays as the explicit alias.
        let file = args
            .get("file")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|file| !file.is_empty())
            .map(str::to_owned);
        let timeout = parse_timeout(args);
        let intent = parse_intent(args);
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let language = match language {
                Ok(language) => language,
                Err(message) => return ToolResult::error(message),
            };
            let Some(code) = code else {
                return ToolResult::error(
                    "ctx_execute: the `code` parameter is required and must be non-empty JavaScript source",
                );
            };
            if let Some(file) = file {
                let read = tokio::task::spawn_blocking({
                    let file = file.clone();
                    let cwd = cwd.clone();
                    move || resolve_confined_file(&cwd, &file)
                })
                .await;
                let resolved = match read {
                    Ok(Ok(resolved)) => resolved,
                    Ok(Err(message)) => return ToolResult::error(message),
                    Err(error) => {
                        return ToolResult::error(format!(
                            "ctx_execute: the file resolver failed: {error}"
                        ))
                    }
                };
                let resolved_text = resolved.to_string_lossy().into_owned();
                let bytes_in = code.len().saturating_add(resolved_text.len());
                return execute(
                    state,
                    ExecuteRequest {
                        job: JsJob {
                            code,
                            language,
                            cwd,
                            env: vec![
                                ("CTX_FILE".to_owned(), resolved_text.clone()),
                                ("FILE_PATH".to_owned(), resolved_text),
                            ],
                        },
                        timeout,
                        bytes_in,
                        source: format!("file:{file}"),
                        intent,
                        signal,
                        is_file: true,
                    },
                )
                .await;
            }
            let bytes_in = code.len();
            execute(
                state,
                ExecuteRequest {
                    job: JsJob {
                        code,
                        language,
                        cwd,
                        env: Vec::new(),
                    },
                    timeout,
                    bytes_in,
                    source: "execute:js".to_owned(),
                    intent,
                    signal,
                    is_file: false,
                },
            )
            .await
        })
    }
}

// ---------------------------------------------------------------------------
// ctx_execute_file
// ---------------------------------------------------------------------------

pub struct CtxExecuteFileTool {
    state: Arc<ContextModeState>,
}

impl CtxExecuteFileTool {
    pub fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxExecuteFileTool {
    fn name(&self) -> &'static str {
        super::CTX_EXECUTE_FILE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Run JavaScript over one confined file and print only the derivation. The absolute path is available as `process.env.CTX_FILE` and the global `FILE_PATH`; read it with `node:fs` inside the script. Only what your code prints enters context — the raw bytes never do. \
Think in code: write analysis code, not data dumps. The path is resolved against the session cwd and must stay inside it; `..` escapes, symlink escapes, non-files and files over 32 MiB are rejected. \
This is for analysis, not editing: use Read when you intend to edit the file or will consume all of it. \
Each call runs as a fresh module; an uncaught exception or nonzero exit returns the error stack. stdout/stderr are each capped at 1 MiB; default deadline 30s, max 120s. \
Use this to know something ABOUT a file (line counts, matches, parsed structure, aggregates) without reading all of it. \
Pass intent for a large derivation: stdout over 5 KiB is indexed and only matching section titles + snippets are returned; retrieve more with ctx_search."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File exposed as process.env.CTX_FILE / FILE_PATH: absolute, or relative to the session working directory. Must resolve inside that directory."
                },
                "language": {
                    "type": "string",
                    "enum": ["javascript", "typescript"],
                    "description": "Source language. Default `javascript`."
                },
                "code": {
                    "type": "string",
                    "description": "JavaScript/TypeScript source that reads process.env.CTX_FILE and prints the findings."
                },
                "timeout": {
                    "type": "number",
                    "description": "Max execution time in seconds. Default 30; values above 120 are clamped."
                },
                "timeoutMs": {
                    "type": "number",
                    "description": "Upstream-shaped millisecond deadline; wins over `timeout` when both are set (max 120000)."
                },
                "intent": {
                    "type": "string",
                    "description": "What you are looking for in the output. When set and stdout exceeds 5 KiB, only matching section titles + snippets are returned."
                }
            },
            "required": ["path", "code"],
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let language = parse_language(args);
        let path_arg = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(str::to_owned);
        let code = args
            .get("code")
            .and_then(serde_json::Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .map(str::to_owned);
        let timeout = parse_timeout(args);
        let intent = parse_intent(args);
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let language = match language {
                Ok(language) => language,
                Err(message) => return ToolResult::error(message),
            };
            let Some(path_arg) = path_arg else {
                return ToolResult::error("ctx_execute_file: the `path` parameter is required");
            };
            let Some(code) = code else {
                return ToolResult::error(
                    "ctx_execute_file: the `code` parameter is required and must be non-empty JavaScript source",
                );
            };

            // Path resolution + confinement run on the blocking pool
            // (canonicalize + filesystem metadata).
            let read = tokio::task::spawn_blocking({
                let path_arg = path_arg.clone();
                let cwd = cwd.clone();
                move || resolve_confined_file(&cwd, &path_arg)
            })
            .await;
            let resolved = match read {
                Ok(Ok(resolved)) => resolved,
                Ok(Err(message)) => return ToolResult::error(message),
                Err(error) => {
                    return ToolResult::error(format!(
                        "ctx_execute_file: the file resolver failed: {error}"
                    ))
                }
            };

            let resolved_text = resolved.to_string_lossy().into_owned();
            let bytes_in = code.len().saturating_add(resolved_text.len());
            let source = format!("file:{path_arg}");
            execute(
                state,
                ExecuteRequest {
                    job: JsJob {
                        code,
                        language,
                        cwd,
                        env: vec![
                            ("CTX_FILE".to_owned(), resolved_text.clone()),
                            ("FILE_PATH".to_owned(), resolved_text),
                        ],
                    },
                    timeout,
                    bytes_in,
                    source,
                    intent,
                    signal,
                    is_file: true,
                },
            )
            .await
        })
    }
}

/// Resolve `raw` against `cwd`, require the canonical result to stay under the
/// canonical cwd (no `..` escape, no symlink escape), require a regular file
/// and enforce [`MAX_FILE_BYTES`]. The script reads the bytes itself.
fn resolve_confined_file(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    // Reject `..` components outright instead of relying on the platform's
    // canonicalization rules for them: a confinement check must not hinge on
    // whether an extended-length Windows path normalizes `..` before the
    // containment comparison.
    if Path::new(raw)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "ctx_execute_file: `{raw}` contains a `..` component and escapes the session working directory"
        ));
    }
    let cwd = cwd.canonicalize().map_err(|error| {
        format!(
            "ctx_execute_file: cannot resolve the session working directory `{}`: {error}",
            cwd.display()
        )
    })?;
    let candidate = {
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };
    let resolved = candidate.canonicalize().map_err(|error| {
        format!("ctx_execute_file: cannot resolve `{raw}` (it must exist): {error}")
    })?;
    // Component-wise containment check: catches `..` and symlink escapes
    // after canonicalization, so the check runs on the real target.
    if !resolved.starts_with(&cwd) {
        return Err(format!(
            "ctx_execute_file: `{raw}` escapes the session working directory; only files under `{}` are allowed",
            cwd.display()
        ));
    }
    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| format!("ctx_execute_file: cannot stat `{raw}`: {error}"))?;
    if !metadata.is_file() {
        return Err(format!(
            "ctx_execute_file: `{raw}` is not a regular file (directories and special files are rejected)"
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "ctx_execute_file: `{raw}` is {} bytes; the limit is {} MiB",
            metadata.len(),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// ctx_batch_execute
// ---------------------------------------------------------------------------

/// Upper bound on the scripts in one batch (mirrors the schema's `maxItems`).
const MAX_BATCH_SCRIPTS: usize = 16;

/// Characters of a failing script's stderr kept in `errorPreview`.
const BATCH_ERROR_PREVIEW_CHARS: usize = 200;

/// One script parsed out of the `scripts` array.
struct BatchScript {
    label: String,
    code: String,
}

/// Validated `ctx_batch_execute` arguments.
struct BatchRequest {
    scripts: Vec<BatchScript>,
    queries: Vec<String>,
    source: Option<String>,
    timeout: Duration,
    language: String,
}

/// Per-script result summary: counts and a failure preview only — the stdout
/// itself stays in the knowledge base.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchScriptSummary {
    label: String,
    indexed_chunks: usize,
    bytes: usize,
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_preview: Option<String>,
}

/// One query's matched sections (the same shape `ctx_search` returns; no raw
/// chunk content).
#[derive(Serialize)]
struct BatchQuerySummary {
    query: String,
    sections: Vec<SearchHit>,
}

fn parse_batch_request(args: &serde_json::Value) -> Result<BatchRequest, String> {
    let raw_scripts = match args.get("scripts") {
        Some(serde_json::Value::Array(scripts)) => scripts,
        Some(_) => return Err(
            "ctx_batch_execute: the `scripts` parameter must be an array of {label?, code} objects"
                .to_owned(),
        ),
        None => return Err("ctx_batch_execute: the `scripts` parameter is required".to_owned()),
    };
    if raw_scripts.is_empty() {
        return Err(format!(
            "ctx_batch_execute: the `scripts` parameter must contain at least 1 script (up to {MAX_BATCH_SCRIPTS})"
        ));
    }
    if raw_scripts.len() > MAX_BATCH_SCRIPTS {
        return Err(format!(
            "ctx_batch_execute: at most {MAX_BATCH_SCRIPTS} scripts are allowed per call (got {})",
            raw_scripts.len()
        ));
    }

    let mut scripts = Vec::with_capacity(raw_scripts.len());
    for (index, raw) in raw_scripts.iter().enumerate() {
        let Some(object) = raw.as_object() else {
            return Err(format!(
                "ctx_batch_execute: script #{} must be an object with a `code` string",
                index + 1
            ));
        };
        let Some(code) = object.get("code").and_then(serde_json::Value::as_str) else {
            return Err(format!(
                "ctx_batch_execute: script #{} requires a `code` string",
                index + 1
            ));
        };
        if code.trim().is_empty() {
            return Err(format!(
                "ctx_batch_execute: script #{} has empty `code`; every script needs non-empty JavaScript source",
                index + 1
            ));
        }
        let label = object
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("script-{}", index + 1));
        scripts.push(BatchScript {
            label,
            code: code.to_owned(),
        });
    }

    // Same bounds and shape as `ctx_search`: trimmed non-empty strings, at
    // most 32 queries, each at most 512 bytes.
    let queries: Vec<String> = args
        .get("queries")
        .and_then(serde_json::Value::as_array)
        .map(|queries| {
            queries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if queries.len() > MAX_SEARCH_QUERIES {
        return Err(format!(
            "ctx_batch_execute: at most {MAX_SEARCH_QUERIES} queries are allowed per call (got {})",
            queries.len()
        ));
    }
    if let Some((index, query)) = queries
        .iter()
        .enumerate()
        .find(|(_, query)| query.len() > MAX_QUERY_BYTES)
    {
        return Err(format!(
            "ctx_batch_execute: query #{} is {} bytes; each query is limited to {MAX_QUERY_BYTES} bytes",
            index + 1,
            query.len()
        ));
    }

    let source = args
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_owned);

    Ok(BatchRequest {
        scripts,
        queries,
        source,
        timeout: parse_timeout(args),
        language: parse_language(args)?,
    })
}

/// Stable default source label: `batch:<fnv1a(cwd + NUL + first script)>`.
/// FNV-1a is version-independent (the same reason the DB path hash uses it),
/// so re-running the same batch writes to the same source instead of piling
/// up new labels.
fn default_batch_source(cwd: &str, first_code: &str) -> String {
    let cwd = if cfg!(windows) {
        cwd.to_ascii_lowercase()
    } else {
        cwd.to_owned()
    };
    let mut key = String::with_capacity(cwd.len() + first_code.len() + 1);
    key.push_str(&cwd);
    key.push('\0');
    key.push_str(first_code);
    format!("batch:{}", fnv1a_hex(key.as_bytes()))
}

fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One-line failure preview: the first informative line of the error stack
/// (the JS engine's message line), truncated to
/// [`BATCH_ERROR_PREVIEW_CHARS`] characters.
fn error_preview(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(match line.char_indices().nth(BATCH_ERROR_PREVIEW_CHARS) {
        Some((byte_index, _)) => {
            let mut preview = line[..byte_index].to_owned();
            preview.push('…');
            preview
        }
        None => line.to_owned(),
    })
}

/// The exact follow-up calls for the sources this batch indexed.
fn search_hint(sources: &[String], fallback: &str) -> String {
    if sources.is_empty() {
        return format!("ctx_search(source: {fallback:?})");
    }
    sources
        .iter()
        .map(|source| format!("ctx_search(source: {source:?})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wait for the host to cancel the turn (never resolves without a signal).
async fn abort_wait(signal: &Option<AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Run the scripts sequentially on the one lazy JS sandbox, auto-index every
/// non-empty stdout under the script's source label, then run all queries over
/// just those sources.
async fn execute_batch(
    state: Arc<ContextModeState>,
    request: BatchRequest,
    cwd: PathBuf,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let BatchRequest {
        scripts,
        queries,
        source,
        timeout,
        language,
    } = request;
    let cwd_text = cwd.to_string_lossy().into_owned();
    state.bind_scope(&cwd);
    let base_source = source.unwrap_or_else(|| default_batch_source(&cwd_text, &scripts[0].code));
    let multiple = scripts.len() > 1;

    let mut summaries = Vec::with_capacity(scripts.len());
    let mut indexed_sources: Vec<String> = Vec::new();

    for script in scripts {
        if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return ToolResult::error("ctx_batch_execute: execution aborted");
        }

        // A single-script batch keeps the bare base source; a multi-script
        // batch scopes each output under `<source>#<label>` so an exact
        // source filter in ctx_search can address one script.
        let script_source = if multiple {
            format!("{base_source}#{}", script.label)
        } else {
            base_source.clone()
        };

        let job = JsJob {
            // Move the source into the job: the summary never reads it again,
            // so cloning it only duplicated every script's bytes.
            code: script.code,
            language: language.clone(),
            cwd: cwd.clone(),
            env: Vec::new(),
        };
        let code_len = job.code.len();
        let outcome = tokio::select! {
            outcome = state.js.run(job, timeout, signal.clone()) => outcome,
            _ = abort_wait(&signal) => {
                return ToolResult::error("ctx_batch_execute: execution aborted");
            }
        };
        let outcome: JsOutcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                return ToolResult::error(format!(
                    "ctx_batch_execute: script \"{}\": {error}",
                    script.label
                ))
            }
        };
        let is_error = outcome.is_error();

        state.stats.record_execution(code_len, outcome.stdout.len());

        // One index transaction per script. Indexing is what makes the output
        // reachable; a DB failure is a batch-level error (echoing the raw
        // output instead would defeat the context-saving contract).
        let mut indexed_chunks = 0usize;
        if !outcome.stdout.is_empty() {
            let doc =
                IndexDoc::from_text(script_source.clone(), "prose".to_owned(), &outcome.stdout);
            if !doc.chunks.is_empty() {
                match state.index.index_documents(vec![doc]).await {
                    Ok(report) => {
                        indexed_chunks = report.chunks;
                        state.stats.record_auto_index();
                    }
                    Err(error) => {
                        return ToolResult::error(format!(
                            "ctx_batch_execute: could not index script \"{}\": {error}",
                            script.label
                        ))
                    }
                }
            }
        }
        if indexed_chunks > 0 {
            indexed_sources.push(script_source);
        }

        summaries.push(BatchScriptSummary {
            label: script.label,
            indexed_chunks,
            bytes: outcome.stdout.len(),
            is_error,
            // Failing scripts keep running; their error stack is summarized in
            // one line instead of being pasted back.
            error_preview: if is_error {
                error_preview(&outcome.stderr)
            } else {
                None
            },
        });
        state
            .journal(
                "batch",
                super::CTX_BATCH_EXECUTE_TOOL_NAME,
                None,
                !is_error,
                Some(outcome.exit_code as i64),
                outcome.stdout.len(),
                format!(
                    "{} -> {} chunk(s), {} B",
                    summaries
                        .last()
                        .map(|summary| summary.label.as_str())
                        .unwrap_or("script"),
                    indexed_chunks,
                    outcome.stdout.len()
                ),
            )
            .await;
    }

    let query_summaries = if queries.is_empty() {
        Vec::new()
    } else {
        match search_batch(&state, &queries, &indexed_sources).await {
            Ok(results) => results,
            Err(error) => return ToolResult::error(format!("ctx_batch_execute: {error}")),
        }
    };

    let mut value = serde_json::json!({
        "type": "batch",
        "source": base_source,
        "scripts": summaries,
        "queries": query_summaries,
    });
    if queries.is_empty() {
        // No inline query: point at the exact follow-up call instead of
        // dumping any output.
        value["hint"] = serde_json::Value::String(search_hint(&indexed_sources, &base_source));
    }

    ToolResult {
        content: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned()),
        images: Vec::new(),
        details: None,
        structured_content: Some(value),
        is_error: false,
        terminate: false,
    }
}

/// Search every indexed batch source with all queries in one pass per source,
/// then merge to the global top [`DEFAULT_SEARCH_LIMIT`] per query. Chunk ids
/// are unique per source; the dedupe is defensive (and cheap).
async fn search_batch(
    state: &Arc<ContextModeState>,
    queries: &[String],
    sources: &[String],
) -> anyhow::Result<Vec<BatchQuerySummary>> {
    if sources.is_empty() {
        return Ok(queries
            .iter()
            .map(|query| BatchQuerySummary {
                query: query.clone(),
                sections: Vec::new(),
            })
            .collect());
    }

    let mut merged: Vec<Vec<SearchHit>> = vec![Vec::new(); queries.len()];
    for source in sources {
        let response = state
            .index
            .search(
                queries.to_vec(),
                Some(source.clone()),
                None,
                DEFAULT_SEARCH_LIMIT,
            )
            .await?;
        // `None` only when the whole knowledge base is empty; nothing can
        // match then, so later sources cannot help either.
        let Some(response) = response else {
            break;
        };
        for (index, query_result) in response.queries.into_iter().enumerate() {
            merged[index].extend(query_result.results);
        }
    }

    Ok(queries
        .iter()
        .zip(merged)
        .map(|(query, mut hits)| {
            hits.sort_by(|left, right| {
                right
                    .score
                    .total_cmp(&left.score)
                    .then_with(|| left.id.cmp(&right.id))
            });
            let mut seen = HashSet::new();
            hits.retain(|hit| seen.insert(hit.id));
            hits.truncate(DEFAULT_SEARCH_LIMIT);
            BatchQuerySummary {
                query: query.clone(),
                sections: hits,
            }
        })
        .collect())
}

pub struct CtxBatchExecuteTool {
    state: Arc<ContextModeState>,
}

impl CtxBatchExecuteTool {
    pub fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxBatchExecuteTool {
    fn name(&self) -> &'static str {
        super::CTX_BATCH_EXECUTE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Run MULTIPLE JavaScript analyses in one call. Every script runs in the embedded JS sandbox on the shared runtime; each script's stdout is auto-indexed into the local FTS5 knowledge base, one transaction per script. Optional queries run over just this batch's indexed outputs and return their top matching sections in the same round trip, so no follow-up search call is needed. \
Batch related analyses in one call instead of running ctx_execute repeatedly: multi-file scans, several parses, or gather-and-query in one round trip. Only matched sections (title/source/snippet/line hints) and a one-line preview of failing scripts enter context; raw output stays in the sandbox and the database and is never echoed in full. \
Scripts run sequentially, 1-16 per call; a failing script (uncaught exception, nonzero exit or per-script timeout) does not abort the batch — its error stack is summarized as errorPreview and the next script still runs. stdout/stderr are each capped at 1 MiB, the default per-script deadline is 30s (max 120s). `queries` is optional: at most 32, each up to 512 bytes. Each script indexes under `<source>#<label>` (or `<source>` for a single-script batch), so `ctx_search(source: ...)` can address one script; `source` defaults to a stable `batch:<hash>` label. \
Use this when you have several analyses that would otherwise be sequential tool calls, or when you want to gather and query in one round trip. Use ctx_execute for a single script; do not use this for short observational commands whose entire output you will read anyway."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "scripts": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 16,
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "description": "Short label for this script's output (used as the source suffix). Defaults to script-1, script-2, ..."
                            },
                            "code": {
                                "type": "string",
                                "description": "JavaScript/TypeScript source for this analysis. Use console.log for the findings that matter."
                            }
                        },
                        "required": ["code"],
                        "additionalProperties": false
                    },
                    "description": "1-16 independent JavaScript analyses, run sequentially. Every non-empty stdout is auto-indexed; raw output is never echoed."
                },
                "language": {
                    "type": "string",
                    "enum": ["javascript", "typescript"],
                    "description": "Source language for every script. Default `javascript`."
                },
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 32,
                    "description": "Optional search queries over just this batch's indexed outputs; each returns the top matching sections inline. Use specific technical terms, 2-4 per query."
                },
                "source": {
                    "type": "string",
                    "description": "Base label for the indexed outputs. Each script becomes `<source>#<label>` (or `<source>` for one script). Defaults to a stable batch:<hash> label."
                },
                "timeout": {
                    "type": "number",
                    "description": "Per-script max execution time in seconds. Default 30; values above 120 are clamped."
                },
                "timeoutMs": {
                    "type": "number",
                    "description": "Upstream-shaped millisecond deadline; wins over `timeout` when both are set (max 120000)."
                }
            },
            "required": ["scripts"],
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        // Scripts can read/write the filesystem and the batch writes the
        // SQLite knowledge base; never treated as read-only.
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let request = parse_batch_request(args);
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let request = match request {
                Ok(request) => request,
                Err(message) => return ToolResult::error(message),
            };
            execute_batch(state, request, cwd, signal).await
        })
    }
}
