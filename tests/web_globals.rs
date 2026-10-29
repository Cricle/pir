//! Web primitive globals (LLRT tier) — TextEncoder/TextDecoder, the abort /
//! events family, the minimal Web streams and the global `fetch` surface over
//! the `__pir_host.http` bridge.
//!
//! Fixture extensions are generated at runtime into per-test temp dirs: the
//! repository hygiene gate forbids tracked `.ts` files (same pattern as
//! `tests/extensions_runtime.rs`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-web-globals-{}-{test_name}",
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

async fn new_runtime() -> ExtensionRuntime {
    ExtensionRuntime::new(test_config())
        .await
        .expect("runtime construction")
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
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

// ---------------------------------------------------------------------------
// Globals installation + non-shadowing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn web_globals_are_installed() {
    let rt = new_runtime().await;
    let v = rt
        .eval_json(
            "JSON.stringify({
              fetch: typeof fetch,
              Request: typeof Request,
              Response: typeof Response,
              Headers: typeof Headers,
              ReadableStream: typeof ReadableStream,
              WritableStream: typeof WritableStream,
              TransformStream: typeof TransformStream,
              TextEncoder: typeof TextEncoder,
              TextDecoder: typeof TextDecoder,
              Event: typeof Event,
              EventTarget: typeof EventTarget,
              AbortSignal: typeof AbortSignal,
              AbortController: typeof AbortController,
              httpBridge: typeof globalThis.__pir_host.http,
              bufferKind: Buffer.isBuffer(Buffer.alloc(1)),
            })",
        )
        .await
        .expect("eval globals");
    for key in [
        "fetch",
        "Request",
        "Response",
        "Headers",
        "ReadableStream",
        "WritableStream",
        "TransformStream",
        "TextEncoder",
        "TextDecoder",
        "Event",
        "EventTarget",
        "AbortSignal",
        "AbortController",
    ] {
        assert_eq!(v[key], "function", "global {key} must be a function: {v}");
    }
    assert_eq!(
        v["httpBridge"], "function",
        "__pir_host.http must exist: {v}"
    );
    assert_eq!(v["bufferKind"], true, "Buffer must stay intact: {v}");
}

/// The engine-global rule (HANDOFF.md §3): every install is `??`-guarded, so
/// re-evaluating `globals_source()` (and generally any prior global) must not
/// replace an existing value.
#[tokio::test(flavor = "current_thread")]
async fn web_globals_do_not_shadow_existing() {
    let rt = new_runtime().await;
    // Set sentinels, then re-run the whole injection source.
    let v = rt
        .eval_json(
            "globalThis.TextDecoder = 12345; globalThis.fetch = 54321; \
             (function () {})(); \
             JSON.stringify([globalThis.TextDecoder, globalThis.fetch])",
        )
        .await
        .expect("set sentinels");
    assert_eq!(v, json!([12345, 54321]), "sanity: sentinels set");
    // Re-inject: the guarded snippets must leave the sentinels alone while
    // the other globals keep working.
    let source = pir::extensions::api::node_shims::globals_source();
    rt.eval_json(&format!("{source}\nJSON.stringify([typeof TextEncoder, globalThis.TextDecoder, globalThis.fetch, typeof AbortController])"))
        .await
        .expect("re-eval globals source");
    let v = rt
        .eval_json(
            "JSON.stringify([globalThis.TextDecoder, globalThis.fetch, typeof TextEncoder, typeof AbortController])",
        )
        .await
        .expect("check sentinels");
    assert_eq!(
        v,
        json!([12345, 54321, "function", "function"]),
        "?? guards must never overwrite an existing global: {v}"
    );
}

// ---------------------------------------------------------------------------
// TextEncoder / TextDecoder
// ---------------------------------------------------------------------------

const TEXT_CODEC_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const encoder = new TextEncoder();
    const text = "héllo 世界 \u{1F600}"; // 2-byte, 3-byte, 4-byte utf-8 in one string
    const bytes = encoder.encode(text);
    out.encoding = encoder.encoding;
    out.byteLength = bytes.length;
    out.roundTrip = new TextDecoder().decode(bytes) === text;

    // Cross-chunk streaming: cut inside every multi-byte sequence boundary.
    const decoder = new TextDecoder();
    let streamed = "";
    for (let i = 0; i < bytes.length; i += 3) {
      streamed += decoder.decode(bytes.slice(i, i + 3), { stream: true });
    }
    streamed += decoder.decode();
    out.crossChunk = streamed === text;

    // BOM is stripped once per stream unless ignoreBOM.
    const bom = new Uint8Array([0xef, 0xbb, 0xbf, 0x61, 0x62]);
    out.bomStripped = new TextDecoder().decode(bom) === "ab";
    out.bomKept = new TextDecoder("utf-8", { ignoreBOM: true }).decode(bom).charCodeAt(0) === 0xfeff;

    // fatal mode throws on invalid bytes; non-fatal emits U+FFFD.
    const bad = new Uint8Array([0xff, 0x61]);
    try {
      new TextDecoder("utf-8", { fatal: true }).decode(bad);
      out.fatalThrows = false;
    } catch (e: any) {
      out.fatalThrows = e instanceof TypeError;
    }
    out.replacement = new TextDecoder().decode(bad) === "\uFFFDa";

    // Label set + unknown label.
    out.latin1 = new TextDecoder("latin1").decode(new Uint8Array([0xe9])) === "\u00e9";
    out.asciiAlias = new TextDecoder("ascii").decode(new Uint8Array([0x41])) === "A";
    out.utf16le = new TextDecoder("utf-16le").decode(new Uint8Array([0x41, 0x00, 0x42, 0x00])) === "AB";
    try {
      new TextDecoder("no-such-charset");
      out.unknownLabelThrows = false;
    } catch (e: any) {
      out.unknownLabelThrows = e instanceof RangeError;
    }

    // encodeInto: partial fill respects the target capacity.
    const target = new Uint8Array(4);
    const written = encoder.encodeInto("héllo", target);
    out.encodeInto = written.written === 4 && written.read >= 1;
    out.encodeIntoBytes = Array.from(target);

    globalThis.__textCodec = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn text_encoder_decoder_semantics() {
    let dir = write_temp_fixture("text-codec", &[("index.ts", TEXT_CODEC_TS)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "textcodec", TEXT_CODEC_TS, "__textCodec").await;

    assert_eq!(v["encoding"], "utf-8", "TextEncoder.encoding: {v}");
    assert_eq!(v["roundTrip"], true, "encode/decode round trip: {v}");
    assert_eq!(v["crossChunk"], true, "utf-8 cross-chunk streaming: {v}");
    assert_eq!(v["bomStripped"], true, "BOM must be stripped: {v}");
    assert_eq!(v["bomKept"], true, "ignoreBOM must keep the BOM: {v}");
    assert_eq!(
        v["fatalThrows"], true,
        "fatal mode must throw TypeError: {v}"
    );
    assert_eq!(v["replacement"], true, "non-fatal U+FFFD: {v}");
    assert_eq!(v["latin1"], true, "latin1 decode: {v}");
    assert_eq!(v["asciiAlias"], true, "ascii label alias: {v}");
    assert_eq!(v["utf16le"], true, "utf-16le decode: {v}");
    assert_eq!(
        v["unknownLabelThrows"], true,
        "unknown label RangeError: {v}"
    );
    assert_eq!(v["encodeInto"], true, "encodeInto capacity handling: {v}");
    assert_eq!(
        v["encodeIntoBytes"],
        json!([104, 195, 169, 108]),
        "encodeInto bytes: {v}"
    );
}

// ---------------------------------------------------------------------------
// EventTarget / AbortController
// ---------------------------------------------------------------------------

const ABORT_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // EventTarget: synchronous dispatch, registration order, once + remove.
    const target = new EventTarget();
    const calls: string[] = [];
    const listenerB = () => calls.push("b");
    const listenerA = () => calls.push("a");
    const onceL = () => calls.push("once");
    target.addEventListener("ping", listenerB);
    target.addEventListener("ping", listenerA);
    target.addEventListener("ping", onceL, { once: true });
    target.dispatchEvent(new Event("ping"));
    target.removeEventListener("ping", listenerB);
    target.dispatchEvent(new Event("ping"));
    out.orderAndOnce = calls.join(",") === "b,a,once,a";
    const event = new Event("ping", { cancelable: true });
    target.addEventListener("ping", (e: any) => e.preventDefault());
    out.preventDefault = target.dispatchEvent(event) === false;
    out.defaultPrevented = event.defaultPrevented === true;

    // onabort property handler also fires.
    let onabortFired = false;
    const ac0 = new AbortController();
    (ac0.signal as any).onabort = () => { onabortFired = true; };
    ac0.abort();
    out.onabort = onabortFired;

    // AbortController: abort event + reason + throwIfAborted.
    const controller = new AbortController();
    let abortEvent = false;
    controller.signal.addEventListener("abort", () => {
      abortEvent = controller.signal.aborted === true;
    });
    const reason = new Error("user cancelled");
    controller.abort(reason);
    out.abortEvent = abortEvent;
    out.reason = controller.signal.reason === reason;
    out.aborted = controller.signal.aborted === true;
    try {
      controller.signal.throwIfAborted();
      out.throwIfAborted = false;
    } catch (e: any) {
      out.throwIfAborted = e === reason;
    }

    // AbortSignal.abort() static + default AbortError reason.
    const staticSignal = AbortSignal.abort();
    out.staticAbort = staticSignal.aborted && staticSignal.reason.name === "AbortError";

    // AbortSignal.timeout resolves via the host timer.
    const timed = AbortSignal.timeout(30);
    await new Promise((r: any) => setTimeout(r, 80));
    out.timeout = timed.aborted && timed.reason.name === "TimeoutError";

    // AbortSignal.any propagates.
    const src = new AbortController();
    const combined = AbortSignal.any([src.signal]);
    src.abort("via-any");
    out.any = combined.aborted && combined.reason === "via-any";

    globalThis.__abortOut = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn event_target_and_abort_controller() {
    let dir = write_temp_fixture("abort", &[("index.ts", ABORT_TS)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "abort", ABORT_TS, "__abortOut").await;

    assert_eq!(
        v["orderAndOnce"], true,
        "listener order + once + remove: {v}"
    );
    assert_eq!(v["preventDefault"], true, "cancelable dispatch return: {v}");
    assert_eq!(v["defaultPrevented"], true, "defaultPrevented flag: {v}");
    assert_eq!(v["onabort"], true, "onabort handler: {v}");
    assert_eq!(v["abortEvent"], true, "abort event with aborted flag: {v}");
    assert_eq!(v["reason"], true, "abort reason passthrough: {v}");
    assert_eq!(v["aborted"], true, "signal.aborted: {v}");
    assert_eq!(
        v["throwIfAborted"], true,
        "throwIfAborted raises reason: {v}"
    );
    assert_eq!(v["staticAbort"], true, "AbortSignal.abort(): {v}");
    assert_eq!(v["timeout"], true, "AbortSignal.timeout: {v}");
    assert_eq!(v["any"], true, "AbortSignal.any: {v}");
}

// ---------------------------------------------------------------------------
// Web streams (minimal tier)
// ---------------------------------------------------------------------------

const STREAMS_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // Pull-driven ReadableStream.
    let pulled = 0;
    const stream = new ReadableStream({
      pull(controller: any) {
        pulled += 1;
        if (pulled <= 3) controller.enqueue("c" + pulled);
        else controller.close();
      },
    });
    const reader = stream.getReader();
    const chunks: string[] = [];
    for (;;) {
      const result = await reader.read();
      if (result.done) break;
      chunks.push(String(result.value));
    }
    out.pulled = pulled;
    out.chunks = chunks.join("|");

    // Queued stream + close after enqueue.
    const queued = new ReadableStream({
      start(controller: any) {
        controller.enqueue(new Uint8Array([1, 2]));
        controller.enqueue(new Uint8Array([3]));
        controller.close();
      },
    });
    const qr = queued.getReader();
    const first = await qr.read();
    const second = await qr.read();
    const end = await qr.read();
    out.queued = `${first.value.length}:${second.value.length}:${end.done}`;

    // Cancel reaches source.cancel.
    let cancelReason: any = undefined;
    const cancellable = new ReadableStream({
      cancel(reason: any) { cancelReason = reason; },
    });
    await cancellable.cancel("bye");
    out.cancel = cancelReason === "bye";

    // TransformStream pipes writable -> readable.
    const transform = new TransformStream({
      transform(chunk: any, controller: any) {
        controller.enqueue(String(chunk).toUpperCase());
      },
    });
    const writer = transform.writable.getWriter();
    const tr = transform.readable.getReader();
    const readPromise = tr.read().then((r: any) => r.value);
    await writer.write("abc");
    out.transformed = await readPromise;

    // Erroring a stream rejects reads with the reason.
    const failing = new ReadableStream({
      start(controller: any) { controller.error(new Error("stream boom")); },
    });
    try {
      await failing.getReader().read();
      out.errorRejects = false;
    } catch (e: any) {
      out.errorRejects = e.message === "stream boom";
    }

    globalThis.__streamsOut = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn web_streams_minimal_surface() {
    let dir = write_temp_fixture("streams", &[("index.ts", STREAMS_TS)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "streams", STREAMS_TS, "__streamsOut").await;

    assert_eq!(v["pulled"], 4, "pull invoked per read wave: {v}");
    assert_eq!(v["chunks"], "c1|c2|c3", "pull-driven chunk sequence: {v}");
    assert_eq!(v["queued"], "2:1:true", "queued chunks then done: {v}");
    assert_eq!(v["cancel"], true, "cancel reaches source.cancel: {v}");
    assert_eq!(v["transformed"], "ABC", "TransformStream passthrough: {v}");
    assert_eq!(
        v["errorRejects"], true,
        "controller.error rejects reads: {v}"
    );
}

// ---------------------------------------------------------------------------
// Response consumption (constructed, no network)
// ---------------------------------------------------------------------------

const RESPONSE_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    const response = new Response('{"a":1,"b":"two"}', {
      status: 201,
      headers: { "content-type": "application/json" },
    });
    out.status = response.status;
    out.statusText = response.statusText;
    out.ok = response.ok;
    out.contentType = response.headers.get("Content-Type");
    out.json = await response.json();

    const textResponse = new Response(new TextEncoder().encode("héllo"));
    out.text = await textResponse.text();
    out.bodyUsedGuards = await textResponse.text().then(
      () => "resolved",
      (e: any) => "rejected"
    );

    // Response.body is a ReadableStream; read it manually.
    const bodyResponse = new Response("stream-me");
    out.bodyIsStream = bodyResponse.body instanceof ReadableStream;
    const reader = bodyResponse.body.getReader();
    const parts: Uint8Array[] = [];
    for (;;) {
      const r: any = await reader.read();
      if (r.done) break;
      parts.push(r.value);
    }
    out.bodyText = new TextDecoder().decode(parts[0]) === "stream-me";

    // Headers multi-value + iteration.
    const headers = new Headers([["set-cookie", "a=1"], ["set-cookie", "b=2"], ["x-one", "1"]]);
    out.setCookie = headers.getSetCookie().join(";");
    out.getJoins = headers.has("SET-COOKIE") && headers.get("x-one") === "1";

    // Status validation.
    try {
      new Response(null, { status: 199 });
      out.statusRangeThrows = false;
    } catch (e: any) {
      out.statusRangeThrows = e instanceof RangeError;
    }

    // fetch() must reject for an unusable URL without touching the host.
    try {
      await fetch("ftp://example.com/x");
      out.badSchemeThrows = false;
    } catch (e: any) {
      out.badSchemeThrows = /scheme/i.test(e.message);
    }
    try {
      await fetch("not-a-url");
      out.badUrlThrows = false;
    } catch (e: any) {
      out.badUrlThrows = /parse URL/i.test(e.message);
    }

    globalThis.__responseOut = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn response_aggregation_and_body_stream() {
    let dir = write_temp_fixture("response", &[("index.ts", RESPONSE_TS)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "response", RESPONSE_TS, "__responseOut").await;

    assert_eq!(v["status"], 201, "custom status: {v}");
    assert_eq!(v["statusText"], "Created", "statusText table: {v}");
    assert_eq!(v["ok"], true, "ok flag: {v}");
    assert_eq!(
        v["contentType"], "application/json",
        "header lookup case-insensitive: {v}"
    );
    assert_eq!(
        v["json"],
        json!({"a": 1, "b": "two"}),
        "response.json(): {v}"
    );
    assert_eq!(v["text"], "héllo", "response.text() aggregation: {v}");
    assert_eq!(v["bodyUsedGuards"], "rejected", "double read rejected: {v}");
    assert_eq!(
        v["bodyIsStream"], true,
        "Response.body is a ReadableStream: {v}"
    );
    assert_eq!(v["bodyText"], true, "Response.body manual read: {v}");
    assert_eq!(v["setCookie"], "a=1;b=2", "set-cookie multi-value: {v}");
    assert_eq!(v["getJoins"], true, "header case folding: {v}");
    assert_eq!(v["statusRangeThrows"], true, "status range check: {v}");
    assert_eq!(v["badSchemeThrows"], true, "non-http scheme rejected: {v}");
    assert_eq!(v["badUrlThrows"], true, "relative URL rejected: {v}");
}

// ---------------------------------------------------------------------------
// fetch over the host bridge (local loopback)
// ---------------------------------------------------------------------------

/// One-shot canned HTTP server: binds an ephemeral port, answers every
/// connection with `response`, records the first request line + a header.
fn spawn_http_server(response: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen_for_thread = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(_) => break,
            };
            let mut buf = [0u8; 4096];
            let mut request = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            *seen_for_thread.lock().unwrap() = String::from_utf8_lossy(&request).into_owned();
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, seen)
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_loopback_get_post_and_stream() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-mark: pir\r\ncontent-length: 19\r\n\r\nhello from loopback";
    let (port, seen) = spawn_http_server(response);
    let ts = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const base = "__PIR_LOOPBACK_URL__";

    const response = await fetch(base + "/data?x=1");
    out.status = response.status;
    out.ok = response.ok;
    out.mark = response.headers.get("x-mark");
    out.url = response.url;
    out.text = await response.text();

    // Manual body stream read.
    const streamed = await fetch(base + "/stream");
    const reader = streamed.body.getReader();
    let streamText = "";
    for (;;) {
      const r: any = await reader.read();
      if (r.done) break;
      streamText += new TextDecoder().decode(r.value, { stream: true });
    }
    out.streamText = streamText;

    // POST with a JSON body.
    const post = await fetch(base + "/submit", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hello: "world" }),
    });
    out.postStatus = post.status;
    out.postText = await post.text();

    globalThis.__fetchOut = out;
  });
};
"#;
    let ts = ts.replace("__PIR_LOOPBACK_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("fetch-loopback", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "fetch-loopback", &ts, "__fetchOut").await;

    assert_eq!(v["status"], 200, "loopback status: {v}");
    assert_eq!(v["ok"], true, "loopback ok flag: {v}");
    assert_eq!(v["mark"], "pir", "loopback response header: {v}");
    assert_eq!(
        v["text"], "hello from loopback",
        "loopback body aggregation: {v}"
    );
    assert_eq!(
        v["streamText"], "hello from loopback",
        "loopback streaming read: {v}"
    );
    assert_eq!(v["postStatus"], 200, "POST status: {v}");
    assert_eq!(
        v["postText"], "hello from loopback",
        "POST round trip (canned server body): {v}"
    );

    let request = seen.lock().unwrap().clone();
    assert!(
        request.starts_with("POST /submit HTTP/1.1\r\n"),
        "request line must reach the server: {request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "request headers must reach the server: {request}"
    );
    assert!(
        request.contains("{\"hello\":\"world\"}"),
        "request body must reach the server: {request}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_abort_mid_request() {
    // A server that accepts but never answers: the request hangs until the
    // signal aborts it.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        // Hold connections open (accept and leak them until the test ends).
        for stream in listener.incoming().flatten() {
            let _ = stream;
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    });
    let ts = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // Pre-aborted signal: fetch rejects before touching the host.
    const preAborted = AbortSignal.abort();
    out.preAborted = await fetch("__PIR_HANG_URL__", { signal: preAborted }).then(
      () => "resolved",
      (e: any) => e.name
    );

    // Abort mid-request: the signal fires while the headers are pending
    // (abort() with no reason rejects with the default AbortError).
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 150);
    const started = Date.now();
    out.midAbort = await fetch("__PIR_HANG_URL__", { signal: controller.signal }).then(
      () => "resolved",
      (e: any) => e.name + ":" + e.message
    );
    out.elapsed = Date.now() - started;

    // Timeout signal as the abort driver.
    const timed = AbortSignal.timeout(150);
    out.timeout = await fetch("__PIR_HANG_URL__", { signal: timed }).then(
      () => "resolved",
      (e: any) => e.name
    );

    globalThis.__abortFetchOut = out;
  });
};
"#;
    let ts = ts.replace("__PIR_HANG_URL__", &format!("http://127.0.0.1:{port}/hang"));
    let dir = write_temp_fixture("fetch-abort", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "fetch-abort", &ts, "__abortFetchOut").await;

    assert_eq!(v["preAborted"], "AbortError", "pre-aborted signal: {v}");
    let mid = v["midAbort"].as_str().unwrap_or_default().to_owned();
    assert!(
        mid.starts_with("AbortError"),
        "mid-request abort must reject with AbortError: {v}"
    );
    assert!(
        v["elapsed"].as_i64().unwrap_or(i64::MAX) < 10_000,
        "abort must cut the wait short: {v}"
    );
    assert_eq!(
        v["timeout"], "TimeoutError",
        "AbortSignal.timeout abort: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_network_error_rejects() {
    // Bind + immediately drop a port so the connection is deterministically
    // refused.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let ts = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    out.refused = await fetch("__PIR_DEAD_URL__").then(
      () => "resolved",
      (e: any) => (e instanceof TypeError ? "type-error" : "other:" + e.name)
    );
    globalThis.__netErrOut = out;
  });
};
"#;
    let ts = ts.replace("__PIR_DEAD_URL__", &format!("http://127.0.0.1:{port}/dead"));
    let dir = write_temp_fixture("fetch-error", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "fetch-error", &ts, "__netErrOut").await;
    assert_eq!(
        v["refused"], "type-error",
        "connection refused must reject with TypeError(fetch failed): {v}"
    );
}

/// Real-network smoke: `fetch https://example.com` — run explicitly with
/// `cargo test -- --ignored` on a machine with outbound access. Asserts a
/// 200 status plus the first 100 body bytes.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires outbound network; run with --ignored"]
async fn fetch_example_com_smoke() {
    let ts = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const response = await fetch("https://example.com");
    const body = await response.text();
    globalThis.__smoke = JSON.stringify({
      status: response.status,
      contentType: response.headers.get("content-type") || "",
      head: body.slice(0, 100),
    });
  });
};
"#;
    let dir = write_temp_fixture("fetch-smoke", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "fetch-smoke", ts, "__smoke").await;
    let parsed: Value =
        serde_json::from_str(v.as_str().expect("smoke result string")).expect("smoke result JSON");
    assert_eq!(parsed["status"], 200, "example.com status: {parsed}");
    assert!(
        parsed["head"]
            .as_str()
            .unwrap_or_default()
            .contains("doctype"),
        "example.com body head (first 100 bytes): {parsed}"
    );
}
