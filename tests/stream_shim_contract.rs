//! Contract lock for the node:stream pad (`js/stream_shim.js.txt`).
//!
//! The vendored official suite measures this surface end to end
//! (`scripts/node_tests`, test-stream-*.js); this file pins the pieces that
//! were fixed for it and that other pir shims depend on, so a future pad
//! rewrite cannot silently regress them:
//!
//!   * constructor options land as own instance hooks
//!     (`new Writable({ write })`, `final`/`construct`/`transform`/`flush`,
//!     `read`/`encoding` on Readable) — the bug that made `Console` write
//!     into a void;
//!
//! The recursive-`write` stack-overflow case lives in the official suite
//! (`test-console-no-swallow-stack-overflow.js`): in-process the recursion
//! overflows the host thread's native stack before the engine's own guard
//! trips, so it is not reproducible from a cargo test thread.
//!
//!   * `write()` past the high-water mark returns false and 'drain' is
//!     edge-triggered, but suppressed once `end()` was called;
//!   * `_construct` gating: writes buffer until it completes, `destroy()`
//!     waits for it, a double callback is ERR_MULTIPLE_CALLBACK, and
//!     construction alone must not start a read;
//!   * `setEncoding` decodes pushed bytes, reports `readableEncoding`, and
//!     re-decodes already-buffered chunks;
//!   * `read()`/`push(null)`/`end`+'close' sequencing with autoDestroy, and
//!     the Web-streams interop statics (`Duplex.fromWeb`/`toWeb`).
//!
//! Fixtures run as extension modules in a per-test temp dir (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-stream-shim-{}-{test_name}",
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

async fn run_fixture(rt: &ExtensionRuntime, name: &str, source: &str, global: &str) -> Value {
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, source).expect("seed fixture");
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

/// One fixture exercising the whole contract; the assertions live on the Rust
/// side so a failure names the broken contract instead of a JS line number.
const STREAM_SHIM_TS: &str = r#"
import { Readable, Writable, Duplex, PassThrough, Transform } from "node:stream";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const tick = (n = 1) => new Promise<void>((resolve) => setTimeout(resolve, n));

    // --- constructor options land as own hooks ---------------------------
    const seen: any[] = [];
    const w = new Writable({
      write(this: any, chunk: any, encoding: any, callback: any) {
        seen.push(["write", Buffer.from(chunk).toString(), encoding]);
        callback();
      },
      final(this: any, callback: any) {
        seen.push(["final"]);
        callback();
      },
    });
    out.ownHooks = [
      Object.prototype.hasOwnProperty.call(w, "_write"),
      Object.prototype.hasOwnProperty.call(w, "_final"),
    ];
    w.on("finish", () => seen.push(["finish"]));
    w.write("x");
    w.end("y");
    await tick(5);
    out.hookOrder = seen;

    const readableWithEncoding = new Readable({ read() {}, encoding: "utf8" } as any);
    out.optionEncoding = readableWithEncoding.readableEncoding;

    // --- high-water mark + drain -----------------------------------------
    const pt = new PassThrough({ highWaterMark: 0 });
    const ptDrains: number[] = [];
    pt.on("drain", () => ptDrains.push(ptDrains.length + 1));
    out.ptWriteRet = pt.write("hello1");
    out.ptWritableLength = pt.writableLength;
    out.ptNeedDrain = pt.writableNeedDrain;
    const firstRead = pt.read();
    out.ptFirstRead = Buffer.from(firstRead).toString();
    pt.read();
    await tick(5);
    out.ptDrains = ptDrains.length;
    out.ptHwm = [pt.readableHighWaterMark, pt.writableHighWaterMark];

    const ended = new Writable({ write(c: any, e: any, cb: any) { process.nextTick(cb); }, highWaterMark: 1 });
    let endedDrains = 0;
    ended.on("drain", () => { endedDrains++; });
    out.endedWriteRet = ended.write("asd");
    ended.end();
    await tick(5);
    out.endedDrains = endedDrains;

    // --- construct gating -------------------------------------------------
    const errs: string[] = [];
    new Writable({ construct(cb: any) { cb(); cb(); } }).on("error", (error: any) => errs.push(error.code));
    new Writable({ construct(cb: any) { cb(new Error("test")); } }).on("error", (error: any) => errs.push(error.message));
    new Readable({ read() {}, construct(cb: any) { cb(new Error("test")); } } as any).on("error", (error: any) => errs.push(error.message));
    new Writable({ construct(cb: any) { process.nextTick(cb, new Error("test")); } }).on("error", (error: any) => errs.push(error.message));
    await tick(5);
    out.constructErrors = errs;

    let constructed = false;
    let closed = false;
    const gated = new Writable({
      construct(cb: any) { constructed = true; process.nextTick(cb); },
    });
    gated.on("close", () => { closed = true; });
    gated.destroy();
    await tick(5);
    out.destroyWaitsConstruct = [constructed, closed];

    let bareConstruct = 0;
    new Duplex({ construct() { bareConstruct++; } });
    let reads = 0;
    new Readable({ construct(cb: any) { cb(); }, read() { reads++; } });
    await tick(5);
    out.bareDuplexConstruct = bareConstruct;
    out.constructNoRead = reads;

    // --- autoDestroy ------------------------------------------------------
    const writeEvents: string[] = [];
    const autoWrite = new Writable({
      autoDestroy: true,
      construct(cb: any) { process.nextTick(cb); },
      write(c: any, e: any, cb: any) { process.nextTick(cb); },
      final(cb: any) { process.nextTick(cb); },
    });
    autoWrite.on("finish", () => writeEvents.push("finish"));
    autoWrite.on("close", () => writeEvents.push("close"));
    autoWrite.end("data");

    const readEvents: string[] = [];
    const autoRead = new Readable({
      autoDestroy: true,
      construct(cb: any) { process.nextTick(cb); },
      read(this: any) { this.push(null); },
    } as any);
    autoRead.on("end", () => readEvents.push("end"));
    autoRead.on("close", () => readEvents.push("close"));
    autoRead.read(1);
    await tick(5);
    // node emits 'end' from the read that observes EOF, so the consumer has to
    // come back after construction (the official test does it with a streaming
    // 'data' listener).
    autoRead.read(0);
    autoRead.read();
    await tick(8);
    out.autoWriteEvents = writeEvents;
    out.autoReadEvents = readEvents;

    // --- setEncoding ------------------------------------------------------
    const dec = new Readable({ read() {} });
    dec.push(Buffer.from("h\u00e9"));
    dec.setEncoding("utf8");
    out.encodingLabel = dec.readableEncoding;
    out.encodingReDecoded = dec.read();
    const dec2 = new Readable({ read() {} });
    dec2.setEncoding("utf8");
    const dec2Data: string[] = [];
    dec2.on("data", (chunk: any) => dec2Data.push(String(chunk)));
    dec2.push(Buffer.from("ab"));
    dec2.push(Buffer.from("cd"));
    dec2.push(null);
    await tick(2);
    out.encodingStreamed = dec2Data;

    // --- read surface -----------------------------------------------------
    const buffered = new Readable({ objectMode: true, read() {} } as any);
    buffered.push({ n: 1 });
    buffered.push({ n: 2 });
    buffered.push(null);
    // `readableEnded` flips only when the EOF read lands on a tick (node's
    // endEmitted), so the synchronous read sees false — like node.
    out.readBuffered = [buffered.read(), buffered.read(), buffered.read(), buffered.readableEnded];
    await tick(3);
    out.readableEndedAfterTick = buffered.readableEnded;

    const text = new Readable({ read() {} });
    text.setEncoding("utf8");
    text.push(Buffer.from("ab"));
    text.push(Buffer.from("cd"));
    out.readConcatenated = text.read();

    // --- Transform --------------------------------------------------------
    const pass = new PassThrough();
    const transformed: string[] = [];
    pass.on("data", (chunk: any) => transformed.push(Buffer.from(chunk).toString()));
    pass.on("end", () => transformed.push("end"));
    pass.on("finish", () => transformed.push("finish"));
    pass.end("t");
    await tick(5);
    out.passThrough = transformed;

    const upper = new Transform({
      transform(chunk: any, _enc: any, cb: any) { cb(null, String(chunk).toUpperCase()); },
    });
    const upperOut: string[] = [];
    upper.on("data", (chunk: any) => upperOut.push(String(chunk)));
    upper.end("abc");
    await tick(5);
    out.transform = upperOut;

    // --- Web-streams interop ---------------------------------------------
    out.interop = [typeof (Duplex as any).fromWeb, typeof (Duplex as any).toWeb];

    globalThis.__streamShimContract = out;
  });
};
"#;

#[tokio::test]
async fn stream_shim_contract() {
    let dir = write_temp_fixture("contract", &[("index.ts", STREAM_SHIM_TS)]);
    let rt = new_runtime_at(&dir).await;
    let v = run_fixture(
        &rt,
        "stream-contract",
        STREAM_SHIM_TS,
        "__streamShimContract",
    )
    .await;

    // Constructor options land on the instance (own properties, node's shape).
    assert_eq!(
        v["ownHooks"],
        json!([true, true]),
        "write/final must land as own hooks: {v}"
    );
    assert_eq!(
        v["hookOrder"],
        json!([
            ["write", "x", "buffer"],
            ["write", "y", "buffer"],
            ["final"],
            ["finish"]
        ]),
        "options.write/options.final must drive the write path: {v}"
    );
    assert_eq!(
        v["optionEncoding"], "utf8",
        "Readable(options.encoding) must set the decoder: {v}"
    );

    // High-water mark accounting and the drain edge.
    assert_eq!(
        v["ptWriteRet"], false,
        "write past hwm 0 returns false: {v}"
    );
    assert_eq!(v["ptWritableLength"], 6, "the chunk stays buffered: {v}");
    assert_eq!(
        v["ptNeedDrain"], true,
        "writableNeedDrain reflects the hwm: {v}"
    );
    assert_eq!(
        v["ptFirstRead"], "hello1",
        "read() drains the transform: {v}"
    );
    assert_eq!(
        v["ptDrains"], 1,
        "drain fires once when the buffer drains: {v}"
    );
    assert_eq!(
        v["ptHwm"],
        json!([0, 0]),
        "a zero highWaterMark applies to both Transform sides: {v}"
    );
    assert_eq!(v["endedWriteRet"], false, "{v}");
    assert_eq!(
        v["endedDrains"], 0,
        "no drain after end() (test-stream-write-drain): {v}"
    );

    // Construct gating.
    let mut construct_errors: Vec<&str> = v["constructErrors"]
        .as_array()
        .expect("constructErrors array")
        .iter()
        .map(|e| e.as_str().unwrap_or_default())
        .collect();
    construct_errors.sort_unstable();
    assert_eq!(
        construct_errors,
        ["ERR_MULTIPLE_CALLBACK", "test", "test", "test"],
        "construct callback errors surface as 'error': {v}"
    );
    assert_eq!(
        v["destroyWaitsConstruct"],
        json!([true, true]),
        "destroy() waits for _construct, then closes: {v}"
    );
    assert_eq!(
        v["bareDuplexConstruct"], 1,
        "construct is invoked once per stream: {v}"
    );
    assert_eq!(
        v["constructNoRead"], 0,
        "construct alone must not start a read: {v}"
    );

    // autoDestroy ordering.
    assert_eq!(
        v["autoWriteEvents"],
        json!(["finish", "close"]),
        "finish then close under autoDestroy: {v}"
    );
    assert_eq!(
        v["autoReadEvents"],
        json!(["end", "close"]),
        "end then close under autoDestroy: {v}"
    );

    // setEncoding.
    assert_eq!(v["encodingLabel"], "utf8", "{v}");
    assert_eq!(
        v["encodingReDecoded"], "h\u{e9}",
        "setEncoding must re-decode the buffered chunk: {v}"
    );
    assert_eq!(
        v["encodingStreamed"],
        json!(["ab", "cd"]),
        "pushed bytes reach a 'data' listener decoded and split per push: {v}"
    );

    // read surface.
    assert_eq!(
        v["readBuffered"],
        json!([{ "n": 1 }, { "n": 2 }, null, false]),
        "read() returns buffered chunks then null; endEmitted lands on a tick: {v}"
    );
    assert_eq!(
        v["readableEndedAfterTick"], true,
        "readableEnded flips once the tick runs: {v}"
    );
    assert_eq!(
        v["readConcatenated"], "abcd",
        "a byte-mode read() concatenates decoded chunks (node BufferList): {v}"
    );

    // Transform.
    assert_eq!(
        v["passThrough"],
        json!(["t", "end", "finish"]),
        "PassThrough passes data, then ends and finishes: {v}"
    );
    assert_eq!(v["transform"], json!(["ABC"]), "{v}");

    // Interop statics.
    assert_eq!(v["interop"], json!(["function", "function"]), "{v}");
}
