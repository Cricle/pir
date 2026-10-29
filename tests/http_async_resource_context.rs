//! `executionAsyncResource()` per execution context, over the real local http
//! server, plus the timer-family continuation capture that carries it.
//!
//! Pins the two halves of the official Node test
//! `test-async-hooks-execution-async-resource.js` at the extension-runtime
//! level, where the failing shape is a *concurrent* loopback burst:
//!
//!   * the http server runs every request handler inside its own execution
//!     context — the resource a handler writes to is the one its `setTimeout`
//!     reads back, and two concurrent requests never share it;
//!   * `server.unref()` stops keeping the runtime busy while an in-process
//!     client still drives the server to completion (Node's handle semantics:
//!     the server stops holding the loop open, it does not stop serving);
//!   * `AsyncLocalStorage.run`'s value reaches a timer created inside the run
//!     (Node's continuation capture), and a timer created outside it does not
//!     inherit the store.
//!
//! Fixtures are generated at runtime into per-test temp dirs (repository
//! hygiene gate; same pattern as `tests/http_shims.rs`).

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-http-ahres-{}-{test_name}", std::process::id()));
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
    if global.is_empty() {
        return Value::Null;
    }
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

/// The official test's mechanism: the handler parks its own state on the
/// execution resource and reads it back inside a `setTimeout`, for ten
/// concurrent requests, against an `unref()`ed server.
#[tokio::test(flavor = "current_thread")]
async fn concurrent_handlers_read_their_own_execution_resource() {
    let ts = r#"
import http from "node:http";
import { executionAsyncResource } from "node:async_hooks";

const sym = Symbol("cls");

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { resources: [] as any[] };
    const root = executionAsyncResource();
    out.rootStable = executionAsyncResource() === root;

    const server = http.createServer((req: any, res: any) => {
      const resource = executionAsyncResource();
      out.resources.push(resource);
      resource[sym] = { state: req.url };
      setTimeout(() => {
        const carried = executionAsyncResource()[sym];
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify({ state: carried ? carried.state : null }));
      }, 5);
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    // Node semantics under test: an unref'd server does not hold the loop
    // open, but an in-process client still gets served.
    server.unref();
    const port = server.address().port;

    const bodies = await Promise.all(
      [0, 1, 2, 3, 4, 5, 6, 7, 8, 9].map((n: number) => new Promise((resolve: any, reject: any) => {
        const req = http.get("http://127.0.0.1:" + port + "/" + n, (res: any) => {
          let body = "";
          res.setEncoding("utf8");
          res.on("data", (chunk: any) => { body += chunk; });
          res.on("end", () => resolve(body));
        });
        req.on("error", reject);
      }))
    );
    server.close();

    out.bodies = bodies;
    out.resourceCount = out.resources.length;
    out.noneIsRoot = out.resources.every((resource: any) => resource !== root);
    out.allDistinct = out.resources.every(
      (resource: any, index: number) => out.resources.indexOf(resource) === index
    );
    globalThis.__pirAhRes = out;
  });
};
"#;
    let dir = write_temp_fixture("concurrent-resource", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "concurrent-resource", ts, "__pirAhRes").await;

    assert_eq!(v["rootStable"], true, "root resource is stable: {v}");
    assert_eq!(
        v["bodies"],
        json!([
            "{\"state\":\"/0\"}",
            "{\"state\":\"/1\"}",
            "{\"state\":\"/2\"}",
            "{\"state\":\"/3\"}",
            "{\"state\":\"/4\"}",
            "{\"state\":\"/5\"}",
            "{\"state\":\"/6\"}",
            "{\"state\":\"/7\"}",
            "{\"state\":\"/8\"}",
            "{\"state\":\"/9\"}"
        ]),
        "every response carries its own request's state: {v}"
    );
    assert_eq!(v["resourceCount"], 10, "one dispatch per request: {v}");
    assert_eq!(
        v["noneIsRoot"], true,
        "a request handler's resource is not the root resource: {v}"
    );
    assert_eq!(
        v["allDistinct"], true,
        "concurrent dispatch contexts never share a resource: {v}"
    );
}

/// Continuation capture for the timer family: an `AsyncLocalStorage` value set
/// by `run` reaches a `setTimeout` created inside the run (Node captures the
/// frame into the timer resource), and a timer created outside the run does
/// not inherit the store.
#[tokio::test(flavor = "current_thread")]
async fn timer_continuations_capture_the_async_context() {
    let ts = r#"
import { AsyncLocalStorage } from "node:async_hooks";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const store = new AsyncLocalStorage();

    const inside: any[] = [];
    const outside: any[] = [];
    setTimeout(() => { outside.push(store.getStore() === undefined ? "no-store" : store.getStore()); }, 1);
    store.run("A", () => {
      setTimeout(() => { inside.push(store.getStore()); }, 1);
    });
    await delay(30);

    out.inside = inside;
    out.outside = outside;
    // The store is back to absent once `run` returned.
    out.after = store.getStore() === undefined;
    globalThis.__pirAhTimerCtx = out;
  });
};
"#;
    let dir = write_temp_fixture("timer-context", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "timer-context", ts, "__pirAhTimerCtx").await;

    assert_eq!(
        v["inside"],
        json!(["A"]),
        "a timer created inside run() reads the store back: {v}"
    );
    assert_eq!(
        v["outside"],
        json!(["no-store"]),
        "a timer created outside run() must not inherit the store: {v}"
    );
    assert_eq!(v["after"], true, "run() restores the prior context: {v}");
}
