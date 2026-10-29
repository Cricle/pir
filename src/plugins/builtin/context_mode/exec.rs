//! `ctx_execute` / `ctx_execute_file` / `ctx_batch_execute`: the "think in
//! code" execution tools.
//!
//! All run Python on the plugin's shared (lazy) RustPython worker, each call
//! in a fresh namespace. Only captured stdout enters the tool result; on an
//! uncaught exception the traceback is appended and the result is an error.
//! `ctx_execute_file` additionally confines the path to the session cwd and
//! binds the UTF-8-lossy bytes to `FILE_CONTENT` / `FILE_PATH`, so the raw
//! file never enters context either. `ctx_batch_execute` runs 1-16 scripts
//! sequentially, auto-indexes every output, and returns only query-matched
//! sections (or a follow-up hint) plus per-script counts.
//!
//! `intent`: when stdout exceeds 5 KiB the output is indexed and the tool
//! returns BM25-matched section titles + snippets instead of the dump, so a
//! large derivation stays recall-by-topic rather than recency-in-context.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use super::index::{
    IndexDoc, SearchHit, DEFAULT_SEARCH_LIMIT, MAX_FILE_BYTES, MAX_QUERY_BYTES, MAX_SEARCH_QUERIES,
};
use super::{
    ContextModeState, AUTO_INDEX_RESULTS, AUTO_INDEX_THRESHOLD_BYTES, DEFAULT_TIMEOUT_SECS,
    MAX_TIMEOUT_SECS,
};
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::ai::types::AbortSignal;
use crate::plugins::builtin::python_vm::{JobGlobal, PythonJob, PythonSession};

// ---------------------------------------------------------------------------
// Shared execution path
// ---------------------------------------------------------------------------

/// A fresh-namespace Python job whose only model-facing output is stdout.
struct ExecuteRequest {
    job: PythonJob,
    timeout: Duration,
    /// Bytes the model supplied to the sandbox (code, plus file bytes for
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

    let session: &PythonSession = &state.python;
    let exec = session.exec(job, timeout);
    let aborted = async {
        match &signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let outcome = tokio::select! {
        outcome = exec => outcome,
        _ = aborted => {
            // The script is already on the worker; dropping the future alone
            // would leave it running, so interrupt and reset before reporting.
            session.abort_worker();
            return finish(
                &state,
                ToolResult::error("context-mode: execution aborted"),
                bytes_in,
                is_file,
            );
        }
    };

    if outcome.error {
        let mut content = outcome.stdout;
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&outcome.stderr);
        return finish(&state, ToolResult::error(content), bytes_in, is_file);
    }

    if outcome.stdout.is_empty() {
        return finish(&state, ToolResult::ok("(no output)"), bytes_in, is_file);
    }

    let intent = intent.filter(|intent| !intent.trim().is_empty());
    if let Some(intent) = intent {
        if outcome.stdout.len() > AUTO_INDEX_THRESHOLD_BYTES {
            let text = match auto_index_intent(&state, &outcome.stdout, &intent, &source).await {
                Ok(text) => text,
                // Indexing is best effort: a DB failure must not swallow the
                // derivation, so fall back to the capped stdout.
                Err(error) => format!(
                    "{}\n\n[context-mode: could not auto-index the output: {error}]",
                    outcome.stdout
                ),
            };
            return finish(&state, ToolResult::ok(text), bytes_in, is_file);
        }
    }

    finish(&state, ToolResult::ok(outcome.stdout), bytes_in, is_file)
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
    let seconds = args
        .get("timeout")
        .and_then(serde_json::Value::as_f64)
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| seconds.min(MAX_TIMEOUT_SECS))
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

pub(crate) struct CtxExecuteTool {
    state: Arc<ContextModeState>,
}

impl CtxExecuteTool {
    pub(crate) fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxExecuteTool {
    fn name(&self) -> &'static str {
        super::CTX_EXECUTE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Run Python in the embedded sandbox. Only stdout enters context — the data your code reads or derives stays inside the sandbox. \
Think in code: write analysis code, not data dumps; filter, count, aggregate, parse and compare inside the script and print only the answer. \
Every call runs in a fresh namespace (imports stay cached); an uncaught exception returns the Python traceback as an error. \
stdout and stderr are each capped at 1 MiB; the default deadline is 30s, max 120s. \
Pass intent when the output may be large: stdout over 5 KiB is indexed into the local knowledge base and only BM25-matched section titles + snippets come back; retrieve more with ctx_search. \
Use this when you intend to derive an answer FROM data and the output shape is not known before execution. Not for file editing, navigation, or short observational commands whose entire output you will read anyway."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute. Use print() for the findings that should enter context; everything else stays in the sandbox. Each call gets a fresh namespace."
                },
                "timeout": {
                    "type": "number",
                    "description": "Max execution time in seconds. Default 30; values above 120 are clamped."
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
        // The embedded stdlib can read/write the filesystem and spawn nothing
        // else; informational, like the `python` tool.
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let code = args
            .get("code")
            .and_then(serde_json::Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .map(str::to_owned);
        let timeout = parse_timeout(args);
        let intent = parse_intent(args);
        let cwd = ctx.cwd.to_string_lossy().into_owned();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let Some(code) = code else {
                return ToolResult::error(
                    "ctx_execute: the `code` parameter is required and must be non-empty Python source",
                );
            };
            let bytes_in = code.len();
            execute(
                state,
                ExecuteRequest {
                    job: PythonJob {
                        code,
                        cwd: Some(cwd),
                        // Exec-only: no REPL repr echo — only explicit stdout
                        // is model-facing output.
                        eval_first: false,
                        globals: Vec::new(),
                    },
                    timeout,
                    bytes_in,
                    source: "execute:python".to_owned(),
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

pub(crate) struct CtxExecuteFileTool {
    state: Arc<ContextModeState>,
}

impl CtxExecuteFileTool {
    pub(crate) fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxExecuteFileTool {
    fn name(&self) -> &'static str {
        super::CTX_EXECUTE_FILE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Read one file into the Python variable FILE_CONTENT (and its absolute path as FILE_PATH), then run code over it inside the embedded sandbox. Only what your code prints enters context — the raw bytes never do. \
Think in code: write analysis code, not data dumps. The path is resolved against the session cwd and must stay inside it; `..` escapes, symlink escapes, non-files and files over 32 MiB are rejected. \
This is for analysis, not editing: use Read when you intend to edit the file or will consume all of it. \
Every call runs in a fresh namespace; an uncaught exception returns the Python traceback as an error. stdout/stderr are each capped at 1 MiB; default deadline 30s, max 120s. \
Use this to know something ABOUT a file (line counts, matches, parsed structure, aggregates) without reading all of it. \
Pass intent for a large derivation: stdout over 5 KiB is indexed and only matching section titles + snippets are returned; retrieve more with ctx_search."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to load into FILE_CONTENT: absolute, or relative to the session working directory. Must resolve inside that directory."
                },
                "code": {
                    "type": "string",
                    "description": "Python source that processes FILE_CONTENT and prints the findings. Each call gets a fresh namespace."
                },
                "timeout": {
                    "type": "number",
                    "description": "Max execution time in seconds. Default 30; values above 120 are clamped."
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
        let session_cwd = ctx.cwd.to_string_lossy().into_owned();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let Some(path_arg) = path_arg else {
                return ToolResult::error("ctx_execute_file: the `path` parameter is required");
            };
            let Some(code) = code else {
                return ToolResult::error(
                    "ctx_execute_file: the `code` parameter is required and must be non-empty Python source",
                );
            };

            // Path resolution, confinement and the bounded read all run on
            // the blocking pool (canonicalize + filesystem I/O).
            let read = tokio::task::spawn_blocking({
                let path_arg = path_arg.clone();
                move || read_confined_file(&cwd, &path_arg)
            })
            .await;
            let (resolved, content) = match read {
                Ok(Ok(read)) => read,
                Ok(Err(message)) => return ToolResult::error(message),
                Err(error) => {
                    return ToolResult::error(format!(
                        "ctx_execute_file: the file reader failed: {error}"
                    ))
                }
            };

            let bytes_in = code.len().saturating_add(content.len());
            let file_path: Arc<str> = Arc::from(resolved.to_string_lossy().into_owned());
            let file_content: Arc<str> = Arc::from(content);
            let source = format!("file:{path_arg}");
            execute(
                state,
                ExecuteRequest {
                    job: PythonJob {
                        code,
                        cwd: Some(session_cwd),
                        eval_first: false,
                        globals: vec![
                            ("FILE_PATH".to_owned(), JobGlobal::Text(file_path)),
                            ("FILE_CONTENT".to_owned(), JobGlobal::Text(file_content)),
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

/// Resolve `raw` against `cwd`, require the canonical result to stay under
/// the canonical cwd (no `..` escape, no symlink escape), require a regular
/// file, and read at most [`MAX_FILE_BYTES`] as UTF-8-lossy text.
fn read_confined_file(cwd: &Path, raw: &str) -> Result<(PathBuf, String), String> {
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
    let file = std::fs::File::open(&resolved)
        .map_err(|error| format!("ctx_execute_file: cannot open `{raw}`: {error}"))?;
    // `take` re-checks the cap on the bytes actually read (the file may have
    // grown between `metadata` and the read); no size-based preallocation.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("ctx_execute_file: cannot read `{raw}`: {error}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "ctx_execute_file: `{raw}` grew past the {} MiB limit while it was read",
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    Ok((resolved, String::from_utf8_lossy(&bytes).into_owned()))
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
                "ctx_batch_execute: script #{} has empty `code`; every script needs non-empty Python source",
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

/// One-line failure preview. A Python traceback's first line is the generic
/// `Traceback (most recent call last):` header, so that case uses the final
/// exception line (the only informative one-line summary).
fn error_preview(stderr: &str) -> Option<String> {
    let mut first: Option<&str> = None;
    let mut last: Option<&str> = None;
    for line in stderr.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if first.is_none() {
            first = Some(line);
        }
        last = Some(line);
    }
    let first = first?;
    let line = if first == "Traceback (most recent call last):" {
        last.unwrap_or(first)
    } else {
        first
    };
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

/// Run the scripts sequentially on the one lazy interpreter (fresh namespace
/// each, as `ctx_execute`), auto-index every non-empty stdout under the
/// script's source label, then run all queries over just those sources.
async fn execute_batch(
    state: Arc<ContextModeState>,
    request: BatchRequest,
    cwd: String,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let BatchRequest {
        scripts,
        queries,
        source,
        timeout,
    } = request;
    let base_source = source.unwrap_or_else(|| default_batch_source(&cwd, &scripts[0].code));
    let multiple = scripts.len() > 1;
    let session = &state.python;

    let mut summaries = Vec::with_capacity(scripts.len());
    let mut indexed_sources: Vec<String> = Vec::new();

    for script in scripts {
        if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            session.abort_worker();
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

        let job = PythonJob {
            code: script.code.clone(),
            cwd: Some(cwd.clone()),
            eval_first: false,
            globals: Vec::new(),
        };
        let outcome = tokio::select! {
            outcome = session.exec(job, timeout) => outcome,
            _ = abort_wait(&signal) => {
                session.abort_worker();
                return ToolResult::error("ctx_batch_execute: execution aborted");
            }
        };

        state
            .stats
            .record_execution(script.code.len(), outcome.stdout.len());

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
            is_error: outcome.error,
            // Failing scripts keep running; their traceback is summarized in
            // one line instead of being pasted back.
            error_preview: if outcome.error {
                error_preview(&outcome.stderr)
            } else {
                None
            },
        });
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

pub(crate) struct CtxBatchExecuteTool {
    state: Arc<ContextModeState>,
}

impl CtxBatchExecuteTool {
    pub(crate) fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxBatchExecuteTool {
    fn name(&self) -> &'static str {
        super::CTX_BATCH_EXECUTE_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Run MULTIPLE Python analyses in one call. Every script runs in the embedded sandbox on the shared interpreter, each in a fresh namespace; each script's stdout is auto-indexed into the local FTS5 knowledge base, one transaction per script. Optional queries run over just this batch's indexed outputs and return their top matching sections in the same round trip, so no follow-up search call is needed. \
Batch related analyses in one call instead of running ctx_execute repeatedly: multi-file scans, several greps/parses, or gather-and-query in one round trip. Only matched sections (title/source/snippet/line hints) and a one-line preview of failing scripts enter context; raw output stays in the sandbox and the database and is never echoed in full. \
Scripts run sequentially, 1-16 per call; a failing script (uncaught exception or per-script timeout) does not abort the batch — its traceback is summarized as errorPreview and the next script still runs. stdout/stderr are each capped at 1 MiB, the default per-script deadline is 30s (max 120s). `queries` is optional: at most 32, each up to 512 bytes. Each script indexes under `<source>#<label>` (or `<source>` for a single-script batch), so `ctx_search(source: ...)` can address one script; `source` defaults to a stable `batch:<hash>` label. \
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
                                "description": "Python source for this analysis. Each script gets a fresh namespace; use print() for the findings that matter."
                            }
                        },
                        "required": ["code"],
                        "additionalProperties": false
                    },
                    "description": "1-16 independent Python analyses, run sequentially. Every non-empty stdout is auto-indexed; raw output is never echoed."
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
        let cwd = ctx.cwd.to_string_lossy().into_owned();
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
