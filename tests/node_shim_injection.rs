//! Node executable shim injection: `spawn("node", …)` / `spawnSync("node", …)`
//! / `execFile("node", …)` / `exec("node …")` inside pir's extension runtime
//! must route to pir's own JS runner (`<pir.exe> js …`) instead of looking for
//! a system node.
//!
//! The fixtures are generated at runtime into a temp workspace (the hygiene
//! gate forbids `.js` sources in the repository) and are driven by a real
//! `pir js <driver.js>` child process: that is what makes the assertions
//! meaningful — inside the driver, `std::env::current_exe()` is the pir
//! binary, which is exactly the condition the shim routes on. The scripted
//! probes mirror pi-subagents' call surface (bare `node`/`node.exe`,
//! `-e <code>`, `<script> <args>`, `--import <preload>` prefix, detached +
//! `unref()`), plus the regression that non-node programs keep the original
//! spawn path.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

/// One hermetic fixture workspace: a temp dir removed on drop, plus the
/// `PIR_AGENT_DIR` isolation every pir invocation carries (the real `~/.pi`
/// must never leak into these runs).
struct Workspace {
    dir: PathBuf,
    agent_dir: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-node-shim-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).expect("workspace create");
        let agent_dir = dir.join(".pir-agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir create");
        Self { dir, agent_dir }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, source).expect("fixture write");
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pir"))
            .env("PIR_AGENT_DIR", &self.agent_dir)
            .current_dir(&self.dir)
            .arg("js")
            .args(args)
            .output()
            .expect("pir binary should start")
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// The driver reports one JSON object per probe on stdout (`name => {json}`),
/// so a failing case is readable from the assertion message.
fn report(stdout: &str, name: &str) -> Value {
    let line = stdout
        .lines()
        .find(|line| line.starts_with(&format!("{name} => ")))
        .unwrap_or_else(|| panic!("no `{name}` line in driver stdout:\n{stdout}"));
    serde_json::from_str(line.trim_start_matches(&format!("{name} => ")))
        .unwrap_or_else(|error| panic!("`{name}` line is not JSON ({error}): {line}"))
}

/// The async/blocking spawn probes a driver needs. Kept in one place so the
/// fixtures stay short.
const CHILD_HELPERS: &str = r#"
const { spawn, spawnSync, exec, execFile, execSync } = require("node:child_process");
function spawnOut(cmd, args, input) {
  return new Promise(function (resolve) {
    const child = spawn(cmd, args, { windowsHide: true });
    let out = "", err = "";
    if (child.stdout) child.stdout.on("data", function (c) { out += c.toString("utf8"); });
    if (child.stderr) child.stderr.on("data", function (c) { err += c.toString("utf8"); });
    child.on("error", function (e) { resolve({ error: String((e && e.code) || e) }); });
    child.on("close", function (code) { resolve({ code: code, out: out, err: err }); });
    if (input !== undefined && input !== null) child.stdin.write(input);
    child.stdin.end();
  });
}
function execFileOut(file, args) {
  return new Promise(function (resolve) {
    execFile(file, args, function (error, stdout, stderr) {
      resolve({ error: error ? String(error) : null, stdout: String(stdout).trim(), stderr: String(stderr).trim() });
    });
  });
}
function execShellOut(command) {
  return new Promise(function (resolve) {
    exec(command, function (error, stdout, stderr) {
      resolve({ error: error ? String(error.message) : null, stdout: String(stdout).trim(), stderr: String(stderr).trim() });
    });
  });
}
function report(name, value) { console.log(name + " => " + JSON.stringify(value)); }
"#;

/// (a) `spawn("node", ["-e", …])`, (b) `node script.js a b`, (c)
/// `execFile("node", ["--version"])`, plus the blocking and shell variants —
/// every entry point the child_process shim exposes funnels into the same
/// router, so one driver covers them all.
#[test]
fn node_spawns_route_to_the_pir_js_runner() {
    let ws = Workspace::new("route");
    ws.write(
        "child.js",
        r#"
console.log(JSON.stringify({
  argv: process.argv,
  shim: process.env.PIR_NODE_SHIM,
  execPath: process.execPath,
}));
"#,
    );
    ws.write(
        "driver.js",
        &format!(
            r#"{CHILD_HELPERS}
async function main() {{
  report("eval", await spawnOut("node", ["-e", "console.log(1+1); console.log('SHIM=' + process.env.PIR_NODE_SHIM)"]));
  report("file", await spawnOut("node", ["child.js", "a", "b"]));
  report("version", await execFileOut("node", ["--version"]));
  report("versionShort", await execFileOut("node.exe", ["-v"]));
  const sync = spawnSync("node", ["-e", "console.log('SYNC_OK')"]);
  report("sync", {{ status: sync.status, stdout: String(sync.stdout).trim(), stderr: String(sync.stderr).trim(), error: sync.error ? String(sync.error) : null }});
  report("execShell", await execShellOut("node -e \"console.log('EXEC_SHELL_OK')\""));
  report("execSyncShell", (function () {{
    try {{ return execSync("node -e \"console.log('EXECSYNC_OK')\"").trim(); }}
    catch (e) {{ return "ERR: " + e.message; }}
  }})());
  console.log("DONE");
}}
main();
"#
        ),
    );

    let output = ws.run(&["driver.js"]);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success() && stdout.contains("DONE"),
        "driver must run to completion.\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&output)
    );

    // (a) `node -e` prints 2 and the routed child carries the shim marker.
    let eval = report(&stdout, "eval");
    assert_eq!(eval["code"], 0, "eval child exit: {eval}");
    assert_eq!(
        eval["out"].as_str().unwrap_or(""),
        "2\nSHIM=1\n",
        "`spawn(\"node\", [\"-e\", …])` must run on the pir runner: {eval}"
    );
    assert_eq!(eval["err"], "", "eval child stderr: {eval}");

    // (b) `node script.js a b`: argv is [execPath, script, a, b] and execPath
    // is the pir image (the decision record: the routed child does not pretend
    // to be node; a nested `spawn(process.execPath, ["js", …])` re-enters the
    // runner instead of looping through the shim).
    let file = report(&stdout, "file");
    assert_eq!(file["code"], 0, "file child exit: {file}");
    let child: Value = serde_json::from_str(file["out"].as_str().unwrap_or("").trim())
        .unwrap_or_else(|error| panic!("child.js output is not JSON ({error}): {file}"));
    assert_eq!(
        child["argv"],
        serde_json::json!([
            child["execPath"].as_str().unwrap_or(""),
            "child.js",
            "a",
            "b"
        ]),
        "argv must be [execPath, script, …args]: {child}"
    );
    assert!(
        child["execPath"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .ends_with("pir.exe")
            || child["execPath"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .ends_with("/pir")
            || child["execPath"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .ends_with("pir"),
        "process.execPath must be the pir image: {child}"
    );
    assert_eq!(child["shim"], "1", "routed child marker env: {child}");

    // (c) `node --version` / `-v` claim the node compatibility baseline.
    let version = report(&stdout, "version");
    assert_eq!(version["error"], Value::Null, "execFile error: {version}");
    assert_eq!(
        version["stdout"].as_str().unwrap_or(""),
        "v24.19.0",
        "execFile(\"node\", [\"--version\"]) must answer the compat baseline: {version}"
    );
    let version_short = report(&stdout, "versionShort");
    assert_eq!(
        version_short["stdout"].as_str().unwrap_or(""),
        "v24.19.0",
        "node.exe -v must answer the compat baseline: {version_short}"
    );

    // Blocking `spawnSync` and the shell forms (`exec`/`execSync`, which run
    // through cmd.exe / sh) route too.
    let sync = report(&stdout, "sync");
    assert_eq!(sync["status"], 0, "spawnSync status: {sync}");
    assert_eq!(sync["stdout"].as_str().unwrap_or(""), "SYNC_OK", "{sync}");
    let exec_shell = report(&stdout, "execShell");
    assert_eq!(exec_shell["error"], Value::Null, "exec error: {exec_shell}");
    assert_eq!(
        exec_shell["stdout"].as_str().unwrap_or(""),
        "EXEC_SHELL_OK",
        "exec(\"node -e …\") must route the leading node token: {exec_shell}"
    );
    assert_eq!(
        report(&stdout, "execSyncShell"),
        "EXECSYNC_OK",
        "execSync(\"node -e …\") must route the leading node token"
    );
}

/// The node flags pi-subagents actually passes: `--import <preload>` before
/// the runner script (its peer-alias preload) is dropped, `--no-warnings`
/// style toggles are dropped, and the script after them still runs. `<node> -`
/// (stdin script) is supported through the same eval path.
#[test]
fn node_flag_prefix_and_stdin_script_route() {
    let ws = Workspace::new("flags");
    ws.write(
        "child.js",
        "console.log(JSON.stringify({ argv: process.argv.slice(1), shim: process.env.PIR_NODE_SHIM }));\n",
    );
    ws.write(
        "driver.js",
        &format!(
            r#"{CHILD_HELPERS}
async function main() {{
  report("preload", await spawnOut("node", ["--import", "file:///nonexistent-preload.mjs", "child.js", "p1"]));
  report("flags", await spawnOut("node", ["--no-warnings", "--experimental-strip-types", "-e", "console.log('FLAGS_OK')"]));
  report("stdin", await spawnOut("node", ["-"], "console.log('STDIN_OK:' + (6*7));\n"));
  console.log("DONE");
}}
main();
"#
        ),
    );

    let output = ws.run(&["driver.js"]);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success() && stdout.contains("DONE"),
        "driver must run to completion.\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&output)
    );

    let preload = report(&stdout, "preload");
    assert_eq!(preload["code"], 0, "preload-flag child exit: {preload}");
    let child: Value = serde_json::from_str(preload["out"].as_str().unwrap_or("").trim())
        .unwrap_or_else(|error| panic!("preload child output is not JSON ({error}): {preload}"));
    assert_eq!(
        child["argv"],
        serde_json::json!(["child.js", "p1"]),
        "the dropped `--import <preload>` must not become the script path: {child}"
    );
    assert_eq!(child["shim"], "1", "preload child marker: {child}");

    let flags = report(&stdout, "flags");
    assert_eq!(
        flags["out"].as_str().unwrap_or(""),
        "FLAGS_OK\n",
        "valueless node flags must be swallowed before `-e`: {flags}"
    );

    let stdin = report(&stdout, "stdin");
    assert_eq!(stdin["code"], 0, "stdin child exit: {stdin}");
    assert_eq!(
        stdin["out"].as_str().unwrap_or(""),
        "STDIN_OK:42\n",
        "`node -` must run the piped script: {stdin}"
    );
}

/// (d) `detached: true` + `unref()`: the routed child is a real OS process and
/// completes even though the spawning side does not wait for it. The driver
/// spawns it and prints DONE immediately; the test waits for the file the
/// child writes.
#[test]
fn detached_unref_child_completes_without_the_driver_waiting() {
    let ws = Workspace::new("detached");
    ws.write(
        "driver.js",
        r#"
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const out = "detached-out.txt";
try { fs.rmSync(out, { force: true }); } catch (error) {}
const child = spawn("node", ["-e", "setTimeout(function () { require('node:fs').writeFileSync(process.argv[1], 'DETACH_OK'); }, 300);", out], { detached: true, stdio: "ignore", windowsHide: true });
child.unref();
console.log("SPAWNED " + child.pid);
"#,
    );

    let output = ws.run(&["driver.js"]);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success() && stdout.contains("SPAWNED"),
        "driver must spawn the detached child.\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&output)
    );

    let out_file = ws.dir.join("detached-out.txt");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(&out_file) {
            if body == "DETACH_OK" {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "detached routed child never wrote {} (driver stdout: {stdout})",
        out_file.display()
    );
}

/// (e) Regression: non-node programs keep the original spawn path — a built-in
/// shell command still runs, a shell line keeps its quoting/chaining, and a
/// *pathed* executable that merely happens to be named `node` is NOT hijacked
/// (only bare node-family names are claimed).
#[test]
fn non_node_programs_keep_the_original_spawn_path() {
    let ws = Workspace::new("regression");
    // A real executable named `node`/`node.exe`, reached by path. On Windows
    // that is a copy of cmd.exe; elsewhere a copy of a ubiquitous binary.
    let (pathed_name, pathed_args) = if cfg!(windows) {
        ("node.exe", vec!["/C", "echo PATHED_ECHO_OK"])
    } else {
        ("node", vec!["PATHED_ECHO_OK"])
    };
    let source = if cfg!(windows) {
        std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Windows/System32/cmd.exe"))
    } else {
        PathBuf::from("/bin/echo")
    };
    let pathed_probe = if std::fs::copy(&source, ws.dir.join(pathed_name)).is_ok() {
        true
    } else {
        eprintln!(
            "[node_shim_injection] skipping the pathed-program probe: {} is not readable",
            source.display()
        );
        false
    };

    ws.write(
        "driver.js",
        &format!(
            r#"{CHILD_HELPERS}
const path = require("node:path");
async function main() {{
  const shellCommand = process.platform === "win32" ? "cmd.exe" : "sh";
  const shellArgs = process.platform === "win32" ? ["/C", "echo PLAIN_OK"] : ["-c", "echo PLAIN_OK"];
  report("plain", await spawnOut(shellCommand, shellArgs));
  report("quoted", await execShellOut("echo \"QUOTED_OUT\" && echo CHAINED"));
  report("pathed", await spawnOut(path.join(process.cwd(), {pathed_name}), {pathed_args}));
  console.log("DONE");
}}
main();
"#,
            pathed_name = serde_json::to_string(pathed_name).expect("json string"),
            pathed_args = serde_json::to_string(&pathed_args).expect("json args"),
        ),
    );

    let output = ws.run(&["driver.js"]);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success() && stdout.contains("DONE"),
        "driver must run to completion.\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&output)
    );

    let plain = report(&stdout, "plain");
    assert_eq!(
        plain["out"].as_str().unwrap_or("").trim(),
        "PLAIN_OK",
        "a non-node program must keep its original spawn path: {plain}"
    );
    // The shell wrapper stays quote-transparent: the rewrite only fires for a
    // leading node token, and `/s` + the raw tail preserve the rest verbatim.
    let quoted = report(&stdout, "quoted");
    assert_eq!(quoted["error"], Value::Null, "quoted shell line: {quoted}");
    let quoted_stdout = quoted["stdout"].as_str().unwrap_or("");
    assert!(
        quoted_stdout.contains("QUOTED_OUT") && quoted_stdout.contains("CHAINED"),
        "shell quoting and chaining must survive: {quoted}"
    );

    let pathed = report(&stdout, "pathed");
    if pathed_probe {
        assert!(
            pathed["error"].is_null(),
            "a pathed executable must not be hijacked: {pathed}"
        );
        assert!(
            pathed["out"]
                .as_str()
                .unwrap_or("")
                .contains("PATHED_ECHO_OK"),
            "the pathed program itself must run: {pathed}"
        );
    }
}

/// The runner forms the shim translates into: `pir js -e`, `pir js -`,
/// `pir js --version` (the `v24.19.0` masquerade), each with Node's argv
/// shape. Exercised directly so a regression is localizable to the runner
/// rather than to the router.
#[test]
fn pir_js_eval_stdin_and_version_forms() {
    let ws = Workspace::new("js-forms");

    let eval = ws.run(&[
        "-e",
        "console.log(JSON.stringify({ n: process.argv.length, one: process.argv[1], two: process.argv[2], req: typeof require('node:fs').readFileSync }));",
        "x",
        "y",
    ]);
    let stdout = stdout_of(&eval);
    assert!(
        eval.status.success(),
        "pir js -e failed: {}\n{stdout}",
        stderr_of(&eval)
    );
    let shape: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("eval output is not JSON ({error}): {stdout}"));
    assert_eq!(
        shape["n"], 3,
        "Node's -e argv is [execPath, ...args]: {shape}"
    );
    assert_eq!(
        shape["one"], "x",
        "argv[1] must be the first extra arg: {shape}"
    );
    assert_eq!(
        shape["two"], "y",
        "argv[2] must be the second extra arg: {shape}"
    );
    assert_eq!(
        shape["req"], "function",
        "the eval scope must expose a cwd-bound require: {shape}"
    );

    // Stdin script (`-`).
    let mut stdin_cmd = Command::new(env!("CARGO_BIN_EXE_pir"));
    stdin_cmd
        .env("PIR_AGENT_DIR", &ws.agent_dir)
        .current_dir(&ws.dir)
        .arg("js")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = stdin_cmd.spawn().expect("pir binary should start");
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin
            .write_all(b"console.log('STDIN_FORM:' + (7*6));\n")
            .expect("write stdin script");
    }
    let piped = child.wait_with_output().expect("stdin run output");
    assert!(
        piped.status.success(),
        "pir js - failed: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    assert!(
        String::from_utf8_lossy(&piped.stdout).contains("STDIN_FORM:42"),
        "the stdin script must run: {}",
        String::from_utf8_lossy(&piped.stdout)
    );

    // `--version` / `-v` claim the node compatibility baseline, not pir's own.
    for flag in ["--version", "-v"] {
        let version = ws.run(&[flag]);
        assert_eq!(
            stdout_of(&version).trim(),
            "v24.19.0",
            "`pir js {flag}` must answer the node compat baseline"
        );
    }

    // A sync throw is Node's uncaught-exception block: stack on stderr, 1.
    let thrown = ws.run(&["-e", "throw new Error('BOOM')"]);
    assert_eq!(thrown.status.code(), Some(1), "eval throw must exit 1");
    assert!(
        stderr_of(&thrown).contains("BOOM"),
        "the stack must reach stderr: {}",
        stderr_of(&thrown)
    );
}
