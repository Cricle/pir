use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// Result shape of `pi.exec` (upstream `ExecResult`, `exec.ts:23-28`):
/// `code` is a plain number — `code ?? 0` (`exec.ts:97`) — so it is `i32`,
/// not `Option`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ExecResultJson {
    stdout: String,
    stderr: String,
    code: i32,
    killed: bool,
}

/// Poll for the runtime's shared tool-call abort flag (`tool_call_aborted`),
/// kept as a closure so this module does not depend on the runtime state
/// shape.
pub(super) type SharedAbortPoll = Arc<dyn Fn() -> bool + Send + Sync + 'static>;

/// Per-call abort handle for one extension `pi.exec` call.
///
/// Upstream `exec.ts:52-72` (`killProcess`): the child is killed when the
/// caller-provided `AbortSignal` aborts — checked up-front via
/// `signal.aborted` (`exec.ts:67-68`) and otherwise through its `abort` event
/// (`exec.ts:70`) — or when the timeout elapses (`exec.ts:75-79`).
///
/// The JS `abort` event cannot reach the worker thread directly, so the
/// prelude relays it to the `execAbort` host fn, which calls
/// [`ExecAbort::trigger`]. When the call carried a signal, the runtime's
/// shared tool-call abort flag is polled too: `ctx.signal` is backed by that
/// flag, and host-initiated cancellation aborts it without firing JS-side
/// listeners.
#[derive(Clone)]
pub(super) struct ExecAbort {
    triggered: Arc<AtomicBool>,
    shared: Option<SharedAbortPoll>,
}

impl ExecAbort {
    pub(super) fn new(shared: Option<SharedAbortPoll>) -> Self {
        Self {
            triggered: Arc::new(AtomicBool::new(false)),
            shared,
        }
    }

    /// Flip the flag (JS signal `abort` event, or an already-aborted signal
    /// at call time — upstream kills in both cases, `exec.ts:66-71`).
    pub(super) fn trigger(&self) {
        self.triggered.store(true, Ordering::SeqCst);
    }

    fn is_aborted(&self) -> bool {
        self.triggered.load(Ordering::SeqCst) || self.shared.as_ref().is_some_and(|poll| poll())
    }
}

/// Grace period after the graceful term signal before the forced kill
/// (upstream `exec.ts:56-61` "Force kill after 5 seconds if SIGTERM doesn't
/// work").
const KILL_ESCALATION_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Longest block between two abort/timeout/exit probes while `pi.exec` waits.
///
/// `Child` has no wait-with-timeout, so the wait is a bounded probe loop; the
/// step grows from 1 ms to this bound (the "adaptive segmentation" budget:
/// never more than 1 s, here 50 ms) instead of a fixed 5 ms tick. A busy
/// command pays the short step, an idle long-running one wakes 20x/s instead
/// of 200x/s.
const CHILD_WAIT_PROBE_MAX: std::time::Duration = std::time::Duration::from_millis(50);

pub(super) async fn run_exec(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_ms: Option<f64>,
    abort: Option<ExecAbort>,
) -> ExecResultJson {
    let (tx, rx) = tokio::sync::oneshot::channel::<ExecResultJson>();
    let command = command.to_string();
    let args = args.to_vec();
    let cwd = cwd.map(String::from);
    // The owned handle doubles as the caller-cancellation trigger: the guard
    // is a local of THIS future, so a caller dropping it mid-await (task
    // abort / JS call-timeout teardown) triggers the abort the poller thread
    // polls, killing the child instead of leaking the poller thread and the
    // process tree until the command finishes naturally. On the normal path
    // the trigger fires after the poller already returned — a harmless flag
    // write.
    let owned = abort.unwrap_or_else(|| ExecAbort::new(None));
    let guard = ExecCancelGuard(Some(owned.clone()));
    std::thread::spawn(move || {
        let _ = tx.send(exec_blocking(command, args, cwd, timeout_ms, Some(owned)));
    });
    let result = match rx.await {
        Ok(result) => result,
        Err(_) => ExecResultJson {
            stdout: String::new(),
            stderr: "exec worker dropped".into(),
            code: 1,
            killed: false,
        },
    };
    drop(guard);
    result
}

/// Caller-cancellation link for one extension `pi.exec` call (see
/// [`run_exec`]): triggers the poller thread's abort handle when the
/// caller's future is dropped mid-await.
struct ExecCancelGuard(Option<ExecAbort>);

impl Drop for ExecCancelGuard {
    fn drop(&mut self) {
        if let Some(abort) = self.0.take() {
            abort.trigger();
        }
    }
}

/// Maximum bytes captured per stream of an extension `pi.exec` call.
const MAX_EXEC_OUTPUT_BYTES: usize = 1024 * 1024;

/// Pipe-quiet grace after the child exits (upstream `waitForChildProcess`,
/// `utils/child-process.ts:88-97`): once the child is gone, the pipe readers
/// must not be joined indefinitely — a detached descendant that inherited
/// the stdout/stderr handles and holds them open would hang `pi.exec`
/// forever, because the abort/timeout kill only reaches the direct child.
/// Upstream releases the readers after `EXIT_STDIO_GRACE_MS` (100 ms) of
/// quiet, re-armed on every chunk so a still-writing descendant is drained;
/// pir takes a slightly larger margin for its 5 ms thread-poll granularity.
const EXIT_STDIO_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

/// Bytes captured from one pipe, shared between its reader thread and the
/// main thread: when the grace expires while a reader is still blocked on a
/// descendant-held handle, the main thread snapshots what was read instead
/// of losing it with the detached thread's (unjoinable) return value.
struct StreamCapture {
    kept: Vec<u8>,
    truncated: bool,
    /// Timestamp of the last chunk; re-arms the post-exit quiet window.
    last_data: std::time::Instant,
}

impl Default for StreamCapture {
    fn default() -> Self {
        Self {
            kept: Vec::new(),
            truncated: false,
            last_data: std::time::Instant::now(),
        }
    }
}

impl StreamCapture {
    /// Keep at most [`MAX_EXEC_OUTPUT_BYTES`], flagging (never silently
    /// dropping) everything past the cap.
    fn push(&mut self, chunk: &[u8]) {
        let room = MAX_EXEC_OUTPUT_BYTES.saturating_sub(self.kept.len());
        if chunk.len() > room {
            self.kept.extend_from_slice(&chunk[..room]);
            self.truncated = true;
        } else {
            self.kept.extend_from_slice(chunk);
        }
        self.last_data = std::time::Instant::now();
    }

    /// The captured text (lossy UTF-8, truncation notice) — the same result
    /// shape the previously-joined reader returned.
    fn into_text(self) -> String {
        let mut text = String::from_utf8_lossy(&self.kept).into_owned();
        if self.truncated {
            text.push_str("\n[output truncated]");
        }
        text
    }
}

/// Read a pipe up to [`MAX_EXEC_OUTPUT_BYTES`] into `capture`, keeping
/// `pi.exec` results bounded for the host. Drains to EOF even past the cap:
/// a writer must never block on a full pipe because the reader stopped
/// early.
fn read_capped(pipe: Option<impl std::io::Read>, capture: &Mutex<StreamCapture>) {
    let Some(mut pipe) = pipe else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => capture
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(&chunk[..n]),
            Err(_) => break,
        }
    }
}

/// Snapshot and finalize a stream's capture; the lock is held only for the
/// take, so a detached reader may keep appending afterwards harmlessly.
fn capture_text(capture: &Mutex<StreamCapture>) -> String {
    std::mem::take(&mut *capture.lock().unwrap_or_else(PoisonError::into_inner)).into_text()
}

fn exec_blocking(
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<f64>,
    abort: Option<ExecAbort>,
) -> ExecResultJson {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(&command);
    cmd.args(&args)
        // Upstream `exec.ts:44` `stdio: ["ignore", "pipe", "pipe"]`: the
        // child must not inherit the host's stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ExecResultJson {
                stdout: String::new(),
                stderr: format!("failed to spawn `{command}`: {e}"),
                code: 1,
                killed: false,
            };
        }
    };

    let stdout_capture = Arc::new(Mutex::new(StreamCapture::default()));
    let stderr_capture = Arc::new(Mutex::new(StreamCapture::default()));
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn({
        let capture = Arc::clone(&stdout_capture);
        move || read_capped(stdout_pipe, &capture)
    });
    let stderr_thread = std::thread::spawn({
        let capture = Arc::clone(&stderr_capture);
        move || read_capped(stderr_pipe, &capture)
    });

    // Upstream ignores falsy / non-positive timeouts (`exec.ts:75`).
    let deadline = timeout_ms
        .filter(|ms| *ms > 0.0)
        // `Instant + Duration` panics on overflow; the clamp keeps absurd JS
        // timeouts from aborting the host.
        .map(|ms| Instant::now() + Duration::from_millis(ms.min(u32::MAX as f64) as u64));
    let mut killed = false;
    // Set when the graceful signal went out; drives the forced-kill
    // escalation (`exec.ts:56-61`).
    let mut term_at: Option<Instant> = None;
    // Adaptive probe step: 1 ms while the child is likely short-lived or the
    // kill escalation is pending, doubling to `CHILD_WAIT_PROBE_MAX` for an
    // idle long-running command.
    let mut probe = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                let now = Instant::now();
                if !killed {
                    // Upstream `killProcess` (`exec.ts:52-63`): the abort
                    // signal and the timeout share one kill path and the
                    // `killed` flag flips on the first signal.
                    let aborted = abort.as_ref().is_some_and(|handle| handle.is_aborted());
                    let timed_out = deadline.is_some_and(|d| now >= d);
                    if aborted || timed_out {
                        killed = true;
                        send_term(&mut child);
                        term_at = Some(now);
                        probe = Duration::from_millis(1);
                    }
                } else if term_at.is_some_and(|at| now.duration_since(at) >= KILL_ESCALATION_GRACE)
                {
                    // The child ignored SIGTERM: force it (`exec.ts:57-60`).
                    let _ = child.kill();
                    term_at = None;
                    probe = Duration::from_millis(1);
                }
                std::thread::sleep(probe);
                probe = (probe * 2).min(CHILD_WAIT_PROBE_MAX);
            }
            Err(_) => break None,
        }
    };

    // Upstream `waitForChildProcess` (`child-process.ts:38-48,88-97`): after
    // the child exits, release the pipe readers once the streams fall quiet
    // — the window re-arms on every chunk, so a detached descendant still
    // writing keeps us reading (no truncated tail), while a descendant that
    // merely holds the inherited handles open can no longer hang the call.
    // A reader still blocked when the window closes is detached (it owns
    // only its pipe handle, closed at EOF, and the shared capture — nothing
    // critical leaks); the bytes it read are snapshotted below.
    let exit_at = Instant::now();
    let mut probe = Duration::from_millis(1);
    loop {
        if stdout_thread.is_finished() && stderr_thread.is_finished() {
            break;
        }
        let last_data = [&stdout_capture, &stderr_capture]
            .into_iter()
            .map(|capture| {
                capture
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .last_data
            })
            .max()
            .unwrap_or(exit_at);
        let elapsed = Instant::now().duration_since(exit_at.max(last_data));
        if elapsed >= EXIT_STDIO_GRACE {
            break;
        }
        std::thread::sleep(probe.min(EXIT_STDIO_GRACE - elapsed));
        probe = (probe * 2).min(CHILD_WAIT_PROBE_MAX);
    }
    let stdout = capture_text(&stdout_capture);
    let stderr = capture_text(&stderr_capture);
    // `code ?? 0` for an exited child (signal death reports 0, upstream
    // `exec.ts:97`) and `1` for the error path (upstream catch,
    // `exec.ts:99-104`).
    let code = match status {
        Some(status) => status.code().unwrap_or(0),
        None => 1,
    };
    ExecResultJson {
        stdout,
        stderr,
        code,
        killed,
    }
}

/// Send the graceful termination signal first (upstream `killProcess`'s
/// `proc.kill("SIGTERM")`, `exec.ts:55`). Windows has no graceful signal, so
/// `Child::kill` (TerminateProcess) is the closest term step.
fn send_term(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The pid came from `Child::id()` and the process is ours to signal;
        // SIGTERM is the graceful step upstream's killProcess relies on.
        // SAFETY: `kill` takes no pointers and only signals the pid of a child
        // this process spawned.
        #[expect(unsafe_code, reason = "libc::kill on a child we spawned")]
        let _ = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}
