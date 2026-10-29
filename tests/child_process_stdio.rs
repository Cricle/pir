//! `child_process.spawn`'s `stdio` option: `"pipe"` / `"ignore"` /
//! `"inherit"` / a parent fd number (the `fs.openSync` descriptor), plus the
//! streaming shape the pi-subagents runner uses — `stdio: ["ignore", fdOut,
//! fdErr]` pointing the child's output straight at its own log files.
//!
//! Fixtures are generated at runtime into per-test temp dirs (repository
//! hygiene gate; same pattern as `tests/node_shims.rs`), and the child program
//! is the platform shell (`cmd.exe` / `sh`), so no rebuilt `pir` binary is
//! needed to drive them.

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-child-stdio-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config() -> ExtensionConfig {
    ExtensionConfig {
        cwd: std::env::temp_dir(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn run_fixture(rt: &ExtensionRuntime, name: &str, ts: &str, global: &str) -> Value {
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, ts).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture {name} must load");
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {name}: {:?}",
        outcome.errors
    );
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

/// Runtime + fixture in one call: the fixture source keeps its `__DIR__`
/// placeholder, which resolves to this test's temp workspace (forward slashes
/// so the Windows path needs no JS escaping).
async fn run_stdio_fixture(name: &str, body: &str) -> (Value, PathBuf) {
    let dir = write_temp_fixture(name, &[]);
    let ts = body.replace("__DIR__", &dir.to_string_lossy().replace('\\', "/"));
    let path = dir.join("index.ts");
    std::fs::write(&path, &ts).expect("write fixture");
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    let value = run_fixture(&rt, name, &ts, "__pirStdio").await;
    (value, dir)
}

/// The probes every fixture shares: one `spawn` helper that reports the
/// JS-visible stream shape, plus the platform shell lines.
const STDIO_HELPERS: &str = r#"
import { spawn, exec } from "node:child_process";
import fs from "node:fs";

const DIR = "__DIR__";
const isWin = process.platform === "win32";
const SHELL = isWin ? "cmd.exe" : "sh";
// One command that writes a marker to stdout and another to stderr.
const BOTH = isWin
  ? ["/C", "echo OUT_TEXT & echo ERR_TEXT 1>&2"]
  : ["-c", "echo OUT_TEXT; echo ERR_TEXT 1>&2"];

function readText(file) {
  try {
    return fs.readFileSync(file, "utf8");
  } catch (error) {
    return "READ_ERR:" + error.code;
  }
}

function spawnReport(options) {
  return new Promise(function (resolve) {
    const spawnOptions = Object.assign({ windowsHide: true }, options);
    const child = spawn(SHELL, BOTH, spawnOptions);
    const report = {
      stdoutIsNull: child.stdout === null,
      stderrIsNull: child.stderr === null,
      stdinIsNull: child.stdin === null,
      stdoutData: "",
      stderrData: "",
      events: [],
    };
    if (child.stdout) child.stdout.on("data", function (chunk) { report.stdoutData += chunk.toString("utf8"); });
    if (child.stderr) child.stderr.on("data", function (chunk) { report.stderrData += chunk.toString("utf8"); });
    child.on("error", function (error) {
      report.error = String((error && error.code) || error);
      resolve(report);
    });
    child.on("exit", function (code) { report.events.push("exit:" + code); });
    child.on("close", function (code) {
      report.events.push("close:" + code);
      report.code = code;
      resolve(report);
    });
  });
}
"#;

fn fixture(body: &str) -> String {
    format!("{STDIO_HELPERS}\n{body}")
}

/// `stdio: ["ignore", <fd>, <fd>]` with fds from `fs.openSync(path, "w")` —
/// the detached-runner shape: the child's stdout/stderr land in the caller's
/// log files, and the JS side sees no streams for those slots.
#[tokio::test(flavor = "current_thread")]
async fn fd_slots_write_the_callers_log_files() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const outFile = DIR + "/runner.stdout.log";
    const errFile = DIR + "/runner.stderr.log";
    const fdOut = fs.openSync(outFile, "w");
    const fdErr = fs.openSync(errFile, "w");
    const out = await spawnReport({ stdio: ["ignore", fdOut, fdErr] });
    out.fdOutIsNumber = typeof fdOut === "number";
    fs.closeSync(fdOut);
    fs.closeSync(fdErr);
    out.stdoutLog = readText(outFile);
    out.stderrLog = readText(errFile);
    out.logFileExists = fs.existsSync(outFile);
    globalThis.__pirStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_stdio_fixture("fd-slots", &ts).await;

    assert_eq!(v["error"], Value::Null, "spawn error: {v}");
    assert_eq!(v["code"], 0, "child exit code: {v}");
    assert_eq!(
        v["fdOutIsNumber"], true,
        "openSync must return a descriptor: {v}"
    );
    assert_eq!(
        v["stdoutIsNull"], true,
        "a non-pipe slot has no JS stream: {v}"
    );
    assert_eq!(
        v["stderrIsNull"], true,
        "a non-pipe slot has no JS stream: {v}"
    );
    assert_eq!(
        v["stdinIsNull"], true,
        "`ignore` stdin has no JS stream: {v}"
    );
    assert_eq!(
        v["logFileExists"], true,
        "the caller's log file exists: {v}"
    );
    assert!(
        v["stdoutLog"].as_str().unwrap_or("").contains("OUT_TEXT"),
        "the child's stdout must land in the fd's log file: {v}"
    );
    assert!(
        v["stderrLog"].as_str().unwrap_or("").contains("ERR_TEXT"),
        "the child's stderr must land in the fd's log file: {v}"
    );
}

/// The same fd slot passed as an fd-backed stream object (`.fd`) — the shape
/// `fs.createWriteStream` exposes.
#[tokio::test(flavor = "current_thread")]
async fn fd_backed_stream_objects_are_accepted() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const outFile = DIR + "/stream.stdout.log";
    const errFile = DIR + "/stream.stderr.log";
    const fdOut = fs.openSync(outFile, "w");
    const fdErr = fs.openSync(errFile, "w");
    const out = await spawnReport({ stdio: ["ignore", { fd: fdOut }, { fd: fdErr }] });
    fs.closeSync(fdOut);
    fs.closeSync(fdErr);
    out.stdoutLog = readText(outFile);
    out.stderrLog = readText(errFile);
    globalThis.__pirStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_stdio_fixture("fd-streams", &ts).await;

    assert_eq!(v["error"], Value::Null, "spawn error: {v}");
    assert_eq!(v["code"], 0, "child exit code: {v}");
    assert!(
        v["stdoutLog"].as_str().unwrap_or("").contains("OUT_TEXT"),
        "an fd-carrying object must resolve to that descriptor: {v}"
    );
    assert!(
        v["stderrLog"].as_str().unwrap_or("").contains("ERR_TEXT"),
        "an fd-carrying object must resolve to that descriptor: {v}"
    );
}

/// `"ignore"` (the string form applies to all three descriptors): the child
/// runs to completion, nothing is captured, and no stream object exists.
#[tokio::test(flavor = "current_thread")]
async fn ignore_leaves_no_streams_and_still_runs_the_child() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = await spawnReport({ stdio: "ignore" });
    // The callback API must keep working while a slot is ignored.
    out.exec = await new Promise(function (resolve) {
      exec("echo EXEC_OK", { stdio: ["ignore", "pipe", "pipe"] }, function (error, stdout) {
        resolve({ error: error ? String(error.message) : null, stdout: String(stdout).trim() });
      });
    });
    globalThis.__pirStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_stdio_fixture("ignore", &ts).await;

    assert_eq!(v["error"], Value::Null, "spawn error: {v}");
    assert_eq!(v["code"], 0, "an ignored child still exits normally: {v}");
    assert_eq!(
        v["events"],
        json!(["exit:0", "close:0"]),
        "exit then close, with no pipe end events: {v}"
    );
    assert_eq!(v["stdoutIsNull"], true, "no stdout stream: {v}");
    assert_eq!(v["stderrIsNull"], true, "no stderr stream: {v}");
    assert_eq!(v["stdinIsNull"], true, "no stdin stream: {v}");
    assert_eq!(
        v["stdoutData"], "",
        "nothing is captured on an ignored slot: {v}"
    );
    assert_eq!(
        v["exec"]["error"],
        Value::Null,
        "exec with ignored stdin still captures stdout: {v}"
    );
    assert_eq!(v["exec"]["stdout"], "EXEC_OK", "exec output: {v}");
}

/// `"inherit"`: the descriptors stay the parent's, so the JS side gets no
/// stream objects, and the child still runs (its output goes to this process's
/// own stdout/stderr, which a test harness cannot capture portably — the
/// shape and the exit are what the runtime promises).
#[tokio::test(flavor = "current_thread")]
async fn inherit_shares_the_parent_descriptors() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    globalThis.__pirStdio = await spawnReport({ stdio: "inherit" });
  });
};
"#,
    );
    let (v, _dir) = run_stdio_fixture("inherit", &ts).await;

    assert_eq!(v["error"], Value::Null, "spawn error: {v}");
    assert_eq!(
        v["code"], 0,
        "an inheriting child still exits normally: {v}"
    );
    assert_eq!(v["stdoutIsNull"], true, "no stdout stream: {v}");
    assert_eq!(v["stderrIsNull"], true, "no stderr stream: {v}");
    assert_eq!(v["stdinIsNull"], true, "no stdin stream: {v}");
}

/// The default (and an explicit `"pipe"`) keeps the historical all-pipe shape:
/// both capture streams exist and deliver the child's output.
#[tokio::test(flavor = "current_thread")]
async fn pipe_slots_still_capture_output() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    out.asDefault = await spawnReport({});
    out.asExplicit = await spawnReport({ stdio: ["pipe", "pipe", "pipe"] });
    globalThis.__pirStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_stdio_fixture("pipes", &ts).await;

    for probe in ["asDefault", "asExplicit"] {
        let seen = &v[probe];
        assert_eq!(seen["error"], Value::Null, "{probe} spawn error: {v}");
        assert_eq!(seen["code"], 0, "{probe} child exit code: {v}");
        assert_eq!(
            seen["stdoutIsNull"], false,
            "{probe}: stdout must be a pipe: {v}"
        );
        assert_eq!(
            seen["stderrIsNull"], false,
            "{probe}: stderr must be a pipe: {v}"
        );
        assert_eq!(
            seen["stdinIsNull"], false,
            "{probe}: stdin must be a pipe: {v}"
        );
        assert!(
            seen["stdoutData"]
                .as_str()
                .unwrap_or("")
                .contains("OUT_TEXT"),
            "{probe}: stdout must be captured: {v}"
        );
        assert!(
            seen["stderrData"]
                .as_str()
                .unwrap_or("")
                .contains("ERR_TEXT"),
            "{probe}: stderr must be captured: {v}"
        );
    }
}

/// Validation: a malformed `stdio` value is a synchronous `TypeError` (Node's
/// argument checking), while a descriptor that is not in the fs table is an
/// asynchronous `'error'` event carrying `EBADF` (the spawn itself fails).
#[tokio::test(flavor = "current_thread")]
async fn invalid_stdio_values_are_rejected() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    function caught(stdio) {
      try {
        spawn(SHELL, ["/C", "echo X"], { stdio: stdio });
        return null;
      } catch (error) {
        return { name: error.name, code: error.code };
      }
    }
    out.unknownString = caught(["wrong"]);
    out.wrongType = caught([{ notAnFd: true }]);
    out.negative = caught([-1]);
    out.fractional = caught([1.5]);
    out.nonArray = caught({});
    out.badFd = await new Promise(function (resolve) {
      const child = spawn(SHELL, ["/C", "echo X"], { stdio: ["ignore", 4242, "pipe"] });
      child.on("error", function (error) { resolve(String(error.code)); });
      child.on("close", function () { resolve("CLOSED"); });
    });
    globalThis.__pirStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_stdio_fixture("invalid", &ts).await;

    assert_eq!(
        v["unknownString"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_VALUE" }),
        "an unknown stdio string must throw synchronously: {v}"
    );
    assert_eq!(
        v["wrongType"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_VALUE" }),
        "an object without a usable fd must throw synchronously: {v}"
    );
    assert_eq!(
        v["negative"],
        json!({ "name": "TypeError", "code": "ERR_OUT_OF_RANGE" }),
        "a negative descriptor must throw synchronously: {v}"
    );
    assert_eq!(
        v["fractional"],
        json!({ "name": "TypeError", "code": "ERR_OUT_OF_RANGE" }),
        "a fractional descriptor must throw synchronously: {v}"
    );
    assert_eq!(
        v["nonArray"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_TYPE" }),
        "a non-array stdio must throw synchronously: {v}"
    );
    assert_eq!(
        v["badFd"], "EBADF",
        "a descriptor outside the fs table must fail the spawn with EBADF: {v}"
    );
}

// ---------------------------------------------------------------------------
// Stream-window backpressure: a child that outruns a consumer whose JS pump is
// stalled blocks on the pipe instead of having bytes dropped (Node's paused
// stream), and every byte still arrives once the consumer resumes.
// ---------------------------------------------------------------------------

/// Bytes the child writes; just past the 8 MiB stream window, so the reader is
/// guaranteed to park while the JS thread is blocked.
const BACKPRESSURE_FILE_BYTES: usize = 9 * 1024 * 1024;

#[tokio::test(flavor = "current_thread")]
async fn spawn_stream_window_backpressures_without_losing_bytes() {
    let dir = write_temp_fixture("backpressure", &[]);
    let ts = r#"
import { spawn } from "node:child_process";

const isWin = process.platform === "win32";
const SHELL = isWin ? "powershell.exe" : "sh";
// A child that writes 9 MiB to stdout and exits: `[Console]::Out.Write` is
// one raw write (no line formatting), `yes | head -c` the portable shape.
const BYTES = 9 * 1024 * 1024;
const CMD = isWin
  ? ["-NoProfile", "-Command", "[Console]::Out.Write('x' * " + BYTES + ")"]
  : ["-c", "yes | head -c " + BYTES];

export default (pi: any) => {
  pi.on("session_start", async () => {
    const report: any = { bytes: 0, chunks: 0, truncated: false, code: null };
    const child = spawn(SHELL, CMD, { windowsHide: true });
    child.on("error", (error: any) => { report.error = String(error && error.code || error); });
    child.stdout.on("data", (chunk: any) => {
      report.bytes += chunk.length;
      report.chunks += 1;
    });
    const closed = new Promise((resolve) => {
      child.on("close", (code: any) => { report.code = code; resolve(null); });
    });

    // Block the JS thread: the pump cannot poll, so the host's stream window
    // fills and the child must block on the pipe after ~8 MiB. Nothing is
    // dropped: the remaining bytes stay in the pipe/kernel.
    const end = Date.now() + 1500;
    while (Date.now() < end) { /* block the pump */ }

    // Probe the host through the raw bridge (the shim's pump is stalled):
    // `exited` is the host's own view of the child, so a child that had
    // finished its 9 MiB would report true here.
    const probe = JSON.parse(__pir_host.nodeExt("child", "spawnPoll", JSON.stringify({ id: child._id, waitMs: 0 })));
    report.exitedWhileStalled = Boolean(probe.value.exited);
    report.truncatedWhileStalled = Boolean(probe.value.truncated);
    report.probeBytes = probe.value.stdout ? Buffer.from(probe.value.stdout, "base64").length : 0;

    await closed;
    report.truncated = child.truncated === true;
    (globalThis as any).__pirStdio = report;
  });
};
"#;
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    let report = run_fixture(&rt, "backpressure", ts, "__pirStdio").await;

    let probe_bytes = report["probeBytes"].as_u64().expect("probeBytes");
    let streamed = report["bytes"].as_u64().expect("bytes");
    assert_eq!(
        probe_bytes + streamed,
        BACKPRESSURE_FILE_BYTES as u64,
        "every byte of the 9 MiB child must arrive (probe drain + data events): {report}"
    );
    assert!(
        probe_bytes <= 8 * 1024 * 1024,
        "the probe drains at most the 8 MiB window: {report}"
    );
    assert_eq!(
        report["truncated"], false,
        "backpressure must never truncate: {report}"
    );
    assert_eq!(
        report["truncatedWhileStalled"], false,
        "no byte is dropped while the reader is parked: {report}"
    );
    assert_eq!(report["code"].as_i64(), Some(0), "child exits 0: {report}");
    // The child was still blocked while the JS thread (and so its pump) was
    // stalled: a drained pipe would have let it finish 9 MiB in a few ms.
    assert_eq!(
        report["exitedWhileStalled"], false,
        "the child must be blocked on the full pipe while the pump is stalled: {report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
