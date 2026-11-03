//! `worker_threads` host backend behind the fourth generic native entry
//! `__pir_host.worker(op, payloadJson) -> String` plus the async message-poll
//! entry `__pir_host.workerRecvPoll(id, waitMs, role) -> String`.
//!
//! Node semantics are "one isolate per Worker"; pir's equivalent is **one OS
//! thread plus one QuickJS runtime per Worker** (the rquickjs `!Send`
//! constraint makes one-runtime-many-threads impossible, and
//! one-isolate-many-realms is not a QuickJS capability). Each worker thread
//! runs the full extension actor pattern (`main_cli/js_mode.rs`): a
//! current-thread tokio runtime with a `LocalSet` and a complete
//! `ExtensionRuntime` boot (globals, prelude, node builtin sweep). Inside
//! the worker, `require("node:fs")`, `node:vm`, npm bare imports —
//! everything the parent can load — works the same way.
//!
//! Messages travel as JSON strings (structured-clone-minimal; the JS shim
//! enforces the clone contract) through per-worker mailboxes:
//!
//! ```text
//! parent JS --parentSend--> [inbound]  --workerRecvPoll("worker")--> worker JS
//! worker JS --workerSend--> [outbound] --workerRecvPoll("parent")--> parent JS
//! ```
//!
//! The parent JS `Worker` and the worker-side boot (`worker_boot.js.txt`)
//! each run a pump loop awaiting `workerRecvPoll`; the poll parks on the
//! mailbox `Notify` with a short timer fallback (the executor-interleaving
//! pattern from `context_fns::waitForIdle`), so a wakeup can never be lost.
//!
//! Lifecycle: `workerExit` (worker's `process.exit`) and `terminate` both set
//! the shared shutdown flag; the worker pump observes it, the thread pushes
//! its final `exit` record, sets `done`, and the parent releases the registry
//! entry once it has observed the exit.
//!
//! A worker spinning in a pure JS loop (never awaiting) also has to stop:
//! the worker runtime installs the shared [`WorkerShared::abort`] flag as its
//! QuickJS interrupt flag ([`ExtensionRuntime::set_interrupt_flag`]), and
//! `terminate` / [`release_generation`] set it synchronously. The engine's
//! interrupt handler then aborts the eval at the next bytecode boundary, so
//! `terminate` no longer depends on the JS pump observing `shutdown`. The
//! worker script's own synchronous body additionally runs under the parent
//! runtime's configured call deadline ([`ExtensionRuntime::arm_deadline`]),
//! so a `while (true) {}` that nobody terminates dies on the timeout instead
//! of pinning a thread forever.
//!
//! Boundary: a QuickJS interrupt is delivered between bytecode instructions,
//! not inside a native host function. A worker blocked in an uninterruptible
//! synchronous host call (`fs.readFileSync` on a stalled FIFO, a `spawnSync`
//! `wait`, ...) still cannot be killed; such a thread is counted as orphaned
//! after its generation is released and the process-wide live-thread cap
//! ([`MAX_LIVE_WORKER_THREADS`]) refuses new workers instead of letting a
//! `/reload` loop accumulate threads without bound. The runtime's own Drop
//! evaluates `process.emit("exit")`; [`AbortWorkerOnDrop`] sets the same
//! interrupt flag first, so a synchronous infinite loop in a user exit
//! handler is bounded by the bytecode poll instead of pinning the thread.
//!
//! Queue bounds: each mailbox (inbound and outbound) holds at most
//! [`MAX_MAILBOX_BYTES`] of message text. Node's MessagePort queues without
//! bound and has no backpressure API; because each direction's sender is the
//! JS thread that the OTHER direction's pump runs on, parking a sender until
//! the receiver polls can deadlock the two pumps against each other — so the
//! bound drops the newest DATA message and reports it loudly (counter +
//! `ext_diag!` + the `dropped` field of both the posting answer and the
//! `workerRecvPoll` envelope). Control records (`error`/`exit`) are exempt.

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::extensions::api::embedded::Shim;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rquickjs::promise::MaybePromise;
use rquickjs::Object;
use serde_json::Value as JsonValue;

use super::*;

use crate::extensions::{ExtensionConfig, ExtensionLimits, ExtensionRuntime};

/// Stack for a worker thread: the QuickJS runtime + swc transpile recurse on
/// deep module graphs, so match the `pir js` script-thread budget
/// (`main_cli::js_mode::SCRIPT_THREAD_STACK`, itself 64 MiB).
const WORKER_THREAD_STACK: usize = 64 * 1024 * 1024;

/// Hard cap on `worker_threads.Worker` registry entries for the process.
///
/// The registry (`WORKERS`) is process-global and entries are removed when a
/// worker exits or its generation is released, so with one live generation
/// this reads as the per-generation worker cap. It is not a live-OS-thread
/// bound: [`MAX_LIVE_WORKER_THREADS`] is the registry-independent cap that
/// also covers threads whose generation was already released.
///
/// Each Worker is one OS thread plus a full QuickJS runtime; a JS loop that
/// creates Workers without terminating them would otherwise exhaust threads
/// and address space (64 MiB of reserved stack per worker). Node has no
/// explicit default cap, so this is a host safety bound, not a parity value:
/// 64 admits realistic parallel workloads (the subagent parser uses a handful)
/// while keeping the worst case bounded. Beyond it `new Worker` throws the
/// ordinary error the shim surfaces.
const MAX_WORKER_THREADS: usize = 64;

/// Process-wide hard cap on OS threads actually alive, **across
/// generations**.
///
/// [`MAX_WORKER_THREADS`] bounds only the registry of the current runtime
/// generation. `/reload` removes the old entries from the registry immediately
/// (`release_generation`), so without a second, registry-independent bound the
/// same process could spawn 64 more workers after every reload — each one a
/// 64 MiB-stack thread plus a QuickJS heap — and a reload loop could grow the
/// leak without limit. Two generations' worth is enough for a successor to
/// start while the predecessor drains; once the count reaches this bound,
/// `op_start` refuses to spawn more until orphaned threads exit.
const MAX_LIVE_WORKER_THREADS: usize = MAX_WORKER_THREADS * 2;

/// JS stack requested for the worker's QuickJS runtime.
///
/// The worker runs far deeper JS recursion than a plain extension runtime
/// (pi-subagents parses workflow scripts with acorn's recursive-descent
/// parser in here), so it asks for the crate's clamped maximum. The request
/// must never exceed rquickjs's own `MAX_SANE` bound, because the engine's
/// mapping for an oversized request is SILENT:
/// `rquickjs-core-0.14.0/src/runtime/raw.rs` does
/// `const MAX_SANE: usize = 1 << 24; if limit > MAX_SANE { 0 }`, and QuickJS
/// (`quickjs.c::update_stack_limit`) reads `0` as `stack_limit = 0`, i.e.
/// "no limit": the check that turns runaway recursion into a catchable
/// `RangeError` is then OFF, deep recursion overflows the real 64 MiB OS
/// thread stack (`WORKER_THREAD_STACK`) and aborts the whole process. Ask
/// for `MAX_STACK_BYTES` (16 MiB) and leave the rest as native headroom; the
/// assertion below keeps the constant from ever being raised past the
/// engine's cutoff.
pub const WORKER_JS_STACK_BYTES: usize = crate::extensions::limits::MAX_STACK_BYTES;

/// A copy of rquickjs's `MAX_SANE` (16 MiB): `set_max_stack_size` rewrites a
/// larger request to `0`, which disables the QuickJS stack check instead of
/// reporting an error (see [`WORKER_JS_STACK_BYTES`]).
const RQUICKJS_MAX_SANE_STACK_BYTES: usize = 1 << 24;

// Compile-time guard: a worker stack request above rquickjs's `MAX_SANE`
// would be read as "no stack check" (the trap is silent), so it must not
// build. Also catches a `MAX_STACK_BYTES` raised past 16 MiB in
// `crate::extensions::limits`.
const _: () = assert!(WORKER_JS_STACK_BYTES <= RQUICKJS_MAX_SANE_STACK_BYTES);

/// Upper bound of one `workerRecvPoll` wait (the JS pumps ask for less).
const MAX_POLL_WAIT_MS: u64 = 1000;

/// Base of the notify fallback period: the executor can lose a parked notify
/// wakeup when an interleaved `async_with` suspends the poll future, so every
/// wait is also timer-bounded (the `waitForIdle` pattern).
const POLL_FALLBACK_MS: u64 = 25;

/// Ceiling of the adaptive poll fallback: consecutive idle slices double from
/// [`POLL_FALLBACK_MS`] up to this bound (the same backoff `runtime.rs` applies
/// to its drain park), so an idle worker mailbox is re-checked at most once a
/// second instead of 40 times. A delivered message resets the slice.
const POLL_FALLBACK_MAX_MS: u64 = MAX_POLL_WAIT_MS;

/// Next fallback slice after one wait: reset on a delivered message, double
/// (up to the ceiling) when the wait timed out.
fn next_poll_slice(current_ms: u64, had_message: bool) -> u64 {
    if had_message {
        return POLL_FALLBACK_MS;
    }
    current_ms.saturating_mul(2).min(POLL_FALLBACK_MAX_MS)
}

/// Worker-side boot overlay (`worker_boot.js.txt`): worker id / threadId /
/// workerData placeholders are replaced with JSON literals before eval.
static WORKER_BOOT: Shim = Shim::new(
    "worker_boot.js.txt",
    include_bytes!("../../../../assets/shims/worker_boot.js.txt.deflate"),
);

/// File-backed shims embedded by this module; `api::all_shims` aggregates it
/// with the `api` shim manifests for the byte-identity test.
pub(crate) const SHIMS: &[(&str, &Shim)] = &[("worker_boot.js.txt", &WORKER_BOOT)];

/// Bound of queued message TEXT per mailbox direction, in bytes.
///
/// Node's `port.postMessage` queues structured clones without bound (there is
/// no backpressure API on a MessagePort), so any bound is a divergence and has
/// to be the loud kind. The bound cannot be a park: `parentSend` runs on the
/// parent runtime's JS thread and `workerSend` on the worker's, while the
/// DRAIN of each mailbox runs on the *other* thread's pump — parking the
/// sender until the receiver polls would let two busy pumps deadlock each other
/// (parent parked on inbound room, worker parked on outbound room), and a
/// worker spinning in synchronous JS never pumps at all. So the mailbox caps
/// instead: 8 MiB of message text is a few hundred typical JSON payloads (the
/// pi-subagents scripted-workflow protocol posts small objects), i.e. a
/// generous window, and past it the NEWEST data message is dropped and
/// counted (in-order delivery of the accepted prefix is preserved; oldest-drop
/// would instead discard the start of a protocol exchange). Control records
/// (`error` / `exit`) are exempt: they end the pump and must never be lost.
/// Every drop is counted ([`Mailbox::dropped`]), reported through `ext_diag!`
/// on the [`MAILBOX_DROP_REPORT_EVERY`] cadence, and the running total rides
/// the `workerRecvPoll` envelope (and the posting answer once it is nonzero) —
/// never silent.
const MAX_MAILBOX_BYTES: usize = 8 * 1024 * 1024;

/// Drop-report cadence: the first dropped message is reported, then one
/// diagnostic per this many further drops (identical repeats are additionally
/// collapsed by the logging router).
const MAILBOX_DROP_REPORT_EVERY: u64 = 1024;

// ---------------------------------------------------------------------------
// Worker registry
// ---------------------------------------------------------------------------

/// One direction's message queue: the queued JSON texts, their total size (the
/// quantity [`MAX_MAILBOX_BYTES`] bounds) and the drop counter.
#[derive(Default)]
struct Mailbox {
    queue: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

impl Mailbox {
    /// Append one message, or drop it because the mailbox is at its bound.
    /// Returns whether the message was accepted.
    fn push(&mut self, text: String) -> bool {
        if self.bytes + text.len() > MAX_MAILBOX_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.bytes += text.len();
        self.queue.push_back(text);
        true
    }

    /// Append one control record (`error`/`exit`): the bound does not apply,
    /// because these are the records that END the pump — dropping one would
    /// leave the peer waiting forever instead of reporting the exit.
    fn push_control(&mut self, text: String) {
        self.bytes += text.len();
        self.queue.push_back(text);
    }

    fn drain(&mut self) -> Vec<String> {
        self.bytes = 0;
        self.queue.drain(..).collect()
    }
}

/// Cross-thread view of one worker's thread lifetime. `release_generation`
/// marks a still-running worker orphaned; the worker's exit guard records
/// `exited` and retires the orphan mark. Both transitions hold the same
/// mutex, so the orphan counter cannot be incremented after its thread has
/// already returned (or decremented twice).
#[derive(Default)]
struct WorkerLifecycle {
    exited: bool,
    orphaned: bool,
}

/// Why a worker was asked to stop. [`WorkerShared::shutdown`] alone cannot be
/// turned into an exit code: the worker's own `process.exit(code)` also sets
/// it, and that code must win over Node's "terminated" code 1. The terminal
/// path therefore records the reason (as an atomic, written before `shutdown`)
/// and [`WorkerShared::finalize_exit_code`] resolves the reported code from it.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum StopReason {
    /// No stop requested yet.
    None = 0,
    /// The worker called `process.exit`; `WorkerShared::exit_code` holds the
    /// requested code (including a legitimate 0).
    ProcessExit = 1,
    /// The parent called `Worker.terminate()`.
    Terminate = 2,
    /// The owning runtime generation was released (`/reload`, shutdown, drop).
    GenerationRelease = 3,
}

impl StopReason {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::ProcessExit,
            2 => Self::Terminate,
            3 => Self::GenerationRelease,
            _ => Self::None,
        }
    }
}

/// One live worker's mailboxes + lifecycle flags, shared between the parent
/// runtime thread, the worker thread, and the host ops.
struct WorkerShared {
    /// Owning runtime generation: a reload/shutdown shuts down exactly the
    /// workers its own runtime started (a successor's workers keep running).
    generation: u64,
    /// Parent -> worker message queue (raw JSON strings).
    inbound: StdMutex<Mailbox>,
    /// Worker -> parent message queue (raw JSON strings: data payloads plus
    /// the `{kind:"error"|"exit", ...}` control records).
    outbound: StdMutex<Mailbox>,
    /// Wakes a parked `workerRecvPoll` on the receiving side.
    inbound_notify: tokio::sync::Notify,
    outbound_notify: tokio::sync::Notify,
    /// Set by `terminate`, generation release, or the worker's `process.exit`;
    /// every poll returns `stop` afterwards.
    shutdown: AtomicBool,
    /// Why the stop was requested; written before [`Self::shutdown`] so the
    /// worker's terminal path can distinguish a forced stop (Node reports 1)
    /// from a normal completion (`process.exit` supplies its own code).
    stop_reason: AtomicU8,
    /// Cooperative interrupt flag installed into THIS worker runtime's
    /// QuickJS interrupt handler. Unlike [`Self::shutdown`] it is observed by
    /// a synchronous JS loop at the next bytecode boundary, which is what
    /// makes `terminate`/`/reload` able to stop `while (true) {}`. Kept as an
    /// `Arc` so the worker runtime's handler can borrow it without touching
    /// the registry.
    abort: Arc<AtomicBool>,
    /// Workers whose generation was released while their thread was still
    /// alive. Guarded by the shared mutex: the release path and the thread
    /// exit guard must agree on whether the thread was still running, and
    /// increment/decrement [`ORPHANED_WORKERS`] exactly once.
    lifecycle: StdMutex<WorkerLifecycle>,
    /// Set by the worker thread after the final exit code and `exit` record
    /// are published; the parent releases the registry entry once it observes
    /// this flag (or the corresponding `exit` answer).
    done: AtomicBool,
    /// Final exit code. `workerExit` (the worker's `process.exit`) writes it
    /// first when the worker chose its own code; otherwise the terminal path
    /// resolves it from [`Self::stop_reason`] (see
    /// [`Self::finalize_exit_code`]). Read by `recv_poll`, `op_terminate` and
    /// the `exit` control record so all three agree.
    exit_code: StdMutex<Option<i32>>,
    /// Test/observability count of timer-fallback wakes in `recv_poll` (a
    /// delivered message does not count). Pins the adaptive park: an idle
    /// mailbox must wake once per (growing) slice, never on a fixed 25 ms
    /// timer.
    poll_wakes: AtomicU64,
}

impl WorkerShared {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            inbound: StdMutex::new(Mailbox::default()),
            outbound: StdMutex::new(Mailbox::default()),
            inbound_notify: tokio::sync::Notify::new(),
            outbound_notify: tokio::sync::Notify::new(),
            shutdown: AtomicBool::new(false),
            stop_reason: AtomicU8::new(StopReason::None as u8),
            abort: Arc::new(AtomicBool::new(false)),
            lifecycle: StdMutex::new(WorkerLifecycle::default()),
            done: AtomicBool::new(false),
            exit_code: StdMutex::new(None),
            poll_wakes: AtomicU64::new(0),
        }
    }

    fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolve and persist this worker's final exit code. The first caller
    /// wins: `workerExit` (the worker's `process.exit`) stores its code
    /// before any terminal path runs, so a legitimate `process.exit(0)` is
    /// preserved even when a `terminate` races it. Otherwise a forced stop
    /// (`Worker.terminate()` or a generation release) reports Node's 1, and a
    /// natural terminal path keeps the caller's default (0 for a completed
    /// pump). Every reader — `recv_poll`, `op_terminate`, and the `exit`
    /// control record — sees this one slot, so a terminate racing an idle
    /// worker cannot report 0 through one path and 1 through another.
    fn finalize_exit_code(&self, natural: i32) -> i32 {
        let mut slot = Self::lock(&self.exit_code);
        if let Some(code) = *slot {
            return code;
        }
        let code = match StopReason::from_u8(self.stop_reason.load(Ordering::SeqCst)) {
            StopReason::Terminate | StopReason::GenerationRelease => 1,
            StopReason::None | StopReason::ProcessExit => natural,
        };
        *slot = Some(code);
        code
    }
}

static WORKERS: OnceLock<StdMutex<HashMap<u64, Arc<WorkerShared>>>> = OnceLock::new();
static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);
static ENV_DATA: OnceLock<StdMutex<HashMap<String, String>>> = OnceLock::new();
/// OS worker threads currently alive (spawned but not yet returned). This is
/// registry-independent: `release_generation` removes registry entries long
/// before a wedged thread exits, and only this count bounds cross-generation
/// accumulation.
static LIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
/// Live worker threads whose owning generation was released before they
/// exited (registry entries gone, thread still running). Observability for
/// the `/reload` regression; always a subset of [`LIVE_WORKERS`]. This counts
/// only the [`release_generation`] path: a worker detached by the shim's
/// terminate-timeout `release` op still occupies a [`LIVE_WORKERS`] slot (and
/// the hard cap) but is not included here.
static ORPHANED_WORKERS: AtomicUsize = AtomicUsize::new(0);
/// Test-only override for [`MAX_LIVE_WORKER_THREADS`] (0 = the constant).
static LIVE_WORKER_LIMIT_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

fn live_worker_limit() -> usize {
    let override_limit = LIVE_WORKER_LIMIT_OVERRIDE.load(Ordering::SeqCst);
    if override_limit == 0 {
        MAX_LIVE_WORKER_THREADS
    } else {
        override_limit
    }
}

/// Reserve one live-thread slot, or report the cap. The reservation is
/// released by [`LiveWorkerReservation`] on every early-return path and, once
/// the thread starts, by the thread's own exit guard.
fn reserve_live_worker_slot() -> bool {
    let limit = live_worker_limit();
    LIVE_WORKERS
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
            (live < limit).then_some(live + 1)
        })
        .is_ok()
}

/// Reservation guard for the spawn path: dropping it before the thread takes
/// ownership restores the counter (failed source read, registry insert, or
/// `thread::Builder::spawn`).
struct LiveWorkerReservation {
    active: bool,
}

impl LiveWorkerReservation {
    fn new() -> Option<Self> {
        let reserved = reserve_live_worker_slot();
        // `bool::then_some` constructs its argument EAGERLY, so
        // `reserved.then_some(Self { active: true })` built (and immediately
        // dropped) a decrementing guard even on a REJECTED reservation. That
        // made every cap rejection subtract a slot and drove `LIVE_WORKERS`
        // below zero (wrapping to `usize::MAX`, so the next reservation
        // overflowed). The guard must exist only when the increment happened.
        if reserved {
            Some(Self { active: true })
        } else {
            None
        }
    }

    /// Hand the reserved slot to the worker thread's exit guard.
    fn transfer(mut self) {
        self.active = false;
    }
}

impl Drop for LiveWorkerReservation {
    fn drop(&mut self) {
        if self.active {
            LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Per-thread exit guard: decrements the live count and clears the orphan
/// marker when the worker function returns (normally, by an interrupt, or by
/// panic unwinding through `catch_unwind`).
struct WorkerThreadSlot {
    shared: Arc<WorkerShared>,
}

impl Drop for WorkerThreadSlot {
    fn drop(&mut self) {
        // Retire the orphan mark before the live count so the orphan total is
        // never transiently larger than the live total.
        let mut lifecycle = WorkerShared::lock(&self.shared.lifecycle);
        lifecycle.exited = true;
        if lifecycle.orphaned {
            lifecycle.orphaned = false;
            ORPHANED_WORKERS.fetch_sub(1, Ordering::SeqCst);
        }
        drop(lifecycle);
        LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn workers() -> &'static StdMutex<HashMap<u64, Arc<WorkerShared>>> {
    WORKERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn env_data() -> &'static StdMutex<HashMap<String, String>> {
    ENV_DATA.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn worker_by_id(id: u64) -> Option<Arc<WorkerShared>> {
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
}

/// Shut down every worker started by `generation` (runtime drop or actor
/// `Shutdown`). The worker thread observes the shutdown flag at its next pump
/// iteration; a thread spinning in pure synchronous JS is aborted through the
/// worker runtime's QuickJS interrupt flag (`WorkerShared::abort`), which is
/// independent of the message pump. The registry entry is removed immediately
/// (the parent pump sees `stop`/`done` instead of a dead registry entry) and
/// the thread's own `Arc` keeps the mailboxes alive until it exits.
///
/// A thread that does not exit (stuck inside a native host function, which a
/// QuickJS interrupt cannot preempt) is marked orphaned: [`ORPHANED_WORKERS`]
/// records it and [`MAX_LIVE_WORKER_THREADS`] keeps repeated `/reload`s from
/// accumulating a fresh generation of leaked threads.
pub(super) fn release_generation(generation: u64) {
    let stale: Vec<Arc<WorkerShared>> = {
        let mut table = workers().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, shared)| shared.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for shared in stale {
        // Record the reason before the flag the worker's terminal path reads,
        // so the exit record cannot be built with a stale "normal" reason.
        shared
            .stop_reason
            .store(StopReason::GenerationRelease as u8, Ordering::SeqCst);
        shared.shutdown.store(true, Ordering::SeqCst);
        shared.abort.store(true, Ordering::SeqCst);
        {
            let mut lifecycle = WorkerShared::lock(&shared.lifecycle);
            // `exited` is set by the worker's exit guard under this same
            // mutex: a thread that already returned is never counted as
            // orphaned, and a thread marked here retires the count when it
            // returns.
            if !lifecycle.exited && !lifecycle.orphaned {
                lifecycle.orphaned = true;
                ORPHANED_WORKERS.fetch_add(1, Ordering::SeqCst);
            }
        }
        shared.inbound_notify.notify_waiters();
        shared.outbound_notify.notify_waiters();
        shared.inbound_notify.notify_one();
        shared.outbound_notify.notify_one();
    }
}

/// Number of live worker handles (test seam for the generation release).
#[doc(hidden)]
pub fn worker_count_for_tests() -> usize {
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
}

/// Process-wide live-worker cap (test seam: the constant is the contract).
#[doc(hidden)]
pub fn worker_limit_for_tests() -> usize {
    MAX_WORKER_THREADS
}

/// Registry-independent count of OS worker threads still alive.
#[doc(hidden)]
pub fn live_worker_count_for_tests() -> usize {
    LIVE_WORKERS.load(Ordering::SeqCst)
}

/// Process-wide live-thread hard cap (across generations).
#[doc(hidden)]
pub fn live_worker_limit_for_tests() -> usize {
    MAX_LIVE_WORKER_THREADS
}

/// Workers whose generation was released while their thread was still alive.
#[doc(hidden)]
pub fn orphaned_worker_count_for_tests() -> usize {
    ORPHANED_WORKERS.load(Ordering::SeqCst)
}

/// Override the cross-generation live cap for one test (`0` restores the
/// constant). Tests must save/restore it around the assertion.
#[doc(hidden)]
pub fn test_set_live_worker_limit(limit: usize) {
    LIVE_WORKER_LIMIT_OVERRIDE.store(limit, Ordering::SeqCst);
}

/// Everything the worker thread needs to boot its runtime. `Send` by design:
/// this crosses the thread boundary (the rquickjs runtime itself never does).
struct WorkerSpec {
    id: u64,
    code: String,
    from_dir: String,
    worker_data: String,
    filename: String,
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    /// The parent runtime's host `mode`/`has_ui`, inherited so the worker's
    /// own stdio routing matches the host it runs inside: a worker is an OS
    /// thread of THIS process, so `process.stderr.write` from worker code
    /// reaches the same stdio the parent writes to (in a TUI, the frame)
    /// unless it carries the parent's interactive host identity.
    host_mode: String,
    has_ui: bool,
    /// The parent runtime's resolved engine limits: the worker is a second
    /// QuickJS runtime in the same process, so it must observe the same heap
    /// cap (and stack/timeout policy) instead of re-resolving global settings
    /// independently.
    limits: Option<ExtensionLimits>,
}

// ---------------------------------------------------------------------------
// Registration + dispatch
// ---------------------------------------------------------------------------

pub(super) fn register_worker_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    // The owning runtime's generation (see `release_generation`).
    generation: u64,
) -> rquickjs::Result<()> {
    let state = state.clone();
    let allowed_roots = allowed_roots.clone();
    host.set(
        "worker",
        Func::from(move |op: String, payload: String| -> String {
            worker_op(&state, &allowed_roots, &op, &payload, generation)
        }),
    )?;
    host.set(
        "workerRecvPoll",
        Func::from(JsAsync(
            move |id: f64, wait_ms: f64, role: String| async move {
                let id = if id.is_finite() && id >= 0.0 {
                    id as u64
                } else {
                    0
                };
                let wait_ms = if wait_ms.is_finite() && wait_ms > 0.0 {
                    (wait_ms as u64).min(MAX_POLL_WAIT_MS)
                } else {
                    0
                };
                recv_poll(id, wait_ms, &role).await
            },
        )),
    )?;
    Ok(())
}

/// Payload string field.
fn payload_str<'a>(payload: &'a JsonValue, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(JsonValue::as_str)
}

/// Result envelope of one `worker(op, payload)` call. Serializing a struct
/// writes the fixed keys straight from `&'static str`s, where the `json!` macro
/// it replaces built a throwaway `Map` (table + entry slots) plus one heap
/// `String` per key before serializing that tree.
#[derive(serde::Serialize)]
struct OpOk {
    ok: bool,
    value: JsonValue,
}

#[derive(serde::Serialize)]
struct OpErr<'a> {
    ok: bool,
    code: &'a str,
    message: &'a str,
}

/// One worker-op failure: the envelope code plus the message. Most ops report
/// the generic [`WORKER_OP_ERROR`]; `start` forwards the Node error code
/// (e.g. `E2BIG` for a Worker entry over the shared 32 MiB source cap) so the
/// async `error` event carries the same code as `fs.readFile`.
struct OpError {
    code: &'static str,
    message: String,
}

const WORKER_OP_ERROR: &str = "ERR_WORKER_OP";

impl From<String> for OpError {
    fn from(message: String) -> Self {
        Self {
            code: WORKER_OP_ERROR,
            message,
        }
    }
}

impl OpError {
    fn coded(code: &'static str, message: String) -> Self {
        Self { code, message }
    }
}

/// One op's answer. Every op but the two message posts builds a `JsonValue`
/// for [`worker_op`] to wrap in the envelope; the posts answer with their
/// *finished* envelope text ([`SENT_OK`] / [`DROPPED_OK`], fixed per op), so
/// posting one message no longer builds a throwaway one-entry `Map` plus its
/// key `String` before that envelope is serialized.
enum OpAnswer {
    Json(JsonValue),
    Wire(&'static str),
}

impl From<JsonValue> for OpAnswer {
    fn from(value: JsonValue) -> Self {
        Self::Json(value)
    }
}

fn worker_op(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    op: &str,
    payload: &str,
    generation: u64,
) -> String {
    let mut payload: JsonValue = serde_json::from_str(payload).unwrap_or(JsonValue::Null);
    let result = match op {
        "start" => op_start(state, allowed_roots, &payload, generation).map(OpAnswer::from),
        "parentSend" => op_send(&mut payload, "inbound").map_err(OpError::from),
        "workerSend" => op_send(&mut payload, "outbound").map_err(OpError::from),
        "workerExit" => op_worker_exit(&payload)
            .map(OpAnswer::from)
            .map_err(OpError::from),
        "terminate" => op_terminate(&payload)
            .map(OpAnswer::from)
            .map_err(OpError::from),
        "state" => op_state(&payload)
            .map(OpAnswer::from)
            .map_err(OpError::from),
        "release" => op_release(&payload)
            .map(OpAnswer::from)
            .map_err(OpError::from),
        "envGet" => op_env_get(&payload)
            .map(OpAnswer::from)
            .map_err(OpError::from),
        "envSet" => op_env_set(&payload)
            .map(OpAnswer::from)
            .map_err(OpError::from),
        other => Err(format!("unknown worker op `{other}`").into()),
    };
    match result {
        // Infallible: `OpOk` is a `bool` plus a `serde_json::Value`, and the
        // writer is an in-memory `Vec<u8>` — `serde_json`'s only error
        // sources (writer I/O, a non-string map key, a fallible custom
        // `Serialize` impl) are absent, and `Value`'s own `Serialize` only
        // calls entry points the built-in serializer implements infallibly.
        // `expect` replaces the `{"ok":true,"value":null}` fallback, which
        // would have reported success while discarding the op's value.
        Ok(OpAnswer::Json(value)) => serde_json::to_string(&OpOk { ok: true, value })
            .expect("OpOk serialization is infallible: bool + Value, in-memory writer"),
        // The posts hand back their envelope text ready-made.
        Ok(OpAnswer::Wire(text)) => text.to_owned(),
        // Same proof as `OpOk` (a `bool` plus two `&str`s, no map): the old
        // fallback could blank out `message`, hiding the operator's error.
        Err(error) => serde_json::to_string(&OpErr {
            ok: false,
            code: error.code,
            message: error.message.as_str(),
        })
        .expect("OpErr serialization is infallible: bool + two &str, in-memory writer"),
    }
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

fn op_start(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
    generation: u64,
) -> Result<JsonValue, OpError> {
    if workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
        >= MAX_WORKER_THREADS
    {
        return Err(
            format!("too many worker threads: the process limit is {MAX_WORKER_THREADS}").into(),
        );
    }
    // Reserve before reading/spawning: the reservation also enforces the
    // cross-generation cap, so a `/reload` loop cannot keep growing the
    // process while the previous generation's threads are still alive.
    let Some(reservation) = LiveWorkerReservation::new() else {
        return Err(format!(
            "too many live worker threads: {} are still running across generations (process limit {})",
            LIVE_WORKERS.load(Ordering::SeqCst),
            live_worker_limit()
        )
        .into());
    };
    let eval = payload
        .get("eval")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let code = payload_str(payload, "code").unwrap_or("").to_owned();
    let worker_data = payload_str(payload, "workerData")
        .unwrap_or("null")
        .to_owned();
    let filename = payload_str(payload, "filename").unwrap_or("").to_owned();
    let (code, from_dir, filename) = if eval {
        if code.trim().is_empty() {
            return Err("worker eval source must be a non-empty string"
                .to_owned()
                .into());
        }
        let cwd = lock_state(state).config.cwd.display().to_string();
        (code, cwd, "[worker eval]".to_owned())
    } else {
        if filename.trim().is_empty() {
            return Err("worker filename must be a non-empty string"
                .to_owned()
                .into());
        }
        // Worker entry sources share `fs.readFile`'s 32 MiB cap: a Worker is
        // one more QuickJS runtime in this process, so an unbounded entry
        // read is an unbounded host-heap allocation.
        let source =
            super::node_fns::read_text_capped(std::path::Path::new(&filename), "open", &filename)
                .map_err(|error| {
                let (code, message) = super::node_fns::node_error_parts(error);
                let message = if code == "ENOENT" {
                    format!("Cannot find module '{filename}': {message}")
                } else {
                    message
                };
                OpError::coded(code, message)
            })?;
        let dir = std::path::Path::new(&filename)
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        (source, dir, filename)
    };

    // The worker loads modules through the same loader chain as the parent:
    // snapshot the parent's allowed roots plus the agent npm root (the
    // `pir js` runner's rule), so npm dependencies of the worker code —
    // `acorn` for pi-subagents' workflow parser — resolve and load.
    let mut roots: Vec<PathBuf> = allowed_roots
        .lock()
        .map(|roots| roots.clone())
        .unwrap_or_default();
    let agent_npm = crate::ai::providers::agent_dir().join("npm");
    if !roots.contains(&agent_npm) {
        roots.push(agent_npm);
    }
    // cwd + the parent's host identity, which the worker's stdio routing
    // inherits (see `WorkerSpec`).
    let (cwd, host_mode, has_ui, limits) = {
        let guard = lock_state(state);
        (
            guard.config.cwd.clone(),
            guard.config.mode.clone(),
            guard.config.has_ui,
            guard.config.limits.clone(),
        )
    };

    let id = NEXT_WORKER_ID.fetch_add(1, Ordering::SeqCst);
    let shared = Arc::new(WorkerShared::new(generation));
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&shared));

    let spec = WorkerSpec {
        id,
        code,
        from_dir,
        worker_data,
        filename,
        cwd,
        roots,
        host_mode,
        has_ui,
        limits,
    };
    let spawned = std::thread::Builder::new()
        .name(format!("pir-worker-{id}"))
        .stack_size(WORKER_THREAD_STACK)
        .spawn(move || worker_thread_main(spec, shared));
    match spawned {
        Ok(_handle) => {
            // The thread's `WorkerThreadSlot` guard now owns the live-count
            // slot; the local reservation must not release it a second time.
            reservation.transfer();
            Ok(serde_json::json!({ "id": id, "threadId": id }))
        }
        Err(error) => {
            workers()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            // Dropping `reservation` releases the slot.
            Err(format!("failed to spawn worker thread: {error}").into())
        }
    }
}

/// Wire answer of a post that enqueued the message. The text is exactly what
/// `OpOk { ok: true, value }` with `value = json!({ "sent": true })` serialized
/// (`preserve_order` keeps the macro's insertion order and the derived
/// `Serialize` writes the fields in declaration order), so the two shapes move
/// together.
const SENT_OK: &str = r#"{"ok":true,"value":{"sent":true}}"#;
/// Wire answer of a post dropped because the worker already exited.
const DROPPED_OK: &str = r#"{"ok":true,"value":{"dropped":true}}"#;

/// Push one data message into the worker's inbound (parent -> worker) or the
/// parent's outbound (worker -> parent) queue. The `data` field carries the
/// already-serialized JSON text of the posted value.
///
/// The payload is taken by `&mut` so the message body can be *moved* into the
/// mailbox: `data` is consumed exactly once, and the `.to_owned()` this
/// replaces copied the whole (unescaped) message text a second time on every
/// post.
///
/// A mailbox at [`MAX_MAILBOX_BYTES`] drops the message (see that constant: a
/// parked sender could deadlock the two pumps against each other) and answers
/// with the running drop total, which is only materialized in the envelope
/// once the counter is nonzero — the accepted path keeps the fixed wire text.
fn op_send(payload: &mut JsonValue, side: &str) -> Result<OpAnswer, String> {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or("missing worker id")?;
    // `mem::take` leaves an empty `String` in the (soon dropped) envelope —
    // no allocation, and non-string `data` falls through to the same error as
    // before.
    let data = match payload.get_mut("data") {
        Some(JsonValue::String(text)) => std::mem::take(text),
        _ => return Err("missing data payload".to_string()),
    };
    let Some(shared) = worker_by_id(id) else {
        // A message to an exited worker is dropped (Node fails it with
        // ERR_WORKER_NOT_RUNNING when the port is gone; the shim layer keeps
        // postMessage best-effort silent).
        return Ok(OpAnswer::Wire(DROPPED_OK));
    };
    if shared.shutdown.load(Ordering::SeqCst) {
        return Ok(OpAnswer::Wire(DROPPED_OK));
    }
    let len = data.len();
    let (mailbox, notify, direction) = match side {
        "inbound" => (&shared.inbound, &shared.inbound_notify, "parent->worker"),
        _ => (&shared.outbound, &shared.outbound_notify, "worker->parent"),
    };
    let (accepted, dropped) = {
        let mut mailbox = WorkerShared::lock(mailbox);
        let accepted = mailbox.push(data);
        (accepted, mailbox.dropped)
    };
    if !accepted {
        report_mailbox_drop(id, direction, len, dropped);
        // Same envelope shape as `SENT_OK` plus the drop accounting: the value
        // only needs materializing on the (rare) post that was dropped.
        return Ok(OpAnswer::Json(serde_json::json!({
            "sent": false,
            "dropped": true,
            "droppedCount": dropped,
        })));
    }
    notify.notify_one();
    Ok(OpAnswer::Wire(SENT_OK))
}

/// Report a mailbox drop on the [`MAILBOX_DROP_REPORT_EVERY`] cadence: the
/// first drop and then one line per further cadence step. A full mailbox means
/// the receiving pump is not running (a worker in synchronous JS, or a parent
/// whose event loop is blocked) — the message is lost, and that must be
/// visible to the operator rather than silent.
fn report_mailbox_drop(id: u64, direction: &str, len: usize, dropped: u64) {
    if dropped == 1 || dropped.is_multiple_of(MAILBOX_DROP_REPORT_EVERY) {
        crate::ext_diag!(
            "[ext] worker {id} {direction} mailbox full ({} bytes queued): dropped a {} byte message ({} dropped so far)",
            MAX_MAILBOX_BYTES,
            len,
            dropped
        );
    }
}

/// The worker's overridden `process.exit`: record the code and flip the
/// shutdown flag. The sentinel throw unwinds the worker code; the pump sees
/// `stop`, the thread pushes the final `exit` record.
fn op_worker_exit(payload: &JsonValue) -> Result<JsonValue, String> {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or("missing worker id")?;
    let code = payload.get("code").and_then(JsonValue::as_i64).unwrap_or(0) as i32;
    let Some(shared) = worker_by_id(id) else {
        return Ok(serde_json::json!({ "dropped": true }));
    };
    *WorkerShared::lock(&shared.exit_code) = Some(code);
    shared
        .stop_reason
        .store(StopReason::ProcessExit as u8, Ordering::SeqCst);
    shared.shutdown.store(true, Ordering::SeqCst);
    shared.inbound_notify.notify_one();
    shared.outbound_notify.notify_one();
    Ok(serde_json::json!({ "exiting": true }))
}

/// Signal one worker to stop and report its current state without blocking
/// the extension actor.
///
/// `shutdown` lets the JS pump observe the stop; `abort` is consumed by the
/// worker runtime's QuickJS interrupt handler, so a pure synchronous JS loop
/// (`while (true) {}`) is interrupted at the next bytecode boundary instead of
/// making this op poll for 200 ms and answer `{done:false}` forever. The JS
/// `terminate()` loop re-invokes this op with a timer yield until `done` is
/// observed; the actor is never parked here (R2-3-N07). The legacy `waitMs`
/// field of the payload is accepted for wire compatibility and ignored: the
/// answer is always the current `{done, exitCode}` snapshot, with `exitCode`
/// finalized before `done` is published so the two views cannot disagree.
fn op_terminate(payload: &JsonValue) -> Result<JsonValue, String> {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or("missing worker id")?;
    let Some(shared) = worker_by_id(id) else {
        return Ok(serde_json::json!({ "done": true, "exitCode": 1 }));
    };
    shared
        .stop_reason
        .store(StopReason::Terminate as u8, Ordering::SeqCst);
    shared.shutdown.store(true, Ordering::SeqCst);
    shared.abort.store(true, Ordering::SeqCst);
    shared.inbound_notify.notify_one();
    shared.outbound_notify.notify_one();
    let done = shared.done.load(Ordering::SeqCst);
    let exit_code = *WorkerShared::lock(&shared.exit_code);
    if done {
        workers()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
    Ok(serde_json::json!({
        "done": done,
        "exitCode": exit_code.unwrap_or(1),
    }))
}

/// Read-only terminal snapshot of one worker, for regression tests that must
/// prove the registry retains a completed worker until the parent explicitly
/// observes the exit. Unlike `terminate`/`release` it has no side effects:
/// it does not drain the mailbox, change the stop reason, or remove the
/// entry. `done` is stored after the final code is published, so a `done`
/// answer implies `exitCode` is final.
fn op_state(payload: &JsonValue) -> Result<JsonValue, String> {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or("missing worker id")?;
    let Some(shared) = worker_by_id(id) else {
        // No entry: either the parent already released it or the worker
        // never existed. Either way there is nothing left to observe.
        return Ok(serde_json::json!({
            "present": false,
            "done": true,
            "exitCode": null,
        }));
    };
    let done = shared.done.load(Ordering::SeqCst);
    let exit_code = *WorkerShared::lock(&shared.exit_code);
    Ok(serde_json::json!({
        "present": true,
        "done": done,
        "exitCode": exit_code,
    }))
}

/// Drop the registry entry once the parent has observed the exit (the worker
/// thread holds its own `Arc` clone until it finishes, so this is safe at any
/// point after `done`).
fn op_release(payload: &JsonValue) -> Result<JsonValue, String> {
    let id = payload.get("id").and_then(JsonValue::as_u64).unwrap_or(0);
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
    Ok(serde_json::json!({ "released": true }))
}

fn op_env_get(payload: &JsonValue) -> Result<JsonValue, String> {
    // `HashMap<String, _>::get` takes `&str` (`String: Borrow<str>`), so the
    // `to_owned()` this replaces only bought a throwaway key copy.
    let key = payload_str(payload, "key").unwrap_or_default();
    let wire = env_data()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .cloned();
    Ok(match wire {
        Some(wire) => serde_json::from_str(&wire).unwrap_or(JsonValue::Null),
        None => JsonValue::Null,
    })
}

fn op_env_set(payload: &JsonValue) -> Result<JsonValue, String> {
    let key = payload_str(payload, "key").unwrap_or_default().to_owned();
    let value = payload.get("value").cloned().unwrap_or(JsonValue::Null);
    env_data()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, value.to_string());
    Ok(serde_json::json!({ "set": true }))
}

// ---------------------------------------------------------------------------
// Message poll (async host entry)
// ---------------------------------------------------------------------------

/// Fixed idle answer (the pump's timer-fallback return): already in its wire
/// bytes instead of rebuilding a fresh `Map` + `Vec` + serializer stack on
/// every idle poll of every worker.
const EMPTY_POLL: &str = r#"{"messages":[],"stop":false,"done":false,"exitCode":null,"dropped":0}"#;
/// Fixed post-exit answer: the worker left the registry, the pump stops.
const GONE_POLL: &str = r#"{"messages":[],"stop":true,"done":true,"exitCode":null}"#;

/// Drain envelope. Struct-serialized so the drained messages go out as a
/// borrowed slice — the `json!` macro it replaces rebuilt the whole batch as a
/// `Vec<Value>` (plus a 4-key `Map` and one heap `String` per key) on every
/// poll that carried messages.
///
/// `dropped` is the mailbox's cumulative drop count (see
/// [`MAX_MAILBOX_BYTES`]); the JS pump ignores the extra key, and a probe/test
/// reads it to observe the bounded mailbox.
#[derive(serde::Serialize)]
struct PollEnvelope<'a> {
    messages: &'a [String],
    stop: bool,
    done: bool,
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
    dropped: u64,
}

/// Drain the caller's mailbox, parking on its `Notify` (with the timer
/// fallback) until messages arrive, the worker shuts down, or `waitMs`
/// elapses. Returns a JSON envelope:
/// `{messages: [..], stop: bool, done: bool, exitCode: int|null, dropped: int}`.
async fn recv_poll(id: u64, wait_ms: u64, role: &str) -> String {
    let Some(shared) = worker_by_id(id) else {
        return GONE_POLL.to_owned();
    };
    let (queue, notify) = match role {
        "worker" => (&shared.inbound, &shared.inbound_notify),
        _ => (&shared.outbound, &shared.outbound_notify),
    };
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    // Adaptive fallback: an idle mailbox is re-checked after 25, 50, 100, ...
    // up to 1 s, so a poll that found nothing costs O(log wait) wakeups
    // instead of wait / 25 ms. A delivered message resets the slice.
    let mut slice_ms = POLL_FALLBACK_MS;
    loop {
        let (messages, dropped) = {
            let mut mailbox = WorkerShared::lock(queue);
            let dropped = mailbox.dropped;
            (mailbox.drain(), dropped)
        };
        let shutdown = shared.shutdown.load(Ordering::SeqCst);
        let done = shared.done.load(Ordering::SeqCst);
        if !messages.is_empty() || shutdown || done {
            let envelope = PollEnvelope {
                messages: &messages,
                stop: shutdown || done,
                done,
                exit_code: *WorkerShared::lock(&shared.exit_code),
                dropped,
            };
            // Infallible: `PollEnvelope` holds `&[String]`, `bool`, `bool`,
            // `Option<i32>` and `u64`, struct-serialized with literal field
            // names (no map keys), into an in-memory writer. `expect` replaces
            // the `GONE_POLL` fallback, whose hypothetical path would silently
            // discard the messages just drained and answer `stop/done`.
            return serde_json::to_string(&envelope)
                .expect("PollEnvelope serialization is infallible");
        }
        let now = Instant::now();
        if now >= deadline {
            return EMPTY_POLL.to_owned();
        }
        // Register on the notify BEFORE checking the mailbox's emptiness was
        // done — a post that landed in between leaves a stored permit, so the
        // await below consumes it immediately (no lost wakeup). `enable`
        // registers the waiter without polling it, which the previous
        // `notify.notified()` future did not do until first polled; that gap
        // is why the 25 ms timer was the only bound.
        let notified = notify.notified();
        tokio::pin!(notified);
        let _enabled = notified.as_mut().enable();
        let wait = Duration::from_millis(slice_ms).min(deadline - now);
        let had_message = tokio::select! {
            _ = notified.as_mut() => true,
            _ = tokio::time::sleep(wait) => false,
        };
        if !had_message {
            shared.poll_wakes.fetch_add(1, Ordering::Relaxed);
        }
        slice_ms = next_poll_slice(slice_ms, had_message);
    }
}

// ---------------------------------------------------------------------------
// Test seams (integration tests live in `tests/`; inline test modules are
// forbidden in `src/`)
// ---------------------------------------------------------------------------

/// `(base, ceiling)` of the adaptive `workerRecvPoll` fallback, in ms.
#[doc(hidden)]
pub fn poll_slice_bounds_for_tests() -> (u64, u64) {
    (POLL_FALLBACK_MS, POLL_FALLBACK_MAX_MS)
}

/// One step of the adaptive `workerRecvPoll` fallback.
#[doc(hidden)]
pub fn next_poll_slice_for_tests(current_ms: u64, had_message: bool) -> u64 {
    next_poll_slice(current_ms, had_message)
}

/// Register an empty worker mailbox without a worker thread (the poll-wait
/// probe), returning its id.
#[doc(hidden)]
pub fn test_register_worker() -> u64 {
    let id = NEXT_WORKER_ID.fetch_add(1, Ordering::SeqCst);
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::new(WorkerShared::new(0)));
    id
}

/// Remove a worker registered by [`test_register_worker`].
#[doc(hidden)]
pub fn test_release_worker(id: u64) {
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
}

/// Run one `workerRecvPoll` against a test worker.
#[doc(hidden)]
pub async fn test_recv_poll(id: u64, wait_ms: u64, role: &str) -> String {
    recv_poll(id, wait_ms.min(MAX_POLL_WAIT_MS), role).await
}

/// Timer-fallback wake count of one test worker.
#[doc(hidden)]
pub fn test_worker_poll_wakes(id: u64) -> u64 {
    worker_by_id(id)
        .map(|shared| shared.poll_wakes.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Post one message into a test worker's mailbox (the other side's pump);
/// returns whether the mailbox accepted it.
#[doc(hidden)]
pub fn test_worker_send(id: u64, to_worker: bool, data: &str) -> bool {
    let Some(shared) = worker_by_id(id) else {
        return false;
    };
    let (mailbox, notify) = if to_worker {
        (&shared.inbound, &shared.inbound_notify)
    } else {
        (&shared.outbound, &shared.outbound_notify)
    };
    let accepted = WorkerShared::lock(mailbox).push(data.to_owned());
    if accepted {
        notify.notify_one();
    }
    accepted
}

// ---------------------------------------------------------------------------
// Worker thread body
// ---------------------------------------------------------------------------

/// Record + notify helper for the control records the parent pump dispatches
/// (`kind: "error" | "exit"`). Control records bypass [`MAX_MAILBOX_BYTES`]:
/// they are the ones that stop the pump, and they only ever arrive from the
/// worker's terminal paths (one boot failure or panic + one exit), so the
/// bypass cannot be used to grow the queue.
fn push_record(shared: &WorkerShared, record: JsonValue) {
    WorkerShared::lock(&shared.outbound).push_control(record.to_string());
    shared.outbound_notify.notify_one();
}

fn finish_worker(shared: &WorkerShared, code: i32) {
    // Persist the terminal code before publishing `done`/the record: every
    // later reader (`recv_poll`, `op_terminate`, the parent's `exit` event)
    // must observe the same value, whichever path settles first.
    let code = shared.finalize_exit_code(code);
    push_record(shared, serde_json::json!({ "kind": "exit", "code": code }));
    shared.done.store(true, Ordering::SeqCst);
    shared.outbound_notify.notify_one();
    shared.inbound_notify.notify_one();
    // Keep the registry entry until the parent observes the exit
    // (`op_release`, or `op_terminate` answering `done:true`). Removing it
    // here would make a later `terminate()` fall back to the hard-coded 1
    // while a normal completion's recorded 0 is still unread; the thread's
    // own `Arc` keeps the mailboxes alive until this function returns.
}

/// Sets the worker's abort flag when the worker function unwinds, just before
/// `ExtensionRuntime::drop` evaluates the process-exit teardown JS. A user
/// `process.on("exit")` handler with a synchronous infinite loop would
/// otherwise pin the worker thread (and its [`LIVE_WORKERS`] slot) forever on
/// every exit path; the QuickJS interrupt handler then breaks the teardown at
/// the next bytecode poll.
///
/// The guarantee is the bound, not handler completion: QuickJS only
/// decrements its interrupt counter between polls, so the remaining phase
/// when the teardown starts is not fixed. A short, well-behaved exit handler
/// USUALLY runs to completion, but it can also be truncated at the first
/// poll. A native host call inside the handler is not interruptible at all
/// (the same boundary as terminate); it just cannot loop in JS forever.
struct AbortWorkerOnDrop(Arc<AtomicBool>);

impl Drop for AbortWorkerOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Thread entry: panic isolation around the whole worker run. A panicked
/// worker reports through the same `error` + `exit(1)` channel as a failed
/// boot, never leaving the parent pump waiting forever.
fn worker_thread_main(spec: WorkerSpec, shared: Arc<WorkerShared>) {
    // Registry-independent live-thread accounting: the slot is released when
    // this function returns (normal exit, interrupt, or panic unwind) — after
    // `release_generation` has already removed the registry entry.
    let _live_slot = WorkerThreadSlot {
        shared: Arc::clone(&shared),
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        worker_thread_run(spec, Arc::clone(&shared))
    }));
    if let Err(panic) = outcome {
        let message = panic
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "worker thread panicked".to_owned());
        if !shared.done.load(Ordering::SeqCst) {
            push_record(
                &shared,
                serde_json::json!({ "kind": "error", "message": message }),
            );
            finish_worker(&shared, 1);
        }
    }
}

fn worker_thread_run(spec: WorkerSpec, shared: Arc<WorkerShared>) {
    let runtime_build = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let runtime = match runtime_build {
        Ok(runtime) => runtime,
        Err(error) => {
            push_record(
                &shared,
                serde_json::json!({
                    "kind": "error",
                    "message": format!("worker runtime failed to start: {error}"),
                }),
            );
            finish_worker(&shared, 1);
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        run_worker_async(spec, shared).await;
    });
}

async fn run_worker_async(spec: WorkerSpec, shared: Arc<WorkerShared>) {
    let fail = |message: String, shared: &Arc<WorkerShared>| {
        push_record(
            shared,
            serde_json::json!({ "kind": "error", "message": message }),
        );
        finish_worker(shared, 1);
    };

    // The worker inherits the parent host's `mode`/`has_ui` rather than
    // declaring itself headless: the worker is an OS thread of the same
    // process, so its `process.stdout/stderr.write` must land where the
    // parent's do — the tracing log in an interactive TUI session, the real
    // stdio in every headless mode (see `super::super::ext_stdio`).
    let config = ExtensionConfig {
        cwd: spec.cwd.clone(),
        mode: spec.host_mode.clone(),
        has_ui: spec.has_ui,
        limits: spec.limits.clone(),
        ..Default::default()
    };
    let runtime = match ExtensionRuntime::new(config).await {
        Ok(runtime) => runtime,
        Err(error) => {
            fail(format!("worker runtime boot failed: {error}"), &shared);
            return;
        }
    };
    // The parent (`terminate` / `release_generation`) sets this flag without
    // touching this runtime; the QuickJS interrupt handler polls it at
    // bytecode boundaries, so a pure synchronous JS loop can be aborted.
    runtime.set_interrupt_flag(Arc::clone(&shared.abort));
    // Declared after `runtime`: Rust drops locals in reverse order, so this
    // flips the abort flag immediately before `runtime`'s Drop runs the
    // `process.emit("exit")` teardown JS. That keeps a synchronous infinite
    // loop in a user exit handler from pinning this thread (P3-05).
    let _abort_on_drop = AbortWorkerOnDrop(Arc::clone(&shared.abort));
    for root in &spec.roots {
        runtime.allow_root(root.clone());
    }
    // The worker is expected to run much deeper JS recursion than a plain
    // extension runtime (pi-subagents parses workflow scripts with acorn's
    // recursive-descent parser in here). `ExtensionRuntime::new` already
    // applied the resolved limits (clamped to `MAX_STACK_BYTES`); re-assert
    // the crate maximum here so the worker's stack policy is visible at the
    // one place that spawns a worker runtime. The request must stay at or
    // below rquickjs's `MAX_SANE`: anything above is silently rewritten to
    // 0, which DISABLES the QuickJS stack check and makes deep recursion
    // abort the process instead of throwing `RangeError` (see
    // `WORKER_JS_STACK_BYTES`).
    let _ = runtime
        .js_runtime
        .set_max_stack_size(WORKER_JS_STACK_BYTES)
        .await;

    // Worker boot overlay: parentPort/workerData/threadId wiring, the
    // worker-local `process.exit`, the code runner and the message pump.
    //
    // The `to_string` calls below serialize a plain `String` through
    // `Value::String(..)`'s `Display`, which writes into an in-memory buffer
    // infallibly — no `Result`, so no fallback (the old placeholder literals
    // (`"."`, `"null"`, ...) could have booted the worker with a wrong
    // dir/data/code value). Escaping is the same serializer `to_string`
    // would use, so the bytes are identical.
    let dir_json = serde_json::Value::String(spec.from_dir.clone()).to_string();
    // worker_data is already the serialized JSON text of the posted value;
    // quote it again so the overlay's JSON.parse receives a string literal.
    let data_json = serde_json::Value::String(spec.worker_data.clone()).to_string();
    let boot = WORKER_BOOT
        .replace("__PIR_WORKER_ID__", &spec.id.to_string())
        .replace("__PIR_WORKER_THREAD_ID__", &spec.id.to_string())
        .replace("__PIR_WORKER_DATA__", &data_json)
        .replace("__PIR_WORKER_DIR__", &dir_json);
    if let Err(error) = runtime.eval_boot(&boot).await {
        if shared.shutdown.load(Ordering::SeqCst) {
            // The boot JS was interrupted by a terminate/reload that raced
            // the worker startup: a clean stop, not a boot failure.
            let code = { *WorkerShared::lock(&shared.exit_code) }.unwrap_or(1);
            finish_worker(&shared, code);
        } else {
            fail(format!("worker boot failed: {error}"), &shared);
        }
        return;
    }

    // Run the worker source as a CommonJS module body (Node's `eval: true`
    // compiles worker code as CJS too): full `require`, top-level `return`,
    // and the worker-runtime `process`. Same infallible construction as the
    // boot literals above: three plain `String`s through `Value::String`'s
    // `Display` (the same serializer and escaping `to_string` would use).
    let code_json = serde_json::Value::String(spec.code.clone()).to_string();
    let file_json = serde_json::Value::String(spec.filename.clone()).to_string();
    let dir_json = serde_json::Value::String(spec.from_dir.clone()).to_string();
    let run_source = format!(
        "globalThis.__pirWorkerRunStart({{ code: {code_json}, filename: {file_json}, fromDir: {dir_json} }})"
    );
    // Synchronous worker bodies run under the parent runtime's configured
    // call deadline (the same `ExtensionRuntime::arm_deadline` the actor uses
    // for a tool call). This is what stops `while (true) {}` even when no one
    // calls `terminate`: the QuickJS interrupt handler aborts the eval at the
    // deadline. Asynchronous work belongs to the pump loop that starts after
    // the body returns, so it is not bounded by this deadline.
    let code_budget = runtime.call_timeout();
    let code_deadline = runtime.arm_deadline();
    let outcome = runtime
        .ctx
        .async_with(async |ctx| ctx.eval::<String, _>(run_source.clone()))
        .await;
    runtime.disarm_deadline(code_deadline);
    let outcome = match outcome {
        Ok(payload) => payload,
        Err(error) => {
            if shared.shutdown.load(Ordering::SeqCst) {
                // `terminate` / `/reload`: the abort flag deliberately tore
                // down the eval. This is a clean stop, not a worker error.
                // Do not touch the engine again: the abort flag is still set,
                // so any further JS entry (including reading the pending
                // exception) would be interrupted immediately.
                let code = { *WorkerShared::lock(&shared.exit_code) }.unwrap_or(1);
                finish_worker(&shared, code);
            } else {
                // Read (and clear) the pending exception before the thread
                // drops the runtime: it distinguishes a deadline interrupt
                // from a genuine engine failure in the error record, and a
                // stale interrupt exception must not leak into teardown.
                let detail = runtime.take_pending_exception_text().await;
                let detail = detail.map(|text| format!(" ({text})")).unwrap_or_default();
                fail(
                    format!(
                        "worker code exceeded its {:?} synchronous deadline or failed to evaluate: {error}{detail}",
                        code_budget
                    ),
                    &shared,
                );
            }
            return;
        }
    };
    let parsed: JsonValue = match serde_json::from_str(&outcome) {
        Ok(parsed) => parsed,
        Err(error) => {
            fail(format!("worker outcome parse failed: {error}"), &shared);
            return;
        }
    };
    if parsed.get("ok").and_then(JsonValue::as_bool) != Some(true) {
        // Node: an uncaught error in the worker's main script surfaces as the
        // parent `Worker`'s `error` event, then the worker exits with 1.
        let message = parsed
            .get("message")
            .and_then(JsonValue::as_str)
            .unwrap_or("worker script failed")
            .to_owned();
        let stack = parsed
            .get("stack")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        push_record(
            &shared,
            serde_json::json!({ "kind": "error", "message": message, "stack": stack }),
        );
        finish_worker(&shared, 1);
        return;
    }

    // Message pump until the shutdown flag (terminate or the worker's own
    // `process.exit`) stops it. Pending timers/microtasks between polls ride
    // the runtime's executor, exactly like the parent's event loop.
    let pump = runtime
        .ctx
        .async_with(async |ctx| -> rquickjs::Result<()> {
            let pump_fn: rquickjs::Function = ctx.globals().get("__pirWorkerPump")?;
            let pending: MaybePromise = pump_fn.call(())?;
            pending.into_future::<()>().await.map(|_| ())
        })
        .await;
    if let Err(error) = pump {
        if shared.shutdown.load(Ordering::SeqCst) {
            // The interrupt was our own `terminate`/`/reload` abort (the pump
            // was executing JS when the flag was observed): clean stop.
            let code = { *WorkerShared::lock(&shared.exit_code) }.unwrap_or(1);
            finish_worker(&shared, code);
        } else {
            fail(format!("worker pump failed: {error}"), &shared);
        }
        return;
    }

    // The pump only returns Ok when it observed `stop`/`done`; a normal,
    // self-directed completion supplies its code through `process.exit`
    // (already stored). If no code was recorded, `finish_worker` consults the
    // stop reason: a terminate/generation release becomes Node's 1, a truly
    // natural return stays 0.
    finish_worker(&shared, 0);
}
