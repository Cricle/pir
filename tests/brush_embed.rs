//! `pir __brush` end-to-end: the embedded brush shell's `-c` mode, the stdin
//! command loop the shell pool drives, and the sentinel framing the pool
//! depends on.
//!
//! Every child process runs under an explicit deadline; a hung shell is
//! killed so the test fails instead of blocking the suite.

#![cfg(feature = "brush")]

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Generous enough for a cold debug binary on CI, small enough that a wedged
/// shell cannot stall the suite.
const TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);

/// The built binary. `option_env!` keeps the file compilable in contexts where
/// cargo does not export the variable (e.g. a plain `cargo check --lib`
/// workspace pass); the tests then skip instead of failing.
fn pir_bin() -> Option<&'static str> {
    option_env!("CARGO_BIN_EXE_pir")
}

/// Read a pipe to EOF on a thread: a child that writes more than a pipe
/// buffer never blocks, and the caller polls the shared buffer.
fn read_pipe(
    pipe: Option<impl Read + Send + 'static>,
) -> (Arc<Mutex<Vec<u8>>>, Option<std::thread::JoinHandle<()>>) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buffer);
    let handle = pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&chunk[..n]),
                }
            }
        })
    });
    (buffer, handle)
}

fn snapshot(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn wait_deadline(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(error) => panic!("wait failed: {error}"),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Run `pir __brush <args>` to completion with stdin closed.
fn run_brush(args: &[&str]) -> Option<(ExitStatus, String, String)> {
    let bin = pir_bin()?;
    let mut child = Command::new(bin)
        .arg("__brush")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pir __brush");
    let (stdout, out_handle) = read_pipe(child.stdout.take());
    let (stderr, err_handle) = read_pipe(child.stderr.take());
    let status = wait_deadline(&mut child).expect("pir __brush timed out");
    if let Some(handle) = out_handle {
        let _ = handle.join();
    }
    if let Some(handle) = err_handle {
        let _ = handle.join();
    }
    Some((
        status,
        String::from_utf8_lossy(&snapshot(&stdout)).into_owned(),
        String::from_utf8_lossy(&snapshot(&stderr)).into_owned(),
    ))
}

fn code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Is `name` (or `name.exe`) on `PATH`? Used to skip the pipeline test on
/// hosts with no `tr`.
fn has_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_owned()]
    } else {
        vec![name.to_owned()]
    };
    std::env::split_paths(&path).any(|dir| {
        candidates
            .iter()
            .any(|candidate| dir.join(candidate).is_file())
    })
}

#[test]
fn dash_c_runs_and_exits_zero() {
    let Some((status, stdout, stderr)) = run_brush(&["-c", "echo hi"]) else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    assert_eq!(code(status), 0, "stderr: {stderr}");
    assert!(stdout.contains("hi"), "stdout: {stdout:?}");
}

#[test]
fn dash_c_propagates_exit_status() {
    let Some((status, stdout, stderr)) = run_brush(&["-c", "exit 7"]) else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    assert_eq!(code(status), 7, "stdout: {stdout:?} stderr: {stderr:?}");
}

#[test]
fn dash_c_arithmetic_expansion() {
    let Some((status, stdout, stderr)) = run_brush(&["-c", "x=1; echo $((x+1))"]) else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    assert_eq!(code(status), 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "2", "stdout: {stdout:?}");
}

#[test]
fn dash_c_pipeline() {
    if !has_on_path("tr") {
        eprintln!("skipping: no `tr` on PATH");
        return;
    }
    let Some((status, stdout, stderr)) = run_brush(&["-c", "echo abc | tr a-z A-Z"]) else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    assert_eq!(code(status), 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "ABC", "stdout: {stdout:?}");
}

/// The pooled-member shape: several complete programs on one stdin stream,
/// each ending with the pool's sentinel `printf`. The shell must run them in
/// order, keep running between them, and exit cleanly at EOF.
#[test]
fn stdin_mode_keeps_running_between_commands() {
    let Some(bin) = pir_bin() else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    let mut child = Command::new(bin)
        .arg("__brush")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pir __brush");
    let (stdout, out_handle) = read_pipe(child.stdout.take());
    let (stderr, err_handle) = read_pipe(child.stderr.take());
    let mut stdin = child.stdin.take().expect("stdin pipe");

    stdin.write_all(b"echo one\n").expect("write command");
    stdin.flush().expect("flush command");
    stdin
        .write_all(b"printf '\\0360:tok\\036'\n")
        .expect("write sentinel");
    stdin.flush().expect("flush sentinel");

    // Wait for both outputs, then assert the order: the sentinel follows the
    // command's own output on the same stream.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = String::from_utf8_lossy(&snapshot(&stdout)).into_owned();
        if let (Some(one), Some(sentinel)) = (text.find("one"), text.find("\x1e0:tok\x1e")) {
            assert!(
                one < sentinel,
                "sentinel must follow the command output: {text:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "timed out waiting for output; stdout={:?} stderr={:?}",
                String::from_utf8_lossy(&snapshot(&stdout)),
                String::from_utf8_lossy(&snapshot(&stderr))
            );
        }
        std::thread::sleep(POLL);
    }

    // The member must still be alive: the pool relies on it across calls.
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the member exited after the sentinel; stderr={:?}",
        String::from_utf8_lossy(&snapshot(&stderr))
    );

    drop(stdin); // EOF ends the session.
    let status = wait_deadline(&mut child).expect("member did not exit at EOF");
    if let Some(handle) = out_handle {
        let _ = handle.join();
    }
    if let Some(handle) = err_handle {
        let _ = handle.join();
    }
    assert_eq!(
        code(status),
        0,
        "stderr: {:?}",
        String::from_utf8_lossy(&snapshot(&stderr))
    );
}

/// A multi-line construct must not be executed line by line: only the
/// closed program unit runs. `if ... then` is the sharp probe — its body line
/// is itself a complete command, so a per-line loop would print `first`
/// before the `fi` arrives.
#[test]
fn stdin_mode_waits_for_a_complete_program() {
    let Some(bin) = pir_bin() else {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable");
        return;
    };
    let mut child = Command::new(bin)
        .arg("__brush")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pir __brush");
    let (stdout, _out_handle) = read_pipe(child.stdout.take());
    let (_stderr, _err_handle) = read_pipe(child.stderr.take());
    let mut stdin = child.stdin.take().expect("stdin pipe");

    stdin
        .write_all(b"if true; then\necho first\n")
        .expect("write open");
    stdin.flush().expect("flush open");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        snapshot(&stdout).is_empty(),
        "the incomplete unit must not run: {:?}",
        String::from_utf8_lossy(&snapshot(&stdout))
    );

    stdin.write_all(b"fi\necho done\n").expect("write close");
    stdin.flush().expect("flush close");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = String::from_utf8_lossy(&snapshot(&stdout)).into_owned();
        if text.contains("done") {
            assert_eq!(text.matches("first").count(), 1, "stdout: {text:?}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for output: {text:?}");
        }
        std::thread::sleep(POLL);
    }

    drop(stdin);
    let status = wait_deadline(&mut child).expect("member did not exit at EOF");
    assert_eq!(code(status), 0);
}
