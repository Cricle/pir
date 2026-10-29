//! node-fetch@3 over the node:http / node:stream pads: response-body chunk
//! delivery contract.
//!
//! Ecosystem census: `node-fetch@3` failed two ways against the same shim
//! behaviour —
//!
//!   * `fetch(url)` on a live loopback server hung (or threw
//!     `FetchError: Invalid response body ...: The "chunk" argument must be of
//!     type string or an instance of Buffer, TypedArray, or DataView`) while
//!     consuming the body, even though a plain `http.get` round trip was
//!     clean;
//!   * a refused connection was `catch`-ed by user code, but the process then
//!     died with `Unhandled promise rejection: not a function`.
//!
//! Three upstream contracts are pinned here (each was violated, each assertion
//! below is red before the corresponding fix):
//!
//!   1. `stream.pipeline(source, ..., destination, callback)` RETURNS THE
//!      DESTINATION STREAM (node's callback-form `node:stream.pipeline`; the
//!      promise-returning form is `node:stream/promises`). node-fetch's
//!      transport does `let body = pump(response_, new PassThrough(), cb)`
//!      (index.js:253) and consumes that value with
//!      `for await (const chunk of body)`; a Promise there is neither iterable
//!      nor the body stream.
//!   2. `Readable.from(value)` pushes a string/Buffer WHOLE through an
//!      objectMode Readable and yields every other iterable's ELEMENTS in
//!      objectMode (node's `lib/internal/streams/readable.js`). A byte-mode
//!      pad iterating a Buffer hands `push()` byte NUMBERS, which the pad
//!      rejects with exactly the census message.
//!   3. `ClientRequest.abort()` exists and is safe to call from an error
//!      handler (node's deprecated abort; node-fetch's `finalize()` calls it
//!      on every failure path). Missing, the TypeError lands inside the
//!      'error' listener and surfaces as an unhandled rejection after the
//!      caller already saw the FetchError.
//!
//! Fixtures run as extension modules in a per-test temp dir (repository
//! hygiene gate; same pattern as `tests/http_shims.rs`).

use std::net::TcpListener;
use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-body-chunks-{}-{test_name}",
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

/// A loopback port with nothing listening on it (bind + drop).
fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Every recorded chunk kind must be a Buffer (the runtime bridges response
/// bytes as Buffers; a number/plain object here means a `push()` type slip).
/// Segmentation is host-dependent, so only the kinds are asserted.
fn assert_all_chunks_are_buffers(v: &Value, key: &str) {
    let kinds = v[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} must be an array of chunk kinds: {v}"));
    assert!(!kinds.is_empty(), "{key} must not be empty: {v}");
    for kind in kinds {
        assert_eq!(kind, "buffer", "{key} chunk kind (want buffer): {v}");
    }
}

// ---------------------------------------------------------------------------
// 1 + 2: response body stream — node-fetch's `pump(res, new PassThrough(), cb)`
// transport shape, then `for await` + `Buffer.concat` over the returned stream
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn response_body_pipeline_returns_destination_stream() {
    let ts = r#"
import Stream, { PassThrough, pipeline as pump } from "node:stream";
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const server = http.createServer((_req: any, res: any) => {
      res.writeHead(200, { "content-type": "text/plain" });
      res.end("hello-body");
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    const url = "http://127.0.0.1:" + server.address().port + "/body";

    const kindOf = (chunk: any) => Buffer.isBuffer(chunk)
      ? "buffer"
      : (chunk instanceof Uint8Array ? "view" : typeof chunk);

    // (a) the plain client path: every chunk the body stream iterates out is a
    //     Buffer/Uint8Array, and the round trip reassembles byte-exactly.
    await new Promise((resolve: any, reject: any) => {
      const request = http.get(url, (res: any) => {
        (async () => {
          const chunks: any[] = [];
          for await (const chunk of res) chunks.push(chunk);
          out.plainChunkKinds = chunks.map(kindOf);
          out.plainText = Buffer.concat(chunks).toString("utf8");
        })().then(resolve, reject);
      });
      request.on("error", reject);
    });

    // (b) node-fetch's shape: the pipeline's return value IS the body stream.
    const body: any = await new Promise((resolve: any, reject: any) => {
      const request = http.get(url, (res: any) => {
        const pumped: any = pump(res, new PassThrough(), (error: any) => {
          out.pipelineCallbackCount = (out.pipelineCallbackCount || 0) + 1;
          out.pipelineCallbackError = error ? String(error.message) : null;
        });
        out.pumpReturnIsDestination = pumped instanceof PassThrough;
        out.pumpReturnIsReadable = pumped instanceof Stream.Readable;
        out.pumpReturnIsPromise = typeof pumped.then === "function";
        out.pumpHasAsyncIterator = typeof pumped[Symbol.asyncIterator] === "function";
        resolve(pumped);
      });
      request.on("error", reject);
    });

    const chunks: any[] = [];
    for await (const chunk of body) chunks.push(chunk);
    out.bodyChunkKinds = chunks.map(kindOf);
    out.bodyText = Buffer.concat(chunks).toString("utf8");
    out.bodyReadableEnded = body.readableEnded === true;
    await new Promise((resolve: any) => setTimeout(resolve, 20));

    // (c) node rejects a pipeline call whose trailing argument is not the
    //     callback (its `popCallback` validates `streams[stream.length - 1]`).
    try {
      (pump as any)(new PassThrough(), new PassThrough());
      out.noCallbackThrew = false;
    } catch (error: any) {
      out.noCallbackThrew = true;
      out.noCallbackCode = error.code;
    }

    server.close();
    globalThis.__pirBodyStreamOut = out;
  });
};
"#;
    let dir = write_temp_fixture("body-pipeline", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "body-pipeline", ts, "__pirBodyStreamOut").await;

    assert_all_chunks_are_buffers(&v, "plainChunkKinds");
    assert_eq!(
        v["plainText"], "hello-body",
        "plain http.get body round trip: {v}"
    );

    assert_eq!(
        v["pumpReturnIsDestination"], true,
        "node:stream.pipeline must return the destination stream (node's \
         callback-form contract; node-fetch's response body is that value): {v}"
    );
    assert_eq!(
        v["pumpReturnIsPromise"], false,
        "node:stream.pipeline must not hand back the stream/promises promise: {v}"
    );
    assert_eq!(
        v["pumpReturnIsReadable"], true,
        "the pipeline's return value must be a Readable: {v}"
    );
    assert_eq!(
        v["pumpHasAsyncIterator"], true,
        "the pipeline's return value must be async iterable: {v}"
    );

    assert_all_chunks_are_buffers(&v, "bodyChunkKinds");
    assert_eq!(
        v["bodyText"], "hello-body",
        "for await + Buffer.concat over the piped body: {v}"
    );
    assert_eq!(
        v["bodyReadableEnded"], true,
        "the piped body must reach 'end' (no premature close): {v}"
    );
    assert_eq!(
        v["pipelineCallbackCount"], 1,
        "the pipeline callback fires exactly once: {v}"
    );
    assert_eq!(
        v["pipelineCallbackError"],
        Value::Null,
        "the pipeline callback reports no error: {v}"
    );

    assert_eq!(
        v["noCallbackThrew"], true,
        "pipeline without the trailing callback is ERR_INVALID_ARG_TYPE: {v}"
    );
    assert_eq!(
        v["noCallbackCode"], "ERR_INVALID_ARG_TYPE",
        "pipeline's missing-callback error carries node's code: {v}"
    );
}

// ---------------------------------------------------------------------------
// 3: Readable.from — node's string/Buffer vs iterable chunk contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn readable_from_matches_node_chunk_contract() {
    let ts = r#"
import { Readable } from "node:stream";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const drain = async (stream: any) => {
      const chunks: any[] = [];
      for await (const chunk of stream) chunks.push(chunk);
      return chunks;
    };

    try {
      const stream = Readable.from(Buffer.from("ok"));
      out.fromBufferObjectMode = stream.readableObjectMode === true;
      const chunks = await drain(stream);
      out.fromBufferCount = chunks.length;
      out.fromBufferKinds = chunks.map((c: any) => (Buffer.isBuffer(c) ? "buffer" : typeof c));
      out.fromBufferText = Buffer.concat(chunks).toString("utf8");
    } catch (error: any) {
      out.fromBufferError = String(error && error.message);
    }

    try {
      const stream = Readable.from(["a", "b"]);
      out.fromArrayObjectMode = stream.readableObjectMode === true;
      const chunks = await drain(stream);
      out.fromArrayChunks = chunks;
    } catch (error: any) {
      out.fromArrayError = String(error && error.message);
    }

    try {
      const stream = Readable.from("str");
      out.fromStringObjectMode = stream.readableObjectMode === true;
      const chunks = await drain(stream);
      out.fromStringChunks = chunks;
    } catch (error: any) {
      out.fromStringError = String(error && error.message);
    }

    globalThis.__pirReadableFromOut = out;
  });
};
"#;
    let dir = write_temp_fixture("readable-from", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "readable-from", ts, "__pirReadableFromOut").await;

    // A Buffer is pushed whole (node iterates it as byte numbers otherwise,
    // which the byte-mode pad rejects with ERR_INVALID_ARG_TYPE).
    assert_eq!(
        v.get("fromBufferError"),
        None,
        "Readable.from(Buffer) must not throw: {v}"
    );
    assert_eq!(
        v["fromBufferObjectMode"], true,
        "Readable.from(Buffer) is an objectMode Readable: {v}"
    );
    assert_eq!(
        v["fromBufferCount"], 1,
        "Readable.from(Buffer) yields the Buffer as one chunk: {v}"
    );
    assert_eq!(
        v["fromBufferKinds"],
        json!(["buffer"]),
        "the single chunk is the Buffer itself: {v}"
    );
    assert_eq!(
        v["fromBufferText"], "ok",
        "Readable.from(Buffer) round trip: {v}"
    );

    assert_eq!(
        v.get("fromArrayError"),
        None,
        "Readable.from(array) must not throw: {v}"
    );
    assert_eq!(
        v["fromArrayObjectMode"], true,
        "Readable.from(iterable) is objectMode: {v}"
    );
    assert_eq!(
        v["fromArrayChunks"],
        json!(["a", "b"]),
        "Readable.from(iterable) yields the elements: {v}"
    );

    assert_eq!(
        v.get("fromStringError"),
        None,
        "Readable.from(string) must not throw: {v}"
    );
    assert_eq!(
        v["fromStringObjectMode"], true,
        "Readable.from(string) is objectMode: {v}"
    );
    assert_eq!(
        v["fromStringChunks"],
        json!(["str"]),
        "Readable.from(string) yields the string whole: {v}"
    );
}

// ---------------------------------------------------------------------------
// 4: refused connection — node-fetch's error handler + finalize() shape must
//    not leave an unhandled rejection behind
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn refused_request_finalize_leaves_no_unhandled_rejection() {
    let port = dead_port();
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { abortEvents: [] };
    await new Promise((resolve: any) => {
      // node-fetch index.js: `request_.on('error', ...)` rejects the fetch and
      // then calls `finalize()`, whose first statement is `request_.abort()`.
      const request = http.request("__PIR_DEAD_URL__/");
      request.on("abort", () => out.abortEvents.push("abort"));
      request.on("close", () => out.abortEvents.push("close"));
      request.on("error", (error: any) => {
        out.errorCode = error.code;
        try {
          out.abortType = typeof (request as any).abort;
          (request as any).abort();
          out.abortedFlag = (request as any).aborted === true;
          out.finalizeOk = true;
        } catch (abortError: any) {
          out.finalizeError = String(abortError && abortError.message);
        }
        resolve(null);
      });
      request.end();
    });
    await new Promise((resolve: any) => setTimeout(resolve, 30));
    globalThis.__pirRefusedOut = out;
  });
};
"#
    .replace("__PIR_DEAD_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("refused-finalize", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "refused-finalize", &ts, "__pirRefusedOut").await;

    assert_eq!(v["errorCode"], "ECONNREFUSED", "refused connect: {v}");
    assert_eq!(
        v["abortType"], "function",
        "ClientRequest.abort must exist (node's deprecated abort): {v}"
    );
    assert_eq!(
        v["abortedFlag"], true,
        "abort() marks the request aborted, like node: {v}"
    );
    assert_eq!(
        v.get("finalizeError"),
        None,
        "finalize()'s request_.abort() must not throw inside the 'error' \
         listener: {v}"
    );
    assert_eq!(
        v["finalizeOk"], true,
        "node-fetch's finalize() shape completes: {v}"
    );
    // node reports close then abort; inside an 'error' emit the pad's destroy
    // has not returned yet, so the pair may arrive in the other order — what
    // matters is that abort() reports itself and never an 'error'.
    let mut events: Vec<String> = v["abortEvents"]
        .as_array()
        .unwrap_or_else(|| panic!("abortEvents must be an array: {v}"))
        .iter()
        .map(|event| event.as_str().unwrap_or_default().to_string())
        .collect();
    events.sort();
    assert_eq!(
        events,
        vec!["abort".to_string(), "close".to_string()],
        "abort() reports abort + close, like node: {v}"
    );

    // The regression symptom: the rejected promise the caller catch-ed was
    // clean, but the TypeError from the missing `abort` escaped as an
    // unhandled rejection (and `pir js` exits 1 on one).
    rt.wait_idle().await;
    let rejections = rt.take_unhandled_rejections().await;
    assert!(
        rejections.is_empty(),
        "the refused-request error path must not leave unhandled rejections: {rejections:?}"
    );
}
