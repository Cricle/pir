//! `child_process` + `zlib` host backends for `pir-node`.
//!
//! Ported from pir's `src/extensions/runtime/host_functions/child_fns.rs`.
//! The host C-ABI glue supplies the JSON envelope, so this module only speaks
//! `serde_json::Value` and returns [`NodeError`]; host state arrives through
//! [`NodeRuntime`] (`NodeContext` / `NodeHooks`).
//!
//! Command execution is the same capability class as `pi.exec`; spawning a
//! process needs no new permission gate. The filesystem *parameters* are still
//! sandboxed — a `cwd` outside the effective roots (allow-list + agent dir +
//! runtime cwd) is rejected with `EACCES`.
//!
//! Queue bounds: each child stream is captured by a reader thread into a
//! bounded sink ([`MAX_STREAM_BYTES`]). Async `spawn` applies backpressure at
//! the cap (the reader parks, the OS pipe fills, the child blocks — Node's
//! paused-until-read contract). Blocking `spawnSync` cannot backpressure (its
//! caller is inside `wait()`), so it keeps the newest bytes and reports
//! `truncated`, Node's `maxBuffer` degradation for that form.
//!
//! Target deviations from the pir-side implementation: signalling uses only
//! `std::process::Child::kill` (no `libc` dependency, so no graceful SIGTERM
//! step), numeric `stdio` descriptors are rejected with `EBADF` (there is no
//! `fs.openSync` descriptor table in this crate), and the node-executable shim
//! routes through `std::env::current_exe()`.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value as JsonValue};

use crate::pir_node::bridge::payload_bool;
use crate::pir_node::error::{NodeError, NodeResult};
use crate::pir_node::runtime::NodeRuntime;

mod zlib_stream;

/// Diagnostic sink captured from `runtime.context` (replaces `ext_diag!`).
type Diag = Arc<dyn Fn(&str) + Send + Sync>;

// ---------------------------------------------------------------------------
// Payload / locking helpers (local so error messages match the source exactly)
// ---------------------------------------------------------------------------

/// Borrow one string field. Every caller only reads it (the value is copied
/// into the `Command` at spawn time, or base64-decoded in place).
fn payload_str<'a>(payload: &'a JsonValue, key: &str) -> NodeResult<&'a str> {
    payload
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing string payload field `{key}`")))
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

fn payload_i32(payload: &JsonValue, key: &str) -> Option<i32> {
    payload
        .get(key)
        .and_then(JsonValue::as_i64)
        .map(|value| value as i32)
}

fn payload_id(payload: &JsonValue) -> NodeResult<u64> {
    payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", "missing child id"))
}

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Capture the owning runtime's diagnostic sink as a `'static` callback; the
/// detached reader/watcher threads hold it without holding the runtime.
fn diag_callback(runtime: &NodeRuntime) -> Diag {
    let context = Arc::clone(&runtime.context);
    Arc::new(move |message: &str| context.diag(message))
}

/// Map a `Command::spawn` failure: Node reports an unresolvable program as
/// `ENOENT` with `syscall: "spawn <cmd>"` and `path: <cmd>`.
fn spawn_failure(error: &std::io::Error, cmd: &str) -> NodeError {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        "ENOENT"
    } else {
        "EACCES"
    };
    let mut err = NodeError::new(code, format!("{code}: spawn {cmd} {error}"));
    err.syscall = Some(format!("spawn {cmd}"));
    err.path = Some(cmd.to_owned());
    err
}

// ---------------------------------------------------------------------------
// Sandbox check for the child's working directory
// ---------------------------------------------------------------------------

/// Agent dir, the single source of truth used by the pir-side effective roots:
/// `$PIR_AGENT_DIR`, `$PIR_CODING_AGENT_DIR`, `$PI_CODING_AGENT_DIR`, else
/// `~/.pi/agent`. This crate cannot depend on pir's provider settings, so the
/// same lookup is reproduced with `std::env` only.
fn agent_dir() -> PathBuf {
    for var in [
        "PIR_AGENT_DIR",
        "PIR_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ] {
        if let Some(dir) = std::env::var_os(var) {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    home.join(".pi").join("agent")
}

/// Effective sandbox roots: the loader allow-list plus the agent directory and
/// the runtime cwd — the phase-2 rule, re-applied here because a child runs
/// *inside* whatever directory it is spawned in.
fn effective_roots(runtime: &NodeRuntime) -> Vec<PathBuf> {
    let mut keys: Vec<PathBuf> = runtime
        .allowed_roots
        .lock()
        .map(|roots| roots.iter().map(|root| sandbox_key(root)).collect())
        .unwrap_or_default();
    keys.push(sandbox_key(&agent_dir()));
    keys.push(sandbox_key(&runtime.context.cwd()));
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

fn cwd_allowed(runtime: &NodeRuntime, cwd: &Path) -> bool {
    let key = sandbox_key(cwd);
    effective_roots(runtime)
        .iter()
        .any(|root| within(&key, root))
}

// ---------------------------------------------------------------------------
// child: process registry
// ---------------------------------------------------------------------------

/// Maximum bytes buffered per stream of a spawned child before the reader
/// stops draining the OS pipe. Async `spawn` parks at the cap; blocking
/// `spawnSync` truncates from the oldest end and raises the `truncated` marker.
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// One in [`MAX_STREAM_BYTES`] worth of reader chunks: the cadence of the
/// overflow diagnostic (the first occurrence is always reported).
const STREAM_REPORT_EVERY: u64 = 4096;

/// Upper bound of one `spawnPoll` wait; the JS poll loop asks for less so the
/// runtime thread is never blocked for long.
const MAX_POLL_WAIT_MS: u64 = 200;

/// Longest single condvar park slice of a blocking host wait: the wait is
/// signalled by the reader/waiter, so this is only the lost-wakeup safety net.
const CHILD_WAIT_SLICE: Duration = Duration::from_millis(50);

/// Extra bounded drain after the child exited: the pipe readers normally hit
/// EOF immediately, and this grace lets the final poll carry the complete tail.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(100);

/// Post-exit quiet window used by the blocking `spawnSync` (mirrors
/// `runtime/exec.rs`: a detached descendant that inherited the pipes must not
/// hang the call forever).
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(300);

/// First sleep of the exit watcher before it re-probes with `Child::try_wait`.
const EXIT_WATCH_BASE_DELAY: Duration = Duration::from_millis(10);

/// Exit-watcher backoff ceiling: after the doubling settles, a live child costs
/// at most one `try_wait` wakeup per second and an exit is noticed within 1 s.
const EXIT_WATCH_MAX_DELAY: Duration = Duration::from_secs(1);

/// How a stream sink answers a full buffer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SinkMode {
    /// Async `spawn`: the reader parks until the consumer drains; the child
    /// blocks on the pipe — Node's paused-stream semantics, zero bytes dropped.
    Backpressure,
    /// Blocking `spawnSync`: the caller is inside `child.wait()` and cannot
    /// consume; keep the newest bytes and raise the truncation marker.
    Truncate,
}

/// One wait slice of the blocking `spawnSync` post-exit drain, bounded by the
/// quiet deadline `exit_at.max(last_data) + EXIT_STDIO_GRACE`.
fn sync_drain_wait_slice(exit_at: Instant, last_data: Instant, now: Instant) -> Duration {
    let deadline = exit_at.max(last_data) + EXIT_STDIO_GRACE;
    deadline
        .saturating_duration_since(now)
        .min(CHILD_WAIT_SLICE)
}

/// One process stream's bytes, captured by its reader thread.
struct StreamBuf {
    data: VecDeque<u8>,
    /// Bytes were dropped since the last `take` (the truncation marker).
    dropped: bool,
    /// How many chunks were dropped in total (diagnostic cadence).
    dropped_chunks: u64,
    eof: bool,
    /// Last data arrival; re-arms the post-exit quiet window.
    last_data: Instant,
    mode: SinkMode,
    /// Per-sink capture cap (a hard memory bound, independent of Node's
    /// `maxBuffer` accounting).
    max_bytes: usize,
    /// `spawnSync`'s `maxBuffer`: once more than this many bytes arrived the
    /// sink reports overflow, but the captured bytes are kept (Node returns
    /// the full output even when it reports ENOBUFS).
    overflow_at: Option<usize>,
    /// Total bytes seen by this sink (for `overflow_at`).
    seen_bytes: u64,
    /// The handle was dropped (child killed/reaped): a parked reader stops.
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
            max_bytes: MAX_STREAM_BYTES,
            overflow_at: None,
            seen_bytes: 0,
            closed: false,
            waits: 0,
        }
    }

    fn with_limit(mode: SinkMode, max_bytes: usize) -> Self {
        let mut buf = Self::new(mode);
        buf.max_bytes = max_bytes.max(1);
        buf
    }

    /// A sink whose *accounting* threshold is `overflow_at` while its byte
    /// capture stays bounded by `max_bytes` (spawnSync: Node reports ENOBUFS
    /// but still returns everything it read).
    fn with_overflow_limit(mode: SinkMode, max_bytes: usize, overflow_at: Option<usize>) -> Self {
        let mut buf = Self::with_limit(mode, max_bytes);
        buf.overflow_at = overflow_at;
        buf
    }

    /// Append one chunk under the standing overflow policy. Returns whether
    /// the caller must park (the buffer is full and the mode backpressures).
    fn push_now(&mut self, chunk: &[u8]) -> bool {
        self.last_data = Instant::now();
        self.seen_bytes = self.seen_bytes.saturating_add(chunk.len() as u64);
        if let Some(limit) = self.overflow_at {
            if self.seen_bytes > limit as u64 {
                // Accounting overflow only: keep the bytes for the result.
                self.dropped = true;
            }
        }
        // Backpressure: park until the WHOLE chunk fits, so the cap is never
        // crossed and no byte is dropped (the caller waits on the condvar).
        if self.mode == SinkMode::Backpressure && self.data.len() + chunk.len() > self.max_bytes {
            return true;
        }
        // A reader hands over at most one 8 KiB read at a time; this branch is
        // defensive: keep only the newest max bytes.
        if chunk.len() > self.max_bytes {
            self.data.clear();
            self.data.extend(&chunk[chunk.len() - self.max_bytes..]);
            self.dropped = true;
            self.dropped_chunks = self.dropped_chunks.saturating_add(1);
            return false;
        }
        let total = self.data.len() + chunk.len();
        if total > self.max_bytes {
            self.data.drain(..total - self.max_bytes);
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
    /// bytes were lost.
    fn take_base64(&mut self) -> (String, bool) {
        let dropped = self.dropped;
        self.dropped = false;
        let encoded = STANDARD.encode(self.data.make_contiguous());
        self.data.clear();
        (encoded, dropped)
    }
}

/// One child stream as the reader thread and the host ops share it: the
/// mutex-guarded buffer, the condvar that parks a backpressured reader, and the
/// runtime diagnostic sink.
struct StreamSink {
    label: String,
    buf: StdMutex<StreamBuf>,
    /// Signalled when the consumer drained (`take_base64`) or the sink closed.
    space: Condvar,
    max_bytes: usize,
    diag: Diag,
}

impl StreamSink {
    fn new(label: String, mode: SinkMode, diag: Diag) -> Arc<Self> {
        Self::with_limit(label, mode, MAX_STREAM_BYTES, diag)
    }

    fn with_limit(label: String, mode: SinkMode, max_bytes: usize, diag: Diag) -> Arc<Self> {
        let max_bytes = max_bytes.max(1);
        Arc::new(Self {
            label,
            buf: StdMutex::new(StreamBuf::with_limit(mode, max_bytes)),
            space: Condvar::new(),
            max_bytes,
            diag,
        })
    }

    fn with_overflow_limit(
        label: String,
        mode: SinkMode,
        max_bytes: usize,
        overflow_at: Option<usize>,
        diag: Diag,
    ) -> Arc<Self> {
        let max_bytes = max_bytes.max(1);
        Arc::new(Self {
            label,
            buf: StdMutex::new(StreamBuf::with_overflow_limit(mode, max_bytes, overflow_at)),
            space: Condvar::new(),
            max_bytes,
            diag,
        })
    }

    /// Push one reader chunk. In [`SinkMode::Backpressure`] this parks while
    /// the buffer is full; in [`SinkMode::Truncate`] it never parks.
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
                parked = true;
                buf.waits = buf.waits.saturating_add(1);
                let waits = buf.waits;
                if waits == 1 || waits.is_multiple_of(STREAM_REPORT_EVERY) {
                    (self.diag)(&format!(
                        "[ext] child {} stream full ({} bytes buffered); the reader is paused (backpressure) after {} waits",
                        self.label, self.max_bytes, waits
                    ));
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

    /// Has this sink dropped bytes since the capture began?
    fn overflowed(&self) -> bool {
        let buf = lock(&self.buf);
        buf.dropped || buf.dropped_chunks > 0
    }

    fn last_data(&self) -> Instant {
        lock(&self.buf).last_data
    }

    /// Drain the buffer and release a parked reader.
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
            (self.diag)(&format!(
                "[ext] child {} output exceeded the {} byte stream cap; the oldest bytes were truncated ({} chunks so far)",
                self.label, self.max_bytes, dropped_chunks
            ));
        }
    }

    fn set_eof(&self) {
        lock(&self.buf).eof = true;
    }

    /// Release a parked reader and stop it: the handle is going away.
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

/// One child's activity signal: a monotone counter plus a condvar.
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

    /// Current activity generation.
    fn epoch(&self) -> u64 {
        lock(&self.state).0
    }

    /// Park until a notification newer than `seen`, or `timeout`.
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

/// One live spawned child. Removing the entry (or releasing the generation)
/// drops this handle, which kills + reaps the process and releases its readers.
struct ChildHandle {
    child: Arc<StdMutex<Child>>,
    stdin: StdMutex<Option<ChildStdin>>,
    stdout: Arc<StreamSink>,
    stderr: Arc<StreamSink>,
    status: Arc<StdMutex<Option<ExitInfo>>>,
    /// Wakes `spawnPoll`'s bounded wait on output/exit.
    activity: Arc<ActivitySignal>,
    /// Owning runtime generation (reload/shutdown teardown).
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
        // Reap: without the wait the child stays a zombie on Unix.
        let _ = child.wait();
        drop(lock(&self.stdin).take());
        // Release (and stop) readers parked on a full buffer.
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

fn child_by_id(id: u64) -> NodeResult<Arc<ChildHandle>> {
    children()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| NodeError::new("ESRCH", format!("no child process with id {id}")))
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

/// Probe the child once and record its exit.
fn refresh_child_exit(handle: &ChildHandle) -> bool {
    record_child_exit(&handle.child, &handle.status, &handle.activity)
}

/// Record an exit from any prober (`spawnPoll` or the exit watcher).
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
        Err(_) => false,
    }
}

/// Record a child's exit without blocking the shared `Child` handle: a
/// dedicated watcher probes with `Child::try_wait` and backs off from
/// [`EXIT_WATCH_BASE_DELAY`] to [`EXIT_WATCH_MAX_DELAY`] between probes.
fn start_exit_watcher(
    child: Arc<StdMutex<Child>>,
    status: Arc<StdMutex<Option<ExitInfo>>>,
    activity: Arc<ActivitySignal>,
    diag: Diag,
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
            // Thread exhaustion: spawnPoll's own try_wait probe still reports
            // the exit; the watcher is an accelerator, not a correctness need.
            diag(&format!(
                "[ext] child exit watcher thread could not start ({error}); polling status"
            ));
        });
}

/// Release every child spawned by `generation` (runtime drop or actor
/// `Shutdown`): taking the entries out drops each [`ChildHandle`], which kills
/// and reaps the process and releases its readers. Entries of a successor
/// generation are untouched.
pub fn release_generation(generation: u64) {
    let stale: Vec<Arc<ChildHandle>> = {
        let mut table = lock(children());
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    // Wake a concurrent `spawnPoll` that still holds one of these handles.
    for handle in &stale {
        handle.activity.notify();
    }
    drop(stale);
}

/// Number of live child handles (test seam for the generation release).
#[doc(hidden)]
pub fn child_count_for_tests() -> usize {
    lock(children()).len()
}

/// Park on one [`ActivitySignal`] and report `(parks, notified, elapsed)`.
#[doc(hidden)]
pub fn activity_park_probe_for_tests(timeout: Duration) -> (u64, bool, Duration) {
    let activity = ActivitySignal::new();
    let started = Instant::now();
    let notified = activity.wait(timeout);
    (activity.parks(), notified, started.elapsed())
}

/// Park slice of the `spawnSync` post-exit drain for a given tuple.
#[doc(hidden)]
pub fn sync_drain_wait_slice_for_tests(
    exit_at: Instant,
    last_data: Instant,
    now: Instant,
) -> Duration {
    sync_drain_wait_slice(exit_at, last_data, now)
}

/// [`append_base64_chunk`] for the seam test.
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

/// Environment marker set on every child the shim routed to the JS runner.
const NODE_SHIM_ENV: &str = "PIR_NODE_SHIM";

/// The program names the shim claims — exactly the node-family basenames
/// `resolveNodeExecutable()` can produce. Only bare names are claimed.
fn is_node_program(program: &str) -> bool {
    if program.contains('/') || program.contains('\\') {
        return false;
    }
    ["node", "node.exe", "nodejs", "nodejs.exe"]
        .iter()
        .any(|name| program.eq_ignore_ascii_case(name))
}

/// Value-taking node CLI flags: flag plus its value are dropped from argv.
const NODE_VALUE_FLAGS: &[&str] = &[
    "--import",
    "--require",
    "-r",
    "--loader",
    "--experimental-loader",
];

/// Valueless node CLI flags that are swallowed.
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

/// Translate a node invocation into the `pir js` argv. Only leading flags
/// (before the script) are touched.
fn route_node_argv(argv: &[String]) -> Vec<String> {
    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();
        if arg == "--version" || arg == "-v" {
            return vec!["--version".to_owned()];
        }
        if arg == "--" {
            index += 1;
            break;
        }
        if arg == "-e" || arg == "--eval" || arg.starts_with("--eval=") || arg == "-" {
            break;
        }
        if NODE_VALUE_FLAGS.contains(&arg) {
            index += 2;
            continue;
        }
        if NODE_VALUELESS_FLAGS.contains(&arg) || (arg.starts_with('-') && arg.len() > 1) {
            index += 1;
            continue;
        }
        break;
    }
    argv[index.min(argv.len())..].to_vec()
}

/// The running image — the program a routed node spawn is handed. `None`
/// leaves the command untouched, so a native node lookup is never broken.
fn shim_program() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
}

/// Rewrite a shell command line whose leading command is a bare node-family
/// program. Only the leading token changes.
fn rewrite_shell_node_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let head_end = trimmed
        .find(|character: char| character.is_whitespace())
        .unwrap_or(trimmed.len());
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

/// One spawn's resolved program + arguments, plus whether the shim rewrote it.
struct Launch<'a> {
    program: String,
    args: LaunchArgs<'a>,
    shimmed: bool,
}

/// How one launch's arguments reach `Command`.
enum LaunchArgs<'a> {
    Plain(Cow<'a, [String]>),
    Shell { options: Vec<String>, line: String },
}

/// `cmd` + args joined with single spaces — the line a `shell: true` spawn
/// hands to the platform shell.
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

/// Resolve program + argv, applying the platform shell wrapper when `shell`
/// is set, and run the node executable shim on bare node-family programs.
fn command_line<'a>(
    cmd: &str,
    args: &'a [String],
    shell: Option<&JsonValue>,
) -> Result<Launch<'a>, NodeError> {
    let resolved = match shell {
        None | Some(JsonValue::Null) | Some(JsonValue::Bool(false)) => None,
        Some(JsonValue::Bool(true)) => Some(default_shell()),
        Some(JsonValue::String(shell)) if !shell.is_empty() => Some(shell.clone()),
        Some(_) => return Err(NodeError::new("EINVAL", "invalid `shell` option")),
    };
    match resolved {
        None => {
            if is_node_program(cmd) {
                if let Some(exe) = shim_program() {
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
    runtime: &NodeRuntime,
    launch: &Launch<'_>,
    payload: &JsonValue,
) -> Result<Command, NodeError> {
    let mut command = Command::new(&launch.program);
    apply_launch_args(&mut command, launch);
    if let Some(cwd) = payload.get("cwd").and_then(JsonValue::as_str) {
        let base = runtime.context.cwd();
        let path = if Path::new(cwd).is_absolute() {
            PathBuf::from(cwd)
        } else {
            base.join(cwd)
        };
        if !cwd_allowed(runtime, &path) {
            return Err(NodeError::new(
                "EACCES",
                format!("EACCES: permission denied, spawn cwd '{cwd}'"),
            ));
        }
        command.current_dir(path);
    } else {
        // Node's child_process inherits process.cwd(), which for an extension
        // is the runtime's configured cwd — not the host pir process cwd.
        command.current_dir(runtime.context.cwd());
    }
    if let Some(env) = payload.get("env").and_then(JsonValue::as_object) {
        // Node semantics: a provided `env` replaces the child's environment
        // wholesale (string values only).
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

/// One spawn's resolved standard streams: the `Stdio` handed to
/// `Command::spawn` plus the labels echoed back to the JS side.
struct ChildStdio {
    slots: [Stdio; 3],
    kinds: [&'static str; 3],
}

impl ChildStdio {
    /// Node's default: three host pipes.
    fn all_pipes() -> Self {
        Self {
            slots: [Stdio::piped(), Stdio::piped(), Stdio::piped()],
            kinds: ["pipe", "pipe", "pipe"],
        }
    }
}

/// Resolve the payload's `stdio` array (`[stdin, stdout, stderr]`). A missing
/// entry keeps its `"pipe"` default; a malformed one is `EINVAL`.
fn resolve_stdio(payload: &JsonValue, runtime: &NodeRuntime) -> Result<ChildStdio, NodeError> {
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
                stdio.slots[index] = stdio_handle_from_fd(fd, runtime)?;
                stdio.kinds[index] = "fd";
            }
            other => {
                return Err(NodeError::new(
                    "EINVAL",
                    format!("invalid stdio entry {}", other),
                ))
            }
        }
    }
    Ok(stdio)
}

fn bad_fd_error(fd: impl std::fmt::Display) -> NodeError {
    NodeError::new("EBADF", format!("EBADF: bad file descriptor, fd {fd}"))
}

/// A numeric `stdio` entry. This crate has no view of the host's
/// `fs.openSync` descriptor table, so no number can be duplicated for a child;
/// Node reports `EBADF` for a descriptor it cannot hand over.
fn stdio_handle_from_fd(fd: i64, runtime: &NodeRuntime) -> Result<Stdio, NodeError> {
    let fd = i32::try_from(fd).map_err(|_| bad_fd_error(fd))?;
    // Duplicate the caller's fs.openSync descriptor for the child: cloning the
    // File yields a second inheritable handle, and Stdio::from(File) wires it to
    // the child's std stream (Std handles inheritance without raw Win32 calls).
    let cloned = {
        let mut table = runtime
            .fd_table
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        table
            .get_mut(fd)
            .and_then(|entry| entry.file.try_clone().ok())
    };
    match cloned {
        Some(file) => Ok(Stdio::from(file)),
        None => Err(bad_fd_error(fd as i64)),
    }
}

/// Terminate step shared by `spawnKill` and `spawnSync`. With no `libc`
/// dependency this uses `Child::kill` on both platforms (SIGKILL on Unix,
/// TerminateProcess on Windows); the graceful SIGTERM-first step of the
/// pir-side path is therefore unavailable here.
fn send_term(child: &mut Child) {
    let _ = child.kill();
}

// ---------------------------------------------------------------------------
// child: ops
// ---------------------------------------------------------------------------

/// `child_process` host op table. `generation` scopes the process registry for
/// runtime teardown/reload release.
pub fn child_op(op: &str, payload: &JsonValue, runtime: &NodeRuntime) -> NodeResult {
    match op {
        "spawnStart" => spawn_start(payload, runtime),
        "spawnPoll" => spawn_poll(payload),
        "spawnWrite" => spawn_write(payload),
        "spawnEnd" => spawn_end(payload),
        "spawnKill" => spawn_kill(payload),
        "spawnSync" => spawn_sync(payload, runtime),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown child op `{other}`"),
        )),
    }
}

fn spawn_start(payload: &JsonValue, runtime: &NodeRuntime) -> NodeResult {
    if lock(children()).len() >= MAX_CHILDREN {
        return Err(NodeError::new(
            "EAGAIN",
            format!("too many live child processes (max {MAX_CHILDREN})"),
        ));
    }
    let cmd = payload_str(payload, "cmd")?;
    let args = payload_strings(payload, "args");
    let launch = command_line(cmd, &args, payload.get("shell"))?;
    let mut command = build_command(runtime, &launch, payload)?;
    if launch.shimmed {
        command.env(NODE_SHIM_ENV, "1");
    }
    let stdio = resolve_stdio(payload, runtime)?;
    let kinds = stdio.kinds;
    let [stdin_slot, stdout_slot, stderr_slot] = stdio.slots;
    command
        .stdin(stdin_slot)
        .stdout(stdout_slot)
        .stderr(stderr_slot);
    let mut child = command
        .spawn()
        .map_err(|error| spawn_failure(&error, cmd))?;

    let id = NEXT_CHILD_ID.fetch_add(1, Ordering::SeqCst);
    let diag = diag_callback(runtime);
    let stdout_sink = StreamSink::new(
        format!("{id} stdout"),
        SinkMode::Backpressure,
        Arc::clone(&diag),
    );
    let stderr_sink = StreamSink::new(
        format!("{id} stderr"),
        SinkMode::Backpressure,
        Arc::clone(&diag),
    );
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
        diag,
    );

    lock(children()).insert(
        id,
        Arc::new(ChildHandle {
            child,
            stdin: StdMutex::new(stdin),
            stdout: stdout_sink,
            stderr: stderr_sink,
            status,
            activity,
            generation: runtime.context.generation(),
        }),
    );
    Ok(json!({ "id": id, "pid": pid, "stdio": kinds }))
}

fn spawn_poll(payload: &JsonValue) -> NodeResult {
    let id = payload_id(payload)?;
    let handle = child_by_id(id)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(MAX_POLL_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
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
        // A reader was parked on a FULL window; take_base64 just released it.
        // Give the readers one more bounded window so the final sample carries
        // their tail, folded into the same base64 payload.
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
        let (tail, tail_dropped) = handle.stdout.take_base64();
        stdout = append_base64_chunk(stdout, tail);
        stdout_dropped |= tail_dropped;
        let (tail, tail_dropped) = handle.stderr.take_base64();
        stderr = append_base64_chunk(stderr, tail);
        stderr_dropped |= tail_dropped;
    }
    // An exited child is final after the bounded drain windows, even when a
    // detached descendant still holds an inherited pipe handle.
    let done = exit.is_some();
    if done {
        lock(children()).remove(&id);
    }
    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "truncated": stdout_dropped || stderr_dropped,
        "exited": exit.is_some(),
        "exitCode": exit.and_then(|exit| exit.code),
        "signal": signal_name(exit.and_then(|exit| exit.signal)),
        "done": done,
    }))
}

fn spawn_write(payload: &JsonValue) -> NodeResult {
    let id = payload_id(payload)?;
    let handle = child_by_id(id)?;
    let data = payload_str(payload, "data")?;
    let bytes = STANDARD
        .decode(data.as_bytes())
        .map_err(|error| NodeError::new("EINVAL", format!("invalid base64 payload: {error}")))?;
    let mut guard = lock(&handle.stdin);
    let Some(stdin) = guard.as_mut() else {
        return Err(NodeError::new(
            "EPIPE",
            format!("EPIPE: child {id} stdin is closed"),
        ));
    };
    stdin
        .write_all(&bytes)
        .and_then(|()| stdin.flush())
        .map_err(|error| {
            NodeError::new(
                "EPIPE",
                format!("EPIPE: write to child {id} failed: {error}"),
            )
        })?;
    Ok(JsonValue::Bool(true))
}

fn spawn_end(payload: &JsonValue) -> NodeResult {
    let id = payload_id(payload)?;
    let handle = child_by_id(id)?;
    // Dropping the handle closes the pipe: the child sees stdin EOF.
    drop(lock(&handle.stdin).take());
    Ok(JsonValue::Bool(true))
}

fn spawn_kill(payload: &JsonValue) -> NodeResult {
    let id = payload_id(payload)?;
    let handle = child_by_id(id)?;
    let mut child = lock(&handle.child);
    send_term(&mut child);
    Ok(JsonValue::Bool(true))
}

/// Blocking `spawnSync`: the stdin writer, both pipe readers and the wait all
/// run on separate threads, so a child that fills a pipe before reading its
/// input cannot deadlock the call.
fn spawn_sync(payload: &JsonValue, runtime: &NodeRuntime) -> NodeResult {
    let cmd = payload_str(payload, "cmd")?;
    let args = payload_strings(payload, "args");
    let launch = command_line(cmd, &args, payload.get("shell"))?;
    let input = payload
        .get("input")
        .and_then(JsonValue::as_str)
        .map(|data| {
            STANDARD
                .decode(data.as_bytes())
                .map_err(|error| NodeError::new("EINVAL", format!("invalid base64 input: {error}")))
        })
        .transpose()?;
    let timeout = payload
        .get("timeout")
        .and_then(JsonValue::as_u64)
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis);
    let max_buffer = payload
        .get("maxBuffer")
        .and_then(JsonValue::as_u64)
        .filter(|bytes| *bytes > 0)
        .map(|bytes| usize::try_from(bytes).unwrap_or(usize::MAX));
    let signal_requested = payload_bool(payload, "signal");
    // Node reports an already-aborted signal in-band (never a throw from
    // spawnSync itself).
    if signal_requested && (payload_bool(payload, "signalAborted") || runtime.context.is_aborted())
    {
        return Ok(json!({
            "pid": 0,
            "status": JsonValue::Null,
            "signal": JsonValue::Null,
            "stdout": JsonValue::Null,
            "stderr": JsonValue::Null,
            "truncated": false,
            "error": {
                "code": "ABORT_ERR",
                "message": "The operation was aborted",
            },
        }));
    }

    let mut command = build_command(runtime, &launch, payload)?;
    if launch.shimmed {
        command.env(NODE_SHIM_ENV, "1");
    }
    let stdio = resolve_stdio(payload, runtime)?;
    let kinds = stdio.kinds;
    // Node feeds `input` only through a piped stdin.
    let input = if kinds[0] == "pipe" { input } else { None };
    let [stdin_slot, stdout_slot, stderr_slot] = stdio.slots;
    command
        .stdin(stdin_slot)
        .stdout(stdout_slot)
        .stderr(stderr_slot);
    let mut child = command
        .spawn()
        .map_err(|error| spawn_failure(&error, cmd))?;
    let pid = child.id();

    let max_buffer_explicit = max_buffer.is_some();
    // Node's spawnSync maxBuffer defaults to 1 MiB. It is an accounting limit
    // (the result still carries every captured byte), so the capture cap stays
    // the hard stream bound.
    let max_buffer = max_buffer.unwrap_or(1024 * 1024);
    let diag = diag_callback(runtime);
    let stdout_sink = StreamSink::with_overflow_limit(
        format!("spawnSync {pid} stdout"),
        SinkMode::Truncate,
        MAX_STREAM_BYTES,
        Some(max_buffer),
        Arc::clone(&diag),
    );
    let stderr_sink = StreamSink::with_overflow_limit(
        format!("spawnSync {pid} stderr"),
        SinkMode::Truncate,
        MAX_STREAM_BYTES,
        Some(max_buffer),
        diag,
    );
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
    }

    // Deadline / abort / maxBuffer enforcement: poll try_wait so the host can
    // kill the child on the first of the three conditions.
    let timeout_at = timeout.map(|timeout| Instant::now() + timeout);
    let mut forced_failure: Option<(&'static str, String)> = None;
    let status = 'wait: loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            NodeError::new("EIO", format!("waiting for `{cmd}` failed: {error}"))
        })? {
            break status;
        }
        let aborted = signal_requested && runtime.context.is_aborted();
        let overflow = stdout_sink.overflowed() || stderr_sink.overflowed();
        let timed_out = timeout_at.is_some_and(|deadline| Instant::now() >= deadline);
        if aborted || overflow || timed_out {
            forced_failure = Some(if aborted {
                ("ABORT_ERR", "The operation was aborted".to_owned())
            } else if overflow {
                (
                    "ENOBUFS",
                    format!("spawnSync {cmd} ENOBUFS: maxBuffer ({max_buffer} bytes) exceeded"),
                )
            } else {
                (
                    "ETIMEDOUT",
                    format!("spawnSync {cmd} ETIMEDOUT: timed out after {timeout:?}"),
                )
            });
            send_term(&mut child);
            let kill_at = Instant::now() + Duration::from_millis(250);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break 'wait status,
                    Ok(None) => {}
                    Err(error) => {
                        return Err(NodeError::new(
                            "EIO",
                            format!("waiting for `{cmd}` failed: {error}"),
                        ))
                    }
                }
                if Instant::now() >= kill_at {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            break 'wait child.wait().map_err(|error| {
                NodeError::new("EIO", format!("waiting for `{cmd}` failed: {error}"))
            })?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Post-exit quiet window (see EXIT_STDIO_GRACE): a descendant that
    // inherited the pipes must not hang the call. The window re-arms on every
    // chunk either reader captures.
    let exit_at = Instant::now();
    loop {
        let seen = activity.epoch();
        if stdout_sink.eof() && stderr_sink.eof() {
            break;
        }
        let last_data = stdout_sink.last_data().max(stderr_sink.last_data());
        let remaining = sync_drain_wait_slice(exit_at, last_data, Instant::now());
        if remaining.is_zero() {
            break;
        }
        let _ = activity.wait_since(seen, remaining);
    }
    // A fast child can exit before the wait loop observes the overflow.
    if forced_failure.is_none()
        && max_buffer_explicit
        && (stdout_sink.overflowed() || stderr_sink.overflowed())
    {
        forced_failure = Some((
            "ENOBUFS",
            format!("spawnSync {cmd} ENOBUFS: maxBuffer ({max_buffer} bytes) exceeded"),
        ));
    }
    let exit = exit_info(&status);
    let (stdout, stdout_dropped) = stdout_sink.take_base64();
    let (stderr, stderr_dropped) = stderr_sink.take_base64();
    let forced = forced_failure.is_some();
    let error = forced_failure.map(|(code, message)| {
        json!({
            "code": code,
            "message": message,
        })
    });
    Ok(json!({
        "pid": pid,
        // Node reports a null status for a killed blocking child.
        "status": if forced {
            JsonValue::Null
        } else {
            serde_json::to_value(exit.code).unwrap_or(JsonValue::Null)
        },
        "signal": signal_name(exit.signal),
        "stdout": captured_slot(kinds[1], stdout),
        "stderr": captured_slot(kinds[2], stderr),
        "truncated": stdout_dropped || stderr_dropped,
        "error": error,
    }))
}

/// One `spawnSync` result slot: base64 text for a piped slot, `null` otherwise.
fn captured_slot(kind: &str, base64: String) -> JsonValue {
    if kind == "pipe" {
        JsonValue::String(base64)
    } else {
        JsonValue::Null
    }
}

/// Append a later base64 chunk to an earlier one as ONE base64 payload of the
/// concatenated bytes (two base64 strings cannot simply be concatenated).
fn append_base64_chunk(first: String, second: String) -> String {
    if second.is_empty() {
        return first;
    }
    let (Ok(first_bytes), Ok(second_bytes)) = (
        STANDARD.decode(first.as_bytes()),
        STANDARD.decode(second.as_bytes()),
    ) else {
        return first;
    };
    let mut bytes = first_bytes;
    bytes.extend_from_slice(&second_bytes);
    STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// zlib
// ---------------------------------------------------------------------------

/// `gzip`/`gunzip`/`deflate`/`inflate` over flate2 (the `zlib-rs` backend),
/// `brotliCompress`/`brotliDecompress` over the `brotli` crate and
/// `zstdCompress`/`zstdDecompress` over `zstd`; base64 in/out.
///
/// One-shot codecs: every op runs a single `Compress`/`Decompress` (or the
/// GzBuilder writer) over the whole payload into a pre-sized `Vec`, instead of
/// the `read::*Encoder` adapters' per-call `BufReader` + growth copies. Node's
/// convenience methods are one-shot too, so this is the shape the benchmark
/// compares against.
///
/// Option surface (Node's convenience-method defaults):
/// * `level` — `Z_DEFAULT_COMPRESSION` (-1) maps to zlib's level 6; 0..=9 pass
///   through. `memLevel`/`strategy` are validated JS-side but flate2 exposes
///   no knob for them, so non-default values are accepted and ignored (legal:
///   Node does not promise a particular compressed byte stream).
/// * `windowBits` — 8..=15 (8 is zlib's windowBits spelling of 9).
/// * `dictionary` — base64; applied before the first block for deflate/
///   deflateRaw and before inflate/inflateRaw, matching zlib's DICTID flow.
///   gzip has no dictionary slot, so (like Node) it is ignored there.
/// * `maxOutputLength` — Node's hard cap on the result buffer. Exceeding it
///   is `ERR_BUFFER_TOO_LARGE`; the internal 32 MiB decompression budget
///   still applies when the caller passes no cap.
///
/// Encoder knobs: brotli `quality` (0..=11, default 11) and `lgwin`
/// (10..=24, default 22); zstd `level` (default 3).
pub fn zlib_op(op: &str, payload: &JsonValue) -> NodeResult {
    // Streaming decoder sessions (stateful inflate/inflateRaw/brotli): the
    // one-shot path below is stateless, while a decompression stream must stop
    // at the readable high-water mark and resume (`zlib_stream.rs`).
    match op {
        "zlibStreamOpen" => return zlib_stream::open(payload),
        "zlibStreamPush" => return zlib_stream::push(payload),
        "zlibStreamDrain" => return zlib_stream::drain(payload),
        "zlibStreamFinish" => return zlib_stream::finish(payload),
        "zlibStreamClose" => return zlib_stream::close(payload),
        _ => {}
    }
    let data = STANDARD
        .decode(payload_str(payload, "data")?.as_bytes())
        .map_err(|error| NodeError::new("EINVAL", format!("invalid base64 payload: {error}")))?;
    let dictionary = zlib_dictionary(payload)?;
    let max_output = zlib_max_output(payload);
    // Decompression streams report `bytesWritten` as the input bytes the codec
    // consumed (Node's zlib total_in), not trailing bytes after the stream's
    // end. `consumed` asks for that count alongside the payload.
    let wants_consumed = payload_bool(payload, "consumed");
    let (out, consumed) = match op {
        "brotliCompress" => (
            zlib_brotli_compress(&data, payload, dictionary.as_deref(), max_output)?,
            data.len(),
        ),
        "brotliDecompress" => {
            if payload_bool(payload, "rejectGarbageAfterEnd") {
                // The stream decoder below checks the consumed prefix.
            }
            zlib_brotli_decompress(
                &data,
                dictionary.as_deref(),
                max_output,
                payload_bool(payload, "rejectGarbageAfterEnd"),
            )?
        }
        "zstdCompress" => (
            zlib_zstd_compress(&data, payload, dictionary.as_deref(), max_output)?,
            data.len(),
        ),
        "zstdDecompress" => (
            zlib_zstd_decompress(
                &data,
                dictionary.as_deref(),
                max_output,
                payload_i32(payload, "finishFlush"),
                payload.get("params"),
                payload_bool(payload, "rejectGarbageAfterEnd"),
            )?,
            data.len(),
        ),
        "inflate" | "inflateSync" => zlib_inflate(
            &data,
            true,
            zlib_window_bits(payload_i32(payload, "windowBits")),
            dictionary.as_deref(),
            max_output,
            payload_i32(payload, "finishFlush"),
            payload_bool(payload, "rejectGarbageAfterEnd"),
        )?,
        "inflateRaw" | "inflateRawSync" => zlib_inflate(
            &data,
            false,
            zlib_window_bits(payload_i32(payload, "windowBits")),
            dictionary.as_deref(),
            max_output,
            payload_i32(payload, "finishFlush"),
            payload_bool(payload, "rejectGarbageAfterEnd"),
        )?,
        other => (
            zlib_bytes_op(
                other,
                &data,
                &ZlibBytesOptions {
                    level: payload_i32(payload, "level"),
                    window_bits: payload_i32(payload, "windowBits"),
                    dictionary: dictionary.as_deref(),
                    max_output_length: max_output,
                    finish_flush: payload_i32(payload, "finishFlush"),
                    reject_garbage: payload_bool(payload, "rejectGarbageAfterEnd"),
                },
            )?,
            data.len(),
        ),
    };
    if wants_consumed {
        let reply = serde_json::json!({
            "data": STANDARD.encode(&out),
            "consumed": consumed,
        });
        return Ok(JsonValue::String(reply.to_string()));
    }
    Ok(JsonValue::String(STANDARD.encode(out)))
}

/// Options for the byte-level codec entry (`__pirZlib`): the same knobs the
/// JSON envelope reads, handed over without the base64/String lap.
#[derive(Default)]
pub struct ZlibBytesOptions<'a> {
    pub level: Option<i32>,
    pub window_bits: Option<i32>,
    pub dictionary: Option<&'a [u8]>,
    pub max_output_length: Option<f64>,
    /// Node's `finishFlush`: `4` (`Z_FINISH`) errors on a truncated stream;
    /// any other flush returns the partial output instead.
    pub finish_flush: Option<i32>,
    pub reject_garbage: bool,
}

/// One-shot codec over raw bytes — the typed C-ABI path (`bytes_ops.rs`'s
/// `__pirZlib`). `op` accepts both the host-table spellings (`deflate`,
/// `gzip`, `deflateRaw`, …) and the JS `*Sync` spellings; brotli/zstd stay on
/// the JSON envelope (their option plumbing is richer than this tier serves).
pub fn zlib_bytes_op(op: &str, data: &[u8], options: &ZlibBytesOptions<'_>) -> NodeResult<Vec<u8>> {
    let level = zlib_compression(options.level);
    let window_bits = zlib_window_bits(options.window_bits);
    let dictionary = options.dictionary;
    let max_output = options.max_output_length;
    match op {
        "gzip" | "gzipSync" => zlib_gzip_compress(data, level, max_output),
        "gunzip" | "gunzipSync" => zlib_gunzip(
            data,
            max_output,
            options.finish_flush,
            options.reject_garbage,
        ),
        "deflate" | "deflateSync" => {
            zlib_compress(data, level, window_bits, true, dictionary, max_output)
        }
        "inflate" | "inflateSync" => zlib_inflate(
            data,
            true,
            window_bits,
            dictionary,
            max_output,
            options.finish_flush,
            options.reject_garbage,
        )
        .map(|(out, _consumed)| out),
        "deflateRaw" | "deflateRawSync" => {
            zlib_compress(data, level, window_bits, false, dictionary, max_output)
        }
        "inflateRaw" | "inflateRawSync" => zlib_inflate(
            data,
            false,
            window_bits,
            dictionary,
            max_output,
            options.finish_flush,
            options.reject_garbage,
        )
        .map(|(out, _consumed)| out),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown zlib op `{other}`"),
        )),
    }
}

/// gzip header bytes: mtime 0 (Node's default) and OS 255 (the portable
/// "unknown" value flate2/zlib-rs emit).
fn zlib_gzip_compress(
    data: &[u8],
    level: flate2::Compression,
    max_output: Option<f64>,
) -> NodeResult<Vec<u8>> {
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(Vec::with_capacity(zlib_compress_hint(data.len())), level);
    encoder
        .write_all(data)
        .map_err(|error| NodeError::new("EIO", format!("compression failed: {error}")))?;
    let out = encoder
        .finish()
        .map_err(|error| NodeError::new("EIO", format!("compression failed: {error}")))?;
    zlib_check_output_limit(out, max_output)
}

/// Largest output one decompression call may materialize when the caller did
/// not pass `maxOutputLength` (32 MiB; a decompression bomb must not allocate
/// without bound on the host heap).
const MAX_DECOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

/// zlib's `Z_DEFAULT_COMPRESSION` maps to level 6 (what Node uses when
/// `options.level` is absent and what C zlib stores as the default).
const ZLIB_DEFAULT_LEVEL: u32 = 6;

/// The zlib/gzip wrapper's default `windowBits` (RFC 1950 window size 32 KiB).
const ZLIB_DEFAULT_WINDOW_BITS: i32 = 15;

fn zlib_compression(level: Option<i32>) -> flate2::Compression {
    match level {
        Some(level) if level >= 0 => flate2::Compression::new(level as u32),
        _ => flate2::Compression::new(ZLIB_DEFAULT_LEVEL),
    }
}

fn zlib_window_bits(bits: Option<i32>) -> u8 {
    let bits = bits.unwrap_or(ZLIB_DEFAULT_WINDOW_BITS);
    // zlib reads windowBits 8 as 9 on the inflate side, and flate2's
    // `*_with_window_bits` constructors require 9..=15 (the JS validator
    // already rejects anything outside 8..=15).
    bits.clamp(9, ZLIB_DEFAULT_WINDOW_BITS) as u8
}

fn zlib_dictionary(payload: &JsonValue) -> NodeResult<Option<Vec<u8>>> {
    match payload.get("dictionary") {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(text)) => {
            STANDARD.decode(text.as_bytes()).map(Some).map_err(|error| {
                NodeError::new("EINVAL", format!("invalid base64 dictionary: {error}"))
            })
        }
        Some(_) => Err(NodeError::new("EINVAL", "invalid `dictionary` payload")),
    }
}

fn zlib_max_output(payload: &JsonValue) -> Option<f64> {
    payload
        .get("maxOutputLength")
        .and_then(JsonValue::as_f64)
        .filter(|limit| !limit.is_nan())
}

/// The effective output cap: the caller's `maxOutputLength` (floored like
/// Node's Buffer sizing) or the internal decompression budget.
fn zlib_output_limit(max_output: Option<f64>) -> u64 {
    match max_output {
        Some(limit) if limit.is_finite() && limit > 0.0 => limit.floor() as u64,
        _ => MAX_DECOMPRESSED_BYTES,
    }
}

fn zlib_limit_error(max_output: Option<f64>) -> NodeError {
    match max_output {
        Some(limit) => NodeError::new(
            "ERR_BUFFER_TOO_LARGE",
            format!("Cannot create a Buffer larger than {limit} bytes"),
        ),
        None => NodeError::new(
            "Z_BUF_ERROR",
            format!(
                "Z_BUF_ERROR: decompressed output exceeds the {}-MiB limit",
                MAX_DECOMPRESSED_BYTES / (1024 * 1024)
            ),
        ),
    }
}

/// Compression output hint: zlib's worst case is `len + len/1000 + 12`, so
/// `len + len/8 + 64` always fits without a reallocation.
fn zlib_compress_hint(len: usize) -> usize {
    len.saturating_add(len / 8).saturating_add(64)
}

/// Decompression output hint. Node's engine starts from `chunkSize` (16 KiB
/// default) and grows; 4x the compressed size covers the common payloads in
/// one allocation, and 16 KiB handles tiny streams (the benchmark's 28-byte
/// deflate of 4 KiB included).
fn zlib_inflate_hint(len: usize, limit: u64) -> usize {
    let hint = len.saturating_mul(4).max(16 * 1024) as u64;
    hint.min(limit).min(usize::MAX as u64) as usize
}

fn zlib_check_output_limit(out: Vec<u8>, max_output: Option<f64>) -> Result<Vec<u8>, NodeError> {
    if let Some(max_output) = max_output {
        let limit = zlib_output_limit(Some(max_output));
        if out.len() as u64 > limit {
            return Err(zlib_limit_error(Some(max_output)));
        }
    }
    Ok(out)
}

/// One idle deflate state per host thread. Creating a compressor allocates
/// zlib's ~256 KiB window/hash tables (`deflateInit2`): ~3 µs on a fresh
/// heap, but tens of microseconds once the process heap is fragmented (a
/// `zlib.deflateSync` corpus run after the JSON/fs/crypto workloads measured
/// 100µs+ for what is otherwise a 4 µs compression). `deflateReset` keeps the
/// parameters and gives byte-identical output, so one pooled state per
/// (level, windowBits, wrapper) triple serves every call on that thread.
struct ZlibCompressorPool {
    level: u32,
    zlib_header: bool,
    window_bits: u8,
    compressor: flate2::Compress,
}

thread_local! {
    static ZLIB_COMPRESSOR: std::cell::RefCell<Option<ZlibCompressorPool>> =
        const { std::cell::RefCell::new(None) };
}

/// One-shot deflate family. `zlib_header` selects the RFC 1950 wrapper
/// (`deflate`) or raw RFC 1951 blocks (`deflateRaw`).
fn zlib_compress(
    data: &[u8],
    level: flate2::Compression,
    window_bits: u8,
    zlib_header: bool,
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
) -> Result<Vec<u8>, NodeError> {
    let level_id = level.level();
    let mut encoder = ZLIB_COMPRESSOR.with(|slot| match slot.borrow_mut().take() {
        Some(mut pooled)
            if pooled.level == level_id
                && pooled.zlib_header == zlib_header
                && pooled.window_bits == window_bits =>
        {
            pooled.compressor.reset();
            pooled.compressor
        }
        _ => flate2::Compress::new_with_window_bits(level, zlib_header, window_bits),
    });
    let outcome = zlib_compress_into(&mut encoder, data, dictionary, max_output);
    // Return the state even on failure: `reset()` at the next call restores a
    // clean stream, and a failed call must not cost the next one its table.
    ZLIB_COMPRESSOR.with(|slot| {
        *slot.borrow_mut() = Some(ZlibCompressorPool {
            level: level_id,
            zlib_header,
            window_bits,
            compressor: encoder,
        });
    });
    outcome
}

fn zlib_compress_into(
    encoder: &mut flate2::Compress,
    data: &[u8],
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
) -> Result<Vec<u8>, NodeError> {
    if let Some(dictionary) = dictionary {
        encoder
            .set_dictionary(dictionary)
            .map_err(|error| NodeError::new("EIO", format!("compression failed: {error}")))?;
    }
    let limit = zlib_output_limit(max_output);
    let mut out = Vec::with_capacity(
        zlib_compress_hint(data.len())
            .min(usize::try_from(limit.saturating_add(1)).unwrap_or(usize::MAX)),
    );
    loop {
        let consumed = usize::try_from(encoder.total_in()).unwrap_or(usize::MAX);
        let status = encoder
            .compress_vec(
                &data[consumed.min(data.len())..],
                &mut out,
                flate2::FlushCompress::Finish,
            )
            .map_err(|error| NodeError::new("EIO", format!("compression failed: {error}")))?;
        if status == flate2::Status::StreamEnd {
            break;
        }
        if out.len() as u64 > limit {
            return Err(zlib_limit_error(max_output));
        }
        let capacity = out.capacity() as u64;
        let grown = capacity
            .saturating_mul(2)
            .min(limit.saturating_add(1))
            .max(capacity.saturating_add(64));
        out.reserve(
            usize::try_from(grown.saturating_sub(out.len() as u64))
                .unwrap_or(1)
                .max(1),
        );
    }
    if out.len() as u64 > limit {
        return Err(zlib_limit_error(max_output));
    }
    Ok(out)
}

/// One-shot zlib-wrapped or raw inflate. Returns the output plus the number
/// of input bytes the decoder consumed (zlib's `total_in`).
fn zlib_inflate(
    data: &[u8],
    zlib_header: bool,
    window_bits: u8,
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
    finish_flush: Option<i32>,
    reject_garbage: bool,
) -> Result<(Vec<u8>, usize), NodeError> {
    let mut decoder = flate2::Decompress::new_with_window_bits(zlib_header, window_bits);
    let limit = zlib_output_limit(max_output);
    let mut out = Vec::with_capacity(zlib_inflate_hint(data.len(), limit));
    if let Some(dictionary) = dictionary {
        if zlib_header {
            // RFC 1950 carries the dictionary's Adler-32 in a DICTID field;
            // zlib-rs only accepts `set_dictionary` after the decoder has
            // seen that field and reported `needs_dictionary` (zlib's
            // inflateSetDictionary contract), so prime one call first. Its
            // output is real data when the stream turns out not to need a
            // dictionary, so it stays in `out`.
            match decoder.decompress_vec(data, &mut out, flate2::FlushDecompress::Finish) {
                Err(error) if error.needs_dictionary().is_some() => {
                    out.clear();
                    decoder
                        .set_dictionary(dictionary)
                        .map_err(|_| NodeError::new("Z_NEED_DICT", "Bad dictionary"))?;
                }
                Err(error) => return Err(zlib_decompress_error(&error)),
                Ok(_) => {}
            }
        } else {
            // Raw DEFLATE has no header, so the dictionary installs up front.
            decoder
                .set_dictionary(dictionary)
                .map_err(|error| zlib_decompress_error(&error))?;
        }
    }
    loop {
        let consumed = usize::try_from(decoder.total_in()).unwrap_or(usize::MAX);
        let before_out = out.len();
        let status = decoder
            .decompress_vec(
                &data[consumed.min(data.len())..],
                &mut out,
                flate2::FlushDecompress::Finish,
            )
            .map_err(|error| zlib_decompress_error(&error))?;
        if status == flate2::Status::StreamEnd {
            break;
        }
        // Node/zlib reports a truncated stream as Z_BUF_ERROR; with `Finish`
        // and every input byte consumed that is exactly what this status is.
        let all_input = usize::try_from(decoder.total_in()).unwrap_or(usize::MAX) >= data.len();
        if all_input && status == flate2::Status::BufError {
            // A non-finalizing `finishFlush` (anything but Z_FINISH=4) accepts
            // the truncated tail and returns what was decoded so far
            // (test-zlib-truncated's partial-flush leg).
            if finish_flush.unwrap_or(4) != 4 {
                break;
            }
            return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
        }
        if out.len() as u64 > limit {
            return Err(zlib_limit_error(max_output));
        }
        let consumed_before = consumed;
        if out.len() == before_out
            && usize::try_from(decoder.total_in()).unwrap_or(usize::MAX) == consumed_before
        {
            // No progress. A full output buffer needs room; with spare
            // capacity the stream is one zlib-rs cannot advance.
            if out.len() == out.capacity() {
                out.reserve(64);
                continue;
            }
            return Err(NodeError::new("Z_DATA_ERROR", "invalid input data"));
        }
        // One byte past the cap lets a stream that ends exactly at the limit
        // report `StreamEnd` instead of looking truncated.
        let capacity = out.capacity() as u64;
        let grown = capacity
            .saturating_mul(2)
            .min(limit.saturating_add(1))
            .max(capacity.saturating_add(64));
        out.reserve(
            usize::try_from(grown.saturating_sub(out.len() as u64))
                .unwrap_or(1)
                .max(1),
        );
    }
    if out.len() as u64 > limit {
        return Err(zlib_limit_error(max_output));
    }
    // `total_in` is the input the decoder consumed; bytes after the stream's
    // end stay unconsumed (the stream `bytesWritten` contract).
    let consumed = usize::try_from(decoder.total_in())
        .unwrap_or(data.len())
        .min(data.len());
    if reject_garbage && consumed < data.len() {
        return Err(NodeError::new(
            "ERR_TRAILING_JUNK_AFTER_STREAM_END",
            "Trailing garbage after stream end",
        ));
    }
    Ok((out, consumed))
}

/// One-shot gzip inflate. `MultiGzDecoder` matches Node's ability to read
/// concatenated members (`gunzipSync(gz("a") + gz("b")) === "ab"`).
fn zlib_gunzip(
    data: &[u8],
    max_output: Option<f64>,
    finish_flush: Option<i32>,
    reject_garbage: bool,
) -> Result<Vec<u8>, NodeError> {
    // zlib checks the 2-byte gzip magic as soon as it has both bytes, so a
    // short non-gzip input ("garbage") is "incorrect header check" rather
    // than flate2's header parser reporting a truncated header body.
    if data.len() >= 2 && (data[0] != 0x1f || data[1] != 0x8b) {
        return Err(NodeError::new("Z_DATA_ERROR", "incorrect header check"));
    }
    let limit = zlib_output_limit(max_output);
    let hint = zlib_inflate_hint(data.len(), limit);
    let mut out = Vec::with_capacity(hint);
    // Members are decoded one at a time (Node's `MultiGzDecoder` contract),
    // and the byte count the decoder consumed tells the loop where the next
    // member starts. Trailing bytes that do not begin with the gzip magic are
    // ignored (test-zlib-from-gzip-with-trailing-garbage); bytes that do are
    // parsed as a member and report their own header error.
    let mut consumed = 0usize;
    let mut first = true;
    while consumed < data.len() {
        let rest = &data[consumed..];
        if !first {
            if reject_garbage {
                return Err(NodeError::new(
                    "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                    "Trailing garbage after stream end",
                ));
            }
            if rest.len() < 2 || rest[0] != 0x1f || rest[1] != 0x8b {
                break;
            }
        }
        // zlib validates the 1-byte method before anything else: a member
        // whose third byte is not 8 is "unknown compression method" (the
        // trailing-garbage test's second case). flate2 words it "invalid gzip
        // header".
        if rest.len() >= 3 && rest[2] != 8 {
            return Err(NodeError::new("Z_DATA_ERROR", "unknown compression method"));
        }
        let mut cursor = std::io::Cursor::new(rest);
        let mut decoder = flate2::bufread::GzDecoder::new(&mut cursor);
        let read = decoder
            .by_ref()
            .take(limit.saturating_add(1).saturating_sub(out.len() as u64))
            .read_to_end(&mut out)
            .map_err(|error| {
                let message = zlib_io_error_text(&error);
                let code = if message == ZLIB_NEEDS_DICTIONARY {
                    "Z_NEED_DICT"
                } else if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    "Z_BUF_ERROR"
                } else {
                    "Z_DATA_ERROR"
                };
                NodeError::new(code, message)
            });
        let used = decoder.into_inner().position() as usize;
        if let Err(error) = read {
            if reject_garbage && consumed > 0 {
                return Err(NodeError::new(
                    "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                    "Trailing garbage after stream end",
                ));
            }
            // Non-finalizing flush: a truncated member returns the decoded
            // prefix instead of the Z_BUF_ERROR.
            if finish_flush.unwrap_or(4) != 4 && error.code == "Z_BUF_ERROR" && !out.is_empty() {
                break;
            }
            return Err(error);
        }
        if used == 0 {
            break;
        }
        consumed += used;
        first = false;
        if out.len() as u64 > limit {
            return Err(zlib_limit_error(max_output));
        }
    }
    if out.len() as u64 > limit {
        return Err(zlib_limit_error(max_output));
    }
    Ok(out)
}

/// Marker for the downcast path of a `DecompressError::needs_dictionary` io
/// error (gzip has no dictionary slot, so this is unreachable in practice).
const ZLIB_NEEDS_DICTIONARY: &str = "requires a dictionary";

fn zlib_decompress_error(error: &flate2::DecompressError) -> NodeError {
    if error.needs_dictionary().is_some() {
        // Node's zlib reports Z_NEED_DICT with "Missing dictionary".
        return NodeError::new("Z_NEED_DICT", "Missing dictionary");
    }
    let message = error
        .message()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string());
    NodeError::new("Z_DATA_ERROR", message)
}

/// zlib-rs' messages are zlib's own ("incorrect header check", "invalid block
/// type"), i.e. Node's wording. `io::Error` wraps them through the gzip
/// adapters, so unwrap the inner `DecompressError` when present.
fn zlib_io_error_text(error: &std::io::Error) -> String {
    if let Some(inner) = error.get_ref() {
        if let Some(decompress) = inner.downcast_ref::<flate2::DecompressError>() {
            if decompress.needs_dictionary().is_some() {
                return ZLIB_NEEDS_DICTIONARY.to_owned();
            }
            if let Some(message) = decompress.message() {
                return message.to_owned();
            }
        }
    }
    error.to_string()
}

/// One-shot Brotli encode with node's defaults for the absent knobs.
///
/// `options.dictionary` is brotli's *custom dictionary*: the encoder seeds its
/// window with those bytes (they never enter the stream), so only a decoder
/// holding the same bytes can reproduce the payload. `BrotliCompress` exposes no
/// dictionary knob, so the dict case drives the same encoder through the
/// custom-io entry point (the one `brotli`'s own CLI uses).
fn zlib_brotli_compress(
    data: &[u8],
    payload: &JsonValue,
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
) -> Result<Vec<u8>, NodeError> {
    let mut params = brotli::enc::BrotliEncoderParams {
        quality: payload_i32(payload, "quality").unwrap_or(11),
        lgwin: payload_i32(payload, "lgwin").unwrap_or(22),
        ..Default::default()
    };
    params.size_hint = data.len();
    let mut out = Vec::with_capacity(zlib_compress_hint(data.len()));
    match dictionary {
        Some(dictionary) if !dictionary.is_empty() => {
            let mut input_buffer = [0u8; 4096];
            let mut output_buffer = [0u8; 4096];
            // Brotli's metablock-hook signature; nothing is logged here.
            let mut nop = |_pm: &mut brotli::interface::PredictionModeContextMap<
                brotli::InputReferenceMut,
            >,
                           _cmds: &mut [brotli::interface::StaticCommand],
                           _mb: brotli::InputPair,
                           _alloc: &mut brotli::enc::StandardAlloc| {};
            brotli::BrotliCompressCustomIoCustomDict(
                &mut brotli::IoReaderWrapper(&mut &data[..]),
                &mut brotli::IoWriterWrapper(&mut out),
                &mut input_buffer,
                &mut output_buffer,
                &params,
                brotli::enc::StandardAlloc::default(),
                &mut nop,
                dictionary,
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Unexpected EOF"),
            )
            .map_err(|error| {
                NodeError::new("Z_BUF_ERROR", format!("brotli compression failed: {error}"))
            })?;
        }
        _ => {
            brotli::BrotliCompress(&mut &data[..], &mut out, &params).map_err(|error| {
                NodeError::new("Z_BUF_ERROR", format!("brotli compression failed: {error}"))
            })?;
        }
    }
    zlib_check_output_limit(out, max_output)
}

/// One-shot Brotli decode with the custom dictionary when one was supplied.
///
/// `brotli::Decompressor`'s io wrapper collapses "the input ended early" and
/// "the payload is corrupt" into one `Invalid Data` error and keeps the decoder
/// state private, so it cannot report *why* it failed. Node surfaces the
/// decoder's own `BROTLI_DECODER_ERROR_*` as `code: "ERR__<NAME>"` with
/// `errno` = the (negative) code, and a truncated stream as `Z_BUF_ERROR`
/// ("unexpected end of file"), so the stream API is driven directly here —
/// that is the only way to keep the code (needed by the no/wrong-dictionary
/// cases of test-zlib-brotli-dictionary).
fn zlib_brotli_decompress(
    data: &[u8],
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
    reject_garbage: bool,
) -> Result<(Vec<u8>, usize), NodeError> {
    use brotli::SliceWrapperMut as _;

    let limit = zlib_output_limit(max_output);
    let mut out = Vec::with_capacity(zlib_inflate_hint(data.len(), limit));
    let mut alloc_u8 = brotli::enc::StandardAlloc::default();
    let mut state = match dictionary {
        Some(dictionary) if !dictionary.is_empty() => {
            let mut cell = <brotli::enc::StandardAlloc as brotli::Allocator<u8>>::alloc_cell(
                &mut alloc_u8,
                dictionary.len(),
            );
            cell.slice_mut().copy_from_slice(dictionary);
            brotli::BrotliState::new_with_custom_dictionary(
                alloc_u8,
                brotli::enc::StandardAlloc::default(),
                brotli::enc::StandardAlloc::default(),
                cell,
            )
        }
        _ => brotli::BrotliState::new(
            alloc_u8,
            brotli::enc::StandardAlloc::default(),
            brotli::enc::StandardAlloc::default(),
        ),
    };

    let mut scratch = vec![0u8; 64 * 1024];
    let mut available_in = data.len();
    let mut input_offset = 0usize;
    let mut total_out = 0usize;
    loop {
        if out.len() as u64 > limit {
            return Err(zlib_limit_error(max_output));
        }
        let mut available_out = scratch.len();
        let mut output_offset = 0usize;
        let before_in = available_in;
        let result = brotli::BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            data,
            &mut available_out,
            &mut output_offset,
            &mut scratch[..],
            &mut total_out,
            &mut state,
        );
        out.extend_from_slice(&scratch[..output_offset]);
        match result {
            // Trailing bytes after a complete stream are ignored, like Node's
            // one-shot brotli decode.
            brotli::BrotliResult::ResultSuccess => {
                // The limit is enforced here too: the whole payload can land
                // in the first scratch window, before the loop head re-checks.
                if out.len() as u64 > limit {
                    return Err(zlib_limit_error(max_output));
                }
                // `available_in` is what the decoder did not consume: trailing
                // bytes after the stream's end. Mirror zlib's total_in so the
                // stream `bytesWritten` counts only the consumed prefix.
                let consumed = data.len().saturating_sub(available_in);
                if reject_garbage && consumed < data.len() {
                    return Err(NodeError::new(
                        "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                        "Trailing garbage after stream end",
                    ));
                }
                return Ok((out, consumed));
            }
            brotli::BrotliResult::NeedsMoreOutput => {
                // The decoder filled the scratch window; anything else is a
                // no-progress loop, which would only be a decoder bug.
                if output_offset == 0 {
                    return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
                }
                continue;
            }
            brotli::BrotliResult::NeedsMoreInput => {
                // No progress and either nothing left or a decoder that cannot
                // consume what is left: the stream ended before the frame did.
                if available_in != 0 && (output_offset != 0 || available_in != before_in) {
                    continue;
                }
                return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
            }
            brotli::BrotliResult::ResultFailure => {
                return Err(brotli_decoder_error(state.error_code as i32));
            }
        }
    }
}

/// Node's brotli error shape: `code` is `"ERR_" + BrotliDecoderErrorString(code)`
/// and the C string for the invalid-input family is `_ERROR_<NAME>` (leading
/// underscore), hence the double underscore; `errno` is the negative enum value.
/// The shim's `constants` table (brotli block at zlib_shim.js.txt:1273-1299)
/// carries the same numbers.
fn brotli_decoder_error(code: i32) -> NodeError {
    let name = match code {
        -1 => "ERROR_FORMAT_EXUBERANT_NIBBLE",
        -2 => "ERROR_FORMAT_RESERVED",
        -3 => "ERROR_FORMAT_EXUBERANT_META_NIBBLE",
        -4 => "ERROR_FORMAT_SIMPLE_HUFFMAN_ALPHABET",
        -5 => "ERROR_FORMAT_SIMPLE_HUFFMAN_SAME",
        -6 => "ERROR_FORMAT_CL_SPACE",
        -7 => "ERROR_FORMAT_HUFFMAN_SPACE",
        -8 => "ERROR_FORMAT_CONTEXT_MAP_REPEAT",
        -9 => "ERROR_FORMAT_BLOCK_LENGTH_1",
        -10 => "ERROR_FORMAT_BLOCK_LENGTH_2",
        -11 => "ERROR_FORMAT_TRANSFORM",
        -12 => "ERROR_FORMAT_DICTIONARY",
        -13 => "ERROR_FORMAT_WINDOW_BITS",
        -14 => "ERROR_FORMAT_PADDING_1",
        -15 => "ERROR_FORMAT_PADDING_2",
        -16 => "ERROR_FORMAT_DISTANCE",
        // -17 exists only in the newer C brotli Node v24 links (see §4); the
        // pinned crate cannot produce it, but keep the name so an upgraded
        // codec lands on the right string.
        -17 => "ERROR_FORMAT_BLOCK_SWITCH",
        -19 => "ERROR_DICTIONARY_NOT_SET",
        -20 => "ERROR_INVALID_ARGUMENTS",
        -21 => "ERROR_ALLOC_CONTEXT_MODES",
        -22 => "ERROR_ALLOC_TREE_GROUPS",
        -25 => "ERROR_ALLOC_CONTEXT_MAP",
        -26 => "ERROR_ALLOC_RING_BUFFER_1",
        -27 => "ERROR_ALLOC_RING_BUFFER_2",
        -30 => "ERROR_ALLOC_BLOCK_TYPE_TREES",
        _ => "ERROR_UNREACHABLE",
    };
    let mut error = NodeError::new(format!("ERR__{name}"), "Decompression failed");
    error.errno = Some(code);
    error
}

fn zlib_zstd_compress(
    data: &[u8],
    payload: &JsonValue,
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
) -> Result<Vec<u8>, NodeError> {
    let level = payload_i32(payload, "level").unwrap_or(3);
    let checksum = payload
        .get("params")
        .and_then(JsonValue::as_object)
        .and_then(|params| params.get("201"))
        .and_then(|value| value.as_i64())
        .map(|value| value != 0)
        .unwrap_or(false);
    let out = match dictionary {
        Some(dictionary) => {
            let mut encoder = zstd::stream::Encoder::with_dictionary(
                Vec::with_capacity(zlib_compress_hint(data.len())),
                level,
                dictionary,
            )
            .map_err(|error| {
                NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
            })?;
            if checksum {
                encoder.include_checksum(true).map_err(|error| {
                    NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
                })?;
            }
            encoder.write_all(data).map_err(|error| {
                NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
            })?;
            encoder.finish().map_err(|error| {
                NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
            })?
        }
        None => {
            if checksum {
                let mut encoder = zstd::stream::Encoder::new(
                    Vec::with_capacity(zlib_compress_hint(data.len())),
                    level,
                )
                .map_err(|error| {
                    NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
                })?;
                encoder.include_checksum(true).map_err(|error| {
                    NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
                })?;
                encoder.write_all(data).map_err(|error| {
                    NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
                })?;
                encoder.finish().map_err(|error| {
                    NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
                })?
            } else {
                zstd::stream::encode_all(data, level).map_err(|error| {
                    NodeError::new("Z_BUF_ERROR", format!("zstd compression failed: {error}"))
                })?
            }
        }
    };
    zlib_check_output_limit(out, max_output)
}

fn zlib_zstd_decompress(
    data: &[u8],
    dictionary: Option<&[u8]>,
    max_output: Option<f64>,
    finish_flush: Option<i32>,
    params: Option<&JsonValue>,
    reject_garbage: bool,
) -> Result<Vec<u8>, NodeError> {
    let limit = zlib_output_limit(max_output);
    let mut out = Vec::with_capacity(zlib_inflate_hint(data.len(), limit));
    let mut frame_offset = 0usize;
    while frame_offset < data.len() {
        let rest = &data[frame_offset..];
        let standard_magic = rest.len() >= 4
            && rest[0] == 0x28
            && rest[1] == 0xb5
            && rest[2] == 0x2f
            && rest[3] == 0xfd;
        let skippable_magic = rest.len() >= 4
            && (0x50..=0x5f).contains(&rest[0])
            && rest[1] == 0x2a
            && rest[2] == 0x4d
            && rest[3] == 0x18;
        if skippable_magic {
            if rest.len() < 8 {
                if reject_garbage {
                    return Err(NodeError::new(
                        "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                        "Trailing garbage after stream end",
                    ));
                }
                break;
            }
            let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            let total = 8usize.saturating_add(size);
            if total > rest.len() {
                if reject_garbage {
                    return Err(NodeError::new(
                        "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                        "Trailing garbage after stream end",
                    ));
                }
                break;
            }
            frame_offset += total;
            continue;
        }
        if !standard_magic {
            if frame_offset == 0 {
                return Err(NodeError::new(
                    "ZSTD_error_prefix_unknown",
                    "Unknown frame descriptor",
                ));
            }
            if reject_garbage {
                return Err(NodeError::new(
                    "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                    "Trailing garbage after stream end",
                ));
            }
            break;
        }
        let frame_size = match zstd::zstd_safe::find_frame_compressed_size(rest) {
            Ok(size) => size,
            Err(_error) => {
                if finish_flush.unwrap_or(4) != 4 {
                    return Ok(out);
                }
                return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
            }
        };
        if frame_size == 0 || frame_size > rest.len() {
            return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
        }
        let frame = &rest[..frame_size];
        let mut frame_decoder =
            zstd::stream::read::Decoder::with_dictionary(frame, dictionary.unwrap_or(&[]))
                .map_err(|error| {
                    NodeError::new(
                        "Z_DATA_ERROR",
                        format!("Z_DATA_ERROR: zstd decompression failed: {error}"),
                    )
                })?;
        if let Some(params) = params.and_then(|value| value.as_object()) {
            if let Some(window) = params.get("100").and_then(JsonValue::as_i64) {
                frame_decoder
                    .window_log_max(window as u32)
                    .map_err(|error| {
                        NodeError::new(
                            "ZSTD_error_frameParameter_windowTooLarge",
                            error.to_string(),
                        )
                    })?;
            }
        }
        frame_decoder.read_to_end(&mut out).map_err(|error| {
            let text = error.to_string();
            if text.contains("Frame requires too much memory") {
                NodeError::new("ZSTD_error_frameParameter_windowTooLarge", text)
            } else if text.to_ascii_lowercase().contains("checksum") {
                NodeError::new("ZSTD_error_checksum_wrong", text)
            } else {
                NodeError::new(
                    "Z_DATA_ERROR",
                    format!("Z_DATA_ERROR: decompression failed: {text}"),
                )
            }
        })?;
        if out.len() as u64 > limit {
            return Err(zlib_limit_error(max_output));
        }
        frame_offset += frame_size;
    }
    Ok(out)
}
