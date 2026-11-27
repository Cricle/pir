//! `ctx_batch`: upstream context-mode's shell-command batch.
//!
//! Runs 1-16 shell commands one at a time through the same shell the bash tool
//! resolves (`resolve_shell_config`), indexes every command block into the
//! project knowledge base under one source label, and returns the matching
//! sections for the caller's queries — the raw blocks never enter context.
//!
//! A failing command does not abort the batch: its block is indexed like any
//! other and the reply keeps going, matching upstream's `runPool`/stop rules
//! for a sequential pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use super::index::{IndexDoc, ScopedSearch, SearchScope, SearchSort};
use super::{ContextModeState, MAX_OUTPUT_CHARS};
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::ai::types::AbortSignal;
// The pure request validation, the stable source label and the reply cap live
// in the engine-free `pir-builtin-context-mode` crate. The two schema bounds are
// re-exported from it too, so the declared `maxItems` cannot drift from the
// validator.
use pir_builtin_context_mode::{
    batch_source, cap_reply, BatchRequest, HITS_PER_QUERY, MAX_COMMANDS, MAX_SEARCH_QUERIES,
};

pub struct CtxBatchTool {
    state: Arc<ContextModeState>,
}

impl CtxBatchTool {
    pub fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxBatchTool {
    fn name(&self) -> &'static str {
        super::CTX_BATCH_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Run MULTIPLE shell commands in one call. Each command runs sequentially in the session working directory; its full output is indexed into the local FTS5 knowledge base, and only the sections matching your `queries` come back. Raw command output never enters context. \
Use this for several inspections that would otherwise be separate bash calls: git log/stat, install logs, test output, greps across trees. Each block is headed `$ <command>` in the index, so a query can match the command text too. \
A failing command does not stop the batch; its block is indexed and the next command still runs. The whole call shares one deadline (default 30s, max 120s), split across commands. \
`queries` is required: 1-32 entries, each up to 512 bytes. If nothing matches, the reply says so and points at ctx_search with the batch source. \
Do not use this for short commands whose output you will read whole anyway."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Run several shell commands in one call and get back only the sections matching \
your queries; use instead of one bash call per inspection when output is large"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "commands": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_COMMANDS,
                    "items": { "type": "string" },
                    "description": "Shell commands to run sequentially, in order."
                },
                "queries": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_SEARCH_QUERIES,
                    "items": { "type": "string" },
                    "description": "Questions to answer from the indexed output; each returns its top matching sections."
                },
                "timeout": {
                    "type": "number",
                    "description": "Whole-call deadline in seconds. Default 30; values above 120 are clamped."
                },
                "timeoutMs": {
                    "type": "number",
                    "description": "Upstream-shaped millisecond deadline; wins over `timeout` when both are set (max 120000)."
                }
            },
            "required": ["commands", "queries"],
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        true
    }

    fn run(&self, args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        // Argument extraction and every bound live in the engine-free
        // `pir-builtin-context-mode` crate, so the tool and the pure tests can
        // never disagree about what a valid request is.
        let request = match BatchRequest::parse(args) {
            Ok(request) => request,
            Err(message) => return Box::pin(async move { ToolResult::error(message) }),
        };
        let BatchRequest {
            commands,
            queries,
            timeout,
        } = request;
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let shell = match crate::coding_agent::tools::resolve_shell_config(None) {
                Ok(shell) => shell,
                Err(message) => {
                    return ToolResult::error(format!("ctx_batch: no usable shell: {message}"))
                }
            };
            state.bind_scope(&cwd);

            let deadline = Instant::now() + timeout;
            let mut blocks: Vec<String> = Vec::with_capacity(commands.len());
            let mut exits: Vec<Option<i32>> = Vec::with_capacity(commands.len());
            for command in &commands {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    blocks.push(format!("$ {command}\ncontext-mode: timed out"));
                    exits.push(None);
                    continue;
                }
                let outcome = run_command(&shell, command, &cwd, remaining, signal.as_ref()).await;
                match outcome {
                    Ok((stdout, stderr, code)) => {
                        let mut block = format!("$ {command}\n{stdout}");
                        if code.unwrap_or(0) != 0 && !stderr.is_empty() {
                            block.push_str(&stderr);
                        }
                        blocks.push(block);
                        exits.push(code);
                    }
                    Err(error) => {
                        blocks.push(format!("$ {command}\n{error}"));
                        exits.push(None);
                    }
                }
            }

            let combined = blocks.join("\n\n");
            let source = batch_source(&cwd.to_string_lossy(), &commands);
            let bytes = combined.len();
            let ok = exits.iter().all(|code| code.unwrap_or(0) == 0);
            let doc = IndexDoc::from_text(source.clone(), "text".to_owned(), &combined);
            if let Err(error) = state.index.index_documents(vec![doc]).await {
                return ToolResult::error(format!(
                    "ctx_batch: could not index the output: {error}"
                ));
            }
            state
                .journal(
                    "batch",
                    super::CTX_BATCH_TOOL_NAME,
                    None,
                    ok,
                    exits.first().copied().flatten().map(i64::from),
                    bytes,
                    format!(
                        "{} command(s) -> {} B indexed as {source}",
                        commands.len(),
                        bytes
                    ),
                )
                .await;

            let worktree = cwd
                .canonicalize()
                .unwrap_or_else(|_| cwd.clone())
                .to_string_lossy()
                .into_owned();
            let search = state
                .index
                .search_scoped(ScopedSearch {
                    queries: queries.clone(),
                    source: Some(source.clone()),
                    content_type: None,
                    limit: HITS_PER_QUERY,
                    offset: 0,
                    scope: SearchScope::Project,
                    sort: SearchSort::Relevance,
                    after: None,
                    session_id: state.hooks.session_id(),
                    worktree: Some(worktree),
                })
                .await;
            let response =
                match search {
                    Ok(Some(response)) => response,
                    Ok(None) => return ToolResult::error(
                        "ctx_batch: the knowledge base is empty after indexing (internal error)",
                    ),
                    Err(error) => return ToolResult::error(format!("ctx_batch: {error}")),
                };

            let mut text = format!(
                "{} command(s) · {} B indexed as {source}\n",
                commands.len(),
                bytes
            );
            let mut shown = 0usize;
            for query in &response.queries {
                text.push_str(&format!("## {}\n", query.query));
                if query.results.is_empty() {
                    text.push_str("(no matching sections)\n");
                    continue;
                }
                for hit in &query.results {
                    text.push_str(&format!(
                        "- {} [lines {}-{}]\n  {}\n",
                        hit.title, hit.line_start, hit.line_end, hit.snippet
                    ));
                    shown += 1;
                }
            }
            if shown == 0 {
                text.push_str(&format!(
                    "No sections matched. Search the whole batch with ctx_search({{queries:[...], source:\"{source}\"}}).\n"
                ));
            } else {
                text.push_str(&format!(
                    "More: ctx_search({{queries:[...], source:\"{source}\", offset:{HITS_PER_QUERY}}})\n"
                ));
            }
            // One cap implementation in the crate (UTF-8 boundary safe).
            ToolResult::ok(cap_reply(text))
        })
    }
}

/// Run one command under the resolved shell, with the call's remaining
/// deadline and the abort signal.
async fn run_command(
    shell: &crate::coding_agent::tools::ShellConfig,
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
    signal: Option<&AbortSignal>,
) -> Result<(String, String, Option<i32>), String> {
    let mut process = tokio::process::Command::new(shell.program());
    process.args(shell.args());
    if shell.stdin_transport() {
        process.stdin(std::process::Stdio::piped());
    } else {
        process.arg(command);
    }
    process
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = process.spawn().map_err(|error| {
        format!(
            "context-mode: could not start `{}`: {error}",
            shell.program()
        )
    })?;
    if shell.stdin_transport() {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            // The command is the stdin payload on a `-s` shell; a failed
            // write means it never ran, so fail instead of reporting the
            // silent-EOF exit 0.
            if let Err(error) = stdin.write_all(command.as_bytes()).await {
                return Err(format!(
                    "context-mode: could not deliver the command to the shell's stdin: {error}"
                ));
            }
            let _ = stdin.shutdown().await;
        }
    }
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let output = tokio::select! {
        output = tokio::time::timeout(timeout, &mut wait) => {
            match output {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return Err(format!("context-mode: command failed: {error}")),
                Err(_) => {
                    return Err(format!(
                        "context-mode: command timed out after {:.0}s",
                        timeout.as_secs_f64()
                    ))
                }
            }
        }
        _ = abort_wait(signal) => {
            // Dropping the wait future drops the child; `kill_on_drop` kills
            // the whole process tree.
            return Err("context-mode: command aborted".to_owned());
        }
    };
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if stdout.len() > MAX_OUTPUT_CHARS {
        stdout.truncate(MAX_OUTPUT_CHARS);
        stdout.push_str("\n... [output truncated at 1 MiB] ...");
    }
    Ok((stdout, stderr, output.status.code()))
}

async fn abort_wait(signal: Option<&AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}
