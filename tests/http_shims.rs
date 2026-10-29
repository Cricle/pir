//! node:http / node:https client + server over the `__pir_host.http` bridge.
//!
//! Loopback tests use a real Rust-side HTTP/1.1 server (or the runtime's own
//! `http.Server`) so request bytes, headers and bodies are asserted as they
//! actually hit the wire. Fixture extensions are generated at runtime into
//! per-test temp dirs (repository hygiene gate; same pattern as
//! `tests/web_globals.rs`). One `--ignored` smoke test exercises the real
//! https client against example.com.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex as StdMutex;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-http-shims-{}-{test_name}", std::process::id()));
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

/// One canned HTTP/1.1 server: reads the request head AND its
/// Content-Length body, records the full raw request, then answers every
/// connection with `response`.
fn spawn_canned_server(response: &'static str) -> (u16, Arc<StdMutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let seen = Arc::new(StdMutex::new(String::new()));
    let seen_for_thread = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let raw = read_full_request(&mut stream);
            *seen_for_thread.lock().unwrap() = raw;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, seen)
}

/// Read head + Content-Length body (0 when the header is absent).
fn read_full_request(stream: &mut TcpStream) -> String {
    let mut request: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => {
                head_end = request.len();
                break;
            }
            Ok(n) => {
                request.extend_from_slice(&buffer[..n]);
                if let Some(position) = find(&request, b"\r\n\r\n") {
                    head_end = position + 4;
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&request[..head_end.min(request.len())]).to_lowercase();
    let content_length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while request.len() < head_end + content_length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => request.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// http.Server + global fetch: full request/response round trip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn server_roundtrip_with_global_fetch() {
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const server = http.createServer((req: any, res: any) => {
      const chunks: any[] = [];
      req.on("data", (chunk: any) => chunks.push(chunk));
      req.on("end", () => {
        out.saw = {
          method: req.method,
          url: req.url,
          httpVersion: req.httpVersion,
          contentType: req.headers["content-type"],
          body: Buffer.concat(chunks).toString("utf8"),
        };
        res.writeHead(201, { "x-mark": "pir-server", "content-type": "text/plain" });
        res.end("echo:" + out.saw.body);
      });
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    out.port = server.address().port;
    out.listening = server.listening;

    const response = await fetch("http://127.0.0.1:" + out.port + "/hi?x=1", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ n: 1 }),
    });
    out.status = response.status;
    out.mark = response.headers.get("x-mark");
    out.text = await response.text();
    server.close();
    out.closedListening = server.listening;
    globalThis.__pirHttpOut = out;
  });
};
"#;
    let dir = write_temp_fixture("server-roundtrip", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "server-roundtrip", ts, "__pirHttpOut").await;

    assert_eq!(v["listening"], true, "server.listening after listen(): {v}");
    let port = v["port"].as_u64().expect("bound port in server.address()");
    assert!(port > 0, "random port bound: {v}");
    assert_eq!(v["status"], 201, "writeHead status round trip: {v}");
    assert_eq!(v["mark"], "pir-server", "response header round trip: {v}");
    assert_eq!(
        v["text"], "echo:{\"n\":1}",
        "ServerResponse body round trip: {v}"
    );
    assert_eq!(v["saw"]["method"], "POST", "req.method: {v}");
    assert_eq!(v["saw"]["url"], "/hi?x=1", "req.url: {v}");
    assert_eq!(v["saw"]["httpVersion"], "1.1", "req.httpVersion: {v}");
    assert_eq!(
        v["saw"]["contentType"], "application/json",
        "req.headers lookup: {v}"
    );
    assert_eq!(
        v["saw"]["body"], "{\"n\":1}",
        "request body reached the server: {v}"
    );
    assert_eq!(
        v["closedListening"], false,
        "close() clears the listening flag: {v}"
    );
}

// ---------------------------------------------------------------------------
// http.get → ClientRequest / IncomingMessage (data/end/statusCode/headers)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn client_get_response_events() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nX-Mark: pir-get\r\ncontent-length: 17\r\n\r\nhello from canned";
    let (port, _seen) = spawn_canned_server(response);
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { chunks: [] };
    await new Promise((resolve: any, reject: any) => {
      http.get("__PIR_URL__/data?x=1", (res: any) => {
        out.statusCode = res.statusCode;
        out.statusMessage = res.statusMessage;
        out.mark = res.headers["x-mark"];
        out.contentType = res.headers["content-type"];
        out.rawHeadersFlat = res.rawHeaders.length % 2 === 0;
        out.rawHeaderNames = res.rawHeaders
          .filter((_v: any, i: number) => i % 2 === 0)
          .map((name: string) => name.toLowerCase());
        out.isReadable = typeof res.on === "function";
        res.on("data", (chunk: any) => out.chunks.push(chunk.toString("utf8")));
        res.on("end", () => {
          out.ended = true;
          resolve(null);
        });
      }).on("error", (error: any) => {
        out.error = String(error && error.code ? error.code : error);
        resolve(null);
      });
    });
    out.body = out.chunks.join("");
    globalThis.__pirHttpOut = out;
  });
};
"#
    .replace("__PIR_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("client-get", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-get", &ts, "__pirHttpOut").await;

    assert_eq!(v["statusCode"], 200, "res.statusCode: {v}");
    assert_eq!(v["statusMessage"], "OK", "res.statusMessage: {v}");
    assert_eq!(
        v["mark"], "pir-get",
        "res.headers case-insensitive lookup: {v}"
    );
    assert_eq!(
        v["contentType"], "text/plain",
        "res.headers content-type: {v}"
    );
    assert_eq!(v["rawHeadersFlat"], true, "rawHeaders is a flat array: {v}");
    assert!(
        v["rawHeaderNames"]
            .as_array()
            .map(|names| names.iter().any(|n| n == "x-mark"))
            .unwrap_or(false),
        "rawHeaders preserve the original case: {v}"
    );
    assert_eq!(
        v["body"], "hello from canned",
        "data events reassemble the body: {v}"
    );
    assert_eq!(v["ended"], true, "end event fires: {v}");
    assert_eq!(v["error"], Value::Null, "no error on the happy path: {v}");
}

// ---------------------------------------------------------------------------
// http.request POST: write/end aggregation reaches the wire with headers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn client_post_body_and_headers_reach_server() {
    let response = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
    let (port, seen) = spawn_canned_server(response);
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { responseText: "" };
    await new Promise((resolve: any) => {
      const req = http.request(
        {
          host: "127.0.0.1",
          port: __PIR_PORT__,
          method: "POST",
          path: "/submit?a=b",
          headers: { "content-type": "text/plain", "x-mark": "pir-post" },
        },
        (res: any) => {
          out.status = res.statusCode;
          res.on("data", (chunk: any) => (out.responseText += chunk.toString("utf8")));
          res.on("end", () => resolve(null));
        },
      );
      req.on("error", (error: any) => {
        out.error = String(error);
        resolve(null);
      });
      req.write("hello ");
      req.write(Buffer.from("world"));
      req.end();
    });
    globalThis.__pirHttpOut = out;
  });
};
"#
    .replace("__PIR_PORT__", &port.to_string());
    let dir = write_temp_fixture("client-post", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-post", &ts, "__pirHttpOut").await;

    assert_eq!(v["status"], 200, "POST response status: {v}");
    assert_eq!(v["responseText"], "ok", "POST response body: {v}");
    assert_eq!(v["error"], Value::Null, "no error on the happy path: {v}");

    let request = seen.lock().unwrap().clone();
    assert!(
        request.starts_with("POST /submit?a=b HTTP/1.1\r\n"),
        "request line must reach the server: {request}"
    );
    assert!(
        request
            .to_lowercase()
            .contains(&format!("host: 127.0.0.1:{port}")),
        "host header present: {request}"
    );
    assert!(
        request.to_lowercase().contains("content-type: text/plain"),
        "caller headers reach the server: {request}"
    );
    assert!(
        request.to_lowercase().contains("x-mark: pir-post"),
        "custom headers reach the server: {request}"
    );
    assert!(
        request.to_lowercase().contains("content-length: 11"),
        "aggregated body length announced: {request}"
    );
    assert!(
        request.ends_with("hello world"),
        "both write() chunks reach the server in order: {request}"
    );
}

// ---------------------------------------------------------------------------
// Redirects are returned, not followed (Node's node:http default)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn client_does_not_follow_redirects() {
    let response = "HTTP/1.1 302 Found\r\nlocation: /there\r\ncontent-length: 0\r\n\r\n";
    let (port, seen) = spawn_canned_server(response);
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    await new Promise((resolve: any) => {
      http.get("__PIR_URL__/jump", (res: any) => {
        out.statusCode = res.statusCode;
        out.location = res.headers["location"];
        res.on("data", () => {});
        res.on("end", () => resolve(null));
      }).on("error", (error: any) => {
        out.error = String(error);
        resolve(null);
      });
    });
    globalThis.__pirHttpOut = out;
  });
};
"#
    .replace("__PIR_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("client-redirect", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-redirect", &ts, "__pirHttpOut").await;

    assert_eq!(v["statusCode"], 302, "3xx returned as-is: {v}");
    assert_eq!(v["location"], "/there", "redirect header visible: {v}");
    assert_eq!(v["error"], Value::Null, "no error: {v}");
    let request_count = seen.lock().unwrap().matches("GET /jump HTTP/1.1").count();
    assert_eq!(
        request_count, 1,
        "exactly one request was sent: {request_count}"
    );
}

// ---------------------------------------------------------------------------
// ECONNREFUSED → 'error' event with the transport code
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn client_error_event_on_refused_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    await new Promise((resolve: any) => {
      const req = http.request("__PIR_DEAD_URL__/dead");
      req.on("error", (error: any) => {
        out.code = error.code;
        out.message = error.message;
      });
      req.on("close", () => {
        out.closed = true;
        resolve(null);
      });
      req.end();
    });
    globalThis.__pirHttpOut = out;
  });
};
"#
    .replace("__PIR_DEAD_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("client-refused", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-refused", &ts, "__pirHttpOut").await;

    assert_eq!(
        v["code"], "ECONNREFUSED",
        "refused connect carries the code: {v}"
    );
    assert_eq!(v["closed"], true, "close follows the error: {v}");
}

// ---------------------------------------------------------------------------
// AbortSignal → destroy + fetchAbort → AbortError error event
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn client_abort_via_signal() {
    // Accept and hold: the request hangs until the signal aborts it.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                drop(stream);
            });
        }
    });
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    await new Promise((resolve: any) => {
      const controller = new AbortController();
      const req = http.request("__PIR_HANG_URL__/hang", { signal: controller.signal });
      req.on("error", (error: any) => {
        out.errorName = error.name;
        out.errorCode = error.code;
      });
      req.on("close", () => {
        out.closed = true;
        resolve(null);
      });
      req.end();
      setTimeout(() => controller.abort(), 120);
    });
    globalThis.__pirHttpOut = out;
  });
};
"#
    .replace("__PIR_HANG_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("client-abort", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-abort", &ts, "__pirHttpOut").await;

    assert_eq!(v["closed"], true, "close fires after abort: {v}");
    assert_eq!(
        v["errorName"], "AbortError",
        "abort produces an AbortError: {v}"
    );
    assert_eq!(
        v["errorCode"], "ABORT_ERR",
        "abort error carries ABORT_ERR: {v}"
    );
}

// ---------------------------------------------------------------------------
// http.Server keep-alive: several requests over one socket, connection:
// keep-alive answers, negotiated close, close() drains in-flight requests
//
// Drive pattern for the raw-socket tests: the JS handler parks on a pending
// promise (its await is what pumps the JS event loop — timers and the server
// poll loop only run while a handler await is in flight), a Rust client
// thread reads the bound port from a fixture file and runs the socket
// exchange concurrently, and a final resolve() un-parks the handler so the
// runtime settles before the records are read.
// ---------------------------------------------------------------------------

/// Read one HTTP/1.1 response (head + Content-Length body) off the socket.
fn read_one_response(stream: &mut TcpStream) -> String {
    let mut response: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => {
                head_end = response.len();
                break;
            }
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                if let Some(position) = find(&response, b"\r\n\r\n") {
                    head_end = position + 4;
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&response[..head_end.min(response.len())]).to_lowercase();
    let content_length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while response.len() < head_end + content_length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}

/// Poll-read the port file the JS handler writes right after `listen()`.
fn read_port_file(path: &std::path::Path) -> u16 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(port) = content.trim().parse::<u16>() {
                return port;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "port file never appeared: {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Seed the fixture, run the Rust client thread concurrently with the parked
/// handler (whose await pumps the JS event loop), then settle the runtime.
///
/// The fixtures used to park for a fixed 6 s and only then read the result, so
/// every such test paid the full timeout even when the exchange finished in
/// 100 ms. The fixture now polls a `client-done` sentinel (10 ms) that the
/// client thread publishes when its exchange is complete; the JS timeout stays
/// as the bound, not as the normal path.
async fn emit_parked_fixture(
    rt: &ExtensionRuntime,
    name: &str,
    ts: &str,
    port_file: &std::path::Path,
    client: impl FnOnce(u16) + Send + 'static,
) {
    let module = format!("pir-ext:{name}.ts");
    let port_file = port_file.to_path_buf();
    let done_file = port_file.with_file_name("client-done");
    let _ = std::fs::remove_file(&done_file);
    let ts = ts.replace(
        "__PIR_DONE__",
        &done_file.to_string_lossy().replace('\\', "/"),
    );
    rt.seed_module(&module, &ts).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture {name} must load");

    let done_for_client = done_file.clone();
    let client = std::thread::spawn(move || {
        let port = read_port_file(&port_file);
        client(port);
        let _ = std::fs::write(&done_for_client, b"done");
    });
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
    client.join().expect("raw client thread");
    // Un-park the handler and let the runtime settle (server.close etc.).
    rt.eval_json(
        "(globalThis.__pirResolve && globalThis.__pirResolve(), JSON.stringify(\"settled\"))",
    )
    .await
    .expect("resolve handler");
    rt.wait_idle().await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_keep_alive_serves_serial_requests_on_one_connection() {
    let dir = write_temp_fixture("server-keepalive", &[]);
    let port_file = dir.join("port.txt");
    let ts = r#"
import http from "node:http";
import fs from "node:fs";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const seen: any[] = [];
    const server = http.createServer((req: any, res: any) => {
      seen.push({ url: req.url, connId: res._pirConnId, connection: req.headers["connection"] });
      res.writeHead(200, { "content-type": "text/plain" });
      res.end("answer:" + req.url);
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    fs.writeFileSync("__PIR_PORT_FILE__", String(server.address().port));
    globalThis.__pirKeepAlive = { seen: seen };
    // Park the handler: its pending await is the JS event-loop pump while the
    // Rust-side raw client runs the exchange. Resolve as soon as the client
    // publishes `__PIR_DONE__` (10 ms poll); 6 s is only the bound.
    await new Promise((resolve: any) => {
      (globalThis as any).__pirResolve = resolve;
      const timer = setInterval(() => {
        try {
          if (fs.existsSync("__PIR_DONE__")) { clearInterval(timer); resolve(); }
        } catch {}
      }, 10);
      setTimeout(() => { clearInterval(timer); resolve(); }, 6000);
    });
    server.close();
  });
};
"#
    .replace(
        "__PIR_PORT_FILE__",
        &port_file.to_string_lossy().replace('\\', "/"),
    );
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    emit_parked_fixture(&rt, "server-keepalive", &ts, &port_file, |port| {
        // Two serialized requests on ONE raw socket, then a negotiated close.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"GET /one HTTP/1.1\r\nhost: loopback\r\n\r\n")
            .expect("write request one");
        let first = read_one_response(&mut stream);
        assert!(
            first.contains("connection: keep-alive"),
            "first response must advertise keep-alive: {first}"
        );
        assert!(first.contains("answer:/one"), "first body: {first}");
        stream
            .write_all(b"GET /two HTTP/1.1\r\nhost: loopback\r\n\r\n")
            .expect("write request two");
        let second = read_one_response(&mut stream);
        assert!(
            second.contains("answer:/two"),
            "second response on the SAME socket: {second}"
        );
        stream
            .write_all(b"GET /three HTTP/1.1\r\nhost: loopback\r\nconnection: close\r\n\r\n")
            .expect("write request three");
        let third = read_one_response(&mut stream);
        assert!(
            third.contains("connection: close"),
            "client-requested close must be honored: {third}"
        );
        assert!(third.contains("answer:/three"), "third body: {third}");
        let mut eof = [0u8; 64];
        assert_eq!(
            stream.read(&mut eof).unwrap_or(0),
            0,
            "socket closes after the negotiated close"
        );
    })
    .await;
    let v = rt
        .eval_json("JSON.stringify(globalThis.__pirKeepAlive)")
        .await
        .expect("read request records");
    let seen = v["seen"].as_array().expect("request records");
    assert_eq!(seen.len(), 3, "all three requests dispatched: {v}");
    assert_eq!(seen[0]["url"], "/one");
    assert_eq!(seen[2]["url"], "/three");
    assert_eq!(
        seen[0]["connId"], seen[2]["connId"],
        "all three requests share one connection: {v}"
    );
    assert_eq!(
        seen[2]["connection"], "close",
        "the close header reaches req.headers: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn server_streams_chunked_request_body_to_handler() {
    // A chunked upload must reach the handler as SEVERAL data events (not one
    // aggregate) and keep the connection usable for a follow-up request.
    let dir = write_temp_fixture("server-chunked", &[]);
    let port_file = dir.join("port.txt");
    let ts = r#"
import http from "node:http";
import fs from "node:fs";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const seen: any = { chunks: 0, body: "" };
    const server = http.createServer((req: any, res: any) => {
      if (req.url === "/upload") {
        req.on("data", (chunk: any) => {
          seen.chunks += 1;
          seen.body += chunk.toString("utf8");
        });
        req.on("end", () => {
          seen.ended = true;
          res.writeHead(200, { "content-type": "text/plain" });
          res.end("got:" + seen.body);
        });
        return;
      }
      res.writeHead(200, { "content-type": "text/plain" });
      res.end("follow-up:" + req.url);
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    fs.writeFileSync("__PIR_PORT_FILE__", String(server.address().port));
    globalThis.__pirChunked = { seen: seen };
    // Resolve when the Rust client publishes `__PIR_DONE__`; 6 s is the bound.
    await new Promise((resolve: any) => {
      (globalThis as any).__pirResolve = resolve;
      const timer = setInterval(() => {
        try {
          if (fs.existsSync("__PIR_DONE__")) { clearInterval(timer); resolve(); }
        } catch {}
      }, 10);
      setTimeout(() => { clearInterval(timer); resolve(); }, 6000);
    });
    server.close();
  });
};
"#
    .replace(
        "__PIR_PORT_FILE__",
        &port_file.to_string_lossy().replace('\\', "/"),
    );
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    emit_parked_fixture(&rt, "server-chunked", &ts, &port_file, |port| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(
                b"POST /upload HTTP/1.1\r\nhost: loopback\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .expect("write head");
        // Chunk 1, pause, chunk 2, pause, terminator: the pauses make the
        // streaming split observable (an aggregator would collapse them).
        stream.write_all(b"4\r\nabcd\r\n").expect("chunk one");
        std::thread::sleep(std::time::Duration::from_millis(60));
        stream.write_all(b"5\r\nefghi\r\n").expect("chunk two");
        std::thread::sleep(std::time::Duration::from_millis(60));
        stream.write_all(b"0\r\n\r\n").expect("chunked terminator");
        let response = read_one_response(&mut stream);
        assert!(
            response.contains("got:abcdefghi"),
            "chunked body reassembles at the handler: {response}"
        );
        // Keep-alive framing survived the chunked body: a follow-up request
        // on the same socket gets its own response.
        stream
            .write_all(b"GET /after HTTP/1.1\r\nhost: loopback\r\n\r\n")
            .expect("write follow-up");
        let follow = read_one_response(&mut stream);
        assert!(
            follow.contains("follow-up:/after"),
            "keep-alive request after a chunked upload: {follow}"
        );
    })
    .await;
    let v = rt
        .eval_json("JSON.stringify(globalThis.__pirChunked)")
        .await
        .expect("read chunked records");
    let chunks = v["seen"]["chunks"].as_u64().unwrap_or(0);
    assert!(
        chunks >= 2,
        "chunked upload streamed as several data events (got {chunks}): {v}"
    );
    assert_eq!(
        v["seen"]["ended"], true,
        "end event after the zero chunk: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn server_close_waits_for_in_flight_request() {
    // server.close(callback): the callback fires only after the in-flight
    // request/response cycle completes (the close is scheduled from JS while
    // the request is in flight).
    let dir = write_temp_fixture("server-close-drain", &[]);
    let port_file = dir.join("port.txt");
    let ts = r#"
import http from "node:http";
import fs from "node:fs";

export default (pi: any) => {
  pi.on("session_start", async () => {
    globalThis.__pirCloseDrain = { answered: false, closeFired: false };
    const server = http.createServer((req: any, res: any) => {
      // Anchor both timers to the REQUEST, not to listen(): a loaded box can
      // delay the client's connect past a fixed post-listen deadline, and a
      // connection that arrives after close() has no in-flight cycle left to
      // drain. Scheduling close from the handler guarantees the cycle is in
      // flight when it lands, and the later answer keeps it there while the
      // drain is requested.
      setTimeout(() => {
        server.close(() => {
          (globalThis as any).__pirCloseDrain.closeFired = true;
          (globalThis as any).__pirResolve?.();
        });
      }, 100);
      setTimeout(() => {
        (globalThis as any).__pirCloseDrain.answered = true;
        res.writeHead(200, { "content-type": "text/plain" });
        res.end("late-answer");
      }, 400);
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    fs.writeFileSync("__PIR_PORT_FILE__", String(server.address().port));
    await new Promise((resolve: any) => {
      (globalThis as any).__pirResolve = resolve;
      setTimeout(resolve, 2000);
    });
  });
};
"#
    .replace(
        "__PIR_PORT_FILE__",
        &port_file.to_string_lossy().replace('\\', "/"),
    );
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    emit_parked_fixture(&rt, "server-close-drain", &ts, &port_file, |port| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nhost: loopback\r\n\r\n")
            .expect("write slow request");
        let response = read_one_response(&mut stream);
        assert!(
            response.contains("late-answer"),
            "in-flight response completes: {response}"
        );
    })
    .await;
    let v = rt
        .eval_json("JSON.stringify(globalThis.__pirCloseDrain)")
        .await
        .expect("read close state");
    assert_eq!(v["answered"], true, "the in-flight answer happened: {v}");
    assert_eq!(
        v["closeFired"], true,
        "server.close() callback fires after the drain: {v}"
    );
}
// ---------------------------------------------------------------------------
// Fetch root-cause regression: ONE process-wide reqwest pool
//
// `http_fns.rs`'s `fetch_runtime()` + `shared_client*()` keep the client's
// connections on a runtime that outlives every request, so (a) a burst of
// node:http requests must all be answered — the failure shape this pins was a
// lost response / `error sending request` when several requests shared the
// pool — and (b) serialized requests must re-use a socket instead of opening
// one per request. Both are observed on the *peer* side, never by timing.
// ---------------------------------------------------------------------------

/// Value of `name` in a raw request head (case-insensitive), if present.
fn header_value(raw: &str, name: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// How long a barrier-gated peer waits for its full burst before answering
/// whatever arrived: a client that fails to fan out fails the observation
/// instead of hanging the runtime.
const BURST_BARRIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Simultaneity observation of the burst peer.
struct BurstServer {
    port: u16,
    paths: Arc<StdMutex<Vec<String>>>,
    gate: Arc<(StdMutex<BurstGate>, Condvar)>,
}

#[derive(Default)]
struct BurstGate {
    open: usize,
    max_open: usize,
    released: bool,
}

impl BurstServer {
    /// Highest number of connections that were simultaneously mid-request.
    fn max_open(&self) -> usize {
        self.gate.0.lock().unwrap().max_open
    }

    /// Request paths the peer read, in arrival order.
    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

/// Loopback HTTP/1.1 peer that reads every request head immediately but
/// withholds every response until `expect` connections are simultaneously
/// mid-request (or `BURST_BARRIER_TIMEOUT` elapses). A client can therefore
/// only complete its burst by having all `expect` requests in flight at once,
/// which makes [`BurstServer::max_open`] a structural proof of the fan-out —
/// no response is gated on a sleep.
fn spawn_burst_server(expect: usize) -> BurstServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let paths = Arc::new(StdMutex::new(Vec::new()));
    let gate = Arc::new((StdMutex::new(BurstGate::default()), Condvar::new()));
    let paths_for_thread = Arc::clone(&paths);
    let gate_for_thread = Arc::clone(&gate);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let paths = Arc::clone(&paths_for_thread);
            let gate = Arc::clone(&gate_for_thread);
            std::thread::spawn(move || {
                let raw = read_full_request(&mut stream);
                let path = raw
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                paths.lock().unwrap().push(path.clone());
                let (lock, cvar) = &*gate;
                {
                    let mut state = lock.lock().unwrap();
                    state.open += 1;
                    state.max_open = state.max_open.max(state.open);
                    if state.open >= expect {
                        state.released = true;
                        cvar.notify_all();
                    }
                }
                {
                    // The predicate re-reads `released` before parking, so a
                    // release that landed between the two locks is not missed.
                    let mut state = lock.lock().unwrap();
                    loop {
                        if state.released {
                            break;
                        }
                        let (next, timeout) =
                            cvar.wait_timeout(state, BURST_BARRIER_TIMEOUT).unwrap();
                        state = next;
                        if timeout.timed_out() {
                            break;
                        }
                    }
                }
                // `connection: close` keeps the burst out of the pool: this
                // test is about simultaneity, reuse has its own test below.
                let body = format!("echo:{path}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                lock.lock().unwrap().open -= 1;
            });
        }
    });
    BurstServer { port, paths, gate }
}

/// Ten node:http requests issued in one synchronous burst must all come back
/// with their OWN body and no error: the pinned failure was a response lost
/// (`error sending request`) when several requests shared the process-wide
/// client/pool. The peer holds every answer until all ten are in flight, so
/// what the assertions read is simultaneity, not speed.
#[tokio::test(flavor = "current_thread")]
async fn client_burst_of_ten_concurrent_requests_is_fully_answered() {
    let server = spawn_burst_server(10);
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const pending: any[] = [];
    for (let i = 0; i < 10; i++) {
      pending.push(new Promise((resolve: any) => {
        let settled = false;
        // Guard: a request that never settles must fail the assertions below
        // with a diagnostic instead of parking the handler forever.
        const guard = setTimeout(() => {
          if (settled) return;
          settled = true;
          resolve({ index: i, outcome: "timeout" });
        }, 15000);
        const finish = (value: any) => {
          if (settled) return;
          settled = true;
          clearTimeout(guard);
          resolve(value);
        };
        const request = http.get("__PIR_URL__/c" + i, (res: any) => {
          let body = "";
          res.on("data", (chunk: any) => { body += chunk.toString("utf8"); });
          res.on("end", () => finish({ index: i, status: res.statusCode, body: body }));
        });
        request.on("error", (error: any) => finish({
          index: i,
          outcome: "error",
          code: error && error.code,
          message: String(error && error.message ? error.message : error),
        }));
      }));
    }
    globalThis.__pirBurst = { results: await Promise.all(pending) };
  });
};
"#
    .replace("__PIR_URL__", &format!("http://127.0.0.1:{}", server.port));
    let dir = write_temp_fixture("client-burst", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-burst", &ts, "__pirBurst").await;

    let results = v["results"].as_array().expect("burst results");
    assert_eq!(results.len(), 10, "every request must settle: {v}");
    for (index, result) in results.iter().enumerate() {
        assert_eq!(
            result["outcome"],
            Value::Null,
            "request {index} must not error or time out (a lost response is the \
             regression): {v}"
        );
        assert_eq!(result["status"], 200, "request {index} status: {v}");
        assert_eq!(
            result["body"],
            format!("echo:/c{index}"),
            "request {index} must receive its own response (crossed answers and \
             drops both fail here): {v}"
        );
    }
    assert_eq!(
        server.max_open(),
        10,
        "all ten requests must be in flight at the same time (the peer answers \
         nothing before the barrier fills)"
    );
    let mut paths = server.paths();
    paths.sort();
    let mut expected: Vec<String> = (0..10).map(|index| format!("/c{index}")).collect();
    expected.sort();
    assert_eq!(
        paths, expected,
        "each request reached the peer exactly once"
    );
}

/// Loopback HTTP/1.1 keep-alive peer: one thread per accepted connection,
/// every request on a socket answered in place, and each request's path
/// recorded under that connection's index — so the client's pool reuse is
/// readable off the *peer's* connection count.
fn spawn_keep_alive_server() -> (u16, Arc<StdMutex<Vec<Vec<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let connections = Arc::new(StdMutex::new(Vec::new()));
    let connections_for_thread = Arc::clone(&connections);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let connections = Arc::clone(&connections_for_thread);
            std::thread::spawn(move || {
                let index = {
                    let mut guard = connections.lock().unwrap();
                    guard.push(Vec::new());
                    guard.len() - 1
                };
                loop {
                    let raw = read_full_request(&mut stream);
                    if raw.is_empty() {
                        break; // the client dropped the idle keep-alive socket
                    }
                    let path = raw
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    connections.lock().unwrap()[index].push(path.clone());
                    // content-length, and no `connection: close`: an HTTP/1.1
                    // peer may keep the socket, and the client's pool decides.
                    let body = format!("echo:{path}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
            });
        }
    });
    (port, connections)
}

/// Three serialized node:http requests must re-use a pooled connection:
/// with a runtime per request the pooled socket died with its request, so
/// every request paid a fresh connection. Read off the peer's connection
/// count, not a clock. One extra connection is tolerated on purpose — the
/// pool hands an idle socket back on its own runtime task, which can trail
/// the first response by a scheduling turn — while three connections is the
/// no-reuse failure shape this pins.
#[tokio::test(flavor = "current_thread")]
async fn serialized_client_requests_reuse_a_pooled_connection() {
    let (port, connections) = spawn_keep_alive_server();
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { bodies: [], errors: [] };
    for (let i = 0; i < 3; i++) {
      try {
        out.bodies.push(await new Promise((resolve: any, reject: any) => {
          const request = http.get("__PIR_URL__/r" + i, (res: any) => {
            let body = "";
            res.on("data", (chunk: any) => { body += chunk.toString("utf8"); });
            res.on("end", () => resolve(body));
          });
          request.on("error", (error: any) => {
            reject(String(error && error.code ? error.code : error));
          });
        }));
      } catch (error) {
        out.errors.push(String(error));
        break;
      }
    }
    globalThis.__pirReuse = out;
  });
};
"#
    .replace("__PIR_URL__", &format!("http://127.0.0.1:{port}"));
    let dir = write_temp_fixture("client-reuse", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-reuse", &ts, "__pirReuse").await;

    assert_eq!(v["errors"], json!([]), "no request may fail: {v}");
    assert_eq!(
        v["bodies"],
        json!(["echo:/r0", "echo:/r1", "echo:/r2"]),
        "every request answered with its own body: {v}"
    );

    let layout = connections.lock().unwrap().clone();
    let total: usize = layout.iter().map(Vec::len).sum();
    assert_eq!(total, 3, "three requests reached the peer: {layout:?}");
    assert!(
        layout.iter().any(|paths| paths.len() >= 2),
        "at least one socket must serve more than one request (no reuse = no \
         pooled connection): {layout:?}"
    );
    assert!(
        layout.len() <= 2,
        "three serialized requests must not open three connections: {layout:?}"
    );
}

/// Loopback peer that records the full raw head of every request it reads (in
/// arrival order) and answers each with `content-length` + `connection: close`,
/// so every request is a fresh socket and the capture order is request order.
fn spawn_head_capture_server() -> (u16, Arc<StdMutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let seen_for_thread = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let raw = read_full_request(&mut stream);
            seen_for_thread.lock().unwrap().push(raw);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            let _ = stream.flush();
        }
    });
    (port, seen)
}

/// Outbound `connection` semantics on the wire (Node v19+/v24): the default
/// agent (`http.globalAgent`, `keepAlive: true`) advertises `keep-alive`;
/// `agent: false` (Node's `keepAlive: false` + unbounded `maxSockets` shape)
/// advertises `close`; an explicit caller header always wins. Asserted on the
/// bytes the peer read, and the value is chosen synchronously in
/// `ClientRequest`'s constructor, so there is no timing in this test at all.
#[tokio::test(flavor = "current_thread")]
async fn client_advertises_node_connection_semantics_on_the_wire() {
    let (port, seen) = spawn_head_capture_server();
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { statuses: [] };
    const send = (options: any) => new Promise((resolve: any) => {
      const request = http.get(options, (res: any) => {
        res.on("data", () => {});
        res.on("end", () => resolve(res.statusCode));
      });
      request.on("error", (error: any) => resolve(
        "error:" + String(error && error.code ? error.code : error)
      ));
    });
    out.statuses.push(await send("__PIR_URL__/default-agent"));
    out.statuses.push(await send({
      host: "127.0.0.1", port: __PIR_PORT__, path: "/no-agent", agent: false,
    }));
    out.statuses.push(await send({
      host: "127.0.0.1", port: __PIR_PORT__, path: "/explicit-close",
      headers: { connection: "close" },
    }));
    globalThis.__pirConnHeader = out;
  });
};
"#
    .replace("__PIR_URL__", &format!("http://127.0.0.1:{port}"))
    .replace("__PIR_PORT__", &port.to_string());
    let dir = write_temp_fixture("client-conn-header", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-conn-header", &ts, "__pirConnHeader").await;

    assert_eq!(
        v["statuses"],
        json!([200, 200, 200]),
        "all three requests must be answered: {v}"
    );
    let heads = seen.lock().unwrap().clone();
    assert_eq!(heads.len(), 3, "three requests reached the peer: {heads:?}");
    assert_eq!(
        header_value(&heads[0], "connection").as_deref(),
        Some("keep-alive"),
        "the default agent advertises keep-alive: {}",
        heads[0]
    );
    assert_eq!(
        header_value(&heads[1], "connection").as_deref(),
        Some("close"),
        "agent: false advertises close: {}",
        heads[1]
    );
    assert_eq!(
        header_value(&heads[2], "connection").as_deref(),
        Some("close"),
        "an explicit caller header wins over the default rule: {}",
        heads[2]
    );
}

// ---------------------------------------------------------------------------
// https client smoke against example.com (real network, --ignored)
// ---------------------------------------------------------------------------

/// Real-network smoke: `https.get https://example.com` — run explicitly with
/// `cargo test -- --ignored` on a machine with outbound access.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires outbound network; run with --ignored"]
async fn https_example_com_smoke() {
    let ts = r#"
import https from "node:https";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    await new Promise((resolve: any) => {
      https.get("https://example.com", (res: any) => {
        out.statusCode = res.statusCode;
        let body = "";
        res.on("data", (chunk: any) => (body += chunk.toString("utf8")));
        res.on("end", () => {
          out.head = body.slice(0, 100);
          resolve(null);
        });
      }).on("error", (error: any) => {
        out.error = error.message;
        resolve(null);
      });
    });
    globalThis.__pirHttpOut = out;
  });
};
"#;
    let dir = write_temp_fixture("https-smoke", &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "https-smoke", ts, "__pirHttpOut").await;
    assert_eq!(v["statusCode"], 200, "example.com status: {v}");
    assert!(
        v["head"].as_str().unwrap_or_default().contains("doctype"),
        "example.com body head (first 100 bytes): {v}"
    );
}

// ---------------------------------------------------------------------------
// Text-channel decision boundaries (`bodyText` vs `bodyB64`)
//
// Both directions of the shim send a pure-UTF-8 string body as JSON text
// (`bodyText`) and fall back to the base64 byte channel for every other
// payload: Buffer/typed array, non-utf8 encoding, or a lone surrogate (UTF-8
// substitutes U+FFFD for it, while JSON.stringify escapes it and the host
// payload parser rejects the escape). These cases pin the fallback decisions
// to the bytes the base64 channel always produced, so a regression that lets
// a non-UTF-8-shaped string ride the text channel shows up as a byte diff.
// ---------------------------------------------------------------------------

/// Canned HTTP/1.1 server that records EVERY request as raw bytes (head + its
/// Content-Length body) in arrival order. `spawn_canned_server` keeps only the
/// last request and decodes it lossily, which cannot express "exactly 3 bytes
/// EF BF BD": replacement characters and invalid sequences must be compared
/// byte-for-byte.
fn spawn_recording_server(response: &'static str) -> (u16, Arc<StdMutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let seen_for_thread = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen_for_thread);
            // One thread per connection: the host reqwest pool reuses one
            // socket across requests (`node:http` and `fetch` keep SEPARATE
            // pools, so a second connection can open while the first stays
            // alive). A single-accept loop would sit in the first connection's
            // keep-alive read and leave the second one unanswered forever.
            std::thread::spawn(move || {
                let mut stream = stream;
                loop {
                    let raw = read_full_request_bytes(&mut stream);
                    if raw.is_empty() {
                        break;
                    }
                    seen.lock().unwrap().push(raw);
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
        }
    });
    (port, seen)
}

/// Byte-exact twin of `read_full_request` (which lossily decodes to String).
fn read_full_request_bytes(stream: &mut TcpStream) -> Vec<u8> {
    let mut request: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => {
                head_end = request.len();
                break;
            }
            Ok(n) => {
                request.extend_from_slice(&buffer[..n]);
                if let Some(position) = find(&request, b"\r\n\r\n") {
                    head_end = position + 4;
                    break;
                }
            }
        }
    }
    let content_length = content_length_of(&request[..head_end.min(request.len())]);
    while request.len() < head_end + content_length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => request.extend_from_slice(&buffer[..n]),
        }
    }
    request
}

/// Byte-exact twin of `read_one_response`.
fn read_one_response_bytes(stream: &mut TcpStream) -> Vec<u8> {
    let mut response: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => {
                head_end = response.len();
                break;
            }
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                if let Some(position) = find(&response, b"\r\n\r\n") {
                    head_end = position + 4;
                    break;
                }
            }
        }
    }
    let content_length = content_length_of(&response[..head_end.min(response.len())]);
    while response.len() < head_end + content_length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&buffer[..n]),
        }
    }
    response
}

/// `content-length` of a raw head (0 when absent), matching `read_full_request`.
fn content_length_of(head: &[u8]) -> usize {
    let head = String::from_utf8_lossy(head).to_lowercase();
    head.lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Body slice of a raw HTTP message: the bytes after the head's blank line.
fn message_body(raw: &[u8]) -> &[u8] {
    match find(raw, b"\r\n\r\n") {
        Some(position) => &raw[position + 4..],
        None => &[],
    }
}

/// Lowercase hex, `Buffer.toString("hex")` parity, for byte-exact assertions.
fn body_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Request-side boundaries: every case asserts the bytes that reach the peer
/// against the byte channel's own definition, `Buffer.from(chunk, encoding)`
/// (reported by the fixture too), so the text fast path can never change the
/// wire image.
///
/// Why it holds: `pirTextChunkEncodable` (http_shim.js.txt:148) refuses any
/// chunk whose encoding does not normalize to utf8/utf-8 and any chunk with a
/// lone surrogate; the first refusal drops `_pirBodyTextOk`, so `_pirStart`
/// (://929) sends the whole request as `bodyB64` — the byte-identical legacy
/// path. If red, check in order: (1) `statuses` — a non-200 means the request
/// never completed, i.e. a lone surrogate DID ride `bodyText` and the host
/// rejected the `\ud800` JSON escape; (2) the recorded request count — fewer
/// records means the same rejection happened before the wire; (3) the head
/// path asserted per case — a mismatch means the records are out of order.
#[tokio::test(flavor = "current_thread")]
async fn client_text_channel_boundaries_send_the_old_bytes() {
    let response = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
    let (port, seen) = spawn_recording_server(response);
    let ts = r#"
import http from "node:http";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = { statuses: [], expectedHex: {} };
    const post = (label: string, write: (req: any) => void) => new Promise((resolve: any) => {
      const req = http.request(
        {
          host: "127.0.0.1",
          port: __PIR_PORT__,
          method: "POST",
          path: "/" + label,
          headers: { "content-type": "text/plain" },
        },
        (res: any) => {
          res.on("data", () => {});
          res.on("end", () => resolve(res.statusCode));
        },
      );
      req.on("error", (error: any) => resolve(
        "error:" + String(error && error.code ? error.code : error)
      ));
      write(req);
    });

    // 1. A lone high surrogate: the text channel must be refused.
    out.expectedHex.loneHigh = Buffer.from("\uD800", "utf8").toString("hex");
    out.statuses.push(await post("lone-high", (req) => req.end("\uD800")));

    // 1b. Lone low surrogate embedded in ASCII.
    out.expectedHex.loneLow = Buffer.from("A\uDC00B", "utf8").toString("hex");
    out.statuses.push(await post("lone-low", (req) => req.end("A\uDC00B")));

    // 1c. Well-formed pair + ASCII: pure text, must ride the text channel and
    //     stay byte-identical.
    out.expectedHex.pair = Buffer.from("ok\uD83D\uDE00", "utf8").toString("hex");
    out.statuses.push(await post("pair", (req) => req.end("ok\uD83D\uDE00")));

    // 2. A pair split across two write() calls: each call carries a lone
    //    surrogate on its own, so both are refused (chunk-by-chunk guard) and
    //    the aggregate is 2x U+FFFD, never a reassembled pair.
    out.expectedHex.splitPair = Buffer.from("\uD83D", "utf8").toString("hex")
      + Buffer.from("\uDE00", "utf8").toString("hex");
    out.statuses.push(await post("split-pair", (req) => {
      req.write("\uD83D");
      req.end("\uDE00");
    }));

    // 3. Non-utf8 encoding keeps the byte channel with latin1 semantics (the
    //    ASCII case is byte-identical to utf8; the high case is not).
    out.expectedHex.latin1Ascii = Buffer.from("abc", "latin1").toString("hex");
    out.statuses.push(await post("latin1-ascii", (req) => req.end("abc", "latin1")));
    out.expectedHex.latin1High = Buffer.from("caf\u00E9", "latin1").toString("hex");
    out.statuses.push(await post("latin1-high", (req) => req.end("caf\u00E9", "latin1")));

    // 3b. An uppercase utf8 spelling still normalizes to the text channel.
    out.expectedHex.utf8Upper = Buffer.from("h\u00E9llo", "utf8").toString("hex");
    out.statuses.push(await post("utf8-upper", (req) => req.end("h\u00E9llo", "UTF-8")));

    // 4. Empty bodies: no bytes on the wire on either client shape.
    out.expectedHex.emptyEnd = "";
    out.statuses.push(await post("empty-end", (req) => req.end("")));
    const fetched = await fetch("http://127.0.0.1:__PIR_PORT__/fetch-empty", {
      method: "POST",
      body: "",
    });
    await fetched.text();
    out.statuses.push(fetched.status);
    out.expectedHex.emptyFetch = "";

    globalThis.__pirHttpTextChannel = out;
  });
};
"#
    .replace("__PIR_PORT__", &port.to_string());
    let dir = write_temp_fixture("client-text-channel", &[("index.ts", &ts)]);
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir);
    let v = run_fixture(&rt, "client-text-channel", &ts, "__pirHttpTextChannel").await;

    assert_eq!(
        v["statuses"],
        json!([200, 200, 200, 200, 200, 200, 200, 200, 200]),
        "every request must be answered (a lone surrogate on the text channel \
         would make the host reject the \\ud800 escape): {v}"
    );

    // (path, expectedHex key, wire body hex == Buffer.from(chunk, encoding))
    let cases: [(&str, &str, &str); 9] = [
        ("/lone-high", "loneHigh", "efbfbd"),
        ("/lone-low", "loneLow", "41efbfbd42"),
        ("/pair", "pair", "6f6bf09f9880"),
        ("/split-pair", "splitPair", "efbfbdefbfbd"),
        ("/latin1-ascii", "latin1Ascii", "616263"),
        ("/latin1-high", "latin1High", "636166e9"),
        ("/utf8-upper", "utf8Upper", "68c3a96c6c6f"),
        ("/empty-end", "emptyEnd", ""),
        ("/fetch-empty", "emptyFetch", ""),
    ];
    let records = seen.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        cases.len(),
        "one recorded request per case (order = fixture order): {v}"
    );
    for (index, (path, label, expected_hex)) in cases.iter().enumerate() {
        let raw = &records[index];
        let head_end = find(raw, b"\r\n\r\n").map(|p| p + 4).unwrap_or(raw.len());
        let head = String::from_utf8_lossy(&raw[..head_end]);
        assert!(
            head.starts_with(&format!("POST {path} HTTP/1.1\r\n")),
            "case {label}: request line reaches the peer: {head}"
        );
        assert_eq!(
            body_hex(message_body(raw)),
            *expected_hex,
            "case {label}: wire body must equal Buffer.from(chunk, encoding): {head}"
        );
    }
    for (path, label, expected_hex) in cases {
        assert_eq!(
            v["expectedHex"][label],
            json!(expected_hex),
            "case {path}: the fixture's own Buffer.from(...).hex must match the \
             expectation table (it defines the legacy byte channel): {v}"
        );
    }
}

/// Response-side boundaries: `ServerResponse` mirrors the request decision
/// (`_pirSend`, http_shim.js.txt:1149), and the raw Rust client asserts the
/// body bytes the peer actually read (a lossy String read would collapse
/// every surrogate sequence to U+FFFD).
///
/// Why it holds: the same `pirTextChunkEncodable` guard applies per `write`/
/// `end` chunk; a refused chunk drops `_pirBodyTextOk` and `_pirSend` encodes
/// the `Buffer.concat` aggregate as `bodyB64` (byte-identical to the legacy
/// path). `res.end("")` carries no body on either channel, and the host stamps
/// `content-length: 0` from the empty body (http_fns.rs:1412). If red: check
/// (1) the status line per case (a missing response means the JS server never
/// answered — look for a host-side error), (2) whether the failure is only on
/// the lone/split cases (then the guard leaked into the text channel and the
/// host wrote U+FFFD bytes framed by a different content-length).
#[tokio::test(flavor = "current_thread")]
async fn server_text_channel_boundaries_write_the_old_bytes() {
    let dir = write_temp_fixture("server-text-channel", &[]);
    let port_file = dir.join("port.txt");
    let ts = r#"
import http from "node:http";
import fs from "node:fs";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const server = http.createServer((req: any, res: any) => {
      res.writeHead(200, { "content-type": "text/plain" });
      if (req.url === "/text") { res.end("text"); return; }
      if (req.url === "/pair") { res.end("ok\uD83D\uDE00"); return; }
      if (req.url === "/lone") { res.write("\uD800"); res.end(); return; }
      if (req.url === "/split") { res.write("\uD83D"); res.end("\uDE00"); return; }
      res.end("");
    });
    await new Promise((resolve: any) => server.listen(0, resolve));
    fs.writeFileSync("__PIR_PORT_FILE__", String(server.address().port));
    // Resolve when the Rust client publishes `__PIR_DONE__`; 6 s is the bound.
    await new Promise((resolve: any) => {
      (globalThis as any).__pirResolve = resolve;
      const timer = setInterval(() => {
        try {
          if (fs.existsSync("__PIR_DONE__")) { clearInterval(timer); resolve(); }
        } catch {}
      }, 10);
      setTimeout(() => { clearInterval(timer); resolve(); }, 6000);
    });
    server.close();
  });
};
"#
    .replace(
        "__PIR_PORT_FILE__",
        &port_file.to_string_lossy().replace('\\', "/"),
    );
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    emit_parked_fixture(&rt, "server-text-channel", &ts, &port_file, |port| {
        // (path, response body hex == what the legacy byte channel wrote)
        let cases: [(&str, &str); 5] = [
            ("/text", "74657874"),
            ("/pair", "6f6bf09f9880"),
            ("/lone", "efbfbd"),
            ("/split", "efbfbdefbfbd"),
            ("/empty", ""),
        ];
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        for (path, expected_hex) in cases {
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nhost: loopback\r\n\r\n").as_bytes())
                .expect("write request");
            let response = read_one_response_bytes(&mut stream);
            let head_end = find(&response, b"\r\n\r\n")
                .map(|position| position + 4)
                .unwrap_or(response.len());
            let head = String::from_utf8_lossy(&response[..head_end]);
            assert!(
                head.starts_with("HTTP/1.1 200"),
                "case {path}: status line reached the peer: {head}"
            );
            assert_eq!(
                body_hex(message_body(&response)),
                expected_hex,
                "case {path}: response body bytes must equal the legacy byte \
                 channel's image: {head}"
            );
        }
    })
    .await;
}
