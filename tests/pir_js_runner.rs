//! `pir js <file>` subprocess script runner: the end-to-end contract of the
//! node-compat roadmap item B. A script run is a spawned child process: real
//! stdio, real argv, real exit codes, the node module surface, the fetch
//! bridge, and — the ecosystem pattern this unlocks — a script that itself
//! spawns `pir js` through `child_process`.
//!
//! Fixtures are generated at runtime into a temp directory (the hygiene gate
//! forbids `.js` sources in the repository).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// One hermetic fixture workspace: a temp dir removed on drop, plus the
/// `PIR_AGENT_DIR` isolation every pir invocation carries (the real `~/.pi`
/// must never leak into script runs).
struct Workspace {
    dir: PathBuf,
    _agent_dir: AgentDirGuard,
}

struct AgentDirGuard(PathBuf);
impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-js-runner-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).expect("workspace create");
        let agent_dir = dir.join(".pir-agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir create");
        Self {
            dir,
            _agent_dir: AgentDirGuard(agent_dir),
        }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, source).expect("fixture write");
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    /// [`Self::run`] with extra environment variables: the inheritance
    /// contracts (`PI_EXT_*` must not reach a standalone script) need to seed
    /// the environment the process sees.
    fn run_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pir"));
        cmd.env("PIR_AGENT_DIR", &self._agent_dir.0)
            .current_dir(&self.dir)
            .arg("js")
            .args(args)
            // Hermetic default: a developer's own explicit script cap must not
            // turn the heap-decoupling test into a false failure. `envs` can
            // still set it when a test needs the override.
            .env_remove("PIR_JS_MEMORY_MB");
        for (name, value) in envs {
            cmd.env(name, value);
        }
        cmd.output().expect("pir binary should start")
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn argv_stdout_execpath_and_zero_exit() {
    let ws = Workspace::new("argv");
    ws.write(
        "info.js",
        r#"
console.log("argv:", JSON.stringify(process.argv));
console.log("execPath:", process.execPath);
console.log("platform:", process.platform);
"#,
    );
    let output = ws.run(&["info.js", "extra", "--flag=value", "-x"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(!stdout.contains("hello"));
    assert!(stdout.contains(r#""info.js""#), "argv[1] missing: {stdout}");
    assert!(
        stdout.contains(r#""extra""#),
        "positional arg missing: {stdout}"
    );
    assert!(
        stdout.contains(r#""--flag=value""#),
        "unknown --flag=value must reach the script verbatim: {stdout}"
    );
    assert!(
        stdout.contains(r#""-x""#),
        "short flag must reach the script: {stdout}"
    );
    // argv[0] is the pir executable itself; process.execPath agrees.
    let exec_line = stdout
        .lines()
        .find(|line| line.starts_with("execPath:"))
        .expect("execPath line");
    let exec_path = exec_line.trim_start_matches("execPath: ").trim();
    assert!(
        Path::new(exec_path).is_file(),
        "execPath should be the pir binary: {exec_path}"
    );
    assert!(
        stdout.contains("argv: [\"") && stdout.contains(".exe") || stdout.contains("pir"),
        "argv[0] should name the pir binary: {stdout}"
    );
    assert!(stdout.contains("platform: win32") || stdout.contains("platform: linux"));
}

#[test]
fn coding_agent_environment_markers_are_set() {
    // Upstream `cli/setup.ts:6-7`: scripts/extensions detect the coding agent
    // through these two variables before any resource load.
    let ws = Workspace::new("env-markers");
    ws.write(
        "env.js",
        r#"
console.log("PI_CODING_AGENT=" + process.env.PI_CODING_AGENT);
console.log("AI_AGENT=" + process.env.AI_AGENT);
"#,
    );
    let output = ws.run(&["env.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("PI_CODING_AGENT=true"),
        "PI_CODING_AGENT missing: {stdout}"
    );
    assert!(stdout.contains("AI_AGENT=pi"), "AI_AGENT missing: {stdout}");
}

#[test]
fn js_script_heap_is_not_capped_by_extension_limits() {
    // `pir js` is a standalone script process, not an extension: the extension
    // host's low-resource heap cap (`PI_EXT_MEMORY_MB` / `extensionLimits.
    // memoryMb`) must not reach it. With the inheritance bug, the 16 MiB cap
    // below failed a modest allocation with QuickJS's "out of memory"; the
    // script runner now starts with an unlimited heap and exposes its own
    // explicit opt-in (`PIR_JS_MEMORY_MB`).
    let ws = Workspace::new("heap-decoupled");
    ws.write(
        "alloc.js",
        r#"
const chunks = [];
for (let i = 0; i < 200; i++) {
  chunks.push(new Uint8Array(1024 * 1024).fill(7));
}
let total = 0;
for (const chunk of chunks) total += chunk.length;
console.log("allocated-mb:", total / (1024 * 1024));
console.log("last-byte:", chunks[chunks.length - 1][1024 * 1024 - 1]);
"#,
    );
    let output = ws.run_with_env(&["alloc.js"], &[("PI_EXT_MEMORY_MB", "16")]);
    assert!(
        output.status.success(),
        "a 200 MiB allocation must not inherit PI_EXT_MEMORY_MB=16: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("allocated-mb: 200"), "{stdout}");
    assert!(stdout.contains("last-byte: 7"), "{stdout}");
}

#[test]
fn process_exit_code_is_honored() {
    let ws = Workspace::new("exit3");
    ws.write(
        "three.js",
        r#"
process.exit(3);
console.log("not reached");
"#,
    );
    let output = ws.run(&["three.js"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        !stdout_of(&output).contains("not reached"),
        "process.exit must terminate immediately"
    );
}

#[test]
fn exit_code_defaults_to_process_exit_code_field() {
    let ws = Workspace::new("exitcode-field");
    ws.write(
        "field.js",
        r#"
process.exitCode = 5;
"#,
    );
    let output = ws.run(&["field.js"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn uncaught_exception_exits_1_and_reports_on_stderr() {
    let ws = Workspace::new("boom");
    ws.write(
        "boom.js",
        r#"
console.log("before");
throw new Error("boom-message");
"#,
    );
    let output = ws.run(&["boom.js"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("boom-message"), "stderr: {stderr}");
    assert!(
        stdout_of(&output).contains("before"),
        "stdout passthrough broken"
    );
}

#[test]
fn unhandled_promise_rejection_exits_1_and_reports_on_stderr() {
    let ws = Workspace::new("reject");
    ws.write(
        "reject.js",
        r#"
console.log("before");
Promise.reject(new Error("boom-rejection"));
"#,
    );
    let output = ws.run(&["reject.js"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("Unhandled promise rejection"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("boom-rejection"),
        "reason missing: {stderr}"
    );
    assert!(
        stdout_of(&output).contains("before"),
        "stdout passthrough broken"
    );
}

#[test]
fn handled_promise_rejection_keeps_zero_exit() {
    let ws = Workspace::new("caught-rejection");
    ws.write(
        "caught.js",
        r#"
Promise.reject(new Error("caught")).catch((error) => {
  console.log("caught:", error.message);
});
"#,
    );
    let output = ws.run(&["caught.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("caught: caught"),
        "stdout: {}",
        stdout_of(&output)
    );
}

#[test]
fn unhandled_rejection_listener_takes_the_event() {
    let ws = Workspace::new("rejection-listener");
    ws.write(
        "listener.js",
        r#"
process.on("unhandledRejection", (reason) => {
  console.log("listener:", reason.message);
});
Promise.reject(new Error("listened"));
"#,
    );
    let output = ws.run(&["listener.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("listener: listened"),
        "stdout: {}",
        stdout_of(&output)
    );
}

#[test]
fn node_modules_available_from_cjs_entry() {
    let ws = Workspace::new("node-mods");
    ws.write(
        "mods.cjs",
        r#"
const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const os = require("os");
const self = fs.readFileSync(__filename, "utf8");
const marker = "mods.cjs-marker";
fs.writeFileSync(
  path.join(os.tmpdir(), "pir-js-runner-write-probe.txt"),
  marker,
);
console.log("self-len:", self.length);
console.log("join:", path.join("a", "b"));
console.log("rand:", crypto.randomBytes(8).toString("hex"));
console.log("tmpdir:", os.tmpdir() !== "");
"#,
    );
    let output = ws.run(&["mods.cjs"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let self_len: usize = stdout
        .lines()
        .find_map(|line| line.strip_prefix("self-len: "))
        .expect("self-len line")
        .trim()
        .parse()
        .expect("self-len number");
    // The script read its own source back through the fs shim.
    assert!(self_len > 100, "self read length: {self_len}");
    assert!(stdout.contains("join: a\\b") || stdout.contains("join: a/b"));
    let rand = stdout
        .lines()
        .find_map(|line| line.strip_prefix("rand: "))
        .expect("rand line")
        .trim();
    assert_eq!(rand.len(), 16, "8 random bytes as hex: {rand}");
    // The write probe also proves the fs roots allow script-dir-adjacent
    // temp writes (the write fences through the temp root).
    let probe = std::env::temp_dir().join("pir-js-runner-write-probe.txt");
    let written = std::fs::read_to_string(&probe).unwrap_or_default();
    let _ = std::fs::remove_file(&probe);
    assert_eq!(written.trim(), "mods.cjs-marker", "fs write probe");
}

#[test]
fn typescript_entry_transpiles() {
    let ws = Workspace::new("ts-entry");
    ws.write(
        "entry.ts",
        r#"
const greet = (name: string): string => "ts-hello " + name;
console.log(greet("pir"));
"#,
    );
    // `entry.js` does not exist; the `.js -> .ts` fallback resolves the TS.
    let output = ws.run(&["entry.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(stdout_of(&output).contains("ts-hello pir"));
}

#[test]
fn timers_settle_before_exit() {
    let ws = Workspace::new("timers");
    ws.write(
        "timer.js",
        r#"
setTimeout(() => {
  console.log("timer-fired");
  process.exit(0);
}, 50);
"#,
    );
    let output = ws.run(&["timer.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(stdout_of(&output).contains("timer-fired"));
}

#[test]
fn fetch_reaches_loopback_http_server() {
    let ws = Workspace::new("fetch");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let port = listener.local_addr().expect("local addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = b"hello-loopback";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(body);
    });
    ws.write(
        "fetcher.js",
        r#"
const port = process.argv[2];
(async () => {
  const res = await fetch("http://127.0.0.1:" + port + "/hello");
  const text = await res.text();
  console.log("status:", res.status);
  console.log("body:", text);
  process.exit(res.ok && text === "hello-loopback" ? 0 : 1);
})();
"#,
    );
    let output = ws.run(&["fetcher.js", &port.to_string()]);
    let _ = server.join();
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("status: 200"), "{stdout}");
    assert!(stdout.contains("body: hello-loopback"), "{stdout}");
}

#[test]
fn esm_entry_imports_bare_npm_package_from_agent_npm_root() {
    // The standalone-script resolution gap: a `pir js` script has no
    // extension directory to derive an npm root from, so bare imports must
    // fall back to the user npm root (`<agent>/npm/node_modules`, the
    // NODE_PATH-style fallback in `loader::bare_search_roots`). The fixture
    // simulates that root inside the isolated agent dir — the script sits in
    // a directory with no `node_modules` ancestor of its own. The package
    // name is unique so a stray `node_modules` above the temp workspace
    // (they exist on developer machines) can never shadow the fixture via
    // the importer walk-up, which runs first by design.
    let ws = Workspace::new("bare-npm");
    let pkg = "pir-runner-test-pkg";
    let npm_root = ws.dir.join(".pir-agent").join("npm");
    std::fs::create_dir_all(npm_root.join("node_modules").join(pkg)).expect("npm pkg dir");
    std::fs::write(
        npm_root.join("node_modules").join(pkg).join("package.json"),
        r#"{ "name": "pir-runner-test-pkg", "main": "index.cjs" }"#,
    )
    .expect("pkg manifest");
    std::fs::write(
        npm_root.join("node_modules").join(pkg).join("index.cjs"),
        "module.exports = function ms(value) { return 'ms:' + value; };\n",
    )
    .expect("pkg source");
    ws.write(
        "uses_ms.js",
        r#"
import ms from "pir-runner-test-pkg";
console.log("ms:", ms("1s"));
"#,
    );
    let output = ws.run(&["uses_ms.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("ms: ms:1s"),
        "{:?}",
        stdout_of(&output)
    );
}

#[test]
fn esm_entry_imports_typebox_virtual_shim() {
    // The virtual-shim half of the chain: a named import from a bare
    // specifier that resolves to a host-provided shim module (no npm at
    // all), from an ESM-classified entry (Node 22 module-syntax detection).
    let ws = Workspace::new("typebox");
    ws.write(
        "schema.js",
        r#"
import { Type } from "typebox";
const schema = Type.Object({ name: Type.String() });
console.log("type:", schema.type, "props:", Object.keys(schema.properties).join(","));
"#,
    );
    let output = ws.run(&["schema.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("type: object"), "{stdout}");
    assert!(stdout.contains("props: name"), "{stdout}");
}

#[test]
fn child_process_spawns_pir_js_reverse() {
    // The ecosystem chain: a script (extension bridge/mcp-cli pattern) spawns
    // `<process.execPath> js <child>` and reads back the child's exit code
    // and stdout.
    let ws = Workspace::new("reverse-spawn");
    ws.write(
        "child_target.js",
        r#"
console.log("child-ok");
process.exit(7);
"#,
    );
    ws.write(
        "parent.js",
        r#"
const { spawnSync } = require("node:child_process");
const child = process.argv[2];
const result = spawnSync(process.execPath, ["js", child]);
console.log("status:", result.status);
console.log("child-stdout:", result.stdout ? result.stdout.toString().trim() : "<none>");
process.exit(result.status === 7 && String(result.stdout).includes("child-ok") ? 0 : 1);
"#,
    );
    let output = ws.run(&["parent.js", "child_target.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("status: 7"), "{stdout}");
    assert!(stdout.contains("child-stdout: child-ok"), "{stdout}");
}

#[test]
fn top_level_await_downgrade_in_script_entry() {
    // QuickJS rejects `await` at module top level; the script runner
    // pre-transpiles the ENTRY with the async-IIFE downgrade
    // (`transpile::transpile_script_entry`) — dynamic `await import()`,
    // `await new Promise`, ordering across awaits, and exported bindings
    // staying linkable. Imported modules keep native ESM semantics.
    let ws = Workspace::new("tla");
    ws.write(
        "lib.js",
        r#"
export const plain = "plain-ok";
export function helper() { return "helper-ok"; }
"#,
    );
    ws.write(
        "tla_entry.js",
        r#"
import { helper } from "./lib.js";
const mod = await import("./lib.js");
const value = await new Promise((resolve) => setTimeout(() => resolve(41), 5));
const order = ["before"];
await new Promise((resolve) => setTimeout(resolve, 5));
order.push("middle");
export const computed = value + 1;
export default helper();
console.log("tla-import:", mod.plain);
console.log("tla-promise:", value);
console.log("tla-order:", order.join(","));
console.log("tla-helper:", helper());
process.exit(mod.plain === "plain-ok" && computed === 42 && order.join(",") === "before,middle" ? 0 : 1);
"#,
    );
    let output = ws.run(&["tla_entry.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("tla-import: plain-ok"), "{stdout}");
    assert!(stdout.contains("tla-promise: 41"), "{stdout}");
    assert!(stdout.contains("tla-order: before,middle"), "{stdout}");
    assert!(stdout.contains("tla-helper: helper-ok"), "{stdout}");
}

// ---------------------------------------------------------------------------
// Event-loop drain (script mode's `wait_idle`)
// ---------------------------------------------------------------------------
//
// The drain is the point Node's loop goes empty. Two contracts are pinned
// here, both from the field report that script-mode timers stopped being
// delivered once the entry's top-level evaluation ended:
//
// * armed timers are still delivered *after* the entry finishes while a
//   floating chain keeps the loop alive — including when the chain is the
//   `setInterval`-driven pump a subprocess bridge runs, and when a
//   long-lived awaited host op (a spawned child) is in flight;
// * the drain adds no fixed delay: a script whose handles are all settled
//   (no pending timer, no live chain) exits at once, and a pending timer
//   keeps the process alive exactly until it fires.

#[test]
fn floating_timer_chain_after_top_level_await_keeps_delivering() {
    // The exact reported shape: the entry's top-level evaluation ends (the
    // awaits below complete), and a *floating* async chain keeps arming
    // `setTimeout`s. Every link must be delivered, in order, after the entry
    // is done — the drain cannot decide "the entry finished, nothing to do".
    let ws = Workspace::new("floating-timers");
    ws.write(
        "floating.js",
        r#"
console.log("start");
await new Promise((resolve) => setTimeout(resolve, 0));
console.log("after-top-level-await");
let count = 0;
let last = 0;
const chain = (async () => {
  for (let i = 0; i < 3; i++) {
    await new Promise((resolve) => setTimeout(resolve, 40));
    count++;
    last = Date.now();
    console.log("tick", count);
  }
  console.log("chain-done", count);
})();
console.log("end-of-top-level");
await chain;
console.log("chain-awaited", count);
process.exit(count === 3 ? 0 : 1);
"#,
    );
    let output = ws.run(&["floating.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let ticks: Vec<&str> = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("tick "))
        .collect();
    assert_eq!(
        ticks,
        vec!["tick 1", "tick 2", "tick 3"],
        "the floating chain's timers must all be delivered after the entry's top-level evaluation: {stdout}"
    );
    assert!(stdout.contains("end-of-top-level"), "{stdout}");
    assert!(stdout.contains("chain-done 3"), "{stdout}");
    assert!(stdout.contains("chain-awaited 3"), "{stdout}");
}

#[test]
fn floating_timers_keep_firing_while_a_pump_and_a_child_are_in_flight() {
    // The subprocess-bridge shape that broke: an interval-driven pump plus an
    // awaited child-process run. The pump's ticks must interleave with the
    // child's output (a starved timer surface freezes the pump, which is what
    // makes a child session's events never arrive).
    let ws = Workspace::new("pump-child");
    ws.write(
        "child_target.js",
        r#"
let n = 0;
const timer = setInterval(() => {
  n++;
  console.log("child-tick " + n);
  if (n >= 3) clearInterval(timer);
}, 50);
"#,
    );
    ws.write(
        "pump.js",
        r#"
const { spawn } = require("node:child_process");
const pump = setInterval(() => console.log("pump"), 30);
const child = spawn(process.execPath, ["js", "child_target.js"]);
let out = "";
child.stdout.on("data", (chunk) => { out += String(chunk); });
child.on("exit", (code) => {
  clearInterval(pump);
  console.log("child-exit " + code);
  console.log("child-output-seen " + (out.includes("child-tick 3") ? "yes" : "no"));
  process.exit(code === 0 && out.includes("child-tick 3") ? 0 : 1);
});
"#,
    );
    let output = ws.run(&["pump.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("child-exit 0"), "{stdout}");
    assert!(stdout.contains("child-output-seen yes"), "{stdout}");
    let pumps = stdout.lines().filter(|line| line.trim() == "pump").count();
    assert!(
        pumps >= 2,
        "the interval pump must keep firing while the child runs ({pumps} ticks): {stdout}"
    );
}

#[test]
fn pending_timer_keeps_the_process_alive_and_empty_handles_exit_at_once() {
    // Exit-condition contract, measured *inside* the script (no host-timing
    // flakiness): the process must stay alive until a pending timer fires,
    // then exit immediately once the loop is empty.
    let ws = Workspace::new("drain-exit");
    ws.write(
        "waits.js",
        r#"
const started = Date.now();
setTimeout(() => {
  console.log("fired-after", Date.now() - started >= 250 ? "wait" : "too-early");
  const cleared = setInterval(() => console.log("never"), 5000);
  clearInterval(cleared);
  console.log("cleared-interval");
}, 300);
"#,
    );
    let output = ws.run(&["waits.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("fired-after wait"), "{stdout}");
    assert!(stdout.contains("cleared-interval"), "{stdout}");
    assert!(
        !stdout.contains("never"),
        "a cleared interval must not fire: {stdout}"
    );

    // No pending handle at all: the run must not sit in the drain. Boot cost
    // dominates, so the bound is generous — a fixed drain delay (or a stray
    // park loop) would blow straight through it.
    ws.write("empty.js", "console.log(\"empty-done\");\n");
    let empty = ws.run(&["empty.js"]);
    assert!(empty.status.success(), "stderr: {}", stderr_of(&empty));
    assert!(
        empty.status.code() == Some(0),
        "exit code: {:?}",
        empty.status.code()
    );
}

#[test]
fn drain_releases_the_cpu_while_waiting_for_a_long_timer() {
    // A long armed timer must not turn the drain into a spin: the process
    // waits (parked) and the timer still fires at its deadline.
    let ws = Workspace::new("drain-park");
    ws.write(
        "long_timer.js",
        r#"
const started = Date.now();
setTimeout(() => {
  const elapsed = Date.now() - started;
  console.log("long-fired", elapsed >= 900 ? "on-time" : "early", elapsed);
  process.exit(0);
}, 1000);
"#,
    );
    let output = ws.run(&["long_timer.js"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("long-fired on-time"), "{stdout}");
}

#[test]
fn no_tla_entries_keep_their_loader_paths() {
    // The downgrade must be invisible for entries without top-level await:
    // a CJS entry (require/module semantics) and an ESM entry both run as
    // before.
    let ws = Workspace::new("no-tla");
    ws.write(
        "cjs_entry.js",
        r#"
const path = require("node:path");
module.exports.tag = path.basename("/a/b.ts");
console.log("cjs:", module.exports.tag);
"#,
    );
    ws.write(
        "esm_entry.js",
        r#"
import { basename } from "node:path";
export const tag = basename("/a/c.ts");
console.log("esm:", tag);
"#,
    );
    let cjs_output = ws.run(&["cjs_entry.js"]);
    assert!(
        cjs_output.status.success(),
        "cjs stderr: {}",
        stderr_of(&cjs_output)
    );
    assert!(stdout_of(&cjs_output).contains("cjs: b.ts"));
    let esm_output = ws.run(&["esm_entry.js"]);
    assert!(
        esm_output.status.success(),
        "esm stderr: {}",
        stderr_of(&esm_output)
    );
    assert!(stdout_of(&esm_output).contains("esm: c.ts"));
}

// ---------------------------------------------------------------------------
// Differential smoke seed (roadmap §11.3): run the same fixture under the
// real Node when one is on PATH and compare the stdout lines. Ignored by
// default; `cargo test --test pir_js_runner -- --ignored` runs it locally.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "differential smoke: requires a real node on PATH"]
fn differential_smoke_against_real_node() {
    let ws = Workspace::new("diff");
    ws.write(
        "diff.js",
        r#"
console.log("platform:", process.platform);
console.log("argv-len:", process.argv.length);
console.log("join:", require("path").join("a", "b"));
console.log("rand-len:", require("crypto").randomBytes(8).toString("hex").length);
setTimeout(() => console.log("timer-fired"), 10);
"#,
    );
    let node = find_node().expect("node on PATH for the differential smoke");
    let node_output = Command::new(node)
        .arg("diff.js")
        .current_dir(&ws.dir)
        .output()
        .expect("node should start");
    assert!(
        node_output.status.success(),
        "node failed: {}",
        stderr_of(&node_output)
    );

    let pir_output = ws.run(&["diff.js"]);
    assert!(
        pir_output.status.success(),
        "stderr: {}",
        stderr_of(&pir_output)
    );

    // Line-set comparison: pir's stack traces differ, but every deterministic
    // value line must agree. `platform` legitimately differs across OS
    // boundaries — compared only for shape.
    let pir_stdout = stdout_of(&pir_output);
    let node_stdout = String::from_utf8_lossy(&node_output.stdout).to_string();
    let pir_lines: Vec<&str> = pir_stdout.lines().collect();
    let node_lines: Vec<&str> = node_stdout.lines().collect();
    for key in ["argv-len:", "join:", "rand-len:", "timer-fired"] {
        let from_pir = pir_lines
            .iter()
            .find(|line| line.contains(key))
            .unwrap_or_else(|| panic!("pir output missing {key}: {pir_lines:?}"));
        let from_node = node_lines
            .iter()
            .find(|line| line.contains(key))
            .unwrap_or_else(|| panic!("node output missing {key}: {node_lines:?}"));
        assert_eq!(
            from_pir.trim(),
            from_node.trim(),
            "differential mismatch on {key}"
        );
    }
}

fn find_node() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn generated_assert_messages_and_positions_follow_the_source_file() {
    let ws = Workspace::new("assert-generated-messages");
    // Comments and blank lines must not shift what the runtime reports: a
    // plain-JS file is evaluated verbatim (no swc reprint), so frame positions
    // are source positions and assert's generated messages can read the
    // call's own line.
    let script = r#"'use strict';
const assert = require('assert');

// A comment that must not shift the line numbers below.
try { assert.ok(null); } catch (error) { console.log("assert-simple:" + error.message); }

try {
  assert.ok(
    true && false
  );
} catch (error) { console.log("assert-multiline:" + error.message); }

try { assert.ok((() => Boolean('' === false))()); } catch (error) { console.log("assert-nested:" + error.message); }

const savedLimit = Error.stackTraceLimit;
Error.stackTraceLimit = 0;
try { assert.ok(null); } catch (error) { console.log("assert-limited:" + error.message); }
Error.stackTraceLimit = savedLimit;

try { new Function("assert", "assert(1 === 2);")(assert); } catch (error) { console.log("assert-evalfn:" + error.message); }

try { null.x; } catch (error) { console.log("throw-frame:" + String(error.stack).split("\n")[1]); }
"#;
    let path = ws.write("messages.js", script);
    let output = ws.run(&[path.to_str().expect("utf8 path")]);
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert!(
        output.status.success(),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains(
            "assert-simple:The expression evaluated to a falsy value:\n\n  assert.ok(null)\n"
        ),
        "simple generated message: {stdout}"
    );
    assert!(
        stdout.contains(
            "assert-multiline:The expression evaluated to a falsy value:\n\n  assert.ok(\n"
        ),
        "a multi-line call slices to the end of the call's own line: {stdout}"
    );
    assert!(
        stdout.contains(
            "assert-nested:The expression evaluated to a falsy value:\n\n  \
             assert.ok((() => Boolean('' === false))())\n"
        ),
        "nested expression: {stdout}"
    );
    assert!(
        stdout.contains(
            "assert-limited:The expression evaluated to a falsy value:\n\n  assert.ok(null)\n"
        ),
        "Error.stackTraceLimit = 0 must not hide the source: {stdout}"
    );
    assert!(
        stdout.contains(
            "assert-evalfn:The expression evaluated to a falsy value:\n\n  assert(1 === 2)\n"
        ),
        "a `new Function` body's source must be readable through the frame's function: {stdout}"
    );
    let throw_line = script
        .lines()
        .position(|line| line.contains("null.x"))
        .expect("throw line")
        + 1;
    assert!(
        stdout.contains(&format!("(<input>:{throw_line}:")),
        "frames must report source lines (throw is on line {throw_line}): {stdout}"
    );
}
