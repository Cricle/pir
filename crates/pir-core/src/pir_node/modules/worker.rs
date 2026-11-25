//! Node `worker_threads` transport (ported from `worker_fns.rs`).
//!
//! One OS thread plus one QuickJS runtime per Worker lives in the pir host
//! (the engine is `!Send` and needs the extension actor boot), so this module
//! owns everything around it: the process-wide registry, both mailboxes with
//! their byte bound and drop accounting, the lifecycle/exit-code state machine,
//! the op table, the poll envelope and generation-scoped teardown. Starting a
//! worker is delegated through [`crate::pir_node::NodeHooks::spawn_worker`]; the host
//! thread drives the runtime and reports back through [`WorkerHandle`].
//!
//! Message flow:
//! ```text
//! parent JS --parentSend--> [inbound]  --recv_poll("worker")--> worker JS
//! worker JS --workerSend--> [outbound] --recv_poll("parent")--> parent JS
//! ```
//!
//! `MAX_MAILBOX_BYTES` bounds each direction and drops the NEWEST data message
//! (control `error`/`exit` records are exempt); every drop is counted and
//! reported. Parked polls wake on a notify with an adaptive timer fallback so
//! no wakeup is lost.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use crate::pir_node::error::{NodeError, NodeResult};

/// Hard cap on `worker_threads.Worker` registry entries for the process.
///
/// Each Worker is one OS thread plus a full QuickJS runtime; 64 admits
/// realistic parallel workloads while keeping the worst case bounded.
const MAX_WORKER_THREADS: usize = 64;

/// Process-wide hard cap on OS worker threads alive, **across generations**
/// (a `/reload` removes registry entries immediately, so without this a reload
/// loop could spawn 64 more workers each time).
const MAX_LIVE_WORKER_THREADS: usize = MAX_WORKER_THREADS * 2;

/// Upper bound of one `workerRecvPoll` wait (the JS pumps ask for less).
const MAX_POLL_WAIT_MS: u64 = 1000;

/// Base of the notify fallback period: a lost notify wakeup must not stall a
/// pump, so every wait is also timer-bounded.
const POLL_FALLBACK_MS: u64 = 25;

/// Ceiling of the adaptive poll fallback: idle slices double up to this bound.
const POLL_FALLBACK_MAX_MS: u64 = MAX_POLL_WAIT_MS;

/// Next fallback slice after one wait: reset on a delivered message, double
/// (up to the ceiling) when the wait timed out.
fn next_poll_slice(current_ms: u64, had_message: bool) -> u64 {
    if had_message {
        return POLL_FALLBACK_MS;
    }
    current_ms.saturating_mul(2).min(POLL_FALLBACK_MAX_MS)
}

const MAX_MAILBOX_BYTES: usize = 8 * 1024 * 1024;

/// Drop-report cadence: the first dropped message is reported, then one
/// diagnostic per this many further drops (identical repeats are additionally
/// collapsed by the logging router).
const MAILBOX_DROP_REPORT_EVERY: u64 = 1024;

// ---------------------------------------------------------------------------
// Worker registry
// ---------------------------------------------------------------------------

/// One queued mailbox entry.
///
/// Legacy callers (`__pir_host.worker`) hand over already-serialized wire
/// text; the typed C-ABI entries hand over a live JSON tree. Both share one
/// queue so ordering — and the byte bound — is the same when a runtime mixes
/// the two paths (a test posting through the raw entry while the shim uses the
/// typed one, or a worker booted by a host that lacks the typed tier).
enum Queued {
    Text(String),
    Value(JsonValue),
}

impl Queued {
    /// Accounted size (the quantity [`MAX_MAILBOX_BYTES`] bounds).
    ///
    /// The typed tree is ESTIMATED without serializing it: an exact wire
    /// length would put back the very conversion the typed path removes on
    /// every send. The estimate counts the queued payload strings/keys (the
    /// memory the host actually holds) plus a small fixed cost per
    /// scalar/container, so it never undercounts a string.
    fn wire_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Value(value) => json_size_estimate(value),
        }
    }

    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Value(value) => value.to_string(),
        }
    }

    fn into_value(self) -> JsonValue {
        match self {
            Self::Text(text) => serde_json::from_str(&text).unwrap_or(JsonValue::Null),
            Self::Value(value) => value,
        }
    }
}

/// Approximate serialized size of one JSON tree (see [`Queued::wire_len`]).
fn json_size_estimate(value: &JsonValue) -> usize {
    match value {
        JsonValue::Null => 4,
        JsonValue::Bool(_) => 5,
        JsonValue::Number(_) => 8,
        JsonValue::String(text) => text.len().saturating_add(2),
        JsonValue::Array(items) => items.iter().fold(2, |total, item| {
            total
                .saturating_add(json_size_estimate(item))
                .saturating_add(1)
        }),
        JsonValue::Object(map) => map.iter().fold(2, |total, (key, item)| {
            total
                .saturating_add(key.len())
                .saturating_add(json_size_estimate(item))
                .saturating_add(4)
        }),
    }
}

/// One direction's message queue: the queued entries, their total size (the
/// quantity [`MAX_MAILBOX_BYTES`] bounds) and the drop counter.
#[derive(Default)]
struct Mailbox {
    queue: VecDeque<Queued>,
    bytes: usize,
    dropped: u64,
}

impl Mailbox {
    /// Append one message, or drop it because the mailbox is at its bound.
    /// Returns whether the message was accepted.
    fn push(&mut self, message: Queued) -> bool {
        let len = message.wire_len();
        if self.bytes.saturating_add(len) > MAX_MAILBOX_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.bytes += len;
        self.queue.push_back(message);
        true
    }

    /// Append one control record (`error`/`exit`): the bound does not apply,
    /// because these are the records that END the pump — dropping one would
    /// leave the peer waiting forever instead of reporting the exit.
    fn push_control(&mut self, message: Queued) {
        self.bytes += message.wire_len();
        self.queue.push_back(message);
    }

    fn drain(&mut self) -> Vec<Queued> {
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
    /// Parent -> worker message queue (typed values, or legacy wire text from
    /// callers that still use the string entry).
    inbound: StdMutex<Mailbox>,
    /// Worker -> parent message queue (typed values, or legacy wire text: data
    /// payloads plus the `{kind:"error"|"exit", ...}` control records).
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
pub struct WorkerExitGuard {
    shared: Arc<WorkerShared>,
}

impl WorkerExitGuard {
    /// Take the live-count slot for the thread that is about to drive
    /// `handle`; dropping the guard releases it.
    pub fn new(handle: &WorkerHandle) -> Self {
        Self {
            shared: Arc::clone(&handle.0),
        }
    }
}

impl Drop for WorkerExitGuard {
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
pub fn release_generation(generation: u64) {
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

/// Number of live worker handles opened by one runtime generation.
///
/// The generation-release regression asserts that a runtime's own teardown
/// returns its count to baseline; counting per generation keeps a worker in
/// another suite from making that assertion flaky.
#[doc(hidden)]
pub fn worker_count_for_generation(generation: u64) -> usize {
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .filter(|handle| handle.generation == generation)
        .count()
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

/// Everything the host worker thread needs to boot its runtime. `Send` by
/// design: this crosses the thread boundary (the rquickjs runtime never does).
/// The host adds its own boot extras (mode/has_ui/limits) around this.
#[derive(Debug, Clone)]
pub struct WorkerLaunch {
    pub id: u64,
    pub code: String,
    pub from_dir: String,
    pub worker_data: String,
    pub filename: String,
    pub cwd: PathBuf,
    pub roots: Vec<PathBuf>,
}

/// The host thread's handle to one registered worker: abort/shutdown state and
/// the terminal record writers. Cloned into the spawned thread; the registry
/// keeps its own `Arc`.
#[derive(Clone)]
pub struct WorkerHandle(Arc<WorkerShared>);

impl WorkerHandle {
    /// QuickJS interrupt flag observed at the next bytecode boundary.
    pub fn abort_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.abort)
    }

    pub fn is_shutdown(&self) -> bool {
        self.0.shutdown.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> Option<i32> {
        *WorkerShared::lock(&self.0.exit_code)
    }

    /// Whether the terminal record has been published.
    pub fn is_done(&self) -> bool {
        self.0.done.load(Ordering::SeqCst)
    }

    /// Publish the `error` control record the parent pump dispatches.
    pub fn push_error(&self, message: &str, stack: &str) {
        let record = if stack.is_empty() {
            serde_json::json!({ "kind": "error", "message": message })
        } else {
            serde_json::json!({ "kind": "error", "message": message, "stack": stack })
        };
        push_record(&self.0, record);
    }

    /// Publish the `exit` record with the resolved code and mark the worker
    /// done. Later readers (`recv_poll`, `terminate`) observe the same slot.
    pub fn finish(&self, code: i32) {
        finish_worker(&self.0, code);
    }
}

/// Payload string field.
fn payload_str<'a>(payload: &'a JsonValue, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(JsonValue::as_str)
}

/// One worker-op failure: the envelope code plus the message. Most ops report
/// the generic [`WORKER_OP_ERROR`]; `start` forwards the Node error code
/// (e.g. `E2BIG` for a Worker entry over the shared 32 MiB source cap) so the
/// async `error` event carries the same code as `fs.readFile`.
struct OpError {
    code: String,
    message: String,
}

const WORKER_OP_ERROR: &str = "ERR_WORKER_OP";

impl From<String> for OpError {
    fn from(message: String) -> Self {
        Self {
            code: WORKER_OP_ERROR.to_owned(),
            message,
        }
    }
}

impl OpError {
    fn coded(code: impl Into<String>, message: String) -> Self {
        Self {
            code: code.into(),
            message,
        }
    }
}

/// `worker` op table entry.
pub fn worker_op(
    runtime: &crate::pir_node::NodeRuntime,
    op: &str,
    payload: &JsonValue,
) -> NodeResult {
    let result = match op {
        "start" => op_start(runtime, payload),
        "parentSend" => op_send(payload, "inbound").map_err(OpError::from),
        "workerSend" => op_send(payload, "outbound").map_err(OpError::from),
        "workerExit" => op_worker_exit(payload).map_err(OpError::from),
        "terminate" => op_terminate(payload).map_err(OpError::from),
        "state" => op_state(payload).map_err(OpError::from),
        "release" => op_release(payload).map_err(OpError::from),
        "envGet" => op_env_get(payload).map_err(OpError::from),
        "envSet" => op_env_set(payload).map_err(OpError::from),
        other => Err(OpError::from(format!("unknown worker op `{other}`"))),
    };
    result.map_err(|error| NodeError::new(error.code, error.message))
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

fn op_start(
    runtime: &crate::pir_node::NodeRuntime,
    payload: &JsonValue,
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
        let cwd = runtime.cwd().display().to_string();
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
            crate::pir_node::modules::fs::read_text_capped(Path::new(&filename), "open", &filename)
                .map_err(|error| {
                    let message = if error.code == "ENOENT" {
                        format!("Cannot find module '{filename}': {}", error.message)
                    } else {
                        error.message
                    };
                    OpError::coded(error.code, message)
                })?;
        let dir = Path::new(&filename)
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        (source, dir, filename)
    };

    // The worker loads modules through the same loader chain as the parent:
    // snapshot the parent's allowed roots plus any host-supplied extra roots
    // (the agent npm root for the `pir js` runner), so npm dependencies of the
    // worker code resolve and load.
    let mut roots: Vec<PathBuf> = runtime
        .allowed_roots
        .lock()
        .map(|roots| roots.clone())
        .unwrap_or_default();
    for root in runtime.hooks.worker_roots() {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let cwd = runtime.cwd();

    let generation = runtime.context.generation();
    let id = NEXT_WORKER_ID.fetch_add(1, Ordering::SeqCst);
    let shared = Arc::new(WorkerShared::new(generation));
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&shared));

    let launch = WorkerLaunch {
        id,
        code,
        from_dir,
        worker_data,
        filename,
        cwd,
        roots,
    };
    match runtime
        .hooks
        .spawn_worker(launch, WorkerHandle(Arc::clone(&shared)))
    {
        Ok(()) => {
            // The host thread's exit guard now owns the live-count slot; the
            // local reservation must not release it a second time.
            reservation.transfer();
            Ok(serde_json::json!({ "id": id, "threadId": id }))
        }
        Err(error) => {
            workers()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            // Dropping `reservation` releases the slot.
            Err(OpError::coded(error.code, error.message))
        }
    }
}

/// Result of one mailbox enqueue, shared by the legacy string op and the
/// typed C-ABI send.
enum EnqueueOutcome {
    /// No registry entry: a message to an exited worker is dropped (Node fails
    /// it with `ERR_WORKER_NOT_RUNNING` when the port is gone; the shim layer
    /// keeps `postMessage` best-effort silent).
    Gone,
    /// The worker is shutting down: dropped, like a dead port.
    Shutdown,
    Accepted,
    Dropped {
        dropped: u64,
    },
}

/// Push one message into the worker's inbound (parent -> worker) or the
/// parent's outbound (worker -> parent) queue. Both wire forms converge here
/// so ordering and the byte bound are identical.
fn enqueue(id: u64, to_worker: bool, message: Queued) -> EnqueueOutcome {
    let Some(shared) = worker_by_id(id) else {
        return EnqueueOutcome::Gone;
    };
    if shared.shutdown.load(Ordering::SeqCst) {
        return EnqueueOutcome::Shutdown;
    }
    let len = message.wire_len();
    let (mailbox, notify, direction) = if to_worker {
        (&shared.inbound, &shared.inbound_notify, "parent->worker")
    } else {
        (&shared.outbound, &shared.outbound_notify, "worker->parent")
    };
    let (accepted, dropped) = {
        let mut mailbox = WorkerShared::lock(mailbox);
        let accepted = mailbox.push(message);
        (accepted, mailbox.dropped)
    };
    if !accepted {
        report_mailbox_drop(id, direction, len, dropped);
        return EnqueueOutcome::Dropped { dropped };
    }
    notify.notify_one();
    EnqueueOutcome::Accepted
}

/// Push one data message into the worker's inbound (parent -> worker) or the
/// parent's outbound (worker -> parent) queue from the legacy string wire. The
/// `data` field carries the already-serialized JSON text of the posted value.
///
/// A mailbox at [`MAX_MAILBOX_BYTES`] drops the message (see that constant: a
/// parked sender could deadlock the two pumps against each other) and answers
/// with the running drop total, which is only materialized in the envelope
/// once the counter is nonzero — the accepted path keeps the fixed wire text.
fn op_send(payload: &JsonValue, side: &str) -> Result<JsonValue, String> {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or("missing worker id")?;
    // The message body is cloned once into the mailbox; non-string `data`
    // falls through to the same error as before.
    let data = match payload.get("data") {
        Some(JsonValue::String(text)) => text.clone(),
        _ => return Err("missing data payload".to_string()),
    };
    match enqueue(id, side == "inbound", Queued::Text(data)) {
        EnqueueOutcome::Gone | EnqueueOutcome::Shutdown => {
            Ok(serde_json::json!({ "dropped": true }))
        }
        EnqueueOutcome::Accepted => Ok(serde_json::json!({ "sent": true })),
        // Same envelope shape as `SENT_OK` plus the drop accounting: the value
        // only needs materializing on the (rare) post that was dropped.
        EnqueueOutcome::Dropped { dropped } => Ok(serde_json::json!({
            "sent": false,
            "dropped": true,
            "droppedCount": dropped,
        })),
    }
}

/// Typed C-ABI send: enqueue one live message tree.
///
/// `wrap` puts the message in the `{kind: "msg", data: ...}` record the
/// worker -> parent pump dispatches; parent -> worker messages stay bare
/// values because the worker pump emits every inbound entry directly (the
/// legacy wire has the same asymmetry). Returns whether the mailbox accepted
/// it; a missing/shut-down worker drops silently, matching `postMessage`'s
/// best-effort contract.
pub fn send_value(id: u64, value: JsonValue, to_worker: bool, wrap: bool) -> bool {
    let message = if wrap {
        Queued::Value(serde_json::json!({ "kind": "msg", "data": value }))
    } else {
        Queued::Value(value)
    };
    matches!(enqueue(id, to_worker, message), EnqueueOutcome::Accepted)
}

/// Report a mailbox drop on the [`MAILBOX_DROP_REPORT_EVERY`] cadence: the
/// first drop and then one line per further cadence step. A full mailbox means
/// the receiving pump is not running (a worker in synchronous JS, or a parent
/// whose event loop is blocked) — the message is lost, and that must be
/// visible to the operator rather than silent.
fn report_mailbox_drop(id: u64, direction: &str, len: usize, dropped: u64) {
    if dropped == 1 || dropped.is_multiple_of(MAILBOX_DROP_REPORT_EVERY) {
        crate::node_diag!(
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

/// Drain envelope (legacy string wire). Struct-serialized so the drained
/// messages go out as a borrowed slice — the `json!` macro it replaces rebuilt
/// the whole batch as a `Vec<Value>` (plus a 4-key `Map` and one heap `String`
/// per key) on every poll that carried messages.
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

/// One poll batch, still in the queue's own representation. The legacy string
/// entry converts it to text, the typed C-ABI entry converts it to live JSON —
/// both share [`poll_batch`], so the park/wake/drain order cannot drift.
struct PollBatch {
    messages: Vec<Queued>,
    stop: bool,
    done: bool,
    exit_code: Option<i32>,
    dropped: u64,
}

/// Outcome of one `poll_batch` wait.
enum PollWait {
    /// The registry entry is gone (worker released or never existed).
    Gone,
    /// The wait elapsed with an empty queue and no terminal state.
    Idle,
    /// Messages and/or a terminal state to report.
    Ready(PollBatch),
}

/// Drain the caller's mailbox, parking on its `Notify` (with the timer
/// fallback) until messages arrive, the worker shuts down, or `wait_ms`
/// elapses. Shared by both wire forms.
async fn poll_batch(id: u64, wait_ms: u64, to_worker: bool) -> PollWait {
    let Some(shared) = worker_by_id(id) else {
        return PollWait::Gone;
    };
    let (queue, notify) = if to_worker {
        (&shared.inbound, &shared.inbound_notify)
    } else {
        (&shared.outbound, &shared.outbound_notify)
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
            return PollWait::Ready(PollBatch {
                messages,
                stop: shutdown || done,
                done,
                exit_code: *WorkerShared::lock(&shared.exit_code),
                dropped,
            });
        }
        let now = Instant::now();
        if now >= deadline {
            return PollWait::Idle;
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

/// Drain the caller's mailbox over the legacy string wire. Returns a JSON
/// envelope:
/// `{messages: [..], stop: bool, done: bool, exitCode: int|null, dropped: int}`.
pub async fn recv_poll(id: u64, wait_ms: u64, role: &str) -> String {
    match poll_batch(id, wait_ms, role == "worker").await {
        PollWait::Gone => GONE_POLL.to_owned(),
        PollWait::Idle => EMPTY_POLL.to_owned(),
        PollWait::Ready(batch) => {
            let messages: Vec<String> = batch.messages.into_iter().map(Queued::into_text).collect();
            let envelope = PollEnvelope {
                messages: &messages,
                stop: batch.stop,
                done: batch.done,
                exit_code: batch.exit_code,
                dropped: batch.dropped,
            };
            // Infallible: `PollEnvelope` holds `&[String]`, `bool`, `bool`,
            // `Option<i32>` and `u64`, struct-serialized with literal field
            // names (no map keys), into an in-memory writer. `expect` replaces
            // the `GONE_POLL` fallback, whose hypothetical path would silently
            // discard the messages just drained and answer `stop/done`.
            serde_json::to_string(&envelope)
                .unwrap_or_else(|e| panic!("PollEnvelope serialization failed: {e}"))
        }
    }
}

/// Convert one drained batch into the live-envelope shape the typed C-ABI
/// pump consumes. Message payloads stay as JSON trees all the way to
/// `json_to_js` — no text form is materialized.
fn batch_to_value(batch: PollBatch) -> JsonValue {
    let messages: Vec<JsonValue> = batch.messages.into_iter().map(Queued::into_value).collect();
    serde_json::json!({
        "messages": messages,
        "stop": batch.stop,
        "done": batch.done,
        "exitCode": batch.exit_code,
        "dropped": batch.dropped,
    })
}

/// Post-exit answer of the typed poll: same flags as [`GONE_POLL`].
fn gone_value() -> JsonValue {
    serde_json::json!({
        "messages": Vec::<JsonValue>::new(),
        "stop": true,
        "done": true,
        "exitCode": JsonValue::Null,
        "dropped": 0u64,
    })
}

/// Typed C-ABI poll: drain the mailbox and answer the live envelope, or
/// `None` when the wait elapsed with nothing to report (the JS pump then just
/// parks again). The `undefined` mapping of `None` happens in the host entry.
pub async fn recv_poll_value(id: u64, wait_ms: u64, to_worker: bool) -> Option<JsonValue> {
    match poll_batch(id, wait_ms, to_worker).await {
        PollWait::Gone => Some(gone_value()),
        PollWait::Idle => None,
        PollWait::Ready(batch) => Some(batch_to_value(batch)),
    }
}

/// Non-blocking typed drain. `None` means the queue is empty and the worker is
/// still running (the JS pump then awaits [`recv_poll_value`]); a terminal
/// state still answers an envelope so the pump can settle the exit.
pub fn try_recv_value(id: u64, to_worker: bool) -> Option<JsonValue> {
    let Some(shared) = worker_by_id(id) else {
        return Some(gone_value());
    };
    let queue = if to_worker {
        &shared.inbound
    } else {
        &shared.outbound
    };
    let (messages, dropped) = {
        let mut mailbox = WorkerShared::lock(queue);
        let dropped = mailbox.dropped;
        (mailbox.drain(), dropped)
    };
    let shutdown = shared.shutdown.load(Ordering::SeqCst);
    let done = shared.done.load(Ordering::SeqCst);
    if messages.is_empty() && !shutdown && !done {
        return None;
    }
    let exit_code = *WorkerShared::lock(&shared.exit_code);
    Some(batch_to_value(PollBatch {
        messages,
        stop: shutdown || done,
        done,
        exit_code,
        dropped,
    }))
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
    let accepted = WorkerShared::lock(mailbox).push(Queued::Text(data.to_owned()));
    if accepted {
        notify.notify_one();
    }
    accepted
}

/// Record + notify helper for the control records the parent pump dispatches
/// (`kind: "error" | "exit"`). Control records bypass [`MAX_MAILBOX_BYTES`]:
/// they are the ones that stop the pump, and they only ever arrive from the
/// worker's terminal paths (one boot failure or panic + one exit), so the
/// bypass cannot be used to grow the queue.
fn push_record(shared: &WorkerShared, record: JsonValue) {
    WorkerShared::lock(&shared.outbound).push_control(Queued::Value(record));
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
