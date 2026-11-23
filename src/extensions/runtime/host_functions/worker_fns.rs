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
//! the worker runtime installs the shared `WorkerShared::abort` flag (in
//! `crate::pir_node::modules::worker`) as its QuickJS interrupt flag
//! ([`ExtensionRuntime::set_interrupt_flag`]), and
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
//! (`MAX_LIVE_WORKER_THREADS` in `crate::pir_node::modules::worker`) refuses new
//! workers instead of letting a `/reload` loop accumulate threads without
//! bound. The runtime's own Drop
//! evaluates `process.emit("exit")`; [`AbortWorkerOnDrop`] sets the same
//! interrupt flag first, so a synchronous infinite loop in a user exit
//! handler is bounded by the bytecode poll instead of pinning the thread.
//!
//! Queue bounds: each mailbox (inbound and outbound) holds at most
//! `MAX_MAILBOX_BYTES` (in `crate::pir_node::modules::worker`) of message text. Node's MessagePort queues without
//! bound and has no backpressure API; because each direction's sender is the
//! JS thread that the OTHER direction's pump runs on, parking a sender until
//! the receiver polls can deadlock the two pumps against each other — so the
//! bound drops the newest DATA message and reports it loudly (counter +
//! `ext_diag!` + the `dropped` field of both the posting answer and the
//! `workerRecvPoll` envelope). Control records (`error`/`exit`) are exempt.

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::extensions::api::embedded::Shim;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::pir_node::modules::worker::{WorkerExitGuard, WorkerHandle};
use rquickjs::promise::MaybePromise;
use rquickjs::Object;
use serde_json::Value as JsonValue;

use super::*;

use crate::extensions::runtime::native_bridge;
use crate::extensions::{ExtensionConfig, ExtensionLimits, ExtensionRuntime};

/// Stack for a worker thread: the QuickJS runtime + swc transpile recurse on
/// deep module graphs, so match the `pir js` script-thread budget
/// (`main_cli::js_mode::SCRIPT_THREAD_STACK`, itself 64 MiB).
pub(super) const WORKER_THREAD_STACK: usize = 64 * 1024 * 1024;

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

/// Worker-side boot overlay (`worker_boot.js.txt`): worker id / threadId /
/// workerData placeholders are replaced with JSON literals before eval.
static WORKER_BOOT: Shim = Shim::new(
    "worker_boot.js.txt",
    include_bytes!("../../../../assets/shims/worker_boot.js.txt.deflate"),
);

/// File-backed shims embedded by this module; `api::all_shims` aggregates it
/// with the `api` shim manifests for the byte-identity test.
pub(crate) const SHIMS: &[(&str, &Shim)] = &[("worker_boot.js.txt", &WORKER_BOOT)];

// ---------------------------------------------------------------------------
// Transport re-exports (implementation lives in `pir-node`)
// ---------------------------------------------------------------------------

// The registry, mailboxes, lifecycle/exit-code state machine, op table and
// poll envelope moved into `crates/pir-node/src/modules/worker.rs`. These
// re-exports keep the runtime-level seam names stable for the tests.
#[doc(hidden)]
pub use crate::pir_node::modules::worker::{
    live_worker_count_for_tests, live_worker_limit_for_tests, next_poll_slice_for_tests,
    orphaned_worker_count_for_tests, poll_slice_bounds_for_tests, release_generation,
    test_recv_poll, test_register_worker, test_release_worker, test_set_live_worker_limit,
    test_worker_poll_wakes, test_worker_send, worker_count_for_tests, worker_limit_for_tests,
};

/// Everything the worker thread needs to boot its runtime. `Send` by design:
/// this crosses the thread boundary (the rquickjs runtime itself never does).
pub(super) struct WorkerSpec {
    pub(super) id: u64,
    pub(super) code: String,
    pub(super) from_dir: String,
    pub(super) worker_data: String,
    pub(super) filename: String,
    pub(super) cwd: PathBuf,
    pub(super) roots: Vec<PathBuf>,
    /// The parent runtime's host `mode`/`has_ui`, inherited so the worker's
    /// own stdio routing matches the host it runs inside.
    pub(super) host_mode: String,
    pub(super) has_ui: bool,
    /// The parent runtime's resolved engine limits: the worker is a second
    /// QuickJS runtime in the same process, so it must observe the same heap
    /// cap (and stack/timeout policy).
    pub(super) limits: Option<ExtensionLimits>,
}

// ---------------------------------------------------------------------------
// Registration + dispatch
// ---------------------------------------------------------------------------

/// Worker id argument (`f64` from JS) as a registry key. Non-finite/negative
/// ids resolve to 0, which never matches an entry (ids start at 1).
fn worker_id_arg(id: f64) -> u64 {
    if id.is_finite() && id >= 0.0 {
        id as u64
    } else {
        0
    }
}

/// `workerRecvPoll` wait argument: clamped to the transport's 1 s maximum.
fn wait_ms_arg(wait_ms: f64) -> u64 {
    if wait_ms.is_finite() && wait_ms > 0.0 {
        (wait_ms as u64).min(1_000)
    } else {
        0
    }
}

/// Shared body of the two typed send entries.
///
/// `wrap` selects the wire shape, not the direction: the worker -> parent pump
/// dispatches `{kind: "msg", data}` control records, while parent -> worker
/// messages are bare values (the worker pump emits every inbound entry as a
/// `message` event, exactly like the legacy wire).
///
/// A value the bridge cannot represent answers `false` so the shim can throw
/// its existing `DataCloneError`; everything else (accepted, dropped because
/// the worker is gone, or the mailbox bound) answers `undefined`, matching
/// `postMessage`'s best-effort contract. The caller validates the
/// structured-clone subset in JS first: cycles would recurse in the bridge and
/// class instances/functions are indistinguishable from plain data there.
fn worker_send_typed<'js>(
    ctx: rquickjs::Ctx<'js>,
    id: f64,
    value: rquickjs::Value<'js>,
    to_worker: bool,
    wrap: bool,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    let json = match native_bridge::js_to_json(&value) {
        Ok(json) => json,
        Err(_) => return Ok(rquickjs::Value::new_bool(ctx, false)),
    };
    // A missing/shut-down worker or a full mailbox drops the message exactly
    // like the legacy `parentSend` answer the shim swallows; only a value the
    // bridge cannot represent answers `false` (the clone contract).
    let _ = crate::pir_node::modules::worker::send_value(worker_id_arg(id), json, to_worker, wrap);
    Ok(rquickjs::Value::new_undefined(ctx))
}

/// `__pirWorkerSend(id, value)` — worker side (`parentPort.postMessage`):
/// enqueue `{kind: "msg", data: value}` on the worker -> parent queue.
fn worker_send_native<'js>(
    ctx: rquickjs::Ctx<'js>,
    id: f64,
    value: rquickjs::Value<'js>,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    worker_send_typed(ctx, id, value, false, true)
}

/// `__pirWorkerSendValue(id, value)` — parent side (`Worker.postMessage`):
/// enqueue the bare value on the parent -> worker queue.
fn worker_send_value_native<'js>(
    ctx: rquickjs::Ctx<'js>,
    id: f64,
    value: rquickjs::Value<'js>,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    worker_send_typed(ctx, id, value, true, false)
}

/// `__pirWorkerTryRecv(id, isWorkerSide)` — non-blocking typed drain. Answers
/// the live envelope (`{messages, stop, done, exitCode, dropped}`) or
/// `undefined` when the queue is empty and the worker is still running.
fn worker_try_recv_native<'js>(
    ctx: rquickjs::Ctx<'js>,
    id: f64,
    to_worker: bool,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    match crate::pir_node::modules::worker::try_recv_value(worker_id_arg(id), to_worker) {
        Some(envelope) => native_bridge::json_to_js(&ctx, &envelope),
        None => Ok(rquickjs::Value::new_undefined(ctx)),
    }
}

/// `__pirWorkerRecvPoll(id, waitMs, isWorkerSide)` — awaited typed drain. Parks
/// on the mailbox notify (timer fallback) exactly like `workerRecvPoll`, but
/// answers live values; `undefined` on an idle timeout.
async fn worker_recv_poll_native<'js>(
    ctx: rquickjs::Ctx<'js>,
    id: f64,
    wait_ms: f64,
    to_worker: bool,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    let envelope = crate::pir_node::modules::worker::recv_poll_value(
        worker_id_arg(id),
        wait_ms_arg(wait_ms),
        to_worker,
    )
    .await;
    match envelope {
        Some(envelope) => native_bridge::json_to_js(&ctx, &envelope),
        None => Ok(rquickjs::Value::new_undefined(ctx)),
    }
}

/// Install `__pir_host.worker(op, payloadJson)` and the async poll entry.
///
/// Also installs the typed C-ABI message tier as plain globals
/// (`__pirWorkerSend` / `__pirWorkerSendValue` / `__pirWorkerTryRecv` /
/// `__pirWorkerRecvPoll`): live message values cross the seam once, instead of
/// the legacy `{id, data: JSON.stringify(...)}` envelope's repeated
/// serialization. The implementation is `pir-node`'s op table; this wrapper
/// only keeps the legacy string wire the worker shims call, resolving the
/// owning [`crate::pir_node::NodeRuntime`] from the context userdata.
pub(super) fn register_worker_host_fns(host: &Object) -> rquickjs::Result<()> {
    host.set(
        "worker",
        Func::from(
            |ctx: rquickjs::Ctx<'_>, op: String, payload: String| -> String {
                worker_call(&ctx, &op, &payload)
            },
        ),
    )?;
    host.set(
        "workerRecvPoll",
        Func::from(JsAsync(
            move |id: f64, wait_ms: f64, role: String| async move {
                crate::pir_node::modules::worker::recv_poll(worker_id_arg(id), wait_ms_arg(wait_ms), &role)
                    .await
            },
        )),
    )?;
    let globals = host.ctx().globals();
    globals.set("__pirWorkerSend", Func::from(worker_send_native))?;
    globals.set("__pirWorkerSendValue", Func::from(worker_send_value_native))?;
    globals.set("__pirWorkerTryRecv", Func::from(worker_try_recv_native))?;
    globals.set(
        "__pirWorkerRecvPoll",
        Func::from(JsAsync(worker_recv_poll_native)),
    )?;
    Ok(())
}

/// One `__pir_host.worker` call over the legacy `{ok,value}` wire.
fn worker_call(ctx: &rquickjs::Ctx<'_>, op: &str, payload: &str) -> String {
    let Some(state) = ctx.userdata::<super::NodeState>() else {
        return serde_json::json!({
            "ok": false,
            "code": "ENOSYS",
            "message": "node runtime is not available",
        })
        .to_string();
    };
    let node = state.0.clone();
    drop(state);
    super::node_string_call(&node, "worker", op, payload)
}

/// Sets the worker's abort flag when the worker function unwinds, just before
/// `ExtensionRuntime::drop` evaluates the process-exit teardown JS. A user
/// `process.on("exit")` handler with a synchronous infinite loop would
/// otherwise pin the worker thread (and its process-wide live-worker slot) forever on
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
pub(super) fn worker_thread_main(spec: WorkerSpec, handle: WorkerHandle) {
    // Registry-independent live-thread accounting: the slot is released when
    // this function returns (normal exit, interrupt, or panic unwind) — after
    // `release_generation` has already removed the registry entry.
    let _live_slot = WorkerExitGuard::new(&handle);
    let outcome = catch_unwind(AssertUnwindSafe(|| worker_thread_run(spec, handle.clone())));
    if let Err(panic) = outcome {
        let message = panic
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "worker thread panicked".to_owned());
        if !handle.is_done() {
            handle.push_error(&message, "");
            handle.finish(1);
        }
    }
}

fn worker_thread_run(spec: WorkerSpec, handle: WorkerHandle) {
    let runtime_build = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let runtime = match runtime_build {
        Ok(runtime) => runtime,
        Err(error) => {
            handle.push_error(&format!("worker runtime failed to start: {error}"), "");
            handle.finish(1);
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        run_worker_async(spec, handle).await;
    });
}

async fn run_worker_async(spec: WorkerSpec, handle: WorkerHandle) {
    let fail = |message: String| {
        handle.push_error(&message, "");
        handle.finish(1);
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
            fail(format!("worker runtime boot failed: {error}"));
            return;
        }
    };
    // The parent (`terminate` / `release_generation`) sets this flag without
    // touching this runtime; the QuickJS interrupt handler polls it at
    // bytecode boundaries, so a pure synchronous JS loop can be aborted.
    runtime.set_interrupt_flag(handle.abort_flag());
    // Declared after `runtime`: Rust drops locals in reverse order, so this
    // flips the abort flag immediately before `runtime`'s Drop runs the
    // `process.emit("exit")` teardown JS. That keeps a synchronous infinite
    // loop in a user exit handler from pinning this thread (P3-05).
    let _abort_on_drop = AbortWorkerOnDrop(handle.abort_flag());
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
        if handle.is_shutdown() {
            // The boot JS was interrupted by a terminate/reload that raced
            // the worker startup: a clean stop, not a boot failure.
            let code = handle.exit_code().unwrap_or(1);
            handle.finish(code);
        } else {
            fail(format!("worker boot failed: {error}"));
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
            if handle.is_shutdown() {
                // `terminate` / `/reload`: the abort flag deliberately tore
                // down the eval. This is a clean stop, not a worker error.
                // Do not touch the engine again: the abort flag is still set,
                // so any further JS entry (including reading the pending
                // exception) would be interrupted immediately.
                let code = handle.exit_code().unwrap_or(1);
                handle.finish(code);
            } else {
                // Read (and clear) the pending exception before the thread
                // drops the runtime: it distinguishes a deadline interrupt
                // from a genuine engine failure in the error record, and a
                // stale interrupt exception must not leak into teardown.
                let detail = runtime.take_pending_exception_text().await;
                let detail = detail.map(|text| format!(" ({text})")).unwrap_or_default();
                fail(format!(
                    "worker code exceeded its {code_budget:?} synchronous deadline or failed to evaluate: {error}{detail}"
                ));
            }
            return;
        }
    };
    let parsed: JsonValue = match serde_json::from_str(&outcome) {
        Ok(parsed) => parsed,
        Err(error) => {
            fail(format!("worker outcome parse failed: {error}"));
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
        handle.push_error(&message, stack);
        handle.finish(1);
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
        if handle.is_shutdown() {
            // The interrupt was our own `terminate`/`/reload` abort (the pump
            // was executing JS when the flag was observed): clean stop.
            let code = handle.exit_code().unwrap_or(1);
            handle.finish(code);
        } else {
            fail(format!("worker pump failed: {error}"));
        }
        return;
    }

    // The pump only returns Ok when it observed `stop`/`done`; a normal,
    // self-directed completion supplies its code through `process.exit`
    // (already stored). If no code was recorded, `finish_worker` consults the
    // stop reason: a terminate/generation release becomes Node's 1, a truly
    // natural return stays 0.
    handle.finish(0);
}
