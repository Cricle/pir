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
use std::time::Instant;

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
        allow_process_exit: false,
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
import { EventEmitter } from "@node/events";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- the wider digest set ---------------------------------------------
    out.sha224 = crypto.createHash("sha224").update("abc").digest("hex");
    out.sha384 = crypto.createHash("sha384").update("abc").digest("hex");
    out.sha3_256 = crypto.createHash("sha3-256").update("abc").digest("hex");
    out.sha3_384 = crypto.createHash("sha3-384").update("aoeu").digest("hex");
    out.sha3_512 = crypto.createHash("sha3-512").update("aoeu").digest("hex");
    // Encoded digests are rendered host-side for hex/base64/base64url; the
    // "buffer" spelling still returns the raw bytes.
    out.digestBase64 = crypto.createHash("sha256").update("abc").digest("base64");
    out.digestBase64url = crypto.createHash("sha256").update("abc").digest("base64url");
    out.digestBufferIsBuffer = Buffer.isBuffer(crypto.createHash("sha256").update("abc").digest("buffer"));
    // Multi-chunk update() streams through the Rust state (no concatenation)
    // and must equal the one-shot digest.
    out.multiChunk = crypto.createHash("sha256").update("a").update("b").update("c").digest("hex");
    out.multiChunkMatchesOneShot = crypto.createHash("sha256").update("abc").digest("hex") === out.multiChunk;
    // copy() clones a live streaming state, not just buffered chunks.
    const copySource = crypto.createHash("sha256").update("a").update("b");
    const copyClone = copySource.copy();
    copySource.update("c");
    out.copySourceDigest = copySource.digest("hex");
    out.copyCloneDigest = copyClone.update("c").digest("hex");
    const freshHash = crypto.createHash("sha256");
    const freshClone = freshHash.copy();
    freshHash.update("abc");
    freshClone.update("abc");
    out.copyFreshMatches = freshClone.digest("hex") === freshHash.digest("hex");
    // The lazy-EventEmitter prototype link still satisfies the stream class
    // relationship Node exposes.
    const streamIdentity = crypto.createHash("sha256");
    out.hashIsEventEmitter = streamIdentity instanceof EventEmitter;
    out.hashHasOn = typeof streamIdentity.on === "function";
    out.hmacMultiChunk = crypto.createHmac("sha256", "key").update("da").update("ta").digest("base64");
    // update() snapshots its input: mutating the caller's buffer afterwards
    // must not change the digest (Node ingests the bytes immediately).
    const mutation = Buffer.from("abc");
    const mutationHash = crypto.createHash("sha256").update(mutation);
    mutation.fill(0x78);
    out.updateSnapshot = mutationHash.digest("hex");
    // Small-chunk -> large-chunk and large-chunk -> small-chunk transitions
    // (the large one enters the streaming state) must both match a single
    // concatenated digest.
    const partSmall = Buffer.alloc(4096, 0x61);
    const partLarge = Buffer.alloc(70 * 1024, 0x62);
    const mixedForward = crypto.createHash("sha256").update(partSmall).update(partLarge).digest("hex");
    const mixedForwardRef = crypto.createHash("sha256").update(Buffer.concat([partSmall, partLarge])).digest("hex");
    const mixedReverse = crypto.createHash("sha256").update(partLarge).update(partSmall).digest("hex");
    const mixedReverseRef = crypto.createHash("sha256").update(Buffer.concat([partLarge, partSmall])).digest("hex");
    out.mixedChunkOrder =
      mixedForward === mixedForwardRef && mixedReverse === mixedReverseRef;
    out.uuidShape = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID());
    const randomErrors: any = {};
    try {
      crypto.randomBytes(-1);
    } catch (e: any) {
      randomErrors.negative = { name: e.name, code: e.code, message: e.message };
    }
    try {
      (crypto.randomBytes as any)("x");
    } catch (e: any) {
      randomErrors.string = { name: e.name, code: e.code, message: e.message };
    }
    out.randomByteErrors = randomErrors;
    // getHashes advertises every host-backed digest (pinned here and in
    // tests/node_shims.rs).
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

/// Same-profile microbenchmark for the crypto hot paths. Ignored by default,
/// and a no-op unless `PIR_CRYPTO_BENCH=1`, so the normal (even
/// `--include-ignored`) suite runs do not pay for timing loops. Prints one
/// `BENCH <name> <ns/op>` line per operation.
#[tokio::test(flavor = "current_thread")]
#[ignore = "timing probe: set PIR_CRYPTO_BENCH=1 to run"]
async fn crypto_hot_path_bench() {
    if std::env::var("PIR_CRYPTO_BENCH").ok().as_deref() != Some("1") {
        println!("[skip] set PIR_CRYPTO_BENCH=1 to run the crypto timing probe");
        return;
    }
    let dir = write_temp_fixture("crypto-bench", &[]);
    let rt = new_runtime_at(&dir).await;

    async fn measure(rt: &ExtensionRuntime, name: &str, body: &str, iters: u32) {
        let script = format!(
            "(() => {{ const dir = globalThis.process.cwd(); \
               const crypto = globalThis.__pirRequireFrom(dir, \"crypto\"); \
               const small = Buffer.alloc(4096, 0x61); \
               const big = Buffer.alloc(1024 * 1024, 0x62); \
               let sink = 0; const n = {iters}; \
               for (let i = 0; i < n; i++) {{ {body} }} \
               return JSON.stringify(sink); }})()"
        );
        // Warm up the IC/parse paths before timing the loop.
        let _ = rt.eval_json(&script).await;
        let started = Instant::now();
        let value = rt.eval_json(&script).await.expect("bench script");
        let elapsed = started.elapsed().as_secs_f64();
        println!("BENCH {name} {:.1}", elapsed * 1e9 / f64::from(iters));
        std::hint::black_box(value);
    }

    measure(
        &rt,
        "sha256-4k-hex",
        "sink += crypto.createHash(\"sha256\").update(small).digest(\"hex\").length;",
        20_000,
    )
    .await;
    measure(
        &rt,
        "sha256-4k-raw",
        "sink += crypto.createHash(\"sha256\").update(small).digest().length;",
        20_000,
    )
    .await;
    measure(
        &rt,
        "hmac-4k-b64",
        "sink += crypto.createHmac(\"sha256\", \"key\").update(small).digest(\"base64\").length;",
        20_000,
    )
    .await;
    measure(
        &rt,
        "hmac-4k-raw",
        "sink += crypto.createHmac(\"sha256\", \"key\").update(small).digest().length;",
        20_000,
    )
    .await;
    measure(&rt, "uuid", "sink += crypto.randomUUID().length;", 50_000).await;
    measure(
        &rt,
        "random-32",
        "sink += crypto.randomBytes(32).length;",
        50_000,
    )
    .await;
    measure(
        &rt,
        "sha256-1m",
        "sink += crypto.createHash(\"sha256\").update(big).digest().length;",
        200,
    )
    .await;
    // Component probes (same-profile attribution for the fixed cost).
    measure(
        &rt,
        "probe-buffer-from-4k",
        "sink += Buffer.from(small).length;",
        40_000,
    )
    .await;
    measure(
        &rt,
        "probe-concat-4k",
        "sink += Buffer.concat([small]).length;",
        40_000,
    )
    .await;
    measure(
        &rt,
        "probe-hex32",
        "sink += Buffer.alloc(32).toString(\"hex\").length;",
        40_000,
    )
    .await;
    measure(
        &rt,
        "probe-hash-ctor",
        "sink += crypto.createHash(\"sha256\") === null ? 1 : 0;",
        40_000,
    )
    .await;
    measure(&rt, "probe-hash-ctor-update", "const h = crypto.createHash(\"sha256\"); h.update(small); sink += h._handle === null ? 1 : 0;", 40_000).await;
    measure(
        &rt,
        "probe-native-digest",
        "sink += globalThis.__pirCryptoDigest(\"sha256\", small).byteLength;",
        40_000,
    )
    .await;
    measure(
        &rt,
        "probe-native-random",
        "sink += globalThis.__pirCryptoRandom(32).byteLength;",
        40_000,
    )
    .await;
    measure(&rt, "probe-native-new", "const h = globalThis.__pirCryptoHashNew(\"sha256\"); sink += h; globalThis.__pirCryptoHashRelease(h);", 40_000).await;
    measure(
        &rt,
        "probe-copy-4k",
        "sink += new Uint8Array(small).byteLength;",
        40_000,
    )
    .await;
    measure(
        &rt,
        "probe-slice-4k",
        "sink += small.slice().byteLength;",
        40_000,
    )
    .await;
    measure(&rt, "probe-native-new-update", "const h = globalThis.__pirCryptoHashNew(\"sha256\"); globalThis.__pirCryptoHashUpdate(h, small); globalThis.__pirCryptoHashRelease(h); sink += h;", 40_000).await;
    measure(&rt, "probe-native-final-raw", "const h = globalThis.__pirCryptoHashNew(\"sha256\"); globalThis.__pirCryptoHashUpdate(h, small); sink += globalThis.__pirCryptoHashFinal(h).byteLength;", 40_000).await;
    measure(&rt, "probe-native-final-hex", "const h = globalThis.__pirCryptoHashNew(\"sha256\"); globalThis.__pirCryptoHashUpdate(h, small); sink += globalThis.__pirCryptoHashFinal(h, \"hex\").length;", 40_000).await;
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
    // Node's encodings for SHA-256("abc") and the streaming/copy contracts.
    assert_eq!(
        v["digestBase64"], "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
        "{v}"
    );
    assert_eq!(
        v["digestBase64url"], "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0",
        "{v}"
    );
    assert_eq!(v["digestBufferIsBuffer"], true, "{v}");
    assert_eq!(
        v["multiChunk"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "{v}"
    );
    assert_eq!(v["multiChunkMatchesOneShot"], true, "{v}");
    assert_eq!(
        v["copySourceDigest"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "{v}"
    );
    assert_eq!(v["copyCloneDigest"], v["copySourceDigest"], "{v}");
    assert_eq!(v["copyFreshMatches"], true, "{v}");
    assert_eq!(v["hashIsEventEmitter"], true, "{v}");
    assert_eq!(v["hashHasOn"], true, "{v}");
    assert_eq!(
        v["hmacMultiChunk"], "UDH+PZicbRU3oBP6bnOdojRj/a7DtwE32Cjjas4iG9A=",
        "{v}"
    );
    assert_eq!(
        v["updateSnapshot"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "update() must snapshot its input: {v}"
    );
    assert_eq!(v["mixedChunkOrder"], true, "{v}");
    assert_eq!(v["uuidShape"], true, "randomUUID format: {v}");
    assert_eq!(
        v["randomByteErrors"]["negative"],
        json!({
            "name": "RangeError",
            "code": "ERR_OUT_OF_RANGE",
            "message": "The value of \"size\" is out of range. It must be >= 0 && <= 2147483647. Received -1",
        }),
        "{v}"
    );
    assert_eq!(
        v["randomByteErrors"]["string"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_TYPE",
            "message": "The \"size\" argument must be of type number. Received type string ('x')",
        }),
        "{v}"
    );
    assert_eq!(
        v["getHashes"],
        "md5,sha1,sha224,sha256,sha384,sha512,sha3-224,sha3-256,sha3-384,sha3-512,shake128,shake256",
        "getHashes advertises every host-backed digest: {v}"
    );
    // Node's Hash constructor reports an unknown digest as a plain Error with
    // no `code` and this exact text (createHmac keeps
    // ERR_CRYPTO_INVALID_DIGEST, checked below).
    assert_eq!(
        v["unknownDigest"],
        json!({ "message": "Digest method not supported" }),
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

    // Cipher output encoding (test-crypto-encoding-validation-error): Node
    // reports the StringDecoder's normalized label ("utf8", not "utf-8").
    assert_eq!(
        v["cipherErrors"]["changeEncoding"]["message"],
        "The argument 'outputEncoding' cannot be changed from 'utf8'. Received 'hex'",
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

// ---------------------------------------------------------------------------
// Node v24.22.1 parity for the crypto surface the read-only differential
// audit flagged: ArrayBufferView handling in `update()`, the string decoder
// rules, one-shot `crypto.hash()` output, and the random/RNG validation.
//
// Every expectation below was captured from node v24.22.1 running the same
// script; no node is needed at test time. The fixtures stay whole-file
// (rather than per-assertion) so the test pins the exact observable JSON.
// ---------------------------------------------------------------------------

const CRYPTO_VIEW_ENCODING_PARITY_TS: &str = r#"
import crypto from "@node/crypto";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    function grab(fn: any) {
      try {
        const r = fn();
        return { ok: Buffer.isBuffer(r) ? "buffer:" + r.toString("hex") : String(r) };
      } catch (e: any) {
        return { name: e.name, code: e.code, message: e.message };
      }
    }
    const sha = (d: any) => crypto.createHash("sha256").update(d).digest("hex");

    // --- ArrayBufferView / ArrayBuffer inputs -----------------------------
    out.dataViewOnly = sha(new DataView(new Uint8Array([0x64]).buffer));
    out.dataViewOffset = sha(new DataView(new Uint8Array([0x61, 0x62, 0x63]).buffer, 1, 1));
    out.dataViewMixed = crypto.createHash("sha256")
      .update("a")
      .update(Buffer.from("b"))
      .update(new Uint8Array([0x63]))
      .update(new DataView(new Uint8Array([0x64]).buffer))
      .digest("hex");
    out.arrayBuffer = grab(() => crypto.createHash("sha256").update(new Uint8Array([1]).buffer));
    out.sharedArrayBuffer = grab(() => crypto.createHash("sha256").update(new SharedArrayBuffer(1)));
    out.numberInput = grab(() => crypto.createHash("sha256").update(42));

    // --- update(data, encoding) decoder rules -----------------------------
    out.oddHex = grab(() => crypto.createHash("sha256").update("abc", "hex"));
    out.badHex = crypto.createHash("sha256").update("zz", "hex").digest("hex");
    out.badBase64 = crypto.createHash("sha256").update("!!!!", "base64").digest("hex");
    out.unknownEncodingTreatsAsUtf8 =
      crypto.createHash("sha256").update("\u00e9\u2603", "bogus").digest("hex") === sha("\u00e9\u2603");
    out.latin1 = crypto.createHash("sha256").update("\u00e9", "latin1").digest("hex");

    // --- one-shot crypto.hash(algorithm, data, outputEncoding) -------------
    out.hashDefault = [typeof crypto.hash("sha256", "abc"), crypto.hash("sha256", "abc")];
    out.hashBuffer = Buffer.isBuffer(crypto.hash("sha256", "abc", "buffer"));
    out.hashObjectBuffer = Buffer.isBuffer(crypto.hash("sha256", "abc", { outputEncoding: "buffer" }));
    out.hashBadEncoding = grab(() => crypto.hash("sha256", "abc", "bogus"));
    out.hashArrayBuffer = grab(() => crypto.hash("sha256", new Uint8Array([97]).buffer));
    out.hashBadAlgorithm = grab(() => crypto.hash("nope", "abc"));
    out.hashDataView = crypto.hash("sha256", new DataView(new Uint8Array([97]).buffer));

    // --- finalized state ---------------------------------------------------
    const h = crypto.createHash("sha256").update("z");
    h.digest();
    out.updateAfterDigest = grab(() => h.update("x"));
    out.copyAfterDigest = grab(() => h.copy());

    // --- timingSafeEqual error shapes --------------------------------------
    out.timingSafeLength = grab(() => crypto.timingSafeEqual(Buffer.from("a"), Buffer.from("bb")));
    out.timingSafeType = grab(() => crypto.timingSafeEqual("a", "a"));
    out.timingSafeView = crypto.timingSafeEqual(
      new DataView(new Uint8Array([1, 2]).buffer), new Uint8Array([1, 2]));

    // --- streaming-handle regressions (must stay green) --------------------
    out.hmacStream = (() => {
      const hm = crypto.createHmac("sha256", "key").update("da").update("ta");
      return hm.digest("base64");
    })();
    out.hashStream = (() => {
      const hs = crypto.createHash("sha3-512");
      let chunk: any = null;
      hs.on("data", (c: any) => { chunk = c; });
      hs.setEncoding("hex");
      hs.write("aoeu");
      hs.end();
      const want = crypto.createHash("sha3-512").update("aoeu").digest("hex");
      return { chunk: chunk === want, afterEnd: hs.digest("hex") === want };
    })();
    out.copyState = (() => {
      const src = crypto.createHash("sha256").update("a").update("b");
      const clone = src.copy();
      src.update("c");
      return { src: src.digest("hex"), clone: clone.update("c").digest("hex") };
    })();
    out.updateSnapshot = (() => {
      const buf = Buffer.from("abc");
      const hh = crypto.createHash("sha256").update(buf);
      buf.fill(0x78);
      return hh.digest("hex");
    })();

    globalThis.__cryptoViewEncoding = out;
  });
};
"#;

const CRYPTO_RANDOM_PARITY_TS: &str = r#"
import crypto from "@node/crypto";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    function grab(fn: any) {
      try {
        return { ok: fn() };
      } catch (e: any) {
        return { name: e.name, code: e.code, message: e.message };
      }
    }

    // --- randomInt overloads + validation ---------------------------------
    out.intOneArg = grab(() => {
      const n = crypto.randomInt(5);
      return Number.isSafeInteger(n) && n >= 0 && n < 5;
    });
    out.intRange = grab(() => {
      const n = crypto.randomInt(2, 4);
      return n === 2 || n === 3;
    });
    out.intFloat = grab(() => crypto.randomInt(1.5, 3));
    out.intBig = grab(() => crypto.randomInt(1n, 3n));
    out.intNaN = grab(() => crypto.randomInt(NaN, 3));
    out.intMaxEq = grab(() => crypto.randomInt(5, 5));
    out.intRangeBig = grab(() => crypto.randomInt(0, 2 ** 48 + 1));
    out.intRangeBigOneArg = grab(() => crypto.randomInt(2 ** 48 + 1));
    out.intThirdArg = grab(() => crypto.randomInt(0, 10, 3));
    out.intFourArgs = grab(() => crypto.randomInt(5, 6, 7, () => {}));
    out.intMaxSafe = grab(() => {
      const n = crypto.randomInt(Number.MAX_SAFE_INTEGER - 1, Number.MAX_SAFE_INTEGER);
      return n === Number.MAX_SAFE_INTEGER - 1;
    });

    // --- getRandomValues types and the 64 KiB quota -----------------------
    out.gvInteger = grab(() => {
      const a = new Uint8Array(4);
      return crypto.getRandomValues(a) === a && a.byteLength === 4;
    });
    out.gvBigInt = grab(() => {
      const a = new BigInt64Array(2);
      return crypto.getRandomValues(a) === a && a.byteLength === 16;
    });
    out.gvFloat64 = grab(() => crypto.getRandomValues(new Float64Array(2)));
    out.gvFloat32 = grab(() => crypto.getRandomValues(new Float32Array(2)));
    out.gvDataView = grab(() => crypto.getRandomValues(new DataView(new ArrayBuffer(4))));
    out.gvArrayBuffer = grab(() => crypto.getRandomValues(new ArrayBuffer(4)));
    out.gvPlainArray = grab(() => crypto.getRandomValues([1, 2]));
    out.gvNull = grab(() => crypto.getRandomValues(null));
    out.gvQuota = grab(() => crypto.getRandomValues(new Uint8Array(65537)));
    out.gvUint16Quota = grab(() => crypto.getRandomValues(new Uint16Array(32769)));
    out.gvMax = grab(() => crypto.getRandomValues(new Uint8Array(65536)).byteLength);

    // --- randomFillSync bounds, ArrayBuffer/SharedArrayBuffer, DataView -----
    out.rfOffset = grab(() => { const b = Buffer.alloc(2); crypto.randomFillSync(b, 3); return 0; });
    out.rfNegOffset = grab(() => { const b = Buffer.alloc(4); crypto.randomFillSync(b, -1); return 0; });
    out.rfSize = grab(() => { const b = Buffer.alloc(4); crypto.randomFillSync(b, 0, 5); return 0; });
    out.rfU16Size = grab(() => { const b = new Uint16Array(4); crypto.randomFillSync(b, 0, 5); return 0; });
    out.rfArrayBuffer = grab(() => {
      const b = new ArrayBuffer(4);
      return crypto.randomFillSync(b, 1, 2) === b;
    });
    out.rfSharedArrayBuffer = grab(() => {
      const b = new SharedArrayBuffer(4);
      return crypto.randomFillSync(b, 1, 2) === b;
    });
    out.rfDataView = grab(() => {
      const b = new DataView(new ArrayBuffer(4));
      return crypto.randomFillSync(b, 1, 2) === b;
    });
    out.rfNaN = grab(() => { const b = Buffer.alloc(4); crypto.randomFillSync(b, NaN); return 0; });
    out.rfPlain = grab(() => crypto.randomFillSync([1, 2]));
    out.rfNoArg = grab(() => crypto.randomFillSync());
    out.rfStringOffset = grab(() => { const b = Buffer.alloc(4); crypto.randomFillSync(b, "1"); return 0; });

    // --- randomBytes digit grouping / randomUUID option validation ---------
    out.randomBytesHuge = grab(() => crypto.randomBytes(Number.MAX_SAFE_INTEGER));
    out.uuidBad = grab(() => crypto.randomUUID("bad"));
    out.uuidFlagBad = grab(() => crypto.randomUUID({ disableEntropyCache: 1 }));
    out.uuidNoCache = grab(() => crypto.randomUUID({ disableEntropyCache: true }).length);

    globalThis.__cryptoRandomParity = out;
  });
};
"#;

#[tokio::test]
async fn crypto_shim_view_encoding_and_oneshot_hash_parity() {
    let (_rt, v) = run_fixture(
        "crypto-view-encoding",
        CRYPTO_VIEW_ENCODING_PARITY_TS,
        "__cryptoViewEncoding",
    )
    .await;

    // DataView-only input used to hash zero bytes; any view is now its own
    // byte range.
    assert_eq!(
        v["dataViewOnly"], "18ac3e7343f016890c510e93f935261169d9e3f565436429830faf0934f4f8e4",
        "DataView-only update() must hash its bytes: {v}"
    );
    assert_eq!(
        v["dataViewOffset"], "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d",
        "a DataView must respect byteOffset/byteLength: {v}"
    );
    assert_eq!(
        v["dataViewMixed"], "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589",
        "string/Buffer/Uint8Array/DataView chunks hash as \"abcd\": {v}"
    );
    assert_eq!(
        v["arrayBuffer"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_TYPE",
            "message": "The \"data\" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received an instance of ArrayBuffer",
        }),
        "{v}"
    );
    assert_eq!(
        v["sharedArrayBuffer"]["message"],
        "The \"data\" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received an instance of SharedArrayBuffer",
        "{v}"
    );
    assert_eq!(
        v["numberInput"]["message"],
        "The \"data\" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received type number (42)",
        "{v}"
    );

    // String decoding: odd hex throws up front, an invalid digit run decodes
    // to empty exactly like Buffer.from, an unknown encoding falls back to
    // UTF-8 (not ERR_UNKNOWN_ENCODING).
    assert_eq!(
        v["oddHex"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_VALUE",
            "message": "The argument 'encoding' is invalid for data of length 3. Received 'hex'",
        }),
        "{v}"
    );
    assert_eq!(
        v["badHex"], "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "invalid hex decodes what it can (nothing): {v}"
    );
    assert_eq!(
        v["badBase64"], "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "invalid base64 decodes what it can (nothing): {v}"
    );
    assert_eq!(v["unknownEncodingTreatsAsUtf8"], true, "{v}");
    assert_eq!(
        v["latin1"], "de2e331d891ae267a7009cb45b4e8830f170e0c937288ea2731a1941c7a53b0d",
        "latin1 decodes to one byte: {v}"
    );

    // One-shot hash defaults to hex; "buffer"/an options object keep bytes.
    assert_eq!(
        v["hashDefault"],
        json!([
            "string",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ]),
        "crypto.hash() must default to hex: {v}"
    );
    assert_eq!(v["hashBuffer"], true, "{v}");
    assert_eq!(v["hashObjectBuffer"], true, "{v}");
    assert_eq!(
        v["hashBadEncoding"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_VALUE",
            "message": "The argument 'outputEncoding' is invalid. Received 'bogus'",
        }),
        "{v}"
    );
    assert_eq!(
        v["hashArrayBuffer"]["message"],
        "The \"input\" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received an instance of ArrayBuffer",
        "{v}"
    );
    assert_eq!(v["hashBadAlgorithm"]["name"], "Error", "{v}");
    assert_eq!(
        v["hashBadAlgorithm"]["message"], "Digest method nope is not supported",
        "oneShotDigest's message differs from createHash: {v}"
    );
    assert_eq!(
        v["hashDataView"], "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb",
        "crypto.hash accepts DataView: {v}"
    );

    assert_eq!(
        v["updateAfterDigest"],
        json!({
            "name": "Error",
            "code": "ERR_CRYPTO_HASH_FINALIZED",
            "message": "Digest already called",
        }),
        "{v}"
    );
    assert_eq!(
        v["copyAfterDigest"],
        json!({
            "name": "Error",
            "code": "ERR_CRYPTO_HASH_FINALIZED",
            "message": "Digest already called",
        }),
        "{v}"
    );

    assert_eq!(
        v["timingSafeLength"],
        json!({
            "name": "RangeError",
            "code": "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH",
            "message": "Input buffers must have the same byte length",
        }),
        "{v}"
    );
    assert_eq!(
        v["timingSafeType"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_TYPE",
            "message": "The \"buf1\" argument must be an instance of ArrayBuffer, Buffer, TypedArray, or DataView.",
        }),
        "{v}"
    );
    assert_eq!(v["timingSafeView"], true, "{v}");

    // Existing streaming/copy behaviour must remain intact.
    assert_eq!(
        v["hmacStream"], "UDH+PZicbRU3oBP6bnOdojRj/a7DtwE32Cjjas4iG9A=",
        "{v}"
    );
    assert_eq!(
        v["hashStream"],
        json!({ "chunk": true, "afterEnd": true }),
        "{v}"
    );
    assert_eq!(
        v["copyState"],
        json!({
            "src": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "clone": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }),
        "{v}"
    );
    assert_eq!(
        v["updateSnapshot"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "{v}"
    );
}

#[tokio::test]
async fn crypto_shim_random_int_and_rng_validation_parity() {
    let (_rt, v) = run_fixture(
        "crypto-random-parity",
        CRYPTO_RANDOM_PARITY_TS,
        "__cryptoRandomParity",
    )
    .await;

    // randomInt(max) is the single-argument overload, not an error.
    assert_eq!(v["intOneArg"], json!({ "ok": true }), "{v}");
    assert_eq!(v["intRange"], json!({ "ok": true }), "{v}");
    assert_eq!(v["intMaxSafe"], json!({ "ok": true }), "{v}");

    let type_error = |message: &str| json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_TYPE", "message": message });
    let out_of_range = |message: &str| json!({ "name": "RangeError", "code": "ERR_OUT_OF_RANGE", "message": message });
    assert_eq!(
        v["intFloat"],
        type_error("The \"min\" argument must be a safe integer. Received type number (1.5)"),
        "{v}"
    );
    assert_eq!(
        v["intBig"],
        type_error("The \"min\" argument must be a safe integer. Received type bigint (1n)"),
        "bigint bounds are rejected with the `n` suffix: {v}"
    );
    assert_eq!(
        v["intNaN"],
        type_error("The \"min\" argument must be a safe integer. Received type number (NaN)"),
        "{v}"
    );
    assert_eq!(
        v["intMaxEq"],
        out_of_range("The value of \"max\" is out of range. It must be greater than the value of \"min\" (5). Received 5"),
        "{v}"
    );
    assert_eq!(
        v["intRangeBig"],
        out_of_range("The value of \"max - min\" is out of range. It must be <= 281474976710655. Received 281_474_976_710_657"),
        "{v}"
    );
    assert_eq!(
        v["intRangeBigOneArg"],
        out_of_range("The value of \"max\" is out of range. It must be <= 281474976710655. Received 281_474_976_710_657"),
        "the one-arg form names the parameter \"max\": {v}"
    );
    assert_eq!(
        v["intThirdArg"],
        type_error("The \"callback\" argument must be of type function. Received type number (3)"),
        "{v}"
    );
    assert_eq!(
        v["intFourArgs"],
        type_error("The \"callback\" argument must be of type function. Received type number (7)"),
        "{v}"
    );

    let type_mismatch = json!({
        "name": "TypeMismatchError",
        "code": 17,
        "message": "The data argument must be an integer-type TypedArray",
    });
    assert_eq!(v["gvInteger"], json!({ "ok": true }), "{v}");
    assert_eq!(v["gvBigInt"], json!({ "ok": true }), "{v}");
    assert_eq!(v["gvFloat64"], type_mismatch, "{v}");
    assert_eq!(v["gvFloat32"], type_mismatch, "{v}");
    assert_eq!(v["gvDataView"], type_mismatch, "{v}");
    assert_eq!(v["gvArrayBuffer"], type_mismatch, "{v}");
    assert_eq!(v["gvPlainArray"], type_mismatch, "{v}");
    assert_eq!(v["gvNull"], type_mismatch, "{v}");
    let quota = json!({
        "name": "QuotaExceededError",
        "code": 22,
        "message": "The requested length exceeds 65,536 bytes",
    });
    assert_eq!(v["gvQuota"], quota, "{v}");
    assert_eq!(v["gvUint16Quota"], quota, "{v}");
    assert_eq!(v["gvMax"], json!({ "ok": 65536 }), "{v}");

    assert_eq!(
        v["rfOffset"],
        out_of_range(
            "The value of \"offset\" is out of range. It must be >= 0 && <= 2. Received 3"
        ),
        "{v}"
    );
    assert_eq!(
        v["rfNegOffset"],
        out_of_range(
            "The value of \"offset\" is out of range. It must be >= 0 && <= 4. Received -1"
        ),
        "{v}"
    );
    assert_eq!(
        v["rfSize"],
        out_of_range("The value of \"size + offset\" is out of range. It must be <= 4. Received 5"),
        "{v}"
    );
    assert_eq!(
        v["rfU16Size"],
        out_of_range(
            "The value of \"size + offset\" is out of range. It must be <= 8. Received 10"
        ),
        "elementSize applies to the Uint16Array bounds: {v}"
    );
    assert_eq!(v["rfArrayBuffer"], json!({ "ok": true }), "{v}");
    assert_eq!(v["rfSharedArrayBuffer"], json!({ "ok": true }), "{v}");
    assert_eq!(v["rfDataView"], json!({ "ok": true }), "{v}");
    assert_eq!(
        v["rfNaN"],
        out_of_range(
            "The value of \"offset\" is out of range. It must be >= 0 && <= 4. Received NaN"
        ),
        "{v}"
    );
    assert_eq!(
        v["rfPlain"],
        type_error("The \"buf\" argument must be an instance of ArrayBuffer or ArrayBufferView. Received an instance of Array"),
        "{v}"
    );
    assert_eq!(
        v["rfNoArg"],
        type_error("The \"buf\" argument must be an instance of ArrayBuffer or ArrayBufferView. Received undefined"),
        "{v}"
    );
    assert_eq!(
        v["rfStringOffset"],
        type_error("The \"offset\" argument must be of type number. Received type string ('1')"),
        "{v}"
    );
    assert_eq!(
        v["randomBytesHuge"],
        out_of_range("The value of \"size\" is out of range. It must be >= 0 && <= 2147483647. Received 9_007_199_254_740_991"),
        "{v}"
    );
    assert_eq!(
        v["uuidBad"],
        type_error("The \"options\" argument must be of type object. Received type string ('bad')"),
        "{v}"
    );
    assert_eq!(
        v["uuidFlagBad"],
        type_error("The \"options.disableEntropyCache\" property must be of type boolean. Received type number (1)"),
        "{v}"
    );
    assert_eq!(v["uuidNoCache"], json!({ "ok": 36 }), "{v}");
}
