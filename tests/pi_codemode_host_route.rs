//! pi-codemode host route (DRIFT §2.3 item 2 / D28): the
//! `@earendil-works/pi-codemode` shim's engine-backed faces —
//! `CodemodeSandboxBase.execute()` / `start()` (and the
//! `CodemodeSandbox` / `InlineCodemodeSandbox` subclasses) — run on the HOST
//! codemode sandbox ([`pir::plugins::codemode`]) through the
//! `__pir_host.codemode*` pull protocol, with the extension's JS side
//! answering the script's calls. `RemoteCodemodeSandbox` /
//! `serveCodemodeRemote` / `loadQuickJSWasm` keep throwing (no second
//! isolate, no wasm engine; §2.3 records the decision).
//!
//! The fixture is seeded from source at runtime (the repo hygiene gate
//! forbids tracked `.ts` files), mirroring tests/suites/js_extension_api.rs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::agent::nested_tool_calls::{NestedEventSink, NestedShared, NestedToolExecutor};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-codemode-route-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn handle_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn started_handle(test_name: &str) -> (ExtensionHandle, PathBuf) {
    let dir = fixture_dir(test_name);
    let handle = ExtensionHandle::start(handle_config(&dir)).expect("runtime start");
    (handle, dir)
}

async fn seed_and_load(handle: &ExtensionHandle, dir: &Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let result = handle
        .load(std::slice::from_ref(&dir.join(name)))
        .await
        .expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

/// A host tool the sandbox's session tier (the nested-call executor) reaches.
struct HostEchoTool;

impl AgentTool for HostEchoTool {
    fn name(&self) -> &'static str {
        "host_echo"
    }

    fn description(&self) -> &'static str {
        "echoes its text argument (host tool, session tier)"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        }))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move { ToolResult::ok(format!("host:{text}")) })
    }
}

/// Bind the host session's in-flight nested executor the way
/// `ExtensionTool::run` does for a real tool call, so the sandbox's session
/// tier can resolve tool names it did not declare.
fn bind_session_executor(handle: &ExtensionHandle, dir: &Path, call_id: &str) -> Arc<NestedShared> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(HostEchoTool));
    let sink: NestedEventSink = Arc::new(|_event| Box::pin(async {}));
    let shared = NestedShared::new(
        Arc::new(registry),
        pir::agent::tool_scheduler::ToolScheduler::new(Default::default()),
        None,
        Some(handle.clone()),
        dir.to_path_buf(),
        None,
        false,
        sink,
        None,
        None,
    );
    let executor = NestedToolExecutor::root(Arc::clone(&shared)).for_call(call_id);
    handle.bind_nested_executor(call_id, Arc::new(executor));
    shared
}

/// Run one probe through the fixture's `probe` extension tool. A probe that
/// throws surfaces as a `call_tool` error; a probe that returns rides the
/// tool result as JSON text.
async fn probe(handle: &ExtensionHandle, call_id: &str, name: &str) -> Value {
    let result = handle
        .call_tool("probe", call_id, &json!({ "name": name }))
        .await
        .unwrap_or_else(|error| panic!("probe `{name}` failed: {error}"));
    let text = result
        .content
        .iter()
        .map(|item| item.text.as_str())
        .collect::<String>();
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("probe `{name}` reply: {text} ({error})"))
}

// ---------------------------------------------------------------------------
// The fixture extension: one `probe` tool dispatching named face checks
// ---------------------------------------------------------------------------

const PROBE_TS: &str = r#"
import {
  CodemodeSandbox, CodemodeSandboxBase, InlineCodemodeSandbox, RemoteCodemodeSandbox,
  serveCodemodeRemote, loadQuickJSWasm,
} from "@earendil-works/pi-codemode";

function summarize(result) {
  return {
    ok: result.ok,
    value: result.value === undefined ? null : result.value,
    valueIsUndefined: result.value === undefined,
    output: result.output,
    calls: result.calls,
    storeWrites: result.storeWrites === undefined ? null : result.storeWrites,
    error: result.error === undefined ? null : result.error,
    during: result.during === undefined ? null : result.during,
  };
}

function thrown(call) {
  try {
    call();
    return "no-throw";
  } catch (error) {
    return { message: String((error && error.message) || error), code: error && error.code };
  }
}

async function rejected(promise) {
  try {
    await promise;
    return "no-reject";
  } catch (error) {
    return { message: String((error && error.message) || error), code: error && error.code };
  }
}

async function runProbe(name) {
  switch (name) {
    case "execute_basic": {
      return summarize(await new CodemodeSandbox().execute("return 1 + 1;"));
    }

    case "execute_output": {
      return summarize(await new CodemodeSandbox().execute(
        "text('first');\nconsole.log('second', 2);\nreturn { n: 7 };",
      ));
    }

    case "execute_values": {
      const sandbox = new CodemodeSandbox();
      return {
        exited: summarize(await sandbox.execute("exit();")),
        nulled: summarize(await sandbox.execute("return null;")),
        object: summarize(await sandbox.execute("return { a: [1, 2] };")),
      };
    }

    case "execute_declared_tool": {
      // A declared tool runs JS side (upstream `CodemodeTool.execute`), and
      // `ALL_TOOLS` lists the declared table (upstream prelude).
      const sandbox = new CodemodeSandbox({
        tools: [{
          name: "add",
          description: "adds two numbers",
          async execute(args) {
            return { sum: args.a + args.b };
          },
        }],
      });
      return summarize(await sandbox.execute(
        "const r = await tools.add({ a: 2, b: 3 });\n" +
        "return { sum: r.sum, all: ALL_TOOLS.map((tool) => tool.name) };",
      ));
    }

    case "execute_declared_global": {
      const sandbox = new CodemodeSandbox({
        globals: [{
          name: "host.attach",
          description: "attaches an image",
          execute(args) {
            return "attached:" + JSON.stringify(args);
          },
        }],
      });
      return summarize(await sandbox.execute("return await host.attach({ id: 3 });"));
    }

    case "execute_host_tool": {
      // Not declared by the sandbox: the host resolves it against the running
      // session's tool set (the nested-call executor bound to the call).
      return summarize(
        await new CodemodeSandbox().execute("return await tools.host_echo({ text: 'hi' });"),
      );
    }

    case "execute_unknown_tool": {
      return summarize(await new CodemodeSandbox().execute(
        "try { await tools.nope(); } catch (error) { return String(error.message); }",
      ));
    }

    case "execute_timeout": {
      return summarize(await new CodemodeSandbox({ timeoutMs: 250 }).execute("while (true) {}"));
    }

    case "execute_abort": {
      const controller = new AbortController();
      setTimeout(() => controller.abort(new Error("caller stopped it")), 100);
      return summarize(
        await new CodemodeSandbox().execute("while (true) {}", { signal: controller.signal }),
      );
    }

    case "execute_store": {
      return summarize(await new CodemodeSandbox().execute(
        "store('k', { a: 1 });\nreturn { written: load('k').a, seeded: load('seeded') };",
        { store: { seeded: 5 } },
      ));
    }

    case "inline_executes": {
      // The inline sandbox routes to the same host engine; the `wasm` option
      // is accepted and ignored (documented residual).
      return summarize(await new InlineCodemodeSandbox({ wasm: {} }).execute("return 2;"));
    }

    case "base_class_executes": {
      return summarize(await new CodemodeSandboxBase({}).execute("return 'base';"));
    }

    case "start_deterministic": {
      const calls = [];
      const idles = [];
      let fingerprint = null;
      let run = null;
      const sandbox = new CodemodeSandbox();
      run = sandbox.start(
        "const a = await tools.double({ n: 2 });\n" +
        "const b = await tools.double({ n: 10 });\n" +
        "return { sum: a + b, clock: Date.now(), random: Math.random() };",
        {
          startedAt: 1000,
          seed: 7,
          tools: [{ name: "double", description: "doubles" }],
          onStarted: (value) => { fingerprint = value; },
          onIdle: (seq, issued) => { idles.push([seq, issued]); },
          onCall: (call) => {
            calls.push({ id: call.id, target: call.target, name: call.name, args: call.args });
            // Answer in `seq` order (the caller's numbering).
            run.deliver(call.id, 5000 + call.id, call.id, { ok: true, value: call.args.n * 2 });
          },
        },
      );
      const result = await run.result;
      return {
        ...summarize(result),
        fingerprint,
        idles,
        hostCalls: calls,
        finished: run.finished,
      };
    }

    case "start_errors": {
      const sandbox = new CodemodeSandbox();
      return {
        startedAt: thrown(() => sandbox.start("return 1;", { startedAt: -1, seed: 1 })),
        seed: thrown(() => sandbox.start("return 1;", { startedAt: 1, seed: 1.5 })),
        duplicateTools: thrown(() => sandbox.start("return 1;", {
          startedAt: 1, seed: 1, tools: [{ name: "a" }, { name: "a" }],
        })),
        reservedGlobal: thrown(() => sandbox.start("return 1;", {
          startedAt: 1, seed: 1, globals: [{ name: "tools.x" }],
        })),
      };
    }

    case "start_abort": {
      const controller = new AbortController();
      const sandbox = new CodemodeSandbox();
      const run = sandbox.start("await tools.never({});\nreturn 1;", {
        startedAt: 1,
        seed: 1,
        tools: [{ name: "never" }],
        signal: controller.signal,
        onCall: () => { setTimeout(() => controller.abort(), 50); },
      });
      return { result: summarize(await run.result) };
    }

    case "close_rejects": {
      const sandbox = new CodemodeSandbox();
      await sandbox.close();
      return {
        execute: await rejected(sandbox.execute("return 1;")),
        start: thrown(() => sandbox.start("return 1;", { startedAt: 0, seed: 1 })),
      };
    }

    case "remote_throws": {
      return {
        remoteExecute: thrown(() => new RemoteCodemodeSandbox({}).execute("return 1;")),
        remoteStart: thrown(() => new RemoteCodemodeSandbox({}).start("return 1;", {})),
        serve: thrown(() => serveCodemodeRemote({}, async () => null, {})),
        loadWasm: await loadQuickJSWasm().then(() => "resolved", (error) => error.message),
      };
    }

    case "start_many_concurrent_calls": {
      // The host bounds its host->JS event queue at 256 events and parks the
      // producer when it is full. A slow consumer (3 ms of JS stalls per
      // answer) makes the queue fill while the sandbox issues its whole
      // `Promise.all` wave, so the bound is exercised end to end: if any call
      // event were lost, its promise would never resolve and the run would
      // not report 300 results.
      const COUNT = 300;
      const calls = [];
      let run = null;
      const sandbox = new CodemodeSandbox();
      run = sandbox.start(
        "const results = await Promise.all(Array.from({ length: " + COUNT + " }, (_, i) => tools.bulk({ i: i })));\n" +
        "return { sum: results.reduce((a, v) => a + v, 0), n: results.length };",
        {
          startedAt: 1,
          seed: 1,
          tools: [{ name: "bulk", description: "bulk call" }],
          onCall: (call) => {
            calls.push(call.args.i);
            const end = Date.now() + 3;
            while (Date.now() < end) { /* stall the consumer */ }
            run.deliver(call.id, 1000 + call.id, call.id, { ok: true, value: call.args.i });
          },
        },
      );
      const result = await run.result;
      return {
        ...summarize(result),
        callCount: calls.length,
        ordered: calls.every((value, index) => value === index),
      };
    }

    default:
      throw new Error("unknown probe: " + name);
  }
}

export default function (pi) {
  pi.registerTool({
    name: "probe",
    description: "codemode host-route probes",
    parameters: {
      type: "object",
      properties: { name: { type: "string" } },
      required: ["name"],
    },
    async execute(_toolCallId, params) {
      const out = await runProbe(String(params.name));
      return { content: [{ type: "text", text: JSON.stringify(out) }] };
    },
  });
}
"#;

async fn probe_handle(test_name: &str) -> (ExtensionHandle, PathBuf) {
    let (handle, dir) = started_handle(test_name).await;
    seed_and_load(&handle, &dir, "codemode-route.ts", PROBE_TS).await;
    (handle, dir)
}

// ---------------------------------------------------------------------------
// a) `execute("1+1")` runs on the host engine
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sandbox_execute_runs_on_host_engine() {
    let (handle, _dir) = probe_handle("execute").await;
    let out = probe(&handle, "call-1", "execute_basic").await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["value"], json!(2), "{out}");
    assert_eq!(out["valueIsUndefined"], json!(false), "{out}");
    assert_eq!(out["output"], json!([]), "{out}");
    assert_eq!(out["calls"], json!([]), "{out}");

    let base = probe(&handle, "call-2", "base_class_executes").await;
    assert_eq!(base["value"], json!("base"), "{base}");

    let inline = probe(&handle, "call-3", "inline_executes").await;
    assert_eq!(inline["value"], json!(2), "{inline}");
}

#[tokio::test(flavor = "current_thread")]
async fn sandbox_execute_returns_the_upstream_result_shape() {
    let (handle, _dir) = probe_handle("shape").await;

    let out = probe(&handle, "call-1", "execute_output").await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["value"], json!({ "n": 7 }), "{out}");
    assert_eq!(
        out["output"],
        json!([
            { "type": "text", "text": "first" },
            { "type": "text", "text": "second 2", "console": true },
        ]),
        "{out}"
    );
    assert_eq!(
        out["storeWrites"],
        json!({ "set": {}, "delete": [] }),
        "{out}"
    );

    // `exit()` completes with `value: undefined`; an explicit `return null`
    // stays `null` (upstream distinguishes them).
    let values = probe(&handle, "call-2", "execute_values").await;
    assert_eq!(
        values["exited"]["valueIsUndefined"],
        json!(true),
        "{values}"
    );
    assert_eq!(
        values["nulled"]["valueIsUndefined"],
        json!(false),
        "{values}"
    );
    assert_eq!(values["nulled"]["value"], json!(null), "{values}");
    assert_eq!(
        values["object"]["value"],
        json!({ "a": [1, 2] }),
        "{values}"
    );

    // Script-visible store (upstream `store()`/`load()` + `storeWrites`).
    let store = probe(&handle, "call-3", "execute_store").await;
    assert_eq!(
        store["value"],
        json!({ "written": 1, "seeded": 5 }),
        "{store}"
    );
    assert_eq!(
        store["storeWrites"],
        json!({ "set": { "k": { "a": 1 } }, "delete": [] }),
        "{store}"
    );
}

// ---------------------------------------------------------------------------
// b) tool calls: JS-declared tools and the host session tool set
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sandbox_tool_calls_reach_declared_js_tools() {
    let (handle, _dir) = probe_handle("declared").await;
    let out = probe(&handle, "call-1", "execute_declared_tool").await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["value"], json!({ "sum": 5, "all": ["add"] }), "{out}");
    let calls = out["calls"].as_array().expect("calls array");
    assert_eq!(calls.len(), 1, "{out}");
    assert_eq!(calls[0]["name"], json!("add"), "{out}");
    assert_eq!(calls[0]["status"], json!("ok"), "{out}");

    // A declared global is callable as a top-level identifier (upstream
    // `globals`) and is not part of `result.calls`.
    let global = probe(&handle, "call-2", "execute_declared_global").await;
    assert_eq!(global["value"], json!("attached:{\"id\":3}"), "{global}");
    assert_eq!(global["calls"], json!([]), "{global}");
}

#[tokio::test(flavor = "current_thread")]
async fn sandbox_tool_calls_reach_the_host_session_tool_set() {
    let (handle, dir) = probe_handle("session").await;
    let _shared = bind_session_executor(&handle, &dir, "call-session");

    let out = probe(&handle, "call-session", "execute_host_tool").await;
    assert_eq!(out["ok"], json!(true), "{out}");
    // The host tool's text output is the tool call's resolved value.
    assert_eq!(out["value"], json!("host:hi"), "{out}");
    let calls = out["calls"].as_array().expect("calls array");
    assert_eq!(calls.len(), 1, "{out}");
    assert_eq!(calls[0]["name"], json!("host_echo"), "{out}");
    assert_eq!(calls[0]["status"], json!("ok"), "{out}");

    // An undeclared, unregistered name fails the call instead of reaching
    // some arbitrary tool. The text comes from the host's nested-call registry
    // bridge (`Tool execute_unknown_tool not found`, upstream
    // agent-loop.ts:720), not upstream's `Unknown tool "nope"` — the session
    // tier is pir's extension of the sandbox model and its failures are the
    // host pipeline's own.
    let unknown = probe(&handle, "call-session", "execute_unknown_tool").await;
    let message = unknown["value"].as_str().unwrap_or_default();
    assert!(!message.is_empty(), "{unknown}");
    assert_eq!(unknown["calls"][0]["status"], json!("error"), "{unknown}");
}

// ---------------------------------------------------------------------------
// c) interrupt: timeout and abort
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sandbox_timeout_and_abort_end_the_run() {
    let (handle, _dir) = probe_handle("interrupt").await;

    let timeout = probe(&handle, "call-1", "execute_timeout").await;
    assert_eq!(timeout["ok"], json!(false), "{timeout}");
    assert_eq!(timeout["error"]["kind"], json!("timeout"), "{timeout}");
    assert_eq!(
        timeout["error"]["message"],
        json!("Execution timed out after 250 ms"),
        "{timeout}"
    );

    let abort = probe(&handle, "call-2", "execute_abort").await;
    assert_eq!(abort["ok"], json!(false), "{abort}");
    assert_eq!(abort["error"]["kind"], json!("aborted"), "{abort}");
    // The caller's own abort reason wins over the host's fixed text.
    assert_eq!(
        abort["error"]["message"],
        json!("caller stopped it"),
        "{abort}"
    );
}

// ---------------------------------------------------------------------------
// `start()`: caller-answered, deterministic clock/randomness, `during`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sandbox_start_is_caller_answered_and_deterministic() {
    let (handle, _dir) = probe_handle("start").await;
    let first = probe(&handle, "call-1", "start_deterministic").await;
    assert_eq!(first["ok"], json!(true), "{first}");
    assert_eq!(
        first["hostCalls"].as_array().map(Vec::len),
        Some(2),
        "{first}"
    );
    assert_eq!(
        first["hostCalls"],
        json!([
            { "id": 1, "target": "tool", "name": "double", "args": { "n": 2 } },
            { "id": 2, "target": "tool", "name": "double", "args": { "n": 10 } },
        ]),
        "{first}"
    );
    // `Date.now()` reads `startedAt` and then each delivered `now`; the second
    // result's `now` (5002) is the clock the script finished on.
    assert_eq!(first["value"]["sum"], json!(24), "{first}");
    assert_eq!(first["value"]["clock"], json!(5002), "{first}");
    // `during` = the last result the VM was seen to consume + the issued calls.
    assert_eq!(first["during"], json!({ "seq": 2, "issued": 2 }), "{first}");
    assert_eq!(first["idles"], json!([[1, 1], [2, 2]]), "{first}");
    assert_eq!(first["finished"], json!(true), "{first}");
    let fingerprint = first["fingerprint"].as_str().unwrap_or_default();
    assert!(fingerprint.starts_with("pir-quickjs-native/"), "{first}");

    // The seed drives `Math.random()`: the same script, seed and deliveries
    // produce the same value.
    let second = probe(&handle, "call-2", "start_deterministic").await;
    assert_eq!(
        first["value"]["random"], second["value"]["random"],
        "{second}"
    );

    // Upstream's synchronous validation set (before any VM starts).
    let errors = probe(&handle, "call-3", "start_errors").await;
    assert_eq!(
        errors["startedAt"]["message"],
        json!("startedAt must be a non-negative number of milliseconds since the epoch"),
        "{errors}"
    );
    assert_eq!(
        errors["seed"]["message"],
        json!("seed must be a 32-bit unsigned integer"),
        "{errors}"
    );
    assert_eq!(
        errors["duplicateTools"]["message"],
        json!("Tool names must be unique"),
        "{errors}"
    );
    assert_eq!(
        errors["reservedGlobal"]["message"],
        json!("Invalid global name \"tools.x\""),
        "{errors}"
    );

    // Aborting a deterministic run ends it with `kind: "aborted"` (the script
    // waits on a call that is never answered).
    let aborted = probe(&handle, "call-4", "start_abort").await;
    assert_eq!(aborted["result"]["ok"], json!(false), "{aborted}");
    assert_eq!(
        aborted["result"]["error"]["kind"],
        json!("aborted"),
        "{aborted}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sandbox_close_rejects_new_runs() {
    let (handle, _dir) = probe_handle("close").await;
    let out = probe(&handle, "call-1", "close_rejects").await;
    assert_eq!(
        out["execute"]["message"],
        json!("Sandbox is closed"),
        "{out}"
    );
    assert_eq!(out["start"]["message"], json!("Sandbox is closed"), "{out}");
}

// ---------------------------------------------------------------------------
// d) the remote / wasm faces keep throwing (unified text + code)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn remote_and_wasm_faces_still_throw() {
    let (handle, _dir) = probe_handle("remote").await;
    let out = probe(&handle, "call-1", "remote_throws").await;
    const UNIFIED: &str =
        "is not supported by the pir extension runtime (codemode runs as a builtin tool on the host engine)";
    for key in ["remoteExecute", "remoteStart", "serve"] {
        let message = out[key]["message"].as_str().unwrap_or_default();
        assert!(message.ends_with(UNIFIED), "{key}: {out}");
        assert_eq!(out[key]["code"], json!("ERR_NOT_SUPPORTED"), "{key}: {out}");
    }
    let wasm = out["loadWasm"].as_str().unwrap_or_default();
    assert!(wasm.ends_with(UNIFIED), "{out}");
}

// ---------------------------------------------------------------------------
// e) the host->JS event queue bound: a slow consumer stalls the sandbox
// producer instead of the queue dropping call events
// ---------------------------------------------------------------------------

/// 300 concurrent calls answered by an intentionally slow consumer. The host
/// queue is bounded at 256 events (`MAX_SANDBOX_EVENTS`), so the producer has
/// to park while the consumer catches up; every call must still be answered,
/// exactly once and in issue order, and the run must report all 300 values.
#[tokio::test(flavor = "current_thread")]
async fn sandbox_call_queue_bounds_without_losing_calls() {
    let (handle, _dir) = probe_handle("many-calls").await;
    let out = probe(&handle, "call-1", "start_many_concurrent_calls").await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["callCount"], json!(300), "{out}");
    assert_eq!(out["ordered"], json!(true), "{out}");
    assert_eq!(out["value"]["n"], json!(300), "{out}");
    assert_eq!(
        out["value"]["sum"],
        json!(44850),
        "0 + 1 + ... + 299 ({out})"
    );
}
