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
use crate::coding_agent::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

use super::bash::{
    apply_session_env, apply_tree_kill_spawn_attributes, drive_shell_to_exit,
    finalize_shell_result_with_limits, resolve_command_timeout, run_with_operations,
    shell_output_schema, spawn_pipe_reader, validate_timeout_secs, BashRun, ShellCapture,
    TreeKillGuard,
};

/// Upstream `POWERSHELL_ARGS` (utils/shell.ts:122): the fixed argument block
/// ahead of the command text, `-Command` included.
pub const POWERSHELL_ARGS: [&str; 5] = [
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

/// Upstream #11964: Windows PowerShell 5.1 turns redirected native stderr
/// into error records and clears `$?`, so `git push 2>&1 | ...` exits 1 even
/// when git exited 0. The leading lone `;` keeps an unfinished last statement
/// (`Get-ChildItem |`) a parse error rather than continuing into the suffix.
/// PowerShell 7 is unaffected and gets no suffix.
const WINDOWS_POWERSHELL_EXIT_SUFFIX: &str = "\n;\nif ($?) { exit 0 }\nif (($LASTEXITCODE -eq 0) -and (\"$($Error[0].FullyQualifiedErrorId)\" -like \"NativeCommandError*\")) { exit 0 }\nexit 1";

/// The suffix appended for `powershell.exe` only, and never after a trailing
/// backtick (it would continue the user's line into the suffix).
#[doc(hidden)]
pub fn windows_powershell_exit_suffix(shell: &str, command: &str) -> &'static str {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == "powershell.exe" && !command.ends_with('`') {
        WINDOWS_POWERSHELL_EXIT_SUFFIX
    } else {
        ""
    }
}

/// Resolve the PowerShell executable (upstream `getPowerShellConfig`):
/// prefer PowerShell 7 (`pwsh.exe`), fall back to Windows PowerShell
/// (`powershell.exe`). PATH first, then the standard installation directories
/// (`%ProgramFiles%\PowerShell\7`, `%SystemRoot%\System32\WindowsPowerShell\v1.0`),
/// which Windows does not always put on PATH (upstream `61060ddf8`). `None`
/// off-Windows or when no executable exists.
pub fn resolve_powershell_executable() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let mut shell = find_on_path("pwsh.exe");
    if shell.is_none() {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            let candidate = std::path::PathBuf::from(program_files)
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe");
            if super::bash::shell_path_exists(&candidate) {
                shell = Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    if shell.is_none() {
        shell = find_on_path("powershell.exe");
    }
    if shell.is_none() {
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let candidate = std::path::PathBuf::from(system_root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe");
            if super::bash::shell_path_exists(&candidate) {
                shell = Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    shell
}

/// Search `PATH` for an executable (upstream `findExecutableOnPath` Windows
/// branch, in-process: first existing match wins).
fn find_on_path(executable: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(executable);
        if super::bash::shell_path_exists(&candidate) {
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
        // Same reason as the bash tool: a `prefer pir grep|sed` hint steers
        // the model to read files in ranges instead of with `read`.
        "Execute a PowerShell command. Default sequential due to side effects."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("Execute PowerShell commands".to_owned())
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
        // one-shot spawn below (see the module docs). Read from the session's
        // own agent directory, not the process environment.
        let prefs = crate::config::settings::shell_prefs_in(&ctx.resolve_agent_dir());
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
                return run_with_operations(BashRun {
                    operations,
                    command: &command,
                    cwd: &cwd,
                    timeout_secs,
                    timeout_from_setting,
                    session_env,
                    on_update,
                    signal,
                })
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
                .arg(format!(
                    "{UTF8_OUTPUT_PREFIX}{command}{}",
                    windows_powershell_exit_suffix(&shell, &command)
                ))
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

            // Wait for exit / timeout / abort, stream throttled snapshots,
            // kill the process tree on timeout/abort, drain detached pipes,
            // and flush the final snapshot (the bash tool's driver).
            let exit = drive_shell_to_exit(
                &mut child,
                &mut tree_kill,
                &capture,
                out_task,
                err_task,
                on_update.as_ref(),
                started_at,
                timeout_secs,
                signal.as_ref(),
                true,
            )
            .await;

            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            finalize_shell_result_with_limits(
                &mut guard,
                started_at,
                exit.exit_code(),
                exit.aborted,
                exit.timed_out,
                timeout_secs,
                timeout_from_setting,
                DEFAULT_MAX_LINES,
                DEFAULT_MAX_BYTES,
            )
        })
    }
}
