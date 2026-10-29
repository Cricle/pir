//! pi-subagents parent-side launch: the real host -> runner path.
//!
//! pi-subagents starts a detached background runner from the *parent* process
//! (`async-execution.js` `spawnRunner`, reached through `executeAsyncSingle`,
//! the function a `subagent { async: true }` tool call runs). That launch is
//! gated on host-SDK discovery: the parent fails closed with "Background
//! children require a supported standalone Pi host or the installed npm package
//! ...; neither is available" unless one of `process.argv[1]`,
//! `import.meta.resolve` or `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT` names a
//! package root whose manifest is `@earendil-works/pi-coding-agent` and whose
//! host peer packages resolve.
//!
//! The extension runtime has an empty `process.argv` and no
//! `import.meta.resolve`, so the host publishes the third channel: the host pi
//! package `loader::ensure_host_pi_package_root` materializes below the agent
//! dir (entry re-exporting the host virtual SDK module, stub peers for the peer
//! probe). The runner is then spawned as `node <bootstrap> <cfg>`, which the
//! node-executable shim routes to `<pir image> js <bootstrap> <cfg>`.
//!
//! Three tests, in increasing reach:
//!
//! * `host_sdk_discovery_gate_and_file_url_imports_hold` (offline, always runs
//!   when pi-subagents is installed): gate channels, the `file:` URL import the
//!   host-SDK loader performs, host peer probe, async availability.
//! * `parent_launch_registers_a_spawned_runner_process` (offline): drives
//!   `executeAsyncSingle` inside the extension runtime and asserts the parent
//!   really spawned a child through the child-process shim (a numeric pid in the
//!   run's status). The spawned program is this test binary (`shim_program`
//!   resolves the node shim's target from `current_exe()`), so the *runner's*
//!   own artifacts are the CLI variant's job.
//! * `real_pir_process_launches_the_detached_runner` (PIR_SUBAGENTS_E2E=1): the
//!   same launch from a real `pir print` process, seeded through `-e`. It is
//!   `#[ignore]`d; run it with `--ignored`. Here the
//!   node shim routes to the real pir image and the detached runner executes:
//!   its `events.jsonl` lands in the run directory.
//!
//! Measured cost (not a boundary): the runner's step start runs pi-subagents'
//! synchronous mutation-evidence snapshot
//! (`src/runs/shared/mutation-evidence.js` `snapshotTrackedMutations` →
//! `execFileSync("git", ["diff", …])` + `sha256` per changed tracked file,
//! up to `MAX_TRACKED_PATHS = 500`). In a worktree with hundreds of changed
//! files (the development tree this was measured in: 500 tracked changes at
//! the cap) that is minutes of *blocking* JS: no timer, no pump and no child
//! spawn happens in that window. The same code blocks Node's loop identically
//! (it is `execFileSync`), so the script-mode event loop is not the
//! constraint; everything after the scan — the child session's spawn, the
//! child-process pump and the timers the pump rides — runs normally, which is
//! what the assertions below cover.
//!
//! That window used to be fatal in a different way: pi-subagents arms three
//! `fs.watch` watchers whose JS pumps ride 30 ms timers, and the host's
//! watcher threads used to reap themselves after 60 s without a `watchPoll`
//! — exactly the state a multi-minute synchronous scan creates. The reap path
//! deadlocked the watcher thread *holding* the global watch registry (an
//! iterator guard re-entered its own mutex), so the first post-scan
//! `watchPoll` — a synchronous host op inside the runner's timer callback —
//! blocked the JS thread forever and the run never reached its child session's
//! event stream. `fs_watch_fns` now reaps on runtime teardown only (a `Weak`
//! on `RuntimeState`) and releases the registry lock before removing an entry.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::Value;

/// Entry point of the installed pi-subagents package, when present.
fn installed_subagents_entry() -> Option<PathBuf> {
    let entry = pir::coding_agent::context::agent_dir()
        .join("npm")
        .join("node_modules")
        .join("pi-subagents")
        .join("index.js");
    entry.is_file().then_some(entry)
}

/// `<agent>/npm/node_modules/pi-subagents` for the installed package.
fn subagents_root(entry: &Path) -> PathBuf {
    entry.parent().expect("package root").to_path_buf()
}

/// pi-subagents' run directory for `run_id`: `<os temp>/pi-subagents-<scope>/
/// async-subagent-runs/<run_id>`. The scope is derived from the OS user
/// (`user-<name>`, `uid-N`, `home-<path>`), so it is discovered by scanning
/// the temp directory for the run instead of re-deriving it.
fn find_run_dir(run_id: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(std::env::temp_dir()).ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("pi-subagents")
        {
            continue;
        }
        let dir = entry.path().join("async-subagent-runs").join(run_id);
        if dir.is_dir() {
            candidates.push(dir);
        }
    }
    candidates.sort();
    candidates.pop()
}

/// Poll until pi-subagents' run directory for `run_id` exists.
fn wait_for_run_dir(run_id: &str, timeout_ms: u64) -> PathBuf {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if let Some(dir) = find_run_dir(run_id) {
            return dir;
        }
        if std::time::Instant::now() >= deadline {
            panic!("pi-subagents wrote no run directory for {run_id}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "print".to_string(),
        has_ui: false,
        model: None,
        session_id: Some("test-parent-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

// ---------------------------------------------------------------------------
// 0. The tool context's `ctx.pi` (the parent-side launch surface)
// ---------------------------------------------------------------------------

/// Upstream's tool context carries the extension API as `ctx.pi` — the same
/// object the extension factory received. pi-subagents' parent-side launch
/// calls `ctx.pi.events.emit(...)` from the tool path
/// (`async-execution.js` `finishSpawnResult`, reached by the `subagent` tool
/// call), so a tool context without `pi` fails that call with a TypeError.
///
/// This runs the real tool dispatch (`ExtensionRuntime::call_tool` →
/// `__pir_callTool` → `makeCtx`) against an extension whose `execute` reads
/// `ctx.pi` and emits on the extension bus, and pins three things: `ctx.pi`
/// exists, it is the *identical* object the factory got (not a rebuild), and
/// the emit reaches the extension's own `pi.events.on` subscriber.
#[tokio::test(flavor = "current_thread")]
async fn tool_context_carries_the_factory_pi_api() {
    let fixture = write_temp_fixture("tool-ctx-pi", &[]);
    let rt = runtime_at(&fixture).await;

    let module = r#"
export default function (pi) {
  globalThis.__pirFactoryPi = pi;
  globalThis.__pirBusEvents = [];
  pi.events.on("launch-probe", (data) => { globalThis.__pirBusEvents.push(data); });
  pi.registerTool({
    name: "pi-ctx-probe",
    description: "reads the tool context's pi surface",
    parameters: { type: "object", properties: {} },
    async execute(toolCallId, _params, _signal, _onUpdate, ctx) {
      const factoryPi = globalThis.__pirFactoryPi;
      const hasPi = typeof ctx.pi === "object" && ctx.pi !== null;
      const sameObject = hasPi && ctx.pi === factoryPi;
      const hasEmit = hasPi && ctx.pi.events && typeof ctx.pi.events.emit === "function";
      if (hasEmit) ctx.pi.events.emit("launch-probe", { toolCallId });
      return {
        content: [{ type: "text", text: "probe-ran" }],
        details: {
          hasPi,
          sameObject,
          hasEmit,
          hasOn: hasPi && typeof ctx.pi.on === "function",
          hasRegisterTool: hasPi && typeof ctx.pi.registerTool === "function",
        },
      };
    },
  });
}
"#;
    rt.seed_module("pir-ext:tool-ctx-pi/index.js", module)
        .expect("seed tool ctx module");
    rt.seeded_load("pir-ext:tool-ctx-pi/index.js")
        .await
        .expect("tool ctx module must load");

    let result = rt
        .call_tool("pi-ctx-probe", "call-1", &serde_json::json!({}))
        .await
        .expect("tool call must not throw (a missing ctx.pi would be a TypeError)");
    eprintln!("[subagents] tool ctx probe: {result:?}");
    assert_eq!(result.details["hasPi"], true, "{result:?}");
    assert_eq!(
        result.details["sameObject"], true,
        "ctx.pi must be the factory's own object: {result:?}"
    );
    assert_eq!(result.details["hasEmit"], true, "{result:?}");
    assert_eq!(result.details["hasOn"], true, "{result:?}");
    assert_eq!(result.details["hasRegisterTool"], true, "{result:?}");

    let bus = rt
        .eval_json("JSON.stringify(globalThis.__pirBusEvents)")
        .await
        .expect("bus events json");
    assert_eq!(
        bus.as_array().map(Vec::len),
        Some(1),
        "ctx.pi.events.emit must reach the extension's subscriber: {bus}"
    );
    assert_eq!(bus[0]["toolCallId"], "call-1", "{bus}");

    let _ = std::fs::remove_dir_all(&fixture);
}

fn write_temp_fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-subagents-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    for (file, content) in files {
        let path = dir.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture subdir");
        }
        std::fs::write(path, content).expect("fixture write");
    }
    dir
}

fn json_literal(value: &str) -> String {
    serde_json::to_string(value).expect("json literal")
}

/// Poll for a file the detached runner (another process) writes.
fn wait_for_file(path: &Path, timeout_ms: u64) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            return Some(text);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

// ---------------------------------------------------------------------------
// 1. Discovery gate + `file:` URL module loading (offline)
// ---------------------------------------------------------------------------

/// Offline probe, in one seeded module with top-level await:
///
/// 1. the host-package env override pi-subagents reads is published by the host
///    and points at a manifest with the expected package identity;
/// 2. importing that package's root entry by `file:` URL (exactly
///    `loadHostPiCodingAgent`'s `import(pathToFileURL(entry).href)`) yields the
///    same module instance as the bare host-virtual SDK specifier;
/// 3. pi-subagents' own `resolveHostPeerAliases` finds every required host peer
///    below that root (an incomplete peer set makes `spawnRunner` fail closed);
/// 4. `isAsyncAvailable()` reports the detached runner as supported.
const GATE_PROBE_JS: &str = r#"
export default function () {}
globalThis.__gateProbe = { state: "failed", error: "probe did not run" };
try {
  const fs = await import("node:fs");
  const path = await import("node:path");
  const url = await import("node:url");
  const root = process.env.PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT;
  const hostRoot = root ? path.normalize(root).replaceAll("\\", "/") : null;
  const manifest = hostRoot
    ? JSON.parse(fs.readFileSync(path.join(hostRoot, "package.json"), "utf8"))
    : null;
  const bare = await import("@earendil-works/pi-coding-agent");
  let entrySameModule = false;
  let entryExportCount = 0;
  if (hostRoot) {
    const entry = await import(url.pathToFileURL(path.join(hostRoot, "index.js")).href);
    entrySameModule = entry.createAgentSession === bare.createAgentSession
      && entry.ModelRuntime === bare.ModelRuntime;
    entryExportCount = Object.keys(entry).length;
  }
  const aliases = await import(url.pathToFileURL(PKG + "/src/runs/background/runner-aliases.js").href);
  const peers = hostRoot ? aliases.resolveHostPeerAliases(hostRoot) : { aliases: {}, missing: [] };
  const exec = await import(url.pathToFileURL(PKG + "/src/runs/background/async-execution.js").href);
  globalThis.__gateProbe = {
    state: "ok",
    root: hostRoot,
    manifestName: manifest ? manifest.name : null,
    entrySameModule,
    entryExportCount,
    peerMissing: peers.missing,
    peerAliasCount: Object.keys(peers.aliases).length,
    asyncAvailable: exec.isAsyncAvailable(),
    runnerBootstrapExists: fs.existsSync(
      path.join(PKG, "src/runs/background/subagent-runner-bootstrap.js"),
    ),
    nodeExecutable: (await import(url.pathToFileURL(PKG + "/src/shared/node-executable.js").href)).resolveNodeExecutable(),
    executableEnv: process.env.PIR_EXECUTABLE ?? null,
  };
} catch (error) {
  globalThis.__gateProbe = {
    state: "failed",
    error: error && error.message ? error.message : String(error),
    stack: error && error.stack ? String(error.stack).slice(0, 1200) : null,
  };
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn host_sdk_discovery_gate_and_file_url_imports_hold() {
    let Some(entry) = installed_subagents_entry() else {
        eprintln!("[subagents] pi-subagents not installed; skipping discovery gate probe");
        return;
    };
    let root = subagents_root(&entry);
    let fixture = write_temp_fixture("gate-probe", &[]);
    let rt = runtime_at(&fixture).await;
    rt.allow_root(root.clone());

    let probe = GATE_PROBE_JS.replace("PKG", &json_literal(&root.display().to_string()));
    rt.seed_module("pir-ext:gate-probe/probe.js", &probe)
        .expect("seed probe module");
    rt.seeded_load("pir-ext:gate-probe/probe.js")
        .await
        .expect("probe module must load");
    let probe = rt
        .eval_json("JSON.stringify(globalThis.__gateProbe)")
        .await
        .expect("probe json");
    eprintln!("[subagents] gate probe: {probe}");
    assert_eq!(probe["state"], "ok", "probe failed: {probe}");

    // 1. The host publishes the discovery channel pi-subagents falls back to.
    let host_root = probe["root"].as_str().expect("host package root");
    assert!(!host_root.is_empty(), "package root must be published");
    assert_eq!(probe["manifestName"], "@earendil-works/pi-coding-agent");
    assert!(
        Path::new(host_root).join("index.js").is_file(),
        "host package entry must exist: {host_root}"
    );

    // 2. `file:` URL import of the package entry == the host virtual SDK.
    assert_eq!(probe["entrySameModule"], true, "{probe}");
    assert!(
        probe["entryExportCount"].as_u64().unwrap_or(0) > 50,
        "the host SDK entry must expose the SDK surface: {probe}"
    );

    // 3. Every peer `resolveHostPeerAliases` requires resolves.
    assert_eq!(
        probe["peerMissing"].as_array().map(Vec::len),
        Some(0),
        "host peer probe must find every peer: {probe}"
    );
    assert!(
        probe["peerAliasCount"].as_u64().unwrap_or(0) >= 8,
        "alias map must cover the host peer set: {probe}"
    );

    // 4. The detached runner is supported, and the spawn target exists.
    assert_eq!(probe["asyncAvailable"], true, "{probe}");
    assert_eq!(probe["runnerBootstrapExists"], true, "{probe}");
    assert!(
        probe["nodeExecutable"]
            .as_str()
            .is_some_and(|name| name.starts_with("node")),
        "the node-executable shim target must be a bare node name: {probe}"
    );
    assert!(
        probe["executableEnv"]
            .as_str()
            .is_some_and(|path| !path.is_empty()),
        "the host must publish its own image for SDK child sessions: {probe}"
    );

    let _ = std::fs::remove_dir_all(&fixture);
}

// ---------------------------------------------------------------------------
// 2. The parent-side launch reaches the spawn (offline)
// ---------------------------------------------------------------------------

/// Drives pi-subagents' parent-side launch entry (`executeAsyncSingle`) from a
/// seeded extension inside the real extension runtime. The launch runs
/// synchronously up to the detached spawn, so the run directory plus a numeric
/// pid prove the child-process shim really created a process. The spawned
/// program is this test binary (`shim_program` uses the host's `current_exe()`),
/// so the runner's own artifacts are asserted by the CLI variant instead.
fn parent_launch_js(root: &Path, cwd: &str, run_id: &str, task: &str) -> String {
    let exec_url = format!(
        "{}/src/runs/background/async-execution.js",
        root.display().to_string().replace('\\', "/")
    );
    format!(
        r#"
export const launched = (async () => {{}})();
globalThis.__launchProbe = {{ state: "running" }};
// The extension factory receives the host extension API (upstream's SDK tool
// context carries the same object as `ctx.pi`), so the launch runs inside it.
export default async function (pi) {{
try {{
  const url = await import("node:url");
  const exec = await import(url.pathToFileURL({exec_url}).href);
  const result = await exec.executeAsyncSingle({run_id}, {{
    agent: "pir-e2e",
    agentConfig: {{
      name: "pir-e2e",
      tools: ["read"],
      inheritProjectContext: false,
      inheritGlobalContext: false,
      inheritSkills: false,
    }},
    ctx: {{
      cwd: {cwd},
      permissions: [],
      currentSessionId: "parent-e2e-session",
      projectTrusted: false,
      pi,
    }},
    task: {task},
    cwd: {cwd},
    maxOutput: 8000,
    shareEnabled: false,
    artifactConfig: {{ enabled: false }},
    maxSubagentDepth: 1,
  }});
  globalThis.__launchProbe = {{ state: "launched", runId: {run_id}, result }};
}} catch (error) {{
  globalThis.__launchProbe = {{
    state: "threw",
    error: error && error.message ? error.message : String(error),
    stack: error && error.stack ? String(error.stack).slice(0, 2000) : null,
  }};
}}
}}
"#,
        exec_url = json_literal(&exec_url),
        run_id = json_literal(run_id),
        cwd = json_literal(cwd),
        task = json_literal(task),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn parent_launch_registers_a_spawned_runner_process() {
    let Some(entry) = installed_subagents_entry() else {
        eprintln!("[subagents] pi-subagents not installed; skipping the parent-side launch");
        return;
    };
    let root = subagents_root(&entry);
    // The child-process sandbox allows a spawn whose cwd sits below the
    // runtime's configured cwd, and pi-subagents spawns the runner in the
    // child's cwd.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let rt = runtime_at(&repo).await;
    rt.allow_root(root.clone());

    let run_id = format!("pir-e2e-unit-{}", std::process::id());
    let probe = parent_launch_js(
        &root,
        &repo.display().to_string(),
        &run_id,
        "Reply with exactly: pong",
    );
    rt.seed_module("pir-ext:launch-probe/probe.js", &probe)
        .expect("seed launch module");
    rt.seeded_load("pir-ext:launch-probe/probe.js")
        .await
        .expect("launch module must load");
    let launch = rt
        .eval_json("JSON.stringify(globalThis.__launchProbe)")
        .await
        .expect("launch probe json");
    eprintln!("[subagents] launch probe: {launch}");
    assert_eq!(
        launch["state"], "launched",
        "the parent-side launch must not throw: {launch}"
    );
    // The parent reports the started run and the directory it wrote.
    assert_eq!(launch["result"]["details"]["mode"], "single", "{launch}");
    assert_eq!(launch["result"]["details"]["runId"], run_id.as_str());
    assert!(
        launch["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Async:") && text.contains(&run_id)),
        "the started message must name the run: {launch}"
    );

    // The parent spawned the runner: its status carries the child's pid and
    // pi-subagents' process-terminal bookkeeping for that instance.
    let run_dir = wait_for_run_dir(&run_id, 20_000);
    let status_path = run_dir.join("status.json");
    let status = wait_for_file(&status_path, 15_000)
        .unwrap_or_else(|| panic!("parent did not write {}", status_path.display()));
    let status: Value = serde_json::from_str(&status).expect("status json");
    eprintln!("[subagents] status: {status}");
    assert!(
        status["pid"].as_u64().is_some_and(|pid| pid > 0),
        "the spawn must yield a real pid: {status}"
    );
    assert_eq!(status["mode"], "single");
    assert_eq!(status["runId"], run_id.as_str());
    assert!(
        status["processTerminal"]["runnerProcessInstanceId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the parent must track the spawned runner instance: {status}"
    );
}

// ---------------------------------------------------------------------------
// 3. The real parent process -> detached runner (opt-in)
// ---------------------------------------------------------------------------

/// The seeded extension that drives the launch from a real `pir print`
/// process. It captures the extension API handed to the factory and passes it
/// as the SDK tool context's `pi` (upstream's tool context carries it; the
/// run's `subagent.async.started` event goes through `ctx.pi.events`).
fn seeded_extension_js(root: &Path, run_id: &str) -> String {
    let exec_url = format!(
        "{}/src/runs/background/async-execution.js",
        root.display().to_string().replace('\\', "/")
    );
    format!(
        r#"// Seeded extension: drives pi-subagents' parent-side background launch from
// a real `pir print` process (the entry the `subagent` tool reaches).
import * as fs from "node:fs";
import * as url from "node:url";

const EXEC_URL = {exec_url};
const RUN_ID = {run_id};
const OUT = process.env.PIR_E2E_OUT;

function record(payload) {{
  try {{
    if (OUT) fs.writeFileSync(OUT, JSON.stringify(payload, null, 2));
  }} catch (error) {{
    console.error("seed record failed: " + String(error && error.message));
  }}
}}

export default async function (pi) {{
  try {{
    const exec = await import(url.pathToFileURL(EXEC_URL).href);
    const result = await exec.executeAsyncSingle(RUN_ID, {{
      agent: "pir-e2e",
      agentConfig: {{
        name: "pir-e2e",
        tools: ["read"],
        inheritProjectContext: false,
        inheritGlobalContext: false,
        inheritSkills: false,
      }},
      ctx: {{
        cwd: process.cwd(),
        permissions: [],
        currentSessionId: "parent-e2e-session",
        projectTrusted: false,
        pi,
      }},
      task: "Reply with exactly: pong",
      cwd: process.cwd(),
      maxOutput: 8000,
      shareEnabled: false,
      artifactConfig: {{ enabled: false }},
      maxSubagentDepth: 1,
    }});
    record({{ state: "launched", runId: RUN_ID, result }});
  }} catch (error) {{
    record({{
      state: "threw",
      error: error && error.message ? error.message : String(error),
      stack: error && error.stack ? String(error.stack).slice(0, 2000) : null,
    }});
  }}
}}
"#,
        exec_url = json_literal(&exec_url),
        run_id = json_literal(run_id),
    )
}

/// `pir print` runs one prompt through the real host; the seeded extension
/// launches the background run while it loads. Requires `PIR_SUBAGENTS_E2E=1`
/// (it spends a model call for the print turn, and the detached runner spends
/// one of its own for the child session).
///
/// Cleanup: while the runner-side boundary in the module header holds (in a
/// worktree with hundreds of changed files the synchronous mutation-evidence
/// scan runs for ~2 minutes before the child session starts, and a run whose
/// child stream never settles stays `running`), the test prints the runner pid
/// and run directory so a CI job's process-tree teardown (or an explicit kill)
/// can collect it.
#[test]
#[ignore = "requires PIR_SUBAGENTS_E2E=1, an installed pi-subagents package, and real model calls"]
fn real_pir_process_launches_the_detached_runner() {
    if std::env::var("PIR_SUBAGENTS_E2E").ok().as_deref() != Some("1") {
        eprintln!("[subagents] PIR_SUBAGENTS_E2E != 1; skipping the real parent process launch");
        return;
    }
    let Some(entry) = installed_subagents_entry() else {
        eprintln!("[subagents] pi-subagents not installed; skipping the launch");
        return;
    };
    let root = subagents_root(&entry);
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let run_id = format!("pir-e2e-cli-{}", std::process::id());
    let outcome_path = std::env::temp_dir().join(format!(
        "pir-subagents-e2e-outcome-{}.json",
        std::process::id()
    ));

    let seed = write_temp_fixture(
        "cli-seed",
        &[
            (
                "package.json",
                "{\n  \"name\": \"pir-e2e-seed\",\n  \"private\": true,\n  \"pi\": { \"extensions\": [\"./index.js\"] }\n}\n",
            ),
            ("index.js", &seeded_extension_js(&root, &run_id)),
        ],
    );

    let pir = env!("CARGO_BIN_EXE_pir");
    // `Stdio::null()`: pir spawns the detached runner as a grandchild that
    // inherits the parent's inheritable handles, so a captured (piped) stdout
    // would stay open for as long as the runner lives - `output()` would never
    // see EOF. The launch outcome is read from the seed's own record instead.
    let status = std::process::Command::new(pir)
        .arg("-e")
        .arg(&seed)
        .arg("print")
        .arg("hi")
        .current_dir(&repo)
        .env("PIR_E2E_OUT", &outcome_path)
        .env("PIR_E2E_RUN_ID", &run_id)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run pir print with the seeded extension");
    eprintln!("[subagents] pir print exit={:?}", status.code());

    // The parent-side launch happened as the extension loaded.
    let outcome =
        std::fs::read_to_string(&outcome_path).expect("seed must record its launch outcome");
    let outcome: Value = serde_json::from_str(&outcome).expect("outcome json");
    eprintln!("[subagents] seed outcome: {outcome}");
    assert_eq!(outcome["state"], "launched", "launch failed: {outcome}");

    // The parent recorded the spawned runner (node shim -> the real pir image).
    let run_dir = wait_for_run_dir(&run_id, 30_000);
    let status = wait_for_file(&run_dir.join("status.json"), 20_000)
        .unwrap_or_else(|| panic!("no parent status in {}", run_dir.display()));
    let status: Value = serde_json::from_str(&status).expect("status json");
    assert!(
        status["pid"].as_u64().is_some_and(|pid| pid > 0),
        "the runner spawn must yield a real pid: {status}"
    );

    // The detached runner really executed pir-side JS: only the node shim
    // route (`node <bootstrap>` -> `<pir image> js <bootstrap>`) can run its
    // ESM module graph, and the run's event stream is its first write.
    let events = wait_for_file(&run_dir.join("events.jsonl"), 90_000)
        .unwrap_or_else(|| panic!("detached runner wrote no events in {}", run_dir.display()));
    eprintln!("[subagents] events.jsonl: {events}");
    assert!(
        events.contains("\"subagent.run.started\""),
        "the runner must record its start: {events}"
    );
    assert!(
        events.contains("\"subagent.step.started\""),
        "the runner must dispatch its step: {events}"
    );

    // Past `subagent.step.started`: the runner rewrites the step's output log
    // (`fs.writeFileSync(ctx.outputFile, "")`, `subagent-runner.js`) at the
    // start of its child-session attempt, immediately before
    // `runChildSession(...)`, so `<run>/output-0.log` appearing is the first
    // observable crossing of the step start. The window covers the synchronous
    // mutation-evidence scan measured in this worktree (~2 minutes, +2m10s to
    // be exact; see the module header) - the assertion costs nothing on a
    // clean tree, where that scan is milliseconds.
    let child_log = wait_for_file(&run_dir.join("output-0.log"), 300_000).unwrap_or_else(|| {
        panic!(
            "the runner never reached its child session in {}",
            run_dir.display()
        )
    });
    eprintln!(
        "[subagents] child-session log staged ({} bytes): {}",
        child_log.len(),
        run_dir.join("output-0.log").display()
    );

    // The run's terminal record. Reaching it means the child session's event
    // pump delivered the child's JSON stream and `session.prompt` settled.
    // `state` is pi-subagents' canonical terminal value (`"complete"`, the
    // projection in `async-status-projection.js`) with `success` true for a
    // clean child run; the runner pid is printed for teardown bookkeeping
    // only, the result assertions are the real gate.
    eprintln!(
        "[subagents] cleanup: runner pid {:?} in {}",
        status["pid"],
        run_dir.display()
    );
    let terminal = wait_for_file(
        &run_dir
            .parent()
            .expect("async-subagent-runs")
            .parent()
            .expect("temp root")
            .join("async-subagent-results")
            .join(format!("{run_id}.json")),
        240_000,
    )
    .unwrap_or_else(|| {
        panic!(
            "the detached runner wrote no terminal result in {}: its child session's event \
             stream never settled",
            run_dir.display()
        )
    });
    let result: Value = serde_json::from_str(&terminal).expect("result json");
    eprintln!("[subagents] result.json: {result}");
    assert_eq!(
        result["success"], true,
        "a completed run must report success: {result}"
    );
    assert_eq!(result["state"], "complete", "{result}");

    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_file(&outcome_path);
}
