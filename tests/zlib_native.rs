//! `node:zlib` codec parity and throughput fixtures.
//!
//! Node's zlib is C zlib; the host side is `flate2` on the `zlib-rs` backend.
//! These tests pin the observable convenience-method surface (defaults,
//! dictionaries, `maxOutputLength`, error codes, container sniffing) and,
//! under `--ignored`, report the dev-profile timings the C-ABI work is judged
//! by. Byte-for-byte compressed output parity is not required (any legal
//! compressor output is accepted) — decompression of Node-produced streams
//! and round-trips are what matter.
//!
//! Fixtures are generated at runtime into per-test temp dirs (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};
use std::time::Instant;

use base64::Engine as _;
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-zlib-native-{}-{test_name}",
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
        ..ExtensionConfig::default()
    }
}

async fn new_runtime_at(dir: &Path) -> ExtensionRuntime {
    ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction")
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

/// Round-trips every codec, checks empty/large inputs, container sniffing,
/// dictionaries and the failure codes.
const PARITY_TS: &str = r#"
import zlib from "@node/zlib";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const text = "hello zlib 世界 hello zlib 世界";
    const small = Buffer.alloc(4096, 0x61);
    const big = Buffer.from(Array.from({ length: 1024 * 1024 }, (_, i) => (i * 131 + 17) & 0xff));
    const empty = Buffer.alloc(0);

    const rt = (encode: any, decode: any, input: Buffer) => {
      const encoded = encode(input);
      const decoded = decode(encoded);
      return decoded.equals(input);
    };

    out.roundtrips = {
      gzipSmall: rt(zlib.gzipSync, zlib.gunzipSync, small),
      deflateSmall: rt(zlib.deflateSync, zlib.inflateSync, small),
      rawSmall: rt(zlib.deflateRawSync, zlib.inflateRawSync, small),
      brotliSmall: rt(zlib.brotliCompressSync, zlib.brotliDecompressSync, small),
      zstdSmall: rt(zlib.zstdCompressSync, zlib.zstdDecompressSync, small),
      gzipBig: rt(zlib.gzipSync, zlib.gunzipSync, big),
      deflateBig: rt(zlib.deflateSync, zlib.inflateSync, big),
      rawBig: rt(zlib.deflateRawSync, zlib.inflateRawSync, big),
      brotliBig: rt(zlib.brotliCompressSync, zlib.brotliDecompressSync, big),
      zstdBig: rt(zlib.zstdCompressSync, zlib.zstdDecompressSync, big),
      gzipEmpty: rt(zlib.gzipSync, zlib.gunzipSync, empty),
      deflateEmpty: rt(zlib.deflateSync, zlib.inflateSync, empty),
      rawEmpty: rt(zlib.deflateRawSync, zlib.inflateRawSync, empty),
      brotliEmpty: rt(zlib.brotliCompressSync, zlib.brotliDecompressSync, empty),
      zstdEmpty: rt(zlib.zstdCompressSync, zlib.zstdDecompressSync, empty),
      utf8Text: zlib.gunzipSync(zlib.gzipSync(Buffer.from(text))).toString("utf8") === text,
    };

    // Node-produced streams must decode with the host backend.
    const nodeGz = Buffer.from("H4sIAAAAAAAACstIzcnJVyjPL8pJAQCFEUoNCwAAAA==", "base64");
    out.nodeGzipDecode = zlib.gunzipSync(nodeGz).toString("utf8");
    const nodeDeflate = Buffer.from("eJzLSM3JyVcozy/KSQEAGgsEXQ==", "base64");
    out.nodeDeflateDecode = zlib.inflateSync(nodeDeflate).toString("utf8");

    // Container sniffing: unzip accepts both wrappers, not raw.
    out.unzip = {
      gzip: zlib.unzipSync(zlib.gzipSync(small)).equals(small),
      zlib: zlib.unzipSync(zlib.deflateSync(small)).equals(small),
      raw: (() => { try { zlib.unzipSync(zlib.deflateRawSync(small)); return "no-throw"; }
                    catch (e: any) { return e.code; } })(),
    };
    out.gunzipStrict = (() => {
      try { zlib.gunzipSync(zlib.deflateSync(small)); return "no-throw"; }
      catch (e: any) { return e.code; }
    })();

    // Dictionaries (deflate/inflate family).
    const dict = Buffer.from("the quick brown fox jumps over the lazy dog");
    const dictInput = Buffer.from("the quick brown fox jumps over the lazy dog, again and again");
    const withDict = zlib.deflateSync(dictInput, { dictionary: dict });
    out.dictionary = {
      roundtrip: zlib.inflateSync(withDict, { dictionary: dict }).equals(dictInput),
      rawRoundtrip: zlib.inflateRawSync(
        zlib.deflateRawSync(dictInput, { dictionary: dict }), { dictionary: dict }).equals(dictInput),
      missing: (() => { try { zlib.inflateSync(withDict); return "no-throw"; }
                        catch (e: any) { return e.code; } })(),
      wrong: (() => { try { zlib.inflateSync(withDict, { dictionary: Buffer.from("wrong") });
                             return "no-throw"; }
                       catch (e: any) { return e.code; } })(),
      gzipIgnored: zlib.gunzipSync(zlib.gzipSync(dictInput, { dictionary: dict })).equals(dictInput),
      zstdRoundtrip: zlib.zstdDecompressSync(
        zlib.zstdCompressSync(dictInput, { dictionary: dict }), { dictionary: dict }).equals(dictInput),
    };

    // maxOutputLength caps the result size.
    out.maxOutputLength = {
      compress: (() => { try { zlib.deflateSync(small, { maxOutputLength: 1 }); return "no-throw"; }
                         catch (e: any) { return e.code; } })(),
      decompress: (() => { try { zlib.inflateSync(zlib.deflateSync(small), { maxOutputLength: 10 });
                                 return "no-throw"; }
                           catch (e: any) { return e.code; } })(),
      exact: zlib.inflateSync(zlib.deflateSync(small), { maxOutputLength: 4096 }).length,
    };

    // Failure codes on garbage and truncated input.
    out.garbage = {
      inflate: (() => { try { zlib.inflateSync(Buffer.from("garbage")); return "no-throw"; }
                        catch (e: any) { return e.code; } })(),
      gunzip: (() => { try { zlib.gunzipSync(Buffer.from("garbage")); return "no-throw"; }
                       catch (e: any) { return e.code; } })(),
      inflateRaw: (() => { try { zlib.inflateRawSync(Buffer.from("garbage")); return "no-throw"; }
                           catch (e: any) { return e.code; } })(),
      truncated: (() => {
        const d = zlib.deflateSync(small);
        try { zlib.inflateSync(d.slice(0, d.length - 3)); return "no-throw"; }
        catch (e: any) { return e.code; }
      })(),
      emptyGzip: (() => { try { zlib.gunzipSync(Buffer.alloc(0)); return "no-throw"; }
                          catch (e: any) { return e.code; } })(),
      emptyInflate: (() => { try { zlib.inflateSync(Buffer.alloc(0)); return "no-throw"; }
                           catch (e: any) { return e.code; } })(),
    };

    // Multi-member gzip: node concatenates members on gunzip.
    const membered = Buffer.concat([
      zlib.gzipSync(Buffer.from("a")),
      zlib.gzipSync(Buffer.from("b")),
    ]);
    out.multiMember = zlib.gunzipSync(membered).toString();

    // Sync methods validate the deflate-family option window too.
    out.syncOptionError = (() => {
      try { zlib.deflateSync(Buffer.from("x"), { chunkSize: 63 }); return "no-throw"; }
      catch (e: any) { return { code: e.code, name: e.name, message: e.message }; }
    })();
    out.maxOutputError = (() => {
      try { zlib.deflateSync(Buffer.from("hello"), { maxOutputLength: 1.5 }); return "no-throw"; }
      catch (e: any) { return { code: e.code, name: e.name, message: e.message }; }
    })();
    out.maxOutputRange = (() => {
      try { zlib.deflateSync(Buffer.from("hello"), { maxOutputLength: 0 }); return "no-throw"; }
      catch (e: any) { return { code: e.code, message: e.message }; }
    })();

    // --- typed C-ABI entry (`bytes_ops::zlib` / `__pirZlib`) ---------------
    const typed = (globalThis as any).__pirZlib;
    const asBuffer = (view: any) => Buffer.from(view.buffer, view.byteOffset, view.byteLength);
    out.typed = {
      present: typeof typed === "function",
      shapecase: typeof typed !== "function" ? null : (() => {
        const deflated = typed("deflate", small);
        return {
          isView: deflated instanceof Uint8Array,
          exact: deflated.byteOffset === 0 && deflated.buffer.byteLength === deflated.length,
          roundtrip: zlib.inflateSync(asBuffer(deflated)).equals(small),
          syncShare: zlib.deflateSync(small).buffer.byteLength === zlib.deflateSync(small).length,
        };
      })(),
      declines: typeof typed !== "function" ? null : {
        brotli: typed("brotliCompress", small) === undefined,
        string: typed("deflate", "abc") === undefined,
        badOption: typed("deflate", small, { level: "9" }) === undefined,
        arrayBufferDict: typed("deflate", small, { dictionary: new ArrayBuffer(8) }) === undefined,
        dataViewDict: typed("deflate", small,
          { dictionary: new DataView(new ArrayBuffer(8)) }) === undefined,
        unknownOp: typed("nope", small) === undefined,
      },
      options: typeof typed !== "function" ? null : {
        level0: (() => {
          const stored = typed("deflate", small, { level: 0 });
          return stored.length > typed("deflate", small).length &&
                 zlib.inflateSync(asBuffer(stored)).equals(small);
        })(),
        dictionary: (() => {
          const raw = typed("deflateRaw", dictInput, { dictionary: dict });
          return zlib.inflateRawSync(asBuffer(raw), { dictionary: dict }).equals(dictInput);
        })(),
        maxOutputLength: (() => {
          try { typed("deflate", small, { maxOutputLength: 1 }); return "no-throw"; }
          catch (e: any) { return e.code + "|" + e.name; }
        })(),
      },
      error: typeof typed !== "function" ? null : (() => {
        try { typed("inflate", Buffer.from("garbage")); return "no-throw"; }
        catch (e: any) { return e.code + "|" + e.name; }
      })(),
      // A DataView dictionary is a shape the typed tier declines; the shim
      // must still serve it through the envelope path.
      envelopeFallback: zlib.inflateSync(
        zlib.deflateSync(dictInput, {
          dictionary: new DataView(dict.buffer, dict.byteOffset, dict.byteLength),
        }),
        { dictionary: dict },
      ).equals(dictInput),
    };

    // gzip header: mtime 0 (Node stamps 0 by default).
    const gz = zlib.gzipSync(small);
    out.gzipHeader = { mtime: gz.readUInt32LE(4), magic: [gz[0], gz[1]] };

    // `info` wraps the buffer with the transform Node would have driven.
    const info = zlib.deflateSync(small, { info: true });
    out.info = {
      keys: Object.keys(info),
      engine: info.engine instanceof zlib.Deflate,
      roundtrip: zlib.inflateSync(info.buffer).equals(small),
    };

    // Levels 0 and 9 must both round-trip (and level 0 must be stored).
    out.levels = {
      zero: zlib.inflateSync(zlib.deflateSync(small, { level: 0 })).equals(small),
      nine: zlib.inflateSync(zlib.deflateSync(small, { level: 9 })).equals(small),
      zeroLarger: zlib.deflateSync(small, { level: 0 }).length > zlib.deflateSync(small).length,
    };

    // Stream constructors still work end-to-end.
    out.stream = await new Promise((resolve) => {
      const chunks: Buffer[] = [];
      const gz = zlib.createGzip();
      gz.on("data", (chunk: Buffer) => chunks.push(chunk));
      gz.on("end", () => {
        resolve(zlib.gunzipSync(Buffer.concat(chunks)).equals(small));
      });
      gz.write(small);
      gz.end();
    });

    // Callback form.
    out.callback = await new Promise((resolve) => {
      zlib.deflate(small, (error: any, result: any) => {
        resolve(!error && zlib.inflateSync(result).equals(small));
      });
    });

    globalThis.__zlibNative = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn zlib_convenience_parity() {
    let (_, v) = run_fixture("parity", PARITY_TS, "__zlibNative").await;

    for key in [
        "gzipSmall",
        "deflateSmall",
        "rawSmall",
        "brotliSmall",
        "zstdSmall",
        "gzipBig",
        "deflateBig",
        "rawBig",
        "brotliBig",
        "zstdBig",
        "gzipEmpty",
        "deflateEmpty",
        "rawEmpty",
        "brotliEmpty",
        "zstdEmpty",
        "utf8Text",
    ] {
        assert_eq!(v["roundtrips"][key], true, "roundtrip {key} failed: {v}");
    }
    assert_eq!(v["nodeGzipDecode"], "hello world", "{v}");
    assert_eq!(v["nodeDeflateDecode"], "hello world", "{v}");
    assert_eq!(
        v["unzip"],
        json!({ "gzip": true, "zlib": true, "raw": "Z_DATA_ERROR" }),
        "{v}"
    );
    assert_eq!(v["gunzipStrict"], "Z_DATA_ERROR", "{v}");
    assert_eq!(
        v["dictionary"],
        json!({ "roundtrip": true, "rawRoundtrip": true, "missing": "Z_NEED_DICT", "wrong": "Z_NEED_DICT", "gzipIgnored": true, "zstdRoundtrip": true }),
        "{v}"
    );
    assert_eq!(
        v["maxOutputLength"],
        json!({ "compress": "ERR_BUFFER_TOO_LARGE", "decompress": "ERR_BUFFER_TOO_LARGE", "exact": 4096 }),
        "{v}"
    );
    assert_eq!(
        v["garbage"],
        json!({
            "inflate": "Z_DATA_ERROR",
            "gunzip": "Z_DATA_ERROR",
            "inflateRaw": "Z_DATA_ERROR",
            "truncated": "Z_BUF_ERROR",
            "emptyGzip": "Z_BUF_ERROR",
            "emptyInflate": "Z_BUF_ERROR",
        }),
        "{v}"
    );
    assert_eq!(v["typed"]["present"], true, "typed zlib entry missing: {v}");
    assert_eq!(
        v["typed"]["shapecase"],
        json!({ "isView": true, "exact": true, "roundtrip": true, "syncShare": true }),
        "{v}"
    );
    assert_eq!(
        v["typed"]["declines"],
        json!({
            "brotli": true,
            "string": true,
            "badOption": true,
            "arrayBufferDict": true,
            "dataViewDict": true,
            "unknownOp": true,
        }),
        "{v}"
    );
    assert_eq!(
        v["typed"]["options"],
        json!({ "level0": true, "dictionary": true, "maxOutputLength": "ERR_BUFFER_TOO_LARGE|Error" }),
        "{v}"
    );
    assert_eq!(v["typed"]["error"], "Z_DATA_ERROR|Error", "{v}");
    assert_eq!(v["typed"]["envelopeFallback"], true, "{v}");
    assert_eq!(v["multiMember"], "ab", "{v}");
    assert_eq!(
        v["syncOptionError"],
        json!({
            "code": "ERR_OUT_OF_RANGE",
            "name": "RangeError",
            "message": "The value of \"options.chunkSize\" is out of range. It must be >= 64. Received 63",
        }),
        "{v}"
    );
    assert_eq!(
        v["maxOutputError"],
        json!({
            "code": "ERR_BUFFER_TOO_LARGE",
            "name": "RangeError",
            "message": "Cannot create a Buffer larger than 1.5 bytes",
        }),
        "{v}"
    );
    assert_eq!(
        v["maxOutputRange"],
        json!({
            "code": "ERR_OUT_OF_RANGE",
            "message": "The value of \"options.maxOutputLength\" is out of range. It must be >= 1 and <= 9007199254740991. Received 0",
        }),
        "{v}"
    );
    assert_eq!(v["gzipHeader"]["mtime"], 0, "{v}");
    assert_eq!(v["gzipHeader"]["magic"], json!([0x1f, 0x8b]), "{v}");
    assert_eq!(
        v["info"],
        json!({ "keys": ["buffer", "engine"], "engine": true, "roundtrip": true }),
        "{v}"
    );
    assert_eq!(
        v["levels"],
        json!({ "zero": true, "nine": true, "zeroLarger": true }),
        "{v}"
    );
    assert_eq!(v["stream"], true, "stream roundtrip failed: {v}");
    assert_eq!(v["callback"], true, "callback roundtrip failed: {v}");
}

// ---------------------------------------------------------------------------
// Timing (dev profile; ignored by default)
// ---------------------------------------------------------------------------

/// The dev-profile timing report for the ops this workstream owns, plus the
/// split between the JS shim path and the raw Rust codec.
#[tokio::test(flavor = "current_thread")]
#[ignore = "timing measurement: run with --ignored --nocapture"]
async fn zlib_sync_timings() {
    const BENCH_TS: &str = r#"
import zlib from "@node/zlib";
import { performance } from "node:perf_hooks";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const small = Buffer.alloc(4096, 0x61);
    const big = Buffer.alloc(1024 * 1024, 0x62);
    const deflatedSmall = zlib.deflateSync(small);
    const deflatedBig = zlib.deflateSync(big);
    const rawSmall = zlib.deflateRawSync(small);
    const time = (fn: any, iterations: number) => {
      for (let i = 0; i < 100; i++) fn();
      const started = performance.now();
      for (let i = 0; i < iterations; i++) fn();
      return ((performance.now() - started) * 1000) / iterations;
    };
    const smallB64 = small.toString("base64");
    const bigB64 = big.toString("base64");
    const invoke = (globalThis as any).__pirNode;
    const typed = (globalThis as any).__pirZlib;
    globalThis.__zlibTiming = {
      deflate4k: time(() => zlib.deflateSync(small), 5000),
      inflate4k: time(() => zlib.inflateSync(deflatedSmall), 5000),
      deflate1m: time(() => zlib.deflateSync(big), 200),
      inflate1m: time(() => zlib.inflateSync(deflatedBig), 200),
      deflateRaw4k: time(() => zlib.deflateRawSync(small), 5000),
      inflateRaw4k: time(() => zlib.inflateRawSync(rawSmall), 5000),
      b64enc4k: time(() => small.toString("base64"), 5000),
      b64dec4k: time(() => Buffer.from(smallB64, "base64"), 5000),
      b64enc1m: time(() => big.toString("base64"), 200),
      b64dec1m: time(() => Buffer.from(bigB64, "base64"), 200),
      invoke4k: time(() => invoke.invoke("zlib", "deflate", { data: smallB64 }), 5000),
      invoke1m: time(() => invoke.invoke("zlib", "deflate", { data: bigB64 }), 200),
      typed4k: typeof typed === "function" ? time(() => typed("deflate", small), 5000) : -1,
      typed1m: typeof typed === "function" ? time(() => typed("deflate", big), 200) : -1,
      typedInflate1m: typeof typed === "function"
        ? time(() => typed("inflate", deflatedBig), 200) : -1,
    };
  });
};
"#;
    let (rt, v) = run_fixture("timing", BENCH_TS, "__zlibTiming").await;
    let _ = rt;
    for (name, us) in [
        ("deflate 4KB", v["deflate4k"].as_f64()),
        ("inflate 4KB", v["inflate4k"].as_f64()),
        ("deflate 1MB", v["deflate1m"].as_f64()),
        ("inflate 1MB", v["inflate1m"].as_f64()),
        ("deflateRaw 4KB", v["deflateRaw4k"].as_f64()),
        ("inflateRaw 4KB", v["inflateRaw4k"].as_f64()),
        ("b64 encode 4KB", v["b64enc4k"].as_f64()),
        ("b64 decode 4KB", v["b64dec4k"].as_f64()),
        ("b64 encode 1MB", v["b64enc1m"].as_f64()),
        ("b64 decode 1MB", v["b64dec1m"].as_f64()),
        ("invoke 4KB", v["invoke4k"].as_f64()),
        ("invoke 1MB", v["invoke1m"].as_f64()),
        ("typed 4KB", v["typed4k"].as_f64()),
        ("typed 1MB", v["typed1m"].as_f64()),
        ("typed inflate 1MB", v["typedInflate1m"].as_f64()),
    ] {
        println!("JS {name:16} {:8.1} us/op", us.unwrap_or(f64::NAN));
    }

    // Raw Rust codec cost (no shim, no JS objects): the same payloads through
    // the host op directly, with the base64 hop the current shim pays.
    let small = vec![0x61u8; 4096];
    let big = vec![0x62u8; 1024 * 1024];
    let encoded_small = base64::engine::general_purpose::STANDARD.encode(&small);
    let encoded_big = base64::engine::general_purpose::STANDARD.encode(&big);

    let rust_time = |label: &str, iterations: u32, mut f: Box<dyn FnMut()>| {
        for _ in 0..20 {
            f();
        }
        let started = Instant::now();
        for _ in 0..iterations {
            f();
        }
        println!(
            "RUST {label:16} {:8.1} us/op",
            started.elapsed().as_secs_f64() * 1e6 / iterations as f64
        );
    };

    let payload_small = json!({ "data": encoded_small });
    let payload_big = json!({ "data": encoded_big });
    let deflated_small =
        pir::pir_node::modules::child::zlib_op("deflate", &payload_small).expect("deflate");
    let deflated_big =
        pir::pir_node::modules::child::zlib_op("deflate", &payload_big).expect("deflate");
    let deflated_small = deflated_small.as_str().expect("base64");
    let deflated_big = deflated_big.as_str().expect("base64");
    let inflate_small_payload = json!({ "data": deflated_small });
    let inflate_big_payload = json!({ "data": deflated_big });

    rust_time(
        "op deflate 4KB",
        1000,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_op("deflate", &payload_small).expect("deflate");
        }),
    );
    rust_time(
        "op inflate 4KB",
        2000,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_op("inflate", &inflate_small_payload)
                .expect("inflate");
        }),
    );
    rust_time(
        "op deflate 1MB",
        100,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_op("deflate", &payload_big).expect("deflate");
        }),
    );
    rust_time(
        "op inflate 1MB",
        100,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_op("inflate", &inflate_big_payload)
                .expect("inflate");
        }),
    );
    // The zlib module dispatch calls `crypto::zlib_raw_op` first for every
    // zlib op. It now reports ENOSYS before decoding, so this is the fixed
    // per-call dispatch cost (it used to be a full base64 decode).
    rust_time(
        "raw-op passthrough 4KB",
        2000,
        Box::new(|| {
            let _ = pir::pir_node::modules::crypto::zlib_raw_op("deflate", &payload_small);
        }),
    );
    rust_time(
        "raw-op passthrough 1MB",
        100,
        Box::new(|| {
            let _ = pir::pir_node::modules::crypto::zlib_raw_op("deflate", &payload_big);
        }),
    );

    // Byte-level codec (`__pirZlib` backend): no base64 on either leg.
    let deflated_small_bytes = base64::engine::general_purpose::STANDARD
        .decode(deflated_small)
        .expect("deflated base64");
    let deflated_big_bytes = base64::engine::general_purpose::STANDARD
        .decode(deflated_big)
        .expect("deflated base64");
    let default_options = pir::pir_node::modules::child::ZlibBytesOptions::default();
    rust_time(
        "bytes-op deflate 4KB",
        2000,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_bytes_op("deflate", &small, &default_options)
                .expect("deflate");
        }),
    );
    rust_time(
        "bytes-op inflate 4KB",
        2000,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_bytes_op(
                "inflate",
                &deflated_small_bytes,
                &default_options,
            )
            .expect("inflate");
        }),
    );
    rust_time(
        "bytes-op deflate 1MB",
        100,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_bytes_op("deflate", &big, &default_options)
                .expect("deflate");
        }),
    );
    rust_time(
        "bytes-op inflate 1MB",
        100,
        Box::new(|| {
            pir::pir_node::modules::child::zlib_bytes_op(
                "inflate",
                &deflated_big_bytes,
                &default_options,
            )
            .expect("inflate");
        }),
    );
}
