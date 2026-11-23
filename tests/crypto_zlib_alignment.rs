//! Node v24.22.1 public-API alignment for `node:crypto` / `node:zlib`.
//!
//! This is the pin for the alignment wave that filled the remaining surface
//! gaps the audit found in `docs/node-test-suite-audit.md`:
//!
//! - crypto: SHAKE128/256 + `options.outputLength`, Node's `createHash`
//!   unknown-digest error (plain `Error`), constructor-time key/IV validation
//!   for `createCipheriv`, the Node cipher-state error shapes, HKDF/scrypt
//!   argument validation and error codes, and `generateKeyPair*` argument
//!   errors (the operation itself is still unsupported).
//! - zlib: `_processChunk`, `reset()`, flush/finishFlush option ranges with
//!   per-family maxima, `params(level, strategy, callback)`, brotli/zstd
//!   `params` validation (`ERR_BROTLI_INVALID_PARAM` /
//!   `ERR_ZSTD_INVALID_PARAM`), Map-valued params, truncated brotli/zstd
//!   streams as `Z_BUF_ERROR`, and the fact that convenience methods ignore
//!   `options.encoding`.
//!
//! Every expected value/message below was captured from
//! `node.exe v24.22.1` on the same inputs (see the probe scripts referenced in
//! the task report).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-crypto-zlib-align-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
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

async fn run_fixture(name: &str, ts: &str, global: &str) -> Value {
    let dir = write_temp_fixture(name);
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
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

const ALIGNMENT_TS: &str = r#"
import crypto from "@node/crypto";
import zlib from "@node/zlib";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    function grab(fn: any) {
      try {
        const value = fn();
        if (Buffer.isBuffer(value)) return { hex: value.toString("hex"), len: value.length };
        if (value instanceof ArrayBuffer) {
          return { hex: Buffer.from(value).toString("hex"), len: value.byteLength };
        }
        return { ok: value === undefined ? "undefined" : value };
      } catch (e: any) {
        return { name: e.name, code: e.code, message: e.message };
      }
    }

    // --- getHashes / SHAKE outputLength -----------------------------------
    out.hashes = crypto.getHashes().filter((h: string) => h.indexOf("shake") === 0);
    // Node: unknown createHash digest is a plain Error with no `code`.
    out.hashUnknown = grab(() => crypto.createHash("sha7"));
    out.shake8 = grab(() => crypto.createHash("shake256", { outputLength: 8 }).update("x").digest("hex"));
    out.shakeDefault = grab(() => crypto.createHash("shake256").update("x").digest("hex"));
    out.shakeCopyDefault = grab(() => crypto.createHash("shake256", { outputLength: 0 }).copy().update("x").digest("hex"));
    out.shakeCopy8 = grab(() => crypto.createHash("shake128", { outputLength: 0 }).copy({ outputLength: 5 }).update("").digest("hex"));
    out.fixedWrongLen = grab(() => crypto.createHash("sha256", { outputLength: 28 }));
    out.fixedRightLen = grab(() => crypto.createHash("sha224", { outputLength: 28 }).digest("hex"));
    out.lenFrac = grab(() => crypto.createHash("shake256", { outputLength: 1.5 }));
    out.lenType = grab(() => crypto.createHash("shake256", { outputLength: "8" }));
    out.oneShotShake = grab(() => crypto.hash("shake128", "", { outputLength: 5 }));

    // --- cipher construction validation -----------------------------------
    out.cipherUnknown = grab(() => crypto.createCipheriv("aes-999-cbc", Buffer.alloc(16), Buffer.alloc(16)));
    out.cipherKeyLen = grab(() => crypto.createCipheriv("aes-256-cbc", Buffer.alloc(16), Buffer.alloc(16)));
    out.cipherIvLen = grab(() => crypto.createCipheriv("aes-128-cbc", Buffer.alloc(16), Buffer.alloc(8)));
    out.cipherIvNull = grab(() => crypto.createCipheriv("aes-128-cbc", Buffer.alloc(16), null));
    out.cipherAlgoType = grab(() => crypto.createCipheriv(42, Buffer.alloc(16), Buffer.alloc(16)));
    out.cipherGcmIv16 = grab(() => crypto.createCipheriv("aes-128-gcm", Buffer.alloc(16), Buffer.alloc(16)) instanceof crypto.Cipheriv);
    out.cipherEncChange = grab(() => {
      const c = crypto.createCipheriv("aes-128-cbc", Buffer.alloc(16), Buffer.alloc(16));
      c.update("a", undefined, "hex");
      return c.final("base64");
    });
    out.cipherUpdateAfterFinal = grab(() => {
      const c = crypto.createCipheriv("aes-128-cbc", Buffer.alloc(16), Buffer.alloc(16));
      c.final();
      return c.update("a");
    });
    out.gcmBadTag = grab(() => {
      const d = crypto.createDecipheriv("aes-128-gcm", Buffer.alloc(16), Buffer.alloc(12));
      d.update("x");
      return d.final();
    });
    out.authTagBeforeFinal = grab(() => crypto.createCipheriv("aes-128-gcm", Buffer.alloc(16), Buffer.alloc(12)).getAuthTag());
    // test-crypto-hmac: digest() after the stream path finalized is empty,
    // while the stream itself produced the digest.
    out.hmacAfterStream = grab(() => {
      const h = crypto.createHmac("sha256", "key");
      h.end("some data to hash");
      const streamed = h.read();
      return { digestLen: h.digest().length, streamed: Buffer.isBuffer(streamed) && streamed.length === 32 };
    });

    // --- hkdf / scrypt ------------------------------------------------------
    out.hkdf = grab(() => crypto.hkdfSync(
      "sha256",
      Buffer.from("0b".repeat(22), "hex"),
      Buffer.from("000102030405060708090a0b0c", "hex"),
      Buffer.from("f0f1f2f3f4f5f6f7f8f9", "hex"),
      42));
    out.hkdfBadDigest = grab(() => crypto.hkdfSync("foo", Buffer.from("i"), Buffer.from("s"), Buffer.from("f"), 16));
    out.hkdfLong = grab(() => crypto.hkdfSync("sha256", Buffer.from("i"), Buffer.from("s"), Buffer.from("f"), 255 * 32 + 1));
    out.hkdfNeg = grab(() => crypto.hkdfSync("sha256", Buffer.from("i"), Buffer.from("s"), Buffer.from("f"), -1));
    out.hkdfSaltNull = grab(() => crypto.hkdfSync("sha256", Buffer.from("i"), null, Buffer.from("f"), 16));
    out.scryptN3 = grab(() => crypto.scryptSync("p", "s", 32, { N: 3, r: 1, p: 1 }));
    out.scryptKeyStr = grab(() => crypto.scryptSync("p", "s", "8"));
    out.scryptKeyNeg = grab(() => crypto.scryptSync("p", "s", -1));
    out.scryptBadSalt = grab(() => crypto.scryptSync("p", 42, 8));
    out.scryptMaxmem = grab(() => crypto.scryptSync("p", "s", 32, { N: 2, r: 1, p: 1, maxmem: 1 }));

    // --- generateKeyPair argument errors ----------------------------------
    out.genPairNum = grab(() => crypto.generateKeyPairSync(42));
    out.genPairFoo = grab(() => crypto.generateKeyPairSync("foo"));
    out.genPairNoCb = grab(() => crypto.generateKeyPair(42, {}));

    // --- zlib internals -----------------------------------------------------
    out.pcGzipHex = grab(() => new zlib.Gzip()._processChunk(Buffer.from("hello"), zlib.constants.Z_FINISH).toString("hex"));
    out.pcRound = grab(() => new zlib.Gunzip()
      ._processChunk(new zlib.Gzip()._processChunk(Buffer.from("hello"), zlib.constants.Z_FINISH), zlib.constants.Z_FINISH)
      .toString());
    out.pcNoFlush = grab(() => new zlib.Deflate()._processChunk(Buffer.from("hello"), zlib.constants.Z_NO_FLUSH).toString("hex"));
    out.pcBrotli = grab(() => new zlib.BrotliCompress()
      ._processChunk(Buffer.from("hello"), zlib.constants.BROTLI_OPERATION_FINISH).length > 0);
    out.resetBeforeWrite = grab(() => {
      const d = new zlib.Deflate();
      const r = d.reset();
      return { returns: r === undefined, ended: d._ended, chunks: d._chunks.length };
    });
    out.resetHandle = grab(() => new zlib.Deflate()._handle.reset());
    out.resetDuringWrite = grab(() => {
      const d = new zlib.Deflate();
      d.write("x");
      return d._handle.reset();
    });
    out.paramsCallback = grab(() => new zlib.Deflate().params(1, 1, () => {}));
    out.flushBadKind = grab(() => new zlib.Deflate().flush(99, () => {}));
    out.flushString = grab(() => new zlib.Deflate().flush("x"));
    out.flushOption = grab(() => new zlib.Deflate({ flush: 99 }));
    out.finishFlushOption = grab(() => new zlib.Deflate({ finishFlush: 99 }));
    out.zstdFlushMax = grab(() => new zlib.ZstdCompress({ flush: 3 }));
    out.brotliFlushMax = grab(() => new zlib.BrotliCompress({ flush: 4 }));
    out.brotliBadKey = grab(() => zlib.brotliCompressSync("x", { params: { 99999: 1 } }));
    out.brotliBadValue = grab(() => zlib.brotliCompressSync("x", { params: { 1: "5" } }));
    out.zstdBadKey = grab(() => zlib.zstdCompressSync("x", { params: { 99999: 1 } }));
    out.zstdBadValue = grab(() => zlib.zstdCompressSync("x", { params: { 100: "1" } }));
    out.brotliMap = grab(() => zlib.brotliDecompressSync(
      zlib.brotliCompressSync("hello", { params: new Map([[1, 5]]) })).toString());
    out.zstdLevelClamp = grab(() => zlib.zstdCompressSync("hello", { params: { 100: 99 } }).length > 0);
    out.truncBrotli = grab(() => zlib.brotliDecompressSync(
      zlib.brotliCompressSync("hello").subarray(0, -4)));
    out.truncZstd = grab(() => zlib.zstdDecompressSync(
      zlib.zstdCompressSync("hello").subarray(0, -4)));
    out.encodingIgnored = (() => {
      const a = zlib.deflateSync("deadbeef", { encoding: "hex" });
      const b = zlib.deflateSync("deadbeef");
      return a.equals(b);
    })();
    out.maxOutput = grab(() => zlib.deflateSync("hello world", { maxOutputLength: 1 }));

    globalThis.__alignment = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn crypto_zlib_public_api_alignment_contract() {
    let v = run_fixture("api-alignment", ALIGNMENT_TS, "__alignment").await;

    assert_eq!(v["hashes"], json!(["shake128", "shake256"]), "{v}");
    assert_eq!(
        v["hashUnknown"],
        json!({ "name": "Error", "message": "Digest method not supported" }),
        "{v}"
    );
    assert_eq!(v["shake8"], json!({ "ok": "c851121abe809597" }), "{v}");
    assert_eq!(
        v["shakeDefault"],
        json!({ "ok": "c851121abe8095973c9bdc1b446089f3249cf74e0b64660b61d73bcae0350f4d" }),
        "{v}"
    );
    assert_eq!(
        v["shakeCopyDefault"], v["shakeDefault"],
        "copy() resets the XOF length: {v}"
    );
    assert_eq!(v["shakeCopy8"], json!({ "ok": "7f9c2ba4e8" }), "{v}");
    assert_eq!(
        v["fixedWrongLen"],
        json!({
            "name": "Error",
            "code": "ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH",
            "message": "error:030000B2:digital envelope routines::not XOF or invalid length",
        }),
        "{v}"
    );
    assert_eq!(
        v["fixedRightLen"],
        json!({ "ok": "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f" }),
        "{v}"
    );
    assert_eq!(v["lenFrac"]["code"], "ERR_OUT_OF_RANGE", "{v}");
    assert_eq!(
        v["lenFrac"]["message"],
        "The value of \"options.outputLength\" is out of range. It must be an integer. Received 1.5",
        "{v}"
    );
    assert_eq!(
        v["lenType"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_TYPE",
            "message": "The \"options.outputLength\" property must be of type number. Received type string ('8')",
        }),
        "{v}"
    );
    assert_eq!(v["oneShotShake"], json!({ "ok": "7f9c2ba4e8" }), "{v}");

    assert_eq!(
        v["cipherUnknown"],
        json!({ "name": "Error", "code": "ERR_CRYPTO_UNKNOWN_CIPHER", "message": "Unknown cipher" }),
        "{v}"
    );
    assert_eq!(
        v["cipherKeyLen"],
        json!({ "name": "RangeError", "code": "ERR_CRYPTO_INVALID_KEYLEN", "message": "Invalid key length" }),
        "{v}"
    );
    assert_eq!(
        v["cipherIvLen"],
        json!({ "name": "TypeError", "code": "ERR_CRYPTO_INVALID_IV", "message": "Invalid initialization vector" }),
        "{v}"
    );
    assert_eq!(v["cipherIvNull"]["code"], "ERR_CRYPTO_INVALID_IV", "{v}");
    assert_eq!(
        v["cipherAlgoType"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_TYPE",
            "message": "The \"cipher\" argument must be of type string. Received type number (42)",
        }),
        "{v}"
    );
    assert_eq!(v["cipherGcmIv16"], json!({ "ok": true }), "{v}");
    assert_eq!(
        v["cipherEncChange"],
        json!({
            "name": "TypeError",
            "code": "ERR_INVALID_ARG_VALUE",
            "message": "The argument 'outputEncoding' cannot be changed from 'hex'. Received 'base64'",
        }),
        "{v}"
    );
    assert_eq!(
        v["cipherUpdateAfterFinal"],
        json!({ "name": "Error", "message": "Trying to add data in unsupported state" }),
        "{v}"
    );
    assert_eq!(
        v["gcmBadTag"],
        json!({ "name": "Error", "message": "Unsupported state or unable to authenticate data" }),
        "{v}"
    );
    assert_eq!(
        v["authTagBeforeFinal"],
        json!({ "name": "Error", "code": "ERR_CRYPTO_INVALID_STATE", "message": "Invalid state for operation getAuthTag" }),
        "{v}"
    );
    assert_eq!(
        v["hmacAfterStream"],
        json!({ "ok": { "digestLen": 0, "streamed": true } }),
        "{v}"
    );

    assert_eq!(
        v["hkdf"],
        json!({ "hex": "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865", "len": 42 }),
        "RFC 5869 case 1: {v}"
    );
    assert_eq!(
        v["hkdfBadDigest"],
        json!({ "name": "TypeError", "code": "ERR_CRYPTO_INVALID_DIGEST", "message": "Invalid digest: foo" }),
        "{v}"
    );
    assert_eq!(
        v["hkdfLong"],
        json!({ "name": "RangeError", "code": "ERR_CRYPTO_INVALID_KEYLEN", "message": "Invalid key length" }),
        "{v}"
    );
    assert_eq!(
        v["hkdfNeg"]["message"],
        "The value of \"length\" is out of range. It must be >= 0 && <= 9007199254740991. Received -1",
        "{v}"
    );
    assert_eq!(v["hkdfSaltNull"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(
        v["scryptN3"],
        json!({ "name": "RangeError", "code": "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", "message": "Invalid scrypt params" }),
        "{v}"
    );
    assert_eq!(
        v["scryptKeyStr"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_TYPE", "message": "The \"keylen\" argument must be of type number. Received type string ('8')" }),
        "{v}"
    );
    assert_eq!(
        v["scryptKeyNeg"]["message"],
        "The value of \"keylen\" is out of range. It must be >= 0 && <= 2147483647. Received -1",
        "{v}"
    );
    assert_eq!(v["scryptBadSalt"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(
        v["scryptMaxmem"]["code"], "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
        "{v}"
    );

    assert_eq!(
        v["genPairNum"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_TYPE", "message": "The \"type\" argument must be of type string. Received type number (42)" }),
        "{v}"
    );
    assert_eq!(
        v["genPairFoo"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_VALUE", "message": "The argument 'type' must be a supported key type. Received 'foo'" }),
        "{v}"
    );
    assert_eq!(
        v["genPairNoCb"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_TYPE", "message": "The \"callback\" argument must be of type function. Received undefined" }),
        "{v}"
    );

    assert_eq!(v["pcRound"], json!({ "ok": "hello" }), "{v}");
    assert_eq!(v["pcNoFlush"], json!({ "ok": "789c" }), "{v}");
    // `_processChunk(...).toString("hex")` is a string, so `grab` reports it
    // under `ok` (the `hex` shape is only for Buffer/ArrayBuffer returns).
    assert!(
        v["pcGzipHex"]["ok"]
            .as_str()
            .is_some_and(|hex| hex.starts_with("1f8b")),
        "{v}"
    );
    assert_eq!(v["pcBrotli"], json!({ "ok": true }), "{v}");
    assert_eq!(
        v["resetBeforeWrite"],
        json!({ "ok": { "returns": true, "ended": false, "chunks": 0 } }),
        "{v}"
    );
    assert_eq!(v["resetHandle"], json!({ "ok": "undefined" }), "{v}");
    assert_eq!(
        v["resetDuringWrite"],
        json!({ "name": "Error", "message": "Cannot reset zlib stream while a write is in progress" }),
        "{v}"
    );
    assert_eq!(v["paramsCallback"], json!({ "ok": "undefined" }), "{v}");
    assert_eq!(
        v["flushBadKind"]["message"],
        "The value of \"kind\" is out of range. It must be >= 0 and <= 5. Received 99",
        "{v}"
    );
    assert_eq!(v["flushString"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(
        v["flushOption"]["message"],
        "The value of \"options.flush\" is out of range. It must be >= 0 and <= 5. Received 99",
        "{v}"
    );
    assert_eq!(
        v["finishFlushOption"]["message"],
        "The value of \"options.finishFlush\" is out of range. It must be >= 0 and <= 5. Received 99",
        "{v}"
    );
    assert_eq!(
        v["zstdFlushMax"]["message"],
        "The value of \"options.flush\" is out of range. It must be >= 0 and <= 2. Received 3",
        "{v}"
    );
    assert_eq!(
        v["brotliFlushMax"]["message"],
        "The value of \"options.flush\" is out of range. It must be >= 0 and <= 3. Received 4",
        "{v}"
    );
    assert_eq!(
        v["brotliBadKey"],
        json!({ "name": "RangeError", "code": "ERR_BROTLI_INVALID_PARAM", "message": "99999 is not a valid Brotli parameter" }),
        "{v}"
    );
    assert_eq!(
        v["brotliBadValue"],
        json!({ "name": "TypeError", "code": "ERR_INVALID_ARG_TYPE", "message": "The \"options.params[key]\" property must be of type number. Received type string ('5')" }),
        "{v}"
    );
    assert_eq!(
        v["zstdBadKey"],
        json!({ "name": "RangeError", "code": "ERR_ZSTD_INVALID_PARAM", "message": "99999 is not a valid zstd parameter" }),
        "{v}"
    );
    assert_eq!(v["zstdBadValue"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(v["brotliMap"], json!({ "ok": "hello" }), "{v}");
    assert_eq!(v["zstdLevelClamp"], json!({ "ok": true }), "{v}");
    assert_eq!(
        v["truncBrotli"],
        json!({ "name": "Error", "code": "Z_BUF_ERROR", "message": "unexpected end of file" }),
        "{v}"
    );
    assert_eq!(
        v["truncZstd"],
        json!({ "name": "Error", "code": "Z_BUF_ERROR", "message": "unexpected end of file" }),
        "{v}"
    );
    assert_eq!(
        v["encodingIgnored"], true,
        "options.encoding is ignored by convenience methods: {v}"
    );
    assert_eq!(
        v["maxOutput"],
        json!({ "name": "RangeError", "code": "ERR_BUFFER_TOO_LARGE", "message": "Cannot create a Buffer larger than 1 bytes" }),
        "{v}"
    );
}
