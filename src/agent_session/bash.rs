use super::*;

use crate::coding_agent::tools::shell_pool::{
    apply_tree_kill_spawn_attributes, shell_spawn_target, ProcessTreeKill,
};
use crate::coding_agent::truncate::{truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// One streamed bash-output chunk callback (upstream `executeBash`'s
/// `onChunk`, agent-session.ts:3851-3870).
pub(super) type BashChunkCallback = Box<dyn Fn(&str) + Send + Sync>;

impl AgentSession {
    // ------------------------------------------------------------------
    // Bash execution (upstream executeBash / abortBash)
    // ------------------------------------------------------------------

    /// Run a shell command outside the agent loop (upstream
    /// `session.executeBash`, backing the RPC `bash` command). The result
    /// carries the combined output, exit code, and cancellation/truncation
    /// flags. `abort_bash` cancels a running command. Like upstream, the
    /// result is recorded into the session context
    /// ([`Self::record_bash_result`]).
    pub async fn execute_bash(&self, command: &str) -> BashResult {
        self.execute_bash_with_options(command, false).await
    }

    /// [`Self::execute_bash`] with upstream's execution options
    /// (`executeBash(command, onChunk, { excludeFromContext })`): when
    /// `exclude_from_context` is set, the recorded result stays in the
    /// transcript but never reaches the LLM context (`!!`-style commands).
    pub async fn execute_bash_with_options(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> BashResult {
        let generation = self.session_storage_generation();
        self.execute_bash_with_options_at_generation(command, exclude_from_context, generation)
            .await
    }

    /// [`Self::execute_bash_with_options`] for a caller that captured the
    /// storage generation BEFORE it dispatched a (possibly slow) `user_bash`
    /// hook: the result is recorded only while that generation still names
    /// the live transcript, so a switch that happened in between cannot
    /// persist the outgoing command's `<bash-execution>` into the new session.
    pub async fn execute_bash_with_options_at_generation(
        &self,
        command: &str,
        exclude_from_context: bool,
        generation: u64,
    ) -> BashResult {
        self.execute_bash_with_id_at_generation(
            command,
            exclude_from_context,
            None,
            None,
            generation,
        )
        .await
    }

    /// Dispatch the `user_bash` event for a `!`-prefixed command through BOTH
    /// plugin faces: the session's JS runtime handle (when configured) and the
    /// unified plugin host's native Rust plugins, in host list order.
    ///
    /// This is the host seam the TUI and RPC `!` paths use. A native
    /// `user_bash` handler is consulted exactly once: the host's JS adapter is
    /// the only JS entry in the ordered list, so a session that owns both the
    /// handle and the host never runs the JS handlers twice. With neither face
    /// configured the outcome is [`crate::agent_session::UserBashOutcome::Local`]
    /// and the caller executes the command itself.
    pub async fn dispatch_user_bash_event(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> crate::agent_session::UserBashOutcome {
        // The `!` path runs on the TUI worker thread. A JS/native `user_bash`
        // handler that never settles must not keep that worker (and the TUI's
        // spinner/steering state) blocked forever; upstream hook timeouts are
        // minutes long, so use a short command-scale bound and fail closed.
        const USER_BASH_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        match tokio::time::timeout(
            USER_BASH_DISPATCH_TIMEOUT,
            crate::agent_session::dispatch_user_bash_with_host(
                self.extensions_handle(),
                self.plugins.as_deref(),
                command,
                exclude_from_context,
            ),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                crate::ext_diag!(
                    "[ext] user_bash dispatch timed out after {}s; command not run",
                    USER_BASH_DISPATCH_TIMEOUT.as_secs()
                );
                crate::agent_session::UserBashOutcome::Aborted
            }
        }
    }

    /// [`Self::execute_bash`] with the full upstream option set
    /// (`executeBash(command, onChunk, { excludeFromContext, id })`,
    /// agent-session.ts:3851-3870): `on_chunk` receives each interleaved
    /// output chunk, and every chunk ALSO emits the
    /// [`SessionEvent::BashExecutionUpdate`] session event carrying `id`
    /// (upstream `_emit({type: "bash_execution_update", id, delta})`,
    /// agent-session.ts:3862) — independent of any `on_chunk` callback.
    pub async fn execute_bash_with_id(
        &self,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
        on_chunk: Option<BashChunkCallback>,
    ) -> BashResult {
        let generation = self.session_storage_generation();
        self.execute_bash_with_id_at_generation(
            command,
            exclude_from_context,
            id,
            on_chunk,
            generation,
        )
        .await
    }

    /// [`Self::execute_bash_with_id`] with an explicit storage generation:
    /// the result is recorded only while `generation` still names the live
    /// transcript (see [`Self::execute_bash_with_options_at_generation`]).
    pub async fn execute_bash_with_id_at_generation(
        &self,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
        on_chunk: Option<BashChunkCallback>,
        generation: u64,
    ) -> BashResult {
        let id = id.map(str::to_owned);
        let events = self.session_events.clone();
        let emit_update = move |delta: &str| {
            emit_session_event_to(
                &events,
                SessionEvent::BashExecutionUpdate {
                    id: id.clone(),
                    delta: delta.to_owned(),
                },
            );
            if let Some(on_chunk) = &on_chunk {
                on_chunk(delta);
            }
        };
        let abort = AbortSignal::new();
        let seq = self.bash_abort_seq.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .bash_abort
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.push((seq, abort.clone()));
        }
        // A12: the registration is owned by an RAII guard, so a caller that
        // drops this future mid-command (RPC disconnect, task cancellation)
        // still removes `(seq, signal)` from the abort set instead of leaking
        // it for the lifetime of the session.
        let registration = BashAbortRegistration { session: self, seq };
        let result = run_shell_command(
            command,
            &self.cwd,
            &self.agent_dir,
            Some(abort),
            Some(&emit_update),
        )
        .await;
        drop(registration);
        // A storage swap during the command (a `/new` while a long `!` was
        // running) makes the captured transcript stale: the old session is
        // persisted by whoever switched, and this result must not land in the
        // replacement.
        if self.session_storage_generation_matches(generation) {
            self.record_bash_result(command, &result, exclude_from_context)
                .await;
        }
        result
    }

    /// Cancel all running bash commands (upstream `abortBash`, which aborts
    /// every controller in `_bashAbortControllers`).
    pub fn abort_bash(&self) {
        let guard = self
            .bash_abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, signal) in guard.iter() {
            signal.abort();
        }
    }

    /// Record a bash execution result in the session context (upstream
    /// `recordBashResult`, agent-session.ts:2293-2395). pir has no
    /// `bashExecution` agent-message role, so the record uses the closest
    /// existing entry shapes:
    /// - `exclude_from_context == false`: a `custom_message` entry with
    ///   `customType: "bashExecution"`, content = the framed
    ///   `bashExecutionToText` user text (upstream `convertToLlm` renders
    ///   bashExecution messages as user text) and `details` carrying the full
    ///   upstream field set (command/output/exitCode/cancelled/truncated/
    ///   fullOutputPath).
    /// - `exclude_from_context == true`: a NON-context `custom` entry (same
    ///   type tag), so the record stays in the transcript but out of the LLM
    ///   context (upstream `excludeFromContext`).
    ///
    /// While a run is streaming the record is deferred to the end of the run
    /// (upstream `_pendingBashMessages` flushed on settle) so it can never
    /// land between an assistant tool call and its result.
    pub async fn record_bash_result(
        &self,
        command: &str,
        result: &BashResult,
        exclude_from_context: bool,
    ) {
        let details = serde_json::json!({
            "command": command,
            "output": result.output,
            "exitCode": result.exit_code,
            "cancelled": result.cancelled,
            "truncated": result.truncated,
            "fullOutputPath": result.full_output_path,
        });
        if self.is_streaming() {
            // Defer to the end of the run (upstream `_pendingBashMessages`).
            self.pending_bash
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(PendingBashRecord {
                    command: command.to_owned(),
                    details,
                    exclude_from_context,
                    // The transcript this record belongs to: a run that is
                    // still streaming when the storage is swapped must not
                    // flush into the replacement.
                    generation: self.session_storage_generation(),
                });
            return;
        }
        self.append_bash_record(command, &details, exclude_from_context)
            .await;
    }

    /// [`Self::record_bash_result`] guarded by the storage generation the
    /// caller captured when it STARTED the command: returns `false` (nothing
    /// recorded) once a switch replaced the transcript.
    pub async fn record_bash_result_at_generation(
        &self,
        command: &str,
        result: &BashResult,
        exclude_from_context: bool,
        generation: u64,
    ) -> bool {
        if !self.session_storage_generation_matches(generation) {
            return false;
        }
        self.record_bash_result(command, result, exclude_from_context)
            .await;
        true
    }

    /// [`Self::record_extension_bash_result`] guarded by the storage
    /// generation captured at command start (see
    /// [`Self::record_bash_result_at_generation`]).
    pub async fn record_extension_bash_result_at_generation(
        &self,
        command: &str,
        output: &str,
        is_error: bool,
        exclude_from_context: bool,
        generation: u64,
    ) -> bool {
        if !self.session_storage_generation_matches(generation) {
            return false;
        }
        self.record_extension_bash_result(command, output, is_error, exclude_from_context)
            .await;
        true
    }

    /// Record a bash result an extension handled itself (upstream
    /// rpc-mode.ts:569-573: `recordBashResult(command, eventResult.result,
    /// { excludeFromContext })`). The reduced
    /// [`UserBashOutcome::Handled`](crate::extensions::UserBashOutcome::Handled)
    /// shape carries only `{output, isError}`, so the error flag maps onto
    /// the recorded exit code (1/0); cancellation/truncation never apply on
    /// this path.
    pub async fn record_extension_bash_result(
        &self,
        command: &str,
        output: &str,
        is_error: bool,
        exclude_from_context: bool,
    ) {
        let result = BashResult {
            output: output.to_owned(),
            exit_code: Some(if is_error { 1 } else { 0 }),
            cancelled: false,
            truncated: false,
            full_output_path: None,
        };
        self.record_bash_result(command, &result, exclude_from_context)
            .await;
    }

    /// Append one bash-execution record to session storage (see
    /// [`Self::record_bash_result`] for the shape rationale).
    async fn append_bash_record(&self, command: &str, details: &JsonValue, exclude: bool) {
        let output = details["output"].as_str().unwrap_or_default();
        let exit_code = details["exitCode"].as_i64();
        let cancelled = details["cancelled"].as_bool().unwrap_or(false);
        let truncated = details["truncated"].as_bool().unwrap_or(false);
        let full_output_path = details["fullOutputPath"].as_str();
        let timestamp = crate::ai::util::now_ms();
        match &self.session {
            Some(session) => {
                let mut session = session.lock().await;
                let _ = session.append_bash_execution(
                    command,
                    output,
                    exit_code,
                    cancelled,
                    truncated,
                    full_output_path,
                    exclude,
                    timestamp,
                );
            }
            None => {
                // Ephemeral sessions keep the output only in the transcript,
                // rendered exactly like `convertToLlm` would (upstream
                // `bashExecutionToText`).
                let text = crate::session_manager::types::bash_execution_to_text(
                    command,
                    output,
                    exit_code,
                    cancelled,
                    truncated,
                    full_output_path,
                );
                let mut history = self
                    .history
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Arc::make_mut(&mut *history).push(Message::User(UserMessage {
                    role: UserRole::User,
                    content: vec![ContentBlock::Text(TextContent {
                        text,
                        text_signature: None,
                    })],
                    tools_added: None,
                    tools_removed: None,
                    sections: None,
                    timestamp,
                }));
            }
        }
    }

    /// Flush bash-execution records deferred during a streaming run (upstream
    /// `_flushPendingBashMessages`, called on settle).
    pub(super) async fn flush_pending_bash_records(&self) {
        let pending: Vec<PendingBashRecord> = self
            .pending_bash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        // A record queued against a transcript that has since been replaced
        // belongs to the OUTGOING session (already persisted there when the
        // switch settled): appending it here would attribute the command to a
        // session that never ran it. Re-read per record: a swap can land
        // between appends.
        for record in pending {
            if !self.session_storage_generation_matches(record.generation) {
                continue;
            }
            self.append_bash_record(
                &record.command,
                &record.details,
                record.exclude_from_context,
            )
            .await;
        }
    }
}

/// One bash-execution record deferred while a run is streaming (see
/// [`AgentSession::record_bash_result`]); flushed on settle.
pub(super) struct PendingBashRecord {
    /// The command line, as recorded in the framed user text.
    command: String,
    /// The upstream `details` payload
    /// (command/output/exitCode/cancelled/truncated/fullOutputPath).
    details: JsonValue,
    /// Whether the record stays in the transcript but out of the LLM context.
    exclude_from_context: bool,
    /// The storage generation captured when the record was queued.
    generation: u64,
}

/// Result of one out-of-loop bash execution (upstream `BashResult`, the RPC
/// `bash` response payload).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    /// Combined stdout + stderr output (possibly truncated).
    pub output: String,
    /// Process exit code; `None` when the process was killed by a signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the command was cancelled via `abort_bash` or an abort signal.
    pub cancelled: bool,
    /// Whether the output was truncated.
    pub truncated: bool,
    /// Path to the temp file holding the full output, when truncation spilled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}
/// RAII guard around one out-of-loop shell's process-tree kill.
///
/// The explicit abort branch inside [`run_shell_command`] already kills the
/// tree, but a caller can leave the wait loop without reaching it: the
/// `execute_bash` future is cancelled (an RPC request task dropped, a host's
/// `select!` losing its consumer), a chunk callback panics, or an early return
/// skips the kill. `kill_on_drop(true)` only reaches the shell itself — the
/// command's descendants (a forked `sleep`, a started server) would survive
/// the future's drop. The guard arms once at spawn, kills the whole tree from
/// `Drop` while it is still armed, and is disarmed once the run reaches its
/// own teardown (normal exit or explicit abort). Same lifecycle as the bash
/// tool's `TreeKillGuard`; that type is private to `coding_agent::tools`, so
/// this module wraps the crate-visible `ProcessTreeKill` seam instead of
/// widening another module's surface.
///
/// The kill is synchronous by necessity (see [`ProcessTreeKill::terminate`]):
/// the MSYS sweep needs the root's process-table entry, so it must complete
/// before the root is reaped. Drop therefore blocks the dropping thread for
/// the same 150-300 ms the abort path already accepts, and only on the
/// abnormal path.
struct ShellTreeKill {
    /// `Some((kill handle, root pid))` while the tree may still be running.
    armed: Option<(ProcessTreeKill, u32)>,
}

impl ShellTreeKill {
    /// Arm for a freshly spawned child. The pid is captured here because
    /// `Child::id` returns `None` once the child has been reaped.
    fn arm(child: &tokio::process::Child) -> Self {
        let pid = child.id();
        Self {
            armed: pid.map(|pid| (ProcessTreeKill::attach(child), pid)),
        }
    }

    /// Kill the tree now and disarm (the explicit abort path).
    fn terminate(&mut self) {
        if let Some((kill, pid)) = self.armed.take() {
            kill.terminate(pid);
        }
    }

    /// The child reached its own teardown: intentionally backgrounded
    /// descendants keep running (upstream semantics), so the guard must not
    /// fire at scope exit.
    fn disarm(&mut self) {
        self.armed = None;
    }
}

impl Drop for ShellTreeKill {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Run one shell command, capturing interleaved stdout/stderr into a bounded
/// rolling buffer (the same capture/spill policy as the `bash` tool). The
/// optional abort signal cancels the process, mirroring `abort_bash`; the
/// wait races the signal (await-able) against the pipe reads, so `abort_bash`
/// kills the child immediately instead of at the next poll tick. `on_chunk`
/// (upstream `executeBash`'s `onChunk`, agent-session.ts:3858-3862) receives
/// every decoded read chunk as it arrives.
///
/// The spawn uses the shared shell-spawn seam
/// (`coding_agent::tools::shell_pool::{shell_spawn_target, ProcessTreeKill}`),
/// exactly like the bash tool's per-call arm: the resolved program may be a
/// Git-for-Windows launcher shim rather than the real MSYS bash, and a shim
/// shell is not an MSYS process — killing it would leave the command's own
/// processes (`sleep 30 &` style descendants) running. The abort therefore
/// kills the whole TREE (`ProcessTreeKill::terminate`, which runs the MSYS
/// subtree sweep, then `taskkill /T`, then the job) instead of only the root.
async fn run_shell_command(
    command: &str,
    cwd: &Path,
    agent_dir: &Path,
    abort: Option<AbortSignal>,
    on_chunk: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> BashResult {
    if !cwd.is_dir() {
        return BashResult {
            output: format!("Working directory '{}' does not exist", cwd.display()),
            exit_code: None,
            cancelled: false,
            truncated: false,
            full_output_path: None,
        };
    }
    // Shell resolution + `shellCommandPrefix` (upstream `executeBash`,
    // agent-session.ts:3871-3900: `createLocalBashOperations({ shellPath })`
    // over the settings `getShellPath()` / `getShellCommandPrefix()`). A bare
    // `bash` is not equivalent: on Windows it resolves through PATH, which can
    // land on the `System32\bash.exe` WSL launcher when the PATH order shifts.
    // This is the same resolution the bash tool and the TUI `!` path use, but
    // scoped to THIS session's agent directory (D1): a mid-process
    // `PIR_AGENT_DIR` change must not re-point a live session's settings.
    let prefs = crate::config::settings::shell_prefs_in(agent_dir);
    let command =
        crate::coding_agent::tools::apply_command_prefix(prefs.command_prefix.as_deref(), command);
    let shell = match crate::coding_agent::tools::resolve_shell_config(prefs.shell_path.as_deref())
    {
        Ok(shell) => shell,
        Err(error) => {
            return BashResult {
                output: error,
                exit_code: None,
                cancelled: false,
                truncated: false,
                full_output_path: None,
            }
        }
    };
    // Spawn target: the resolved shell, or the real MSYS bash when the
    // resolved one is a Git-for-Windows launcher shim (with that spawn's
    // `PATH` fixup). A shim shell is not an MSYS process, so the abort tree
    // kill below could not reach the command's own processes.
    let (spawn_program, spawn_path) = shell_spawn_target(shell.program());
    let mut spawn = tokio::process::Command::new(&spawn_program);
    if let Some(path) = spawn_path {
        spawn.env("PATH", path);
    }
    spawn.args(shell.args());
    if shell.stdin_transport() {
        // Legacy WSL bash takes the command on stdin (`-s`).
        spawn.stdin(std::process::Stdio::piped());
    } else {
        spawn.arg(&command);
        // Upstream `stdio: [commandFromStdin ? "pipe" : "ignore", ...]`
        // (tools/bash.ts:116): the shell's stdin must NOT be inherited. A
        // host whose stdin is a held-open pipe (RPC mode reads commands from
        // it) otherwise makes Git Bash block during startup until that pipe
        // closes — the command only runs after the RPC client disconnects.
        spawn.stdin(std::process::Stdio::null());
    }
    spawn
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // The tree kill relies on the child leading its own process group on Unix
    // (`killpg`); on Windows it arms the job/sweep machinery at termination
    // time, so attaching here is enough for both.
    apply_tree_kill_spawn_attributes(&mut spawn);
    let mut child = match spawn.spawn() {
        Ok(child) => child,
        Err(error) => {
            return BashResult {
                output: format!("Command failed: {error}"),
                exit_code: None,
                cancelled: false,
                truncated: false,
                full_output_path: None,
            }
        }
    };
    // Tree kill: a Job Object on Windows (fallback taskkill /T /F), the
    // child's own process group on Unix (upstream `killProcessTree` fires on
    // abort/timeout). The RAII guard covers the paths that leave this future
    // without reaching the explicit abort kill below (caller cancellation, a
    // panicking chunk callback, an early return).
    let mut tree_kill = ShellTreeKill::arm(&child);
    if shell.stdin_transport() {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(command.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }
    // Bounded rolling capture: memory stays at the keep window no matter how
    // chatty the command is; an evicted prefix streams to the spill temp file
    // and `full_output_path` points at the complete output once truncation
    // applies (the coding-agent bash tool's capture/spill contract).
    let mut capture = BashOutputCapture::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut stderr = child.stderr.take();
        let mut stdout_buf = [0u8; 8192];
        let mut stderr_buf = [0u8; 8192];
        let mut stdout_done = false;
        let mut stderr_done = stderr.is_none();
        let mut cancelled = false;
        // Resolves only when the abort signal fires (never with no signal);
        // borrowed by every select iteration so the signal stays alive.
        let mut abort_wait = std::pin::pin!(async {
            if let Some(signal) = abort.as_ref() {
                signal.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        });
        loop {
            if stdout_done && stderr_done {
                break;
            }
            tokio::select! {
                read = read_into(&mut stdout, &mut stdout_buf), if !stdout_done => {
                    match read {
                        Ok(0) | Err(_) => stdout_done = true,
                        Ok(count) => {
                            let chunk = String::from_utf8_lossy(&stdout_buf[..count]);
                            if let Some(on_chunk) = on_chunk {
                                on_chunk(&chunk);
                            }
                            capture.append(chunk.as_bytes());
                        }
                    }
                }
                read = read_into_optional(stderr.as_mut(), &mut stderr_buf), if !stderr_done => {
                    match read {
                        Ok(0) | Err(_) => stderr_done = true,
                        Ok(count) => {
                            let chunk = String::from_utf8_lossy(&stderr_buf[..count]);
                            if let Some(on_chunk) = on_chunk {
                                on_chunk(&chunk);
                            }
                            capture.append(chunk.as_bytes());
                        }
                    }
                }
                // `if !cancelled`: once the abort future has completed it must
                // never be polled again (select! would resume a finished async
                // block and panic with "`async fn` resumed after completion").
                _ = &mut abort_wait, if !cancelled => {
                    cancelled = true;
                    // Kill the whole tree, not just the shell: the command may
                    // have forked descendants (`sleep 30 &`) that a bare
                    // `start_kill` leaves running (measured: the survivor
                    // shape was exactly the forked descendant). `start_kill`
                    // still runs afterwards, covering the root when the tree
                    // kill found nothing.
                    tree_kill.terminate();
                    let _ = child.start_kill();
                }
            }
        }
        let status = child.wait().await.ok();
        let (output, truncated, full_output_path) = capture.finish();
        // The command reached its own teardown: backgrounded descendants keep
        // running (upstream kills only on abort/timeout), so the guard must
        // not fire at scope exit.
        tree_kill.disarm();
        return BashResult {
            output,
            exit_code: if cancelled {
                None
            } else {
                status.and_then(|status| status.code())
            },
            cancelled,
            truncated,
            full_output_path,
        };
    }
    let status = child.wait().await.ok();
    let (output, truncated, full_output_path) = capture.finish();
    tree_kill.disarm();
    BashResult {
        output,
        exit_code: status.and_then(|status| status.code()),
        cancelled: false,
        truncated,
        full_output_path,
    }
}

/// RAII registration of one in-flight bash command's abort signal (A12).
///
/// [`AgentSession::execute_bash_with_id`] pushes `(seq, signal)` into the
/// session's abort set before awaiting [`run_shell_command`]. Owning the
/// registration through this guard removes that entry in `Drop`, so a future
/// cancelled at any await point (dropped RPC request task, session teardown)
/// cannot leave a stale `(seq, AbortSignal)` behind for the session's
/// lifetime. Taking the registry mutex here also wakes an `abort_bash`
/// caller blocked on it.
struct BashAbortRegistration<'a> {
    session: &'a AgentSession,
    seq: u64,
}

impl Drop for BashAbortRegistration<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .session
            .bash_abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|(seq, _)| *seq != self.seq);
    }
}

/// In-memory window kept while a bash command streams: 2× the truncation cap,
/// matching the coding-agent bash tool's `ShellCapture` window (the buffer is
/// only trimmed once it doubles, so a steady stream moves each retained byte
/// O(1) times instead of memmoving the window on every chunk).
const BASH_OUTPUT_KEEP_BYTES: usize = 2 * DEFAULT_MAX_BYTES;

/// Process-unique sequence for spill temp-file names: combined with a
/// fixed-length random suffix it keeps concurrent runs from colliding and
/// makes pre-planted names unguessable in the shared temp directory.
static BASH_SPILL_FILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Create a spill temp file under a fresh name
/// `pir-shell-{pid}-{seq}-{uuid-v4}`, opened exclusively (`create_new`, the
/// Rust O_EXCL) so the call fails rather than following a symlink or
/// overwriting a pre-planted file in the shared temp directory. On Unix the
/// file is created with mode 0600 (owner-only, matching the coding-agent bash
/// tool); `None` when creation fails.
fn create_bash_spill_file() -> Option<(std::fs::File, std::path::PathBuf)> {
    let path = std::env::temp_dir().join(format!(
        "pir-shell-{}-{}-{}",
        std::process::id(),
        BASH_SPILL_FILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // The mode applies at creation, so the spill never has a window with
    // 0644 permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).ok()?;
    Some((file, path))
}

/// Bounded rolling capture for [`run_shell_command`]'s interleaved
/// stdout/stderr, mirroring the coding-agent bash tool's `ShellCapture`
/// (whose module is private to `coding_agent::tools`): the in-memory tail is
/// capped at [`BASH_OUTPUT_KEEP_BYTES`], every evicted prefix is written to a
/// 0600 spill temp file as the command streams, and a truncated result points
/// `full_output_path` at the complete stream.
struct BashOutputCapture {
    /// Retained tail of the decoded stream (bounded by the keep window).
    tail: Vec<u8>,
    /// Spill file holding every evicted prefix (and, after
    /// [`Self::spill_if_needed`], the retained tail as well).
    spill: Option<(std::fs::File, std::path::PathBuf)>,
    /// Latched spill failure: the file can no longer be the complete stream
    /// and must not be advertised as `full_output_path` (advertising a
    /// truncated file would point the caller at silent output loss).
    spill_failed: bool,
    /// The retained `tail` was appended to the spill file, so the file now
    /// holds the complete stream; a later eviction must not re-write the
    /// buffered head.
    tail_flushed: bool,
}

impl BashOutputCapture {
    fn new() -> Self {
        Self {
            tail: Vec::new(),
            spill: None,
            spill_failed: false,
            tail_flushed: false,
        }
    }

    /// Append one decoded read chunk, trimming the window back to the keep
    /// size once it doubled and streaming the evicted prefix to the spill
    /// file.
    fn append(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() <= BASH_OUTPUT_KEEP_BYTES.saturating_mul(2) {
            return;
        }
        let mut excess = self.tail.len() - BASH_OUTPUT_KEEP_BYTES;
        // Keep the retained tail starting at a UTF-8 character boundary: the
        // chunks are decoded text, so the cut would otherwise split a
        // multi-byte character and leave a leading replacement char. A UTF-8
        // character has at most three continuation bytes, so the cut moves
        // back at most three bytes.
        let mut backed = 0;
        while excess < self.tail.len() && backed < 3 && (self.tail[excess] & 0xC0) == 0x80 {
            excess -= 1;
            backed += 1;
        }
        if excess == 0 {
            return;
        }
        if self.tail_flushed {
            // Defensive: the buffered head already went into the file with
            // the tail flush, so writing it again would duplicate it.
            self.spill_failed = true;
        } else {
            // Latch spill failures: a failed creation is never retried per
            // chunk, and a failed evicted-prefix write (disk full, the file
            // vanishing) marks the spill file incomplete so the result stops
            // advertising it as the full output.
            if self.spill.is_none() && !self.spill_failed {
                match create_bash_spill_file() {
                    Some(spill) => self.spill = Some(spill),
                    None => self.spill_failed = true,
                }
            }
            if let Some((file, _)) = self.spill.as_mut() {
                if std::io::Write::write_all(file, &self.tail[..excess]).is_err() {
                    self.spill_failed = true;
                }
            }
        }
        self.tail.drain(..excess);
    }

    fn tail_string(&self) -> String {
        String::from_utf8_lossy(&self.tail).into_owned()
    }

    /// Ensure the spill file holds the COMPLETE decoded stream, then return
    /// its path (idempotent). With no eviction yet, `full_text` — the whole
    /// captured output — is written; after an eviction the retained tail is
    /// appended exactly once. A latched spill failure (or a fresh failure
    /// here) yields `None`.
    fn spill_if_needed(&mut self, full_text: &str) -> Option<String> {
        if self.spill_failed {
            return None;
        }
        if self.spill.is_none() {
            let Some((mut file, path)) = create_bash_spill_file() else {
                self.spill_failed = true;
                return None;
            };
            if std::io::Write::write_all(&mut file, full_text.as_bytes()).is_err() {
                self.spill_failed = true;
                return None;
            }
            self.spill = Some((file, path));
            self.tail_flushed = true;
        } else if !self.tail_flushed {
            if let Some((file, _)) = self.spill.as_mut() {
                if std::io::Write::write_all(file, &self.tail).is_err() {
                    self.spill_failed = true;
                    return None;
                }
            }
            self.tail_flushed = true;
        }
        self.spill
            .as_ref()
            .map(|(_, path)| path.to_string_lossy().into_owned())
    }

    /// Final `(output, truncated, full_output_path)` triple with the same
    /// tail-truncation semantics `truncate_tail` previously applied to the
    /// whole buffer: the window always contains the final `2 × max_bytes`
    /// bytes, so the retained content is byte-identical, and an evicted
    /// prefix always implies truncation.
    fn finish(&mut self) -> (String, bool, Option<String>) {
        let text = self.tail_string();
        let truncation = truncate_tail(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let full_output_path = if truncation.truncated {
            self.spill_if_needed(&text)
        } else {
            None
        };
        (truncation.content, truncation.truncated, full_output_path)
    }
}

/// Read one chunk from a process pipe.
async fn read_into<R>(reader: &mut R, buffer: &mut [u8; 8192]) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    reader.read(buffer).await
}

/// [`read_into`] for an optional pipe (`Ok(0)` when absent).
async fn read_into_optional<R>(
    reader: Option<&mut R>,
    buffer: &mut [u8; 8192],
) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match reader {
        Some(reader) => read_into(reader, buffer).await,
        None => Ok(0),
    }
}
