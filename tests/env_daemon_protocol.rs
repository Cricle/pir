//! `pir env-daemon` — the built-in pi-env protocol-v1 daemon
//! (`src/env_daemon/`, ported from upstream `packages/env/daemon`) and the
//! ported unit vectors upstream keeps as inline `#[cfg(test)]` modules.
//!
//! The end-to-end half spawns the real binary
//! (`env!("CARGO_BIN_EXE_pir") env-daemon serve --token <hex>`) and speaks the
//! documented wire protocol over its stdio — the same shape the JS shim's
//! `Connection` and upstream's `pi-env` client use:
//!
//!   hello → writeFile/readFile → listFiles → exec (stdout/stderr/exit code)
//!   → cancel a running exec → error-frame shapes.
//!
//! The unit half re-states the upstream `#[cfg(test)]` vectors that could not
//! stay inline in `src/` (the pir hygiene gate forbids inline tests there):
//! frame round-trip, WHATWG UTF-8 stream decoding, the `LineScanner` selection
//! rules, `listFiles` traversal/ignore/bounds, the output window's skip
//! accounting and libuv's argv quoting.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use pir::env_daemon::frame::{self, Frame, CANCEL, ERROR, EVENT, PING, REQUEST, RESULT};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Wire client (the test-side half of docs/protocol.md)
// ---------------------------------------------------------------------------

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A spawned daemon plus a reader thread turning stdout into frames.
struct Daemon {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Frame>,
    /// Events by request id, in arrival order (one request may stream many).
    events: HashMap<u32, Vec<(Value, Vec<u8>)>>,
    next_id: u32,
}

impl Daemon {
    /// Start `pir env-daemon serve --token <token>` and consume the sync line.
    fn start() -> Daemon {
        Daemon::start_with_env(&[])
    }

    /// [`Daemon::start`] with the file-op worker-pool override
    /// (`PIR_ENV_DAEMON_WORKERS`), for the low-resource sizing tests.
    fn start_with_workers(workers: Option<&str>) -> Daemon {
        match workers {
            Some(workers) => Daemon::start_with_env(&[("PIR_ENV_DAEMON_WORKERS", workers)]),
            None => Daemon::start(),
        }
    }

    /// [`Daemon::start`] with explicit environment overrides (admission caps
    /// and worker sizing), per-test so parallel daemons cannot see each
    /// other's configuration.
    fn start_with_env(envs: &[(&str, &str)]) -> Daemon {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
        command
            .args(["env-daemon", "serve", "--token", TOKEN])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn pir env-daemon");
        let stdin = child.stdin.take().expect("daemon stdin");
        let mut stdout = child.stdout.take().expect("daemon stdout");
        // The documented sync line: everything before `PI-ENV <token>\n` is
        // diagnostic noise from the transport.
        let marker = format!("PI-ENV {TOKEN}\n");
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while !seen.ends_with(marker.as_bytes()) {
            let read = stdout.read(&mut byte).expect("read the sync line");
            assert_ne!(read, 0, "daemon ended before its sync line");
            seen.push(byte[0]);
        }
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(frame)) = frame::read_frame(&mut stdout) {
                if sender.send(frame).is_err() {
                    return;
                }
            }
        });
        Daemon {
            child,
            stdin,
            frames,
            events: HashMap::new(),
            next_id: 1,
        }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, kind: u8, id: u32, json: Value, payload: &[u8]) {
        let frame = Frame::with_payload(kind, id, json, payload.to_vec());
        frame::write_frame(&mut self.stdin, &frame).expect("write frame");
    }

    /// One request; returns its result or error frame. Events are kept for
    /// `events()`; pings are skipped (the daemon pings every five seconds).
    fn request(&mut self, op: &str, mut json: Value, payload: &[u8]) -> Frame {
        let id = self.id();
        json["op"] = json!(op);
        self.send(REQUEST, id, json, payload);
        self.wait_for(id)
    }

    fn wait_for(&mut self, id: u32) -> Frame {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            match self.frames.recv_timeout(left) {
                Ok(frame) => match frame.kind {
                    PING => continue,
                    EVENT => {
                        self.events
                            .entry(frame.id)
                            .or_default()
                            .push((frame.json.clone(), frame.payload.clone()));
                    }
                    _ if frame.id == id => return frame,
                    _ => continue,
                },
                Err(RecvTimeoutError::Timeout) => panic!("no answer for request {id} in 30 s"),
                Err(RecvTimeoutError::Disconnected) => panic!("daemon stdout ended"),
            }
        }
    }

    /// The event frames of one request, as `(json, payload)` pairs.
    fn events(&mut self, id: u32) -> Vec<(Value, Vec<u8>)> {
        self.events.remove(&id).unwrap_or_default()
    }

    /// The token used by the sync line (for the shim-style byte helpers).
    fn token() -> &'static str {
        TOKEN
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Closing stdin makes the daemon end (it kills every command it started).
        let _ = self.child.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A hermetic temporary directory, removed on drop.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(name: &str) -> Tree {
        let root = std::env::temp_dir().join(format!(
            "pir-env-daemon-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Tree(root)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.0.join(path);
        if path.to_string_lossy().ends_with('/') || path.to_string_lossy().ends_with('\\') {
            std::fs::create_dir_all(&path).expect("create dir");
            return;
        }
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, content).expect("write file");
    }

    fn path(&self, path: &str) -> String {
        self.0.join(path).to_string_lossy().into_owned()
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

/// `hello`, then the assertions every client makes about the machine report.
fn hello(daemon: &mut Daemon) -> Value {
    let frame = daemon.request("hello", json!({ "protocol": 1 }), &[]);
    assert_eq!(frame.kind, RESULT, "hello must succeed: {:?}", frame.json);
    let json = frame.json.clone();
    assert_eq!(json["protocol"], json!(1), "protocol 1: {json}");
    assert!(
        json["features"]
            .as_array()
            .is_some_and(|features| features.iter().any(|value| value == "listFiles")),
        "listFiles is advertised: {json}"
    );
    assert_eq!(
        json["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "hello.version follows the package version: {json}"
    );
    for key in [
        "version",
        "os",
        "arch",
        "home",
        "separator",
        "tmpdir",
        "cwd",
    ] {
        assert!(json[key].is_string(), "hello.{key} is a string: {json}");
    }
    assert!(
        json["pid"].as_u64().is_some_and(|pid| pid > 0),
        "hello.pid: {json}"
    );
    assert!(json["driveCwds"].is_object(), "hello.driveCwds: {json}");
    json
}

// ---------------------------------------------------------------------------
// End-to-end: the documented handshake and op set over the real binary
// ---------------------------------------------------------------------------

#[test]
fn starts_with_the_sync_line_and_answers_hello() {
    let mut daemon = Daemon::start();
    let hello = hello(&mut daemon);
    // The daemon reports the system it runs on, in Rust's spelling.
    assert_eq!(hello["os"], json!(std::env::consts::OS));
    assert_eq!(hello["arch"], json!(std::env::consts::ARCH));
    assert_eq!(
        hello["separator"],
        json!(if cfg!(windows) { "\\" } else { "/" })
    );
}

#[test]
fn writes_then_reads_a_file_back() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("roundtrip");
    let path = tree.path("nested/deep.txt");

    // `write` creates missing parents (Node's recursive mkdir) and answers {}.
    let written = daemon.request(
        "write",
        json!({ "path": path, "append": false, "keep": false }),
        b"hello pi-env\n",
    );
    assert_eq!(written.kind, RESULT, "write: {:?}", written.json);
    assert_eq!(written.json, json!({}));

    // A second write without `append` truncates (Node's `writeFile` `w`).
    daemon.request(
        "write",
        json!({ "path": path, "append": false, "keep": false }),
        b"hello",
    );
    daemon.request(
        "write",
        json!({ "path": path, "append": true, "keep": false }),
        b" env\n",
    );

    let stat = daemon.request("lstat", json!({ "path": path }), &[]);
    assert_eq!(stat.kind, RESULT, "lstat: {:?}", stat.json);
    assert_eq!(stat.json["kind"], json!("file"));
    assert_eq!(stat.json["size"], json!(10));
    assert_eq!(stat.json["name"], json!("deep.txt"));

    // `open` + `pread` is how the client reads whole files.
    let opened = daemon.request("open", json!({ "path": path, "noFollow": false }), &[]);
    assert_eq!(opened.kind, RESULT, "open: {:?}", opened.json);
    let handle = opened.json["handle"].as_u64().expect("handle");
    assert_eq!(opened.json["info"]["kind"], json!("file"));
    let read = daemon.request(
        "pread",
        json!({ "handle": handle, "offset": 0, "length": 64 }),
        &[],
    );
    assert_eq!(read.kind, RESULT);
    assert_eq!(read.payload, b"hello env\n");
    daemon.request("close", json!({ "handle": handle }), &[]);

    // realpath resolves to the same file, mkdir/rename/rm round out the family.
    let real = daemon.request("realpath", json!({ "path": path }), &[]);
    assert_eq!(real.kind, RESULT, "realpath: {:?}", real.json);
    assert!(real.json["path"].is_string());
    let dir = tree.path("made");
    assert_eq!(
        daemon
            .request("mkdir", json!({ "path": dir, "recursive": true }), &[])
            .kind,
        RESULT
    );
    // An existing directory is fine for a recursive mkdir; a file is EEXIST.
    assert_eq!(
        daemon
            .request("mkdir", json!({ "path": dir, "recursive": true }), &[])
            .kind,
        RESULT
    );
    let file_in_the_way = daemon.request("mkdir", json!({ "path": path, "recursive": true }), &[]);
    assert_eq!(file_in_the_way.kind, ERROR);
    assert_eq!(file_in_the_way.json["code"], json!("EEXIST"));
    let renamed = tree.path("renamed.txt");
    assert_eq!(
        daemon
            .request("rename", json!({ "path": path, "to": renamed }), &[])
            .kind,
        RESULT
    );
    assert!(std::path::Path::new(&renamed).exists());
    assert_eq!(
        daemon
            .request(
                "rm",
                json!({ "path": renamed, "force": false, "recursive": false }),
                &[]
            )
            .kind,
        RESULT
    );
    assert!(!std::path::Path::new(&renamed).exists());
    // Node's `rm`: a missing path fails unless `force`.
    assert_eq!(
        daemon
            .request("rm", json!({ "path": renamed, "force": false }), &[])
            .json["code"],
        json!("ENOENT")
    );
    assert_eq!(
        daemon
            .request("rm", json!({ "path": renamed, "force": true }), &[])
            .kind,
        RESULT
    );
}

#[test]
fn kept_write_handles_append_chunks_in_order() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("chunks");
    let path = tree.path("chunked.bin");
    let opened = daemon.request(
        "write",
        json!({ "path": path, "append": false, "keep": true }),
        b"first:",
    );
    assert_eq!(opened.kind, RESULT, "write keep: {:?}", opened.json);
    let handle = opened.json["handle"].as_u64().expect("write handle");
    for chunk in [&b"second"[..], &b"third"[..]] {
        let reply = daemon.request("writeChunk", json!({ "handle": handle }), chunk);
        assert_eq!(reply.kind, RESULT, "writeChunk: {:?}", reply.json);
    }
    daemon.request("close", json!({ "handle": handle }), &[]);
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        b"first:secondthird"
    );
}

#[test]
fn lists_files_with_the_documented_payload_records() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("list");
    tree.write("b.txt", "b");
    tree.write("a/z.txt", "z");
    tree.write("a/b/c.txt", "c");
    tree.write("empty/", "");
    tree.write(".hidden/x", "x");
    tree.write(".git/config", "git");
    let frame = daemon.request("listFiles", json!({ "path": tree.root() }), &[]);
    assert_eq!(frame.kind, RESULT, "listFiles: {:?}", frame.json);
    // One record per entry: a kind byte (`f`/`d`/`l`), the relative path with
    // `/` separators, and a NUL. `.git` is skipped with everything below it.
    let records: Vec<String> = frame
        .payload
        .split(|byte| *byte == 0)
        .filter(|record| record.len() > 1)
        .map(|record| {
            format!(
                "{} {}",
                record[0] as char,
                String::from_utf8_lossy(&record[1..])
            )
        })
        .collect();
    assert_eq!(
        records,
        [
            "d .hidden",
            "f .hidden/x",
            "d a",
            "d a/b",
            "f a/b/c.txt",
            "f a/z.txt",
            "f b.txt",
            "d empty"
        ],
        "depth first, siblings in name order: {records:?}"
    );
    assert_eq!(frame.json["count"], json!(8));
    assert_eq!(frame.json["truncated"], json!(false));

    // `hidden: false` drops dot names; `exclude` skips a name with its subtree.
    let frame = daemon.request(
        "listFiles",
        json!({ "path": tree.root(), "hidden": false, "exclude": ["a"] }),
        &[],
    );
    let records: Vec<String> = frame
        .payload
        .split(|byte| *byte == 0)
        .filter(|record| record.len() > 1)
        .map(|record| String::from_utf8_lossy(&record[1..]).into_owned())
        .collect();
    assert_eq!(records, ["b.txt", "empty"]);

    // `maxEntries` truncates with the flag set (the bracket behaviour).
    let frame = daemon.request(
        "listFiles",
        json!({ "path": tree.root(), "maxEntries": 2 }),
        &[],
    );
    assert_eq!(frame.json["truncated"], json!(true));
    assert_eq!(frame.json["count"], json!(2));

    // A file is ENOTDIR, a missing path ENOENT.
    let file = daemon.request("listFiles", json!({ "path": tree.path("b.txt") }), &[]);
    assert_eq!(file.kind, ERROR);
    assert_eq!(file.json["code"], json!("ENOTDIR"));
    let missing = daemon.request("listFiles", json!({ "path": tree.path("nope") }), &[]);
    assert_eq!(missing.json["code"], json!("ENOENT"));
}

#[test]
fn opens_a_directory_and_pages_entries() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("dir");
    tree.write("one.txt", "1");
    tree.write("two.txt", "2");
    let opened = daemon.request("opendir", json!({ "path": tree.root() }), &[]);
    assert_eq!(opened.kind, RESULT, "opendir: {:?}", opened.json);
    let handle = opened.json["handle"].as_u64().expect("dir handle");
    let mut names = Vec::new();
    let mut done = false;
    while !done {
        let page = daemon.request("readdir", json!({ "handle": handle, "max": 1 }), &[]);
        assert_eq!(page.kind, RESULT, "readdir: {:?}", page.json);
        for entry in page.json["entries"].as_array().expect("entries") {
            names.push(entry["name"].as_str().expect("name").to_string());
            assert!(entry["info"]["kind"].is_string(), "entry info: {entry}");
        }
        done = page.json["done"] == json!(true);
    }
    names.sort();
    assert_eq!(names, ["one.txt", "two.txt"]);
    daemon.request("close", json!({ "handle": handle }), &[]);
    // The handle is gone after close.
    let gone = daemon.request("readdir", json!({ "handle": handle, "max": 10 }), &[]);
    assert_eq!(gone.json["code"], json!("EBADF"));
}

#[test]
fn scans_lines_with_the_documented_selection() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("scan");
    let path = tree.path("lines.txt");
    // Four newlines: "a", "\u{fffd}" (invalid), "", "\u{feff}b", then "é".
    std::fs::write(
        &path,
        [
            0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf, 0x62, 0x0a,
            0xc3, 0xa9,
        ],
    )
    .expect("fixture");
    let opened = daemon.request("open", json!({ "path": path, "mode": "read" }), &[]);
    let handle = opened.json["handle"].as_u64().expect("handle");
    let all = daemon.request(
        "scanLines",
        json!({ "handle": handle, "startLine": 0 }),
        &[],
    );
    assert_eq!(all.kind, RESULT, "scanLines: {:?}", all.json);
    assert_eq!(all.json["newlines"], json!(4));
    // "a\n\u{fffd}\n\n\u{feff}b\né" is 1+1+3+1+1+3+1+1+2 = 14 decoded bytes.
    assert_eq!(all.json["selectedBytes"], json!(14));
    assert_eq!(all.json["firstLineBytes"], json!(1));
    let middle = daemon.request(
        "scanLines",
        json!({ "handle": handle, "startLine": 1, "endLine": 3 }),
        &[],
    );
    assert_eq!(middle.json["selectedBytes"], json!(4));
    // An impossible range is EINVAL.
    let invalid = daemon.request(
        "scanLines",
        json!({ "handle": handle, "startLine": 3, "endLine": 3 }),
        &[],
    );
    assert_eq!(invalid.kind, ERROR);
    assert_eq!(invalid.json["code"], json!("EINVAL"));
    daemon.request("close", json!({ "handle": handle }), &[]);
}

#[test]
fn execs_a_shell_command_with_streaming_output() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("exec");
    let id = daemon.id();
    // A string runs through the shell (the daemon resolves `/bin/bash` on
    // POSIX, Git Bash or `where bash.exe` on Windows).
    daemon.send(
        REQUEST,
        id,
        json!({
            "op": "exec",
            "command": "echo out && echo err 1>&2 && exit 3",
            "cwd": tree.root(),
            "env": {},
            "inheritEnv": true,
        }),
        &[],
    );
    let frame = daemon.wait_for(id);
    assert_eq!(frame.kind, RESULT, "exec: {:?}", frame.json);
    assert_eq!(
        frame.json["exitCode"],
        json!(3),
        "the shell's own exit code"
    );
    let events = daemon.events(id);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (json, payload) in &events {
        assert_eq!(json["kind"], json!("output"));
        match json["stream"].as_str() {
            Some("stdout") => stdout.extend_from_slice(payload),
            Some("stderr") => stderr.extend_from_slice(payload),
            other => panic!("unexpected stream {other:?}"),
        }
    }
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    // Windows shells end lines with CRLF, as the daemon reports them.
    assert_eq!(stdout.trim_end(), "out", "stdout payload: {stdout:?}");
    assert_eq!(stderr.trim_end(), "err", "stderr payload: {stderr:?}");
}

#[test]
fn execs_an_argv_command_without_a_shell() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("argv");
    let argv = if cfg!(windows) {
        json!(["cmd", "/c", "echo argv-ok"])
    } else {
        json!(["echo", "argv-ok"])
    };
    let id = daemon.id();
    daemon.send(
        REQUEST,
        id,
        json!({ "op": "exec", "argv": argv, "cwd": tree.root(), "inheritEnv": true }),
        &[],
    );
    let frame = daemon.wait_for(id);
    assert_eq!(frame.kind, RESULT, "argv exec: {:?}", frame.json);
    assert_eq!(frame.json["exitCode"], json!(0));
    let events = daemon.events(id);
    let text: String = events
        .iter()
        .filter(|(json, _)| json["stream"] == json!("stdout"))
        .map(|(_, payload)| String::from_utf8_lossy(payload).into_owned())
        .collect();
    assert!(text.contains("argv-ok"), "stdout: {text:?}");

    // A missing program is a spawn_error, and a missing cwd too.
    let id = daemon.id();
    daemon.send(
        REQUEST,
        id,
        json!({ "op": "exec", "argv": ["definitely-not-a-program-xyz"], "cwd": tree.root() }),
        &[],
    );
    let frame = daemon.wait_for(id);
    assert_eq!(frame.kind, ERROR);
    assert_eq!(frame.json["code"], json!("spawn_error"));
    let id = daemon.id();
    daemon.send(
        REQUEST,
        id,
        json!({ "op": "exec", "argv": ["echo", "x"], "cwd": tree.path("missing") }),
        &[],
    );
    assert_eq!(daemon.wait_for(id).json["code"], json!("spawn_error"));
}

#[test]
fn cancels_a_running_exec_with_aborted() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("cancel");
    let id = daemon.id();
    // A command that outlives the cancel on both platforms.
    let command = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    daemon.send(
        REQUEST,
        id,
        json!({ "op": "exec", "command": command, "cwd": tree.root(), "inheritEnv": true }),
        &[],
    );
    // The daemon registers the request before running it, so a cancel arriving
    // right after the request is never lost.
    std::thread::sleep(Duration::from_millis(500));
    daemon.send(CANCEL, id, json!({}), &[]);
    let frame = daemon.wait_for(id);
    assert_eq!(
        frame.kind, ERROR,
        "cancel finishes the request: {:?}",
        frame.json
    );
    assert_eq!(frame.json["code"], json!("aborted"));

    // The connection still works afterwards.
    let again = daemon.request(
        "exec",
        json!({ "op": "exec", "command": "echo alive", "cwd": tree.root(), "inheritEnv": true }),
        &[],
    );
    assert_eq!(again.kind, RESULT, "after cancel: {:?}", again.json);
}

#[test]
fn reports_timeouts_and_unknown_operations_as_error_frames() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("errors");
    // An unknown op is EINVAL (the client checks `features` first for op gaps).
    let unknown = daemon.request("nosuchop", json!({}), &[]);
    assert_eq!(unknown.kind, ERROR);
    assert_eq!(unknown.json["code"], json!("EINVAL"));
    // Every error carries `code` and `message`; path/syscall come along where they apply.
    assert!(unknown.json["message"].is_string(), "{:?}", unknown.json);
    let missing = daemon.request("lstat", json!({ "path": tree.path("gone") }), &[]);
    assert_eq!(missing.kind, ERROR);
    assert_eq!(missing.json["code"], json!("ENOENT"));
    assert_eq!(missing.json["syscall"], json!("lstat"));
    assert!(missing.json["path"].is_string(), "{:?}", missing.json);
    // A missing field is EINVAL, not a crash.
    assert_eq!(
        daemon.request("lstat", json!({}), &[]).json["code"],
        json!("EINVAL")
    );
    // `timeout` is a documented execution code.
    let id = daemon.id();
    let command = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    daemon.send(
        REQUEST,
        id,
        json!({
            "op": "exec",
            "command": command,
            "cwd": tree.root(),
            "timeoutMs": 300,
        }),
        &[],
    );
    let timed_out = daemon.wait_for(id);
    assert_eq!(timed_out.kind, ERROR);
    assert_eq!(timed_out.json["code"], json!("timeout"));
}

#[test]
fn pty_is_enotsup_on_windows_and_runs_elsewhere() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    let tree = Tree::new("pty");
    let id = daemon.id();
    daemon.send(
        REQUEST,
        id,
        json!({
            "op": "pty",
            "cwd": tree.root(),
            "cols": 80,
            "rows": 24,
            "env": { "TERM": "xterm-256color" },
        }),
        &[],
    );
    if cfg!(windows) {
        // Upstream refuses terminals on Windows; the client maps `ENOTSUP` to
        // an `unknown` ExecutionError, exactly as it does with a pi-env daemon.
        let frame = daemon.wait_for(id);
        assert_eq!(frame.kind, ERROR);
        assert_eq!(frame.json["code"], json!("ENOTSUP"), "{:?}", frame.json);
        return;
    }
    // POSIX: the running program is the login shell on the new terminal. The
    // `started` event comes first (the client's open() resolves on it), and
    // `ptyWrite` is answered in arrival order by the frame reader.
    std::thread::sleep(Duration::from_millis(300));
    let write = daemon.id();
    daemon.send(
        REQUEST,
        write,
        json!({ "op": "ptyWrite", "target": id }),
        b"exit\n",
    );
    assert_eq!(daemon.wait_for(write).kind, RESULT);
    let frame = daemon.wait_for(id);
    assert_eq!(frame.kind, RESULT, "pty: {:?}", frame.json);
    assert!(
        frame.json["exitCode"].as_i64().is_some(),
        "{:?}",
        frame.json
    );
    let events = daemon.events(id);
    assert_eq!(events[0].0["kind"], json!("started"));
    assert!(events[0].0["pid"].as_u64().is_some_and(|pid| pid > 0));
}

#[test]
fn pings_keep_the_connection_alive_and_unknown_ids_are_ignored() {
    let mut daemon = Daemon::start();
    hello(&mut daemon);
    // An unsolicited cancel for an unknown id is dropped, not answered.
    daemon.send(CANCEL, 9_999_999, json!({}), &[]);
    // A ping from the client is ignored by the receiver.
    daemon.send(PING, 0, json!({}), &[]);
    let tree = Tree::new("alive");
    let reply = daemon.request("lstat", json!({ "path": tree.root() }), &[]);
    assert_eq!(reply.kind, RESULT, "still answering: {:?}", reply.json);
    assert_eq!(reply.json["kind"], json!("directory"));
    assert_eq!(Daemon::token(), TOKEN);
}

// ---------------------------------------------------------------------------
// Ported vectors (upstream inline unit tests)
// ---------------------------------------------------------------------------

#[test]
fn frame_codec_round_trips_json_and_payload() {
    let mut buffer = Vec::new();
    frame::write_frame(
        &mut buffer,
        &Frame::with_payload(RESULT, 7, json!({ "a": 1 }), vec![0, 255, 10]),
    )
    .expect("write");
    let parsed = frame::read_frame(&mut buffer.as_slice())
        .expect("read")
        .expect("one frame");
    assert_eq!((parsed.kind, parsed.id), (RESULT, 7));
    assert_eq!(parsed.json, json!({ "a": 1 }));
    assert_eq!(parsed.payload, vec![0, 255, 10]);
    assert!(frame::read_frame(&mut &[][..])
        .expect("clean end")
        .is_none());

    // A frame length outside [9, 16 MiB] ends the connection (framing error).
    let mut bad = Vec::new();
    bad.extend_from_slice(&8u32.to_be_bytes());
    assert!(frame::read_frame(&mut bad.as_slice()).is_err());
    let mut too_big = Vec::new();
    too_big.extend_from_slice(&((frame::MAX_FRAME + 1) as u32).to_be_bytes());
    assert!(frame::read_frame(&mut too_big.as_slice()).is_err());

    // Unparseable JSON keeps the framing and arrives as `Null` (an EINVAL reply
    // from the dispatcher), it does not end the connection.
    let mut buffer = Vec::new();
    let body = b"{not json";
    buffer.extend_from_slice(&(9u32 + body.len() as u32).to_be_bytes());
    buffer.push(REQUEST);
    buffer.extend_from_slice(&1u32.to_be_bytes());
    buffer.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buffer.extend_from_slice(body);
    let parsed = frame::read_frame(&mut buffer.as_slice())
        .expect("read")
        .expect("one frame");
    assert!(parsed.json.is_null());
}

#[test]
fn large_frame_payload_round_trips_byte_for_byte() {
    // A multi-megabyte payload, larger than any JSON the frame carries: the
    // decoder's payload must be exactly the tail bytes after the JSON. The
    // old body-allocation-plus-copy shape returned the same bytes, so this
    // pins the wire/decoder contract while the decode now fills the payload
    // buffer directly.
    let payload: Vec<u8> = (0..2 * 1024 * 1024usize)
        .map(|index| (index.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect();
    let mut buffer = Vec::new();
    frame::write_frame(
        &mut buffer,
        &Frame::with_payload(
            RESULT,
            4_242,
            json!({ "kind": "large", "n": payload.len() }),
            payload.clone(),
        ),
    )
    .expect("write");
    let parsed = frame::read_frame(&mut buffer.as_slice())
        .expect("read")
        .expect("one frame");
    assert_eq!((parsed.kind, parsed.id), (RESULT, 4_242));
    assert_eq!(parsed.json["kind"], json!("large"));
    assert_eq!(parsed.json["n"].as_u64(), Some(payload.len() as u64));
    assert_eq!(
        parsed.payload, payload,
        "the large payload must survive byte for byte"
    );
}

#[test]
fn stream_decoder_matches_whole_input_whatwg_rules() {
    use pir::env_daemon::decode::StreamDecoder;

    // A U+FEFF that does not start the stream is kept; the leading BOM is dropped.
    let bytes = [0xe2, 0x82, 0xef, 0xbb, 0xbf, 0x61];
    let mut decoder = StreamDecoder::new();
    let text: String = bytes
        .iter()
        .map(|byte| decoder.decode(&[*byte], false))
        .collect::<String>()
        + &decoder.decode(&[], true);
    assert_eq!(text, "\u{fffd}\u{feff}a");

    // Only a leading mark is dropped, and UTF-16 BOMs are never sniffed.
    let mut decoder = StreamDecoder::new();
    assert_eq!(
        decoder.decode(&[0xef, 0xbb], false) + &decoder.decode(&[0xbf, 0x61], true),
        "a"
    );
    let mut decoder = StreamDecoder::new();
    assert_eq!(
        decoder.decode(&[0xff, 0xfe, 0x41, 0x00], true),
        "\u{fffd}\u{fffd}A\u{0}"
    );

    // An incomplete sequence becomes one replacement, at the end of the stream.
    let mut decoder = StreamDecoder::new();
    assert_eq!(decoder.decode(&[0xe2, 0x82, 0x41], true), "\u{fffd}A");
    let mut decoder = StreamDecoder::new();
    assert_eq!(decoder.decode(&[0xe2, 0x82], true), "\u{fffd}");
}

#[test]
fn line_scanner_locates_and_measures_lines() {
    use pir::env_daemon::scan::LineScanner;

    fn scan(file: &[u8], start: u64, end: Option<u64>, chunk: usize) -> Value {
        let mut scanner = LineScanner::new(start, end);
        for piece in file.chunks(chunk.max(1)) {
            scanner.push(piece);
        }
        scanner.finish()
    }

    // BOM, "a", an invalid sequence, an empty line, a later U+FEFF, no final newline.
    let file = [
        0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf, 0x62, 0x0a, 0xc3,
        0xa9,
    ];
    for chunk in 1..6 {
        let all = scan(&file, 0, None, chunk);
        assert_eq!(all["newlines"], 4, "chunk {chunk}");
        // "a\n\u{fffd}\n\n\u{feff}b\né" is 1+1+3+1+1+3+1+1+2 = 14 bytes.
        assert_eq!(all["selectedBytes"], 14, "chunk {chunk}");
        assert_eq!(all["firstLineBytes"], 1, "chunk {chunk}");
        let middle = scan(&file, 1, Some(3), chunk);
        assert_eq!(
            (middle["start"].as_u64(), middle["end"].as_u64()),
            (Some(5), Some(8)),
            "chunk {chunk}"
        );
        assert_eq!(middle["selectedBytes"], 4, "chunk {chunk}");
        assert_eq!(middle["lastLineStart"], 8, "chunk {chunk}");
        let past = scan(&file, 9, None, chunk);
        assert_eq!(past["start"], 16, "chunk {chunk}");
    }
}

#[test]
fn output_window_counts_everything_and_keeps_the_tail() {
    use pir::env_daemon::window::{Pending, Window};
    use std::time::Duration;

    fn window(max_bytes: usize, max_lines: usize) -> Window {
        Window {
            max_bytes,
            max_lines,
            min_interval: Duration::ZERO,
            bytes_per_second: 1e12,
        }
    }

    fn newline_count(text: &str) -> usize {
        text.bytes().filter(|byte| *byte == b'\n').count()
    }

    /// Bytes and newlines delivered or skipped, and the text after the last skip.
    fn total(events: &[pir::env_daemon::window::Event]) -> (u64, u64, String) {
        let mut bytes = 0;
        let mut newlines = 0;
        let mut after = String::new();
        for event in events {
            if let Some(skipped) = &event.skipped {
                bytes += skipped["bytes"].as_u64().expect("bytes");
                newlines += skipped["newlines"].as_u64().expect("newlines");
                after.clear();
            }
            bytes += event.text.len() as u64;
            newlines += newline_count(&event.text) as u64;
            after.push_str(&event.text);
        }
        (bytes, newlines, after)
    }

    let mut pending = Pending::new(window(10, 2));
    let mut full = String::new();
    let mut events = Vec::new();
    for index in 0..500 {
        let text = format!("line {index} é😀\n");
        full.push_str(&text);
        pending.push(if index % 7 == 0 { "stderr" } else { "stdout" }, text);
        if index % 50 == 49 {
            events.extend(pending.take());
        }
    }
    events.extend(pending.take());
    let (bytes, newlines, after) = total(&events);
    assert_eq!(
        bytes,
        full.len() as u64,
        "counts everything, delivered or skipped"
    );
    assert_eq!(newlines, newline_count(&full) as u64);
    assert!(full.ends_with(&after));
    assert!(after.len() > 10 || newline_count(&after) > 2);
    assert!(events.iter().any(|event| event.skipped.is_some()));

    // Without a skip, the held streams stay apart, in arrival order.
    let mut pending = Pending::new(window(1000, 1000));
    pending.push("stdout", "a".into());
    pending.push("stderr", "b".into());
    pending.push("stdout", "c".into());
    let streams: Vec<_> = pending.take().iter().map(|event| event.stream).collect();
    assert_eq!(streams, ["stdout", "stderr", "stdout"]);
}

#[test]
fn list_files_walker_matches_fd_and_gitignore_semantics() {
    use pir::env_daemon::list::{list, Options};

    let tree = Tree::new("walker");
    for file in [
        "b.txt",
        "a/z.txt",
        "a/b/c.txt",
        "empty/",
        ".hidden/x",
        "A.txt",
    ] {
        tree.write(file, "");
    }
    let options = Options::from_json(&json!({})).expect("defaults");
    let root = tree.root();
    let listing = list(&root, &options, &|| false).expect("list");
    let entries: Vec<String> = listing
        .payload
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            format!(
                "{} {}",
                record[0] as char,
                String::from_utf8_lossy(&record[1..])
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            "d .hidden",
            "f .hidden/x",
            "f A.txt",
            "d a",
            "d a/b",
            "f a/b/c.txt",
            "f a/z.txt",
            "f b.txt",
            "d empty"
        ],
        "depth first, siblings in byte order"
    );
    assert!(!listing.truncated);

    // `hidden: false` drops dot names (fd's `--hidden` default).
    let listing = list(
        &root,
        &Options::from_json(&json!({ "hidden": false })).expect("options"),
        &|| false,
    )
    .expect("list");
    let text = String::from_utf8_lossy(&listing.payload);
    assert!(!text.contains(".hidden"), "{text}");

    // Git ignore rules apply only inside a work tree, and a subdirectory of one
    // applies its parents' rules. The file set is upstream's unit-test tree.
    let git_tree = Tree::new("walker-git");
    for file in [
        "keep.txt",
        "out/x.js",
        "sub/keep.txt",
        "sub/drop.log",
        "node_modules/m/i.js",
    ] {
        git_tree.write(file, "");
    }
    let git_root = git_tree.root();
    git_tree.write(".gitignore", "out/\n*.log\n");
    git_tree.write("sub/.gitignore", "!keep.txt\n");
    let listing = list(&git_root, &options, &|| false).expect("list");
    let text = String::from_utf8_lossy(&listing.payload);
    assert!(text.contains("out/x.js"), "not a work tree yet: {text}");
    git_tree.write(".git/HEAD", "ref: refs/heads/main\n");
    git_tree.write(".git/info/exclude", "keep.txt\n");
    let options_excl =
        Options::from_json(&json!({ "exclude": ["node_modules"] })).expect("options");
    let listing = list(&git_root, &options_excl, &|| false).expect("list");
    let entries: Vec<String> = listing
        .payload
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            format!(
                "{} {}",
                record[0] as char,
                String::from_utf8_lossy(&record[1..])
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            "f .gitignore",
            "d sub",
            "f sub/.gitignore",
            "f sub/keep.txt"
        ],
        "ignore rules of the work tree, `.git` skipped"
    );
    let listing = list(
        &git_root,
        &Options::from_json(&json!({ "gitignore": false })).expect("options"),
        &|| false,
    )
    .expect("list");
    let text = String::from_utf8_lossy(&listing.payload);
    assert!(text.contains("out/x.js"), "{text}");
    assert!(!text.contains(".git/"), "{text}");
    let sub = list(
        &git_tree.path("sub"),
        &Options::from_json(&json!({})).expect("options"),
        &|| false,
    )
    .expect("list");
    assert_eq!(sub.payload, b"f.gitignore\0fkeep.txt\0");

    // Bounds stop with `truncated`; a missing root is ENOENT, a file ENOTDIR,
    // and an aborted walk reports `aborted`.
    let listing = list(
        &root,
        &Options::from_json(&json!({ "maxEntries": 2 })).expect("options"),
        &|| false,
    )
    .expect("list");
    assert_eq!((listing.count, listing.truncated), (2, true));
    let missing = list(&tree.path("gone"), &options, &|| false)
        .err()
        .expect("missing root");
    assert_eq!(missing.code, "ENOENT");
    let file = list(&tree.path("b.txt"), &options, &|| false)
        .err()
        .expect("file root");
    assert_eq!(file.code, "ENOTDIR");
    let aborted = list(&root, &options, &|| true).err().expect("aborted");
    assert_eq!(aborted.code, "aborted");
}

#[cfg(unix)]
#[test]
fn list_files_reports_symbolic_links_without_following_them() {
    use pir::env_daemon::list::{list, Options};
    let tree = Tree::new("symlinks");
    tree.write("dir/inner.txt", "");
    // Canonicalize the fixture root once: `temp_dir()` may itself be reached
    // through a symlinked prefix (containers/CI export TMPDIR through a
    // link), and the walker derives every entry path from exactly the root
    // string it is given. Building the links and the listing from the same
    // canonical root keeps `strip_prefix` and the link targets consistent.
    let root = std::fs::canonicalize(tree.root()).expect("canonical fixture root");
    std::os::unix::fs::symlink(root.join("dir"), root.join("link")).expect("symlink");
    std::os::unix::fs::symlink("missing", root.join("dangling")).expect("symlink");
    std::os::unix::fs::symlink(&root, root.join("dir/loop")).expect("symlink");
    let root_text = root.to_string_lossy().into_owned();
    let listing = list(
        &root_text,
        &Options::from_json(&json!({})).expect("options"),
        &|| false,
    )
    .expect("list");
    let records: Vec<(char, String)> = listing
        .payload
        .split(|byte| *byte == 0)
        .filter(|record| record.len() > 1)
        .map(|record| {
            (
                record[0] as char,
                String::from_utf8_lossy(&record[1..]).into_owned(),
            )
        })
        .collect();
    // The contract covers the symlink semantics (reported as `l`, never
    // followed); the depth-first byte order itself is pinned by
    // `list_files_walker_matches_fd_and_gitignore_semantics`, so do not make
    // this fixture depend on walker ordering between the link and the file.
    let mut names: Vec<&str> = records.iter().map(|(_, name)| name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["dangling", "dir", "dir/inner.txt", "dir/loop", "link"],
        "symlinks are listed as leaves: {records:?}"
    );
    let kind = |name: &str| {
        records
            .iter()
            .find(|(_, record)| record.as_str() == name)
            .map(|(kind, _)| *kind)
    };
    assert_eq!(kind("dangling"), Some('l'), "{records:?}");
    assert_eq!(kind("dir"), Some('d'), "{records:?}");
    assert_eq!(kind("dir/inner.txt"), Some('f'), "{records:?}");
    assert_eq!(kind("dir/loop"), Some('l'), "{records:?}");
    assert_eq!(kind("link"), Some('l'), "{records:?}");
}

#[test]
fn error_codes_follow_libuvs_names() {
    use pir::env_daemon::errors::Failure;

    let plain = Failure::new("EINVAL", "missing field path").to_json();
    assert_eq!(
        plain,
        json!({ "code": "EINVAL", "message": "missing field path" })
    );

    // `Failure::io` names the OS error the way Node (libuv) does and carries
    // the syscall and path the client maps to a FileError code.
    let error = std::io::Error::from_raw_os_error(2); // ENOENT on Windows and Unix alike
    let failure = Failure::io(&error, "lstat", "/tmp/pi-env-missing").to_json();
    assert_eq!(failure["code"], json!("ENOENT"), "{failure}");
    assert_eq!(failure["syscall"], json!("lstat"));
    assert_eq!(failure["path"], json!("/tmp/pi-env-missing"));
    assert!(
        failure["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("ENOENT: no such file or directory, lstat")),
        "{failure}"
    );

    // A two-path call spells both paths, as Node does.
    let failure = Failure::io_between(&error, "rename", "/a", Some("/b")).to_json();
    assert!(
        failure["message"]
            .as_str()
            .is_some_and(|message| message.contains("rename '/a' -> '/b'")),
        "{failure}"
    );
}

#[cfg(windows)]
#[test]
fn quotes_arguments_like_libuv() {
    use pir::env_daemon::sys::quote_argument;

    assert_eq!(quote_argument(""), "\"\"");
    assert_eq!(quote_argument("plain"), "plain");
    assert_eq!(quote_argument("a b"), "\"a b\"");
    assert_eq!(quote_argument("*.txt"), "*.txt");
    assert_eq!(quote_argument("say \"hi\""), "\"say \\\"hi\\\"\"");
    assert_eq!(
        quote_argument("C:\\dir with space\\"),
        "\"C:\\dir with space\\\\\""
    );
    assert_eq!(quote_argument("a\\\\b c"), "\"a\\\\b c\"");
}

// ---------------------------------------------------------------------------
// Low-resource thread model (mini-device lane)
// ---------------------------------------------------------------------------

/// The daemon's thread model on 1-2 core / 512 MB devices: the file-op pool
/// follows `available_parallelism()` (bounded), not the upstream constant 16,
/// and an explicit `PIR_ENV_DAEMON_WORKERS` wins (bounded too).
#[test]
fn worker_pool_is_sized_for_small_devices() {
    use pir::env_daemon::daemon_worker_count_from;

    // Adaptive: at least MIN_WORKERS (a single-core device still gets one
    // spare thread so a slow disk op cannot serialize the console), at most
    // MAX_WORKERS (a 64-core host does not need 64 idle pool threads).
    assert_eq!(daemon_worker_count_from(None, 1), 2);
    assert_eq!(daemon_worker_count_from(None, 2), 2);
    assert_eq!(daemon_worker_count_from(None, 4), 4);
    assert_eq!(daemon_worker_count_from(None, 8), 8);
    assert_eq!(daemon_worker_count_from(None, 64), 8);

    // Explicit override, clamped; unparseable values fall back to adaptive.
    assert_eq!(daemon_worker_count_from(Some("1"), 8), 1);
    assert_eq!(daemon_worker_count_from(Some("4"), 1), 4);
    assert_eq!(daemon_worker_count_from(Some("999"), 2), 64);
    assert_eq!(daemon_worker_count_from(Some("0"), 4), 4);
    assert_eq!(daemon_worker_count_from(Some("many"), 4), 4);
    assert_eq!(daemon_worker_count_from(Some(""), 4), 4);
}

/// Per-request stacks: only the recursively scanning `watch` keeps the
/// upstream 16 MiB; `exec`/`pty` run on the Rust default instead of reserving
/// 16 MiB per concurrent request.
#[test]
fn long_lived_request_stacks_are_sized_per_op() {
    use pir::env_daemon::request_stack_bytes;

    const MIB: usize = 1024 * 1024;
    assert_eq!(request_stack_bytes("watch"), 16 * MIB);
    assert_eq!(request_stack_bytes("exec"), 2 * MIB);
    assert_eq!(request_stack_bytes("pty"), 2 * MIB);
}

/// The semantic contract must survive the smaller pool: with a ONE-worker
/// pool, `writeChunk`s on one handle still land in arrival order (the
/// per-handle serialization is independent of pool size), and the daemon still
/// serves the other ops.
#[test]
fn single_worker_pool_keeps_per_handle_chunk_order() {
    let mut daemon = Daemon::start_with_workers(Some("1"));
    hello(&mut daemon);
    let tree = Tree::new("one-worker");
    let path = tree.path("chunked.bin");
    let opened = daemon.request(
        "write",
        json!({ "path": path, "append": false, "keep": true }),
        b"first:",
    );
    assert_eq!(opened.kind, RESULT, "write keep: {:?}", opened.json);
    let handle = opened.json["handle"].as_u64().expect("write handle");
    for chunk in [&b"second"[..], &b"third"[..], &b"fourth"[..]] {
        let reply = daemon.request("writeChunk", json!({ "handle": handle }), chunk);
        assert_eq!(reply.kind, RESULT, "writeChunk: {:?}", reply.json);
    }
    daemon.request("close", json!({ "handle": handle }), &[]);
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        b"first:secondthirdfourth"
    );
    // A separate request still runs on the single worker: open + pread the
    // file back through the daemon.
    let opened = daemon.request("open", json!({ "path": path, "mode": "read" }), &[]);
    assert_eq!(opened.kind, RESULT, "open read: {:?}", opened.json);
    let handle = opened.json["handle"].as_u64().expect("read handle");
    let read = daemon.request(
        "pread",
        json!({ "handle": handle, "offset": 0, "length": 64 }),
        &[],
    );
    assert_eq!(read.kind, RESULT, "pread: {:?}", read.json);
    assert_eq!(read.payload, b"first:secondthirdfourth");
    daemon.request("close", json!({ "handle": handle }), &[]);
}

/// `exec`/`watch`/`pty` admission is capped: past the limit the daemon answers
/// a structured `EAGAIN` instead of spawning another thread/process tree.
/// The cap is lowered per daemon through the documented environment override
/// so the test needs one long command, not 32.
#[test]
fn long_requests_are_rejected_past_the_concurrency_cap() {
    let mut daemon = Daemon::start_with_env(&[("PIR_ENV_DAEMON_MAX_LONG_REQUESTS", "1")]);
    hello(&mut daemon);
    let tree = Tree::new("long-cap");
    // A command that outlives the rest of this test (and exits soon after it,
    // so a killed daemon leaves no long-lived child).
    let slow = if cfg!(windows) {
        json!(["cmd", "/c", "ping -n 6 127.0.0.1 > nul"])
    } else {
        json!(["sleep", "5"])
    };
    let first = daemon.id();
    daemon.send(
        REQUEST,
        first,
        json!({
            "op": "exec",
            "argv": slow,
            "cwd": tree.root(),
            "env": {},
            "inheritEnv": true,
        }),
        &[],
    );
    // Let the daemon admit and park on the first request.
    std::thread::sleep(Duration::from_millis(300));

    let second = daemon.id();
    daemon.send(
        REQUEST,
        second,
        json!({
            "op": "exec",
            "argv": ["definitely-not-a-program-xyz"],
            "cwd": tree.root(),
            "env": {},
            "inheritEnv": true,
        }),
        &[],
    );
    let rejected = daemon.wait_for(second);
    assert_eq!(
        rejected.kind, ERROR,
        "the request past the cap is refused: {:?}",
        rejected.json
    );
    assert_eq!(
        rejected.json["code"],
        json!("EAGAIN"),
        "the refusal is a structured EAGAIN: {:?}",
        rejected.json
    );
    assert!(
        rejected.json["message"]
            .as_str()
            .is_some_and(|message| message.contains("concurrent")),
        "the message names the admission limit: {:?}",
        rejected.json
    );
}

/// The pty input queue is bounded by depth AND bytes: a writer that outruns a
/// terminal nobody reads gets `EAGAIN` instead of queueing without limit.
#[cfg(unix)]
#[test]
fn pty_input_queue_is_bounded() {
    let mut daemon = Daemon::start_with_env(&[("PIR_ENV_DAEMON_MAX_LONG_REQUESTS", "4")]);
    hello(&mut daemon);
    let tree = Tree::new("pty-bound");
    let pty_id = daemon.id();
    daemon.send(
        REQUEST,
        pty_id,
        json!({
            "op": "pty",
            "argv": ["sleep", "5"],
            "cwd": tree.root(),
            "cols": 80,
            "rows": 24,
        }),
        &[],
    );
    // The terminal is registered before its `started` event is queued; give
    // the request thread a moment, then flood writes the program never reads.
    std::thread::sleep(Duration::from_millis(300));
    let payload = vec![b'x'; 16 * 1024];
    let mut saw_eagain = false;
    for _ in 0..256 {
        let id = daemon.id();
        daemon.send(
            REQUEST,
            id,
            json!({ "op": "ptyWrite", "target": pty_id }),
            &payload,
        );
        let reply = daemon.wait_for(id);
        if reply.kind == ERROR && reply.json["code"] == json!("EAGAIN") {
            saw_eagain = true;
            break;
        }
        assert_eq!(
            reply.kind, RESULT,
            "a ptyWrite below the cap succeeds: {:?}",
            reply.json
        );
    }
    assert!(
        saw_eagain,
        "a program that does not drain its terminal must eventually hit the pty input cap"
    );
}
