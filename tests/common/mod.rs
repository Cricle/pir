//! Shared test helpers. Included via `mod common;` from individual test
//! binaries; everything is `#[allow(dead_code)]` because each binary only
//! uses the pieces it needs.
#![allow(dead_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The ONE process-wide lock every test that mutates `PIR_AGENT_DIR` (or
/// its `PI_CODING_AGENT_DIR` alias) must take.
///
/// The environment is process-global, while a merged test binary runs its
/// test functions on parallel threads. A lock declared in a suite module only
/// serializes that module's tests: two suites holding two different `static`
/// mutexes do NOT exclude each other, so one suite can observe the scratch
/// agent directory pinned by another (the `NoModel { "…/w8d-other" not found }`
/// interleaving failure in `logs/w13/g_agent.log`).
///
/// `tests/common/mod.rs` is included exactly once per test binary (`mod
/// common;` at the binary's crate root), so the static below is shared by
/// every suite module inside that binary. The copy is duplicated per binary,
/// which is correct: separate test binaries are separate processes with
/// separate environments, so they can never race.
///
/// This is a **tokio** mutex rather than `std::sync::Mutex` because every
/// guard here is held across `.await` points (the pinned directory must stay
/// in place for the whole async test): the async lock keeps the test future
/// `Send` and avoids `clippy::await_holding_lock`. The critical sections are
/// instant env mutations, so contention costs nothing but scheduling.
pub fn agent_dir_env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Set the process-wide `PI_OFFLINE=1` flag without taking the
/// [`agent_dir_env_lock`] for the rest of the test.
///
/// This is the only env variable a large family of builder tests needs: they
/// pass an explicit `.agent_dir()` and only want the remote-catalog refresh
/// disabled. The value is constant and idempotent, so the mutation does not
/// need to exclude `PIR_AGENT_DIR` pins (different variable), and the test no
/// longer holds the binary-wide lock across its async builds — which was the
/// dominant source of multi-second queueing inside `g_agent`.
pub fn ensure_offline_mode() {
    // Constant, idempotent value: re-set on every call so a preceding test's
    // env restore cannot leave the flag unset.
    std::env::set_var("PI_OFFLINE", "1");
}

/// RAII pin of the process-global `PIR_AGENT_DIR`, serialized by
/// [`agent_dir_env_lock`]: restores the previous value (or absence) of the
/// pinned variables on drop, panic included, so a failing test cannot leak a
/// scratch agent directory into its neighbours.
pub struct AgentDirEnvGuard {
    /// `Drop::drop` (which restores the environment) runs before any field
    /// is dropped, so the restore stays serialized with the pin: the lock is
    /// still held while the previous values go back.
    _lock: tokio::sync::MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
    owned_dir: Option<PathBuf>,
}

impl AgentDirEnvGuard {
    /// Pin `PIR_AGENT_DIR` at a fresh scratch directory, removed on drop.
    /// `settings_json`, when given, is written to `<dir>/settings.json`
    /// before the pin is published.
    pub async fn scratch(label: &str, settings_json: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-test-agent-dir-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create pinned agent dir");
        if let Some(settings) = settings_json {
            std::fs::write(dir.join("settings.json"), settings).expect("write pinned settings");
        }
        let mut guard = Self::acquire(&dir, &[]).await;
        guard.owned_dir = Some(dir);
        guard
    }

    /// Pin `PIR_AGENT_DIR` at an existing directory (the caller owns its
    /// cleanup) and set `extra` variables; everything is restored on drop.
    pub async fn at(dir: impl AsRef<Path>, extra: &[(&'static str, &str)]) -> Self {
        Self::acquire(dir.as_ref(), extra).await
    }

    /// The scratch directory this guard owns, when it created one.
    pub fn dir(&self) -> Option<&Path> {
        self.owned_dir.as_deref()
    }

    async fn acquire(dir: &Path, extra: &[(&'static str, &str)]) -> Self {
        let lock = agent_dir_env_lock().lock().await;
        let mut saved = Vec::with_capacity(1 + extra.len());
        saved.push(("PIR_AGENT_DIR", std::env::var_os("PIR_AGENT_DIR")));
        std::env::set_var("PIR_AGENT_DIR", dir);
        for (key, value) in extra {
            saved.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self {
            _lock: lock,
            saved,
            owned_dir: None,
        }
    }
}

impl Drop for AgentDirEnvGuard {
    fn drop(&mut self) {
        // Reverse order so a variable set twice unwinds to its original value.
        for (key, previous) in self.saved.drain(..).rev() {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        if let Some(dir) = self.owned_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
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
/// This is a **tokio** mutex for the same reason as [`agent_dir_env_lock`]:
/// every holder awaits inside the critical section.
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
