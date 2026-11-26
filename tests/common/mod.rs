//! Shared test helpers. Included via `pub mod common;` from individual test
//! binaries as the crate's public test-support surface; each binary uses only
//! the pieces it needs, and `pub` keeps the unused-in-this-binary helpers
//! reachable instead of dead code.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Process-wide monotonic counter mixed into every scratch path name.
///
/// `SystemTime::now()` cannot make a temp name unique on its own: its
/// granularity on Windows is the system tick (~15.6 ms), so two threads in the
/// same merged test binary that read it inside one tick get *identical*
/// nanoseconds. Combined with a shared literal label that is a real collision
/// (`create_dir_all` is content to reuse an existing dir; `create_dir` and
/// `File::create_new` are not, and a reused dir means two tests share state).
/// A per-process atomic is the only thing that is unique without a syscall and
/// without relying on the clock (audit finding TEST-4).
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp-dir path that is unique **within this process**, with no lock.
///
/// Use for any scratch directory or file a test creates. The name carries the
/// label (so a leftover is traceable to its suite), the pid (so concurrent test
/// *binaries* cannot collide) and a process-wide sequence number (so parallel
/// tests *within* one merged binary cannot collide even in the same clock
/// tick). Prefer this over hand-rolled `temp_dir().join(format!(...))`.
pub fn unique_temp_path(label: &str) -> PathBuf {
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    // Nanoseconds are kept as a tiebreaker for a reused pid across runs; they
    // are not required for uniqueness, which the sequence number provides.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pir-{label}-{}-{seq}-{stamp}", std::process::id()))
}

/// The three spellings of the agent-dir environment variable, in precedence
/// order (`PIR_AGENT_DIR` → `PIR_CODING_AGENT_DIR` → `PI_CODING_AGENT_DIR`).
pub const AGENT_DIR_ENV_VARS: [&str; 3] = [
    "PIR_AGENT_DIR",
    "PIR_CODING_AGENT_DIR",
    "PI_CODING_AGENT_DIR",
];

/// A restore-on-drop pin of process environment variables, WITHOUT a lock.
///
/// Only for the tests whose property IS the process environment: variable
/// precedence, a default-built session following the env, a child process
/// inheriting it. Those cannot be expressed as an explicit argument, so they
/// pin the variables instead — and each such test first calls
/// [`skip_without_nextest`], so the pin is only ever applied while this
/// process runs exactly one test. That is what makes the lock unnecessary
/// rather than merely cheap.
///
/// `Drop` restores every saved name to its previous value (absence included),
/// panic included, so a failing test cannot leak a scratch directory into the
/// rest of its binary.
pub struct ProcessEnvPin {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ProcessEnvPin {
    /// Clear every name in `cleared`, then apply `pins`; all previous values
    /// are restored on drop.
    ///
    /// The pin VALUES are consumed immediately (`set_var` copies them), so only
    /// the NAMES have to outlive the call — a caller can pass a path borrowed
    /// from a local `PathBuf`.
    pub fn isolate(cleared: &[&'static str], pins: &[(&'static str, &str)]) -> Self {
        let mut saved: Vec<(&'static str, Option<OsString>)> = Vec::new();
        for key in cleared {
            saved.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
        }
        for (key, value) in pins {
            if !saved.iter().any(|(saved_key, _)| saved_key == key) {
                saved.push((key, std::env::var_os(key)));
            }
            std::env::set_var(key, value);
        }
        Self { saved }
    }

    /// Pin `PIR_AGENT_DIR` at `dir` and clear the two lower-precedence aliases,
    /// so the caller gets `dir` regardless of what the host exported.
    pub fn agent_dir(dir: &Path) -> Self {
        let value = dir.to_str().expect("temp paths are UTF-8");
        Self::isolate(&AGENT_DIR_ENV_VARS, &[("PIR_AGENT_DIR", value)])
    }
}

impl Drop for ProcessEnvPin {
    fn drop(&mut self) {
        // Reverse order so a variable set twice unwinds to its original value.
        for (key, previous) in self.saved.drain(..).rev() {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Whether this PROCESS may pin a process-global environment variable.
///
/// A few suites have a property that IS the environment: the precedence of
/// `PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR`, a
/// default-built session following the env, a child process inheriting it, a
/// `HOME`-anchored skill scan, the crash hook's live-dir read. Those cannot be
/// expressed as an explicit argument, so they pin the variables — and a
/// process-global pin only makes sense when THIS process runs one test at a
/// time.
///
/// `cargo nextest` runs one process per test, so `NEXTEST_RUN_ID` (nextest's
/// documented per-run identifier — the `insta` snapshot crate keys on it too)
/// is the proof that no sibling is concurrently reading the same variable.
/// Under a plain `cargo test` the merged harness runs suites on parallel
/// threads inside one process, where a pin is observable by everything else —
/// there the test reports a SKIP instead of passing on a value it may have
/// raced for.
///
/// `escape_var` is the explicit opt-in for a single-process run that knows
/// what it is doing (e.g. filtering to exactly one such test with
/// `--test-threads=1`). Each variable family has its own escape hatch so an
/// operator can allow the cheap pin they trust without allowing every other
/// one.
fn exclusive_process_available(escape_var: &str) -> bool {
    std::env::var_os("NEXTEST_RUN_ID").is_some() || std::env::var_os(escape_var).is_some()
}

/// The standard SKIP line for a test that pins `variable`, when the process is
/// shared with other suites and `escape_var` is unset. Returns `true` when the
/// caller should return early.
///
/// `what` names the ambient input in one clause ("re-points PIR_AGENT_DIR and
/// would break every concurrently running suite"), so the operator sees why
/// the test has to be alone rather than just which knob to turn.
fn skip_unless_exclusive_process(test_name: &str, escape_var: &str, what: &str) -> bool {
    if exclusive_process_available(escape_var) {
        return false;
    }
    eprintln!(
        "SKIP {test_name}: this test {what}. It runs under `cargo nextest` (CI); to run it \
         under plain `cargo test`, filter to this test and set {escape_var}=1."
    );
    true
}

/// [`skip_unless_exclusive_process`] for the agent-dir variable family.
pub fn skip_without_nextest(test_name: &str) -> bool {
    skip_unless_exclusive_process(
        test_name,
        "PIR_ALLOW_SHARED_PROCESS_AGENT_DIR",
        "re-points the process-wide agent-dir environment and would break every concurrently \
         running suite",
    )
}

/// [`skip_unless_exclusive_process`] for the `HOME` variable.
pub fn skip_without_nextest_home(test_name: &str) -> bool {
    skip_unless_exclusive_process(
        test_name,
        "PIR_ALLOW_SHARED_PROCESS_HOME",
        "re-points the process-wide HOME and would break every concurrently running suite",
    )
}

/// [`skip_unless_exclusive_process`] for the temporary-directory variables.
pub fn skip_without_nextest_temp(test_name: &str) -> bool {
    skip_unless_exclusive_process(
        test_name,
        "PIR_ALLOW_SHARED_PROCESS_TEMP",
        "re-points the process-wide temporary directory and would break every concurrently \
         running suite",
    )
}

/// [`skip_unless_exclusive_process`] for `PI_PACKAGE_DIR`.
pub fn skip_without_nextest_package(test_name: &str) -> bool {
    skip_unless_exclusive_process(
        test_name,
        "PIR_ALLOW_SHARED_PROCESS_PACKAGE_DIR",
        "re-points the process-wide package dir and would break every concurrently running \
         suite",
    )
}

/// Set the process-wide `PI_OFFLINE=1` flag.
///
/// This is the only env variable a large family of builder tests needs: they
/// pass an explicit `.agent_dir()` and only want the remote-catalog refresh
/// disabled. The value is constant and idempotent, so the mutation does not
/// need to exclude `PIR_AGENT_DIR` pins (different variable) and nothing has
/// to restore it afterwards.
pub fn ensure_offline_mode() {
    // Constant, idempotent value: re-set on every call so a preceding test's
    // env restore cannot leave the flag unset.
    std::env::set_var("PI_OFFLINE", "1");
}

/// An agent dir a test owns and passes explicitly (see
/// [`scratch_agent_dir`]). Removed on drop, panic included.
pub struct TempAgentDir(pub PathBuf);

impl TempAgentDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempAgentDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Create a fresh scratch agent dir, optionally with a `settings.json`.
///
/// The directory is returned WITHOUT touching the process environment: the
/// caller hands it to its session/builder explicitly (`.agent_dir(..)`,
/// `new_with_agent_dir`, `PluginHost::build_with_agent_dir`). An explicit
/// directory cannot race another test, so nothing here needs a lock and
/// `PIR_AGENT_DIR` is left alone.
pub fn scratch_agent_dir(label: &str, settings_json: Option<&str>) -> TempAgentDir {
    let dir = unique_temp_path(&format!("test-agent-dir-{label}"));
    std::fs::create_dir_all(&dir).expect("create scratch agent dir");
    if let Some(settings) = settings_json {
        std::fs::write(dir.join("settings.json"), settings).expect("write settings");
    }
    TempAgentDir(dir)
}

/// The ONE process-wide lock for tests that touch the **global Codex
/// WebSocket pool** (`codex_ws::close_codex_websocket_sessions`), which is a
/// `static` shared by every suite in a merged test binary.
///
/// Most codex suites are already isolated: each pools under its own session id
/// and tears down only that id. The exception is a *global* close
/// (`close_codex_websocket_sessions(None)`), which retires **every** pooled
/// connection and every lease in flight, including other suites' — that is
/// exactly the production teardown-all semantics, so the pool cannot make it
/// harmless. A global close landing between two turns of another suite drops
/// that suite's pooled connection; its next turn then handshakes fresh and a
/// scripted server re-serves turn 1 (`resp_1` where `resp_2` was expected —
/// `ai_codex_ws_w10::codex_ws_handshake_headers_…`, CL36).
///
/// So: a test that must observe pool continuity (reuse across turns) holds
/// [`codex_ws_pool_guard`], and a test that must wipe the whole pool holds it
/// too. Session-scoped closes deliberately do NOT take it — that keeps the
/// lock off every test's path and serializes only the genuinely global case.
///
/// This is a **tokio** mutex because every holder awaits inside the critical
/// section (the pooled socket is released by an async transport task).
pub fn codex_ws_pool_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Hold the process-wide Codex WebSocket pool for a whole test: pool
/// continuity and global wipes are mutually exclusive across every suite in
/// this binary.
pub async fn codex_ws_pool_guard() -> tokio::sync::MutexGuard<'static, ()> {
    codex_ws_pool_lock().lock().await
}

/// The ONE process-wide lock every test that RESETS and then reads the TUI
/// frame counters (`pir::tui::{RowsWork, ToolMemoWork}`) must take.
///
/// Those counters are single process-global atomics (`ROWS_WORK`,
/// `TOOL_MEMO_WORK`), not thread-locals: `RowsWork::reset()` zeroes the same
/// seven atomics every renderer thread increments, and a renderer only bumps
/// them when its own thread is measuring. So a concurrent render — any other
/// TUI test drawing *any* frame — adds its own `pool_growth`/`row_writes`
/// between one test's `reset()` and `snapshot()`, and a steady-frame assertion
/// reads a non-zero pool growth it never caused
/// (`tui_frame_reuse::user_bash_block_serves_its_whole_row_set_from_the_memo`
/// failed exactly this way under `cargo test --test zz_merged_2`).
///
/// As with [`codex_ws_pool_lock`], a lock declared inside one suite module only
/// serializes that module: `tui_frame_reuse` and `tui_windowed_transcript`
/// each had their own `static TEST_LOCK`, so they did not exclude each other.
/// Declaring it here (once per test binary) makes every frame-measuring suite in
/// the binary share one lock. Separate binaries are separate processes, so the
/// per-binary copy cannot race across harnesses.
///
/// This is a **std** mutex: every holder is a synchronous `#[test]` rendering
/// frames (no `.await` inside the critical section), so an async lock would
/// only add a runtime dependency.
pub fn tui_frame_counter_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Wait until the producer side of the Codex WebSocket transport has returned
/// the previous turn's socket to the process-global pool.
///
/// The terminal `Done` event is pushed to the consumer **before** the transport
/// releases its lease and inserts the socket, so a follow-up turn started
/// immediately after `Done` can handshake fresh and lose the pooled
/// continuation. Tests that pin pool continuity must wait on the production
/// pool instead of sleeping for a fixed interval: a fixed delay is a guess
/// that loses under CI load.
pub async fn wait_for_pooled_codex_websocket(session_id: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while pir::ai::api::openai_responses::codex::codex_ws::pooled_websocket_count(session_id) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session {session_id} never returned its WebSocket to the pool"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// A local HTTP endpoint that answers every request with an immediate 401:
/// failed prompts in sandboxed CLI/RPC tests must be FAST (auth errors are
/// non-retryable), while an unreachable endpoint (127.0.0.1:1) stalls ~8s in
/// retry backoff. Bound once per test binary on an OS-assigned port.
pub fn sandbox_endpoint() -> &'static str {
    use std::sync::OnceLock;
    static ENDPOINT: OnceLock<String> = OnceLock::new();
    ENDPOINT.get_or_init(|| {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind sandbox listener");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Consume the request head BEFORE answering. Writing first races
                // the client's request write; hyper then rejects the unsolicited
                // response with `UnexpectedMessage`, which surfaces as a SEND
                // error — retryable, so the client backs off ~8s. That is the
                // exact slow path this helper exists to prevent (CL36 measured
                // it on the sibling write-first server in
                // `ai_transport_idle`), so answer only after the request is read.
                read_request_head(&mut stream);
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n{\"error\":{\"message\":\"sandbox auth rejected\"}}",
                );
                // The connection closes when `stream` drops.
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    })
}

/// Read one HTTP request head off `stream` (through the blank line), so a mock
/// server can answer in order. Returns `false` if the peer closed first.
///
/// A server that writes its response as soon as it accepts races the client's
/// request write: hyper reads a response for a request it has not dispatched,
/// fails the send with `UnexpectedMessage`, and the client reports
/// "error sending request" instead of the scripted answer (measured ~8% under
/// parallel load on Windows; 0 in ~4.8k requests with this read first).
pub fn read_request_head(stream: &mut std::net::TcpStream) -> bool {
    use std::io::Read;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    true
}

/// Publish `contents` at `path` ATOMICALLY (write a sibling temp file, then
/// rename over the destination).
///
/// Mock MCP children report observed notifications by writing a small file that
/// the parent polls and immediately parses. A plain `fs::write` truncates the
/// destination before writing, so a poll landing in that window reads an empty
/// file and the parse fails with `json: EOF` — the recorded
/// `a_timed_out_request_is_cancelled_on_the_server` flake. A rename is atomic on
/// both Windows and Unix, so a reader sees either the old file or the whole new
/// one, never a partial write.
pub fn write_file_atomically(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);
    if std::fs::write(&temp, contents).is_err() {
        return;
    }
    // `rename` replaces an existing destination on both platforms (Windows
    // `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` under `fs::rename`).
    let _ = std::fs::rename(&temp, path);
}

/// Wait for an external process to make `path` satisfy `check`, **without
/// sleeping**.
///
/// Tests must not use `sleep` as a synchronisation primitive: a sleep is either
/// too short (flaky under load) or too long (slow), and it hides the event the
/// test actually depends on. This blocks on the same OS facility production
/// uses — a filesystem watch over the *parent directory* (`notify`, the crate
/// behind `fs.watch`) — and re-runs `check` on every relevant event.
///
/// `check` is evaluated once before waiting, so a producer that already
/// finished cannot deadlock the waiter.
///
/// `timeout` is a *failure bound*, not a delay: it only fires when the producer
/// never delivers, and the panic names the path that was being waited on.
///
/// Watching the parent (not the file) is deliberate: a producer that creates
/// the file, or replaces it by rename (`write_file_atomically`), never emits an
/// event addressed to a path that did not exist when the watch started — but a
/// directory watch reports the creation/rename regardless.
pub fn wait_for_file<T>(path: &Path, timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    use notify::{RecursiveMode, Watcher};

    if let Some(value) = check() {
        return value;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    // A watched directory that does not exist yet cannot be watched; create it
    // so a producer writing into it still wakes us. This is the only case where
    // the test itself may create the parent.
    let _ = std::fs::create_dir_all(dir);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        // A send failure means the waiter is gone; dropping the event is fine.
        let _ = tx.send(event);
    })
    .expect("create fs watcher");
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .expect("watch the waiting directory");

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return value;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for {}", path.display());
        }
        // Either an fs event or the deadline, whichever comes first: no sleep.
        match rx.recv_timeout(remaining) {
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for {}", path.display())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("fs watcher stopped while waiting for {}", path.display())
            }
        }
    }
}
