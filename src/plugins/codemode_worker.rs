//! Worker-**process** isolation for the `codemode` sandbox — pir's mirror of
//! upstream `packages/codemode/src/runtime/{host,worker,protocol}.ts`.
//!
//! Upstream runs the QuickJS-WASM sandbox in a dedicated worker **process**
//! (`host.ts` spawns it, `worker.ts` runs the VM) and talks to it over a
//! typed message channel: worker→host `call`/`output`/`done`/`crash`, and
//! host→worker `result`. pir's in-process thread sandbox (the fallback below)
//! predates this; this module adds the process mode:
//!
//! - The parent spawns `<current_exe> __codemode-worker` (a hidden subcommand
//!   intercepted in `main.rs` before clap parsing, so it never appears in
//!   `--help`) and drives a **versioned JSON-lines protocol** over stdio.
//! - The child only runs JS. Tool calls cross back to the parent as `call`
//!   frames; the parent executes them through the same nested-call executor
//!   as the thread sandbox and replies with `result` frames (upstream's
//!   worker↔host protocol shape). `output` frames stream text/console items
//!   live; `done`/`crash` end the script.
//! - The parent keeps ALL the budget semantics: the output buffer and its
//!   caps live parent-side (identical [`super::OutputBuffer`] accounting),
//!   the deadline is enforced both by the child's QuickJS interrupt handler
//!   and by a parent-side kill (`Timeout`), and abort is delivered as an
//!   `abort` frame that the child's interrupt handler honors — with a
//!   grace-period kill if the worker does not stop.
//! - ONE frame-size definition for the whole protocol: a frame line's
//!   serialized byte count (escaping and the trailing newline included),
//!   capped by [`super::frame_line_cap`]. The output budget
//!   ([`super::MAX_OUTPUT_CHARS`]) is measured in the same escaped bytes, so
//!   a payload the sandbox accounting admits always fits one frame; every
//!   writer checks the definition BEFORE writing (an over-cap tool result /
//!   call frame becomes an explicit host-side error reply, an over-cap init
//!   frame falls back to the thread sandbox, an over-cap final frame is
//!   replaced by a small explicit limit error) and every reader refuses an
//!   over-cap line instead of buffering unboundedly.
//! - **Global live-worker admission**: at most
//!   [`MAX_CONCURRENT_LIVE_WORKERS`] worker **processes** execute at once in
//!   the whole process (upstream `MAX_CONCURRENT_CODEMODE_WORKERS = 2`). A
//!   run waits for a slot ([`acquire_live_worker_slot`]) right before the
//!   child is spawned and holds the owned permit until the child is reaped
//!   and its in-flight tool tasks settle, so normal exit, error, abort, panic
//!   and future cancellation all release it. Upstream throws at the cap;
//!   queueing is pir's deliberate port (a script is not failed because two
//!   other workers are live). The in-process thread sandbox — the documented
//!   fallback when the worker process is unavailable — is deliberately NOT
//!   admission-gated: its fallback decision and engine semantics are
//!   unchanged.
//!
//! ## Fallback semantics
//!
//! [`run_in_worker_process`] returns `None` (= fall back to the in-process
//! thread sandbox) only while NO script side effects can have happened:
//! spawn failure, init-frame write failure (including an over-cap init
//! frame), or a failed/bounded handshake (no `ready` frame, wrong protocol
//! version, or the exe does not implement the hidden subcommand — e.g. a
//! foreign `current_exe`). The support verdict is memoized per process, so
//! `cargo test` binaries (whose `current_exe` is the test harness) probe
//! once and then run thread mode. After the handshake, protocol failures
//! FAIL the script instead of falling back — re-running the script would
//! duplicate tool-call side effects.

use std::collections::HashMap;
use std::io::{BufRead, Read as StdRead, Write as IoWrite};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::agent::nested_tool_calls::NestedToolExecutor;
use crate::ai::types::AbortSignal;
// Post-timeout / post-interrupt recovery: the same helper the thread sandbox
// uses (see the parent module's imports).
use crate::extensions::runtime::recover_interrupted as recover;

use super::{OutputBuffer, OutputItem, SandboxLimits, ScriptOutcome, SANDBOX_PRELUDE};

// ---------------------------------------------------------------------------
// Wire protocol (versioned)
// ---------------------------------------------------------------------------

/// Wire-protocol version. Carried on EVERY frame as `v`; a mismatch on the
/// handshake (or any frame) means the exe is not a compatible worker and the
/// parent falls back to the thread sandbox.
pub const CODEMODE_WORKER_PROTOCOL_VERSION: u32 = 1;

/// Default single-frame size cap for the JSON-lines protocol, at the default
/// output budget: [`super::MAX_OUTPUT_CHARS`] (the output budget) plus
/// [`super::FRAME_ENVELOPE_MARGIN`] (the constant envelope overhead of one
/// frame line). The ENFORCED cap is [`super::frame_line_cap`], which scales
/// with `PIR_CODEMODE_MAX_OUTPUT_CHARS`; this const is that function's value
/// when no override is set, and stays the public/default anchor the tests
/// and docs use.
///
/// The single definition the whole protocol enforces in BOTH directions:
/// the SERIALIZED FRAME LINE's byte count (the JSON envelope, all escaping
/// and the trailing newline included). Every reader and every writer checks
/// that one number before a frame crosses the pipe:
///
/// - readers refuse a completed line over the cap instead of buffering it
///   unboundedly (the child's stdin reads use `take(cap + 1)`, so the memory
///   spent before the verdict is bounded by one line);
/// - writers refuse an over-cap frame BEFORE writing it, so a frame is never
///   delivered and then judged (the parent turns an over-cap result into an
///   explicit host-side error reply, the child refuses an over-cap tool-call
///   frame as a host-side tool error, and an over-cap init frame falls back
///   to the thread sandbox before any side effect — see the call sites);
/// - `MAX_OUTPUT_CHARS` is itself measured in the same escaped bytes
///   (codemode.rs `frame_line_cap`), so a payload admitted by the sandbox
///   accounting always fits one frame: the guarantee "a legal payload up to
///   the 16 MiB budget is transmittable" holds by construction instead of by
///   the two limits coincidentally matching. What the accounting does NOT
///   cover (residual boundary): a script's RETURN VALUE and a bridge call's
///   arguments are not output-budget items, so an over-cap frame from one of
///   those is refused with an explicit error naming this limit (never a
///   silent kill, never a "script crash" classification).
pub const MAX_FRAME_LINE_BYTES: usize = super::MAX_OUTPUT_CHARS + super::FRAME_ENVELOPE_MARGIN;

/// The read-side over-limit verdict, shared by the parent's frame reader, the
/// child's init read, and the child's stdin reader thread, so both sides
/// report the same error text for an over-limit frame.
pub fn frame_size_limit_message() -> String {
    format!(
        "codemode worker frame exceeds the protocol size limit ({} bytes)",
        super::frame_line_cap()
    )
}

/// The write-side over-limit verdict: a frame was refused BEFORE it was
/// written, with the attempted size known. Used by both directions' writers
/// so an over-cap frame reports an explicit boundary error instead of
/// surfacing as a worker crash or a script crash.
pub fn frame_over_limit_message(bytes: usize) -> String {
    format!(
        "codemode worker frame exceeds the protocol size limit ({bytes} bytes > {} bytes)",
        super::frame_line_cap()
    )
}

/// The script-visible host-side error for a payload the frame definition
/// refuses (`what` = "tool call arguments" in the child, "the tool result"
/// in the parent). The bridge turns it into a regular tool-call failure, so
/// an over-cap payload is a failed call — never a protocol teardown and
/// never a "script crash" classification.
pub fn frame_payload_limit_message(what: &str, bytes: usize) -> String {
    format!(
        "host error: {what} too large for the codemode worker frame limit ({bytes} bytes > {} bytes)",
        super::frame_line_cap()
    )
}

/// The hidden subcommand the parent spawns (`<current_exe> __codemode-worker`).
/// Handled in `main.rs` before clap parsing so it stays out of `--help`.
pub const WORKER_SUBCOMMAND: &str = "__codemode-worker";

/// Parent → worker frames (upstream `HostToWorkerMessage` + init/abort).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// First frame: the script and the sandbox budgets (upstream `WorkerData`:
    /// code, tool list, memory limit; the timeout feeds the interrupt
    /// deadline). Tool calls execute in the PARENT, so nothing else crosses.
    Init {
        v: u32,
        code: String,
        /// Allowlist of tool names (upstream `tools`); `None` allows every
        /// registered tool except `codemode` itself.
        #[serde(default)]
        allowed_tools: Option<Vec<String>>,
        max_output_chars: usize,
        max_output_items: usize,
        /// Hard deadline for the whole script, including tool calls.
        timeout_ms: u64,
        /// QuickJS heap cap in bytes (`codemode.sandboxMemoryMb`, the
        /// `PIR_CODEMODE_MEMORY_MB` override, or the 128 MiB default);
        /// `None` = no cap. Defaulted on decode so an older/simpler init
        /// frame stays valid (the child then runs uncapped, like the
        /// pre-configuration default of a custom frame).
        #[serde(default)]
        memory_bytes: Option<u64>,
    },
    /// Reply to a `call` frame (upstream `{type:"result", id, ok, payload}`).
    /// `json` marks `value` as structured output (upstream resolves
    /// outputSchema tools to structuredContent).
    Result {
        v: u32,
        id: u64,
        ok: bool,
        #[serde(default)]
        value: String,
        #[serde(default)]
        json: bool,
        #[serde(default)]
        error: Option<String>,
    },
    /// Cancel the script: the child's QuickJS interrupt handler stops the JS
    /// job uncatchably (the same mechanics as the thread sandbox's abort).
    Abort { v: u32 },
}

/// Worker → parent frames (upstream `WorkerToHostMessage` + ready/crash).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// Handshake ack, sent right after the init frame is accepted (before any
    /// script runs). The parent's fallback gate.
    Ready { v: u32 },
    /// Tool call (upstream `{type:"call", id, target:"tool", name, args}`);
    /// `args` is the JSON-serialized arguments text, exactly what the thread
    /// sandbox bridge passes.
    Call {
        v: u32,
        id: u64,
        name: String,
        args: String,
    },
    /// One output item, `kind` = `"text"` | `"console"` (upstream
    /// `{type:"output", item}`).
    Output { v: u32, kind: String, text: String },
    /// Script finished (upstream `done`). `value` is the return-value JSON
    /// text (thread-sandbox bridge shape); `name`/`error` classify a failed
    /// script; `head` (when present) is the verbatim error head for
    /// worker-side failures — setup errors, JS errors, the timeout — which
    /// the parent must not re-classify.
    Done {
        v: u32,
        ok: bool,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        head: Option<String>,
    },
    /// The VM failed outside the script's control (upstream `crash`).
    Crash { v: u32, message: String },
}

/// Serialize a parent→worker frame (no trailing newline; see the writers).
pub fn encode_request(frame: &WorkerRequest) -> Result<String, serde_json::Error> {
    serde_json::to_string(frame)
}

/// Serialize a worker→parent frame.
pub fn encode_event(frame: &WorkerEvent) -> Result<String, serde_json::Error> {
    serde_json::to_string(frame)
}

fn request_version(frame: &WorkerRequest) -> u32 {
    match frame {
        WorkerRequest::Init { v, .. }
        | WorkerRequest::Result { v, .. }
        | WorkerRequest::Abort { v } => *v,
    }
}

fn event_version(frame: &WorkerEvent) -> u32 {
    match frame {
        WorkerEvent::Ready { v }
        | WorkerEvent::Call { v, .. }
        | WorkerEvent::Output { v, .. }
        | WorkerEvent::Done { v, .. }
        | WorkerEvent::Crash { v, .. } => *v,
    }
}

/// Decode one parent→worker frame from a JSON line. Tolerates trailing
/// `\r\n` (Windows pipes) and surrounding whitespace; rejects unknown frame
/// types and wrong protocol versions so the caller can fall back cleanly.
pub fn decode_request(line: &str) -> Result<WorkerRequest, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("empty request frame".to_owned());
    }
    let frame: WorkerRequest =
        serde_json::from_str(line).map_err(|error| format!("invalid request frame: {error}"))?;
    let got = request_version(&frame);
    if got != CODEMODE_WORKER_PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch (expected {CODEMODE_WORKER_PROTOCOL_VERSION}, got {got})"
        ));
    }
    Ok(frame)
}

/// Decode one worker→parent frame from a JSON line (same rules).
pub fn decode_event(line: &str) -> Result<WorkerEvent, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("empty event frame".to_owned());
    }
    let frame: WorkerEvent =
        serde_json::from_str(line).map_err(|error| format!("invalid event frame: {error}"))?;
    let got = event_version(&frame);
    if got != CODEMODE_WORKER_PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch (expected {CODEMODE_WORKER_PROTOCOL_VERSION}, got {got})"
        ));
    }
    Ok(frame)
}

/// One JSON line: frame bytes + `\n` (never `\r\n`; readers trim `\r`).
fn frame_line(encoded: Result<String, serde_json::Error>) -> std::io::Result<String> {
    let mut line =
        encoded.map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push('\n');
    Ok(line)
}

/// Size-check one serialized frame line against the single definition (the
/// line's byte count, newline included): `Ok(line)` when it fits, `Err(bytes)`
/// when it does not — the caller must not write it.
fn check_frame_line(line: String) -> Result<String, usize> {
    if line.len() > super::frame_line_cap() {
        Err(line.len())
    } else {
        Ok(line)
    }
}

/// Serialize one worker→parent frame and enforce the frame-size definition
/// BEFORE it crosses the protocol. `Ok(Ok(line))` is write-ready;
/// `Ok(Err(bytes))` is an over-cap frame that was NOT written (the attempted
/// size is carried so the caller can report it); `Err` is a serialization
/// failure.
fn checked_event_line(frame: &WorkerEvent) -> std::io::Result<Result<String, usize>> {
    Ok(check_frame_line(frame_line(encode_event(frame))?))
}

/// Serialize one parent→worker frame under the same definition.
fn checked_request_line(frame: &WorkerRequest) -> std::io::Result<Result<String, usize>> {
    Ok(check_frame_line(frame_line(encode_request(frame))?))
}

/// Write one already-serialized frame line to stdout (locked, flushed).
fn write_out_line(line: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(line.as_bytes())?;
    lock.flush()
}

// ---------------------------------------------------------------------------
// Shared outcome assembly (used by BOTH the thread sandbox and the
// worker-process parent loop)
// ---------------------------------------------------------------------------

/// Assemble the model-visible [`ScriptOutcome`] from a script classification
/// plus the output items the run produced (upstream `execute.ts`: the output
/// items — with the return value appended as one text item — then the error
/// head). Shared by the thread sandbox's `execute_sandbox` and the
/// worker-process parent loop so both keep the exact caps/truncation
/// semantics; the output-cap enforcement itself happens where the buffer is
/// drained (the caller knows whether the buffer refused an item).
pub(super) fn assemble_script_outcome(
    ok: bool,
    error_head: Option<String>,
    return_value: Option<String>,
    items: Vec<OutputItem>,
    limits: SandboxLimits,
) -> ScriptOutcome {
    let mut items = items;
    // A top-level `return <value>` adds output (upstream contract); `exit()`
    // and a bare `return` produce no value item.
    if ok {
        if let Some(value) = return_value {
            if !value.is_empty() && value != "null" {
                items.push(OutputItem {
                    kind: "text",
                    text: value,
                });
            }
        }
    }
    let partial = super::render_output(&items);
    // The error travels separately (upstream `formatError` appends it after
    // the output items), so head-truncation of the output can never cut it
    // off.
    let output_text = super::truncate_to_budget(&partial, limits.inline_budget);
    ScriptOutcome {
        ok,
        output: output_text,
        error_head,
    }
}

// ---------------------------------------------------------------------------
// The worker child process: `pir __codemode-worker`
// ---------------------------------------------------------------------------

/// Parsed init frame (the child's whole view of the run).
struct WorkerInit {
    code: String,
    allowed_tools: Option<Vec<String>>,
    max_output_chars: usize,
    max_output_items: usize,
    timeout: Duration,
    /// QuickJS heap cap in bytes (`None` = no cap).
    memory_bytes: Option<usize>,
}

/// A `result` reply routed to a pending bridge call.
struct ToolReply {
    ok: bool,
    value: String,
    json: bool,
    error: Option<String>,
}

/// Pending bridge calls: call id → the one-shot sender the awaiting bridge
/// closure polls. The stdin reader thread resolves them.
type PendingMap = Arc<StdMutex<HashMap<u64, tokio::sync::oneshot::Sender<ToolReply>>>>;

/// In-flight parent-side tool-call tasks (one spawned tokio task per `Call`
/// frame). The parent event loop collects their
/// [`tokio::task::JoinHandle`]s here so every teardown path can cancel or
/// await whatever is still running: the init frame's timeout is a hard
/// deadline for the WHOLE script, including tool calls, so a timed-out or
/// aborted script must not leave a tool executing in the background.
pub type ToolTaskSet = Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>>;

/// How the child's script run ended.
enum WorkerExit {
    Done(ScriptDone),
    Crash(String),
}

/// The `done` payload (see [`WorkerEvent::Done`]).
struct ScriptDone {
    ok: bool,
    value: Option<String>,
    name: Option<String>,
    error: Option<String>,
    head: Option<String>,
}

impl ScriptDone {
    /// A verbatim-head failure (worker-side setup errors, JS errors, the
    /// deadline): the parent must not re-classify these.
    fn failed(head: String) -> Self {
        Self {
            ok: false,
            value: None,
            name: None,
            error: None,
            head: Some(head),
        }
    }
}

/// Write one event frame to stdout (locked, flushed, `\n`-terminated, UTF-8
/// by construction — serde only produces valid UTF-8 text). The frame-size
/// definition is enforced BEFORE the write: an over-cap frame is refused
/// with [`frame_over_limit_message`] as an `InvalidData` error (nothing is
/// written), so an over-cap frame is never delivered and then judged by the
/// parent's reader.
fn write_event(frame: &WorkerEvent) -> std::io::Result<()> {
    let line = match checked_event_line(frame)? {
        Ok(line) => line,
        Err(bytes) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                frame_over_limit_message(bytes),
            ))
        }
    };
    write_out_line(&line)
}

/// The hidden-subcommand entry point (`main.rs` intercepts
/// `pir __codemode-worker`). Reads the versioned JSON-lines protocol from
/// stdin and writes event frames to stdout; exits quietly on EOF before the
/// init frame. The script budget semantics are the thread sandbox's: the
/// same prelude, the same QuickJS memory limit and interrupt-handler
/// deadline mechanics, and output-cap accounting in the prelude (mirrored by
/// the parent's buffer).
pub fn run_worker_main() -> crate::Result<()> {
    // First frame must be the init frame; EOF before it exits quietly.
    let init = {
        let mut line = String::new();
        // Bounded read: one frame, capped at the effective frame-size limit
        // (`frame_line_cap()`; the +1 lets read_line see the newline of an
        // exactly-at-cap line, so the verdict below is "over the cap", never
        // a silent truncation).
        let read = {
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            lock.by_ref()
                .take((super::frame_line_cap() + 1) as u64)
                .read_line(&mut line)
                .unwrap_or(0)
        };
        if read == 0 {
            return Ok(());
        }
        if read as usize > super::frame_line_cap() {
            let _ = write_event(&WorkerEvent::Crash {
                v: CODEMODE_WORKER_PROTOCOL_VERSION,
                message: frame_size_limit_message(),
            });
            return Ok(());
        }
        match decode_request(&line) {
            Ok(WorkerRequest::Init {
                code,
                allowed_tools,
                max_output_chars,
                max_output_items,
                timeout_ms,
                memory_bytes,
                ..
            }) => WorkerInit {
                code,
                allowed_tools,
                max_output_chars,
                max_output_items,
                timeout: Duration::from_millis(timeout_ms.max(1)),
                memory_bytes: memory_bytes
                    .map(|bytes| usize::try_from(bytes).unwrap_or(usize::MAX)),
            },
            _ => {
                let _ = write_event(&WorkerEvent::Crash {
                    v: CODEMODE_WORKER_PROTOCOL_VERSION,
                    message: "invalid or unsupported init frame".to_owned(),
                });
                return Ok(());
            }
        }
    };
    let _ = write_event(&WorkerEvent::Ready {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
    });

    let pending: PendingMap = Arc::default();
    let abort_flag = Arc::new(AtomicBool::new(false));
    let reader = spawn_reader_thread(Arc::clone(&pending), Arc::clone(&abort_flag));

    // The sandbox runtime must not be built inside the outer tokio block_on
    // context (a nested `block_on` panics); run it on a fresh thread exactly
    // like the in-process thread sandbox does.
    let pending_for_run = Arc::clone(&pending);
    let abort_for_run = Arc::clone(&abort_flag);
    // The sandbox grants JS `SANDBOX_JS_STACK_BYTES` of stack (run_script);
    // the OS thread reserves the same derived budget as the in-process thread
    // sandbox (`super::sandbox_thread_stack_bytes`: 2x the JS stack + native
    // headroom), so deep recursion hits QuickJS's RangeError before the OS
    // stack overflows.
    let exit = match std::thread::Builder::new()
        .name("pir-codemode-worker-sandbox".to_owned())
        .stack_size(super::sandbox_thread_stack_bytes())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run_script(init, pending_for_run, abort_for_run)),
                Err(error) => {
                    WorkerExit::Crash(format!("failed to start the sandbox worker: {error}"))
                }
            }
        }) {
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|_| WorkerExit::Crash("sandbox worker thread panicked".to_owned())),
        Err(error) => WorkerExit::Crash(format!("failed to start the sandbox worker: {error}")),
    };

    match exit {
        WorkerExit::Done(done) => {
            let frame = WorkerEvent::Done {
                v: CODEMODE_WORKER_PROTOCOL_VERSION,
                ok: done.ok,
                value: done.value,
                name: done.name,
                error: done.error,
                head: done.head,
            };
            write_final_event(frame);
        }
        WorkerExit::Crash(message) => {
            let frame = WorkerEvent::Crash {
                v: CODEMODE_WORKER_PROTOCOL_VERSION,
                message,
            };
            write_final_event(frame);
        }
    }
    // Detach (do NOT join) the stdin reader: it blocks on EOF, and a host
    // that keeps stdin open after `done` — while waiting for our exit —
    // would deadlock the handshake. The thread dies with the process; any
    // late frames land in `pending`, which nothing reads after this point.
    drop(reader);
    Ok(())
}

/// Write the final frame of the run under the frame-size definition. An
/// over-cap final frame (a script return value or error text bigger than one
/// frame can carry — the return value and error text are not output-budget
/// items) is replaced by a small explicit failure `done` naming the boundary,
/// so the outcome reports a host-side limit error instead of the parent's
/// reader judging an over-cap line (which classified the run as a sandbox
/// crash).
fn write_final_event(frame: WorkerEvent) {
    match checked_event_line(&frame) {
        Ok(Ok(line)) => {
            let _ = write_out_line(&line);
        }
        Ok(Err(bytes)) => {
            // The replacement is small by construction (the cap always
            // reserves the envelope margin), so its own check cannot trip.
            let failed = WorkerEvent::Done {
                v: CODEMODE_WORKER_PROTOCOL_VERSION,
                ok: false,
                value: None,
                name: None,
                error: None,
                head: Some(format!(
                    "Script sandbox failed: the script result exceeds the codemode worker frame limit ({bytes} bytes > {} bytes)",
                    super::frame_line_cap()
                )),
            };
            let _ = write_event(&failed);
        }
        Err(_) => {}
    }
}

/// The stdin reader thread: routes `result` frames to their pending bridge
/// calls and flips the abort flag on `abort` frames. Unknown frames are
/// ignored (forward compatibility). On EOF every pending call is failed by
/// dropping its sender, so a dead host cannot hang the script forever.
fn spawn_reader_thread(
    pending: PendingMap,
    abort_flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut line = String::new();
        // The effective frame cap does not change during the run (it derives
        // from the environment inherited at spawn): compute once.
        let cap = super::frame_line_cap();
        loop {
            line.clear();
            // Bounded read (see the frame-size definition): a line over the
            // effective cap is a host protocol failure — report the shared
            // over-limit verdict to the host (never a silent kill), flip the
            // abort flag so a running script stops, and fail every pending
            // call (the break below drops the senders).
            let read = reader.by_ref().take((cap + 1) as u64).read_line(&mut line);
            match read {
                Ok(0) | Err(_) => break,
                Ok(read) if read > cap => {
                    let _ = write_event(&WorkerEvent::Crash {
                        v: CODEMODE_WORKER_PROTOCOL_VERSION,
                        message: frame_size_limit_message(),
                    });
                    abort_flag.store(true, Ordering::Release);
                    break;
                }
                Ok(_) => {}
            }
            match decode_request(&line) {
                Ok(WorkerRequest::Result {
                    id,
                    ok,
                    value,
                    json,
                    error,
                    ..
                }) => {
                    let sender = pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                    if let Some(sender) = sender {
                        let _ = sender.send(ToolReply {
                            ok,
                            value,
                            json,
                            error,
                        });
                    }
                }
                Ok(WorkerRequest::Abort { .. }) => abort_flag.store(true, Ordering::Release),
                Ok(WorkerRequest::Init { .. }) => {}
                Err(_) => {}
            }
        }
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    })
}

/// Run one script end-to-end in the child: the same runtime assembly as the
/// thread sandbox's `execute_in_runtime`, with the tool bridge replaced by
/// the frame protocol and the abort signal replaced by the abort flag.
async fn run_script(
    init: WorkerInit,
    pending: PendingMap,
    abort_flag: Arc<AtomicBool>,
) -> WorkerExit {
    use rquickjs::prelude::{Async as JsAsync, Func};
    use rquickjs::promise::MaybePromise;
    use rquickjs::{AsyncContext, AsyncRuntime};

    let deadline: Arc<StdMutex<Option<Instant>>> = Arc::default();
    let runtime = match AsyncRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            return WorkerExit::Done(ScriptDone::failed(format!(
                "failed to start the codemode sandbox: {error}"
            )))
        }
    };
    // The JS stack budget the sandbox thread's OS stack is derived from
    // (`super::sandbox_thread_stack_bytes`, spawned in `run_worker_main`): the
    // 1 MiB engine default RangeErrors far too early for real recursive
    // scripts.
    runtime
        .set_max_stack_size(super::SANDBOX_JS_STACK_BYTES)
        .await;
    // Heap cap from the init frame (`codemode.sandboxMemoryMb` /
    // `PIR_CODEMODE_MEMORY_MB`, resolved by the parent — the child inherits
    // the parent's configuration instead of re-reading settings); the frame's
    // `None` (or an older frame without the field) leaves QuickJS uncapped.
    if let Some(memory_bytes) = init.memory_bytes {
        runtime.set_memory_limit(memory_bytes).await;
    }
    // Interrupt handler: aborts the JS job once the deadline passes OR the
    // parent delivered an abort frame (the reader thread flips the flag).
    {
        let deadline = Arc::clone(&deadline);
        let abort = Arc::clone(&abort_flag);
        let _ = runtime
            .set_interrupt_handler(Some(Box::new(move || {
                if abort.load(Ordering::Acquire) {
                    return true;
                }
                let guard = deadline.lock().unwrap_or_else(PoisonError::into_inner);
                match *guard {
                    Some(deadline) => Instant::now() >= deadline,
                    None => false,
                }
            })))
            .await;
    }
    let ctx = match AsyncContext::full(&runtime).await {
        Ok(ctx) => ctx,
        Err(error) => {
            return WorkerExit::Done(ScriptDone::failed(format!(
                "failed to start the codemode sandbox: {error}"
            )))
        }
    };

    let id_counter = Arc::new(AtomicU64::new(0));
    {
        let pending = Arc::clone(&pending);
        let id_counter = Arc::clone(&id_counter);
        let abort = Arc::clone(&abort_flag);
        let registered = ctx
            .async_with(async |js_ctx| {
                let globals = js_ctx.globals();
                // Registered as `__pir_hostOutput`: the prelude wraps it with
                // the output-cap accounting (see SANDBOX_PRELUDE); each item
                // streams to the parent as one output frame. The prelude
                // bounds every item by its ESCAPED frame size (the same
                // measure the frame definition counts), so an over-cap output
                // frame can only come from a tampered prelude: refuse the
                // frame (write_event enforces the definition) and stop the
                // script — the same tamper defense the parent's buffer
                // mirrors.
                let output_abort = Arc::clone(&abort);
                globals.set(
                    "__pir_hostOutput",
                    Func::from(move |kind: String, text: String| {
                        let kind = if kind == "console" { "console" } else { "text" };
                        let frame = WorkerEvent::Output {
                            v: CODEMODE_WORKER_PROTOCOL_VERSION,
                            kind: kind.to_owned(),
                            text,
                        };
                        if write_event(&frame).is_err() {
                            output_abort.store(true, Ordering::Release);
                        }
                    }),
                )?;
                // Host-configured output caps for the prelude's accounting.
                js_ctx.eval::<(), _>(format!(
                    "globalThis.__pir_maxOutputChars = {}; globalThis.__pir_maxOutputItems = {};",
                    init.max_output_chars, init.max_output_items
                ))?;
                // The single host capability: a tool call frame to the
                // parent, whose `result` reply is rebuilt into the exact
                // bridge-reply JSON the thread sandbox's prelude parses.
                globals.set(
                    "__pir_callTool",
                    Func::from(JsAsync(
                        move |_target: String, name: String, args_json: String| {
                            let pending = Arc::clone(&pending);
                            let id_counter = Arc::clone(&id_counter);
                            let abort = Arc::clone(&abort);
                            async move {
                                if name == "codemode" {
                                    return r#"{"error":"scripts must not start other scripts"}"#
                                        .to_owned();
                                }
                                if abort.load(Ordering::Acquire) {
                                    return r#"{"error":"aborted"}"#.to_owned();
                                }
                                let id = id_counter.fetch_add(1, Ordering::Relaxed) + 1;
                                let (tx, rx) = tokio::sync::oneshot::channel::<ToolReply>();
                                pending
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .insert(id, tx);
                                let call = WorkerEvent::Call {
                                    v: CODEMODE_WORKER_PROTOCOL_VERSION,
                                    id,
                                    name: name.clone(),
                                    args: args_json,
                                };
                                let line = match checked_event_line(&call) {
                                    Ok(Ok(line)) => line,
                                    // Over-cap call frame (arguments bigger than
                                    // one frame can carry): refuse it BEFORE it
                                    // crosses the pipe and fail the call with an
                                    // explicit host-side error — the script sees
                                    // a regular tool-call rejection instead of
                                    // the run being torn down as a protocol
                                    // failure.
                                    Ok(Err(bytes)) => {
                                        pending
                                            .lock()
                                            .unwrap_or_else(PoisonError::into_inner)
                                            .remove(&id);
                                        return serde_json::to_string(&serde_json::json!({
                                            "ok": false,
                                            "error": frame_payload_limit_message(
                                                "tool call arguments",
                                                bytes,
                                            ),
                                        }))
                                        .unwrap_or_else(|_| {
                                            r#"{"error":"tool call rejected"}"#.to_owned()
                                        });
                                    }
                                    Err(_) => {
                                        pending
                                            .lock()
                                            .unwrap_or_else(PoisonError::into_inner)
                                            .remove(&id);
                                        return r#"{"error":"worker lost connection to the host"}"#
                                            .to_owned();
                                    }
                                };
                                if write_out_line(&line).is_err() {
                                    pending
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .remove(&id);
                                    return r#"{"error":"worker lost connection to the host"}"#
                                        .to_owned();
                                }
                                let reply = match rx.await {
                                    Ok(reply) => reply,
                                    Err(_) => {
                                        return r#"{"error":"tool result channel dropped"}"#
                                            .to_owned()
                                    }
                                };
                                if reply.ok {
                                    serde_json::to_string(&serde_json::json!({
                                        "ok": true,
                                        "value": reply.value,
                                        "json": reply.json,
                                    }))
                                    .unwrap_or_else(|_| {
                                        r#"{"error":"tool result serialization failed"}"#.to_owned()
                                    })
                                } else {
                                    serde_json::to_string(&serde_json::json!({
                                        "ok": false,
                                        "error": reply
                                            .error
                                            .unwrap_or_else(|| format!("tool '{name}' failed")),
                                    }))
                                    .unwrap_or_else(|_| r#"{"error":"tool failed"}"#.to_owned())
                                }
                            }
                        },
                    )),
                )?;
                // Allowlist default: everything (replaced before the prelude
                // runs when a `tools` list was requested — the prelude
                // captures the list into its closure and clears the global,
                // so the script can neither widen it nor reach the raw
                // bridge). A JSON array literal is a valid JS expression.
                js_ctx.eval::<(), _>("globalThis.__pir_allowedTools = null;")?;
                if let Some(allowed) = &init.allowed_tools {
                    let allowed = serde_json::to_string(allowed).unwrap_or_default();
                    js_ctx.eval::<(), _>(format!("globalThis.__pir_allowedTools = {allowed};"))?;
                }
                js_ctx.eval::<(), _>(SANDBOX_PRELUDE)?;
                Ok::<_, rquickjs::Error>(())
            })
            .await;
        if let Err(error) = registered {
            return WorkerExit::Done(ScriptDone::failed(format!(
                "failed to prepare the codemode sandbox: {error}"
            )));
        }
    }

    // Arm the interrupt deadline and eval the script under the timeout (the
    // deadline the parent resolved; the parent kill is the outer bound).
    let timeout = init.timeout;
    let started = Instant::now();
    *deadline.lock().unwrap_or_else(PoisonError::into_inner) = Some(started + timeout);
    let call = ctx.async_with(async |js_ctx| {
        let run: rquickjs::Function = js_ctx.globals().get("__pir_run")?;
        let result: MaybePromise = run.call((init.code,))?;
        result.into_future::<String>().await
    });
    let outcome = tokio::time::timeout(timeout, call).await;
    // Disarm regardless of outcome.
    *deadline.lock().unwrap_or_else(PoisonError::into_inner) = None;

    match outcome {
        Ok(Ok(reply_json)) => {
            recover(&ctx, &runtime).await;
            let parsed: JsonValue = serde_json::from_str(&reply_json).unwrap_or(JsonValue::Null);
            if parsed.get("ok").and_then(JsonValue::as_bool) == Some(true) {
                WorkerExit::Done(ScriptDone {
                    ok: true,
                    value: Some(
                        parsed
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    name: None,
                    error: None,
                    head: None,
                })
            } else {
                // The parent classifies `{name, error}` into the upstream
                // error head, exactly like the thread sandbox's host side.
                WorkerExit::Done(ScriptDone {
                    ok: false,
                    value: None,
                    name: parsed
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned),
                    error: Some(
                        parsed
                            .get("error")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("unknown script error")
                            .to_owned(),
                    ),
                    head: None,
                })
            }
        }
        Ok(Err(js_error)) => {
            recover(&ctx, &runtime).await;
            WorkerExit::Done(ScriptDone::failed(format!(
                "script execution failed: {js_error}"
            )))
        }
        Err(_elapsed) => WorkerExit::Done(ScriptDone::failed(format!(
            "Script timed out: Execution timed out after {} ms",
            started.elapsed().as_millis()
        ))),
    }
}

// ---------------------------------------------------------------------------
// The parent side: spawn the worker child, drive the protocol, fall back
// ---------------------------------------------------------------------------

/// Memoized `current_exe` support verdict: 1 = the hidden subcommand
/// answered the handshake, 2 = unavailable (fall straight to the thread
/// sandbox from now on); anything else = not probed yet.
///
/// All accesses use `Ordering::Relaxed` on purpose: this is a single
/// per-process memo of one probe verdict with no other state whose ordering
/// it could affect — the worst a stale read does is one redundant spawn
/// probe (or one redundant fallback), never a correctness issue.
static WORKER_EXE_SUPPORT: AtomicU8 = AtomicU8::new(0);
const EXE_OK: u8 = 1;
const EXE_UNAVAILABLE: u8 = 2;

/// How long the parent waits for the `ready` handshake frame. A worker
/// answers immediately after parsing one line; anything slower is not a
/// codemode worker (a foreign `current_exe`, e.g. a test harness).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// After delivering an abort frame, how long the worker gets to report its
/// own `done` (the QuickJS interrupt handler stops the script) before the
/// parent kills it. Far shorter than the sandbox deadline.
const ABORT_GRACE: Duration = Duration::from_secs(5);

/// Bounded fan-out: at most this many parent-side tool tasks may be in
/// flight at once. The bridge waits for each reply before the next call, so
/// a worker queuing more `Call` frames than this is misbehaving; the run is
/// torn down as a protocol error instead of spawning unbounded work.
const MAX_IN_FLIGHT_TOOL_TASKS: usize = 32;

/// How long the parent waits for its own in-flight tool tasks to settle
/// after the protocol loop ends. They mostly await subprocesses, so a
/// cancelled task exits at its next await point; this bound is the last
/// resort before giving up (and warning) rather than hanging the outcome.
const TOOL_TASK_DRAIN: Duration = Duration::from_secs(2);

/// `PIR_CODEMODE_WORKER` kill switch: `0` / `off` / `false` forces the
/// in-process thread sandbox.
fn worker_mode_enabled() -> bool {
    !matches!(
        std::env::var("PIR_CODEMODE_WORKER").ok().as_deref(),
        Some("0") | Some("off") | Some("false")
    )
}

fn mark_worker_unavailable(reason: &str) {
    WORKER_EXE_SUPPORT.store(EXE_UNAVAILABLE, Ordering::Relaxed);
    tracing::warn!(
        target: "codemode",
        "codemode worker process unavailable ({reason}); using the in-process thread sandbox"
    );
}

async fn write_request(
    stdin: &mut tokio::process::ChildStdin,
    frame: &WorkerRequest,
) -> std::io::Result<()> {
    let line = match checked_request_line(frame)? {
        Ok(line) => line,
        Err(bytes) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                frame_over_limit_message(bytes),
            ))
        }
    };
    write_request_line(stdin, &line).await
}

/// Write one already-serialized parent→worker frame line (flush included).
async fn write_request_line(
    stdin: &mut tokio::process::ChildStdin,
    line: &str,
) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// The serialized `result` reply for one bridge call, size-checked against
/// the frame definition: an over-cap reply (a tool result bigger than one
/// frame can carry) is replaced by a small explicit host-side error result,
/// so the script's call fails cleanly — the alternative would be the child's
/// over-cap verdict, which classified the whole run as a sandbox crash. The
/// replacement is small by construction (the cap always reserves the
/// envelope margin).
fn checked_result_reply(
    id: u64,
    ok: bool,
    value: String,
    json: bool,
    error: Option<String>,
) -> std::io::Result<String> {
    let reply = WorkerRequest::Result {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
        id,
        ok,
        value,
        json,
        error,
    };
    match checked_request_line(&reply)? {
        Ok(line) => Ok(line),
        Err(bytes) => {
            let fallback = WorkerRequest::Result {
                v: CODEMODE_WORKER_PROTOCOL_VERSION,
                id,
                ok: false,
                value: String::new(),
                json: false,
                error: Some(frame_payload_limit_message("the tool result", bytes)),
            };
            match checked_request_line(&fallback)? {
                Ok(line) => Ok(line),
                Err(bytes) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    frame_over_limit_message(bytes),
                )),
            }
        }
    }
}

/// Read ONE worker→parent frame line, capped by the effective frame-size
/// definition (`super::frame_line_cap()`; [`MAX_FRAME_LINE_BYTES`] is its
/// default/no-override value): a completed line whose length (newline
/// included) exceeds the cap is a protocol-read failure (the caller tears
/// the run down as [`WorkerRunExit::Protocol`]).
///
/// Cancel-safe, unlike a raw `AsyncBufReadExt::read_line` (tokio documents
/// it as not cancel safe): the frame reader is the caller's PERSISTENT
/// [`tokio::io::Lines`] adapter, so a dropped read — the parent's `select!`
/// loop re-arms the future every iteration while the abort/deadline branches
/// can win — only parks the read. Bytes already received stay buffered in
/// the adapter, and the next call resumes the same half-line instead of
/// losing it (a lost half-line used to surface as a bogus "invalid event
/// frame" protocol failure after an abort poll or deadline fired mid-frame).
///
/// The cap verdict lands when the line completes: bytes are buffered until
/// then, and a line over the cap is an error, never a silent truncation
/// (the child's stdin reads keep the bounded `take` form). A well-behaved
/// child never produces an over-cap line: every writer on both sides refuses
/// an over-cap frame before writing it (see `write_event` /
/// `checked_result_reply` / `frame_line_cap`), so this read verdict is the
/// last-resort defense against a foreign/tampered peer.
pub async fn read_event_line<R>(lines: &mut tokio::io::Lines<R>) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let line = match lines.next_line().await? {
            None => return Ok(None),
            // `next_line` strips the newline; the cap counts it, matching the
            // child's `take(cap + 1)` + `read_line` verdict (content + '\n' over
            // the cap is a failure, at the cap is fine).
            Some(line) if line.len() + 1 > super::frame_line_cap() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    frame_size_limit_message(),
                ))
            }
            Some(line) => line,
        };
        // Node 24/26 `--watch` leaks dependency reports onto the worker
        // channel (nodejs/node#65044); upstream `host.ts` drops them in its
        // message listener so they cannot fail the run as a broken bridge
        // (upstream #10725, `4130bd5b3`). The frame reader is the one choke
        // point for both the handshake and the event loop.
        if is_node_watch_notification(&line) {
            continue;
        }
        return Ok(Some(line));
    }
}

/// Whether a channel line is a Node `--watch` dependency report — an object
/// carrying `watch:import` or `watch:require` (Node 24/26 posts them on the
/// worker channel, nodejs/node#65044). Upstream's message listener ignores
/// exactly these (upstream `host.ts`, #10725). Public for tests/.
pub fn is_node_watch_notification(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<JsonValue>(line.trim()) else {
        return false;
    };
    let Some(fields) = value.as_object() else {
        return false;
    };
    fields.contains_key("watch:import") || fields.contains_key("watch:require")
}

/// Settle the in-flight tool-call tasks collected in `tasks` once the parent
/// protocol loop is over (called after the worker child has been killed).
///
/// With `graceful` (the worker reported its own `done`), the tasks are given
/// a bounded chance to finish on their own — every `Call` frame should
/// already have been answered before `done`, so the set is normally empty
/// and this is purely defensive. Without it (timeout / abort-grace expiry /
/// crash / protocol failure), every task is cancelled up front: the executor
/// awaits subprocesses, so cancellation ends each task at its next await
/// point. Tasks that do not stop within [`TOOL_TASK_DRAIN`] are aborted and
/// the wait is given up with a warning rather than blocking the outcome.
///
/// Public for the cancellation tests (`tests/codemode_worker_cancel.rs`).
pub async fn settle_tool_tasks(tasks: &ToolTaskSet, graceful: bool) {
    let mut handles = tasks
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<tokio::task::JoinHandle<()>>>();
    if handles.is_empty() {
        return;
    }
    let total = handles.len();
    let drain_deadline = tokio::time::Instant::now() + TOOL_TASK_DRAIN;
    if !graceful {
        for handle in &handles {
            handle.abort();
        }
    }
    while let Some(mut handle) = handles.pop() {
        let budget = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        // Joining through `&mut` keeps the handle owned, so a straggler that
        // outlives the drain budget can still be cancelled below. A task
        // that was already cancelled reports a `JoinError`, which settles
        // the join just like a clean finish.
        if tokio::time::timeout(budget, &mut handle).await.is_ok() {
            continue;
        }
        handle.abort();
        for handle in &handles {
            handle.abort();
        }
        tracing::warn!(
            target: "codemode",
            "timed out waiting for {total} in-flight codemode tool task(s) to settle; aborted the stragglers"
        );
        return;
    }
}

/// How the parent's protocol loop ended.
enum WorkerRunExit {
    /// The worker's own `done` frame.
    Done {
        ok: bool,
        value: Option<String>,
        name: Option<String>,
        error: Option<String>,
        head: Option<String>,
    },
    /// The worker's `crash` frame (VM failure outside the script's control).
    Crash(String),
    /// The outer deadline fired; the child was killed.
    TimedOut(Duration),
    /// The worker did not stop within the abort grace period; killed.
    AbortGraceExpired,
    /// Protocol/IO failure after the handshake (no fallback: the script may
    /// already have executed tool calls).
    Protocol(String),
}

// ---------------------------------------------------------------------------
// Live-worker admission (upstream `MAX_CONCURRENT_CODEMODE_WORKERS`)
// ---------------------------------------------------------------------------

/// Upstream `MAX_CONCURRENT_CODEMODE_WORKERS` (execute.ts:58): how many live
/// codemode workers may run in the whole process at once.
pub const MAX_CONCURRENT_LIVE_WORKERS: usize = 2;

/// The process-global admission slots for live codemode worker executions.
///
/// One semaphore for the whole process (not one per session/host): the cap
/// bounds real worker processes, which are process-global resources. A
/// `LazyLock` because the value must exist before any runtime does (the
/// worker CLI subcommand and test binaries have no host to thread it
/// through).
fn live_worker_admission() -> &'static Arc<tokio::sync::Semaphore> {
    static ADMISSION: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| {
            Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LIVE_WORKERS))
        });
    &ADMISSION
}

/// The admission semaphore as an owned handle (`available_permits()` reports
/// the slots not currently held). Public for the admission tests.
pub fn live_worker_admission_handle() -> Arc<tokio::sync::Semaphore> {
    Arc::clone(live_worker_admission())
}

/// Acquire one live-worker slot, waiting while both slots are held (upstream
/// `acquireWorker` throws at the cap; queueing is pir's deliberate port — a
/// third script waits for a slot instead of failing, and the caller's abort
/// signal cancels the wait).
///
/// The returned OWNED permit IS the slot: dropping it releases the slot on
/// every exit path — normal return, early `?`/`return`, an unwind from a
/// panic, and cancellation of the future holding it. There is no hand-written
/// release path that an exit could miss.
///
/// `None` means `signal` was already aborted, or fired while queued — or the
/// semaphore was closed (which nothing in pir does: the slot source is a
/// process-global `LazyLock` that is never `close()`d, so that arm is
/// unreachable in practice). In every `None` case NO worker was spawned and no
/// script ran, so the caller treats it as "aborted before execution" and must
/// not fall back to another engine.
///
/// The closed-semaphore arm is expressed as `Option` rather than a panicking
/// unwrap: production code may not panic outside the `source_hygiene`
/// allowlist, and a total function here costs one branch that the compiler
/// folds into the already-present `Option` return.
pub async fn acquire_live_worker_slot(
    signal: Option<&AbortSignal>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return None;
    }
    // One Arc clone per acquire; the permit is `'static`/owned so it can be
    // held across the whole worker lifetime without borrowing the static.
    let acquire = live_worker_admission_handle().acquire_owned();
    let permit = match signal {
        Some(signal) => tokio::select! {
            permit = acquire => permit,
            _ = signal.cancelled() => return None,
        },
        None => acquire.await,
    };
    permit.ok()
}

/// Run `code` in a dedicated worker **process** (upstream host.ts +
/// worker.ts): spawn `<current_exe> __codemode-worker`, hand it the script
/// and budgets, stream output/call frames back, execute tool calls through
/// the nested executor, and enforce abort/timeout with frame + kill.
///
/// Returns `None` when the worker process is unavailable (fall back to the
/// in-process thread sandbox); see the module doc for the exact fallback
/// window.
///
/// A live worker process holds one [`MAX_CONCURRENT_LIVE_WORKERS`] admission
/// slot (upstream `MAX_CONCURRENT_CODEMODE_WORKERS`). The slot is taken
/// after the fallback gates above, so a run that cannot use the worker
/// process keeps the thread fallback's pre-admission semantics (no slot, no
/// queueing); it is held until the worker is reaped and its tool tasks
/// settle.
pub(super) async fn run_in_worker_process(
    code: &str,
    allowed_tools: Option<&[String]>,
    executor: NestedToolExecutor,
    signal: Option<AbortSignal>,
    limits: SandboxLimits,
) -> Option<ScriptOutcome> {
    if !worker_mode_enabled() {
        return None;
    }
    match WORKER_EXE_SUPPORT.load(Ordering::Relaxed) {
        EXE_OK => {}
        EXE_UNAVAILABLE => return None,
        _ => {}
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            mark_worker_unavailable(&format!("current_exe failed: {error}"));
            return None;
        }
    };

    // Live-worker admission (see the module doc): the owned permit IS the
    // slot guard for this worker's whole lifetime — spawn, handshake,
    // protocol loop, kill/reap and the tool-task drain. Every exit path
    // (success, error, timeout, abort), a panic during the run, and
    // cancellation of this future drop the guard and free the slot.
    let _live_worker_slot = match acquire_live_worker_slot(signal.as_ref()).await {
        Some(slot) => slot,
        // Aborted while queued (or — unreachable in practice — the slot
        // source was closed): no worker was spawned and no script ran, so do
        // NOT fall back to the thread sandbox (a cancelled run must not
        // execute). The head matches the sandbox's aborted-run shape.
        None => {
            return Some(ScriptOutcome {
                ok: false,
                output: String::new(),
                error_head: Some("script execution failed: Execution aborted".to_owned()),
            })
        }
    };

    let mut command = tokio::process::Command::new(&exe);
    command
        .arg(WORKER_SUBCOMMAND)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Lifecycle safety: if this future is dropped before the explicit
        // teardown below runs (caller cancellation), the `Child` drop kills
        // the worker instead of orphaning it — an orphan loses the protocol
        // bridge and would keep running until its own deadline. The explicit
        // exit paths are unaffected: they `start_kill()` + `wait()` first,
        // after which the drop-time kill is a no-op. The worker runs only
        // the QuickJS VM in-process (tool calls execute parent-side; it
        // spawns no descendants), so killing the direct child is complete.
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: the worker is a headless helper; never flash a
        // console window for it.
        command.creation_flags(0x0800_0000);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            mark_worker_unavailable(&format!("spawn failed: {error}"));
            return None;
        }
    };

    let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
        (Some(stdin), Some(stdout)) => (stdin, stdout),
        _ => {
            let _ = child.start_kill();
            mark_worker_unavailable("worker stdio pipes were not captured");
            return None;
        }
    };
    let stdin_writer: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>> =
        Arc::new(tokio::sync::Mutex::new(stdin));
    let stdout = tokio::io::BufReader::new(stdout);
    // One persistent frame reader for the handshake AND the event loop (see
    // `read_event_line`): the adapter owns the partial-line buffer, so a
    // read dropped between `select!` iterations cannot lose a half frame.
    let mut event_lines = stdout.lines();

    // Init frame (the script + budgets; nothing else crosses the bridge).
    // `write_request` enforces the frame-size definition before writing: an
    // over-cap init frame (a script source bigger than one frame can carry,
    // escaped) is refused here and falls back to the thread sandbox with the
    // explicit size error logged — this happens BEFORE any script side
    // effect, so the fallback re-run is safe (same window as a spawn or
    // handshake failure).
    // The worker-process path always runs with a deadline (the builtin
    // `codemode` tool's face); the extension sandbox route, which may have
    // none, never reaches this function.
    let timeout = limits.timeout.unwrap_or(super::SANDBOX_TIMEOUT);
    let init = WorkerRequest::Init {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
        code: code.to_owned(),
        allowed_tools: allowed_tools.map(|names| names.to_vec()),
        max_output_chars: limits.max_output_chars,
        max_output_items: limits.max_output_items,
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        memory_bytes: limits.memory_bytes.map(|bytes| bytes as u64),
    };
    if let Err(error) = write_request(&mut *stdin_writer.lock().await, &init).await {
        let _ = child.start_kill();
        mark_worker_unavailable(&format!("failed to write the init frame: {error}"));
        return None;
    }

    // Handshake: the first event must be `ready` with a matching version,
    // bounded by READY_TIMEOUT. Every failure here happens BEFORE the script
    // could execute any tool call (the parent executes those), so falling
    // back to the thread sandbox cannot duplicate side effects.
    let handshake = tokio::time::timeout(READY_TIMEOUT, read_event_line(&mut event_lines)).await;
    let ready_line = match handshake {
        Err(_) => {
            let _ = child.start_kill();
            mark_worker_unavailable("worker handshake timed out");
            return None;
        }
        Ok(Err(error)) => {
            let _ = child.start_kill();
            mark_worker_unavailable(&format!("failed to read the worker handshake: {error}"));
            return None;
        }
        Ok(Ok(None)) => {
            let _ = child.start_kill();
            mark_worker_unavailable("worker exited before the handshake");
            return None;
        }
        Ok(Ok(Some(line))) => line,
    };
    match decode_event(&ready_line) {
        Ok(WorkerEvent::Ready { v }) if v == CODEMODE_WORKER_PROTOCOL_VERSION => {
            WORKER_EXE_SUPPORT.store(EXE_OK, Ordering::Relaxed);
        }
        Ok(_) => {
            let _ = child.start_kill();
            mark_worker_unavailable("unexpected first worker frame");
            return None;
        }
        Err(error) => {
            let _ = child.start_kill();
            mark_worker_unavailable(&error);
            return None;
        }
    }

    // Parent-side output accounting: identical OutputBuffer/caps semantics as
    // the thread sandbox (the prelude counts; the buffer enforces).
    let buffer = Arc::new(StdMutex::new(OutputBuffer::new(
        limits.max_output_chars,
        limits.max_output_items,
    )));
    let started = Instant::now();
    let deadline = started + timeout;
    let deadline_tokio = tokio::time::Instant::from(deadline);
    let mut abort_deadline: Option<tokio::time::Instant> = None;
    // The abort waiter is a real future (`AbortSignal` is a
    // `CancellationToken`): a cancellation wakes the protocol loop
    // immediately instead of at a 25 ms poll tick. A run without a signal
    // waits forever, which disables the branch exactly like the old tick did.
    let abort = async {
        match signal.as_ref() {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(abort);

    // In-flight tool-call task handles (see ToolTaskSet): collected so the
    // teardown after the loop can cancel/await them on EVERY exit path.
    let tool_tasks: ToolTaskSet = Arc::default();

    let exit: WorkerRunExit = loop {
        let grace_tokio = abort_deadline.unwrap_or(deadline_tokio);
        tokio::select! {
            line = read_event_line(&mut event_lines) => {
                match line {
                    Ok(Some(line)) => match decode_event(&line) {
                        Ok(WorkerEvent::Output { kind, text, .. }) => {
                            buffer
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(if kind == "console" { "console" } else { "text" }, text);
                        }
                        Ok(WorkerEvent::Call { id, name, args, .. }) => {
                            // Whitelist enforcement (worker-process mode; the
                            // mirror of the thread bridge's check — upstream
                            // host.ts never hands the worker a tool outside
                            // the filtered table, pir enforces at the parent
                            // frame handler instead): a `call` frame naming
                            // a tool outside Init.allowed_tools is answered
                            // with a tool-failure result instead of being
                            // executed. The `codemode` hard reject below
                            // stays unconditional.
                            if super::allowlist_rejects(allowed_tools, &name) {
                                let reply = WorkerRequest::Result {
                                    v: CODEMODE_WORKER_PROTOCOL_VERSION,
                                    id,
                                    ok: false,
                                    value: String::new(),
                                    json: false,
                                    error: Some(super::allowlist_error_text(&name)),
                                };
                                let mut stdin = stdin_writer.lock().await;
                                if let Err(write_error) = write_request(&mut stdin, &reply).await {
                                    tracing::warn!(
                                        target: "codemode",
                                        "failed to deliver the allowlist rejection to the codemode worker: {write_error}"
                                    );
                                }
                                continue;
                            }
                            // The tool executes in the parent (upstream
                            // host.ts handles the worker's `call`); the
                            // nested executor runs on THIS runtime, so the
                            // call is spawned as a plain task and its reply
                            // frame written when it settles.
                            //
                            // Bounded fan-out: prune finished handles, then
                            // refuse further calls once the in-flight cap
                            // is reached — the bridge waits for each reply,
                            // so a worker this deep in queued calls is
                            // misbehaving. Torn down as a protocol error.
                            {
                                let mut pending = tool_tasks
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                pending.retain(|handle| !handle.is_finished());
                                if pending.len() >= MAX_IN_FLIGHT_TOOL_TASKS {
                                    drop(pending);
                                    let _ = child.start_kill();
                                    break WorkerRunExit::Protocol(format!(
                                        "codemode worker exceeded the in-flight tool-call limit ({MAX_IN_FLIGHT_TOOL_TASKS})"
                                    ));
                                }
                            }
                            let executor = executor.clone();
                            let signal = signal.clone();
                            let stdin_writer = Arc::clone(&stdin_writer);
                            let handle = tokio::spawn(async move {
                                let (ok, value, json, error) =
                                    if name == "codemode" {
                                        (false, String::new(), false,
                                         Some("scripts must not start other scripts".to_owned()))
                                    } else if signal
                                        .as_ref()
                                        .is_some_and(|signal| signal.is_aborted())
                                    {
                                        (false, String::new(), false, Some("aborted".to_owned()))
                                    } else {
                                        let args: JsonValue =
                                            serde_json::from_str(&args).unwrap_or(JsonValue::Null);
                                        let outcome = executor.execute(&name, args).await;
                                        if outcome.is_error {
                                            (false, String::new(), false, Some(outcome.result.content))
                                        } else {
                                            let structured =
                                                outcome.result.structured_content.is_some();
                                            let value = if structured {
                                                serde_json::to_string(
                                                    &outcome.result.structured_content.unwrap_or_default(),
                                                )
                                                .unwrap_or_default()
                                            } else {
                                                outcome.result.content
                                            };
                                            (true, value, structured, None)
                                        }
                                    };
                                let reply_line =
                                    checked_result_reply(id, ok, value, json, error);
                                let mut stdin = stdin_writer.lock().await;
                                let write_result = match reply_line {
                                    Ok(line) => write_request_line(&mut stdin, &line).await,
                                    Err(error) => Err(error),
                                };
                                if let Err(write_error) = write_result {
                                    tracing::warn!(
                                        target: "codemode",
                                        "failed to deliver a tool result to the codemode worker: {write_error}"
                                    );
                                }
                            });
                            tool_tasks
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(handle);
                        }
                        Ok(WorkerEvent::Done { ok, value, name, error, head, .. }) => {
                            break WorkerRunExit::Done { ok, value, name, error, head };
                        }
                        Ok(WorkerEvent::Crash { message, .. }) => {
                            break WorkerRunExit::Crash(message);
                        }
                        Ok(WorkerEvent::Ready { .. }) => {}
                        Err(decode_error) => {
                            let _ = child.start_kill();
                            break WorkerRunExit::Protocol(decode_error);
                        }
                    },
                    Ok(None) => {
                        break WorkerRunExit::Protocol(
                            "worker exited before reporting a result".to_owned(),
                        );
                    }
                    Err(error) => {
                        break WorkerRunExit::Protocol(format!(
                            "failed to read the worker protocol stream: {error}"
                        ));
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline_tokio) => {
                // Hard deadline: kill the worker (the strong-kill bound that
                // matches the thread sandbox's outer tokio deadline).
                let _ = child.start_kill();
                break WorkerRunExit::TimedOut(started.elapsed());
            }
            _ = &mut abort, if abort_deadline.is_none() => {
                // Cancellation: deliver the abort frame; the child's
                // interrupt handler stops the script and reports its own
                // `done` (thread-sandbox error semantics), bounded by the
                // grace period below.
                abort_deadline = Some(tokio::time::Instant::now() + ABORT_GRACE);
                let abort = WorkerRequest::Abort {
                    v: CODEMODE_WORKER_PROTOCOL_VERSION,
                };
                let mut stdin = stdin_writer.lock().await;
                if let Err(write_error) = write_request(&mut stdin, &abort).await {
                    tracing::warn!(
                        target: "codemode",
                        "failed to deliver the abort frame to the codemode worker: {write_error}"
                    );
                }
            }
            _ = tokio::time::sleep_until(grace_tokio), if abort_deadline.is_some() => {
                let _ = child.start_kill();
                break WorkerRunExit::AbortGraceExpired;
            }
        }
    };

    // The run is over: make sure the worker process is gone (killing an
    // already-exited child is a no-op) and reap it.
    let _ = child.start_kill();
    let _ = child.wait().await;

    // Cancellation sweep (see ToolTaskSet): the script's hard deadline
    // includes its tool calls, so NO exit path may leave a spawned tool task
    // running in the background. With the worker dead, settle what is still
    // in flight — cancelling first unless the worker reported its own `done`
    // (where every call should already be answered; defensive wait only).
    let graceful = matches!(exit, WorkerRunExit::Done { .. });
    settle_tool_tasks(&tool_tasks, graceful).await;

    let (ok, error_head, return_value) = match exit {
        WorkerRunExit::Done {
            ok: true, value, ..
        } => (true, None, value),
        WorkerRunExit::Done {
            head: Some(head), ..
        } => (false, Some(head), None),
        WorkerRunExit::Done { name, error, .. } => {
            // Upstream error head (formatError): `${name}: ${message}`, with
            // the name defaulting to "Error" for thrown non-Error values.
            let message = error.unwrap_or_else(|| "unknown script error".to_owned());
            let head = match name {
                Some(name) if !name.is_empty() => format!("{name}: {message}"),
                _ => format!("Error: {message}"),
            };
            (false, Some(head), None)
        }
        WorkerRunExit::Crash(message) => (
            false,
            Some(format!("Script sandbox failed: {message}")),
            None,
        ),
        WorkerRunExit::TimedOut(elapsed) => (
            false,
            Some(format!(
                "Script timed out: Execution timed out after {} ms",
                elapsed.as_millis()
            )),
            None,
        ),
        WorkerRunExit::AbortGraceExpired => (
            false,
            Some(
                "Script sandbox failed: sandbox worker did not stop after the abort signal"
                    .to_owned(),
            ),
            None,
        ),
        WorkerRunExit::Protocol(message) => (
            false,
            Some(format!("Script sandbox failed: {message}")),
            None,
        ),
    };
    let (items, over_limit) = buffer.lock().unwrap_or_else(PoisonError::into_inner).take();
    // Host-side output-cap enforcement (defense in depth against a tampered
    // prelude wrapper): past the caps the script fails like upstream's
    // `done()` error path. The buffer refused items past the cap, so the
    // buffered output stays bounded; the items accepted before the cap are
    // kept (a failed script keeps its partial output).
    let (ok, error_head) = if over_limit {
        (
            false,
            Some(format!(
                "RangeError: {}",
                super::output_limit_error(limits.max_output_chars, limits.max_output_items)
            )),
        )
    } else {
        (ok, error_head)
    };
    Some(assemble_script_outcome(
        ok,
        error_head,
        return_value,
        items,
        limits,
    ))
}
