//! One connection to a pi-env daemon: lazy start, the `hello` handshake, the
//! 13-byte frame header, request/response correlation, `event` frames,
//! cancel-on-abort, `{ mode: "kill" }`, session pinning for handle requests,
//! the 5 s ping / 30 s silence rules, the `PI-ENV <token>` sync line, `lost`
//! errors and `close()` — upstream `packages/env/src/connection.ts`, which the
//! JS shim (`src/extensions/api/js/pi_env_shim.js.txt`) also carries verbatim.
//!
//! Frames are encoded and decoded by `crate::env_daemon::frame` — the daemon's
//! own codec — so both ends of the wire share one framing implementation:
//! `write_frame` into a `Vec<u8>` that goes to the child's stdin, and
//! `read_frame` over the bytes buffered so far (a slice is a `Read`, and a
//! short frame is `UnexpectedEof`, which means "wait for more bytes").
//!
//! Shape: one task owns the child's stdout and turns it into frames, one drains
//! stderr into `on_log`, one sends pings and enforces the silence limit, and
//! one owns the `Child` to observe (or force) its exit. Requests are
//! `tokio::sync::oneshot` slots in the session's pending map, so several run at
//! once and are matched by id, as upstream.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::ai::types::AbortSignal;
use crate::env_daemon::frame::{self, Frame, CANCEL, ERROR, EVENT, PING, REQUEST, RESULT};

use super::errors::RemoteError;

/// Largest frame either side accepts (docs/protocol.md).
pub const MAX_FRAME: usize = frame::MAX_FRAME;
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// The daemon pings every five seconds; this much silence means the connection is gone.
const SILENCE_LIMIT_MS: u64 = 30_000;
/// One frame write may not be stuck longer than this. Without it a wedged
/// pipe parks the writer forever and every request behind it never settles;
/// a peer that accepts no bytes for 30 s is declared lost.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Outbox depth (frames) past which the client reports that the writer is
/// stalled. Every pushed frame is popped by the writer task, so a healthy
/// session's depth tracks its in-flight requests; 4096 is far past that.
/// Observed, not enforced: see [`Session::outbox`] for why the queue is never
/// parked and never dropped.
const OUTBOX_WARN_DEPTH: usize = 4096;

/// Diagnostic cadence: the first crossing is reported, then one line per this
/// many further crossings (the daemon end uses the same cadence).
const OUTBOX_REPORT_EVERY: u64 = 1024;
/// How long starting the daemon and its `hello` may take.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a graceful shutdown may take before the client forces the daemon
/// down.
///
/// Graceful here means the documented protocol's only shutdown signal: the
/// daemon's stdin reaches EOF. `env_daemon::daemon::serve` then cancels the
/// registered controls, runs `kill_all` over the exec/pty process groups and
/// exits. Killing the daemon first (the old `kill` teardown) skipped that:
/// each exec/pty child is its own session (`setsid`), so it survived the
/// daemon's death. The bound is generous for that cleanup (signals are sent
/// synchronously) while still making a wedged or hostile daemon recoverable.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the forced path waits for the daemon to be reaped after SIGKILL
/// (or `taskkill /F /T` on Windows) before giving up. The child is already
/// dead after this phase's signal; this only bounds the reaper-thread wait.
const FORCED_KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// Exit-watcher poll interval. The watcher is an OS thread (not a runtime
/// task), so it can observe the daemon's exit even while a caller's `Drop` is
/// blocking a runtime worker; polling keeps it independent of tokio's child
/// reaper and of the async runtime being shut down.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// `#[doc(hidden)]` live task/session counters, shared by every task spawned
/// for one [`Connection`] and readable after the connection itself is gone.
///
/// The regression C2-05 is a lifetime leak: a task that keeps `Arc<Inner>`
/// alive also keeps the daemon and its SSH session alive. Tests capture this
/// handle, drop the connection, and assert every count returns to zero.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct TaskCounters {
    tasks: AtomicUsize,
    live_sessions: AtomicUsize,
}

impl TaskCounters {
    /// Tasks/threads currently running for the connection (reader, logger,
    /// writer, exit watcher, shutdown reaper).
    pub fn tasks(&self) -> usize {
        self.tasks.load(Ordering::SeqCst)
    }

    /// Sessions that have started and whose daemon child has not been reaped.
    pub fn live_sessions(&self) -> usize {
        self.live_sessions.load(Ordering::SeqCst)
    }

    fn session_started(&self) {
        self.live_sessions.fetch_add(1, Ordering::SeqCst);
    }

    fn session_exited(&self) {
        // Saturating: a session can only be marked exited once, but keep this
        // robust against a test/user constructing odd states.
        let _ = self
            .live_sessions
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            });
    }
}

/// A snapshot of one connection's live resources (`#[doc(hidden)]` test seam).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugCounts {
    pub tasks: usize,
    pub live_sessions: usize,
    pub pending_requests: usize,
}

/// RAII task-count guard: increments when a task starts, decrements when it
/// returns — including on panic.
struct TaskGuard {
    counters: Arc<TaskCounters>,
}

impl TaskGuard {
    fn new(counters: Arc<TaskCounters>) -> TaskGuard {
        counters.tasks.fetch_add(1, Ordering::SeqCst);
        TaskGuard { counters }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.counters.tasks.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Diagnostic text (the daemon's stderr, shell noise before the sync line).
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// One `event` frame of a running request: its JSON and payload.
pub type EventFn = Arc<dyn Fn(&Value, &[u8]) + Send + Sync>;
/// Called with `(request id, session id)` once a request is registered.
pub type StartFn = Arc<dyn Fn(u32, u32) + Send + Sync>;
/// The daemon command, computed at every start (an SSH start detects the
/// remote system, deploys the daemon, then builds the command).
pub type CommandFn = Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<String>, String>> + Send + Sync>;

/// Lock a `std::sync::Mutex`, ignoring poisoning (a panic in a task must not
/// take the connection's state with it — every critical section here is a
/// short map/slot access whose invariants hold regardless).
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Register one pending request slot and close the race with teardown.
///
/// `teardown` swaps the session's `live` flag to false and then drains the
/// pending map. A registration that inserts AFTER that drain (the caller
/// checked `is_live()` just before the swap) would leave its oneshot sender in
/// the map with no task left to answer it — the caller would await forever.
/// Insert first, then re-check: if the session is no longer live, take the
/// slot back (or find that teardown already drained it) and report it in
/// `Some`, so the caller fails with `lost` instead of hanging. `None` means
/// the slot is registered on a live session.
///
/// Public only so the race decision can be pinned without timing tricks; the
/// connection is the sole production caller.
#[doc(hidden)]
pub fn register_pending<T>(
    pending: &Mutex<HashMap<u32, T>>,
    live: &AtomicBool,
    id: u32,
    slot: T,
) -> Option<T> {
    lock(pending).insert(id, slot);
    if live.load(Ordering::SeqCst) {
        return None;
    }
    lock(pending).remove(&id)
}

fn lost_with_code(code: &str, message: impl Into<String>) -> RemoteError {
    RemoteError {
        lost: true,
        ..RemoteError::new(code, message)
    }
}

/// The incremental frame decoder of one session: buffered bytes in, whole
/// frames out, with the documented sync rule (shell startup files may print to
/// stdout before the daemon runs, so everything up to and including the
/// `PI-ENV <token>\n` line is dropped). `pub` so the integration tests can
/// drive it with the byte vectors of `packages/env/docs/protocol.md`; the
/// connection's reader task uses it for every chunk it reads.
pub struct StreamParser {
    buffer: Vec<u8>,
    /// Start of the unconsumed bytes. Frames advance the cursor instead of
    /// draining the front (a `drain(..total)` per frame memmoved the whole
    /// remainder, quadratic for a burst of small frames); the consumed prefix
    /// is compacted away in amortized O(1) once it is both large and at least
    /// half the buffer.
    start: usize,
    synced: bool,
    token: String,
    /// Shell noise seen before the sync marker. Bounded (last
    /// [`PRESYNC_NOISE_CAP`] bytes) and logged once, when the marker arrives;
    /// without it a marker split across two reads would drop the noise prefix
    /// that was already scanned (the regression the split-read test pins).
    presync: Vec<u8>,
    /// Bytes moved by compaction (the copy-cost regression seam).
    compacted: usize,
}

/// Upper bound on retained pre-sync shell noise: enough for a real startup
/// diagnostic, small enough that a wrapper printing forever cannot grow the
/// parser without bound.
const PRESYNC_NOISE_CAP: usize = 64 * 1024;

/// Compact the consumed prefix once it reaches this size (and at least half
/// the buffer), keeping the parser's total copying linear in the input.
const STREAM_PARSER_COMPACT_THRESHOLD: usize = 64 * 1024;

impl StreamParser {
    pub fn new(token: impl Into<String>) -> StreamParser {
        StreamParser {
            buffer: Vec::new(),
            start: 0,
            synced: false,
            token: token.into(),
            presync: Vec::new(),
            compacted: 0,
        }
    }

    /// Retain pre-sync noise for the eventual diagnostic, keeping the newest
    /// bytes when the cap is exceeded (the tail is what usually names the
    /// failing wrapper).
    fn append_presync(&mut self, bytes: &[u8]) {
        if bytes.len() >= PRESYNC_NOISE_CAP {
            self.presync.clear();
            self.presync
                .extend_from_slice(&bytes[bytes.len() - PRESYNC_NOISE_CAP..]);
            return;
        }
        let overflow = (self.presync.len() + bytes.len()).saturating_sub(PRESYNC_NOISE_CAP);
        if overflow > 0 {
            self.presync.drain(..overflow);
        }
        self.presync.extend_from_slice(bytes);
    }

    /// Whether the sync line has been seen.
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Bytes moved by buffer compaction so far. A quadratic implementation
    /// grows this with the square of the frame count; the cursor shape keeps
    /// it linear.
    #[doc(hidden)]
    pub fn compacted_bytes(&self) -> usize {
        self.compacted
    }

    /// Feed one chunk of the daemon's stdout; returns every complete frame it
    /// now holds, in arrival order. `Err` is the documented corruption failure
    /// (a length outside `9..=16 MiB`, or a frame the codec refuses): the
    /// connection is torn down. `on_log` receives shell noise printed before
    /// the sync line.
    ///
    /// **A frame that is not whole yet is kept, never an error.** The daemon
    /// writes each frame with several `write` calls (`output.rs::run` →
    /// `write_frame`'s header/JSON/payload), so a reader sees partial frames
    /// routinely; this mirrors the JS client's `_onData`, which returns as soon
    /// as `buffered < 4 + length`. `frame::read_frame` cannot make that
    /// decision: it reads a *blocking* stream, where a short body means the
    /// input ended, so it reports `UnexpectedEof` as an I/O error instead of
    /// "need more bytes". The completeness check therefore lives here, and the
    /// codec is used to decode the complete frame.
    pub fn feed(
        &mut self,
        chunk: &[u8],
        on_log: &mut dyn FnMut(&str),
    ) -> Result<Vec<Frame>, String> {
        self.buffer.extend_from_slice(chunk);
        if !self.synced {
            let marker = format!("PI-ENV {}\n", self.token);
            match find(&self.buffer[self.start..], marker.as_bytes()) {
                Some(relative) => {
                    let index = self.start + relative;
                    let mut noise = std::mem::take(&mut self.presync);
                    noise.extend_from_slice(&self.buffer[self.start..index]);
                    let noise = String::from_utf8_lossy(&noise).trim().to_string();
                    if !noise.is_empty() {
                        on_log(&noise);
                    }
                    self.start = index + marker.len();
                    self.synced = true;
                }
                None => {
                    // Keep only the suffix that could still be the start of a
                    // marker split across reads; everything before it is
                    // definitely noise. Move that noise into the bounded
                    // `presync` diagnostic buffer instead of dropping it, so a
                    // split marker does not lose the `banner\n` that preceded
                    // it. The buffer itself only ever retains a marker-sized
                    // tail, so an endless pre-sync stream stays bounded.
                    let available = &self.buffer[self.start..];
                    let keep = longest_suffix_prefix(available, marker.as_bytes());
                    let safe_end = available.len() - keep;
                    if safe_end > 0 {
                        let noise = available[..safe_end].to_vec();
                        self.append_presync(&noise);
                        self.start += safe_end;
                        self.compacted += safe_end;
                    }
                    if self.start >= STREAM_PARSER_COMPACT_THRESHOLD
                        && self.start * 2 >= self.buffer.len()
                    {
                        self.buffer.drain(..self.start);
                        self.start = 0;
                    }
                    return Ok(Vec::new());
                }
            }
        }
        let mut frames = Vec::new();
        loop {
            let available = &self.buffer[self.start..];
            if available.len() < 4 {
                break;
            }
            // `length` counts the bytes after the length field: `u32 length |
            // u8 type | u32 id | u32 jsonLength | json | payload`.
            let length =
                u32::from_be_bytes([available[0], available[1], available[2], available[3]])
                    as usize;
            if !(9..=MAX_FRAME).contains(&length) {
                return Err("Corrupt frame from pi-env".to_string());
            }
            let total = 4 + length;
            if available.len() < total {
                // The rest of the frame is still on its way.
                break;
            }
            let mut cursor: &[u8] = &available[..total];
            match frame::read_frame(&mut cursor) {
                Ok(Some(parsed)) => {
                    self.start += total;
                    frames.push(parsed);
                }
                // A whole frame the codec refuses (a JSON length beyond the
                // frame) is corruption.
                Ok(None) | Err(_) => return Err("Corrupt frame from pi-env".to_string()),
            }
        }
        // Reclaim the consumed prefix in one move (amortized), never once per
        // frame.
        if self.start >= STREAM_PARSER_COMPACT_THRESHOLD && self.start * 2 >= self.buffer.len() {
            self.compacted += self.buffer.len() - self.start;
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        Ok(frames)
    }
}

/// The handshake's version check: `protocol: 1` is the contract, anything else
/// ends the start with upstream's message (an absent protocol reads `none`).
pub fn checked_protocol(json: &Value) -> Result<(), RemoteError> {
    match json.get("protocol").and_then(Value::as_u64) {
        Some(1) => Ok(()),
        other => Err(RemoteError::lost(format!(
            "Unsupported protocol {}",
            other
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        ))),
    }
}

/// What the daemon reports about its machine (`hello`, docs/protocol.md).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteInfo {
    pub protocol: u64,
    pub version: String,
    /// Rust's `std::env::consts::OS` spelling (`linux`, `android`, `macos`, `windows`).
    pub os: String,
    /// Rust's `std::env::consts::ARCH` spelling (`x86_64`, `aarch64`).
    pub arch: String,
    pub home: String,
    pub separator: String,
    pub tmpdir: String,
    pub cwd: String,
    /// Windows' `=C:` variables: what `path.resolve` falls back to for drive-relative paths.
    pub drive_cwds: serde_json::Map<String, Value>,
    pub pid: u64,
    /// Operations added to protocol 1 after its first release (`listFiles`).
    pub features: Vec<String>,
    /// The full hello JSON, so callers can read fields this struct does not name.
    pub raw: Value,
}

impl RemoteInfo {
    pub fn from_json(json: &Value) -> RemoteInfo {
        let string = |key: &str| {
            json.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        RemoteInfo {
            protocol: json.get("protocol").and_then(Value::as_u64).unwrap_or(0),
            version: string("version"),
            os: string("os"),
            arch: string("arch"),
            home: string("home"),
            separator: string("separator"),
            tmpdir: string("tmpdir"),
            cwd: string("cwd"),
            drive_cwds: json
                .get("driveCwds")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            pid: json.get("pid").and_then(Value::as_u64).unwrap_or(0),
            features: json
                .get("features")
                .and_then(Value::as_array)
                .map(|features| {
                    features
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            raw: json.clone(),
        }
    }

    /// `nodePlatform`: the daemon's names in Node's spelling (`windows` is
    /// `win32`, `macos` is `darwin`, `x86_64` is `x64`), so they compare with
    /// `process.platform` / `process.arch`.
    pub fn node_platform(&self) -> (String, String) {
        node_platform(&self.os, &self.arch)
    }

    /// Whether the daemon advertises an optional operation (`hello.features`).
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|name| name == feature)
    }
}

/// Node's names for the daemon's system and architecture (upstream
/// `nodePlatform`). Other names are kept.
pub fn node_platform(os: &str, arch: &str) -> (String, String) {
    let platform = match os {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    (platform.to_string(), arch.to_string())
}

/// One successful reply: the result frame's JSON, its payload, the session that
/// answered (handles belong to the session that opened them) and the request id
/// (so a caller can drop the request from its running set, or cancel it).
#[derive(Debug, Clone)]
pub struct Reply {
    pub json: Value,
    pub payload: Vec<u8>,
    pub session: u32,
    pub request_id: u32,
}

/// Per-request options: a handle's session pin, the abort signal, the event
/// handler, the registration callback and the request payload.
#[derive(Default)]
pub struct RequestOptions {
    /// Pin the request to one session: handles are only valid there.
    pub session: Option<u32>,
    pub signal: Option<AbortSignal>,
    pub on_event: Option<EventFn>,
    pub on_start: Option<StartFn>,
    pub payload: Vec<u8>,
}

struct Pending {
    reply: oneshot::Sender<Result<Reply, RemoteError>>,
    on_event: Option<EventFn>,
}

/// Options for a [`Connection`]: how to start the daemon, where its diagnostics
/// go, and whether it runs on this machine.
pub struct ConnectionOptions {
    pub command: CommandFn,
    pub on_log: Option<LogFn>,
    /// Whether the daemon runs on **this** machine (started as a child
    /// process), which [`ConnectionOptions::argv`] assumes and an SSH
    /// connection clears.
    ///
    /// A local daemon inherits this process's environment when it starts; it
    /// is long-lived, so `exec` with `inheritEnv` forwards the *live*
    /// environment along with each request (see `RemoteExecutionEnv::exec`).
    /// For a daemon on another machine that environment belongs to the other
    /// host, so nothing is forwarded and the daemon's own inherited
    /// environment is the base.
    pub local: bool,
    /// The writer's ping cadence (default [`PING_INTERVAL`]). Exposed for
    /// embedded hosts and the liveness tests, whose fake daemon cannot wait
    /// out the production 5 s/30 s pair.
    #[doc(hidden)]
    pub ping_interval: Duration,
    /// How much silence (no frame from the daemon) means the connection is
    /// gone (default 30 s, see [`SILENCE_LIMIT_MS`]).
    #[doc(hidden)]
    pub silence_limit: Duration,
}

impl ConnectionOptions {
    /// A connection to a fixed command, e.g. `<pir> env-daemon` (`localDaemon`).
    ///
    /// The client appends `serve --token <hex>` to whatever is given here, so
    /// the daemon subcommand belongs in `args`/`argv`
    /// (`["env-daemon"]` for a pir image, `[]` for an upstream `pi-env`
    /// binary). Passing neither produces `<program> serve --token …`, which is
    /// not any daemon this client can talk to.
    pub fn command(program: impl Into<String>, args: Vec<String>) -> ConnectionOptions {
        let mut command = vec![program.into()];
        command.extend(args);
        ConnectionOptions::argv(command)
    }

    /// A connection to a complete argv, e.g. `local_daemon()`'s
    /// `[<pir>, "env-daemon"]`. The client appends `serve --token <hex>`.
    pub fn argv(command: Vec<String>) -> ConnectionOptions {
        ConnectionOptions::dynamic(Arc::new(move || {
            Box::pin(std::future::ready(Ok(command.clone())))
        }))
    }

    /// A connection whose daemon command is computed at every start (the SSH
    /// deployment path computes it after detecting the remote system).
    pub fn dynamic(command: CommandFn) -> ConnectionOptions {
        ConnectionOptions {
            command,
            on_log: None,
            local: true,
            ping_interval: PING_INTERVAL,
            silence_limit: Duration::from_millis(SILENCE_LIMIT_MS),
        }
    }

    /// Override the writer's ping cadence and silence limit (the production
    /// defaults are 5 s / 30 s). Embedded hosts and the liveness tests use
    /// this; no behavior changes at the defaults.
    pub fn with_liveness(mut self, ping_interval: Duration, silence_limit: Duration) -> Self {
        self.ping_interval = ping_interval;
        self.silence_limit = silence_limit;
        self
    }

    pub fn on_log(mut self, on_log: LogFn) -> ConnectionOptions {
        self.on_log = Some(on_log);
        self
    }

    /// Mark the daemon as running on another machine (an SSH connection): its
    /// environment is that host's, not this process's, so `exec` does not
    /// forward the caller's environment.
    pub fn remote(mut self) -> ConnectionOptions {
        self.local = false;
        self
    }
}

/// One running daemon.
struct Session {
    id: u32,
    token: String,
    start: Instant,
    last_seen_ms: AtomicU64,
    live: AtomicBool,
    /// The writer's ping cadence for this session ([`PING_INTERVAL`] unless
    /// [`ConnectionOptions::with_liveness`] overrode it).
    ping_interval: Duration,
    /// Silence after which the writer declares the daemon lost.
    silence_limit: Duration,
    /// Whether `hello` was answered: the exit message depends on it.
    ready: AtomicBool,
    /// Frames waiting for the writer task, in the order they were written.
    /// `write` is synchronous into this queue (exactly the order the request's
    /// `onStart`/cancel callbacks run in), so a `cancel` can never overtake the
    /// `request` it cancels, however the async tasks are scheduled.
    ///
    /// The queue is NOT parked when long: `write` is called from the caller's
    /// async task (and from synchronous callbacks), so blocking it would stall
    /// the caller's runtime; dropping or rejecting a frame would silently break
    /// the request it carries (its reply would never be awaited). Its depth is
    /// therefore observed instead ([`OUTBOX_WARN_DEPTH`]): every pushed frame
    /// is answered by one writer pop, so in practice the depth tracks the
    /// caller's own in-flight request count, and a depth past the water mark
    /// means the writer's pipe is stalled — reported, never silent.
    outbox: Mutex<VecDeque<Frame>>,
    /// Outbox water-mark crossings (diagnostic cadence).
    outbox_warned: AtomicU64,
    /// Wakes the writer task; created before the queue is checked, so no push
    /// is lost.
    wake: tokio::sync::Notify,
    pending: Mutex<HashMap<u32, Pending>>,
    /// The `hello` reply, cached per session as upstream caches it per start.
    info: tokio::sync::OnceCell<RemoteInfo>,
    /// Set once the daemon child has been reaped. The shutdown reaper waits
    /// on this before escalating to a forced kill.
    exited: AtomicBool,
    /// Guards [`Session::exited`] + [`Session::exit_cv`] wakeups (a plain
    /// atomic check could miss the notification between check and wait).
    exit_lock: Mutex<()>,
    exit_cv: Condvar,
    /// Set by [`Session::request_kill`]: the exit-watcher thread calls
    /// `start_kill` on its next poll. A flag (not a channel) so a `Drop` from
    /// any thread — including one with no tokio runtime — can raise it.
    kill_requested: AtomicBool,
    /// Stops the writer/ping task.
    stop: Mutex<Option<oneshot::Sender<()>>>,
    /// Task/live-session accounting (`#[doc(hidden)]` test seam).
    counters: Arc<TaskCounters>,
}

impl Session {
    fn touch(&self) {
        self.last_seen_ms
            .store(self.start.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    fn silent_ms(&self) -> u64 {
        self.start
            .elapsed()
            .as_millis()
            .saturating_sub(u128::from(self.last_seen_ms.load(Ordering::SeqCst))) as u64
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    /// Queue one frame for the session's writer task. This is synchronous (the
    /// caller's order is the wire order); the frame is dropped when the session
    /// is gone, exactly as upstream's `_write` drops writes on a dead session.
    fn write(&self, frame: Frame) {
        if !self.is_live() {
            return;
        }
        let depth = {
            let mut outbox = lock(&self.outbox);
            outbox.push_back(frame);
            outbox.len()
        };
        // Observation (see `Session::outbox`): a writer stalled behind a full
        // pipe is reported rather than parked (parking a caller's task) or
        // silently dropped (breaking the request the frame carries).
        if depth >= OUTBOX_WARN_DEPTH {
            let crossings = self.outbox_warned.fetch_add(1, Ordering::Relaxed) + 1;
            if crossings == 1 || crossings.is_multiple_of(OUTBOX_REPORT_EVERY) {
                crate::ext_diag!(
                    "[env-client] outbox at {depth} frames (daemon not draining; {} crossings)",
                    crossings
                );
            }
        }
        self.wake.notify_one();
    }

    /// Cancel one registered request (`{ mode: "kill" }` is sent by
    /// [`Connection::kill`] instead).
    fn cancel(&self, id: u32) {
        if self.is_registered(id) {
            self.write(Frame::new(CANCEL, id, Value::Object(Default::default())));
        }
    }

    fn is_registered(&self, id: u32) -> bool {
        lock(&self.pending).contains_key(&id)
    }

    fn dispatch(&self, frame: Frame) {
        if frame.kind == EVENT {
            let handler = lock(&self.pending)
                .get(&frame.id)
                .and_then(|pending| pending.on_event.clone());
            if let Some(handler) = handler {
                handler(&frame.json, &frame.payload);
            }
            return;
        }
        if frame.kind != RESULT && frame.kind != ERROR {
            return;
        }
        let pending = lock(&self.pending).remove(&frame.id);
        let Some(pending) = pending else {
            return;
        };
        if frame.kind == RESULT {
            let _ = pending.reply.send(Ok(Reply {
                json: frame.json,
                payload: frame.payload,
                session: self.id,
                request_id: frame.id,
            }));
        } else {
            let _ = pending.reply.send(Err(RemoteError::from_json(&frame.json)));
        }
    }

    /// Mark the daemon child reaped (idempotent) and wake the shutdown reaper.
    fn mark_exited(&self) {
        let _guard = lock(&self.exit_lock);
        if !self.exited.swap(true, Ordering::SeqCst) {
            self.counters.session_exited();
        }
        self.exit_cv.notify_all();
    }

    /// Wait at most `timeout` for the daemon child to be reaped.
    fn wait_exited(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = lock(&self.exit_lock);
        loop {
            if self.exited.load(Ordering::SeqCst) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self
                .exit_cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            guard = next;
        }
    }

    /// Ask the exit watcher to send the forced kill; the watcher also reaps
    /// the child.
    fn request_kill(&self) {
        self.kill_requested.store(true, Ordering::SeqCst);
    }
}

struct Inner {
    options: ConnectionOptions,
    session: Mutex<Option<Arc<Session>>>,
    /// Serializes start attempts; concurrent requests share the start that won.
    gate: AsyncMutex<()>,
    next_id: AtomicU32,
    sessions: AtomicU32,
    closed: AtomicBool,
    counters: Arc<TaskCounters>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last external `Connection` (and every task that held one) is
        // gone. There is no caller left to invoke `close()`, so this is the
        // connection's Drop path: mark closed, take the published session and
        // start the same graceful shutdown `close()` uses. A reaper thread
        // waits out `GRACEFUL_SHUTDOWN_TIMEOUT` and only then sets the forced
        // kill flag, so the daemon gets its chance to run `kill_all` and reap
        // the exec/pty process groups.
        self.closed.store(true, Ordering::SeqCst);
        let session = lock(&self.session).take();
        if let Some(session) = session {
            teardown_session(None, &session, &RemoteError::closed());
        }
    }
}

/// Tears one starting session down when the [`Connection::start`] future is
/// dropped before it completes (a caller's timeout or cancellation).
///
/// The start's spawned tasks hold `Arc<Session>` themselves, so a cancelled
/// start would otherwise leave a half-started (or already handshaken but
/// unpublished) daemon running with no owner left to stop it: the writer would
/// ping it forever.
struct StartGuard {
    connection: Connection,
    session: Arc<Session>,
    armed: bool,
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if self.armed {
            self.connection
                .teardown(&self.session, &RemoteError::lost("pi-env connection lost"));
        }
    }
}

/// One connection to a pi-env daemon: started lazily on the first request and
/// started again after it is lost. Requests in flight when it is lost fail with
/// code `unknown` and `lost: true`; for mutations their outcome is then
/// unknown.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    pub fn new(options: ConnectionOptions) -> Connection {
        Connection {
            inner: Arc::new(Inner {
                options,
                session: Mutex::new(None),
                gate: AsyncMutex::new(()),
                next_id: AtomicU32::new(1),
                sessions: AtomicU32::new(0),
                closed: AtomicBool::new(false),
                counters: Arc::new(TaskCounters::default()),
            }),
        }
    }

    /// A connection to `<program> <args…>`, started lazily. The daemon
    /// subcommand belongs in `args`: `["env-daemon"]` for a pir image.
    pub fn command(program: impl Into<String>, args: Vec<String>) -> Connection {
        Connection::new(ConnectionOptions::command(program, args))
    }

    /// A connection to a complete argv (e.g. `local_daemon()`'s
    /// `[<pir>, "env-daemon"]`), started lazily.
    pub fn argv(command: Vec<String>) -> Connection {
        Connection::new(ConnectionOptions::argv(command))
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// `#[doc(hidden)]` test seam: a handle that stays readable after this
    /// connection (and its `Inner`) is dropped.
    #[doc(hidden)]
    pub fn debug_counters(&self) -> Arc<TaskCounters> {
        self.inner.counters.clone()
    }

    /// `#[doc(hidden)]` test seam: live task/session/pending counts right now.
    #[doc(hidden)]
    pub fn debug_counts(&self) -> DebugCounts {
        let session = lock(&self.inner.session).clone();
        DebugCounts {
            tasks: self.inner.counters.tasks(),
            live_sessions: self.inner.counters.live_sessions(),
            pending_requests: session
                .map(|session| lock(&session.pending).len())
                .unwrap_or(0),
        }
    }

    /// Whether the daemon runs on this machine (see [`ConnectionOptions::local`]).
    pub fn is_local(&self) -> bool {
        self.inner.options.local
    }

    /// Connect if needed and return what the daemon reported about its machine.
    pub async fn info(&self) -> Result<RemoteInfo, RemoteError> {
        self.session_info().await
    }

    /// The session's `hello` reply, JSON as received.
    pub async fn hello(&self) -> Result<Value, RemoteError> {
        Ok(self.session_info().await?.raw)
    }

    /// The start's `hello` reply, cached per session (upstream's `_connect()`
    /// keeps the reply the handshake produced).
    async fn session_info(&self) -> Result<RemoteInfo, RemoteError> {
        let session = self.connect().await?;
        if let Some(info) = session.info.get() {
            return Ok(info.clone());
        }
        let reply = self
            .send(
                &session,
                "hello",
                serde_json::json!({ "protocol": 1 }),
                RequestOptions::default(),
            )
            .await?;
        let info = RemoteInfo::from_json(&reply.json);
        let _ = session.info.set(info.clone());
        Ok(info)
    }

    /// Connect if needed and return the live session's id.
    pub async fn session(&self) -> Result<u32, RemoteError> {
        Ok(self.connect().await?.id)
    }

    pub async fn request(
        &self,
        op: &str,
        json: Value,
        options: RequestOptions,
    ) -> Result<Reply, RemoteError> {
        if let Some(id) = options.session {
            let current = lock(&self.inner.session)
                .clone()
                .filter(|session| session.id == id && session.is_live());
            let Some(current) = current else {
                return Err(RemoteError::lost("pi-env connection lost"));
            };
            return self.send(&current, op, json, options).await;
        }
        let session = self.connect().await?;
        self.send(&session, op, json, options).await
    }

    /// Stop the daemon; it kills everything it started. Requests in flight fail
    /// with `Connection closed`.
    ///
    /// The shutdown order is deliberate: stdin is closed first (the writer
    /// stops and drops it), which is the protocol's graceful shutdown — the
    /// daemon's `serve` loop ends, cancels its controls and runs `kill_all`
    /// over the exec/pty process groups. Only if the daemon has not exited
    /// within [`GRACEFUL_SHUTDOWN_TIMEOUT`] is a forced kill requested. This
    /// method waits for that bounded sequence, so a returned `close()` means
    /// the daemon was either reaped or force-killed.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        // Read the session out with the guard released BEFORE `teardown` runs:
        // an `if let` over `lock(..).clone()` keeps the guard alive for the
        // whole block, and `teardown` re-locks the same mutex (std mutexes are
        // not reentrant), which deadlocks the calling thread.
        let session = lock(&self.inner.session).clone();
        if let Some(session) = session {
            self.teardown(&session, &RemoteError::closed());
            if !session.wait_exited(GRACEFUL_SHUTDOWN_TIMEOUT) {
                session.request_kill();
                let _ = session.wait_exited(FORCED_KILL_TIMEOUT);
            }
        }
    }

    /// Kill one `exec` without aborting it: the command settles with its killed
    /// status, as `cleanup()` does.
    pub fn kill(&self, id: u32, session: u32) {
        let current = lock(&self.inner.session)
            .clone()
            .filter(|current| current.id == session && current.is_live());
        if let Some(current) = current {
            current.write(Frame::new(
                CANCEL,
                id,
                serde_json::json!({ "mode": "kill" }),
            ));
        }
    }

    async fn connect(&self) -> Result<Arc<Session>, RemoteError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RemoteError::closed());
        }
        if let Some(session) = lock(&self.inner.session).clone().filter(|s| s.is_live()) {
            return Ok(session);
        }
        // One start at a time; the session the winner published is reused.
        let _guard = self.inner.gate.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RemoteError::closed());
        }
        if let Some(session) = lock(&self.inner.session).clone().filter(|s| s.is_live()) {
            return Ok(session);
        }
        // A failed start is not remembered: the next request tries again.
        self.start().await
    }

    async fn start(&self) -> Result<Arc<Session>, RemoteError> {
        let command = match (self.inner.options.command)().await {
            Ok(command) => command,
            Err(message) => return Err(lost_with_code("spawn_error", message)),
        };
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RemoteError::closed());
        }
        let mut parts = command.into_iter();
        let Some(program) = parts.next() else {
            return Err(RemoteError::new("spawn_error", "No daemon command"));
        };
        let mut args: Vec<String> = parts.collect();
        let token = random_token();
        args.push("serve".to_string());
        args.push("--token".to_string());
        args.push(token.clone());
        let mut spawn = Command::new(&program);
        spawn
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match spawn.spawn() {
            Ok(child) => child,
            Err(error) => return Err(lost_with_code("spawn_error", error.to_string())),
        };
        let session = Arc::new(Session {
            id: self.inner.sessions.fetch_add(1, Ordering::SeqCst) + 1,
            token,
            start: Instant::now(),
            last_seen_ms: AtomicU64::new(0),
            live: AtomicBool::new(true),
            ping_interval: self.inner.options.ping_interval,
            silence_limit: self.inner.options.silence_limit,
            ready: AtomicBool::new(false),
            outbox: Mutex::new(VecDeque::new()),
            outbox_warned: AtomicU64::new(0),
            wake: tokio::sync::Notify::new(),
            pending: Mutex::new(HashMap::new()),
            info: tokio::sync::OnceCell::new(),
            exited: AtomicBool::new(false),
            exit_lock: Mutex::new(()),
            exit_cv: Condvar::new(),
            kill_requested: AtomicBool::new(false),
            stop: Mutex::new(None),
            counters: self.inner.counters.clone(),
        });
        self.inner.counters.session_started();
        session.touch();
        // Start the session's tasks BEFORE publishing it: each installs the
        // stop/kill sender a teardown needs, so a `close()` — or a daemon that
        // exits immediately — that races the start can always stop the writer
        // and kill the child. Publishing first left a window in which teardown
        // found no senders and the child lived until stdin EOF.
        //
        // The pipes are taken first and the exit watcher is spawned first, so
        // its kill sender exists before any other task can tear the session
        // down.
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        self.spawn_exit_watcher(child, &session);
        if let Some(stdout) = stdout {
            self.spawn_reader(stdout, &session);
        }
        if let Some(stderr) = stderr {
            self.spawn_logger(stderr, &session);
        }
        self.spawn_writer(stdin, &session);
        // The start future may be cancelled (a caller's timeout) while the
        // handshake is in flight. The spawned tasks hold `Arc<Session>` and
        // keep the child alive on their own, so the start itself must tear the
        // session down when the future is dropped before publishing.
        let mut start_guard = StartGuard {
            connection: self.clone(),
            session: session.clone(),
            armed: true,
        };
        if !session.is_live() {
            // The daemon exited (or was torn down) before it was published.
            return Err(RemoteError::lost("pi-env connection lost"));
        }
        // A `close()` that raced the spawn saw no published session to tear
        // down; do it here, now that the stop/kill senders exist.
        if self.inner.closed.load(Ordering::SeqCst) {
            self.teardown(&session, &RemoteError::closed());
            return Err(RemoteError::closed());
        }
        // The handshake: `hello { protocol: 1 }`, answered within the start timeout.
        let hello = tokio::time::timeout(
            START_TIMEOUT,
            self.send(
                &session,
                "hello",
                serde_json::json!({ "protocol": 1 }),
                RequestOptions::default(),
            ),
        )
        .await;
        let result = match hello {
            Ok(Ok(reply)) => match checked_protocol(&reply.json) {
                Ok(()) => {
                    session.ready.store(true, Ordering::SeqCst);
                    // The handshake's own reply is what `info()` reports, as
                    // upstream caches the start's hello for the connection.
                    let _ = session.info.set(RemoteInfo::from_json(&reply.json));
                    Ok(session.clone())
                }
                Err(error) => Err(error),
            },
            Ok(Err(error)) => Err(error),
            Err(_) => Err(RemoteError::lost(format!(
                "pi-env did not answer within {} s",
                START_TIMEOUT.as_secs()
            ))),
        };
        let session = match result {
            Ok(session) => session,
            Err(error) => {
                self.teardown(&session, &error);
                return Err(error);
            }
        };
        // Publish only a READY session: a concurrent `connect()` must never
        // hand out a daemon whose handshake is still in flight (it would skip
        // the protocol check), and it waits on the start gate until here.
        *lock(&self.inner.session) = Some(session.clone());
        if !session.is_live() {
            // The exit watcher tore the session down between the handshake and
            // the publish: never leave a dead session in the slot.
            self.teardown(&session, &RemoteError::lost("pi-env connection lost"));
            return Err(RemoteError::lost("pi-env connection lost"));
        }
        if self.inner.closed.load(Ordering::SeqCst) {
            // `close()` raced the publish (it read the slot before this
            // session landed): tear the fresh daemon down here.
            self.teardown(&session, &RemoteError::closed());
            return Err(RemoteError::closed());
        }
        start_guard.armed = false;
        Ok(session)
    }

    /// The task that turns the daemon's stdout into frames.
    fn spawn_reader(&self, mut stdout: tokio::process::ChildStdout, session: &Arc<Session>) {
        // Weak, not `Connection`: a long-lived reader must never be the reason
        // `Inner` (and with it the daemon/SSH session) stays alive after the
        // caller drops every handle. `Inner::drop` performs the teardown; a
        // failed upgrade means that path is already running.
        let inner = Arc::downgrade(&self.inner);
        let on_log = self.inner.options.on_log.clone();
        let session = session.clone();
        let counters = self.inner.counters.clone();
        tokio::spawn(async move {
            let _task = TaskGuard::new(counters);
            // The documented framing and sync rules live in `StreamParser`,
            // which the protocol-vector tests drive directly.
            let mut parser = StreamParser::new(session.token.clone());
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                // A read failure is an end of input here: the daemon's stdout
                // closed (the exit watcher reports the code).
                let read = stdout.read(&mut chunk).await.unwrap_or_default();
                if read == 0 {
                    // The daemon's end of the pipe closed: the connection is gone.
                    teardown_via(
                        &inner,
                        &session,
                        &RemoteError::lost("pi-env connection lost"),
                    );
                    return;
                }
                if !session.is_live() {
                    return;
                }
                session.touch();
                let mut log = |noise: &str| {
                    if let Some(on_log) = &on_log {
                        on_log(noise);
                    }
                };
                let frames = match parser.feed(&chunk[..read], &mut log) {
                    Ok(frames) => frames,
                    Err(message) => {
                        teardown_via(&inner, &session, &RemoteError::lost(message));
                        return;
                    }
                };
                for parsed in frames {
                    // A JSON body that is not an object is as corrupt as a bad
                    // length (the codec keeps the framing intact for the
                    // daemon's own error reply, which is not a case the client
                    // can act on).
                    if !parsed.json.is_object() {
                        teardown_via(
                            &inner,
                            &session,
                            &RemoteError::lost("Corrupt frame from pi-env"),
                        );
                        return;
                    }
                    session.dispatch(parsed);
                    if !session.is_live() {
                        return;
                    }
                }
            }
        });
    }

    fn spawn_logger(&self, mut stderr: tokio::process::ChildStderr, session: &Arc<Session>) {
        let on_log = self.inner.options.on_log.clone();
        let session = session.clone();
        let counters = self.inner.counters.clone();
        tokio::spawn(async move {
            let _task = TaskGuard::new(counters);
            let mut chunk = vec![0u8; 16 * 1024];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        if let Some(on_log) = &on_log {
                            on_log(&String::from_utf8_lossy(&chunk[..read]));
                        }
                        if !session.is_live() {
                            return;
                        }
                    }
                }
            }
        });
    }

    /// The session's writer and pinger: it drains the outbox in FIFO order
    /// into the daemon's stdin (so frame order is the order `write` was called
    /// in), and sends a `ping` every 5 s while enforcing the 30 s silence
    /// limit. Returns when the session is torn down, dropping stdin with it.
    fn spawn_writer(&self, stdin: Option<ChildStdin>, session: &Arc<Session>) {
        let inner = Arc::downgrade(&self.inner);
        let session = session.clone();
        let counters = self.inner.counters.clone();
        let (stop_sender, mut stop_receiver) = oneshot::channel();
        *lock(&session.stop) = Some(stop_sender);
        tokio::spawn(async move {
            let _task = TaskGuard::new(counters);
            let mut stdin = stdin;
            let mut next_ping = tokio::time::Instant::now() + session.ping_interval;
            loop {
                // The authority on the session's state: a teardown that ran
                // before this task installed its stop sender (the start spawn
                // race) cannot wake it through the channel, but it did clear
                // `live`.
                if !session.is_live() {
                    return;
                }
                // The ping deadline is an independent step, not something
                // polled only when a frame happens to be queued: an idle
                // session must still ping (the daemon exits after 30 s of
                // client silence) and must still notice a daemon that went
                // silent (no ping and no reply).
                let now = tokio::time::Instant::now();
                if now >= next_ping {
                    if Duration::from_millis(session.silent_ms()) > session.silence_limit {
                        teardown_via(
                            &inner,
                            &session,
                            &RemoteError::lost("pi-env connection timed out"),
                        );
                        return;
                    }
                    session.write(Frame::new(PING, 0, Value::Object(Default::default())));
                    next_ping = now + session.ping_interval;
                }
                // A `notified` future created before the queue check cannot
                // miss a wakeup that happens in between.
                let notified = session.wake.notified();
                let frame = lock(&session.outbox).pop_front();
                let Some(frame) = frame else {
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep_until(next_ping) => {}
                        _ = &mut stop_receiver => return,
                    }
                    continue;
                };
                let Some(pipe) = stdin.as_mut() else {
                    // The child has no stdin pipe, so no frame (not even the
                    // handshake) can ever reach it: every pending request would
                    // wait forever. Fail the session like a lost connection.
                    teardown_via(
                        &inner,
                        &session,
                        &RemoteError::lost("pi-env connection lost"),
                    );
                    return;
                };
                let mut bytes: Vec<u8> = Vec::with_capacity(64 + frame.payload.len());
                if frame::write_frame(&mut bytes, &frame).is_err() {
                    // An unencodable frame (an oversized request) cannot be
                    // answered by the daemon, so leaving the session live
                    // would strand every later request behind a frame that
                    // never reaches the wire. Tear the session down; the
                    // request's pending slot fails with `lost`.
                    teardown_via(
                        &inner,
                        &session,
                        &RemoteError::lost("pi-env request exceeds the frame limit"),
                    );
                    return;
                }
                // A write that does not make progress within `WRITE_TIMEOUT`
                // means the daemon's input pipe is wedged: tear the session
                // down instead of parking every queued request behind it. The
                // bound is per write call (progress resets it), so a large
                // frame on a slow but live link is not mistaken for a stall.
                let mut written = 0;
                while written < bytes.len() {
                    match tokio::time::timeout(WRITE_TIMEOUT, pipe.write(&bytes[written..])).await {
                        Ok(Ok(0)) => {
                            teardown_via(
                                &inner,
                                &session,
                                &RemoteError::lost("pi-env connection lost"),
                            );
                            return;
                        }
                        Ok(Ok(count)) => written += count,
                        Ok(Err(_)) => {
                            teardown_via(
                                &inner,
                                &session,
                                &RemoteError::lost("pi-env connection lost"),
                            );
                            return;
                        }
                        Err(_) => {
                            teardown_via(
                                &inner,
                                &session,
                                &RemoteError::lost("pi-env write timed out"),
                            );
                            return;
                        }
                    }
                }
                match tokio::time::timeout(WRITE_TIMEOUT, pipe.flush()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        teardown_via(
                            &inner,
                            &session,
                            &RemoteError::lost("pi-env connection lost"),
                        );
                        return;
                    }
                    Err(_) => {
                        teardown_via(
                            &inner,
                            &session,
                            &RemoteError::lost("pi-env write timed out"),
                        );
                        return;
                    }
                }
            }
        });
    }

    /// The thread that owns the `Child`: it reports the exit and reaps it, or
    /// kills the child when the session was torn down first.
    ///
    /// An OS thread rather than a tokio task: `Child` must be reaped even if
    /// the last `Connection` is dropped while a caller blocks a runtime worker
    /// in `Drop`, and the shutdown reaper needs `exited` to be set no matter
    /// which runtime (if any) is still alive.
    fn spawn_exit_watcher(&self, mut child: Child, session: &Arc<Session>) {
        let inner = Arc::downgrade(&self.inner);
        let session = session.clone();
        let thread_session = session.clone();
        let counters = self.inner.counters.clone();
        let spawned = std::thread::Builder::new()
            .name("pir-env-exit".to_string())
            .spawn(move || {
                let _task = TaskGuard::new(counters);
                let mut kill_sent = false;
                loop {
                    match child.try_wait() {
                        Ok(Some(_status)) => {
                            thread_session.mark_exited();
                            let message = if thread_session.ready.load(Ordering::SeqCst) {
                                "pi-env connection lost".to_string()
                            } else {
                                format!(
                                    "pi-env exited with code {} before it was ready",
                                    _status
                                        .code()
                                        .map(|code| code.to_string())
                                        .unwrap_or_else(|| "unknown".to_string())
                                )
                            };
                            teardown_via(&inner, &thread_session, &RemoteError::lost(message));
                            return;
                        }
                        Ok(None) => {
                            if thread_session.kill_requested.load(Ordering::SeqCst) && !kill_sent {
                                kill_sent = true;
                                let _ = child.start_kill();
                            }
                            std::thread::sleep(EXIT_POLL_INTERVAL);
                        }
                        Err(_) => {
                            thread_session.mark_exited();
                            teardown_via(
                                &inner,
                                &thread_session,
                                &RemoteError::lost("pi-env connection lost"),
                            );
                            return;
                        }
                    }
                }
            });
        if spawned.is_err() {
            // No watcher means no way to observe (or later force) the child.
            // `kill_on_drop(true)` still terminates it when the failed spawn
            // drops the closure's `Child`; account the session as gone.
            session.mark_exited();
            session.request_kill();
        }
    }

    /// Register a request id and send its frame; the reply arrives on the
    /// oneshot the caller awaits.
    async fn send(
        &self,
        session: &Arc<Session>,
        op: &str,
        json: Value,
        options: RequestOptions,
    ) -> Result<Reply, RemoteError> {
        if !session.is_live() {
            return Err(RemoteError::lost("pi-env connection lost"));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        let slot = Pending {
            reply: sender,
            on_event: options.on_event.clone(),
        };
        // Insert, then re-check liveness: `teardown` swaps `live` to false and
        // drains the map, so a registration that lands after that drain would
        // otherwise keep its oneshot sender in the map forever and the caller
        // would await a reply that can never arrive.
        if let Some(slot) = register_pending(&session.pending, &session.live, id, slot) {
            let _ = slot
                .reply
                .send(Err(RemoteError::lost("pi-env connection lost")));
            return Err(RemoteError::lost("pi-env connection lost"));
        }
        if let Some(on_start) = &options.on_start {
            on_start(id, session.id);
        }
        let mut request = json;
        if let Value::Object(fields) = &mut request {
            fields.insert("op".to_string(), Value::String(op.to_string()));
        } else {
            let mut fields = serde_json::Map::new();
            fields.insert("op".to_string(), Value::String(op.to_string()));
            fields.insert("value".to_string(), request);
            request = Value::Object(fields);
        }
        session.write(Frame::with_payload(REQUEST, id, request, options.payload));
        // Cancel-on-abort (upstream `_send`'s `onAbort` listener): the signal
        // may already be aborted when the request is written.
        let signal = options.signal.clone();
        let already_aborted = signal.as_ref().is_some_and(AbortSignal::is_aborted);
        let abort_guard = signal.map(|signal| {
            // Weak: an abort listener can live as long as the caller's signal,
            // and it must not pin the connection (or its daemon) alive.
            let inner = Arc::downgrade(&self.inner);
            let session = session.clone();
            let (done_sender, done_receiver) = oneshot::channel::<()>();
            tokio::spawn(async move {
                tokio::select! {
                    _ = signal.cancelled() => cancel_via(&inner, &session, id),
                    _ = done_receiver => {}
                }
            });
            done_sender
        });
        if already_aborted {
            session.cancel(id);
        }
        let reply = receiver
            .await
            .unwrap_or_else(|_| Err(RemoteError::lost("pi-env connection lost")));
        if let Some(done_sender) = abort_guard {
            let _ = done_sender.send(());
        }
        reply
    }

    /// Tear one session down: stop its tasks, shut its daemon down gracefully,
    /// and fail every request still in flight. Idempotent.
    fn teardown(&self, session: &Arc<Session>, error: &RemoteError) {
        teardown_session(Some(&self.inner), session, error);
    }
}

/// Initiate one session's teardown.
///
/// The forced kill is deliberately *not* sent here: the writer is stopped so
/// its `ChildStdin` drops, which is the daemon protocol's shutdown signal. The
/// reaper thread waits [`GRACEFUL_SHUTDOWN_TIMEOUT`] for the child to be
/// reaped by the exit watcher, and only then sets the kill flag — giving the
/// daemon time to cancel controls and run `kill_all` over the exec/pty process
/// groups. Idempotent: only the first caller (`live` false→true) acts.
fn teardown_session(inner: Option<&Inner>, session: &Arc<Session>, error: &RemoteError) {
    let was_live = session.live.swap(false, Ordering::SeqCst);
    // Clear the published slot even when the session was already dead: a
    // start publishes only after its tasks run, so the exit watcher may
    // have torn the session down between the handshake and that publish —
    // a dead Arc must not stay readable as the connection's session.
    if let Some(inner) = inner {
        let current = lock(&inner.session).clone();
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            *lock(&inner.session) = None;
        }
    }
    if !was_live {
        return;
    }
    // Fail every in-flight request before waiting: callers observe closure
    // immediately, not after the daemon's graceful shutdown.
    let pending = std::mem::take(&mut *lock(&session.pending));
    for (_, pending) in pending {
        let _ = pending.reply.send(Err(error.clone()));
    }
    // Graceful shutdown: stop the writer; it drops the daemon's stdin, and
    // `serve` ends on EOF, cancelling controls and killing its process groups.
    if let Some(stop) = lock(&session.stop).take() {
        let _ = stop.send(());
    }
    spawn_shutdown_reaper(session.clone());
}

/// Teardown from a task that only holds a `Weak<Inner>`: if the connection is
/// already being dropped (`Inner::drop`), that path owns the teardown.
fn teardown_via(inner: &Weak<Inner>, session: &Arc<Session>, error: &RemoteError) {
    match inner.upgrade() {
        Some(inner) => teardown_session(Some(&inner), session, error),
        None => teardown_session(None, session, error),
    }
}

/// Cancel one request from a task holding a `Weak<Inner>` (abort listener).
fn cancel_via(inner: &Weak<Inner>, session: &Arc<Session>, id: u32) {
    // `cancel` only needs the session: the request table and outbox live
    // there. Nothing to do once the session died.
    let _ = inner;
    session.cancel(id);
}

/// Wait for the daemon child to be reaped after its stdin closed; force-kill
/// it if the graceful shutdown did not finish within
/// [`GRACEFUL_SHUTDOWN_TIMEOUT`].
fn spawn_shutdown_reaper(session: Arc<Session>) {
    let counters = session.counters.clone();
    let thread_session = session.clone();
    let spawned = std::thread::Builder::new()
        .name("pir-env-shutdown".to_string())
        .spawn(move || {
            let _task = TaskGuard::new(counters);
            if !thread_session.wait_exited(GRACEFUL_SHUTDOWN_TIMEOUT) {
                thread_session.request_kill();
                let _ = thread_session.wait_exited(FORCED_KILL_TIMEOUT);
            }
        });
    if spawned.is_err() {
        // Without a reaper the daemon must not be left running: force it now.
        session.request_kill();
    }
}

/// Length of the longest suffix of `haystack` that is a prefix of `needle`.
/// Used to hold back exactly the bytes that could still become a marker after
/// the next read; the marker is short, so the quadratic worst case is bounded
/// by its length, not by the stream.
fn longest_suffix_prefix(haystack: &[u8], needle: &[u8]) -> usize {
    let max = haystack.len().min(needle.len());
    for length in (1..=max).rev() {
        if haystack[haystack.len() - length..] == needle[..length] {
            return length;
        }
    }
    0
}

/// The first index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    // `randomBytes(16).toString("hex")`, the token shape the sync line carries.
    if getrandom::fill(&mut bytes).is_err() {
        // No entropy: the token only marks the sync line, so an empty one is
        // still unambiguous (nothing else in the daemon's output is a prefix of
        // `PI-ENV \n` by accident, and the daemon is the only writer).
        return String::new();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
