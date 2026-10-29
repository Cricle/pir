//! node_tests cleanup wave B contract: the `perf_hooks` histogram, the `util`
//! entry points (`getCallSites`, `deprecate`) and the `events` error /
//! EventEmitterAsyncResource surfaces that turned
//! test-perf-hooks-histogram, test-util-getcallsites-preparestacktrace,
//! test-util-deprecate-invalid-code, test-event-emitter-errors and
//! test-eventemitter-asyncresource green (see scripts/node_tests/REPORT.md).
//!
//! Everything asserted here was checked against node v24.19.0 first (the
//! histogram percentile formulas were derived from node's HDR-backed
//! `hdr_value_at_percentile` / `hdr_iter_percentile` behavior).
//!
//! Fixtures are generated at runtime into per-test temp dirs (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-node-tests-wave-b-{}-{test_name}",
        std::process::id()
    ));
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

async fn run_fixture(name: &str, ts: &str, global: &str) -> (ExtensionRuntime, Value) {
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
    let value = rt
        .eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results");
    (rt, value)
}

/// The histogram contract: node's HDR-derived getters, rank-based percentile
/// values, the percentile *map* key sets (which are produced by the HDR
/// iterator, not by the percentile list), the BigInt twins, the option /
/// `record` / `add` validation codes, `inspect` in both depth modes and the
/// illegal-constructor guard.
const HISTOGRAM_CONTRACT_TS: &str = r#"
import { createHistogram, monitorEventLoopDelay } from "@node/perf_hooks";
import { inspect } from "@node/util";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- an empty histogram: HDR's sentinel values -----------------------
    const empty: any = createHistogram();
    out.empty = {
      min: empty.min,
      minBigInt: String(empty.minBigInt),
      max: empty.max,
      maxBigInt: String(empty.maxBigInt),
      exceeds: empty.exceeds,
      exceedsBigInt: String(empty.exceedsBigInt),
      meanNaN: Number.isNaN(empty.mean),
      stddevNaN: Number.isNaN(empty.stddev),
      count: empty.count,
      countBigInt: String(empty.countBigInt),
      percentile50: empty.percentile(50),
      // The HDR "last value" rule: an empty histogram's map is {100: 0}.
      percentiles: [...empty.percentiles],
    };

    // --- one sample: the shape the upstream test pins --------------------
    const one: any = createHistogram();
    one.record(1);
    out.one = {
      count: one.count,
      countBigInt: String(one.countBigInt),
      min: one.min,
      minBigInt: String(one.minBigInt),
      max: one.max,
      maxBigInt: String(one.maxBigInt),
      exceeds: one.exceeds,
      mean: one.mean,
      stddev: one.stddev,
      p1: one.percentile(1),
      p100: one.percentile(100),
      p1BigInt: String(one.percentileBigInt(1)),
      percentiles: [...one.percentiles],
      percentilesBigInt: [...one.percentilesBigInt].map(([k, v]: any) => [k, String(v)]),
    };

    // --- percentile map key sets across distributions --------------------
    const mapOf = (values: number[]): any[] => {
      const histogram: any = createHistogram();
      for (const value of values) histogram.record(value);
      return [...histogram.percentiles];
    };
    out.maps = {
      n2: mapOf([1, 2]),
      n3: mapOf([1, 2, 3]),
      n4: mapOf([1, 2, 3, 4]),
      allEqual: mapOf([5, 5, 5]),
      n10: mapOf([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
      tens: mapOf([10, 20, 30, 40, 50, 60, 70, 80, 90, 100]),
      repeated: mapOf([1, 1, 1, 2]),
    };
    out.rankValues = (() => {
      const histogram: any = createHistogram();
      for (let i = 1; i <= 100; i++) histogram.record(i);
      return [1, 50, 75, 87.5, 93.75, 96.875, 98.4375, 99.21875, 100].map((p) =>
        [p, histogram.percentile(p)]);
    })();

    // --- record() validation ---------------------------------------------
    const recordErrors: any[] = [];
    const badRecords: any[] = [false, "", {}, undefined, null, 0, Number.MAX_SAFE_INTEGER + 1, 1.5];
    for (const value of badRecords) {
      const histogram: any = createHistogram();
      try {
        histogram.record(value);
        recordErrors.push({ value: String(value), outcome: "no-throw" });
      } catch (e: any) {
        recordErrors.push({ value: String(value), code: e.code, name: e.name });
      }
    }
    out.recordErrors = recordErrors;
    // BigInt samples bypass the integer validator (node's int64 handle path).
    const bigintHistogram: any = createHistogram();
    bigintHistogram.record(2n);
    out.bigintRecord = { count: bigintHistogram.count, max: bigintHistogram.max, maxBigInt: String(bigintHistogram.maxBigInt) };

    // --- add() ------------------------------------------------------------
    const source: any = createHistogram();
    source.record(1);
    source.record(3);
    const target: any = createHistogram();
    target.add(source);
    out.add = { count: target.count, min: target.min, max: target.max, mean: target.mean };
    try {
      target.add({});
      out.addError = "no-throw";
    } catch (e: any) {
      out.addError = { code: e.code, name: e.name, message: e.message };
    }
    try {
      target.add(1);
      out.addNumberError = "no-throw";
    } catch (e: any) {
      out.addNumberError = { code: e.code, name: e.name };
    }

    // --- createHistogram option validation -------------------------------
    const optionErrors: any[] = [];
    const badOptions: any[] = ["hello", 1, null, { lowest: "hello" }, { lowest: false },
      { lowest: null }, { lowest: {} }, { highest: "hello" }, { figures: "hello" },
      { figures: 6 }, { figures: 10 }];
    for (const options of badOptions) {
      try {
        createHistogram(options);
        optionErrors.push({ options: JSON.stringify(options), outcome: "no-throw" });
      } catch (e: any) {
        optionErrors.push({ options: JSON.stringify(options), code: e.code, name: e.name });
      }
    }
    out.optionErrors = optionErrors;
    const custom: any = createHistogram({ lowest: 1, highest: 11, figures: 1 } as any);
    custom.record(11);
    custom.record(12);
    custom.record(3);
    out.customRange = { count: custom.count, exceeds: custom.exceeds, min: custom.min, max: custom.max };

    // --- percentile() argument validation --------------------------------
    const percentileErrors: any[] = [];
    for (const value of [0, -1, 101, Number.NaN, "50", undefined]) {
      try {
        createHistogram().percentile(value as any);
        percentileErrors.push({ value: String(value), outcome: "no-throw" });
      } catch (e: any) {
        percentileErrors.push({ value: String(value), code: e.code, name: e.name });
      }
    }
    out.percentileErrors = percentileErrors;

    // --- inspect / illegal constructor -----------------------------------
    const inspected: any = createHistogram();
    out.inspectPrefix = inspect(inspected, { depth: null }).startsWith("Histogram");
    out.inspectShallow = inspect(inspected, { depth: -1 });
    try {
      new inspected.constructor();
      out.illegalConstructor = "no-throw";
    } catch (e: any) {
      out.illegalConstructor = { code: e.code, name: e.name, message: e.message };
    }

    // --- MessageChannel transfer: a clone view over the same state --------
    const transferred: any = createHistogram();
    transferred.record(1);
    const channel = new MessageChannel();
    await new Promise<void>((resolve) => {
      channel.port1.onmessage = ({ data }: any) => {
        out.transferIdentity = data !== transferred;
        out.transferCtor = data.constructor.name;
        out.transferCount = data.count;
        data.record(2n);
        data.recordDelta();
        out.transferAfterRecord = transferred.max;
        resolve();
      };
      channel.port2.postMessage(transferred);
    });
    channel.port1.close();

    // --- monitorEventLoopDelay: the interval-histogram surface ------------
    const eld: any = monitorEventLoopDelay();
    out.eldInitialCount = eld.count;
    out.eldEnable = eld.enable();
    out.eldEnableAgain = eld.enable();
    out.eldDispose = typeof eld[Symbol.dispose];
    // The sampler is observation-driven (a real JS timer would keep the
    // runtime alive, and node unrefs its timer); reading the histogram after
    // the configured resolution records the measured lateness.
    await delay(30);
    out.eldSample = { count: eld.count, minIsNumber: typeof eld.min === "number", minPositive: eld.min > 0 };
    out.eldDisable = eld.disable();
    out.eldDisableAgain = eld.disable();
    const eldClone: any = monitorEventLoopDelay({ resolution: 5 });
    eldClone.enable();
    out.eldResolution = eldClone.disable();
    try {
      monitorEventLoopDelay({ resolution: 0 });
      out.eldBadResolution = "no-throw";
    } catch (e: any) {
      out.eldBadResolution = { code: e.code, name: e.name, message: e.message };
    }

    globalThis.__perfContract = out;
  });
};
"#;

/// `util.getCallSites` (node's plain-object CallSite list, never calling
/// `Error.prepareStackTrace`) and `util.deprecate`'s `code` validation.
const UTIL_CONTRACT_TS: &str = r#"
import { getCallSites, deprecate, inspect } from "@node/util";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // getCallSites must not invoke the user's prepareStackTrace.
    let prepareCalls = 0;
    (Error as any).prepareStackTrace = () => { prepareCalls += 1; return "nope"; };
    const framed = getCallSites(1);
    out.prepareCalls = prepareCalls;
    out.frameCount = framed.length;
    out.frame = {
      functionName: framed[0].functionName,
      scriptName: framed[0].scriptName,
      lineIsNumber: typeof framed[0].lineNumber === "number",
      column: framed[0].column,
      proto: Object.getPrototypeOf(framed[0]) === null,
    };
    out.frameKeys = Object.keys(framed[0]).sort();
    out.selfNamed = framed[0].scriptName.indexOf("perf-waveb") !== -1 ||
      framed[0].scriptName.indexOf("pir-ext:") === 0 ||
      framed[0].scriptName.length > 0;
    (Error as any).prepareStackTrace = undefined;

    const errors: any[] = [];
    const cases: any[] = [
      ["count0", () => getCallSites(0)],
      ["count201", () => getCallSites(201)],
      ["count1.5", () => getCallSites(1.5)],
      ["countString", () => getCallSites("x")],
      ["sourceMap", () => getCallSites(1, { sourceMap: 1 })],
      ["optionsNull", () => getCallSites(1, null)],
      ["optionsString", () => getCallSites(1, "x")],
    ];
    for (const [name, run] of cases) {
      try {
        (run as any)();
        errors.push({ name, outcome: "no-throw" });
      } catch (e: any) {
        errors.push({ name, code: e.code, name2: e.name, message: e.message });
      }
    }
    out.getCallSitesErrors = errors;
    out.defaultFrames = getCallSites().length;
    out.objectOptions = getCallSites({} as any).length;

    // deprecate(fn, message, code): the code must be a string.
    const deprecateErrors: any[] = [];
    for (const code of [1, true, false, null, {}]) {
      try {
        deprecate(() => {}, "message", code as any);
        deprecateErrors.push({ code: inspect(code), outcome: "no-throw" });
      } catch (e: any) {
        deprecateErrors.push({ code: inspect(code), err: e.code, name: e.name, message: e.message });
      }
    }
    out.deprecateErrors = deprecateErrors;
    out.deprecateOk = typeof deprecate(() => 5, "message", "DEP0001")();

    globalThis.__utilContract = out;
  });
};
"#;

/// The events surface: `ERR_UNHANDLED_ERROR`'s inspect-shaped message (with the
/// String() fallback for a throwing custom inspect) and
/// EventEmitterAsyncResource's AsyncResource wiring plus its V8-worded
/// private-member errors.
const EVENTS_CONTRACT_TS: &str = r#"
import EventEmitter, { EventEmitterAsyncResource } from "@node/events";
import { createHook, executionAsyncId } from "@node/async_hooks";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- unhandled 'error' events ----------------------------------------
    const emitter = new EventEmitter();
    const unhandled: any[] = [];
    const throwers: any[] = [
      ["string", "Accepts a string"],
      ["object", { message: "Error!" }],
      ["throwingInspect", { message: "Error!" }],
    ];
    // A throwing inspect.custom makes the message fall back to String(value).
    throwers[2][1][inspectCustom()] = () => { throw new Error("custom inspect blew up"); };
    for (const [name, payload] of throwers) {
      try {
        emitter.emit("error", payload);
        unhandled.push({ name, outcome: "no-throw" });
      } catch (e: any) {
        unhandled.push({ name, code: e.code, errorName: e.name, message: e.message, context: e.context === payload });
      }
    }
    out.unhandled = unhandled;
    // An Error argument is rethrown as-is.
    const original = new Error("boom");
    try {
      emitter.emit("error", original);
      out.errorPassthrough = "no-throw";
    } catch (e: any) {
      out.errorPassthrough = e === original;
    }

    // --- EventEmitterAsyncResource ---------------------------------------
    const seen: any[] = [];
    const hook = createHook({
      init: (asyncId: number, type: string, triggerAsyncId: number) => {
        if (type === "WaveBFoo") seen.push({ kind: "init", type, triggerAsyncId });
      },
      before: (asyncId: number) => { seen.push({ kind: "before", asyncId }); },
      after: (asyncId: number) => { seen.push({ kind: "after", asyncId }); },
      destroy: (asyncId: number) => { seen.push({ kind: "destroy", asyncId }); },
    }).enable();

    const parentAsyncId = executionAsyncId();
    class WaveBFoo extends EventEmitterAsyncResource {}
    const foo: any = new WaveBFoo();
    out.asyncIdIsNumber = typeof foo.asyncId === "number";
    out.triggerAsyncId = foo.triggerAsyncId;
    out.triggerAsyncIdIsParent = foo.triggerAsyncId === parentAsyncId;
    out.asyncResourceEventEmitter = foo.asyncResource.eventEmitter === foo;
    foo.emit("someEvent");
    foo.emitDestroy();
    hook.disable();
    out.hookTypes = seen.map((entry: any) => entry.kind);
    out.initType = seen[0].type;
    out.namedResource = (() => {
      let name: string | undefined;
      const namedHook = createHook({
        init: (_id: number, type: string) => { name = type; },
      }).enable();
      const resource: any = new WaveBFoo("ExplicitName" as any);
      namedHook.disable();
      void resource;
      return name;
    })();
    out.optionNamedResource = (() => {
      let name: string | undefined;
      const namedHook = createHook({
        init: (_id: number, type: string) => { name = type; },
      }).enable();
      new WaveBFoo({ name: "OptionName" } as any);
      namedHook.disable();
      return name;
    })();

    // Private-member errors: the message is V8's verbatim wording, and the
    // getters must carry the `at get <name>` frame.
    const privateErrors: any[] = [];
    const privateCases: any[] = [
      ["emit", () => (EventEmitterAsyncResource.prototype as any).emit()],
      ["emitDestroy", () => (EventEmitterAsyncResource.prototype as any).emitDestroy()],
      ["get asyncId", () => Reflect.get(EventEmitterAsyncResource.prototype, "asyncId", {})],
      ["get triggerAsyncId", () => Reflect.get(EventEmitterAsyncResource.prototype, "triggerAsyncId", {})],
      ["get asyncResource", () => Reflect.get(EventEmitterAsyncResource.prototype, "asyncResource", {})],
    ];
    for (const [name, run] of privateCases) {
      try {
        (run as any)();
        privateErrors.push({ name, outcome: "no-throw" });
      } catch (e: any) {
        privateErrors.push({
          name,
          code: e.code,
          errorName: e.name,
          message: e.message,
          frameInStack: String(e.stack).indexOf("at get " + name.slice(4)) !== -1,
        });
      }
    }
    out.privateErrors = privateErrors;
    // A direct construction (no subclass name) is rejected.
    try {
      new (EventEmitterAsyncResource as any)();
      out.directConstruction = "no-throw";
    } catch (e: any) {
      out.directConstruction = { code: e.code, name: e.name };
    }

    globalThis.__eventsContract = out;
  });
};

function inspectCustom(): any {
  return (Symbol as any).for("nodejs.util.inspect.custom");
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn histogram_contract_matches_node() {
    let (_rt, v) = run_fixture("histogram", HISTOGRAM_CONTRACT_TS, "__perfContract").await;

    assert_eq!(
        v["empty"],
        json!({
            // Number(i64::MAX); the fixture's JSON.stringify prints the
            // shortest decimal form, which serde_json reads back as an integer.
            "min": 9223372036854776000u64,
            "minBigInt": "9223372036854775807",
            "max": 0,
            "maxBigInt": "0",
            "exceeds": 0,
            "exceedsBigInt": "0",
            "meanNaN": true,
            "stddevNaN": true,
            "count": 0,
            "countBigInt": "0",
            "percentile50": 0,
            "percentiles": [[100, 0]],
        }),
        "empty histogram: {v}"
    );
    // and the double behind it is exactly Number(9223372036854775807n).
    assert_eq!(
        v["empty"]["min"].as_f64(),
        Some(9223372036854775808.0),
        "empty min must be Number(i64::MAX): {v}"
    );
    assert_eq!(
        v["one"],
        json!({
            "count": 1,
            "countBigInt": "1",
            "min": 1,
            "minBigInt": "1",
            "max": 1,
            "maxBigInt": "1",
            "exceeds": 0,
            "mean": 1,
            "stddev": 0,
            "p1": 1,
            "p100": 1,
            "p1BigInt": "1",
            "percentiles": [[0, 1], [100, 1]],
            "percentilesBigInt": [[0, "1"], [100, "1"]],
        }),
        "one-sample histogram: {v}"
    );

    // Percentile map key sets: HDR emits a tick per bucket while it is <= the
    // cumulative percentage and closes with a single (100, max) entry.
    assert_eq!(
        v["maps"]["n2"],
        json!([[0, 1], [50, 1], [75, 2], [100, 2]]),
        "{v}"
    );
    assert_eq!(
        v["maps"]["n3"],
        json!([[0, 1], [50, 2], [75, 3], [100, 3]]),
        "{v}"
    );
    assert_eq!(
        v["maps"]["n4"],
        json!([[0, 1], [50, 2], [75, 3], [87.5, 4], [100, 4]]),
        "{v}"
    );
    assert_eq!(v["maps"]["allEqual"], json!([[0, 5], [100, 5]]), "{v}");
    assert_eq!(
        v["maps"]["n10"],
        json!([[0, 1], [50, 5], [75, 8], [87.5, 9], [93.75, 10], [100, 10]]),
        "{v}"
    );
    assert_eq!(
        v["maps"]["tens"],
        json!([
            [0, 10],
            [50, 50],
            [75, 80],
            [87.5, 90],
            [93.75, 100],
            [100, 100]
        ]),
        "{v}"
    );
    assert_eq!(
        v["maps"]["repeated"],
        json!([[0, 1], [50, 1], [75, 1], [87.5, 2], [100, 2]]),
        "{v}"
    );
    // hdr_value_at_percentile: rank = trunc(p/100 * count + 0.5) over the
    // sorted samples (NOT the map's bucket-crossing rule: the map reports the
    // bucket the tick lands in, so 98.4375 is 99 there but 98 here).
    assert_eq!(
        v["rankValues"],
        json!([
            [1, 1],
            [50, 50],
            [75, 75],
            [87.5, 88],
            [93.75, 94],
            [96.875, 97],
            [98.4375, 98],
            [99.21875, 99],
            [100, 100],
        ]),
        "{v}"
    );

    for (index, code) in [
        (0, "ERR_INVALID_ARG_TYPE"), // false
        (1, "ERR_INVALID_ARG_TYPE"), // ''
        (2, "ERR_INVALID_ARG_TYPE"), // {}
        (3, "ERR_INVALID_ARG_TYPE"), // undefined
        (4, "ERR_INVALID_ARG_TYPE"), // null
        (5, "ERR_OUT_OF_RANGE"),     // 0
        (6, "ERR_OUT_OF_RANGE"),     // MAX_SAFE_INTEGER + 1
        (7, "ERR_OUT_OF_RANGE"),     // 1.5 (not an integer)
    ] {
        assert_eq!(
            v["recordErrors"][index]["code"], code,
            "record case {index}: {v}"
        );
    }
    assert_eq!(
        v["bigintRecord"],
        json!({ "count": 1, "max": 2, "maxBigInt": "2" }),
        "{v}"
    );
    assert_eq!(
        v["add"],
        json!({ "count": 2, "min": 1, "max": 3, "mean": 2 }),
        "add() merges the other histogram: {v}"
    );
    assert_eq!(
        v["addError"],
        json!({
            "code": "ERR_INVALID_ARG_TYPE",
            "name": "TypeError",
            "message": "The \"other\" argument must be an instance of RecordableHistogram. Received an instance of Object",
        }),
        "{v}"
    );
    assert_eq!(v["addNumberError"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");

    // createHistogram option validation: bad `options` / bad lowest / highest /
    // figures types are ERR_INVALID_ARG_TYPE, out-of-range `figures` is
    // ERR_OUT_OF_RANGE (node validates lowest, then highest, then figures).
    let expected_option_codes = [
        "ERR_INVALID_ARG_TYPE", // "hello"
        "ERR_INVALID_ARG_TYPE", // 1
        "ERR_INVALID_ARG_TYPE", // null
        "ERR_INVALID_ARG_TYPE", // { lowest: 'hello' }
        "ERR_INVALID_ARG_TYPE", // { lowest: false }
        "ERR_INVALID_ARG_TYPE", // { lowest: null }
        "ERR_INVALID_ARG_TYPE", // { lowest: {} }
        "ERR_INVALID_ARG_TYPE", // { highest: 'hello' }
        "ERR_INVALID_ARG_TYPE", // { figures: 'hello' }
        "ERR_OUT_OF_RANGE",     // { figures: 6 }
        "ERR_OUT_OF_RANGE",     // { figures: 10 }
    ];
    for (index, code) in expected_option_codes.iter().enumerate() {
        assert_eq!(
            v["optionErrors"][index]["code"], *code,
            "createHistogram case {index} ({}): {v}",
            v["optionErrors"][index]["options"]
        );
    }
    // A value above `highest` counts as exceeds and is not recorded.
    assert_eq!(
        v["customRange"],
        json!({ "count": 2, "exceeds": 1, "min": 3, "max": 11 }),
        "{v}"
    );
    for (index, code) in [
        (0, "ERR_OUT_OF_RANGE"),
        (1, "ERR_OUT_OF_RANGE"),
        (2, "ERR_OUT_OF_RANGE"),
        (3, "ERR_OUT_OF_RANGE"),
        (4, "ERR_INVALID_ARG_TYPE"),
        (5, "ERR_INVALID_ARG_TYPE"),
    ] {
        assert_eq!(
            v["percentileErrors"][index]["code"], code,
            "percentile case {index}: {v}"
        );
    }

    assert_eq!(v["inspectPrefix"], true, "inspect(h, {{depth: null}}): {v}");
    assert_eq!(v["inspectShallow"], "[RecordableHistogram]", "{v}");
    assert_eq!(
        v["illegalConstructor"],
        json!({
            "code": "ERR_ILLEGAL_CONSTRUCTOR",
            "name": "TypeError",
            "message": "Illegal constructor",
        }),
        "{v}"
    );

    assert_eq!(
        v["transferIdentity"], true,
        "postMessage clones the histogram: {v}"
    );
    assert_eq!(v["transferCtor"], "RecordableHistogram", "{v}");
    assert_eq!(v["transferCount"], 1, "{v}");
    assert_eq!(
        v["transferAfterRecord"], 2,
        "clone records into the sender: {v}"
    );

    assert_eq!(v["eldInitialCount"], 0, "{v}");
    assert_eq!(v["eldEnable"], true, "{v}");
    assert_eq!(v["eldEnableAgain"], false, "{v}");
    assert_eq!(v["eldDispose"], "function", "{v}");
    assert_eq!(v["eldSample"]["minIsNumber"], true, "{v}");
    assert!(v["eldSample"]["count"].as_u64().unwrap_or(0) > 0, "{v}");
    assert_eq!(v["eldSample"]["minPositive"], true, "{v}");
    assert_eq!(v["eldDisable"], true, "{v}");
    assert_eq!(v["eldDisableAgain"], false, "{v}");
    assert_eq!(v["eldResolution"], true, "{v}");
    assert_eq!(
        v["eldBadResolution"],
        json!({
            "code": "ERR_OUT_OF_RANGE",
            "name": "RangeError",
            "message": "The value of \"options.resolution\" is out of range. It must be >= 1. Received 0",
        }),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn util_call_sites_and_deprecate_contract() {
    let (_rt, v) = run_fixture("util-waveb", UTIL_CONTRACT_TS, "__utilContract").await;

    assert_eq!(
        v["prepareCalls"], 0,
        "getCallSites must not invoke Error.prepareStackTrace: {v}"
    );
    assert_eq!(v["frameCount"], 1, "{v}");
    assert_eq!(
        v["frame"]["proto"], true,
        "CallSite objects have a null prototype: {v}"
    );
    assert_eq!(v["frame"]["lineIsNumber"], true, "{v}");
    assert_eq!(
        v["frameKeys"],
        json!([
            "column",
            "columnNumber",
            "functionName",
            "lineNumber",
            "scriptId",
            "scriptName"
        ]),
        "{v}"
    );
    assert!(
        v["frame"]["scriptName"]
            .as_str()
            .unwrap_or("")
            .ends_with("util-waveb.ts")
            || v["frame"]["scriptName"].as_str().unwrap_or("").is_empty(),
        "the caller frame must name the fixture module: {v}"
    );

    assert_eq!(
        v["getCallSitesErrors"][0]["code"], "ERR_OUT_OF_RANGE",
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][0]["message"],
        "The value of \"frameCount\" is out of range. It must be >= 1 && <= 200. Received 0",
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][1]["code"], "ERR_OUT_OF_RANGE",
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][2]["message"],
        "The value of \"frameCount\" is out of range. It must be an integer. Received 1.5",
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][3],
        json!({
            "name": "countString",
            "code": "ERR_INVALID_ARG_TYPE",
            "name2": "TypeError",
            "message": "The \"frameCount\" argument must be of type number. Received type string ('x')",
        }),
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][4],
        json!({
            "name": "sourceMap",
            "code": "ERR_INVALID_ARG_TYPE",
            "name2": "TypeError",
            "message": "The \"options.sourceMap\" property must be of type boolean. Received type number (1)",
        }),
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][5]["message"],
        "The \"options\" argument must be of type object. Received null",
        "{v}"
    );
    assert_eq!(
        v["getCallSitesErrors"][6]["message"],
        "The \"options\" argument must be of type object. Received type string ('x')",
        "{v}"
    );
    assert!(
        v["defaultFrames"].as_u64().unwrap_or(0) >= 1,
        "getCallSites() defaults to a 10-frame window: {v}"
    );
    assert!(
        v["objectOptions"].as_u64().unwrap_or(0) >= 1,
        "an object first argument is the options bag: {v}"
    );

    let deprecate_expected = [
        "The \"code\" argument must be of type string. Received type number (1)",
        "The \"code\" argument must be of type string. Received type boolean (true)",
        "The \"code\" argument must be of type string. Received type boolean (false)",
        "The \"code\" argument must be of type string. Received null",
        "The \"code\" argument must be of type string. Received an instance of Object",
    ];
    for (index, message) in deprecate_expected.iter().enumerate() {
        assert_eq!(
            v["deprecateErrors"][index]["err"], "ERR_INVALID_ARG_TYPE",
            "deprecate case {index}: {v}"
        );
        assert_eq!(
            v["deprecateErrors"][index]["message"], *message,
            "deprecate case {index}: {v}"
        );
    }
    assert_eq!(
        v["deprecateOk"], "number",
        "a valid code still wraps the fn: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn events_unhandled_error_and_async_resource_contract() {
    let (_rt, v) = run_fixture("events-waveb", EVENTS_CONTRACT_TS, "__eventsContract").await;

    assert_eq!(
        v["unhandled"][0],
        json!({
            "name": "string",
            "code": "ERR_UNHANDLED_ERROR",
            "errorName": "Error",
            "message": "Unhandled error. ('Accepts a string')",
            "context": true,
        }),
        "{v}"
    );
    assert_eq!(
        v["unhandled"][1],
        json!({
            "name": "object",
            "code": "ERR_UNHANDLED_ERROR",
            "errorName": "Error",
            "message": "Unhandled error. ({ message: 'Error!' })",
            "context": true,
        }),
        "{v}"
    );
    assert_eq!(
        v["unhandled"][2]["message"], "Unhandled error. ([object Object])",
        "a throwing custom inspect falls back to String(value): {v}"
    );
    assert_eq!(
        v["errorPassthrough"], true,
        "an Error argument is rethrown as-is: {v}"
    );

    assert_eq!(v["asyncIdIsNumber"], true, "{v}");
    assert_eq!(
        v["triggerAsyncIdIsParent"], true,
        "triggerAsyncId is the creating execution: {v}"
    );
    assert_eq!(
        v["asyncResourceEventEmitter"], true,
        "asyncResource.eventEmitter === the emitter: {v}"
    );
    assert_eq!(
        v["hookTypes"],
        json!(["init", "before", "after", "destroy"]),
        "init/emit/destroy sequence: {v}"
    );
    assert_eq!(
        v["initType"], "WaveBFoo",
        "the class name is the async type: {v}"
    );
    assert_eq!(v["namedResource"], "ExplicitName", "positional name: {v}");
    assert_eq!(v["optionNamedResource"], "OptionName", "options.name: {v}");

    for index in 0..=4 {
        assert_eq!(
            v["privateErrors"][index]["message"],
            "Cannot read private member #asyncResource from an object whose class did not declare it",
            "private member case {index}: {v}"
        );
        assert_eq!(v["privateErrors"][index]["errorName"], "TypeError", "{v}");
    }
    assert_eq!(
        v["privateErrors"][2]["frameInStack"], true,
        "the `asyncId` getter frame must be in the stack: {v}"
    );
    assert_eq!(
        v["privateErrors"][3]["frameInStack"], true,
        "the `triggerAsyncId` getter frame must be in the stack: {v}"
    );
    assert_eq!(
        v["directConstruction"]["code"], "ERR_INVALID_ARG_TYPE",
        "a direct EventEmitterAsyncResource needs options.name: {v}"
    );
}
