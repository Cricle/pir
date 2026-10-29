//! `node:zlib` / `node:crypto` shim contract (js/zlib_shim.js.txt,
//! js/crypto_shim.js.txt) — the module family covered by the node_tests wave
//! that turned test-zlib-const / -crc32 / -from-string / -deflate-constructors
//! and test-crypto-hmac / -hash-stream-pipe / -encoding-validation-error green.
//!
//! Pins, per module:
//! - zlib: the `constants`/`codes` attribute shapes (null prototype,
//!   non-writable entries, frozen `codes`, non-writable `codes`/`constants` on
//!   the object `require("zlib")` hands out), `Z_MAX_CHUNK === Infinity`, the
//!   CRC-32 argument validation, container-sniffing `unzip`, the
//!   `{buffer, engine}` `info` option, and the option / `params()` ranges with
//!   Node's exact ERR_INVALID_ARG_TYPE / ERR_OUT_OF_RANGE messages.
//! - crypto: the SHA-224/384 and SHA-3 digests the host backend does not
//!   expose (RFC 6234 / FIPS 202 vectors), HMAC over them, the Hmac
//!   stream/`read()`/second-`digest()` semantics, the cipher output-encoding
//!   errors, and the WebCrypto `subtle`/`CryptoKey` surface reachable through
//!   `globalThis.crypto.subtle`.
//!
//! Not covered here: the DEP0181 deprecation warning for the exported `Hmac`
//! constructor (it is deferred to a microtask and asserted by
//! test-crypto-hmac at the suite level). Brotli and Zstd *are* covered: the
//! node-produced streams in ZLIB_CONTRACT_TS pin wire-format interop with the
//! host `brotli`/`zstd` backends, which the convenience-method wave added.
//!
//! Fixtures are generated at runtime into per-test temp dirs (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-zlib-crypto-{}-{test_name}",
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

/// `require("zlib")` goes through the CJS builtin path (not the ESM namespace),
/// which is where the module object's own property attributes are observable.
async fn require_probe(rt: &ExtensionRuntime, expression: &str) -> Value {
    let source = format!(
        "JSON.stringify((() => {{ \
           const dir = globalThis.process.cwd(); \
           const z = globalThis.__pirRequireFrom(dir, \"zlib\"); \
           const z2 = globalThis.__pirRequireFrom(dir, \"zlib\"); \
           return ({expression}); \
         }})())"
    );
    rt.eval_json(&source).await.expect("require probe")
}

const ZLIB_CONTRACT_TS: &str = r#"
import zlib from "@node/zlib";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- tables: the attribute shapes Node's zlib exposes ----------------
    out.constantsProtoNull = Object.getPrototypeOf(zlib.constants) === null;
    out.codesProtoNull = Object.getPrototypeOf(zlib.codes) === null;
    out.codesFrozen = Object.isFrozen(zlib.codes);
    out.zMaxChunk = zlib.constants.Z_MAX_CHUNK === Infinity;
    out.constantWritable = Object.getOwnPropertyDescriptor(zlib.constants, "Z_OK").writable;
    const writes: any = {};
    const writeCases: any[] = [
      ["constant", () => { (zlib.constants as any).Z_OK = 1; }],
      ["codeEntry", () => { (zlib.codes as any).Z_OK = 1; }],
      ["codesBinding", () => { (zlib as any).codes = { Z_OK: 1 }; }],
    ];
    for (const [name, run] of writeCases) {
      try {
        run();
        writes[name] = "no-throw";
      } catch (e: any) {
        writes[name] = { name: e.name, code: e.code };
      }
    }
    out.writes = writes;

    // --- crc32 argument validation ---------------------------------------
    // Explicit labels: `String(fn)` differs between engines (QuickJS reprints
    // the AST without V8's spacing), so the keys must not come from source
    // text.
    const crcErrors: any = {};
    const badData: any[] = [
      ["undefined", undefined], ["null", null], ["boolean", true],
      ["number", 1], ["function", () => {}], ["object", {}],
      ["arrayBuffer", new ArrayBuffer(3)],
    ];
    for (const [label, bad] of badData) {
      try {
        (zlib.crc32 as any)(bad);
        crcErrors[label] = "no-throw";
      } catch (e: any) {
        crcErrors[label] = { code: e.code, name: e.name };
      }
    }
    const valueErrors: any = {};
    const badValues: any[] = [
      ["null", null], ["boolean", true], ["function", () => {}], ["object", {}],
    ];
    for (const [label, bad] of badValues) {
      try {
        (zlib.crc32 as any)("test", bad);
        valueErrors[label] = "no-throw";
      } catch (e: any) {
        valueErrors[label] = { code: e.code, name: e.name };
      }
    }
    out.crcDataCodes = {
      undefined: crcErrors.undefined.code,
      null: crcErrors.null.code,
      boolean: crcErrors.boolean.code,
      number: crcErrors.number.code,
      function: crcErrors.function.code,
      object: crcErrors.object.code,
      arrayBuffer: crcErrors.arrayBuffer.code,
    };
    out.crcValueCodes = {
      null: valueErrors.null.code,
      boolean: valueErrors.boolean.code,
      function: valueErrors.function.code,
      object: valueErrors.object.code,
    };
    out.crcKnown = zlib.crc32("abacus", 0) === 0xc3d7115b;

    // --- unzip sniffs the container --------------------------------------
    const text = "blah".repeat(8);
    out.unzipGzip = zlib.unzipSync(zlib.gzipSync(text)).toString();
    out.unzipZlib = zlib.unzipSync(zlib.deflateSync(text)).toString();
    out.unzipDistinctBinding = zlib.unzip !== zlib.gunzip;

    // --- options.info wraps as { buffer, engine } ------------------------
    const info: any = zlib.gzipSync(text, { info: true });
    out.infoKeys = Object.keys(info);
    out.infoEngine = info.engine instanceof (zlib as any).Gzip;
    // `info.buffer` carries the codec output (the gzip container here).
    out.infoBuffer = zlib.gunzipSync(info.buffer).toString();
    const deflated: any = zlib.deflateSync(text, { info: true });
    out.infoInflate = zlib.inflateSync(deflated.buffer, { info: true }).engine
      instanceof (zlib as any).Inflate;

    // --- option + params() ranges (Node's exact messages) ----------------
    const optionRanges: any[] = [
      { chunkSize: "test" }, { chunkSize: -Infinity }, { chunkSize: 0 },
      { windowBits: 0 }, { level: -2 }, { memLevel: 16 }, { strategy: -2 },
    ];
    const optionErrors: any[] = [];
    for (const options of optionRanges) {
      try {
        new (zlib as any).Deflate(options);
        optionErrors.push({ options, outcome: "no-throw" });
      } catch (e: any) {
        optionErrors.push({ options, code: e.code, name: e.name, message: e.message });
      }
    }
    out.optionErrors = optionErrors;
    try {
      new (zlib as any).Deflate({ dictionary: "not a buffer" });
      out.dictionaryError = "no-throw";
    } catch (e: any) {
      out.dictionaryError = { code: e.code, name: e.name, message: e.message };
    }

    const paramCases: any[] = [["test"], [-Infinity], [Infinity], [-2], [0, "test"], [0, -Infinity], [0, -2]];
    const paramErrors: any[] = [];
    for (const args of paramCases) {
      try {
        new (zlib as any).Deflate().params(...(args as any[]));
        paramErrors.push({ args, outcome: "no-throw" });
      } catch (e: any) {
        paramErrors.push({ args, code: e.code, name: e.name, message: e.message });
      }
    }
    out.paramErrors = paramErrors;
    out.deflateCallable = (zlib as any).Deflate() instanceof (zlib as any).Deflate;
    out.paramsReturn = new (zlib as any).Deflate().params(1, 1) === undefined;
    out.validStrategies = [1, 2, 3, 4, 0].every((s) => {
      new (zlib as any).Deflate({ strategy: s });
      return true;
    });

    // --- the callback form requires a callback ---------------------------
    try {
      (zlib.gzip as any)("abc");
      out.missingCallback = "no-throw";
    } catch (e: any) {
      out.missingCallback = { code: e.code, message: e.message };
    }

    // --- brotli / zstd: real codecs, node-interoperable -------------------
    // The base64 vectors below are the *node v24.19.0* output for
    // "brotli interop 世界" (brotli quality 5 / zstd level 5, selected through
    // `params`), so decoding them here pins wire-format interop with node,
    // not just self-consistency.
    const NODE_BROTLI = "CwqAYnJvdGxpIGludGVyb3Ag5LiW55WMAw==";
    const NODE_ZSTD = "KLUv/SAVqQAAYnJvdGxpIGludGVyb3Ag5LiW55WM";
    const interop = "brotli interop 世界";
    out.brotliFromNode = zlib.brotliDecompressSync(Buffer.from(NODE_BROTLI, "base64")).toString();
    out.zstdFromNode = zlib.zstdDecompressSync(Buffer.from(NODE_ZSTD, "base64")).toString();
    out.brotliSync = zlib.brotliDecompressSync(
      zlib.brotliCompressSync(interop, { params: { 1: 5 } }),
    ).toString();
    out.zstdSync = zlib.zstdDecompressSync(
      zlib.zstdCompressSync(interop, { params: { 100: 5 } }),
    ).toString();
    // The brotli pair is callable without `new`; the zstd pair is not.
    out.brotliCallable = (zlib as any).BrotliCompress() instanceof (zlib as any).BrotliCompress;
    try {
      (zlib as any).ZstdCompress();
      out.zstdCallable = "no-throw";
    } catch (e: any) {
      out.zstdCallable = e.name;
    }
    // `options.info` reports the family transform for the modern codecs too.
    const brotliInfo: any = zlib.brotliCompressSync(interop, { info: true });
    out.brotliInfoEngine = brotliInfo.engine instanceof (zlib as any).BrotliCompress;
    const zstdInfo: any = zlib.zstdCompressSync(interop, { info: true });
    out.zstdInfoEngine = zstdInfo.engine instanceof (zlib as any).ZstdCompress;
    out.zstdInfoRoundtrip =
      zlib.zstdDecompressSync(zstdInfo.buffer, { info: true }).buffer.toString() === interop;
    // `params(level, strategy)` exists on the brotli pair and not on zstd.
    out.paramsOnBrotli = typeof (zlib as any).BrotliCompress.prototype.params;
    out.paramsOnZstd = typeof (zlib as any).ZstdCompress.prototype.params;
    // Modern-codec option window: chunkSize >= 64 (node's Z_MIN_CHUNK).
    try {
      zlib.brotliCompressSync(interop, { chunkSize: 1 });
      out.brotliChunkSize = "no-throw";
    } catch (e: any) {
      out.brotliChunkSize = { code: e.code, message: e.message };
    }

    globalThis.__zlibContract = out;
  });
};
"#;

const CRYPTO_CONTRACT_TS: &str = r#"
import crypto from "@node/crypto";
import assert from "node:assert";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- digests the host backend does not serve -------------------------
    out.sha224 = crypto.createHash("sha224").update("abc").digest("hex");
    out.sha384 = crypto.createHash("sha384").update("abc").digest("hex");
    out.sha3_256 = crypto.createHash("sha3-256").update("abc").digest("hex");
    out.sha3_384 = crypto.createHash("sha3-384").update("aoeu").digest("hex");
    out.sha3_512 = crypto.createHash("sha3-512").update("aoeu").digest("hex");
    // getHashes stays the host backend's four (tests/node_shims.rs pins that
    // list); the JS digests above are computed but not advertised yet.
    out.getHashes = crypto.getHashes().join(",");
    let unknownDigest: any;
    try {
      (crypto.createHash as any)("sha7");
      unknownDigest = "no-throw";
    } catch (e: any) {
      unknownDigest = { code: e.code, message: e.message };
    }
    out.unknownDigest = unknownDigest;
    let doubleDigest: any;
    try {
      const h = crypto.createHash("sha256").update("a");
      h.digest("hex");
      h.digest("hex");
      doubleDigest = "no-throw";
    } catch (e: any) {
      doubleDigest = e.code;
    }
    out.hashDoubleDigest = doubleDigest;

    // --- HMAC over the same digests + the dss1 alias ----------------------
    out.hmacSha224 = crypto.createHmac("sha224", "key").update("data").digest("hex");
    out.hmacSha384 = crypto.createHmac("sha384", "key").update("data").digest("hex");
    out.hmacSha3 = crypto.createHmac("sha3-512", "key").update("aoeu").digest("hex");
    out.hmacDss1 = crypto.createHmac("dss1", "key").update("data").digest("hex");
    out.hmacDss1Upper = crypto.createHmac("DSS1", "key").update("data").digest("hex");

    // stream form: end() then read()
    const streamed = crypto.createHmac("sha384", "key");
    (streamed as any).end(Buffer.from("data"));
    out.hmacStreamRead = (streamed as any).read().toString("hex");
    // a second digest() on an Hmac is the empty value (Hash throws, above)
    const reused = crypto.createHmac("sha1", "key").update("data");
    const firstDigest = reused.digest("hex");
    out.hmacSecondDigest = reused.digest("hex");
    out.hmacFirstDigestLength = firstDigest.length;

    // --- Hash as a stream -------------------------------------------------
    const hashStream = crypto.createHash("sha3-512");
    let streamedChunk: any = null;
    (hashStream as any).on("data", (chunk: any) => { streamedChunk = chunk; });
    (hashStream as any).setEncoding("hex");
    (hashStream as any).write("aoeu");
    (hashStream as any).end();
    out.hashStreamChunkIsDigest = streamedChunk === out.sha3_512;
    out.hashStreamDigestAfterEnd = (hashStream as any).digest("hex") === out.sha3_512;

    // --- createHmac validation -------------------------------------------
    const hmacErrors: any = {};
    const hmacCases: any[] = [
      ["nullAlgorithm", () => crypto.createHmac(null as any)],
      ["nullKey", () => crypto.createHmac("sha1", null as any)],
      ["unknownDigest", () => crypto.createHmac("sha7", "key")],
    ];
    for (const [name, run] of hmacCases) {
      try {
        run();
        hmacErrors[name] = "no-throw";
      } catch (e: any) {
        hmacErrors[name] = { code: e.code, name: e.name, message: e.message };
      }
    }
    out.hmacErrors = hmacErrors;
    // digest({toString}) must surface the encoding object's own error
    let digestThrow: any;
    try {
      crypto.createHmac("sha256", "key").digest({ toString: () => { throw new Error("boom"); } } as any);
      digestThrow = "no-throw";
    } catch (e: any) {
      digestThrow = { name: e.name, message: e.message };
    }
    out.hmacDigestThrow = digestThrow;
    out.secretKeyEqualsBuffer =
      crypto.createHmac("sha256", Buffer.alloc(0)).update("foo").digest("hex") ===
      crypto.createHmac("sha256", crypto.createSecretKey(Buffer.alloc(0))).update("foo").digest("hex");

    // --- cipher output encoding ------------------------------------------
    const mk = () => crypto.createCipheriv("aes-256-cbc", Buffer.alloc(32), Buffer.alloc(16));
    const cipherErrors: any = {};
    let cipher = mk();
    cipher.update("test", "utf-8", "utf-8");
    try {
      cipher.update("666f6f", "hex", "hex");
      cipherErrors.changeEncoding = "no-throw";
    } catch (e: any) {
      cipherErrors.changeEncoding = { message: e.message, code: e.code };
    }
    cipher = mk();
    cipher.update("test", "utf-8", "utf-8");
    try {
      (cipher as any).final("bad2");
      cipherErrors.finalUnknown = "no-throw";
    } catch (e: any) {
      cipherErrors.finalUnknown = { message: e.message, code: e.code };
    }
    cipher = mk();
    try {
      cipher.update("test", "utf-8", "bad3");
      cipherErrors.updateUnknown = "no-throw";
    } catch (e: any) {
      cipherErrors.updateUnknown = { message: e.message, code: e.code };
    }
    out.cipherErrors = cipherErrors;
    const enc = crypto.createCipheriv("aes-128-cbc", Buffer.alloc(16), Buffer.alloc(16));
    const dec = crypto.createDecipheriv("aes-128-cbc", Buffer.alloc(16), Buffer.alloc(16));
    out.cipherRoundTrip = dec.update(enc.update("data")) + dec.update(enc.final()).toString() +
      dec.final().toString();

    // --- WebCrypto surface behind globalThis.crypto.subtle ---------------
    const subtle: any = (globalThis as any).crypto.subtle;
    out.subtlePresent = typeof subtle === "object" && typeof subtle.importKey === "function";
    out.subtleIdentity = subtle === crypto.subtle;
    out.subtleDigest = Buffer.from(await subtle.digest("SHA-256", Buffer.from("abc"))).toString("hex");

    const a = await subtle.importKey("raw", Buffer.alloc(1, 0), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
    const b = await subtle.importKey("raw", Buffer.alloc(1, 1), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
    const same = await subtle.importKey("raw", Buffer.alloc(1, 0), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
    const diffExtractable = await subtle.importKey("raw", Buffer.alloc(1, 0), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
    const diffHash = await subtle.importKey("raw", Buffer.alloc(1, 0), { name: "HMAC", hash: "SHA-384" }, true, ["sign"]);
    const diffUsages = await subtle.importKey("raw", Buffer.alloc(1, 0), { name: "HMAC", hash: "SHA-256" }, true, ["verify"]);
    const distinct = (x: any, y: any) => {
      try {
        assert.deepStrictEqual(x, y);
        return false;
      } catch (e: any) {
        return e && e.code === "ERR_ASSERTION";
      }
    };
    out.cryptoKeyAlgorithm = a.algorithm;
    out.cryptoKeyType = a.type;
    out.cryptoKeyExtractable = a.extractable;
    out.cryptoKeyUsages = a.usages;
    out.cryptoKeyDistinct = {
      keyMaterial: distinct(a, b),
      extractable: distinct(a, diffExtractable),
      hash: distinct(a, diffHash),
      usages: distinct(a, diffUsages),
      identical: distinct(a, same),
    };
    const generated = await subtle.generateKey({ name: "HMAC", hash: "SHA-256", length: 256 }, true, ["sign", "verify"]);
    out.generatedKeyBits = generated.algorithm.length;
    out.generatedKeyBytes = generated._raw.length;
    out.keyObjectFrom = crypto.KeyObject.from(generated).export().length;
    out.cryptoKeyGlobal = typeof (globalThis as any).CryptoKey === "function";

    globalThis.__cryptoContract = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn zlib_tables_options_and_info_contract() {
    let (rt, v) = run_fixture("zlib-contract", ZLIB_CONTRACT_TS, "__zlibContract").await;

    // Table attribute shapes (test-zlib-const).
    assert_eq!(v["constantsProtoNull"], true, "{v}");
    assert_eq!(v["codesProtoNull"], true, "{v}");
    assert_eq!(v["codesFrozen"], true, "{v}");
    assert_eq!(v["constantWritable"], false, "{v}");
    assert_eq!(v["zMaxChunk"], true, "Z_MAX_CHUNK must be Infinity: {v}");
    assert_eq!(v["writes"]["constant"]["name"], "TypeError", "{v}");
    assert_eq!(v["writes"]["codeEntry"]["name"], "TypeError", "{v}");
    assert_eq!(v["writes"]["codesBinding"]["name"], "TypeError", "{v}");

    // require("zlib") hands out the module object itself, so `codes` keeps
    // Node's non-writable attribute instead of a flattened copy.
    let required = require_probe(
        &rt,
        "({ writable: Object.getOwnPropertyDescriptor(z, 'codes').writable, \
           constantsWritable: Object.getOwnPropertyDescriptor(z, 'constants').writable, \
           frozen: Object.isFrozen(z.codes), hasDefault: 'default' in z, \
           identical: z === z2, keys: Object.keys(z).length })",
    )
    .await;
    assert_eq!(required["writable"], false, "{required}");
    assert_eq!(required["constantsWritable"], false, "{required}");
    assert_eq!(required["frozen"], true, "{required}");
    assert_eq!(required["hasDefault"], false, "{required}");
    assert_eq!(required["identical"], true, "{required}");
    assert_eq!(required["keys"], 47, "Node's zlib exports: {required}");

    // CRC-32 argument validation (test-zlib-crc32).
    assert_eq!(
        v["crcDataCodes"],
        json!({
            "undefined": "ERR_INVALID_ARG_TYPE",
            "null": "ERR_INVALID_ARG_TYPE",
            "boolean": "ERR_INVALID_ARG_TYPE",
            "number": "ERR_INVALID_ARG_TYPE",
            "function": "ERR_INVALID_ARG_TYPE",
            "object": "ERR_INVALID_ARG_TYPE",
            // a bare ArrayBuffer is not a source Node's crc32 accepts
            "arrayBuffer": "ERR_INVALID_ARG_TYPE",
        }),
        "{v}"
    );
    assert_eq!(
        v["crcValueCodes"],
        json!({
            "null": "ERR_INVALID_ARG_TYPE",
            "boolean": "ERR_INVALID_ARG_TYPE",
            "function": "ERR_INVALID_ARG_TYPE",
            "object": "ERR_INVALID_ARG_TYPE",
        }),
        "{v}"
    );
    assert_eq!(v["crcKnown"], true, "the upstream crc32 table vector: {v}");

    // unzip sniffs both containers (test-zlib-from-string).
    assert_eq!(v["unzipGzip"], "blahblahblahblahblahblahblahblah", "{v}");
    assert_eq!(v["unzipZlib"], "blahblahblahblahblahblahblahblah", "{v}");
    assert_eq!(v["unzipDistinctBinding"], true, "{v}");

    // options.info (test-zlib-convenience-methods).
    assert_eq!(v["infoKeys"], json!(["buffer", "engine"]), "{v}");
    assert_eq!(v["infoEngine"], true, "{v}");
    assert_eq!(v["infoBuffer"], "blahblahblahblahblahblahblahblah", "{v}");
    assert_eq!(v["infoInflate"], true, "{v}");

    // Option ranges with Node's messages (test-zlib-deflate-constructors).
    assert_eq!(
        v["optionErrors"][0],
        json!({
            "options": { "chunkSize": "test" },
            "code": "ERR_INVALID_ARG_TYPE",
            "name": "TypeError",
            "message": "The \"options.chunkSize\" property must be of type number. Received type string ('test')",
        }),
        "{v}"
    );
    assert_eq!(v["optionErrors"][1]["code"], "ERR_OUT_OF_RANGE", "{v}");
    assert_eq!(
        v["optionErrors"][1]["message"],
        "The value of \"options.chunkSize\" is out of range. It must be a finite number. Received -Infinity",
        "{v}"
    );
    assert_eq!(
        v["optionErrors"][2]["message"],
        "The value of \"options.chunkSize\" is out of range. It must be >= 64. Received 0",
        "{v}"
    );
    assert_eq!(
        v["optionErrors"][3]["message"],
        "The value of \"options.windowBits\" is out of range. It must be >= 8 and <= 15. Received 0",
        "{v}"
    );
    assert_eq!(
        v["optionErrors"][4]["message"],
        "The value of \"options.level\" is out of range. It must be >= -1 and <= 9. Received -2",
        "{v}"
    );
    assert_eq!(
        v["optionErrors"][5]["message"],
        "The value of \"options.memLevel\" is out of range. It must be >= 1 and <= 9. Received 16",
        "{v}"
    );
    assert_eq!(
        v["optionErrors"][6]["message"],
        "The value of \"options.strategy\" is out of range. It must be >= 0 and <= 4. Received -2",
        "{v}"
    );
    assert_eq!(
        v["dictionaryError"],
        json!({
            "code": "ERR_INVALID_ARG_TYPE",
            "name": "TypeError",
            "message": "The \"options.dictionary\" property must be an instance of Buffer, TypedArray, DataView, or ArrayBuffer. Received type string ('not a buffer')",
        }),
        "{v}"
    );
    assert_eq!(
        v["paramErrors"][0],
        json!({
            "args": ["test"],
            "code": "ERR_INVALID_ARG_TYPE",
            "name": "TypeError",
            "message": "The \"level\" argument must be of type number. Received type string ('test')",
        }),
        "{v}"
    );
    assert_eq!(
        v["paramErrors"][1]["message"],
        "The value of \"level\" is out of range. It must be a finite number. Received -Infinity",
        "{v}"
    );
    assert_eq!(
        v["paramErrors"][2]["message"],
        "The value of \"level\" is out of range. It must be a finite number. Received Infinity",
        "{v}"
    );
    assert_eq!(
        v["paramErrors"][3]["message"],
        "The value of \"level\" is out of range. It must be >= -1 and <= 9. Received -2",
        "{v}"
    );
    assert_eq!(
        v["paramErrors"][4]["message"],
        "The \"strategy\" argument must be of type number. Received type string ('test')",
        "{v}"
    );
    assert_eq!(
        v["paramErrors"][6]["message"],
        "The value of \"strategy\" is out of range. It must be >= 0 and <= 4. Received -2",
        "{v}"
    );
    assert_eq!(
        v["deflateCallable"], true,
        "zlib.Deflate() without new: {v}"
    );
    assert_eq!(v["paramsReturn"], true, "params() returns undefined: {v}");
    assert_eq!(
        v["validStrategies"], true,
        "Z_FILTERED..Z_FIXED must pass: {v}"
    );
    assert_eq!(
        v["missingCallback"],
        json!({
            "code": "ERR_INVALID_ARG_TYPE",
            "message": "The \"callback\" argument must be of type function. Received undefined",
        }),
        "{v}"
    );

    // --- brotli / zstd: wire-format interop with node ---------------------
    assert_eq!(
        v["brotliFromNode"], "brotli interop 世界",
        "node-produced brotli stream must decode: {v}"
    );
    assert_eq!(
        v["zstdFromNode"], "brotli interop 世界",
        "node-produced zstd stream must decode: {v}"
    );
    assert_eq!(v["brotliSync"], "brotli interop 世界", "{v}");
    assert_eq!(v["zstdSync"], "brotli interop 世界", "{v}");
    assert_eq!(
        v["brotliCallable"], true,
        "BrotliCompress() without new: {v}"
    );
    assert_eq!(
        v["zstdCallable"], "TypeError",
        "ZstdCompress() without new must be a TypeError: {v}"
    );
    assert_eq!(v["brotliInfoEngine"], true, "brotli info engine: {v}");
    assert_eq!(v["zstdInfoEngine"], true, "zstd info engine: {v}");
    assert_eq!(v["zstdInfoRoundtrip"], true, "zstd info roundtrip: {v}");
    assert_eq!(
        v["paramsOnBrotli"], "function",
        "brotli keeps params(): {v}"
    );
    assert_eq!(v["paramsOnZstd"], "undefined", "zstd has no params(): {v}");
    assert_eq!(
        v["brotliChunkSize"],
        json!({
            "code": "ERR_OUT_OF_RANGE",
            "message": "The value of \"options.chunkSize\" is out of range. It must be >= 64. Received 1",
        }),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn crypto_digests_hmac_streams_and_subtle_contract() {
    let (_rt, v) = run_fixture("crypto-contract", CRYPTO_CONTRACT_TS, "__cryptoContract").await;

    // RFC 6234 / FIPS 202 vectors.
    assert_eq!(
        v["sha224"], "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
        "{v}"
    );
    assert_eq!(
        v["sha384"],
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
        "{v}"
    );
    assert_eq!(
        v["sha3_256"], "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532",
        "{v}"
    );
    assert_eq!(
        v["sha3_384"],
        "291668a355792aff3f5b763bd79cba985f30604b68fc2805c84075091fce3786893f21deb76557ddadbf8b2a34fb1463",
        "{v}"
    );
    assert_eq!(
        v["sha3_512"],
        "36a38a2a35e698974d4e5791a3f05b05198235381e864f91a0e8cd6a26b677ecdcde8e2b069bd7355fabd68abd6fc80119659f25e92f8efc961ee3a7c815c758",
        "{v}"
    );
    assert_eq!(
        v["getHashes"], "md5,sha1,sha256,sha512",
        "getHashes advertises the host backend's set: {v}"
    );
    assert_eq!(
        v["unknownDigest"],
        json!({ "code": "ERR_CRYPTO_INVALID_DIGEST", "message": "Invalid digest: sha7" }),
        "{v}"
    );
    assert_eq!(v["hashDoubleDigest"], "ERR_CRYPTO_HASH_FINALIZED", "{v}");

    assert_eq!(
        v["hmacSha224"], "19424d4210e50d7a4521b5f0d54b4b0cff3060deddccfd894fda5b3b",
        "{v}"
    );
    assert_eq!(
        v["hmacSha384"],
        "c5f97ad9fd1020c174d7dc02cf83c4c1bf15ee20ec555b690ad58e62da8a00ee44ccdb65cb8c80acfd127ebee568958a",
        "{v}"
    );
    assert_eq!(
        v["hmacSha3"],
        "a0ea6d6495b20dc8a1154fd175bbf6596310b0fbfd6b9ccb4c8fd91581b86e9d13d336bc1cd827e0b925d9cb050d99f392868ca39b44e802cc99125a9dddd717",
        "{v}"
    );
    // dss1 is SHA-1's historical alias in Node's digest table.
    assert_eq!(v["hmacDss1"], v["hmacDss1Upper"], "{v}");
    assert_eq!(
        v["hmacDss1"], "104152c5bfdca07bc633eebd46199f0255c9f49d",
        "HMAC-SHA1 of 'data' under key 'key': {v}"
    );
    assert_eq!(v["hmacStreamRead"], v["hmacSha384"], "{v}");
    assert_eq!(v["hmacFirstDigestLength"], 40, "{v}");
    assert_eq!(v["hmacSecondDigest"], "", "Hmac digest() resets: {v}");

    // Hash-as-stream (test-crypto-hash-stream-pipe).
    assert_eq!(v["hashStreamChunkIsDigest"], true, "{v}");
    assert_eq!(v["hashStreamDigestAfterEnd"], true, "{v}");

    assert_eq!(
        v["hmacErrors"]["nullAlgorithm"],
        json!({
            "code": "ERR_INVALID_ARG_TYPE",
            "name": "TypeError",
            "message": "The \"hmac\" argument must be of type string. Received null",
        }),
        "{v}"
    );
    assert_eq!(
        v["hmacErrors"]["nullKey"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["hmacErrors"]["unknownDigest"]["message"], "Invalid digest: sha7",
        "{v}"
    );
    assert_eq!(
        v["hmacDigestThrow"],
        json!({ "name": "Error", "message": "boom" }),
        "digest() must surface the encoding object's own error: {v}"
    );
    assert_eq!(v["secretKeyEqualsBuffer"], true, "{v}");

    // Cipher output encoding (test-crypto-encoding-validation-error).
    assert_eq!(
        v["cipherErrors"]["changeEncoding"]["message"], "Cannot change encoding",
        "{v}"
    );
    assert_eq!(
        v["cipherErrors"]["finalUnknown"],
        json!({ "message": "Unknown encoding: bad2", "code": "ERR_UNKNOWN_ENCODING" }),
        "{v}"
    );
    assert_eq!(
        v["cipherErrors"]["updateUnknown"],
        json!({ "message": "Unknown encoding: bad3", "code": "ERR_UNKNOWN_ENCODING" }),
        "{v}"
    );
    assert_eq!(v["cipherRoundTrip"], "data", "{v}");

    // WebCrypto behind the global (test-assert-deep's Crypto case).
    assert_eq!(v["subtlePresent"], true, "{v}");
    assert_eq!(
        v["subtleIdentity"], true,
        "global crypto.subtle is crypto.subtle: {v}"
    );
    assert_eq!(
        v["subtleDigest"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "{v}"
    );
    assert_eq!(
        v["cryptoKeyAlgorithm"],
        json!({ "name": "HMAC", "length": 8, "hash": { "name": "SHA-256" } }),
        "{v}"
    );
    assert_eq!(v["cryptoKeyType"], "secret", "{v}");
    assert_eq!(v["cryptoKeyExtractable"], true, "{v}");
    assert_eq!(v["cryptoKeyUsages"], json!(["sign"]), "{v}");
    assert_eq!(
        v["cryptoKeyDistinct"],
        json!({
            "keyMaterial": true,
            "extractable": true,
            "hash": true,
            "usages": true,
            "identical": false,
        }),
        "deepStrictEqual must separate different keys and join identical ones: {v}"
    );
    assert_eq!(v["generatedKeyBits"], 256, "{v}");
    assert_eq!(v["generatedKeyBytes"], 32, "{v}");
    assert_eq!(v["keyObjectFrom"], 32, "{v}");
    assert_eq!(v["cryptoKeyGlobal"], true, "{v}");
}
