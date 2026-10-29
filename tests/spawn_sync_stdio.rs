//! `child_process.spawnSync`'s `stdio` option: the blocking form must resolve
//! the same `[stdin, stdout, stderr]` table `spawn` does (`"pipe"` /
//! `"ignore"` / `"inherit"` / a parent fd number from `fs.openSync`), instead
//! of always piping all three descriptors. A slot this call does not pipe
//! reports `null` in `result.stdout` / `result.stderr` / `result.output` —
//! Node's blocking-form shape.
//!
//! Reference behavior was measured against a real node (v24) with `node -e`;
//! every expectation here is one of those measurements:
//!
//! | call | node's `result` |
//! |---|---|
//! | `spawnSync(sh, ["/C", cmd])` (no `stdio`) | `stdout`/`stderr` Buffers, `output[0]` null |
//! | `{stdio: ["ignore", fd, fd]}` | `stdout`/`stderr` null, child writes into the fd |
//! | `{stdio: "ignore"}` / `{stdio: "inherit"}` | `stdout`/`stderr` null |
//! | `{stdio: ["pipe","pipe","pipe"]}` | both Buffers, `output` = `[null, out, err]` |
//! | `{input, stdio: ["ignore", …]}` | `input` dropped silently, no error |
//! | `{input}` with a piped stdin | the child reads `input` |
//! | piped stdin without `input` | the write end is closed: the child sees EOF |
//!
//! Fixtures are generated at runtime into per-test temp dirs (repository
//! hygiene gate; same pattern as `tests/node_shims.rs`), and the child program
//! is the platform shell (`cmd.exe` / `sh`), so no rebuilt `pir` binary is
//! needed to drive them.

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// A fixture workspace removed on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!(
        "pir-spawn-sync-stdio-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    TempDir(dir)
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
async fn run_sync_fixture(name: &str, body: &str) -> (Value, TempDir) {
    let dir = write_temp_fixture(name);
    let ts = body.replace("__DIR__", &dir.0.to_string_lossy().replace('\\', "/"));
    std::fs::write(dir.0.join("index.ts"), &ts).expect("write fixture");
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.0.clone());
    let value = run_fixture(&rt, name, &ts, "__pirSyncStdio").await;
    (value, dir)
}

/// The probes every fixture shares: a `spawnSync` reporter reduced to
/// JSON-friendly fields, a stdin-reading child, and the platform shell lines.
/// The reader (`sort` on Windows, a `read` loop in `sh`) prints what it read
/// and nothing at EOF, so "the child received bytes" and "the child saw EOF"
/// are distinguishable.
const SYNC_STDIO_HELPERS: &str = r#"
import { spawnSync } from "node:child_process";
import fs from "node:fs";

const DIR = "__DIR__";
const isWin = process.platform === "win32";
const SHELL = isWin ? "cmd.exe" : "sh";
// One command that writes a marker to stdout and another to stderr.
const BOTH = isWin
  ? ["/C", "echo OUT_TEXT & echo ERR_TEXT 1>&2"]
  : ["-c", "echo OUT_TEXT; echo ERR_TEXT 1>&2"];
const STDIN_READER = isWin
  ? ["/C", "sort"]
  : ["-c", "while read -r l; do printf \"GOT:%s\\n\" \"$l\"; done"];
// What the reader prints for the single line `HELLO` (see the probes below).
const STDIN_MARK = isWin ? "HELLO" : "GOT:HELLO";

function readText(file) {
  try {
    return fs.readFileSync(file, "utf8");
  } catch (error) {
    return "READ_ERR:" + error.code;
  }
}

function report(result) {
  return {
    status: result.status,
    signal: result.signal,
    error: result.error === undefined || result.error === null
      ? null
      : String(result.error.code || result.error.message),
    stdoutIsNull: result.stdout === null,
    stderrIsNull: result.stderr === null,
    // Strings, so a Buffer never has to survive the JSON round trip.
    stdoutText: result.stdout === null ? null : String(result.stdout).trim(),
    stderrText: result.stderr === null ? null : String(result.stderr).trim(),
    outputIsNull: result.output.map(function (value) { return value === null; }),
    pidIsNumber: typeof result.pid === "number" && result.pid > 0,
  };
}

// `spawnSync` on the OUT/ERR marker command.
function syncReport(options) {
  return report(spawnSync(SHELL, BOTH, Object.assign({ windowsHide: true }, options)));
}

// `spawnSync` on a child that echoes its stdin; `stdoutText` is the marker
// when the input arrived and "" when the child saw EOF.
function stdinReport(options) {
  return report(spawnSync(SHELL, STDIN_READER, Object.assign({ windowsHide: true }, options)));
}
"#;

fn fixture(body: &str) -> String {
    format!("{SYNC_STDIO_HELPERS}\n{body}")
}

/// `stdio: ["ignore", <fd>, <fd>]` with fds from `fs.openSync(path, "w")` —
/// the detached-runner shape. The child's output lands in the caller's log
/// files, the parent captures nothing, and the result slots are null. The
/// mixed probe (`["ignore", <fd>, "pipe"]`) pins that the slots resolve
/// independently: only the fd one is muted.
#[tokio::test(flavor = "current_thread")]
async fn fd_slots_write_the_callers_log_files() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    const outFile = DIR + "/sync.stdout.log";
    const errFile = DIR + "/sync.stderr.log";
    const fdOut = fs.openSync(outFile, "w");
    const fdErr = fs.openSync(errFile, "w");
    out.fd = syncReport({ stdio: ["ignore", fdOut, fdErr] });
    out.fdIsNumber = typeof fdOut === "number" && typeof fdErr === "number";
    fs.closeSync(fdOut);
    fs.closeSync(fdErr);
    out.stdoutLog = readText(outFile);
    out.stderrLog = readText(errFile);

    const mixedFile = DIR + "/mixed.stdout.log";
    const fdMixed = fs.openSync(mixedFile, "w");
    out.mixed = syncReport({ stdio: ["ignore", fdMixed, "pipe"] });
    fs.closeSync(fdMixed);
    out.mixedLog = readText(mixedFile);

    globalThis.__pirSyncStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_sync_fixture("fd-slots", &ts).await;

    assert_eq!(v["fd"]["error"], Value::Null, "spawn error: {v}");
    assert_eq!(v["fdIsNumber"], true, "openSync must return fds: {v}");
    assert_eq!(v["fd"]["status"], 0, "child exit code: {v}");
    assert_eq!(
        v["fd"]["stdoutIsNull"], true,
        "an fd slot has no result bytes: {v}"
    );
    assert_eq!(
        v["fd"]["stderrIsNull"], true,
        "an fd slot has no result bytes: {v}"
    );
    assert_eq!(
        v["fd"]["outputIsNull"],
        json!([true, true, true]),
        "`output` mirrors the resolved slots: {v}"
    );
    assert_eq!(
        v["fd"]["pidIsNumber"], true,
        "the child pid is reported: {v}"
    );
    assert!(
        v["stdoutLog"].as_str().unwrap_or("").contains("OUT_TEXT"),
        "the child's stdout must land in the fd's log file: {v}"
    );
    assert!(
        v["stderrLog"].as_str().unwrap_or("").contains("ERR_TEXT"),
        "the child's stderr must land in the fd's log file: {v}"
    );

    assert_eq!(v["mixed"]["status"], 0, "mixed child exit code: {v}");
    assert_eq!(
        v["mixed"]["stdoutIsNull"], true,
        "the fd slot is not piped: {v}"
    );
    assert_eq!(
        v["mixed"]["stderrIsNull"], false,
        "the `pipe` slot is captured: {v}"
    );
    assert!(
        v["mixed"]["stderrText"]
            .as_str()
            .unwrap_or("")
            .contains("ERR_TEXT"),
        "the piped slot carries the child's stderr: {v}"
    );
    assert_eq!(
        v["mixed"]["outputIsNull"],
        json!([true, true, false]),
        "`output` mirrors the mixed slots: {v}"
    );
    assert!(
        v["mixedLog"].as_str().unwrap_or("").contains("OUT_TEXT"),
        "the mixed child's stdout must land in its log file: {v}"
    );
}

/// The `"inherit"` and `"ignore"` string forms: nothing is captured, so both
/// result slots are null (`"ignore"` also drains stdin, which is why the
/// reader probe is safe there). The explicit `["pipe","pipe","pipe"]` form is
/// the contrast: real bytes, and `output` = `[null, out, err]`. `encoding`
/// does not turn a non-piped slot into a string, and a malformed `stdio` is a
/// synchronous `TypeError` (Node validates before it spawns anything).
#[tokio::test(flavor = "current_thread")]
async fn inherit_and_ignore_report_null_slots() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    out.inherit = syncReport({ stdio: "inherit" });
    out.ignore = syncReport({ stdio: "ignore" });
    out.pipes = syncReport({ stdio: ["pipe", "pipe", "pipe"] });
    out.utf8Ignored = syncReport({ stdio: ["ignore", "ignore", "ignore"], encoding: "utf8" });
    // The throw happens before the call reaches the host.
    function caught(stdio) {
      try {
        spawnSync(SHELL, BOTH, { stdio: stdio, windowsHide: true });
        return null;
      } catch (error) {
        return { name: error.name, code: error.code };
      }
    }
    out.badString = caught(["wrong"]);
    out.badFractional = caught(["pipe", 1.5, "pipe"]);
    globalThis.__pirSyncStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_sync_fixture("inherit-ignore", &ts).await;

    for probe in ["inherit", "ignore", "utf8Ignored"] {
        let seen = &v[probe];
        assert_eq!(seen["error"], Value::Null, "{probe} spawn error: {v}");
        assert_eq!(seen["status"], 0, "{probe} child exit code: {v}");
        assert_eq!(
            seen["stdoutIsNull"], true,
            "{probe}: stdout is not piped: {v}"
        );
        assert_eq!(
            seen["stderrIsNull"], true,
            "{probe}: stderr is not piped: {v}"
        );
        assert_eq!(
            seen["outputIsNull"],
            json!([true, true, true]),
            "{probe}: `output` mirrors the muted slots: {v}"
        );
    }

    assert_eq!(v["pipes"]["error"], Value::Null, "pipes spawn error: {v}");
    assert_eq!(v["pipes"]["status"], 0, "pipes child exit code: {v}");
    assert_eq!(
        v["pipes"]["stdoutIsNull"], false,
        "pipes: stdout captured: {v}"
    );
    assert_eq!(
        v["pipes"]["stderrIsNull"], false,
        "pipes: stderr captured: {v}"
    );
    assert_eq!(
        v["pipes"]["outputIsNull"],
        json!([true, false, false]),
        "pipes: `output` = [null, out, err]: {v}"
    );
    assert!(
        v["pipes"]["stdoutText"]
            .as_str()
            .unwrap_or("")
            .contains("OUT_TEXT"),
        "pipes: stdout bytes: {v}"
    );
    assert!(
        v["pipes"]["stderrText"]
            .as_str()
            .unwrap_or("")
            .contains("ERR_TEXT"),
        "pipes: stderr bytes: {v}"
    );

    // Measured against node: both shapes throw synchronously (ERR_INVALID_ARG_VALUE
    // for an unknown string, ERR_OUT_OF_RANGE for a fractional descriptor).
    assert_eq!(
        v["badString"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_VALUE" }),
        "an unknown stdio string must throw synchronously: {v}"
    );
    assert_eq!(
        v["badFractional"],
        json!({ "name": "TypeError", "code": "ERR_OUT_OF_RANGE" }),
        "a fractional descriptor must throw synchronously: {v}"
    );
}

/// The absent `stdio` option keeps the historical three-pipe shape (Node's
/// documented default), and its piped stdin closes its write end immediately:
/// a child that reads stdin gets EOF and the call returns instead of hanging.
/// The explicit `"pipe"`/`"ignore"` stdin slots behave the same way here.
#[tokio::test(flavor = "current_thread")]
async fn default_stdio_is_three_pipes_with_eof_stdin() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    out.environment = syncReport({});
    out.pipes = syncReport({ stdio: ["pipe", "pipe", "pipe"] });
    out.defaultStdin = stdinReport({});
    out.pipeStdin = stdinReport({ stdio: ["pipe", "pipe", "pipe"] });
    out.ignoredStdin = stdinReport({ stdio: ["ignore", "pipe", "pipe"] });
    out.mark = STDIN_MARK;
    globalThis.__pirSyncStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_sync_fixture("default", &ts).await;

    for probe in ["environment", "pipes"] {
        let seen = &v[probe];
        assert_eq!(seen["error"], Value::Null, "{probe} spawn error: {v}");
        assert_eq!(seen["status"], 0, "{probe} child exit code: {v}");
        assert_eq!(
            seen["stdoutIsNull"], false,
            "{probe}: stdout is a pipe: {v}"
        );
        assert_eq!(
            seen["stderrIsNull"], false,
            "{probe}: stderr is a pipe: {v}"
        );
        assert_eq!(
            seen["outputIsNull"],
            json!([true, false, false]),
            "{probe}: `output` = [null, out, err]: {v}"
        );
        assert!(
            seen["stdoutText"]
                .as_str()
                .unwrap_or("")
                .contains("OUT_TEXT"),
            "{probe}: stdout bytes: {v}"
        );
    }
    assert_eq!(
        v["environment"]["stdoutText"], v["pipes"]["stdoutText"],
        "the default shape is the explicit all-pipe shape: {v}"
    );

    // `""` means the reader reached EOF without a line (see the helper).
    for probe in ["defaultStdin", "pipeStdin", "ignoredStdin"] {
        let seen = &v[probe];
        assert_eq!(seen["error"], Value::Null, "{probe} spawn error: {v}");
        assert_eq!(
            seen["stdoutText"], "",
            "{probe}: a stdin nobody wrote to reaches EOF at once: {v}"
        );
        assert_eq!(
            seen["stdoutIsNull"], false,
            "{probe}: stdout is still captured: {v}"
        );
    }
}

/// `input` only feeds a piped stdin (Node's blocking form): the bytes arrive
/// on the default and explicit `"pipe"` slots, while an `"ignore"` stdin drops
/// them silently — no error, the child just reads EOF. A piped stdin with no
/// `input` stays empty.
#[tokio::test(flavor = "current_thread")]
async fn input_only_feeds_a_piped_stdin() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    out.mark = STDIN_MARK;
    out.defaultWithInput = stdinReport({ input: "HELLO\n" });
    out.pipeWithInput = stdinReport({ input: "HELLO\n", stdio: ["pipe", "pipe", "pipe"] });
    out.pipeWithoutInput = stdinReport({ stdio: ["pipe", "pipe", "pipe"] });
    out.ignoredInput = stdinReport({ input: "HELLO\n", stdio: ["ignore", "pipe", "pipe"] });
    globalThis.__pirSyncStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_sync_fixture("input", &ts).await;

    let mark = v["mark"].as_str().expect("marker").to_string();
    assert!(!mark.is_empty(), "the platform marker is defined: {v}");

    for probe in ["defaultWithInput", "pipeWithInput"] {
        let seen = &v[probe];
        assert_eq!(seen["error"], Value::Null, "{probe} spawn error: {v}");
        assert_eq!(
            seen["stdoutText"], mark,
            "{probe}: the child must read the input: {v}"
        );
    }
    assert_eq!(
        v["pipeWithoutInput"]["stdoutText"], "",
        "no input: the piped stdin reaches EOF untouched: {v}"
    );
    assert_eq!(
        v["ignoredInput"]["stdoutText"], "",
        "an ignored stdin drops `input` silently: {v}"
    );
    assert_eq!(
        v["ignoredInput"]["error"],
        Value::Null,
        "a dropped `input` is not an error: {v}"
    );
}

/// The fd slot keeps the caller's offset: two calls through one descriptor
/// opened with `"a"` append both runs' output in order, which is what a
/// detached runner's log file relies on.
#[tokio::test(flavor = "current_thread")]
async fn fd_slots_append_through_the_shared_offset() {
    let ts = fixture(
        r#"
export default (pi) => {
  pi.on("session_start", async () => {
    const out = {};
    const logFile = DIR + "/append.log";
    const fd = fs.openSync(logFile, "a");
    out.first = syncReport({ stdio: ["ignore", fd, fd] });
    out.second = syncReport({ stdio: ["ignore", fd, fd] });
    fs.closeSync(fd);
    out.log = readText(logFile);
    out.outCount = out.log.split("OUT_TEXT").length - 1;
    out.errCount = out.log.split("ERR_TEXT").length - 1;
    out.outBeforeErr = out.log.indexOf("OUT_TEXT") < out.log.indexOf("ERR_TEXT");
    globalThis.__pirSyncStdio = out;
  });
};
"#,
    );
    let (v, _dir) = run_sync_fixture("append", &ts).await;

    assert_eq!(v["first"]["status"], 0, "first run exit code: {v}");
    assert_eq!(v["second"]["status"], 0, "second run exit code: {v}");
    assert_eq!(
        v["first"]["stdoutIsNull"], true,
        "first run captured nothing: {v}"
    );
    assert_eq!(
        v["second"]["stdoutIsNull"], true,
        "second run captured nothing: {v}"
    );
    assert_eq!(v["outCount"], 2, "both runs wrote their stdout: {v}");
    assert_eq!(v["errCount"], 2, "both runs wrote their stderr: {v}");
    assert_eq!(v["outBeforeErr"], true, "the order is preserved: {v}");
}
