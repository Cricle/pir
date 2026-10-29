//! The stdio transport on top of the official `rmcp` SDK (upstream
//! `packages/mcp/src/transports/stdio.ts`).
//!
//! rmcp's framing (line-delimited JSON-RPC, invalid-line tolerance) is reused
//! through its async-read/write transport. What this module adds on top:
//!
//! - a **bounded frame reader**: the bytes buffered for one JSON-RPC line are
//!   capped (default 1 MiB, [`StdioSpawnOptions::max_message_bytes`]) and the
//!   reader grows incrementally — it never preallocates a declared size and
//!   never buffers more than one 8 KiB chunk beyond the cap. On overflow it
//!   records the violation, stops reading, kills the server's process tree
//!   with [`terminate_process_tree`], and returns an `InvalidData` I/O error
//!   naming the cap. rmcp then closes the service; the client maps the
//!   pending requests to [`super::error::McpError::MessageTooLarge`] using
//!   the shared [`FrameCapState`], so in-flight calls fail instead of
//!   hanging;
//! - pir's spawn environment: resolved env, `~` expansion (done by the
//!   manager), CREATE_NO_WINDOW on Windows and an own process group on Unix,
//!   so the shutdown tree kill reaches children spawned by `.cmd`/`npx`/`uvx`
//!   shims;
//! - stderr is piped (never inherited), drained continuously into a bounded
//!   tail and mirrored to the MCP log (upstream `handleStderr`).
//!
//! The child process is owned by a `ChildGuard` inside the reader: when the
//! rmcp service drops the transport (close, read error, timeout) the guard
//! runs the same process-tree kill and reaps the direct child, so an
//! overflowing server cannot outlive the connection.
//!
//! [`super::transport`] documents the matching HTTP/SSE bound.

use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::error::McpError;
use super::log::McpLog;
use super::transport::{frame_cap_label, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_MAX_STDERR_BYTES};

/// Size of one incremental read: bounds the extra buffering beyond the frame
/// cap and keeps allocation independent of the frame's declared content.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// How long the graceful tree signal gets before the forced one. Short on
/// purpose: a server that ignores SIGTERM must not hold the process tree open.
/// [`ChildGuard::drop`] treats it as a deadline and returns as soon as the
/// direct child exits (a cooperative server: ~10–50 ms), so the full window is
/// only paid by a server that actually ignores the signal.
const TREE_KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// Send one termination signal to `pid`'s whole process group. On Windows
/// there are no graceful signals, so both phases use `taskkill /T /F`
/// (`.cmd`/`.bat` shims run through `cmd.exe`, and killing only that would
/// leave the server running); on Unix the child was spawned in its own
/// process group, so a group-wide signal reaches wrappers like `npx`/`uvx`
/// and the server they started.
fn signal_process_tree(pid: u32, signal: i32) {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, signal);
    }
    #[cfg(windows)]
    {
        let _ = signal;
        use std::os::windows::process::CommandExt;
        // Trusted absolute path: a bare `taskkill` could resolve to a planted
        // executable on PATH (or the current directory). CREATE_NO_WINDOW:
        // the kill helper must not flash a console.
        let system_root = std::env::var_os("SystemRoot")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));
        let _ = std::process::Command::new(
            std::path::Path::new(&system_root)
                .join("System32")
                .join("taskkill.exe"),
        )
        .args(["/pid", &pid.to_string(), "/T", "/F"])
        .env("NoDefaultCurrentDirectoryInExePath", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .status();
    }
    #[cfg(unix)]
    {
        // Negative pid: the whole group. ESRCH (group already gone) is fine.
        #[expect(
            unsafe_code,
            reason = "libc::killpg on the spawned server's process group"
        )]
        // SAFETY: `pid` is the spawned MCP server's pid (it leads its own
        // process group); `killpg` takes a plain integer id and no pointers,
        // and an already-gone group is a harmless `ESRCH`.
        unsafe {
            let _ = libc::killpg(pid as libc::pid_t, signal);
        }
    }
}

/// Send the graceful phase of the tree kill (SIGTERM on Unix; on Windows the
/// one available phase is already `taskkill /T /F`).
fn signal_process_tree_gracefully(pid: u32) {
    #[cfg(unix)]
    signal_process_tree(pid, libc::SIGTERM);
    #[cfg(not(unix))]
    signal_process_tree(pid, 0);
}

/// Kill `pid`'s whole process tree with the graceful signal, then escalate to
/// SIGKILL once [`TREE_KILL_GRACE`] expires (upstream `killProcessTree` plus
/// the force phase): a server that ignores SIGTERM (or a shell wrapper whose
/// grandchild does) must not survive the connection. The grace stays inside
/// the `CLOSE_TIMEOUT` close budget.
pub fn terminate_process_tree(pid: u32) {
    signal_process_tree_gracefully(pid);
    std::thread::sleep(TREE_KILL_GRACE);
    terminate_process_tree_force(pid);
}

/// The force phase alone (an oversized frame / an already-expired grace):
/// SIGKILL the process group on Unix, `taskkill /T /F` on Windows.
pub fn terminate_process_tree_force(pid: u32) {
    #[cfg(unix)]
    signal_process_tree(pid, libc::SIGKILL);
    #[cfg(windows)]
    signal_process_tree(pid, 0);
}

/// Shared per-connection overflow state: a bounded reader (the stdio frame
/// reader or the HTTP client's plain-JSON body reader) records the violation,
/// the client checks it when rmcp reports the transport closed, and turns the
/// generic close into a typed [`McpError::MessageTooLarge`].
#[derive(Debug)]
pub struct FrameCapState {
    limit: usize,
    overflowed: AtomicBool,
}

impl FrameCapState {
    /// A fresh cap state. Shared by the stdio frame reader and the HTTP
    /// client's plain-JSON body reader (both enforce the same message bound
    /// and report overflow through the generic request path).
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            overflowed: AtomicBool::new(false),
        })
    }

    /// The configured per-frame byte cap.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Whether the reader hit the cap on this connection.
    pub fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_overflowed(&self) {
        // SeqCst pairs with the client's check after rmcp observes the read
        // error: the flag is visible before the transport can close.
        self.overflowed.store(true, Ordering::SeqCst);
    }
}

/// Owns the spawned child for the transport's lifetime. Dropping the
/// transport (graceful close, read error, handshake timeout, client drop)
/// sweeps the process tree and reaps the direct child.
struct ChildGuard {
    child: Option<Child>,
    pid: Option<u32>,
}

impl ChildGuard {
    fn new(child: Child, pid: Option<u32>) -> Self {
        Self {
            child: Some(child),
            pid,
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // The tree sweep reaches `.cmd`/`npx`/`uvx` grandchildren; the direct
        // kill then guarantees this child exits even if it ignores the
        // graceful signal.
        //
        // The grace is a DEADLINE, not a fixed sleep: on Windows the tree
        // signal is already `taskkill /T /F`, and on Unix a well-behaved
        // server exits on SIGTERM within a few milliseconds. Poll the direct
        // child and escalate as soon as it is gone; a server that ignores the
        // signal still gets the full [`TREE_KILL_GRACE`] before the forced
        // sweep, exactly like [`terminate_process_tree`]. Measured: mock stdio
        // servers exit in ~10–50 ms, so every close used to waste ~200 ms
        // (66 `mcp_integration` tests × 1–2 closes).
        if let Some(pid) = self.pid {
            signal_process_tree_gracefully(pid);
            let deadline = std::time::Instant::now() + TREE_KILL_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        std::thread::sleep(left.min(std::time::Duration::from_millis(10)));
                    }
                    Err(_) => break,
                }
            }
            terminate_process_tree_force(pid);
        }
        let _ = child.start_kill();
        // Reap off the runtime worker: `Drop` cannot await. Tokio also reaps
        // children best-effort on drop, but an explicit wait is deterministic
        // for tests that assert the pid disappeared.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = child.wait().await;
                });
            }
            Err(_) => {
                let _ = child.try_wait();
            }
        }
    }
}

/// The bounded stdout half of a stdio MCP connection.
///
/// Delivers at most `limit` bytes between two `\n` delimiters (plus the
/// delimiter itself); the read chunk is bounded to 8 KiB, so growth is
/// incremental and no allocation is driven by a frame's content.
pub struct StdioReader {
    inner: ChildStdout,
    limit: usize,
    line_bytes: usize,
    state: Arc<FrameCapState>,
    pid: Option<u32>,
    _guard: ChildGuard,
}

impl StdioReader {
    fn overflow_error(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "MCP stdio frame exceeds the {}-byte ({}) cap; terminating the server",
                self.limit,
                frame_cap_label(self.limit)
            ),
        )
    }

    fn record_overflow(&self) {
        self.state.mark_overflowed();
        if let Some(pid) = self.pid {
            // A frame-cap violation is not a graceful shutdown: force the
            // tree immediately so the reader does not keep buffering (and the
            // poll is not blocked for the grace period).
            terminate_process_tree_force(pid);
        }
    }
}

impl AsyncRead for StdioReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let available = buf.remaining();
        if available == 0 {
            return Poll::Ready(Ok(()));
        }
        // One byte past the remaining budget lets a frame of exactly `limit`
        // bytes end in `\n` without being rejected as oversized.
        let remaining = this.limit.saturating_sub(this.line_bytes);
        let want = available
            .min(READ_CHUNK_BYTES)
            .min(remaining.saturating_add(1));
        let mut scratch = [0u8; READ_CHUNK_BYTES];
        let mut limited = ReadBuf::new(&mut scratch[..want]);
        match Pin::new(&mut this.inner).poll_read(cx, &mut limited) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {}
        }
        let filled = limited.filled();
        if filled.is_empty() {
            // EOF: a trailing partial frame is left to rmcp's codec, exactly
            // like the unbounded reader it replaces.
            return Poll::Ready(Ok(()));
        }
        // Track the current frame's length across chunk boundaries and
        // multiple frames per chunk. A newline resets the count.
        let mut line_bytes = this.line_bytes;
        let mut overflowed = false;
        for byte in filled {
            if *byte == b'\n' {
                line_bytes = 0;
            } else {
                line_bytes += 1;
                if line_bytes > this.limit {
                    overflowed = true;
                    break;
                }
            }
        }
        if overflowed {
            this.record_overflow();
            return Poll::Ready(Err(this.overflow_error()));
        }
        this.line_bytes = line_bytes;
        buf.put_slice(filled);
        Poll::Ready(Ok(()))
    }
}

/// The two halves handed to rmcp's async-read/write transport.
pub type StdioTransport = (StdioReader, ChildStdin);

/// One spawned stdio MCP server, ready to be handed to the rmcp client
/// service. `child_pid` lets the client sweep the whole tree after shutdown;
/// `frame_cap` carries the reader's overflow flag to the request path.
pub struct StdioConnection {
    pub transport: StdioTransport,
    pub child_pid: Option<u32>,
    /// Bounded tail of the server's stderr. Retained for the upstream
    /// connect-failure diagnostics seam (`stderrTail` enriches the failure
    /// message in runtime.ts `connectFailed`/`handleClientClose`); the drain
    /// also mirrors every line to the MCP log live.
    pub stderr_tail: Arc<Mutex<Vec<u8>>>,
    /// Overflow flag shared with the bounded reader.
    pub frame_cap: Arc<FrameCapState>,
}

/// Options for spawning a stdio MCP server (upstream's stdio transport
/// options).
#[derive(Debug, Clone)]
pub struct StdioSpawnOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    /// Resolved environment merged over the inherited environment.
    pub env: BTreeMap<String, String>,
    /// Hard cap on the bytes buffered for one JSON-RPC frame (the default is
    /// [`DEFAULT_MAX_MESSAGE_BYTES`], 1 MiB). The reader enforces it; a frame
    /// that exceeds it is a transport error and the server is terminated.
    pub max_message_bytes: usize,
}

impl Default for StdioSpawnOptions {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            env: BTreeMap::new(),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

/// Spawn the server process, start its stderr drain, and return the bounded
/// stdio transport for the client service (upstream
/// `createDefaultTransport`'s stdio branch).
///
/// `env` entries are resolved by the caller (config value resolution) and
/// merged over the inherited environment, matching upstream's
/// `{ ...process.env, ...env }`. `log` receives the stderr stream.
pub fn spawn(
    options: StdioSpawnOptions,
    log: Option<Arc<McpLog>>,
    server_name: &str,
) -> Result<StdioConnection, McpError> {
    let limit = options.max_message_bytes;
    let mut command = tokio::process::Command::new(&options.command);
    command
        .args(options.args.iter().map(String::as_str))
        .current_dir(&options.cwd)
        .envs(options.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: server consoles must not flash on Windows.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        // Own process group, so the tree kill on shutdown reaches the
        // server's children too (upstream `detached: USE_PROCESS_GROUPS`).
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(McpError::from)?;
    let child_pid = child.id();
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.start_kill();
            return Err(McpError::Protocol(
                "MCP stdio server was spawned without a stdin pipe".to_owned(),
            ));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.start_kill();
            return Err(McpError::Protocol(
                "MCP stdio server was spawned without a stdout pipe".to_owned(),
            ));
        }
    };
    let stderr = child.stderr.take();
    let frame_cap = FrameCapState::new(limit);
    let reader = StdioReader {
        inner: stdout,
        limit,
        line_bytes: 0,
        state: Arc::clone(&frame_cap),
        pid: child_pid,
        _guard: ChildGuard::new(child, child_pid),
    };
    let stderr_tail = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = stderr {
        spawn_stderr_drain(
            stderr,
            Arc::clone(&stderr_tail),
            log,
            server_name.to_owned(),
        );
    }
    Ok(StdioConnection {
        transport: (reader, stdin),
        child_pid,
        stderr_tail,
        frame_cap,
    })
}

/// Drain the server's stderr: retain a bounded tail and mirror every line to
/// the MCP log (upstream `handleStderr`).
fn spawn_stderr_drain(
    mut stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<Vec<u8>>>,
    log: Option<Arc<McpLog>>,
    server_name: String,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        // A real (sized) buffer: `read` hands the whole slice to the OS, and
        // an empty one reports immediate EOF, which would end the drain at
        // once, drop the read end of the child's stderr pipe, and make the
        // server's next stderr write fail (EPIPE on a Node server is fatal).
        let mut buffer = [0u8; 8192];
        loop {
            // Read one chunk; stderr is line-oriented but never parsed, so a
            // fixed-size chunk drain keeps the tail bounded either way.
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let chunk = &buffer[..read];
                    {
                        let mut tail = tail
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tail.extend_from_slice(chunk);
                        if tail.len() > DEFAULT_MAX_STDERR_BYTES {
                            let drop = tail.len() - DEFAULT_MAX_STDERR_BYTES;
                            tail.drain(..drop);
                        }
                    }
                    if let Some(log) = &log {
                        let text = String::from_utf8_lossy(chunk).into_owned();
                        log.write_stderr(&server_name, &text);
                    }
                }
            }
        }
    });
}
