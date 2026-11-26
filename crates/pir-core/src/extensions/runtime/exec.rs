use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// Result shape of `pi.exec` (upstream `ExecResult`, `exec.ts:32-40`):
/// `code` is a plain number — `code ?? 0` (`exec.ts:137`) — so it is `i32`,
/// not `Option`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ExecResultJson {
    stdout: String,
    stderr: String,
    code: i32,
    killed: bool,
    /// Output exceeded `maxBuffer`; `stdout`/`stderr` are incomplete and
    /// `code` is nonzero (upstream `ExecResult.truncated`, `exec.ts:38-40`).
    truncated: bool,
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

/// Run one `pi.exec` call. Invalid options (an out-of-range `maxBuffer`)
/// reject before spawning, mirroring upstream's `RangeError`
/// (`exec.ts:53-57`).
pub(super) async fn run_exec(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_ms: Option<f64>,
    max_buffer: Option<f64>,
    abort: Option<ExecAbort>,
) -> Result<ExecResultJson, String> {
    let max_buffer_bytes = resolve_max_buffer(max_buffer)?;
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
        let _ = tx.send(exec_blocking(
            command,
            args,
            cwd,
            timeout_ms,
            max_buffer_bytes,
            Some(owned),
        ));
    });
    let result = match rx.await {
        Ok(result) => result,
        Err(_) => ExecResultJson {
            stdout: String::new(),
            stderr: "exec worker dropped".into(),
            code: 1,
            killed: false,
            truncated: false,
        },
    };
    drop(guard);
    Ok(result)
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

/// The runtime string limit upstream caps `maxBuffer` against
/// (`buffer.constants.MAX_STRING_LENGTH`, 64-bit V8: 2^29 − 24,
/// `exec.ts:10-13,53-57`).
const MAX_STRING_LENGTH_BYTES: usize = (1 << 29) - 24;

/// Default combined stdout/stderr cap (upstream `DEFAULT_EXEC_MAX_BUFFER`,
/// `exec.ts:10-11`: the smaller of 256 MiB and the string limit).
const DEFAULT_EXEC_MAX_BUFFER_BYTES: usize = 256 * 1024 * 1024;

/// Validate `options.maxBuffer` (upstream `exec.ts:53-57`): absent means the
/// default; an explicit cap must be a nonnegative integer up to the runtime
/// string limit. Everything else rejects before spawning.
fn resolve_max_buffer(max_buffer: Option<f64>) -> Result<usize, String> {
    match max_buffer {
        None => Ok(DEFAULT_EXEC_MAX_BUFFER_BYTES),
        Some(value)
            if value.is_finite()
                && value >= 0.0
                && value.fract() == 0.0
                && value <= MAX_STRING_LENGTH_BYTES as f64 =>
        {
            Ok(value as usize)
        }
        Some(_) => Err(format!(
            "maxBuffer must be an integer between 0 and {MAX_STRING_LENGTH_BYTES} bytes"
        )),
    }
}

/// Combined stdout/stderr byte budget shared by the two pipe readers
/// (upstream `takeWithinCap`, `exec.ts:90-102`). Whoever consumes the last
/// byte first flips `truncated`.
struct OutputBudget {
    used: AtomicUsize,
    max: usize,
    truncated: AtomicBool,
}

impl OutputBudget {
    fn new(max: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            max,
            truncated: AtomicBool::new(false),
        }
    }

    /// Reserve up to `len` bytes; returns how many were accepted.
    fn take(&self, len: usize) -> usize {
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            let room = self.max.saturating_sub(used);
            let accepted = len.min(room);
            match self.used.compare_exchange(
                used,
                used + accepted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    if accepted < len {
                        self.truncated.store(true, Ordering::SeqCst);
                    }
                    return accepted;
                }
                Err(current) => used = current,
            }
        }
    }

    fn is_truncated(&self) -> bool {
        self.truncated.load(Ordering::SeqCst)
    }
}

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
    budget: Arc<OutputBudget>,
    /// Timestamp of the last chunk; re-arms the post-exit quiet window.
    last_data: std::time::Instant,
}

impl StreamCapture {
    fn new(budget: Arc<OutputBudget>) -> Self {
        Self {
            kept: Vec::new(),
            budget,
            last_data: std::time::Instant::now(),
        }
    }

    /// Keep only bytes within the shared budget, flagging (never silently
    /// dropping) everything past the cap.
    fn push(&mut self, chunk: &[u8]) {
        let accepted = self.budget.take(chunk.len());
        self.kept.extend_from_slice(&chunk[..accepted]);
        self.last_data = std::time::Instant::now();
    }
}

/// Decode captured bytes as lossy UTF-8; an incomplete trailing sequence is
/// discarded when the output was truncated (upstream `exec.ts:121-125`).
fn bytes_into_text(bytes: Vec<u8>, truncated: bool) -> String {
    let bytes = if truncated {
        trim_incomplete_utf8_suffix(bytes)
    } else {
        bytes
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Drop a trailing incomplete UTF-8 sequence. Upstream discards the
/// `StringDecoder` suffix on truncation instead of emitting a replacement
/// character that was not in the accepted output (`exec.ts:121-125`).
fn trim_incomplete_utf8_suffix(mut bytes: Vec<u8>) -> Vec<u8> {
    let mut lead = bytes.len();
    let mut continuations = 0usize;
    while lead > 0 && continuations < 3 && bytes[lead - 1] & 0b1100_0000 == 0b1000_0000 {
        lead -= 1;
        continuations += 1;
    }
    let needed = lead
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .map_or(0, |byte| match byte {
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            _ => 0,
        });
    if lead > 0 && needed > continuations {
        bytes.truncate(lead - 1);
    }
    bytes
}

/// Read a pipe into `capture`, keeping `pi.exec` results within the shared
/// byte budget. Once the cap is reached the read end is closed and the
/// reader stops, mirroring upstream's `proc.stdout.destroy()` /
/// `proc.stderr.destroy()` on overflow (`exec.ts:97-99`).
fn read_capped(pipe: Option<impl std::io::Read>, capture: &Mutex<StreamCapture>) {
    let Some(mut pipe) = pipe else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut capture = capture.lock().unwrap_or_else(PoisonError::into_inner);
                capture.push(&chunk[..n]);
                let truncated = capture.budget.is_truncated();
                drop(capture);
                if truncated {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Snapshot and finalize a stream's capture; the lock is held only for the
/// take, so a detached reader may keep appending afterwards harmlessly.
fn capture_text(capture: &Mutex<StreamCapture>, truncated: bool) -> String {
    let kept = {
        let mut capture = capture.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut capture.kept)
    };
    bytes_into_text(kept, truncated)
}

fn exec_blocking(
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<f64>,
    max_buffer_bytes: usize,
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
                truncated: false,
            };
        }
    };

    let budget = Arc::new(OutputBudget::new(max_buffer_bytes));
    let stdout_capture = Arc::new(Mutex::new(StreamCapture::new(Arc::clone(&budget))));
    let stderr_capture = Arc::new(Mutex::new(StreamCapture::new(Arc::clone(&budget))));
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
                    // Output over `maxBuffer` terminates the command too
                    // (upstream `takeWithinCap` calls `killProcess`,
                    // `exec.ts:95-100`).
                    let over_budget = budget.is_truncated();
                    if aborted || timed_out || over_budget {
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
    let truncated = budget.is_truncated();
    // A truncated result reports `killed: true` even when the child happened
    // to exit before the main thread sent the term signal (upstream sets the
    // flag inside `takeWithinCap`'s kill path).
    if truncated {
        killed = true;
    }
    let stdout = capture_text(&stdout_capture, truncated);
    let stderr = capture_text(&stderr_capture, truncated);
    // `code ?? 0` for an exited child (signal death reports 0, upstream
    // `exec.ts:137`) and `1` for the error path (upstream catch,
    // `exec.ts:139-144`). A truncated result must never read as success.
    let code = match status {
        Some(status) => status.code().unwrap_or(0),
        None => 1,
    };
    let code = if truncated && code == 0 { 1 } else { code };
    ExecResultJson {
        stdout,
        stderr,
        code,
        killed,
        truncated,
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
