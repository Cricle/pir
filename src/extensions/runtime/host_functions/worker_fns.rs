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
//! its final `exit` record, sets `done`, and leaves the registry. A worker
//! spinning in a pure JS loop (never awaiting) cannot observe the flag —
//! the interrupt-handler route is not wired here; `terminate` reports
//! `{done:false}` after its bounded wait instead of blocking forever.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Hard cap on live `worker_threads.Worker` threads for the whole process.
///
/// Each Worker is one OS thread plus a full QuickJS runtime; a JS loop that
/// creates Workers without terminating them would otherwise exhaust threads
/// and address space (64 MiB of reserved stack per worker). Node has no
/// explicit default cap, so this is a host safety bound, not a parity value:
/// 64 admits realistic parallel workloads (the subagent parser uses a handful)
/// while keeping the worst case bounded. Beyond it `new Worker` throws the
/// ordinary error the shim surfaces.
const MAX_WORKER_THREADS: usize = 64;

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

/// Upper bound of one `terminate` op's in-thread wait; the JS `terminate`
/// loops the op with `setTimeout(0)` yields between calls so the parent JS
/// thread stays responsive.
const MAX_TERMINATE_WAIT_MS: u64 = 200;

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

/// One live worker's mailboxes + lifecycle flags, shared between the parent
/// runtime thread, the worker thread, and the host ops.
struct WorkerShared {
    id: u64,
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
    /// Set by `terminate` or the worker's `process.exit`; every poll
    /// returns `stop` afterwards.
    shutdown: AtomicBool,
    /// Set by the worker thread right before it leaves the registry.
    done: AtomicBool,
    /// Exit code recorded by `workerExit` (the worker's `process.exit`).
    exit_code: StdMutex<Option<i32>>,
    /// Test/observability count of timer-fallback wakes in `recv_poll` (a
    /// delivered message does not count). Pins the adaptive park: an idle
    /// mailbox must wake once per (growing) slice, never on a fixed 25 ms
    /// timer.
    poll_wakes: AtomicU64,
}

impl WorkerShared {
    fn new(id: u64, generation: u64) -> Self {
        Self {
            id,
            generation,
            inbound: StdMutex::new(Mailbox::default()),
            outbound: StdMutex::new(Mailbox::default()),
            inbound_notify: tokio::sync::Notify::new(),
            outbound_notify: tokio::sync::Notify::new(),
            shutdown: AtomicBool::new(false),
            done: AtomicBool::new(false),
            exit_code: StdMutex::new(None),
            poll_wakes: AtomicU64::new(0),
        }
    }

    fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

static WORKERS: OnceLock<StdMutex<HashMap<u64, Arc<WorkerShared>>>> = OnceLock::new();
static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);
static ENV_DATA: OnceLock<StdMutex<HashMap<String, String>>> = OnceLock::new();

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
/// iteration and leaves; an OS thread spinning in pure synchronous JS cannot
/// be killed, so the entry is removed immediately (the parent pump sees
/// `stop`/`done` instead of a dead registry entry) and the thread's own `Arc`
/// keeps the mailboxes alive until it exits.
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
        shared.shutdown.store(true, Ordering::SeqCst);
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
        "parentSend" => op_send(&mut payload, "inbound"),
        "workerSend" => op_send(&mut payload, "outbound"),
        "workerExit" => op_worker_exit(&payload).map(OpAnswer::from),
        "terminate" => op_terminate(&payload).map(OpAnswer::from),
        "release" => op_release(&payload).map(OpAnswer::from),
        "envGet" => op_env_get(&payload).map(OpAnswer::from),
        "envSet" => op_env_set(&payload).map(OpAnswer::from),
        other => Err(format!("unknown worker op `{other}`")),
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
        Err(message) => serde_json::to_string(&OpErr {
            ok: false,
            code: "ERR_WORKER_OP",
            message: message.as_str(),
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
) -> Result<JsonValue, String> {
    if workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
        >= MAX_WORKER_THREADS
    {
        return Err(format!(
            "too many worker threads: the process limit is {MAX_WORKER_THREADS}"
        ));
    }
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
            return Err("worker eval source must be a non-empty string".to_owned());
        }
        let cwd = lock_state(state).config.cwd.display().to_string();
        (code, cwd, "[worker eval]".to_owned())
    } else {
        if filename.trim().is_empty() {
            return Err("worker filename must be a non-empty string".to_owned());
        }
        let source = std::fs::read_to_string(&filename)
            .map_err(|error| format!("Cannot find module '{filename}': {error}"))?;
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
    let shared = Arc::new(WorkerShared::new(id, generation));
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
        Ok(_handle) => Ok(serde_json::json!({ "id": id, "threadId": id })),
        Err(error) => {
            workers()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            Err(format!("failed to spawn worker thread: {error}"))
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
    shared.shutdown.store(true, Ordering::SeqCst);
    shared.inbound_notify.notify_one();
    shared.outbound_notify.notify_one();
    Ok(serde_json::json!({ "exiting": true }))
}

/// One bounded wait of the terminate loop. The JS side loops this op (with
/// executor yields between calls) until `done` or its own budget runs out.
fn op_terminate(payload: &JsonValue) -> Result<JsonValue, String> {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or("missing worker id")?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(MAX_TERMINATE_WAIT_MS)
        .min(MAX_TERMINATE_WAIT_MS);
    let Some(shared) = worker_by_id(id) else {
        return Ok(serde_json::json!({ "done": true, "exitCode": 1 }));
    };
    shared.shutdown.store(true, Ordering::SeqCst);
    shared.inbound_notify.notify_one();
    shared.outbound_notify.notify_one();
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    while Instant::now() < deadline {
        if shared.done.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
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
        .insert(id, Arc::new(WorkerShared::new(id, 0)));
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
    push_record(shared, serde_json::json!({ "kind": "exit", "code": code }));
    shared.done.store(true, Ordering::SeqCst);
    shared.outbound_notify.notify_one();
    shared.inbound_notify.notify_one();
    // Drop the registry entry: the thread's own Arc keeps the mailboxes alive
    // until the thread function returns.
    workers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&shared.id);
}

/// Thread entry: panic isolation around the whole worker run. A panicked
/// worker reports through the same `error` + `exit(1)` channel as a failed
/// boot, never leaving the parent pump waiting forever.
fn worker_thread_main(spec: WorkerSpec, shared: Arc<WorkerShared>) {
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
        fail(format!("worker boot failed: {error}"), &shared);
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
    let outcome = runtime
        .ctx
        .async_with(async |ctx| ctx.eval::<String, _>(run_source.clone()))
        .await;
    let outcome = match outcome {
        Ok(payload) => payload,
        Err(error) => {
            fail(format!("worker code failed to evaluate: {error}"), &shared);
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
        fail(format!("worker pump failed: {error}"), &shared);
        return;
    }

    let code = { *WorkerShared::lock(&shared.exit_code) }.unwrap_or(0);
    finish_worker(&shared, code);
}
