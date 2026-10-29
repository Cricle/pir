//! `node:async_hooks` shim contract (js/async_hooks_shim.js.txt), plus the
//! nextTick-uncaught routing in js/node_call_helper.js.txt.
//!
//! Pins the semantics the shim implements against Node v24:
//! - AsyncLocalStorage is a real multi-store execution context: two stores are
//!   visible at once with their own values (the property the old single-slot
//!   approximation could not provide — diagnostics_channel's `bindStore`),
//!   nesting restores the outer value, `defaultValue` applies only when the
//!   store is absent from the frame, `exit()` hides the store explicitly
//!   (undefined, not defaultValue) and `disable()` drops it until the next
//!   `run`/`enterWith`;
//! - `createHook` validates each listener with ERR_ASYNC_CALLBACK and
//!   enable/disable chain and are idempotent;
//! - `AsyncResource` validates `type` (ERR_INVALID_ARG_TYPE), the empty name
//!   once an init hook exists (ERR_ASYNC_TYPE) and the trigger id
//!   (ERR_INVALID_ASYNC_ID), and `bind` forwards call/thisArg;
//! - `AsyncLocalStorage.bind` is `AsyncResource.bind` and validates `fn`;
//! - an enabled init hook observes exactly one `init` for a `setImmediate`;
//! - an exception escaping a `nextTick` callback reaches
//!   `process.on("uncaughtException")` and the remaining ticks still run
//!   (Node's nextTick contract; the host's promise-rejection channel is not
//!   the right surface for it).
//!
//! Fixtures are generated at runtime into per-test temp dirs (the repository
//! hygiene gate forbids tracked `.js`/`.ts` files).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-ah-shims-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(path, content).expect("write fixture");
    }
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn new_runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

/// Seed a fixture extension, load it, fire `session_start`, and read the
/// result object it published on `global`.
async fn run_fixture(name: &str, ts: &str, global: &str) -> Value {
    let dir = write_temp_fixture(name, &[]);
    let rt = new_runtime_at(&dir).await;
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

const AH_CONTRACT_TS: &str = r#"
import asyncHooks, { AsyncLocalStorage, AsyncResource, createHook, executionAsyncId, triggerAsyncId, executionAsyncResource } from "@node/async_hooks";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- the default export carries the whole surface ----------------------
    out.exports = Object.keys(asyncHooks).sort();

    // --- two stores on one call stack --------------------------------------
    const s1 = new AsyncLocalStorage();
    const s2 = new AsyncLocalStorage();
    out.empty = [s1.getStore() === undefined, s2.getStore() === undefined];
    out.twoStores = s1.run("one", () => {
      const outerS1 = s1.getStore();
      const inner = s2.run("two", () => [s1.getStore(), s2.getStore()]);
      return [outerS1, inner, s1.getStore(), s2.getStore()];
    });
    out.twoStoresAfter = [s1.getStore(), s2.getStore()];

    // Nested contexts of the same store restore the outer value.
    out.nested = s1.run("A", () => [s1.getStore(), s1.run("B", () => s1.getStore()), s1.getStore()]);

    // --- defaultValue applies only when the store is absent ----------------
    const d = new AsyncLocalStorage({ defaultValue: "DEF" });
    out.defaultValue = [
      d.getStore(),
      d.run(undefined, () => d.getStore()),
      d.run("v", () => d.getStore()),
      d.getStore(),
      d.exit(() => d.getStore()),
    ];

    // --- exit hides explicitly (undefined, not defaultValue) ---------------
    out.exit = s1.run("x", () => [s1.exit(() => s1.getStore() === undefined), s1.getStore()]);
    out.exitReturn = s1.run("x", () => s1.exit(() => 7));

    // --- disable drops the store; run/enterWith still work ----------------
    const dis = new AsyncLocalStorage({ defaultValue: "D" });
    dis.run("keep", () => {});
    dis.disable();
    out.disable = [dis.getStore(), dis.run("again", () => dis.getStore())];
    const ent = new AsyncLocalStorage();
    ent.enterWith("held");
    out.enterWith = [ent.getStore(), ent.exit(() => ent.getStore() === undefined), ent.getStore()];

    // --- constructor validation -------------------------------------------
    const badOptions: any = {};
    for (const bad of [null, 5, "x", true]) {
      try {
        new (AsyncLocalStorage as any)(bad);
        badOptions[String(bad)] = "no-throw";
      } catch (e: any) {
        badOptions[String(bad)] = { name: e.name, code: e.code, message: e.message };
      }
    }
    out.badOptions = badOptions;
    out.nameCoerced = new AsyncLocalStorage({ name: 42 }).name;
    out.nameDefault = new AsyncLocalStorage().name;

    // --- createHook: listener validation + chainable enable/disable --------
    const hookErrors: any = {};
    for (const key of ["init", "before", "after", "destroy", "promiseResolve"]) {
      for (const bad of [null, -1, 1, {}, []]) {
        try {
          createHook({ [key]: bad } as any);
          hookErrors[key + ":" + JSON.stringify(bad)] = "no-throw";
        } catch (e: any) {
          hookErrors[key] = { name: e.name, code: e.code, message: e.message, stringified: String(e).includes("ERR_ASYNC_CALLBACK") };
        }
      }
    }
    out.hookErrors = hookErrors;
    out.hookUnknownKey = (() => {
      try { createHook({ bogus: 1 } as any); return "no-throw"; } catch { return "threw"; }
    })();
    const chainHook = createHook({});
    out.hookChain = [chainHook.enable() === chainHook, chainHook.enable() === chainHook, chainHook.disable() === chainHook, chainHook.disable() === chainHook];

    // --- AsyncResource validation -----------------------------------------
    const resErrors: any = {};
    const capture = (fn: () => any) => {
      try { fn(); return "no-throw"; } catch (e: any) {
        return { name: e.name, code: e.code, message: e.message };
      }
    };
    resErrors.noArgs = capture(() => new (AsyncResource as any)());
    resErrors.numberType = capture(() => new (AsyncResource as any)(5));
    resErrors.nullType = capture(() => new (AsyncResource as any)(null));
    resErrors.negFour = capture(() => new AsyncResource("type", -4 as any));
    resErrors.pi = capture(() => new AsyncResource("type", Math.PI as any));
    resErrors.optsNegFour = capture(() => new AsyncResource("type", { triggerAsyncId: -4 } as any));
    resErrors.emptyWithoutHook = capture(() => new AsyncResource(""));
    out.resErrors = resErrors;
    const okRes = new AsyncResource("type", -1 as any);
    out.resourceIds = [okRes.asyncId() > 0, okRes.triggerAsyncId(), new AsyncResource("probe").runInAsyncScope(() => 42)];

    // --- bind -------------------------------------------------------------
    out.bind = AsyncResource.bind(function (this: any, a: number) { return [this && this.tag, a]; }, "probe", { tag: "t" })(7);
    out.bindNoThisArg = AsyncResource.bind(function () { return 1; })();
    out.bindErrors = [
      capture(() => AsyncResource.bind(5 as any)),
      capture(() => AsyncResource.bind(() => {}, { t: "x" } as any)),
      capture(() => AsyncResource.bind("x" as any)),
    ];
    out.alsBindErrors = [capture(() => (AsyncLocalStorage as any).bind(1)), capture(() => (AsyncLocalStorage as any).bind(false))];
    out.alsBind = (AsyncLocalStorage as any).bind(() => 123)();

    // --- ids / resources ---------------------------------------------------
    out.ids = [executionAsyncId(), triggerAsyncId(), typeof executionAsyncResource()];
    out.snapshot = capture(() => (AsyncLocalStorage as any).snapshot());

    // --- init emission for setImmediate ------------------------------------
    const seen: any[] = [];
    let emptyWithHook = "not-run";
    const initHook = createHook({
      init(asyncId: any, type: any, triggerAsyncId: any, resource: any) {
        seen.push([typeof asyncId, type, typeof triggerAsyncId, typeof resource]);
      },
    });
    initHook.enable();
    (globalThis as any).setImmediate(() => {});
    emptyWithHook = capture(() => new AsyncResource(""));
    initHook.disable();
    (globalThis as any).setImmediate(() => {});
    out.initSeen = seen;
    out.emptyWithHook = emptyWithHook;

    // --- nextTick throw reaches uncaughtException --------------------------
    const caught: any[] = [];
    process.on("uncaughtException", (err: any, origin: any) => {
      caught.push({ message: err.message, origin });
    });
    process.nextTick(() => { throw new Error("tick boom"); });
    process.nextTick(() => { caught.push({ ran: "second tick" }); });
    await delay(20);
    out.tickUncaught = caught;

    globalThis.__ahContract = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn async_local_storage_multi_store_and_resource_validation() {
    let v = run_fixture("ah-contract", AH_CONTRACT_TS, "__ahContract").await;

    assert_eq!(
        v["exports"],
        json!([
            "AsyncLocalStorage",
            "AsyncResource",
            "asyncWrapProviders",
            "createHook",
            "executionAsyncId",
            "executionAsyncResource",
            "triggerAsyncId"
        ]),
        "{v}"
    );

    // Two stores, one call stack: each reads its own value back.
    assert_eq!(v["empty"], json!([true, true]), "{v}");
    assert_eq!(
        v["twoStores"],
        json!(["one", ["one", "two"], "one", null]),
        "two stores must stay visible simultaneously, and the inner run must not leak: {v}"
    );
    assert_eq!(v["twoStoresAfter"], json!([null, null]), "{v}");
    assert_eq!(v["nested"], json!(["A", "B", "A"]), "{v}");
    assert_eq!(
        v["defaultValue"],
        json!(["DEF", null, "v", "DEF", null]),
        "defaultValue only when the store is absent from the frame: {v}"
    );
    assert_eq!(
        v["exit"],
        json!([true, "x"]),
        "exit hides and restores: {v}"
    );
    assert_eq!(v["exitReturn"], 7, "{v}");
    assert_eq!(
        v["disable"],
        json!(["D", "again"]),
        "disable drops to defaultValue, run re-enables: {v}"
    );
    assert_eq!(v["enterWith"], json!(["held", true, "held"]), "{v}");

    // Node v24 constructor validation.
    for key in ["null", "5", "x", "true"] {
        let e = &v["badOptions"][key];
        assert_eq!(e["code"], "ERR_INVALID_ARG_TYPE", "options {key}: {v}");
        assert_eq!(e["name"], "TypeError", "options {key}: {v}");
    }
    assert_eq!(
        v["badOptions"]["5"]["message"],
        "The \"options\" argument must be of type object. Received type number (5)",
        "{v}"
    );
    assert_eq!(v["nameCoerced"], "42", "{v}");
    assert_eq!(v["nameDefault"], "", "{v}");

    // createHook listener validation: ERR_ASYNC_CALLBACK per key.
    for key in ["init", "before", "after", "destroy", "promiseResolve"] {
        let e = &v["hookErrors"][key];
        assert_eq!(e["code"], "ERR_ASYNC_CALLBACK", "hook.{key}: {v}");
        assert_eq!(e["name"], "TypeError", "hook.{key}: {v}");
        assert_eq!(
            e["message"],
            format!("hook.{key} must be a function"),
            "{v}"
        );
        assert_eq!(
            e["stringified"], true,
            "String(err) must carry the code: {v}"
        );
    }
    assert_eq!(v["hookUnknownKey"], "no-throw", "{v}");
    assert_eq!(v["hookChain"], json!([true, true, true, true]), "{v}");

    // AsyncResource validation (the official constructor test's contract).
    assert_eq!(
        v["resErrors"]["noArgs"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["resErrors"]["numberType"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["resErrors"]["nullType"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["resErrors"]["negFour"]["code"], "ERR_INVALID_ASYNC_ID",
        "{v}"
    );
    assert_eq!(v["resErrors"]["negFour"]["name"], "RangeError", "{v}");
    assert_eq!(
        v["resErrors"]["negFour"]["message"], "Invalid triggerAsyncId value: -4",
        "{v}"
    );
    assert_eq!(v["resErrors"]["pi"]["code"], "ERR_INVALID_ASYNC_ID", "{v}");
    assert_eq!(
        v["resErrors"]["optsNegFour"]["code"], "ERR_INVALID_ASYNC_ID",
        "{v}"
    );
    assert_eq!(v["resErrors"]["emptyWithoutHook"], "no-throw", "{v}");
    assert_eq!(v["resourceIds"], json!([true, -1, 42]), "{v}");

    // bind semantics.
    assert_eq!(v["bind"], json!(["t", 7]), "{v}");
    assert_eq!(v["bindNoThisArg"], 1, "{v}");
    assert_eq!(v["bindErrors"][0]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(
        v["bindErrors"][0]["message"],
        "The \"fn\" argument must be of type function. Received type number (5)",
        "{v}"
    );
    assert_eq!(
        v["bindErrors"][1]["message"],
        "The \"type\" argument must be of type string. Received an instance of Object",
        "{v}"
    );
    assert_eq!(v["bindErrors"][2]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(v["alsBindErrors"][0]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(v["alsBindErrors"][1]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(v["alsBind"], 123, "{v}");

    assert_eq!(v["ids"], json!([0, 0, "object"]), "{v}");
    assert_eq!(
        v["snapshot"]["message"],
        "AsyncLocalStorage.snapshot is not supported by the pir extension runtime",
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn async_hooks_init_emission_and_next_tick_uncaught_routing() {
    let v = run_fixture("ah-init", AH_CONTRACT_TS, "__ahContract").await;

    // An enabled init hook observes exactly one init for a setImmediate — the
    // contract test-async-hooks-enable-disable pins — and the hook registry is
    // off again after disable().
    let seen = v["initSeen"].as_array().expect("initSeen");
    assert_eq!(seen.len(), 1, "one init for one setImmediate: {v}");
    assert_eq!(
        seen[0],
        json!(["number", "Immediate", "number", "object"]),
        "{v}"
    );

    // The empty name is only illegal while an init hook would observe it.
    assert_eq!(v["emptyWithHook"]["code"], "ERR_ASYNC_TYPE", "{v}");
    assert_eq!(v["emptyWithHook"]["name"], "TypeError", "{v}");
    assert_eq!(
        v["emptyWithHook"]["message"], "Invalid name for async \"type\": ",
        "{v}"
    );

    // nextTick: a throwing callback is an uncaught exception; the listener sees
    // it with origin "uncaughtException" and the following tick still runs.
    let caught = v["tickUncaught"].as_array().expect("tickUncaught");
    assert_eq!(caught.len(), 2, "{v}");
    assert_eq!(caught[0]["message"], "tick boom", "{v}");
    assert_eq!(caught[0]["origin"], "uncaughtException", "{v}");
    assert_eq!(caught[1]["ran"], "second tick", "{v}");
}
