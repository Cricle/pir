//! The env client's terminal surface (`openTerminal` / `ptyWrite` /
//! `ptyResize` / `ptyForeground`, upstream `packages/env/src/terminal.ts`)
//! over the real wire framing.
//!
//! The mock daemon below is this test binary re-invoked through a tiny
//! wrapper that forwards the client-generated `--token` as an environment
//! variable; it speaks the documented `u32 length | u8 type | u32 id | u32
//! jsonLength | json | payload` frames with the shared
//! `pir::env_daemon::frame` codec. This covers the client's request/event
//! handling on every platform. A real `pir env-daemon` is exercised too:
//! Windows refuses terminals with `ENOTSUP` (as upstream does), POSIX runs a
//! shell in a real PTY.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::env_client::{
    open_terminal, was_closed, writer_queue_limits, Connection, TerminalOptions, WRITE_CHUNK,
};
use pir::env_daemon::frame::{self, Frame, CANCEL, ERROR, EVENT, REQUEST, RESULT};

// ---------------------------------------------------------------------------
// The mock daemon (child entry of this test binary)
// ---------------------------------------------------------------------------

fn append(dir: &Path, name: &str, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
        .expect("open capture file");
    file.write_all(bytes).expect("capture bytes");
}

fn reply(out: &mut impl Write, id: u32, value: Value) {
    frame::write_frame(out, &Frame::new(RESULT, id, value)).expect("write result");
}

/// The mock daemon loop. Enabled by `PIR_MOCK_PTY_DAEMON=1`; the token and
/// capture directory come from the wrapper script's environment.
#[test]
fn mock_pty_daemon_child() {
    if std::env::var("PIR_MOCK_PTY_DAEMON").as_deref() != Ok("1") {
        return;
    }
    let token = std::env::var("PIR_MOCK_PTY_TOKEN").unwrap_or_default();
    let dir = PathBuf::from(std::env::var("PIR_MOCK_PTY_DIR").unwrap_or_default());
    let stdout = std::io::stdout();
    {
        let mut out = stdout.lock();
        writeln!(out, "PI-ENV {token}").expect("sync line");
        out.flush().expect("sync line flush");
    }
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut terminals: Vec<u32> = Vec::new();
    while let Ok(Some(request)) = frame::read_frame(&mut input) {
        let op = request
            .json
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut out = stdout.lock();
        match (request.kind, op.as_str()) {
            (REQUEST, "hello") => reply(
                &mut out,
                request.id,
                json!({
                    "protocol": 1,
                    "version": "mock-daemon",
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "home": dir.to_string_lossy(),
                    "separator": std::path::MAIN_SEPARATOR.to_string(),
                    "tmpdir": dir.to_string_lossy(),
                    "cwd": dir.to_string_lossy(),
                    "pid": 99,
                    "features": [],
                }),
            ),
            (REQUEST, "pty") => {
                terminals.push(request.id);
                append(&dir, "pty.jsonl", format!("{}\n", request.json).as_bytes());
                frame::write_frame(
                    &mut out,
                    &Frame::new(EVENT, request.id, json!({ "kind": "started", "pid": 1234 })),
                )
                .expect("started event");
                frame::write_frame(
                    &mut out,
                    &Frame::with_payload(
                        EVENT,
                        request.id,
                        json!({ "kind": "output" }),
                        b"hello from pty".to_vec(),
                    ),
                )
                .expect("output event");
            }
            (REQUEST, "ptyWrite") => {
                // A wedged daemon: the frame is consumed but never answered,
                // so only the client's per-request timeout (or a close) can
                // free the serial writer. `hung.jsonl` records that the
                // daemon DID take it, which lets a test order `close()`
                // after the writer parked on this request.
                if request.payload.windows(4).any(|window| window == b"hang") {
                    append(&dir, "hung.jsonl", b"write\n");
                    continue;
                }
                append(&dir, "writes.bin", &request.payload);
                let preview: String =
                    String::from_utf8_lossy(&request.payload[..request.payload.len().min(32)])
                        .into_owned();
                append(
                    &dir,
                    "ops.jsonl",
                    format!(
                        "{}\n",
                        json!({
                            "op": "write",
                            "bytes": request.payload.len(),
                            "preview": preview,
                        })
                    )
                    .as_bytes(),
                );
                reply(&mut out, request.id, json!({}));
                if request.payload.windows(4).any(|window| window == b"exit") {
                    if let Some(id) = terminals.pop() {
                        reply(&mut out, id, json!({ "exitCode": 7 }));
                    }
                }
            }
            (REQUEST, "ptyResize") => {
                // Same wedge as `hang` writes: a resize the daemon swallows.
                if request.json.get("cols").and_then(Value::as_u64) == Some(666) {
                    append(&dir, "hung.jsonl", b"resize\n");
                    continue;
                }
                append(
                    &dir,
                    "resizes.jsonl",
                    format!("{}\n", request.json).as_bytes(),
                );
                append(
                    &dir,
                    "ops.jsonl",
                    format!(
                        "{}\n",
                        json!({
                            "op": "resize",
                            "cols": request.json.get("cols"),
                            "rows": request.json.get("rows"),
                        })
                    )
                    .as_bytes(),
                );
                reply(&mut out, request.id, json!({}));
            }
            (REQUEST, "ptyForeground") => reply(
                &mut out,
                request.id,
                json!({ "foreground": { "pid": 4321, "name": "ed" } }),
            ),
            (REQUEST, "ptyForegroundEmpty") => {
                reply(&mut out, request.id, json!({ "foreground": null }))
            }
            (REQUEST, "lstat") => reply(&mut out, request.id, json!({ "kind": "directory" })),
            (CANCEL, _) => {
                if terminals.contains(&request.id) {
                    frame::write_frame(
                        &mut out,
                        &Frame::new(
                            ERROR,
                            request.id,
                            json!({ "code": "aborted", "message": "aborted" }),
                        ),
                    )
                    .expect("abort error");
                    terminals.retain(|id| *id != request.id);
                }
            }
            (REQUEST, _) => frame::write_frame(
                &mut out,
                &Frame::new(
                    ERROR,
                    request.id,
                    json!({ "code": "ENOSYS", "message": "mock daemon: unsupported op" }),
                ),
            )
            .expect("unsupported error"),
            // Pings and unsolicited frames are ignored, as the real daemon does.
            _ => {}
        }
        out.flush().expect("frame flush");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Tree {
        let path = std::env::temp_dir().join(format!(
            "pir-env-pty-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create test tree");
        Tree(path)
    }

    fn root(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A wrapper that turns the client-generated `serve --token <token>` suffix
/// into `PIR_MOCK_PTY_TOKEN` for the mock daemon child.
#[cfg(windows)]
fn write_daemon_wrapper(dir: &Path) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let wrapper = dir.join("mock-daemon.bat");
    let script = format!(
        "@echo off\r\nset \"PIR_MOCK_PTY_TOKEN=%~3\"\r\nset \"PIR_MOCK_PTY_DIR={}\"\r\nset \"PIR_MOCK_PTY_DAEMON=1\"\r\n\"{}\" mock_pty_daemon_child --exact --nocapture\r\n",
        dir.display(),
        exe.display()
    );
    std::fs::write(&wrapper, script).expect("write wrapper");
    wrapper
}

#[cfg(unix)]
fn write_daemon_wrapper(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().expect("current exe");
    let wrapper = dir.join("mock-daemon.sh");
    let script = format!(
        "#!/bin/sh\nPIR_MOCK_PTY_TOKEN=\"$3\"\nPIR_MOCK_PTY_DIR=\"{}\"\nPIR_MOCK_PTY_DAEMON=1\nexport PIR_MOCK_PTY_TOKEN PIR_MOCK_PTY_DIR PIR_MOCK_PTY_DAEMON\nexec \"{}\" mock_pty_daemon_child --exact --nocapture\n",
        dir.display(),
        exe.display()
    );
    std::fs::write(&wrapper, script).expect("write wrapper");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("wrapper mode");
    wrapper
}

fn mock_connection(dir: &Path) -> Connection {
    let wrapper = write_daemon_wrapper(dir);
    // Same overlayfs ETXTBSY window as the env-client wrappers: let the write
    // settle before the daemon is exec'd.
    std::thread::sleep(std::time::Duration::from_millis(20));
    #[cfg(windows)]
    let argv = vec![
        "cmd".to_owned(),
        "/c".to_owned(),
        wrapper.to_string_lossy().into_owned(),
    ];
    #[cfg(unix)]
    let argv = vec![wrapper.to_string_lossy().into_owned()];
    Connection::argv(argv)
}

async fn wait_for_output(output: &Arc<Mutex<Vec<u8>>>, needle: &[u8]) {
    for _ in 0..200 {
        if output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "output never contained {:?}: {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(
            &output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        )
    );
}

/// Wait until the daemon captured exactly `bytes` of `ptyWrite` payloads.
async fn wait_for_writes(path: &Path, bytes: usize) -> Vec<u8> {
    for _ in 0..600 {
        if let Ok(captured) = std::fs::read(path) {
            if captured.len() >= bytes {
                return captured;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no {bytes}-byte write capture at {}", path.display());
}

/// Wait until the daemon logged `count` operations (`ptyWrite`/`ptyResize`),
/// in wire-arrival order.
async fn wait_for_ops(dir: &Path, count: usize) -> Vec<Value> {
    let path = dir.join("ops.jsonl");
    for _ in 0..600 {
        let ops: Vec<Value> = std::fs::read_to_string(&path)
            .ok()
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| serde_json::from_str(line).expect("ops line JSON"))
                    .collect()
            })
            .unwrap_or_default();
        if ops.len() >= count {
            return ops;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "daemon never logged {count} terminal operations: {:?}",
        std::fs::read_to_string(&path).unwrap_or_default()
    );
}

/// Wait until the mock daemon consumed a wedged (`hang`) request, i.e. the
/// client's serial writer is parked inside it.
async fn wait_for_hang(dir: &Path, needle: &[u8]) {
    let path = dir.join("hung.jsonl");
    for _ in 0..600 {
        if let Ok(captured) = std::fs::read(&path) {
            if captured
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the daemon never consumed the wedged request (`{}`)",
        String::from_utf8_lossy(needle)
    );
}

// ---------------------------------------------------------------------------
// The client surface over the wire
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_opens_writes_resizes_and_reports_the_foreground_job() {
    let tree = Tree::new("wire");
    let connection = mock_connection(&tree.0);
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_callback = Arc::clone(&output);
    let mut env = BTreeMap::new();
    env.insert("TERM".to_owned(), "xterm-256color".to_owned());
    let options = TerminalOptions::new(tree.root(), 80, 24, move |bytes| {
        output_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
    })
    .argv(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "cat".to_owned(),
    ])
    .env(env)
    .inherit_env(false);

    let terminal = open_terminal(connection.clone(), options)
        .await
        .expect("open terminal");
    assert_eq!(terminal.pid(), 1234, "the started event carries the pid");

    terminal.write_async(b"hello\n").await.expect("write input");
    terminal.resize_async(100, 40).await.expect("resize");
    let foreground = terminal
        .foreground()
        .await
        .expect("foreground")
        .expect("a foreground job while a shell child runs");
    assert_eq!(foreground.pid, 4321);
    assert_eq!(foreground.name.as_deref(), Some("ed"));

    terminal.write_async(b"exit\n").await.expect("exit input");
    assert_eq!(terminal.exited().wait().await.expect("exit code"), 7);
    assert!(
        terminal.exited().try_get().is_some(),
        "the settlement is retained for later readers"
    );

    wait_for_output(&output, b"hello from pty").await;

    // The pty request body carries every option; the wrapper captured it.
    let pty: Value = serde_json::from_str(
        std::fs::read_to_string(tree.0.join("pty.jsonl"))
            .expect("pty capture")
            .trim(),
    )
    .expect("pty JSON");
    assert_eq!(pty["cwd"], json!(tree.root()));
    assert_eq!(pty["cols"], json!(80));
    assert_eq!(pty["rows"], json!(24));
    assert_eq!(pty["inheritEnv"], json!(false));
    assert_eq!(pty["env"]["TERM"], json!("xterm-256color"));
    assert_eq!(
        pty["argv"],
        json!(["/bin/sh", "-c", "cat"]),
        "argv survives the request"
    );

    // Input is written in call order, one payload per `ptyWrite`.
    let writes = std::fs::read(tree.0.join("writes.bin")).expect("write capture");
    assert!(
        writes.windows(6).any(|window| window == b"hello\n"),
        "first input: {:?}",
        String::from_utf8_lossy(&writes)
    );
    assert!(
        writes.windows(5).any(|window| window == b"exit\n"),
        "second input: {:?}",
        String::from_utf8_lossy(&writes)
    );

    let resize: Value = serde_json::from_str(
        std::fs::read_to_string(tree.0.join("resizes.jsonl"))
            .expect("resize capture")
            .trim(),
    )
    .expect("resize JSON");
    assert_eq!(resize["cols"], json!(100));
    assert_eq!(resize["rows"], json!(40));
    assert!(
        resize["target"].as_u64().is_some(),
        "the resize targets the running terminal: {resize}"
    );

    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_writes_are_chunked_and_reach_the_daemon_in_call_order() {
    let tree = Tree::new("chunk-order");
    let connection = mock_connection(&tree.0);
    let terminal = open_terminal(
        connection.clone(),
        TerminalOptions::new(tree.root(), 80, 24, |_| {}),
    )
    .await
    .expect("open terminal");

    // More than two full chunks: the chunk boundaries and their order are the
    // `WRITE_CHUNK` slices of the submitted buffer.
    let trailing = 517usize;
    let payload: Vec<u8> = (0..WRITE_CHUNK * 2 + trailing)
        .map(|index| b'a' + (index % 26) as u8)
        .collect();
    terminal.write(&payload);

    let captured = wait_for_writes(&tree.0.join("writes.bin"), payload.len()).await;
    assert_eq!(
        captured, payload,
        "chunked write bytes must arrive concatenated in call order"
    );
    let ops = wait_for_ops(&tree.0, 3).await;
    let sizes: Vec<u64> = ops
        .iter()
        .filter(|op| op["op"] == "write")
        .map(|op| op["bytes"].as_u64().expect("write byte count"))
        .collect();
    assert_eq!(
        sizes,
        vec![WRITE_CHUNK as u64, WRITE_CHUNK as u64, trailing as u64],
        "one frame per chunk, in order: {ops:?}"
    );
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_and_resizes_reach_the_daemon_in_call_order() {
    let tree = Tree::new("write-resize-order");
    let connection = mock_connection(&tree.0);
    let terminal = open_terminal(
        connection.clone(),
        TerminalOptions::new(tree.root(), 80, 24, |_| {}),
    )
    .await
    .expect("open terminal");

    // Three fire-and-forget calls: the serial outbox must preserve the exact
    // order even though no reply is awaited between them.
    terminal.write(b"before");
    terminal.resize(120, 50);
    terminal.write(b"after");

    let ops = wait_for_ops(&tree.0, 3).await;
    assert_eq!(ops[0]["op"], json!("write"), "{ops:?}");
    assert_eq!(ops[0]["preview"], json!("before"), "{ops:?}");
    assert_eq!(ops[1]["op"], json!("resize"), "{ops:?}");
    assert_eq!(ops[1]["cols"], json!(120), "{ops:?}");
    assert_eq!(ops[1]["rows"], json!(50), "{ops:?}");
    assert_eq!(ops[2]["op"], json!("write"), "{ops:?}");
    assert_eq!(ops[2]["preview"], json!("after"), "{ops:?}");

    let writes = wait_for_writes(&tree.0.join("writes.bin"), 11).await;
    assert_eq!(writes, b"beforeafter", "writes keep their order");
    connection.close();
}

/// A daemon that swallows `ptyWrite`/`ptyResize` frames must not park the
/// serial writer forever: each request fails on its own timeout, and the
/// writer then drains the operations queued behind it (in call order).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_daemon_fails_each_operation_by_timeout_and_the_writer_continues() {
    let tree = Tree::new("write-timeout");
    let connection = mock_connection(&tree.0);
    let terminal = open_terminal(
        connection.clone(),
        TerminalOptions::new(tree.root(), 80, 24, |_| {})
            .request_timeout(Duration::from_millis(300)),
    )
    .await
    .expect("open terminal");

    let started = std::time::Instant::now();
    let error = terminal
        .write_async(b"hang")
        .await
        .expect_err("an unanswered ptyWrite must fail instead of parking the writer");
    assert_eq!(error.code, "timeout", "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the write must fail on its configured timeout, not hang: {:?}",
        started.elapsed()
    );

    // The writer survived the timeout: the next operation is sent and
    // answered, including after a resize timeout.
    let error = terminal
        .resize_async(666, 1)
        .await
        .expect_err("an unanswered ptyResize must time out the same way");
    assert_eq!(error.code, "timeout", "{error:?}");
    terminal
        .resize_async(120, 50)
        .await
        .expect("the writer drains after a resize timeout");
    terminal
        .write_async(b"ok")
        .await
        .expect("the writer drains after a write timeout");

    let ops = wait_for_ops(&tree.0, 2).await;
    assert_eq!(ops[0]["op"], json!("resize"), "{ops:?}");
    assert_eq!(ops[0]["cols"], json!(120), "{ops:?}");
    assert_eq!(ops[1]["op"], json!("write"), "{ops:?}");
    assert_eq!(ops[1]["preview"], json!("ok"), "{ops:?}");
    let writes = wait_for_writes(&tree.0.join("writes.bin"), 2).await;
    assert_eq!(writes, b"ok", "the wedged write never reached the daemon");
    connection.close();
}

/// `close()` while the writer is parked on a swallowed request must cancel
/// that request, fail every queued write with the explicit closed error, stop
/// the writer task, and keep refusing later writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_while_a_write_is_stuck_stops_the_writer_and_refuses_later_writes() {
    let tree = Tree::new("close-stuck");
    let connection = mock_connection(&tree.0);
    let terminal = Arc::new(
        open_terminal(
            connection.clone(),
            // Long enough that only `close()` can free the pending write.
            TerminalOptions::new(tree.root(), 80, 24, |_| {})
                .request_timeout(Duration::from_secs(60)),
        )
        .await
        .expect("open terminal"),
    );

    let stuck = {
        let terminal = Arc::clone(&terminal);
        tokio::spawn(async move { terminal.write_async(b"hang").await })
    };
    // Only close once the daemon has the frame and the writer is parked on it.
    wait_for_hang(&tree.0, b"write").await;

    terminal.close();
    let error = stuck
        .await
        .expect("the stuck write task")
        .expect_err("close must fail the pending write");
    assert_eq!(error.code, "aborted", "{error:?}");
    assert_eq!(error.message, "terminal closed");
    assert!(was_closed(&error));

    // The serial writer exits with the terminal ...
    tokio::time::timeout(Duration::from_secs(5), terminal.writer_closed())
        .await
        .expect("the serial writer must exit after close");

    // ... and later input is refused explicitly (`aborted`), not queued into a
    // dead writer.
    let error = terminal
        .write_async(b"after")
        .await
        .expect_err("a closed terminal refuses writes");
    assert_eq!(error.code, "aborted", "{error:?}");
    assert_eq!(error.message, "terminal closed");
    let error = terminal
        .resize_async(100, 40)
        .await
        .expect_err("a closed terminal refuses resizes");
    assert_eq!(error.code, "aborted", "{error:?}");
    assert_eq!(error.message, "terminal closed");

    // The pty request itself settles `aborted`, as before.
    let error = terminal
        .exited()
        .wait()
        .await
        .expect_err("close aborts the pty request");
    assert_eq!(error.code, "aborted", "{error:?}");
    connection.close();
}

/// The serial writer's outbox is bounded: with the writer parked on a request
/// the daemon never answers, enqueueing past the documented depth refuses the
/// NEWEST operation with an explicit `EAGAIN` instead of growing without a
/// bound (the small-device contract). Already-queued operations keep their
/// order — the refusal never rewrites the queue — and `close()` still settles
/// every queued waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_serial_outbox_refuses_the_newest_operation_instead_of_growing() {
    let (max_depth, max_bytes) = writer_queue_limits();
    assert!(max_depth > 0, "the outbox must advertise a depth bound");
    assert!(
        max_bytes <= 16 * 1024 * 1024,
        "the outbox byte budget must stay small-device sized: {max_bytes}"
    );

    let tree = Tree::new("outbox-bound");
    let connection = mock_connection(&tree.0);
    let terminal = Arc::new(
        open_terminal(
            connection.clone(),
            // Long enough that only the bound (not a timeout) refuses input.
            TerminalOptions::new(tree.root(), 80, 24, |_| {})
                .request_timeout(Duration::from_secs(3600)),
        )
        .await
        .expect("open terminal"),
    );

    // Park the serial writer on a request the mock daemon swallows.
    terminal.write(b"hang");
    wait_for_hang(&tree.0, b"write").await;

    // Fill the outbox to its documented depth with fire-and-forget resizes
    // (the writer holds one operation, so this is exactly the cap).
    for _ in 0..max_depth {
        terminal.resize(80, 24);
    }

    // The NEXT operation is refused with the explicit overload error ...
    let error = terminal
        .resize_async(90, 30)
        .await
        .expect_err("a full outbox must refuse the newest operation");
    assert_eq!(error.code, "EAGAIN", "{error:?}");
    assert!(
        error.message.contains("queue is full"),
        "the refusal must name the queue: {error:?}"
    );
    // ... and a write is refused the same way (no input is silently accepted
    // past the bound).
    let error = terminal
        .write_async(b"after the bound")
        .await
        .expect_err("a full outbox must refuse writes too");
    assert_eq!(error.code, "EAGAIN", "{error:?}");

    // The queued resizes stay behind the parked request in order; close()
    // settles the parked request and answers every queued waiter, so nothing
    // is stranded by the refusal.
    terminal.close();
    tokio::time::timeout(Duration::from_secs(5), terminal.writer_closed())
        .await
        .expect("the serial writer must exit after close");
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_terminal_aborts_its_request() {
    let tree = Tree::new("close");
    let connection = mock_connection(&tree.0);
    let terminal = open_terminal(
        connection.clone(),
        TerminalOptions::new(tree.root(), 80, 24, |_| {}),
    )
    .await
    .expect("open terminal");
    terminal.close();
    let error = terminal
        .exited()
        .wait()
        .await
        .expect_err("closing rejects the terminal's settlement");
    assert_eq!(error.code, "aborted", "{error:?}");
    assert!(was_closed(&error));
    // The connection itself survives closing one terminal.
    let reply = connection
        .request("lstat", json!({ "path": tree.root() }), Default::default())
        .await
        .expect("connection still answers");
    assert_eq!(reply.json["kind"], json!("directory"));
    connection.close();
}

// ---------------------------------------------------------------------------
// The real daemon
// ---------------------------------------------------------------------------

fn pir_binary() -> &'static str {
    env!("CARGO_BIN_EXE_pir")
}

fn real_connection() -> Connection {
    Connection::argv(vec![pir_binary().to_string(), "env-daemon".to_string()])
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_builtin_daemon_refuses_terminals_on_windows() {
    // Upstream refuses terminals on Windows; the client surfaces the daemon's
    // ENOTSUP without breaking the connection.
    let tree = Tree::new("windows-enotsup");
    let connection = real_connection();
    let error = open_terminal(
        connection.clone(),
        TerminalOptions::new(tree.root(), 80, 24, |_| {}),
    )
    .await
    .expect_err("terminals are not supported on Windows");
    assert_eq!(error.code, "ENOTSUP", "{error:?}");
    let reply = connection
        .request("lstat", json!({ "path": tree.root() }), Default::default())
        .await
        .expect("the daemon still answers");
    assert_eq!(reply.json["kind"], json!("directory"));
    connection.close();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_builtin_daemon_runs_a_terminal_end_to_end() {
    let tree = Tree::new("unix-pty");
    let connection = real_connection();
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_callback = Arc::clone(&output);
    let terminal = open_terminal(
        connection.clone(),
        TerminalOptions::new(tree.root(), 80, 24, move |bytes| {
            output_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
        })
        .argv(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf ready; read line; printf \"got:$line\"; exit 3".to_owned(),
        ])
        .inherit_env(true),
    )
    .await
    .expect("open terminal");
    assert!(terminal.pid() > 0);
    wait_for_output(&output, b"ready").await;
    // A program at its prompt has no separate foreground job.
    assert_eq!(terminal.foreground().await.expect("foreground"), None);
    terminal.resize_async(100, 40).await.expect("resize");
    terminal.write_async(b"hi\n").await.expect("write");
    assert_eq!(terminal.exited().wait().await.expect("exit code"), 3);
    wait_for_output(&output, b"got:hi").await;
    connection.close();
}
