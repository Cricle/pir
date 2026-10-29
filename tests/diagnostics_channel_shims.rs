//! `node:diagnostics_channel` shim contract (js/diagnostics_channel_shim.js.txt).
//!
//! Pins the semantics the shim implements against Node v24:
//! - the registry is keyed by the name value itself (symbols keep identity and
//!   reach subscribers as the second argument);
//! - a name that is neither a string nor a symbol is rejected with
//!   ERR_INVALID_ARG_TYPE (and `String(err)` carries the code, as Node's
//!   internal errors do);
//! - `subscribe` validates the subscription and appends unconditionally;
//! - `publish` isolates subscriber errors: they never escape the publisher,
//!   they are deferred to an uncaught exception, and later subscribers still
//!   run;
//! - `runStores` applies each bound store's transform, runs the callback inside
//!   the store, restores the previous value afterwards, and contains transform
//!   errors the way Node's `wrapStoreRun` does (deferred error, run continues
//!   without entering that store).
//!
//! Deliberately not covered: two stores bound to one channel observing their
//! own value inside a single run. That needs per-store AsyncLocalStorage
//! contexts, and the `node:async_hooks` shim is a documented single-slot
//! approximation (later wave) — see scripts/node_tests/REPORT.md.
//!
//! Fixtures are generated at runtime into per-test temp dirs (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-dc-shims-{}-{test_name}", std::process::id()));
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

const DC_CONTRACT_TS: &str = r#"
import dc, { Channel } from "@node/diagnostics_channel";
import { AsyncLocalStorage } from "@node/async_hooks";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- registry identity: strings by value, symbols by identity ---------
    const symbol = Symbol("test");
    const symChannel = dc.channel(symbol);
    out.symbolNameIsSymbol = symChannel.name === symbol;
    out.symbolChannelInstance = symChannel instanceof Channel;
    let symSeen: any = null;
    symChannel.subscribe((message: any, name: any) => {
      symSeen = { same: name === symbol, payload: message.foo };
    });
    symChannel.publish({ foo: "bar" });
    out.symbolSubscriber = symSeen;

    const a1 = dc.channel("same");
    const a2 = dc.channel("same");
    out.sameChannelObject = a1 === a2;
    out.hasSubscribersUnknown = dc.hasSubscribers("never-created");
    out.hasSubscribersNullName = dc.hasSubscribers(null);

    // --- name validation --------------------------------------------------
    const nameErrors: any = {};
    for (const bad of [null, undefined, 1, {}, true]) {
      try {
        dc.channel(bad as any);
        nameErrors[String(bad)] = "no-throw";
      } catch (e: any) {
        nameErrors[String(bad)] = {
          name: e.name,
          code: e.code,
          stringified: String(e).includes("ERR_INVALID_ARG_TYPE"),
          message: e.message,
        };
      }
    }
    out.nameErrors = nameErrors;

    // --- subscription validation and duplicate semantics -------------------
    const subChannel = dc.channel("subs");
    const subErrors: any = {};
    for (const bad of [null, 3, "nope"]) {
      try {
        subChannel.subscribe(bad as any);
        subErrors[String(bad)] = "no-throw";
      } catch (e: any) {
        subErrors[String(bad)] = { name: e.name, code: e.code };
      }
    }
    try {
      dc.subscribe("subs", null as any);
      subErrors.viaModule = "no-throw";
    } catch (e: any) {
      subErrors.viaModule = { name: e.name, code: e.code };
    }
    out.subscriptionErrors = subErrors;

    let duplicateCalls = 0;
    let duplicateSub: any;
    const dupChannel = dc.channel("dup");
    duplicateSub = () => { duplicateCalls++; };
    dupChannel.subscribe(duplicateSub);
    dupChannel.subscribe(duplicateSub);
    out.hasSubscribersAfterDuplicate = dupChannel.hasSubscribers;
    dupChannel.publish({});
    out.duplicateCalls = duplicateCalls;

    // --- unsubscribe ------------------------------------------------------
    const unsubChannel = dc.channel("unsub");
    const unsubTarget = () => {};
    out.unsubscribeNotSubscribed = unsubChannel.unsubscribe(unsubTarget);
    unsubChannel.subscribe(unsubTarget);
    out.unsubscribeSubscribed = unsubChannel.unsubscribe(unsubTarget);
    out.unsubscribeTwice = unsubChannel.unsubscribe(unsubTarget);
    out.unsubscribeUnknownChannel = dc.unsubscribe("unsub-never-created", unsubTarget);

    // --- publish: error isolation + deferred uncaught exception ------------
    const observed: any = [];
    process.on("uncaughtException", (err: any) => {
      observed.push({ caught: true, message: err.message, sameObject: err === publishError });
    });
    const publishError = new Error("subscriber boom");
    const errChannel = dc.channel("publish-errors");
    errChannel.subscribe((message: any) => {
      observed.push({ first: message.foo });
      throw publishError;
    });
    errChannel.subscribe((message: any) => { observed.push({ second: message.foo }); });
    let publishThrew = "no-throw";
    try {
      errChannel.publish({ foo: "payload" });
    } catch (e: any) {
      publishThrew = e.message;
    }
    out.publishThrew = publishThrew;
    await delay(20);
    // Snapshot: the later runStores phase emits another uncaught exception.
    out.publishObserved = observed.slice();

    // --- runStores: transform, nesting, restore, unbind --------------------
    const store = new AsyncLocalStorage();
    const storeChannel = dc.channel("stores");
    storeChannel.bindStore(store, (data: any) => ({ data }));
    const thisArg = { tag: "this" };
    const runLog: any = {};
    storeChannel.subscribe((data: any) => { runLog.subscriber = data.foo; });
    storeChannel.runStores({ foo: "one" }, function (a: number, b: number) {
      runLog.thisMatches = this === thisArg;
      runLog.args = [a, b];
      runLog.storeInside = store.getStore();
    }, thisArg, 1, 2);
    // Copy the fields now: the unbind phase publishes into the same log again.
    out.runStores = {
      thisMatches: runLog.thisMatches,
      args: runLog.args,
      storeInside: runLog.storeInside,
      subscriber: runLog.subscriber,
      storeAfterUndefined: store.getStore() === undefined,
    };

    out.unbindFirst = storeChannel.unbindStore(store);
    out.unbindSecond = storeChannel.unbindStore(store);
    let ranAfterUnbind = false;
    storeChannel.runStores({}, () => { ranAfterUnbind = true; });
    out.ranAfterUnbind = ranAfterUnbind;
    out.storeAfterUnbindUndefined = store.getStore() === undefined;

    // --- runStores: transform error is deferred, the run continues ---------
    const fail = new Error("transform boom");
    const failing = dc.channel("failing-transform");
    failing.bindStore(store, () => { throw fail; });
    const deferred: any = [];
    process.on("uncaughtException", (err: any) => {
      deferred.push({ message: err.message, sameObject: err === fail });
    });
    let callbackRan = false;
    let subscriberRan = false;
    failing.subscribe(() => { subscriberRan = true; });
    let runThrew = "no-throw";
    try {
      failing.runStores({}, () => { callbackRan = true; });
    } catch (e: any) {
      runThrew = e.message;
    }
    out.transformErrorRunThrew = runThrew;
    out.transformErrorCallbackRan = callbackRan;
    out.transformErrorSubscriberRan = subscriberRan;
    await delay(20);
    out.transformDeferred = deferred;

    globalThis.__dcContract = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn diagnostics_channel_registry_validation_and_publish_isolation() {
    let v = run_fixture("dc-contract", DC_CONTRACT_TS, "__dcContract").await;

    // Name identity.
    assert_eq!(v["symbolNameIsSymbol"], true, "{v}");
    assert_eq!(v["symbolChannelInstance"], true, "{v}");
    assert_eq!(v["symbolSubscriber"]["same"], true, "{v}");
    assert_eq!(v["symbolSubscriber"]["payload"], "bar", "{v}");
    assert_eq!(v["sameChannelObject"], true, "{v}");
    assert_eq!(v["hasSubscribersUnknown"], false, "{v}");
    assert_eq!(
        v["hasSubscribersNullName"], false,
        "hasSubscribers must not throw: {v}"
    );

    // ERR_INVALID_ARG_TYPE for every non string/symbol name, code carried both
    // as a property and in the stringified form Node's internal errors use.
    for key in ["null", "undefined", "1", "[object Object]", "true"] {
        let e = &v["nameErrors"][key];
        assert_eq!(e["code"], "ERR_INVALID_ARG_TYPE", "channel({key}): {v}");
        assert_eq!(e["name"], "TypeError", "channel({key}): {v}");
        assert_eq!(
            e["stringified"], true,
            "String(err) must carry the code: {v}"
        );
    }
    assert_eq!(
        v["nameErrors"]["null"]["message"],
        "The \"channel\" argument must be one of type string or symbol. Received null",
        "{v}"
    );

    // Subscription validation.
    for key in ["null", "3", "nope"] {
        assert_eq!(
            v["subscriptionErrors"][key]["code"], "ERR_INVALID_ARG_TYPE",
            "{v}"
        );
        assert_eq!(v["subscriptionErrors"][key]["name"], "TypeError", "{v}");
    }
    assert_eq!(
        v["subscriptionErrors"]["viaModule"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );

    // Node appends duplicates instead of de-duplicating.
    assert_eq!(v["hasSubscribersAfterDuplicate"], true, "{v}");
    assert_eq!(v["duplicateCalls"], 2, "{v}");

    // Unsubscribe bookkeeping.
    assert_eq!(v["unsubscribeNotSubscribed"], false, "{v}");
    assert_eq!(v["unsubscribeSubscribed"], true, "{v}");
    assert_eq!(v["unsubscribeTwice"], false, "{v}");
    assert_eq!(v["unsubscribeUnknownChannel"], false, "{v}");

    // Publish isolation: no escape, later subscribers still run, the error is
    // reported as an uncaught exception with the original object.
    assert_eq!(v["publishThrew"], "no-throw", "{v}");
    let observed = v["publishObserved"].as_array().expect("publishObserved");
    assert_eq!(observed.len(), 3, "first + second + uncaught: {v}");
    assert_eq!(observed[0]["first"], "payload", "{v}");
    assert_eq!(observed[1]["second"], "payload", "{v}");
    assert_eq!(observed[2]["caught"], true, "{v}");
    assert_eq!(observed[2]["message"], "subscriber boom", "{v}");
    assert_eq!(observed[2]["sameObject"], true, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn diagnostics_channel_run_stores_transform_and_error_containment() {
    let v = run_fixture("dc-run-stores", DC_CONTRACT_TS, "__dcContract").await;

    // Transform applied, callback this/args forwarded, value restored after.
    let run = &v["runStores"];
    assert_eq!(run["thisMatches"], true, "{v}");
    assert_eq!(run["args"], json!([1, 2]), "{v}");
    assert_eq!(
        run["storeInside"],
        json!({ "data": { "foo": "one" } }),
        "{v}"
    );
    assert_eq!(
        run["subscriber"], "one",
        "publish runs before the callback: {v}"
    );
    assert_eq!(
        run["storeAfterUndefined"], true,
        "store must be restored after runStores: {v}"
    );

    // unbindStore reports removal and stops entering the store.
    assert_eq!(v["unbindFirst"], true, "{v}");
    assert_eq!(v["unbindSecond"], false, "{v}");
    assert_eq!(v["ranAfterUnbind"], true, "{v}");
    assert_eq!(v["storeAfterUnbindUndefined"], true, "{v}");

    // A throwing transform never escapes runStores: it is deferred to an
    // uncaught exception and the run continues without entering the store.
    assert_eq!(v["transformErrorRunThrew"], "no-throw", "{v}");
    assert_eq!(v["transformErrorCallbackRan"], true, "{v}");
    assert_eq!(v["transformErrorSubscriberRan"], true, "{v}");
    let deferred = v["transformDeferred"]
        .as_array()
        .expect("transformDeferred");
    assert_eq!(deferred.len(), 1, "{v}");
    assert_eq!(deferred[0]["message"], "transform boom", "{v}");
    assert_eq!(deferred[0]["sameObject"], true, "{v}");
}
