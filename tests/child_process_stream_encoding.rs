//! `child_process`'s capture streams are real `Readable`s from the stream pad:
//! `setEncoding`/`readableEncoding`, `pause`/`resume` and `destroy` work, and
//! chunks arrive as strings once an encoding is set.
//!
//! This is the gap that kept the durable `NodeExecutionEnv` shell probe
//! (`findBashOnPath` → `runCommand` → `child.stdout?.setEncoding("utf8")`)
//! unusable on the QuickJS runtime: the streams used to be a hand-rolled
//! `EventEmitter` with a `_deliver` that emitted Buffers and had no
//! `setEncoding` at all. The child's `exit` → stdio `end` → `close` ordering is
//! locked by `tests/node_shims.rs` and asserted here again over the same path.
//!
//! Fixtures are generated at runtime into per-test temp dirs (repository
//! hygiene gate; same pattern as `tests/child_process_stdio.rs`).

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/child_process_stdio.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-child-encoding-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
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

async fn run_fixture(name: &str, body: &str) -> Value {
    let dir = write_temp_fixture(name);
    let ts = body.replace("__DIR__", &dir.to_string_lossy().replace('\\', "/"));
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    let module = format!("pir-ext:child-encoding-{name}.ts");
    rt.seed_module(&module, &ts).expect("seed fixture");
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
    rt.eval_json(&format!("JSON.stringify(globalThis.{name})"))
        .await
        .expect("read fixture results")
}

/// The probe every case shares: the platform shell plus `code(run)`.
const HELPERS: &str = r#"
import { spawn } from "node:child_process";

const DIR = "__DIR__";
const isWin = process.platform === "win32";
const SHELL = isWin ? "cmd.exe" : "sh";
const ECHO = (text) => (isWin ? ["/C", "echo " + text] : ["-c", "echo " + text]);
// `echo … 1>&2`: the shell's own stderr redirection works on both platforms.
const ECHO_ERR = (text) => (isWin ? ["/C", "echo " + text + " 1>&2"] : ["-c", "echo " + text + " 1>&2"]);

function code(run) {
  try {
    return { ok: true, value: String(run()) };
  } catch (error) {
    return { ok: false, name: error.name, code: error.code, message: error.message };
  }
}
"#;

/// `setEncoding("utf8")` turns every chunk into a string, reports
/// `readableEncoding` (node's normalized label), returns the stream, and the
/// pad keeps the child's `exit` → `end` → `close` order.
#[tokio::test(flavor = "current_thread")]
async fn child_stdout_set_encoding_yields_string_chunks() {
    let ts = format!(
        r#"{HELPERS}
export default (pi) => {{
  pi.on("session_start", async () => {{
    const out = {{}};
    out.encoding = await new Promise((resolve) => {{
      const child = spawn(SHELL, ECHO("OUT_TEXT"), {{ windowsHide: true }});
      const report = {{
        readableEncodingBefore: child.stdout.readableEncoding,
        chunkKinds: [],
        data: "",
        flowingInData: undefined,
        events: [],
      }};
      const returned = child.stdout.setEncoding("utf8");
      report.setEncodingReturnsStream = returned === child.stdout;
      report.readableEncodingAfter = child.stdout.readableEncoding;
      child.stdout.on("data", (chunk) => {{
        report.chunkKinds.push(typeof chunk);
        report.flowingInData = child.stdout.readableFlowing;
        report.data += chunk;
      }});
      child.stdout.on("end", () => report.events.push("stdout-end"));
      child.on("exit", (exitCode) => report.events.push("exit:" + exitCode));
      child.on("error", (error) => {{ report.error = String(error.code); resolve(report); }});
      child.on("close", (closeCode) => {{
        report.events.push("close:" + closeCode);
        resolve(report);
      }});
    }});

    // Other node encoding labels normalize the same way.
    out.labels = await new Promise((resolve) => {{
      const child = spawn(SHELL, ECHO("X"), {{ windowsHide: true }});
      const labels = {{}};
      labels.utf8 = child.stdout.setEncoding("utf-8").readableEncoding;
      labels.utf16 = child.stdout.setEncoding("ucs2").readableEncoding;
      labels.upper = child.stdout.setEncoding("UTF8").readableEncoding;
      labels.invalid = code(() => child.stdout.setEncoding("bogus")).code;
      labels.nullish = child.stdout.setEncoding(null).readableEncoding;
      child.stdout.resume();
      child.on("error", (error) => {{ labels.error = String(error.code); resolve(labels); }});
      child.on("close", () => resolve(labels));
    }});

    // stderr carries the same surface.
    out.stderr = await new Promise((resolve) => {{
      const child = spawn(SHELL, ECHO_ERR("ERR_TEXT"), {{ windowsHide: true }});
      const report = {{ data: "", kinds: undefined, encoding: undefined }};
      child.stderr.setEncoding("utf8");
      report.encoding = child.stderr.readableEncoding;
      child.stderr.on("data", (chunk) => {{ report.kinds = typeof chunk; report.data += chunk; }});
      child.stdout.resume();
      child.on("error", (error) => {{ report.error = String(error.code); resolve(report); }});
      child.on("close", () => resolve(report));
    }});

    // Buffers without an encoding (the pre-existing contract).
    out.buffered = await new Promise((resolve) => {{
      const child = spawn(SHELL, ECHO("RAW"), {{ windowsHide: true }});
      const report = {{ isBuffer: undefined, encoding: undefined }};
      report.encoding = child.stdout.readableEncoding;
      child.stdout.on("data", (chunk) => {{ report.isBuffer = Buffer.isBuffer(chunk); }});
      child.on("error", (error) => {{ report.error = String(error.code); resolve(report); }});
      child.on("close", () => resolve(report));
    }});

    globalThis.__childEncoding = out;
  }});
}};
"#
    );
    let v = run_fixture("__childEncoding", &ts).await;

    let encoding = &v["encoding"];
    assert_eq!(
        encoding["readableEncodingBefore"],
        Value::Null,
        "without setEncoding the reporting getter is null, like node: {v}"
    );
    assert_eq!(
        encoding["readableEncodingAfter"], "utf8",
        "setEncoding('utf8') reports utf8: {v}"
    );
    assert_eq!(
        encoding["setEncodingReturnsStream"], true,
        "setEncoding returns the stream (chainable): {v}"
    );
    assert_eq!(
        encoding["chunkKinds"],
        json!(["string"]),
        "every chunk is a string once an encoding is set: {v}"
    );
    assert_eq!(
        encoding["data"].as_str().unwrap_or("").trim(),
        "OUT_TEXT",
        "the decoded text survives: {v}"
    );
    assert_eq!(
        encoding["flowingInData"], true,
        "a 'data' listener puts the child stream in flowing mode: {v}"
    );
    assert_eq!(
        encoding["events"],
        json!(["exit:0", "stdout-end", "close:0"]),
        "exit, stdio end and close keep node's order: {v}"
    );

    let labels = &v["labels"];
    assert_eq!(labels["utf8"], "utf8", "utf-8 normalizes to utf8: {v}");
    assert_eq!(
        labels["utf16"], "utf16le",
        "ucs2 normalizes to utf16le: {v}"
    );
    assert_eq!(labels["upper"], "utf8", "UTF8 normalizes to utf8: {v}");
    assert_eq!(
        labels["invalid"], "ERR_UNKNOWN_ENCODING",
        "an unknown encoding is refused: {v}"
    );
    assert_eq!(
        labels["nullish"], "utf8",
        "setEncoding(null) means utf8: {v}"
    );

    let stderr = &v["stderr"];
    assert_eq!(
        stderr["encoding"], "utf8",
        "stderr takes an encoding too: {v}"
    );
    assert_eq!(stderr["kinds"], "string", "stderr chunks are strings: {v}");

    let buffered = &v["buffered"];
    assert_eq!(
        buffered["encoding"],
        Value::Null,
        "no encoding means no decoder: {v}"
    );
    assert_eq!(
        buffered["isBuffer"], true,
        "chunks stay Buffers without setEncoding: {v}"
    );
}

/// `pause()` holds the chunks back and `resume()` releases them; `destroy()`
/// tears the pipe down without breaking the child's own lifecycle.
#[tokio::test(flavor = "current_thread")]
async fn child_streams_pause_resume_and_destroy() {
    let ts = format!(
        r#"{HELPERS}
export default (pi) => {{
  pi.on("session_start", async () => {{
    const out = {{}};

    out.pausing = await new Promise((resolve) => {{
      const child = spawn(SHELL, ECHO("HELD"), {{ windowsHide: true }});
      const report = {{ seenWhilePaused: 0, data: "", beforeResume: undefined }};
      // Pause before any listener exists (node's resume-only-on-new-data
      // contract keeps it paused), then attach the consumer.
      child.stdout.pause();
      report.pausedImmediately = child.stdout.isPaused();
      child.stdout.on("data", (chunk) => {{
        report.seenWhilePaused += 1;
        report.data += chunk.toString("utf8");
      }});
      report.pausedWithListener = child.stdout.isPaused();
      child.on("error", (error) => {{ report.error = String(error.code); resolve(report); }});
      child.on("close", (closeCode) => {{
        // The child is gone and everything the host captured sits in the
        // paused pipe's buffer: nothing was delivered yet.
        report.closeCode = closeCode;
        report.beforeResume = report.data;
        child.stdout.on("end", () => {{
          report.afterResume = report.data;
          resolve(report);
        }});
        child.stdout.resume();
        report.pausedAfterResume = child.stdout.isPaused();
      }});
    }});

    out.destroying = await new Promise((resolve) => {{
      const child = spawn(SHELL, ECHO("GONE"), {{ windowsHide: true }});
      const report = {{ destroyed: false, pipeClosed: false, childClosed: false }};
      child.stdout.on("close", () => {{ report.pipeClosed = true; }});
      child.stdout.destroy();
      report.destroyed = child.stdout.destroyed;
      child.on("close", (closeCode) => {{
        report.childClosed = true;
        report.closeCode = closeCode;
        resolve(report);
      }});
    }});

    globalThis.__childLifecycle = out;
  }});
}};
"#
    );
    let v = run_fixture("__childLifecycle", &ts).await;

    let pausing = &v["pausing"];
    assert_eq!(
        pausing["pausedImmediately"], true,
        "pause() reports paused: {v}"
    );
    assert_eq!(
        pausing["pausedWithListener"], true,
        "attaching a 'data' listener to an explicitly paused stream keeps it paused: {v}"
    );
    assert_eq!(
        pausing["beforeResume"], "",
        "a paused pipe buffers the captured output instead of dropping it: {v}"
    );
    assert_eq!(
        pausing["pausedAfterResume"], false,
        "resume() clears the paused state: {v}"
    );
    assert_eq!(
        pausing["afterResume"].as_str().unwrap_or("").trim(),
        "HELD",
        "resume() releases everything the host captured: {v}"
    );

    let destroying = &v["destroying"];
    assert_eq!(
        destroying["destroyed"], true,
        "destroy() marks the pipe: {v}"
    );
    assert_eq!(
        destroying["pipeClosed"], true,
        "the pipe emits 'close': {v}"
    );
    assert_eq!(
        destroying["childClosed"], true,
        "the child still reaches its own 'close' after a pipe destroy: {v}"
    );
    assert_eq!(
        destroying["closeCode"], 0,
        "the child's exit code is unaffected: {v}"
    );
}
