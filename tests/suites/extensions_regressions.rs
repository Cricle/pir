//! Regression tests for the extension-runtime audit findings: per-dialog
//! dialog wakeups (P1-3), the expiry sweep of dialog replies (P2-9), and the
//! owner-tagged interrupt deadlines for overlapping timed calls (P1-4).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::models_store::{read_models_store_entry, FileModelsStore};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel,
};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};

// ---------------------------------------------------------------------------
// Harness (same shape as tests/js_extension_api.rs)
// ---------------------------------------------------------------------------

fn fixture_dir(test_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-ext-regressions-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn handle_config(cwd: &std::path::Path, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn seed_and_load(handle: &ExtensionHandle, dir: &std::path::Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let path = dir.join(name);
    let result = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

/// `worker_threads` tests inspect process-wide live-thread counters and may
/// temporarily lower the spawn cap; serialize them within this test binary.
/// (No other suite in `g_extensions` spawns `worker_threads` Workers; the
/// codemode worker is a separate process.)
static WORKER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn worker_test_guard() -> std::sync::MutexGuard<'static, ()> {
    WORKER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// E1: reload/shutdown releases the previous generation's host resources
// ---------------------------------------------------------------------------

/// Starts one child process, one worker thread and one loopback HTTP server,
/// reports their identities, and keeps them alive (the worker loop and the
/// server accept loop never end on their own).
const HOLD_RESOURCES_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "hold_resources",
    description: "starts a child, a worker and an HTTP server for the reload-release regression",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const cp = require("node:child_process");
      const http = require("node:http");
      const { Worker } = require("node:worker_threads");
      const isWindows = process.platform === "win32";
      const child = cp.spawn(
        isWindows ? "powershell" : "sleep",
        isWindows ? ["-NoProfile", "-Command", "Start-Sleep -Seconds 300"] : ["300"],
        { stdio: "ignore" }
      );
      const worker = new Worker("setInterval(() => {}, 1000);", { eval: true });
      const server = http.createServer((req, res) => res.end("ok"));
      await new Promise((resolve) => server.listen(0, resolve));
      const info = { pid: child.pid, port: server.address().port, workerId: worker.threadId };
      // Keep the live handles referenced: a GC'd child/server/worker shim
      // object must not tear the resource down before the reload does.
      globalThis.__heldResources = { info: info, child: child, worker: worker, server: server };
      return { content: [{ type: "text", text: JSON.stringify(info) }] };
    },
  });
}
"#;

#[cfg(unix)]
fn test_pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn test_pid_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .output()
    else {
        return true;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let wanted = pid.to_string();
    stdout.lines().any(|line| {
        line.split(',')
            .nth(1)
            .map(|field| field.trim().trim_matches('"'))
            == Some(wanted.as_str())
    })
}

/// One `/reload` must kill the old generation's child, stop its worker, and
/// release its HTTP listener (the port becomes bindable again); the host
/// tables return to their pre-run state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn reload_releases_the_previous_generations_children_workers_and_servers() {
    use std::net::TcpListener;

    let _worker_test = worker_test_guard();
    let dir = fixture_dir("reload-release");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "hold.ts", HOLD_RESOURCES_TS).await;

    let baseline = pir::extensions::runtime::host_resource_counts_for_tests();
    let result = handle
        .call_tool("hold_resources", "call-1", &serde_json::json!({}))
        .await
        .expect("hold_resources tool");
    let text = result
        .content
        .iter()
        .find(|item| item.kind == "text")
        .map(|item| item.text.clone())
        .expect("tool text");
    let held: serde_json::Value = serde_json::from_str(&text).expect("held resource json");
    let pid = held["pid"].as_u64().expect("child pid") as u32;
    let port = held["port"].as_u64().expect("server port") as u16;
    assert!(test_pid_alive(pid), "the child starts alive");
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_err(),
        "the HTTP server holds its port before the reload"
    );
    assert!(
        !pir::extensions::runtime::host_resource_counts_for_tests().is_empty(),
        "the runtime registered its host handles: {:?}",
        pir::extensions::runtime::host_resource_counts_for_tests()
    );

    handle.reload().await.expect("reload");

    // The child is killed + reaped by `ChildHandle::drop`; poll briefly (the
    // OS needs a beat to retire the pid).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while test_pid_alive(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!test_pid_alive(pid), "the previous child must be gone");

    // The listener is dropped with the server handle; retry the bind until
    // the accept thread's cloned handle is closed.
    let mut bound = None;
    while std::time::Instant::now() < deadline {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                bound = Some(listener);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    assert!(
        bound.is_some(),
        "the previous generation's server port must be bindable after reload"
    );

    assert_eq!(
        pir::extensions::runtime::host_resource_counts_for_tests(),
        baseline,
        "reload must release the previous generation's host tables"
    );
    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P2-01: terminate vs. normal completion must report one stable exit code
// ---------------------------------------------------------------------------

/// Extension surface for the exit-code regressions. Every worker posts a
/// `ready` message before its body returns, so a boot failure cannot
/// masquerade as an "idle but running" worker:
/// - `spawn_idle` leaves a worker parked in its message pump;
/// - `terminate_idle` requests the stop through the host op and keeps the
///   parent JS thread busy while the worker's `exit` record is queued — the
///   ordering that used to surface code 0 for a terminated idle worker;
/// - `spawn_exit_zero` completes through the worker's own `process.exit(0)`;
/// - `exit_zero_then_terminate_without_yielding` exits only after the parent
///   releases a `go` gate, then asks the host for `terminate` from the same
///   non-yielding window; a read-only `state` snapshot proves the completed
///   worker is still retained in the registry at that instant;
/// - `race_terminate_with_exit_zero` runs both terminal paths concurrently.
const WORKER_IDLE_TS: &str = r#"
export default function (pi) {
  const { Worker } = require("node:worker_threads");
  const idleWorkers = [];
  globalThis.__idleWorkers = idleWorkers;

  const spawn = (code) => {
    const worker = new Worker(code, { eval: true });
    worker.on("error", () => {});
    return worker;
  };

  // Synchronous wait: the JS thread (and therefore the parent pump) does not
  // run until it returns.
  const busyWait = (ms) => {
    const end = Date.now() + ms;
    while (Date.now() < end) { /* do not yield */ }
  };

  pi.registerTool({
    name: "spawn_idle",
    description: "starts a worker that becomes idle in its message pump",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = spawn(`
        const { parentPort } = require("node:worker_threads");
        parentPort.postMessage("ready");
      `);
      await new Promise((resolve, reject) => {
        worker.once("message", resolve);
        worker.once("error", reject);
      });
      // Let the body return and the worker park in `workerRecvPoll` before
      // the parent asks for a terminate.
      await new Promise((resolve) => setTimeout(resolve, 50));
      idleWorkers.push(worker);
      return { content: [{ type: "text", text: String(worker.threadId) }] };
    },
  });

  pi.registerTool({
    name: "terminate_idle",
    description: "terminates the last idle worker and reports both exit-code views",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = idleWorkers[idleWorkers.length - 1];
      const exitEvent = new Promise((resolve) => worker.once("exit", resolve));
      // Request the stop through the host op, then busy-poll it without
      // yielding: the worker thread finalizes and queues its `exit` record
      // while the parent pump cannot run. When the pump resumes it must read
      // that record (this is the ordering that exposed the old code 0).
      const callTerminate = () => JSON.parse(
        __pir_host.worker("terminate", JSON.stringify({ id: worker._id }))
      );
      let final = callTerminate();
      const waitUntil = Date.now() + 2000;
      while (Date.now() < waitUntil && !(final.value && final.value.done)) {
        busyWait(1);
        final = callTerminate();
      }
      const exitCode = await exitEvent;
      const terminateCode = await worker.terminate();
      return {
        content: [{
          type: "text",
          text: JSON.stringify({
            exitCode: exitCode,
            terminateCode: terminateCode,
            directExitCode: final.value ? final.value.exitCode : null,
          }),
        }],
      };
    },
  });

  pi.registerTool({
    name: "spawn_exit_zero",
    description: "starts a worker that completes normally through process.exit(0)",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = spawn("process.exit(0)");
      const exitCode = await new Promise((resolve, reject) => {
        worker.once("exit", resolve);
        worker.once("error", reject);
      });
      const terminateCode = await worker.terminate();
      return {
        content: [{ type: "text", text: JSON.stringify({ exitCode: exitCode, terminateCode: terminateCode }) }],
      };
    },
  });

  pi.registerTool({
    name: "exit_zero_then_terminate_without_yielding",
    description: "completes normally while the parent pump is blocked, then terminates",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = spawn(`
        const { parentPort } = require("node:worker_threads");
        parentPort.postMessage("ready");
        parentPort.once("message", (message) => {
          if (message === "go") process.exit(0);
        });
      `);
      const exitEvent = new Promise((resolve) => worker.once("exit", resolve));
      await new Promise((resolve, reject) => {
        worker.once("message", resolve);
        worker.once("error", reject);
      });
      // Timing dependency: handling `ready` resumes the shim's `_pump`, which
      // queues its next `workerRecvPoll` host future, but the host call only
      // spawns that future; it is not polled while JS jobs are runnable. Yield
      // once here so the drive loop polls the future into its pending state
      // before `go` lets the worker exit. Without this barrier the pump has no
      // poll in flight when `op_terminate` drops the registry entry, so its
      // first poll afterwards hits `GONE_POLL` and `_settleExit(1)` turns the
      // recorded normal exit 0 into a spurious 1.
      await new Promise((resolve) => setTimeout(resolve, 50));
      // The `go` gate forces the completion into the non-yielding window:
      // the worker cannot call process.exit(0) before this message, and the
      // parent pump cannot run between the post and the terminate call below.
      worker.postMessage("go");
      // `state` is a read-only host snapshot (it neither drains the exit
      // record nor releases the registry entry). Spin on the worker's
      // published `done` flag rather than guessing a sleep: once done is
      // observed, the exit record is queued and the entry still holds the
      // recorded 0 — exactly the retention guarantee under test. On the
      // pre-fix code the entry is removed the moment the worker finishes, so
      // the probe reports `present: false` and terminate hits the fallback.
      const snapshot = () => JSON.parse(
        __pir_host.worker("state", JSON.stringify({ id: worker._id }))
      );
      let state = snapshot();
      const waitUntil = Date.now() + 5000;
      while (
        Date.now() < waitUntil &&
        !(state.ok && state.value && (state.value.done || !state.value.present))
      ) {
        busyWait(1);
        state = snapshot();
      }
      const direct = JSON.parse(__pir_host.worker("terminate", JSON.stringify({ id: worker._id })));
      const exitCode = await exitEvent;
      const terminateCode = await worker.terminate();
      return {
        content: [{
          type: "text",
          text: JSON.stringify({
            exitCode: exitCode,
            terminateCode: terminateCode,
            directExitCode: direct.value ? direct.value.exitCode : null,
            directDone: direct.value ? direct.value.done : null,
            retained: state.value ? state.value.present === true : false,
            retainedExitCode: state.value ? state.value.exitCode : null,
          }),
        }],
      };
    },
  });

  pi.registerTool({
    name: "race_terminate_with_exit_zero",
    description: "races terminate() against the worker's own process.exit(0)",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = spawn(`
        const { parentPort } = require("node:worker_threads");
        setTimeout(() => process.exit(0), 0);
        parentPort.postMessage("ready");
      `);
      const exitEvent = new Promise((resolve) => worker.once("exit", resolve));
      await new Promise((resolve, reject) => {
        worker.once("message", resolve);
        worker.once("error", reject);
      });
      const terminateCode = await worker.terminate();
      const exitCode = await exitEvent;
      return {
        content: [{ type: "text", text: JSON.stringify({ terminateCode: terminateCode, exitCode: exitCode }) }],
      };
    },
  });
}
"#;

/// P2-01: terminating a worker that already returned its body and is parked
/// in the message pump must report exit code 1 (`terminate` semantics), not
/// the 0 of a normal completion. The test forces the old race window: the
/// worker's `exit` record is queued while the parent JS thread is busy, so
/// the parent pump reads it before `terminate()`'s next poll can settle. On
/// the pre-fix code the record says `{kind:"exit",code:0}`, so both the
/// `exit` event and `terminate()` report 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_terminate_on_idle_worker_reports_exit_code_one() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-terminate-idle");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "idle.ts", WORKER_IDLE_TS).await;

    settle_worker_threads().await;
    let baseline_live = pir::extensions::runtime::live_worker_count_for_tests();
    let baseline_registry = pir::extensions::runtime::worker_count_for_tests();

    let spawned = handle
        .call_tool("spawn_idle", "call-idle-spawn", &serde_json::json!({}))
        .await
        .expect("spawn_idle tool");
    assert!(
        tool_text(&spawned).parse::<u64>().unwrap_or(0) > 0,
        "the idle worker started: {}",
        tool_text(&spawned)
    );

    let terminated = tokio::time::timeout(
        Duration::from_secs(10),
        handle.call_tool(
            "terminate_idle",
            "call-idle-terminate",
            &serde_json::json!({}),
        ),
    )
    .await
    .expect("terminate_idle must resolve")
    .expect("terminate_idle tool");
    let outcome: serde_json::Value =
        serde_json::from_str(tool_text(&terminated)).expect("terminate json");
    assert_eq!(
        outcome["exitCode"],
        serde_json::json!(1),
        "a terminated idle worker's exit event must be 1, not 0: {outcome}"
    );
    assert_eq!(
        outcome["terminateCode"],
        serde_json::json!(1),
        "terminate() must return the same code as the exit event: {outcome}"
    );
    assert_eq!(
        outcome["directExitCode"],
        serde_json::json!(1),
        "the host terminate answer must agree with the shim: {outcome}"
    );

    let (live, orphaned) = wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
    assert_eq!(live, baseline_live, "the terminated worker's thread exits");
    assert_eq!(orphaned, 0, "a terminated worker is not orphaned");
    assert_eq!(
        pir::extensions::runtime::worker_count_for_tests(),
        baseline_registry,
        "the terminated idle worker leaves the registry"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The control half of P2-01: a worker that completes through its own
/// `process.exit(0)` keeps code 0, both in its `exit` event and in the
/// shim's `terminate()` answer afterwards. This pins the fix to "forced stop
/// = 1" without turning every completed worker into a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_normal_exit_zero_survives_a_later_terminate() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-exit-zero");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "idle.ts", WORKER_IDLE_TS).await;

    settle_worker_threads().await;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        handle.call_tool("spawn_exit_zero", "call-exit-zero", &serde_json::json!({})),
    )
    .await
    .expect("spawn_exit_zero must resolve")
    .expect("spawn_exit_zero tool");
    let outcome: serde_json::Value = serde_json::from_str(tool_text(&result)).expect("exit json");
    assert_eq!(
        outcome["exitCode"],
        serde_json::json!(0),
        "a normal process.exit(0) completion stays 0: {outcome}"
    );
    assert_eq!(
        outcome["terminateCode"],
        serde_json::json!(0),
        "terminate() after a normal completion returns the recorded 0: {outcome}"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deterministic normal-completion race: after the worker's `ready`, the
/// tool yields once so the parent pump's next `workerRecvPoll` is pending
/// (a host call only spawns its future; the drive loop polls it once JS
/// yields) before the `go` gate lets the worker call `process.exit(0)`. The
/// parent then stays in the same non-yielding window (no pump, no
/// `op_release`) until it has observed the worker's published `done` flag
/// through the read-only host `state` snapshot. The registry must still
/// retain the completed worker at that point, so `terminate` reads the
/// recorded 0 instead of the hard-coded forced-stop 1; the shim's later
/// `terminate()` must return the same 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_terminate_after_blocked_normal_exit_reports_zero() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-exit-zero-blocked");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "idle.ts", WORKER_IDLE_TS).await;

    settle_worker_threads().await;
    let baseline_registry = pir::extensions::runtime::worker_count_for_tests();

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        handle.call_tool(
            "exit_zero_then_terminate_without_yielding",
            "call-exit-zero-blocked",
            &serde_json::json!({}),
        ),
    )
    .await
    .expect("blocked normal-exit tool must resolve")
    .expect("blocked normal-exit tool");
    let outcome: serde_json::Value =
        serde_json::from_str(tool_text(&result)).expect("blocked exit json");
    assert_eq!(
        outcome["exitCode"],
        serde_json::json!(0),
        "the normal completion's exit event stays 0: {outcome}"
    );
    assert_eq!(
        outcome["retained"],
        serde_json::json!(true),
        "the completed worker must still be retained in the registry before terminate: {outcome}"
    );
    assert_eq!(
        outcome["retainedExitCode"],
        serde_json::json!(0),
        "the retained entry must already carry the recorded 0: {outcome}"
    );
    assert_eq!(
        outcome["directDone"],
        serde_json::json!(true),
        "the worker must already be done when terminate is asked: {outcome}"
    );
    assert_eq!(
        outcome["directExitCode"],
        serde_json::json!(0),
        "the host terminate answer must use the recorded 0, not the forced-stop 1: {outcome}"
    );
    assert_eq!(
        outcome["terminateCode"],
        serde_json::json!(0),
        "the shim's terminate() agrees with the exit event: {outcome}"
    );
    assert_eq!(
        pir::extensions::runtime::worker_count_for_tests(),
        baseline_registry,
        "the released worker's registry entry is gone"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P2-01 race variant: `terminate()` and the worker's own `process.exit(0)`
/// may win in either order, but the two observable views must agree and the
/// value must be one of Node's terminal codes (0 = normal exit, 1 =
/// terminated). A stale 0 produced by the terminate path would still pass the
/// range check here, which is why the deterministic idle test above carries
/// the regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_terminate_racing_normal_exit_reports_consistent_codes() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-terminate-race");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "idle.ts", WORKER_IDLE_TS).await;

    settle_worker_threads().await;
    let baseline_live = pir::extensions::runtime::live_worker_count_for_tests();
    let baseline_registry = pir::extensions::runtime::worker_count_for_tests();

    for round in 0..3 {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            handle.call_tool(
                "race_terminate_with_exit_zero",
                &format!("call-exit-race-{round}"),
                &serde_json::json!({}),
            ),
        )
        .await
        .expect("race tool must resolve")
        .expect("race tool");
        let outcome: serde_json::Value =
            serde_json::from_str(tool_text(&result)).expect("race json");
        let terminate_code = outcome["terminateCode"].as_i64();
        let exit_code = outcome["exitCode"].as_i64();
        assert_eq!(
            terminate_code, exit_code,
            "round {round}: terminate() and the exit event must agree: {outcome}"
        );
        assert!(
            matches!(exit_code, Some(0) | Some(1)),
            "round {round}: only Node's normal/terminated codes are valid: {outcome}"
        );
    }

    let (live, orphaned) = wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
    assert_eq!(live, baseline_live, "every raced worker's thread exits");
    assert_eq!(orphaned, 0);
    assert_eq!(
        pir::extensions::runtime::worker_count_for_tests(),
        baseline_registry,
        "every raced worker's registry entry is released"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// E1: reload releases the previous generation's fs.watch threads too
// ---------------------------------------------------------------------------

/// `fs.watch` is the one host resource whose thread reaps itself only when its
/// `Weak<RuntimeState>` owner is gone — and a `/reload` REUSES that state for
/// the successor runtime. Without a generation-scoped release the previous
/// runtime's OS watcher and thread survive the reload with no JS pump left to
/// drain them.
const HOLD_WATCH_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "hold_watch",
    description: "starts an fs.watch for the reload-release regression",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const fs = require("node:fs");
      const watcher = fs.watch(process.cwd(), () => {});
      globalThis.__heldWatcher = watcher;
      return { content: [{ type: "text", text: "watching" }] };
    },
  });
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_releases_the_previous_generations_fs_watchers() {
    let dir = fixture_dir("reload-watcher");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "watch.ts", HOLD_WATCH_TS).await;

    let baseline = pir::extensions::runtime::fs_watch_count_for_tests();
    handle
        .call_tool("hold_watch", "call-1", &serde_json::json!({}))
        .await
        .expect("hold_watch tool");
    assert_eq!(
        pir::extensions::runtime::fs_watch_count_for_tests(),
        baseline + 1,
        "the tool started exactly one watcher"
    );

    handle.reload().await.expect("reload");
    assert_eq!(
        pir::extensions::runtime::fs_watch_count_for_tests(),
        baseline,
        "reload must release the previous generation's fs.watch handles"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// E4: a detached descendant holding the inherited pipe must not withhold the
// child's `exit` event (and must not keep the registry entry alive forever)
// ---------------------------------------------------------------------------

/// The direct child exits at once while a detached grandchild keeps the
/// inherited stdout pipe open for seconds. The shim only delivers Node's
/// `exit` event on the host's final sample (`done`), so `done` must be
/// finalized at process exit after the bounded drain — not gated on reader
/// EOF, which a held pipe never reaches.
const DETACHED_EXIT_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "detached_exit",
    description: "spawns a child whose detached grandchild holds the stdout pipe",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const cp = require("node:child_process");
      const isWindows = process.platform === "win32";
      const command = isWindows ? "cmd" : "sh";
      const args = isWindows
        ? ["/c", "start /b powershell -NoProfile -Command Start-Sleep -Seconds 12"]
        : ["-c", "sleep 12 &"];
      const child = cp.spawn(command, args, { stdio: ["ignore", "pipe", "pipe"] });
      const outcome = await new Promise((resolve) => {
        child.on("exit", (code) => resolve("exit:" + code));
        child.on("error", (error) =>
          resolve("error:" + (error && error.code ? error.code : String(error))));
      });
      return { content: [{ type: "text", text: outcome }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn a_detached_descendant_does_not_withhold_the_child_exit_event() {
    let dir = fixture_dir("detached-exit");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "detached.ts", DETACHED_EXIT_TS).await;
    // Generous: the pre-fix shape resolved only when the grandchild exited
    // (12 s later); this bound still makes the regression observable while
    // leaving room for process-spawn contention when the whole suite runs.
    handle.set_call_timeout(Duration::from_secs(25));

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        handle.call_tool("detached_exit", "call-1", &serde_json::json!({})),
    )
    .await
    .expect("the tool resolves")
    .expect("detached_exit tool");
    let elapsed = started.elapsed();
    assert_eq!(result.content[0].text, "exit:0");
    assert!(
        elapsed < Duration::from_secs(5),
        "the exit event must not wait for the detached pipe holder, took {elapsed:?}"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// E3: the 10-minute refreshModels window must serve the refreshed list, not
// the stale configured one, to a session whose registry never refreshed
// ---------------------------------------------------------------------------

const WINDOW_REFRESH_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("windowprov", {
    baseUrl: "http://localhost:11434",
    api: "openai-chat",
    models: [{ id: "stale", name: "Stale", contextWindow: 1024, maxTokens: 64 }],
    refreshModels: async () => ([
      { id: "fresh", name: "Fresh", contextWindow: 2048, maxTokens: 128 },
    ]),
  });
}
"#;

/// The refresh stamp is process-wide (a server hosts several sessions, a
/// subagent is another session) while every session has its own registry. A
/// second session inside the window must reuse the models the first refresh
/// produced; before the cached list it fell back to the configured (stale)
/// models because its own registry was still empty.
#[tokio::test(flavor = "current_thread")]
async fn refresh_window_reuses_the_refreshed_models_for_a_second_session() {
    let dir = fixture_dir("provider-refresh-window");

    let first_runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&first_runtime, &dir, "window.ts", WINDOW_REFRESH_TS).await;
    let first_registry = prompt_registry(&dir);
    let first_session = session_with_extensions(first_registry.clone(), first_runtime, &dir);
    first_session
        .prompt("go", no_events())
        .await
        .expect("first prompt");
    assert_eq!(
        provider_model_ids(&first_registry, "windowprov"),
        vec!["fresh".to_string()],
        "precondition: the first session refreshed"
    );
    drop(first_session);

    let second_runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&second_runtime, &dir, "window.ts", WINDOW_REFRESH_TS).await;
    let second_registry = prompt_registry(&dir);
    let second_session = session_with_extensions(second_registry.clone(), second_runtime, &dir);
    second_session
        .prompt("go", no_events())
        .await
        .expect("second prompt");
    assert_eq!(
        provider_model_ids(&second_registry, "windowprov"),
        vec!["fresh".to_string()],
        "a session inside the refresh window must not serve the configured stale list"
    );
    drop(second_session);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P1-3: answering one dialog must wake exactly its own waiter
// ---------------------------------------------------------------------------

const TWO_DIALOGS_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "ask_two",
    description: "opens two dialogs concurrently",
    parameters: { type: "object", properties: {}, required: [] },
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      const [first, second] = await Promise.all([
        ctx.ui.select("First", ["a", "b"]),
        ctx.ui.select("Second", ["x", "y"]),
      ]);
      return { content: [{ type: "text", text: first + ":" + second }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn answering_the_second_dialog_wakes_only_its_own_waiter() {
    let dir = fixture_dir("two-dialogs");
    let handle = ExtensionHandle::start(handle_config(&dir, true)).expect("runtime start");
    seed_and_load(&handle, &dir, "two.ts", TWO_DIALOGS_TS).await;
    // Bounded, but generous vs the test's own timeouts.
    handle.set_dialog_ttl(Duration::from_secs(30));

    let worker = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .call_tool("ask_two", "call-1", &serde_json::json!({}))
                .await
        })
    };

    // Collect both dialog requests (the host-fn bodies may schedule in
    // either order, so match by title, not collection order).
    struct Request {
        id: u64,
        title: String,
    }
    let mut requests: Vec<Request> = Vec::new();
    for _ in 0..500 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        for notification in handle.take_notifications(|notification| {
            matches!(notification, Notification::DialogRequest { .. })
        }) {
            if let Notification::DialogRequest { id, title, .. } = notification {
                requests.push(Request {
                    id,
                    title: title.unwrap_or_default(),
                });
            }
        }
        if requests.len() >= 2 {
            break;
        }
    }
    assert_eq!(requests.len(), 2, "both dialog requests must be observed");
    let reply_of = |title: &str| -> serde_json::Value {
        if title == "First" {
            serde_json::json!("a")
        } else {
            serde_json::json!("y")
        }
    };

    // Answer the dialog collected LAST first. With the old shared
    // `notify_one`, the other waiter could consume the permit, and the later
    // answer for its own dialog woke the wrong waiter again — stalling the
    // JS await. Each reply must wake exactly its own dialog.
    let last = requests[1].id;
    let last_reply = reply_of(&requests[1].title);
    handle.respond_dialog(&last.to_string(), last_reply);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let first = requests[0].id;
    let first_reply = reply_of(&requests[0].title);
    handle.respond_dialog(&first.to_string(), first_reply);

    let result = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("tool resolves (no dialog-waiter stall)")
        .expect("join")
        .expect("tool ok");
    assert_eq!(result.content[0].text, "a:y");
}

// ---------------------------------------------------------------------------
// P2-9: dialog replies/waiters expire instead of accumulating
// ---------------------------------------------------------------------------

const SELECT_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "ask",
    description: "asks through ctx.ui",
    parameters: { type: "object", properties: {}, required: [] },
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      const answer = await ctx.ui.select("Pick", ["a", "b"]);
      return { content: [{ type: "text", text: "select:" + answer }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn dialog_responses_expire_after_ttl() {
    let dir = fixture_dir("dialog-sweep");
    let handle = ExtensionHandle::start(handle_config(&dir, true)).expect("runtime start");
    handle.set_dialog_ttl(Duration::from_millis(150));

    // A reply for an id with no waiter would otherwise sit forever.
    handle.respond_dialog("ghost-1", serde_json::json!("stale"));
    assert_eq!(handle.dialog_response_count(), 1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    // The next reply sweeps expired entries before recording itself.
    handle.respond_dialog("ghost-2", serde_json::json!("fresh"));
    assert_eq!(
        handle.dialog_response_count(),
        1,
        "expired entry swept, only the fresh one remains"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unanswered_dialog_resolves_null_after_ttl() {
    let dir = fixture_dir("dialog-timeout");
    let handle = ExtensionHandle::start(handle_config(&dir, true)).expect("runtime start");
    seed_and_load(&handle, &dir, "dialog.ts", SELECT_TS).await;
    handle.set_dialog_ttl(Duration::from_millis(200));

    let started = std::time::Instant::now();
    let result = handle
        .call_tool("ask", "call-1", &serde_json::json!({}))
        .await
        .expect("unanswered dialog resolves null (bounded wait)");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "wait bounded by the dialog ttl, took {:?}",
        started.elapsed()
    );
    assert_eq!(result.content[0].text, "select:undefined");
    assert_eq!(handle.dialog_response_count(), 0);
}

// ---------------------------------------------------------------------------
// P1-4: overlapping timed calls keep their own interrupt deadlines
// ---------------------------------------------------------------------------

const SPIN_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "spin",
    description: "runaway JS loop",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const start = Date.now();
      while (Date.now() - start < 60000) {
        // runaway
      }
      return { content: [{ type: "text", text: "done" }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn overlapping_timed_calls_keep_their_own_deadlines() {
    let dir = fixture_dir("deadline-overlap");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "spin.ts", SPIN_TS).await;
    handle.set_call_timeout(Duration::from_secs(2));

    // Call B: a runaway tool; its interrupt deadline is armed first.
    let runaway = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .call_tool("spin", "call-runaway", &serde_json::json!({}))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Call A: a quick successful timed call that arms and then disarms ITS
    // OWN deadline while B's JS is still spinning. With the old shared
    // deadline, A's arm overwrote B's and A's disarm cancelled the
    // protection entirely — B's runaway loop was never interrupted and the
    // actor thread wedged.
    let mut payload = serde_json::json!({});
    handle
        .emit("no_handlers_event", &mut payload)
        .await
        .expect("quick overlapping call succeeds");

    let outcome = tokio::time::timeout(Duration::from_secs(15), runaway)
        .await
        .expect("runaway call completes (no actor wedge)");
    assert!(
        outcome.expect("join").is_err(),
        "runaway loop interrupted (its deadline survived A's disarm)"
    );

    // The runtime stays usable after the interrupts (`eval_json` parses the
    // JS string as JSON, so stringify and compare the number).
    let value = handle
        .eval_json("JSON.stringify(1 + 1)")
        .await
        .expect("runtime usable after interrupts");
    assert_eq!(value, serde_json::json!(2));
}

/// INVARIANT (bounded rejection log): the extension host never drains the
/// unhandled-promise-rejection record — only the `pir js` script runner does —
/// so the record must not grow without bound over a long-lived session that
/// keeps producing unhandled rejections. Past `RejectionLog::MAX` the OLDEST
/// reasons are evicted, the retained window stays in oldest-first order, and
/// the newest reasons (the useful ones, and what keeps `pir js`'s
/// non-empty-means-crash contract intact) survive. The `rejectionHandled`
/// FIFO removal pairs with the OLDEST record.
#[test]
fn rejection_log_evicts_oldest_past_the_cap() {
    use pir::extensions::runtime::RejectionLog;

    let mut log = RejectionLog::default();
    assert!(log.is_empty(), "a fresh log holds nothing");

    // A `rejectionHandled` event with an empty log is a no-op (the FALSE event
    // was never recorded).
    log.resolved();
    assert!(log.is_empty());

    let cap = RejectionLog::MAX;
    let overflow = 76usize;
    for index in 0..cap + overflow {
        log.record(format!("reason-{index}"));
    }
    assert_eq!(log.len(), cap, "the log never exceeds MAX reasons");

    let reasons = log.take();
    assert!(log.is_empty(), "take drains the log");
    assert_eq!(reasons.len(), cap);
    assert_eq!(
        reasons.first().map(String::as_str),
        Some("reason-76"),
        "the oldest {overflow} reasons are the evicted ones"
    );
    // The retained window is exactly the NEWEST `cap` reasons in oldest-first
    // order (the log is a bounded FIFO: `record` appends as newest, the
    // overflow pops the front). Compared as a whole window — an ordered
    // comparison of the `reason-<n>` strings themselves would be
    // lexicographic, where "reason-100" sorts before "reason-99".
    let expected: Vec<String> = (overflow..cap + overflow)
        .map(|index| format!("reason-{index}"))
        .collect();
    assert_eq!(
        reasons, expected,
        "the retained window is the newest {cap} reasons, oldest first"
    );

    // FIFO pairing: a handler attached to the oldest unhandled rejection drops
    // THAT record, leaving the newer ones.
    log.record("oldest".to_owned());
    log.record("newer".to_owned());
    log.resolved();
    assert_eq!(log.take(), vec!["newer".to_owned()]);
}

// ---------------------------------------------------------------------------
// Bounded fire-and-forget backlog (the provider_stream_event dispatch
// pipeline): one worker per backlog, ordered drain, bounded queue
// ---------------------------------------------------------------------------

/// N accepted events cost EXACTLY ONE spawned task (the pre-existing
/// spawn-per-event dispatcher cost N), and because the single worker awaits
/// one dispatch at a time the order is exactly the push order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_backlog_dispatches_with_one_worker_in_push_order() {
    use std::sync::{Arc, Mutex};

    use pir::extensions::runtime::EventBacklog;

    let dispatched: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&dispatched);
    let backlog = EventBacklog::<usize>::new(
        64,
        Arc::new(move |item: usize| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                seen.lock().unwrap().push(item);
            })
        }),
    );

    for index in 0..32 {
        assert!(backlog.push_with(|| index), "push {index} accepted");
    }
    assert_eq!(
        backlog.worker_starts(),
        1,
        "32 accepted pushes must cost ONE worker task"
    );
    assert_eq!(backlog.accepted(), 32);
    assert_eq!(backlog.dropped(), 0);

    for _ in 0..2000 {
        if backlog.processed() == 32 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(backlog.processed(), 32, "the worker drained every event");
    assert_eq!(
        *dispatched.lock().unwrap(),
        (0..32).collect::<Vec<_>>(),
        "one in-flight dispatch = exact push order"
    );
    // The probe is honest about ordering pressure too: no capacity was
    // exceeded (`accepted + dropped == pushes`).
    assert_eq!(backlog.accepted() + backlog.dropped(), 32);
}

/// A slow handler bounds memory instead of growing the queue without limit:
/// once the bounded queue is full the newest pushes are dropped (and counted)
/// — the non-blocking alternative to delaying the provider stream — and an
/// overflow never spawns another worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_backlog_drops_newest_when_the_bounded_queue_is_full() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use pir::extensions::runtime::EventBacklog;

    // Park the worker on its first item so the queue keeps its bounded
    // content while the test overflows it.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let backlog = EventBacklog::<usize>::new(2, {
        let gate = Arc::clone(&gate);
        let started = Arc::clone(&started);
        Arc::new(move |_item: usize| {
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&started);
            Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                let _permit = gate.acquire().await;
            })
        })
    });

    assert!(backlog.push_with(|| 0), "the first push starts the worker");
    for _ in 0..200 {
        if started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the worker must be parked on the first item"
    );

    // Fill the bounded queue (capacity 2) and overflow it (3 more pushes).
    for index in 1..=2 {
        assert!(backlog.push_with(|| index), "push {index} fits the bound");
    }
    for index in 3..=5 {
        assert!(
            !backlog.push_with(|| index),
            "push {index} must be dropped, not queued"
        );
    }
    assert_eq!(
        backlog.accepted(),
        3,
        "the bound held: 1 in flight + 2 queued"
    );
    assert_eq!(backlog.dropped(), 3);
    assert_eq!(
        backlog.worker_starts(),
        1,
        "overflow spawns no extra worker"
    );

    // Release the worker: it drains the queued items and then the dropped
    // count stays what it was.
    gate.add_permits(3);
    for _ in 0..200 {
        if backlog.processed() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(backlog.processed(), 3, "the queued items still dispatch");
    assert_eq!(backlog.dropped(), 3);
}

// ---------------------------------------------------------------------------
// Provider refresh: headers survive a refreshed catalog; publish({persist})
// writes models-store.json and restores across sessions.
// ---------------------------------------------------------------------------

fn session_model() -> Model {
    Model {
        id: "storefake-model".to_string(),
        name: "Store fake".to_string(),
        api: "storefake-api".to_string(),
        provider: "storefake".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

struct PromptProvider {
    model: Model,
}

impl Provider for PromptProvider {
    fn id(&self) -> &str {
        "storefake"
    }

    fn name(&self) -> &str {
        "Store fake"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "storefake-api".to_string(),
                provider: "storefake".to_string(),
                model: "storefake-model".to_string(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".to_string(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..AssistantMessage::default()
            },
        ));
        stream
    }
}

fn prompt_registry(agent_dir: &Path) -> Arc<ProviderRegistry> {
    let mut registry =
        ProviderRegistry::default().with_models_store(Arc::new(FileModelsStore::new(agent_dir)));
    registry.set_provider(Arc::new(PromptProvider {
        model: session_model(),
    }));
    Arc::new(registry)
}

fn session_with_extensions(
    registry: Arc<ProviderRegistry>,
    runtime: ExtensionHandle,
    agent_dir: &Path,
) -> Arc<AgentSession> {
    Arc::new(AgentSession::new_with_agent_dir(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: registry,
            model: session_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: None,
            cwd: Some(agent_dir.to_path_buf()),
            event_stream: None,
            extensions: Some(runtime),
            tool_selection: None,
        },
        agent_dir,
    ))
}

fn no_events() -> pir::agent_session::EventHandler {
    Arc::new(|_| {})
}

const REFRESH_HEADERS_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("headerprov", {
    baseUrl: "http://localhost:11434",
    api: "openai-chat",
    apiKey: "sk-test-123",
    authHeader: true,
    headers: { "X-Provider": "provider-value", "X-Shared": "provider" },
    models: [{
      id: "stale",
      name: "Stale",
      contextWindow: 1024,
      maxTokens: 64,
      headers: { "X-Model": "model-value", "X-Shared": "model" },
    }],
    refreshModels: async () => ([
      {
        id: "fresh",
        name: "Fresh",
        contextWindow: 2048,
        maxTokens: 128,
        headers: { "X-Refresh": "refresh-value" },
      },
    ]),
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn provider_refresh_keeps_provider_headers_and_auth_header() {
    let dir = fixture_dir("provider-refresh-headers");
    let runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&runtime, &dir, "headers.ts", REFRESH_HEADERS_TS).await;
    let registry = prompt_registry(&dir);
    let session = session_with_extensions(registry.clone(), runtime, &dir);
    session.prompt("go", no_events()).await.expect("prompt");

    let models = registry.get_provider_models("headerprov");
    assert_eq!(
        models.len(),
        1,
        "refresh replaces the configured list: {models:?}"
    );
    let model = &models[0];
    assert_eq!(model.id, "fresh");
    let headers = model
        .headers
        .as_ref()
        .expect("refreshed models must keep the configured headers");
    assert_eq!(
        headers.get("Authorization").map(String::as_str),
        Some("Bearer sk-test-123"),
        "authHeader must be re-applied after refresh: {headers:?}"
    );
    assert_eq!(
        headers.get("X-Provider").map(String::as_str),
        Some("provider-value")
    );
    assert_eq!(
        headers.get("X-Refresh").map(String::as_str),
        Some("refresh-value"),
        "model-level headers returned by the refresh must survive too"
    );
    assert_eq!(
        headers.get("X-Shared").map(String::as_str),
        Some("provider"),
        "provider headers merge under the refreshed model headers"
    );
    assert!(
        !headers.contains_key("X-Model"),
        "configured per-model headers must not leak onto refreshed models"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

const PUBLISH_PERSIST_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("storeprov", {
    baseUrl: "http://localhost:11434",
    api: "openai-chat",
    models: [{ id: "configured", name: "Configured", contextWindow: 512, maxTokens: 32 }],
    refreshModels: async (context) => {
      globalThis.__sawStored = context.stored
        ? (context.stored.models ?? []).map((model) => model.id)
        : null;
      await context.publish({
        persist: {
          models: [{ id: "cached-a", name: "Cached A", contextWindow: 111, maxTokens: 11 }],
          checkedAt: 42,
        },
      });
      // A native provider returns void and lets the publication supply the
      // refreshed list.
    },
  });
}
"#;

const OFFLINE_RESTORE_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("storeprov", {
    baseUrl: "http://localhost:11434",
    api: "openai-chat",
    models: [{ id: "configured", name: "Configured", contextWindow: 512, maxTokens: 32 }],
    refreshModels: async (context) => {
      globalThis.__sawStored = context.stored
        ? (context.stored.models ?? []).map((model) => model.id)
        : null;
      return [];
    },
  });
}
"#;

fn provider_model_ids(registry: &ProviderRegistry, provider: &str) -> Vec<String> {
    registry
        .get_provider_models(provider)
        .into_iter()
        .map(|model| model.id)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn publish_persist_writes_models_store_and_restores_across_restarts() {
    let dir = fixture_dir("provider-store-restore");

    // First session: the refresh callback publishes a catalog into the
    // models store and returns no models of its own; the publication's models
    // become this run's list.
    let first_runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&first_runtime, &dir, "publish.ts", PUBLISH_PERSIST_TS).await;
    let first_registry = prompt_registry(&dir);
    let first_session = session_with_extensions(first_registry.clone(), first_runtime, &dir);
    first_session
        .prompt("go", no_events())
        .await
        .expect("first prompt");
    assert_eq!(
        provider_model_ids(&first_registry, "storeprov"),
        vec!["cached-a".to_string()],
        "a publication's models are used when refreshModels returns none"
    );

    let entry = read_models_store_entry(&dir, "storeprov").expect("persisted catalog");
    assert_eq!(
        entry.checked_at,
        Some(42),
        "checkedAt round-trips: {entry:?}"
    );
    assert_eq!(entry.models.len(), 1, "{entry:?}");
    assert_eq!(entry.models[0]["id"], "cached-a");
    assert_eq!(
        entry.models[0]["provider"], "storeprov",
        "persisted models are stamped with the owning provider"
    );
    drop(first_session);

    // Second session (the "restart"): a fresh extension runtime that returns
    // no models. The host must backfill `context.stored` from models-store.json
    // and fall back to it when the refresh yields nothing.
    let second_runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&second_runtime, &dir, "offline.ts", OFFLINE_RESTORE_TS).await;
    let second_registry = prompt_registry(&dir);
    let second_session =
        session_with_extensions(second_registry.clone(), second_runtime.clone(), &dir);
    second_session
        .prompt("go", no_events())
        .await
        .expect("second prompt");
    assert_eq!(
        provider_model_ids(&second_registry, "storeprov"),
        vec!["cached-a".to_string()],
        "the persisted catalog must be restored when the refresh returns no models"
    );
    let saw = second_runtime
        .eval_json("JSON.stringify(globalThis.__sawStored ?? null)")
        .await
        .expect("probe");
    assert_eq!(
        saw,
        serde_json::json!(["cached-a"]),
        "refreshModels must receive the persisted catalog as context.stored"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn publish_persist_mirrors_to_the_agent_dir_for_an_in_memory_registry_store() {
    let dir = fixture_dir("provider-store-memory-mirror");
    let runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&runtime, &dir, "publish.ts", PUBLISH_PERSIST_TS).await;
    // The stock registry is memory-backed; the session must still persist the
    // publication to `<agent_dir>/models-store.json` so a restart restores it.
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(PromptProvider {
        model: session_model(),
    }));
    let registry = Arc::new(registry);
    let session = session_with_extensions(registry.clone(), runtime, &dir);
    session.prompt("go", no_events()).await.expect("prompt");

    assert_eq!(
        provider_model_ids(&registry, "storeprov"),
        vec!["cached-a".to_string()]
    );
    assert!(
        registry
            .models_store()
            .read("storeprov")
            .expect("in-memory read")
            .is_some(),
        "the registry's own store receives the publication"
    );
    let entry = read_models_store_entry(&dir, "storeprov")
        .expect("the agent-dir models-store.json mirrors the publication");
    assert_eq!(entry.models[0]["id"], "cached-a");
    assert_eq!(entry.checked_at, Some(42));

    let _ = std::fs::remove_dir_all(&dir);
}

// The `publish({persist: null})` deletion path end to end: the JS side hands
// the null decision back on the refresh outcome (it must survive the JS
// dispatch merge and the wire round trip), and the host deletes the provider's
// `models-store.json` entry — in-memory store and agent-dir mirror alike — so
// `read_models_store_entry` observes `None` afterwards and the deleted catalog
// is not restored into the live model list.
const PUBLISH_DELETE_TS: &str = r#"
export default function (pi) {
  pi.registerProvider("storeprov", {
    baseUrl: "http://localhost:11434",
    api: "openai-chat",
    models: [{ id: "configured", name: "Configured", contextWindow: 512, maxTokens: 32 }],
    refreshModels: async (context) => {
      globalThis.__storedSeen = globalThis.__storedSeen ?? [];
      globalThis.__storedSeen.push(
        context.stored ? (context.stored.models ?? []).map((model) => model.id) : null,
      );
      const applied = await context.publish({ persist: null });
      globalThis.__deleteApplied = applied;
      // Native provider shape: the publication (not the return value) is the
      // decision; an empty list keeps the configured models.
      return [];
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn publish_persist_null_deletes_the_models_store_entry_end_to_end() {
    let dir = fixture_dir("provider-store-delete");

    // Seed the store exactly like a previous session would have: the refresh
    // publishes `cached-a` (persist object).
    let first_runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&first_runtime, &dir, "publish.ts", PUBLISH_PERSIST_TS).await;
    let first_registry = prompt_registry(&dir);
    let first_session = session_with_extensions(first_registry.clone(), first_runtime, &dir);
    first_session
        .prompt("go", no_events())
        .await
        .expect("seed prompt");
    assert_eq!(
        provider_model_ids(&first_registry, "storeprov"),
        vec!["cached-a".to_string()],
        "precondition: the publication supplied the refreshed list"
    );
    assert!(
        read_models_store_entry(&dir, "storeprov").is_some(),
        "precondition: the seeded catalog is persisted"
    );
    drop(first_session);

    // Second session: a refresh that publishes `persist: null` deletes it.
    let second_runtime = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&second_runtime, &dir, "delete.ts", PUBLISH_DELETE_TS).await;
    let second_registry = prompt_registry(&dir);
    let second_session =
        session_with_extensions(second_registry.clone(), second_runtime.clone(), &dir);
    second_session
        .prompt("go", no_events())
        .await
        .expect("delete prompt");

    let seen = second_runtime
        .eval_json("JSON.stringify(globalThis.__storedSeen ?? null)")
        .await
        .expect("probe stored");
    let seen: Vec<Option<Vec<String>>> = serde_json::from_value(seen).expect("stored observations");
    assert!(
        !seen.is_empty(),
        "the refresh callback must have run at least once"
    );
    assert_eq!(
        seen[0],
        Some(vec!["cached-a".to_string()]),
        "the first delete refresh must observe the persisted catalog: {seen:?}"
    );
    let applied = second_runtime
        .eval_json("JSON.stringify(globalThis.__deleteApplied ?? null)")
        .await
        .expect("probe publish result");
    assert_eq!(
        applied,
        serde_json::json!(true),
        "publish({{persist: null}}) reports the deletion as applied"
    );

    assert!(
        read_models_store_entry(&dir, "storeprov").is_none(),
        "the agent-dir models-store.json entry must be deleted"
    );
    assert!(
        second_registry
            .models_store()
            .read("storeprov")
            .expect("in-memory read")
            .is_none(),
        "the registry's own store entry must be deleted too"
    );
    assert_eq!(
        provider_model_ids(&second_registry, "storeprov"),
        vec!["configured".to_string()],
        "a deleted catalog must not be restored into the live model list"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Resource bounds: fd table, readFile, decompression, worker count
// ---------------------------------------------------------------------------

const FD_FLOOD_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "fd_flood",
    description: "opens descriptors until the runtime cap",
    parameters: { type: "object", properties: {} },
    async execute() {
      const fs = require("node:fs");
      const path = __PATH__;
      const opened = [];
      let code = null;
      for (let i = 0; i < 400; i += 1) {
        try {
          opened.push(fs.openSync(path, "r"));
        } catch (error) {
          code = error.code || String(error);
          break;
        }
      }
      const count = opened.length;
      for (const fd of opened) {
        try { fs.closeSync(fd); } catch (error) {}
      }
      globalThis.__fdFlood = { count: count, code: code };
      return { content: [{ type: "text", text: String(count) }] };
    },
  });
}
"#;

/// A JS loop that opens descriptors without closing them must stop at the
/// runtime cap with Node's `EMFILE`, not exhaust the host process's OS fds.
#[tokio::test(flavor = "current_thread")]
async fn fd_table_caps_open_descriptors_with_emfile() {
    let dir = fixture_dir("fd-cap");
    let target = dir.join("readable.txt");
    std::fs::write(&target, "fd cap fixture").expect("write fixture");
    let path_json = serde_json::to_string(&target.to_string_lossy()).expect("path json");

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "fd.ts",
        &FD_FLOOD_TS.replace("__PATH__", &path_json),
    )
    .await;
    let result = handle
        .call_tool("fd_flood", "call-fd", &serde_json::json!({}))
        .await
        .expect("fd flood tool runs");
    assert_eq!(result.content[0].text, "256", "the table cap is 256 fds");
    let observed = handle
        .eval_json("JSON.stringify(globalThis.__fdFlood)")
        .await
        .expect("probe fd flood");
    assert_eq!(observed["count"], serde_json::json!(256));
    assert_eq!(observed["code"], serde_json::json!("EMFILE"));

    // The descriptors were closed by the fixture; a fresh open must work
    // again (the cap is live entries, not a lifetime budget).
    let code = format!(
        "JSON.stringify((() => {{ const fs = require('node:fs'); const fd = fs.openSync({path_json}, 'r'); fs.closeSync(fd); return true; }})())"
    );
    let reopened = handle.eval_json(&code).await.expect("reopen after close");
    assert_eq!(reopened, serde_json::json!(true));
}

const READ_FILE_CAP_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "read_file_cap",
    description: "reads a file through fs.readFileSync",
    parameters: { type: "object", properties: {} },
    async execute() {
      const fs = require("node:fs");
      let code = null;
      try {
        fs.readFileSync(__PATH__);
      } catch (error) {
        code = error.code || String(error);
      }
      return { content: [{ type: "text", text: String(code) }] };
    },
  });
}
"#;

/// `fs.readFile` materializes the file + base64 + JSON envelope on the host
/// heap; a 32 MiB+ file must be refused with `E2BIG`, and a small file must
/// still read normally.
#[tokio::test(flavor = "current_thread")]
async fn read_file_is_capped_and_small_files_still_read() {
    let dir = fixture_dir("read-file-cap");
    let huge = dir.join("huge.bin");
    let file = std::fs::File::create(&huge).expect("create huge");
    file.set_len(32 * 1024 * 1024 + 1).expect("sparse length");
    drop(file);
    let path_json = serde_json::to_string(&huge.to_string_lossy()).expect("path json");

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "read.ts",
        &READ_FILE_CAP_TS.replace("__PATH__", &path_json),
    )
    .await;
    let result = handle
        .call_tool("read_file_cap", "call-read", &serde_json::json!({}))
        .await
        .expect("read tool runs");
    assert_eq!(
        result.content[0].text, "E2BIG",
        "an over-cap readFile must report E2BIG"
    );

    let small = dir.join("small.txt");
    std::fs::write(&small, "small file body").expect("write small");
    let small_path = serde_json::to_string(&small.to_string_lossy()).expect("small path");
    let read = handle
        .eval_json(&format!(
            "JSON.stringify(require('node:fs').readFileSync({small_path}, 'utf8'))"
        ))
        .await
        .expect("small readFile");
    assert_eq!(read, serde_json::json!("small file body"));
}

/// A gzip bomb expands past the host-side decompression budget and must fail
/// with `Z_BUF_ERROR` instead of allocating the expansion on the host heap
/// (the QuickJS heap cap cannot see host allocations).
#[tokio::test(flavor = "current_thread")]
async fn decompression_output_is_capped() {
    use std::io::Write as _;

    let dir = fixture_dir("zlib-cap");
    let bomb = dir.join("bomb.gz");
    {
        let file = std::fs::File::create(&bomb).expect("create bomb");
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let chunk = vec![0u8; 1024 * 1024];
        for _ in 0..33 {
            encoder.write_all(&chunk).expect("write bomb input");
        }
        encoder.finish().expect("finish bomb");
    }
    let path_json = serde_json::to_string(&bomb.to_string_lossy()).expect("path json");
    let source = format!(
        r#"
export default function (pi) {{
  pi.registerTool({{
    name: "gunzip_bomb",
    description: "attempts a gzip bomb",
    parameters: {{ type: "object", properties: {{}} }},
    async execute() {{
      const fs = require("node:fs");
      const zlib = require("node:zlib");
      let code = null;
      try {{
        zlib.gunzipSync(fs.readFileSync({path_json}));
      }} catch (error) {{
        code = error.code || String(error);
      }}
      return {{ content: [{{ type: "text", text: String(code) }}] }};
    }},
  }});
}}
"#
    );
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(&handle, &dir, "bomb.ts", &source).await;
    let result = handle
        .call_tool("gunzip_bomb", "call-bomb", &serde_json::json!({}))
        .await
        .expect("gunzip tool runs");
    assert_eq!(result.content[0].text, "Z_BUF_ERROR");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The process-wide Worker cap is a deliberate host bound (one OS thread and
/// one QuickJS runtime per worker): keep it from drifting silently.
#[test]
fn worker_thread_cap_is_bounded() {
    assert_eq!(
        pir::extensions::runtime::worker_limit_for_tests(),
        64,
        "the per-generation worker registry cap is 64 (see worker_fns::MAX_WORKER_THREADS)"
    );
    assert_eq!(
        pir::extensions::runtime::live_worker_limit_for_tests(),
        128,
        "the cross-generation live-thread cap is 2x the per-generation cap"
    );
}

// ---------------------------------------------------------------------------
// R1-L5-003: worker_threads synchronous loops must terminate (deadline,
// terminate interrupt, and cross-generation bounding)
// ---------------------------------------------------------------------------

/// Extension surface used by the three worker-lifecycle regressions:
/// - `spawn_spin` starts `while (true) {}` (a worker that never awaits);
/// - `terminate_spin` calls the real `Worker.terminate()` on it;
/// - `spawn_and_exit` starts a worker that exits by itself and resolves on
///   its `exit` event, proving a later spawn still works.
const WORKER_SPIN_TS: &str = r#"
export default function (pi) {
  const { Worker } = require("node:worker_threads");
  const spinWorkers = [];
  globalThis.__spinWorkers = spinWorkers;

  pi.registerTool({
    name: "spawn_spin",
    description: "starts a worker spinning in a synchronous loop",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = new Worker("while (true) {}", { eval: true });
      worker.on("error", () => {});
      spinWorkers.push(worker);
      return {
        content: [{ type: "text", text: JSON.stringify({ id: worker.threadId }) }],
      };
    },
  });

  pi.registerTool({
    name: "terminate_spin",
    description: "terminates the last spinning worker",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = spinWorkers[spinWorkers.length - 1];
      const started = Date.now();
      const code = await worker.terminate();
      return {
        content: [{
          type: "text",
          text: JSON.stringify({ code: code, elapsedMs: Date.now() - started }),
        }],
      };
    },
  });

  pi.registerTool({
    name: "spawn_and_exit",
    description: "starts a worker that exits on its own",
    parameters: { type: "object", properties: {}, required: [] },
    async execute() {
      const worker = new Worker("process.exit(0)", { eval: true });
      worker.on("error", () => {});
      const code = await new Promise((resolve) => worker.once("exit", resolve));
      return { content: [{ type: "text", text: String(code) }] };
    },
  });
}
"#;

fn tool_text(result: &pir::extensions::ToolResult) -> &str {
    result
        .content
        .iter()
        .find(|item| item.kind == "text")
        .map(|item| item.text.as_str())
        .expect("tool text")
}

/// Poll the registry-independent live-thread count until it falls to
/// `target_live` AND no released-generation worker remains marked orphaned.
async fn wait_for_worker_quiescence(target_live: usize, timeout: Duration) -> (usize, usize) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let live = pir::extensions::runtime::live_worker_count_for_tests();
        let orphaned = pir::extensions::runtime::orphaned_worker_count_for_tests();
        if (live <= target_live && orphaned == 0) || std::time::Instant::now() >= deadline {
            return (live, orphaned);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `LIVE_WORKERS` is process-wide: wait for a previous test's worker thread
/// (e.g. E1's `setInterval` worker after `shutdown()`) to finish before
/// capturing a baseline, so a stale count cannot shift `baseline + 1`.
async fn settle_worker_threads() {
    let _ = wait_for_worker_quiescence(0, Duration::from_secs(10)).await;
}

/// EXT2 retains a finished worker's registry entry until the parent pump has
/// observed the exit (`_settleExit` → `op_release`), so `LIVE_WORKERS` can
/// fall before the registry drains. Wait for the registry to reach the
/// baseline before asserting on it (P3-EXT2-04).
async fn wait_for_worker_registry(target_registry: usize, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while pir::extensions::runtime::worker_count_for_tests() > target_registry
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `terminate()` on a worker stuck in `while (true) {}` must resolve in
/// bounded time (the abort flag drives the worker runtime's QuickJS interrupt
/// handler, not the message pump), the OS thread must actually exit, and a
/// later `new Worker` must still succeed. On the pre-fix implementation the
/// terminate op polls forever/200 ms and the thread never exits, so the
/// outer `tokio::time::timeout` fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_terminate_interrupts_a_synchronous_loop_and_releases_the_thread() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-terminate-spin");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "spin.ts", WORKER_SPIN_TS).await;

    settle_worker_threads().await;
    let baseline_live = pir::extensions::runtime::live_worker_count_for_tests();
    let baseline_registry = pir::extensions::runtime::worker_count_for_tests();

    let spawned = handle
        .call_tool("spawn_spin", "call-spawn-spin", &serde_json::json!({}))
        .await
        .expect("spawn_spin tool");
    let info: serde_json::Value = serde_json::from_str(tool_text(&spawned)).expect("spawn json");
    assert!(
        info["id"].as_u64().unwrap_or(0) > 0,
        "the worker started: {info}"
    );
    assert_eq!(
        pir::extensions::runtime::live_worker_count_for_tests(),
        baseline_live + 1,
        "the worker thread is counted live"
    );

    // Let the worker enter the synchronous loop (the abort flag is checked by
    // the worker's own QuickJS interrupt handler regardless, but a real spin
    // makes the regression exercise the intended path).
    tokio::time::sleep(Duration::from_millis(100)).await;

    let terminated = tokio::time::timeout(
        Duration::from_secs(10),
        handle.call_tool(
            "terminate_spin",
            "call-terminate-spin",
            &serde_json::json!({}),
        ),
    )
    .await
    .expect("terminate() must resolve for a synchronous worker loop")
    .expect("terminate_spin tool");
    let outcome: serde_json::Value =
        serde_json::from_str(tool_text(&terminated)).expect("terminate json");
    assert_eq!(outcome["code"], serde_json::json!(1), "{outcome}");

    let (live, orphaned) = wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
    assert_eq!(
        live, baseline_live,
        "the terminated worker's OS thread must exit"
    );
    assert_eq!(orphaned, 0, "a terminated worker is not orphaned");
    assert_eq!(
        pir::extensions::runtime::worker_count_for_tests(),
        baseline_registry,
        "the registry entry must be removed"
    );

    // A later spawn must still work: the per-worker thread/QuickJS runtime
    // was released, not merely detached.
    let exited = handle
        .call_tool("spawn_and_exit", "call-spawn-exit", &serde_json::json!({}))
        .await
        .expect("spawn_and_exit tool");
    assert_eq!(tool_text(&exited), "0", "a fresh worker exits normally");
    let (live, _) = wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
    assert_eq!(live, baseline_live, "the probe worker also exits");

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Requirement 1: the worker script's synchronous body runs under the
/// inherited call deadline; a `while (true) {}` nobody terminates must still
/// be interrupted by the worker runtime's QuickJS interrupt handler and the
/// thread must exit. On the pre-fix implementation the loop runs until the
/// process exits, so the wait times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_synchronous_loop_is_stopped_by_the_worker_deadline() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-deadline-spin");
    let mut config = handle_config(&dir, false);
    config.limits = Some(pir::extensions::ExtensionLimits {
        call_timeout: Duration::from_millis(600),
        ..Default::default()
    });
    let handle = ExtensionHandle::start(config).expect("runtime start");
    seed_and_load(&handle, &dir, "spin.ts", WORKER_SPIN_TS).await;

    settle_worker_threads().await;
    let baseline_live = pir::extensions::runtime::live_worker_count_for_tests();
    let baseline_registry = pir::extensions::runtime::worker_count_for_tests();

    handle
        .call_tool("spawn_spin", "call-spawn-deadline", &serde_json::json!({}))
        .await
        .expect("spawn_spin tool");
    assert_eq!(
        pir::extensions::runtime::live_worker_count_for_tests(),
        baseline_live + 1
    );

    let (live, orphaned) = wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
    assert_eq!(
        live, baseline_live,
        "the worker body deadline must abort the synchronous loop"
    );
    assert_eq!(orphaned, 0);
    // The thread can exit before the parent pump consumes its error/exit
    // records and calls `op_release`; the retention path makes the registry
    // assertion racy unless both counters have drained.
    wait_for_worker_registry(baseline_registry, Duration::from_secs(10)).await;
    assert_eq!(
        pir::extensions::runtime::worker_count_for_tests(),
        baseline_registry,
        "the deadline-aborted worker leaves the registry"
    );

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Requirement 3: `/reload` must abort the old generation's spinning workers
/// (not just drop their registry entries). Repeating the cycle must not grow
/// the live-thread count, and the cross-generation counter must return to the
/// baseline after every reload. On the pre-fix implementation the leaked
/// threads accumulate, so the live count never returns to baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn reload_aborts_spinning_workers_without_cross_generation_accumulation() {
    let _worker_test = worker_test_guard();
    let dir = fixture_dir("worker-reload-spin");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "spin.ts", WORKER_SPIN_TS).await;

    settle_worker_threads().await;
    let baseline_live = pir::extensions::runtime::live_worker_count_for_tests();
    let baseline_registry = pir::extensions::runtime::worker_count_for_tests();

    for round in 0..3 {
        handle
            .call_tool(
                "spawn_spin",
                &format!("call-spawn-reload-{round}"),
                &serde_json::json!({}),
            )
            .await
            .expect("spawn_spin tool");
        assert_eq!(
            pir::extensions::runtime::live_worker_count_for_tests(),
            baseline_live + 1,
            "round {round}: exactly one old-generation worker is alive"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        handle.reload().await.expect("reload");
        let (live, orphaned) =
            wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
        assert_eq!(
            live, baseline_live,
            "round {round}: the reloaded generation must not inherit a leaked worker thread"
        );
        assert_eq!(
            orphaned, 0,
            "round {round}: every released-generation thread exited"
        );
        assert_eq!(
            pir::extensions::runtime::worker_count_for_tests(),
            baseline_registry,
            "round {round}: the registry holds only the fresh generation's workers"
        );
    }

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Requirement 3: once the registry-independent live-thread cap is reached
/// (simulated through the test override, so the test does not reserve 128
/// real 64 MiB stacks), `op_start` must refuse to spawn instead of letting a
/// `/reload` loop grow the leak without bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // the worker-test lock must span the runtime awaits
async fn worker_live_cap_rejects_spawn_while_orphaned_threads_remain() {
    struct RestoreLiveLimit;
    impl Drop for RestoreLiveLimit {
        fn drop(&mut self) {
            pir::extensions::runtime::test_set_live_worker_limit(0);
        }
    }

    let _worker_test = worker_test_guard();
    let _restore = RestoreLiveLimit;
    let dir = fixture_dir("worker-live-cap");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    seed_and_load(&handle, &dir, "spin.ts", WORKER_SPIN_TS).await;

    settle_worker_threads().await;
    let baseline_live = pir::extensions::runtime::live_worker_count_for_tests();
    // One free slot: the spinning worker below consumes it.
    pir::extensions::runtime::test_set_live_worker_limit(baseline_live + 1);

    handle
        .call_tool("spawn_spin", "call-cap-spawn", &serde_json::json!({}))
        .await
        .expect("spawn_spin tool");
    assert_eq!(
        pir::extensions::runtime::live_worker_count_for_tests(),
        baseline_live + 1
    );

    // Probe the host op directly: the Worker shim surfaces a failed start as
    // an async `error` event, which would hide the rejection shape here.
    let probe = handle
        .eval_json(
            r#"JSON.stringify(JSON.parse(__pir_host.worker("start", JSON.stringify({ eval: true, code: "0", filename: "" }))))"#,
        )
        .await
        .expect("start probe");
    assert_eq!(
        probe["ok"],
        serde_json::json!(false),
        "cap must refuse: {probe}"
    );
    let message = probe["message"].as_str().unwrap_or("");
    assert!(
        message.contains("live worker threads"),
        "the rejection names the cross-generation cap: {message}"
    );

    handle
        .call_tool(
            "terminate_spin",
            "call-cap-terminate",
            &serde_json::json!({}),
        )
        .await
        .expect("terminate_spin tool");
    let (live, orphaned) = wait_for_worker_quiescence(baseline_live, Duration::from_secs(10)).await;
    assert_eq!(live, baseline_live);
    assert_eq!(orphaned, 0);

    handle.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}
