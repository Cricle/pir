//! `node:net` minimal loopback TCP: the real server/socket line.
//!
//! The runtime's net tier is one real loopback TCP pipe (`createServer` +
//! `createConnection`) carried by the stream pad's read/write machinery and
//! driven by the `__pir_host.http` net ops (see `http_fns.rs`'s net section
//! and the documented deviations in `js/net_shim.js.txt`). Everything
//! asserted here was checked against node v24.19.0 first; the two official
//! consumers travel in `scripts/node_tests/runner.mjs`
//! (test-stream-base-typechecking is green, and test-process-beforeexit's
//! `tryListen` step now runs).
//!
//! Fixtures are generated at runtime into per-test temp dirs (the repository
//! hygiene gate forbids tracked `.ts` files; same pattern as
//! `tests/http_poll_pacing.rs`).

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-net-shims-{}-{test_name}", std::process::id()));
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

async fn run_fixture(name: &str, ts: &str, global: &str) -> Value {
    let dir = write_temp_fixture(name, &[("index.ts", ts)]);
    let rt = ExtensionRuntime::new(test_config())
        .await
        .expect("runtime construction");
    rt.allow_root(dir);
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

/// The loopback E2E every fixture shares: a server and a client over
/// 127.0.0.1, an ack-paced chunked transfer (the peer's byte-count acks pace
/// the sender, so the same-thread pump can never deadlock on a full window),
/// a multibyte `setEncoding('utf8')` payload sliced mid-character, the close
/// ordering, `bytesRead`/`bytesWritten`, the refused-connection shape and the
/// official `write('broken','buffer')` typecheck. `checks` is a list of
/// `[name, ok, detail]`.
const NET_E2E_TS: &str = r#"
import net from "node:net";
import { Duplex } from "node:stream";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const checks: any[] = [];
    const check = (name: string, ok: boolean, detail?: any) => {
      checks.push([name, Boolean(ok), detail === undefined ? "" : String(detail)]);
    };

    const CHUNK = 4096;
    const CHUNKS = 4; // 16 KiB, ASCII (so setEncoding stays lossless)
    const BULK = CHUNK * CHUNKS;
    const ACK_WIDTH = 6;
    const MULTI = "A中B\ud83d\ude00C".repeat(20);
    const order: string[] = [];
    let bulk = true;
    let ackPending = "";
    let ackBytes = 0;
    const waiters: Array<{ target: number; resolve: () => void }> = [];

    function waitAcks(target: number): Promise<void> {
      if (ackBytes >= target) return Promise.resolve();
      return new Promise<void>((resolve) => waiters.push({ target, resolve }));
    }

    // --- the official write() typecheck, on its own isolated connection -----
    // (after the throw the pad's writable state is left with an outstanding
    // callback, exactly like node's native throw — so it must not share the
    // transfer socket.)
    const probeServer = net.createServer();
    const probePort: number = await new Promise((resolve) =>
      probeServer.listen(0, () => resolve(probeServer.address().port)),
    );
    let typeError: any = null;
    await new Promise<void>((resolve, reject) => {
      const probe = net.connect(probePort, () => {
        try {
          probe.write("broken", "buffer");
        } catch (error: any) {
          typeError = error;
        }
        probe.destroy();
        resolve();
      });
      probe.on("error", reject);
    });
    await new Promise<void>((resolve) => probeServer.close(() => resolve()));
    check(
      "write('broken','buffer') typecheck (official test-stream-base-typechecking)",
      typeError !== null &&
        typeError.name === "TypeError" &&
        typeError.code === "ERR_INVALID_ARG_TYPE" &&
        typeError.message === "Second argument must be a buffer",
      JSON.stringify(typeError && [typeError.name, typeError.code, typeError.message]),
    );

    // --- main server + client, ack-paced chunked transfer --------------------
    const server = net.createServer((socket: any) => {
      let total = 0;
      let text = "";
      let acks = 0;
      socket.setEncoding("utf8");
      socket.setNoDelay(true);
      socket.on("data", (chunk: string) => {
        check("setEncoding yields strings", typeof chunk === "string", typeof chunk);
        const bytes = Buffer.byteLength(chunk, "utf8");
        const bulkPart = Math.max(0, Math.min(bytes, BULK - total));
        if (bulkPart > 0) {
          total += bulkPart;
          acks += bulkPart;
          socket.write(String(bulkPart).padStart(ACK_WIDTH, "0")); // byte-count ack
        }
        if (bulkPart < bytes) text += chunk.slice(bulkPart);
      });
      socket.on("end", () => {
        order.push("server:end");
        check("chunked transfer total", total === BULK, "got " + total + " want " + BULK);
        check("acked every byte", acks === BULK, "got " + acks);
        check("multibyte re-assembly across writes", text === MULTI, JSON.stringify(text.slice(0, 24)));
        socket.end("done:" + total + ":" + text.length);
      });
      socket.on("close", () => order.push("server-conn:close"));
    });

    const address: any = await new Promise((resolve) => server.listen(0, () => resolve(server.address())));
    check(
      "server.address() is loopback IPv4",
      address && address.address === "127.0.0.1" && address.family === "IPv4" && address.port > 0,
      JSON.stringify(address),
    );

    const out: any = { received: "", connect: null as any };
    await new Promise<void>((resolveConnect, rejectConnect) => {
      const client: any = net.createConnection(address.port, async () => {
        try {
          out.connect = {
            remoteAddress: client.remoteAddress,
            remotePort: client.remotePort,
            localPort: client.localPort,
            pending: client.pending,
            isDuplex: client instanceof Duplex,
            isSocket: client instanceof net.Socket,
          };
          client.setEncoding("utf8");
          client.setNoDelay(true);
          client.on("data", (chunk: string) => {
            if (bulk) {
              ackPending += chunk;
              while (ackPending.length >= ACK_WIDTH) {
                ackBytes += Number(ackPending.slice(0, ACK_WIDTH));
                ackPending = ackPending.slice(ACK_WIDTH);
              }
              while (waiters.length > 0 && ackBytes >= waiters[0].target) waiters.shift()!.resolve();
              return;
            }
            out.received += chunk;
          });
          for (let c = 0; c < CHUNKS; c++) {
            const buf = Buffer.alloc(CHUNK);
            for (let i = 0; i < CHUNK; i++) buf[i] = 33 + ((c * 7 + i) % 94);
            client.write(buf);
            await waitAcks((c + 1) * CHUNK);
          }
          bulk = false;
          const payload = Buffer.from(MULTI, "utf8");
          for (let off = 0; off < payload.length; off += 7) {
            client.write(payload.subarray(off, off + 7));
          }
          await new Promise<void>((resolveEnd) => {
            client.on("end", () => {
              order.push("client:end");
              resolveEnd();
            });
            client.end();
          });
          client.on("close", () => order.push("client:close"));
          await delay(20);
          out.bytesWritten = client.bytesWritten;
          out.bytesRead = client.bytesRead;
          resolveConnect();
        } catch (error: any) {
          rejectConnect(error);
        }
      });
      client.on("error", (error: any) => rejectConnect(error));
    });

    check(
      "connect event fields",
      out.connect.remoteAddress === "127.0.0.1" &&
        out.connect.remotePort === address.port &&
        out.connect.pending === false,
      JSON.stringify(out.connect),
    );
    check("Socket is a stream.Duplex", out.connect.isDuplex === true && out.connect.isSocket === true);
    check(
      "bytesWritten counts payload",
      out.bytesWritten === BULK + Buffer.byteLength(MULTI, "utf8"),
      "got " + out.bytesWritten,
    );
    check("bytesRead counts the acks", out.bytesRead > 0, "got " + out.bytesRead);
    check(
      "client received the server echo",
      out.received.indexOf("done:" + BULK + ":") === 0,
      JSON.stringify(out.received.slice(0, 32)),
    );
    check(
      "close order (server end < conn close < client end)",
      order.indexOf("server:end") !== -1 &&
        order.indexOf("server-conn:close") > order.indexOf("server:end") &&
        order.indexOf("client:end") > order.indexOf("server:end"),
      order.join(" > "),
    );

    // server.close() waits for the live connection: the callback lands with
    // the connection gone, and address() reports null after it.
    await new Promise<void>((resolve) => server.close(() => resolve()));
    check("server.address() is null after close", server.address() === null, JSON.stringify(server.address()));

    // Refused connection: node's asynchronous 'error' + 'close' shape.
    const deadServer = net.createServer();
    const deadPort: number = await new Promise((resolve) =>
      deadServer.listen(0, () => resolve(deadServer.address().port)),
    );
    await new Promise<void>((resolve) => deadServer.close(() => resolve()));
    const refusedEvents: string[] = [];
    await new Promise<void>((resolve) => {
      const refused = net.connect(deadPort);
      refused.on("error", (error: any) => refusedEvents.push("error:" + error.code));
      refused.on("close", () => {
        refusedEvents.push("close");
        resolve();
      });
    });
    check(
      "refused connect -> error then close",
      refusedEvents.join(">") === "error:ECONNREFUSED>close",
      refusedEvents.join(">"),
    );

    globalThis.__pirNetE2E = { checks };
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn loopback_echo_typecheck_and_close_ordering() {
    let v = run_fixture("net-e2e", NET_E2E_TS, "__pirNetE2E").await;
    let checks = v["checks"].as_array().expect("checks array");
    let mut failed: Vec<String> = Vec::new();
    for entry in checks {
        let pair = entry.as_array().expect("check entry");
        let name = pair[0].as_str().unwrap_or("<unnamed>");
        let ok = pair[1].as_bool().unwrap_or(false);
        if !ok {
            failed.push(format!("{name} :: {}", pair[2].as_str().unwrap_or("")));
        }
    }
    assert!(
        failed.is_empty(),
        "net loopback checks failed: {failed:?} (all: {v})"
    );
    assert!(
        checks.len() >= 12,
        "the fixture must run every check (got {}): {v}",
        checks.len()
    );
}

/// Argument-shape errors are synchronous, the loopback rule is explicit, and
/// the out-of-tier calls keep one clear error each.
const NET_ARGUMENT_TS: &str = r#"
import net from "node:net";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const attempt = (fn: () => any) => {
      try {
        fn();
        return "no-throw";
      } catch (error: any) {
        return (error.code || error.name) + ": " + error.message;
      }
    };

    out.socketNoArgs = attempt(() => new net.Socket());
    out.serverNoArgs = attempt(() => net.createServer());
    out.badPort = attempt(() => net.connect({ port: 0 }));
    out.nonLoopback = attempt(() => net.connect({ port: 9, host: "198.51.100.7" }));
    out.unixSocketPath = attempt(() => net.connect({ path: "/tmp/does-not-exist.sock" }));
    out.listenUnixPath = attempt(() => net.createServer().listen({ path: "/tmp/nope.sock" }));
    out.listenerHandle = attempt(() => net.createServer().listen({ handle: 1234 }));

    // listen() twice on one server is node's ERR_SERVER_ALREADY_LISTEN.
    const server = net.createServer();
    await new Promise<void>((resolve) => server.listen(0, () => resolve()));
    out.listenTwice = attempt(() => server.listen(0));
    out.addressFamily = server.address().family;
    await new Promise<void>((resolve) => server.close(() => resolve()));
    out.addressAfterClose = server.address();
    out.unrefChainable = server.unref() === server && server.ref() === server;

    // `_createServerHandle` is a private internal: it stays a clear error.
    out.privateHandle = attempt(() => net._createServerHandle());

    globalThis.__pirNetArguments = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn argument_errors_and_loopback_only_rule() {
    let v = run_fixture("net-args", NET_ARGUMENT_TS, "__pirNetArguments").await;

    assert_eq!(v["socketNoArgs"], "no-throw", "new net.Socket(): {v}");
    assert_eq!(v["serverNoArgs"], "no-throw", "net.createServer(): {v}");
    assert!(
        v["badPort"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_SOCKET_BAD_PORT"),
        "port 0 must be rejected: {v}"
    );
    assert!(
        v["nonLoopback"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_NOT_SUPPORTED"),
        "non-loopback hosts are out of tier with a clear error: {v}"
    );
    assert!(
        v["nonLoopback"]
            .as_str()
            .unwrap_or_default()
            .contains("loopback"),
        "the error must say loopback: {v}"
    );
    assert!(
        v["unixSocketPath"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_NOT_SUPPORTED"),
        "Unix-domain sockets are out of tier: {v}"
    );
    assert!(
        v["listenUnixPath"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_NOT_SUPPORTED"),
        "listen(path) is out of tier: {v}"
    );
    assert!(
        v["listenerHandle"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_NOT_SUPPORTED"),
        "listen(handle) is out of tier: {v}"
    );
    assert!(
        v["listenTwice"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_SERVER_ALREADY_LISTEN"),
        "a second listen() must fail like node: {v}"
    );
    assert_eq!(v["addressFamily"], "IPv4", "{v}");
    assert!(
        v["addressAfterClose"].is_null(),
        "address() after close: {v}"
    );
    assert_eq!(v["unrefChainable"], true, "unref/ref stay chainable: {v}");
    assert!(
        v["privateHandle"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ERR_NOT_SUPPORTED"),
        "_createServerHandle stays a clear error: {v}"
    );
}

/// `unref()` releases the loop: the net pump is ref-counted, so once every
/// handle is unref'd/closed its timer is gone from the prelude map the
/// runtime's own liveness probe reads (`__pirTimers`). The process-level
/// consequence — a `pir js` script exiting on its own — is covered by the
/// acceptance script `e2e_unref.cjs`; this pins the mechanism.
const NET_UNREF_TS: &str = r#"
import net from "node:net";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const timers = () => (typeof __pirTimers === "object" && __pirTimers !== null ? __pirTimers.size : -1);
    const turns = async (n: number) => {
      for (let i = 0; i < n; i++) {
        await new Promise<void>((resolve) => {
          if (typeof globalThis.__pirNextTick === "function") globalThis.__pirNextTick(resolve);
          else resolve();
        });
      }
    };

    const server = net.createServer((socket: any) => {
      socket.unref();
      socket.on("data", () => socket.write("pong"));
    });
    const port: number = await new Promise((resolve) => server.listen(0, () => resolve(server.address().port)));
    out.timersWhileListening = timers();
    out.refChainable = server.ref() === server;

    await new Promise<void>((resolve, reject) => {
      const client = net.connect(port, () => {
        client.write("ping");
        client.on("data", () => {
          client.unref();
          server.close(() => resolve());
          client.destroy();
        });
      });
      client.on("error", reject);
    });
    await turns(6);
    out.timersSettled = timers();
    globalThis.__pirNetUnref = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn unref_releases_the_pump() {
    let v = run_fixture("net-unref", NET_UNREF_TS, "__pirNetUnref").await;
    let while_listening = v["timersWhileListening"].as_i64().unwrap_or(-1);
    assert!(
        while_listening >= 1,
        "a ref'd listening server must keep one pump timer armed: {v}"
    );
    assert_eq!(v["refChainable"], true, "{v}");
    // Every handle unref'd/closed and the pump step that ran the callback
    // retired: no prelude timer may be left (that is precisely what lets a
    // process whose handles are all unref'd exit).
    let settled = v["timersSettled"].as_i64().unwrap_or(-1);
    assert!(
        settled <= 0,
        "no prelude timer may remain after every handle is unref'd: {v}"
    );
}
