//! Fallible writes to the process stdio for the CLI hosts.
//!
//! Rust's `print!`/`println!`/`eprintln!` macros PANIC when the underlying
//! write fails. That is the wrong contract for a CLI: a downstream consumer
//! closing its end of a pipe (`pir print --mode json … | head -5`,
//! `… 2>&1 | head -5`) is ordinary Unix behavior, not an internal error.
//! The standard library reports it as `BrokenPipe` (Windows `ERROR_BROKEN_PIPE`,
//! os error 232), and the panic would additionally be recorded in
//! `crashes.json` by the process panic hook and shown as a crash banner in the
//! next TUI session.
//!
//! The helpers here are the single fallible write path for the print/json/rpc
//! hosts: a broken stdout pipe ends the process quietly (exit 0, the
//! conventional "consumer went away" status), while any other stdout write
//! failure is reported once on stderr and exits 1. Stderr writes never panic;
//! when stderr itself is gone the diagnostic is simply dropped.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a stdout write reported `BrokenPipe`; the RPC serve loop consults
/// it to distinguish a consumer disconnect from a genuine writer failure.
static STDOUT_BROKEN: AtomicBool = AtomicBool::new(false);

/// Whether a write error means the reader closed the pipe. The raw OS codes
/// cover platforms/toolchains whose `ErrorKind` mapping is less direct:
/// `EPIPE` (32) on Unix; `ERROR_BROKEN_PIPE` (109) and `ERROR_NO_DATA`
/// (232, "the pipe is being closed") on Windows.
pub fn is_broken_pipe(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::BrokenPipe
        || (cfg!(windows) && matches!(error.raw_os_error(), Some(109) | Some(232)))
        || (cfg!(unix) && error.raw_os_error() == Some(32))
}

/// Record that stdout is gone. Idempotent.
pub fn mark_stdout_broken() {
    STDOUT_BROKEN.store(true, Ordering::SeqCst);
}

/// Whether [`mark_stdout_broken`] was recorded in this process.
pub fn stdout_is_broken() -> bool {
    STDOUT_BROKEN.load(Ordering::SeqCst)
}

/// Write `text` as one stdout line. All write errors are returned; callers on
/// the CLI path should pass them to [`exit_stdout_error`].
pub fn write_stdout_line(text: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(text.as_bytes())?;
    handle.write_all(b"\n")?;
    handle.flush()
}

/// Write `text` to stdout verbatim (no trailing newline), the fallible
/// counterpart of `print!`.
pub fn write_stdout_text(text: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(text.as_bytes())?;
    handle.flush()
}

/// Write `text` as one stderr line without ever panicking. Write failures are
/// intentionally swallowed: there is no third stream to report them on, and a
/// closed stderr must not turn a diagnostic into a crash.
pub fn write_stderr_line(text: &str) -> io::Result<()> {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    handle.write_all(text.as_bytes())?;
    handle.write_all(b"\n")?;
    handle.flush()
}

/// Terminal disposition of a failed stdout write on the CLI path: a closed
/// consumer pipe exits 0 (quietly), every other I/O error is reported on
/// stderr and exits 1. Diverges, so call sites can use it in `if let`/`match`
/// expressions without an extra `return`.
pub fn exit_stdout_error(error: io::Error) -> ! {
    if is_broken_pipe(&error) {
        mark_stdout_broken();
        std::process::exit(0);
    }
    let _ = write_stderr_line(&format!("pir: cannot write to stdout: {error}"));
    std::process::exit(1);
}

/// Whether an `anyhow` error chain wraps a broken-pipe I/O error. Used by the
/// RPC writer path, whose errors are boxed while crossing the writer thread.
pub fn is_broken_pipe_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(is_broken_pipe)
    })
}

/// A stderr writer for `tracing` that never reports a write error.
///
/// `tracing-subscriber` reacts to a writer error with its OWN `eprintln!`,
/// which panics when stderr is the closed stream. The CLI log sink must not
/// be able to crash the process: diagnostics are best-effort, so a failed
/// write is dropped here (the same policy as [`write_stderr_line`]).
pub struct LossyStderr;

impl Write for LossyStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        Ok(())
    }
}

/// `MakeWriter`-compatible factory for [`LossyStderr`] (a function pointer
/// `Fn() -> W` satisfies `tracing_subscriber::fmt::MakeWriter`).
pub fn lossy_stderr_writer() -> LossyStderr {
    LossyStderr
}
