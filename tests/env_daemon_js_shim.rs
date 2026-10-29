//! `@earendil-works/pi-env` client shim — the JS half of the env protocol
//! surface, driven through the extension runtime against pir's own daemon.
//!
//! The shim source (`src/extensions/api/js/pi_env_shim.js.txt`) is copied into
//! the fixture directory and imported by relative path, so this test does not
//! depend on the `shims.rs` registration (a one-line-per-table change owned by
//! the main session — see the wave report).
//!
//! What is exercised, in one loaded extension:
//!   * `Connection` against `[<pir>, "env-daemon"]`: lazy start, the
//!     `PI-ENV <token>` sync line, the `hello` handshake (protocol 1 +
//!     `features`), request/response id matching with two requests in flight,
//!     cancel-on-abort, `close()`, and `isConnectionLost` on a failed start.
//!   * `RemoteExecutionEnv`: createTempDir → writeFile → readTextFile →
//!     readTextLines → fileInfo → listFiles/listDir → exec (streamed output +
//!     exit code) → abort → error mapping (`not_found`) → remove.
//!
//! Fixtures are generated at runtime into a temp directory (the hygiene gate
//! forbids tracked `.js`/`.ts` sources).

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// The shim under test, included as source so the fixture copy is byte-identical.
const SHIM: &str = include_str!("../src/extensions/api/js/pi_env_shim.js.txt");

/// The fixture extension: registers one tool that drives the env client and
/// reports everything it saw as JSON.
const PROBE_TS: &str = r#"
import { Connection, RemoteExecutionEnv, localDaemon, isConnectionLost } from "./pi_env_shim.mjs";

const EXE = "__PIR_EXE__";

export default (pi: any) => {
  pi.registerTool({
    name: "env_probe",
    description: "Drive pir's built-in pi-env daemon through the @earendil-works/pi-env client shim",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const log: any = {};
      try {
      const connection = new Connection({ command: [EXE, "env-daemon"] });
      const info = await connection.info();
      log.step = "hello";
      log.protocol = info.protocol;
      log.features = info.features;
      log.os = info.os;
      log.infoKeys = Object.keys(info).length;
      // The default local command: this runtime's process.execPath is the pir
      // image name, so the daemon subcommand rides along.
      log.localDaemon = localDaemon();
      log.isConnectionLost = typeof isConnectionLost === "function";

      const env = new RemoteExecutionEnv({ connection, id: "js-probe", cwd: info.tmpdir });
      const dir = await env.createTempDir("pir-js-env-", {});
      log.step = "tempdir";
      log.tempdir = dir.ok && typeof dir.value === "string" && dir.value.length > 0;
      const file = dir.value + "/probe.txt";
      log.write = (await env.writeFile(file, "first\nsecond\n", {})).ok;
      log.read = (await env.readTextFile(file, {})).value;
      log.lines = (await env.readTextLines(file, { maxLines: 1 }, {})).value;
      log.step = "read";
      const stat = await env.fileInfo(file, {});
      log.fileKind = stat.value.kind;
      log.fileSize = stat.value.size;

      const listing = (await env.listFiles(dir.value, {}, {})).value;
      log.listFiles = listing.entries.map((entry: any) => entry.kind + ":" + entry.path);
      log.listDir = (await env.listDir(dir.value, {})).value.length;
      log.step = "list";

      // exec: the daemon streams decoded output events and reports the exit code.
      const seen: any[] = [];
      const result = await env.exec(
        "echo js-stdout && echo js-stderr 1>&2 && exit 4",
        { onOutput: (text: string, _context: any, out: any) => seen.push(out.stream + "=" + text.trim()) },
        {},
      );
      log.exitCode = result.value.exitCode;
      log.output = seen.sort();
      log.step = "exec";

      // Abort: the client sends `cancel` and the request settles as `aborted`.
      const controller = new AbortController();
      const long = process.platform === "win32" ? "ping -n 20 127.0.0.1 > /dev/null" : "sleep 20";
      const running = env.exec(long, {}, { abortSignal: controller.signal });
      setTimeout(() => controller.abort(), 150);
      const aborted = await running;
      log.aborted = aborted.ok ? "resolved" : aborted.error.code;
      log.step = "abort";

      // Error mapping: the daemon's ENOENT becomes FileError `not_found`.
      const missing = await env.fileInfo(dir.value + "/nope.txt", {});
      log.missing = missing.ok ? "resolved" : missing.error.code;

      // Two requests in flight on one connection: responses match by id.
      const both = await Promise.all([
        connection.request("lstat", { path: info.tmpdir }),
        connection.request("hello", { protocol: 1 }),
      ]);
      log.interleaved = [both[0].json.kind, both[1].json.protocol];

      log.cleanup = (await env.remove(dir.value, { recursive: true, force: true }, {})).ok;
      connection.close();

      // Connecting to a program that cannot start fails as a lost connection
      // (the retryable class), not as a daemon refusal.
      const bogus = new Connection({ command: ["definitely-not-a-real-program-xyz"] });
      try {
        await bogus.info();
        log.lostOnFailedStart = "resolved";
      } catch (error: any) {
        log.lostOnFailedStart = isConnectionLost(error) + ":" + error.code;
      }
      } catch (error: any) {
        // The tool must not merely fail: report where and why, so a regression
        // names the API instead of "tool failed".
        log.error = String(error && error.message ? error.message : error);
        log.stack = String(error && error.stack ? error.stack : "");
      }
      return { content: [{ type: "text", text: JSON.stringify(log) }] };
    },
  });
};
"#;

/// See tests/node_shims.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-env-js-{}-{test_name}", std::process::id()));
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

/// The test binary's `pir` image, spelled for a JS string literal (forward
/// slashes: the child_process host accepts them on Windows too).
fn pir_executable() -> String {
    env!("CARGO_BIN_EXE_pir").replace('\\', "/")
}

async fn run_probe() -> Value {
    let probe = PROBE_TS.replace("__PIR_EXE__", &pir_executable());
    let dir = write_temp_fixture("probe", &[("index.ts", &probe), ("pi_env_shim.mjs", SHIM)]);
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    let loaded = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        loaded.errors.is_empty(),
        "the pi-env client fixture must load (shim + extension): {:?}",
        loaded.errors
    );
    assert_eq!(loaded.loaded.len(), 1, "one loaded entry: {loaded:?}");
    let outcome = rt
        .call_tool("env_probe", "call-env-probe", &json!({}))
        .await
        .expect("tool executes");
    assert_eq!(outcome.content.len(), 1);
    assert_eq!(
        outcome.content[0].kind, "text",
        "tool result shape: {outcome:?}"
    );
    serde_json::from_str(&outcome.content[0].text).expect("probe answers JSON")
}

#[tokio::test(flavor = "current_thread")]
async fn env_client_shim_round_trips_the_protocol_against_pir_daemon() {
    let log = run_probe().await;
    assert!(
        log.get("error").is_none(),
        "the env client probe must not fail at step {:?}: {} / {}",
        log.get("step"),
        log.get("error").cloned().unwrap_or_default(),
        log.get("stack").cloned().unwrap_or_default()
    );

    // hello: the documented machine report, protocol 1 with the feature list.
    assert_eq!(log["protocol"], json!(1), "hello.protocol: {log}");
    assert!(
        log["features"]
            .as_array()
            .is_some_and(|features| features.iter().any(|value| value == "listFiles")),
        "hello.features: {log}"
    );
    assert_eq!(
        log["os"],
        json!(if cfg!(windows) {
            "windows"
        } else {
            std::env::consts::OS
        }),
        "{log}"
    );
    assert!(
        log["infoKeys"].as_u64().is_some_and(|count| count >= 9),
        "{log}"
    );
    // `localDaemon()` names the daemon subcommand of the running image: `pir`
    // itself, or the PIR_EXECUTABLE this host exported (the test binary under
    // `cargo test`), so the image path is asserted to be present, not fixed.
    let local_daemon = log["localDaemon"].as_array().expect("localDaemon argv");
    assert_eq!(local_daemon.len(), 2, "{log}");
    assert_eq!(local_daemon[1], json!("env-daemon"), "{log}");
    assert!(
        local_daemon[0]
            .as_str()
            .is_some_and(|image| !image.is_empty()),
        "localDaemon names its image: {log}"
    );
    assert_eq!(log["isConnectionLost"], json!(true));

    // File family: write → read → line reader → lstat.
    assert_eq!(log["tempdir"], json!(true), "{log}");
    assert_eq!(log["write"], json!(true));
    assert_eq!(log["read"], json!("first\nsecond\n"));
    assert_eq!(log["lines"], json!(["first"]));
    assert_eq!(log["fileKind"], json!("file"));
    // "first\nsecond\n" is 13 bytes (the daemon's lstat is Node's).
    assert_eq!(log["fileSize"], json!(13));

    // listFiles payload decoding and the directory reader.
    assert_eq!(log["listFiles"], json!(["file:probe.txt"]), "{log}");
    assert_eq!(log["listDir"], json!(1));

    // exec: stdout/stderr events arrive per stream and the exit code is the process's.
    assert_eq!(log["exitCode"], json!(4), "{log}");
    assert_eq!(
        log["output"],
        json!(["stderr=js-stderr", "stdout=js-stdout"]),
        "{log}"
    );

    // Cancellation and error mapping.
    assert_eq!(log["aborted"], json!("aborted"), "{log}");
    assert_eq!(log["missing"], json!("not_found"), "{log}");

    // Requests in flight together stay matched by id.
    assert_eq!(log["interleaved"], json!(["directory", 1]), "{log}");

    // Cleanup through the daemon, and a failed start is the retryable class.
    assert_eq!(log["cleanup"], json!(true), "{log}");
    assert_eq!(log["lostOnFailedStart"], json!("true:spawn_error"), "{log}");
}

/// A second fixture: the parts of the surface that are pure or locally bound,
/// read back from a global the factory writes.
const SURFACE_TS: &str = r#"
import { Connection, LocalExecutionEnv, nodePlatform, sshArguments } from "./pi_env_shim.mjs";

export default (pi: any) => {
  const seen: any = {};
  // `LocalExecutionEnv` without a connection creates one from `localDaemon()`
  // lazily (nothing is spawned until the first request), and the daemon
  // subcommand rides on the shared command shape.
  const local = new (LocalExecutionEnv as any)({ id: "local-probe", cwd: "/" });
  seen.localId = local.id;
  seen.localExec = typeof local.exec === "function" && typeof local.listFiles === "function";
  seen.localDispose = typeof local.dispose === "function";
  seen.ownsConnection = local.connection instanceof Connection;
  local.dispose();

  seen.platform = nodePlatform({ os: "windows", arch: "aarch64" });
  seen.ssh = sshArguments({ host: "example.com", knownHostsFile: "/tmp/known_hosts", hostKeyAlias: "pi-env-x" });
  (globalThis as any).__envShimSurface = seen;
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn env_client_shim_surface_is_thin_and_local() {
    let dir = write_temp_fixture(
        "surface",
        &[("index.ts", SURFACE_TS), ("pi_env_shim.mjs", SHIM)],
    );
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    let loaded = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        loaded.errors.is_empty(),
        "the surface fixture must load: {:?}",
        loaded.errors
    );
    let seen: Value = rt
        .eval_json("JSON.stringify(globalThis.__envShimSurface)")
        .await
        .expect("fixture results");
    // `LocalExecutionEnv` is a real, thin local environment: the daemon carries
    // the file/exec surface (durable's NodeExecutionEnv is D4), and the
    // connection it creates itself can be released with `dispose()`.
    assert_eq!(seen["localId"], json!("local-probe"), "{seen}");
    assert_eq!(seen["localExec"], json!(true), "{seen}");
    assert_eq!(seen["localDispose"], json!(true), "{seen}");
    assert_eq!(seen["ownsConnection"], json!(true), "{seen}");
    // `nodePlatform` maps Rust's spellings to Node's.
    assert_eq!(
        seen["platform"],
        json!({ "platform": "win32", "arch": "arm64" })
    );
    // `sshArguments` is upstream's hardened `ssh` argv, alias and `--` included.
    let ssh = seen["ssh"].as_array().expect("ssh argv");
    let ssh: Vec<&str> = ssh.iter().filter_map(Value::as_str).collect();
    assert!(ssh.contains(&"BatchMode=yes"), "{ssh:?}");
    assert!(ssh.contains(&"ClearAllForwardings=yes"), "{ssh:?}");
    assert!(ssh.contains(&"HostKeyAlias=pi-env-x"), "{ssh:?}");
    assert!(
        ssh.contains(&"-T") && ssh.contains(&"-a") && ssh.contains(&"-x"),
        "{ssh:?}"
    );
    assert_eq!(ssh[ssh.len() - 2..], ["--", "example.com"], "{ssh:?}");
}

#[test]
fn shim_source_stays_within_the_runtime_surface() {
    // Cheap guards on the shim source itself: no TypeScript syntax, no
    // CommonJS `require`, and the imports are all runtime-provided builtins
    // (the virtual-module contract the loader enforces).
    assert!(!SHIM.contains("require("), "the shim must be pure ESM");
    assert!(!SHIM.contains(": string"), "the shim must be plain JS");
    for line in SHIM.lines() {
        if let Some(rest) = line.trim().strip_prefix("import ") {
            let specifier = rest
                .split('"')
                .nth(1)
                .unwrap_or_else(|| panic!("unparsable import line: {line}"));
            // node builtins, plus the one virtual module wave D4 registered
            // for it: the durable `env` contract the client and
            // `NodeExecutionEnv` now share (`@earendil-works/pi-durable/env`).
            assert!(
                specifier.starts_with("node:")
                    || specifier == "@earendil-works/pi-durable/env",
                "the shim may only import node builtins and the durable env contract, found {specifier}"
            );
        }
    }
    assert!(
        SHIM.contains("export {"),
        "the shim exports a named surface"
    );
    assert!(SHIM.contains("class Connection"), "the client is present");
}
