//! PowerShell tool (upstream `core/tools/powershell.ts`): the Windows-native
//! shell, reusing the bash tool's spawn/capture machinery. Registered by
//! default like every core tool (upstream `createAllToolDefinitions` registers
//! it unconditionally; execution stays Windows-gated — off-Windows the
//! executable resolution returns `None`). It is not in the upstream
//! `DEFAULT_TOOL_NAMES`, so `--tools`/modifier paths keep it out unless named.
//!
//! **One-shot only — no pooled member, ever.** Every call spawns a fresh
//! `pwsh -Command <script>` (stdin null, completion = process exit, output =
//! the two pipes). The persistent PowerShell member the shell pool still
//! supports for bash was removed from the product for this tool: it delivered
//! the command script on the member's stdin together with a per-invocation
//! completion nonce, and pwsh 7 parses `-Command -` lazily, so a command could
//! read the not-yet-consumed tail of that channel with
//! `[Console]::OpenStandardInput()` (which bypasses `Console.In`/`SetIn`),
//! lift the nonce and print a byte-identical sentinel — completing the pool
//! with a forged exit code while the real command kept running. The one-shot
//! path has no nonce and no stdin script, so there is nothing to steal: the
//! command can read its own argv, but the completion signal is the process's
//! own exit status, which no output can forge. The cost is one process
//! (400-1500 ms of .NET startup) per PowerShell call; bash/brush keep their
//! pooled member, which never places a secret on stdin (`< /dev/null` in the
//! command subshell, and the nonce lives only in the post-command `printf`).
//! The generic pool's PowerShell protocol arms were deleted, not disabled, so
//! a future caller cannot re-enable the vulnerable path.

use crate::agent::tool_batch::{ToolExecutionMode, ToolResult};
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::coding_agent::truncate::{
    format_shell_output, truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

use super::bash::{
    append_status, apply_session_env, apply_tree_kill_spawn_attributes, format_command_timed_out,
    resolve_command_timeout, run_with_operations, shell_heartbeat_payload, shell_output_schema,
    spawn_pipe_reader, validate_timeout_secs, ShellCapture, TreeKillGuard, EXIT_STDIO_ABSOLUTE_CAP,
    EXIT_STDIO_GRACE, SHELL_HEARTBEAT_INTERVAL, STRUCTURED_OUTPUT_MAX_BYTES, UPDATE_THROTTLE_MS,
};

/// Upstream `POWERSHELL_ARGS` (utils/shell.ts:122): the fixed argument block
/// ahead of the command text, `-Command` included.
pub(crate) const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

/// Upstream `UTF8_OUTPUT_PREFIX`: forces UTF-8 console output so captured
/// bytes decode losslessly.
const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// Resolve the PowerShell executable (upstream `getPowerShellConfig`):
/// prefer PowerShell 7 (`pwsh.exe`), fall back to Windows PowerShell
/// (`powershell.exe`), both found on `PATH`. `None` off-Windows or when no
/// executable exists.
pub(crate) fn resolve_powershell_executable() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    for name in ["pwsh.exe", "powershell.exe"] {
        if let Some(path) = find_on_path(name) {
            return Some(path);
        }
    }
    None
}

/// Search `PATH` for an executable (upstream `findExecutableOnPath` Windows
/// branch, in-process: first existing match wins).
fn find_on_path(executable: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(executable);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// The PowerShell tool: `ToolName "powershell"` (upstream `allToolNames`).
/// Default-active like the rest of the core registry (upstream registers it
/// unconditionally in `createAllToolDefinitions`; only the `DEFAULT_TOOL_NAMES`
/// flag-path base leaves it out unless named).
/// Upstream `PowerShellOperations = BashOperations`: the PowerShell tool shares
/// the bash tool's pluggable execution backend.
pub use super::bash::BashOperations as PowerShellOperations;

pub struct PowerShellTool {
    operations: Option<std::sync::Arc<dyn PowerShellOperations>>,
}

impl PowerShellTool {
    /// The built-in tool: a fresh `pwsh -Command` one-shot per call (or the
    /// `env_client` remote arm when configured).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit execution backend (upstream
    /// `createPowerShellTool(cwd, { operations })`).
    pub fn with_operations(operations: std::sync::Arc<dyn PowerShellOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }
}

impl Default for PowerShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool for PowerShellTool {
    fn name(&self) -> &'static str {
        "powershell"
    }
    fn description(&self) -> &'static str {
        "Execute a PowerShell command. Default sequential due to side effects. When PIR_BIN is set, prefer `& $env:PIR_BIN grep|sed|python …` over the system grep/sed/python; use the plain commands only when PIR_BIN is unavailable or pir rejects the usage."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Execute PowerShell commands; prefer pir grep|sed|python ($env:PIR_BIN) when available"
                .to_owned(),
        )
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec![
            "You can inspect PI_* environment variables for current model and session details."
                .to_owned(),
        ]
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    fn has_external_side_effects(&self) -> bool {
        true
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        // Upstream shares the bash schema: the timeout is a NUMBER (fractional
        // values allowed), optional, no default.
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The PowerShell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional; when omitted, the tools.commandTimeoutMs setting applies, 120 seconds by default)" }
            },
            "required": ["command"]
        }))
    }
    /// Upstream PowerShell shares `bashOutputSchema` with the bash tool
    /// (`#getPowerShellTool` builds it from `bashOutputSchema`), and the
    /// structured content this tool emits uses exactly that shape.
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(shell_output_schema())
    }
    /// Registered on every platform but NOT in upstream `DEFAULT_TOOL_NAMES`
    /// (`settings-manager.ts:83` = `["read", "bash", "edit", "write"]`): the
    /// tool stays out of the default active set and is only enabled through
    /// the `--tools` allowlist / modifiers (upstream's same behavior).
    fn default_active(&self) -> bool {
        false
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let command = args
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        // Upstream bash.ts (shared shell tool): the timeout is OPTIONAL, may
        // be fractional, is capped at 2_147_483 seconds, and a non-finite or
        // non-positive value is rejected.
        let call_timeout_secs =
            match validate_timeout_secs(args.get("timeout").and_then(|v| v.as_f64())) {
                Ok(timeout) => timeout,
                Err(error) => return Box::pin(async move { ToolResult::error(error) }),
            };
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();
        // PI_* session environment, same contract as the bash tool (upstream
        // `resolveSpawnContext`): previous values cleared first.
        let session_env = ctx.session_env.clone();
        // Shell preferences: `tools.commandTimeoutMs` (the 120 s default
        // command timeout unless configured or disabled with 0). The
        // persistent-shell pool is bash-only: PowerShell always takes the
        // one-shot spawn below (see the module docs).
        let prefs = crate::config::settings::current_shell_prefs();
        // A detached background call skips the settings default (only an
        // explicit per-call timeout caps it).
        let setting_timeout = if ctx.background {
            None
        } else {
            prefs.command_timeout()
        };
        let (timeout_secs, timeout_from_setting) =
            resolve_command_timeout(call_timeout_secs, setting_timeout);
        // Live partial-result channel (upstream tool `onUpdate`).
        let on_update = ctx.update_channel();
        // An injected backend wins over the local one-shot spawn: it runs the
        // command text as-is (upstream `createShellToolDefinition` does not add
        // the local UTF-8 prefix to custom operations).
        let operations = self.operations.clone();
        Box::pin(async move {
            if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                return ToolResult::error("Command aborted");
            }
            if command.trim().is_empty() {
                return ToolResult::error("Invalid command: command is required");
            }
            if let Some(operations) = operations {
                return run_with_operations(
                    operations,
                    &command,
                    &cwd,
                    timeout_secs,
                    timeout_from_setting,
                    session_env,
                    on_update,
                    signal,
                )
                .await;
            }
            if !cwd.is_dir() {
                return ToolResult::error(format!(
                    "Working directory '{}' does not exist",
                    cwd.display()
                ));
            }
            if command.trim().is_empty() {
                return ToolResult::error("Invalid command: command is required");
            }
            // Upstream `getPowerShellConfig`: the tool is Windows-only.
            if !cfg!(windows) {
                return ToolResult::error("The powershell tool is only available on Windows.");
            }
            let Some(shell) = resolve_powershell_executable() else {
                return ToolResult::error(
                    "No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH.",
                );
            };

            let started_at = std::time::Instant::now();
            // One-shot spawn, unconditionally: no pooled member for
            // PowerShell (stdin-borne sentinel nonce → forgeable via
            // `OpenStandardInput`; see the module docs). Completion is the
            // child's own exit status, which no captured output can fake.
            let mut spawn = tokio::process::Command::new(&shell);
            spawn
                .args(POWERSHELL_ARGS)
                .arg(format!("{UTF8_OUTPUT_PREFIX}{command}"))
                .current_dir(&cwd)
                // Upstream `stdio: ["ignore", "pipe", "pipe"]` (the powershell
                // tool has no stdin command transport, so the first slot is
                // always "ignore"): never hand the host's own stdin (an RPC
                // pipe / the TUI terminal) to the child. With the one-shot
                // path there is also no command script on stdin to defend.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                // CREATE_NO_WINDOW (0x08000000): the per-call PowerShell must
                // not flash a console window (upstream `windowsHide: true`).
                spawn.creation_flags(0x0800_0000);
            }
            // PI_* session environment (upstream `resolveSpawnContext`; the
            // same variables the bash tool manages).
            apply_session_env(&mut spawn, session_env.as_deref(), None);
            apply_tree_kill_spawn_attributes(&mut spawn);
            let mut child = match spawn.spawn() {
                Ok(child) => child,
                Err(error) => return ToolResult::error(format!("Command failed: {error}")),
            };
            // Tree-kill handle (upstream `killProcessTree`): the Job Object on
            // Windows, the child's own process group on Unix. The RAII guard
            // covers the paths that leave this future without reaching the
            // explicit kill below (caller cancellation, panic, an early
            // return).
            let mut tree_kill = TreeKillGuard::arm(&child);

            // Interleaved stdout/stderr capture (the bash tool's shape): the
            // same `2 * DEFAULT_MAX_BYTES` rolling window, with evicted
            // prefixes spilled to a temp file instead of staying in memory.
            let capture = std::sync::Arc::new(std::sync::Mutex::new(ShellCapture::new(
                2 * DEFAULT_MAX_BYTES,
            )));
            let Some(stdout) = child.stdout.take() else {
                return ToolResult::error("Command failed: stdout pipe unavailable");
            };
            let Some(stderr) = child.stderr.take() else {
                return ToolResult::error("Command failed: stderr pipe unavailable");
            };
            let cap_out = std::sync::Arc::clone(&capture);
            let cap_err = std::sync::Arc::clone(&capture);
            let out_task = spawn_pipe_reader(stdout, move |chunk| {
                if let Ok(mut guard) = cap_out.lock() {
                    guard.append(chunk);
                }
            });
            let err_task = spawn_pipe_reader(stderr, move |chunk| {
                if let Ok(mut guard) = cap_err.lock() {
                    guard.append(chunk);
                }
            });

            // Wait for exit / timeout / abort (bash tool's polling loop), with
            // throttled streaming snapshots and process-tree kill.
            let deadline = timeout_secs
                .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
            let mut timed_out = false;
            let mut aborted = false;
            // Live partial-result streaming (upstream `emitOutputUpdate` with
            // BASH_UPDATE_THROTTLE_MS).
            let mut last_update_total = 0usize;
            // Heartbeat clock: reset on every output snapshot (see the bash
            // tool's `SHELL_HEARTBEAT_INTERVAL`).
            let mut last_heartbeat_at = std::time::Instant::now();
            if let Some(emit) = &on_update {
                let mut initial = serde_json::Map::new();
                initial.insert("content".to_owned(), serde_json::json!([]));
                emit(serde_json::Value::Object(initial));
            }
            // Wait for exit / timeout / abort on their own futures (the bash
            // tool's shape): the child wait wakes the moment the process
            // exits instead of a 50 ms poll tick, and the rolling snapshot
            // rides a throttled interval branch.
            let update_period = std::time::Duration::from_millis(UPDATE_THROTTLE_MS as u64);
            let mut update_ticker = tokio::time::interval_at(
                tokio::time::Instant::now() + update_period,
                update_period,
            );
            update_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let deadline_wait = async {
                match deadline {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline_wait);
            let abort_wait = async {
                match signal.as_ref() {
                    Some(signal) => signal.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(abort_wait);
            // `child.wait()` borrows the child mutably, so it is scoped to this
            // block; the timeout/abort kill runs once the borrow is dropped.
            let wait_status = {
                let wait = child.wait();
                tokio::pin!(wait);
                let mut status = None;
                loop {
                    tokio::select! {
                        biased;
                        result = &mut wait => {
                            status = Some(result);
                            break;
                        }
                        _ = &mut abort_wait => {
                            aborted = true;
                            break;
                        }
                        _ = &mut deadline_wait => {
                            timed_out = true;
                            break;
                        }
                        _ = update_ticker.tick() => {
                            // Stream a throttled rolling snapshot when new
                            // output arrived (the bash tool's `update_snapshot`:
                            // window tail plus truncation details and the spill
                            // path once one exists), else the silent-command
                            // heartbeat.
                            if let Some(emit) = &on_update {
                                let snapshot = capture
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .update_snapshot(&mut last_update_total);
                                match snapshot {
                                    Some(payload) => {
                                        emit(payload);
                                        last_heartbeat_at = std::time::Instant::now();
                                    }
                                    None if last_heartbeat_at.elapsed()
                                        >= SHELL_HEARTBEAT_INTERVAL =>
                                    {
                                        let guard = capture
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                                        emit(shell_heartbeat_payload(
                                            &guard,
                                            started_at.elapsed(),
                                        ));
                                        drop(guard);
                                        last_heartbeat_at = std::time::Instant::now();
                                    }
                                    None => {}
                                }
                            }
                        }
                    }
                }
                status
            };
            if wait_status.is_none() {
                tree_kill.terminate();
                let _ = child.start_kill();
            }
            // Same post-exit pipe-quiet grace as the bash tool (upstream
            // `waitForChildProcess`): a detached grandchild holding the
            // inherited stdout/stderr write ends must not hang the join past
            // the already-fired timeout/abort. Wait a bounded quiet window
            // (re-armed on every captured chunk, under an absolute cap), then
            // drop the readers and proceed with whatever was captured.
            let exit_at = std::time::Instant::now();
            loop {
                if out_task.is_finished() && err_task.is_finished() {
                    break;
                }
                let last_data = capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_chunk_at();
                let now = std::time::Instant::now();
                if now.duration_since(exit_at) >= EXIT_STDIO_ABSOLUTE_CAP
                    || now.duration_since(exit_at.max(last_data)) >= EXIT_STDIO_GRACE
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            out_task.abort();
            err_task.abort();
            let _ = tokio::join!(out_task, err_task);
            // Final snapshot flush (upstream `finishOutput`'s emit).
            if let Some(emit) = &on_update {
                let payload = capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .update_snapshot(&mut last_update_total);
                if let Some(payload) = payload {
                    emit(payload);
                }
            }
            // The wait resolved in the select above when the child exited on
            // its own; a timeout/abort kill still needs its reaping wait.
            let status = match wait_status {
                Some(status) => status,
                None => child.wait().await,
            };
            // The run is over on its own terms: intentionally backgrounded
            // descendants keep running (upstream kills only on
            // abort/timeout), so the RAII guard must not fire at scope exit.
            tree_kill.disarm();

            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Exit code for the shared result builder: the child's exit
            // status; abort/timeout carry none.
            let exit_code = if aborted || timed_out {
                None
            } else {
                status
                    .as_ref()
                    .ok()
                    .and_then(|status| status.code())
                    .map(i64::from)
            };
            finalize_powershell_result(
                &mut guard,
                started_at,
                exit_code,
                aborted,
                timed_out,
                timeout_secs,
                timeout_from_setting,
            )
        })
    }
}

/// Build the final [`ToolResult`] for a finished PowerShell run (the bash
/// tool's bounded capture + spill file, text trimmed, structured output raw).
fn finalize_powershell_result(
    capture: &mut ShellCapture,
    started_at: std::time::Instant,
    exit_code: Option<i64>,
    aborted: bool,
    timed_out: bool,
    timeout_secs: Option<f64>,
    timeout_from_setting: bool,
) -> ToolResult {
    // Status line appended after the output (upstream appendStatus).
    // A signal-killed shell has no exit code: upstream reports the
    // termination explicitly instead of treating it as success.
    let status_line = if aborted {
        Some("Command aborted".to_owned())
    } else if timed_out {
        Some(format_command_timed_out(timeout_secs, timeout_from_setting))
    } else {
        match exit_code {
            None => Some("Command terminated without an exit code".to_owned()),
            Some(0) => None,
            Some(code) => Some(format!("Command exited with code {code}")),
        }
    };
    // Upstream `formatOutput`: "(no output)" on completed runs, empty
    // on abort/timeout.
    let empty_text = if aborted || timed_out {
        ""
    } else {
        "(no output)"
    };
    // Raw captured output only (trailing whitespace trimmed for display); the
    // status line is appended AFTER truncation in the result branches below
    // (upstream `appendStatus` runs post-`truncateTail`). Folding it into
    // `text` here as well appended it twice.
    //
    // The TRIMMED view is display-only. The advertised full-output file must
    // hold the complete captured stream (the bash arm passes its untrimmed
    // tail for the same reason): `spill_if_needed` writes its argument
    // verbatim when nothing was evicted, so handing it the trimmed view would
    // drop the trailing whitespace from the file the model is pointed at.
    let raw_text = capture.tail_string();
    // Borrow the trimmed display view: the old `trim_end().to_owned()`
    // allocated a full second copy of the captured output before
    // `truncate_tail` immediately allocated its own retained window. The
    // untrimmed `raw_text` stays available for the spill file below.
    let trimmed_len = raw_text.trim_end().len();
    let display_text = &raw_text[..trimmed_len];

    // Structured content for programmatic callers (upstream bashOutputSchema
    // plus `readFullOutput`, shared with the bash tool): full output up to
    // 1MB, exit code, wall time; completed paths only. `truncated` and
    // `full_output_path` describe what the 1MB structured cap dropped, not
    // the 50KB display truncation.
    //
    // Same three-copy size shape as the bash tool (see
    // `finalize_shell_result`): `content` is the model-facing truncated tail,
    // `details.truncation.content` repeats it for upstream session parity, and
    // this `structuredContent.output` is the full body that only live
    // programmatic callers read. The session writer drops the
    // `structuredContent` copy (`session_manager::types::session_tool_result`).
    let structured_content = if !aborted && !timed_out {
        match exit_code {
            Some(code) => {
                let (full_output, truncated) = capture.full_output(STRUCTURED_OUTPUT_MAX_BYTES);
                let wall_time_seconds =
                    (started_at.elapsed().as_millis() as f64 / 100.0).round() / 10.0;
                let mut structured = serde_json::json!({
                    "output": full_output,
                    "truncated": truncated,
                    "exit_code": serde_json::json!(code),
                    "wall_time_seconds": wall_time_seconds,
                });
                if truncated {
                    if let Some(path) = capture.spill_path() {
                        structured["full_output_path"] = serde_json::json!(path);
                    }
                }
                Some(structured)
            }
            None => None,
        }
    } else {
        None
    };
    let apply_structured = |result: &mut ToolResult| {
        if let Some(structured) = &structured_content {
            result.structured_content = Some(structured.clone());
        }
    };

    // 截断（tail）+ 全量落盘 footer + details（与 bash 的最终分支一致）；
    // totalLines/totalBytes 用全量计数覆盖窗口统计。
    let truncation = capture.with_full_totals(truncate_tail(
        display_text,
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES,
    ));
    if truncation.truncated {
        // The advertised full-output file must be the complete captured
        // stream (upstream temp-file semantics), not the trimmed display copy.
        let full_path = capture.spill_if_needed(&raw_text);
        // Upstream `BashToolDetails` = {truncation, fullOutputPath}.
        let mut details = serde_json::Map::new();
        details.insert("truncation".to_owned(), truncation.to_details_json());
        if let Some(path) = &full_path {
            details.insert("fullOutputPath".to_owned(), serde_json::json!(path));
        }
        let details = serde_json::Value::Object(details);
        // Upstream `formatOutput`: the placeholder applies before the
        // truncation footer.
        let rendered = format_shell_output(&truncation, full_path.as_deref(), empty_text);
        return match status_line {
            None => {
                let mut result = ToolResult::ok(rendered);
                result.details = Some(details);
                apply_structured(&mut result);
                result
            }
            Some(line) => {
                let mut result = ToolResult::error(append_status(&rendered, &line));
                result.details = Some(details);
                apply_structured(&mut result);
                result
            }
        };
    }
    // Non-truncated result: `truncation.content` is already a byte-for-byte
    // owned copy of the trimmed display text (built by `truncate_tail`).
    // Reuse it and release the untrimmed capture instead of allocating a
    // second trimmed copy as the old `trim_end().to_owned()` did.
    drop(raw_text);
    let text = truncation.content;
    match status_line {
        None => {
            let mut result = ToolResult::ok(if text.is_empty() {
                empty_text.to_owned()
            } else {
                text
            });
            apply_structured(&mut result);
            result
        }
        Some(line) => {
            let trimmed = text.trim_end();
            let mut result = ToolResult::error(append_status(
                if trimmed.is_empty() {
                    empty_text
                } else {
                    trimmed
                },
                &line,
            ));
            apply_structured(&mut result);
            result
        }
    }
}
