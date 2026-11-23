//! util.inspect / util.format / console alignment cases that pin Node 24
//! behavior shared by the native fast path and the JS shim.
//!
//! The fixture runs the same table of cases twice: once with the typed
//! `__pirInspect`/`__pirFormat` entry points installed (the default) and once
//! with them deleted, forcing the faithful JS implementation. Both must agree
//! with the strings Node 24.22.1 produces (captured with the same script).

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-inspect-alignment-{}-{test_name}",
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

fn test_config_at(cwd: &std::path::Path) -> ExtensionConfig {
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

async fn new_runtime_at(dir: &std::path::Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

/// `collect()` below is the shared case table. Expectations are Node
/// v24.22.1's output.
const ALIGNMENT_TS: &str = r#"
import util from "node:util";
import { Console } from "node:console";

function collect() {
  const r: any = {};

  // --- N3: options are own + enumerable (node's Object.keys pass) ---------
  r.optOwn = util.inspect({ a: { b: 1 } }, { depth: 0 });
  r.optNonEnumerable =
    util.inspect({ a: { b: 1 } }, Object.defineProperty({}, "depth", { value: 0 }));
  r.optInherited = util.inspect({ a: { b: 1 } }, Object.create({ depth: 0 }));
  const hidden = { a: 1 };
  Object.defineProperty(hidden, "z", { value: 9 });
  r.optHiddenNonEnumerable =
    util.inspect(hidden, Object.defineProperty({}, "showHidden", { value: true }));
  r.optInheritedSorted =
    util.inspect({ b: 1, a: 2 }, Object.create({ sorted: true }));

  // --- B7: inherited Symbol.toStringTag on built-in fast paths ------------
  const map = new Map([[1, 2]]);
  Object.defineProperty(Map.prototype, Symbol.toStringTag,
    { value: "Foo", configurable: true });
  try { r.mapTag = util.inspect(map); } finally {
    delete (Map.prototype as any)[Symbol.toStringTag];
  }
  const set = new Set([1]);
  Object.defineProperty(Set.prototype, Symbol.toStringTag,
    { value: "Bar", configurable: true });
  try { r.setTag = util.inspect(set); } finally {
    delete (Set.prototype as any)[Symbol.toStringTag];
  }
  const date = new Date(0);
  Object.defineProperty(Date.prototype, Symbol.toStringTag,
    { value: "D", configurable: true });
  try { r.dateTag = util.inspect(date); } finally {
    delete (Date.prototype as any)[Symbol.toStringTag];
  }
  const re = /a/;
  Object.defineProperty(RegExp.prototype, Symbol.toStringTag,
    { value: "R", configurable: true });
  try { r.regexpTag = util.inspect(re); } finally {
    delete (RegExp.prototype as any)[Symbol.toStringTag];
  }

  // --- option shapes ------------------------------------------------------
  r.depth0 = util.inspect({ a: { b: 1 } }, { depth: 0 });
  r.depthNull = util.inspect({ a: { b: { c: 1 } } }, { depth: null });
  r.maxArrayLength = util.inspect([1, 2, 3, 4, 5], { maxArrayLength: 2 });
  r.maxStringLength = util.inspect("abcdefghij", { maxStringLength: 5 });
  r.breakLength = util.inspect([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], { breakLength: 20 });
  r.compactFalse = util.inspect([1, 2, 3, 4, 5, 6, 7, 8, 9], { compact: false });
  r.numericSeparator = util.inspect([1000000], { numericSeparator: true });
  r.sorted = util.inspect({ b: 1, a: 2 }, { sorted: true });
  r.getters = util.inspect(
    Object.defineProperty({ a: 1 }, "g", { get() { return 5; }, enumerable: true }),
    { getters: true });
  r.colors = util.inspect({ a: 1 }, { colors: true });
  r.customInspectOff = util.inspect(
    { a: 1, [util.inspect.custom]() { return "CUSTOM"; } },
    { customInspect: false });
  r.nullProto = util.inspect(Object.assign(Object.create(null), { a: 1 }));

  // --- format -------------------------------------------------------------
  r.fmtS = util.format("%s", { a: 1 });
  r.fmtD = util.format("%d", "42.9");
  r.fmtI = util.format("%i", "42.9");
  r.fmtF = util.format("%f", "1.25abc");
  r.fmtBigint = util.format("%d", 42n);
  r.fmtSymbol = util.format("%d", Symbol("s"));
  r.fmtJson = util.format("%j", { a: 1, b: [2] });
  const circular: any = {};
  circular.o = circular;
  r.fmtJsonCircular = util.format("%j", circular);
  r.fmtO = util.format("%O", { a: 1 });
  r.fmtPct = util.format("%s %% %d", "a", 2);
  r.fmtExtra = util.format("%s", "a", "b", { c: 1 });
  r.fmtNoFormat = util.format("hello", 1, { a: 2 });
  r.fmtWithOptions = util.formatWithOptions({ numericSeparator: true }, "%d", 1000000);

  // --- console ------------------------------------------------------------
  function capture(fn: (c: any) => void) {
    let text = "";
    const stream = {
      write(s: string) { text += s; },
      isTTY: false,
      once() {},
      removeListener() {},
      listenerCount() { return 0; },
    };
    const c = new Console({ stdout: stream, stderr: stream, colorMode: false });
    fn(c);
    return text;
  }
  r.consoleLog = capture((c) => c.log("a", { b: 1 }, 2));
  r.consoleDir = capture((c) => c.dir({ a: { b: 1 } }, { depth: 0 }));
  r.consoleAssert = capture((c) => c.assert(false, "bad %s", "x"));
  r.consoleCount = capture((c) => { c.count(); c.count("x"); c.count("x"); });
  r.consoleGroup = capture((c) => { c.group("g"); c.log("in"); c.groupEnd(); });
  r.consoleTable = capture((c) => c.table([{ a: 1, b: 2 }, { a: 3, b: 4 }]));

  return r;
}

export default function (pi: any) {
  pi.on("session_start", () => {
    const native = collect();
    // Force the faithful JS implementation for the second column.
    (globalThis as any).__pirInspect = undefined;
    (globalThis as any).__pirFormat = undefined;
    const shim = collect();
    (globalThis as any).__inspectAlignment = { native, shim };
  });
}
"#;

fn expect(v: &Value, key: &str, expected: &str) {
    assert_eq!(v[key].as_str(), Some(expected), "case {key}: {v}");
}

#[tokio::test(flavor = "current_thread")]
async fn util_inspect_format_console_match_node() {
    let dir = write_temp_fixture("cases", &[("align.ts", ALIGNMENT_TS)]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:align.ts", ALIGNMENT_TS)
        .expect("seed alignment extension");
    assert_eq!(
        rt.seeded_load("pir-ext:align.ts")
            .await
            .expect("seeded load"),
        1
    );
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while running alignment cases: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__inspectAlignment)")
        .await
        .expect("read alignment results");

    // The typed and shim columns must agree byte-for-byte.
    assert_eq!(v["native"], v["shim"], "native/shim divergence: {v}");

    let native = &v["native"];
    // N3: only own enumerable options are read (node's Object.keys).
    expect(native, "optOwn", "{ a: [Object] }");
    expect(native, "optNonEnumerable", "{ a: { b: 1 } }");
    expect(native, "optInherited", "{ a: { b: 1 } }");
    expect(native, "optHiddenNonEnumerable", "{ a: 1 }");
    expect(native, "optInheritedSorted", "{ b: 1, a: 2 }");

    // B7: inherited tags are honored (Fast path delegates, shim renders).
    expect(native, "mapTag", "Map(1) [Foo] { 1 => 2 }");
    expect(native, "setTag", "Set(1) [Bar] { 1 }");
    expect(native, "dateTag", "Date [D] 1970-01-01T00:00:00.000Z");
    expect(native, "regexpTag", "RegExp [R] /a/");

    // Option shapes.
    expect(native, "depth0", "{ a: [Object] }");
    expect(native, "depthNull", "{ a: { b: { c: 1 } } }");
    expect(native, "maxArrayLength", "[ 1, 2, ... 3 more items ]");
    expect(native, "maxStringLength", "'abcde'... 5 more characters");
    expect(native, "sorted", "{ a: 2, b: 1 }");
    expect(native, "nullProto", "[Object: null prototype] { a: 1 }");
    expect(native, "fmtS", "{ a: 1 }");
    expect(native, "fmtD", "42.9");
    expect(native, "fmtI", "42");
    expect(native, "fmtF", "1.25");
    expect(native, "fmtBigint", "42n");
    expect(native, "fmtSymbol", "NaN");
    expect(native, "fmtJson", r#"{"a":1,"b":[2]}"#);
    expect(native, "fmtJsonCircular", "[Circular]");
    expect(native, "fmtO", "{ a: 1 }");
    expect(native, "fmtPct", "a % 2");
    expect(native, "fmtExtra", "a b { c: 1 }");
    expect(native, "fmtNoFormat", "hello 1 { a: 2 }");
    expect(native, "fmtWithOptions", "1_000_000");
    expect(native, "consoleLog", "a { b: 1 } 2\n");
    expect(native, "consoleDir", "{ a: [Object] }\n");
    expect(native, "consoleAssert", "Assertion failed: bad x\n");
    expect(native, "consoleCount", "default: 1\nx: 1\nx: 2\n");
    expect(native, "consoleGroup", "g\n  in\n");
    expect(
        native,
        "consoleTable",
        "┌─────────┬───┬───┐\n\
         │ (index) │ a │ b │\n\
         ├─────────┼───┼───┤\n\
         │ 0       │ 1 │ 2 │\n\
         │ 1       │ 3 │ 4 │\n\
         └─────────┴───┴───┘\n",
    );
}
