//! Tests for the codemode worker-process isolation (upstream
//! `packages/codemode/src/runtime/{host,worker,protocol}.ts`):
//!
//! - Wire-protocol frame round-trips (every `WorkerRequest`/`WorkerEvent`
//!   variant), CRLF tolerance, and the version gate (a wrong `v` or an
//!   unknown frame type must fail decoding so the parent falls back to the
//!   thread sandbox cleanly).
//! - The abort frame (parent ↔ worker cancellation path).
//! - Real-subprocess e2e against `pir __codemode-worker`: script run +
//!   tool-call bridge, the interrupt deadline, the abort frame, and the
//!   output caps. Skips (passes) when the pir binary cannot be spawned,
//!   following the repo's skip-and-pass convention.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use pir::plugins::codemode::worker::{
    decode_event, decode_request, encode_event, encode_request, WorkerEvent, WorkerRequest,
    CODEMODE_WORKER_PROTOCOL_VERSION, WORKER_SUBCOMMAND,
};

// ---------------------------------------------------------------------------
// Protocol frames: encode/decode round-trips
// ---------------------------------------------------------------------------

#[test]
fn request_frames_round_trip() {
    let frames = vec![
        WorkerRequest::Init {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            code: "return 1 + 1".to_owned(),
            allowed_tools: Some(vec!["echo".to_owned(), "read".to_owned()]),
            max_output_chars: 16 * 1024 * 1024,
            max_output_items: 100_000,
            timeout_ms: 30_000,
            // The configurable heap cap crosses the protocol frame
            // (`codemode.sandboxMemoryMb`); the frame round-trip must pin it.
            memory_bytes: Some(96 * 1024 * 1024),
        },
        WorkerRequest::Init {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            code: String::new(),
            allowed_tools: None,
            max_output_chars: 1,
            max_output_items: 2,
            timeout_ms: 3,
            memory_bytes: None,
        },
        WorkerRequest::Result {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            id: 42,
            ok: true,
            value: "{\"structured\":true}".to_owned(),
            json: true,
            error: None,
        },
        WorkerRequest::Result {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            id: 7,
            ok: false,
            value: String::new(),
            json: false,
            error: Some("tool exploded".to_owned()),
        },
        WorkerRequest::Abort {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
        },
    ];
    for frame in frames {
        let encoded =
            encode_request(&frame).unwrap_or_else(|error| panic!("encode failed: {error}"));
        assert!(!encoded.contains('\n'), "frames are single JSON lines");
        let decoded =
            decode_request(&encoded).unwrap_or_else(|error| panic!("decode failed: {error}"));
        assert_eq!(decoded, frame, "request frame must round-trip");
    }
}

#[test]
fn event_frames_round_trip() {
    let frames = vec![
        WorkerEvent::Ready {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
        },
        WorkerEvent::Call {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            id: 1,
            name: "echo".to_owned(),
            args: "{\"x\":7}".to_owned(),
        },
        WorkerEvent::Output {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            kind: "text".to_owned(),
            text: "hello 世界".to_owned(),
        },
        WorkerEvent::Output {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            kind: "console".to_owned(),
            text: "log line".to_owned(),
        },
        WorkerEvent::Done {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            ok: true,
            value: Some("\"2\"".to_owned()),
            name: None,
            error: None,
            head: None,
        },
        WorkerEvent::Done {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            ok: false,
            value: None,
            name: Some("RangeError".to_owned()),
            error: Some("script output exceeded the limit".to_owned()),
            head: None,
        },
        WorkerEvent::Done {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            ok: false,
            value: None,
            name: None,
            error: None,
            head: Some("Script timed out: Execution timed out after 300 ms".to_owned()),
        },
        WorkerEvent::Crash {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            message: "wasm trap".to_owned(),
        },
    ];
    for frame in frames {
        let encoded = encode_event(&frame).unwrap_or_else(|error| panic!("encode failed: {error}"));
        let decoded =
            decode_event(&encoded).unwrap_or_else(|error| panic!("decode failed: {error}"));
        assert_eq!(decoded, frame, "event frame must round-trip");
    }
}

#[test]
fn frames_tolerate_crlf_and_surrounding_whitespace() {
    // Windows pipes may surface `\r\n` line endings; the decoder trims.
    let frame = WorkerRequest::Abort {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
    };
    let mut encoded = encode_request(&frame).unwrap_or_default();
    encoded.push_str("\r\n");
    assert_eq!(
        decode_request(&encoded).unwrap_or_else(|error| panic!("decode failed: {error}")),
        frame
    );
    let event = WorkerEvent::Ready {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
    };
    let mut encoded = encode_event(&event).unwrap_or_default();
    encoded.insert_str(0, "  ");
    encoded.push('\n');
    assert_eq!(
        decode_event(&encoded).unwrap_or_else(|error| panic!("decode failed: {error}")),
        event
    );
}

#[test]
fn decode_rejects_garbage_and_unknown_types() {
    let error = decode_request("this is not json").expect_err("decode must fail");
    assert!(
        error.contains("invalid"),
        "garbage must be rejected: {error}"
    );
    let error = decode_event("[]").expect_err("decode must fail");
    assert!(
        error.contains("invalid"),
        "wrong shape must be rejected: {error}"
    );
    // Unknown frame types (a future protocol) are rejected, not misrouted.
    let error = decode_event(r#"{"type":"wat","v":1}"#).expect_err("decode must fail");
    assert!(
        error.contains("invalid"),
        "unknown type must be rejected: {error}"
    );
    let error = decode_request("").expect_err("decode must fail");
    assert!(
        error.contains("empty"),
        "empty lines must be rejected: {error}"
    );
}

#[test]
fn decode_rejects_protocol_version_mismatch() {
    // The parent falls back to the thread sandbox on any version mismatch;
    // both directions must fail loudly.
    let wrong = CODEMODE_WORKER_PROTOCOL_VERSION + 1;
    let init = format!(
        r#"{{"type":"init","v":{wrong},"code":"x","max_output_chars":1,"max_output_items":1,"timeout_ms":1}}"#
    );
    let error = decode_request(&init).expect_err("decode must fail");
    assert!(
        error.contains("protocol version mismatch"),
        "version gate must trip on requests: {error}"
    );
    let ready = format!(r#"{{"type":"ready","v":{wrong}}}"#);
    let error = decode_event(&ready).expect_err("decode must fail");
    assert!(
        error.contains("protocol version mismatch"),
        "version gate must trip on events: {error}"
    );
    // The current version round-trips through the same raw shapes.
    assert!(decode_request(&format!(
        r#"{{"type":"init","v":{CODEMODE_WORKER_PROTOCOL_VERSION},"code":"","max_output_chars":1,"max_output_items":1,"timeout_ms":1}}"#
    ))
    .is_ok());
}

// ---------------------------------------------------------------------------
// Real-subprocess e2e: `pir __codemode-worker`
// ---------------------------------------------------------------------------

/// A spawned worker child with its stdout pumped into a channel of raw
/// lines (the reader thread keeps the pipe drained so the child never
/// blocks on a full buffer).
struct WorkerProc {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
}

fn spawn_worker() -> Option<WorkerProc> {
    let mut child = match Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg(WORKER_SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("SKIP: cannot spawn the pir binary for the codemode worker e2e: {error}");
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let stdin = child.stdin.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    Some(WorkerProc {
        child,
        stdin,
        lines: rx,
    })
}

fn send_request(proc: &mut WorkerProc, frame: &WorkerRequest) {
    let mut line = encode_request(frame).unwrap_or_else(|error| panic!("encode failed: {error}"));
    line.push('\n');
    proc.stdin
        .write_all(line.as_bytes())
        .unwrap_or_else(|error| panic!("failed to write a worker frame: {error}"));
    let _ = proc.stdin.flush();
}

/// Receive the next decodable event, bounding the wait so a wedged worker
/// fails the test with a clear message instead of hanging CI.
fn recv_event(proc: &WorkerProc, timeout: Duration, context: &str) -> WorkerEvent {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match proc.lines.recv_timeout(remaining) {
            Ok(line) => match decode_event(&line) {
                Ok(event) => return event,
                Err(_) => continue, // harness noise; keep waiting
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for a worker event ({context})");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("worker stdout closed before the expected event ({context})");
            }
        }
    }
}

fn init_frame(code: &str, timeout_ms: u64, max_output_chars: usize) -> WorkerRequest {
    // `None` = no heap cap; the e2e tests here exercise timeout/output
    // behavior, not the memory budget (see `init_frame_with_memory`).
    init_frame_with_memory(code, timeout_ms, max_output_chars, None)
}

fn init_frame_with_memory(
    code: &str,
    timeout_ms: u64,
    max_output_chars: usize,
    memory_bytes: Option<u64>,
) -> WorkerRequest {
    WorkerRequest::Init {
        v: CODEMODE_WORKER_PROTOCOL_VERSION,
        code: code.to_owned(),
        allowed_tools: None,
        max_output_chars,
        max_output_items: 100_000,
        timeout_ms,
        memory_bytes,
    }
}

/// Consume the worker's `ready` handshake (every scripted run starts with
/// one; the main e2e below asserts its exact shape).
fn consume_ready(proc: &WorkerProc) {
    match recv_event(proc, Duration::from_secs(15), "ready") {
        WorkerEvent::Ready { .. } => {}
        other => panic!("expected a ready frame, got {other:?}"),
    }
}

#[test]
fn worker_process_runs_script_and_bridges_tool_calls() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    send_request(
        &mut proc,
        &init_frame(
            r#"text("hello"); const value = await tools.echo({ x: 7 }); return value;"#,
            20_000,
            1024 * 1024,
        ),
    );
    assert_eq!(
        recv_event(&proc, Duration::from_secs(15), "ready"),
        WorkerEvent::Ready {
            v: CODEMODE_WORKER_PROTOCOL_VERSION
        }
    );
    // `text("hello")` runs in the sync prefix before the first `await`, so
    // its output frame streams before the tool call.
    match recv_event(&proc, Duration::from_secs(15), "output") {
        WorkerEvent::Output { kind, text, .. } => {
            assert_eq!(kind, "text");
            assert_eq!(text, "hello");
        }
        other => panic!("expected an output frame, got {other:?}"),
    }
    // The tool call crosses back to the host as a `call` frame...
    let call = match recv_event(&proc, Duration::from_secs(15), "tool call") {
        WorkerEvent::Call { id, name, args, .. } => (id, name, args),
        other => panic!("expected a call frame, got {other:?}"),
    };
    assert_eq!(call.1, "echo");
    assert_eq!(call.2, r#"{"x":7}"#);
    // ...which the host answers with a `result` frame...
    send_request(
        &mut proc,
        &WorkerRequest::Result {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
            id: call.0,
            ok: true,
            value: "echo:7".to_owned(),
            json: false,
            error: None,
        },
    );
    // ...and the done value comes back.
    match recv_event(&proc, Duration::from_secs(15), "done") {
        WorkerEvent::Done {
            ok, value, head, ..
        } => {
            assert!(ok, "script must complete: {head:?}");
            assert_eq!(value.as_deref(), Some("echo:7"));
        }
        other => panic!("expected a done frame, got {other:?}"),
    }
    // The worker exits cleanly after `done`: close our stdin half first (the
    // child's reader thread blocks on EOF) and only then wait for the exit.
    let WorkerProc {
        mut child,
        stdin,
        lines,
    } = proc;
    drop(stdin);
    drop(lines);
    let status = child
        .wait()
        .unwrap_or_else(|error| panic!("wait failed: {error}"));
    assert!(status.success(), "worker must exit 0 after done: {status}");
}

#[test]
fn worker_process_rejects_an_invalid_init_frame() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    let mut line = r#"{"type":"wat","v":1}"#.to_owned();
    line.push('\n');
    let _ = proc.stdin.write_all(line.as_bytes());
    let _ = proc.stdin.flush();
    match recv_event(&proc, Duration::from_secs(15), "crash on bad init") {
        WorkerEvent::Crash { message, .. } => {
            assert!(message.contains("invalid"), "crash must explain: {message}");
        }
        other => panic!("expected a crash frame, got {other:?}"),
    }
}

#[test]
fn worker_process_enforces_the_deadline() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    // A runaway loop must actually be stopped by the child's QuickJS
    // interrupt deadline (the same mechanics the thread sandbox uses), and
    // the worker reports the thread-sandbox timeout head itself.
    let started = std::time::Instant::now();
    send_request(&mut proc, &init_frame("while (true) {}", 300, 1024 * 1024));
    consume_ready(&proc);
    match recv_event(&proc, Duration::from_secs(20), "timeout done") {
        WorkerEvent::Done {
            ok, head, error, ..
        } => {
            assert!(!ok, "a runaway script must fail");
            // Either the child's QuickJS interrupt deadline surfaces as the
            // uncatchable exception (thread-sandbox parity: "script
            // execution failed: ...") or the outer tokio deadline wins
            // ("Script timed out: ..."); both are the correct classification.
            // Any other head (a setup error, an unrelated crash) is a
            // different failure and must not satisfy the timeout contract.
            let head = head.unwrap_or_default();
            assert!(
                head.starts_with("Script timed out:")
                    || head.starts_with("script execution failed:"),
                "timeout classification expected, got head: {head:?}, error: {error:?}"
            );
            // The configured 300 ms deadline is what ended the script.
            assert!(
                started.elapsed() >= Duration::from_millis(300),
                "the script ended before the configured deadline (took {:?})",
                started.elapsed()
            );
        }
        other => panic!("expected a done frame, got {other:?}"),
    }
}

#[test]
fn worker_process_enforces_the_configured_heap_cap() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    // The init frame's `memory_bytes` (the parent's
    // `codemode.sandboxMemoryMb` / `PIR_CODEMODE_MEMORY_MB` resolution) must
    // cap the child's QuickJS heap: a script growing by 1 MiB per round
    // cannot complete under a 48 MiB cap. The failure classification is
    // deliberately loose (catchable OOM → script error; uncatchable →
    // "script execution failed: ..."): the contract is "the script fails,
    // the worker process stays alive and reports a done frame".
    send_request(
        &mut proc,
        &init_frame_with_memory(
            "const chunks = []; for (let i = 0; i < 8192; i++) chunks.push(new Uint8Array(1024 * 1024));",
            20_000,
            1024 * 1024,
            Some(48 * 1024 * 1024),
        ),
    );
    consume_ready(&proc);
    loop {
        match recv_event(&proc, Duration::from_secs(20), "heap-cap done") {
            WorkerEvent::Output { .. } => {}
            WorkerEvent::Done { ok, head, .. } => {
                assert!(
                    !ok,
                    "an over-cap script must fail, got done ok with head {head:?}"
                );
                break;
            }
            other => panic!("expected a done frame, got {other:?}"),
        }
    }
}

#[test]
fn worker_process_honors_the_abort_frame() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    // A long deadline, cancelled by the abort frame: the child's interrupt
    // handler sees the flag and stops the script uncatchably (the same
    // cancellation path the thread sandbox's abort signal drives).
    send_request(
        &mut proc,
        &init_frame("while (true) {}", 60_000, 1024 * 1024),
    );
    std::thread::sleep(Duration::from_millis(300));
    consume_ready(&proc);
    send_request(
        &mut proc,
        &WorkerRequest::Abort {
            v: CODEMODE_WORKER_PROTOCOL_VERSION,
        },
    );
    match recv_event(&proc, Duration::from_secs(20), "aborted done") {
        WorkerEvent::Done { ok, .. } => {
            assert!(!ok, "an aborted script must fail");
        }
        other => panic!("expected a done frame, got {other:?}"),
    }
}

#[test]
fn worker_process_enforces_the_output_caps() {
    let Some(mut proc) = spawn_worker() else {
        return;
    };
    // 16-char cap: the first item fits (16 == cap), the second trips the
    // prelude's accounting and FAILS the script (upstream `done()` error
    // path; catching the throw does not resume output).
    send_request(
        &mut proc,
        &init_frame(r#"text("0123456789ABCDEF"); text("more");"#, 20_000, 16),
    );
    consume_ready(&proc);
    loop {
        match recv_event(&proc, Duration::from_secs(20), "cap done") {
            WorkerEvent::Output { text, .. } => assert_eq!(text, "0123456789ABCDEF"),
            WorkerEvent::Done {
                ok, name, error, ..
            } => {
                assert!(!ok, "the capped script must fail");
                assert_eq!(name.as_deref(), Some("RangeError"), "cap error name");
                let error = error.unwrap_or_default();
                assert!(
                    error.contains("script output exceeded the limit"),
                    "cap error text: {error}"
                );
                break;
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
}
