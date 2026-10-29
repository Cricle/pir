//! Integration tests for real in-process `worker_threads` (one OS thread +
//! one QuickJS runtime per Worker, host-backed — `worker_fns.rs`).
//!
//! Covers the consumer surface pi-subagents' `scripted-workflow.js` needs and
//! the general minimal face: workerData injection, bidirectional messages,
//! `terminate`, node modules inside the worker (fs), file-mode workers,
//! `process.exit` ending only the worker, error propagation, cross-worker
//! environment data, the worker-side module state (isMainThread/threadId/
//! parentPort), the clone bridge contract, and a scripted-workflow-isomorphic
//! sequence (eval worker + workerData require + vm sandbox + request/response
//! message protocol).

use std::path::{Path, PathBuf};
use std::time::Duration;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// Fixture directory per test (the hygiene gate forbids tracked JS sources).
fn fixture_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-worker-fixtures-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "script".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// The script-mode module runner (`pir js` boot defines the same shape; see
/// `main_cli/js_mode.rs` + `script_boot.js.txt`).
const SCRIPT_RUN_BOOT: &str = r#"
globalThis.__pirScriptRun = async (moduleName) => {
  try {
    await import(moduleName);
    return JSON.stringify({ ok: true });
  } catch (error) {
    const message = error && error.message !== undefined ? String(error.message) : String(error);
    const rawStack = error && error.stack ? String(error.stack) : "";
    const stack = rawStack.indexOf(message) >= 0 ? rawStack : message + (rawStack ? "\n" + rawStack : "");
    return JSON.stringify({ ok: false, message: message, stack: stack });
  }
};
"#;

/// Boot a script-mode runtime, run the test entry module (top-level await
/// included), drain the loop, and read back `globalThis.__testResult`.
async fn run_worker_test(test_name: &str, source: &str) -> Value {
    let cwd = fixture_dir(test_name);
    let rt = ExtensionRuntime::new(test_config_at(&cwd))
        .await
        .expect("runtime construction");
    rt.eval_boot(SCRIPT_RUN_BOOT).await.expect("script boot");
    rt.seed_module("pir-ext:worker_test.js", source)
        .expect("seed module");
    let run = tokio::time::timeout(
        Duration::from_secs(120),
        rt.run_script("pir-ext:worker_test.js"),
    )
    .await
    .expect("script run timed out");
    match run {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => panic!("test module failed: {stack}"),
        Err(error) => panic!("script run failed: {error}"),
    }
    tokio::time::timeout(Duration::from_secs(60), rt.wait_idle())
        .await
        .expect("event loop drain timed out (worker left running?)");
    rt.eval_json("JSON.stringify(globalThis.__testResult ?? null)")
        .await
        .expect("read result")
}

// ---------------------------------------------------------------------------
// workerData injection + bidirectional messages
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_data_and_bidirectional_messages() {
    let result = run_worker_test(
        "worker_data",
        r#"
        import { Worker } from "node:worker_threads";
        const worker = new Worker(`
          const { parentPort, workerData } = require("node:worker_threads");
          parentPort.on("message", (message) => {
            parentPort.postMessage({ echo: message.ping, answer: workerData.answer });
          });
        `, { eval: true, workerData: { answer: 42 } });
        const reply = await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
        worker.postMessage({ ping: "hello" });
        });
        await worker.terminate();
        globalThis.__testResult = reply;
        "#,
    )
    .await;
    assert_eq!(result, json!({ "echo": "hello", "answer": 42 }));
}

// ---------------------------------------------------------------------------
// Uncaught worker error -> parent `error` event, then exit 1
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_error_propagation() {
    let result = run_worker_test(
        "worker_error",
        r#"
        import { Worker } from "node:worker_threads";
        const worker = new Worker(`throw new Error("boom-worker");`, { eval: true });
        let exitCode = null;
        worker.on("exit", (code) => { exitCode = code; });
        const outcome = await new Promise((resolve) => {
          worker.on("error", (error) => resolve({ error: String(error.message) }));
        });
        await new Promise((resolve) => setTimeout(resolve, 100));
        globalThis.__testResult = { ...outcome, exitCode };
        "#,
    )
    .await;
    assert_eq!(result, json!({ "error": "boom-worker", "exitCode": 1 }));
}

// ---------------------------------------------------------------------------
// process.exit inside the worker ends only the worker
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_exit_only_exits_worker() {
    let result = run_worker_test(
        "worker_exit",
        r#"
        import { Worker } from "node:worker_threads";
        const worker = new Worker(`
          const { parentPort } = require("node:worker_threads");
          parentPort.postMessage({ ready: true });
          parentPort.on("message", () => { process.exit(7); });
        `, { eval: true });
        await new Promise((resolve) => {
          worker.on("message", resolve);
          worker.on("error", resolve);
        });
        worker.postMessage({ go: true });
        const exitCode = await new Promise((resolve) => {
          worker.on("exit", resolve);
        });
        // The parent runtime keeps running after the worker exited.
        await new Promise((resolve) => setTimeout(resolve, 30));
        globalThis.__testResult = { exit: exitCode, parentAlive: true };
        "#,
    )
    .await;
    assert_eq!(result, json!({ "exit": 7, "parentAlive": true }));
}

// ---------------------------------------------------------------------------
// node modules inside the worker (fs)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_node_modules_fs() {
    let result = run_worker_test(
        "worker_fs",
        r#"
        import { Worker } from "node:worker_threads";
        import { writeFileSync } from "node:fs";
        const file = process.cwd() + "/worker-input.txt";
        writeFileSync(file, "hello-from-file");
        const worker = new Worker(`
          const { parentPort, workerData } = require("node:worker_threads");
          const { readFileSync } = require("node:fs");
          parentPort.postMessage({ content: readFileSync(workerData.file, "utf8") });
        `, { eval: true, workerData: { file } });
        const message = await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
        });
        await worker.terminate();
        globalThis.__testResult = message;
        "#,
    )
    .await;
    assert_eq!(result, json!({ "content": "hello-from-file" }));
}

// ---------------------------------------------------------------------------
// File-mode worker (CJS entry file)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_file_mode() {
    let result = run_worker_test(
        "worker_file",
        r#"
        import { Worker } from "node:worker_threads";
        import { writeFileSync } from "node:fs";
        const entry = process.cwd() + "/worker-entry.cjs";
        writeFileSync(entry, `
          const { parentPort, workerData } = require("node:worker_threads");
          parentPort.postMessage({ from: "file-worker", data: workerData });
        `);
        const worker = new Worker(entry, { workerData: { id: 9 } });
        const message = await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
        });
        await worker.terminate();
        globalThis.__testResult = message;
        "#,
    )
    .await;
    assert_eq!(
        result,
        json!({ "from": "file-worker", "data": { "id": 9 } })
    );
}

// ---------------------------------------------------------------------------
// terminate() stops a waiting worker and resolves
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_terminate_stops_worker() {
    let result = run_worker_test(
        "worker_terminate",
        r#"
        import { Worker } from "node:worker_threads";
        const worker = new Worker(`setInterval(() => {}, 1000);`, { eval: true });
        await new Promise((resolve) => setTimeout(resolve, 50));
        const code = await worker.terminate();
        const exitCode = await new Promise((resolve) => {
          if (worker._exited) resolve(worker._exitCode);
          else worker.on("exit", resolve);
        });
        globalThis.__testResult = { terminateCode: code, exitCode };
        "#,
    )
    .await;
    assert_eq!(result, json!({ "terminateCode": 1, "exitCode": 1 }));
}

// ---------------------------------------------------------------------------
// Environment data crosses worker runtimes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_environment_data() {
    let result = run_worker_test(
        "worker_env",
        r#"
        import { Worker, setEnvironmentData } from "node:worker_threads";
        setEnvironmentData("shared", { v: 5 });
        const worker = new Worker(`
          const { parentPort, getEnvironmentData } = require("node:worker_threads");
          parentPort.postMessage({ env: getEnvironmentData("shared") });
        `, { eval: true });
        const message = await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
        });
        await worker.terminate();
        globalThis.__testResult = message;
        "#,
    )
    .await;
    assert_eq!(result, json!({ "env": { "v": 5 } }));
}

// ---------------------------------------------------------------------------
// Worker-side module state: isMainThread=false, threadId, parentPort
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_side_module_state() {
    let result = run_worker_test(
        "worker_state",
        r#"
        import { Worker } from "node:worker_threads";
        const worker = new Worker(`
          const wt = require("node:worker_threads");
          wt.parentPort.postMessage({
            isMainThread: wt.isMainThread,
            threadIdPositive: wt.threadId > 0,
            hasPort: wt.parentPort !== null,
            data: wt.workerData,
          });
        `, { eval: true, workerData: { n: 1 } });
        const message = await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
        });
        await worker.terminate();
        globalThis.__testResult = {
          ...message,
          parentThreadIdPositive: worker.threadId > 0,
        };
        "#,
    )
    .await;
    assert_eq!(
        result,
        json!({
            "isMainThread": false,
            "threadIdPositive": true,
            "hasPort": true,
            "data": { "n": 1 },
            "parentThreadIdPositive": true,
        })
    );
}

// ---------------------------------------------------------------------------
// Clone bridge contract: functions are rejected with DataCloneError
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_clone_rejects_functions() {
    let result = run_worker_test(
        "worker_clone",
        r#"
        import { Worker } from "node:worker_threads";
        const worker = new Worker(`setInterval(() => {}, 1000);`, { eval: true });
        let threw = null;
        try { worker.postMessage({ fn: () => {} }); } catch (error) {
          threw = { name: error.name, code: error.code };
        }
        await worker.terminate();
        globalThis.__testResult = threw;
        "#,
    )
    .await;
    assert_eq!(result, json!({ "name": "DataCloneError", "code": 25 }));
}

// ---------------------------------------------------------------------------
// scripted-workflow-isomorphic sequence: eval worker + workerData require of
// an absolute CJS module + vm sandbox + call/response message protocol
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_scripted_workflow_isomorph() {
    let result = run_worker_test(
        "worker_workflow",
        r#"
        import { Worker } from "node:worker_threads";
        import { writeFileSync } from "node:fs";
        const parserPath = process.cwd() + "/parser-module.cjs";
        writeFileSync(parserPath, "module.exports = { parseMarker: 'parsed-ok' };");
        const workerSource = String.raw`
          const { parentPort, workerData } = require("node:worker_threads");
          const vm = require("node:vm");
          const parser = require(workerData.parserPath);
          const pending = new Map();
          let nextCallId = 0;
          function hostCall(method, args) {
            return new Promise((resolve, reject) => {
              const callId = ++nextCallId;
              pending.set(callId, { resolve, reject });
              parentPort.postMessage({ type: "call", callId, method, args });
            });
          }
          parentPort.on("message", (message) => {
            if (message.type === "response") {
              const entry = pending.get(message.callId);
              if (!entry) return;
              pending.delete(message.callId);
              if (message.ok) entry.resolve(message.value); else entry.reject(new Error(message.error));
              return;
            }
            if (message.type !== "start") return;
            (async () => {
              const sandbox = {
                runs: { host: hostCall },
                parser,
                emit(value) { parentPort.postMessage({ type: "emit", value }); },
              };
              const context = vm.createContext(sandbox, { codeGeneration: { strings: false, wasm: false } });
              const script = new vm.Script("(async () => {\n" + message.script + "\n})()", { filename: "workflow-script.js" });
              const value = await script.runInContext(context);
              parentPort.postMessage({ type: "complete", value });
            })().catch((error) => parentPort.postMessage({ type: "error", error: String(error && error.message || error) }));
          });
        `;
        const worker = new Worker(workerSource, { eval: true, workerData: { parserPath } });
        const emits = [];
        const result = await new Promise((resolve, reject) => {
          worker.on("message", (message) => {
            if (message.type === "call") {
              worker.postMessage({ type: "response", callId: message.callId, ok: true, value: { output: "deployed " + message.args.target, stage: "build" } });
            } else if (message.type === "complete") {
              resolve(message.value);
            } else if (message.type === "emit") {
              emits.push(message.value);
            } else if (message.type === "error") {
              reject(new Error(message.error));
            }
          });
          worker.on("error", reject);
          worker.postMessage({ type: "start", script: "const r = await runs.host('deploy', { target: 'staging' }); emit({ stage: r.stage }); return { output: r.output, used: parser.parseMarker };" });
        });
        await worker.terminate();
        globalThis.__testResult = { value: result, emits };
        "#,
    )
    .await;
    assert_eq!(
        result,
        json!({
            "value": { "output": "deployed staging", "used": "parsed-ok" },
            "emits": [{ "stage": "build" }],
        })
    );
}

// ---------------------------------------------------------------------------
// Startup cost: a worker boots a full runtime; measure the first round trip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn worker_startup_cost() {
    let result = run_worker_test(
        "worker_startup",
        r#"
        import { Worker } from "node:worker_threads";
        const started = Date.now();
        const worker = new Worker(`
          const { parentPort } = require("node:worker_threads");
          parentPort.postMessage({ ready: true });
        `, { eval: true });
        const elapsed = await new Promise((resolve, reject) => {
          worker.on("message", () => resolve(Date.now() - started));
          worker.on("error", reject);
        });
        await worker.terminate();
        globalThis.__testResult = { elapsedMs: elapsed };
        "#,
    )
    .await;
    let elapsed = result["elapsedMs"].as_i64().expect("elapsedMs");
    // Full runtime boot on a fresh thread; generous CI bound, informative in
    // the failure message.
    assert!(
        elapsed < 30_000,
        "worker startup took {elapsed}ms (expected well under 30s)"
    );
}

// ---------------------------------------------------------------------------
// Mailbox bound: a worker busy in synchronous JS cannot drain its inbox, so
// the parent's posts hit the byte bound. Nothing is silent: each post reports
// `sent` or `droppedCount`, the accepted prefix still arrives in order, and
// posting works again once the pump is running.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn mailbox_bound_counts_drops_and_resumes() {
    let result = run_worker_test(
        "worker_mailbox_bound",
        r#"
        import { Worker } from "node:worker_threads";

        // The worker reports "up", then blocks its JS thread for 1.5 s: its
        // message pump starts only after the body returns, so while it is
        // blocked the inbound mailbox is the only thing holding the posted
        // messages (`parentSend` never blocks the parent).
        const worker = new Worker(`
          const { parentPort } = require("node:worker_threads");
          const received = [];
          parentPort.on("message", (m) => { received.push(m.n); });
          parentPort.postMessage({ phase: "up" });
          const end = Date.now() + 2500;
          while (Date.now() < end) { /* block the pump */ }
          setTimeout(() => parentPort.postMessage({
            received: received.length,
            first: received.length > 0 ? received[0] : -1,
            last: received.length > 0 ? received[received.length - 1] : -1,
          }), 250);
        `, { eval: true });

        await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
        });

        // 40 x 256 KiB > the 8 MiB mailbox bound: the tail must be dropped and
        // counted. The raw host bridge is used because the Worker shim's
        // postMessage swallows its answer by design (`catch { drop }`).
        const blob = "x".repeat(256 * 1024);
        let accepted = 0;
        let dropped = 0;
        let droppedCount = 0;
        for (let n = 0; n < 40; n++) {
          const reply = JSON.parse(__pir_host.worker("parentSend", JSON.stringify({
            id: worker.threadId,
            data: JSON.stringify({ n, blob }),
          })));
          if (!reply || reply.ok !== true) throw new Error("parentSend failed: " + JSON.stringify(reply));
          if (reply.value.sent) accepted++;
          else if (reply.value.dropped) { dropped++; droppedCount = Math.max(droppedCount, reply.value.droppedCount); }
        }

        const summary = await new Promise((resolve, reject) => {
          worker.on("message", resolve);
          worker.on("error", reject);
          worker.on("exit", () => resolve(null));
        });

        // The worker's pump is now draining: a further post is accepted again.
        const resumed = JSON.parse(__pir_host.worker("parentSend", JSON.stringify({
          id: worker.threadId,
          data: JSON.stringify({ n: "after" }),
        })));
        await new Promise((resolve) => setTimeout(resolve, 200));
        await worker.terminate();
        globalThis.__testResult = {
          accepted,
          dropped,
          droppedCount,
          summary,
          resumedSent: Boolean(resumed.value && resumed.value.sent),
        };
        "#,
    )
    .await;

    let accepted = result["accepted"].as_i64().expect("accepted");
    let dropped = result["dropped"].as_i64().expect("dropped");
    assert_eq!(
        accepted + dropped,
        40,
        "every post is either accepted or dropped: {result}"
    );
    assert!(
        dropped > 0,
        "40 x 256 KiB must exceed the 8 MiB mailbox bound: {result}"
    );
    assert_eq!(
        result["droppedCount"].as_i64(),
        Some(dropped),
        "the reply carries the cumulative drop count: {result}"
    );
    // The accepted prefix arrives exactly once, in order (drop-newest keeps
    // the start of the exchange).
    let summary = &result["summary"];
    assert_eq!(
        summary["received"].as_i64(),
        Some(accepted),
        "the worker receives exactly the accepted prefix: {result}"
    );
    assert_eq!(summary["first"].as_i64(), Some(0));
    assert_eq!(summary["last"].as_i64(), Some(accepted - 1));
    assert_eq!(
        result["resumedSent"], true,
        "a drained mailbox accepts again: {result}"
    );
}
