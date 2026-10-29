//! Shared embedded-RustPython worker (used by the `python` tool and the
//! `context-mode` builtin).
//!
//! One [`PythonSession`] owns at most one worker thread and one
//! RustPython `Interpreter` (the VM is `!Send`, so it lives and dies on the
//! thread that created it). The session is created lazily: constructing it
//! allocates only the channel/atomic bookkeeping, and the worker thread (plus
//! the frozen-stdlib interpreter) starts on the first [`PythonSession::exec`].
//!
//! ## Session policies
//!
//! - [`SessionPolicy::Persistent`] — the `python` tool's semantics: one scope
//!   survives across jobs, so definitions made in one call are visible in the
//!   next. A deadline that lands on the running job resets the worker (fresh
//!   VM, empty namespace).
//! - [`SessionPolicy::Fresh`] — `context-mode`'s semantics: every job runs in
//!   a brand-new scope with `__builtins__`, so no name from an earlier call
//!   leaks into the next. The interpreter (and its module cache) is reused, so
//!   imports stay warm; only the namespace is fresh.
//!
//! ## Bounded execution
//!
//! A host-side watchdog turns the deadline into a KeyboardInterrupt-style
//! interrupt delivered through RustPython's user-signal channel; the eval loop
//! polls it (`vm.check_signals()` per bytecode), so runaway loops stop
//! cooperatively.
//!
//! Captured stdout and stderr are written through a capping sink: `write`
//! appends at most [`MAX_OUTPUT_CHARS`] chars and drops the rest, so a loop
//! that prints millions of small chunks holds no more than the cap in memory.
//! The eval-mode repr is still materialized before being capped. A single
//! huge value (e.g. `print("x" * 500_000_000)`) is allocated by the Python
//! expression itself *before* `write` sees it — the cap bounds retained
//! captured output, not peak allocation of user-created objects.
//!
//! ## Interrupt ownership
//!
//! A deadline that fires while the call is still QUEUED behind another job only
//! abandons that call (the worker skips a `Cancelled` job), leaving the job
//! that owns the eval loop untouched. A deadline on the RUNNING job delivers
//! the interrupt and drops the worker handle; the next call spawns a fresh VM.
//!
//! ## Process-wide execution lock (known limitation)
//!
//! [`PY_VM_EXEC_LOCK`] is necessarily process-global (RustPython's user-signal
//! trigger flag is process-global), and it is held for the whole job. An
//! interrupt only reaches code that polls `check_signals`; a job wedged inside
//! non-cooperative native code (e.g. catastrophic `re` backtracking) never
//! observes its deadline and keeps the lock until it returns or the process
//! exits. There is no safe way to kill that thread. The worker therefore does
//! not block forever acquiring the lock: it parks on a condition variable
//! that the holder signals when it releases (so a queued job starts at
//! handoff, not on a poll tick), wakes at most every
//! [`EXEC_LOCK_CHECK_SLICE`] to notice its own cancellation, and refuses to
//! start a job at all once the holder has exceeded
//! [`EXEC_LOCK_WEDGED_AFTER`] (2x the hard deadline ceiling). The wedged
//! thread itself remains parked (a bounded per-process leak).
//!
//! ## Labels and shutdown
//!
//! Each session carries its consumer's name (`python tool`, `ctx_execute`) and
//! uses it in every session-level error/interrupt message, so a context-mode
//! failure never claims to be the python tool. [`PythonSession::shutdown`]
//! (plugin `on_shutdown`) delivers the same interrupt, queues an explicit
//! teardown message, and waits at most [`SHUTDOWN_GRACE`] for the worker
//! thread to exit; it is idempotent and a no-op when no worker ever started.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex as StdMutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use rpy::AsObject as _;
use rustpython_vm as rpy;
use rustpython_vm::function::FuncArgs;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// Default per-call deadline (30s; a caller may raise it up to
/// [`MAX_TIMEOUT_MS`]).
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Hard ceiling for a call's deadline (120s) — a model-supplied value cannot
/// raise it beyond this.
pub const MAX_TIMEOUT_MS: u64 = 120_000;

/// Per-call output cap: captured stdout and stderr each retain at most this
/// many chars (1 MiB; enforced at write time) plus a trailing notice. The
/// eval-mode repr is capped to the same length after it is produced.
pub const MAX_OUTPUT_CHARS: usize = 1024 * 1024;

const TRUNCATION_NOTICE: &str = "\n... [output truncated at 1 MiB] ...";

/// Appended to timeout errors: the actionable remediation (batch/vectorize or
/// raise the deadline deliberately). The deadline itself is never extended
/// silently, and the error always names the budget that elapsed.
const TIMEOUT_GUIDANCE: &str = "reduce the work per call (batch the input or vectorize tight loops with comprehensions/built-ins) or raise timeout_ms";

/// Upper bound on [`PythonSession::shutdown`]: an interrupt lands in the
/// running eval loop within milliseconds, and 900 ms keeps a non-cooperative
/// script from stretching teardown anywhere near the call deadline (while
/// staying comfortably under the one-second teardown contract).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(900);

const SOURCE_PATH: &str = "<python-tool>";

// ---------------------------------------------------------------------------
// Worker: the persistent VM thread
// ---------------------------------------------------------------------------

/// One completed execution as the worker reports it back.
pub(crate) struct ExecOutcome {
    pub stdout: String,
    pub stderr: String,
    /// `Some` when the run raised an uncaught exception (traceback is in
    /// `stderr`; `print_exception` writes through the captured stream).
    pub error: bool,
    /// The `eval`-mode result's repr when the job ran in eval-first mode and
    /// `code` was a single expression.
    pub eval_repr: Option<String>,
    pub duration_ms: u128,
}

/// A value injected into a job's Python namespace before the code runs.
#[cfg(feature = "context-mode")]
pub(crate) enum JobGlobal {
    /// A Python `str`, created from the shared bytes inside the worker.
    Text(Arc<str>),
}

/// One execution request for the worker thread. Typed end to end: no JSON
/// round-trip and no clones of the (possibly large) injected content beyond
/// the `Arc` bump.
pub(crate) struct PythonJob {
    pub code: String,
    pub cwd: Option<String>,
    /// Eval-first compilation with REPL-style repr echo. The `context-mode`
    /// executor turns this off so only explicit stdout is returned.
    pub eval_first: bool,
    #[cfg(feature = "context-mode")]
    pub globals: Vec<(String, JobGlobal)>,
}

impl PythonJob {
    /// A plain `python`-tool job: eval-first echo, no injected globals (the
    /// session policy decides whether it sees the persistent namespace).
    pub fn for_python_tool(code: String, cwd: Option<String>) -> Self {
        Self {
            code,
            cwd,
            eval_first: true,
            #[cfg(feature = "context-mode")]
            globals: Vec::new(),
        }
    }
}

/// How many interpreter namespaces one session keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPolicy {
    Persistent,
    Fresh,
}

/// One execution request handed to the worker thread.
struct ExecJob {
    job: PythonJob,
    reply: tokio::sync::oneshot::Sender<ExecOutcome>,
    /// Queued -> Running -> Done, or Queued -> Cancelled when the call's own
    /// deadline expires before the worker starts it. The CAS decides whether
    /// a deadline may interrupt the VM: only the job that won Queued ->
    /// Running owns the eval loop.
    state: Arc<AtomicU8>,
}

const JOB_QUEUED: u8 = 0;
const JOB_RUNNING: u8 = 1;
const JOB_CANCELLED: u8 = 2;
const JOB_DONE: u8 = 3;

/// One message for the worker thread.
///
/// `Shutdown` is the teardown wake-up: taking the session's handle and simply
/// dropping it would not stop an idle worker while any in-flight call still
/// holds a sender clone, and an infinite loop only notices the closed channel
/// after it is interrupted. The explicit message breaks the loop deterministically
/// once the interrupted job has reported.
enum WorkerMessage {
    Job(ExecJob),
    /// Teardown acknowledgement; sent before the interpreter (and its frozen
    /// stdlib) is dropped so the host's bounded wait covers the loop exit.
    Shutdown(tokio::sync::oneshot::Sender<()>),
}

/// Handles onto the live worker thread. Cloned per call; dropping every clone
/// closes the job channel, so the worker exits after its current job.
#[derive(Clone)]
struct WorkerHandle {
    job_tx: std_mpsc::Sender<WorkerMessage>,
    /// Host-side interrupt channel (RustPython `UserSignalSender`): sending a
    /// closure that returns `Err` aborts the running eval loop at its next
    /// signal check. `Sender` is `!Sync`; the mutex makes the shared handle
    /// usable from any caller.
    interrupt_tx: Arc<StdMutex<Option<rpy::signal::UserSignalSender>>>,
    /// Latched when the session is reset: queued jobs must not run against
    /// the abandoned VM (they report the reset instead).
    abandoned: Arc<AtomicBool>,
    /// State cell of the job that currently owns the eval loop (`None`
    /// between jobs). [`deliver_interrupt`] watches it to stop retrying as
    /// soon as the interrupted job reports done.
    running_state: Arc<StdMutex<Option<Arc<AtomicU8>>>>,
    /// The worker thread itself, shared so [`PythonSession::shutdown`] can
    /// reap it without holding a clone in every in-flight call. `None` when
    /// the thread could not be spawned.
    join: Arc<StdMutex<Option<std::thread::JoinHandle<()>>>>,
}

/// An embedded-interpreter session: lazily spawns the VM worker on first use
/// and can reset it (fresh VM, fresh globals) after a timeout or abort.
pub struct PythonSession {
    worker: StdMutex<Option<WorkerHandle>>,
    policy: SessionPolicy,
    /// Product name used in every session-level error/interrupt message, so a
    /// `context-mode` failure names `ctx_execute` instead of claiming to be
    /// the `python` tool (and vice versa).
    label: &'static str,
    /// Number of worker threads this session actually spawned. A test seam
    /// (and diagnostics): construction and non-Python tools leave it at 0.
    spawns: AtomicU64,
}

impl Default for PythonSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PythonSession {
    /// A session with no worker yet; the first `exec` spawns one and keeps
    /// its globals across calls.
    pub fn new() -> Self {
        Self::with_policy(SessionPolicy::Persistent, "python tool")
    }

    /// A session whose jobs each run in a fresh namespace (the interpreter
    /// and its module cache are still reused).
    pub fn fresh() -> Self {
        Self::with_policy(SessionPolicy::Fresh, "ctx_execute")
    }

    fn with_policy(policy: SessionPolicy, label: &'static str) -> Self {
        Self {
            worker: StdMutex::new(None),
            policy,
            label,
            spawns: AtomicU64::new(0),
        }
    }

    /// Whether a worker thread/VM currently exists. Hidden test seam for the
    /// laziness contract: a freshly constructed plugin must report `false`,
    /// and only an execute call may flip it to `true`.
    pub fn has_worker(&self) -> bool {
        match self.worker.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// How many worker threads this session has spawned over its lifetime.
    pub fn spawn_count(&self) -> u64 {
        self.spawns.load(Ordering::Acquire)
    }

    fn policy_noun(&self) -> &'static str {
        match self.policy {
            SessionPolicy::Persistent => "session",
            SessionPolicy::Fresh => "interpreter",
        }
    }

    /// Run `job` against the worker with the given deadline.
    pub(crate) async fn exec(&self, job: PythonJob, timeout: Duration) -> ExecOutcome {
        // Snapshot (or spawn) the current worker under a short lock —the
        // handle clones are cheap and the lock is never held across `.await`.
        let handle = {
            let mut guard = match self.worker.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if guard.is_none() {
                *guard = Some(spawn_worker(self.policy, self.label));
                self.spawns.fetch_add(1, Ordering::AcqRel);
            }
            match guard.as_ref() {
                Some(worker) => worker.clone(),
                None => {
                    return failed_outcome(&format!(
                        "{}: failed to start the interpreter session",
                        self.label
                    ))
                }
            }
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<ExecOutcome>();
        // This call's ownership token for the VM: the worker moves it to
        // Running only when it actually starts THIS job, and the deadline arm
        // below is allowed to interrupt the VM only after winning that same
        // transition.
        let state = Arc::new(AtomicU8::new(JOB_QUEUED));
        if handle
            .job_tx
            .send(WorkerMessage::Job(ExecJob {
                job,
                reply: reply_tx,
                state: Arc::clone(&state),
            }))
            .is_err()
        {
            // The worker exited under us; reset so the next call gets a
            // fresh one.
            self.reset_worker();
            return failed_outcome(&format!(
                "{}: the interpreter session stopped unexpectedly",
                self.label
            ));
        }

        let noun = self.policy_noun();
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_recv_dropped)) => {
                // The worker died mid-job.
                self.reset_worker();
                failed_outcome(&format!(
                    "{}: the interpreter session stopped unexpectedly",
                    self.label
                ))
            }
            Err(_elapsed) => {
                // Deadline hit. The interrupt goes through the session's
                // SHARED user-signal channel, so it may only be delivered for
                // the job the VM is actually running: a call still queued
                // behind another job must instead abandon itself (the worker
                // skips a Cancelled job), leaving the running job and its
                // globals untouched.
                match state.compare_exchange(
                    JOB_QUEUED,
                    JOB_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // The worker may be parked behind another vm's job:
                        // wake it so it drops this cancelled job at handoff
                        // instead of at the next check slice. Best effort (a
                        // release that lands before the park is covered by
                        // the slice), and `notify_all` never blocks.
                        PY_VM_EXEC_RELEASED.notify_all();
                        failed_outcome(&format!(
                            "{}: execution timed out after {:.1}s while queued behind another call; this call did not run (the {noun} was left intact); {}",
                            self.label,
                            timeout.as_secs_f64(),
                            TIMEOUT_GUIDANCE
                        ))
                    }
                    Err(JOB_RUNNING) => {
                        // This call owns the eval loop: interrupt it, then
                        // reset the session (documented: globals are lost
                        // after a timeout). Mark the worker abandoned BEFORE
                        // the bounded interrupt delivery, so a job queued
                        // behind this one is refused instead of running
                        // against the doomed VM.
                        handle.abandoned.store(true, Ordering::Release);
                        deliver_interrupt(
                            &handle,
                            &format!(
                                "{}: execution timed out (the deadline was enforced by pir)",
                                self.label
                            ),
                        );
                        self.reset_worker();
                        match self.policy {
                            SessionPolicy::Persistent => failed_outcome(&format!(
                                "{}: execution timed out after {:.1}s (the call's budget); the persistent session was reset (variables defined this session were lost); {}",
                                self.label,
                                timeout.as_secs_f64(),
                                TIMEOUT_GUIDANCE
                            )),
                            SessionPolicy::Fresh => failed_outcome(&format!(
                                "{}: execution timed out after {:.1}s (the call's budget); the interpreter was reset (each call starts from a fresh namespace); {}",
                                self.label,
                                timeout.as_secs_f64(),
                                TIMEOUT_GUIDANCE
                            )),
                        }
                    }
                    Err(_) => {
                        // The job finished (or was already cancelled) around
                        // the deadline: nothing of this call's is running, so
                        // the session is not reset.
                        failed_outcome(&format!(
                            "{}: execution timed out after {:.1}s; the call had already finished or been abandoned (the {noun} was left intact); {}",
                            self.label,
                            timeout.as_secs_f64(),
                            TIMEOUT_GUIDANCE
                        ))
                    }
                }
            }
        }
    }

    /// Drop the current worker handle: the job channel closes, so the worker
    /// exits after its in-flight job (or stays parked if it wedged in
    /// non-cooperative native code —bounded leak, documented in the module
    /// header). Any queued job is refused by the now-abandoned worker instead
    /// of running against a VM whose session was reset. The next `exec`
    /// spawns a fresh VM.
    fn reset_worker(&self) {
        let mut guard = match self.worker.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(handle) = guard.take() {
            handle.abandoned.store(true, Ordering::Release);
        }
    }

    /// Abort the in-flight job (the host cancelled the turn): take the worker
    /// handle out first — taking it *is* the reset, so a concurrent call can
    /// only ever reach a fresh worker — then deliver the interrupt to the
    /// running eval loop and drop the taken handle, which closes the job
    /// channel so the worker exits after its (interrupted) job. Queued jobs
    /// are refused by the abandoned worker rather than run against the old
    /// VM. The next `exec` spawns a fresh VM, exactly like the timeout path.
    pub(crate) fn abort_worker(&self) {
        let handle = {
            let mut guard = match self.worker.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.take()
        };
        if let Some(handle) = handle {
            handle.abandoned.store(true, Ordering::Release);
            deliver_interrupt(
                &handle,
                &format!(
                    "{}: execution aborted (the turn was cancelled by the host)",
                    self.label
                ),
            );
        }
    }

    /// Tear the session down: interrupt the in-flight job, stop the worker
    /// thread, and wait (bounded by [`SHUTDOWN_GRACE`]) for it to exit.
    ///
    /// Idempotent and safe on a session that never spawned a worker (returns
    /// immediately). A cooperative eval loop consumes the interrupt within
    /// milliseconds; a script wedged in non-cooperative native code cannot,
    /// so the wait never extends past the grace window and that thread is
    /// detached (the documented bounded leak). A later `exec` starts a fresh
    /// worker.
    pub(crate) async fn shutdown(&self) {
        let handle = {
            let mut guard = match self.worker.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.take()
        };
        let Some(handle) = handle else {
            // No worker was ever started: nothing to interrupt or reap.
            return;
        };
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        handle.abandoned.store(true, Ordering::Release);
        // First interrupt send is synchronous (the detached retries stop
        // once the running job reports done).
        let _ = send_interrupt(
            &handle,
            &format!(
                "{}: execution aborted (the session is shutting down)",
                self.label
            ),
        );
        // Queue the teardown marker behind the running (interrupted) job.
        // Without it an idle worker would only notice every sender dropping,
        // and an in-flight call holds a sender clone until its reply lands.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let queued = handle.job_tx.send(WorkerMessage::Shutdown(ack_tx)).is_ok();
        if queued {
            // The worker acknowledges before dropping the interpreter.
            let _ = tokio::time::timeout_at(deadline, ack_rx).await;
        }
        // Reap the thread if it finished within the grace window; otherwise
        // detach it (a wedged native call may outlive teardown).
        let join = {
            let mut guard = match handle.join.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.take()
        };
        if let Some(join) = join {
            while !join.is_finished() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}

/// Queue one KeyboardInterrupt-style interrupt; `false` when the VM is gone.
fn send_interrupt(handle: &WorkerHandle, message: &str) -> bool {
    let sender = match handle.interrupt_tx.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(sender) = sender else {
        return false;
    };
    let message = message.to_owned();
    sender
        .send(Box::new(move |vm: &rpy::VirtualMachine| {
            let exc_type = vm.ctx.exceptions.keyboard_interrupt.to_owned();
            Err(vm.new_exception_msg(exc_type, message))
        }))
        .is_ok()
}

/// Whether a job still owns the eval loop (or is about to: a queued job is
/// refused by the abandoned worker, so `None` means no retry is needed).
fn job_owns_eval_loop(handle: &WorkerHandle) -> bool {
    let guard = match handle.running_state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .as_ref()
        .is_some_and(|state| state.load(Ordering::Acquire) != JOB_DONE)
}

/// Deliver a KeyboardInterrupt-style interrupt and keep it raised until the
/// vm actually consumes it (bounded retry; see [`PY_VM_EXEC_LOCK`] for why a
/// single send can be starved by a concurrent vm's eval loop).
///
/// The first send is synchronous — it only queues the signal closure and sets
/// the process-global trigger flag, no I/O — so the abort/timeout path returns
/// to the agent loop immediately. The bounded retries run on a detached task
/// with async sleeps; they stop as soon as the worker reports the job done, so
/// a single-threaded host runtime is never parked for the old five-second
/// window.
fn deliver_interrupt(handle: &WorkerHandle, message: &str) {
    if !send_interrupt(handle, message) {
        // Receiver gone: the vm already exited.
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // No async context (not reachable from the tool's async run path):
        // the first send is all a synchronous caller can use.
        return;
    };
    let handle = handle.clone();
    let message = message.to_owned();
    // Detached on purpose: the caller must not wait out the retry window.
    drop(runtime.spawn(async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if std::time::Instant::now() >= deadline || !job_owns_eval_loop(&handle) {
                break;
            }
            if !send_interrupt(&handle, &message) {
                // Receiver gone: the vm already exited.
                break;
            }
        }
    }));
}

/// Process-wide bytecode exclusion for the embedded interpreters.
///
/// RustPython's user-signal trigger flag is a PROCESS-GLOBAL (`signal.rs`
/// `ANY_TRIGGERED`): with several interpreter VMs in one process (parallel
/// agent sessions, test binaries), a vm whose eval loop polls
/// `check_signals` aggressively consumes the flag that an interrupt sent to
/// a DIFFERENT vm's channel was raising — that vm's queued signal closure is
/// then never drained and the interrupt never lands. Holding this lock
/// around each job guarantees only one vm executes bytecode at a time, so
/// the flag set for the running vm's interrupt is always consumed by the vm
/// it was sent to. Python tools serialize process-wide for the duration of
/// a job (interrupts remain fully deliverable; parallel python tool calls
/// queue instead of racing).
static PY_VM_EXEC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Wakes jobs parked on [`PY_VM_EXEC_LOCK`] the moment its holder releases.
///
/// The wait is event-driven: a release notifies every parked job, so a queued
/// call starts at handoff instead of on the next poll tick. The timed wait
/// that bounds the cancellation/wedge re-checks (see
/// [`EXEC_LOCK_CHECK_SLICE`]) is not how the lock is normally acquired.
static PY_VM_EXEC_RELEASED: std::sync::Condvar = std::sync::Condvar::new();

/// Orders the release notification against a waiter's last availability
/// check.
///
/// Not the execution lock: a waiter holds it only while it re-tests
/// [`PY_VM_EXEC_LOCK`] and parks, and [`ExecLockGuard::drop`] takes it only to
/// notify. A release that lands between a waiter's failed `try_lock` and its
/// park therefore cannot lose the wake-up — the releaser blocks on the gate
/// until the waiter is parked (or has acquired on the re-test).
static PY_VM_EXEC_WAIT_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How long a parked job sleeps before it re-checks its own cancellation and
/// the holder's age.
///
/// This bounds only those two checks, and matches the old 20 ms bound on how
/// long a deadline-cancelled queued call could linger. The lock itself is
/// handed over by [`PY_VM_EXEC_RELEASED`]'s notification, so this interval
/// never gates a normal acquisition.
const EXEC_LOCK_CHECK_SLICE: Duration = Duration::from_millis(20);

/// Starvation safety valve: once the current lock holder has run longer than
/// this, waiting jobs fail fast instead of parking behind a call that ignored
/// its deadline. 2x [`MAX_TIMEOUT_MS`]: a cooperative job is interrupted at
/// the 120 s ceiling, so a holder past 240 s is in non-cooperative native
/// code (or the interrupt failed to land).
const EXEC_LOCK_WEDGED_AFTER: Duration = Duration::from_millis(2 * MAX_TIMEOUT_MS);

/// Monotonic reference for [`EXEC_LOCK_HOLDER_MS`].
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Start of the lock holder in milliseconds since [`PROCESS_START`] (+1);
/// `0` means the lock is not held.
static EXEC_LOCK_HOLDER_MS: AtomicU64 = AtomicU64::new(0);

fn process_ms() -> u64 {
    PROCESS_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn exec_lock_holder_age() -> Option<Duration> {
    let since = EXEC_LOCK_HOLDER_MS.load(Ordering::Acquire);
    (since != 0).then(|| Duration::from_millis(process_ms().saturating_sub(since)))
}

/// Hidden test seam: the starvation valve's threshold (see
/// [`EXEC_LOCK_WEDGED_AFTER`]).
#[cfg(feature = "context-mode")]
pub(crate) fn exec_lock_wedged_after() -> Duration {
    EXEC_LOCK_WEDGED_AFTER
}

/// What one waiter did while parked, for [`exec_lock_wait_probe`].
///
/// Production callers pass `None`; the counters exist so the hidden probe can
/// prove the handoff is notification-driven rather than a fixed-interval
/// poll.
#[derive(Debug, Default)]
struct ExecLockWaitStats {
    /// Condvar waits entered.
    parks: AtomicU64,
    /// Wakes that came from [`PY_VM_EXEC_RELEASED`] before the slice elapsed.
    notified_wakes: AtomicU64,
    /// Wakes where [`EXEC_LOCK_CHECK_SLICE`] elapsed first (the cancellation
    /// and wedge re-check path).
    timed_out_wakes: AtomicU64,
}

/// Owns the process-wide exec lock for one job and clears the holder marker
/// even on a panic in user code (the mutex itself is poisoned then, and the
/// next job recovers it explicitly).
struct ExecLockGuard {
    guard: Option<std::sync::MutexGuard<'static, ()>>,
}

impl Drop for ExecLockGuard {
    fn drop(&mut self) {
        // Release exclusion first: a waiter woken while the mutex is still
        // held would fail its re-test and park again.
        drop(self.guard.take());
        EXEC_LOCK_HOLDER_MS.store(0, Ordering::Release);
        // Take the gate before notifying: a waiter that is between its failed
        // `try_lock` and its park holds the gate, so this notification cannot
        // slip into that window and be lost.
        let _gate = lock_wait_gate();
        PY_VM_EXEC_RELEASED.notify_all();
    }
}

fn lock_wait_gate() -> std::sync::MutexGuard<'static, ()> {
    match PY_VM_EXEC_WAIT_GATE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn mark_exec_lock_held() {
    EXEC_LOCK_HOLDER_MS.store(process_ms().saturating_add(1), Ordering::Release);
}

enum ExecLockWait {
    Acquired(ExecLockGuard),
    /// The call's own deadline already cancelled it while it waited.
    Cancelled,
    /// The holder exceeded [`EXEC_LOCK_WEDGED_AFTER`]; refuse to start.
    Wedged,
}

/// Acquire the process-wide execution lock without an unbounded blocking
/// wait: a queued call that has been cancelled stops waiting, and a holder
/// that ignored its deadline cannot keep new jobs parked forever.
fn acquire_exec_lock(state: &AtomicU8) -> ExecLockWait {
    acquire_exec_lock_observed(state, None)
}

/// [`acquire_exec_lock`] with an optional park observer (the hidden test
/// probe; production passes `None`).
fn acquire_exec_lock_observed(state: &AtomicU8, stats: Option<&ExecLockWaitStats>) -> ExecLockWait {
    loop {
        if state.load(Ordering::Acquire) != JOB_QUEUED {
            return ExecLockWait::Cancelled;
        }
        match PY_VM_EXEC_LOCK.try_lock() {
            Ok(guard) => {
                mark_exec_lock_held();
                return ExecLockWait::Acquired(ExecLockGuard { guard: Some(guard) });
            }
            Err(TryLockError::Poisoned(poisoned)) => {
                // A previous job panicked while holding the lock; the mutex
                // state is irrelevant (it guards nothing but exclusion).
                mark_exec_lock_held();
                return ExecLockWait::Acquired(ExecLockGuard {
                    guard: Some(poisoned.into_inner()),
                });
            }
            Err(TryLockError::WouldBlock) => {
                if exec_lock_holder_age().is_some_and(|age| age >= EXEC_LOCK_WEDGED_AFTER) {
                    return ExecLockWait::Wedged;
                }
            }
        }
        // Block until the holder releases. The gate is held across the
        // re-test and the park, which is what makes the release notification
        // reliable: a release either happens before the re-test (which then
        // acquires) or after the park.
        let gate = lock_wait_gate();
        match PY_VM_EXEC_LOCK.try_lock() {
            Ok(guard) => {
                mark_exec_lock_held();
                drop(gate);
                return ExecLockWait::Acquired(ExecLockGuard { guard: Some(guard) });
            }
            Err(TryLockError::Poisoned(poisoned)) => {
                mark_exec_lock_held();
                drop(gate);
                return ExecLockWait::Acquired(ExecLockGuard {
                    guard: Some(poisoned.into_inner()),
                });
            }
            Err(TryLockError::WouldBlock) => {}
        }
        // The re-checks the timed wait exists for. They run before parking
        // and once per slice while parked.
        if state.load(Ordering::Acquire) != JOB_QUEUED {
            return ExecLockWait::Cancelled;
        }
        if exec_lock_holder_age().is_some_and(|age| age >= EXEC_LOCK_WEDGED_AFTER) {
            return ExecLockWait::Wedged;
        }
        if let Some(stats) = stats {
            stats.parks.fetch_add(1, Ordering::AcqRel);
        }
        let (gate, timed_out) = PY_VM_EXEC_RELEASED
            .wait_timeout(gate, EXEC_LOCK_CHECK_SLICE)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(gate);
        if let Some(stats) = stats {
            if timed_out.timed_out() {
                stats.timed_out_wakes.fetch_add(1, Ordering::AcqRel);
            } else {
                stats.notified_wakes.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}

/// The probe lives next to the lock it observes. An inherent impl may be
/// written from any module of the defining crate, so this keeps the public
/// `python` module surface untouched (`python.rs` exports the type).
impl super::python::PythonTool {
    /// Hidden test seam: measure one contended handoff of the process-wide
    /// execution lock.
    ///
    /// Takes the lock, starts a waiter through the production acquisition
    /// path, waits until that waiter is parked, holds for `hold`, then
    /// releases. Returns what the waiter observed as
    /// `(acquired, parks, notified_wakes, timed_out_wakes, waited_micros)`:
    ///
    /// * `parks` counts condvar waits — a fixed-interval poller would enter a
    ///   fresh wait every interval while the lock stays held;
    /// * `notified_wakes` counts wakes the release notification caused, and
    ///   `timed_out_wakes` wakes where the check slice elapsed first. An
    ///   event-driven handoff reports at least one notified wake and no
    ///   timed-out wake when `hold` is well inside the slice.
    #[doc(hidden)]
    #[must_use]
    pub fn exec_lock_wait_probe(hold: Duration) -> (bool, u64, u64, u64, u64) {
        let holder = match PY_VM_EXEC_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        mark_exec_lock_held();
        let guard = ExecLockGuard {
            guard: Some(holder),
        };
        let stats = Arc::new(ExecLockWaitStats::default());
        let waiter_stats = Arc::clone(&stats);
        let state = Arc::new(AtomicU8::new(JOB_QUEUED));
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let acquired = matches!(
                acquire_exec_lock_observed(&state, Some(&waiter_stats)),
                ExecLockWait::Acquired(_)
            );
            let waited = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            (acquired, waited)
        });
        // `parks` is incremented immediately before the condvar wait, while
        // the waiter still holds the wait gate, so this release cannot notify
        // before the park (see [`PY_VM_EXEC_WAIT_GATE`]).
        let deadline = Instant::now() + Duration::from_secs(5);
        while stats.parks.load(Ordering::Acquire) == 0 {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }
        std::thread::sleep(hold);
        drop(guard);
        let (acquired, waited) = waiter.join().unwrap_or((false, u64::MAX));
        (
            acquired,
            stats.parks.load(Ordering::Acquire),
            stats.notified_wakes.load(Ordering::Acquire),
            stats.timed_out_wakes.load(Ordering::Acquire),
            waited,
        )
    }
}

/// A session-level failure (no job ran) shaped as an outcome.
pub(crate) fn failed_outcome(message: &str) -> ExecOutcome {
    ExecOutcome {
        stdout: String::new(),
        stderr: message.to_owned(),
        error: true,
        eval_repr: None,
        duration_ms: 0,
    }
}

/// Spawn the worker thread owning the RustPython `Interpreter`.
///
/// The VM (and the session scope) are `!Send` (`PyRc`-based), so they live
/// and die on this thread; jobs arrive over an mpsc channel and results go
/// back through a tokio oneshot (`Sender::send` is sync, callable from a
/// plain thread). The interpreter is built inside the thread on its first
/// iteration, so spawning the thread does not block the caller on stdlib
/// initialization.
fn spawn_worker(policy: SessionPolicy, label: &'static str) -> WorkerHandle {
    let (job_tx, job_rx) = std_mpsc::channel::<WorkerMessage>();
    let (interrupt_tx, interrupt_rx) = rpy::signal::user_signal_channel();
    let interrupt_tx = Arc::new(StdMutex::new(Some(interrupt_tx)));
    let abandoned = Arc::new(AtomicBool::new(false));
    let running_state = Arc::new(StdMutex::new(None));

    let worker_abandoned = Arc::clone(&abandoned);
    let worker_running_state = Arc::clone(&running_state);
    let spawn = std::thread::Builder::new()
        .name("pir-python-vm".to_owned())
        .spawn(move || {
            worker_loop(
                job_rx,
                interrupt_rx,
                worker_abandoned,
                worker_running_state,
                policy,
                label,
            );
        });
    // If the spawn failed, the closure (holding the receiver) drops with the
    // builder: the job channel is already closed and the next `exec` reports
    // the failure and resets.
    WorkerHandle {
        job_tx,
        interrupt_tx,
        abandoned,
        running_state,
        join: Arc::new(StdMutex::new(spawn.ok())),
    }
}

/// The worker's main loop: build the interpreter once (frozen stdlib, user
/// signal channel wired for the host-side interrupt), then run jobs
/// sequentially against the session (or a fresh) scope.
fn worker_loop(
    job_rx: std_mpsc::Receiver<WorkerMessage>,
    interrupt_rx: rpy::signal::UserSignalReceiver,
    abandoned: Arc<AtomicBool>,
    running_state: Arc<StdMutex<Option<Arc<AtomicU8>>>>,
    policy: SessionPolicy,
    label: &'static str,
) {
    let mut settings = rpy::Settings::default();
    // Embedded posture: no site dirs, no environment-variable config, no
    // bytecode writing, no OS signal-handler installation (the process's own
    // Ctrl+C handling stays untouched), and no cwd on sys.path (cwd is added
    // per call below, deduped).
    settings.isolated = true;
    settings.ignore_environment = true;
    settings.import_site = false;
    settings.user_site_directory = false;
    settings.write_bytecode = false;
    settings.install_signal_handlers = false;

    let interpreter = rpy::Interpreter::with_init(settings, |vm| {
        vm.add_native_modules(rustpython_stdlib::get_module_inits());
        vm.add_frozen(rustpython_pylib::FROZEN_STDLIB);
        // Host-side interrupt channel: polled by the eval loop's
        // `check_signals` (the Python-level handler table stays `Some`, which
        // is what gates the poll).
        vm.set_user_signal_channel(interrupt_rx);
    });

    // Session namespace: one scope with `__builtins__` wired into the shared
    // globals dict. The persistent policy runs every job against it (so
    // definitions persist across calls); the fresh policy creates a new
    // scope per job and leaves this one unused.
    let scope = interpreter.enter(|vm| vm.new_scope_with_builtins());
    // sys._stdlib_dir back-reference for importlib (the same hack the
    // `rustpython` meta-crate's `init_stdlib` applies for frozen builds).
    interpreter.enter(|vm| {
        if vm.sys_module.get_attr("_stdlib_dir", vm).is_err() {
            let lib_path = rustpython_pylib::LIB_PATH.to_owned();
            let _ = vm
                .sys_module
                .set_attr("_stdlib_dir", vm.new_pyobj(lib_path), vm);
        }
    });

    let mut added_cwds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Whether the native JSON fast path was installed on this worker's module
    // cache. Installed lazily on the first job so the `json` import runs while
    // the process-wide exec lock is held (same rules as user bytecode).
    let mut json_installed = false;
    for message in job_rx {
        let job = match message {
            WorkerMessage::Shutdown(ack) => {
                // Acknowledge while the interpreter is still alive; teardown
                // (dropping the VM and its frozen stdlib) follows.
                let _ = ack.send(());
                break;
            }
            WorkerMessage::Job(job) => job,
        };
        if abandoned.load(Ordering::Acquire) {
            // The session was reset (timeout/abort) while this job waited:
            // running it against the abandoned VM would be work nobody can
            // observe, on globals the reset promised to discard.
            let _ = job.reply.send(failed_outcome(&format!(
                "{label}: the interpreter session was reset while this call was queued; the call did not run"
            )));
            continue;
        }
        // Process-wide bytecode exclusion (see [`PY_VM_EXEC_LOCK`]): while
        // THIS vm runs user code, no other vm in the process may, so the
        // global signal trigger flag set by an interrupt can only be
        // consumed by THIS vm's `check_signals` — never eaten by a
        // concurrent vm's eval loop. The wait is bounded: a cancelled queued
        // call stops waiting, and a wedged holder cannot park new jobs
        // forever (see [`EXEC_LOCK_WEDGED_AFTER`]).
        let _exec_guard = match acquire_exec_lock(&job.state) {
            ExecLockWait::Acquired(guard) => guard,
            ExecLockWait::Cancelled => continue,
            ExecLockWait::Wedged => {
                let _ = job.reply.send(failed_outcome(&format!(
                    "{label}: another Python job has held the process-wide interpreter lock for over {}s without yielding; refusing to start behind a wedged call (retry later)",
                    EXEC_LOCK_WEDGED_AFTER.as_secs()
                )));
                continue;
            }
        };
        // A reset may have landed while this worker waited for the lock.
        if abandoned.load(Ordering::Acquire) {
            let _ = job.reply.send(failed_outcome(&format!(
                "{label}: the interpreter session was reset while this call was queued; the call did not run"
            )));
            continue;
        }
        // Only the job that wins Queued -> Running owns the eval loop; a
        // call whose own deadline expired while queued already moved itself
        // to Cancelled and must be skipped (running it would also feed its
        // caller's deadline error to the wrong job).
        if job
            .state
            .compare_exchange(JOB_QUEUED, JOB_RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        // Publish the running job's state cell: an abort/timeout retry loop
        // stops as soon as this job reports done instead of sleeping out its
        // whole retry window.
        *running_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&job.state));
        let outcome = match policy {
            SessionPolicy::Persistent => interpreter.enter(|vm| {
                run_job(
                    vm,
                    &scope,
                    &job.job,
                    &mut added_cwds,
                    &mut json_installed,
                    label,
                )
            }),
            SessionPolicy::Fresh => {
                // Fresh namespace: created and dropped on this thread, so no
                // Python object ever crosses a thread boundary.
                let job_scope = interpreter.enter(|vm| vm.new_scope_with_builtins());
                interpreter.enter(|vm| {
                    run_job(
                        vm,
                        &job_scope,
                        &job.job,
                        &mut added_cwds,
                        &mut json_installed,
                        label,
                    )
                })
            }
        };
        job.state.store(JOB_DONE, Ordering::Release);
        *running_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let _ = job.reply.send(outcome);
    }
    // job_rx exhausted (all senders dropped): interpreter and scope drop here
    // on this thread —RustPython object destruction requires the VM
    // context, which is exactly what thread ownership guarantees.
}

/// Run one job: inject globals, redirect sys.stdout/sys.stderr into capping
/// sinks, ensure the call's cwd is importable, compile (eval-first when
/// requested, exec fallback), run, collect output, restore the std streams.
fn run_job(
    vm: &rpy::VirtualMachine,
    scope: &rpy::scope::Scope,
    job: &PythonJob,
    added_cwds: &mut std::collections::BTreeSet<String>,
    json_installed: &mut bool,
    label: &str,
) -> ExecOutcome {
    let started = Instant::now();

    // Native JSON fast path (see the "Native JSON fast path" section below).
    // Installed before user code on the first job. A non-interrupt failure
    // leaves the stdlib `json` module untouched — the fast path is never a
    // correctness dependency.
    if !*json_installed {
        *json_installed = true;
        if let Err(exc) = install_json_fast_path(vm) {
            // A deadline interrupt can land while the frozen `json` module is
            // being imported. Swallowing it would let user code run past a
            // deadline whose interrupt was already consumed (the watchdog's
            // bounded retries would stop), wedging the worker and the
            // process-wide exec lock. Report it as this job's failure instead.
            if exc.fast_isinstance(vm.ctx.exceptions.keyboard_interrupt)
                || exc.fast_isinstance(vm.ctx.exceptions.system_exit)
            {
                return ExecOutcome {
                    duration_ms: started.elapsed().as_millis(),
                    ..failed_outcome(&format!(
                        "{label}: the interpreter was interrupted before this call could run"
                    ))
                };
            }
        }
    }

    // cwd import support (parity with the upstream bash+python3 tool, which
    // runs with the project as cwd): prepend once per unique directory.
    if let Some(cwd) = &job.cwd {
        if !cwd.is_empty() && added_cwds.insert(cwd.clone()) {
            if let Err(exc) = vm.insert_sys_path(vm.new_pyobj(cwd.clone())) {
                let mut text: Vec<u8> = Vec::new();
                let _ = vm.write_exception(&mut rpy::py_io::IoWriter(&mut text), &exc);
                let message = format!(
                    "{label}: could not add the working directory to sys.path: {}",
                    String::from_utf8_lossy(&text)
                );
                return ExecOutcome {
                    stdout: String::new(),
                    stderr: message,
                    error: true,
                    eval_repr: None,
                    duration_ms: started.elapsed().as_millis(),
                };
            }
        }
    }

    // Sandbox variables (`FILE_CONTENT` / `FILE_PATH` for context-mode):
    // defined before the user code runs, never echoed anywhere. The shared
    // `Arc<str>` is read once here; RustPython's string is the only copy the
    // interpreter needs.
    #[cfg(feature = "context-mode")]
    for (name, value) in &job.globals {
        let value = match value {
            JobGlobal::Text(text) => vm.new_pyobj(text.as_ref()),
        };
        if let Err(exc) = scope.globals.set_item(name.as_str(), value, vm) {
            let mut text: Vec<u8> = Vec::new();
            let _ = vm.write_exception(&mut rpy::py_io::IoWriter(&mut text), &exc);
            let message = format!(
                "{label}: could not define the sandbox variable `{name}`: {}",
                String::from_utf8_lossy(&text)
            );
            return ExecOutcome {
                duration_ms: started.elapsed().as_millis(),
                ..failed_outcome(&message)
            };
        }
    }

    // Capture plumbing: fresh capping sinks per call, std streams restored on
    // every exit path below.
    let capture = match install_capture(vm) {
        Ok(capture) => capture,
        Err(exc) => {
            let mut text: Vec<u8> = Vec::new();
            let _ = vm.write_exception(&mut rpy::py_io::IoWriter(&mut text), &exc);
            let message = format!(
                "{label}: could not install output capture: {}",
                String::from_utf8_lossy(&text)
            );
            return ExecOutcome {
                duration_ms: started.elapsed().as_millis(),
                ..failed_outcome(&message)
            };
        }
    };

    let run = compile_and_run(vm, scope, &job.code, job.eval_first);
    let outcome = finish_job(vm, &capture, run);
    ExecOutcome {
        duration_ms: started.elapsed().as_millis(),
        ..outcome
    }
}

/// Bounded captured-output state for one stream (stdout or stderr).
#[derive(Default)]
struct CaptureBuffer {
    text: String,
    /// Number of chars in `text` (kept incrementally; never re-counted).
    chars: usize,
    /// The stream produced output past [`MAX_OUTPUT_CHARS`].
    truncated: bool,
}

impl CaptureBuffer {
    /// Append `text`, retaining at most [`MAX_OUTPUT_CHARS`] chars.
    ///
    /// Returns the input's char count (the Python `write` contract reports
    /// how many chars it accepted, so dropping the tail must not make user
    /// code retry or raise). Once truncated, further writes are counted and
    /// discarded without touching the buffer.
    fn write(&mut self, text: &str) -> usize {
        let chars = text.chars().count();
        if self.truncated {
            return chars;
        }
        let room = MAX_OUTPUT_CHARS.saturating_sub(self.chars);
        if chars <= room {
            self.text.push_str(text);
            self.chars += chars;
            return chars;
        }
        // Keep exactly the remaining room (char-boundary safe); drop the
        // tail. `nth(room)` is the byte index of the first dropped char.
        let keep = text
            .char_indices()
            .nth(room)
            .map(|(byte_index, _)| byte_index)
            .unwrap_or(text.len());
        self.text.push_str(&text[..keep]);
        self.chars = MAX_OUTPUT_CHARS;
        self.truncated = true;
        chars
    }
}

/// The capping stdout/stderr state replacing sys.stdout/sys.stderr for one
/// call, plus the saved originals for restoration.
struct Capture {
    stdout: Arc<StdMutex<CaptureBuffer>>,
    stderr: Arc<StdMutex<CaptureBuffer>>,
    saved_stdout: rpy::PyObjectRef,
    saved_stderr: rpy::PyObjectRef,
}

/// Build the Python-visible stream object: an attribute namespace whose
/// `write` is a native function appending to `buffer`. `print` and
/// `print_exception` only need `write`/`flush`; the extra attributes keep
/// common library probes (`encoding`, `isatty`, ...) working.
fn capped_stream(
    vm: &rpy::VirtualMachine,
    buffer: Arc<StdMutex<CaptureBuffer>>,
) -> rpy::PyResult<rpy::PyObjectRef> {
    let namespace = rpy::builtins::PyNamespace::new_ref(&vm.ctx);
    let object: rpy::PyObjectRef = namespace.into();
    let sink = Arc::clone(&buffer);
    let write = vm.new_function("write", move |text: rpy::builtins::PyStrRef| -> usize {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write(text.as_str())
    });
    object.set_attr("write", write, vm)?;
    object.set_attr("flush", vm.new_function("flush", || {}), vm)?;
    let sink = Arc::clone(&buffer);
    let getvalue = vm.new_function("getvalue", move || -> String {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text
            .clone()
    });
    object.set_attr("getvalue", getvalue, vm)?;
    object.set_attr("isatty", vm.new_function("isatty", || false), vm)?;
    object.set_attr("writable", vm.new_function("writable", || true), vm)?;
    object.set_attr("encoding", vm.new_pyobj("utf-8"), vm)?;
    object.set_attr("errors", vm.new_pyobj("strict"), vm)?;
    Ok(object)
}

fn install_capture(vm: &rpy::VirtualMachine) -> Result<Capture, rpy::builtins::PyBaseExceptionRef> {
    let stdout = Arc::new(StdMutex::new(CaptureBuffer::default()));
    let stderr = Arc::new(StdMutex::new(CaptureBuffer::default()));
    let stdout_stream = capped_stream(vm, Arc::clone(&stdout))?;
    let stderr_stream = capped_stream(vm, Arc::clone(&stderr))?;
    let saved_stdout = vm.sys_module.get_attr("stdout", vm)?;
    let saved_stderr = vm.sys_module.get_attr("stderr", vm)?;
    vm.sys_module.set_attr("stdout", stdout_stream, vm)?;
    vm.sys_module.set_attr("stderr", stderr_stream, vm)?;
    Ok(Capture {
        stdout,
        stderr,
        saved_stdout,
        saved_stderr,
    })
}

/// Restore the std streams (best effort; the sinks are dropped with the job
/// either way).
fn restore_capture(vm: &rpy::VirtualMachine, capture: &Capture) {
    let _ = vm
        .sys_module
        .set_attr("stdout", capture.saved_stdout.clone(), vm);
    let _ = vm
        .sys_module
        .set_attr("stderr", capture.saved_stderr.clone(), vm);
}

/// Take a captured stream's text and "was truncated" flag.
fn take_buffer(buffer: &Arc<StdMutex<CaptureBuffer>>) -> (String, bool) {
    let mut guard = buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (std::mem::take(&mut guard.text), guard.truncated)
}

/// Capped text plus the same trailing notice `cap_output` produces.
fn capped_text(mut text: String, truncated: bool) -> String {
    if truncated {
        text.push_str(TRUNCATION_NOTICE);
    }
    text
}

// ---------------------------------------------------------------------------
// Native JSON fast path
// ---------------------------------------------------------------------------
//
// `json` is pure Python in the frozen stdlib (`rustpython-pylib`'s
// `Lib/json/{__init__,encoder,decoder,scanner}.py`): the encoder recurses per
// value and concatenates Python strings, and the decoder is a Python loop over
// the native `_json` scanner. For record-shaped workloads (`json.dumps` on
// 10k dicts, `json.loads` of the same) that Python layer dominates.
//
// The worker replaces the four module functions with native wrappers that
// convert the payload once through `serde_json`. Anything the fast path cannot
// reproduce exactly is forwarded UNCHANGED to the original stdlib function,
// which is stashed on the module as `_pir_stdlib_*` at install time:
//
// - value subclasses (dict/list/tuple/str/int/float subclasses may override
//   iteration, `items()`, `__repr__`, ...),
// - `default=` / `cls=` / decode hooks / `skipkeys=True` / unknown kwargs,
// - non-finite floats (`allow_nan` spellings `NaN`/`Infinity` are not JSON),
// - circular or deeper-than-[`MAX_JSON_DEPTH`] values,
// - integers outside u64/i64 (CPython ints are arbitrary precision; serde_json
//   would silently turn them into f64) and `-0` (Python int, serde f64),
// - non-str dict keys other than int/bool/None (float keys need
//   `float.__repr__` formatting).
//
// The override mutates the shared `json` module object, so `import json`,
// `from json import dumps`, and re-imports all observe it in both session
// policies (Persistent and Fresh namespaces share the worker's module cache).
// The wrappers look the stdlib fallback up in `sys.modules` on demand instead
// of capturing it: RustPython leaks native-function closures (`Box::leak` in
// `Context::new_method_def`), so a captured PyObjectRef would stay alive for
// the process lifetime.

const STD_DUMPS: &str = "_pir_stdlib_dumps";
const STD_LOADS: &str = "_pir_stdlib_loads";
const STD_DUMP: &str = "_pir_stdlib_dump";
const STD_LOAD: &str = "_pir_stdlib_load";

/// Fast-path nesting cap. Deeper values go to the stdlib encoder, which keeps
/// the interpreter's own recursion accounting (`RecursionError` semantics)
/// authoritative and keeps this recursive conversion far below stack limits.
const MAX_JSON_DEPTH: usize = 128;

/// Indent widths above this are delegated to the stdlib encoder: the fast
/// formatter would otherwise materialize huge whitespace runs.
const MAX_JSON_INDENT: u64 = 64;

/// The `dumps`/`dump` keyword whitelist. Any other keyword is forwarded so the
/// stdlib raises the same `TypeError` a direct call would.
const DUMPS_KEYWORDS: [&str; 9] = [
    "skipkeys",
    "ensure_ascii",
    "check_circular",
    "allow_nan",
    "cls",
    "indent",
    "separators",
    "default",
    "sort_keys",
];

/// The `loads`/`load` keyword whitelist (all must be absent or `None` for the
/// fast path; hooks force the stdlib decoder).
const LOADS_KEYWORDS: [&str; 6] = [
    "cls",
    "object_hook",
    "parse_float",
    "parse_int",
    "parse_constant",
    "object_pairs_hook",
];

/// Install the native `json` wrappers on the worker's module cache.
///
/// Idempotent: a second call is a no-op when the backups already exist. Runs
/// on the worker thread (Python objects are `!Send`).
fn install_json_fast_path(vm: &rpy::VirtualMachine) -> rpy::PyResult<()> {
    // Eager import: the override must be visible before user code runs, and
    // `json` is frozen, so this costs no filesystem access.
    vm.import("json", 0)?;
    let modules = vm.sys_module.get_attr("modules", vm)?;
    let json = modules.get_item("json", vm)?;
    if json.get_attr(STD_DUMPS, vm).is_ok() {
        return Ok(());
    }
    for (name, backup) in [
        ("dumps", STD_DUMPS),
        ("loads", STD_LOADS),
        ("dump", STD_DUMP),
        ("load", STD_LOAD),
    ] {
        let original = json.get_attr(name, vm)?;
        json.set_attr(backup, original, vm)?;
    }
    json.set_attr("dumps", vm.new_function("dumps", json_dumps), vm)?;
    json.set_attr("loads", vm.new_function("loads", json_loads), vm)?;
    json.set_attr("dump", vm.new_function("dump", json_dump), vm)?;
    json.set_attr("load", vm.new_function("load", json_load), vm)?;
    Ok(())
}

/// Call one of the stashed stdlib functions, forwarding the caller's exact
/// arguments (positional and keyword) so its behavior is unchanged.
fn call_json_stdlib(
    vm: &rpy::VirtualMachine,
    backup: &'static str,
    args: FuncArgs,
) -> rpy::PyResult {
    let modules = vm.sys_module.get_attr("modules", vm)?;
    let json = modules.get_item("json", vm)?;
    let original = json.get_attr(backup, vm)?;
    original.call(args, vm)
}

/// `json.dumps` replacement.
fn json_dumps(args: FuncArgs, vm: &rpy::VirtualMachine) -> rpy::PyResult {
    match fast_dumps(vm, &args) {
        Some(text) => Ok(vm.ctx.new_str(text).into()),
        None => call_json_stdlib(vm, STD_DUMPS, args),
    }
}

/// `json.loads` replacement.
fn json_loads(args: FuncArgs, vm: &rpy::VirtualMachine) -> rpy::PyResult {
    match fast_loads(vm, &args) {
        Some(value) => Ok(value),
        None => call_json_stdlib(vm, STD_LOADS, args),
    }
}

/// `json.dump` replacement: encode natively, then write the single chunk.
fn json_dump(args: FuncArgs, vm: &rpy::VirtualMachine) -> rpy::PyResult {
    if args.args.len() == 2 {
        if let Some(text) = fast_dump_text(vm, &args) {
            match args.args[1].get_attr("write", vm) {
                Ok(write) => {
                    write.call((vm.ctx.new_str(text),), vm)?;
                    return Ok(vm.ctx.none());
                }
                // No `write` attribute: let the stdlib raise its own error
                // (nothing has been written yet, so delegating is side-effect
                // free).
                Err(_) => return call_json_stdlib(vm, STD_DUMP, args),
            }
        }
    }
    call_json_stdlib(vm, STD_DUMP, args)
}

/// `json.load` replacement. The stream is read exactly once: after that, all
/// remaining work (including error reporting) goes through the stashed
/// `json.loads`, never `json.load` (which would read the stream again).
fn json_load(args: FuncArgs, vm: &rpy::VirtualMachine) -> rpy::PyResult {
    if args.args.len() != 1 || !load_kwargs_are_default(vm, &args) {
        return call_json_stdlib(vm, STD_LOAD, args);
    }
    let read = match args.args[0].get_attr("read", vm) {
        Ok(read) => read,
        // No `read` attribute: the stdlib call raises the same AttributeError.
        Err(_) => return call_json_stdlib(vm, STD_LOAD, args),
    };
    // The user's `read()` raising propagates: a retry would read twice.
    let data = read.call((), vm)?;
    if let Some(text) = data.downcast_ref_if_exact::<rpy::builtins::PyStr>(vm) {
        if let Some(value) = parse_json_text(vm, text.as_str()) {
            return Ok(value);
        }
    }
    // Bytes, bytearray, valid-but-unsupported JSON, or a non-JSON value: the
    // stdlib decoder owns the remaining semantics and the error messages.
    let mut forward = FuncArgs::default();
    forward.args.push(data);
    call_json_stdlib(vm, STD_LOADS, forward)
}

/// Encode `args.args[0]` with the native path; `None` means "delegate".
fn fast_dumps(vm: &rpy::VirtualMachine, args: &FuncArgs) -> Option<String> {
    if args.args.len() != 1 {
        return None;
    }
    fast_dump_text(vm, args)
}

/// Shared by `dumps` (one positional) and `dump` (value + stream).
fn fast_dump_text(vm: &rpy::VirtualMachine, args: &FuncArgs) -> Option<String> {
    let options = DumpOptions::parse(vm, args)?;
    let mut path = Vec::new();
    let value = py_to_json(vm, &args.args[0], &options, &mut path, 0)?;
    serialize_json(value, &options)
}

/// Parse `args.args[0]` with the native path; `None` means "delegate".
fn fast_loads(vm: &rpy::VirtualMachine, args: &FuncArgs) -> Option<rpy::PyObjectRef> {
    if args.args.len() != 1 || !load_kwargs_are_default(vm, args) {
        return None;
    }
    let text = args.args[0].downcast_ref_if_exact::<rpy::builtins::PyStr>(vm)?;
    parse_json_text(vm, text.as_str())
}

/// Whether every `loads`-family keyword is absent or `None`.
fn load_kwargs_are_default(vm: &rpy::VirtualMachine, args: &FuncArgs) -> bool {
    args.kwargs
        .iter()
        .all(|(key, value)| LOADS_KEYWORDS.contains(&key.as_str()) && vm.is_none(value))
}

/// Parse a JSON document into Python values, or `None` when serde_json cannot
/// represent the document the way CPython would (the caller then delegates).
///
/// serde_json is built without its `float_roundtrip` feature (this project's
/// dependency list is fixed), so its decimal parser is accurate only to
/// within ~1 ulp. CPython parses floats correctly rounded. The fast path
/// therefore re-parses every float literal from the source text with Rust's
/// correctly-rounded `f64::from_str` and substitutes the values in document
/// order; a count mismatch or a non-finite value delegates the whole call.
fn parse_json_text(vm: &rpy::VirtualMachine, text: &str) -> Option<rpy::PyObjectRef> {
    if serde_json_loses_python_numbers(text) {
        return None;
    }
    let mut value: JsonValue = serde_json::from_str(text).ok()?;
    let mut floats = scan_float_literals(text)?.into_iter();
    if !replace_floats_with_precise(&mut value, &mut floats) || floats.next().is_some() {
        return None;
    }
    json_value_to_py(vm, &value, 0)
}

/// Replace every float-form number in `value` (document order) with the next
/// correctly-rounded literal; `false` if the traversal and the literal list do
/// not line up.
fn replace_floats_with_precise(
    value: &mut JsonValue,
    floats: &mut std::vec::IntoIter<f64>,
) -> bool {
    match value {
        JsonValue::Number(number) if number.is_f64() => {
            let Some(precise) = floats.next() else {
                return false;
            };
            match JsonNumber::from_f64(precise) {
                Some(replacement) => {
                    *number = replacement;
                    true
                }
                None => false,
            }
        }
        JsonValue::Array(items) => items
            .iter_mut()
            .all(|item| replace_floats_with_precise(item, floats)),
        JsonValue::Object(map) => map
            .values_mut()
            .all(|item| replace_floats_with_precise(item, floats)),
        _ => true,
    }
}

/// Collect every float-form number literal (`1.5`, `1e9`, ...) outside strings,
/// in document order, parsed with Rust's correctly-rounded float parser.
fn scan_float_literals(text: &str) -> Option<Vec<f64>> {
    let bytes = text.as_bytes();
    let mut floats = Vec::new();
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if byte == b'-' || byte.is_ascii_digit() {
            let start = i;
            if byte == b'-' {
                i += 1;
            }
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let mut is_float = false;
            if bytes.get(i) == Some(&b'.') {
                is_float = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if matches!(bytes.get(i), Some(b'e' | b'E')) {
                is_float = true;
                i += 1;
                if matches!(bytes.get(i), Some(b'+' | b'-')) {
                    i += 1;
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if is_float {
                floats.push(text.get(start..i)?.parse::<f64>().ok()?);
            }
            continue;
        }
        i += 1;
    }
    Some(floats)
}

/// Conservative pre-scan for number literals serde_json would silently change:
/// integers wider than 19 digits (CPython: arbitrary-precision `int`,
/// serde_json: `f64`), negative 19-digit integers (also outside `i64`), and the
/// integer `-0` (CPython: `int` 0, serde_json: `-0.0`). String contents (with
/// escapes) are skipped.
fn serde_json_loses_python_numbers(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut negative = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            negative = false;
            i += 1;
            continue;
        }
        if byte.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = i - start;
            // 20+ digits always exceed u64; negative 19-digit values may
            // exceed i64 (serde_json would then produce an f64).
            if digits > 19 || (negative && digits >= 19) {
                return true;
            }
            negative = false;
            continue;
        }
        if byte == b'-'
            && bytes.get(i + 1) == Some(&b'0')
            && !matches!(
                bytes.get(i + 2).copied(),
                Some(b'0'..=b'9' | b'.' | b'e' | b'E')
            )
        {
            return true;
        }
        negative = byte == b'-';
        i += 1;
    }
    false
}

/// serde_json value -> Python object (exact builtin containers, insertion
/// order preserved by `preserve_order`).
fn json_value_to_py(
    vm: &rpy::VirtualMachine,
    value: &JsonValue,
    depth: usize,
) -> Option<rpy::PyObjectRef> {
    if depth > MAX_JSON_DEPTH {
        return None;
    }
    Some(match value {
        JsonValue::Null => vm.ctx.none(),
        JsonValue::Bool(boolean) => vm.ctx.new_bool(*boolean).into(),
        JsonValue::Number(number) => {
            if let Some(integer) = number.as_i64() {
                vm.ctx.new_int(integer).into()
            } else if let Some(integer) = number.as_u64() {
                vm.ctx.new_int(integer).into()
            } else {
                vm.ctx.new_float(number.as_f64()?).into()
            }
        }
        JsonValue::String(text) => vm.ctx.new_str(text.as_str()).into(),
        JsonValue::Array(items) => {
            let mut converted = Vec::with_capacity(items.len());
            for item in items {
                converted.push(json_value_to_py(vm, item, depth + 1)?);
            }
            vm.ctx.new_list(converted).into()
        }
        JsonValue::Object(map) => {
            let dict = vm.ctx.new_dict();
            for (key, item) in map {
                let converted = json_value_to_py(vm, item, depth + 1)?;
                dict.set_item(key.as_str(), converted, vm).ok()?;
            }
            dict.into()
        }
    })
}

/// Parsed `dumps`/`dump` options that the native encoder can honor exactly.
struct DumpOptions {
    ensure_ascii: bool,
    sort_keys: bool,
    indent: Option<usize>,
    item_separator: String,
    key_separator: String,
}

impl DumpOptions {
    /// Parse the call's keywords. `None` means the call is outside the fast
    /// path and must be forwarded to the stdlib.
    fn parse(vm: &rpy::VirtualMachine, args: &FuncArgs) -> Option<Self> {
        for key in args.kwargs.keys() {
            if !DUMPS_KEYWORDS.contains(&key.as_str()) {
                return None;
            }
        }
        let skipkeys = bool_kw(vm, args, "skipkeys")?.unwrap_or(false);
        if skipkeys {
            return None;
        }
        let ensure_ascii = bool_kw(vm, args, "ensure_ascii")?.unwrap_or(true);
        // Both are accepted as booleans; the encoder always detects cycles and
        // delegates non-finite floats, so their values do not change the path.
        let _check_circular = bool_kw(vm, args, "check_circular")?.unwrap_or(true);
        let _allow_nan = bool_kw(vm, args, "allow_nan")?.unwrap_or(true);
        let sort_keys = bool_kw(vm, args, "sort_keys")?.unwrap_or(false);
        if !none_kw(vm, args, "cls") || !none_kw(vm, args, "default") {
            return None;
        }
        let indent = indent_kw(vm, args)?;
        let separators = separators_kw(vm, args)?;
        let (item_separator, key_separator) = match separators {
            Some((item, key)) => (item, key),
            None if indent.is_some() => (",".to_owned(), ": ".to_owned()),
            None => (", ".to_owned(), ": ".to_owned()),
        };
        Some(Self {
            ensure_ascii,
            sort_keys,
            indent,
            item_separator,
            key_separator,
        })
    }
}

/// Bool keyword: `Some(None)` absent, `Some(Some(v))` exact `bool`, `None`
/// unsupported (forward to the stdlib, which applies its own truthiness).
fn bool_kw(vm: &rpy::VirtualMachine, args: &FuncArgs, name: &str) -> Option<Option<bool>> {
    match args.kwargs.get(name) {
        None => Some(None),
        Some(value) if value.class().is(vm.ctx.types.bool_type) => {
            Some(Some(value.is(&vm.ctx.true_value)))
        }
        Some(_) => None,
    }
}

/// Whether a keyword is absent or `None`.
fn none_kw(vm: &rpy::VirtualMachine, args: &FuncArgs, name: &str) -> bool {
    match args.kwargs.get(name) {
        None => true,
        Some(value) => vm.is_none(value),
    }
}

/// `indent=`: `None`, or an exact `int` in `0..=MAX_JSON_INDENT` (negative
/// ints mean newline-only in CPython, i.e. width 0).
fn indent_kw(vm: &rpy::VirtualMachine, args: &FuncArgs) -> Option<Option<usize>> {
    match args.kwargs.get("indent") {
        None => Some(None),
        Some(value) if vm.is_none(value) => Some(None),
        Some(value) if value.class().is(vm.ctx.types.int_type) => {
            let integer = value.payload::<rpy::builtins::PyInt>()?;
            let width: i64 = integer.as_bigint().to_string().parse().ok()?;
            if width < 0 {
                return Some(Some(0));
            }
            if width as u64 > MAX_JSON_INDENT {
                return None;
            }
            Some(Some(width as usize))
        }
        Some(_) => None,
    }
}

/// `separators=`: `None`, or an exact tuple/list of two exact `str`s.
#[allow(clippy::type_complexity)]
fn separators_kw(vm: &rpy::VirtualMachine, args: &FuncArgs) -> Option<Option<(String, String)>> {
    match args.kwargs.get("separators") {
        None => Some(None),
        Some(value) if vm.is_none(value) => Some(None),
        Some(value) => {
            let elements: Vec<rpy::PyObjectRef> =
                if let Some(tuple) = value.downcast_ref_if_exact::<rpy::builtins::PyTuple>(vm) {
                    tuple.as_slice().to_vec()
                } else {
                    value
                        .downcast_ref_if_exact::<rpy::builtins::PyList>(vm)?
                        .borrow_vec()
                        .to_vec()
                };
            if elements.len() != 2 {
                return None;
            }
            let item = elements[0]
                .downcast_ref_if_exact::<rpy::builtins::PyStr>(vm)?
                .as_str()
                .to_owned();
            let key = elements[1]
                .downcast_ref_if_exact::<rpy::builtins::PyStr>(vm)?
                .as_str()
                .to_owned();
            Some(Some((item, key)))
        }
    }
}

/// Convert a supported Python value into a `serde_json::Value`; `None` means
/// "unsupported, let the stdlib encode it".
///
/// `path` holds the raw addresses of the containers currently being encoded
/// (identity-based cycle detection); entries are popped on the way out.
fn py_to_json(
    vm: &rpy::VirtualMachine,
    obj: &rpy::PyObjectRef,
    options: &DumpOptions,
    path: &mut Vec<*const rpy::PyObject>,
    depth: usize,
) -> Option<JsonValue> {
    if depth > MAX_JSON_DEPTH {
        return None;
    }
    if vm.is_none(obj) {
        return Some(JsonValue::Null);
    }
    if obj.class().is(vm.ctx.types.bool_type) {
        return Some(JsonValue::Bool(obj.is(&vm.ctx.true_value)));
    }
    if let Some(integer) = obj.downcast_ref_if_exact::<rpy::builtins::PyInt>(vm) {
        // CPython ints are arbitrary precision; serde_json numbers are not.
        return number_from_bigint(&integer.as_bigint().to_string()).map(JsonValue::Number);
    }
    if let Some(float) = obj.downcast_ref_if_exact::<rpy::builtins::PyFloat>(vm) {
        let value = float.to_f64();
        if !value.is_finite() {
            // `NaN`/`Infinity` spellings are stdlib-only.
            return None;
        }
        return JsonNumber::from_f64(value).map(JsonValue::Number);
    }
    if let Some(text) = obj.downcast_ref_if_exact::<rpy::builtins::PyStr>(vm) {
        return Some(JsonValue::String(text.as_str().to_owned()));
    }
    if let Some(list) = obj.downcast_ref_if_exact::<rpy::builtins::PyList>(vm) {
        if !enter_container(path, obj) {
            return None;
        }
        let converted = (|| {
            let elements = list.borrow_vec();
            let mut converted = Vec::with_capacity(elements.len());
            for element in elements.iter() {
                converted.push(py_to_json(vm, element, options, path, depth + 1)?);
            }
            Some(JsonValue::Array(converted))
        })();
        path.pop();
        return converted;
    }
    if let Some(tuple) = obj.downcast_ref_if_exact::<rpy::builtins::PyTuple>(vm) {
        if !enter_container(path, obj) {
            return None;
        }
        let converted = (|| {
            let elements = tuple.as_slice();
            let mut converted = Vec::with_capacity(elements.len());
            for element in elements {
                converted.push(py_to_json(vm, element, options, path, depth + 1)?);
            }
            Some(JsonValue::Array(converted))
        })();
        path.pop();
        return converted;
    }
    if obj
        .downcast_ref_if_exact::<rpy::builtins::PyDict>(vm)
        .is_some()
    {
        if !enter_container(path, obj) {
            return None;
        }
        let converted = (|| {
            let dict: rpy::builtins::PyDictRef = obj.clone().downcast().ok()?;
            let mut pairs: Vec<(String, JsonValue)> = Vec::with_capacity(dict.len());
            for (key, value) in dict {
                let key = if options.sort_keys {
                    // CPython sorts the original keys; only all-`str` maps
                    // have identical ordering after string coercion.
                    key.downcast_ref_if_exact::<rpy::builtins::PyStr>(vm)?
                        .as_str()
                        .to_owned()
                } else {
                    py_key_to_string(vm, &key)?
                };
                pairs.push((key, py_to_json(vm, &value, options, path, depth + 1)?));
            }
            if options.sort_keys {
                pairs.sort_by(|left, right| left.0.cmp(&right.0));
            }
            let mut map = JsonMap::with_capacity(pairs.len());
            for (key, value) in pairs {
                // Two Python keys can coerce to the same JSON key (e.g. 1 and
                // "1"); CPython emits duplicates, serde_json cannot.
                if map.insert(key, value).is_some() {
                    return None;
                }
            }
            Some(JsonValue::Object(map))
        })();
        path.pop();
        return converted;
    }
    // Value subclasses and arbitrary objects: stdlib encoder (or its error).
    None
}

/// Push a container's address onto the cycle path; `false` on a cycle.
fn enter_container(path: &mut Vec<*const rpy::PyObject>, obj: &rpy::PyObjectRef) -> bool {
    let address = obj.as_raw();
    if path.contains(&address) {
        return false;
    }
    path.push(address);
    true
}

/// Dict key coercion for `sort_keys=False` (CPython accepts str, int, bool and
/// None; float keys need `float.__repr__` and stay on the stdlib path).
fn py_key_to_string(vm: &rpy::VirtualMachine, key: &rpy::PyObjectRef) -> Option<String> {
    if vm.is_none(key) {
        return Some("null".to_owned());
    }
    if let Some(text) = key.downcast_ref_if_exact::<rpy::builtins::PyStr>(vm) {
        return Some(text.as_str().to_owned());
    }
    if key.class().is(vm.ctx.types.bool_type) {
        return Some(if key.is(&vm.ctx.true_value) {
            "true".to_owned()
        } else {
            "false".to_owned()
        });
    }
    if let Some(integer) = key.downcast_ref_if_exact::<rpy::builtins::PyInt>(vm) {
        return Some(integer.as_bigint().to_string());
    }
    None
}

/// `BigInt` decimal text -> JSON number, or `None` when it does not fit the
/// serde_json number model.
fn number_from_bigint(text: &str) -> Option<JsonNumber> {
    if let Ok(value) = text.parse::<i64>() {
        return Some(JsonNumber::from(value));
    }
    if let Ok(value) = text.parse::<u64>() {
        return Some(JsonNumber::from(value));
    }
    None
}

/// Serialize with the Python-compatible formatter.
fn serialize_json(value: JsonValue, options: &DumpOptions) -> Option<String> {
    use serde::Serialize as _;
    let mut buffer = Vec::new();
    {
        let formatter = PyJsonFormatter::new(options);
        let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
        value.serialize(&mut serializer).ok()?;
    }
    String::from_utf8(buffer).ok()
}

/// `serde_json` formatter matching CPython's separators, indentation and
/// `ensure_ascii` escapes (the default formatter would emit raw non-ASCII and
/// fixed separators).
struct PyJsonFormatter<'a> {
    ensure_ascii: bool,
    item_separator: &'a str,
    key_separator: &'a str,
    indent: Option<usize>,
    /// Container nesting while writing (1 = inside the outermost container).
    depth: usize,
    /// Whether the container being written got at least one value; mirrors
    /// `serde_json::ser::PrettyFormatter`'s single-flag bookkeeping.
    has_value: bool,
}

impl<'a> PyJsonFormatter<'a> {
    fn new(options: &'a DumpOptions) -> Self {
        Self {
            ensure_ascii: options.ensure_ascii,
            item_separator: &options.item_separator,
            key_separator: &options.key_separator,
            indent: options.indent,
            depth: 0,
            has_value: false,
        }
    }

    /// Newline plus `depth * indent` spaces (newline only for indent 0).
    fn newline_indent<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(b"\n")?;
        if let Some(width) = self.indent {
            const SPACES: &[u8; 64] =
                b"                                                                ";
            let mut left = width * self.depth;
            while left > 0 {
                let chunk = left.min(SPACES.len());
                writer.write_all(&SPACES[..chunk])?;
                left -= chunk;
            }
        }
        Ok(())
    }
}

impl<'a> serde_json::ser::Formatter for PyJsonFormatter<'a> {
    fn begin_array<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(b"[")
    }

    fn end_array<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth -= 1;
        if self.has_value && self.indent.is_some() {
            self.newline_indent(writer)?;
        }
        writer.write_all(b"]")
    }

    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if self.indent.is_some() {
            if !first {
                writer.write_all(self.item_separator.as_bytes())?;
            }
            self.newline_indent(writer)
        } else if first {
            Ok(())
        } else {
            writer.write_all(self.item_separator.as_bytes())
        }
    }

    fn end_array_value<W>(&mut self, _writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.has_value = true;
        Ok(())
    }

    fn begin_object<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(b"{")
    }

    fn end_object<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.depth -= 1;
        if self.has_value && self.indent.is_some() {
            self.newline_indent(writer)?;
        }
        writer.write_all(b"}")
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if self.indent.is_some() {
            if !first {
                writer.write_all(self.item_separator.as_bytes())?;
            }
            self.newline_indent(writer)
        } else if first {
            Ok(())
        } else {
            writer.write_all(self.item_separator.as_bytes())
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(self.key_separator.as_bytes())
    }

    fn end_object_value<W>(&mut self, _writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.has_value = true;
        Ok(())
    }

    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if !self.ensure_ascii {
            return writer.write_all(fragment.as_bytes());
        }
        // CPython's ensure_ascii escapes everything outside printable ASCII as
        // `\uXXXX` (surrogate pairs above the BMP, lowercase hex). serde_json
        // splits out control characters/quote/backslash before this hook, so
        // only printable ASCII and non-ASCII remain in `fragment`.
        let mut start = 0;
        for (index, character) in fragment.char_indices() {
            let code = character as u32;
            if (0x20..=0x7E).contains(&code) {
                continue;
            }
            writer.write_all(&fragment.as_bytes()[start..index])?;
            if code <= 0xFFFF {
                write!(writer, "\\u{code:04x}")?;
            } else {
                let code = code - 0x1_0000;
                let high = 0xD800 | ((code >> 10) & 0x3FF);
                let low = 0xDC00 | (code & 0x3FF);
                write!(writer, "\\u{high:04x}\\u{low:04x}")?;
            }
            start = index + character.len_utf8();
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

// ---------------------------------------------------------------------------
// Execution results
// ---------------------------------------------------------------------------

/// The result of `compile_and_run`: a finished value (eval mode), a clean
/// exec, or a failed run whose traceback was printed into the capture.
enum RunResult {
    Eval(rpy::PyObjectRef),
    Exec,
    Failed,
}

fn compile_and_run(
    vm: &rpy::VirtualMachine,
    scope: &rpy::scope::Scope,
    source: &str,
    eval_first: bool,
) -> RunResult {
    // Eval first (python tool): a single expression returns its value for the
    // repr echo. Anything else (statements, definitions, multiple lines)
    // falls back to exec mode; if both fail the exec-mode syntax error is
    // reported (users mostly send statements). The context-mode executor
    // asks for exec-only, so only explicit stdout is produced.
    if eval_first {
        if let Ok(code) = vm.compile(source, rpy::compiler::Mode::Eval, SOURCE_PATH.to_owned()) {
            return match vm.run_code_obj(code, scope.clone()) {
                Ok(value) => RunResult::Eval(value),
                Err(exc) => {
                    print_failure_traceback(vm, exc);
                    RunResult::Failed
                }
            };
        }
    }

    match vm.compile(source, rpy::compiler::Mode::Exec, SOURCE_PATH.to_owned()) {
        Ok(code) => match vm.run_code_obj(code, scope.clone()) {
            Ok(_value) => RunResult::Exec,
            Err(exc) => {
                print_failure_traceback(vm, exc);
                RunResult::Failed
            }
        },
        Err(err) => {
            let exc = vm.new_syntax_error(&err, Some(source));
            print_failure_traceback(vm, exc);
            RunResult::Failed
        }
    }
}

/// Collect captured output (and the traceback on failure) and restore the
/// std streams.
fn finish_job(vm: &rpy::VirtualMachine, capture: &Capture, run: RunResult) -> ExecOutcome {
    let (stdout, stdout_truncated) = take_buffer(&capture.stdout);
    let (stderr, stderr_truncated) = take_buffer(&capture.stderr);
    restore_capture(vm, capture);

    match run {
        RunResult::Failed => {
            // The traceback was printed into the captured stderr by
            // [`print_failure_traceback`] while the capture was installed;
            // partial stdout stays on the error result.
            ExecOutcome {
                stdout: capped_text(stdout, stdout_truncated),
                stderr: capped_text(stderr, stderr_truncated),
                error: true,
                eval_repr: None,
                duration_ms: 0,
            }
        }
        RunResult::Eval(value) => ExecOutcome {
            stdout: capped_text(stdout, stdout_truncated),
            stderr: capped_text(stderr, stderr_truncated),
            error: false,
            eval_repr: repr_if_not_none(vm, &value).map(cap_output),
            duration_ms: 0,
        },
        RunResult::Exec => ExecOutcome {
            stdout: capped_text(stdout, stdout_truncated),
            stderr: capped_text(stderr, stderr_truncated),
            error: false,
            eval_repr: None,
            duration_ms: 0,
        },
    }
}

/// Print the traceback into the captured stderr for a failed run. Must run
/// while the output capture is still installed (it writes through
/// `sys.stderr`), which is why it is called inside [`compile_and_run`] and
/// not in [`finish_job`].
fn print_failure_traceback(vm: &rpy::VirtualMachine, exc: rpy::builtins::PyBaseExceptionRef) {
    vm.print_exception(exc);
}

/// The eval-mode echo: the repr of the expression's value, suppressed for
/// `None` (matching the REPL, where bare statements produce nothing).
fn repr_if_not_none(vm: &rpy::VirtualMachine, value: &rpy::PyObjectRef) -> Option<String> {
    if vm.is_none(value) {
        return None;
    }
    // A top-level string expression echoes its VALUE (not the quoted repr):
    // the tool result is model-facing output, where `'alive'` quotes are
    // noise (mirrors how a print of the same value would read). Everything
    // else keeps the REPL-style repr echo.
    if value.class().name().to_string() == "str" {
        return match value.str(vm) {
            Ok(text) => Some(text.as_str().to_owned()),
            Err(_) => Some("<unrepresentable repr>".to_owned()),
        };
    }
    match value.repr(vm) {
        Ok(repr) => Some(repr.as_str().to_owned()),
        Err(_) => Some("<unrepresentable repr>".to_owned()),
    }
}

/// Char-boundary-safe truncation at [`MAX_OUTPUT_CHARS`].
pub(crate) fn cap_output(text: String) -> String {
    match text.char_indices().nth(MAX_OUTPUT_CHARS) {
        // At or under the cap: keep the text as-is.
        None => text,
        Some((byte_index, _)) => {
            let mut truncated = text[..byte_index].to_owned();
            truncated.push_str(TRUNCATION_NOTICE);
            truncated
        }
    }
}
