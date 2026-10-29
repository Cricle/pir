//! `child_process` + `zlib` host backends behind the second generic native
//! entry `__pir_host.nodeExt(module, op, payloadJson) -> String`.
//!
//! The primary `__pir_host.node` dispatcher belongs to the phase-2
//! fs/os/crypto table ([`super::node_fns`]); the JS `__pirNodeCall` bridge
//! routes the `child` and `zlib` module names here instead. The JSON
//! envelope (`{ok,value}` / `{ok:false,code,message,...}`) and the failure
//! shape are identical, so both entries raise the same Node-shaped `Error`.
//!
//! Command execution is the same capability class as `pi.exec`
//! (`runtime/exec.rs`): spawning a process needs no new permission gate. The
//! filesystem *parameters* are still sandboxed — a `cwd` outside the phase-2
//! roots (allow-list + agent dir + runtime cwd) is rejected with `EACCES`.
//!
//! The one program-name rewrite performed here is the node executable shim
//! (see the "node executable shim" section): a bare `node`-family program is
//! routed to the pir binary's `js` subcommand, so extensions that spawn
//! `node` work in an installation with no system node.
//!
//! Queue bounds: each child stream is captured by a reader thread into a
//! bounded sink ([`MAX_STREAM_BYTES`]). The async `spawn` reader applies
//! BACKPRESSURE at the cap — it parks, the OS pipe fills, the child blocks on
//! `write`, and not one byte is dropped (Node's paused-until-read stream
//! contract; the cap is the poll protocol's `highWaterMark`). The blocking
//! `spawnSync` cannot backpressure (its caller is inside `wait()`, so a parked
//! reader would deadlock the call against a child that never stops writing):
//! it keeps the newest bytes and reports `truncated`, which is Node's own
//! `maxBuffer` degradation for that form. Both conditions are counted and
//! reported through `ext_diag!` on the [`STREAM_REPORT_EVERY`] cadence.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Component, Path};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rquickjs::Object;

use super::*;

// ---------------------------------------------------------------------------
// Registration + dispatch
// ---------------------------------------------------------------------------

pub(super) fn register_child_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    // The owning runtime's generation: every spawned child is tagged so a
    // `/reload`/shutdown can kill exactly its own process table.
    generation: u64,
) -> rquickjs::Result<()> {
    let state = state.clone();
    let allowed_roots = allowed_roots.clone();
    host.set(
        "nodeExt",
        Func::from(
            move |module: String, op: String, payload: String| -> String {
                node_ext_call(&state, &allowed_roots, &module, &op, &payload, generation)
            },
        ),
    )?;
    Ok(())
}

fn node_ext_call(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    module: &str,
    op: &str,
    payload: &str,
    generation: u64,
) -> String {
    let started = std::time::Instant::now();
    let payload: JsonValue = serde_json::from_str(payload).unwrap_or(JsonValue::Null);
    let result = match module {
        "child" => child_op(state, allowed_roots, op, &payload, generation),
        "zlib" => zlib_op(op, &payload),
        other => Err(FnError::new(
            "ENOSYS",
            format!("unknown extended node host module `{other}`"),
        )),
    };
    let output = match result {
        Ok(value) => serde_json::json!({ "ok": true, "value": value }).to_string(),
        Err(error) => error.into_json().to_string(),
    };
    let label = match module {
        "child" => "child_process",
        "zlib" => "zlib",
        _ => "nodeExt.other",
    };
    super::super::host_op_stats::record(label, started.elapsed());
    output
}

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

/// One failure in the shared node-surface envelope. The optional
/// `syscall`/`path` fields carry the conventional spawn coordinates
/// (`syscall: "spawn <cmd>"`, `path: <cmd>`) the JS bridge copies onto the
/// raised `Error`.
struct FnError {
    code: &'static str,
    message: String,
    syscall: Option<String>,
    path: Option<String>,
}

impl FnError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            syscall: None,
            path: None,
        }
    }

    /// Map a `Command::spawn` failure: Node reports an unresolvable program as
    /// `ENOENT` with `syscall: "spawn <cmd>"` and `path: <cmd>`.
    fn spawn_failure(error: &std::io::Error, cmd: &str) -> Self {
        let code = if error.kind() == std::io::ErrorKind::NotFound {
            "ENOENT"
        } else {
            "EACCES"
        };
        Self {
            code,
            message: format!("{code}: spawn {cmd} {error}"),
            syscall: Some(format!("spawn {cmd}")),
            path: Some(cmd.to_owned()),
        }
    }

    /// The `{ok:false, code, message, …}` envelope. Consumes the error: the
    /// message and the spawn coordinates move into the JSON value instead of
    /// being cloned into it.
    fn into_json(self) -> JsonValue {
        let mut out = serde_json::Map::new();
        out.insert("ok".to_owned(), JsonValue::Bool(false));
        out.insert("code".to_owned(), JsonValue::String(self.code.to_owned()));
        out.insert("message".to_owned(), JsonValue::String(self.message));
        if let Some(syscall) = self.syscall {
            out.insert("syscall".to_owned(), JsonValue::String(syscall));
        }
        if let Some(path) = self.path {
            out.insert("path".to_owned(), JsonValue::String(path));
        }
        JsonValue::Object(out)
    }
}

type OpResult = Result<JsonValue, FnError>;

/// Borrow one string field. Every caller only reads it (the value is copied
/// into the `Command` at spawn time, or base64-decoded in place), so no owned
/// copy is made here.
fn payload_str<'a>(payload: &'a JsonValue, key: &str) -> Result<&'a str, FnError> {
    payload
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FnError::new("EINVAL", format!("missing string payload field `{key}`")))
}

fn payload_bool(payload: &JsonValue, key: &str) -> bool {
    payload
        .get(key)
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

fn payload_strings(payload: &JsonValue, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(JsonValue::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| match value {
                    JsonValue::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Sandbox check for the child's working directory
// ---------------------------------------------------------------------------

/// Effective sandbox roots: the loader allow-list plus the agent directory and
/// the runtime cwd — the phase-2 rule, re-applied here because a child runs
/// *inside* whatever directory it is spawned in.
fn effective_roots(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
) -> Vec<PathBuf> {
    // The keys are built directly off the guard: cloning the allow-list into a
    // `Vec<PathBuf>` only to map it through `sandbox_key` (an extra heap
    // allocation plus copy per root) buys nothing here.
    let mut keys: Vec<PathBuf> = allowed_roots
        .lock()
        .map(|roots| roots.iter().map(|root| sandbox_key(root)).collect())
        .unwrap_or_default();
    keys.push(sandbox_key(&crate::ai::providers::agent_dir()));
    keys.push(sandbox_key(&lock_state(state).config.cwd));
    keys
}

/// Canonical form used for sandbox comparisons: existing paths resolve through
/// the filesystem (links included), a missing tail keeps its deepest existing
/// parent plus the remainder, and the Windows `\\?\` verbatim prefix is
/// stripped so every form compares alike.
fn sandbox_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_verbatim(canonical);
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if parent != path => {
            let mut joined = sandbox_key(parent);
            joined.push(name);
            joined
        }
        _ => strip_verbatim(path.to_path_buf()),
    }
}

/// `\\?\C:\...` -> `C:\...` (and `\\?\UNC\a\b` -> `\\a\b`).
fn strip_verbatim(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// Component-wise prefix test, case-insensitive on Windows (NTFS is).
fn within(candidate: &Path, root: &Path) -> bool {
    let parts = |path: &Path| -> Vec<String> {
        path.components()
            .map(|component| match component {
                Component::CurDir | Component::ParentDir => String::new(),
                other => {
                    let text = other.as_os_str().to_string_lossy().into_owned();
                    if cfg!(windows) {
                        text.to_lowercase()
                    } else {
                        text
                    }
                }
            })
            .collect()
    };
    let root_parts = parts(root);
    if root_parts.is_empty() {
        return false;
    }
    let candidate_parts = parts(candidate);
    candidate_parts.len() >= root_parts.len()
        && root_parts.iter().zip(&candidate_parts).all(|(a, b)| a == b)
}

fn cwd_allowed(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    cwd: &Path,
) -> bool {
    let key = sandbox_key(cwd);
    effective_roots(state, allowed_roots)
        .iter()
        .any(|root| within(&key, root))
}

// ---------------------------------------------------------------------------
// child: process registry
// ---------------------------------------------------------------------------

/// Maximum bytes buffered per stream of a spawned child before the reader
/// stops draining the OS pipe.
///
/// Node's `child.stdout` is a paused-until-read stream: a child that writes
/// faster than its parent reads blocks on the pipe once the kernel buffer
/// (64 KiB on Linux, ~64 KiB on Windows) fills, and NOTHING is lost — that is
/// the backpressure contract this cap reproduces. The cap is the poll-side
/// analogue of Node's `highWaterMark` (64 KiB): the JS pump polls every ≤200 ms
/// (`POLL_WAIT_MS`) and a poll delivers whatever is buffered, so 8 MiB gives a
/// fast producer a full poll window of headroom (Node's OS pipe only holds
/// 64 KiB) without retaining more than a bounded window per stream per child.
/// Overflow policy per mode (see [`SinkMode`]): async `spawn` PARKS the reader
/// (the pipe fills, the child blocks — no byte is ever dropped); blocking
/// `spawnSync` truncates from the oldest end and raises the `truncated`
/// marker, which is Node's own `maxBuffer` degradation for the form where no
/// consumer can run while the call waits.
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// One in [`MAX_STREAM_BYTES`] worth of reader chunks: the cadence of the
/// overflow diagnostic (the first occurrence is always reported; identical
/// repeats are additionally collapsed by the logging router).
const STREAM_REPORT_EVERY: u64 = 4096;

/// Upper bound of one `spawnPoll` wait; the JS poll loop asks for less (one
/// tick's worth) so the runtime thread is never blocked for long.
const MAX_POLL_WAIT_MS: u64 = 200;

/// Longest single condvar park slice of a blocking host wait (`spawnPoll`, the
/// `spawnSync` drain window): the wait is signalled by the reader/waiter, so
/// this is only the lost-wakeup safety net, not the poll cadence.
const CHILD_WAIT_SLICE: Duration = Duration::from_millis(50);

/// Extra bounded drain after the child exited: the pipe readers normally hit
/// EOF immediately, and this grace lets the final poll carry the complete tail
/// before the registry entry is removed.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(100);

/// Post-exit quiet window used by the blocking `spawnSync` (mirrors
/// `runtime/exec.rs`: a detached descendant that inherited the pipes must not
/// hang the call forever).
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(300);

/// First sleep of the exit watcher before it re-probes with `Child::try_wait`.
const EXIT_WATCH_BASE_DELAY: Duration = Duration::from_millis(10);

/// Exit-watcher backoff ceiling: after the doubling settles, a live child costs
/// at most one `try_wait` wakeup per second and an exit is noticed within one
/// interval (≤1 s).
const EXIT_WATCH_MAX_DELAY: Duration = Duration::from_secs(1);

/// One wait slice of the blocking `spawnSync` post-exit drain, bounded by the
/// quiet deadline `exit_at.max(last_data) + EXIT_STDIO_GRACE`.
///
/// The deadline is anchored at the LATER of the exit and the last captured
/// chunk (the window re-arms on every chunk). Measuring the remaining time
/// from `last_data` alone (the previous shape) is wrong whenever the last
/// chunk predates the exit: a command that printed early and then ran for a
/// while yields a saturating zero slice and the drain busy-spins for the whole
/// grace window instead of parking. `0` means the deadline has passed.
fn sync_drain_wait_slice(exit_at: Instant, last_data: Instant, now: Instant) -> Duration {
    let deadline = exit_at.max(last_data) + EXIT_STDIO_GRACE;
    deadline
        .saturating_duration_since(now)
        .min(CHILD_WAIT_SLICE)
}

/// How a stream sink answers a full buffer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SinkMode {
    /// Async `spawn`: the reader parks until the consumer drains. The OS pipe
    /// fills, the child blocks on `write` — Node's paused-stream semantics,
    /// zero bytes dropped.
    Backpressure,
    /// Blocking `spawnSync`: the caller is inside `child.wait()` and cannot
    /// consume, so a parked reader would deadlock the call against a child
    /// that never stops writing. Keep the newest bytes and raise the
    /// truncation marker instead — Node's `maxBuffer` behavior for the
    /// blocking form (`spawnSync` truncates; note Node *terminates* the child
    /// at the limit while this keeps draining it, which the stdout/stderr
    /// result's `truncated` field reports).
    Truncate,
}

/// One process stream's bytes, captured by its reader thread.
struct StreamBuf {
    data: VecDeque<u8>,
    /// Bytes were dropped since the last `take` (the truncation marker).
    dropped: bool,
    /// How many chunks were dropped in total (diagnostic cadence; `dropped`
    /// alone is reset per take).
    dropped_chunks: u64,
    eof: bool,
    /// Last data arrival; re-arms the post-exit quiet window.
    last_data: Instant,
    mode: SinkMode,
    /// The handle was dropped (child killed/reaped): a parked reader must be
    /// released and stop reading.
    closed: bool,
    /// Chunks that had to wait for buffer space (observation).
    waits: u64,
}

impl StreamBuf {
    fn new(mode: SinkMode) -> Self {
        Self {
            data: VecDeque::new(),
            dropped: false,
            dropped_chunks: 0,
            eof: false,
            last_data: Instant::now(),
            mode,
            closed: false,
            waits: 0,
        }
    }

    /// Append one chunk under the standing overflow policy. Returns whether
    /// the caller must park (the buffer is full and the mode backpressures).
    fn push_now(&mut self, chunk: &[u8]) -> bool {
        self.last_data = Instant::now();
        // Backpressure: park until the WHOLE chunk fits, so the cap is never
        // crossed and no byte is dropped (the caller waits on the condvar).
        if self.mode == SinkMode::Backpressure && self.data.len() + chunk.len() > MAX_STREAM_BYTES {
            return true;
        }
        // A reader hands over at most one 8 KiB read at a time, so the
        // oversized branch is defensive: keep only the newest MAX bytes.
        if chunk.len() > MAX_STREAM_BYTES {
            self.data.clear();
            self.data.extend(&chunk[chunk.len() - MAX_STREAM_BYTES..]);
            self.dropped = true;
            self.dropped_chunks = self.dropped_chunks.saturating_add(1);
            return false;
        }
        let total = self.data.len() + chunk.len();
        if total > MAX_STREAM_BYTES {
            self.data.drain(..total - MAX_STREAM_BYTES);
            self.dropped = true;
            self.dropped_chunks = self.dropped_chunks.saturating_add(1);
        }
        self.data.extend(chunk);
        false
    }

    fn has_data(&self) -> bool {
        self.dropped || !self.data.is_empty()
    }

    /// Drain what is buffered now as base64 text; the flag reports whether
    /// bytes were lost. The encode reads straight out of the ring buffer's
    /// storage, so the intermediate byte vector a `drain(..).collect()` would
    /// allocate and copy is gone (`make_contiguous` re-orders an
    /// already-wrapped ring in place and never allocates).
    fn take_base64(&mut self) -> (String, bool) {
        let dropped = self.dropped;
        self.dropped = false;
        let encoded = STANDARD.encode(self.data.make_contiguous());
        self.data.clear();
        (encoded, dropped)
    }
}

/// One child stream as the reader thread and the host ops share it: the
/// mutex-guarded buffer plus the condvar that parks a backpressured reader
/// and is signalled by every drain/close.
struct StreamSink {
    label: String,
    buf: StdMutex<StreamBuf>,
    /// Signalled when the consumer drained (`take_base64`) or the sink closed.
    space: Condvar,
}

impl StreamSink {
    fn new(label: String, mode: SinkMode) -> Arc<Self> {
        Arc::new(Self {
            label,
            buf: StdMutex::new(StreamBuf::new(mode)),
            space: Condvar::new(),
        })
    }

    /// Push one reader chunk. In [`SinkMode::Backpressure`] this parks while
    /// the buffer is at [`MAX_STREAM_BYTES`] — the pipe fills and the child
    /// blocks, so no byte is dropped; the wait ends when the consumer drains
    /// or the handle closes. In [`SinkMode::Truncate`] it never parks (the
    /// oldest bytes go instead).
    fn push(&self, chunk: &[u8]) {
        let mut buf = lock(&self.buf);
        let mut parked = false;
        loop {
            if buf.closed {
                return;
            }
            if !buf.push_now(chunk) {
                return;
            }
            if !parked {
                // Report the park on a bounded cadence — the first waiting
                // chunk and then one line per [`STREAM_REPORT_EVERY`] — so a
                // child blocked on a consumer that stopped polling is an
                // operator-visible condition, not a silent hang.
                parked = true;
                buf.waits = buf.waits.saturating_add(1);
                let waits = buf.waits;
                if waits == 1 || waits.is_multiple_of(STREAM_REPORT_EVERY) {
                    crate::ext_diag!(
                        "[ext] child {} stream full ({} bytes buffered); the reader is paused (backpressure) after {} waits",
                        self.label,
                        MAX_STREAM_BYTES,
                        waits
                    );
                }
            }
            let (next, _timeout) = self
                .space
                .wait_timeout(buf, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner);
            buf = next;
        }
    }

    fn has_data(&self) -> bool {
        lock(&self.buf).has_data()
    }

    fn eof(&self) -> bool {
        lock(&self.buf).eof
    }

    fn last_data(&self) -> Instant {
        lock(&self.buf).last_data
    }

    /// Drain the buffer and release a parked reader. The truncation diagnostic
    /// fires here (the mode that drops bytes also reports them).
    fn take_base64(&self) -> (String, bool) {
        let (encoded, dropped, dropped_chunks) = {
            let mut buf = lock(&self.buf);
            let (encoded, dropped) = buf.take_base64();
            (encoded, dropped, buf.dropped_chunks)
        };
        self.space.notify_all();
        if dropped {
            self.report_drops(dropped_chunks);
        }
        (encoded, dropped)
    }

    fn report_drops(&self, dropped_chunks: u64) {
        if dropped_chunks == 1 || dropped_chunks.is_multiple_of(STREAM_REPORT_EVERY) {
            crate::ext_diag!(
                "[ext] child {} output exceeded the {} byte stream cap; the oldest bytes were truncated ({} chunks so far)",
                self.label,
                MAX_STREAM_BYTES,
                dropped_chunks
            );
        }
    }

    fn set_eof(&self) {
        lock(&self.buf).eof = true;
    }

    /// Release a parked reader and stop it: the handle is going away, so the
    /// remaining bytes have no consumer (the child is being killed/reaped in
    /// the same drop).
    fn close(&self) {
        lock(&self.buf).closed = true;
        self.space.notify_all();
    }
}

/// How one child ended.
#[derive(Clone, Copy)]
struct ExitInfo {
    code: Option<i32>,
    signal: Option<i32>,
}

/// One child's activity signal: a monotone counter plus a condvar. Readers
/// bump it after every captured chunk and the exit watcher / `spawnPoll` bumps
/// it after recording an exit; `spawnPoll` and `spawnSync`'s post-exit quiet
/// window park on it instead of sleeping 5 ms steps (each park is bounded by
/// the caller's deadline, and a spurious/lost notification only costs one
/// slice).
struct ActivitySignal {
    state: StdMutex<(u64, u64)>,
    signal: Condvar,
}

impl ActivitySignal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new((0, 0)),
            signal: Condvar::new(),
        })
    }

    fn notify(&self) {
        let mut state = lock(&self.state);
        state.0 = state.0.wrapping_add(1);
        drop(state);
        self.signal.notify_all();
    }

    /// Current activity generation. A caller checks the data under its own
    /// lock, snapshots this, then calls [`Self::wait_since`]: a notification
    /// that lands between the check and the park is observed instead of lost.
    fn epoch(&self) -> u64 {
        lock(&self.state).0
    }

    /// Park until a notification newer than `seen`, or `timeout`. Returns
    /// whether a notification arrived (a generation already newer than `seen`
    /// returns immediately). The watchdog counter (second word) increments per
    /// park so a test can prove the wait is not a poll loop.
    fn wait_since(&self, seen: u64, timeout: Duration) -> bool {
        let mut state = lock(&self.state);
        state.1 = state.1.wrapping_add(1);
        let (state, outcome) = self
            .signal
            .wait_timeout_while(state, timeout, |state| state.0 == seen)
            .unwrap_or_else(PoisonError::into_inner);
        drop(state);
        !outcome.timed_out()
    }

    /// Park until the next notification or `timeout`.
    fn wait(&self, timeout: Duration) -> bool {
        let seen = self.epoch();
        self.wait_since(seen, timeout)
    }

    fn parks(&self) -> u64 {
        lock(&self.state).1
    }
}

/// One live spawned child. Removing the entry (or dropping the runtime) drops
/// this handle, and [`ChildHandle::drop`] kills + reaps the process so a
/// removed handle never leaves a stray process behind. The drop also closes
/// both sinks, which releases a reader parked on a full buffer (backpressure)
/// so no reader thread outlives its child.
struct ChildHandle {
    child: Arc<StdMutex<Child>>,
    stdin: StdMutex<Option<ChildStdin>>,
    stdout: Arc<StreamSink>,
    stderr: Arc<StreamSink>,
    status: Arc<StdMutex<Option<ExitInfo>>>,
    /// Wakes `spawnPoll`'s bounded wait on output/exit.
    activity: Arc<ActivitySignal>,
    /// Owning runtime generation (see `register_child_host_fns`).
    generation: u64,
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // Read the status before taking the child lock: `record_child_exit`
        // locks status then child, so the reverse order here would deadlock.
        let exit_known = lock(&self.status).is_some();
        let mut child = lock(&self.child);
        if !exit_known {
            let _ = child.kill();
        }
        // Reap: without the wait the child stays a zombie on Unix. An already
        // recorded exit is normally reaped by the successful `try_wait`.
        let _ = child.wait();
        drop(lock(&self.stdin).take());
        // Release (and stop) readers parked on a full buffer: the buffer's
        // consumer is gone with this handle.
        self.stdout.close();
        self.stderr.close();
    }
}

/// Process-wide cap on live extension children: each one owns three threads
/// (stdout/stderr readers + exit watcher) and up to two 8 MiB stream windows.
const MAX_CHILDREN: usize = 64;
static CHILDREN: OnceLock<StdMutex<HashMap<u64, Arc<ChildHandle>>>> = OnceLock::new();
static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(1);

fn children() -> &'static StdMutex<HashMap<u64, Arc<ChildHandle>>> {
    CHILDREN.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn child_by_id(id: u64) -> Result<Arc<ChildHandle>, FnError> {
    children()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no child process with id {id}")))
}

fn start_reader(
    pipe: Option<impl Read + Send + 'static>,
    sink: Arc<StreamSink>,
    activity: Arc<ActivitySignal>,
) {
    let Some(mut pipe) = pipe else {
        sink.set_eof();
        return;
    };
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                // `push` parks while the buffer is full (async mode), so a
                // child that outruns its consumer blocks on the pipe instead
                // of growing this buffer.
                Ok(n) => {
                    sink.push(&chunk[..n]);
                    activity.notify();
                }
            }
        }
        sink.set_eof();
        activity.notify();
    });
}

/// Probe the child once and record its exit. Called by `spawnPoll` on every
/// loop iteration (the JS child shim polls every 25 ms while a child lives)
/// and by the exit watcher on its backoff schedule.
///
/// The probe itself is lock-short: `try_wait` takes the child lock only for
/// the syscall, so a concurrent kill or `Drop` never waits for the child's
/// whole lifetime. The exit watcher ([`start_exit_watcher`]) is what makes
/// the status update independent of JS polling; this probe keeps the answer
/// fresh even if the watcher thread could not be spawned.
fn refresh_child_exit(handle: &ChildHandle) -> bool {
    record_child_exit(&handle.child, &handle.status, &handle.activity)
}

/// Record an exit from any prober (spawnPoll or the exit watcher).
fn record_child_exit(
    child: &StdMutex<Child>,
    status: &StdMutex<Option<ExitInfo>>,
    activity: &ActivitySignal,
) -> bool {
    if lock(status).is_some() {
        return true;
    }
    let mut child = lock(child);
    match child.try_wait() {
        Ok(Some(exit)) => {
            *lock(status) = Some(exit_info(&exit));
            drop(child);
            activity.notify();
            true
        }
        Ok(None) => false,
        // A transient probe failure (e.g. a handle in a transitional state)
        // is not an exit: keep the child alive and retry on the next probe.
        Err(_) => false,
    }
}

/// Record a child's exit without blocking the shared `Child` handle: a
/// dedicated watcher thread probes with the safe `Child::try_wait` — the same
/// call `spawnPoll` makes — and backs off from [`EXIT_WATCH_BASE_DELAY`] to
/// [`EXIT_WATCH_MAX_DELAY`] between probes.
///
/// `try_wait` is non-blocking and needs only a momentary
/// `Arc<Mutex<Child>>` borrow, so `ChildHandle::drop` can still kill and reap
/// the process while the watcher sleeps. The watcher records the exit through
/// [`record_child_exit`] (which notifies [`ActivitySignal`], waking a parked
/// `spawnPoll`) and then ends; a child that stays alive is re-probed at most
/// once per second, so the watcher neither busy-waits nor leaks. The thread's
/// exit check runs independently of JS polling: `spawnPoll` also probes on
/// every poll tick, which keeps interactive latency to one tick, while this
/// watcher covers the intervals where JS is not polling.
fn start_exit_watcher(
    child: Arc<StdMutex<Child>>,
    status: Arc<StdMutex<Option<ExitInfo>>>,
    activity: Arc<ActivitySignal>,
) {
    std::thread::Builder::new()
        .name("pir-child-exit".to_owned())
        .spawn(move || {
            let mut delay = EXIT_WATCH_BASE_DELAY;
            loop {
                if record_child_exit(&child, &status, &activity) {
                    break;
                }
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2).min(EXIT_WATCH_MAX_DELAY);
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| {
            // Thread exhaustion: spawnPoll's own `try_wait` probe still
            // reports the exit; the watcher is an accelerator, not a
            // correctness requirement.
            crate::ext_diag!(
                "[ext] child exit watcher thread could not start ({error}); polling status"
            );
        });
}

/// Release every child spawned by `generation` (runtime drop or actor
/// `Shutdown`): taking the entries out drops each [`ChildHandle`], which kills
/// and reaps the process and releases its readers. Entries of a successor
/// generation are untouched, so a reload can never kill the new runtime's
/// children.
pub(super) fn release_generation(generation: u64) {
    let stale: Vec<Arc<ChildHandle>> = {
        let mut table = children().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    // Wake a concurrent `spawnPoll` that still holds one of these handles (it
    // then re-checks and releases its own reference).
    for handle in &stale {
        handle.activity.notify();
    }
    drop(stale);
}

/// Number of live child handles (test seam for the generation release).
#[doc(hidden)]
pub fn child_count_for_tests() -> usize {
    children()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
}

/// Park on one [`ActivitySignal`] for `timeout` and report `(parks, notified,
/// elapsed)`: a blocking host wait must park ONCE (the condvar) instead of
/// polling every few milliseconds.
#[doc(hidden)]
pub fn activity_park_probe_for_tests(timeout: Duration) -> (u64, bool, Duration) {
    let activity = ActivitySignal::new();
    let started = Instant::now();
    let notified = activity.wait(timeout);
    (activity.parks(), notified, started.elapsed())
}

/// Park slice of the `spawnSync` post-exit drain for a given
/// `(exit, last chunk, now)` (see [`sync_drain_wait_slice`]).
#[doc(hidden)]
pub fn sync_drain_wait_slice_for_tests(
    exit_at: Instant,
    last_data: Instant,
    now: Instant,
) -> Duration {
    sync_drain_wait_slice(exit_at, last_data, now)
}

/// [`append_base64_chunk`] for the seam test (the concatenation must be a
/// valid single base64 payload, not two glued strings).
#[doc(hidden)]
pub fn append_base64_chunk_for_tests(first: &str, second: &str) -> String {
    append_base64_chunk(first.to_owned(), second.to_owned())
}

fn exit_info(status: &std::process::ExitStatus) -> ExitInfo {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitInfo {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no POSIX signals; only the exit code exists.
        ExitInfo {
            code: status.code(),
            signal: None,
        }
    }
}

/// Signal number -> Node's `signal` name for the common term cases.
fn signal_name(signal: Option<i32>) -> Option<String> {
    signal.map(|number| match number {
        2 => "SIGINT".to_owned(),
        6 => "SIGABRT".to_owned(),
        9 => "SIGKILL".to_owned(),
        15 => "SIGTERM".to_owned(),
        other => format!("SIG{other}"),
    })
}

// ---------------------------------------------------------------------------
// node executable shim
// ---------------------------------------------------------------------------
//
// Extensions assume a `node` executable exists (the real pi ships inside a
// node/bun runtime, so `spawn("node", …)` always resolves there). pir is a
// self-contained binary, so the shim routes the node-family program names to
// pir's own JS runner: the OS program becomes the pir image and the argv is
// translated to `pir js <…>` (the node-CLI-shaped entry forms documented in
// `main_cli/js_mode.rs`). pi-subagents is the driving consumer: its
// `resolveNodeExecutable()` returns the bare name `node`/`node.exe` whenever
// `process.execPath` is not itself node — which is exactly pir's case — and
// its detached background runner spawns `<node> <runner-bootstrap.js> <cfg>`.
//
// What is deliberately NOT rewritten: the JS-visible `spawnfile`/`spawnargs`
// (Node reports the command as passed), `process.execPath` inside the routed
// child (it is the pir image, and `spawn(process.execPath, …)` re-enters the
// same shim through the `js` subcommand), and `process.versions.node` (the
// host stub's value; see the report for the `--version` divergence).

/// Environment marker set on every child the shim routed to the pir JS
/// runner: a routed process (or an operator reading a stack trace) can tell a
/// shimmed `node` from a real one. Additive even when the payload replaced
/// the child environment, and inherited by the routed child's own children.
const NODE_SHIM_ENV: &str = "PIR_NODE_SHIM";

/// The program names the shim claims — exactly the node-family basenames
/// pi-subagents' `resolveNodeExecutable()` can produce (`node` / `node.exe`
/// on Windows, `nodejs` / `nodejs.exe` from the same name check). Only bare
/// names are claimed: an explicit path to a real node installation keeps
/// resolving to that installation.
fn is_node_program(program: &str) -> bool {
    if program.contains('/') || program.contains('\\') {
        return false;
    }
    // Case-insensitive exact match, without the throwaway lowercased `String`
    // the old `to_ascii_lowercase().as_str()` match allocated per call.
    ["node", "node.exe", "nodejs", "nodejs.exe"]
        .iter()
        .any(|name| program.eq_ignore_ascii_case(name))
}

/// Value-taking node CLI flags: flag plus its value are dropped from the
/// routed argv.
///
/// `--import` / `--require` / `--loader` are module preload hooks; pir's
/// runner has no loader-hook surface (a dropped `--import <preload>` means
/// the preload's alias resolution does not run — bare specifiers then resolve
/// through the normal npm search roots). Dropping the pair keeps the script
/// path discoverable, which is the difference between "runs with slightly
/// different resolution" and "runs the preload path as if it were a script".
const NODE_VALUE_FLAGS: &[&str] = &[
    "--import",
    "--require",
    "-r",
    "--loader",
    "--experimental-loader",
];

/// Valueless node CLI flags that are swallowed: pir's runner already
/// transpiles TypeScript, and warning/source-map toggles have no equivalent
/// (they are dropped rather than forwarded, so they never reach the JS
/// loader as a fake script path).
const NODE_VALUELESS_FLAGS: &[&str] = &[
    "--no-warnings",
    "--no-deprecation",
    "--trace-warnings",
    "--trace-deprecation",
    "--enable-source-maps",
    "--experimental-strip-types",
    "--experimental-transform-types",
    "--experimental-modules",
    "--experimental-vm-modules",
    "--experimental-detect-module",
    "--no-experimental-fetch",
    "--no-experimental-require-module",
    "--preserve-symlinks",
    "--no-addons",
];

/// Translate a node invocation into the `pir js` argv.
///
/// | node invocation | routed argv | note |
/// |---|---|---|
/// | `script.js a b` | `script.js a b` | script path and args verbatim |
/// | `-e CODE [rest]` / `--eval CODE` | same | `pir js` evaluates the code |
/// | `--eval=CODE [rest]` | same | |
/// | `-` | `-` | stdin script |
/// | `--version` / `-v [anything]` | `--version` | `pir js --version` answers the compat baseline |
/// | `--import SPEC` / `--require MOD` / `-r MOD` / `--loader SPEC` | dropped with the value | no loader-hook surface |
/// | `--no-warnings`, `--experimental-*`, `--enable-source-maps` | dropped | valueless runtime toggles |
/// | any other leading `--flag` / `--flag=value` / `-x` | dropped | no node CLI flag surface; dropping keeps the script path discoverable |
/// | `--` | dropped | Node's end-of-options marker; the next token is the script |
///
/// Only leading flags (before the script) are touched: everything from the
/// first non-flag token on is verbatim script/argv material.
fn route_node_argv(argv: &[String]) -> Vec<String> {
    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();
        if arg == "--version" || arg == "-v" {
            // Node prints its version and ignores the rest of the line.
            return vec!["--version".to_owned()];
        }
        if arg == "--" {
            // End of node options: the next token is the script.
            index += 1;
            break;
        }
        if arg == "-e" || arg == "--eval" || arg.starts_with("--eval=") || arg == "-" {
            break;
        }
        if NODE_VALUE_FLAGS.contains(&arg) {
            index += 2; // flag plus its value
            continue;
        }
        if NODE_VALUELESS_FLAGS.contains(&arg) || (arg.starts_with('-') && arg.len() > 1) {
            index += 1;
            continue;
        }
        break; // the script path
    }
    argv[index.min(argv.len())..].to_vec()
}

/// The running pir image — the program a routed node spawn is handed. `None`
/// (query failure) leaves the command untouched, so a working native node
/// lookup is never broken by the shim.
fn shim_program() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
}

/// Rewrite a shell command line whose leading command is a bare node-family
/// program: `node x.js && echo ok` becomes `"<pir.exe>" js x.js && echo ok`.
/// Only the leading token changes — the rest of the line travels verbatim, so
/// shell operators, redirection and chaining keep working (this is what makes
/// `exec("node …")` / `execSync("node …")`, which run through the platform
/// shell, reach the runner). `None` for every other line.
fn rewrite_shell_node_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let head_end = trimmed
        .find(|character: char| character.is_whitespace())
        .unwrap_or(trimmed.len());
    // A quoted program name (`"node" x.js`) is accepted; a quoted *path* to a
    // real node is not, because `is_node_program` rejects path components.
    let head = trimmed[..head_end].trim_matches('"');
    if !is_node_program(head) {
        return None;
    }
    let exe = shim_program()?;
    let rest = &trimmed[head_end..];
    Some(format!("\"{exe}\" js{rest}"))
}

// ---------------------------------------------------------------------------
// child: command construction
// ---------------------------------------------------------------------------

/// One spawn's resolved program + arguments, plus whether the node shim
/// rewrote it (marker env).
struct Launch<'a> {
    program: String,
    args: LaunchArgs<'a>,
    /// The node executable shim rewrote this launch.
    shimmed: bool,
}

/// How one launch's arguments reach `Command`.
enum LaunchArgs<'a> {
    /// The payload's own argv is handed to `Command::args` as it is (which
    /// copies it into the `Command` anyway), so the `to_vec()` clone the
    /// struct used to hold is gone. Owned only when the shim rewrote the argv.
    Plain(Cow<'a, [String]>),
    /// A `shell: true` launch: the shell's own options (`/d /s /C` on Windows,
    /// `-c` elsewhere) are ordinary arguments and `line` is the command line
    /// that must reach the OS verbatim. On Windows an ordinary argument goes
    /// through MSVCRT quoting, which backslash-escapes embedded quotes — out
    /// from under cmd.exe's own parser — so the line is appended with
    /// `raw_arg` instead; Unix passes it as the ordinary final argument to
    /// `/bin/sh`.
    Shell { options: Vec<String>, line: String },
}

/// `cmd` + args joined with single spaces — the line a `shell: true` spawn
/// hands to the platform shell (Node's own form: name and args verbatim, no
/// re-quoting). One exact-size buffer instead of collecting a `Vec<&str>` and
/// letting `join` grow the string append by append.
fn join_command_line(cmd: &str, args: &[String]) -> String {
    let size = cmd.len() + args.iter().map(String::len).sum::<usize>() + args.len();
    let mut line = String::with_capacity(size);
    line.push_str(cmd);
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Resolve program + argv, applying the platform shell wrapper when `shell` is
/// set: `cmd.exe /d /s /C <line>` on Windows, `/bin/sh -c <line>` elsewhere.
/// The command name and args travel verbatim (joined with spaces, no
/// re-quoting), exactly like Node's `shell: true` — callers must not pass
/// untrusted input. (`/d` skips cmd's AutoRun and `/s` makes cmd strip only
/// the enclosing quote pair — Node's recipe, which is what keeps a line with
/// embedded quotes intact; see [`LaunchArgs::Shell`].)
///
/// The node executable shim runs here: a bare node-family program is replaced
/// by the pir image with a translated argv (and a shell line starting with one
/// is rewritten in place).
fn command_line<'a>(
    cmd: &str,
    args: &'a [String],
    shell: Option<&JsonValue>,
) -> Result<Launch<'a>, FnError> {
    let resolved = match shell {
        None | Some(JsonValue::Null) | Some(JsonValue::Bool(false)) => None,
        Some(JsonValue::Bool(true)) => Some(default_shell()),
        Some(JsonValue::String(shell)) if !shell.is_empty() => Some(shell.clone()),
        Some(_) => return Err(FnError::new("EINVAL", "invalid `shell` option")),
    };
    match resolved {
        None => {
            if is_node_program(cmd) {
                if let Some(exe) = shim_program() {
                    // `<node> <…>` -> `<pir> js <translated …>`: the `js`
                    // subcommand is the runner's entry (run_mode intercepts it
                    // before clap).
                    let mut argv = Vec::with_capacity(args.len() + 1);
                    argv.push("js".to_owned());
                    argv.extend(route_node_argv(args));
                    return Ok(Launch {
                        program: exe,
                        args: LaunchArgs::Plain(Cow::Owned(argv)),
                        shimmed: true,
                    });
                }
            }
            Ok(Launch {
                program: cmd.to_owned(),
                args: LaunchArgs::Plain(Cow::Borrowed(args)),
                shimmed: false,
            })
        }
        Some(shell) => {
            let owned = join_command_line(cmd, args);
            let (line, shimmed) = match rewrite_shell_node_line(&owned) {
                Some(rewritten) => (rewritten, true),
                None => (owned, false),
            };
            Ok(Launch {
                program: shell,
                args: LaunchArgs::Shell {
                    options: shell_options(),
                    line,
                },
                shimmed,
            })
        }
    }
}

/// The platform shell's own options for the command line carried by
/// [`LaunchArgs::Shell`].
///
/// Windows: `cmd.exe /d /s /C "<line>"` — `/s` makes cmd strip exactly the
/// enclosing pair, so the line reaches cmd's parser byte-identical (quote
/// characters included). This is what makes `exec("node -e \"…\"")` work: the
/// shim rewrite embeds a quoted path, and without `/s` + the raw line, cmd's
/// auto-strip would eat the wrong quote pair.
///
/// Unix: `/bin/sh -c <line>` — execve passes argv verbatim, nothing to do.
fn shell_options() -> Vec<String> {
    let flag = shell_command_flag().to_owned();
    if cfg!(windows) {
        vec!["/d".to_owned(), "/s".to_owned(), flag]
    } else {
        vec![flag]
    }
}

fn default_shell() -> String {
    if cfg!(windows) {
        "cmd.exe".to_owned()
    } else {
        "/bin/sh".to_owned()
    }
}

fn shell_command_flag() -> &'static str {
    if cfg!(windows) {
        "/C"
    } else {
        "-c"
    }
}

/// Build the common spawn configuration (cwd sandbox, env replacement, hidden
/// window) shared by `spawnStart` and `spawnSync`.
fn build_command(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    launch: &Launch<'_>,
    payload: &JsonValue,
) -> Result<Command, FnError> {
    let mut command = Command::new(&launch.program);
    apply_launch_args(&mut command, launch);
    if let Some(cwd) = payload.get("cwd").and_then(JsonValue::as_str) {
        let base = lock_state(state).config.cwd.clone();
        let path = if Path::new(cwd).is_absolute() {
            PathBuf::from(cwd)
        } else {
            base.join(cwd)
        };
        if !cwd_allowed(state, allowed_roots, &path) {
            return Err(FnError::new(
                "EACCES",
                format!("EACCES: permission denied, spawn cwd '{cwd}'"),
            ));
        }
        command.current_dir(path);
    } else {
        // Node's child_process inherits process.cwd(), which for an extension
        // is the runtime's configured cwd — not the host pir process cwd.
        command.current_dir(lock_state(state).config.cwd.clone());
    }
    if let Some(env) = payload.get("env").and_then(JsonValue::as_object) {
        // Node semantics: a provided `env` replaces the child's environment
        // wholesale (string values only; non-strings are skipped by the JS
        // layer).
        command.env_clear();
        for (key, value) in env {
            if let Some(text) = value.as_str() {
                command.env(key, text);
            }
        }
    }
    if payload_bool(payload, "windowsHide") {
        hide_window(&mut command);
    }
    Ok(command)
}

/// Windows: a shell launch's command line is appended with `raw_arg` so no
/// MSVCRT quoting is applied to it; everything else is an ordinary argument.
#[cfg(windows)]
fn apply_launch_args(command: &mut Command, launch: &Launch<'_>) {
    use std::os::windows::process::CommandExt as _;
    match &launch.args {
        LaunchArgs::Plain(argv) => {
            command.args(argv.iter());
        }
        LaunchArgs::Shell { options, line } => {
            command.args(options.iter());
            command.raw_arg(format!("\"{line}\""));
        }
    }
}

#[cfg(not(windows))]
fn apply_launch_args(command: &mut Command, launch: &Launch<'_>) {
    match &launch.args {
        LaunchArgs::Plain(argv) => {
            command.args(argv.iter());
        }
        LaunchArgs::Shell { options, line } => {
            command.args(options.iter());
            command.arg(line);
        }
    }
}

/// `windowsHide: true` maps onto CREATE_NO_WINDOW; the numeric flag keeps the
/// audited windows-sys feature list untouched.
#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

// ---------------------------------------------------------------------------
// child: the `stdio` option
// ---------------------------------------------------------------------------
//
// Node's three-element `stdio` form, in the subset this host can honor:
//
// | entry | host mapping | JS-visible child stream |
// |---|---|---|
// | `"pipe"` (default) | `Stdio::piped()` | a pipe reader/writer |
// | `"ignore"` | `Stdio::null()` | `null` |
// | `"inherit"` | `Stdio::inherit()` | `null` |
// | a parent fd number | a duplicate of the parent's handle (`Stdio::from(File)`) | `null` |
//
// The fd form is what a detached runner uses to point a child's stdout/stderr
// at log files it opened itself (`fs.openSync(path, "w")` then
// `spawn(node, [...], {stdio: ["ignore", fdOut, fdErr]})`): the child writes
// straight into the log file and nothing has to flow back through this host
// registry. The handle is DUPLICATED (`File::try_clone`), so the parent's
// descriptor stays open and both sides share the offset — Node's semantics for
// a numeric stdio entry.
//
// The JS layer only builds `child.stdin/stdout/stderr` for the slots the host
// reports back as `"pipe"`; `spawnSync` resolves the same table and reports a
// non-piped slot as `null` in its result (Node's blocking-form shape).
//
// Residual stdio path (documented, not routed): with `"inherit"` the child
// receives this process's own descriptors, so whatever it prints goes straight
// to the terminal — in an interactive TUI session that is the frame, outside
// the routing `super::super::ext_stdio` applies to the host-backed
// `process.stdout/stderr` bridge (the fd form cannot reach the terminal: only
// descriptors opened through the fs table are accepted, never 1/2). Extensions
// that must stay inside the frame spawn with pipes and read the child's output
// (a selection the host cannot override without breaking Node's `stdio`
// contract — a child awaiting a TTY needs the real descriptors).

/// One spawn's resolved standard streams: the `Stdio` handed to
/// `Command::spawn` plus the labels echoed back to the JS side.
struct ChildStdio {
    slots: [Stdio; 3],
    kinds: [&'static str; 3],
}

impl ChildStdio {
    /// Node's default: three host pipes (the absent `stdio` option).
    fn all_pipes() -> Self {
        Self {
            slots: [Stdio::piped(), Stdio::piped(), Stdio::piped()],
            kinds: ["pipe", "pipe", "pipe"],
        }
    }
}

/// Resolve the payload's `stdio` array (`[stdin, stdout, stderr]`, each
/// `"pipe"`/`"ignore"`/`"inherit"` or a parent fd number). Extra entries past
/// the third are ignored (the JS layer drops them too); a missing entry keeps
/// its `"pipe"` default. A malformed entry is a call-shape error, mirroring
/// Node's synchronous `ERR_INVALID_ARG_VALUE` for `stdio`. Shared by the
/// asynchronous `spawnStart` and the blocking `spawnSync`.
fn resolve_stdio(
    state: &Arc<StdMutex<RuntimeState>>,
    payload: &JsonValue,
) -> Result<ChildStdio, FnError> {
    let mut stdio = ChildStdio::all_pipes();
    let Some(entries) = payload.get("stdio").and_then(JsonValue::as_array) else {
        return Ok(stdio);
    };
    for (index, entry) in entries.iter().take(3).enumerate() {
        match entry {
            JsonValue::Null => {}
            JsonValue::String(kind) if kind == "pipe" => {}
            JsonValue::String(kind) if kind == "ignore" => {
                stdio.slots[index] = Stdio::null();
                stdio.kinds[index] = "ignore";
            }
            JsonValue::String(kind) if kind == "inherit" => {
                stdio.slots[index] = Stdio::inherit();
                stdio.kinds[index] = "inherit";
            }
            JsonValue::Number(number) => {
                let fd = number
                    .as_i64()
                    .ok_or_else(|| bad_fd_error(number.to_string()))?;
                stdio.slots[index] = stdio_handle_from_fd(state, fd)?;
                stdio.kinds[index] = "fd";
            }
            other => {
                return Err(FnError::new(
                    "EINVAL",
                    format!("invalid stdio entry {}", other),
                ))
            }
        }
    }
    Ok(stdio)
}

fn bad_fd_error(fd: impl std::fmt::Display) -> FnError {
    FnError::new("EBADF", format!("EBADF: bad file descriptor, fd {fd}"))
}

/// A numeric `stdio` entry: the parent descriptor's handle, duplicated for the
/// child. The lookup is the fs surface's own descriptor table (`fs.openSync`
/// and friends), so only descriptors the JS side actually opened can be
/// handed over; anything else is `EBADF`, like Node.
fn stdio_handle_from_fd(state: &Arc<StdMutex<RuntimeState>>, fd: i64) -> Result<Stdio, FnError> {
    let fd = i32::try_from(fd).map_err(|_| bad_fd_error(fd))?;
    let mut guard = lock_state(state);
    let entry = guard.fd_table.get_mut(fd).ok_or_else(|| bad_fd_error(fd))?;
    let file = entry.file.try_clone().map_err(|error| {
        FnError::new(
            "EBADF",
            format!("EBADF: fd {fd} cannot be handed to a child: {error}"),
        )
    })?;
    Ok(Stdio::from(file))
}

/// Terminate step shared by `spawnKill` and the blocking test path: Windows has
/// no graceful signal so `Child::kill` (TerminateProcess) is it; Unix sends
/// SIGTERM first, mirroring the `pi.exec` kill path.
fn send_term(child: &mut Child) {
    #[cfg(unix)]
    {
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

// ---------------------------------------------------------------------------
// child: ops
// ---------------------------------------------------------------------------

fn child_op(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    op: &str,
    payload: &JsonValue,
    generation: u64,
) -> OpResult {
    match op {
        "spawnStart" => spawn_start(state, allowed_roots, payload, generation),
        "spawnPoll" => spawn_poll(payload),
        "spawnWrite" => spawn_write(payload),
        "spawnEnd" => spawn_end(payload),
        "spawnKill" => spawn_kill(payload),
        "spawnSync" => spawn_sync(state, allowed_roots, payload),
        other => Err(FnError::new(
            "ENOSYS",
            format!("unknown child op `{other}`"),
        )),
    }
}

fn spawn_start(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
    generation: u64,
) -> OpResult {
    if children()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
        >= MAX_CHILDREN
    {
        return Err(FnError::new(
            "EAGAIN",
            format!("too many live child processes (max {MAX_CHILDREN})"),
        ));
    }
    let cmd = payload_str(payload, "cmd")?;
    let args = payload_strings(payload, "args");
    let launch = command_line(cmd, &args, payload.get("shell"))?;
    let mut command = build_command(state, allowed_roots, &launch, payload)?;
    if launch.shimmed {
        command.env(NODE_SHIM_ENV, "1");
    }
    let stdio = resolve_stdio(state, payload)?;
    let kinds = stdio.kinds;
    let [stdin_slot, stdout_slot, stderr_slot] = stdio.slots;
    command
        .stdin(stdin_slot)
        .stdout(stdout_slot)
        .stderr(stderr_slot);
    let mut child = command
        .spawn()
        .map_err(|error| FnError::spawn_failure(&error, cmd))?;

    let id = NEXT_CHILD_ID.fetch_add(1, Ordering::SeqCst);
    let stdout_sink = StreamSink::new(format!("{id} stdout"), SinkMode::Backpressure);
    let stderr_sink = StreamSink::new(format!("{id} stderr"), SinkMode::Backpressure);
    let status: Arc<StdMutex<Option<ExitInfo>>> = Arc::new(StdMutex::new(None));
    let activity = ActivitySignal::new();
    start_reader(
        child.stdout.take(),
        Arc::clone(&stdout_sink),
        Arc::clone(&activity),
    );
    start_reader(
        child.stderr.take(),
        Arc::clone(&stderr_sink),
        Arc::clone(&activity),
    );
    let stdin = child.stdin.take();
    let pid = child.id();
    let child = Arc::new(StdMutex::new(child));
    start_exit_watcher(
        Arc::clone(&child),
        Arc::clone(&status),
        Arc::clone(&activity),
    );

    children()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            id,
            Arc::new(ChildHandle {
                child,
                stdin: StdMutex::new(stdin),
                stdout: stdout_sink,
                stderr: stderr_sink,
                status,
                activity,
                generation,
            }),
        );
    Ok(serde_json::json!({ "id": id, "pid": pid, "stdio": kinds }))
}

fn spawn_poll(payload: &JsonValue) -> OpResult {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing child id"))?;
    let handle = child_by_id(id)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(MAX_POLL_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    // Bounded wait for something to report: new output or the exit. The
    // readers/waiter notify `activity`, so the thread parks on the condvar
    // (one wake per event) instead of sleeping 5 ms steps; the wait slice is
    // the lost-wakeup safety net.
    loop {
        // Probe the child here rather than on a dedicated 10 ms waiter
        // thread: this loop is already the consumer of the status and runs
        // once per JS poll (25 ms), so an exit is reported just as promptly
        // with no extra thread and no idle wakeups for unpolled children.
        refresh_child_exit(&handle);
        let seen = handle.activity.epoch();
        if lock(&handle.status).is_some() || handle.stdout.has_data() || handle.stderr.has_data() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        handle
            .activity
            .wait_since(seen, (deadline - now).min(CHILD_WAIT_SLICE));
    }
    if lock(&handle.status).is_some() {
        // Short bounded drain so the poll that reports the exit also carries
        // the readers' final tail (notified per chunk; one slice is the
        // safety net).
        let drain_deadline = Instant::now() + EXIT_DRAIN_GRACE;
        loop {
            let seen = handle.activity.epoch();
            if handle.stdout.eof() && handle.stderr.eof() {
                break;
            }
            let now = Instant::now();
            if now >= drain_deadline {
                break;
            }
            handle
                .activity
                .wait_since(seen, (drain_deadline - now).min(CHILD_WAIT_SLICE));
        }
    }
    let exit = *lock(&handle.status);
    let (mut stdout, mut stdout_dropped) = handle.stdout.take_base64();
    let (mut stderr, mut stderr_dropped) = handle.stderr.take_base64();
    if exit.is_some() && !(handle.stdout.eof() && handle.stderr.eof()) {
        // The drain above could not reach EOF because a reader was parked on a
        // FULL window (the JS pump was not draining it); `take_base64` just
        // released it (backpressure). Give the readers one more bounded window
        // so this final sample still carries their tail.
        let drain_deadline = Instant::now() + EXIT_DRAIN_GRACE;
        loop {
            let seen = handle.activity.epoch();
            if handle.stdout.eof() && handle.stderr.eof() {
                break;
            }
            let now = Instant::now();
            if now >= drain_deadline {
                break;
            }
            handle
                .activity
                .wait_since(seen, (drain_deadline - now).min(CHILD_WAIT_SLICE));
        }
        // The readers may have appended the tail after the take above; fold it
        // into the same sample (the JS side decodes each poll's payload as one
        // byte chunk, so two base64 strings cannot simply be concatenated).
        let (tail, tail_dropped) = handle.stdout.take_base64();
        stdout = append_base64_chunk(stdout, tail);
        stdout_dropped |= tail_dropped;
        let (tail, tail_dropped) = handle.stderr.take_base64();
        stderr = append_base64_chunk(stderr, tail);
        stderr_dropped |= tail_dropped;
    }
    // An exited child is final after the bounded drain windows, even when a
    // detached descendant still holds an inherited pipe handle: the readers
    // can never reach EOF in that case, and gating `done` on them would keep
    // the entry registered and the JS pump polling forever while the child's
    // `exit` event was never delivered (Node delivers `exit` at process exit;
    // only the trailing bytes of the descendant are abandoned, which the
    // bounded windows above already document).
    let done = exit.is_some();
    if done {
        // Final poll: removing the entry drops the handle (kill + reap).
        children()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
    Ok(serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
        "truncated": stdout_dropped || stderr_dropped,
        "exited": exit.is_some(),
        "exitCode": exit.and_then(|exit| exit.code),
        "signal": signal_name(exit.and_then(|exit| exit.signal)),
        "done": done,
    }))
}

fn spawn_write(payload: &JsonValue) -> OpResult {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing child id"))?;
    let handle = child_by_id(id)?;
    let data = payload_str(payload, "data")?;
    let bytes = STANDARD
        .decode(data.as_bytes())
        .map_err(|error| FnError::new("EINVAL", format!("invalid base64 payload: {error}")))?;
    let mut guard = lock(&handle.stdin);
    let Some(stdin) = guard.as_mut() else {
        return Err(FnError::new(
            "EPIPE",
            format!("EPIPE: child {id} stdin is closed"),
        ));
    };
    stdin
        .write_all(&bytes)
        .and_then(|()| stdin.flush())
        .map_err(|error| {
            FnError::new(
                "EPIPE",
                format!("EPIPE: write to child {id} failed: {error}"),
            )
        })?;
    Ok(JsonValue::Bool(true))
}

fn spawn_end(payload: &JsonValue) -> OpResult {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing child id"))?;
    let handle = child_by_id(id)?;
    // Dropping the handle closes the pipe: the child sees stdin EOF.
    drop(lock(&handle.stdin).take());
    Ok(JsonValue::Bool(true))
}

fn spawn_kill(payload: &JsonValue) -> OpResult {
    let id = payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing child id"))?;
    let handle = child_by_id(id)?;
    let mut child = lock(&handle.child);
    send_term(&mut child);
    Ok(JsonValue::Bool(true))
}

/// Blocking `spawnSync`: the stdin writer, both pipe readers and the wait all
/// run on separate threads, so a child that fills a pipe before reading its
/// input cannot deadlock the call (the same shape `runtime/exec.rs` uses).
///
/// The caller's `stdio` resolves exactly like `spawnStart`'s (`pipe` /
/// `ignore` / `inherit` / parent fd); a slot that is not a pipe has no bytes to
/// report and comes back as `null`, Node's blocking-form result shape.
fn spawn_sync(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
) -> OpResult {
    let cmd = payload_str(payload, "cmd")?;
    let args = payload_strings(payload, "args");
    let launch = command_line(cmd, &args, payload.get("shell"))?;
    let input = payload
        .get("input")
        .and_then(JsonValue::as_str)
        .map(|data| {
            STANDARD
                .decode(data.as_bytes())
                .map_err(|error| FnError::new("EINVAL", format!("invalid base64 input: {error}")))
        })
        .transpose()?;

    let mut command = build_command(state, allowed_roots, &launch, payload)?;
    if launch.shimmed {
        command.env(NODE_SHIM_ENV, "1");
    }
    let stdio = resolve_stdio(state, payload)?;
    let kinds = stdio.kinds;
    // Node feeds `input` only through a piped stdin: an `"ignore"`/`"inherit"`/
    // fd slot drops it silently (no error, no bytes).
    let input = if kinds[0] == "pipe" { input } else { None };
    let [stdin_slot, stdout_slot, stderr_slot] = stdio.slots;
    command
        .stdin(stdin_slot)
        .stdout(stdout_slot)
        .stderr(stderr_slot);
    let mut child = command
        .spawn()
        .map_err(|error| FnError::spawn_failure(&error, cmd))?;
    let pid = child.id();

    // `SinkMode::Truncate`: this call is inside `child.wait()` below, so no
    // consumer can drain while the child runs — a parked reader would deadlock
    // against a child that outruns the cap. Keeping the newest bytes and
    // reporting `truncated` is Node's own blocking-form degradation
    // (`maxBuffer`).
    let stdout_sink = StreamSink::new(format!("spawnSync {pid} stdout"), SinkMode::Truncate);
    let stderr_sink = StreamSink::new(format!("spawnSync {pid} stderr"), SinkMode::Truncate);
    let activity = ActivitySignal::new();
    start_reader(
        child.stdout.take(),
        Arc::clone(&stdout_sink),
        Arc::clone(&activity),
    );
    start_reader(
        child.stderr.take(),
        Arc::clone(&stderr_sink),
        Arc::clone(&activity),
    );
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(input) = input {
            // Detached: the write ends at EPIPE when the child exits without
            // reading, so the joined wait below can never block on it.
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
        }
        // Otherwise the handle drops here: the pipe's write end closes, so a
        // child that reads stdin sees EOF at once (Node's blocking form — the
        // asynchronous `spawn` keeps it open for `child.stdin.write`).
    }

    let status = child
        .wait()
        .map_err(|error| FnError::new("EIO", format!("waiting for `{cmd}` failed: {error}")))?;
    // Post-exit quiet window (see EXIT_STDIO_GRACE): a descendant that
    // inherited the pipes must not hang the call. The window re-arms on every
    // chunk either reader captures, so a still-writing descendant is drained.
    let exit_at = Instant::now();
    loop {
        let seen = activity.epoch();
        if stdout_sink.eof() && stderr_sink.eof() {
            break;
        }
        let last_data = stdout_sink.last_data().max(stderr_sink.last_data());
        // Recompute the re-armed quiet window per wake: a chunk restarts the
        // grace, so the slice must never outlive `exit_at.max(last_data) +
        // grace` (and must not collapse to a busy spin when the last chunk
        // predates the exit).
        let remaining = sync_drain_wait_slice(exit_at, last_data, Instant::now());
        if remaining.is_zero() {
            break;
        }
        let _ = activity.wait_since(seen, remaining);
    }
    let exit = exit_info(&status);
    let (stdout, stdout_dropped) = stdout_sink.take_base64();
    let (stderr, stderr_dropped) = stderr_sink.take_base64();
    Ok(serde_json::json!({
        "pid": pid,
        "status": exit.code,
        "signal": signal_name(exit.signal),
        "stdout": captured_slot(kinds[1], stdout),
        "stderr": captured_slot(kinds[2], stderr),
        "truncated": stdout_dropped || stderr_dropped,
    }))
}

/// One `spawnSync` result slot: the base64 text of what the pipe readers
/// captured, or `null` for a descriptor this call did not pipe (`ignore`,
/// `inherit`, or a parent fd) — Node reports `null` there. A non-piped slot has
/// no reader behind its sink, so its encode is of an empty buffer (no
/// allocation); the text itself is already encoded by
/// [`StreamBuf::take_base64`].
fn captured_slot(kind: &str, base64: String) -> JsonValue {
    if kind == "pipe" {
        JsonValue::String(base64)
    } else {
        JsonValue::Null
    }
}

/// Append a later base64 chunk to an earlier one as ONE base64 payload of the
/// concatenated bytes (the JS side decodes each poll's payload as a single
/// chunk, so `first + second` would be invalid base64). The empty-second fast
/// path keeps the common single-take poll allocation-free; the combine only
/// runs when a released backpressured reader appended a tail after the take.
fn append_base64_chunk(first: String, second: String) -> String {
    if second.is_empty() {
        return first;
    }
    let (Ok(first_bytes), Ok(second_bytes)) = (
        STANDARD.decode(first.as_bytes()),
        STANDARD.decode(second.as_bytes()),
    ) else {
        // Unreachable for text this module encoded; never lose `first` on a
        // hypothetical corrupt tail.
        return first;
    };
    let mut bytes = first_bytes;
    bytes.extend_from_slice(&second_bytes);
    STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// zlib
// ---------------------------------------------------------------------------

/// `gzip`/`gunzip`/`deflate`/`inflate` over flate2, `brotliCompress`/
/// `brotliDecompress` over the `brotli` crate and `zstdCompress`/
/// `zstdDecompress` over `zstd`, base64 in/out. `deflate` produces the
/// zlib-wrapped stream and `inflate` consumes it, matching Node's
/// `zlib.deflate`/`zlib.inflate` pair.
///
/// The two modern codecs take their encoder knobs from the payload as plain
/// numbers, resolved on the JS side (Node ignores unknown options there, so
/// only the keys the shim understood are forwarded):
///   * brotli: `quality` (BROTLI_PARAM_QUALITY, 0..=11, node default 11) and
///     `lgwin` (BROTLI_PARAM_LGWIN, 10..=24, node default 22);
///   * zstd: `level` (node's ZSTD_CLEVEL_DEFAULT is 3).
fn zlib_op(op: &str, payload: &JsonValue) -> OpResult {
    let data = STANDARD
        .decode(payload_str(payload, "data")?.as_bytes())
        .map_err(|error| FnError::new("EINVAL", format!("invalid base64 payload: {error}")))?;
    let level = flate2::Compression::default();
    let out = match op {
        "gzip" => zlib_encode(flate2::read::GzEncoder::new(&data[..], level))?,
        "deflate" => zlib_encode(flate2::read::ZlibEncoder::new(&data[..], level))?,
        "gunzip" => zlib_decode(flate2::read::GzDecoder::new(&data[..]))?,
        "inflate" => zlib_decode(flate2::read::ZlibDecoder::new(&data[..]))?,
        "brotliCompress" => zlib_brotli_compress(&data, payload)?,
        "brotliDecompress" => zlib_brotli_decompress(&data)?,
        "zstdCompress" => zlib_zstd_compress(&data, payload)?,
        "zstdDecompress" => zlib_zstd_decompress(&data)?,
        other => return Err(FnError::new("ENOSYS", format!("unknown zlib op `{other}`"))),
    };
    Ok(JsonValue::String(STANDARD.encode(out)))
}

/// Largest output one decompression call may materialize (32 MiB).
///
/// A compressed payload is attacker-sized by definition: a tiny gzip/brotli/
/// zstd frame can expand to gigabytes (a decompression bomb) and would be
/// allocated on the HOST heap, outside the QuickJS heap cap. The cap matches
/// the `fs.readFile` budget; a legitimate larger stream must use the
/// streaming codec surface rather than one-shot sync helpers.
const MAX_DECOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

fn payload_i32(payload: &JsonValue, key: &str) -> Option<i32> {
    payload
        .get(key)
        .and_then(JsonValue::as_i64)
        .map(|value| value as i32)
}

/// One-shot Brotli encode with node's defaults for the absent knobs. The whole
/// input is in memory, so a single `BrotliCompress` call with the finish flag
/// is the same output shape the streaming encoder emits at `end()`.
fn zlib_brotli_compress(data: &[u8], payload: &JsonValue) -> Result<Vec<u8>, FnError> {
    let mut params = brotli::enc::BrotliEncoderParams {
        quality: payload_i32(payload, "quality").unwrap_or(11),
        lgwin: payload_i32(payload, "lgwin").unwrap_or(22),
        ..Default::default()
    };
    params.size_hint = data.len();
    let mut out = Vec::with_capacity(data.len() / 2 + 16);
    brotli::BrotliCompress(&mut &data[..], &mut out, &params).map_err(|error| {
        FnError::new("Z_BUF_ERROR", format!("brotli compression failed: {error}"))
    })?;
    Ok(out)
}

fn zlib_brotli_decompress(data: &[u8]) -> Result<Vec<u8>, FnError> {
    let decoder = brotli::Decompressor::new(data, 4096);
    zlib_decode(decoder)
}

fn zlib_zstd_compress(data: &[u8], payload: &JsonValue) -> Result<Vec<u8>, FnError> {
    let level = payload_i32(payload, "level").unwrap_or(3);
    zstd::stream::encode_all(data, level)
        .map_err(|error| FnError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}")))
}

fn zlib_zstd_decompress(data: &[u8]) -> Result<Vec<u8>, FnError> {
    let decoder = zstd::stream::read::Decoder::new(data).map_err(|error| {
        FnError::new(
            "Z_DATA_ERROR",
            format!("Z_DATA_ERROR: zstd decompression failed: {error}"),
        )
    })?;
    zlib_decode(decoder)
}

fn zlib_encode(mut encoder: impl Read) -> Result<Vec<u8>, FnError> {
    let mut out = Vec::new();
    encoder
        .read_to_end(&mut out)
        .map_err(|error| FnError::new("EIO", format!("compression failed: {error}")))?;
    Ok(out)
}

fn zlib_decode(decoder: impl Read) -> Result<Vec<u8>, FnError> {
    let mut out = Vec::new();
    decoder
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|error| {
            // Node's decompression failures carry `Z_DATA_ERROR`.
            FnError::new(
                "Z_DATA_ERROR",
                format!("Z_DATA_ERROR: decompression failed: {error}"),
            )
        })?;
    if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(FnError::new(
            "Z_BUF_ERROR",
            format!(
                "Z_BUF_ERROR: decompressed output exceeds the {}-MiB limit",
                MAX_DECOMPRESSED_BYTES / (1024 * 1024)
            ),
        ));
    }
    Ok(out)
}
